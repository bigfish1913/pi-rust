# rpi 投放素材

本文件用于发布 rpi 的开源推广内容。发布前请根据平台规则调整措辞，不要在多个社区原样重复发帖。

## 统一信息

- 项目：rpi，基于 Pi agent 的 Rust 原生 Agent SDK 和终端 coding agent
- 版本：0.1.6
- GitHub：https://github.com/bigfish1913/pi-rust
- 官网：https://rpi.laofu.online/
- 文档：https://rpi.laofu.online/docs.html
- 安装：`cargo install rpi-cli`
- 启动：`rpi -p "hello"`
- 许可证：MIT

## 中文短文案

### V2EX / Linux.do / Rust 中文社区

标题：

> rpi：基于 Pi agent 的 Rust 原生 Agent SDK，支持工具调用、会话和插件

正文：

> rpi 是一个基于 Pi agent 的 Rust 原生实现，采用 library-first、多 crate 的设计，目标是让开发者可以把 Agent runtime 嵌入自己的应用或工作流。
>
> 目前包含：
> - 异步、流式的 Agent loop
> - Anthropic Messages、OpenAI-compatible 和 faux provider
> - `read`、`write`、`edit`、`bash`、`grep`、`find`、`ls` 等 coding-agent 工具
> - session、JSONL 持久化、上下文压缩和 prompt templates
> - `rpi-plugin-sdk` 与稳定 ABI 扩展能力
> - `rpi-cli` 终端编码 Agent
>
> 直接安装：
>
> ```bash
> cargo install rpi-cli
> rpi -p "hello"
> ```
>
> 项目地址：https://github.com/bigfish1913/pi-rust
>
> 官网和文档：https://rpi.laofu.online/
>
> 欢迎 Rust、LLM Agent 和开发者工具方向的朋友试用、反馈和贡献。

## Hacker News

标题：

> Show HN: rpi – A Rust-native Pi agent SDK and terminal coding agent

正文：

> rpi is a Rust-native implementation of the Pi agent SDK. It is library-first and split into composable crates for providers, agent runtime, tools, sessions, harnesses, plugins, and a terminal coding-agent CLI.
>
> Highlights:
> - Async, streaming agent runtime
> - Anthropic Messages, OpenAI-compatible chat completions, and a faux provider
> - Built-in coding tools: read, write, edit, bash, grep, find, and ls
> - Durable JSONL sessions, context compaction, hooks, queues, and prompt templates
> - Plugin SDK and stable ABI registration for tools, providers, events, and resources
>
> ```bash
> cargo install rpi-cli
> rpi -p "hello"
> ```
>
> GitHub: https://github.com/bigfish1913/pi-rust
>
> Website: https://rpi.laofu.online/
>
> The project is MIT licensed and currently at version 0.1.6.

## Reddit

### r/rust

标题：

> [Showcase] rpi: a Rust-native Pi agent SDK with composable crates and plugin ABI

重点：Rust crate 分层、异步流式 runtime、trait 设计、插件 ABI、测试和 faux provider。避免只强调“又一个 AI CLI”。

### r/LocalLLaMA / r/LLMDevs

标题：

> rpi: a Rust coding-agent toolkit with provider adapters, built-in tools, sessions, and plugins

重点：Provider 适配、工具循环、会话持久化、OpenAI-compatible endpoint 和不需要 API key 的本地示例。发布前查看各版自荐规则。

## GitHub Release

建议 Release 标题：

> rpi v0.1.6 — Rust-native Pi agent SDK and CLI

Release 摘要：

> rpi v0.1.6 provides the first complete path from provider and agent loop to built-in coding tools, sessions, harness, plugins, and the `rpi` terminal CLI.
>
> Install the CLI with:
>
> ```bash
> cargo install rpi-cli
> ```
>
> See the documentation at https://rpi.laofu.online/docs.html and the architecture guide at https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture.md.

## This Week in Rust

Issue title:

> Project Submission: rpi — Rust-native Pi agent SDK and terminal coding agent

Issue body:

> Hi TWiR team,
>
> I would like to propose rpi for the Project/Crate of the Week.
>
> rpi is a Rust-native implementation of the Pi agent SDK and a terminal coding-agent CLI. It follows a library-first design and splits providers, the async agent loop, coding tools, durable sessions, harnesses, plugins, and the CLI into composable crates.
>
> Highlights:
> - Async, streaming Agent runtime with tool calls, hooks, queues, and cancellation.
> - Anthropic Messages, OpenAI-compatible Chat Completions, and a deterministic faux provider.
> - Built-in `read`, `write`, `edit`, `bash`, `grep`, `find`, and `ls` tools.
> - JSONL session persistence, branching, prompt templates, and context compaction.
> - A stable `#[repr(C)]` plugin ABI through `rpi-plugin-sdk`, with a host-side dynamic loader.
> - A terminal CLI that can be installed with `cargo install rpi-cli`.
>
> Quick start:
>
> ```bash
> cargo install rpi-cli
> rpi -p "hello"
> ```
>
> Links:
> - Repository: https://github.com/bigfish1913/pi-rust
> - Website: https://rpi.laofu.online/
> - Crates.io: https://crates.io/crates/rpi-cli
> - Docs.rs: https://docs.rs/rpi-cli
> - Plugin SDK: https://crates.io/crates/rpi-plugin-sdk

## Awesome Rust PR

Suggested list entry for an appropriate AI / agent section:

> - [rpi](https://github.com/bigfish1913/pi-rust) - Rust-native Pi agent SDK and terminal coding-agent CLI with composable providers, tools, sessions, and a stable plugin ABI.

Use the list maintainer's preferred category and contribution format. Submit the
entry as a focused PR rather than opening multiple issues.

## X / Bluesky / LinkedIn

> Introducing rpi: a Rust-native implementation of the Pi agent SDK.
>
> Build composable coding agents with async streaming, provider adapters, built-in tools, durable sessions, and a plugin ABI.
>
> Install: `cargo install rpi-cli`
>
> GitHub: https://github.com/bigfish1913/pi-rust
> Website: https://rpi.laofu.online/

## 发布检查清单

- 确认链接使用 `https://github.com/bigfish1913/pi-rust`，不要使用旧仓库地址。
- 确认版本号为 `0.1.6`，不要写成 `0.1.x`。
- 确认安装命令是 `cargo install rpi-cli`。
- 每个平台使用一张最相关的截图，正文中说明截图展示的是 CLI 启动或工作状态。
- Hacker News 和 Reddit 使用英文；中文社区使用中文，并按版规选择分类。
- 发帖后优先回复安装、Provider 配置和插件扩展问题，不要连续重复推送。
- GitHub Issue、论坛帖子和社交平台公开提交前，确认标题、链接、截图和项目状态无误。
