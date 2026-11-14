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
/// 英単語の候補の辞書コスト（辞書の外来語 コード=2234・テスト=3637 程度）
const ENGLISH_WORD_DICT_COST: i32 = 3000;
/// 共起の文脈として覚えておく直前の確定文の名詞の数
pub const CONTEXT_NOUNS_MAX: usize = 12;

/// 文章から読み取る最大文字数（カーソルに近い側を残す）
pub const FIELD_TEXT_MAX_CHARS: usize = 300;
/// 名詞の最長一致で試す最大の文字数
const FIELD_NOUN_MAX_CHARS: usize = 8;

/// 漢字・カタカナ（長音符を含む）か。名詞の拾い出しはこの文字の連続の中だけで行う
fn is_noun_char(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{30A1}'..='\u{30FA}' | 'ー' | '々')
}

/// 文章中の共起モデルの名詞を、出現順（左から）に最長一致で拾う。1文字の語は
/// 熟語の一部からの誤検出が多い（「会議」の「会」等）ので、前後がかなの
/// 孤立した1文字（「駅まで」の「駅」）のときだけ拾う。
fn extract_cooc_nouns(cooc: &crate::judge_cooc::JudgeCooc, text: &str) -> Vec<String> {
    let all: Vec<char> = text.chars().collect();
    let chars = &all[all.len().saturating_sub(FIELD_TEXT_MAX_CHARS)..];
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !is_noun_char(chars[i]) {
            i += 1;
            continue;
        }
        let run_end = (i..chars.len()).find(|&k| !is_noun_char(chars[k])).unwrap_or(chars.len());
        let run_start = i == 0 || !is_noun_char(chars[i - 1]);
        let min_len = if run_start && run_end - i == 1 { 1 } else { 2 };
        let longest = (min_len..=FIELD_NOUN_MAX_CHARS.min(run_end - i)).rev().find_map(|len| {
            let s: String = chars[i..i + len].iter().collect();
            cooc.noun_id(&s).map(|_| (s, len))
        });
        match longest {
            Some((s, len)) => {
                out.push(s);
                i += len;
            }
            None => i += 1,
        }
    }
    out
}

impl ViterbiConverter {
    /// 判断層を付け替える（`None` で従来の挙動に戻る）
    pub fn set_judge(&mut self, judge: Option<Arc<JudgeLm>>) {
        self.judge = judge;
    }

    /// 判断層の共起モデルを付け替える（`None` で共起を見ない）
    /// 英単語の自動判定の頻度表を設定する（`None` で英単語の候補を出さない）
    pub fn set_english(&mut self, english: Option<Arc<crate::english::EnglishLexicon>>) {
        self.english = english;
    }

    /// 今の入力の読みがどの打鍵から来たかを設定する（片のかなをつなげると読みになる）。
    /// 判断層はこれと一致する読み（またはその先頭部分）を変換するときだけ英単語の
    /// 候補を足す。`None` なら英単語の候補を出さない。
    /// `capital` は入力の直前に Shift で直接入力した大文字（無ければ空）。
    pub fn set_romaji_hint(&mut self, pieces: Option<Vec<crate::english::RomajiPiece>>, capital: &str) {
        self.romaji_hint = pieces;
        self.romaji_capital.clear();
        self.romaji_capital.push_str(capital);
    }

    /// 判断層のラティスに英単語の候補を足す（`crate::english`）。読みの文字位置
    /// `min_start` より前（固定した先頭文節）には足さない。
    fn add_english_nodes(&self, lattice: &mut Lattice, reading: &str, min_start: usize) {
        let (Some(english), Some(pieces)) = (self.english.as_ref(), self.romaji_hint.as_ref()) else {
            return;
        };
        // 打鍵の記録が今の読み（の先頭部分）と一致するときだけ使う
        let mut covered = 0usize;
        let mut n = 0usize;
        for p in pieces {
            if !reading[covered..].starts_with(p.kana.as_str()) {
                break;
            }
            covered += p.kana.len();
            n += 1;
        }
        if n == 0 {
            return;
        }
        let byte_at: Vec<usize> = reading
            .char_indices()
            .map(|(b, _)| b)
            .chain(std::iter::once(reading.len()))
            .collect();
        let continuation = if min_start == 0 && !self.romaji_capital.is_empty() {
            english.continuation_spans(&pieces[..n], &self.romaji_capital)
        } else {
            Vec::new()
        };
        for span in english.spans(&pieces[..n]).into_iter().chain(continuation) {
            if span.start < min_start {
                continue;
            }
            let (start, end) = (byte_at[span.start], byte_at[span.end]);
            for e in crate::english::english_entries(&span, &reading[start..end]) {
                lattice.add_word(start, end, e);
            }
        }
    }

