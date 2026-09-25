# 实测：Rust 重写的 coding agent 比原生 TypeScript 版快多少？

> 投放渠道：掘金 / OSCHINA / RustCC（跟帖或独立成文）/ 知乎 / Hacker News
> 数据日期：2026-09-25 · 复现命令见文末
> 所有数字都是同一台机器上实测，未做任何外推

## 起因

rpi 是 TypeScript 版 pi SDK 的 Rust 移植。这类"重写"项目最常见的宣传口径是"Rust 更快"，
但快多少、快在哪里，通常没人给数字。所以我把两个工具装在同一个目录下，用同一套协议测了一遍。

先说结论：**启动快 9.7 倍，内存小 4.9 倍，但"到能用"只快 1.7 倍——而且瓶颈不在我以为的地方。**

## 怎么测才公平

两个工具都是"终端里的 coding agent"，但一个是原生二进制，一个是 Node 脚本，很容易测成
"比谁启动得快"这种没有信息量的对比。所以约束如下：

| 约束 | 做法 | 为什么 |
| --- | --- | --- |
| 同一个终点 | 双方都通过同一个 JSONL RPC 协议，测"发出 `get_state` 到收到响应" | 这才是"agent 可用了"，而不是"参数解析完了" |
| 冷且隔离 | 每轮用全新的空 `PI_CODING_AGENT_DIR` | 不带入任何已有会话、扩展、provider 配置 |
| 离线 | `PI_OFFLINE=1`，清空 provider 凭据 | 测的是运行时开销，不是网络 |
| 中性目录 | 双方都在一个空临时目录里启动 | 避免谁去扫了大仓库 |
| 不采样 | 计时窗口内不做任何 RSS 采样 | 见下面"我自己踩的坑" |
| 无 shell | 绕过 npm 的 `.bin/pi` shim，直接 `node cli.js` | 否则会把 `cmd.exe` 的启动算进 pi 的时间 |

"RPC ready"这个终点不是我发明的——原生 pi 自己的
`scripts/profile-coding-agent-node.mjs` 就是这么定义启动完成的。用对手的定义测对手，
比自选一个有利口径更有说服力。

## 结果

环境：Windows 11 (26200) / x86_64、rustc 1.97.1 `--release`、Node v25.9.0、
rpi 0.3.1、pi 0.87.1。2 轮预热后测 9 轮，取中位数。

| 指标 | rpi（Rust） | pi（TypeScript） | 差距 |
| --- | --: | --: | --- |
| `--version` | **17.9 ms** | 172.9 ms | **9.7×** |
| 到可用 agent（RPC ready） | **103.6 ms** | 176.4 ms | **1.7×** |
| 常驻内存 | **18.6 MiB** | 91.6 MiB | **4.9×** |
| 安装体积 | **23.9 MiB** | 约 513 MiB | **约 21×** |

原始数据：

```
rpi  --version  19.8 17.8 20.0 17.8 17.8 19.4 17.7 17.9 19.6   median 17.9 ms
rpi  rpc ready 111.2 103.6 105.1 100.7 100.9 106.4 109.0 103.5 103.6   median 103.6 ms
pi   --version 172.8 171.1 181.2 173.6 172.9 175.0 173.2 171.6 170.6   median 172.9 ms
pi   rpc ready 174.7 174.0 172.7 181.1 174.6 187.4 176.4 176.9 180.4   median 176.4 ms
```

## 真正有意思的部分

**pi 的开销几乎全在 Node 启动。** 它的 `--version` 是 172.9 ms，完整 agent 是 176.4 ms——
初始化 agent 只比启动运行时多了约 3 ms。也就是说这个项目在这个维度上已经没什么可优化的了，
地板就是运行时本身的启动成本。

**rpi 的开销大部分是它自己的初始化。** 进程启动 17.9 ms，但到可用 agent 要 103.6 ms。
**中间 86 ms（占 83%）是 config、session store、工具注册表、扩展/资源发现这些初始化工作。**

这个结论直接改变了优化方向：继续把二进制做小、或者说"Rust 启动快"已经不再是瓶颈——
**真正的瓶颈是那 86 ms 的初始化，得靠惰性化去砍**。如果这段能砍一半，
rpi 的 time-to-usable-agent 会落到 60 ms 附近，差距从 1.7× 拉到约 3×。

