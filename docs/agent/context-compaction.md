# 上下文压缩

上下文压缩由 `rpi-harness` 管理：较早消息生成摘要，最近消息保留原文，摘要与保留消息写入新的会话 entry。原始历史仍保存在会话树中。

## 触发与配置

默认配置定义在 [types.rs](../../crates/rpi-harness/src/types.rs)：

| 配置 | 默认值 | 用途 |
| --- | --- | --- |
| `enabled` | `true` | 启用自动阈值检查 |
| `reserve_tokens` | `16384` | 提前于上下文窗口上限触发 |
| `keep_recent_tokens` | `20000` | 切分时保留最近消息的近似预算 |

触发条件为 `context_tokens > context_window - reserve_tokens`，达到等号时不触发。模型窗口为 0 时不做自动检查。

[AgentHarness](../../crates/rpi-harness/src/agent_harness.rs) 提供三条路径：

- run 开始前：新 prompt 已持久化后检查当前 branch。
- 运行中：Agent Loop 的 `after_tool_results` 回调在工具结果返回后检查内存上下文，成功后替换下一轮使用的消息。
- 手动压缩：`compact_core` 直接准备并生成摘要，不要求先达到自动阈值，允许传入额外摘要指令。

自动压缩记录 `Threshold` 原因，手动压缩记录 `Manual`。类型中虽然定义了 `Overflow`，当前这两处自动入口不提供收到超窗错误后的专项压缩重试。

## Token 估算与切分

[估算器](../../crates/rpi-harness/src/compaction/tokens.rs) 优先采用最近有效 assistant 的 `usage.context_tokens()`，再加其后消息的估算量。忽略 aborted/error 响应；没有有效 usage 时估算全部消息。文本使用 UTF-8 长度除以 4 向上取整，图片采用固定预算，均非精确 tokenizer 计数。

[切分器](../../crates/rpi-harness/src/compaction/cut_point.rs) 从后向前累计消息 token，并选择合法的保留起点。`toolResult` 不能成为起点，以保留对应的 assistant 工具调用。预算是近似保留目标，不是严格上限。

若起点落在一轮对话中间，会分为较早完整历史、本轮前缀、保留后缀。空路径或最后一个 entry 已是压缩 entry 时，准备阶段返回无操作。

## 摘要生成

[prepare_compaction / compact](../../crates/rpi-harness/src/compaction/compaction.rs) 处理摘要与原文保留：

- 正式 branch 中存在旧压缩 entry 时，取出旧摘要，并将其 `retained_tail` 与之后新增的消息作为本次待切分历史。
- 普通压缩生成一份结构化摘要：目标、约束、进度、关键决策、下一步、关键上下文。旧摘要通过更新提示词合并。
- 切分一轮对话时，历史与本轮前缀分别总结后拼接；没有较早历史时仅调用前缀总结，并使用 `No prior history.` 占位。
- 历史摘要输出预算为 `floor(0.8 * reserve_tokens)`，前缀摘要为 `floor(0.5 * reserve_tokens)`，分别受模型输出上限约束。
- 对话转为纯文本输入，工具结果每条保留约 2000 字符；图片内容不进入文本摘要。
- 提取 `read`、`write`、`edit` 工具调用中的路径，并附加已读取和已修改文件列表。

摘要使用当前模型、单独的系统提示词、不提供工具。请求关闭缓存保留，并使用新的 session ID。

## 持久化与后续上下文

`persist_compaction_entry` 先记录写入意图，再提交 entry，最后记录压缩步骤。entry 保存 `summary`、`retained_tail`、`tokens_before`、`details` 与摘要调用的 `usage`。

[上下文构造器](../../crates/rpi-harness/src/session/context.rs) 从当前 branch 的最后一个压缩 entry 开始，构造：

```text
最新摘要 + retained_tail 原文 + 压缩后新增消息
```

模型、thinking level 和 active tools 等状态仍从完整 branch 推导。调用者的 entry transforms 在默认压缩边界处理之后应用。

## 当前实现边界

- 运行前摘要失败会结束 run；运行中摘要或持久化失败返回无更新，继续原上下文。
- 运行前与手动路径使用配置的 thinking level 和 retry；运行中路径未传入这两项。
- 运行中将消息包装为普通 entry，内存中的旧摘要不会作为正式 `CompactionEntry` 识别，因此未走 `previous_summary` 更新路径，也未继承旧 entry 的文件元数据。
- 保留的 assistant 仍携带压缩前 usage；压缩后的估算可能继续采用该 usage，造成偏高估算和重复触发。
- 成功后未验证最终上下文一定低于阈值；摘要输出与保留预算也不是合并后的硬上限。

相关测试位于 `crates/rpi-harness/tests/compaction_cut_point.rs`、`compaction_summary.rs` 与 `compaction_split_turn.rs`。上述实现边界来自代码检查，不代表新增测试已覆盖。
