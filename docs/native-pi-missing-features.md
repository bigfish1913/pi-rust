# pi-rust 相对原生 Pi 的功能缺失审计

审计日期：2026-09-12（**功能状态更新：见下方「实现进展」**）
当前 Rust：`a072f31570ee3477b45c14f8304c1abf21b1fcb4`
原生 Pi：`earendil-works/pi@71dca871bc80b6bc97be37f0ca3189399d651fff`

## 实现进展（本轮已落地）

下表记录本轮把审计项从「缺失」推进到「已实现」的部分。标记为已实现的项目仍保留原文，
以便对照当时判定依据；新增的入口在「落地位置」列。

| 审计项 | 状态 | 落地位置 |
|---|---|---|
| §4 图片输入链路 | 已实现 | `pi-cli/src/app.rs` (`process_file_args`/`image_content_from_path`)、`interactive_tui.rs` 的 `Event::Paste` 图片拖入/粘贴路径 |
| §6 AgentHarness/AgentLane operation API | 已实现 | `pi-harness/src/agent_harness.rs`：`get_tip_id`/`find_entries`/`find_entry`/`get_entry`/`append_message`/`append_custom_entry`/`get_name`/`set_name`/`get_label`/`set_label`/`get_stats`/`snapshot`/`inspect_execution`/`get_result`/`accept`/`request_abort`/`drive`/`resume`（`AgentLane` trait + `AgentHarness`/`LaneHandle` 双实现），`watch`/`watch_session` + `HarnessWatcher`（`pi-harness/src/watcher.rs`） |
| §7 Durable operation runtime / value store | 已实现 | `pi-harness/src/runtime.rs`（`SessionRuntime`：`admit`/`recover_lane`/`reconcile`/`checkpoint`/`recover_all_lanes`，`LaneRecovery`+`RecoveryFinding`+`RecoveryDecision`）；`pi-harness/src/session/values.rs`（`SessionValues`/`SessionValueWriter`）。CLI 启动时对每条 lane 跑恢复扫描（`pi-cli/src/session.rs`）。
| §8 产品层 AgentSession | 已实现 | `pi-cli/src/agent_session.rs`（prompt/continue/steer/queue、model/thinking、scoped models、compact/retry、usage/context stats、HTML/JSONL/Markdown 导出、fork、value 持久化）。TUI `/usage`、`/export md\|html\|jsonl` 与 Ctrl+T/Ctrl+O 会话级偏好都走这一层。
| §9 JSON 细粒度事件流 | 已实现 | `pi-cli/src/remote/protocol.rs`（`RemoteEvent::from_agent_event`，覆盖 agent/turn/message/delta/tool/retry）；`pi-cli/src/modes.rs` 的 json/rpc 模式逐条输出。
| §10 导出格式与 CLI export | 已实现 | `pi-cli/src/export.rs`（`ExportFormat` + `export_session`/`export_file`，Markdown/HTML/JSONL）；`--export <in> [out]` 与 `/export <fmt>`。
| §11 已识别但未生效的 CLI 功能 | 已实现 | `--offline`、`--approve`/`--no-approve`、`--tui-mode`、`--no-themes`、`--theme`、`--list-models [search]`、`--export` 均已接入行为（`pi-cli/src/app.rs`、`session.rs`、`interactive_tui.rs`）。
| §12 交互快捷键与编辑器工作流 | 已实现 | `interactive_tui.rs` 键循环：Ctrl+G 外部编辑器、Ctrl+O 输出展开、Ctrl+T thinking 折叠、Ctrl+M 模型循环、Shift+Tab thinking 档位循环；`pi-tui/src/keybindings.rs` 可配置键位。
| §15 TUI 基础组件 | 已实现 | `pi-tui/src/`：`ArminComponent`、`BorderedLoader`、`MouseRegion` 等价事件处理、`Editor` 全量接口。
| §1 Provider 覆盖 | 部分实现（实用子集） | 新增 `pi-ai/src/providers/{deepseek,openrouter,llama_cpp,proxy}.rs`，复用 anthropic SSE 解析；未追求覆盖原生全部 ~40 个 provider。`pi-ai/src/constrained.rs` 提供约束采样（JSON schema/grammar/regex）。 |
| §2 OAuth 与 credential 工具 | 已实现（基础） | `pi-cli/src/oauth.rs`：device-code 登录、token 刷新/过期、token store。 |
| §5 ModelRegistry/ModelRuntime | 已实现 | `pi-cli/src/model_registry.rs`：`models.json` 运行时刷新、`ModelDefinition`→`Model` 转换、查询与重载。 |
| §13 `/llama` 集成 | 已实现 | `pi-cli/src/llama_command.rs`：`rpi llama start|stop|list|status|download`（手工参数解析，无 clap）；`pi-ai/src/providers/llama_cpp.rs` 运行时。 |
| §14 Trust gate 细节 | 已实现（基础） | `pi-cli/src/trust.rs`（`TrustStore`/`TrustDecision`）；`pi-cli/src/resource_dirs.rs` 的 `discover_system_prompt_file_with_trust` 按信任策略门控。 |
| §18 生产 telemetry schema | 已实现 | `pi-telemetry/src/production.rs`：`ProductionTelemetryContext` 输出 JSON span 记录。 |
| 基础设施（会话/工具） | 已实现 | `pi-harness/src/session/{keyed_queue,resources,search}.rs`（按 key 操作队列、资源清理注册表、会话全文检索）；`pi-tools/src/{image_processing.rs,tools/image.rs,utils/{git,open_browser}.rs}`（图片缩放/转码/EXIF、图片工具、Git URL 解析、跨平台打开浏览器）。 |
| 基础设施（本轮） | 已实现 | `pi-cli/timings.rs`（PI_TIMING 启动计时）、`pi-cli/experimental.rs`（PI_EXPERIMENTAL）、`pi-cli/session_cwd.rs`（会话 cwd 丢失恢复）、`pi-cli/fs_watch.rs`（文件监听，含错误重试）、`pi-cli/source_info.rs`（资源来源）、`pi-cli/auth_guidance.rs`（登录指引）、`pi-cli/changelog.rs`（远程 changelog）、`pi-cli/attribution.rs`（provider 归因头）。 |
| §7 缓存统计（cache-stats） | 已实现 | `pi-harness/cache_stats.rs`：提示缓存浪费统计（噪声下限/idle/模型切换/漏读计价），TUI cache-miss notice 改用该逻辑。 |
| §4 传输：HTTP 代理 + 空闲超时 | 已实现 | `pi-ai/http.rs`：HTTP(S)_PROXY/ALL_PROXY/NO_PROXY 解析（含通配/host:port，拒绝 SOCKS/PAC）、池化 idle timeout、UA；provider 构造统一接入；settings 新增 `httpIdleTimeout`。 |
| 语法高亮 | 已实现 | `pi-tui/syntax_highlight.rs`（无依赖轻量高亮，块注释跨行），markdown 代码块接入。 |
| 富 footer | 已实现 | `pi-tui/footer.rs` 两行状态栏（pwd(分支)•会话 / ↑↓RWCH$ 用量 + 上下文占比 + 模型右对齐）；`.git/HEAD` 经 fs_watch 实时刷新。 |
| §5 远程模型目录 | 已实现 | `pi-cli/remote_catalog.rs`：pi.dev `/api/models/providers/<id>` 拉取 + ETag/Last-Modified + 4h 窗口 + 覆盖合并 + 缓存；`--list-models` 接入。 |
| 图片生成（SDK） | 已实现 | `pi-ai/images.rs`：ImagesApi/ImagesModel/AssistantImages + registry + `generate_images`；内置 openrouter-images provider。 |
| §3 二进制协议（CBOR） | 已实现 | `pi-cli/remote/{framing,cbor,codec}.rs`：4 字节分帧 + 严格 CBOR + 流式 MessageDecoder。 |
| §1 自定义 provider / UA / 归因 | 已实现 | `model_registry` 注册 models.json 新 provider；`http::user_agent`/`ensure_user_agent` 接入 openai-completions/deepseek/openrouter；`attribution.rs` 归因头并入模型头。 |

