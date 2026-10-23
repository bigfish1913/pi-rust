# Releasing rpi

## Normal releases: merge the Release PR

Contributors, including fork contributors, submit code and tests without changing
versions. After an ordinary PR lands on `main` and all CI checks pass,
`release-plz` creates or updates a draft Release PR. It prepares the version and
root changelog, synchronizes the embedded documentation, validates the nine
crates, and marks the PR ready. The shared workspace version and `rpi` version
group keep the nine core crates together; examples are excluded.

Automatic releases currently stay on **0.8.x**, as configured by
`workspace.metadata.release.series` in `Cargo.toml`. After release-plz proposes
a version, the preparation step keeps it on this series, updates the workspace
and internal pins, and retargets the new changelog section and PR metadata. The
publish gate rejects versions outside the series. Change this setting explicitly
when starting 0.9.x. API compatibility findings remain visible in the Release PR;
the numbering policy does not disable those checks.

Merge that Release PR when ready to ship. After its main-branch CI passes:

1. The existing release helper publishes the nine crates in dependency order.
2. Automation creates `vX.Y.Z` at the tested main commit. Existing tags are never
   moved; retrying the same release commit is supported.
3. A reusable workflow builds all five platforms, verifies the archives, uploads
   their checksums and publishes `rpi-release.json` last.
4. Homebrew and Scoop are updated independently; a new-version PR is submitted
   to `microsoft/winget-pkgs`. Microsoft review/CLA completion remains external.

No release runs in fork repositories or on PR events. Publication additionally
requires a merged PR from an upstream `release-plz-*` branch whose merge commit
matches the tested main commit. A fork branch with that name does not qualify.
Do not use `pull_request_target` to execute contribution code with release secrets.

### Documentation-only updates without a release

A documentation update does not require a version bump. Keep `Cargo.toml`,
`Cargo.lock`, existing tags and install-channel manifests unchanged, and do not
run `task publish` or merge a Release PR solely to publish documentation. Update
the relevant `docs/rpi-tool/` sources and run `sh scripts/sync-embedded-docs.sh`
when the bundled documentation changes.

An ordinary merge runs CI and may prepare or update a Release PR; it does not
authorize publication. If the explicit requirement is to avoid release
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
`RELEASE_AUTOMATION_TOKEN`, for preparing PRs and writing the external channels.
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
that prepare PRs or write channels.

The binaries workflow is called directly after tag creation: a tag created with
the built-in `GITHUB_TOKEN` does not start another push-triggered workflow.

### Recovery and release records

- Re-run failed jobs on the Release PR's original main CI run. Existing crates
  are skipped by the release helper; an existing tag must match the same commit.
- For a channel failure, run **Release install channels** from Actions on `main`
  with the published tag. It verifies all five archives before regenerating
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
