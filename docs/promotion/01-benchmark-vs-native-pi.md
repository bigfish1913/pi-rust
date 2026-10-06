# rpi 与原生 pi：一次返工三次的性能基准

> Windows 11 (26200) / x86_64，rustc 1.97.1 `--release`，Node v25.9.0。rpi 0.3.6，pi 0.87.1。复现命令在文末。

rpi 是 TypeScript 版 pi 的 Rust 移植。移植项目最常见的宣传口径是"Rust 更快"，但快多少、快在哪，很少有人给数字。我把两个工具装在同一台机器上，用同一套口径测了启动、内存和安装体积。

## 怎么测才公平

两个工具都是终端里的 coding agent，一个原生二进制，一个 Node 脚本，很容易测成"比谁启动快"这种没信息量的对比。所以口径先定死：

| 约束 | 做法 | 为什么 |
| --- | --- | --- |
| 同一终点 | 双方都通过同一套 JSONL 命令通道，测"发出 `get_state` 到收到响应" | 这才是"agent 可用了"，不是"参数解析完了" |
| 冷且隔离 | 每轮用全新的空配置目录：pi 用 `PI_CODING_AGENT_DIR`，rpi 用 `RPI_CODING_AGENT_DIR` | 两边变量名不同，只设一个会漏掉另一边 |
| 离线 | pi 用 `PI_OFFLINE=1`，rpi 用 `RPI_OFFLINE=1` | 测运行时开销，不是网络 |
| 占位凭据 | 给一个假 `ANTHROPIC_API_KEY` | rpi 在空目录下没凭据就不建会话，离线也不会真的用 |
| 中性目录 | 双方都在空临时目录里启动 | 避免谁去扫了这个大仓库 |
| 不采样 | 计时窗口内不做任何 RSS 采样 | 否则采样本身会拖慢被测进程 |
| 无 shell | 绕过 npm 的 `.bin/pi` shim，直接 `node cli.js` | 否则把 `cmd.exe` 的启动算进 pi 的时间里 |

"冷启动"这个终点不是我发明的。原生 pi 自己的 `scripts/profile-coding-agent-node.mjs` 就是这么定义"启动完成"的。用对手的定义测对手，比自选一个有利口径更有说服力。

## 结果

环境：Windows 11 (26200) / x86_64、rustc 1.97.1 `--release`、Node v25.9.0、rpi 0.3.6、pi 0.87.1。3 轮预热后测 11 轮，取中位数。

| 指标 | rpi（Rust） | pi（TypeScript） | 差距 |
| --- | --: | --: | --: |
| `--version` | 17.5 ms | 169.5 ms | 9.7× |
| 冷启动 | 17.7 ms | 189.5 ms | 10.7× |
| 常驻内存 | 12.0 MiB | 91.5 MiB | 7.6× |
| 安装体积 | 21.8 MiB（单二进制） | 约 385 MiB（另加 91 MiB Node） | 约 18-22× |

原始数据（单位 ms）：

```
rpi  --version  17.8 19.4 17.4 16.9 19.8 16.8 17.5 18.9 17.2 17.0 19.5   median 17.5
rpi  cold start 18.3 22.2 16.6 17.7 17.9 17.0 17.7 18.6 16.6 17.3 17.5   median 17.7
pi   --version 170.7 170.6 171.8 168.7 169.5 168.9 169.2 171.9 166.7 172.0 167.0   median 169.5
pi   cold start 188.4 190.4 189.0 189.9 189.5 190.8 189.1 189.7 190.5 189.3 187.4   median 189.5

rpi  rss at ready  12.0 12.0 12.0  MiB
pi   rss at ready  91.5 91.3 91.6  MiB
```

## 时间花在哪

时间花在哪，比倍数本身有意思。

rpi 的 `--version` 是 17.5 ms，冷启动是 17.7 ms，几乎一样。config、会话、harness、6 个内置工具的初始化加起来可以忽略，真正花钱的是把进程拉起来。

pi 的两个数差约 20 ms：`--version` 169.5 ms，冷启动 189.5 ms。那 20 ms 是它加载 agent 本身的成本，剩下的全被 Node 启动吃掉，所以在这个维度上它没什么可优化的。

## 这个数字我测错了三次

值得写下来，因为基准太容易测错，而且方向完全不可预测。

第一次是 RSS 采样污染了计时窗口。我在 Windows 上每 10 ms 起一个 `tasklist` 去采样对方内存，这个进程本身的开销把两个工具都拖慢了，对快的那个影响更大，正好把真实差距抹平。现在内存单独一轮测，计时窗口内不做任何采样。

第二次是 npm 的 shim 白送对方一个 shell。npm 装的 `pi` 是 `.bin/pi`，在 Windows 上要走 `cmd.exe`，直接跑会把 `cmd.exe` 的启动算进 pi 的时间里。现在解析 shim 指向的 JS 入口，直接用 `node` 跑。

