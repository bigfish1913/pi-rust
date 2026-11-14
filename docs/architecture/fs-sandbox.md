# 文件系统沙箱（rpi 自身即沙箱）

> 状态：**设计文档，尚未实现**。本文描述目标行为、跨平台实现方式、授权协议与落地阶段。
> 当前版本的 `rpi` 没有文件系统沙箱；实现进度以 [ROADMAP](../../ROADMAP.md) 为准。

## 1. 目标与非目标

### 目标

- **仅文件系统**：限制写操作，不限制读，不做网络、进程、信号隔离。
- **rpi 自身处于沙箱内**：agent、内置工具、所有插件、`bash` 及其全部后代都在同一约束下。
- **默认语义**：
  1. 所有路径可读；
  2. 项目目录（cwd）、系统临时目录（`/tmp`、`$TMPDIR`）可写；
  3. 其余路径可读但不可写；
  4. 用户可现场授权某个路径可写，授权可撤销、按作用域生效。
- **可配置**：配置文件与命令行均可启用/关闭；可配置保护路径（目录级与文件级）。
- **性能优先**：沙箱在启动时一次性应用，正常工具调用路径上不增加开销。
- **失败可见**：平台不支持或实现未完成时回退为无沙箱，并在启动横幅、TUI 状态和诊断输出中明确提示。

### 非目标

- 不限制网络访问、Unix socket、信号与进程创建。
- 不限制读取（包括读取 `~/.ssh` 等敏感路径；如需限制，属于后续独立能力）。
- 不提供 `rpi` 自带的 GUI；授权协议面向外部 GUI/IDE 客户端兼容，而不是新增一个图形应用。
- 不保证元数据操作（`chmod`/`chown`/`utimes`）被 Landlock 拦截，见第 10 节。

## 2. 不变量

以下四条是实现和测试的硬性约束，任何平台后端都必须满足同一语义：

| # | 不变量 |
| --- | --- |
| I1 | 读操作在任何路径上都放行 |
| I2 | 写操作仅在白名单内放行：cwd、临时目录、显式配置项、实际生效的会话目录 |
| I3 | `protected` 规则优先于 `writable`；未被任何规则命中的路径默认可读、不可写 |
| I4 | 授权只能在运行时扩大“可写”，不能扩大“可读”，也不能关闭沙箱本身 |

补充约束：

- **I5**：项目级配置只能收紧策略（追加 `protected`），不能追加 `writable`、不能关闭沙箱。
- **I6**：沙箱域一旦建立不可扩展；授权通过受信任的 broker 执行越权写，而不是放宽内核域。
- **I7**：能力缺失时必须显式提示；`mode = "required"` 时拒绝启动，`mode = "auto"` 时回退为无沙箱并保持提示。

## 3. 威胁模型与信任边界

| 主体 | 信任级别 | 说明 |
| --- | --- | --- |
| 模型输出 | 不可信 | 可能被提示注入，尝试写 `~/.ssh`、`~/.rpi/agent/extensions` 等 |
| 项目仓库及其 `.rpi/settings.json` | 不可信 | 只能收紧策略 |
| `rpi` 进程内代码（agent、工具、harness） | 可信 | 但仍在沙箱内运行，内核层约束同样生效 |
| 第三方插件 | 不完全可信 | 与宿主同进程同权限，因此需要进程级（而非钩子级）强制 |
| supervisor / broker | 受信任、极小 | 唯一有权执行“越权写”的组件；不解析模型自由文本 |
| 用户授权决定 | 可信 | 来自前端（TUI/GUI/RPC），由受信任代码提交 |

设计含义：扩展钩子（如 `before_tool_call`）只能做“询问/拒绝”，不能作为强制边界；真正的边界必须由内核（Linux Landlock、macOS Seatbelt、Windows restricted token）提供。

## 4. 策略模型

### 4.1 数据结构

```rust
pub struct SandboxPolicy {
    pub mode: SandboxMode,             // Off | Auto | Required
    pub writable: Vec<PathRule>,       // 可写（含创建、删除、改名）
    pub protected: Vec<PathRule>,      // 强制只读，优先于 writable
    pub allow_file_writes: Vec<PathBuf>, // 单文件例外：/dev/null、/dev/tty
    pub approvals: ApprovalConfig,     // 是否允许动态授权、默认作用域
}

pub enum PathRule {
    Dir(PathBuf),   // 递归；对应 Landlock 的 PATH_BENEATH
    File(PathBuf),  // 精确文件
}
```

