# 生产 Host Provider observer：#46 后续阶段

## 结论

生产 Host 已在首个 tool-result 路径之上接通三个显式批准的 **Provider observer**：before-request、before-headers、after-response。实际 binary + C DLL + HTTP 全套 **38/38**，包括双 Agent 身份隔离、两轮工具/Provider 往返及取消/成功/HTTP 400 的真实迟到回调结算。

**#46 整体仍不标完成。** 功能路径已通，但扩大的 Agent 全套回归被一个未改的 ToolProgram 大结果嵌套配额测试阻挡；它在默认并行、两线程及隔离运行均撞到三秒 poll deadline。SDK 独立严格 Clippy 仍有 15 项诊断。本文不把这些失败隐藏成“全部全绿”，也不把未执行的 before-source 验证当作已完成。

首阶段 `docs/host-native-hooks.md`/JSON 已冻结为历史验收，未改写。新增阶段输入冻结在 `/tmp/rpi-host-integration-checks/host-provider-stage-before/`：core 1153 文件、extension 823 文件、两树 HEAD/branch/index 及 binary patches。仅在两个既有 worktree 工作；未提交、发布、部署、安装、注册服务或增加 workspace crate/锁定包，不改旧内存报告、不重跑 benchmark。

## 最小兼容边界，不重写 loop/SDK

原 `ProviderHooks` 同步方法保持。`rpi-ai::provider` 新增对象安全的 `ProviderHookFuture` 与有默认实现的 `before_request_async`/`after_response_async`：默认适配仍调用原同步实现，因而现有 SDK/独立 CLI 插件不要求迁移，插件 ABI 不变。

只有既有三处接线改为 await：

- Harness 构造 Provider request 的既有 before-request 点；patch 仍应用于本次 live options，history 仍借用，无 transform patch 不复制。
- Agent loop 收到终止事件后、发出原 MessageEnd 之前。
- Agent loop 事件已耗尽但 `result()` 仍有终止结果的既有 fallback。

没有新事件总线、替代 Agent loop/AgentLane、嵌套 runtime 或 `block_in_place`。同步 `transform_context` 保留兼容，但本 Host **不批准原生 prompt transformer**，不会在该同步点执行 DLL。

## 显式精确批准

以下是在已有完整 profile 的 `nativeHooks.registrations` 中给**单个** DLL 的批准片段；工具/文件/私有 staging 权限仍须同时满足首阶段规则：

```json
{
  "toolResult": 1,
  "providerEvents": [
    {"event": "beforeRequest", "count": 1},
    {"event": "beforeHeaders", "count": 1},
    {"event": "afterResponse", "count": 1}
  ]
}
```

- 省略 `providerEvents` 等于没有批准 Provider observer，旧 tool-result inventory/identity 语义不变。
- event 是闭合枚举，不接受 raw tag、BeforeAgentStart、其他 Agent/UI 事件或任意字段；count 只允许 1–16，重复事件拒绝。
- 转换成 SDK tag 后按数值排序，与完整 `RegistrationInventory` equality 比较，输入批准顺序不能造成 false mismatch；不是只比选中字段。
- ActionBridge/UI、插件工具/命令、prompt transformer、Provider 实现注册和非默认声明仍拒绝。这里的 Provider observer 不等于批准插件替换 Provider。
- 批次注册前仍校验只读保存 authority，批次完成前不发布任何 hook。保存 inventory 包含精确 tag/count，原 artifact hash、工具 schema digest、指令和逐 incarnation staging 流程不变。
- 批准有权执行受信原生代码，不是 DLL 系统调用沙箱；path/hash 也不取代批准。

## 真实 worker、取消排序及上下文

`native_provider_hooks.rs` 只保留 Weak owner、冻结的 `ExtensionProviderContext` 和逐 binding 的异步串行 gate。原生执行使用现有 `ExtensionProviderHooks::from_session(...).with_context(...)`，不复制 SDK 序列化/FFI dispatch 逻辑，也不改 SDK 源码。

实际 blocking worker 才持有 `NativeExtensionCustody::call()`；SDK hook 与其 code keepalive 先 drop，call scope 后 drop，串行 guard 最后释放。worker 所需 planned model/context/options 或 terminal message 使用 owned snapshot；只有显式批准的 native observer 支付该快照成本，普通无 hook Agent 保留借用 history 的路径。

串行 guard 在 enqueue 前异步取得，随后**移入实际 worker**。取消 Provider opening 会丢掉请求 hook 的等待 future，但不能丢掉真实 callback 的 gate/custody。终止的 Aborted observer 必须等待这个真实 worker，不能先于 before-request 完成。所有等待均让出异步执行器，关闭不能只看 JoinHandle/等待者已取消就释放 owner、resident、writer 或 DLL。

Agent/Session 身份沿 SDK 既有 Provider payload 的顶层 `agentId/sessionId`，不是工具 envelope 的 `__rpi`；显式 context 不 fallback 到进程环境。SDK sessionless/null 的既有测试保留，但本阶段没有开放生产 mini/sessionless 后端。

## 验收与限制

### 当前通过

