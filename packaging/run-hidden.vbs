' Launcher for the installed IME Live Converter.
' Starts conversion-service.exe (next to this file) hidden and resident.
' The working directory is the install folder so dictionaries\ is found.
'
' Manual start:  wscript run-hidden.vbs
' Debug log:     wscript run-hidden.vbs /debug   (log: %LOCALAPPDATA%\IMELiveConverter\hook_debug.log)
' Stop:          end conversion-service.exe in Task Manager
'
' NOTE: ASCII-only so Windows Script Host reads it regardless of code page.

Set sh  = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")

dir = fso.GetParentFolderName(WScript.ScriptFullName)
exe = dir & "\conversion-service.exe"

If Not fso.FileExists(exe) Then
    MsgBox "conversion-service.exe was not found:" & vbCrLf & exe, 48, "IME Live Converter"
    WScript.Quit 1
End If

args = " --background"
If WScript.Arguments.Count > 0 Then
    If LCase(WScript.Arguments(0)) = "/debug" Then args = args & " --debug"
End If

sh.CurrentDirectory = dir
' 0 = hidden window, False = do not wait
sh.Run """" & exe & """" & args, 0, False
