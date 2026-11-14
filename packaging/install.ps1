# Install or update IME Live Converter (the same script does both).
#
# Steps: stop the running IME -> copy the program -> install the models if the
# required version is missing -> register the logon task -> start.
# The learning DB and logs in %LOCALAPPDATA%\IMELiveConverter are never touched,
# so learning survives updates.
# See docs/release-and-versioning.md for why it works this way.
#
# Usage (from the extracted package folder):
#     powershell -ExecutionPolicy Bypass -File install.ps1
# Options:
#     -InstallDir <dir>        default: %LOCALAPPDATA%\Programs\IMELiveConverter
#     -ModelsDir <dir>         folder that already has system.dic / judge_lm.bin /
#                              judge_cooc.bin (offline install; otherwise they are
#                              downloaded from GitHub Releases when needed)
#     -ImportLearningDb <file> copy an existing ime-learning.db (e.g. from a dev
#                              checkout) when there is no learning DB yet
#     -NoAutostart             do not register the logon task (no admin needed)
#     -NoStart                 do not start the IME after installing
#     -NoShortcuts             do not create the Start menu shortcuts
#
# Updating later: run update.ps1 in the install folder (or the Start menu
# shortcut "Update IME Live Converter"); it downloads the latest release and runs
# its install.ps1.
#
# Registering the logon task needs admin rights (the task runs with highest
# privileges so conversion also works in elevated apps); the script asks for
# elevation by itself.
#
# NOTE: ASCII-only so Windows PowerShell 5.1 parses it regardless of code page.
param(
    [string]$InstallDir = "",
    [string]$ModelsDir = "",
    [string]$ImportLearningDb = "",
    [string]$DataDir = "",
    [switch]$NoAutostart,
    [switch]$NoStart,
    [switch]$NoShortcuts,
    [string]$StartMenuDir = "",
    [switch]$Elevated
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"   # Invoke-WebRequest is very slow with the progress bar on PS 5.1

$TaskName   = "IMELiveConverter"
$Repo       = "takamurayuki/ime-live-converter"
$ModelFiles = @("system.dic", "judge_lm.bin", "judge_cooc.bin")
$Src        = $PSScriptRoot

# Resolve per-user folders here (before elevation) so an elevated copy still
# installs for the user who started the script.
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\IMELiveConverter" }
if (-not $DataDir)    { $DataDir    = Join-Path $env:LOCALAPPDATA "IMELiveConverter" }
if (-not $StartMenuDir) { $StartMenuDir = Join-Path ([Environment]::GetFolderPath("Programs")) "IME Live Converter" }

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

if (-not $NoAutostart -and -not (Test-Admin)) {
    Write-Host "Asking for administrator rights (needed to register the logon task)..."
    $argList = "-NoProfile -ExecutionPolicy Bypass -File " + (Quote $PSCommandPath) +
        " -InstallDir " + (Quote $InstallDir) + " -DataDir " + (Quote $DataDir) +
        " -StartMenuDir " + (Quote $StartMenuDir) + " -Elevated"
    if ($ModelsDir)        { $argList += " -ModelsDir " + (Quote $ModelsDir) }
    if ($NoShortcuts)      { $argList += " -NoShortcuts" }
    if ($ImportLearningDb) { $argList += " -ImportLearningDb " + (Quote $ImportLearningDb) }
    if ($NoStart)          { $argList += " -NoStart" }
    $p = Start-Process powershell -Verb RunAs -ArgumentList $argList -Wait -PassThru
    exit $p.ExitCode
}

try {
    # ---- 0. check the package ------------------------------------------------
    foreach ($f in @("conversion-service.exe", "hook_dll.dll", "run-hidden.vbs", "VERSION", "MODELS_VERSION_REQUIRED")) {
        if (-not (Test-Path (Join-Path $Src $f))) { throw "Package file missing: $f (run install.ps1 from the extracted package folder)" }
    }
    $newVersion    = (Get-Content (Join-Path $Src "VERSION") -Raw).Trim()
    $modelsVersion = (Get-Content (Join-Path $Src "MODELS_VERSION_REQUIRED") -Raw).Trim()
    $exe = Join-Path $InstallDir "conversion-service.exe"
    $oldVersion = $null
    if (Test-Path $exe) {
        try { $oldVersion = (& $exe --version | Select-Object -First 1).Trim() } catch { $oldVersion = "unknown" }
    }
    if ($oldVersion) { Write-Host "Updating IME Live Converter $oldVersion -> $newVersion" }
    else             { Write-Host "Installing IME Live Converter $newVersion" }
    Write-Host "  program: $InstallDir"
    Write-Host "  data:    $DataDir"

    # ---- 1. stop the running IME ---------------------------------------------
    Write-Host "1) Stopping the running IME..."
    if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
        Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    }
    Get-Process conversion-service -ErrorAction SilentlyContinue | Stop-Process -Force
    for ($i = 0; $i -lt 40 -and (Get-Process conversion-service -ErrorAction SilentlyContinue); $i++) {
        Start-Sleep -Milliseconds 250
    }
    if (Get-Process conversion-service -ErrorAction SilentlyContinue) {
        throw "conversion-service.exe is still running (it may run as another user). Stop it and retry."
    }

    # ---- 2. copy the program -------------------------------------------------
    Write-Host "2) Copying the program..."
    New-Item -ItemType Directory -Force -Path (Join-Path $InstallDir "dictionaries") | Out-Null
    $samePlace = ((Resolve-Path $Src).Path.TrimEnd('\') -ieq (Resolve-Path $InstallDir).Path.TrimEnd('\'))
    if (-not $samePlace) {
        $items = @("conversion-service.exe", "hook_dll.dll", "run-hidden.vbs", "install.ps1", "uninstall.ps1",
                   "update.ps1", "VERSION", "MODELS_VERSION_REQUIRED", "README.txt")
        foreach ($f in $items) {
            $from = Join-Path $Src $f
            if (Test-Path $from) { Copy-Item $from -Destination $InstallDir -Force }
        }
        if (Test-Path (Join-Path $Src "licenses")) {
            Copy-Item (Join-Path $Src "licenses") -Destination $InstallDir -Recurse -Force
        }
        # small bundled data (commands.csv, word_priority.tsv, ...). Models are handled below.
        Get-ChildItem (Join-Path $Src "dictionaries") -File | ForEach-Object {
            Copy-Item $_.FullName -Destination (Join-Path $InstallDir "dictionaries") -Force
        }
    }
    # A ZIP downloaded with a browser marks every extracted file as "from the internet"
    # (Mark of the Web) and Copy-Item keeps that mark; wscript would then show a security
    # warning when the logon task starts run-hidden.vbs. Clear it on the installed copy.
    Get-ChildItem $InstallDir -Recurse -File | Unblock-File -ErrorAction SilentlyContinue

    # ---- 3. models (only when the required version is not installed) ----------
    $dictDir = Join-Path $InstallDir "dictionaries"
    $markerPath = Join-Path $dictDir "MODELS_VERSION"
    $installedModels = $null
    if (Test-Path $markerPath) { $installedModels = (Get-Content $markerPath -Raw).Trim() }
    $missing = @($ModelFiles | Where-Object { -not (Test-Path (Join-Path $dictDir $_)) })
    if ($installedModels -eq $modelsVersion -and $missing.Count -eq 0) {
        Write-Host "3) Models $modelsVersion are already installed."
    } else {
        Write-Host "3) Installing models $modelsVersion..."
        $from = $null
        if ($ModelsDir) {
            $from = $ModelsDir
        } elseif (Test-Path (Join-Path $Src "models\system.dic")) {
            $from = Join-Path $Src "models"
        } else {
            $from = Join-Path $env:TEMP "IMELiveConverter-$modelsVersion"
            New-Item -ItemType Directory -Force -Path $from | Out-Null
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
            $base = "https://github.com/$Repo/releases/download/$modelsVersion"
            Write-Host "   Downloading from $base (about 370 MB, first install or model update only)..."
            foreach ($f in ($ModelFiles + @("SHA256SUMS.txt"))) {
                $dest = Join-Path $from $f
                if (Test-Path $dest) { continue }
                Write-Host "   $f"
                Invoke-WebRequest -UseBasicParsing -Uri "$base/$f" -OutFile "$dest.part"
                Move-Item "$dest.part" $dest -Force
            }
        }
        foreach ($f in $ModelFiles) {
            if (-not (Test-Path (Join-Path $from $f))) { throw "Model file not found: $(Join-Path $from $f)" }
        }
        # verify checksums when the list is available
        $sums = Join-Path $from "SHA256SUMS.txt"
        if (Test-Path $sums) {
            foreach ($line in Get-Content $sums) {
                if ($line -match '^([0-9a-fA-F]{64})\s+\*?(\S+)$' -and ($ModelFiles -contains $Matches[2])) {
                    $expected = $Matches[1]; $name = $Matches[2]
                    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $from $name)).Hash
                    if ($actual -ne $expected.ToUpper()) {
                        Remove-Item (Join-Path $from $name) -Force -ErrorAction SilentlyContinue
                        throw "Checksum mismatch for $name (the broken download was removed; run install.ps1 again)"
                    }
                }
            }
        }
        foreach ($f in $ModelFiles) { Copy-Item (Join-Path $from $f) -Destination $dictDir -Force }
        Set-Content -Path $markerPath -Value $modelsVersion -Encoding ASCII
    }

    # ---- 4. learning DB import (optional, only when there is none yet) --------
    if ($ImportLearningDb) {
        $dbDest = Join-Path $DataDir "ime-learning.db"
        if (Test-Path $dbDest) {
            Write-Host "4) A learning DB already exists, not importing: $dbDest"
        } elseif (-not (Test-Path $ImportLearningDb)) {
            throw "Learning DB to import was not found: $ImportLearningDb"
        } else {
            Write-Host "4) Importing the learning DB from $ImportLearningDb"
            New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
            Copy-Item $ImportLearningDb -Destination $dbDest
            # WAL mode: recent writes may only be in the -wal file
            if (Test-Path "$ImportLearningDb-wal") { Copy-Item "$ImportLearningDb-wal" -Destination "$dbDest-wal" }
        }
    }

    # ---- 5. logon task ---------------------------------------------------------
    $vbs = Join-Path $InstallDir "run-hidden.vbs"
    if ($NoAutostart) {
        Write-Host "5) Skipping the logon task (-NoAutostart)."
    } else {
        Write-Host "5) Registering the logon task '$TaskName'..."
        $action = New-ScheduledTaskAction -Execute "wscript.exe" -Argument (Quote $vbs) -WorkingDirectory $InstallDir
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
        $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" `
            -LogonType Interactive -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
            -ExecutionTimeLimit ([TimeSpan]::Zero) -StartWhenAvailable
        Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
            -Principal $principal -Settings $settings -Force | Out-Null
    }

    # ---- 5b. Start menu shortcuts (update / uninstall) -------------------------------
    if ($NoShortcuts) {
        Write-Host "5b) Skipping the Start menu shortcuts (-NoShortcuts)."
    } else {
        Write-Host "5b) Creating Start menu shortcuts in $StartMenuDir"
        New-Item -ItemType Directory -Force -Path $StartMenuDir | Out-Null
        $shell = New-Object -ComObject WScript.Shell
        $ps = Join-Path $env:SystemRoot "System32\WindowsPowerShell\v1.0\powershell.exe"
        $links = @(
            @{ Name = "Update IME Live Converter.lnk";    Script = "update.ps1";    Args = "-Pause" },
            @{ Name = "Uninstall IME Live Converter.lnk"; Script = "uninstall.ps1"; Args = "" }
        )
        foreach ($l in $links) {
            $lnk = $shell.CreateShortcut((Join-Path $StartMenuDir $l.Name))
            $lnk.TargetPath = $ps
            $lnk.Arguments = "-NoProfile -ExecutionPolicy Bypass -File " + (Quote (Join-Path $InstallDir $l.Script)) + " " + $l.Args
            $lnk.WorkingDirectory = $InstallDir
            $lnk.Save()
        }
    }

    # ---- 6. start ----------------------------------------------------------------
    if ($NoStart) {
        Write-Host "6) Not starting (-NoStart)."
    } elseif (-not $NoAutostart) {
        Write-Host "6) Starting..."
        Start-ScheduledTask -TaskName $TaskName
    } else {
        Write-Host "6) Starting..."
        Start-Process "wscript.exe" -ArgumentList (Quote $vbs) -WorkingDirectory $InstallDir
    }

    Write-Host ""
    Write-Host "Done: IME Live Converter $newVersion (models $modelsVersion)."
    Write-Host "To update:    powershell -ExecutionPolicy Bypass -File `"$(Join-Path $InstallDir 'update.ps1')`""
    Write-Host "To uninstall: powershell -ExecutionPolicy Bypass -File `"$(Join-Path $InstallDir 'uninstall.ps1')`""
    Finish 0
} catch {
    Write-Host ""
    Write-Host "Install failed: $($_.Exception.Message)" -ForegroundColor Red
    Finish 1
}
