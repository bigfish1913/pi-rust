# 内置 Host 模块

在 `rpi-cli` 内按文件夹划分代码边界，不新增 crate，不引入额外服务程序。
模块间通过明确类型交互，不以 CLI 进程全局状态装配并发 Agent。

## 已有基础

| 文件 | 职责 |
| --- | --- |
| `types.rs` | Agent/Run/Request/Host epoch 身份、普通/mini、启动摘要与生命周期 |
| `config.rs` | Host 默认策略、逐 Agent 超时继承/禁用、硬上限与有效值来源 |
| `capacity.rs` | 原子驻留预占、重复检查、动态降低上限而不驱逐、令牌释放 |
| `catalog.rs` | 64 槽有界入口、单个专用存储 worker、epoch/revision 守卫、分页与关闭确认 |
| `registry.rs` | SQLite WAL/FULL、私有目录与单实例锁、准确引用去重、重启分类及容量提交/回滚 |
| `ordinary/mod.rs` | 普通 Harness 后端所有者、弱客户端、非持久化 ticket、输入预算及错误类型 |
| `ordinary/supervisor.rs` | 准确恢复、绑定预检、运行/投递监督、取消、deferred 状态、结算与关闭 |
| `ordinary/tests.rs` | 真实 Harness/JSONL/工具并发、断连、恢复与存储故障测试 |
| `error.rs` | 固定对外诊断；存储错误 Display 不输出原始存储内容 |
| `tests.rs`、`registry_tests.rs` | 基础规则、真实存储、跨进程锁、重启、并发与清理测试 |

`HostCatalog::open` 仅在显式调用时创建一个存储线程，不按 Agent 创建线程，SQLite
不在 Tokio worker 上执行。普通后端已可由 Rust API 显式装配，但**没有后台 Host 服务、
IPC、新 CLI 命令或 mini 后端**。`lib.rs` 导出不改变普通 CLI 启动路径。
超时目前只验证策略，尚未启动计时器。清单只支持 Unix，已在 macOS 验证；Windows
显式返回 UnsupportedPlatform，Linux/cgroup 仍需实机验收。

## 普通 Agent 的执行边界

- `OrdinaryAgent` 是长期所有者，必须由未来服务注册表独立保留；`OrdinaryClient`
  仅持弱引用。客户端、等待 future、ticket 丢弃不取消已接收的后台工作。
- 每个普通 Agent 使用真正的 Harness 与原生 JSONL Session，不另写 Agent loop。
  支持 prompt、steering/follow-up/next-run、pending/deferred resume 与显式 compact。
  空闲时不保留一个专门等待命令的 Tokio 任务，也不创建逐 Agent OS 线程。
- 从清单指定的 Session ID 和绝对路径恢复，不查找“最新 Session”，不重放 argv/prompt。
  验证 Session ID、cwd、解析后的 model/provider/tool 名称及自动驱动模式；绑定拒绝发生在
  writer 获取与修复前。随后 `commit_strict()` 在 writer lease 内复核完整历史摘要，再修复。
- `OrdinaryBinder` 是**可信的 Rust 装配边界**，不是生产资源解析器。它必须正确恢复
  profile/资源、凭据作用域、工具权限与 hooks，不调用 CLI 全局 cwd/env 初始化。
  现有测试 binder 使用 FauxProvider；不能据此声称完整权限/凭据策略恢复已经接好。
- `RunTicket` 只标识本次 Host 生命周期中的监督任务；Harness operation ID 位于运行结果，
  两者不可混用。成功 submit、Running 元数据和 ticket 都**不是持久化输入 ACK**，没有
  Request 收据、跨 Session 接收协议或工具 exactly-once 保证。
- 同一个 Agent 拒绝重叠执行。不同 Agent 的工具在单线程 Tokio runtime 内确实重叠，
  不用“进入队列”代替并发；测试还检查工具返回的实际 cwd，而非只检查 Session header。
- 取消只作用于匹配的 ticket/真实 Harness run ID；旧 ticket 不会取消新一轮执行。
  不 abort/drop 实际 Harness future；取消后继续等待真实结算、迟到 callback 与 Session drain。
- `Suspended` 不是完成。停驻 deferred 时仍为 Running/busy，没有活跃 Tokio 执行任务也不
  发布 Idle；恢复时直接 Starting -> Running，不隐式轮询 provider。显式轮询保留原生 run ID。
  本地 close 不代表远端 deferred 作业已完成；服务层仍需定义远端作业的取消/保留策略。
- 关闭先停止接收，再协作取消、等待实际执行与写入排空，最后关闭 Harness、释放 writer，
  才提交 Closed 并释放槽。Closing 提交失败仍要请求取消并结算；Closed 提交失败保守保留
  驻留槽，重试只补交元数据，不再次调用已关闭 Harness 的 wait-for-idle。
- 写入失败不一定生成 write-warning 或恢复副本。直接的 SessionStorage/Io 错误、panic、
  warning/unsaved 数据都会使后端进入需对账状态；运行报告提供 `persistence_uncertain`。
  不继续接收操作，不正常卸载，不把零 warning/零副本当成“已保存”。可取出的恢复副本
  可以导出，但不能承诺所有失败都有副本；原生日志中的未知工具结果也不能盲目重放。
- Owner Drop 仅尝试清理，不保证成功卸载。服务必须在显式 close 成功后才移除所有者；
  对失败或持久化不确定的所有者需要显式对账/恢复流程，该管理协议尚未实现。

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

## 验证与性能口径

在 `pi-rust-wt-host` worktree 内运行，构建目录也放在该 worktree：

```bash
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-cli --lib host:: --locked --offline -j 2
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-cli --lib args::tests --locked --offline -j 2
CARGO_TARGET_DIR="$PWD/target" cargo test -p rpi-cli --lib startup_diagnostics --locked --offline -j 2
```

本轮分别 **56（含 15 项普通后端）、41、6 项通过**。Harness 库 **509 通过、4 ignored**，
定向集成 **88 通过**；上游 registry-aware discovery、tree preview 另 **16 项通过**。
Python fixture **19 项全部通过**，显式启用当前 release 二进制的六模式 smoke。
完整 workspace 未重跑；合入版本、review 发现、具体套件见
[main 集成记录](../../../../docs/architecture/shared-host-main-integration.md)。

此前两轮 600-Agent release 执行底座测试见
[性能报告](shared-host-worktree-performance.md)；0.27.0 集成后的 600/3000 复测见
[600 报告](../../../../docs/architecture/shared-host-integrated-performance.md) 与
[3000 报告](../../../../docs/architecture/shared-host-3000-performance.md)。
这些探针不经过本模块的 catalog/ordinary 后端或 IPC，不能重标为生产 Host/mini 的
验收数据。后续须对接好的真实服务路径重测，不宣称统一内存上限。
