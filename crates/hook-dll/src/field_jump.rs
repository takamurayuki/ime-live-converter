//! 入力欄ワープ（Ctrl+無変換 で次の入力欄へ、Ctrl+Shift+無変換 で前の入力欄へ）
//!
//! 画面に見えているウィンドウ（前面だけでなく、並べて表示している他の
//! ウィンドウも）から「文字を打てる場所」（Edit、書き換え可能な Document、
//! ターミナルのペイン）を UI Automation で集め、ウィンドウ単位で左→右、
//! 各ウィンドウの中は上→下（同じ行なら左→右）に並べて、今フォーカスが
//! ある欄の次／前へフォーカスを移す。別のウィンドウの欄へ移るときは、
//! そのウィンドウを前面に出してから欄にフォーカスを移す。
//!
//! 設計上の制約:
//! - UIA の木の走査はクロスプロセスの同期 COM 呼び出しで、Chrome 等では
//!   数百ms以上かかりうる。フックは300ms以内に返さないと生キーが漏れる
//!   （[[hook-latency-limit]]）ので、フックは要求をチャネルに積むだけにして、
//!   走査とフォーカス移動はこの専用スレッドで行う。UIAポーラー
//!   （`uia::start_uia_poller`）に相乗りしないのは、ポーラーのパスワード欄
//!   判定の周期を走査の重さで遅らせないため。
//! - 移動はクリックの合成ではなく UIA の `SetFocus` で行う。ホットキーを
//!   押した瞬間は Ctrl が物理的に押されたままなので、クリックを合成すると
//!   Ctrl+クリック（リンクを新しいタブで開く・複数選択）に化ける
//!   （[[synthetic-keys-inherit-held-modifier]]）。
//!
//! 試作段階: 見つかった欄の一覧と所要時間を hook_debug.log に残す
//! （IME_DEBUG_LOG=1）。どのアプリで何個見つかるかを見て条件を詰める。

use crate::*;
use std::sync::mpsc::{Receiver, Sender};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::Accessibility::*;

/// フックからワーカーへの要求。true = 前へ（Shift併用）
static JUMP_TX: OnceLock<Mutex<Sender<bool>>> = OnceLock::new();

/// 一覧を使い回す時間（ms）。連打のたびに木を走査し直すと遅いので、同じ窓で
/// この時間内に次の要求が来たら前回の一覧を使う（押すたびに延長）。
const CACHE_TTL_MS: u64 = 1500;

/// 同じ「行」とみなす上端のずれ（px）。横並びの欄を左→右で巡回するため。
const ROW_TOLERANCE_PX: i32 = 8;

/// フックから呼ぶ: 次（`backward=false`）／前の入力欄への移動を依頼する。
/// チャネルに積むだけで待たない。
pub(crate) fn request_jump(backward: bool) {
    if let Some(tx) = JUMP_TX.get() {
        if let Ok(tx) = tx.try_lock() {
            let _ = tx.send(backward);
        }
    }
}

/// ワーカースレッドを起動する（`install_hook`から1回だけ）
pub(crate) fn start_field_jump_worker() {
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    if JUMP_TX.set(Mutex::new(tx)).is_err() {
        return; // 起動済み
    }
    std::thread::spawn(move || unsafe { worker(rx) });
}

struct Field {
    elem: IUIAutomationElement,
    rect: RECT,
    /// この欄があるトップレベルウィンドウ
    hwnd: isize,
}

struct Cache {
    fields: Vec<Field>,
    /// 最後に移動した先の位置（フォーカス照合に失敗したときの起点）
    last_index: Option<usize>,
    used_at: u64,
}

unsafe fn worker(rx: Receiver<bool>) {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let auto: IUIAutomation = match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
        Ok(a) => a,
        Err(e) => {
            debug_log!("入力欄ワープ: UIA初期化失敗 {:?}", e);
            return;
        }
    };
    let mut cache: Option<Cache> = None;
    while let Ok(backward) = rx.recv() {
        jump_once(&auto, &mut cache, backward);
    }
}

