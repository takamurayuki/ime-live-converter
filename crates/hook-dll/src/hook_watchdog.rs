//! フックの見張り番（ウォッチドッグ）とメモリの常駐化。
//!
//! 背景（2026-10-06 実機で発生）: 判断層のモデル（数百MB）を読み込んだ状態で
//! 長時間放置すると、Windows がプロセスのワーキングセットを削ってモデルを
//! ページアウトする。次の打鍵で変換がモデルをディスクから読み戻すと
//! `LowLevelKeyboardProc` が LowLevelHooksTimeout（既定300ms）を超え、
//! Windows 7 以降はそのフックを**黙って外す**。以後キーは素通りになり、
//! 「IME は動いているのに変換が一切効かない」状態になる（プロセスは生きている
//! ので気づく手段が無かった。`hook_thread.rs` の TODO）。
//!
//! 対策は2段:
//! 1. 予防: 実際に使っているワーキングセットに下限（ハード最小値）を設け、
//!    モデルがページアウトされないようにする（`raise_working_set_floor`）。
//! 2. 復旧: 「入力があったのにキーボードフックもマウスフックも呼ばれていない」
//!    ことを検出したらキーボードフックを張り直す（`run`）。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Memory::{
    SetProcessWorkingSetSizeEx, QUOTA_LIMITS_HARDWS_MAX_DISABLE, QUOTA_LIMITS_HARDWS_MIN_ENABLE,
};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

/// キーボードフックのコールバックが最後に呼ばれた時刻（GetTickCount）
static LAST_HOOK_TICK: AtomicU32 = AtomicU32::new(0);
/// マウスフックのコールバックが最後に呼ばれた時刻（入力がマウスだったかの判定用）
static LAST_MOUSE_TICK: AtomicU32 = AtomicU32::new(0);
/// ウォッチドッグを止める要求
static STOP: AtomicBool = AtomicBool::new(false);
/// フックを張り直した回数（調査用）
pub(crate) static REINSTALL_COUNT: AtomicU32 = AtomicU32::new(0);

/// 見回りの間隔
const CHECK_INTERVAL: Duration = Duration::from_millis(1000);
/// 入力からこの時間たってから判定する（フックが処理中の場合を待つ）
const SETTLE_MS: u32 = 500;
/// 入力時刻とフック呼び出し時刻の許容差（GetTickCount の分解能約15.6ms＋余裕）
const SAME_EVENT_MS: u32 = 250;
/// 張り直しの最短間隔（マウスホイール等を打鍵と誤認したときの空振りを抑える）
const REINSTALL_MIN_INTERVAL_MS: u32 = 5000;
/// ワーキングセットの下限の上限（これ以上は確保しない）
const FLOOR_CAP_BYTES: usize = 2 << 30;
/// 下限は「いま使っている量＋この余裕」に引き上げる
const FLOOR_MARGIN_BYTES: usize = 64 << 20;

/// キーボードフックのコールバック入口で呼ぶ（アトミックな書き込みだけ）
#[inline]
pub(crate) fn note_hook_called() {
    LAST_HOOK_TICK.store(unsafe { GetTickCount() }, Ordering::Relaxed);
}

/// マウスフックのコールバック入口で呼ぶ（アトミックな書き込みだけ）
#[inline]
pub(crate) fn note_mouse_called() {
    LAST_MOUSE_TICK.store(unsafe { GetTickCount() }, Ordering::Relaxed);
}

/// 見張り番の出来事を `hook_watchdog.log` に残す（時刻と回数だけ。キーの内容は
/// 一切書かない。フックが外れた・張り直した事実を後から確認できるように）
fn log_event(msg: &str) {
    use std::io::Write;
    let path = "C:\\Projects\\ime-live-converter\\hook_watchdog.log";
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {}", chrono_like_now(), msg);
    }
}

/// ローカル時刻の簡易表記（依存を増やさないため SystemTime から秒まで）
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix={}", secs)
}

/// 見張り番スレッドを起動する（`HookThread::start` から）
pub(crate) fn start() -> Option<JoinHandle<()>> {
    STOP.store(false, Ordering::Release);
    note_hook_called();
    thread::Builder::new().name("ime-hook-watchdog".into()).spawn(run).ok()
}

/// 見張り番スレッドに停止を要求する（join は呼び出し側）
pub(crate) fn request_stop() {
    STOP.store(true, Ordering::Release);
}

fn last_input_tick() -> Option<u32> {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe { GetLastInputInfo(&mut info) }.as_bool().then_some(info.dwTime)
}

/// a が b より後か（GetTickCount の約49.7日周期の折り返しを考慮）
fn after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// 入力 `input` のあと（許容差 `SAME_EVENT_MS` 込み）にキーボード／マウスの
/// どちらかのフックが呼ばれていれば、その入力はフックに届いている
fn input_reached_hooks(input: u32, keyboard_tick: u32, mouse_tick: u32) -> bool {
    let reached = |tick: u32| !after(input, tick.wrapping_add(SAME_EVENT_MS));
    reached(keyboard_tick) || reached(mouse_tick)
}

