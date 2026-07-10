# Guard 防护系统设计

> 本文解释循环检测插件的三类钩子、无状态检测器、四级严重程度与重置语义。API 签名见 `cargo doc --workspace`；配置见 [配置项参考](../参考/配置项参考.md) 的 `[guard.loop]`。

## 架构

```text
LoopGuardPlugin（实现 Plugin trait，注册 3 个钩子）
  └─ LoopGuardState（协调层，持有两个子检测器 + 中断 / 注入逻辑）
       ├─ ToolLoopGuard（工具循环状态机）
       │    └─ detectors.rs 纯函数（工具重复 / 序列检测）
       └─ TextLoopGuard（文本循环状态机）
            └─ detectors.rs 纯函数（文本自相似检测）
```

Guard 经 Plugin 接入引擎（见 [Hooks 与 Plugin 设计](Hooks与Plugin设计.md)），不持有引擎内部 channel，所有主动发消息能力通过 `send_input` 钩子获取的 PluginEmitter 实现。

## 三个钩子

| 钩子 | 类型 | 阶段 | 职责 |
|------|------|------|------|
| `output_observe` | 异步副作用 | UI **后** | 检测累积：处理 ToolCall / Chunk，跑检测器，设 pending |
| `output_intercept` | 同步可修改 | UI **前** | 注入 / 修改：拦截 ToolResult，读 pending，追加或替换 content |
| `send_input` | 引擎启动时 | — | 获取 PluginEmitter，后续发 Plugin / Interrupt / User 消息 |

> intercept 在事件到达 UI **之前**（可修改），observe 在事件到达 UI **之后**（只读副作用）。这意味着工具检测是"事后"的——ToolCall 已发给 UI，检测在 observe 跑，反应（注入警告）发生在**下一个** ToolResult 的 intercept。

## 无状态检测器（detectors.rs）

三个纯函数，只接收数据 + 阈值，返回 `Option<String>`（检测描述）。状态由调用方状态机管理。

### 工具重复检测

- **什么算重复**：工具名 AND 参数字符串完全相同
- **计数**：从历史记录从后往前连续匹配，当前调用计入
- **触发**：count ≥ threshold（默认 4）

### 工具序列检测

- **检测什么**：A→B→A→B 交替循环模式
- **算法**：在窗口内找最短重复模式，要求完整重复 ≥ 2 次且无剩余
- **窗口**：由 `tool_alternate_threshold`（默认 6）派生

### 文本自相似检测

- **算法**：trigram（3-gram）Jaccard 相似度
- **窗口**：取累积文本末尾两个相邻窗口（`streaming_window_ratio`，默认 0.2 = 末尾 20%，下限 20 字符）
- **触发**：相似度 ≥ threshold（默认 0.6）
- UTF-8 安全（用 char_indices 切分，中文正确处理）

## 四级严重程度

检测到循环后，按升级链路分级响应：

```text
Warn → Inject → Interrupt → Abort
```

| 级别 | 触发 | 动作 |
|------|------|------|
| **Warn** | 第 threshold 次 | emit_plugin 通知 + 设 pending_warn（ToolResult 前追加警告） |
| **Inject** | 再犯 | emit_plugin + 设 pending_inject（ToolResult 内容替换为拦截消息） |
| **Interrupt** | 继续犯 | send_interrupt（中断当前轮）+ send_inject_message（注入引导消息到对话历史）|
| **Abort** | interrupt_count ≥ 3 | aborted=true + send_interrupt（彻底终止，不再注入引导） |

> 工具路径有 Inject 级别，文本路径只有 Warn → Interrupt（无 Inject）。

### 中断引导消息

| interrupt_count | 引导消息 |
|----------------|---------|
| 1 | 你的输出内容在重复。请直接给出结论 |
| 2 | 你已多次输出重复内容。请立即停止，用一句话总结 |
| ≥3（Abort） | AI 在多次干预后仍持续重复，已彻底终止 |

## 重置语义

Guard 按用户消息来源区分重置范围：

| User 来源 | 行为 | 目的 |
|----------|------|------|
| `User`（用户主动） | `reset_turn()`：清空**所有**状态 | 用户开新对话 → 全新检测上下文 |
| `Plugin` / `System` | `clear_pending()`：**仅清 pending**，保留历史 + 计数 | 插件注入引导后，检测保持"热"状态——AI 若继续循环，1 次即再次触发 |

> 这解决了"注入引导后 AI 继续循环"的场景——不清空历史，检测器"记得"之前的循环行为。

## 已知局限

1. **文本检测仅对流式生效**：只处理 Chunk 事件，不处理 Assistant（最终聚合消息）。非流式 Provider 不触发文本检测。

2. **streaming_check_interval 单位**：配置注释写"每隔 N 个字符"，实现用的是 `String::len()`（字节长度）。对 UTF-8 中文，实际约每 N/3 个字符检查一次。

3. **aborted 标志未读**（已知 TODO）：`aborted` 布尔标志只被 SET 和 RESET，从未在生产代码路径中被 READ 作为条件判断。Abort 触发后 Guard 自身不再阻断后续工具调用。空工具结果的问题源于中断机制与工具执行生命周期的交互，不在 guard crate 内部。

## 关键设计决策

### 为什么检测器是无状态纯函数？

状态与逻辑分离。检测器只管"给定历史 + 当前调用，是否构成循环"，状态机管"升级到哪个级别、何时重置"。这让检测器可独立单元测试，且未来加新检测模式只需加纯函数。

### 为什么工具检测是"事后"的（observe）而非"事前"拦截？

ToolCall 已经发给 UI 显示了。检测在 observe（UI 后）跑，反应在下一个 ToolResult 的 intercept（UI 前）注入。这样 UI 能看到完整的工具调用过程，而注入的警告/拦截让 LLM 收到修正后的反馈。如果事前拦截 ToolCall，UI 会丢失调用信息。

### 为什么 Plugin 注入后不重置历史？

注入引导消息后，如果 AI 仍执行相同操作，说明引导无效。保留历史 + 计数让检测器"记得"之前的循环，1 次重复就再次触发 Interrupt——快速升级，避免 AI 在无效循环中浪费 token。
