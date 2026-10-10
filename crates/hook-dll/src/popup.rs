//! 候補一覧ポップアップ（かな漢字候補・コマンド候補・モード切替フラッシュの表示）

use crate::*;
use std::sync::mpsc;

/// 候補一覧ウィンドウのハンドル（UIスレッドで生成・操作。フックスレッドから
/// 直接触ってはいけない。`post_ui_command`経由で指示すること）
pub(crate) static mut CANDIDATE_HWND: Option<HWND> = None;

/// 候補一覧ウィンドウの表示内容（WndProc の描画と共有）
pub(crate) struct CandidateUi {
    pub(crate) items: Vec<String>,
    /// コマンドモード時、items と並行するコマンドの簡易説明（無ければ空文字）
    pub(crate) descriptions: Vec<String>,
    /// コマンドモード時、items と並行する「Enterで即実行か」（設定画面のチェックと同じ値）。
    /// 見出しに Enter:実行 / Enter:挿入→編集 を出すために使う。
    pub(crate) auto_runs: Vec<bool>,
    pub(crate) selected: usize,
    pub(crate) visible: bool,
    /// ステータス表示モード（モード切替フラッシュ等の通知。番号を付けず強調表示）
    pub(crate) status: bool,
    /// コマンドモードのコマンド候補表示か（見た目・番号付けを変える）
    pub(crate) command_mode: bool,
}

pub(crate) static CANDIDATE_UI: Mutex<CandidateUi> = Mutex::new(CandidateUi {
    items: Vec::new(),
    descriptions: Vec::new(),
    auto_runs: Vec::new(),
    selected: 0,
    visible: false,
    status: false,
    command_mode: false,
});

/// 候補一覧の1行の高さ（px）
pub(crate) const CANDIDATE_LINE_HEIGHT: i32 = 24;

/// 候補一覧の1ページの件数（既存IMEと同様に9件＝番号キー1〜9に対応）。
/// これを超える候補はページ送り（Tab/Space で次の候補へ進むとページが切り替わる）。
pub(crate) const CANDIDATE_PAGE_SIZE: usize = 9;

/// 選択中インデックスが属するページの先頭インデックスを返す。
pub(crate) fn candidate_page_start(selected: usize) -> usize {
    (selected / CANDIDATE_PAGE_SIZE) * CANDIDATE_PAGE_SIZE
}

// ============ 候補一覧ウィンドウ ============

/// 候補一覧ウィンドウの WndProc
/// 最前面維持タイマーのID
pub(crate) const TOPMOST_TIMER_ID: usize = 1;
/// モード切替フラッシュの自動非表示タイマーのID
pub(crate) const MODE_FLASH_TIMER_ID: usize = 2;
/// UIAポーラー→フックスレッドへの通知: カーソル（フォーカス）がターミナルへ
/// 戻ったので、打鍵を待たずにコマンド候補モーダルを再表示する（WM_APP+1）。
pub(crate) const WM_APP_RESHOW_COMMAND: u32 = 0x8000 + 1;
/// UIAポーラー→フックスレッドへの通知: コマンド候補モーダルを表示していた
/// ターミナル窓が閉じられた（破棄された）ので、打鍵を待たずにモーダルを
/// 閉じる（WM_APP+2）。
pub(crate) const WM_APP_TERMINAL_CLOSED: u32 = 0x8000 + 2;
/// フック→自スレッドのメッセージループへの通知: 確定時に溜めた学習
/// （DB書き込み）を処理する（WM_APP+3）。キーボードフックの中で行うと
/// 応答上限(300ms)を超えて生キーが漏れるため、フックから戻った後に行う。
pub(crate) const WM_APP_FLUSH_LEARNING: u32 = 0x8000 + 3;
/// フックスレッド→UIスレッドへの通知: `UI_COMMAND_RX`に溜まった
/// `UiCommand`をドレインして適用する（WM_APP+5）。候補ウィンドウの
/// 直接操作（`SetWindowPos`/`ShowWindow`等）はフックスレッド分離後、
/// UIスレッド所有のウィンドウへの操作になるため、フックスレッドから
/// 直接呼ぶとクロススレッドのマーシャリングでブロックする。必ずこの
/// チャネル経由で依頼し、実際のWin32呼び出しはUIスレッド自身にさせる。
pub(crate) const WM_APP_UI_COMMAND: u32 = 0x8000 + 5;

/// 候補ウィンドウ関連の指示。フックスレッドから`post_ui_command`で送る。
/// 実行（Win32呼び出し）は必ずUIスレッド側（`drain_ui_commands`）で行う。
///
/// 状態系（Show*/Hide）は「最後の1件だけ」が意味を持つ。動作系
/// （OpenSettings等）は出現順にすべて実行する必要がある。両者を区別
/// せず1本のキューとして順に処理すると、UIスレッドが重い間にフック
/// スレッドが積んだShow→Hide→Show→Hideが全部律儀に実行され、
/// ポップアップがちらつく。`is_state`で区別し、`drain_ui_commands`が
/// 状態系は最後の1件だけ・動作系は全件を適用する。
pub(crate) enum UiCommand {
    ShowCandidateWindow { items: Vec<String>, selected: usize, caret: (i32, i32, i32) },
    ShowCommandPopup {
        items: Vec<String>,
        descs: Vec<String>,
        auto_runs: Vec<bool>,
        selected: usize,
        caret: (i32, i32, i32),
    },
    FlashModeIndicator { label: String, caret: (i32, i32, i32) },
    Hide,
    OpenSettings,
    OpenWordSettings,
}

