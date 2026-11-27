# Shared Host 两仓库接入：协议、身份隔离、native 绑定与控制 daemon

> **范围收敛**：本轮先做薄 Host 与配置化恢复/daemon，不再扩展 SDK/DLL 装配。
> A 批仅接通私有启动策略的严格解析、校验和未实现能力前置拒绝；尚不是后台运行或
> 整组恢复。普通 CLI 路径仍未切换，旧功能与四合同保留。
> B1 进一步把唯一成员表/生命周期状态提取到无 catalog 的内存核心，HostService 已复用，
> B2a 增加无 catalog 的私有原生 Harness 接管适配，B2b 接入可信可重试 cleanup 屏障；
> B3a 补已关闭 Harness 的运行绑定 retirement，不等于最终卸载或外部工作结算。
> B3b1 仅补现有加载器 Rust 侧全代际收据观察（含失败候选），正常 CLI prepare/reload 共用；
> 不改 SDK ABI/加载策略，不以记录证明资源清理。B3b2 增加 CLI 插件绑定 retirement owner、
> 串行 reload 门及 TUI task joins；具体关闭仍是 staged 子集，工具/辅助任务/进程须另行
> 结算所有 lease 使用者。B3b3 为 native child/side-question 增加 owned 准备、任务及最终
> join，仍不证明 OS 工具/后台后代退出。Host/binary 通过，完整 CLI 重跑保 4 项 codesign
> 失败；首跑另一次 fork 断言失败未在定向/完整重跑复现，原因尚未确定，原日志保留。
> B3c1 历史仅补 Darwin 原始进程组的 waitable PID/内核 zombie 状态/真实退出与 guard custody
> 基础（7 项实际进程测试、Host 275、重建 binary 31 通过）；要求独占 wait，不覆盖逃逸、
> 外来 reaper 或父死，尚未接现有 OS 工具/全代际 env/dev worker，不是完整资源关闭。
> B3c2 新增逐 dev 实例的 UUID staging 与原子独占创建，碰撞不采用既有目录；dev 8、
> Host 279、重建 binary 31 通过。仅修同进程同包/PID 别名与普通 PID 复用风险，不是
> watcher/延迟 helper/Cargo 后台后代托管；rebuild 保持同步。SDK/完整 CLI 本批未重跑，
> 上述 CLI 失败与未确定 fork 原因仍保留为 B3b3 历史。
> 默认 CLI Host、完整外部资源清理与后台恢复仍未装配。
> 说明及当前结果见 [Host README](../../crates/rpi-cli/src/host/README.md)。
> 下文阶段结果与 SHA 为既有快照，不得当作收敛阶段当前产物或新能力证明。

## 当前结论

这是完整接入计划中的阶段性实现，**不是完整生产 Host 执行服务或管理扩展**。
本阶段已落地以下可独立验证的边界：

1. 核心与 extcore 的同版控制协议及有界 Unix 客户端。
2. Provider hook 的显式逐 Agent/Session 上下文，保留旧单 Session CLI 语义。
3. 核心 `HostService` 普通后端 owner 注册表与受监督生命周期。
4. native-only 普通 Agent 显式配置绑定，经过真实 HTTP Provider/内置工具的隔离测试。
5. 有界认证 IPC、前台 daemon 与 `rpi host serve/status/list/inspect`（另含 Agent 级
   receipt 查询与文本 submit），通过真实二进制
   及 extension 仓库 extcore 客户端的跨仓库验收。
6. request-linked admission 的存储前置：显式、writer 队列内且取消安全的原生 history
   物理同步屏障；它只证明同步时的历史 identity，不是输入 ACK 或 Request receipt。
7. 在普通 Agent 后端接入 request-linked 原生 admission：精确输入在 Provider/工具前写入
   原生日志并完成同步，返回不等于执行完成的 `HostAdmissionReceipt`；同 ID 重试不重复执行。
8. daemon 启动 profile：显式私有 JSON 创建/加载普通 Agent，真实二进制 + 本地 HTTP
   Provider + IPC submit/receipt 端到端执行并在重启后复用同一注册与 Session。
9. 原生 `AdministrativeCoordinator`：受信 grant/immutable profiles、有界 owned 请求、
   journal/fresh 守卫、native create/restore/cancel/close 与分阶段 Stop；已接 daemon
   私有配置与 native handle，部分完整意图变更已接 IPC。
10. 行政只读 IPC：固定 authority 下的允许 profile 分页发现、完整历史意图收据查询；
    不准备新意图、不加载 Agent、不解析新版本或转授 fresh。
11. 完整意图 profile create/restore/guarded close IPC：唯一 fresh 执行、owned 等待不随
    caller 丢失取消；真实 binary+HTTP 动态执行、断连收据与旧版本重复不加载。
12. 受控行政 CLI：Profiles 分页与完整原请求文件的 receipt/create/restore/close，严格
    私有有界输入、先于连接的操作/schema匹配、保留ID/epoch/nonce、保守退出码与stdout失败对账。

13. 普通文本submit/receipt CLI：严格原文件校验、durable admission早于完成、原输入对账不重放。
14. 稳定输入取消Native底座及独立IPC：精确submitted_request_id、固定grant/fresh/owned job，
    timeout/丢reply后完整原意图对账、后续输入隔离及重启不改JSONL。
15. 稳定取消提交CLI：完整原文件与target/result匹配，true/false仅历史信号，丢stdout返回3，
    真实CLI+门控HTTP/后续隔离/重启原文件对账与18类坏文件/目标无连接。

16. 独立Stop IPC：当前owner fresh原生结算/发布/关存储后才通知停接入，有界reply后退出；
    旧历史不能误停新daemon，丢waiter/迟到callback/失败保留与binary+HTTP退出均已验证。

尚未完成：完整生产资源/扩展/权限解析、
真正的 Session-free mini、事件 replay、审批授权、计时器、
Host 执行 CLI/TUI driver、`packages/rpi-host` 管理扩展，以及管理扩展 DLL 参与的贯通
验收（真实二进制 + HTTP Provider 的 native/IPC 闭环已见第 8 节）。现有 CLI 默认路径未
切换，没有部署、安装、发布或合并。

## 隔离工作树

| 仓库 | 工作树 | 分支 | 基线 |
| --- | --- | --- | --- |
| core | `/Users/bigfish/work/rpi-projects/pi-rust-wt-host` | `feat/shared-host-integration` | `699b73d036138e0fe10bbf9b14e29af4119d6fad` |
| extension | `/Users/bigfish/work/rpi-projects/rpi-extension-wt-host` | `feat/shared-host-management` | `607c00d3915d16cf26129979766bcd65ec58b059` |

修改只针对上述独立 worktree；未操作原 checkout 的 index、stash、分支或服务。
复核时原 extension 仍为干净的 `main@607c00d`。原 core 当前观察为干净的
`main@3f643136a5058fed2863107dc945247f97952761`，与历史快照的 HEAD/75 个本地路径
状态不同，属于本轮操作之外的状态更新；没有按旧快照执行恢复、reset 或覆盖。

## 1. 控制合同与客户端

核心位置：`crates/rpi-cli/src/host/{protocol,client,client_tests}.rs`，golden fixture 为
`fixtures/control-v1.json`。extension 对应 `packages/rpi-extcore/src/shared_host/`，
四个合同/客户端/测试/fixture 文件逐字一致；不引入跨仓库 path dependency。
extcore 不链接 `rpi-cli`、Harness 或插件 ABI。

- `rpi-shared-host` v1.0，独立 Host/epoch/Agent/request/call/ticket 身份，拒绝 nil UUID。
- major 不兼容即拒绝；未来 minor/未知 capability 不授予权限。
- 页最多 128 条、帧最多 1 MiB，协商 in-flight 最多 64；当前每客户端只串行一个交换。
  **没有服务端时不能宣称服务端连接/并发/事件预算已落实。**
- 显式 endpoint、Host ID 和 token；检查绝对路径、私有且同 UID 的目录/socket、无叶子
  符号链接、实际 peer UID。父路径按受信本机文件系统处理，不隔离恶意同 UID 进程。
- 握手/每次交换各用整体 deadline，帧长先校验后分配；中途取消使该连接失效，避免
  残帧或迟到回复污染下一次调用。没有后台 reader、隐式进程启动或自动重试。
- 通讯关联 call ID 与 epoch。mutation 尝试发送后的失败是 `OutcomeUnknown`，带原
  request ID；不是拒绝、回滚或可安全重试的证明。应显式查 receipt，**但 receipt 服务
  仍未实现**。诊断不含 token、原始 peer 帧、路径或原始 OS 错误。
- detach 不 cancel/close Agent；旧 Host/缺能力明确 Unsupported，不回退 ChildAgent。

测试还固定了 serde 的边界：内部标签 unit variant 可忽略额外字段，因此使用
`Command::Status {}` 空 struct variant，使未知字段确实被拒绝。

这些测试用真实 Unix socket 的合成 peer；不把合成 peer 称作生产 Host 服务。

## 2. Provider hook 显式身份

`crates/rpi-extensions/src/provider_hooks.rs` 新增 `ExtensionProviderContext` 和
`ExtensionProviderHooks::with_context`。

