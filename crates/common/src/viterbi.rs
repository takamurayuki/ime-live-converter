use crate::candidate::hiragana_to_katakana;
use crate::dictionary::{Dictionary, WordEntry, PosId};
use std::collections::HashMap;

/// ラティス上のノード
#[derive(Debug, Clone)]
pub struct LatticeNode {
    /// 開始位置（バイト単位）
    pub start: usize,
    /// 終了位置（バイト単位）
    pub end: usize,
    /// 単語エントリ（BOSとEOSはNone）
    pub entry: Option<WordEntry>,
    /// 左文脈ID
    pub left_id: PosId,
    /// 右文脈ID
    pub right_id: PosId,
    /// 単語コスト
    pub word_cost: i32,
    /// 累積コスト（このノードまでの最小コスト）
    pub total_cost: i32,
    /// 最適パスの前のノードのインデックス
    pub prev_node: Option<usize>,
}

impl LatticeNode {
    /// BOS（文頭）ノードを作成
    pub fn bos(bos_id: PosId) -> Self {
        Self {
            start: 0,
            end: 0,
            entry: None,
            left_id: bos_id,
            right_id: bos_id,
            word_cost: 0,
            total_cost: 0,
            prev_node: None,
        }
    }

    /// EOS（文末）ノードを作成
    pub fn eos(pos: usize, eos_id: PosId) -> Self {
        Self {
            start: pos,
            end: pos,
            entry: None,
            left_id: eos_id,
            right_id: eos_id,
            word_cost: 0,
            total_cost: i32::MAX,
            prev_node: None,
        }
    }

    /// 単語ノードを作成
    pub fn word(start: usize, end: usize, entry: WordEntry) -> Self {
        Self {
            start,
            end,
            left_id: entry.left_id,
            right_id: entry.right_id,
            word_cost: entry.cost as i32,
            total_cost: i32::MAX,
            prev_node: None,
            entry: Some(entry),
        }
    }

    /// 未知語ノードを作成
    pub fn unknown(start: usize, end: usize, surface: String, default_id: PosId, cost: i32) -> Self {
        Self {
            start,
            end,
            entry: Some(WordEntry {
                surface: surface.clone(),
                reading: surface,
                left_id: default_id,
                right_id: default_id,
                cost: cost as i16,
                pos: "未知語".to_string(),
            }),
            left_id: default_id,
            right_id: default_id,
            word_cost: cost,
            total_cost: i32::MAX,
            prev_node: None,
        }
    }
}

/// ラティス（単語候補のグラフ）
#[derive(Debug)]
pub struct Lattice {
    /// 入力テキスト
    pub input: String,
    /// 各位置で始まるノードのリスト
    pub nodes_starting_at: Vec<Vec<usize>>,
    /// 各位置で終わるノードのリスト
    pub nodes_ending_at: Vec<Vec<usize>>,
    /// 全ノードの配列
    pub nodes: Vec<LatticeNode>,
    /// BOSノードのインデックス
    pub bos_index: usize,
    /// EOSノードのインデックス
    pub eos_index: usize,
}

impl Lattice {
    /// 新しいラティスを作成
    pub fn new(input: &str, bos_id: PosId, eos_id: PosId) -> Self {
        let len = input.len();
        let mut lattice = Self {
            input: input.to_string(),
            nodes_starting_at: vec![Vec::new(); len + 1],
            nodes_ending_at: vec![Vec::new(); len + 1],
            nodes: Vec::new(),
            bos_index: 0,
            eos_index: 0,
        };

        // BOSノードを追加
        lattice.bos_index = lattice.add_node(LatticeNode::bos(bos_id));
        lattice.nodes_ending_at[0].push(lattice.bos_index);

        // EOSノードを追加
        lattice.eos_index = lattice.add_node(LatticeNode::eos(len, eos_id));
        lattice.nodes_starting_at[len].push(lattice.eos_index);

        lattice
    }

    /// ノードを追加
    fn add_node(&mut self, node: LatticeNode) -> usize {
        let index = self.nodes.len();
        self.nodes.push(node);
        index
    }

    /// 単語ノードを追加
    pub fn add_word(&mut self, start: usize, end: usize, entry: WordEntry) {
        let node = LatticeNode::word(start, end, entry);
        let index = self.add_node(node);
        self.nodes_starting_at[start].push(index);
        self.nodes_ending_at[end].push(index);
    }

    /// 未知語ノードを追加
    pub fn add_unknown(&mut self, start: usize, end: usize, surface: String, default_id: PosId, cost: i32) {
        let node = LatticeNode::unknown(start, end, surface, default_id, cost);
        let index = self.add_node(node);
        self.nodes_starting_at[start].push(index);
        self.nodes_ending_at[end].push(index);
    }
}

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

