# 会话与恢复测试

## 下次直接运行

从 `pi-rust` 仓库根目录执行。前提是 Python 3、Rust/Cargo 和 Go Task 已安装。下面是本机 Windows PowerShell 可直接复制的命令；**任一步失败就先保留日志排查，不要把后续部分成功当成整轮通过。** 测试不会自动安装新 binary、替换全局 DLL 或重启服务。

### 1. 日常回归：先跑这两项

```powershell
task test:session
if ($LASTEXITCODE -ne 0) { throw 'Session 冒烟失败，请检查输出中的诊断目录' }
task test:recovery
if ($LASTEXITCODE -ne 0) { throw '恢复回归失败' }
```

第一项自动构建当前源码、运行 Python 控制器测试，再检查两种 RPC 编码下的消息/工具结果、重启恢复、真实 kill 后续跑和 session 隔离；每次自动创建唯一诊断目录。第二项运行底层恢复回归。**这两项默认不加载全局扩展，不能替代下面的扩展检查。**

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

## CI

`.github/workflows/ci.yml` 的现有 Ubuntu `test` job 在 workspace 测试后显式构建 CLI，再执行：

```sh
cargo build -p rpi-cli --bin rpi --locked --target-dir target
python3 -B scripts/test_session_smoke.py
python3 -B scripts/session-smoke.py --rpi target/debug/rpi --diagnostics-dir target/session-smoke-ci
```

CI 不传 `--extension`，因此不依赖相邻的独立包仓库或全局安装。失败时上传 `target/session-smoke-ci` 为 `session-smoke-diagnostics` artifact（包含隔离 HOME 等隐藏文件）；若尚未生成目录则不因缺少 artifact 再报错。CI 的该 smoke 结果只证明本次二进制的离线会话链路，不代表全部扩展业务得到认证。