fn now_ms() -> u64 {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

unsafe fn jump_once(auto: &IUIAutomation, cache: &mut Option<Cache>, backward: bool) {
    // 連打中（前回からCACHE_TTL_MS以内）は一覧を使い回す。別のウィンドウへ
    // 移ると前面ウィンドウが変わるので、前面ウィンドウは鍵にしない。
    let fresh = matches!(cache, Some(c) if now_ms().saturating_sub(c.used_at) < CACHE_TTL_MS);
    if !fresh {
        let t0 = std::time::Instant::now();
        let fields = collect_all_fields(auto);
        debug_log!("入力欄ワープ: 走査 計{}件 {}ms", fields.len(), t0.elapsed().as_millis());
        *cache = Some(Cache { fields, last_index: None, used_at: now_ms() });
    }
    let Some(c) = cache.as_mut() else { return };
    c.used_at = now_ms();
    if c.fields.is_empty() {
        flash_at("入力欄なし", None);
        return;
    }

    let n = c.fields.len();
    let current = current_index(auto, &c.fields).or(c.last_index);
    let target = match current {
        Some(i) if backward => (i + n - 1) % n,
        Some(i) => (i + 1) % n,
        None => start_index_from_focus(auto, &c.fields, backward),
    };

    let target_hwnd = HWND(c.fields[target].hwnd as *mut core::ffi::c_void);
    let activated = activate_window(target_hwnd);
    let mut ok = c.fields[target].elem.SetFocus().is_ok();
    // SetFocus が成功を返しても、フォーカスが着地しないことがある（開いた
    // 直後の Windows Terminal で分割ペインのどちらにもフォーカスが無い状態に
    // なったのを観測。2026-11-14）。少し待って確かめ、外れていたら1回だけやり直す。
    if ok {
        std::thread::sleep(std::time::Duration::from_millis(80));
        if current_index(auto, &c.fields) != Some(target) {
            debug_log!("入力欄ワープ: フォーカスが着地しなかったので再試行");
            ok = c.fields[target].elem.SetFocus().is_ok();
        }
    }
    debug_log!(
        "入力欄ワープ: {} {:?} → {} ({}/{}) 前面化={} SetFocus={}",
        if backward { "前へ" } else { "次へ" },
        current,
        target,
        target + 1,
        n,
        activated,
        ok
    );
    if !ok {
        // 古い一覧（欄が消えた等）の可能性があるので次回は走査し直す
        *cache = None;
        flash_at("移動できませんでした", None);
        return;
    }
    c.last_index = Some(target);
    // 移った先の入力欄の文章を判断層の文脈として読み直してもらう
    request_field_text();
    // 表示はカーソルのある行に出す。ターミナルのペインや大きな文書欄では
    // 欄の上端に出すと入力している行から遠く離れてしまうため（実機報告
    // 2026-11-14）。カーソルが取れない・欄の外を指すときは欄の上端に出す。
    let r = c.fields[target].rect;
    let caret = crate::uia::uia_caret_from_textpattern(&c.fields[target].elem)
        .filter(|&(x, top, bottom)| caret_inside(r, x, top, bottom));
    debug_log!("入力欄ワープ: 表示位置 カーソル={:?} 欄=({},{})-({},{})", caret, r.left, r.top, r.right, r.bottom);
    let label = format!("入力欄 {}/{}", target + 1, n);
    match caret {
        Some(pos) => post_ui_command(UiCommand::FlashModeIndicator { label, caret: pos }),
        None => flash_at(&label, Some(r)),
    }
}

/// カーソル位置 (x, 上端y, 下端y) が欄の中（多少のはみ出しは許す）にあるか
fn caret_inside(r: RECT, x: i32, top: i32, bottom: i32) -> bool {
    const SLACK: i32 = 4;
    bottom > top
        && x >= r.left - SLACK
        && x <= r.right + SLACK
        && top >= r.top - SLACK
        && bottom <= r.bottom + SLACK
}

/// ウィンドウを前面に出す。Windows は他のアプリによる前面の奪取を制限して
/// いる（ForegroundLockTimeout）ので、効かなければ順に別の手を試す。
/// 戻り値はどの手で前面に出たか（ログ用）。ターミナルから起動した
/// PowerShell では素の SetForegroundWindow で通ったが、それは起動元の
/// 特権を引き継いでいたためと考えられ、常駐プロセスでどれが効くかは
/// 実機のログで確かめる（2026-11-14 時点で未確認）。
unsafe fn activate_window(hwnd: HWND) -> &'static str {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_F24,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetWindowThreadProcessId, SetForegroundWindow,
    };
    if GetForegroundWindow() == hwnd {
        return "不要";
    }
    let _ = SetForegroundWindow(hwnd);
    if wait_foreground(hwnd) {
        return "SetForegroundWindow";
    }
    // 「直前の入力を受けたプロセスは前面を変えてよい」という規則を使う。
    // 押されていないキー（F24）の「離す」だけを送る（Ctrl を押したままでも
    // アプリ側で何も起きない）。自己送信マーカー付きなのでフックは無視する。
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_F24,
                wScan: 0,
                dwFlags: KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: SELF_INJECTED_MAGIC,
            },
        },
    };
    SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    let _ = SetForegroundWindow(hwnd);
    if wait_foreground(hwnd) {
        return "keyup+SetForegroundWindow";
    }
    // 今の前面ウィンドウの入力キューに一時的に相乗りして前面を譲ってもらう
    let fg = GetForegroundWindow();
    let fg_thread = GetWindowThreadProcessId(fg, None);
    let me = GetCurrentThreadId();
    if fg_thread != 0 && fg_thread != me {
        let _ = AttachThreadInput(me, fg_thread, true);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = AttachThreadInput(me, fg_thread, false);
        if wait_foreground(hwnd) {
            return "AttachThreadInput";
        }
    }
    "失敗"
}

