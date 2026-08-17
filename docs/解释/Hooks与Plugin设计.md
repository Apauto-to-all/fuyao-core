# Hooks 与 Plugin 设计

> 本文解释钩子系统的两类钩子（拦截 + 观察）、Plugin 两层模型（工厂 + session 实例）与无状态快捷路径、SessionSender 两通道分流、注册表装配后冻结、插件实例生命周期闭环与安全防护。API 签名见 `cargo doc --workspace`。

## 设计目标

提供 Agent 行为的扩展点，不修改引擎内核即可：

- 拦截 / 修改 / 阻止 Agent 的输出
- 观察 Agent 的行为（只读副作用）
- 主动向 Agent 注入消息或发出中断（经 session 装配期注入的 `SessionSender`，不走钩子）

## 统一消息处理管道（dispatch）

所有 OutputEvent 必经此管道（三段串行），管道在每个 session 内运行：

```text
OutputEvent（输入转化的、引擎内部产生的）
   ↓
┌─ 拦截（intercept）─┐  插件经 &mut 原地修改；返回 Some(原因) 则阻止（短路丢弃）
└────────┬───────────┘
         ↓ 阻止则丢弃整条（不落库、不发送、不观察）
┌─ 发送（deliver）───┐  Emitter::emit（盖 session_id + tx.send）
└────────┬───────────┘
         ↓
┌─ 观察（observe）───┐  插件只读副作用（持久化/日志/统计），Arc 共享同一份事件
└─────────────────────┘
```

| 段 | 职责 | 关键约束 |
|----|------|---------|
| **拦截** | 插件原地改、可阻 | 同步串行（priority 降序）；前一个钩子的修改对后续可见；任一阻止立即短路 |
| **发送** | 推到该 session 出站通道 | Emitter::emit，盖 session_id 后 `tx_event.send`（per-session 通道；fan-in 由 App 承担）；事件物化成本恒定为一次 clone（emit 消费一份），观察侧零拷贝 |
| **观察** | 插件只读副作用 | 不能改数据、不能阻；多钩子经 `Arc` 共享同一份事件本体；发送在前、观察在后（串行） |

进历史的消息（User / Assistant / ToolResult，要落 DB、参与计费）走 history 统一入口，在拦截与发送之间叠加「事件投影 Message 落 DB（seq 回填 + 计费）」一段；纯事件（Chunk / Error / 中断通知等）不落库，直走三段管道。工具调用需要拿拦截结果回灌时，单独调 `intercept` 段（完整管道的拦截子集），拦截后的 payload 作为执行 / 存储 / 发送三者的统一数据源。

## 两类钩子

### 拦截型（可原地修改 / 阻止）

| 钩子签名 | 作用 | 返回 | 执行方式 |
|---------|------|------|---------|
| `OutputInterceptFn` | 经 `&mut` 原地修改 / 阻止 OutputEvent | `Option<String>` | 同步，串行，任一阻止短路 |

```text
OutputInterceptFn = Fn(&mut OutputEvent) -> Option<String>
```

返回值语义与修改语义显式分离：

- `None`：通过——事件保留钩子已做的原地修改，继续走下一个钩子
- `Some(reason)`：阻止当前事件（携带原因），执行链立即短路

钩子拿到的是 `&mut` 借用：不修改的钩子零开销，前一个钩子的修改对后续钩子可见（原地链式）。拦截只负责修改或阻止事件，不承载中断职责——中断统一经 `SessionSender::send_interrupt` 发出，由引擎主循环处理。

拦截钩子带优先级（`register_output_intercept(priority, handler)`）：高优先级先执行，同优先级按注册顺序（装配期 `finalize` 稳定排序保证）。

### 观察型（只读副作用）

| 钩子签名 | 作用 | 执行方式 |
|---------|------|---------|
| `OutputObserveFn` | 持久化 / 日志 / 统计 | 异步，串行，OutputEvent 发送后执行 |

事件以 `Arc` 传入：同一事件分发给多个观察钩子时，每个钩子只做一次引用计数拷贝（廉价），事件本体全程零 clone、全程只读——需要 owned 数据的钩子自行解引用后 clone 所需字段。

