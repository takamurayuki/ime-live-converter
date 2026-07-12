use crate::candidate::hiragana_to_katakana;
use crate::dictionary::{Dictionary, WordEntry, PosId};
use std::collections::HashMap;

mod incremental;
mod lattice;
mod nbest;
mod scoring;
#[cfg(test)]
mod tests;

pub use incremental::*;
pub use lattice::*;
pub(crate) use nbest::*;
pub use scoring::*;

/// Viterbi変換エンジン
#[derive(Debug)]
pub struct ViterbiConverter {
    /// 辞書
    pub dictionary: Dictionary,
    /// 未知語のデフォルト品詞ID
    pub unknown_id: PosId,
    /// 未知語のコスト
    pub unknown_cost: i32,
    /// カタカナ候補ノードに使う品詞ID（名詞相当）
    pub katakana_pos_id: PosId,
    /// カタカナ候補ノードを生成するか
    pub enable_katakana_fallback: bool,
    /// カタカナ候補の基底コスト（実コスト = base + len * step）
    pub katakana_base_cost: i32,
    /// カタカナ候補の文字あたりコスト
    pub katakana_step_cost: i32,
    /// カタカナ候補を生成する最小ひらがな文字数
    pub katakana_min_len: usize,
    /// カタカナ候補を生成する最大ひらがな文字数
    pub katakana_max_len: usize,
    /// 開始位置が辞書ヒットしている場合に上乗せするペナルティコスト
    /// （短い助詞連続は辞書勝ち、長い外来語パターンはカタカナ勝ちになるよう調整）
    pub katakana_dict_start_penalty: i32,
    /// 1文字漢字の単独語に上乗せするコスト
    ///
    /// IPA辞書は解析用のため、1文字漢字の名詞（教・卿・挟 など）が
    /// 単独語として実際の出現頻度より低コストに設定されていることが多い。
    /// そのままだと「きょうは」→「教は」のように稀な1文字漢字が
    /// 選ばれてしまうため、ラティス構築時に実効コストを底上げする。
    pub single_kanji_penalty: i32,
    /// 学習したユニグラム: (読み, 表記) → コスト減額（正=優先度上げ）
    ///
    /// ユーザーが確定した語を、次回のライブ変換で優先させる。
    /// 使うほど賢くなる核となる仕組み。
    pub learned_unigram: HashMap<(String, String), i32>,
    /// 学習したバイグラム: (前の表記, 次の表記) → 接続コスト減額
    ///
    /// ユーザーが確定した文の語のつながりを学習し、文全体の整合性を上げる。
    pub learned_bigram: HashMap<(String, String), i32>,
    /// 学習した内容語の連想: (前の内容語, 次の内容語) → スコア
    ///
    /// 助詞・助動詞を飛ばした「内容語どうしの結びつき」。
    /// 「会社…帰社」「新聞…記者」のように、助詞を挟んで離れた語の
    /// 繋がりを覚える。N-best候補の再ランクに使い、学習した繋がりを
    /// 最も多く含む変換を「正しい」として選ぶ。
    pub learned_assoc: HashMap<(String, String), i32>,
    /// 学習したひらがな優先: 読み → コスト減額
    ///
    /// ユーザーが Esc でひらがなに戻した読みを覚え、次回からその読みを
    /// ひらがなのまま出しやすくする（例: 「したい」を「慕い」にしない）。
    /// ラティスにその読みのひらがなノードを低コストで追加して実現する。
    pub learned_hiragana: HashMap<String, i32>,
}

