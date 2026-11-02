# 版本化 managed recovery 原型（仅隔离测试）

## 状态与非目标

用户授权的第二期原型，基于第一期本地提交 `2ac1cc3`。实现位于：

- `crates/rpi-harness/tests/managed_recovery_prototype.rs`
- `crates/rpi-harness/tests/support/managed_recovery_prototype.rs`

**不是 production SessionStorage，不自动创建、发现、升级或迁移任何真实 session。**
仅 integration test 可达；fixture 全部来自 tempdir。现有 JSONL v4、snapshot SQLite、
CLI/Web/ACP、active progress 和外部 workflow 插件路径均不改变。
唯一依赖调整是现有 rusqlite 的 dev-only `hooks` feature，用于 SQL 访问授权负向探针；
不增加 production dependency，不修改 lockfile、版本或发布顺序。

本原型选择独立 SQLite 行存储，而不是给 v4 JSONL 增加“不验证 prefix 的恢复缓存”。
v4 缺少跨重启可验证的不可变 prefix 契约；长度、mtime、末尾摘要不足以证明旧内容未改，
全前缀 hash 校验又仍是全量 I/O。现有 SQLite backend 依然读取完整 `state_json`，
其 snapshot 还不能完整表示 JSONL wrapper 保留的冷 frame ID/位置，不直接复用。

## 文件与一致性边界

实验 artifact 后缀 `.rpidb`，SQLite `application_id=0x52504950`、`user_version=1`，
格式标识 `rpi-managed-recovery-prototype-v1`；不是新的公开稳定格式或正式 JSONL v5。
旧 JSONL reader 不能把 SQLite 文件当作可追加的 v4 JSONL。
正式接入还必须给所有 repo 的格式路由/拒绝误写加保护；application_id 本身不会阻止任意旧 SQL writer。

三个表在同一个 SQLite 数据库和同一个事务中提交：

| 表 | 内容与用途 |
| --- | --- |
| store_meta | 唯一格式/session/generation/revision 根；prototype generation 由创建者显式提供 |
| history | append-only 规范行：sequence、唯一 mutation ID、lane/type/target 索引、metadata JSON/摘要、opaque body/摘要 |
| checkpoint | 唯一恢复投影与摘要；包含格式及 session/generation 绑定 |

history 主键按 sequence；mutation ID 唯一；另有 `(lane,type,sequence)`、
`(target,type,sequence)` 索引及 `(lane,target)` enqueue 唯一约束。
正文不复制到 checkpoint 或另一张 tail 表；tail 是 checkpoint sequence 之后的 metadata 索引范围。
已覆盖 history 不删除，不重编号。索引用来查旧 ID/entry，不在恢复时重建全历史 HashSet。

协议假设所有正常写入都遵守这个事务协议。摘要用于绑定与意外损坏检测，不是签名或
防恶意改库的安全边界；不在启动时重新验证所有旧历史。整个数据库回滚到同 identity 的
旧备份，也不能仅靠库内 generation 发现，需要未来的外部发布/备份协议。

## 恢复投影与范围限制

checkpoint 只保存：sequence、lane heads、活动 operation ID、pending enqueue 引用
（enqueue ID、target ID、queue kind、FIFO sequence）。不含任何消息/工具/队列正文。

原型支持的 metadata 子集：create/move lane、message、enqueue/cancel、operation start/finish。
它不是现有完整 SessionMutation codec：尚不含 tool/write/retry/usage、labels/name/stats、
compaction 与模型上下文、独立 assistant progress 等事实。
同 lane/target 只允许一条 enqueue；允许多个活动 operation 引用以便上层发现不一致，
不把“恢复得到了多个 operation”当作可以执行多个 worker 的许可。
提交 message 会按全局 target identity 清除各 lane 的 pending；cancel 只影响所属 lane。
历史 entry 是否存在，通过唯一 ID 索引查询，不把所有 committed target 留在 checkpoint。

实验硬上限：tail 16 条、单条 metadata 16 KiB、checkpoint 128 KiB、单个 body 256 KiB、
identity/reference 128 UTF-8 bytes。正常第 16 条 append 同事务生成新 checkpoint，故通常 tail <16。
这些是验证预算的实验参数，不是已经确定的用户数据配额。
超过 active-state checkpoint 预算时整次事务失败，不能静默截断活动状态。

## 提交顺序、ACK 与恢复

每次 append：

1. `BEGIN IMMEDIATE`，从同一事务读取最新 root/checkpoint/tail；不信任 handle 的陈旧内存状态。
2. 校验 expected revision（如果提供）、身份、parent/lane、连续 sequence；通过索引验证旧 entry 引用。
3. INSERT history（含 metadata/body 摘要）；重复 ID/queue target 由唯一索引拒绝。
4. 更新 revision；达到 tail 阈值则同事务更新 checkpoint。
5. COMMIT 成功才给存储 ACK；此前任意错误由 transaction drop 回滚整次更改。