显式上下文用于 prompt transform、BeforeAgentStart、BeforeProviderRequest、
BeforeProviderHeaders、AfterProviderResponse。`session_id: None` 是真实无 Session
身份，不回退环境；未绑定上下文的旧 CLI 仍按 turn 读取其动态环境身份。

每个 Agent 必须有自己的 hook 实例与 prompt cache，即使共享同一不可变 DLL registry。
身份字段只是 observer metadata，不是 authority 或凭据容器。此处没有宣称全局
`RPI_SESSION_ID`、proxy、Provider credentials 的所有其他使用点均已消除。

验证分两层：

- 单元：8 个并发绑定共享 registry，包含普通和无 Session 身份；检查 transform 与四种
  observer 事件，同 timestamp 不跨 Agent 去重。环境非空的回退测试在独立测试进程设置
  环境，避免测试线程间全局 env mutation。
- 真 DLL：重建 `examples/plugin-stub`，显式提供 `RPI_PLUGIN_STUB_PATH`；8 个并发 hook
  使用同一加载会话/DLL，由 DLL 将收到的身份回写 prompt，再精确断言逐 Agent/Session
  归属和缓存命中。缺失产物/字段不是 skip，而是失败。

无 Session hook 绑定测试不等于已经实现了 mini Agent 后端。

## 3. 普通后端 owner coordinator

`crates/rpi-cli/src/host/service.rs` 提供 `HostService`，仍是显式 Rust API，不是 daemon。

- owner 安装到服务注册表后才返回弱 `OrdinaryClient`。调用方丢弃客户端、startup/close/
  shutdown 等待 future 不移除 owner，不取消监督中的事务。
- 全局同步 map 锁不跨 backend await；读 reservation 允许不同 Agent 并发加载/关闭，
  shutdown 独占 fence 等待在途事务。实际两个不同 cwd 的工具在 current-thread runtime
  经过 barrier，验证不是排队冒充并行。
- 重复恢复不接管 owner；安全 rollback 后移除临时 entry，rollback 不确定则保留
  `StartFailed` fence。没有依据错误去伪造 Idle/Closed。
- close 成功才移除 owner；失败是 `CloseFailed`。迟到 callback 未结算时保留 owner、
  writer、容量。Closing 提交失败也先取消并结算执行，writer 仍保留。
- 原生清理成功但最后 Closed 提交失败时，writer 已真实释放，owner/catalog reservation
  仍保留；重试只完成元数据提交。不错误声称此时 writer 仍存活。
- native persistence uncertain 永远不正常卸载；重复 close/shutdown 都保留 fault 与
  resident slot。测试最后的强制 owner 销毁仅为故障夹具回收，不是生产修复 API。
- shutdown 先拒绝新加载、并发结算 owner；确认 registry 与 catalog resident 均归零才
  关闭 catalog。失败保留 singleton 锁，允许显式重试，不提供强杀/假成功。

服务现由 `HostDaemon` 长期持有；Drop 仍只是 best-effort cleanup。调用方不能
绕过 service 关闭另一个 catalog clone。普通 RunTicket/Running 元数据依旧不是 durable ACK。

## 4. Native-only 普通 Agent 绑定（后续增量）

`host/bindings.rs` 的 `NativeBindingProfile` 接收显式 `RuntimeConfig`、闭合 map 值源、
独立工具环境、绝对 shell 路径与可信预授权工具集合；配置结构不派生 Debug/Serialize。
`OrdinaryBinding` 把同一个环境交给 Session、工具和技能执行入口；
`HostService::restore_bound` 保留已有受监督 owner 生命周期，旧 Rust binder API 兼容保留。
默认 CLI 启动路径未改，未使用 ChildAgent 或逐 Agent RPC 进程。

- 不读取用户 CLI settings/ambient provider env，不写入进程 cwd、proxy、Session ID。
  ProviderRegistry/Runtime 生成真实 Provider；验证解析结果准确匹配保存的 provider/model，
  禁止模糊匹配导致跨模型恢复。API key 不自动传给 shell。`bind(&self, spec)` 的闭合
  Provider/header/credential 解析与 AgentSpec 静态校验早于新建 Session/注册 Agent；
  可重复借用同一可信 profile，每次独立创建环境/cleanup 所有权，不缓存跨 Agent 环境。
  解析不等于验证远端可达性；immutable allowlist/coordinator 已有显式 native 装配，
  daemon 私有配置已有统一冻结和固定 authority，五项完整意图生命周期变更已接 IPC。
- 明确 `DenyAll`/`Preapproved` 工具策略，真实装配 read/bash/edit/write/docs；工具调用前
  再校验 grant/cancel。预授权来自可信 Rust Host 配置，不是模型提供的审批证明，亦不构成
  参数级权限、交互审批、cwd 文件系统沙箱或多租户安全隔离。
- 阶段45支持显式冻结 instruction 及准确工具 inventory；其它 resource profile、未实现工具、
  未支持的保存资源绑定/版本和资源清单仍明确拒绝，不能静默换成原生资源。
  secret command 与无法解析的必需 headers 拒绝，不启动凭据 shell command。具体配置错误
  不进入公开诊断。精确资源/身份/cwd/模型预检失败不获取 writer、不修复 torn JSONL。
- `OsExecutionEnv::with_environment` 使 shell 基线和文件 HOME 路径不依赖进程环境；
  保留 legacy CLI、inherit=false、shell/per-call overlay 及私有通道过滤语义。闭合基线
  缺 HOME 时保持字面 tilde，不把 `~/file` 意外解释为 `/file`。

10 项 native 测试全部使用真实配置解析，包含：拒绝前置与 lease 可重获、错误 proxy、
跨 Agent 复用拒绝、环境 cwd 不匹配、精确模型/缺 key、必需 headers/secret command；
执行测试使用本地 HTTP SSE fixture，不使用 FauxProvider。两个不同 cwd 的 Agent 在
current-thread runtime 经双向 shell barrier 并行，验证独立 Bearer key、HTTP proxy 与
NO_PROXY 直连、HOME/工具变量及无 Provider key 泄漏。另验证真实 write→edit→read
HTTP 工具往返，以及运行中原生 shell 被取消并结算后才释放 owner/capacity。

这些测试不经过 rpi CLI 或管理扩展 DLL，**不是完整二进制贯通验收**。
扩展资源/显式 provider hooks 尚未由该 native binder 组合装配；mini、原生 durable receipts、
完整审批与 timer/执行 CLI/TUI/管理扩展仍待后续实现。

## 5. Control daemon 与 CLI（查询/submit/显式行政 IPC）

已实现 `server.rs`/`daemon.rs`/`cli.rs`，详细能力、私有配置示例、期限/字节预算、状态投影与
验收边界见 [daemon 说明](shared-host-control-daemon.md)。基础能力为 status/list/inspect/receipts/submit；
显式行政策略额外启用 profiles/administrative_receipts 只读查询。
submit 向已加载 owner 投递文本 prompt 并返回 durable admission receipt；另有固定策略下的
administrative_create/restore/close/cancel 四项独立执行能力与对应完整请求CLI；另有独立Stop IPC及完整Stop提交CLI。
查询、metadata、ticket 与 submit 回执都不冒充
执行完成或 exactly-once 工具副作用。

真实二进制 5 项与 extcore→真实 daemon 1 项均显式通过、0 ignored；原生 owner 的工具保活、
迟到结算与失败 shutdown 重试也已覆盖。测试创建的 daemon 均已退出，没有部署服务。
这些仍不是管理扩展 DLL + HTTP Provider 执行贯通，不能把前几阶段的独立测试拼成完整 E2E。

## 6. 原生 history 同步屏障（admission 的前置，不是 receipt）

新增 `FileSystem::sync_file_and_parent`、`SessionStorage::sync_history` 和 Session facade
转发。JSONL 原有 append 已调用 `sync_file`；本次不改普通 append、CLI 或旧文件系统的
兼容语义，而是增加一个不能由兼容 no-op 冒充成功的显式边界。

- 严格文件系统接口默认 `NotSupported`。Unix OS 后端要求普通文件，并依次同步文件全部
  元数据/内容和直接父目录；任一步失败都返回失败。Windows 等非 Unix 后端明确不支持，
  不把既有目录同步 no-op 提升为持久化证明。没有 Linux/Windows 实机验收。
- JSONL 屏障与 append、reducer commit 和完整 history replacement 共用 `tail`，由
  detached owner 持有队列位置，完成同步后采样 `(revision, generation)`。caller 丢弃等待
  不取消已准入同步、不让后续写入越过；尚未获得 tail 时取消则不启动同步。
- 必须有有效 writer lease，未决 append、已退役 binding 或同步期间失去 lease 均拒绝。
  同步失败既不是 ACK，也不表示已落盘输入被回滚；允许显式重试这个无历史变更的屏障。
  内存存储和没有显式实现的后端保持拒绝。
- Session 转发在等待 mutation gate 前固定 binding；并发 `set_storage`/`rebind_to` 不改变
  屏障目标。在原生 journal 独占 gate 内调用可重入，不升级锁、不自锁。
