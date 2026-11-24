# Shared Host：前台控制与执行 daemon（阶段性）

> **薄 Host 收敛 A 批**：新增私有 `mode` 与 `recovery` 配置、严格缺省/组合校验。
> 真正后台 daemon、整组恢复和自动重启仍未接通；`serve` 在 profile/catalog 前明确拒绝
> 这些请求，合法配置仍可用于查询已有 Host。旧配置保持前台行为，控制协议未变。
> B1 已提取无 catalog 的内存成员表与生命周期状态，现有 HostService 共用唯一核心。
> B2a 增加私有原生 Harness 接管/取消/收尾适配，B2b 增加可信可重试 cleanup 屏障；
> B3a 在等待屏障前解除已关闭 Harness 自身对运行绑定的持有，但具体资源和默认 CLI 未装配。
> B3b1 已让正常 CLI prepare/reload 收集全代际（含失败加载）的不持库收据；不改 SDK ABI，
> 记录不等于最终资源关闭。B3b2 增加现有 CLI 插件绑定 owner、串行 reload/retirement 门和
> TUI 任务 joins；关闭方法仍是待接的子集，必须先结算所有其他 lease 使用者，不证明
> 工具进程退出。B3b3 已将 native child/side-question 准备与任务纳入 owned 域并实际 join，
> 不等于 OS 工具/后台后代结算；Host/binary 通过。完整 CLI 重跑仍有 4 项 codesign 失败，
> 首跑另有一次 fork 断言失败、定向与完整重跑未复现，原因尚未确定，原日志保留。
> B3c1 历史新增仅 Darwin 的原始进程组 keeper 基础：独占 wait/WNOWAIT 固定 PID，完整内核
> zombie 快照及冻结后实际退出/reap；失败/Drop 保真 guard 和撤销证据。7 项实际进程测试、
> Host 275、重建 binary 31 通过；尚未接工具环境，不覆盖逃逸后代/外来 reaper/父死，
> 不能作为完整 HarnessCleanup。B3c2 为每个 dev 实例分配新 UUID 并原子独占创建 staging，
> 修复同包同 PID 的目录别名；dev 8、Host 279、重建 binary 31 通过。仅命名空间前提，
> 未托管 watcher/延迟 helper 或 Cargo 后台后代；rebuild 仍同步。SDK/完整 CLI 本批未重跑，
> 上述 CLI 失败属于 B3b3 历史，未因当前结果消除。
> 字段、边界及当前结果见 [Host README](../../crates/rpi-cli/src/host/README.md)。
> 以下阶段记录与 SHA 为既有验收快照，不是新 daemon/整组恢复能力的验收。

当前 feature worktree 新增 `rpi host serve/status/list/inspect`。它是同一个 rpi 二进制的显式
Unix 入口，不是新 crate、逐 Agent RPC 子进程或已部署服务。**这还不是完整生产 Host：
daemon 可按启动 `agents` 或固定私有行政策略，经完整意图 IPC 创建/恢复/关闭 Agent；
IPC 可查询收据并向已加载 owner 提交文本与稳定输入取消；行政 CLI 使用完整原请求文件。
独立Stop IPC及有界reply/退出排序已接；管理扩展、mini、审批和执行 TUI 尚未接通；Stop提交CLI已接。**

## 入口与私有配置

```text
rpi host serve --config /absolute/private/host.json
rpi host status --config /absolute/private/host.json
rpi host list --config /absolute/private/host.json --limit 16
rpi host list --config /absolute/private/host.json --after <agent-uuid> --limit 16
rpi host inspect --config /absolute/private/host.json --agent <agent-uuid>
rpi host profiles --config /absolute/private/host.json --limit 16
rpi host submit --config /absolute/private/host.json --request /absolute/private/original-submission.json
rpi host receipt --config /absolute/private/host.json --request /absolute/private/original-submission.json
rpi host administrative-receipt --config /absolute/private/host.json --request /absolute/private/original-intent.json
rpi host administrative-create --config /absolute/private/host.json --request /absolute/private/create-intent.json
rpi host administrative-restore --config /absolute/private/host.json --request /absolute/private/restore-intent.json
rpi host administrative-close --config /absolute/private/host.json --request /absolute/private/close-intent.json
rpi host administrative-cancel --config /absolute/private/host.json --request /absolute/private/original-cancel-intent.json
```

`serve` 只在前台运行；不自动 fork、查找端点、生成 token、扫描/恢复 catalog Agent 或
重放输入；只有显式 `agents` profile 会在启动时加载。
下面只是生成私有 demo 配置的操作示例，**未在全局位置安装或启动服务**。长期目录应由使用者
明确选择；本例 `/tmp` 目录不是长期存储部署方案。

