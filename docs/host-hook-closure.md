# #46 受限生产 hooks 收口 / #64 共享配额夹具稳定化

## 当前结论

- **#64 完成**：只改变一个 Agent 集成测试内的结果形状并增加大小界限，未改生产实现、原 64 MiB 共享树配额、原三秒 poll 期限或原权限/释放/计费断言。三次隔离均通过；最终默认测试并行的 `rpi-agent --lib --tests` **18 套、120 passed / 0 failed / 0 ignored**。
- **#46 按原任务的受限合同完成**：显式受信 DLL 冻结/staging、只读预检与 owned strict 全批次加载、完整声明/inventory、tool-result 与三个 Provider observer、逐 Agent 上下文、实际 late callback/finish/retained 的 owner/resident/writer 结算已贯通并验收。不是批准 ActionBridge/UI/插件工具或所有 SDK 注册。
- **不称 workspace 全绿**：SDK 单独严格 Clippy 仍有 15 项诊断；默认并行 CLI 曾有 TUI fork 和 saved-read deadline 失败，当前通过的未过滤 CLI 门禁使用两测试线程。没有把这次 Agent 夹具修正说成修复了这些其他问题。
- **整体 Host 目标未完成**：#14/#23、默认一按需 N 的薄 Host、具体 watcher/helper/全代际 dev 与受控 OS 资源、后台 daemon/配置化整组恢复仍独立推进。SDK/sessionless 单测不等于生产 mini 已实现。

前两份 `docs/host-native-hooks.md`、`docs/host-provider-hooks.md` 及其 JSON 按原样保存；它们的未完成/失败状态是当时快照，不能通过改写历史来关闭任务。本报告提供后续收口，而非替换原证据。

## #64 的诊断与最小变更

此前 `reference_shared_tree_quota_and_foreign_programs_are_enforced` 在默认并行、两线程、隔离和带 backtrace 的隔离运行均失败。调用栈指向外层等待整个 nested 调用的 `tests/tool_program.rs:2124`，其中包含多轮 18 MiB 结果处理。相关生产模块与测试源码当时均未改，config 的 `provider_hooks` 为 None；没有执行 before-Provider-source 构建，故不把静态对照当作完整基线运行。

本轮只对**自建隔离测试进程**采样，完成后 wait/reap。有效样本只有 **6** 个，活动线程栈位于 `serde_json::to_string(AgentToolResult)` 的大字符串 escape scan，不是等待 Provider hook/异步锁。样本量不足以统计时间比例或证明 release 吞吐，因此这里只用于定位后续单变量测试改动；不是性能/内存 benchmark，也不将 sample 自带的 footprint 字段纳入旧内存证据。未捕获采样前测试可执行文件 SHA，不能用后来重建的测试 binary 回填旧 artifact 身份。

冻结本轮输入后，仅修改 `crates/rpi-agent/tests/tool_program.rs` 中同一个测试的两处：

1. 将重复 `{"big":"x".repeat(18 MiB)}` 换为 `Value::Array(vec![Value::Null; 256 * 1024])`。
2. 保留原 status=done 断言，并增加首个 retained manifest 的编码长度 **>1 MiB 且 <2 MiB** 断言。

其他测试断言、poll helper、调用次序、原 metadata/usage=12、跨 Program reference 读/释放/解析拒绝、持有父引用时子配额拒绝、释放父引用后子准入、nested finish 后根再次准入全部保留。没有 sleep、放宽时间/配额/断言、skip/ignore 或 lint 豁免；生产源文件未改变。

### 为什么不是降低配额覆盖

既有 `tool_program_results.rs` 保持以下保守计费规则：

```text
charge = 2 * encoded_bytes
       + 4096
       + native_details_node_overhead
       + 256 * content_blocks
TREE_BYTES = 64 MiB
```

256k 个 Null 节点及数组根按原每节点 128-byte 预算贡献 **32 MiB + 128 bytes**。这里是 quota 的保守计费，不是实际堆、RSS 或物理内存。

由新增的实际 manifest 大小界限可知：单份 charge 约 34–36 MiB，能进入原 64 MiB 树预算；两份超过 68 MiB，不能共存。因此原“拒绝→释放父引用→再准入”断言仍验证同一共享树 quota。反过来，仅按编码字节计费的两份结果远低于 64 MiB，不会触发预期拒绝；新夹具还加强了对**漏掉原生节点计费**的覆盖。此为基于原规则和可观察断言的推导，未声称运行过 mutation test。

## 验证记录

全部 Cargo 使用既有 CARGO_HOME、worktree target、`TMPDIR=/private/tmp` 和 `--locked --offline -j 2`。

