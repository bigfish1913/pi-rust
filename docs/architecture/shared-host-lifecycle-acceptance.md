# Shared Host 动态生命周期验收矩阵（阶段42审计，阶段46进行中）

> **收敛说明**：原阶段46保留但暂停扩展；A 批完成私有启动策略，B1 提取共享内存成员
> 表/生命周期状态并让既有服务复用。B2a 原生适配、B2b cleanup 屏障及 B3a 关闭后运行绑定
> retirement 已测，B3b1 补正常 CLI prepare/reload 全代际（含失败加载）的不持库收据观察，
> 不改 SDK ABI，也不等于最终资源结算。B3b2 增加 CLI 插件绑定 owner、串行 reload 门和
> TUI task joins，已测实际 DLL/Harness/物理 writer 与资源 lease；其关闭仍是 staged 子集，
> 工具进程等全部 lease 使用者仍须结算。B3b3 为 native child/side-question 增加 owned
> 准备、任务及最终 join，不证明 OS 工具/后台后代退出。Host/binary 通过，完整 CLI 重跑
> 仍有 4 项 codesign 失败；首跑另一次 fork 断言失败未在定向/完整重跑复现，原因尚未
> 确定，原日志保留。未改/跳过相关测试，默认 CLI Host 尚未装配；
> B3c1 历史增加 Darwin 原始组的独占 wait/WNOWAIT、正向 zombie 快照及实际退出/reap 托管，
> 失败/Drop 保留真实 guard；7 项进程测试、Host 275、重建 binary 31 通过。尚未接工具
> 环境，不覆盖逃逸/外来 reaper/父死，不等于完整 HarnessCleanup。B3c2 仅修 dev staging
> 的 owner 命名空间：新 UUID 与独占 mkdir，碰撞/符号链接/旧目录不采用、不覆盖；真实
> metadata 双实例和单实例 cleanup 隔离通过，dev 8、Host 279、重建 binary 31 通过。
> watcher/延迟 helper/Cargo 后台后代仍未托管，rebuild 保持同步。SDK/完整 CLI 本批未重跑，
> B3b3 的原有失败和 fork 原因未确定记录不因此消除。
> 这些测试不证明 daemon、整组恢复或父死清理已经完成。
> 最新结果见 [Host README](../../crates/rpi-cli/src/host/README.md)；下文矩阵和 SHA 是
> 原阶段历史证据，#23、#46及三方 DLL E2E 不因此标为完成。

## 结论与边界

完整意图 create/restore/cancel/close/stop 的 Native coordinator、独立 IPC 与原文件 CLI 已接通。
阶段42补齐真实 binary 的“关闭失败保留 custody → shutdown 失败不退出 → 显式重试”证据，
不修改生产代码、wire v1.4、固定 authority 或失败重放策略。

**#23 仍不标完成**：迟到 `AfterToolCall` callback 持 writer/owner 的实际场景已在真实
Harness + JSONL + Native IPC 中验证，但尚未在实际 rpi binary 内由受信扩展 callback 重现。
当前 Host 绑定仍拒绝未支持的扩展/资源，不能用不同测试拼成这项 binary 证据，更不能拼成
“真实 binary + 管理 DLL + HTTP Provider”三方 E2E。后者仍属于长期 #14 的最终验收要求。

这里的动态 load/restore 指**已经登记的普通 Agent**，用受信 profile 与原 revision 经
`RestoreProfile` 严格读取准确 Session，不是从任意路径导入孤儿/未登记历史，也不是隐式恢复
执行。没有另行宣称裸 `load` 命令或任意 Session 导入能力；CreateProfile 必须 exclusive-create，
发现既有文件不会改成接管/覆盖。mini、资源审批、定时器、执行 TUI 与管理扩展仍未完成。

## 逐项证据

