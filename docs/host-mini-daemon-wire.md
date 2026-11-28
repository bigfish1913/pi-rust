# Mini daemon / wire v1.6 阶段验收

## 结论与范围

本阶段接通 **显式 opt-in Mini daemon → authenticated IPC → 真实管理 DLL → HTTP Provider**。
Mini 复用既有 raw `rpi_agent::Agent`、唯一 `HostService` / `HostRuntime` owner 表与
`NativeExtensionCustody`，不新建 crate、依赖、第二 Agent loop、Harness、AgentLane、Session、
JSONL 或 writer。Persistent 管理与 durable receipt 路径不替代为 Mini 易失结果。

这不是 #14/#65/#66 全部完成或生产全故障 qualification：迟到 native hook callback/finish
的 gated DLL 已经过真实 Rust daemon 回归，但**没有装进本次 Python 管理 DLL 单链路**。
仍需将该边界与断连/故障组合收敛到同一实际 binary + 管理 DLL 链路。
默认薄 CLI、审批/定时器、后台服务/整组恢复以及 #65 的严格 Agent scoped Clippy 仍未完成。

## 接口与权限

- Wire minor 1.5 → **1.6**。`protocol.rs`、`client.rs`、`client_tests.rs`、
  `fixtures/control-v1.json` 在两个 worktree 保持 byte-identical。
- 私有 daemon 配置的 `mini_control` 独立于 Persistent administrative grant：固定 scope、
  startup Mini allowlist、submit/cancel/close permissions（默认全拒绝），操作容量1–64。
  不接收 peer 自授权限；未启用时在 catalog/Session 创建前拒绝 Mini，不公布 capability。
- `mini_execute` 接受完整原 epoch/request/action；`mini_receipt` 携带同一完整原意图对账；
  `mini_result` 固定 scope/epoch/Agent/input/run，并提供4–16384 bytes的 UTF-8 页。
- 操作与结果明确 `durable=false, volatile=true`。Submit succeeded 只证明易失 admission，
  cancel_signalled 不证明结算；close succeeded 才证明 native settlement 和 Lost 发布。
- 相同 ID/意图不重放，不同意图冲突；容量耗尽不驱逐旧记录。不将丢回复改写为新的请求。
- 重启保留 catalog 的 Mini 身份与 Lost 状态，但不恢复 Agent/input/output、不 autoload/replay。
  原 epoch 查询 unknown；原意图执行 StaleEpoch。不会自动重写 epoch/UUID。

## 实际单链路与回归

使用明确的 worktree 构建工件，无 PATH/安装版本 fallback、无内存 benchmark：

| 验证 | 结果 |
| --- | --- |
| CLI 未过滤 lib（2线程） | 1556 passed / 0 failed / 3 ignored |
| 实际 binary `host_control` | 39 passed / 0 failed / 0 ignored |
| extcore lib（含镜像合同） | 407 passed / 0 failed / 2 ignored |
| 管理扩展 lib | 20 passed / 0 failed / 0 ignored |
| CLI lib/bins/完整 host_control scoped strict Clippy | exit 0；无新增 lint 豁免 |
| 管理包 all-targets no-deps strict Clippy | exit 0；仅沿用已批准 SDK export 宏命令行例外 `-A clippy::not_unsafe_ptr_arg_deref` |
| core workspace/all-targets check | exit 0；保留 manifest unused-dependency warnings |
| core workspace fmt / 两个扩展包 fmt / 两树 diff check | 通过 |
| 真实 Mini binary + 管理 DLL + HTTP | **43 次管理调用 / 7 次原生请求 / 9 项场景** |
| Persistent basic / administrative | 7/1 与 21/1（管理调用/原生请求） |
| Persistent errors | 10 cases；含 controlled fault peers，不冒称全是真实 Host |
| Persistent results | 31/6；9 项结果场景 |

Mini 单链路覆盖：明确 capability/唯一 owner 表；准确 UTF-8 多页答案；同意图无重放及
scope/Agent/run 隔离；真实内置 read 工具往返与 `{{...}}` 不展开；HTTP 失败不涂绿；
活跃取消只 signal；显式 close 结算后 Lost；fixture Host SIGKILL/reap 后重启 Lost、unknown、
无 autoload/replay；Mini 工作目录的文件内容、目录、符号链接检查与全 fixture 无 JSONL。