/// 前面になるまで最大50ms待つ
unsafe fn wait_foreground(hwnd: HWND) -> bool {
    for _ in 0..5 {
        if GetForegroundWindow() == hwnd {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    GetForegroundWindow() == hwnd
}

/// 画面に見えている各ウィンドウの入力欄を集め、ウィンドウ単位で左→右、
/// ウィンドウ内は位置順に並べる
unsafe fn collect_all_fields(auto: &IUIAutomation) -> Vec<Field> {
    let mut windows = visible_top_level_windows();
    windows.sort_by_key(|(_, r)| (r.left, r.top));
    let mut all = Vec::new();
    for (hwnd, _) in windows {
        let t0 = std::time::Instant::now();
        let fields = collect_fields(auto, hwnd);
        debug_log!(
            "入力欄ワープ: 窓 class='{}' {}件 {}ms",
            class_name_of(hwnd),
            fields.len(),
            t0.elapsed().as_millis()
        );
        all.extend(fields);
    }
    all
}

/// 画面に少しでも見えているトップレベルウィンドウ。
/// 最小化・非表示・クローク（別の仮想デスクトップ、裏で待機中のUWP等）・
/// ツールウィンドウ・デスクトップとタスクバー・自分自身のウィンドウは除く。
/// 他のウィンドウに完全に隠れているものも、走査が無駄なので除く。
unsafe fn visible_top_level_windows() -> Vec<(HWND, RECT)> {
    use windows::Win32::Foundation::{BOOL, LPARAM, TRUE};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };
    unsafe extern "system" fn push(h: HWND, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<HWND>);
        v.push(h);
        TRUE
    }
    let mut all: Vec<HWND> = Vec::new();
    let _ = EnumWindows(Some(push), LPARAM(&mut all as *mut Vec<HWND> as isize));
    let me = GetCurrentProcessId();
    let mut out = Vec::new();
    for h in all {
        if !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() || is_cloaked(h) {
            continue;
        }
        if GetWindowLongPtrW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            continue;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == me {
            continue;
        }
        if matches!(
            class_name_of(h).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            continue;
        }
        let mut r = RECT::default();
        if GetWindowRect(h, &mut r).is_err() || r.right - r.left < 50 || r.bottom - r.top < 50 {
            continue;
        }
        if !window_partly_visible(h, r) {
            continue;
        }
        out.push((h, r));
    }
    out
}

unsafe fn is_cloaked(h: HWND) -> bool {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    let mut cloaked = 0u32;
    DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut u32 as *mut core::ffi::c_void, 4).is_ok()
        && cloaked != 0
}

