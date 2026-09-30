# Changelog

All notable changes to the `rpi-*` crate family are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The nine published crates share one workspace version and are released together
in dependency order. `git tag vX.Y.Z` marks the commit that a release was cut
from; the matching [GitHub Release](../../releases) carries the same notes.

## [Unreleased]

## [0.3.6] - 2026-09-30

## [0.3.5] - 2026-09-29

## [0.3.4] - 2026-09-27

### Added

- **Long pastes fold into a `[paste #N …]` marker instead of flooding the
  editor.** A bracketed paste longer than 10 lines or 1000 characters collapses
  to a single `[paste #N +M lines]` / `[paste #N M chars]` token, matching native
  pi, so dropping a stack trace or a file into the prompt still leaves a readable
  draft. The editor keeps the full text beside the marker — submit sends the
  expansion — and the marker is atomic: one backspace or arrow press removes or
  steps over the whole thing, and deleting an earlier marker renumbers the later
  ones so `#N` stays gapless. Pastes under the threshold still insert verbatim,
  and Windows' Ctrl+V clipboard fallback takes the same path.
- **`Ctrl+X` copies the way native pi's `app.message.copy` does.** The editor
  selection is copied when there is one, otherwise the last assistant reply.
  Native pi binds the same action to the same key; this is where selection-copy
  lives now that `Ctrl+C` clears.

### Changed

- **Transcript repaints are now O(1) per unchanged message, closing the
  long-session lag against native pi.** rpi re-parsed every finished message's
  markdown/layout on every frame, so a 400-message session cost ~18 ms per
  frame (over the 16 ms budget) and got slower as it grew. It now mirrors
  native pi's caching layers: `Markdown` and `Text` memoize
  `(content, width) -> lines`, `UserMessageComponent` builds its `Box` +
  `Markdown` once instead of per frame, finalized `ToolExecutionComponent` /
  `BashExecutionComponent` panels cache their output, and the layout pass has a
  frame-local `renderCached` map so a component measured for an auto basis is
  not rendered again when painted. A live theme switch bumps a global revision
  that every cache keys on, so `apply_theme_preset` cannot leave stale colours.
  A 400-message transcript drops from ~18 ms to ~0.8 ms per frame (~22×) and
  stays under budget past 3000 messages; `crates/rpi-tui/examples/bench_transcript.rs`
  reproduces the measurement.
- **The terminal window/tab title now carries the startup mark.** An idle
  session sets `▃ ▅ ▂ rpi · 🦀` instead of `🦀π rpi`, keeping the activity
  suffix it had before (`⟳` in flight, `↻` retrying), so the tab and the startup
  lockup read as one brand: the title's three eighth-blocks are the lockup's
  3:5:2 bar silhouette flattened into a single row, and a test reads the bar
  heights back out of the lockup so the two cannot drift apart. Each bar is one
  cell wide — the first cut used `██` per bar and read as three fat slabs in a
  tab strip. The title is built by `pi-cli`'s `brand` module, so it drops the
  crab with `RPI_NO_EMOJI` exactly like the lockup — and it drops `π`, which is
  East-Asian *Ambiguous* and therefore measured differently under a CJK locale.
  Block elements share `█`'s width class, so the title is still single-width
  everywhere.
- **`Ctrl+C` clears the editor instead of aborting the run, matching native pi's
  `handleCtrlC`.** One press clears the draft — text *and* selection — and a
  second press within 500ms quits. Aborting stays on `Esc` (`app.interrupt`), so
  a stray `Ctrl+C` while a turn is streaming can no longer kill it, which is easy
  to do while a half-typed steering message sits in the editor. Auto-repeat is
  ignored, so holding the key cannot turn one tap into the quit; the quit still
  forces the process out when the async loop is parked inside a run. The footer,
  `/help`, and the remote-shell footer now read `Ctrl+C: Clear/Exit` and
  `Esc: Abort` instead of `Ctrl+C: Abort/Exit`.
- **`Ctrl+D` and the model-cycle keys now match native pi too.** `Ctrl+D` no
  longer aborts a run: with text in the editor it deletes the character forward
  (`tui.editor.deleteCharForward`), and on an empty editor it quits
  (`app.exit`). Aborting is now `Esc` and nothing else. Model cycling moved from
  `Ctrl+M` to native's `Ctrl+P`, joined by a new `Shift+Ctrl+P` (`Alt+P` on
  Windows) for the previous model — every hint, selector description, and doc
  comment that named the old key was updated with it.
