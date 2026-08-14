# M4 (pi-tools) open questions & divergences

> Written per the user's instruction "中间有问题，写文档，我睡醒统一处理"
> (if problems arise mid-way, write them to a doc; I'll review them on waking).
> All issues below were **resolved** during M4 but are recorded here so the
> divergences from the TS reference are explicit and reviewable.

## 1. bash `on_chunk` uses `try_lock`, not `blocking_lock`

**Where:** `crates/pi-tools/src/tools/bash.rs` (on_chunk closure passed to
`execute_shell_with_capture`).

**The problem.** The TS `bash.ts` throttles stdout/stderr chunks with a plain
closure that mutates a `ThrottleState`. The direct Rust port captured that state
behind a `tokio::sync::Mutex` and called `blocking_lock()` inside `on_chunk`.
This panics at runtime:

```
thread 'tokio-runtime-worker' panicked at:
Cannot block the current thread from within a runtime
```

**Why.** `ExecutionEnv::exec` fires `on_stdout`/`on_stderr` **synchronously on a
runtime worker thread** (the env's exec future runs on the executor; its
callback invocations are just function calls on that same worker). `tokio::sync`
mutexes forbid `blocking_lock` on a runtime worker — it's a forced context
switch that the executor cannot satisfy.

**The fix.** `on_chunk` now uses `try_lock()`:

```rust
let mut st = match throttle_for_cb.try_lock() {
    Ok(g) => g,
    Err(_) => return, // busy — skip; the final post-capture flush covers it
};
```

The throttle invariant is still preserved because **the final post-capture flush
always emits the terminal state** (last output, truncation, full-output path)
after `execute_shell_with_capture` returns, in an async context where `.lock().await` is legal. Mid-stream flushes are best-effort; a skipped flush just
means the next successful `try_lock` (or the final flush) carries the newer
state forward.

**Divergence from TS.** TS calls run on the JS event loop and never contend, so
every `on_chunk` flushes. The Rust port can drop a mid-stream flush under
contention, but the throttle's purpose — coalescing into ≤1 update per 100ms
window — is satisfied either way, and no terminal state is lost. The
`bash_throttle.rs` test asserts the TS-equivalent bound (`n < 25`, `n >= 1`).

**Open question for review:** is `try_lock`-with-final-flush the right shape,
or should the throttle state move off a `tokio::Mutex` entirely (e.g. an
`AtomicUsize`-driven ring, or a dedicated `std::thread`-owned aggregator)? The
current approach is the smallest sound change; I deferred a fancier design.

## 2. Windows path-absoluteness in tests

**Where:** `crates/pi-tools/tests/{execution_env_conformance,read_truncation,...}.rs`.

**The problem.** Tests that seeded `InMemoryExecutionEnv::with_cwd("/tmp/work")`
failed on Windows because `Path::is_absolute()` requires a drive letter —
`/tmp/work` is **not** absolute on Windows. `absolute_path` then produced keys
like `/tmp/work\test.txt` (mixed separators) and the `is_absolute()` assertion
failed.

**The fix.** Two complementary rules adopted across the test suite:

1. **Conformance tests** use an OS-absolute root:
   `std::env::temp_dir().join("pi-tools-im-conf")` — has a drive letter on
   Windows, `/tmp/...` on POSIX.
2. **InMemory-seeded tests** never seed by a raw literal path. Instead a `seed()`
   helper computes the storage key the tool will resolve to via
   `env.absolute_path(rel)` first, then writes there. This sidesteps the
   separator-mismatch entirely — the env's own `join_path` decides the key.

**Divergence from TS.** The TS reference uses `/tmp/work` string literals
uniformly because Node's `path.isAbsolute` accepts POSIX roots cross-platform
(Node normalizes). The Rust port cannot, so tests are written OS-agnostically.
No production-code divergence — `OsExecutionEnv`/`InMemoryExecutionEnv` behave
correctly on both platforms; only test fixtures needed adjustment.

## 3. `shell_output` spill timing (intentional v1 divergence, pre-dated this session)

**Where:** `crates/pi-tools/src/shell_output.rs`.

Already documented in the plan (§5.4 area): the TS `shell-output.ts` spills to
the temp file **mid-stream** from a synchronous `FnMut` callback; Rust spills
**post-stream** because a sync `FnMut` cannot safely drive async FS writes on a
runtime worker thread (same root cause as issue 1). The post-stream spill is
sound and loses no data — the rolling tail buffer holds the full output until
capture completes. Recorded here for completeness; not a new finding.

## 4. No `docs/m4-open-questions.md` test references left dangling

The `crates/pi-tools/tests/bash.rs` module doc references this file. It now
exists. No action needed beyond keeping this doc in tree.

---

## 5. `grep`/`find` ported in-process (no `rg`/`fd`); `ls` pure fs — RESOLVED

The M4 pi-tools scope shipped `read`/`write`/`edit`/`bash` only. The read-only
`grep`/`find`/`ls` trio is now ported (separate follow-on to M6, not a
milestone). Sources:
`crates/pi-tools/src/tools/{grep,find,ls}.rs`, tests
`crates/pi-tools/tests/{grep,find,ls}.rs` (23 tests, all green against
`InMemoryExecutionEnv`).

**Divergence from TS.** The TS `grep` shells out to `rg` (ripgrep, JSON-stream
parsing, gitignore-aware) and `find` to `fd` (`--full-path` rewrites,
gitignore-aware walking, optional auto-download of the binary). `ls` is already
pure `FileSystem`. The Rust port implements **all three in-process** through the
`FileSystem` trait + the `regex`/`globset` crates. Rationale: `read`/`write`/
`edit` already go through `FileSystem` (so they run against both
`OsExecutionEnv` and `InMemoryExecutionEnv`); shelling out to `rg`/`fd` would
bypass the trait — it would only work on the real OS fs and could not be tested
with `InMemoryExecutionEnv`, breaking the abstraction the rest of `pi-tools` is
built on. The in-process port works against *any* `ExecutionEnv`.

**Trade-offs (v1).** No full `.gitignore` awareness (only `.git/` directories
are skipped; revisit via the [`ignore`](https://docs.rs/ignore) crate for an
`OsExecutionEnv`-only fast path if parsing performance demands it). No external
binary, no auto-download. Traversal order is a deterministic sorted BFS rather
than rg/fd's walk order. `grep`'s output shape, match/context line formats,
per-line (`GREP_MAX_LINE_LENGTH`=500) + byte (`DEFAULT_MAX_BYTES`=50KB)
truncation, and match-limit semantics match TS exactly. `find` ports the
`/**/`-prepend rewrite for path-containing patterns and `relativizeFindResultPath`;
the Windows `[/\\]` separator rewrite is unnecessary (paths are posix-normalized
internally). `ls` matches TS exactly (the only folding: `list_dir` already
returns `FileInfo` with a `kind`, so the per-entry stat is folded into the
listing — no second round-trip).

**Cwd resolution.** Porting these uncovered that
`InMemoryExecutionEnv::resolve` preserved `.`/`..` path components (so
`resolve_read_tool_path(".")` → `/tmp/work/.`, missing the BTreeMap key), while
`OsExecutionEnv::normalize_absolute` collapses them. `resolve` now collapses
components to match the OS env, and `with_cwd` pre-registers the cwd + ancestors
as directories (the cwd always exists on a real fs). See also
`docs/m6-cli-open-questions.md` §9.
