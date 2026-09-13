# rpi Package 与扩展开发最佳实践

本文是 rpi 创建 package 和扩展项目的模型可读规范。开始编码前先选择后端：需要最大 Pi 生态兼容性和快速迭代时使用 Pi JS/TS package；需要 Rust 原生性能、强类型和稳定宿主边界时使用 Rust `cdylib` 扩展。不要在一个项目里重复实现两套相同工具，除非明确需要同时发布两个后端。

## 1. 两种扩展形式

| 目标 | Pi JS/TS package | Rust 原生扩展 |
| --- | --- | --- |
| 安装 | `rpi install-pi npm:...` | `rpi install crate-name` |
| 入口 | `package.json` 的 `pi.extensions` / `rpi.extensions` | `rpi_plugin_register` C ABI 符号 |
| 开发反馈 | 退出并重新启动 rpi | `rpi dev` watch + 热重载 |
| 静态资源 | skills、prompts、themes、SYSTEM.md | `resources_discover` 或随项目放入 `.rpi` |
| 运行环境 | Node.js，当前用户权限 | 本机动态库，当前用户权限 |
| 适用场景 | 兼容现有 Pi package、UI/命令扩展、快速交付 | 本地工具、系统集成、性能敏感或纯 Rust 项目 |

扩展应遵循能力检测：只调用宿主明确提供的 capability。JS/TS 检查 `ctx.capabilities`；Rust 插件检查 `PluginApiVt` 对应注册函数是否为 `Some`。缺少能力时返回清晰错误，不要依赖 `undefined`、空指针或 panic 表达不支持。

## 2. Pi JS/TS package 推荐结构

```text
my-pi-package/
├── package.json
├── README.md
├── src/
│   └── index.ts
├── dist/
│   └── index.js
├── skills/
│   └── my-skill/
│       └── SKILL.md
├── prompts/
│   └── review.md
└── themes/
    └── custom.json
```

发布 npm 时推荐提交或在 `prepack` 生成 `dist/`，并让 manifest 指向可以在普通 Node.js 环境加载的产物。rpi 支持 `.js`、`.mjs`、`.cjs`、`.ts`、`.tsx`，但发布包优先使用编译后的 JavaScript，以减少用户对 Node 版本和 `jiti` 的依赖。

### package.json

```json
{
  "name": "@scope/my-rpi-package",
  "version": "0.1.0",
  "type": "module",
  "files": ["dist", "skills", "prompts", "themes", "README.md"],
  "pi": {
    "extensions": ["./dist/index.js"],
    "skills": ["./skills"],
    "prompts": ["./prompts"],
    "themes": ["./themes"]
  },
  "scripts": {
    "build": "tsc -p tsconfig.json",
    "prepack": "npm run build"
  }
}
```

面向 Pi 和 rpi 同时发布时，把可移植配置放在 `pi` 对象中。只有 rpi 行为确实不同才增加 `rpi` 对象；rpi 对同一个资源键优先读取 `rpi`，缺失键再回退到 `pi`。不要复制两份完全相同的配置。

### 最小入口

```ts
export default function setup(pi: any) {
  pi.registerTool({
    name: "project_status",
    label: "Project status",
    description: "Read bounded project status without modifying files.",
    parameters: {
      type: "object",
      properties: {
        path: { type: "string", description: "Project-relative path" }
      },
      required: ["path"],
      additionalProperties: false
    },
    async execute(_toolCallId: string, params: { path: string }) {
      return {
        content: [{ type: "text", text: `status for ${params.path}` }]
      };
    }
  });

  pi.registerCommand("project-status", {
    description: "Show project status",
    async handler(args: string, ctx: any) {
      ctx.ui.notify(`Checking ${args || "."}`, "info");
    }
  });
}
```

### JS/TS 最佳实践

1. 工具名和 slash command 名使用稳定的小写 `snake_case` / `kebab-case`，发布后不要随意改名。
2. JSON Schema 必须限制必填字段、枚举、长度和数值范围；默认设置 `additionalProperties: false`。
3. 工具返回标准 `{content:[{type:"text",text:"..."}]}`，错误消息说明失败对象、原因和可执行的修复动作。
4. 文件、网络和命令操作必须有明确边界：限制根目录、响应大小、超时和重定向；拒绝凭据 URL、私网地址或越界路径。
5. 不要在模块加载阶段发网络请求或修改用户文件。注册应该快速、确定；副作用放到工具执行或命令 handler 中。
6. `ctx.ui.custom` 只用于真正需要全屏交互的流程。普通反馈用 `ctx.ui.notify`，并准备无 UI/能力缺失时的降级路径。
7. package 拥有与当前用户相同的文件和网络权限。README 必须声明外部访问、持久化位置和可能执行的命令。
8. 静态 skill 必须包含清晰 frontmatter、触发条件和边界；不要把整个 API 手册重复塞入 skill。

