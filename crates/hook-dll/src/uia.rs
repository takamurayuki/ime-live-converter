//! UI Automation ポーラー: フォーカス入力欄・キャレット位置の取得と統合ターミナル検出

use crate::*;
use windows::Win32::Foundation::HANDLE;

/// フォーカス中の要素が「統合ターミナル」（VSCode の xterm 等）か。UIAポーラーが
/// クラス名から判定して更新する。窓クラスで判別できないアプリの端末検出に使う。
pub(crate) static FOCUSED_IS_TERMINAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// パスワード欄／昇格プロセス判定の3値状態。
///
/// 2値(真偽)ではなく3値にしているのは、ポーリング間隔（アイドル時最大250ms）
/// の間だけ「安全」を騙ってしまう窓を無くすため。フォーカスが変わった瞬間
/// （`SetWinEventHook`のコールバック、`focus_change_win_event_proc`）に
/// 即座に`Unknown`へ落とし、実際にUIA/整合性レベルの判定が付くまでは
/// `Safe`に戻さない。フックコールバック側は`Safe`だけを「通常どおりキーを
/// 消費してよい」とし、`Unknown`と`Unsafe`はどちらも素通しにする
/// （判定が付くまでの数十ms、変換が一時的に効かなくなるが、パスワードや
/// 昇格プロセスへの入力を誤って消費するよりはるかに安全）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusSafety {
    /// フォーカスが変わった直後で、まだ判定していない（フェイルセーフ）
    Unknown = 0,
    /// 判定済みで、通常どおり処理してよい
    Safe = 1,
    /// 判定済みで、パスワード欄／昇格プロセスなので素通しすべき
    Unsafe = 2,
}

impl FocusSafety {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => FocusSafety::Safe,
            2 => FocusSafety::Unsafe,
            _ => FocusSafety::Unknown,
        }
    }
}

pub(crate) struct FocusSafetyCell(std::sync::atomic::AtomicU8);

impl FocusSafetyCell {
    const fn new() -> Self {
        Self(std::sync::atomic::AtomicU8::new(0))
    }
    pub(crate) fn load(&self) -> FocusSafety {
        FocusSafety::from_u8(self.0.load(std::sync::atomic::Ordering::Acquire))
    }
    fn store(&self, v: FocusSafety) {
        self.0.store(v as u8, std::sync::atomic::Ordering::Release);
    }
}

/// フォーカス中の入力欄がパスワード欄か（`crate::hook`のフックコールバックが
/// 打鍵ごとに参照し、`Safe`以外なら完全パススルーしてログも一切残さない）。
/// グローバルキーボードフック＋SendInputという構成はキーロガーと振る舞いが
/// 一致するため、この判定は配布可否に直結する。
///
/// 実際の判定（UIAの`IsPassword`／`ES_PASSWORD`）はこのポーラースレッドが
/// 行いキャッシュに書く。フックスレッド内で同期COM呼び出し（クロスプロセスで
/// ブロックしうる）を直接行うことは無い。
pub(crate) static FOCUSED_PASSWORD_STATE: FocusSafetyCell = FocusSafetyCell::new();
/// フォアグラウンド窓が自プロセスより高い整合性レベル（管理者権限で起動された
/// プロセス等）を持つか。`crate::hook`のフックコールバックが打鍵ごとに参照し、
/// `Safe`以外ならキーを消費せず素通しする。
///
/// 理由: `SendInput`によるテキスト確定はUIPI（User Interface Privilege
/// Isolation）により低整合性→高整合性プロセスへの注入が拒否される。
/// 素通し判定を入れずにキーを消費してしまうと、変換結果を届けられないまま
/// 元の打鍵も失われ、ユーザーには「入力が消えた」ようにしか見えない。
/// 恒久対応はマニフェストへの`uiAccess=true`指定だが、署名済み実行ファイルを
/// `Program Files`配下に置くという条件が付くため、それまでの防御線として
/// ここで検出する。
///
/// このポーラースレッドが更新するキャッシュであり、フックスレッド内で
/// `OpenProcess`/`GetTokenInformation`等の同期呼び出しは直接行わない
/// （他プロセスの状態次第でブロックしうるため）。
pub(crate) static FOCUSED_ELEVATED_STATE: FocusSafetyCell = FocusSafetyCell::new();

