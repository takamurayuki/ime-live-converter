//! フックのレイテンシ計測用に、実際の低レベルキーボードフックへ
//! SendInputで大量の打鍵を注入する負荷生成ハーネス。
//!
//! 手打ちよりずっと速く、再現性のある条件で母集団を集められる。
//! `hook_latency.log` に統計が出るのは `crates/hook-dll/src/hook.rs` の
//! `hook_latency`モジュール（フック本体）側の仕組みで、このハーネストは
//! 「打鍵を注入するだけ」の別プロセス。
//!
//! 注意点（実装上の必須事項）:
//! - このハーネス自身のSendInputに`SELF_INJECTED_MAGIC`を載せてしまうと、
//!   フック側の自己送信除外チェックに引っかかり、注入した打鍵が
//!   一切処理されない（無音の失敗）。ここでは意図的にそのマーカーを
//!   **使わない**（`dwExtraInfo`は`HARNESS_MAGIC`という別の値にする。
//!   0にしないのは、他の正当な注入元と区別してログ上で追跡しやすくする
//!   ため）。
//! - `LLKHF_INJECTED`自体は立つので、IME側が「注入キー全般お断り」という
//!   別の判定を今後追加した場合はこのハーネスも影響を受ける
//!   （現状のフックはそのような判定を持たない。実測済み）。
//!
//! - フォーカスは**自分から奪いに行かない**（`SetForegroundWindow`は
//!   無関係なプロセスからのフォーカス強奪をWindowsが制限しており、
//!   失敗しても成功したように見えることがある。実際に一度、対象アプリ
//!   ではなく全く別の、無関係なターミナルへ大量に打鍵注入する事故が
//!   起きた）。起動後にカウントダウンを出し、ユーザー自身の実際の
//!   クリックを待ってから`GetForegroundWindow`で確認する。さらに
//!   **1文字送るごと**にも確認し、途中でフォーカスが外れたら即座に
//!   プロセス全体を停止する（1文字も余分に無関係な窓へ送らないため）。
//!
//! 使い方:
//!   1. 対象アプリ（メモ帳等の使い捨てで構わないウィンドウ）を開く
//!   2. 他の作業中のターミナル・アプリは閉じておくことを強く推奨
//!      （このハーネスは狙いを外した瞬間に実害が出る種類のツール）
//!   3. cargo run --release -p hook-dll --example keystroke_harness -- "メモ帳"
//!   4. カウントダウン中に対象ウィンドウを手動でクリックする
//!
//! 引数のウィンドウタイトル部分一致でフォーカス先を探す。見つからなければ
//! 何もせず終了する（無関係なウィンドウへ誤って打鍵しないため）。

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

/// このハーネス由来の注入であることを示す値。IME本体の
/// `SELF_INJECTED_MAGIC`（`crates/hook-dll/src/hook.rs`）とは別の値にして
/// 自己送信除外に引っかからないようにする。
const HARNESS_MAGIC: usize = 0x5445_5354; // "TEST"

fn find_window_by_title_substr(needle: &str) -> Option<HWND> {
    struct SearchState {
        needle_lower: String,
        found: Option<HWND>,
    }
    let mut state = SearchState { needle_lower: needle.to_lowercase(), found: None };

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let state = &mut *(lparam.0 as *mut SearchState);
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len == 0 {
                return BOOL(1);
            }
            let mut buf = vec![0u16; len as usize + 1];
            let read = GetWindowTextW(hwnd, &mut buf);
            if read == 0 {
                return BOOL(1);
            }
            let title = String::from_utf16_lossy(&buf[..read as usize]);
            if title.to_lowercase().contains(&state.needle_lower) {
                state.found = Some(hwnd);
                return BOOL(0); // 見つかったので列挙を止める
            }
            BOOL(1)
        }
    }

    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut state as *mut _ as isize));
    }
    state.found
}

fn key_down_up(vk: VIRTUAL_KEY) {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: HARNESS_MAGIC,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: HARNESS_MAGIC,
                },
            },
        },
    ];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// a-z, 0-9, カンマ・ピリオドのみサポート（ローマ字入力として十分）。
fn char_to_vk(ch: char) -> Option<VIRTUAL_KEY> {
    match ch {
        'a'..='z' => Some(VIRTUAL_KEY(ch.to_ascii_uppercase() as u16)),
        '0'..='9' => Some(VIRTUAL_KEY(ch as u16)),
        ',' => Some(VIRTUAL_KEY(0xBC)), // VK_OEM_COMMA
        '.' => Some(VIRTUAL_KEY(0xBE)), // VK_OEM_PERIOD
        _ => None,
    }
}

