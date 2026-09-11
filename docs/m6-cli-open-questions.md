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

### 4a. ✅ RESOLVED — `~/.rpi/agent/` is nested (parity with upstream `~/.pi/agent/`)

**Status.** v1 now mirrors upstream `getAgentDir()`: config lives under
`~/.rpi/agent/` (`auth.json`, `models.json`, `settings.json`, `trust.json`),
matching `~/.pi/agent/`. `RPI_CODING_AGENT_DIR` (absolute path only) overrides
the agent dir itself — same semantics as `PI_CODING_AGENT_DIR`. The goal
(".pi 目录靠过来就能用" — drop a `.pi/agent/` dir at `~/.rpi/agent/` or point
`RPI_CODING_AGENT_DIR` at it and it works) is met for the config layer: rpi
reads the same `auth.json`/`models.json`/`settings.json`/`trust.json` pi
writes, honors a saved `defaultModel`/`defaultThinkingLevel`/`theme`, and
expands `$ENV`/`!command` config values (§4c).

**Migration.** A one-shot `config::migrate_legacy_layout()` (called from
`app::run` on startup, **only when `RPI_CODING_AGENT_DIR` is unset**) moves
legacy flat `~/.rpi/{auth.json,models.json,.setup_done,.earendil_seen}` into
`~/.rpi/agent/`. It's idempotent, best-effort (rename with copy-fallback),
no-ops when `agent/` already exists or no flat files are present, and never
blocks startup. Existing flat installs upgrade transparently on the next run.

**Sentinels.** `.setup_done` / `.earendil_seen` moved under `agent/` (the
`extras.rs` sentinels now delegate to `config::agent_dir()` instead of a
private `rpi_dir()`, which also fixes an old bug where they ignored the env
override).

**Remaining divergences under this heading (deferred):**
- ~~**`bare apiKey` → `x-api-key`.**~~ **✅ RESOLVED.** Upstream
  `provider-composer` routes a models.json provider's bare `apiKey` (no
  `authHeader`) as `x-api-key` (`composeApiKeyAuth`). rpi now mirrors this:
  `provider::models_json_api_key` extracts the first anthropic-compatible
  provider's bare `apiKey` (resolved via `resolve_config_value`), and
  `provider::resolve` auth step 3b folds it as `x-api-key` onto **gateway
  model headers** specifically — NOT as the global `provider_key` (which
  `assemble_headers` would stamp onto every model incl. built-in `claude-*`,
  routing a gateway key to `api.anthropic.com` → 401). The fold reuses the
  same `is_gateway` predicate + `auth_from_models_json` flag as the
  `authHeader:true` Bearer path (step 3a), so a bare-`apiKey` gateway
  satisfies auth on its own and a no-`--model` launch picks the gateway
  model. `has_header_auth` treats the model-header `x-api-key` as owned
  auth. `authHeader:true` wins over bare `apiKey` when both exist (3a before
  3b). A copied pi models.json using bare `apiKey` now "just works"; tests
  `models_json_bare_apikey_satisfies_auth_without_env`,
  `default_prefers_gateway_when_only_bare_apikey_configured`,
  `models_json_bare_apikey_env_template_resolves`,
  `auth_header_provider_beats_bare_apikey_provider` pin it.
- **Project-trust prompt + `trust.json` gate.** rpi reads `trust.json`
  (`config::read_trust`) for layout parity (a copied pi `trust.json` parses +
  is located at `~/.rpi/agent/trust.json`), but does **not** wire a trust
  prompt or gate project `.rpi`/`.pi` resources behind it — rpi doesn't load the
  resources pi gates there (skills/templates/context, §8). Deferred until
  resource discovery lands.
- **Session dir.** v1's default session dir is `<cwd>/.rpi/sessions`, with an
  existing `<cwd>/.pi/sessions` directory retained as a compatibility fallback
  (see §6), **not** `<agentDir>/sessions`. pi encodes cwd into session
  filenames; rpi's session layer is a separate design. Aligning the session
  location is out of scope for this config-parity pass.

### 4b. No file lock; atomic rename instead

**Divergence.** Upstream uses `proper-lockfile` for cross-process safety on
auth.json/models.json writes. v1 is a single-process CLI, so it writes a temp file
then `fs::rename`s it into place (atomic on the same filesystem) and chmods 0o600 on
Unix afterward. Concurrent `rpi auth login` from two shells could lose one write
(last-rename-wins); this is a documented v1 trade-off, not a defect.

**To revisit.** Add `fs4`/`proper-lockfile` if multi-process safety matters (e.g. a
future daemon/TUI left open while a `rpi` one-shot runs).

