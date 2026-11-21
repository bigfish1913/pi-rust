# Shared Host：合入 main、性能代码 review 与普通后端回归

## 集成范围与保护

- 工作树：独立 worktree `pi-rust-wt-host`，分支 `feat/shared-host`。
- 从 `61198572b76045add403705c52b29c5db97ca536` 快进到远端 main 的
  `59f985a998c9e2297b24d2675d720f43a9824d96`，workspace 版本变为 **0.27.0**。
- 合入七条已提交变更，包括 `460d3bee` prepared restore stat/digest、`59f985a9`
  envelope reservation/中断恢复按需解码、`99dd90ea` registry-aware extension discovery、
  `4fcf2d85` tree preview 及发布/文档变更。
- 合入前备份 main 的 HEAD/index/75 个本地路径与内容摘要，以及 Host 的 41 个本地路径、
  二进制 patch 和完整文件副本。Host 使用 include-untracked stash 保存并恢复，stash 保留。
- 唯一冲突在 `owned_decode_tests.rs` 的 benchmark 注释，保留上游更准确的说明及新增测试。
  原有本地路径未删除，Host index 恢复为空；本轮没有创建实现提交或合回 main。
- 再核对主工作区：HEAD 仍为 `61198572…`，index/staged patch/path 集合及全部 **75 个
  本地文件摘要不变**。没有把主工作区未提交的其他工作混入本次集成。

## Review 结论

### 1. stat 命中不是精确恢复证明：新增严格提交路径

上游默认 `PreparedJsonlRestore::commit()` 在文件长度和 mtime 匹配时跳过完整摘要。
这是性能启发式，而不是“准备时读取的历史仍为当前历史”的证明。复现用例保持文件长度、
恢复原 mtime，分别进行原地改写和 atomic replacement；默认提交均接受了不同历史。

Host 的准确 Session 恢复不能依赖这一假设。新增 `commit_strict()`，顺序为：

1. 获取真实原生 writer lease。
2. 在 lease 内计算完整历史摘要，与准备阶段结果比较。
3. 验证通过才执行准备阶段记录的修复。

普通 Host 后端固定使用严格路径。失败释放 ownership、不覆盖更新后的源文件。
同长度/mtime 的两个回归先复现失败，再在严格路径通过；另补 unchanged 成功、实际 writer
占用及竞争 writer 拒绝测试。修正了把 stat 命中当成文件身份保证的注释。

**保留上游默认 fast path**，没有把普通 CLI 的恢复都改为全量复核；默认 CLI 的同戳限制
仍存在并已明确。严格路径有一次额外全文件读取，复杂度与历史字节数相关，不宣称免费或
提供新的恢复耗时数据。它也不防御忽略 lease 的外部写入者、同 UID 恶意文件系统操作，
不增加 power-loss fsync 保证。

### 2. envelope reservation：保留 canonical decoder 作为语义权威

复核外层对象单次 capacity reservation、重复 key、嵌套 Value、私有 serde_json protocol、
转义及尾部语法处理。优化只预留外层，嵌套仍由 serde_json 解码；私有协议或任何尝试失败
会在**同一内存行**重走 canonical Value 路径，不重新读取文件。上游 parity/生成式嵌套测试
包含在 509 项库回归中，没有发现需要额外修改的语义差异。

### 3. 中断恢复：按需解码不等于跳过 ancestry 验证

复核最近恢复条目的投影/按需 hydration、分支 ancestry、deferred 与 streaming/tool 恢复
查询的复用。相关 tail 查询、无序/稀疏元数据、选择性 hydration 与恢复执行契约测试通过。
“少解码 payload”不等于历史扫描、索引或完整摘要都成为常数成本。

Host 为观察当前分支的 deferred 状态新增只读 `pending_deferred()`，复用相同恢复查询，
不触发 provider 轮询或 execution admission。已有 `active_operation_snapshot()` 也只是
瞬时观察，不能当作 admission、取消或结算 barrier。

### 4. 普通后端故障边界：零 warning 不代表保存成功

真实原生 writer fence 故障注入发现：Session 可能直接返回 Harness Io 错误，却没有
write-warning 或 unsaved recovery copy。例如失败后出现 pending message 提交顺序错误。
若仅凭 warning 数判断，后端会错误回到 Idle 并允许正常 close。

