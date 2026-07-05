' conversion-service を「非表示のバックグラウンド」で起動するランチャ。
'
' コンソールアプリをそのまま起動すると黒いウィンドウが出るため、
' WScript.Shell.Run の第2引数 0（ウィンドウ非表示）で起動する。
' 作業ディレクトリをプロジェクトルートに設定し、dictionaries/ を読めるようにする。
'
' 手動で試す:  wscript scripts\run-hidden.vbs
' 停止:        タスクマネージャーで conversion-service.exe を終了

Set sh  = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")

' このスクリプトは scripts\ にあるので、1つ上がプロジェクトルート
root = fso.GetParentFolderName(fso.GetParentFolderName(WScript.ScriptFullName))
exe  = root & "\target\release\conversion-service.exe"

If Not fso.FileExists(exe) Then
    MsgBox "先に 'cargo build --release' でビルドしてください:" & vbCrLf & exe, 48, "IME Live Converter"
    WScript.Quit 1
End If

sh.CurrentDirectory = root
' 0 = ウィンドウ非表示, False = 完了を待たずに戻る
sh.Run """" & exe & """ --background", 0, False
