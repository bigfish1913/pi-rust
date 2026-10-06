# 远程模式（`--server` / `--connect`）

rpi 支持把 agent 跑在**一个无头服务端**上，然后从**另一个进程**的 TUI 远程驱动它。
客户端是纯输入/显示——**没有任何本地 agent 资源**（无 provider、无工具、无扩展、无 session 文件）。

> 设计取向：rpi 是 TS `pi` 的 Rust 替代品。远程层**参考**了原生 pi
> `packages/coding-agent/src/modes/rpc/` 与 `src/client/`（`RemoteSession` + transcript）
> 的分层思想，但协议与会话模型都是 rpi 自己的 Rust 实现，**不依赖任何 pi 运行时组件**。

## 快速开始

```bash
# 1) 服务端（无头）——启动时会打印 token
rpi --server --port 9899

# 2) 客户端（远程 TUI）
rpi --connect 127.0.0.1:9899 --token <token>
```

服务端只监听 TCP、不启动 TUI；每个 `start_session` 会 fork 一个
`rpi --mode rpc` 子进程真正执行 agent 循环。

### 服务端参数

| 参数 | 说明 |
|---|---|
| `--server` | 进入无头服务端模式（由内置/扩展 `rpi-server` 提供 TCP 监听） |
| `--port <n>` | 监听端口（默认 `9800`） |
| `--bind <ip>` | 监听地址（默认 `127.0.0.1`） |
| `--token <t>` | 使用指定 token（默认随机生成 32 位 hex） |
| `--no-token` | **关闭认证**（仅限可信本地环境） |

启动日志示例：

```text
rpi-server listening on 127.0.0.1:9899
token: 3592139afa7b77e43592139afa7b77e4
```

### 客户端参数

| 参数 | 说明 |
|---|---|
| `--connect <host:port>` | 连接服务端并进入远程 TUI |
| `--token <t>` | 认证 token；缺省时回退到环境变量 `RPI_SERVER_TOKEN` |

```bash
export RPI_SERVER_TOKEN=3592139afa7b77e43592139afa7b77e4
rpi --connect 127.0.0.1:9899        # 无需再传 --token
```

`--token` 优先级高于环境变量；空/纯空白的环境变量会被忽略。

## 认证

当服务端启动了 token（默认）时，连接必须先认证：

1. 客户端在连接后、任何其他请求之前发送
   `{"jsonrpc":"2.0","id":1,"method":"authenticate","params":{"token":"…"}}`。
2. 匹配 → `{"result":{"authenticated":true}}`，此后可正常调用其他方法。
3. 不匹配 → `{"error":{"code":-32001,"message":"invalid token"}}` 并**断开连接**。
4. 未认证就调用其他方法（含 `subscribe`）→
   `{"error":{"code":-32001,"message":"authentication required"}}`。

客户端在认证失败时会给出去哪取 token 的提示；`--no-token` 启动的服务端不校验。

## 架构

| 层 | 位置 | 职责 |
|---|---|---|
| 协议（单一真源） | `crates/rpi-cli/src/remote/protocol.rs` | `RemoteEvent` / `RemoteCommand` / `RemoteResponse` / `SessionState`，服务端与客户端**共用**这些类型 |
| 无头 agent 循环 | `crates/rpi-cli/src/modes.rs`（`modes::rpc`） | 读 stdin 命令、驱动 `main` lane、把 `AgentEvent` 投影成线协议输出 |
| 服务端适配 | `rpi-package/packages/rpi-server` | TCP JSON-RPC、会话管理、token 校验、把子进程 stdout 转发为事件 |
| 客户端连接 | `crates/rpi-cli/src/remote/client.rs` | TCP 握手、`authenticate`、事件泵、发送命令 |
| 客户端会话模型 | `crates/rpi-cli/src/remote/session.rs` | 把事件流折叠成 `Transcript`（用户/助手/思考/工具/通知）+ `SessionState` |
| 客户端 TUI | `crates/rpi-cli/src/remote/tui.rs` | 基于 `pi-tui` 渲染 transcript + 输入编辑器 + 状态栏 |