/// 品詞が内容語（助詞・助動詞・記号・未知語でない）か判定する
pub fn is_content_pos(pos: &str) -> bool {
    !(pos.starts_with("助詞")
        || pos.starts_with("助動詞")
        || pos.starts_with("記号")
        || pos.starts_with("未知語")
        || pos.starts_with("フィラー")
        || pos.starts_with("カタカナ"))
}

/// 頻出語の初期プリセット（読み, 表記）
///
/// IPA辞書は解析用のため、ごく一般的な語（会う・水 など）が稀な同音漢字
/// （遭う・瑞 など）より低コストなことがある。起動時にこれらを薄く
/// 「学習済み」として入れておき、初回から自然な変換にする。
/// ユーザーの実学習（頻度が上がる）が優先されるので上書きされる。
const COMMON_WORD_SEED: &[(&str, &str)] = &[
    ("あう", "会う"), ("みず", "水"), ("ひと", "人"), ("て", "手"),
    ("め", "目"), ("き", "木"), ("いえ", "家"), ("やま", "山"),
    ("うみ", "海"), ("そら", "空"), ("みる", "見る"), ("いう", "言う"),
    ("おもう", "思う"), ("きく", "聞く"), ("かく", "書く"), ("よむ", "読む"),
    ("はなす", "話す"), ("たべる", "食べる"), ("のむ", "飲む"), ("かう", "買う"),
    ("まつ", "待つ"), ("もつ", "持つ"), ("しる", "知る"), ("つくる", "作る"),
    ("つかう", "使う"), ("わかる", "分かる"), ("かえる", "帰る"), ("あるく", "歩く"),
    ("はしる", "走る"), ("たつ", "立つ"), ("すわる", "座る"), ("ある", "有る"),
    ("てんき", "天気"), ("しごと", "仕事"), ("じかん", "時間"), ("ばしょ", "場所"),
    ("ことば", "言葉"), ("かんがえ", "考え"), ("きもち", "気持ち"), ("せかい", "世界"),
    // 形容動詞語幹（文末で接続ペナルティを受け、稀な同音語に負けやすい）
    ("かのう", "可能"), ("じゅうよう", "重要"), ("ひつよう", "必要"), ("べんり", "便利"),
    ("たいせつ", "大切"), ("かんたん", "簡単"), ("じゅうぶん", "十分"), ("あんぜん", "安全"),
    ("じゆう", "自由"), ("とくべつ", "特別"), ("ゆうめい", "有名"), ("げんき", "元気"),
    ("だいじょうぶ", "大丈夫"), ("しんぱい", "心配"), ("べんきょう", "勉強"),
    ("せつめい", "説明"), ("じゅんび", "準備"), ("せいこう", "成功"), ("しっぱい", "失敗"),
    // IT・変換まわりで誤変換しやすい超頻出語
    ("かんじ", "漢字"), ("へんかん", "変換"), ("にゅうりょく", "入力"),
    ("しゅつりょく", "出力"), ("もじ", "文字"), ("たんご", "単語"), ("ぶんしょう", "文章"),
    ("けんさく", "検索"), ("せってい", "設定"), ("がめん", "画面"), ("そうさ", "操作"),
    ("もんだい", "問題"), ("ないよう", "内容"), ("かいけつ", "解決"),
    ("うしろ", "後ろ"), ("まえ", "前"), ("となり", "隣"), ("よこ", "横"),
    // 長い動詞は辞書コストが高く、安いカタカナ断片(カン 等)+助詞の分割に
    // 負けやすい。よく使う動詞を優先しておく。
    ("かんがえる", "考える"), ("かんがえた", "考えた"),
];

/// 頻出語プリセットのボーナス（moderate。実学習で上書きされる）
const COMMON_WORD_SEED_BONUS: i32 = 1500;

/// バイグラム学習ボーナスの上限（接続コスト規模に合わせ、断片パスの暴走を防ぐ）
const BIGRAM_BONUS_CAP: i32 = 2500;

/// ユニグラム学習ボーナスの上限（短い語が長い語を分断するのを防ぐ）
const UNIGRAM_BONUS_CAP: i32 = 6000;


