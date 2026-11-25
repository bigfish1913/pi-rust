# 宿主管理的子 agent

扩展通过 `RuntimeActionId::GetExecutionContext` 和 `ChildAgent` 创建独立任务。
这两个 action 追加在现有 ABI 后面，不改变 `PluginApi` vtable 的布局。
旧宿主对未知 action 返回明确错误；不要静默改为使用默认配置的子进程。

## 有效执行上下文

`GetExecutionContext`（23），参数 `{}`，返回父宿主的当前状态，例如：

```json
{
  "model": {"id":"deepseek-flash","provider":"deepseek","api":"openai-completions"},
  "thinkingLevel":"high",
  "tools":["read","bash","delegate_task"],
  "cwd":"D:/Projects/example",
  "sessionId":"parent-session-id",
  "childAgent":true
}
```

这不是 CLI flags 的副本。交互切换模型或 thinking 后，读取的是有效状态。
公开对象不包含 API key、认证头、endpoint、原始 Model 或 provider binding。
`childAgent` 表示当前 harness 是否允许创建子任务；并非资源可用性探测或启动保证。

## 生命周期

`ChildAgent`（24）使用 `op` 分派。插件应从工作线程调用同步 ABI，不能在
单线程 Tokio executor 内同步等待宿主 action。所有结果字符串由宿主的
`free_string` 释放。

### 调用身份（必需）

`PluginApi.user_data` 是宿主注册表中的 opaque handle，不是可解引用地址；
失效 handle 返回错误，ID 不复用。宿主为每次工具调用创建 invocation，
在 `__rpi` 中注入 `runtimeHandle`（整数）和随机 `runtimeKey`（字符串）。
这些值不属于公开执行上下文，不能写入模型输出、持久化参数或跨调用缓存。

- 同步 execute/poll/cancel/destroy 内调用 action 时，宿主自动使用当前 driver 的
  invocation，插件不能通过注册时缓存的上下文把 child 调用冒充为父调用。
- 插件自行创建的工作线程必须用该次 `runtimeHandle` 作为 runtime_action 的
  `user_data`，并在每个 ChildAgent 参数对象中添加 `__rpiRuntimeKey: runtimeKey`。
  以下 op 示例省略此字段；宿主校验后将它移除，不传递给业务 action。
- ChildAgent 拒绝仅注册级、已取消、失效或 child-originated invocation，
  无论工具叫 `delegate_task` 还是其他别名。缺少这些能力时明确提示升级，不回退。
- 注册实例的裸指针不能保存在插件全局状态中。可缓存进程级函数指针，但每次
  工作线程必须绑定本次调用的 handle；候选 reload 回退不影响旧会话的绑定。

### start

```json
{"op":"start","task":"检查 parser","role":"reviewer","maxTurns":8,"timeoutSeconds":300,"tools":["read"]}
```

- `task` 必填，1–20,000 bytes；`role` 默认 `worker`，1–200 bytes。
- `maxTurns` 默认 8，范围 1–32；`timeoutSeconds` 默认 300，范围 10–1800。
  宿主拒绝非法类型或越界值，不隐式扩张预算。
- `tools` 可省略以继承有效父工具；指定时只能缩小集合。`[]` 表示无工具。
- `delegate_task` 从子任务的工具注册表中删除；child harness 和工具执行作用域
  均拒绝创建孙任务，插件别名也不能经根宿主绕过子任务工具子集。
- 不能通过 JSON 覆盖 cwd、model、凭据、审批规则或加载额外扩展。

立即返回 `{"childId":"opaque-id","sessionId":"child-session-id","status":"running"}`。
父 lane 正在执行工具时也能启动，不会重入父 lane。

### poll

```json
{"op":"poll","childId":"opaque-id"}
```

返回 `status`（`running` / `done` / `failed`）、`deltas` 字符串数组、`turns`、
`result` 和 `error`。poll 不等待完整运行；通常每 25ms 调用一次。
文本增量按顺序消费，一批目标为 64 KiB（单个更大的 delta 单独交付）。
终态在待交付增量排空后返回，避免客户端丢掉末尾内容。