impl ViterbiConverter {
    pub fn new(dictionary: Dictionary) -> Self {
        // カタカナ候補・未知語ノードに使う文脈IDは、辞書に実在する
        // 一般名詞のIDを流用する。文脈ID体系は辞書ごとに異なる
        // （sample=1, IPA辞書の名詞一般=1285 など）ため、固定値では
        // 接続コストがでたらめになり未知語が不当に有利/不利になる。
        let noun_id = ["ひと", "やま", "ほん", "みず"]
            .iter()
            .find_map(|reading| {
                dictionary.lookup(reading).and_then(|entries| {
                    entries
                        .iter()
                        .filter(|e| e.left_id == e.right_id)
                        .min_by_key(|e| e.cost)
                        .map(|e| e.left_id)
                })
            })
            .unwrap_or(1);

        let mut conv = Self {
            dictionary,
            unknown_id: noun_id,
            unknown_cost: 10000, // 未知語は高コスト
            katakana_pos_id: noun_id,
            enable_katakana_fallback: true,
            // 3文字なら 4000+3*1000=7000、辞書名詞(5000前後)よりは高く、3未知語(45000)より低い
            katakana_base_cost: 4000,
            katakana_step_cost: 1000,
            katakana_min_len: 2,
            katakana_max_len: 8,
            katakana_dict_start_penalty: 1500,
            // 300: 「きょうは→教は」のような僅差(88)の誤選択は覆せて、
            // かつ 私・雨 のような一般的な1文字漢字がカタカナ/未知語に
            // 負けない値。大きくすると 雨→アメ 等の副作用が出る。
            single_kanji_penalty: 300,
            learned_unigram: HashMap::new(),
            learned_bigram: HashMap::new(),
            learned_assoc: HashMap::new(),
            learned_hiragana: HashMap::new(),
        };
        conv.seed_common_words();
        conv
    }

    /// 頻出語プリセットを learned_unigram に薄く入れる（初回変換の品質向上）
    fn seed_common_words(&mut self) {
        for &(reading, surface) in COMMON_WORD_SEED {
            self.learned_unigram
                .entry((reading.to_string(), surface.to_string()))
                .or_insert(COMMON_WORD_SEED_BONUS);
        }
        // 助詞「を」は IPA辞書の安いカタカナ「ヲ」に負け、かつ他語の部分
        // 文字列にならない（安全）ため強めに優先する。
        // （「ほん」等は「にほんご」の部分列になり強優先すると壊れるので
        //  中程度プリセット止まりにする）
        self.learned_unigram
            .insert(("を".to_string(), "を".to_string()), 8000);
        // 「考える」は辞書コストが高く(7049)、単独だと安いカタカナ断片
        // 「カン」+助詞の分割に負けるため強めに優先する。
        self.learned_unigram
            .insert(("かんがえる".to_string(), "考える".to_string()), 4000);
        // 「後ろ」も辞書コストが高く(6292)、安いカタカナ「ウシ」+炉 の分割に
        // 負ける（うしろ→ウシ炉）ため強めに優先する。
        self.learned_unigram
            .insert(("うしろ".to_string(), "後ろ".to_string()), 4000);
        // 「文章」は「文書(ぶんしょ,超低コスト1432)＋う」の分割に負けやすい
        // （ぶんしょう→文書雨/文書う）ため強めに優先する。
        self.learned_unigram
            .insert(("ぶんしょう".to_string(), "文章".to_string()), 9000);
        // 「問題ない」も、以内(いない)が異常に安く内部接続コストも負のため
        // 「揉ん+だ+以内」の分割(合計コストが極端に低い)に負ける。補助辞書の
        // 「問題ない」を強めに優先して単独でも正しく出す。
        self.learned_unigram
            .insert(("もんだいない".to_string(), "問題ない".to_string()), 8000);
        // 「ごじ」は ご(５)+じ(時) の数字+助数詞で「５時」に割れる。誤字校正が
        // 主目的なので「誤字」を優先する（時間は文脈/学習で選び直せる）。
        self.learned_unigram
            .insert(("ごじ".to_string(), "誤字".to_string()), 4000);

        // 既定でひらがな優先にする読み。漢字表記(慕い)が稀で、ひらがな
        // (助動詞「〜したい」)の方が圧倒的に多い。ユーザーが Esc で戻さ
        // なくても最初からひらがなで出るようにする。
        //（「たい」等の短い読みは「対象」などを壊すため入れない）
        self.learned_hiragana
            .entry("したい".to_string())
            .or_insert(COMMON_WORD_SEED_BONUS);
    }

    /// 学習データをすべてクリア（頻出語プリセットは残す）
    pub fn clear_learning(&mut self) {
        self.learned_unigram.clear();
        self.learned_bigram.clear();
        self.learned_assoc.clear();
        self.learned_hiragana.clear();
        self.seed_common_words();
    }

    /// ひらがな優先（Escで戻した読み）を頻度から設定する
    pub fn learn_hiragana(&mut self, reading: &str, freq: u32) {
        // ひらがなノードを既存語より優先させるが、強すぎると周囲の分割を
        // 乱すため中程度に抑える（Escで漢字学習は忘れるので過剰に強くしない）。
        let bonus = frequency_to_bonus(freq).clamp(COMMON_WORD_SEED_BONUS, 3000);
        self.learned_hiragana.insert(reading.to_string(), bonus);
    }