- 范围只有 history 文件及其直接父目录，不涵盖 progress companion、Host SQLite、创建
  目录的祖先链或不遵守 lease 的外部写入。调用方必须维持受信路径与 ownership；这不是
  对恶意同 UID 文件替换的安全边界，也不承诺硬件一定遵守 fsync。

新增 11 项 Harness 测试与 4 项 tools 测试，覆盖真实磁盘 reopen、文件/目录拒绝、FIFO、
同步故障、前后取消、writer 丢失、未决写入、history replacement、facade 重绑定与原生
journal gate 内调用。屏障本身不新增 request-ID 关联或跨日志 admission 事务；后续第 7 节
已将其接到受监督的 Harness 准入路径。IPC 已开放文本 submit 与 Receipt 查询，
restore/cancel/close/stop 保持关闭。

## 7. Request-linked 原生 admission（新增）

`AgentHarness::admit_host_request` 在 `message_writes` gate 与独占 journal mutation gate 内：

1. 校验 request/host/agent 身份为 1..128 的 ASCII identity，request ID 必须是非 nil UUID；
   精确 messages ≤ 512 KiB、1..64 条。
2. 以 request ID 作为 entry ID 追加 `rpi.host.admission.v1` custom entry（CAS 到当前
   revision），负载含精确 messages、SHA-256 摘要和追加前 `(revision, generation)`。
3. 调用 `Session::sync_history`，要求同步后 generation 不变、revision 恰为 +1、entry seq
   与之一致，才返回 `HostAdmissionReceipt { entry_id, entry_seq, history, digest, ... }`。

语义与边界：

- 幂等：同 request ID + 相同内容返回相同收据（`replayed = true`），不再执行；相同 ID
  不同内容返回 `Conflict`。Host 监督层据此返回 `OrdinaryOutput::AlreadyAdmitted`，
  不会重复调用 Provider/工具。要再次发送相同文本必须使用新 request ID。
- 收据不是执行证明。对账时只看 admission entry 之后**第一个** `operation_started`：
  其 `source_leaf_id` 必须是 admission entry，且 intent 的输入以 admitted messages 为
  后缀（允许 queue next-run 前缀）且摘要匹配，才报告 `Started { run_id, finished }`；
  否则报告 `Admitted`。紧凑/恢复/无关运行不会误报。不自动重放已 admitted 输入。
- 失败：同步失败、丢失 writer lease 或 identity 变化返回 `CommitUncertain`，不产生收据；
  重试相同 ID 必须重新同步，不能复用失败调用的 ACK。无效请求在 catalog 转 Running 前
  拒绝，Agent 保持 Idle。内存/未实现后端保持拒绝。
- 记录不进入模型上下文（即使注册同名 projector 也跳过）；公共 custom-entry API 保留
  `rpi.host.admission.*` 命名空间，不得伪造 admission。`host_admissions()` 枚举上限 64 条，超过显式
  `LimitExceeded`；目标请求查询与接收不受该枚举上限影响。枚举按 custom type 读取，
  当前 Host 普通 Agent 只使用 `main` lane，跨 lane 的 admission 尚未接入。
- `AdmittedRunTicket::wait_admitted()` 与 `wait()` 分离；前者是持久输入，后者是执行结算。
  `HostService::admission/admissions` 不追加日志，但返回已存在 receipt 前会严格同步。
  IPC 新增 Agent 级 `receipt` 查询与
  `submit`：submit 仅接受 ≤256 KiB 非空文本，指向 Live owner，回复 `wait_admitted` 的
  receipt（同 request ID 幂等且不重复执行，不设 revision 门槛）。未知 request
  `found=false`，未加载 Agent `NotFound`，同步不确定 `PersistenceUncertain`。
  create/restore/cancel/close/stop 旧占位命令仍返回 Unsupported；动态 create/restore/close
  使用独立完整意图命令（见后文），不代表已有 CLI 参数。
  token 持有者可对已加载 Agent 触发预授权工具执行，token 不是逐 Agent scope。

新增 8 项 Harness 原生测试（真实 JSONL + fsync barrier）与 2 项 Host 监督层测试：并发
admission 序列、幂等/冲突、同步失败后重试、取消/队列前缀、重开对账、上下文中立、拒绝不
写、真实 FauxProvider + gated 工具下“收据先于结算”、同 ID 不重复执行。

## 8. Startup Agent profile 与首个真实二进制执行 E2E（新增）

`crates/rpi-cli/src/host/profile.rs` 定义 daemon 启动用的私有 profile：Agent 身份、
workspace、Session id/path、模型引用、闭合 `runtime.providers`、`providerValues`、
完整工具环境、绝对 shell、`approvedTools`、thinking 与可选 timeout/input budget。

- `HostDaemon::open_with_agents` 在 catalog 打开后先验证各 Agent 显式绑定/授权：已注册且
  spec 相同则复用，否则仅当 Session 文件不存在时创建 JSONL 并注册；随后 `restore_bound`
  严格恢复（id/cwd/历史摘要、
  writer lease），最后才 bind 控制 listener。任一 Agent 失败先结算已启动 owner 再返回，
  不发布 readiness。
- 没有 CLI/IPC 运行时 create/load；没有 ambient settings、凭据、proxy、扩展或 Session
  自动发现。Session 已存在时不覆盖；profile 与注册记录不一致直接拒绝。
- submit 仍走原生 admission：重启后 receipt 可查询，相同 request ID 返回同一收据且不
  再执行。

验收（`actual_binary_loads_startup_profile_and_executes_ipc_submit_end_to_end`）使用
Cargo 构建的真实 rpi 二进制、测试进程内本地 HTTP SSE Provider、0600 profile/config：
启动即加载并 `inspect` 为 idle；HostClient `submit` 得到 `entrySeq=1` 的 durable receipt；
轮询 `receipt` 到 `execution.finished=true`；Session 含 Provider 回复且不含 Provider key；
SIGTERM 后按同一 profile 重启，注册仍为 1、receipt 仍可读、重复 submit 不再执行。
这是首个真实二进制 + Provider + IPC 执行闭环，但**仍不含管理扩展 DLL**。

### Admission 故障重试加固（已验收）

- supervisor 重复提交调用 `resync_host_admission`：在原生 gate 内比较完整请求，再严格
  同步；只重证已存在 admission，不追加缺失请求、不执行模型/工具。同步仍失败时返回
  `AdmissionUncertain` / IPC `PersistenceUncertain`，绝不把可见 entry 直接当作 ACK。
- `host_admission/host_admissions` 在固定 Session binding 的独占 mutation gate 内读取，
  严格同步后校验 entry 的 generation/revision 覆盖，再返回 receipt。列表一个 barrier
  覆盖整页；不存在的请求不做无意义同步。查询不追加日志或推进执行，但有物理同步 IO。
- 新负载版本为 2，digest 是标准 `SHA256(serialized messages)`；版本 1 的双 SHA256
  保留原收据 fingerprint 和执行关联，重开/重试不迁移或重写旧历史。custom type 名字
  `rpi.host.admission.v1` 不变；已知空/非空向量独立校验摘要。
- request→run 仍是保守的原生结构关联：只认第一个 start，旧 leaf 被后续导航重用也不
  追认。Host 普通 Agent 当前仅使用 main lane；没有将其宣传为工具副作用 exactly-once。
- native admission 测试现为 **12 项**；Host 新增 **5 项**故障注入，覆盖目录同步失败后
  可见 entry、重复提交/查询均拒绝、同步恢复后只 ACK 不执行、运行中重试不取消/重跑、
  submit 客户端超时后继续结算，以及 receipt 服务端 deadline 丢弃等待后 barrier 仍完成。
- 真实二进制新增 **SIGKILL-after-admission/start**：HTTP 请求已到达但不回复，杀进程后
  显式清理测试私有目录内的 stale socket，按相同 profile 重启，receipt/runId 不变，重复
  submit 不发第二个 Provider 请求。daemon 仍不自动删除 stale socket。

五项故障测试与真实 crash 用例另重复三轮通过。这关闭了此前已知的“可见即 durable”
快捷路径缺陷，但不等于证明所有硬件/外部写入/跨 lane 恢复或工具 exactly-once；**完整
Host 目标仍在进行，管理 DLL/mini/审批/生命周期等不能由这些测试替代**。

### Startup Session exclusive-create（已验收）

- `FileSystem::write_new_file` 是独立能力，默认 `NotSupported`，不回退到 exists+write。
  OS 使用 `create_new`，不创建父目录、不截断或跟随既有最终路径；Unix 新文件为 0600。
  文件创建/写入的 blocking 所有者不会因 async 等待方丢弃而中止；取消/IO 错误可留下
  部分新文件，不做可能误删其他文件的回滚 unlink。该接口本身不声称物理持久化。
- `JsonlSessionStorage::create_new` 要求真实 writer lease，整个初始化由独立 owner 保活。
  先独占创建**空主文件**，再独占创建并严格同步新 progress，最后向自有空主文件追加
  header 并严格同步主文件/父目录。不能先写可读 header：否则创建失败时，已有的同
  Session ID 孤儿 checkpoint 可能在下次 startup 被误接纳。