| 命令/范围 | 结果 |
|---|---|
| `cargo test -p rpi-cli --test host_control`，默认测试并行 | **38 passed / 0 failed / 0 ignored** |
| `cargo test -p rpi-cli --lib -- --test-threads=2`，未过滤 | **1538 / 0 / 3** |
| `cargo test -p rpi-harness --lib`，默认测试并行 | **546 / 0 / 4** |
| `cargo test -p rpi-ai -p rpi-extensions --lib -- --test-threads=2` | **AI 63/0/0；SDK 259/0/1** |
| Agent `async_stream` 与新 `provider_hooks_async` 集成 | **6+1 passed** |
| Harness 新 awaited request 单元测试 | **1 passed**，也包含在完整 Harness 库内 |
| 核心 `clippy -p rpi-cli --lib --bins --no-deps -- -D warnings` | **exit 0** |
| scoped rustfmt、两树 diff、四合同、输入/历史报告保全 | 见 JSON |

所有 Cargo 命令均使用已有 CARGO_HOME、worktree target、`TMPDIR=/private/tmp` 和 `--locked --offline -j 2`。AI 数字对应表中实际 feature 集合；不是 workspace/all-targets、所有 Provider features、跨平台或 MSRV 专用门禁。

真实 C DLL 新 observer 带 ABI size/offset 静态断言，和 tool-result、pthread worker、finish、unload 标记共用同一 fixture：

- 双 Agent 各两次 Provider request，共 **4 before-request + 4 before-headers + 4 after-response**；每类事件分别验证 Agent/Session 的精确两两归属，terminal 同时涵盖 ToolUse 和 Stop。保持 SDK 原有 serde 值，不新增 wire 规范化。
- 原 tool-result transform→下一 HTTP 请求、保存 inventory 篡改前置拒绝、冻结后换源、关闭/重启原请求不重放的真实验证仍在。
- 整批失败扩大到缺少 Provider 批准和 Provider count 错误；全部实际 worker cleanup，且没有任何 tool/Provider callback 被发布。
- 新生产测试分别阻塞 before-request/after-response：Close 期间真实认证 IPC 仍响应，owner/resident=1，实际 OS writer lease 不可取得；before-request 的取消等待不能让 Aborted response 越过它，也不能发送 HTTP。
- 正常 SSE 与 HTTP 400 分别产生 Stop/Error，不能被关闭时的取消重写成假 Aborted；after-response 完成之后才允许 lifecycle begin。再阻塞真实 finish，确认 owner/writer 仍保留，最后才释放。
- 每例断言真实标记 `Q→q→H→A→a→begin→worker-exit→finish-entry→finish-return→unload`，并在最后重新取得物理 writer；重复原关闭意图只对账，没有额外 HTTP。
- 原 retained 两种负向场景继续通过。私有负向 fixture 最后强制 kill/reap 不冒充成功 Host cleanup。unload destructor/SDK custody 不夸大成 OS VM-page map 审计。

### 保留的失败与未完成门禁

1. 新 Agent 测试首次把**用户消息**的 MessageEnd 也纳入“终止消息不可早发”的断言，失败 0/1；修正为仅检查 assistant MessageEnd 后，正常 terminal、result-only fallback、cancel 三条路径均验证 worker 等待和 current-thread timer 可运行。生产逻辑未为该误断言改动。
2. 新 binary Context 断言首次混淆 `StopReason::as_str()` 的 `toolUse` 与既有 serde 的 `tooluse`，5/1；改为比较 `json!(StopReason::ToolUse)`，不改 SDK payload，随后完整 38/38。
3. 默认并行 CLI 测试已输出 **1536 passed / 2 failed / 3 ignored**：原 TUI fork 回归和 saved-session-read 十秒处理 deadline。工具命令在 300 秒上限结束，未写出 `.exit`；因此不把父命令 exit 101 当作已收取的结果。确认完整 cargo/cargoc/rustc 树无存活构建后，另日志跑两线程未过滤全库，1538/0/3。
4. Agent `--lib --tests` 在默认和两线程均到 ToolProgram suite **34 passed / 1 failed** 后停止，不宣称后面的 suite 已跑。隔离同一 case 仍失败；backtrace 指向 `tests/tool_program.rs:2124` 中**等待整个 nested 子调用**的三秒 poll，而该子调用包含多轮 18 MiB 结果/共享配额检查。
   - `tool_program.rs` 及其生产模块和 common config 均与新阶段冻结输入逐字相同；该 config 的 `provider_hooks` 为 None。
   - 这些静态事实与调用栈不能替代完整 before-source 执行，也还不能证明纯粹是主机负载/性能问题或排除既有功能问题。
   - 未增加 sleep、放宽 deadline/配额/断言、删除或 skip case；不通过重跑挑绿来宣称 Agent 全套通过。后续应独立诊断此大结果测试/实现，再完成扩大门禁。
5. SDK 独立严格 no-deps Clippy **exit 101、15 项诊断**，与首阶段记录的诊断位置相同；SDK 源码本阶段未改，无新 allow/expect。不称 SDK/workspace lint 全绿。

机器证据保留所有这些日志、SHA、命令结果及未收到 exit 的状态；旧失败日志不覆盖。

## 下一步

先处理 Agent 全套的 nested-result quota/poll 阻挡并保留原语义，再据完整验收决定 #46 收口。#14/#23 的真实 mini、审批/定时器、Host TUI/整体证据缺口不能由本阶段自动补齐。具体 watcher/helper、全代际 dev 和受控 OS 资源清理、默认一按需 N 的薄 Host、配置化整组恢复及真正后台 daemon 仍按原目标继续，原请求和未知工具副作用不重放。