观察钩子与拦截钩子统一的优先级语义（`register_output_observe(priority, handler)`）：高优先级先执行，同优先级按注册顺序（装配期 `finalize` 稳定排序保证）。单个钩子可配置执行超时（`[hooks] timeout_secs`，0 表示不超时），超时跳过不阻塞后续。

### 发消息不是钩子

插件主动发消息的能力（注入 User、发 Interrupt）**不通过钩子表达**——钩子只有拦截 / 观察两类，分别是「改数据」与「看数据」的扩展点。发消息是持句柄的能力，经 session 装配期的 `register` 参数注入（见下文 SessionSender 一节）：需要发消息的插件在 `register` 时保存 `SessionSender`，此后任何时机都可调用。

## Plugin 两层模型（工厂 + session 实例）

多 session 并发时，**每个 session 拥有自己独立的一份插件实例**，互不串台（session A 的循环计数不影响 session B）。隔离靠「工厂 + 实例」两层模型：

```text
引擎级（启动一次注册）：
  PluginHost 持有若干 Plugin（工厂模板）
         │ 只持有配置/共享依赖，无 per-session 状态
         ↓ 每个 session 启动时调 create_instances()
session 级（每 session 独立）：
  (插件名, PluginInstance) 配对（实例持该 session 独立状态）
         ↓ 逐个 instance.register(&mut HooksRegistry, &SessionSender)
  HooksRegistry → finalize() 冻结 → 绑定到该 session 的 dispatch 管道
  实例集合 → SessionHandle 持有（session 结束时逆序 dispose，见生命周期闭环）
```

### Plugin trait（工厂模板）

```text
trait Plugin {
    fn name(&self) -> &str;                            // 插件唯一标识
    fn create_instance(&self) -> Box<dyn PluginInstance>;  // 每 session 调一次，生成独立实例
    fn dispose(&self) {}                               // 引擎 shutdown 最后清理（默认空）
}
```

Plugin 是**编译期**扩展点——`impl Plugin` 后编译进二进制，运行时按 `[plugins.enabled]` 过滤启用。

- **无状态插件**：直接用 `simple_plugin` 一个闭包搞定（见下节）；走两层模型则 `create_instance` 返回无字段实例，闭包本身无状态，天然隔离
- **有状态插件**（如 LoopGuard）：`create_instance` 每 session 新建独立状态（独立的循环计数器/检测器），多 session 并发互不串台

### PluginInstance trait（session 实例）

```text
trait PluginInstance {
    fn register(&self, hooks: &mut HooksRegistry, sender: &SessionSender);  // 注册 hook + 接收发送器
    fn dispose(&self) {}                                                    // session 结束时逆序清理（默认空）
}
```

每 session 装配时引擎调 `create_instances` 生成 `(插件名, 实例)` 配对，逐个 `instance.register(&mut registry, &sender)` 注册到该 session 私有的 `HooksRegistry`——`sender` 绑定该插件名（注入消息的 source 据此可追溯），需要发消息的插件在此 clone 保存，不需要的可忽略。

`register` 是**同步**方法：只做注册闭包与保存发送器两个动作，不执行异步操作；注册的观察闭包内部可以是异步的（执行时被 await）。

### simple_plugin：无状态快捷路径

无 per-session 状态的插件（纯日志、纯统计钩子等）不必写两层样板——`simple_plugin(name, register)` 一个注册闭包即插件：

```text
simple_plugin(name, register) -> SimplePlugin
  register: Fn(&mut HooksRegistry, &SessionSender)   // 每 session 装配时调用一次
```

`SimplePlugin` 实现了 `Plugin`：每次 `create_instance` 都返回同一份注册闭包的共享句柄（闭包无状态，跨 session 复用），各 session 装配时各自调用一次完成钩子注册。对 `PluginHost` 与引擎装配完全透明——快捷路径产物与两层模型产物同级混用。

取舍：

- **无状态插件**走 `simple_plugin`：省去 Plugin struct + PluginInstance struct + 两个 trait impl 的样板
- **有状态插件**（每 session 需独立计数器、缓存等）仍走完整两层模型：`create_instance` 每 session 新建独立状态

### PluginHost

