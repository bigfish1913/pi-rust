# Contributing to rpi

Thanks for taking the time to contribute. This document covers the practical
details: how to build, what CI requires, and how to get a change merged.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Ways to contribute

- **Bug reports** — use the [bug report form](../../issues/new?template=bug_report.yml).
  A minimal reproduction (command, config, session file) is worth more than a
  long description.
- **Feature requests** — use the [feature request form](../../issues/new?template=feature_request.yml).
  Describe the problem first, then the proposed solution.
- **Extensions** — publish `cdylib` extensions in
  [`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package). Bugs in the
  ABI or the loader itself belong in this repository.
- **Documentation** — `README.md`, `docs/`, and the static site in `website/`
  are all fair game. Typos and broken links are welcome as direct PRs.
- **Code** — see below.

## Prerequisites

- Rust **1.88 or newer**. CI checks workspace targets and all extension features
  on 1.88.0 on Linux and Windows, in addition to the stable-toolchain checks.
  The default development toolchain is not pinned.
- A C toolchain for `rusqlite` (`bundled`) and, on Windows, the MSVC linker.
- Optional: [`task`](https://taskfile.dev/) for the release/deploy targets in
  `Taskfile.yml`. Ordinary builds do not need it.

No API key is required for the test suite: `rpi-ai` ships a deterministic
`faux` provider and the tests run offline.

## Build and test

```bash
cargo build --workspace --locked
cargo test  --workspace --locked
```

For the regression inventory, scenario-specific commands, CI coverage and optional
checks, see [Regression testing](docs/maintaining/testing.md). This includes
session/crash recovery, resource budgets and large-history cold-start gates.
Keep generated test reports, measurements and protocol logs outside Git. CI uses
owned synthetic fixtures and diagnostic-file allowlists, never real user histories
or credentials.

For changes that may affect the minimum supported Rust version:

```bash
rustup toolchain install 1.88.0 --profile minimal
cargo +1.88.0 check --workspace --all-targets --locked
cargo +1.88.0 check -p rpi-extensions --all-features --tests --locked
```

The minimum version matches the current locked dependencies, including
`image`, `time`, and ICU. Using a newer stable toolchain is supported.

Before opening a PR, run what CI enforces:

```bash
RUSTFLAGS="-D warnings" cargo check --workspace --all-targets --locked
RUSTFLAGS="-D warnings" RUSTDOCFLAGS="-D warnings" cargo test --workspace --locked
```

CI rejects compiler warnings on both Linux and Windows, including test and
example targets. The Linux test job also rejects rustdoc warnings. In
PowerShell, set `$env:RUSTFLAGS = "-D warnings"` and
`$env:RUSTDOCFLAGS = "-D warnings"` before running the Cargo commands.

Formatting: run `cargo fmt --all -- --check` before opening a PR. CI runs the
same check. The repo-wide formatting sweep is listed in
`.git-blame-ignore-revs`; to hide it in local blame, run:

```bash
git config blame.ignoreRevsFile .git-blame-ignore-revs
```

Clippy is not part of CI; it can be run locally for additional lint checks:

```bash
cargo clippy --workspace --all-targets --locked
```

Network-dependent tests are gated. Do not add a test that requires a live
provider to the default `cargo test` path.

## Repository layout

The workspace directories and the published crate names both use the `rpi-`
prefix (`crates/rpi-ai` ↔ `rpi-ai`). `package.name` in each `Cargo.toml` is the
crate name; the directory name is not part of the public API.

| Directory                          | Crate            |
| ---------------------------------- | ---------------- |
| `crates/rpi-telemetry`              | `rpi-telemetry`  |
| `crates/rpi-ai`                     | `rpi-ai`         |
| `crates/rpi-runtime`                | `rpi-runtime`    |
| `crates/rpi-agent`                  | `rpi-agent`      |
| `crates/rpi-tools`                  | `rpi-tools`      |
| `crates/rpi-harness`                | `rpi-harness`    |
| `crates/rpi-cli`                    | `rpi-cli`        |
| `crates/rpi-tui`                    | `rpi-tui`        |
| `crates/rpi-plugin-sdk`            | `rpi-plugin-sdk` |
| `crates/rpi-extensions`            | `rpi-extensions` |

Dependency direction is one-way and must stay that way:

```
rpi-telemetry → rpi-ai → rpi-agent → rpi-tools → rpi-harness → rpi-cli
```

`rpi-plugin-sdk` is a zero-dependency leaf so that plugins do not pull the
whole runtime; `rpi-extensions` sits on top of it. Adding a dependency that
points backwards (for example `rpi-agent` depending on `rpi-cli`) will be
rejected.

`docs/architecture/overview.md` explains the design intent behind the split;
`docs/agent/project-structure.md` is the recommended structure for projects built on top
of the crates.

## Coding guidelines

- Keep the public API of a crate additive within a minor version. Breaking
  changes to a published crate need a version bump and a `CHANGELOG.md` entry.
- Prefer the smallest change that solves the problem. Refactors that are not
  required by the fix belong in a separate PR.
- New behaviour needs a test. Prefer the in-memory execution environment and
  the faux provider over anything that touches the network or the user's real
  filesystem.
- Error messages should name the failing component and what the user can do
  next.
- Run `cargo fmt`. There is no separate style document beyond rustfmt defaults.
- Public items should carry a doc comment. `#![warn(missing_docs)]` is not
  enabled workspace-wide yet, so this is a review expectation rather than a
  build failure.

## Commit messages

The history follows [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(cli): add --timeout override
fix(tools): preserve @ when accepting file autocomplete
docs: sync website package data
chore: release v0.3.0
```

Keep the subject in the imperative mood and under ~72 characters. Chinese and
English subjects are both present in the history; either is acceptable, but
keep the type prefix and scope in the conventional form.

## Pull requests

1. Fork, branch from `main`, and keep the branch focused on one change.
2. Fill in the PR template — including which crates are affected and how you
   verified the change.
3. Make sure CI is green. `Cargo test` and both `Cargo check` jobs (Linux and
   Windows) must pass without warnings.
4. Expect review comments on API additions; the public surface of these crates
   is intended to be small and stable.

If your change touches the plugin ABI (`rpi-plugin-sdk`), call it out
explicitly in the PR description. ABI changes affect every already-built
extension and require a documented negotiation story.

If your change touches session persistence (`rpi-harness`), describe the
recovery behaviour for a crash in the middle of the affected operation.

## Releases

Releases are cut by a maintainer, not by contributors:

1. Ordinary PRs do not change versions. After changes accumulate on `main`, a
   maintainer manually runs **Actions → CI → Run workflow** on `main` with
   `prepare_release` enabled.
2. The workflow creates or updates the Release PR. After review, merging that
   Release PR is the only action that authorizes publication.
3. The main-branch CI publishes the crates, creates `vX.Y.Z`, builds the
   GitHub Release binaries, and updates the install channels.

Do not bump a version or publish a crate in a PR. The local `task publish` command
remains available for explicit manual recovery, but should not be run concurrently
with the GitHub release workflow.

## Reporting security issues

Do not open a public issue. See [SECURITY.md](SECURITY.md).