- **The last native-only keybindings are wired up.** `Ctrl+Z` suspends to the
  background on Unix (`app.suspend` — native binds no key on Windows, so neither
  does rpi): it stops the renderer, hands the terminal back to the shell, and
  re-enters the alternate screen when the process is resumed with `fg`. On
  Windows, `Ctrl+Q` joins `Alt+Enter` for follow-ups and `Alt+V` joins `Ctrl+V`
  for image paste — native's Windows keys, added alongside rpi's rather than
  replacing them, because the Windows clipboard-*text* fallback still rides
  `Ctrl+V`. The registry now matches native's defaults for every app-level
  action rpi implements.

### Fixed

- **Cancelling a run hands the messages you queued while it was in flight back
  to the editor.** Esc / Ctrl+C / Ctrl+D aborted the lane, which drains its
  steering and follow-up queues — and the helper meant to pull them into the
  editor first existed but was never called, so anything staged during a run
  vanished on cancel. The three cancel paths now drain the queue into the draft
  (queued text first, then whatever you had typed, caret at the end of the last
  line) *before* the abort, mirroring native pi's
  `restoreQueuedMessagesToEditor({ abort: true })`. Dequeuing with Alt+Q / Alt+Up
  reports `Restored N queued message(s) to editor` (or `No queued messages to
  restore`) like native `handleDequeue`, and the restored draft's caret lands on
  the last line at a byte offset — the old `(0, char_count)` call parked it at
  the end of the first line and split multi-byte characters.

## [0.3.3] - 2026-09-26

### Added

- **Plugin tools now receive the session they run in.** The host merges a
  reserved `__rpi` object — `{"cwd", "sessionId"}` — into every plugin tool
  call's arguments, in the tool adapter's `execute` rather than in the agent loop
  or `prepare_arguments`, so the field reaches the plugin and nothing else: not
  the model, not the `ToolExecutionStart`/`Update` events, not the session log.
  A model cannot forge it either — the host's value overrides one that matches.
  Built-in tools are unaffected; they already receive an
  `ExecutionToolContext`. This is what lets a plugin keep per-session state
  without asking the model to guess an identity. `docs/extension-authoring.md`
  documents the contract, including the one failure mode worth naming: a
  `#[serde(deny_unknown_fields)]` parameter struct will reject the injected key.
- `cargo binstall rpi-cli` works: `[package.metadata.binstall]` points at the
  release assets, which are named after the binary (`rpi`) rather than the crate,
  so the URL template uses `{ bin }`.

### Changed

- A tool result can ask to be rendered as markdown by returning
  `details.markdown = true`. The TUI renders those bodies with its markdown
  component — real table borders, list markers, inline code — instead of printing
  the source literally; every other tool is untouched. Markdown rows are not
  re-wrapped (the panel already wraps) and drop the `│` gutter so table borders
  stay clean.

### Fixed

- **A session log written by two processes at once can be opened again.** Two
  writers diverged their sequence counters: a mutation landed with an
  already-consumed seq, a later writer's seqs jumped forward, and an entry
  chained to a leaf the local state never reached — at which point the v4 loader
  rejected the whole session, so it could not be opened at all. Loading now drops
  a stale-seq append, resyncs the counter across a divergent writer's forward gap
  (raw seqs untouched, so later appends stay consistent), and re-chains an entry
  onto the lane leaf; when it dropped anything the file is republished
  atomically.
- A session already open in another `rpi` process is refused with `session … is
  already open in another rpi process` instead of being written to by both. The
  host holds an exclusive OS advisory lock per session (`<path>.lock`) for its
  lifetime.
- `Could not open the saved session:` was printed twice when a session could not
  be opened.

## [0.3.2] - 2026-09-25

### Added

- Session pickers (`rpi -r` and `/session`) show readable rows instead of a wall
  of near-identical timestamped file names. Each row comes from a cheap
  head/tail summary of the session file — display name, first user prompt
  (single-lined and truncated to a label), and on-disk size — so saved sessions
  can be told apart without parsing logs that can run to tens of megabytes.
  Search matches the label, the short id and the file name, and the primary
  column is wide enough that a long label is not clipped.
- `--name` / `-n` records a session's display name durably, for a fresh session
  as well as a restored one; an empty value clears a name set earlier.

### Changed

