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

/// 2次 Viterbi（`lm_viterbi`）で各ノードに残す状態数
pub const JUDGE_BEAM: usize = 4;

impl ViterbiConverter {
    /// 判断層を付け替える（`None` で従来の挙動に戻る）
    pub fn set_judge(&mut self, judge: Option<Arc<JudgeLm>>) {
        self.judge = judge;
    }

    /// 1語の (辞書側の実効コスト, 個人学習のボーナス)。`effective_word_cost` から
    /// 学習ユニグラムのボーナスだけを切り出したもの（判断層では個人学習を
    /// 辞書コストと別の尺度で効かせるため）。
    pub(crate) fn judge_word_cost(&self, e: &WordEntry) -> (i32, i32) {
        let learned = self
            .learned_unigram
            .get(&(e.reading.clone(), e.surface.clone()))
            .copied()
            .unwrap_or(0);
        (self.effective_word_cost(e).saturating_add(learned), learned)
    }

    /// 2語間の (辞書側の接続コスト, 個人学習のバイグラムボーナス)
    pub(crate) fn judge_conn_cost(
        &self,
        prev_right_id: PosId,
        prev_surface: Option<&str>,
        cur_left_id: PosId,
        cur_surface: &str,
    ) -> (i32, i32) {
        let (conn, learned) = self.base_connection_cost(prev_right_id, prev_surface, cur_left_id, cur_surface);
        (conn.saturating_add(learned), learned)
    }

