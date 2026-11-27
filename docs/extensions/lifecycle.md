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

## 卸载钩子：`register_plugin_lifecycle`

每次插件注册可通过 `PluginApi::register_plugin_lifecycle` 安装一对**同步**关闭钩子。
该槽位是 ABI v4 追加的可选槽位。支持该槽位的 SDK 在较旧的宿主上可能读取到 `None`。
调用 `register(begin, finish, registration_data)` 时传入由插件为本次注册分配的上下文。
`api.user_data` 由宿主拥有，供对应的宿主 API 使用。检查 registrar 的返回值，仅在成功
安装钩子后启动后台工作或把该上下文交给其他 registrar。安装失败时由插件回收上下文。
缺少槽位时，只启用能独立安全回收的能力。

调用时序与语义：

- `begin` 在宿主持有会话锁**之外**调用，必须幂等、非阻塞。它拒绝本次注册的新工作并
  通知在飞工作退出，不 join 线程，也不回调宿主。
- 宿主通过 `PluginKeepalive` 保活 `Library`。相关工具适配器、事件分发器、在飞 FFI
  调用和插件字符串释放都交还 keepalive 后，宿主在 blocking 清理线程调用 `finish`。
  在这里 join 本次注册的后台工作并释放资源。确认本次注册的后台线程、任务与回调都已
  结束后，释放插件上下文并返回 `0`。
- `finish` 返回非零时，宿主保留该 library 引用与宿主回调上下文直到进程退出。仍被
  后台工作使用的插件上下文必须继续有效。
- 会话关闭、`/new`（换会话）和插件 reload（`/reload` 与 `rpi dev` 的 watch）都可
  触发注册清理。入口返回非零时，宿主丢弃该次注册的部分结果，也会执行已成功安装的钩子。
- 启用共享映像时，旧注册的 `begin` 与 `finish` 可以与新注册或其他会话的回调重叠。
  两个钩子只处理本次注册持有的资源。最后一个会话关闭后，Host 仍可保留映像及其全局状态。

扩展注册、支持的事件与工具生命周期见 [扩展开发指南](authoring.md)。
