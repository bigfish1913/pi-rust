# 共享宿主：并发基线与演进边界

状态：**P0 基准及原生 async Provider 入口已实现；Host 控制清单层已有实现和测试，生产共享宿主与全局预算尚未实现。**
当前性能验证范围为 **600 个真正活动的完整 Agent**，不通过排队降低实际并发。
早期的 3000 规模仅是历史探索目标，本轮不执行，也不承诺任意长上下文、原生扩展
和任意子进程均能满足统一内存上限。

## 不可混淆的并发与内存口径

- 一个 Agent 是独立 Harness、Session、运行状态、权限和取消域；不能用共享
  `inner`、active-run 和队列的 `AgentHarness::lane()` 代替独立 Agent。
- 模型并发必须由服务端确认：完整收到 N 个请求体后，N 个处理器仍在响应
  barrier 内。在 HTTP 连接队列、任务队列或 semaphore 外等待不算在途请求。
- 工具并发必须在工具函数内部证明。bash 变体必须先启动 N 个真实进程，且
  每个 bash 已执行并输出 readiness，再计入 barrier；不能把等待执行槽的调用算进去。
- 普通“模型 → 工具 → 模型”存在因果关系。本基准分别证明 N 个模型请求和
  N 个工具调用的重叠，不声称同时存在 N 个模型请求 **加** N 个工具调用。
  后一种要求需要独立工作分支和另一个重叠验收场景。
- `ps` 的 RSS 是宿主观察值；进程组 RSS 求和包含工具，但可能重复计算共享页，
  不是物理内存精确总量。采样峰值可能漏过短暂尖峰。
- Linux 验收应使用专用 cgroup v2 的 `memory.current` / `memory.peak`，包括
  anonymous、文件页缓存、内核记账和子进程。请求 spool、Session/工具输出文件的
  page cache 也计入；不能通过换页、远程工具或移动记账边界宣称节省了总内存。

## P0 文件与实际覆盖

| 文件 | 职责 |
| --- | --- |
| `crates/rpi-harness/examples/shared_host_probe.rs` | 单一 Tokio runtime，共享 Provider/HTTP client/工具绑定，独立 Harness 与 JSONL Session，内部工具 barrier，完整工具往返及会话写入检查 |
| `scripts/bench-shared-host.py` | 独立 asyncio HTTP/SSE 服务、阶段控制、请求体计数、内存采样、超时与整个进程组清理、结果校验 |
| `scripts/test_bench_shared_host.py` | 参数/防覆盖、真实请求 barrier、错误工具结果、结果完整性、超时子进程清理与可选真实二进制回归 |

本地 fixture 使用真正的 `OpenAiCompletionsProvider`，不是 faux provider，也不是
只返回一次字符串的精简 Agent。`tool` 模式运行 `LLM → probe_barrier → LLM`，
并校验 Agent ID、tool-call ID、工具返回内容和最终响应。默认原生 durable 要求不关闭。
每个 Session 获取独立 writer lease，结束时 drain、检查写入错误并 close。

注册了 read/write/edit/bash，但脚本仅调用专门的 `probe_barrier`。`--tool-kind bash`
让该工具启动真实 bash，用 shell 内置命令输出 readiness、等待 stdin 放行并退出；
它**不是**对生产 bash 工具所有语义的测试。绑定共享是因为本 fixture 所有 Agent
明确共用同一合成工作区，不代表不同 cwd/权限的工具实例可以任意共享。

除 raw 诊断模式外，fixture 不旁路生产 Harness/Agent 循环；不加载用户会话、用户配置、
技能和扩展。单独的 Python 控制器/模型服务不在测量边界内。Provider 明确匿名、禁用代理和
HTTP 重定向，只允许显式 loopback HTTP URL，不需要真实模型凭据。
当前协议为 HTTP/1.1、无 TLS、每响应后 `Connection: close`（先 `write_eof()` 并等待
对端 EOF，超时后关闭；未证明这能消除所有连接重置）；没有实现 HTTP/2/TLS 场景。
短响应不会覆盖大 token 流、慢订阅者或持续会话增长的内存压力。

### 模式

| `--mode` | 配置与执行 | 模型/工具并发证明 |
| --- | --- | --- |
| `empty` | 独立 JSONL Session/Harness，无已注册工具和 Provider 绑定；全局客户端等 fixture 服务仍存在 | 无，仅对象基线 |
| `idle` | 同样的独立 Session，注册内置工具、barrier 工具及共享 Provider，不发起请求 | 无，仅配置后空闲基线 |
| `model` | 每个 Agent 一次 HTTP/SSE 请求及持久化完成 | 模型请求 barrier |
| `raw` | 只用真实 Provider/HTTP/SSE 发一次请求；**不建 Session/Harness/Agent**，用于按层归因 | 模型请求 barrier（不代表 Agent 并发） |
| `tool` | 每个 Agent 两次模型请求与一次工具执行 | 首轮请求 barrier + 工具内部 barrier |

`--context-bytes` 是每条合成 ASCII 用户内容的大小，不是 token 数；
`--history-entries` 在每个 Session 中先持久化指定数量的历史用户消息。
活动模式再附加包含 Agent ID 的当前提示。应记录二者，不能把小上下文结果外推到
真实长历史。当前没有图片、附件、长响应、扩展或 compaction 专项负载。

### 构建与小规模运行