仍待实现（保留在原文，未在本轮落地）：§1 的**全量** provider 覆盖（按约定只落地 OpenRouter/DeepSeek/LLaMA.cpp/Proxy 子集）、§16 JS bridge 剩余接口 `pi.sendMessage`/`pi.sendUserMessage`（已决定不做）。其余审计项（含 §2/§5/§7/§13/§14/§18、图片生成、CBOR 协议、HTTP 代理、语法高亮、富 footer、远程目录、fs-watch、缓存统计、环境开关、session cwd、结构化诊断、自定义 provider/UA/归因/指引/changelog）均已落地。

## 判定标准

这里只记录“原生 Pi 已提供可用功能，而当前 `pi-rust` 没有可用实现”的缺失。以下情况不计入：

- 同一功能的目录、文件格式、默认路径或协议实现不同；
- Rust 已经能完成用户目标，只是 API 名称或内部架构不同；
- 原生 Pi 自身默认不提供的能力（例如 subagents、plan mode）；
- 已在当前分支实现的旧 gap 文档项目（例如 `/new`、`/resume`、基础 TUI、SQLite session）。

## 优先级总览

| 优先级 | 缺失范围 | 影响 |
|---|---|---|
| P0 | Provider/API 覆盖、OAuth | 大量原生配置无法运行（图片输入 §4 与 RPC/远程 §3 已实现） |
| P1 | 模型 registry（§5） | 运行时 catalog 变更与统一 provider 解析仍缺失 |
| P1 | Trust/resource gate 细节（§14）、Settings 未消费字段（§15） | 项目安全策略与部分配置面不完整 |
| P2 | JS/TS 扩展桥接剩余接口（§16）、telemetry schema（§18） | 扩展生态和低层 SDK 兼容性受限 |