    /// 指定した読みの「漢字/カタカナ変換」の学習を忘れる（メモリ側）
    ///
    /// Esc でひらがなに戻したとき、その読みで過去に学習した表記の
    /// 優先を打ち消し、ひらがなが勝てるようにする。
    pub fn forget_reading(&mut self, reading: &str) {
        self.learned_unigram.retain(|(r, _), _| r != reading);
    }

    /// 誤学習を1件だけ忘れる（候補一覧の Delete によるリセット用）。
    ///
    /// `forget_reading` は読み全体を消すが、こちらは (reading, surface) の1組だけ。
    /// 同じ読みの他の正しい学習は残す。あわせて、その表記が絡むバイグラム・
    /// 連想の学習も消して、誤変換の再浮上を防ぐ。
    pub fn forget_unigram(&mut self, reading: &str, surface: &str) {
        self.learned_unigram.remove(&(reading.to_string(), surface.to_string()));
        self.learned_bigram.retain(|(p, s), _| p != surface && s != surface);
        self.learned_assoc.retain(|(p, c), _| p != surface && c != surface);
    }

    /// ユニグラム（読み→表記）の学習を頻度から設定する
    pub fn learn_unigram(&mut self, reading: &str, surface: &str, freq: u32) {
        // ボーナスが大きすぎると、短い語（ぶんしょ→文書）が長い語（ぶんしょう
        // →文章）を分断してしまう（文書＋う に割れる）。同音語を選ぶには十分
        // だが分割を壊さない程度に上限を設ける。
        let bonus = frequency_to_bonus(freq).min(UNIGRAM_BONUS_CAP);
        self.learned_unigram
            .insert((reading.to_string(), surface.to_string()), bonus);
    }

    /// バイグラム（前の表記→次の表記）の学習を頻度から設定する
    pub fn learn_bigram(&mut self, prev_surface: &str, surface: &str, freq: u32) {
        // バイグラムは接続コスト（通常 0〜8000 程度）の減額に使う。単語コスト
        // 用の frequency_to_bonus は最大 20000 と大きすぎ、「て→い」等の高頻度
        // 活用バイグラムが接続を大きくマイナスにして断片パス（てい系 等）を
        // 生む。接続コストの規模に収まる上限に抑える。
        let bonus = frequency_to_bonus(freq).min(BIGRAM_BONUS_CAP);
        self.learned_bigram
            .insert((prev_surface.to_string(), surface.to_string()), bonus);
    }

    /// 内容語連想（前の内容語→次の内容語）の学習を頻度から設定する
    pub fn learn_assoc(&mut self, prev_content: &str, content: &str, freq: u32) {
        self.learned_assoc
            .insert((prev_content.to_string(), content.to_string()), frequency_to_bonus(freq));
    }

    /// 文脈（内容語の繋がり）を考慮して変換する
    ///
    /// まず通常の1-bestで変換し、文中の各内容語について、同じ読みの
    /// 別表記に差し替えると「文中の他の内容語との学習済み連想」が強まる
    /// 場合、コスト差を上回る限り差し替える。これにより「駅の汽車 /
    /// 新聞の記者」のように、助詞を挟んで離れた前後の語の関係から
    /// 尤もらしい変換を選ぶ（学習が無ければ通常の1-bestのまま）。
    pub fn convert_context_aware(&self, reading: &str) -> Vec<WordEntry> {
        // 入力が助詞1文字だけ（は・と・も 等）のときはひらがなのままにする。
        // 接続コストの都合で単独だと漢字（刃・賭・藻）に負けるのを防ぐ。
        // 文中の助詞や長い語には影響しない（reading 全体が1助詞のときのみ）。
        if is_lone_particle(reading) {
            return vec![WordEntry {
                surface: reading.to_string(),
                reading: reading.to_string(),
                left_id: self.dictionary.bos_id,
                right_id: self.dictionary.eos_id,
                cost: 0,
                pos: "助詞-係助詞-*-*".to_string(),
            }];
        }
        let base = self.convert(reading);
        self.rerank_by_assoc(base)
    }

