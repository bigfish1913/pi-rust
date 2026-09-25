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

### 4.6 一次 run 的消息何时落盘？——**确认为设计缺陷（未改，已展开为 §十一）**

`agent_harness.rs:2179` 的 `persist_new_messages` 是在 retry `loop` 跳出**之后**才调用的
（定义在 `:1698`）。

实证：`01a0d43e` 里 run 1 的 `operation_started` 在 `…907317`，`operation_finished` 在 `…775022`（867.7 秒）；
而 seq 3..225 这 **223 条 entry 的 `timestamp` 全挤在 69 毫秒内**（`…774953`→`…775022`），
它们内部 `message.timestamp` 却是从 `…928215` 铺到 `…044215`。⇒ 整批是 run 结束时一次性 append 的。

后果：run 中途崩溃/被 kill，除了开头那条 prompt，整段历史全丢；`/tree`、`/session`、导出在 run 进行中也看不到进度。

为什么不能简单改成「每轮落盘」：per-attempt retry 复用同一份 `agent_context`，
若失败尝试的消息已落盘，重试会产生重复条目。要改需要先把 retry 改成从落盘后的 tip 重建上下文。

**§十一 展开了这一条的完整证据链**（逐时间线 + 真实会话 + 与原生 pi 的逐项对照）。
展开后最重要的修正是：这**不是**「flush 得不够勤」，而是 rpi 的 run 路径
**从不写入任何进度记录**（`StepAttempt`/`ToolStarted`/`WriteDeferred` 有类型、reducer 能读，
但生产路径一次都没 append），而原生 pi 是**每帧 `appendList` 提交**并能在崩溃后按帧前缀重建。

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

**放大器是真的，但它不是「卡住」的原因** —— 见 §9.4 的订正。
它降低的是长流式输出的成本上限，不能解释那 6 分钟无输出：那段时间 provider 请求本身
没有返回（run 未产生任何 assistant 消息），而 O(n²) 只有在消息已经很长的**流式过程**中才会
显著。两件事都要修，但不是同一个病因。

建议修法：
- 已做：方案 B（provider 内按字节+时间窗合并 delta），见 §9.4；
- 未做：方案 A（`partial` 改为共享，或让 `MessageUpdate` 只带 delta + index），
  这是唯一能去掉指数项的做法，需要动事件契约。
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
| re-plan 本身（无持久计划 / thinking 不回传 / 无轮数上限） | **见 §八 / §九**：根因已定位并证明；§9.1 提示词规则、§9.2 run 级 120 轮护栏已实现并验证；§8.8 第 2 项为配置改动；仅第 4 项（O(n²) 深拷贝）未做 |

---

## 八、re-plan 因果链（「LLM 重复思考」的原因）

**一句话：模型看不到自己的计划——计划只写在 `thinking` 里，而 `thinking` 在构造下一次请求时被丢掉了。**

### 8.1 第 0 层：计划 100% 只存在于 `thinking`

会话 `01a0d429` 的通道构成：

| 通道 | 体量 | 内容 |
|---|---|---|
| `thinking` | **48,559 字符** | **全部计划**："The user wants me to: 1..5"、"Let me explore…" |
| `text` | 20,432 字符 | 114 轮里 **66 轮（57%）正文为空**；其余是 "Let me look at X"，无状态 |
| `toolResult` | **326,627 字符** | 148 条，中位 840，最大 44,139 |

### 8.2 第 1 层：openai-compat 组请求时丢弃 `thinking`（根因）

`crates/pi-ai/src/providers/openai_completions.rs:272`：

```rust
let assistant_content = if requires_thinking_as_text(model) {
    // …把 thinking 变成 text part 发回去
} else {
    let text = Content::text_only(&message.content, "\n");   // ← 只取 Text
    ...
};
```

`Content::text_only`（`crates/pi-ai/src/types.rs:244`）只 filter `Content::Text`，
`Content::Thinking` **整块丢弃**。开关 `requires_thinking_as_text`（同文件 `:545`）读
`compat.requires_thinking_as_text`，**默认 `false`**。

发生故障的 provider 正好落在默认值上：

```
providers.alicoding keys : [name, baseUrl, api, apiKey, authHeader, headers, models]
qwen3.7-plus keys        : [id, name, reasoning, input, contextWindow, maxTokens]
  reasoning = False        compat = None
```

⇒ 每轮发给该模型的 assistant 消息，**只剩工具调用和那几句 "Let me look at X"**。
它上一轮推导出的 5 条计划，对它自己不存在。

**对照（同一仓库的另一条 provider 路径）**：Anthropic 路径**保留** thinking ——
`crates/pi-ai/src/providers/anthropic/build_params.rs:771` 会把 `Content::Thinking`
连同 signature 一起发回去。

⇒ **这是 provider-specific 的**。用 Anthropic 模型时模型能看到自己上一轮推理，
这个 re-plan 循环就不容易成形；用 openai-compat 的 reasoning 模型（且没开 compat）时会。

⚠️ 需要说清楚：**「丢 thinking」这个行为本身是对的**。DeepSeek 等 openai-compat 端点明确要求
不要把 `reasoning_content` 回传。真正的设计问题是——**把唯一的计划载体放进了会被丢弃的通道**。

### 8.3 第 2 层：丢掉之后还剩什么

模型能看到的「自己的东西」只有 48 轮 "Let me look at X"（20K 字符），加上 326 KB 工具输出。

**比例 16 : 1。** 那 5 条需求是一粒信号，埋在 326 KB 文件转储里。
所以它每轮都得把那粒信号重新捞出来、重新推导一遍 —— 这就是「thinking 措辞各不相同、
主题却完全一致」的原因：不是复读，是**每轮从同一起点重新推理**。

### 8.4 第 3 层：没有任何机制把计划重新注入上下文

两个 todo 实现都查过：

**JS 插件 `@juicesharp/rpiv-todo`**（`~/.pi/agent/npm/node_modules/@juicesharp/rpiv-todo/index.ts:194-288`）
注册的事件是：

```
session_start   session_compact   session_tree   session_shutdown
tool_execution_end   agent_start
```

做的事全是 `updateTodoOverlay()` / `setWidget(KEY, …)` —— **纯 UI overlay**。
没有 `before_agent_start` 注入、没有 system prompt 增强、没有任何往 context 塞 todo 的 hook。

**Rust 插件 `rpi-todo`**：只在**工具调用的返回值**里带完整列表（`display_list(&items)`）。

⇒ 计划只在模型主动调用 todo 的那一瞬间进入上下文。而那 114 轮里它只调了 **6 次**，
于是从第 7 轮到第 113 轮之间的大段区间，上下文里**没有任何「我计划做什么 / 做到哪」的记录**。

### 8.5 第 4 层：循环没有刹车

`crates/pi-agent/src/agent_loop.rs:190`：

```rust
while has_more_tool_calls || !pending_messages.is_empty()
```

配上 `crates/pi-harness/src/agent_harness.rs:1993-1994`：

```rust
should_stop_after_turn: None,
prepare_next_turn: None,
```

⇒ **唯一的出口是「模型这一轮不再发 tool call」。没有轮数、token、无进展任何一种上限。**

而每轮都发一个 grep，grep 总有输出 ⇒ `has_more_tool_calls` 永远为 true
⇒ 单次 run 跑到 **112 轮 / 867 秒**。再叠加 §4.7 的 O(n²) per-delta 深拷贝，代价被进一步放大。

### 8.6 第 5 层：直接症状 —— 模型自造用户输入

会话里两次出现：

> The user is greeting me with "hello". This is a simple greeting.

而**整个会话没有任何 "hello" 用户消息**（只有 3 条 user：原始需求、"你继续吧"、"修改下吧"）。
在「看不到自己上一轮推理」的前提下，模型对会话状态的认知只能从工具输出反推；
反推失败就自己造一个输入。这是 8.2 的直接后果，也是最能说明问题的证据。

### 8.7 验证记录：哪些是证明，哪些只是迹象

**已证明（确定性）**

| 命题 | 证据 |
|---|---|
| 计划只在 thinking | `01a0d429` 统计：thinking 48,559 vs text 20,432；66/114 轮正文为空 |
| openai-compat 丢弃 thinking | `openai_completions.rs:272` + `types.rs:244`；默认 `false` 见 `:545` |
| Anthropic 保留 thinking | `anthropic/build_params.rs:771` |
| `compat` 确实能从 models.json 传到 provider | 新增测试 `config::tests::models_json_compat_reaches_the_model_for_openai_completions`；开关本身的语义由既有测试 `applies_message_compat_and_preserves_tool_result_images` 钉住（断言 `body["messages"][1]["content"][0]["text"] == "private"`） |
| 没有重新注入计划的机制 | JS 插件事件列表（`index.ts:194-288`）全是 UI；Rust 插件只在工具返回值里带列表 |
| 没有轮数上限 | `agent_loop.rs:190` + `agent_harness.rs:1993-1994` |

**只是迹象（n=1，且存在混淆变量）**

给 `qwen3.7-plus` 加上 `compat: { "requiresThinkingAsText": true }` 前后，同一个任务（统计
`SPINNER_FRAMES` / `SPINNER_FRAME_MS` 的定义与引用并写文件）各跑一次：

| | A（无 compat） | B（有 compat） |
|---|---|---|
| assistant 轮数 | 7 | **5** |
| 工具调用 | 7（bash 5、write 1、read 1） | **4**（bash 2、write 1、read 1） |
| Σ input tokens | 32,595 | **8,802** |
| Σ output tokens | 2,513 | 1,864 |
| 墙钟 | 60 s | 43 s |
| 工具报错 | **2**（`cd D:\Projects\pi-rust` 被 bash 吃成 `cd D:Projectspi-rust`） | **0** |

**必须说明的混淆变量**：A 多出的那 2 轮正是被反斜杠转义搞坏的那两次 `cd` 重试，
不是 re-plan。把这两次剔掉，A 的有效下限约 5 次调用 vs B 的 4 次 —— **落在单样本噪声范围内**。

⇒ 结论：**这个任务太短（4–7 次调用就做完），无法区分 re-plan 行为。**
上表只能证明「打开 compat 没有回归、运行正常」，**不能**证明「模型因此少 re-plan」。
要验证行为差异需要一个 50+ 轮的长任务，成本高且单样本仍不可靠；
更稳的做法是先把 8.8 的护栏做出来，再用「护栏触发次数」作为可观测量。

**顺带发现（与本次故障无关，但值得单独记录）**

1. **`/tmp` 在 bash 与 read/write 工具之间解析不一致**：git-bash 里 `/tmp` = `D:/Temp`，
   而 read/write 工具把 `/tmp/x` 解析到 `D:\tmp\x`。模型「写入 /tmp/x 再读回」能成功
   （两侧都用文件工具的解析），但**用户在 shell 的 /tmp 下找不到**；
   若模型用 `bash cat /tmp/x` 去读文件工具刚写的内容，就会假性 not-found。
2. **该端点不回报缓存字段**：`cache_read` / `cache_write` 全为 0/缺失，
   所以 §4.7 提到的 cache-miss 提示在这条 provider 上永远不会触发。

### 8.8 修法（按性价比排序）

| # | 做法 | 效果 | 代价 |
|---|---|---|---|
| 1 | **把计划从 thinking 通道搬到 text 通道**：靠提示词/工具设计让模型把计划写成正文（或强制「先 todo 再动手」） | 治本。计划进了 text 就会被回传 | 零风险，但依赖模型配合 |
| 2 | 给 openai-compat reasoning provider 开 `compat: { "requiresThinkingAsText": true }` | 一行配置即让模型看到自己推理 | 提示词暴涨（该会话 48 K 字符 thinking 每轮都发）、缓存重计费、端点可能拒收或质量下降 |
| 3 | **加护栏**：run 级轮数/token 上限 + 无进展检测（连续 N 轮只有只读工具、未改任何文件就停） | 把「14 分钟 112 轮」变成一次可解释的停止；并提供可观测的验证指标 | 需设计阈值 |
| 4 | 修 §4.7 的 O(n²) per-delta 深拷贝 | 长 run 不再满核空转 | 需改事件契约 |