```bash
HOST_DIR=$(mktemp -d /tmp/rpi-host-demo.XXXXXX)
python3 - "$HOST_DIR" <<'PY'
import json, os, pathlib, secrets, sys, uuid
root = pathlib.Path(sys.argv[1]).resolve()
config = {
    "directory": str(root / "catalog"),
    "socket_path": str(root / "control.sock"),
    "host_id": str(uuid.uuid4()),
    "token": secrets.token_urlsafe(32),
    "max_connections": 8,
    "handshake_timeout_ms": 5000,
    "request_timeout_ms": 30000,
    "client_timeout_ms": 5000,
}
fd = os.open(root / "host.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as output:
    json.dump(config, output)
PY
# 在 core feature worktree 构建后，用它自己的二进制；不要替换已安装的 rpi。
./target/debug/rpi host serve --config "$HOST_DIR/host.json"
# 另一终端使用同一显式配置调用 status/list/inspect；SIGINT/SIGTERM 请求正常停止。
```

所有路径必须绝对。配置文件上限 64 KiB，要求 owned/private parent、owned/private
regular file、单硬链接；以 NOFOLLOW/CLOEXEC/NONBLOCK 打开后检查 FD metadata，拒绝
symlink、FIFO、宽松权限、未知字段和错误配置，不打印原始内容。没有 token argv/env fallback。
完整字段见 `rpi host --help`；可选 `host.max_agents` 沿用现有容量规则，非空 Host timeout
策略当前明确拒绝，不能配置一个尚未运行的计时器。IPC 的握手/请求期限则已真实执行。

配置 token 可**查询整个 catalog，并向所有已加载 Agent 提交实际 Provider/工具执行**，
不是按 Agent/项目限制的 grant；不要将 token、
配置内容或连接配置作为模型上下文/工具参数。未来管理扩展必须走受控 runtime action，
不能把该 token 发给模型。路径父链依赖可信本机文件系统，不防恶意同 UID 进程。

## 可选私有行政配置（固定 native authority；typed journalled IPC）

在 Host JSON 中显式增加，例如（身份值应由 owner 选择并固定保存）：

```json
{
  "administration": {
    "scope": "22222222-2222-4222-8222-222222222222",
    "allowed_agents": ["33333333-3333-4333-8333-333333333333"],
    "permissions": ["create", "restore", "close"],
    "profiles": ["/absolute/private/on-demand-agent.json"],
    "max_jobs": 8
  }
}
```

- `scope` 是固定请求 namespace，不是凭证；每次启动随机改变它会失去原 scope 的精确
  对账入口。允许的 Agent/动作默认空；重复 ID/动作、未知字段/动作、相对动态路径拒绝。
- `max_jobs` 1..64，默认 8；启动 `agents` 与动态 `profiles` 合计最多 1024。只有 `serve`
  读取 profile 文件，status/list/inspect 不会加载配置中的 Agent 文件或创建行政权限。
- 启用后在 catalog/Session 创建前统一 freeze 两组输入，拒绝跨集合身份/Session/路径/
  inode 别名和闭合绑定错误。允许的 Agent 必须在该集合中，启动项不要再重复列入动态项。
  `profiles` **不创建 Session、不注册、不自动加载**；它只提供受信的动态输入。
- daemon 保留固定 grant 的 native `administration()` handle，只发现允许的 Agent ref。
  真实 native Create/Restore/Close/Stop 仍通过 journal/fresh/reservation；Native Stop 的
  NativeSettled 不证明 listener 已退出，另由当前owner成功close_storage后的易失通知驱动退出。
- 配置后额外开放 `profiles` / `administrative_receipts` 查询以及五项独立的
  administrative_create/administrative_restore/administrative_close/administrative_cancel/administrative_stop（见下文）；
  无 legacy ticket Cancel/裸Stop占位 IPC。四项执行的完整请求CLI见下文；配置也不缩小现有 token 对所有已加载 Agent 的
  查询/submit 权力，不是 scoped tool approval
  或文件系统沙箱；模型/IPC 不能提供 grant、凭据或提升预授权工具集合。
- 不配置 `administration` 时保留原 startup 行为，不安装 facade。启动失败结算已经
  加载的 owner，bind 失败保留外来端点并释放 writer/catalog，不偷偷删除或覆盖历史。

## 显式 Startup Agent profile

要让冷启动 daemon 有可执行 owner，在上面的 config 增加：

```json
{"agents": ["/absolute/private/agent.json"]}
```

`agent.json` 必须位于私有 owned 父目录、为单硬链接 owned 私有常规文件，最多 256 KiB；
沿用 config 的 FD 校验，不跟随最终 symlink，不打印解析错误中的原始值。下面是结构示例，
其中 URL、密钥和路径须由使用者明确填写；不会从进程环境补全 `$PROFILE_KEY`：

```json
{
  "agent": {
    "id": "11111111-1111-4111-8111-111111111111",
    "name": "ordinary-agent",
    "cwd": "/absolute/workspace",
    "session": {"id": "ordinary-session", "path": "/absolute/workspace/session.jsonl"},
    "model": {"provider": "explicit", "model": "explicit-model"},
    "tools": []
  },
  "runtime": {"providers": {"explicit": {
    "api": "openai-completions",
    "baseUrl": "https://provider.example/v1",
    "apiKey": "$PROFILE_KEY",
    "models": [{"id": "explicit-model", "contextWindow": 32768, "maxTokens": 1024}]
  }}},
  "providerValues": {"PROFILE_KEY": "replace-with-private-provider-key"},
  "toolEnvironment": {"HOME": "/absolute/workspace", "PATH": "/usr/bin:/bin"},
  "shell": "/bin/sh",
  "approvedTools": [],
  "thinking": "off",
  "timeoutMs": 30000
}
```

