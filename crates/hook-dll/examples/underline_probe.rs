//! 変換中の文字に下線を引けるかの検証用ツール（IME 本体とは独立）。
//!
//! IME はアプリに「未確定文字列」を渡さず文字を直接打ち込んでいるので、
//! アプリ側は下線を引けない。代わりに「カーソルの直前 N 文字」の画面上の
//! 位置を UI Automation の TextPattern で取り、クリックを素通しする細い
//! 最前面ウィンドウを重ねて下線に見せる。この方式が各アプリで成り立つかを
//! 確かめるのが目的。
//!
//! 使い方:
//!     cargo run --release -p hook-dll --example underline_probe -- [N]
//! 起動したら、確かめたいアプリの入力欄で普通に文字を打つ。カーソルの直前
//! N 文字（既定 4）に赤い下線が付けば成功。結果はコンソールに出る
//! （アプリのクラス名・取れた文字・矩形・所要時間）。Ctrl+C で終了。
//!
//! 文字の位置だけを読み、入力欄の文章は保存しない。パスワード欄は読まない。

use std::time::Instant;
use windows::core::{w, BSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayUnaccessData,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 下線ウィンドウの数（＝同時に描ける行数）
const LINES: usize = 6;
const THICKNESS: i32 = 2;
const TICK_MS: u32 = 100;

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

unsafe fn create_line_windows() -> Vec<HWND> {
    let hinst = GetModuleHandleW(None).unwrap();
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        lpszClassName: w!("UnderlineProbe"),
        // 赤（COLORREF は 0x00BBGGRR）
        hbrBackground: CreateSolidBrush(COLORREF(0x0000_30E0)),
        ..Default::default()
    };
    RegisterClassW(&class);
    (0..LINES)
        .map(|_| {
            let h = CreateWindowExW(
                // LAYERED+TRANSPARENT でクリックを下のアプリへ素通しする。
                // NOACTIVATE で表示してもフォーカスを奪わない。
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                w!("UnderlineProbe"),
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
            .unwrap();
            let _ = SetLayeredWindowAttributes(h, COLORREF(0), 230, LWA_ALPHA);
            h
        })
        .collect()
}

/// (left, top, width, height) の矩形群
unsafe fn rects_of(range: &IUIAutomationTextRange) -> Vec<(f64, f64, f64, f64)> {
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
                out.push((c[0], c[1], c[2], c[3]));
            }
            let _ = SafeArrayUnaccessData(psa);
        }
    }
    let _ = SafeArrayDestroy(psa);
    out
}

/// カーソル位置の空範囲。TextPattern2 があればその GetCaretRange、無ければ
/// 選択範囲の末尾に縮めたもの。どちらを使ったかも返す。
unsafe fn caret_range(elem: &IUIAutomationElement) -> Option<(IUIAutomationTextRange, &'static str)> {
    if let Ok(p2) = elem.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) {
        let mut active = BOOL::default();
        if let Ok(r) = p2.GetCaretRange(&mut active) {
            return Some((r, "TextPattern2"));
        }
    }
    let p: IUIAutomationTextPattern = elem.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
    let sel = p.GetSelection().ok()?;
    if sel.Length().ok()? < 1 {
        return None;
    }
    let r = sel.GetElement(0).ok()?;
    // 選択の末尾へ縮める（選択していなければ元から空範囲）
    let _ = r.MoveEndpointByRange(TextPatternRangeEndpoint_Start, &r, TextPatternRangeEndpoint_End);
    Some((r, "Selection"))
}

struct Probe {
    text: String,
    rects: Vec<(f64, f64, f64, f64)>,
    via: &'static str,
    moved: i32,
}

unsafe fn probe(elem: &IUIAutomationElement, n: i32) -> Result<Probe, &'static str> {
    let (caret, via) = caret_range(elem).ok_or("TextPattern なし／カーソル範囲が取れない")?;
    let r = caret.Clone().map_err(|_| "Clone 失敗")?;
    let moved = r
        .MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -n)
        .map_err(|_| "MoveEndpointByUnit 失敗")?;
    let text = r.GetText(64).map(|b: BSTR| b.to_string()).unwrap_or_default();
    Ok(Probe { text, rects: rects_of(&r), via, moved })
}

