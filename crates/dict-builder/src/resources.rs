//! dict-builder の CPU の使い方（作業スレッド数とプロセス優先度）
//!
//! Wikipedia 全量の抽出・学習は数十分 CPU を回し続けるため、既定では
//! - 作業スレッドを論理コア数の半分に抑え（CPU 全体の約半分。従来はコア数−1 で
//!   75〜85% を使い続けていた）、
//! - プロセス優先度を「通常以下」に下げて、他のアプリの操作を妨げない
//! ようにする。急ぐときは環境変数で戻せる:
//! - `DICT_BUILDER_THREADS=<数>`: 作業スレッド数（メインスレッドは別に1つ）
//! - `DICT_BUILDER_PRIORITY=normal`: 優先度を下げない

/// 作業スレッド数（`DICT_BUILDER_THREADS` が無ければ論理コア数の半分、最低1）
pub fn worker_threads() -> usize {
    if let Some(n) = std::env::var("DICT_BUILDER_THREADS").ok().and_then(|s| s.parse::<usize>().ok()) {
        return n.max(1);
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    (cores / 2).max(1)
}

/// プロセス優先度を「通常以下」に下げる（`DICT_BUILDER_PRIORITY=normal` なら何もしない）
pub fn lower_priority() {
    if std::env::var("DICT_BUILDER_PRIORITY").is_ok_and(|v| v.eq_ignore_ascii_case("normal")) {
        return;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS};
        SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}
