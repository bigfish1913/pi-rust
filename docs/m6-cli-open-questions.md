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

## 4. Anthropic-only; persistent config + Bearer/custom-endpoint support; no OAuth/Copilot

**Where:** `crates/pi-cli/src/provider.rs::resolve`, `crates/pi-cli/src/config.rs`,
`crates/pi-cli/src/auth.rs`.

**What.** v1 is Anthropic-only (plan §5.16: "OAuth/Copilot skipped v1"). `--provider`
must be `anthropic` (or absent); any other value is a hard `UnknownProvider` error.
Beyond the original `ANTHROPIC_API_KEY` / `--api-key` x-api-key paths, v1 now mirrors
the upstream native pi config layer to support:

- **Persistent credentials** — `rpi auth login` writes a key to `~/.rpi/auth.json`
  (0o600 on Unix) under the `anthropic` provider id; `resolve` loads it ahead of env
  vars. `rpi auth check` / `rpi auth logout` round out the subcommand.
- **Third-party Anthropic-compatible endpoints** — `ANTHROPIC_BASE_URL` / `--base-url`
  override `model.base_url` at request time; `ANTHROPIC_AUTH_TOKEN` (or a
  `models.json` provider with `authHeader: true` + `apiKey`) authenticates as
  `Authorization: Bearer …` instead of `x-api-key`. This lets `rpi` talk to
  one-api / new-api / claude-code-router / private reverse proxies that speak the
  Anthropic Messages protocol over a Bearer header.
- **`~/.rpi/models.json`** — a hand-edited `{providers: {id: ProviderConfig}}` file
  that augments/replaces the built-in Anthropic catalog with custom model ids, a
  custom `base_url`, extra `headers`, and a gateway `authHeader`+`apiKey` Bearer
  source. A models.json-only gateway (no env, no stored cred, no `--api-key`) is a
  complete setup: `authHeader:true` + `apiKey` satisfies auth and the Bearer rides
  on `model.headers`.

Auth resolution precedence (`provider.rs::resolve`, mirrors upstream
`packages/coding-agent/src/cli/anthropic.ts` resolve order):

1. `--api-key` → `x-api-key` header.
2. `~/.rpi/auth.json` `anthropic.api_key.key` → `x-api-key` (the persistent login).
3. `~/.rpi/models.json` first anthropic-compatible provider with `authHeader:true` +
   non-empty `apiKey` → `Authorization: Bearer <apiKey>` on `model.headers`.
4. `ANTHROPIC_AUTH_TOKEN` env → `Authorization: Bearer <token>` on `model.headers`.
5. `ANTHROPIC_API_KEY` env → `x-api-key` header.
6. none → `NoApiKey` (exit 2, before any network call).

`base_url` resolves `--base-url` → `ANTHROPIC_BASE_URL` → the model's catalog
`base_url`; the winner overwrites `resolved.model.base_url`, which the provider
reads at request time (`rpi-ai` — no protocol change, the `Model.base_url` field is
already a mutable `String`).

**How the Bearer path authenticates without an x-api-key (rpi-ai fix).** Upstream,
the model-resolution layer merges `providerOrModel.headers` into `options.headers`
*before* `assertRequestAuth` (`models.ts:560` `mergeHeaders(result.auth.headers,
providerOrModel.headers)`), so the auth check sees the Bearer the model carries.
The Rust port keeps auth headers on `model.headers` and merges them later in
`assemble_headers`, so the `has_header_auth(&opts.headers)` check — which decides
whether to demand an x-api-key — saw an empty `opts.headers` and errored
"No API key for provider: anthropic" even when `model.headers` carried a Bearer.
The fix consults *both*: `has_header_auth(&opts.headers) || has_header_auth(&model.headers)`
(`crates/pi-ai/src/providers/anthropic/mod.rs`, `run_anthropic_stream`). This mirrors
the TS net effect and lets a Bearer folded onto `model.headers` authenticate. A
regression test (`header_auth_on_model_headers_counts_as_owned`) pins it.

### 4a. `~/.rpi` is flat; upstream `~/.pi/agent/` is nested

**Divergence.** Upstream uses `<agentDir>/` = `~/.pi/agent/` and nests `auth.json`,
`models.json`, *plus* themes/bin/prompts/sessions/etc. under it. v1 uses `~/.rpi/`
**directly** with only `auth.json` + `models.json` — two files, one flat dir. The
extra upstream nesting exists because that one dir hosts many subsystems; v1 has
only auth+models, so the `agent/` layer would be dead weight. `RPI_CODING_AGENT_DIR`
(absolute path only) overrides the dir, mirroring `PI_CODING_AGENT_DIR`. The default
session dir is still `<cwd>/.pi/sessions` (see §6) — independent of `~/.rpi`.

