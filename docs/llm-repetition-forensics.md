# 「LLM 一直在重复思考」问题复盘

审计日期：2026-09-25
涉及仓库：`pi-rust`（`crates/pi-agent`、`crates/pi-ai`、`crates/pi-harness`、`crates/pi-cli`、`crates/pi-tui`）

本文记录一次「模型看起来卡在重复思考 / 第二轮不回复」的排查过程、证据、完整代码链路，
以及哪些断点被确认、哪些被排除。目的不是给结论，而是给一份**可复核的调用链**，
下次出现同类症状可以照着断点逐段验证。

---

## 一、上报症状（三条，实测后判定为两个不同问题）

| # | 用户描述 | 实测判定 |
|---|---|---|
| 1 | 「LLM 一直在重复思考」 | **不是逐字复读，是每轮重新推导计划**（re-plan）。见 §二 |
| 2 | 「第二轮发送的内容 llm 没有回复，但动画在转」 | **不是第二轮 bug**。是 run 从未返回（provider 慢 + O(n²) 热点放大）。见 §五 |
| 3 | 「Ctrl+C 无法退出，卡死」 | 真 bug：`Aborting` 状态下 Ctrl+C 被吞掉，而主循环被 run 占住读不到 `Exit`。见 §六 |

---

## 二、症状 1 的证据：不是复读，是 re-plan

被观察的会话：`.pi/sessions/--D--Projects-pi-rust--/2026-09-24T16-04-18-236Z_01a0d429-*.jsonl`
（271 行 / 662 KB，cwd = `D:\Projects\pi-rust`）

| 指标 | 值 |
|---|---|
| user / assistant / toolResult 条目 | 3 / **114** / 148 |
| `stopReason` 分布 | `tooluse` ×112、`stop` ×2、**`error` ×0** |
| 工具调用 | bash 115、read 15、todo 6、**edit 11、write 1** |
| 产出体量 | thinking 48,559 字符，正文仅 20,432 字符（约 425 / 180 字符每轮） |
| 同一文件重复读取 | `editor.rs` 9×、`tool_execution.rs` 8×、`interactive_tui.rs` 5× |
| 单次 run 时长 | `operation_started` → `operation_finished` = **867.7 秒 / 112 轮** |

关键判断：

- `thinking` 块**措辞各不相同**（不是逐字重复），但主题完全一致：每轮都在写
  `The user wants me to: 1..5 / Let me explore… / Let me continue exploring…`。
- 114 轮里只有 12 次真正写文件（edit+write），约 90% 的轮次是只读探索。
- 期间两次出现 `The user is greeting me with "hello"`，但**整个会话没有任何 "hello" 用户消息** ——
  模型在丢失自己上一轮推理的情况下自造了用户输入。

所以「重复思考」的实质是：**模型没有可依赖的持久计划状态，每轮从用户那 5 条需求重新推导一遍。**

---

## 三、完整代码链路

### 3.1 提交到 run 启动