- 失败保留空占位/部分新文件供显式检查；没有两文件原子事务、自动清理或孤儿收养。
  空占位不能作为可恢复 Session。Header 成功发布后，已有有效 pair 可走显式严格恢复；
  无法以重复 create 替换它。旧的通用 `create` 行为不变，daemon 新建已切至新接口。
- tools **5 项**覆盖默认拒绝、16 并发唯一完整赢家、既有文件/目录/缺失父目录、预取消、
  symlink/悬空 symlink/hardlink/FIFO 不跟随不阻塞；JSONL **10 项**覆盖真实同步重开、
  12 并发唯一赢家、既有 entry/progress 保留、注入竞争路径、孤儿（包括合法且同 ID）、
  capability 缺失、companion sync/header publication/最终 sync 失败、放弃等待仍持 writer。
- Host 新增创建失败及再次 startup 严格拒绝的测试，断言无 ready/socket、孤儿字节与
  空占位不变、writer/catalog 已释放。上述 tools/JSONL/Host 定向测试另重复三轮通过；
  真实二进制执行与 SIGKILL 重启套件仍全部通过。

这个前置安全窗口已关闭。动态 lifecycle 的 native 前置又补齐以下边界；IPC 仍关闭。

### Guarded cancel/close 原生边界（已验收，非 lifecycle durable receipt）

- `OrdinaryClient::cancel_guarded` 在同一 control critical section 比较 owner 观察到的
  epoch/revision 和精确 volatile ticket，再发协作取消；取消信号成功不代表实际结算。
  旧 revision、旧 ticket 不能取消已开始的后续运行。
- `OrdinaryAgent::close_guarded` 与无条件 owner `close`/shutdown 分离。管理关闭必须先
  校验 guard 并提交 Closing CAS，随后才能 retire/cancel；即使内存中 guard 匹配，若
  catalog 已被其他路径推进，也不能取消当前运行。已 Closing/Closed 的重试复核实际
  catalog，旧 closed handle 不能为后来重新加载的 owner 返回关闭成功。
- `BackendError::GuardRejected` 只表示明确的前置拒绝，和接收后结算/持久化错误分开。
  无条件 shutdown 仍在 Closing 提交失败后尝试取消/结算，不因管理入口的前置条件回退
  而失去清理能力。接受的 guarded close 在等待方取消后继续保有 writer/槽直到迟到
  callback 与实际 drain 完成。
- 新增 **6 项**测试并另重复三轮：错 epoch/revision、精确及旧 ticket/后续活跃 run、
  外部 catalog revision 竞争、Closing DB 失败、旧 handle/新 owner、丢弃等待与迟到
  callback。竞争测试仅为清理 fixture 撤销注入的外部写入，**不是提供运行时 SQL 恢复**。

这些 guarded API 本身仍不是行政收据；现已由固定 native coordinator/journal 装配，五项执行 IPC 已接。

### 独立行政 journal（已验收，未开放执行IPC）

`lifecycle.rs` 与 `lifecycle_tests.rs` 在现有 catalog worker 中实现
`prepare_lifecycle/finish_lifecycle/lifecycle_receipt`，不另建 Agent 进程或 Session 日志：

- schema 1 → 2、Agent 重启分类、所有 journal 校验及 epoch 递增共用一个事务；任何损坏
  都回滚，不能先改变 Agent 状态/epoch 再报告 journal 错误。每项 ≤4096 bytes，全 catalog
  ≤1024 项，不淘汰身份；满额拒绝新请求，但保留精确重复与查询。
- scope+request ID 和完整闭合参数（含 epoch/revision/精确 ticket）逐字段比较；同请求
  一个 fresh 赢家，冲突不覆盖。新意图检查 guard/存在性，同 Agent pending 和全局 Stop
  互斥。scope 是去重名字空间，**不是 scoped authorization grant**。
- 只有 commit + FULL WAL checkpoint 成功才能返回 fresh；已提交的可见行不跳过同步。
  checkpoint 忙或部分完成返回不确定；查询/精确重复也重证 checkpoint，即使后来同步成功
  也不能重新授予 fresh。没有用空读事务/COMMIT 冒充同步屏障。
- Prepared 重启只成为 Interrupted，不取回执行资格、不重放；终态 Succeeded/Rejected/
  Uncertain 不可覆盖。Rejected 专指确定无动作的拒绝，其他结算/持久化失败须标不确定。
  CancelSignalled 仅证明取消信号，NativeSettled 不证明进程/listener 已退出。
- **意图和动作不是原子事务**；可信 coordinator 必须拥有不随客户端取消的
  prepare→action→finish，且只在 fresh 时执行。finish 不能开放为客户端可写接口。
  现已接入固定权限的监督执行层，但仍不能承诺 pending 一定执行；丢弃回复不转移执行资格。
- 新增 **15 项测试**（journal 14、catalog 丢回复 1）：16 并发唯一 fresh、完整参数冲突、
  scope、guard、重启中断/终态保存、迁移、损坏事务回滚、满额/超额、intent/result 写入失败。
  实际 reader 阻塞 checkpoint 分别验证 Prepared 与终态可见但无 ACK，解除后只同步而不重授。
  另有真正 SIGKILL 的 catalog 子进程，确认提交后不依赖优雅退出仍保留意图且不执行；
  没有默认跳过的 helper，不把该库级子进程测试称作 daemon/DLL E2E。
- 上述子进程的重复并发验收曾出现 shutdown ACK 后重开 `AlreadyRunning`：CLOEXEC
  只能限制 exec 后的描述符，fork 窗口的共享引用会延迟 last-close flock 释放。
  Registry 现在先关闭数据库/释放配额，再显式 unlock/关闭实例锁，成功后才 ACK；Drop
  同样解锁，unlock 错误不能被确认成功。新增 **1 项确定性回归**用 dup 复现共享 open
  file description，验证 shutdown/Drop、原 inode 保留、旧描述符不解锁新 owner。
  没有加入 sleep/retry；这 **16 项**本轮新增测试额外重复三轮全通过。原失败日志保留在
  `/tmp/rpi-host-integration-checks/lifecycle-journal-fork-lock-race.log`。

### Service guarded 装配（已验收，仍无行政 journal dispatch/IPC）

- `HostService::cancel_ordinary_guarded/close_ordinary_guarded` 已连接原生 guarded API。
  owned 任务持有 service lifecycle 读预占；shutdown 写屏障等待真正结算，不随 caller abort
  释放 owner/writer/驻留槽。不同 Agent 不串行化，不在 registry mutex 内 await。
- GuardRejected 和原生 close gate 的前置 Busy 恢复捕获的**原 phase**，不是一律 Live；
  CloseFailed 仍失败保留。泛化 Catalog/Harness/任务错误不能推断无动作，进入 CloseFailed。
  StartFailed 没有 native owner，guarded close 不发明 guard 校验或清理证明，也不移除 fence。
- stopping 快速拒绝新行政操作，并在 lifecycle 预占后复核；原有无条件 close/shutdown
  仍可在失败后重试。cancel 仍只是 owner-observed revision 与精确 ticket 检查，不冒充
  authoritative catalog CAS 或结算证明。
- **5 项新增 Harness/JSONL 测试**及额外三轮全部通过：拒绝后仍 Live、旧 ticket/后续 run、
  CloseFailed 原 phase 恢复、Closing DB 失败无取消但 shutdown 仍取消/重试、StartFailed fence、
  accepted close 丢弃 waiter、迟到 callback 及 shutdown fence。这不是 journal 动作装配。

### Immutable profile 集合与版本化 journal（原生底座，未连接执行）

- `TrustedProfileSet::freeze` 消费最多 1024 个可信 HostStartupAgent，先预检再发布引用；
  私有输入不派生 Debug/Serialize，无插入/替换 API。一项固定一个 AgentId/完整 spec，
  不是传入任意身份/环境/工具的模板。重复 ID、Session id、canonical parent 路径及
  Unix dev/inode 硬链接别名拒绝；最终 symlink/目录或不可用 parent 也拒绝。
- `ProfileRef { agent_id, version }` 是每次冻结新生成的随机身份，不是秘密的 hash，也
  不代表授权。准确命中版本才可 `prepare` 新 binding；不缓存 Agent 的环境/cleanup。
  freeze/prepare 不创建 Session、不持 writer、不注册 Agent；现存 Session 的内容与资源
  仍须 readonly facts 校验。检查只针对可见路径，不冻结外部文件系统或声称 sandbox。
- 新 `CreateProfile/RestoreProfile` 意图完整包含 profile 引用，版本变更即参数冲突，
  仍按 Agent 排除 pending 竞争并校验 revision/结果。v2 表布局不变；旧 `Create/Restore`
  按原格式保留，重启 Prepared 仍 Interrupted，终态字节不改写，不补发配置版本或重放。
  不保证旧 binary 能读取新 op；原生 coordinator 已拒绝新的 unbound legacy 动作。
- 新增 **7 项集合 + 3 项 journal** 测试及各额外三轮通过：有界/空集合、配置预检失败无
  Session 副作用、引用纯身份/错误版本拒绝、路径别名保留、完整去重/guards、字面 v2
  JSON 与新动作混存重启。journal 仍不验证 profile 授权，亦不执行或加载 Agent。
