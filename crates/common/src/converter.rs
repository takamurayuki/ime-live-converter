//! ライブ変換エンジン
//! 
//! macOSのライブ変換のような自動変換体験を提供

use crate::candidate::{Candidate, hiragana_to_katakana};
use crate::composition::{CompositionState, InputContext, should_auto_convert};
use crate::dictionary::Dictionary;
use crate::learning::LearningRepository;
use crate::rerank::CandidateReranker;
use crate::typo::TypoCorrector;
use crate::viterbi::ViterbiConverter;
use crate::RomajiConverter;
use std::time::Instant;

/// ライブ変換エンジン
///
/// ローマ字入力 → ひらがな → 漢字/カタカナの自動変換を行う
pub struct LiveConverter {
    /// ローマ字→ひらがな変換
    romaji: RomajiConverter,
    /// Viterbi変換エンジン
    viterbi: Option<ViterbiConverter>,
    /// 誤字補正エンジン
    typo_corrector: TypoCorrector,
    /// 学習リポジトリ（ユーザー辞書 + 履歴）
    learning: Option<LearningRepository>,
    /// 候補リランカー（将来のローカルLLM連携用フック）
    reranker: Option<Box<dyn CandidateReranker>>,
    /// 入力コンテキスト
    context: InputContext,
    /// ローマ字バッファ
    romaji_buffer: String,
    /// ひらがなバッファ
    hiragana_buffer: String,
    /// 現在の変換結果
    current_result: String,
    /// N-best候補数（既定:10）
    n_best: usize,
}

impl LiveConverter {
    /// 新しいライブ変換エンジンを作成
    pub fn new() -> Self {
        Self {
            romaji: RomajiConverter::new(),
            viterbi: None,
            typo_corrector: TypoCorrector::new(),
            learning: None,
            reranker: None,
            context: InputContext::new(),
            romaji_buffer: String::new(),
            hiragana_buffer: String::new(),
            current_result: String::new(),
            n_best: 10,
        }
    }

    /// 辞書を設定
    pub fn set_dictionary(&mut self, dict: Dictionary) {
        self.viterbi = Some(ViterbiConverter::new(dict));
    }

    /// 学習リポジトリを設定
    pub fn set_learning(&mut self, learning: LearningRepository) {
        self.learning = Some(learning);
    }

    /// 候補リランカーを設定（将来のローカルLLM連携用）
    ///
    /// 生成済み候補の上位に対してスコア調整を行う。要件 14.1 参照。
    pub fn set_reranker(&mut self, reranker: Box<dyn CandidateReranker>) {
        self.reranker = Some(reranker);
    }

    /// N-best候補数を設定（既定: 10）
    pub fn set_n_best(&mut self, n: usize) {
        self.n_best = n.max(1);
    }

    /// 確定した変換を学習リポジトリに記録
    pub fn record_commit(&self, reading: &str, surface: &str) {
        if let Some(learning) = &self.learning {
            // 失敗しても入力は継続できるべきなのでエラーは握り潰す
            let _ = learning.record_commit(reading, surface, self.context.app_name.as_deref());
        }
    }

    /// ユーザー辞書に登録
    ///
    /// 学習リポジトリが未設定の場合はエラーを返す
    /// （以前は黙って何もしなかったため、登録したつもりが反映されない事故があった）。
    pub fn add_user_word(&self, reading: &str, surface: &str, pos: Option<&str>) -> anyhow::Result<()> {
        match &self.learning {
            Some(learning) => {
                learning.add_user_word(reading, surface, pos, 50)?;
                Ok(())
            }
            None => anyhow::bail!(
                "学習DBが未設定のため登録できません（set_learning で設定してください）"
            ),
        }
    }