优先级：`protected > writable > 默认只读`。目录规则递归生效；文件规则精确匹配。符号链接在策略编译阶段解析为真实路径。

### 4.2 配置

全局配置（用户可信）：`~/.rpi/agent/settings.json`

```jsonc
{
  "sandbox": {
    "enabled": true,
    "mode": "auto",                     // auto | required | off
    "writable": ["${cwd}", "${tmp}", "${cwd}/.rpi"],
    "protected": [
      "${home}/.ssh",
      "${home}/.aws",
      "${home}/.gnupg",
      "${home}/.rpi/agent"              // 防止模型持久化恶意插件
    ],
    "allowFileWrites": ["/dev/null", "/dev/tty"],
    "approval": { "enabled": true, "defaultScope": "session" },
    "auditLog": "${home}/.rpi/audit/sandbox.jsonl"
  }
}
```

支持展开：`${cwd}`、`${home}`、`${tmp}`、`~`。相对路径相对于 `cwd` 解析。

### 4.3 配置来源与提权规则

| 来源 | 追加 `writable` | 关闭沙箱 | 说明 |
| --- | --- | --- | --- |
| 全局 `~/.rpi/agent/settings.json` | 允许 | 允许 | 用户自己的配置 |
| 项目 `.rpi/settings.json` | **禁止** | **禁止** | 仓库不可信，只能追加 `protected` |
| 命令行 `--sandbox` / `--no-sandbox` / `--allow-write <path>` | 允许 | 允许 | 用户显式意图 |
| 环境变量 `RPI_SANDBOX=0` | 允许 | 允许 | 与命令行等价 |

### 4.4 自动纳入的可写路径

以下路径即使未写入配置也必须可写，否则 rpi 自身无法工作：

- 当前工作目录 `cwd`；
- 实际生效的会话目录（`sessionDir`、`--session-dir`、`RPI_CODING_AGENT_SESSION_DIR` 的最终结果）；
- `--export` 的目标目录；
- 系统临时目录。

### 4.5 维护类命令

`rpi install` / `uninstall` / `auth login` / `update` / `package link` 由用户主动发起，会写 `~/.rpi`、`~/.local/bin` 等位置。这些子命令**默认绕过沙箱**，启动时打印一行：

```text
sandbox: bypassed (maintenance command)
```

模型在会话内无法获得同样的绕过能力：`bash` 子进程继承的沙箱域不可撤销，重新执行 `rpi` 也无法逃逸。

## 5. 强制架构

### 5.1 组件

```text
                    ┌──────────────────────────────┐
  用户执行 rpi ───▶ │ supervisor（不受限，极小）    │
                    │  · 编译策略、探测平台能力      │
                    │  · fork/启动沙箱内 rpi         │
                    │  · 处理授权请求（broker）      │
                    │  · 写审计日志                  │
                    └───────────┬──────────────────┘
                                │ socketpair / 命名管道
                    ┌───────────▼──────────────────┐
                    │ rpi agent（已进入内核沙箱）    │
                    │  TUI / harness / tools / 插件  │
                    │  bash 后代自动继承沙箱域       │
                    └──────────────────────────────┘
```

- **Linux**：supervisor 仅在启用动态授权时需要；关闭授权时可完全省略，进程在建立沙箱后继续运行。
- **macOS**：Seatbelt 在进程内应用，supervisor 同样只为授权存在。
- **Windows**：restricted token 不能施加到当前进程，因此 supervisor 是必需的：由它以受限 token 启动子进程 `rpi`。

### 5.2 启动顺序（关键约束）

当前入口是 `crates/rpi-cli/src/bin/rpi.rs` 的 `#[tokio::main]`，多线程 runtime 在任何业务代码之前创建。Landlock 与 Seatbelt 都按线程生效，因此必须在 runtime 创建前应用。目标形态：

```rust
fn main() -> ExitCode {
    // 1. 同步预检：参数、配置、策略、子命令判定（不初始化 tracing / 插件 / 线程）
    let boot = rpi_sandbox::bootstrap();

    // 2. 应用沙箱
    //    Linux:   fork broker（可选）→ landlock_restrict_self
    //    macOS:   sandbox_init(profile)
    //    Windows: supervisor 以受限 token 启动子进程（本进程留作 supervisor）
    let guard = boot.enter();

    // 3. 创建多线程 runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    ExitCode::from(rt.block_on(rpi_cli::app::run()) as u8)
}
```