- 版本在重启重新冻结；原生 coordinator 先对账已存在的完整历史请求，再为 fresh
  请求解析当前 profile，避免因版本更新丢失旧回执或将旧意图重新执行。

### Service 行政 reservation 与分阶段 Stop（原生底座，未接生产 dispatch）

- `with_administration` 在独立监督任务中调用借用 reservation callback，读预占覆盖整个
  journal prepare → native 动作 → finish。返回值必须 `'static`，不能把借用 token 带出任务；
  caller abort 不取消 callback。reserved restore/cancel/close 直接调用内部动作，不重复读
  公平 RwLock，避免排队 shutdown writer 导致递归读死锁；registry mutex 仍不跨 await。
- stopping 在预占前/后检查；已接纳的 reservation 是现存事务，随后 stopping 信号不会
  中途撤销它，shutdown 必须等 callback 完成。不要在 callback 内改调再次预占的 public API。
- 行政 Stop 先独占预占，不因此改变 stopping 或取消 native；fresh job 才应 `drain`。
  成功返回 typed `DrainedService`，保持独占 fence 和 open catalog，允许持久化
  NativeSettled 后再 `close_storage`。结果发布失败时可留下 stopping/open catalog；无条件
  shutdown 仍立即拒绝新工作、允许结算失败重试，保持原语义。
- token 不校验授权/profile/fresh 资格，drained 也不是 journal 已同步的证明。原生无条件
  shutdown 不要求行政 receipt；生产 coordinator 仍需显式排序、监督结果发布与故障分类，
  不能把任意错误标为 Rejected，不能先 `service.shutdown()` 关闭 catalog 再写结果。
- 新增 **4 项测试**及额外三轮通过：writer 排队后的 reserved restore/cancel/close 和
  journal finish，Stop caller abort/迟到 callback/native drain 后发布再关存储，stale/nonfresh
  无取消/无 stopping，结果写入失败保留 Prepared/open catalog 且重启仅 Interrupted。
  它们直接使用可信 native callback，不证明 profile 授权或生产行政 dispatch 已接通。

### 原生有界行政 facade（已验收；五项完整意图 IPC，legacy ticket Cancel/裸Stop占位仍关闭）

- `AdministrativeGrant` 是可信 Rust owner 构造的 scope/Agent/动作权限，默认拒绝，
  不可反序列化，不从引用、模型参数或 scope ID 推导授权。授权早于 lookup 和副作用。
- 每 service 只 claim 一次 coordinator，drop 不转授旧 epoch custody；owned 表最多
  64 个不同请求，完整参数比较，重复 join，满额拒绝新任务。caller drop 不取消任务，
  mutex 不跨 await。该上限不替代未来 IPC 的连接/waiter 预算。
- 先完整历史对账，再解析当前冻结 profile；reservation 内再次对账，覆盖入口查询为
  None 后旧任务已完成移出表的竞态。仅 fresh 分支可执行/finish；prepare/checkpoint
  无确认不接管，Prepared/Interrupted 也不重放。旧版本回执不依赖新 freeze 版本。
- Create 不复用既有文件，exclusive create 及随后注册/加载失败保守 Uncertain、保留
  部分文件。Restore 在 Starting 前核对完整 spec/revision 与 readonly saved facts；
  startup 同样在注册前预检，孤儿重启拒绝不会创建 Agent 行。实际 strict restore 重验。
- 旧 Cancel 只信号精确 volatile ticket；新增 CancelSubmitted 精确原输入目标，均不等于结算。
  Close 等待迟到 callback。Stop fresh
  后独占 settle，失败仍持 fence 发布 Uncertain；成功则严格发布 NativeSettled 后关闭
  storage。发布失败保留 stopping/open catalog。仅缓存一条已 checkpoint 的成功 Stop
  收据，供 worker 关闭后的精确重复对账；不证明 listener/进程已经退出。
- **12 项**测试（9 项 facade/存储故障、3 项真实 Harness/JSONL）及额外三轮通过：
  grant/lookup 前拒绝、完整去重/旧版本、丢 waiter/背压、精确取消/迟到关闭、部分创建、
  checkpoint 不给 fresh、终态写失败、Stop 结算失败保留 owner 与显式重试。
  这些不是管理 DLL 的联合 E2E，也不宣称工具副作用 exactly-once。

### Daemon 私有行政配置与统一 profile 所有权（未开放 lifecycle 变更 IPC）

- `HostDaemon::open_configured` 可显式接受 `DaemonAdministrationConfig`；CLI 仅在
  `serve` 从 private Host JSON 解析可选 `administration`，query 不读取 profile 文件。
  文件策略只映射可信 native grant；不允许把 peer grant、凭据或工具授权作为 wire 输入。
- 固定 `scope`、`allowed_agents` 与动作 `permissions`，Agent/动作默认空；`max_jobs`
  1..64、默认 8。允许的 Agent 必须在当前统一 profile 集合中；scope 不是凭证。
- 启动 profiles 与动态 profiles 在打开 catalog 前统一冻结（合计 ≤1024），跨集合
  身份/Session/路径/inode 别名及闭合绑定错误都前置拒绝。动态项不创建 Session、不注册、
  不自动加载；保留原 agents 默认启动行为。native handle 只发现允许的 Agent 引用。
- 已加载 owner 的 startup/bind 失败仍经原生结算，保留外来端点；配置中的权限不会
  缩小既有 control token 的全 catalog 查询/submit 权力，也不等于 scoped tool approval。
  NativeSettled不是退出证明；当前owner成功close_storage后另发易失通知，listener有界排空reply后退出。
- 新增 **6 项 daemon + 1 项默认值**测试，另重复三轮；实际 binary 新增 **2 项**覆盖
  动态输入不自动加载/信号重启/查询不读 profile、15 类私有行政配置前置拒绝；binary
  全套三轮各 **10 passed, 0 ignored**。不声称动态 IPC 或管理 DLL 联合 E2E。

### 行政只读 IPC（wire v1.1，不执行 lifecycle）

- 配置 `administration` 才公布 `profiles` / `administrative_receipts`；None 保留原五项能力。
  `Profiles { after, limit }` 按 AgentId 分页（1..128），只投影允许的 `{agent_id, version}`、
  固定 scope 和 next_cursor。无凭据、配置、路径、摘要、自动创建/注册/加载。
- `AdministrativeReceipt { request }` 是完整闭合的原 scope/request_id/epoch/action；外层
  当前 transport epoch 与原意图历史 epoch 分开。server 仅使用 daemon 固定 grant，先授权
  再严格同步/完整参数对账，不解析新 profile 或提供 fresh custody。found false 不是执行入口，
  同 ID 不同参数返回 Conflict；默认拒绝/错 scope/无动作权限返回 Unauthorized。
- 原 action 的 legacy Create/Restore、Cancel/Close/Stop 只是历史意图，不启用对应变更。
  query 的 `Command::request_id()` 仍为 None，丢回复是 QueryFailed，不是 mutation
  OutcomeUnknown；连接、帧和 read/dispatch/write 总期限仍受既有预算约束。
- **3 项真实 daemon IPC + 2 项协议 + 1 项客户端**新增测试覆盖允许列表/分页/无泄漏、
  refreeze/旧 epoch 完整收据、参数冲突/默认拒绝/无执行权限、断线/超时分类。binary 额外
  **1 项**通过两次信号重启查询真实 pending→Interrupted，无 Agent 注册或 Session；原动态
  配置测试也扩展发现查询。四合同含 golden fixture 逐字镜像到 extcore。

### 完整意图行政执行 IPC（wire v1.3）

- 四项独立命令/能力 administrative_create、administrative_restore、administrative_close、
  administrative_cancel，分别只接受 CreateProfile、RestoreProfile、guarded Close、
  CancelSubmitted；不复用旧占位 capability。
  默认 grant 拒绝，作用域/Agent/动作必须匹配固定 native authority，不能反序列化 peer grant。
- 完整原请求与外层 transport epoch 分开；调用既有 coordinator，只有已严格持久 fresh
  能执行。历史完整请求先对账，旧版本/旧 epoch 相同请求返回原结果，不重载或重放。
  返回原 LifecycleReceipt，历史 Loaded 也不证明现在 resident；Uncertain 不是成功。
- mutation request_id 保留，丢连接/回复/期限为 OutcomeUnknown。owned job 独立于连接，
  满预算拒绝新身份；存活 epoch 的 found=false 可能只是 job 尚在 reservation 前，不代表
  将来没有作用，不可换 ID 重放。legacy ticket Cancel/裸Stop占位仍关闭；完整请求 CLI 见下节。
- 新增 3 daemon（真实创建/恢复/关闭/guard/旧版本、默认拒绝/原始帧错操作、500ms deadline
  丢 waiter 后仍占容量及解 fence 完成）、1协议矩阵、1客户端测试。binary 新增2项：
  实际动态生命周期+两轮 HTTP 用户 prompt，以及丢弃真实 reply 后查询/重复不重建 Session。
  HTTP fixture contextWindow=131072，避免 tiny window 的 auto-compaction 假执行，完整
  HTTP reader 单请求≤1MiB，保留旧 header-only helper。两仓库四合同逐字一致。

