//! 判断層（judge-lm）による自動変換結果の選び直し
//!
//! 従来の変換結果（Viterbi 1-best＋連想リランク）を「候補の1つ」として残した
//! まま、同じラティスの N-best を加えた候補集合を `crate::judge::JudgeLm` で
//! 採点し、最も確率の高いものを採用する。判断層が無い（`judge == None`）とき
//! は何もしない（従来の挙動と完全に同じ）。

use super::*;
use crate::judge::{JudgeLm, JudgeScore};

/// 判断層に渡す N-best の候補数（従来の変換結果は別枠で必ず含める）
pub const JUDGE_N_BEST: usize = 10;

/// 判断層が採点した1候補
#[derive(Clone, Debug)]
pub struct JudgedCandidate {
    pub entries: Vec<WordEntry>,
    pub score: JudgeScore,
    /// 候補集合上の確率（全候補の合計が1）
    pub prob: f32,
    /// 従来エンジン（判断層なし）の変換結果そのものか
    pub is_baseline: bool,
}

impl JudgedCandidate {
    pub fn surface(&self) -> String {
        self.entries.iter().map(|e| e.surface.as_str()).collect()
    }
}

impl ViterbiConverter {
    /// 判断層を付け替える（`None` で従来の挙動に戻る）
    pub fn set_judge(&mut self, judge: Option<Arc<JudgeLm>>) {
        self.judge = judge;
    }

    /// 語列の辞書コスト（単語の実効コスト＋接続コスト、文頭・文末を含む）。
    /// どの候補にも同じ式を当てるための共通の物差しで、`find_best_path` の
    /// 手調整ガード群は含まない。
    pub fn path_dict_cost(&self, path: &[WordEntry]) -> i32 {
        let mut total = 0i32;
        let mut prev: Option<&WordEntry> = None;
        for e in path {
            let prev_right = prev.map(|p| p.right_id).unwrap_or(self.dictionary.bos_id);
            let (conn, _) = self.base_connection_cost(
                prev_right,
                prev.map(|p| p.surface.as_str()),
                e.left_id,
                &e.surface,
            );
            total = total
                .saturating_add(conn)
                .saturating_add(self.effective_word_cost(e));
            prev = Some(e);
        }
        let last_right = prev.map(|p| p.right_id).unwrap_or(self.dictionary.bos_id);
        total = total.saturating_add(self.dictionary.matrix.get(last_right, self.dictionary.eos_id) as i32);
        total.saturating_sub(self.path_learned_assoc_bonus(path))
    }

    /// 文中の内容語（漢字等に変換された自立語）の (位置, 表記)
    fn content_words(path: &[WordEntry]) -> Vec<(usize, &str)> {
        path.iter()
            .enumerate()
            .filter(|(_, e)| is_content_pos(&e.pos) && e.surface != e.reading)
            .map(|(i, e)| (i, e.surface.as_str()))
            .collect()
    }

    /// 離れた内容語どうしのユーザー学習連想（`learned_assoc`）の合計。
    /// 従来エンジンでは1-best の後処理（`rerank_by_assoc_from`）として効いている
    /// 個人の学習を、判断層ではどの候補にも同じ式で当てる（隣接語は対象外、
    /// `rerank_by_assoc_from` と同じ規約）。
    fn path_learned_assoc_bonus(&self, path: &[WordEntry]) -> i32 {
        if self.learned_assoc.is_empty() {
            return 0;
        }
        let content = Self::content_words(path);
        let mut bonus = 0i32;
        for (a, &(i, si)) in content.iter().enumerate() {
            for &(j, sj) in &content[a + 1..] {
                if j == i + 1 {
                    continue;
                }
                let key = (si.to_string(), sj.to_string());
                bonus = bonus
                    .saturating_add(self.learned_assoc.get(&key).copied().unwrap_or(0))
                    .saturating_add(self.learned_assoc.get(&(key.1, key.0)).copied().unwrap_or(0));
            }
        }
        bonus
    }

