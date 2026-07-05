//! 入力状態管理
//! 
//! 要件定義書 7.4 自動仮変換機能に基づく実装

use crate::candidate::Candidate;
use std::time::Instant;

/// 入力状態
#[derive(Debug, Clone)]
pub enum CompositionState {
    /// 未変換のひらがな
    RawKana(String),
    /// 仮変換中
    Provisional {
        /// 読み（ひらがな）
        reading: String,
        /// 変換結果
        converted: String,
        /// 変換候補リスト
        candidates: Vec<Candidate>,
        /// 現在選択中の候補インデックス
        selected_index: usize,
    },
    /// 確定済み
    Committed(String),
}

impl CompositionState {
    /// 新しい未変換状態を作成
    pub fn new() -> Self {
        Self::RawKana(String::new())
    }

    /// 入力中かどうか
    pub fn is_composing(&self) -> bool {
        match self {
            Self::RawKana(s) => !s.is_empty(),
            Self::Provisional { .. } => true,
            Self::Committed(_) => false,
        }
    }

    /// 現在のテキストを取得
    pub fn get_text(&self) -> &str {
        match self {
            Self::RawKana(s) => s,
            Self::Provisional { converted, .. } => converted,
            Self::Committed(s) => s,
        }
    }

    /// 読みを取得
    pub fn get_reading(&self) -> &str {
        match self {
            Self::RawKana(s) => s,
            Self::Provisional { reading, .. } => reading,
            Self::Committed(_) => "",
        }
    }
}

impl Default for CompositionState {
    fn default() -> Self {
        Self::new()
    }
}

/// 自動変換のタイミングを判定
/// 
/// 要件定義書より：
/// - 入力停止から150〜300ms程度で仮変換を実行
/// - 句読点入力時に仮変換
/// - 「です」「ます」「けど」など文節境界らしい入力で仮変換
pub fn should_auto_convert(input: &str, elapsed_ms: u64) -> bool {
    // 空の場合は変換しない
    if input.is_empty() {
        return false;
    }

    // 一定時間経過で変換
    if elapsed_ms >= 250 {
        return true;
    }

    // 句読点で変換
    if input.ends_with('。') || input.ends_with('、') 
       || input.ends_with('！') || input.ends_with('？') {
        return true;
    }

    // 文節境界で変換
    let endings = [
        "です", "ます", "でした", "ました",
        "けど", "けれど", "けれども",
        "から", "ので", "のに", "ても",
        "して", "した", "する",
        "って", "った",
        "には", "では", "とは",
    ];

    for ending in &endings {
        if input.ends_with(ending) {
            return true;
        }
    }

    false
}

/// 入力コンテキスト
#[derive(Debug, Clone)]
pub struct InputContext {
    /// 現在の入力状態
    pub state: CompositionState,
    /// 最後の入力時刻
    pub last_input_time: Option<Instant>,
    /// 入力履歴（確定済みテキスト）
    pub history: Vec<String>,
    /// アプリケーション名（コンテキスト学習用）
    pub app_name: Option<String>,
}

impl InputContext {
    pub fn new() -> Self {
        Self {
            state: CompositionState::new(),
            last_input_time: None,
            history: Vec::new(),
            app_name: None,
        }
    }

    /// 入力を追加
    pub fn add_input(&mut self, text: &str) {
        self.last_input_time = Some(Instant::now());
        match &mut self.state {
            CompositionState::RawKana(s) => s.push_str(text),
            CompositionState::Provisional { reading, .. } => {
                // 仮変換中に追加入力があった場合は未変換に戻す
                let new_reading = format!("{}{}", reading, text);
                self.state = CompositionState::RawKana(new_reading);
            }
            CompositionState::Committed(_) => {
                self.state = CompositionState::RawKana(text.to_string());
            }
        }
    }

    /// バックスペース
    pub fn backspace(&mut self) -> bool {
        match &mut self.state {
            CompositionState::RawKana(s) => {
                if s.pop().is_some() {
                    self.last_input_time = Some(Instant::now());
                    true
                } else {
                    false
                }
            }
            CompositionState::Provisional { reading, .. } => {
                // 仮変換を解除して未変換に戻す
                let mut new_reading = reading.clone();
                new_reading.pop();
                self.state = CompositionState::RawKana(new_reading);
                self.last_input_time = Some(Instant::now());
                true
            }
            CompositionState::Committed(_) => false,
        }
    }

    /// 仮変換を設定
    pub fn set_provisional(&mut self, reading: String, converted: String, candidates: Vec<Candidate>) {
        self.state = CompositionState::Provisional {
            reading,
            converted,
            candidates,
            selected_index: 0,
        };
    }

    /// 次の候補を選択
    pub fn next_candidate(&mut self) -> bool {
        if let CompositionState::Provisional { candidates, selected_index, converted, .. } = &mut self.state {
            if *selected_index + 1 < candidates.len() {
                *selected_index += 1;
                *converted = candidates[*selected_index].text.clone();
                return true;
            }
        }
        false
    }

    /// 前の候補を選択
    pub fn prev_candidate(&mut self) -> bool {
        if let CompositionState::Provisional { candidates, selected_index, converted, .. } = &mut self.state {
            if *selected_index > 0 {
                *selected_index -= 1;
                *converted = candidates[*selected_index].text.clone();
                return true;
            }
        }
        false
    }

    /// 確定
    pub fn commit(&mut self) -> Option<String> {
        let text = self.state.get_text().to_string();
        if !text.is_empty() {
            self.history.push(text.clone());
            self.state = CompositionState::Committed(text.clone());
            Some(text)
        } else {
            None
        }
    }

    /// キャンセル（ひらがなに戻す）
    pub fn cancel(&mut self) -> Option<String> {
        if let CompositionState::Provisional { reading, .. } = &self.state {
            let reading = reading.clone();
            self.state = CompositionState::RawKana(reading.clone());
            Some(reading)
        } else {
            None
        }
    }

    /// クリア
    pub fn clear(&mut self) {
        self.state = CompositionState::new();
        self.last_input_time = None;
    }

    /// 最後の入力からの経過時間（ミリ秒）
    pub fn elapsed_since_last_input(&self) -> Option<u64> {
        self.last_input_time.map(|t| t.elapsed().as_millis() as u64)
    }

    /// 自動変換すべきかどうか
    pub fn should_auto_convert(&self) -> bool {
        if let CompositionState::RawKana(input) = &self.state {
            let elapsed = self.elapsed_since_last_input().unwrap_or(0);
            should_auto_convert(input, elapsed)
        } else {
            false
        }
    }
}

impl Default for InputContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_auto_convert_punctuation() {
        assert!(should_auto_convert("こんにちは。", 0));
        assert!(should_auto_convert("はい、", 0));
        assert!(!should_auto_convert("こんにちは", 0));
    }

    #[test]
    fn test_should_auto_convert_endings() {
        assert!(should_auto_convert("そうです", 0));
        assert!(should_auto_convert("いきます", 0));
        assert!(should_auto_convert("それから", 0));
        assert!(!should_auto_convert("それ", 0));
    }

    #[test]
    fn test_should_auto_convert_timeout() {
        assert!(should_auto_convert("こんにちは", 300));
        assert!(!should_auto_convert("こんにちは", 100));
    }

    #[test]
    fn test_input_context_basic() {
        let mut ctx = InputContext::new();
        ctx.add_input("きょう");
        assert_eq!(ctx.state.get_reading(), "きょう");
        
        ctx.backspace();
        assert_eq!(ctx.state.get_reading(), "きょ");
    }
}