fn class_name_of(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// 画面上のこの点に見えているのが、トップレベルウィンドウ`h`（の子孫）か
unsafe fn point_shows_window(h: HWND, x: i32, y: i32) -> bool {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, WindowFromPoint, GA_ROOT};
    // どのモニターにも載っていない点は見えていない（WindowFromPoint は画面外の
    // 座標でも窓を返すことがある。y=-10000 に退避している Tauri の窓で観測）
    if MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL).is_invalid() {
        return false;
    }
    let w = WindowFromPoint(POINT { x, y });
    !w.0.is_null() && GetAncestor(w, GA_ROOT) == h
}

/// ウィンドウ内の3×3の点のどれかで、そのウィンドウが見えているか
unsafe fn window_partly_visible(h: HWND, r: RECT) -> bool {
    let (w, ht) = (r.right - r.left, r.bottom - r.top);
    (1..=3).any(|i| (1..=3).any(|j| point_shows_window(h, r.left + w * i / 4, r.top + ht * j / 4)))
}

/// 移動先の欄の左上付近に「入力欄 2/5」のような表示を一瞬出す
fn flash_at(label: &str, rect: Option<RECT>) {
    let caret = match rect {
        // 縦に大きい文書領域でも欄の上端付近に出るよう、下端は上端+30pxで頭打ち
        Some(r) => (r.left, r.top, r.top + (r.bottom - r.top).clamp(1, 30)),
        None => caret_screen_pos(),
    };
    post_ui_command(UiCommand::FlashModeIndicator { label: label.to_string(), caret });
}

/// ウィンドウ`hwnd`内の入力欄を集め、画面上の位置順に並べる
unsafe fn collect_fields(auto: &IUIAutomation, hwnd: HWND) -> Vec<Field> {
    let Ok(root) = auto.ElementFromHandle(hwnd) else {
        return Vec::new();
    };
    let Some(cond) = build_condition(auto) else {
        return Vec::new();
    };
    let Ok(req) = auto.CreateCacheRequest() else {
        return Vec::new();
    };
    for p in [
        UIA_ControlTypePropertyId,
        UIA_ClassNamePropertyId,
        UIA_BoundingRectanglePropertyId,
        UIA_NamePropertyId,
        UIA_IsPasswordPropertyId,
        UIA_IsValuePatternAvailablePropertyId,
        UIA_ValueIsReadOnlyPropertyId,
    ] {
        let _ = req.AddProperty(p);
    }
    let Ok(arr) = root.FindAllBuildCache(TreeScope_Descendants, &cond, &req) else {
        return Vec::new();
    };
    let len = arr.Length().unwrap_or(0);
    let mut fields = Vec::new();
    for i in 0..len {
        let Ok(e) = arr.GetElement(i) else { continue };
        let ct = e.CachedControlType().map(|c| c.0).unwrap_or(0);
        let class = e.CachedClassName().map(|b| b.to_string()).unwrap_or_default();
        let rect = e.CachedBoundingRectangle().unwrap_or_default();
        let name = e.CachedName().map(|b| b.to_string()).unwrap_or_default();
        let is_pw = e.CachedIsPassword().map(|b| b.as_bool()).unwrap_or(false);
        let value_avail = cached_bool(&e, UIA_IsValuePatternAvailablePropertyId);
        let read_only = cached_bool(&e, UIA_ValueIsReadOnlyPropertyId);
        let kind = classify(ct, &class);
        // 採用条件:
        // - 大きさのある欄だけ（見えない・潰れた要素は除く）
        // - 読み取り専用と明示されたものは除く
        // - Document は「書き換えられる」と分かるものだけ。Chrome 等は
        //   Webページ全体が Document として出るので、それを拾うと
        //   ページ本体が「入力欄」扱いになってしまう。
        // - ターミナルのペインは常に採用。VS Code 等の xterm.js は、カーソル
        //   位置に置かれた小さな（幅0のこともある）textarea が入力を受けるので、
        //   大きさの条件を掛けない。
        // パスワード欄は移動先として残す（移るだけで打鍵は扱わない。
        // 移った後はフックがパスワード欄として素通しする）。
        let has_size = rect.right - rect.left >= 4 && rect.bottom - rect.top >= 4;
        // 他のウィンドウに隠れている欄は除く。欄の縦の中央で、横に1/4・1/2・3/4
        // の3点のどれかにこのウィンドウが見えていれば残す（横長の欄が半分だけ
        // 隠れている場合も拾うため）。画面外の欄もここで落ちる。
        let cy = (rect.top + rect.bottom) / 2;
        let w = rect.right - rect.left;
        let visible = [1, 2, 3].iter().any(|k| point_shows_window(hwnd, rect.left + w * k / 4, cy));
        let accepted = visible && match kind {
            // 矩形が全く取れない（全部0）ものだけ除く
            FieldKind::Terminal => rect != RECT::default(),
            FieldKind::Edit => has_size && read_only != Some(true),
            FieldKind::Document => has_size && value_avail == Some(true) && read_only == Some(false),
        };
        debug_log!(
            "入力欄ワープ:  {} {:?} class='{}' name='{}' rect=({},{})-({},{}) 見える={} pw={} value={:?} ro={:?}",
            if accepted { "○" } else { "×" },
            kind,
            class,
            name.chars().take(30).collect::<String>(),
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            visible,
            is_pw,
            value_avail,
            read_only
        );
        if accepted {
            fields.push(Field { elem: e, rect, hwnd: hwnd.0 as isize });
        }
    }
    sort_by_position(&mut fields, |f| f.rect);
    fields
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Edit,
    Document,
    /// ターミナルのペイン（Windows Terminal の分割ペイン、VS Code の統合ターミナル）
    Terminal,
}