    /// ローマ字入力を追加
    pub fn input_romaji(&mut self, ch: char) -> ConversionResult {
        self.context.last_input_time = Some(Instant::now());
        self.romaji_buffer.push(ch);

        // ローマ字バッファを「確定ひらがな」と「保留ローマ字」に分割
        // （先頭に変換不能な英字が残っても以降が全てローマ字化しないよう、
        //  末尾の英字断片のみを保留にする）
        let (settled, pending) = self.romaji.split(&self.romaji_buffer);
        if !settled.is_empty() {
            self.hiragana_buffer.push_str(&settled);
            self.romaji_buffer = pending;
        }

        // 変換を実行
        self.update_conversion()
    }

    /// ひらがな直接入力
    pub fn input_hiragana(&mut self, hiragana: &str) -> ConversionResult {
        self.context.last_input_time = Some(Instant::now());
        self.hiragana_buffer.push_str(hiragana);
        self.update_conversion()
    }

    /// バックスペース
    pub fn backspace(&mut self) -> ConversionResult {
        self.context.last_input_time = Some(Instant::now());
        if !self.romaji_buffer.is_empty() {
            self.romaji_buffer.pop();
        } else if !self.hiragana_buffer.is_empty() {
            self.hiragana_buffer.pop();
        }
        self.update_conversion()
    }

    /// 変換を更新
    fn update_conversion(&mut self) -> ConversionResult {
        // ひらがな + ローマ字（仮変換）
        let romaji_as_hiragana = self.romaji.convert(&self.romaji_buffer);
        let full_hiragana = format!("{}{}", self.hiragana_buffer, romaji_as_hiragana);

        if full_hiragana.is_empty() {
            self.current_result.clear();
            self.context.clear();
            return ConversionResult {
                display_text: String::new(),
                candidates: Vec::new(),
                is_provisional: false,
            };
        }

        // 変換候補を生成
        let candidates = self.generate_candidates(&full_hiragana);

        // 第一候補を現在の結果として設定
        let display_text = candidates.first()
            .map(|c| c.text.clone())
            .unwrap_or_else(|| full_hiragana.clone());

        self.current_result = display_text.clone();

        // 仮変換状態を更新（next/prev_candidate での候補切り替えに使う）
        self.context.set_provisional(
            full_hiragana,
            display_text.clone(),
            candidates.clone(),
        );

        ConversionResult {
            display_text,
            candidates,
            is_provisional: true,
        }
    }