impl UiCommand {
    fn is_state(&self) -> bool {
        matches!(
            self,
            UiCommand::ShowCandidateWindow { .. }
                | UiCommand::ShowCommandPopup { .. }
                | UiCommand::FlashModeIndicator { .. }
                | UiCommand::Hide
        )
    }
}

static UI_COMMAND_TX: OnceLock<mpsc::Sender<UiCommand>> = OnceLock::new();
static UI_COMMAND_RX: Mutex<Option<mpsc::Receiver<UiCommand>>> = Mutex::new(None);

/// UIスレッド（候補ウィンドウを所有するスレッド）のスレッドID。
/// `init_ui_thread`で一度だけ記録し、`debug_assert_current_thread_is_ui`が
/// 「本当にUIスレッドから呼ばれているか」を機械的に検証するための基準値。
/// レビューと規律だけで「フックスレッドから直接呼ばない」規約を守るのは
/// 破綻するため、実行時アサーションで構造的に強制する。
static UI_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) fn debug_assert_current_thread_is_ui(fn_name: &str) {
    let expected = UI_THREAD_ID.load(std::sync::atomic::Ordering::Acquire);
    let actual = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    debug_assert!(
        expected == 0 || expected == actual,
        "{fn_name} はUIスレッド専用だが、別スレッド（tid={actual}, 期待={expected}）から呼ばれた。\
         候補ウィンドウへの直接操作はpost_ui_command経由にすること。"
    );
}

/// UIスレッド起動時（`install_hook`の呼び出し元スレッド＝`conversion-service`の
/// メインスレッド）に一度だけ呼ぶ。
/// (1) このスレッドをUIスレッドとして記録する。
/// (2) 候補ウィンドウを非表示のまま先に作っておく。フックスレッドからの
///     最初の`post_ui_command`より前にHWNDが存在しないと、`PostMessageW`の
///     宛先が無くコマンドが誰にも引き取られずに残る（起動直後の1回だけ
///     候補が出ない、という再現しにくい形で出る）。
/// (3) コマンドチャネルを準備する。
pub(crate) fn init_ui_thread() {
    UI_THREAD_ID.store(
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
        std::sync::atomic::Ordering::Release,
    );
    unsafe {
        ensure_candidate_window();
    }
    let (tx, rx) = mpsc::channel::<UiCommand>();
    let _ = UI_COMMAND_TX.set(tx);
    if let Ok(mut guard) = UI_COMMAND_RX.lock() {
        *guard = Some(rx);
    }
}

