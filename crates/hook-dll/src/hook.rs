//! 低レベルキーボードフック本体、モード切替、IME制御、キー送出

use crate::*;

/// ウィンドウ(環境)ごとの入力モード(OUR_ACTIVE)と打鍵中コマンド行を覚える。
/// ターミナルAを日本語、Bはコマンド、のように**環境ごとに独立**して切替できる。
/// 別環境から戻った時にコマンド行も復元し、現在の入力内容で予測を再表示する。
/// (hwnd, our_active, command_line) の小さなリスト。
pub(crate) static WINDOW_MODES: Mutex<Vec<(isize, bool, String)>> = Mutex::new(Vec::new());
/// 直近にフォーカスしていたウィンドウ（モードの保存/復元の切替検出用）
pub(crate) static mut LAST_FG_HWND: isize = 0;
/// 直近に押した横矢印キー(VK_LEFT/VK_RIGHT)と時刻(ms)。素早い2回押しで
/// 行端(Home/End)へジャンプさせるための連打検出に使う。
pub(crate) static mut LAST_ARROW_VK: u32 = 0;
pub(crate) static mut LAST_ARROW_TICK: u32 = 0;
/// 連打とみなす間隔(ms)
pub(crate) const ARROW_DOUBLE_TAP_MS: u32 = 250;

/// VKコード→文字変換
///
/// 返した文字は RomajiConverter に渡り、句読点は
/// `,`→、 `.`→。 `-`→ー `?`→？ `!`→！ `[`→「 `]`→」 に変換される。
pub(crate) fn vk_to_char(vk_code: u32, _scan_code: u32, shift_pressed: bool) -> Option<char> {
    // A-Z はローマ字入力用にそのまま英字を返す
    if (0x41..=0x5A).contains(&vk_code) {
        let base = (vk_code as u8 - 0x41) + b'a';
        return if shift_pressed {
            Some((base - 32) as char) // 大文字（romaji側で小文字化される）
        } else {
            Some(base as char)
        };
    }

    // 日本語入力でよく使う記号のみ、レイアウト非依存の OEM VK で拾う。
    // 返した記号は add_char → RomajiConverter で全角化される
    // （, . - ? ! [ ] / のみ。& @ 等は None を返して半角のまま素通し）。
    match vk_code {
        0xBC if !shift_pressed => Some(','), // VK_OEM_COMMA → 、
        0xBE if !shift_pressed => Some('.'), // VK_OEM_PERIOD → 。
        0xBD if !shift_pressed => Some('-'), // VK_OEM_MINUS → ー
        0xBF if !shift_pressed => Some('/'), // VK_OEM_2 → ・
        0xBF if shift_pressed => Some('?'),  // VK_OEM_2 shift → ？
        0x31 if shift_pressed => Some('!'),  // '1' shift → ！
        0xDB if !shift_pressed => Some('['), // VK_OEM_4 → 「
        0xDD if !shift_pressed => Some(']'), // VK_OEM_6 → 」
        _ => None,
    }
}

/// 入力を確定させる文字（句読点・終端記号）か
///
/// 日本語IMEの一般的な挙動に合わせ、句読点の入力でそれまでの
/// 変換を自動確定する。
pub(crate) fn is_commit_char(ch: char) -> bool {
    matches!(ch, ',' | '.' | '?' | '!')
}

/// フック自身が同じスレッドで観測したShift押下状態（レースなし）。
///
/// `GetAsyncKeyState` は「呼んだ瞬間の実時間の状態」を返すため、低レベル
/// フックのコールバックがOSから遅れて呼ばれた場合、直前に離したはずの
/// Shiftがまだ押下中と誤判定されることがある（特に連続入力時）。これに
/// より句読点キー（VK_OEM_PERIOD/COMMA）が「Shift押下中」と誤判定され、
/// 全角化されず素通しされる＝「。のはずが.になる／稀に両方混ざって出る」
/// 不具合につながり得る。フック自身がShiftのkeydown/keyupを同じコール
/// バック内で順序通りに観測して更新するので、この方式ならレースが起きない。
pub(crate) static mut SHIFT_HELD: bool = false;

/// Shift系のvkCode（左右どちらでも）か
fn is_shift_vk(vk_code: u32) -> bool {
    matches!(vk_code, 0x10 /* VK_SHIFT */ | 0xA0 /* VK_LSHIFT */ | 0xA1 /* VK_RSHIFT */)
}

/// フックが観測したキーイベントから `SHIFT_HELD` を更新する。
/// `LowLevelKeyboardProc` の先頭、他の判定より前に呼ぶこと。
pub(crate) unsafe fn track_shift_state(vk_code: u32, event: u32) {
    if !is_shift_vk(vk_code) {
        return;
    }
    if event == WM_KEYDOWN || event == WM_SYSKEYDOWN {
        SHIFT_HELD = true;
    } else if event == WM_KEYUP || event == WM_SYSKEYUP {
        SHIFT_HELD = false;
    }
}

/// Shiftキーが押されているか確認
pub(crate) fn is_shift_pressed() -> bool {
    unsafe { SHIFT_HELD }
}

/// vkCodeごとの「現在押されっぱなしか」を追う配列（OSキーリピート検出用）。
///
/// WH_KEYBOARD_LL の KBDLLHOOKSTRUCT には、古典的な WM_KEYDOWN の lParam
/// ビット30（「直前も押されていたか」）に相当する情報が無いため、自前で
/// vkCodeごとの押下状態を追跡しないとOSのキーリピート（押しっぱなし）を
/// 検出できない。`static mut`配列への索引アクセスは共有参照の警告が出る
/// ため、他の共有可変状態（COMMAND_LINE 等）と同様に Mutex で持つ。
pub(crate) static KEY_REPEAT_HELD: Mutex<[bool; 256]> = Mutex::new([false; 256]);

/// このイベントがOSのキーリピート（同じキーを押しっぱなしにした再送）かを
/// 判定しつつ、held状態を更新する。呼ぶ側は自己注入キー（SendInputで
/// 送った合成キー）を渡さないこと（自分が送ったBackspace/Enter等の連続
/// 送出を「実キーのリピート」と誤認識し、次の本物のキー入力を誤って
/// リピート扱いしてしまうため）。
pub(crate) fn track_key_repeat(vk_code: u32, event: u32) -> bool {
    let idx = vk_code as usize;
    let Ok(mut held) = KEY_REPEAT_HELD.lock() else { return false };
    if idx >= held.len() {
        return false;
    }
    if event == WM_KEYDOWN || event == WM_SYSKEYDOWN {
        let was_held = held[idx];
        held[idx] = true;
        was_held
    } else {
        // KEYUP等: 押されていない状態に戻す（リピートではない）
        held[idx] = false;
        false
    }
}

/// Ctrlキーが押されているか確認
pub(crate) fn is_ctrl_pressed() -> bool {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        GetAsyncKeyState(VK_CONTROL.0 as i32) < 0
    }
}