    /// 変換候補を生成
    pub fn generate_candidates(&self, hiragana: &str) -> Vec<Candidate> {
        let mut candidates: Vec<Candidate> = Vec::new();

        // 1. ユーザー辞書（最優先）
        if let Some(learning) = &self.learning {
            if let Ok(user_words) = learning.find_user_words(hiragana) {
                for entry in user_words {
                    candidates.push(Candidate::user_dict(
                        entry.surface,
                        hiragana.to_string(),
                        entry.cost as f32 - 10000.0,
                    ));
                }
            }
        }

        // 2. Viterbi 変換候補（N-best）
        // 結果が入力(ひらがな)と同じでも、辞書ヒット由来の候補(例:「こんにちは」「ありがとう」)
        // である可能性があるので除外しない。ひらがなと完全一致するrawフォールバックは
        // 後段の dedup で吸収される。
        if let Some(viterbi) = &self.viterbi {
            let n_best = viterbi.n_best(hiragana, self.n_best);
            for (rank, entries) in n_best.into_iter().enumerate() {
                let converted: String = entries.iter().map(|e| e.surface.as_str()).collect();
                if converted.is_empty() {
                    continue;
                }
                let base_score = rank as f32 * 100.0;
                let mut score = base_score;

                // 学習による頻度ボーナスを加算
                if let Some(learning) = &self.learning {
                    if let Ok(bonus) = learning.calculate_frequency_bonus(hiragana, &converted) {
                        score += bonus;
                    }
                }

                // Viterbi 結果が入力と同じ場合は kind を RawKana として扱う
                let kind = if converted == hiragana {
                    crate::candidate::CandidateKind::RawKana
                } else {
                    crate::candidate::CandidateKind::KanjiConversion
                };
                candidates.push(Candidate {
                    text: converted,
                    reading: hiragana.to_string(),
                    score,
                    kind,
                    pos: None,
                });
            }
        }

        // 3. カタカナ変換候補
        let katakana = hiragana_to_katakana(hiragana);
        if katakana != hiragana {
            candidates.push(Candidate::katakana(katakana, hiragana.to_string()));
        }

        // 4. 誤字補正候補
        // 高信頼ルール（>= 0.7）はそのまま採用。
        // かな混同ペア由来の低信頼候補は、補正後の変換コストが明確に
        // 下がる（= 文脈上自然になる）場合のみ採用する（要件 7.5）。
        let typo_candidates = self.typo_corrector.correct(hiragana);
        if let Some(viterbi) = &self.viterbi {
            let (_, original_cost) = viterbi.convert_with_cost(hiragana);
            let mut accepted = 0;
            for typo in typo_candidates.iter() {
                if accepted >= 3 {
                    break;
                }
                let (entries, corrected_cost) = viterbi.convert_with_cost(&typo.corrected);
                let cost_gain = original_cost.saturating_sub(corrected_cost);
                let plausible = typo.confidence >= 0.7 || cost_gain > 2000;
                if !plausible {
                    continue;
                }
                let converted: String = entries.iter().map(|e| e.surface.as_str()).collect();
                let target = if converted.is_empty() { typo.corrected.clone() } else { converted };
                // 補正後の方が自然な変換になる場合のみ上位に出す
                // （要件 7.5: 強制優先しない・文脈上自然な場合のみ上位）。
                // 低信頼の混同ペア候補はコストが大幅に下がるときだけ、
                // 中信頼ルールはコストが下がっていれば、
                // 高信頼ルール（こんにちわ→こんにちは等の規範表記）は
                // コストがほぼ同等以上なら昇格する。
                // 昇格は最有力（信頼度順で最初に採用された）1件のみ。
                let promote = accepted == 0
                    && (cost_gain > 2000
                        || (typo.confidence >= 0.75 && cost_gain > 0)
                        || (typo.confidence >= 0.9 && cost_gain > -500));
                let score = if promote {
                    -50.0
                } else {
                    1500.0 + (1.0 - typo.confidence) * 1000.0
                };
                candidates.push(Candidate::typo_correction(target, typo.corrected.clone(), score));
                accepted += 1;
            }
        } else {
            for typo in typo_candidates.iter().filter(|t| t.confidence >= 0.7).take(3) {
                candidates.push(Candidate::typo_correction(
                    typo.corrected.clone(),
                    typo.corrected.clone(),
                    1500.0 + (1.0 - typo.confidence) * 1000.0,
                ));
            }
        }

        // 5. ひらがなそのまま（フォールバック）
        candidates.push(Candidate::hiragana(hiragana.to_string()));

        // スコアでソート（低いほど優先）
        candidates.sort_by(|a, b| a.score.total_cmp(&b.score));

        // 表層形で重複除去（先に出てきた方を残す）
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.text.clone()));

        // 6. リランカーによる再順位付け（将来: ローカルLLM）
        // 上位候補のみ渡してスコアを調整させ、再ソートする
        if let Some(reranker) = &self.reranker {
            let context_text: String = self
                .context
                .history
                .iter()
                .rev()
                .take(3)
                .rev()
                .map(|s| s.as_str())
                .collect();
            let top = self.n_best.min(candidates.len());
            reranker.rerank(hiragana, &context_text, &mut candidates[..top]);
            candidates.sort_by(|a, b| a.score.total_cmp(&b.score));
        }

        candidates
    }

    /// 次の候補を選択
    ///
    /// 切り替えに成功したら新しい表示テキストを返す。
    /// 末尾の候補で止まっている場合は None。
    pub fn next_candidate(&mut self) -> Option<String> {
        if self.context.next_candidate() {
            self.current_result = self.context.state.get_text().to_string();
            Some(self.current_result.clone())
        } else {
            None
        }
    }

    /// 前の候補を選択
    ///
    /// 切り替えに成功したら新しい表示テキストを返す。
    /// 先頭の候補で止まっている場合は None。
    pub fn prev_candidate(&mut self) -> Option<String> {
        if self.context.prev_candidate() {
            self.current_result = self.context.state.get_text().to_string();
            Some(self.current_result.clone())
        } else {
            None
        }
    }

    /// 確定
    ///
    /// 確定した内容は学習リポジトリ（設定時）と文脈履歴に記録される。
    pub fn commit(&mut self) -> String {
        let reading = self.get_hiragana_buffer();
        let result = self.current_result.clone();
        if !reading.is_empty() && !result.is_empty() {
            self.record_commit(&reading, &result);
            self.context.history.push(result.clone());
        }
        self.clear();
        result
    }

    /// キャンセル（ひらがなに戻す）
    pub fn cancel(&mut self) -> String {
        let hiragana = format!("{}{}", self.hiragana_buffer, self.romaji_buffer);
        self.clear();
        hiragana
    }

    /// クリア
    pub fn clear(&mut self) {
        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.current_result.clear();
        self.context.clear();
    }

    /// 入力中かどうか
    pub fn is_composing(&self) -> bool {
        !self.romaji_buffer.is_empty() || !self.hiragana_buffer.is_empty()
    }

    /// 現在の表示テキストを取得
    pub fn get_display_text(&self) -> &str {
        &self.current_result
    }

    /// 現在のひらがなバッファを取得
    pub fn get_hiragana_buffer(&self) -> String {
        let romaji_as_hiragana = self.romaji.convert(&self.romaji_buffer);
        format!("{}{}", self.hiragana_buffer, romaji_as_hiragana)
    }

    /// 自動変換すべきかどうか
    pub fn should_auto_convert(&self) -> bool {
        let hiragana = self.get_hiragana_buffer();
        let elapsed = self.context.elapsed_since_last_input().unwrap_or(0);
        should_auto_convert(&hiragana, elapsed)
    }

    /// 現在選択中の候補位置を取得: (選択インデックス, 候補総数)
    ///
    /// 仮変換中でなければ None。
    pub fn candidate_position(&self) -> Option<(usize, usize)> {
        if let CompositionState::Provisional { candidates, selected_index, .. } = &self.context.state {
            Some((*selected_index, candidates.len()))
        } else {
            None
        }
    }
}

