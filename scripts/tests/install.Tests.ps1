#requires -Version 5.1
# Offline installer tests; run with powershell or pwsh -NoProfile -File this-file.
# Only HTTP is stubbed on Windows; hashing, extraction, copying and execution
# use real files. The fixture executable needs no Rust installation.
$ErrorActionPreference = 'Stop'
$Installer = Join-Path (Split-Path $PSScriptRoot -Parent) 'install.ps1'
$TestRoot = Join-Path ([IO.Path]::GetTempPath()) ("rpi-installer-tests-" + [guid]::NewGuid().ToString('N'))
$OnWindows = [Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT
$SavedEnvironment = @{}
$TestState = @{}
foreach ($Name in @('PROCESSOR_ARCHITECTURE', 'PROCESSOR_ARCHITEW6432', 'LOCALAPPDATA')) {
    $SavedEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name, 'Process')
}

function Assert-True($Condition, [string] $Message) {
    if (-not $Condition) { throw $Message }
}

function Invoke-RestMethod {
    param($Uri, $Headers)
    $TestState.ApiCalls += $Uri
    if ($TestState.Mode -eq 'ApiFailure') { throw 'HTTP 403' }
    if ($TestState.Mode -eq 'EmptyTag') { return @{ tag_name = '' } }
    return @{ tag_name = 'v0.3.19' }
}