| #23 条目 | 已有证据与实际层级 | 尚未证明的内容 |
| --- | --- | --- |
| Session 创建不覆盖 | [exclusive create 测试](../../crates/rpi-harness/src/session/jsonl/exclusive_create_tests.rs)、[OS 文件测试](../../crates/rpi-tools/tests/exclusive_create.rs)；binary 创建前拒绝/重启测试 | 不因此提供孤儿历史自动接管 |
| 固定权限/闭合意图/唯一 fresh | [协议](../../crates/rpi-cli/src/host/protocol.rs)、[Native coordinator](../../crates/rpi-cli/src/host/administration_tests.rs)、[真实 binary CLI 拒绝矩阵](../../crates/rpi-cli/tests/host_control/administrative_cli.rs) | capability 不是 grant，不是 scoped tool approval 或 sandbox |
| 动态 create → HTTP → close → restore → HTTP | [真实 binary 动态执行](../../crates/rpi-cli/tests/host_control/administrative_execution.rs)、[真实 CLI 执行](../../crates/rpi-cli/tests/host_control/administrative_cli.rs) | 不证明扩展资源绑定已接入 |
| 稳定目标运行取消及后续输入隔离 | [真实 binary + 两次 HTTP](../../crates/rpi-cli/tests/host_control/cancellation.rs)，含原文件/丢 stdout/重启对账 | CancelSignalled 不证明结算，不从历史复活目标 |
| 迟到 callback 保持 writer/owner | [真实 Harness/JSONL + Native IPC](../../crates/rpi-cli/src/host/ordinary/administration_owner_tests.rs)，Stop waiter 超时后仍 Prepared/持 writer/驻留槽，释放 callback 后才退休 listener | **实际 binary 内扩展 callback 场景未覆盖** |
| 关闭失败与 shutdown 显式重试 | [本轮真实 binary + HTTP + SQLite 故障](../../crates/rpi-cli/tests/host_control/closing_failure.rs)，详见下节 | 不是提供运行时 SQL 修复接口；故障仅在独占测试 fixture 内注入/撤销 |
| 客户端断连不丢 owned custody | [Native IPC deadline](../../crates/rpi-cli/src/host/daemon_administration_execution_tests.rs)、[binary 丢 reply](../../crates/rpi-cli/tests/host_control/administrative_execution.rs)、[Stop 两种丢输出](../../crates/rpi-cli/tests/host_control/stopping.rs) | found=false 不证明以后无作用，不换 ID 重放 |
| 重启原意图对账、不误关新 owner | [取消](../../crates/rpi-cli/tests/host_control/cancellation.rs)、[Stop](../../crates/rpi-cli/tests/host_control/stopping.rs)、[本轮失败 close](../../crates/rpi-cli/tests/host_control/closing_failure.rs) | 历史成功/失败均不是现在的运行状态证明 |
| Stop 回复/停接入/退出顺序 | [Native IPC](../../crates/rpi-cli/src/host/daemon_stop_tests.rs)、[真实 binary + HTTP](../../crates/rpi-cli/tests/host_control/stopping.rs) | NativeSettled/本地 flush 不证明 peer 收到；进程退出由测试的实际 wait 单独观察 |

## 本轮新增真实进程故障场景

`actual_binary_close_failure_retains_owner_and_shutdown_retries_only_on_explicit_signal`：

1. 实际 rpi binary 加载普通 Agent，submit 触发本地 HTTP 请求；Provider 故意不回 headers。
2. 在测试自己的 catalog 注入最终 `resident=0` 发布失败。原文件 CLI close 返回3/Uncertain，
   原文件 receipt 与重复 close 保留同一结果，不重放。owner/resident 保留1；JSONL 有 Aborted
   和唯一 admission，物理 writer 已实际释放。这两种 custody/物理事实分别验证，不混为一谈。
3. 首次 SIGTERM 的无条件 shutdown 仍失败；等待明确失败诊断后确认进程、socket、查询入口、
   owner/resident 都保留，且没有自动重试/诊断忙循环。未泄漏 key、token 或原始注入错误。
