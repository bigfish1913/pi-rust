# pi-rust Agent Loop 与消息解析链路

> 本文从 TUI 用户按下 Enter 开始，追踪一条消息经过 TUI、Harness、Agent Loop、Provider、SSE 解析，再回到 TUI 渲染的完整链路。
>
> 文中的源码链接均为相对路径并带行号，可以直接点击跳转。

## 1. 总体链路

```text
TUI Enter
  ↓
Editor::submit
  ↓
interactive_tui::on_submit
  ↓
TuiMessage::UserInput
  ↓
run_prompt_streaming
  ↓
AgentLane::prompt_text
  ↓
AgentHarness::run_core
  ↓
run_agent_loop
  ↓
stream_assistant_response
  ↓
StreamFn
  ↓
Provider::stream_simple
  ↓
HTTP / SSE
  ↓
AssistantMessageEvent
  ↓
AssistantMessage
  ├── 普通文本 → AgentEnd
  └── ToolCall → 执行工具 → ToolResultMessage → 下一轮模型请求
  ↓
AgentEvent 广播
  ↓
TUI drain task
  ↓
终端渲染
```

最重要的三个层次是：

- [`pi-agent/src/agent_loop.rs`](../crates/rpi-agent/src/agent_loop.rs)：纯 Agent Loop，负责调用模型、识别工具、循环运行。
- [`pi-harness/src/agent_harness.rs`](../crates/rpi-harness/src/agent_harness.rs)：带 session、持久化、压缩、系统提示和工具配置的运行外壳。
- [`pi-ai/src/providers/`](../crates/rpi-ai/src/providers/)：具体模型协议和 HTTP/SSE 解析。

---

## 2. TUI：用户消息从哪里发出

### 2.1 TUI 创建和事件管道