```
用户按 Enter
└─ crates/pi-cli/src/interactive_tui.rs:5956   editor.on_submit 闭包（阻塞在 key 线程）
   ├─ text.starts_with('/')                 → dispatch_slash(..)
   ├─ parse_user_bash(text)                 → start_user_bash(..)     【! / !! 直连 shell】
   ├─ RunStatus != Idle                     → lane.steer(msg)
   │                                          排队；等 MessageStart 回显，不动 run
   └─ RunStatus == Idle
      ├─ state.try_start_working()          → apply_status(Working)   ← 动画起点
      ├─ add_user_message(chat, text)                                  ← 直发在此回显（见 §四.4）
      ├─ push_history(state, text)
      └─ tx.send(TuiMessage::UserInput(text))
         └─ interactive_tui.rs:7719  run_prompt_streaming(..)
            ├─ state.set_status(Working)
            ├─ ensure_js_runtime_before_prompt(..)   ← JS 扩展 before_agent_start 可在此阻塞
            └─ lane.prompt_text(prompt, images)      ──────────────┐
                                                                   │
harness 层                                                         │
└─ crates/pi-harness/src/agent_harness.rs:1799  run_core(prompts) ◄──┘
   ├─ :1805  next_run_queue 取出排队 prompt，接到本次 prompts 前面
   ├─ :1812  finish_interrupted_operation(..)
   │          把上一次没写 operation_finished 的 run 记成 aborted/interrupted
   ├─ :1840  逐条 append_message(prompt)        ← prompt 先落盘（durable-first）
   ├─ :1881  branch_path_oldest_first()         ← 上下文来源：整条分支
   ├─ :1886  snapshot_config() → should_compact(..)
   │        └─ compact(&prep, &llm_opts).await  ← 预压缩会额外打一次 LLM
   ├─ :1964  压缩后重建 path
   ├─ :1974  agent_context = build_session_context(&path)   ← ★ 已包含 prompt
   ├─ :2102  run_agent_loop(Vec::new(), ..)     ← ★★ 传空 prompts
   └─ :2159  persist_new_messages(&new_messages[cut..])      ← ★★ 只在 run 结束才落盘
```

### 3.2 agent loop

```
crates/pi-agent/src/agent_loop.rs:72  run_agent_loop(prompts, context, config, emit, stream_fn)
  ├─ :79   new_messages = prompts.clone()            ← ★ 这里传入的是空 vec
  ├─ :80   current_context.messages = context.messages + prompts
  ├─ :90   emit AgentStart
  ├─ :91   emit TurnStart
  ├─ :92   for prompt in &new_messages { emit MessageStart; emit MessageEnd }
  │          ★★ 空 vec ⇒ 循环 0 次 ⇒ 用户消息永远没有 message_start
  └─ :108  run_loop(current_context, new_messages, ..)
       └─ :176  run_loop
          ├─ :185  pending_messages = drain_steering(config).await
          ├─ :190  loop {
          │    :193    while has_more_tool_calls || !pending_messages.is_empty()
          │    :201       pending_messages.drain(..) → emit MessageStart/End
          │                 ← steering / follow-up 在这里回显（与直发路径不同）
          │    :223       stream_assistant_response(current_context, ..)
          │    :225       new_messages.push(Assistant(..))
          │    :227       if stop_reason ∈ {Error, Aborted} → emit TurnEnd/AgentEnd, return
          │    :248       收集 content 里的 tool_calls
          │    :259       if !tool_calls.is_empty()
          │    :262          stop_reason == Length ? fail_tool_calls_from_truncated_message(..)
          │    :264                                  : execute_tool_calls(..)
          │    :267       has_more_tool_calls = !batch.terminate
          │ }
```

### 3.3 单次 provider 请求

```
crates/pi-agent/src/agent_loop.rs:383  stream_assistant_response(&mut context, ..)
  ├─ :390  transform_context(context.messages.clone())
  ├─ :397  convert_to_llm(messages)                      ← AgentMessage[] → Message[]
  ├─ :399  llm_context = { system_prompt, messages, tools }
  ├─ :419  stream_fn(&model, &llm_context, &opts)
  └─ 消费 AssistantMessageEvent（`:423  while let Some(event) = response.next().await`）：
      ├─ Start     → :426 (**partial).clone() → push 进 context → emit MessageStart  ← ★ 深拷贝 #2
      ├─ *Delta    → :441 (**partial).clone() → emit MessageUpdate                     ← ★ 每个 delta 都拷
      └─ Done/Error→ response.result() → emit MessageEnd → return
```

### 3.4 provider 流式解码（以 openai-completions 为例）

