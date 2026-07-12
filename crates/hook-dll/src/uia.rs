//! UI Automation ポーラー: フォーカス入力欄・キャレット位置の取得と統合ターミナル検出

use crate::*;

/// フォーカス中の要素が「統合ターミナル」（VSCode の xterm 等）か。UIAポーラーが
/// クラス名から判定して更新する。窓クラスで判別できないアプリの端末検出に使う。
pub(crate) static FOCUSED_IS_TERMINAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// この Space 押下で既に LLM 変換を発火したか（オートリピートの二重発火防止）

/// UI Automation で取得したフォーカス入力欄の位置キャッシュ (x, y)
/// バックグラウンドスレッドが更新し、ポップアップ表示時に参照する。
/// ブラウザ・ターミナル等 Win32 キャレットを公開しないアプリ向け。
/// 値は (x, キャレット上端y, キャレット下端y)（画面座標）。
pub(crate) static UIA_ANCHOR: Mutex<Option<(i32, i32, i32)>> = Mutex::new(None);

/// UI Automation でフォーカス入力欄の位置を定期取得するスレッドを開始
///
/// クロスプロセスの同期 COM 呼び出しはブロックしうるため、フック
/// スレッドではなく専用スレッドで実行し、結果をキャッシュに置く。
pub(crate) fn start_uia_poller() {
    std::thread::spawn(|| unsafe {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};

        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(a) => a,
                Err(_) => return,
            };

        let mut last_hwnd: isize = 0;
        // カーソル（フォーカス）がターミナルへ「戻った」瞬間の検出用
        let mut was_terminal = false;
        let mut last_terminal_hwnd: isize = 0;
        loop {
            // 軽量: フォーカス要素のクラス名から「統合ターミナル(xterm 等)」かを判定。
            // 窓クラスで判別できない VSCode 等の端末をコマンドモード対象にするため、
            // アイドル時でも実施する（キャレット/テキスト範囲は触らないのでチラつかない）。
            // 対象はアプリ本体と同じ窓クラスになりがちな Electron 系(Chrome_WidgetWin_1)に絞る。
            let fg_class = foreground_class_name();
            let is_term = if is_terminal_class(&fg_class) {
                false // 純粋端末は同期判定側に任せる（フラグは不要）
            } else if fg_class == "Chrome_WidgetWin_1" {
                focused_element_is_terminal(&auto)
            } else {
                false
            };
            FOCUSED_IS_TERMINAL.store(is_term, std::sync::atomic::Ordering::Relaxed);

            // カーソルがターミナルへ戻った（非ターミナル→ターミナル、または別の
            // ターミナル窓へ切替）瞬間を検出し、フックスレッドへ「コマンド候補の
            // 再表示」を依頼する。打鍵を待たずにモーダルが復元される。
            // ※ UI操作はウィンドウを作ったフックスレッドで行うため PostMessage で渡す。
            let fg_now = GetForegroundWindow().0 as isize;
            let terminal_now = is_term || is_terminal_class(&fg_class);
            if terminal_now && (!was_terminal || fg_now != last_terminal_hwnd) {
                if let Some(hwnd) = CANDIDATE_HWND {
                    let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        hwnd,
                        WM_APP_RESHOW_COMMAND,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            was_terminal = terminal_now;
            if terminal_now {
                last_terminal_hwnd = fg_now;
            }

            // 入力欄の位置が必要なのは「変換中(OUR_ACTIVE)」か「候補/コマンドの
            // ポップアップ表示中」だけ。アイドル時に UIA(キャレット) や AttachConsole を
            // 毎回叩くと、対象アプリのカーソル点滅が乱れるため、不要時は休む。
            let need_pos = OUR_ACTIVE || candidate_window_visible();
            if !need_pos {
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }
            let hwnd_fg = GetForegroundWindow();
            let uia = uia_focused_anchor(&auto);
            // UIA でカーソルが取れなければ、クラシックコンソール(conhost)向けに
            // コンソールAPIでカーソル位置を取得する（PowerShell窓 等）。
            let pos = uia.or_else(|| console_caret_screen_pos(hwnd_fg));

            // 診断: フォアグラウンド窓が変わったら、そのクラス名と取得結果をログ
            let cur = hwnd_fg.0 as isize;
            if cur != last_hwnd {
                last_hwnd = cur;
                let mut cls = [0u16; 128];
                let n = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd_fg, &mut cls);
                let class = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
                debug_log!(
                    "位置診断: fg class='{}' uia={} console={} pos={:?}",
                    class,
                    uia.is_some(),
                    console_caret_screen_pos(hwnd_fg).is_some(),
                    pos
                );
                // UIA が取れていないなら、原因を1回詳しくログ
                if uia.is_none() {
                    uia_diag(&auto);
                }
            }

            if let Ok(mut c) = UIA_ANCHOR.lock() {
                *c = pos;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    });
}

/// クラシックコンソール(conhost)のカーソル画面座標 (x, 上端y, 下端y) を取得する
///
/// UIA が効かない旧来のコンソール窓（PowerShell/cmd を直接起動）向け。対象
/// コンソールに AttachConsole し、GetConsoleScreenBufferInfo でカーソルの
/// セル位置を取り、フォントサイズとクライアント原点から画面ピクセルへ変換する。
///
/// 自プロセスが既にコンソールを持つ（対話起動）場合は AttachConsole が失敗/
/// 破壊的になるためスキップする（--background 常駐時は安全）。
pub(crate) unsafe fn console_caret_screen_pos(hwnd_fg: HWND) -> Option<(i32, i32, i32)> {
    use windows::Win32::System::Console::{
        AttachConsole, FreeConsole, GetConsoleScreenBufferInfo, GetConsoleWindow,
        GetCurrentConsoleFont, GetStdHandle, CONSOLE_FONT_INFO, CONSOLE_SCREEN_BUFFER_INFO,
        STD_OUTPUT_HANDLE,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;

    if hwnd_fg.0.is_null() {
        return None;
    }
    // フォアグラウンドがクラシックコンソール窓か（クラス名で判定）
    let mut cls = [0u16; 64];
    let n = GetClassNameW(hwnd_fg, &mut cls);
    let class = if n > 0 {
        String::from_utf16_lossy(&cls[..n as usize])
    } else {
        String::new()
    };
    if class != "ConsoleWindowClass" {
        return None; // コンソール窓でない（ログは出さない：他アプリで頻発するため）
    }
    debug_log!("console: 窓検出 class='{}'", class);
    // 既に自分のコンソールがある（対話モード）ならアタッチできないのでスキップ
    if !GetConsoleWindow().0.is_null() {
        debug_log!("console: 自プロセスにコンソールあり→スキップ（--background で起動してください）");
        return None;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd_fg, Some(&mut pid));
    if pid == 0 {
        return None;
    }
    if AttachConsole(pid).is_err() {
        debug_log!("console: AttachConsole 失敗 pid={}", pid);
        return None;
    }
    let result = (|| {
        // AttachConsole 後、標準出力ハンドルが対象コンソールの画面バッファを指す
        let conout = GetStdHandle(STD_OUTPUT_HANDLE).ok()?;
        let mut csbi = CONSOLE_SCREEN_BUFFER_INFO::default();
        let ok1 = GetConsoleScreenBufferInfo(conout, &mut csbi).is_ok();
        let mut font = CONSOLE_FONT_INFO::default();
        let ok2 = GetCurrentConsoleFont(conout, false, &mut font).is_ok();
        if !ok1 || !ok2 || font.dwFontSize.X <= 0 || font.dwFontSize.Y <= 0 {
            debug_log!("console: 情報取得失敗 sbi={} font={} size={}x{}", ok1, ok2, font.dwFontSize.X, font.dwFontSize.Y);
            return None;
        }
        let cw = font.dwFontSize.X as i32;
        let ch = font.dwFontSize.Y as i32;
        // カーソルの「可視ウィンドウ内」相対セル
        let col = (csbi.dwCursorPosition.X - csbi.srWindow.Left) as i32;
        let row = (csbi.dwCursorPosition.Y - csbi.srWindow.Top) as i32;
        // コンソール窓クライアント原点（画面座標）
        let mut origin = POINT { x: 0, y: 0 };
        let _ = ClientToScreen(hwnd_fg, &mut origin);
        let x = origin.x + col * cw;
        let top = origin.y + row * ch;
        debug_log!(
            "console: cursor cell=({},{}) font={}x{} origin=({},{}) -> ({},{},{})",
            col, row, cw, ch, origin.x, origin.y, x, top, top + ch
        );
        Some((x, top, top + ch))
    })();
    let _ = FreeConsole();
    result
}

/// フォーカス中の UIA 要素の入力位置を返す
///
/// まず TextPattern で実際のカーソル（選択範囲）位置を取得する。
/// これは Chrome・Electron 等の大きな入力欄でも正確。取得できない場合は
/// 要素の矩形（大きすぎる要素は除外）にフォールバックする。
/// フォーカス中の UIA 要素が統合ターミナル（xterm.js 等）か判定する。
/// VSCode の統合ターミナルはフォーカス要素のクラス名が "xterm-helper-textarea"、
/// アクセシブル名に "Terminal"/"ターミナル" を含むことが多い。
/// キャレットやテキスト範囲は触らない（クラス名/名前の読み取りのみ）ので軽い。
pub(crate) unsafe fn focused_element_is_terminal(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
) -> bool {
    let Ok(elem) = auto.GetFocusedElement() else {
        return false;
    };
    if let Ok(cn) = elem.CurrentClassName() {
        let s = cn.to_string().to_lowercase();
        if s.contains("xterm") {
            return true;
        }
    }
    if let Ok(nm) = elem.CurrentName() {
        let s = nm.to_string();
        if s.to_lowercase().contains("terminal") || s.contains("ターミナル") {
            return true;
        }
    }
    false
}

pub(crate) unsafe fn uia_focused_anchor(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
) -> Option<(i32, i32, i32)> {
    let elem = auto.GetFocusedElement().ok()?;

    // 1. TextPattern で選択（＝カーソル）位置を取得
    if let Some(pos) = uia_caret_from_textpattern(&elem) {
        return Some(pos);
    }

    // 2. 要素の矩形（単一行に近い入力欄向け。巨大要素は不採用）
    let r = elem.CurrentBoundingRectangle().ok()?;
    if r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    if (r.bottom - r.top) > 200 {
        return None;
    }
    Some((r.left + 2, r.top, r.bottom))
}

/// フォーカス要素からカーソルの画面座標 (x, 上端y, 下端y) を取得する
///
/// まず TextPattern2 の GetCaretRange でカーソルを直接取得する（Windows
/// Terminal など対応アプリで正確）。取れなければ TextPattern の選択範囲末尾を
/// カーソル位置とみなす。
pub(crate) unsafe fn uia_caret_from_textpattern(
    elem: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Option<(i32, i32, i32)> {
    use windows::Win32::UI::Accessibility::{
        IUIAutomationTextPattern, IUIAutomationTextPattern2, UIA_TextPattern2Id, UIA_TextPatternId,
    };

    // 1. TextPattern2::GetCaretRange（キャレットを直接取得）
    if let Ok(p2) = elem.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) {
        let mut is_active = windows::Win32::Foundation::BOOL::default();
        if let Ok(range) = p2.GetCaretRange(&mut is_active) {
            if let Some(pos) = rect_with_expand(&range) {
                return Some(pos);
            }
        }
    }

    // 2. TextPattern の選択範囲末尾（＝カーソル）
    let pattern: IUIAutomationTextPattern = elem.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
    let selection = pattern.GetSelection().ok()?;
    if selection.Length().ok()? < 1 {
        return None;
    }
    let range = selection.GetElement(0).ok()?;
    rect_with_expand(&range)
}

/// UIA のカーソル取得がなぜ失敗するかを1回だけ詳しくログする（診断用）
pub(crate) unsafe fn uia_diag(auto: &windows::Win32::UI::Accessibility::IUIAutomation) {
    use windows::Win32::UI::Accessibility::{
        IUIAutomationTextPattern, IUIAutomationTextPattern2, UIA_TextPattern2Id, UIA_TextPatternId,
    };
    let elem = match auto.GetFocusedElement() {
        Ok(e) => e,
        Err(e) => {
            debug_log!("uia診断: GetFocusedElement 失敗 {:?}", e);
            return;
        }
    };
    let name = elem.CurrentName().map(|b| b.to_string()).unwrap_or_default();
    let ct = elem.CurrentControlType().map(|c| c.0).unwrap_or(-1);
    let has_tp2 = elem
        .GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id)
        .is_ok();
    let has_tp = elem
        .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        .is_ok();
    debug_log!(
        "uia診断: name='{}' ctrlType={} TextPattern2={} TextPattern={}",
        name, ct, has_tp2, has_tp
    );
    if let Ok(p2) = elem.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) {
        let mut a = windows::Win32::Foundation::BOOL::default();
        match p2.GetCaretRange(&mut a) {
            Ok(r) => debug_log!(
                "uia診断: GetCaretRange ok active={} rect={:?}",
                a.as_bool(),
                rect_with_expand(&r)
            ),
            Err(e) => debug_log!("uia診断: GetCaretRange err {:?}", e),
        }
    }
    if let Ok(p) = elem.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
        if let Ok(sel) = p.GetSelection() {
            debug_log!("uia診断: selection len={:?}", sel.Length());
        }
    }
}

