# 第三期：真实原生事实的隔离恢复投影

## 状态

> 本页记录第三期提交 `79606147` 的历史设计与验证。第四期将同源引擎移到 feature-gated
> 实验模块并使用独立 v2 格式；当前 checkout 的 support 是共享 wrapper，不再创建下述 v1。
> 参见 [第四期实验后端边界](experimental-native-store.md)，不要把历史测试数值当作新格式测量。

基于第二期 `1a80247`，仍为 integration-test-only，**未接入任何 production SessionStorage、
repo、CLI 或宿主启动路径**，也不创建或迁移真实 session。

- `crates/rpi-harness/tests/native_recovery_store.rs`
- `crates/rpi-harness/tests/support/native_recovery_store.rs`

本阶段不再用第二期自定义 `Action` 代表原生事件，而直接接受真实 `SessionMutation`，
用真实 `SessionState` 作为逐步对照 oracle；现有 JSONL codec 负责 canonical typed payload。
不新增依赖、Cargo feature 或公开 API。第二期原型保持不变，提供先前的事务/并发故障证据。

## 独立格式与存储

实验标识 `rpi-native-fact-prototype-v1`，SQLite application_id `0x5250494e`、user_version=1，
不与第二期的 `rpi-managed-recovery-prototype-v1` 混用，也不是正式发布格式。
create_new 不覆盖既有文件；open 不创建缺失文件。格式/session/generation 均需匹配。

`facts` 每行拥有：全局 sequence、可空的唯一 entry/record ID、lane、category、target、
无正文 metadata 与其摘要、canonical JSONL mutation body 与其摘要。
`Lane`/`FactName`/`FactLabel` 没有强造假的 mutation ID，以 sequence 标识事实。
ID 的唯一约束同时覆盖 entry 和 record，而非仅各类型内部唯一。

root revision、facts 与 checkpoint 在同一个 SQLite transaction 中提交。
checkpoint 后 metadata tail 的阈值仍为 16；超过预算、缺行、逆序或最新 history sequence
与 root 不一致时拒绝，不做全量扫描回退或静默 rewrite。读取用一致快照，写入用
BEGIN IMMEDIATE 后的最新投影和可选 expected revision，不依赖旧 handle 的缓存。

metadata 上限 16 KiB，checkpoint 256 KiB，单个 canonical mutation 4 MiB；这些只是实验预算。
metadata/index/root/digest 字段在返回 Rust 前有长度护栏。超预算导致事务失败，不丢弃活动引用。
存在性、历史取消、parent/lane target 通过唯一或复合 SQL 索引查询；不恢复全历史 used-ID 集合。

## 与真实模型的语义对齐

投影通过 exhaustive match 覆盖 5 个 SessionMutation、7 个 Entry、11 个 LaneRecord 变种。
全部正文仍可显式还原为真实 typed mutation；projection 不复制以下 payload：
operation prompt/intent、tool effective_args、queue target 消息、frame、compaction summary、
custom data、label/name 文本等。

| 事实 | 投影/索引处理 |
| --- | --- |
| Entry（所有类型） | 校验 ID、parent、可选 lane chaining；更新 lane head，所有 lane 的同 target pending 退休；message 单独累计 message_count |
| Entry lane=None | 保留 fork 原始 parent/ID，校验 parent 存在，但不更新任何 lane head |
| Lane | upsert，包括已存在 lane 的 move；target 可指向任何已有 Entry |
| OperationStarted | 保存 lane/sequence/ID 引用，允许多个以便上层发现 corruption |
| OperationFinished | 只按同 lane/run 移除 open 引用；未知 run 不在 storage 层擅自拒绝 |
| QueueEnqueued | 保留全部 pending enqueue 引用，不按 target 去重，四种真实 QueueKind 均保留在 canonical payload |
| QueueCancelled | 同 lane/target 全部排除；持久索引让“先取消、后 enqueue”仍遵循现有 pending 查询的历史排除语义 |
| Queue target | 缺少字符串 ID 时与现有 pending 查询一致：存储事实，但不返回为 pending；任何 Entry 已提交都排除，而非只看 message |
| Usage | 按现有 stats 规则累加 cache_read、input+cache_write、total_tokens、cost.total；检查整数溢出和非有限 cost |
| Abort/Step/Tool/Deferred/Frame/Retry | 各自显式 metadata 类别，完整 canonical payload 保留，不能 wildcard 静默漏掉新变种 |
| FactName | checkpoint 只保留 latest fact 的 sequence 引用，显式读取时再取文本 |
| FactLabel | 不积累到启动 checkpoint；按 category/target/sequence 索引懒取最新 label，含清除事实 |

这不把 SessionState 等同于 operation validator。`validate_record_log` 对 multiple open、
unknown operation、attempt/tool/queue 等的矛盾判断仍由真实上层执行。
测试证明 multiple-open 和 nonconsecutive-attempt 的 corruption 分类保持；新存储不执行工具。
历史 `assistant_frame` 是本阶段可存的真实历史 record；**不代表独立 active-progress 序号域已经接入**。

