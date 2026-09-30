# Releasing rpi

One command cuts a release and ships it to every channel:

```bash
task publish                 # auto-increments the version (patch)
task publish RELEASE_BUMP=minor
task publish RELEASE_VERSION=0.4.0
task release RELEASE_VERSION=0.4.0   # identical, explicit-version spelling
```

`task publish` resolves the version with [`scripts/next-version.sh`](../scripts/next-version.sh)
and then runs `scripts/release.sh`, which walks the pipeline in order,
**printing each step before it runs**. Review the plan first with:

```bash
task publish -- --dry-run
# or directly:
bash scripts/release.sh 0.4.0 --dry-run
```

### Which version does `task publish` pick?

`RELEASE_VERSION` wins when you pass it. Otherwise the version is derived from
the highest released `vX.Y.Z` git tag, incremented per `RELEASE_BUMP`
(`patch` by default, or `minor` / `major`).

One exception: if the workspace version in `Cargo.toml` is **already ahead** of
every tag, it is reused verbatim. That state means a bump was prepared but has
not shipped, so incrementing again would skip a version. This is also how you
resume an interrupted release:

```bash
# 0.3.8 was bumped/tagged but the push failed — keep 0.3.8, do not go to 0.3.9
task publish RELEASE_BUMP=none -- --from push --yes
# equivalently
task publish RELEASE_VERSION=0.3.8 -- --from push --yes
```

Anything after `--` is forwarded to `scripts/release.sh`, so the phase flags
below are available through `task publish` too.

## The pipeline

| Phase | Does |
| --- | --- |
| `preflight` | clean tree, tag unused, version differs |
| `bump` | `Cargo.toml` workspace version + internal pins → `cargo update --workspace` |
| `changelog` | cuts `## [Unreleased]` into `## [X.Y.Z] - <date>` |
| `docs` | re-syncs `crates/rpi-cli/embedded-docs` (CI enforces it) |
| `validate` | `task release:validate` (version consistency across the nine crates) |
| `commit` | `chore(release): X.Y.Z` |
| `tag` | `git tag vX.Y.Z` |
| `push` | pushes the branch + tag → `release-binaries.yml` builds the assets |
| `wait` | polls the GitHub release until every archive + `.sha256` exists |
| `channels` | rewrites Homebrew / Scoop / winget / site data (`scripts/refresh-channels.sh`) |
| `commit-channels` | `chore(release): refresh the install channels and the site for X.Y.Z` |
| `taps` | pushes the formula/manifest into `homebrew-tap` + `scoop-bucket` |
| `winget` | opens the `microsoft/winget-pkgs` PR (`scripts/winget-submit.sh`) |
| `crates` | `task publish:crates RELEASE_VERSION=X.Y.Z` (nine crates, dependency order) |
| `site` | `task rpi-deploy` |

## Flags

| Flag | Effect |
| --- | --- |
| `--dry-run` | print the plan, execute nothing |
| `--yes` | do not stop at the confirmation checkpoints (`crates`, `site`) |
| `--only a,b` | run just these phases |
| `--from P` | start at phase `P` |
| `--skip a,b` | skip these phases |
| `--date YYYY-MM-DD` | changelog/`ReleaseDate` date (default: today, UTC) |

Recovery is just a re-run from the failed phase — every phase is idempotent:

```bash
# CI needed longer than the poll window:
task publish RELEASE_BUMP=none -- --from channels
# channels are live; only crates.io + site left:
task publish RELEASE_VERSION=0.3.4 -- --only crates,site
```

Crates.io only, no bump/tag/push — for republishing a version whose tree is
already prepared (it defaults to the `Cargo.toml` version):

```bash
task publish:crates
task publish:crates RELEASE_VERSION=0.3.4
```

## Credentials

| Phase | Needs |
| --- | --- |
| `push`, `taps` | git/`gh` write access to `pi-rust`, `homebrew-tap`, `scoop-bucket` |
| `wait`, `channels` | `gh` authenticated (reads release assets) |
| `winget` | `gh` authenticated (forks `winget-pkgs`, opens the PR) |
| `crates` | `cargo login` (a publish-scoped token) |
| `site` | SSH to the rpi web host |

## What no command can finish

- **winget**: `scripts/winget-submit.sh` only *opens* the PR. A winget-team
  moderator merges it, which is why `packaging/README.md` marks the channel
  `submitted` until then.
- **CLA**: if the policy bot asks, comment `@microsoft-github-policy-service
  agree` at **column 0** with nothing else on the line (a leading space silently
  fails the match).

## The channel sources of truth

`packaging/{homebrew,scoop,winget}` are the originals; the tap/bucket repos are
copies. `scripts/refresh-channels.sh` rewrites them from the **real** release
`.sha256` sidecars — never a template — so a manifest always points at a real
asset carrying its real checksum.