fn run() {
    let mut handled_input = last_input_tick().unwrap_or(0);
    let mut last_reinstall: Option<u32> = None;
    let mut floor = 0usize;
    // 動作確認用: IME_WATCHDOG_SELFTEST=1 なら起動20秒後に1回だけフックを外し、
    // Windows に黙って外された状態を再現する（張り直されるかをログで確かめる）
    let mut selftest_drop_in: Option<u32> = std::env::var("IME_WATCHDOG_SELFTEST")
        .is_ok_and(|v| v == "1")
        .then_some(20);
    log_event("見張り番を開始しました");
    while !STOP.load(Ordering::Acquire) {
        thread::sleep(CHECK_INTERVAL);
        if STOP.load(Ordering::Acquire) {
            break;
        }
        floor = raise_working_set_floor(floor);
        if let Some(n) = selftest_drop_in.as_mut() {
            *n -= 1;
            if *n == 0 {
                selftest_drop_in = None;
                if crate::hook_thread::HookThread::request_unhook_for_selftest() {
                    log_event("自己テスト: フックを外しました（張り直しを待ちます）");
                }
            }
        }

        let Some(input) = last_input_tick() else { continue };
        let now = unsafe { GetTickCount() };
        // 新しい入力があり、フックが処理し終えるだけの時間がたったら判定する。
        // OS は入力をフックに通してから配送するので、フックが生きていれば
        // その時刻以降にキーボードかマウスのフックが必ず呼ばれている。
        if !after(input, handled_input) || now.wrapping_sub(input) < SETTLE_MS {
            continue;
        }
        handled_input = input;
        let kb = LAST_HOOK_TICK.load(Ordering::Relaxed);
        let mouse = LAST_MOUSE_TICK.load(Ordering::Relaxed);
        if input_reached_hooks(input, kb, mouse) {
            continue;
        }
        let rate_ok = last_reinstall.map_or(true, |t| now.wrapping_sub(t) >= REINSTALL_MIN_INTERVAL_MS);
        if rate_ok && crate::hook_thread::HookThread::request_reinstall() {
            last_reinstall = Some(now);
            let n = REINSTALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            log_event(&format!(
                "入力がフックに届いていないため張り直しました（{}回目、キーボードフックの最終呼び出しから{}ms）",
                n,
                now.wrapping_sub(kb)
            ));
        }
    }
}

/// プロセスのワーキングセットに下限（ハード最小値）を設け、使っている量が
/// 増えたら追従して引き上げる。モデルのページアウトによるフックのタイムアウトを
/// 防ぐ。権限等で失敗しても何もしない（ウォッチドッグの張り直しで復旧はできる）。
/// 戻り値は現在の下限。
fn raise_working_set_floor(current_floor: usize) -> usize {
    let mut pmc = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    let process = unsafe { GetCurrentProcess() };
    if unsafe { GetProcessMemoryInfo(process, &mut pmc, pmc.cb) }.is_err() {
        return current_floor;
    }
    let want = (pmc.WorkingSetSize + FLOOR_MARGIN_BYTES).min(FLOOR_CAP_BYTES);
    // 64MB以上増えたときだけ設定し直す（毎秒の呼び出しを避ける）
    if want <= current_floor + FLOOR_MARGIN_BYTES && current_floor != 0 {
        return current_floor;
    }
    let max = want.saturating_mul(2).max(want + (256 << 20));
    let ok = unsafe {
        SetProcessWorkingSetSizeEx(process, want, max, QUOTA_LIMITS_HARDWS_MIN_ENABLE | QUOTA_LIMITS_HARDWS_MAX_DISABLE)
    }
    .is_ok();
    log_event(&format!(
        "ワーキングセットの下限を {}MB に設定: {}",
        want >> 20,
        if ok { "成功" } else { "失敗" }
    ));
    if ok {
        want
    } else {
        current_floor.max(want) // 失敗しても毎秒再試行しない
    }
}

#[cfg(test)]
mod tests {
    use super::{after, input_reached_hooks};

    #[test]
    fn tick_comparison_handles_wraparound() {
        assert!(after(10, 5));
        assert!(!after(5, 10));
        assert!(after(3, u32::MAX - 2)); // 折り返し後
    }

    #[test]
    fn detects_input_that_no_hook_saw() {
        // キーボードフックが入力と同時刻に呼ばれている → 届いている
        assert!(input_reached_hooks(10_000, 10_000, 0));
        // GetTickCount の分解能ぶんの前後ずれは同じ入力とみなす
        assert!(input_reached_hooks(10_000, 9_900, 0));
        // マウス入力ならマウスフックが呼ばれている
        assert!(input_reached_hooks(10_000, 1_000, 10_016));
        // どちらのフックも入力より前で止まっている → 外れている
        assert!(!input_reached_hooks(10_000, 5_000, 5_000));
    }
}