### 4c. ✅ RESOLVED — `resolveConfigValue` (`$ENV` / `${ENV}` / `!command`) ported

**Where:** `crates/pi-cli/src/config.rs::resolve_config_value` +
`resolve_headers` + `resolve_command`, applied at the three consumption points
that mirror upstream (`resolve-config-value.ts`):

| Value | Where applied | Upstream mirror |
|---|---|---|
| `auth.json` `anthropic.api_key.key` | `provider::resolve` auth step 2 | `auth-storage.ts:267` (with `credential.env` overlay) |
| `models.json` provider `apiKey` (Bearer wrap) | `models_json_bearer_token` | `provider-composer.ts:351` |
| `models.json` provider `apiKey` (bare → x-api-key) | `models_json_api_key` + resolve step 3b | `provider-composer.ts:349-354` `composeApiKeyAuth` |
| `models.json` `headers` values | `provider_to_models` (merge) | `provider-composer.ts:361` `resolveHeadersOrThrow` |

**Semantics.** A `!cmd` value runs the shell (`sh -c` / `cmd /C`), cached
per-process (10s, success⇒trimmed stdout, ENOENT/non-zero⇒`None`). A `$VAR`/
`${VAR}` template interpolates from an optional env overlay then the process
env; `$$`→`$` and `$!`→`!` escape; **any referenced unset var ⇒ the whole
value resolves to `None`** (pi semantics). A literal otherwise. Auth headers
that resolve to `None` are dropped (`resolve_headers`), matching pi
`resolveHeaders`. A models.json referencing `$ANTHROPIC_API_KEY` no longer
needs the secret copied into the file.

**Bare-`apiKey` routing (resolved, see §4a):** a models.json provider's
`api_key` with no `authHeader` now folds as `x-api-key` onto gateway model
headers (endpoint-specific), completing the copy-over gap that was previously
deferred under §4c.

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

**Default selection (no `--model`).** When `--model` is absent, v1 now picks the
default the way the TS `findInitialModel`
(`packages/coding-agent/src/core/model-resolver.ts`) does over the
auth-filtered snapshot — including the **saved settings default** (step 3),
which is what makes a copied pi `settings.json`'s `defaultModel` come alive
on launch. The precedence (`provider.rs::resolve` None-branch):

1. **Saved settings default** (pi step 3): if `~/.rpi/agent/settings.json`
   has `defaultProvider` = `"anthropic"` (or absent — v1 is anthropic-only)
   **and** `defaultModel`, and that model is in the catalog **and authed**,
   select it (honoring `defaultThinkingLevel`). This is the on-disk-parity
   path — drop a pi `settings.json` at `~/.rpi/agent/` and the saved default
   wins without `--model`.
