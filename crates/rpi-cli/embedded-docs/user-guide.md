# rpi 使用手册

这是一份面向使用者和扩展作者的 rpi 手册。rpi 是基于 Pi agent 设计的 Rust 原生 coding-agent：核心能力以 crate 形式提供，`rpi` 是开箱即用的 CLI。本文记录当前版本已经实现的命令和行为，随 `rpi-cli` 一起发布。

## 1. 安装

### 直接安装 CLI

需要 Rust/Cargo：

```bash
cargo install rpi-cli
rpi --version
rpi --help
```

从本仓库源码安装开发版本：

```bash
git clone https://github.com/bigfish1913/pi-rust.git
cd pi-rust
cargo install --path crates/rpi-cli --force
```

`cargo install` 安装的是可执行文件；它不等同于 `rpi install`。后者用于安装由 `rpi-plugin-sdk` 构建的 Rust 动态库扩展。

### 从源码启动与构建

本项目是 Rust Cargo workspace，不是 Node/npm 项目，根目录没有 `package.json`。在仓库根目录执行：

```bash
# 直接启动 CLI
cargo run -p rpi-cli

# 向 CLI 传递参数（`--` 后的内容属于 rpi）
cargo run -p rpi-cli -- -p "总结 README.md"
cargo run -p rpi-cli -- --mode json -p "列出项目中的 crates"

# 构建 release 版本
cargo build -p rpi-cli --release
```

构建完成后，Windows 可执行文件位于 `target/release/rpi.exe`，Linux/macOS 下通常为 `target/release/rpi`。也可以把本地 CLI 安装到 Cargo 的 bin 目录后直接使用：

```bash
cargo install --path crates/rpi-cli --force
rpi
```

离线示例（不需要 API Key 或网络）可以使用：

```bash
cargo run -p minimal
```

### 第一次运行

交互模式：

```bash
rpi
rpi "解释当前项目的目录结构"
```

单次输出模式：

```bash
rpi -p "总结 README.md"
rpi --mode json -p "列出项目的风险"
```

通过 `@` 把文件放进第一条消息：

```bash
rpi @README.md "给出这个项目的改进建议"
```

## 2. 模型和凭据

### 凭据优先级

当多个来源同时存在时，优先级为：

1. `--api-key`
2. `~/.rpi/auth.json`（由 `rpi auth login` 写入）
3. `~/.rpi/agent/models.json` 中对应 provider 的 `apiKey`
4. 环境变量

Anthropic 使用 `ANTHROPIC_API_KEY` 或 `ANTHROPIC_AUTH_TOKEN`；OpenAI-compatible provider 使用 `OPENAI_API_KEY`。检查凭据时不会发起网络请求：

```bash
rpi auth check
rpi auth check --provider gateway --json
rpi auth login
rpi auth logout
```

不要把密钥写入 Git 仓库、项目文档或 package manifest。

### 自定义 Provider