第三次最严重，rpi 根本没被隔离。我给两边都设了 `PI_CODING_AGENT_DIR` 和 `PI_OFFLINE`，那是原生 pi 的变量名；rpi 用的是 `RPI_CODING_AGENT_DIR` 和 `RPI_OFFLINE`。结果是 pi 老老实实用空目录冷启动，rpi 忽略这些变量，直接读本机真实的 `~/.rpi/agent`，把我装在里面的 25 个插件 DLL 全加载了（`rpi_voice.dll` 19 MB、`rpi_im_message.dll` 7 MB，还有 `mcp_adapter`、`langfuse`、`codegraph` 等等）。证据在 `get_state` 的回包里：那个配置下 rpi 报告 31 个工具，真正隔离后只有 6 个内置工具。那"83 ms 初始化"几乎全是加载我自己的插件，不是 rpi 的核心开销。修好隔离后，冷启动从 101 ms 掉到 10 ms。

但这 10 ms 也没算全。修好隔离后我得到一个比 `--version` 还小的冷启动，这不合理。查脚本发现计时器是这么起的：

```js
const child = spawn(...);              // 进程已经在这儿被创建
const startedAt = performance.now();   // 秒表却在这之后才起
```

而 `--version` 的计时器在 `spawnSync` 之前。也就是说冷启动漏掉了 `spawn()` 内部的进程创建开销。实测在 Windows 上，Node 的 `spawn()` 调用本身就要 7.3 ms。补回去，rpi 的冷启动就从 10.1 ms 变成 17.7 ms，和 `--version` 对齐了。

顺带说清冷启动到底量的是什么：从 `spawn()` 之前开始计时，到父进程从子进程 stdout 解析出 `get_state` 的响应为止。中间是进程创建、参数解析、config、模型目录、工具注册、扩展和插件发现与加载、harness 和会话构建、输出 `{"type":"ready"}`、读入并回答 `get_state`。进程退出和清理不算（测完立即 kill），LLM 和工具调用也不算。

顺便一提，第三次那个数字也说明插件加载是有成本的。你装一堆插件，启动就会变慢，这部分是"你装了什么"，不是"运行时本身多快"。

## 没测的东西

- LLM 延迟和工具循环吞吐。这取决于模型和网络，跟运行时无关。
- 长会话的内存增长。这里只测了"刚就绪"的常驻内存。
- TUI 帧开销。两边都做过缓存优化，是另一类问题，需要单独基准。

## 复现

```bash
git clone https://github.com/bigfish1913/pi-rust && cd pi-rust
cargo build -p rpi-cli --release

npm install --prefix .bench @earendil-works/pi-coding-agent
node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi --runs 11 --warmup 3 \
  --json .bench/results.json
```

换一台机器重跑会得到不同的绝对值，比值也请以你自己机器上的结果为准，别直接引用这里的毫秒数。完整方法和原始数据在 `docs/performance/benchmark-vs-pi.md`。

## 项目

rpi 是 Rust 原生、library-first 的 coding-agent runtime，九个 crate 同版本发布：`rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`，另有 `rpi-plugin-sdk`（稳定 `#[repr(C)]` ABI）与 `rpi-extensions`（宿主加载器）。MIT 许可。

- GitHub：https://github.com/bigfish1913/pi-rust
- 官网：https://rpi.laofu.online/
- 完整数据与方法：https://github.com/bigfish1913/pi-rust/blob/main/docs/performance/benchmark-vs-pi.md
- 免 Rust 工具链安装：`curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh`

---

## English version

### rpi vs the TypeScript original: a measured startup and memory benchmark

rpi is a Rust port of the TypeScript `pi` coding agent. Rewrites usually claim "Rust is faster" without numbers, so I installed both on the same machine and measured startup, memory, and install size the same way.

### How it is measured

Both are terminal coding agents, one a native binary, one a Node script, which makes it easy to produce a meaningless "who starts faster" number. The rules:

| Rule | What | Why |
| --- | --- | --- |
| Same endpoint | Both answer a `get_state` request over the same JSONL command channel | That is "the agent is usable", not "args parsed" |
| Cold and isolated | A fresh empty config dir per run: pi via `PI_CODING_AGENT_DIR`, rpi via `RPI_CODING_AGENT_DIR` | The two tools use different variable names; setting one misses the other |
| Offline | pi via `PI_OFFLINE=1`, rpi via `RPI_OFFLINE=1` | Measure runtime cost, not network |
| Placeholder credential | A dummy `ANTHROPIC_API_KEY` | rpi will not build a session in an empty dir without one; offline, it is never sent |
| Neutral cwd | A fresh empty temp directory for both | Neither scans the repository on startup |
| No sampling | No RSS sampling inside the timing window | Sampling slows the process being measured |
| No shell | Resolve npm's `.bin/pi` shim to the JS entrypoint, run under `node` | Otherwise `cmd.exe` startup lands in pi's time |

