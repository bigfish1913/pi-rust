# M6 (pi-cli) open questions & divergences

> Written per the user's instruction "中间有问题，写文档，我睡醒统一处理"
> (if problems arise mid-way, write them to a doc; I'll review them on waking).
> These are the design divergences from the TS reference that the pi-cli port
> (`crates/pi-cli/`) introduced. None are blockers — all are recorded for
> review. The CLI builds, its 40 unit tests pass, the full workspace test
> suite stays green, and the `pi` binary runs end-to-end (version/help/usage
> errors + a real Anthropic-backed run fails cleanly at the network/auth layer
> with a correct exit code); the items below are deliberate v1 simplifications,
> not defects.

## Context recap

The TS CLI lives in `packages/coding-agent` (`src/cli.ts` → `main.ts` →
`cli/args.ts` → `modes/{print-mode,json-event,rpc-mode,interactive/*}.ts`). It
is a large, full-featured product with an Ink/React TUI, an extension system,
a package manager, OAuth/Copilot auth, HTML export, skills/prompt-template/theme
discovery, model cycling, and project-trust prompts. The Rust `pi-cli` is a
**deliberately minimal-but-real CLI** that exercises the `pi-harness`
`AgentHarness` end-to-end: parse args → resolve an Anthropic model → build a
harness over `OsExecutionEnv` + a JSONL session → drive `prompt_text` → print
the outcome. Everything else is deferred here.

The section names mirror the library divergence docs (M4/M5c/M5e/M5f) so this
slots into the same review cadence.

---

## 1. CLI surface is a strict subset; ignored flags warn, not error

**Where:** `crates/pi-cli/src/args.rs::parse_args` — the
`recognized-but-ignored v1 scope cuts` arms.

**What.** The TS `parseArgs` returns an `unknownFlags` map that extensions
claim later. v1 has no extension system, so unknown/deferred flags are folded
into `Args::ignored` and surfaced as warnings (on `--verbose`) rather than hard
errors. This lets users keep muscle-memory flags (`--models`, `--tui-mode`,
`--export`, `--list-models`, `--fork`, `--offline`, `--approve`/`-na`, the
`--no-*` discovery toggles, `--extension`/`-e`, `--skill`, `--prompt-template`,
`--theme`) without a hard error.

**Divergence.** TS silently stores these for extensions; v1 warns. Unknown
**short** flags (`-Z`) still hard-error (matches TS), and unknown **long** flags
warn (matches the TS unknown-flag heuristic, minus the extension claim).

**To revisit.** When extensions/skills/templates/themes land, route the
matching flags to those systems instead of warning. Until then the warning is
the honest signal.

---

## 2. `--mode rpc` is parsed but not implemented

**Where:** `crates/pi-cli/src/app.rs::run` — the `RunMode::Rpc` arm prints an
error and exits 2 (usage).

**What.** TS `runRpcMode` drives a JSON-RPC session protocol over stdio (the
remote-session surface that `packages/client/` consumes as a transport-neutral
CBOR client). v1 has no RPC server. `--mode rpc` is accepted by the parser so
it doesn't hard-error, then rejected at dispatch with a clear message.

**To revisit.** Port the RPC server (or wire `pi-harness` sessions to a CBOR
transport) before enabling this. The `packages/client/` CBOR `PiClient` is the
matching client side and was intentionally **not** ported (it is transport
plumbing, not part of the SDK library layer this port targets).

---

## 3. Interactive mode is a minimal line REPL, not the TS TUI

**Where:** `crates/pi-cli/src/modes.rs::interactive`.

**What.** The TS `modes/interactive/*` is a full terminal UI built on
Ink/React: theme support, keyboard protocols, a rich prompt surface, command
palette, slash commands, scrolling transcript, model cycling, queued-message
indicators. v1 ships a line-oriented `read_line` REPL: read a line, run it via
`lane.prompt_text`, print the assistant text, loop to EOF/`/exit`. Only `/exit`
(`/quit`) and `/abort` are recognized.

**Divergence.** No theming, no TUI rendering, no command palette, no
scrollback. The `--tui-mode` flag is parsed-and-ignored (§1).

**To revisit.** A Rust TUI (ratatui) is a large follow-on. The library APIs
the TUI would need (lane watch, queue peek/steer/follow-up, session tree
navigation) already exist on `AgentLane`/`AgentHarness`; the gap is purely UI.

---

## 4. Anthropic-only; no OAuth/Copilot; API key required up-front

**Where:** `crates/pi-cli/src/provider.rs::resolve`.

**What.** v1 is Anthropic-only (plan §5.16: "OAuth/Copilot skipped v1; API-key
auth only"). `--provider` must be `anthropic` (or absent); any other value is a
hard `UnknownProvider` error. The API key is resolved `--api-key` →
`ANTHROPIC_API_KEY`; if neither is set, `resolve` returns `NoApiKey` and the
CLI exits 2 with guidance before any network call.

**Divergence.** TS `ModelRuntime`/`ModelRegistry` resolves multiple providers
and auth strategies (API key, OAuth token, Copilot seat). v1 builds a single
`AnthropicProvider` from the key. The `models` catalog is the fixed
`pi_ai::providers::anthropic::models::anthropic_models()` set.

**To revisit.** OpenAI/Google/Bedrock providers have a `Provider` trait seam in
`pi-ai` already; wiring them needs a resolver + their catalogs. OAuth/Copilot
needs an auth store + token refresh — not started.

---

## 5. Model matching is exact (case-insensitive), not fuzzy

**Where:** `crates/pi-cli/src/provider.rs::find_model` + `split_model_pattern`.

**What.** `--model` accepts `provider/id[:thinking]` or `id[:thinking]` (a
trailing `:level` is peeled only if it is a valid thinking level, else kept in
the id). The id is matched **exactly, case-insensitively** against the catalog.
TS `resolveCliModel` additionally does fuzzy/partial matching.

**Divergence.** v1 drops fuzzy/partial match deliberately: partial match is a
common source of "got the wrong model" surprises. A typo now yields a clear
`NoMatch { pattern, available }` error listing the full catalog.

**To revisit.** If users want shorthand (`sonnet` → `claude-sonnet-5`), add an
opt-in `--fuzzy-model` flag rather than changing the default.

---

## 6. Session restore (`-c`/`-r`/`--session`) is recognized but not wired

**Where:** `crates/pi-cli/src/session.rs::select_session` + `build_session`
(the `SessionSelection::Existing` arm → `BuildError::RestoreNotImplemented`).

**What.** `-c`/`--continue`, `-r`/`--resume`, and `--session <id|path>` are
parsed (so they don't hard-error) and routed to `SessionSelection::Existing`.
`build` then returns `RestoreNotImplemented { requested, flag }` with guidance
to drop the flag and start fresh. This is the direct consequence of M5f
divergence #3: `AgentHarness::create` rejects sessions that already have
records (restore is not implemented in the harness).

**Divergence.** TS `SessionManager` resolves continue/resume/specific-session
into an existing JSONL file and the harness replays it. v1 always creates a
*fresh* session (ephemeral via `--no-session`, or a new JSONL file under
`--session-dir`/the default `<cwd>/.pi/sessions`).

**To revisit.** Depends on harness restore (M5f #1/#3). Once `AgentHarness` can
rehydrate a session from an existing JSONL file, wire `-c` (most-recent in the
cwd's session dir), `-r` (a picker — needs a TUI), and `--session` (id/path
resolution).

**Note.** The v1 default session dir is `<cwd>/.pi/sessions` (TS uses
`<agentDir>/sessions` under the home dir). This is a documented divergence so
sessions live *with the project* rather than globally; revisit if a global
location is preferred.

---

## 7. `@file` attachments: text-only; images refused

**Where:** `crates/pi-cli/src/app.rs::process_file_args`.

**What.** The TS `processFileArguments` has two branches: text files are
wrapped in `<file name="…">…</file>`; image files are mime-detected, resized,
base64-encoded into `ImageContent` and attached to the prompt. v1 ports **only
the text branch**. Image extensions (`png`/`jpg`/`jpeg`/`gif`/`webp`/`bmp`)
return an error ("image attachments are not supported in v1") rather than
mis-parsing binary as text.

**Divergence.** `pi-tools` ships an image *detector* (`detect_supported_image_mime_type`)
but no CLI-facing image processor (resize/encode pipeline), and the v1 `modes`
do not forward `Vec<ImageContent>` into `prompt_text` (the lane accepts images,
but the CLI never builds them). Binary/non-UTF-8 text files error on
`read_to_string`.

**To revisit.** Wire `pi_tools::image::encode_base64` + a resize step into the
`@file` path and pass collected `ImageContent` to `prompt_text`. The lane
already accepts `Vec<ImageContent>`; this is purely CLI-side wiring.

---

## 8. System prompt is a condensed default; no skills/templates/resources

**Where:** `crates/pi-cli/src/session.rs::default_system_prompt` + the
`AgentHarnessResources::empty()` in `build`.

**What.** TS composes the system prompt from a base prompt + discovered
skills (`<available_skills>`) + context files + extension contributions. v1
uses a **condensed** port of the TS base prompt (role + tools + guidelines +
cwd), with no skills/templates/context-file machinery. `--system-prompt`
replaces it; `--append-system-prompt` (repeatable, text-or-file) appends.

**Divergence.** The library APIs for skills (`pi_harness::skills`),
prompt-templates (`pi_harness::prompt_templates`), and the system-prompt
composer (`compose_system_prompt`) **already exist and are tested** — the CLI
just doesn't discover `.md` files from disk into `AgentHarnessResources`. So
the gap is discovery + wiring, not capability.

**To revisit.** Port the TS resource discovery (`ResourceLoader`):
`.pi/skills/**`, `.pi/prompts/**`, context files. Feed them into
`AgentHarnessResources` and let `compose_system_prompt` do the composition.

---

## 9. Read-only tools `grep`/`find`/`ls` are absent (not yet ported to pi-tools)

**Where:** `crates/pi-cli/src/session.rs::BUILTIN_TOOL_NAMES` + the help text's
"read-only tools grep/find/ls are not in v1" note.

**What.** v1 ships `read`/`bash`/`edit`/`write` (the M4 pi-tools set). The TS
`createCodingTools` also registers `grep`/`find`/`ls`; those were **not** ported
in M4 (scope was read/write/edit/bash + `ExecutionEnv`). The CLI help mentions
the gap so `--tools grep` doesn't silently no-op — it just produces a tool that
isn't there (the allowlist retains only existing names, so `grep` is simply
absent from the active set).

**To revisit.** Port `grep`/`find`/`ls` to `pi-tools` (they are read-only
`ExecutionEnv` consumers, lower-risk than the mutating tools), then add them to
`BUILTIN_TOOL_NAMES`. This unblocks genuinely useful read-only coding-agent
runs.

---

## 10. JSON event stream is a lossy but stable shape

**Where:** `crates/pi-cli/src/modes.rs::emit_json_event`.

**What.** `--mode json` emits one JSON object per line per harness event
(`run_start`/`run_end`) plus a terminal `result` line. The TS `toJsonEvent`
projects a richer event set (stream deltas, tool events, message updates); v1
only sees `HarnessEvent` (`RunStart`/`RunEnd`), so its JSON stream is coarser.

**Divergence.** The harness `HarnessEventBus` currently surfaces only run
lifecycle events to `pi-cli` (the fine-grained `AgentEvent` stream —
`MessageStart`/`MessageUpdate`/`ToolExecutionEnd`/… — is consumed internally by
the harness run loop, not re-broadcast as `HarnessEvent`). So `--mode json`
can't emit per-delta lines without a harness change.

**To revisit.** If a streaming JSON event stream is wanted, either (a) have the
harness re-broadcast `AgentEvent`s as `HarnessEvent` variants, or (b) give
`pi-cli` a way to subscribe to the lane's `AgentEvent` broadcast directly.
Option (b) is cleaner and matches the TS layering.

---

## 11. Exit-code policy

**Where:** `crates/pi-cli/src/app.rs` (`EXIT_USAGE = 2`, `EXIT_RUNTIME = 1`;
`modes::outcome_exit_code` maps `Failed`/`Aborted` → 1).

**What.** v1 distinguishes *usage* errors (parse errors, no API key, `--mode
rpc`) with exit 2 from *runtime* failures (model resolution other than
NoApiKey, harness build, run `Failed`/`Aborted`) with exit 1. TS uses
`process.exit(1)` / `process.exitCode = 1` for most paths and does not separate
usage from runtime.

**Divergence.** v1's 2-vs-1 split is a deliberate ergonomic so scripts can tell
"bad invocation" from "run failed". `Suspended` (deferred) runs also exit 1 with
a "resume not supported" message.

**To revisit.** Align with TS (everything non-zero = 1) if script authors find
the split surprising; otherwise keep it.

---

## Verification (M6)

- `cargo build -p pi-cli` — green (lib + bin).
- `cargo test -p pi-cli` — 40 unit tests pass (args/provider/session/modes/app).
- `cargo test --workspace` — full suite green (pi-telemetry/pi-ai/pi-agent/
  pi-tools/pi-harness/pi-cli + examples).
- `cargo run -p minimal` / `cargo run -p tools-example` — both run (M2/M4
  examples unaffected by the CLI addition).
- `pi --version` → `pi 0.1.0` (exit 0); `pi --help` → help (exit 0);
  `pi -Z` → "Unknown option" (exit 2); `pi -p hi` with no key → `NoApiKey`
  (exit 2); `pi --mode rpc -p hi` → "rpc not implemented" (exit 2);
  `pi --no-session --model bogus-model -p hi` → `NoMatch` (exit 1);
  `pi --no-session --model claude-sonnet-5 -p "Say hi"` with a fake key →
  harness builds, run fails at the Anthropic 401 with a clean error (exit 1).
  No silent exits; no panics.
