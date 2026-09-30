# rpi 远程模式：无头服务端 + 零本地资源的远程 TUI

rpi 是一个 Rust 原生、library-first 的 Pi agent SDK，同时提供可直接使用的终端 coding-agent CLI。稳定版本目前为 `v0.1.23`。最近的开发线补齐了一块长期缺失的拼图：**让 agent 跑在一边，交互界面跑在另一边**。

现在 rpi 支持：

```bash
# 1) 服务端：无头运行，启动时打印 token
rpi --server --port 9899

# 2) 客户端：在另一台机器/另一个进程里连接它
rpi --connect 127.0.0.1:9899 --token <token>
```

客户端是一个**纯输入/显示**的终端界面——不加载 provider、不加载工具、不加载扩展、不创建 session 文件。所有 agent 资源都在服务端。

## 为什么需要远程模式

常见的几个场景：

- **算力在服务器上**：agent 需要跑在装着代码、凭据、工具链的机器上，而你只想在本地敲键盘；
- **一个服务端，多个前端**：以后可以让多个客户端连同一个 agent 会话；
- **无头 CI/沙箱**：先起一个无头 agent，再用脚本或 UI 驱动。

和很多“远程”方案不同的一点是：**客户端不复制 agent 实现**，它只是一个瘦客户端。

## Token 认证

服务端默认启用 token 认证，启动时打印：

```text
rpi-server listening on 127.0.0.1:9899
token: 3592139afa7b77e43592139afa7b77e4
```

客户端传递 token 有两种方式，`--token` 优先：

```bash
rpi --connect 127.0.0.1:9899 --token 3592139afa7b77e43592139afa7b77e4

# 或者走环境变量，避免把 token 写进命令历史
export RPI_SERVER_TOKEN=3592139afa7b77e43592139afa7b77e4
rpi --connect 127.0.0.1:9899
```

服务端侧：

```bash
rpi --server --port 9899 --bind 127.0.0.1      # 默认：随机 token
rpi --server --port 9899 --token my-secret     # 指定 token
rpi --server --port 9899 --no-token            # 关闭认证（仅限可信本地环境）
```

认证是**连接级**的：连接建立后必须先 `authenticate`，否则其他请求（包括 `subscribe`）会被拒绝，错误码统一为 `-32001`。

## 设计：参考 pi 的分层，但完全是 Rust 自己的实现

rpi 的定位是 `pi` 的 Rust 替代品，所以远程层在**思想上**参考了原生 pi 的分层——尤其是 `packages/coding-agent/src/modes/rpc/`（命令/响应/事件分层）和 `src/client/`（把会话抽象成与本地 harness 无关的 transcript）——但**不依赖任何 pi 运行时组件**。

具体分层：

| 层 | 位置 | 职责 |
|---|---|---|
| 协议（单一真源） | `crates/rpi-cli/src/remote/protocol.rs` | `RemoteEvent` / `RemoteCommand` / `RemoteResponse`，**服务端与客户端共用**，线协议不会两端漂移 |
| 无头 agent 循环 | `crates/rpi-cli/src/modes.rs` | 读 stdin 命令、驱动 `main` lane、把 `AgentEvent` 投影成线协议 |
| 服务端适配 | `rpi-package/packages/rpi-server` | TCP JSON-RPC、会话管理、token 校验、转发子进程事件 |
| 客户端连接 | `crates/rpi-cli/src/remote/client.rs` | 握手、认证、事件泵 |
| 客户端会话模型 | `crates/rpi-cli/src/remote/session.rs` | 把事件流折叠成 transcript（用户/助手/思考/工具/通知） |
| 客户端 TUI | `crates/rpi-cli/src/remote/tui.rs` | 基于 `pi-tui` 渲染 |

值得强调的一点：`--connect` 在 CLI 入口**最早**被拦截，直接跳过 provider / harness / session / extension 的全部本地构建。这就是“客户端零本地资源”的落地方式，而不是给本地 harness 套一层远程代理。

## 线协议一览

连接后，客户端发 JSON-RPC：`start_session`（fork 一个 `rpi --mode rpc` 子进程）、`subscribe`（订阅事件流）、`send`（把命令写进子进程 stdin）、`stop_session`。

服务端把子进程 stdout 的每一行作为一个 `event` 推给订阅者，内容就是 `--mode rpc` 的 JSONL：

```json
{"type":"agent_start"}
{"type":"message_update","eventType":"text_delta","delta":"P","contentIndex":0}
{"type":"tool_execution_start","toolCallId":"c1","toolName":"read","args":{"path":"a.rs"}}
{"type":"response","id":1,"status":"ok","result":{"outcome":"completed"}}
```

命令集合：`prompt`、`abort`、`get_state`、`set_model`、`set_thinking_level`、`set_active_tools`、`ping`、`stop`。

## 远程 TUI 里能做什么

直接输入文本回车即可发送提示词，回复、思考过程、工具调用都会流式渲染。斜杠命令：

| 命令 | 作用 |
|---|---|
| `/abort` | 中断当前运行 |
| `/state` | 刷新 model / thinking / 工具 |
| `/model <id>` | 切换模型 |
| `/thinking <level>` | `off`…`max` |
| `/tools` | 查看当前启用的工具 |
| `/exit` | 退出 |
| `Ctrl+C` | 运行中=中断；空闲=退出 |

会话树类命令（`/tree`、`/fork`、`/switch`、`/export`、`/name`、`/reload`）在远程模式下会明确提示不可用——它们依赖本地 harness 状态，远程模式下不应假装可用。

## 顺带修好的一件事

配合远程模式，还统一了扩展侧与 SDK 的一个不一致：`rpi-plugin-sdk` 已经把 `register_entrypoint` 标记为 `unsafe fn`，但仓库里只有两个扩展跟进了这个改动，导致整个 workspace 编译不过。现在所有 20 个扩展包都已对齐，`cargo build --release` 全绿。

## 验证方式

- 协议与会话折叠有单元测试（`remote/protocol.rs`、`remote/session.rs`）；
- 客户端有自包含集成测试（假服务端）：握手 + transcript 折叠、事件泵忽略响应帧、**token 认证三态**；
- 还有一个环境变量门控的真实端到端测试，用真实客户端连真实服务端跑一轮真实模型调用。

## 稳定能力

rpi 的稳定主线仍然是：

- Rust 原生的异步、流式 Agent loop
- Anthropic、OpenAI-compatible 和 faux provider
- 与 Pi 对齐的 `read`、`write`、`edit`、`bash` 工具
- JSONL session、上下文压缩和 AgentHarness
- `rpi-plugin-sdk` 稳定 ABI
- 可嵌入的多 crate SDK 和 `rpi` 终端 CLI

Node/TypeScript 扩展桥接目前仍属于 Beta，建议仅用于本地评估和兼容性测试。

## 快速开始

```bash
cargo install rpi-cli --version 0.1.23

# 直接本地用
rpi -p "检查当前项目结构并列出最值得优先修复的问题"

# 或体验远程模式
rpi --server --port 9899          # 终端 A
rpi --connect 127.0.0.1:9899 --token <token>   # 终端 B
```

完整的协议说明、参数和限制见 `docs/remote-mode.md`。

项目地址：<https://github.com/bigfish1913/pi-rust>

官网和文档：<https://rpi.laofu.online/>

欢迎反馈远程模式在网络、认证、跨平台终端下遇到的问题，以及你希望客户端补充哪些能力。
