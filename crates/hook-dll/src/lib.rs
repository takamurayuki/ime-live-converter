use windows::Win32::{
    Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM, HMODULE},
    Graphics::Gdi::{
        BeginPaint, ClientToScreen, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW,
        EndPaint, FillRect, FrameRect, GetMonitorInfoW, GetTextExtentPoint32W, InvalidateRect,
        MonitorFromPoint, SelectObject, SetBkMode, SetTextColor,
        DT_LEFT, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, HGDIOBJ,
        MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, TRANSPARENT,
    },
    UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, GetClientRect,
        GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics, GetWindowRect,
        GetWindowThreadProcessId, RegisterClassW,
        SendMessageW, SetWindowPos, SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx,
        GUITHREADINFO, HHOOK, HWND_NOTOPMOST, HWND_TOPMOST, KBDLLHOOKSTRUCT, LLKHF_INJECTED, SM_CXSCREEN,
        SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_SHOWWINDOW, SW_HIDE, WINDOWS_HOOK_ID, WM_IME_CONTROL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
        WM_NOTIFY, WM_PAINT, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_POPUP, MSG,
    },
    UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, INPUT_0,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        VK_BACK, VK_RETURN, VK_ESCAPE, VK_SPACE, VK_TAB, VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN,
        VK_SHIFT, VK_CONTROL, VK_MENU,
        VIRTUAL_KEY,
    },
    UI::Input::Ime::{
        ImmGetDefaultIMEWnd,
        IME_CMODE_NATIVE, IME_CMODE_KATAKANA,
    },
    System::LibraryLoader::{GetModuleHandleW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT},
};
use windows::core::w;

use common::LearningRepository;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

// `debug_log!`マクロ本体と`debug_log_enabled`は`common`へ移設した
// （[[golden-test-harness]]でLiveConversionStateをcommonへ移した際、
// そこからの呼び出しを成立させるため）。crateルートで再公開することで、
// 既存の呼び出し箇所（bareな`debug_log!(...)`）は無変更のまま動く。
pub(crate) use common::debug_log;

// グローバル変数
/// キーボードフック専用スレッド（[[hook-latency-log-thread-model]]対応）。
/// `WH_KEYBOARD_LL`は`hook_thread::HookThread`が管理する専用スレッドで
/// インストールする（候補ウィンドウの描画・SQLite書き込み等の同期処理から
/// フック配送を完全に切り離すため）。`HOOK_HANDLE`（後述）はもう使わない。
static HOOK_THREAD: Mutex<Option<hook_thread::HookThread>> = Mutex::new(None);
/// 低レベルマウスフック（ポップアップ外クリックで閉じるため。`hook::LowLevelMouseProc`）。
/// キーボードフックとは異なりUI（メイン）スレッドに残したままにする
/// （クリック検出自体は軽量で、候補ウィンドウの直接操作と同じスレッドに
/// あることに問題は無い）。
static mut MOUSE_HOOK_HANDLE: Option<HHOOK> = None;
/// 変換状態（OnceLock: `static mut` への参照は未定義動作の恐れがあり警告になるため）。
/// 解放はできないため、アンインストール後も保持したまま（プロセス終了で回収）。
static LIVE_CONTEXT: OnceLock<Mutex<LiveConversionState>> = OnceLock::new();
static mut IS_ENABLED: bool = false;
/// 我々がIMEとして動作中か。初期は false (まだ初回キー入力で未判定の意味も兼ねる)
static mut OUR_ACTIVE: bool = false;
/// 初回キー入力での MS-IME 状態確認を済ませたか
static mut INITIAL_CHECK_DONE: bool = false;

// 機能別モジュール（詳細は各ファイル先頭の //! を参照）
// `conversion`（`LiveConversionState`等）と`brackets`は`common`へ移設済み
// （CLIとhook-dllが同一の変換エンジンを使うようにするため。
// [[golden-test-harness]]参照）。
mod command_mode;
mod hook;
mod hook_thread;
mod hook_watchdog;
mod popup;
mod settings_ui;
mod uia;
#[cfg(test)]
mod golden_tests;

// 旧単一ファイル時代からの相互参照が多いため、クレート内へフラットに再公開する
pub(crate) use command_mode::*;
pub(crate) use common::conversion::*;
pub(crate) use hook::*;
pub(crate) use popup::*;
pub(crate) use settings_ui::*;
pub(crate) use uia::*;