    /// 学習した内容語連想で 1-best を微調整する（差し替え）
    fn rerank_by_assoc(&self, mut base: Vec<WordEntry>) -> Vec<WordEntry> {
        if self.learned_assoc.is_empty() || base.len() < 2 {
            return base;
        }

        // 文中の内容語の (位置, 表記) を集める
        let content: Vec<(usize, String)> = base
            .iter()
            .enumerate()
            .filter(|(_, e)| is_content_pos(&e.pos) && e.surface != e.reading)
            .map(|(i, e)| (i, e.surface.clone()))
            .collect();

        // 各内容語について、連想が強まる別表記へ差し替えを検討する
        for &(i, _) in &content {
            let cur = base[i].clone();
            let Some(alts) = self.dictionary.lookup(&cur.reading) else {
                continue;
            };
            let cur_cost = self.effective_word_cost(&cur);

            let mut best: Option<WordEntry> = None;
            let mut best_gain = 0i32;
            for alt in alts {
                if alt.surface == cur.surface {
                    continue;
                }
                // 文中の他の内容語との連想スコア（前後両方向）
                let mut assoc = 0i32;
                for (j, w) in &content {
                    if *j == i {
                        continue;
                    }
                    assoc += self
                        .learned_assoc
                        .get(&(w.clone(), alt.surface.clone()))
                        .copied()
                        .unwrap_or(0);
                    assoc += self
                        .learned_assoc
                        .get(&(alt.surface.clone(), w.clone()))
                        .copied()
                        .unwrap_or(0);
                }
                if assoc == 0 {
                    continue;
                }
                // 差し替えの純利得 = 連想スコア - コスト増加
                let net = assoc.saturating_sub(self.effective_word_cost(alt) - cur_cost);
                if net > best_gain {
                    best_gain = net;
                    best = Some(alt.clone());
                }
            }
            if let Some(alt) = best {
                base[i] = alt;
            }
        }
        base
    }

    /// 文脈考慮変換の結果を文字列で返す
    pub fn convert_context_aware_to_string(&self, reading: &str) -> String {
        self.convert_context_aware(reading)
            .iter()
            .map(|e| e.surface.as_str())
            .collect()
    }

    /// 「もしかして」誤字補正候補を返す。戻り値: (補正後の読み, 補正後の表記)。
    ///
    /// 方針（ユーザー要望）: **文全体ではなく、自動変換に失敗した「単語単位」**に
    /// だけ補正を出す。正しく漢字に変換できている入力にはノイズを出さない。
    ///
    /// 「変換に失敗した単語」= 変換結果が未知語（ひらがなのまま）または
    /// カタカナ・フォールバック（例: きづついて→キヅツイテ）になっている文節。
    /// その語の読みに対してのみ、実在する辞書語へ寄せる補正を探す:
    ///  1. 取り違えやすいかな置換／1文字削除・小書き挿入の1編集変種（実在語のみ採用）
    ///  2. 辞書のあいまい検索（Trie上の Levenshtein、距離2まで）で見つかる実在読み
    /// 例: きづついて→傷ついて、がっこ→学校、わたしわ 単体→私は。
    pub fn fuzzy_suggest(&self, reading: &str) -> Option<(String, String)> {
        let total_n = reading.chars().count();
        if total_n < 2 || total_n > 32 {
            return None;
        }
        let segments = self.convert_context_aware(reading);

        // 変換に失敗した文節（未知語 / カタカナ・フォールバック）を連続でまとめ、
        // グループごとに (開始文節index, 終了index排他, 読み) を得る。
        let mut groups: Vec<(usize, usize, String)> = Vec::new();
        let mut i = 0;
        while i < segments.len() {
            if is_failed_segment(&segments[i]) {
                let start = i;
                let mut r = String::new();
                while i < segments.len() && is_failed_segment(&segments[i]) {
                    r.push_str(&segments[i].reading);
                    i += 1;
                }
                groups.push((start, i, r));
            } else {
                i += 1;
            }
        }

        // 対象は2文字以上の失敗語。複数あれば末尾（入力中に近い）を優先。
        let (gs, ge, target) = groups
            .into_iter()
            .filter(|(_, _, r)| r.chars().count() >= 2)
            .next_back()?;
        let corrected = self.best_word_correction(&target)?;

        // 全体の読みを組み立て直し（対象グループだけ差し替え）
        let mut new_reading = String::new();
        for (idx, seg) in segments.iter().enumerate() {
            if idx < gs || idx >= ge {
                new_reading.push_str(&seg.reading);
            } else if idx == gs {
                new_reading.push_str(&corrected);
            }
        }

        let base_surface = self.convert_context_aware_to_string(reading);
        let surface = self.convert_context_aware_to_string(&new_reading);
        // 補正結果が元と同じ、または全てひらがな（＝改善になっていない）なら出さない
        if surface == base_surface
            || surface.chars().all(|c| ('\u{3041}'..='\u{3096}').contains(&c))
        {
            return None;
        }
        Some((new_reading, surface))
    }