硬性要求：

- `bootstrap()` 只做同步读操作与能力探测，不得创建线程、不得初始化 DNS/插件/日志运行时；
- 若未来出现无法提前执行的初始化，退化为 re-exec 自身（一次性 +10–20 ms），不得放弃全进程约束；
- Windows 上父进程作为 supervisor 等待子进程，转发信号/退出码，并负责 ACL 清理。

## 6. 平台后端

### 6.1 Linux：Landlock

| 项 | 内容 |
| --- | --- |
| 机制 | `landlock_create_ruleset` + `landlock_add_rule(PATH_BENEATH)` + `landlock_restrict_self` + `prctl(PR_SET_NO_NEW_PRIVS)` |
| 处理位 | `WRITE_FILE`、`MAKE_*`、`REMOVE_*`、`REFER`、`TRUNCATE`；`READ_FILE`/`READ_DIR`/`EXECUTE` 不处理即放行 |
| ABI | v1（5.13）基础；`REFER` 需 v2（5.19）；`TRUNCATE` 需 v3（6.2）。启动时探测 ABI，低版本按能力降级并提示 |
| 继承 | 子进程与所有后代自动继承，无需 `pre_exec` |
| 权限 | 非特权用户即可使用，不依赖 user namespace / bwrap |

已知行为：

- 规则绑定 **inode**：启动后若可写目录被删除重建，新 inode 不在规则内，写会被拒绝；重启 rpi 恢复。
- 沙箱建立前已打开的 fd 仍可写（rpi 自身的会话/日志 fd 属于预期情况）。
- `chmod`/`chown`/`utimes` 不受约束（见第 10 节）。

### 6.2 macOS：Seatbelt

| 项 | 内容 |
| --- | --- |
| 机制 | `sandbox_init(profile, 0, &err)`，与 `sandbox-exec` 同一套策略引擎 |
| Profile | `(allow default)` + `(deny file-write*)` + 对每个可写根 `(allow file-write* (subpath ...))` + 单文件 `(literal ...)` |
| 生效位置 | 启动最早阶段，进程内应用，无需 re-exec |
| 注意 | 路径先 `realpath`；`/dev/null`、`/dev/tty` 需显式放行；API 已 deprecated，但当前系统仍可用 |

### 6.3 Windows：restricted token + ACL

| 项 | 内容 |
| --- | --- |
| 机制 | 按沙盒名派生合成 SID → 在可写根加 Allow ACE（`OI|CI`）→ `CreateRestrictedToken(WRITE_RESTRICTED)` → `CreateProcessAsUserW` |
| 为什么必须 spawn | token 是进程 primary token，无法把当前进程降权 |
| 嵌入方式 | 原进程当 supervisor，以受限 token 启动隐藏模式的自身，不额外发布启动器 |
| ACL 生命周期 | ACL 修改是持久的；需要标记文件，崩溃后下次启动执行清理 |
| bcrypt | restricted token 下 `bcrypt.dll` 的 `DllMain` 失败导致 `0xC0000142`；正解是切换 rustls crypto provider，去掉对 `bcrypt.dll` 的静态依赖，而不是长期携带 shim |

实现参考：`rpi-sandbox`（Windows 启动器实验）已经验证了合成 SID、`WRITE_RESTRICTED`、`DuplicateTokenEx` 等关键细节；正式实现应将这些逻辑收进 `rpi-sandbox` crate，由 `rpi` 自身调用。

### 6.4 能力探测与回退

```rust
pub enum SandboxSupport {
    Full(Backend),
    Partial { backend: Backend, reason: String },
    Unsupported { reason: String },
}
```

| `mode` | 行为 |
| --- | --- |
| `auto`（默认） | 不支持或未实现时无沙箱运行，启动横幅、TUI 状态、`/sandbox` 面板三处提示 |
| `required` | 拒绝启动，返回非 0 退出码并给出可操作提示 |
| `off` | 不尝试应用 |

## 7. 授权协议

### 7.1 前端抽象

授权逻辑不放在 TUI 中，而是定义与 UI 无关的请求/响应模型：

