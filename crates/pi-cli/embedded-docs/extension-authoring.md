# rpi Rust 扩展开发最佳实践

本文是 rpi 创建 Rust `cdylib` 扩展的模型可读规范。扩展是 Rust 原生动态库，
通过稳定 C ABI（`rpi-plugin-sdk`）注册工具、事件处理器、资源发现、Provider
与渲染器，宿主（`rpi` CLI）负责加载与生命周期。创建完整 Agent 项目的默认结构见
`docs` 的 `agent` 主题；本指南聚焦扩展本身的 ABI 与生命周期。

## 1. 扩展模型总览

| 项目 | Rust 原生扩展 |
| --- | --- |
| 安装 | `rpi install crate-name`（crates.io 或 `--path`） |
| 入口 | `rpi_plugin_register` (统一 ABI) C ABI 符号，临时兼容 `rpi_plugin_register_v2` / `rpi_plugin_register_v3` |
| 开发反馈 | `rpi dev` watch + 热重载；`rpi dev-local` 隔离调试 |
| 静态资源 | `resources_discover` 或随项目放入 `.rpi` |
| 运行环境 | 本机动态库，当前用户权限 |
| 适用场景 | 本地工具、系统集成、性能敏感或纯 Rust 项目 |

扩展应遵循能力检测：只调用宿主明确提供的 capability，检查 `PluginApiVt`
对应注册槽位是否为 `Some`。缺少能力时返回清晰错误并优雅降级，不要 panic、
空指针或依赖 `undefined` 语义。

## 2. 推荐结构

```text
my-rpi-extension/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs
│   ├── tool.rs
│   └── error.rs
└── tests/
    └── behavior.rs
```

### Cargo.toml

```toml
[package]
name = "my-rpi-extension"
version = "0.1.0"
edition = "2021"
license = "MIT"
repository = "https://github.com/example/my-rpi-extension"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
rpi-plugin-sdk = "0.1"
serde_json = "1"
```

`cdylib` 是宿主加载的产物；同时保留 `rlib` 便于 Rust 集成测试。扩展边界只依赖
`rpi-plugin-sdk`，不要依赖 `rpi-cli`、`rpi-extensions` 或 harness 内部模块，
否则插件会和宿主实现耦合。

## 3. ABI 入口

宿主按 **v3 → v2 → v1** 协商。新插件导出 v3（需要声明优先级/平台时）或 v2；
只有极老的插件才用 v1。同时导出多个符号时宿主只调用最高版本，失败不回退。

### v2 入口（默认）

```rust
rpi_plugin_sdk::export_plugin_v2!(|api| {
    let Some(register_tool) = api.register_tool else {
        return -1;
    };
    // 构建 StableToolSchema 和 execute/poll/cancel/destroy 函数表，
    // 然后调用 register_tool。完整实现参考 examples/plugin-stub。
    let _ = register_tool;
    0
});
```

### v3 入口（声明优先级 / 平台）

```rust
rpi_plugin_sdk::export_plugin_v3!(|api, ext| {
    if let Some(declare) = ext.declare {
        declare(rpi_plugin_sdk::StbStringRef::from_str(
            r#"{"priority":60,"platforms":["linux"]}"#,
        ));
    }
    // 其余注册与 v2 相同
    0
});
```

`declare` 的 JSON 支持 `priority`（数字，越大越先）和 `platforms`（字符串数组，
如 `["linux","windows","macos"]`）。

### 能力检测

每个注册槽位都是 `Option`。注册前检查 vtable slot 是否为 `Some`，缺失时返回
清晰错误；不要依赖空指针或 panic 表达不支持。宿主对未知 runtime action ID
返回结构化错误，不会进入分发。旧版 ABI v1 的 runtime action 仅允许 ID `0..=15`（已废弃），
v2/v3 允许当前定义的 `0..=17`（含 `GetCliFlag`、`UiDialog`）。

## 4. 工具生命周期：execute → poll → cancel → destroy

工具导出四个 `extern "C"` 函数，宿主按固定协议驱动：

- `execute` 解析宿主传入的 JSON（owned `StbString`，用 `free_with` 释放），
  创建每次调用独立的 handle（`Box::into_raw`）。
- `poll` 必须非阻塞；未完成返回 `Pending`，完成返回 `Done` 或 `Err`。
  `partial_cb` 在 poll 内同步调用。
- `cancel` 只设置线程安全取消标记，必须幂等，不能释放 handle。
- `destroy` 由宿主最终调用一次并释放 handle（空指针 no-op）。

所有权协议：

- 宿主产生的 `StbString`（execute 的 params、事件 payload）由插件通过宿主的
  `free_string` 释放。