已从本表移除（本轮实现）：§2 OAuth、§5 ModelRegistry 运行时、§6 AgentHarness/AgentLane API、§7 durable runtime/value store、§8 AgentSession、§9 JSON 细粒度事件、§10 导出格式、§11 CLI 功能开关、§12 交互快捷键、§13 `/llama`、§14 trust gate 细节、§17 TUI 基础组件、§18 telemetry schema。仍保留：§1 provider **全量**覆盖（仅落地实用子集）、§15 settings 其余未消费字段、§16 JS bridge 剩余接口（已决定不做）。

## P0：模型、认证和传输

### 1. 原生 provider/API 大量没有运行时实现

当前 Rust provider 目录只有 `faux`、`anthropic`、`openai_completions`、`openai_responses`（`crates/pi-ai/src/providers/mod.rs`）。原生 `packages/ai/src/providers` 还提供以下 provider，但 Rust 没有对应的 HTTP/鉴权/流式运行时：

`amazon-bedrock`、`ant-ling`、`azure-openai-responses`、`baseten`、`cerebras`、`cloudflare-ai-gateway`、`cloudflare-workers-ai`、`deepseek`、`fireworks`、`github-copilot`、`google`、`google-vertex`、`groq`、`huggingface`、`kimi-coding`、`minimax`、`minimax-cn`、`mistral`、`moonshotai`、`moonshotai-cn`、`nvidia`、`openai-codex`、`opencode`、`opencode-go`、`openrouter`、`qwen-token-plan*`、`together`、`vercel-ai-gateway`、`xai`、`xiaomi*`、`zai`、`zai-coding-cn`，以及 OpenRouter/provider-specific image API。

`Api` enum 中预留名称不等于 provider 已实现；没有 provider 注册和请求实现的 API 仍然不可用。

证据：

- Rust：`crates/pi-ai/src/providers/mod.rs`、`crates/pi-ai/src/types.rs`
- 原生：`packages/ai/src/providers/`、`packages/ai/src/api/`

### 2. OAuth、订阅登录和 credential 工具缺失

Rust `rpi auth` 当前只真正支持 Anthropic API key 的 `login/check/logout`，源码也明确标注 OAuth 未移植。缺失内容包括：