"Cold start" is native pi's own definition of startup completion (its `scripts/profile-coding-agent-node.mjs`), not a metric chosen here. Using the competitor's definition is fairer than picking a flattering one.

### Results

Windows 11 (26200) / x86_64, rustc 1.97.1 `--release`, Node v25.9.0, rpi 0.3.6, pi 0.87.1. Three warmup runs discarded, 11 measured, medians reported.

| Metric | rpi (Rust) | pi (TypeScript) | Difference |
| --- | --: | --: | --: |
| `--version` | 17.5 ms | 169.5 ms | 9.7× |
| Cold start | 17.7 ms | 189.5 ms | 10.7× |
| RSS at ready | 12.0 MiB | 91.5 MiB | 7.6× |
| Install footprint | 21.8 MiB (one binary) | ~385 MiB (+ ~91 MiB Node) | ~18-22× |

Raw, in milliseconds:

```
rpi  --version  17.8 19.4 17.4 16.9 19.8 16.8 17.5 18.9 17.2 17.0 19.5   median 17.5
rpi  cold start 18.3 22.2 16.6 17.7 17.9 17.0 17.7 18.6 16.6 17.3 17.5   median 17.7
pi   --version 170.7 170.6 171.8 168.7 169.5 168.9 169.2 171.9 166.7 172.0 167.0   median 169.5
pi   cold start 188.4 190.4 189.0 189.9 189.5 190.8 189.1 189.7 190.5 189.3 187.4   median 189.5

rpi  rss at ready  12.0 12.0 12.0  MiB
pi   rss at ready  91.5 91.3 91.6  MiB
```

### Where the time goes

rpi's `--version` (17.5 ms) and cold start (17.7 ms) are effectively equal. Config, session, harness, and the six built-in tools add nothing measurable; the cost is process creation. Pi's two numbers differ by about 20 ms (`--version` 169.5, cold start 189.5); that 20 ms is its own agent initialisation and the rest is Node boot, so there is little left to optimise there.

### I got this number wrong three times

Worth writing down, because a benchmark is easy to get wrong and the error can point either way.

The first bug was RSS sampling inside the timing window. Spawning `tasklist` every 10 ms to read the other process's memory loaded the CPU and slowed both tools, and slowed the faster one more, flattening the real gap. Memory is now measured in a separate phase.

The second was npm's `.bin/pi` shim, which needs `cmd.exe` on Windows and would have charged that startup to pi. The harness now resolves the shim to its JS entrypoint and runs `node` directly.

The third was the worst: rpi was not actually isolated. I set `PI_CODING_AGENT_DIR` and `PI_OFFLINE`, which are pi's variables; rpi reads `RPI_CODING_AGENT_DIR` and `RPI_OFFLINE`. So pi ran against an empty config dir while rpi read the real `~/.rpi/agent` and loaded the 25 plugin DLLs installed there (`rpi_voice.dll` at 19 MB, `rpi_im_message.dll` at 7 MB, plus `mcp_adapter`, `langfuse`, `codegraph`). The proof is in the `get_state` reply: that run reported 31 tools; properly isolated, 6 built-ins. The "83 ms of initialisation" was almost all my own plugins. Fixing isolation took cold start from 101 ms to 10 ms.

But 10 ms was not the whole story either. A cold start smaller than `--version` is impossible, and that is what I got. The stopwatch was the problem:

```js
const child = spawn(...);              // the process already exists here
const startedAt = performance.now();   // the clock starts after it
```

`measureVersion` timed from before `spawnSync`, so the two were not comparable. Node's `spawn()` does a synchronous chunk of process creation, measured at 7.3 ms for `rpi.exe` on Windows. Adding it back took rpi's cold start from 10.1 ms to 17.7 ms, in line with `--version`.

For the record, cold start means: clock starts before `spawn()`, stops when the parent parses the `get_state` response from the child's stdout. In between: process creation, arg parsing, config, model catalog, tool registry, extension and plugin discovery/loading, harness and session build, emitting `{"type":"ready"}`, reading and answering `get_state`. Process teardown is excluded (the child is killed right after), and so is any LLM or tool work.

That third number also says something about plugins: loading them costs time, so a big plugin set makes startup slower. That is about what you installed, not about the runtime.

### What is not measured

- LLM latency and tool-loop throughput. Those depend on the model and network, not the runtime.
- Memory growth over a long session. Only the "just ready" RSS is measured here.
- TUI frame cost. Both projects cache transcript rendering; that needs its own benchmark.

### Reproduce

```bash
git clone https://github.com/bigfish1913/pi-rust && cd pi-rust
cargo build -p rpi-cli --release

npm install --prefix .bench @earendil-works/pi-coding-agent
node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi --runs 11 --warmup 3 \
  --json .bench/results.json
```

Absolute milliseconds are machine-specific. Re-run on your own hardware and compare ratios, not the numbers above. The full method and raw data are in `docs/performance/benchmark-vs-pi.md`.
