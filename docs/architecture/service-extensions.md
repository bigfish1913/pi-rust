# Model-free service extensions

A native stdio-owning extension may opt into service bootstrap with the existing
`declare` callback and JSON:

```json
{"ownsStdio":true,"requiresAgent":false}
```

The host prepares/registers extensions normally, enforces stdio ownership, then
dispatches BeforeTuiStart and SessionStart without resolving model credentials or
creating an AgentHarness/transcript. It awaits the existing StdioLifecycle finish
action, Ctrl+C or Unix SIGTERM, and dispatches SessionShutdown while keeping native libraries
and the action bridge alive. Agent-dependent actions remain unavailable until an
extension creates/owns its appropriate execution context.

This changes no plugin ABI structure, callback signature or ABI version. Only a
stdio owner explicitly declaring both fields in its own declaration can opt in.
Ordinary Agent extensions and owners omitting requiresAgent keep normal startup.
Extensions should declare service ownership only when their service CLI entry is
selected. A service requiring multiple project/session owners can compose that
runtime itself, then expose protocol adapters to the same owner.

Registry tests cover legacy defaults, non-owner isolation and registry merging.
The rpi HTTP extension's isolated smoke test additionally loads a real cdylib with
no model keys/configuration, checks authenticated node APIs, and verifies shutdown
without creating an unrelated native Agent session. This startup support is not
a claim that older published rpi binaries have been updated.
