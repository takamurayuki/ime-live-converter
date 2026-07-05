pub mod dictionary;
pub mod viterbi;
pub mod serialization;
pub mod candidate;
pub mod composition;
pub mod typo;
pub mod converter;
pub mod learning;
pub mod rerank;
pub mod llm;

pub use dictionary::{Dictionary, WordEntry, TrieNode, ConnectionMatrix, CharCategory, PosId};
pub use viterbi::{ViterbiConverter, LiveConversionContext, Lattice, LatticeNode, IncrementalViterbi};
pub use candidate::{Candidate, CandidateKind, hiragana_to_katakana, katakana_to_hiragana};
pub use composition::{CompositionState, InputContext, should_auto_convert};
pub use typo::{TypoCorrector, TypoCandidate};
pub use converter::{LiveConverter, ConversionResult};
pub use learning::{LearningRepository, UserDictEntry, ConversionHistoryEntry};
pub use rerank::{CandidateReranker, NoopReranker};
pub use llm::{llm_convert, llm_convert_with, llm_correct, llm_rerank, warm_up, LlmBackend, LlmConfig, OllamaBackend};

use std::collections::HashMap;

/// ローマ字→ひらがな変換マッピング
pub struct RomajiConverter {
    mapping: HashMap<&'static str, &'static str>,
}