- Claude Pro/Max Anthropic OAuth/device-code 登录；
- OpenAI Codex、GitHub Copilot、OpenRouter、xAI、Kimi 等 OAuth；
- OAuth token refresh、过期时间和 `--min-expiry`；
- `auth print-api-key`、`auth print-bearer-token`；
- 原生 provider 选择器和按 provider 的完整 credential 解析。

证据：`crates/pi-cli/src/auth.rs`；原生 `packages/ai/src/auth/`、`packages/ai/src/auth/oauth/`、`packages/coding-agent/src/cli/auth-command.ts`、`credential-print.ts`。

### 3. RPC 模式、协议和远程 client/server —— 已实现

> 状态：**已实现**。`--mode rpc` 的 JSONL 服务端、`rpi --server` 无头模式、
> `rpi --connect` 远程 TUI，以及 `--token` 认证均已落地。详见
> [`remote-mode.md`](remote-mode.md)。

rpi 现在自带一套 **Rust 原生**的远程协议与会话模型（`crates/pi-cli/src/remote/`）：
协议类型服务端/客户端共用（`protocol.rs`），客户端有独立的
`RemoteSession` + transcript 抽象（`session.rs`）与 `pi-tui` 渲染层（`tui.rs`）。
设计上参考了原生 pi `packages/coding-agent/src/modes/rpc/` 与 `src/client/` 的分层，
但**不依赖任何 pi 运行时组件**。

证据：Rust `crates/pi-cli/src/modes.rs`、`crates/pi-cli/src/remote/{protocol,client,session,tui}.rs`；
`rpi-package/packages/rpi-server/`。

### 4. 图片输入链路缺失 —— 已实现（见「实现进展」）

Rust `process_file_args` 对图片直接报 `image attachments are not supported in v1`（`crates/pi-cli/src/app.rs:416`），随后只把文本文件作为字符串传入，`prompt_text` 的 image 参数没有从 CLI 附件转发。缺失内容包括：

- `@image.png` 等初始附件的 MIME 检测和 base64 `ImageContent`；
- terminal clipboard 图片粘贴和拖拽；
- 图片自动缩放、`blockImages` 设置；
- tool result 中的图片内容。

证据：Rust `crates/pi-cli/src/app.rs`；原生 `packages/coding-agent/src/utils/image-process.ts`、`clipboard-image.ts`、`tool-result-images.ts`。

## P1：Harness、Session 和模型运行时

### 5. 原生 ModelRegistry/ModelRuntime 没有等价运行时

Rust 主要在启动时读取 provider/model 配置并建立快照。缺失的是原生统一运行时提供的动态能力：

- `models.json` 异步刷新和 provider availability refresh；
- `getProvider`、`getAuth`、provider auth status；
- `registerProvider` / `unregisterProvider`；
- extension provider 与内建 provider 的统一解析；
- 运行时模型 catalog 变更后向 TUI/会话同步。

证据：Rust `crates/pi-cli/src/provider.rs`、`crates/pi-cli/src/session.rs`；原生 `packages/coding-agent/src/core/model-registry.ts`、`model-runtime.ts`、`model-resolver.ts`。

### 6. 最新 AgentHarness/AgentLane operation API 缺失 —— 已实现（见「实现进展」）

当前 `crates/pi-harness/src/agent_harness.rs` 仍以旧的 prompt/queue/compact/navigation 接口为主，没有原生最新 lane/runtime 暴露的完整 operation surface，包括：

- `getTipId`、`findEntries`、`findEntry`、`appendMessage`、`appendCustomEntry`；
- `getResult`、`accept`、`drive`、`requestAbort`、`inspectExecution`；
- deferred operation 的 `resume`；
- `watch`/`watchSession` 和 lane snapshot；
- harness 名称/标签、stream options、retry、compaction、steering/follow-up 等完整 getter/setter。

Rust 能识别 `Suspended`，但 CLI 明确输出“resume is not supported in v1”，因此 deferred run 不能继续。

证据：Rust `crates/pi-harness/src/agent_harness.rs`、`crates/pi-cli/src/modes.rs:101`、`interactive_tui.rs:4067`；原生 `packages/agent/src/harness/agent-harness.ts`、`packages/agent/src/harness/runtime/`。