管理 CLI 和 Mini 使用**分开的 cwd**。管理 CLI 的现有资源 lease 在其自身 cwd 内，
不是 Mini 创建的 Session 或文件。所有 fixture 进程、RPC reader、HTTP 线程与临时目录回收；
SIGKILL/stale socket 清理仅针对 fixture 自己持有并确认退出的进程，不证明生产后代回收。

实际 Mini gate 的 Rust daemon 测试另覆盖：受信政策预检、默认拒绝/旧 epoch、完整意图
去重/冲突、有界易失操作、cancel ≠ settle、close socket observer 断开后 owned work 继续、
真实 native DLL callback/finish 双门控保留 owner/custody、事件 `RABEFU`、unload 后 staging
为空、`sessionId=null` 与重启 Lost/no reload。该测试不是管理 DLL 单链路的替代品。

## 工件与证据

- Core binary: `/Users/bigfish/work/rpi-projects/pi-rust-wt-host/target/debug/rpi`
  - SHA256 `026cc40a77061d006e36f43a3911455a6c791b83929e77bb58a5495ad2015d07`
- Management DLL: `/Users/bigfish/work/rpi-projects/rpi-extension-wt-host/target/debug/librpi_host.dylib`
  - SHA256 `39cb5fd04d2b998fcb032d1b6dda40d62e282080691ce5957a3f0caed5f3f281`
- 证据目录：`/tmp/rpi-host-integration-checks/mini-daemon-wire-20261124T231922388222Z`
  - `before.json`、初始 WIP patches/archive、每次独立 `.log/.exit`。
  - `mini-wire-native-mini-4.json` 为最终加强后的单链路结果，原始 tool events 保留。
  - `final-mini-wire-source-snapshot.json`、`validate-mini-wire-final-v2.py`、
    `validation-mini-wire.json` 记录最终源 hash、工件 hash、四合同和范围校验。
  - [可追踪机器摘要](host-mini-daemon-wire-validation.json)。历史报告与失败 validator 不覆盖。

## 失败记录与未完成项

1. 扩展测试首次 406/1/2：旧 historical receipt 测试依赖 golden 数组最后一项；新增 Mini
   fixture 后假设失效。改为按 `administrative_receipt` 操作选择原 fixture，保留旧 epoch
   不等于 transport epoch 的原断言，之后两仓完整测试通过。
2. 管理扩展 schema 测试原先固定13项命令。新增3项 Mini 后更新为完整16项集合，并断言
   Mini request/action 闭合；保留未知字段/凭据不能从模型侧覆盖等检查。
3. 初次 Mini 单链路在工作区精确比较处失败；诊断运行确认是在任何管理工具调用前，
   管理 CLI 启动创建了 `.rpi/.resource-lease`。分离 cwd 后保留严格 Mini before/after，
   再加强为包含目录和禁止符号链接的树快照；两次通过均有单独记录，不删除先前失败。
4. 扩大到 `-p rpi-extcore -p rpi-host --lib` 的 strict Clippy **失败，3项诊断**：
   `prompt.rs` 的两项 useless_conversion，以及镜像 protocol 中此前已有的
   `is_none_or` 与 extcore MSRV 1.78 不匹配。已验证诊断表达式存在于 HEAD，未重跑 baseline，
   **不宣称验证了 baseline 构建失败**。未修无关代码或新增豁免；scope 记录在独立 JSON。
5. 上阶段扩大的 CLI 全测试 strict Clippy 的42项诊断、#65 Agent strict scoped Clippy
   阻塞仍保留；本轮通过的 CLI scoped Clippy 不能替代它们，也不能宣称 workspace strict
   Clippy、所有测试目标或全部平台全绿。native DLL daemon 回归为 Unix，本轮实测 macOS。

6. 最终 validator v1 错把 tracked `mod.rs` 中既有的 lint attribute 当作新增而失败，未写
   最终输出。已确认 attribute 存在于 HEAD；v2 校验所有新增 diff attribute 与 untracked
   Rust 文件，不修改生产属性或放宽新增豁免禁令。v1、日志与 guard 故障分类全部保留。

只修改两个已有 Host worktree；未 commit/push/merge、发布、安装、部署、注册服务或修改锁包。
