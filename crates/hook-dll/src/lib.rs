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
        SWP_SHOWWINDOW, SW_HIDE, WINDOWS_HOOK_ID, WM_IME_CONTROL, WM_KEYDOWN, WM_LBUTTONDOWN,
        WM_NOTIFY, WM_PAINT, WM_SYSKEYDOWN, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_POPUP,
    },
    UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, INPUT_0,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        VK_BACK, VK_RETURN, VK_ESCAPE, VK_SPACE, VK_TAB, VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN,
        VK_HOME, VK_END, VK_SHIFT, VK_CONTROL, VK_MENU,
        VIRTUAL_KEY,
    },
    UI::Input::Ime::{
        ImmGetDefaultIMEWnd,
        IME_CMODE_NATIVE, IME_CMODE_KATAKANA,
    },
    System::LibraryLoader::{GetModuleHandleW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT},
};
use windows::core::w;

use common::{RomajiConverter, Dictionary, ViterbiConverter, LearningRepository};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// デバッグログ有効判定（IME_DEBUG_LOG=1 のときのみ）
///
/// このログはシステム全体のキー入力を平文でファイルに残すため
/// （パスワード入力も含まれ得る）、既定では完全に無効。
/// 調査時のみ `IME_DEBUG_LOG=1` で起動して有効化すること。
fn debug_log_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("IME_DEBUG_LOG").map(|v| v == "1").unwrap_or(false)
    })
}

// デバッグログ（UTF-8 BOM付きで出力、IME_DEBUG_LOG=1 のときのみ）
#[allow(unused_macros)]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if crate::debug_log_enabled() {
            use std::io::Write;
            let path = "C:\\Projects\\ime-live-converter\\hook_debug.log";
            let needs_bom = !std::path::Path::new(path).exists();
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                if needs_bom {
                    let _ = file.write_all(&[0xEF, 0xBB, 0xBF]);
                }
                let _ = writeln!(file, "[IME] {}", format!($($arg)*));
            }
        }
    };
}

// グローバル変数
static mut HOOK_HANDLE: Option<HHOOK> = None;
/// 変換状態（OnceLock: `static mut` への参照は未定義動作の恐れがあり警告になるため）。
/// 解放はできないため、アンインストール後も保持したまま（プロセス終了で回収）。
static LIVE_CONTEXT: OnceLock<Mutex<LiveConversionState>> = OnceLock::new();
static mut IS_ENABLED: bool = false;
/// 我々がIMEとして動作中か。初期は false (まだ初回キー入力で未判定の意味も兼ねる)
static mut OUR_ACTIVE: bool = false;
/// 初回キー入力での MS-IME 状態確認を済ませたか
static mut INITIAL_CHECK_DONE: bool = false;

// 機能別モジュール（詳細は各ファイル先頭の //! を参照）
mod command_mode;
mod conversion;
mod hook;
mod popup;
mod settings_ui;
mod uia;

// 旧単一ファイル時代からの相互参照が多いため、クレート内へフラットに再公開する
pub(crate) use command_mode::*;
pub(crate) use conversion::*;
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

        // 入力欄の位置を追う UI Automation ポーラーを開始（ポップアップ位置用）
        start_uia_poller();
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
        
        let hook = SetWindowsHookExW(
            WINDOWS_HOOK_ID(13), // WH_KEYBOARD_LL (14はWH_MOUSE_LL)
            Some(LowLevelKeyboardProc),
            hinstance,
            0,
        );

        match hook {
            Ok(h) => {
                HOOK_HANDLE = Some(h);
                println!("Keyboard hook installed successfully");
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
        hide_candidate_window();
        let candidate_hwnd = CANDIDATE_HWND; // static mut への参照(take)を避けるため値コピー
        CANDIDATE_HWND = None;
        if let Some(hwnd) = candidate_hwnd {
            use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
            let _ = DestroyWindow(hwnd);
        }

        if let Some(hook) = HOOK_HANDLE {
            let result = UnhookWindowsHookEx(hook);
            HOOK_HANDLE = None;
            // LIVE_CONTEXT (OnceLock) は解放できないが、プロセス終了時に回収されるため
            // ここでは触らない（再インストール時は install_hook が中身を入れ替える）。
            println!("Keyboard hook uninstalled");
            result.is_ok()
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