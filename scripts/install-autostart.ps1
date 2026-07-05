<#
    ログオン時に IME Live Converter を「非表示・常駐」で自動起動する
    スケジュールタスクを登録する。

    ポイント:
      - キーボードフックはユーザーセッションで動く必要があるため、Windows
        サービス(セッション0)ではなく「ログオン時タスク」で常駐させる。
      - 最上位の権限(Highest)で実行し、管理者権限で動くアプリ上でも
        変換が効くようにする（UACプロンプトは出ない）。
      - 実体は run-hidden.vbs 経由で起動し、黒いコンソールを出さない。

    使い方（管理者権限の PowerShell で）:
        powershell -ExecutionPolicy Bypass -File scripts\install-autostart.ps1

    解除:
        powershell -ExecutionPolicy Bypass -File scripts\uninstall-autostart.ps1
#>
$ErrorActionPreference = "Stop"

$taskName = "IMELiveConverter"
$root = Split-Path -Parent $PSScriptRoot          # プロジェクトルート
$vbs  = Join-Path $PSScriptRoot "run-hidden.vbs"
$exe  = Join-Path $root "target\release\conversion-service.exe"

if (-not (Test-Path $exe)) {
    throw "先に 'cargo build --release' でビルドしてください: $exe"
}
if (-not (Test-Path $vbs)) {
    throw "ランチャが見つかりません: $vbs"
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

Write-Host "登録しました: タスク '$taskName'（次回ログオンから自動起動）。"
Write-Host "今すぐ開始する:  Start-ScheduledTask -TaskName $taskName"
Write-Host "状態を確認する:  Get-ScheduledTask -TaskName $taskName"
