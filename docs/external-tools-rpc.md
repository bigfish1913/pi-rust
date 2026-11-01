# Native external tools over JSONL RPC

This additive capability is for a **trusted RPC controller**. It is not MCP,
ACP, a sandbox, a new approval system, or a read-only-policy assertion.
No tools are installed by default. Existing message update encoding is unchanged.

## Configuration

`{"type":"configure_external_tools","id":1,"tools":[{"name":"remote_lookup","description":"Lookup","inputSchema":{"type":"object"}}]}`

Atomically replaces this connection's external definitions while idle. Both
accepted foreground admission and lane activity reject configuration. Invalid
batches leave the registry and active set unchanged. Native/plugin entries and
their active selection are retained; new external tools become active. Empty
`tools` clears external definitions. Response data contains `tools` (installed
names) and `activeTools`. Native/security names remain reserved even if disabled;
all existing plugin/native registry names are reserved as well.

Limits: 128 definitions, 256 KiB aggregate encoded definitions, 1..64 ASCII
alphanumeric/underscore/hyphen names, 8 KiB descriptions, object schemas with
`type:object`. Arguments are validated before the backend runs. External
descriptors do not establish trusted read-only provenance for any future mode
implementation.

### Supported schema subset

Default JSON Schema draft is Draft 7. Explicit built-in Draft 4, 6 and 7
`http://json-schema.org/draft-0N/schema` declarations (with optional final `#`)
are accepted; custom/newer dialects are rejected. Actual references must be
root-local fragment JSON Pointers, with ordinary `~0`/`~1` escapes. Acyclic local
references work. Cycles (including unused/consuming recursion), anchors,
percent-encoded fragments, missing/non-schema targets, schema-position `id`/
`$id`, and recursive/dynamic reference directives are rejected **at registration**
before either native argument validator sees the schema. This avoids stack
overflow in the current validator; arbitrary recursive schemas are not supported.

Schema graphs are limited to 4096 nodes, expanded depth 64 and 16384 expanded
node visits. Literal `const`/`enum` data and ordinary annotation data remain
opaque. When actual references exist, annotation IDs containing fragments are
also rejected because this validator indexes those IDs as resolver aliases.
These limits do not hard-preempt all synchronous validator work (for example
regular-expression execution).

## Invocation and response

Native emits `external_tool_request` with `requestId`, `toolName`, `arguments`.
IDs are random host-owned IDs, never caller IDs. At most 64 invocations may be
outstanding. The operation deadline is 60 seconds, including output enqueue.
Only trusted root execution can allocate requests: child execution is rejected
before allocation or event emission, regardless of spoofed arguments.

Reply with `external_tool_response`, `requestId`, and **exactly one** of:

* `result`: object containing a `content` array of native text/image blocks;
* `error`: string describing invocation failure;
* `cancelled:true`.

`cancelled` may only be present with the value `true`. Malformed outcomes are
rejected without retiring the waiter. A valid reply returns response data
`{"accepted":true}` once; duplicate, unknown and late valid replies return
`{"accepted":false}`. The RPC envelope `id` is independent of `requestId`.

Text requires a string `text`; image requires string base64 `data` and an
`image/*` `mimeType`. Unsupported content is rejected, never silently dropped.
Optional `isError` must be boolean; optional `structuredContent` must be an
object. Structured results are projected as explicit JSON text unless equivalent
JSON text already exists. `isError:true` is a native tool error, not success.
Unknown metadata, text signatures and annotations are not provider content.
Successful results retain the unmodified raw object in
`details.externalToolResult`. The existing AgentError API has no details channel:
error results preserve explicit content/structured data in the error message,
**not** unknown raw metadata. Nothing is silently truncated.

Native emits `external_tool_request_closed {requestId}` on completion, timeout,
cancellation or future drop. Normal returns finalize its synchronous enqueue
explicitly; RAII is the fallback for dropped futures. A full or closed queue
signals authoritative transport failure. Request retirement is synchronous and
does not depend on output delivery or detached tasks.

## Transport and ownership

Input is bounded while reading to 1 MiB per frame, including LF/CRLF; a final
unterminated EOF frame is supported. Invalid UTF-8 or oversize input fails the
transport and cancels the run. Blank lines, malformed-JSON error responses and
legacy `prompt` message/content/prompt aliases retain existing behavior.

The output queue has 64 slots. External requests alone have a 200 ms enqueue
deadline; ordinary healthy producers retain backpressure without that deadline.
Every awaited producer wakes on transport failure. Enqueue acceptance does not
promise OS delivery. Failed closure enqueue is authoritative failure too.

Abort/stop/failure signal the foreground run before retiring failed waiters.
The backend weakly references the foreground lane and awaits root cancellation
before returning an output failure, including failure to enqueue a closure. This
does not rely on the RPC owner being scheduled first or cancel only a tool's
child token. EOF disables the backend and resolves outstanding calls before joining. An accepted ordinary
healthy model-only prompt may finish after EOF, including runs longer than two
seconds: there is no shutdown watchdog or process-exit timer. Registry-retained
backends lose their writer on shutdown, including cancellation of the RPC owner
future. A drop guard cancels owned run/pumps and clears the commit publisher.

**Inherited platform limit:** an OS stdout write already blocked in the blocking
writer cannot be hard-preempted safely. Queue failure cancels async producers,
but writer join/runtime exit can still await OS write release. Tests use a
controllable writer and release it explicitly. This feature does not claim
bounded termination against a permanently blocked OS pipe.