    /// 失敗した1単語の読みを補正した候補（読み）を返す。
    ///
    /// 候補: (1) 取り違えやすいかなの1編集変種、(2) 辞書Trieの距離1あいまい検索。
    /// 採用条件は「変換すると失敗文節（未知語/カタカナ化）が残らず、実在の
    /// 辞書語（漢字/カタカナ）で構成される」こと。＝存在しない語を組み立てない。
    /// 編集距離が最小、次に変換コストが最小のものを選ぶ。
    ///
    /// 性能: 毎打鍵でフックスレッドから呼ばれるため軽さが最優先。Trie検索は
    /// 距離1のみ（距離2は辞書全体で 100ms超になりフックが無視され生キーが漏れる）。
    fn best_word_correction(&self, word: &str) -> Option<String> {
        let n = word.chars().count();
        if !(2..=16).contains(&n) {
            return None;
        }
        // 候補（読み, 編集距離）を集める。手書き変種は距離1、Trieは距離1のみ。
        let mut cands: Vec<(String, usize)> = Vec::new();
        for (v, _is_del) in fuzzy_variants(word) {
            if v != word {
                cands.push((v, 1));
            }
        }
        for (v, dist) in self.dictionary.fuzzy_readings(word, 1) {
            if v != word && dist > 0 {
                cands.push((v, dist));
            }
        }
        cands.sort();
        cands.dedup();

        // 距離が小さいほど、次にコストが低いほど良い。
        let mut best: Option<(String, usize, i32)> = None;
        for (v, dist) in cands {
            let (path, cost) = self.convert_with_cost(&v);
            if cost >= i32::MAX / 2 {
                continue;
            }
            // 補正後に未変換の失敗文節が残るなら、実在語に補正できていない
            if path.iter().any(is_failed_segment) {
                continue;
            }
            // 漢字/カタカナ実語になっていること（ひらがなのままは補正にならない）
            let has_real = path
                .iter()
                .any(|e| e.surface.chars().any(|c| !('\u{3041}'..='\u{3096}').contains(&c)));
            if !has_real {
                continue;
            }
            let better = match &best {
                None => true,
                Some((_, bd, bc)) => dist < *bd || (dist == *bd && cost < *bc),
            };
            if better {
                best = Some((v, dist, cost));
            }
        }
        best.map(|(v, _, _)| v)
    }

    /// 読みが「失敗文節を残さず綺麗に変換できる」なら (表記, 総コスト) を返す。
    /// 失敗文節（未知語/カタカナ化）が残る場合は None。全ひらがなでも可。
    /// ローマ字取り残しの補正（フック側）で、直した読みが本当に変換成功するか、
    /// またどれくらい自然か（コスト）で候補を比較するのに使う。
    /// コストが低いほど自然（＝意味の通る語列）。造語の寄せ集めは高コストになる。
    pub fn clean_reading(&self, reading: &str) -> Option<(String, i32)> {
        if reading.is_empty() {
            return None;
        }
        let (path, cost) = self.convert_with_cost(reading);
        if path.is_empty() || cost >= i32::MAX / 2 || path.iter().any(is_failed_segment) {
            return None;
        }
        let surface: String = path.iter().map(|e| e.surface.as_str()).collect();
        Some((surface, cost))
    }

    /// 単語の実効コスト（1文字漢字ペナルティ・学習ユニグラムを反映）
    fn effective_word_cost(&self, e: &WordEntry) -> i32 {
        let pen = single_kanji_penalty(&e.surface, self.single_kanji_penalty);
        let bonus = self
            .learned_unigram
            .get(&(e.reading.clone(), e.surface.clone()))
            .copied()
            .unwrap_or(0);
        (e.cost as i32).saturating_add(pen).saturating_sub(bonus)
    }

