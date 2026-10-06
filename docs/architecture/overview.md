# 架构概览

rpi 是 Rust 原生 Agent SDK 与终端应用。当前 workspace 包含九个产品 crate；CLI 组合底层能力，嵌入式应用可按需使用 SDK。

## 功能分层

| Crate | 职责 |
| --- | --- |
| `rpi-telemetry` | 遥测接口与 span/event 合约 |
| `rpi-ai` | 模型与消息类型、Provider、流式响应、协议适配及 faux Provider |
| `rpi-agent` | Agent Loop、工具调用、事件、队列、取消与轮次回调 |
| `rpi-tools` | 内置工具与 ExecutionEnv 执行环境抽象 |
| `rpi-harness` | 会话树、JSONL 持久化、上下文压缩、恢复、skills 与模板 |
| `rpi-tui` | 终端组件、编辑器、布局、Markdown 与 transcript 渲染 |
| `rpi-plugin-sdk` | 动态库扩展使用的 C ABI 类型与注册接口 |
| `rpi-extensions` | 宿主扩展加载与跨 ABI 调用桥接 |
| `rpi-cli` | 配置、认证、资源发现、CLI/TUI、远程服务端与客户端 |

具体依赖以各 crate 的 `Cargo.toml` 为准。`rpi-agent` 可独立运行；需要会话持久化和压缩时再使用 Harness。插件通过 ABI 加载，不要求与宿主使用相同 Rust 编译器版本。

## 一次请求的路径

```text
TUI Enter → Editor::submit → editor.on_submit 回调
  → TuiMessage::UserInput → TUI 主循环
  → interactive_tui/run.rs::run_prompt_streaming
  → LaneHandle::prompt_text → AgentHarness::prompt_text
  → run_core → run_core_with_entry（持久化用户消息、构造上下文）
  → run_agent_loop → run_loop
      → stream_assistant_response → StreamFn → Provider → HTTP/SSE
      → AssistantMessage → 工具执行 → ToolResultMessage → 下一轮模型请求
  → AgentEvent → BroadcastEmitter
  → interactive_tui/events.rs::drain_agent_events → handle_agent_event
  → TranscriptView / TUI 组件 → 终端渲染
```

运行中提交通过 `lane.steer` 入队，由 loop 在检查点消费。SDK 的 `Agent::prompt`
走 `prompt_messages → run_prompt → run_agent_loop`；TUI 不经过这个入口或 `AgentSession`。
完整调用与恢复分支见 [Agent Loop 与流式消息链路](../agent/loop-walkthrough.md)。

## TUI 模块边界

[interactive_tui.rs](../../crates/rpi-cli/src/interactive_tui.rs) 保留启动、输入提交与主循环。
同名目录的子模块按职责组织：`commands`、`extensions`、`settings`、`sessions`、`state`、
`run`、`events`、`selectors`、`autocomplete`、`rendering`；回归测试放在 `tests.rs`。
主文件负责调度，子模块共享宿主状态，辅助接口限制在 TUI 模块内部。

Provider 是 LLM 协议边界。上层处理统一消息和事件，具体 HTTP/SSE 协议由 `rpi-ai` 适配。工具通过 AgentTool 接口执行，内置工具使用 ExecutionEnv，以支持真实环境与离线测试环境。

## 会话与长任务

会话是带 branch 和 lane 的 entry 树，JSONL 后端记录消息、操作及步骤。Harness 根据 branch 构造模型上下文，并管理提交、取消与崩溃恢复。压缩新增摘要 entry，保留原始历史；当前既支持 run 前检查，也支持工具结果后的运行中检查。

恢复和重放行为受记录状态与工具 replay 策略约束，不能将所有工具视为可安全重复执行。详细排查证据见 [恢复与重复思考复盘](../debugging/llm-repetition-forensics.md)。

## 按功能继续阅读

- [Agent 项目结构](../agent/project-structure.md)
- [Agent Loop 与流式消息链路](../agent/loop-walkthrough.md)
- [上下文压缩](../agent/context-compaction.md)
- [扩展开发与 ABI 生命周期](../extensions/authoring.md)
- [远程服务端与客户端](../remote/user-guide.md)
- [用户指南](../guides/user-guide.md)
- [性能测量](../performance/benchmark-vs-pi.md)

各 crate 的 README 和 API 文档提供具体接口说明。历史 TypeScript 移植计划不再作为当前功能规格；后续工作统一见根目录 [ROADMAP](../../ROADMAP.md)。