- The interactive header now shows the three-bar brand mark from the site
  favicon, labelled with the crab in place of the language name, instead of the
  previous crab-and-π logo.
  The old mark mixed an emoji with a box-drawing glyph, whose east-asian width is
  ambiguous, so the header rendered inconsistently depending on the terminal.
  The mark follows the active theme's accent colour for its middle bar.
  `RPI_NO_EMOJI=1` renders the lockup without the crab (and without the
  separator that would otherwise dangle).
- The workspace is now `rustfmt`-clean. The sweep is listed in
  `.git-blame-ignore-revs` so `git blame` skips it.
- `rpi-ai`, `rpi-agent`, `rpi-tools`, `rpi-harness` and `rpi-cli` now carry the
  crates.io `artificial-intelligence` category, so they are reachable by
  browsing that category and not only by keyword search.

### Fixed

- A superseded runtime-context update could publish its `activeTools` list after
  a newer one had already been applied. Concurrent `set_runtime_context` calls
  run on separate threads, and the thread holding the older response could reach
  the recording step last, resurrecting a tool list the newer context had
  replaced. The result is now discarded unless its revision is still the newest,
  matching the rule Node already applies to the request itself.
- `-c` / `--continue` and `-r` / `/session` no longer offer sessions that hold
  only their header, the leftovers an abandoned launch leaves behind. Resuming
  one of them previously opened an empty transcript; `-c` now picks the newest
  session that has content, falling back to the newest overall only when every
  session is empty.

## [0.3.1] - 2026-09-25

No functional change to the agent, tools or CLI. This release repairs the build
on `main` and fixes how every crate presents itself on crates.io and docs.rs.

### Fixed

- **`rpi-plugin-sdk` did not compile on `main`.** Duplicate definitions added
  after 0.3.0 shipped — a `pub type PluginApi = PluginApiVt` alias beside the
  `PluginApi` struct, and a second `register_entrypoint_unified` — produced
  `E0428`, `E0119` and `E0609`, so `cargo test --workspace` failed immediately
  for anyone cloning the repository. The duplicates are removed and the build is
  green again. The published 0.3.0 artifacts were built before they landed and
  were never affected.

### Changed

- Every crate declares `homepage` and `documentation`, and ships its own
  `README.md` instead of the workspace README. Previously all nine crates.io and
  docs.rs pages showed the same generic document, and its relative links
  (`docs/architecture.md`, `LICENSE`, `examples/plugin-stub`) resolved to 404 on
  those sites.
- docs.rs now builds with `all-features` for every crate except `rpi-cli`, whose
  clipboard feature needs platform libraries that are not present there.

### Docs

- Added `docs/performance-vs-pi.md` and `scripts/bench-vs-pi.mjs` — a reproducible,
  same-machine comparison against native Pi over the same JSONL RPC endpoint,
  with isolated config directories and both tools offline. rpi is 9.7× faster to
  start, 1.7× faster to a usable agent, 4.9× smaller in memory and ~21× smaller
  installed. The same measurement shows 83% of rpi's startup is its own runtime
  initialisation rather than process overhead, which is now the roadmap's
  optimisation target.
- `crates/rpi-cli/embedded-docs/` — the documentation snapshot compiled into the
  `rpi` binary for the `docs` tool — is refreshed and now guarded: CI runs
  `scripts/sync-embedded-docs.sh` and fails if the snapshot drifts from the
  repository documents. It had fallen behind by several releases.

## [0.3.0] - 2026-09-25

### Changed

- **Breaking:** the plugin ABI is unified on a single `rpi_plugin_register`
  entrypoint (`RPI_PLUGIN_ABI_VERSION_UNIFIED`), with the version carried inside
  the `PluginApi` struct instead of the symbol name. `rpi-extensions` still
  resolves `rpi_plugin_register_v3` and then `rpi_plugin_register_v2`, so plugins
  built against an older ABI keep loading; extension authors should rebuild
  against the current SDK when convenient.
- Workspace dependencies (`rpi-*` internal crates) bumped to `0.2.0`, then to
  `0.3.0` for the whole family.

### Added

- A model-visible system prompt section that makes the agent aware of reasoning
  replay, developed alongside `docs/llm-repetition-forensics.md`.

### Docs

- `docs/llm-repetition-forensics.md` updated with the latest evidence and the
  corrections to earlier conclusions in that document.

## [0.2.0] - 2026-09-25

### Changed

