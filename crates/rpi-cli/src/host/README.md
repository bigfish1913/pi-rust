# 内置 Host 模块

在 `rpi-cli` 内按文件夹划分代码边界，不新增 crate，不引入额外服务程序。
模块间通过明确类型交互，不以 CLI 进程全局状态装配并发 Agent。

## Mini 显式配置与 raw Agent 装配（#66 进行中，生产 owner/wire 未接通）

- profile 新增 `agent.mode`：缺省 `persistent` 仍必须提供 Session；显式 `mini` 不接受
  Session 引用。Mini 尚未接入唯一 service owner/volatile engine，daemon 在 catalog 或
  任一先行 persistent Session 创建前 fail closed；dynamic-only Mini 也不能隐式加载。
- `NativeBindingProfile` 已复用闭合 Provider/model、工具授权与环境校验，直接构建原
  raw Agent；真实 async HTTP、冻结 prompt、真实 bash 工具结果往返已通过，不创建
  Harness/AgentLane/内存 Session/JSONL/writer。DLL loader/callback 装配复用既有 custody，
  本轮不称已验证 Mini DLL、late callback/finish 或完整生产生命周期。
- 定向 **23/0/0**；未过滤 CLI 两线程 **1544/0/3**；ordinary binary 两线程 **39/0/0**；
  CLI lib/bins/whole host_control scoped strict Clippy exit 0。不是 Mini binary＋管理 DLL E2E。
- [实现与验证边界](../../../../docs/host-mini-raw-binding.md) /
  [机器证据](../../../../docs/host-mini-raw-binding-validation.json)。接下来仍需唯一 owner 表、
  epoch/owner 易失回执、wire/管理 DLL 与重启 Lost 不重放验收；#14/#65/#66 均未结项。

## 真正无 Session mini 的原生 Agent hooks 前置（历史阶段，生产 mini 当时未接通）

- 不能把 Harness 默认的内存 Session 冒充无 Session。原 AgentBuilder 已增加三个
  增量 setter，使用私有可选 callback 表原样透传既有 loop hooks；不改 AgentOptions
  字段、原循环/AgentLane、SDK C ABI 或事件布局。Provider request 开启仍由 factory 负责。
- 最新 #14 前置拆分了 Host 冻结 resources 与 Harness options 的耦合：同一 instruction prompt/provenance
  可直接配置而无需构造 `AgentHarnessOptions`；native DLL 仍使用原 `NativeExtensionCustody`
  loader，按原 inventory staging，并可产出已有 AgentLoop callbacks。持久路径三项定向测试、
  原完整 `host_control` binary **39/0/0** 和 CLI lib/bins/whole host_control strict Clippy 通过。
  此为 raw Agent 装配前置；mini 尚未 profile 化、注册/监督、接 daemon 或 wire，不能称生产 mini。
- [最新装配前置报告](../../../../docs/host-mini-native-binding-prep.md) /
  [机器证据](../../../../docs/host-mini-native-binding-prep-validation.json)。
- 新增 raw Agent current-thread 回归 3 项；本轮未过滤 Agent **19 套/123/0/0**、
  Harness **546/0/4**、CLI 两线程 **1538/0/3**、ordinary binary 全套 **39/0/0**；
  CLI lib/bins/整个 host_control 严格 no-deps Clippy exit 0。
- Agent strict Clippy **仍 exit 101**（lib 4 类、lib test 5 类既有位置诊断）；
  普通 Clippy 还有原 common helper warning，不将其说成 strict 通过。
  #65 的 strict 条件未满足，#14、生产 mini 和整体薄 Host/daemon/恢复仍未完成。
- [阶段报告](../../../../docs/host-mini-agent-hooks.md) /
  [机器证据](../../../../docs/host-mini-agent-hooks-validation-final.json)。以下 #23/#46/#64 为
  各自阶段历史快照，旧源码/CLI SHA 与门禁描述保持原样，不与本轮新 artifact 混用。

## 受信 profile 动态生命周期收口（#23）

- 新增同一真实 binary+C DLL+HTTP 场景的两种回调变体：动态 Create → Submit →
  DLL 回调中 `CancelSubmitted` → Close durable Prepared → 关闭调用方 socket 真正断开 →
  callback/finish 期间仍 Prepared 且保 owner/resident/实际 writer → 真正 Closed 收据 →
  重启后不自动加载 → 新独立 Restore → 原 Create/Cancel/Close/Restore/Submit 对账不重放。
- 收据完整 request identity 逐项保持，CancelSignalled 不是结算，关闭 peer 的 future
  abort 后实际 join 完成。新连接通过原意图观察结算，不靠丢失的 ACK 声称成功。
- 当前完整 binary **39/0/0**（默认测试并行），未过滤 CLI lib **1538/0/3**（两线程），
  核心 lib/bins **及整个 host_control 集成测试**严格 no-deps Clippy 均通过。
  生产 Rust/合同/Cargo 锁未改，CLI SHA 与上一阶段相同；没有将组件测试拼作新 E2E。
- [本阶段证据与完整生命周期矩阵](../../../../docs/host-native-close-disconnect.md) /
  [机器证据](../../../../docs/host-native-close-disconnect-validation.json)。#23 受控动态合同
  完成；不开放任意 load/legacy 裸命令，不称管理扩展 DLL 参与了本次原生 hook 场景，
  不称所有命令×hook 的交叉矩阵均实测。#14、默认薄 Host/daemon/整组恢复仍未完成。
  SDK 15 项严格 lint 与默认并行 CLI 的既有失败仍保留，没有宣布 workspace 全绿。

## 受限生产 hooks 收口（#46）与配额夹具稳定化（#64，阶段历史验收）

- #46 的显式冻结/staging、精确 inventory、tool-result 与三个 Provider observer、
  逐 Agent 上下文、真实 late callback/finish/retained 所有权已完成受限合同验收。
  实际 binary+C DLL+HTTP 全套 **38/38**；不是开放 ActionBridge/UI/插件工具或任意事件。
- 扩大门禁的 Agent 配额测试已仅调整私有结果形状：按原生节点保守计费的结构化结果，
  编码仍 >1 MiB 且 <2 MiB，一份可保留、两份不能共存于原 64 MiB 共享树预算。
  原三秒 poll、生产代码、外部 reference 拒绝/释放再准入/usage 断言保持；另新增大小界限。
  隔离三次通过，最终未过滤 Agent 库+全部集成 **18 套/120 passed/0 failed/0 ignored**。
- #46 其余实际门禁继承未变化生产源码的上一阶段：未过滤 CLI 两线程 **1538/0/3**、
  Harness **546/0/4**、AI/SDK **63/0/0、259/0/1**，核心 lib/bins 严格 Clippy 通过。
  默认并行 CLI 的 TUI fork/saved-read deadline 与 SDK 严格 Clippy 的 15 项诊断仍记录，
  本次没有声称修复，亦不称 workspace 全绿。没有重跑内存 benchmark。
- [收口与夹具报告](../../../../docs/host-hook-closure.md) /
  [机器证据](../../../../docs/host-hook-closure-validation.json)。以下两段为按先后保留的
  历史快照，其中“#46 未完成/Agent gate 失败”描述当时状态；旧报告文件不改写。
  #14/#23、默认薄 Host、daemon/整组恢复和全域受控资源清理仍需独立推进。

## 生产 Provider observer：可等待的兼容边界（#46 后续阶段历史验收）

- `ProviderHooks` 增加两个带默认适配的 async 入口；同步实现/SDK ABI 不变。Harness
  的既有 before-request 调用与 Agent loop 的两个既有 terminal 调用点改为 await，
  不重写执行循环或引入旁路事件总线。同步 prompt transformer 在此仍不获批准。
- 私有 `providerEvents` 只接受精确 `beforeRequest/beforeHeaders/afterResponse` 及
  1–16 count，重复/未知事件拒绝；转成有序 SDK inventory 后仍做完整 equality。
- Host 使用逐 Agent Weak-owner bridge 和真实 blocking worker 的 call scope；串行
  guard 移入实际 worker，取消 opening 的等待者不会让 Aborted observer 越过迟到的
  before-request callback。SDK 自带显式 Agent/Session context，无 ambient fallback。
- 真实 binary 全套 **38/38**：含双 Agent 两轮 Provider 事件身份、HTTP transform，
  以及取消/成功/HTTP 400 的迟到 Provider callback→finish→writer 释放顺序与 IPC 可用性。
  CLI 未过滤全库以 `--test-threads=2` **1538/0/3**；Harness **546/0/4**；
  AI/SDK 库分别 **63/0/0** 与 **259/0/1**；核心严格 Clippy 通过。
- 不称全部门禁全绿：默认并行 CLI 出现旧 TUI fork 与 saved-read deadline 失败；
  Agent 全套在默认/两线程及隔离运行均触发未修改 ToolProgram 大结果嵌套配额测试的
  三秒 poll deadline（栈指向等待整个 nested 子调用）。没有跳过/修改该测试，亦未执行
  before-source 全套来证明基线。SDK 独立严格 Clippy 仍 15 项；#46 整体暂不标完成。
- [后续阶段报告](../../../../docs/host-provider-hooks.md) /
  [机器证据](../../../../docs/host-provider-hooks-validation.json)。下文首阶段与旧阶段保留
  为历史快照；其中“只支持 tool-result/Provider 未接通”不代表本段的当前进度。

## 生产受信 DLL：首个 tool-result 路径（#46 首阶段历史验收）

- 私有 Agent profile 可显式配置 `resources.extensions` 与 `nativeHooks`，批准有序
  DLL 批次、私有 staging root 和每个 DLL 的精确 `toolResult` 注册数。只读冻结/预检
  不执行注册；真正装配才走既有 owned strict loader，整批失败仍结算已启动的原生 worker。
- 当前只支持 tool-result transform；Provider observer、ActionBridge/UI/插件工具及其他
  未支持注册一律拒绝，不经 CLI ambient discovery 补齐。受信原生代码不是 OS 沙箱。
  现有同步 Provider hook 不能直接搬进共享异步执行器；该缺口仍属 #46，不标整体完成。
- `AfterToolCall` 的实际 blocking worker 持有 custody call scope，逐 Agent 的冻结
  `cwd/agentId/sessionId` 仅放入插件 envelope 的 `__rpi`；取消等待不提前卸载代码。
  SDK finish 失败保留 owner/resident、已取得的 writer 与 staging，不伪造 Closed/unmap。
- 新增实际 binary + C DLL + HTTP 验收：双 Agent transform/上下文隔离、保存 inventory
  不符在注册前拒绝、冻结后替换源文件、整批注册失败及 metadata/count 拒绝、迟到 callback
  与 finish 期间保 writer、retained 的加载失败/关闭和重启原请求不重放。
- 本阶段最终：实际 binary **37/37**、未过滤 CLI **1538 passed / 0 failed / 3 ignored**、
  SDK **259 passed / 0 failed / 1 ignored**；核心 lib/bins 严格 no-deps Clippy 通过。
  SDK 单独严格 Clippy 仍有 **15** 项诊断，不能称 SDK/workspace lint 全绿。
- 四项 prebuilt codesign 失败已由真实已签名 Mach-O 测试夹具修复（#62）；全库复跑中另
  观察到一次未改 TUI fork 测试失败和一次 dev-copy 字节断言失败。后者已定位为 cargoc
  重建改变 Mach-O install_name/UUID，并仅固定私有测试夹具的链接元数据（#63），原整文件
  字节比较保留。失败日志不覆盖；后续两次未过滤全库通过不等于证明 TUI 偶发根因已解决。
- [阶段报告](../../../../docs/host-native-hooks.md) / [机器证据](../../../../docs/host-native-hooks-validation.json)。
  下文 #56/#60 及更早计数保留为历史快照；默认 CLI Host、daemon/整组恢复与全域资源清理未由此完成。

## Dev preparation review 收口（#56）

- 接住既有初始准备 WIP：仅 `Arc::get_mut` 可证明无其他 strong/weak owner，且尚无
  build generation/current stage 时，才允许初始失败回收独占 namespace；共享或已 build
  实例拒绝此入口。Cargo/staging 原始错误保留，回收失败额外报告路径与原因。
- 补真实 DLL copy 失败验收，确认另一实例、legacy 目录及 Cargo artifact 不受影响；
  回收拒绝当时观察到的 file/symlink 替换，缺失路径幂等。不是目录身份或并发替换的
  OS 沙箱，也不用于已加载 DLL 的 retirement。
- Darwin `build.rs` 复用锁中已有 `cc = 1.4.7`，未新增 workspace crate/锁定包；实际
  Cargo CC/AR wrapper、多词 target-specific CC wrapper/AR、deployment target 与非 mac
  early-return 验证通过。非 mac 验收仅执行构建脚本分支，不是 Linux 交叉编译。
- 顶层帮助改为 `host <command>` / authenticated IPC；真实 binary fixture 同步已有
  wire v1.5 `results` 能力的精确断言，不新增 `rpi host result` CLI 子命令。
