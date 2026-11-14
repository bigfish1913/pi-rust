# rpi-sandbox

Filesystem-only sandbox policy for `rpi`:

- read access everywhere;
- writes limited to an allowlist (project directory, temp directory, explicit
  configuration);
- `protected` rules win over `writable` rules;
- user approvals can widen writes at runtime (M2);
- platform capability probing (Linux Landlock today, other backends later).

Current status: **M1** for Linux. The crate models the policy, parses the
`sandbox` settings block, probes the platform, and installs a Landlock ruleset
(whole process, applied before the Tokio runtime starts). macOS Seatbelt and
Windows restricted tokens are later milestones. See
[`docs/architecture/fs-sandbox.md`](../../docs/architecture/fs-sandbox.md).