workspace 与 Session 直接父目录须已存在；无效绑定/授权在创建 Session 或获取 writer 前
拒绝。仅当 Session 路径不存在时才尝试原子 exclusive-create；不是检查之后再截断写。
真实 writer lease 覆盖整个初始化：先独占创建空主文件，再独占创建/严格同步 progress，
成功后才追加可读 header 并严格同步主文件及父目录。任一既有最终路径（包括 symlink、
hardlink、孤儿 progress）不被新建覆盖；同 ID 的有效孤儿也不因失败新建而获得权威。
取消等待不提前释放初始化 writer。失败可留下空占位/部分新文件供显式检查，不能盲目
删除、覆写或视为两文件原子事务；空占位在下次 startup 也不能被恢复为成功 Session。
既有 Session 交给严格恢复（允许原生恢复自身的修复，不等于新建可覆盖它）。
同 Agent ID 重启时要求 catalog `AgentSpec` 完全匹配，复用注册和 Session。不读取 ambient
settings/凭据/工具环境或插件；工具授权只来自私有 profile，不能由 IPC 提升权限。
`bind(&self, spec)` 在创建 Session/注册新 Agent 前完成 AgentSpec 静态校验、闭合必需
headers/非命令凭据及精确 Provider/model 解析；不等于探测远端 Provider 可达性。只读
profile 可重复生成独立执行环境，不共用 cleanup 所有权。现存 Session 的资源声明仍在
readonly facts 阶段校验，先于 writer/修复。

已新增 native `TrustedProfileSet` 底座：消费可信输入并预检后冻结（最多 1024），私有配置
不派生 Debug/Serialize，无插入/替换接口。引用只含固定 AgentId 与随机版本，不含秘密
或配置摘要；准确版本查找后每次生成独立 binding。拒绝重复 Session 身份、归一化路径、
Unix inode 别名及最终 symlink/非文件路径；不冻结外部文件系统，也不预先接受已有内容。
它已连接 `serve` 私有配置、只读发现/收据及三项显式 profile create/restore/guarded close；
不能由 peer 提供 spec 或工具授权，只有完整意图经过固定 grant/journal/fresh 才可执行。
若后一个 Agent 启动失败，先结算前面的 owner 并释放 writer/catalog，不发布 readiness；
已成功创建的 Session 和注册记录会保留供检查/重试，不承诺多 profile 原子事务回滚。

此路径只在 `serve` 读取；status/list/inspect 不读取 profile 或加载 Agent。CLI 目前没有
submit 子命令；执行仍通过认证 `HostClient` 的文本 Submit/Receipt，运行时 create/load
与其他 lifecycle 变更未开放。

## 控制合同与上限

- 使用 `rpi-shared-host` wire v1.3（major 1 兼容），显式 Host ID/token、Unix peer UID、私有 socket parent。
  仅新建不存在的 socket，0600；拒绝已有文件/端点，不自动 unlink stale socket。
- 基础 `Welcome.capabilities` 为 **status/list/inspect/receipts/submit**；只有配置
  `administration` 才增加 **profiles/administrative_receipts** 及独立的
  **administrative_create/administrative_restore/administrative_close/administrative_cancel**，合计11项；能力不是权限。
  每连接 `max_in_flight=1`，帧最多 1 MiB，
  页最多 128。最多同时保留 8 个连接任务（默认，可显式设置 1..64）；超限连接直接关闭，
  不分配帧/任务。已结束任务及时回收，无逐请求无限后台队列。
- 握手总期限默认 5 秒；每次查询的空闲/read + dispatch + write 总期限默认 30 秒。
  不因每个字节到达重置；两个期限均须非零且不超过 300 秒。慢读/慢写消费者被断开，
  不持全局异步执行锁，不阻塞另一连接的查询。
- 除 framing/响应预算外，底层 catalog 仍有 64 槽入口、128 KiB 单记录上限与分页上限。
  list 在 worker 中最多读取一页完整记录再投影，不宣称统一进程内存配额或小于页上限的
  临时内存。CLI/stdout 的消费和调用者自行保留结果不属于 IPC 任务预算。
- 旧 epoch 返回 StaleEpoch；错误只有固定枚举，不输出原始帧、token、路径或 OS 错误。
  client 对未协商能力返回 NotSent；绕过 client 发送未实现的 create/restore/cancel/close/stop
  返回 Unsupported；submit 仅向 Live owner 投递文本（≤256 KiB、request ID 幂等），另有三项
  独立完整意图行政执行。submit 回复 durable admission receipt 而非执行结果。
  未声明/非法 JSON 请求关闭连接。

`Reply::Data.data` 当前结构：

| 查询 | 数据 |
| --- | --- |
| status | epoch、registered/resident/executing/busy/mini、max_agents、owners、stopping |
| list | agents 摘要（id/name/parent_id/mode/state/revision）、next_cursor |
| inspect | 单个原生 catalog AgentRecord：spec/state/revision/last_epoch |
| submit | durable、agent_id、request_id、receipt；不等待执行完成 |
| receipt | found、agent_id、request_id，存在时有 receipt 与 execution |

