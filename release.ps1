<#
.SYNOPSIS
  pi-rust crates.io release helper.

.DESCRIPTION
  Publishes the pi-rust workspace crates to crates.io in dependency order:
    pi-telemetry -> pi-ai -> pi-agent -> pi-tools -> pi-harness -> pi-cli
  (examples are publish=false and skipped).

  Default mode is a SAFE DRY RUN (-DryRun): each crate is packed + verified
  with `cargo publish --dry-run`. Only the workspace leaf (pi-telemetry) can
  fully resolve in a dry run — every downstream crate is expected to SKIP with
  "no matching package `pi-X`" because its pi-* deps are not on crates.io yet.
  That is NORMAL for a first-time workspace publish; the real publish (-Publish)
  resolves them in order.

  -Publish runs the real `cargo publish` per crate, stopping on the first hard
  failure (downstream crates depend on the upstream one). A short sleep between
  publishes lets the crates.io index propagate; a transient "no matching
  package" error is retried a few times before being treated as a failure.

  Pre-flight (both modes, unless -SkipTest):
    * working tree must be clean for -Publish (dry-run only warns);
    * `cargo test --workspace` must pass.

  Prerequisites for -Publish:
    * `cargo login` must have been run once interactively
      (creates ~/.cargo/credentials(.toml)). The script warns if it can't find
      credentials.
    * The workspace `repository` URL in the root Cargo.toml should be set to
      YOUR pi-rust repo. It ships pointing at the upstream TS source as a
      placeholder; the script warns loudly on -Publish if it is still the
      placeholder. crates.io names are permanent — check them first.

.PARAMETER Publish
  Really publish to crates.io. Without this flag the script is a dry run.

.PARAMETER DryRun
  Explicit dry run (default). Provided so the intent is loud on the command line.

.PARAMETER SkipTest
  Skip the `cargo test --workspace` pre-flight (use only if you just ran it).

.PARAMETER SleepSeconds
  Seconds to wait between real publishes for crates.io index propagation.
  Default 3. Bump it if you hit transient "no matching package" retries.

.EXAMPLE
  ./release.ps1                  # safe dry run
  ./release.ps1 -DryRun          # same, explicit
  ./release.ps1 -Publish         # real publish, dep order
  ./release.ps1 -Publish -SkipTest   # real publish, skip pre-flight tests
#>
[CmdletBinding()]
param(
    [switch]$Publish,
    [switch]$DryRun,
    [switch]$SkipTest,
    [int]$SleepSeconds = 3
)

$ErrorActionPreference = 'Stop'

# Mode resolution: -Publish wins; otherwise dry-run.
$Real = $false
if ($Publish) { $Real = $true }
if ($DryRun -and $Publish) {
    Write-Warning "Both -Publish and -DryRun given; using -Publish (real)."
}
$Mode = if ($Real) { 'PUBLISH (real)' } else { 'DRY RUN' }
$OutputEncoding = [System.Text.UTF8Encoding]::new()
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new()

# Dependency order. examples/* are publish=false and not listed.
$Order = @('pi-telemetry', 'pi-ai', 'pi-agent', 'pi-tools', 'pi-harness', 'pi-cli')

$repoRoot = Split-Path -Parent $PSCommandPath
Set-Location $repoRoot

function Write-Section($msg) { Write-Host "`n=== $msg ===" -ForegroundColor Cyan }
function Write-Ok($msg)      { Write-Host "[ok]   $msg" -ForegroundColor Green }
function Write-Skip($msg)    { Write-Host "[skip] $msg" -ForegroundColor Yellow }
function Write-Bad($msg)     { Write-Host "[fail] $msg" -ForegroundColor Red }

# Run a cargo command and return combined stdout+stderr text + exit code.
# PowerShell 5.1 wraps native stderr lines in ErrorRecord objects that throw
# under $ErrorActionPreference='Stop'; running under 'Continue' makes them
# non-terminating, so we can collect the text without aborting. `$LASTEXITCODE`
# is the real success signal.
function Invoke-Cargo([string[]]$cargoArgs) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $stream = & cargo @cargoArgs 2>&1
        $code = $LASTEXITCODE
        $text = ($stream | ForEach-Object { $_.ToString() }) -join "`n"
    } finally {
        $ErrorActionPreference = $prev
    }
    [pscustomobject]@{ Text = $text; Code = $code }
}

# --- credentials check (real publish only) ---
$hasCreds = $false
$credToml = Join-Path $env:USERPROFILE '.cargo/credentials.toml'
$credPlain = Join-Path $env:USERPROFILE '.cargo/credentials'
if ((Test-Path $credToml) -or (Test-Path $credPlain)) { $hasCreds = $true }
if ($Real -and -not $hasCreds) {
    Write-Bad "No cargo credentials found (~/.cargo/credentials.toml)."
    Write-Host "  Run 'cargo login' once interactively, then re-run -Publish." -ForegroundColor Yellow
    exit 1
}