    pub fn set_cooc(&mut self, cooc: Option<Arc<crate::judge_cooc::JudgeCooc>>) {
        self.cooc = cooc;
    }

    /// 確定した文節（読み, 表記, 品詞）の名詞を、次の入力の共起の文脈として覚える
    pub fn note_committed(&mut self, segments: &[(String, String, String)]) {
        for (_, surface, pos) in segments {
            if crate::judge_cooc::is_cooc_noun_entry(pos, surface) {
                self.context_nouns.retain(|n| n != surface);
                self.context_nouns.insert(0, surface.clone());
            }
        }
        self.context_nouns.truncate(CONTEXT_NOUNS_MAX);
    }

    /// 共起の文脈（直前の確定文の名詞）を捨てる（フォーカス移動など、前の文と
    /// 話題がつながらないとき）
    pub fn clear_context(&mut self) {
        self.context_nouns.clear();
    }

    /// 入力欄に既にある文章（カーソルより前）の名詞を共起の文脈に加える。
    ///
    /// 自分の確定だけを文脈にすると、フォーカス移動やクリックで文脈が消え、
    /// 返信先のメールや前に書いた段落など「画面には見えている話題」が使えない。
    /// 文章は漢字かな混じりの表記なので、共起モデルの名詞の語彙を表記で
    /// 最長一致して拾う（形態素解析はしない。漢字・カタカナの連続の中だけを見る）。
    /// カーソルに近い名詞ほど新しい文脈として先頭に来る。
    pub fn note_field_text(&mut self, text: &str) {
        let Some(cooc) = self.cooc.as_ref() else { return };
        let nouns = extract_cooc_nouns(cooc, text);
        for surface in nouns {
            self.context_nouns.retain(|n| n != &surface);
            self.context_nouns.insert(0, surface);
        }
        self.context_nouns.truncate(CONTEXT_NOUNS_MAX);
    }

