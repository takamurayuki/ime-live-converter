# Update IME Live Converter to the latest release on GitHub.
#
# Looks up the latest release, and when it is newer than the installed version:
# downloads the program package -> checks its SHA-256 -> extracts it -> runs its
# install.ps1 (which stops the IME, replaces the program, fetches new models only
# when needed and starts it again). The learning DB and logs in
# %LOCALAPPDATA%\IMELiveConverter are never touched, so learning survives.
# install.ps1 installs into the same folder this script was installed into.
#
# Usage (installed next to the program; the Start menu has a shortcut for it):
#     powershell -ExecutionPolicy Bypass -File "%LOCALAPPDATA%\Programs\IMELiveConverter\update.ps1"
# Options:
#     -CheckOnly        only report whether a newer version exists
#     -Version <x.y.z>  install that version instead of the latest (also older ones)
#     -Pause            wait for Enter before closing (used by the Start menu shortcut)
#
# NOTE: ASCII-only so Windows PowerShell 5.1 parses it regardless of code page.
param(
    [string]$InstallDir = "",
    [string]$Version = "",
    [switch]$CheckOnly,
    [switch]$Pause
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"   # Invoke-WebRequest is very slow with the progress bar on PS 5.1

$Repo = "takamurayuki/ime-live-converter"
if (-not $InstallDir) { $InstallDir = $PSScriptRoot }

function Finish([int]$code) {
    if ($Pause) { Read-Host "Press Enter to close" | Out-Null }
    exit $code
}

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $headers = @{ "User-Agent" = "IMELiveConverter-update"; "Accept" = "application/vnd.github+json" }

    # ---- installed version -----------------------------------------------------
    $current = $null
    $versionFile = Join-Path $InstallDir "VERSION"
    if (Test-Path $versionFile) { $current = (Get-Content $versionFile -Raw).Trim() }
    if (-not $current) { Write-Host "Installed version: (not found in $InstallDir)" }
    else               { Write-Host "Installed version: $current" }

    # ---- release to install ------------------------------------------------------
    # "latest" never points at a models release (they are published with --latest=false)
    if ($Version) {
        $url = "https://api.github.com/repos/$Repo/releases/tags/v$($Version.TrimStart('v'))"
    } else {
        $url = "https://api.github.com/repos/$Repo/releases/latest"
    }
    try {
        $release = Invoke-RestMethod -UseBasicParsing -Uri $url -Headers $headers
    } catch {
        $status = $null
        if ($_.Exception.Response) { $status = [int]$_.Exception.Response.StatusCode }
        if ($status -eq 404) {
            if ($Version) { throw "Release v$($Version.TrimStart('v')) was not found on GitHub ($Repo)." }
            throw "No release has been published on GitHub yet ($Repo)."
        }
        throw "Could not reach GitHub to check for updates: $($_.Exception.Message)"
    }
    $target = $release.tag_name.TrimStart('v')
    Write-Host "Release:           $target  ($($release.html_url))"

    if (-not $Version -and $current) {
        $newer = $false
        try { $newer = ([version]$target) -gt ([version]$current) } catch { $newer = ($target -ne $current) }
        if (-not $newer) {
            Write-Host "You already have the latest version."
            Finish 0
        }
    }
    if ($CheckOnly) {
        Write-Host "A newer version is available: $current -> $target"
        Write-Host "Run update.ps1 without -CheckOnly to install it."
        Finish 0
    }

    # ---- download and verify -------------------------------------------------------
    $zipAsset = $release.assets | Where-Object { $_.name -like "IMELiveConverter-*-x64.zip" } | Select-Object -First 1
    if (-not $zipAsset) { throw "The release $target has no program package (IMELiveConverter-*-x64.zip)." }
    $sumAsset = $release.assets | Where-Object { $_.name -eq "$($zipAsset.name).sha256" } | Select-Object -First 1

    $work = Join-Path $env:TEMP "IMELiveConverter-update-$target"
    if (Test-Path $work) { Remove-Item $work -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $work | Out-Null
    $zip = Join-Path $work $zipAsset.name
    Write-Host "Downloading $($zipAsset.name)..."
    Invoke-WebRequest -UseBasicParsing -Uri $zipAsset.browser_download_url -OutFile $zip -Headers @{ "User-Agent" = $headers["User-Agent"] }

    if ($sumAsset) {
        $sumText = (Invoke-WebRequest -UseBasicParsing -Uri $sumAsset.browser_download_url -Headers @{ "User-Agent" = $headers["User-Agent"] }).Content
        if ($sumText -is [byte[]]) { $sumText = [Text.Encoding]::ASCII.GetString($sumText) }
        $expected = ($sumText -split '\s+')[0].Trim().ToUpper()
        $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash
        if ($actual -ne $expected) { throw "Checksum mismatch for $($zipAsset.name) (download broken or tampered; nothing was changed)." }
        Write-Host "Checksum OK."
    } else {
        Write-Host "Warning: the release has no checksum file; skipping verification." -ForegroundColor Yellow
    }

    # ---- extract and install ---------------------------------------------------------
    $extract = Join-Path $work "package"
    Expand-Archive -Path $zip -DestinationPath $extract -Force
    $installer = Get-ChildItem $extract -Recurse -Filter "install.ps1" | Select-Object -First 1
    if (-not $installer) { throw "install.ps1 was not found in the downloaded package." }
    Write-Host "Running install.ps1 from the new package..."
    & powershell -NoProfile -ExecutionPolicy Bypass -File $installer.FullName -InstallDir $InstallDir
    if ($LASTEXITCODE -ne 0) { throw "install.ps1 failed (exit code $LASTEXITCODE). The previous version may need to be started again." }
    Write-Host ""
    Write-Host "Updated to $target."
    Finish 0
} catch {
    Write-Host ""
    Write-Host "Update failed: $($_.Exception.Message)" -ForegroundColor Red
    Finish 1
}