```
crates/pi-ai/src/providers/openai_completions.rs
  ├─ :145  body = build_request(model, ctx, opts)     ← max_tokens / system / tools 在此成型
  ├─ :156  retry_provider_request(..)                 ← 可重试错误在此重试
  │          crates/pi-ai/src/providers/anthropic/retry.rs:160
  ├─ :158  http.post(url).timeout(opts.request_timeout())
  │          DEFAULT_LLM_API_TIMEOUT = 600s（crates/pi-ai/src/provider.rs:14）
  │          reqwest 语义：覆盖「连接 + 响应体读完」全程 ⇒ 整个流必须 10 分钟内结束
  ├─ :201  SseEventStream::new(response, signal)
  └─ :202  loop { events.next_event().await
       ├─ "[DONE]"  → state.finish(..) → return
       ├─ Ok(Some)  → state.apply_chunk(..)
       │                 └─ :651 apply_chunk
       │                    ├─ :676 delta["content"]           → :709 push_text
       │                    ├─ :683 delta["reasoning_content"] → :733 push_thinking
       │                    └─ :691 delta["tool_calls"]        → push_tool_delta
       │                       ★★★ 每条 delta 都 Arc::new(self.output.clone())  ← O(n²) 深拷贝 #1
       ├─ Ok(None)  → state.finish(..) → return
       └─ Err(e)    → state.error(..) → return
      }
```

SSE 解码器本体：

```
crates/pi-ai/src/providers/anthropic/sse.rs
  :213  next_event
    ├─ :221  while let Some((line, rest)) = consume_line(&leftover, self.done)
    ├─ :234  if self.done { …最后半行…; self.state.flush() }
    ├─ :258  tokio::select! {
    │          biased;
    │          _ = self.signal.cancelled() => return Err(Abort)   ← 取消必须能打断 body 读
    │          next = self.bytes_stream.next() => next
    │        }
    └─ (Ok(None) → self.done = true → continue)
  :59  decode_line     每个完整行；空行触发 flush
  :91  flush           交出 pending event
  :127 consume_line    切一行出来；无换行返回 None（⇒ 外层 while 正常退出）
```

### 3.5 回到 TUI

```
crates/pi-cli/src/interactive_tui.rs:8084  handle_agent_event（drain 任务，独立于主循环）
  ├─ :8092  AgentStart        → set_status(Working)
  ├─ :8097  AgentEnd          → set_status(Idle)
  │                            + flush pending_bash_messages
  │                            + refresh_pending_messages(..)
  ├─ :8164  MessageStart
  │    ├─ :8165 Assistant(a)  → 新建 AssistantMessageComponent, set_streaming(true)
  │    └─ :8219 User(user)    → add_user_message(..)   ← 排队消息的回显路径
  ├─          MessageUpdate(Assistant)
  │                            → comp.update_blocks(assistant_blocks(a))   ← 每个 delta 重绘
  └─          MessageEnd(Assistant)
                               → cache-miss tracker / footer usage / notice
```

### 3.6 动画与耗时（与 §六、§七 相关）

```
crates/pi-cli/src/interactive_tui.rs  render-tick 任务
  interval = rpi_tui::loader::SPINNER_FRAME_MS (= 80ms，对齐原生 DEFAULT_INTERVAL_MS)
  只 request_render(..)，不推帧
        └─ TuiAltScreen::do_render(:689)
             └─ Editor::render  ← 在这里推帧（镜像 Loader::render）
                  · follow: 每个 token 都排一帧 ⇒ 有输出时明显加快
                  · idle:   80ms tick 一帧 ⇒ 与原生 pi 同频

工具/bash 耗时：
  crates/pi-tui/src/tool_execution.rs   started_at / finished_at + format_elapsed
  crates/pi-tui/src/bash_execution.rs   started_at / finished_at + format_elapsed
```

---

## 四、逐个断点的判定

排查顺序即 §三 链路顺序。每条都给了结论和依据。

### 4.1 上下文是否丢失 / 错序？——**排除**

判定方法：从会话最后一条 entry 沿 `parentId` 反走到根。