function Invoke-WebRequest {
    param($Uri, $OutFile, [switch] $UseBasicParsing)
    $TestState.Downloads += $Uri
    $TestState.DownloadDirectories += Split-Path $OutFile -Parent
    if ($Uri.EndsWith('.sha256')) {
        if ($TestState.Mode -eq 'MissingChecksum') { throw 'HTTP 404' }
        $Hash = (Get-FileHash -LiteralPath $TestState.FixtureArchive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($TestState.Mode -eq 'Mismatch') { $Hash = '0' * 64 }
        $Checksum = "$Hash  rpi-v0.3.19-x86_64-pc-windows-msvc.zip"
        if ($TestState.Mode -eq 'MalformedChecksum') { $Checksum = 'not a sha256 checksum' }
        if ($TestState.Mode -eq 'WrongChecksumAsset') { $Checksum = "$Hash  another.zip" }
        [IO.File]::WriteAllText($OutFile, "$Checksum`n")
    }
    else {
        if ($TestState.Mode -eq 'MissingAsset') { throw 'HTTP 404' }
        Microsoft.PowerShell.Management\Copy-Item -LiteralPath $TestState.FixtureArchive -Destination $OutFile
    }
}

function Expand-Archive {
    param($LiteralPath, $DestinationPath)
    $TestState.Extractions++
    Microsoft.PowerShell.Archive\Expand-Archive -LiteralPath $LiteralPath -DestinationPath $DestinationPath
}

function Copy-Item {
    param($LiteralPath, $Destination, [switch] $Force)
    Microsoft.PowerShell.Management\Copy-Item @PSBoundParameters
    if (-not $OnWindows) {
        # ZIP extraction does not retain execute permission on Linux.
        & chmod +x $Destination
        if ($LASTEXITCODE -ne 0) { throw 'Could not make the Linux test fixture executable.' }
    }
}

function Invoke-Test([string] $Name, [scriptblock] $Arrange, [scriptblock] $Check) {
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    $env:PROCESSOR_ARCHITEW6432 = $null
    $CaseDir = Join-Path $TestRoot $Name
    $env:LOCALAPPDATA = Join-Path $CaseDir 'local app data'
    $TestState.InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\rpi'
    $TestState.Options = @{ Version = 'v0.3.19' }
    $TestState.Mode = 'Success'
    $TestState.FixtureArchive = $TestState.BinaryArchive
    $TestState.ApiCalls = @()
    $TestState.Downloads = @()
    $TestState.DownloadDirectories = @()
    $TestState.Extractions = 0
    $TestState.Failure = $null
    $ProtocolBefore = [Net.ServicePointManager]::SecurityProtocol
    & $Arrange
    try {
        $Options = $TestState.Options
        $TestState.Output = @(& $Installer @Options 6>&1) -join "`n"
    }
    catch {
        $TestState.Failure = $_.Exception.Message
    }
    & $Check
    foreach ($Directory in $TestState.DownloadDirectories) {
        Assert-True (-not (Test-Path -LiteralPath $Directory)) 'Installer left temporary downloads behind.'
    }
    Assert-True ([Net.ServicePointManager]::SecurityProtocol -eq $ProtocolBefore) 'Installer did not restore TLS settings.'
    Write-Host "PASS: $Name"
}

function Assert-Installed {
    Assert-True (-not $TestState.Failure) "Installation failed: $TestState.Failure"
    $Installed = Join-Path $TestState.InstallDir 'rpi.exe'
    Assert-True (Test-Path -LiteralPath $Installed -PathType Leaf) 'rpi.exe was not installed.'
    Assert-True ((Get-FileHash -LiteralPath $Installed).Hash -eq $TestState.BinaryHash) 'Installed binary differs from the archive.'
    Assert-True ($TestState.Output -match 'rpi fixture 0\.3\.19') 'Installed binary did not print its version.'
    Assert-True ($TestState.Downloads.Count -eq 2) 'Expected archive and checksum downloads.'
    Assert-True ($TestState.Downloads[0] -eq 'https://github.com/bigfish1913/pi-rust/releases/download/v0.3.19/rpi-v0.3.19-x86_64-pc-windows-msvc.zip') 'Wrong archive URL.'
    Assert-True ($TestState.Downloads[1] -eq "$($TestState.Downloads[0]).sha256") 'Wrong checksum URL.'
}

function Assert-Preview {
    Assert-True (-not $TestState.Failure) "Preview failed: $TestState.Failure"
    Assert-True ($TestState.Output -match 'platform: x86_64-pc-windows-msvc') 'Preview omitted the target.'
    Assert-True ($TestState.Output -match 'version:  0\.3\.19') 'Preview omitted the normalized version.'
    Assert-True ($TestState.Output.Contains($TestState.InstallDir)) 'Preview omitted the installation directory.'
    Assert-True ($TestState.Downloads.Count -eq 0) 'DryRun downloaded an asset.'
    Assert-True (-not (Test-Path -LiteralPath $TestState.InstallDir)) 'DryRun created an installation directory.'
}

function Assert-FailedBeforeExtraction([string] $Message) {
    Assert-True ($TestState.Failure -like "*$Message*") "Expected '$Message', got '$TestState.Failure'."
    Assert-True ($TestState.Extractions -eq 0) 'Installer extracted an unverified archive.'
    Assert-True (-not (Test-Path -LiteralPath $TestState.InstallDir)) 'Failed installation created an installation directory.'
}

try {
    $FixtureDir = Join-Path $TestRoot 'fixture'
    New-Item -ItemType Directory -Path $FixtureDir -Force | Out-Null
    $Binary = Join-Path $FixtureDir 'rpi.exe'
    if ($OnWindows) {
        $Source = Join-Path $TestRoot 'fixture.cs'
        [IO.File]::WriteAllText($Source, 'class Fixture { static void Main() { System.Console.WriteLine("rpi fixture 0.3.19"); } }')
        # .NET Framework's compiler is included in Windows, including the CI image.
        $Compiler = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
        & $Compiler /nologo /target:exe "/out:$Binary" $Source
        if ($LASTEXITCODE -ne 0) { throw 'Could not compile the Windows test fixture.' }
    }
    else {
        [IO.File]::WriteAllText($Binary, "#!/bin/sh`nprintf 'rpi fixture 0.3.19\n'`n")
    }
    $TestState.BinaryHash = (Get-FileHash -LiteralPath $Binary).Hash
    $TestState.BinaryArchive = Join-Path $TestRoot 'binary.zip'
    Compress-Archive -LiteralPath $Binary -DestinationPath $TestState.BinaryArchive
    $Readme = Join-Path $TestRoot 'README.txt'
    [IO.File]::WriteAllText($Readme, 'Archive without rpi.exe')
    $TestState.EmptyArchive = Join-Path $TestRoot 'missing-binary.zip'
    Compress-Archive -LiteralPath $Readme -DestinationPath $TestState.EmptyArchive

    Invoke-Test 'pinned-default-directory' {} {
        Assert-Installed
        Assert-True ($TestState.ApiCalls.Count -eq 0) 'Pinned version unexpectedly called the API.'
        Assert-True ($TestState.Output -match 'user PATH') 'Installer omitted PATH instructions.'
    }
    Invoke-Test 'custom-directory' {
        $TestState.InstallDir = Join-Path $CaseDir "tools [rpi]'s directory"
        $TestState.Options = @{ Version = '0.3.19'; Dir = $TestState.InstallDir }
    } {
        Assert-Installed
        Assert-True ($TestState.Output.Contains("tools [rpi]''s directory;")) 'PATH instruction did not quote the apostrophe.'
    }
    Invoke-Test 'latest-release' { $TestState.Options = @{} } {
        Assert-Installed
        Assert-True ($TestState.ApiCalls.Count -eq 1) 'Latest version did not call the API once.'
        Assert-True ($TestState.ApiCalls[0] -eq 'https://api.github.com/repos/bigfish1913/pi-rust/releases/latest') 'Wrong release API URL.'
    }
    Invoke-Test 'pinned-preview' { $TestState.Options.DryRun = $true } {
        Assert-Preview
        Assert-True ($TestState.ApiCalls.Count -eq 0) 'Pinned preview called the API.'
    }
    Invoke-Test 'latest-preview' { $TestState.Options = @{ DryRun = $true } } {
        Assert-Preview
        Assert-True ($TestState.ApiCalls.Count -eq 1) 'Latest preview did not resolve the version.'
    }
    Invoke-Test '32-bit-shell-on-x64' {
        $env:PROCESSOR_ARCHITECTURE = 'x86'
        $env:PROCESSOR_ARCHITEW6432 = 'AMD64'
        $TestState.Options.DryRun = $true
    } { Assert-Preview }
    foreach ($Architecture in @('ARM64', 'x86')) {
        Invoke-Test "unsupported-$Architecture" {
            $env:PROCESSOR_ARCHITECTURE = $Architecture
        } {
            Assert-FailedBeforeExtraction 'cargo install rpi-cli'
            Assert-True ($TestState.Downloads.Count -eq 0 -and $TestState.ApiCalls.Count -eq 0) 'Unsupported platform accessed the network.'
        }
    }
    Invoke-Test 'missing-asset' { $TestState.Mode = 'MissingAsset' } {
        Assert-FailedBeforeExtraction 'cargo install rpi-cli'
    }
    Invoke-Test 'checksum-mismatch' { $TestState.Mode = 'Mismatch' } {
        Assert-FailedBeforeExtraction 'Checksum verification failed'
    }
    Invoke-Test 'checksum-mismatch-preserves-install' {
        $TestState.Mode = 'Mismatch'
        New-Item -ItemType Directory -Path $TestState.InstallDir -Force | Out-Null
        [IO.File]::WriteAllText((Join-Path $TestState.InstallDir 'rpi.exe'), 'existing installation')
    } {
        Assert-True ($TestState.Failure -like '*Checksum verification failed*') 'Checksum mismatch did not fail.'
        Assert-True ($TestState.Extractions -eq 0) 'Mismatched archive was extracted.'
        Assert-True ((Get-Content -LiteralPath (Join-Path $TestState.InstallDir 'rpi.exe') -Raw) -eq 'existing installation') 'Checksum mismatch overwrote the existing installation.'
    }
    foreach ($Mode in @('MissingChecksum', 'MalformedChecksum', 'WrongChecksumAsset')) {
        Invoke-Test $Mode { $TestState.Mode = $Mode } {
            Assert-FailedBeforeExtraction 'checksum'
        }
    }
    Invoke-Test 'missing-binary' { $TestState.FixtureArchive = $TestState.EmptyArchive } {
        Assert-True ($TestState.Failure -eq 'Archive did not contain rpi.exe.') 'Archive without rpi.exe did not fail clearly.'
        Assert-True (-not (Test-Path -LiteralPath $TestState.InstallDir)) 'Incomplete archive created an installation directory.'
    }
    foreach ($Mode in @('ApiFailure', 'EmptyTag')) {
        Invoke-Test $Mode { $TestState.Mode = $Mode; $TestState.Options = @{} } {
            Assert-FailedBeforeExtraction 'pass -Version'
            Assert-True ($TestState.Downloads.Count -eq 0) 'Invalid latest release downloaded an asset.'
        }
    }
    Write-Host 'All installer tests passed.'
}
finally {
    foreach ($Name in $SavedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($Name, $SavedEnvironment[$Name], 'Process')
    }
    if (Test-Path -LiteralPath $TestRoot) {
        Remove-Item -LiteralPath $TestRoot -Recurse -Force
    }
}