impl RomajiConverter {
    pub fn new() -> Self {
        let mut mapping = HashMap::new();
        
        // 基本的な変換ルール
        // あ行
        mapping.insert("a", "あ");
        mapping.insert("i", "い");
        mapping.insert("u", "う");
        mapping.insert("e", "え");
        mapping.insert("o", "お");
        
        // か行
        mapping.insert("ka", "か");
        mapping.insert("ki", "き");
        mapping.insert("ku", "く");
        mapping.insert("ke", "け");
        mapping.insert("ko", "こ");
        
        // が行
        mapping.insert("ga", "が");
        mapping.insert("gi", "ぎ");
        mapping.insert("gu", "ぐ");
        mapping.insert("ge", "げ");
        mapping.insert("go", "ご");
        
        // さ行
        mapping.insert("sa", "さ");
        mapping.insert("si", "し");
        mapping.insert("shi", "し");
        mapping.insert("su", "す");
        mapping.insert("se", "せ");
        mapping.insert("so", "そ");
        
        // ざ行
        mapping.insert("za", "ざ");
        mapping.insert("zi", "じ");
        mapping.insert("ji", "じ");
        mapping.insert("zu", "ず");
        mapping.insert("ze", "ぜ");
        mapping.insert("zo", "ぞ");
        
        // た行
        mapping.insert("ta", "た");
        mapping.insert("ti", "ち");
        mapping.insert("chi", "ち");
        mapping.insert("tu", "つ");
        mapping.insert("tsu", "つ");
        mapping.insert("te", "て");
        mapping.insert("to", "と");
        
        // だ行
        mapping.insert("da", "だ");
        mapping.insert("di", "ぢ");
        mapping.insert("du", "づ");
        mapping.insert("de", "で");
        mapping.insert("do", "ど");
        
        // な行
        mapping.insert("na", "な");
        mapping.insert("ni", "に");
        mapping.insert("nu", "ぬ");
        mapping.insert("ne", "ね");
        mapping.insert("no", "の");
        
        // は行
        mapping.insert("ha", "は");
        mapping.insert("hi", "ひ");
        mapping.insert("hu", "ふ");
        mapping.insert("fu", "ふ");
        mapping.insert("he", "へ");
        mapping.insert("ho", "ほ");
        
        // ば行
        mapping.insert("ba", "ば");
        mapping.insert("bi", "び");
        mapping.insert("bu", "ぶ");
        mapping.insert("be", "べ");
        mapping.insert("bo", "ぼ");
        
        // ぱ行
        mapping.insert("pa", "ぱ");
        mapping.insert("pi", "ぴ");
        mapping.insert("pu", "ぷ");
        mapping.insert("pe", "ぺ");
        mapping.insert("po", "ぽ");
        
        // ま行
        mapping.insert("ma", "ま");
        mapping.insert("mi", "み");
        mapping.insert("mu", "む");
        mapping.insert("me", "め");
        mapping.insert("mo", "も");
        
        // や行
        mapping.insert("ya", "や");
        mapping.insert("yi", "い");
        mapping.insert("yu", "ゆ");
        mapping.insert("ye", "いぇ");
        mapping.insert("yo", "よ");
        
        // ら行
        mapping.insert("ra", "ら");
        mapping.insert("ri", "り");
        mapping.insert("ru", "る");
        mapping.insert("re", "れ");
        mapping.insert("ro", "ろ");
        
        // わ行
        mapping.insert("wa", "わ");
        mapping.insert("wi", "うぃ");
        mapping.insert("wu", "う");
        mapping.insert("we", "うぇ");
        mapping.insert("wo", "を");
        
        // ん - "n"単独は削除（特殊処理で対応）、"nn"のみ登録
        mapping.insert("nn", "ん");
        
        // nn + 母音/子音 のパターン
        mapping.insert("nna", "んな");
        mapping.insert("nni", "んに");
        mapping.insert("nnu", "んぬ");
        mapping.insert("nne", "んね");
        mapping.insert("nno", "んの");
        mapping.insert("nnya", "んにゃ");
        mapping.insert("nnyi", "んにぃ");
        mapping.insert("nnyu", "んにゅ");
        mapping.insert("nnye", "んにぇ");
        mapping.insert("nnyo", "んにょ");
        
        // きゃ行
        mapping.insert("kya", "きゃ");
        mapping.insert("kyi", "きぃ");
        mapping.insert("kyu", "きゅ");
        mapping.insert("kye", "きぇ");
        mapping.insert("kyo", "きょ");
        
        // ぎゃ行
        mapping.insert("gya", "ぎゃ");
        mapping.insert("gyi", "ぎぃ");
        mapping.insert("gyu", "ぎゅ");
        mapping.insert("gye", "ぎぇ");
        mapping.insert("gyo", "ぎょ");
        
        // しゃ行
        mapping.insert("sya", "しゃ");
        mapping.insert("sha", "しゃ");
        mapping.insert("syi", "しぃ");
        mapping.insert("syu", "しゅ");
        mapping.insert("shu", "しゅ");
        mapping.insert("sye", "しぇ");
        mapping.insert("she", "しぇ");
        mapping.insert("syo", "しょ");
        mapping.insert("sho", "しょ");
        
        // じゃ行
        mapping.insert("ja", "じゃ");
        mapping.insert("jya", "じゃ");
        mapping.insert("ji", "じ");
        mapping.insert("jyi", "じぃ");
        mapping.insert("ju", "じゅ");
        mapping.insert("jyu", "じゅ");
        mapping.insert("je", "じぇ");
        mapping.insert("jye", "じぇ");
        mapping.insert("jo", "じょ");
        mapping.insert("jyo", "じょ");
        
        // ちゃ行
        mapping.insert("cha", "ちゃ");
        mapping.insert("cya", "ちゃ");
        mapping.insert("chi", "ち");
        mapping.insert("cyi", "ちぃ");
        mapping.insert("chu", "ちゅ");
        mapping.insert("cyu", "ちゅ");
        mapping.insert("che", "ちぇ");
        mapping.insert("cye", "ちぇ");
        mapping.insert("cho", "ちょ");
        mapping.insert("cyo", "ちょ");
        
        // にゃ行
        mapping.insert("nya", "にゃ");
        mapping.insert("nyi", "にぃ");
        mapping.insert("nyu", "にゅ");
        mapping.insert("nye", "にぇ");
        mapping.insert("nyo", "にょ");
        
        // ひゃ行
        mapping.insert("hya", "ひゃ");
        mapping.insert("hyi", "ひぃ");
        mapping.insert("hyu", "ひゅ");
        mapping.insert("hye", "ひぇ");
        mapping.insert("hyo", "ひょ");
        
        // びゃ行
        mapping.insert("bya", "びゃ");
        mapping.insert("byi", "びぃ");
        mapping.insert("byu", "びゅ");
        mapping.insert("bye", "びぇ");
        mapping.insert("byo", "びょ");
        
        // ぴゃ行
        mapping.insert("pya", "ぴゃ");
        mapping.insert("pyi", "ぴぃ");
        mapping.insert("pyu", "ぴゅ");
        mapping.insert("pye", "ぴぇ");
        mapping.insert("pyo", "ぴょ");
        
        // みゃ行
        mapping.insert("mya", "みゃ");
        mapping.insert("myi", "みぃ");
        mapping.insert("myu", "みゅ");
        mapping.insert("mye", "みぇ");
        mapping.insert("myo", "みょ");
        
        // りゃ行
        mapping.insert("rya", "りゃ");
        mapping.insert("ryi", "りぃ");
        mapping.insert("ryu", "りゅ");
        mapping.insert("rye", "りぇ");
        mapping.insert("ryo", "りょ");
        
        // ふぁ行
        mapping.insert("fa", "ふぁ");
        mapping.insert("fi", "ふぃ");
        mapping.insert("fe", "ふぇ");
        mapping.insert("fo", "ふぉ");
        mapping.insert("fya", "ふゃ");
        mapping.insert("fyu", "ふゅ");
        mapping.insert("fyo", "ふょ");

        // ゔ行
        mapping.insert("va", "ゔぁ");
        mapping.insert("vi", "ゔぃ");
        mapping.insert("vu", "ゔ");
        mapping.insert("ve", "ゔぇ");
        mapping.insert("vo", "ゔぉ");

        // つぁ行
        mapping.insert("tsa", "つぁ");
        mapping.insert("tsi", "つぃ");
        mapping.insert("tse", "つぇ");
        mapping.insert("tso", "つぉ");

        // てゃ行・でゃ行（てぃ/でぃ を含む）
        mapping.insert("tha", "てゃ");
        mapping.insert("thi", "てぃ");
        mapping.insert("thu", "てゅ");
        mapping.insert("the", "てぇ");
        mapping.insert("tho", "てょ");
        mapping.insert("dha", "でゃ");
        mapping.insert("dhi", "でぃ");
        mapping.insert("dhu", "でゅ");
        mapping.insert("dhe", "でぇ");
        mapping.insert("dho", "でょ");
        mapping.insert("twu", "とぅ");
        mapping.insert("dwu", "どぅ");

        // うぁ行
        mapping.insert("wha", "うぁ");
        mapping.insert("whi", "うぃ");
        mapping.insert("whu", "う");
        mapping.insert("whe", "うぇ");
        mapping.insert("who", "うぉ");

        // くぁ行
        mapping.insert("qa", "くぁ");
        mapping.insert("qi", "くぃ");
        mapping.insert("qu", "く");
        mapping.insert("qe", "くぇ");
        mapping.insert("qo", "くぉ");

        // c 単独系（MS-IME互換）
        mapping.insert("ca", "か");
        mapping.insert("ci", "し");
        mapping.insert("cu", "く");
        mapping.insert("ce", "せ");
        mapping.insert("co", "こ");

        // 小書き文字（x / l プレフィックス）
        for (k, v) in [
            ("xa", "ぁ"), ("xi", "ぃ"), ("xu", "ぅ"), ("xe", "ぇ"), ("xo", "ぉ"),
            ("la", "ぁ"), ("li", "ぃ"), ("lu", "ぅ"), ("le", "ぇ"), ("lo", "ぉ"),
            ("xya", "ゃ"), ("xyu", "ゅ"), ("xyo", "ょ"),
            ("lya", "ゃ"), ("lyu", "ゅ"), ("lyo", "ょ"),
            ("xtu", "っ"), ("ltu", "っ"), ("xtsu", "っ"), ("ltsu", "っ"),
            ("xwa", "ゎ"), ("lwa", "ゎ"),
            ("xka", "ゕ"), ("xke", "ゖ"),
        ] {
            mapping.insert(k, v);
        }

        // 記号（日本語入力でよく使う最小セットのみ全角化）
        // プログラマ記号（& @ / 等）は半角のまま素通しさせ、意図しない
        // 全角化を避ける。
        for (k, v) in [
            ("-", "ー"), (",", "、"), (".", "。"), ("?", "？"), ("!", "！"),
            ("[", "「"), ("]", "」"), ("~", "〜"),
        ] {
            mapping.insert(k, v);
        }

        // 訓令式・別綴りの拗音（zy* = じゃ行, ty* = ちゃ行, dy* = ぢゃ行）
        // これらが無いと先頭に変換不能な子音が残り、以降が全てローマ字化する
        for (k, v) in [
            ("zya", "じゃ"), ("zyi", "じぃ"), ("zyu", "じゅ"), ("zye", "じぇ"), ("zyo", "じょ"),
            ("tya", "ちゃ"), ("tyi", "ちぃ"), ("tyu", "ちゅ"), ("tye", "ちぇ"), ("tyo", "ちょ"),
            ("dya", "ぢゃ"), ("dyi", "ぢぃ"), ("dyu", "ぢゅ"), ("dye", "ぢぇ"), ("dyo", "ぢょ"),
        ] {
            mapping.insert(k, v);
        }

        // 促音（っ）- 次の子音が重なる場合
        // これは別途ロジックで処理

        Self { mapping }
    }