/// 使用頻度をコスト減額（ボーナス）に変換する
///
/// 1回=1500、上限20000。同音語のIPAコスト差（数千）を数回の使用で
/// 覆せるスケール。ユーザーが選んだ語を確実に優先させ、
/// 「使うほど賢くなる」を実現する。(reading,surface) 単位のボーナスなので
/// 大きくても他の語には影響しない。
pub fn frequency_to_bonus(freq: u32) -> i32 {
    ((freq as i32) * 1500).min(20000)
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

/// カタカナ表記の辞書エントリに対するコストペナルティ
///
/// IPA辞書はカタカナ表記（ネコ・イヌ・ヤマ・ホン・ヲ 等）を低コストで
/// 持っており、「ねこ」→「ネコ」のように一般漢字語（猫）を押しのけてしまう。
/// 本来必要なカタカナ（外来語）は未知語のフォールバックで別途生成される
/// ので、辞書のカタカナ表記は実効コストを上げて漢字/かなを優先させる。
/// フォールバックで生成したカタカナ(pos=カタカナ)は対象外。
fn katakana_proper_noun_penalty(surface: &str, pos: &str) -> i32 {
    if !pos.contains("固有名詞") {
        return 0;
    }
    let all_katakana = !surface.is_empty()
        && surface.chars().all(|c| {
            ('\u{30A1}'..='\u{30FA}').contains(&c) || ('\u{30FC}'..='\u{30FF}').contains(&c)
        });
    if all_katakana {
        3000
    } else {
        0
    }
}

/// 入力全体が助詞1文字か（単独助詞をひらがなのままにする判定）
///
/// これらは単独で打つと接続コストの都合で同音漢字（刃・賭・藻・戸 等）に
/// 負けやすいが、助詞としてはひらがなが正しい。文中では通常の変換に任せる
/// ため、reading 全体がちょうど1つの助詞のときだけ真を返す。
fn is_lone_particle(reading: &str) -> bool {
    matches!(
        reading,
        "は" | "を" | "が" | "に" | "へ" | "と" | "も" | "の" | "で" | "や"
    )
}

/// 記号（＆＠ 等）表記へのコストペナルティ
///
/// IPA辞書は「と」→「＆」のように、かな読みに ASCII/全角記号を低コストで
/// 割り当てていることがある。かなを記号へ変換するのはほぼ誤りなので、
/// 記号品詞かつ ASCII/全角英数記号の表記に強いペナルティを付ける。
/// 句読点（、。「」等）は CJK 記号域なので対象外。
fn symbol_penalty(surface: &str, pos: &str) -> i32 {
    if !pos.starts_with("記号") {
        return 0;
    }
    let is_ascii_symbol = !surface.is_empty()
        && surface.chars().all(|c| {
            ('\u{0021}'..='\u{007E}').contains(&c) || ('\u{FF01}'..='\u{FF5E}').contains(&c)
        });
    if is_ascii_symbol {
        5000
    } else {
        0
    }
}

/// 1文字漢字の表記に対するコストペナルティを返す
///
/// 単独で使われることが稀な1文字漢字（教・卿・挟 など）が、IPA辞書の
/// 低コストのせいで一般的な複合語（今日・天気 など）より優先されるのを防ぐ。
/// ひらがな1文字（助詞 は・が・を 等）は対象外なので誤って下げない。
fn single_kanji_penalty(surface: &str, penalty: i32) -> i32 {
    let mut chars = surface.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return 0; // 2文字以上は対象外
    };
    let is_kanji =
        ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c);
    if is_kanji {
        penalty
    } else {
        0
    }
}

/// span が全てひらがな（または長音符「ー」）か判定
///
/// 「らーめん」のような長音符入りの外来語表記を一括でカタカナ化
/// できるよう、長音符も許容する。
fn is_all_hiragana(chars: &[char]) -> bool {
    chars.iter().all(|&c| ('\u{3041}'..='\u{3096}').contains(&c) || c == 'ー')
}

/// N-best探索で使うパーシャルパス
#[derive(Clone, Eq, PartialEq)]
struct PartialPath {
    /// f_cost = cost + head_node.total_cost (完成時の総コスト下界)
    f_cost: i64,
    /// バックワード走査でこれまでに積み上げたコスト (head_node → EOS)
    cost: i64,
    /// 現在のヘッドノード（バックワードに最も左にあるノード）
    head_node: usize,
    /// 訪問済みノードのインデックス列（EOS→...→head_node の順）
    path: Vec<usize>,
}

impl Ord for PartialPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeapは最大ヒープなので逆順に比較してmin-heapにする
        other.f_cost.cmp(&self.f_cost)
    }
}

