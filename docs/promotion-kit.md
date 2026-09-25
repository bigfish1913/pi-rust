# rpi 投放素材（0.3.0）

本文件用于发布 rpi 的开源推广内容。发布前请根据平台规则调整措辞，不要在多个社区原样重复发帖。

**发布前务必核对本文末尾的检查清单。**本文件上一版停留在 0.1.10，已落后两个大版本。

## 统一信息

| 项目 | 内容 |
| --- | --- |
| 项目 | rpi —— Rust 原生、library-first 的 coding-agent runtime + 终端 agent |
| 稳定版本 | **0.3.0**（9 个 crate 同版本一起发布） |
| MSRV | Rust 1.78 |
| 许可 | MIT |
| GitHub | https://github.com/bigfish1913/pi-rust |
| 官网 | https://rpi.laofu.online/ |
| 文档 | https://rpi.laofu.online/docs.html |
| crates.io | https://crates.io/crates/rpi-cli |
| 扩展包仓库 | https://github.com/pi-rust/rpi-package |
| 安装（预编译） | `curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh \| sh` |
| 安装（Cargo） | `cargo install rpi-cli` |
| 启动 | `rpi` 或 `rpi -p "hello"` |

### 0.3.0 可以讲的稳定能力

- 异步、流式的 agent loop；`AgentTool` trait、事件、hooks、队列、取消。
- Provider：Anthropic Messages、OpenAI Chat Completions、OpenAI Responses，以及 OpenRouter / DeepSeek / llama.cpp 等 OpenAI 兼容网关；另有用于离线测试的 `faux` provider。
- 内置工具：`read`、`write`、`edit`、`bash`（CLI 默认加载的 Pi 兼容集合），另有 `grep`、`find`、`ls`、`powershell` 与 `ExecutionEnv` 抽象（内存/OS 两套后端）。
- Session：JSONL 持久化、分支、上下文压缩，以及**崩溃恢复**——按流式帧记录进度，进程中途死亡后可续跑；排队的 steering / follow-up 消息不会随进程丢失。
- 远程模式：`rpi --server` 无头 TCP 服务 + `rpi --connect` 零本地资源 TUI 客户端，连接级 token 认证。
- 插件：`rpi-plugin-sdk` 提供稳定 `#[repr(C)]` ABI（含 panic 隔离、版本协商），`rpi-extensions` 是宿主侧加载器；`rpi install` / `rpi dev` 覆盖安装与热重载。
- 性能：release 二进制 23.9 MiB，`rpi --version` 中位 17.5 ms（Windows 11 / rustc 1.97.1，`sh scripts/bench.sh` 可复现）。

### 必须标注为实验性的部分

不要在稳定卖点里出现以下内容：

- **Node/TypeScript 扩展桥接**：需要 `--enable-pi-packages` 显式开启，且接口面不完整——`ui.select` / `ui.confirm` / `ui.input` / `ui.editor` 与 `session.sendMessage` 目前直接抛 `unsupported capability`，事件面只覆盖 `resources_discover`。描述为「实验性、仅本地评估」。
- **插件 ABI 统一**：0.3.0 把入口统一到单一 `rpi_plugin_register`（版本号移入 `PluginApi` 结构体），宿主仍会回退解析 `_v3` / `_v2`，老插件可继续加载；但扩展作者应重新构建。
- **benchmark 对比**：目前只有 rpi 自己的数字，没有与其他 agent 的同机对比。不要编造对比数据。

## 中文短文案

### V2EX / Linux.do / Rust 中文社区

标题：

> rpi：Rust 原生的 coding-agent runtime，既能当 CLI 也能嵌进你的进程

正文：

> rpi 是一个 library-first 的 Rust coding-agent runtime，九个 crate 同版本发布，入口既可以是终端里的 `rpi` 命令，也可以是 `rpi-agent` 这个库。
>
> 0.3.0 里比较实在的部分：
>
> - 流式 agent loop，带工具调用、hooks、队列和取消
> - Provider 抽象：Anthropic、OpenAI 兼容（含 OpenRouter / DeepSeek / llama.cpp 网关），以及不联网即可测试的 faux provider
> - Session 是树而不是日志：JSONL 持久化、分支、上下文压缩
> - **崩溃恢复**：按流式帧记录进度，进程中途挂掉之后能续跑，而不是丢掉整轮；排队消息也不会随进程消失
> - 远程模式：`rpi --server` 起无头服务，`rpi --connect` 用零本地资源的 TUI 客户端连过去
> - 插件走稳定 `#[repr(C)]` ABI，`rpi install` 装、`rpi dev` 热重载
>
> 想试的话不必先装 Rust 工具链：
>
> ```bash
> curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
> rpi
> ```
>
> 或者 `cargo install rpi-cli`。
>
> Node/TypeScript 扩展桥接目前仍是实验性的（需要 `--enable-pi-packages`，接口面不完整），请只用于本地评估。
>
> 项目地址：https://github.com/bigfish1913/pi-rust
> 官网和文档：https://rpi.laofu.online/
>
> 欢迎 Rust、LLM Agent 和开发者工具方向的朋友试用、反馈和贡献。

