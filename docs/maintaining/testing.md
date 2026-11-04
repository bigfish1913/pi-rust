# 回归测试清单与运行流程

## 当前覆盖清单

本页是仓库回归测试的统一入口，按**场景组**列出测试，而不是逐个罗列 Rust
`#[test]`。CI 状态以 [工作流配置](../../.github/workflows/ci.yml) 为准；“已接入”只说明
配置会执行，不代表远端最新一次运行已经通过。默认 CI 不读取真实用户会话、不使用真实
模型凭据，也不会替换安装产物或重启线上服务。

### 已接入默认 CI

| 场景组 | 本地入口 / 主要测试 | CI 位置与边界 |
| --- | --- | --- |
| Rust 基础、文档与默认功能回归 | `task test` / `cargo test --workspace --locked` | Ubuntu `test`；包含默认 feature 下的单元、集成和 doc tests，ignored/条件提前返回不等于实际覆盖。 |
| macOS regular 主屏历史回看、后台计时/输出及命令完整性 | `cargo test -p rpi-tui --test main_screen_scroll_up --locked`；[修复语义与验收步骤](../debugging/macos-iterm2-scrollback.md) | PR #237 已修复计时刷新、历史区光标与强制重绘路径，保留既有历史重排测试；默认 workspace 会执行。虚拟终端不代替真实 iTerm2 视觉验证。 |
| MSRV、跨平台编译与格式 | Rust 1.88 `cargo check`、stable all-targets、`cargo fmt --all -- --check` | `msrv` / `check` 覆盖 Linux、Windows；格式在 `test`，编译检查不能代替运行测试。 |
| 会话恢复、消息顺序、工具重放、半截 JSONL、cwd | `task test:recovery` | 已含于 `test` 的 workspace 测试，无须重复执行同一组 Rust tests。 |
| 冷帧/冷消息、分页、fork、progress 升级与完整性 | `rpi-harness` 的 JSONL、branch queries、storage contract 等测试 | `test` 默认 Rust 测试；主要是语义/I/O 契约，不代替全进程耗时门禁。 |
| 资源选择、重绑定、saved-session 读取及隔离 | CLI `resource_selection`、`resource_management`、`saved_session_read`，harness 对应测试 | `test` 默认 Rust 测试；只读与写入入口分别覆盖。 |
| 真实 CLI 消息/工具 roundtrip、重启、kill 后恢复和会话隔离 | `task test:session`、`test_session_smoke.py` | `test` 显式构建 CLI，运行两种 RPC 编码；状态扩展未传入时不算覆盖。 |
| 编辑器大粘贴、undo/redo owner、多轮 RPC 内存/CPU/最终结论 | `task test:resources`、`test_resource_smoke.py`、TUI Rust tests | `resources`；release 编辑器和两种 RPC 编码各 5 个测量轮次，不是实际终端视觉验证。 |
| **701,000 append frames 的历史冷启动** | **`task test:cold-start`、`test_resource_cold_start.py`** | **`resources`；modern/legacy 各 701,000 append frames + 21,733 entries，每格式 3 次 fresh process；检查 ready、查询、内容、idle 和清理。** |
| 外部工具 RPC、结果隔离、abort 原子性与 stdin EOF 生命周期 | `external-tools-smoke.py --rpi ... --diagnostics-dir ...` | `resources`；compact-deltas / snapshot 各 3 个真实 native 场景，使用隔离 loopback fixture。 |
| 多版本资源比较控制器 | `python -B scripts/test_compare_resources.py` | `resources`；验证统计、门槛、语义、参数与结果判定，不代表实际多版本性能对比已执行。 |
| PTY 压力控制器、资源上限、有限输出与 owned cleanup | `python -B scripts/test_resource_tui.py` | `resources`；**fake PTY 单测**，无 pywinpty 依赖，不代表 Windows ConPTY 实测。 |
| SDK/插件 ABI、provider bridge、真实 plugin-stub | `rpi-plugin-sdk` provider tests、`plugin_stub_smoke` 等 | `test` 显式构建仓库 plugin-stub；`windows-plugin-lifecycle` 检查 Windows ABI/DLL，不能替代独立业务扩展。 |
| Windows 安装器 | `scripts/tests/install.Tests.ps1` | `windows-installer`，分别用 Windows PowerShell / PowerShell Core 离线执行。 |
| 发布授权、Release PR、渠道、预构建清单 | `test_release_automation.py`、`test_release_pr_readiness.py`、`test_release_channels.py`、`test_prebuilt_release.py` | `test`；mock/临时仓库测试，不执行真实发布。 |
| 官网 HTTP 校验器的状态码、缓存、内容与重定向断言 | `python -B scripts/test_website.py VerifierTests` | `test`；mock 响应单测，不访问线上站点，不代表真实 Caddy HTTP 集成。 |

### 手动、可选或尚未接入的覆盖