4. 撤销测试注入后，先确认仍未自动释放 owner；第二次显式 SIGTERM 才成功结算并退出0。
5. 同一配置重启新的 Live owner，旧失败意图仍返回同一 Uncertain；resident/owner 为1，
   不误关新 owner、不请求额外 HTTP、不改变原请求文件或 Session JSONL。

## 阶段46进行中：实际DLL加载的custody与cleanup receipt（尚未接Host profile）

- [严格owned loader](../../crates/rpi-extensions/src/loader.rs) 新增独立显式入口：完整集合先验证
  绝对常规文件、预算和canonical别名，再按顺序加载；不扫描、不skip、不接ActionBridge。
  任一失败不返回可执行session；不可执行custody持有此前成功DLL、缺入口符号的已打开DLL及
  注册失败DLL。后者即使返回非零也可能已有SDK workers，不能直接回滚Host驻留预占。
- [cleanup receipt](../../crates/rpi-extensions/src/cleanup.rs) 不保活DLL；观察Pending/Released/
  Retained。必须等SDK shutdown成功、显式library.close成功、API及host guard销毁后才发布
  Released；线程创建/SDK finish/OS close失败或reporter丢失都不能作为release证明。取消waiter
  不取消清理。只证明本次load的handle/context已释放，不证明其它OS loader handle不存在。
- [7项实际编译DLL测试](../../crates/rpi-extensions/src/loader/tests/owned_tests.rs) 验证先成功后
  注册失败/打开失败/符号失败的整批custody、不启动后续DLL、全路径先检、最后callback
  keepalive释放后cleanup仍门控、guard析构返回前不得Released，以及成功/失败注册下SDK
  finish失败均Retained（隔离子进程验证，避免遗留故意保留的DLL）。另3项receipt测试覆盖
  reporter丢失、终态发布与sender关闭竞争、waiter取消。
- [Host具体custody](../../crates/rpi-cli/src/host/extension_custody.rs) 已接可信Rust binding的
  owned assembly、StartFailed及关闭路径；只claim一次，实际FFI worker持session/call scope，
  封门后等真实worker返回再Drop session并等receipts。Retained/未知不回滚Closed，保留
  owner/resident及已取得writer；StartFailed重试先预占Closing，不能误关新load。最后Drop
  无释放证明时保留writer/catalog单实例锁至进程退出。不是profile/IPC grant。
- [冻结artifact](../../crates/rpi-cli/src/host/extension_artifacts.rs) 只读捕获路径、完整bytes、
  SHA及顺序；至多8个DLL、单64 MiB/总128 MiB，拒绝canonical/hardlink别名。owned阶段
  在显式private owned根目录原子创建每Agent/每次装配独立的0700 staging，新文件只读，
  不加载可变原路径、不复用inode或ambient TMPDIR。SDK guard持staging直到Released，
  Retained时保留；这不是sandbox、持久化manifest或native code授权。
- [7项真实Host DLL场景](../../crates/rpi-cli/src/host/ordinary/extension_custody_tests.rs) 使用
  真实Harness/Faux Provider，覆盖纯预检、register73失败、commit后Idle发布失败、实际
  AfterToolCall callback和SDK finish双屏障、放弃等待、finish7下三模式Retained、跨Agent
  custody复用拒绝，以及同一artifact双Agent的独立真实静态runtime和逐个关闭。另
  [5项artifact测试](../../crates/rpi-cli/src/host/extension_artifacts/tests.rs) 覆盖冻结/替换/删除、
  有界读取、别名、显式私有根、权限/新inode/最终guard释放。不是binary/DLL联合验收。