## Hacker News

标题：

> Show HN: rpi – A Rust-native, library-first coding-agent runtime

正文：

> rpi is a Rust-native coding-agent runtime, published as nine composable crates, plus a terminal agent built on them.
>
> The part I care about is that the agent loop is a library, not a CLI you shell out to. `rpi-agent` + `rpi-ai` is enough to run an agent inside your own process with your own event handling.
>
> Highlights:
>
> - Streaming agent loop with tool calls, hooks, queues and cancellation
> - Anthropic Messages, OpenAI Chat Completions/Responses, OpenRouter/DeepSeek/llama.cpp gateways, and a deterministic `faux` provider for offline tests
> - Sessions as a tree: JSONL persistence, branching, compaction, and crash recovery — per-frame progress means a process that dies mid-tool-call resumes instead of losing the turn
> - Remote mode: a headless `--server` plus a `--connect` TUI client that holds no local agent state
> - Plugins are Rust `cdylib`s behind a hand-written `#[repr(C)]` ABI with version negotiation and panic containment
>
> The default build pulls no HTTP stack at all (providers are feature-gated), so the test suite cannot silently depend on the network.
>
> ```bash
> curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
> # or: cargo install rpi-cli
> rpi -p "hello"
> ```
>
> Measured on Windows 11 / rustc 1.97.1: 23.9 MiB release binary, 17.5 ms median for `rpi --version`. I have not benchmarked it against other agents yet, so I am not claiming a comparison.
>
> Known gaps, stated up front: the Node/TypeScript extension bridge is experimental and incomplete, and Intel macOS prebuilt binaries are not built yet. There is a `docs/native-pi-missing-features.md` in the repo that tracks compatibility honestly.
>
> GitHub: https://github.com/bigfish1913/pi-rust
> Website: https://rpi.laofu.online/
>
> MIT licensed. Happy to answer questions about the crate split or the plugin ABI.

## Reddit

### r/rust

标题：

> [Showcase] rpi: a Rust coding-agent runtime split into nine composable crates

重点（r/rust 反感「又一个 AI CLI」自荐，要讲 Rust 本身）：

> - 单向依赖的 crate 分层：`rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`，插件 SDK 是零依赖叶子，避免插件拖入整个 runtime。
> - 用 trait 把 LLM 边界收成一个 `StreamFn`，provider、测试替身和录制回放三者可互换。
> - 工具参数用 `schemars` 从类型派生 schema，再用一层 coercion 修复模型略写错的 JSON。
> - 插件是手写的 `#[repr(C)]` ABI：不能有 `Drop` 类型跨界、字符串用 ptr+len 加显式 `free_string`、两边都 `catch_unwind`。文档里逐条写了为什么。
> - 默认构建不含 HTTP 依赖，测试离线可跑。
>
> 可以先 `cargo run -p minimal` 跑通一个不联网的 agent。

### r/LocalLLaMA / r/LLMDevs

标题：

> rpi: a Rust coding-agent runtime with pluggable providers and offline tests

重点：

> Provider 适配、工具循环、会话持久化与崩溃恢复、OpenAI 兼容 endpoint（含 llama.cpp / DeepSeek），以及不需要 API key 的 faux provider。发布前查看各版自荐规则。

## GitHub Release

Release 标题：

> rpi v0.3.0 — Rust-native coding-agent runtime

Release 摘要（正文主体从 `CHANGELOG.md` 的 0.3.0 小节复制，不要重复粘贴 0.1.x 的内容）：

> rpi v0.3.0 unifies the plugin ABI on a single `rpi_plugin_register` entrypoint and lands the crash-recovery work from 0.1.28.
>
> Install without a Rust toolchain:
>
> ```bash
> curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh
> ```
>
> Or with Cargo:
>
> ```bash
> cargo install rpi-cli
> ```
>
> See https://rpi.laofu.online/docs.html for the user guide and `CHANGELOG.md` for the full list.

> 注意：Release 必须附带预编译二进制（由 `.github/workflows/release-binaries.yml` 生成），否则上面的 install.sh 命令会失败。

## This Week in Rust

标题：

> Project Submission: rpi — a Rust-native, library-first coding-agent runtime

正文：

> Hi TWiR team,
>
> I would like to propose rpi for the Project/Crate of the Week.
>
> rpi is a coding-agent runtime written in Rust, published as nine composable crates with a terminal agent on top. The design goal is that the agent loop is a library: `rpi-agent` + `rpi-ai` is enough to embed an agent in your own process.
>
> Highlights:
> - Streaming agent loop with tool calls, events, hooks, queues and cancellation.
> - Provider-agnostic layer: Anthropic, OpenAI-compatible providers, and a deterministic `faux` provider so the test suite runs offline. HTTP providers are feature-gated; the default build has no network stack.
> - Durable sessions: JSONL persistence, branching, compaction, and crash recovery driven by frame-level progress records.
> - A stable `#[repr(C)]` plugin ABI (`rpi-plugin-sdk`) with version negotiation and panic containment across the boundary, and a host loader (`rpi-extensions`) that bridges plugin lifecycles into async tool implementations.
> - One-way crate dependency direction: `rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`.
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
> - Architecture: https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture.md
>
> MIT licensed. Thanks for reading.