```bash
cargo build --release -p rpi-harness --example shared_host_probe --locked

python3 -B scripts/bench-shared-host.py \
  --binary target/release/examples/shared_host_probe \
  --output-root /tmp/rpi-shared-tool-8 \
  --agents 8 --mode tool --tool-kind async --hold-seconds 2 --timeout 120

python3 -B scripts/bench-shared-host.py \
  --binary target/release/examples/shared_host_probe \
  --output-root /tmp/rpi-shared-bash-8 \
  --agents 8 --mode tool --tool-kind bash --hold-seconds 2 --timeout 120
```

输出根目录及其父目录由调用者明确选择，**根目录必须不存在，父目录必须存在**。
Rust fixture 目录也用独占创建并写 `PROBE_ONLY` 标记。重复运行应换一个新根目录；
脚本不会清空目录，不会清理或复用历史结果。失败也保留 fixture 和诊断资料供排查。

成功必须同时满足子进程 exit 0、精确阶段序列、全部 Agent 完成、正确请求/工具
结果计数及真实 barrier 峰值；任何一项不满足都输出 `success: false` 并返回非零。
只有创建 N 个 Harness 或建立 N 个连接，不能通过校验。

输出：

- `results.json`：配置、OS、资源限制、采样、服务端统计、成功/失败及错误信息。
- `phases.jsonl`：每创建 100 个 Agent 记录 `setup_progress`；执行阶段为 `ready`、
  `requests_held`、`tools_held`、`settled`、`complete`，不适用的阶段省略。
  初始化进度不计入模型/工具并发证明。`ready` 和进度包含 Session 创建/lease、
  Harness 创建、历史追加的累计耗时。超时可能在 `ready` 之前发生，应先区分初始化
  和活动阶段，不能把未完成初始化当作并发容量结论。
- `memory.jsonl`：逐次追加的内存与请求进度采样；即使控制器意外中断，也保留已落盘部分。
- `probe.stderr`：Rust panic/系统错误。超时和异常尝试终止整个专用进程组，避免
  只杀 Harness 而遗留 bash。成功路径也执行进程组兜底清理。清理权限错误若不能
  确认进程组已消失，会作为 `cleanupErrors` 保留并判失败，不覆盖原始错误或丢弃报告。
- `fixture/`：专用 JSONL Session、锁和进度文件；由用户决定何时删除。

`ready`、首轮请求被全部持有、全部工具被持有、执行完成但 Harness 仍存活时，
各有显式采样；另外约每 200 ms 采样一次。`--hold-seconds` 延长这些观察窗口。
Linux 额外读取宿主 VmHWM、线程数和 FD 数；macOS 通过 libproc 读取线程数和
FD 数，无需启动 lsof。结果汇总保留线程/FD 的采样峰值。进程退出或计数接口不可用时
保留缺失，不把缺失值伪装成零；已退出宿主的 RSS 记为 null。

### 600/3000 级定位与验收边界

600 个 Agent 用于快速定位和修改效果；先用小规模检查功能，再在 600 级观察 FD、
线程、进程数、初始化耗时和活动峰值。初始化 deadline 不足时应显式调整；不要从
初始化超时推导模型最大并发。3000 级已在本轮之后用当前 0.27.0 集成构建的异步工具
执行底座跑通两轮，但仍不是生产 Host 验收，见
[3000 报告](shared-host-3000-performance.md)。

```bash
# 仅提高本命令所在 shell 及其子进程限制；不修改全局系统配置。
# 数值须在当前机器的 hard limit 和管理员策略允许范围内。
(
  ulimit -n 32768
  python3 -B scripts/bench-shared-host.py \
    --binary target/release/examples/shared_host_probe \
    --output-root /tmp/rpi-shared-tool-600 \
    --agents 600 --mode tool --context-bytes 1024 \
    --hold-seconds 2 --timeout 120
)
```

bash 变体需要至少 N 个额外真实进程及更多 FD，可能被系统/用户进程限制拒绝。
这应诚实记录为未达到并发，不能自动退化为较少进程并仍报告 3000。

### Linux cgroup v2 测量

调用者预先准备**全新、空闲、已委派写入权限**且启用了 memory controller 的
专用 cgroup，并通过 `--cgroup /sys/fs/cgroup/.../fresh-run` 指定。控制器不会创建
cgroup、修改全局层级、调整系统限制、自动开启 controller 或重置共享 cgroup 峰值。
要求 `cgroup.procs` 为空、`cgroup.events` 的 populated=0、`memory.peak`=0；
有子层级负载或旧峰值的目录会被拒绝。不要同时把这个 cgroup 分配给其他任务。

Rust 进程在创建 runtime、Session 和子进程之前自行写入 `cgroup.procs`；工具进程
继承归属，Python 控制器和 HTTP 服务留在外部。启动参数解析及 enrollment 之前的
极少量进程启动分配不构成完整生命周期的内存证明；重点观察初始化及活动阶段。

脚本在每次采样读取 `memory.current`、`memory.peak`、`memory.stat` 和
`memory.events`，保留运行前后数据；观察到 OOM/oom_kill 计数增加即判失败。
`memory.peak` 是内核高水位，不是仅依赖轮询的峰值。Linux 内核/cgroup 接口不支持时
应报告缺口，不得用 RSS 总和冒充同等验收。

正式验收还必须确认该 cgroup 禁止 swap、没有其他进程/后台负载混入、FD/进程限制
足够，并审核运行中没有改变记账归属。当前开发验证在 macOS 上进行，**Linux
真实 cgroup enrollment 和计账尚未执行验证**；文件格式与拒绝规则有单元测试，不能
替代内核集成测试。不要仅凭 `success: true` 宣称已满足总内存 <1 GB。

### 已观察的开发基线

