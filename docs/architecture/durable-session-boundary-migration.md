# Session 到 `rpi-durable` 的持久化边界迁移

## 目标

将 workspace 调整为与原生 Pi 一致的分层结构，同时不破坏现有 Session 数据格式和兼容 API：

```text
rpi-harness::AgentHarness / Session / Agent Loop
        │
        │  Session domain adapter
        ▼
rpi-durable::runtime
        │
        │  storage boundary
        ▼
rpi-durable::storage
        │
        ├── JSONL journal
        └── optional SQLite journal
```

`rpi-harness` 继续拥有 Pi-facing `Session`、session tree、transcript、上下文准备和 Agent Loop。
`rpi-durable` 只拥有 durable domain、host runtime authority、journal sequence/revision 和 backend。

## 不变的边界

### `rpi-harness` 保留

- `Session` facade 和所有 Pi-facing API；
- entry tree、branch、lane、transcript 查询；
- `AgentHarness`、Agent Loop、上下文压缩和工具执行；
- `AgentMessage`、`LaneRecord`、`Entry` 等 Pi 专属类型；
- Memory/JSONL/SQLite typed backend 的兼容读取和旧格式支持；
- 原有 recovery、`ToolReplay`、operation/tool record 语义。

### `rpi-durable` 负责

- durable domain 的校验和状态转换；
- actor、scope、admission、receipt、reconcile runtime authority；
- opaque journal frame 的最终写入；
- sequence 分配和 revision CAS；
- JSONL/SQLite backend 的 reopen、连续性和冲突校验。

### 明确不增加

- `effectKey`、`argsDigest` 等外部副作用协议；
- exactly-once 工具执行或外部写入保证；
- `Unknown`、`RecoveryRequired` 等新的状态分类；
- inspect/reconcile API 作为新的业务事实模型；
- 自动迁移、自动重编号或删除旧 session frame。

## 当前状态

已完成：

1. `rpi-durable` 已按 `domain/runtime/storage` 分层；
2. domain 模块已从旧路径迁移，并保留兼容 re-export；
3. runtime 类型已拆为：
   - `actor.rs`
   - `scope.rs`
   - `admission.rs`
   - `receipt.rs`
   - `reconcile.rs`
   - `service.rs`
4. storage 已提供 `DurableBackend`、`JsonlBackend`、可选 `SqliteBackend`；
5. `DurableService` 已负责 append、load、refresh、sequence 和 revision CAS；
6. `rpi-harness::session::durable_adapter` 已将 `Entry` 与 `LaneRecord` 转为不同的 journal kind；
7. `DurableSessionWriter<B>` 已提供 harness 到 durable runtime 的显式写入 seam；
8. `SessionDurableWriter` / `SharedDurableSessionWriter` 已提供 host-owned、object-safe capability；
9. `Session::new_with_durable_writer`、`append_entry_to_durable` 和 `append_record_to_durable` 已提供显式 opt-in 路径；
10. JSONL 和 opt-in SQLite 都已有 `Session facade → adapter → DurableService → backend → reopen/load` 测试；
11. `DurableBackend::append_batch`、`DurableService::append_batch` 和 Session durable adapter 已支持 record + entry 的显式逻辑批量写入；
12. `entry_from_frame` / `lane_record_from_frame` 已提供带 discriminator/payload 一致性校验的反向投影；
13. `entries_from_frames` / `lane_records_from_frames` 可以从混合 journal 中分别投影 transcript 和 execution record namespace；
14. `Session::load_durable_entries` / `load_durable_records` 已提供显式 durable read path；
15. durable capability 还提供 expected sequence 查询和 refresh，用于处理外部 writer 导致的 CAS 冲突；refresh 会同步 mutation reducer cache；
16. `Session::load_durable_frames` 保留完整 opaque frame 读取能力，projection 不会丢弃未知 host-owned frame；
17. `SessionMutation` 的 entry、record、lane、name fact、label fact 已有显式 adapter round-trip 映射；
18. 旧 harness API 和 typed backend 仍保持兼容；
19. `DurableSessionMutationLog` 可在显式 opt-in 下将 durable mutation journal replay 到现有 `SessionState`，并在写入前先验证 reducer，再提交 backend；
20. raw frame read 会保留未知 host-owned frame，mutation/state projection 只解释 `session.mutation.*` namespace；
21. `DurableSessionWriter::open_for_session` 可绑定 Session identity，facade 在每次操作校验 identity，storage rebind 会清除旧 capability；
22. backend 返回违反 DurableService contract 的 frame 后，service 进入终态 poisoned/fail-closed，不继续使用不可信 revision；
23. 兼容 JSONL storage 的 branch ancestry 走 metadata-only cold path，避免 branch/fork 读取被归档 body 或 torn/poisoned payload 阻断。

尚未完成：

- `Session::append_entry` / `Session::append_record` 的默认写入流程还没有切换到 `DurableService`；
- 现有 Memory/JSONL/SQLite backend 仍是 canonical Session writer；
- durable journal frame 与旧 Session JSONL/SQLite 格式尚未建立默认读写映射；当前只支持显式 adapter frame 的受校验反向投影、namespace projection，以及新的 `DurableSessionMutationLog` opt-in replay，不执行自动迁移；
- 默认 `Session::append_record_and_entry` 还没有切换到 durable batch path；
- JSONL 的批量写入只保证进程内 logical batch 的预校验和一次 append，不宣称断电原子性；SQLite 才提供数据库事务边界。

## 实施计划

### 阶段 A：定义写入适配边界

