# 会话、恢复与资源测试

## 下次直接运行

从 `pi-rust` 仓库根目录执行。前提是 Python 3、Rust/Cargo 和 Go Task 已安装。下面是本机 Windows PowerShell 可直接复制的命令；**任一步失败就先保留日志排查，不要把后续部分成功当成整轮通过。** 测试不会自动安装新 binary、替换全局 DLL 或重启服务。

### 1. 日常回归：会话、恢复和资源

```powershell
task test:session
if ($LASTEXITCODE -ne 0) { throw 'Session 冒烟失败，请检查输出中的诊断目录' }
task test:recovery
if ($LASTEXITCODE -ne 0) { throw '恢复回归失败' }
task test:resources
if ($LASTEXITCODE -ne 0) { throw '资源回归未通过；请区分 FAIL 与平台 SKIP 并保留诊断' }
```

第一项自动构建当前源码、运行 Python 控制器测试，再检查两种 RPC 编码下的消息/工具结果、重启恢复、真实 kill 后续跑和 session 隔离；每次自动创建唯一诊断目录。第二项运行底层恢复回归。第三项构建 release CLI 和实际编辑器压力进程，检查大粘贴/撤销历史及两种 RPC 编码的多轮内存、CPU、完成性和最终结论。**三项默认都不加载全局扩展，不调用真实模型，不能替代下面的扩展检查。**

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

本次真实模型覆盖明确为 SKIP。live 通路只用 loopback OpenAI Completions 假 endpoint 验证配置、限制、凭据隔离和脱敏；真实服务以及 Responses/Anthropic 真实集成未执行。普通 `task test:resources`、Python 测试和 CI 不会调用真实模型。

## CI

`.github/workflows/ci.yml` 的现有 Ubuntu `test` job 在 workspace 测试后显式构建 CLI，再执行：

```sh
cargo build -p rpi-cli --bin rpi --locked --target-dir target
python3 -B scripts/test_session_smoke.py
python3 -B scripts/session-smoke.py --rpi target/debug/rpi --diagnostics-dir "$RUNNER_TEMP/$SESSION_SMOKE_DIAGNOSTICS_NAME"
```

session 的 basename 包含 run_id/run_attempt，输出在 runner.temp 而不是 Cargo 缓存中的 target；失败从同一路径上传 `session-smoke-diagnostics`，保留拒绝覆盖旧目录的保护。CI 不传独立扩展，因此该结果不代表全扩展业务认证。

新增 Ubuntu `Offline resource regression` job 独立构建 release CLI/probe，运行 Python 控制器及 Rust editor 测试，再执行编辑器和两种 RPC 编码各 5 个测量轮次。资源诊断同样使用 runner.temp 下 run/attempt 唯一目录，失败上传 `resource-smoke-diagnostics`。release/preview 的发布准备依赖该门禁。无 secrets、无 live 模式；不能将 CI 的合成模型 PASS 当作真实模型 PASS。
