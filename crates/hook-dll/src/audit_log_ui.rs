//! 変換パイプラインの段階別監査ログウィンドウ（Ctrl+Alt+L）
//!
//! `LiveConversionState::trace_buffer` に溜まった直近の変換トレースを
//! `SysListView32`（レポート表示）に並べる。標準コントロールなので
//! MSAA/UI Automation を既定でエクスポーズし、Narrator/NVDA が行を読み上げ、
//! ↑↓ で段階を遡行できる。選択ステップ／全ステップのコピーと JSON
//! エクスポートはここ（UIスレッド）でのみ行い、フックのホットパスでは
//! 一切のファイルI/O・描画をしない。

use crate::*;
use common::ConversionTrace;

/// 監査ログウィンドウのハンドル（なければ未生成）
pub(crate) static mut AUDIT_HWND: Option<HWND> = None;

pub(crate) const ID_AUDIT_LIST: i32 = 301;
pub(crate) const ID_AUDIT_DETAIL: i32 = 302;
pub(crate) const ID_AUDIT_STATUS: i32 = 303;
pub(crate) const ID_AUDIT_COPY_SEL: i32 = 304;
pub(crate) const ID_AUDIT_COPY_ALL: i32 = 305;
pub(crate) const ID_AUDIT_EXPORT: i32 = 306;
pub(crate) const ID_AUDIT_REFRESH: i32 = 307;
pub(crate) const ID_AUDIT_CLOSE: i32 = 308;

/// ウィンドウを開いた時点のスナップショット（古い順）。表示中の新規変換は
/// 反映せず「更新」で取り直す（ホットパスからUIへ同期プッシュしない）。
static AUDIT_SNAPSHOT: Mutex<Vec<ConversionTrace>> = Mutex::new(Vec::new());
/// 一覧の各行が指す (トレース番号, 段階番号)。行番号からこれで引く。
static AUDIT_ROWS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

// ---- クリップボード（Win32_System_DataExchange 機能を有効化せずに直接リンク）----
#[link(name = "user32")]
extern "system" {
    fn OpenClipboard(hwnd_new_owner: *mut core::ffi::c_void) -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn CloseClipboard() -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> *mut core::ffi::c_void;
    fn GlobalLock(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn GlobalUnlock(hmem: *mut core::ffi::c_void) -> i32;
    fn GlobalFree(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
}
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

/// プレーンテキストをクリップボードへ設定する。成功なら true。
pub(crate) unsafe fn set_clipboard_text(owner: HWND, text: &str) -> bool {
    let wide = to_wide(text);
    let bytes = wide.len() * std::mem::size_of::<u16>();
    let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
    if hmem.is_null() {
        return false;
    }
    let dst = GlobalLock(hmem);
    if dst.is_null() {
        GlobalFree(hmem);
        return false;
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, dst as *mut u8, bytes);
    GlobalUnlock(hmem);
    if OpenClipboard(owner.0) == 0 {
        GlobalFree(hmem);
        return false;
    }
    EmptyClipboard();
    let ok = !SetClipboardData(CF_UNICODETEXT, hmem).is_null();
    CloseClipboard();
    if !ok {
        GlobalFree(hmem);
    }
    ok
}

/// エクスポート先ディレクトリ（`%APPDATA%\ime-live-converter`）
pub(crate) fn audit_export_dir() -> std::path::PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("ime-live-converter")
}

/// 全トレースを JSON でエクスポートし、保存先パスを返す。
pub(crate) fn export_traces_json(traces: &[ConversionTrace]) -> std::io::Result<std::path::PathBuf> {
    let dir = audit_export_dir();
    std::fs::create_dir_all(&dir)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("audit-{}.json", millis));
    std::fs::write(&path, common::audit_trace::traces_to_json(traces))?;
    Ok(path)
}

/// 全トレースのコピー用テキスト（古い順）
pub(crate) fn all_traces_report(traces: &[ConversionTrace]) -> String {
    let mut out = String::new();
    for (i, t) in traces.iter().enumerate() {
        out.push_str(&format!("[変換 #{}]\n", i + 1));
        out.push_str(&t.to_report_text());
    }
    out
}

