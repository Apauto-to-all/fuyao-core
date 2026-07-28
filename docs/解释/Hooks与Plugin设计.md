# Hooks 与 Plugin 设计

> 本文解释钩子系统的三类钩子、Plugin 两层模型（工厂 + session 实例）、SessionSender 三通道分流、执行引擎与安全防护。API 签名见 `cargo doc --workspace`。

## 设计目标

提供 Agent 行为的扩展点，不修改引擎内核即可：

- 拦截 / 修改 / 取消 Agent 的输入输出
- 观察 Agent 的行为（只读副作用）
- 主动向 Agent 注入消息

## 统一消息处理管道（dispatch）

所有 OutputEvent 必经此管道（四段串行），管道在每个 session 内运行：

```text
OutputEvent（输入转化的、引擎内部产生的）
   ↓
┌─ 拦截（intercept）─┐  插件可修改或阻断（InterceptResult::Block 短路丢弃）
└────────┬───────────┘
         ↓ Block 则丢弃整条（不处理、不发送、不观察）
┌─ 处理（process）───┐  引擎内部业务逻辑（入队/收集批次/投递信号），耗时动作在管道外
└────────┬───────────┘
         ↓
┌─ 发送（deliver）───┐  Emitter::emit（盖 session_id + tx.send）
└────────┬───────────┘
         ↓
┌─ 观察（observe）───┐  插件只读副作用（持久化/日志/统计）
└─────────────────────┘
```

| 段 | 职责 | 关键约束 |
|----|------|---------|
| **拦截** | 插件可改、可阻 | 链式（每个拿到前一个的输出）；Block 立即短路 |
| **处理** | 引擎内部业务逻辑 | **只做轻量、不阻塞的动作**（入队、收集、投递）；耗时动作（工具执行）在管道外 |
| **发送** | 推到该 session 出站通道 | Emitter::emit，盖 session_id 后 `tx_event.send`（per-session 通道；fan-in 由 App 承担） |
| **观察** | 插件只读副作用 | 不能改数据、不能阻；发送在前、观察在后（串行） |

## 三类钩子

### 拦截型（可修改 / 阻止）

| 钩子签名 | 作用 | 返回 | 执行方式 |
|---------|------|------|---------|
| `OutputInterceptFn` | 修改 / 阻止 OutputEvent | `InterceptResult<OutputEvent>` | 同步，串行，任一 Block 短路 |

```text
InterceptResult<T> = Pass(T) | Block(String)
```

Block 短路后续钩子——拦截失败但不丢失信息（Block 带原因字符串）。

### 观察型（只读副作用）

| 钩子签名 | 作用 | 执行方式 |
|---------|------|---------|
| `OutputObserveFn` | 持久化 / 日志 / 统计 | 异步，串行，OutputEvent 发送后执行 |

观察钩子不能修改事件，只做副作用。

### 主动型（插件自主发送）

| 钩子签名 | 作用 | 执行方式 |
|---------|------|---------|
| `SendInputFn` | 拿 `SessionSender`，插件随时发 User / Interrupt / Plugin | 每 session 装配时调一次 |

这是"真·主动"——插件拿到 `SessionSender` 后可在任何时机发消息（不依赖 emit 频率）。

## Plugin 两层模型（工厂 + session 实例）

多 session 并发时，**每个 session 拥有自己独立的一份插件实例**，互不串台（session A 的循环计数不影响 session B）。隔离靠「工厂 + 实例」两层模型：

```text
引擎级（启动一次注册）：
  PluginHost 持有若干 Plugin（工厂模板）
         │ 只持有配置/共享依赖，无 per-session 状态
         ↓ 每个 session 启动时调 create_instance()
session 级（每 session 独立）：
  PluginInstance（实例）持该 session 独立状态
         ↓ register(&mut HooksRegistry) 把 hook 注册到该 session 私有 registry
  HooksRegistry → 绑定到该 session 的 dispatch 管道
```

### Plugin trait（工厂模板）