- **Breaking:** plugin ABI unified — see 0.3.0 for the entrypoint negotiation
  order.

## [0.1.28] - 2026-09-25

### Added

- **Crash recovery, end to end.** Frame-level progress (`LaneRecord::AssistantFrame`),
  commit-on-settle for assistant messages carrying tool calls, startup
  continuation from three entry points (before the first request, during tool
  execution, while waiting to retry), and retry-intent records so a crash during
  backoff retries rather than salvaging a failed attempt as history.
- **Durable queues.** Steering, follow-up and `nextRun` messages are recorded on
  enqueue and rebuilt on start, so a message typed while the agent works survives
  a restart.
- **Deferred (long-poll) provider responses.** New `DeferredProvider` capability
  and a defaulted `Provider::deferred()` in `rpi-ai`; `run_agent_loop_from_assistant`
  in `rpi-agent` executes parked tool calls exactly once; `resume_deferred` in
  `rpi-harness` polls, records, and re-suspends as needed.
- **Settings parity.** `retry`, `compaction`, `steeringMode` and `followUpMode`
  now come from `settings.json` instead of hardcoded defaults. `sessionDir`,
  `shellCommandPrefix` and `httpProxy` are wired, and native Pi's key names
  `enabledModels` / `httpIdleTimeoutMs` are accepted.

### Fixed

- A run that kept dying can no longer restart the same work forever: every
  continuation point is bounded by a resume budget.

### Docs

- `docs/llm-repetition-forensics.md` §11 documents the per-state comparison
  against native Pi's durable state machine.
- `docs/native-pi-missing-features.md` §7/§15 corrected against verified
  behaviour.

## [0.1.27] - 2026-09-24

### Added

- CBOR binary protocol (framing + codec) in the CLI.
- Image generation API (`openrouter-images`) in `rpi-ai`.
- Remote model catalog refresh (`pi.dev`).
- TUI: rich footer status bar with live git branch refresh; code-block syntax
  highlighting.
- CLI: file-watch auto reload (`fs-watch`); HTTP proxy and pooled idle timeout;
  startup timing / experimental switches, session cwd restore, structured
  resource diagnostics, and cache-waste statistics.
- Custom provider, user-agent, guidance and attribution support, plus
  `--changelog`.

### Fixed

- Anthropic request failures no longer write to stderr, which previously
  corrupted fullscreen TUI rendering.
- Extensions: a plugin panic is now isolated instead of taking down the host
  process.
- `rpi-ai`: corrected a `constrained.rs` doc example that referenced a
  non-existent `ToolDefinition`.

## [0.1.26] - 2026-09-23

### Fixed

- `clipboard`: gate the `arboard` dependency and clipboard functions on
  non-Android targets so `cargo install` works on Android/Termux.

## [0.1.25] - 2026-09-23

### Added

- Extension UI prompt events and fullscreen copy-on-select.
- Transcript search (`Ctrl+Shift+F`).
- Paste-content detection to fix multi-line pasting on Windows; bracketed paste
  mode is enabled explicitly.
- PowerShell tool registered on Windows, plus a `default_tools` setting.
- `rpi-agent`: updated agent construction API and restructured documentation.

### Fixed

- Bash tool throttled-flush behaviour.
- `@` symbol is preserved when accepting file autocomplete.
- `arboard` gated to non-Android targets.

## [0.1.24] - 2026-09-22

### Added

- **Remote mode.** `rpi --mode rpc` is a real JSONL command loop, `rpi --server`
  runs the agent headless over TCP and prints a token at startup, and
  `rpi --connect <host:port>` attaches a terminal client that holds no local
  provider, tools, extensions or session files. Token auth is connection-level
  (`-32001` on missing/invalid token); `--no-token` disables it, and
  `RPI_SERVER_TOKEN` is honoured. Shared wire types (`RemoteEvent` /
  `RemoteCommand` / `RemoteResponse`) live in one module so the two ends cannot
  drift.
- **Extension lifecycle events (P0–P5) with veto support** — a handler can
  refuse startup by returning `EVENT_HANDLER_ABORT` (new `BeforeTuiStart` hook,
  exit code `3`).
- **ABI v3** (`PluginApiVt3Ext.declare`) adds priority and platform declarations.
- Optional JSONL event journal (`RPI_EVENT_LOG=1`) with `rpi events tail` and
  `rpi events path`.

### Docs