/// vk_code が修飾キー自体（Ctrl/Alt/Shift の左右どちらか）か。
///
/// 低レベルキーボードフックの vkCode は、修飾キー自体が押された/離された
/// イベントでは総称の VK_CONTROL(0x11)/VK_MENU(0x12) ではなく、左右を
/// 区別した VK_LCONTROL(0xA2)/VK_RCONTROL(0xA3)/VK_LMENU(0xA4)/VK_RMENU(0xA5)
/// 等で届く。Ctrl+数字のような組み合わせキー判定で「Ctrlキー自体の押下」を
/// combo として誤検出しないよう、除外判定に使う。
pub(crate) fn is_modifier_vk(vk_code: u32) -> bool {
    matches!(vk_code, 0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5)
}

/// Altキーが押されているか確認
pub(crate) fn is_alt_pressed() -> bool {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        GetAsyncKeyState(VK_MENU.0 as i32) < 0
    }
}

// WM_IME_CONTROL wparam 定数 (windows crate に未定義のため手動定義)
// https://learn.microsoft.com/en-us/windows/win32/intl/wm-ime-control
pub(crate) const IMC_GETCONVERSIONMODE: usize = 0x0001;
pub(crate) const IMC_SETCONVERSIONMODE: usize = 0x0002;
pub(crate) const IMC_GETOPENSTATUS: usize = 0x0005;
pub(crate) const IMC_SETOPENSTATUS: usize = 0x0006;

/// 半角/全角キー (IME mode toggle) の vkCode 群
///
/// 環境によって発火する vk が異なるため複数を OR で見る:
/// - 0x19  : VK_KANJI / VK_HANJA  漢字キー
/// - 0xF3  : VK_DBE_DBCSCHAR / VK_OEM_AUTO  全角化キー
/// - 0xF4  : VK_DBE_SBCSCHAR / VK_OEM_ENLW  半角化キー
pub(crate) fn is_ime_toggle_vk(vk: u32) -> bool {
    matches!(vk, 0x19 | 0xF3 | 0xF4)
}

/// フォアグラウンド窓の「実際にキーボードフォーカスを持つ窓」を解決する。
///
/// `ImmGetDefaultIMEWnd`はIMEコンテキストを引数のhwndから解決するが、
/// WebView2/Chromium系のように「トップレベル窓とは別の子窓が実際に入力を
/// 受け取る」構成のアプリでは、`GetForegroundWindow()`が返すトップレベル窓
/// と、実際にIME上フォーカスされている窓が一致しないことがある。その場合
/// IMEの開閉指示がトップレベル窓へ送られても実際にテキストを受け取る窓
/// には届かず、MS-IMEが開いたまま残って注入した文字が二重に変換される
/// （実機報告: WebView2アプリで二重変換・文字化けが発生）。
/// `GetGUIThreadInfo`でフォアグラウンドスレッドの実際のフォーカス窓
/// (`hwndFocus`)を取得できればそちらを対象にし、取れなければ従来通り
/// トップレベル窓自体にフォールバックする。
pub(crate) fn resolve_ime_target_hwnd(hwnd_fg: HWND) -> HWND {
    unsafe {
        let tid = GetWindowThreadProcessId(hwnd_fg, None);
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(tid, &mut gti).is_ok() && !gti.hwndFocus.0.is_null() {
            gti.hwndFocus
        } else {
            hwnd_fg
        }
    }
}

/// フォアグラウンドウィンドウの MS-IME を強制的に閉じる
///
/// 我々が SendInput KEYEVENTF_UNICODE で送る文字を MS-IME が
/// composition として取り込むのを防ぐ。IMC_SETOPENSTATUS=0 で
/// IME 全体を閉じる(=半角英数モード相当)。
pub(crate) fn close_ms_ime_for_foreground() {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG};
        let hwnd_fg = GetForegroundWindow();
        if hwnd_fg.0.is_null() {
            return;
        }
        let hwnd = resolve_ime_target_hwnd(hwnd_fg);
        let ime_wnd = ImmGetDefaultIMEWnd(hwnd);
        if ime_wnd.0.is_null() {
            return;
        }
        // ブロックしないようタイムアウト付きで送る（相手が固まっても最大30ms）
        let _ = SendMessageTimeoutW(
            ime_wnd,
            WM_IME_CONTROL,
            WPARAM(IMC_SETOPENSTATUS),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            30,
            None,
        );
    }
}

/// フォアグラウンドウィンドウの MS-IME を開き、ひらがな入力モードにする。
///
/// 単語登録の読み欄のように「必ずひらがなで入力してほしい」フィールドに
/// フォーカスが移ったときに呼ぶ。IMC_SETOPENSTATUS=1 でIMEを開き、
/// IMC_SETCONVERSIONMODE で NATIVE（日本語入力）ビットのみ立てる
/// （KATAKANA/ROMAN 等は落として確実にひらがなにする）。
pub(crate) fn open_ms_ime_hiragana_for_foreground() {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG};
        let hwnd_fg = GetForegroundWindow();
        if hwnd_fg.0.is_null() {
            return;
        }
        let hwnd = resolve_ime_target_hwnd(hwnd_fg);
        let ime_wnd = ImmGetDefaultIMEWnd(hwnd);
        if ime_wnd.0.is_null() {
            return;
        }
        let _ = SendMessageTimeoutW(
            ime_wnd,
            WM_IME_CONTROL,
            WPARAM(IMC_SETOPENSTATUS),
            LPARAM(1),
            SMTO_ABORTIFHUNG,
            30,
            None,
        );
        let _ = SendMessageTimeoutW(
            ime_wnd,
            WM_IME_CONTROL,
            WPARAM(IMC_SETCONVERSIONMODE),
            LPARAM(IME_CMODE_NATIVE.0 as isize),
            SMTO_ABORTIFHUNG,
            30,
            None,
        );
    }
}

/// フォアグラウンド（環境）が変わったら、前の環境のモードを保存し、新しい環境の
/// モードを復元する。これにより「ターミナルAは日本語、Bはコマンド」のように
/// **環境ごとに独立して**モードを切り替えられる（切替が全体に波及しない）。
/// 未知の環境は MS-IME の状態を初期値にする。
pub(crate) unsafe fn sync_window_mode() {
    let fg = GetForegroundWindow().0 as isize;
    if fg == 0 || fg == LAST_FG_HWND {
        return; // 変化なし
    }
    let cur_cmd = COMMAND_LINE.lock().map(|b| b.clone()).unwrap_or_default();
    // 直前の環境のモード＋コマンド行を保存
    if LAST_FG_HWND != 0 {
        if let Ok(mut m) = WINDOW_MODES.lock() {
            if let Some(e) = m.iter_mut().find(|(h, _, _)| *h == LAST_FG_HWND) {
                e.1 = OUR_ACTIVE;
                e.2 = cur_cmd;
            } else {
                m.push((LAST_FG_HWND, OUR_ACTIVE, cur_cmd));
                if m.len() > 64 {
                    m.remove(0);
                }
            }
        }
    }
    // 新しい環境のモード＋コマンド行を復元（未知なら MS-IME 状態＋空）
    let known = WINDOW_MODES
        .lock()
        .ok()
        .and_then(|m| m.iter().find(|(h, _, _)| *h == fg).map(|(_, a, c)| (*a, c.clone())));
    let (mode, cmd) = match known {
        Some((a, c)) => (a, c),
        None => (is_ime_hiragana_mode(), String::new()),
    };
    OUR_ACTIVE = mode;
    LAST_FG_HWND = fg;
    // 日本語の下書きは環境をまたがないのでクリア
    if let Some(cm) = LIVE_CONTEXT.get() {
        if let Ok(mut c) = cm.try_lock() {
            c.romaji_buffer.clear();
            c.hiragana_buffer.clear();
            c.pinned.clear();
            c.conversion_result.clear();
            c.last_sent_length = 0;
        }
    }
    // コマンド行は環境ごとに復元
    if let Ok(mut b) = COMMAND_LINE.lock() {
        *b = cmd;
    }
    hide_candidate_window();
    // 戻ってきた環境がコマンドモードで、以前の入力が残っていれば予測を再表示する
    if !OUR_ACTIVE && IS_ENABLED && is_terminal_focused() {
        update_command_suggestions();
    }
}

