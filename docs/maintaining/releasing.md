# Releasing rpi

## Normal releases: daily preparation and automatic merge

Contributors, including fork contributors, submit code and tests without changing
versions. Every day at 00:00 Asia/Shanghai (16:00 UTC), the **CI** workflow runs
against `main`. If `release-plz` finds publishable package changes since the last
stable release, it creates or updates a draft Release PR. It prepares the version
and root changelog, synchronizes the embedded documentation, validates the core
crates, and marks the PR ready. The shared workspace version and `rpi` version
group keep the core crates together; examples are excluded.

The automation requests GitHub Auto-merge **only for this upstream
`release-plz-*` Release PR**. It never enables or requests Auto-merge for ordinary
feature, fix, dependency, or fork PRs. `main` remains protected by its Ruleset;
the Release PR is merged only after the required validation checks pass. No
publishable package changes means no Release PR, tag, or release.

Only same-repository `release-plz-*` draft PRs targeting `main` defer CI until
preparation is complete; ordinary PRs (including drafts and fork PRs) still run
all checks. Before updating an existing Release PR, automation returns it to
draft so intermediate version/changelog commits are not checked prematurely.
After documentation synchronization and validation succeed, marking the PR ready
triggers full CI on its final head, even when a retry produces no new commit.
Skipped draft jobs use distinct names, so they cannot satisfy the required
`Cargo check` / `Cargo test` checks while the ready-event run is being scheduled.
A failed preparation leaves the PR in draft; fix the failure and rerun preparation
rather than marking an incomplete Release PR ready manually. The embedded-docs
consistency check and publishing authorization remain unchanged.

Automatic releases use the version proposed by release-plz without pinning a
minor series or rewriting it to a patch release. Versions such as `0.9.0` and
`1.0.0` may proceed through the same Release PR workflow. The publish gate still
requires a stable version newer than existing release tags, or an identical-commit
retry. API compatibility findings remain visible in the Release PR.

An ordinary merge runs CI and the next daily run prepares a Release PR. To
prepare one immediately instead, use **Actions → CI → Run workflow**, select
`main`, and leave `prepare_release` enabled. The manual dispatch does not request
Auto-merge. After the automatically merged Release PR reaches `main` and its
main-branch CI passes:

1. The existing release helper publishes the core crates in dependency order.
2. Automation creates `vX.Y.Z` at the tested main commit. Existing tags are never
   moved; retrying the same release commit is supported.
3. A reusable workflow builds all six platforms (including the static
   `x86_64-unknown-linux-musl` Linux binary), verifies the archives, uploads
   their checksums and publishes `rpi-release.json` last.
4. Homebrew and Scoop are updated independently; a new-version PR is submitted
   to `microsoft/winget-pkgs`. Microsoft review/CLA completion remains external.

No release runs in fork repositories or on PR events. Publication additionally
requires a merged PR from an upstream `release-plz-*` branch whose merge commit
matches the tested main commit. A fork branch with that name does not qualify.
Do not use `pull_request_target` to execute contribution code with release secrets.

### Preview releases

A preview merge on the `preview` branch runs only the prebuilt binary workflow. It creates a
GitHub prerelease with a `vX.Y.Z-preview.N` tag and does **not** run release-plz, publish any
crate to crates.io, or update Homebrew/Scoop/winget channels. The preview binary is available
through the explicit `rpi update preview` command; normal stable update checks never select a
prerelease. A preview binary still checks crates.io for the stable line and therefore tells the
user to run `rpi update` when a formal release is available.


A documentation update does not require a version bump. Keep `Cargo.toml`,
`Cargo.lock`, existing tags and install-channel manifests unchanged, and do not
run `task publish` or merge a Release PR solely to publish documentation. Update
the relevant `docs/rpi-tool/` sources and run `sh scripts/sync-embedded-docs.sh`
when the bundled documentation changes.

The Release PR merge remains the only action that authorizes publication. If the
explicit requirement is to avoid release
automation entirely for a documentation-only change, maintainers can include
`[skip ci]` in the final merge commit message (or the squash commit message).
GitHub then skips the push/PR workflows, including the CI job that calls release
automation. Run documentation checks locally first; do not use this shortcut for
code changes or to bypass required checks. Skip directives leave required checks
pending, so repositories that require them must use their normal reviewed merge
process instead. They do not disable manually dispatched workflows.

Website deployment is separate from merging documentation and is not implied by
a documentation-only merge.

### One-time setup

The repository already uses `CARGO_REGISTRY_TOKEN` for crates.io. Add one secret,
`RELEASE_AUTOMATION_TOKEN`, for preparing PRs, refreshing README image dates via
an ordinary PR, and writing the external channels.
Use a maintainer or dedicated bot **classic PAT with `public_repo`**, with access
to these public repositories:

- `bigfish1913/pi-rust`: create/update Release PRs. Using a user token also lets
  those PRs and the documentation follow-up commit trigger their normal CI.