- 插件产生的 `StbString`（done 结果、partial 进度、resources_discover 的 out）
  由宿主通过插件注册时提供的 `plugin_free_string` 释放。
- 每个 `extern "C"` 边界都不能 unwind：**插件内部必须捕获 panic**，否则 Rust
  会以 `panic in a function that cannot unwind` **终止整个宿主进程**（不是只跳过
  该扩展）。用 SDK 提供的 `rpi_plugin_sdk::guard` / `guard_or` 包裹每个入口的
  body（execute/poll/cancel/destroy、事件处理器、resources_discover、provider/
  render、runtime action）。register 入口由 `export_plugin_v2!` / `export_plugin_v3!`
  自动包裹：panic 会被转换成 `REGISTER_PANIC_STATUS`，宿主据此跳过该插件并给出
  诊断，而不是崩溃。

  反例（会拖垮宿主）：

  ```rust,ignore
  extern "C" fn my_execute(..) -> StepHandle {
      do_work()          // 若这里 panic，整个 rpi 进程 abort
  }
  ```

  正确写法：

  ```rust,ignore
  extern "C" fn my_execute(..) -> StepHandle {
      rpi_plugin_sdk::guard_or(std::ptr::null_mut(), || do_work())
  }
  ```

完整、可运行的 ABI 模板位于 `examples/plugin-stub/src/lib.rs`。创建新工具时先
复制生命周期骨架，再替换参数 schema 和领域逻辑，不要重新设计所有权协议。

## 5. 事件处理器、资源发现与 Provider

### 事件处理器

通过 `api.register_event_handler` 按 `EventTag`（36 类）订阅：

- **会话生命周期**：`SessionStart`、`SessionBeforeSwitch`、`SessionBeforeFork`、
  `SessionBeforeCompact`、`SessionCompact`、`SessionShutdown` 等。
- **Agent 循环**：`BeforeAgentStart`、`AgentStart`、`AgentEnd`、`AgentSettled`、
  `TurnStart`、`TurnEnd`、`MessageStart`/`MessageUpdate`/`MessageEnd`、
  `ToolExecutionStart`/`Update`/`End`、`ToolCall`、`ToolResult`、`UserBash`、`Input`。
- **rpi 特有 / UI**：`BeforeTuiStart`（TUI 初始化前）、`UiPromptStart`/`UiPromptEnd`。

处理器收到 `StablePluginEvent`，不得释放事件里的字符串（宿主 fan-out 后统一释放）。
返回 `0` 成功；返回 `EVENT_HANDLER_ABORT` 表示 veto。

### 生命周期 veto

`BeforeTuiStart` 是带 veto 语义的 rpi 专有钩子：第一个返回 `EVENT_HANDLER_ABORT`
的处理器中止启动，CLI 以退出码 `3` 退出（用于"进入 TUI 前必须先建立外部连接"）。
`SessionStart`/`SessionShutdown` 的 veto 只是 advisory。分发带超时，慢处理器不会
无限阻塞主流程。

### 资源发现

`api.register_resources_discover` 注册 handler，宿主在 `startup`/`reload` 时
fan-out 到所有已注册 handler，合并返回的 `{skillPaths, promptPaths, themePaths}`。
单个 handler 报错不中止 fan-out。`out` 是插件拥有的 `StbString`，宿主通过插件
注册时给的 `plugin_free_string` 回收。

### Provider 与渲染器

`api.register_provider` 注入自定义 LLM Provider（`provider_id`/`base_url`/
`api_style` + `request_fn`）。`register_message_renderer`/`register_markdown_transformer`/
`register_entry_renderer` 注册消息渲染能力。这些槽位都可能为 `None`，使用前必须检测。

## 6. Rust 工具设计最佳实践

1. 把领域逻辑写成普通 Rust 函数，FFI 函数只负责转换、生命周期和错误映射。
2. 对普通逻辑写 `rlib` 单元测试；对 ABI 注册、加载和卸载写至少一个宿主 smoke test。
3. Schema 和实现使用同一字段命名，拒绝未知字段；对路径、URL、命令、数量和输出大小设置上限。
4. `poll` 不执行长时间阻塞任务。耗时工作放入线程/任务，handle 保存状态；取消标记必须能让任务在有限时间内结束。
5. 全局可变状态使用明确同步，避免把 session 或 tool call 状态存进无保护的 `static mut`。
6. runtime action、provider、renderer、event handler 和 resources handler 都是可选能力；注册前检查 vtable slot。
7. 插件注册失败返回非零；单个工具执行失败返回结构化错误，不要让整个宿主退出。
8. README 记录工具 schema、权限边界、持久化文件、网络访问、支持平台和卸载方式。

## 7. 使用 rpi dev 开发

在扩展 crate 目录运行：

