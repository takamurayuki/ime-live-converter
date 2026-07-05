<#
    install-autostart.ps1 で登録した自動起動タスクを解除する。
    起動中のプロセスも停止する。

    使い方（管理者権限の PowerShell で）:
        powershell -ExecutionPolicy Bypass -File scripts\uninstall-autostart.ps1
#>
$ErrorActionPreference = "SilentlyContinue"

$taskName = "IMELiveConverter"

# 実行中の常駐プロセスを停止
Get-Process -Name "conversion-service" -ErrorAction SilentlyContinue | Stop-Process -Force

# タスクを削除
if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
    Write-Host "解除しました: タスク '$taskName' を削除し、プロセスを停止しました。"
} else {
    Write-Host "タスク '$taskName' は登録されていません。"
}