/// Windows Terminal はペインごとに ClassName="TermControl"（ControlType は Text）の
/// 要素を出す（2026-11-14 実測）。非アクティブなタブのペインは木に出ないので、
/// 今見えているタブの分割ペインだけが巡回対象になる。
const WT_TERM_CONTROL_CLASS: &str = "TermControl";

fn classify(control_type: i32, class: &str) -> FieldKind {
    if class == WT_TERM_CONTROL_CLASS || class.to_lowercase().contains("xterm-helper-textarea") {
        FieldKind::Terminal
    } else if control_type == UIA_EditControlTypeId.0 {
        FieldKind::Edit
    } else {
        FieldKind::Document
    }
}

/// ((ControlType が Edit か Document) または Windows Terminal のペイン)
/// かつ 有効 かつ キーボードで選べる かつ 画面内
unsafe fn build_condition(auto: &IUIAutomation) -> Option<IUIAutomationCondition> {
    use windows::core::VARIANT;
    let edit = auto
        .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_EditControlTypeId.0))
        .ok()?;
    let doc = auto
        .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_DocumentControlTypeId.0))
        .ok()?;
    let kind = auto.CreateOrCondition(&edit, &doc).ok()?;
    let term = auto
        .CreatePropertyCondition(
            UIA_ClassNamePropertyId,
            &VARIANT::from(windows::core::BSTR::from(WT_TERM_CONTROL_CLASS)),
        )
        .ok()?;
    let kind = auto.CreateOrCondition(&kind, &term).ok()?;
    let enabled = auto.CreatePropertyCondition(UIA_IsEnabledPropertyId, &VARIANT::from(true)).ok()?;
    let focusable = auto
        .CreatePropertyCondition(UIA_IsKeyboardFocusablePropertyId, &VARIANT::from(true))
        .ok()?;
    let onscreen = auto.CreatePropertyCondition(UIA_IsOffscreenPropertyId, &VARIANT::from(false)).ok()?;
    let c = auto.CreateAndCondition(&kind, &enabled).ok()?;
    let c = auto.CreateAndCondition(&c, &focusable).ok()?;
    auto.CreateAndCondition(&c, &onscreen).ok()
}

/// キャッシュ済みの真偽値プロパティ。未対応（NotSupported）なら None
unsafe fn cached_bool(e: &IUIAutomationElement, prop: UIA_PROPERTY_ID) -> Option<bool> {
    let v = e.GetCachedPropertyValue(prop).ok()?;
    bool::try_from(&v).ok()
}

/// 今フォーカスがある要素が一覧の何番目か。要素そのものが一覧に無くても
/// （Webの入力欄の中の子要素にフォーカスがある等）、それを内側に含む欄の
/// うち一番小さいものを「今の欄」とみなす。
unsafe fn current_index(auto: &IUIAutomation, fields: &[Field]) -> Option<usize> {
    let focused = auto.GetFocusedElement().ok()?;
    for (i, f) in fields.iter().enumerate() {
        if auto.CompareElements(&focused, &f.elem).map(|b| b.as_bool()).unwrap_or(false) {
            return Some(i);
        }
    }
    let r = focused.CurrentBoundingRectangle().ok()?;
    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    fields
        .iter()
        .enumerate()
        .filter(|(_, f)| contains(f.rect, cx, cy))
        .min_by_key(|(_, f)| area(f.rect))
        .map(|(i, _)| i)
}