list 是 keyset 分页，不是跨页快照。status 的 catalog 计数与 owner/stopping 分别采样，不是
跨层原子快照，也不是工具 barrier。inspect 只读取管理元数据，不打开 Session transcript、
不获取 Session writer、不验证或修复它指向的 JSONL；返回的 Session/path 也不是恢复成功证明。
mini 的 Lost 元数据不是 mini 执行实现。

## 生命周期与失败

`HostDaemon` 持有 `HostService` 和 `ControlServer`；Rust 原生可信集成可经 service 装配绑定，
但 CLI 不从配置加载 binder/provider/资源。普通 CLI 默认路径未切换；host 入口在常规
settings、durable、trust、Session、Provider、DLL 初始化之前分流。

- client detach/EOF、等待 future 被取消、IPC server stop，均不等于关闭 Agent。
- SIGINT/SIGTERM 请求 service 原生协作取消与结算，先排空后端、清单和单实例锁，再确认
  control 连接任务结束并清理 socket。不强杀 backend，不伪造 shutdown grace 成功。
- 原生关闭失败保留 owner/容量/catalog singleton 和查询端点，status 显示 stopping；下个
  信号或 Rust API 调用可显式重试。最终 Closed commit 失败时 writer 可能已经释放，
  保留管理槽不代表仍持 writer。完整 reconciliation 管理 API 尚未实现。
- ControlServer stop 只取消连接 waiter，不取消独立监督的 admission/owned 行政 job。
  service shutdown 的生命周期 fence 仍负责原生结算，不把 abort 连接当作执行结算。
  socket 清理检查创建时 dev/inode，并在 listener FD 仍持有时执行；不删除替换文件/端点。
- ready stdout 写失败也经过 shutdown，不因打印 panic 遗留端点。Drop 仍只是 best effort；
  嵌入者必须显式 shutdown 并保留 runtime，不能把丢弃所有者视为结算完成。

## 行政发现与收据查询（不执行 lifecycle）

- `Profiles { after, limit }` 按 AgentId 返回允许集合中的 `{agent_id, version}`、固定
  `scope` 与 `next_cursor`，1..128 项；不包含凭据、路径、配置摘要，不加载或注册 Agent。
- `AdministrativeReceipt { request }` 携带完整原 scope/request_id/epoch/action；外层 epoch
  仍是当前握手 epoch，内层历史 epoch/旧 profile 版本不要求属于当前冻结版本。server
  使用固定 grant 先授权，然后原生 journal 严格同步/完整参数对账，返回 found/receipt。
  同 ID 改参数是 Conflict；无权限是 Unauthorized。missing 不 prepare、不接管、不重放。
- 原动作是闭合 typed enum，包含 legacy Create/Restore、Cancel/Close/Stop 以查询历史，
  不是相应命令权限；不能夹带 peer grant、配置、凭据或任意 JSON。query 嵌套的目标 request
  ID 不会被当作本次 mutation ID；丢连接/超时为 QueryFailed，不误报 OutcomeUnknown。
- 沿用既有连接/帧/总期限预算；查询不执行，独立变更/CLI见下文。稳定输入取消已接 IPC；
  Stop 已有独立owner完成通知与有界reply/退出排序，不能把 NativeSettled 当进程退出证明。

## DLL装配推进中（阶段46，尚未开放profile输入）

严格owned loader与cleanup receipt已接入可信Rust binding的Host custody，失败注册/装配及
关闭都必须等待实际callback、SDK/handle/guard释放，才可释放已取得的writer与驻留预占。
Retained/未知保留Starting/StartFailed/CloseFailed；同一custody拒绝跨Agent复用。最后Drop
无证明时保留writer/catalog单实例锁至进程退出，而不是把排队清理当作结算。

[冻结artifact](../../crates/rpi-cli/src/host/extension_artifacts.rs) 只读冻结至多8个DLL、单个
64 MiB/合计128 MiB，拒绝canonical/hardlink别名。owned阶段在显式private owned根目录
原子创建每Agent/每次装配独立的0700 staging，只加载冻结bytes的新文件；SDK guard让
目录覆盖迟到cleanup，Retained时不删除。identity和Rust API都不是native code grant。
新增12项Host测试（7个真实DLL场景、5个artifact单元测试）覆盖真实Harness/Faux Provider
AfterToolCall双屏障、部分失败、取消waiter、Retained以及双Agent实际静态runtime隔离。

当前235 Host、28实际binary、255 extensions功能（排除既有benchmark）、4显式DLL smoke、
8资源、69旧CLI Session、20 extcore及1显式daemon artifact通过，均0ignored；额外各三轮
235 Host/28 binary通过。Clippy保持55/97既有warnings，沿用已有raw-pointer兼容例外。
并行SDK/Clippy压力下曾有既有300ms Stop排空测试失败，未改时限，后续单独三轮全通过。
当前binary SHA256：`9aabb4a285b8add64cc636e4afbccf3138c0873a744d2621968e094b15e2ffe5`。
日志 `/tmp/rpi-host-integration-checks/host-dll-custody-*`；staging权限首次失败单独保留。
准确插件inventory/独立artifact grant/unsupported能力拒绝与生产逐Agent hooks尚待接通，
profile仍拒DLL。**Host Rust/Faux+DLL和独立HTTP binary测试不能拼成#23 binary callback或
管理DLL三方E2E，#46仍未完成。** 细节见[验收矩阵](shared-host-lifecycle-acceptance.md)。

