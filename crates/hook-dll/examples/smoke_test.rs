//! フックスレッド分離後の動作確認専用の最小スモークテスト。
//! keystroke_harness.rsと同じ安全策（カウントダウン＋1文字ごとのフォーカス
//! 確認）を使い、既知の正しい変換結果を持つ短いフレーズを1つだけ打って
//! Enterで確定する。実際に変換されたかどうかは呼び出し側がNotepadの
//! テキストを読んで確認すること。

use std::thread;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, BOOL};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowTextLengthW, IsWindowVisible,
};

const HARNESS_MAGIC: usize = 0x5445_5354; // "TEST"

fn find_window_by_title_substr(needle: &str) -> Option<HWND> {
    struct SearchState { needle_lower: String, found: Option<HWND> }
    let mut state = SearchState { needle_lower: needle.to_lowercase(), found: None };
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let state = &mut *(lparam.0 as *mut SearchState);
            if !IsWindowVisible(hwnd).as_bool() { return BOOL(1); }
            let len = GetWindowTextLengthW(hwnd);
            if len == 0 { return BOOL(1); }
            let mut buf = vec![0u16; len as usize + 1];
            let read = GetWindowTextW(hwnd, &mut buf);
            if read == 0 { return BOOL(1); }
            let title = String::from_utf16_lossy(&buf[..read as usize]);
            if title.to_lowercase().contains(&state.needle_lower) {
                state.found = Some(hwnd);
                return BOOL(0);
            }
            BOOL(1)
        }
    }
    unsafe { let _ = EnumWindows(Some(enum_proc), LPARAM(&mut state as *mut _ as isize)); }
    state.found
}

fn key_down_up(vk: VIRTUAL_KEY) {
    let inputs = [
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT {
            wVk: vk, wScan: 0, dwFlags: KEYBD_EVENT_FLAGS(0), time: 0, dwExtraInfo: HARNESS_MAGIC } } },
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT {
            wVk: vk, wScan: 0, dwFlags: KEYEVENTF_KEYUP, time: 0, dwExtraInfo: HARNESS_MAGIC } } },
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32); }
}

fn char_to_vk(ch: char) -> Option<VIRTUAL_KEY> {
    match ch {
        'a'..='z' => Some(VIRTUAL_KEY(ch.to_ascii_uppercase() as u16)),
        _ => None,
    }
}

fn require_foreground_or_abort(target: HWND) {
    let current = unsafe { GetForegroundWindow() };
    if current != target {
        eprintln!("中断: フォーカスが対象ウィンドウから外れました。");
        std::process::exit(2);
    }
}

fn main() {
    let target = std::env::args().nth(1).unwrap_or_else(|| "メモ帳".to_string());
    let Some(hwnd) = find_window_by_title_substr(&target) else {
        eprintln!("ウィンドウが見つかりません: {target}");
        std::process::exit(1);
    };

    println!("5秒以内に対象ウィンドウ（\"{target}\"）を手動でクリックして最前面にしてください。");
    for remaining in (1..=5).rev() {
        print!("\r  開始まで {remaining} 秒...   ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        thread::sleep(Duration::from_secs(1));
    }
    println!();

    if unsafe { GetForegroundWindow() } != hwnd {
        eprintln!("中断: 対象ウィンドウが最前面になっていません。1文字も送信していません。");
        std::process::exit(3);
    }

    // 既知の正しい変換結果を持つ短いフレーズ。
    // きょうはいいてんきです -> 今日はいい天気です
    let phrase = "kyouhaiitenkidesu";
    for ch in phrase.chars() {
        require_foreground_or_abort(hwnd);
        if let Some(vk) = char_to_vk(ch) {
            key_down_up(vk);
            thread::sleep(Duration::from_millis(40));
        }
    }
    require_foreground_or_abort(hwnd);
    key_down_up(VK_RETURN);
    println!("送信完了: '{phrase}' -> Enter。Notepadの内容を確認してください。");
}
