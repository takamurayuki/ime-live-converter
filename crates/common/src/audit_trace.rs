//! 変換パイプラインの段階別監査ログ（入力→前処理→エンジン選定→出力）
//!
//! 1回の変換ごとに `ConversionTrace` を1件作り、その中に段階ごとの
//! `StageRecord`（入出力・所要時間・適用ルール）を並べる。CLI と hook-dll の
//! どちらからも同じ構造で記録できるよう、型だけを common に置く。
//! 収集はメモリ上のみで、ファイルへの永続化は明示的なエクスポート操作でしか行わない。

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// リングバッファに保持する直近トレース数の上限
pub const TRACE_BUFFER_CAPACITY: usize = 50;

/// パイプラインの段階
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StageKind {
    /// 生キーイベント → ローマ字バッファ
    Input,
    /// ローマ字 → ひらがな（保留ローマ字の分割）
    Preprocess,
    /// ひらがな → 変換結果（文脈Viterbi）
    EngineSelect,
    /// 変換結果 → 差分計算・アプリ送信
    Output,
}

impl StageKind {
    /// 画面表示・コピー用の日本語ラベル
    pub fn label(self) -> &'static str {
        match self {
            StageKind::Input => "入力",
            StageKind::Preprocess => "前処理",
            StageKind::EngineSelect => "エンジン選定",
            StageKind::Output => "出力",
        }
    }
}

/// 1段階分の記録
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageRecord {
    pub stage: StageKind,
    pub input: String,
    pub output: String,
    pub duration_micros: u64,
    pub applied_rule: Option<String>,
}

impl StageRecord {
    /// フィードバックに貼り付けられる1段階分の整形テキスト
    pub fn to_report_line(&self, step_no: usize) -> String {
        format!(
            "Step{} [{}] 入力='{}' 出力='{}' 所要時間={}µs 適用ルール={}",
            step_no,
            self.stage.label(),
            self.input,
            self.output,
            self.duration_micros,
            self.applied_rule.as_deref().unwrap_or("-"),
        )
    }
}

/// 1回の変換（キー入力1回に対する更新）のトレース
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionTrace {
    /// 開始時刻（UNIXエポックからのミリ秒。`SystemTime` を直列化可能にしたもの）
    pub started_at_unix_millis: u64,
    pub stages: Vec<StageRecord>,
}

impl ConversionTrace {
    pub fn new(started_at: SystemTime) -> Self {
        let started_at_unix_millis = started_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            started_at_unix_millis,
            stages: Vec::with_capacity(4),
        }
    }

    /// 全段階の所要時間合計（µs）
    pub fn total_micros(&self) -> u64 {
        self.stages.iter().map(|s| s.duration_micros).sum()
    }

    /// フィードバックに貼り付けられる全段階の整形テキスト
    pub fn to_report_text(&self) -> String {
        let mut out = format!(
            "--- 変換トレース (started_at_unix_millis={}, 合計={}µs) ---\n",
            self.started_at_unix_millis,
            self.total_micros()
        );
        for (i, s) in self.stages.iter().enumerate() {
            out.push_str(&s.to_report_line(i + 1));
            out.push('\n');
        }
        out
    }
}

/// 直近 `TRACE_BUFFER_CAPACITY` 件のトレースを保持するリングバッファ
#[derive(Debug, Default)]
pub struct TraceBuffer {
    traces: VecDeque<ConversionTrace>,
}

impl TraceBuffer {
    pub fn new() -> Self {
        Self {
            traces: VecDeque::with_capacity(TRACE_BUFFER_CAPACITY),
        }
    }

    /// 末尾に追加し、上限を超えたら最古を破棄する
    pub fn push(&mut self, trace: ConversionTrace) {
        if self.traces.len() >= TRACE_BUFFER_CAPACITY {
            self.traces.pop_front();
        }
        self.traces.push_back(trace);
    }

    pub fn len(&self) -> usize {
        self.traces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ConversionTrace> {
        self.traces.iter()
    }

    pub fn front(&self) -> Option<&ConversionTrace> {
        self.traces.front()
    }

    pub fn back(&self) -> Option<&ConversionTrace> {
        self.traces.back()
    }

    /// UI表示・エクスポート用のスナップショット（古い順）
    pub fn snapshot(&self) -> Vec<ConversionTrace> {
        self.traces.iter().cloned().collect()
    }
}

/// トレース列をエクスポート用の整形JSONにする
pub fn traces_to_json(traces: &[ConversionTrace]) -> String {
    serde_json::to_string_pretty(traces).unwrap_or_else(|_| "[]".to_string())
}

/// エクスポートしたJSONを読み戻す（エクスポート結果の検証用）
pub fn traces_from_json(json: &str) -> Result<Vec<ConversionTrace>, serde_json::Error> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_trace() -> ConversionTrace {
        let mut t = ConversionTrace::new(SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1234));
        t.stages.push(StageRecord {
            stage: StageKind::Input,
            input: "u".to_string(),
            output: "kyou".to_string(),
            duration_micros: 1,
            applied_rule: None,
        });
        t.stages.push(StageRecord {
            stage: StageKind::Preprocess,
            input: "kyou".to_string(),
            output: "きょう".to_string(),
            duration_micros: 2,
            applied_rule: Some("romaji_split".to_string()),
        });
        t.stages.push(StageRecord {
            stage: StageKind::EngineSelect,
            input: "きょう".to_string(),
            output: "今日".to_string(),
            duration_micros: 300,
            applied_rule: Some("context_viterbi".to_string()),
        });
        t.stages.push(StageRecord {
            stage: StageKind::Output,
            input: "今日".to_string(),
            output: "delete=0 insert='今日'".to_string(),
            duration_micros: 4,
            applied_rule: Some("common_prefix_diff".to_string()),
        });
        t
    }

    #[test]
    fn serializes_to_json_with_expected_keys() {
        let t = sample_trace();
        let json = serde_json::to_string(&t).unwrap();
        for key in ["stage", "input", "output", "duration_micros", "applied_rule", "started_at_unix_millis", "stages"] {
            assert!(json.contains(&format!("\"{}\"", key)), "missing key {}: {}", key, json);
        }
        assert!(json.contains("\"EngineSelect\""));
        assert!(json.contains("\"started_at_unix_millis\":1234"));
        // 再パース可能
        let back: ConversionTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn traces_to_json_is_reparsable_array() {
        let json = traces_to_json(&[sample_trace(), sample_trace()]);
        let back: Vec<ConversionTrace> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].stages.len(), 4);
    }

    #[test]
    fn buffer_keeps_only_latest_50() {
        let mut buf = TraceBuffer::new();
        for i in 0..(TRACE_BUFFER_CAPACITY + 1) {
            let mut t = sample_trace();
            t.started_at_unix_millis = i as u64;
            buf.push(t);
        }
        assert_eq!(buf.len(), TRACE_BUFFER_CAPACITY);
        // 51回追記後、先頭は2番目（index 1）のトレースになっている
        assert_eq!(buf.front().unwrap().started_at_unix_millis, 1);
        assert_eq!(buf.back().unwrap().started_at_unix_millis, TRACE_BUFFER_CAPACITY as u64);
    }

    #[test]
    fn report_text_contains_every_stage() {
        let text = sample_trace().to_report_text();
        for label in ["入力", "前処理", "エンジン選定", "出力"] {
            assert!(text.contains(&format!("[{}]", label)), "{}", text);
        }
        assert!(text.contains("Step3"));
        assert!(text.contains("合計=307µs"));
    }
}
