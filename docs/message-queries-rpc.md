# Optional paginated transcript metadata and context RPC

Discover `get_message_summaries` and `get_message_window` in the ready frame's
`supportedCommands`. They are additive; `get_messages` and its existing clients
are unchanged. Neither command runs the model, tools, or navigates the session.

## User-only summaries

```json
{"type":"get_message_summaries","id":"q1","role":"user","limit":10}
```

The response data contains `filtered:true`, `role:"user"`, `messages` in branch
order (oldest first), `firstId`, `lastId`, `nextBeforeId`, `hasMore`, and `leafId`.
Each summary contains `entryId`, `role`, `preview`, the native user `timestamp`
(milliseconds), and `hasImages`. Previews normalize whitespace, inspect at most
2048 text characters and return at most 140 Unicode scalar values. Images are
reported without copying their encoded data. Only `role=user` is accepted.

Pass `beforeId:nextBeforeId` and the returned `leafId` to get the next older
page. `beforeId` is exclusive and must identify a user message on this snapshot
branch. Defaults: 10 summaries; accepted limits: 1..100. Empty results have null
edge IDs; the end of history has `hasMore:false,nextBeforeId:null`.

A rebuildable, in-memory nearest-user ancestor index is maintained on append and
replayed/rebuilt by JSONL and SQLite. Summary selection bypasses tool payloads;
it does not materialize and serialize mixed history and then filter it.

## Bounded nearby message content

```json
{"type":"get_message_window","id":"w1","anchorId":"USER_ENTRY_ID","leafId":"SNAPSHOT_LEAF","limit":48}
```

Exactly one of `anchorId`, `beforeId`, or `afterId` is required. Defaults: 48
messages, maximum 100. An anchor window contains its message, preferring one
third of the window before it and filling unused space at branch boundaries.
Before/after cursors are exclusive. Messages and compaction boundary markers use
the existing `get_messages` encoding, including stable `entryId`.

Response data: `messages`, `firstId`, `lastId`, `nextBeforeId`, `nextAfterId`,
`hasMoreBefore`, `hasMoreAfter`, and `leafId`. Use the indicated cursor and same
leaf for either direction. The cursor itself must belong to this branch.
The implementation retains bounded neighbour indices and clones only selected
entries. Membership can still require walking parent metadata from the leaf to
an old cursor; this is not a claim of constant-time arbitrary-position seeking.
Bounds are message counts, not a guarantee that one large tool result is small.

## Snapshot and compatibility contract

Omitting `leafId` selects the current branch leaf; supply the returned leaf on
subsequent pages. Appends after a pinned leaf are excluded from that snapshot.
Rewinding or switching to a sibling that no longer contains the pinned leaf
returns an invalid query containing `branch_changed:`; discard accumulated pages
and fetch a fresh snapshot. Invalid/foreign cursors never silently fall back to
the latest page. Supplemental metadata does not replace native transcripts.
External SessionStorage/SessionTree implementations default to an unsupported
query error and are not forced to scan their history to implement this feature.

## Local synthetic measurement (not production latency)

Run `cargo test --release -p rpi-harness --lib
benchmark_borrowed_user_index_against_full_projection -- --ignored --nocapture`.
The fixture has 100 user messages and 10,000 assistant payload entries. On one
Windows release-mode run the complete entry clone/projection baseline was
64.47 ms / 22,280,144 bytes, versus 43.2 microseconds / 1,100 bytes for the latest
10 user summaries. The baseline serializes complete entries (not exactly the old
RPC envelope). This isolates projection/payload work; it does not measure native
startup, disk replay, HTTP latency or a deployed Web runtime.