```rust
#[async_trait]
pub trait ApprovalFrontend: Send + Sync {
    async fn ask(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ApprovalError>;
}

pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub session_id: String,
    pub tool_call_id: Option<String>,
    pub kind: ApprovalKind,
    pub created_at: SystemTime,
    pub deadline: Option<SystemTime>,
    pub options: Vec<ApprovalOption>,
}

pub enum ApprovalKind {
    FsWrite {
        operation: WriteOp,           // create | overwrite | append | delete | rename
        paths: Vec<CanonicalPath>,    // 解析后的真实路径
        raw_input: Vec<String>,       // 用户/模型输入的原样路径，用于展示
        bytes: Option<u64>,
        exists: bool,
        outside_workspace: bool,
    },
}

pub struct ApprovalDecision {
    pub id: ApprovalId,
    pub decision: Decision,           // Allow | Deny
    pub scope: ApprovalScope,         // Once | Session | Project
    pub remember: bool,
    pub reason: Option<String>,
}
```

前端实现：

| 实现 | 场景 |
| --- | --- |
| `TuiApproval` | 交互式 TUI 弹窗 |
| `RpcApproval` | `--server`/`--connect` 客户端、未来桌面/Web GUI、IDE 插件 |
| `HeadlessApproval` | `-p`/CI：默认拒绝，可用 `--allow-write` 预授权 |

### 7.2 协议消息

授权请求作为远程事件流中的一类事件，外部客户端无需了解沙箱内部实现：

```jsonc
// server → client
{ "type": "approval.request", "v": 1, "request": { /* ApprovalRequest */ } }

// client → server
{ "type": "approval.response", "v": 1, "decision": { /* ApprovalDecision */ } }
```

约束：

- **版本化**：新增字段只加不删，未知字段忽略；
- **可重放**：未决请求持久化，客户端重连后可重新展示；
- **多客户端仲裁**：同一 `id` 只接受第一个有效决定，其余返回 `already_decided`；
- **超时默认拒绝**：不得让 agent 无限等待；
- **文案分离**：协议只传 `kind` 与参数，展示文案由前端本地化。

### 7.3 授权执行流程

```text
1. 工具尝试写 /etc/hosts → 内核返回 EACCES
2. 映射为 SandboxDenied { path, op }
3. 调用 ApprovalFrontend（TUI / RPC / headless）
4. 用户选择：仅本次 / 本会话 / 拒绝
5. 允许 → agent 把 { path, op, scope } 交给 broker
6. broker 二次校验：策略 → 会话授权 → realpath 仍在授权范围
7. broker 以 openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS) 执行写入
8. 审计日志追加一条（由 broker 写入，沙箱内进程无权修改）
```

要点：

- 沙箱进程始终没有越权路径的写权限；“授权”只是让 broker 代为执行。
- 授权记录绑定解析后的真实路径，每次使用重新解析，防止“先授权软链、后换目标”。
- 模型无法伪造授权：请求由模型触发，决定来自前端与受信任代码。

### 7.4 `bash` 子进程

- 子进程越权写直接得到 `Permission denied`，不在管道中弹窗（避免死锁）。
- 模型看到错误后可改用 `write`/`edit` 工具重试，从而进入授权流程。
- 后续可选：提供沙箱内 CLI（连接 broker），让脚本也能申请授权；首个版本不做。

## 8. 状态与 UX

| 位置 | 内容 |
| --- | --- |
| 启动横幅 | `🔒 sandbox: landlock · rw: <cwd>, /tmp · protected: 3` 或 `🔓 sandbox: off (unsupported: no landlock)` |
| TUI 页脚 | 常驻锁标记：`🔒` 有沙箱 / `🔓` 无沙箱 / `⏸` 维护命令绕过 |
| `/sandbox` | 展示后端、模式、可写根、保护项、本会话已授权路径、最近拒绝记录 |
| 拒绝弹窗 | 显示原始路径与真实路径、操作类型、字节数，提供“仅本次 / 本会话 / 拒绝” |
| `--mode json` | 启动事件包含 `sandbox: { enabled, backend, writable }` |
| 日志 | `auto` 回退时在 stderr 打印一行原因；不得静默降级 |

## 9. 性能预算

| 环节 | 估算 | 说明 |
| --- | --- | --- |
| Linux 应用 Landlock | 50–200 µs | 数个 syscall，一次性 |
| macOS `sandbox_init` | 1–5 ms | profile 编译，一次性 |
| Windows token + ACL | 5–50 ms | 取决于可写目录规模，一次性 |
| 每次 `write`/`edit` | 0 | 内核在打开文件时顺带判定 |
| 每次 `bash` | 0 | 域继承，无需 `pre_exec` |
| 越权写（需授权） | 0.2–0.5 ms + 用户决定时间 | 本地 socket/命名管道往返 |
| 无 broker 模式额外进程 | 0 | Linux/macOS 首选 |

