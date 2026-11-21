# Host worktree：600-Agent 执行底座复测

> **历史测量，不随代码升级重标。** 本文结果属于下述旧基线和旧执行底座探针。
> 工作树此后已合入 main `59f985a9`（0.27.0），并接入普通 Harness 后端；
> 新进展、review 发现及定向回归见 [集成记录](shared-host-main-integration.md)。
> 本文数据不是新版本 catalog/ordinary 后端、完整 Host 或 mini 的性能结果。

## 测量边界

- 开发位置：`pi-rust-wt-host`，分支 `feat/shared-host`，基于 `61198572` 加已转移的任务基线。
- 二进制：本 worktree 新编译的 `shared_host_probe`，release、thin LTO、16 codegen units。
- **这是接入 registry 之前的执行底座复测，不是生产 Host 或 mini 的性能验收。**
  探针不经过新建的 CLI Host catalog/registry、IPC、TUI 或完整资源装配。
- macOS arm64；600 个独立普通 Harness 与 JSONL Session；共享 Provider/HTTP client；
  HTTP/SSE 走本机真实连接，服务端返回合成内容；无 TLS、无扩展。
- 每个上下文参数 1024 bytes，历史条目为 0；4 runtime workers、32 blocking threads。
  后者仅是探针参数，不是生产默认值，也不是 Agent 执行槽数。
- 每轮从完整请求 barrier、工具内部 barrier 分别证明真实 600 并发，不把排队计入。
- fixture/controller 独立于被测进程，不计入下表；600 规模使用 async 工具，不含
  600 个 bash 子进程。另有 2-Agent bash smoke 校验进程与工具清理。
- 仅在 benchmark shell 及其子进程内把 FD 上限设为 8192，不修改系统/用户配置。

原始指标摘要与二进制 SHA256 见
[JSON 记录](shared-host-worktree-performance.json)，完整临时 artifacts 路径也记录在其中。

## 两轮结果

内存是 macOS `phys_footprint` 的阶段样本；1 MiB = 1,048,576 bytes。

| 指标 | 第 1 轮 | 第 2 轮 |
| --- | ---: | ---: |
| 完整请求同时在途 | 600 | 600 |
| 工具内部 barrier 同时存活 | 600 | 600 |
| HTTP 请求总数 | 1200 | 1200 |
| 最终响应 / 工具结果 / 独立 Session 校验 | 全部通过 | 全部通过 |
| 初始化总耗时 | 8035 ms | 8199 ms |
| 其中 Session 创建 | 8004 ms | 8169 ms |
| 其中 Harness 创建 | 30 ms | 29 ms |
| 请求保持阶段 footprint | 125.98 MiB | 126.22 MiB |
| 工具保持阶段 footprint | 112.91 MiB | 115.75 MiB |
| 全程采样峰值 footprint | 159.52 MiB | 156.92 MiB |
| 关闭后 footprint | 94.84 MiB | 95.80 MiB |
| 峰值线程 | 38 | 38 |
| 峰值 FD | 1809 | 1809 |
| 关闭后 FD | 9 | 9 |
| fixture 错误 / cleanup 错误 | 0 / 0 | 0 / 0 |

整轮 `runMs` 为 20474 / 21819 ms，其中含人为 barrier 保持与控制器等待，不能
据此计算真实模型响应延迟或服务吞吐 SLA。初始化约 99.6% 计入 Session 创建，
这只是阶段计时，不进一步推断其中磁盘同步、目录检查或解码的精确分配/耗时比例。

没有同一轮匹配的旧版对照，不宣称本次有性能提升或回退。采样峰值不是瞬时峰值的
严格上界。关闭后的 footprint 也不等于 live heap：不能仅凭残留物理记账认定泄漏，
也不能宣称全部内存已经归还系统。不把这个小上下文 fixture 推广为统一内存上限。

## 复现

在该 worktree 中执行：

```bash
cargo build -p rpi-harness --example shared_host_probe --release --locked --offline -j 2
ulimit -n 8192
python3 scripts/bench-shared-host.py \
  --binary "$PWD/target/release/examples/shared_host_probe" \
  --output-root /tmp/rpi-host-tool-600-unique \
  --agents 600 --mode tool --tool-kind async \
  --workers 4 --max-blocking-threads 32 \
  --context-bytes 1024 --history-entries 0 \
  --hold-seconds 2 --timeout 180
```

输出目录必须尚不存在；重复执行应更换路径。初次 release 编译耗时约 12 分钟，
不计入 Agent 初始化/执行测量。本轮没有运行 3000。

## 当时性能测试后完成的清单层（历史进度）

性能复测后完成了 `crates/rpi-cli/src/host/` 控制清单层：

- 单个专用 SQLite worker，有界邮箱、元数据字节上限与分页；不按 Agent 建 DB 线程。
- 私有目录/文件、当前用户检查、CLOEXEC 实例锁与跨进程互斥；不在关闭时 unlink 锁。
- WAL/FULL 提交，响应跟随提交；epoch/revision 守卫、Session 元数据引用去重。
- 原子重启分类；普通运行记录变中断，空闲变卸载，mini 变 Lost；不恢复执行内存。
- 持久化失败时回滚新预占，关闭写失败时不提前释放容量；客户端放弃等待不撤销提交。
- 清单关闭确认在 SQLite 和实例锁释放之后；clone 客户端断开不结束其他持有者。

验证：**Host 41 项、CLI 参数 41 项、启动诊断 6 项通过**。Python fixture **19 项通过**，
包含使用当前 release 二进制的 empty/idle/model/raw/async-tool/bash-tool 六模式 smoke。
这些是定向测试，未在本轮重跑完整 workspace。

清单目前只有元数据与管理 API，尚未连接真实后端、超时计时器、mini、IPC、输入
持久化受理/Request 收据或 CLI Host 命令。状态转换测试不是实际 Agent 运行证明。
后续先接入普通 Harness 后端与准确 Session 恢复，再做生产路径的 600-Agent 复测。
