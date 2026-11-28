# Package and local snapshot management

`rpi package` installs published agent packages and local package snapshots.
`rpi skill` and `rpi extension` manage standalone local snapshots. Local
installation copies existing artifacts or directories without contacting a network,
running Cargo, registering native code, or changing a running agent. Use
`rpi package link` or `rpi install` with `--path` or `--build` to compile
Rust extensions.

## Published agent packages

```sh
rpi package install NAME [--version VERSION | --manifest URL]
rpi package update NAME --scope global [--version VERSION | --manifest URL]
rpi package inspect NAME --scope global
rpi package remove NAME --scope global
```

The installer resolves the release, verifies and extracts its archive, and
validates the packaged agent manifest before publishing it as a global snapshot.
Published updates require an existing published installation. Use `--manifest`
again when updating from a custom release URL.

## Commands and observations

```sh
rpi package list --scope project|global
rpi package inspect NAME --scope project|global
rpi package install NAME --path PATH --scope project|global [--version TEXT]
rpi package update NAME --path PATH --scope project|global [--version TEXT]
rpi package reinstall NAME --path PATH --scope project|global [--version TEXT]
rpi package remove NAME --scope project|global
rpi package recover --scope project|global
```

Use `list`, `inspect`, `install`, `update`, `reinstall`, and `remove` with
`rpi skill` or `rpi extension` to manage snapshots of that kind.
`rpi package recover` handles incomplete transactions for all three kinds in
the selected scope. Scoped snapshot commands return bounded JSON v1, including
errors. Names are portable ASCII identifiers. The optional local version is a
label supplied by the caller.
Observations include source, scope-qualified identity, observed SHA-256 inventory,
selectable paths, diagnostics, ownership, and lease-based in-use status. In-use is
not loaded/applied: only stage A's live resource state can establish those facts.
Inspection is read-only and never runs Cargo or opens a native library. Unmanaged
native files are reported but are not adopted, overwritten, or removed.
Success envelopes carry `operation`, `scope`, `kind`, `name`, and
`data.outcome`. List results have a null `name`. Recovery results have a
null `kind` and `name`. Errors have
`success:false` and bounded diagnostics, with parsed request context when valid.
Limits per scope/request: 128 owned resources, 256 KiB metadata/result, 2048
inventory entries, depth 32, 64 MiB per file and 256 MiB total observed bytes.
Catalog shares the entry/byte budget across all owned and unmanaged observations.
Install is idempotent for identical source/version/inventory; changing an existing
owned install requires update/reinstall. Removing an absent identity is a no-op;
an unmanaged destination is never treated as a removable install.

## Ownership and storage

Project root: `<cwd>/.rpi`; global root: the existing native agent directory
(`RPI_CODING_AGENT_DIR` in isolated tests). Packages keep `packages/NAME/{extensions,
skills,prompts,themes,...}` and native `packages.json`; standalone Skills keep
`skills/NAME`, standalone libraries `extensions/NAME.<platform suffix>`.
`resource-receipts.json` supplements the native registries with ownership, local
source provenance, optional declared version, and copied inventory. It is host
metadata, never a required source manifest. Local bundles are immutable copied
snapshots; links/reparse points, special files, escapes, unmanaged destinations,
corrupt metadata, unknown native Package metadata fields, and excessive inputs
are rejected rather than reset or silently dropped. Explicit source paths may
contain `..`: each addressed ancestor is checked before normalization, so it
cannot hide a traversed link/junction. Owned relative targets/receipts continue
to reject traversal. Source/target overlap is rejected using canonical path
identity, including Windows extended paths and case aliases. Packages copy their complete
bounded directory, including explicit instruction files; prompts are not implicitly
persistent instructions. On Unix, copied files retain ordinary rwx permissions
(including executable helper scripts), but never setuid, setgid, or sticky bits.
Removal verifies the recorded inventory before deleting.

## Publication, leases and failure

