# Provider 接入规范

本文约定当前 provider package 的实现、认证与验收要求。架构背景见
[Provider packages](../architecture/provider-packages.md)，底层 ABI 见
[扩展开发](authoring.md)。生产统一包 `rpi-providers`（含 DeepSeek / OpenRouter / llama.cpp / Proxy）见
[独立 package 项目](https://github.com/pi-rust/rpi-package/tree/feature-1014/packages/rpi-providers)。正式 provider 的源码、流式测试和宿主集成测试均维护在该 package；本仓库不再保留 DeepSeek 示例实现。

## 1. 选择接入方式

| 需求 | 接入方式 |
| --- | --- |
| 已支持的 Anthropic Messages、OpenAI Chat Completions / Responses 协议，只改端点、模型和 key | 优先使用 `models.json`，无需新 package |
| 提供可安装的模型目录、供应商默认值、专属请求头、匿名策略或管理命令 | 独立 provider package，优先复用已有协议 |
| 新协议或现有协议无法表达的请求 / 响应差异 | package 实现 `rpi_ai::Provider`，经统一 ABI 接入 |
| 浏览器登录、device-code、token 刷新或撤销 | 另外注册 OAuth provider；推理与认证是两项独立能力 |

核心保留 OpenAI、Anthropic 与离线测试用的 Faux；DeepSeek 已迁到独立 `rpi-package` 项目。新增供应商适配默认放入
独立 package，不给 CLI 增加供应商分支或默认依赖。协议兼容不等于支持该供应商的所有
功能；README 必须列出已验证能力和限制。

## 2. 职责与依赖边界

| 层 | 负责 | 接入要求 |
| --- | --- | --- |
| 宿主 | 扩展发现、模型配置合并、凭据解析与存储、factory 选择、执行绑定、会话切换 | package 复用这些能力，不重新实现一套凭据优先级 |
| package | provider ID、默认目录、认证策略、协议适配、供应商命令 / OAuth 流程 | 不将供应商特判写回 CLI |
| `rpi-plugin-sdk::provider` | 同步 C ABI 与异步 Provider 桥接、流式事件、请求取消 | 开启 SDK 的 `provider` feature，使用普通 Rust factory 即可导出 package |
| `rpi-ai` | `Model`、`Provider`、流式事件和可复用协议 | 优先复用现有协议解析、错误、用量与重试实现 |
| `rpi-plugin-sdk` | 动态库入口、能力检测、跨 ABI 类型与字符串释放 | ABI 边界只传 SDK 类型及约定 JSON |

package 运行时依赖可使用 `rpi-ai` 与开启 `provider` feature 的 `rpi-plugin-sdk`，不得依赖
`rpi-cli` 或宿主私有 factory / binding。真实加载测试可把 `rpi-extensions` 放在
`dev-dependencies`。依赖版本按当前仓库模板保持兼容，不根据示例假定 crate 已发布。

宿主的私有 `ProviderFactory` 和 package 的 `fn(&Model, &SimpleStreamOptions)
-> Arc<dyn Provider>` 是不同接口。前者统一选择内置协议或扩展；后者接收一次请求的
执行配置，构造插件内部协议实例。扩展作者无需实现宿主私有 trait。

## 3. ID 与模型目录

必须区分 package 名、provider ID、wire model ID 和 API 协议：

| 概念 | DeepSeek 正式 package | 用途 |
| --- | --- | --- |
| Cargo package | `rpi-providers` | 构建与分发统一 provider 动态库 |
| provider ID | `deepseek` | 注册、配置、凭据隔离和 `--provider` |
| model ID | `deepseek-flash` | 服务端接收的模型名，不能随意加 provider 前缀 |
| API | `openai-completions` | 选择请求 / 响应协议，不用于推断认证环境变量 |

推荐用 `Model::new` 构造模型，再通过 `serde_json::json!` 序列化；`models` 的元素是
完整 `rpi_ai::Model`，不能照抄 `models.json` 中允许省略字段的简写对象。

目录声明通过现有 `declare` 提交：

```rust,ignore
serde_json::json!({
    "providers": [{
        "id": PROVIDER_ID,
        "models": models(),
        "apiKeyEnv": "VENDOR_API_KEY",
        "requiresAuth": true
    }]
})
```

- `id` 必须非空，并与实际注册的 provider ID 一致；每个模型的 `provider` 必须等于此 ID。
- `models` 必须提供，允许空目录但不能发现预置模型；模型 ID 必须非空。填写真实的
  API、端点、上下文与输出上限、输入类型、reasoning 和兼容选项，不夸大能力。
- `apiKeyEnv` 可省略，表示没有 package 专属环境变量回退；`requiresAuth` 默认 `true`。
  只有明确允许无凭据请求的服务才设为 `false`，它仍可接受用户配置的凭据。
- 声明不能代替实现注册，只有同一插件成功注册的 provider 才能提供对应目录。
  同名 provider 跨插件冲突时按加载顺序保留第一份实现及其目录，不拼接两家的声明。
- 目录声明不得包含 key、Bearer token 或已解析的私密请求头。注册与目录发现应能离线完成，
  不要求先登录；用户 `models.json` 可以覆盖 package 的默认端点、请求头与模型定义。
- 没有声明的旧插件仍可搭配 `models.json` 工作；新 package 应提供声明，保证安装后可发现。

`--list-models` 展示完整目录，不要求凭据；`--no-extensions` 同时禁用扩展实现和声明。
`/provider` 展示完整目录与认证状态，选择或 `/provider <id>` 会即时切换当前会话并保存默认值。
切换只使用会话中已认证的模型；失败时保留当前模型与原默认值。`/model` 用于选择具体模型。
`/provider remove <id>` 删除配置；卸载 package 使用 `rpi package unlink <name>`。

## 4. 推荐实现骨架

生产供应商统一维护在 `rpi-providers`，新增供应商优先增加包内模块，不新增独立 crate。
根 `src/lib.rs` 只导出一次 `export_plugin!`，遍历各模块的注册函数；每个模块仍有独立的
provider ID、目录、认证策略与 factory。安装 / 卸载 / 升级一次作用于整个包。

下面展示单 provider 教学扩展的写法。SDK 的 `provider` feature 默认关闭，需要显式开启：

```toml
[dependencies]
rpi-plugin-sdk = { path = "/path/to/pi-rust/crates/rpi-plugin-sdk", features = ["provider"] }
rpi-ai = { path = "/path/to/pi-rust/crates/rpi-ai", features = ["providers"] }
serde_json = "1"
```

路径替换为同一份兼容的核心源码。已发布的 0.6.2 及更早版本 SDK 不含该 feature；
包含迁移的版本发布前，使用上述源码依赖或固定到包含迁移的 Git 提交。

用于统一包时，调用包内
`crate::registration::register_provider!` 生成注册函数，并在根模块登记；不要在各模块
重复调用 `export_provider!`，否则会重复导出原生插件入口。

生产 package 的实现位于 [rpi-providers](https://github.com/pi-rust/rpi-package/tree/feature-1014/packages/rpi-providers)。新增 provider 应从该 package 的注册宏和测试结构开始，而不是复制本仓库已删除的 DeepSeek 示例。

```rust,ignore
use rpi_ai::{Api, Model, Provider, SimpleStreamOptions};
use std::sync::Arc;

const ID: &str = "vendor-example";
const BASE_URL: &str = "https://gateway.example/v1";

fn models() -> Vec<Model> {
    let mut model = Model::new(
        "wire-model-id", "Example Model", Api::OpenaiCompletions, ID, BASE_URL,
    );
    model.context_window = 32_768; // 用供应商实际支持值替换
    model.max_tokens = 4_096;
    vec![model]
}

fn create(
    context: &rpi_plugin_sdk::provider::ProviderContext,
    model: &Model,
    options: &SimpleStreamOptions,
) -> Result<Arc<dyn Provider>, String> {
    // 使用本次请求的端点与凭据，并传播 HTTP 客户端配置错误。
    context.completions(model, options, false)
}

rpi_plugin_sdk::export_provider_with_context!(
    ID, BASE_URL, "openai-completions",
    serde_json::json!({
        "id": ID, "models": models(),
        "apiKeyEnv": "VENDOR_API_KEY", "requiresAuth": true
    }),
    create
);
```

匿名服务必须同时声明 `requiresAuth: false`，并使用允许匿名的协议构造路径；对上面的
helper 即 `context.completions(model, options, true)`。仅修改 manifest 不会自动改变插件内部
客户端的认证要求。自定义协议替换 `create` 的实现，仍通过同一桥接宏导出。

## 5. 认证与私有执行配置

Provider 负责“怎样请求模型”，auth 负责“以谁的凭据请求”。新增推理 provider 不要求
同时实现 OAuth；API key package 声明认证策略后即可使用通用 `auth` 命令。

宿主按 provider ID 解析默认凭据：选中 provider 的 `--api-key` → `auth.json` 的有效 key
或未过期 OAuth token → `models.json` 的 `apiKey` → package 声明的环境变量 → 显式允许的匿名。
空白值、无法解析的引用与过期 token 不算可用凭据；显式认证请求头也参与就绪判断。
只有内置 Anthropic、OpenAI 保留自己的默认环境变量规则；DeepSeek 也通过 package 声明认证，新 provider 不会因为
使用 `openai-completions` 而继承 `OPENAI_API_KEY`，也不会根据 ID 自动推导环境变量名。

package 必须使用请求传入的 `model` 和 `options`，不得自行从环境变量或另一 provider 的
存储中回填 key，不得把目录默认端点硬编码成运行时端点。日志、普通工具结果、事件、错误
与目录输出中不得包含凭据；不要在端点 URL 中内嵌凭据。

宿主将执行配置分为两个视图：

- 公开模型描述供目录、当前模型、切换和配置后的 `Provider::models()` 使用，`headers`
  为 `None`；`base_url` 仍公开以兼容展示。不能靠这些描述保存认证状态。
- 私有 `ExecutionBinding` 保存实际端点、模型请求头及默认认证。执行前匹配 provider / API /
  model ID，并恢复权威配置；改写公开描述的端点或请求头不会改变已绑定请求。
- 每次请求显式传入的 `SimpleStreamOptions.api_key` / `headers` 仍按既有规则生效。
  插件收到的是执行请求，可能包含敏感配置，不能将它原样输出为诊断信息。

OAuth package 另外使用 `register_oauth_provider`，认证 ID 应与推理 provider ID 一致。
供应商的 `begin` / `exchange` / `refresh` / `revoke` 留在 package；标准交互、凭据存储与
命令留在宿主。完整 action / response 契约见 [OAuth 扩展接口](authoring.md#11-oauth-扩展接口统一标准实验阶段)。

```sh
rpi auth login --provider vendor-example
rpi auth check --provider vendor-example --json
rpi auth refresh --provider vendor-example
rpi auth logout --provider vendor-example
# 尚未安装的动态库：显式路径写在 auth 子命令后
rpi auth check --provider vendor-example --extension /path/to/provider.dll --json
```

`refresh` 需要对应 OAuth 实现；API key 不通过它刷新。`auth check` 只检查本地就绪状态，
不访问服务端、不刷新 token，不能证明 key 在服务端有效。JSON 的 `source` 可能是
`auth.json`、`oauth`、`models.json`、专属环境变量名、`headers`、`anonymous` 或 `none`；
退出码依次为就绪 `0`、缺少凭据 `1`、配置读取失败 `2`。API-key logout 只删除本地存储项，
不清除环境变量或配置；OAuth logout 通过匹配扩展撤销成功后再删除。

## 6. 流式、取消与 ABI 生命周期

推荐桥接宏统一处理这些规则；手写 ABI 时也必须遵守：

- 检测可选槽位，优先 `register_streaming_provider`，旧宿主缺少该能力时降级
  `register_provider`。使用 SDK 的结构体大小与能力检测，不直接假定尾部字段存在。
- 请求为 `{model, context, options}` JSON。增量在生成期间立即通过 `emit` 提交；不能先
  缓冲完整响应再切片伪造流式。事件粒度不保证等于 token 或 HTTP chunk。
- 支持文本、thinking、toolcall 的 start / delta / end 及 usage；`contentIndex` 和工具
  `id` / `name` / `arguments` 必须一致。工具参数可能跨多帧，不能逐帧当作完整 JSON。
- 同步 request fn 返回时，`out` 提供完整 `AssistantMessage`，成功、失败和取消都必须有
  对应终态；增量不是最终消息的替代品。宿主产生唯一的 `Done` 或 `Error`，插件桥接
  不再把协议内部的终态当普通 chunk 重复发送。
- 取消后宿主立即结束事件流，保留已收到的文本、模型标识和用量。插件应响应 `control`
  或 `emit` 的取消结果，停止网络与后台工作；阻塞读等待期间也要有可退出机制。
  `control == null` 表示该宿主没有取消控制面，不能解引用。
- 当前宿主 `completion` 恒为 `None`。request fn 返回后不得继续调用 `emit`，不得保存
  借用指针或回调上下文；字符串由分配方提供的释放回调回收，不跨 ABI 传 Rust 私有对象。
- 使用 SDK guard 防止 panic 穿越 C ABI。桥接库为每次请求创建 runtime 并在返回前结束；
  不留下可能在 DLL 卸载后运行的线程或任务。宿主保持 DLL 引用直到仍在执行的请求返回。

原生扩展拥有当前进程权限，私有绑定负责请求路由与凭据隔离，不提供恶意代码沙箱。
当前流式通道没有有界背压保证；不要将其描述成已经实现完整流量控制。

## 7. 重载与兼容

启动时先发现扩展和目录，再合并配置、解析认证并绑定协议实例。provider ID 与 API 同时匹配的扩展
factory 优先于内置 API factory，两者使用同一 `ConfiguredProvider` 和私有绑定路径。

涉及当前或原有 provider package 的 `/reload` 会重建运行时、执行绑定和公开目录；
只有内置 provider 的会话仍按现有资源重载逻辑执行，不承诺重新解析所有 provider 配置。
当前 package 消失或无法重新解析时，重载失败并保留旧会话；不能静默转交给另一个协议。
移除前先切换到其他模型。完整重载涉及多个对象，尚不保证全会话的原子代次切换。

迁移旧插件时，保留已有 provider ID 与 wire model ID，补齐目录和认证声明；将供应商
环境变量读取移交宿主，运行时改用请求参数。不要依赖公开模型的 `headers` 携带 token。

## 8. 提交前验收

| 验收项 | 必须提供的证据 |
| --- | --- |
| 发现与禁用 | 无凭据时可列目录；`--no-extensions` 不再出现扩展实现 / 声明 |
| 真实加载 | 构建动态库后通过宿主加载并请求；缺失产物不能算作测试通过 |
| 真正流式 | 本地 SSE 先发首段，等待宿主收到增量后才发送余下内容与 EOF |
| 编码与终态 | UTF-8 跨网络块、最终文本与 usage 正确、唯一 Done / Error |
| 中途取消 | 服务端保持响应未结束时取消，及时得到 Aborted，保留部分输出并停止工作 |
| 错误 | HTTP 401 与协议 / 网络错误可传达，不伪造成功终态 |
| 认证隔离 | 包专属 env、存储 key、显式配置、匿名策略符合声明，不借用 OpenAI 的 key |
| 执行绑定 | 覆盖端点与模型请求头确实生效；公开目录无认证头，篡改描述不能重定向默认凭据 |
| 命令与重载 | `auth check` / `/provider` 状态与请求一致；配置更新后重绑，移除当前 package 保留旧会话 |
| 声明的附加能力 | 支持工具则验证完整工具循环；支持 OAuth 则验证交互、刷新、撤销与过期处理 |

在仓库根目录运行正式 package 的离线验证：

```sh
cd /path/to/rpi-package
cargo build -p rpi-providers
cargo test -p rpi-providers --test streaming
cargo test -p rpi-providers --test host
```

在 `pi-rust` 仓库运行：

```sh
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
```

这些命令验证正式 `rpi-providers` package；新增 package 必须加入自身的动态库加载与协议测试，不能只依赖宿主测试。
真实 API 测试应默认 ignored，通过环境变量注入 key 后显式运行，并限制超时、重试与用量；具体命令见
`rpi-package/packages/rpi-providers/README.md`。
本仓库不再提供 DeepSeek 的重复教学实现；CLI 跨项目测试使用正式 `rpi_providers` 动态库。

统一包验证在 rpi-package 仓库运行 `cargo build -p rpi-providers`、`cargo test -p rpi-providers`。
本仓库 CLI 测试通过 `RPI_TEST_PROVIDER_LIBRARY` 指定该包的 DLL / so / dylib，
通过 `RPI_TEST_PROVIDER_PACKAGE` 指定 `packages/rpi-providers` 源码目录，覆盖 link / unlink。
不设置路径的跳过结果不能作为跨项目验收；具体命令见 [Provider packages](../architecture/provider-packages.md#验证)。
同一动态库必须注册全部预期 provider，并验证并发请求不会串用 factory 或 key。