**已做的配置改动**：`~/.rpi/agent/models.json` 的 `alicoding.qwen3.7-plus` 已加上
`"compat": { "requiresThinkingAsText": true }`，原文件备份在
`~/.rpi/agent/models.json.ab-backup-1790269893`。要回退就覆盖回去。

---

## 九、本轮实际动手修了什么（配套 §八）

§8.8 列了四项。本轮做了 **1（把计划搬进可见通道）** 和 **3（护栏）**，
并把「无进展检测」用真实数据**否决**掉了。

### 9.1 修：把工作状态放进可见通道（§8.8 第 1 项）

`crates/pi-cli/src/session.rs::default_system_prompt` 新增三条规则：

```
- Keep your working state in your VISIBLE replies, not only in reasoning. Reasoning is not
  carried into your next turn: only the text you write and the tool output you produce come
  back. Before each batch of tool calls, write one short line naming the task you are on and
  what remains. When you finish a step, say which one is done. If you keep the plan only in
  your head you will re-derive it from scratch every turn.
- Track multi-step work with the todo tool instead of re-listing the plan in prose: add the
  steps once, then mark them done as you go. Re-stating the same plan without acting on it is
  a bug, not progress.
- Read only what you need. If a file or search result was already shown earlier in this
  conversation, use it instead of fetching it again.
```

规则里写出了**机制**（"reasoning 不会带到下一轮"），不只是"要写清楚"——否则模型没理由改行为。

验证（确定性）：
- `cargo test -p rpi-cli --lib default_prompt` —— 新增
  `default_prompt_keeps_working_state_in_the_visible_channel`，逐条断言机制、补救手段、
  反重复规则、反重读规则都在提示词里。
- 上到真实链路：`rpi --debug-system-prompt -p x` 的输出里能 grep 到这三条
  （说明它们确实进了最终发给 provider 的 system prompt，而不是只存在于源码）。

### 9.2 修：run 级轮数护栏（§8.8 第 3 项）

新增 `crates/pi-harness/src/run_budget.rs`：

| 项 | 值 / 说明 |
|---|---|
| `DEFAULT_MAX_TURNS_PER_RUN` | **120** |
| `RunBudget::observe_turn()` | 每轮调一次，到顶返回 `Some(BudgetStop::TurnLimit{..})` |
| `RunBudget::stop()` | 事后读取「是哪次 stop 结束了本轮 run」，供落盘用 |
| `max_turns = 0` | 关闭护栏 |
| `BudgetStop::message()` | `"Stopped after N turns in a single run (limit L). The task may be unfinished — send a follow-up to continue."` |

接线在 `agent_harness.rs::run_core`：这个 hook 原本是空着的
（`should_stop_after_turn: None`），现在挂上 `RunBudget`。等价的清空说明——**不新增
`ConfigSnapshot` / `AgentHarnessOptions` 字段**，先只给默认值，需要可配置时再加。

**为什么必须显式记录原因**：护栏触发时循环走的是正常出口，run 的 outcome 是
`Completed`。如果不记，用户看到的就是「任务做完了」，而实际上是**被截断**了。
所以 `run_core` 在 `persist_new_messages` 之后会往 lane 追加一条
`customType = "runBudget"` 的 custom 消息；live 与 session-reload 两条渲染路径
（`interactive_tui.rs:8188` / `:4202`）都走 `custom_message_fallback`，直接读 `content`，
所以两条路径都能看到这句话。

验证：
- `run_budget::tests` ×6（到顶恰好停、`0` 关闭、默认有限、`stop()` 保留原因、消息文案）
- **端到端** `harness_run_e2e.rs::run_budget_stops_a_looping_run_and_records_why`：
  用 faux provider 塞 **200 个只会调工具的 turn**，断言
  ① run 会终止而不是空转；② `provider.call_count == 120`（恰好停在顶，不是提前也不是跑过）；
  ③ lane 上**恰好一条**含 `Stopped after` + `unfinished` 的 custom 消息。

### 9.3 否决：无进展检测（原本是 §8.8 第 3 项的另一半）

设计前先拿真实会话量化，结论是不做：

| 候选信号 | 实测 | 判断 |
|---|---|---|
| 连续「无成功写入」的轮数 | 最长 **40 轮**（第 1 次成功写入发生在第 41 轮） | 看着像病，但那 40 轮是**正当探索**；阈值低到能抓住它就会截断正常任务 |
| 完全相同的 tool+args 重复 | 148 次调用里只有 **3 次** | 信号太弱，抓不到 |
| 相邻而非相同的重复 | `editor.rs` 被 9 次不同 `sed -n` 区间读 | 无法与正常探索区分 |

所以只保留「一个宽松的轮数上限」这一个既简单又安全的信号。

### 9.4 未做

| # | 项 | 状态 |
|---|---|---|
| 2 | `compat.requiresThinkingAsText` | 已应用为配置改动（不改代码）；A/B 因单样本 + 混淆变量无法作为行为证据 |
| 4 | per-delta 深拷贝 | **已完成**：provider 内 delta 合并（§10）+ 事件改共享快照去掉消费侧拷贝（§10.5），深拷贝次数约降 186× |

### 9.5 这一轮的可观测验收点

护栏给了之前缺失的**可观测指标**：`BudgetStop` 触发次数。
以后要验证 §九.1 的提示词改动是否真的减少了 re-plan，不必再跑不可比的长任务对比，
而可以看「同一任务下，120 轮上限被触发的次数从 N 降到 M」。

---

## 十、本轮动手：delta 合并（§8.8 第 4 项）

### 10.1 病因与手段的区分（先订正我自己）

§4.7 把 per-delta 深拷贝说成「没有回复但动画在转的完整解释」，**这个说法不成立**，此处订正：

- 那 6 分钟的表现是 **run 从未返回、没有产生任何 assistant 消息** ⇒ provider 请求本身没回来。
  O(n²) 只在**消息已经很长**的流式过程中显著，无法解释「一开始就没输出」。
- 深拷贝是真实存在的低效（有实测），降低的是长流式输出的成本上界，**不是那次卡住的病因**。

两件事都要修，但不是一个病因。

### 10.2 修了什么：共享的 delta 合并策略

新增共享策略（`crates/pi-ai/src/providers/mod.rs`）：

| 常量 | 值 | 作用 |
|---|---|---|
| `DELTA_FLUSH_BYTES` | 64 | 攒够 64 字节就发一个合并后的 delta |
| `DELTA_FLUSH_INTERVAL` | 40ms | 兜底：慢流最多憋 40ms 就必须发（否则纯字节阈值会让「每 100ms 一个字符」的流 6 秒不显示） |

落到**全部三个生产 provider**：

- `providers/openai_completions.rs` — `StreamState` 加 `pending_text`/`pending_thinking`/`last_flush`；
  `push_text`/`push_thinking` 只累加，`flush_text`/`flush_thinking` 才发事件。
- `providers/anthropic/mapper.rs` — 同构改造；`apply_block_delta` 原来在整个 match 上持有
  `&mut self.blocks[i]`，与 `flush_pending` 需要的 `&mut self` 冲突，改成只取两个 `Copy` 标量
  （`content_index`/`kind`），把 `partial_json` 的借用挪进 tool-call 分支内。
- `providers/openai_responses.rs` — `ResponsesStreamState` 加同样的字段（它原本 `derive(Default)`，
  但 `Instant` 没有 `Default`，改成手写 `impl Default`）；`append_text` 累加并在通道切换时
  先 flush 另一条通道；`finish_item` / `finish_open_slots` 是条目边界（后者是三条终态路径的
  共同必经点，所以三处终止都覆盖到了）。

`providers/faux.rs` 未改：它是测试替身，不在生产路径上。

**边界一定 flush**，所以合并只影响观感、不影响内容：
`content_block_stop` / `finish()` / `error()` / `content_block_start`，以及通道切换
（text → thinking、text|thinking → tool_call）。

### 10.3 实测

1 字节 delta（这些端点真实的分块粒度），`openai_completions`：

| deltas | 事件数（合并后） | 耗时 |
|---|---|---|
| 500 | 12 | 0.52 ms |
| 2000 | 36 | 2.03 ms |
| 8000 | **129**（原 8000） | **8.47 ms** |

- 事件数降 **62×**；`2000 → 8000`（4× delta）耗时 **4.0×** ⇒ **线性**（原来超线性）。
- 每个事件后面还有 3 份工作：provider 一次快照 clone、`agent_loop.rs:441` 再一次、
  TUI 一次 `update_blocks`。事件数降 62× ⇒ **这三处同时降 62×**，所以端到端收益大于 provider 单点数字。
- 10 字节的粗分块下事event 数只降 5×、仍超线性（1604 个事件 × 增长中的消息体仍然可观）。
  这是合并粒度的固有取舍：阈值越小越平滑、事件越多。

### 10.4 回归保护

- `openai_completions`：5 个测试 —— 事件数上界（确定性，不测墙钟）、短块尾必须在 `TextEnd` 前发出、
  通道切换顺序、thinking 同样合并且无丢字节、`error` 前 flush。
- `anthropic/mapper`：2 个测试 —— 500 个 1 字节 delta 合并且 `text_end` 前 flush 尾块；单字节块仍产生一个 delta。
- `openai_responses`：1 个测试 —— 同上，断言事件数上界、字节无损、`text_end` 前 flush。
- 时间路径用**老化 `last_flush`**（`Instant::now() - interval - 5ms`）来测，不 sleep，因此不 flaky。

### 10.5 再进一步：把消费侧的逐 delta 拷贝去掉（事件契约改动）

§10.2 之后每个事件仍有 **3 次整条消息深拷贝**：provider 建快照 1 次、loop 里建 event 1 次、
loop 再更新 `context.messages` 1 次（TUI 只读不拷）。又做了两件事：

**(1) 删掉 loop 里那次纯多余的拷贝。** `agent_loop.rs` 原来每个 delta 都做
`*context.messages.last_mut() = am.clone()`。查链路后确认**没有任何东西在 delta 之间读它**
（`transform_context`/`convert_to_llm` 都在循环之前跑完，`Start` 已经 push 了坑位，
终态分支会用最终消息覆盖）—— 这次拷贝没有任何可观察效果，直接删掉。

**(2) `AgentEvent::MessageUpdate.message` 改成 `Arc<AssistantMessage>`。** 原来是 owned
`AgentMessage`，所以 loop 必须深拷贝一次给事件，TUI 侧也只能拿 owned 值。改成共享快照后
loop 只做一次 `Arc::clone`（指针+1）。

**保持外部格式不变**是这次改动的关键约束：remote protocol 和扩展桥接都是把这个消息
**序列化**出去的，而 `AgentMessage` 是 `#[serde(tag = "kind")]`、`AssistantMessage` 不是。
所以新增 `pi_agent::message::assistant_json(&AssistantMessage) -> Value`，用一个同样
internally-tagged 的借用包装 enum 产出**逐字节等价**的 JSON，并由测试
`assistant_json_matches_the_agent_message_shape` 钉住等价性 —— 这样既不拷贝，也不动线格式。

| | 改前 | 改后 |
|---|---|---|
| provider 深拷贝 / 事件 | 1 | 1（固有：payload 必须是不可变视图） |
| loop 深拷贝 / 事件 | 2 | **0** |
| TUI 深拷贝 / 事件 | 0 | 0 |
| **合计** | **3** | **1** |

配回归测试 `message_update_forwards_the_provider_snapshot_without_copying`：断言
`Arc::ptr_eq(MessageUpdate.message, 该 delta 的 partial)` —— 指针相同**只可能**在没有发生任何
拷贝时成立，所以这条断言精确地钉住了「转发而非复制」。