/// 選択ステップのコピー用テキスト（どの変換の何番目かを含める）
pub(crate) fn selected_step_report(traces: &[ConversionTrace], trace_idx: usize, stage_idx: usize) -> Option<String> {
    let t = traces.get(trace_idx)?;
    let s = t.stages.get(stage_idx)?;
    Some(format!(
        "[変換 #{} / started_at_unix_millis={}]\n{}\n",
        trace_idx + 1,
        t.started_at_unix_millis,
        s.to_report_line(stage_idx + 1)
    ))
}

/// `LIVE_CONTEXT` からトレースのスナップショットを取り、一覧へ反映する。
pub(crate) unsafe fn audit_refresh_list(parent: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    let Ok(list) = GetDlgItem(parent, ID_AUDIT_LIST) else { return };

    let traces: Vec<ConversionTrace> = LIVE_CONTEXT
        .get()
        .and_then(|m| m.lock().ok().map(|c| c.trace_buffer.snapshot()))
        .unwrap_or_default();

    SendMessageW(list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));
    let mut rows: Vec<(usize, usize)> = Vec::new();
    // 最新の変換を先頭に出す（↓で過去へ遡る）
    for (ti, t) in traces.iter().enumerate().rev() {
        for (si, s) in t.stages.iter().enumerate() {
            let row = rows.len() as i32;
            lv_insert_row(list, row, &format!("#{}.{}", ti + 1, si + 1));
            lv_set_sub(list, row, 1, s.stage.label());
            lv_set_sub(list, row, 2, &s.input);
            lv_set_sub(list, row, 3, &s.output);
            lv_set_sub(list, row, 4, &s.duration_micros.to_string());
            lv_set_sub(list, row, 5, s.applied_rule.as_deref().unwrap_or("-"));
            rows.push((ti, si));
        }
    }

    let status = if !audit_log_enabled() {
        "収集無効（IME_AUDIT_LOG=0）。環境変数を外して再起動すると収集されます。".to_string()
    } else if traces.is_empty() {
        "トレースはまだありません。日本語モードで入力すると記録されます。".to_string()
    } else {
        format!(
            "トレース {} 件（最新が先頭）。↑↓で段階を移動、ボタンでコピー/エクスポート。",
            traces.len()
        )
    };
    settings_set_text(parent, ID_AUDIT_STATUS, &status);
    if traces.is_empty() {
        settings_set_text(parent, ID_AUDIT_DETAIL, "");
    }

    if let Ok(mut s) = AUDIT_SNAPSHOT.lock() {
        *s = traces;
    }
    if let Ok(mut r) = AUDIT_ROWS.lock() {
        *r = rows;
    }
    if !AUDIT_ROWS.lock().map(|r| r.is_empty()).unwrap_or(true) {
        lv_select_row(list, 0);
    }
}

/// 選択行の (トレース番号, 段階番号)
unsafe fn audit_selected(parent: HWND) -> Option<(usize, usize)> {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    let list = GetDlgItem(parent, ID_AUDIT_LIST).ok()?;
    let row = lv_selected(list)?;
    AUDIT_ROWS.lock().ok()?.get(row).copied()
}

/// 選択行の全文を詳細欄へ出す（スクリーンリーダーが長い入出力も読めるように）
unsafe fn audit_show_detail(parent: HWND) {
    let Some((ti, si)) = audit_selected(parent) else { return };
    let text = AUDIT_SNAPSHOT
        .lock()
        .ok()
        .and_then(|t| selected_step_report(&t, ti, si))
        .unwrap_or_default()
        .replace('\n', "\r\n");
    settings_set_text(parent, ID_AUDIT_DETAIL, &text);
}

