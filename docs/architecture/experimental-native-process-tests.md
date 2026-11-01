# 实验 native store：隔离进程终止与 SQLite 写锁验证

## 入口与范围

```bash
cargo test -p rpi-harness --features experimental-native-store --lib process_tests \
  --locked --offline -- --test-threads=1
```

父测试有十项；`child` 为默认 ignored 的内部 helper，不是独立验收项。父仅启动当前
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
| 强制 pager spill + hot journal | 2 MiB 正文、小 cache；暂停时有效 journal 且原有 main bytes 已变，kill 后 Store/open/list 首次访问恢复 |
| 大 progress stream replacement | ClearStream+Append 真正改写旧页；COMMIT 前后 kill，首次 repo.open 与全状态 oracle |
| 大 progress prune commit boundaries | terminal proof 删同 lane/run 两帧、保留 side 同 run 及 keep；不声称旧页 spill |
| unsupported header + hot journal | foreign/v1/v2 APP/version 在任何可写 SQLite 访问前拒绝，main/journal 全字节不变 |
| 缺失/partial/坏 header | gate 本身拒绝各字段，三个入口失败且不创建/改写 artifact 或 sidecar |
| recognized header + 坏 root | 允许 SQLite 恢复，但后续验证仍拒绝坏逻辑格式；不冒充无副作用错误 |

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

## 强制 spill 与现有 repo 打开顺序修复

原小事务矩阵不保证未提交页已写出 pager cache。新增变体只在 child 的测试连接设置
`cache_size=8` 页、`cache_spill=ON/8`，将 queued user 正文扩为固定 ASCII 2 MiB；仍走
真实五项 native batch，不改产品默认配置或正文预算。Store / repo.open / repo.list 各自
独立 fixture，分别在 COMMIT 前、成功后但返回前 kill；list 是首次逻辑访问，不能先用
snapshot 偷做恢复。control 位于子目录，不混入 catalog。

before marker 后 parent 不打开 SQLite，仅以普通 OS readonly handle 检查：

- journal 首 28 bytes 的 magic `d9 d5 05 f9 20 a1 63 d7`、有限非零 nRec、original DB
  页数、合法 sector/page size，checked arithmetic 验证首段记录空间不超过文件长度。
- 可信 closed baseline 原有范围内必须至少一个字节已变；单纯文件增长/mtime/journal
  存在不算。main/journal 大小上限各 32 MiB，不由字段驱动无界分配。
- Windows 使用共享 READ|WRITE|DELETE 的只读 handle；所有 raw handle 在 kill/recovery
  前关闭。kill/reap 后、首次 SQLite open 前再次验证 main/journal 仍处于该状态。

实测 page size 4096、journal sector size **512**（不是硬盘物理 sector 承诺），main 从
106496 增长至 2207744 bytes，原有范围约 1900 bytes 改变；journal 首段 nRec=4。
首 header/空间检查不等于验证所有 journal record/checksum。before 回到完整 baseline，
after 精确等于完整 committed oracle；深比较不打印多 MiB body。

该测试曾真实暴露 `ExperimentalNativeRepo::open` 的只读 identity probe 在 hot journal
上报 `attempt to write a readonly database`。现有私有打开路径已修正：先以 OS 只读方式
检查固定 100-byte SQLite header（magic、native v3 APP/user_version、page size、已知
SQLite 读写版本），再用无 CREATE 的 RW probe，让 SQLite 自行恢复后读取身份，最后
保留原 Store/root/checkpoint/digest 校验。Store::open 本身同样先 gate，避免仅为判型而
提前回放 foreign journal。不是手写 journal replay，也没有新增 CLI 恢复或 API。

header gate 只是格式家族准入，不认证内部状态、不防恶意路径替换或提供 generation
fencing。识别为 v3 但 root/schema/digest 坏时，可能已回放 journal 后才拒绝；错误不保证
物理无改写。`list()` 逻辑上不追加事实/不迁移，但可恢复已识别 native hot journal，不是
严格 filesystem readonly；目录后续有坏 artifact 时，前面的恢复也不会因此撤销。
unsupported APP/version/magic/短 header 则在任何 RW SQLite 前拒绝。另以四组真实
foreign/旧格式 hot journal 验证 main/journal 全字节不变；known-v3-header + 坏 root 的
用例确认恢复后仍拒绝且不“修好”逻辑格式。初始 create 未完成、损坏 journal/内部状态或
恶意伪造 header 不在无副作用拒绝保证内。