合计效果（8000 个 1 字节 delta）：事件数 8000 → 129（62×），每事件拷贝 3 → 1，
即**深拷贝次数 24000 → 129，约 186×**，且 2000→8000 的耗时增长已回到 4.0×（线性）。

**仍然固有剩下的那 1 次**：provider 每发一个事件就得建一个不可变快照。要连它也去掉，
payload 必须变成共享可变状态（`Arc<Mutex<..>>`），代价是语义变化 —— 消费者会看到**最新**状态
而非**该事件时刻**的状态，并且渲染路径要持锁。对「按事件快照」的契约来说不值，故不做。

### 10.6 顺带修掉的一个既有红测试

`crates/pi-ai/src/providers/proxy.rs` 的文档示例本来就编译不过（`use rpi_ai::proxy::…`
路径错误 + 缺 `headers` 字段 + 悬空的 `model`/`ctx`/`opts`），让 `cargo test --workspace`
常年为红。已改为 `no_run` 且补全可见字段与正确的
`rpi_ai::providers::proxy` 路径。该文件其余 11 处 rustfmt 差异是既有债，未动。

---

## 十一、run 中途落盘 / 崩溃可恢复性（§4.6 详细展开）

§4.6 当时只写了「run 结束才落盘，中途崩溃会丢」，并把它归为「设计缺陷」。
展开查证后发现真正的差距比那句话大得多 —— 不是「少了几次 flush」，而是
**rpi 缺少原生 pi 那套逐帧持久化的 run 状态机**。下面是完整证据链。

> **§11.1–§11.6 描述的是修复前的状态**（问题诊断），§11.7–§11.8 是取舍与实现记录。
> 帧级进度已在 §11.7.1 落地；`StepAttempt` / `ToolStarted` / `WriteDeferred` 的写侧仍是缺口。

### 11.1 症状

用户在长任务中途 kill 掉进程（或被 OOM / 终端断连打断）后：

- 该次 run 产生的 assistant 消息、工具结果**全部消失**；
- 会话里只剩一条**悬空的用户 prompt**；
- 重新进入后模型看不到自己刚做过什么，于是**从头再来**；
- 而且**无法恢复** —— 不是「等一下会补上」，是产物从未落盘。

### 11.2 现在到底在什么时刻落盘（逐时间线）

`crates/pi-harness/src/agent_harness.rs::run_core`（`:1799` 起）：

```
:1799  run_core(prompts)
  :1805    从 next_run_queue 取出排队 prompt
  :1812    finish_interrupted_operation(..)      ← 只做一件事：把上一次没写完的
                                                   operation 记为 aborted/interrupted
  :1824    bus.emit(RunStart)
  :1830    intent = OperationIntent::Run { original_prompt, initial_messages: [] , resume_data: None }
  :1836    write_operation_started(run_id, source_leaf, intent)     ★ 落盘 #1（记录）
  :1840    for msg in &prompts { append_message(msg) }              ★ 落盘 #2（用户输入）
  :1883    branch_path_oldest_first()            ← 上下文来源（此时已含 prompt）
  :1886    预压缩判断 → 可能 compact(..)
  :1964    压缩后重建 path
  :1974    agent_context = build_session_context(&path)             ← ★ 快照一次，之后不再变
  :2119    let result = loop {                                       ┐
  :2120        run_agent_loop(Vec::new(), agent_context.clone(), ..) │ 整个 run 在这里
               …112 轮 / 867 秒（实测最长）都在这个循环里…            │ 内存中累积
           }                                                        ┘
  :2174    match result {
  :2179        Ok(new_messages) => persist_new_messages(&new_messages[cut..])  ★ 落盘 #3（全部产物）
               Err(_)          => 什么都不落盘
           }
  :2256    write_operation_finished(run_id, outcome, error)          ★ 落盘 #4
```

**关键事实**：`persist_new_messages`（`:1698`）只在 `run_core` 的**最后**被调用一次
（`:2179`）。它前面那次 `append_message` 是 prompt，不是模型产物。

所以 run 的**全部产物**（assistant 消息 + 工具结果）在整个 run 期间**只存在于内存**。
时间窗口 = 整个 run 的时长，实测最长 **867.7 秒 / 112 轮**。

### 11.3 真实会话证据

`.pi/sessions/--D--Projects-pi-rust--/2026-09-24T16-27-30-919Z_01a0d43e-*.jsonl` 全文只有 4 条：

```
header
record operation_started   seq=1    ts=1790267255099   intent=Run{prompt:"读取下当前项目"}
entry  seq=2 user          "读取下当前项目"
record operation_finished  seq=3    ts=1790267607684   outcome=aborted
                            error={code:"interrupted", message:"Operation was interrupted before it could finish"}
record operation_started   seq=4    ts=1790267613661   intent=Run{prompt:"读取下当前项目"}   ← 同样的 prompt，第二次
```

三个观察：

1. **run 1 持续 352 秒，落盘的只有 1 条 user entry** —— 那 352 秒里模型读了多少文件、想了什么，一个字都没留下。
2. 那条 `operation_finished{outcome:aborted}` 的**时间戳与 run 2 的 `operation_started` 几乎相同**
   （…607684 vs …613661，差 6 秒），因为它是 **run 2 启动时**由 `finish_interrupted_operation`
   （`:1364-1396`）补写的 —— 不是 run 1 自己写的。run 1 从未写出自己的结束记录。
3. run 2 的 prompt 与 run 1 **完全相同** —— 用户重发了一遍，因为第一次什么都没产出。

同一模式在 `01a0d44a` 重现：`operation_started` + user "hi"，之后什么都没有。

### 11.4 为什么这不是「顺手加个 flush」

三个真实约束，任何一个单独看都不难，叠起来才让它变成设计题：

**(a) retry 语义与「逐段落盘」直接冲突。**
`:2119-2165` 的重试循环用**同一份** `agent_context.clone()`（`:2120`）重跑
`run_agent_loop`，而 `run_core` 只采用**最后一次尝试**的 `new_messages`。
也就是说：**失败尝试的产物是被有意丢弃的**。
如果改成「每轮落盘」，失败尝试的消息也会进会话，于是

- 会话里出现模型从未继续过的历史；
- 重试成功后再落一次 → **重复条目**；
- 而那个失败尝试还是个 `stop_reason: Error` 的 assistant 消息，放回上下文对 provider 是非法输入。

**(b) `persist_new_messages` 的切片依赖 run 结束时的状态。**
`:2179` 传的是 `&new_messages[cut.min(len)..]`，`cut` 来自压缩钩子的
`post_compaction_cut`（`:1987` 建、`:2025` 传进钩子）。逐段落盘必须让这个 cut 在增量语义下
仍然一致 —— 否则压缩前的那一段会被重复写入。

**(c) 现在的中止路径恰恰依赖「run 结束才统一落盘」。**
Esc / Ctrl+C 中止走的是 `run_loop` 的
`if matches!(stop_reason, Error | Aborted) { … return Ok(LoopOutcome::Aborted) }`
（`pi-agent/src/agent_loop.rs:227-244`）—— **返回 `Ok`**，所以 `run_core` 走的是
`Ok(new_messages)` 分支，**中止的 run 目前是会完整落盘的**。
改成增量落盘时不能把这个行为改坏（否则中止反而丢历史，比现在更糟）。

⇒ 所以这不是「加一行 `flush()`」，而是要重新定义**哪些产物算已提交**。

### 11.5 与原生 pi 的差距（这才是问题本体）

原生 pi 把「run 进行中的进度」本身也做成持久化对象。证据：

**帧级持久化**

- `packages/agent/src/harness/runtime/progress.ts:65` `openFrameProgress(lane, drive, responseEntryId)`
  → `openProgress(..., commitWrite = (frame) => appendList(address, frame), ...)`
  → 每写一个帧就 `lane.command({ kind: "commit", writes: [appendList(...)] })`
  —— **每帧一次持久化提交**，不是 run 末尾批量。
- 帧类型是一套流式事件编码：`packages/agent/src/harness/pico3/kinds/frames.ts:12` 的
  `applyFrame` 按 `start / text_start / text_delta / text_end / thinking_* / tool_*` 累加，
  与 `reduceAssistantMessageFrames` 同一套语义（该文件的注释明确说它**就是**那个 oracle）。
- 读回：`runtime/progress.ts:17` `readAssistantFrames(reader, operationId, responseEntryId, context)` 分页读回帧列表。
- 提交成功后帧列表被删除：`runtime/drive/terminal.ts:47` `deleteList(pendingAssistantFrames(...))`
  —— 即帧列表是**临时进度存储**，最终消息落盘后清理。

**崩溃恢复**（`runtime/drive/recovery.ts`）

- `:36` `recoverAssistantGeneration(...)`：读已提交的**帧前缀**，
- `:53/:61` `readAssistantFrames` + `reduceAssistantMessageFrames` 重建部分消息，
- `:19-33` `interruptedAssistantMessage(...)`：把它包成
  `stopReason: "error"`、并附一句
  *"The preceding content is the latest committed partial; newer live output may be missing and the external outcome is unknown."*
- `:70` 以 `message_start { recovery: true }` 发出 —— 于是**崩溃前已提交的内容被保留下来**，
  并明确标注为「中断的部分产物」，而不是凭空消失。

**可恢复的运行状态机**

`runtime/lane.ts:1769` 起，操作状态包含
`assistant.retry_wait { attempt, maxAttempts, nextAttemptAt }`、
`assistant.effect_pending`、`deferred.suspended` / `deferred.effect_pending`。
也就是说**重试等待和 deferred 挂起都是可跨进程恢复的状态**，不是内存里的循环。

**对照 rpi**

| 能力 | 原生 pi | rpi |
|---|---|---|
| run 进行中把产物持久化 | 每帧 `appendList` 提交 | **不写**；只在 `:2179` 批量写 |
| 崩溃后保留已产出的内容 | 读帧前缀重建，标为 interrupted | **全丢**，只剩 prompt |
| 重试等待可跨重启 | `assistant.retry_wait` 状态 | 内存循环（`agent_loop` 内的 `loop`） |
| 恢复动作 | 重建 + 标 interrupted（`recovery: true`） | 只有 **Abort** |
| deferred 挂起 | `deferred.suspended` 可恢复 | `WriteDeferred` 有类型，run 路径不写 |

**rpi 有一半脚手架，但没接上**（这点值得单独记）：

- 记录类型已定义：`session/types.rs:911-919` 有
  `OperationStarted / AbortRequested / OperationFinished / StepAttempt / ToolStarted / QueueEnqueued / QueueCancelled / WriteDeferred / Usage`。
- reducer 与 runtime **能读**它们：`runtime.rs:250/267` 处理 `WriteDeferred` / `ToolStarted`；
  `session/memory.rs:400-418` 有对应的 seq/timestamp 打戳分支。
- 恢复侧甚至能**识别**工具帧悬空：`RecoveryFinding::DanglingToolFrame { run_id, tool_call_id }`（`runtime.rs:90`）。
- **但 `run_core` 从不写 `StepAttempt` / `ToolStarted` / `WriteDeferred`**
  —— 全仓 grep 只有定义与读取，没有任何生产路径的 append。
- 恢复动作集合里**没有 Resume**：`RecoveryDecision` 只有
  `Abort / FlagCorruption / DropDangling`（`runtime.rs:142-149`）。
- CLI 启动时的扫描是**只读**的，只报告并补发事件，不做修复：
  `pi-cli/src/session.rs:818-855`（注释原文 *"This is read-only: nothing is mutated here"*）。
  真正的 `operation_finished` 由**下一次 run 启动时**的 `finish_interrupted_operation` 补写。

⇒ 所以 §7「durable operation runtime 已实现」这个自评对**记录/恢复的读侧**成立，
但**run 路径的写侧是空的**。这是本次查证最主要的修正。

### 11.6 影响面（不只是「崩溃丢数据」）

因为几乎所有派生视图都读持久化会话，run 进行中它们**都看不到当前进度**：