/// テキスト範囲から矩形を取り出す。空範囲（0幅キャレット）で取れない場合は
/// 文字単位に広げて再取得する（Windows Terminal のカーソル等）。
pub(crate) unsafe fn rect_with_expand(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
) -> Option<(i32, i32, i32)> {
    use windows::Win32::UI::Accessibility::TextUnit_Character;
    if let Some(pos) = rect_from_text_range(range) {
        return Some(pos);
    }
    // 空範囲 → 複製して1文字分に広げてから矩形を取る
    let expanded = range.Clone().ok()?;
    let _ = expanded.ExpandToEnclosingUnit(TextUnit_Character);
    rect_from_text_range(&expanded)
}

/// テキスト範囲の境界矩形の末尾から (x, 上端y, 下端y) を取り出す
pub(crate) unsafe fn rect_from_text_range(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
) -> Option<(i32, i32, i32)> {
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
        SafeArrayUnaccessData,
    };

    let psa = range.GetBoundingRectangles().ok()?;
    if psa.is_null() {
        return None;
    }
    // SAFEARRAY of f64: 4個ずつ (left, top, width, height) の矩形群
    let result = (|| {
        let lb = SafeArrayGetLBound(psa, 1).ok()?;
        let ub = SafeArrayGetUBound(psa, 1).ok()?;
        let count = (ub - lb + 1).max(0) as usize;
        if count < 4 {
            return None;
        }
        let mut pdata: *mut core::ffi::c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut pdata).ok()?;
        let data = std::slice::from_raw_parts(pdata as *const f64, count);
        // 最後の矩形（範囲末尾＝カーソル位置）の上端・下端
        let base = count - 4;
        let left = data[base];
        let top = data[base + 1];
        let height = data[base + 3];
        let pos = (left as i32 + 2, top as i32, (top + height) as i32);
        let _ = SafeArrayUnaccessData(psa);
        Some(pos)
    })();

    let _ = SafeArrayDestroy(psa);
    result
}
