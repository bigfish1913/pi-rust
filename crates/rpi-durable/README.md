# rpi-durable

Native durable domain, runtime boundary and journal storage for the rpi host.
The Pi-facing `Session` abstraction and Agent Loop remain in `rpi-harness`; this
crate owns backend-neutral durable state plus the host-controlled persistence
boundary.

## Boundary

```text
rpi-harness::Session / Agent Loop
    -> rpi-durable::runtime / storage
        -> JSONL or optional SQLite backend
```

`domain` contains validation and state transitions only. `storage` preserves
opaque JSON journal frames, ordering and revision CAS; it does not interpret
`AgentMessage` or tool semantics. JSONL backend instances coordinate the head
check and append through a sidecar file lock; this does not provide fsync or
power-loss atomicity. The runtime API is grouped by authority:
`actor` owns trusted host identity, `scope` owns branch/revision snapshots,
`admission` owns mutation requests and validation, `receipt` owns committed
receipts, and `reconcile` exposes only fully validated journal views. These
modules do not add an external effect protocol or an exactly-once guarantee.
The harness is responsible for converting its session records into frames and
for exposing the Pi-compatible Session API. The adapter can also round-trip the
complete typed Session mutation classes (entry, record, lane and global facts)
without making those payloads part of this crate's domain model. The old typed
harness backends remain compatibility adapters while the migration is staged.

This crate does not execute/replay tools, start workers, decide terminal status,
or promise fsync, power-loss atomicity or exactly-once external side effects.
The host's transcript and durable execution records remain logically distinct,
even when they share the same durable writer/backend.

## Compatibility and scope

The legacy typed Session state format remains unchanged and is not automatically
migrated. The new durable journal stores one opaque `JournalFrame` per JSONL
line, or one row per frame in SQLite; its backend sequence is distinct from
Session mutation sequence. The host adapter preserves the existing typed frame
payloads and discriminator rules. Primary JSONL history still needs a full
initial scan; extracting a reducer alone does not make startup bounded.

This is a **native library**, distinct from the existing `rpi-package`
`rpi-durable` workflow plugin. This step neither loads both packages into one
workspace nor replaces the plugin. Before a future release/integration, the
native/library and plugin naming/version ownership must be reconciled explicitly.
The native crate participates in the workspace's version/dependency metadata so
that the published harness is not made dependent on a `publish = false` package.
No publishing is performed as part of this change.

See [session recovery design](../../docs/architecture/native-durable-session.md) and the
[Session boundary migration](../../docs/architecture/durable-session-boundary-migration.md)
for the implementation boundary, diagnostics, tests and remaining work.