```text
trait Plugin {
    fn name(&self) -> &str;
    fn identity(&self) -> PluginEventSource;            // 默认桥接 name
    fn create_instance(&self) -> Box<dyn PluginInstance>;  // 每 session 调一次，生成独立实例
}
```

Plugin 是**编译期**扩展点——`impl Plugin` 后编译进二进制，运行时按 `[plugins.enabled]` 过滤启用。

- **无状态插件**：`create_instance` 返回无字段实例，闭包本身无状态，天然隔离
- **有状态插件**（如 LoopGuard）：`create_instance` 每 session 新建独立状态（独立的循环计数器/检测器），多 session 并发互不串台

### PluginInstance trait（session 实例）

```text
trait PluginInstance {
    fn register(&self, hooks: &mut HooksRegistry);  // 把 hook 注册到该 session 私有 registry
}
```

每 session 装配时 `assemble_session_hooks` 调 `create_instances` 生成实例集，每个 `instance.register(&mut registry)` 注册到该 session 私有的 `HooksRegistry`。

### PluginHost

| API | 说明 |
|-----|------|
| `add(plugin: Box<dyn Plugin>)` | 添加工厂（不立即创建实例） |
| `create_instances() -> Result<Vec<Box<dyn PluginInstance>>, PluginInstallError>` | 每 session 调一次，生成所有插件的独立实例（含重名检查 + create_instance panic 防护） |
| `list()` | 列出已注册工厂名 |

重名硬失败（`PluginInstallError::DuplicateName`）——防止两个插件同名导致行为不确定。

## SessionSender：三通道分流

`SessionSender` 封装"往**这个 session** 发消息"的能力，三种消息类型自动分流到该 session 的三条独立通道：

| 消息类型 | 走向 | 复用的通道 |
|---------|------|-----------|
| User | 该 session 的入站通道 → dispatch 管道 → 触发 ReAct | `tx_inbound`（已存在） |
| Interrupt | 该 session 的中断通道 → select! 中断点 | `tx_interrupt`（已存在） |
| Plugin | 该 session 的 Plugin 通道 → dispatch 管道 → 直接发外部（不参与 ReAct） | `tx_plugin`（新增） |

`SessionSender` 在 `send_input` hook 回调里由引擎注入（`assemble_session_hooks` 的步骤 4：`registry.init_send_inputs(sender).await`），绑定一个插件身份（自动填充 Plugin 消息的 source 字段）。所有发送方法用 `try_send`（非阻塞），失败仅记 warn。

### SessionSender 方法

| 方法 | 用途 |
|------|------|
| `send_user(content)` | 注入 User 消息（默认 Guide 模式，source 自动填 `Plugin(identity)`） |
| `send_user_with_mode(content, mode)` | 指定消息模式（Guide / Pending） |
| `send_interrupt(reason)` | 发中断信号 |
| `send_plugin(event_type, message)` | 发 Plugin 通知（带 identity） |
| `send_plugin_data(event_type, data)` | 发 Plugin 通知（带任意 JSON data） |
| `send_plugin_full(source, event_type, data, error, message)` | 完全自定义所有字段 |

## Plugin 消息也走 dispatch 管道

外部 `InputEvent::Plugin`（插件通知）**不在 Engine 层直接转 `OutputEvent::Plugin` 发外部**——那样会绕过 dispatch 管道，无法被 intercept/observe。

正确链路：

```text
InputEvent::Plugin → Engine::send 路由到该 session 的 tx_plugin 通道
                  → session task 的 select! 收到 → handle_inbound_plugin
                  → 转成 OutputEvent::Plugin 过完整 dispatch 管道
                  → Emitter 自动盖 session_id 标签发外部
```

这样所有消息（User/Interrupt/Plugin）统一经 dispatch 管道，拦截/观察机制对它们都生效。

## 执行引擎（HooksRegistry）

```text
registry.rs:
  HooksRegistry 持有所有注册的钩子函数（该 session 私有）
  init_send_inputs(sender) → 把 SessionSender 传给所有 send_input hook
```

执行规则：

