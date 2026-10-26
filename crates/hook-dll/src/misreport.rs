//! 誤変換レポートの書き出し（形式は `common::report`）。
//!
//! フック内でディスクI/Oをすると`LowLevelHooksTimeout`を超えて生キーが漏れる
//! （[[hook-latency-limit]]）。`WM_APP_*`への委譲もフックと同じスレッドで
//! 動くため不十分（`hook_latency`のコメント参照）。そこで`hook_latency`と同じく
//! 専用のバックグラウンドスレッドへ`mpsc`で渡し、追記はそちらで行う。
//! フック側で行うのはレポートの組み立て（文字列のクローン）と送信だけ。

use crate::*;
use common::report::{MisconversionReport, ReportKind, REPORT_FILE, REPORT_HEADER};
use std::io::Write;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;

static REPORT_TX: OnceLock<Sender<MisconversionReport>> = OnceLock::new();

fn sender() -> &'static Sender<MisconversionReport> {
    REPORT_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<MisconversionReport>();
        std::thread::spawn(move || {
            while let Ok(report) = rx.recv() {
                if let Err(e) = append(&report) {
                    eprintln!("誤変換レポートの書き込みに失敗: {e}");
                }
            }
        });
        tx
    })
}

fn append(report: &MisconversionReport) -> std::io::Result<()> {
    let path = common::paths::data_file(REPORT_FILE);
    let is_new = !path.exists();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    if is_new {
        f.write_all(REPORT_HEADER.as_bytes())?;
    }
    f.write_all(report.to_tsv_line().as_bytes())
}

/// レポートを1件記録する（フックから呼んでよい。I/Oは別スレッド）。
/// 読みか誤った表記が空なら何もしない。
pub(crate) fn submit(kind: ReportKind, reading: &str, wrong: &str, corrected: Option<&str>) {
    if reading.is_empty() || wrong.is_empty() {
        return;
    }
    let unix_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let report = MisconversionReport {
        unix_time,
        kind,
        reading: reading.to_string(),
        wrong: wrong.to_string(),
        corrected: corrected.map(str::to_string),
        app: crate::command_mode::foreground_class_name(),
    };
    debug_log!("誤変換レポート: {:?}", report);
    let _ = sender().send(report);
}

/// 変換中に候補一覧で誤変換を直して確定したときの記録
/// （`LiveConversionState::correction_reporter`に差し込む）。
pub(crate) fn submit_candidate_fix(reading: &str, wrong: &str, corrected: &str) {
    submit(ReportKind::Candidate, reading, wrong, Some(corrected));
}