macOS ARM64、release、4 个 worker、小文本输出、未加载扩展、无 TLS，均为本地
HTTP fixture；表中数值为一次运行的**采样峰值**，不是可移植的性能承诺。
`tool/async` 均使用每 Agent 1024-byte 当前内容、无预载历史。

**这些 RSS 数字已被后来的口径修正取代**：macOS 压缩页不计入 RSS，同一进程在活动
期间可读到 0.7 MB 的 RSS；请以“macOS 内存口径修正与按层归因”一节的 libproc
`phys_footprint` 数值为准。此表仅保留为历史记录与 FD/线程/初始化时间的参照。

| 场景 | 实际首轮在途 / 工具峰值 | 宿主 RSS bytes | 进程组 RSS sum bytes |
| --- | --- | --- | --- |
| 4 Agent tool/async | 4 / 4 | 15,040,512 | 15,040,512 |
| 8 Agent tool/bash | 8 / 8，9 个进程 | 16,924,672 | 34,062,336 |
| 128 Agent tool/async | 128 / 128 | 70,746,112 | 70,746,112 |
| 600 Agent tool/async | 600 / 600 | 270,843,904 | 270,843,904 |

600 场景初始化约 21 秒，活动阶段约 19 秒。一次 3000 场景用 60 秒 deadline 时
在 Session 初始化期间超时、未到 `ready`，请求数为 0：这是未完成测量，不是
3000 活动 Agent 的低内存结果。再以 600 秒 deadline 运行时，初始化约 415 秒后
进入活动阶段，但某 Agent 的 HTTP 传输失败，未到全量请求 barrier；该次还暴露了
清理权限异常中断结果写入的问题，现已修复并增加回归及增量采样。该次没有完整
汇总，不能给出可靠的在途峰值或内存数字，也不能据此认定 HTTP 失败的根因。
这些是早期 3000 尝试的失败记录；后续 0.27.0 集成构建的 3000 异步工具两轮已通过
完整请求/工具 barrier，见 [3000 报告](shared-host-3000-performance.md)。初始化
复杂度与活动阶段传输/资源限制仍需单独分析；不要线性外推小规模数值，也不要把
探针通过当成生产 Host 验收。

### 首轮优化与 600 级复测

已确认初始化热路径：`JsonlSessionRepo::session_id_exists` 只需要文件名后缀，却通过
`list_dir` 为所有 Session、lock 和 progress sidecar 收集完整 FileInfo；OS 后端对每项
执行一次异步 lstat 和 blocking-pool 往返，批量创建会重复支付这笔代价。

现新增兼容默认实现的 `FileSystem::has_file_with_suffix`，OS 后端在一次 blocking
任务中扫描名称，仅对匹配项读取文件类型，不构造所有文件的 size/mtime/path 列表。
目录不算匹配文件，symlink 不跟随，特殊文件不算匹配；取消和真实 I/O 错误保留。
查询不缓存负结果，因此外部创建者新建的文件不会因缓存被漏掉。没有修改 writer lease、
fsync 或原子发布路径，也没有增强或削弱原有跨进程创建保证。

这**仍是目录名称扫描**，批量创建的名称比较数量仍可能是二次增长，并未实现常数时间
ID 索引。没有为了提速引入会过期的目录缓存或随历史无界增长的常驻索引。

同机、同一 600-Agent 负载（4 workers、1 KiB 当前内容、无历史、async 工具）的观察：

| 构建 / 运行 | 初始化 | 其中 Session 创建及 lease | 活动阶段 | 模型 / 工具峰值 | 宿主 RSS 采样峰值 |
| --- | --- | --- | --- | --- | --- |
| 修改前复跑 | 23.196 s | 未分项 | HTTP 失败，未完成 | 438 / 未进入 | 不与完整运行比较 |
| 轻量查询 A | 8.243 s | 8.210 s | 28.673 s | 600 / 600 | 281,313,280 bytes |
| 轻量查询 B | 8.595 s | 8.551 s | 22.840 s | 600 / 600 | 224,706,560 bytes |

初始化观察值约缩短到原来的三分之一；这是少量开发测量，不是统计性能承诺。分项
显示剩余初始化主要仍在持久化 Session/lease，Harness 创建仅约 32–43 ms。
这次优化针对初始化，不声称降低活动内存；RSS 和运行耗时仍有明显波动。

两次成功运行都观察到 **518 个宿主线程**；A 的请求 barrier 时为 1809 个 FD。
这是后续异步入口/线程膨胀分析的依据，不是已证实 HTTP 失败根因。当时同步 Provider
bridge 仍使用 `block_in_place`；原生 async-open 的后续对照见下一节。

HTTP 失败此前只留下 reqwest 的顶层文字。现在共享请求发送层保留 connect/timeout/
request/body 分类，以及 source 链中 typed `std::io::Error` 的 kind 和 OS errno；
移除请求 URL，不字符串化任意 source 错误，避免凭据、私有 path/query 或代理信息泄漏。
拒连和超时回归验证这些诊断及原有 retryability。600 的偶发传输失败尚未定位根因；
后续若重现，可据类别/errno 与 FD/线程记录判断，不把两次成功当作问题已彻底修复。

### 原生异步入口与阻塞池预算：600 级对照

`rpi-agent` 新增 `AsyncStreamFn`、`async_stream_fn`、`StreamSource` 和三个
`run_agent_loop_*_with_stream` 入口。原有同步 `StreamFn`、loop 函数签名和
`AgentOptions` 的 struct-literal API 保留。`AgentBuilder::async_stream_fn` 与同步
setter 最后设置者生效；没有安装新的进程级默认工厂。