- 当前 dev **22**、Host **287**、真实 binary **32** 项通过，均零失败/忽略；库与 binary
  严格 Clippy、定点 fmt、四合同逐字及双树保全审计通过。未过滤全库为
  **1533 passed / 4 failed / 3 ignored**，仍是 #61 已记录的四项 codesign 夹具失败，
  未跳过或修改签名检查，不称 workspace 全绿。
- [本轮验收报告](../../../../docs/host-dev-preparation.md) 与
  [机器证据](../../../../docs/host-dev-preparation-validation.json) 独立保存；以下 #60 的
  55 项 lint 及各阶段计数是历史快照，#61 已清零的事实不通过改写旧记录表达。
- watcher/延迟 helper、Cargo/工具后代与父死清理、全代际 OS 资源、默认 CLI Host、
  真正 daemon/整组恢复以及 #46 生产 Host DLL 装配仍未由此完成。

## 可选管理扩展的原请求结果接口（#60，wire v1.5）

- 新增只读 `result { agent_id, request_id, run_id?, offset, max_bytes }` 与 `results`
  capability；无行政配置为 6 项能力，有行政配置为 13 项。旧命令语义不变，旧 peer
  没有 results 时不发送新命令；查询携 request_id 仍不是 mutation，丢回复为 QueryFailed。
- 只从原生 admission 关联的 run 经已有 `Harness::get_result` 稳定引用取结果，不读
  当前 Session tip、不复制 transcript、不新增执行 journal、不重放。终止结果读取后再
  `sync_history` 并核验 history 覆盖；不能以 admission 的 durable 冒充结果持久化。
- 区分 admitted/running/completed/aborted/failed/unknown；只有同请求/同原生活动 run
  且 owner 无 fault/retiring/closed 才报告 running。缺引用、悬挂、恢复或同步无法确认
  时为 unknown，不能从 finished=true 猜成功。
- 只导出最终 assistant Text：4–16384 UTF-8 bytes/页，非零 offset 必须固定原 run_id，
  拒绝非字符边界或越界。thinking/tool args/images/原始 Provider payload 不导出；
  错误 code/message 分别最多 128/4096 bytes。此命令是 IPC/管理工具接口，未新增
  `rpi host result` CLI 子命令，也不是完整事件/多模态平台。
- 当前 284 Host 单测、SDK 合同 22、管理包 19 通过；真实管理 DLL 33 次调用/6 次
  Provider 请求覆盖 9 项结果场景，含运行中取消和崩溃后 unknown/无重放。旧管理 smoke
  7/21 次、10 个错误/兼容性用例通过。核心严格 Clippy 仍被既有 55 项问题阻挡，
  没有扩大 lint 例外；不称 workspace 全绿。后台/默认 CLI/#56 WIP 均未由此完成。
- 产物、原始 native 事件、重启所需 fixture stale socket 清理与完整边界见
  [结果验收报告](/Users/bigfish/work/rpi-projects/rpi-extension-wt-host/packages/rpi-host/docs/result-interface.md)。
  后文各阶段计数、wire v1.4 与旧 artifact 为历史记录，未重跑/改写 20-Agent 内存证据。

## 薄 Host 收敛：配置与内存所有权核心

本轮目标是一个 Host 复用现有 Agent/Harness loop，默认一个、按需多个；增加配置化整组
恢复与真正后台 daemon，而不是成员接管、选举或运行现场迁移。既有行政管理、SDK/DLL
阶段代码全部保留，但不再作为默认 CLI 薄容器的前置扩展。

**A 批目前只接通配置解析、校验和启动前拒绝；后台运行、自动重启、整组恢复尚未接通。**
现有 `HostDaemon`/`serve` 仍是前台。以下为现有私有 Host 配置的新增字段片段，不是完整配置：

```json
{
  "mode": "foreground",
  "recovery": {
    "restore_on_start": false,
    "restart_on_crash": false
  }
}
```

- 省略这些字段保持旧行为。`mode` 仅接受字符串 `foreground`/`daemon`；`recovery` 必须是
  对象，字段必须是真正的 boolean；拒绝未知/重复字段、null、数组和拼写别名。
- `restart_on_crash: true` 只允许与 `mode: daemon` 组合。`restore_on_start: false` 不会
  删除 Session，也不阻止显式启动 profile 复用原 Session；它不自动恢复额外目标成员。
- 当前 `serve` 对 `daemon` 或 `restore_on_start: true` 明确报未实现，在读取 Agent profile、
  创建 catalog/Session/socket 或后台工作进程前拒绝，不静默退回前台或部分恢复。
- 查询可以读取合法的未来模式配置并联系**已运行**的 Host；它不会应用启动策略、读取
  startup profile 或创建后台进程。配置文件里的 mode 不证明远端进程实际以该模式运行。
- 这些字段仅属于私有启动配置，不加入公开 `HostConfig` 或控制协议，不改变四合同。
- 后续复用现有 catalog/Session 保存目标成员集合；未知工具副作用不自动重放，正常 Stop
  不触发自动重启。Unix 主进程被强杀后的工具后代退出仍须专门实现和实际验证。

### A 批验证记录（历史快照）

- A 批源码：Host **242 passed**；实际 `rpi` binary `host_control` **31 passed**，均 0 failed、0 ignored。
  包含 6 个新增策略/私有配置单元测试及 3 个新增真实 binary 测试；真实查询覆盖所有 6 个
  合法策略组合，非法类型/组合与未实现启动均不创建 Host 资源。
- Clippy（lib + host_control）通过，保留既有 lib 55 warnings，沿用已有
  `-A clippy::not_unsafe_ptr_arg_deref`；本批不改变 SDK、工具权限或四合同。
- core workspace fmt、双工作树 diff、四合同逐字及新增文件空白检查通过；extension 全
  workspace fmt 在本批未修改的 `packages/rpi-voice/src/pet.rs` 报格式差异，未代为改动。
  extcore 合同与 daemon artifact 测试文件的 scoped fmt 通过。
- 实际测试产物 `target/debug/rpi` SHA-256：
  `ba836ab348083710b7187a1b9a6eacd8b69d5a8745140b0e12f01f010f96bf50`。
- 日志：`/tmp/rpi-host-integration-checks/host-launch-policy-{host,binary,clippy}.log`。
  最初发现 Serde 会把 `recovery: []` 当作全缺省结构，已改为仅接受对象并保留重复字段检查；
  初始失败日志保留为 `host-launch-policy-initial-failure.log`，不放宽测试掩盖解析问题。
- 这不是新一轮 SDK/extcore/所有 CLI 模式全回归，也不证明后台启动、整组恢复或父死联动。

### B1：共享内存 owner 状态（历史快照）

- `runtime.rs` 持有唯一成员表、owner phase、事务/整组 stop 的同一公平读写门，以及
  stopping/closed 标志；创建它不需要 catalog、profile、socket、环境发现或 Tokio runtime。
  owner 与失败启动保留资源以泛型承载，核心不加载插件、不创建 Agent loop。
- `HostService` 复用这张表和这些状态，不再自己拥有第二套成员/锁/停止标志；原
  `service::OwnerPhase` 公共路径通过 re-export 保留。catalog、固定行政策略、实际
  Ordinary/Harness 取消、等待、drain、关闭和失败 custody 逻辑仍由既有适配层处理。
- B1 是状态提取，不是通用 actor/container 框架。默认 CLI 尚未接入；直接接纳无 catalog
  Harness 的生命周期适配仍待下一批。后台 daemon、整组恢复和父死清理仍未实现，A 批
  的启动前拒绝继续生效；core Drop 不冒充异步关闭。
- B1 源码：Host **247 passed**（新增 5 个无 catalog 核心测试）；实际 binary **31 passed**。
  核心测试覆盖共享表与独立 owner、观察者消失、失败启动资源、poison 保留与公平 stop 门。
  既有真实 Harness/DLL custody、关闭失败/重试、行政持久化及 binary Stop 测试均通过。
- Clippy（lib + tests）通过，既有 lib 55/lib-test 97 warnings，沿用既有 raw-pointer allowance；
  无 `runtime.rs`/新增核心测试警告。日志为
  `/tmp/rpi-host-integration-checks/host-runtime-{host,binary,clippy}.log`。
- 实际 `target/debug/rpi` SHA-256：
  `df64b94db8681bd4a6180fe07d0346dbe28a623def0b63064147f7fba99c5d32`。
  不以本批 core/Host 测试代替 SDK/extcore 全回归或未来默认 CLI/daemon 恢复联合 E2E。

### B2a：已构造 Harness 的私有原生接管适配（历史快照）

- `in_process.rs` 在同一个 `HostRuntime<ManagedHarness>` 表内接管独立、空闲的 Harness，
  不创建 catalog、不发现配置、不重写 Agent loop。重复 AgentId/Session（包括同一 Harness
  clone）拒绝，失败把原 Harness 返回调用者；客户端同时弱绑定具体 owner 实例，旧客户端
  不得在同 AgentId 换实例后重定向 prompt/cancel/诊断。
- 文本 prompt 是有所有者的调用，丢弃等待方不撤销执行；取消复用 Ordinary 的原生
  admission 前等待/精确 run abort 模式，不丢弃执行 future。单 Agent Busy 不串行其他 Agent。
- close 先退休并取消，再等真实调用/迟到 callback/idle/drain，最后关闭 Harness、retire writer，
  才从表移除。未保存内容/未知持久化/关闭失败保留 owner、writer 和 CloseFailed；诊断保留
  可读，不把失败清空成成功。整组 stop 只在所有成员结算后发布 closed。
- **这是原生 Harness/Session writer 适配，不是默认 CLI、资源 factory 或 DLL/进程清理集成。**
  外部进程/DLL cleanup 屏障尚未纳入，相关 binding 不得提前使用此适配或在 writer 释放后
  才清理；也不支持绕过客户端继续驱动 raw Harness。默认 CLI 接入与容量/装配策略留后续。
  私有模块暂时允许 dead_code，未增加公开 CLI/wire 能力；A 的未实现策略拒绝保持不变。
- B2a Host **253 passed**，其中新增 6 项真实 Harness/JSONL 测试：独立并发/取消隔离、重复
  身份、观察者消失、丢 prompt/close waiter 但迟到 callback 保 owner/真实 writer、unsaved
  关闭失败、stopping 退回未接管 Harness，以及旧客户端不重定向。实际 binary **31 passed**。
  这些是原生组件与既有 binary 回归，不是 B2a 默认 CLI、HTTP 或 DLL 联合 E2E。
- Clippy lib + tests 通过，既有 lib 55/lib-test 97 warnings；新增代码无警告。
  日志 `/tmp/rpi-host-integration-checks/host-in-process-{host,binary,clippy,core}.log`。
  首次编译的 fixture trait/Arc 类型问题另存 `host-in-process-initial-compile.log`。
- 实际 `target/debug/rpi` SHA-256：
  `e45d00208cd2b9f15561aac45fe52bbafbacdfb2188ba929354f0a004be651bd`。

### B2b：可信资源清理屏障与可重试分段关闭（历史快照）

- 原私有适配增加 `attach_with_cleanup`：借用可信、可重试的异步 cleanup 能力，接管成功才
  克隆并持有它；拒绝时原 Harness 返回且调用者仍持有 cleanup，不提前卸载资源。
  无外部工作时仍可用原生 `attach`。这不是可由模型、配置或 IPC 自行提供的授权。
- 关闭分段保存为 Open → HarnessClosed → ResourcesReleased → Complete。Harness 关闭后才
  等资源屏障，只有屏障证明实际 callback/worker 结算，才 drain、重新检查保存失败/unsaved、
  retire writer 并移除 owner。writer 仍声明被占用时也不得发布成功。
- cleanup 阻塞、错误或 panic 都保 owner、上下文与真实 writer；owned shutdown 不随 waiter
  消失。失败重试从 HarnessClosed 继续，不再次等待/关闭已经关闭的 Harness；资源成功后
  不重复 cleanup。cleanup 必须自行持有真实资源并提供可重试结算，不能把生命周期回执
  或“已发送停止请求”冒充释放，也不能捕获强 runtime 引用构成所有权环。
- **屏障接口已接入，但默认 CLI 与具体外部进程/DLL adapter 尚未装配。** 本轮检查发现
  `app.rs` 各模式各有 run 监督，`ReloadContext` 可持有重载后的插件/后台资源；现有生命周期
  dispatch 返回不自动证明后台工作已结算，不能简单在其后释放 writer。普通 CLI 行为未改，
  A 的 daemon/整组恢复未实现拒绝保持；后续必须先完成这条具体资源持有和结算路径。
- B2b Host **257 passed**，适配定向 **10 passed**（原 6 项 + 新增 4 项）：cleanup gate 下丢
  shutdown waiter，健康成员仍可关闭，而被阻塞者保上下文/实际 JSONL writer；cleanup error
  和 panic 后分段重试；拒绝接管不消费 cleanup；cleanup 成功但迟到 unsaved 仍拒绝释放。
  回调 gate 是测试夹具，Harness/Session/物理 writer 是真实实现，不冒充具体 DLL/进程 E2E。