pub(crate) extern "system" fn audit_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, ShowWindow, HMENU, SW_HIDE, WM_CLOSE, WM_COMMAND, WM_CREATE,
        WINDOW_EX_STYLE, WINDOW_STYLE, WS_BORDER, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
    };
    unsafe {
        match msg {
            WM_CREATE => {
                let hinst = GetModuleHandleW(None).unwrap_or_default();
                let font = CreateFontW(
                    -15, 0, 0, 0, 400, 0, 0, 0,
                    1, 0, 0, 5, 0,
                    w!("Meiryo UI"),
                );
                let mk = |class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: i32| {
                    let cw = to_wide(class);
                    let tw = to_wide(text);
                    if let Ok(ctrl) = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        windows::core::PCWSTR(cw.as_ptr()),
                        windows::core::PCWSTR(tw.as_ptr()),
                        WINDOW_STYLE(style),
                        x, y, w, h,
                        hwnd,
                        HMENU(id as isize as *mut core::ffi::c_void),
                        hinst,
                        None,
                    ) {
                        SendMessageW(ctrl, 0x0030, WPARAM(font.0 as usize), LPARAM(1)); // WM_SETFONT
                    }
                };
                {
                    use windows::Win32::UI::Controls::{
                        InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX,
                    };
                    let icc = INITCOMMONCONTROLSEX {
                        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                        dwICC: ICC_LISTVIEW_CLASSES,
                    };
                    let _ = InitCommonControlsEx(&icc);
                }
                let vis = WS_VISIBLE.0 | WS_CHILD.0;
                let btn = vis | WS_TABSTOP.0;

                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let cw = rc.right;
                let ch = rc.bottom;
                const M: i32 = 14;
                let il = M;
                let ir = cw - M;
                let iw = ir - il;

                mk("STATIC", "", vis, il, 10, iw, 22, ID_AUDIT_STATUS);

                let lvy = 40;
                let detail_h = 110;
                let btn_h = 30;
                let lvh = ch - lvy - detail_h - btn_h - 3 * 10;
                let list_style = vis | WS_BORDER.0 | WS_TABSTOP.0
                    | 0x0001 /*LVS_REPORT*/ | 0x0004 /*LVS_SINGLESEL*/ | 0x0008 /*LVS_SHOWSELALWAYS*/;
                let lvcls = to_wide("SysListView32");
                if let Ok(list) = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    windows::core::PCWSTR(lvcls.as_ptr()),
                    windows::core::PCWSTR(to_wide("").as_ptr()),
                    WINDOW_STYLE(list_style),
                    il, lvy, iw, lvh,
                    hwnd,
                    HMENU(ID_AUDIT_LIST as isize as *mut core::ffi::c_void),
                    hinst,
                    None,
                ) {
                    SendMessageW(list, 0x0030, WPARAM(font.0 as usize), LPARAM(1));
                    SendMessageW(
                        list,
                        LVM_SETEXTENDEDLISTVIEWSTYLE,
                        WPARAM(0),
                        LPARAM(LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES),
                    );
                    let avail = iw - 22;
                    lv_insert_column(list, 0, "Step#", avail * 8 / 100);
                    lv_insert_column(list, 1, "段階", avail * 12 / 100);
                    lv_insert_column(list, 2, "入力", avail * 24 / 100);
                    lv_insert_column(list, 3, "出力", avail * 24 / 100);
                    lv_insert_column(list, 4, "所要時間(µs)", avail * 12 / 100);
                    lv_insert_column(list, 5, "適用ルール", avail * 20 / 100);
                }
                let detail_style = vis | WS_BORDER.0 | WS_TABSTOP.0
                    | 0x0004 /*ES_MULTILINE*/ | 0x0800 /*ES_READONLY*/
                    | 0x0040 /*ES_AUTOVSCROLL*/ | 0x0020_0000 /*WS_VSCROLL*/;
                let dy = lvy + lvh + 10;
                mk("EDIT", "", detail_style, il, dy, iw, detail_h, ID_AUDIT_DETAIL);

                let by = dy + detail_h + 10;
                let bw = 150;
                let gap = 8;
                mk("BUTTON", "選択ステップをコピー", btn, il, by, bw, btn_h, ID_AUDIT_COPY_SEL);
                mk("BUTTON", "全ステップをコピー", btn, il + bw + gap, by, bw, btn_h, ID_AUDIT_COPY_ALL);
                mk("BUTTON", "JSONエクスポート", btn, il + 2 * (bw + gap), by, bw, btn_h, ID_AUDIT_EXPORT);
                mk("BUTTON", "更新", btn, ir - 84 - gap - 84, by, 84, btn_h, ID_AUDIT_REFRESH);
                mk("BUTTON", "閉じる", btn, ir - 84, by, 84, btn_h, ID_AUDIT_CLOSE);
                audit_refresh_list(hwnd);
                LRESULT(0)
            }
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const windows::Win32::UI::Controls::NMHDR;
                if !nmhdr.is_null() && (*nmhdr).idFrom == ID_AUDIT_LIST as usize {
                    let nmlv = lparam.0 as *const windows::Win32::UI::Controls::NMLISTVIEW;
                    if (*nmhdr).code == LVN_ITEMCHANGED && !nmlv.is_null() {
                        let ns = (*nmlv).uNewState;
                        let os = (*nmlv).uOldState;
                        if (ns & LVIS_SELECTED) != 0 && (os & LVIS_SELECTED) == 0 {
                            audit_show_detail(hwnd);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    ID_AUDIT_COPY_SEL => {
                        let text = audit_selected(hwnd).and_then(|(ti, si)| {
                            AUDIT_SNAPSHOT.lock().ok().and_then(|t| selected_step_report(&t, ti, si))
                        });
                        let status = match text {
                            Some(t) if set_clipboard_text(hwnd, &t) => "選択ステップをクリップボードへコピーしました。",
                            Some(_) => "クリップボードへのコピーに失敗しました。",
                            None => "コピーするステップを選択してください。",
                        };
                        settings_set_text(hwnd, ID_AUDIT_STATUS, status);
                        LRESULT(0)
                    }
                    ID_AUDIT_COPY_ALL => {
                        let text = AUDIT_SNAPSHOT.lock().ok().map(|t| all_traces_report(&t)).unwrap_or_default();
                        let status = if text.is_empty() {
                            "コピーするトレースがありません。"
                        } else if set_clipboard_text(hwnd, &text) {
                            "全ステップをクリップボードへコピーしました。"
                        } else {
                            "クリップボードへのコピーに失敗しました。"
                        };
                        settings_set_text(hwnd, ID_AUDIT_STATUS, status);
                        LRESULT(0)
                    }
                    ID_AUDIT_EXPORT => {
                        let traces = AUDIT_SNAPSHOT.lock().ok().map(|t| t.clone()).unwrap_or_default();
                        let status = if traces.is_empty() {
                            "エクスポートするトレースがありません。".to_string()
                        } else {
                            match export_traces_json(&traces) {
                                Ok(p) => format!("エクスポートしました: {}", p.display()),
                                Err(e) => format!("エクスポートに失敗しました: {}", e),
                            }
                        };
                        settings_set_text(hwnd, ID_AUDIT_STATUS, &status);
                        LRESULT(0)
                    }
                    ID_AUDIT_REFRESH => {
                        audit_refresh_list(hwnd);
                        LRESULT(0)
                    }
                    ID_AUDIT_CLOSE => {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        LRESULT(0)
                    }
                    _ => LRESULT(0),
                }
            }
            WM_CLOSE => {
                // 破棄せず隠す（次回すぐ開けるように）
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 監査ログウィンドウを開く（なければ作る）。ホットキー Ctrl+Alt+L から呼ぶ。
pub(crate) unsafe fn open_audit_log_window() {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, GetDlgItem, RegisterClassW, ShowWindow, SW_SHOW,
        WNDCLASSW, WS_CAPTION, WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
    };
    if let Some(pop) = CANDIDATE_HWND {
        let _ = SetWindowPos(
            pop, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    if let Some(hwnd) = AUDIT_HWND {
        let _ = ShowWindow(hwnd, SW_SHOW);
        audit_refresh_list(hwnd);
        audit_force_foreground(hwnd, GetDlgItem(hwnd, ID_AUDIT_LIST).ok());
        return;
    }
    let Ok(hinst) = GetModuleHandleW(None) else { return };
    let class_name = w!("ImeAuditLog");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(audit_wndproc),
        hInstance: hinst.into(),
        lpszClassName: class_name,
        hCursor: windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
            None,
            windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
        )
        .unwrap_or_default(),
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH((16 + 1) as *mut core::ffi::c_void),
        ..Default::default()
    };
    RegisterClassW(&wc);
    let title = w!("変換監査ログ（段階別トレース）");
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST,
        class_name,
        title,
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        120, 40, 900, 640,
        None,
        None,
        hinst,
        None,
    ) else {
        return;
    };
    AUDIT_HWND = Some(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOW);
    audit_force_foreground(hwnd, GetDlgItem(hwnd, ID_AUDIT_LIST).ok());
}