后端现在对 SessionStorage/Io、panic、write-warning/unsaved 数据保守标记
`persistence_uncertain`，保持 fault 与驻留资源，拒绝后续执行和正常卸载。
该标记跨操作保留；queue 投递错误也不能绕过它。恢复副本可导出，但**不承诺所有直接
失败都有副本**，不声称修复了 Harness 所有失败路径的副本可用性。原生日志中的未知工具
结果仍须显式对账，不能因错误重试而重复外部副作用。

### 5. 与 CLI 及原有本地改动的重叠

保留本地异步 provider/目录查询/诊断等变更及新 Host 模块，没有通过 CLI 全局准备函数
装配并发 Agent。上游 extension discovery 与本地 `session.rs` 干净合并，registry-aware
选择/GC 与 tree preview 定向测试通过。参数与启动诊断回归通过；不据此宣称完整 CLI 或
workspace 已验收。

## 普通后端已验证的范围

`crates/rpi-cli/src/host/ordinary/` 已接入实际 Harness、JSONL 与受监督工具：

- 单线程 Tokio 中两个 Agent 的实际工具重叠、各自 cwd/会话隔离；同 Agent Busy 拒绝。
- prompt、steering/follow-up、多轮执行与 compact；迟到旧 ticket 的取消不影响新运行。
- 客户端/ticket 断连不取消；启动等待者取消后仍清理产生的 owner、writer 与预占。
- 精确 SessionRef 恢复而非更新文件；独立 catalog 的竞争 writer 拒绝与关闭后重开。
- 错误 identity/绑定拒绝不修复 torn source，不保留 writer/容量；输入预算先于写入。
- 模型/provider/tools/manual 模式错误及 binder panic 在 commit 前拒绝并回滚。
- deferred 持续 busy、显式 polling 保持原生 run ID、恢复不隐式轮询。
- close 等待者取消不提前释放槽或 writer；迟到 after-tool callback 必须真实结束。
- Closing 提交失败仍取消结算；Closed 提交失败保留槽，修复数据库后幂等补交。
- 实际 writer fence 故障不发布正常 Idle/Closed；重启仅分类 Interrupted，不伪造完成。

这是可显式调用的普通后端模块，不是完整 Host 服务。可信 binder 仍待接生产的资源、
权限与凭据解析；mini、服务注册表/协调器、IPC、Request 收据、超时计时器、CLI Host
入口及故障对账管理协议均未实现。catalog shutdown 不能替代实际后端关闭。

## 定向回归结果

全部在 Host worktree 内构建，`CARGO_TARGET_DIR="$PWD/target"`、`--locked --offline -j 2`：

| 套件 | 结果 |
| --- | ---: |
| CLI `host::` | 56 通过（含 15 项 ordinary 测试） |
| CLI `args::tests` | 41 通过 |
| CLI `startup_diagnostics` | 6 通过 |
| CLI `registry_aware` | 5 通过 |
| CLI `tree_preview_tests` | 10 通过 |
| CLI prebuilt registry discovery | 1 通过 |
| Harness `--lib` | 509 通过、4 ignored |
| `deferred_settlement` | 8 通过 |
| `durable_cancellation` | 7 通过 |
| `harness_run_e2e` | 62 通过 |
| `jsonl_writer_lease`（含独立子进程） | 3 通过 |
| `prepared_restore_strict` | 4 通过 |
| `recovery_execution_contract` | 4 通过 |
| Python benchmark fixture | 19 通过、0 skipped |

Python 本轮显式启用 release 二进制 smoke，六模式均通过。
对修改的 Rust 文件做 scoped rustfmt 检查，`git diff --check` 通过。
完整 workspace、Linux/cgroup、Windows 未在本轮重跑；新版本 600/3000 执行底座已在
后续单独复测，见 [600 报告](shared-host-integrated-performance.md) 与
[3000 报告](shared-host-3000-performance.md)。
已知历史 Agent `tool_program` 配额测试超时问题未据此宣称修复；全 workspace formatting
还有独立的 voice smoke 既有问题，本轮没有顺带改动。

## 性能数据不重标

[此前 600-Agent 报告](shared-host-worktree-performance.md) 的两轮数据属于当时旧基线、
执行底座探针，不经过新的 catalog/ordinary 后端或 IPC。0.27.0 集成后的 600/3000
数据见 [600 报告](shared-host-integrated-performance.md) 和
[3000 报告](shared-host-3000-performance.md)；它们同样不是生产 Host 验收。不能
改称生产 Host 数据，不能推出性能提升、无回退或统一内存上限。下一次性能验收应
针对接好的实际服务路径，明确上下文、工具、历史规模与清理边界，再完成 600/3000
barrier 和资源测量。
