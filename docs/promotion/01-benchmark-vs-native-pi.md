# Rust 重写的 coding agent，和原生 TypeScript 版到底差在哪？

> 投放渠道：掘金 / OSCHINA / RustCC（跟帖或独立成文）/ 知乎 / Hacker News
> 数据日期：2026-09-30 · rpi 0.3.6 vs pi 0.87.1 · 11 轮实测（3 轮预热）· 复现命令见文末
> 所有数字同一台机器实测，不做外推；所有结构结论都能在当前源码里指到位置

## 起因

rpi 是 TypeScript 版 pi 的 Rust 移植。这类"重写"项目最常见的宣传口径是"Rust 更快"，
但**快多少、快在哪、以及除了快还差在哪**，通常没人讲清楚。

所以我把两个工具装在同一台机器上，用同一套协议跑了一遍，同时把两边的代码结构也摊开对比。
先说结论：

- 结构上：不是"同一个东西换个语言"，而是**同一个 agent 的两种宿主形态**——
  一边是 Node 进程里的 SDK + CLI，一边是单文件二进制 + 可嵌入的 crate 库。
- 性能上：**启动快 9.7 倍，冷启动快 10.7 倍，内存小 7.6 倍**。
  但这组数字我返工了三次：先是**两边没真正隔离**（rpi 一直在加载我本机的 25 个插件），
  然后是**秒表起晚了**（冷启动计时漏掉了 `spawn()` 里的进程创建）。两个坑都记在下面。

## 一、结构对比（基于 rpi 0.3.6 / pi 0.87.1 的当前代码）

### 1. 运行时与分发：单二进制 vs npm 依赖树

| | rpi | 原生 pi |
| --- | --- | --- |
| 语言 | Rust | TypeScript |
| 分发物 | 一个静态可执行文件 **21.8 MiB** | npm 包 + 依赖树 **约 385 MiB** |
| 额外运行时 | 无 | Node.js **约 91 MiB** |
| 包管理器 | cargo / brew / scoop / install.sh | npm |

rpi 的安装产物就是 `~/.cargo/target/release/rpi.exe` 一个文件（`Cargo.toml` 里
`strip = "symbols"`、`lto = "thin"`）。原生 pi 的 `package.json` 依赖表里除了自家
`@earendil-works/*`，还带 `chalk`、`undici`、`jiti`、`highlight.js`、`diff`、`photon-node`
等十余个运行时依赖，装完就是几百 MiB。

这不是"谁更先进"，而是**两种分发模型**:一个把自己压进一个二进制，一个复用 Node 生态。

### 2. 依赖分层：单向 crate 图 vs monorepo packages

rpi 是 **library-first**：agent loop 是库，CLI 只是第一个使用者。九个 crate 单向依赖，
`Cargo.toml` 的 `members` 里能看到完整清单：

```
rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli
                  ↑                                        ↑
            rpi-tui                          rpi-plugin-sdk → rpi-extensions
```

两条硬约束（评审会直接拒）：不能反向依赖；`rpi-plugin-sdk` 必须是零依赖叶子。

native pi 同样是 monorepo，但拆的是**能力包**而不是依赖层：
`packages/agent`、`ai`、`tui`、`coding-agent`、`durable`、`chord`、`protocol`、
`server`、`client`、`session-backends`、`telemetry`。两边都是模块化，
区别在于 rpi 把"依赖方向"当成可验证的契约，pi 的包之间耦合更松但也更扁。

### 3. 嵌入方式

- **native pi**：`createAgentSession()` 在 **Node.js / Bun 进程内**嵌入
  （见 `packages/coding-agent/docs/sdk.md`），TypeScript 直接调用；跨语言要走
  `--mode rpc` 子进程。
- **rpi**：`cargo add rpi-agent` 直接在**你自己的 Rust 进程内**跑 agent loop，
  带自己的事件处理和输出，不用起子进程、不用解析 stdout。跨语言同样有 RPC 模式。

pi 的 RPC 文档写得很清楚：SDK 适合 Node/Bun 宿主，RPC 适合"其他语言、隔离进程、IDE"。
rpi 的定位是把这两条路都做成第一等公民——库可以嵌，CLI 可以跑，两边是同一份实现。

