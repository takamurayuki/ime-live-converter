use anyhow::{Context, Result};
use std::io::{self, Write, BufRead};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::sync::mpsc;
use windows::Win32::{
    Foundation::HMODULE,
    System::LibraryLoader::{GetProcAddress, LoadLibraryW},
    UI::WindowsAndMessaging::{
        PeekMessageW, TranslateMessage, DispatchMessageW, 
        MSG, PM_REMOVE, WM_QUIT,
    },
};
use windows::core::PCWSTR;

/// DLLの関数型定義
type InstallHookFn = extern "C" fn() -> bool;
type UninstallHookFn = extern "C" fn() -> bool;
type LoadDictionaryFn = extern "C" fn(*const u8, usize) -> bool;
type SetEnabledFn = extern "C" fn(bool);
type IsEnabledFn = extern "C" fn() -> bool;

/// DLLハンドラ
struct HookDll {
    #[allow(dead_code)]
    module: HMODULE,
    install_hook: InstallHookFn,
    uninstall_hook: UninstallHookFn,
    load_dictionary: LoadDictionaryFn,
    set_enabled: SetEnabledFn,
    is_enabled: IsEnabledFn,
}

impl HookDll {
    /// DLLをロード
    fn load(dll_path: &str) -> Result<Self> {
        unsafe {
            // パスをワイド文字列に変換
            let wide_path: Vec<u16> = dll_path.encode_utf16().chain(std::iter::once(0)).collect();
            
            let module = LoadLibraryW(PCWSTR(wide_path.as_ptr()))
                .context("hook_dll.dllのロードに失敗")?;

            // 関数ポインタを取得
            let install_hook = std::mem::transmute::<_, InstallHookFn>(
                GetProcAddress(module, windows::core::s!("install_hook"))
                    .ok_or_else(|| anyhow::anyhow!("install_hook関数が見つかりません"))?
            );

            let uninstall_hook = std::mem::transmute::<_, UninstallHookFn>(
                GetProcAddress(module, windows::core::s!("uninstall_hook"))
                    .ok_or_else(|| anyhow::anyhow!("uninstall_hook関数が見つかりません"))?
            );

            let load_dictionary = std::mem::transmute::<_, LoadDictionaryFn>(
                GetProcAddress(module, windows::core::s!("load_dictionary"))
                    .ok_or_else(|| anyhow::anyhow!("load_dictionary関数が見つかりません"))?
            );

            let set_enabled = std::mem::transmute::<_, SetEnabledFn>(
                GetProcAddress(module, windows::core::s!("set_enabled"))
                    .ok_or_else(|| anyhow::anyhow!("set_enabled関数が見つかりません"))?
            );

            let is_enabled = std::mem::transmute::<_, IsEnabledFn>(
                GetProcAddress(module, windows::core::s!("is_enabled"))
                    .ok_or_else(|| anyhow::anyhow!("is_enabled関数が見つかりません"))?
            );

            Ok(Self {
                module,
                install_hook,
                uninstall_hook,
                load_dictionary,
                set_enabled,
                is_enabled,
            })
        }
    }

    /// フックをインストール
    fn install(&self) -> bool {
        (self.install_hook)()
    }

    /// フックをアンインストール
    fn uninstall(&self) -> bool {
        (self.uninstall_hook)()
    }

    /// 辞書をロード
    fn load_dict(&self, path: &str) -> bool {
        (self.load_dictionary)(path.as_ptr(), path.len())
    }

    /// 有効/無効を切り替え
    fn set_enabled(&self, enabled: bool) {
        (self.set_enabled)(enabled)
    }

    /// 有効かどうかを取得
    fn is_enabled(&self) -> bool {
        (self.is_enabled)()
    }
}

fn find_dll_path() -> Result<String> {
    // 実行ファイルと同じディレクトリを探す
    let exe_dir = std::env::current_exe()?
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let dll_path = exe_dir.join("hook_dll.dll");
    if dll_path.exists() {
        return Ok(dll_path.to_string_lossy().to_string());
    }

    // カレントディレクトリを探す
    let current_dir = std::env::current_dir()?;
    let dll_path = current_dir.join("hook_dll.dll");
    if dll_path.exists() {
        return Ok(dll_path.to_string_lossy().to_string());
    }

    // target/debug を探す
    let dll_path = current_dir.join("target/debug/hook_dll.dll");
    if dll_path.exists() {
        return Ok(dll_path.to_string_lossy().to_string());
    }

    anyhow::bail!("hook_dll.dll が見つかりません")
}

fn print_help() {
    println!();
    println!("=== IME Live Converter ===");
    println!();
    println!("コマンド:");
    println!("  h, help     - このヘルプを表示");
    println!("  t, toggle   - 変換の有効/無効を切り替え");
    println!("  s, status   - 現在の状態を表示");
    println!("  d, dict     - 辞書をロード（パスを指定）");
    println!("  q, quit     - 終了");
    println!();
}

/// コマンド
enum Command {
    Quit,
    Help,
    Toggle,
    Status,
    LoadDict(String),
}