unsafe fn place(lines: &[HWND], rects: &[(f64, f64, f64, f64)]) {
    for (i, h) in lines.iter().enumerate() {
        match rects.get(i) {
            Some(&(x, y, w, ht)) if w >= 1.0 && ht >= 1.0 => {
                let _ = SetWindowPos(
                    *h,
                    HWND_TOPMOST,
                    x as i32,
                    (y + ht) as i32 - THICKNESS,
                    w.ceil() as i32,
                    THICKNESS,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
            _ => {
                let _ = ShowWindow(*h, SW_HIDE);
            }
        }
    }
}

fn class_of(h: HWND) -> String {
    let mut b = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

/// `--scan`: フォーカスを動かさず、デスクトップ上の入力欄候補（Edit・Document・
/// Windows Terminal のペイン）それぞれで、カーソル（選択）直前の n 文字の矩形が
/// 取れるかを一覧する。フォーカスの無い欄でも選択位置は残っているので、
/// 「そのアプリで方式が成り立つか」を打鍵せずに確かめられる。下線は描かない。
unsafe fn scan(auto: &IUIAutomation, n: i32) {
    let Ok(root) = auto.GetRootElement() else { return };
    let mk = |id: UIA_PROPERTY_ID, v: windows::core::VARIANT| auto.CreatePropertyCondition(id, &v).unwrap();
    let edit = mk(UIA_ControlTypePropertyId, UIA_EditControlTypeId.0.into());
    let doc = mk(UIA_ControlTypePropertyId, UIA_DocumentControlTypeId.0.into());
    let term = mk(UIA_ClassNamePropertyId, BSTR::from("TermControl").into());
    let any = auto.CreateOrCondition(&auto.CreateOrCondition(&edit, &doc).unwrap(), &term).unwrap();
    let t0 = Instant::now();
    let Ok(all) = root.FindAll(TreeScope_Descendants, &any) else { return };
    let count = all.Length().unwrap_or(0);
    println!("候補 {count} 個（走査 {}ms）", t0.elapsed().as_millis());
    for i in 0..count {
        let Ok(e) = all.GetElement(i) else { continue };
        if e.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(true) {
            continue;
        }
        let ct = e.CurrentControlType().map(|c| c.0).unwrap_or(0);
        let cls = e.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();
        let name: String = e.CurrentName().map(|b| b.to_string()).unwrap_or_default().chars().take(20).collect();
        let hwnd = e.CurrentNativeWindowHandle().unwrap_or_default();
        let top = GetAncestor(hwnd, GA_ROOT);
        let app = if top.0.is_null() { String::from("?") } else { class_of(top) };
        if e.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) {
            println!("- パスワード欄は読まない app='{app}'");
            continue;
        }
        let t1 = Instant::now();
        match probe(&e, n) {
            Ok(p) => println!(
                "○ app='{app}' 要素={ct}/'{cls}' name='{name}' 経由={} 戻れた={}字 文字={:?} 矩形={:?} [{}ms]",
                p.via,
                -p.moved,
                p.text,
                p.rects.iter().map(|r| (r.0 as i32, r.1 as i32, r.2 as i32, r.3 as i32)).collect::<Vec<_>>(),
                t1.elapsed().as_millis()
            ),
            Err(err) => println!("× app='{app}' 要素={ct}/'{cls}' name='{name}' {err}"),
        }
    }
}

fn main() {
    let n: i32 = std::env::args().skip(1).find_map(|s| s.parse().ok()).unwrap_or(4);
    unsafe {
        // UIA の矩形は物理ピクセル。こちらも物理ピクセルで置くため DPI 対応にする
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        println!("system DPI = {} (96 = 100%)", GetDpiForSystem());
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let auto: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).expect("UIA");
        if std::env::args().any(|a| a == "--scan") {
            scan(&auto, n);
            return;
        }
        let lines = create_line_windows();
        let me = std::process::id() as i32;
        SetTimer(None, 0, TICK_MS, None);
        println!("カーソル直前の {n} 文字に下線を引きます。入力欄で打ってみてください（Ctrl+C で終了）");

        let mut last_report = String::new();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message != WM_TIMER {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                continue;
            }
            let t0 = Instant::now();
            let Ok(elem) = auto.GetFocusedElement() else { continue };
            if elem.CurrentProcessId().unwrap_or(0) == me {
                continue;
            }
            let fg = GetForegroundWindow();
            let app = class_of(fg);
            let ct = elem.CurrentControlType().map(|c| c.0).unwrap_or(0);
            let cls = elem.CurrentClassName().map(|b| b.to_string()).unwrap_or_default();
            let is_pw = elem.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false);
            let result = if is_pw { Err("パスワード欄なので読まない") } else { probe(&elem, n) };
            let ms = t0.elapsed().as_millis();
            let report = match &result {
                Ok(p) => {
                    place(&lines, &p.rects);
                    let rs: Vec<String> = p
                        .rects
                        .iter()
                        .map(|r| format!("({:.0},{:.0} {:.0}x{:.0})", r.0, r.1, r.2, r.3))
                        .collect();
                    format!(
                        "○ app='{app}' 要素={ct}/'{cls}' 経由={} 戻れた={}字 文字={:?} 矩形{}個 {}",
                        p.via,
                        -p.moved,
                        p.text,
                        p.rects.len(),
                        rs.join(" ")
                    )
                }
                Err(e) => {
                    place(&lines, &[]);
                    format!("× app='{app}' 要素={ct}/'{cls}' {e}")
                }
            };
            // 同じ内容は繰り返さない。時間は変化があったときだけ出す
            if report != last_report {
                println!("{report}  [{ms}ms]");
                last_report = report;
            }
        }
    }
}
