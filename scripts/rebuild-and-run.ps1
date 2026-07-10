<#
    常駐IMEを止めて、ビルドし直し、非表示で再起動する一括スクリプト。

    「conversion-service.exe が起動したままだとビルドが上書きできず
     『アクセスが拒否されました (os error 5)』で失敗する」問題を防ぐため、
    停止 → ビルド → 起動 を正しい順序で行う。

    使い方（PowerShell で。管理者権限は不要）:
        powershell -ExecutionPolicy Bypass -File scripts\rebuild-and-run.ps1

    デバッグログ付きで起動したい場合:
        powershell -ExecutionPolicy Bypass -File scripts\rebuild-and-run.ps1 -Debug
#>
param([switch]$Debug)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Host "1) 常駐IMEを停止..."
Get-Process conversion-service -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

Write-Host "2) ビルド (cargo build --release)..."
cargo build --release
if ($LASTEXITCODE -ne 0) {
    Write-Error "ビルド失敗。上のエラーを確認してください。"
    exit 1
}

$vbs = if ($Debug) { "run-hidden-debug.vbs" } else { "run-hidden.vbs" }
Write-Host "3) 非表示で起動 ($vbs)..."
Start-Process "wscript.exe" -ArgumentList ('"{0}"' -f (Join-Path $PSScriptRoot $vbs))

Write-Host "完了。IMEが常駐しました。停止するには:"
Write-Host "    Get-Process conversion-service | Stop-Process -Force"
