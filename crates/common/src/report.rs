//! 誤変換レポート: 実機で気づいた誤変換を1キーで記録し、ゴールデンテスト
//! （`crates/hook-dll/src/golden/regressions.tsv`）へ移しやすい形で残す。
//!
//! これまでは「実機で誤変換に気づく → 会話で報告 → 読みを推測して再現」
//! という手作業だった。確定時の読みと表記はエンジンが知っているので、
//! それをそのまま書き出せば再現の手間と推測ミスが無くなる。
//!
//! ファイル: `%LOCALAPPDATA%\IMELiveConverter\misconversion_reports.tsv`
//! （`paths::data_file(REPORT_FILE)`）。1行1件、タブ区切り:
//! `日時(unix秒)  種別  ローマ字  読み  誤った表記  正しい表記  アプリ`
//! - 種別: `manual`（報告ホットキー）/ `restore`（確定後の復元で選び直した）
//! - 正しい表記: `restore`では選び直した表記。`manual`では空（後で人が埋める）
//!
//! トリアージは hook-dll の `triage_misconversion_reports`（ignoredテスト）で、
//! 現在のエンジンで再変換し「まだ誤変換するか」と regressions.tsv 用の行を出す。

use crate::RomajiConverter;

/// レポートのファイル名（ユーザーデータのフォルダ直下）
pub const REPORT_FILE: &str = "misconversion_reports.tsv";

/// レポートファイルを新規作成するときの先頭コメント
pub const REPORT_HEADER: &str = "# 誤変換レポート（IME Live Converter）\n\
# 日時(unix秒)\t種別\tローマ字\t読み\t誤った表記\t正しい表記\tアプリ\n";

/// レポートの種別
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    /// 報告ホットキーで明示的に記録した（正しい表記はまだ不明）
    Manual,
    /// 確定後の復元（変換キー）で別の候補を選び直した
    Restore,
    /// 変換中に候補一覧で表示中以外の候補を選んで確定した
    Candidate,
}

impl ReportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::Manual => "manual",
            ReportKind::Restore => "restore",
            ReportKind::Candidate => "candidate",
        }
    }
}

/// 誤変換レポート1件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MisconversionReport {
    pub unix_time: u64,
    pub kind: ReportKind,
    /// 確定時の読み（ひらがな）
    pub reading: String,
    /// 実際に確定された（誤っていた）表記
    pub wrong: String,
    /// 正しい表記（分かっていれば）
    pub corrected: Option<String>,
    /// 入力先アプリの窓クラス名（再現条件の手がかり）
    pub app: String,
}

impl MisconversionReport {
    /// TSVの1行（末尾改行つき）。タブ・改行はフィールドを壊すので空白に置き換える。
    pub fn to_tsv_line(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\r', '\n'], " ");
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            self.unix_time,
            self.kind.as_str(),
            clean(&kana_to_romaji(&self.reading)),
            clean(&self.reading),
            clean(&self.wrong),
            clean(self.corrected.as_deref().unwrap_or("")),
            clean(&self.app),
        )
    }
}

/// TSVの1行から読み戻したレポート（トリアージ用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReport {
    pub kind: String,
    pub romaji: String,
    pub reading: String,
    pub wrong: String,
    pub corrected: String,
}

/// レポートファイルの内容を読み戻す。コメント行・空行・列数不足の行は飛ばす。
pub fn parse_reports(tsv: &str) -> Vec<ParsedReport> {
    tsv.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 6 {
                return None;
            }
            Some(ParsedReport {
                kind: f[1].to_string(),
                romaji: f[2].to_string(),
                reading: f[3].to_string(),
                wrong: f[4].to_string(),
                corrected: f[5].to_string(),
            })
        })
        .collect()
}

/// ひらがな（カタカナ可）の読みを、このIMEに1文字ずつ打てば同じ読みに
/// なるローマ字へ戻す。ゴールデンテストの入力はローマ字なので、確定時の
/// 読みからテストケースを作るのに使う。
///
/// - ん は常に `nn`（後続が母音・y でも「ん」に確定させるため）
/// - っ は後続の子音を重ねる。重ねられない位置（語末・母音/ん/記号の前）は `xtu`
/// - 記号は `RomajiConverter` の記号表の逆（ー→`-`、。→`.` 等）
/// - かな以外（英数字・漢字）はそのまま残す（再現できないことがある）
pub fn kana_to_romaji(reading: &str) -> String {
    let hira = crate::katakana_to_hiragana(reading);
    let chars: Vec<char> = hira.chars().collect();
    // まず「っ」以外を、モーラ（2文字の拗音を優先）ごとのローマ字列に割る
    let mut units: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'っ' {
            units.push("っ".to_string());
            i += 1;
            continue;
        }
        if i + 1 < chars.len() {
            let two: String = chars[i..i + 2].iter().collect();
            if let Some(r) = romaji_of(&two) {
                units.push(r.to_string());
                i += 2;
                continue;
            }
        }
        let one = chars[i].to_string();
        units.push(romaji_of(&one).map(str::to_string).unwrap_or(one));
        i += 1;
    }
    // 「っ」を後続ユニットの先頭子音の重ねに置き換える
    let mut out = String::new();
    for (k, u) in units.iter().enumerate() {
        if u != "っ" {
            out.push_str(u);
            continue;
        }
        let next_first = units.get(k + 1).and_then(|n| n.chars().next());
        match next_first {
            Some(c) if c.is_ascii_lowercase() && !"aiueon".contains(c) => out.push(c),
            _ => out.push_str("xtu"),
        }
    }
    out
}