    /// ローマ字バッファを「確定したひらがな」と「保留中のローマ字」に分割する。
    ///
    /// 逐次入力で使う。戻り値 `(settled, pending)`:
    /// - `settled`: これ以上入力しても変わらない確定部分（ひらがな。ごく稀に
    ///   変換不能な英字が混じる）。ひらがなバッファに送ってよい。
    /// - `pending`: まだモーラを構成しうる末尾のローマ字断片（例: "k", "ky", "n"）。
    ///   ローマ字バッファに残す。
    ///
    /// 重要: 変換結果の途中や先頭に変換不能な英字があっても、末尾の英字連続
    /// だけを pending とし、それ以外は settled に含める。これにより
    /// 「先頭に詰まった英字のせいで以降が全てローマ字化する」不具合を防ぐ。
    pub fn split(&self, romaji: &str) -> (String, String) {
        let converted = self.convert(romaji);
        // 末尾の英字連続 = 保留中のローマ字
        let pending: String = converted
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let settled_len = converted.len() - pending.len();
        (converted[..settled_len].to_string(), pending)
    }

    /// ローマ字文字列をひらがなに変換
    ///
    /// 大文字は小文字として扱う（Shift押下時の入力も同じかなにする）。
    pub fn convert(&self, romaji: &str) -> String {
        let mut result = String::new();
        let mut i = 0;
        let chars: Vec<char> = romaji.chars().map(|c| c.to_ascii_lowercase()).collect();
        
        while i < chars.len() {
            let mut matched = false;
            
            // 最長一致を試す（4文字 → 3文字 → 2文字 → 1文字）
            for len in (1..=4).rev() {
                if i + len > chars.len() {
                    continue;
                }
                
                let substr: String = chars[i..i+len].iter().collect();
                
                // マッピングテーブルで検索
                if let Some(kana) = self.mapping.get(substr.as_str()) {
                    result.push_str(kana);
                    i += len;
                    matched = true;
                    break;
                }
            }
            
            // マッチしなかった場合
            if !matched {
                // 促音処理: 同じ子音が続く場合
                if i + 1 < chars.len() && chars[i] == chars[i + 1] {
                    let ch = chars[i];
                    // 子音の判定
                    if matches!(ch, 'k' | 's' | 't' | 'p' | 'g' | 'z' | 'd' | 'b' | 'c' | 'h' | 'm' | 'r' | 'w' | 'f' | 'v' | 'j' | 'q' | 'y') {
                        result.push('っ');
                        i += 1; // 最初の子音をスキップ
                        continue;
                    }
                }
                
                // 'n' の特殊処理: 次が母音でない場合は「ん」
                if chars[i] == 'n' {
                    // 次の文字を確認
                    if i + 1 < chars.len() {
                        let next = chars[i + 1];
                        // 次が母音（a,i,u,e,o,y）でない場合は「ん」として確定
                        if !matches!(next, 'a' | 'i' | 'u' | 'e' | 'o' | 'y') {
                            result.push('ん');
                            i += 1;
                            continue;
                        }
                    } else {
                        // 最後の文字が 'n' ならそのまま（まだ確定しない）
                        result.push('n');
                        i += 1;
                        continue;
                    }
                }
                
                // それでもマッチしなければそのまま追加
                result.push(chars[i]);
                i += 1;
            }
        }
        
        result
    }
}