### 完整原请求文件行政 CLI

新增 `profiles`（cursor/limit 1..128）和 `administrative-receipt/create/restore/close/cancel`，后者
必须 `--request <absolute-private-json>`。完整原 scope/request_id/epoch/action 原样使用，
不生成/替换身份或根据新 epoch/profile 重构，不自动重试。输入仅private owned regular
file/parent、无symlink/multiple-hardlink、4096bytes；schema/动作/flags错误在连接前拒绝。
query不读serve-only profiles。新增 `cli_administration.rs` 复用 Native receipt validate，
返回0只为完整匹配的 Succeeded；Rejected/NotSent=1，用法/坏请求=2；missing、Prepared、
Interrupted、Uncertain、丢reply/查询失败、错响应、mutation丢stdout=3。保留JSON并提示以
完整原文件对账，不换ID重放；历史成功非当前驻留/执行结算/进程退出证明。

该阶段新增5 unit与3 binary（真实CLI+两HTTP动态生命周期、重启old intent不读取坏profile、
断stdout已commit关闭返回3再查成功、13坏输入通过真实socket确认无connect、None/Some
unsupported/default-deny）。原Interrupted测试扩两重启CLI返回3；没有更改四份wire合同。
额外三轮每轮7 CLI unit+16 binary均0ignored。明确不是管理DLL三方E2E。

### 普通 submit / Agent receipt CLI

`submit`和`receipt`均要求完整原私有request JSON（agent_id/request_id/prompt）；不生成/改写
ID/输入、不重试、不自动等待、query不读serve profile。绝对私有owned parent/regular文件、
单hardlink/非symlink、≤1MiB；UTF-8 prompt非空≤256KiB、精确Native消息序列化≤512KiB，
都在连接前验证。query需保留原prompt以核对Native digest，而不是凭ID假定输入相同。

新增 `cli_submission.rs` 绑定Host/Agent/request/entry身份，entrySeq/history、单消息bytes、
SHA256 v2或legacy双SHA256、closed response/execution与run UUID。submit的0只是durable
custody，receipt的0只是精确匹配的准入观察，不是运行/工具成功或恢复重放许可；finished
仅Native finish marker。missing、无法验证/不确定/丢回复/查询失败、mutation丢stdout=3；
明确未发/拒绝1，坏输入/用法2。错原文查询已存在收据也3且无误导JSON。输入冲突保留既有
InvalidRequest分类，不修改wire合同或以新ID规避。

新增4unit/2binary：实际CLI在gated Provider响应前完成准入，运行观察finished=false，
重复、真实丢stdout、异prompt拒绝/错原文查询、missing、完成后重启不重放/字节不变；
14类坏文件两操作均无connect。此前行政生命周期测试的两轮投递/观察也已切成实际CLI。
全套200 Host/18 binary通过；额外三轮每轮11 CLI unit+18 binary，全部0ignored。
该阶段显式二进制SHA256：`b034349b30ad350f3008db92f431e1f34570d497d483acc2e7e19d8748677194`。

### 稳定输入取消 Native 底座

Active保存原submitted_request_id而不是重复observer ticket，guard/精确目标/信号在同一
control gate中。旧精确volatile ticket API保持，新增Native CancelSubmitted及Service
reservation/coordinator分支，复用固定cancel权限、fresh journal资格和CancelSignalled结果。
新目标属于完整去重参数，与旧ticket意图区分且不改旧行；Pending重启只Interrupted。
历史结果不重查active、不向后来输入重发信号。没有从普通receipt推导执行授权。

同owner的显式deferred poll携带输入关联，parked无active则false、不自动resume；运行
完成/成功close清除，restore不从历史复活关联。Snapshot仅易失观察，不是持久ACK。
新增6项覆盖真实Harness/JSONL的guard/duplicate ticket/下个输入隔离、迟到callback持
writer/owner、deferred/restore，journal参数/legacy/重启，held reservation丢waiter仍
owned完成/默认拒绝/无目标false，以及Native daemon+实际HTTP的无响应取消、下次HTTP
门控时历史重试/旧目标不误伤、重启无HTTP/JSONL不变。不是Cancel IPC或管理DLL测试。

### 稳定输入取消 IPC（wire v1.3，CLI见下节）

`AdministrativeCancel {request}` / `administrative_cancel` 仅匹配闭合 `CancelSubmitted`。
完整原scope/request_id/epoch/action，目标是原submitted_request_id而非observer ticket或行政
request_id。固定daemon cancel grant默认拒绝，唯一fresh执行；阶段38当时Some11/None5，当前v1.4为Some12/None5。
丢reply/timeout为原行政ID的OutcomeUnknown，不换ID、自动等待或重放；完整历史先于当前guard。
legacy ticket Cancel仍关闭，独立Stop IPC见后文，parked/restore无关联不resume，历史信号不证明结算。

新增协议闭合/guard测试，并扩展四执行×八意图矩阵、四变更的断线/timeout分类。真实daemon+
HTTP测试验证scope/Agent/epoch/revision拒绝无误伤，500ms waiter超时后job仍占唯一capacity，
held reservation释放后owned完成；第二输入门控时改目标Conflict、历史重试/旧目标false不误伤。
新增真实binary+HTTP丢取消reply测试：原意图查询后首输入Aborted（Provider不回headers），
第二输入正常完成，重启原收据/重复对账、JSONL字节不变。不是管理DLL三方E2E。

### 稳定输入取消提交 CLI

`administrative-cancel --config <private-json> --request <absolute-private-original-intent-json>`
只接受CancelSubmitted，复用私有owned regular file/parent、单hardlink/非symlink、≤4096bytes
完整意图输入。原scope/行政ID/epoch/agent/revision/submitted_request_id不生成或改写，不按
当前活动重建目标，不等待/重试/replay。administrative-receipt按原文件对账，0仅匹配历史
Succeeded；true/false均不是运行/工具结算。1确定拒绝/未发送，2坏输入，3未知/丢reply/stdout。

cli_administration.rs新增Cancel parse/command/Expectation，保证完整Native receipt/action/result
匹配与mutation输出分类；新增1 unit测true/false/目标/guard/result篡改及非成功状态，原矩阵
扩4×8，parser拒ID/epoch/target override。新增2 binary：真实CLI+两HTTP门控fresh取消后
stdout断开返回3，原文件查成功/重复不伤第二输入，重启原文件对账、JSONL不变；18坏输入
真实Unix listener确认无连接。默认拒绝/Unsupported也扩4操作。不是管理DLL三方E2E。

阶段39历史：209 Host/21 binary/19 extcore/1显式artifact全部0ignored；额外三轮每轮209 Host+21 binary。
阶段39 SHA256：`730c9f76f9f8ee22cb4a69074e4bba67da05f765928b5bf052ce67659a93e76f`。
阶段39未改wire1.3/Some11/None5；当前阶段40已完成Stop与wire1.4/Some12/None5，见下节。不宣称完整Host。

## DLL装配推进中（阶段46，尚未开放profile输入）

严格owned loader/cleanup receipt现已接可信Rust binding的具体Host custody；失败装配和
正常关闭都在实际callback及SDK/handle/guard清理完成前保留owner/resident与已取得的writer。
Retained/未知保留StartFailed/CloseFailed，不能凭inactive行或最后Drop当作释放证明；
同一custody只能claim一次，拒绝第二binding不能误关第一Agent。

[冻结artifact](../../crates/rpi-cli/src/host/extension_artifacts.rs) 只读捕获完整bytes、路径、SHA
与顺序，至多8个DLL、单个64 MiB/合计128 MiB，拒绝canonical/hardlink别名。owned阶段
使用显式private owned根目录和每Agent/每次装配独立的0700 staging，不读ambient TMPDIR；
新只读文件不复用原路径/inode，SDK guard覆盖迟到清理且Retained时保留。identity不授权。
新增12项Host测试（7个真实DLL场景+5个artifact单元测试），包括真实Harness/Faux Provider
AfterToolCall双门控、register/post-commit失败、waiter取消、Retained三模式与实际双Agent
DLL静态runtime隔离。参见[Host测试](../../crates/rpi-cli/src/host/ordinary/extension_custody_tests.rs)。

当前235 Host、28实际binary、255 extensions功能（排除既有benchmark）、4显式DLL smoke、
8资源、69旧CLI Session、20 extcore及1当前binary显式daemon artifact通过，均0ignored；
额外各三轮235 Host与28 binary通过。Core Clippy保持55/97既有warnings。并行SDK/Clippy
压力下曾有既有300ms Stop排空测试失败，未改时限，后续单独三轮全通过。当前binary SHA256：
`9aabb4a285b8add64cc636e4afbccf3138c0873a744d2621968e094b15e2ffe5`。
日志 `/tmp/rpi-host-integration-checks/host-dll-custody-*`（含保留的并发失败证据）。
profile仍拒DLL；准确inventory、独立artifact grant、unsupported能力检查、生产逐Agent
Provider/tool-result hooks尚待接通。**#46/#23 binary迟到callback及管理DLL三方E2E未完成**；
Host Rust/Faux+DLL与既有HTTP binary证据不能拼接冒充。见[验收矩阵](shared-host-lifecycle-acceptance.md)。

