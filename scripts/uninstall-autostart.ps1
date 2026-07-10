# Remove the logon auto-start task created by install-autostart.ps1,
# and stop the running resident process.
#
# Usage (run in an ELEVATED PowerShell):
#     powershell -ExecutionPolicy Bypass -File scripts\uninstall-autostart.ps1
#
# NOTE: ASCII-only so Windows PowerShell 5.1 parses it regardless of code page.
$ErrorActionPreference = "SilentlyContinue"

$taskName = "IMELiveConverter"

# Stop the running resident process
Get-Process -Name "conversion-service" -ErrorAction SilentlyContinue | Stop-Process -Force

# Remove the task
if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
    Write-Host "Removed task '$taskName' and stopped the process."
} else {
    Write-Host "Task '$taskName' is not registered."
}