### 安装验证

```bash
npm run build
rpi install-pi ./my-pi-package --force
rpi package list
rpi --enable-pi-packages
```

进入 TUI 后确认欢迎页显示新增 tool/skill，并调用 slash command 和工具。修改 JS/TS package 后，退出并重新使用 `rpi --enable-pi-packages` 启动来验证重新加载；当前 `/reload` 不会重建 JS/TS package session。发布前再测试真实安装形式：

```bash
npm pack
rpi install-pi --force npm:@scope/my-rpi-package@0.1.0
```

## 3. Rust 原生扩展推荐结构

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

`cdylib` 是宿主加载的产物；同时保留 `rlib` 便于 Rust 集成测试。扩展边界只依赖 `rpi-plugin-sdk`，不要依赖 `rpi-cli`、`rpi-extensions` 或 harness 内部模块，否则插件会和宿主实现耦合。

### ABI 入口原则

```rust
use rpi_plugin_sdk::{register_entrypoint, PluginApiVt};

#[no_mangle]
pub extern "C" fn rpi_plugin_register(api: *const PluginApiVt, abi: u32) -> i32 {
    register_entrypoint(api, abi, |api| {
        let Some(register_tool) = api.register_tool else {
            return -1;
        };
        // 构建 StableToolSchema 和 execute/poll/cancel/destroy 函数表，
        // 然后调用 register_tool。完整实现参考 examples/plugin-stub。
        let _ = register_tool;
        0
    })
}
```

工具生命周期是 `execute -> poll -> cancel -> destroy`：

- `execute` 解析宿主传入的 JSON，释放收到的 owned `StbString`，创建每次调用独立的 handle。
- `poll` 必须非阻塞；未完成返回 `Pending`，完成返回 `Done` 或 `Err`。
- `cancel` 只设置线程安全取消标记，必须幂等，不能释放 handle。
- `destroy` 由宿主最终调用一次并释放 handle。
- 插件产生的 `StbString` 由插件提供的 `free_string` 释放；任何 Rust `String`、`Vec`、trait object 或带 `Drop` 类型都不能直接跨 ABI。
- 每个 `extern "C"` 边界都不能 unwind；插件内部捕获 panic 并转换为错误结果。

完整、可运行的 ABI 模板位于 `examples/plugin-stub/src/lib.rs`。创建新工具时先复制生命周期骨架，再替换参数 schema 和领域逻辑，不要重新设计所有权协议。

### Rust 工具设计最佳实践

1. 把领域逻辑写成普通 Rust 函数，FFI 函数只负责转换、生命周期和错误映射。
2. 对普通逻辑写 `rlib` 单元测试；对 ABI 注册、加载和卸载写至少一个宿主 smoke test。
3. Schema 和实现使用同一字段命名，拒绝未知字段；对路径、URL、命令、数量和输出大小设置上限。
4. `poll` 不执行长时间阻塞任务。耗时工作放入线程/任务，handle 保存状态；取消标记必须能让任务在有限时间内结束。
5. 全局可变状态使用明确同步，避免把 session 或 tool call 状态存进无保护的 `static mut`。
6. runtime action、provider、renderer、event handler 和 resources handler 都是可选能力；注册前检查 vtable slot。
7. 插件注册失败返回非零；单个工具执行失败返回结构化错误，不要让整个宿主退出。
8. README 记录工具 schema、权限边界、持久化文件、网络访问、支持平台和卸载方式。

## 4. 使用 rpi dev 开发 Rust 扩展

在扩展 crate 目录运行：

```bash
rpi dev
```

rpi 会通过 Cargo metadata 识别当前 `cdylib`，首次编译后把版本化副本放在工作区 `.rpi/extensions/.dev/`，启动正常 TUI，并监控 `src/`、`build.rs`、package/workspace `Cargo.toml` 和 `Cargo.lock`。源文件变化后会自动编译并走现有 reload 流程；编译失败会保留当前已加载版本。