- 当前235 Host、28实际binary、255 extensions功能（仅排除既有benchmark）、4显式DLL
  smoke、8 resource、69旧CLI Session、20 extcore合同与1当前binary显式daemon artifact
  通过，均0ignored；额外各三轮235 Host与28 binary通过。Clippy保持55/97既有warnings，
  沿用既有 `-A clippy::not_unsafe_ptr_arg_deref`，未修改actions/lint配置。并行SDK/Clippy
  压力下曾有既有300ms Stop排空测试Connection失败，未改时限，后续单独三轮全通过；
  `host-dll-custody-repeat-concurrent-failure.log`保留。staging首次错误将tempfile默认目录
  误视为0700，已改为显式原子0700，失败另存`host-dll-staging-initial-failure.log`。
  当前binary SHA256：`9aabb4a285b8add64cc636e4afbccf3138c0873a744d2621968e094b15e2ffe5`；
  stub DLL SHA仍为下述阶段45值。当前日志 `/tmp/rpi-host-integration-checks/host-dll-custody-*`。
- **未完成项不收口**：Host profile仍拒DLL。准确inventory、独立artifact grant、unsupported
  能力拒绝、生产逐Agent Provider/tool-result hooks及保存绑定尚待接通。初始受限路径不得
  暗中授权插件工具、ActionBridge/UI、自定义Provider或resource discovery。仍须实际binary
  内迟到AfterToolCall与装配失败/关闭/重启；#46仍in_progress，#23及binary+管理DLL+HTTP
  三方E2E仍未完成。Host Rust/Faux+DLL与独立binary HTTP证据不能相加冒充。

## 阶段45最近完整验收：显式 instruction 资源的持久装配

- [冻结资源](../../crates/rpi-cli/src/host/instruction_resources.rs) 沿用 ResourceSelection v1，
  只接受32个以内绝对文件、合计64 KiB UTF-8；只允许准确 `only` 或空工具 `none`，仍依赖
  Host approvedTools。package/skills/DLL/default/discovery 拒绝，身份与声明不能当作 grant。
- [profile 测试](../../crates/rpi-cli/src/host/profile.rs) 验证内容冻结、重启身份改变及不支持
  权限/路径/编码/预算拒绝；[2项真实JSONL同步测试](../../crates/rpi-cli/src/host/ordinary/instruction_resource_tests.rs)
  验证 readonly 不写不sync、不取writer，首次sync门控时保Starting/resident/writer，waiter
  abort不撤销装配；失败后可见绑定必须重sync、不重复append，坏schema+torn尾在repair前拒绝。
  等待条件使用 Service Live，不把先到的 catalog Idle 当作发布完成；初次测试竞态日志保留。
- [实际binary+HTTP](../../crates/rpi-cli/tests/host_control/instruction_resources.rs) 验证资源native
  value先于admission/Provider，HTTP收到冻结原文和原样 `{{keep_this}}`，AGENTS.md不发现；
  运行中源文件变更不影响快照，新快照重启拒绝且JSONL不变，原文件恢复后可重开，原输入
  不重放。HTTP监听保留到所有重启/退出完成，任何额外请求均失败。
- 最终223 Host、28 binary、8 resource selection、69 CLI session、20 extcore、1显式daemon
  artifact、245 extensions功能（排除1个既有benchmark）、4显式DLL smoke全通过、0ignored。
  最终额外三轮每轮223+28通过；Core/extcore Clippy通过，core保持55/97既有warnings。
- binary SHA256：`b752e19b4cdceb64cfc2c14d15246eba6305c2942231476078b0c6b5e7f05fb2`；
  DLL SHA256：`7cfa946238548c442702fdc48d1971301f41b61631ab8d9631ae210486ed7770`。
  日志 `/tmp/rpi-host-integration-checks/instruction-*`。本轮重新构建缺失的显式stub DLL后必需
  artifact 4项通过，没有以skip计验收。**没有开放Host DLL装配，#23和管理DLL三方E2E仍未完成。**

## 阶段44历史：纯预检与owned装配边界

- [OrdinaryBinding](../../crates/rpi-cli/src/host/ordinary/mod.rs) 新增deferred接口：纯validator
  看准确facts并返回未poll的异步assembly；[supervisor](../../crates/rpi-cli/src/host/ordinary/supervisor.rs)
  只在actual owned restore中poll，再校验完整model/provider/tools/drive，之后才commit_strict。
  preflight不poll计划，旧immediate API维持原有语义/分配/校验次数。纯validator是可信实现合同，
  不是Rust类型能自动证明的无副作用性质，不能把DLL注册直接放进validator。
