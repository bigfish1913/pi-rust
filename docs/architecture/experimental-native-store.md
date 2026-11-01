# 实验原生 SessionStorage/Repo：v3 选择性查询

## 状态与启用边界

第四期提交 `6d2c2b50` 引入默认关闭的 harness feature `experimental-native-store`，导出
`session::experimental_native::{ExperimentalNativeRepo, ExperimentalNativeStorage}`。
第五期第一步 `98fb646` 在相同显式接口增加 v3 查询索引及 UI summary/window。
第五期第二步增加默认关闭的 [CLI 显式 print opt-in](experimental-native-cli.md)，仅新建
text/json print session。**没有 Web 路由、默认 backend 替换、旧 session discovery 或迁移**。
直接 API 调用者仍须显式构造专用目录；只使用合成 fixture 验收，不是生产交付。

```bash
cargo test -p rpi-harness --features experimental-native-store --test experimental_native --locked --offline -- --test-threads=1
cargo test -p rpi-harness --features experimental-native-store --lib --locked --offline -- --test-threads=1
cargo test -p rpi-harness --test native_recovery_store --locked --offline -- --test-threads=1
```

## 共享事实引擎与格式

事实引擎位于 `src/session/experimental_native/engine.rs`；原 integration test support 通过
path wrapper 使用同一实现，避免复制 reducer。当前格式为独立
`rpi-native-fact-experimental-v3`、application_id `0x52504951`、user_version=3。
**拒绝第三期 v1 和第四期 v2；不自动升级**。第三/四期提交的测试及测量是历史证据，不能
当作 v3 结果。后缀仍为 `.rpinative`，不表示与旧实验版本兼容。

safe ASCII session ID 映射路径；拒绝明显 symlink/non-regular 文件。这不是针对恶意目录
替换的 sandbox，也不防同 identity 的整库旧备份回滚。checkpoint、固定预算 tail 与 lazy
body 校验继承事实引擎；auxiliary 中 metadata/progress 各带 identity-bound 摘要及 4 MiB
实验预算。摘要不是安全签名。

## 事务、异步与取消

- SQLite 工作在 `spawn_blocking`，history 写使用 BEGIN IMMEDIATE 后的 fresh projection。
- Store 单条 append 与 SessionStorage 批写共用 `insert_fact`：canonical fact、selection/tool
  索引、navigation、user summary 同事务插入，再发布 root/checkpoint。任一插入失败整体回滚。
- 批写、record+entry、record+entry+record、取消批次均单事务，失败不消耗 sequence。
- expected history revision 在事务内比较，不是先读后写。
- admitted blocking owner 持有 owned gate；调用者取消不取消已启动 mutation。
  `drain` 等待本 handle 组 gate，**不是跨进程 drain**。
- Repo create/open/list 的 owner 也持有生命周期 gate。create/fork 同事务初始化 schema、
  metadata、progress 与初始 facts；失败可留下不可打开的 artifact，不自动删除或覆盖残留。
- 同进程同路径 repo open 共用 inner，拒绝活动 handle 的不同 generation。
  全局 registry 只弱引用，不持有历史正文；不是跨进程 fencing。
- fork 显式全历史 materialize，使用真实 SessionState 生成 fork mutations；不复制 progress。
- delete 始终 fail closed：活动 handles 明确拒绝，其他情形也因尚无跨进程 lifecycle claim
  而报错。没有安全删除实现，不能宣称完整 Repo 生命周期已完成。

## SQL key selection 与读取预算

支持 lanes、get_entry、entry/branch/record 查询、record_page、open operations、log、name/label、stats。
selection 保存 type/custom/run/operation/queue/tool/outbox 等无正文查询 keys，并建立 SQL 索引。
普通 entry/record 查询先在 SQL 应用过滤、顺序、cursor 和 limit，再对命中项校验 canonical
metadata 摘要、镜像列及 tool keys，最后只 materialize 所选正文。
record_page 在相同读事务计算 COUNT 和所选 keys；get_log 先选 sequence 范围。
不再为了有 limit 的普通查询在 Rust 收集整份历史 metadata Vec。

这些不是任意查询常数成本承诺：无 limit 的 API 仍可返回全部命中项；SQL 组合谓词、COUNT、
B-tree、锁和物理 I/O 可能随历史增长。显式 branch/context 查询仍可扫描并保留整条路径，
fork 仍全量 materialize；active pending/open 查询随活动引用增长。checkpoint 恢复只解码
受预算约束的 tail 和 active references，不读取 retired bodies。

## UI summary / transcript window

三个分类保持分离：stats 的所有 Message、真正的 User message、window 的
Message 或 Compaction。custom message 即使 role 为 `user` 也不成为 user summary；
BranchSummary 不作为 transcript window 项。