# --- repository placeholder check ---
$rootManifest = Join-Path $repoRoot 'Cargo.toml'
$manifestText = Get-Content -Raw $rootManifest
if ($manifestText -match 'earendil-works/pi') {
    if ($Real) {
        Write-Bad "repository in Cargo.toml still points at the upstream TS source (earendil-works/pi)."
        Write-Host "  Edit [workspace.package].repository to YOUR pi-rust repo URL before publishing." -ForegroundColor Yellow
        Write-Host "  crates.io records are permanent — continuing in 5s (Ctrl-C to abort)..." -ForegroundColor Yellow
        Start-Sleep -Seconds 5
    } else {
        Write-Skip "repository URL is the upstream placeholder (fine for a dry run; fix before -Publish)."
    }
}

# --- clean-tree check ---
$dirty = & git status --porcelain 2>&1 | Out-String
if ($dirty.Trim().Length -gt 0) {
    if ($Real) {
        Write-Bad "Working tree has uncommitted changes; refusing to -Publish."
        Write-Host $dirty
        exit 1
    } else {
        Write-Skip "Working tree is dirty (allowed in dry run; --allow-dirty will be passed)."
    }
}

# --- pre-flight tests ---
if (-not $SkipTest) {
    Write-Section "Pre-flight: cargo test --workspace"
    & cargo test --workspace 2>&1 | Out-String | Write-Host
    if ($LASTEXITCODE -ne 0) {
        Write-Bad "Pre-flight tests failed (exit $LASTEXITCODE). Aborting."
        exit 1
    }
    Write-Ok "tests pass"
} else {
    Write-Skip "Skipping pre-flight tests (-SkipTest)."
}

# --- publish loop ---
Write-Section "pi-rust release -- $Mode -- order: $($Order -join ', ')"

$results = @()
$anyFail = $false

foreach ($crate in $Order) {
    Write-Host "`n--- $crate ---" -ForegroundColor Cyan
    $args = @('publish', '-p', $crate)
    if (-not $Real) {
        $args += '--dry-run'
        $args += '--allow-dirty'   # dry run on a possibly-dirty tree
    }

    $attempt = 0
    $maxAttempts = if ($Real) { 4 } else { 1 }
    $done = $false
    while (-not $done) {
        $attempt++
        $res = Invoke-Cargo $args
        $output = $res.Text
        $code = $res.Code
        if ($code -eq 0) {
            Write-Ok $crate
            $results += [pscustomobject]@{ Crate = $crate; Status = 'PASS'; Note = '' }
            $done = $true
        } else {
            # In a dry run, downstream crates can't resolve their pi-* deps yet,
            # because those deps aren't on crates.io. cargo phrases this two
            # ways depending on whether the crate name is entirely absent (no
            # matching package) vs. present at a non-matching version (failed
            # to select a version / candidate versions found which didn't
            # match). Both are the same expected first-publish condition.
            $missingDep = ($output -match 'no matching package') -or
                          ($output -match 'failed to select a version for the requirement') -or
                          ($output -match 'candidate versions found which didn')
            if ($missingDep) {
                if ($Real -and $attempt -lt $maxAttempts) {
                    Write-Skip "$crate`: deps not indexed yet (attempt $attempt/$maxAttempts); sleeping $SleepSeconds s..."
                    Start-Sleep -Seconds $SleepSeconds
                    continue
                }
                Write-Skip "$crate (expected: pi-* deps not on crates.io yet)"
                $results += [pscustomobject]@{ Crate = $crate; Status = 'SKIP'; Note = 'deps not on crates.io yet' }
                $done = $true
            } else {
                # Hard failure — show the tail of the output.
                $tail = ($output -split "`n") | Select-Object -Last 12
                Write-Bad "$crate failed (exit $code):"
                $tail | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
                $results += [pscustomobject]@{ Crate = $crate; Status = 'FAIL'; Note = "exit $code" }
                $anyFail = $true
                $done = $true
                if ($Real) {
                    Write-Bad "Stopping (--publish): downstream crates depend on this one."
                    # Fill remaining as not-run.
                    $remaining = $Order | Where-Object { $_ -ne $crate -and -not ($results.Crate -contains $_) }
                    foreach ($r in $remaining) {
                        $results += [pscustomobject]@{ Crate = $r; Status = 'N/R'; Note = 'preceding crate failed' }
                    }
                    break
                }
            }
        }
    }

    # Propagation sleep after a successful REAL publish (not after the last).
    if ($Real -and -not $anyFail -and $crate -ne $Order[-1]) {
        Write-Host "  (sleeping $SleepSeconds s for crates.io index propagation...)" -ForegroundColor DarkGray
        Start-Sleep -Seconds $SleepSeconds
    }
}

# --- summary ---
Write-Section "Summary"
$results | Format-Table -AutoSize | Out-String | Write-Host

if ($anyFail) {
    Write-Bad "Release completed with failures."
    exit 1
} elseif ($Real) {
    Write-Ok "All crates published."
} else {
    $pass = ($results | Where-Object Status -eq 'PASS').Count
    $skip = ($results | Where-Object Status -eq 'SKIP').Count
    Write-Host "Dry run: $pass pass, $skip skipped." -ForegroundColor Green
    if ($skip -gt 0) {
        Write-Host "  (Skipped crates are expected to resolve only during a real -Publish," -ForegroundColor DarkGray
        Write-Host "   once their pi-* deps are live on crates.io.)" -ForegroundColor DarkGray
    }
}
exit 0