/// 我々のアクティブ状態をトグルする
///
/// アクティブ化時は MS-IME を閉じてコンポジションを無効化。
/// 非アクティブ化時はそのまま閉じた状態を維持 (ユーザーは英数モードに戻りたいはず)。
pub(crate) fn toggle_our_active() {
    unsafe {
        OUR_ACTIVE = !OUR_ACTIVE;
        let active_now = OUR_ACTIVE; // static mut への参照を避けるため値コピー
        debug_log!("OUR_ACTIVE トグル: {}", active_now);
        hide_candidate_window();
        // コマンド行の追跡もリセット（日本語⇄コマンドの切替時に持ち越さない）
        if let Ok(mut b) = COMMAND_LINE.lock() {
            b.clear();
        }
        if OUR_ACTIVE {
            close_ms_ime_for_foreground();
            // 入力中だった composition バッファをクリア
            if let Some(context_mutex) = LIVE_CONTEXT.get() {
                if let Ok(mut c) = context_mutex.try_lock() {
                    c.romaji_buffer.clear();
                    c.hiragana_buffer.clear();
                    c.pinned.clear();
                    c.conversion_result.clear();
                    c.last_sent_length = 0;
                }
            }
        }
        // モード切替を明示的にフラッシュ表示（自動で消える）。
        // ターミナルでは「日本語入力 ⇄ コマンドモード」の切替であることを示す。
        let terminal = is_terminal_focused();
        let label = if OUR_ACTIVE {
            "\u{3042} 日本語入力"
        } else if terminal {
            "\u{2318} コマンドモード"
        } else {
            "A 半角英数"
        };
        flash_mode_indicator(label);
    }
}

/// フォアグラウンドアプリのIMEがひらがな入力モードか判定する
///
/// Low-Level Keyboard Hook は別スレッドで動くため `ImmGetContext` が常にNULLになる。
/// 代わりに `ImmGetDefaultIMEWnd(hwnd)` で IME ウィンドウを取得し、
/// `SendMessageW(WM_IME_CONTROL, IMC_GETOPENSTATUS / IMC_GETCONVERSIONMODE)` で
/// IME 本体スレッドに問い合わせる（スレッドセーフな経路）。
///
/// 判定:
/// - IME ウィンドウが取れない → 「IMEなしのアプリ(コンソール等)」と判断、パススルー(false)
/// - IMC_GETOPENSTATUS が FALSE → IME OFF (半角英数モード相当) → パススルー
/// - IMC_GETCONVERSIONMODE で IME_CMODE_NATIVE が立っていない → 英数モード → パススルー
/// - IME_CMODE_KATAKANA が立っている → カタカナモード → パススルー
/// - それ以外 (NATIVE && !KATAKANA) → ひらがなモード → 変換ON
pub(crate) fn is_ime_hiragana_mode() -> bool {
    unsafe {
        let hwnd_fg = GetForegroundWindow();
        if hwnd_fg.0.is_null() {
            debug_log!("IMEモード: フォアグラウンドウィンドウなし - パススルー");
            return false;
        }
        let hwnd = resolve_ime_target_hwnd(hwnd_fg);

        let ime_wnd = ImmGetDefaultIMEWnd(hwnd);
        if ime_wnd.0.is_null() {
            // IMEを持たないアプリ。半角英数のキーをそのまま打ちたいケースなのでパススルー。
            debug_log!("IMEモード: IMEウィンドウなし(IME非対応アプリ) - パススルー");
            return false;
        }

        // STEP 1: IME が開いているか
        let open_status = SendMessageW(
            ime_wnd,
            WM_IME_CONTROL,
            WPARAM(IMC_GETOPENSTATUS),
            LPARAM(0),
        );
        if open_status.0 == 0 {
            debug_log!("IMEモード: IME OFF (半角英数モード) - パススルー");
            return false;
        }

        // STEP 2: 変換モードを取得
        let conv_mode = SendMessageW(
            ime_wnd,
            WM_IME_CONTROL,
            WPARAM(IMC_GETCONVERSIONMODE),
            LPARAM(0),
        );
        let mode_bits = conv_mode.0 as u32;
        let is_native = (mode_bits & IME_CMODE_NATIVE.0) != 0;
        let is_katakana = (mode_bits & IME_CMODE_KATAKANA.0) != 0;

        // ひらがなモード = NATIVE 立ち & KATAKANA 立たず
        let is_hiragana = is_native && !is_katakana;

        debug_log!(
            "IMEモード: conv=0x{:X}, native={}, katakana={}, hiragana_mode={}",
            mode_bits, is_native, is_katakana, is_hiragana
        );
        is_hiragana
    }
}

