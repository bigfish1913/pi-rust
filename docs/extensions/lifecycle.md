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

事件之外还有一对**同步**的卸载钩子，由 `PluginApi::register_plugin_lifecycle` 注册
（追加在 vtable 尾部，ABI 版本不变；旧宿主或旧 SDK 没有该槽位）。它解决的是事件
表达不了的问题：宿主准备 `dlclose` 这个 cdylib 时，插件自己的后台线程还在执行它的代码。

```rust
extern "C" fn begin(_ud: *mut c_void) { /* 幂等、非阻塞：置停止标志、取消在飞请求 */ }
extern "C" fn finish(_ud: *mut c_void) -> i32 { /* join 线程；0 = 可以卸载 */ }
let _ = api.register_plugin_lifecycle.map(|register| register(begin, finish, api.user_data));
```

调用时序与语义：

- `begin` 在宿主持有会话锁**之外**调用，必须幂等、非阻塞：只做「拒绝新工作 + 通知
  在飞工作退出」，不得 join，也不得回调宿主（此时宿主动作已标记为 stale）。
- `finish` 在宿主的 blocking 清理线程上调用，等所有在飞 FFI 调用与插件字符串释放
   relinquish keepalive 之后执行；返回 `0` 才表示「不会再有插件代码被执行」。
- `finish` 返回非 0 时，宿主**永久保留**该 library 与它的 runtime 上下文，进程退出前
  不会再尝试卸载——宿主**不会**超时之后照样卸载活代码。因此返回非 0 是安全的
  「我还没收干净」，但会泄漏一次加载。
- 三条路径都会走这对钩子：`SessionShutdown`、`/new`（换会话）以及**插件 reload**
  （`/reload` 与 `rpi dev` 的 watch）。纯插件 reload 不再需要另设 unload 事件。
- 常驻线程的插件（回环服务、自管 Tokio 运行时）**必须**注册这对钩子；`finish` 里
  join 线程并释放端口，否则 reload 会留下一个执行已卸载代码的线程。

工具闭包与事件 handler 的保活由宿主负责（`PluginKeepalive` 持有 `Library`，直到最后
一个工具适配器/事件分发器释放），所以不需要插件自己做引用计数。

扩展注册、支持的事件与工具生命周期见 [扩展开发指南](authoring.md)。