**To revisit.** If v1 grows themes/prompts/skills on disk, either adopt the `agent/`
subdir or keep flat and name files distinctly.

### 4b. No file lock; atomic rename instead

**Divergence.** Upstream uses `proper-lockfile` for cross-process safety on
auth.json/models.json writes. v1 is a single-process CLI, so it writes a temp file
then `fs::rename`s it into place (atomic on the same filesystem) and chmods 0o600 on
Unix afterward. Concurrent `rpi auth login` from two shells could lose one write
(last-rename-wins); this is a documented v1 trade-off, not a defect.

**To revisit.** Add `fs4`/`proper-lockfile` if multi-process safety matters (e.g. a
future daemon/TUI left open while a `rpi` one-shot runs).

### 4c. models.json has no `$ENV` / `!command` / `${ENV}` credential expansion

**Where:** `crates/pi-cli/src/config.rs` — `apiKey`/`headers` values are taken as
literal strings.

**Divergence.** Upstream `resolveConfigValue` interpolates `$ENV_VAR`,
`!shell-command`, and `${ENV}` inside config values, so a models.json can reference
`$ANTHROPIC_API_KEY` without copying the secret. v1 parses only literals — put the
secret in the file, or use the env-var auth sources (`ANTHROPIC_AUTH_TOKEN` /
`ANTHROPIC_API_KEY`) instead. This avoids pulling a shell-eval / env-expansion
machinery (and its security surface) into a v1 config reader.

**To revisit.** Port a constrained `resolveConfigValue` (env-only, no `!command`)
if users want models.json to stay secret-free on disk.

### 4d. OAuth / Copilot device-code still deferred

**Divergence.** The `Credential` enum declares an `Oauth { access, refresh, expires }`
variant for format forward-compat, but `rpi auth login` writes only the `ApiKey`
variant and `resolve` never consults OAuth. Claude Pro/Max subscription auth
(device-code grant + token refresh) remains deferred (plan §5.16). The stored-cred
path (`auth.json` `anthropic.api_key.key`) is the "persistent login" equivalent.

**To revisit.** Port the device-code flow + token refresh when subscription auth is
needed; the `Credential::Oauth` shape is already there to hold it.

### 4e. `authHeader: true` Bearer synthesis is centralized, not per-model

**Where:** `crates/pi-cli/src/provider.rs::resolve` (via `models_json_bearer_token`)
vs `provider_to_models` (which deliberately does *not* synthesize it).

**Divergence.** Upstream folds the `authHeader:true`-wrapped Bearer onto each model
at config-load time (`provider-composer.ts`). v1 synthesizes it **centrally in
`resolve`** and only when no higher-priority x-api-key source (`--api-key` /
auth.json / `ANTHROPIC_API_KEY`) wins — otherwise the x-api-key path would also
carry a spurious Bearer. So `provider_to_models` merges declared `headers` only;
`resolve` adds the `Authorization: Bearer` per-model when it's the chosen auth
source. This means a models.json gateway `apiKey` is *both* an auth source (step 3
above) *and* the value folded into the Bearer — one place, one decision.

### 4f. models.json provider id is config-namespacing only

**Where:** `crates/pi-cli/src/config.rs::provider_to_models` stamps
`provider = "anthropic"` (not the models.json key) on every models.json model.

