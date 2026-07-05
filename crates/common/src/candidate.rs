//! 変換候補の定義
//! 
//! 要件定義書 7.2 候補生成機能に基づく実装

/// 候補の種類
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateKind {
    /// 漢字変換
    KanjiConversion,
    /// カタカナ変換
    KatakanaConversion,
    /// 予測入力
    Prediction,
    /// 誤字補正
    TypoCorrection,
    /// ユーザー辞書
    UserDictionary,
    /// ひらがなのまま
    RawKana,
}

/// 変換候補
#[derive(Debug, Clone)]
pub struct Candidate {
    /// 変換後のテキスト
    pub text: String,
    /// 読み（ひらがな）
    pub reading: String,
    /// スコア（低いほど優先）
    pub score: f32,
    /// 候補の種類
    pub kind: CandidateKind,
    /// 品詞情報（オプション）
    pub pos: Option<String>,
}

impl Candidate {
    /// 新しい候補を作成
    pub fn new(text: String, reading: String, score: f32, kind: CandidateKind) -> Self {
        Self {
            text,
            reading,
            score,
            kind,
            pos: None,
        }
    }

    /// 漢字変換候補を作成
    pub fn kanji(text: String, reading: String, score: f32) -> Self {
        Self::new(text, reading, score, CandidateKind::KanjiConversion)
    }

    /// カタカナ変換候補を作成
    pub fn katakana(text: String, reading: String) -> Self {
        Self::new(text, reading, 1000.0, CandidateKind::KatakanaConversion)
    }

    /// ひらがな候補を作成
    pub fn hiragana(text: String) -> Self {
        Self::new(text.clone(), text, 2000.0, CandidateKind::RawKana)
    }

    /// 誤字補正候補を作成
    pub fn typo_correction(text: String, reading: String, score: f32) -> Self {
        Self::new(text, reading, score, CandidateKind::TypoCorrection)
    }

    /// ユーザー辞書候補を作成
    pub fn user_dict(text: String, reading: String, score: f32) -> Self {
        Self::new(text, reading, score, CandidateKind::UserDictionary)
    }
}

/// ひらがなをカタカナに変換
pub fn hiragana_to_katakana(hiragana: &str) -> String {
    hiragana
        .chars()
        .map(|c| {
            if ('\u{3041}'..='\u{3096}').contains(&c) {
                // ひらがな→カタカナ（+0x60）
                char::from_u32(c as u32 + 0x60).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// カタカナをひらがなに変換
pub fn katakana_to_hiragana(katakana: &str) -> String {
    katakana
        .chars()
        .map(|c| {
            if ('\u{30A1}'..='\u{30F6}').contains(&c) {
                // カタカナ→ひらがな（-0x60）
                char::from_u32(c as u32 - 0x60).unwrap_or(c)
            } else if c == '\u{30FC}' {
                // 長音符はそのまま
                'ー'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hiragana_to_katakana() {
        assert_eq!(hiragana_to_katakana("きょう"), "キョウ");
        assert_eq!(hiragana_to_katakana("こんにちは"), "コンニチハ");
        assert_eq!(hiragana_to_katakana("らすと"), "ラスト");
    }

    #[test]
    fn test_katakana_to_hiragana() {
        assert_eq!(katakana_to_hiragana("キョウ"), "きょう");
        assert_eq!(katakana_to_hiragana("コンニチハ"), "こんにちは");
        assert_eq!(katakana_to_hiragana("ラスト"), "らすと");
    }

    #[test]
    fn test_candidate_creation() {
        let kanji = Candidate::kanji("今日".to_string(), "きょう".to_string(), 100.0);
        assert_eq!(kanji.kind, CandidateKind::KanjiConversion);

        let katakana = Candidate::katakana("キョウ".to_string(), "きょう".to_string());
        assert_eq!(katakana.kind, CandidateKind::KatakanaConversion);
    }
}