| API | 说明 |
|-----|------|
| `add(plugin: Box<dyn Plugin>)` | 添加工厂（不立即创建实例） |
| `create_instances() -> Result<Vec<(String, Box<dyn PluginInstance>)>, PluginInstallError>` | 每 session 调一次，生成所有插件的 `(插件名, 实例)` 配对（生成前内置重名校验；create_instance panic 防护，崩溃插件的实例被跳过） |
| `list()` | 列出已注册工厂名 |
| `dispose_all()` | 逆序销毁所有工厂（LIFO，单个 panic 不阻塞） |

重名硬失败（`PluginInstallError::DuplicateName`，由 `create_instances` 在生成实例前内部校验）——防止两个插件同名导致行为不确定。返回 `(名, 实例)` 配对而非裸实例，是为了让装配方不必反向查询身份就能构造每个实例专属的 `SessionSender`。

## SessionSender：两通道分流

`SessionSender` 封装"往**这个 session** 发消息"的能力，两种消息类型自动分流到该 session 的两条既有通道：

| 消息类型 | 走向 | 复用的通道 | source 自动填充 |
|---------|------|-----------|----------------|
| User | 该 session 的入站通道 → 入 guide/pending 队列 → 触发 ReAct | `tx_inbound`（已存在） | `Plugin(PluginSource { name })` |
| Interrupt | 该 session 的中断通道 → select! 中断点 | `tx_interrupt`（已存在） | `Hook` |

`SessionSender` 在 session 装配期由引擎构造（绑定一个插件名 + 该 session 的两条通道），经 `register` 参数传给插件。插件 clone 后保存（已实现 `Clone`）。所有发送方法用 `try_send`（非阻塞），通道满或关闭时仅记 warn 日志，不阻塞钩子执行。

### SessionSender 方法

| 方法 | 用途 |
|------|------|
| `send_user(content)` | 注入 User 消息（默认 Guide 模式，source 自动填 `Plugin(插件名)`） |
| `send_user_with_mode(content, mode)` | 指定消息模式（Guide / Pending） |
| `send_interrupt(reason)` | 发中断信号（source 自动填 `Hook`） |

两条通道的载荷统一为 output 侧类型——插件是内核内组件，直接产出 output 侧消息，不经 input 中间态（与外部 `InputEvent` 经 `Engine::send` 入口转化的路径在地基上统一）。User 消息注入后走与其他用户消息完全相同的链路（入站 → 队列 → 注入时刻过 dispatch 管道），拦截 / 观察机制对它同样生效。

## 执行引擎（HooksRegistry）

```text
registry.rs:
  HooksRegistry 持有该 session 的全部钩子（装配期注册，finalize 后冻结）
  finalize() → 按 priority 稳定排序拦截与观察两类钩子（高优先级先执行）
```

执行规则：

- 拦截钩子：同步串行执行（priority 降序），`&mut` 原地修改链式生效，任一返回 `Some(reason)` 短路
- 观察钩子：异步串行执行（priority 降序、同优先级注册序，发送后），事件以 `Arc` 共享传入；单个超时跳过（`[hooks] timeout_secs`，0 表示不超时）
- 钩子执行期带 **panic 防护**（拦截 / 观察都在 HooksRegistry 内 catch_unwind；观察钩子另带超时防护）——单个钩子 panic 或超时只影响自身，记 warn 后继续
- 装配期 panic 防护双层：`PluginHost::create_instances` 防 `create_instance` panic；引擎装配处再 catch_unwind 防 `register` panic（单个实例崩溃不阻塞其他实例注册）

### SharedHooks：装配后冻结

```text
SharedHooks = Arc<HooksRegistry>
```

注册只发生在 session 装配期（`register_*` 需 `&mut self`）。装配方在所有 register 完成后调一次 `finalize()` 排定两类钩子的优先级，随后包进 `Arc` 冻结——运行期 registry 只读共享给该 session 的所有 dispatch 调用点，**无锁**。hook 闭包如持共享状态（如插件 state），需自行内部同步（如 `std::sync::Mutex`）。

定义在 fuyao-hooks 是为了让 Plugin trait 等签名能引用它，而不产生对 fuyao-core 的环依赖。

### 插件实例生命周期闭环

