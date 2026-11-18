# 扩展生命周期事件

本文描述当前实现，替代旧的生命周期事件总线设计稿。事件标签和 ABI 定义见 [rpi-plugin-sdk](../../crates/rpi-plugin-sdk/src/lib.rs)，分发实现见 [translate.rs](../../crates/rpi-extensions/src/translate.rs)。

`BeforeTuiStart` 是 rpi 特有的启动事件，在 TUI 初始化前调用。扩展可通过返回值否决启动。
`SessionStart` 和 `SessionShutdown` 处理会话启动与关闭。会话树相关事件在交互式 TUI
和 RPC 导航/压缩路径中派发：`SessionBeforeTree` 在导航前，`SessionTree` 在导航成功后；
`SessionBeforeCompact` 在压缩前，`SessionCompact` 在压缩成功后；`SessionBeforeSwitch`
在新建、导入或切换会话前；`SessionBeforeFork` 在 fork 前。当前这些事件的 veto
仅作 advisory 记录，不会阻止宿主操作。生命周期事件使用无 payload 的 `EventEmpty`，
不是旧设计稿中的 Rust 资源注入总线。

上述 session-tree 标签在本宿主里**不带 payload**：`EventTag` 把它们列在 `data_text()`
的可读集合里（Pi 会给 `{"reason":…}` 之类的 JSON），但 rpi 发出的是空数据，
`EventView::data_text()` 得到 `Some("")`。订阅者应当把“空”当作正常状态，不要直接
`serde_json::from_str` 后 `unwrap`。要判断具体是哪一种切换，用自己缓存的最新分支
状态（`GetSessionBranch`）或后续事件，不要依赖这些标签的参数。

`SessionTree` 与 `SessionCompact` 只在分支真的变了之后才会发：导航被取消或失败、
压缩被拒绝（没有写入 compaction entry）时都不会发，所以订阅者可以把它们当成
“缓存已失效”的可靠信号。`--fork` 启动路径和插件通过 runtime action 发起的
fork/switch/navigate 目前不发这些事件。

宿主在 blocking 线程中调用 handler，并在 FFI 边界捕获 panic。分发使用以下超时预算：

| 事件 | 单个 handler 的超时 |
| --- | --- |
| `BeforeTuiStart` | 5 秒 |
| `SessionShutdown` | 15 秒 |
| 其他生命周期标签 | 10 秒 |

超时后记录日志并跳过等待；已经运行的 blocking handler 仍可能继续执行，超时不会强制终止它。扩展应自行限制阻塞和资源占用，且不能让 panic 穿过 ABI 边界。

扩展注册、支持的事件与工具生命周期见 [扩展开发指南](authoring.md)。
