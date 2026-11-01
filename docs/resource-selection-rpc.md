# Native RPC startup resource selection (stage A, v1)

`rpi --mode rpc --resource-config ./resources.json` opts **one native host** into
exact startup selection. Without this option, existing project/global/package
loading and the native plugin ABI remain unchanged. This is a host configuration
contract over existing `rpi package link` packages, Skills, and native cdylibs —
not a new executable package format or an installation manager.

## Configuration

```json
{
  "version": 1,
  "main_package": {
    "name": "my-agent",
    "instructions": ["prompts/agent-instructions.md"]
  },
  "skills": ["./shared-skills/review/SKILL.md"],
  "extensions": ["./build/helper.dll"],
  "instructions": ["./instructions/team.md"],
  "tools": { "mode": "only", "names": ["read", "echo"] }
}
```

* `version` (integer `1`) and `tools` are required. All objects reject unknown
  fields. Unsupported versions, invalid JSON/types, duplicate JSON fields,
  duplicate resource paths (including canonical aliases), duplicate tool names,
  unknown tools, missing files/packages, invalid Skills, name collisions, empty
  selected Skill sources, and failed native registration are hard startup errors.
  There is no fallback to discovery. `main_package` may be omitted or `null`;
  the three top-level path arrays default to empty. Only **one** main Package is
  possible, not an array of interchangeable packages.
* The configuration filename and top-level resource paths resolve against the
  **effective session cwd**, including native restored-session cwd adoption.
  Use absolute configuration paths when launching from another cwd. Config,
  instructions, and Skills are UTF-8 text. Config and each persistent instruction
  are limited to 64 KiB, read with a bounded reader. Exact-mode registry input is
  limited to 256 KiB using a bounded reader too.
* `main_package.name` resolves exactly against `<cwd>/.rpi/packages.json`.
  A missing or duplicate registry entry is an error. Existing package layout is
  retained: `.rpi/packages/<name>/skills` supplies Skills, and the registry's
  `artifacts` entries name files under that Package's `extensions` directory.
  Only those registered artifacts load; unregistered DLLs beside them do not.
  A resource-only Package with no native artifacts is supported. `.` and `..`
  are not package identities. Package roots must remain inside the canonical
  package collection; package artifacts, instructions and nested Skill paths
  (including symlinks/junctions) must stay inside their canonical Package root.
  Explicit standalone resources remain caller-authorized paths.
* `skills` explicitly adds Skill files or directories. The existing native
  Skills loader handles frontmatter, ignore files, and recursion. Explicit
  additions precede Package Skills using the existing first-registration-wins
  merge. In exact mode a collision diagnostic is a hard error, rather than a
  silent loss of a declared resource. Successful snapshots therefore have an
  empty `skillDiagnostics` array; failures are returned as startup errors.
* `extensions` explicitly adds native library **files**, not directories or Pi/JS
  packages. Use the platform's `.dll`, `.so`, or `.dylib`. Package artifacts load
  before these additions. Native tool registration and built-in override
  precedence are unchanged. Extension identity in observation is its native
  file stem plus canonical source path (the ABI does not require a separate
  manifest identity). Loading an extension is independent of activating its
  tools. Stdio-owning extensions are unsupported in this RPC host mode.
* `main_package.instructions` is an **explicit mapping** of Package-relative
  files to persistent instructions. Absolute paths, traversal, and symlink
  escapes from the Package are rejected. These files need not be templates;
  a file in `prompts/` executes persistently **only because this mapping selects
  it**, not because of its directory or extension. Top-level `instructions`
  supplies additional persistent files. Package instructions come first,
  followed by top-level instructions, preserving list order. They are appended
  to every native model system input, not inserted as fake user messages.
  Observation reports each canonical path, `package:<name>` or `explicit`
  provenance, and SHA-256, never the instruction content.

### Tools: registry versus actual model capabilities

```json
{"version":1,"tools":{"mode":"none"}}
```

This is a valid empty resource selection and advertises **zero model tools**.
The following modes are explicit:

| `tools` | Model-facing behavior |
| --- | --- |
| `{"mode":"default"}` | All native built-ins for this platform plus tools registered by the selected extensions. Ignores saved `defaultTools`; deterministic from this host's registry. |
| `{"mode":"only","names":["read","echo"]}` | Exactly these registered names; unknown/duplicate names error. |
| `{"mode":"only","names":[]}` | No tools, **not** the legacy empty-means-all sentinel. |
| `{"mode":"none"}` | No tools. Does not suppress extension loading/registration. |