- 实际 binary **31 passed**；Clippy lib + tests 通过，既有 lib 55/lib-test 97 warnings，新增代码
  无警告。core workspace fmt、双 worktree diff、四合同逐字和文档链接通过。
  日志 `/tmp/rpi-host-integration-checks/host-cleanup-{host,core,binary,clippy,fmt}.log`；
  `host-cleanup-sources.json` 记录验证源码快照（797 个 Rust/Cargo 文件）。
- 实际 `target/debug/rpi` SHA-256：
  `50258cbf883c01e65c23a1ad7a41a407ab43513cb80a169b4ee5090455069521`。
  这些是原生适配与旧 binary 回归，不是默认 CLI/HTTP/DLL 或 daemon 恢复联合验收。

### B3a：已关闭原生 Harness 的运行绑定 retirement（历史验收）

- 原生 [AgentHarness](../../../rpi-harness/src/agent_harness.rs) 增加显式
  `retire_runtime_bindings`：仅允许 closed 且无 active operation，在消息写门内验证并取走工具、
  Provider、转换器/投影器、hook/emitter 和执行环境/作用域引用，释放两把原生锁后才析构。
  已 sealed 的 exact-resource Harness 也能收尾，但不解除封闭状态或重新授予执行能力。
  原 `close()` 语义不变，不暗中清空 Session、writer、保存失败或 unsaved 恢复内容。
- 私有 Host 的关闭阶段现在为 Open → HarnessClosed → BindingsRetired → ResourcesReleased →
  Complete。先解除 Harness 自身对可执行绑定的持有，再等可信 cleanup 的实际结算证据；
  失败保阶段/owner/writer，重试不重复已完成的关闭/绑定释放；保存状态仍在最终释放前复查。
- 绑定 retirement **不是 DLL 卸载、子任务/后代进程退出或资源关闭证明**。其余调用/owner
  仍可能保有引用。具体资源 owner 必须独立保留 cleanup 能力，不能从已取走的绑定中临时
  找回；callback/析构也不能重入 Host 正持有的生命周期门。
- 已确认可复用现有 ExtensionSession 的 `begin_shutdown`/`cleanup_receipts`，最终 loader
  cleanup 在线程内完成 SDK shutdown、实际库 handle/context/guard 释放才报 Released；
  Retained/未知不能报成功。普通 CLI 的 ReloadContext、ActionBridge、Provider 及 reload
  代际仍需完整装配/撤销和收据收集；OS env 的 best-effort cleanup 不证明后台后代已退出。
  **本批没有切换默认 CLI，也没有扩展 SDK 装配或接通具体进程/DLL资源、daemon/整组恢复。**
- 新增 3 项原生定向测试通过：open/busy 拒绝；exact-resource 收尾、幂等、锁外重入析构和
  Session/unsaved/写失败保留；等待写门时取消不丢绑定。Harness lib **545 passed / 4 ignored**；
  4 项为原有 ignored benchmark，未运行。Host **257 passed**，其中强化原 cleanup gate 测试，
  真实工具绑定已解除，但被阻塞 cleanup 的上下文与实际 JSONL writer 仍保留。实际 binary
  **31 passed**。这些是原生组件与既有 binary 回归，不是新 CLI/DLL 联合 E2E。
- Harness/CLI 的 lib + tests Clippy 通过，保留既有 warnings，新增方法/测试无告警；core fmt、
  双 diff、四合同逐字、文档链接与源码稳定核验通过。
  日志 `/tmp/rpi-host-integration-checks/host-retirement-{core,harness,host,binary,clippy,fmt}.log`；
  `host-retirement-sources.json` 是 798 个 Rust/Cargo 文件快照。
- 实际 `target/debug/rpi` SHA-256：
  `8fdb51b9ff4275c3ba76c1cde6554adbdcddf758e2a238236576f865d36ddcd2`。

### B3b1：正常 CLI 启动与 reload 的全代际插件清理收据（历史验收）

- [现有加载器](../../../rpi-extensions/src/loader.rs) 在库打开后、符号查询/注册前，通过
  Rust `PluginDiagnostics` 的默认空观察方法报告不持库的 cleanup receipt。成功、缺入口符号、
  拒绝注册都可观察；open 本身失败不制造假库收据。旧加载顺序、跳过失败策略、诊断输出和
  SDK ABI 均未改变，不新增 SDK/FFI 能力或另一套装配平台。
- [正常 CLI prepare/reload](../session.rs) 共用逐 preparation 的
  [PluginCleanupLedger](cli_cleanup.rs)：包括失败候选和旧代际，不只依赖当前成功
  ExtensionSession。只压缩已 Released 的观察，最多保留 1024 项未结算证据；溢出或封闭后
  出现新的库观察为 sticky Unknown，不能把证据遗失当清理完成。等待取消不清空证据。
- ledger **不持库、不调用 shutdown，也尚未接入默认 CLI 的资源关闭屏障**。后续 owner
  仍须停 reload/watcher、释放 ActionBridge/Provider 等全部代际引用、等待实际 SDK cleanup，
  并独立证明工具环境/后台任务/进程结算。现有 OS env best-effort cleanup 仍不足；默认
  Host 接管、daemon、整组恢复与父死清理均未完成，A 批拒绝策略保留。
- 新增真实 DLL 观察测试 **3 passed**：成功/缺符号/拒绝注册（失败 session 没有成功收据
  仍可观察）；两代际持有、取消等待与重试；有界元数据故障注入及封闭后观察保守失败。
  SDK lib **258 passed / 1 ignored**，Host **260 passed**，实际 binary **31 passed**。
  这些不是默认 CLI + DLL + HTTP 的联合 E2E，也不证明独立 Agent 的 DLL static 隔离。
- 额外完整 CLI lib 回归并非全绿：默认 macOS TMPDIR 经 `/var` 符号链接触发路径拒绝及
  测试锁 poison；使用规范 `TMPDIR=/private/tmp` 后 **1496 passed / 4 failed / 3 ignored**。
  剩余 4 项位于未改的 prebuilt 测试路径：128-byte 伪对象交给本机 codesign，报格式不合适。
  未修改/跳过这些测试，未削弱路径或签名校验，未将完整 CLI 报为通过。
- SDK/CLI lib + tests Clippy 通过；CLI 既有 lib 55 / lib-test 97、SDK 14 / 28 warnings
  保留，新观察器/ledger 无告警。core fmt、双 diff、四合同逐字、链接与 800 源码快照通过。
  日志 `/tmp/rpi-host-integration-checks/host-cleanup-ledger-{core,sdk,host,binary,cli,cli-canonical,clippy,fmt}.log`；
  原失败与规范临时目录重跑日志均保留，快照 `host-cleanup-ledger-sources.json`。
- 实际 `target/debug/rpi` SHA-256：
  `bd24135f3d1d0e014123fe6b8bacd2c8fc9ed62098ac3cb9951f5b53aedbe1b4`。

### B3b2：现有 CLI 插件绑定的具体 retirement owner（历史验收）

- [CliPluginResources](cli_resources.rs) 复用现有 Session/ActionBridge/Provider cells、
  cleanup ledger 与实际资源 lease，不增加加载器或 SDK/FFI 能力。[CLI build](../session.rs)
  的 Harness lifetime hook 现在保留共享 owner 而非不可取走的 lease Arc：提前丢弃
  ReloadContext 仍不会释放活跃 Harness 的保护；显式 retirement 后，仍活着的外层 Harness
  只保留空容器，不再阻住物理资源 lease。
- reload 的整个候选准备（含 dev rebuild、拒绝候选）与 retirement 共用单一异步互斥门；
  候选之间也串行，避免匹配的 cells 交叉发布。封闭后拒绝新 reload。最终阶段在锁外
  invalidate bridge、begin SDK shutdown、释放当前 Provider/Session/Bridge，再等全代际
  实际 receipts Released；失败/取消保实际 leases，可重试，外部 keepalive 不算已释放。
- [TUI shutdown](../interactive_tui.rs) 对 tick、stream drain 和 reload bridge 的 abort
  现在都 await join，避免只发停止信号就返回、留下持有 Harness/ReloadContext 的任务。
- **关闭方法仍是 staged 子集，不是默认 CLI 的完整 cleanup**。调用前必须完成 Harness
  close/retire、frontend joins，且结算所有其他 lease 使用者（包括 native child agents）。
  它不证明工具进程、辅助 Agent/side-question、dev worker、detached task 已退出；SDK
  begin_shutdown/bridge cancel 也不是这些域的最终证明。外层 ResolvedModel/Provider refs
  必须退出自身作用域，否则 receipts 会正确继续等待。默认 Host 接管与后台恢复未开启。
- 新增 [资源 owner 测试](cli_resources/tests.rs) 2 项：候选串行/关闭屏障、封闭后拒绝；
  真实 DLL 外部 keepalive、等待取消时保物理资源 lease，实际 Released 后重试。
  [实际 DLL + Harness/JSONL 联测](in_process/tests/cli_resources.rs) 1 项：丢 close waiter
  不撤销 owned 关闭，DLL 未释放时 owner/writer/物理资源 lease 都保留，释放后两把物理锁
  均可重新获取，外层 Harness lifetime hook 仍在。不是默认 CLI/HTTP/DLL 三方 E2E。
- 最终 SDK lib **258 passed / 1 ignored**、Host **263 passed**、实际 binary **31 passed**；
  完整 CLI lib（规范 TMPDIR）**1499 passed / 4 failed / 3 ignored**，仅同样的 4 项
  prebuilt 伪对象 codesign 失败，未跳过或修改。CLI Clippy 通过，原 55 / 97 warnings
  保留，新 owner/测试无告警；core fmt、双 diff、四合同逐字与 803 源码快照通过。
  日志 `/tmp/rpi-host-integration-checks/host-cli-resources-{sdk,host,binary,cli,clippy,fmt}.log`；
  快照 `host-cli-resources-sources.json`，完整边界见 `host-cli-resources-validation.json`。
- 实际 `target/debug/rpi` SHA-256：
  `06b0970d1fcadb6d979197610c0a1ce7b5a5bcdf02bc4df88c007dc70c099d31`。

### B3b3：native 辅助 Agent 的 owned 准备与实际任务结算（历史验收）

- [CliTaskScope](cli_tasks.rs) 仅为现有 native 辅助服务提供 admission 与任务保管；复用
  tokio-util TaskTracker 的完整 future 析构保证，不新增 executor/Agent loop/SDK 平台。
  普通 admission 受封闭门保护，已接收准备可交接自身 continuation，持 token 防止结算
  出现空窗口。准备独立于回复 waiter，最多 32 项；跨 spawn 保留真实逐调用身份和取消
  task-local，不能靠离开原任务绕过 child delegation 限制。panic/非正常任务销毁或清理
  失败为 sticky Unknown，不能因 tracker 变空而报成功；取消 wait 不停止被保管任务。
- [ChildAgents](../child_agents.rs) 的准备与 runner 均纳入同域；不再丢失 constructor
  waiter 后就丢掉准备。worker 原有最终 close、panic fallback 与 execution_scope.wait_idle
  保留，dispose/revoked 从 roster 移除也不丢任务 custody。原父服务 Drop 仍发取消。
- [SideQuestions](../side_questions.rs) 的准备、串行 worker、parent watcher 和清队列
  close 全部跟踪；终止状态/移除 thread 不是最终结算。最终 seal 后取走并在锁外 retire
  registry threads，拒绝新调用，并等待所有 continuations、child scopes 与任务析构。
  HarnessActionHost 增加仅 Rust 私有的最终 shutdown/join，不改变 SDK ABI 或每轮取消语义。
- 新 [任务域测试](cli_tasks/tests.rs) **4 passed**（含慢析构、丢 waiter、冻结上下文、
  关闭交接与 Unknown）；child **16 passed**（增强实际迟到 driver/dispose/panic 结算）；
  side-question/UI **23 passed**（新增丢准备 waiter、移除 thread 后的实际结算）。
  [Host 联测](in_process/tests/auxiliary.rs) **1 passed**：native 后台 driver 尚活时丢 close
  waiter仍保 owner/物理 writer/资源 lease；driver 退出后外部真实 DLL keepalive 继续
  阻住释放，实际 Released 后两把物理锁才可重取。不是默认 CLI/HTTP/DLL 联合 E2E。
- Host **268 passed**、实际 binary **31 passed**，CLI Clippy 通过（原 55 / 97 warnings，
  新实现无告警）；core fmt、双 diff、四合同逐字与 807 源码快照通过。SDK 源码未改，
  未重复执行前批已过的 SDK 258 / 1 ignored gate，该数字不冒称本批新跑。
- 完整 CLI 首次 **1505 passed / 5 failed / 3 ignored**：除原 4 项 prebuilt 伪对象
  codesign，还出现一次未改 fork 测试的 Session ID 未变化断言失败。该项单独 **1 passed**，
  完整并行重跑 **1506 passed / 4 failed / 3 ignored**；fork 未再复现，**原因尚未确定**，
  不能据此断言与本批无关。未改/跳过测试、未放宽签名/路径检查，所有日志保留。