入口是 [`interactive_tui`](../crates/rpi-cli/src/interactive_tui.rs#L5921)：

```rust
let lane: Arc<dyn AgentLane> = harness.lane("main");
```

Harness 创建时会安装 `BroadcastEmitter`：

- [`session.rs:779`](../crates/rpi-cli/src/session.rs#L779)
- [`BroadcastEmitter`](../crates/rpi-agent/src/events.rs#L135)

```rust
let (broadcast, event_rx) =
    rpi_agent::events::BroadcastEmitter::new(256);
```

其中：

- `broadcast` 被注入 harness，Agent Loop 用它发布事件。
- `event_rx` 被 TUI 消费，用于渲染模型增量输出。

### 2.2 Enter 键进入 Editor

键盘事件在 TUI 的 blocking key loop 中读取，最终交给：

```rust
editor_for_key.handle_key(key);
```

Editor 对 Enter 的处理位于 [`editor.rs:1274`](../crates/rpi-tui/src/editor.rs#L1274)：

```rust
(KeyModifiers::NONE, KeyCode::Enter) => {
    self.submit();
}
```

`Editor::submit` 位于 [`editor.rs:1185`](../crates/rpi-tui/src/editor.rs#L1185)，它会调用之前注册的 `on_submit` 回调。

### 2.3 TUI 提交回调

提交回调位于 [`interactive_tui.rs:6431`](../crates/rpi-cli/src/interactive_tui.rs#L6431)。这里会先做本地命令分流：

```text
/command  → slash command
!command  → 用户 shell 命令
运行中    → steering queue
空闲      → 普通 Agent prompt
```

普通 prompt 的关键操作是：

```rust
add_user_message(&ctx_for_cb.chat, text);
push_history(&ctx_for_cb.state, text);
ctx_for_cb.tx.send(TuiMessage::UserInput(text.to_string()));
```

注意：这里会先把用户气泡显示到界面，然后把消息放进 TUI 主循环的 channel。

### 2.4 TUI 主循环接收消息

主循环处理 `UserInput` 的位置是 [`interactive_tui.rs:7762`](../crates/rpi-cli/src/interactive_tui.rs#L7762)：

```rust
Some(TuiMessage::UserInput(prompt)) => {
    state.clear_editor_programmatically();
    let prompt_images = state.take_pending_images();

    run_prompt_streaming(
        &lane,
        &prompt,
        false,
        ...,
        prompt_images,
    ).await;
}
```

### 2.5 调用 Harness

[`run_prompt_streaming`](../crates/rpi-cli/src/interactive_tui.rs#L8264) 最终调用：

```rust
lane.prompt_text(prompt, images).await
```

从这里开始，消息离开 TUI，进入 Harness。

---

## 3. Harness：给 Agent Loop 准备运行上下文

### 3.1 `prompt_text`

主 lane 的 `prompt_text` 位于 [`agent_harness.rs:3761`](../crates/rpi-harness/src/agent_harness.rs#L3761)：

```rust
let message = AgentMessage::User(UserMessage::new(content, now_ms()));
self.run_core(vec![message]).await
```

这里完成：

```text
String + Images
  ↓
UserContent
  ↓
UserMessage
  ↓
AgentMessage::User
```

### 3.2 `run_core`

[`run_core`](../crates/rpi-harness/src/agent_harness.rs#L2890) 只是一个薄封装：

```rust
async fn run_core(&self, prompts: Vec<AgentMessage>) -> HarnessResult<RunResult> {
    self.run_core_with_entry(prompts, false).await
}
```

真正的准备工作在 [`run_core_with_entry`](../crates/rpi-harness/src/agent_harness.rs#L2903)。

### 3.3 `run_core_with_entry` 做什么

它依次完成：

1. 获取 `run_id` 和取消信号。
2. 恢复中断的 operation。
3. 记录 `RunStart`。
4. **先持久化用户消息**。
5. 读取 session branch。
6. 判断是否需要 compaction。
7. 生成 system prompt。
8. 收集 active tools。
9. 生成 `AgentContext`。
10. 生成 `AgentLoopConfig`。
11. 构造 `StreamFn`。
12. 调用 Agent Loop。

调用 Agent Loop 的位置是 [`agent_harness.rs:3332`](../crates/rpi-harness/src/agent_harness.rs#L3332)：

```rust
run_agent_loop(
    Vec::new(),
    agent_context,
    config,
    Arc::clone(&emitter),
    Arc::clone(&stream_fn),
)
.await
```

这里传入 `Vec::new()` 是有意的：用户 prompt 已经持久化，也已经出现在 `agent_context.messages` 中，不能再次追加，否则会被模型看到两遍。

### 3.4 构造 StreamFn

Harness 中的 `build_stream_fn` 位于 [`agent_harness.rs:2362`](../crates/rpi-harness/src/agent_harness.rs#L2362)。

它根据：

```rust
model.provider
```

找到实际的 provider，然后调用：

```rust
p.stream_simple(&model, &ctx, &opts).await
```

这就是 Harness 和 `pi-ai` Provider 之间的连接点。

---

## 4. Agent Loop：核心循环

核心文件：[`crates/rpi-agent/src/agent_loop.rs`](../crates/rpi-agent/src/agent_loop.rs)

### 4.1 入口 `run_agent_loop`

位置：[`agent_loop.rs:72`](../crates/rpi-agent/src/agent_loop.rs#L72)

```rust
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: Arc<dyn AgentEmitter>,
    stream_fn: StreamFn,
) -> Result<NewMessages, AgentError>
```

它负责：

- 初始化 `current_context`。
- 初始化 `new_messages`。
- 发出 `AgentStart`。
- 发出 `TurnStart`。
- 进入 `run_loop`。

### 4.2 `run_loop`

位置：[`agent_loop.rs:220`](../crates/rpi-agent/src/agent_loop.rs#L220)

这是 Agent 的真正状态循环：

```text
当前 Context
  ↓
请求模型
  ↓
得到 AssistantMessage
  ↓
包含 ToolCall？
  ├─ 否：判断是否结束
  └─ 是：执行工具
          ↓
       生成 ToolResultMessage
          ↓
       加回 Context
          ↓
       再请求模型
```

关键逻辑：

```rust
let message = stream_assistant_response(
    current_context,
    config,
    emit,
    stream_fn,
).await?;
```

从 assistant message 中提取工具调用：

```rust
let tool_calls: Vec<ToolCall> = message
    .content
    .iter()
    .filter_map(|content| match content {
        Content::ToolCall(tool_call) => Some(tool_call.clone()),
        _ => None,
    })
    .collect();
```

### 4.3 `stream_assistant_response`

位置：[`agent_loop.rs:441`](../crates/rpi-agent/src/agent_loop.rs#L441)

这是“上下文转换 + Provider 调用 + 流式消息折叠”的核心函数。

#### 第一步：AgentMessage 转成 LLM Message

```rust
let messages = context.messages.clone();
let llm_messages = (config.convert_to_llm)(messages).await;
```

转换链路：

```text
AgentMessage[]
  ↓ transform_context
AgentMessage[]
  ↓ convert_to_llm
Message[]
```

然后生成 Provider Context：

```rust
let llm_context = rpi_ai::types::Context {
    system_prompt: ...,
    messages: llm_messages,
    tools: ...,
};
```

#### 第二步：调用 StreamFn

```rust
let mut response = stream_fn(&config.model, &llm_context, &opts);
```

`StreamFn` 类型定义在 [`stream_fn.rs:28`](../crates/rpi-agent/src/stream_fn.rs#L28)：

```rust
pub type StreamFn = Arc<dyn Fn(
    &Model,
    &Context,
    &SimpleStreamOptions,
) -> AssistantMessageEventStream + Send + Sync>;
```

#### 第三步：消费增量事件

```rust
while let Some(event) = response.next().await {
    match &event {
        AssistantMessageEvent::Start { partial } => { ... }
        AssistantMessageEvent::TextDelta { partial, .. } => { ... }
        AssistantMessageEvent::ThinkingDelta { partial, .. } => { ... }
        AssistantMessageEvent::ToolCallDelta { partial, .. } => { ... }
        AssistantMessageEvent::Done { .. }
        | AssistantMessageEvent::Error { .. } => { ... }
    }
}
```

增量事件会被转换成 Agent 层事件：

```text
AssistantMessageEvent::Start
  → AgentEvent::MessageStart

TextDelta / ThinkingDelta / ToolCallDelta
  → AgentEvent::MessageUpdate

Done / Error
  → AgentEvent::MessageEnd
```

最终通过：

```rust
response.result().await?
```

拿到完整的 `AssistantMessage`。

---

## 5. Provider 和消息协议解析

### 5.1 Provider 抽象

Provider trait 位于 [`pi-ai/src/provider.rs`](../crates/rpi-ai/src/provider.rs)。

核心方法是：

```rust
async fn stream_simple(
    &self,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) -> AssistantMessageEventStream;
```

### 5.2 Anthropic Provider

Anthropic 的入口位于 [`anthropic/mod.rs:135`](../crates/rpi-ai/src/providers/anthropic/mod.rs#L135)：

```rust
async fn stream_simple(
    &self,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) -> AssistantMessageEventStream
```

它不会直接返回完整消息，而是：

1. 创建 `AssistantMessageEventStream`。
2. 启动一个 producer task。
3. 后台执行 HTTP 请求。
4. 读取 SSE。
5. 把 SSE 转换成 `AssistantMessageEvent`。
6. 推入 event stream。

### 5.3 通用事件流

事件流定义在 [`event_stream.rs:25`](../crates/rpi-ai/src/event_stream.rs#L25)：

```rust
pub struct AssistantMessageEventStream
```

生产者和消费者的关系：

```text
Provider 后台 task
    ↓ producer.push(event)
AssistantMessageEventStream
    ↓ response.next().await
Agent Loop
```

创建函数位于 [`event_stream.rs:154`](../crates/rpi-ai/src/event_stream.rs#L154)：

```rust
create_assistant_message_event_stream()
```

### 5.4 SSE 分帧和解析

SSE 流位于 [`anthropic/sse.rs:190`](../crates/rpi-ai/src/providers/anthropic/sse.rs#L190)：

```rust
pub struct SseEventStream
```

读取下一个 SSE 事件：

```rust
pub async fn next_event(
    &mut self,
) -> Result<Option<ServerSentEvent>, AiError>
```

位置：[`sse.rs:213`](../crates/rpi-ai/src/providers/anthropic/sse.rs#L213)

协议解析链路：

```text
HTTP response body
  ↓
SSE frame
  ↓
Anthropic raw event
  ↓
AssistantMessageEvent
  ↓
AssistantMessage
```

典型映射：

```text
message_start
  → AssistantMessageEvent::Start

content_block_delta / text_delta
  → AssistantMessageEvent::TextDelta

content_block_delta / input_json_delta
  → AssistantMessageEvent::ToolCallDelta

message_stop
  → AssistantMessageEvent::Done
```

---

## 6. Tool Call 分支

### 6.1 识别 ToolCall

在 [`agent_loop.rs:300`](../crates/rpi-agent/src/agent_loop.rs#L300) 附近，从 assistant message 的 content 中提取：

```rust
Content::ToolCall(tool_call)
```

### 6.2 执行工具

工具批处理入口：[`agent_loop.rs:624`](../crates/rpi-agent/src/agent_loop.rs#L624)

```rust
async fn execute_tool_calls(...)
```

它会根据配置选择：

```text
Sequential
  → execute_tool_calls_sequential

Parallel
  → execute_tool_calls_parallel
```

实际调用 `AgentTool::execute` 的位置：[`agent_loop.rs:1025`](../crates/rpi-agent/src/agent_loop.rs#L1025)

```rust
tool.execute(
    &tool_call.id,
    args.clone(),
    child_token,
    on_update,
).await
```

工具接口定义在 [`agent_tool.rs`](../crates/rpi-agent/src/agent_tool.rs)：

```rust
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn schema(&self) -> &Tool;

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError>;
}
```

工具执行结果会转换成：

```text
AgentToolResult
  ↓
ToolResultMessage
  ↓
AgentMessage::ToolResult
  ↓
current_context.messages
```

然后 Agent Loop 再次请求模型。

---

## 7. AgentEvent 如何回到 TUI

Agent Loop 产生的事件通过 `BroadcastEmitter` 广播。

TUI 的消费入口是 [`interactive_tui.rs:8752`](../crates/rpi-cli/src/interactive_tui.rs#L8752)：

```rust
async fn drain_agent_events(
    mut rx: broadcast::Receiver<AgentEvent>,
    ...
)
```

它不断接收事件：

```rust
while let Ok(event) = rx.recv().await {
    handle_agent_event(event, ...).await;
}
```

具体渲染逻辑位于 [`interactive_tui.rs:8774`](../crates/rpi-cli/src/interactive_tui.rs#L8774)：

```rust
async fn handle_agent_event(...) {
    match event {
        AgentEvent::AgentStart => { ... }
        AgentEvent::MessageStart { message } => { ... }
        AgentEvent::MessageUpdate { message, .. } => { ... }
        AgentEvent::ToolExecutionStart { .. } => { ... }
        AgentEvent::ToolExecutionUpdate { .. } => { ... }
        AgentEvent::ToolExecutionEnd { .. } => { ... }
        AgentEvent::AgentEnd { .. } => { ... }
    }
}
```

最终的显示链路是：

```text
Provider SSE
  ↓
AssistantMessageEvent
  ↓
AgentEvent::MessageUpdate
  ↓
BroadcastEmitter
  ↓
TUI drain_agent_events
  ↓
transcript_view
  ↓
terminal render
```

---

## 8. 推荐源码阅读顺序

### 第一遍：只看核心循环

1. [`run_agent_loop`](../crates/rpi-agent/src/agent_loop.rs#L72)
2. [`run_loop`](../crates/rpi-agent/src/agent_loop.rs#L220)
3. [`stream_assistant_response`](../crates/rpi-agent/src/agent_loop.rs#L441)
4. [`execute_tool_calls`](../crates/rpi-agent/src/agent_loop.rs#L624)

### 第二遍：看消息类型

1. [`pi-ai/src/types.rs`](../crates/rpi-ai/src/types.rs)
2. [`pi-agent/src/message.rs`](../crates/rpi-agent/src/message.rs)
3. [`pi-agent/src/events.rs`](../crates/rpi-agent/src/events.rs)

重点理解：

```text
AgentMessage
Message
UserMessage
AssistantMessage
ToolResultMessage
Content
AssistantMessageEvent
AgentEvent
```

### 第三遍：看 Harness

1. [`prompt_text`](../crates/rpi-harness/src/agent_harness.rs#L3761)
2. [`run_core`](../crates/rpi-harness/src/agent_harness.rs#L2890)
3. [`run_core_with_entry`](../crates/rpi-harness/src/agent_harness.rs#L2903)
4. [`build_stream_fn`](../crates/rpi-harness/src/agent_harness.rs#L2362)
5. [`run_agent_loop 调用`](../crates/rpi-harness/src/agent_harness.rs#L3332)

### 第四遍：看 Provider

1. [`StreamFn`](../crates/rpi-agent/src/stream_fn.rs#L28)
2. [`AiProvider::stream_simple`](../crates/rpi-ai/src/provider.rs)
3. [`Anthropic::stream_simple`](../crates/rpi-ai/src/providers/anthropic/mod.rs#L135)
4. [`SseEventStream`](../crates/rpi-ai/src/providers/anthropic/sse.rs#L190)
5. [`AssistantMessageEventStream`](../crates/rpi-ai/src/event_stream.rs#L25)

---

## 9. 一句话总结

> `pi-agent/src/agent_loop.rs` 负责“模型请求—工具执行—再次请求”的决策循环；`pi-harness/src/agent_harness.rs` 负责把 session、持久化、压缩和工具配置接到这个循环上；`pi-ai/src/providers/anthropic/` 负责把 HTTP/SSE 协议解析成 Agent Loop 能消费的 `AssistantMessageEvent`。