内置 Harness 的正常、continue 与 deferred 路径改为直接 await Provider 开流。
future 借用 model/context/options，不通过转发任务、额外流队列或完整历史复制来桥接。
观测型 hooks 保持借用，实际 context patch 才触发 COW；固定 stream options 的合并、
每次请求 hooks、惰性 Provider 解析及无 Provider 错误保持。开流期间取消会 drop future，
通过带真实 epoch 时间戳的 Aborted 终态保留 MessageEnd/TurnEnd/AgentEnd；预先取消
不调用异步工厂。工厂自行派生的工作仍必须传播 `options.signal` 并正确清理资源。

**只改 async-open 没有降低线程峰值。** 同机相同负载、4 runtime workers、600 个独立
持久化 Session、1 KiB 当前内容、无历史、async 工具、hold 1 秒，得到：

| 工厂 / 阻塞池预算 | 宿主线程峰值 | FD 峰值 | 宿主 footprint 采样峰值 |
| --- | --- | --- | --- |
| 同步入口 / 默认 512（8.033 s + 24.591 s） | 518 | 1809 | 未测（旧二进制无该指标） |
| async / 默认 512 A（8.110 s + 25.443 s） | 518 | 1809 | 未测（旧采样） |
| async / 默认 512 B（8.098 s + 21.858 s） | 518 | 1809 | 未测（旧采样） |
| async / 默认 512 C | 518 | 1809 | 194,332,040 bytes |
| async / 32 A | 38 | 1809 | 181,715,568 bytes |
| async / 32 B | 38 | 1809 | 未测（旧采样） |

全部运行均完成：600 个完整首轮请求同时在服务端等待、600 个工具在内部同时等待、
1200 个 HTTP 请求、600 个正确工具结果和 600 个最终响应。没有以 32 个 Agent 槽位
替代 600 并发；两种预算的 FD 峰值也相同。async/32 在请求 barrier、工具 barrier 和
settled 时均为 38 线程。线程数是精确计数，不受下述内存口径修正影响。

JSONL append、read 和 OS 文件系统操作也使用 Tokio 阻塞池；移除 Provider 同步桥接
不会自动为这些工作建立全局预算。基准新增 `--max-blocking-threads`，仅配置该 runtime
的阻塞池上限，不限制异步任务、Agent、HTTP 或工具活动数。未设置时仍为 512；**没有
修改 CLI 或库使用方的生产 runtime 默认值**。该实验说明此负载下可用有界阻塞池保持
完整并发，不是所有生产工具适用 32 的证明；共享阻塞池中的 CPU 密集工作、阻塞扩展及
依赖同步 bridge 的外部工厂还需要单独审计，不能盲目降低其上限以免饥饿或死锁。

### macOS 内存口径修正与按层归因

`ps` 的 RSS **不能单独代表 macOS 的内存成本**：实测同一存活进程在 600 连接保持
期间 RSS 大幅变化。压缩页是两种口径不同的原因之一，但本次没有独立证明这些波动
全部来自压缩。因此脚本同时记录 libproc `proc_pid_rusage` 的 `ri_phys_footprint`
作为主口径，并保留 RSS；二者都不是精确的存活堆分配统计。`rusage_info_v0` 末尾的
两个 abstime 字段必须存在，否则内核越界写入会损坏 Python 堆（已修）。

新增 `--mode raw`：只走真实 Provider/HTTP/SSE，**不创建 Session、Harness 和 Agent
循环**，同样满足 600 个完整请求体同时在服务的 barrier。用同一 1 KiB 内容、无历史、
4 workers、阻塞池 32（raw 另加 `--stagger-ms 1`，见下）测得：

| 模式 | footprint 采样峰值 | 线程峰值 | FD 峰值 | 说明 |
| --- | --- | --- | --- | --- |
| `empty` | 7,618,992 | 8 | 609 | 600 个独立 Session，无工具/Provider |
| `idle` | 9,109,912 | 8 | 609 | 加内置工具与 Provider 绑定，不发请求 |
| `raw` | 49,185,200 | 6 | 609 | 600 个 Provider 请求同时在途，无 Harness |
| `model` | 159,416,944 | 38 | 1809 | 每个 Agent 一次完整 Harness 运行 |
| `tool` | 181,715,568 | 38 | 1809 | 每个 Agent 模型→工具→模型 |

这些是**整轮采样峰值**，不保证出现在相同阶段，不能直接相减得到某一层的存活开销。
若仅把峰值除以 600，raw 约 82 KB、model 约 266 KB；此前的“74 KB / 250 KB、额外
176 KB/Agent”混用了基线扣除口径，应撤回。raw/model 的工具 schema、系统提示、请求
大小与历史处理也不同；差异只能作为进一步分析 Harness/Agent/持久化路径的线索，
不是精确的分层归因。后续对照应使用相同负载、匹配阶段和重复运行。

FD 归因已用 `lsof` 核实：每个 Agent 在运行时持有 1 个 socket + 1 个 JSONL 文件
+ 1 个 `.lock`；600 Agent 即 1809 = 600×3 + 9 基础 FD。

`closed` 阶段在所有 Harness `close()` 之后、进程退出之前采样。核对上述 footprint
报告，pool32 tool 的 closed 为 **103,694,984 bytes**，raw 为 **49,185,200 bytes**。
此前“tool closed 45.4 MB / raw closed 1.9 MB”并非这些报告的 footprint，不能引用。
仅凭 closed 读数无法区分分配器保留页、仍存活的共享服务/后台任务与内核记账影响，
不能据此证明全是泄漏或全是可回收的分配器内存。