- [4项真实Harness/JSONL Native测试](../../crates/rpi-cli/src/host/ordinary/binding_plan_tests.rs)：
  预检/陈旧guard不装配；validator拒绝/恐慌、assembly拒绝/恐慌、错model/tools/manual drive
  共7类失败均不修复torn源、writer仍可用；Service在waiter取消后持Starting/resident，
  assembly等待期间无writer，释放后Live/持真实writer；成功预检后修改header身份，restore重新拒绝。
- 阶段44历史220 Host、27 binary、20 extcore、1显式daemon artifact、245 extensions功能（排除1个既有
  benchmark）、4显式DLL smoke通过，全0ignored；额外三轮每轮220+27。Clippy/fmt/合同通过。
- 阶段44历史binary SHA256：`934a5accb347088d0c34f2d34f046ed964043e9522b7b26e05fceae8455743a9`；日志`/tmp/rpi-host-integration-checks/binding-plan-*`。
  尚未放开DLL/profile资源输入，更没有实现#23的actual binary迟到callback验收；#23保持未完成。

## 阶段43历史：工具身份前置（不扩大生命周期验收）

- [ToolCallContext](../../crates/rpi-extensions/src/tool.rs) 新增 `for_agent`，冻结逐Agent
  cwd/agentId/sessionId，显式None序列化成null，CLI late setter不能重绑；旧CLI保持原语义。
- [3个unit](../../crates/rpi-extensions/src/tool/context_tests.rs) 验证冻结/伪造覆盖/参数副本，
  同一registry两个ABI driver均让两普通Agent与一个Session-free身份在真实FFI调用内重叠。
- [显式实际DLL工具测试](../../crates/rpi-extensions/tests/plugin_stub_smoke.rs) 验证8个并发
  adapter共享同一真实DLL/registry，其中4个无Session；fixture只在probeContext请求时回显
  已注入元数据，普通echo结果不变。**这不是Host binary里的扩展callback或管理DLL联合E2E。**
- 当前Host216、binary27、CLI session69、extcore20、显式daemon artifact1、extensions功能245
  （排除1个既有benchmark）、显式DLL smoke4、stub unit4，均0ignored；额外三轮每轮
  216 Host+27 binary+3身份unit+4显式DLL smoke。Clippy/fmt/合同检查通过。
- 阶段43历史binary SHA256：`ef847515c9eeb7885659f5404bbb77b32114dfce99924b107f80fd0c4edd626b`；阶段42下面的SHA是历史，不再是当前产物。
  新日志：`/tmp/rpi-host-integration-checks/tool-context-*`。#23依然未完成。

## 阶段42历史验收

- Host **216 passed**；binary **27 passed**；extcore 精确 `shared_host::` **20 passed**；
  显式最终 binary 的 extcore artifact **1 passed**。全部 **0 ignored**。
- 额外三轮每轮 **216 Host + 27 binary passed, 0 ignored**。
- Core/extcore Clippy 通过，沿用既有 `-A clippy::not_unsafe_ptr_arg_deref`，无新增本阶段诊断。
  fmt、双方 whitespace、四合同逐字一致与测试进程清理均通过。
- 生产代码与 binary 未改变，SHA256 仍为
  `d24782de1e7dea1e73caa4c12ea4f540aa9a0b9fc3ef84c4d2c220a4f346b005`。
- 本轮日志：`/tmp/rpi-host-integration-checks/lifecycle-audit-*`。
  未提交、安装、部署、合并或发布；没有清理用户数据。

下一步是受信的逐 Agent 扩展/资源绑定前置，为实际 binary callback 与最终管理 DLL 联合验收
提供真正的生产装配路径；#23 的剩余证据不以 Native 测试代替，保持未完成状态。