- 拦截钩子：串行执行，任一 Block 短路
- 观察钩子：串行执行（发送后）
- 所有钩子带 **panic 防护**（`catch_unwind` + `panic_payload_to_string`）——单个钩子 panic 只影响自身
- panic 防护双层：`PluginHost::create_instances` 防 `create_instance` panic；Engine 再加一层 `catch_unwind` 防 `register` panic（单个实例崩溃不阻塞其他）

### SharedHooks

```text
SharedHooks = Arc<tokio::sync::Mutex<HooksRegistry>>
```

每 session 装配时新建一份（per-session 独立），通过 dispatch 管道使用。定义在 L1 的 fuyao-hooks 是为了让 Plugin trait 等签名能引用它，而不产生对 fuyao-core 的环依赖。

## 类型层级归属

层级约束：`fuyao-guard`(L2) 不能依赖 `fuyao-core`(L3)。因此：

- `SessionSender` + `Plugin` + `PluginInstance` + `PluginHost` trait 定义在 **fuyao-hooks**(L1)
- `SessionSender` 持有的通道载荷类型用 **fuyao-api**(L0) 的类型（如 `InboundUser`、`InterruptMessage`、`PluginMessage`）
- 为此把 `InboundUser`（原 core 的 `pub(crate)` 类型）**提升到 fuyao-api**，让 hooks 能引用

## 内置插件

| 插件 | crate | 注册的钩子 | 作用 |
|------|-------|-----------|------|
| `LoopGuardPlugin` | fuyao-guard | output_observe / output_intercept / send_input | 循环检测（工厂 + 每 session 独立 LoopGuardInstance） |

按 `[plugins.enabled]` 配置过滤，未列出默认启用。

## 关键设计决策

### 为什么插件用「工厂 + 实例」两层模型？

多 session 并发时若插件状态共享，session A 的循环计数会影响 session B（串台）。工厂模板引擎级共享（无 per-session 状态），每 session 调 `create_instance` 生成独立实例（持独立 state）。这样：

- 有状态插件（如 LoopGuard）天然 session 隔离
- 无状态插件 `create_instance` 返回无字段实例，零开销
- 引擎层只持工厂集合，不感知实例细节

### 为什么统一原则是「插件一切能力都是 hook」？

铁律：插件的所有能力（观察/拦截/发消息）都只能是 hook。**没有 hook 之外的"特殊注入通道"**。新增能力 = 新增 hook 类型。

这条原则的具体体现：发消息能力**不通过** register 参数注入，而是通过 `send_input` hook 获得。插件注册 send_input hook → 引擎在 session 启动时调用该 hook，传入 `SessionSender` → 插件保存后随时调用。

### 为什么分拦截 / 观察 / 主动三类？

三种扩展需求的语义完全不同：

- 拦截：需要修改 / 阻止 → 串行 + 短路
- 观察：只读副作用 → 不影响主流程
- 主动：插件自主发消息 → 需要持有 SessionSender

混在一起会导致执行顺序混乱和职责不清。

### 为什么 Plugin 是编译期而非运行时加载？

编译期 `impl Plugin` 有类型安全 + 零运行时开销 + panic 可被 catch_unwind 捕获。运行时加载（如 dylib / WASM）引入复杂度（ABI 兼容、安全管理），不符合核心的轻量定位。

### 为什么钩子带 panic 防护？

插件是第三方代码，panic 不应崩溃引擎。`catch_unwind` 确保单个钩子 panic 只影响自身，不影响主流程。双层防护（`create_instance` + `register` 各一层）确保装配阶段的 panic 也不阻塞 session 启动。

### 为什么重名硬失败？

两个同名插件会导致行为不确定（哪个先注册？哪个的钩子先生效？）。硬失败让开发者在开发阶段就发现问题，而非运行时产生难以排查的行为异常。

### 为什么 SessionSender 三通道？

User 和 Interrupt 通道本来就存在（session task 用），完全复用。只有 Plugin 消息需要新增 session 级入口 `tx_plugin`——因为 Plugin 消息要"直接进 dispatch 管道发外部，不参与 ReAct"。三通道各自独立，消息类型清晰分流。