/// フォーカス変更イベント（`focus_change_win_event_proc`）を受けたら、
/// ポーラーのアイドルスリープ（最大250ms）を待たずに即座に起こすための
/// チャネル。ポーラー開始時に`start_uia_poller`がSenderをここへ格納する。
/// コールバック→ポーラーの一方向シグナルなので、容量1の`sync_channel`で
/// 「起床済みの通知が既にあれば重ねて送らない」（`try_send`が素直に失敗する）
/// だけで十分。
///
/// 既知の限界（推測される影響は軽微）: `OnceLock`のため`install_hook`が
/// 再インストールされてポーラースレッドが再生成されても、ここに残るのは
/// 最初のスレッドのSenderのまま（`set`は2回目以降失敗する）。その場合でも
/// `FOCUSED_PASSWORD_STATE`/`FOCUSED_ELEVATED_STATE`への`Unknown`書き込み
/// （フェイルセーフ本体）はコールバックから直接行われるため安全性は保たれる。
/// 失われるのは新しいポーラーの即時起床（最悪250ms遅延にフォールバック）
/// だけ。現状`install_hook`はプロセス起動時に一度だけ呼ばれる運用のため
/// 実害は無いと判断し、対応は見送っている。
static FOCUS_WAKE_TX: std::sync::OnceLock<std::sync::mpsc::SyncSender<()>> =
    std::sync::OnceLock::new();