/// フックスレッドからの SetForegroundWindow は拒否されるため、前面ウィンドウの
/// スレッド入力キューに一時アタッチして遷移・フォーカスさせる（settings_ui と同じ定石）。
unsafe fn audit_force_foreground(hwnd: HWND, focus_ctrl: Option<HWND>) {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

    let fg = GetForegroundWindow();
    let fg_thread = if fg.0.is_null() { 0 } else { GetWindowThreadProcessId(fg, None) };
    let cur_thread = GetCurrentThreadId();
    let attached = fg_thread != 0
        && fg_thread != cur_thread
        && AttachThreadInput(cur_thread, fg_thread, true).as_bool();
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(focus_ctrl.unwrap_or(hwnd));
    if attached {
        let _ = AttachThreadInput(cur_thread, fg_thread, false);
    }
}

/// フォアグラウンドが監査ログウィンドウ（またはその子）か。
/// その間フックは何もしない（↑↓やTabを横取りして遡行を壊さないため）。
pub(crate) unsafe fn audit_window_is_foreground() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetAncestor;
    use windows::Win32::UI::WindowsAndMessaging::GA_ROOT;
    let Some(hwnd) = AUDIT_HWND else { return false };
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    fg == hwnd || GetAncestor(fg, GA_ROOT) == hwnd
}