impl PartialOrd for PartialPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// ラティスからN-bestパスを取り出す
fn n_best_from_lattice(lattice: &Lattice, dict: &Dictionary, n: usize) -> Vec<Vec<WordEntry>> {
    use std::collections::BinaryHeap;

    let mut heap: BinaryHeap<PartialPath> = BinaryHeap::new();
    heap.push(PartialPath {
        f_cost: lattice.nodes[lattice.eos_index].total_cost as i64,
        cost: 0,
        head_node: lattice.eos_index,
        path: vec![lattice.eos_index],
    });

    let mut results: Vec<Vec<WordEntry>> = Vec::new();
    let mut seen_surfaces: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 暴走を防ぐためにループ上限を設ける（候補数 × 1000）
    let max_iterations = n.saturating_mul(1000).max(10_000);
    let mut iterations = 0;

    while let Some(current) = heap.pop() {
        iterations += 1;
        if iterations > max_iterations {
            break;
        }
        if results.len() >= n {
            break;
        }

        // BOSに到達したら完成
        if current.head_node == lattice.bos_index {
            let entries: Vec<WordEntry> = current
                .path
                .iter()
                .rev()
                .filter_map(|&idx| lattice.nodes[idx].entry.clone())
                .collect();
            let surface: String = entries.iter().map(|e| e.surface.as_str()).collect();
            if seen_surfaces.insert(surface) {
                results.push(entries);
            }
            continue;
        }

        // ヘッドノードの前駆を展開
        let head = &lattice.nodes[current.head_node];
        let pos = head.start;
        let head_word_cost = head.word_cost as i64;
        let head_left_id = head.left_id;

        for &prev_idx in &lattice.nodes_ending_at[pos] {
            let prev = &lattice.nodes[prev_idx];
            if prev.total_cost == i32::MAX {
                continue;
            }

            let conn_cost = dict.matrix.get(prev.right_id, head_left_id) as i64;
            // step_cost(prev → head) = conn_cost + head.word_cost
            let step_cost = conn_cost + head_word_cost;
            let new_cost = current.cost + step_cost;
            let new_f = new_cost + prev.total_cost as i64;

            let mut new_path = current.path.clone();
            new_path.push(prev_idx);

            heap.push(PartialPath {
                f_cost: new_f,
                cost: new_cost,
                head_node: prev_idx,
                path: new_path,
            });
        }
    }

    results
}

/// ライブ変換用のコンテキスト
#[derive(Debug)]
pub struct LiveConversionContext {
    /// 変換エンジン
    converter: ViterbiConverter,
    /// 入力バッファ（ひらがな）
    input_buffer: String,
    /// 確定済みテキスト
    committed_text: String,
    /// 現在の変換候補
    current_conversion: String,
}

impl LiveConversionContext {
    pub fn new(converter: ViterbiConverter) -> Self {
        Self {
            converter,
            input_buffer: String::new(),
            committed_text: String::new(),
            current_conversion: String::new(),
        }
    }

    /// ひらがなを追加して再変換
    pub fn add_hiragana(&mut self, hiragana: &str) -> &str {
        self.input_buffer.push_str(hiragana);
        self.update_conversion();
        &self.current_conversion
    }

    /// バックスペース処理
    pub fn backspace(&mut self) -> &str {
        self.input_buffer.pop();
        self.update_conversion();
        &self.current_conversion
    }

    /// 変換を更新
    fn update_conversion(&mut self) {
        if self.input_buffer.is_empty() {
            self.current_conversion.clear();
        } else {
            self.current_conversion = self.converter.convert_to_string(&self.input_buffer);
        }
    }

    /// 現在の変換を確定
    pub fn commit(&mut self) -> String {
        let result = self.current_conversion.clone();
        self.committed_text.push_str(&result);
        self.input_buffer.clear();
        self.current_conversion.clear();
        result
    }

    /// 入力バッファをクリア
    pub fn clear(&mut self) {
        self.input_buffer.clear();
        self.current_conversion.clear();
    }

    /// 現在の入力バッファを取得
    pub fn get_input_buffer(&self) -> &str {
        &self.input_buffer
    }

    /// 現在の変換結果を取得
    pub fn get_conversion(&self) -> &str {
        &self.current_conversion
    }
}

/// 差分計算対応のライブ変換エンジン
/// 
/// 入力が末尾に追加された場合、前回のラティスを再利用して高速に変換
#[derive(Debug)]
pub struct IncrementalViterbi {
    /// 変換エンジン
    converter: ViterbiConverter,
    /// キャッシュされたラティス
    cached_lattice: Option<Lattice>,
    /// 前回の入力文字列
    cached_input: String,
    /// 前回の変換結果
    cached_result: Vec<WordEntry>,
    /// 前回の入力のバイト位置（文字境界）
    char_byte_positions: Vec<usize>,
}

impl IncrementalViterbi {
    pub fn new(converter: ViterbiConverter) -> Self {
        Self {
            converter,
            cached_lattice: None,
            cached_input: String::new(),
            cached_result: Vec::new(),
            char_byte_positions: Vec::new(),
        }
    }