### 7. Durable operation runtime/recovery/value store 缺失 —— 已实现

provider 端长轮询续跑（`resume_deferred`）也已接通：`pi-ai/src/provider.rs` 的
`DeferredProvider` + 默认的 `Provider::deferred()`、`pi-agent` 的
`run_agent_loop_from_assistant`、`pi-harness` 的 `resume_deferred`。
仍未做的只是「哪个**真实** provider 去实现 long-poll API」——目前只有 faux 驱动。
细节见 `docs/llm-repetition-forensics.md` §11.7.18。

Rust 有 JSONL、内存和 SQLite session 存储，但没有原生新增的 durable operation 分层：admission/drive/recovery/reconcile/checkpoint、deferred polling/resume、operation state/value store、pending assistant/tool frame 持久化、lane snapshots 和 recovery events。原生 `packages/agent/src/harness/runtime/` 及 `packages/coding-agent/src/core/session/{commit,fork,fork-policy,values}.ts` 均有对应实现，Rust session 目录没有 `values` 和 operation runtime 层。

### 8. coding-agent 产品层 AgentSession 缺失 —— 已实现（见「实现进展」）

Rust CLI 直接操作 `AgentHarness`，没有原生 `AgentSession` 这一层统一承载：prompt/continue、queue、model/thinking mutation、scoped models、compaction/retry、bash、HTML/JSONL export、session switching、tree/fork、extension binding、usage/context stats、reload、auto compaction/retry。缺失会让依赖 coding-agent 产品 API 的调用方无法直接迁移。

证据：原生 `packages/coding-agent/src/core/agent-session.ts`；Rust `crates/pi-cli/src/interactive_tui.rs`、`crates/pi-cli/src/session.rs`。

### 9. JSON 输出没有原生细粒度事件流 —— 已实现（见「实现进展」）

Rust `--mode json` 的事件投影只有 `run_start`、`run_end` 和最终 `result`（`crates/pi-cli/src/modes.rs:222-235`）。原生 JSON mode 还会输出 agent/turn 生命周期、message start/update/end、文本和 thinking delta、tool execution start/update/end、compaction/retry/session 事件。当前 harness 虽有内部 event bus，但没有把这些细粒度事件投影到 CLI JSON 合同。

证据：Rust `crates/pi-cli/src/modes.rs`、`crates/pi-harness/src/events.rs`；原生 `packages/coding-agent/src/modes/json-event.ts`。

### 10. 原生导出格式和 CLI export 缺失 —— 已实现（见「实现进展」）

Rust `/export` 只生成当前目录下的 Markdown（`crates/pi-cli/src/interactive_tui.rs:2115` 附近），`--export <file>` 目前只是识别后忽略。缺失内容包括：

- `AgentSession.exportToHtml()`；
- `AgentSession.exportToJsonl()`；
- HTML theme、tool renderer、语法高亮和输出路径处理。

证据：原生 `packages/coding-agent/src/core/export-html/`、`agent-session.ts:3463-3488`；Rust `interactive_tui.rs`、`args.rs`。

## P1：CLI、TUI、资源和设置

### 11. 已识别但未生效的 CLI 功能 —— 已实现（见「实现进展」）

当前 parser 对以下原生参数只接受/警告，没有实现其原生行为：

- `--export <file>`；
- `--offline`；
- `--approve` / `--no-approve`（没有真正控制 trust）；
- `--tui-mode regular|fullscreen`；
- `--no-themes`；
- 原生语义的 `--use-theme <name>`；
- `--list-models [search]`（当前明确提示 unsupported/ignored）。

证据：Rust `crates/pi-cli/src/args.rs:339-390`、`app.rs`；原生 `packages/coding-agent/src/cli/args.ts`。

### 12. 交互快捷键和编辑器工作流缺失 —— 已实现（见「实现进展」）

当前 TUI 已有输入、模型切换、工具展开等基础操作，但仍缺少原生提供的独立功能：