## 显式 instruction 资源与持久绑定（最近完整阶段45验收）

私有 Agent profile 的可选 `resources` 沿用 `ResourceSelection v1`，当前只接受绝对 instruction
文件（至多32个、合计64 KiB UTF-8），冻结正文、路径与 SHA。`tools` 必须为 `none` 或与
Agent 工具顺序完全一致的 `only`，仍需 `approvedTools` 授权；不是交互审批或 sandbox。
`default`、package、skills、DLL 与隐式发现均拒绝。示例片段：

```json
"resources": {
  "version": 1,
  "tools": { "mode": "only", "names": ["read"] },
  "instructions": ["/absolute/trusted/instructions.md"]
}
```

`NativeInstructionResources` 的 `instructions-v1:SHA` 写入不可变 AgentSpec；纯预检比较完整
保存声明、工具 schema inventory 和预算，在 writer/repair 前拒绝不匹配。真正 owned restore
取得准确 writer 后，在创建 Harness、发布 Live 或 Provider/工具执行前原子写入 native binding
并严格同步；重开和失败同步后可见的绑定也须重建同步证据，不重复追加。等待者取消不撤销
该过程。源文件变化不影响运行中的冻结正文；重启的新快照不能偷偷替换已有 Session authority。
正文中的 `{{...}}` 不插值，AGENTS.md/环境/CLI discovery 不参与装配。DLL 加载仍未开放，
不能将这些证据称为 #23 binary 迟到 callback 或管理 DLL 三方 E2E。

本轮 **223 Host / 28 binary / 20 extcore / 1显式daemon artifact passed, 0 ignored**；
resource selection **8**、旧 CLI session **69**、extensions功能 **245**（排除1个既有benchmark）、
显式真实 DLL smoke **4** 均通过。最终额外三轮每轮 **223+28** 全通过；Core/extcore Clippy通过。
阶段45完整验收 binary SHA256：`b752e19b4cdceb64cfc2c14d15246eba6305c2942231476078b0c6b5e7f05fb2`；
日志 `/tmp/rpi-host-integration-checks/instruction-*`，证据见[验收矩阵](shared-host-lifecycle-acceptance.md)。

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

## 完整意图行政执行（wire v1.4，五项执行 CLI 使用显式请求文件；Stop IPC见下文）

- 独立 AdministrativeCreate/Restore/Close/Cancel 只接受 CreateProfile/RestoreProfile/
  guarded Close/CancelSubmitted。旧占位 capability 未公布；legacy Create/Restore 只能查询，
  ticket Cancel/Stop 不能夹入新命令。默认 grant 拒绝全部；scope/Agent/动作先授权。
- 原 scope/request_id/epoch/action 全部保留，outer current epoch 与原 epoch 不混淆。
  固定 native coordinator 只有严格持久 fresh 可执行，重试先查完整历史再解析 profile。
  Response 是原 LifecycleReceipt，状态需显式检查；旧 Loaded 也不代表当前已加载。
- caller 丢失/整体期限只放弃 waiter，owned job 继续；本次 mutation ID 由原 request_id
  提供，客户端保守 OutcomeUnknown。重连以完整原意图查 receipt；found=false 可能只是
  owned job 仍在 reservation 前，不能据此换 ID 重放。remote OutcomeUnknown/Uncertain
  也需要对账，不声称副作用 exactly-once。
- 既有连接/帧/期限与 native distinct jobs 预算独立有界。500ms server deadline 测试在
  held fence 下丢 waiter，确认 active job 占 capacity、无 fresh/文件，然后释放并完成。
- 实际 binary 两轮 HTTP 动态 Create→Submit→Close→Restore→Submit→Close，逐次验证用户
  prompt 到达 Provider、Session 有输出而不泄 key；重启 refreeze 后重复旧完整执行请求
  不重新加载或改文件。另测丢弃真正回复后对账/重复不重建；不包含管理 DLL。

## 行政 CLI 的请求与退出码

`--request` 是完整原意图 JSON：`scope`、`request_id`、`epoch`、`action`（snake_case op、
profile 引用或 guarded close），不是 Agent profile/spec。请在第一次执行前保存到绝对私有
owned regular 文件（parent 0700、file 0600、无 symlink/hardlink、≤4096 bytes）；同一个
文件用于原操作重复或 `administrative-receipt`。CLI 不 mint/更换 ID、epoch、版本，不自动
重试或根据当前 Profiles 重构原请求。不允许 `--epoch`/`--request-id`/grant/secret flags。
无效/错操作输入在连接前拒绝；查询进程不读取 `agents`/动态 profile 内容。

