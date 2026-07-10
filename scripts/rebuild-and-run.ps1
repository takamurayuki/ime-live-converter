# Stop the resident IME, rebuild, and relaunch it hidden.
#
# Prevents the "Access is denied (os error 5)" build failure that happens when
# conversion-service.exe is still running and cargo cannot overwrite it.
# Order: stop -> build -> launch.
#
# Usage (PowerShell; no admin needed):
#     powershell -ExecutionPolicy Bypass -File scripts\rebuild-and-run.ps1
#
# Launch with debug logging:
#     powershell -ExecutionPolicy Bypass -File scripts\rebuild-and-run.ps1 -Debug
#
# NOTE: This file is intentionally ASCII-only so Windows PowerShell 5.1 parses it
#       regardless of the system code page.
param([switch]$Debug)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Host "1) Stopping resident IME..."
Get-Process conversion-service -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

Write-Host "2) Building (cargo build --release)..."
cargo build --release
if ($LASTEXITCODE -ne 0) {
    Write-Error "Build failed. See the errors above."
    exit 1
}

$vbs = if ($Debug) { "run-hidden-debug.vbs" } else { "run-hidden.vbs" }
Write-Host "3) Launching hidden ($vbs)..."
Start-Process "wscript.exe" -ArgumentList ('"{0}"' -f (Join-Path $PSScriptRoot $vbs))

Write-Host "Done. IME is resident. To stop it:"
Write-Host "    Get-Process conversion-service | Stop-Process -Force"