## Awesome Rust

建议条目（按列表维护者要求的分类和格式提交，只发一个 PR，不要同时开 issue）：

> - [rpi](https://github.com/bigfish1913/pi-rust) - Rust-native, library-first coding-agent runtime with composable providers, tools, durable sessions and a stable plugin ABI.

## X / Bluesky / LinkedIn

> Introducing rpi 0.3.0 — a Rust-native, library-first coding-agent runtime.
>
> Nine composable crates: providers, agent loop, tools, durable sessions, plugin ABI, terminal CLI. Embed the loop in your own process, or just use the `rpi` command.
>
> No Rust toolchain needed to try it:
>
> `curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh`
>
> GitHub: https://github.com/bigfish1913/pi-rust
> MIT licensed.

## 其他渠道

RustCC 已发布过项目介绍，不再重复投放同一篇文章。后续按平台调整内容角度。

### RustCC（远程模式专题）

- 文章草稿：`docs/rustcc-post-remote-mode.md`
- 标题：

> rpi 远程模式：无头服务端 + 零本地资源的远程 TUI

- 角度：

> `rpi --server` 无头运行 + `rpi --connect` 零本地资源客户端 + `--token` / `RPI_SERVER_TOKEN` 认证。强调「客户端不复制 agent 实现」，说明线协议分层，附远程 TUI 可用命令与限制。**发布前先核对 `docs/remote-mode.md`，把版本号和命令与 0.3.0 对齐。**

### OSCHINA

标题：

> rpi：用 Rust 构建可嵌入的 coding-agent runtime

角度：

> 重点讲 `rpi-ai`、`rpi-agent`、`rpi-tools`、`rpi-harness` 的单向依赖，解释为什么 library-first，以及如何从 faux provider 开始做离线测试。结尾放安装命令和 GitHub 链接。

### 掘金

标题：

> 从 Prompt 到工具循环：一个 Rust Agent Runtime 的分层实践

结构：

> 1. Agent runtime 要解决哪些问题；
> 2. Provider、agent loop、tool、session 如何分层；
> 3. 用 faux provider + `InMemoryExecutionEnv` 写不依赖网络的测试；
> 4. 崩溃恢复为什么需要「帧级进度」而不是「回合级持久化」；
> 5. 插件 ABI 为什么要手写 `#[repr(C)]`（以及哪些类型不能跨界）。
>
> 主体写技术实践，项目介绍放末尾，避免被判定为纯广告。

### 知乎

采用问答形式，不直接复制项目公告：

> 问题方向：Rust 适合用来构建 LLM Agent 吗？
>
> 回答重点：trait 收窄 LLM 边界、异步流式、可测试执行环境、稳定 ABI 如何帮助构建长期运行的 coding agent。用 rpi 的四层代码示例说明，再附项目链接。

### DEV.to / Hashnode

标题：

> Building a Testable Coding Agent in Rust with rpi

重点：

> 从离线的 faux provider 开始，接入 `read`/`write`/`edit`/`bash`，订阅流式事件，最后引入 session 与崩溃恢复。Node/TypeScript 桥接放在明确标注的实验性章节里。

### Lobsters

标题：

> rpi: a Rust-native, library-first coding-agent runtime

正文保持短小，强调 crate 分层、离线测试和插件 ABI 的设计；附 GitHub、架构文档和最小运行命令。先确认账号满足社区发帖要求。

### 发布节奏

- RustCC 已发布项目介绍，不在相邻几天内重复同类中文文章。
- OSCHINA 和掘金间隔 3 至 5 天，使用不同标题和文章主体。
- 知乎、DEV.to / Hashnode 作为技术跟进，间隔 5 至 7 天。
- This Week in Rust 和 Awesome Rust 用项目提交 / PR 形式，不要当软文重复发布。

## 发布检查清单

- [ ] 版本号写 **0.3.0**；如果已经发了新版本，先更新本文件再对外发。
- [ ] 链接统一使用 `https://github.com/bigfish1913/pi-rust`，不要用旧仓库地址。
- [ ] 安装命令二选一，且**确认预编译二进制真的存在于该 Release**：
      `curl -fsSL .../scripts/install.sh | sh` 或 `cargo install rpi-cli`。
- [ ] 不要承诺 benchmark 对比——目前只有 rpi 自身的启动时间和二进制体积。
- [ ] Node/TypeScript 扩展桥接必须标注为实验性、需要 `--enable-pi-packages`。
- [ ] 每个平台使用一张最相关的截图：`docs/images/rpi-interactive.png` 或 `docs/images/rpi-working.png`。
- [ ] Hacker News / Reddit 用英文；中文社区用中文，并按版规选择分类。
- [ ] 发帖后优先回复安装、Provider 配置和插件扩展问题，不要连续重复推送。
- [ ] 发帖前确认 Release 页面最新 tag 与 crates.io 版本一致（本文件上一版就是因为这里脱节而过期）。