| 受影响 | 原因 |
|---|---|
| `/tree` | `open_tree_selector` 走 `harness.session().view(..).find_entries(..)` → 只有已落盘的 |
| `/session`、`/export`、`/usage`、`/context` | 同上，统计与导出都基于已落盘条目 |
| 崩溃后继续 | 无法 resume，只能重来 |
| 崩溃后的会话状态 | 留下一个**未关闭的 operation**；直到下次在该 lane 上跑 run 才被补记为 interrupted |
| cache-miss 重算、会话全文检索 | 都基于已落盘条目 |

### 11.7 修法选项与取舍

**A. 逐轮落盘 + 重试改为从「落盘后的 tip」重建上下文**（原生方向）
- 每轮结束就把该轮的 `new_messages` 写入 lane；
- 重试时不再复用 `:2120` 那份 pre-run 快照，而是从 lane 的当前 tip 重建 `agent_context`。
- 难点：失败尝试是 `stop_reason: Error` 的 assistant 消息，放回上下文对 provider 非法
  → 需要在重建时**跳过/替换**它（原生用 `interruptedAssistantMessage` 正是干这个：
  把部分产物标成 error 并附说明）。
- 代价：语义变化（重试从「重做该轮」变成「带着上一轮的部分产物继续」）。
- 风险：中高（动 run/重试主路径）。

**B. 逐轮落盘 + 失败时回滚 lane 到之前的 leaf**（保留现有重试语义）
- 需要「截断分支」能力：把 leaf 移回 run 起点，让失败尝试的条目变成不可达。
- 会话日志是 append-only JSONL，回滚意味着留下垃圾条目（或需要新的 tombstone 语义）。
- 代价：引入一套新的持久化机制与不变量。
- 风险：中高（动持久化不变量）。

**C. 先只做「崩溃损失有上界」，不动重试语义**（最小步）
- 在 run 内按轮/flush 阈值把 `new_messages` 的**已提交前缀**写盘，并在
  abort / shutdown / 进程信号路径上 flush；
- 对重试：只在**不处于重试等待**时 flush，或把已 flush 的部分标记为「未提交」以便重试成功后
  由恢复逻辑忽略 —— 两种都带特例。
- 收益：把「崩一次丢 867 秒」变成「丢最多 N 轮」。不是原生那种帧级精度，但代价小得多。
- 风险：中（不动重试主路径，但要保证幂等）。

**D. 对齐原生：帧级进度 + 可恢复状态机**
- 引入 `AssistantMessageFrame` 列表（落盘）、`StepAttempt`/`ToolStarted`/`WriteDeferred` 的实际写入、
  以及 `RecoveryDecision::Resume`；
最大，但只有它能让「崩溃后保留部分产物」真正成立。
- 建议：**分两步** —— 先落 D 里最便宜的部分（帧列表 + 崩溃时标 interrupted），
  再补 retry/deferred 的状态机恢复。

**推荐**：先做 **C**（有界损失，不动语义，可独立验证），再评估 D。
A/B 都会改重试语义或持久化不变量，值得单独一轮评审。

### 11.7.1 实际实现：走了 D 的第一阶段（已完成）

最终没有走 C，而是直接做了 **D 里最便宜、也最关键的那一半**：帧级进度落盘 +
崩溃时按已提交前缀重建。理由是 C 的「按轮 flush」在 rpi 里并不比帧级更便宜 ——
反正都要往会话里写东西，写**一帧**和写**一整轮**的机制完全一样；而写帧顺带把
「重试时已提交产物」的问题一起解决了（失败尝试的帧会被 `ClearRun` 退掉，见下）。

**存储位置：记录流，不是 list，也不是 entry。**
这是实现中唯一一处**推翻原设计**的地方，值得记下来：

- 原生用 session 级 **list** 存储（`pendingAssistantFrames(operationId, responseEntryId)`），
  与分支彻底隔离；rpi 没有 list 原语。
- 第一版按 `values.rs` 的老办法把帧写成 **custom entry**。结果 `harness_run_e2e` 立刻红：
  `run_with_no_tool_calls_completes_in_single_turn` 断言「user + assistant reply only = 2 条」，
  实际 7 条 —— 每帧一条 entry，全进了分支路径。
- 结论：**帧是进度，不是历史**。它必须走 `LaneRecord`（记录流），因为记录不进分支路径，
  模型上下文永远看不到它。这跟 §11.4 的判断是同一条：问题在写侧，而写侧的
  「写哪儿」决定了它会不会污染上下文。

**写侧**（`crates/pi-harness/src/frame_progress.rs`）：

- `FrameRecordingEmitter` 包住原来的 emitter，在 `MessageStart` / `MessageUpdate` 时把
  帧追加成 `LaneRecord::AssistantFrame { run_id, stream_index, op: Append, frame }`；
  `MessageEnd` 收束当前流。每个 assistant 消息在 run 内拿一个 `stream_index`。
- `assistant_frame` 已加进 `jsonl/codec.rs` 的 `RECORD_TYPES` 白名单（否则重新载入会话会
  被当作未知记录类型拒绝）。
- run 的 `new_messages` 成功落盘之后写一条 `op: ClearRun` 记录把整轮的帧退掉：
  它们只是进度。放在「落盘之后」而不是每个 `MessageEnd`，是为了让**失败重试尝试**的帧
  不会被当成真历史捞回来。
- 落盘失败只记 warn，不致命 —— 写不进进度的会话仍然要能跑 agent。

**读侧**：

- `salvage_run_frames(session, run_id)`：按 `record_type + run_id` 取记录，按 `stream_index`
  分组，遇到 `ClearRun` 直接返回空（该 run 已提交，无需打捞），否则用
  `reduce_frames` 把每路帧归约成部分消息。
- `interrupted_message`：`stop_reason: error` + 原生原文说明
  （*"Assistant request was interrupted. The preceding content is the latest committed partial;
  newer live output may be missing and the external outcome is unknown."*），
  并把 **usage 清零** —— 与原生一致，避免打捞出来的部分产物和之后的重试重复计费。

**两个恢复入口**：

1. `finish_interrupted_operation`（已有）：发现未关闭的 operation 后结清它，并打捞该 run 的帧、
   以 interrupted assistant 消息形式追加到分支。这是**用户重开会话**时走的路径。
2. `run_core` 的 `Err` 分支：该路径本来什么都不落盘，现在也打捞一次。

**验证**（`crates/pi-harness/tests/harness_run_e2e.rs` 新增 3 条）：

- `a_crashed_run_can_be_salvaged_from_its_committed_frames`：慢速流跑到一半 `abort`
  任务（等价于 kill -9 对 run future 的影响），断言帧已落盘、分支里**只有 prompt**、
  打捞出来的正文是完整回复的**真前缀**且 usage 为零。
- `reopening_a_crashed_session_restores_the_committed_prefix`：**端到端走真实恢复入口** ——
  崩溃后重新 `AgentHarness::create` 同一会话，断言未关闭的 operation 被结清、
  分支里 prompt + interrupted assistant 两条、正文仍是前缀。
- `a_completed_run_leaves_no_frames_to_salvage`：正常完成的 run 必须已被 `ClearRun` 退掉，
  恢复时打捞不到任何东西。

**尚未做**（D 的剩下部分，仍是 §11.5 的差距）：`StepAttempt` / `ToolStarted` /
`WriteDeferred` 的**写侧**依然不写（只有类型定义和读侧），
`RecoveryDecision::Resume` 那套「崩在工具执行中间」的状态机恢复也没有。
也就是说：现在能恢复**模型已经产出的内容**，还不能恢复**崩在工具调用中间的执行状态**。

### 11.7.2 打捞出来的残片必须是合法上下文（§11.7.1 的后续修正）

§11.7.1 上线后复查发现它自己引入了一个**真 bug**，而且触发条件是**最常见的那种崩溃**：

崩溃十有八九发生在**工具执行期间**（长任务的时间几乎都花在工具里）。此时已落盘的帧里
含有一个完整的 assistant 消息**带 tool call**，而对应的 tool result 永远不会落盘。
把这样的 assistant 消息原样追加回分支，就等于把一个
**assistant(tool_calls=[X]) 后面没有 tool_result(X)** 的历史交给 provider ——
Anthropic 会直接拒绝（*"`tool_use` ids were found without `tool_result` blocks"*），
OpenAI 同理。而 `default_convert_to_llm`（`pi-agent/src/hooks.rs`）**不做任何孤儿工具调用修复**，
只是过滤 custom 消息。

也就是说：修「崩溃丢数据」的那个补丁，会让崩溃后的**下一轮请求直接失败**。

**修法**：`frame_progress::salvage_run_messages` 返回的不是 assistant 消息列表，而是
**可直接持久化的消息序列** —— 每个打捞出来的部分 assistant 消息后面，紧跟它所含
**没落盘**的每个 tool call 的**合成 error tool result**：

- `is_error: true`；
- 正文为 `INTERRUPTED_TOOL_RESULT`：
  *"Tool execution was interrupted before its result was recorded. The external outcome is unknown:
  this tool may or may not have taken effect."*；
- `tool_call_id` 与被打断的调用一致。

这一对不是装饰：它同时解决了三件事 ——

1. **上下文合法**：每个 tool call 都有配对 result，下一轮请求能建起来；
2. **模型知道真相**：不是「工具没执行」，而是「结果未知」；
3. **用户看得见**：TUI 恢复会话时把 tool result 渲染成工具面板
   （`interactive_tui.rs` 的 `AgentMessage::ToolResult` 分支 + `result.is_error`），
   所以这条 error 面板会**自动**出现在恢复后的转录里，不需要任何 TUI 改动。

判定「哪个 tool call 没落盘」用的是 **`tool_call_id`**，不是预留的 `result_entry_id`：
因为 rpi 是在 run 结束时才生成 id 落盘，工具真正跑的时候那个 id 还不存在。
`tool_call_id` 才是实际存在的持久链接。

回归测试 `crashing_during_a_tool_call_leaves_a_valid_transcript`
（`harness_run_e2e.rs`）：注册一个永不返回的工具，跑起来后等 `toolcall_end` 帧落盘，
再 `abort`（等价于崩在工具执行中间），然后重开会话，断言：

- 打捞出的 assistant 消息仍然**带着**那个被打断的 tool call；
- 分支路径上（`find_entries_on_branch`，不是全部 entry）存在与之配对的 error tool result，
  且正文里含 "unknown"。

### 11.7.3 记录流上的「悬空」判定修正

`runtime.rs::recover_lane` 原来把「悬空」定义为 **record 的 `run_id` 不在 open 集合里**。
这个定义是错的：**正常完成的 run 也没有 open operation**。
所以只要 write 侧真的开始写 `write_deferred`/`tool_started`，
每个成功 run 的每条已结清帧都会被报成 dangling，
`LaneRecovery::is_clean()` 永远为 false，真正的信号（进程死掉导致没落盘的那条）被埋在噪音里。

**修正后的定义是「结果没落盘」**：

- `write_deferred` 已结清 ⇔ `target.id` 对应的 entry 存在；
- `tool_started` 已结清 ⇔ 分支上存在 `tool_call_id` 相同的 tool result；
- 已结清的记录**根本不算 pending**，也就不会进 `pending_writes` / `tool_frames`；
- 只有「没落盘 **且** run 已关闭」才产生 finding（这才是崩溃残留）；
- 「没落盘但 run 还开着」= 正常的执行中状态，**不产生 finding**
  （只有已经死掉的 run 才值得报）。

新增 4 条测试：已结清的 write / 已结清的 tool frame 在 run 关闭后**不得**被报；
没落盘的 tool frame 要报出来**且带工具名**；执行中的 tool frame 不算 finding。
已有测试 `dangling_write_is_flagged_and_dropped` 继续通过（它的 entry 确实不存在）。

### 11.7.4 写侧：仍然做不到，以及为什么