结果：265 条 entry 全部在分支上，**seq 单调、无缺号、无重复、无环**。

⇒ `branch_path_oldest_first()` 交出的上下文是正确的，模型看到的就是完整对话。

### 4.2 是否 provider 报错触发重试导致重现？——**排除**

`stopReason` 统计：`error` ×0、`RetryScheduled` ×0。

重试逻辑在 `agent_harness.rs:2100-2145`：只有 `stop_reason == Error && is_retryable_assistant_error(..)`
才重试，且重试复用同一份 `agent_context`（失败尝试不落盘）。本次会话根本没进这条分支。

### 4.3 工具结果是否没回传？——**排除**

148 条 toolResult 内容长度：中位 839 字符、最大 44,139、仅 10 条 `isError`。

⇒ 工具输出确实进了上下文。

### 4.4 用户消息为什么看不到？——**确认为真 bug（已修）**

链路：
- `interactive_tui.rs:5956` 提交处理器**不再**在提交时 `add_user_message`；
- 改为依赖 `AgentEvent::MessageStart`（`interactive_tui.rs:8219`）；
- 但 harness 在 `agent_harness.rs:2102` 传的是 `Vec::new()`；
- 于是 `agent_loop.rs:92` 的 `for prompt in &new_messages` **一次都不执行** ⇒ 用户消息永远没有 `message_start`。

为什么 harness 要传空 vec：prompt 已在 `:1840` 落盘，`agent_context`（`:1974`）又是从分支路径构建的（已含 prompt），
再传一次会在 provider 上下文里重复计数。

**修复**：只在「直接发送」的三条路径恢复提交时渲染
（`interactive_tui.rs` 提交处理器、Alt+Enter 空闲分支、启动 `-p` 循环）；
排队（steer / follow-up）仍由 `agent_loop.rs:201` drain 时的 `MessageStart` 回显 —— 两者各渲染一次，不会重叠。

回归保护：`crates/pi-harness/tests/harness_run_e2e.rs::directly_sent_prompt_emits_no_user_message_start`
把这个「harness 不发用户 message_start」的契约钉住：如果哪天 harness 改成发，这个测试会红，
提醒必须同时去掉 TUI 的提交时渲染，否则会出现两条气泡。

### 4.5 `now_ms()` 是不是真时钟？——**确认为真 bug（已修）**

原实现（`crates/pi-agent/src/agent_loop.rs` 与 `crates/pi-agent/src/agent.rs` 各一份）：

```rust
fn now_ms() -> i64 {
    static T: AtomicI64 = AtomicI64::new(1);
    T.fetch_add(1, Ordering::Relaxed)   // 返回 1,2,3...
}
```

注释自称「测试确定性」，但 `create_tool_result_message`（`agent_loop.rs:1097`）在生产路径上用它。

实证：同一会话里 assistant 的 `message.timestamp` 是真实毫秒（`1790265928215`），
而 toolResult 的是 `29, 38, 55, 62, …`。

影响面：目前排序用插入顺序（`EntryOrder::OldestFirst` 直接迭代数组，见 `session/state.rs:295`），
所以上下文没被搞乱；但 JSONL 落盘的时间是垃圾，任何按时间戳的逻辑（cache idle 判定、导出、第三方读日志）都会错。

**修复**：新增 `crates/pi-agent/src/clock.rs::now_ms()` —— 真实 `SystemTime` + 单调下限（CAS 保证严格递增），
两处调用点改为 `crate::clock::now_ms()`。

### 4.6 一次 run 的消息何时落盘？——**确认为设计缺陷（未改）**

`agent_harness.rs:2159` 的 `persist_new_messages` 是在 retry `loop` 跳出**之后**才调用的。

