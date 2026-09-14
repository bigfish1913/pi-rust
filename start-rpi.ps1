param(
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$AgentArgs
)

$ErrorActionPreference = 'Stop'
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  throw 'Rust/Cargo is required. Install Rust 1.78+ first.'
}

Write-Host 'Starting RPI Interactive Content Agent in rpi-tui...' -ForegroundColor Cyan
Write-Host 'Project resources: .pi/SYSTEM.md + .pi/skills/' -ForegroundColor DarkCyan
cargo run -p rpi-cli -- @AgentArgs
