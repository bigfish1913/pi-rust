# Native RPC: message identity, inline images and client text I/O

These additive contracts belong to native `rpi --mode rpc`. Controllers must
inspect `ready` for the exact requested feature; a version string, plugin ABI
or adapter capability alone is insufficient. Legacy text prompts and local
built-in tools retain their existing path. This document describes source
contracts, not a passing build, installed-runtime upgrade or deployment.

## Message identity

The identity contract is gated by the exact advertisement
`ready.semantics.messageIds: "sessionEntryId-v1"`. Controllers must not assume
it from the existence of transcript IDs or `session_entry_committed` events.
An absent advertisement means legacy behavior, not permission to generate IDs.
Enabling the advertisement requires qualification of the native session/harness
paths, not only the JSON serializer.

Native session owners reserve an entry ID before publishing a message and use
that same ID when committing it. Message lifecycle events carry optional
`messageId` on `message_start`, assistant `message_update` and `message_end`,
including compact-delta and snapshot projection. Native message identity is
separate from provider response IDs, tool-call IDs, content-block indices and
RPC correlation IDs. Equal-content messages are not the same message. A live
reservation is not a promise that an unfinished message has already reached
durable storage. Direct library agents without a session owner may emit no ID.

Native history exposes the committed `entryId`. Tool-result messages can own
native message identity, but tool-execution events are still correlated by
`toolCallId`, not a fabricated message ID. Unrelated lifecycle events and
compaction summaries do not acquire message identity. The public Rust API
change is documented in [rpi-agent](../crates/rpi-agent/README.md#optional-native-message-identity),
including optional fields and the default-compatible `emit_injected` hook.

ACP maps this contract to optional stable-v1 ContentChunk `messageId` for live
assistant text/thinking and validated replayed user text/images and assistant
text/thinking. It does not add live user echoes or IDs to every tool/plan/config/
usage update. Core validates advertised assistant identity and final-text
reconciliation rather than assigning an adapter UUID or matching by text.

## Ordered inline image prompts

`ready.promptContent` advertises `contentBlocksVersion: 1`, `image: true`, exact
`imageMimeTypes: ["image/png", "image/jpeg"]` and the limits below. Example
wire shape (the data placeholder must be replaced with a valid image):

```json
{"id":"prompt-1","type":"prompt","contentBlocks":[{"type":"text","text":"before"},{"type":"image","mimeType":"image/png","data":"<canonical standard base64>"},{"type":"text","text":"after"}]}
```

Plain text blocks permit only `type` and `text`; image blocks only `type`,
`data` and `mimeType`. Blocks remain in their original order, including
image-only input. `contentBlocks` cannot coexist with legacy `message`,
`content` or `prompt` fields. The reserved `images` field is rejected even
when null; it is not a parallel image-input API. Legacy string input retains
its existing alias precedence and path.

| Advertised field | v1 limit |
| --- | --- |
| `maxContentBlocks` | 64 (at least one block) |
| `maxImages` | 4 |
| `maxImageBytes` | 262,144 decoded bytes per image |
| `maxImageBase64Bytes` | 349,528 bytes per image |
| `maxTotalImageBytes` | 524,288 decoded bytes |
| `maxTextBytes` | 32,768 aggregate UTF-8 bytes |
| `maxPromptBytes` | 786,432 bytes, normalized serialized full command plus LF |
| `maxImageSide` | 2000 pixels per side |
| `maxImagePixels` | 4,000,000 |
| `imageDecodeAllocBudgetBytes` | 33,554,432, best-effort decoder allocation budget |
| `frameBytes` | 1,048,576 native inbound frame bytes |

Only nonempty canonical standard base64 is accepted, with exact declared
PNG/JPEG MIME/signature agreement and successful pixel decoding. No data URL,
URL-safe/whitespace base64, APNG, JPEG-XT, arbitrary format, provider signature
or image URI/path is accepted by the native block schema. Images requiring
orientation transforms are rejected; clients must normalize them first. This
path does not silently transform, resize, re-encode or discard images.
The locked decoder's 32 MiB accounting is **best effort**, not a hard process
heap/RSS ceiling: JPEG scratch and other allocations are not fully accounted.
Structural validation in adapters/core is not pixel validation.

The production dispatcher owns/offloads pixel preflight before model/run
admission, and rejects an image prompt for a selected model without image input.
Validation is tracked as active work: concurrent prompt/configuration mutations
are not admitted through an apparently idle gap. Cancellation/shutdown retain
the worker cleanup owner; synchronous decoder work is not hard-preemptible.
Prompt acceptance and `agent_settled` still delimit the native run.

Extcore requires the exact compatible v1 profile before writing an ordered
prompt, validates the final correlated envelope including escaping and LF, and
never falls back to text or strips images on an old host. ACP advertises its
adapter image support at initialize; this does not bypass per-session native
admission. Optional ACP image URI metadata is inert and never fetched/opened.
Text/resource-only ACP prompts retain their prior conversion and legacy path.
Native owns persisted image content; replay preserves ordered user images.

## Client-authoritative text files

A trusted controller may configure the existing built-in `read`, `write` and
`edit` tools using native `ready.fileIO` v1 plus the advertised commands
`configure_file_io`, `file_io_response` and events `file_io_request`,
`file_io_request_closed`. This is not MCP, a replacement tool registry, client
terminal execution or an OS sandbox. Configuration is startup/idle work;
configuration during a run or prompt validation fails. An embedded harness
without an attached built-in `FileIORouter` explicitly rejects configuration.

```json
{"id":"configure-1","type":"configure_file_io","readText":true,"writeText":true}
```

Both flags are required booleans in native configuration. They select authority
independently:

| readText | writeText | `read` | `write` | `edit` |
| --- | --- | --- | --- | --- |
| false | false | Original host implementation | Original host implementation | Original host implementation |
| true | false | Client text | Rejected | Rejected |
| false | true | Original host implementation | Client text | Rejected |
| true | true | Client text | Client text | Client complete read + write |

The native owner replaces/closes its prior broker only while idle. Extcore's
session API deliberately freezes its installed capabilities and handler identity;
changing them requires close/reopen. ACP independently negotiates boolean
`clientCapabilities.fs.readTextFile` / `writeTextFile`, configures during
new/load/resume before tools execute, and rejects nonempty selection on an old
host. Reconnection negotiates again; transcripts do not configure authority.

### Wire exchange

Requests contain a native-minted `requestId` (canonical `native:<UUID>`), an
absolute lexical `path`, operation and bounded payload. Example ranged read:

```json
{"type":"file_io_request","requestId":"native:00000000-0000-4000-8000-000000000001","operation":"read_text_file","path":"/workspace/source.txt","line":1,"limit":100,"maxBytes":262144}
{"type":"file_io_response","requestId":"native:00000000-0000-4000-8000-000000000001","result":{"content":"raw text\n"}}
```

`line` is one-based and `limit` positive; apply the requested range exactly once
and return unnumbered text. Without line/limit, read means complete content,
as needed by edit: fail on overflow, do not truncate. The write operation is
`write_text_file` with `content`; native success is `result: {}`. A failure
instead carries `error: {"code":"denied","message":"reason"}`. The supported
codes are `unsupported`, `denied`, `not_found`, `invalid_response`, `too_large`,
`timeout`, `cancelled`, `disconnected` and `invalid_path`. Error messages are
limited to 4096 UTF-8 bytes. Native returns whether a correlated response was
accepted; unknown/late/duplicate IDs do not resolve another request. Published
requests retire with `file_io_request_closed`; owner EOF/shutdown cancels pending
work and releases its writer/backend ownership.

ACP maps these operations to `fs/read_text_file` and `fs/write_text_file`, adding
its session ID; ACP response envelopes are not the native envelopes above.
The ACP write mapping accepts the standard empty object (optional `_meta`) and
also tolerates null acknowledgements, but never treats a JSON-RPC error as success.

### Authority, bounds and side effects

Each tool freezes authority before resolving its source path. Selected client
operations use lexical resolution against native cwd, not host canonicalization,
metadata probes, source-file reads/writes or local parent-directory creation.
The client is authoritative even if its buffer differs from the host disk.
Failure, denial, malformed response, transport loss and image-file read requests
never cause selected client text operations to fall back to local source I/O.
Native still owns permission-before-execution, edit matching and diff/tool events.
Permission denial must prevent execution, not execute and hide the result.

Delegation does not cover native startup configuration, transcript storage,
shell commands, arbitrary extensions or unrelated tools; write-only intentionally
leaves read local. Do not describe it as a filesystem/OS sandbox or all client
files as image-capable. Native serializes write/edit per router and lexical path,
not symlink identity, external editors or version/CAS. A dispatched write may
already have committed when cancellation, timeout or disconnect loses its
acknowledgement. There is no rollback, automatic retry or side-effect replay.

`ready.fileIO` v1 advertises `maxPending: 64`, `maxTextBytes: 262144`,
`maxPayloadBytes: 786432`, `timeoutMs: 60000`. The 256 KiB UTF-8 content bound is
independent of the 768 KiB encoded JSON bound; escaping can exhaust JSON first.
Native inbound framing is separately 1 MiB. Core handlers have 50 seconds from
admission including queue wait, with a separate 5-second native response-dispatch
budget; ACP client operations have 45 seconds including frame-write wait.
Core uses a bounded reliable reader queue and retires work on native closure,
settlement, cancellation or shutdown. These layered cooperative deadlines do not
promise a hard global shutdown bound, preempt synchronous OS/decoder calls or
undo a remote write.

## Validation and evidence boundary

Existing native workspace CI's `cargo test --workspace --locked` selects added
CLI production-dispatch, prompt/broker/image, tools and harness identity tests.
The companion package's existing six-package Cargo test command includes core
identity/prompt/file-I/O and ACP replay/mapping fixtures, alongside its existing
direct MCP core/stdio/HTTP bridge checks. No new CI architecture is required.

The package-side authored Windows proof is
`packages/rpi-acp/tests/message_image_fs_native.py`, not a native-repository
script. From the package root, use `python -B <that-path> --help`. It requires
explicit verified native/bridge binaries, SHA-256 arguments, a parent-created
complete prebuild source inventory binding both subsequent successful builds,
and a new evidence directory under
`C:/Users/bigfish/.cargo/target/acp-stage12-01`. It does not build/discover an
installed binary. Parent build records are attestations, not cryptographic
reproducible-build proof.

The harness is authored infrastructure, not a claimed execution result. Review
retained results, source/binary provenance, provider/wire/native transcript and
cleanup audits before asserting an actual pass. Its local model/native proof
covers ordered PNG/JPEG payloads, live/history IDs and client-buffer/disk
divergence, not thought streaming, old-native execution, permission denial,
cancellation/late replies, Unix native, installed extensions, CI, PR publication
or deployment. Separate fixtures/proofs cover those additional behaviors.