多扩展 workspace 必须明确选择 package：

```bash
rpi dev --package rpi-todo
rpi dev -P rpi-webfetch
```

其他模式：

```bash
rpi dev --release
rpi dev --no-watch
rpi dev --package rpi-todo -- --model gateway/model
```

TUI 中执行 `/reload` 会强制重新运行 Cargo build，再加载新阶段产物。Windows 上已加载 DLL 无法原地覆盖，因此不要手工把 Cargo target DLL 复制到固定文件名；`rpi dev` 的版本化 staging 会安全地完成切换，并在会话退出、loader 释放后清理。

## 5. 资源与配置约定

项目级资源放在 `.rpi/`，兼容 Pi 的资源可放在 `.pi/`。同名资源优先级是 `.rpi` 高于 `.pi`，项目高于全局，package 最后：

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

动态 `resources_discover` 适合根据运行环境决定路径；固定资源优先使用 manifest 或约定目录，便于安装器、欢迎页和 `/context` 在扩展代码运行前发现它们。

### 自定义加载路径

项目级 `.rpi/settings.json` 可以声明额外资源路径；`.pi/settings.json` 作为兼容回退。路径相对于项目根目录，配置路径会排在约定目录前面：

```json
{
  "skillDirs": ["./team-skills"],
  "promptDirs": ["./prompts/shared"],
  "extensionDirs": ["./target/debug"],
  "packages": ["./packages/review-tools"]
}
```

`skills`、`prompts`、`extensions` 也可分别作为三个路径数组的简写。全局 `~/.rpi/agent/settings.json` 支持相同的 `skillDirs`、`promptDirs`、`extensionDirs`，其中相对路径相对于全局 agent 目录。项目配置优先于全局配置；`.rpi` 资源和设置排在 `.pi` 之前。需要作为一个整体复用或发布时，仍推荐使用 Pi package manifest 的 `rpi` / `pi` 字段并通过 `rpi install-pi` 启用。

单次运行可以显式追加路径：

```bash
rpi --skill ./extra-skills
rpi --prompt-template ./extra-prompts
rpi --extension ./target/debug/my_extension.dll
rpi --extensions-dir ./target/debug
```

`--skill`、`--prompt-template`、`--extension` 和 `--extensions-dir` 都可以重复使用。额外的 Rust 扩展目录也可通过 `RPI_EXTENSIONS_DIR` 配置；Windows 使用分号分隔，Linux/macOS 使用冒号分隔。命令行和环境变量适合本机开发、CI 与排错；团队共享配置使用项目 `.rpi/settings.json` 或 package manifest。

## 6. 发布检查清单

### JS/TS package

- `npm run build` 和测试通过。
- `npm pack --dry-run` 只包含必要产物，不包含密钥、缓存和本地配置。
- 从生成的 tarball 或指定 npm 版本执行一次 `rpi install-pi`。
- 使用 `rpi --enable-pi-packages` 启动后，settings 中启用的 package 的工具、命令、skills、themes
  在干净目录可发现；未带该参数时 package 不会被加载，`--no-extensions` 可作为最终关闭开关。
- Node 最低版本、权限和兼容能力写入 README。

### Rust 扩展

- `cargo fmt --check`、`cargo clippy --all-targets`、`cargo test` 通过。
- `crate-type` 包含 `cdylib`，导出符号严格命名为 `rpi_plugin_register`。
- 依赖已发布的 `rpi-plugin-sdk` 兼容版本，不链接宿主私有 crate。
- `rpi dev --no-watch` 能编译、加载并注册预期工具。
- `rpi install <crate> --force` 的干净安装路径通过。
- Linux/macOS/Windows 构建由 CI 验证，README 标明支持矩阵。

## 7. 模型执行规则

模型收到“创建 rpi package、Pi package、Rust 插件、扩展工具”任务时：

1. 先调用 `docs` 查询 `authoring`，并按目标选择 JS/TS 或 Rust 后端。
2. 阅读当前仓库现有 manifest、SDK 版本和相邻扩展约定，不猜 API。
3. 优先复制 `examples/plugin-stub` 或已有同类型 package 的最小骨架。
4. 实现领域逻辑与边界测试，再连接 loader/ABI。
5. 使用本节对应的安装路径做真实 smoke test。
6. 不声称未验证的 Pi capability 已完全兼容；明确记录降级行为。