- 证据 `/tmp/rpi-host-integration-checks/host-cli-aux-{tasks,children,questions,writer,cli,cli-repeat,fork-isolated,host,binary,clippy,fmt}.log`；
  `host-cli-aux-sources.json` / `host-cli-aux-validation.json`。实际 binary SHA-256：
  `8e617677424f32273d712e8f5dfb8f34343f2e7361240b35044d11a323e75f09`。
- **范围仍是 staged native 辅助域，不是完整 CLI cleanup**。不证明 OS 工具进程/后台
  后代、dev worker、未登记任务或未知工具副作用已结算；默认 Host、daemon/整组恢复和
  父死清理未开启。最终生产关闭还须协调所有域的停止发起、等待和失败 custody，不能
  在等待其他域之前过早释放 SDK/资源 leases，也不能假定晚发 SDK shutdown 总能解阻。

### B3c1：Darwin 原始进程组的 waitable PID 托管（历史验收，尚未接工具环境）

- [PinnedProcessGroup](tool_processes.rs) 是私有、阻塞式 keeper 基础，不是完整 CLI cleanup
  receipt。直接持有 `std::process::Child`，用 `waitid(WNOWAIT)` 观察前台结果而不回收 PID；
  **要求独占 wait 所有权**，不能与外来 process-wide reaper 混用。最终 guardian 尚未实现，
  不能据此宣称现有 `OsExecutionEnv` 已消除陈旧 PID 风险。
- [原生快照 shim](tool_processes/census.c) 由 [私有 build script](../../build.rs) 使用目标
  Darwin SDK 编译，不在 Rust 中猜测未绑定的 `kinfo_proc` ABI，不新增 crate 或 SDK/wire
  接口。`KERN_PROC_PGRP` 正向报告成员状态和启动时间；先确认稳定的 stopped/exited cohort，
  再向仍有 waitable leader 固定身份的原始组发 kill，实际观察全部 `SZOMB` 后才 reap leader、
  释放传入的真实资源 guard。不是 `kill + clear`、`ESRCH`、计数或单独 foreground exit 证明。
- Darwin 的 `proc_pidinfo` 不能读取上述 zombie；`waitid(WEXITED)` 也可能返回待消费的
  stopped 事件。前者改用完整内核快照，后者明确不是退出。超时/快照失败保留 custody 并可
  重试；`ECHILD` 永久撤销 signal authority。未结算 Drop 将 Child、guard 和撤销证据一并
  retained（最多 64 pins，容量在 spawn 前预占）；**没有生产 retained 重置/恢复接口**。
  不在锁内销毁这些 guard，不回退到猜测的单 PID kill。
- [七项实际进程测试](tool_processes/tests.rs) 通过：未回收前台保 PID/物理 lease、真实后台
  成员仍活、超时后重试退出、另一组不被误伤、spawn 失败、丢 wait authority、Drop 后仍保管。
  Host **275 passed**（包含上述 7 项），重建的实际 binary **31 passed**；CLI 全目标 Clippy
  通过（原 55 / 97 warnings，新实现无告警），core fmt、双 diff、四合同逐字和 **811 个
  编译输入**快照通过（包含新增 C shim）。Darwin arm64 实测；非 macOS 仅验证 build script
  不启动 Darwin 编译器的旁路，不声称 Linux/Windows 后端或跨编译已验收。
- 保留两轮初始失败：native 首轮 **1 passed / 2 failed**（zombie metadata）；Host 首轮
  **273 passed / 2 failed**（stopped 事件），均未改断言/跳过测试，修复后上述 275 项通过。
  证据 `/tmp/rpi-host-integration-checks/host-tool-processes-{native-first,native,host-first,host,clippy,binary,fmt,build-smoke}.log`、
  `host-tool-processes-sources.json` / `host-tool-processes-validation.json`。B3c1 实际 binary
  SHA-256：`b4986d33547f79c872d28e116115da3dd844603e7e64fcd81074dcd73893208e`。
- **仅证明原始进程组这个子集**，不覆盖 `setsid/setpgid` 逃逸、外部 `SIGCONT`/group join、
  外来 reaper、父死清理或所有外部副作用，不能把返回值直接作为完整 `HarnessCleanup`。
  现有工具执行/全代际 env、dev worker、prepare 失败 custody 和跨域停机协调仍待接；
  默认 CLI Host、真正 daemon/整组恢复仍未开启。SDK/完整 CLI 未在本批重跑；B3b3 的四项
  codesign 失败及一次原因未确定的 fork 失败记录仍保留，不能将历史 gate 当本批新结果。

### B3c2：逐 dev 实例独占 staging 命名空间（当前，仅所有权前提）

- [私有分配器](dev_staging.rs) 让每次 [DevExtension::detect](../dev_extension.rs) 使用新
  UUID，包名和 PID 只用于诊断；同进程、同 workspace、同包的不同实例不再共享 staging。
  最终 owner 目录用原子 `create_dir` 独占创建，不以 `create_dir_all` 采用既有目录；碰撞
  最多重试 8 次后失败，不覆盖旧数据、不退回 PID-only 路径，也不回收既有旧目录。
- 原 build/reload、watcher、延迟 cleanup helper 的流程不变。新的随机命名空间避免普通
  PID 复用让旧 helper 指向新实例，但不是抵御外部目录替换的 OS 沙箱或删除授权协议。
  `rebuild()` 当前是持 reload 门的同步调用，并非已经存在的独立异步 build worker。
- 新增 4 项 Host 分配测试：同包同 PID 独立、碰撞后重试、耗尽保留数据、最终 owner 路径
  的符号链接不被采用/跟随。另以真实 Cargo metadata 连续 detect 两个实例，保留一个
  旧 PID-only 目录；清理一个实例后，另一个及旧目录中的实际文件均保持。测试仅使用
  临时 fixture，不删除用户 staging。实际 binary 的隐藏 cleanup helper 另有定向隔离检查。
- 本批 dev **8 passed**、Host **279 passed**、重建实际 binary `host_control` **31 passed**，
  均 0 failed / 0 ignored。CLI 全目标 Clippy 通过，原 lib **55** / lib-test **97** warnings
  保留，新实现无告警；core fmt、双 diff、四合同逐字及源码/链接核验见同批证据。
  源码快照补录已有 CLI bench 的编译输入；**没有运行 benchmark**。
- 证据 `/tmp/rpi-host-integration-checks/host-dev-staging-{dev,host,build,binary,clippy,fmt}.log`、
  `host-dev-staging-sources.json` / `host-dev-staging-validation.json`；当前实际 binary SHA-256：
  `1efbea04597de94d5ddd7080f6f9bb605917c5d0fc061bbfe484cb6e53fca923`。
- **不证明 watcher/延迟 helper 已由 Host 持有并结算，也不证明 Cargo 后台后代退出**。
  未改为异步 rebuild，避免在 dev owner 与停机顺序未完整接好前制造脱离 waiter 的任务。
  全代际 OS env/dev 执行资源、prepare 失败 custody、跨域 shutdown、默认 CLI Host、真正
  daemon/整组恢复仍未完成；SDK/完整 CLI 本批未重跑，B3b3 的历史失败记录继续保留。

下文阶段45/46数据与 binary SHA 是既有阶段快照，不是收敛阶段或 daemon/整组恢复的完成证明。

## 已有基础

| 文件 | 职责 |
| --- | --- |
| `types.rs` | Agent/Run/Request/Host epoch 身份、普通/mini、启动摘要与生命周期 |
| `config.rs` | Host 默认策略、逐 Agent 超时继承/禁用、硬上限与有效值来源 |
| `launch_policy.rs` | 私有前台/daemon及恢复策略；严格缺省/组合校验、尚未接通能力的serve前置拒绝 |
| `runtime.rs`、`runtime/tests.rs` | 无catalog的唯一内存成员表、owner阶段、事务/stop门与停止/关闭状态；尚未接默认CLI |
| `in_process.rs`、`in_process/tests.rs` | 私有Harness接管/prompt/取消/分段收尾及可信cleanup屏障；具体owner实例弱客户端，未装配CLI/进程/DLL资源 |
| `cli_cleanup.rs`、`cli_cleanup/tests.rs` | 正常CLI prepare/reload全代际不持库收据；真实DLL失败加载观测，有界/取消/封闭后Unknown；尚未装配最终cleanup |
| `capacity.rs` | 原子驻留预占、重复检查、动态降低上限而不驱逐、令牌释放 |
| `catalog.rs` | 64 槽有界入口、单个专用存储 worker、epoch/revision 守卫、分页与关闭确认 |
| `registry.rs` | SQLite WAL/FULL、私有目录与单实例锁、准确引用去重、重启分类及容量提交/回滚 |
| `lifecycle.rs`、`lifecycle_tests.rs`、`lifecycle_profile_tests.rs` | 独立行政意图/结果 journal、类型化去重、profile 版本、checkpoint 屏障与重启不重放；不自行执行，IPC 可查询 |
| `administration.rs`、`administration_tests.rs` | 可信 grant、有界 owned 请求协调、历史对账、fresh 动作与终态；五项完整意图变更已接 IPC |
| `daemon_administration.rs`、`daemon_administration_tests.rs` | daemon 固定权限/统一 profile 冻结/native handle；动态 profile 不自动加载 |
| `ordinary/mod.rs` | 普通 Harness 后端所有者、弱客户端、非持久化 ticket、输入预算及错误类型 |
| `ordinary/supervisor.rs` | 准确恢复、绑定预检、运行/投递监督、取消、deferred 状态、结算与关闭 |
| `extension_custody.rs` | 可信Rust binding的具体DLL owner、实际callback scope、失败custody与SDK/OS清理屏障 |
| `extension_artifacts.rs`、`extension_artifacts/tests.rs` | 纯读取冻结DLL bytes/路径/hash、有界artifact、显式根目录下每Agent/每次装配独立的私有staging |
| `ordinary/tests.rs` | 真实 Harness/JSONL/工具并发、断连、恢复与存储故障测试 |
| `service.rs` | 共用runtime成员表的catalog/普通后端适配：受监督加载/关闭、guarded 行政入口、shutdown fence 与失败保留 |
| `bindings.rs`、`bindings_tests.rs` | native-only 显式 RuntimeConfig/凭据/proxy/工具绑定及真实 HTTP/原生工具隔离测试 |
| `profile.rs` | daemon 启动 Agent 的显式私有 profile 解析/校验（Provider、环境、shell、工具授权） |
| `profile_set.rs`、`profile_set_tests.rs` | 冻结受信输入、随机版本引用、准确查找及独立 binding；已接 native facade 与 daemon 私有配置 |
| `ordinary/service_tests.rs` | coordinator 的真实 Harness 并发、客户端消失、迟到回调及故障测试 |
| `protocol.rs`、`fixtures/control-v1.json` | 版本化控制合同、独立身份、能力/预算及跨仓库 golden fixture |
| `client.rs`、`client_tests.rs` | 显式 Unix 连接、有界帧/整体截止时间、结果不确定分类、端点/peer 检查 |
| `server.rs`、`server_tests.rs` | 认证 IPC、原生 submit/receipt、连接/帧/期限上限、摘要分页及 inode 清理 |
| `server_administration.rs`、`daemon_administration_query_tests.rs`、`daemon_administration_execution_tests.rs` | 固定 grant 的允许 profile 发现/完整收据查询及 profile create/restore/guarded close/stable-input cancel/Stop（含 daemon_stop_tests.rs 退出排序验收） |
| `daemon.rs`、`cli.rs` | 持有 service 的前台 daemon、私有配置及 host 查询入口 |
| `error.rs` | 固定对外诊断；存储错误 Display 不输出原始存储内容 |
| `tests.rs`、`registry_tests.rs` | 基础规则、真实存储、跨进程锁、重启、并发与清理测试 |

`HostCatalog::open` 仅在显式调用时创建一个存储线程，不按 Agent 创建线程，SQLite
不在 Tokio worker 上执行。普通后端及其 owner coordinator 已可由 Rust API 显式装配；
新增显式 `rpi host serve/status/list/inspect` 前台 daemon/认证 IPC，并通过真实 extcore
客户端跨仓库验证。显式行政策略可额外开放允许 profile 发现与完整行政收据查询；
daemon 可在启动时按显式 `agents` profile 创建/加载普通 Agent，之后 IPC 可查询 receipt
并向已加载 owner 提交文本 prompt（submit）。显式行政策略现已支持完整意图的 profile
create/restore/guarded close/稳定输入取消 IPC 与完整请求文件 CLI；独立Stop IPC已接；**mini 与管理扩展尚未接通**；Stop提交CLI已接；协议枚举不代表已实现
能力。普通 CLI 默认路径未切换。
Host 超时目前只验证策略，尚未启动计时器；daemon 拒绝非空 Host timeout 策略，不静默忽略。
IPC 握手/请求期限则已运行。清单和 control 只支持 Unix，已在 macOS 验证；Windows
明确不支持，Linux/cgroup 仍需实机验收。
配置、能力、生命周期和验收口径见 [控制 daemon 说明](../../../../docs/architecture/shared-host-control-daemon.md)。