原生 Entry 的 envelope seq/timestamp 与内嵌 EntryBase 必须一致，且要求现有 codec round-trip
回相同 typed mutation。这比直接调用旧 SessionState 更严格：旧 reducer 没有检查该 envelope
一致性。不兼容的 malformed/溢出输入仅在实验 API 拒绝，不改变任何生产错误码或接纳策略。

## Checkpoint 引用校验与懒加载

checkpoint 只含 seq、lane heads、open-operation 引用、pending-enqueue 引用、name-fact 引用和 stats。
恢复在 checkpoint fence 内用索引 point metadata 校验：

- heads 必须指向已存在 Entry；open 引用必须是对应 lane/ID 的 start，且 fence 内未 finish；
- pending 引用必须是对应 lane/ID/target 的 enqueue，且 fence 内没有 target Entry 或取消；
- 引用序号有序、去重、不越过 fence，引用 lane 必须存在；name 必须指向 name fact。

metadata 摘要绑定 format/session/generation/seq，读取时还验证索引列与 metadata 一致。
显式读取 body 才验证 body 摘要、parse typed mutation，以及重新提取的 metadata 与存储 metadata
完全一致；因此旧 body 损坏可能在恢复成功后、用户真正请求时才报告，不可声称启动校验过所有正文。

摘要不是签名。校验不能证明攻击者重写且重新计算摘要后 projection 的**完备性**，也不在启动时
重算全历史 stats 或查找被恶意从 checkpoint 删掉的活动引用。事务协议是正常写入一致性的来源，
不是任意外部 SQL 修改后的无限自修复机制。完整数据库回滚到同 identity 的旧副本仍需外部代际协议。

应用层 metadata 解码随 active refs 和固定 tail 增长；SQL B-tree 定位、journal recovery、锁等待、
物理页 I/O 和活动状态自身增长均不保证常数。这里不是完整宿主恢复或模型上下文性能承诺。

## ACK 与内容对账

COMMIT 成功才返回存储 ACK，precommit 错误和 SQL checkpoint 拒绝都整体回滚。
模拟 COMMIT 后 ACK 丢失则返回 uncertain；reopen 后的 `reconcile(request)` 不只检查 ID：
有 ID 时找同 ID 的事实，无 ID 的 facts 按 sequence 找，再比较完整 typed mutation。
相同 ID 但不同正文、不同 sequence、不同 name/label 内容必须拒绝，不能当作重复成功。
当前对账接口要求重试者保留 stamped request；尚无生产 request-ID/序号分配封装。
存储 ACK 不等于事件已送达或 workflow 已结算，不提供外部副作用 exactly-once 或 fsync/断电承诺。

## 验证与结构性证据

```bash
cargo test -p rpi-harness --locked --offline --test native_recovery_store -- --nocapture --test-threads=1
```

测试逐条与真实 SessionState 比较 heads、pending FIFO（包括重复target）、open newest-first、
stats、name/labels；跨 checkpoint 重开后继续比较。所有 entry/record/mutation 变种均有 fixture。
另覆盖真实 validator corruption、错误引用（即使重算 checkpoint 摘要）、body/metadata 不一致、
正文延迟损坏、tail/index/root/version 损坏、ID内容冲突对账、stale handle、事务失败和拒绝覆盖。

合成真实 typed fixture，历史 message 正文各 16 KiB，并包含大 operation prompt、pending queue、
大量大 label fact：

| history rows | checkpoint bytes | tail rows | tail metadata bytes | checkpoint 引用 metadata rows |
| --- | --- | --- | --- | --- |
| 146 | 409 | 2 | 188 | 3 |
| 2066 | 413 | 2 | 190 | 3 |

3 个引用分别是 head、活动 run、pending enqueue。数字位数引起几个 bytes 的变化，
大 label 文本和已退休 history 不装入 checkpoint。恢复期间 SQLite authorizer **禁止 facts.body**，
显式正文读取必须实际失败；解除 guard 后 active payload 才可 materialize。
临时把 tail SQL 改为选择 body 会让规模探针精确失败，恢复原始源码后再跑最终绿色测试。
这些是应用层选择性读取证据，不是磁盘页统计、峰值内存或整体恢复倍数。

## 尚未交付的生产能力

- 完整 SessionStorage/Repo、branch/context/summary 查询和 run/tool 查询索引；当前 explicit materialization
  仅提供 native mutation、pending/open、name/label，不模拟完整宿主接口。
- async/nonblocking、取消后的 detached mutation owner、生产错误码、跨进程 crash/fault、request-ID 协议。
- 独立 active progress 与 terminal proof、宿主恢复执行路径、UI/模型上下文适配。
- 正式文件格式 registry/旧 writer 拒绝、create 失败残留清理、完整备份/回滚、idle独占迁移。
- 外部同名 durable plugin/workflow 的所有权与发布协调。

**生产 v4 JSONL 仍然全量初扫；第三期不宣称总体目标完成。**