### 基准 fixture 的连接失败：观察与未证实解释

实验中确实观察到 `ConnectionReset`（errno 54）和 `BrokenPipe`（errno 32）。可能的
因素包括连接突发超过监听积压能力，以及服务端关闭与客户端连接复用的竞态；本机
`kern.ipc.somaxconn=128` 是已知配置，**不是已证明的失败原因**。

`--stagger-ms` 只错开 raw 的连接建立，barrier 仍要求全部完整请求体同时保持，默认 0。
fixture 也改成完整 Content-Length 响应后先 `write_eof()`，等待对端 EOF 再关闭。
但 stagger 与半关闭后，实验版 **两请求 raw** 仍出现失败；当前成功的 raw 已改为一次
请求、类似 model 的语义。因此不能把成功归因于已修复关闭竞态，也不能宣称之前
`no probe_barrier tool call` / `unexpected final text ""` 的根因已经定位或解决。
当前成功只证明当前配置下的 barrier/往返；历史间歇 HTTP 失败继续保留为未解问题。

复现（先构建 release；macOS 本次 nofile 为 32768）：

```bash
(ulimit -n 32768; python3 -B scripts/bench-shared-host.py \
  --binary target/release/examples/shared_host_probe \
  --output-root /tmp/rpi-shared-attribution-600 \
  --agents 600 --mode tool --tool-kind async --workers 4 \
  --max-blocking-threads 32 --hold-seconds 1 --timeout 120)

(ulimit -n 32768; python3 -B scripts/bench-shared-host.py \
  --binary target/release/examples/shared_host_probe \
  --output-root /tmp/rpi-shared-raw-600 \
  --agents 600 --mode raw --workers 4 --max-blocking-threads 32 \
  --stagger-ms 1 --hold-seconds 1 --timeout 120)
```

报告为 `/tmp/rpi-shared-fp-*`（footprint 表）与 `/tmp/rpi-shared-attr-*`（分层与
`closed` 采样）下的 `results.json`；这些是本机开发产物，不是仓库中持久化的验收资料。
macOS footprint 不含控制器/模型服务，也不是 Linux cgroup 总账；不能从这些结果推出
3000/<1 GB。没有重跑 3000。

### 回归命令

```bash
cargo test -p rpi-tools --test directory_suffix --locked
cargo test -p rpi-ai --features providers --lib providers::request::tests --locked
cargo test -p rpi-harness --lib session::jsonl::repo::tests --locked
cargo test -p rpi-agent --lib tool_execution_scope --locked
cargo test -p rpi-agent --test async_stream --locked
cargo test -p rpi-harness --lib borrowed_context_tests --locked
cargo test -p rpi-harness --test harness_run_e2e --test deferred_settlement --locked
python3 -B -m unittest discover -s scripts -p test_bench_shared_host.py -v
RPI_SHARED_HOST_BINARY="$PWD/target/release/examples/shared_host_probe" \
  python3 -B -m unittest discover -s scripts -p test_bench_shared_host.py -v
cargo check -p rpi-harness --example shared_host_probe --locked
```

`raw` 模式与 `closed` 阶段已纳入 Python 回归的真实二进制 smoke（四种模式 + raw +
真实 bash）；`proc_pid_rusage` 的 footprint 读数有独立测试，避免再次出现 RSS 误判。

不设置 `RPI_SHARED_HOST_BINARY` 的 Python 回归无需预先构建示例二进制，可选真实
probe 会跳过；设置该变量后覆盖全部四种模式及真实 bash，使用独占临时目录和小规模 Agent。

## P2 首次落地：消除 scoped future 重复存储

类型尺寸诊断发现，`ToolExecutionScope::run` 原先的 async 包装同时为参数 future
和移入 `TaskLocalFuture` 的 future 预留了槽位。即使普通 Harness 没有 child scope，
外层 future 仍需能容纳这个分支。改为普通函数直接返回 Tokio 的 scoped future：
**不增加 Box、任务或通道，不改 Agent/工具并发数**，仍可 `.await`；身份只在 poll/drop
期间可见，构造仅克隆 Arc。

本机 debug 编译器类型布局（bytes；不是总堆大小，不保证跨版本/平台相同）：

| Future | 修改前 | 修改后 |
| --- | ---: | ---: |
| `run_admitted_inner` | 14,240 | 14,240 |
| scope 包装返回值 | 28,520 | 14,264（直接 `TaskLocalFuture`） |
| `run_core_with_admission` | 28,672 | 14,416 |
| `run_core` | 28,872 | 14,616 |
| `send_message` | 28,840 | 14,584 |
| `drive` | 31,720 | 17,464 |
| Agent `run_agent_loop_with_stream` | 9,056 | 9,056 |

这些 future 相互嵌套，**不能相加计费**。外层减少 14,256 bytes，600 个约 8.55 MB
内联状态。诊断命令仅用于分析，不改变生产工具链配置：

```bash
RUSTC_BOOTSTRAP=1 cargo rustc -p rpi-harness --lib --locked -- -Zprint-type-sizes
```

另外观察到 Tokio mpsc 初始 block 的类型尺寸：Provider 事件 block 17,440 bytes、
帧写队列 block 10,784 bytes。它们说明通道的槽位布局值得继续分析，但不是精确的
每-Agent 常驻开销；不能把嵌套类型尺寸或不同阶段的对象直接相加。Session、事件与
帧持久化的完整存活分配归因仍未完成，本次没有据此缩减队列容量或省略持久化。