实证：`01a0d43e` 里 run 1 的 `operation_started` 在 `…907317`，`operation_finished` 在 `…775022`（867.7 秒）；
而 seq 3..225 这 **223 条 entry 的 `timestamp` 全挤在 69 毫秒内**（`…774953`→`…775022`），
它们内部 `message.timestamp` 却是从 `…928215` 铺到 `…044215`。⇒ 整批是 run 结束时一次性 append 的。

后果：run 中途崩溃/被 kill，除了开头那条 prompt，整段历史全丢；`/tree`、`/session`、导出在 run 进行中也看不到进度。

为什么不能简单改成「每轮落盘」：per-attempt retry 复用同一份 `agent_context`，
若失败尝试的消息已落盘，重试会产生重复条目。要改需要先把 retry 改成从落盘后的 tip 重建上下文。

### 4.7 为什么「第二轮没回复但动画在转」？——**根因：run 未返回（provider 慢 + O(n²) 放大）**

证据链：

| 观测 | 值 |
|---|---|
| `01a0d43e` run 1 | `operation_started` 00:27:35 → **无 assistant、无 finish** → 00:33:27 被回收为 `aborted/interrupted`（352 s） |
| 同文件 run 2 | 00:33:33，**prompt 文本与 run 1 完全相同** ⇒ 说明第一轮什么都没出，用户重发了一遍 |
| `01a0d44a` run 1 | `hi`，`operation_started` 之后什么都没有 |
| **assistant 消息** | **一条都没有**（连 error 的都没有）⇒ run 从未返回 |
| 进程状态 | **烧 CPU（约 1 核），不是阻塞** |
| 端点本身 | 直接 curl → HTTP 200 / 5.4 s；00:48 之后 `rpi -p` 3 秒返回 |
| 多轮复现 | 单轮 `-p` 与 `--continue` 双轮、临时目录与项目目录，**全部正常** |

⇒ 是 00:27–00:46 一段 provider 侧慢/抽风窗口（`models.json` 在 00:20 被重写过），**不是第二轮逻辑坏了**。

**被找到的放大器**：每个 delta 深拷贝整条累积消息，而且拷两次。

- provider 侧：`openai_completions.rs` 共 10 处 `partial: Arc::new(self.output.clone())`（`:646 :718 :729 :747 :758 :784 :822 :836 :847 :865`）
- agent loop 侧：`agent_loop.rs:426`（Start）与 `:441`（每个 delta）各再来一次 `(**partial).clone()`

实测（临时探针，测完已删除）：

```
deltas=  500  elapsed=1.09ms
deltas= 2000  elapsed=13.4ms    (4× 增量 → 12× 时间)
deltas= 8000  elapsed=105ms     (4× 增量 → 7.8× 时间)
```

明显 O(n²)。当前默认模型 `qwen3.7-plus` 会吐 `reasoning_content`，
**一句 "hi" 就产出 232 个 reasoning token**；真实 prompt 轻易上万 delta。
外推 10 万 delta ≈ provider 侧纯拷贝 16 秒，乘 2–3 倍（agent loop + TUI 重绘）⇒ 几十秒到几分钟满核 CPU、
界面什么都不显示，只受 `DEFAULT_LLM_API_TIMEOUT = 600s` 兜底。

**这就是「没有回复但动画在转」的完整解释。**

建议修法（**未做**，需要改事件契约）：
- 方案 A：`partial` 改为共享（`Arc<Mutex<..>>` 或让 `MessageUpdate` 不携带快照，只带 delta + index），
  消费方在需要时才 clone；
- 方案 B：provider 内合并 delta（按字符数或时间窗），把事件数量降一个量级；
- 两者都应配一个「delta 数与耗时增长不成平方」的回归测试。
- 参考：`crates/pi-ai/src/types.rs:1015` 已有 `AssistantMessageEvent::partial()` 辅助函数，可作为收敛点。

### 4.8 todo 工具为什么会重复登记？——**确认为真 bug（已修）**

`.rpi/todo.json` 原始字节显示一个合法数组后面跟着残渣：