## 普通 Agent 的执行边界

- `OrdinaryAgent` 是长期所有者，已由 `HostService` 注册表独立保留；`OrdinaryClient`
  仅持弱引用。客户端、等待 future、ticket 丢弃不取消已接收的后台工作。
- 每个普通 Agent 使用真正的 Harness 与原生 JSONL Session，不另写 Agent loop。
  支持 prompt、steering/follow-up/next-run、pending/deferred resume 与显式 compact。
  空闲时不保留一个专门等待命令的 Tokio 任务，也不创建逐 Agent OS 线程。
- 从清单指定的 Session ID 和绝对路径恢复，不查找“最新 Session”，不重放 argv/prompt。
  验证 Session ID、cwd、解析后的 model/provider/tool 名称及自动驱动模式；绑定拒绝发生在
  writer 获取与修复前。随后 `commit_strict()` 在 writer lease 内复核完整历史摘要，再修复。
- `OrdinaryBinder` 是**可信的 Rust 装配边界**，不是通用生产资源解析器。它必须正确恢复
  profile/资源、凭据作用域、工具权限与 hooks，不调用 CLI 全局 cwd/env 初始化。
  `NativeBindingProfile` 已实现下述 native-only 绑定；legacy/FauxProvider 测试入口仍保留，
  不能据此声称完整资源、扩展和交互审批恢复已经接好。
- `RunTicket` 只标识本次 Host 生命周期中的监督任务；Harness operation ID 位于运行结果，
  两者不可混用。成功 submit、Running 元数据和 ticket 都**不是持久化输入 ACK**，没有
  Request 收据、跨 Session 接收协议或工具 exactly-once 保证。
- 同一个 Agent 拒绝重叠执行。不同 Agent 的工具在单线程 Tokio runtime 内确实重叠，
  不用“进入队列”代替并发；测试还检查工具返回的实际 cwd，而非只检查 Session header。
- 旧取消API匹配精确ticket/真实Harness run ID；稳定输入取消另匹配原submitted_request_id。
  旧目标不会取消新一轮执行。
  不 abort/drop 实际 Harness future；取消后继续等待真实结算、迟到 callback 与 Session drain。
- `Suspended` 不是完成。停驻 deferred 时仍为 Running/busy，没有活跃 Tokio 执行任务也不
  发布 Idle；恢复时直接 Starting -> Running，不隐式轮询 provider。显式轮询保留原生 run ID。
  本地 close 不代表远端 deferred 作业已完成；服务层仍需定义远端作业的取消/保留策略。
- 无条件 owner close/shutdown 先停止接收，再协作取消、等待实际执行与写入排空，最后
  关闭 Harness、释放 writer，才提交 Closed 并释放槽。Closing 提交失败仍请求取消并结算；
  Closed 提交失败保守保留驻留槽，重试只补交元数据，不再次关闭已关闭的 Harness。
- 另有 native `cancel_guarded`/`close_guarded` 管理入口：同一 control gate 内检查
  epoch/revision，取消还必须匹配精确的 volatile ticket。guarded close 在 retire/cancel
  前先赢得 Closing 的 catalog CAS；旧 guard 或竞争 CAS 失败不能变成无条件 shutdown。
  `GuardRejected` 单独标记确定的前置拒绝，不能和已经取消/清理后的 catalog 错误混淆。
  旧 closed handle 还会复核实际 catalog，不能确认关闭后来重载的新 owner；等待方丢弃
  仍不取消已接受的结算。这些裸 API **本身不是 request-ID durable receipt**；下述 journal、
  受信 profile allowlist 与行政 coordinator 已通过独立完整意图命令接入 IPC。
- 写入失败不一定生成 write-warning 或恢复副本。直接的 SessionStorage/Io 错误、panic、
  warning/unsaved 数据都会使后端进入需对账状态；运行报告提供 `persistence_uncertain`。
  不继续接收操作，不正常卸载，不把零 warning/零副本当成“已保存”。可取出的恢复副本
  可以导出，但不能承诺所有失败都有副本；原生日志中的未知工具结果也不能盲目重放。
- Owner Drop 仅尝试清理，不保证成功卸载。服务必须在显式 close 成功后才移除所有者；
  对失败或持久化不确定的所有者需要显式对账/恢复流程，该管理协议尚未实现。

## 独立行政 lifecycle journal（原生底座，受控 facade 已接 IPC）

- `HostCatalog::prepare_lifecycle/finish_lifecycle/lifecycle_receipt` 由同一个有界 catalog
  worker 执行，不是 Session 输入 admission，也不执行 Agent。schema **1 → 2** 在单个
  SQLite 事务内迁移；重启分类、journal 校验和 epoch 更新一起提交，损坏时整体回滚。
- 用 `ControlScopeId + RequestId` 去重，逐字段比较 epoch 和完整类型化 action 参数。
  相同请求只可能有一个 `fresh=true`；相同身份但参数不同被拒绝。旧 epoch 的**精确重复**
  只返回原收据，绝不重新授予执行资格。scope 是名字空间，不是授权凭证。
- 新意图先检查 epoch、Agent 存在性/revision 和容量，再提交 Prepared；同一 Agent 的
  pending 操作互斥，Stop 与所有 pending 操作互斥。只有 SQLite commit 与严格 FULL WAL
  checkpoint 都成功才返回 fresh 资格。checkpoint 忙/部分完成时不承认可见记录；查询和
  精确重复也须 checkpoint 重证同步，成功后仍只返回 `fresh=false`。
- 意图与动作**不是一个原子事务**。调用 coordinator 必须独立于客户端 waiter 持有
  prepare → action → finish，且只在 fresh 时执行。丢弃 journal 回复不能把执行资格转给
  重试；当前原生 facade 已装配这一顺序，也不承诺 pending 一定执行或自动取得结果。
- 终态不可改写：Succeeded、确定未产生动作的 Rejected、可能已有动作的 Uncertain。
  重启时 Prepared 只改为 Interrupted，不能自动重放；不存在“从 pending 接管再执行”。
  `CancelSignalled` 不证明结算；`NativeSettled` 不证明 listener/进程已经退出。
- journal 全 catalog 最多 **1024 项**、每项 **4096 bytes**；不 TTL、不静默淘汰请求身份。
  满额拒绝新意图，既有精确重复/查询不因此失效；尚无生产归档/删除 API。参数和结果均为
  闭合类型，不接收任意 JSON、凭据或工具负载；finish 只能由可信 coordinator 使用，不能
  作为客户端可写结果接口，更不是工具副作用 exactly-once。
- **15 项新测试**覆盖并发唯一 fresh、完整参数冲突、scope、旧 guard、未接收回复、
  重启中断、终态幂等、v1 迁移/损坏事务回滚、容量边界和 insert/update 故障；真实 reader
  阻塞 checkpoint 分别验证 Prepared/终态可见但不得 ACK，解除后只同步、不重授执行资格。
  另用真正 SIGKILL 的 catalog 子进程验证已提交意图存活、重启不执行、不重授。
  这是原生 journal 证据，不是动态 lifecycle、管理 DLL 或三方联合 E2E。
- SIGKILL 子进程的重复并发验收暴露了既有实例锁释放窗口：CLOEXEC 只在 exec 时关闭
  继承描述符，单靠最后 close 可能使 shutdown ACK 后仍短暂持锁。现在数据库/配额排空后
  显式 unlock，再关闭描述符并 ACK；Drop 也防守性解锁，unlock 失败不冒充成功。
  另 **1 项确定性共享描述符测试**复现旧 last-close 行为，验证显式 shutdown/Drop 都能
  立即重开、不 unlink 原 inode，且旧继承描述符关闭不解锁后来 owner。以上 **16 项**
  本轮新增测试另重复三轮全通过，没有用 sleep/retry 掩盖锁问题。

## Service guarded 行政入口（已连接 native journal/固定权限 IPC）

- `HostService::close_ordinary_guarded/cancel_ordinary_guarded` 在 owned 任务中运行，并持有
  service lifecycle 读预占，防止 shutdown 在信号/结算结束前关闭存储。不同 Agent 仍并发。
- close 的明确 `GuardRejected` 或原生 close gate 前置 `Busy` 恢复**原 phase**，不一律设 Live；
  原先 CloseFailed 的 owner 仍 CloseFailed。泛化 Catalog/Harness/任务错误不假定无副作用，
  保留 owner、标记 CloseFailed。没有 native owner 的 StartFailed 不由 guarded close 清掉。
- stopping 快速拒绝新行政请求，并在预占后复核；无条件 close/shutdown 保留原有重试与
  取消结算语义。已经接收的 close 即使 caller abort 也保留 owner/writer/槽直至迟到 callback
  和 native drain 完成。cancel 校验 owner 观察到的 revision + 精确 volatile ticket，**不是
  authoritative catalog CAS**；取消信号本身不证明结算。
- **5 项真实 Harness/JSONL 测试**及额外三轮覆盖 stale guard 无取消/retire/卸载、旧 ticket
  不影响后续 run、CloseFailed 不误恢复 Live、Closing 写入故障与无条件 shutdown 重试、
  StartFailed fence，以及丢弃等待/迟到 callback/shutdown 写屏障。
  这些 API 仍须由可信 profile/journal coordinator 装配，不是授权或 durable 行政回执。

## Native-only 显式绑定（新增）

- `NativeBindingProfile::bind` + `HostService::restore_bound` 接入真实 Runtime Provider 和
  read/bash/edit/write/docs 内置工具，不读取 CLI Args/settings，不设置进程 cwd、proxy 或
  `RPI_SESSION_ID`。这是 native-only Rust API，不是完整 CLI/daemon 的生产入口。
- 每个 Agent 独立提供 `RuntimeConfig`、闭合 `provider_values`、闭合 `tool_environment`
  和绝对 shell 路径。Provider 的 key/proxy/NO_PROXY 只从该 Agent 的值源解析，key 不自动
  复制给工具；shell、文件路径 HOME 展开和 Session I/O 使用同一独立执行环境。
  `OsExecutionEnv::with_environment` 缺 HOME 时保持字面 `~`，不退回 ambient HOME 或 `/`。
  未使用该接口的旧 CLI 保持原环境继承与 shell/per-call overlay 语义。
- 工具默认 deny-all，仅接受可信 Host 提供的明确 `Preapproved` 名称集合；模型/未来 IPC
  输入不能自行铸造 grant。这不是交互审批或文件系统沙箱；预授权 bash 仍能运行本机命令。
- `NativeBindingProfile::bind(&self, spec)` 可重复使用只读可信输入，每次生成独立环境和
  cleanup 所有权，不缓存跨 Agent 的执行环境。完整 AgentSpec 静态校验、闭合必需 headers、
  禁用 secret command 与精确 provider/model 解析先于 Session 新建和 catalog 新注册；
  这不证明远端 Provider 可达。保存资源声明/资源清单检查及环境 cwd 比对仍在 readonly
  facts 阶段完成，先于 writer 获取和 torn log 修复。阶段45支持显式冻结 instruction 与准确
  工具 inventory，拒绝其它 resource profile/资源恢复、未实现工具和不解析的必需 headers；
  错误不携带原始配置。扩展 registry/hook 与完整资源解析仍待接入，五项受控 lifecycle IPC 已接通。
- current-thread 测试通过真实 HTTP SSE Provider、HTTP proxy/NO_PROXY 与两个 shell 的双向
  barrier 检查 key/cwd/HOME/工具并行隔离；另验证真实 write/edit/read、原生 shell 的取消与
  close 结算。没有把 Rust API 测试标成真实 rpi binary + 管理 DLL 的完整贯通验收。

## Immutable trusted profiles（已接私有配置/发现及匹配的行政 IPC）

- `TrustedProfileSet::freeze` 消费可信 `HostStartupAgent`，上限 1024；发布引用前预检所有
  配置，不创建 Session、不获取 writer、不注册 Agent。集合不派生 Debug/Serialize，
  不提供插入/替换入口；一个条目固定一个 AgentId/完整 spec，不是任意身份生成模板。
- `ProfileRef { agent_id, version }` 只有身份与每次冻结新生成的随机版本，不是秘密配置的
  hash/凭据猜测 oracle，也不代表授权。`prepare` 必须准确命中整对引用，旧版本/伪造引用
  不能回退到当前配置；每次生成独立 binding/environment，不缓存 cleanup 所有权。
- 拒绝重复 AgentId、Session id、canonical parent 归一化后的路径及 Unix dev/inode
  硬链接别名；最终 symlink/目录或不可用 parent 也拒绝。只检查此刻可见的别名，不冻结
  外部文件系统或提供 sandbox；已有 Session 内容/资源仍须严格 readonly facts 验证。
- journal 新增 `CreateProfile/RestoreProfile`，完整记录 profile 引用并沿用原 revision、
  pending 排他及结果兼容校验。保留 v2 `Create/Restore` 原格式，不转换/补发 profile 版本；
  原生 coordinator 拒绝新的无绑定 legacy 动作。schema 2 表布局不变，不承诺旧
  binary 能读取新 op。journal 本身既不验证 profile 授权，也不执行或加载 Agent。