原计划是「补齐 `StepAttempt` / `ToolStarted` / `WriteDeferred` 的写侧」。
读完 reducer 的不变量后可以确定：**在当前持久化模型下 `ToolStarted` 写不出来**。

证据链：

1. `reducer.rs::validate_tool_start` 要求 `ToolStartedRecord.assistant_entry_id`
   **必须已经存在于 entries 里**，而且该 assistant entry 的第 `tool_index` 个 tool call
   的 `id`/`name` 必须与 record 一致（否则 `ToolCallMismatch` / 记录日志被判损坏）。
2. rpi 的 run 路径**只在 run 结束时**才落盘消息（`persist_new_messages`，run_core 的 `Ok` 分支），
   工具执行发生在循环内部、**远早于**落盘。
3. 所以在 `ToolExecutionStart` 那一刻，被引用的 assistant entry **还不存在** →
   任何 `ToolStarted` 写入都会被 reducer 判成损坏。

原生能做到是因为它的顺序相反：assistant 消息先 provision + 落盘（`assistant.ready`），
**再**跑工具（`tools`），所以 `assistantEntryId` 天然存在。

`StepAttempt` 和 `WriteDeferred` 在 reducer 层面**允许**目标 entry 缺席
（`validate_result_entry` 对缺席放行），所以二者理论上可以先写；
但要写得**有意义**（record 里那个 id 能在恢复时被核对）就必须先把
「先预留 entry id、内容后提交」的 provision 机制接进消息持久化路径 ——
那正是原生 `write_deferred` 的模型，也是对 run 持久化路径的实质改动。

结论：写侧不是「忘了调用」，而是**缺一个 provision 阶段**。
在补上那个阶段之前，宁可**不写**这些记录 —— 写一条 `result_entry_id`
永远对不上的 `StepAttempt` 只会制造「看起来有恢复信息、实际核不了」的假象。

当前实际交付的是**用已有的帧把同一个目标做成**：崩溃后能明确告诉用户
**哪个工具的副作用未知**（合成 error tool result，见 §11.7.2），
不需要新增任何写侧记录。

### 11.7.5 provision 阶段：第一步已落地（compaction）

「开动」之后做的第一步，是把 provision 机制本身建起来，并接上**第一个真实写入方**：
**compaction** —— 它是唯一一个内容在 provision 时就已经完全确定的 entry，
所以 `write_deferred` 要求的「精确 target」天然能满足。

新增两个写侧辅助（`agent_harness.rs`）：

- `write_write_deferred(run_id, &ProvisionedEntry)`：在 append **之前**写
  `write_deferred`。target 用 `provisioned_into_entry(...)` 造一个占位 entry，
  再经 reducer 自己的 `entry_provisioned_json` 取 provisioned JSON ——
  也就是 reducer 用来做深度比较的**同一个函数**，所以「记录下的意图」和
  「最终提交的 entry」不可能漂移（该函数因此改为 `pub(crate)`）。
- `write_step_attempt(run_id, step, attempt, result_entry_id, reason)`：在 append
  **之后**写 `step_attempt`，让 `result_entry_id` 指向真正落盘的那个 entry。

`persist_compaction_entry` 现在接收 `run_id` + `CompactionReason`，按
`write_deferred` → `append_entry` → `step_attempt` 的顺序写。两个调用点的 reason 分别为：

- run 路径的预检 / 轮后钩子 → `Threshold`（跨过配置阈值，主动触发，不是响应 provider overflow）；
- `compact_core`（`/compact`、compaction 命令）→ `Manual`。

（`Overflow` 在 rpi 里没有对应的响应式路径，因此未使用。）

**验证**：`runtime.rs` 新增 `compaction_intent_records_validate_against_the_reducer` ——
用手工构造的 compaction provisioned entry 写出这两个记录，然后调
`validate_record_log`（reducer）断言日志**合法**（包括 target 与提交 entry 深度相等、
`step_attempt` 序列与结果一致），并断言 `recover_lane` 把这条 write 看作**已结清**。
写一条 validator 会拒绝的记录比不写更坏 —— 损坏的日志会直接堵死恢复，所以这个断言是必须的。

**这一步没有动 run 主路径的任何行为**：只是多了两条记录，现有测试全部继续通过。

### 11.7.6 下一步：assistant 步与 `ToolStarted`，及其真实风险

要把 assistant 步做像，就绕不开一个结论：**必须把「消息落盘时刻」从 run 结束提到
assistant 步定稿时刻**。原因：

- `step_attempt{Assistant}` 要指一个 `result_entry_id`。如果这个 id 在最终落盘时被换掉，
  记录就只是一个永远对不上的空引用（reducer 允许缺席，但恢复时核不了）；
- `tool_started` 更硬：它要求 `assistant_entry_id` **已经存在**。

而 rpi 现在的 retry 循环在 **harness 层**（`run_core` 里重新调用整个 `run_agent_loop`），
所以「一次 attempt」产生的是**多条** assistant 消息，与「一个 assistant 步」不对应。
正确的粒度是**每条 assistant 消息定稿时（`MessageEnd`）就提交**，此时：

- 每条 assistant 消息可以持有自己预留的 entry id → `step_attempt{Assistant}` 有意义；
- 工具执行前 assistant entry 已存在 → `tool_started` 写得出来；
- 重试语义仍可保持：`MessageEnd` 时 harness 就能算出「这条会不会被重试」
  （`retry_policy.enabled && retry_attempt < max && is_retryable(stop_reason == Error)`），
  会重试的 attempt **不提交**。但这是把重试判定逻辑搬到了两个地方，有分叉风险。

**为什么单独列出**：这一步会改 run 持久化的主干 ——
`persist_new_messages`、`leaf_id`/`final_entry_id` 的推导、
以及 `post_compaction_cut` 那套「按 cut 索引切片后再落盘」的假设（compaction 就是为了
**不落盘**被摘要掉的那段前缀，改成增量落盘后这套假设必须重建）。
它属于「改持久化模型」，不属于「补几个写入调用」，应该单独评审 ——
因为这正是之前被你划在范围之外的那类改动。

### 11.7.7 已落地：commit-on-settle（把落盘时刻提前到消息定稿）

§11.7.6 说的那一步已经做了。新增 `crates/pi-harness/src/settle.rs`：

- `SettlingEmitter` 包在帧记录器**外面**（所以每个 delta 的帧仍然先落盘，
  「已定稿但未提交」这个窗口仍然可用帧恢复）；
- `MessageEnd{Assistant}` 且该消息**带 tool call**（即后面会跑去执行工具）、
  且这个 attempt **不会被重试** → 立即用**预留的 entry id** 把这条消息落盘，
  并写一条 `step_attempt{step: assistant, attempt, result_entry_id}`；
- 这条 entry 一旦存在，`tool_started` 就写得出来了：
  `ToolExecutionStart` 时写 record（带 `effective_args`、`tool_index`、`replay`），
  并**预留** result entry id；等 tool result 定稿时再用那个 id 提交。

**为什么不提前提交两类消息**（这是刻意的，不是遗漏）：

- **最终 assistant 消息**（无 tool call）：它结束整个 run，仍由 run 末尾的
  ordinary 路径落盘，所以 `leaf_id`/`final_entry_id` 与 `Completed` outcome 语义不变。
- **将要被重试的 attempt**：retry 是 harness 层重新跑整个 `run_agent_loop`，
  提交失败 attempt 的消息会把一条死掉的 assistant 消息留在历史里，
  并在 provider 面前弄出**连续两条 assistant**。判定条件在定稿时就可算出
  （`enabled && attempt < max && is_retryable_error`），与 retry 循环用同一套输入 ⇒
  `settle::will_retry`。

**run 末尾的 dedup**：`persist_new_messages` 增加 `start_index` + `committed`
两个参数，跳过 settled 已经提交过的下标，否则会在分支里重复一条。

#### 带来的行为变化（唯一一处）

已经被 settle 提交的消息无法再被 `post_compaction_cut` 跳过，
所以**被中等压缩摘要掉的那段消息现在会留在日志里**。这与原生一致
（原生压缩只追加一条 summary entry，旧 entry 仍在，上下文构建靠 cut-point 扫描
从最后一次 compaction 之后开始），但确实与 rpi 之前的「干脆不落盘」不同 ——
这是有意取舍。

#### 验证

- `a_real_run_leaves_a_valid_record_log`：真实跑一次带工具调用的 run，
  然后拿**整份 record log** 过 `validate_record_log`（reducer 本体）——
  包括 `step_attempt` 序列/结果一致性、每个 `step_attempt.result_entry_id`
  确实对应一条已落盘的 assistant 消息。写 validator 会拒绝的记录比不写更坏，
  所以这是强制项。
- `a_retried_attempt_is_not_committed`：脚本一次可重试失败 + 一次成功，
  断言历史里**恰好一条** assistant 消息，且是重试后那条。
- `crashing_during_a_tool_call_leaves_a_valid_transcript` 扩充：崩溃在工具执行中时，
  断言 `tool_started` record 已存在并带着 `effective_args`，
  且 `recover_lane` 把它当作**执行中**（不是 finding）——
  并显式断言此时产生的是 `OrphanedOperation`：
  **只有已经死掉的 run 才值得报**。工具真正变成 `DanglingToolFrame`
  发生在 operation 被关闭而结果未落盘时，由
  `an_unlanded_tool_frame_names_the_tool` 覆盖。

这条把写侧与读侧接上了：`tool_started` 现在真的会被写出来，
而 `runtime::recover_lane` 的 `tool_frames`/`DanglingToolFrame` 不再是死代码。

**仍未做**：`RecoveryDecision::Resume`（需要原生那套 durable `OperationState`
状态机 / `settleOperation`，属于架构重写）与 `WriteDeferred` 在消息路径上的使用
（目前只用于 compaction；消息走的是 `step_attempt` + 预留 id，而非 `write_deferred`）。

> **§11.7.8 修正了这段结论里的一处措辞错误**，请以 §11.7.8 为准。

### 11.7.8 `ToolReplay`：修正反转的默认值，并让它真的被消费

复查时发现两个问题，都是真 bug：

**（1）默认值是反的。** 原生：

```ts
// packages/agent/src/types.ts
tool.replay ?? "never"            // 默认 never
// harness/pico3/types.ts
readonly replay?: "safe" | "unsafe"; // default unsafe
```

rpi：

```rust
pub enum ToolReplay {
    Never,
    #[default]
    Safe,        // ← 反了
}
```

因为 `HarnessTool::new` 用的是 `ToolReplay::default()`，所以**所有没显式声明的工具都是
`Safe`**；而 `pi-cli` 又用 `.map(|(_, t)| t.with_replay(ToolReplay::Safe))` 把**每一个**
内置工具（含 `bash`/`edit`/`write`）都标成了 `Safe`。
今天无害（没人读它），但一旦恢复开始按它重跑，副作用会被重复施加。

**修法**：`#[default]` 移到 `Never`；`pi-cli` 改成显式策略——只有 `read` 与 `docs`
这两个真正无副作用的工具是 `Safe`，其余保持 `Never`（扩展工具也默认 `Never`，
因为扩展工具的 `read` 不一定只读）。新增测试 `only_read_only_tools_are_replayable` 钉住这个名单。

**（2）没有任何决策方消费它。** 全仓搜 `.replay` 只有「设置」和「映射进记录」。

**实现（原生 `drive/tools.ts::recoverToolInvocation` 的移植）**：

```ts
if (!cancelled && call.replay === "safe" && tool?.replay === "safe") {
    const args = await clearReplayCheckpoint(...);   // 取回原始 args
    return performToolInvocation(..., cleared, ..., true);
}
const checkpoint = await readCheckpoint(...);
return publishToolOutcome(..., interruptedOutcome(toolCall, checkpoint), true);
```

rpi 侧新增 `AgentHarness::resolve_interrupted_run` / `resolve_tool_call`：
每个未落盘的 tool call ——

