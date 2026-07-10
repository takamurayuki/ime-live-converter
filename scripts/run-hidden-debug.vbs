' 診断用ランチャ: IME_DEBUG_LOG=1 を付けて、コンソール無し(--background)で起動する。
' コンソール窓（PowerShell/cmd）のカーソル位置取得がなぜ効かないかを
' C:\Projects\ime-live-converter\hook_debug.log で確認するため。
'
' 使い方:  wscript scripts\run-hidden-debug.vbs
' 停止:    タスクマネージャーで conversion-service.exe を終了
' ログ:    hook_debug.log の "console:" 行を確認（キー入力も平文で残るので調査後は削除推奨）

Set sh  = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")

root = fso.GetParentFolderName(fso.GetParentFolderName(WScript.ScriptFullName))
exe  = root & "\target\release\conversion-service.exe"

If Not fso.FileExists(exe) Then
    MsgBox "先に 'cargo build --release' でビルドしてください:" & vbCrLf & exe, 48, "IME Live Converter"
    WScript.Quit 1
End If

' --debug でプロセス内から確実にログ有効化（環境変数の受け渡しに依存しない）
sh.CurrentDirectory = root
sh.Run """" & exe & """ --background --debug", 0, False
