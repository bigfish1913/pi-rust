#requires -Version 5.1
<#
.SYNOPSIS
Install the prebuilt rpi binary from GitHub Releases on Windows.
.EXAMPLE
& .\install.ps1
.EXAMPLE
& .\install.ps1 -Version 0.3.19 -Dir "$env:LOCALAPPDATA\Programs\rpi" -DryRun
.NOTES
No Rust toolchain or administrator privileges are required. To build from
source instead, use 'cargo install rpi-cli'.
#>
[CmdletBinding()]
param(
    [string] $Version = '',
    [string] $Dir = '',
    [switch] $DryRun
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Repo = 'bigfish1913/pi-rust'
$TempDir = $null
$PreviousSecurityProtocol = [Net.ServicePointManager]::SecurityProtocol

try {
    # Windows PowerShell 5.1 may otherwise default to TLS 1.0.
    [Net.ServicePointManager]::SecurityProtocol = $PreviousSecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    # A 32-bit PowerShell process on 64-bit Windows reports x86; use the
    # native architecture when Windows supplies it.
    $Architecture = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) {
        $Architecture = $env:PROCESSOR_ARCHITEW6432
    }
    if ($Architecture -ne 'AMD64') {
        throw "No prebuilt binary for Windows architecture '$Architecture'. Build from source with 'cargo install rpi-cli'."
    }
    $Target = 'x86_64-pc-windows-msvc'

    if ([string]::IsNullOrWhiteSpace($Dir)) {
        if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
            throw 'LOCALAPPDATA is not set; pass -Dir to choose an installation directory.'
        }
        $Dir = Join-Path $env:LOCALAPPDATA 'Programs\rpi'
    }
    $Dir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Dir)

    if ([string]::IsNullOrWhiteSpace($Version)) {
        try {
            $Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{
                Accept = 'application/vnd.github+json'
                'User-Agent' = 'rpi-installer'
            }
            $Version = $Release.tag_name
        }
        catch {
            throw "Could not determine the latest release version; pass -Version. $($_.Exception.Message)"
        }
    }
    $Version = $Version.Trim() -replace '^v', ''
    if ($Version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
        throw 'Could not determine a valid release version; pass -Version (for example, -Version 0.3.19).'
    }

    $Asset = "rpi-v$Version-$Target.zip"
    $Url = "https://github.com/$Repo/releases/download/v$Version/$Asset"

    if ($DryRun) {
        Write-Host "platform: $Target"
        Write-Host "version:  $Version"
        Write-Host "url:      $Url"
        Write-Host "checksum: $Url.sha256"
        Write-Host "dir:      $Dir"
        return
    }

    $TempDir = Join-Path ([IO.Path]::GetTempPath()) ("rpi-install-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $TempDir | Out-Null
    $ArchivePath = Join-Path $TempDir $Asset
    $ChecksumPath = "$ArchivePath.sha256"

    Write-Host "Downloading $Asset"
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $ArchivePath
    }
    catch {
        throw "Could not download $Url. Check that v$Version has a prebuilt binary for $Target at https://github.com/$Repo/releases. Build from source with 'cargo install rpi-cli'. $($_.Exception.Message)"
    }
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$Url.sha256" -OutFile $ChecksumPath
    }
    catch {
        throw "Could not download the checksum for $Asset; checksum verification is required. $($_.Exception.Message)"
    }

    # The release workflow publishes sha256sum/shasum output: hash + filename.
    $Checksum = (Get-Content -LiteralPath $ChecksumPath -Raw).Trim()
    if ($Checksum -notmatch '^([0-9a-fA-F]{64})\s+\*?(.+)$' -or $Matches[2] -ne $Asset) {
        throw "Invalid SHA-256 checksum file for $Asset."
    }
    $ExpectedHash = $Matches[1]
    $ActualHash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash
    if ($ActualHash -ne $ExpectedHash) {
        throw "Checksum verification failed for $Asset. Expected $ExpectedHash, got $ActualHash."
    }
    Write-Host 'SHA-256 checksum verified.'

    $ExtractDir = Join-Path $TempDir 'extracted'
    Expand-Archive -LiteralPath $ArchivePath -DestinationPath $ExtractDir
    $Source = Join-Path $ExtractDir 'rpi.exe'
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw 'Archive did not contain rpi.exe.'
    }

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $Destination = Join-Path $Dir 'rpi.exe'
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
    Write-Host "Installed rpi $Version to $Destination"

    if (($env:PATH -split ';') -notcontains $Dir) {
        # Quote literal paths safely, including directories containing apostrophes.
        $QuotedDir = $Dir.Replace("'", "''")
        Write-Host ''
        Write-Host "Add $Dir to your user PATH in Environment Variables. For this PowerShell session, run:"
        Write-Host "  `$env:PATH = '$QuotedDir;' + `$env:PATH"
    }
    & $Destination --version
    if ($LASTEXITCODE -ne 0) {
        throw "Installed rpi could not run --version (exit code $LASTEXITCODE)."
    }
}
finally {
    if ($TempDir -and (Test-Path -LiteralPath $TempDir)) {
        Remove-Item -LiteralPath $TempDir -Recurse -Force
    }
    [Net.ServicePointManager]::SecurityProtocol = $PreviousSecurityProtocol
}
