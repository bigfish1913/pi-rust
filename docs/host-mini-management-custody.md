# Mini：管理 DLL / native DLL custody 同链路验收

## 本阶段结论

上阶段尚缺的 **实际 binary → 管理 DLL → 认证 IPC → raw Mini → 实际 native-hook DLL → HTTP**
迟到回调、断连及关闭结算验收现已接通并通过；同时补充 callback 中途崩溃与 SIGTERM drain。
未改生产 Rust、四份 wire 合同、SDK、依赖、锁包或 CLI 默认路径。唯一基础夹具调整是
`MiniFixture.write_profile` 受信 profile 注入点；新增 `native_mini_custody.py` 复用原装配，
不引入第二 owner/loop、Session/Harness/AgentLane/JSONL/writer。

**这不是所有验收全绿或 #14/#65/#66 结项。** 实际 Agent scoped strict Clippy 仍失败4项，
extcore 本轮未过滤测试亦有1项 startup ownership 失败。保留日志、未重试失败项、未修无关
代码、未新增 lint 豁免。管理包自身20项通过不能覆盖 extcore 全库失败。

## 同一管理链路的三种实际故障

所有管理命令与结果通过真实 `host` 工具读取，不直接读 Session/JSONL 取答案。
两个 Mini 使用同一 Host、同一 owner 表；native hook 仅显式授予 Agent A 的 afterResponse。
Agent B 在 A 的 callback 阻塞期间完成真实内置 read 工具与 HTTP 答案往返。
两者的工作目录与管理 CLI cwd 分开，Mini 树的文件/目录/符号链接与全 fixture 无 JSONL
检查均保留。native 事件 payload 明确断言原 AgentId、`sessionId=null`。

| 模式 | 管理调用 / HTTP 请求 | 核心实际断言 |
| --- | --- | --- |
| close 回复丢失 | **30 / 4** | 原样转发真实认证帧，只丢一次真实 Host 已完整返回的 close 回复；DLL 给出 `outcome_unknown` 与完整原意图 MiniReceipt，durable=false，无自动重试；callback/finish 双门控保留 owner/resident、mapped DLL 与 staging；最终 `RABEFU`、staging 清空、Lost 后才 close succeeded |
| callback 中途 SIGKILL | **18 / 3** | 只杀并 reap 自持 Host PID；重启0 owner/resident、两 Mini Lost；旧 submit/cancel 查询 unknown、执行 StaleEpoch，无 Provider/DLL replay；事件仅 `RA`，旧 staging 文件 hash 保持不变，不宣称 native finish/unload |
| callback 中途 SIGTERM drain | **17 / 3** | 同一管理 DLL 观察 stopping、Host 仍活着、owner/resident 与 staging 保留；callback 放行后 `RABE` 仍不可退出，finish 放行后 `RABEFU`、staging 清空、Host exit0并自行移除 socket；重启 Lost、不 autoload/replay |

私有 proxy 只转发实际 Host 帧，不生成 Welcome/Reply、不改认证或请求 epoch/UUID，且不记录
握手 secret。默认模式验证的是真实 Host close reply 丢失，不是 controlled fake peer。
显式重复原 close 只是去重探针；丢回复后的第一刻另断言仅发送一次原 mutation，不能将探针
冒称客户端自动重试。进程终止全部仅针对 fixture 保存的 Popen，绝不按名字匹配杀进程。
SIGKILL 后只在确认 reap 之后清理 fixture 私有 stale socket；没有生产自动恢复结论。

C fixture 的 Event/PluginApi layout 来自显式提供的**本 worktree SDK 源码**：直接 rustc
编译既有 std-only ABI 与临时 layout probe，生成真实 offset/size/tag 后编译 C DLL。
不新增 workspace crate/Cargo manifest/依赖，不读取安装版 SDK、不猜测 ABI 常数。
当前 layout ABI4 / API216 / payload56 / event64 / payload offset8；lifecycle160、event80、tag14。
fixture 的临时 `CARGO_PKG_VERSION` 只满足 SDK HOST_VERSION 编译常量，不参与 ABI、生产版本或发布。

原生 DLL、C source、layout source/JSON、compiler log/exit、原始 hook context/事件和崩溃后的
staging 残留 hash manifest 保存在本阶段私有证据中。所有 fixture RPC reader、HTTP/proxy
线程、编译进程、管理 CLI 与 Host 回收；SIGKILL 不等于 native graceful settlement。

## 回归与真实阻塞