- **双重确认**同时成立（`tool_started.replay == Safe` **且**当前工具声明 == Safe）
  → 用**记录里的 `effective_args`**（内存里那份已随进程死亡）重新执行，真实结果落盘，
  并在结果末尾附上 `[recovered]` 标记（原生也是在内容**之后**追加标记）；
- 否则 → 保持原行为，写合成 error result（"outcome is unknown"）。

为了做这个，`ToolFrame` 增加了两个字段（`effective_args` / `replay`）——
recovery 需要的这两样只在 `tool_started` 记录里，帧里没有。

**双重确认为何必须两边都要**（新增测试覆盖的方向）：

- 只看记录 → 会把调用重跑进一个**已经被改成不安全**的工具；
- 只看声明 → 会把声明之前发出的调用当成 safe 重跑。

`a_recorded_safe_call_is_not_replayed_into_a_now_unsafe_tool` 钉住了第一种：
记录是 `Safe`、恢复时工具是 `Never` ⇒ 工具调用次数必须仍是 **1**。

**这一步的收益**：以前崩在工具执行中，**每个**工具都变"结果未知"，模型丢掉真实输入，
用户要重新提。现在只读工具直接重跑，真实结果落盘，对话保持可用；
只有**真正不确定的副作用**才标 "unknown"。

**这一步没做到什么**（重要）：**run 仍然不会继续跑**。
rpi 的 run 是直线函数，状态在栈帧里，崩溃后没有可重入的落点
（原生 `driveOperation` 从 `state.at` 重入）。变的是「留下的转录完整且正确」，
不是「带着状态继续执行」。要后者仍需 §11.7.6 / §11.7.4 说的可重入状态机。

### 11.7.9 三处需要更正的说法

复查时把整个 `.reference/pi` 搜了一遗，必须更正之前文档里不准的表述：

1. **`step_attempt` / `write_deferred` / `danglingWrite` 在原生里根本不存在。**
   它们不是原生特性，是 rpi 自己那套「持久化意图记录」的设计。有真实用途
   （让 `recover_lane` 能指名哪个工具结果未知），但不是 parity 缺口。
2. **原生没有 `RecoveryDecision` 这个类型，也没有 `Resume` 变体。**
   它的恢复不是「决策」，是可重入状态机（`driveOperation` + `OperationState`）。
   所以「加一个 `RecoveryDecision::Resume`」这个说法本身就是误导。
3. **「消息路径应该用 `write_deferred`」是错的。** `write_deferred` 的语义是
   「内容已确定、只差提交」，reducer 会用 `validate_exact_provisioned_entry` 把最终 entry
   与 target **深度比对**；assistant 消息的内容在流式结束前根本不存在，硬填只会判损坏。
   原生对应的做法就是**纯 id 预留**（状态叶里的 `responseEntryId`）——
   也就是 rpi 现在用 `step_attempt` + 预留 id 做的事。**这条不是欠账，反而已经比原方案更接近原生。**


### 11.7.10 启动时自动续跑（对齐原生的 resume）

R1 把转录修对了，但 run 仍然结束。这一步补上「**继续跑**」。

#### 为什么不能靠加一个枚举

原生的 resume 是架构：`driveOperation`（`runtime/drive.ts:33`）是一个
**按 `state.at` 分派的循环**，状态从 lane 里读、不在栈帧里，所以重启后能从同一状态继续。

```ts
export async function driveOperation(lane, drive) {
	for (;;) {
		const state = operation.state;        // ← 从 lane 读，不是局部变量
		switch (state.at) { case "tools": result = await runTools(...); ... }
	}
}
```

rpi 的 `run_core` 是直线 `async fn`，状态全在栈帧；而 `run_agent_loop`
（`pi-agent/src/agent_loop.rs:72`）**一次调用就拥有整个多轮循环**，
所以崩溃后手上只有「日志里有什么」，没有「跑到哪一步」。

#### 用「空 prompt 的 run」实现同样效果

关键观察：`run_core` 本来就是以 `run_agent_loop(Vec::new(), context, ...)` 驱动的，
prompt 是单独落盘的。所以**一次没有新 prompt 的 run，就是「接着上次继续」** ——
分支已经以 tool 结果结尾，循环的下一件事正好就是被打断的那个 provider 调用。

新增：

- `PendingResume { interrupted_run_id, chain_origin_run_id, attempt }`。
- `create`（`allow_existing_session`）：先记下未关闭的 operation，
  跑 `finish_interrupted_operation` 修复，然后用 `lane_is_resumable` 判断
  分支是否处在可续跑状态（**末尾是 tool result**；带 `terminate` 的 tool 自己就是终点，排除）。
- `resume_pending()`：发一条 `runResume` custom notice（说清楚为什么自己跑起来了），
  然后 `run_core(Vec::new())`。
- `AgentLane` trait 加 `has_pending_resume()` / `resume_pending()`（2 个实现，同一文件）。
- **TUI 侧**：在「跑初始 prompt」之前调用，仅当命令行没给 prompt（`-p` 意味着用户
  已经说了下一步想干什么）。放在 TUI 而不是 harness 里，是因为 run 的输出去向必须
  和普通 run 走同一条 streaming/render 路径，且启动不能被一个用户看不见的 run 阻塞。

#### 崩溃循环的护栏（这一条是我加的，不是原生的）

一个确定性崩溃的 run 会在每次启动时被重新驱动 —— 花 token、重复副作用，
而用户只看到一次崩溃。所以续跑有预算上限 `MAX_RESUME_ATTEMPTS = 3`，
并用 **`OperationIntent::Run.resume_data`** 持久化（这个字段之前是纯占位，从没人写过）：

```json
{ "resumedFrom": "<chain 起始 run id>", "resumeAttempt": 2 }
```

链的**起点**一路传递（不在每步改指前一个 run），这样次数是「这条链」的属性，
而 `resume_chain()` 只需读一个 run 自己的 intent 就能算出下次是第几次 ——
不需要跨记录求 max。预算用尽时不再续跑，而是在转录里说明原因。

#### 验证

- `an_interrupted_run_is_continued_without_a_new_prompt`：崩在工具执行中 → 重开会话
  → 断言有 pending resume → `resume_pending()` 产出了下一个 assistant 轮次（
  无需新 prompt）→ 转录里有 `runResume` notice → 且不会重复提供。
- `a_run_that_keeps_dying_is_not_resumed_forever`：手工构造一条已耗进 3 次续跑的链
  （storage 限制每 lane 只能有一个 open operation，所以早期的都要先结清），
  断言 `pending_resume()` 为 `None` 且 `resume_pending()` 不做事。

#### 仍然不是完整的状态机

这一步做到的是「**从分支继续下一轮 provider 调用**」，
不是原生那种「恢复到精确状态」（例如崩在 `deferred.suspended` 里能接着轮询）。
真正的可重入状态机仍需拆开 `run_agent_loop`（turn 边界必须变成持久状态）。
但对于**绝大多数崩溃**（工具执行中，长任务的主要时间就在那里），行为已经与原生一致：
用户不用重新描述需求，run 自己跑完。

### 11.7.11 续跑决策矩阵（何时 *不* 该续跑）

实现续跑时最容易做错的是「什么时候不该续」。矩阵已用测试钉住：

| 崩在哪 / 状态 | 分支末尾 | 是否续跑 | 为什么 |
|---|---|---|---|
| provider 流式中（还没 tool call） | interrupted **assistant** 消息 | **否** | 续跑会把「以 assistant 结尾」的请求交给 provider，Anthropic 直接拒。`pi-agent` 自己的 `run_agent_loop_continue` 就为此报错（`agent_loop.rs`：「cannot continue from message role: assistant」）。原生也如此：出错的 assistant 响应是**终结**该轮，不是再要一轮。 |
| 工具执行中 | tool result | **是** | 分支正好停在「下一步就是给 provider 要下一个 assistant」的位置。 |
| 工具跑完、下一次 provider 调用前 | tool result | **是** | 同上。 |
| run 正常完成 | assistant 消息 | 否 | 没东西可续。 |
| **用户主动 abort** | —— | **否** | 这是构造上保证的：续跑要求存在 **open operation**，而 abort 会把它关闭。否则 agent 会自动重启用户刻意停下的活。 |
| 已续跑 3 次 | —— | 否 | 预算用尽（§11.7.10）。 |

三条测试：`a_crash_mid_stream_is_not_auto_resumed`、
`an_explicitly_aborted_run_is_not_resumed`、`a_completed_run_is_not_resumed`。

### 11.7.12 两处需要更正的说法（第三轮复查）

1. **「崩在 `deferred.suspended` 里不能接着轮询」是错的说法。**
   rpi 的 `resume_deferred` 直接返回 `HarnessError::not_implemented`
   （`agent_harness.rs:1281`）——当时 rpi **根本没有 deferred operation 这个功能**。
   所以那不是「恢复路径缺失」，而是「功能本身不存在」；恢复不会丢一个从未实现的特性。
   （**这条现在已经过时了**：deferred 已实现，见 §11.7.18 —— 保留是因为它当时的推理是对的。）
2. **崩在流式中不续跑不是漏洞，是正确行为**（见 §11.7.11）。
   我一度以为 `lane_is_resumable` 只接受 tool result 是个漏洞，读完 `run_agent_loop_continue`
   的前置校验后才确认：在那里续跑会构造出非法请求。

### 11.7.13 steering / follow-up 队列的持久化（已实现）

这是同一个形状的缺口（记录类型存在 + 读侧存在 + **写侧从不写**）：

- `QueueEnqueuedRecord` / `QueueCancelledRecord` 有完整定义（`session/types.rs:874`），
  reducer 也有校验；
- 之前全仓搜 `QueueEnqueued(` 只命中 `memory.rs` 的 stamping 和**测试构造**；
- `steering_queue` / `follow_up_queue` 是 `HarnessInner` 里的
  `Arc<Mutex<MessageQueue>>`，纯内存。

**后果**：agent 干活时你敲进去的 steer 消息（以及 follow-up / nextRun），
一崩就丢。原生对应的是 lane 级持久 inbox（`LaneState.inbox`）。

#### 设计：用「消费时就写 cancel」代替「让预留 id 落地」

两种可选做法：

- **A（未采用）**：消费时用**预留 id** 把消息落盘，重建规则为「enqueue 的 entry 未落地」。
  难点：steering/follow-up 是在 `pi-agent` 的循环**内部**被抽走的
  （`get_steering_messages` 回调），此时消息已经进了 `new_messages`，
  最终由 `persist_new_messages` **新铸 id** 落盘 ——
  预留 id 永远不落地，重建时会把**已消费**的消息复活成幽灵重复消息。
  要修就得把「已持久化下标」的关联穿过循环传出去，很难且易错。
- **B（采用）**：抽走时就写一条 `QueueCancelled`（消费即取消）。
  重建规则变成简单的「enqueue 无后继 cancel」：
  已消费的被 cancel 排除，未消费的保留。预留 id 从不需要落地。

B 的关键约束：cancel **必须在消息成为 entry 之前**写 ——
reducer 的校验是 `!entry_exists`（被取消的目标 entry 不能已存在）。
因为在 B 里消息始终用新 id 落盘，预留 id 永远不存在，所以这个条件始终成立。

代价（已记录）：抽走后到消息真正落盘之间有一个窗口（对 steer/follow-up 而言
是「抽走 → run 结束」，可能很长），在这个窗口崩溃会丢这条消息。
但这与改动前的行为**相同**（改动前整个队列都丢），而赢下的是
**未消费队列的持久化** —— 那才是「agent 干活时你敲的话」的大头。

#### 实现

1. **写侧**：`enqueue_message`（`steer`/`follow_up`/`next_run` 共用的唯一路径，
   顺带消除了原先三份重复的方法体）在入内存队列后写 `QueueEnqueued`，
   `target` 用 `provisioned_target()` 构造 —— 和 reducer 比较用的是**同一个函数**。
   `run_id` 与条目一起记录，因为 cancel 必须带上它。