- `bigfish1913/homebrew-tap` and `bigfish1913/scoop-bucket`: update channel files.
- The token owner's fork of `microsoft/winget-pkgs`: create/sync the fork, push
  manifest branches, and submit upstream PRs. The upstream repository belongs to
  Microsoft; an own-repositories-only fine-grained token is not a replacement
  for this public contribution permission. See [GitHub's token limitations](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens#fine-grained-personal-access-tokens-limitations).

Set it in repository Settings → Secrets and variables → Actions, or locally with
`gh secret set RELEASE_AUTOMATION_TOKEN --repo bigfish1913/pi-rust` (interactive
input). Never put the token in a commit or PR description. The publishing jobs
use the built-in `GITHUB_TOKEN` for this repository and the Cargo secret only in
the crates publishing step. The automation token is only supplied to the steps
that prepare PRs (including the README image refresh), request Auto-merge for the
Release PR, or write channels.

Repository Settings must have **Allow auto-merge** enabled. The `main` Ruleset
continues to protect the branch and must require the `Release PR validation`
check, which aggregates the complete Release PR CI. Auto-merge is requested only
when the PR head is an upstream `release-plz-*` branch; ordinary and fork PRs
remain manual.

The binaries workflow is called directly after tag creation: a tag created with
the built-in `GITHUB_TOKEN` does not start another push-triggered workflow.

### Workflow reliability and required checks

The required `Release PR validation` check aggregates every validation dependency
for ordinary PRs as well as ready Release PRs. A resource, platform or extension
compatibility failure must fail that check; it is not an advisory result. Only
upstream draft Release PRs defer validation while being prepared, and their
separate Cargo check/test names cannot satisfy the normal branch requirements.

Linux x86_64 release and startup-smoke runners use `ubuntu-24.04`, keeping the
release glibc baseline explicit. The historical resource benchmark also uses
`ubuntu-24.04`: moving `ubuntu-latest` must not silently change the environment
under an absolute latency budget. Normal check/test jobs retain `ubuntu-latest`
coverage. Resource diagnostics include the OS, runner image, CPU and compiler;
the modern/legacy budgets and complete 701000-frame workload remain enforced.
A pinned runner reduces environment drift, not all hosted-runner contention: a
repeatable failure on that runner still needs profiling, not a threshold bump.

Both musl setup and extension native-dependency setup bound apt calls and retry
failures. Partial index/DNS errors must fail `apt-get update` instead of allowing
stale indexes to look successful. An existing musl compiler avoids apt access,
but must still link and run a static probe. Release setup stays inline because
recovery checks out an old tag that cannot contain a newly added helper script.

The daily README image refresh updates `automation/readme-images` and creates or
reuses an ordinary PR. It never pushes directly to protected `main`, requests
a protection bypass, or uses `[skip ci]`. It reuses `RELEASE_AUTOMATION_TOKEN`
because PRs created with `GITHUB_TOKEN` do not trigger normal CI. Missing token
or API/push failure fails visibly. The PR still needs normal checks and a manual
merge; only Release PRs receive automatic merge requests from repository bots.

### Recovery and release records

- Re-run failed jobs on the Release PR's original main CI run. Existing crates
  are skipped by the release helper; an existing tag must match the same commit.
- A re-run reuses the workflow file from the original commit, so it cannot pick
  up an automation fix. When the binaries job was blocked by infrastructure (a
  failed apt install, for example), dispatch **Release binaries** on `main`
  with the already-published `tag` instead: the attach step updates an existing
  release in place, and the tag and crates.io versions are left untouched.
- For a channel failure, run **Release install channels** from Actions on `main`
  with the published tag. It verifies all six archives before regenerating
  manifests. Existing winget PRs/files are reused; closed submissions require
  maintainer inspection. Tap updates refuse to downgrade a newer version.
- Each run saves `channel-manifests-vX.Y.Z` as an artifact. The checked-in
  `packaging/` files are the baseline; generated manifests carry the actual
  published version/checksums. No bot commits directly to protected `main`.
- Website deployment remains the separate `task rpi-deploy` operation.

## Manual release and recovery tools

The existing local entry point remains available:

```bash
task publish                 # auto-increments the version (patch)
task publish RELEASE_BUMP=minor
task publish RELEASE_VERSION=0.4.0
task release RELEASE_VERSION=0.4.0   # identical, explicit-version spelling
```

`task publish` resolves the version with [`scripts/next-version.sh`](../../scripts/next-version.sh)
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
| `validate` | `task release:validate` (version consistency across the core crates) |
| `commit` | `chore(release): X.Y.Z` |
| `tag` | `git tag vX.Y.Z` |
| `push` | pushes the branch + tag → `release-binaries.yml` builds the assets |
| `wait` | polls the GitHub release until every archive + `.sha256` exists |
| `channels` | rewrites Homebrew / Scoop / winget / site data (`scripts/refresh-channels.sh`) |
| `commit-channels` | `chore(release): refresh the install channels and the site for X.Y.Z` |
| `taps` | pushes the formula/manifest into `homebrew-tap` + `scoop-bucket` |
| `winget` | opens the `microsoft/winget-pkgs` PR (`scripts/winget-submit.sh`) |
| `crates` | `task publish:crates RELEASE_VERSION=X.Y.Z` (core crates, dependency order) |
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

Resume from the failed phase without repeating the version bump or creating an
existing tag again:

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

## Optional RustFS/CDN mirror

`release-binaries.yml` always publishes and validates GitHub Release assets
first. If the optional RustFS secrets are configured, a final best-effort
step mirrors the exact `rpi-release.json`, archives, and checksum sidecars to
an S3-compatible endpoint:

```text
RPI_RUSTFS_ENDPOINT
RPI_RUSTFS_BUCKET
RPI_RUSTFS_ACCESS_KEY
RPI_RUSTFS_SECRET_KEY
RPI_CDN_BASE_URL=https://assets.bigfisharc.cn
```

Objects are versioned under `rpi/releases/<tag>/`. GitHub remains the
canonical manifest and trust source; a mirror failure is reported in the
workflow summary without invalidating the GitHub Release. Configure public
read-only object access, disable listing/write/delete, and use short caching
for the JSON manifest and immutable caching for archives/checksums.

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