## 显式 instruction 资源与持久绑定（最近完整阶段45验收）

私有 Agent profile 的可选 `resources` 沿用 `ResourceSelection v1`。当前只接受绝对 instruction
文件（至多32个、合计64 KiB UTF-8），冻结正文、路径与 SHA；`tools` 必须为 `none` 或与
Agent 工具顺序完全一致的 `only`，仍需 `approvedTools` 授权，不是交互审批或 sandbox。
`default`、package、skills、DLL 与隐式发现均拒绝。`instructions-v1:SHA` 写入不可变 AgentSpec。
纯预检比较完整保存声明、工具 schema inventory 和预算，在 writer/repair 前拒绝不匹配。
真正 owned restore 取得准确 writer 后，在创建 Harness、发布 Live 或 Provider/工具执行前
原子写入 native binding 并严格同步；重开和失败同步后可见的绑定也须重建同步证据，不重复
追加。等待者取消不撤销该过程。源文件变化不影响运行中的冻结正文，重启的新快照不能偷偷
替换已有 Session authority。`{{...}}` 不插值，AGENTS.md/环境/CLI discovery 不参与装配。

本轮 **223 Host / 28 binary / 20 extcore / 1显式daemon artifact passed, 0 ignored**；
resource selection **8**、旧 CLI session **69**、extensions功能 **245**（排除1个既有benchmark）、
显式真实 DLL smoke **4** 均通过。最终额外三轮每轮 **223+28** 全通过；Core/extcore Clippy通过。
阶段45完整验收 binary SHA256：`b752e19b4cdceb64cfc2c14d15246eba6305c2942231476078b0c6b5e7f05fb2`；
日志 `/tmp/rpi-host-integration-checks/instruction-*`，证据见[验收矩阵](shared-host-lifecycle-acceptance.md)。
**DLL 加载仍未开放，#23 binary 迟到 callback 与管理 DLL 三方 E2E 未完成。**

## 只读预检与owned异步装配边界（阶段44历史）

`OrdinaryBinding::new_deferred` 接受纯validator，基于准确Session facts产生未poll的
`OrdinaryAssembly`；readonly preflight直接丢弃计划，真正owned restore才poll。装配错误/恐慌
保守失败，model/provider/tools/drive完整校验仍在writer/repair前；原immediate binder语义不变，
没有新增legacy堆分配或重复校验。validator必须纯，类型本身不证明任意Rust代码没有副作用。
新增4项Native测试：预检/旧guard不装配、7类拒绝/恐慌不修复torn历史且writer可用、放弃waiter
后Service仍持Starting/resident并完成装配、新facts身份改变拒绝。尚未开放DLL/profile资源输入，
仍不是#23实际binary callback或管理DLL三方E2E。

阶段44历史 **220 Host / 27 binary / 20 extcore / 1显式artifact passed, 0 ignored**；extensions功能
**245**（排除1个既有benchmark）、显式DLL smoke **4** 也通过。额外三轮每轮220 Host+27 binary，
全0ignored；Clippy/fmt/四合同一致。阶段44历史binary SHA256：`934a5accb347088d0c34f2d34f046ed964043e9522b7b26e05fceae8455743a9`；
日志`/tmp/rpi-host-integration-checks/binding-plan-*`。

## 插件工具显式Agent身份前置（阶段43）

`ToolCallContext::for_agent` 冻结cwd/agentId/sessionId；显式无Session保持null，CLI late setter
不能重绑，插件调用前覆盖模型伪造的整个`__rpi`对象，原参数不变。旧CLI延迟身份保持兼容。
新增3个unit含同registry两种ABI driver的三Agent实际重叠；显式真实plugin-stub DLL额外验证
8个Agent共享registry/工具、不串Session（其中4个Session-free）。**仍未在Host开放DLL/资源加载**，
不替代#23 binary callback或binary+管理DLL+HTTP三方E2E。

阶段43历史验收Host **216**、binary **27**、CLI session兼容 **69**、extcore **20**、显式daemon artifact **1**；
extensions功能集合 **245**（明确排除1个既有benchmark）、显式DLL smoke **4**、stub unit **4**，
上述验收均0ignored。完整extensions库另为245 passed/1既有benchmark ignored，不将ignored算验收。
额外三轮每轮216 Host+27 binary+3身份unit+4显式DLL smoke，均0ignored；Clippy/fmt/四合同通过。
阶段43debug binary SHA256：`ef847515c9eeb7885659f5404bbb77b32114dfce99924b107f80fd0c4edd626b`；日志`/tmp/rpi-host-integration-checks/tool-context-*`。

## 生命周期验收收口（阶段42）

新增真实binary+HTTP关闭失败保留/显式shutdown重试场景，详见[逐项验收矩阵](shared-host-lifecycle-acceptance.md)。
阶段42历史验收 **216 Host / 27 binary / 20 extcore / 1显式artifact passed, 0 ignored**；额外三轮每轮
**216 Host + 27 binary passed, 0 ignored**。仅新增测试与文档，生产代码/binary及wire1.4未改。
#23仍不标完成：迟到callback目前是Native IPC证据，尚缺实际binary内受信扩展callback场景；
更不能将当前证据冒充binary+管理DLL+HTTP三方E2E。下一步推进显式扩展/资源绑定前置。

## 完整 Stop 提交 CLI（阶段41）

```text
rpi host administrative-stop --config <private-json> --request <absolute-private-original-intent-json>
```

仅接受闭合Stop完整原scope/request_id/epoch/action；沿用4096bytes、私有owned regular file/
parent、单hardlink、非symlink校验。无身份/epoch override，不生成或替换原意图，不读serve-only
profile，不自动重试或等待daemon退出。用同一个原文件administrative-receipt对账。

退出0只表示匹配且验证过的Succeeded NativeSettled历史，不证明当前listener/进程已退出，也不证明
peer收到；1确定拒绝/未发送，2用法/坏输入，3missing/Prepared/Interrupted/Uncertain/丢reply/
不可验证响应/变更丢stdout。历史成功重复不能停掉重启后的新Live owner。旧裸Stop仍不开放。

新增1 unit验证完整Stop收据/epoch/身份/result与状态分类，操作匹配扩五操作×八意图；parser拒绝
重建身份/epoch/目标。新增3 binary：真实CLI+无headers HTTP下正常输出/丢stdout（返回3但远端
正常退出），两种模式均保留原文件、重启原文件查询/重复不误停新owner、不发HTTP/不改JSONL；
另20类坏文件/闭合意图真实listener确认无连接，默认拒绝/Unsupported矩阵加入Stop。

阶段41历史验收 **216 Host / 26 binary / 20 extcore / 1显式artifact passed, 0 ignored**；精确CLI unit **13**。
额外三轮每轮 **216 Host + 26 binary passed, 0 ignored**；Core/extcore Clippy、fmt、双方
whitespace与四合同检查通过。wire1.4/Some12/None5未改，四份wire文件本阶段未改。
最终debug二进制SHA256：`d24782de1e7dea1e73caa4c12ea4f540aa9a0b9fc3ef84c4d2c220a4f346b005`。
日志：`/tmp/rpi-host-integration-checks/stop-cli-*`。仍非管理DLL三方E2E；未提交/安装/部署/发布。

## 受控 Stop IPC（阶段40，wire v1.4）

- 独立命令/能力 `administrative_stop { request }` 只接受闭合 Stop 原意图，保留
  scope/request_id/epoch/action，固定 stop grant 默认拒绝。旧裸 `Stop` 占位仍不开放。
  Some 行政策略公布12能力，None仍为5；四份 wire 合同与 extcore 镜像逐字一致。
- 唯一 fresh owned Stop 在独占 reservation 内完成 native settle、严格发布 NativeSettled、
  成功 close_storage 后，才发本 owner 的易失完成通知。通知不从 journal/cache 恢复；
  历史查询/重复、拒绝、Prepared/Uncertain、发布失败都不能误停新 daemon。
- listener 收到通知后停止 accept/应用层接入；空闲 read/handshake 立即结束。已经 dispatch
  的 exchange 保留原 request_timeout 内的 reply write_all/flush 机会，然后退出，不再读取
  下一帧。所有连接并行有界收尾，不逐连接续期；不依赖 peer 读取，也不 abort 最终 Stop 回复。
  完整 write/flush 仅证明本地写出，不证明 peer 已收到。Socket lease/inode 清理在连接收尾后。
- daemon/serve 区分当前行政 Stop 正常结束和 listener 故障；前者成功退出0。无条件 shutdown
  仍可幂等调用；失败 native 结算/结果发布保留 stopping、owner/catalog 与查询入口，供诊断和
  显式 shutdown 重试。丢 reply/超时不取消 owned job、不换ID或自动重试；跨重启保留原意图对账。
- 新增6个Host测试：Stop闭合schema、4个真实IPC退出/历史/丢waiter/发布失败/背压场景，
  以及真实Harness迟到callback超时仍持writer/owner，直到实际结算才退listener。扩五操作×八意图
  与五变更断线/timeout矩阵、默认拒绝。新增2个真实binary+HTTP测试：不回headers的运行中Stop、
  完整reply/主动丢reply后均正常退出，释放writer/socket/singleton；重启新Live owner后原文件
  receipt与旧Stop重复不误停、不请求HTTP、JSONL不变。不是管理DLL三方E2E。