- 重启会重新冻结版本；原生 coordinator 先对账历史完整请求，再对 fresh 请求查当前
  profile，不能把旧回执重放或因版本更新而改写。7 项集合测试及 3 项版本/legacy journal
  测试各额外三轮通过；尚不是动态生命周期或管理 DLL 的 E2E。

## 原生 AdministrativeCoordinator（固定 authority，五项变更已接 IPC）

- `AdministrativeGrant` 由可信 Rust owner 显式构造，固定 scope、允许的 Agent 集合及
  create/restore/cancel/close/stop 权限，默认全部拒绝；不派生 Deserialize/Serialize/Debug。
  所有授权在 lookup/副作用前执行。profile 引用与 scope 本身都不是凭证。
- 每个 service 只允许 claim 一次 facade；即使 facade drop，也不转授可能仍在执行的
  epoch custody。owned job 表最多 64 个不同请求，以 scope/request ID 比较完整参数；
  重复 waiter join 原任务，冲突拒绝，满额拒绝新 job，丢弃 waiter 不取消任务。
  此上限不是 IPC 连接或 waiter 数量上限；该底座不定义 wire 变更操作，只读接入见下节。
- 历史请求先对账，再查当前冻结 profile；reservation 内再次对账，关闭初次 None 查询
  与旧 job 完成移出表之间的竞态。历史 Prepared/Interrupted 不授予执行资格。
  只有唯一 fresh 分支执行动作并 finish，prepare/checkpoint 未获确认时绝不接管。
- Create 不复用已有路径，使用 exclusive Session create；初始化、注册或加载中出现
  可能已有副作用的失败保守记 Uncertain，部分文件不删除、不覆盖、不重放。
- Restore 校验完整 spec/revision 与 readonly saved facts，早于 Starting；startup 也在
  新注册前执行该只读预检，因此孤儿/不支持资源在重启尝试中也不产生注册记录。
  实际 strict restore 仍再次检查，不把 preflight 当成外部文件系统冻结或 writer 证明。
- Cancel 保留精确 volatile ticket 语义，另有 CancelSubmitted 稳定输入目标，见下节。
  成功收据不等于结算；Close 持 reservation 等待迟到 callback 后才确认 Closed。
  仅稳定输入取消经独立行政命令开放，legacy ticket Cancel 仍不开放 IPC。
- Stop 先独占预占、fresh 后 settle；native 失败仍持 fence 发布 Uncertain，保留失败
  owner/catalog 供显式无条件 shutdown 重试。成功后严格发布 NativeSettled 才关闭存储；
  发布失败保留 stopping/open catalog。仅缓存一条已 checkpoint 的成功 Stop 收据，允许
  当前 facade 在 worker 关闭后对账精确重复；它不证明 listener/进程已经退出。
- **12 项测试**（9 项 facade/存储故障及 3 项真实 Harness/JSONL）额外三轮通过：授权、
  容量/丢 waiter/去重竞态、精确取消、迟到关闭、旧版本历史、部分创建、checkpoint 未获
  fresh、终态发布失败及 Stop 原生结算失败；不作为管理 DLL 的联合 E2E。

## Daemon 私有行政装配（固定 authority，五项变更已接 IPC）

- 可选私有 `administration` 配置固定 `scope`、`allowed_agents`、`permissions`、
  `profiles` 与 `max_jobs`（1..64，默认 8）；允许的 Agent/动作默认空。scope 必须作为
  固定 namespace 保存，不是凭证。该配置只由 private-file loader 读取，不能从 peer
  request 反序列化 grant。重复 Agent/权限、未知字段/动作、相对动态路径、预算越界拒绝。
- 启用后，在 catalog open 前统一 freeze 启动 `agents` 与动态 `profiles`（合计最多
  1024），拒绝跨集合重复身份/Session/路径/inode 别名。允许的 Agent 必须属于统一集合。
  startup 可直接获得自身 ref，不要再在动态列表重复配置同一 Agent。未配置行政策略时
  保持原有 `open_with_agents` 行为，不安装 facade。
- 只有 startup 会自动创建/注册/加载；动态 profiles 只表示可用的受信输入，须显式行政命令。daemon 持有
  固定 grant 的 `DaemonAdministration`，native `administration()` 可取 handle；发现的
  refs 仅包含允许的 Agent，native execute/receipt 仍按完整请求授权/对账。不存在模型
  提供凭据、任意 spec、工具提权或 scope 自动获权。
- 启动失败结算早已加载的 owner；bind 失败保留外来端点并释放 catalog/writer。
  NativeSettled 不证明 listener 已退出；当前owner实际成功关闭存储后，另以易失通知驱动有界退出。行政 CLI 的显式
  命令仍须同一固定 grant 授权。策略也不缩小既有 token 的全 catalog 查询/submit 权限，
  不是按工具参数的审批或文件系统沙箱。
- 新增 **6 项 daemon + 1 项配置默认值测试**及额外三轮；真实 binary 新增 **2 项**，
  覆盖动态输入不自动加载/两次信号重启/查询不读 profile，以及 15 类错误在 Host 产物前
  拒绝。当轮 binary 全套每轮 **10 passed, 0 ignored**；仍不是动态 lifecycle 变更 IPC 或管理 DLL E2E。

## 行政只读 IPC（wire v1.1）

- 只有配置 `administration` 的 daemon 才额外公布 `profiles` / `administrative_receipts`；
  未配置仍只有原先五项能力。minor 1 不改变 major 1 兼容性，未知能力不授予权限。
- `Profiles { after, limit }` 按 AgentId 返回允许集合中的不透明 `{agent_id, version}`，
  页大小 1..128，携带固定 `scope` 与 `next_cursor`；无凭据、路径、配置摘要或自动加载。
- `AdministrativeReceipt { request }` 必须携带完整原 scope/request_id/epoch/action。
  外层 epoch 是当前连接 epoch，内层可为历史 epoch/profile 版本；由固定 grant 先授权，
  原生 journal 再严格同步并比较完整参数。返回 found/receipt，冲突不降格为 missing。
  未找到也不 prepare、不解析当前 profile、不接管或重放；不接受 peer grant 或任意 JSON。
- 收据查询中的 action 包含 legacy Create/Restore 与 Cancel/Close/Stop，只表示历史意图，
  不因查询而执行；Close 的独立执行命令见下节。嵌套原 request ID 不是本次 mutation ID，断线/超时返回 QueryFailed，
  不能误报 OutcomeUnknown。查询复用既有连接/帧/总期限预算，CLI 尚无动态命令。
- 新增 **3 项真实 daemon IPC + 2 项协议 + 1 项客户端**测试；真实 binary 新增 **1 项**
  两次重启对账 Interrupted（真实 pending journal，绝不执行），并扩展动态配置测试验证
  发现结果无配置泄漏/无加载。不会将这些查询验收称为管理 DLL 三方 E2E。

## 完整意图行政执行 IPC（wire v1.4，五项执行 CLI 见下节；Stop IPC见后文）

- 行政策略配置后额外公布独立的 `administrative_create` / `administrative_restore` /
  `administrative_close` / `administrative_cancel`，与旧占位能力完全分开。分别只接受
  CreateProfile、RestoreProfile、revision-guarded Close、CancelSubmitted；legacy
  Create/Restore、ticket Cancel、Stop 不能借新操作执行。默认 grant 拒绝全部，capability 不是权限。
- 都携带完整原 scope/request_id/epoch/action，沿用固定 native grant、唯一 fresh 和 owned
  coordinator。外层当前 epoch 不改写内层原 epoch；完整历史先于当前 profile 版本解析。
  Data 内是原 LifecycleReceipt，不是新的执行 ACK；历史 Loaded 不代表现在已 resident。
- 本次 mutation ID 为原 request_id。断线/超时是 OutcomeUnknown，重连用完整原意图查询；
  不取消已登记的 owned job。存活 epoch 的 found=false 也可能是 job 正等待 reservation，
  不证明将来不会有作用，不能换 ID 重放。native Uncertain/OutcomeUnknown 同样须对账。
- 连接/帧/整体期限与 distinct owned jobs 预算分别有界；无任意 spec、凭据或 peer grant。
  稳定输入取消及独立 Stop 已接 wire；Stop 的完整意图/回复有界写出/停接入与退出见后文。
  CLI 不改变这些权限；稳定取消 CLI 见下文。
- 新增 **3 项真实 daemon IPC + 1 项协议矩阵 + 1 项客户端**；500ms server deadline 确认
  caller 丢失后 job 仍占 capacity，fence 释放后完成。binary 新增 **2 项**：真实动态
  Create→HTTP Submit→Close→Restore→HTTP Submit→Close、重启旧版本重复不加载，以及
  丢弃真实 reply 后查询/重复不重建文件。HTTP fixture 用足够 context window，验证实际
  用户 prompt 而非自动 compaction；旧 helper 保留，专用新 reader 有界读取完整 HTTP。
  额外三轮每轮 **3 + 1 + 13 passed, 0 ignored**；不是管理 DLL 三方 E2E。

## 完整意图行政 CLI（显式请求文件，不自动重试）

- `profiles --config <private-file> [--after <uuid>] [--limit <1..128>]` 只发现允许引用。
  `administrative-receipt/create/restore/close/cancel/stop --config <private-file> --request <private-file>`
  中的 request 不是 Agent profile，而是完整原 scope/request_id/epoch/action JSON。
  文件须绝对路径、私有 owned parent/regular file、单 hardlink、非 symlink、最多4096bytes。
- CLI 从不生成/替换 ID、epoch、profile nonce，不根据当前发现结果重建旧意图，也不重试。
  请求读取/闭合校验与操作匹配在连接前完成；查询不读取 serve-only profile。legacy ticket Cancel/裸Stop
  以及旧无绑定 create/restore/close 命令不开放；普通文本 submit/Agent receipt CLI 见下节。
- 退出码：0 仅验证过的 Succeeded 历史结果；1 确定拒绝/未发送；2 用法或不安全/无效输入；
  3 缺失、Prepared/Interrupted/Uncertain、丢回复/查询失败、不可验证响应或 mutation stdout
  写失败。响应验证 scope/requestID/完整原意图与 Native result/action/revision，拒绝伪成功。
  found=false 不是无作用证明；0 也不是当前驻留、工具完成或进程退出证明。保留原文件对账。
- 前序新增5个 unit（当时含3×7操作匹配、schema/私有有界文件/不改写、状态和传输分类），binary
  新增3项：真实CLI+两轮HTTP动态创建/关闭/恢复，旧epoch/refreeze请求原样对账且不读坏
  profile，stdout断开后的已commit关闭仍返回3并可查询；13类坏输入真实监听器确认未连接；
  None unsupported/Some默认拒绝均非零。原Interrupted测试补两次重启CLI返回3。
  当轮全套 **196 Host / 16 binary / 18 extcore / 1 显式artifact passed, 0 ignored**；
  额外三轮每轮 **7 CLI unit + 16 binary passed, 0 ignored**。未包含管理 DLL。

## 普通文本 submit/receipt CLI（0 仅证明 custody）

- `submit` 与 `receipt` 均为 `--config <private-file> --request <private-original-file>`，
  原文件闭合字段为 agent_id/request_id/prompt，不是行政 scope/epoch 意图。query 仍保留原
  prompt 以校验输入 digest。文件绝对私有、单hardlink、非symlink、≤1MiB；UTF-8 prompt
  非空且≤256KiB，稳定 timestamp=0 的 Native messages JSON≤512KiB，连接前全部校验。
- `cli_submission.rs` 从不生成/替换ID/输入、不自动等待或重试。严格验证Host/Agent/request/
  entry身份、entrySeq/history、单消息计数和字节数、精确SHA256 v2或legacy双SHA256，
  found/durable/execution闭合形状和run UUID。非法响应不回显；不能借错原prompt查询伪成功。
- submit 退出0仅为精确输入已durable；receipt退出0仅为匹配custody observation，Admitted/
  Started(finished=false/true)都不代表工具/运行成功，finished仅Native finish marker。
  missing、丢回复/查询失败/存储不确定、非法响应、mutation stdout失败均3；未发送/明确拒绝1，
  坏用法/输入2。保持原文件和JSON对账，不换ID重放；同ID改prompt沿用现有InvalidRequest。
- 新增4 unit、2实际binary：gated HTTP下submit退出0早于Provider响应，receipt证明finished
  false，同ID重复/真实丢stdout/错prompt拒绝/错误原文查询3/missing3；释放后完成，重启
  查询/重复不改JSONL且只一条admission。14类坏文件两操作均无连接。前轮动态生命周期
  binary也已把投递/receipt换成真正CLI，两轮HTTP闭环不再靠Rust客户端投递。
  **200 Host / 18 binary / 18 extcore / 1显式artifact passed, 0 ignored**；额外三轮每轮
  **11 CLI unit + 18 binary passed, 0 ignored**。仍无管理DLL三方E2E。

