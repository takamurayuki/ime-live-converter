# Publish the model files (dist\<models-version>\, made by
# "scripts\package.ps1 -Models") as the GitHub Release <models-version>.
#
# Do this only when the models changed: bump packaging\models-version.txt,
# run package.ps1 -Models, then this script. Program releases (vX.Y.Z) refer to
# the models release named in packaging\models-version.txt; install.ps1 and CI
# download from it. Requires the GitHub CLI (gh) logged in.
#
# The release is PUBLIC when the repository is public. Check the license notes in
# docs/release-and-versioning.md before publishing.
#
# Usage:
#     powershell -ExecutionPolicy Bypass -File scripts\publish-models.ps1
#
# NOTE: ASCII-only so Windows PowerShell 5.1 parses it regardless of code page.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$modelsVersion = (Get-Content (Join-Path $root "packaging\models-version.txt") -Raw).Trim()
$dir = Join-Path $root "dist\$modelsVersion"
$files = @("system.dic", "judge_lm.bin", "judge_cooc.bin", "SHA256SUMS.txt") | ForEach-Object { Join-Path $dir $_ }
foreach ($f in $files) {
    if (-not (Test-Path $f)) { throw "Missing $f (run scripts\package.ps1 -Models first)" }
}

gh release view $modelsVersion *> $null
if ($LASTEXITCODE -eq 0) {
    throw "Release $modelsVersion already exists. Model releases are immutable: bump packaging\models-version.txt instead."
}

$notes = "Model files for IME Live Converter (dictionary and judge models). " +
    "Downloaded automatically by install.ps1; you do not need to download them by hand. " +
    "Licenses: IPA dictionary (licenses/IPADIC-COPYING.txt in the program package); " +
    "judge models are statistics learned from Japanese Wikipedia (CC BY-SA 4.0)."
gh release create $modelsVersion @files --title "Models $modelsVersion" --notes $notes --latest=false
if ($LASTEXITCODE -ne 0) { throw "gh release create failed." }
Write-Host "Published models release $modelsVersion."