2. **Built-in default** `claude-sonnet-5` if it is *already authenticated*
   (pi step 4 / `defaultModelPerProvider`; v1 keeps `claude-sonnet-5` vs
   pi's `claude-opus-4-8` as a deliberate divergence).
3. **First authenticated model** in the catalog (TS `availableModels[0]`).

A model is "authenticated" when it carries an auth-owned header (a folded
Bearer — see `Auth source → fold scope` below) or the provider holds a
resolved `x-api-key` (the `--api-key`/`auth.json`/`ANTHROPIC_API_KEY` path).

This matters for the **gateway-only** case: a `~/.rpi/models.json` with a
single `authHeader:true` gateway and no Anthropic key. The gateway's Bearer is
folded onto the gateway model *only* (its `base_url` is the custom endpoint);
the built-in `claude-*` models keep `api.anthropic.com` and stay Bearer-less,
so they are not "authenticated" → the default selector skips them and picks
the gateway model. The previous behavior folded the gateway Bearer onto *every*
model and then defaulted to `claude-sonnet-5` (base_url `api.anthropic.com`),
which sent a foreign token to Anthropic → 401 "Invalid bearer token". Fixed in
M6-followup F (see `rpi-config-auth-bearer` notes).

**Auth source → fold scope** (`provider.rs::resolve`):

| Bearer source | Fold scope | Default picks |
|---|---|---|
| `~/.rpi/models.json` gateway (`authHeader:true`+`apiKey`) | gateway models only (`base_url` ≠ Anthropic, or a `--base-url` override is active) | the gateway model |
| `ANTHROPIC_AUTH_TOKEN` env | **every** model (a global credential for the configured endpoint) | `claude-sonnet-5` |
| `--api-key` / `auth.json` / `ANTHROPIC_API_KEY` (x-api-key path) | no Bearer at all (auth rides on the provider key) | `claude-sonnet-5` |

Why two fold scopes: a `models.json` gateway key is endpoint-specific (the
DashScope key only works against DashScope), so it must not ride on built-in
models pointed at `api.anthropic.com`. An `ANTHROPIC_AUTH_TOKEN` is a global
credential the user intends for whatever endpoint is configured (default
Anthropic, or `--base-url`), so it rides on every model — matching the TS
provider-level credential behavior and the pre-gateway v1 behavior.

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

**Note.** The v1 default session dir is `<cwd>/.rpi/sessions`; an existing
`<cwd>/.pi/sessions` is used when no `.rpi/sessions` exists (TS uses
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

## 8. ✅ RESOLVED — Skills / prompt-templates / context-files/packages discovery wired

**Where:** `crates/pi-cli/src/session.rs::build` + `crates/pi-cli/src/resource_dirs.rs`
+ `crates/pi-harness/src/context_files.rs` + `crates/pi-harness/src/system_prompt.rs`.

**Status (Part A, done).** Resource discovery is wired end-to-end:
- **Skills**: discovered from `<cwd>/.rpi/skills`, then legacy
  `<cwd>/.pi/skills`, then `agent_dir()/skills`, project-wins-first dedupe
  (`resource_dirs::dedupe_skills`, mirrors pi
  `addSkills` collision semantics). The `<available_skills>` listing is injected
  into the system prompt by `AgentHarness::compose_prompt`, gated on the `read`
  tool being active AND `disable_model_invocation` filtering (applied inside
  `format_skills_for_system_prompt`, mirroring pi `skills.ts:335-336`).
- **Prompt-templates**: discovered from `<cwd>/.rpi/prompts`, then legacy
  `<cwd>/.pi/prompts`, then `agent_dir()/prompts`, project-wins-first dedupe.
  On-demand only (never in the
  system prompt); surfaced as `/<name>` slash commands in the TUI autocomplete +
  expandable via the harness `prompt_from_template` lane call. `/context` lists
  them.
- **Context-files**: the new `context_files.rs` loader mirrors pi
  `loadProjectContextFiles` (AGENTS-first candidates `["AGENTS.override.md",
  "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"]`, global agentDir first
  then ancestor-walk cwd→root with the **deepest (cwd) file concatenated last**).
  Rendered as a `<project_context>` block by `format_project_context`.
- **SYSTEM.md / APPEND_SYSTEM.md**: project `<cwd>/.rpi/` wins, then legacy
  `<cwd>/.pi/`, then global `<agent_dir>/` (mirrors pi
  `discoverSystemPromptFile` with an rpi-owned project layer); the same
  precedence applies to `APPEND_SYSTEM.md`. Explicit `--system-prompt` wins over
  SYSTEM.md; `--append-system-prompt` wins over APPEND_SYSTEM.md (pi
  `appendSystemPrompt`). System-prompt order mirrors pi `buildSystemPrompt`:
  base → append → context → skills.
- **Flags**: `--no-skills`/`-ns`, `--no-prompt-templates`/`-np`,
  `--no-context-files`/`-nc` each suppress one channel independently;
  `--no-extensions`/`-ne` is parsed (no-op until Part B).
- **`/context` (TUI)**: lists discovered skills, prompt templates, and a note on
  context/system/append sources (`interactive_tui::show_context_panel`).

**Divergences (documented, deferred).**
- **Trust gating**: pi gates project config files (+ some resources) behind
  `isProjectTrusted()`; rpi v1 has no trust prompt, so project resources are
  read unconditionally (a copied `.rpi/` or `.pi/` drops in and works). Full
  trust gating deferred.
- **Discovery roots**: pi reads 4 roots (`.pi/skills`, `.agents/skills`,
  `~/.pi/agent/skills`, `~/.agents/skills`) + installed packages; rpi reads
  `<cwd>/.rpi/<sub>` first, then legacy `<cwd>/.pi/<sub>`, then
  `agent_dir()/<sub>` plus enabled static package resources. rpi also resolves
  Pi's native npm store (`~/.pi/agent/npm/node_modules/<package>`) when a copied
  Pi `settings.json` contains an `npm:` package spec. `.agents/*` remains
  deferred.
- **Worktree shadowed-context-file dedup** (`findShadowedContextFile`,
  `.reference/.../resource-loader.ts:100-116`): deferred (git-layout edge case).
- **Full structured collision diagnostics**: pi carries `winnerPath`/`loserPath`
  on skill collisions; rpi v1 encodes collisions as an `InvalidMetadata`/
  `ParseFailed` diagnostic with a descriptive message naming both paths (or the
  template name — `PromptTemplate` carries no file path). Structured fields
  deferred.
- **Skill validation parity**: pi drops empty-description skills + warns on
  invalid name/description length (`skills.ts:290-307`). rpi's per-dir loader
  already drops missing-description skills; the name/desc-length warning is
  flagged as a gap to mirror (or defer with doc).

**Still deferred (Part B + later):** `.agents/*` discovery, npm package
lockfile/update management parity, and JavaScript/TypeScript package extensions
beyond the current `registerTool`/`registerCommand`/`resources_discover` plus
provider/runtime/UI bridge,
`--no-themes`, `--skill`/`--prompt-template`/`--models` cycling, and session
restore (`-c`/`-r`/`--session`). Local static package resources are now loaded
from settings package specs via `rpi package add|list|remove`.

### Part B status (Rust-native cdylib plugin system — 完整复刻 pi)

Part B lands a Rust-native (cdylib via `libloading`, NOT TS/jiti) plugin system
mirroring pi's extension surface: the 8 `register*` methods (tool, command,
event_handler, shortcut, flag, provider, + the three renderers), the 33 `on()`
event categories, 16 `runtime_action` host actions, and `resources_discover`.
Phased commits B0→B5e; **B5e completes the final phase**.

**Wired + active:**
- `register_tool` (plugin tools override same-named built-ins; first-extension-
  wins; explicit `--tools`/`--exclude-tools` still apply) — `rpi-extensions`.
- `register_event_handler` → 10 always-emitted `AgentEvent`s fanned out via the
  `ExtensionEmitter` (`rpi-extensions/translate.rs`); the three exists-but-`None`
  loop hooks (`before_tool_call`/`after_tool_call`/`transform_context`) populated
  via new `AgentHarnessOptions` fields (no crate cycle).
- `register_provider` → `PluggableProvider` (v1 one-shot non-streaming, see
  `crate-level docs`); `register_resources_discover` → `emit_resources_discover`
  feeding Part-A loaders (the "三者同交付" coherence point — plugin skill paths
  land through the SAME loaders as static skills; project skills keep winning
  name collisions).
- `runtime_action` (16 actions) via `ActionBridge` (the inverted-FFI
  `user_data`→bridge recovery + `spawn`+std-mpsc park; no ambient-runtime
  unsoundness). `/reload` staleness: `ActionBridge.invalidate()` + a fresh
  `ExtensionSession` + harness setters (`set_system_prompt`/`set_resources`/
  `set_agent_emitter`/`set_models`/`set_provider_hooks`/`set_tools`); a plugin's
  `runtime_action(Reload)` signals a `ReloadMailbox` the TUI drains (avoids the
  self-unmapping race).
- **B5e: `register_markdown_transformer`** wires into the TUI render path —
  `AssistantMessageComponent` applies an installed `Fn(&str) -> String` to raw
  assistant text BEFORE `Markdown` styling (both the text arm + the thinking arm;
  thinking transforms the plain body first, then ANSI-wraps). The cycle-free
  seam: `rpi-tui` takes only the trait object (NO `rpi-extensions` dep); `rpi-cli`
  builds the closure from the live `RegistrySnapshot` (chains handlers in
  registration order, `{"markdown":…}` envelope, `catch_unwind`-wrapped FFI,
  per-handler skip-on-error, stale-snapshot no-ops to identity). `/reload`
  rebuilds the transformer from the fresh snapshot + reinstalls on the in-flight
  streaming component so a reloaded plugin's transform takes effect immediately.

**Message/entry renderer UI is now wired:**
- `register_message_renderer` / `register_entry_renderer` are held in the
  live `RegistrySnapshot` and invoked by both restored-history and streaming
  TUI paths. The host accepts `{text, markdown?}` or `{lines:[...]}` output and
  maps it to native terminal components; failures fall back to the built-in
  custom-message/entry display. `session::report_deferred_renderers` now only
  reports active renderer counts for `--verbose`.

**Extension command UI is now wired:**
- Registered commands participate in slash autocomplete and dispatch. A
  handler may return `{kind:"message",text}`, `{kind:"selector",items:[...]}`
  or `{kind:"editor",initialText}`; selector/editor submissions call the same
  handler with an `action` envelope and restore the native editor on completion
  or cancellation.

**Documented limits (v1):** plugin providers are one-shot (sync `ProviderRequestFn`
can't drive a chunked `stream_simple`); SDK JSON crosses as a string
round-trip (documented precision caveat — enable `arbitrary_precision`+
`preserve_order` consistently host+plugin, or accept the limit). `.agents/*`,
npm package update/lockfile management, JavaScript/TypeScript package
extensions beyond the supported Node bridge surface, worktree shadow (`findShadowedContextFile`), full skill
name/desc-length validation, and project-trust gating remain deferred. Static
package skills, prompts, themes, and system prompt fragments are supported by
`rpi package`.

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