### 4. 扩展机制：稳定 C ABI vs 进程内 TS 模块

这是两个项目差异最大的一处。

- **native pi 扩展**是 TypeScript 模块，`jiti` 动态加载，**运行在 Pi 进程内，同权限**
  （官方文档原话："load extensions only from sources you trust"）。
- **rpi 插件**是 Rust `cdylib`，走手写的 `#[repr(C)]` ABI，带**版本协商 + panic 隔离**。
  ABI 在编译器/依赖版本漂移下仍能加载，插件出 panic 不会把宿主带走。

两者都能注册工具、命令、provider、事件处理器。区别是信任模型：pi 的扩展就是宿主进程的一部分；
rpi 的插件被一道显式 ABI 边界隔离，代价是只能写 Rust（对 Node 扩展的桥接目前是实验性、默认关闭）。

### 5. 会话、工具与 provider

| 维度 | rpi | 原生 pi |
| --- | --- | --- |
| 会话存储 | JSONL 会话树 + **逐帧进度 + 崩溃恢复** | JSONL 会话树（`session-format.md`） |
| 工具 | `read`/`write`/`edit`/`bash` + `grep`/`find`/`ls`/`powershell` | 同名工具集（`bash`/`edit`/`find`/`grep`/`ls`/`powershell`/`read`/`write`） |
| 工具执行抽象 | `ExecutionEnv`（内存 / OS 两套后端） | 直接跑在宿主进程里 |
| Provider | Anthropic / OpenAI / Responses + OpenAI 兼容网关 | OpenAI / Anthropic / Google 等 |
| 离线测试 | `faux` provider + 内存 `ExecutionEnv`，**默认构建不含 HTTP 栈** | — |

工具集基本对齐——rpi 的 CLI 在 `crates/rpi-cli/src/session.rs` 里注册
`read`/`write`/`edit`/`bash`/`docs`（Windows 再挂 `powershell`），和 pi 的
`packages/coding-agent/src/core/tools/` 一一对应。

真正的结构性差异在两个地方：

1. **崩溃恢复**。rpi 把流式帧的进度写进 session，进程中途死掉能续跑，
   排队的 steering / follow-up 消息不随进程丢；pi 的 SessionManager 也是会话真相源，
   但没有把"恢复"做成一等能力。
2. **可测性**。rpi 的 `faux` provider 和内存 `ExecutionEnv` 是一等公民，
   `cargo test --workspace --locked` **全程离线**，测试不可能偷偷依赖网络；
   pi 测试框架里没有对应的确定性 provider 层。

### 小结：两种宿主形态

| | rpi | 原生 pi |
| --- | --- | --- |
| 形态 | 单文件二进制 + Rust 库 | Node 进程 + TS SDK |
| 嵌入语言 | Rust（进程内）/ 任意语言（RPC） | Node/Bun（进程内）/ 任意语言（RPC） |
| 扩展 | Rust cdylib，稳定 ABI、panic 隔离 | 进程内 TS 模块 |
| 分发成本 | 21.8 MiB | ~385 MiB + 91 MiB Node |
| 适合 | 想自己拥有 loop、要塞进 Rust 服务/CLI | 想用最成熟的 Node 生态、最省心 | 

如果你就是要一个开箱即用的终端 agent，native pi 生态更厚、模型更新更快。
rpi 是给**想自己掌控 loop**的人准备的。

## 二、性能对比（同机实测）

### 怎么测才公平

两个工具都是"终端里的 coding agent"，但一个是原生二进制、一个是 Node 脚本，
很容易测成"比谁启动得快"这种没有信息量的对比。所以约束如下：

