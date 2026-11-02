# Session history：共享抽象、JSONL 冷 payload 与 TUI 分页

本次保持现有 V4 JSONL 为默认后端；不会迁移或改写为另一个上游格式。`experimental_native` 仍需 feature 和现有显式开关，且仅允许新建 print 会话；不扩大到 TUI、RPC 或恢复，也不静默回退。没有新增 backend CLI 参数、registry、owner/epoch/fencing 或持久索引协议。

## 复用边界

- 保留 `SessionStorage`、`SessionRepo` 和 `Session`。Repo 的关联 `Storage` 继续保留各自 typed metadata/锁，业务入口使用 `Arc<dyn SessionStorage>`。
- `session/state/index.rs` 的 `IndexedState<P>` 共用 mutation 验证、IDs/sequence、lanes、分支、records/log、stats 与小型 facts。
- eager `SessionState` 使用完整 `Entry` payload；其公开行为和 `indexed-session-v1` snapshot wire 保持不变。
- JSONL 私有 `ResidentState` 使用真实 typed `JsonlPayload`，不是在 eager `Vec<Entry>` 中塞空内容占位。小型 `EntryMeta` 与 payload 分离；其他后端不必实现另一套验证器。
- `branch_entry_page`、`branch_overview` 是已有 storage/tree trait 的兼容默认方法，不是新 trait。旧实现仍可使用有界查询默认实现；JSONL overview 从 metadata 派生，不 hydrate 历史正文。

## JSONL 加载与写入

支持 handle-bound 长度/range 的 reader 使用 1 MiB byte scanner。完整物理行才进行 UTF-8/schema decode；最大单行可以超过一个块。保留 seq gap、语义等价重复 seq、冲突拒绝、最后物理行 syntax torn-tail 修复、schema 硬错误和旧 assistant-frame 特例。重复 seq 通过重读原 span 比较 mutation，不把 raw hash 当作语义等价规则。

常驻的是索引及 `(reader generation, offset, length, digest, identity)` 引用，而非全部正文。读取先验证 digest/identity，再应用 replay 的 parent 规范化。新 append 中少量 presence-sensitive null 字段有 typed 小修正位/retained-tail 索引，以保留原 live eager 行为；不会为此保留整条 JSON，也不改变旧 reopen 对这些字段的既有反序列化语义。

查询在锁内选引用、锁外 hydrate，不设置无限增长的 warm cache。payload-only tool/outbox 过滤在 limit 前执行；summaries、window、context、log 与 fork 使用真实 payload。普通 context 仅 hydrate 最新 compaction 及后续内容，模型/thinking/tools facts 从完整分支 metadata 提取；需要完整分支的 hooks、导出和全量查询仍可读完整历史。

`ReadFile::byte_len` 与 `AppendFile::append_with_receipt` 均有兼容默认实现。没有稳定能力的自定义 filesystem 使用明确的 eager 回退；真实 I/O 错误不会伪装成“不支持”。OS receipt 绑定实际写入的 generation，并在同 generation 内共享 reader；Windows 用 `same-file` 的安全 handle identity 避免每条 append 永久保留一个新 reader。缺少 receipt 的成功写入保持真实 resident payload，不猜 offset，也不把已成功写入改报失败。

修复、progress-header upgrade 使用分块复制/hash 和原子 publication，旧 reader 在 rename 后仍可服务已选中的读取计划。稳定 reader 上失败 append 的回滚只读取 batch 长度加一字节的尾窗口，并验证真实行边界；无此能力的兼容 filesystem 保留旧回滚路径。fork 固定来源后逐条 hydrate/写入，避免第二份全历史 payload，同时保持原 records/progress 不复制语义。

append ACK 仍是现有写入可见性边界，**不新增 power-loss fsync 承诺**。

## TUI

- 默认恢复最近 **50 entries / 4 MiB 编码 payload**，覆盖全部展示 variants；预算不含 page envelope/separators。单条超过预算时独占一页，不截断该条。
- `/history` 或 `/history older` 翻旧页，`/history newer` 翻新页，`/history latest` 回最新页。
- 页边界的 PageUp/PageDown 和回到底部绑定复用现有可配置键位；不会抢非空 editor、selector、search 或 extension dialog 的输入。
- 独立异步 worker 不经过等待完整 prompt 的主 mailbox。请求绑定 storage、leaf 与 UI generation；切换/分支/新操作使旧请求失效。读取失败保留当前页，错误提示只保留单槽。
- 成功翻页替换而非累加旧页，并清理组件映射、搜索、selection 与滚动缓存引用。当前操作的 active slot 与历史页分离；旧页不会持续追加新流式或 blocking assistant 输出，settlement 后回到有界历史。写入失败时保留未保存内容并阻止会丢失它的翻页。
- 当前 plan/todo 面板独立于所见旧页恢复，按 seq 合并新旧 custom kind；翻旧页不回滚面板。当前 capabilities header 也独立保留。
- 模型恢复和 footer 使用 branch overview，而不是为显示信息构造全量 provider context。现有 RPC `message_window` 契约及完整导出不变。

## 资源边界与未作出的保证

这不是恒定总内存或随机访问数据库：首次仍须扫描并验证全部历史；索引随条目数增长；单条最大 payload、活跃操作、完整 hooks/导出/全量查询可能很大。没有 compaction 时不能靠丢模型上下文省内存。自定义 eager fallback 不获得普通历史冷存收益。

保留旧损坏处理边界：非法 UTF-8 仍是硬错误，包括写断在多字节字符中的尾行；不把它扩展成新的自动修复规则。TUI 有界分页不等于整个应用的硬 RSS 配额，也不限制单次活跃操作/未保存数据。

验证在 Windows x86_64 完成；未执行真实终端视觉/按键/图片上传缓存测试，未运行 Linux 原生测试或真实 provider/session 测试。性能数据、复现方法与加载耗时回退见 [长历史实测](../performance/session-history-memory.md)。