/// フックスレッドから呼ぶ。実際のWin32呼び出しは一切せず、コマンドを
/// チャネルへ積んでUIスレッドを起こすだけ。`mpsc::channel`（unbounded）を
/// 使っており、`sync_channel`にしてはいけない（バッファが埋まると`send`が
/// ブロックし、フックスレッドがUIスレッド待ちで止まって分離した意味が
/// 消える）。宛先の候補ウィンドウは`init_ui_thread`で先に作成済みなので、
/// 「作成前に送って誰にも引き取られない」事故は起きない。
pub(crate) fn post_ui_command(cmd: UiCommand) {
    if let Some(tx) = UI_COMMAND_TX.get() {
        let _ = tx.send(cmd);
    }
    unsafe {
        if let Some(hwnd) = CANDIDATE_HWND {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                hwnd,
                WM_APP_UI_COMMAND,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// UIスレッド側（`WM_APP_UI_COMMAND`ハンドラ）から呼ぶ。キューを最後まで
/// 引き切ってから、状態系は最後の1件だけ・動作系は出現順に全て適用する。
fn drain_ui_commands() {
    let mut latest_state: Option<UiCommand> = None;
    let mut actions: Vec<UiCommand> = Vec::new();
    {
        let Ok(guard) = UI_COMMAND_RX.lock() else { return };
        let Some(rx) = guard.as_ref() else { return };
        while let Ok(cmd) = rx.try_recv() {
            if cmd.is_state() {
                latest_state = Some(cmd);
            } else {
                actions.push(cmd);
            }
        }
    }
    if let Some(state) = latest_state {
        apply_ui_command(state);
    }
    for action in actions {
        apply_ui_command(action);
    }
}

fn apply_ui_command(cmd: UiCommand) {
    match cmd {
        UiCommand::ShowCandidateWindow { items, selected, caret } => {
            show_candidate_window_impl(&items, selected, caret)
        }
        UiCommand::ShowCommandPopup { items, descs, auto_runs, selected, caret } => {
            show_command_popup_impl(&items, &descs, &auto_runs, selected, caret)
        }
        UiCommand::FlashModeIndicator { label, caret } => flash_mode_indicator_impl(&label, caret),
        UiCommand::Hide => hide_candidate_window_impl(),
        UiCommand::OpenSettings => unsafe { open_settings_window_impl() },
        UiCommand::OpenWordSettings => unsafe { open_word_settings_window_impl() },
    }
}

/// 溜めた学習の処理をメッセージループへ依頼する。依頼できたら true
/// （候補ウィンドウが無く作れない場合は false → 呼び出し側は同期で学習する）。
///
/// 注意: フックスレッド分離（[[hook-latency-log-thread-model]]）後は、
/// このメッセージループは候補ウィンドウを所有するUIスレッドで動き、
/// フックスレッドとは別になる。したがってこの依頼は本来の意図通り
/// フックの応答時間から完全に切り離される。
pub(crate) fn request_deferred_learning() -> bool {
    unsafe {
        let hwnd = match CANDIDATE_HWND {
            Some(h) => Some(h),
            None => ensure_candidate_window(),
        };
        let Some(hwnd) = hwnd else {
            return false;
        };
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd,
            WM_APP_FLUSH_LEARNING,
            WPARAM(0),
            LPARAM(0),
        )
        .is_ok()
    }
}

pub(crate) extern "system" fn candidate_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, WINDOWPOS, WM_TIMER, WM_WINDOWPOSCHANGING, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER,
    };
    match msg {
        WM_PAINT => {
            let start = std::time::Instant::now();
            // A/B証明専用（`cargo ... --features ab_proof`時のみ有効）。
            // 同一スレッド構造が本当にフック配送を塞ぐことを、GetTickCountの
            // 分解能(約15.6ms)を余裕で超える人工的な遅延（LowLevelHooksTimeout
            // の既定300msに対して安全域を取り100ms）で証明するための仕込み。
            // featureゲートのため通常ビルドには物理的に含まれない。
            #[cfg(feature = "ab_proof")]
            std::thread::sleep(std::time::Duration::from_millis(100));
            unsafe { paint_candidates(hwnd) };
            crate::hook::hook_latency::record_handler("paint_candidates", start.elapsed());
            LRESULT(0)
        }
        // コマンドモードのポップアップ内「設定」ボタンのクリックで設定画面を開く
        WM_LBUTTONDOWN => {
            unsafe {
                let cm = CANDIDATE_UI.lock().map(|ui| ui.command_mode).unwrap_or(false);
                if cm {
                    let x = (lparam.0 & 0xFFFF) as i16 as i32;
                    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let btn = command_settings_button_rect(&rc);
                    if x >= btn.left && x <= btn.right && y >= btn.top && y <= btn.bottom {
                        open_settings_window();
                    }
                }
            }
            LRESULT(0)
        }
        // UIAポーラーからの通知: カーソルがターミナルへ戻った。打鍵を待たずに
        // その環境のコマンド行を復元し、残っていれば候補モーダルを再表示する。
        WM_APP_RESHOW_COMMAND => {
            unsafe {
                if IS_ENABLED && !any_settings_open() && !foreground_is_ours() {
                    sync_window_mode();
                    if !OUR_ACTIVE && is_terminal_focused() {
                        update_command_suggestions();
                    }
                }
            }
            LRESULT(0)
        }
        // UIAポーラーからの通知: コマンド候補モーダルを出していたターミナル窓が
        // 閉じられた。持ち主が消えたコマンド行はもう意味が無いので、破棄して
        // モーダルを閉じる（Escでの取消と同じ扱い）。
        WM_APP_TERMINAL_CLOSED => {
            if let Ok(mut b) = COMMAND_LINE.lock() {
                b.clear();
            }
            hide_candidate_window();
            LRESULT(0)
        }
        // フックから戻った後に、確定時に溜めた学習（DB書き込み）を処理する。
        // ロックが取れなければ（別スレッドが参照中）後で再試行する。
        WM_APP_FLUSH_LEARNING => {
            let start = std::time::Instant::now();
            if let Some(cm) = LIVE_CONTEXT.get() {
                match cm.try_lock() {
                    Ok(mut c) => c.flush_pending_learning(),
                    Err(_) => {
                        let _ = request_deferred_learning();
                    }
                }
            }
            crate::hook::hook_latency::record_handler("flush_pending_learning", start.elapsed());
            LRESULT(0)
        }
        // フックスレッドから積まれた候補ウィンドウ操作をまとめて適用する。
        WM_APP_UI_COMMAND => {
            let start = std::time::Instant::now();
            drain_ui_commands();
            crate::hook::hook_latency::record_handler("drain_ui_commands", start.elapsed());
            LRESULT(0)
        }
        // Z順が変更されるたびに「最前面(HWND_TOPMOST)」を強制し、他ウィンドウに
        // 前面を奪われないようにする（環境によって背面に回るのを防ぐ）。
        WM_WINDOWPOSCHANGING => {
            unsafe {
                // 設定ウィンドウ表示中は最前面固定を止める（設定を前面に保つため）
                if !any_settings_open() {
                    let wp = lparam.0 as *mut WINDOWPOS;
                    if !wp.is_null() {
                        (*wp).hwndInsertAfter = HWND_TOPMOST;
                        (*wp).flags &= !SWP_NOZORDER;
                    }
                }
            }
            LRESULT(0)
        }
        // 表示中は定期的に最前面へ再指定（他アプリが後から前面化しても復帰）。
        // モード切替フラッシュ用タイマーが発火したら、その表示を消す。
        WM_TIMER => {
            unsafe {
                if wparam.0 == MODE_FLASH_TIMER_ID {
                    let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(hwnd, MODE_FLASH_TIMER_ID);
                    // フラッシュ表示中だったときだけ消す（コマンド候補等が
                    // 後から出ていたら消さない）
                    let is_flash = CANDIDATE_UI.lock().map(|ui| ui.status).unwrap_or(false);
                    if is_flash {
                        hide_candidate_window();
                    }
                } else if any_settings_open() {
                    // 設定表示中は非最前面に落として、設定ウィンドウの下に置く
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_NOTOPMOST,
                        0, 0, 0, 0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                } else {
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        0, 0, 0, 0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 候補一覧を描画
pub(crate) unsafe fn paint_candidates(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let (items, descriptions, auto_runs, selected, status, command_mode) = match CANDIDATE_UI.lock() {
        Ok(ui) => (
            ui.items.clone(),
            ui.descriptions.clone(),
            ui.auto_runs.clone(),
            ui.selected,
            ui.status,
            ui.command_mode,
        ),
        Err(_) => {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
    };

    let mut rc_client = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc_client);

    // 背景と枠。
    // - ステータス（モード切替フラッシュ）: アクセント色で目立たせる
    // - コマンドモード: 濃紺の落ち着いた背景で「別モード」だと直感的に分かる
    // - 通常（かな漢字候補）: 白
    let bg_color = if status {
        COLORREF(0x00D77800)          // アクセント青（BBGGRR）
    } else if command_mode {
        COLORREF(0x00301E14)          // 濃紺（ターミナル風・派手すぎない）
    } else {
        COLORREF(0x00FFFFFF)          // 白
    };
    let bg = CreateSolidBrush(bg_color);
    FillRect(hdc, &rc_client, bg);
    let _ = DeleteObject(HGDIOBJ::from(bg));
    let frame_color = if command_mode { COLORREF(0x00C88A3C) } else { COLORREF(0x00999999) };
    let frame = CreateSolidBrush(frame_color);
    FrameRect(hdc, &rc_client, frame);
    let _ = DeleteObject(HGDIOBJ::from(frame));

    // コマンドモード: 先頭に控えめなモード見出しを描く
    if command_mode {
        paint_command_items(hdc, &rc_client, &items, &descriptions, &auto_runs, selected);
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // 日本語が読みやすいフォント
    // (charset=DEFAULT_CHARSET(1), precision/clip=default(0),
    //  quality=CLEARTYPE_QUALITY(5), pitch/family=default(0))
    let font = CreateFontW(
        -16, 0, 0, 0, 400, 0, 0, 0,
        1, 0, 0, 5, 0,
        w!("Meiryo UI"),
    );
    let old_font = SelectObject(hdc, HGDIOBJ::from(font));
    SetBkMode(hdc, TRANSPARENT);

    let line_h = CANDIDATE_LINE_HEIGHT;
    let total = items.len();

    // ステータス表示（モード切替フラッシュ）は単一行なのでページングしない。
    if status {
        for (i, item) in items.iter().enumerate() {
            let top = 4 + (i as i32) * line_h;
            let mut rc = RECT { left: 12, top, right: rc_client.right - 4, bottom: top + line_h };
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
            let mut wide: Vec<u16> = item.encode_utf16().collect();
            DrawTextW(hdc, &mut wide, &mut rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        }
        SelectObject(hdc, old_font);
        let _ = DeleteObject(HGDIOBJ::from(font));
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // 既存IMEと同様のページング表示: 1ページ CANDIDATE_PAGE_SIZE 件を固定で並べ、
    // 選択が別ページに移ると自動でページが切り替わる。番号キー1〜9は各ページで有効。
    // 予測変換一覧も同じ描画を通るが、件数が1ページに収まる場合はインジケータを出さない。
    let page_start = candidate_page_start(selected);
    let page_end = (page_start + CANDIDATE_PAGE_SIZE).min(total);
    for (row, i) in (page_start..page_end).enumerate() {
        let item = &items[i];
        let top = 4 + (row as i32) * line_h;
        let mut rc = RECT {
            left: 4,
            top,
            right: rc_client.right - 4,
            bottom: top + line_h,
        };

        if i == selected {
            // 選択行はアクセント色で強調 (RGB 0,120,215 / COLORREF は 0x00BBGGRR)
            let hl = CreateSolidBrush(COLORREF(0x00D77800));
            FillRect(hdc, &rc, hl);
            let _ = DeleteObject(HGDIOBJ::from(hl));
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
        } else {
            SetTextColor(hdc, COLORREF(0x00000000));
        }

        // ページ内の連番（1〜9）。番号キーはこの番号（＝ページ内位置）で選ぶ。
        let text = format!("{}  {}", row + 1, item);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        rc.left += 8;
        DrawTextW(hdc, &mut wide, &mut rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }

    // ページ位置インジケータ（総数が1ページを超えるときだけ最下段に出す）。
    // 例: 「12 / 23  ▲▼」。既存IMEの「現在位置/全体」に相当。
    if total > CANDIDATE_PAGE_SIZE {
        let rows = page_end - page_start;
        let top = 4 + (rows as i32) * line_h;
        let ind_rc = RECT { left: 4, top, right: rc_client.right - 4, bottom: top + line_h };
        let bg = CreateSolidBrush(COLORREF(0x00EFEFEF));
        FillRect(hdc, &ind_rc, bg);
        let _ = DeleteObject(HGDIOBJ::from(bg));
        SetTextColor(hdc, COLORREF(0x00606060));
        let label = format!("{} / {}  \u{25B2}\u{25BC}", selected + 1, total);
        let mut wide: Vec<u16> = label.encode_utf16().collect();
        let mut rc = RECT { left: 12, ..ind_rc };
        DrawTextW(hdc, &mut wide, &mut rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }

    SelectObject(hdc, old_font);
    let _ = DeleteObject(HGDIOBJ::from(font));
    let _ = EndPaint(hwnd, &ps);
}

/// コマンドモードのポップアップ右上「⚙ 設定」ボタンの矩形（クライアント座標）。
/// 描画とクリック判定で同じ値を使う。
pub(crate) fn command_settings_button_rect(rc_client: &RECT) -> RECT {
    let right = rc_client.right - 6;
    let left = right - 84;
    RECT {
        left,
        top: 5,
        right,
        bottom: 5 + CANDIDATE_LINE_HEIGHT - 2,
    }
}

/// コマンドモードのコマンド候補を描画する（見出し＋⚙設定ボタン＋番号＋コマンド＋薄い説明）。
/// 番号は Ctrl+数字 で直接選択できる（素の数字キーはコマンド文字列の一部として
/// 打つため、選択キーには使えない）。Tab で先頭を補完。
pub(crate) unsafe fn paint_command_items(
    hdc: HDC,
    rc_client: &RECT,
    items: &[String],
    descriptions: &[String],
    auto_runs: &[bool],
    selected: usize,
) {
    let font = CreateFontW(
        -16, 0, 0, 0, 400, 0, 0, 0,
        1, 0, 0, 5, 0,
        w!("Meiryo UI"),
    );
    let old_font = SelectObject(hdc, HGDIOBJ::from(font));
    SetBkMode(hdc, TRANSPARENT);

    // 右上に「⚙ 設定」ボタンを描く（クリックで追加/編集/削除の画面へ）
    let btn = command_settings_button_rect(rc_client);
    let btn_bg = CreateSolidBrush(COLORREF(0x00C88A3C)); // 琥珀の塗り
    FillRect(hdc, &btn, btn_bg);
    let _ = DeleteObject(HGDIOBJ::from(btn_bg));
    SetTextColor(hdc, COLORREF(0x00FFFFFF));
    let mut btn_txt: Vec<u16> = "\u{2699} 設定".encode_utf16().collect();
    let mut btn_rc = RECT { left: btn.left + 8, ..btn };
    DrawTextW(hdc, &mut btn_txt, &mut btn_rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);

    // 見出し行（琥珀色）: 一目でコマンドモードと分かるように。ボタンに被らないよう右端を制限。
    let mut rc_head = RECT {
        left: 12,
        top: 4,
        right: btn.left - 10,
        bottom: 4 + CANDIDATE_LINE_HEIGHT,
    };
    SetTextColor(hdc, COLORREF(0x0046C8F0)); // 琥珀 (BBGGRR)
    // 選択中候補の「Enterで即実行」設定（設定画面のチェックボックス）を見出しに反映する。
    // オン: Enter:即実行 / オフ: Enter:挿入のみ（編集してから実行）
    let enter_hint = if auto_runs.get(selected).copied().unwrap_or(true) {
        "Enter:即実行"
    } else {
        "Enter:挿入のみ"
    };
    let mut head: Vec<u16> = format!(
        "\u{2318} コマンド  ( Tab:補完 / Ctrl+数字:選択 / {} )",
        enter_hint
    )
    .encode_utf16()
    .collect();
    DrawTextW(hdc, &mut head, &mut rc_head, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);

    // 左カラム（コマンド）の幅を実測して、説明カラムの開始 x を揃える。
    // これで「左＝コマンド / 右＝説明」の表（モーダル）らしい整列になる。
    const LEFT_PAD: i32 = 14;
    const COL_GAP: i32 = 28;
    // 各行に Ctrl+数字 で選べる番号（1〜9）を付ける。件数は呼び出し側
    // （command_predictions の limit）で 9 件以下に抑えてあるので、
    // 常に単一ページで番号キーと一対一に対応する。
    let numbered: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(i, cmd)| format!("{}  {}", i + 1, cmd))
        .collect();
    let mut cmd_col_w = 0i32;
    for cmd in numbered.iter() {
        let w: Vec<u16> = cmd.encode_utf16().collect();
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &w, &mut sz);
        cmd_col_w = cmd_col_w.max(sz.cx);
    }
    let has_desc = descriptions.iter().any(|d| !d.is_empty());
    let desc_x = LEFT_PAD + cmd_col_w + COL_GAP;

    // カラム区切りの薄い縦線（説明がある時だけ）
    if has_desc && desc_x < rc_client.right - 40 {
        let sep = CreateSolidBrush(COLORREF(0x00463628));
        let sep_rc = RECT {
            left: desc_x - COL_GAP / 2,
            top: 4 + CANDIDATE_LINE_HEIGHT,
            right: desc_x - COL_GAP / 2 + 1,
            bottom: 4 + (items.len() as i32 + 1) * CANDIDATE_LINE_HEIGHT,
        };
        FillRect(hdc, &sep_rc, sep);
        let _ = DeleteObject(HGDIOBJ::from(sep));
    }

    for (i, cmd) in numbered.iter().enumerate() {
        let top = 4 + ((i + 1) as i32) * CANDIDATE_LINE_HEIGHT;
        let rc_row = RECT {
            left: 4,
            top,
            right: rc_client.right - 6,
            bottom: top + CANDIDATE_LINE_HEIGHT,
        };
        if i == selected {
            // 先頭（Tabで補完される候補）を控えめに強調
            let hl = CreateSolidBrush(COLORREF(0x00553A24));
            FillRect(hdc, &rc_row, hl);
            let _ = DeleteObject(HGDIOBJ::from(hl));
        }
        // 左カラム: 番号＋コマンド本体（明るい文字）
        SetTextColor(hdc, COLORREF(0x00FFFFFF));
        let mut wcmd: Vec<u16> = cmd.encode_utf16().collect();
        let mut rc_cmd = RECT { left: LEFT_PAD, ..rc_row };
        DrawTextW(hdc, &mut wcmd, &mut rc_cmd, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
        // 右カラム: 説明（薄いグレー・左寄せで揃える）
        if let Some(desc) = descriptions.get(i) {
            if !desc.is_empty() {
                SetTextColor(hdc, COLORREF(0x00A8A8A8));
                let mut wdesc: Vec<u16> = desc.encode_utf16().collect();
                let mut rc_desc = RECT { left: desc_x, ..rc_row };
                DrawTextW(
                    hdc,
                    &mut wdesc,
                    &mut rc_desc,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
            }
        }
    }

    SelectObject(hdc, old_font);
    let _ = DeleteObject(HGDIOBJ::from(font));
}

/// 候補一覧ウィンドウを（なければ作って）返す
///
/// フックを張ったスレッド（conversion-service のメインスレッド）で
/// 呼ばれるため、そのスレッドの既存メッセージループが描画を駆動する。
pub(crate) unsafe fn ensure_candidate_window() -> Option<HWND> {
    if let Some(hwnd) = CANDIDATE_HWND {
        return Some(hwnd);
    }
    // ここから先は実際にウィンドウを作る経路。`init_ui_thread`がフック
    // スレッド起動前に一度作っておくため、通常はここへ到達しない
    // （到達＝起動順序の前提が崩れている）。
    debug_assert_current_thread_is_ui("ensure_candidate_window (creation path)");

    let hinstance = GetModuleHandleW(None).ok()?;
    let class_name = w!("ImeLiveCandidateList");

    // 通常の矢印カーソルを設定する。未設定だとホバー時に「読み込み中（砂時計）」
    // カーソルが出てしまう。
    let arrow_cursor = windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
        None,
        windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
    )
    .unwrap_or_default();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(candidate_wndproc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        hCursor: arrow_cursor,
        ..Default::default()
    };
    // 二重登録はエラーになるが、その場合も既存クラスが使えるので無視
    RegisterClassW(&wc);

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        class_name,
        w!(""),
        WS_POPUP,
        0, 0, 10, 10,
        None,
        None,
        hinstance,
        None,
    )
    .ok()?;

    CANDIDATE_HWND = Some(hwnd);
    Some(hwnd)
}

/// 候補ウィンドウの表示位置を決める（入力位置の近く）
///
/// 優先順位（いずれもブロックしない軽量 API のみ）:
/// 1. テキストキャレットの真下（メモ帳・conhost など Win32 が
///    キャレット位置を公開するアプリ）
/// 2. フォアグラウンドウィンドウの下部・左寄り
///    （Windows Terminal 等はキャレットを公開しないが、ターミナルは
///    出力が上へ流れて入力行が下端付近にあるため、ここが入力位置に近い）
///
/// マウスカーソル位置は使わない（打鍵中に動かず、たまたまある場所に
/// 出て「右上に飛ぶ」ように見えるため）。
///
/// 注: UI Automation でブラウザ内の入力要素を正確に狙うことも可能だが、
/// クロスプロセスの同期 COM 呼び出しはフックスレッドをブロックして
/// 候補ウィンドウの描画を止めてしまうため使わない。
///
/// 戻り値は (x, キャレット上端y, キャレット下端y)（画面座標）。
/// place_popup がこの上下端を見て、真下に余白があれば下、無ければ真上に出す
/// （既存IMEと同様の出し分け）。
/// `caret_screen_pos` が実位置（Win32キャレット or UIAキャッシュ）を返せるか。
/// 実位置が無い状態は、フォアグラウンド窓の最下行やスクリーン最下部という
/// 粗いフォールバックに頼ることになる。初回表示だけはこれを避けたい
/// （後述の呼び出し側を参照）。
pub(crate) fn has_reliable_caret_pos() -> bool {
    unsafe {
        let hwnd_fg = GetForegroundWindow();
        if hwnd_fg.0.is_null() {
            return false;
        }
        let tid = GetWindowThreadProcessId(hwnd_fg, None);
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(tid, &mut gti).is_ok()
            && !gti.hwndCaret.0.is_null()
            && (gti.rcCaret.bottom > gti.rcCaret.top || gti.rcCaret.right > gti.rcCaret.left)
        {
            return true;
        }
        UIA_ANCHOR.lock().map(|c| c.is_some()).unwrap_or(false)
    }
}

pub(crate) fn caret_screen_pos() -> (i32, i32, i32) {
    unsafe {
        let hwnd_fg = GetForegroundWindow();
        if !hwnd_fg.0.is_null() {
            let tid = GetWindowThreadProcessId(hwnd_fg, None);
            let mut gti = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            // 1. テキストキャレット（メモ帳・多くの Win32 エディタ）
            if GetGUIThreadInfo(tid, &mut gti).is_ok()
                && !gti.hwndCaret.0.is_null()
                && (gti.rcCaret.bottom > gti.rcCaret.top || gti.rcCaret.right > gti.rcCaret.left)
            {
                let mut top = POINT { x: gti.rcCaret.left, y: gti.rcCaret.top };
                let mut bot = POINT { x: gti.rcCaret.left, y: gti.rcCaret.bottom };
                let _ = ClientToScreen(gti.hwndCaret, &mut top);
                let _ = ClientToScreen(gti.hwndCaret, &mut bot);
                return (top.x, top.y, bot.y);
            }

            // 2. UI Automation で取得した入力欄の位置（ブラウザ・ターミナル等）
            if let Ok(cache) = UIA_ANCHOR.lock() {
                if let Some((x, t, b)) = *cache {
                    return (x, t, b);
                }
            }

            // 3. フォアグラウンドウィンドウの最下行を「入力行」とみなす。
            //    ターミナル等はキャレットを公開せずここに来る。最下行を
            //    1行分の高さのキャレットとして扱い、その上に出す。
            let mut rc = RECT::default();
            if GetWindowRect(hwnd_fg, &mut rc).is_ok() && rc.bottom > rc.top {
                let x = rc.left + 24;
                return (x, rc.bottom - CANDIDATE_LINE_HEIGHT, rc.bottom);
            }
        }
        // 取得できない場合は画面左下寄りに固定表示（少なくとも見える）
        let h = GetSystemMetrics(SM_CYSCREEN);
        (80, h - CANDIDATE_LINE_HEIGHT, h)
    }
}

/// 候補一覧ウィンドウを表示・更新する（どのスレッドからも呼べる）。
/// キャレット位置を呼び出し元スレッドで取得したうえで`UiCommand`に載せ、
/// 実際のWin32呼び出しはUIスレッド側（`show_candidate_window_impl`）に
/// 委ねる。
pub(crate) fn show_candidate_window(items: &[String], selected: usize) {
    if items.is_empty() {
        hide_candidate_window();
        return;
    }
    let caret = caret_screen_pos();
    post_ui_command(UiCommand::ShowCandidateWindow {
        items: items.to_vec(),
        selected,
        caret,
    });
}

fn show_candidate_window_impl(items: &[String], selected: usize, caret: (i32, i32, i32)) {
    debug_assert_current_thread_is_ui("show_candidate_window_impl");
    if let Ok(mut ui) = CANDIDATE_UI.lock() {
        ui.items = items.to_vec();
        ui.descriptions = Vec::new();
        ui.auto_runs = Vec::new();
        ui.selected = selected;
        ui.visible = true;
        ui.status = false;
        ui.command_mode = false;
    }
    let max_len = items.iter().map(|s| s.chars().count()).max().unwrap_or(1);
    // 高さは「1ページ分の行数（最大 CANDIDATE_PAGE_SIZE）＋ページインジケータ1行」。
    // 候補が多くても縦に伸び続けず、既存IMEのように一定サイズで収まる。
    let total = items.len();
    let page_start = candidate_page_start(selected.min(total.saturating_sub(1)));
    let rows_on_page = (total - page_start).min(CANDIDATE_PAGE_SIZE);
    let indicator = if total > CANDIDATE_PAGE_SIZE { 1 } else { 0 };
    place_popup(max_len, (rows_on_page + indicator) as i32, caret);
}

/// コマンドモードのコマンド候補をカーソル付近に表示する。
/// items[i] = コマンド、descs[i] = 簡易説明（無ければ空）。先頭を Tab で補完。
/// auto_runs[i] = Enter で即実行するか（見出しに Enter:実行 / 挿入→編集 を出す）。
pub(crate) fn show_command_popup(items: &[String], descs: &[String], auto_runs: &[bool], selected: usize) {
    if items.is_empty() {
        hide_candidate_window();
        return;
    }
    let sel = selected.min(items.len() - 1);
    let caret = caret_screen_pos();
    post_ui_command(UiCommand::ShowCommandPopup {
        items: items.to_vec(),
        descs: descs.to_vec(),
        auto_runs: auto_runs.to_vec(),
        selected: sel,
        caret,
    });
}

fn show_command_popup_impl(
    items: &[String],
    descs: &[String],
    auto_runs: &[bool],
    selected: usize,
    caret: (i32, i32, i32),
) {
    debug_assert_current_thread_is_ui("show_command_popup_impl");
    if let Ok(mut ui) = CANDIDATE_UI.lock() {
        ui.items = items.to_vec();
        ui.descriptions = descs.to_vec();
        ui.auto_runs = auto_runs.to_vec();
        ui.selected = selected;
        ui.visible = true;
        ui.status = false;
        ui.command_mode = true;
    }
    // 幅: コマンド最大長 + 説明最大長 + 見出し分の余白
    let cmd_len = items.iter().map(|s| s.chars().count()).max().unwrap_or(1);
    let desc_len = descs.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let head_len = 36; // 見出し行の目安（Tab:補完 / Enter:挿入→編集 まで入る幅）
    let max_len = (cmd_len + desc_len + 4).max(head_len);
    // 見出し行 + 候補行
    place_popup(max_len, items.len() as i32 + 1, caret);
}

/// モード切替時に、現在のモード名をカーソル付近へ一定時間フラッシュ表示する。
/// 「モードが切り替わった」ことを明示するためのもの（自動で消える）。
pub(crate) fn flash_mode_indicator(label: &str) {
    let caret = caret_screen_pos();
    post_ui_command(UiCommand::FlashModeIndicator { label: label.to_string(), caret });
}

fn flash_mode_indicator_impl(label: &str, caret: (i32, i32, i32)) {
    debug_assert_current_thread_is_ui("flash_mode_indicator_impl");
    if let Ok(mut ui) = CANDIDATE_UI.lock() {
        ui.items = vec![label.to_string()];
        ui.descriptions = Vec::new();
        ui.auto_runs = Vec::new();
        ui.selected = 0;
        ui.visible = true;
        ui.status = true;       // アクセント背景で目立たせる
        ui.command_mode = false;
    }
    place_popup(label.chars().count() + 2, 1, caret);
    // 一定時間後に自動で消すタイマーを仕掛ける
    unsafe {
        if let Some(hwnd) = CANDIDATE_HWND {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
                hwnd,
                MODE_FLASH_TIMER_ID,
                1100,
                None,
            );
        }
    }
}

/// 予測変換の候補をカーソル付近に表示する（↑↓で選択移動・Enter/番号で確定）。
pub(crate) fn show_prediction_popup(items: &[String], selected: usize) {
    if items.is_empty() {
        hide_candidate_window();
        return;
    }
    let sel = selected.min(items.len() - 1);
    show_candidate_window(items, sel);
}

/// ポップアップ（候補/ステータス）を組み立ててカーソル付近に配置・表示する。
/// `caret`はフックスレッドが打鍵の瞬間に`caret_screen_pos()`で取得した値を
/// `UiCommand`に載せて渡したもの（`caret_screen_pos`自体は
/// `GetGUIThreadInfo`＋キャッシュ参照のみでクロススレッドのブロックを
/// 起こさないため、どちらのスレッドで呼んでも安全だが、打鍵の瞬間の
/// 値を使う方が正確なためフックスレッド側で取得する）。
pub(crate) fn place_popup(max_len_chars: usize, line_count: i32, caret: (i32, i32, i32)) {
    debug_assert_current_thread_is_ui("place_popup");
    unsafe {
        let Some(hwnd) = ensure_candidate_window() else { return };

        let width = ((max_len_chars + 4) * 16 + 24).min(640) as i32;
        let mut height = 8 + line_count * CANDIDATE_LINE_HEIGHT;
        let (mut x, caret_top, caret_bottom) = caret;

        // 基点が乗っているモニターの作業領域を取得してクランプする
        // （SM_CXSCREEN は主モニターのみなので、マルチモニターだと
        //  副モニターの座標が主モニター右端に丸められ「右上」に飛ぶ）
        let mon = MonitorFromPoint(POINT { x, y: caret_bottom }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let (left, top, right, bottom) = if GetMonitorInfoW(mon, &mut mi).as_bool() {
            (mi.rcWork.left, mi.rcWork.top, mi.rcWork.right, mi.rcWork.bottom)
        } else {
            (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        };

        // 候補が多いとポップアップが縦に伸びて画面下からはみ出す。作業領域の
        // 高さに収まるようにクランプし、あふれる分は paint_candidates が
        // 選択行に追従してスクロール表示する。
        height = height.min(bottom - top);

        // 既存IMEと同様: キャレットの真下に余白があれば下、無ければ真上に出す。
        // これで入力文字（カーソル）にリストが被らない。
        let below_top = caret_bottom + 2;
        let above_top = caret_top - height - 2;
        let mut y = if below_top + height <= bottom {
            below_top
        } else {
            above_top
        };

        x = x.clamp(left, (right - width).max(left));
        y = y.clamp(top, (bottom - height).max(top));

        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x, y, width, height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        // 前面固定を確実にするため最前面を再指定（位置・サイズは維持）
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0, 0, 0, 0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
        // 表示中は定期的に最前面へ戻すタイマーを起動（他アプリに前面を
        // 奪われても復帰させる。環境依存の背面化対策）
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
            hwnd,
            TOPMOST_TIMER_ID,
            200,
            None,
        );
        let _ = InvalidateRect(hwnd, None, true);
    }
}

/// 候補一覧ウィンドウを隠す（どのスレッドからも呼べる。実処理はUIスレッドへ
/// 委ねる）。
pub(crate) fn hide_candidate_window() {
    post_ui_command(UiCommand::Hide);
}

fn hide_candidate_window_impl() {
    debug_assert_current_thread_is_ui("hide_candidate_window_impl");
    unsafe {
        let was_visible = match CANDIDATE_UI.lock() {
            Ok(mut ui) => {
                let v = ui.visible;
                ui.visible = false;
                ui.status = false;
                ui.command_mode = false;
                v
            }
            Err(_) => false,
        };
        if was_visible {
            if let Some(hwnd) = CANDIDATE_HWND {
                let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(hwnd, TOPMOST_TIMER_ID);
                let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(hwnd, MODE_FLASH_TIMER_ID);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

/// スクリーン座標 (x, y) が候補ポップアップのウィンドウ矩形内か
/// （マウスフックが「ポップアップの外側をクリックした」を判定するのに使う。
///  ウィンドウが未生成・非表示なら false）
pub(crate) fn popup_contains_screen_point(x: i32, y: i32) -> bool {
    unsafe {
        let Some(hwnd) = CANDIDATE_HWND else {
            return false;
        };
        if !candidate_window_visible() {
            return false;
        }
        let mut rc = RECT::default();
        if windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rc).is_err() {
            return false;
        }
        point_in_rect(x, y, &rc)
    }
}

/// 点 (x, y) が矩形 `rc` の内側か（右辺・下辺は含まない。Win32 の慣例）
pub(crate) fn point_in_rect(x: i32, y: i32, rc: &RECT) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

/// 候補一覧が表示中か
pub(crate) fn candidate_window_visible() -> bool {
    CANDIDATE_UI.lock().map(|ui| ui.visible).unwrap_or(false)
}
