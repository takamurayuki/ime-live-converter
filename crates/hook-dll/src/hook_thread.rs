//! フック専用スレッド（WH_KEYBOARD_LL用）。
//!
//! 目的: `LowLevelKeyboardProc`（`crates/hook-dll/src/hook.rs`）の配送を、
//! 候補ウィンドウの描画・SQLite書き込みなどの同期処理から完全に切り離す。
//! 低レベルフックのコールバックは「SetWindowsHookExWを呼んだスレッド」の
//! メッセージ取得中にOSから配送されるため、そのスレッドのメッセージ
//! ループに他の仕事を一切置かないことがそのままLowLevelHooksTimeout
//! （既定300ms）対策になる（[[hook-latency-log-thread-model]]で確認した
//! 問題への根本対応）。
//!
//! 重要: 候補ウィンドウはこのスレッドで作らない。従来どおりUIスレッド
//! （`conversion-service`のメインスレッド、`install_hook`の呼び出し元）が
//! 所有したままにする（ウィンドウはスレッドアフィニティを持つため、移動
//! させようとすると作り直しになる。その必要はない）。フックスレッドが
//! 候補ウィンドウを操作する必要があるときは、`crate::popup::post_ui_command`
//! で指示を送るだけにする（実際のWin32呼び出しはUIスレッド自身が行う。
//! `SendMessageW`/`ShowWindow`/`SetWindowPos`/`DestroyWindow`等を他スレッド
//! 所有のウィンドウへ直接呼ぶと、内部でクロススレッドのマーシャリングが
//! 発生してブロックし、分離した意味が消える）。
//!
//! `LowLevelKeyboardProc`自体（catch_unwindによるpanic対策込み）は
//! `hook.rs`に残したまま、ここではそれを登録するスレッドの生成・
//! 生存管理・終了処理だけを担う。

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HINSTANCE, LPARAM, WPARAM};
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK,
    MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_QUIT, WM_USER,
};

/// フックスレッドへの制御メッセージ。停止はWM_QUITを使うのでここには含めない。
const WM_HOOK_REINSTALL: u32 = WM_USER + 1;
/// 動作確認用: フックを外すだけ（`hook_watchdog` の自己テスト）
const WM_HOOK_UNHOOK_SELFTEST: u32 = WM_USER + 2;

thread_local! {
    /// HHOODはフックスレッドからしか触らないのでthread_localで持つ。
    /// 他スレッドからUnhookWindowsHookExを呼ばないための構造的な担保でもある。
    static HOOK_HANDLE: Cell<Option<HHOOK>> = const { Cell::new(None) };
}

/// フックスレッドが生きているか。ウォッチドッグ（UI側の定期チェック）から参照する。
/// ポンプを抜けた場合もpanicした場合もDropガードで false になる。
static HOOK_ALIVE: AtomicBool = AtomicBool::new(false);

/// PostThreadMessageWの宛先。0は未起動を意味する。
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// 生存フラグを必ず倒すためのガード。panic unwindでもDropは走る
/// （このスレッドの最上位はpump()の中で完結しており、`LowLevelKeyboardProc`
/// 自体は既にhook.rs側でcatch_unwind済みなのでここまでpanicが上がってくる
/// ことは想定していないが、二重の安全網として置く）。
struct AliveGuard;

impl Drop for AliveGuard {
    fn drop(&mut self) {
        HOOK_ALIVE.store(false, Ordering::Release);
        HOOK_THREAD_ID.store(0, Ordering::Release);
    }
}

pub(crate) struct HookThread {
    join: Option<JoinHandle<()>>,
    /// フックが外されていないか見張るスレッド（`hook_watchdog`）
    watchdog: Option<JoinHandle<()>>,
}

