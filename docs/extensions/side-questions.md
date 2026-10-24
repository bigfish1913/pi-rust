# SideQuestion runtime action (25), protocol v2

`SideQuestion` is command-level, owner-scoped, tool-free inference with a host-owned native side-chat workspace. It never acquires the main run lane or writes the parent transcript/steering queues. It is separate from `ChildAgent`; do not relax that action's invocation/runtime-key checks.

## Wire protocol

The existing `PluginApi.runtime_action` slot accepts JSON; no C ABI vtable changes are required.

| Operation | Input | Result |
| --- | --- | --- |
| capabilities | `{"op":"capabilities"}` | `{"protocolVersion":2,"sideChat":true,"queuedFollowUps":true}`; no request or view is created |
| start | `{"op":"start","question":"..."}` | Starts or continues this owner's thread; `status: running` or `queued`, plus thread/request IDs |
| new | `{"op":"new","question":"..."}` | Retires the old thread and queue, starts a new topic |
| status | `{"op":"status"}` | Latest full snapshot, including complete bounded text and queue |
| cancel | `{"op":"cancel"}` | Cancels current generation and all pending questions; keeps completed turns |
| clear | `{"op":"clear"}` | Cancels and removes the thread/history/view |
| hide / show | `{"op":"hide"}` / `{"op":"show"}` | Hide/resume; show creates an empty thread if necessary |

An unknown owner returns idle for status/cancel/hide and cannot read another owner's thread. Invalid new/start input is rejected before the old thread is retired. Model, credentials, owner and parent session cannot be selected through plugin JSON. New plugins must probe capabilities before starting: old hosts must not silently start independent requests while claiming to support follow-ups.

Registration-level commands receive their own opaque runtime handle in `CommandHandlerFn.user_data`. It is not a bridge pointer and must not be cached globally. Only host function pointers can be shared. Tool-invocation and root/unowned calls are rejected.

## Native workspace

The CLI uses `UserMessageComponent`, `AssistantMessageComponent`, an independent `ScrollView`, and an independent `Editor`. Markdown and code blocks reuse the native renderer. Full bounded answers are retained; the former 160-byte line splitting / 28-line preview is removed. Generic passive `UiComponent` panels are not turned into chat widgets.

- Open/resume via `/btw` or `/btw show`; follow up directly in the BTW editor.
- Enter submits; running requests accept FIFO queued questions.
- PgUp/PgDn scroll; Ctrl+Home/Ctrl+End go to first/latest content. Manual browsing remains detached from follow-end across streaming updates and reflow.
- At >=100 columns, the main workspace remains on the left and BTW appears on the right. Click to choose keyboard focus. Wheel events target the pointer's pane without stealing focus.
- Narrow terminals and regular/native-scrollback mode use the side view alone. Closing restores the main workspace and draft.
- Esc hides without cancelling. Ctrl+C while side-focused cancels only BTW generation and queued questions. Modal dialogs, provider authentication and approval interactions take priority.

The host internally exposes revisioned snapshots through `RuntimeActionHost::side_question_revision` / `side_question_views`. `side_question_ui(owner, thread_id, args)` validates a live thread identity before accepting native UI input. These are host-internal Rust methods, not cross-owner runtime actions or cross-DLL component callbacks. Replaced-thread UI events are rejected.

Snapshots include owner/thread/session IDs, visibility, focus revision, turns, queued questions, latest status/question/text/error and `historyTruncated`. Explicit open/start requests can focus the view; streaming deltas do not steal focus from the main editor.

## Context, execution and lifecycle

Every accepted question prepares an independent in-memory child harness using its submission-time effective parent model/provider binding and thinking level. A queued child does not run until preceding work has actually cleaned up. Tools are empty, max turns is one, and parent transform/projector/approval hooks are not installed. Provider observation hooks remain available.

The initial committed parent branch supplies up to 64 KiB of user/assistant text only. Tool protocols, images, thinking and custom metadata are excluded. This reference excerpt stays fixed for the thread. Successful side turns are replayed as actual user/assistant messages into each independent child session. Failed/cancelled partial answers may be displayed but are not replayed as successful context. Main-session content is never modified.

Each owner retains one thread. Side history is memory-only, at most 16 completed turns and 1 MiB of question/answer text; trimming removes oldest complete turns and sets `historyTruncated`. Only text is retained from assistant responses for follow-up context.

Main completion does not cancel side work. Parent session switch/close and plugin retirement cancel and remove running, completed and empty views. A weak owner-bound sink still validates bridge liveness (`op: check`); it holds no plugin callback or DLL pointer. Cancellation runs outside the bridge lifecycle gate and no provider work occurs under the registry/thread state locks. Late output cannot resurrect a retired thread.

Bounds: 20,000 UTF-8 bytes per question, 8 queued questions per thread, 120-second request deadline, 256 KiB per answer, 8192 deltas, 16 retained owners, 8 running/cleaning thread workers per host. Output/queue limits fail explicitly. The worker retains its execution permit until actual foreign/provider cleanup completes; cancelling is not forcibly killing a foreign thread. New questions on a cancelled-but-cleaning thread wait behind its cleanup.

Headless hosts keep queryable state but do not render a workspace. Missing capabilities produce upgrade errors; there is no subprocess/model fallback.

## Verification

```sh
cargo test -p rpi-cli --lib side_questions
cargo test -p rpi-tui --lib scroll_view
cargo test -p rpi-extensions --lib side_questions
cargo test -p rpi-harness --lib side_
```

Build `rpi-package/packages/rpi-btw`, set `RPI_TEST_BTW_LIBRARY` to the actual DLL/so/dylib, then run:

```sh
cargo test -p rpi-cli --test btw_package
```

Without that variable the opt-in integration test skips. Tests use local fake providers and require no real API key. Native terminal playtesting remains a separate acceptance step.