| 退出码 | 含义 |
| --- | --- |
| 0 | 严格匹配原意图且 result/action/revision 合法的 Succeeded 收据；历史结果不是当前驻留/进程退出证明 |
| 1 | 确定未发送/拒绝；包括不支持的能力、默认权限拒绝和 journal Rejected |
| 2 | 用法或 request 文件路径/权限/schema/预算/动作匹配错误，不连接 IPC |
| 3 | missing/Prepared/Interrupted/Uncertain、丢回复/查询失败、不可验证响应或 mutation stdout 写失败；保留原文件查收据，不换ID重放 |

valid 收据（包括非成功）输出原 JSON；非法响应不回显其内容，固定诊断。stdout 写失败
不能把已完成的远程 mutation 变成安全新请求。`profiles` 复用 list 的稳定 cursor/1..128
预算。Stop CLI 已接（见阶段41）；稳定取消CLI与普通文本submit/receipt CLI见下文。

## 普通文本 submit 与 Agent receipt CLI

两者都读取同一原私有JSON文件：`agent_id`、`request_id`、`prompt`，不接受prompt/凭据
argv flags，也不生成、改写ID/输入或自动重试。文件≤1MiB，绝对private owned regular/
parent、无symlink/multiple-hardlink；UTF-8 prompt非空≤256KiB、稳定Native单消息序列化
≤512KiB。包括receipt查询的文件/schema/预算都在连接前验证，query不读取serve profiles。

submit退出0是durable custody ACK，不是执行完成。receipt退出0是匹配精确原输入的
custody observation，`execution`的Admitted/Started和`finished`只是Native日志证据，
不是工具/运行成功。missing、丢回复/查询失败/存储不确定、非法响应及mutation stdout错误3；
明确未发送/拒绝1，坏输入/用法2。请保留原文件对账；同ID不同prompt沿用InvalidRequest，
不能换ID绕过。查询用错原prompt的已存在收据也返回3且不输出误导JSON。

响应严格核对Host/Agent/request/entry身份、entrySeq/history、messageCount=1和精确bytes、
SHA256(v2)或历史v1双SHA256、found/durable/execution形状和run UUID。没有等待执行、
恢复/replay或工具exactly-once保证。新增4unit/2binary证明真实gated HTTP在CLI准入后尚未
完成，重复/真实断stdout/错原文/missing/完成后重启对账均正确、只有一条admission且文件
不改；14坏文件两操作均未connect。行政CLI动态测试也已改用真正submit/receipt CLI。

## 稳定 submitted_request_id 取消 Native 底座

原volatile ticket接口保留，新增普通后端 `cancel_submitted_guarded`、Service对应方法和
Native `LifecycleAction::CancelSubmitted {agent_id,revision,submitted_request_id}`；不是
生命周期请求自身的request_id。Active保存原输入关联，同control gate检查guard/目标并
signal。重复submit生成的observer ticket不能覆盖关联，晚到取消不能击中下一输入。

显式deferred poll只在原owner生命周期内携带关联；parked没有active时signal=false，
不自动resume。完成/成功close清除，restore不从receipt/runId猜测或恢复取消关联。
Snapshot关联仅易失观察，非持久化收据。固定cancel grant、Service read reservation、
owned job与fresh journal资格均沿用；旧Cancel(ticket_id)与新意图完整参数区分，旧行不
改写，Pending重启仅Interrupted，历史CancelSignalled不再次信号当前运行。

新增6测试含真实Harness/JSONL、held reservation丢waiter、默认拒绝/不存在输入false、
legacy journal/重启、以及Native daemon+HTTP：首次Provider连headers都不返也可Abort
结算；第二次HTTP gated时重复历史/旧目标无误伤，重启不请求Provider且JSONL字节不变。
这些Native-only测试不将信号成功说成工具结算/运行成功；IPC增量见下节，管理DLL仍未接通。

## 稳定输入取消 IPC（wire v1.3）

独立 `AdministrativeCancel {request}` / `administrative_cancel` 仅接受 `CancelSubmitted`，
必须保留完整scope/request_id/原epoch/action。行政request_id不是被取消输入ID；固定cancel
权限默认拒绝，不从scope或peer反序列化grant。阶段38当时Some11能力/None5能力；当前v1.4为Some12/None5，旧ticket Cancel/裸Stop仍不开放。
丢回复/timeout仍为原行政ID的OutcomeUnknown；用完整原意图查询，不换ID、不接管、不重放。
历史成功仅是当时signal，缺目标/parked/restore无关联返回false，不resume；不是结算证明。

新增闭合协议测试及真实daemon+HTTP IPC：错scope/Agent/guard/epoch无误伤，旧ticket动作
不匹配；500ms server deadline后job仍占唯一槽，held reservation释放后owned完成。下一输入
门控时同ID改目标Conflict、历史重试/新意图旧目标false均不误伤。新增真实binary+HTTP测试
主动丢取消reply，首输入在Provider不发headers时Aborted，第二输入正常完成，重启对账与
JSONL字节保持。扩展四执行×八意图矩阵、断线/timeout分类及默认拒绝artifact；非管理DLL E2E。

## 稳定输入取消 CLI