2. **消费**：`drain_for_lane` 不再丢掉 `QueuedMessage.entry_id`（这是关键），
   `drain_queue` / `drain_shared_queue` 在抽走每条时写 `QueueCancelled`；
   `clear_queue`（把队列退回编辑器）同样算取消。
   用户主动 `cancel_queued` 也会写 —— 而且带的是**从 enqueue 记录里查出来的** run_id：
   reducer 用 run_id 配对，而「run 中入队、run 结束后取消」在两边不一致时会被判日志损坏。
   只有在 enqueue 记录存在时才写 cancel，否则就撞上「cancellation 没有匹配的 enqueue」那条损坏检查。
3. **重建**：`create` 调 `rebuild_queues`，按记录顺序把「无 cancel 的 enqueue」
   还原成 `QueuedMessage` 并推回对应对列；
   消息从记录的 `target` 反序列化（补回 seq/parentId/timestamp 占位再走 `Entry` 的反序列化）。

#### 验证

- `a_steer_message_survives_a_restart`：跑一个长流，等它**开始流式输出**后 steer
  （必须等 —— 循环开头就会抽一次 steering，早于那一刻发的消息会被 run 自己消费掉，
  这是正确行为但不是这个测试要测的），
  断言 enqueue 已落盘 → 杀进程 → 重开 → 消息**仍然在队列里**，
  并且能用**原 id** 取消。
- `a_cancelled_queued_message_does_not_come_back`：取消后重启，不得复活。
- `queue_records_validate_against_the_reducer`：整份 record log 过 `validate_record_log`
  —— 包括「cancel 的 run_id 必须与 enqueue 相同」这条容易踩的校验。

### 11.7.14 多轮 run 的崩溃：一个被单轮测试掩盖的真 bug

前面的恢复测试全部基于**单轮** run（assistant → tools → assistant）。
补上多轮（assistant → tools → assistant → tools → assistant）后发现两个串在一起的 bug，
两个都是 commit-on-settle（§11.7.7）引入的，且都被单轮测试掩盖：

**bug 1：打捞会把已提交的流重放一遍，造出重复的 assistant 消息。**

commit-on-settle 在每条 assistant 消息定稿时就把它提交为 entry，但
`ClearRun`（退掉整轮帧）只在整个 run 结束时才写。所以在**第二轮工具执行中**崩溃时：

- assistant#1 已是 entry（真实条目）、assistant#2 已是 entry；
- 而帧记录里 **两路流都还在**（stream 0 与 stream 1）；
- 打捞于是把 **两路都重放**，把 assistant#1 又当成 interrupted 消息追加一遍。

实测：3 个 assistant 消息的 run 崩在第二轮后，分支上出现 **4** 条 assistant 消息。

修法：新增 `AssistantFrameOp::ClearStream` —— 提交一条 assistant 消息时
**同时退掉它自己那一路流**。打捞时跳过已退掉的流。
流序号靠事件顺序对应：第 n 条定稿的 assistant 消息就是记录器的第 n 路流
（只有 assistant 消息会开流，两者都在同一事件序上分配）。

**bug 2：修 bug 1 把工具解析也一起跳过了。**

原来未落盘的 tool call 是从**打捞出来的 partial**里取的。一旦跳过已提交的流，
那条流里的 tool call 就再也不会被解析 —— 于是分支上留下一个
「assistant 带 tool call、但没有任何 result」的消息，**下一个请求依然是非法**。
实测：期望 2 条 tool result，只得到 1 条。

修法：**工具解析不再依赖「哪一路被打了捞」**，分两个来源合并（按 `tool_call_id` 去重）：

1. 未提交流的 partial（同上）；
2. **分支末尾的 assistant 消息** —— 它是一个已提交、但工具还没跑完的消息。
   这一条还顺带盖住了另一个极小窗口：消息已提交、但 `tool_started` 还没写
   （两个事件之间崩溃）—— 那种情况从记录里根本查不到。

在末尾追加结果是**顺序正确**的：循环会先跑完整批工具才会产生下一条 assistant 消息，
所以可能存在未完成工具调用的只可能是**最后那条** assistant。

#### 验证

- `a_multi_turn_run_resumes_after_a_crash_in_a_later_tool_call`：
  脚本两个 tool call（第三个是文本）；测试工具在第 **2** 次调用时挂住（`hang_at = 2`），
  所以第一轮真实完成、第二轮死掉。断言：崩溃后分支上**恰好 2 条** assistant、
  **恰好 2 条** tool result（第二条是「结果未知」）、能从 tool result 继续跑完、
  且整份日志仍然合法。
- `a_multi_turn_run_leaves_a_valid_record_log`：不崩溃的多轮 run，
  断两个 `step_attempt` 的 attempt 都是 **1** —— 这看上去像违规，其实是正确的：
  reducer 的「连续 attempt」规则只适用于**结果尚未落盘**的活系列，
  已落盘的结果**终结**该系列（详见测试注释）。

### 11.7.15 崩在重试退避中：已实现（用一条自己的记录绕开 reducer 的序列规则）

这是最后一个“崩溃导致 run 结束”的情形。一次 attempt 以可重试的 provider 错误失败后，
harness 在 sleep 等重试，而这个窗口里**什么都没落盘**（失败 attempt 的消息刻意不提交），
所以重启后分支末尾还是用户的 prompt。它本来是**要重试**的。

#### 第一次尝试失败的原因（值得记下来）

我原本想用帧判定“这是在等重试”，**做不到**，而且原因很具体：

- 帧记录里**没有“终止帧”**；失败 attempt 归约出的消息 `stop_reason` 是 `Pending`
  （`reduce_frames` 开头就设 Pending，之后没有帧去改它）；
- 所以“这是一次可重试失败”**无法从帧里读回来**；
- 失败 attempt **没有其他任何痕迹**：`step_attempt` 只在提交时写。

**我接着又踩了同一个坑一次**：修的时候我先写了
“按 partial 判断 `stop_reason == Error && is_retryable` 就跳过打捞”，
而那个条件**永远为假**（同一个 Pending 原因）—— 测试直接把它抓出来了
（分支上多出一条 interrupted 消息）。最终改成**run 级判断**：既然
`retry_pending` 已经说明“这条 run 在等重试”，那么它未提交的流就都是这条重试链的失败尝试，
不需要（也无法）逐个辨认。

#### 为什么不用 `step_attempt`（这是关键设计决定）

看起来最自然的是复用 `step_attempt` 的 `attempt` 字段。但 reducer 的
`validate_attempt_sequence` 把 `attempt` 当作**序列内序号**，而“已落盘的结果会终结序列”
（§11.7.14：多轮 run 的两条 assistant 消息是两个独立系列，各自 attempt 1）。
于是多轮 run 里“重试第 ≥2 次”的那个 attempt 的第二条 assistant 无法取号 ——
写 2 会被期待 1 → **判日志损坏**。

所以新增了 `LaneRecord::RetryPending`（`RetryPendingRecord { run_id, attempt, result_entry_id }`）：

- 在**退避 sleep 之前**写，所以等待期间崩溃能被识别；
- `result_entry_id` 是**为该 attempt 预留的 assistant entry id**：
  它一旦落地就说明重试成功了，没有待续之事 —— 这就是“跳过打捞”与“提供续跑”
  两个判断共用的同一个依据；
- reducer 侧是**无校验接受**（和 `AssistantFrame` 一样：它引用任何 entry、
  不施加树不变量），所以**完全不动 attempt 序列规则**。

新增记录类型要同步的地方（和 §11.7.1 加帧记录时一样四处）：
`types.rs` 的变体 + `base()`/`record_type()`/`run_id()`、`memory.rs` 的 stamping、
`reducer.rs` 的接受、`jsonl/codec.rs` 的 `RECORD_TYPES` 白名单。

#### 另外修正的一处判断

`resume_pending()` 原来复用了 `lane_is_resumable()`（要求分支末尾是 tool result）。
这对“等待重试”的情形是错的（此时末尾是用户 prompt），会把刚记录好的续跑机会丢掉。
改成检查**真正的那条约束**：**分支末尾不能是 assistant 消息**
（provider 会拒，`run_agent_loop_continue` 也正因此报错）。
“是否**提供**续跑”仍由 `create` 决定，这里只是最后一道防线。

#### 验证

- `a_crash_during_retry_backoff_still_retries`：退避 30s 给足窗口 →
  等 `retry_pending` 落盘 → kill → 重开 → 断言：**没有** interrupted 消息、
  有 pending resume、续跑后拿到成功那次 attempt 的输出。
- `a_successful_retry_leaves_nothing_pending`：
  重试在**同一进程内**成功时，`retry_pending` 记录仍在日志里，
  唯一阻止它再次触发的是**预留 entry 已落地** —— 这条测试专门钉住这一点。

### 11.7.16 注入的 steer 消息：改为在它自己定稿时落盘

§11.7.13 里我给队列持久化记了一个「代价」：抽走（=取消）之后到消息真正落盘之间有个窗口，
在那里崩溃会丢掉这条消息 —— 当时判断「与改动前相同，可接受」。做完 §11.7.15 后回头看，
这个代价其实**可以直接消掉**，而且它丢的是用户**亲手敲进去的那句话**：

- 注入的消息是循环**内部**唯一的 user 消息（`run_core` 用空 prompt 驱动，
  调用方的 prompt 由自己落盘）；
- 而 user 消息**不在** commit-on-settle 的范围内（那时只提交带 tool call 的 assistant
  与 tool result），所以它一直要等到 **run 结束**才落盘；
- 但队列项在抽走时就已经被 cancel —— 于是「模型已经看到、转录里却没有、队列里也没了」：
  不可恢复。

**修法**：让注入的消息也走 settle 提交。`SettleState` 增加
`injected: VecDeque<(entry_id, message)>`：

- `drain_injected`（两个注入回调走的路）在抽走每条时把 `(预留 id, 消息)` 登记进 settle 观察者；
- `MessageEnd{User}` 时从队首取出一条，**比较内容一致**后用那个预留 id 提交，
  并把下标记进 `committed`（run 末尾的 `persist_new_messages` 于是跳过它，不重复）；
- 内容比较是刻意的保守做法：万一两者不一致（未来循环改写消息），
  宁可不提交，也不要往那个 id 里写错文本。

这需要把 `settle_state` 的创建**提到 `config` 构建之前**（两个注入回调要能捕获它）。

**验证**：`an_injected_steering_message_survives_a_crash` ——
steer 在 run 开始**之前**入队（于是它会在循环第一个边界被稳定抽走并注入），
然后 run 死在**第二次**工具调用里（脚本 tool_call → tool_call → text，工具第 2 次调用挂住），
即远早于 run 结束。断言：注入的文本**在 run 还在跑的时候**就已经是 entry
（证明它来自 settle 提交，而不是不可达的 run 末尾）；崩溃重开后它**仍然在**。

这条测试我用「临时把新提交路径改成直接 `return`」验证过**不是空跑** —— 会失败。

### 11.7.17 R2 判定：逐 state 对照原生「结果」，结论是不需要状态机

我前几轮一直把 R2（移植原生那套可重入的 13 状态机）当作「剩下的架构工作」。
在补完前面那些之后，我把它逐 state 对了一遍 —— **对照的是每个状态的「结果」，
不是实现方式**。结论是：在 rpi 已建模的每个状态上，结果都已经一致。

