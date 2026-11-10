# 原生 durable 与 session 选择性恢复

## 目标与当前边界

启动时只恢复继续执行所必需的状态，历史消息按游标分批读取，工具大正文按需读取；
不是把全量回放拆成几个批次后仍全部装进内存。最终恢复成本应主要随活动状态与
checkpoint 后增量增长，而非整个 session 的年龄增长。

当前第一期以 `preview` 的 progress companion/checkpoint 和 legacy frame 冷加载为前提。
**这一步没有实现 primary JSONL checkpoint/索引，也没有让 primary JSONL 初次扫描有界。**
不修改已有 session，不执行旧数据迁移，不替换正在运行的 Web/native host。

## 第一阶段实现

### 1. 原生 `rpi-durable` 基础

`rpi-harness -> rpi-durable -> serde/thiserror`，不反向依赖 harness、agent、plugin SDK、Web 或 ACP。

从 harness 提取既有活动 progress reducer，成为泛型 `ProgressState<F>` 与 `ProgressRecord`：
身份、batch 校验与序号分配、stream/run 清理、checkpoint 候选、已完成 run 退休、恢复验证。
Memory/JSONL/SQLite 继续使用同一个状态机；harness 的 `AssistantFrameRecord` 适配 trait。
这不是把整个业务插件搬进宿主，也没有创建新的 writer。

保留的契约：

- checkpoint 仍是 `sequence` / `frames`，frame 字段及 `assistant-progress-v1` 不变；
- payload 仍不进入 transcript、history sequence 或 record pagination；
- 存储成功 ACK 后才能更新 live state；候选发布失败不得伪报成功；
- 文件发布、poison/reopen、history 绑定、持久 terminal proof 仍由现有 backend/harness 负责；
- completed proof 按 lane/run 区分，不能用 UI 完成或模型结束代替；
- 不增加工具执行/replay、worker、scheduler，不保证外部副作用 exactly-once。

原生 crate 与外部 `rpi-package` 工作流插件目前同名，不能视为已经完成命名/升级迁移。
workspace 依赖与发布顺序元数据需要识别新依赖，但本改动不 bump、不 tag、不发布。

### 2. 选择性队列恢复

既有 legacy-frame 优化已让恢复只请求 `queue_enqueued` / `queue_cancelled`，而不是所有 records。
本次进一步把两次全队列查询与合并排序替换为一次：

```text
lane + record_type=queue_enqueued + pending_queue_only=true + oldestFirst
```

各 backend 在复制 payload **之前**过滤已取消、target 已写入 transcript 的 enqueue。
保留原来的 FIFO、消息结构验证与队列种类。一次读取也避免将不同时间点的 enqueue/cancel
结果在消费者中合并。`set_session` 仍先完整准备目标队列，读取失败保留旧 session/队列。

这减少 payload 分配和返回记录数，并不意味着当前内存 reducer 的扫描复杂度已经变成 O(pending)。
JSONL 热 state 的过滤仍可能扫描历史 queue metadata；索引属于后续阶段。

### 3. 数字化恢复诊断

设置 `RPI_SESSION_DIAGNOSTICS=1`（`0` 或未设置关闭）。成功阶段输出到 stderr：

```text
RPI_SESSION_DIAGNOSTICS phase=history_read elapsed_ns=... input=... output=...
RPI_SESSION_DIAGNOSTICS phase=history_replay elapsed_ns=... input=... output=...
RPI_SESSION_DIAGNOSTICS phase=queue_rebuild elapsed_ns=... input=... output=...
```

| phase | input | output | 时间范围 |
| --- | --- | --- | --- |
| history_read | UTF-8 bytes | UTF-8 bytes | 打开 reader、读取和 UTF-8 解码 |
| history_replay | 除 header 外物理行数 | 本次接受的 mutation 行数 | header/metadata 校验、parse/apply、cold 索引及必要 tail repair |
| queue_rebuild | backend 返回的 pending enqueue 数 | 成功重建的队列项数 | 选择性查询与消息解码 |

仅输出固定阶段名、时间和数量，不输出内容、文件路径、entry/session ID。
阶段缺失不能当作成功；错误路径可能只输出已成功的先前阶段。
这些计时不包括前端渲染、模型请求，也不代表完整首屏或端到端恢复时间。