Only model-effective tools get schemas in the model request. Registered tools
remain separately observable, including in `none` mode. No bogus tool name is
used to implement `none`. `set_active_tools` (including empty arrays), external
tool registration, resource/system-prompt replacement, and session hot switching
are rejected in exact mode. Restart with a new selection and a new session for
capability changes. The harness enforces this for plugin runtime actions too,
not only for RPC commands. Exact `none` also cannot expand through child-agent
inheritance. Legacy `set_active_tools([])` still means all **registered** tools
for execution, and the legacy `get_state.activeTools` / `get_available_tools`
wire values remain `[]`. Only exact-mode resource observation publishes the
normalized actual set; the legacy sentinel is not rewritten on the wire.

Skills are loaded independently of tool activation. A loaded Skill is listed in
the model system input only if an actual effective `read` tool exists and its
`disable-model-invocation` metadata does not hide it. `loadedSkills[].modelVisible`
reports precisely that gate. This also corrects legacy empty-array Skills
visibility, without changing legacy tool selection semantics.

## No implicit resource discovery

Exact mode disables automatic project/global/settings/other-Package Skills,
prompt templates, extensions, `SYSTEM.md`, `APPEND_SYSTEM.md`, and ancestor/global
project context (`AGENTS.md` / `CLAUDE.md`). `RPI_EXTENSIONS_DIR` is ignored.
`resources_discover` callbacks are **not called**, so even selected plugins
cannot reintroduce unselected resource paths through that discovery channel.
No prompt templates are registered in v1 exact mode. A prompt template is
on-demand text expansion, not a persistent instruction.

The native built-in base system prompt remains, rendered with only effective
tool guidance, followed by selected persistent instructions and the gated Skills
listing. Selected native extensions retain their normal executable capabilities
and ABI hooks/events/providers. They can execute code and contribute runtime
behavior; this is **not a plugin security sandbox**. Suppressing resource
discovery does not disable a selected plugin's tool implementation or lifecycle.

Model/provider credentials, model settings, retry/compaction settings, and
session-root resolution continue to use the normal native runtime. These are
not resource selection, and credentials are not exposed by resource observation.
Theme/UI installation or selection is not part of this RPC contract.

Conflicting CLI resource controls are rejected: `--system-prompt`,
`--append-system-prompt`, `--tools`, `--exclude-tools`, `--no-tools`,
`--no-builtin-tools`, `--skill`, `--prompt-template`, `--extension`,
`--extensions-dir`, `--no-skills`, `--no-prompt-templates`, `--no-context-files`,
`--no-extensions`, `--project-only` / `--package-only`, dev-local loading,
unknown extension flags, and explicit/unresolved negative project trust.
Use trusted `--mode rpc` (and `--approve` when necessary). Standard model,
session, timeout, and thinking options remain usable. An existing native session
requires the **same declaration and inventory** on restart. Resource changes
require a new session; `/reload` and dev hot apply return an error without
changing the existing exact selection, even if a caller modifies reload args.

## Read-only observation and readiness

Ready includes the normal native RPC capabilities and:

```json
{
  "resourceState": {
    "version": 1,
    "enabled": true,
    "command": "get_resource_state",
    "maxSnapshotBytes": 262144,
    "changes": "restart-required",
    "setActiveTools": "unsupported",
    "automaticPersistenceRecovery": false,
    "hotApply": false
  }
}
```

In exact mode this descriptor also carries observed `hostId`, `host`, native
`sessionId`, and declared `selection`; `configureExternalTools` is `unsupported`.
In legacy mode `enabled` is false and `setActiveTools` is `legacy`.
The command is advertised but returns an unsupported-mode error without resource
configuration. Clients must check `enabled`, not infer exact loading from a
command name alone.

```json
{"type":"get_resource_state","id":"resources-1","version":1}
```

`version` defaults to `1` if absent. Unknown request fields and unsupported
versions error. The standard native response envelope wraps `data` containing:

* `version`, unique per-host `hostId`, host name/version/PID, and **native**
  `sessionId` (unchanged when reopening the same transcript);
* typed declared `selection`, resolved `package` (name, recorded version, source,
  Cargo identity and canonical root; `null` without a main Package), and resolved
  `selectedSkills` input paths;
