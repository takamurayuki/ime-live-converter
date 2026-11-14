IME Live Converter
==================

ライブ変換を核にした、Windows 向けの日本語入力効率化ツールです。
ローマ字を打った端から漢字かな交じり文に変換するほか、確定後の選び直し
（変換キー）、画面上の入力欄への移動（Ctrl+無変換 / Ctrl+Shift+無変換）など、
入力の前後の手間も減らします。入力した文章を外部へ送ることはありません。
全機能は Ctrl+Space で一時停止／再開できます。
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

■ 更新（2回目以降）
  スタートメニューの「IME Live Converter」→「Update IME Live Converter」を開くだけです。
  GitHub の最新版を調べ、新しければダウンロード（チェックサムを照合）して入れ替えます。
  学習データはそのまま残ります。コマンドで行う場合:
    powershell -ExecutionPolicy Bypass -File "%LOCALAPPDATA%\Programs\IMELiveConverter\update.ps1"
  新しい版があるかだけ調べる: 上のコマンドに -CheckOnly を付ける
  特定の版を入れる（戻す）:   上のコマンドに -Version 1.0.0 のように付ける

■ アンインストール
    powershell -ExecutionPolicy Bypass -File "%LOCALAPPDATA%\Programs\IMELiveConverter\uninstall.ps1"
  （スタートメニューの「Uninstall IME Live Converter」からも実行できます）
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
