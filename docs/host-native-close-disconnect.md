# #23 受信 profile 动态生命周期收口：原生关闭断连与完整意图对账

## 结论与范围

**#23 的受控动态生命周期合同完成**。既有真实 binary+HTTP 动态 create/restore、稳定输入取消、CLI 原意图/丢 stdout、关闭失败/显式 shutdown 重试及 Stop/重启证据，本阶段在完整 39 项套件重新执行；新增一例含两个变体的 **实际 binary+C 原生 hook DLL+HTTP** 闭环，直接覆盖此前欠缺的 native callback 期间关闭调用方断连及恢复后原意图不重放。

本阶段仅新增集成测试和模块声明、文档；**没有改生产实现、四合同、权限、默认行为、workspace crate、Cargo 锁或依赖**。使用已有 sha2 记录实际私有测试 DLL 的 SHA。CLI artifact 与上一阶段相同。不是把 SDK/unit 组件测试拼成完整运行。

**整体薄 Host 未完成**：#14 的 mini/审批/定时器及完整默认 CLI/TUI 接入、默认一按需 N、后台 daemon/整组恢复和具体 watcher/helper/OS 资源清理仍独立推进。显式 `host serve` 已存在，不等于完整后台 daemon 模式交付。

## 新增的同一三方链路

`crates/rpi-cli/tests/host_control/native_close_disconnect_tests.rs` 的一例测试依次运行 `tool-result` 与 `provider-response`：

1. 编译真实 C DLL，声明既有 lifecycle/tool-result，Provider 变体另准确注册三个 observer。C pthread worker、callback gate、begin/finish/pthread_join 与 destructor unmap 标记均为实际本地代码，不用 test binder。
2. Host 只配置动态 allowlist；发现 profile 后 owner=0 且没有 native registration。使用固定 grant 的 **CreateProfile** 原意图实际 owned strict 装配，然后 Submit，接到真实 HTTP。tool-result 返回 read tool call 并进入真实 T 回调；Provider 变体返回成功 SSE 并进入真实 A observer。两者均检验自己的 Agent/Session，不用 ambient 身份。
3. 回调尚未返回时，以原 submitted_request_id 和当前 guard 显式执行 **CancelSubmitted**。严格验证原请求与 `CancelSignalled=true`，同时确认 callback 未返回、未 begin cleanup；不能把信号回执当真实结算。随后重新观察当前 revision，创建另一个明确的 Close 意图，而不是修改/重试已有意图。
4. 等查询到 Close 的 **durable Prepared**，才 abort 并 **await join** 关闭调用方。`HostClient::call` 把 UnixStream 移进自身 future；join 完成证明该 socket 随取消 future 实际 drop，不是只停止 poll。没有固定 sleep 猜测 server 已接收。
5. 使用全新 peer 查询同一完整 scope/request_id/epoch/action/agent/revision。在 callback 与实际 finish 的两个阶段，均严格验证 Prepared、owner=1、resident=1、两秒内 IPC 可响应，**实际 OS writer lease 获取失败**。callback 未返回不 begin；finish 已 pthread_join、尚被 gate 阻塞时也不能 unload/release。
6. 放行后通过新 peer 的原意图查询观察 **Succeeded/Closed**，验证 tool `T→D→S→B→W→E→F→U` 或 Provider `A→a→B→W→E→F→U`，每代 register/begin/join/finish/unload 一次、staging 空、owner/resident=0、物理 writer 可重新取得。丢失的原 Close ACK 从未用于断言成功。
7. 完整重发原 Close，结果相同。显式 SIGTERM 并 wait/reap，再启动新 epoch；动态 profile **仍不自动加载**。使用新独立 Restore 意图和新版本批准引用恢复准确 Session/真实 DLL，第二代 register 后，重发原 Create/Cancel/Close、当前 Restore 及原 Submit。历史回执相同，新 Live owner/resident=1、实际 writer 仍被持有；无新 HTTP、原 JSONL 字节不变、无新增 native 事件。最终显式停自建 Host 并验第二次正常 unload。

测试记录每次实际 DLL SHA、原完整意图、两阶段 Prepared/owner/resident、Create/Cancel/Restore/Closed 回执、新 epoch 和 unload 次数。只输出测试身份与 receipt，不输出配置 token、Provider key 或原始 hook header payload。

### 明确不夸大

- 实际参与的是**被 Host 装配的 C hook DLL**，不是 `rpi-host` 管理扩展 DLL。管理扩展交付与原结果通道的历史证据保留，但未冒称本次新场景的第三方。
- destructor 标记+custody 结算不是 OS VM-page 审计；受信 DLL 不是沙箱。既有 retained 负向仍保守持有，不会因本例 cooperative finish=0 而报全部 DLL 可强制清理。
- 现有 AdministrativeStop 的运行中 HTTP/丢回执/重启场景没有阻塞 C hook；本报告不声称已测完所有命令×hook 的笛卡尔积。#23 具体合同和迟到 native 关闭断连闭环通过，不等于额外交叉组合全部有证据。
- 不新增任意 `load(path)`/无授权恢复、legacy 裸 Cancel/Stop 或 actor 可提供凭据的路径；受信现有 Session 加载由已批准的 RestoreProfile 表达。旧意图先对账，新的 Restore 是独立明确操作，不是旧请求自动重放。

## #23 验收矩阵（均在本次完整 binary 套件实际重跑）

