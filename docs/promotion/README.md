# 推广素材

本目录保存按功能编写的文章草稿，与用户和开发者文档分开维护。发布前应按当前代码、CHANGELOG 和实际测量结果校对版本、数据与功能描述。

| File | What it is | Where it goes |
| --- | --- | --- |
| [`01-benchmark-vs-native-pi.md`](01-benchmark-vs-native-pi.md) | The measured comparison against native TypeScript `pi`, in Chinese and English | 掘金 / OSCHINA / RustCC / HN |
| [`02-rust-agent-runtime-architecture.md`](02-rust-agent-runtime-architecture.md) | Long-form Chinese article on the layering, the provider seam, the plugin ABI and crash recovery | 掘金 / OSCHINA / 知乎 |
| [`03-english-showhn-devto-reddit.md`](03-english-showhn-devto-reddit.md) | English posts | Hacker News, DEV.to / Hashnode, r/rust, r/LocalLLaMA, Lobsters |
| [`04-submissions-twir-and-awesome-rust.md`](04-submissions-twir-and-awesome-rust.md) | Verified submission rules for two third-party lists | This Week in Rust, Awesome Rust |
| [`05-awesome-list-survey.md`](05-awesome-list-survey.md) | Five curated lists checked against their written rules, with the exact gate on each | 历史调研，投稿前重新核实 |
| [`06-directory-survey.md`](06-directory-survey.md) | Category-specific directories (terminal coding agents, agent harnesses) checked against their contributing rules, with two ready-to-submit entries | 历史渠道： `awesome-cli-coding-agents`, `awesome-harness-engineering` |
| [`07-agent-loop-streaming.md`](07-agent-loop-streaming.md) | 从 Prompt 到 AgentEnd 的异步流式 Agent Loop | 掘金 / 知乎 / Rust 社区 |
| [`08-provider-abstraction-and-offline-testing.md`](08-provider-abstraction-and-offline-testing.md) | Provider 抽象、多模型协议与离线 faux 测试 | Rust / LLM 工程社区 |
| [`09-tool-execution-and-sandbox.md`](09-tool-execution-and-sandbox.md) | AgentTool、ExecutionEnv 与可测试工具执行 | Rust / Agent 工具链社区 |
| [`10-session-jsonl-crash-recovery.md`](10-session-jsonl-crash-recovery.md) | JSONL 会话、流式帧进度与崩溃恢复 | 架构 / 后端 / Agent 社区 |
| [`11-steering-follow-up-queues.md`](11-steering-follow-up-queues.md) | Steering 与 Follow-up 消息队列设计 | Agent / 交互式开发工具社区 |
| [`12-plugin-abi-design.md`](12-plugin-abi-design.md) | Rust cdylib 插件 ABI、版本协商与 panic 隔离 | Rust / 系统编程社区 |
| [`13-remote-server-client.md`](13-remote-server-client.md) | 无头远程 Agent 服务与零状态 TUI 客户端 | DevOps / 远程开发社区 |
| [`14-context-compaction-and-skills.md`](14-context-compaction-and-skills.md) | Context compaction、Skills 与 Harness 分层 | LLM 应用 / Agent 架构社区 |
| [`15-tui-rendering-performance.md`](15-tui-rendering-performance.md) | 长会话 TUI 的缓存渲染与性能优化 | Rust / TUI / 性能社区 |

| [`16-release-and-cross-platform-distribution.md`](16-release-and-cross-platform-distribution.md) | 发布与跨平台分发 | Rust / DevOps 社区 |

| [`16-release-and-cross-platform-distribution.md`](16-release-and-cross-platform-distribution.md) | 发布与跨平台分发 | Rust / DevOps 社区 |

另见 [Langfuse 扩展介绍](langfuse.md)。

`04`、`05`、`06` 是带时间背景的渠道调研，保留作历史参考。第三方规则、项目 stars/downloads 与投稿资格应在投稿前重新核实，不能将旧调研当作当前结论。

当前使用说明见 [文档索引](../README.md)，发布历史见 [CHANGELOG](../../CHANGELOG.md)。