实例从生成到销毁的完整链路（hooks 的运行义务与实例的销毁义务分离持有）：

```text
session 装配期（assemble_session_hooks）：
  create_instances 生成 (插件名, 实例) 配对 → 逐个 register → finalize
  → (SharedHooks, 实例集合)
       hooks 进 SessionCtx 运行；实例集合进 SessionHandle，只承载 dispose 义务
session 结束（end_session 与 shutdown 共用同一份收尾）：
  task 退出处理完成后 → 逆序逐个 instance.dispose()（后注册的先销毁，panic 防护）
引擎 shutdown 最后：
  PluginHost::dispose_all → 逆序销毁所有插件工厂（引擎级资源；无活跃 session 也调用）
```

关键时序与边界：

- **dispose 在 task 退出后调用**：保证插件 dispose 时观察到的 session 状态已落定（task 内最后的落库与状态更新已结束）
- **逆序销毁**：与装配顺序对称——后注册的插件可能依赖先注册插件的产物，先销毁依赖方
- **两处 dispose 职责不同**：实例 dispose 归 session 收尾（per-session 资源，如刷计数器），工厂 dispose 归引擎 shutdown（引擎级共享资源，如关闭共享连接）
- **失败容错**：插件实例化失败（重名等）该 session 以空 hooks 运行（不硬 panic，session 仍可用），此时实例集合为空、无实例可 dispose

## 类型层级归属

层级约束：`fuyao-guard`(L2) 不能依赖 `fuyao-core`(L3)。因此：

- `SessionSender` + `Plugin` + `PluginInstance` + `PluginHost` + `SimplePlugin` 定义在 **fuyao-hooks**(L1)
- `SessionSender` 持有的通道载荷类型用 **fuyao-api**(L0) 的 output 侧类型（`UserMessage`、`InterruptMessage`）

## 内置插件

| 插件 | crate | 注册的钩子 | 作用 |
|------|-------|-----------|------|
| `LoopGuardPlugin` | fuyao-guard | output_observe（priority 0）+ output_intercept（priority 10）（并存 sender） | 循环检测（工厂 + 每 session 独立 LoopGuardInstance） |

观察钩子走默认档（只读累积检测，无跨钩子顺序依赖）；拦截钩子高于默认档，先于其他拦截器完成警告注入 / 内容替换。

按 `[plugins.enabled]` 配置过滤，未列出默认启用。

## 关键设计决策

### 为什么插件用「工厂 + 实例」两层模型？

多 session 并发时若插件状态共享，session A 的循环计数会影响 session B（串台）。工厂模板引擎级共享（无 per-session 状态），每 session 调 `create_instance` 生成独立实例（持独立 state）。这样：

- 有状态插件（如 LoopGuard）天然 session 隔离
- 无状态插件直接用 `simple_plugin` 一个闭包搞定；走两层模型则 `create_instance` 返回无字段实例，零开销
- 引擎层只持工厂集合，不感知实例细节

### 为什么发消息能力经 register 注入而非 hook？

发消息是「持句柄、随时可用」的能力，而 hook 是「在特定时机被回调」的扩展点——两者生命周期语义不同。若发消息也做成 hook（引擎启动时回调一次、传入 sender），会引入一个只被调用一次、却承担持续能力的伪 hook：它既不拦截也不观察，只是参数投递通道。

经 `register(&mut hooks, &sender)` 注入则把能力交付合并进插件本就要参与的装配阶段：注册钩子与拿发送器是同一次调用，插件要么两者都要、要么忽略 sender 只注册钩子，没有多余的钩子类型。同时 sender 在构造时就绑定插件名，注入消息的 source 自动可追溯，不需要插件自行申报身份。

### 为什么只有拦截 / 观察两类钩子？

两类钩子对应输出消息的两类扩展需求，语义边界清晰：

- 拦截：需要修改 / 阻止 → 同步串行 + 短路
- 观察：只读副作用 → 异步串行、发送后执行

发消息不属此列（它不改也不看正在流经管道的消息，是独立于管道的能力），因此不设第三类钩子。混在一起会导致执行顺序混乱和职责不清。

### 为什么拦截协议用原地 mutate？

拦截是热路径——每条输出事件都要过整条拦截链，协议形态直接决定这条链的成本：

