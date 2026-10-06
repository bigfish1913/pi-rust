# 扩展生命周期事件

本文描述当前实现，替代旧的生命周期事件总线设计稿。事件标签和 ABI 定义见 [rpi-plugin-sdk](../../crates/rpi-plugin-sdk/src/lib.rs)，分发实现见 [translate.rs](../../crates/rpi-extensions/src/translate.rs)。

`BeforeTuiStart` 是 rpi 特有的启动事件，在 TUI 初始化前调用。扩展可通过返回值否决启动。`SessionStart` 和 `SessionShutdown` 处理会话启动与关闭。生命周期事件使用无 payload 的 `EventEmpty`，不是旧设计稿中的 Rust 资源注入总线。

宿主在 blocking 线程中调用 handler，并在 FFI 边界捕获 panic。分发使用以下超时预算：

| 事件 | 单个 handler 的超时 |
| --- | --- |
| `BeforeTuiStart` | 5 秒 |
| `SessionShutdown` | 15 秒 |
| 其他生命周期标签 | 10 秒 |

超时后记录日志并跳过等待；已经运行的 blocking handler 仍可能继续执行，超时不会强制终止它。扩展应自行限制阻塞和资源占用，且不能让 panic 穿过 ABI 边界。

扩展注册、支持的事件与工具生命周期见 [扩展开发指南](authoring.md)。