- Ctrl+G 外部编辑器（`externalEditor` / `$VISUAL` / `$EDITOR`）；
- Ctrl+O 的 tool-output filter/collapse cycle；
- Ctrl+T 对 thinking block 的折叠/展开（Rust Ctrl+T 当前只作用于最近 tool）；
- Shift+Tab thinking level cycle；
- 配置化 keybindings、double-escape action、mouse region/click workflow；
- 图片粘贴/拖拽入口。

证据：Rust `crates/pi-cli/src/interactive_tui.rs:3151,3671`、`crates/pi-tui/src/keybindings.rs`；原生 `packages/coding-agent/README.md:167-218,263`、`settings-manager.ts`。

### 13. 原生 slash command / llama.cpp 集成缺失

当前 registry 已覆盖 `/new`、`/resume`（alias）以及 tree/fork/import 等命令，但仍缺少：

- `/changelog`：显示版本历史；
- `/llama`：连接 llama.cpp router，下载、加载、卸载模型，并配合 `/login llama.cpp` 和 `/model` 使用。

证据：Rust `crates/pi-cli/src/interactive_tui.rs:1317-1350`；原生 `packages/coding-agent/src/core/slash-commands.ts:31`、`packages/coding-agent/README.md:139,180,200`。

### 14. Trust gate 和资源发现仍不完整

Rust 已有 `.rpi/.pi` 资源发现、package 资源和部分 collision diagnostics，但仍缺少原生安全/兼容行为：

- `.agents/skills` 和 `~/.agents/skills`；
- 未信任项目时限制 project-local resources/extensions 的 trust prompt/gate；
- worktree shadowed context-file 去重；
- 完整 skill metadata 校验及结构化 winner/loser collision diagnostics。

Rust 代码明确说明项目资源当前无条件加载，`trust.json` 只做布局读写，不参与 gate。

证据：Rust `crates/pi-cli/src/resource_dirs.rs:20-29,106-107,212`、`session.rs:402`、`config.rs:364-366`；原生 `packages/coding-agent/src/core/resource-loader.ts`、`project-trust.ts`、`trust-manager.ts`。

### 15. Settings 可控功能面缺失 —— 已大幅收窄（2026-09-25 复核）

**复核结论（逐字段 grep 过）**：rpi 的 `Settings`（`pi-cli/src/settings.rs`）里
**24 个字段全部有读取点，没有一个"建模了但没人读"**。下面原文里点名的那批
（hide thinking、external editor、quiet startup、trust、terminal progress、
image resize、default tools、doubleEscapeAction、tree filters、UI padding/autocomplete、
markdown/http timeout 等）**多数已经落地并接线**。

仍然剩下的分三类（键名对照原生 `settings-manager.ts` 的 52 个键）：

**A. 同一能力、键名不同**（写原生的键名不生效）：
`enabledModels`→rpi `scopedModels`；`httpIdleTimeoutMs`→`httpIdleTimeout`；
`images`→`showImages`（只覆盖布尔，原生的 `images` 是对象）。

**B. 能力在别处有，但 settings.json 里写了不生效**：
`retry`/`compaction`/`branchSummary`（harness 有对应类型，构造时硬编码默认）；
`steeringMode`/`followUpMode`（同）；`sessionDir`（有 CLI `--session-dir`）；
`httpProxy`（只读 `HTTP_PROXY` 等环境变量，见 `pi-ai/src/http.rs`）；
`tuiMode`（有 CLI `--tui-mode`）；`externalEditor`（只读 `EDITOR`/`VISUAL`）；
`showHardwareCursor`、`fullscreenExitOutput`、`fullscreenScrollbar`、
`thinkingBudgets`（类型有、无设置项）；`shellPath`/`shellCommandPrefix`
（bash 工具有 `command_prefix`，但 `bash_options()` 里是 `None`，未接线）；
`markdown`/`warnings` 子设置。

**C. rpi 完全没有**：`modelThinkingLevels`、`defaultProjectTrust`（有 `TrustStore`，
但没有这个全局设置项）、`collapseChangelog`、`enableInstallTelemetry`、
`enableAnalytics`、`trackingId`（`extras.rs` 里只有一句"v1 简化为一次性 banner"）、
`enableSkillCommands`、`treeFilterMode`、`cacheWarming`、`websocketConnectTimeoutMs`。