fn main() -> Result<()> {
    // --debug: フック側のデバッグログ(IME_DEBUG_LOG)を確実に有効化する。
    //   DLL は同一プロセスの環境変数を読むので、ロード前にここで設定する。
    //   （VBS の環境変数設定が子へ伝わらないケースの回避）
    if std::env::args().any(|a| a == "--debug") {
        std::env::set_var("IME_DEBUG_LOG", "1");
        println!("デバッグログ有効: C:\\Projects\\ime-live-converter\\hook_debug.log");
    }

    // --background / -b: 標準入力を読まず、常駐（ログオン時自動起動向け）
    let background = std::env::args().any(|a| a == "--background" || a == "-b");

    println!("IME Live Converter を起動しています...");
    println!();

    // DLLをロード
    let dll_path = find_dll_path()?;
    println!("DLLをロード: {}", dll_path);
    
    let dll = HookDll::load(&dll_path)?;

    // フックをインストール（メインスレッドで！）
    if dll.install() {
        println!("キーボードフックをインストールしました");
    } else {
        anyhow::bail!("キーボードフックのインストールに失敗しました");
    }

    // 辞書を絶対パスで探してロード（フル辞書 system.dic を優先）
    let exe_dir = std::env::current_dir()?;
    let sample_paths = [
        exe_dir.join("dictionaries/system.dic"),
        exe_dir.join("dictionaries/sample.dic"),
    ];
    
    let mut dict_loaded = false;
    for path in &sample_paths {
        if path.exists() {
            let path_str = path.to_string_lossy().to_string();
            println!("辞書をロード: {}", path_str);
            if dll.load_dict(&path_str) {
                println!("辞書のロードに成功しました");
                dict_loaded = true;
                break;
            }
        }
    }
    
    if !dict_loaded {
        println!("警告: 辞書が見つかりません。ひらがな変換のみ動作します。");
    }

    if background {
        println!("バックグラウンドモードで常駐します（終了はプロセスを停止）。");
    } else {
        print_help();
        println!("対話モードで動作中。'q' で終了、'h' でヘルプ。");
        println!();
    }

    // コマンド受信用チャネル
    let (tx, rx) = mpsc::channel::<Command>();
    // 元の tx を main 側で保持し、入力スレッドが終了（標準入力EOF）しても
    // rx が切断されて常駐が勝手に終わらないようにする。
    let _keep_tx = tx.clone();

    // 対話モードのみ標準入力を読むスレッドを起動する。
    // バックグラウンドでは標準入力が無い（あるいは即EOF）ため起動しない。
    if !background {
        thread::spawn(move || {
            let stdin = io::stdin();
            let reader = stdin.lock();

            for line in reader.lines() {
                if let Ok(input) = line {
                    let cmd = input.trim().to_lowercase();
                    let command = match cmd.as_str() {
                        "q" | "quit" | "exit" => Some(Command::Quit),
                        "h" | "help" => Some(Command::Help),
                        "t" | "toggle" => Some(Command::Toggle),
                        "s" | "status" => Some(Command::Status),
                        _ if cmd.starts_with("d ") || cmd.starts_with("dict ") => {
                            let path = cmd.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
                            Some(Command::LoadDict(path))
                        }
                        "" => None,
                        _ => {
                            println!("不明なコマンド: {}", cmd);
                            None
                        }
                    };

                    if let Some(cmd) = command {
                        if tx.send(cmd).is_err() {
                            break;
                        }
                    }
                    print!("> ");
                    let _ = io::stdout().flush();
                }
            }
        });

        print!("> ");
        let _ = io::stdout().flush();
    }

    // メインスレッドでメッセージループを実行
    let running = Arc::new(AtomicBool::new(true));
    
    unsafe {
        let mut msg = MSG::default();
        while running.load(Ordering::Relaxed) {
            // メッセージを処理
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    running.store(false, Ordering::Relaxed);
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            
            // コマンドをチェック（非ブロッキング）
            match rx.try_recv() {
                Ok(Command::Quit) => {
                    running.store(false, Ordering::Relaxed);
                }
                Ok(Command::Help) => {
                    print_help();
                    print!("> ");
                    let _ = io::stdout().flush();
                }
                Ok(Command::Toggle) => {
                    let enabled = !dll.is_enabled();
                    dll.set_enabled(enabled);
                    println!("変換: {}", if enabled { "有効" } else { "無効" });
                    print!("> ");
                    let _ = io::stdout().flush();
                }
                Ok(Command::Status) => {
                    println!("変換: {}", if dll.is_enabled() { "有効" } else { "無効" });
                    print!("> ");
                    let _ = io::stdout().flush();
                }
                Ok(Command::LoadDict(path)) => {
                    if dll.load_dict(&path) {
                        println!("辞書をロードしました: {}", path);
                    } else {
                        println!("辞書のロードに失敗しました: {}", path);
                    }
                    print!("> ");
                    let _ = io::stdout().flush();
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    running.store(false, Ordering::Relaxed);
                }
            }
            
            // 少し待機
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    // フックをアンインストール
    dll.uninstall();
    println!("\nキーボードフックをアンインストールしました");

    Ok(())
}
