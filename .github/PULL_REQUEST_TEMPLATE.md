<!--
Thanks for the PR. Keep the description focused on what changed and how you know
it works. Delete any section that does not apply.
Do not bump crate versions for a feature/fix PR; the automated Release PR manages
the shared workspace version and release changelog.
-->

## Summary

<!-- What does this change, and why? One or two sentences is fine. -->

## Affected crates

<!-- Tick everything the diff touches. -->

- [ ] `rpi-telemetry`
- [ ] `rpi-ai`
- [ ] `rpi-agent`
- [ ] `rpi-tools`
- [ ] `rpi-harness`
- [ ] `rpi-tui`
- [ ] `rpi-cli`
- [ ] `rpi-plugin-sdk`
- [ ] `rpi-extensions`
- [ ] docs / website / CI only

## Type of change

- [ ] Bug fix (non-breaking)
- [ ] New feature (non-breaking, additive)
- [ ] Breaking change to a published API
- [ ] Plugin ABI change
- [ ] Documentation or CI only

## How this was verified

<!--
Paste the commands you ran and the result. `cargo test --workspace --locked` is
the baseline; add whatever else is relevant (a new test name, a manual CLI
transcript, a session file that now recovers).
-->

```bash
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked
RUSTFLAGS="-D warnings" RUSTDOCFLAGS="-D warnings" cargo test --workspace --locked
```

<!-- PowerShell: set $env:RUSTFLAGS = "-D warnings" and
$env:RUSTDOCFLAGS = "-D warnings" before running the Cargo commands. -->

## Notes for reviewers

<!--
Call out anything that needs a second opinion: dependency direction, a public
API addition, a change to session persistence, the plugin ABI, or the security
boundary described in SECURITY.md.
-->

- [ ] I kept the `rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli`
      dependency direction (no new backwards edges).
- [ ] Compilation is warning-free (CI checks Linux and Windows with `-D warnings`).
- [ ] New behaviour has a test that runs offline (no live provider, no network).
- [ ] Public API additions have doc comments.

## Related issues

<!-- "Closes #123", "Refs #456". -->