/// 今のフォーカスが入力欄の外（ボタン・本文等）にあるとき、前面ウィンドウの
/// 中でその位置から見て次（前）にある欄を選ぶ。前面ウィンドウに欄が無い、
/// または位置が取れなければ、並び順で前面ウィンドウの次（前）のウィンドウの
/// 欄へ進む。
unsafe fn start_index_from_focus(auto: &IUIAutomation, fields: &[Field], backward: bool) -> usize {
    let fg = GetForegroundWindow().0 as isize;
    let focus_rect = auto.GetFocusedElement().ok().and_then(|f| f.CurrentBoundingRectangle().ok());
    let fallback = fallback_index(fields.iter().map(|f| f.hwnd), fg, backward);
    let in_fg: Vec<usize> = (0..fields.len()).filter(|&i| fields[i].hwnd == fg).collect();
    match focus_rect {
        Some(fr) if !in_fg.is_empty() => pick_relative(in_fg.iter().map(|&i| fields[i].rect), fr, backward)
            .map(|k| in_fg[k])
            .unwrap_or(fallback),
        _ => fallback,
    }
}

// ---- 位置計算（UIAに依存しない純粋関数。単体テスト対象） ----

fn area(r: RECT) -> i64 {
    (r.right - r.left) as i64 * (r.bottom - r.top) as i64
}