- **不修改的钩子零开销**：`&mut` 纯借用，不碰堆分配。返回值式协议（钩子改完把整个事件还回去）下，即使只想看一眼的钩子也隐含一次整事件传递
- **修改的钩子无 clone 往返**：原地改字段即生效。返回值式协议下每层钩子都要整事件 clone 一次（拿出来、改完再还），链上 N 个钩子就是 N 次整事件克隆，热路径 clone 税高
- **返回值与修改语义显式分离**：`Option<String>` 只表达「通过 / 阻止」，修改经 `&mut` 直接落在事件上——调用方不会把「钩子改了内容」误读成「钩子阻止了事件」，反之亦然

借用检查同时天然保证链式串行：前一个钩子的修改对后续钩子可见，无需任何显式传递协议。

### 为什么观察载荷共享只读（Arc）？

一条事件通常要分发给多个观察钩子（持久化、统计、日志各一个）。若每个钩子都拿 owned 事件，成本随钩子数量线性增长——而观察钩子绝大多数只读几个字段。

事件包进 `Arc` 后，每个钩子只做一次引用计数拷贝（整数递增），事件本体全程零 clone——观察侧成本恒定，不随钩子数量增长。需要 owned 数据的钩子（如要把内容转存进自己的缓冲）自行解引用后 clone 所需字段，把克隆成本精确落在真正需要它的钩子上。

只读共享同时消除了写冲突的可能：观察钩子拿到的类型本身就不支持修改，不靠约定约束。

### 为什么注册表装配后冻结（无锁共享）？

钩子集合是 session 装配期的产物：装配完成后，该 session 会注册哪些钩子就已确定，运行期不会增删。把「注册（可变）」与「执行（只读）」分成两个阶段，运行期就能以 `Arc<HooksRegistry>` 只读共享——dispatch 调用点直接调用，无锁、无 await 竞争。若注册表运行期可变，每个调用点都要过锁或异步互斥，热路径（每条输出事件都过拦截 + 观察）付出无谓的同步成本。

代价是 hook 闭包持有的共享状态（插件 state）需自行内部同步——这把同步成本精确限制在真正有状态的插件上，无状态钩子零开销。

### 为什么砍掉 Plugin 主动通知通道？

此前插件有一条专用的 Plugin 通知通道（session 级独立入口 + 独立转发 task + input/output 两侧的 Plugin 事件变体），用于向外部发「纯通知」信息。砍掉它的依据是：插件需要对外表达的所有语义，都能用既有机制更准确地表达——

- **打断执行** → `SessionSender::send_interrupt`（中断通道，source 标 Hook）
- **引导 AI 调整策略** → `SessionSender::send_user`（User 通道，注入对话历史，AI 真的能读到）
- **修正 AI 收到的反馈** → 拦截钩子把警告注入或替换进工具结果内容（LLM 下轮输入即含修正）
- **纯通知类信息**（人看的运维信息，AI 与流程都不消费）→ tracing 日志（WARN / INFO）

专用通知通道承载的信息三类都不沾：UI 消费它需自行约定 event_type 格式，AI 看不到它，执行流也不受它影响——实际语义就是日志。为「结构化日志」维护一条 session 级通道、两个事件变体与一个转发 task，成本高于收益。砍掉后事件协议更小，session 入口通道只承载真正参与对话流转的消息。

### 为什么 Plugin 是编译期而非运行时加载？

编译期 `impl Plugin` 有类型安全 + 零运行时开销 + panic 可被 catch_unwind 捕获。运行时加载（如 dylib / WASM）引入复杂度（ABI 兼容、安全管理），不符合核心的轻量定位。

### 为什么钩子带 panic 防护？

插件是第三方代码，panic 不应崩溃引擎。`catch_unwind` 确保单个钩子 panic 只影响自身，不影响主流程。装配期双层防护（`create_instance` 与 `register` 各一层）确保插件装配阶段的 panic 也不阻塞 session 启动。

### 为什么重名硬失败？

两个同名插件会导致行为不确定（哪个先注册？哪个的钩子先生效？）。硬失败让开发者在开发阶段就发现问题，而非运行时产生难以排查的行为异常。
