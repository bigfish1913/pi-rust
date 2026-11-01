# Explicit native resource transition (integration contract)

## Safe capability negotiation (before any resource operation)

1. Run **only** `rpi --help`. Require the advertised `--resource-capabilities`
   option. An older host without it is **Unsupported**: do not probe unknown
   resource commands/flags, because old CLIs can interpret them as prompt text.
2. Run **only** `rpi --resource-capabilities`. Require a successful, parseable
   JSON object with `version: 1`, `type: "resource_capabilities"`, the needed
   exact-selection/management/transition versions and operations. Reject unknown
   versions, missing support or output over **16 KiB**; never infer support from
   a binary version string or fall back to model execution.
3. This pure static profile reads no trust/settings/credential/provider/resource/
   session files, claims no lease, registers no DLL and reads no stdin. It is
   intercepted before normal CLI startup, including subcommands/remote clients.
   Combining it with any other argument returns bounded JSON usage error, never
   executes that argument. `--help` itself performs no native host startup.
4. `exactSelection` describes the v1 startup flag, tools modes, scopes and
   64-KiB configuration / 256-KiB observation bounds. `management` describes
   local `resource` kinds (`package`, `skill`, `extension`), explicit project/
   global scopes, list/inspect/install/update/reinstall/remove/recover and their
   native bounds. `transition` describes its v1 request fields, expected state,
   target schema, operation ID constraint and 64-KiB request bound.
   `loadedFacts: false` is deliberate: the profile makes **no installed resource
   or ABI compatibility claim**. Verify actual configured startup ready/state.

## Controlled startup request

A trusted RPC caller may restart an **existing** native session with
`--resource-transition <file>` instead of `--resource-config`. The UTF-8 JSON
file is bounded to 64 KiB, rejects unknown/duplicate fields, and contains:

```json
{
  "version": 1,
  "operation_id": "caller-stable-id",
  "expected": {"revision": 1, "fingerprint": "sha256-from-native-state"},
  "target": {"version": 1, "tools": {"mode": "none"}}
}
```

The complete target uses the existing v1 exact selection schema and resolver,
including exact default/only/none semantics. No legacy target exists. An
explicit `expected: {"revision": 0, "fingerprint": null}` permits legacy to
exact binding. Existing v1 bindings lacking revision fields are revision 1;
their fingerprint is SHA-256 of the canonical native declaration + inventory.
No ordinary reopen migrates or repairs a resource binding.

Operation IDs are 1–128 ASCII alphanumeric/dot/dash/underscore characters. Native
storage associates each successful ID with its target and resulting binding.
Same ID and same target may reopen only while that operation's resulting
binding is still current and loaded inventory is unchanged. ID reuse with a
different target, superseded retry, or stale expected binding is a conflict.
An operation is not an authorization credential. Retry must retain the original
`expected` as well as operation ID and target. The complete native binding and
successful-operation ledger are bounded to 256 KiB; exhaustion fails before the
append and does not prune associations or silently reset history. Core must treat
the native fingerprint as opaque (it covers typed declaration and actual inventory,
not instruction contents in the response); revision increments even for a new
operation selecting the identical inventory.

Transition startup requires an existing native session with durable progress
already declared, and rejects fork, no-session, `--name`, initial prompt messages,
resource-config and conflicting legacy resource CLI flags. A historical native
session without durable progress must first be opened normally with its original
configuration, using existing native progress-upgrade semantics. That open does
not alter its resource binding. Transition reads themselves never upgrade it.

Read-only preflight checks the expected binding, operation identity and native
pending execution before any executable registration. Transition startup claims
the canonical session writer and rechecks facts before registration. It rejects
history requiring recovery and pending native work rather than repairing it.
This includes open operations on any lane, unsettled writes/tool frames,
undelivered queues, resumable branches and pending native progress. Branch,
queue, recovery and progress read errors fail closed. Historical inline assistant
payloads remain cold; strict facts readers retain no file/storage capability.
Torn/duplicate history and incomplete progress are rejected byte-for-byte without
repair; legacy ordinary reopen retains its existing recovery behavior.
Host construction validates actual resources, ABI registration and tools before
one native append publishes declaration, observed inventory, revision and
operation association together. A second CAS under writer ownership precedes
commit. Candidate validation/host-construction failures before the append never
publish partial binding facts. History and native session ID remain unchanged.
Existing strict ordinary reopen remains in force. Public append APIs cannot forge
resource binding/operation facts.

### Commit uncertainty and deliberate recovery

A failed append/flush or lost startup result is **not proof of rollback**. Native
storage attempts to cut back its own failed append, but an uncertain disk error
may leave the complete new single binding record or a torn tail. The valid prefix
always contains the prior messages and complete old binding; declaration,
inventory, revision and operation association are never separately published.
No power-loss/fsync or transactional rollback of arbitrary native code is claimed.

Core must keep its gate blocked after an uncertain result. Retry the **identical**
operation, original expected binding and target: a complete new record is validated
as a current retry (no second revision/append), while a clean old binding may apply
once. If strict startup rejects damaged history/progress, do not repair it through
transition startup or automatically fall back to an old config. Require deliberate
native recovery/inspection under the canonical writer lock. Existing ordinary
reopen recovery can be deliberately invoked with the appropriate original or
committed config after establishing persisted facts; it is not an automatic core
rollback policy. A valid complete new record requires its new configuration, not
the old one. Never authorize execution from an application operation ACK alone.

Ready and `get_resource_state` advertise `transition` support and authoritative
`binding` (`revision`, `fingerprint`, `operationId`). These facts describe what
this native host actually loaded, not a caller ACK. Observations remain bounded
and omit instruction bodies and secrets. Resource OS leases outlive DLL cleanup.

Extcore owns authorization, application operation records and a shared execution/
configuration gate. Before launch it must claim that gate, confirm fully idle
(including dialogs, compaction, cancellation and recovery), persist its operation,
and await old child exit. Pass the same native session ID and this request on
restart; read authoritative ready/state before reopening execution. A pre-commit
candidate failure leaves the old binding available; an append error or lost result
instead has the commit-uncertainty rules above. A crash after a complete commit is
resolved by retrying the same operation, not by assuming application failure means
native rollback. Native rejection of persisted pending work is not proof that all
application-level gates are idle.
No hot unload, transcript copying, ID replacement or model prompt injection is
part of this protocol.