## 稳定输入取消 Native 底座

- `Active.submitted_request_id` 关联原执行而不是重复 submit 的新 observer ticket；同一个
  control gate 中核对owner epoch/revision及精确输入ID再signal。旧精确ticket接口不变，
  不扫描receipt/runId猜测目标，也不取消下一输入。API不表示授权或执行结算。
- 显式deferred poll在同一owner内携带关联，parked无active时返回false、不发起resume；
  完成/成功close清除关联，restore不从历史重建它。Snapshot仅易失观察，非durable证明。
- Native `LifecycleAction::CancelSubmitted {agent_id,revision,submitted_request_id}` 与旧
  Cancel(ticket_id)是不同闭合意图。Service reservation与固定cancel权限复用，coordinator
  仍仅fresh执行，CancelSignalled只表示信号。目标属于完整去重参数，旧ticket行不改写，
  prepared重启只Interrupted；重复历史不重新查当前active、更不重信号。
- 前一 Native 增量新增6项：三项真实Harness/JSONL证明重复ticket/guard/后续输入隔离、迟到callback仍持
  writer/owner、deferred携带与restore清空；一项journal完整目标/legacy/重启；一项owned
  job在held reservation丢waiter后仍完成、默认拒绝/无目标false/历史不误伤下一输入；一项
  Native daemon+实际HTTP，Provider连headers都不返回也能Abort结算，下一HTTP门控下
  重复取消/旧目标false不误伤，重启对账不发HTTP不改JSONL。不是Cancel IPC或管理DLL E2E。
## 稳定输入取消 IPC（wire v1.3，CLI见下节）

- 独立 `AdministrativeCancel {request}` / `administrative_cancel` capability，仅匹配闭合
  `CancelSubmitted {agent_id,revision,submitted_request_id}`；不复用旧 ticket Cancel。
  阶段38当时Some公布11能力，当前v1.4为Some12/None5；执行固定cancel grant，默认拒绝。
- 完整 scope/request_id/epoch/action 原样对账；行政 request_id 与被取消输入ID分开。
  mutation 发送后丢回复/超时保留原行政ID为 OutcomeUnknown，不重试、不换ID。
  历史成功只记录当时 signal，缺目标/parked/restore无关联为false，不自动resume或重放。
- 新增协议闭合/guard测试及真实daemon+HTTP IPC测试；扩展四操作×八意图矩阵及丢回复/
  timeout分类。500ms waiter超时后owned job仍持容量，held reservation释放后完成；
  默认拒绝、scope/Agent/epoch/revision拒绝与同ID改目标冲突均不误伤当前输入。
- 新增真实binary+两次HTTP：丢取消reply后用完整原意图查询；首输入在Provider不回headers时
  Aborted，第二输入门控时旧历史/旧目标不误伤并正常完成。重启对账，JSONL字节不变。
  这不是管理DLL三方E2E；legacy ticket Cancel仍关闭，独立Stop IPC见后文。

## DLL 严格加载与物理清理凭证（阶段46进行中）

已在 [extension loader](../../../rpi-extensions/src/loader.rs) 落地
`load_explicit_session_owned`：先检查完整绝对文件集合与 canonical 别名（至多32个），
不扫描、不跳过坏 DLL、不接 ActionBridge。任一加载失败均不发布可执行 registry；返回
不可执行的 `PluginLoadCustody`，持有先前成功 DLL，以及入口符号缺失或注册失败的已打开
DLL。失败注册也可能已有 SDK workers，不能按普通参数拒绝直接释放 Host 容量。

[cleanup receipt](../../../rpi-extensions/src/cleanup.rs) 不持有代码/上下文；`Pending` 不是
结算，`Released` 仅在 SDK shutdown 成功、该 load 的 library handle 显式关闭、API 与
host guard 销毁后发布，`Retained` 表示失败或缺失完成证明。等待者取消不影响清理；不能把
Drop 排队、超时、零回调或 SDK finish 单独当作 handle/context 已释放，也不声称其它 OS
loader handle 不存在。旧 CLI loader 仍保留 skip 行为，不能拿它装配严格 Host profile。

SDK新增 **10项测试（7项真实编译DLL + 3项receipt状态/并发/取消）** 已保持通过。
Host现已接入 [具体custody](extension_custody.rs)：`OrdinaryBinding::with_extensions` 仅为
可信Rust装配口，不是profile或IPC授权。一次claim后不可复用；失败装配先等待DLL清理，
`Retained`/未知保留Starting/StartFailed及已取得的writer，成功关闭也先等待实际callback、
SDK/handle/guard释放，才释放writer和提交Closed。StartFailed重试先预占Closing，不能让
旧reconciler误关新load。最后Drop无证明时保留writer及catalog单实例锁至进程退出。

[冻结artifact](extension_artifacts.rs) 纯读取至多8个DLL、单个64 MiB、合计128 MiB，拒绝
canonical/hardlink别名；固定正文、路径、SHA与顺序，identity不是grant。只在owned装配中
于显式private owned根目录创建每Agent/每次装配独立的0700 staging，新文件只读且不复用
原路径/inode；不使用ambient TMPDIR。目录guard交SDK持有，Released后回收、Retained时保留。
[新增12项Host测试](ordinary/extension_custody_tests.rs) 包括7项真实DLL场景（使用真实Harness、
Faux Provider）及5项artifact测试：纯预检不加载/stage、失败注册和post-commit失败门控、
迟到AfterToolCall加SDK finish双屏障、waiter取消、Retained三种失败、拒绝跨Agent复用、
同一artifact的双Agent静态runtime隔离与独立关闭。artifact单元测试位于
[extension_artifacts/tests.rs](extension_artifacts/tests.rs)。

当前 **235 Host / 28实际binary / 255 extensions功能 / 4显式DLL smoke / 8资源 / 69旧CLI
Session / 20 extcore合同 / 1显式daemon artifact** 通过，均0ignored；仅排除既有benchmark。
额外各三轮235 Host和28 binary通过；Clippy保持55/97既有warnings，沿用
`-A clippy::not_unsafe_ptr_arg_deref`，未改actions或lint配置。并行SDK/Clippy压力下曾有一次
既有300ms Stop排空测试Connection失败，未修改时限，保留失败日志，后续单独三轮全通过。
当前binary SHA256：`9aabb4a285b8add64cc636e4afbccf3138c0873a744d2621968e094b15e2ffe5`；
日志 `/tmp/rpi-host-integration-checks/host-dll-custody-*`，staging权限初次失败另存
`host-dll-staging-initial-failure.log`（tempfile默认目录权限不是0700，已明确原子创建0700）。

**#46仍进行：Host profile DLL输入尚未开放。** 准确插件inventory、独立artifact grant、
不支持能力拒绝、逐Agent生产Provider/tool-result hook装配仍待接通，再验收实际binary内
迟到AfterToolCall。不能把Host Rust/Faux+DLL测试或独立binary HTTP测试相加冒充该场景；
#23及binary+管理DLL+HTTP三方E2E仍未完成。

## 显式 instruction 资源与持久绑定（最近完整阶段45验收）

私有 Agent profile 新增可选 `resources`，沿用 `ResourceSelection v1`，不是新的权限来源。
当前只接受绝对 instruction 文件（至多32个、合计64 KiB UTF-8），冻结正文、路径与 SHA；
`tools` 必须为 `none` 或与 Agent 工具顺序完全一致的 `only`，仍需 `approvedTools` 授权。
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
日志 `/tmp/rpi-host-integration-checks/instruction-*`。证据见生命周期验收矩阵的阶段45条目。

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

新增真实binary+HTTP关闭失败保留/显式shutdown重试场景，详见[逐项验收矩阵](../../../../docs/architecture/shared-host-lifecycle-acceptance.md)。
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

## 稳定输入取消 CLI

`rpi host administrative-cancel --config <private-json> --request <absolute-private-original-intent-json>`
仅接受CancelSubmitted完整原意图，复用4096bytes/私有owned regular file/单hardlink/非symlink
校验。行政request_id与submitted_request_id分开，绝不生成或替换scope/ID/epoch/guard/目标。
不等待结算、不重试、不改旧原文件；用administrative-receipt保留完整原意图查询。

退出0仅验证Succeeded历史，CancelSignalled(true/false)都不证明输入/工具结算；其余仍沿用
1确定拒绝/未发送、2坏输入、3missing/Prepared/Interrupted/Uncertain/丢回复/非法响应/丢stdout。
新增1个unit验证true/false与完整目标/guard/result匹配，扩展四操作×八意图及解析拒绝override。
新增2 binary：真实CLI+HTTP在fresh信号后丢stdout返回3、原文件查询/重复、下个输入隔离与
重启对账/JSONL不变；18类坏文件/目标真实监听器无连接。None/Some默认拒绝矩阵也纳入取消。

阶段39历史验收 **209 Host / 21 binary / 19 extcore / 1显式artifact passed, 0 ignored**；额外三轮
每轮 **209 Host + 21 binary**，全部0ignored。wire1.3/Some11/None5及四合同均未改变。

## 原生 admission 的存储前置（新增）

`rpi-harness` 新增 `SessionStorage::sync_history` 与固定 binding 的 Session 转发，JSONL
实现将物理同步与历史写入/替换排入同一 writer 队列，返回同步完成时的 history
`(revision, generation)`。已准入同步不随 caller 取消，拒绝未决写入、退役/失去 lease，
并通过严格文件系统接口同步文件和直接父目录。默认/内存后端明确拒绝；Unix OS 实现
不沿用旧 `sync_file`/`sync_directory` 的兼容 no-op 证明持久化。

这只是 **request-linked admission 的存储前置**；旧 `OrdinaryClient::submit` 仍不提供持久化
ACK（只有 `submit_admitted` 提供）。尚无 request-ID 原生日志关联、跨 Host/Session 对账或 crash reconciliation；IPC
变更/Receipt 仍不声明能力。范围不包括 progress、Host SQLite、祖先目录或绕过 lease
的外部写入者。测试与具体限制见集成文档第 6 节。

## Request-linked 原生 admission 与 IPC submit（新增）

- 显式入口 `OrdinaryClient::submit_admitted(host_id, request_id, messages)` 返回
  `AdmittedRunTicket`。`wait_admitted()` 只在原生日志已追加并完成物理同步后返回
  `HostAdmissionReceipt`；`wait()` 仍表示受监督执行的结算。两者不能互相替代。
- 每个请求写入一条原生 custom entry：类型 `rpi.host.admission.v1`，entry ID 等于
  request ID（必须是非 nil UUID），负载包含 host/agent 身份、kind、精确 messages、
  摘要和追加前的 history identity。新负载版本 2 使用标准 SHA-256；旧版本 1 保留原双
  SHA256 fingerprint 可恢复。公共 custom-entry API 禁止伪造该命名空间，context builder
  即使遇到同名 projector 也跳过该 entry。
- 收据只证明**输入保管**：`entry_id`/`entry_seq`/`history` 与同步边界覆盖的身份。
  它不是 Provider 收据、工具结果、执行完成或 exactly-once 工具副作用证明。
- `request_id` 是幂等键：相同内容重试返回同一收据且**不会再次执行**
  （`OrdinaryOutput::AlreadyAdmitted`）；相同 ID 不同内容明确拒绝；成功 submit、
  Running 元数据、ticket 或查询响应都不是持久 ACK。客户端重发相同文本必须换新 ID。
- 执行关联由原生结构决定：admission entry 成为 lane leaf，紧随其后的第一个
  `operation_started` 若以它为 `source_leaf_id`，且 intent 的输入以 admitted messages
  为后缀摘要结尾，才报告 `Started { run_id, finished }`；否则只报告 `Admitted`。
  不做自动重放、恢复或推断“可能已执行”。
- 独立输入预算：每条 admission messages ≤ 512 KiB 且 ≤ 64 条，超出在写入前拒绝。
  `host_admissions()` 枚举上限 64 条，超过返回 `LimitExceeded`（目标请求查询不受限）。
- 同步失败返回 `AdmissionUncertain`，不返回收据；包括 supervisor 执行中快捷重试在内，
  重试相同请求都经 `resync_host_admission` 比较完整请求并重证严格同步，不复用失败调用
  的 ACK。同步故障恢复后对已录入请求只返回收据，不自动执行。无效请求在 catalog 转
  Running 前拒绝，Agent 保持 Idle。
- IPC 新增 `receipt { agent_id, request_id }` 只读查询与 `submit { agent_id, request_id,
  prompt }`，公布 `receipts`/`submit`。submit 仅接受非空文本 prompt（≤256 KiB），必须指向
  已加载的 Live owner；回复是 `wait_admitted` 得到的原生收据（`durable: true` + receipt），
  **不是执行完成或工具副作用证明**。同 request ID 重试返回同一收据且不会再次执行；
  不设 catalog revision 门槛，幂等性来自 request ID。