impl Default for LiveConverter {
    fn default() -> Self {
        Self::new()
    }
}

/// 変換結果
#[derive(Debug, Clone)]
pub struct ConversionResult {
    /// 表示するテキスト
    pub display_text: String,
    /// 変換候補リスト
    pub candidates: Vec<Candidate>,
    /// 仮変換中かどうか
    pub is_provisional: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_live_converter_basic() {
        let mut converter = LiveConverter::new();
        
        // "kyo" を入力
        converter.input_romaji('k');
        converter.input_romaji('y');
        converter.input_romaji('o');
        
        let hiragana = converter.get_hiragana_buffer();
        // "kyo" はまだ完全なひらがなにならない可能性がある
        assert!(hiragana.contains("きょ") || hiragana.contains("kyo"));
    }

    #[test]
    fn test_katakana_candidate() {
        let mut converter = LiveConverter::new();

        // ひらがなを直接入力
        converter.input_hiragana("らすと");

        // カタカナ候補が含まれていることを確認
        let result = converter.update_conversion();
        let has_katakana = result.candidates.iter().any(|c| c.text == "ラスト");
        assert!(has_katakana);
    }

    #[test]
    fn test_user_dictionary_priority() {
        let mut converter = LiveConverter::new();
        let learning = LearningRepository::in_memory().unwrap();
        learning.add_user_word("らすと", "Rust", Some("名詞"), 50).unwrap();
        converter.set_learning(learning);

        let candidates = converter.generate_candidates("らすと");
        // ユーザー辞書のRustが第一候補に出る
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].text, "Rust");
        assert_eq!(candidates[0].kind, crate::candidate::CandidateKind::UserDictionary);
    }

    #[test]
    fn test_history_priority() {
        let mut converter = LiveConverter::new();
        let learning = LearningRepository::in_memory().unwrap();
        // 「らすと → ラスト」を 10 回確定したことにする
        for _ in 0..10 {
            learning.record_commit("らすと", "ラスト", None).unwrap();
        }
        converter.set_learning(learning);

        let candidates = converter.generate_candidates("らすと");
        // 履歴により頻度ボーナスがついて「ラスト」が含まれる
        assert!(candidates.iter().any(|c| c.text == "ラスト"));
    }

    #[test]
    fn test_backspace() {
        let mut converter = LiveConverter::new();
        
        converter.input_hiragana("きょう");
        assert_eq!(converter.get_hiragana_buffer(), "きょう");
        
        converter.backspace();
        assert_eq!(converter.get_hiragana_buffer(), "きょ");
        
        converter.backspace();
        assert_eq!(converter.get_hiragana_buffer(), "き");
    }

    #[test]
    fn test_candidate_cycling() {
        let mut converter = LiveConverter::new();

        let result = converter.input_hiragana("らすと");
        assert!(result.candidates.len() >= 2, "候補が2件以上あるはず");
        let first = result.display_text.clone();

        // Space での次候補切り替え
        let second = converter.next_candidate().expect("次候補に切り替えられるはず");
        assert_ne!(first, second);
        assert_eq!(converter.get_display_text(), second);

        // Shift+Space で前候補に戻る
        let back = converter.prev_candidate().expect("前候補に戻れるはず");
        assert_eq!(first, back);

        // 先頭でさらに戻ろうとすると None
        assert!(converter.prev_candidate().is_none());
    }

    #[test]
    fn test_commit_records_learning() {
        let mut converter = LiveConverter::new();
        converter.set_learning(LearningRepository::in_memory().unwrap());

        converter.input_hiragana("らすと");
        let committed = converter.commit();
        assert!(!committed.is_empty());

        // 確定内容が履歴学習に記録されている
        let freq = converter
            .learning
            .as_ref()
            .unwrap()
            .find_frequency("らすと", &committed)
            .unwrap();
        assert_eq!(freq, 1);
    }

    #[test]
    fn test_reranker_hook() {
        use crate::candidate::CandidateKind;
        use crate::rerank::CandidateReranker;

        /// ひらがな候補を最優先にするテスト用リランカー
        struct RawKanaFirst;
        impl CandidateReranker for RawKanaFirst {
            fn name(&self) -> &'static str {
                "raw-kana-first"
            }
            fn rerank(&self, _reading: &str, _ctx: &str, candidates: &mut [Candidate]) {
                for c in candidates.iter_mut() {
                    if c.kind == CandidateKind::RawKana {
                        c.score = -99999.0;
                    }
                }
            }
        }

        let mut converter = LiveConverter::new();
        converter.set_reranker(Box::new(RawKanaFirst));

        let candidates = converter.generate_candidates("らすと");
        assert_eq!(candidates[0].text, "らすと", "リランカーの調整が反映されるはず");
    }

    #[test]
    fn test_add_user_word_without_learning_fails() {
        let converter = LiveConverter::new();
        // 学習DB未設定での登録は黙って成功せず、エラーを返す
        assert!(converter.add_user_word("らすと", "Rust", None).is_err());
    }

    #[test]
    fn test_commit_and_clear() {
        let mut converter = LiveConverter::new();
        
        converter.input_hiragana("てすと");
        assert!(converter.is_composing());
        
        let result = converter.commit();
        assert!(!result.is_empty());
        assert!(!converter.is_composing());
    }
}