### 600-Agent release 配对实测

同一源码基线只改变上述包装，保留修改前后 release 二进制；按 before1 → after1 →
before2 → after2 顺序运行。均为 tool/async、1 KiB、无历史、4 workers、blocking pool
32、hold 2 秒、nofile 32768。所有四轮都满足：完整请求体 600 同时在途、真实工具
600 同时在途、1200 HTTP 请求、600 最终响应和工具结果、持久化成功；均为 38 线程、
1809 FD 峰值。下表为 `hostFootprintBytes`（bytes）：

| 轮次 | ready | requests_held | tools_held | 整轮采样峰值 | closed |
| --- | ---: | ---: | ---: | ---: | ---: |
| before1 | 8,749,464 | 140,001,856 | 128,811,632 | 184,451,720 | 65,077,872 |
| after1 | 8,831,408 | 131,613,248 | 113,279,576 | 167,772,784 | 44,450,392 |
| before2 | 8,733,080 | 142,819,928 | 131,793,544 | 170,508,912 | 85,590,664 |
| after2 | 8,765,872 | 134,660,672 | 121,061,976 | 177,062,536 | 113,427,056 |

完整请求 barrier 阶段分别减少 **8,388,608 / 8,159,256 bytes**，约 5.7–6.0%，与去掉
一份内联 future 状态的量级相符。工具 barrier 也下降，但整轮峰值与 closed 波动明显，
第二轮整轮峰值反而升高，**不能宣称整轮峰值或释放后内存稳定改善**。这是匹配阶段的
局部优化，不是已实现 3000/<1 GB，也没有重跑 3000。

报告：`/tmp/rpi-scope-600-{before1,after1,before2,after2}-1795187999/results.json`。
原始采样在同目录 `memory.jsonl`；这些仍是本机开发产物。复现实测时先分别保存修改前后
二进制，再对每个二进制使用新的输出目录：

```bash
(ulimit -n 32768; python3 -B scripts/bench-shared-host.py \
  --binary /path/to/saved/shared_host_probe \
  --output-root /tmp/rpi-scope-new-run \
  --agents 600 --mode tool --workers 4 --max-blocking-threads 32 \
  --hold-seconds 2 --timeout 180)
```

新增尺寸预算回归（旧实现失败、新实现通过），以及跨 yield 的身份恢复、取消/drop
与后续复用检查。Harness lib 481 项通过、1 项原有 ignored，E2E 62 项与 deferred 8 项
通过；async stream 6 项、scope 5 项及 Python 19 项（真实二进制 smoke）通过。
Agent 套件另有 118 项通过，显式过滤一项此前已在同步 HEAD 基线复现的失败：
`reference_shared_tree_quota_and_foreign_programs_are_enforced`（3 秒等待超时，随后
`parent invocation is no longer active`）。对应命令：

```bash
cargo test -p rpi-agent --locked -- \
  --skip reference_shared_tree_quota_and_foreign_programs_are_enforced
```

这项基线问题仍未修复，不能把这次带过滤的结果称为 Agent 套件无条件全绿；另有一项
原有 doc-test ignored。未修改生产 runtime 默认值，也未改动其他并行工作中的文件。

## prepare-profile worktree 整合验收

main 已快进到 `61198572`，包含历史 Session 恢复优化，以及前置的 TUI 图片重绘
优化；同时整合了当时 worktree 中 5 个文件的未提交 profiling 改动。源 worktree
保持不动，之后在源 worktree 发生的新提交/修改不自动追入当前工作区。已有本地
async、scoped future、benchmark 与其他并行工作均保留，未创建混合提交。

此次恢复优化和共享宿主优化的关系：

- owned decode 消耗已解析的 JSON 对象，减少 Entry/Record 解码中的深拷贝；保留
  字段验证、错误顺序、重复键和 RawValue 协议的兼容性检查。
- 恢复工具调用时，校验冷载荷后只提取所需的调用身份，不因只投影身份而省略完整性
  验证。它优化恢复路径，不等于普通执行时可以省略工具结果或持久化。
- JSONL 的 SHA-256 换用已有 ring 的加速后端，算法不变，保留标准向量、分块与
  sha2 对照测试；不是跳过校验或增加信任缓存。
- 启动 span 和 replay scan/classify/decode/index-apply 累加计时用于定位。嵌套启动
  span 不能简单相加；未开启诊断时不逐行取时钟，不输出会话内容或凭据。
- 这些主要作用于历史加载/恢复，与 native async 开流和减少 future 状态重复存储
  分属不同层。不能把两者收益相加，或据此声称已进一步降低 600-Agent 活动内存；
  性能收益仍须相同输入、匹配阶段的独立前后对照。

整合后回归（本次没有读取真实用户 transcript 运行 ignored codec probe）：

| 范围 | 结果 |
| --- | --- |
| Harness lib，含恢复/解码/摘要/diagnostics | 495 passed，3 ignored |
| Harness E2E | 62 passed |
| deferred settlement | 8 passed |
| Agent 套件 | 118 passed，1 项已知基线失败显式过滤，1 doc-test ignored |
| TUI image repaint | 8 passed |
| CLI startup diagnostics | 6 passed |

Agent 过滤项仍为 `reference_shared_tree_quota_and_foreign_programs_are_enforced`，
本次没有修复，不宣称 Agent 套件无条件全绿。初次整合构建达到命令等待上限后，检查
构建状态，再以 offline、2 个编译任务完成回归；构建等待超时不计作测试失败或通过。