    /// ひらがな文字列を最適な単語列に変換
    pub fn convert(&self, hiragana: &str) -> Vec<WordEntry> {
        self.convert_with_cost(hiragana).0
    }

    /// ひらがな文字列を変換し、最適パスの総コストも返す
    ///
    /// コストは低いほど自然な変換。誤字補正候補の妥当性検証
    /// （補正後の方がコストが下がるか）などに使う。
    /// EOSに到達できない場合は i32::MAX を返す。
    pub fn convert_with_cost(&self, hiragana: &str) -> (Vec<WordEntry>, i32) {
        if hiragana.is_empty() {
            return (Vec::new(), 0);
        }

        // ラティスを構築
        let mut lattice = self.build_lattice(hiragana);

        // Viterbiアルゴリズムで最適パスを探索
        self.find_best_path(&mut lattice);

        let cost = lattice.nodes[lattice.eos_index].total_cost;
        (self.extract_result(&lattice), cost)
    }

    /// ラティスを構築
    /// ラティスを構築（pub for IncrementalViterbi）
    pub fn build_lattice(&self, input: &str) -> Lattice {
        let mut lattice = Lattice::new(input, self.dictionary.bos_id, self.dictionary.eos_id);

        // 各位置のバイトオフセットを事前計算
        let chars: Vec<char> = input.chars().collect();
        let mut byte_positions: Vec<usize> = Vec::with_capacity(chars.len() + 1);
        let mut bp = 0;
        for ch in &chars {
            byte_positions.push(bp);
            bp += ch.len_utf8();
        }
        byte_positions.push(bp);

        for (char_idx, _) in chars.iter().enumerate() {
            let byte_pos = byte_positions[char_idx];
            let remaining = &input[byte_pos..];

            // 辞書からプレフィックス検索
            let matches = self.dictionary.common_prefix_search(remaining);
            let has_dict_hit = !matches.is_empty();

            if !has_dict_hit {
                // マッチがない場合は未知語として1文字を追加
                let ch = chars[char_idx];
                let end_pos = byte_pos + ch.len_utf8();
                lattice.add_unknown(
                    byte_pos,
                    end_pos,
                    ch.to_string(),
                    self.unknown_id,
                    self.unknown_cost,
                );
            } else {
                // マッチした単語をすべて追加
                for (len, entries) in matches {
                    let end_pos = byte_pos + len;
                    for entry in entries.iter() {
                        lattice.add_word(byte_pos, end_pos, entry.clone());
                        let idx = lattice.nodes.len() - 1;
                        // 1文字漢字の単独語 / カタカナ固有名詞 / 記号には実効コストを上乗せ
                        let penalty = single_kanji_penalty(&entry.surface, self.single_kanji_penalty)
                            + katakana_proper_noun_penalty(&entry.surface, &entry.pos)
                            + symbol_penalty(&entry.surface, &entry.pos);
                        // 学習したユニグラムはコストを減額（優先度を上げる）
                        let bonus = self
                            .learned_unigram
                            .get(&(entry.reading.clone(), entry.surface.clone()))
                            .copied()
                            .unwrap_or(0);
                        if penalty != 0 || bonus != 0 {
                            lattice.nodes[idx].word_cost = lattice.nodes[idx]
                                .word_cost
                                .saturating_add(penalty)
                                .saturating_sub(bonus);
                        }
                    }
                }

                // 未知語も追加（より短い単位での分割を許容）
                let ch = chars[char_idx];
                let end_pos = byte_pos + ch.len_utf8();
                lattice.add_unknown(
                    byte_pos,
                    end_pos,
                    ch.to_string(),
                    self.unknown_id,
                    self.unknown_cost + 5000, // 辞書にある場合は未知語のコストを上げる
                );
            }

            // カタカナ候補ノードを生成する。全ひらがなの範囲について
            // カタカナ表記を候補として出し、最終的な採否は Viterbi のコストに
            // 委ねる。開始位置が辞書ヒットの場合はペナルティを付け、通常語を
            // 不当にカタカナ化しないようにする（例: きょうは→キョウハ は抑止、
            // ぶらうざ→ブラウザ は許容。宇/座 等の高コスト漢字に勝つ）。
            if self.enable_katakana_fallback {
                let start_penalty = if has_dict_hit {
                    self.katakana_dict_start_penalty
                } else {
                    0
                };
                self.add_katakana_nodes(
                    &mut lattice,
                    &chars,
                    &byte_positions,
                    char_idx,
                    start_penalty,
                );
            }
        }

        // 学習したひらがな優先（Escで戻した読み）のノードを追加する。
        // 入力中に該当する読みが現れたら、その範囲にひらがなノードを
        // 低コストで足し、既存語（例: 慕い）より優先させる。
        self.add_learned_hiragana_nodes(&mut lattice, input);

        lattice
    }