```
[ {id:1 …}, {id:2 …} ]
  { "id": 3, … }
]
```

这是**非截断覆盖写**的典型形状：先写了 3 项的较长文档，又写了 2 项的较短文档且没有 truncate，
旧尾巴留在 `]` 之后。严格 JSON 解析必然失败（实测 `Extra data: line 19 column 3`），
插件自带的 `load` 虽然能容忍拼接 JSON，但这里连它也会被尾部那个 `]` 卡住。

内容上也重复：#1 = `工具名改为小写 (tool_label …)`，#2 = `1. 工具名改为小写 (tool_label …)`
—— 模型把**带编号的计划行原文**当 todo 项传了进来。全会话 6 次 `todo` 调用只建出 2 个真实条目，
5 项计划从未被完整登记，这正是 re-plan 的直接燃料。

**修复**（`rpi-package/packages/rpi-todo/src/lib.rs`）：
- `save` 改为 temp 文件 + `rename`（Unix / Windows 均原子替换），消灭整类「非截断覆盖」损坏；
- `normalize_text` 去掉 `1.` / `12)` / `-` / `*` / `•` / `·` 前缀并折叠空白（`1.5x` 这类小数不误伤）；
- `add` 对**已 pending** 的同名任务返回既有 id，不再重复登记；
- 检测发现 `rpi-goal` / `rpi-memory` / `rpi-permissions` 有完全相同的非原子写，已一并改为 `write_store_atomically`。
- 已损坏的 `D:\Projects\pi-rust\.rpi\todo.json` 已就地修复：用容错解析器取出全部 3 条（含尾部残渣里的 #3），
  丢掉与 #1 重复的 #2，剥掉 #3 的 `2. ` 前缀，用 `ensure_ascii=False` 重写成严格合法 JSON
  （#1 保留 `done: true`）。修前该文件让插件的 `load` 直接报错，todo 工具整个不可用。

### 4.9 working 动画频率——**确认为真 bug（已修）**

两条独立缺陷：

1. **双推帧**：tick 任务既调 `editor.tick_working()`（推一帧），又 `request_render(..)`
   → `Editor::render` 再推一帧 ⇒ 空闲时 **40ms/帧，正好是原生 pi 的两倍速**。
2. **帧序抄错**：`editor.rs` 里硬编码的数组第 2 帧是 `⠘`，原生 `DEFAULT_FRAMES` 是 `⠙`。

**修复**：
- tick 只 `request_render(..)`，推帧统一由 `Editor::render` 负责（先取快照再前进，
  首帧渲染 0，与原生 `start()` 行为一致）⇒ 空闲 **80ms/帧、10 帧 800ms 一轮**，与原生同频；
  有 token 时每次重绘都推一帧，仍保留「按 token 输出加速」的特性。
- 帧集收敛为 `crates/pi-tui/src/loader.rs::SPINNER_FRAMES`（+ `SPINNER_FRAME_MS = 80`），
  `Loader` 与编辑器顶栏共用，不再各抄一份。
- 确认 `request_render_reusing_scroll_content()` 只缓存 transcript（`layout.rs:346` 的
  `sv.render_with_cached_content`），编辑器在 dock 中每帧都会真的走 `render`，空闲动画不会被缓存冻住。

### 4.10 Ctrl+C 无法退出——**确认为真 bug（已修）**

链路事实：
- `run_prompt_streaming(..).await` 在**主循环内联 await 整个 run**（`interactive_tui.rs:7719`），
  所以 run 进行中主循环根本不会去读 channel；
- 按键线程只在 `RunStatus::Idle` 时发 `TuiMessage::Exit`；
- `RunStatus::Aborting` 分支原本是空的（注释：`Keep waiting for the in-flight cancellation…`）。

⇒ 一旦 abort 没落地（provider 卡住 / 工具不返回），`Exit` 永远排在 channel 里读不到，进程无法退出。