## 验证方式

- `cargo test -p rpi-durable --locked --offline`：身份/操作/溢出/恢复校验、跨 lane/run/stream、候选与逐条应用等价；
  clone counter 证明退休 payload 不被复制、无退休项不生成候选。
- `cargo test -p rpi-harness --locked --offline --test selective_queue_recovery -- --test-threads=1`：
  三种 backend 的 FIFO、128 条已取消历史、已提交 target、跨 lane 同 target、session switch 与读失败。
  探针检查**生产调用发出的查询**及返回条数；全量/缺少 pending filter 的旧调用应失败，不能仅检查最终队列。
- harness adapter 的 golden JSON / error-code 测试固定旧格式；既有 progress store、retirement、failure、
  cold-frame、interrupted recovery 测试验证存储与 ACK 边界。
- 性能探针用合成 fixture，不以当前活跃 session 为实验对象；耗时对比应使用 release、相同输入，
  不在回归测试中断言脆弱的墙钟阈值。结构性 payload 数量减少不等于已测出完整恢复加速倍数。

## 合成队列选择探针

```bash
cargo test -p rpi-harness --release --locked --offline --test queue_recovery_cost -- --ignored --nocapture --test-threads=1
```

fixture 为 12,132,666 bytes，包含 4096 个已取消 enqueue、4096 条取消记录、4096 个冷 frame、
2 个 pending enqueue。比较旧的两次队列查询/合并/过滤与新的 backend pending 查询，
断言返回结果等价、原始文件未变；不是完整 harness 恢复或前端首屏 benchmark。

一次 Windows 本地验证，3 次独立进程运行，每次两条路径各 9 个交替顺序样本：

| 指标 | 旧选择路径 | 新选择路径 |
| --- | --- | --- |
| backend 返回记录数 | 8194 | 2 |
| 序列化查询返回量 | 7,130,168 bytes | 482 bytes |
| 各进程中位耗时范围 | 9.61–11.27 ms | 0.769–0.854 ms |
| 进程中位数的中位数 | 9.89 ms | 0.774 ms |

返回量不是进程峰值内存；fixture 刚写入、查询已预热，不能视为冷磁盘结果。
primary history 初次读取/解析依然存在，这组数值不能外推为整个 session 恢复加速倍数。

## 当前 Session storage 边界

旧 harness 内置的 SQLite Session backend、`experimental-native-store` feature 及其兼容测试已移除；本项目按新项目假设不提供旧格式迁移。`rpi-harness` 默认只构建 memory/JSONL Session storage。

Durable journal storage 属于 `rpi-durable`，默认也是 JSONL；需要 SQLite 时显式启用 `rpi-durable/sqlite`，或使用 harness 的 `durable-sqlite` 转发 feature。两者不是同一个 Session backend，durable SQLite 不会让默认 harness 构建引入 `rusqlite`。

当前 durable runtime/storage boundary 继续负责 journal 的顺序、revision CAS、actor/scope/admission/receipt/reconcile 等边界；Session facade 通过 adapter 使用该 boundary，不承担旧 SQLite 格式兼容职责。

## 后续阶段（生产路径尚未实现）

1. 在隔离旧 session 副本上分别测 load/replay、queue、interrupted repair、上下文准备、首屏 history。
2. 定义 primary recovery checkpoint 的 session/branch/generation/序号边界，以及可验证的索引与有界 tail。
   checkpoint 只装恢复必需状态，不把所有历史 payload 再复制一遍。
3. 将底层历史正文读取与 UI 分页/模型上下文解耦；现有 summary/window API 不代表所有 backend 已避免全量初读。
4. 明确读写代际和发布顺序，覆盖 stale checkpoint、缺失 companion、乱序/重复、分支切换、写入不确定及取消。
5. 显式迁移旧布局：idle、独占写入、完整备份、副本验证、可回滚；禁止活跃源文件删 frame 或重编号。
6. 再迁入 workflow/manual work queue；native run/tool 事实为权威，插件成为兼容入口，始终单一 writer。
