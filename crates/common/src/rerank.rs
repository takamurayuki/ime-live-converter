//! 候補リランク用フック
//!
//! 要件定義書 14.1「軽量LMによるrerank」に向けた抽象化。
//! 候補生成は辞書 + Viterbi で高速に行い、絞り込まれた上位候補だけを
//! より文脈を理解できるモデル（将来的にはローカルLLM）で再順位付けする。
//!
//! 設計方針（要件 8.1 / 14.1）:
//! - リランカーはリアルタイム入力のメイン経路に入れない。
//!   毎キー入力ではなく、入力停止後や文節確定のタイミングで呼ぶこと。
//! - リランカーが失敗・タイムアウトしても変換自体は継続できること。
//!   そのため実装側は rerank 内でエラーを外に漏らさず、
//!   スコアを変更しないことで「何もしない」にフォールバックする。

use crate::candidate::Candidate;

/// 変換候補の再順位付けを行う trait
///
/// 実装例:
/// - `NoopReranker`: 何もしない（デフォルト）
/// - 将来: llama.cpp / Ollama 等のローカルLLMに上位候補を渡し、
///   文脈上最も自然な候補のスコアを下げる（低スコア = 高優先）実装
pub trait CandidateReranker: Send + Sync {
    /// 識別名（ログ・デバッグ用）
    fn name(&self) -> &'static str;

    /// 候補のスコアを文脈に応じて調整する
    ///
    /// スコアは低いほど優先。順序の並べ替えは呼び出し側が
    /// スコアで再ソートするので、この中では score の書き換えだけでよい。
    ///
    /// * `reading` - 変換対象の読み（ひらがな）
    /// * `committed_context` - 直前に確定したテキスト（文脈情報）
    /// * `candidates` - スコア順に並んだ変換候補（上位のみ渡される）
    fn rerank(&self, reading: &str, committed_context: &str, candidates: &mut [Candidate]);
}

/// 何もしないリランカー
pub struct NoopReranker;

impl CandidateReranker for NoopReranker {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn rerank(&self, _reading: &str, _committed_context: &str, _candidates: &mut [Candidate]) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::CandidateKind;

    /// 特定の表層形を最優先にするテスト用リランカー
    struct PreferReranker(&'static str);

    impl CandidateReranker for PreferReranker {
        fn name(&self) -> &'static str {
            "prefer"
        }

        fn rerank(&self, _reading: &str, _ctx: &str, candidates: &mut [Candidate]) {
            for c in candidates.iter_mut() {
                if c.text == self.0 {
                    c.score = f32::MIN;
                }
            }
        }
    }

    #[test]
    fn test_reranker_adjusts_score() {
        let mut candidates = vec![
            Candidate::new("A".into(), "あ".into(), 100.0, CandidateKind::KanjiConversion),
            Candidate::new("B".into(), "あ".into(), 200.0, CandidateKind::KanjiConversion),
        ];
        let reranker = PreferReranker("B");
        reranker.rerank("あ", "", &mut candidates);
        candidates.sort_by(|a, b| a.score.total_cmp(&b.score));
        assert_eq!(candidates[0].text, "B");
    }
}
