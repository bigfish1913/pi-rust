# rpi-sandbox

Filesystem-only sandbox policy for `rpi`:

- read access everywhere;
- writes limited to an allowlist (project directory, temp directory, explicit
  configuration);
- `protected` rules win over `writable` rules;
- user approvals can widen writes at runtime (M2);
- platform capability probing (Linux Landlock today, other backends later).

Current status: **M1 done, M2 in progress** for Linux. The crate models the
policy, parses the opt-in `sandbox` settings block, probes the platform, installs
a Landlock ruleset (whole process, applied before the Tokio runtime starts), and
ships the approval broker (protocol, approval store, audit log, client). macOS
Seatbelt and Windows restricted tokens are later milestones. See
[`docs/architecture/fs-sandbox.md`](../../docs/architecture/fs-sandbox.md).