/// フォーカスが本当に対象ウィンドウにあるかを確認する。
///
/// 起動直後にカウントダウンで確認していても、実行中に通知ポップアップ等
/// でフォーカスが奪われる可能性は残る（実際にこのハーネスの初回実装で
/// `SetForegroundWindow`の自動呼び出しが黙って失敗し、対象アプリでは
/// なく全く別のウィンドウへ大量に打鍵注入する事故が起きた）。
/// **1文字送るごとに**必ずこれで確認し、対象でなければ即座にプロセス
/// 全体を停止する（1文字でも余分に無関係な窓へ送らないため、スキップ
/// ではなく中断にする）。
fn require_foreground_or_abort(target: HWND) {
    let current = unsafe { GetForegroundWindow() };
    if current != target {
        eprintln!(
            "中断: フォーカスが対象ウィンドウから外れました（想定外の窓へ打鍵する事故を防ぐため即停止）。"
        );
        std::process::exit(2);
    }
}

fn type_text(target: HWND, text: &str, delay: Duration) {
    for ch in text.chars() {
        require_foreground_or_abort(target);
        if let Some(vk) = char_to_vk(ch) {
            key_down_up(vk);
            thread::sleep(delay);
        }
    }
}

fn commit(target: HWND) {
    require_foreground_or_abort(target);
    key_down_up(VK_RETURN);
}

/// バケット別のローマ字入力を生成する（`crates/hook-dll/src/hook.rs`の
/// バケット境界 20/50/100/200/400 と対応する文字数付近を狙う）。
fn build_corpus() -> Vec<(&'static str, String)> {
    let base = "kyouhagakkouniittebenkyouwoshitekaishaniokonattekarabangohanwotabeteofuronihaitteneta,";
    vec![
        ("short(1-20)", base.chars().take(10).collect()),
        ("mid(21-50)", base.repeat(1).chars().take(40).collect()),
        ("large(51-100)", base.repeat(2).chars().take(90).collect()),
        ("xlarge(101-200)", base.repeat(3).chars().take(180).collect()),
        ("huge(201-400)", base.repeat(5).chars().take(380).collect()),
        ("mega(400+)", base.repeat(10).chars().take(600).collect()),
    ]
}

fn main() {
    let target = std::env::args().nth(1);
    let iterations: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    let Some(target) = target else {
        eprintln!("使い方: keystroke_harness <ウィンドウタイトル部分一致> [繰り返し回数]");
        eprintln!("例: cargo run --release -p hook-dll --example keystroke_harness -- \"メモ帳\" 5");
        std::process::exit(1);
    };

    let Some(hwnd) = find_window_by_title_substr(&target) else {
        eprintln!("ウィンドウが見つかりません（部分一致: \"{target}\"）。対象アプリを開いてから再実行してください。");
        std::process::exit(1);
    };

    // `SetForegroundWindow`は自ら呼ばない。Windowsは無関係なプロセスからの
    // フォーカス強奪を制限しており、失敗しても成功したように見えることが
    // ある（実際に一度、全く別のウィンドウへ大量に打鍵注入する事故が
    // 起きた）。代わりにカウントダウンを出し、ユーザー自身の実際のクリック
    // （＝Windowsが必ず許可する、本物のフォーカス移動）を待つ。
    println!("5秒以内に対象ウィンドウ（\"{target}\"）を手動でクリックして最前面にしてください。");
    for remaining in (1..=5).rev() {
        print!("\r  開始まで {remaining} 秒...   ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        thread::sleep(Duration::from_secs(1));
    }
    println!();

    let actual_foreground = unsafe { GetForegroundWindow() };
    if actual_foreground != hwnd {
        eprintln!(
            "中断: 対象ウィンドウ（\"{target}\"）が最前面になっていません。1文字も送信していません。クリックしてから再実行してください。"
        );
        std::process::exit(3);
    }

    println!("注入開始: target={target:?} iterations={iterations}");
    let corpus = build_corpus();
    for i in 0..iterations {
        for (label, text) in &corpus {
            println!("  [{i}] bucket={label} len={}", text.chars().count());
            // 打鍵間隔は人間の実タイピング速度の幅を模した20〜60ms。
            type_text(hwnd, text, Duration::from_millis(35));
            commit(hwnd);
            thread::sleep(Duration::from_millis(200));
        }
        // コールドパス計測用に、意図的に2秒超のアイドルを挟む。
        thread::sleep(Duration::from_millis(2500));
    }
    println!("注入完了。数秒待ってから hook_latency.log を確認してください（500打鍵ごとにフラッシュ）。");
}