/// フォーカスを変えた瞬間に`FOCUSED_PASSWORD_STATE`/`FOCUSED_ELEVATED_STATE`を
/// `Unknown`へ落とすための`SetWinEventHook`ハンドル（`EVENT_SYSTEM_FOREGROUND`と
/// `EVENT_OBJECT_FOCUS`の2つ）。`isize`で保持するのは、`HWINEVENTHOOK`が生
/// ポインタのnewtypeで`Sync`を実装しないため`static`に直接置けないから
/// （`hook_thread.rs`の`HHOOK`がthread_localで持つのと同じ理由の裏返し。
/// こちらはUIスレッドからしか触らない前提で単純な`AtomicIsize`にしている）。
static FOREGROUND_WIN_EVENT_HOOK: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
static FOCUS_WIN_EVENT_HOOK: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// `install_hook`から、UIスレッド（`popup::init_ui_thread`を呼んだのと同じ
/// スレッド）上で呼ぶこと。`SetWinEventHook`（`WINEVENT_OUTOFCONTEXT`）は
/// 登録したスレッドがメッセージを汲み続けて（`PeekMessageW`/`GetMessageW`）
/// いないとコールバックが配送されないため、常時メッセージポンプを回している
/// UIスレッドである必要がある（キー配送専用のフックスレッドには置かない。
/// COM/UIA呼び出しを増やさないという[[hook-latency-log-thread-model]]の
/// 制約に反するため）。
pub(crate) fn install_focus_watch() {
    crate::popup::debug_assert_current_thread_is_ui("install_focus_watch");
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::UI::Accessibility::SetWinEventHook;
    use windows::Win32::UI::WindowsAndMessaging::{
        EVENT_OBJECT_FOCUS, EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT,
    };
    unsafe {
        let h_fg = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            HMODULE::default(),
            Some(focus_change_win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let h_focus = SetWinEventHook(
            EVENT_OBJECT_FOCUS,
            EVENT_OBJECT_FOCUS,
            HMODULE::default(),
            Some(focus_change_win_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        FOREGROUND_WIN_EVENT_HOOK.store(h_fg.0 as isize, std::sync::atomic::Ordering::Release);
        FOCUS_WIN_EVENT_HOOK.store(h_focus.0 as isize, std::sync::atomic::Ordering::Release);
        if h_fg.0.is_null() || h_focus.0.is_null() {
            eprintln!(
                "警告: SetWinEventHookに失敗しました。パスワード欄/昇格プロセス検出が\
                 ポーリングのみ（最大250ms遅延）にフォールバックします。"
            );
        }
    }
}

/// `uninstall_hook`から、`install_focus_watch`を呼んだのと同じUIスレッドで
/// 呼ぶこと。
pub(crate) fn uninstall_focus_watch() {
    crate::popup::debug_assert_current_thread_is_ui("uninstall_focus_watch");
    use windows::Win32::UI::Accessibility::{UnhookWinEvent, HWINEVENTHOOK};
    unsafe {
        let h_fg = FOREGROUND_WIN_EVENT_HOOK.swap(0, std::sync::atomic::Ordering::AcqRel);
        if h_fg != 0 {
            let _ = UnhookWinEvent(HWINEVENTHOOK(h_fg as *mut core::ffi::c_void));
        }
        let h_focus = FOCUS_WIN_EVENT_HOOK.swap(0, std::sync::atomic::Ordering::AcqRel);
        if h_focus != 0 {
            let _ = UnhookWinEvent(HWINEVENTHOOK(h_focus as *mut core::ffi::c_void));
        }
    }
}

/// `SetWinEventHook`のコールバック本体。フォーカスが変わった「事実」だけを
/// 扱い、実際にパスワード欄/昇格プロセスかどうかの判定（UIA/トークン照会）は
/// 一切行わない。ここで重い処理をすると登録元スレッド（UIスレッド）の
/// メッセージポンプが詰まるため、`Unknown`への書き込みとポーラーの起床
/// 通知だけに留める（どちらもpanicしうるロック/アロケーションを伴わない）。
unsafe extern "system" fn focus_change_win_event_proc(
    _hwineventhook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _idobject: i32,
    _idchild: i32,
    _ideventthread: u32,
    _dwmseventtime: u32,
) {
    FOCUSED_PASSWORD_STATE.store(FocusSafety::Unknown);
    FOCUSED_ELEVATED_STATE.store(FocusSafety::Unknown);
    // フォーカス変更は確定後の復元用リングバッファの無効化条件
    // （[[commit-ring-buffer]]）。`try_lock`で非ブロッキングにする
    // （このコールバックはUIスレッドのメッセージポンプ経由で呼ばれる
    // ため、ここで待たされるとポンプが詰まる）。
    if let Some(cm) = LIVE_CONTEXT.get() {
        if let Ok(mut c) = cm.try_lock() {
            c.invalidate_commit_ring();
        }
    }
    if let Some(tx) = FOCUS_WAKE_TX.get() {
        let _ = tx.try_send(());
    }
}
/// この Space 押下で既に LLM 変換を発火したか（オートリピートの二重発火防止）

/// この回数だけ連続して位置が変化しなかったら「安定」とみなし、
/// ポーリング間隔を`POSITION_STABLE_INTERVAL_MS`まで落とす。
const POSITION_STABLE_THRESHOLD: u32 = 3;
/// 位置が安定している間のポーリング間隔（ミリ秒）。通常の150msより長くし、
/// AttachConsole/UIA呼び出しの頻度を下げてカーソル点滅への影響を減らす。
const POSITION_STABLE_INTERVAL_MS: u64 = 500;

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

        // フォーカス変更イベント（win_event側）が届いたら、アイドル時の
        // 250ms固定スリープを待たずにここで即座に起こす。
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let _ = FOCUS_WAKE_TX.set(wake_tx);

        let mut last_hwnd: isize = 0;
        // カーソル（フォーカス）がターミナルへ「戻った」瞬間の検出用
        let mut was_terminal = false;
        let mut last_terminal_hwnd: isize = 0;
        // 位置が連続して変化していない回数（作文中の点滅対策用、下記参照）
        let mut stable_count: u32 = 0;
        let mut last_pos: Option<(i32, i32, i32)> = None;
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

            // パスワード欄判定は毎周期・無条件で行う（need_posのゲートに
            // 掛けない）。need_posは「変換中/ポップアップ表示中」限定だが、
            // パスワード欄はIME操作前の入力欄フォーカス移動だけで即座に
            // 反映されている必要があるため。
            let is_pw = focused_hwnd_has_es_password() || focused_element_is_password(&auto);
            FOCUSED_PASSWORD_STATE.store(if is_pw { FocusSafety::Unsafe } else { FocusSafety::Safe });

            // 昇格プロセス判定もパスワード欄判定と同様に毎周期・無条件で行う。
            let is_elevated_fg = foreground_process_is_more_elevated_than_self();
            FOCUSED_ELEVATED_STATE.store(if is_elevated_fg { FocusSafety::Unsafe } else { FocusSafety::Safe });

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

            // コマンド候補モーダルを表示中、それを出した張本人のターミナル窓が
            // （Xボタン・Alt+F4・taskkill 等で）閉じられていないか確認する。
            // 閉じられていたら、次の打鍵を待たずにモーダルを閉じる（そうしないと
            // 誰も所有していない浮遊ウィンドウとして残ってしまう）。
            if last_terminal_hwnd != 0 {
                let cm_visible = CANDIDATE_UI
                    .lock()
                    .map(|ui| ui.visible && ui.command_mode)
                    .unwrap_or(false);
                if cm_visible {
                    let still_exists = windows::Win32::UI::WindowsAndMessaging::IsWindow(
                        HWND(last_terminal_hwnd as *mut core::ffi::c_void),
                    )
                    .as_bool();
                    if !still_exists {
                        if let Some(hwnd) = CANDIDATE_HWND {
                            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                                hwnd,
                                WM_APP_TERMINAL_CLOSED,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        last_terminal_hwnd = 0;
                    }
                }
            }

            // 入力欄の位置が必要なのは「変換中(OUR_ACTIVE)」か「候補/コマンドの
            // ポップアップ表示中」、または「コマンド行を打ち始めていて、
            // これから初回表示しようとしている」とき。アイドル時に
            // UIA(キャレット) や AttachConsole を毎回叩くと、対象アプリの
            // カーソル点滅が乱れるため、不要時は休む。
            // 最後の条件（打ち始め）が無いと、ポップアップがまだ一度も
            // 表示されていない最初の打鍵の時点では need_pos が立たず、
            // UIA_ANCHOR が空のまま初回表示だけフォールバック位置（画面/窓の
            // 最下部）になってしまう（次の打鍵で正しい位置へ飛んで見える
            // 原因だった）。COMMAND_LINE の空チェックだけなら軽量なミューテックス
            // ロックのみでカーソル点滅への影響は無いため、アイドル時でも見て良い。
            let command_line_pending = COMMAND_LINE.lock().map(|b| !b.is_empty()).unwrap_or(false);
            let need_pos = OUR_ACTIVE || candidate_window_visible() || command_line_pending;
            if !need_pos {
                // 通常は250ms待つが、`focus_change_win_event_proc`が起床通知を
                // 送ってきたら即座に戻ってパスワード欄/昇格プロセス判定を
                // やり直す（`Unknown`のフェイルセーフ窓を最小化するため）。
                let _ = wake_rx.recv_timeout(std::time::Duration::from_millis(250));
                continue;
            }
            // command_line_pending だけで need_pos になった（＝コマンド候補が
            // まだ一度も表示されていない）場合は、初回表示までの体感速度を
            // 優先し、この後の通常ループ間隔（150ms）より短い間隔で回す。
            let just_started_typing = command_line_pending && !OUR_ACTIVE && !candidate_window_visible();
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
            // 位置が変わっていなければ安定カウンタを進め、しばらく安定して
            // いたらポーリング間隔を落とす。UIA/AttachConsoleの呼び出し自体
            // （このループの上のuia_focused_anchor/console_caret_screen_pos）
            // は対象アプリのカーソル点滅を乱すことがコード上分かっている
            // （106行目付近のコメント参照）ため、位置がまだ動く可能性が高い
            // 打ち始め・移動直後は素早く追従しつつ、ポップアップの位置が
            // 落ち着いている間はAttachConsole等の呼び出し頻度自体を下げて
            // 点滅への影響を減らす。
            if pos == last_pos {
                stable_count = stable_count.saturating_add(1);
            } else {
                stable_count = 0;
                last_pos = pos;
            }
            let interval_ms = if just_started_typing {
                30
            } else if stable_count >= POSITION_STABLE_THRESHOLD {
                POSITION_STABLE_INTERVAL_MS
            } else {
                150
            };
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
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
        if (s.to_lowercase().contains("terminal") || s.contains("ターミナル"))
            && !is_non_text_control_type(&elem)
        {
            return true;
        }
    }
    false
}

/// アクセシブル名だけで「ターミナル」判定すると、実際には統合ターミナル
/// を"開くボタン"やメニュー項目のように、名前に"terminal"を含むだけの
/// 無関係な要素にフォーカスがあってもコマンドモードへ誤って落ちてしまう
/// （実機報告: WebView2アプリでターミナルパネルの周辺UIにフォーカスが
/// あるだけで変換が一切走らなくなる）。実際のテキスト入力・ターミナル
/// 内容表示に使われそうにないコントロール種別（ボタン・メニュー項目・
/// タブ・ツールバー等）を除外することで、名前ベースの誤検出を減らす
/// （ターミナル本体が使う種別を限定してallowlist化すると、未知の実装
/// パターンを取りこぼす恐れがあるため、denylist方式にしている）。
unsafe fn is_non_text_control_type(elem: &windows::Win32::UI::Accessibility::IUIAutomationElement) -> bool {
    use windows::Win32::UI::Accessibility::{
        UIA_ButtonControlTypeId, UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId,
        UIA_ListItemControlTypeId, UIA_MenuControlTypeId, UIA_MenuItemControlTypeId,
        UIA_TabItemControlTypeId, UIA_ToolBarControlTypeId, UIA_ToolTipControlTypeId,
        UIA_TreeItemControlTypeId,
    };
    let Ok(ct) = elem.CurrentControlType() else {
        return false;
    };
    ct == UIA_ButtonControlTypeId
        || ct == UIA_HyperlinkControlTypeId
        || ct == UIA_ImageControlTypeId
        || ct == UIA_ListItemControlTypeId
        || ct == UIA_MenuControlTypeId
        || ct == UIA_MenuItemControlTypeId
        || ct == UIA_TabItemControlTypeId
        || ct == UIA_ToolBarControlTypeId
        || ct == UIA_ToolTipControlTypeId
        || ct == UIA_TreeItemControlTypeId
}

/// フォーカス中のUIA要素がパスワード欄か（`IsPassword`プロパティ）。
/// モダンなWin32/WPF/WinUI/Chromium系アプリのパスワード欄はこちらで
/// 検出できる。取得に失敗したら（プロパティ未対応・要素なし等）falseに
/// 倒す（既存のUIA失敗時の扱いと同じ方針。この機能は追加の防御層であり、
/// 唯一の防衛線ではないため、誤って過検出するより見逃す方を選ぶ）。
unsafe fn focused_element_is_password(auto: &windows::Win32::UI::Accessibility::IUIAutomation) -> bool {
    let Ok(elem) = auto.GetFocusedElement() else {
        return false;
    };
    elem.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false)
}

/// フォーカス中のウィンドウが古典的なWin32エディットコントロール
/// （`ES_PASSWORD`スタイル）か。UIAを実装しない/していない古いアプリの
/// パスワード欄向けのフォールバック。`GetWindowLongW`は対象がどのスレッド・
/// プロセスに属していてもウィンドウマネージャの構造体を直接読むだけで、
/// `SendMessage`のようなクロススレッドの同期メッセージ配送を伴わないため、
/// ブロックする心配がない。
unsafe fn focused_hwnd_has_es_password() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_STYLE};
    const ES_PASSWORD: i32 = 0x0020;

    let hwnd_fg = GetForegroundWindow();
    if hwnd_fg.0.is_null() {
        return false;
    }
    let tid = GetWindowThreadProcessId(hwnd_fg, None);
    let mut gti = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if GetGUIThreadInfo(tid, &mut gti).is_ok() && !gti.hwndFocus.0.is_null() {
        let style = GetWindowLongW(gti.hwndFocus, GWL_STYLE);
        return (style & ES_PASSWORD) != 0;
    }
    false
}

