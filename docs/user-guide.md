# rpi 使用手册

这是一份面向使用者和扩展作者的 rpi 手册。rpi 是基于 Pi agent 设计的 Rust 原生 coding-agent：核心能力以 crate 形式提供，`rpi` 是开箱即用的 CLI。本文记录当前版本已经实现的命令和行为，随 `rpi-cli` 一起发布。

## 1. 安装

### 直接安装 CLI

需要 Rust/Cargo 和 Node.js（只有使用 Pi JavaScript/TypeScript package 时才必须 Node.js）：

```bash
cargo install rpi-cli
rpi --version
rpi --help
```

从本仓库源码安装开发版本：

```bash
git clone https://github.com/bigfish1913/pi-rust.git
cd pi-rust
cargo install --path crates/pi-cli --force
```

`cargo install` 安装的是可执行文件；它不等同于 `rpi install`。后者用于安装由 `rpi-plugin-sdk` 构建的 Rust 动态库扩展。

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
--thinking <level>      off/minimal/low/medium/high/xhigh/max
--print, -p             单次运行并退出
--mode text|json|rpc    输出模式
--continue, -c          继续最近会话
--resume, -r            选择历史会话
--session <id|path>     指定会话
--session-dir <dir>     指定会话目录
--tools <list>          只允许指定工具
--exclude-tools <list>  禁用指定工具
--no-tools              禁用所有工具
--no-extensions         禁用扩展加载
--extensions-dir <dir>  额外扫描 Rust 扩展目录
```

子命令：

```text
rpi auth login|check|logout
rpi package list|add|remove|update
rpi install <crate>
rpi install-pi <spec>
rpi uninstall <crate>
rpi uninstall-pi <spec>
rpi update
```

## 4. 内置工具和文档查询

默认工具为 `docs`、`read`、`bash`、`edit`、`write`、`grep`、`find`、`ls`。其中 `docs` 是只读工具，文档在编译时嵌入二进制，安装后无需联网也能使用。

模型遇到 rpi 命令、扩展 API、Pi package 或 `.rpi` 配置不确定时，应先调用 `docs`：

```json
{}
```

列出主题；或者：

```json
{"topic":"guide"}
{"topic":"extensions","query":"ctx.ui.custom"}
{"topic":"guide","query":"install-pi"}
```

当前主题包括 `guide`、`authoring`、`overview`、`extensions`、`architecture`、`compatibility`。创建 package 或扩展前优先查询 `authoring`；`plugin`、`plugins`、`js`、`ts`、`pi` 等常用别名也可以使用。普通 `read` 仍然适合读取项目中的任意文件。

限制工具范围时请显式列出 `docs`：

```bash
rpi --tools docs,read -p "查一下 Pi package 的安装方式"
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
└── .pi/                 # 兼容旧 Pi 项目
```

项目 `.rpi` 优先于项目 `.pi`。全局资源默认位于 `~/.rpi/agent/`，包括 `settings.json`、`models.json`、`skills/`、`prompts/`、`themes/`、`extensions/` 和 `packages/`。同名资源发生冲突时，项目资源优先于全局资源，`.rpi` 优先于 `.pi`。

项目级 `.rpi/settings.json`（兼容 `.pi/settings.json`）可以追加资源目录和 package：

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"],
  "packages": ["./packages/review-tools"]
}
```

路径相对于项目根目录；`skills`、`prompts`、`extensions` 是对应 `*Dirs` 字段的简写。自定义目录会与 `.rpi`、`.pi` 和全局约定目录一起加载，`.rpi` 优先。全局 `~/.rpi/agent/settings.json` 也支持这些字段，相对路径相对于 agent 目录。

配置目录可以重定位：

```bash
RPI_CODING_AGENT_DIR=/work/rpi-agent rpi
```

会话默认保存到 agent 配置目录下的 sessions；`--no-session` 可使用临时会话。

## 6. Pi package

### 安装和管理

支持 npm、Git 和本地 package：

```bash
rpi install-pi npm:@narumitw/pi-btw
rpi install-pi npm:@scope/package@1.0.0
rpi install-pi git:github.com/user/repo@v1
rpi install-pi ./my-pi-package
rpi install-pi --global npm:@scope/package
```

卸载时，Rust 扩展使用：

```bash
rpi uninstall <crate>
```

Pi package 使用：

```bash
rpi uninstall-pi npm:@scope/package
rpi uninstall-pi --global npm:@scope/package
# 等价写法
rpi uninstall pi npm:@scope/package
```

卸载 Pi package 会同时移除启用配置。只有位于 `.rpi/packages`、`.pi/packages`
或全局 agent package store 的安装目录才会被删除；项目源码目录只会被禁用，不会删除。

