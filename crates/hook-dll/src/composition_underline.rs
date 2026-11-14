//! 変換中の文字列の下線（マーカー）表示
//!
//! このIMEはアプリに「未確定文字列」を渡さず、Backspace と文字の打ち込みで
//! ライブ変換を再現している。そのためアプリは通常のIMEのような下線を引けず、
//! どこまでが確定済みで、どこからがまだ変換中なのかが画面から分からない。
//!
//! そこで、変換中の文字列のうち直近の単語（`LiveConversionState::marker_range`。
//! 後ろに付いた助詞・助動詞を含む。候補一覧を開いているときは選び直している語）
//! の画面上の位置を UI Automation の TextPattern で取り、クリックを素通しする
//! 半透明の細い最前面ウィンドウ（黄色のマーカー）を重ねる。変換中の文字列
//! 全体に引くと長い線が文章を覆って読みにくいので、今変わりうる語だけに絞る
//! （2026-11-14 ユーザー要望）。
//!
//! 設計上の制約:
//! - UIA の呼び出しはクロスプロセスで数ms〜十数msかかるので、フックの中では
//!   行わない（[[hook-latency-limit]]）。フックは起床の通知を送るだけで、
//!   位置の取得とウィンドウの移動はこの専用スレッドで行う。
//! - 変換中の文字は必ずカーソルの直前にある（カーソルを動かすと確定する）
//!   ので、「カーソルから N 文字戻った範囲」が変換中の文字列になる。
//! - 位置が取れないアプリ（TextPattern が無い等）では何も描かない。
//! - パスワード欄・管理者権限のアプリ（`FocusSafety` が Safe 以外）では、
//!   UIA で文字を読むこと自体をしない。
//! - 打鍵が無くても、タイマー側の再変換やスクロールで位置が変わるので、
//!   表示中は短い周期で取り直す。
//!
//! 無効化: 環境変数 `IME_UNDERLINE=0`

use crate::*;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::*;

static WAKE_TX: OnceLock<SyncSender<()>> = OnceLock::new();

/// 同時に描ける行数（変換中の文字列が折り返したとき用）
const MAX_LINES: usize = 8;
/// マーカーの色（黄色。COLORREF は 0x00BBGGRR）
const MARKER_COLOR: u32 = 0x004D_E1FF;
/// マーカーの太さと不透明度
const THICKNESS: i32 = 5;
const ALPHA: u8 = 150;
/// 表示中に位置を取り直す周期
const ACTIVE_POLL: Duration = Duration::from_millis(80);
/// 打鍵の直後、アプリが文字を描き終えるまで待つ時間
const SETTLE: Duration = Duration::from_millis(25);

/// フックから呼ぶ。位置の取り直しを頼むだけで、すぐ返る。
pub(crate) fn notify() {
    if let Some(tx) = WAKE_TX.get() {
        let _ = tx.try_send(());
    }
}