（以下为 2026-09-12 的原始审计，保留以便对照当时判定依据。）

Rust `crates/pi-cli/src/settings.rs:19` 只真正建模并使用 provider/model/thinking/theme、scopedModels、packages 和 resource dirs；未知字段虽会保留，但不会产生行为。原生 settings 中以下功能因此不可用：retry、compaction、branch summary、steering/follow-up mode、transport（SSE/WebSocket/auto）、hide thinking、external editor、shell path/prefix、quiet startup、project trust、terminal image/progress/hyperlink/trueColor、image resize/block、enabled models/default tools、doubleEscapeAction、tree filters、thinking budgets、UI padding/autocomplete、markdown/mermaid/warning/http timeout 等。

证据：Rust `crates/pi-cli/src/settings.rs`；原生 `packages/coding-agent/src/core/settings-manager.ts:100-120` 及其 getter/setter 实现。

## P2：扩展和低层组件

### 16. JS/TS extension bridge 的能力缺失

Rust 原生 cdylib 扩展已有不少事件、provider、renderer 和 runtime bridge，不能整体视为缺失；但以下明确接口仍是 stub/unsupported：

- C ABI `register_shortcut` 返回成功但不注册（`crates/rpi-extensions/src/lib.rs:410-414`）；
- Node host 的 `on(event, handler)` 只处理 `resources_discover`，不是完整事件面；
- `ui.select`、`ui.confirm`、`ui.input`、`ui.editor` 直接抛 `unsupported capability`；
- `session.sendMessage`、`session.sendUserMessage` 直接抛 `unsupported capability`；
- message renderer、markdown transformer、entry renderer 等原生 JS extension 注册面未完整暴露。

证据：Rust `crates/rpi-extensions/src/lib.rs`、`crates/pi-cli/src/node_host.mjs:240-243,342-343,440`、`js_extensions.rs`；原生 `packages/coding-agent/src/core/extensions/types.ts`、`runner.ts`。

### 17. TUI 基础组件没有直接等价物 —— 已实现（见「实现进展」）

原生 `packages/tui/src/components` 的 `AltScreenFlash`、`MouseRegion` 以及 `editor-component.ts` 接口，在 `crates/pi-tui/src` 没有直接等价 API。因此依赖这些组件的原生 TUI/extension 不能直接迁移。

### 18. 生产 telemetry schema/backend 不完整（SDK 级）

Rust telemetry 目前以 noop backend 和少量 span contract/testing memory context 为主；原生还提供 typed telemetry schemas、typed span starter，以及 agent/AI/harness 的丰富事件和属性。若以 SDK 兼容为目标，这部分属于低优先级功能缺失；若只审计 CLI 用户功能，可暂不纳入发布阻断项。

证据：Rust `crates/pi-telemetry/src/{lib,types,noop,testing,context}.rs`；原生 `packages/telemetry/` 及 agent/AI/harness telemetry schema。

## 明确不算“缺失”的项目

- Rust 已有基础 TUI、assistant/tool/footer/selector 等组件；旧 `docs/tui-gap-analysis.md` 中“完整 TUI 缺失”的表述已过时。
- `/new`、`/resume` 已通过当前命令 registry 的 alias 提供；真正缺失的 `/changelog` 和 `/llama` 已在上文单列。
- Rust 已有 `/tree`、`/fork`、`/clone`、`/compact`、`/import`、`/share` 和基础 `/export`；缺失的是 export 格式和 CLI export 能力，不是整个命令不存在。
- Rust 已有 read/write/edit/bash 以及 grep/find/ls 工具；不把工具实现差异写成缺失。
- Rust 已有 SQLite session backend；`.rpi`/`.pi` 路径、默认 session 目录、provider route 等差异属于实现/兼容策略，不单列为功能缺失。
- 原生 Pi 默认也不带 subagents、plan mode，因此不列为 Rust 缺失。