    /// 学習したひらがな優先の読みに対応するひらがなノードを追加する
    fn add_learned_hiragana_nodes(&self, lattice: &mut Lattice, input: &str) {
        if self.learned_hiragana.is_empty() {
            return;
        }
        for (reading, bonus) in &self.learned_hiragana {
            if reading.is_empty() {
                continue;
            }
            // input 中の全出現位置に対してノードを追加
            let mut from = 0usize;
            while let Some(rel) = input[from..].find(reading.as_str()) {
                let start = from + rel;
                let end = start + reading.len();
                // ひらがな表記は基準コストから学習ボーナス分を引いて優先
                let cost = (5000 - bonus).clamp(-30000, i16::MAX as i32) as i16;
                lattice.add_word(
                    start,
                    end,
                    WordEntry {
                        surface: reading.clone(),
                        reading: reading.clone(),
                        left_id: self.katakana_pos_id,
                        right_id: self.katakana_pos_id,
                        cost,
                        pos: "名詞-一般-*-*".to_string(),
                    },
                );
                from = start + reading.chars().next().unwrap().len_utf8();
            }
        }
    }

    /// カタカナ候補ノードをラティスに追加する
    ///
    /// start_idx から始まる、全ひらがなの範囲（min_len..=max_len）について
    /// カタカナ表記の候補ノードを足す。採否は Viterbi のコストが決める。
    /// `start_penalty` は開始位置が辞書ヒットのとき通常語を守るための上乗せ。
    fn add_katakana_nodes(
        &self,
        lattice: &mut Lattice,
        chars: &[char],
        byte_positions: &[usize],
        start_idx: usize,
        start_penalty: i32,
    ) {
        let max_len = self.katakana_max_len.min(chars.len() - start_idx);
        if max_len < self.katakana_min_len {
            return;
        }

        // カタカナ範囲が内部に「安い辞書語」（助詞・常用語）の開始位置を
        // 含むなら、その語を分断することになるので、その長さ以降は出さない。
        // 例: 「だと」→ 内部(index1)の「と」が安い(＆/と)ので ダト を出さない。
        // 逆に「ぶらうざ」→ 内部の ら/う/ざ は高コストの稀漢字なので ブラウザ
        // を出してよい（きょうは→内部の は が安いので キョウハ は出さない）。
        const INTERIOR_STOP_MAX_COST: i16 = 4500;
        // lattice を可変借用する add_word と競合しないよう入力を控えておく
        let input_owned = lattice.input.clone();
        let cheapest_at = |char_pos: usize| -> Option<i16> {
            let bp = byte_positions[char_pos];
            self.dictionary
                .common_prefix_search(&input_owned[bp..])
                .iter()
                .flat_map(|(_, ws)| ws.iter())
                .map(|e| e.cost)
                .min()
        };
        for len in self.katakana_min_len..=max_len {
            let end_idx = start_idx + len;

            // 範囲が全てひらがな（長音符含む）でなければそれ以上伸ばせない
            if !is_all_hiragana(&chars[start_idx..end_idx]) {
                break;
            }
            // 内部（開始位置を除く）に安い辞書語があれば、この長さ以降は打ち切り
            let has_cheap_interior = (start_idx + 1..end_idx).any(|p| {
                cheapest_at(p).map_or(false, |c| c <= INTERIOR_STOP_MAX_COST)
            });
            if has_cheap_interior {
                break;
            }

            let start_byte = byte_positions[start_idx];
            let end_byte = byte_positions[end_idx];
            let reading: String = chars[start_idx..end_idx].iter().collect();
            let surface = hiragana_to_katakana(&reading);

            let cost =
                self.katakana_base_cost + (len as i32) * self.katakana_step_cost + start_penalty;
            let entry = WordEntry {
                surface,
                reading,
                left_id: self.katakana_pos_id,
                right_id: self.katakana_pos_id,
                cost: cost.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                pos: "カタカナ".to_string(),
            };
            lattice.add_word(start_byte, end_byte, entry);
        }
    }