阶段40历史验收 **215 Host / 23 binary / 20 extcore / 1显式artifact passed, 0 ignored**；额外三轮
每轮 **215 Host + 23 binary passed, 0 ignored**。Core/extcore Clippy通过（沿用既有lint例外），
无新增本阶段诊断。最终debug二进制SHA256：`cfa86158703da3725fcda6767a5cebad2f4b0eb1463af36e5ad1ce9cd7569666`。
日志：`/tmp/rpi-host-integration-checks/stop-ipc-*`。未提交、安装、部署或发布。

## 验证汇总

所有构建使用各 worktree 的 `CARGO_TARGET_DIR="$PWD/target"`、`--locked --offline -j 2`。

| 验证 | 结果 |
| --- | --- |
| core `cargo test -p rpi-cli --lib host::` | **235 passed**，本轮7个真实DLL Host场景与5个artifact测试，前轮3个instruction资源测试，前轮4个预检/装配测试，前轮1个Stop CLI unit，前轮6个Stop测试，前轮1稳定取消CLI unit，前轮2稳定取消IPC/协议，前轮6稳定输入取消测试，前轮4 submit/receipt CLI unit，前轮5行政 CLI unit，前轮新增 5 项行政执行 IPC/协议/客户端，前轮 6 项行政只读 IPC/协议/客户端，另含 7 项 daemon 私有行政装配、12 项原生行政 facade、4 项行政 reservation/分阶段 Stop、7 项 immutable profile 集合、只读 profile 独立环境、5 项 backend/IPC 故障、孤儿 startup 拒绝、6 项 native guarded、5 项 service guarded、18 项行政 journal 及 1 项共享实例锁回归 |
| core全Host与binary全套，各额外三轮 | 各轮 **235 Host / 28 binary passed, 0 ignored**；包含Host Rust/Faux+真实DLL的callback/SDK双屏障、Retained/partial失败及staging隔离，既有HTTP binary回归独立通过，不相加冒充binary DLL E2E；并行SDK/Clippy压力下300ms Stop测试一次失败保留日志，单独各三轮通过 |
| core 行政执行 IPC + 客户端断线/超时 + binary 全套（前序），额外三轮 | 每轮 **3 + 1 + 13 passed, 0 ignored**；含真实动态 HTTP，无管理 DLL |
| core 行政只读 IPC + 客户端断线/超时 + binary 全套（前序），额外三轮 | 每轮 **3 + 1 + 11 passed, 0 ignored**；不执行 lifecycle、不包含管理 DLL |
| core daemon 装配/默认值 + binary 全套（前序），额外三轮 | 每轮 **6 + 1 + 10 passed, 0 ignored**；包含 15 类私有行政配置前置拒绝，无 lifecycle IPC/DLL |
| core 原生行政 facade/存储故障 + Harness owner，额外三轮 | 每轮 **9 + 3 passed, 0 ignored**；非 daemon/IPC 或管理 DLL 贯通 |
| core 行政 reservation/分阶段 Stop，额外三轮 | 每轮 **4 passed, 0 ignored**；包含实际 journal/native 调用，但不是生产 coordinator/IPC |
| core profile 集合/版本/legacy journal，各额外三轮 | 每轮 **7 + 2 + 1 passed, 0 ignored**；不执行 Agent、不升级旧意图为新动作 |
| core `cargo test -p rpi-cli --test host_control` | **27 passed, 0 ignored**；含2个Stop+HTTP退出/重启隔离，取消CLI+HTTP丢stdout/18坏输入无连接/重启原文件对账、丢取消reply/HTTP Aborted/后续输入隔离/重启JSONL不变、gated HTTP准入早于完成/精确原文绑定/重启不重放、14坏文件无连接、真实CLI/HTTP、坏输入无连接/默认拒绝、丢stdout对账、动态生命周期+HTTP、丢行政 reply 对账、Interrupted 历史查询、允许 profile 发现、动态 profile 不自动加载、15 类私有行政配置/7 类无效 profile 创建前拒绝、profile+HTTP Provider+IPC 执行及 SIGKILL 后重启不重放 |
| core 只读 profile 独立环境 + binary 创建前拒绝，各额外三轮 | 每轮分别 **1 passed, 0 ignored**；binary 单项覆盖 7 类无效 profile，Session/progress/writer 与 Agent 记录均未创建 |
| core `host::server::` 额外重复三轮 | 每轮 **11 passed, 0 ignored** |
| extcore `shared_host_daemon -- --include-ignored`，显式本轮二进制 | **1 passed, 0 ignored**；None 保五能力/五执行本地 Unsupported，Some 十二能力/五执行默认拒绝、空发现与未授权 Stop 查询；不跳过、不默认 PATH fallback |
| core `host::bindings::` 额外重复三轮 | 每轮 **10 passed, 0 ignored** |
| `rpi-tools --lib --tests`（前序回归，本阶段未重跑） | **240 passed, 0 ignored**，含 exclusive-create 5 项、严格同步 4 项及显式环境 3 项；另 **4 doc tests** |
| core `cargo test -p rpi-runtime --lib` | **73 passed**（前序验证） |
| core `cargo test -p rpi-harness --lib` | **542 passed, 4 ignored**，含物理同步 11 项、原生 request admission 12 项与 exclusive-create 10 项 |
| core `cargo test -p rpi-cli --lib args::tests` | **41 passed** |
| core `cargo test -p rpi-cli --lib startup_diagnostics` | **6 passed** |
| core `cargo test -p rpi-extensions --lib` | **242 passed, 1 ignored** |
| 显式构建 DLL 后 `plugin_stub_smoke -- --include-ignored --nocapture` | **3 passed, 0 ignored**；缺 fixture 不允许跳过 |
| extcore `cargo test -p rpi-extcore --lib shared_host::` | **20 passed**；含Stop闭合/完整意图、八意图/五操作匹配、稳定目标闭合/guard、丢 mutation reply 分类及行政闭合查询、receipt 形状与 golden fixture |
| extcore 完整 `cargo test -p rpi-extcore` | **371 库 + 11 resource recovery passed；5 ignored** |
| extcore `cargo check -p rpi-extcore --all-targets` | 通过 |
| 四个跨仓库文件字节比较、两侧 `git diff --check` | 通过 |

extcore 默认五项 ignored 包括已有四项显式 native/CDylib 套件与新增一项必需 binary 套件，
都不算通过；本轮新增 binary 套件另行显式执行，结果见上表。核心默认 integration smoke
可能自行跳过 fixture，所以此前另行显式构建并运行必需产物 DLL 测试，不能仅从默认测试
的 `ok` 推断 DLL 已加载。本轮行政 control 验证未将历史 DLL hook 测试重标为管理 DLL E2E。

本轮显式 DLL：

- 路径：`/Users/bigfish/work/rpi-projects/pi-rust-wt-host/target/debug/libplugin_stub.dylib`
- SHA-256：`2a21f5ea4af06cb8ea7957a6d0829aac2d7e466ee127428a741c90d72cd56507`

重现真实 DLL 测试（macOS；其他平台替换产物文件后缀）：

```bash
CARGO_TARGET_DIR="$PWD/target" cargo build -p plugin-stub --locked --offline -j 2
RPI_PLUGIN_STUB_PATH="$PWD/target/debug/libplugin_stub.dylib" \
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-extensions \
  --test plugin_stub_smoke --locked --offline -j 2 -- --include-ignored --nocapture
```

### Lint 限制

未改写不相关代码，也未在源码添加全局 lint suppression：

- core 默认 clippy 被既有 `actions.rs:1557` 的 `not_unsafe_ptr_arg_deref` deny 阻止。
  仅在验证命令行豁免该已知项后，两目标库检查通过，仍有既有 warnings；本轮新增的
  service/protocol/client/provider hook 文件未报告 warning。不宣称 workspace 零告警。
- extcore 严格 all-target clippy 发现既有 `prompt.rs` 的 `useless_conversion` 和测试夹具
  `resource_saved_core.rs` 的 `needless_late_init`；仅在验证命令行豁免这两项后通过。
- 最初离线依赖求解带来的无关 wasm/js 锁版本升级已撤回；最终 lock diff 只新增 nix
  及 extcore 的 nix/tempfile 依赖边。

日志在 `/tmp/rpi-host-integration-checks/`，不作为项目内可预览文件链接。
本轮遇到其他工作区的长时间 `cargo fetch` 占用全局 package-cache 锁，未终止它、未删除锁。
后续使用独立 `/tmp/rpi-host-cargo.*` APFS clone 缓存离线验证，没有复制 credentials/config，
也没有共享写入原缓存。构建产物仍位于各自 worktree 的 target；缓存路径记录在上述日志目录，
保留供后续增量验证，避免来回切换源码缓存路径导致全量重编译。
完整 workspace/Windows/Linux 实机未跑；没有本轮 600/3000 性能结果。既有底座 benchmark
不经过新服务路径，不重标为 Host/mini 性能验收。
