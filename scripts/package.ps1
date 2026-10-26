# Build the distributable packages into dist\.
#
#   dist\IMELiveConverter-<version>-x64.zip   program (a few MB): exe, dll,
#                                            launcher, install/uninstall scripts,
#                                            small bundled data, licenses
#   dist\<models-version>\                    (-Models only) the model files to
#                                            upload as the GitHub Release
#                                            <models-version>: system.dic,
#                                            judge_lm.bin, judge_cooc.bin,
#                                            SHA256SUMS.txt, NOTICE.txt,
#                                            IPADIC-COPYING.txt
#
# <version> comes from [workspace.package] in Cargo.toml and <models-version>
# from packaging\models-version.txt (see docs/release-and-versioning.md).
#
# Builds into target\package by default so it does not collide with a running
# IME that was started from target\release.
#
# Usage:
#     powershell -ExecutionPolicy Bypass -File scripts\package.ps1 [-Models] [-SkipBuild] [-TargetDir target]
#
# NOTE: ASCII-only so Windows PowerShell 5.1 parses it regardless of code page.
param(
    [switch]$Models,
    [switch]$SkipBuild,
    [string]$TargetDir = "target\package",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# ---- versions ---------------------------------------------------------------
$cargo = Get-Content (Join-Path $root "Cargo.toml") -Raw
if ($cargo -notmatch '(?s)\[workspace\.package\].*?\r?\nversion\s*=\s*"([^"]+)"') {
    throw "version not found in [workspace.package] of Cargo.toml"
}
$version = $Matches[1]
$modelsVersion = (Get-Content (Join-Path $root "packaging\models-version.txt") -Raw).Trim()
Write-Host "Packaging IME Live Converter $version (models $modelsVersion)"

# ---- build --------------------------------------------------------------------
$env:CARGO_TARGET_DIR = Join-Path $root $TargetDir
if (-not $SkipBuild) {
    Write-Host "Building (release, static CRT) into $TargetDir..."
    cargo build --release -p conversion-service -p hook-dll
    if ($LASTEXITCODE -ne 0) { throw "Build failed." }
}
$bin = Join-Path $env:CARGO_TARGET_DIR "release"

# ---- program package ------------------------------------------------------------
$name  = "IMELiveConverter-$version-x64"
$out   = Join-Path $root $OutDir
$stage = Join-Path $out $name
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $stage "dictionaries") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $stage "licenses") | Out-Null

Copy-Item (Join-Path $bin "conversion-service.exe") $stage
Copy-Item (Join-Path $bin "hook_dll.dll") $stage
Copy-Item (Join-Path $root "packaging\run-hidden.vbs") $stage
Copy-Item (Join-Path $root "packaging\install.ps1") $stage
Copy-Item (Join-Path $root "packaging\uninstall.ps1") $stage
Copy-Item (Join-Path $root "packaging\update.ps1") $stage
Copy-Item (Join-Path $root "packaging\README-dist.txt") (Join-Path $stage "README.txt")
Copy-Item (Join-Path $root "packaging\licenses\*") (Join-Path $stage "licenses")
foreach ($f in @("commands.csv", "word_priority.tsv", "word_assoc.tsv", "english_words.tsv", "english_tech.txt", "sample.dic")) {
    Copy-Item (Join-Path $root "dictionaries\$f") (Join-Path $stage "dictionaries")
}
Set-Content -Path (Join-Path $stage "VERSION") -Value $version -Encoding ASCII
Set-Content -Path (Join-Path $stage "MODELS_VERSION_REQUIRED") -Value $modelsVersion -Encoding ASCII

# the built exe must report the same version (guards against a stale build)
$reported = (& (Join-Path $stage "conversion-service.exe") --version | Select-Object -First 1).Trim()
if ($reported -ne $version) { throw "conversion-service.exe reports $reported, expected $version" }

$zip = Join-Path $out "$name.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $stage -DestinationPath $zip
# update.ps1 checks the download against this file (uploaded next to the zip)
$zipHash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
Set-Content -Path "$zip.sha256" -Value "$zipHash  $name.zip" -Encoding ASCII
Write-Host "Program package: $zip"

# ---- models (uploaded once per models version, not per program release) --------
if ($Models) {
    $mstage = Join-Path $out $modelsVersion
    if (Test-Path $mstage) { Remove-Item $mstage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $mstage | Out-Null
    $lines = @()
    foreach ($f in @("system.dic", "judge_lm.bin", "judge_cooc.bin")) {
        $src = Join-Path $root "dictionaries\$f"
        if (-not (Test-Path $src)) { throw "Model file not found: $src" }
        Copy-Item $src $mstage
        $hash = (Get-FileHash -Algorithm SHA256 (Join-Path $mstage $f)).Hash.ToLower()
        $lines += "$hash  $f"
    }
    Set-Content -Path (Join-Path $mstage "SHA256SUMS.txt") -Value $lines -Encoding ASCII
    # The models are published on their own, so they carry their license notices.
    Copy-Item (Join-Path $root "packaging\licenses\NOTICE.txt") $mstage
    Copy-Item (Join-Path $root "packaging\licenses\IPADIC-COPYING.txt") $mstage
    Write-Host "Models folder:   $mstage"
    Write-Host "Publish it with: powershell -ExecutionPolicy Bypass -File scripts\publish-models.ps1"
}