| 原生 `state.at` | 崩在这里时 rpi 的结果 | 测试 |
|---|---|---|
| `starting` | 操作已开、prompt 已落盘、还没请求 → 重启**续跑**（§11.7.17 新增） | `a_crash_before_the_first_request_still_continues` |
| `checkpoint` | 无显式对应状态；「下一步要做什么」由**分支末尾**决定（tool result / user message） → 恢复后重新走到同一决策 | 同上 / `a_multi_turn_run_resumes_...` |
| `assistant.ready` | 同上（还没产生任何东西，末尾未变） | `a_multi_turn_run_resumes_...` |
| `assistant.effect_pending` | 已提交前缀打捞成 interrupted 消息，run 结束 | `a_crashed_run_can_be_salvaged_...`、`a_crash_mid_stream_is_not_auto_resumed` |
| `assistant.retry_wait` | `retry_pending` 记录 → 重启**重试** | `a_crash_during_retry_backoff_still_retries` |
| `tools` | `tool_started` + 双重确认重放或标「未知」 → **续跑** | `crashing_during_a_tool_call_...`、两条 replay 测试、`a_partially_completed_tool_batch_...` |
| `deferred.suspended` / `deferred.effect_pending` | 已接通（§11.7.18）：轮询 → 落盘 → 仍 Deferred 则继续挂起，否则结清并从 assistant 进入循环（先跑工具） | `a_deferred_response_suspends_and_then_resumes_through_its_tools`、`a_still_deferred_poll_keeps_the_run_parked` |
| `summary.deciding` / `summary.ready` | 压缩决策在下次 run **重评估**（重评估即幂等） | `a_crash_during_compaction_leaves_a_usable_session` |
| `summary.effect_pending` | 同上；且 `write_deferred` 已写，提交与否都能正确分类 | 同上 + `compaction_intent_records_validate_...` |
| `summary.retry_wait` | 压缩调用有自己的 retry 策略；崩了则整个压缩重评估重做 —— 结果同为「压缩最终完成」 | 同上 |
| `navigation.ready_to_commit` | 导航是用户动作；崩在提交前则未生效，用户重做一次（**唯一一处结果略有差异**，见下） | —— |

**所以 R2 的价值不是「多一层保证」，而是原生**实现**这些保证的方式。** 原生的可重入状态机与
rpi 的「从分支推导状态 + 把进度写进记录流」在已建模的状态上给出**相同的可观察结果**。
差别只在实现：原生存「我在哪一步」，rpi 存「已经产出什么」并从它推出「下一步」。

因此**不做那个重写**：它会动 `run_agent_loop`（拆出持久 turn 边界）、
`persist_new_messages`、压缩 cut 逻辑与一批断言当前语义的测试，而换不来行为差异。
`§11.7.4/§11.7.6` 当初把 R2 列为剩余工作，是因为当时还不知道「分支＋记录」能否覆盖全部状态 ——
现在可以了，这一节就是那个证据。

两处**确实**的差异，都不值得为它们重写：

1. **`navigation.ready_to_commit`**：崩在导航提交前，rpi 不会自动补提交（用户重做 `/tree`）。
   但导航是**用户当场发起的动作**，在用户不在时替他把分支移走，弊大于利。
2. **`deferred.*`**：见 §11.7.18 —— 那是**功能未实现**，不是恢复缺口。

### 11.7.18 deferred operation：已实现（缺的那三件都补上了）

我一开始以为这是从零开始的大特性。读代码后发现 **suspend 管道已经现成大半**，
只缺最后一环 —— 这也是为什么它值得做：不是造新机制，而是接通已有机制。

**原本就有的**（逐条核过）：`StopReason::Deferred`、`AssistantMessage.deferred`、
`DeferredHandle`；`derive_outcome` 把 Deferred+handle 映射成 `HarnessRunOutcome::Suspended`
（缺 handle 时给 `deferred_no_handle` 失败）；run 尾部对 `Suspended` 跳过
`operation_finished`/`run_end`（操作保持打开）；`find_deferred_handle(suspended_id)`
从分支取回 handle；`drive(operation_id)` 与 `resume(suspended_id)` 两个入口。

**补上的三件**：

1. **provider 能力**（`pi-ai/src/provider.rs`）：新增 `DeferredProvider` trait
   （`stream_deferred` + 默认 `cancel_deferred`），并在 `Provider` 上加**带默认实现**的
   `fn deferred(&self) -> Option<&dyn DeferredProvider> { None }`。
   这样另外 **10 个** `Provider` 实现一行都不用改 —— 与 `ProviderHooks` 那段注释的顾虑同源，
   默认实现把顾虑消掉了。faux 实现它（脚本化的 poll 队列），于是特性可以端到端测。
2. **`pi-agent` 的「从末尾 assistant 开始」入口**（关键的一件）：
   `run_agent_loop_from_assistant`。`run_loop` 增加一个可选的
   `pending_assistant`：设了它，第一轮就**执行这条消息的工具**而不是去请求 provider，
   并且**不**把它再 push 进 `new_messages`（它已经是转录的一部分）。
   之所以必须有它：轮询拿回的 assistant 消息的工具**从未跑过**，
   而带着「没有结果的 tool call」去请求会被任何 provider 拒绝 ——
   用现成的「空 prompt 继续」会让 provider 再答一次，那是**非法请求**。
3. **`resume_deferred`**（harness）：取回 provider 能力 → 用快照里的超时/thinking
   构造 `SimpleStreamOptions` → 轮询 → **把轮询结果落盘**（它既是真实历史，
   也是新 handle 的发现来源）→ 若仍是 `Deferred` 则**继续挂起**（操作保持打开），
   否则结清被挂起的操作并继续：**若该消息带 tool call，就从它进入循环**（第 2 件）。

**一个刻意的安全选择**：从 assistant 进入的那次循环**只做一次尝试，不重试**。
因为它的第一个动作就是执行那条消息的工具，而重试会把工具**再跑一遍**。
失败就直接上报，不重试。这是我在接线时发现的真实危险（不是理论）。

**provider 缺失时如实报错**：没有 long-poll 能力的 provider 返回
`not_implemented`，而不是假装 run 完成 —— 后者会记录一个没人产生过的结果。

#### 验证（3 条）

- `a_deferred_response_suspends_and_then_resumes_through_its_tools`：
  挂起一条带 handle 的 assistant 消息 → `resume` → 轮询返回一条**带 tool call** 的消息
  → 断言 run 完成、且那个 tool call **真的被执行了**（分支上有它的结果，
  不是「未知」占位）。这是第 2 件存在的理由。
- `a_still_deferred_poll_keeps_the_run_parked`：轮询返回**新 handle** →
  断言仍是 `Suspended` 且带上了新 handle，并且新 handle 可被 `drive` 继续轮询。
- `resuming_through_a_provider_without_the_capability_is_an_error`：
  handle 指向未注册的 provider → 断言报错而不是假完成。

**仍未做的**：`pi-ai` 里**没有任何真实 provider 实现** long-poll deferred API，
所以这条链目前只由 faux 驱动。接一个真实 provider（如某个 batch/long-poll 接口）
是它各自的小活，不再是机制问题。

### 11.8 验收标准
### 11.8 验收标准
### 11.8 验收标准


§4.6 最初提的五条，逐条对照现在的测试（每条都有测试，无一条是推定）：

1. **在 run 进行中 kill -9，重新进入后能看到崩溃前已提交的内容**
   —— 满足。`a_crashed_run_can_be_salvaged_from_its_committed_frames`（内存）与
   `frames_survive_a_reload_from_disk` / `reloaded_frames_reduce_to_an_interrupted_message`
   （JSONL 真实落盘往返）；`reopening_a_crashed_session_restores_the_committed_prefix`
   走的是真实恢复入口。
2. **崩溃会话不再留下未关闭的 operation** —— 满足。
   `reopening_a_crashed_session_restores_the_committed_prefix` 与
   `a_crash_during_compaction_leaves_a_usable_session` 都显式断言 open operations 归零。
3. **正常 / 中止 / 重试成功三种路径下不出现重复条目** —— 满足。
   `run_with_no_tool_calls_completes_in_single_turn`、
   `full_run_with_write_tool_call_completes_and_persists`（条目链断言）、
   `a_retried_attempt_is_not_committed`、
   `recovering_the_same_crashed_session_twice_does_not_duplicate`。
   这条在实现中真起了作用：帧一度被写成 custom entry，正是这两条条目数断言把它拦下的（§11.7.1）。
4. **历史里不出现非法输入**（不能把非法/半成品消息当正常上下文回传）—— 满足。
   两个面：被打断的 assistant 消息会配齐合成 error tool result
   （`crashing_during_a_tool_call_leaves_a_valid_transcript`）；
   而「以 assistant 结尾」的情形干脆**不续跑**（`a_crash_mid_stream_is_not_auto_resumed`）。
   另有 `a_real_run_leaves_a_valid_record_log` 拿整份日志过 `validate_record_log`。
   （重试语义本身未改：仍是重做整轮。）
5. **有一条真实 kill 进程的集成测试** —— 满足，且不止一条：
   那些 `abort()` 掉 run task 的测试落盘的都是真实的中间前缀；
   另有 `a_replayable_tool_is_re_run_and_its_real_result_recovered` /
   `an_unreplayable_tool_is_not_re_run` / `a_steer_message_survives_a_restart`
   覆盖崩溃后的各种处置。

### 11.9 最终小结

这个缺口从头到尾只有一个根因：**rpi 的 run 写侧不持久化进度，而原生把「操作状态」
本身当成持久对象**。修的过程是把这个根因拆成能分别验证的几层：

| 层 | 做了什么 | 关键实现 |
|---|---|---|
| 进度落盘 | assistant 每帧即时写入记录流（不是 entry） | `frame_progress.rs` |
| 崩溃打捞 | 重放已提交前缀；每个未落盘 tool call 补一个结果 | `resolve_interrupted_run` |
| 写侧意图 | 消息定稿即提交（含**注入的 steer 消息**）+ `step_attempt`/`tool_started`；compaction 的 `write_deferred` | `settle.rs` |
| 工具重放 | 双重确认后安全重跑，否则标「结果未知」 | `resolve_tool_call` |
| 启动续跑 | 空 prompt 的 run = 继续；含 `starting`/工具中/等待重试三种入口 + 崩溃循环护栏 | `resume_pending` |
| 重试意图 | 退避前写 `retry_pending`，重启改为**重试**而非打捞失败尝试 | `RetryPendingRecord` |
| 队列持久化 | steering/follow-up/nextRun 不再只存内存 | `enqueue_message` / `rebuild_queues` |

**覆盖的崩溃点**（每个都有测试）：流式中、工具执行中（单轮 / 多轮 / 并行批量）、
工具跑完待下次调用、压缩中、重试退避中、队列有未消费消息、注入消息、
以及**第一次 provider 调用之前**。

三处我中途说错、后来自己推翻并更正的（都留在文档里，不抹掉）：

1. `step_attempt` / `write_deferred` / `danglingWrite` **在原生里不存在** ——
   它们是 rpi 自己的设计，不是 parity 缺口；
2. 原生**没有** `RecoveryDecision::Resume`（它的恢复是可重入状态机）；
3. 「消息路径应该用 `write_deferred`」是错的 —— 流式消息的内容无法提前钉死，
   原生用的也是纯 id 预留。

还有一处**同一个坑踩了两次**：我两次试图用「帧归约出的消息 `stop_reason`」判断
「这是一次可重试失败」，而它在两种情况下都不成立（帧里没有终止帧，
归约结果永远是 `Pending`；失败 attempt 也没有任何其他痕迹）。最终用一条
自己的记录解决，见 §11.7.15。

**不做的事**（§11.7.17 有逐 state 对照的证据）：

- **可重入状态机（R2）不做。** 逐 state 对照原生后，rpi 在已建模的每个状态上
  **可观察结果一致** —— 那两个机制（原生存「我在哪一步」、rpi 存「已产出什么」
  并推出下一步）是同一组保证的两种实现。为此重写 `run_agent_loop` 与持久化主干
  换不来行为差异。
- **deferred operation 已实现**（§11.7.18）：补上了 provider 能力
  （`DeferredProvider` + 默认的 `Provider::deferred()`）、`pi-agent` 的
  `run_agent_loop_from_assistant` 入口、以及 `resume_deferred`。
  仍未做的只是「哪个真实 provider 去实现 long-poll」——按 provider 各自的小活。
