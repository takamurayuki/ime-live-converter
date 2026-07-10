# Register a logon scheduled task that starts the IME hidden and resident.
#
# Notes:
#   - The keyboard hook must run in the user session, so we use a logon task
#     (not a Windows service in session 0).
#   - Runs with Highest privileges so conversion works over elevated apps too
#     (no UAC prompt).
#   - Launched via run-hidden.vbs so no console window appears.
#
# Usage (run in an ELEVATED PowerShell):
#     powershell -ExecutionPolicy Bypass -File scripts\install-autostart.ps1
#
# Uninstall:
#     powershell -ExecutionPolicy Bypass -File scripts\uninstall-autostart.ps1
#
# NOTE: ASCII-only so Windows PowerShell 5.1 parses it regardless of code page.
$ErrorActionPreference = "Stop"

$taskName = "IMELiveConverter"
$root = Split-Path -Parent $PSScriptRoot
$vbs  = Join-Path $PSScriptRoot "run-hidden.vbs"
$exe  = Join-Path $root "target\release\conversion-service.exe"

if (-not (Test-Path $exe)) {
    throw "Build first: cargo build --release ($exe not found)"
}
if (-not (Test-Path $vbs)) {
    throw "Launcher not found: $vbs"
}

$action = New-ScheduledTaskAction -Execute "wscript.exe" `
    -Argument ('"{0}"' -f $vbs) -WorkingDirectory $root
$trigger = New-ScheduledTaskTrigger -AtLogOn
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" `
    -LogonType Interactive -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable

Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null

Write-Host "Registered task '$taskName' (auto-starts at next logon)."
Write-Host "Start now:     Start-ScheduledTask -TaskName $taskName"
Write-Host "Check status:  Get-ScheduledTask -TaskName $taskName"