| 约束 | 做法 | 为什么 |
| --- | --- | --- |
| 同一终点 | 双方都通过同一套 JSONL 命令通道，测"发出 `get_state` 到收到响应" | 这才是"agent 可用了"，而不是"参数解析完了" |
| 冷且隔离 | 每轮用全新的空配置目录：pi 用 `PI_CODING_AGENT_DIR`，**rpi 用 `RPI_CODING_AGENT_DIR`** | 两边变量名不同，只设一个会漏掉另一边（见坑 3） |
| 离线 | pi 用 `PI_OFFLINE=1`、rpi 用 `RPI_OFFLINE=1` | 测的是运行时开销，不是网络 |
| 占位凭据 | 给一个假 `ANTHROPIC_API_KEY` | rpi 在空目录下没凭据就不建会话；离线不会真的用 |
| 中性目录 | 双方都在一个空临时目录里启动 | 避免谁去扫了这个大仓库 |
| 不采样 | 计时窗口内不做任何 RSS 采样 | 见坑 1 |
| 无 shell | 绕过 npm 的 `.bin/pi` shim，直接 `node cli.js` | 见坑 2 |

"冷启动"这个终点不是我发明的——**原生 pi 自己的
`scripts/profile-coding-agent-node.mjs` 就是这么定义"启动完成"的**。
用对手的定义测对手，比自选一个有利口径更有说服力。

### 结果

环境：Windows 11 (26200) / x86_64、rustc 1.97.1 `--release`、Node v25.9.0、
rpi **0.3.6**、pi **0.87.1**。3 轮预热后测 11 轮，取中位数。

| 指标 | rpi（Rust） | pi（TypeScript） | 差距 |
| --- | --: | --: | --: |
| `--version` | **17.5 ms** | 169.5 ms | **9.7×** |
| 冷启动 | **17.7 ms** | 189.5 ms | **10.7×** |
| 常驻内存（RSS at ready） | **12.0 MiB** | 91.5 MiB | **7.6×** |
| 安装体积 | **21.8 MiB**（单二进制） | 约 385 MiB（+91 MiB Node） | **约 18–22×** |

原始数据（11 轮，单位 ms）：

```
rpi  --version  17.8 19.4 17.4 16.9 19.8 16.8 17.5 18.9 17.2 17.0 19.5   median 17.5
rpi  cold start 18.3 22.2 16.6 17.7 17.9 17.0 17.7 18.6 16.6 17.3 17.5   median 17.7
pi   --version 170.7 170.6 171.8 168.7 169.5 168.9 169.2 171.9 166.7 172.0 167.0   median 169.5
pi   cold start 188.4 190.4 189.0 189.9 189.5 190.8 189.1 189.7 190.5 189.3 187.4   median 189.5

rpi  rss at ready  12.0 12.0 12.0  MiB
pi   rss at ready  91.5 91.3 91.6  MiB
```

### 真正有意思的部分：时间去哪了

**rpi 的成本几乎就是进程创建。** 它的 `--version` 是 17.5 ms，冷启动是 17.7 ms——
两者几乎相等。config、会话、harness、6 个内置工具的初始化加起来可以忽略，
真正花钱的是把进程拉起来。

**pi 的开销几乎全在 Node 启动，外加约 20 ms 的 agent 初始化。** 它的 `--version`
是 169.5 ms，冷启动是 189.5 ms——这约 20 ms 的差就是它加载 agent 本身的成本。
两者都被 Node 运行时主导，所以在这个维度上它没什么可优化的。

### 我踩过的四个坑（这也是这些数字为什么可信）

**坑 3（最重要）：rpi 根本没被隔离，测出来的"1.7 倍"是错的。**

上一版测出来 `--version` 快 9 倍，但"冷启动"只快 1.7 倍——两者差得离谱。
查下去，还是我自己的锅：**隔离用的环境变量只对 pi 生效**。
我在脚本里给两边都设了 `PI_CODING_AGENT_DIR` / `PI_OFFLINE`（原生 pi 的变量名），
但 rpi 用的是 `RPI_CODING_AGENT_DIR` / `RPI_OFFLINE`。结果是：pi 老老实实用空目录冷启动，
而 **rpi 忽略这些变量，直接读本机真实的 `~/.rpi/agent`**——
包括我装在里面的 **25 个插件 DLL**（`rpi_voice.dll` 19 MB、`rpi_im_message.dll` 7 MB、
`mcp_adapter`、`langfuse`、`codegraph`……）。

证据在 `get_state` 的回包里：那个配置下 rpi 报告 **31 个工具**，真正隔离后只有 **6 个内置工具**。
那"83 ms 初始化"几乎全是加载我自己的插件，不是 rpi 的核心开销。修正隔离后，
冷启动从 101 ms 掉到 10 ms（但这 10 ms 其实也没算全，见坑 4）。

