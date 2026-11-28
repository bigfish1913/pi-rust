# main 集成后：0.27.0 执行底座 3000-Agent 复测

## 口径与版本

- 独立工作树 `pi-rust-wt-host` / `feat/shared-host`，HEAD
  `59f985a998c9e2297b24d2675d720f43a9824d96`，**含本地未提交实现**。
- 与 [600-Agent 复测](shared-host-integrated-performance.md) 使用同一份当前源码和同一
  release 二进制，SHA256：
  `f1a121c546d9843a137922e53bf5145ebdfd76f60c501010ae5d12b3b2c6f3ae`。
- **测共享 Harness 执行底座，不经过 catalog/ordinary 后端、IPC、完整资源装配或 mini。**
  不是生产 Host 的性能验收。
- macOS arm64、16 GiB 内存；3000 个独立 Harness/JSONL Session；4 runtime workers、
  32 blocking threads；async 工具；context 参数 1024 bytes、历史 0 条；本机真实
  HTTP/1.1 loopback 合成响应，无 TLS/扩展。不是 3000 个 bash 子进程，也不代表任意
  coding workload。
- 测试 shell 内将 `nofile` 设为 **16384**；两轮结果均记录 `[16384, 16384]`。本机
  `kern.maxfilesperproc=61440`、`kern.ipc.somaxconn=128`。3000 连接仍无 fixture 错误，
  但不能据此认为其他机器或更高并发都不受 backlog/FD 限制。
- 控制器/fixture 不计入被测进程内存。内存使用 macOS `phys_footprint`；1 MiB =
  1,048,576 bytes。完整配置、阶段证据与临时 artifacts 见
  [JSON 记录](shared-host-3000-performance.json)。

## 两轮实测

| 指标 | 第 1 轮 | 第 2 轮 |
| --- | ---: | ---: |
| Agent 数 | 3000 | 3000 |
| 完整请求 barrier 同时在途 | 3000 | 3000 |
| 工具内部 barrier 同时存活 | 3000 | 3000 |
| 完成 Agent / 最终响应 / 工具结果 | 3000 / 3000 / 3000 | 3000 / 3000 / 3000 |
| HTTP 请求总数 | 6000 | 6000 |
| fixture 错误 | 0 | 0 |
| 初始化总耗时 | 63.60 s | 67.86 s |
| 其中 Session 创建 | 63.43 s | 67.64 s |
| 其中 Harness 创建 | 159 ms | 215 ms |
| 请求保持 footprint | 550.74 MiB | 558.92 MiB |
| 工具保持 footprint | 511.58 MiB | 511.66 MiB |
| 全程采样峰值 footprint | 661.75 MiB | 657.20 MiB |
| 关闭后 footprint | 192.30 MiB | 188.13 MiB |
| 峰值线程 | 38 | 38 |
| 峰值 FD | 9009 | 9009 |
| 关闭后 FD | 9 | 9 |
| `runMs` / 关闭时 `runMs` | 76.74 / 78.92 s | 85.68 / 87.85 s |

两轮均 `success=true`，探针 stderr 为空，运行后未发现遗留 `shared_host_probe` 进程。
`runMs` 包含 ready、请求保持、工具保持和结算阶段的各 2 秒人为等待，不能当作吞吐、
模型响应延迟或服务 SLA。

### 与 600-Agent 结果的关系

| 指标 | 600（两轮） | 3000（两轮） |
| --- | ---: | ---: |
| 请求保持 footprint | 126.42 / 128.73 MiB | 550.74 / 558.92 MiB |
| 工具保持 footprint | 111.31 / 111.42 MiB | 511.58 / 511.66 MiB |
| 采样峰值 footprint | 158.72 / 158.88 MiB | 661.75 / 657.20 MiB |
| 峰值 FD | 1809 | 9009 |

FD 基本符合每 Agent 3 个连接槽加 9 个基础 FD；3000 的请求/工具保持 footprint
约为 600 的 4.3–4.6 倍，而 Agent 数为 5 倍。这个结果**不能**证明线性扩展、效率提升或
统一内存上限：上下文仅 1024 bytes、无历史、无扩展，且采样峰值不是严格瞬时上界。

初始化也不是线性增长：600 约 8.4–9.0 s，3000 约 63.6–67.9 s，耗时仍由顺序创建
JSONL Session 主导。它是探针阶段计时，不是生产 Host 启动 SLA；不能据此单独推断
磁盘同步、目录检查或解码的精确分配/耗时比例，也没有同轮匹配的旧版本 A/B。

## 边界

- 3000 是当前探针验证到的规模，不自动证明 5000、10000 或生产 Host 同样可行。
- 本测试不经过 `HostCatalog` 的 64 槽命令入口、驻留容量、ordinary supervisor、
  绑定预检、关闭对账或 IPC；这些路径的成本未计入上述内存、FD 和耗时。
- 空历史 Session 不测 prepared restore 的 stat/strict 校验，也不测长历史中断恢复的
  payload hydration，因此不能证明本次合入的历史恢复优化在 3000 规模下的收益。
- 关闭后仍有物理记账，不能仅凭 footprint 判断泄漏或认定全部内存已归还系统。
- 未跑完整 workspace、Linux/cgroup、Windows、3000 个真实编码任务或 3000 个 bash 工具。

## 复现

在上述工作树内执行，输出目录必须尚不存在：

```bash
CARGO_TARGET_DIR="$PWD/target" cargo build \
  -p rpi-harness --example shared_host_probe --release --locked --offline -j 2

ulimit -n 16384
python3 scripts/bench-shared-host.py \
  --binary "$PWD/target/release/examples/shared_host_probe" \
  --output-root /tmp/rpi-integrated-tool-3000-unique \
  --agents 3000 --mode tool --tool-kind async \
  --workers 4 --max-blocking-threads 32 \
  --context-bytes 1024 --history-entries 0 \
  --hold-seconds 2 --timeout 300
```

本轮输出目录：
`/tmp/rpi-host-3000-8RE8Xs/tool-3000` 与 `/tmp/rpi-host-3000-8RE8Xs/tool-3000-2`。

下一次生产 Host 性能验收必须改为实际服务/ordinary 后端路径，计入清单、监督、绑定
与故障对账成本，并用相同 barrier 和明确的回收边界重新测量。