    /// 入力を変換（差分計算対応）
    pub fn convert(&mut self, input: &str) -> &[WordEntry] {
        if input.is_empty() {
            self.clear_cache();
            return &self.cached_result;
        }

        // 入力が前回と同じ場合はキャッシュを返す
        if input == self.cached_input {
            return &self.cached_result;
        }

        // TODO: 差分計算は複雑なので、一旦無効化
        // 新しい文字が追加された場合、古い位置から始まる長い単語も
        // 検出する必要があるため、現在の実装では正しく動作しない
        // 
        // 例: "き" + "ょ" + "う" → "きょう" → "今日"
        // 「う」追加時に「き」の位置から始まる「きょう」を検出できない
        
        // 常に完全再構築
        self.rebuild_lattice(input);
        &self.cached_result
    }

    /// 変換結果を文字列として取得
    pub fn convert_to_string(&mut self, input: &str) -> String {
        self.convert(input)
            .iter()
            .map(|e| e.surface.as_str())
            .collect()
    }

    /// ラティスを拡張（差分計算）
    ///
    /// 注意: 現在は convert() から呼ばれていない（末尾追加時に既存位置から
    /// 始まる長い単語を検出できない問題が未解決のため）。将来の差分計算
    /// 実装の土台として残している。
    #[allow(dead_code)]
    fn extend_lattice(&mut self, full_input: &str, added: &str) {
        let lattice = self.cached_lattice.as_mut().unwrap();
        let old_len = self.cached_input.len();
        let new_len = full_input.len();

        // 入力文字列を更新
        lattice.input = full_input.to_string();

        // EOSノードを更新
        lattice.nodes[lattice.eos_index].start = new_len;
        lattice.nodes[lattice.eos_index].end = new_len;

        // ノード配列を拡張
        let old_nodes_len = lattice.nodes_starting_at.len();
        lattice.nodes_starting_at.resize(new_len + 1, Vec::new());
        lattice.nodes_ending_at.resize(new_len + 1, Vec::new());

        // 古いEOSの位置からノードを削除
        if old_len < old_nodes_len {
            lattice.nodes_starting_at[old_len].retain(|&idx| idx != lattice.eos_index);
        }
        // 新しいEOSの位置にノードを追加
        lattice.nodes_starting_at[new_len].push(lattice.eos_index);

        // 追加された部分に対してノードを追加
        let added_chars: Vec<char> = added.chars().collect();
        let mut byte_pos = old_len;

        for ch in added_chars.iter() {
            let remaining = &full_input[byte_pos..];

            // 辞書からプレフィックス検索
            let matches = self.converter.dictionary.common_prefix_search(remaining);

            if matches.is_empty() {
                // 未知語として1文字を追加
                let end_pos = byte_pos + ch.len_utf8();
                let node = LatticeNode::unknown(
                    byte_pos,
                    end_pos,
                    ch.to_string(),
                    self.converter.unknown_id,
                    self.converter.unknown_cost,
                );
                let index = lattice.nodes.len();
                lattice.nodes.push(node);
                lattice.nodes_starting_at[byte_pos].push(index);
                lattice.nodes_ending_at[end_pos].push(index);
            } else {
                // マッチした単語をすべて追加
                for (len, entries) in &matches {
                    let end_pos = byte_pos + len;
                    for entry in entries.iter() {
                        let node = LatticeNode::word(byte_pos, end_pos, entry.clone());
                        let index = lattice.nodes.len();
                        lattice.nodes.push(node);
                        lattice.nodes_starting_at[byte_pos].push(index);
                        lattice.nodes_ending_at[end_pos].push(index);
                    }
                }

                // 未知語も追加
                let end_pos = byte_pos + ch.len_utf8();
                let node = LatticeNode::unknown(
                    byte_pos,
                    end_pos,
                    ch.to_string(),
                    self.converter.unknown_id,
                    self.converter.unknown_cost + 5000,
                );
                let index = lattice.nodes.len();
                lattice.nodes.push(node);
                lattice.nodes_starting_at[byte_pos].push(index);
                lattice.nodes_ending_at[end_pos].push(index);
            }

            // バイト位置を記録
            self.char_byte_positions.push(byte_pos);
            byte_pos += ch.len_utf8();
        }

        // 新しい部分からViterbiを再計算
        self.recompute_viterbi_from(old_len);

        // 結果を抽出
        self.cached_result = self.extract_result();
        self.cached_input = full_input.to_string();
    }