impl HookThread {
    /// フックスレッドを起動する。フックのインストール成否を待ってから返るので、
    /// 呼び出し側は「起動したが実は失敗していた」状態を掴まされない。
    pub(crate) fn start() -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let join = thread::Builder::new()
            .name("ime-hook".into())
            .spawn(move || {
                // ポンプ突入前でも抜けた後でも生存フラグが正しくなるよう最初に置く。
                let _guard = AliveGuard;

                // 負荷時のスケジューリング待ちを減らす。安価なので入れておく。
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
                }

                // PostThreadMessageWはスレッドにメッセージキューが無いと失敗する。
                // キューはユーザー系メッセージに初めて触れた時点で作られるので、
                // readyを返す前にPeekMessageで強制的に作っておく。これを省くと
                // 「起動直後の停止要求だけが失敗する」という再現しにくいバグになる。
                unsafe {
                    let mut msg = MSG::default();
                    let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
                }

                let tid = unsafe { GetCurrentThreadId() };
                HOOK_THREAD_ID.store(tid, Ordering::Release);

                match install_hook_on_this_thread() {
                    Ok(()) => {
                        HOOK_ALIVE.store(true, Ordering::Release);
                        let _ = ready_tx.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return; // ポンプに入らず終了。_guardがフラグを倒す。
                    }
                }

                pump();

                // ポンプを抜けたら、必ず同じスレッドでアンフックする。
                uninstall_hook_on_this_thread();
            })
            .map_err(|e| format!("フックスレッドの起動に失敗: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { join: Some(join), watchdog: crate::hook_watchdog::start() }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                Err("フックスレッドが初期化中に異常終了しました".into())
            }
        }
    }

    /// ウォッチドッグ用。falseなら再起動を検討する。
    /// TODO: UIスレッド側から定期的にこれを見る仕組み（タイマー等）を
    /// まだ配線していない。フックスレッドがpanicで死んだ場合、現状は
    /// 「変換が一切効かないが、アプリは動いている」ことに気づく手段が無い
    /// （`LowLevelKeyboardProc`自体はcatch_unwind済みなので通常はここへ
    /// 来ないはずだが、二重の安全網として重要）。
    #[allow(dead_code)]
    pub(crate) fn is_alive() -> bool {
        HOOK_ALIVE.load(Ordering::Acquire)
    }

    /// フックだけを張り直す（外部要因でフックが外された場合の復旧。
    /// `hook_watchdog` が「入力がフックに届いていない」ことを検出したときに呼ぶ）。
    pub(crate) fn request_reinstall() -> bool {
        post(WM_HOOK_REINSTALL)
    }

    /// 動作確認用: フックを外すだけ（Windows に黙って外された状態の再現）
    pub(crate) fn request_unhook_for_selftest() -> bool {
        post(WM_HOOK_UNHOOK_SELFTEST)
    }

    /// 停止してjoinする。Dropからも呼ばれる。
    pub(crate) fn stop(&mut self) {
        if let Some(w) = self.watchdog.take() {
            crate::hook_watchdog::request_stop();
            let _ = w.join();
        }
        if let Some(join) = self.join.take() {
            // WM_QUITを投げてポンプを抜けさせる。アンフックはスレッド側で行う。
            post(WM_QUIT);
            let _ = join.join();
        }
    }
}

impl Drop for HookThread {
    fn drop(&mut self) {
        self.stop();
    }
}

fn post(msg: u32) -> bool {
    let tid = HOOK_THREAD_ID.load(Ordering::Acquire);
    if tid == 0 {
        return false;
    }
    unsafe { PostThreadMessageW(tid, msg, WPARAM(0), LPARAM(0)).is_ok() }
}

fn install_hook_on_this_thread() -> Result<(), String> {
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(crate::hook::LowLevelKeyboardProc),
            HINSTANCE::default(),
            0,
        )
    }
    .map_err(|e| format!("SetWindowsHookExW に失敗: {e}"))?;

    HOOK_HANDLE.with(|h| h.set(Some(hook)));
    Ok(())
}

fn uninstall_hook_on_this_thread() {
    HOOK_HANDLE.with(|h| {
        if let Some(hook) = h.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
        }
    });
}

/// 最小メッセージポンプ。ここに仕事を足さないこと。
/// このスレッドはウィンドウを持たないのでTranslateMessage/DispatchMessageWは不要。
fn pump() {
    let mut msg = MSG::default();
    loop {
        // GetMessageWはWM_QUITで0、エラーで-1を返す。BOOLの真偽だけで判定すると
        // -1を「真」として無限ループする。
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        match r.0 {
            0 => break,  // WM_QUIT
            -1 => break, // エラー
            _ => {}
        }

        // フックのコールバック自体はGetMessageWの内部でOSから直接呼ばれるため、
        // ここに現れるのは自前の制御メッセージだけ。
        if msg.message == WM_HOOK_UNHOOK_SELFTEST {
            uninstall_hook_on_this_thread();
        }
        if msg.message == WM_HOOK_REINSTALL {
            uninstall_hook_on_this_thread();
            if install_hook_on_this_thread().is_err() {
                HOOK_ALIVE.store(false, Ordering::Release);
                break;
            }
        }
    }
}
