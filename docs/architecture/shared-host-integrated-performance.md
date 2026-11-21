# main 集成后：0.27.0 执行底座 600-Agent 复测

## 口径与版本

- 独立工作树 `pi-rust-wt-host` / `feat/shared-host`，HEAD `59f985a998c9e2297b24d2675d720f43a9824d96`，
  **含本地未提交实现**，不是纯 upstream main 的性能数据。
- 新编译当前源码的 release `shared_host_probe`，不是复用旧二进制。构建用时 7 分 09 秒，
  不计入下表初始化与运行耗时。二进制 SHA256：
  `f1a121c546d9843a137922e53bf5145ebdfd76f60c501010ae5d12b3b2c6f3ae`。
- **测共享 Harness 执行底座，不经过新建 catalog/ordinary 后端、IPC、完整资源装配或 mini。**
  普通 Host 后端单独通过下述功能/稳定性测试，未在本轮用 600 个后端做性能测量。
- macOS arm64；600 个独立 Harness/JSONL Session；4 runtime workers、32 blocking threads；
  每个 context 参数 1024 bytes、历史 0 条；async 工具；本机真实 HTTP/SSE 合成响应，
  无 TLS/扩展，不代表任意 coding workload 或 600 个 bash 子进程。
- 控制器/fixture 不计入被测进程内存。仅在测试 shell/子进程将 FD 上限设为 8192。
- 内存使用 macOS `phys_footprint`，不是 RSS；1 MiB = 1,048,576 bytes。

完整摘要、配置、阶段证据与临时 artifacts 位置见
[JSON 记录](shared-host-integrated-performance.json)。旧版本报告保留，不覆盖或重标。

## 两轮实测

| 指标 | 第 1 轮 | 第 2 轮 |
| --- | ---: | ---: |
| 完整请求 barrier 同时在途 | 600 | 600 |
| 工具内部 barrier 同时存活 | 600 | 600 |
| 完成 Agent / 最终响应 / 工具结果 | 600 / 600 / 600 | 600 / 600 / 600 |
| HTTP 请求总数 | 1200 | 1200 |
| 初始化总耗时 | 8436 ms | 9045 ms |
| 其中 Session 创建 | 8397 ms | 8987 ms |
| 其中 Harness 创建 | 38 ms | 56 ms |
| 请求保持 footprint | 126.42 MiB | 128.73 MiB |
| 工具保持 footprint | 111.31 MiB | 111.42 MiB |
| 全程采样峰值 footprint | 158.72 MiB | 158.88 MiB |
| 关闭后 footprint | 91.73 MiB | 93.14 MiB |
| 峰值线程 | 38 | 38 |
| 峰值 FD | 1809 | 1809 |
| 关闭后 FD | 9 | 9 |
| fixture 错误 | 0 | 0 |

两轮均 `success=true`，探针 stderr 为空，运行后未发现遗留 `shared_host_probe` 进程。
`runMs` 分别 21415/24055 ms，包含人为 barrier 保持与控制器等待，不当作吞吐或响应 SLA。
关闭后仍有物理记账，不能仅凭这些样本认定泄漏或认定所有内存均归还系统。
采样峰值不是瞬时峰值严格上界，也不推广为所有 600-Agent 场景的统一内存上限。

### 与此前报告的关系

[旧基线两轮](shared-host-worktree-performance.md) 的初始化为 8035/8199 ms、峰值
159.52/156.92 MiB。本轮与旧记录不构成同轮匹配 A/B；初始化数值略高不能单凭两个
历史批次定性性能回退，内存接近也不能据此声称无回退。

本轮是**新建空历史 Session**，不测 prepared restore 的 stat/strict 校验，也不测长历史
中断恢复的 payload hydration，因此不能用它证明本次合入的历史恢复优化有多少收益。
其作用是确认当前集成代码仍能达到 600 请求/工具真实并发，并给出该受控负载的资源样本。

## 功能与稳定性回归

- Host **56 项连续 5 轮全通过**，共 280 次测试执行；其中每轮包含 15 项真实普通后端测试。
  覆盖实际工具并发、断连、精确恢复、deferred、writer 竞争、迟到结算与持久化故障。
- Harness 库 **509 通过、4 ignored**。
- Harness 定向集成 **88 通过**：deferred 8、durable cancellation 7、run E2E 62、
  writer lease 3（含独立进程）、strict restore 4、recovery execution contract 4。
- Python fixture **19 项全通过，无 skipped**。显式启用当前 release 二进制 smoke，
  包括 empty/idle/model/raw/async-tool/bash-tool 六模式。
- 测试期间 tracked 实现 patch 未变，本轮没有修改实现代码，只新增测试报告。
- 未跑完整 workspace、Linux/cgroup 或 Windows。3000-Agent 执行底座已在同一构建上
  单独复测，见 [3000 报告](shared-host-3000-performance.md)；它不是生产 Host 验收。

## 复现

在上述工作树内执行，输出目录必须不存在：

```bash
CARGO_TARGET_DIR="$PWD/target" cargo build \
  -p rpi-harness --example shared_host_probe --release --locked --offline -j 2
ulimit -n 8192
python3 scripts/bench-shared-host.py \
  --binary "$PWD/target/release/examples/shared_host_probe" \
  --output-root /tmp/rpi-integrated-tool-600-unique \
  --agents 600 --mode tool --tool-kind async \
  --workers 4 --max-blocking-threads 32 \
  --context-bytes 1024 --history-entries 0 --hold-seconds 2 --timeout 180
RPI_SHARED_HOST_BINARY="$PWD/target/release/examples/shared_host_probe" \
  python3 -m unittest discover -s scripts -p test_bench_shared_host.py
```

下一次完整 Host 性能验收仍须改为实际服务/ordinary 后端路径，计入其清单、监督、
绑定与故障处理成本，再使用相同 barrier 和明确的回收边界测量。3000-Agent 探针需要
`nofile` 至少约 10000；本轮在测试 shell 内使用 16384，见
[3000 报告](shared-host-3000-performance.md)。