我本来以为 Rust 重写的优势会线性体现在所有指标上，实测下来完全不是——这个"反直觉但不难查"
的结论，比任何一个倍数都更有价值。

## 我自己踩的两个坑（顺便说明为什么这些数字可信）

第一版测出来两个工具都是 220 ms 左右，看起来"差不多"。查下去发现是我自己的锅：

1. **RSS 采样污染了计时窗口。** 我在 Windows 上每 10 ms 起一个 `tasklist` 去采样对方内存，
   这个进程本身的开销把两个工具都拖慢了，而且对快的那个影响更大——正好抹平了真实差距。
   现在内存单独一轮测，计时窗口内不做任何采样。
2. **npm 的 shim 会白送对方一个 shell。** npm 装的 `pi` 是 `.bin/pi`，在 Windows 上要走
   `cmd.exe`。直接跑会把 `cmd.exe` 的启动算进 pi 的时间里。现在解析 shim 指向的 JS 入口，
   直接用 `node` 跑。

基准脚本和完整方法在仓库里，未测的范围也写在文档里：**没有测 LLM 延迟、工具循环吞吐、
长会话内存增长、TUI 帧开销**。这些是另一类问题，不该被这张表暗示成"也快"。

## 复现

```bash
git clone https://github.com/bigfish1913/pi-rust && cd pi-rust
cargo build -p rpi-cli --release

npm install --prefix .bench @earendil-works/pi-coding-agent
node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi --runs 9 --warmup 2
```

换一台机器重跑会得到不同的绝对值——所以比值也请以你自己机器上的结果为准。

## 项目

rpi 是 Rust 原生、library-first 的 coding-agent runtime，九个 crate 同版本发布：
`rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`，
另有 `rpi-plugin-sdk`（稳定 `#[repr(C)]` ABI）与 `rpi-extensions`（宿主加载器）。
MIT 许可。

- GitHub：https://github.com/bigfish1913/pi-rust
- 官网：https://rpi.laofu.online/
- 完整数据与方法：https://github.com/bigfish1913/pi-rust/blob/main/docs/performance-vs-pi.md
- 免 Rust 工具链安装：
  `curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh`

---

## English version

### How much faster is a Rust rewrite? I measured it against the TypeScript original

rpi is a Rust port of the TypeScript `pi` SDK. Rewrite projects usually claim
"Rust is faster" without numbers, so I installed both tools side by side and
measured them the same way.

**Headline: 9.7× faster to start, 4.9× less memory — but only 1.7× faster to a
usable agent, and the bottleneck is not where I expected.**

To make it a fair fight:

- **Same endpoint.** Both are asked for `get_state` over the same JSONL RPC
  channel. That is native pi's own definition of "startup complete" from
  `scripts/profile-coding-agent-node.mjs`, so neither side got to pick a
  flattering metric.
- **Cold and isolated.** A fresh empty `PI_CODING_AGENT_DIR` per run, both
  offline, credentials cleared, a neutral working directory.
- **No shell in the path.** npm installs `pi` behind a `.bin/pi` shim, which
  needs `cmd.exe` on Windows; the harness resolves the JS entrypoint instead.
- **No sampling inside the timing window** (see below).

| Metric | rpi (Rust) | pi (TypeScript) | Difference |
| --- | --: | --: | --- |
| `--version` | **17.9 ms** | 172.9 ms | **9.7×** |
| RPC ready | **103.6 ms** | 176.4 ms | **1.7×** |
| RSS at ready | **18.6 MiB** | 91.6 MiB | **4.9×** |
| Install footprint | **23.9 MiB** | ~513 MiB | **~21×** |

**Where the time actually goes.** Pi's cost is almost entirely Node boot: its
`--version` and its fully-initialised agent differ by ~3 ms. Meanwhile rpi's
process start is 17.9 ms but reaching a usable agent takes 103.6 ms — so **~86 ms
(83%) is rpi's own initialisation**, not process or loader overhead. That flips
the optimisation target: making the binary smaller is no longer interesting;
lazy initialisation is.

Two mistakes in my first harness, both fixed before publishing the numbers: a
10 ms RSS sampler spawned `tasklist` inside the timing window and flattened the
difference to ~220 ms for both, and the npm shim would have added `cmd.exe`
startup to pi's time.

Not measured, stated plainly: LLM latency, tool-loop throughput, memory growth
over a long session, TUI frame cost.

Reproduce:

```bash
node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi --runs 9 --warmup 2
```