| 本轮验证 | 结果 |
|---|---|
| 结构夹具首次隔离 | 1/0/0，0.45s |
| 第二/第三次隔离 | 各 1/0/0，0.44s / 0.39s |
| 初次未过滤 Agent 库+全部集成 | 18 套、120/0/0 |
| 加入 <2 MiB 上界后的最终未过滤 Agent 库+全部集成，默认测试并行 | **18 套、120/0/0**；其中 ToolProgram **35/35** |
| 22 个累计改动 Rust 文件定点 fmt、两树 diff、四合同、输入/旧报告保全 | 见机器证据 |

上述时间仅是 test harness 的测试持续时间，不是 release 性能改善比例；整个 ToolProgram suite 仍包含其他大结果场景，不能把单例夹具时间当作全套加速。

本轮没有改动任何生产源码。以下 #46 验收直接引用未变化源码和相同 CLI artifact 的上一阶段结果，**没有伪装成本轮重跑**：

| #46 实际门禁 | 结果 / 原日志 |
|---|---|
| 实际 binary + C DLL + HTTP 全套，默认测试并行 | **38/0/0**，`host-provider-binary-final.log` |
| 未过滤 CLI lib，两测试线程 | **1538/0/3**，`host-provider-cli-bounded.log` |
| 完整 Harness lib，默认测试并行 | **546/0/4**，`host-provider-harness-regression.log` |
| AI/SDK lib，两测试线程 | **63/0/0、259/0/1**，`host-provider-ai-sdk-regression.log` |
| 核心 lib/bins 严格 no-deps Clippy | **exit 0**，`host-provider-cli-clippy.log` |
| SDK 严格 no-deps Clippy | **exit 101、15 项**，`host-provider-sdk-clippy.log`，不是绿色门禁 |

本轮 source freeze、具体命令/退出码、每套 Agent 结果、22 文件 fmt、原始/修正日志 SHA、current CLI SHA 与双树 WIP/index/旧证据不变证明见 `docs/host-hook-closure-validation.json`。不覆盖旧 validator/report。

## #46 合同收口矩阵

| 要求 | 当前生产接线与真实证明 |
|---|---|
| 批准与身份 | 私有 profile、有序 frozen bytes、per-incarnation staging、精确内置 tools、完整 inventory/metadata equality，未批准注册拒绝；纯冻结无 native registration/staging |
| 保存 authority | writer/repair 前读取保存 facts 并拒绝变更，首次 Provider/工具前持久化并严格同步 artifact hash、工具 schema digest 和 hook approvals；篡改不覆盖原 Session |
| 逐 Agent tool-result | 实际 read→原生 transform→下一次 HTTP，冻结 cwd/agentId/sessionId 只进插件 envelope，不把上下文元数据混入原参数或工具结果；显式 transform 的最终结果正常持久化 |
| 逐 Agent Provider observer | 双 Agent 各两次 request/headers/response，以既有 SDK 顶层 Agent/Session context 装饰；保留 ABI 和 legacy 同步适配，无 ambient identity fallback |
| shared executor | 只在既有 request/terminal 调用点 await；实际 blocking worker 取得 call scope，串行 guard 随 worker 保留，取消 opening 的等待者不能让 Aborted callback 越过迟到 before-request |
| late custody | 真实 C callback/finish 文件 gate 期间 IPC 可响应，owner/resident 保留，实际 OS writer lease 拒绝；按 callback→finish→unload 才结算 |
| 批失败/retained | 注册错误、metadata、tool/Provider count 和未批准事件均整批拒绝、已启动 worker cleanup；finish 失败保守保留 code/staging/owner/已取得 writer，不宣告成功 |
| HTTP/error/restart | 正常/工具循环/HTTP 400/取消三方验证；保存 inventory、冻结后替换源、旧关闭/提交原意图、重启均不重放未知副作用 |
| 文档/回归/保全 | 当前 Agent 120/0/0，既有完整 CLI/SDK/Harness 和 38 项真实 binary 结果按准确并行度保留；四合同逐字，双树 HEAD/index/WIP/旧证据保全 |

unload destructor 与 SDK custody 结算不被夸大为 OS VM-page 审计；受信原生代码不是沙箱。retained 私有负向子进程最后被显式 kill/reap 的事实仍保留，不冒称生产安全 cleanup。

## 后续边界

下一阶段先核对 #23 的三方生命周期闭环，尤其 native callback 期间**关闭调用方断连**与原意图 receipt 对账能否由同一真实 binary+DLL+HTTP 场景证明；不能只拼接独立 unit/SDK 测试宣布完成。然后继续 #14 真实 mini/审批定时器/Host CLI-TUI 和薄 Host 默认接入、daemon/整组恢复及具体资源清理，不改原请求或自动重放工具副作用。