    /// 語列の (辞書コスト, 個人学習のボーナス)。辞書コストは単語の実効コスト＋
    /// 接続コスト（文頭・文末を含む）で、どの候補にも同じ式を当てるための
    /// 共通の物差し（`find_best_path` の手調整ガード群は含まない）。個人学習は
    /// 学習ユニグラム・学習バイグラム・離れた内容語どうしの学習連想の合計。
    pub fn path_costs(&self, path: &[WordEntry]) -> (i32, i32) {
        let mut dict = 0i32;
        let mut learned = self.path_learned_assoc_bonus(path);
        let mut prev: Option<&WordEntry> = None;
        for e in path {
            let prev_right = prev.map(|p| p.right_id).unwrap_or(self.dictionary.bos_id);
            let (conn, conn_learned) =
                self.judge_conn_cost(prev_right, prev.map(|p| p.surface.as_str()), e.left_id, &e.surface);
            let (wc, wc_learned) = self.judge_word_cost(e);
            dict = dict.saturating_add(conn).saturating_add(wc);
            learned = learned.saturating_add(conn_learned).saturating_add(wc_learned);
            prev = Some(e);
        }
        let last_right = prev.map(|p| p.right_id).unwrap_or(self.dictionary.bos_id);
        dict = dict.saturating_add(self.dictionary.matrix.get(last_right, self.dictionary.eos_id) as i32);
        (dict, learned)
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
    ///
    /// LM がトライグラム（直前2語）を見るため、状態は「ノード × 直前のノード」の
    /// 2次の Viterbi になる。各ノードでは直前ノードごとに最良の1状態だけを残し、
    /// さらにスコア上位 `JUDGE_BEAM` 個に絞る（ビーム）。トライグラムの無い
    /// モデルでは状態が直前ノードに依らないので、厳密な1次の Viterbi と一致する。
    pub fn lm_viterbi(&self, judge: &JudgeLm, lattice: &Lattice) -> Option<Vec<WordEntry>> {
        #[derive(Clone, Copy)]
        struct State {
            score: f32,
            /// 直前のノード（BOS の状態では usize::MAX）
            prev_node: usize,
            /// 直前のノードの状態リスト内の位置
            prev_state: usize,
            /// 次の語から見たトライグラム文脈（`JudgeLm::trigram_ctx`）
            tri_ctx: Option<u32>,
        }

        let params = judge.params();
        let nodes = &lattice.nodes;
        let (bos, eos) = (lattice.bos_index, lattice.eos_index);
        // ノードごとに1回だけ求めるもの（辺・状態ごとに作り直さない）:
        // LM 単位、実効語コスト、学習バイグラムの前語になりうるか、
        // コロケーションの前語になりうるか
        let units: Vec<Option<crate::judge::LmUnit>> = nodes
            .iter()
            .map(|n| n.entry.as_ref().map(|e| judge.unit(e)))
            .collect();
        let word_costs: Vec<(i32, i32)> = nodes
            .iter()
            .map(|n| n.entry.as_ref().map_or((0, 0), |e| self.judge_word_cost(e)))
            .collect();
        let bigram_prev: Vec<bool> = nodes
            .iter()
            .map(|n| {
                n.entry.as_ref().map_or(false, |e| {
                    (!self.learned_bigram.is_empty() && self.bigram_prev_surfaces.contains(&e.surface))
                        || (!self.corpus_bigram.is_empty() && self.corpus_bigram_prev_surfaces.contains(&e.surface))
                })
            })
            .collect();
        let seed_firsts: std::collections::HashSet<&str> =
            self.seeded_assoc.keys().map(|(a, _)| a.as_str()).collect();
        let seed_prev: Vec<bool> = nodes
            .iter()
            .map(|n| n.entry.as_ref().map_or(false, |e| seed_firsts.contains(e.surface.as_str())))
            .collect();
        // 文脈としての LM 語（BOS は文頭の語、語彙外は None）
        let last_word = |idx: usize| -> Option<u32> {
            if idx == bos {
                Some(crate::judge::BOS_ID)
            } else {
                units[idx].as_ref().and_then(|u| u.last.word)
            }
        };
        let mut states: Vec<Vec<State>> = vec![Vec::new(); nodes.len()];
        states[bos].push(State { score: 0.0, prev_node: usize::MAX, prev_state: usize::MAX, tri_ctx: None });

        for pos in 0..lattice.nodes_starting_at.len() {
            for &cur in &lattice.nodes_starting_at[pos] {
                let node = &nodes[cur];
                let mut cands: Vec<State> = Vec::new();
                for &prev in &lattice.nodes_ending_at[pos] {
                    if states[prev].is_empty() {
                        continue;
                    }
                    let prev_entry = nodes[prev].entry.as_ref();
                    // 2つ前の語に依らない部分（辞書コスト・コロケーション・バイグラム）
                    let (base, unit_first, bigram_logp) = if cur == eos {
                        // `path_costs` と同じく文末は連接行列だけ（LM の文末は見ない）
                        let conn = self.dictionary.matrix.get(nodes[prev].right_id, self.dictionary.eos_id) as i32;
                        (-(conn as f32) / params.dict_scale, None, 0.0)
                    } else {
                        let (Some(e), Some(u)) = (node.entry.as_ref(), units[cur].as_ref()) else { continue };
                        let prev_ctx = if prev == bos {
                            crate::judge::LmCtx::BOS
                        } else {
                            match units[prev].as_ref() {
                                Some(u) => u.last,
                                None => continue,
                            }
                        };
                        let prev_surface = prev_entry.filter(|_| bigram_prev[prev]).map(|p| p.surface.as_str());
                        let (conn, conn_learned) =
                            self.judge_conn_cost(nodes[prev].right_id, prev_surface, node.left_id, &e.surface);
                        let seed = match prev_entry {
                            Some(p) if seed_prev[prev] && self.is_seed_pair(p, e) => params.seed_bonus,
                            _ => 0.0,
                        };
                        let (wc, wc_learned) = word_costs[cur];
                        let base = -(conn.saturating_add(wc) as f32) / params.dict_scale
                            + conn_learned.saturating_add(wc_learned) as f32 / params.learn_scale
                            + seed
                            + params.lm_weight * (params.unk_char_logp * u.unk_chars as f32 + u.internal);
                        (base, Some(u.first), judge.logp(prev_ctx, u.first, u.first_class))
                    };
                    let mut best_here: Option<State> = None;
                    for (si, st) in states[prev].iter().enumerate() {
                        let lm = match unit_first {
                            Some(first) => params.lm_weight * judge.logp_tri(st.tri_ctx, first, bigram_logp),
                            None => 0.0,
                        };
                        let score = st.score + lm + base;
                        if best_here.map_or(true, |b| score > b.score) {
                            best_here = Some(State { score, prev_node: prev, prev_state: si, tri_ctx: None });
                        }
                    }
                    cands.extend(best_here);
                }
                cands.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
                cands.truncate(JUDGE_BEAM);
                if cur != eos {
                    let cur_last = units[cur].as_ref().and_then(|u| u.last.word);
                    for st in &mut cands {
                        st.tri_ctx = judge.trigram_ctx(last_word(st.prev_node), cur_last);
                    }
                }
                states[cur] = cands;
            }
        }

        let mut st = *states[eos].first()?;
        let mut path = Vec::new();
        while st.prev_node != usize::MAX && st.prev_node != bos {
            let idx = st.prev_node;
            if let Some(e) = &nodes[idx].entry {
                path.push(e.clone());
            }
            st = states[idx][st.prev_state];
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
                let (dict_cost, learned) = self.path_costs(&entries);
                let seed_hits = self.path_seed_hits(&entries);
                let score = judge.combine(lm_logp, dict_cost, learned, seed_hits);
                (entries, JudgeScore { lm_logp, lm_base, unk_chars, dict_cost, learned, seed_hits, score }, is_baseline)
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