    /// 共起モデルの文脈語ID: 直前の確定文の名詞 ＋ `extra`
    fn cooc_context_ids<'a>(
        &self,
        cooc: &crate::judge_cooc::JudgeCooc,
        extra: impl Iterator<Item = &'a str>,
    ) -> Vec<u32> {
        let mut ids: Vec<u32> = self.context_nouns.iter().filter_map(|s| cooc.noun_id(s)).collect();
        ids.extend(extra.filter_map(|s| cooc.noun_id(s)));
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// 語列の共起の加点（重み前）: 候補中の名詞・動詞（`cooc_target_key_entry`）
    /// ごとに、文中の他の名詞と直前の確定文の名詞の中で最も強い PMI
    /// （上限 `cooc_cap`）の合計
    pub fn path_cooc_bonus(&self, path: &[WordEntry]) -> f32 {
        let (Some(judge), Some(cooc)) = (self.judge.as_ref(), self.cooc.as_ref()) else {
            return 0.0;
        };
        let cap = judge.params().cooc_cap;
        let targets: Vec<&WordEntry> = path
            .iter()
            .filter(|e| crate::judge_cooc::cooc_target_key_entry(&e.pos, &e.surface, &e.reading).is_some())
            .collect();
        if targets.is_empty() {
            return 0.0;
        }
        let nouns = path
            .iter()
            .filter(|e| crate::judge_cooc::is_cooc_noun_entry(&e.pos, &e.surface))
            .map(|e| e.surface.as_str());
        let ctx = self.cooc_context_ids(cooc, nouns);
        targets.iter().map(|e| self.cooc_target_bonus(cooc, e, &ctx, cap)).sum()
    }

    /// 1語の共起の加点（重み前）。名詞は文脈との最も強い PMI。
    ///
    /// 動詞は、同じ読みの漢字違いの動詞（活か/行か/生か）のうち最も結びつく
    /// ものを基準にした差（0 以下）に `COOC_VERB_SCALE` を掛ける。動詞の見出しは
    /// 漢字を含む形にしか無いので、生の PMI を足すとひらがなで書くのが普通の
    /// 動詞まで漢字に寄せてしまう（評価用データで できる→出来る、つける→付ける、
    /// なす→成す が多数悪化、2026-11-14）。差にすれば漢字違いの間の選び分けだけに効き、
    /// ひらがなの表記より有利になることは無い。
    fn cooc_target_bonus(&self, cooc: &crate::judge_cooc::JudgeCooc, e: &WordEntry, ctx: &[u32], cap: f32) -> f32 {
        let Some(key) = crate::judge_cooc::cooc_target_key_entry(&e.pos, &e.surface, &e.reading) else {
            return 0.0;
        };
        if !key.starts_with("動:") {
            return cooc.assoc(&key, ctx, cap);
        }
        let own = cooc.assoc(&key, ctx, cap);
        let best = self
            .merged_lookup(&e.reading)
            .map(|alts| {
                alts.iter()
                    .filter_map(|a| crate::judge_cooc::verb_key_entry(&a.pos, &a.surface, &a.reading))
                    .map(|k| cooc.assoc(&k, ctx, cap))
                    .fold(own, f32::max)
            })
            .unwrap_or(own);
        cooc_verb_scale() * (own - best)
    }

    /// ラティスの各ノードの共起の加点（重み込み）。文脈は1回目の探索で選ばれた
    /// 経路 `first` の名詞のうち、そのノードと読みの範囲が重ならないもの ＋
    /// 直前の確定文の名詞。
    fn cooc_node_bonus(&self, judge: &JudgeLm, lattice: &Lattice, first: &[WordEntry]) -> Option<Vec<f32>> {
        let cooc = self.cooc.as_ref()?;
        let params = judge.params();
        // 1回目の経路の名詞と、その読みのバイト範囲
        let mut spans: Vec<(usize, usize, &str)> = Vec::new();
        let mut pos = 0usize;
        for e in first {
            let end = pos + e.reading.len();
            if crate::judge_cooc::is_cooc_noun_entry(&e.pos, &e.surface) {
                spans.push((pos, end, e.surface.as_str()));
            }
            pos = end;
        }
        if spans.is_empty() && self.context_nouns.is_empty() {
            return None;
        }
        let bonus = lattice
            .nodes
            .iter()
            .map(|n| {
                let Some(e) = n.entry.as_ref() else { return 0.0 };
                if crate::judge_cooc::cooc_target_key_entry(&e.pos, &e.surface, &e.reading).is_none() {
                    return 0.0;
                }
                let others = spans
                    .iter()
                    .filter(|&&(s, t, _)| t <= n.start || s >= n.end)
                    .map(|&(_, _, w)| w);
                let ctx = self.cooc_context_ids(cooc, others);
                params.cooc_weight * self.cooc_target_bonus(cooc, e, &ctx, params.cooc_cap)
            })
            .collect();
        Some(bonus)
    }

    /// 1語の (辞書側の実効コスト, 個人学習のボーナス)。`effective_word_cost` から
    /// 学習ユニグラムのボーナスだけを切り出したもの（判断層では個人学習を
    /// 辞書コストと別の尺度で効かせるため。辞書側は学習を入れずに求める）。
    ///
    /// 接頭詞には学習ユニグラムを効かせない。接頭詞の学習（同社・同年 等の「同」）は
    /// 「どう」の漢字の選び分けにしか意味が無いのに、1語ごとに数nats乗るため
    /// 「接頭詞＋何でも」の分割をまとめて後押ししてしまう（どうかせん→同河川、
    /// 2026-10-09。従来エンジンの再摘果 [[bonused-prefix-stacking-conversion-bug]] と
    /// 同じ形）。接頭詞の漢字は LM が後続語との並びで選ぶ。
    pub(crate) fn judge_word_cost(&self, e: &WordEntry) -> (i32, i32) {
        let learned = if e.pos.starts_with("接頭詞") {
            0
        } else {
            self.learned_unigram
                .get(&(e.reading.clone(), e.surface.clone()))
                .copied()
                .unwrap_or(0)
        };
        // 英単語の候補の `cost` には英単語の対数確率が入っている（`english_entries`）。
        // 辞書コストとしては辞書の外来語と同じくらいの一定値で扱う
        if crate::english::is_english_entry(e) {
            return (ENGLISH_WORD_DICT_COST, learned);
        }
        (self.word_cost_with_learned(e, 0), learned)
    }

    /// 1文字漢字が2つ続き、読みをつなげると辞書の内容語（2文字以上の表記）になる
    /// 並びで、2語の学習ユニグラム（`prev_learned`/`cur_learned`）を打ち消す量。
    /// 「両方」が「両|方」に分かれて確定されると「りょう→両」が単独で学習され、
    /// それが別の語の「じゅうりょう」まで「十|両」に割ってしまう（実測:
    /// じゅうりょうかきん→十両課金、2026-10-29）。断片の学習は「その2文字で
    /// 1語になる並び」では効かせず、漢字の選び分けは LM に任せる。断片を
    /// さらに学習して悪化する自己強化も止まる
    /// （[[learned-hiragana-fragments-longer-word-bug]] と同じ形）。
    /// A|B|C で AB・BC どちらも語になる場合 B の分は2回引かれるが、
    /// それは断片化をより不利にする向きなので構わない。
    pub(crate) fn single_kanji_pair_learned_refund(
        &self,
        prev: &WordEntry,
        cur: &WordEntry,
        prev_learned: i32,
        cur_learned: i32,
    ) -> i32 {
        let refund = prev_learned.saturating_add(cur_learned);
        if refund == 0 || !is_single_kanji_surface(&prev.surface) || !is_single_kanji_surface(&cur.surface) {
            return 0;
        }
        let combined_reading = format!("{}{}", prev.reading, cur.reading);
        let forms_word = self.merged_lookup(&combined_reading).is_some_and(|entries| {
            entries.iter().any(|e| is_content_pos(&e.pos) && e.surface.chars().count() >= 2)
        });
        if forms_word { refund } else { 0 }
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
        let mut prev_wc_learned = 0;
        for e in path {
            let prev_right = prev.map(|p| p.right_id).unwrap_or(self.dictionary.bos_id);
            let (conn, conn_learned) =
                self.judge_conn_cost(prev_right, prev.map(|p| p.surface.as_str()), e.left_id, &e.surface);
            let (wc, wc_learned) = self.judge_word_cost(e);
            let refund = prev.map_or(0, |p| self.single_kanji_pair_learned_refund(p, e, prev_wc_learned, wc_learned));
            dict = dict.saturating_add(conn).saturating_add(wc);
            learned = learned.saturating_add(conn_learned).saturating_add(wc_learned).saturating_sub(refund);
            prev = Some(e);
            prev_wc_learned = wc_learned;
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
        let mut lattice = if pinned.is_empty() {
            self.build_lattice(reading)
        } else {
            self.build_pinned_lattice(reading, pinned)
        };
        let pinned_chars: usize = pinned.iter().map(|e| e.reading.chars().count()).sum();
        self.add_english_nodes(&mut lattice, reading, pinned_chars);
        let Some(path) = self.lm_viterbi(judge, &lattice, None) else {
            return Vec::new();
        };
        let mut paths = vec![path];
        // 共起モデルがあれば、1回目の経路の名詞（と直前の確定文の名詞）を文脈に
        // もう一度探索する（文中の離れた名詞どうしの結びつきは辺に分解できない
        // ため、文脈を固定した2回目の探索でノードごとの加点として扱う）
        if let Some(bonus) = self.cooc_node_bonus(judge, &lattice, &paths[0]) {
            if let Some(second) = self.lm_viterbi(judge, &lattice, Some(&bonus)) {
                paths.push(second);
            }
        }
        let n_pinned = pinned.len();
        let mut out = Vec::new();
        for path in paths {
            let n = n_pinned.min(path.len());
            let mut repaired = path[..n].to_vec();
            repaired.extend(self.repair_single_kanji_fragments(path[n..].to_vec()));
            out.push(path);
            out.push(repaired);
        }
        out
    }

    /// ラティス上で総合スコア（`JudgeLm::combine` と同じ式。ただし離れた語どうしの
    /// 学習連想は辺に分解できないので含めない）が最大の経路を求める。
    /// 到達できなければ None。
    ///
    /// LM がトライグラム（直前2語）を見るため、状態は「ノード × 直前のノード」の
    /// 2次の Viterbi になる。各ノードでは直前ノードごとに最良の1状態だけを残し、
    /// さらにスコア上位 `JUDGE_BEAM` 個に絞る（ビーム）。トライグラムの無い
    /// モデルでは状態が直前ノードに依らないので、厳密な1次の Viterbi と一致する。
    pub fn lm_viterbi(&self, judge: &JudgeLm, lattice: &Lattice, node_bonus: Option<&[f32]>) -> Option<Vec<WordEntry>> {
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
                        let refund = match prev_entry {
                            Some(p) => self.single_kanji_pair_learned_refund(p, e, word_costs[prev].1, wc_learned),
                            None => 0,
                        };
                        let english_cont = match prev_entry {
                            Some(p) => judge.english_continuation(Some(p), e),
                            None => 0.0,
                        };
                        let base = english_cont - (conn.saturating_add(wc) as f32) / params.dict_scale
                            + conn_learned.saturating_add(wc_learned).saturating_sub(refund) as f32 / params.learn_scale
                            + seed
                            + node_bonus.map_or(0.0, |b| b[cur])
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

    /// 語列1本の総合スコア（`judge_candidates` / `lm_viterbi` と同じ式、大きいほど良い）。
    /// 候補一覧の並び順を自動変換と同じ物差しにそろえるために使う。
    /// 判断層が無ければ None。
    pub fn judge_path_score(&self, path: &[WordEntry]) -> Option<f32> {
        let judge = self.judge.as_ref()?;
        let lm = judge.sequence_logp(None, path);
        let (dict_cost, learned) = self.path_costs(path);
        Some(judge.combine(lm, dict_cost, learned, self.path_seed_hits(path), self.path_cooc_bonus(path)))
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
                let cooc = self.path_cooc_bonus(&entries);
                let score = judge.combine(lm_logp, dict_cost, learned, seed_hits, cooc);
                (entries, JudgeScore { lm_logp, lm_base, unk_chars, dict_cost, learned, seed_hits, cooc, score }, is_baseline)
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
        let mut best = judged.swap_remove(0);
        crate::english::space_between_english_words(&mut best.entries);
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

/// 動詞の共起の差に掛ける係数。評価用に環境変数 `JUDGE_COOC_VERB_W` で上書きできる
fn cooc_verb_scale() -> f32 {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("JUDGE_COOC_VERB_W").ok().and_then(|v| v.parse().ok()).unwrap_or(COOC_VERB_SCALE)
    })
}

/// 動詞の共起の差に掛ける既定の係数。既定は 0（使わない）。
///
/// 2026-11-14、Wikipedia 全量で学習した名詞＋動詞の共起モデルを評価用データ
/// （2万文）で測ると、係数 1.0 で改善 39 / 悪化 63、0.5 でも 24 / 28 と悪化が
/// 上回った。PMI は珍しい語ほど高く出るので、見る→観る・読む→詠む・取る→採る の
/// ように珍しい漢字に寄り、見る/みる のような表記の揺れにも振り回される。
/// 同じ文の名詞との共起は動詞の漢字を決める手がかりとして粗すぎるので、
/// 「名詞＋助詞＋動詞」（キャリア に 活かす、責任 を 負う）の組で学ぶ方式に
/// 作り直すまでは使わない。
const COOC_VERB_SCALE: f32 = 0.0;