A persistent `.resource-lease` OS advisory lock per scope is shared by native
hosts and exclusive for mutations (including overlapping old installer/package
commands). Leases are acquired before validation/loading and retained through
host lifetime, including library keepalives. Actual asynchronous library cleanup
retains the lease until API/library teardown is complete. If safe unload cannot be
established, the library and lease are deliberately retained until process exit.
They are intentionally coarse: a host
protects both native roots, not just selected paths. Multiple readers coexist;
crashes release OS locks. Lock files are never unlinked. Catalog may probe an
existing lease without creating it. Explicit unmanaged paths outside these roots,
older hosts, external filesystem writers and embedded SDK loaders bypass this
protocol; no protection is claimed for them.

Host startup creates missing native roots and persistent lease files even for
`--no-extensions`, an empty exact selection, or external-only selections. Thus
startup now needs writable project/global native roots; there is no read-only-cwd
fallback that bypasses exclusion. This orchestration write is separate from pure
selection/session fact readers: rejected exact startup does not repair or rewrite
session bytes/metadata. Catalog/inspect never creates roots or locks, retains a
noncreating observation lease when present, and returns busy/retry rather than
exposing a mutator's half-publication. If the first persistent lock appears during
an initially lock-free observation, that observation is rejected for retry.

The facade stages snapshots and metadata on the destination filesystem and uses
rename/backup publication under the exclusive lease, following the existing
native install transaction pattern. A bounded rollback journal precedes activation.
Failure restores old bytes and receipts; a crash leaves a journal and blocks new
host startup until `rpi package recover` rolls it back under the same exclusion.
Next-state metadata and the full success envelope are bounded before publication;
post-commit backup cleanup failure remains a committed success with a bounded
`diagnostics` entry, not a false claim that the install failed. Orphan backups
from a committed cleanup failure are not active installs or auto-adopted resources.
Recovery validates journal targets/digests and refuses altered content rather than
removing it. Catalog diagnoses incomplete transactions without repairing them.
No cross-filesystem atomicity, power-loss fsync guarantee, or rollback of arbitrary
external code execution is claimed. Existing old installer transactions retain
their existing rollback/crash semantics, but share lease exclusion.

## Stage A and compatibility

Exact v1 `main_package` adds optional `scope: "global" | "project"`; absence
continues to mean project with no global fallback. Native global packages resolve
through the global native `packages.json`, without copying or settings changes.
Standalone receipts expose real paths usable in `skills` / `extensions` arrays.
Persistent instructions remain the explicit `main_package.instructions` mapping.
Legacy project/global Skill precedence and existing discovery remain deliberate;
global Packages are selected explicitly, not injected into legacy discovery.
Use scoped commands to replace or remove managed snapshots. Automatic `extension update`
(and its `package update` alias) skips facade-owned Packages; explicitly naming one
gives resource-update guidance rather than invoking Cargo. Legacy Package writes
check the addressed directory, extensions and artifact/staging paths; aliases and
pre-existing staging files are refused rather than followed or truncated. Safe
legacy registry names such as `_legacy` and `.tools` remain listable/inspectable;
this does not widen the identity grammar for new managed resources. Resource
mutations affect future startups only. Exact-session declaration/inventory checks remain unchanged;
no transcript migration, task stopping, unload, or live capability widening occurs.

For example, install a resource-only or combined bundle globally:

```sh
rpi package install demo --path ../bundle --scope global
rpi package inspect demo --scope global
```

Then start RPC with this caller-authored resource configuration (persistent
instructions are deliberately explicit):

```json
{
  "version": 1,
  "main_package": {
    "name": "demo",
    "scope": "global",
    "instructions": ["prompts/agent-instructions.md"]
  },
  "tools": { "mode": "only", "names": ["read", "echo"] }
}
```

Use only instruction files and registered tool names that actually exist in your
bundle; missing files/tools are hard startup errors. Installation itself never
selects or applies this configuration.