配置文件默认是 `~/.rpi/agent/models.json`，也可以通过 `RPI_CODING_AGENT_DIR` 指定 agent 配置目录：

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "models": [
        { "id": "custom-claude", "name": "Custom Claude" }
      ]
    },
    "openai-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://api.example.com/v1",
      "apiKey": "sk-openai-secret",
      "models": [
        { "id": "custom-model", "name": "Custom Model" }
      ]
    }
  }
}
```

选择模型：

```bash
rpi --model gateway/custom-claude -p "hello"
rpi --model openai-gateway/custom-model -p "hello"
rpi --provider gateway --model custom-claude -p "hello"
```

不传 `--model` 时，rpi 会优先选择已经认证的模型。标准 Anthropic 凭据存在时使用内置默认模型；只有自定义 gateway 已认证时，会选择该 gateway 的第一个可用模型。

Anthropic 的兼容网关还可以使用：

```text
ANTHROPIC_BASE_URL=https://gateway.example.com
ANTHROPIC_AUTH_TOKEN=token
```

## 3. CLI 命令

常用全局选项：

```text
--provider <name>       Provider 名称
--model <pattern>       模型 ID，支持 provider/model 和 thinking 后缀
--api-key <key>         本次运行覆盖凭据
--base-url <url>        覆盖模型 endpoint
--timeout <seconds>     LLM API 请求超时（默认 600）
--thinking <level>      off/minimal/low/medium/high/xhigh/max
--print, -p             单次运行并退出
--mode text|json|rpc    输出模式
--continue, -c          继续最近会话
--resume, -r            选择历史会话
--session <id|path>     指定会话
--session-dir <dir>     指定会话目录
--name, -n <name>       设置会话显示名称
--tools <list>          只允许指定工具
--exclude-tools <list>  禁用指定工具
--no-tools              禁用所有工具
--no-builtin-tools      禁用内置工具（read/bash/edit/write/docs）
--no-skills             跳过技能发现
--no-prompt-templates   跳过 prompt-template 发现
--no-context-files      跳过 AGENTS.md/CLAUDE.md 发现
--no-extensions         禁用扩展加载
--extensions-dir <dir>  额外扫描 Rust 扩展目录
--package-only         只加载当前项目 package/resource，排除全局资源
--project-only         `--package-only` 的等价别名
--list-models [search]  列出可用模型（可带模糊搜索）
--offline               禁用启动时的网络检查
--export <file>         把 JSONL 会话导出为 HTML
--tui-mode <mode>       TUI buffer：regular 或 fullscreen
--system-prompt <text>  替换默认系统提示词
--append-system-prompt <text>  追加系统提示词（可重复）
--debug-system-prompt   把解析后的系统提示词各段打印到 stderr
--verbose               显示启动警告（如被忽略的 flag）
--server [--port <n>] [--bind <ip>]
                        无头服务端（启动时打印 token）
--connect <host:port>   远程客户端 TUI（零本地资源）
--token <token>         远程认证 token（也支持环境变量 RPI_SERVER_TOKEN）
```

子命令：

```text
rpi auth login|check|logout
rpi events path|tail    检查扩展事件日志（需 RPI_EVENT_LOG=1）
rpi install <crate>
rpi uninstall <crate>
rpi update                 # 更新 rpi CLI 自身
rpi dev [options]          # 开发 Rust 扩展：编译、watch、热重载
rpi dev-local [options]    # 只调试当前 Rust 扩展（隔离模式）
```

### TUI 历史消息滚动

交互式 TUI 默认使用 alternate screen，并由应用自己管理 transcript 滚动。可以使用 `PageUp` / `PageDown`、`Home` / `End`，或鼠标和触控板滚轮浏览历史消息。

macOS 的行为与 main-screen 的终端原生 scrollback 不同：alternate-screen TUI 必须接收鼠标追踪事件，滚轮和触控板才能滚动历史消息。进入 raw mode 后，TUI 会在 alternate-screen 模式显式重新启用鼠标追踪；main-screen 模式则继续关闭鼠标追踪，以保留终端原生滚动和文本选择。

如果 macOS 上滚轮无法滚动历史消息，应检查 `TuiAltScreen::start_readerless` 是否包含 alternate-screen 的鼠标追踪初始化，不要直接修改 macOS 的全局默认值。

### macOS 键盘与插件面板

支持 Kitty 键盘协议的终端会启用修饰键、重复和松键事件，供编辑器和插件快捷键使用。协议在切换到当前屏幕后开启，并在退出或挂起前恢复；恢复运行时也会重新启用全屏鼠标滚动。

本地 Terminal.app 的 `Shift+Enter` 使用 macOS 原生 Shift 状态补足换行识别，普通 `Enter` 继续发送。SSH 会话不会读取本机 Shift 状态。未支持增强键盘协议的终端仍使用传统输入，按住快捷键等依赖松键事件的功能需要终端支持。

插件输入框、编辑器、选择器和历史搜索打开时优先接收输入，插件全局快捷键不会抢走这些窗口的按键。已被插件接管的按键仍会收到对应的松键事件，避免按住状态卡住。浮层按实际宽度排版，并避开输入框和页脚；多个被动面板在空间足够时自动纵向避让，窗口过小时暂时隐藏放不下的面板。需要避免浮层遮住聊天正文时可关闭面板，或使用插件的 sidebar 布局。

### 远程模式（`--server` / `--connect`）

把 agent 跑在无头服务端，另一个进程用 TUI 远程驱动。客户端**无任何本地 agent 资源**。

```bash
rpi --server --port 9899          # 服务端：启动时打印 token
rpi --connect 127.0.0.1:9899 --token <token>   # 客户端

