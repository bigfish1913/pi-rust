# 真正无 Session mini 的 AgentBuilder hook 接线前置

## 当前结论与范围

**这不是生产 Host mini 的交付报告。** 本阶段仅消除原生 `rpi_agent::AgentBuilder`
不能安装现有 loop hooks 的装配限制。生产 mini、权限审批/定时器、默认 CLI/TUI 薄 Host、
daemon/整组恢复和具体资源全域清理仍未完成；#14 与 #65 保持进行中。

`AgentHarnessOptions.session` 是必填项，其默认值也创建内存 Session。
因此不能通过默认 Harness 或 `InMemorySessionStorage` 冒充“真正无 Session”。
后续 mini 将复用原生 `rpi_agent::Agent`，而不是创建第二个 Agent loop、AgentLane、
Session facade 或 transcript 权威。

## 最小变更

- 在 `crates/rpi-agent/src/agent.rs` 的 builder/Inner 私有字段中加入可选 callback 表，
  以三个增量 setter 将现有 `before_tool_call`、`after_tool_call` 与 `ProviderHooks`
  原样交给原 `AgentLoopConfig`。未提供 hooks 时不分配 callback 表。
- `AgentOptions` 的公开字段保持逐字不变，并由不带 `..Default` 的完整结构体 literal
  回归测试锁定兼容性。Agent loop、AgentLane、SDK C ABI、事件布局均未改写。
- Provider setter **只透传原循环的终端 observer**。`before_request` 仍是 stream factory
  的责任，不能因为 setter 接受同一 trait 就宣称自动接通 request/header hooks。
- 新增 `crates/rpi-agent/tests/agent_builder_hooks.rs`，不调用 Harness/Session 构造器。
  所有异步测试使用 current-thread executor；原有 shared test helper 文件未改。
- 未改生产 Host profile/binder/supervisor、wire/合同、extension tree、Cargo manifest/lock。
  默认 callback 语义仍是三个 `None`；没有新增 lint allow/expect。

## 本轮实测

| 门禁 | 结果 | 限定 |
|---|---:|---|
| 新增 AgentBuilder 集成测试 | 3/0/0 | SDK 回归，不是 mini binary 验收 |
| 未过滤 Agent `--lib --tests` | 19 套，123/0/0 | 最终独立运行，实际 exit 0 |
| 未过滤 Harness `--lib` | 546/0/4 | 实际 exit 0 |
| 未过滤 CLI `--lib` | 1538/0/3 | 两线程；最终独立运行实际 exit 0 |
| 完整 `host_control` binary integration | 39/0/0 | 默认测试并行；仍是 ordinary 生产路径 |
| CLI lib/bins/整个 host_control strict no-deps Clippy | exit 0 | 不代表依赖或 workspace strict 全绿 |
| Agent lib+tests strict Clippy | **exit 101** | lib 4 类、lib test 5 类既有位置诊断，未修 |
| Agent lib+新增 test 普通 Clippy | exit 0，有 warnings | lib 4 类，原 common helper 另 1 类；不是 strict 通过 |

新增测试覆盖：

1. `AgentOptions` 完整结构体 literal 仍编译，不需要新增字段。
2. 真正 raw Agent 在 `session_id=None` 下经过多轮工具调用，stream factory 借用跨 yield
   仍为 None；原输入 `{{literal}}` 不改写。已有 before hook 可批准、改 args 或 veto；
   结果 hook 可转换成功/工具异常结果，保留原 veto 不执行、不重复转换的语义。
   三个 setter 都验证 last-wins，旧同步 terminal observer 仍通过默认 async 适配工作。
3. terminal/result-only fallback/Provider error/abort 四种路径下，真实 blocking worker
   持有回调时，单线程 executor 的 timer 仍可运行；prompt 不完成，assistant MessageEnd
   与 AgentEnd 不提前发布。释放 worker 后仅一次终端发布，borrow 仍有效。