## 大 active progress：replacement spill 与 prune 提交边界

另一个父测试覆盖四格独立合成 fixture。baseline 在原三小帧（main/run、main/keep、
side/run）之后，真实提交 main/run 与 main/keep 各 1 MiB ASCII delta，allocator=5。
实际 progress auxiliary JSON 在 baseline、control 和恢复后续写时均检查 ≤4 MiB；
没有调高现有产品预算，也没有修改 root/schema、手动 flush 或注入无关 dirty pages。

- **Stream replacement**：同一次真实 `append_progress` 提交 ClearStream(main/run,
  index1，无 payload，seq6) 和 Append(main/run，index2，1 MiB，seq7)。旧大 run 帧
  被替换，clear 不保留为 active frame；其他四帧逐值保留，allocator=7。COMMIT 前
  raw main 长度仍为 2203648 bytes/538 pages，但原范围 2070502 bytes 已改变；journal
  2188816 bytes，首段 nRec=7。暂停时及 kill/reap 后、任何 SQLite 打开前均严格验收
  旧页变化和有效 journal 结构，不以文件增长代替。COMMIT 后 kill 则是 ack-loss 格。
- **Prune**：故障前只持久化 main/run finish，使 history15→16；prune 删除 main/run
  两帧，保留 side/run 与 main/keep 两帧的完整值，allocator 仍为5。真实 COMMIT 前后
  kill，完整状态必须分别等于 baseline/control。**本 fixture 未建立 prune 旧页已
  写回的证据**：原范围变化实测为0。因此它是大 payload 的提交/终止测试，不是
  prune dirty-old-page-spill 测试；raw 观察只输出诊断，不作为 spill 验收。

最初的纯追加 1 MiB fixture 同样只有增长而无旧页变化，严格 raw 检查正确失败；不能把
replacement 的证据推广为纯 Append spill。上述范围收窄不是把失败当作通过，缺少的两类
spill 证据仍明确未交付。已有小 pure-append/prune crash 矩阵继续保留。

每格关闭 baseline 后另建 control，first recovery 确为新 repo.open 保存的 metadata，
不先 snapshot/list 偷恢复。比较全部八表、projection 和全部查询，history 与全部
非 progress 辅助状态逐值不变；除与操作相关的 progress query 外，其余查询也不变。
独立逐帧完整预期补强共享 action oracle，覆盖 lane/run/index/op/seq/time、全部 payload、
clear 不留帧和跨 lane 保留。恢复后向 main/keep index2 追加小帧，replacement before/after
必须 seq6/8，prune before/after 必须 seq6/6；重开后的完整 progress、allocator 和 keep
查询必须确实持久化，所有原帧保留，history 及非 progress 辅助行不变。断言不打印大正文。

## 实测与限制

本机 Windows、SQLite 3.45.0、`journal_mode=delete`、`synchronous=2`。已实测串行及
并行隔离执行。没有声称 Linux/macOS、WAL、网络盘或其他 SQLite 配置通过；header 接受
SQLite 读写版本 2 不是 WAL recovery 的运行证据。

可逆 mutation：将 history after 探针移到 COMMIT 前，ACK-loss 场景快照比较按预期失败；
恢复正确位置后重新全绿。这验证 oracle 能识别“仅暂存”冒充“已提交”。分别移除 Store
和 repo 的 header gate 也都会使真实 foreign-journal bytes 比较变红；移除 read/write
version gate 时，直接字段级 gate 断言变红。三项可逆修改均已恢复。

新增 progress 负例也实际变红：共享 action 将新大帧完整 payload 改为另一固定值时，
独立逐帧预期拒绝它；将续写后的 snapshot 暂时替换为旧 snapshot、模拟未持久化续写时，
完整 progress/allocator 断言拒绝它。两项均已恢复，再运行最终完整回归。后者是 oracle
负例，不是声称观察到真实存储丢写。

进程终止不等同断电/内核崩溃，PRAGMA 数值也不是 fsync/物理 I/O 承诺。测试零 busy timeout
不改变生产 5 秒设置，也不证明生产等待/自动重试语义。SQLite 事务锁证据不等于外部
lifecycle claim、generation fencing、跨进程安全 delete 或外部 exactly-once。

没有新增 whole-batch reconcile API、request-ID 自动对账或自动重放；提交不确定时不能
盲目重试。生产恢复入口、稳定 registry、跨平台/更细 mutation 边界、真正断电验证与迁移
仍未交付；生产 v4 JSONL 初次全扫不变。