- 收据查询返回 `found`、receipt 与 `execution`；未知 request 是 `found=false`，未加载/未知
  Agent 是 `NotFound`，同步不确定映射 `PersistenceUncertain`。查询不加载 Session、不重放、
  不追加日志；返回既有 receipt 前会严格同步并校验 history 覆盖（列表一次同步），不能
  把失败同步后可见 entry 当作 durable。restore/cancel/close/stop 仍返回 Unsupported。
- token 现在能对已加载 Agent 触发真实 Provider/工具执行（使用该 Agent 预授权工具）。
  token 不是逐 Agent scope，必须留在可信管理进程，不能进入模型上下文；submit 不创建
  Agent、不加载 Session、不改变权限或工具授权。

## Startup Agent profile（新增）

- daemon 配置可列 `agents: [<absolute profile path>, ...]`，仅在 `serve` 时读取。每个
  profile 是 0600、私有父目录、绝对路径的 JSON（`deny_unknown_fields`），包含
  `agent{id,name,cwd,session{id,path},model{provider,model},tools}`、闭合
  `runtime.providers`、`providerValues`、完整 `toolEnvironment`（绝对 HOME）、绝对
  `shell`、`approvedTools`、`thinking`、可选 `timeoutMs`/`maxInputBytes`。
- 没有 ambient settings/凭据/proxy/扩展发现；Provider key 只存在于 profile 或闭合
  `providerValues`，不会复制到工具环境。`agent.tools` 必须是 `approvedTools` 的子集，
  未实现工具由 native binder 在 writer 获取前拒绝。
- 启动顺序：catalog -> 逐个 Agent（先验证显式绑定/授权；已注册且 spec 相同则复用；
  否则在 Session 文件**不存在**时创建新 JSONL，再注册）-> `restore_bound` 严格恢复
  -> 绑定控制 listener。
  Session 已存在时交给严格 restore 校验 id/cwd/torn tail。新建使用持有真实 writer lease
  的 `JsonlSessionStorage::create_new`：内核独占创建空主文件，再独占创建并严格同步
  progress，最后追加 header 并严格同步主文件/父目录；不以存在性检查后截断写模拟独占。
  既有主文件、符号链接或孤儿 progress 均不覆盖，连相同 Session ID 的孤儿也不能被
  失败创建后的可读 header 误接纳。初始化中途失败可留下空占位/部分新文件，保留供
  显式检查，不自动删除重试；两文件不是原子事务。等待方取消不释放未完成初始化的
  writer。任何 Agent 失败都会先结算已启动 owner，不发布 readiness。
- 重启复用同一注册和 Session；相同 request ID 的 submit 仍返回持久 receipt 且不再执行。
  本节只描述 startup-only 阶段；后续显式行政 IPC/CLI 的动态创建/恢复/关闭见上文。
- 真实二进制测试已用本地 HTTP SSE Provider 完成 profile -> load -> IPC submit -> 原生
  执行 -> receipt -> 重启复用的端到端验证；仍不包含管理扩展 DLL。

## 存储与所有权

- 清单目录新建时 0700，文件新建时 0600；检查当前用户所有权，拒绝符号链接、文件
  硬链接与组/其他用户可访问路径，不静默 chmod。父路径按受信本机文件系统处理，
  不提供对同 UID 恶意文件系统操作的安全隔离，也不把这些清单检查推广为 Session 文件策略。
- `instance.lock` 不在关闭时 unlink，FD 带 CLOEXEC。清单关闭确认前释放 SQLite、驻留
  令牌与实例锁；最后一个清单句柄丢弃后 worker 收尾。
- 每条清单记录最多 128 KiB；命令队列 64 槽；分页最多 128 条（内部至多一条 lookahead）；
  SQLite 单行 256 KiB。普通后端每次输入也有字节预算。这些不代替未来 IPC、事件、输出、
  全局缓存预算，不限制调用者自行保留的响应或普通 Harness 的模型/工具结果体积。
- 普通 Session ID/path 分别去重不等于底层文件身份、可恢复性或 writer lease 证明。
  后端依赖原生存储 lease，并增加严格历史复核；不保护绕过 lease 的外部写入者。
- 重启校验、分类、Host epoch 更新同事务提交；坏记录整批回滚。mini 只能列出元数据，
  不携带 Session，不恢复内存。普通运行记录变 Interrupted 不等于已经恢复真实执行。
- 新驻留预占在数据库写入失败时回滚；关闭写失败保留令牌。只在确认实际清理并提交后
  发布释放。降低上限不强杀现有 Agent，调用者负责保存全局配置。
- catalog 的 `executing` 是 Running/WaitingApproval 元数据计数，不是工具 barrier，
  也不是外部工作停止的证明。Starting、Closing、模型/工具/审批/deferred 等待都不能
  作为空闲回收依据；未来管理器还必须观察后端 fault/retiring，而非仅看清单状态。
- 清单不替后端停止工作。服务必须先关闭全部实际后端，再关闭 catalog；`shutdown`
  只确认清单 worker、容量与锁清理，不等待模型/工具/外部进程。
- 清单 mutation 响应跟随 SQLite 提交，但不是输入 ACK。放弃等待不撤销已接收操作；
  须按稳定 ID、epoch/revision 查询后决定是否重试。
- 启动摘要不接收自由 argv/env/prompt。调用者必须保证名称、模型/profile 引用不含秘密；
  结构验证不能识别误填入名称的凭据。共享进程也不是安全隔离边界。
- 未配置超时不自动启用；有限硬上限不能 disabled 绕过；最大有限时长 365 天。
  不兼容新上限的历史记录仍可查询，但不得加载。计时器与重启截止时间恢复尚未接入。

## Owner coordinator 与控制客户端（新增）

- `HostService::restore_ordinary` 自己持有已加载 owner，不把 owner 交给临时请求 future。
  已开始的加载/关闭/shutdown 由独立监督任务结算，调用方取消等待不会撤销它们。
  不同 Agent 的执行不经过全局异步锁；shutdown 的独占 fence 等待在途加载/关闭事务。
- 重复加载不能替换已有 owner。加载回滚失败保留 `StartFailed` fence，不伪造 Closed；
  关闭失败保留 `CloseFailed` owner 与驻留槽，不关闭清单和单实例锁。
  只有在原生清理成功但最终 Closed 发布失败时，writer 已真实释放；不得把保留的
  管理 reservation 误报为仍持有 writer。持久化不确定时不会成功卸载或成功 shutdown。
- shutdown 拒绝新加载、并发结算全部 owner，确认 owner 和清单 resident 均归零后才关闭
  清单。没有 force-abort、虚假超时成功或自动重放；完整 reconciliation 管理 API 仍未接好。
- 内部 `with_administration` 让 owned callback 持有不能作为 `'static` 结果逃逸的借用
  reservation，覆盖 journal prepare、native 动作及 finish。reserved restore/cancel/close
  不重复获取公平读锁；已经接纳的事务不因随后 stopping 信号被中途撤销，shutdown 等待
  整个 callback。callback 内不得改调会再次预占的 public 方法。
- 内部 Stop 独占预占本身不 drain、不设置 stopping；可信 fresh owner 才应调用 `drain`，
  成功后取得 `DrainedService`，保留 catalog 与独占 fence 以发布 NativeSettled，然后
  `close_storage`。发布失败可留下 stopping/open catalog；原无条件 shutdown 仍可重试。
  token 仅证明 native settlement，不授予权限/fresh 资格，也不强制回执已同步；上述原生
  facade 与 daemon native handle 已按此顺序装配；五项受控执行 IPC 已接，Stop transport退出排序见本轮说明。4 项真实 Harness/JSONL/SQLite 回归及额外三轮通过。
- wire v1 是 `rpi-shared-host`，不是 native session RPC。Host/epoch/Agent/request/call/ticket
  各自独立，call 仅关联回复。帧最多 1 MiB，页最多 128 条；协商 in-flight 上限不超过 64，
  当前客户端每条连接只串行一个交换。新只读 server 明确协商 1；默认 8 个连接任务，
  实际连接/帧/读写期限预算见 daemon 说明，不把 wire 上限误报为运行时并发数。
- 客户端只接受显式 endpoint/Host ID/token。验证私有 Unix 目录/socket 与 peer UID；每次
  交换使用整体 deadline，先检查帧长后分配，取消中的连接丢弃，不复用残帧。
  无自动发现、进程 spawn、透明重试、后台 reader 或 transcript 读取；detach 不 close Agent。
- mutation 尝试发送后发生超时/断线/坏回复，返回 `OutcomeUnknown` 和原 request ID；
  不能自动重发。应重连并用 Agent 级 `receipt` 命令查询原生 admission；查询结果是日志
  事实，不是“操作已执行”或可安全重试的回执；仍不自动重放。token 和原始 peer/OS 数据
  不进入客户端诊断。
- Provider hooks 新增显式 `ExtensionProviderContext`；Host binder 应为每个 Agent 建独立 hook
  缓存，即使共享 DLL registry。显式 `session_id: None` 不读取进程环境；旧单 Session CLI
  仍保留动态环境行为。这里只消除了该 hook 的全局身份依赖，尚未完成所有生产资源/凭据
  装配路径的并发隔离。

## 验证与性能口径

在 `pi-rust-wt-host` worktree 内运行，构建目录也放在该 worktree：

```bash
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-cli --lib host:: --locked --offline -j 2
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-cli --test host_control --locked --offline -j 2
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-cli --lib args::tests --locked --offline -j 2
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-cli --lib startup_diagnostics --locked --offline -j 2
```

当前两侧接入增量：Host **235 项通过**（本轮7个真实DLL Host场景及5个artifact测试，前轮3个instruction资源测试，前轮4个预检/装配边界测试，前轮1个Stop CLI unit，前轮6个Stop测试，前轮1稳定取消CLI unit，前轮2稳定取消IPC/协议，前轮6稳定输入取消、前轮4普通submit/receipt CLI unit、前轮5行政 CLI unit，前轮 5 项行政执行 IPC/协议/客户端、6 项行政只读 IPC/协议/客户端，另含 7 项 daemon 私有行政装配、12 项原生行政 facade、4 项行政 reservation/分阶段 Stop、
7 项 immutable profile 集合与 3 项新增
版本/legacy journal，另有 5 项 backend/IPC 同步故障、超时、取消，
新建失败/孤儿重启拒绝、6 项 native guarded lifecycle、15 项前序独立行政 journal、
1 项共享实例锁、5 项 service guarded 及 1 项只读 profile 独立环境回归测试）；
真实二进制 **28 通过、0 ignored**（新增冻结instruction+持久inventory/重启HTTP测试，含2个Stop+HTTP退出/重启原意图隔离，取消CLI+HTTP丢stdout/重启原文件对账与18坏目标/文件无连接、丢取消reply/真实HTTP Aborted/后续输入隔离及重启对账，gated HTTP下真实submit CLI早准入、输入冲突/重启不重放、14类坏输入无连接、真实行政CLI+HTTP、坏请求无连接/默认拒绝、丢stdout对账、动态生命周期+真实 HTTP、丢行政 reply 对账、Interrupted 历史查询、允许 profile 发现、动态 profile 不自动加载、15 类私有行政配置前置拒绝、
13 类无效 profile 创建前拒绝（含6类资源拒绝）、SIGKILL-after-admission/start 重启不重放，以及 profile + HTTP Provider + IPC submit/receipt），
extcore→最终真实 daemon 必需产物 **1 通过、0 ignored**。
此前 native binding 全套另重复三轮通过。前序工具库完整回归（本阶段未重跑）**240 通过、0 ignored**（新增
exclusive-create 5 项），另 **4 doc tests**；Runtime **73 通过**（前序）；Harness
**542 通过、4 ignored**（含原生 admission 12 项与 exclusive-create 10 项）；
核心 extensions 库 **242 通过、1 ignored**（前序）。此前显式构建并选择
`plugin-stub` DLL 后，必需产物的 smoke **3 通过、0 ignored**，包括 8 个并发绑定共享
同一真实 DLL 的身份往返；未拿可能跳过的默认 smoke 充当验收。extcore 新增控制合同测试
**20 通过**，其完整回归结果见 [分阶段接入记录](../../../../docs/architecture/shared-host-control-integration.md)。

此前 main 集成轮分别 **56（含 15 项普通后端）、41、6 项通过**。Harness 库 **509 通过、4 ignored**，
定向集成 **88 通过**；上游 registry-aware discovery、tree preview 另 **16 项通过**。
Python fixture **19 项全部通过**，显式启用当前 release 二进制的六模式 smoke。
完整 workspace 未重跑；合入版本、review 发现、具体套件见
[main 集成记录](../../../../docs/architecture/shared-host-main-integration.md)。

此前两轮 600-Agent release 执行底座测试见
[性能报告](../../../../docs/architecture/shared-host-worktree-performance.md)；0.27.0 集成后的 600/3000 复测见
[600 报告](../../../../docs/architecture/shared-host-integrated-performance.md) 与
[3000 报告](../../../../docs/architecture/shared-host-3000-performance.md)。
这些探针不经过本模块的 catalog/ordinary 后端或 IPC，不能重标为生产 Host/mini 的
验收数据。后续须对接好的真实服务路径重测，不宣称统一内存上限。
