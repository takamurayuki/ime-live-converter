# Uninstall IME Live Converter: stop it, remove the logon task and the program.
#
# The learning DB and logs (%LOCALAPPDATA%\IMELiveConverter) are kept unless you
# answer "y" to the question or pass -RemoveUserData, so a later reinstall
# keeps your learning.
#
# Usage:
#     powershell -ExecutionPolicy Bypass -File uninstall.ps1 [-RemoveUserData | -KeepUserData]
#
# NOTE: ASCII-only so Windows PowerShell 5.1 parses it regardless of code page.
param(
    [string]$InstallDir = "",
    [string]$DataDir = "",
    [switch]$RemoveUserData,
    [switch]$KeepUserData,
    [switch]$Elevated
)

$ErrorActionPreference = "Stop"
$TaskName = "IMELiveConverter"
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\IMELiveConverter" }
if (-not $DataDir)    { $DataDir    = Join-Path $env:LOCALAPPDATA "IMELiveConverter" }

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    return (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}
function Quote([string]$s) { return '"' + $s + '"' }
function Finish([int]$code) {
    if ($Elevated) { Read-Host "Press Enter to close" | Out-Null }
    exit $code
}

# Ask about user data before elevating (the question is answered by the user who started it)
if (-not $RemoveUserData -and -not $KeepUserData -and (Test-Path $DataDir)) {
    $ans = Read-Host "Also delete your learning data and logs in $DataDir ? [y/N]"
    if ($ans -match '^[yY]') { $RemoveUserData = $true } else { $KeepUserData = $true }
}

$task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($task -and -not (Test-Admin)) {
    Write-Host "Asking for administrator rights (needed to remove the logon task)..."
    $argList = "-NoProfile -ExecutionPolicy Bypass -File " + (Quote $PSCommandPath) +
        " -InstallDir " + (Quote $InstallDir) + " -DataDir " + (Quote $DataDir) + " -Elevated"
    if ($RemoveUserData) { $argList += " -RemoveUserData" } else { $argList += " -KeepUserData" }
    $p = Start-Process powershell -Verb RunAs -ArgumentList $argList -Wait -PassThru
    exit $p.ExitCode
}

try {
    Set-Location $env:TEMP   # do not stand inside the folder we are deleting
    Write-Host "1) Stopping the IME..."
    if ($task) { Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue }
    Get-Process conversion-service -ErrorAction SilentlyContinue | Stop-Process -Force
    for ($i = 0; $i -lt 40 -and (Get-Process conversion-service -ErrorAction SilentlyContinue); $i++) {
        Start-Sleep -Milliseconds 250
    }

    if ($task) {
        Write-Host "2) Removing the logon task '$TaskName'..."
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    } else {
        Write-Host "2) The logon task is not registered."
    }

    if (Test-Path (Join-Path $InstallDir "conversion-service.exe")) {
        Write-Host "3) Removing the program: $InstallDir"
        Remove-Item $InstallDir -Recurse -Force
    } else {
        Write-Host "3) Program folder not found (already removed?): $InstallDir"
    }

    if ($RemoveUserData -and (Test-Path $DataDir)) {
        Write-Host "4) Removing learning data and logs: $DataDir"
        Remove-Item $DataDir -Recurse -Force
    } else {
        Write-Host "4) Kept learning data: $DataDir"
    }
    Write-Host "Done."
    Finish 0
} catch {
    Write-Host "Uninstall failed: $($_.Exception.Message)" -ForegroundColor Red
    Finish 1
}