pub(crate) fn start_composition_underline_worker() {
    if std::env::var("IME_UNDERLINE").map(|v| v == "0").unwrap_or(false) {
        debug_log!("下線表示: IME_UNDERLINE=0 のため無効");
        return;
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if WAKE_TX.set(tx).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("composition-underline".into())
        .spawn(move || unsafe { worker(rx) })
        .ok();
}

/// 下線を引く範囲（カーソルから戻った文字数の (始まり, 終わり)）。変換中でなければ None
fn marker_range() -> Option<(usize, usize)> {
    unsafe {
        if !IS_ENABLED || !OUR_ACTIVE {
            return None;
        }
    }
    if uia::FOCUSED_PASSWORD_STATE.load() != uia::FocusSafety::Safe
        || uia::FOCUSED_ELEVATED_STATE.load() != uia::FocusSafety::Safe
    {
        return None;
    }
    let ctx = LIVE_CONTEXT.get()?.lock().ok()?;
    if ctx.last_sent_length == 0 {
        return None;
    }
    ctx.marker_range()
}

unsafe fn worker(rx: Receiver<()>) {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::HiDpi::{
        SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // UIA の矩形は物理ピクセル。このスレッドのウィンドウも物理ピクセルで置く
    // （プロセス全体の DPI 設定は変えない）
    let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let auto: IUIAutomation = match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
        Ok(a) => a,
        Err(e) => {
            debug_log!("下線表示: UIA初期化失敗 {:?}", e);
            return;
        }
    };
    let Some(windows) = create_marker_windows() else {
        debug_log!("下線表示: ウィンドウ作成失敗");
        return;
    };
    let mut shown = false;
    loop {
        pump_messages();
        // 表示中は周期的に取り直す。非表示なら次の打鍵まで待つ
        let wait = if shown { ACTIVE_POLL } else { Duration::from_millis(1000) };
        match rx.recv_timeout(wait) {
            Ok(()) => std::thread::sleep(SETTLE),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let lines = marker_range().and_then(|(from, to)| marker_lines(&auto, from, to));
        match lines {
            Some(lines) if !lines.is_empty() => {
                place(&windows, &lines);
                shown = true;
            }
            _ => {
                if shown {
                    hide_all(&windows);
                }
                shown = false;
            }
        }
    }
}

unsafe fn pump_messages() {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// カーソルから `from` 文字戻った位置〜`to` 文字戻った位置の各行のマーカー位置
unsafe fn marker_lines(auto: &IUIAutomation, from: usize, to: usize) -> Option<Vec<RECT>> {
    let elem = auto.GetFocusedElement().ok()?;
    if elem.CurrentProcessId().ok()? == std::process::id() as i32 {
        return None;
    }
    if elem.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(true) {
        return None;
    }
    let bounds = elem.CurrentBoundingRectangle().ok()?;
    let caret = caret_range(&elem)?;
    let mut out = range_rects(&caret, from as i32, to as i32)?;
    // Windows Terminal は全角文字を含む範囲の矩形を1〜2マス長めに返す
    // （2026-11-14 実測: 「とへn」が5マスのところ7マス）。カーソルのマスの
    // 左端より右へは描かない。他のアプリでは、文末のカーソルを文字単位に
    // 広げると直前の文字になってしまうので、この補正はしない。
    if elem.CurrentClassName().map(|c| c.to_string()).unwrap_or_default() == "TermControl" {
        if let Some(cx) = terminal_cursor_left(&caret) {
            for r in out.iter_mut() {
                if r.left < cx && cx < r.right {
                    r.right = cx;
                }
            }
        }
    }
    // 欄の外に出た矩形（スクロールで隠れた部分、最小化中の座標等）は描かない
    out.retain(|r| inside(r, &bounds));
    out.truncate(MAX_LINES);
    Some(out)
}

/// 欄の矩形の中に（少しのはみ出しは許して）収まっているか
fn inside(r: &RECT, bounds: &RECT) -> bool {
    const SLACK: i32 = 4;
    r.left >= bounds.left - SLACK
        && r.right <= bounds.right + SLACK
        && r.top >= bounds.top - SLACK
        && r.bottom <= bounds.bottom + SLACK
        && r.right > r.left
        && r.bottom > r.top
}

/// カーソル位置の空範囲（TextPattern2 の GetCaretRange、無ければ選択の末尾）
unsafe fn caret_range(elem: &IUIAutomationElement) -> Option<IUIAutomationTextRange> {
    if let Ok(p2) = elem.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) {
        let mut active = BOOL::default();
        if let Ok(r) = p2.GetCaretRange(&mut active) {
            return Some(r);
        }
    }
    let p: IUIAutomationTextPattern = elem.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
    let sel = p.GetSelection().ok()?;
    if sel.Length().ok()? < 1 {
        return None;
    }
    let r = sel.GetElement(0).ok()?;
    let _ = r.MoveEndpointByRange(TextPatternRangeEndpoint_Start, &r, TextPatternRangeEndpoint_End);
    Some(r)
}

/// ターミナルのカーソルのマスの左端（空範囲を1文字に広げた矩形の左）
unsafe fn terminal_cursor_left(caret: &IUIAutomationTextRange) -> Option<i32> {
    let r = caret.Clone().ok()?;
    r.ExpandToEnclosingUnit(TextUnit_Character).ok()?;
    bounding_rects(&r).first().map(|r| r.left)
}

/// カーソルから `from` 文字戻った位置〜`to` 文字戻った位置の範囲の、行ごとの矩形
unsafe fn range_rects(caret: &IUIAutomationTextRange, from: i32, to: i32) -> Option<Vec<RECT>> {
    let r = caret.Clone().ok()?;
    let moved = r
        .MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -from)
        .ok()?;
    // 戻れた文字数が足りない＝カーソルの前に変換中の文字列が無い（位置の
    // 取り違え）ので描かない
    if -moved != from {
        return None;
    }
    if to > 0 {
        let moved_end = r
            .MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, -to)
            .ok()?;
        if -moved_end != to {
            return None;
        }
    }
    Some(merge_rows(bounding_rects(&r)))
}