COMMIT 后丢 ACK 是“不确定”，不是证明未落盘。reopen 后按 mutation ID 对账，避免把同一逻辑
mutation 再写一遍；本原型没有执行工具、事件分发、异步 detached owner 或副作用重试 API。
存储 ACK 不等于上层事件已发出或逻辑 workflow 已结算。不承诺 fsync/断电原子性或外部 exactly-once。

恢复使用一致读事务：先校验 root，再校验 checkpoint 摘要/结构/identity，拒绝逆序/超预算 fence；
通过主键查最新 sequence，随后只查询 checkpoint 后最多 17 行 metadata，严格回放至 root revision。
缺行、改摘要、未知格式、错 session/generation、缺 checkpoint、超预算均失败，不静默扫描全历史补救。
失败不会执行应用层 rewrite/repair；SQLite 自身的事务日志恢复不等于应用层迁移。

这里的有界性是 **Rust 应用层解码/返回量随活动状态与固定 tail 预算增长**，不是所有物理 I/O
都为常数：B-tree 定位、SQLite journal 恢复/锁等待、页缓存和文件体积仍影响成本。
活动状态本身也可能增长。checkpoint 摘要不是“已经从 canonical prefix 重放验证过所有引用”的证明。

## 历史正文与分页

`page(lane, after_seq, limit)`：按 lane/type/sequence 索引返回至多 128 个 message ID，
不读 body。`body(id)`：显式读取单条 opaque bytes，并验证 session/generation/sequence/ID 绑定摘要。
正文坏掉可以在启动恢复成功后、真正请求正文时才暴露；这就是懒加载的检测时机变化，
不能把启动成功解释为全历史正文完整性已验证。尚未实现生产 summary/UI 游标或完整跨分支 context API。

## 验证与故障矩阵

```bash
cargo test -p rpi-harness --locked --offline --test managed_recovery_prototype -- --nocapture --test-threads=1
```

- checkpoint+tail 每一步都与从头应用同一 metadata 序列的参考投影比较，覆盖多 lane、branch head、
  cancel/committed target、pending FIFO sequence、start/finish 和 checkpoint 跨界。
- after-history / checkpoint 更新后 / commit 前错误必须回滚 history/root/checkpoint；ACK 丢失后按 ID 对账。
- 两个独立 handle 的陈旧 revision 拒绝；两个真实并发线程无 expected revision 时均提交，
  同 expected revision 时只有一个成功，另一个必须是 revision conflict。
- 拒绝错误格式/身份、缺失文件、重复 ID/queue target、错误 lane/parent/run、超预算正文、
  checkpoint/tail 摘要损坏、缺行、超大 metadata/checkpoint/root、checkpoint identity 置换。
- create 使用文件系统 create_new，已有文件不得覆盖；失败 open 不做应用层修复。
- 历史分页无重叠/遗漏、lane 隔离；旧正文损坏只在显式 body 请求时报错。
- SQLite authorizer 在恢复/分页期间拒绝所有 history.body 访问，显式 body 读取此时必须失败；
  解除后才能成功。这比单独断言一个“body bytes=0”计数器更强。

合成 fixture 的应用层读量（并非完整恢复速度测量）：

| history 行数 | 每条历史 message 正文 | checkpoint bytes | tail rows | tail metadata bytes | 解码正文 |
| --- | --- | --- | --- | --- | --- |
| 130 | 16 KiB | 204 | 2 | 169 | 0 |
| 2050 | 16 KiB | 205 | 2 | 169 | 0 |

两个 fixture 活动形状相同；checkpoint 的 1-byte 差别来自 sequence 十进制位数，
不是历史正文驻留。计数不包含 SQLite 内部页读取，不据此推导真实 session 的速度倍数。

## 正式接入前仍必须完成

1. 完整事实模型与现有 reducer 的语义等价、限额策略、格式 registry 与双向拒绝误写。
2. 完整 SessionStorage/Repo、branch history/context/summary cursor、正文错误与 UI 的可见处理。
3. 非阻塞执行、caller cancellation 与 detached writer、busy/error 分类、active progress 与 terminal proof。
4. 真实独立进程 crash/损坏/锁故障、I/O 与 allocation 测量；当前 failpoint 不模拟 OS kill/断电。
5. 显式 idle+独占+备份+副本验证迁移及回滚、伴随文件识别、导入导出、旧 writer 隔离。
6. 工作流业务/插件迁移与同名 crate 的发布所有权协调。原型通过不代表这些项目已完成。