/// フックをインストール
#[no_mangle]
pub extern "C" fn install_hook() -> bool {
    unsafe {
        debug_log!("install_hook: デバッグログ有効（IME_DEBUG_LOG=1）");
        // コンテキストを初期化
        let mut state = LiveConversionState::new();
        // 確定時の学習（DB書き込み）はフックの外（メッセージループ）で処理する
        state.defer_learning = true;
        // 判断層（judge-lm）の大きなモデルはバックグラウンドで読み込む
        // （辞書ロード中のロック保持時間を延ばさない）
        state.load_judge_async = true;
        // `common`crateはWin32非依存のため、遅延通知の実手段（PostMessageW）を
        // 持たない。実運用（フック配送）ではここで注入する。
        state.request_deferred_learning = Some(popup::request_deferred_learning);
        // 学習DBをオープン（CLIと共有。失敗しても変換は継続できる）
        match LearningRepository::open("ime-learning.db") {
            Ok(learning) => {
                seed_commands_from_csv(&learning);
                state.learning = Some(learning);
                println!("学習DBをオープン: ime-learning.db");
            }
            Err(e) => {
                eprintln!("学習DBのオープンに失敗（学習なしで継続）: {}", e);
            }
        }
        // OnceLock は解放できないため、再インストール時は中身を入れ替える
        match LIVE_CONTEXT.get() {
            Some(existing) => {
                if let Ok(mut c) = existing.lock() {
                    *c = state;
                }
            }
            None => {
                let _ = LIVE_CONTEXT.set(Mutex::new(state));
            }
        }
        IS_ENABLED = true;
        OUR_ACTIVE = false;
        // Shift追跡（SHIFT_HELD）の初期値をここでだけ実時間状態から拾う。
        // 以降はフック自身がkeydown/keyupを観測して更新するのでレースが無い。
        {
            use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
            hook::SHIFT_HELD = GetAsyncKeyState(VK_SHIFT.0 as i32) < 0;
        }

        // 入力欄の位置を追う UI Automation ポーラーを開始（ポップアップ位置用）
        start_uia_poller();
        // パスワード欄/昇格プロセス判定を、ポーリング（最大250ms遅延）だけに
        // 頼らず、フォーカス変更の瞬間に`Unknown`へ落とすための監視を開始する。
        // UIスレッド（今この関数を呼んでいるスレッド）で行う必要がある
        // （`SetWinEventHook`はWINEVENT_OUTOFCONTEXTでも登録元スレッドが
        // メッセージポンプを回し続けている必要があるため）。
        uia::install_focus_watch();
        // フックレイテンシ統計のバックグラウンド書き出しスレッドを前もって
        // 起動しておく（初回打鍵時にスレッド作成コストが乗らないように）
        hook::hook_latency::ensure_started();
        // （LLM校正は実用性が低いため廃止。誤字補正は「もしかして」＝即時fuzzy）
        INITIAL_CHECK_DONE = false;
        
        // DLLのHINSTANCEを取得
        let mut hmodule: HMODULE = HMODULE::default();
        let proc_addr = install_hook as *const ();
        let result = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(proc_addr as *const u16),
            &mut hmodule,
        );
        
        let hinstance = if result.is_ok() {
            HINSTANCE(hmodule.0)
        } else {
            println!("Warning: Could not get DLL HINSTANCE, using default");
            HINSTANCE::default()
        };
        
        println!("Installing hook with HINSTANCE: {:?}", hinstance);

        // このスレッド（install_hookの呼び出し元＝conversion-serviceの
        // メインスレッド）をUIスレッドとして記録し、候補ウィンドウを
        // 先に（非表示で）作っておく。フックスレッド起動より前に行う
        // こと（post_ui_commandの宛先が無く指示が誰にも引き取られない
        // 事故を防ぐため）。既にUIスレッドとして記録済み（再インストール）
        // でも、候補ウィンドウは`ensure_candidate_window`内の早期returnで
        // 二重生成されない。
        popup::init_ui_thread();

        // WH_KEYBOARD_LLは専用スレッドでインストールする
        // （[[hook-latency-log-thread-model]]。候補ウィンドウの描画や
        // SQLite書き込み等の同期処理からフック配送を切り離すため）。
        match hook_thread::HookThread::start() {
            Ok(ht) => {
                if let Ok(mut guard) = HOOK_THREAD.lock() {
                    *guard = Some(ht);
                }
                println!("Keyboard hook thread started successfully");
                // ポップアップ（予測変換・候補一覧・コマンド候補）の外側をクリック
                // したら閉じるためのマウスフック。キーボードフックとは異なり
                // UIスレッド自身にインストールする（クリック検出は軽量で、
                // 候補ウィンドウの直接操作と同じスレッドにあっても問題ない）。
                // 失敗しても変換自体は動くので警告のみ。
                match SetWindowsHookExW(
                    WINDOWS_HOOK_ID(14), // WH_MOUSE_LL
                    Some(LowLevelMouseProc),
                    hinstance,
                    0,
                ) {
                    Ok(mh) => {
                        MOUSE_HOOK_HANDLE = Some(mh);
                        println!("Mouse hook installed successfully");
                    }
                    Err(e) => {
                        eprintln!("Warning: Failed to install mouse hook (popup will not close on outside click): {:?}", e);
                    }
                }
                true
            }
            Err(e) => {
                eprintln!("Failed to install hook: {:?}", e);
                false
            }
        }
    }
}

