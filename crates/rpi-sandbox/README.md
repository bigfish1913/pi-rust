# rpi-sandbox

Filesystem-only sandbox policy for `rpi`:

- read access everywhere;
- writes limited to an allowlist (project directory, temp directory, explicit
  configuration);
- `protected` rules win over `writable` rules;
- user approvals can widen writes at runtime (M2);
- platform capability probing (Linux Landlock today, other backends later).

Current status: **M0**. This crate models the policy, parses the `sandbox`
settings block and probes the platform. It does not enforce anything yet;
enforcement lands in M1 (Linux Landlock) and later milestones. See
[`docs/architecture/fs-sandbox.md`](../../docs/architecture/fs-sandbox.md).