fn contains(r: RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// 上端が`ROW_TOLERANCE_PX`以内なら同じ行として左→右、違えば上→下の順
fn position_cmp(a: RECT, b: RECT) -> std::cmp::Ordering {
    if (a.top - b.top).abs() <= ROW_TOLERANCE_PX {
        a.left.cmp(&b.left)
    } else {
        a.top.cmp(&b.top)
    }
}

fn sort_by_position<T>(items: &mut [T], rect_of: impl Fn(&T) -> RECT) {
    // position_cmp は行の許容幅のせいで厳密な全順序ではないので、先に
    // (top, left) で安定に並べてから行ごとに左→右へ並べ直す
    items.sort_by_key(|t| {
        let r = rect_of(t);
        (r.top, r.left)
    });
    let mut start = 0;
    while start < items.len() {
        let row_top = rect_of(&items[start]).top;
        let mut end = start + 1;
        while end < items.len() && rect_of(&items[end]).top - row_top <= ROW_TOLERANCE_PX {
            end += 1;
        }
        items[start..end].sort_by_key(|t| rect_of(t).left);
        start = end;
    }
}

/// `focus`の位置から見て、並び順で次（`backward`なら前）にある欄の番号
fn pick_relative(rects: impl Iterator<Item = RECT>, focus: RECT, backward: bool) -> Option<usize> {
    let rects: Vec<RECT> = rects.collect();
    if backward {
        rects.iter().rposition(|r| position_cmp(*r, focus) == std::cmp::Ordering::Less)
    } else {
        rects.iter().position(|r| position_cmp(*r, focus) == std::cmp::Ordering::Greater)
    }
}

/// 前面ウィンドウの欄を通り過ぎた（または無い）ときの行き先。並び順で
/// 前面ウィンドウより後ろ（`backward`なら前）にある最初の欄。前面ウィンドウが
/// 一覧に無ければ先頭（末尾）。
fn fallback_index(hwnds: impl Iterator<Item = isize>, fg: isize, backward: bool) -> usize {
    let hwnds: Vec<isize> = hwnds.collect();
    let n = hwnds.len();
    let first = hwnds.iter().position(|&h| h == fg);
    let last = hwnds.iter().rposition(|&h| h == fg);
    match (first, last) {
        (Some(_), Some(l)) if !backward => (l + 1) % n,
        (Some(f), Some(_)) => (f + n - 1) % n,
        _ if backward => n - 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32) -> RECT {
        RECT { left: l, top: t, right: l + 100, bottom: t + 20 }
    }

    /// 実機の画面で、ワープ先として何が見つかるかを一覧するだけ（フォーカスは
    /// 動かさない）。`cargo test -p hook-dll --lib dump_fields -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_fields_on_this_desktop() {
        unsafe {
            use windows::Win32::System::Com::{
                CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
            };
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let auto: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap();
            for (h, r) in visible_top_level_windows() {
                eprintln!("窓 class='{}' rect=({},{})-({},{})", class_name_of(h), r.left, r.top, r.right, r.bottom);
            }
            let t0 = std::time::Instant::now();
            let fields = collect_all_fields(&auto);
            eprintln!("計 {}件 {}ms", fields.len(), t0.elapsed().as_millis());
            for (i, f) in fields.iter().enumerate() {
                let class = f.elem.CachedClassName().map(|b| b.to_string()).unwrap_or_default();
                let name = f.elem.CachedName().map(|b| b.to_string()).unwrap_or_default();
                eprintln!("    カーソル={:?}", crate::uia::uia_caret_from_textpattern(&f.elem));
                eprintln!(
                    "{:>2}: 窓='{}' class='{}' name='{}' rect=({},{})-({},{})",
                    i + 1,
                    class_name_of(HWND(f.hwnd as *mut core::ffi::c_void)),
                    class,
                    name.chars().take(30).collect::<String>(),
                    f.rect.left,
                    f.rect.top,
                    f.rect.right,
                    f.rect.bottom
                );
            }
        }
    }

    #[test]
    fn falls_back_to_next_or_previous_window() {
        // 窓1の欄2つ、窓2(前面)の欄2つ、窓3の欄1つ
        let hw = [1, 1, 2, 2, 3];
        assert_eq!(fallback_index(hw.iter().copied(), 2, false), 4);
        assert_eq!(fallback_index(hw.iter().copied(), 2, true), 1);
        // 末尾の窓から「次」は先頭へ回り込む
        assert_eq!(fallback_index(hw.iter().copied(), 3, false), 0);
        // 前面が一覧に無い（欄の無いウィンドウ）
        assert_eq!(fallback_index(hw.iter().copied(), 9, false), 0);
        assert_eq!(fallback_index(hw.iter().copied(), 9, true), 4);
    }

    #[test]
    fn caret_must_be_inside_field() {
        let pane = RECT { left: 960, top: 40, right: 1920, bottom: 1080 };
        assert!(caret_inside(pane, 1000, 600, 620));
        // 別のウィンドウや画面外を指す値は使わない
        assert!(!caret_inside(pane, 100, 600, 620));
        assert!(!caret_inside(pane, 1000, -9000, -8980));
        assert!(!caret_inside(pane, 1000, 620, 620));
    }

    #[test]
    fn classifies_terminal_panes() {
        let text = UIA_TextControlTypeId.0;
        let edit = UIA_EditControlTypeId.0;
        assert_eq!(classify(text, "TermControl"), FieldKind::Terminal);
        assert_eq!(classify(edit, "xterm-helper-textarea"), FieldKind::Terminal);
        assert_eq!(classify(edit, ""), FieldKind::Edit);
        assert_eq!(classify(UIA_DocumentControlTypeId.0, ""), FieldKind::Document);
    }

    #[test]
    fn sorts_rows_top_to_bottom_and_left_to_right_within_row() {
        // 横並びの2欄（上端が数pxずれている）と、その下の1欄
        let mut v = vec![r(300, 103), r(10, 200), r(10, 100)];
        sort_by_position(&mut v, |x| *x);
        assert_eq!(v.iter().map(|x| (x.left, x.top)).collect::<Vec<_>>(), vec![(10, 100), (300, 103), (10, 200)]);
    }

    #[test]
    fn picks_next_and_previous_field_relative_to_non_field_focus() {
        let fields = vec![r(10, 100), r(10, 200), r(10, 300)];
        // フォーカスは 2番目と3番目の間にあるボタン
        let button = r(10, 250);
        assert_eq!(pick_relative(fields.iter().copied(), button, false), Some(2));
        assert_eq!(pick_relative(fields.iter().copied(), button, true), Some(1));
        // 全部より下なら「次」は無い（呼び出し側で先頭へ回り込む）
        assert_eq!(pick_relative(fields.iter().copied(), r(10, 900), false), None);
    }
}