本地 package 也可以只启用、不下载：

```bash
rpi package add ../my-pi-package
rpi package list
rpi package remove ../my-pi-package
rpi package update
```

项目 package 默认存放在 `.rpi/packages`，全局 package 存放在 `~/.rpi/agent/packages`。rpi 也兼容 Pi 原生 npm store：读取 Pi 写入的 `npm:` package spec 时，会搜索 `~/.pi/agent/npm/node_modules/<package>`（以及 rpi agent 下对应的 `npm/node_modules`）。安装过程执行 `npm install --omit=dev`；Node.js 是运行 JS/TS extension 的必要条件。

### 静态资源

package 可以提供以下资源：

```text
skills/
prompts/
themes/
SYSTEM.md
APPEND_SYSTEM.md
extensions/
```

`package.json` 中可使用 `rpi` 对象声明资源路径；为兼容 Pi，也支持 `pi` 对象，二者同时存在时 `rpi` 优先。资源会注入系统提示词、技能列表、prompt template 和主题选择器。

### JS/TS extension

Node host 支持 Pi 风格的 `registerTool`、`registerCommand` 和资源发现。扩展可以使用命令上下文中的 `mode`、`hasUI`、`capabilities`、`model`、`modelRegistry`、`ui.notify`、编辑器文本和 session metadata。

`modelRegistry.getProvider(id)` 在当前 Rust provider 可用时提供 Pi 兼容的 `streamSimple()`/`complete()`；`ctx.ui.custom` 提供终端输入、resize、overlay 和生命周期事件的组件代理。扩展应先检查 `ctx.capabilities`，不要假定每个宿主都启用了所有能力。

扩展和 rpi 运行在同一用户权限下，能够访问当前用户允许的文件和网络。安装不受信任的 package 前，先阅读其源码和 manifest。

## 7. Rust 插件

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
```

`rpi dev` 会自动识别 Cargo `cdylib`、首次编译并从 `.rpi/extensions/.dev` 加载版本化产物。源码变化会触发重新编译和热重载；手工执行 `/reload` 也会先重新编译。编译失败时继续保留当前已经加载的版本。完整模板和边界规则见在线扩展作者指南：<https://rpi.laofu.online/extension-authoring.md>。

安装后的动态库位于 `~/.rpi/agent/extensions`（或 `RPI_CODING_AGENT_DIR` 指定的目录），下次启动 rpi 时加载。插件通过稳定 ABI 注册工具、Provider、事件处理器和资源处理器；不要直接依赖 `rpi-cli` 的私有模块。

## 8. SDK 集成

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

## 9. 故障排查

### 找不到模型或接口报错

运行 `rpi auth check`，确认 provider、`models.json`、环境变量和 `baseUrl`。使用 `--provider` 和完整的 `provider/model` 避免同名模型歧义。接口错误会保留 provider 返回的诊断文本；先检查 endpoint、认证头和模型 ID。

### `/command` 或 package 命令不存在

运行 `rpi package list`，确认 package 已启用且安装目录存在。重新安装时使用 `rpi install-pi --force ...`。Node extension 需要 Node.js；可以先执行 `node --version`。

### 扩展加载失败

确认 Rust 插件是 `cdylib` 并与当前平台匹配；临时使用 `--extensions-dir` 指向生成目录。JS/TS 扩展失败时保留完整错误堆栈，检查扩展是否假设 Pi 独有的 UI 能力或未调用宿主要求的初始化流程。

### 资源没有出现在欢迎页

确认文件位于 `.rpi`（或兼容的 `.pi`）、全局 `~/.rpi/agent` 或已启用 package 的资源目录。使用 `rpi --debug-system-prompt` 查看最终系统提示词、skills 和资源计数；同名资源优先检查 `.rpi` 是否覆盖了 `.pi`。

## 10. 开发、测试和发布

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

发布前更新版本号和 `docs/release-vX.Y.Z.md`，确认 README、官网 `website/data/docs.json` 和本手册中的命令一致。官网是静态站点，文档文件提交到仓库后仍需按项目部署流程重新部署；CLI 内置文档则会随新二进制一起发布。

## 11. 文档查询入口

- 在线文档：<https://rpi.laofu.online/docs.html>
- 源码仓库：<https://github.com/bigfish1913/pi-rust>
- Rust API：<https://docs.rs/rpi-agent>、<https://docs.rs/rpi-plugin-sdk>
- Pi 参考实现：<https://github.com/earendil-works/pi>
- Package 与扩展作者指南：<https://rpi.laofu.online/extension-authoring.md>

当在线文档和已安装版本不一致时，以当前二进制中的 `docs` 工具和对应版本的 Git tag 为准。