# 或通过环境变量传递 token
export RPI_SERVER_TOKEN=<token>
rpi --connect 127.0.0.1:9899
```

服务端默认开启 token 认证（`--no-token` 可关闭）。客户端支持 `/state`、`/model`、
`/thinking`、`/tools`、`/abort` 等命令；`/tree`、`/fork`、`/switch`、`/export`、
`/name`、`/reload` 依赖本地 harness，在远程模式下不可用。完整协议、可用命令与
限制见 [`remote-mode.md`](remote-mode.md)。

## 4. 内置工具

默认工具包含 Pi 的 `read`、`bash`、`edit`、`write`，以及 rpi 自带的只读 `docs` 文档查询工具。Windows 上还会默认注册 `powershell`。`docs` 可以查询使用手册、扩展开发、Agent 项目结构、Rust 调试和架构说明；`grep`、`find`、`ls` 仍保留为库实现，但不由 CLI 默认注册。

可以通过 `.rpi/settings.json` 的 `defaultTools` 字段（或简写 `default_tools`）在未指定
`--tools` 时限定启动工具集：

```json
{
  "defaultTools": ["read", "bash", "edit", "write", "docs"]
}
```

需要限制工具范围时，显式列出 Pi 的四个工具：

```bash
rpi --tools read,bash,edit,write,docs -p "检查并修改项目文件"
```

## 5. 项目目录和资源优先级

rpi 会优先使用 rpi 自己的目录，同时兼容原 Pi 的 `.pi` 布局：

```text
项目/
├── .rpi/
│   ├── settings.json
│   ├── SYSTEM.md
│   ├── APPEND_SYSTEM.md
│   ├── skills/
│   ├── prompts/
│   ├── themes/
│   ├── extensions/
│   └── packages/
└── themes/
```

项目 `.rpi` 优先于项目 `.pi`。全局资源默认位于 `~/.rpi/agent/`，包括 `settings.json`、`models.json`、`skills/`、`prompts/`、`themes/`、`extensions/` 和 `packages/`。同名资源发生冲突时，项目资源优先于全局资源，`.rpi` 优先于 `.pi`。

项目级 `.rpi/settings.json` 可以追加资源目录：

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"],
  "packages": ["./packages/review-tools"]
}
```

路径相对于项目根目录；`skills`、`prompts`、`extensions` 是对应 `*Dirs` 字段的简写。自定义目录会与 `.rpi`、`.pi` 和全局约定目录一起加载，`.rpi` 优先。全局 `~/.rpi/agent/settings.json` 也支持这些字段，相对路径相对于 agent 目录。

只加载当前项目的 package/resource、排除全局资源时，使用以下任一等价参数：

```bash
rpi --package-only
rpi --project-only
```

两者都会加载当前项目的 skills、prompts、themes、extensions 和 packages，并读取项目 `.rpi/settings.json`；不会自动加载全局资源。`--local-only` 语义不同，只加载通过 `--extensions-dir` 或 `--extension` 显式指定的扩展。

配置目录可以重定位：

```bash
RPI_CODING_AGENT_DIR=/work/rpi-agent rpi
```

会话默认保存到 agent 配置目录下的 sessions；`--no-session` 可使用临时会话。

## 6. Rust 插件

Rust 原生插件是 `cdylib`，只依赖 `rpi-plugin-sdk`，由 `rpi-extensions` 动态加载：

```bash
cargo build -p plugin-stub
rpi --extensions-dir target/debug -p 'echo "hi"'
```

从 crates.io 安装：

```bash
rpi install rpi-extension-example
rpi install rpi-extension-example --version 0.1.0
rpi install rpi-extension-example --force
```

本地开发：

```bash
rpi install my-extension --path ../my-rpi-extension --force
```

开发扩展项目时使用 watch 模式：

```bash
cd ../my-rpi-extension
rpi dev
# 多扩展 workspace：rpi dev --package my-extension
# 只调试当前扩展：rpi dev-local（隔离模式，不加载全局扩展）
```

`rpi dev` 会自动识别 Cargo `cdylib`、首次编译并从 `.rpi/extensions/.dev` 加载版本化产物。源码变化会触发重新编译和热重载；手工执行 `/reload` 也会先重新编译。编译失败时继续保留当前已经加载的版本。完整模板和边界规则见在线扩展作者指南：<https://rpi.laofu.online/extension-authoring.md>，调试技巧见 `docs` 的 `debugging` 主题。