impl Default for RomajiConverter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_romaji_conversion() {
        let converter = RomajiConverter::new();

        assert_eq!(converter.convert("konnichiha"), "こんにちは");
        assert_eq!(converter.convert("arigatou"), "ありがとう");
        assert_eq!(converter.convert("kyou"), "きょう");
        assert_eq!(converter.convert("gakkou"), "がっこう");
        assert_eq!(converter.convert("senpai"), "せんぱい");
    }

    #[test]
    fn test_romaji_extended_mappings() {
        let converter = RomajiConverter::new();

        // 長音・記号
        assert_eq!(converter.convert("ra-menn"), "らーめん");
        assert_eq!(converter.convert("hai."), "はい。");
        assert_eq!(converter.convert("sou,"), "そう、");
        // ふぁ行・ゔ・つぁ・てぃ/でぃ
        assert_eq!(converter.convert("fairu"), "ふぁいる");
        assert_eq!(converter.convert("vaiorinn"), "ゔぁいおりん");
        assert_eq!(converter.convert("thi-mu"), "てぃーむ");
        assert_eq!(converter.convert("dhizuni-"), "でぃずにー");
        // 小書き文字
        assert_eq!(converter.convert("xtu"), "っ");
        assert_eq!(converter.convert("ltsu"), "っ");
        assert_eq!(converter.convert("xyo"), "ょ");
        // うぉ・くぁ
        assert_eq!(converter.convert("who-ta-"), "うぉーたー");
    }

    #[test]
    fn test_romaji_uppercase() {
        let converter = RomajiConverter::new();
        assert_eq!(converter.convert("KYOU"), "きょう");
        assert_eq!(converter.convert("Kyou"), "きょう");
    }

    #[test]
    fn test_romaji_kunrei_youon() {
        let converter = RomajiConverter::new();
        // zyu などの別綴りも変換できる（先頭に z が詰まらない）
        assert_eq!(converter.convert("zyunbann"), "じゅんばん");
        assert_eq!(converter.convert("tya"), "ちゃ");
        assert_eq!(converter.convert("zya"), "じゃ");
    }

    #[test]
    fn test_romaji_sokuon_and_n() {
        let c = RomajiConverter::new();
        // 促音・撥音の基本
        assert_eq!(c.convert("kitte"), "きって");
        assert_eq!(c.convert("gakkou"), "がっこう");
        // 末尾の単独 n は未確定のまま残る（次の入力/確定で「ん」になる）
        assert_eq!(c.convert("shinbunn"), "しんぶん");
        assert_eq!(c.convert("annnai"), "あんない");
        // 拗音
        assert_eq!(c.convert("kyakka"), "きゃっか");
        assert_eq!(c.convert("shixtu"), "しっ"); // xtu = っ
    }

    #[test]
    fn test_romaji_never_all_ascii_stuck() {
        // 未知の綴りでも全ASCIIで詰まらない（split で settled が進む）
        let c = RomajiConverter::new();
        let (settled, _pending) = c.split("zyabsurd");
        // 先頭が確定して英字だけで固まらない
        assert!(settled.chars().any(|ch| ('\u{3041}'..='\u{3096}').contains(&ch)));
    }

    #[test]
    fn test_romaji_split() {
        let converter = RomajiConverter::new();
        // 完全に変換できれば pending は空
        assert_eq!(converter.split("kyou"), ("きょう".to_string(), String::new()));
        // 末尾の子音は保留
        assert_eq!(converter.split("k"), (String::new(), "k".to_string()));
        assert_eq!(converter.split("kya"), ("きゃ".to_string(), String::new()));
        // 途中まで確定 + 末尾保留
        assert_eq!(converter.split("kyouk"), ("きょう".to_string(), "k".to_string()));
        // 単独 n は保留（次第第で ん か な行になる）
        assert_eq!(converter.split("n"), (String::new(), "n".to_string()));
        assert_eq!(converter.split("nn"), ("ん".to_string(), String::new()));
        // 以前バグっていた zyu 系: 全てローマ字化せず確定できる
        assert_eq!(converter.split("zyunban"), ("じゅんば".to_string(), "n".to_string()));
    }
}