`--connect` 在 `crates/rpi-cli/src/app.rs::run()` 中**最早**被拦截，直接跳过
provider / harness / session / extension 的全部本地构建——这是“客户端零本地资源”的实现方式。

## 线协议

两个层次，都是“一行一个 JSON 对象”。

### 外层：服务端 JSON-RPC（TCP）

| 方法 | 参数 | 结果 |
|---|---|---|
| `authenticate` | `{token}` | `{authenticated:true}`；失败 `-32001` |
| `start_session` | `{}` | `{sessionId, session}`（fork 一个 `rpi --mode rpc` 子进程） |
| `subscribe` | `{sessionId}` | `{subscriptionId, sessionId, status:"subscribed"}` |
| `send` | `{sessionId, command}` | `{sessionId, status:"sent"}`（把 `command` 写进子进程 stdin） |
| `stop_session` | `{sessionId}` | `{sessionId, status:"stopped"}` |
| `list_sessions` / `server_status` / `ping` | — | 会话列表 / 服务端信息 / `{type:"pong"}` |

订阅后服务端持续推送：

```json
{"jsonrpc":"2.0","method":"event","params":{"subscriptionId":7,"event":{ … 子进程输出行 … }}}
```

### 内层：`--mode rpc` 的 JSONL

子进程 stdout 的每一行都是下列之一（亦即 `event` 字段的内容）：

- `{"type":"ready"}` — 就绪标记；
- 生命周期事件（`RemoteEvent`，与 `agent_event_json` 同形）：
  `agent_start`、`agent_end`、`retry_scheduled`、`turn_start`、`turn_end`、
  `message_start`、`message_update`（含 `eventType` / `delta` / `contentIndex` 便捷字段）、
  `message_end`、`tool_execution_start` / `tool_execution_update` / `tool_execution_end`；
- 命令响应：`{"type":"response","id":…,"status":"ok"|"error","result"|"error":…}`。

客户端可用命令（`RemoteCommand`）：`prompt`、`abort`、`get_state`、`set_model`、
`set_thinking_level`、`set_active_tools`、`ping`、`stop`。

## 远程 TUI 中可用的命令

| 命令 | 作用 |
|---|---|
| 直接输入文本 + `Enter` | 发送 prompt（流式渲染回复/思考/工具） |
| `/abort` | 中断当前运行 |
| `/state` | 刷新 `get_state`（model / thinking / 工具） |
| `/model <id>` | 切换模型 |
| `/thinking <level>` | `off`…`max` |
| `/tools` | 显示当前启用的工具 |
| `/help` | 命令帮助 |
| `/exit`、`/quit`、`/q` | 退出 |
| `Ctrl+C` | 运行中=中断；空闲=退出 |

会话树类命令（`/tree`、`/fork`、`/switch`、`/export`、`/name`、`/reload`）在远程模式下
**不可用**（输入会得到明确的“不支持”提示），因为它们依赖本地 harness 状态。

## 限制

- `start_session` 使用**服务端进程的默认参数**（provider / model / 工具来自服务端启动时的配置）；
  客户端可在会话内用 `/model`、`/thinking`、`/tools` 调整。
- 客户端不做本地会话持久化；会话文件在**服务端**。
- TUI 需要真实终端（alt-screen），在非 TTY 环境下 `--connect` 会在认证/握手阶段就返回错误。

## 测试

- 单元测试：`crates/rpi-cli/src/remote/protocol.rs`（线形往返）、`session.rs`（事件折叠）。
- 集成测试：`crates/rpi-cli/tests/remote_client_smoke.rs`
  - 自包含（假服务端）：握手 + transcript 折叠、事件泵忽略响应帧、**token 认证三态**；
  - 真实服务端（env 门控）：`RPI_REMOTE_E2E_ADDR` + `RPI_REMOTE_E2E_TOKEN`
    驱动真实客户端跑一轮真实模型调用。