```bash
cargo test -p rpi-harness --lib --test harness_run_e2e --test deferred_settlement \
  --locked --offline -j 2
cargo test -p rpi-agent --locked --offline -j 2 -- \
  --skip reference_shared_tree_quota_and_foreign_programs_are_enforced
cargo test -p rpi-tui --test image_repaint_cost --locked --offline -j 2
cargo test -p rpi-cli --lib startup_diagnostics --locked --offline -j 2
```

## 共享宿主设计与当前实施状态

### 开发边界：独立 worktree、先模块后 crate

共享 Host 开发位于独立 worktree `pi-rust-wt-host`，分支 `feat/shared-host`，
起始于 `61198572`，本轮已合入 main `59f985a9`（0.27.0）。从主仓库只复制本任务
相关的 25 个文件作为未提交基线，未携带无关迁移改动及先前临时 `rpi-host` crate
骨架。后续 main 合入保留全部本地文件；源 worktree 的变化不自动追入。主仓库源码、
既有未提交修改、HEAD 和暂存区保持不动。

代码先放在 `crates/rpi-cli/src/host/` 文件夹，按模块划分边界；**不新增 workspace
crate**，等确有复用或依赖隔离需求时再提取。已实现 `types`、`config`、`capacity`、
有界 `catalog` worker、SQLite `registry` 和错误类型，覆盖身份、元数据校验、
状态转换、超时策略、单实例锁、原子驻留预占与提交回滚、分页重启分类。
Host 56 项（含 15 项真实普通后端测试）、CLI 参数 41 项、启动诊断 6 项、Harness
509 项库测试及 88 项定向集成、Python fixture 19 项通过。

普通 Harness/JSONL 后端模块已接入，支持准确恢复、真实工具并发、断连不取消、
deferred 和清理结算；但仍不是完整 Host 服务：mini、IPC、服务协调器、超时计时器、
输入持久化受理/Request 收据、生产资源/权限绑定和新 CLI 命令尚未实现。普通后端
测试不能计为生产 600/3000 Host 验收。

早期两轮 600-Agent **执行底座**复测的请求保持 footprint 为 125.98 / 126.22 MiB，
采样峰值 159.52 / 156.92 MiB，均为 38 线程、1809 FD；关闭后 FD 回到 9。0.27.0
集成后的 600 两轮为 126.42 / 128.73 MiB 请求保持、158.72 / 158.88 MiB 峰值；
3000 两轮为 550.74 / 558.92 MiB 请求保持、661.75 / 657.20 MiB 峰值，均为 38 线程、
9009 FD，关闭后 FD 回到 9。它们都不经过新 catalog/ordinary 后端或 IPC，详细边界
见[600 报告](shared-host-integrated-performance.md) 与
[3000 报告](shared-host-3000-performance.md)。

### 发布与实现边界：先内置，后按需求拆分

第一版直接内置到 rpi 宿主，不以安装扩展为前提，不把服务生命周期挂到普通会话
插件。后台服务复用 rpi 可执行文件的内部服务入口，关闭首个交互窗口不关闭其他
Agent。以下命令仍是已确认方向，尚未提供；表内能力以本节开头的实施状态为准。

代码仍按 Host 核心、Agent 后端、控制协议和管理客户端分层，当前用同一 crate 内的
模块隔离。管理接口与 UI 通过同一控制协议，将来确有需求再拆 crate、管理扩展或
单独服务组件，不先扩大插件 ABI，也不引入另一套必须安装的服务程序。

| 维度 | 第一版约定 |
| --- | --- |
| 执行位置 | 独立后端 / 共享 Host 内异步任务；共享基础设施，不共享对话和权限 |
| 持久化 | 普通模式沿用权威 JSONL 与 durable；mini 无 Session/durable 初始化 |
| 组合 | 普通/mini 可在同一 Host 混跑；mini 不是整个 Host 的全局属性 |
| 入口 | 拟议 `--host`、`--mini`、`--standalone`；自动加入兼容 Host 最后接入 |
| 旧入口 | 保留 `--connect`、`--server`、RPC 与独立启动的语义 |
| 客户端 | 轻量 UI/控制进程；断连不等于取消普通 Agent，不重复创建本地副本 |
| 登记清单 | 稳定 Agent/父子身份、准确 Session 引用、启动摘要、恢复配置和超时策略 |
| 身份 | Agent/Session/Run/Request ID 与 Host epoch 分开；PID 仅用于观察 |
| 恢复 | 普通会话可独立打开；批量恢复不依赖 `-c` 最新选择、不重发初始提示 |
| mini 恢复 | 宿主崩溃后明确状态丢失；重新创建不称为恢复 |
| 管理 | 拟议 `rpi host status/list/inspect/cancel/close/restore/config/stop`，支持 JSON |

Host registry 是查找和管理入口，不替代 Session 的执行事实。启动参数脱敏存档，
恢复使用规范化配置，不保存明文凭据或重放原始 argv。mini 可出现在 Host 清单中，
但清单不能偷偷保存其对话、工具参数/输出以实现隐式持久化。Request ID 去重与受理
确认必须处理 registry/Session 两份存储之间的崩溃窗口，不以入队或 RunStart 代替
持久化受理；未知外部副作用不自动重做。

### 超时、容量与资源回收

Host 有默认策略，每个 Agent 可独立覆盖空闲、单次运行、断连保留和关闭宽限时间。
继承、禁用和有限时长分开表达；硬上限单独配置，超出时明确拒绝。未配置的超时不
默认启用，不从本次 600 规模推导生产容量或线程默认值。