- 基础 Mini 单链路仍 **43/7，9项场景通过**。
- Persistent basic **7/1**、administrative **21/1**、errors **10 cases**、results **31/6、9场景**
  全部通过。errors 的 controlled peers 仍只作为错误分类探针，不冒充真实 Host。
- 管理包 `cargo test -p rpi-host --lib`：**20 passed / 0 failed**。
- CLI lib/bins/完整 host_control scoped strict Clippy：exit0；管理包 all-targets no-deps strict
  Clippy：exit0，唯一沿用已批准 SDK export 宏命令行例外 `-A clippy::not_unsafe_ptr_arg_deref`。
- Core workspace fmt、两个扩展包 fmt 与两树 diff check 通过。没有重跑内存 benchmark。
- 本轮生产 Rust 未变，上阶段 CLI **1556/0/3**、host_control **39/0/0** 是**历史已通过记录**，
  不是本轮重新执行的全库测试；本轮新增覆盖是上述实际管理故障链路。

### #65 实际 strict Agent scoped Clippy：仍失败

```sh
cargo clippy --locked --offline -j 2 -p rpi-agent --lib \
  --test agent_builder_hooks --no-deps -- -D warnings
```

exit101，共4项：

1. `crates/rpi-agent/src/agent.rs:563` — unwrap_or_else 默认值。
2. `crates/rpi-agent/src/agent_loop.rs:958` — doc list continuation。
3. `crates/rpi-agent/src/agent_loop.rs:1749` — single_match。
4. `crates/rpi-agent/src/events.rs:23` — large_enum_variant。

这些源文件与阶段开始快照相同，但未另做 baseline checkout 构建，不冒称复现 baseline 失败。
本轮不为过 lint 擅自给公共 AgentEvent 加 Box/更改 API 或添加 allow；#65 不标完成。

### extcore 未过滤回归：406/1/2，失败未重试

```text
host::startup::tests::unix::early_exit_reports_native_stderr_and_releases_launch_ownership
packages/rpi-extcore/src/host/startup_tests.rs:219
native session is already owned or its cleanup is still in progress
```

`cargo test -p rpi-extcore -p rpi-host --lib` 在 extcore exit101，未继续执行管理包，因此另运行
管理包自身测试并如实分开记录。失败文件未修改；未证明 baseline/flaky 成因，不能以无 diff
或历史其它 child ownership 失败代替因果分析。没有 retry-to-green，也未把这次全库失败改成成功。

首次新增 fixture 运行另因临时 rlib 文件名未以 `lib` 开头而在 layout probe 编译阶段失败，
尚未启动 Host/HTTP/管理 CLI；修正为 `librpi_plugin_sdk_layout.rlib` 后重新运行。原始失败和
compiler `.log/.exit` 保留，未覆盖任何旧阶段 validator/报告。

## 工件、范围与机器证据

沿用上阶段已构建、显式指定且逐次核对 hash 的工件，**没有为了本阶段纯测试改动重建版本**：

- Core `target/debug/rpi` SHA256
  `026cc40a77061d006e36f43a3911455a6c791b83929e77bb58a5495ad2015d07`。
- Extension `target/debug/librpi_host.dylib` SHA256
  `39cb5fd04d2b998fcb032d1b6dda40d62e282080691ce5957a3f0caed5f3f281`。
- 本阶段证据：`/tmp/rpi-host-integration-checks/mini-custody-chain-20261125T005911692076Z`。
  - `before.json`、两树初始 patches/WIP archive、上阶段证据 SHA256 清单。
  - 最终 `native-mini-custody-final-6.json`、`native-mini-custody-crash-7.json`、
    `native-mini-custody-drain-5.json` 及各自 `*-fixtures/`，原始 tool events 与资源回收断言。
  - `remaining-gates-classification.json`、每次单独 `.log/.exit`。
  - `validate-mini-custody-final-v1.py`、`final-mini-custody-source-snapshot.json`、
    `validation-mini-custody.json` 审计最终范围、四合同、工件与原证据未变；审计通过不等于失败测试通过。
- [机器摘要](host-mini-management-custody-validation.json)。

本阶段仅两个现有 worktree 的测试/README 与新增报告；HEAD/index 不变，四合同仍 byte-identical，
不新增第二 owner、持久输入/结果 journal 或恢复机制。没有 commit/push/merge、发布、安装、部署、
注册后台服务。#14/#65/#66 继续进行中；下一项真实阻塞是 Agent strict Clippy 与 extcore startup ownership，
不是重复已完成的 Mini owner 或 wire 接线。