/// 変換アクションを 1 回の SendInput でアトミックに実行
///
/// 削除(BS)と挿入(KEYEVENTF_UNICODE)を別々の SendInput 呼び出しにすると
/// 呼び出し間で IME がコンポジション状態を変えてしまい、後の挿入が前を
/// 上書きするように見える。Vec<INPUT> をまとめて 1 回で渡せば BS と
/// 挿入の間に他処理が割り込めない。
/// 仮想キー（Home/End/矢印など）を1回押して離す。カーソル移動の送出に使う。
pub(crate) fn send_vk(vk: VIRTUAL_KEY) {
    unsafe {
        let inputs = [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk,
                        wScan: 0,
                        dwFlags: KEYBD_EVENT_FLAGS(0),
                        time: 0,
                        dwExtraInfo: 0,
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
                        dwExtraInfo: 0,
                    },
                },
            },
        ];
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 修飾キー（Ctrl等）だけを押す/離す1イベントだけ送る。
///
/// Ctrl+数字のような修飾キー付きのトリガーで確定処理を行う場合、その時点
/// ではユーザーが物理的にまだCtrlを押したままになっている。ここで送る
/// 合成 Backspace（`execute_action`）はキー自体には修飾キー情報を持たない
/// 単なる VK_BACK だが、OSは「現在押されている修飾キー」を見て配信先へ
/// 通知するため、物理Ctrlが押されたままだと合成 Backspace が
/// Ctrl+Backspace（多くのシェルで「単語ごと削除」）に化けてしまう。
/// 確定処理の前後でCtrlを合成的に離す/押し直すことで、この化けを防ぎつつ、
/// 処理後もユーザーの物理的な押下状態と辻褄を合わせる。
pub(crate) fn send_vk_edge(vk: VIRTUAL_KEY, down: bool) {
    unsafe {
        let inputs = [INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }];
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

pub(crate) fn execute_action(action: ConversionAction) {
    if action.delete_count == 0 && action.insert_text.is_empty() {
        return;
    }

    // 文字を送り込む直前に MS-IME を閉じ、注入文字が MS-IME に
    // コンポジションとして拾われる（二重変換・競合）のを防ぐ。
    close_ms_ime_for_foreground();

    let mut inputs: Vec<INPUT> = Vec::with_capacity(
        action.delete_count * 2 + action.insert_text.chars().count() * 2,
    );

    // 1. BS x delete_count
    for _ in 0..action.delete_count {
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_BACK,
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_BACK,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }

    // 2. UNICODE で挿入文字を送信
    for ch in action.insert_text.chars() {
        // BMP 外文字はサロゲートペアになるが、KEYEVENTF_UNICODE の wScan は
        // UTF-16 code unit を渡すので 1 文字 = 1〜2 INPUT イベント。
        let mut buf = [0u16; 2];
        let units = ch.encode_utf16(&mut buf);
        for &unit in units.iter() {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }

    unsafe {
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != inputs.len() {
            debug_log!("SendInput 一部失敗: sent={} / total={}", sent, inputs.len());
        }
    }
}

/// キーボードフックのコールバック関数
#[no_mangle]
/// 低レベルマウスフックで「ボタン押下」として扱うメッセージか
/// （左/右/中/サイドボタンの押下。移動やホイールでは何もしない）
pub(crate) fn is_mouse_button_down_message(msg: u32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
    };
    msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN || msg == WM_XBUTTONDOWN
}

/// ポップアップの外側がクリックされたときの下書きの後始末。
///
/// 日本語入力の下書き（候補一覧で選択中を含む）があれば確定扱いにして、
/// 内部状態を画面から切り離す。クリックでキャレットが別の場所へ移った後に
/// Backspace や次の打鍵が「古い下書きに対する差分編集」として送られ、
/// 無関係な文字を消したり上書きしたりするのを防ぐ（一般的なIMEでも
/// クリックは変換中の文字列の確定として扱われる）。確定した場合 true。
pub(crate) fn settle_composition_on_click(state: &mut LiveConversionState) -> bool {
    if !(state.is_composing() || !state.candidates.is_empty()) {
        return false;
    }
    // 末尾の未確定 'n' があると commit は「ん」への置換アクションを返すが、
    // キャレットはもうクリック先に移っているので画面には送らない。
    let _ = state.commit();
    true
}

/// ポップアップの外側がクリックされた: 下書きを確定扱いにしてポップアップを閉じる
pub(crate) fn on_click_outside_popup() {
    if let Some(cm) = LIVE_CONTEXT.get() {
        if let Ok(mut c) = cm.try_lock() {
            let settled = settle_composition_on_click(&mut c);
            debug_log!("ポップアップ外クリック: 下書き確定={}", settled);
        }
    }
    hide_candidate_window();
}

/// 低レベルマウスフック（WH_MOUSE_LL）。
///
/// 予測変換・候補一覧・コマンド候補のポップアップ表示中に、ポップアップの
/// 外側でマウスボタンが押されたらポップアップを閉じる。ポップアップ自身の
/// クリック（コマンドモードの「設定」ボタン等）は矩形内なので対象外。
/// マウス移動・ホイールはボタン押下でないため即座に次のフックへ渡す。
#[allow(non_snake_case)]
pub extern "system" fn LowLevelMouseProc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && is_mouse_button_down_message(wparam.0 as u32) && candidate_window_visible() {
            use windows::Win32::UI::WindowsAndMessaging::MSLLHOOKSTRUCT;
            let info = lparam.0 as *const MSLLHOOKSTRUCT;
            if !info.is_null() {
                let pt = (*info).pt;
                if !popup_contains_screen_point(pt.x, pt.y) {
                    on_click_outside_popup();
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[allow(non_snake_case)] // Win32 のコールバック名（LowLevelMouseProc と同じ流儀）
pub extern "system" fn LowLevelKeyboardProc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let kb = *(lparam.0 as *const KBDLLHOOKSTRUCT);
        let event = wparam.0 as u32;

        // 他の判定より前に、Shift押下状態をレースなく更新する
        // （早期returnの影響を受けないよう最優先で行う）。
        track_shift_state(kb.vkCode, event);

        // デバッグ: キー入力をログ
        if event == WM_KEYDOWN || event == WM_SYSKEYDOWN {
            debug_log!("キー入力検出: vkCode={}, flags={}", kb.vkCode, kb.flags.0);
        }
        
        // 自分が送信したキーは無視
        if (kb.flags.0 & LLKHF_INJECTED.0) != 0 {
            debug_log!("自己送信キーをスキップ");
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // 実キーのリピート状態を更新（自己注入キーを除外した後で行う。
        // 自分が送るBackspace/Enter等の連続送出を「押しっぱなし」と
        // 誤認識すると、次の本物のキー入力まで誤ってリピート扱いされる）。
        let is_key_repeat = track_key_repeat(kb.vkCode, event);

        // 有効でない場合はパススルー
        if !IS_ENABLED {
            debug_log!("変換無効: パススルー");
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // 自分のUI（設定ウィンドウ・ファイルダイアログ）にフォーカスがある間は、
        // その入力を横取りしない（設定画面の編集欄に普通に打てるように）。
        if foreground_is_ours() {
            return CallNextHookEx(None, code, wparam, lparam);
        }


        if event == WM_KEYDOWN || event == WM_SYSKEYDOWN {
            let vk_code = kb.vkCode;

            // 環境(ウィンドウ)ごとのモードを同期（切替が全体に波及しないように）。
            // フォーカスが変わっていたら前の環境のモードを保存し、今の環境のを復元する。
            sync_window_mode();

            // Ctrl+Alt+A: モードに応じて設定ウィンドウを出し分ける。
            //  - 日本語変換モード（OUR_ACTIVE=true） → 単語登録ウィンドウ
            //  - それ以外（コマンドモード/英数）   → コマンド/エイリアス設定ウィンドウ
            // 両者は交わらない別モーダル。
            if is_ctrl_pressed() && is_alt_pressed() && vk_code == 0x41 {
                if OUR_ACTIVE {
                    open_word_settings_window();
                } else {
                    open_settings_window();
                }
                return LRESULT(1);
            }

            // 半角/全角キー: OUR_ACTIVE をトグル (MS-IME には届かせない)
            if is_ime_toggle_vk(vk_code) {
                toggle_our_active();
                return LRESULT(1); // 元のキーは MS-IME に渡さず消費
            }

            // Ctrl+Space: ライブ変換全体のトグル (緊急 OFF 用)
            if is_ctrl_pressed() && vk_code == VK_SPACE.0 as u32 {
                IS_ENABLED = !IS_ENABLED;
                debug_log!("ライブ変換トグル: {}", if IS_ENABLED { "有効" } else { "無効" });
                if !IS_ENABLED {
                    OUR_ACTIVE = false;
                    hide_candidate_window();
                    if let Some(context_mutex) = LIVE_CONTEXT.get() {
                        if let Ok(mut context) = context_mutex.try_lock() {
                            context.romaji_buffer.clear();
                            context.hiragana_buffer.clear();
                            context.pinned.clear();
                            context.conversion_result.clear();
                            context.last_sent_length = 0;
                        }
                    }
                }
                return LRESULT(1);
            }

            // （初期モードは sync_window_mode が環境ごとに決めるのでここでは何もしない）

            // 我々が非アクティブ（日本語入力OFF）の場合:
            //   - ターミナルにフォーカスがある → コマンドモード（打鍵を観測して
            //     よく使うコマンドを前方一致で提案。Enterで履歴学習）。
            //   - それ以外 → 従来どおりパススルー。
            if !OUR_ACTIVE {
                if IS_ENABLED && is_terminal_focused() {
                    // Ctrl+数字(1〜9): コマンド候補一覧を番号で直接選んで実行する。
                    // 素の数字キーは git log -1 やポート番号等、実際のコマンド
                    // 文字列で頻繁に使うため選択キーにできない（Ctrl併用なら
                    // 衝突しない）。対象候補が無ければ None が返り、下の通常の
                    // Ctrl/Alt処理（追跡中断・パススルー）にそのまま委ねる。
                    if is_ctrl_pressed() && !is_alt_pressed() && (0x31..=0x39).contains(&vk_code) {
                        if let Some(r) = command_select_by_number((vk_code - 0x31) as usize) {
                            return r;
                        }
                    }
                    // Ctrl/Alt 併用（Ctrl+C 等の行編集・ショートカット）は追跡を中断。
                    // ただし vk_code が修飾キー自体（Ctrlを押した/離した瞬間の
                    // イベント）のときは対象外にする。除外しないと、Ctrl+数字を
                    // 押そうとして先に Ctrl だけが単独のイベントとして届いた
                    // 時点でバッファが消え、ポップアップが即座に閉じてしまう
                    // （その後の数字キーはターミナルへ生で漏れ、ベル音や
                    // ターミナル側のCtrl+数字ショートカットが誤発火する）。
                    if (is_ctrl_pressed() || is_alt_pressed()) && !is_modifier_vk(vk_code) {
                        if let Ok(mut b) = COMMAND_LINE.lock() {
                            b.clear();
                        }
                        hide_candidate_window();
                        return CallNextHookEx(None, code, wparam, lparam);
                    }
                    if let Some(r) = handle_command_mode(vk_code, is_shift_pressed()) {
                        return r; // 消費（Tab補完など）
                    }
                    // 観測のみ。元のキーはターミナルへ渡してエコーさせる。
                    return CallNextHookEx(None, code, wparam, lparam);
                }
                // 非ターミナル: コマンド行が残っていれば掃除してパススルー
                if let Ok(mut b) = COMMAND_LINE.lock() {
                    if !b.is_empty() {
                        b.clear();
                        hide_candidate_window();
                    }
                }
                return CallNextHookEx(None, code, wparam, lparam);
            }

            // 念のため: MS-IME が外部要因で再オープンされていたら毎回閉じ直す
            // (ユーザーがタスクトレイ等から触った場合の保険)
            // ※ パフォーマンス劣化を避けるため、特定キーだけにしてもよいが
            //   今はシンプルに毎回呼ぶ。
            // close_ms_ime_for_foreground();  // 必要なら有効化

            // 修飾キー組み合わせはパススルー
            if is_ctrl_pressed() || is_alt_pressed() {
                return CallNextHookEx(None, code, wparam, lparam);
            }

            if let Some(context_mutex) = LIVE_CONTEXT.get() {
                // try_lockでブロッキングを回避
                if let Ok(mut context) = context_mutex.try_lock() {
                    // 数字キー 1-9: 候補一覧の表示中は番号で直接選択して確定
                    // （選んだ = その変換が正しい、として学習にも記録される）
                    if (0x31..=0x39).contains(&vk_code)
                        && !is_shift_pressed()
                        && candidate_window_visible()
                        && !context.candidates.is_empty()
                    {
                        // 番号キーは「現在ページ内の位置」を選ぶ（ページ先頭 + 番号）。
                        let page_start = candidate_page_start(context.candidate_index);
                        let index = page_start + (vk_code - 0x31) as usize;
                        if index < context.candidates.len() {
                            let mut actions = Vec::new();
                            if let Some(action) = context.select_candidate(index) {
                                actions.push(action);
                            }
                            if let Some(action) = context.commit() {
                                actions.push(action);
                            }
                            drop(context);
                            hide_candidate_window();
                            for action in actions {
                                execute_action(action);
                            }
                            return LRESULT(1);
                        }
                    }

                    // Delete: 候補一覧の表示中に、選択中候補の「誤学習」をリセットする。
                    // 過去に誤って確定して上位に居座っている変換を、その1件だけ取り消す。
                    // リセット後は候補を作り直して一覧を出し直す（1件以下なら閉じる）。
                    {
                        use windows::Win32::UI::Input::KeyboardAndMouse::VK_DELETE;
                        if vk_code == VK_DELETE.0 as u32
                            && candidate_window_visible()
                            && !context.candidates.is_empty()
                        {
                            let action = context.reset_learning_for_selected();
                            // 更新後の候補一覧（最後の文節の表記）と選択位置を取り出す。
                            let items = context.cand_seg_surfaces.clone();
                            let selected = context.candidate_index;
                            drop(context);
                            if let Some(action) = action {
                                execute_action(action);
                            }
                            // 候補が2件以上あれば一覧を出し直す。1件以下なら閉じる。
                            if items.len() >= 2 {
                                show_candidate_window(&items, selected);
                            } else {
                                hide_candidate_window();
                            }
                            return LRESULT(1);
                        }
                        // 予測一覧（履歴補完）の表示中も Delete でその予測の学習を消す。
                        // 「誤字を修正するう」のような誤確定がそのまま補完に出るのを止める。
                        if vk_code == VK_DELETE.0 as u32
                            && candidate_window_visible()
                            && context.candidates.is_empty()
                            && !context.predictions.is_empty()
                        {
                            let did = context.reset_prediction_learning();
                            let preds = context.prediction_display();
                            let sel = context.prediction_index.min(preds.len().saturating_sub(1));
                            drop(context);
                            if did {
                                if preds.is_empty() {
                                    hide_candidate_window();
                                } else {
                                    show_prediction_popup(&preds, sel);
                                }
                            }
                            return LRESULT(1);
                        }
                    }

                    // 数字キー 1-9: 予測変換の表示中（同音候補一覧は出ていない）は
                    // 番号で予測語を確定する（前方一致補完・次単語予測）。
                    if (0x31..=0x39).contains(&vk_code)
                        && !is_shift_pressed()
                        && candidate_window_visible()
                        && context.candidates.is_empty()
                        && !context.predictions.is_empty()
                    {
                        // 予測一覧も同じページング。番号キーはページ内位置で選ぶ
                        // （現状は最大6件で1ページに収まるが、増えても整合する）。
                        let page_start = candidate_page_start(context.prediction_index);
                        let index = page_start + (vk_code - 0x31) as usize;
                        if index < context.predictions.len() {
                            let action = context.commit_prediction(index);
                            // 確定後の次単語予測を用意（commit_prediction 内で更新済み）
                            let preds = context.prediction_display();
                            drop(context);
                            hide_candidate_window();
                            if let Some(action) = action {
                                execute_action(action);
                            }
                            show_prediction_popup(&preds, 0);
                            return LRESULT(1);
                        }
                    }

                    // Shift+英字 = その1文字だけ英大文字を直接入力（かな変換しない）。
                    //   頭字語(API)や「PCで」のような直後のかな入力が予測どおり動く。
                    //   小文字始まりの英単語を続けたい場合は 半角/全角 で英数モードへ。
                    if (0x41..=0x5A).contains(&vk_code) && is_shift_pressed() {
                        let action = if context.is_composing() {
                            context.commit()
                        } else {
                            None
                        };
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        hide_candidate_window();
                        // 元の Shift+英字をアプリに渡して大文字を入力させる
                        return CallNextHookEx(None, code, wparam, lparam);
                    }

                    // アルファベット・句読点キー
                    if let Some(ch) = vk_to_char(vk_code, kb.scanCode, is_shift_pressed()) {
                        // OSのキーリピート（押しっぱなし）による再送は無視する。
                        // 特に句読点（,.?!）は入力後に自動確定するため、リピートで
                        // 複数回処理されると「。」「、」等が連続して重複挿入され、
                        // かつ確定処理（学習の記録等）も余分に走ってしまう
                        // （実機報告: "." で「。。」が連続して出る）。
                        if is_key_repeat {
                            return LRESULT(1);
                        }
                        let mut actions = Vec::new();

                        // 候補一覧から選択中に次の入力が来たら、選択中の候補を
                        // 確定してから新しい入力を始める（選択 = 確定の解釈）
                        if candidate_window_visible() && !context.candidates.is_empty() {
                            if let Some(action) = context.commit() {
                                actions.push(action);
                            }
                        }

                        if let Some(action) = context.add_char(ch) {
                            actions.push(action);
                        }
                        // 句読点は入力後に自動確定（日本語IMEの標準動作）
                        if is_commit_char(ch) {
                            if let Some(action) = context.commit() {
                                actions.push(action);
                            }
                        }
                        // 予測変換を更新して表示（番号キーで選べる）
                        context.update_predictions();
                        let preds = context.prediction_display();
                        // ロックを解放してからアクションを実行
                        drop(context);
                        for action in actions {
                            execute_action(action);
                        }
                        show_prediction_popup(&preds, 0);
                        // 元のキー入力を抑制
                        return LRESULT(1);
                    }

                    // かなにしないテキストキー（数字・@ & ; 等）が変換中に来たら、
                    // まず下書きを確定してから、そのキーを半角のままアプリへ渡す。
                    //   確定せずに素通しすると、下書きの内部状態（送信済み文字数）と
                    //   実際の表示がズレて、次の変換時に BS 回数が狂い「たまに文字が
                    //   半角で二重に出る」原因になる。ここで確定して同期を保つ。
                    if context.is_composing()
                        && vk_to_char(vk_code, kb.scanCode, is_shift_pressed()).is_none()
                        && vk_to_ascii(vk_code, is_shift_pressed()).is_some()
                    {
                        let action = context.commit();
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        hide_candidate_window();
                        // 元のキー（半角の数字・記号）をアプリへ渡す
                        return CallNextHookEx(None, code, wparam, lparam);
                    }

                    // バックスペース
                    if vk_code == VK_BACK.0 as u32 && context.is_composing() {
                        let action = context.backspace();
                        context.update_predictions();
                        let preds = context.prediction_display();
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        show_prediction_popup(&preds, 0);
                        return LRESULT(1);
                    }

                    // Space（Shift有無を問わず）: 現在の変換を確定して空白を通す。
                    //   （LLM校正は実用性が低いため廃止。誤字補正は「もしかして」
                    //    ＝即時のfuzzy補正で行う）
                    if vk_code == VK_SPACE.0 as u32 && context.is_composing() {
                        let action = context.commit();
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        hide_candidate_window();
                        // 元の Space をアプリに渡して空白を入力させる
                        return CallNextHookEx(None, code, wparam, lparam);
                    }

                    // 通常変換（候補一覧）:
                    //   Tab / ↓ : 次候補（Shift+Tab は前へ）
                    //   ↑       : 前候補
                    // 変換中に押すと候補一覧を表示し、選択を移動する。
                    // Tab は常に「通常変換（同音候補の切替）」に使う。
                    // もしかして/予測は Tab では確定しない（誤ったもしかしてを
                    // Tab で誤爆させないため）。予測の確定は Enter か番号キーで行う。
                    // Tab を押すと下の cycle_candidate が走り、予測ポップアップは
                    // 同音候補一覧に置き換わる（＝もしかしてを無視して普通に変換）。
                    let is_next_key = vk_code == VK_TAB.0 as u32
                        || vk_code == VK_DOWN.0 as u32;
                    let is_prev_key = vk_code == VK_UP.0 as u32;

                    // 予測変換リスト表示中の ↑/↓ は、その中の選択カーソルを移動する。
                    // （通常変換の候補一覧＝別モーダルを立ち上げない）。Tab は従来どおり
                    // 下の cycle_candidate に落として通常変換に使う。
                    if (vk_code == VK_DOWN.0 as u32 || vk_code == VK_UP.0 as u32)
                        && context.is_composing()
                        && candidate_window_visible()
                        && context.candidates.is_empty()
                        && !context.predictions.is_empty()
                    {
                        let last = context.predictions.len() - 1;
                        if vk_code == VK_DOWN.0 as u32 {
                            context.prediction_index =
                                if context.prediction_index >= last { 0 } else { context.prediction_index + 1 };
                        } else {
                            context.prediction_index =
                                if context.prediction_index == 0 { last } else { context.prediction_index - 1 };
                        }
                        let preds = context.prediction_display();
                        let sel = context.prediction_index;
                        drop(context);
                        show_prediction_popup(&preds, sel);
                        return LRESULT(1); // 予測リスト内の移動として消費
                    }

                    if (is_next_key || is_prev_key) && context.is_composing() {
                        let backwards = is_prev_key || is_shift_pressed();
                        let action = context.cycle_candidate(backwards);
                        // 一覧には直近（最後）の文節の候補だけを表示する
                        let items = context.cand_seg_surfaces.clone();
                        let selected = context.candidate_index;
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        show_candidate_window(&items, selected);
                        // 変換中の移動キーは候補切替として消費
                        return LRESULT(1);
                    }

                    // ←→: カーソルを1文字移動する（横矢印＝カーソル移動）。
                    //   変換中なら、まず今の変換を確定してからカーソルを動かす
                    //   （確定せず素通しすると下書きとズレるため）。
                    //   同じ向きを素早く2回押したら行端へジャンプ（←=Home / →=End）。
                    if vk_code == VK_LEFT.0 as u32 || vk_code == VK_RIGHT.0 as u32 {
                        // イベントのタイムスタンプ(ms)で連打を判定（GetTickCount 相当）
                        let now = kb.time;
                        let is_double = LAST_ARROW_VK == vk_code
                            && now.wrapping_sub(LAST_ARROW_TICK) <= ARROW_DOUBLE_TAP_MS;
                        LAST_ARROW_VK = vk_code;
                        LAST_ARROW_TICK = now;

                        let action = if context.is_composing() {
                            context.commit()
                        } else {
                            None
                        };
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        hide_candidate_window();
                        if is_double {
                            // 連打 → 行端へジャンプ（元の矢印は消費して Home/End に置換）
                            let edge = if vk_code == VK_LEFT.0 as u32 { VK_HOME } else { VK_END };
                            send_vk(edge);
                            return LRESULT(1);
                        }
                        // 単発 → 元の矢印をアプリに渡してカーソルを1文字動かす
                        return CallNextHookEx(None, code, wparam, lparam);
                    }

                    // Enter: 確定のみ（IME標準動作: 変換中のEnterは改行しない）
                    if vk_code == VK_RETURN.0 as u32 && context.is_composing() {
                        // 予測変換（もしかして/補完）が表示中なら、Enter で
                        // ↑↓で選択中の予測を確定する（番号キーでも選べる）。
                        if candidate_window_visible()
                            && context.candidates.is_empty()
                            && !context.predictions.is_empty()
                        {
                            let sel = context.prediction_index;
                            let action = context.commit_prediction(sel);
                            let preds = context.prediction_display();
                            drop(context);
                            hide_candidate_window();
                            if let Some(action) = action {
                                execute_action(action);
                            }
                            show_prediction_popup(&preds, 0);
                            return LRESULT(1);
                        }
                        // それ以外は通常の確定（確定後は次単語予測を表示）
                        let action = context.commit();
                        let preds = context.prediction_display();
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        show_prediction_popup(&preds, 0);
                        return LRESULT(1);
                    }

                    // Escape:
                    //   末尾から一文節ずつひらがなに戻す（押すたびに前へ）。
                    //   全て戻し終えていたら入力を取り消す。
                    if vk_code == VK_ESCAPE.0 as u32 && context.is_composing() {
                        let action = match context.extend_kana_revert() {
                            Some(a) => Some(a),        // 末尾の文節をかなに戻した
                            None => context.cancel(),  // 戻すものが無い → 取消
                        };
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        hide_candidate_window();
                        return LRESULT(1);
                    }
                }
                // ロックが取れない場合はパススルー
            }
        }

        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[cfg(test)]
mod key_repeat_tests {
    use super::*;

    /// 他のテストと static を共有するため、衝突しない未使用の vkCode を使う。
    const TEST_VK: u32 = 250;

    #[test]
    fn keydown_then_keyup_is_not_a_repeat() {
        // 前のテストの残留状態を掃除
        track_key_repeat(TEST_VK, WM_KEYUP);
        // 最初の押下はリピートではない
        assert!(!track_key_repeat(TEST_VK, WM_KEYDOWN));
        // 離す
        assert!(!track_key_repeat(TEST_VK, WM_KEYUP));
        // 離した後の押下も、また新規の押下なのでリピートではない
        assert!(!track_key_repeat(TEST_VK, WM_KEYDOWN));
        track_key_repeat(TEST_VK, WM_KEYUP);
    }

    #[test]
    fn held_key_keydown_is_a_repeat() {
        const VK: u32 = 251;
        track_key_repeat(VK, WM_KEYUP);
        assert!(!track_key_repeat(VK, WM_KEYDOWN), "最初の押下");
        // keyupを挟まず連続でkeydownが来る＝OSのキーリピート
        assert!(track_key_repeat(VK, WM_KEYDOWN), "2回目以降はリピート扱い");
        assert!(track_key_repeat(VK, WM_KEYDOWN), "3回目もリピート扱い");
        track_key_repeat(VK, WM_KEYUP);
        // keyup後の押下はリピートではない
        assert!(!track_key_repeat(VK, WM_KEYDOWN));
        track_key_repeat(VK, WM_KEYUP);
    }
}

#[cfg(test)]
mod hook_pure_fn_tests {
    use super::*;

    #[test]
    fn vk_to_char_maps_lowercase_letters_without_shift() {
        assert_eq!(vk_to_char(0x41, 0, false), Some('a')); // VK 'A'
        assert_eq!(vk_to_char(0x5A, 0, false), Some('z')); // VK 'Z'
    }

    #[test]
    fn vk_to_char_maps_uppercase_letters_with_shift() {
        assert_eq!(vk_to_char(0x41, 0, true), Some('A'));
        assert_eq!(vk_to_char(0x5A, 0, true), Some('Z'));
    }

    #[test]
    fn vk_to_char_maps_oem_punctuation() {
        assert_eq!(vk_to_char(0xBC, 0, false), Some(',')); // VK_OEM_COMMA → 、
        assert_eq!(vk_to_char(0xBE, 0, false), Some('.')); // VK_OEM_PERIOD → 。
        assert_eq!(vk_to_char(0xBD, 0, false), Some('-')); // VK_OEM_MINUS → ー
        assert_eq!(vk_to_char(0xBF, 0, false), Some('/')); // VK_OEM_2 → ・
        assert_eq!(vk_to_char(0xBF, 0, true), Some('?')); // VK_OEM_2 + shift → ？
        assert_eq!(vk_to_char(0x31, 0, true), Some('!')); // '1' + shift → ！
        assert_eq!(vk_to_char(0xDB, 0, false), Some('[')); // VK_OEM_4 → 「
        assert_eq!(vk_to_char(0xDD, 0, false), Some(']')); // VK_OEM_6 → 」
    }

    #[test]
    fn vk_to_char_returns_none_for_unmapped_or_shifted_oem_vk() {
        // shift付きの VK_OEM_COMMA はマッピング対象外（半角のまま素通しさせる想定）
        assert_eq!(vk_to_char(0xBC, 0, true), None);
        // VK_BACK 等、そもそも対応表に無いキー
        assert_eq!(vk_to_char(0x08, 0, false), None);
    }

    #[test]
    fn is_commit_char_accepts_only_punctuation_marks() {
        assert!(is_commit_char(','));
        assert!(is_commit_char('.'));
        assert!(is_commit_char('?'));
        assert!(is_commit_char('!'));
        assert!(!is_commit_char('a'));
        assert!(!is_commit_char('-'));
    }

    #[test]
    fn is_ime_toggle_vk_matches_known_toggle_keys_only() {
        assert!(is_ime_toggle_vk(0x19)); // VK_KANJI / VK_HANJA
        assert!(is_ime_toggle_vk(0xF3)); // VK_DBE_DBCSCHAR / VK_OEM_AUTO
        assert!(is_ime_toggle_vk(0xF4)); // VK_DBE_SBCSCHAR / VK_OEM_ENLW
        assert!(!is_ime_toggle_vk(0x41)); // 'A' は対象外
    }

    #[test]
    fn is_shift_vk_matches_left_and_right_shift_only() {
        assert!(is_shift_vk(0x10)); // VK_SHIFT
        assert!(is_shift_vk(0xA0)); // VK_LSHIFT
        assert!(is_shift_vk(0xA1)); // VK_RSHIFT
        assert!(!is_shift_vk(0x11)); // VK_CONTROL は対象外
    }
}

#[cfg(test)]
mod shift_state_tests {
    use super::*;
    use serial_test::serial;

    /// plan.md Step 2 の「キー状態追跡（旧称 `track_key_repeat`）の追試」に対応する。
    /// 現コードでキーイベントから状態を追跡する関数は `track_shift_state` のみで、
    /// OS のオートリピート（keyup を挟まず WM_KEYDOWN が連続する）を含む
    /// keydown/keyup 遷移を `SHIFT_HELD` に正しく反映することを検証する。
    /// 共有 static `SHIFT_HELD` に触れるため `#[serial]` で直列化する。
    #[test]
    #[serial]
    fn track_shift_state_follows_keydown_and_keyup() {
        unsafe {
            SHIFT_HELD = false;
            track_shift_state(0xA0, WM_KEYDOWN); // VK_LSHIFT down
            assert!(is_shift_pressed());
            track_shift_state(0xA0, WM_KEYUP); // VK_LSHIFT up
            assert!(!is_shift_pressed());
        }
    }

    /// Alt 併用時は WM_SYSKEYDOWN/WM_SYSKEYUP で届くため、SYS 系イベントでも
    /// 同じ遷移になることを検証する。
    #[test]
    #[serial]
    fn track_shift_state_follows_sys_key_events() {
        unsafe {
            SHIFT_HELD = false;
            track_shift_state(0xA1, WM_SYSKEYDOWN); // VK_RSHIFT down (Alt併用)
            assert!(is_shift_pressed());
            track_shift_state(0xA1, WM_SYSKEYUP);
            assert!(!is_shift_pressed());
        }
    }

    /// OS オートリピート相当: keyup を挟まず keydown が連続しても押下状態は
    /// true のまま安定し、最初の keyup で false に戻ること（リピート境界）。
    #[test]
    #[serial]
    fn track_shift_state_stays_held_across_auto_repeat_keydowns() {
        unsafe {
            SHIFT_HELD = false;
            track_shift_state(0x10, WM_KEYDOWN); // VK_SHIFT down
            track_shift_state(0x10, WM_KEYDOWN); // オートリピート 2回目
            track_shift_state(0x10, WM_KEYDOWN); // オートリピート 3回目
            assert!(is_shift_pressed());
            track_shift_state(0x10, WM_KEYUP);
            assert!(!is_shift_pressed());
        }
    }

    /// Shift 以外の vkCode（修飾キー Ctrl を含む）は `SHIFT_HELD` を変化させない
    /// （キーごとの独立性）。
    #[test]
    #[serial]
    fn track_shift_state_ignores_non_shift_keys() {
        unsafe {
            SHIFT_HELD = false;
            track_shift_state(0x11, WM_KEYDOWN); // VK_CONTROL
            track_shift_state(0x41, WM_KEYDOWN); // 'A'
            assert!(!is_shift_pressed());

            SHIFT_HELD = true;
            track_shift_state(0x11, WM_KEYUP); // 他キーの keyup でも解除されない
            assert!(is_shift_pressed());
            SHIFT_HELD = false; // 後始末
        }
    }
}

#[cfg(test)]
mod window_modes_tests {
    use super::*;
    use serial_test::serial;

    /// `sync_window_mode`（`hook.rs`）内部で行う「hwnd をキーに検索し、無ければ
    /// 追加・あれば更新する」ロジックを `WINDOW_MODES` に対して直接検証する。
    /// `sync_window_mode` 自体は `GetForegroundWindow` 等の実 Win32 呼び出しに
    /// 依存し単体テストできないため、共有状態コンテナの不変条件
    /// （hwnd ごとにモード・コマンド行が独立して保持される）をここで担保する。
    /// 共有 static のため `#[serial]` でテスト間の干渉を防ぐ。
    #[test]
    #[serial]
    fn window_modes_store_independent_state_per_hwnd() {
        WINDOW_MODES.lock().unwrap().clear();

        // ウィンドウA: 日本語入力ON + コマンド行 "foo"
        WINDOW_MODES.lock().unwrap().push((111, true, "foo".to_string()));
        // ウィンドウB: 日本語入力OFF + コマンド行 "bar"
        WINDOW_MODES.lock().unwrap().push((222, false, "bar".to_string()));

        let m = WINDOW_MODES.lock().unwrap();
        let a = m.iter().find(|(h, _, _)| *h == 111).unwrap();
        let b = m.iter().find(|(h, _, _)| *h == 222).unwrap();
        assert_eq!((a.1, a.2.as_str()), (true, "foo"));
        assert_eq!((b.1, b.2.as_str()), (false, "bar"));
    }

    #[test]
    #[serial]
    fn window_modes_update_existing_entry_in_place_without_duplicating() {
        WINDOW_MODES.lock().unwrap().clear();
        WINDOW_MODES.lock().unwrap().push((333, true, "old".to_string()));

        // sync_window_mode の「既存 hwnd なら find で見つけて上書き」相当の操作
        {
            let mut m = WINDOW_MODES.lock().unwrap();
            if let Some(e) = m.iter_mut().find(|(h, _, _)| *h == 333) {
                e.1 = false;
                e.2 = "new".to_string();
            } else {
                m.push((333, false, "new".to_string()));
            }
        }

        let m = WINDOW_MODES.lock().unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0], (333, false, "new".to_string()));
    }
}

#[cfg(test)]
mod mouse_hook_tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
    };

    #[test]
    fn only_button_down_messages_count_as_clicks() {
        assert!(is_mouse_button_down_message(WM_LBUTTONDOWN));
        assert!(is_mouse_button_down_message(WM_RBUTTONDOWN));
        assert!(is_mouse_button_down_message(WM_XBUTTONDOWN));
        assert!(!is_mouse_button_down_message(WM_LBUTTONUP));
        assert!(!is_mouse_button_down_message(WM_MOUSEMOVE));
        assert!(!is_mouse_button_down_message(WM_MOUSEWHEEL));
    }

    #[test]
    fn point_in_rect_excludes_right_and_bottom_edges() {
        let rc = RECT { left: 10, top: 20, right: 110, bottom: 60 };
        assert!(point_in_rect(10, 20, &rc));
        assert!(point_in_rect(109, 59, &rc));
        assert!(!point_in_rect(110, 30, &rc));
        assert!(!point_in_rect(50, 60, &rc));
        assert!(!point_in_rect(9, 30, &rc));
    }

    /// ポップアップ窓が無ければ、どの座標も「ポップアップ内」ではない
    #[test]
    fn popup_without_window_contains_nothing() {
        let hwnd = unsafe { CANDIDATE_HWND }; // static mut への参照を避けるため値コピー
        if hwnd.is_none() {
            assert!(!popup_contains_screen_point(0, 0));
        }
    }

    /// 下書き中のクリックは確定扱い（バッファが空になり、学習用の文節列も
    /// 消化される）。下書きが無ければ何もしない。
    #[test]
    fn settle_composition_on_click_commits_only_while_composing() {
        let mut state = LiveConversionState::new();
        assert!(!settle_composition_on_click(&mut state));

        for ch in "kyou".chars() {
            let _ = state.add_char(ch);
        }
        assert!(state.is_composing());
        assert!(settle_composition_on_click(&mut state));
        assert!(!state.is_composing());
        assert!(state.hiragana_buffer.is_empty());
        assert!(state.candidates.is_empty());
        // 2回目は下書きが無いので何もしない
        assert!(!settle_composition_on_click(&mut state));
    }
}
