# Host 历史消息分叉验收

在 macOS arm64 上，真实 `rpi` 通过扩展服务的历史消息分叉场景。由指定消息切出的子会话保留切点及更早的历史。切点之后的消息在首次启动和关闭后重新打开时都不出现。子会话取得独立的会话 ID 和修订号为 1 的资源绑定，源会话的历史和资源绑定保持原样。

## 修复

- 核心 CLI 识别 `--fork-entry` 和 `--fork-position at|before`，并交给现有 JSONL 分叉引擎。`at` 包含目标消息，`before` 从目标消息的父节点切分。只给出 `--fork-entry` 时采用引擎现有的 `before` 默认。只给出 `--fork` 时仍按当前分支叶分叉。
- 资源会话在原子发布时把新资源绑定写到消息切点之后。扩展先比对新会话 ID 与叶节点，再确认该叶节点是子会话的新资源绑定且其直接父节点为所请求的消息。旧版核心忽略切点参数时，扩展拒绝发布不匹配的分叉。
- 核心保留 `Args` 的结构，分叉参数存放在现有参数表中，并在交给扩展前过滤。资源配置只接受这两个经过验证的保留选项。四份共享 Host 协议镜像逐字一致。
- 核心定向回归曾在分叉发布后重新声明子会话写锁时收到 `writer lease is already held`。独立探针用同一个打开文件描述和 `fs2` 验证，旧 guard 销毁后重复描述符仍能持锁。Unix 写锁现在在原释放边界显式解锁。回归确认 writer 存活时仍排斥第二写者，释放后即使重复描述符仍开着也可重新获得锁，持久化 fence 继续递增。原测试失败保留，当时持有重复描述符的进程未被直接观测。

## 验收

- 真实资源会话夹具覆盖当前叶分叉、克隆、离线源历史分叉，以及在线源分叉和克隆。它检查子会话的资源修订号和历史边界、源会话的设置和资源状态，以及子会话关闭后重新打开。扩展的 8 项分叉及恢复回归通过，包括旧版核心忽略切点时拒绝发布的场景。
- CLI 共 1,696 项通过，3 项按原配置忽略。其中 CLI 库 1,559 项、集成 137 项，后者含 `host_control` 39 项和单独运行的 `resource_selection` 18 项。扩展默认测试 444 项通过、6 项忽略。`rpi-tools` 库 154 项通过，Harness 分叉初始化与原子发布共 12 项通过。
- 最终工件通过 4 项 Mini 和 4 项 Persistent 真实场景，以及资源服务生命周期、历史分叉和跨仓库 daemon 连接回归。Mini 正常排空完成后观测到 `RABEFU`。SIGKILL 场景仅观测到 `RA`，重启后会话为 `Lost`，未重放请求。
- CLI `--lib --bins --test host_control --no-deps -D warnings` 通过。管理包 `--all-targets --no-deps -D warnings` 通过，沿用 SDK 宏的既有命令行例外 `-A clippy::not_unsafe_ptr_arg_deref`。格式和两树 `git diff --check` 通过。

## 剩余门禁

Agent 严格 Clippy 仍以 101 退出，只报告公共 `AgentEvent::MessageUpdate` 的 `large_enum_variant`。可见的大小差来自该字段内嵌的公开 `AssistantMessageEvent`。本轮保留字段类型、SDK ABI 和 lint 策略。#14、#65、#66 继续开放，整组恢复、默认薄 Host CLI/TUI、审批及定时器仍属后续范围。

## 证据

日志、退出状态和工件散列保存在 `/tmp/rpi-host-integration-checks/historical-fork-ttv8Aw7w`。前一轮的 `extension-native-resource-fork-3.log` 记录资源模式拒绝未识别的分叉参数。本轮的 `cli-fork-1.log` 记录写锁失败，`actual-os-writer-lease-before.log` 和 `actual-os-writer-lease-after.log` 是针对生产 guard 的确定性对照。最终重建工件和复验结果在 `writer-lease-fix/`。

- 最终 `rpi` SHA256 `d9ab5d583288a51f03fac332a1d62d994040da0d6db9844000d24f7341dff7c6`。
- 最终 `librpi_host.dylib` SHA256 `c855c6df41cd68c6ce7c306ca61c55f834c68cfedace4b2d894d374eb60b8b2b`。
