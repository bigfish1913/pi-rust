# Install channels

rpi reaches users six ways. Five are live today and the sixth is a manifest
sitting in review. The point of this directory is that every manifest points at
a **real release asset** and carries that asset's **real checksum** — none of it
is a template with placeholders.

## Status

| Channel | Lives in | Status | Remaining step |
| --- | --- | --- | --- |
| `cargo install rpi-cli` | crates.io | **live** | none |
| `curl … install.sh \| sh` | [`scripts/install.sh`](../scripts/install.sh) | **live** | none |
| `cargo binstall rpi-cli` | [`crates/pi-cli/Cargo.toml`](../crates/pi-cli/Cargo.toml) | **configured** | none — takes effect with the next published crate version |
| Homebrew | [`bigfish1913/homebrew-tap`](https://github.com/bigfish1913/homebrew-tap) | **live** | none — `brew tap bigfish1913/tap && brew install rpi` |
| Scoop | [`bigfish1913/scoop-bucket`](https://github.com/bigfish1913/scoop-bucket) | **live** | none — `scoop bucket add bigfish1913 <bucket url>` |
| winget | [open at `microsoft/winget-pkgs#441408`](https://github.com/microsoft/winget-pkgs/pull/441408) | **submitted** | review by the winget team; `winget install bigfish1913.rpi` does not work until it merges |

The files in this directory are the source of truth for the two taps; the tap
repos are copies. Homebrew-in-`homebrew-core` and `winget` carry the real
discovery weight — both index by name inside a package manager the user already
has open, which is traffic the website cannot reach.

## The asset contract these manifests depend on

`.github/workflows/release-binaries.yml` publishes one archive per target, **with
the binary at the archive root** and a `.sha256` beside every archive:

| Target | Asset | sha256 (v0.3.2) |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `rpi-v0.3.2-x86_64-unknown-linux-gnu.tar.gz` | `b943b36414decd5af9c6b94115ab5fb69ec1780ad60fe7533b8d985f7ec8fa23` |
| `aarch64-unknown-linux-gnu` | `rpi-v0.3.2-aarch64-unknown-linux-gnu.tar.gz` | `2a940c245ba029d47421e6adb7adafb4fd6018853d37fe9e8da07e53d1438f3f` |
| `aarch64-apple-darwin` | `rpi-v0.3.2-aarch64-apple-darwin.tar.gz` | `92e1851f5987d99899653e52e120d21fa57f49d5ccd1c7d44e8bc7d9de5b1842` |
| `x86_64-pc-windows-msvc` | `rpi-v0.3.2-x86_64-pc-windows-msvc.zip` | `c5b572d7e7f0748981a3564dc7368b7d4fc9396aae9ee389436311297ec30d7f` |

Two consequences worth knowing:

- **There is no `x86_64-apple-darwin` build.** Intel Macs are served by
  `cargo install rpi-cli`; the Homebrew formula deliberately omits that branch.
- The assets are named after the **binary** (`rpi`), while the crate is
  `rpi-cli`. Anything templating a URL from the crate name is wrong — which is
  why the binstall `pkg-url` uses `{ bin }` and not `{ name }`.

## Refresh procedure for a new release

`curl` against `github.com/.../releases/download/...` returns nothing from some
networks (this machine included), so use `gh`, which goes through the API:

```bash
TAG=v0.3.3                      # the tag being packaged
gh release download "$TAG" --pattern "*.sha256" --dir /tmp/rpi-sha --clobber
cat /tmp/rpi-sha/*.sha256
```

Then update, in this order:

1. **binstall** — nothing to change; the URL is templated from `{ version }`.
2. **Scoop** — `scoop/rpi.json`: `version`, the `architecture.64bit.url`, and
   `architecture.64bit.hash`. `checkver`/`autoupdate` keep later releases
   automatic.
3. **Homebrew** — `homebrew/rpi.rb`: `version` and the three `url`/`sha256`
   pairs.
4. **winget** — all three files' `PackageVersion`, the `InstallerUrl`, and the
   **uppercase** `InstallerSha256`.

The winget manifests track the **current** repo convention rather than the
oldest supported schema: `ManifestVersion 1.12.0` with the
`yaml-language-server` schema comment, and `NestedInstallerFiles` inside each
installer entry. That is what the newest manifests merged into
`microsoft/winget-pkgs` look like (checked against `BurntSushi.ripgrep.MSVC
15.2.0`), so a reviewer does not have to ask for the newer shape.

### Submitting to winget

Done once for 0.3.2 — [PR #441408](https://github.com/microsoft/winget-pkgs/pull/441408).
For the next version, repeat it without cloning the 866 MB monorepo; every step
below is a server-side API call:

```bash
V=0.3.3
winget validate packaging/winget                       # 1. validate what you will submit
gh repo fork microsoft/winget-pkgs --clone=false       # 2. server-side fork, no download
UP=$(gh api repos/microsoft/winget-pkgs/commits/master --jq .sha)
gh api --method POST repos/<you>/winget-pkgs/git/refs \
  -f ref="refs/heads/rpi-$V" -f sha="$UP"               # 3. branch off upstream master
# 4. PUT each file to manifests/b/bigfish1913/rpi/$V/ via the contents API.
#    Use `--input` with a JSON body: the README-sized payloads blow past the
#    ~32 KB command-line limit if you pass them with -f content=.
gh pr create --repo microsoft/winget-pkgs --base master --head <you>:rpi-$V \
  --title "New package: bigfish1913.rpi version $V"     # 5. open the PR
```

One trap in the CLA step, worth not rediscovering: the policy bot matches
`@microsoft-github-policy-service agree` **anchored to the start of the
comment**, so a single leading space fails the match. The bot does not report
that — it silently re-posts the CLA text, which reads exactly like the signature
never arrived. The comment body must begin with `@` at column 0 and contain
nothing else on the line.

Also pick the right form: the bare `agree` asserts you are **not** contributing
as part of work for an employer. If you are, use `agree company="…"`.

## What was verified, and what was not

Verified on this machine:

- `winget validate packaging/winget` → **Manifest validation succeeded.**
- Every URL and hash above was read back from the published v0.3.2 assets
  (`gh release download --pattern '*.sha256'`), not copied from a template.
- `scoop/rpi.json` parses, and its `bin` (`rpi.exe`) matches the zip's root
  entry produced by the workflow's `7z a -tzip … .` step.
- The published archives themselves were downloaded and hashed here, byte for
  byte: the Windows zip is `c5b572d7…0d7f` and the macOS tarball is
  `92e1851f…1842`, matching `scoop/rpi.json` and `homebrew/rpi.rb` exactly. The
  zip was listed to confirm `rpi.exe` really is at the archive root — the thing
  `bin` claims and the only reason the Scoop manifest works.
- Both taps are pushed and public (`bigfish1913/homebrew-tap`,
  `bigfish1913/scoop-bucket`), so their files are fetchable at the URLs the
  package managers will use.

**Not** verified here, and worth one manual pass before relying on them:

- **Homebrew** — no `brew`/`ruby` on this machine, so the formula was never
  syntax-checked or installed, even though it is published. `brew audit
  --new-formula rpi` in the tap is the cheap check.
- **binstall** — `cargo-binstall` is not installed here, so the metadata is
  written from the documented schema (`SUPPORT.md` in `cargo-bins/cargo-binstall`)
  but untested. `cargo binstall --no-confirm rpi-cli` against the next published
  version is the test. Note the docs do **not** mention verifying our `.sha256`
  files, so do not claim checksum verification for this channel.
- **Scoop** — the manifest was not installed from; `scoop checkver` in the real
  bucket is the check.
- A `winget install` was deliberately **not** run, because it would modify this
  machine's package state — and the package is not installable yet anyway, since
  the manifest is an open PR.