    /// 指定位置からViterbiを再計算
    #[allow(dead_code)]
    fn recompute_viterbi_from(&mut self, start_pos: usize) {
        let lattice = self.cached_lattice.as_mut().unwrap();
        let input_len = lattice.input.len();

        // 新しいノードのコストをリセット
        for node in &mut lattice.nodes {
            if node.start >= start_pos {
                node.total_cost = i32::MAX;
                node.prev_node = None;
            }
        }
        // EOSもリセット
        lattice.nodes[lattice.eos_index].total_cost = i32::MAX;
        lattice.nodes[lattice.eos_index].prev_node = None;

        // start_posから再計算
        for pos in start_pos..=input_len {
            let starting_indices: Vec<usize> = lattice.nodes_starting_at[pos].clone();

            for &node_idx in &starting_indices {
                let ending_indices: Vec<usize> = lattice.nodes_ending_at[pos].clone();

                let mut best_cost = i32::MAX;
                let mut best_prev: Option<usize> = None;

                for &prev_idx in &ending_indices {
                    let prev_node = &lattice.nodes[prev_idx];
                    if prev_node.total_cost == i32::MAX {
                        continue;
                    }

                    let current_node = &lattice.nodes[node_idx];

                    let conn_cost = self.converter.dictionary.matrix.get(
                        prev_node.right_id,
                        current_node.left_id,
                    ) as i32;

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

    /// ラティスを完全再構築
    fn rebuild_lattice(&mut self, input: &str) {
        let mut lattice = self.converter.build_lattice(input);
        self.converter.find_best_path(&mut lattice);
        
        self.cached_result = self.converter.extract_result(&lattice);
        self.cached_lattice = Some(lattice);
        self.cached_input = input.to_string();

        // バイト位置を記録
        self.char_byte_positions.clear();
        let mut byte_pos = 0;
        for ch in input.chars() {
            self.char_byte_positions.push(byte_pos);
            byte_pos += ch.len_utf8();
        }
    }

    /// 結果を抽出
    #[allow(dead_code)]
    fn extract_result(&self) -> Vec<WordEntry> {
        let lattice = self.cached_lattice.as_ref().unwrap();
        self.converter.extract_result(lattice)
    }

    /// キャッシュをクリア
    pub fn clear_cache(&mut self) {
        self.cached_lattice = None;
        self.cached_input.clear();
        self.cached_result.clear();
        self.char_byte_positions.clear();
    }

    /// バックスペース処理（1文字削除）
    pub fn backspace(&mut self) -> &[WordEntry] {
        if self.cached_input.is_empty() {
            return &self.cached_result;
        }

        // 最後の文字を削除した新しい入力を作成
        let mut chars: Vec<char> = self.cached_input.chars().collect();
        chars.pop();
        let new_input: String = chars.into_iter().collect();

        // 簡略化のため、バックスペース時は再構築
        // （将来的には部分的な再計算も可能）
        if new_input.is_empty() {
            self.clear_cache();
        } else {
            self.rebuild_lattice(&new_input);
        }

        &self.cached_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_dictionary() -> Dictionary {
        let mut dict = Dictionary::new();
        
        // テスト用の単語を追加
        dict.add_word(WordEntry {
            surface: "今日".to_string(),
            reading: "きょう".to_string(),
            left_id: 1, right_id: 1, cost: 5000,
            pos: "名詞".to_string(),
        });
        
        dict.add_word(WordEntry {
            surface: "は".to_string(),
            reading: "は".to_string(),
            left_id: 2, right_id: 2, cost: 3000,
            pos: "助詞".to_string(),
        });
        
        dict.add_word(WordEntry {
            surface: "良い".to_string(),
            reading: "いい".to_string(),
            left_id: 3, right_id: 3, cost: 5500,
            pos: "形容詞".to_string(),
        });
        
        dict.add_word(WordEntry {
            surface: "天気".to_string(),
            reading: "てんき".to_string(),
            left_id: 1, right_id: 1, cost: 5200,
            pos: "名詞".to_string(),
        });
        
        dict.add_word(WordEntry {
            surface: "です".to_string(),
            reading: "です".to_string(),
            left_id: 4, right_id: 4, cost: 4000,
            pos: "助動詞".to_string(),
        });

        dict
    }

    #[test]
    fn test_viterbi_conversion() {
        let dict = create_test_dictionary();
        let converter = ViterbiConverter::new(dict);
        
        // "きょうは" を変換
        let result = converter.convert_to_string("きょうは");
        assert!(result.contains("今日") || result.contains("は"));
    }

    #[test]
    fn test_katakana_fallback_for_unknown() {
        // 辞書には「は」(助詞) のみ。「らすと」は未知語なのでカタカナ化されるはず。
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        dict.add_word(WordEntry {
            surface: "は".to_string(),
            reading: "は".to_string(),
            left_id: 4, right_id: 4, cost: 3000,
            pos: "助詞".to_string(),
        });

        let converter = ViterbiConverter::new(dict);
        let result = converter.convert_to_string("らすと");
        assert_eq!(result, "ラスト", "カタカナフォールバックが効いていない");
    }

    #[test]
    fn test_learned_unigram_changes_live_conversion() {
        // 学習したユニグラムがライブ変換の1-bestを変える
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        // 「きしゃ」に 記者(低コスト) と 汽車(高コスト)
        dict.add_word(WordEntry {
            surface: "記者".to_string(), reading: "きしゃ".to_string(),
            left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
        });
        dict.add_word(WordEntry {
            surface: "汽車".to_string(), reading: "きしゃ".to_string(),
            left_id: 1, right_id: 1, cost: 5500, pos: "名詞".to_string(),
        });
        let mut converter = ViterbiConverter::new(dict);
        // 学習前は 記者
        assert_eq!(converter.convert_to_string("きしゃ"), "記者");
        // 「きしゃ→汽車」を5回使ったことにする
        converter.learn_unigram("きしゃ", "汽車", 5);
        // 学習後は 汽車 が勝つ
        assert_eq!(converter.convert_to_string("きしゃ"), "汽車");
    }

    #[test]
    fn test_context_assoc_disambiguates() {
        // 内容語連想により、助詞を挟んだ文脈で同音語を正しく選ぶ
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        // きしゃ = 記者/汽車（同コスト）、の = 助詞、しんぶん=新聞、えき=駅
        dict.add_word(WordEntry { surface: "記者".into(), reading: "きしゃ".into(), left_id: 1, right_id: 1, cost: 5000, pos: "名詞-一般".into() });
        dict.add_word(WordEntry { surface: "汽車".into(), reading: "きしゃ".into(), left_id: 1, right_id: 1, cost: 5000, pos: "名詞-一般".into() });
        dict.add_word(WordEntry { surface: "新聞".into(), reading: "しんぶん".into(), left_id: 1, right_id: 1, cost: 5000, pos: "名詞-一般".into() });
        dict.add_word(WordEntry { surface: "駅".into(), reading: "えき".into(), left_id: 1, right_id: 1, cost: 5000, pos: "名詞-一般".into() });
        dict.add_word(WordEntry { surface: "の".into(), reading: "の".into(), left_id: 2, right_id: 2, cost: 3000, pos: "助詞-連体化".into() });
        let mut converter = ViterbiConverter::new(dict);
        converter.enable_katakana_fallback = false; // 連想の検証にカタカナ候補は不要

        // 学習: 新聞…記者 / 駅…汽車（助詞「の」を挟んだ内容語連想）
        converter.learn_assoc("新聞", "記者", 5);
        converter.learn_assoc("駅", "汽車", 5);

        // 文脈で選び分けられる
        assert!(converter.convert_context_aware_to_string("しんぶんのきしゃ").contains("記者"));
        assert!(converter.convert_context_aware_to_string("えきのきしゃ").contains("汽車"));
    }

    #[test]
    fn test_learned_bigram_improves_consistency() {
        // バイグラム学習が語のつながりを優先する
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        dict.add_word(WordEntry {
            surface: "貴社".to_string(), reading: "きしゃ".to_string(),
            left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
        });
        dict.add_word(WordEntry {
            surface: "記者".to_string(), reading: "きしゃ".to_string(),
            left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
        });
        dict.add_word(WordEntry {
            surface: "会見".to_string(), reading: "かいけん".to_string(),
            left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
        });
        let mut converter = ViterbiConverter::new(dict);
        // 「記者」の後に「会見」が来るつながりを学習
        converter.learn_bigram("記者", "会見", 5);
        // きしゃかいけん → 記者会見（貴社ではなく記者が選ばれる）
        let result = converter.convert_to_string("きしゃかいけん");
        assert!(result.contains("記者"), "bigram学習が効いていない: {}", result);
    }

    #[test]
    fn test_wo_stays_particle_not_katakana() {
        // IPA辞書由来の「ヲ」(カタカナ,低コスト)より助詞「を」を優先する
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 500);
            }
        }
        dict.add_word(WordEntry {
            surface: "ヲ".into(), reading: "を".into(),
            left_id: 1, right_id: 1, cost: 3733, pos: "名詞-固有名詞-一般-*".into(),
        });
        dict.add_word(WordEntry {
            surface: "を".into(), reading: "を".into(),
            left_id: 4, right_id: 4, cost: 4183, pos: "助詞-格助詞-一般-*".into(),
        });
        let converter = ViterbiConverter::new(dict);
        assert_eq!(converter.convert_to_string("を"), "を");
    }

    #[test]
    fn test_learned_hiragana_preference() {
        // Escで戻した読みを学習すると、その読みがひらがなで出るようになる
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        // 「こい」に漢字「恋」だけ登録（プリセット外の読みを使う）
        dict.add_word(WordEntry {
            surface: "恋".into(), reading: "こい".into(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-一般".into(),
        });
        let mut converter = ViterbiConverter::new(dict);
        converter.enable_katakana_fallback = false; // カタカナ候補を除外して判定を明確に
        // 学習前は漢字
        assert_eq!(converter.convert_to_string("こい"), "恋");
        // ひらがな優先を学習
        converter.learn_hiragana("こい", 1);
        // 学習後はひらがな
        assert_eq!(converter.convert_to_string("こい"), "こい");
    }

    #[test]
    fn test_common_word_seed_applied() {
        // 頻出語プリセットが空でなく、代表語が入っている
        let converter = ViterbiConverter::new(create_test_dictionary());
        assert!(converter.learned_unigram.contains_key(&("あう".to_string(), "会う".to_string())));
        // を の強優先も入っている
        assert_eq!(
            converter.learned_unigram.get(&("を".to_string(), "を".to_string())),
            Some(&8000)
        );
    }

    #[test]
    fn test_single_kanji_penalty_prefers_common_word() {
        // 1文字漢字ペナルティにより、同音の複合語が優先される
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        // 「きょう」に対し 今日(2文字) と 教(1文字) を登録。教の方が低コスト。
        dict.add_word(WordEntry {
            surface: "今日".to_string(), reading: "きょう".to_string(),
            left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
        });
        dict.add_word(WordEntry {
            surface: "教".to_string(), reading: "きょう".to_string(),
            left_id: 1, right_id: 1, cost: 4800, pos: "名詞".to_string(),
        });
        let converter = ViterbiConverter::new(dict);
        // ペナルティ(300)により 教(4800+300=5100) より 今日(5000) が勝つ
        assert_eq!(converter.convert_to_string("きょう"), "今日");
    }

    #[test]
    fn test_katakana_fallback_with_long_vowel_mark() {
        // 長音符「ー」を含む外来語表記も一括カタカナ化できる
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        let converter = ViterbiConverter::new(dict);
        let result = converter.convert_to_string("らーめん");
        assert_eq!(result, "ラーメン");
    }

    #[test]
    fn test_katakana_fallback_with_particle() {
        // 「きょうはらすと」: きょう=今日、は=助詞、らすと=ラスト を期待
        let dict = create_test_dictionary();
        let converter = ViterbiConverter::new(dict);
        let result = converter.convert_to_string("きょうはらすと");
        // 「らすと」部分が「ラスト」になっていること
        assert!(result.contains("ラスト"), "カタカナ化未実施: {}", result);
        // 辞書ヒットは保たれる
        assert!(result.contains("今日") || result.contains("きょう"));
    }

    #[test]
    fn test_katakana_fallback_disabled() {
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        let mut converter = ViterbiConverter::new(dict);
        converter.enable_katakana_fallback = false;
        let result = converter.convert_to_string("らすと");
        // フォールバック無効ならひらがなのまま（未知語列）
        assert_eq!(result, "らすと");
    }

    #[test]
    fn test_n_best() {
        let dict = create_test_dictionary();
        let converter = ViterbiConverter::new(dict);

        let results = converter.n_best_strings("きょうはいいてんきです", 3);
        assert!(!results.is_empty(), "N-best候補が0件");
        // 第一候補に「今日」が含まれる
        assert!(results[0].contains("今日"));
        // 重複なし
        let unique: std::collections::HashSet<_> = results.iter().collect();
        assert_eq!(unique.len(), results.len());
    }

    #[test]
    fn test_n_best_empty_input() {
        let dict = create_test_dictionary();
        let converter = ViterbiConverter::new(dict);
        assert!(converter.n_best("", 5).is_empty());
        assert!(converter.n_best("きょう", 0).is_empty());
    }

    #[test]
    fn test_live_conversion_context() {
        let dict = create_test_dictionary();
        let converter = ViterbiConverter::new(dict);
        let mut context = LiveConversionContext::new(converter);
        
        context.add_hiragana("きょう");
        assert!(!context.get_conversion().is_empty());
        
        context.add_hiragana("は");
        let conversion = context.get_conversion();
        assert!(conversion.contains("今日") || conversion.contains("は"));
    }
}
