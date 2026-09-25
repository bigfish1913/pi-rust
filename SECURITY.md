# Security Policy

## Supported versions

Security fixes are issued for the latest published version of each `rpi-*`
crate. The workspace releases all nine crates together, so the version you see
on crates.io is the supported one.

| Version        | Supported |
| -------------- | --------- |
| Latest release | ✅        |
| Anything older | ❌ (upgrade, then re-test) |

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report privately through GitHub's
[private vulnerability reporting](../../security/advisories/new)
(Security → Advisories → Report a vulnerability). If that form is unavailable,
email the maintainer listed in the repository's `Cargo.toml` author field.

Please include:

- The affected crate(s) and version.
- A description of the impact and who is exposed.
- A reproduction (command line, config, and a minimal session or extension if
  relevant).
- Whether the issue is already public anywhere.

We aim to acknowledge a report within a few days and to agree with you on a
disclosure timeline before publishing details. Credit is given in the advisory
unless you ask to stay anonymous.

## Threat model and scope

`rpi` is a coding agent that runs with the privileges of the user who starts
it. The following areas are treated as security-relevant:

- **Plugin loading.** `rpi-plugin-sdk` defines a `#[repr(C)]` ABI and
  `rpi-extensions` loads `cdylib`s into the host process with `libloading`.
  A plugin is **trusted native code** — it can do anything the host can. There
  is no sandbox. Loading a plugin from an untrusted source is equivalent to
  running an arbitrary binary.
- **Remote mode.** `rpi --server` exposes a JSONL agent protocol over TCP.
  Token authentication is connection-level. Do not bind it to a public
  interface without a token and a trusted network path; the protocol has no
  transport encryption of its own.
- **Tool execution.** The `bash` and `powershell` tools execute arbitrary
  commands, and the file tools read and write paths with the user's
  permissions. Project trust prompts and the settings that gate them are part
  of the security boundary.
- **Credentials.** `~/.rpi/agent/auth.json` is written with `0o600` on Unix.
  Anything that logs, prints or transmits that file's contents is a
  vulnerability.
- **Untrusted input handling.** Provider error bodies and model output are
  sanitized before they reach the terminal, because terminal control sequences
  can corrupt or spoof the TUI. Regressions here are in scope.
- **Package installation.** `rpi install` builds and installs a crate with
  Cargo. Build scripts in a third-party extension execute arbitrary code, so
  the provenance and trust checks around installation are in scope.

## Out of scope

- Vulnerabilities in the LLM provider or model itself.
- Prompt injection that only causes the agent to do what the user already
  permitted it to do (for example, an instruction in a repository file that
  makes the agent edit another file in the same repository).
- Anything that requires an attacker to already have local code execution as
  the user.
- Missing hardening on a server the user chose to expose publicly without
  authentication, when the documentation warns against it.

## Hardening recommendations

- Run the CLI on a checkout you trust, and review `.rpi/` / `.pi/` resources
  before granting a project trust.
- Prefer `--server` with a token on a private network or behind an SSH tunnel.
- Install extensions from a source you trust, and pin `--version`.
- Keep `~/.rpi/agent/auth.json` out of version control and out of shared
  session directories.
