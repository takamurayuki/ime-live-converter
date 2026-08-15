# Run the full workspace test suite (cargo test --workspace).
#
# Single entry point for local regression checks: common's viterbi/candidate/
# learning tests and hook-dll's conversion/hook/command_mode tests all run
# from one command. Exits non-zero on any test failure so it composes with
# other scripts or a future CI step.
#
# Usage (PowerShell; no admin needed):
#     powershell -ExecutionPolicy Bypass -File scripts\run-tests.ps1
#
# NOTE: This file is intentionally ASCII-only so Windows PowerShell 5.1 parses it
#       regardless of the system code page.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Host "Running cargo test --workspace..."
cargo test --workspace
if ($LASTEXITCODE -ne 0) {
    Write-Error "Tests failed. See the errors above."
    exit 1
}

Write-Host "All tests passed."