- New `docs/remote-mode.md` and `docs/lifescope.md`.

## [0.1.23] - 2026-09-21

### Added

- Headless RPC mode behind the `--server` flag.

### Fixed

- `register_entrypoint` is now marked `unsafe fn`, making the host/plugin safety
  boundary explicit (breaking change for extension authors).
- Restored the render scheduler and reload error reporting.

## [0.1.22] - 2026-09-19

### Added

- `rpi dev-local` degrades gracefully to skills-only mode when no Cargo `cdylib`
  is present.

### Fixed

- Restore the editor state on abort so error text can no longer leak into the
  input box.
- Render Markdown content in the `ask_user` dialog.
- Correct error handling for local development mode without a Cargo `cdylib`.

## [0.1.21] - 2026-09-19

### Changed

- CI: removed a redundant `release.yml` that conflicted with the `ci.yml`
  release job; the crates.io publish now runs on pushes to `main` and supports
  trusted publishing.

### Docs

- Added repository status badges and improved repository discoverability.

## [0.1.20] - 2026-09-19

### Fixed

- Preserve steering messages in the active transcript when a blocking tool is
  cancelled, so queued input is consumed during tool-batch cleanup instead of
  waiting indefinitely for a future run.
- Wait for Windows process-tree termination and stop stdout/stderr readers on
  cancellation, preventing listener commands from hanging the TUI.

## [0.1.19] - 2026-09-16

### Fixed

- Soft-wrap long editor drafts inside the bordered input area.
- Preserve row/column after multiline file or command autocomplete replaces text.
- Clear stale autocomplete suggestions when selectors close.
- Keep regular-mode 405, authentication and network diagnostics visible.
- Sanitize provider error bodies so terminal control sequences and oversized
  responses cannot corrupt the layout.

## [0.1.18] - 2026-09-15

### Fixed

- Redraw only the actual changed line range, so status updates no longer rewrite
  the input editor and footer.
- Avoid all terminal output for unchanged frames, stopping background render
  ticks from pulling the native viewport back to the bottom.
- Track terminal height and cursor position precisely across renders and resizes.
- Clear removed tail rows without adding artificial scrollback lines.

## [0.1.17] - 2026-09-15

### Changed

- macOS default interactive sessions use regular/main-screen mode so the
  terminal owns both text selection and scrollback; Windows and Linux keep
  fullscreen as the default. `--tui-mode regular|fullscreen` overrides on every
  platform.

## [0.1.16] - 2026-09-15

### Fixed

- Reconcile the live assistant component from the harness's authoritative final
  message when prompt completion wins the race with asynchronous event delivery,
  preventing truncated response tails.
- Keep `/copy` output consistent with the rendered transcript.

## [0.1.15] - 2026-09-15

### Added

- `--timeout <seconds>` / `--timeout=<seconds>` CLI override with strict
  positive-integer validation.

### Changed

- Consistent 600-second default timeout for Anthropic Messages, OpenAI Chat
  Completions and OpenAI Responses, including streamed bodies.
- Mouse tracking is off by default on macOS so native text selection works;
  wheel behaviour on Windows and Linux is unchanged.

## [0.1.14] - 2026-09-14

### Added

- Retry progress in the TUI (retry number, budget, live backoff countdown) and
  structured `retry_scheduled` events in JSON mode.
- Retry transient provider failures up to 10 times per request by default;
  authentication, parameter, quota and billing failures stay terminal.

### Fixed

- Parse standard YAML block sequences in skill frontmatter, so lists such as
  `triggers:` are loaded instead of discarded.
- Preserve provider and abort diagnostics when a request ultimately fails.
- Removed the duplicate `Working...` footer status.

## [0.1.13] - 2026-09-14

### Added

- `rpi dev-local` and `rpi dev --local-only` load only resources from the current
  project and the active development extension, including project-local
  `.rpi/skills`, `.pi/skills`, configured project resource paths, and the
  extension's own resources. `/reload` matches the local-only scope.

## [0.1.12] - 2026-09-14

### Added

- Pi-compatible managed npm and Git package stores: structured `npmCommand`
  argv, npm aliases, trusted project settings, bounded subprocesses and
  conservative provenance checks.
- Independent rpi and package update checks at startup, with `PI_OFFLINE`
  support across startup and manual update commands.
- Resumable, clean-tree release workflow for the nine published crates.

### Changed