`rpi host administrative-cancel --config <private-json> --request <absolute-private-original-intent-json>`
只接受CancelSubmitted，复用完整原scope/request_id/epoch/action与私有有界≤4096bytes文件。
不生成或替换guard/目标/ID，不等结算、不重试、不自动等待；保留原文件查administrative-receipt。
退出0仅校验Succeeded历史，CancelSignalled(false)同样不是永久无作用或结算证明。
1确定拒绝/未发送，2用法/坏文件/schema，3missing/Prepared/Interrupted/Uncertain/丢回复或stdout。

新增完整target/guard/result unit，扩4×8意图及override拒绝。新增2个binary：真实CLI fresh
取消后丢stdout返回3、原文件查历史/重复，第二HTTP门控时旧目标/冲突不误伤，重启原文件
对账且JSONL不变；18类坏文件/目标无连接。默认拒绝/Unsupported矩阵扩4操作。wire合同不变。

## 本轮验证与证据边界

- 最新 Host 库 **235 passed, 0 ignored**，本轮新增 **7个真实DLL Host场景及5个artifact测试**，前轮 **3个instruction资源测试**，前轮 **4个预检/装配测试**，前轮 **1个Stop CLI unit**，前轮 **6个Stop测试**，前轮新增 **1稳定取消CLI unit**，前轮 **2稳定取消IPC/协议测试**，前轮 **6稳定取消目标测试**，前轮 **4 submit/receipt unit**，前轮 **5行政 CLI unit**，前轮新增 **3 项真实行政执行 + 1 协议矩阵 + 1 客户端**，
  前轮另有 **3 项真实 daemon 查询 + 2 项协议 + 1 项客户端**，
  另含 **7 项 daemon 私有行政装配**、
  **12 项原生行政 facade**、
  **7 项 immutable profile 集合**、
  profile 解析、只读 profile 并发绑定的独立环境、真实 HTTP Provider 的启动/重启、
  部分启动失败清理、孤儿 progress 的创建/重启拒绝、5 项 backend/IPC 同步故障、
  6 项 native guarded cancel/close 及 **18 项独立行政 journal** 测试。后者新增 profile 版本
  去重、restore guards 与 literal legacy v2 重启兼容；前序覆盖 schema 1→2、
  完整参数去重/唯一 fresh、丢弃回复、重启 Interrupted 不重放、终态保存、容量、真正
  SIGKILL 的 catalog 子进程和写入/checkpoint 故障；见
  [journal 边界](shared-host-control-integration.md#独立行政-journal已验收未开放执行ipc)。
  journal 本身不执行 Agent；固定 facade 已连接 guarded 动作及五项完整意图 IPC，legacy ticket Cancel/裸Stop占位除外。
  另有 **1 项**共享描述符确定性回归：数据库/配额排空后显式 unlock 再确认 shutdown，
  防止并发 fork/exec 临时继承锁而延迟最后 close；旧描述符不能解锁新 owner。
  这 **16 项**本轮新增另重复三轮通过。最新另有 **5 项 service guarded** 测试并重复三轮：
  GuardRejected 恢复原 phase、已接受失败保留 owner、精确 ticket、StartFailed fence、
  丢弃 waiter/迟到结算及 shutdown 写屏障。其原生 journal 装配与五项执行 IPC 已完成。
  最新另有 **4 项行政 reservation/分阶段 Stop**，并额外重复三轮：owned callback 覆盖
  prepare/native/finish，reserved restore/cancel/close 在 shutdown writer 排队后不递归读锁；
  丢弃 Stop waiter 仍等待迟到 callback，native drain 后保留 catalog 发布 NativeSettled
  才关闭存储。stale/nonfresh 不 drain、不设置 stopping；结果写入失败保留可查 catalog，
  重启 pending 仅 Interrupted。上述为可信 native callback 测试，不是生产授权 coordinator。
  最新另有 **12 项原生 facade** 测试并额外三轮（每轮 9 + 3）通过：可信 scope/Agent/动作
  grants 早于 lookup/副作用，bounded owned jobs、丢 waiter/背压、历史先于新 profile、
  reservation 内重证关闭去重竞态、checkpoint 不给 fresh、部分创建与终态写失败不重放；
  真实 Harness 精确取消/迟到关闭、Stop 失败保留 owner/catalog 并允许无条件 shutdown 重试。
  startup 也在新注册前检查 saved facts，孤儿重启失败不再创建注册行。成功 Stop 仅缓存
  一条已严格同步的收据，供 catalog worker 关闭后对账，不证明 listener/进程退出。
  当前 daemon 已持有固定 authority/native handle，接通发现/收据及create/restore/close；
  稳定输入取消Native、wire与提交CLI已完成，管理 DLL E2E仍待实现。新增 **6 项 daemon + 1 项默认值**测试额外三轮通过：统一
  冻结/跨集合别名/权限预算在 catalog 前拒绝、动态不自动加载、旧版本收据、启动/bind
  失败结算；配置默认拒绝，未知 peer grant 字段拒绝。
  底层另有 tools exclusive-create **5 项**、
  JSONL 独占新建 **10 项**。前序 server 全套另重复三轮，
  每轮 11 通过；既有真实 HTTP/内置工具绑定测试亦包含在 Host 套件中。
- `cargo test -p rpi-cli --test host_control --locked --offline -j 2`：真实本工作树 rpi 二进制
  **27 passed, 0 ignored**，新增2个Stop+HTTP退出/重启原意图隔离，覆盖取消CLI丢stdout/18坏输入无连接/重启原文件对账、丢取消reply/HTTP Aborted/后续输入隔离与重启不改JSONL、gated HTTP准入早于完成、原输入冲突/不重放、14坏文件无连接、真实行政CLI/HTTP、13类坏输入不连接、None/Some默认拒绝、stdout丢失对账、查询/准确元数据、SIGTERM/SIGINT、重启 epoch、第二实例拒绝、
  配置权限/symlink/hardlink/未知字段、无 ambient 资源副作用、错误 token 不泄漏、ready EPIPE 清理。
  前序新增 2 项覆盖动态配置不自动加载/查询不读 profile、15 类私有行政配置前置拒绝；
  当轮额外三轮各 10 项通过。最新扩展 profile 发现校验，另新增 1 项实际 binary 两次重启查询
  真实 pending→Interrupted，原请求/旧 epoch 完整保留且不创建 Session/注册 Agent。
- extension 的 `shared_host_daemon` 必需产物测试：**1 passed, 0 ignored**，真实 extcore client
  连接同一构建二进制，经两轮启动/退出验证无策略/默认拒绝策略、认证、capability、空 profile
  发现、未授权 Stop 收据查询、detach、重连与 epoch；不授予 Stop 执行权限。
  它默认 ignored，显式运行时没有“找不到就跳过”或默认 PATH fallback。
- extcore 本阶段 `shared_host::` 定向回归 **20 passed, 0 ignored**。前序完整回归
  **371 库 + 11 resource recovery passed，5 ignored**（本阶段未重跑）；原 4 项 fixture
  ignored 与新增 1 项显式 binary 测试都不算通过。Args **41**、启动诊断 **6** 通过。
- 两侧定向 Clippy 通过命令行豁免既有 lint；没有新增文件 warnings，没有源码级全局 suppression。
  全 workspace、Windows/Linux/cgroup 和性能探针未重跑。

真实 daemon + 真实原生 journal 的 found/negative receipt 查询另由 Host 库内
`daemon_ipc_receipt_reads_the_real_native_admission_without_mutating` 覆盖；它不包含
create/submit IPC，因此不构成完整管理 E2E。

跨仓库必需产物复现（在 extension feature worktree）：

```bash
RPI_SHARED_HOST_BINARY=/Users/bigfish/work/rpi-projects/pi-rust-wt-host/target/debug/rpi \
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-extcore --test shared_host_daemon \
  --locked --offline -j 2 -- --include-ignored
```

Startup profile、admission、绑定预检、行政/文本CLI与稳定取消/Stop及instruction资源后真实二进制套件为 **28 passed, 0 ignored**：
除本地 HTTP SSE Provider + IPC submit/receipt + 重启复用、SIGKILL 后 receipt 恢复且不重放，
新增13类无效profile（含6类资源拒绝及模糊模型、缺key、命令key、必需header、proxy、超长Session id、
超量工具）均在 Session/progress/writer 文件与 Agent 注册前拒绝，不发布 readiness/listener。
二进制前置拒绝与只读 profile 独立环境两项各额外重复三轮通过。
extcore 真实 daemon 仍 **1 passed, 0 ignored**；覆盖 Some 默认拒绝五项执行与 None 本地
Unsupported。本轮额外三轮每轮 **223 Host + 28 binary passed, 0 ignored**；前轮另有
每轮3真实行政IPC+1客户端+13 binary通过。阶段45完整验收 debug 二进制 SHA-256：
`b752e19b4cdceb64cfc2c14d15246eba6305c2942231476078b0c6b5e7f05fb2`。

该增量新增 Agent 级 `receipt { agent_id, request_id }` 查询与
`submit { agent_id, request_id, prompt }`，公布 `receipts`/`submit`。submit 回复
`wait_admitted` 的收据：同 request ID 重试返回同一收据且不重复执行；执行在回复后继续，
因此回执不是运行结果。未知 request `found=false`，未加载 Agent `NotFound`，同步不确定
`PersistenceUncertain`。查询返回已存在 receipt 前也会重证严格同步，但不追加 transcript、
不启动/重放执行；同步失败时不把 entry 可见性当作持久证明。重复 submit 经相同的 native
严格同步检查，即使原 run 仍活跃也不跳过。独立完整意图 create/restore/close/stable-input cancel
及独立Stop已开放，legacy ticket Cancel/裸Stop占位仍关闭。连接整体期限覆盖
admission 等待；超时断开连接时客户端得到 OutcomeUnknown，必须查询 receipt。
改动未提交；没有安装、发布、合并、启动生产服务或替换原 checkout。测试创建的 daemon 均已退出。

这些证据是**控制链路**（含真实二进制 profile 启动、真实 HTTP Provider 执行、IPC submit/receipt 与重启复用）：
仍未包含管理扩展 DLL 或真正 mini；native/IPC 下的动态create/close/restore与稳定取消
已有独立真实binary+HTTP覆盖，但不作为管理DLL三方贯通。
此前 DLL hook 隔离和 HTTP Provider 测试是独立证据，不拼成虚假的完整 E2E。