unsafe fn bounding_rects(range: &IUIAutomationTextRange) -> Vec<RECT> {
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
        SafeArrayUnaccessData,
    };
    let Ok(psa) = range.GetBoundingRectangles() else { return vec![] };
    if psa.is_null() {
        return vec![];
    }
    let mut out = vec![];
    if let (Ok(lb), Ok(ub)) = (SafeArrayGetLBound(psa, 1), SafeArrayGetUBound(psa, 1)) {
        let count = (ub - lb + 1).max(0) as usize;
        let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
        if count >= 4 && SafeArrayAccessData(psa, &mut p).is_ok() {
            let d = std::slice::from_raw_parts(p as *const f64, count);
            for c in d.chunks_exact(4) {
                let (x, y, w, h) = (c[0], c[1], c[2], c[3]);
                if w >= 1.0 && h >= 1.0 {
                    out.push(RECT {
                        left: x.round() as i32,
                        top: y.round() as i32,
                        right: (x + w).round() as i32,
                        bottom: (y + h).round() as i32,
                    });
                }
            }
            let _ = SafeArrayUnaccessData(psa);
        }
    }
    let _ = SafeArrayDestroy(psa);
    out
}

/// 同じ行（上下がほぼ同じ）の矩形を1本にまとめる。アプリによっては1文字
/// ごと・書式の切れ目ごとに矩形を返すため。
fn merge_rows(mut rects: Vec<RECT>) -> Vec<RECT> {
    rects.sort_by_key(|r| (r.top, r.left));
    let mut out: Vec<RECT> = Vec::new();
    for r in rects {
        match out.last_mut() {
            Some(last) if (last.top - r.top).abs() <= 2 && (last.bottom - r.bottom).abs() <= 2 => {
                last.left = last.left.min(r.left);
                last.right = last.right.max(r.right);
            }
            _ => out.push(r),
        }
    }
    out
}

unsafe extern "system" fn marker_wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

unsafe fn create_marker_windows() -> Option<Vec<HWND>> {
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    let hinst = GetModuleHandleW(None).ok()?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(marker_wndproc),
        hInstance: hinst.into(),
        lpszClassName: w!("IMELiveConverterMarker"),
        hbrBackground: CreateSolidBrush(COLORREF(MARKER_COLOR)),
        ..Default::default()
    };
    RegisterClassW(&class);
    let mut out = Vec::with_capacity(MAX_LINES);
    for _ in 0..MAX_LINES {
        // LAYERED+TRANSPARENT: クリックを下のアプリへ素通しする（ヒットテストの
        // 対象外になるので、入力欄ワープの WindowFromPoint にも映らない）。
        // NOACTIVATE: 表示してもフォーカスを奪わない。
        let h = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("IMELiveConverterMarker"),
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            hinst,
            None,
        )
        .ok()?;
        out.push(h);
    }
    Some(out)
}

unsafe fn place(windows: &[HWND], lines: &[RECT]) {
    for (i, h) in windows.iter().enumerate() {
        let Some(m) = lines.get(i) else {
            let _ = ShowWindow(*h, SW_HIDE);
            continue;
        };
        let _ = SetLayeredWindowAttributes(*h, COLORREF(0), ALPHA, LWA_ALPHA);
        // 文字の下端に、蛍光ペンで下線を引いたように重ねる
        let _ = SetWindowPos(
            *h,
            HWND_TOPMOST,
            m.left,
            m.bottom - THICKNESS,
            m.right - m.left,
            THICKNESS,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

unsafe fn hide_all(windows: &[HWND]) {
    for h in windows {
        let _ = ShowWindow(*h, SW_HIDE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: r, bottom: b }
    }

    #[test]
    fn merges_rects_on_the_same_row() {
        let got = merge_rows(vec![rc(30, 10, 40, 30), rc(10, 10, 20, 30), rc(10, 40, 20, 60)]);
        assert_eq!(got, vec![rc(10, 10, 40, 30), rc(10, 40, 20, 60)]);
    }

    #[test]
    fn rejects_rects_outside_the_field() {
        let field = rc(0, 0, 100, 50);
        assert!(inside(&rc(10, 10, 50, 30), &field));
        // 最小化中のウィンドウ等の画面外の座標
        assert!(!inside(&rc(-31956, -31948, -31955, -31932), &field));
        assert!(!inside(&rc(10, 10, 10, 30), &field));
    }
}