| 合同 | 实际 binary 用例/证据 |
|---|---|
| 动态 create、准确 Session restore、旧 profile version/epoch 不授新执行 | `actual_binary_dynamic_profile_http_close_restore_and_historical_execute_do_not_replay`；本次新增同场景还覆盖真实 DLL create/restore |
| 丢行政 reply，不重新创建 Session | `actual_binary_abandoned_administrative_reply_is_reconciled_without_recreating_session` |
| stable submitted_request_id 取消、后续输入不误伤、CLI 丢 stdout/重启不重放 | `actual_binary_stable_cancel_lost_reply_aborts_http_without_retargeting_or_restart_replay`、`actual_binary_cancel_cli_preserves_original_intent_after_lost_stdout_and_restart`；新增 DLL callback 中 CancelSignalled 不作结算 |
| native 迟到 callback/finish、关闭 caller 真正断连、原意图查询/新代际不误关 | **新增测试的 tool-result、Provider 两个变体**，同一次 binary+DLL+HTTP 链路，不是 SDK 拼接 |
| Closed 最终发布失败、Uncertain、保 owner/resident 与实际 writer 状态、显式 shutdown 重试 | `actual_binary_close_failure_retains_owner_and_shutdown_retries_only_on_explicit_signal` 的实际 SQLite trigger 注入、第二个 SIGTERM、重启原失败意图 |
| Stop NativeSettled 的 reply/退出排序、丢 reply/stdout、重启旧 Stop 不关新 owner | `actual_binary_stop_flushes_native_receipt_then_exits_and_history_does_not_stop_restart` 及其 lost_reply/CLI/lost_stdout 三变体；NativeSettled 不等于 peer 收到/进程退出 |
| 原意图/schema/私有有界输入、默认拒绝和不支持 capability | `actual_binary_administrative_cli_*`、`actual_binary_cancel_cli_rejects_*`、`actual_binary_stop_cli_rejects_*`，错误输入不连接/不提升权限 |
| Submit 原生 durable admission/receipt、答案不重放 | `actual_binary_submission_cli_admits_before_completion_and_reconciles_without_replay` 和坏文件不连接；新增同场景恢复后原 Submit 也逐字保持 |
| startup exclusive create、不截断已有主历史/companion、真实 writer custody | 本轮 CLI 中 `exclusive_create_partial_failure_is_uncertain_and_duplicate_cannot_replay`、`daemon_rejects_orphan_companion_on_create_and_restart_without_overwriting` 及 binary 启动重启实际通过；Session crate 的完整原子创建测试沿用 #24 历史，不冒称本次单独重跑；动态 DLL 创建不加绕行 |

## 本阶段门禁与版本演进

所有 Cargo 使用既有 worktree、`CARGO_HOME`、worktree target、`TMPDIR=/private/tmp`、`--locked --offline -j 2`。所有日志独立，不覆盖原失败证据。

| 日志 | 实际结果 / 版本范围 |
|---|---|
| `host-native-disconnect-focused-1` | 1/0/0，最早 startup profile 断连变体，尚非最终动态版本 |
| `host-native-disconnect-focused-{2,3}` | 各 1/0/0，动态 Create/Restore 版本，尚未加显式 CancelSubmitted |
| `host-native-disconnect-binary` | 39/0/0，上述中间动态版本，保留不替换最终 |
| `host-native-disconnect-focused-{4,5}` | 各 **1/0/0**，最终 Create/Cancel/Close/Restore 版本的独立重复 |
| `host-native-disconnect-binary-final` | **39/0/0、0 filtered**，最终源码、默认测试并行、nocapture 记录实际 DLL 和 receipt |
| `host-native-disconnect-unit` | **1538/0/3、0 filtered**，未过滤 CLI lib，两测试线程；最后新增 Cancel 仅在独立 integration 文件，生产/lib-unit 输入不变 |
| `host-native-disconnect-clippy` | 核心 lib/bins strict no-deps **exit 0** |
| `host-native-disconnect-test-clippy[-final]` | 整个 host_control 集成测试 strict no-deps **exit 0**，final 对最终含 Cancel 源码执行 |
| 定点 fmt/双树 diff/四合同/完整输入/index/旧报告保全 | 机器证据；累计 **23** 个改动 Rust 文件 fmt，新增测试无 allow/expect/ignore |

当前 CLI SHA256：`7c216d205260a31b1bd94ccf38872891b87ecd52a604a12483558d4341a02c76`。Agent **18套120/0/0**、Harness **546/0/4**、AI/SDK **63/0/0、259/0/1** 沿用源码不变的上一阶段结果，未伪装本轮重跑。

SDK 单独严格 Clippy 的 **15 项/exit101**、默认并行 CLI 旧 TUI fork/saved-read deadline 失败、此前 Agent 大字符串配额夹具失败均保留。当前 CLI 全库门禁是两测试线程，不是默认并行复现已修好；不称 workspace/all-targets/MSRV/跨平台全绿。没有重跑内存 benchmark、修改原 checkout、提交、安装、发布、部署或注册服务。

## 下一步

#14 继续从真实生产 mini 与权限/审批/定时器边界核对，接入原执行/SDK，不另造 Agent loop；默认薄 Host/后台 daemon/整组恢复及具体清理继续按独立可执行阶段验收。此前报告里的“#23 未完成”保留为历史状态，本报告和 README 顶部提供新收口，不改写历史。