/// フックをアンインストール
#[no_mangle]
pub extern "C" fn uninstall_hook() -> bool {
    unsafe {
        IS_ENABLED = false;
        OUR_ACTIVE = false;
        INITIAL_CHECK_DONE = false;
        // 候補ウィンドウを破棄する前に、遅延させていた学習（`WM_APP_FLUSH_LEARNING`
        // 待ち）を今のうちに反映させる。ここで flush せずに DestroyWindow すると、
        // ポストされたメッセージがメッセージループに届く前にウィンドウが消え、
        // 溜めていた学習が失われたまま無効化・アンインストールされてしまう。
        if let Some(context_mutex) = LIVE_CONTEXT.get() {
            if let Ok(mut context) = context_mutex.lock() {
                context.flush_pending_learning();
            }
        }
        // `hide_candidate_window()`は今やUiCommand経由の非同期post（次に
        // WM_APP_UI_COMMANDが処理されるまで反映されない）なので、直後に
        // DestroyWindowする以下の同期的な破棄で代替する（呼ぶ意味が無い）。
        let candidate_hwnd = CANDIDATE_HWND; // static mut への参照(take)を避けるため値コピー
        CANDIDATE_HWND = None;
        if let Some(hwnd) = candidate_hwnd {
            use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
            let _ = DestroyWindow(hwnd);
        }

        if let Some(mh) = MOUSE_HOOK_HANDLE {
            let _ = UnhookWindowsHookEx(mh);
            MOUSE_HOOK_HANDLE = None;
        }
        uia::uninstall_focus_watch();
        // IS_ENABLED=falseは既に立てた（フックスレッドは生きたままでも以降の
        // 打鍵を処理しなくなる）ので、最後にフックスレッドを止めてjoinする。
        // `HookThread::stop`はWM_QUITを送ってポンプを抜けさせ、スレッド自身が
        // 同じスレッドでUnhookWindowsHookExを行ってから終了する
        // （SetWindowsHookExWを呼んだのと同じスレッドからアンフックする必要が
        // あるため）。
        // LIVE_CONTEXT (OnceLock) は解放できないが、プロセス終了時に回収されるため
        // ここでは触らない（再インストール時は install_hook が中身を入れ替える）。
        let taken = HOOK_THREAD.lock().ok().and_then(|mut g| g.take());
        if let Some(mut ht) = taken {
            ht.stop();
            println!("Keyboard hook thread stopped");
            true
        } else {
            false
        }
    }
}

/// 辞書をロード
#[no_mangle]
pub extern "C" fn load_dictionary(path_ptr: *const u8, path_len: usize) -> bool {
    unsafe {
        debug_log!("辞書ロード開始: ptr={:?}, len={}", path_ptr, path_len);
        
        if path_ptr.is_null() || path_len == 0 {
            debug_log!("辞書ロード失敗: パスが無効");
            return false;
        }

        let path_bytes = std::slice::from_raw_parts(path_ptr, path_len);
        let path_str = match std::str::from_utf8(path_bytes) {
            Ok(s) => s,
            Err(e) => {
                debug_log!("辞書ロード失敗: UTF-8エラー: {:?}", e);
                return false;
            }
        };
        
        debug_log!("辞書パス: {}", path_str);

        if let Some(context_mutex) = LIVE_CONTEXT.get() {
            if let Ok(mut context) = context_mutex.lock() {
                let result = context.load_dictionary(Path::new(path_str));
                debug_log!("辞書ロード結果: {}", result);
                return result;
            } else {
                debug_log!("辞書ロード失敗: コンテキストロック失敗");
            }
        } else {
            debug_log!("辞書ロード失敗: コンテキストなし");
        }

        false
    }
}

/// 変換を有効/無効にする
#[no_mangle]
pub extern "C" fn set_enabled(enabled: bool) {
    unsafe {
        IS_ENABLED = enabled;
        if let Some(context_mutex) = LIVE_CONTEXT.get() {
            if let Ok(mut context) = context_mutex.lock() {
                context.enabled = enabled;
            }
        }
    }
}

/// 変換が有効かどうかを取得
#[no_mangle]
pub extern "C" fn is_enabled() -> bool {
    unsafe { IS_ENABLED }
}

/// 自前の設定系ウィンドウ（単語登録・コマンド設定）宛のメッセージなら
/// IsDialogMessage で処理し、処理済みなら true を返す。ホストのメッセージ
/// ループから、TranslateMessage/DispatchMessage の前に毎回呼んでもらう
/// 想定（true が返ったらそちらは呼ばない）。これが無いと、Tab での
/// コントロール間移動などダイアログ標準のキー操作がカスタムウィンドウ
/// では効かない。
#[no_mangle]
pub extern "C" fn try_handle_dialog_message(msg: *const MSG) -> bool {
    if msg.is_null() {
        return false;
    }
    unsafe { settings_ui::try_handle_dialog_message(&*msg) }
}
