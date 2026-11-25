# 可嵌入运行时与选择器拆分

状态：无界面运行时已在 `feature-0.9.0` 抽取；终端业务选择器留待下一阶段。
本文承接原《可嵌入运行时与选择器重构规划》，对应 #67（可复用模型配置）与
#68（可复用选择器）。新增 crate 尚未发布，版本沿用 workspace，由发布流程统一更新。

## 目标与边界

嵌入式应用可以不依赖 `rpi-cli`，也不复制目录、凭据优先级与模型解析规则，
使用自己存储中的配置与凭据，构建可供 Agent/Harness 使用的 provider。

| 层 | 职责 |
| --- | --- |
| `rpi-ai` | 通用 Model、Provider、ThinkingLevel、协议实现与流式响应 |
| `rpi-runtime` | 模型目录、凭据解析、本地认证状态、模型/thinking 解析、私有请求绑定和可选刷新通知 |
| `rpi-extensions` | 动态库加载、原有 ABI、provider 声明与实现、取消和插件生命周期 |
| `rpi-tui` | 通用终端组件；不依赖 runtime、模型目录或凭据业务 |
| `rpi-cli` | 参数、默认文件路径、读写存储、项目信任、OAuth 交互、会话与终端挂载、RPC 传输 |

`rpi-runtime` 只依赖 `rpi-ai` 和通用库；不依赖 CLI、TUI、Harness 或扩展加载器。
供应商专属实现仍归原 provider package，runtime 不建立第二套插件加载机制。

## 公共 API

- `RuntimeConfig`：宿主提供 `ModelsConfig`、`AuthStore` 和模型默认值。
  宿主可以从文件、数据库或内存读取，库没有固定的 `~/.rpi` 路径。
- `ValueSource`：宿主提供环境值、时间和可选的凭据命令执行。
  `BTreeMap<String, String>` 是内存实现，不读取进程环境且默认禁用命令。
  CLI 的适配器保留原环境变量、模板、命令缓存与未缓存请求头行为。
  内置 HTTP provider 的代理也通过该环境来源解析，按每个请求的 URL 应用
  `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY`；代理配置无效时明确报错。
- `ProviderRegistry`：登记已加载的 provider 实现与声明。现有扩展会话由 CLI
  适配进来；实现的 `Arc` 保持原加载器持有期，不复制 ABI 或生命周期。
- `ModelRequest`：显式 provider、API、模型、thinking、key 和 endpoint 覆盖。
  API 可以区分同一 provider 下同名但不同协议的模型。
- `RuntimeConfig::resolve`：产生 `ResolvedModel`，其 provider 可交给 Agent/Harness。
  `selection()` 返回完整身份、请求的 thinking 和按模型能力限制后的等级。
  非推理模型的有效等级为 Off；宿主将有效等级传入请求选项，由 provider 转换为协议参数。
- `RuntimeConfig::catalog`：无需认证即可读取目录和兼容诊断。
- `RuntimeConfig::auth_status`：缺失、就绪、过期、明确匿名四种本地状态。
  就绪仅表示本地存在可用凭据来源，不表示服务端已经验证成功。
  查询只解析目标 provider；已有有效凭据时不执行认证头命令，也不展开其他 provider 的头。
- `Runtime`：可选的活动配置发布边界。`reload` 先构建新绑定和快照，成功后再替换，
  失败保留原状态。`subscribe()` 使用 watch 通知最新目录与认证快照。
  快照直接复用本次解析结果，不重新读取凭据或执行认证头命令。
  显式 key 覆盖只让选中的 provider 就绪，来源为 `CredentialSource::Explicit`
  （`as_json_str()` 返回 `override`），不发布 key。

配置和凭据 DTO 属于宿主私有输入，不应发送给 UI。公开模型描述、选择结果和
`RuntimeSnapshot` 不含密钥和私有请求头；真实请求由不可替换的内部绑定恢复
endpoint、模型与认证。宿主替换配置后，已取得的旧 provider 仍保持原绑定。
公开 endpoint 仅保留 URL 的协议、host 与 path，去除 userinfo、query、fragment；
真实请求仍由私有绑定恢复完整 URL。环境、时间和可缓存凭据命令在一次解析内保持一致，
未缓存的认证头命令继续按原有模型展开语义执行。

## CLI 迁移

`config.rs` 保留路径、存储、原子写入与信任决定，重新导出 runtime 的数据类型。
`provider.rs` 只负责读取有效配置和适配扩展会话，调用 runtime 执行解析。
CLI、TUI、RPC 的会话构建继续走同一入口；认证命令、RPC 认证状态与 provider
选择列表使用共享的凭据规则和状态计算。

已有配置路径、可信项目覆盖顺序、CLI 参数优先级与跨 provider 凭据隔离保持兼容。
OAuth 登录、刷新、撤销的交互和落盘仍由原宿主负责，runtime 消费其结果。
旧 `model_registry` 保留为兼容接口，它的配置转换也调用共享实现；新宿主应使用
`RuntimeConfig` / `Runtime`。

## 独立嵌入示例

```text
cargo run -p rpi-runtime --example embedded --locked
```

示例仅依赖 runtime 与 AI 类型，使用内存配置、宿主提供的 faux provider 和空环境，
完成目录/认证快照、模型选择和一次流式请求；不读取用户配置，不需要密钥或网络。
替换为真实服务时，由宿主读取配置与凭据、适配 provider，再把返回的 provider
交给自己的 Agent 或 Harness。

## 验证与后续工作

回归测试迁移到 runtime，覆盖凭据优先级、provider 隔离、header 隐藏、同名模型、
协议路由和默认模型顺序。独立嵌入测试覆盖多个宿主环境、失败刷新、旧绑定存活、
安全通知、过期/匿名状态和显式 API 选择。项目默认值与真实扩展集成仍在 CLI 验证。
review 回归覆盖混合 endpoint 的双向代理路由与代理认证头、无效代理拒绝、认证查询的
命令隔离、显式覆盖快照、凭据/头轮换、失败刷新、非推理模型和 URL 凭据脱敏。

本地验证命令：

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
cargo clippy -p rpi-runtime --all-targets --no-deps --locked -- -D warnings
cargo run -p rpi-runtime --example embedded --locked
```

真实 provider 集成需显式设置 `RPI_TEST_PROVIDER_LIBRARY`（已构建的
`rpi-providers` 动态库）和 `RPI_TEST_PROVIDER_PACKAGE`（源码包目录），再运行
`cargo test -p rpi-cli --test provider_packages --locked -- --nocapture`。
本次在 Windows 加载相邻 rpi-extension 的真实 DLL，7 项集成测试全部执行并通过，
包含流式请求、provider 隔离、匿名策略、重载和项目包发现。

第二阶段开放 provider/model/thinking 终端选择器。建议通过可选 `tui` feature
提供 `rpi-runtime::ui`，默认运行时仍不依赖终端。公共组件只返回类型化选择结果；
保存默认值、执行 OAuth、切换会话和挂载位置仍由宿主决定。

本次不重写 Agent Loop、Harness、远程传输或插件 ABI，不接管宿主的分支、
计划和历史业务语义。桌面/Web/手机端可复用无界面 runtime，自行提供界面。