    /// Viterbiアルゴリズムで最適パスを探索（pub for IncrementalViterbi）
    pub fn find_best_path(&self, lattice: &mut Lattice) {
        let input_len = lattice.input.len();

        // 各位置を左から右に処理
        for pos in 0..=input_len {
            // この位置で始まるノードを処理
            let starting_indices: Vec<usize> = lattice.nodes_starting_at[pos].clone();
            
            for &node_idx in &starting_indices {
                // この位置で終わるノードから連接コストを計算
                let ending_indices: Vec<usize> = lattice.nodes_ending_at[pos].clone();
                
                let mut best_cost = i32::MAX;
                let mut best_prev: Option<usize> = None;

                let use_bigram = !self.learned_bigram.is_empty();
                for &prev_idx in &ending_indices {
                    let prev_node = &lattice.nodes[prev_idx];
                    if prev_node.total_cost == i32::MAX {
                        continue;
                    }

                    let current_node = &lattice.nodes[node_idx];

                    // 連接コスト
                    let mut conn_cost = self.dictionary.matrix.get(
                        prev_node.right_id,
                        current_node.left_id,
                    ) as i32;

                    // 学習したバイグラム（語のつながり）は接続コストを減額
                    if use_bigram {
                        if let (Some(pe), Some(ce)) = (&prev_node.entry, &current_node.entry) {
                            if let Some(bonus) = self
                                .learned_bigram
                                .get(&(pe.surface.clone(), ce.surface.clone()))
                            {
                                conn_cost = conn_cost.saturating_sub(*bonus);
                            }
                        }
                    }

                    // 総コスト = 前のノードまでのコスト + 連接コスト + 単語コスト
                    let total = prev_node.total_cost
                        .saturating_add(conn_cost)
                        .saturating_add(current_node.word_cost);

                    if total < best_cost {
                        best_cost = total;
                        best_prev = Some(prev_idx);
                    }
                }

                if best_cost < i32::MAX {
                    lattice.nodes[node_idx].total_cost = best_cost;
                    lattice.nodes[node_idx].prev_node = best_prev;
                }
            }
        }
    }

    /// 最適パスから結果を抽出（pub for IncrementalViterbi）
    pub fn extract_result(&self, lattice: &Lattice) -> Vec<WordEntry> {
        let mut result = Vec::new();
        let mut current_idx = Some(lattice.eos_index);

        // 後ろから前へたどる
        let mut path = Vec::new();
        while let Some(idx) = current_idx {
            path.push(idx);
            current_idx = lattice.nodes[idx].prev_node;
        }

        // 逆順にして結果を構築（BOS, EOSを除く）
        for &idx in path.iter().rev() {
            if let Some(entry) = &lattice.nodes[idx].entry {
                result.push(entry.clone());
            }
        }

        result
    }

    /// 変換結果を文字列として取得
    pub fn convert_to_string(&self, hiragana: &str) -> String {
        let entries = self.convert(hiragana);
        entries.iter().map(|e| e.surface.as_str()).collect()
    }

    /// N-best候補を取得する
    ///
    /// Viterbiのforward costを完全ヒューリスティックとして用いるバックワードA*探索。
    /// 表層形が重複する候補は除外する。
    pub fn n_best(&self, hiragana: &str, n: usize) -> Vec<Vec<WordEntry>> {
        if hiragana.is_empty() || n == 0 {
            return Vec::new();
        }

        let mut lattice = self.build_lattice(hiragana);
        self.find_best_path(&mut lattice);

        // EOS が到達不能なら結果なし
        if lattice.nodes[lattice.eos_index].total_cost == i32::MAX {
            return Vec::new();
        }

        n_best_from_lattice(&lattice, &self.dictionary, n)
    }

    /// N-best候補を表層形の文字列として取得
    pub fn n_best_strings(&self, hiragana: &str, n: usize) -> Vec<String> {
        self.n_best(hiragana, n)
            .into_iter()
            .map(|entries| entries.iter().map(|e| e.surface.as_str()).collect())
            .collect()
    }
}