新增一个 harness-owned、backend-neutral 的 journal writer capability：

- 输入只接受已经由 harness 校验过的 `Entry` / `LaneRecord`；
- 输出 durable `JournalFrame` 或明确的 storage error；
- transcript entry 与 execution record 使用不同 kind namespace；
- capability 不暴露给 extension，extension 只能通过 host-mediated API 使用；
- `Session` 不直接持有具体 JSONL/SQLite writer 类型；
- writer 的 owner、生命周期和 revision 由 host runtime 管理。

这一阶段不改变旧 backend 的默认行为，先提供显式 opt-in 构造路径。

### 阶段 B：建立 Session 端到端测试（已完成第一批）

当前已覆盖：

- Session 安装显式 durable writer capability；
- Session entry 经过 adapter 和 DurableService 写入 JSONL；
- Session operation record 经过 adapter 和 DurableService 写入 SQLite；
- backend reopen/load 保留 journal frame 和 sequence；
- transcript entry 与 execution record 的 kind namespace 分离。

以下行为已由真实 backend 回归测试覆盖：

1. `Session` append entry；
2. `Session` append operation/tool record；
3. durable adapter 生成不同 journal kind；
4. `DurableService` 分配连续 sequence；
5. JSONL backend 写入；
6. reopen 后 load；
7. stale expected sequence 被拒绝；
8. transcript 与 execution record 不互相覆盖；
9. SQLite feature 下同样的行为；
10. append 失败时 Session/live state 不伪造成功。

测试必须使用真实 backend，但不得替换现有 Session compatibility contract。当前
`durable-sqlite` 是 `rpi-harness` 的显式测试/opt-in feature，不影响默认 feature 集合。

### 阶段 C：迁移 JSONL writer

阶段 B 已先增加 durable batch contract：record + entry 可以通过一次
`expected_sequence` 检查提交。JSONL 在写入前完成整批校验和编码，但仍明确不保证
power-loss atomicity；SQLite 的对应实现使用 transaction。默认 Session 路径仍未切换。


只迁移新建、显式 opt-in 的 durable session：

- 保留旧 v4 JSONL header、entry、record、progress 格式；
- 不把 opaque journal frame 直接当成旧 Session JSONL 文件格式；
- 通过 adapter 映射 logical mutation，而不是覆盖旧 reader；
- 保留 torn-tail、metadata、branch/tree、checkpoint 和 progress 语义；
- 读取旧 session 时不自动生成新 journal；
- 写入失败、revision conflict、reopen failure 都 fail closed。

### 阶段 D：迁移 SQLite writer

在 JSONL 适配稳定后：

- 复用同一套 Session adapter 和 durable runtime contract；
- 使用 SQLite transaction 维护 sequence/revision CAS；
- 保留现有 SQLite schema 的读取兼容；
- 只有显式新建或显式切换才使用 durable journal path；
- 不进行隐式 schema migration。

### 阶段 E：默认路径评估

只有满足以下条件，才评估将 durable path 设为默认：

- 旧 Session JSONL/SQLite 全部兼容测试通过；
- branch/lane/fork/rebind/recovery/progress 测试通过；
- 已证明不会重复写 transcript 或 execution record；
- stale writer、外部修改、torn tail 和 reopen 行为已固定；
- CLI、extensions、TUI 不需要直接了解 durable backend；
- 有明确 rollback 和 feature-off 行为。

在此之前，experimental/native store 不得被宣布为默认 backend。

## 关键设计决策

### Journal frame 不是 Session entry

`JournalFrame` 是 storage boundary 的 opaque persistence unit，不替代 `Entry`、`LaneRecord` 或 Session tree。
只有 harness adapter 能解释 `session.entry.*` 和 `session.record.*` payload。

### Sequence 不复用 Session seq

`JournalFrame.sequence` 由 durable backend 分配，不能直接复用 transcript entry seq 或 lane record seq。
这样可以避免两个 revision domain 互相污染。

### 批量写入不自动宣称原子性

如果 Session 操作需要 record + entry 的逻辑事务，必须在 backend/runtime 中明确实现并测试。
不能把多个 `append` 调用简单串联后称为 atomic commit，也不能新增 exactly-once 外部副作用语义。

### 旧 backend 先兼容、后替换

现有 typed backend 暂时保留。迁移必须是 staged opt-in，不能通过删除旧实现来强迫所有调用方切换。

## 验收标准

### 架构

- `rpi-durable` 不依赖 harness、Agent Loop、CLI 或 extension；
- `Session` 仍在 `rpi-harness`；
- extension 无法直接取得 durable storage writer；
- 旧模块路径和公开 API 继续编译。

### 行为

- sequence 只由 durable backend 分配；
- revision CAS 失败不会发布部分 durable state；
- transcript 和 execution record 始终逻辑分离；
- reopen/load 不改变旧 Session 读取语义；
- 失败路径 fail closed，不伪造成功、不重复执行工具。

### 验证命令

```text
cargo fmt --all -- --check
cargo test -p rpi-durable --all-features --locked --offline
cargo test -p rpi-harness --locked --offline
cargo test -p rpi-cli --locked --offline
cargo test -p rpi-extensions --locked --offline
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
git diff --check
```

## 当前执行目标

> 阶段 A/B 已完成第一批；阶段 C 当前验证 `SessionMutation → durable frame → backend → reopen → SessionState` 的显式 opt-in 路径。下一步仍需补齐 metadata、progress、checkpoint、recovery、branch/fork 和旧 typed backend 的无损映射，再评估默认 writer 替换；在此之前不切换默认 backend。
