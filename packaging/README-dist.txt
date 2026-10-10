IME Live Converter
==================

Windows 用のライブ変換 IME（ローマ字入力を打鍵中に漢字かな交じり文へ変換）です。
Rust などの開発環境は不要です。

■ インストール / 更新（同じ手順）
  1. この ZIP を任意のフォルダに展開する
  2. 展開したフォルダで PowerShell を開き、次を実行する
       powershell -ExecutionPolicy Bypass -File install.ps1
  3. 管理者権限の確認が出たら「はい」（ログオン時の自動起動を登録するため）

  - 初回（またはモデル更新時）だけ、辞書・判断層モデル（約 370MB）を
    GitHub Releases から自動でダウンロードします。
    オフラインで入れる場合は、system.dic / judge_lm.bin / judge_cooc.bin を
    置いたフォルダを -ModelsDir で指定してください。
  - プログラムは %LOCALAPPDATA%\Programs\IMELiveConverter に入ります。
  - 学習データは %LOCALAPPDATA%\IMELiveConverter に保存され、更新しても消えません。

■ アンインストール
    powershell -ExecutionPolicy Bypass -File "%LOCALAPPDATA%\Programs\IMELiveConverter\uninstall.ps1"
  学習データも消すかを確認されます（既定は残す）。

■ 注意
  - キー入力を受け取る仕組み（キーボードフック）のため、ウイルス対策ソフトや
    SmartScreen に警告されることがあります。
  - 版は conversion-service.exe --version、またはコマンド設定画面のタイトルで確認できます。

■ ライセンス
  本体: MIT License
  辞書: IPA 辞書（licenses\IPADIC-COPYING.txt）
  判断層モデル: 日本語 Wikipedia（執筆者: Wikipedia の寄稿者）の本文から統計的に
  学習したもの。モデル自体を CC BY-SA 4.0 で配布（licenses\NOTICE.txt）