| 场景 | 当前入口 | 为什么不与默认 CI PASS 混淆 / 后续条件 |
| --- | --- | --- |
| 实验 native store 与进程终止/SQLite spill 恢复 | `cargo test -p rpi-harness --features experimental-native-store --lib process_tests --locked -- --test-threads=1` | 具备合成、离线、子进程隔离条件，**适合后续独立 feature CI job**；当前工作流未启用此 feature，默认 workspace 绿灯不覆盖它。详见 [实验进程测试](../architecture/experimental-native-process-tests.md)。 |
| 真实 Windows TUI 压力 | `compare-resources.py --case tui ...` | 需要 Windows ConPTY、可用的 pywinpty 与实际输入确认；当前仅控制器单测进 CI，实际终端缺依赖/未确认输入应记 SKIP。 |
| 真实 macOS / iTerm2 长会话向上回看 | [iTerm2 人工验收](../debugging/macos-iterm2-scrollback.md#iterm2-人工验收修复后仍必需) | 后台工具仍运行、耗时行进入历史区时观察多帧闪屏/跳位，以及完整命令和消息唯一性；当前环境未执行，不能用虚拟终端或 fullscreen 对照 PASS 代替。 |
| 多版本 binary 的资源/冷启动 AB/BA 对比 | `compare-resources.py --variant ...`；`resource-cold-start.py --baseline ... --candidate ...` | 需要可验证的旧、新产物与一致 profile；CI 冷启动门禁只测当前产物，不伪造旧基线。可后续做受控 performance workflow。 |
| 独立 provider/subagent/btw/todo/durable 库 | 下文真实库环境变量或 session smoke 的 `--extension` | 依赖独立仓库/可信共享库及明确版本，当前 CI 未准备这些产物；缺失时记 SKIP。 |
| 真实模型多轮资源回归 | `task test:resources:live -- ...` | 有凭据、费用与外部服务依赖，必须显式授权、有请求预算，不进入默认 PR CI。 |
| 真实 Caddy HTTP 集成 | 设置 `CADDY_BIN` 后 `python -B scripts/test_website.py` | 可后续在固定 Caddy 版本的独立 job 跑本地服务；当前默认 CI 只运行 `VerifierTests`，不声称真实服务通过。 |
| 真实域名 Web 打开历史/首屏、认证与渲染 | 隔离浏览器环境中的专项验收 | Native ready 不等于 Web 首屏；真实域名认证、Vite/正式 assets 和服务状态需分别核实，不能绕过认证或操作真实 session 凑 PASS。 |
| 历史 `scripts/rpc-smoke.py` | 手工脚本，不纳入默认 CI | 沿用用户 HOME/配置和 provider，隔离条件不足；日常恢复用 `session-smoke.py`，需先改造隔离与 fixture 才能接 CI。 |

新增或修复场景时，同步更新本表、实际测试入口和对应 CI 步骤；详细性能预算只在明确
负载、profile、平台与测量边界下解释。**RSS 降低、I/O 次数减少、storage load、native
ready 和浏览器首屏是不同指标，不能互相代替。**

## macOS / iTerm2 原生历史滚动验收

场景、提交归因、协议测试和人工步骤见 [macOS / iTerm2 长会话验收](../debugging/macos-iterm2-scrollback.md)。

```bash
cargo test -p rpi-tui --test main_screen_scroll_up --locked -- --nocapture
```

验收覆盖后台计时、持续追加输出的逐帧完整性、历史区光标隐藏及返回、列 0/显示设置，
以及强制重绘对消息唯一性和外部残留单元格的修复。默认 workspace 测试会执行，
并保留既有历史重排测试。
regular 工具面板使用稳定运行状态，实时耗时仍在输入区工作状态中显示；fullscreen 和
完成总耗时不变。真实 macOS / iTerm2 视觉验收未执行，不能用虚拟终端或 fullscreen
对照 PASS 代替；真实历史编辑/重排仍可能重建 scrollback。

## 下次直接运行

从 `pi-rust` 仓库根目录执行。前提是 Python 3、Rust/Cargo 和 Go Task 已安装。下面是本机 Windows PowerShell 可直接复制的命令；**任一步失败就先保留日志排查，不要把后续部分成功当成整轮通过。** 测试不会自动安装新 binary、替换全局 DLL 或重启服务。

### 1. 日常回归：会话、恢复、资源和历史冷启动

```powershell
task test:session
if ($LASTEXITCODE -ne 0) { throw 'Session 冒烟失败，请检查输出中的诊断目录' }
task test:recovery
if ($LASTEXITCODE -ne 0) { throw '恢复回归失败' }
task test:resources
if ($LASTEXITCODE -ne 0) { throw '资源回归未通过；请区分 FAIL 与平台 SKIP 并保留诊断' }
task test:cold-start
if ($LASTEXITCODE -ne 0) { throw '历史冷启动回归未通过；检查 modern/legacy 各自门槛与清理结果' }
```

第一项自动构建当前源码、运行 Python 控制器测试，再检查两种 RPC 编码下的消息/工具结果、重启恢复、真实 kill 后续跑和 session 隔离；每次自动创建唯一诊断目录。第二项运行底层恢复回归。第三项构建 release CLI 和实际编辑器压力进程，检查大粘贴/撤销历史及两种 RPC 编码的多轮内存、CPU、完成性和最终结论。第四项用合成海量流式记录验证现代格式恢复与旧格式首次升级，每种格式启动 5 个全新 CLI 进程。**四项默认都不加载全局扩展，不调用真实模型，不能替代下面的扩展检查。** 冷启动 Task 构建带 `--locked --offline`，首次使用前需准备好 Cargo 依赖缓存；build 和性能测量不混为一项。

准备合并或发版时，再运行完整检查（不要只依据某次测试数量判断是否通过）：

```powershell
cargo fmt --all -- --check
if ($LASTEXITCODE -ne 0) { throw '格式检查失败' }
cargo test --workspace --locked --target-dir target/session-smoke-cargo
if ($LASTEXITCODE -ne 0) { throw '完整 workspace 测试失败' }
```

### 2. 检查当前实际安装的版本

这一步与源码构建分开，防止“源码已修复，但正在使用的安装版仍有问题”被遗漏：

```powershell
$rpi = (Get-Command rpi.exe -ErrorAction Stop).Source
& $rpi --version
$diag = 'target/session-smoke-installed-' + [guid]::NewGuid().ToString('N')
python -B scripts/session-smoke.py --rpi "$rpi" --diagnostics-dir "$diag"
if ($LASTEXITCODE -ne 0) { throw "安装版回归失败，诊断目录：$diag" }
```

要验证安装中的状态扩展，在同一 PowerShell 中继续执行；缺少 DLL 时应先确认安装位置，不要删除参数后声称扩展已通过：

```powershell
$extensions = Join-Path $env:USERPROFILE '.rpi/agent/extensions'
$diag = 'target/session-smoke-installed-state-' + [guid]::NewGuid().ToString('N')
python -B scripts/session-smoke.py --rpi "$rpi" --diagnostics-dir "$diag" --extension "$extensions/rpi_todo.dll" --extension "$extensions/rpi_durable.dll"
if ($LASTEXITCODE -ne 0) { throw "已安装状态扩展回归失败，诊断目录：$diag" }
```

### 3. 与相邻仓库新构建的状态扩展对比

相邻仓库默认是 `../rpi-package`。仅构建和测试，不执行 install，也不替换安装中的扩展。先完成步骤 1，确保待测宿主是当前源码产物，再运行：

```powershell
$siblingTarget = Join-Path $PWD 'target/session-smoke-sibling-cargo'
cargo build --manifest-path ../rpi-package/Cargo.toml -p rpi-todo -p rpi-durable --locked --target-dir "$siblingTarget"
if ($LASTEXITCODE -ne 0) { throw '相邻扩展构建失败；不能记为扩展测试通过' }
$diag = 'target/session-smoke-sibling-state-' + [guid]::NewGuid().ToString('N')
python -B scripts/session-smoke.py --rpi target/session-smoke-cargo/debug/rpi.exe --diagnostics-dir "$diag" --extension "$siblingTarget/debug/rpi_todo.dll" --extension "$siblingTarget/debug/rpi_durable.dll"
if ($LASTEXITCODE -ne 0) { throw "相邻状态扩展回归失败，诊断目录：$diag" }
```

若在步骤 1 设置了 `SESSION_CARGO_TARGET_DIR`，这里的 `--rpi` 也要指向该目录下的 binary。首次构建可能下载依赖；依赖已缓存时可为 Cargo 加 `--offline`。不要为解决构建阻塞而临时改相邻仓库的 SDK 版本或复制旧 DLL 冒充新产物。

### 4. 结果判定与诊断位置

| 结果 | 判定 |
| --- | --- |
| `task test:session` | Python 控制器测试成功，且 `compact-deltas`、`snapshot` 均为 PASS；进程退出码为 0。 |
| `task test:recovery` | 所有测试目标均无失败，Task 退出码为 0。 |
| `task test:resources` | 编辑器与两个 RPC 场景全部 PASS；逐轮资源预算和最终结论通过。退出 77 / 不支持平台为 SKIP，不算通过。 |
| `task test:cold-start` | 控制器和真实 RPC 小测通过，modern/legacy 各 5 次完整规模观测全部完成，ready/query 门槛、消息与 idle 语义、清理均 PASS；旧格式使用单独的显式预算。 |
| todo / durable | 显式传入 DLL 后，`results.json` 中对应 `todoState` / `durableState` 为 PASS；没有加载库时的 SKIP 不算通过。 |
| provider / subagent / btw | 按下文设置真实库变量，使用 `--nocapture` 确认没有因缺少库而提前跳过。 |

每次 smoke 的诊断目录均由控制台输出，主要查看：

- `results.json`：两种编码的最终结果及可选状态扩展结果。
- `<encoding>/failure.log`：断言或进程错误。
- `<encoding>/<阶段>/command.json`、`stdin.jsonl`、`stdout.jsonl`、`stderr.log`：实际命令、RPC 请求、事件和宿主输出。
- `<encoding>/provider-requests.json`：fixture 实际收到的消息历史，用于确认恢复后的历史确实进入模型请求。
- `<encoding>/snapshot-*.json` 和 `sessions/`：恢复前后消息及隔离的原始 session 文件。

失败目录必须保留；下一次使用新目录。不要把之前某次 `target/session-smoke-validation-summary.json` 当成新一轮测试结果，它只是当次本地验证的汇总，不会自动更新。

### 常见回归信号

- **todo 添加成功，重启后 `list` 为空**：此前已在旧安装 DLL 上复现。先分别测试安装 DLL 和相邻新构建 DLL；新源码通过不代表全局安装已更新。
- **带 durable 时 kill 重启后 `pendingResume=false`**：此前宿主曾因尾部 custom entry 遮住用户消息而漏判续跑。当前源码已有修复和 `custom_entries_do_not_hide_a_crashed_user_prompt` 回归；若安装版失败、源码版通过，先核实实际运行的 binary。
- **构建报 SDK 字段不匹配，独立 target 构建通过**：检查跨仓库共享 Cargo target 的旧产物；这里的 Task 已显式隔离 target，不需要修改 SDK 或清空用户全局缓存。
- **相邻旧 `tools/rpi-package-smoke` 依赖版本无法解析**：此前该工具的路径依赖要求 `^0.3.3`，与当前宿主 `0.15.0` 不匹配。应报告构建阻塞，不得计为 PASS；使用上述真实宿主流程及下文真实库测试验证当前覆盖范围。

## 测试分层

- `task test` 保持原样：运行 `cargo test --workspace --locked`。
- `task test:recovery`：运行 `rpi-harness` 的 `harness_run_e2e`、`context_message_order`、`frame_progress_recovery`、`m5c_jsonl_torn_tail`、`jsonl_failure_regressions`，以及 `rpi-cli` 的 `session_cwd_restore`。它们覆盖提交记录恢复、半截 JSONL 修复、工具重放、待恢复运行、消息队列隔离和会话工作目录恢复；不等同于真实进程被终止后的端到端检查。
- `task test:session`：先构建 `rpi-cli`、运行控制器的 Python 单元测试，再对该二进制运行 `scripts/session-smoke.py`。默认显式使用 `--target-dir target/session-smoke-cargo`，Windows binary 为该目录下的 `debug/rpi.exe`，其他平台为 `debug/rpi`。这样不会受用户全局 Cargo `build.target-dir` 影响或误测残留旧 binary；两个 Task 均可用 `SESSION_CARGO_TARGET_DIR` 覆盖构建目录。

## 离线真实进程 smoke test

前提：Python 3；通过 Task 构建时还需要 Rust、Cargo 和 Go Task。Windows 默认使用 `python`，其他平台使用 `python3`，可通过 `SESSION_PYTHON` 覆盖。测试本身只连接脚本启动的 loopback provider，不需要 API key、外部 provider、相邻仓库或用户的全局扩展。首次 Cargo 构建仍可能需要下载依赖；“离线”指 smoke 运行链路，不承诺未缓存依赖也能离线构建。

脚本隔离 HOME、配置及 session，覆盖：

1. 用户消息、模型工具调用与工具结果的完整 roundtrip。
2. 正常退出后重启，恢复已有消息历史。
3. 真实终止 CLI 进程，再重启并通过 `resume_pending` 恢复未完成运行。
4. 新建空会话不继承其他会话的历史或待恢复状态。
5. 显式加载库时检查实际加载数量和工具/命令注册；如果注册了 `todo`，检查任务添加、重启恢复及不同 session 的隔离；如果注册了 `durable`，检查 workflow/checkpoint 的重启恢复和隔离。每次检查必须匹配本轮新增工具调用及结果 ID，不能使用历史结果充当成功。

正常重启比较保留消息 ID、顺序、正文、时间戳和工具关联，只将工具结果可选的 `details: null` 与字段省略视为等价。崩溃恢复允许宿主添加 `runResume` 自定义提示，仍验证原始用户消息只出现一次。

默认同时检查 `compact-deltas` 与 `snapshot` 两种 RPC 编码。诊断目录**必须不存在**，由脚本创建；不要预先 `mkdir`，也不要复用旧诊断目录。Task 默认使用 `target/session-smoke-<UUID>`，避免多次运行或并行运行冲突：

```sh
task test:session
# 覆盖路径时，请选择一个尚不存在的目录：
task test:session SESSION_DIAGNOSTICS_DIR=target/session-smoke-local-001
# CLI 参数在 -- 后透传给脚本：
task test:session -- --encoding snapshot
```

完整脚本接口：

```text
python3 -B scripts/session-smoke.py --rpi <显式二进制路径> --diagnostics-dir <尚不存在的目录> [--extension <共享库路径> ...] [--encoding compact-deltas|snapshot|both]
```

`--encoding` 默认为 `both`。`--extension` 可重复，每个库都应是已构建的实际文件；Windows 使用 `.dll`，Linux 使用 `.so`，macOS 使用 `.dylib`。不传此选项时，不要求任何独立扩展库。显式 DLL/共享库检查仅检验这些库与本次 smoke 场景的加载及兼容性，**不是全扩展业务功能认证**；不能据此声称 provider、subagent、btw 等业务全部通过。

### 测试当前安装的 binary

不必重新编译当前仓库。可直接指定当前安装版本的实际可执行文件路径（而不是依赖脚本搜索 PATH）：

```sh
# Linux/macOS；每次运行都换一个未使用的诊断目录。
python3 -B scripts/session-smoke.py --rpi "$(command -v rpi)" --diagnostics-dir target/session-smoke-installed-001
# 同时加载两个已构建库（路径仅为示例）：
python3 -B scripts/session-smoke.py --rpi /absolute/path/to/rpi --diagnostics-dir target/session-smoke-libraries-001 --extension /absolute/path/to/libfirst.so --extension /absolute/path/to/libsecond.so
```

```powershell
# Windows PowerShell
python -B scripts/session-smoke.py --rpi (Get-Command rpi.exe).Source --diagnostics-dir target/session-smoke-installed-win-001
```

Task 默认构建目录不继承 `CARGO_TARGET_DIR`；需要共用其他目录时设置 `SESSION_CARGO_TARGET_DIR=<路径>`。也可用 `SESSION_BINARY=<实际路径>` 改测已有 binary（该任务仍执行构建；只测安装版本可直接执行脚本）。失败后保留整个诊断目录，结合进程输出、RPC 记录及 session 状态定位；不要把失败目录当成下一次运行的输出位置。分享诊断前检查其中的本地路径和消息内容。`results.json` 记录两种编码和可选状态检查的 PASS/SKIP；显式库不存在或实际加载数量不符直接失败。

**隔离不是 OS 沙箱。** `--extension` 运行的是可信 native code；脚本不加载全局配置和凭据、不发起外网请求，但恶意或自行联网的扩展仍有宿主进程权限。仅传入已知可信的库。

## 真实独立扩展库测试：PASS 与 SKIP

以下变量来自当前实际测试文件，而不是 smoke 脚本的参数：

| 测试文件 | 需要的变量 | 用途 |
| --- | --- | --- |
| `crates/rpi-cli/tests/provider_packages.rs` | `RPI_TEST_PROVIDER_LIBRARY` | 指向独立 `rpi-providers` 的实际 DLL/so/dylib，覆盖 provider 包策略、认证等真实 ABI 路径。 |
| 同上 | `RPI_TEST_PROVIDER_PACKAGE` | 指向 `rpi-providers` 包目录，完整运行该测试目标时用于 link/unlink 测试；仅设置库路径不够。 |
| `crates/rpi-cli/tests/subagent_packages.rs` 及其子模块 | `RPI_TEST_SUBAGENT_LIBRARY` | 指向独立 `rpi-subagents` 库，检查真实子代理工具、上下文、取消及生命周期。 |
| `crates/rpi-cli/tests/btw_package.rs` | `RPI_TEST_BTW_LIBRARY` | 指向独立 `rpi-btw` 库，检查真实命令 ABI、owner 绑定及父历史隔离。 |

先在对应独立包工作区构建所需库，再将上述变量设为绝对路径。按需运行：

```sh
cargo test -p rpi-cli --test provider_packages --locked --target-dir target/session-smoke-cargo -- --nocapture
cargo test -p rpi-cli --test subagent_packages --locked --target-dir target/session-smoke-cargo -- --nocapture
cargo test -p rpi-cli --test btw_package --locked --target-dir target/session-smoke-cargo -- --nocapture
```

本机 Windows 检查当前安装的三个真实扩展库，可直接运行以下独立 PowerShell 块。它会校验路径，并在结束时恢复原环境变量：

```powershell
$extensions = Join-Path $env:USERPROFILE '.rpi/agent/extensions'
$values = @{
    RPI_TEST_PROVIDER_LIBRARY = "$extensions/rpi_providers.dll"
    RPI_TEST_PROVIDER_PACKAGE = (Join-Path $PWD '../rpi-package/packages/rpi-providers')
    RPI_TEST_SUBAGENT_LIBRARY = "$extensions/rpi_subagents.dll"
    RPI_TEST_BTW_LIBRARY = "$extensions/rpi_btw.dll"
}
$previous = @{}
foreach ($key in $values.Keys) {
    if (-not (Test-Path -LiteralPath $values[$key])) { throw "缺少前置文件或目录：$($values[$key])" }
    $previous[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
}
try {
    foreach ($key in $values.Keys) {
        [Environment]::SetEnvironmentVariable($key, $values[$key], 'Process')
    }
    cargo test -p rpi-cli --test provider_packages --test subagent_packages --test btw_package --locked --target-dir target/session-smoke-cargo -- --nocapture
    if ($LASTEXITCODE -ne 0) { throw '真实扩展库回归失败' }
} finally {
    foreach ($key in $values.Keys) {
        [Environment]::SetEnvironmentVariable($key, $previous[$key], 'Process')
    }
}
```

还可检查仓库自带的真实 plugin-stub 动态库。必须先 build，并确认库存在，避免测试因未构建而跳过：

```powershell
cargo build -p plugin-stub --locked --target-dir target/session-smoke-cargo
if ($LASTEXITCODE -ne 0) { throw 'plugin-stub 构建失败' }
if (-not (Test-Path target/session-smoke-cargo/debug/plugin_stub.dll)) { throw '缺少 plugin_stub.dll' }
cargo test -p rpi-extensions --features provider-performance --test plugin_stub_smoke --locked --target-dir target/session-smoke-cargo -- --nocapture
if ($LASTEXITCODE -ne 0) { throw 'plugin-stub 真实动态库回归失败' }
```

`provider_packages.rs` 中的 `RPI_TEST_PACKAGE_RELOAD`、`RPI_TEST_PACKAGE_CWD` 是测试派生子进程的内部标记，由测试自身设置，不是日常手动运行的前提变量。

**库未构建或对应变量未设置时，必须把真实库覆盖记为 SKIP，不得记为 PASS。** 这些测试可能通过提前返回而让 Cargo 汇总显示 `ok`；请用 `--nocapture` 查看 `SKIP` 提示，并在报告中区分“测试进程成功退出”和“真实库断言确实执行”。设置了变量但文件不存在属于错误，不应伪装成成功。默认 workspace 测试通过也不能替代这些显式真实库测试。

## 内存、CPU 与多轮结论回归

### 运行入口

```sh
# 默认：release 编辑器 20 轮 + 两种 RPC 编码各 20 轮；无真实模型费用。
task test:resources
# 加长轮次，或只检查一个 RPC 编码：
task test:resources -- --rounds 100 --encoding snapshot
# 直接检查指定产物，不构建、不安装：
python3 -B scripts/resource-smoke.py --rpi /path/to/rpi --editor-probe /path/to/editor_resource_probe --diagnostics-dir target/resource-smoke-unique --profile release
```

Windows 的独立调用示例：

```powershell
$diag = 'target/resource-smoke-' + [guid]::NewGuid().ToString('N')
python -B scripts/resource-smoke.py --rpi target/resource-tests-cargo/release/rpi.exe --editor-probe target/resource-tests-cargo/release/examples/editor_resource_probe.exe --diagnostics-dir "$diag" --profile release
if ($LASTEXITCODE -ne 0) { throw "资源回归未通过：$diag" }
```

Task 显式使用 `target/resource-tests-cargo`，不依赖全局 Cargo target。可覆盖 `RESOURCE_CARGO_TARGET_DIR`、`RESOURCE_BINARY`、`RESOURCE_EDITOR_PROBE`、`RESOURCE_PYTHON`、`RESOURCE_DIAGNOSTICS_DIR`。binary/probe 覆盖只是改测已有产物，Task 仍构建源码；只测现成产物用脚本。`--profile` 是记录用户声明的构建 profile，不能自动认证外部 binary 的构建方式；性能对比应使用同 profile 的 release 产物。诊断目录必须不存在。

### 必须分开的覆盖

1. **编辑器内存路径**：`editor_resource_probe` 使用真实 `Editor`，不是模拟内存数组。保留同一个 1 MiB 折叠粘贴正文，每轮插入/退格 64 次，积累 undo 历史，验证 undo/redo 正文和渲染。压力前采集基线，不把首次复制藏入 warmup。它不打开终端，覆盖编辑器内核/渲染，**不代表全部终端后端或交互 TUI 都通过**。
2. **RPC 多轮路径**：在隔离会话连接本地 streaming provider，分别检查 compact-deltas/snapshot。默认先运行 2 轮 RPC warmup，之后重设增长基线，测量 20 轮。每轮检查当前 user/assistant 的原生 ID、completed outcome、正常 stop、累加任务的 round/sum；最后一轮要求正确的 final 结论。warmup 仍受绝对内存上限保护，但不计入增长/单轮 CPU 预算。
3. **确定性 owner 回归**：Task 同时运行 TUI Rust 测试，包括共享粘贴正文、最后 owner 释放、增量粘贴不改变旧快照，以及 probe 协议测试。单测检查数据所有权，进程压力检查实际资源，不能互相冒充。

每个测量轮次后都有独立 idle 窗口，所以能标注首次出现空转 CPU 问题的轮次。所有场景只启动和结束本次子进程，隔离 HOME/config/session/tmp，禁用全局扩展、工具、skills、prompt templates、上下文文件、自动重试和 compaction；不会操作正在运行的 rpi 或真实用户会话。

### 指标、预算与结论边界

| 指标/预算 | 默认值或口径 |
| --- | --- |
| Windows 主内存指标 | PrivateUsage（私有提交字节），另报 Working Set、峰值 resident/commit |
| Linux 主内存指标 | RSS、峰值 RSS，取自 `/proc`；不与 Windows Private Bytes 混为同一含义 |
| CPU | 进程 user+kernel 秒；百分比按单核口径，100% 为一个逻辑核，可超过 100% |
| 绝对主内存上限 | 512 MiB，`--max-memory-mib` |
| RPC 增长预算 | warmup 后 96 MiB，`--max-rpc-growth-mib` |
| 编辑器增长预算 | 压力前基线后 32 MiB，`--max-editor-growth-mib` |
| 每轮活动 CPU | 5 秒，`--max-round-cpu-seconds`；不是墙钟/网络延迟 |
| idle CPU | 单核 25%，`--max-idle-cpu-percent` |
| idle 窗口 / 采样周期 | 1 秒 / 200ms，`--idle-seconds` / `--sample-interval` |

以上预算针对受控测试负载，不是任意真实会话的 SLA。采样或指标缺失、时间倒退、进程崩溃、超时、超预算、回复截断或结论错误均不能算 PASS。退出码：0=PASS、1=FAIL、2=参数错误、77=不支持平台的 SKIP。目前 Windows/Linux 有实际采样后端；其他平台不伪造零值。

诊断文件：

- `results.json`：版本/实际 binary 和 probe 摘要、profile、workload/预算、场景汇总、`firstProblemRound`/`firstProblem`、`finalConclusion`。仓库 HEAD/dirty 元数据描述控制器所在仓库，**不是外部 binary 的构建来源证明**。
- `metrics.jsonl`：各阶段及每轮活动/idle 原始指标、CPU delta、内存增长、经过验证的答案/entry IDs、可用的 usage 和累计观察到的消息 JSON 字节。
- `<case>/process.jsonl`：有大小上限并脱敏的进程输入/输出；`sessions/` 为本次合成历史。

`firstProblem` 给出类别、phase、轮次、实际值和预算。startup/prepare/warmup 失败按阶段报告，不伪造一个已测量轮次。功能失败归为 semantic/protocol 等类别，不直接称为内存泄漏。短测 PASS 只说明已执行轮次满足预算；内存增长可能来自合法历史，reset 后 RSS 未立即下降可能是 allocator 保留 arena，均不能单凭一个数推断泄漏。累加任务只验证多轮完成性和小型确定性结论，不认证一般推理质量。普通离线结果中 live 为 **SKIP**，不得宣称真实模型已验证。

### 可选真实模型：显式启用，默认不执行

```powershell
# 凭据由你在当前进程环境安全设置；不要把密钥写到命令行或测试文件。
# 例中仅写环境变量名 RESOURCE_TEST_API_KEY，不展示其值。
task test:resources:live -- --api openai-completions --base-url https://api.openai.com/v1 --model YOUR_MODEL --api-key-env RESOURCE_TEST_API_KEY --rounds 5 --max-output-tokens 256 --max-requests 5
```

支持 `openai-completions`、`openai-responses`、`anthropic-messages` 参数；endpoint 必须使用 HTTPS（仅 loopback 允许 HTTP），不得包含 userinfo、query 或 fragment。需显式提供 model、endpoint 和已设置的凭据环境变量名；不读取默认 auth，不加载原会话。为避免覆盖隔离环境，`RPI_`、`XDG_` 前缀及 HOME/PATH 等保留变量不能作为凭据变量。

live 默认 snapshot、5 轮、无 warmup、每轮最多 256 输出 tokens、请求超时 60 秒，跨编码/含 warmup 的总请求计划不得超过默认 `--max-requests 5`。两种编码、额外 warmup 或更长测试需显式调整请求预算。密钥仅通过指定环境变量传入，临时 models.json 保存 `$ENV_NAME` 引用，不保存值；日志脱敏原值及常见编码。输出/token/call 上限不是未知价格下的美元费用保证。模型忽略 JSON 格式、输出被截断或结论错误也会失败，不会为凑 PASS 自动追加请求。

默认离线回归不覆盖真实模型。loopback OpenAI Completions fixture 只用于验证配置、限制、凭据隔离和脱敏，不认证真实服务或 Responses/Anthropic 的真实集成。普通 `task test:resources`、Python 控制器测试和 CI 不会调用真实模型。

## 海量流式记录的历史冷启动回归

固定场景名为 `historical-frame-heavy-v1`，只生成合成历史，不读取或复制真实
session。每次包含 **701,000 个 append frames（每个 delta 为 256 bytes）、21,733 条消息 entries**，每 4,000 个
append（含最后一批）写 commit；分别生成已声明 progress 的现代格式与首次需升级的
旧格式，不依赖预建索引。默认脚本参数由控制器单测锁定。

```sh
task test:cold-start
# 只跑快速控制器单测，无需构建 native；真实 RPC 小测默认明确 SKIP：
python3 -B scripts/test_resource_cold_start.py
```

完整 Task 构建 release CLI，运行控制器/真实 RPC 小测，然后按格式各运行 5 次独立
进程。默认诊断在仓库外的系统 temp；需要固定到仓库外位置时，设置
`RESOURCE_CARGO_TARGET_DIR` 和 `COLD_START_DIAGNOSTICS_DIR`。只检查已有产物可直接
运行 `scripts/resource-cold-start.py --candidate <binary> --output <new-outside-directory>`，
`--baseline` 可选；命令完整参数见 [冷启动诊断与门禁](../debugging/session-cold-start.md)。

每次必须检查 ready、最后 40 条消息的数量/顺序/内容、`pendingResume=false`、
`activeOperation=null`、两条 queue 为空，以及正常 stop 和 owned-process 清理。
耗时缺失、非有限、负数、功能错误、轮次不完整均不能 PASS；清理失败停止后续观测。

| 入口 | Modern ready 中位数 / 最大值 | Legacy ready 中位数 / 最大值 | 消息查询最大值 | State 查询最大值 |
| --- | --- | --- | --- | --- |
| 本地 Task（每格式 5 次） | 2000 / 3000 ms | 3500 / 5000 ms | 100 ms | 1500 ms |
| CI `resources`（每格式 3 次） | 4000 / 8000 ms | 6000 / 10000 ms | 250 ms | 2000 ms |

旧格式含锁内升级、额外摘要校验及复制；单独的 legacy 预算**不等于默认两秒目标**，
不能用另一组预算冒充两秒 PASS。共享 runner 的宽松预算必须结合远端结果校准；配置接入不等于已经在
GitHub runner 实测。所有结果为 fresh process，**不清 OS 磁盘缓存，也不是 Web 首屏 SLA**。

每种格式保留 `results.json`（含 gate、实际 binary hash 与所有观测）、`reproduce.json`
（参数和脚本 hash）及各次 `report.json`/协议日志。只删除本次生成的 history、companion
和 lock，不删除未知文件。CI 在成功/失败时均上传 `historical-cold-start-diagnostics`，
使用文件白名单，避免清理失败时将巨大合成历史或配置上传为 artifact。诊断、测量结果及原始日志不提交到 Git；分享前检查内容和路径。隔离配置不是 OS 网络/文件访问沙箱，必须使用可信的待测二进制。

## CI

`.github/workflows/ci.yml` 的现有 Ubuntu `test` job 在 workspace 测试后显式构建 CLI，再执行：

```sh
cargo build -p rpi-cli --bin rpi --locked --target-dir target
python3 -B scripts/test_session_smoke.py
python3 -B scripts/session-smoke.py --rpi target/debug/rpi --diagnostics-dir "$RUNNER_TEMP/$SESSION_SMOKE_DIAGNOSTICS_NAME"
```

session 的 basename 包含 run_id/run_attempt，输出在 runner.temp 而不是 Cargo 缓存中的 target；失败时从同一路径按白名单上传 `session-smoke-diagnostics`（不含原始 sessions、snapshot、provider 请求或配置文件），保留拒绝覆盖旧目录的保护。CI 不传独立扩展，因此该结果不代表全扩展业务认证。

Ubuntu `Offline resource regression`（`resources`）job 共用同一次构建的 release CLI/probe，依次检查：

1. 资源 smoke、冷启动、资源比较、fake PTY 四组 Python 控制器，以及 Rust editor invariants；设置 `RPI_COLD_START_BINARY` 以确保冷启动的小型真实 RPC 测试不是 SKIP。
2. 外部工具 RPC 两种编码各 3 个场景，含 roundtrip/abort、外部工具 pending 时 EOF、仅模型运行时 EOF，使用 loopback fixture；成功收回 owned 进程树/provider 后删除生成的 home/config/session/tmp，清理失败停止后续场景并保留数据。失败仅按白名单上传 `external-tools-smoke-diagnostics`，不含生成的 auth/config/session 文件。
3. 编辑器与两种 RPC 编码各 5 个资源测量轮次；失败上传 `resource-smoke-diagnostics`。
4. modern/legacy 各 3 次完整海量帧冷启动；成功/失败上传白名单 `historical-cold-start-diagnostics`。

诊断均在 runner.temp，名称带 run/attempt，避免 Cargo cache 复用旧输出。`test` job 另显式运行官网校验器 `VerifierTests`，真实 Caddy 集成不包含在内。release/preview 的发布准备依赖 `test` 与 `resources` 等门禁；仓库 branch protection 的 required checks 仍需在 GitHub 设置中另行确认，工作流不能替代该设置。

这些新增步骤不需要 secrets、不启用 live、不下载或安装独立业务扩展。本地验证不能冒充远端 CI 结果；普通 CI 的合成模型、fake PTY 与仓库 plugin-stub PASS，也不能推广为真实模型、真实终端或所有独立扩展 PASS。
