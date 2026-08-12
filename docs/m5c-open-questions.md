# M5c (pi-harness JSONL) open questions & divergences

> Written per the user's instruction "中间有问题，写文档，我睡醒统一处理"
> (if problems arise mid-way, write them to a doc; I'll review them on waking).
> These are the design divergences from the TS reference that the M5c JSONL
> backend introduced. The in-memory FS normalization (§2) was a real bug
> uncovered during testing; the cwd-carrying split (§1) is a deliberate
> structural adaptation. Both are resolved and recorded for review.

## 1. The `cwd`-carrying split: inherent typed methods vs. the `SessionRepo` trait

**Where:** `crates/pi-harness/src/session/jsonl/repo.rs`,
`crates/pi-harness/src/session/jsonl/types.rs`,
`crates/pi-harness/src/session/types.rs`.

**The problem.** The TS `JsonlSessionRepo` takes cwd-load-bearing options on
every call — `create(options: JsonlSessionCreateOptions & { cwd })`,
`list(options: JsonlSessionListOptions = { cwd? })`, `fork(source, options:
ForkOptions & JsonlSessionCreateOptions)`. The on-disk layout is
**cwd-encoded**: the directory under the sessions root is `--<cwd-encoded>--`
and the leaf file name embeds an ISO timestamp + id. So `cwd` is not optional
flavor — it selects *which directory on disk* a session lives in.

The crate-wide Rust [`SessionRepo`] trait, however, was designed to be shared by
the in-memory and JSONL backends and by the `Session`/`AgentHarness` facade. Its
signatures carry no `cwd`:

```rust
async fn create(&self, options: &SessionCreateOptions) -> SessionResult<...>;
async fn list(&self) -> SessionResult<Vec<SessionMetadata>>;
async fn fork(&self, source: &SessionMetadata, options: &SessionCreateOptions,
              fork: &ForkOptions) -> SessionResult<...>;
```

`SessionCreateOptions` carries only `{ id, parent_session_id, metadata }` — no
cwd. So the type system cannot carry cwd through the trait the way the TS repo
expects, yet the JSONL backend *must* have it.

**The resolution.** A split API, mirroring the TS signatures faithfully on the
inherent side and delegating on the trait side:

1. **Inherent typed methods — the primary entry points** (direct ports of the TS
   signatures, `JsonlSessionCreateOptions` / `JsonlSessionListOptions` carry
   `cwd` non-optionally):
   - [`JsonlSessionRepo::create_typed(&JsonlSessionCreateOptions)`]
   - [`JsonlSessionRepo::list_typed(&JsonlSessionListOptions)`]
   - [`JsonlSessionRepo::fork_typed(&JsonlSessionMetadata, &JsonlSessionCreateOptions, &ForkOptions)`]
   - [`JsonlSessionRepo::open_by_jsonl_metadata(&JsonlSessionMetadata)`] —
     TS's preferred open path; it already holds the on-disk `path`, no scan.
   - [`JsonlSessionRepo::delete_by_jsonl_metadata(&JsonlSessionMetadata)`] —
     direct-path remove, the TS fast path.

2. **`SessionRepo` trait impl — delegates using `default_cwd`** (the repo is
   constructed with a default cwd, derived from the env's cwd via
   `with_env_cwd`). `create`/`list`/`fork` forward to the typed methods with
   `default_cwd` filled in. `open`/`delete`/`fork` (which take the base
   `SessionMetadata`, not the rich `JsonlSessionMetadata`) **scan `list_typed`
   to find the JSONL metadata by id** — they cannot use the direct-path fast
   path the TS `open`/`delete` have via `metadata.path`, because the trait only
   hands them the id.

**Consequence.** Callers that hold a `JsonlSessionMetadata` (e.g. the harness
after its first `list`) should prefer `open_by_jsonl_metadata` /
`delete_by_jsonl_metadata` to avoid the scan. Callers going through the trait
(the `Session` facade) pay a directory scan on `open`/`delete`/`fork`. This is
correctness-preserving (the scan finds the right file) at the cost of an O(n)
list per trait-level open — acceptable for v1 since the harness mostly operates
on `JsonlSessionMetadata` it already has.

**Divergence from TS.** TS has one `JsonlSessionRepo` class whose every method
takes the full typed options; it does not implement a sharing trait. The Rust
port adds the trait-delegation layer to keep the in-memory + JSONL backends
behind one `SessionRepo` the facade consumes. The typed methods are a 1:1 port;
the trait impl is the adaptation.

**Open question for review:** should the shared `SessionRepo` trait grow
optional `cwd`/`metadata` plumbing (an options struct the JSONL impl reads and
the in-memory impl ignores), eliminating the trait-level scan? I deferred this
to avoid a cross-crate signature churn in M5c; the typed API already gives a
zero-scan path for callers that want it.

## 2. In-memory FS forward-slash normalization (cross-platform fix)

**Where:** `crates/pi-tools/src/in_memory.rs`
(`InMemoryExecutionEnv::norm` / `resolve_key`, and the rewritten
`absolute_path` / `join_path`), with blast radius across all M5c repo tests.

**The problem.** The in-memory env keys its `BTreeMap` by path string and
`list_dir`/`remove` do **prefix matching** with `/`. `PathBuf::push`/`join`
insert the OS path separator, which is `\` on Windows. So a session created at
cwd `/proj` ended up written under a key like
`/sessions\--proj--\2023-..._sess-1.jsonl` on Windows, while `list_dir`
prefixed its scan with `/sessions/--proj--/` (forward slashes, as the test
fixtures and the repo's `join_path` inputs use them). The prefix never matched,
`list_typed` returned empty, and 8 of the 11 repo tests failed **only on
Windows**.

Compounding it: on Windows, `/sessions` and `/proj` are *not*
`Path::is_absolute()` (only drive-rooted paths are absolute). The env's
`resolve` branches on `is_absolute()`, so a "relative" `/proj` got joined onto
the cwd `/` producing `/\proj`-style keys — also breaking the prefix match.

**The fix.** Normalize *all* internal keys to forward slashes:

- `norm(s) = s.replace('\\', "/")` — applied to every key before insert/lookup.
- `resolve_key(cwd, path) = norm(resolve(cwd, path).to_string_lossy())` — the
  single key-forming helper every storage method now calls.
- `absolute_path` and `join_path` build their result with string concatenation
  on `/` (no `PathBuf::push`), then `norm` it, so returned `PathBuf`s are
  forward-slash and consistent with the BTreeMap keys on every platform.

This is a **test double**, so canonicalizing to `/` is safe: real OS paths are
never round-tripped through it, and test fixtures are POSIX-style by
convention. `OsExecutionEnv` is untouched and continues to use real `PathBuf`
semantics.

**Divergence from TS.** The TS harness has no in-memory env — it tests against
`NodeExecutionEnv` + `mkdtempSync`, where Node's `path` module normalizes
separators cross-platform. The Rust port's `InMemoryExecutionEnv` needed the
equivalent normalization to keep its string-keyed `BTreeMap` coherent on
Windows. No production-code divergence for `OsExecutionEnv`; only the test
double changed.

**Why it matters beyond tests.** The `InMemoryExecutionEnv` is the primary
backend for every M5 unit + integration test (session state, JSONL codec,
torn-tail, atomic publish, and the upcoming compaction + harness e2e tests).
Without this fix, none of the JSONL repo tests pass on Windows, which is the
development platform here.

## 3. Torn-tail `publish_file_atomically` cleanup on rename failure

**Where:** `crates/pi-harness/src/session/jsonl/storage.rs`
(`publish_file_atomically`).

**The problem.** The original port cleaned up the `.tmp` only on a *staging*
failure (the `populate` future returning `Err`), then ran the rename
unconditionally. A rename failure left the staged `.tmp` on disk — contradicting
the TS invariant ("no `.tmp` left behind") and the `m5c_jsonl_atomic_publish`
test `fork_rename_failure_leaves_no_destination_and_no_tmp`.

**The fix.** Restructured to mirror the TS `try { populate; rename } catch {
remove(tempPath) }` shape exactly: both staging and rename run inside one
`async` block; on *any* `Err` (staging OR rename), the `.tmp` is best-effort
removed before propagating the original error. The destination is untouched
until the rename commits, so a failure at either stage leaves the pre-op state
intact and no residue.

**Divergence from TS.** None — this now matches `publishFileAtomically` byte for
byte in behavior. Recorded because it was a latent correctness gap the atomic-
publish tests surfaced.
