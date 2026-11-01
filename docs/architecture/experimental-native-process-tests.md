# 实验 native store：隔离进程终止与 SQLite 写锁验证

## 入口与范围

```bash
cargo test -p rpi-harness --features experimental-native-store --lib process_tests \
  --locked --offline -- --test-threads=1
```

父测试有五项；`child` 为默认 ignored 的内部 helper，不是独立验收项。父仅启动当前
libtest executable 的精确 helper 名（不是 CLI、Cargo 或真实 session），通过子进程专属
manifest/nonce 传参。每次使用独立 TempDir，不修改父环境/cwd，不读取实际配置或密钥。
没有增加 CLI 恢复入口、公共测试 API、生产 fault 环境变量或新格式。

探针字段和调用均 `cfg(test)`，生产 artifact 不包含它们。observer 属于单个 Store
connection，默认 None，不是全局可变状态。实际调用点：

- history：整批 `apply_writes` 成功后、COMMIT 前；成功 COMMIT 后、调用返回前。
- progress append 与 prune：各自发布辅助状态后、COMMIT 前；成功 COMMIT 后、返回前。

helper 发出精确阶段事件后等待父命令，不能自行跨越故障点。父收到匹配事件才 kill/reap，
因此不是用 sleep 猜测事务状态，也不是 drop future 或普通错误返回模拟进程死亡。
**ACK 丢失仅指 SQLite COMMIT 已成功、storage 调用尚未返回结果时进程死亡。**

## 矩阵与 oracle

| 用例 | 检查 |
| --- | --- |
| history COMMIT 前 kill | 整批及全部派生行均不可见，baseline 完整保留 |
| history COMMIT 后/返回前 kill | 整批全部可见，不能只保留 prefix |
| history 正常 release | 与提交后 kill 的完整状态一致，确实收到结果并正常退出 |
| progress append pre/post kill | 整批辅助 frames 全无/全有；history 不变 |
| progress prune pre/post kill | 同 lane/run 的 terminal proof 删除全无/全有；保留另一 lane 同 run 及未结束 run |
| 两进程 writer contention | A 真实暂存持锁；B 已独立 open，零 timeout 的真实 commit 必须得到 typed SQLITE_BUSY |
| 持锁 A release / kill 后 B retry | release 时旧 expected revision 拒绝，再写连续序号；kill 时 A 全回滚，B 紧接 baseline |
| 错误阶段 | 必须准确识别阶段不符，unwind 时回收持写锁子进程，后续 writer 成功 |
| helper panic | 非预期 exit 101 必须成为协议失败，不能当作“预期 crash”绿灯 |

baseline history=15，batch 五项（user、tool result、finish、name、label）跨 checkpoint
门槛，明确检查 checkpoint 0→16、revision 15→20，pending/open 被消费/关闭，tool mirror
非空。后续追加必须得到 16 或 21，不跳号。

oracle 为**关闭连接后的合成 fixture 同 identity 副本**，在另一路径执行相同正常写入。
它不是业务语义的独立实现，而是检查故障与无故障路径的事务等价性；显式边界断言补强
基本语义。没有复制活跃 SQLite 文件或真实 session，也不提供迁移/产品 fork 功能。

恢复时不经过进程内 repo handle registry，重新建立连接。每个 seq 都读 canonical
mutation 并校验正文/metadata/相关 mirrors，验证 checkpoint 与有界 tail。比较八张表的
**全部列及全部行**，含 digest、metadata、正文、navigation/summary、auxiliary；同时比较
projection、trait log、lanes/stats、pending/open、summary/window 全字段、progress。
测试断言 sqlite_schema 的业务表集合等于 oracle 列表，新增表必须纳入比较。

## 协议与清理

控制文件含 nonce、单调序号、精确阶段及 payload，64 KiB 上限；写入新临时文件、关闭后
同目录 rename 发布，读方不接受半写/空文件。父子各次等待限 30 秒；10ms 轮询只负责
IPC/watchdog，不用来证明事务或锁阶段。stdout/stderr 直接到临时日志而非管道，失败诊断
只读取尾部最多 8 KiB。

父仅终止自己创建的 Child；正常与 unwind 路径都 kill + 最长 10 秒 try_wait/reap，不做
无界 wait。子未收到 release 就超时非零退出，绝不自行提交。自然提前退出、零测试匹配、
错误阶段、timeout、spawn/kill/reap/目录清理错误均不可作为成功。

`result` 后子仍等待 `finish`，避免父读最终事件时子已正常退出的竞态；父发送 finish 后
允许子在调用 finish() 前退出。先确认子退出，再 reopen/删除文件。所有清理都受限且报错，
不是父进程自身被强制终止时“绝不遗留子进程”的 OS 级保证。

## 实测与限制

本机 Windows、SQLite 3.45.0、`journal_mode=delete`、`synchronous=2`。已实测串行与五项
并行隔离执行。没有声称 Linux/macOS、WAL、网络盘或其他 SQLite 配置通过。

可逆 mutation：将 history after 探针移到 COMMIT 前，ACK-loss 场景快照比较按预期失败；
恢复正确位置后重新全绿。这验证 oracle 能识别“仅暂存”冒充“已提交”。

进程终止不等同断电/内核崩溃，PRAGMA 数值也不是 fsync/物理 I/O 承诺。测试零 busy timeout
不改变生产 5 秒设置，也不证明生产等待/自动重试语义。SQLite 事务锁证据不等于外部
lifecycle claim、generation fencing、跨进程安全 delete 或外部 exactly-once。

没有新增 whole-batch reconcile API、request-ID 自动对账或自动重放；提交不确定时不能
盲目重试。生产恢复入口、稳定 registry、跨平台/更细 mutation 边界、真正断电验证与迁移
仍未交付；生产 v4 JSONL 初次全扫不变。