**Divergence.** v1 has a **single** `AnthropicProvider` (its `id()` is hardcoded
`"anthropic"`) and the harness routes by `provider.id() == model.provider`
(`AgentHarness::resolve_provider`). Upstream `registerProvider(providerName, …)`
registers a distinct provider per models.json key and routes by that key. So a
`gateway/custom-claude` models.json entry carries `provider = "anthropic"` and is
addressed as `--model gateway/custom-claude` (the `gateway/` prefix is stripped by
`split_model_pattern`; it's pure CLI namespacing). The actual per-endpoint
differentiation — `base_url` and `headers` — rides on the model fields, which the
provider reads at request time. Divergence is structural: v1 has no multi-provider
registry.

**To revisit.** If v1 supports non-Anthropic protocols (OpenAI/Google) via
models.json, build a provider registry keyed by the models.json `api` value and
stamp the real provider id; the current `"anthropic"` stamp is the single-provider
shortcut.

**To revisit (whole section).** OpenAI/Google/Bedrock providers have a `Provider`
trait seam in `rpi-ai` already; wiring them needs a resolver + their catalogs. The
`models.json` non-`anthropic-messages` `api` values are parsed-and-ignored in v1
(`provider_is_anthropic_compatible` returns false → the provider is skipped,
documented).

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

## 9. ✅ RESOLVED — Read-only `grep`/`find`/`ls` ported to `pi-tools` (in-process)

**Where:** `crates/pi-cli/src/session.rs::BUILTIN_TOOL_NAMES` + the help text.

**Status.** `grep`/`find`/`ls` are now in `BUILTIN_TOOL_NAMES` and registered by
`build_tools`. The CLI help lists them; `--tools grep` works. The tools live in
`crates/pi-tools/src/tools/{grep,find,ls}.rs` with full test coverage
(`tests/{grep,find,ls}.rs`, 23 tests, all green) against `InMemoryExecutionEnv`.

**Divergence from TS (documented).** The TS `grep` shells out to `rg`
(JSON-stream parsing) and `find` to `fd` (`--full-path` rewrites, gitignore-aware
walking, optional auto-download); `ls` is pure fs. The Rust port implements
**all three in-process** through the `FileSystem` trait + the `regex`/`globset`
crates, so they run against *any* `ExecutionEnv` — both `OsExecutionEnv` and
`InMemoryExecutionEnv` (the TS shell-out design cannot do the latter, which is
the reason this port chose in-process: trait-fidelity + testability). v1 skips
full `.gitignore` matching (only `.git/` directories are skipped); revisit via
the [`ignore`](https://docs.rs/ignore) crate for an `OsExecutionEnv`-only fast
path if parsing performance demands it. `ls` matches TS exactly. `grep` output
shape, match/context line formats, per-line + byte truncation, and match-limit
semantics match TS exactly. `find` ports the `**/`-prepend rewrite and
relativization; the Windows `[/\\]` separator rewrite is unnecessary (paths are
posix-normalized internally).

**Cwd resolution fix.** Porting these uncovered that
`InMemoryExecutionEnv::resolve` preserved `.`/`..` path components (so
`resolve_read_tool_path(".")` → `/tmp/work/.`, missing the BTreeMap key
`/tmp/work`), while `OsExecutionEnv::normalize_absolute` collapses them. `resolve`
now collapses `.`/`..` component-by-component to match the OS env, and
`with_cwd` pre-registers the cwd + ancestors as directories (the cwd always
exists on a real fs — you're in it), so tools that default their search path to
`.` work without the test needing to `make_dir` the cwd first.

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

## Verification (M6 / config+auth follow-up)

- `cargo build -p rpi-cli` — green (lib + bin).
- `cargo test -p rpi-cli` — 68 unit tests pass (args/provider/session/modes/app
  + the new config.rs path/auth/models tests + auth.rs login/check/logout +
  provider.rs Bearer/base_url/models.json-precedence tests).
- `cargo test --workspace` — full suite green (pi-telemetry/pi-ai/pi-agent/
  pi-tools/pi-harness/pi-cli + examples, `--features rpi-ai/providers`).
- `pi --version` → `pi 0.1.0` (exit 0); `pi --help` → help (exit 0);
  `pi -Z` → "Unknown option" (exit 2); `pi -p hi` with no key → `NoApiKey`
  (exit 2); `pi --mode rpc -p hi` → "rpc not implemented" (exit 2);
  `pi --no-session --model bogus-model -p hi` → `NoMatch` (exit 1);
  `pi --no-session --model claude-sonnet-5 -p "Say hi"` with a fake key →
  harness builds, run fails at the Anthropic 401 with a clean error (exit 1).
  No silent exits; no panics.
- **Bearer/custom-endpoint verification (no real Anthropic key needed):** a local
  fake Anthropic-SSE server confirms both Bearer paths end-to-end —
  `ANTHROPIC_AUTH_TOKEN` + `ANTHROPIC_BASE_URL` → probe received
  `Authorization: Bearer sk-…` with no `x-api-key`; and a `~/.rpi/models.json`
  gateway (`authHeader:true` + `apiKey`, no env vars, no stored cred, no flag) →
  probe received `Authorization: Bearer <gateway apiKey>` with no `x-api-key`.
  Both streamed the probe's reply text. The `--api-key` path correctly sends
  `x-api-key` and *no* Bearer (the central-bearer synthesis is skipped on the
  x-api-key path — pinned by `api_key_flag_beats_models_json_bearer`).
