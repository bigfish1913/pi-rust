# Mini 与既有 CLI 的兼容性验收

在 macOS arm64 上重建 `rpi` 和管理动态库 `librpi_host.dylib`，通过真实管理工具、认证 IPC 和本机 HTTP Provider 验证 Mini 与 Persistent 的正常和故障路径。验收过程中复现并修复了两处生命周期缺陷。Agent 严格 Clippy 和历史消息分叉仍有阻塞。

## 生命周期修复

- **扩展 native 启动锁。** Unix 上，`fork` 或 `dup` 可以暂时共享持锁文件的打开状态。旧 guard 只关闭自身的描述符，子进程已确认退出后重新启动仍可能收到 `native session is already owned or its cleanup is still in progress`。新回归用保留的重复描述符稳定复现。现在 guard 在原有释放边界显式解锁。进程退出待确认时，仍由 reaper 保管 guard。原 early-exit 用例、启动取消用例、未确认退出保锁用例和 stderr 生命周期用例均通过。
- **CLI RPC 取消。** 已准入的执行操作在 Session I/O 期间可能持有消息写入锁。旧取消分支等待也需要该锁的 `abort`，同时暂停推进执行操作。新用例在存储读处设门控，旧实现达到 15 秒截止时间。现在取消与已准入操作继续推进，保留原有队列回收和真实结算。相关 7 项测试均通过。
- **测试夹具。** 权限用例为临时脚本设置测试进程的有效组。在本机的 `wheel` 继承组下，`chmod(07754)` 会变成 `05754`。组修正后仍验证 setuid、setgid、sticky 和普通 rwx 位。扩展保存历史测试改为断言只读响应 `executable=false`，并分别验证无待处理执行或进度、支持空闲证明。资源构建测试使用隔离的标准 Cargo 构建目录，避免管理缓存包装器和共享目标目录影响测试 DLL 的位置与加载。

## 最终工件的真实链路

| 场景 | 管理工具调用 / HTTP 请求 | 验证结果 |
| --- | ---: | --- |
| Mini 基础 | 43 / 7 | 工具往返、原样 `{{literal}}`、权限与身份隔离、UTF-8 结果、取消与重启 `Lost` |
| Mini 丢失 close 回复 | 30 / 4 | 原请求对账、无自动重试，迟到回调与 finish 期间保留 owner 和 DLL，最终 `RABEFU` |
| Mini 回调中 SIGKILL | 18 / 3 | 最终事件 `RA`、staging 保留，重启 `Lost`，无自动加载或重放 |
| Mini 回调中 SIGTERM | 17 / 3 | 等待真实 callback 和 finish 后卸载，最终 `RABEFU`，进程正常退出 |
| Persistent 基础 | 7 / 1 | 查询、提交及原生准入收据 |
| Persistent 管理 | 21 / 1 | 创建、关闭、恢复、取消与停止 |
| Persistent 结果 | 33 / 6 | 运行中和终态结果、精准分页、跨 Agent 隔离、失败与重启 |
| 管理错误协议 | 10 个受控场景 | 配置缺失、Agent 不存在、断连、丢回复及历史协议能力差异 |

Mini 场景均断言工作区树保持原样，全夹具无 JSONL，native hook 身份为 `sessionId=null`。扩展真实资源服务另验证原生资源快照、工具往返、恢复和只读历史；跨仓库 Host 客户端另通过真实 daemon 认证、断连及重连测试。

## 默认回归结果

- CLI 库 1,557 项通过，3 项按原配置忽略。CLI 集成 137 项通过，其中 `host_control` 39 项、隔离构建的 `resource_selection` 18 项。Agent 库和集成合计 123 项通过。
- 扩展默认测试合计 444 项通过，含 extcore 库 408 项、其他 extcore 集成 16 项和管理包 20 项。默认忽略的测试中另执行了真实 daemon 和资源服务，两项通过。真实历史分叉用例的结果见下节。
- CLI `--lib --bins --test host_control --no-deps -D warnings` 通过。管理包 `--all-targets --no-deps -D warnings` 通过，沿用 SDK 导出宏的命令行例外 `-A clippy::not_unsafe_ptr_arg_deref`。Core workspace 和两个扩展包的格式检查、两树 `git diff --check` 通过。四份 wire 合同逐字一致。

## 待通过的门禁

- `cargo clippy --locked --offline -j 2 -p rpi-agent --lib --test agent_builder_hooks --no-deps -- -D warnings` 仍因 `AgentEvent` 的 `large_enum_variant` 退出 101。本轮修复其余三项机械 lint。`MessageUpdate` 的公共值类型为 `AssistantMessageEvent`，当前最大和次大变体分别至少为 560、289 字节。保持字段类型和现有 lint 策略。
- 扩展的真实历史消息分叉会发送 `--fork-entry` 和 `--fork-position at`。当前核心只解析 `--fork`，因此私有资源模式以未知参数拒绝启动。验收保留该失败。此前的当前叶分叉及克隆步骤已通过，到历史消息切点时失败。
- #14、#65、#66 保持开放。薄 Host 默认 CLI/TUI、批准与计时器、daemon 整组恢复属于其余阶段。

## 工件与证据

- `rpi` SHA256 `d117a25c61482993b452f1223ae87cb117d0da3b135b7e598ac44dbed2063d58`。
- `librpi_host.dylib` SHA256 `1b612c06c6ac6b2d745668a291d5d93b76afa8a527d3bcf838e843147a025ab7`。
- 独立证据目录 `/tmp/rpi-host-integration-checks/compat-startup-20261125T015915895460Z`，保留原始 `.log`、`.exit`、两树初始快照和最终工件哈希。前一阶段报告继续对应其原始源码与工件。