对比外部启动器（bubblewrap 等）：每次调用 5–20 ms，且需要额外二进制与命名空间支持；整进程内核沙箱在正常路径上没有这些成本。

## 10. 已知缺口

1. Landlock 不拦截 `chmod`/`chown`/`utimes` 等纯元数据操作。
2. 不隔离网络、Unix socket、信号与进程（本设计只做文件系统）。
3. `bash` 越权写不弹窗，只能通过工具重试触发授权。
4. macOS Seatbelt API 已 deprecated，未来系统版本可能需要替代方案。
5. Windows ACL 修改是全局副作用，异常退出可能残留，需要清理机制。
6. 若把 `~/.rpi/agent` 配置为可写，模型可持久化恶意插件；默认必须保护该目录。
7. 沙箱建立前已打开的 fd 不受约束。

## 11. 测试计划

| 类别 | 用例 |
| --- | --- |
| 基础语义 | cwd 可写、`/tmp` 可写、`/etc` 可读不可写、读任意路径成功 |
| 路径逃逸 | `..`、符号链接指向外部、硬链接、`/proc/self/fd`、跨目录 `rename`（`REFER`） |
| 子进程继承 | `bash -c 'echo x > /etc/y'` 失败；`sh -c 'python -c ...'` 同样失败 |
| 授权 | 同意后 broker 写入成功；撤销后立即拒绝；作用域 once/session 正确；超时拒绝 |
| 配置 | 项目配置无法追加 `writable`、无法关闭沙箱；`protected` 覆盖 `writable` |
| 回退 | 能力缺失时 `auto` 无沙箱运行并提示；`required` 拒绝启动 |
| 维护命令 | `rpi install` 等绕过并打印提示 |
| 性能 | 启动增量、100 次写操作、100 次 `bash` 调用与基线对比 |
| 平台矩阵 | Linux（Landlock v1/v2/v3 内核）、macOS、Windows 各跑一遍 |

## 12. 落地阶段

| 阶段 | 内容 | 验收 |
| --- | --- | --- |
| M0 | `rpi-sandbox` 策略模型、路径展开、优先级、能力探测桩；`settings.rs` 增加 `sandbox` 段与“项目只能收紧”合并；同步 `bootstrap()` 空实现 | 配置可解析、可展示、行为不变 |
| M1 | Linux Landlock 全进程应用（入口改为同步预检）；`EACCES → SandboxDenied`；bash 后代继承 | 基础语义与逃逸用例通过；默认 `auto` |
| M2 | supervisor + broker、`ApprovalFrontend`（TUI/headless/RPC）、审计、撤销、pending 持久化 | 授权链路完整；多客户端仲裁正确 |
| M3 | macOS Seatbelt | 同一测试矩阵通过 |
| M4 | Windows restricted token + ACL 事务 + 崩溃清理 + crypto provider 切换 | 测试矩阵通过；无残留 ACL |
| M5 | 外部客户端（GUI/IDE）消费授权事件；远程模式下的授权认证 | 协议兼容性测试 |

默认开启节奏：M1 稳定后 Linux 默认 `auto` 开启；macOS/Windows 先 opt-in，稳定后再调整默认值。

## 13. 代码落点

| 变更 | 位置 |
| --- | --- |
| 新增 crate | `crates/rpi-sandbox/`（策略、平台后端、能力探测、broker 客户端/服务端；不依赖其它 `rpi-*`） |
| 启动接线 | `crates/rpi-cli/src/bin/rpi.rs`（同步预检 + 应用沙箱 + 建 runtime） |
| 配置合并 | `crates/rpi-cli/src/settings.rs`、`crates/rpi-cli/src/config.rs` |
| 工具层 | `crates/rpi-tools/src/os_env.rs`（捕获 `EACCES`、映射 `SandboxDenied`、broker 重试） |
| 授权 UI | `crates/rpi-cli/src/interactive_tui/`（弹窗、`/sandbox`、页脚） |
| 事件 | `rpi-harness` 会话事件、`--mode json`、远程模式协议 |
| 维护命令 | `crates/rpi-cli/src/install.rs`、`auth.rs`、`updates.rs`（显式绕过标记） |

注意：**不要实现为扩展**。扩展无法在进程启动时应用约束，也无法替换宿主构造的 `OsExecutionEnv`；`rpi-sandbox` 的 Windows 实验结果已经证明这条路只能靠外部启动器。