/// 監査ログウィンドウ宛のメッセージを IsDialogMessage で処理する
/// （Tab でのコントロール間移動・Esc で閉じる等を有効にする）。
pub(crate) unsafe fn try_handle_audit_dialog_message(msg: &windows::Win32::UI::WindowsAndMessaging::MSG) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{IsDialogMessageW, MSG};
    match AUDIT_HWND {
        Some(hwnd) => IsDialogMessageW(hwnd, msg as *const MSG).as_bool(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{StageKind, StageRecord};

    fn trace(tag: &str) -> ConversionTrace {
        let mut t = ConversionTrace::new(std::time::SystemTime::UNIX_EPOCH);
        for (i, k) in [StageKind::Input, StageKind::Preprocess, StageKind::EngineSelect, StageKind::Output]
            .into_iter()
            .enumerate()
        {
            t.stages.push(StageRecord {
                stage: k,
                input: format!("{}-in{}", tag, i),
                output: format!("{}-out{}", tag, i),
                duration_micros: i as u64,
                applied_rule: None,
            });
        }
        t
    }

    #[test]
    fn selected_step_report_identifies_trace_and_step() {
        let traces = vec![trace("a"), trace("b")];
        let text = selected_step_report(&traces, 1, 2).unwrap();
        assert!(text.contains("[変換 #2"));
        assert!(text.contains("Step3 [エンジン選定]"));
        assert!(text.contains("b-in2"));
        assert!(selected_step_report(&traces, 2, 0).is_none());
        assert!(selected_step_report(&traces, 0, 4).is_none());
    }

    #[test]
    fn all_traces_report_lists_every_conversion() {
        let text = all_traces_report(&[trace("a"), trace("b")]);
        assert!(text.contains("[変換 #1]"));
        assert!(text.contains("[変換 #2]"));
        assert_eq!(text.matches("Step1 [入力]").count(), 2);
    }

    #[test]
    fn export_writes_reparsable_json_under_export_dir() {
        let traces = vec![trace("x")];
        let path = export_traces_json(&traces).expect("export succeeds");
        assert!(path.starts_with(audit_export_dir()));
        assert!(path.extension().map(|e| e == "json").unwrap_or(false));
        let body = std::fs::read_to_string(&path).unwrap();
        let back = common::audit_trace::traces_from_json(&body).unwrap();
        assert_eq!(back, traces);
        let _ = std::fs::remove_file(&path);
    }
}