安装后的动态库位于 `~/.rpi/agent/extensions`（或 `RPI_CODING_AGENT_DIR` 指定的目录），下次启动 rpi 时加载。插件通过稳定 ABI 注册工具、Provider、事件处理器和资源处理器；不要直接依赖 `rpi-cli` 的私有模块。

## 7. SDK 集成

只需要嵌入 Agent 时，依赖 `rpi-agent` 和 `rpi-ai`；需要内置工具时再加入 `rpi-tools`；需要会话、压缩、skills 和上下文文件时加入 `rpi-harness`。

```rust
use rpi_agent::{Agent, AgentEvent};
use rpi_ai::providers::faux::faux_provider;

let model = faux_provider().model("echo");
let mut agent = Agent::builder(model)
    .system_prompt("You are a helpful assistant.")
    .build();

let mut events = agent.subscribe();
agent.prompt("Hello from rpi").await?;
while let Some(event) = events.recv().await {
    if matches!(event, AgentEvent::AgentEnd { .. }) {
        break;
    }
}
```

自定义工具实现 `rpi-agent::AgentTool`；Provider 实现 `rpi-ai::Provider`。使用 `faux provider` 和 `InMemoryExecutionEnv` 可以在没有网络、API key 或真实文件系统的情况下测试 Agent loop。

## 8. 故障排查

### 找不到模型或接口报错

运行 `rpi auth check`，确认 provider、`models.json`、环境变量和 `baseUrl`。使用 `--provider` 和完整的 `provider/model` 避免同名模型歧义。接口错误会保留 provider 返回的诊断文本；先检查 endpoint、认证头和模型 ID。

### 扩展加载失败

确认 Rust 插件是 `cdylib` 并与当前平台匹配；临时使用 `--extensions-dir` 指向生成目录。加载失败时保留宿主输出的完整错误信息，检查扩展是否声明了宿主未提供的能力。

### 扩展事件处理器没触发或启动被中止

```bash
export RPI_EVENT_LOG=1
rpi            # 记录所有扩展事件处理器调用
rpi events tail   # 跟踪日志；看 result 是 Continue 还是 Error/Abort/Panic/Timeout
```

日志默认在 `~/.rpi/logs/events.jsonl`（可用 `RPI_EVENT_LOG_PATH` 覆盖）。如果
`BeforeTuiStart` 阶段有扩展返回 veto（`EVENT_HANDLER_ABORT`），CLI 以退出码 `3`
中止启动并打印原因。

### 资源没有出现在欢迎页

确认文件位于 `.rpi`（或兼容的 `.pi`）或全局 `~/.rpi/agent`。使用 `rpi --debug-system-prompt` 查看最终系统提示词、skills 和资源计数；同名资源优先检查 `.rpi` 是否覆盖了 `.pi`。

### 远程模式连不上

确认服务端已用 `rpi --server --port <port>` 启动且 token 正确（或通过
`RPI_SERVER_TOKEN` 提供）。客户端是纯显示端，不加载任何本地 provider/工具/扩展；
会话文件在服务端。

## 9. 开发、测试和发布

运行格式化、检查和测试：

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
```

发布 crate 时按依赖顺序：

```text
rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness →
rpi-plugin-sdk → rpi-extensions → rpi-tui → rpi-cli
```

发布前更新版本号和 `docs/release-vX.Y.Z.md`，确认 README、官网 `website/data/docs.json` 和本手册中的命令一致。官网是静态站点，文档文件提交到仓库后仍需按项目部署流程重新部署。

## 10. 文档查询入口

- 在线文档：<https://rpi.laofu.online/docs.html>
- 源码仓库：<https://github.com/bigfish1913/pi-rust>
- Rust API：<https://docs.rs/rpi-agent>、<https://docs.rs/rpi-plugin-sdk>

- Package 与扩展作者指南：<https://rpi.laofu.online/extension-authoring.md>
- 远程模式：<docs/remote/user-guide.md>
- Rust 扩展与 agent 调试：`rpi` 内 `docs` 工具的 `debugging` 主题

当在线文档和已安装版本不一致时，以对应版本的 Git tag 和仓库内文档为准。