* `loadedSkills` with observed names, source file paths, `modelVisible`, and
  `skillDiagnostics` (see fail-fast policy above);
* `loadedExtensions` with successfully loaded/registered paths and identities;
* `registeredTools` from the actual harness registry and `modelEffectiveTools`
  from the same normalized execution snapshot used by the model/tool loop;
* instruction path/source/SHA-256 provenance, `baseSystemPrompt`,
  `projectContext`, `resourceDiscovery`, `promptTemplates`, and restart policy.

Loaded resources are observed from successful native loading and live harness
getters, not inferred from requested filenames. No complete system prompt,
Skill body, instruction content, credential, or private provider header is
included. Snapshots exceeding 256 KiB error rather than truncate. Native frames
remain limited to 1 MiB. This request additionally bounds its serialized `id` to
4096 bytes; oversized IDs error with no echoed ID, keeping even error frames
bounded. There is no pagination/truncated-success interpretation in v1.

## Native restart and history

Launch again with the **same** native `--session-id` (or native session path) and
`--resource-config`. Native JSONL history, branch identity, and tool results are
preserved. The declaration, resolved paths, instruction hashes, and observed
inventory (Skill/native artifact hashes and tool-schema digest) are stored using
native session values in one append, not a parallel transcript. Reopening checks
these; changed selection, added/removed/changed Skill files, changed instructions/artifacts or
registrations, and reopening an exact session without its config error instead
of widening. A legacy transcript may be explicitly bound to exact selection;
subsequent launches must honor it. After binding, resource changes require a new
session. Before any native extension registration or provider callback, startup
preflights the frozen declaration and artifact hashes through a **nonrepairing
in-memory snapshot** of the canonical native parser/reducer. Preflight never
claims a writer, creates session/lock files, or changes transcript bytes, even
with a recoverable torn tail or duplicate sequence. Accepted normal opens retain
native recovery under the existing writer lock (including legacy torn tails).
Rejected missing/different declarations cannot execute automatic or differently
selected native code. Dev compilation is also gated by preflight;
`dev`/`dev-local` with `--resource-config` is rejected before a build.

Public generic `append_entry` RPC and runtime actions cannot erase or forge
`rpi.resource-selection.*` / `rpi.resource-inventory.*` session values. Other
custom entries/user values remain supported, and the host's internal native
`SessionValueWriter` still persists its binding atomically. Trusted native code
and direct transcript edits are not a security sandbox promise.

Files are expected to remain stable while a host runs; this is not a
live filesystem watcher or an atomic installation transaction.

**Stage boundary:** this provides native restart verification and native
persistence only. Extcore automatic persistence/recovery, idle hot apply,
installation/download/update management, marketplaces, Work UI, and extcore
integration are **not delivered**. Installation management belongs to **Stage B**,
not this contract. Existing native interrupted-run handling is not replaced. An
integrator must retain the config and pass it at restart.

## Regression tests

With `CARGO_TARGET_DIR` set to the desired local cache, run:

```text
cargo test -p rpi-cli --test resource_selection --locked
cargo test -p rpi-cli --lib resource_selection --locked
cargo test -p rpi-harness --lib agent_harness::child::tests --locked
```

The native-process tests automatically build the real workspace `plugin-stub`
against the workspace SDK using `cargo build --lib --locked --offline` (never
recursive tests). Each build owns fresh isolated temporary target **and Cargo
build/intermediate directories**, disables incremental output, has a 120-second /
2-MiB output bound, and cleans up after native hosts exit.
There is no prebuilt/stale-DLL fallback or silent skip; concurrent test processes
cannot share loaded DLL paths. A build-only source/SDK digest in the real
registration side effect proves the fixture came from current source. Ordinary
`cargo test -p rpi-cli` and `cargo test --workspace` need no fixture preparation.

Tests run concurrent hosts in the same cwd with the same contaminated project and
global resources. A loopback-only deterministic OpenAI-compatible SSE server
checks actual system inputs and tool schemas, requests a real ABI `echo` tool
invocation, and verifies its result in the next model request. It also covers
none/empty-only/default, explicit extra extensions, same-session restart/history,
changed inventory rejection, malformed/unknown/missing/duplicate selection, and
legacy discovery/empty-array Skills visibility. No external model or user
credentials/resources are used.