严格 Agent Clippy 的诊断位置是原 `abort_token` 的 `unwrap_or_else`、原 loop 文档缩进、
原 after-result `single_match`、原 `AgentEvent` 大枚举、原 loop 测试模块位置。
普通 Clippy 还报告未修改的 common helper `collapsible_match`。
这些代码片段/文件相对本阶段冻结输入未改；**没有运行阶段前 strict 构建基线，静态保全不冒充
阶段前运行复现**。不为刷绿改 SDK 事件布局、重写循环或加豁免；#65 的 strict 通过条件尚未满足。
此前 SDK 15 项 strict 诊断和默认并行 CLI 旧失败仍作为历史阻塞保留，没有本轮重跑或宣布修复。

## 运行与证据说明

构建均使用既有 CARGO_HOME、worktree target、`TMPDIR=/private/tmp`、`--locked --offline -j 2`。

- 首次 Agent 全套与后续 lint 的组合调用触及 120 秒工具等待期限；日志已有全部 suite 结果，
  但没有捕获 Cargo 退出码。检查完整进程树无存活后，使用新日志重跑完整 Agent，最终 exit 0。
- 首次 Harness/CLI 等组合调用触及 600 秒工具期限；Harness 已捕获 exit 0，CLI 仍存活。
  未重启/杀掉它，等实际完成（1538/0/3）且无进程后，再用独立 runner、新日志复跑 CLI。
  runner 对每个门禁单独写实际 `.exit`，最后正常 exit 0，无残留自建 Host/构建进程。
- 保留两个超时调用及两个 strict lint 失败日志，不补写虚假的 Cargo exit 0。
- validator 初版把全机另一项 Cargo 构建误列为本轮残留；失败版本/日志保留。
  v2 按两 worktree cwd、明确 runner 和实际 target-cache namespace 判归属，
  无法归属的存活进程保守拒绝验收，不停止或干扰别的构建。
- 新 CLI debug binary SHA256：
  `2859963c801c706868e91f35ef3ba93efaf2572a699fb8a155b394bcf2dfd7ed`。
  不将旧 20-Agent 内存表绑定到新 artifact，不重跑内存 benchmark。
- 阶段输入：`/tmp/rpi-host-integration-checks/host-mini-agent-hooks-before/inputs.json`。
  原两树 WIP/HEAD/branch/index、旧报告和 818 份既有 evidence 的保全由独立 validator 检查。
  [最终机器证据](host-mini-agent-hooks-validation-final.json) 记录精确命令、hash、合同及边界。
  v2 初版将 5 个 example/fixture 的 src 文件混入源码统计，原文件保留；最终版本严格按
  `crates/*/src/**/*.rs` 复核：599 份生产 Rust 中 598 份未改，仅 Agent builder 文件变化。

## 下一阶段实际接线，尚未实现

- 在原唯一 HostRuntime/owner coordinator 内分流 Persistent 与 Mini，复用现有监督/取消/关闭，
  raw Agent 作为 mini 的唯一会话内上下文所有者；不另建 Agent loop 或平行 owner 表。
- 从 profile 的显式 mode/session 组合开始闭合校验；mini 不能构造任何 Session、JSONL、
  companion 或 writer，也不能通过 default Harness options 暗中创建内存 Session。
- 显式闭合 Provider/工具/资源配置；native hook 上下文的 session 必须为 null，不回退 ambient。
  late callback/finish/retained 的 owner/resident custody 必须真实维持。
- mini 的 admission/result 只在明确 epoch/owner lifetime 内有效；不能返回 ordinary 的 durable
  Session receipt，不能把 mini 的内存回执称 durable，不能恢复 Lost mini 或重放旧工作。
  若新增 wire capability/类型，必须同步两树四合同及可选管理扩展的实际行为。
- 在新阶段冻结两树输入后，使用真实 binary+DLL+HTTP 单链路验收无 Session/文件副作用、
  取消/断连/迟到结算、Lost/重启不重放；不能把本报告的 SDK tests 和 ordinary binary tests
  拼作 mini E2E。
