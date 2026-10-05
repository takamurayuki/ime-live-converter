//! 判断層（judge-lm）による自動変換結果の選び直し
//!
//! 従来の変換結果（Viterbi 1-best＋連想リランク）を「候補の1つ」として残した
//! まま、同じラティス上で「LM 対数確率＋辞書コスト」の総合スコアが最大になる
//! 経路（`lm_viterbi`）を加えた候補集合を `crate::judge::JudgeLm` で採点し、
//! 最も確率の高いものを採用する。判断層が無い（`judge == None`）ときは何も
//! しない（従来の挙動と完全に同じ）。
//!
//! LM はバイグラムなので、総合スコアは「直前の語→今の語」の辺ごとの和に
//! 分解できる。よってラティスのノードをそのまま状態にした Viterbi で、全経路の
//! 中の厳密な最適を従来の Viterbi と同程度の計算量で求められる（N-best から
//! 選ぶ方式は長文で正解が上位N件に入らず、探索も重かったため置き換えた）。

use super::*;
use crate::judge::{JudgeLm, JudgeScore};

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
        path.windows(2).filter(|w| self.is_seed_pair(&w[0], &w[1])).count() as u32
    }

    /// 隣接する2語が事前精査済みコロケーション（`seeded_assoc`）の組か
    fn is_seed_pair(&self, a: &WordEntry, b: &WordEntry) -> bool {
        !self.seeded_assoc.is_empty()
            && (is_content_pos(&a.pos) || is_content_pos(&b.pos))
            && self.seeded_assoc.contains_key(&(a.surface.clone(), b.surface.clone()))
    }

    /// 判断層の候補（従来の結果以外）を作る: 総合スコア最大の経路と、その
    /// 断片修復版。固定文節 `pinned` は全候補で保たれる。
    fn judge_candidate_paths(&self, judge: &JudgeLm, reading: &str, pinned: &[WordEntry]) -> Vec<Vec<WordEntry>> {
        let pinned = matching_pinned_prefix(reading, pinned);
        let lattice = if pinned.is_empty() {
            self.build_lattice(reading)
        } else {
            self.build_pinned_lattice(reading, pinned)
        };
        let Some(path) = self.lm_viterbi(judge, &lattice) else {
            return Vec::new();
        };
        let n = pinned.len().min(path.len());
        let mut repaired = path[..n].to_vec();
        repaired.extend(self.repair_single_kanji_fragments(path[n..].to_vec()));
        vec![path, repaired]
    }

    /// ラティス上で総合スコア（`JudgeLm::combine` と同じ式。ただし離れた語どうしの
    /// 学習連想は辺に分解できないので含めない）が最大の経路を求める。
    /// 到達できなければ None。
    pub fn lm_viterbi(&self, judge: &JudgeLm, lattice: &Lattice) -> Option<Vec<WordEntry>> {
        let params = judge.params();
        let nodes = &lattice.nodes;
        // ノードごとの LM 単位と実効語コスト（辺ごとに作り直さない）
        let units: Vec<Option<crate::judge::LmUnit>> = nodes
            .iter()
            .map(|n| n.entry.as_ref().map(|e| judge.unit(e)))
            .collect();
        let word_costs: Vec<i32> = nodes
            .iter()
            .map(|n| n.entry.as_ref().map_or(0, |e| self.effective_word_cost(e)))
            .collect();
        let mut best = vec![f32::NEG_INFINITY; nodes.len()];
        let mut back = vec![usize::MAX; nodes.len()];
        best[lattice.bos_index] = 0.0;

        for pos in 0..lattice.nodes_starting_at.len() {
            for &cur in &lattice.nodes_starting_at[pos] {
                let node = &nodes[cur];
                for &prev in &lattice.nodes_ending_at[pos] {
                    if best[prev] == f32::NEG_INFINITY {
                        continue;
                    }
                    let prev_entry = nodes[prev].entry.as_ref();
                    let score = if cur == lattice.eos_index {
                        // `path_dict_cost` と同じく文末は連接行列だけ（LM の文末は見ない）
                        let conn = self.dictionary.matrix.get(nodes[prev].right_id, self.dictionary.eos_id) as i32;
                        best[prev] - conn as f32 / params.dict_scale
                    } else {
                        let (Some(e), Some(u)) = (node.entry.as_ref(), units[cur].as_ref()) else { continue };
                        let prev_last = if prev == lattice.bos_index {
                            crate::judge::LmCtx::BOS
                        } else {
                            match units[prev].as_ref() {
                                Some(u) => u.last,
                                None => continue,
                            }
                        };
                        let (conn, _) = self.base_connection_cost(
                            nodes[prev].right_id,
                            prev_entry.map(|p| p.surface.as_str()),
                            node.left_id,
                            &e.surface,
                        );
                        let lm = judge.edge_logp(prev_last, u) + params.unk_char_logp * u.unk_chars as f32;
                        let seed = match prev_entry {
                            Some(p) if self.is_seed_pair(p, e) => params.seed_bonus,
                            _ => 0.0,
                        };
                        best[prev] + params.lm_weight * lm
                            - (conn.saturating_add(word_costs[cur])) as f32 / params.dict_scale
                            + seed
                    };
                    if score > best[cur] {
                        best[cur] = score;
                        back[cur] = prev;
                    }
                }
            }
        }

        if best[lattice.eos_index] == f32::NEG_INFINITY {
            return None;
        }
        let mut path = Vec::new();
        let mut idx = back[lattice.eos_index];
        while idx != lattice.bos_index && idx != usize::MAX {
            if let Some(e) = &nodes[idx].entry {
                path.push(e.clone());
            }
            idx = back[idx];
        }
        path.reverse();
        Some(path)
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
        for path in self.judge_candidate_paths(judge, reading, pinned) {
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