**修复**：`Aborting` 状态下的**第二次有意按下** Ctrl+C 直接恢复终端并退出（`130 = 128+SIGINT`）。
过滤 `KeyEventKind::Repeat`，长按不会误杀。

---

## 五、三个 bug 的共同形状

三个独立缺陷指向同一个设计取舍：**把「事件驱动的 UI 状态」建立在「不保证存在的事件」上**。

| bug | 依赖的事件 | 实际发生 |
|---|---|---|
| 4.4 用户消息不可见 | 直发 prompt 的 `MessageStart` | harness 传空 prompts 发不出 |
| 4.6 run 中途丢历史 | run 结束才 `persist_new_messages` | 长 run 内没有任何落盘点 |
| 4.10 Ctrl+C 无效 | 主循环能读到 `Exit` | 主循环被 `prompt_text(..).await` 占住 |

判据可以记成一句：**任何「等某个事件来做 X」的代码，都要在链路里确认那个事件一定发得出来。**

---

## 六、排查命令备忘

```bash
# 1) 会话概览：角色、stopReason、工具直方图
python - <<'PY'
import json,collections,sys
p=sys.argv[1]; roles=collections.Counter(); stops=collections.Counter(); tools=collections.Counter()
for l in open(p,encoding='utf-8'):
    l=l.strip()
    if not l: continue
    d=json.loads(l)
    if d.get('kind')!='entry': continue
    m=d.get('message') or {}
    roles[m.get('role')]+=1
    if m.get('role')=='assistant':
        stops[m.get('stopReason')]+=1
        for c in m.get('content',[]):
            if c.get('type')=='toolCall': tools[c.get('name')]+=1
print('roles',dict(roles)); print('stops',dict(stops)); print('tools',dict(tools))
PY <session.jsonl>

# 2) 分支完整性：从末条沿 parentId 反走，检查单调/缺号/重复/环
#    （脚本见本文 §四.4 判定过程；关键断言：chain 覆盖全部 entry 且 seq 严格递增）

# 3) 落盘时机：对比 entry.timestamp 与其中 message.timestamp 的分布
#    两者严重脱节 ⇒ 该批 entry 是在之后一次性 append 的

# 4) 是阻塞还是空转
powershell -NoProfile -Command "Get-Process rpi | Select Id,CPU,WS"

# 5) provider 流形状（是否 reasoning_content、是否发 [DONE]）
curl -sS -N -X POST "$BASE_URL/chat/completions" \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model":"…","messages":[{"role":"user","content":"hi"}],"stream":true}' | head -20
```

---

## 七、当前状态汇总

| 项 | 状态 | 落点 |
|---|---|---|
| 4.4 直发用户消息回显 | 已修 | `interactive_tui.rs` 三处提交时渲染 + harness 回归测试 |
| 4.5 `now_ms()` 计数器 | 已修 | `pi-agent/src/clock.rs`（新增），`agent_loop.rs` / `agent.rs` 调用点 |
| 4.8 todo 非原子写 + 重复登记 | 已修 | `rpi-package/packages/rpi-todo/src/lib.rs`（含 3 个兄弟包） |
| 4.9 working 动画频率 / 帧序 | 已修 | `pi-tui/src/editor.rs`、`loader.rs`、`interactive_tui.rs` tick |
| 4.10 Ctrl+C 无法退出 | 已修 | `interactive_tui.rs::emergency_exit` |
| 4.7 O(n²) per-delta 深拷贝 | **未修**（需改事件契约） | `pi-ai/providers/openai_completions.rs`、`pi-agent/agent_loop.rs:426,441` |
| 4.6 run 中途不落盘 | **未修**（需先改 retry 语义） | `pi-harness/agent_harness.rs:2159` |
| re-plan 本身（无持久计划 / thinking 不回传 / 无轮数上限） | 策略层，未处理 | 见 §二 |