`done` 的 `result` 包含运行的 `status`、`text`、可能的 `error`、`turns` 和
`turnLimited`。客户端还必须检查内部 `status` 是否为 `completed`；
`done` 只表示生命周期结束，不等于任务成功。`turnLimited` 表示到达回合预算，
报告可能不完整。运行失败、取消、超时和输出超限都有明确诊断。

### cancel / dispose

```json
{"op":"cancel","childId":"opaque-id"}
{"op":"dispose","childId":"opaque-id"}
```

`cancel` 设置取消信号并使错误可观察，保留 handle 供 poll 查询。
`dispose` 同时取消并删除 handle；成功、错误或客户端取消后都必须执行。
未知或已释放 handle 返回错误。handle 绑定创建它的插件 owner，其他插件
不能通过知道 ID 来读取、取消或释放任务，JSON 不能伪造 owner。

## 继承与清理边界

- 子任务在新的内存 Session 中运行，不产生持久化会话文件，也不复制或修改父历史。
- 同一快照继承当前模型、thinking、系统提示词、资源、请求选项、provider Arc
  及其私有凭据绑定，而不是重新解析配置。端点与认证数据不经过插件。
- 工具、provider hooks 和审批 hook 继承现有对象，不重新发现全局插件，
  因此父 `--package-only`、显式扩展加载及工具范围保持有效。
- 工具继承父项目的执行环境/cwd。共享的扩展工具仍由父宿主所有，注入的
  `__rpi.sessionId` 表示其父扩展状态空间；独立执行会话用返回的 `sessionId`
  标识。这不是为每个子任务重新注册一套插件。
- 权限门与 `ask_user` 使用父 UI mailbox，无需 RPC 转发。无交互消费者时
  保留显式错误/拒绝行为，不能默认允许。子任务取消信号送达工具和审批等待。
- 父运行结束/取消、父 harness close/释放、会话切换和所属插件卸载/reload 会取消子任务。
  即使 runtime action 已排队或父运行空闲，原工具调用的取消信号仍会阻止/取消 start。
  正确的父 invocation capability 在取消后仍可 poll/cancel/dispose，不能再 start；
  不允许通过取消来跳过清理，也不允许错 owner 或子调用借此访问任务。
  前台取消先协作清理，最多再等待 2 秒；超出宽限期的后台 driver 仍拥有参数、
  DLL keepalive 和调用身份，直到工具 execute/poll/destroy、provider 请求或审批回调
  真正返回。清理任务继续保留活动名额，不能通过 dispose 或宽限期超时绕过并发限制。
  宿主 runtime binding 弱持有父 harness，避免插件/provider 形成保活环。
  不承诺强制终止不配合取消的外部线程。
- 每宿主最多保留 16 个 handle，同时最多 16 个活动/清理中任务。完成任务不自动
  删除，插件必须 dispose。文本增量与最终文本共享 4 MiB 总预算，增量最多 8192 个。
- 回合预算属于 child 实例，不通过修改全局环境变量实现。

这是能力继承与生命周期隔离，不是 OS 安全沙箱。子任务允许使用的 shell 或其他
工具仍有其正常能力；需要只读任务时显式缩小工具集并配置权限规则。

## 跨仓库验证

```sh
# D:/Projects/rpi-extension
cargo test -p rpi-subagents
cargo build -p rpi-subagents

# D:/Projects/pi-rust，将变量设为实际 DLL / so / dylib 路径
RPI_TEST_SUBAGENT_LIBRARY=/path/to/rpi_subagents.dll cargo test -p rpi-cli --test subagent_packages
```

集成测试加载真实 DLL，使用本地 faux provider 和临时项目，验证进度、cwd、工具
范围、父审批 UI、headless 拒绝、别名递归拒绝、重复注册和 reload 回退，
以及连续取消超过 handle 容量后的复用和 harness/provider/DLL 释放，
不需要真实 API key。