- 请求模型、工具执行、重试和审批等待不是空闲；不能把没有输出当作空闲。
- 单次运行超时触发该 Agent 的取消与收尾，不能终止整个共享 Host；绝对截止时间
  不能因重启延长，子 Agent 不能延长父调用已经确定的取消期限。
- 登记、驻留、执行计数分别报告。容量限制原子预占，超限拒绝或显式排队，排队不算
  活动并发；降低上限不强杀已有 Agent。
- 普通空闲 Agent 可以 drain/close 后卸载，保留恢复清单；mini 回收丢失内存上下文。
- 事件与订阅者、工具输出、请求及缓存按数量和字节设预算。慢客户端必须背压或断开
  后重同步，不能在有界入口后再接无界队列；终止结果不静默丢失。
- 后台工具未返回时不能提前宣告安全关闭或释放执行归属；Host 崩溃后仍可能有旧工具
  存活，恢复必须防止重复执行与迟到回调串入新运行。

复用同一 Agent loop 与现有 TUI shell。mini 不支持的 Session/durable 能力在启动前
明确拒绝，不用 MemorySession 假装无 Session，也不复制第二套模型/工具循环。
本机 IPC、受理去重、重连快照/事件水位、审批控制权、独立权限与凭据绑定属于第一版
可靠闭环；自动发现和普通 CLI 路由切换在显式 Host 路径验收后接入。

### 服务层与隔离

生产共享宿主在 `crates/rpi-cli/src/host/` 内逐步增加后端、Provider、workspace、
事件与预算等模块。它承载一个 runtime 和共享服务，不重新发明 Agent loop，也不把
现有 child-Agent 的递归/轮次限制和 CLI 的 MAX_JOBS 扩展成其他规模的 Agent 调度器。

共享：

- RuntimeServices：异步 runtime、时钟、ID、关闭与诊断。
- Provider/HttpClient registries：按 endpoint、代理、TLS、认证和调用配置隔离的
  安全绑定；不能仅按 provider 名称共享带有不同凭据/请求头的实例。
- ResourceRegistry：版本化不可变技能、系统资源和只读定义；配置变更发布新快照。
- WorkspaceServices：真实 canonical 文件路径上的 mutation ordering；不同 Agent
  修改同一文件需要串行化，这是正确性要求，不是模型并发限流。
- SessionServices、EventRouter、BudgetManager、ToolExecutor：共享实现与基础设施，
  但 Session 数据、权限和取消归属不可串线。

独立：Agent ID、Harness、Session、当前运行、历史逻辑视图、工具绑定、权限、取消
与子任务、重试与 steering/follow-up、compaction 和恢复点。严禁通过修改进程全局
cwd 或 environment 模拟各 Agent 的执行环境。

工具定义可共享，但现有工具捕获 `ExecutionEnv`；不同 cwd/安全上下文的实例不能
直接复用。后续按 `ToolDefinition` / `ToolBinding` / `InvocationContext` 分层。
原生 cdylib 扩展须逐个审计线程安全、生命周期和 Session 状态；共享代码不是共享状态
安全性的证明。共享宿主不是进程级安全隔离；不可信扩展需要另行隔离。

### 执行与内存演进顺序

1. **P1 宿主/异步入口**：原生 async Provider-open 与同步兼容路径已实现。剩余为独立
   Harness 注册、关闭、取消与恢复，以及生产 RuntimeServices 的显式阻塞工作预算；
   不能把本 fixture 的 runtime 参数当作生产全局预算已经落地。继续用相同负载对比，
   不靠降低活动 Agent 数达标。
2. **P2 分配与持久化**：先按类型尺寸和匹配阶段定位，再优化；不能用不同模式的峰值
   差精确归因。已消除 `ToolExecutionScope::run` 的重复 future 存储（见前述实测），
   下一步继续检查 Session 上下文/事件与持久化帧的实际存活分配。按分配
   结果优化 Arc 快照、只读 message view、copy-on-write
   hooks、增量事件和受全局字节预算管理的缓存。保留 `PreparedJsonRequest` 的编码
   请求体共享，不能每次重试重新复制大 JSON。大请求按需落盘、可重放且 Content-Length
   兼容；small Bytes 快路径保留，并处理敏感数据清理、失败与磁盘满。
3. Session 继续使用独立权威 JSONL。后续旁路索引必须有界、可分页并可由 JSONL 重建；
   不能仅让正文变 cold，却允许所有历史元数据、事件或 UI 列表无限增长。
4. **全局预算**：精确区分流队列、当前完整消息、消费方持有数据、mapper 状态、编码请求、
   工具输出和 allocator 开销。现有每流事件缓冲上限不是 RSS cap。采用 eviction、spill、
   取消感知 backpressure 和明确容量错误，不将排队 Agent 假报为并发执行。
5. **P3 完整兼容与故障**：扩展/技能、慢订阅者、超大事件、重试、取消与 steering、
   compaction、磁盘满、进程崩溃恢复、子进程清理、HTTP/2 与 TLS。宿主崩溃会影响所有
   Agent，保留独立恢复边界；未知外部副作用不能盲目重试或承诺 exactly-once。

验收使用真实目标上下文/输出/工具负载、600/3000 服务端完整请求 barrier、工具内部
barrier（异步工具和真实 shell）、混合多轮、隔离与恢复测试，并区分宿主、客户端/
控制器与工具进程成本。3000 执行底座已跑通，但生产 Host 尚未按该规模验收；
3000 并发与目标负载下低于 1 GB 仍是生产目标，不能把探针 footprint 当作达标。
P0 是测量入口，in-process probe 成功也不等于生产 Host 服务和协议已经验收。