/// トークンの整合性レベル（Mandatory Integrity Controlの RID 値。例:
/// `SECURITY_MANDATORY_MEDIUM_RID` = 0x2000, `..HIGH_RID` = 0x3000）を取得する。
/// 取得できなければ`None`を返す（呼び出し側は「不明」を「昇格していない」
/// 扱いにする。整合性レベルが取れないケースは通常の非管理者プロセス同士の
/// やり取りでも起こりうるため、失敗のたびに変換を止めると実害の方が大きい）。
unsafe fn process_integrity_level(process: HANDLE) -> Option<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::OpenProcessToken;

    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;

    let mut len: u32 = 0;
    // 1回目はサイズ取得だけが目的の、失敗する前提の呼び出し。
    let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut len);
    if len == 0 {
        let _ = CloseHandle(token);
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    let mut written: u32 = 0;
    let ok = GetTokenInformation(
        token,
        TokenIntegrityLevel,
        Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
        len,
        &mut written,
    )
    .is_ok();
    let _ = CloseHandle(token);
    if !ok {
        return None;
    }

    let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let sid = label.Label.Sid;
    let count = *GetSidSubAuthorityCount(sid);
    if count == 0 {
        return None;
    }
    Some(*GetSidSubAuthority(sid, (count - 1) as u32))
}

/// 自プロセスの整合性レベル。プロセス生存中は変化しないため一度だけ計算して
/// キャッシュする（毎周期`OpenProcessToken`する必要はない）。
fn own_integrity_level() -> Option<u32> {
    static CACHE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| unsafe {
        use windows::Win32::System::Threading::GetCurrentProcess;
        process_integrity_level(GetCurrentProcess())
    })
}

/// フォアグラウンド窓のプロセスが自プロセスより高い整合性レベルを持つか。
/// 双方または片方の整合性レベルが取得できない場合は「昇格していない」
/// （＝従来通りキーを消費する）扱いにする。
unsafe fn foreground_process_is_more_elevated_than_self() -> bool {
    use windows::Win32::Foundation::{CloseHandle, BOOL};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let hwnd_fg = GetForegroundWindow();
    if hwnd_fg.0.is_null() {
        return false;
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd_fg, Some(&mut pid as *mut u32));
    if pid == 0 {
        return false;
    }
    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, BOOL(0), pid) else {
        return false;
    };
    let fg_level = process_integrity_level(process);
    let _ = CloseHandle(process);

    match (fg_level, own_integrity_level()) {
        (Some(fg), Some(own)) => fg > own,
        _ => false,
    }
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
