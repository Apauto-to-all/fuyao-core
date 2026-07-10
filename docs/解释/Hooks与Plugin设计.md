# Hooks 与 Plugin 设计

> 本文解释钩子系统的三类钩子、Plugin trait、执行引擎与安全防护。API 签名见 `cargo doc --workspace`。

## 设计目标

提供 Agent 行为的扩展点，不修改引擎内核即可：
- 拦截 / 修改 / 取消 Agent 的输入输出
- 观察 Agent 的行为（只读副作用）
- 主动向 Agent 注入消息

## 三类钩子

### 拦截型（可修改 / 阻止）

| 钩子 | 作用 | 返回 | 执行方式 |
|------|------|------|---------|
| BeforeLlmFn | LLM 调用前注入消息 / skip_tools | BeforeLlmOutput | 异步，串行 |
| OutputInterceptFn | 修改 / 阻止 OutputEvent | InterceptResult | 同步，串行，任一 Block 短路 |
| OnLlmErrorFn | LLM 错误时重试 / 中止 | LlmErrorAction | 同步，首个非 Retry 返回 |

> **默认 = 无限重试**：无 `on_llm_error` 钩子（或所有钩子均返回 Retry）时，引擎**永不放弃**重试——退避指数增长但无次数上限。必须注册 Abort 钩子才能终止（LoopGuard 的 Plugin 目前不覆盖此钩子）。

```text
InterceptResult<T> = Pass(T) | Block(String)
```

Block 短路后续钩子——拦截失败但不丢失信息（Block 带原因字符串）。

### 观察型（只读副作用）

| 钩子 | 作用 | 执行方式 |
|------|------|---------|
| OutputObserveFn | 持久化 / 日志 / 统计 | 异步，串行，OutputEvent 发送后执行 |

观察钩子不能修改事件，只做副作用。

### 主动型（插件自主发送）

| 钩子 | 作用 | 执行方式 |
|------|------|---------|
| SendInputFn | 拿 Sender<InputEvent>，插件随时发任意输入 | 引擎启动时调一次 |

这是"真·主动"——插件拿到 Sender 后可在任何时刻 try_send InputEvent（User / Interrupt / Shutdown）。

## Plugin 系统

### Plugin trait

```text
trait Plugin {
    fn name() -> &str
    fn identity() -> PluginEventSource    // 默认桥接 name
    fn register(&SharedHooks)             // 默认空，注册钩子
    fn dispose()                          // 默认空，清理资源
}
```

Plugin 是**编译期**扩展点——`impl Plugin` 后编译进二进制，运行时按 `[plugins.enabled]` 过滤启用。

### PluginHost

| API | 说明 |
|-----|------|
| `add(plugin)` | 添加（不安装） |
| `install(&hooks)` | 安装（调 plugin.register(&hooks)） |
| `dispose_all()` | 清理所有插件 |
| `list()` | 列出已安装插件名 |

重名硬失败（panic）——防止两个插件同名导致行为不确定。

### PluginEmitter

绑定插件身份（identity），向 UI 发送 Plugin 事件。panic / 重试不阻塞主流程。

## 执行引擎（HooksRegistry）

```text
registry.rs:
  HooksRegistry 持有所有注册的钩子函数
```

执行规则：
- 拦截钩子：串行执行，任一 Block 短路
- 观察钩子：串行执行（发送后）
- 所有钩子带 **panic 防护**（catch_unwind）——单个钩子 panic 只影响自身
- 可选**超时**（读 `[hooks] timeout_secs`，默认 5s，0 表示不超时）

### SharedHooks

```text
SharedHooks = Arc<Mutex<HooksRegistry>>
```

定义在 L1 的 fuyao-hooks，fuyao-core re-export——避免环依赖（Plugin trait 签名需要引用它，而 core 依赖 hooks）。

## 内置插件

| 插件 | crate | 注册的钩子 | 作用 |
|------|-------|-----------|------|
| SessionPlugin | fuyao-session | before_llm / output_observe / send_input | 会话持久化 + 上下文压缩 |
| LoopGuardPlugin | fuyao-guard | output_observe | 循环检测 |

按 `[plugins.enabled]` 配置过滤，未列出默认启用。

## 关键设计决策

### 为什么分拦截 / 观察 / 主动三类？

三种扩展需求的语义完全不同：
- 拦截：需要修改 / 阻止 → 串行 + 短路
- 观察：只读副作用 → 不影响主流程
- 主动：插件自主发消息 → 需要持有 Sender

混在一起会导致执行顺序混乱和职责不清。

### 为什么 Plugin 是编译期而非运行时加载？

编译期 `impl Plugin` 有类型安全 + 零运行时开销 + panic 可被 catch_unwind 捕获。运行时加载（如 dylib / WASM）引入复杂度（ABI 兼容、安全管理），不符合核心的轻量定位。

### 为什么钩子带 panic 防护？

插件是第三方代码，panic 不应崩溃引擎。catch_unwind 确保单个钩子 panic 只影响自身，不影响主流程。

### 为什么重名硬失败？

两个同名插件会导致行为不确定（哪个先注册？哪个的钩子先生效？）。硬失败让开发者在开发阶段就发现问题，而非运行时产生难以排查的行为异常。
