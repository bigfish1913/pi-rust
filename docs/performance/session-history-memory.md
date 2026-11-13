# JSONL 长历史内存实测

## 范围与方法

2026-11-02，Windows `x86_64-pc-windows-msvc`，rustc `1.99.1 (58cf8f9c3)`，release 构建。基线为 `3a49e3036d517fbf3cce2a349ce1239e7270650a` 的 eager 实现加同一个合成 load probe；优化版为此工作树的 streaming/cold-payload 实现。未使用真实 sessions、配置或 provider，没有迁移数据。

`jsonl_history_probe` 仅使用带专用 marker 的合成根目录：`generate` 拒绝已存在目录，`load`/`exercise` 验证 marker，exercise 另有防重入标记。fixture 为 V4 JSONL，包含链式普通 user messages；只有文件 header 中临时 cwd 路径长度相差少量字节，条目数和正文大小相同。

三个 load case 各使用独立 timing child 和 memory child。load timer 仅覆盖加载；在输出 ready 前额外读取 5 条，进程继续持有 storage/page 供采样。内存来自 Windows `GetProcessMemoryInfo` 的 WorkingSet/PeakWorkingSet/PrivateUsage，另在静置 250 ms 后采样一次；不 trim heap。计时与内存分进程，避免 sampler 与被计时加载竞争。以下为各一次本机测量，不是跨机器保证或统计显著性结论，也不是 TUI 整进程 RSS。

## 加载结果

MiB = 1,048,576 bytes。

| 合成历史 | ready RSS：基线 → 优化 | peak RSS：基线 → 优化 | load：基线 → 优化 |
|---|---:|---:|---:|
| 512 × 8 KiB 正文 | 11.88 → 8.02 MiB | 15.89 → 9.73 MiB | 7.57 → 10.42 ms |
| 512 × 128 KiB 正文（64 MiB payload） | 72.47 → 8.54 MiB | 136.25 → 9.86 MiB | 74.98 → 183.52 ms |
| 4096 × 8 KiB 正文（32 MiB payload） | 45.76 → 11.61 MiB | 78.40 → 13.70 MiB | 58.11 → 73.24 ms |

**内存明显下降，但本轮加载更慢。** 最大正文 case 的 load 约为原来的 2.45 倍。不能把此更改宣传为启动加速；按行冷引用和验证等工作引入额外成本，尚未进行 CPU 成本归因。仍需全扫历史，也没有新增持久索引协议。

同样 512 条、正文从 8 KiB 增到 128 KiB 时，优化版 ready RSS 仅从约 8.02 增到 8.54 MiB；增大到 4096 条时则升至约 11.61 MiB，符合索引随条目数增长，而不是总内存恒定。

原始数据：
- [baseline](data/jsonl-history-20261102-baseline.json)
- [lazy](data/jsonl-history-20261102-lazy.json)

## 翻页与连续 append

另一个独立 child 从 512 × 128 KiB 开始，不保留前述 ready 的 5 条 page。使用默认 50 条 / 4 MiB 页预算，三轮 older/newer/latest，共读取 102 页，返回 page 随即释放；随后分八批追加 512 × 128 KiB，即再增加 64 MiB 正文。每次 append 的返回 Entry 随即释放，各阶段 flush 后暂停让外部进程采样；不清 allocator。

| 阶段 | 消息数 | 当前 RSS | 累计 peak RSS |
|---|---:|---:|---:|
| 加载后 | 512 | 7.86 MiB | 9.84 MiB |
| 翻阅 102 页后 | 512 | 10.70 MiB | 13.03 MiB |
| 再追加 64 MiB 后 | 1024 | 9.91 MiB | 13.03 MiB |

翻页原始操作计时合计约 736 ms；512 次 append 合计约 2161 ms，不含阶段暂停。这些 exercise 计时可能受 sampler 调度影响，仅为诊断数据，不是性能门禁。结构性测试另外验证 cold payload、pending 清空及同 generation reader 共享，避免以不稳定 RSS 阈值替代正确性测试。

数据见 [exercise 原始结果](data/jsonl-history-20261102-exercise.json)。这不包含真实 TUI 图片/markdown 组件成本，也不能推断多小时运行、任意最大行或任意活跃操作的 RSS 上限。

## 复现

在独立测试环境中保留现有 Cargo cache，设置新的合成输出目录。此项目开发环境还通过 `CARGO_BUILD_JOBS=2`、`CARGO_INCREMENTAL=0`、显式共享 target/build-dir 控制构建资源；不要删除共享缓存或替换活跃宿主。

```bash
cargo build -p rpi-harness --release --example jsonl_history_probe --locked --offline
python scripts/bench-session-history.py \
  --binary <CARGO_TARGET_DIR>/release/examples/jsonl_history_probe.exe \
  --output-root <new-synthetic-directory> --label lazy-jsonl --exercise
```

Linux 可使用无 `.exe` 的 binary，runner 采 `/proc` VmRSS/VmHWM；**本报告没有 Linux 实测结果**。基线 executable 已独立保存，不能用当前实现重复构建却标成旧基线。

用于本次记录的 executable SHA-256：
- baseline：`74fee06599d89235999890590764733d5a791ebdaf9f989ec29ef6521a99f174`
- verified lazy：`b4d3213bb55d0543f9dcdd333eb3385b7414f914e1249dccb9aa927526ffe170`

## 正确性验证

全部 Cargo 验证使用隔离 HOME/config/session 根、`--locked --offline`，测试串行运行；合成测试使用内存/临时文件和 faux/mock provider。

- 默认 `cargo test --workspace`：**2984 passed / 0 failed / 10 ignored**。
- harness 默认构建使用 memory/JSONL；durable SQLite 仅通过 `rpi-durable/sqlite`（或 harness 的 `durable-sqlite`）显式启用。
- `cargo check --workspace --all-targets`，同上 feature：通过。
- 正常 Clippy 被未修改的 `crates/rpi-extensions/src/actions.rs:986` 的 `not_unsafe_ptr_arg_deref` deny 阻塞；`--cap-lints warn` 的 advisory 全 workspace/all-targets 检查完成，仍有 warnings，**不是 lint-clean**。
- focused tests 包含 shared trait contract、source generation/篡改检测、跨块 Unicode/CRLF、大行、duplicate/rechain/torn-tail、兼容 filesystem、失败 append/重试、长期 append、compaction context/intent、snapshot、fork、TUI 翻页/旧引用释放/失败保留/过期结果/active 分离/当前面板/header。

默认仍是 JSONL；实验后端仍限显式新建 print。没有 push、发布、版本变更、真实 session 迁移或活跃宿主替换。实现边界见 [架构说明](../architecture/session-history-paging.md)。