这个坑值得单独说，因为它是**基准作弊的常见形态**：环境变量写错，一方读了真实配置/插件，
另一方是干净的，数字就废了。而且它偏向的方向完全不可预测——这次是让 rpi 看起来慢。

**副产物**：这个数字也说明插件加载是有成本的。你要是装一堆插件，启动就会变慢——
这部分是"你装了什么"，不是"运行时本身多快"。

**坑 1：RSS 采样污染了计时窗口。**
我在 Windows 上每 10 ms 起一个 `tasklist` 去采样对方内存，
这个进程本身的开销把两个工具都拖慢了，而且对快的那个影响更大——正好抹平了真实差距。
现在内存单独一轮测，计时窗口内不做任何采样。

**坑 2：npm 的 shim 会白送对方一个 shell。**
npm 装的 `pi` 是 `.bin/pi`，在 Windows 上要走 `cmd.exe`。
直接跑会把 `cmd.exe` 的启动算进 pi 的时间里。现在解析 shim 指向的 JS 入口，直接用 `node` 跑。

**坑 4：秒表放在了 `spawn()` 之后。**

修好隔离后，我得到一个 10.1 ms 的冷启动，比 `--version` 还小——这不可能。
查脚本发现 `measureRpc` 里的计时器是这么起的：

```js
const child = spawn(...);              // 进程已经在这儿被创建
const startedAt = performance.now();   // 秒表却在这之后才起
```

而 `--version` 的计时器在 `spawnSync` **之前**。也就是说冷启动的计时漏掉了 `spawn()`
内部的进程创建开销。实测在 Windows 上，Node 的 `spawn()` 调用本身就要 **7.3 ms**——
把它补回去，rpi 的冷启动就从 10.1 ms 变成 **17.7 ms**，和 `--version` 对齐了。

所以 **“冷启动”到底量的是什么**：从 `spawn()` **之前**开始计时，到父进程从子进程
stdout 解析出那条 `get_state` 的 response 为止。它包含：进程创建 → 参数解析 → config →
模型目录 → 工具注册 → 扩展/插件发现与加载 → harness/会话构建 → 输出
`{"type":"ready"}` → 读入并回答 `get_state`。它**不**包含：进程退出/清理
（测完立即 kill），以及任何 LLM 或工具调用。

### 没有测的部分（说清楚，免得被这张表暗示）

- **LLM 延迟、工具循环吞吐**：这取决于模型和网络，跟运行时无关。
- **长会话内存增长**：只测了"刚就绪"的常驻内存，没测跑几百轮之后。
- **TUI 帧开销**：这块两边都做过缓存优化，是另一类问题，需要单独基准。

## 三、复现

```bash
git clone https://github.com/bigfish1913/pi-rust && cd pi-rust
cargo build -p rpi-cli --release

npm install --prefix .bench @earendil-works/pi-coding-agent
node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi --runs 11 --warmup 3 \
  --json .bench/results.json
```

换一台机器重跑会得到不同的绝对值——所以**比值也请以你自己机器上的结果为准**，
不要直接引用这里的绝对 ms 数。完整方法与原始数据在
[`docs/performance-vs-pi.md`](../performance-vs-pi.md)。

## 四、什么时候选哪个

| 你的需求 | 建议 |
| --- | --- |
| 开箱即用的终端 agent，要最厚生态 | 原生 pi |
| 想把 agent 嵌进 Rust 服务 / 自己的 CLI | rpi |
| 要单文件分发、无 Node 依赖 | rpi |
| 要跨语言、子进程隔离集成 | 两边都行（都是 RPC 模式） |
| 要用 TS 写扩展、复用 npm 生态 | 原生 pi |
| 要有 panic 隔离的稳定插件 ABI | rpi |

## 项目

rpi 是 Rust 原生、library-first 的 coding-agent runtime，九个 crate 同版本发布：
`rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`，
另有 `rpi-plugin-sdk`（稳定 `#[repr(C)]` ABI）与 `rpi-extensions`（宿主加载器）。MIT 许可。

