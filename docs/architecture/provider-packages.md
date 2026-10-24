# Provider packages

供应商策略统一放在独立 rpi-package 项目的
[`rpi-providers`](https://github.com/pi-rust/rpi-package/tree/feature-1014/packages/rpi-providers)。
一个 Cargo package、一个动态库、一个安装 / 升级入口，内部按供应商分模块。
新增接入遵循 [Provider 接入规范](../extensions/provider-integration.md)。

核心保留 Anthropic Messages、OpenAI Chat Completions / Responses，以及离线测试用的 Faux。
CLI 不依赖供应商实现。通用 ABI / 异步协议桥接由核心仓库的
`rpi-plugin-sdk::provider` 提供，只有显式开启 SDK 的 `provider` feature 才会引入
AI、Tokio 与 HTTP 依赖；默认 ABI SDK 保持轻量。桥接层不内置供应商，也不解析宿主凭据。

## 统一包中的 provider

| provider ID | 模块 | 默认认证 |
| --- | --- | --- |
| `deepseek` | `src/deepseek.rs`：目录、思考开关、历史思考与工具续轮 | `DEEPSEEK_API_KEY`，必须认证 |
| `openrouter` | `src/openrouter.rs`：目录、HTTP-Referer / X-Title | `OPENROUTER_API_KEY`，必须认证 |
| `llama-cpp` | `src/llama_cpp/`：本地推理、模型查询及服务管理 | 可匿名，可选 `LLAMA_CPP_API_KEY` |
| `proxy` | `src/proxy/`：通用 OpenAI-compatible 代理 | 可匿名，可选 `PROXY_API_KEY` |

旧的各供应商 crate 已从本仓库 workspace 移除，DeepSeek 专用核心实现也已移除。
维护任意供应商只修改统一包的对应模块；增加模块后由 `src/lib.rs` 的唯一插件入口注册。
`src/registration.rs` 复用通用桥接库，不让每个模块分别导出插件入口。

## 安装、使用与迁移

在 rpi-package 仓库根目录，使用本分支构建的宿主安装一次：

```sh
rpi package link providers --path packages/rpi-providers --debug
rpi --list-models
rpi auth login --provider deepseek
rpi --provider deepseek --model deepseek-flash
# 配置 OPENROUTER_API_KEY 后
rpi --provider openrouter --model openrouter/auto
# 本地服务已运行时
rpi --provider llama-cpp --model local --base-url http://localhost:8080/v1
```

`local` 与 Proxy 的 `default` 是目录预设；服务器使用其他模型名、上下文或输出上限时，
在 models.json 中覆盖对应 provider。用户端点与请求头同样覆盖包的默认值。
匿名目录就绪不代表服务已运行，使用时指定需要的 provider。

若已分别安装旧包，先 `rpi package unlink <旧安装名>`，再 link `providers`，避免旧实现
按加载顺序占用同名 ID。原有 provider ID、models.json 与 auth.json 保持兼容，不合并供应商凭据。
之后升级只更新统一包，卸载使用 `rpi package unlink providers`；会一起移除四份目录和实现。
本包尚未发布 crates.io，固定依赖兼容的核心提交，当前使用源码 link。

Rust 调用方改用 `rpi_providers::{deepseek,openrouter,llama_cpp,proxy}` 中的类型。
llama.cpp 管理程序仍在包内，入口改为：

```sh
cargo run -p rpi-providers --bin rpi-llama -- status
```

已有 models.json 可以继续使用通用协议；但只配置通用协议不会自动提供统一包中的
供应商思考 / 工具策略或专属环境变量声明。

## 注册与加载

插件使用现有 `declare` 槽位声明模型目录，无需增加 ABI 字段：

```rust,ignore
serde_json::json!({
    "providers": [{
        "id": "openrouter",
        "models": [model], // 完整 rpi_ai::Model
        "apiKeyEnv": "OPENROUTER_API_KEY",
        "requiresAuth": true
    }]
})
```

每个模型的 `provider` 必须等于声明的 `id`，声明必须由同一个插件的实际 provider 注册支持。
同名 provider 按加载顺序保留第一个，其目录也随该实现一起保留。声明可以早于或晚于 provider 注册。
没有目录声明的旧插件仍可配合 `models.json` 工作。

## Factory 与执行绑定

内置协议和原生扩展实现同一个宿主侧 `ProviderFactory` 契约。注册表优先查找 provider ID 与 API 同时匹配的扩展 factory，
再查找 API 协议对应的内置 factory；两者均由同一个 `ConfiguredProvider` 包装。
扩展只处理其注册的 API；同名 provider 的其他已配置协议仍使用对应的内置 factory，
例如安装 DeepSeek Completions 扩展后，原有 Responses 配置保持走 Responses。
同一 provider ID 的多个协议绑定合并为一个公开运行时，按 provider / API / model ID
选择内部绑定，避免 harness 按 provider ID 查找时误用另一个协议。
factory 负责提供协议实现，不读取环境变量、不决定凭据优先级；认证统一由宿主解析并绑定。

解析阶段将每个模型分成两种视图：

| 视图 | 内容与用途 |
| --- | --- |
| 公开描述 | 仍使用现有 `Model` 类型以兼容 CLI / harness / 插件 ABI，但不携带任何请求头。用于目录、当前模型和模型切换 |
| 私有 `ExecutionBinding` | 保存该实例的真实端点、模型级请求头与默认认证，只在执行请求时恢复 |

`Provider::models()`、`catalog_all()`、`catalog_with_manifests()`、认证后的目录以及 `ResolvedModel.model` 均返回公开描述。
`base_url` 仍在描述中可见以兼容现有配置和展示；实际执行以私有绑定为准。
调用者传入的模型必须匹配已注册的 provider / API / model ID，不能通过改写描述中的端点或请求头重定向已绑定凭据。
每次请求的 `SimpleStreamOptions` 仍可以按既有规则提供显式 API key 或额外请求头。

涉及 provider package 的 `/reload` 会重新解析配置并构建新的执行绑定，随后替换 provider 实例和公开目录；
凭据不再通过当前模型的 headers 传递。仅内置 provider 的会话保留既有的资源重载行为。
当前 package 被移除时，重载会在切换前失败并保留旧会话，避免把同名配置静默转交给另一个协议实现。
完整重载仍包含多个对象的更新，单一代次快照的原子切换属于后续生命周期改进。

`rpi_ai::providers::deepseek::DeepSeekProvider` 已移除。Rust 调用方改用独立
`rpi_providers::deepseek::DeepSeekProvider::new`；CLI 用户安装 package 后仍使用 `deepseek` ID，
已有同 ID 的 auth.json / models.json 配置可以复用。package 顺序提供默认模型，核心不再保留 DeepSeek 默认项。
通用 Chat Completions 的 `requiresReasoningContent` 兼容选项支持独立回传历史思考；DeepSeek package
启用该能力并处理思考开关与 effort 映射，核心协议不判断 DeepSeek 身份。

推理模型的思考 token 计费方式由网关决定，这里也是“消息突然中断”的常见来源之一。
部分兼容网关（DeepSeek 的 reasoning 模型即是典型）把隐藏思考计入 `maxTokens` 描述的同一个补全预算：
只发送答案额度时，长思考会把额度全部消耗，流以 `finish_reason: length` 结束而正文为空或截断。
声明 `reasoningCountsTowardMaxTokens: true` 的模型会在开启思考时额外预留当前思考级别的预算
（与 Anthropic 的 `adjustMaxTokensForThinking` 形态一致）；未声明则保持原有单一预算行为。
`maxTokens` 仍表示可见答案的额度，不是模型的服务端上限——声明该选项时两者需要分别核对。
调用方显式传入的 `maxTokens` 按原值发送，不做叠加，默认由传入方自己算好思考余量。

需要注意生效位置：插件 provider 在插件自己编进去的 `rpi-ai` 里执行请求构建，
而不是宿主核心。因此这条选项对插件 provider 生效的前提，是插件把 `rpi-ai` 依赖升到包含它的版本；
仅升级宿主 CLI 不会改变插件发出的 `maxTokens`。

启动顺序是：加载扩展一次 → 收集 package 目录和 runtime factories → 合并用户模型配置 →
按 provider id 解析凭据 → 选择模型并绑定 runtime → 创建 harness。
`--list-models` 可在无凭据时显示 package 模型；`--no-extensions` 禁用这些声明和实现。
TUI 的 `/provider` 同样显示已加载的 package、模型数和认证状态，未配置 key 的 package 也会显示，但不能切换到未在当前会话加载的模型。
`/provider <id>` 与菜单选择即时更新当前 lane 和底部模型显示，成功后保存默认 provider / model。
缺少可用认证、未知 ID 或 lane 切换失败时，保留原模型与默认值，并显示错误。
`/model` 选择具体模型；正在生成时的 provider 切换按输入顺序在当前回复结束后处理。
`/provider remove <id>` 只移除 `models.json` 配置，不卸载 package；卸载使用 `rpi package unlink <name>`。
`/model openrouter/auto` 和完整的 `openrouter/openrouter/auto` 均可定位该模型，跨 provider 重名时需完整限定。
扩展目录仍遵守项目信任、`--project-only` 和显式扩展路径规则。

凭据顺序保持 `--api-key`（仅选中的 provider）→ `auth.json` → `models.json` → provider 自己的环境变量。
匿名运行必须由 package 明确声明，OpenAI 默认认证检查不会因此放宽，其他 package 不会继承 `OPENAI_API_KEY`。

认证策略跟随 package，通用 `auth login/logout/check/refresh` 和 `auth.json` 存储由宿主管理。
`auth check`、`/provider` 和实际请求共用凭据解析：空值、未解析的环境变量引用和过期 OAuth token 不算可用凭据；
有效的 OAuth access token 按 Bearer 注入请求。`auth check` 不刷新 token，也不发起服务端认证验证。

```sh
rpi auth login --provider openrouter
rpi auth check --provider openrouter --json
rpi auth logout --provider openrouter
# 未安装时也可显式加载（扩展路径在 auth 子命令后）
rpi auth check --provider openrouter --extension /path/to/provider.dll --json
```

`auth check --json` 的 `source` 返回 `auth.json`、`oauth`、`models.json`、专属环境变量名、`headers`、`anonymous` 或 `none`，
不输出凭据内容；`--no-extensions` 禁用 package 认证声明。auth 和启动使用相同的扩展发现、项目信任与显式路径规则。
退出码为：就绪 `0`，缺少凭据 `1`，配置读取失败 `2`。API-key logout 只删除存储的 key，不清除环境变量或 models.json。

核心只保留 Anthropic、OpenAI 的环境变量回退和默认模型优先项。DeepSeek 等供应商不再隐式识别
环境变量：安装统一的 `rpi-providers`（其中分别声明 `apiKeyEnv`），或在 models.json 显式填写 `apiKey: "$VENDOR_API_KEY"`。
已有 auth.json 和显式 apiKey 配置无需迁移，package 默认模型由其目录提供。

涉及 provider package 的 `/reload` 重新绑定 fresh plugin runtime、私有执行配置和公开模型目录，同时更新 `/model` 选择器和插件的 `set_model`。
`/provider` 每次打开时读取重载后的 package 声明。
如果当前模型所属的 package 消失或无法重新解析，reload 返回错误并保留原会话，先切换模型再移除该 package。
取消立即结束宿主请求；插件桥接会取消自己的网络任务，DLL 引用保持到同步请求真正退出。

## 验证

可运行的 DeepSeek 流式实现和测试位于独立的 `rpi-providers` package，provider ID 为 `deepseek`。

```sh
cd /path/to/rpi-package
cargo build -p rpi-providers
cargo test -p rpi-providers --test streaming
cargo test -p rpi-providers --test host
```

在 `pi-rust` 中运行 CLI 集成测试前，将 `RPI_TEST_PROVIDER_LIBRARY` 指向构建出的
`rpi_providers.dll` / `librpi_providers.so` / `librpi_providers.dylib`。

端到端测试用真实 DLL、CLI 子进程和本机 SSE 服务，覆盖无凭据目录、禁用扩展、认证隔离、带认证的
OpenRouter、匿名 llama.cpp / Proxy，以及实际流式输出。其他协议可以沿同样的声明和 factory 边界接入。

生产统一包在 rpi-package 仓库根目录运行 `cargo build -p rpi-providers` 与
`cargo test -p rpi-providers`。包的单 DLL 测试覆盖四份目录与认证策略，以及并发请求的
factory / key 隔离；DeepSeek 思考工具续轮、取消与错误测试也在包内。

本仓库的跨项目 CLI / reload / link 集成测试使用同一份动态库，无需 CLI 依赖供应商 crate：

```powershell
$env:RPI_TEST_PROVIDER_LIBRARY = 'C:\path\to\target\debug\rpi_providers.dll'
$env:RPI_TEST_PROVIDER_PACKAGE = 'C:\path\to\rpi-package\packages\rpi-providers'
cargo test -p rpi-cli --test provider_packages
```

Linux / macOS 改用对应的 `librpi_providers.so` / `librpi_providers.dylib` 路径。
未设置路径时跳过外部包集成用例；显式提供无效路径会报错。跨项目验收应同时构建两个项目，
显式设置路径运行，不能把跳过当作已验证。