- `models.json` is read as an ordered provider/model catalog and native Pi's
  authenticated default-selection rules are applied without leaking first-party
  API keys into custom compatible providers.
- Git package updates are staged before atomically replacing the installed
  checkout; build or activation failures preserve the previous package.
- Self-updates are staged and validated before replacing the running executable
  on Unix and Windows.
- Install, update and uninstall of Rust-native extension artifacts share one
  mutation lock with rollback for ordinary filesystem and registry failures.

### Known limitation

- Rust-native multi-file artifact transactions recover from reported errors but
  keep no crash journal. Termination in the narrow interval between filesystem
  renames can require manual repair of the extension registry or artifact
  directory.

## [0.1.11] - 2026-09-13

### Added

- Image attachments and clipboard paste, native keybinding configuration, model
  and thinking controls, settings panels, external-editor drafts, session
  navigation and improved tool rendering in the TUI.
- CLI parity: `--list-models`, `--offline`, project trust controls, streamed JSON
  lifecycle events, and HTML/JSONL session export.

### Changed

- Default provider and model selection follows native Pi's precedence (saved
  defaults first, then known provider defaults) while preserving declaration
  order of custom providers.
- OpenAI Responses streaming handles sparse output indexes, reasoning and text
  phases, function/custom tool calls, terminal failures, incomplete responses,
  usage accounting, replay identifiers and malformed frames.
- Anthropic SSE decoding streams incrementally, preserves split UTF-8, surfaces
  transport failures and responds promptly to cancellation.
- Pi JavaScript/TypeScript packages are opt-in via `--enable-pi-packages`.

### Fixed

- The startup update checker no longer re-reads untrusted project package
  declarations; it uses the same trust-gated package set as the session.
- Rust 1.78 remains the supported minimum, verified with the 1.78 toolchain for
  both source APIs and the locked dependency graph.
- The published `rpi-cli` crate embeds its documentation so the crates.io tarball
  builds independently of the workspace.

## [0.1.10] - 2026-09-12

### Added

- `rpi dev` builds and watches a local Rust extension; `/reload` forces a fresh
  build without replacing a working extension when compilation fails.
- `rpi uninstall` and `rpi uninstall-pi`.
- Multiplexed Node extension transport allowing concurrent requests with
  out-of-order responses; tool and command cancellation propagates through
  `AbortSignal`.

### Fixed

- Provider context compatibility keeps Pi responses without an explicit `role`
  field usable in side threads.

> **Beta notice:** the Node/TypeScript extension bridge is an experimental
> compatibility feature. Use it for local evaluation only.

## [0.1.9] - 2026-09-10

### Added

- The welcome screen lists every active tool, including tools registered by
  runtime extensions, plus discovered skills. Empty skill sets render as
  `Skills (0) none`.

### Fixed

- Crate metadata now points at the correct GitHub repository.

## [0.1.8] - 2026-09-10

### Changed

- Session history, sharing and exports request entries in chronological order,
  matching the terminal transcript.
- The package catalog distinguishes core Rust libraries (`cargo add rpi-tools`)
  from loadable `cdylib` extensions (`rpi install rpi-todo`).

## [0.1.7] - 2026-09-09

### Added

- `rpi install <crate>` for crates.io `cdylib` extensions, with `--version`,
  `--path`, `--locked` and `--force`.
- Nine published extensions added to the website catalog: `rpi-mcp-adapter`,
  `rpi-web-access`, `rpi-subagents`, `rpi-background-tasks`, `rpi-lens`,
  `rpi-todo`, `rpi-codegraph`, `rpi-memory`, `rpi-token-usage`.

## [0.1.6] - 2026-09-09

### Added

- First public crate family: async streaming agent runtime; Anthropic Messages,
  OpenAI-compatible Chat Completions and faux providers; `read`, `write`, `edit`,
  `bash`, `grep`, `find`, `ls` tools; JSONL sessions, context compaction, hooks,
  queues and prompt templates; `rpi-plugin-sdk` with a stable ABI.

## [0.1.0] – [0.1.5]

Early crates.io publications while the workspace layout, provider layer and
agent loop were being established. See `git log` for details.

[Unreleased]: https://github.com/bigfish1913/pi-rust/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/bigfish1913/pi-rust/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/bigfish1913/pi-rust/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/bigfish1913/pi-rust/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/bigfish1913/pi-rust/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bigfish1913/pi-rust/compare/v0.1.28...v0.2.0