/// かな1〜2文字に対する正準のローマ字。`RomajiConverter`の対応表から
/// 「最も素直な綴り」を選んだもの（最初の1回だけ組み立てる）。
fn romaji_of(kana: &str) -> Option<&'static str> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let conv = RomajiConverter::new();
            let mut best: HashMap<&'static str, &'static str> = HashMap::new();
            for (&romaji, &kana) in conv.mapping_entries() {
                // 「nn+母音」は「ん」+次のかなの合成なので、逆引きには使わない
                if romaji.starts_with("nn") && romaji.len() > 2 {
                    continue;
                }
                let better = match best.get(kana) {
                    None => true,
                    Some(&cur) => spelling_rank(romaji) < spelling_rank(cur),
                };
                if better {
                    best.insert(kana, romaji);
                }
            }
            best
        })
        .get(kana)
        .copied()
}

/// 綴りの好ましさ（小さいほど良い）。ヘボン式寄りの一般的な綴りを優先し、
/// c/q/l/x/wh 始まりの代替綴りは避ける。同点なら短い方、さらに辞書順。
fn spelling_rank(romaji: &str) -> (u8, usize, &str) {
    let preferred = ["shi", "chi", "tsu", "fu", "ji", "sha", "shu", "sho", "cha", "chu", "cho", "ja", "ju", "jo"];
    let penalty = if preferred.contains(&romaji) {
        0
    } else if romaji.starts_with(['c', 'q', 'l']) || romaji.starts_with("wh") {
        3
    } else if romaji.starts_with('x') {
        2
    } else {
        1
    };
    (penalty, romaji.len(), romaji)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実際の打鍵（1文字ずつ入力し、確定したかなだけを送り出す）と同じ
    /// 方法でローマ字をかなに戻す。`LiveConversionState::add_char`の分割と同じ。
    fn type_incrementally(romaji: &str) -> String {
        let conv = RomajiConverter::new();
        let mut out = String::new();
        let mut buf = String::new();
        for ch in romaji.chars() {
            buf.push(ch);
            let (settled, pending) = conv.split(&buf);
            out.push_str(&settled);
            buf = pending;
        }
        out.push_str(&conv.convert(&buf));
        out
    }

    #[test]
    fn kana_to_romaji_round_trips_through_incremental_typing() {
        for reading in [
            "いっかんせい", "かいしゃにいった", "さいきどうする", "ぶらうざをひらく",
            "こんにちは", "きんえん", "たんい", "ほんや", "がっこう", "まっちゃ",
            "ちょっと", "あっ", "らーめん、たべた。", "ふぉるだ", "てぃーしゃつ",
            "じゅんび", "しゅくだい", "つづく", "ぢゃ", "ゔぁいおりん", "えっ？", "かっこ「」",
            "ブラウザ",
        ] {
            let romaji = kana_to_romaji(reading);
            assert_eq!(
                type_incrementally(&romaji),
                crate::katakana_to_hiragana(reading),
                "{reading} -> {romaji} が同じ読みに戻らない"
            );
        }
    }

    #[test]
    fn kana_to_romaji_uses_plain_spellings() {
        assert_eq!(kana_to_romaji("いっかんせい"), "ikkannsei");
        assert_eq!(kana_to_romaji("しゃしん"), "shashinn");
        assert_eq!(kana_to_romaji("まっちゃ"), "maccha");
    }

    #[test]
    fn report_line_round_trips_through_parser() {
        let r = MisconversionReport {
            unix_time: 1,
            kind: ReportKind::Restore,
            reading: "どうかしましたか".into(),
            wrong: "同化しましたか".into(),
            corrected: Some("どうかしましたか".into()),
            app: "Chrome_WidgetWin_1".into(),
        };
        let parsed = parse_reports(&format!("{REPORT_HEADER}{}", r.to_tsv_line()));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, "restore");
        assert_eq!(parsed[0].romaji, "doukashimashitaka");
        assert_eq!(parsed[0].wrong, "同化しましたか");
        assert_eq!(parsed[0].corrected, "どうかしましたか");
    }
}
