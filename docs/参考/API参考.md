# API 参考

fuyao-core 的完整 API 参考由 rustdoc 自动生成，手写文档不抄录签名（避免双份维护过时）。

## 生成方式

```bash
cargo doc --workspace --no-deps --open
```

生成后用浏览器打开 `target/doc/fuyao_api/index.html`，或从各 crate 首页导航：

| crate | 文档入口 |
|-------|---------|
| fuyao-api | `target/doc/fuyao_api/` |
| fuyao-provider | `target/doc/fuyao_provider/` |
| fuyao-mcp | `target/doc/fuyao_mcp/` |
| fuyao-skills | `target/doc/fuyao_skills/` |
| fuyao-hooks | `target/doc/fuyao_hooks/` |
| fuyao-prompt | `target/doc/fuyao_prompt/` |
| fuyao-guard | `target/doc/fuyao_guard/` |
| fuyao-core | `target/doc/fuyao_core/` |
| fuyao-session | `target/doc/fuyao_session/` |
| fuyao-tools | `target/doc/fuyao_tools/` |
| fuyao-app | `target/doc/fuyao_app/` |

## 查什么用 rustdoc

- 公开结构体 / 枚举的字段与方法签名
- trait 的方法列表与实现者
- 类型继承与 re-export 关系
- 函数参数 / 返回值 / 错误类型

手写参考文档（配置项、事件协议、crate 能力清单）聚焦 rustdoc 做不了的：toml 配置语法、事件业务语义、crate 间分层关系。