    /// 隣接する2語の組のうち、事前精査済みコロケーション（`seeded_assoc`、
    /// word_assoc.tsv）に載っているものの数。`rerank_by_seeded_collocation_from`
    /// と同じく、隣の語はひらがな表記の語（「かけっこ」等）でもよい。
    pub fn path_seed_hits(&self, path: &[WordEntry]) -> u32 {
        if self.seeded_assoc.is_empty() {
            return 0;
        }
        path.windows(2)
            .filter(|w| {
                (is_content_pos(&w[0].pos) || is_content_pos(&w[1].pos))
                    && self.seeded_assoc.contains_key(&(w[0].surface.clone(), w[1].surface.clone()))
            })
            .count() as u32
    }

    /// 判断層の候補集合（N-best＋従来の結果）を作る。固定文節 `pinned` は
    /// 全候補で保たれる。
    fn judge_candidate_paths(&self, reading: &str, pinned: &[WordEntry]) -> Vec<Vec<WordEntry>> {
        let pinned = matching_pinned_prefix(reading, pinned);
        if pinned.is_empty() {
            return self.n_best(reading, JUDGE_N_BEST);
        }
        let mut lattice = self.build_pinned_lattice(reading, pinned);
        self.find_best_path(&mut lattice);
        if lattice.nodes[lattice.eos_index].total_cost == i32::MAX {
            return Vec::new();
        }
        let n = pinned.len();
        n_best_from_lattice(&lattice, &self.dictionary, JUDGE_N_BEST)
            .into_iter()
            .map(|path| {
                if path.len() < n {
                    return path;
                }
                let mut out = path[..n].to_vec();
                out.extend(self.repair_single_kanji_fragments(path[n..].to_vec()));
                out
            })
            .collect()
    }

    /// 候補を採点し、確率の高い順に並べて返す。`baseline` は従来エンジンの
    /// 変換結果（必ず候補に含める）。判断層が無ければ空。
    pub fn judge_candidates(
        &self,
        reading: &str,
        pinned: &[WordEntry],
        baseline: &[WordEntry],
    ) -> Vec<JudgedCandidate> {
        let Some(judge) = self.judge.as_ref() else {
            return Vec::new();
        };
        let base_surface: String = baseline.iter().map(|e| e.surface.as_str()).collect();
        let mut seen = std::collections::HashSet::new();
        seen.insert(base_surface);
        let mut paths = vec![(baseline.to_vec(), true)];
        for path in self.judge_candidate_paths(reading, pinned) {
            let s: String = path.iter().map(|e| e.surface.as_str()).collect();
            if seen.insert(s) {
                paths.push((path, false));
            }
        }

        let scored: Vec<(Vec<WordEntry>, JudgeScore, bool)> = paths
            .into_iter()
            .map(|(entries, is_baseline)| {
                let (lm_base, unk_chars) = judge.sequence_logp_parts(None, &entries);
                let lm_logp = lm_base + judge.params().unk_char_logp * unk_chars as f32;
                let dict_cost = self.path_dict_cost(&entries);
                let seed_hits = self.path_seed_hits(&entries);
                let score = judge.combine(lm_logp, dict_cost, seed_hits);
                (entries, JudgeScore { lm_logp, lm_base, unk_chars, dict_cost, seed_hits, score }, is_baseline)
            })
            .collect();
        let probs = judge.distribution(&scored.iter().map(|(_, s, _)| s.score).collect::<Vec<_>>());
        let mut out: Vec<JudgedCandidate> = scored
            .into_iter()
            .zip(probs)
            .map(|((entries, score, is_baseline), prob)| JudgedCandidate { entries, score, prob, is_baseline })
            .collect();
        // 同点なら従来の結果を優先する（安定ソート＋先頭が baseline）
        out.sort_by(|a, b| b.score.score.partial_cmp(&a.score.score).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    /// 判断層があれば候補を選び直し、無ければ `base` をそのまま返す
    pub(crate) fn judge_or_keep(&self, reading: &str, pinned: &[WordEntry], base: Vec<WordEntry>) -> Vec<WordEntry> {
        if self.judge.is_none() || base.is_empty() {
            return base;
        }
        let mut judged = self.judge_candidates(reading, pinned, &base);
        if judged.is_empty() || judged[0].is_baseline {
            return base;
        }
        let best = judged.swap_remove(0);
        crate::debug_log!(
            "judge: {}→{}（確率{:.2}, lm={:.1}, dict={}）",
            base.iter().map(|e| e.surface.as_str()).collect::<String>(),
            best.surface(),
            best.prob,
            best.score.lm_logp,
            best.score.dict_cost
        );
        best.entries
    }
}