```bash
rpi dev
```

rpi 会通过 Cargo metadata 识别当前 `cdylib`，首次编译后把版本化副本放在工作区
`.rpi/extensions/.dev/`，启动正常 TUI，并监控 `src/`、`build.rs`、package/workspace
`Cargo.toml` 和 `Cargo.lock`。源文件变化后会自动编译并走现有 reload 流程；编译失败
会保留当前已加载版本。

多扩展 workspace 必须明确选择 package：

```bash
rpi dev --package rpi-todo
rpi dev -P rpi-webfetch
```

其他模式：

```bash
rpi dev --release
rpi dev --no-watch
rpi dev-local
rpi dev --package rpi-todo -- --model gateway/model
```

调试单个扩展时使用 `rpi dev-local`（等价于 `rpi dev --local-only`）。它只加载当前
编译的扩展、当前项目目录的 skill/prompt，以及该扩展发现的 skill/prompt；不会扫描
全局扩展、已安装 Pi 包或其他项目资源，避免同名命令和无关命令干扰调试。内置命令
（包括 `/reload`）仍然可用。没有 Cargo cdylib 时优雅降级为 skills-only 模式。

TUI 中执行 `/reload` 会强制重新运行 Cargo build，再加载新阶段产物。Windows 上
已加载 DLL 无法原地覆盖，因此不要手工把 Cargo target DLL 复制到固定文件名；
`rpi dev` 的版本化 staging 会安全地完成切换，并在会话退出、loader 释放后清理。

## 8. 资源与配置约定

项目级资源放在 `.rpi/`，兼容 Pi 的资源可放在 `.pi/`。同名资源优先级是 `.rpi`
高于 `.pi`，项目高于全局，package 最后：

```text
.rpi/
├── SYSTEM.md
├── APPEND_SYSTEM.md
├── settings.json
├── skills/
├── prompts/
├── themes/
└── extensions/
```

动态 `resources_discover` 适合根据运行环境决定路径；固定资源优先使用 manifest 或
约定目录，便于安装器、欢迎页和 `/context` 在扩展代码运行前发现它们。

### 自定义加载路径

项目级 `.rpi/settings.json` 可以声明额外资源路径；`.pi/settings.json` 作为兼容
回退。路径相对于项目根目录，配置路径会排在约定目录前面：

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"],
  "packages": ["./packages/review-tools"]
}
```

`skills`、`prompts`、`extensions` 也可分别作为三个路径数组的简写。全局
`~/.rpi/agent/settings.json` 支持相同的 `skillDirs`、`promptDirs`、`extensionDirs`，
其中相对路径相对于全局 agent 目录。项目配置优先于全局配置；`.rpi` 资源和设置排在
`.pi` 之前。

单次运行可以显式追加路径：

```bash
rpi --skill ./extra-skills
rpi --prompt-template ./extra-prompts
rpi --extension ./target/debug/my_extension.dll
rpi --extensions-dir ./target/debug
```

`--skill`、`--prompt-template`、`--extension` 和 `--extensions-dir` 都可以重复使用。
额外的 Rust 扩展目录也可通过 `RPI_EXTENSIONS_DIR` 配置；Windows 使用分号分隔，
Linux/macOS 使用冒号分隔。命令行和环境变量适合本机开发、CI 与排错；团队共享配置
使用项目 `.rpi/settings.json`。

## 9. 发布检查清单

- `cargo fmt --check`、`cargo clippy --all-targets`、`cargo test` 通过。
- `crate-type` 包含 `cdylib`，使用 `export_plugin_v2!` / `export_plugin_v3!` 导出
  `rpi_plugin_register`（统一 ABI）。临时兼容 `rpi_plugin_register_v2` / `rpi_plugin_register_v3`。
- 依赖已发布的 `rpi-plugin-sdk` 兼容版本，不链接宿主私有 crate。
- `rpi dev --no-watch` 能编译、加载并注册预期工具。
- `rpi install <crate> --force` 的干净安装路径通过。
- Linux/macOS/Windows 构建由 CI 验证，README 标明支持矩阵。

## 10. 模型执行规则

模型收到"创建 rpi Rust 插件、扩展工具"任务时：

1. 先调用 `docs` 查询 `authoring`（扩展开发）或 `debugging`（调试）。
2. 阅读当前仓库现有 manifest、SDK 版本和相邻扩展约定，不猜 API。
3. 优先复制 `examples/plugin-stub` 的最小骨架。
4. 实现领域逻辑与边界测试，再连接 loader/ABI。
5. 使用 `rpi dev --no-watch` 与 `rpi install <crate> --force` 做真实 smoke test。
6. 不声称未验证的 Pi capability 已完全兼容；明确记录降级行为。
