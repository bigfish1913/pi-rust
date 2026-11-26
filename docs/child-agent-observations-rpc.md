# Delegated child observations (RPC v1)

`ready.childAgents` advertises an additive host-only observation contract. It is
separate from ToolProgram children (nested tool invocations) and from root
sessions. Nothing here grants a plugin access to another plugin's child handle.
The plugin ABI and its owner-scoped start/poll/cancel/dispose operations are
unchanged. These commands are available only to the controlling native RPC host.
The host advertises the profile and its three commands only when its extension
action bridge is active. Hosts started without that bridge omit both.

Every request requires `version: 1` and `parentSessionId` matching the harness's
current session. Ordinary RPC `id` correlation is unchanged.

```json
{"type":"get_child_agents","id":"list","version":1,"parentSessionId":"PARENT"}
```

The roster returns `version`, `parentSessionId`, an opaque native `epoch`,
`retention: "volatile"`, and at most 32 `children`, newest started first. Rows
include `childId`, `status` (`running`, `done`, `failed`), `role`, `taskPreview`,
`turns`, `startedMs`, `finishedMs`, `earliestSeq`, and `lastSeq`. Disposing a plugin
handle does not erase its retained observations or resurrect that handle.

```json
{"type":"get_child_agent_events","id":"events","version":1,"parentSessionId":"PARENT","epoch":"EPOCH-FROM-LIST","childId":"CHILD","afterSeq":0,"limit":32}
```

Keep the complete `(parentSessionId, epoch, childId, afterSeq)` cursor tuple.
Pages include that binding, `retention`, `earliestSeq`, `lastSeq`, `nextSeq`,
`hasMore`, `resyncRequired`, and `events: [{seq, event}]`. Resume strictly after
`nextSeq`; `afterSeq > lastSeq` is invalid. A page contains at most 32 events and
131072 encoded bytes. Events reuse the compact native agent event projection
(message lifecycle/deltas, tools, turns), plus a native `child_status` marker (job status, not cleanup settlement).
An event over 32768 encoded bytes becomes an explicit `observation_gap` with
`reason: "event_too_large"`, never a silently truncated message.

Each child retains at most 256 events / 1 MiB of observation payload. If an older
cursor was evicted, `resyncRequired: true` accompanies the retained tail. Clients
must visibly mark the gap; absence of a final event is not successful completion.
Read from the same cursor after an empty page to wait for future events. A
client can establish a cursor before polling without losing a handoff event.

```json
{"type":"cancel_child_agent","id":"stop","version":1,"parentSessionId":"PARENT","epoch":"EPOCH-FROM-LIST","childId":"CHILD"}
```

Cancellation goes through the native job owner; adapters must not kill native
processes themselves. `cancelled` acknowledges the cancellation signal, not OS
cleanup completion. A finished child returns `already-finished`. Unknown,
foreign-parent, evicted or stale-epoch children fail explicitly.

Observations remain **volatile**: the oldest finished children are evicted at the
32-child cap and process restart loses the entire observer epoch. This is not a
durable transcript export, historical root-session load, independent child
prompt/resume API, or recursive delegation. Model and workspace inheritance and
parent-run cancellation semantics are unchanged.

## 中文说明

该接口由原生宿主管理数据，仅允许当前父会话查询或取消子 Agent；不会扩大插件的
跨 owner 权限。列表、事件分页和取消均显式协商 v1 能力。子插件 dispose 后保留
有界观察记录，但不恢复插件句柄，也不创建可独立续聊的根会话。

客户端必须同时保存父会话、epoch、子会话 ID 和序号。分页最多 32 条 / 128 KiB；
每个子任务只保留 256 条 / 1 MiB，淘汰时明确返回 resyncRequired；超过 32 KiB 的
单条事件返回 observation_gap。最多保留 32 个子任务，进程重启后记录消失。
取消响应仅证明原生已接受取消信号，不能当作操作系统进程已清理的证据。

## Native ToolProgram records inside a delegated child

Delegated child sessions publish their own canonical native journal commits into
that child's existing bounded observation tail. `tool_program_child_begin` and
`tool_program_child_phase` retain the native source Session, lane, entry ID,
sequence, origin and descriptor. No independent HTTP Session is created.

`tool_program_child_body` contains a verified native body fragment with
`sourceSessionId`, `lane`, `childId`, `phaseEntryId`, `descriptorId`, `byteOffset`,
base64 `data` and fragment `sha256`. Fragments are at most 8192 decoded bytes;
a phase hydrates at most 256 KiB. The canonical phase descriptor remains the
whole-body identity and checksum. A consumer must not claim full-body integrity
until all descriptor bytes are available and its SHA-256 matches.

The child runner owns the commit receiver, reads bodies through the existing
strict harness query, and drains after native cleanup. Neither observation nor
hydration executes tools or changes run settlement. Channel lag, body limits,
source loss and bounded drain timeout produce `observation_gap`. Existing tail
retention (256 events / 1 MiB) and epoch rules remain unchanged. Consumers can
mirror the observed records with explicit gaps; the tail is not durable history.