- 每个 entry 有 digest-bound parent/nearest-user/previous-user/nearest-transcript/previous-transcript
  links，带 sequence 单调护栏；读取时校验自身与直接 parent metadata 和继承关系。
- 真正 user message 单独存储 preview/timestamp/has_images；共享 Memory 的预览函数，最多
  140 Unicode 字符、2048 字符扫描预算，不保存图片 payload。navigation/summary 每行 JSON
  最多 4096 bytes，超预算拒绝写入，不截断身份字段。
- summary 的正常读取不授权 canonical `facts.body`，从 links 选择用户 keys，仅解码该页摘要。
- window 的 anchor/before/after 互斥、limit 1..100、pinned leaf、ancestor/cursor membership、
  顺序及 has_more 与 Memory oracle 对照。只读取选中正文，不读取 lookahead 正文。
- 每页在一致读事务中校验 lane、root/checkpoint/tail、leaf、cursor 和所访问 links。
  深 pinned ancestor 验证或寻找深 cursor 仍可走长 metadata 链；保留空间限于页/小 ring，
  不宣称所有分页 CPU 或物理 I/O 恒定。
- 显式读取正文时重算 selection 和 user summary，与存储值对照；summary 读取自身不反查正文。

事务协议是正常写入一致性的来源。命中行校验、摘要及局部 links 校验**不证明任意外部 SQL
修改后的索引完备性**：被删掉/改成不匹配的旧 selection 行可能未被本次查询访问；恶意重算
摘要并连贯改写 links 可以制造遗漏。不在每页全局重扫 canonical facts，也不把损坏自动当作
有效空结果修复；被访问的损坏必须报错。校验并非安全签名、全局防篡改或 rollback fencing。

## 独立 progress

progress 与 history 使用独立 sequence，存同一数据库但不进入 immutable history；CommitStream
只允许历史 record，不可写入 transient progress。cleanup 仅按同 lane/run 的 durable finish
退休活动 payload。当前 progress 整体读写且受预算约束，不声称任意大活动状态可扩展。

## 验证范围与未完成工作

第四期既有回归保留：Memory/Session facade 对照、条件并发写、跨 repo handles、重开、批次
回滚、取消批次 pending/run 校验、progress 隔离及 terminal proof、fork 与 legacy 文件不变；
gate/channel 确定性取消测试、catalog/初始化/错误码、全部 entry/record 变种与 ACK 协议边界。

v3 新增跨分支/reopen/pinned leaf/cursor/limit 的 summary/window oracle 矩阵，覆盖 Unicode、
图片、custom user role、Compaction 与 BranchSummary 的分类。私有测试通过 SQLite authorizer
拒绝正文读取来验证 summary；损坏未选正文、未选旧 metadata、summary/links 和镜像 keys；
拒绝 UI 索引 INSERT 验证 fact/索引/navigation/root/sequence 一起回滚。
固定两项 latest page 在 32/512 条历史中比较 metadata point-read 的 prepare 次数：这是结构性
读取探针，不是物理页数、进程峰值内存、冷启动 benchmark 或完整恢复加速倍数。

新增 [隔离进程故障验证](experimental-native-process-tests.md)：真实 history/progress/prune
COMMIT 前后杀子进程、提交后返回前 ACK 丢失、跨进程 typed SQLITE_BUSY、持锁者 release/kill
后连续序号及 stale revision，另测错误阶段与 helper panic 不可假绿。只使用合成 fixture，
不新增 CLI 恢复、生产 fault hook 或迁移。现有取消测试仍为同进程 admission/owner 语义。
强制小 cache/2 MiB 正文变体验证原有 main bytes 已 spill、有效 journal 留存，kill 后
Store/repo.open/repo.list 首次访问完成恢复。由真实红测修正现有 repo 的只读 identity
probe：OS header gate 后允许 SQLite 恢复已初始化 v3，再做原有完整校验。list 可能回放
native journal，逻辑只读不等于物理无写；识别 v3 但内部坏的错误也不保证 bytes 不变。
foreign/旧 header 必须在 RW SQLite 前拒绝，并用真实 hot-journal 字节不变回归验证。

生产仍缺：完整错误分类及等价性矩阵、跨平台/更多故障点与真正断电验证、外部 generation fencing、
稳定格式 registry、request-ID 不确定提交对账封装、安全 delete、全面索引/上下文优化，
以及隔离旧数据测量和 idle/独占/备份/回滚迁移。当前 CLI 显式 opt-in 不提供恢复已有
session 或生产默认 backend 切换。
实验协议错误部分统一映射 StorageError；不能当作生产错误码兼容。

**生产 v4 JSONL 初次扫描未改变；总体目标尚未完成。**