- GitHub：https://github.com/bigfish1913/pi-rust
- 官网：https://rpi.laofu.online/
- 完整数据与方法：https://github.com/bigfish1913/pi-rust/blob/main/docs/performance-vs-pi.md
- 免 Rust 工具链安装：
  `curl -fsSL https://raw.githubusercontent.com/bigfish1913/pi-rust/main/scripts/install.sh | sh`

---

## English version

### A Rust rewrite vs the TypeScript original: structure and measured speed

rpi is a Rust port of the TypeScript `pi` coding agent. Rewrite projects usually
claim "Rust is faster" without numbers, so I installed both tools side by side,
measured them the same way, and compared the code structure too.

**Headline: 9.7× faster to start, 10.7× faster to cold start, 7.6× less
memory — after fixing two measurement bugs: rpi was not actually isolated, and
the cold-start stopwatch started after `spawn()`.**

**Structure.** It is not "the same thing in another language", it is two host
shapes for the same agent:

| | rpi (Rust) | native pi (TypeScript) |
| --- | --- | --- |
| Artifact | one **21.8 MiB** static binary | npm tree **~385 MiB** (+ ~91 MiB Node) |
| Embedding | in-process from **Rust** (`cargo add rpi-agent`), or any language over RPC | in-process from **Node/Bun** (`createAgentSession`), or any language over RPC |
| Extensions | Rust `cdylib` behind a versioned, panic-isolated `#[repr(C)]` ABI | in-process TypeScript modules (same OS permissions) |
| Tools | `read`/`write`/`edit`/`bash` + `grep`/`find`/`ls`/`powershell` over an `ExecutionEnv` seam (in-memory and OS backends) | the same tool set, running directly in the host process |
| Offline testing | first-class `faux` provider + in-memory env; default build pulls no HTTP stack | — |
| Sessions | JSONL tree with **per-frame progress and crash recovery** | JSONL session tree |

**Performance.** Same machine, same JSONL command channel, a fresh empty config dir
per run (pi via `PI_CODING_AGENT_DIR`, rpi via `RPI_CODING_AGENT_DIR`), both
offline, real credentials cleared, no shell in the path.

| Metric | rpi (Rust) | pi (TypeScript) | Difference |
| --- | --: | --: | --: |
| `--version` | **17.5 ms** | 169.5 ms | **9.7×** |
| Cold start | **17.7 ms** | 189.5 ms | **10.7×** |
| RSS at ready | **12.0 MiB** | 91.5 MiB | **7.6× smaller** |
| Install footprint | **21.8 MiB** | ~385 MiB (+ ~91 MiB Node) | **~18–22×** |

What **Cold start** actually measures: the clock starts *before* `spawn()`, and
stops when the parent parses the `get_state` response from the child's stdout. It
covers process creation → arg parse → config → model catalog → tool registry →
extension discovery/loading → harness/session build → emitting `{"type":"ready"}`
→ reading and answering `get_state`. It excludes process teardown (the child is
killed right after) and any LLM or tool work.

Where the time goes: rpi's `--version` (17.5 ms) and its cold start (17.7 ms) are
effectively equal — initialisation is negligible; the cost is process creation.
Pi's `--version` (169.5 ms) versus cold start (189.5 ms) leaves ~20 ms for its
own agent initialisation; both are dominated by Node boot.

Four measurement bugs were found and fixed. The two that mattered most were an
**isolation bug** (the harness set pi's `PI_*` variables but rpi reads `RPI_*`, so
rpi loaded the 25 plugin DLLs in my real `~/.rpi/agent` — it reported 31 tools
instead of 6) and a **stopwatch bug** (cold-start timing started after `spawn()`, hiding
~7 ms of process-creation cost). The other two: a 10 ms RSS sampler spawned
`tasklist` inside the timing window, and the npm shim would have charged
`cmd.exe` startup to pi.

Not measured, stated plainly: LLM latency, tool-loop throughput, memory growth
over a long session, TUI frame cost.

Reproduce:

```bash
node scripts/bench-vs-pi.mjs --pi .bench/node_modules/.bin/pi --runs 11 --warmup 3
```
