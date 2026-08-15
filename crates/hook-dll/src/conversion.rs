//! ライブ変換の状態と変換ロジック（ローマ字→かな→漢字、候補生成、学習ガード）

use crate::*;

/// 変換候補（Tab/Space で巡回する同音異義語）の最大数。
/// 番号キーではなく Tab/Space で選ぶため 9 に縛る必要はない。ポップアップは
/// 画面に収まる分だけスクロール表示するので、多めに集めても破綻しない。
pub(crate) const MAX_CONVERSION_CANDIDATES: usize = 50;

/// ユーザー登録単語の品詞（名詞-一般）と接続ID・既定コスト。
/// extend_from_csv（補助辞書）と同じ扱いにして、単独でも文中でも変換に出るようにする。
pub(crate) const USER_WORD_POS: &str = "名詞-一般-*-*";
pub(crate) const USER_WORD_POS_ID: common::PosId = 1285;
/// ユーザー登録語のコスト。低めにして、学習済みの別解（例「思い＋で」）にも
/// 勝てるようにする。さらに登録時に学習ボーナスも付与する（下記）。
pub(crate) const USER_WORD_COST: i16 = 2000;
/// ユーザー登録語に与える擬似学習頻度。frequency_to_bonus で上限(6000)の
/// ボーナスが付き、「ユーザーが明示登録した語」を最優先で選ばせる。
pub(crate) const USER_WORD_LEARN_FREQ: u32 = 20;

/// 隣接する漢字複合語を自動登録するために必要な最小バイグラム頻度
/// （同じ隣接ペアがこの回数だけ確定されたら辞書に単語として登録する）。
/// 1回で登録すると誤変換をそのまま固定してしまう恐れがあるため、
/// 学習系ボーナスと同様に反復を要求する。
pub(crate) const AUTO_COMPOUND_MIN_FREQ: u32 = 3;
/// 自動登録する複合語の読みの最大文字数（暴走的に長い結合を防ぐ）。
pub(crate) const AUTO_COMPOUND_MAX_READING_LEN: usize = 8;

/// ライブ変換の状態
pub(crate) struct LiveConversionState {
    /// ローマ字→ひらがな変換
    pub(crate) romaji: RomajiConverter,
    /// ひらがな→漢字変換
    pub(crate) converter: Option<ViterbiConverter>,
    /// 現在のローマ字入力バッファ
    pub(crate) romaji_buffer: String,
    /// 現在のひらがなバッファ
    pub(crate) hiragana_buffer: String,
    /// 現在の変換結果
    pub(crate) conversion_result: String,
    /// 前回送信した文字数
    pub(crate) last_sent_length: usize,
    /// 現在の変換候補（Tab/Spaceで切替。バッファ変更でクリア）
    pub(crate) candidates: Vec<String>,
    /// 選択中の候補インデックス
    pub(crate) candidate_index: usize,
    /// 候補一覧が対象とする文節（＝直近に打った最後の文節）の読み
    pub(crate) cand_seg_reading: String,
    /// 候補一覧の各項目に対応する対象文節の表記（candidates と並行）
    pub(crate) cand_seg_surfaces: Vec<String>,
    /// 対象文節より前（確定扱いにしない、変えない部分）の変換済み表記
    pub(crate) cand_prefix_surface: String,
    /// 対象文節より前の読み（学習時に前半を再分解するために保持）
    pub(crate) cand_prefix_reading: String,
    /// 対象文節より後ろ（末尾の平仮名など）の変換済み表記
    pub(crate) cand_suffix_surface: String,
    /// 対象文節より後ろの読み
    pub(crate) cand_suffix_reading: String,
    /// この合成で → により部分確定済みの文節列（読み, 表記, 品詞）
    /// 最終確定時にユニグラム/バイグラム/内容語連想の学習へ使う。
    pub(crate) committed_segments: Vec<(String, String, String)>,
    /// 直近に確定したテキスト（LLM変換へ渡す前後文脈。末尾数十文字を保持）
    pub(crate) recent_context: String,
    /// 予測変換の候補（読み, 表記）。読みが空なら「次単語予測（追記）」。
    /// 打鍵中は前方一致補完、確定直後は次単語予測を入れる。番号キーで選ぶ。
    pub(crate) predictions: Vec<(String, String)>,
    /// predictions[0] が「もしかして（誤字補正）」かどうか。表示ラベル用。
    pub(crate) prediction_top_is_fuzzy: bool,
    /// prediction_top_is_fuzzy が true のとき、その場で計算した誤字補正では
    /// なく、過去にユーザーが実際に確定した学習済みの誤字修正かどうか。
    /// Deleteキーでの「誤学習リセット」対象の判定に使う（その場計算のもの
    /// には消す学習が無い）。
    pub(crate) prediction_top_is_learned_typo: bool,
    /// 予測リスト内で選択中の位置（↑↓で移動。Enterでこれを確定）。
    pub(crate) prediction_index: usize,
    /// 直近に確定した表記（次単語予測・バイグラム記録に使う）
    pub(crate) last_committed: String,
    /// Escで「かなに戻した」末尾の読み文字数。update_conversion は末尾の
    /// この文字数分を変換せずひらがなのまま表示する。Escを押すたびに
    /// 一つ前の文節分だけ増え、前の変換も順にひらがなへ戻す。
    pub(crate) kana_tail_len: usize,
    /// 入力世代。入力・確定・取消のたびに増える。非同期のLLM結果が
    /// 発火時と同じ世代のときだけ適用し、古い結果が別の位置に誤って
    /// 差し込まれる（前の入力が壊れる）のを防ぐ。
    pub(crate) generation: u64,
    /// 学習リポジトリ（確定履歴の記録・候補の頻度順ソート）
    pub(crate) learning: Option<LearningRepository>,
    /// 変換が有効かどうか
    pub(crate) enabled: bool,
}

impl LiveConversionState {
    pub(crate) fn new() -> Self {
        Self {
            romaji: RomajiConverter::new(),
            converter: None,
            romaji_buffer: String::new(),
            hiragana_buffer: String::new(),
            conversion_result: String::new(),
            last_sent_length: 0,
            candidates: Vec::new(),
            candidate_index: 0,
            cand_seg_reading: String::new(),
            cand_seg_surfaces: Vec::new(),
            cand_prefix_surface: String::new(),
            cand_prefix_reading: String::new(),
            cand_suffix_surface: String::new(),
            cand_suffix_reading: String::new(),
            committed_segments: Vec::new(),
            recent_context: String::new(),
            predictions: Vec::new(),
            prediction_top_is_fuzzy: false,
            prediction_top_is_learned_typo: false,
            prediction_index: 0,
            last_committed: String::new(),
            kana_tail_len: 0,
            generation: 0,
            learning: None,
            enabled: true,
        }
    }

    /// 学習リポジトリの内容を変換エンジンのメモリへ一括ロードする
    ///
    /// 起動時と学習DB切替時に呼ぶ。これによりライブ変換が過去の
    /// 学習を反映する（使うほど賢くなる仕組みの土台）。
    pub(crate) fn reload_learning_into_converter(&mut self) {
        let (Some(conv), Some(learning)) = (self.converter.as_mut(), self.learning.as_ref())
        else {
            return;
        };
        conv.clear_learning();
        if let Ok(unigrams) = learning.all_unigrams() {
            for (reading, surface, freq) in unigrams {
                conv.learn_unigram(&reading, &surface, freq);
            }
        }
        if let Ok(bigrams) = learning.all_bigrams() {
            for (prev, surface, freq) in bigrams {
                conv.learn_bigram(&prev, &surface, freq);
            }
        }
        if let Ok(assocs) = learning.all_assocs() {
            for (prev, content, freq) in assocs {
                conv.learn_assoc(&prev, &content, freq);
            }
        }
        if let Ok(prefs) = learning.all_hiragana_prefs() {
            for (reading, freq) in prefs {
                conv.learn_hiragana(&reading, freq);
            }
        }
        if let Ok(typos) = learning.all_typo_corrections() {
            for (wrong, reading, surface, freq) in typos {
                conv.learn_typo_correction(&wrong, &reading, &surface, freq);
            }
        }
        debug_log!(
            "学習ロード: unigram={}, bigram={}, assoc={}, typo={}",
            conv.learned_unigram.len(),
            conv.learned_bigram.len(),
            conv.learned_assoc.len(),
            conv.typo_corrections.len()
        );
    }

    /// ユーザー登録の単語（user_dictionary）をライブ変換器の辞書へ注入する。
    /// 辞書ロード直後に呼ぶ。辞書に無い複合語を変換候補に出せるようにする。
    pub(crate) fn inject_user_words(&mut self) {
        // 先に読み出してから（learning の借用を落として）converter を可変借用する。
        let words = match self.learning.as_ref().and_then(|l| l.get_all_user_words().ok()) {
            Some(w) => w,
            None => return,
        };
        let n = words.len();
        if let Some(conv) = self.converter.as_mut() {
            for e in words {
                let reading = e.reading.clone();
                let surface = e.surface.clone();
                conv.dictionary.add_word(common::WordEntry {
                    surface: e.surface,
                    reading: e.reading,
                    left_id: USER_WORD_POS_ID,
                    right_id: USER_WORD_POS_ID,
                    cost: e.cost as i16,
                    pos: e.pos.unwrap_or_else(|| USER_WORD_POS.to_string()),
                });
                // 学習済みの別解（例「思い＋で」）に負けないよう、登録語にも
                // 学習ボーナスを与えて最優先で選ばせる。
                conv.learn_unigram(&reading, &surface, USER_WORD_LEARN_FREQ);
            }
        }
        debug_log!("ユーザー辞書を注入: {} 語", n);
    }

    /// 単語を登録する（DB へ保存＋ライブ変換器の辞書へ即注入）。
    /// 読みはひらがな、表記は任意。空や重複はそのまま上書き（INSERT OR REPLACE）。
    /// 成功したら true。
    pub(crate) fn register_user_word(&mut self, reading: &str, surface: &str) -> bool {
        let reading = reading.trim().to_string();
        let surface = surface.trim().to_string();
        if reading.is_empty() || surface.is_empty() {
            return false;
        }
        // 読みが漢字/カタカナ等（＝読みと表記を入力欄で取り違えた等）だと、
        // その読みで打っても一致しない死んだエントリになる。読みは常に
        // ひらがなである契約なので、そうでなければ登録を拒否する。
        if !is_hiragana_reading(&reading) {
            return false;
        }
        if let Some(learning) = self.learning.as_ref() {
            if learning
                .add_user_word(&reading, &surface, Some(USER_WORD_POS), USER_WORD_COST as i32)
                .is_err()
            {
                return false;
            }
        } else {
            return false;
        }
        if let Some(conv) = self.converter.as_mut() {
            conv.dictionary.add_word(common::WordEntry {
                surface: surface.clone(),
                reading: reading.clone(),
                left_id: USER_WORD_POS_ID,
                right_id: USER_WORD_POS_ID,
                cost: USER_WORD_COST,
                pos: USER_WORD_POS.to_string(),
            });
            // 学習済みの別解にも勝てるよう、登録語に学習ボーナスを付与する。
            conv.learn_unigram(&reading, &surface, USER_WORD_LEARN_FREQ);
        }
        true
    }

    /// 隣接する漢字複合語の反復使用から自動登録する（`register_user_word`と
    /// 同じ辞書注入だが、DBには`source='auto'`で記録し手動登録と区別する）。
    pub(crate) fn register_auto_compound(&mut self, reading: &str, surface: &str) -> bool {
        if reading.is_empty() || surface.is_empty() || !is_hiragana_reading(reading) {
            return false;
        }
        if let Some(learning) = self.learning.as_ref() {
            if learning
                .add_auto_compound_word(reading, surface, USER_WORD_COST as i32)
                .is_err()
            {
                return false;
            }
        } else {
            return false;
        }
        if let Some(conv) = self.converter.as_mut() {
            conv.dictionary.add_word(common::WordEntry {
                surface: surface.to_string(),
                reading: reading.to_string(),
                left_id: USER_WORD_POS_ID,
                right_id: USER_WORD_POS_ID,
                cost: USER_WORD_COST,
                pos: USER_WORD_POS.to_string(),
            });
            conv.learn_unigram(reading, surface, USER_WORD_LEARN_FREQ);
        }
        true
    }

    /// 登録済みの単語を削除する（DB とライブ辞書の両方から）。消したら true。
    pub(crate) fn delete_user_word(&mut self, reading: &str, surface: &str) -> bool {
        let ok = self
            .learning
            .as_ref()
            .map(|l| l.remove_user_word(reading, surface).unwrap_or(false))
            .unwrap_or(false);
        if let Some(conv) = self.converter.as_mut() {
            conv.dictionary.remove_word(reading, surface);
        }
        ok
    }

    /// 登録済みの単語一覧（読み, 表記）を返す（設定画面用）。
    pub(crate) fn all_user_words(&self) -> Vec<(String, String)> {
        self.learning
            .as_ref()
            .and_then(|l| l.get_all_user_words().ok())
            .map(|v| v.into_iter().map(|e| (e.reading, e.surface)).collect())
            .unwrap_or_default()
    }

    /// ローマ字が変換されずに取り残された打ち間違いを補正して「もしかして」を返す。
    ///
    /// ローマ字変換は変換できない英字をそのまま結果に混ぜて先へ進むため、取り残し
    /// は末尾だけでなく **途中・先頭に埋め込まれる**（例: saynara→「さyなら」、
    /// gm…→「gm…」）。そこで「かな＋末尾ローマ字」を1本の文字列として見て、
    /// 埋め込まれた英字（＝行き止まりのローマ字）を母音補完/削除で直し、綺麗に
    /// 変換できればそれを提案する。母音待ちの正常な途中入力（末尾の k/sh 等）は
    /// 対象外（失敗ではないので出さない）。
    pub(crate) fn romaji_repair_suggest(&self) -> Option<(String, String)> {
        let embedded = self.hiragana_buffer.chars().any(|c| c.is_ascii_alphabetic());
        let trailing_stuck =
            !self.romaji_buffer.is_empty() && self.is_stuck_romaji(&self.romaji_buffer);
        if !embedded && !trailing_stuck {
            return None; // 取り残しなし（正常）
        }
        let converter = self.converter.as_ref()?;
        let full = format!("{}{}", self.hiragana_buffer, self.romaji_buffer);
        if full.chars().count() > 20 {
            return None;
        }

        // 綺麗に変換できる候補の中から、**最も自然（総コスト最小）**なものを選ぶ。
        // 母音の順番ではなくコストで選ぶことで、紗綾なら のような造語ではなく
        // さよなら のような意味の通る語が選ばれる。
        let mut best: Option<(String, String, i32)> = None;
        for reading in self.romaji_repair_readings(&full) {
            if reading.chars().count() < 2 {
                continue;
            }
            if let Some((surface, cost)) = converter.clean_reading(&reading) {
                if best.as_ref().map_or(true, |(_, _, bc)| cost < *bc) {
                    best = Some((reading, surface, cost));
                }
            }
        }
        let (reading, surface, cost) = best?;
        // 造語ガード: 1文字あたりのコストが高い（＝不自然な語の寄せ集め）なら
        // 「意味を成す補正が見つからなかった」として出さない。もしかして
        // （かな置換）側の best_word_correction と共通のしきい値を使う。
        if !common::viterbi::is_plausible_correction_cost(cost, reading.chars().count()) {
            return None;
        }
        Some((reading, surface))
    }

    /// 「かな＋英字」混在文字列から、英字（取り残しローマ字）を直した「読み」候補
    /// （latin を含まないかな列）を生成する。誤字パターンを幅広くカバーする:
    ///   A) 母音抜け: 各英字の直後に母音を入れる（先頭/中間/末尾・最大3英字の全組合せ）
    ///   B) 子音を母音で打ち間違え: 各英字を母音に置換
    ///   C) 打ちすぎ: 英字を1個ずつ削除
    ///   D) 入れ替え: 英字と隣接文字を入れ替え（タイプミスの転置）
    ///   E) フォールバック: 英字を全部削除
    /// これらを romaji 変換して「英字が残らず確定するもの」だけを候補にする。
    pub(crate) fn romaji_repair_readings(&self, full: &str) -> Vec<String> {
        let chars: Vec<char> = full.chars().collect();
        let latin_idx: Vec<usize> = chars
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_ascii_alphabetic())
            .map(|(i, _)| i)
            .collect();
        let k = latin_idx.len();
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        if (1..=3).contains(&k) {
            let combos = vowel_combos(k); // 5^k（k<=3 で最大125）
            // A) 母音補完（各英字の後ろに母音）
            for combo in &combos {
                let mut edited: Vec<char> = Vec::with_capacity(chars.len() + k);
                let mut li = 0;
                for (i, &ch) in chars.iter().enumerate() {
                    edited.push(ch);
                    if li < k && latin_idx[li] == i {
                        edited.push(combo[li]);
                        li += 1;
                    }
                }
                self.push_settle(&edited, &mut out, &mut seen);
            }
            // B) 母音置換（各英字を母音に置き換え）
            for combo in &combos {
                let mut edited = chars.clone();
                for (li, &idx) in latin_idx.iter().enumerate() {
                    edited[idx] = combo[li];
                }
                self.push_settle(&edited, &mut out, &mut seen);
            }
        }
        // C) 各英字を1個削除
        for &i in &latin_idx {
            let mut edited = chars.clone();
            edited.remove(i);
            self.push_settle(&edited, &mut out, &mut seen);
        }
        // D) 英字と隣接文字を入れ替え（転置ミス）
        for &i in &latin_idx {
            if i + 1 < chars.len() {
                let mut edited = chars.clone();
                edited.swap(i, i + 1);
                self.push_settle(&edited, &mut out, &mut seen);
            }
            if i > 0 {
                let mut edited = chars.clone();
                edited.swap(i - 1, i);
                self.push_settle(&edited, &mut out, &mut seen);
            }
        }
        // F) 'n' は末尾で「ん」になり損ねやすい（例 nippon→にっぽn）→ ん に置換
        for &i in &latin_idx {
            if chars[i] == 'n' {
                let mut edited = chars.clone();
                edited[i] = 'ん';
                self.push_settle(&edited, &mut out, &mut seen);
            }
        }
        // E) フォールバック: 英字を全部削除
        let no_latin: Vec<char> = chars
            .iter()
            .cloned()
            .filter(|c| !c.is_ascii_alphabetic())
            .collect();
        self.push_settle(&no_latin, &mut out, &mut seen);

        out.truncate(80); // clean_reading(Viterbi) の回数を抑える上限
        out
    }

    /// 編集後の列を romaji 変換して、英字が残らず2文字以上のかな列になれば out に足す。
    pub(crate) fn push_settle(
        &self,
        edited: &[char],
        out: &mut Vec<String>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        if let Some(r) = self.romaji_settle(edited) {
            if seen.insert(r.clone()) {
                out.push(r);
            }
        }
    }

    /// 編集後の（かな＋英字）列をローマ字変換し、英字が残らず2文字以上の
    /// かな列になったらそれを返す（＝ちゃんと確定した読み）。
    pub(crate) fn romaji_settle(&self, edited: &[char]) -> Option<String> {
        let s: String = edited.iter().collect();
        let conv = self.romaji.convert(&s);
        if conv.chars().any(|c| c.is_ascii_alphabetic()) || conv.chars().count() < 2 {
            return None;
        }
        Some(conv)
    }

    /// 末尾ローマ字が「行き止まり」（母音を足しても確定しない＝打ち間違い）か。
    pub(crate) fn is_stuck_romaji(&self, tail: &str) -> bool {
        if tail.is_empty() {
            return false;
        }
        for vowel in ['a', 'i', 'u', 'e', 'o'] {
            let mut r = tail.to_string();
            r.push(vowel);
            let (settled, pending) = self.romaji.split(&r);
            if pending.is_empty() && !settled.is_empty() {
                return false; // 母音で確定する＝正常な途中入力
            }
        }
        true
    }

    /// 予測変換の候補を更新する
    ///
    /// - 打鍵中（hiragana_buffer あり）: 読みが前方一致する確定履歴を補完候補に
    /// - 確定直後（buffer 空・last_committed あり）: 次単語をバイグラムから予測
    pub(crate) fn update_predictions(&mut self) {
        self.predictions.clear();
        self.prediction_top_is_fuzzy = false;
        self.prediction_top_is_learned_typo = false;
        self.prediction_index = 0;

        // 0) 過去に実際にユーザーが確定した誤字修正（学習済み）が今回の読みに
        //    そのまま一致するなら最優先で使う。都度のローマ字探索・辞書の
        //    あいまい検索より速く、かつユーザー本人が一度確定した確実な
        //    修正なので誤検出の心配が無い（personalized correction）。
        let learned_typo = self.converter.as_ref().and_then(|conv| {
            conv.typo_corrections
                .get(&self.hiragana_buffer)
                .map(|(reading, surface, _bonus)| (reading.clone(), surface.clone()))
        });
        if let Some((reading, surface)) = learned_typo {
            if surface != self.conversion_result {
                self.predictions.push((reading, surface));
                self.prediction_top_is_fuzzy = true;
                self.prediction_top_is_learned_typo = true;
            }
        }

        // もしかして（誤字補正）は「ローマ字取り残し」（gm… のように英字が
        // 残って変換に失敗）→ romaji_repair のみを対象にする。
        //
        // 以前は「ローマ字は全部かなになったが、辞書上意味を成さない断片
        // （するう→する う、さい+起動 等）」も fuzzy_suggest で拾っていたが、
        // これは辞書のあいまい検索による“その場の推測”で、外れることも
        // 多くノイズになるという実機フィードバックにより対象から外した
        // （[[fuzzy-correction-approach]]参照）。上の学習済み誤字修正
        // （ユーザー本人が過去に確定した確実な修正）は対象外にしない。
        // 学習済み修正が既に見つかっていれば、そちらを優先する
        // （同じ読みに対して重複した候補を出さないため）。
        if !self.predictions.is_empty() {
            // 学習済み修正を既に積んだので何もしない
        } else if let Some((reading, surface)) = self.romaji_repair_suggest() {
            self.predictions.push((reading, surface));
            self.prediction_top_is_fuzzy = true;
        }

        // ここから先（履歴による前方一致補完）は学習DBが要る。
        if self.learning.is_none() {
            return;
        }
        // 前方一致補完（履歴）のみ。2文字以上打った時だけ、打った読みより長い履歴を
        // 候補にする（頻度1以上）。前方一致なので無関係語は出ない。
        let prefix_len = self.hiragana_buffer.chars().count();
        if prefix_len >= 2 {
            let prefix_list = self
                .learning
                .as_ref()
                .and_then(|l| l.predict_by_prefix(&self.hiragana_buffer, 5).ok())
                .unwrap_or_default();
            for (reading, surface, freq) in prefix_list {
                // もしかしてと重複する表記は出さない
                let dup = self.predictions.iter().any(|(_, s)| *s == surface);
                if freq >= 1 && reading.chars().count() > prefix_len && !dup {
                    self.predictions.push((reading, surface));
                }
            }
        }

        // 定型句の続き予測: 打った読みが確定語にちょうど一致した（語の
        // 切れ目に来た）ら、その語の後によく続く言い回し（例「削除」→
        // 「して下さい」）を1件だけ追記候補として提案する。
        // 追加のかな漢字変換はせず索引検索のみなので、prefix_len>=2 の
        // 前方一致補完と同じコスト感で毎打鍵呼んでも問題ない。
        if prefix_len >= 2 && self.romaji_buffer.is_empty() {
            if let Some(learning) = self.learning.as_ref() {
                if let Ok(Some((_, tail))) = learning.predict_phrase_tail(&self.hiragana_buffer) {
                    let dup = self.predictions.iter().any(|(_, s)| *s == tail);
                    if !dup {
                        // reading を空にする＝現在の表示に追記するモード
                        // （commit_prediction の既存の追記規約に合わせる）
                        self.predictions.push((String::new(), tail));
                    }
                }
            }
        }
    }

    /// 予測候補を選んで確定する（番号キー）
    ///
    /// 前方一致補完: 現在の表示を予測語の表記に置き換えて確定。
    /// 次単語予測(読み空): 現在の表示の後ろに追記して確定。
    pub(crate) fn commit_prediction(&mut self, index: usize) -> Option<ConversionAction> {
        if !self.enabled || index >= self.predictions.len() {
            return None;
        }
        let (reading, surface) = self.predictions[index].clone();
        // 誤字修正の学習用に、確定でクリアされる前の生の読み（誤字そのもの）を控える。
        let original_reading = self.hiragana_buffer.clone();
        // 前方一致は現在表示を置換、次単語は追記
        let delete_count = if reading.is_empty() {
            0
        } else {
            self.conversion_result.chars().count()
        };
        let action = ConversionAction {
            delete_count,
            insert_text: surface.clone(),
        };

        // 学習（確定として記録）
        if let Some(learning) = self.learning.as_ref() {
            if !reading.is_empty() && is_learnable_pair(&reading, &surface) {
                let _ = learning.record_commit(&reading, &surface, None);
                let freq = learning.find_frequency(&reading, &surface).unwrap_or(1);
                if let Some(conv) = self.converter.as_mut() {
                    conv.learn_unigram(&reading, &surface, freq);
                }
            }
            if !self.last_committed.is_empty() {
                let _ = learning.record_bigram(&self.last_committed, &surface);
                let freq = learning
                    .find_bigram_frequency(&self.last_committed, &surface)
                    .unwrap_or(1);
                if let Some(conv) = self.converter.as_mut() {
                    conv.learn_bigram(&self.last_committed, &surface, freq);
                }
            }
            // もしかして（誤字補正）の先頭候補を実際に選んだ＝その場で提示した
            // 修正をユーザーが確認・採用したということなので、誤字そのもの
            // （original_reading）→修正後(reading, surface) を学習する。
            // 次回同じ誤字を打ったとき、辞書のあいまい検索をやり直さずに
            // 即座に同じ修正を最優先で出せるようになる（使うほど賢くなる）。
            if index == 0
                && self.prediction_top_is_fuzzy
                && original_reading != reading
                && !original_reading.is_empty()
                && is_learnable_pair(&reading, &surface)
            {
                let _ = learning.record_typo_correction(&original_reading, &reading, &surface);
                let freq = learning
                    .find_typo_correction_frequency(&original_reading, &reading, &surface)
                    .unwrap_or(1);
                if let Some(conv) = self.converter.as_mut() {
                    conv.learn_typo_correction(&original_reading, &reading, &surface, freq);
                }
                debug_log!(
                    "誤字修正を学習: '{}' → 読み='{}' 表記='{}'",
                    original_reading, reading, surface
                );
            }
        }

        // 文脈・状態を更新して確定扱いにする
        self.recent_context.push_str(&surface);
        let chars: Vec<char> = self.recent_context.chars().collect();
        if chars.len() > 60 {
            self.recent_context = chars[chars.len() - 60..].iter().collect();
        }
        self.last_committed = surface;
        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.conversion_result.clear();
        self.last_sent_length = 0;
        self.committed_segments.clear();
        self.kana_tail_len = 0;
        self.generation = self.generation.wrapping_add(1);
        self.clear_candidates();
        // 確定後は次単語予測を用意
        self.update_predictions();
        Some(action)
    }

    /// 予測候補の表示用文字列（番号は描画側で付く）。先頭が誤字補正なら
    /// 「もしかして」ラベルを付けて、履歴補完と区別する。読みが空（＝選ぶと
    /// 現在の表示に追記するモード）の候補には「→」を付け、置き換えと
    /// 区別できるようにする。
    pub(crate) fn prediction_display(&self) -> Vec<String> {
        self.predictions
            .iter()
            .enumerate()
            .map(|(i, (reading, s))| {
                if i == 0 && self.prediction_top_is_fuzzy {
                    format!("もしかして: {}", s)
                } else if reading.is_empty() {
                    format!("→{}", s)
                } else {
                    s.clone()
                }
            })
            .collect()
    }

    /// 予測一覧で選択中の候補の「誤学習」をリセットする（Delete キー）。
    ///
    /// 「誤字を修正するう」のように、過去に誤って確定した内容がそのまま履歴補完
    /// として出てくる場合、その (読み, 表記) の学習だけを消す。先頭が「もしかして」
    /// の場合、その場で計算した誤字補正（学習由来ではない）なら消すものが無いので
    /// 何もしないが、学習済みの誤字修正（prediction_top_is_learned_typo）なら
    /// その学習を消す。リセットしたら true。
    pub(crate) fn reset_prediction_learning(&mut self) -> bool {
        if self.predictions.is_empty() {
            return false;
        }
        let idx = self.prediction_index.min(self.predictions.len() - 1);
        if self.prediction_top_is_fuzzy && idx == 0 {
            // 学習済みの誤字修正なら、その誤字学習だけを消す
            // （その場計算のもしかしてには消す学習が無いので何もしない）。
            if !self.prediction_top_is_learned_typo {
                return false;
            }
            let wrong_reading = self.hiragana_buffer.clone();
            let (reading, surface) = self.predictions[0].clone();
            if wrong_reading.is_empty() {
                return false;
            }
            if let Some(learning) = self.learning.as_ref() {
                let _ = learning.forget_typo_correction(&wrong_reading, &reading, &surface);
            }
            if let Some(conv) = self.converter.as_mut() {
                conv.forget_typo_correction(&wrong_reading);
            }
            debug_log!(
                "誤字修正の学習リセット: '{}' → 読み='{}' 表記='{}'",
                wrong_reading, reading, surface
            );
            self.update_predictions();
            return true;
        }
        let (reading, surface) = self.predictions[idx].clone();
        if reading.is_empty() || surface.is_empty() {
            return false;
        }
        if let Some(learning) = self.learning.as_ref() {
            let _ = learning.forget_commit(&reading, &surface);
        }
        if let Some(conv) = self.converter.as_mut() {
            conv.forget_unigram(&reading, &surface);
        }
        debug_log!("予測の誤学習リセット: 読み='{}' 表記='{}'", reading, surface);
        // 消した結果で予測を作り直す
        self.update_predictions();
        true
    }

    /// 候補一覧の状態をすべてクリア
    pub(crate) fn clear_candidates(&mut self) {
        self.candidates.clear();
        self.candidate_index = 0;
        self.cand_seg_reading.clear();
        self.cand_seg_surfaces.clear();
        self.cand_prefix_surface.clear();
        self.cand_prefix_reading.clear();
        self.cand_suffix_surface.clear();
        self.cand_suffix_reading.clear();
    }

    /// 現在の未確定部分（hiragana_buffer）を文節列に分解する
    ///
    /// 候補選択中なら先頭文節はその選択表記、残りは1-best。
    /// 未選択なら全体を1-bestで分解する。学習の記録に使う。
    pub(crate) fn segment_remaining(&self) -> Vec<(String, String, String)> {
        let Some(conv) = self.converter.as_ref() else {
            return Vec::new();
        };
        if self.hiragana_buffer.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.cand_seg_surfaces.is_empty()
            && self.candidate_index < self.cand_seg_surfaces.len()
            && !self.cand_seg_reading.is_empty()
        {
            // 候補選択あり: 前半 + 選択した対象文節 + 後半 で分解
            for e in conv.convert(&self.cand_prefix_reading) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
            let seg_surface = self.cand_seg_surfaces[self.candidate_index].clone();
            // 候補選択した語は内容語とみなす（品詞は名詞相当を既定に）
            out.push((self.cand_seg_reading.clone(), seg_surface, "名詞-一般".to_string()));
            for e in conv.convert(&self.cand_suffix_reading) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
        } else if self.kana_tail_len > 0 {
            // Escで戻した末尾はかなのまま（学習しない）、前半だけ変換して学習
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(self.kana_tail_len);
            let prefix: String = self.hiragana_buffer.chars().take(keep).collect();
            for e in conv.convert(&prefix) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
            let tail: String = self.hiragana_buffer.chars().skip(keep).collect();
            if !tail.is_empty() {
                out.push((tail.clone(), tail, "名詞-一般".to_string()));
            }
        } else {
            for e in conv.convert(&self.hiragana_buffer) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
        }
        out
    }

    /// 確定した文節列から学習する（DB記録 + 変換エンジンのメモリ更新）
    ///
    /// - 各文節の (読み→表記) をユニグラムとして記録
    /// - 隣接する文節の (前表記→次表記) をバイグラムとして記録
    /// メモリも即時更新するので、次の変換からすぐ賢くなる。
    pub(crate) fn learn_from_segments(&mut self, segments: &[(String, String, String)]) {
        let Some(learning) = self.learning.as_ref() else {
            return;
        };
        // ユニグラム（読み→表記）
        for (reading, surface, pos) in segments {
            if reading == surface {
                continue; // ひらがなそのままは学習しない
            }
            if !is_learnable_pair(reading, surface) {
                continue; // 助詞の漢字化・英字ゴミ等は学習しない
            }
            // 接尾辞（性・的・化 等）は単独ユニグラムとして学習しない。
            // 「可能性」→ 可能+性 のように複合語の一部で出るため、単独で
            // 学習すると「せい→性」が強まり「しんせい」が「しん性」に割れる。
            // 語のつながりは下のバイグラム（可能→性）で捕捉する。
            if pos.contains("接尾") {
                continue;
            }
            let _ = learning.record_commit(reading, surface, None);
            let freq = learning.find_frequency(reading, surface).unwrap_or(1);
            if let Some(conv) = self.converter.as_mut() {
                conv.learn_unigram(reading, surface, freq);
            }
        }
        // バイグラム（隣接する表記のつながり）
        // 収集だけしておき、実際の登録は関数末尾（learning の借用が終わった後）で行う
        // （register_auto_compound は &mut self で learning/converter 両方に触るため）。
        let mut auto_compound_candidates: Vec<(String, String)> = Vec::new();
        for pair in segments.windows(2) {
            let prev = &pair[0].1;
            let next = &pair[1].1;
            if prev.is_empty() || next.is_empty() {
                continue;
            }
            // 助詞の読みが絡む/英字ゴミのペアは学習しない
            if !is_learnable_pair(&pair[0].0, prev) || !is_learnable_pair(&pair[1].0, next) {
                continue;
            }
            let _ = learning.record_bigram(prev, next);
            let freq = learning.find_bigram_frequency(prev, next).unwrap_or(1);
            if let Some(conv) = self.converter.as_mut() {
                conv.learn_bigram(prev, next, freq);
            }
            // 隣接する漢字複合語の自動登録判定（辞書に無く・両方とも漢字を
            // 含む内容語・反復して確定されている場合、1語として登録する）。
            if freq >= AUTO_COMPOUND_MIN_FREQ
                && common::viterbi::is_content_pos(&pair[0].2)
                && common::viterbi::is_content_pos(&pair[1].2)
                && contains_kanji(prev)
                && contains_kanji(next)
            {
                let combined_reading = format!("{}{}", pair[0].0, pair[1].0);
                if combined_reading.chars().count() <= AUTO_COMPOUND_MAX_READING_LEN {
                    let combined_surface = format!("{}{}", prev, next);
                    let already_exists = self
                        .converter
                        .as_ref()
                        .and_then(|c| c.dictionary.lookup(&combined_reading))
                        .is_some_and(|entries| entries.iter().any(|e| e.surface == combined_surface));
                    if !already_exists {
                        auto_compound_candidates.push((combined_reading, combined_surface));
                    }
                }
            }
        }
        // 内容語連想（助詞・助動詞を飛ばした内容語どうしの繋がり）
        // 「会社…帰社」「新聞…記者」のように離れた語の関係を学習する。
        let mut last_content: Option<String> = None;
        for (reading, surface, pos) in segments {
            if !common::viterbi::is_content_pos(pos) || surface == reading || surface.is_empty() {
                continue;
            }
            if !is_learnable_pair(reading, surface) {
                continue; // 助詞の漢字化・英字ゴミは連想学習しない
            }
            if let Some(prev) = &last_content {
                let _ = learning.record_assoc(prev, surface);
                let freq = learning.find_assoc_frequency(prev, surface).unwrap_or(1);
                if let Some(conv) = self.converter.as_mut() {
                    conv.learn_assoc(prev, surface, freq);
                }
            }
            last_content = Some(surface.clone());
        }
        for (reading, surface) in auto_compound_candidates {
            self.register_auto_compound(&reading, &surface);
        }
    }

    /// 辞書をロード
    pub(crate) fn load_dictionary(&mut self, path: &Path) -> bool {
        debug_log!("Dictionary::load 呼び出し: {}", path.display());
        match Dictionary::load(path) {
            Ok(dict) => {
                debug_log!("辞書読み込み成功、ViterbiConverter作成中");
                self.converter = Some(ViterbiConverter::new(dict));
                // コーパス由来の語頻度データ（あれば）を読み込む。辞書と
                // 同じディレクトリの corpus_lm.dic を探す（無くても動作する
                // 任意のファイル。Stage 0の検証用資産）。
                let corpus_lm_path = path.with_file_name("corpus_lm.dic");
                if corpus_lm_path.exists() {
                    match common::CorpusLm::load(&corpus_lm_path) {
                        Ok(lm) => {
                            if let Some(conv) = self.converter.as_mut() {
                                conv.load_corpus_lm(&lm);
                            }
                            debug_log!(
                                "コーパスLMを読み込みました: {} (unigram={}, bigram={})",
                                corpus_lm_path.display(),
                                lm.unigrams.len(),
                                lm.bigrams.len()
                            );
                        }
                        Err(e) => {
                            debug_log!("コーパスLMのロードに失敗: {}", e);
                        }
                    }
                }
                // 過去の学習をライブ変換エンジンに反映
                self.reload_learning_into_converter();
                // ユーザー登録の単語を辞書へ注入（辞書に無い複合語を変換可能に）
                self.inject_user_words();
                debug_log!("辞書をロードしました: {}", path.display());
                println!("辞書をロードしました: {}", path.display());
                true
            }
            Err(e) => {
                debug_log!("辞書のロードに失敗: {}", e);
                eprintln!("辞書のロードに失敗: {}", e);
                false
            }
        }
    }

    /// 文字を追加
    pub(crate) fn add_char(&mut self, ch: char) -> Option<ConversionAction> {
        if !self.enabled {
            return None;
        }

        // Escでひらがなに戻した内容は「確定済み」として扱う。
        // 新しい入力が来たらそこまでを確定し、戻したかなを再変換しない。
        // （表示済みテキストはそのまま。内部状態だけ確定して新規入力を始める）
        if self.kana_tail_len > 0 {
            self.commit();
        }

        // バッファが変わるので候補リストは無効化し、世代を進める
        self.clear_candidates();
        self.generation = self.generation.wrapping_add(1);

        self.romaji_buffer.push(ch);
        debug_log!("入力: '{}' → ローマ字バッファ: '{}'", ch, self.romaji_buffer);

        // ローマ字バッファを「確定ひらがな」と「保留ローマ字」に分割
        // （先頭に変換不能な英字が残っても以降が全てローマ字化しないよう、
        //  末尾の英字断片のみを保留にする）
        let (settled, pending) = self.romaji.split(&self.romaji_buffer);
        if !settled.is_empty() {
            self.hiragana_buffer.push_str(&settled);
            self.romaji_buffer = pending;
        }

        debug_log!("現在の状態: ひらがな='{}', ローマ字='{}'", self.hiragana_buffer, self.romaji_buffer);

        // ひらがな→漢字変換（ライブ変換）
        self.update_conversion()
    }

    /// バックスペース処理
    pub(crate) fn backspace(&mut self) -> Option<ConversionAction> {
        if !self.enabled {
            return None;
        }

        self.clear_candidates();
        self.generation = self.generation.wrapping_add(1);
        self.kana_tail_len = 0;

        if !self.romaji_buffer.is_empty() {
            // 入力途中のローマ字は1文字ずつ削除
            self.romaji_buffer.pop();
        } else if !self.hiragana_buffer.is_empty() {
            // 変換済みの部分は「最後の変換単語（文節）」ごと削除する
            let last_len = self
                .converter
                .as_ref()
                .and_then(|c| c.convert(&self.hiragana_buffer).last().map(|e| e.reading.chars().count()))
                .filter(|&n| n > 0)
                .unwrap_or(1);
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(last_len);
            self.hiragana_buffer = self.hiragana_buffer.chars().take(keep).collect();
        } else {
            return None; // 削除するものがない
        }

        debug_log!("バックスペース後: ひらがな='{}', ローマ字='{}'", self.hiragana_buffer, self.romaji_buffer);
        self.update_conversion()
    }

    /// 変換を更新（macOS方式：ひらがな確定時のみ漢字変換）
    ///
    /// 共通プレフィックスを保持する差分計算で必要最小限の編集のみを送信する。
    /// これにより「今日」→「今日h」のように先頭が共通な場合は cursor を動かさず
    /// 末尾だけ更新できる。旧実装(毎回全削除→再挿入)は cursor が頻繁に左に飛び、
    /// 視覚的に「後の入力が前を上書きする」ように見える原因だった。
    pub(crate) fn update_conversion(&mut self) -> Option<ConversionAction> {
        // ひらがなバッファのみを漢字変換
        // ローマ字バッファはそのまま末尾に追加
        let converted_hiragana = if let Some(converter) = &self.converter {
            if self.hiragana_buffer.is_empty() {
                String::new()
            } else if self.kana_tail_len > 0 {
                // Escで戻した末尾はひらがなのまま、前半だけ変換する
                let total = self.hiragana_buffer.chars().count();
                let keep = total.saturating_sub(self.kana_tail_len);
                let prefix: String = self.hiragana_buffer.chars().take(keep).collect();
                let tail: String = self.hiragana_buffer.chars().skip(keep).collect();
                let conv = if prefix.is_empty() {
                    String::new()
                } else {
                    converter.convert_context_aware_to_string(&prefix)
                };
                format!("{}{}", conv, tail)
            } else {
                // 文脈（内容語の繋がり）を考慮して尤もらしい変換を選ぶ
                converter.convert_context_aware_to_string(&self.hiragana_buffer)
            }
        } else {
            // 辞書がない場合はひらがなのまま
            debug_log!("辞書なし: ひらがなのまま '{}'", self.hiragana_buffer);
            self.hiragana_buffer.clone()
        };

        // 変換結果 + ローマ字（未確定）
        let new_result = format!("{}{}", converted_hiragana, self.romaji_buffer);
        self.apply_new_result(new_result)
    }

    /// 表示テキストを new_result に差し替えるための差分アクションを作る
    pub(crate) fn apply_new_result(&mut self, new_result: String) -> Option<ConversionAction> {
        if new_result == self.conversion_result {
            debug_log!("変化なし: アクションなし");
            return None;
        }

        // 共通プレフィックスを文字単位で計算
        let old_chars: Vec<char> = self.conversion_result.chars().collect();
        let new_chars: Vec<char> = new_result.chars().collect();
        let common = old_chars
            .iter()
            .zip(new_chars.iter())
            .take_while(|(a, b)| a == b)
            .count();

        let delete_count = old_chars.len() - common;
        let insert_text: String = new_chars[common..].iter().collect();

        debug_log!(
            "差分: '{}' → '{}' (共通={}, 削除={}, 挿入='{}')",
            self.conversion_result, new_result, common, delete_count, insert_text
        );

        self.conversion_result = new_result;
        self.last_sent_length = new_chars.len();

        if delete_count == 0 && insert_text.is_empty() {
            return None;
        }

        Some(ConversionAction {
            delete_count,
            insert_text,
        })
    }

    /// 次/前の変換候補に切り替える（Tab / Space）
    ///
    /// 初回呼び出し時に N-best 候補を生成し、以降は循環する。
    pub(crate) fn cycle_candidate(&mut self, backwards: bool) -> Option<ConversionAction> {
        if !self.enabled || self.hiragana_buffer.is_empty() || self.converter.is_none() {
            return None;
        }

        if self.candidates.is_empty() {
            // 一覧を初めて開いたとき: 候補1(index 0)を選択状態にして表示する
            // （表示中のライブ変換結果が候補1と一致しないことがあるため、
            //  最初の Tab で候補1へ切り替える。次の Tab から順送りになる）
            let (
                candidates,
                seg_reading,
                seg_surfaces,
                prefix_surface,
                prefix_reading,
                suffix_surface,
                suffix_reading,
            ) = self.build_candidates();
            if candidates.len() < 2 {
                return None; // 切り替える候補がない
            }
            self.candidates = candidates;
            self.cand_seg_reading = seg_reading;
            self.cand_seg_surfaces = seg_surfaces;
            self.cand_prefix_surface = prefix_surface;
            self.cand_prefix_reading = prefix_reading;
            self.cand_suffix_surface = suffix_surface;
            self.cand_suffix_reading = suffix_reading;
            self.candidate_index = 0;
            return self.select_candidate(0);
        }

        let len = self.candidates.len();
        let next = if backwards {
            (self.candidate_index + len - 1) % len
        } else {
            (self.candidate_index + 1) % len
        };
        self.select_candidate(next)
    }

    /// 候補一覧で選択中の候補について「誤学習」をリセットする（Delete キー）。
    ///
    /// その候補の対象文節の (読み, 表記) の学習だけを DB とメモリの両方から消し、
    /// 候補を作り直して並び順を更新する。過去に誤って確定して上位に居座っていた
    /// 変換を、その1件だけ取り消せる（同じ読みの他の正しい学習は残る）。
    /// 表示を更新するアクションを返す（候補が無くなれば None）。
    pub(crate) fn reset_learning_for_selected(&mut self) -> Option<ConversionAction> {
        if !self.enabled || self.candidates.is_empty() {
            return None;
        }
        let idx = self.candidate_index.min(self.candidates.len() - 1);
        let reading = self.cand_seg_reading.clone();
        let surface = self.cand_seg_surfaces.get(idx).cloned()?;
        if reading.is_empty() || surface.is_empty() {
            return None;
        }

        // DB（確定履歴＋バイグラム）とメモリ（ユニグラム等）の両方から消す。
        if let Some(learning) = self.learning.as_ref() {
            let _ = learning.forget_commit(&reading, &surface);
        }
        if let Some(conv) = self.converter.as_mut() {
            conv.forget_unigram(&reading, &surface);
        }
        debug_log!("誤学習リセット: 読み='{}' 表記='{}'", reading, surface);

        // 学習が変わったので候補を作り直す（並び順が更新される）。
        let (
            candidates,
            seg_reading,
            seg_surfaces,
            prefix_surface,
            prefix_reading,
            suffix_surface,
            suffix_reading,
        ) = self.build_candidates();
        if candidates.is_empty() {
            self.candidates.clear();
            return None;
        }
        self.candidates = candidates;
        self.cand_seg_reading = seg_reading;
        self.cand_seg_surfaces = seg_surfaces;
        self.cand_prefix_surface = prefix_surface;
        self.cand_prefix_reading = prefix_reading;
        self.cand_suffix_surface = suffix_surface;
        self.cand_suffix_reading = suffix_reading;
        // 先頭（学習リセット後の最有力候補）を選択して表示を更新する。
        self.candidate_index = 0;
        self.select_candidate(0)
    }

    /// Escで、まだ変換されている末尾の文節を一つ、ひらがなに戻す
    ///
    /// 呼ぶたびに末尾から一文節ずつ戻していく（累積）。戻す対象が
    /// 残っていれば表示を更新するアクションを返し、全てひらがなに
    /// 戻し終えていれば `None`（呼び出し側は取消にフォールバック）。
    pub(crate) fn extend_kana_revert(&mut self) -> Option<ConversionAction> {
        if !self.enabled || self.hiragana_buffer.is_empty() {
            return None;
        }
        let total = self.hiragana_buffer.chars().count();
        if self.kana_tail_len >= total {
            return None; // すべてひらがなに戻し済み
        }
        // まだ変換されている前半 = 先頭から (total - kana_tail_len) 文字
        let keep = total - self.kana_tail_len;
        let prefix: String = self.hiragana_buffer.chars().take(keep).collect();
        let Some(converter) = self.converter.as_ref() else {
            return None;
        };
        let entries = converter.convert(&prefix);
        // 前半の「最後の変換された文節」以降を、ひらがな末尾に加える
        let revert_len: usize = if let Some(idx) =
            entries.iter().rposition(|e| e.surface != e.reading)
        {
            entries[idx..].iter().map(|e| e.reading.chars().count()).sum()
        } else {
            // 変換済み文節が無ければ残り全部をかなに
            keep
        };
        self.kana_tail_len = (self.kana_tail_len + revert_len.max(1)).min(total);
        self.clear_candidates();
        self.update_conversion()
    }

    /// 候補一覧を組み立てる（直近＝最後の文節の同音語をコスト＋学習頻度順に）
    ///
    /// 候補の対象は「一番最後に打った文節」。前半（それより前）は
    /// 変換済みのまま固定し、最後の文節だけを差し替える。これにより
    /// Tab を押しても前の文が変わらず、直近で入力した語だけを選べる。
    ///
    /// 戻り値: (候補文字列, 対象文節の読み, 各候補の対象文節表記,
    ///          前半の変換済み表記, 前半の読み, 後半の変換済み表記, 後半の読み)
    pub(crate) fn build_candidates(
        &self,
    ) -> (Vec<String>, String, Vec<String>, String, String, String, String) {
        let empty = || {
            (
                Vec::new(),
                String::new(),
                Vec::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            )
        };
        let Some(converter) = self.converter.as_ref() else {
            return empty();
        };

        // ライブ表示と同じ context_aware で分解する。
        let entries = converter.convert_context_aware(&self.hiragana_buffer);
        if entries.is_empty() {
            return empty();
        }

        // 対象は「最後の“変換された”文節」。表層==読み（＝ひらがなのまま）の
        // 文節は変換とみなさず飛ばす。主に 平仮名→漢字/カタカナ を拾うため。
        // 変換済みが1つも無ければ最後の文節を対象にする。
        let end = entries
            .iter()
            .rposition(|e| e.surface != e.reading)
            .unwrap_or(entries.len() - 1);
        // 隣接する1文字漢字は分割された複合語のことが多いので、対象を
        // 左へ広げて連続する1文字漢字の文節をまとめて変換対象にする
        //（例: 変+換 が別文節でも「へんかん」全体を対象にする）。
        let mut start = end;
        while start > 0
            && is_single_kanji_entry(&entries[start])
            && is_single_kanji_entry(&entries[start - 1])
        {
            start -= 1;
        }
        // 対象span [start..=end] の結合読み
        let seg_reading: String =
            entries[start..=end].iter().map(|e| e.reading.as_str()).collect();

        // 前半（対象より前）と後半（対象より後ろ＝末尾の平仮名など）
        let prefix_surface: String =
            entries[..start].iter().map(|e| e.surface.as_str()).collect();
        let prefix_reading: String =
            entries[..start].iter().map(|e| e.reading.as_str()).collect();
        let suffix_surface: String =
            entries[end + 1..].iter().map(|e| e.surface.as_str()).collect();
        let suffix_reading: String =
            entries[end + 1..].iter().map(|e| e.reading.as_str()).collect();

        // 対象文節の同音語を集め、実際にその位置に置いたときの文全体コストで
        // 並べる。以前は対象語だけの辞書コスト＋ペナルティ－学習ボーナスという
        // 独自の簡易式（前後の語との接続コストを一切見ない）で並べていたため、
        // 自動変換（文全体のViterbi）の1-bestと候補一覧の1位が食い違ったり、
        // 接続コストの都合で本来は選ばれない低頻度語が候補一覧の上位に出る
        // ことがあった。ここでは`effective_word_cost`（学習ユニグラム・
        // コーパス頻度・1文字漢字ペナルティ込みの単語自身のコスト）に、
        // 前後の確定済み文節との接続コスト（`context_connection_cost`。
        // 学習/コーパスのバイグラムボーナスを含む、自動変換と同じ計算式）を
        // 加えた合計で並べる。これにより自動変換の1-bestと候補一覧の1位が
        // 構造的に一致する（ただし`scoring.rs`のガード関数群は含まない簡易版
        // であり、完全に同一のロジックではない）。
        struct Seg {
            surface: String,
            eff_cost: i32,
        }
        let prev_entry = if start > 0 { entries.get(start - 1) } else { None };
        let next_entry = entries.get(end + 1);
        let mut segs: Vec<Seg> = Vec::new();
        if let Some(words) = converter.dictionary.lookup(&seg_reading) {
            for w in words {
                // 記号品詞（ギリシャ文字等）は辞書コストが低くても実用語より
                // 優先させない（scoring.rsの`symbol_penalty`はASCII/全角記号
                // だけを対象にしており、χ等の記号POS全般はカバーしないため
                // ここで別途ペナルティを科す）。
                let penalty = symbol_entry_penalty(&w.pos);
                let own_cost = converter.effective_word_cost(w).saturating_add(penalty);
                let ctx_cost = converter.context_connection_cost(prev_entry, w, next_entry);
                segs.push(Seg {
                    surface: w.surface.clone(),
                    eff_cost: own_cost.saturating_add(ctx_cost),
                });
            }
        }
        // カタカナ・ひらがな表記も候補に含める（末尾寄り）
        let katakana = common::hiragana_to_katakana(&seg_reading);
        if katakana != seg_reading {
            segs.push(Seg { surface: katakana, eff_cost: 50000 });
        }
        segs.push(Seg { surface: seg_reading.clone(), eff_cost: 52000 });

        segs.sort_by(|a, b| a.eff_cost.cmp(&b.eff_cost));

        // 前半（固定）+ 対象文節表記 + 後半（固定）で候補文字列を作る
        let mut seen = std::collections::HashSet::new();
        let mut candidates: Vec<String> = Vec::new();
        let mut seg_surfaces: Vec<String> = Vec::new();
        for seg in segs {
            let cand = format!("{}{}{}", prefix_surface, seg.surface, suffix_surface);
            if seen.insert(cand.clone()) {
                candidates.push(cand);
                seg_surfaces.push(seg.surface);
            }
            // 同音異義語は読みによっては20語以上あり（「けん」「こう」等）、
            // 9件では目的の字まで辿り着けないことが多い。多めに集めておき、
            // ポップアップ側は画面に収まる範囲をスクロールして全件を選べるようにする。
            if candidates.len() >= MAX_CONVERSION_CANDIDATES {
                break;
            }
        }
        (
            candidates,
            seg_reading,
            seg_surfaces,
            prefix_surface,
            prefix_reading,
            suffix_surface,
            suffix_reading,
        )
    }

    /// 指定インデックスの候補を選択して表示を更新する（番号キー選択）
    pub(crate) fn select_candidate(&mut self, index: usize) -> Option<ConversionAction> {
        if index >= self.candidates.len() {
            return None;
        }
        self.candidate_index = index;

        debug_log!(
            "候補選択: {}/{} '{}'",
            index + 1, self.candidates.len(), self.candidates[index]
        );

        let new_result = format!("{}{}", self.candidates[index], self.romaji_buffer);
        self.apply_new_result(new_result)
    }

    /// 先頭の1語を部分確定する。
    ///
    /// 「前半の変換は正しいが後半が違う」場合に、正しい前半を語単位で
    /// 順に確定していくための操作。現在は横矢印をカーソル移動に使うため
    /// 未割り当て（将来別キーに割り当てる可能性があるので残置）。
    #[allow(dead_code)]
    pub(crate) fn commit_first_word(&mut self) -> Vec<ConversionAction> {
        let mut actions = Vec::new();
        if !self.enabled || self.hiragana_buffer.is_empty() {
            return actions;
        }
        let Some(converter) = &self.converter else {
            return actions;
        };

        let entries = converter.convert(&self.hiragana_buffer);
        let Some(first) = entries.first() else {
            return actions;
        };
        let surface = first.surface.clone();
        let reading_len = first.reading.chars().count();

        // 候補選択中など、表示が1-bestと異なる場合は一旦1-best表示に戻す
        // （そうしないと画面上の先頭と確定する語がずれる）
        if !self.conversion_result.starts_with(&surface) {
            let full: String = entries.iter().map(|e| e.surface.as_str()).collect();
            let full = format!("{}{}", full, self.romaji_buffer);
            if let Some(action) = self.apply_new_result(full) {
                actions.push(action);
            }
        }

        // 先頭語を管理対象（未確定領域）から外す
        self.conversion_result = self
            .conversion_result
            .strip_prefix(&surface)
            .unwrap_or("")
            .to_string();
        self.last_sent_length = self.conversion_result.chars().count();
        self.hiragana_buffer = self.hiragana_buffer.chars().skip(reading_len).collect();
        self.clear_candidates();

        debug_log!(
            "部分確定: '{}' / 残り読み='{}'",
            surface, self.hiragana_buffer
        );

        // 部分確定した文節を記録（最終確定時にユニグラム/バイグラム/連想学習へ）
        self.committed_segments
            .push((first.reading.clone(), surface.clone(), first.pos.clone()));

        // 残り部分を単独で再変換（文脈が変わるため結果が変わり得る）
        if let Some(action) = self.update_conversion() {
            actions.push(action);
        }
        actions
    }

    /// 確定（Enter・句読点）
    ///
    /// 未確定のローマ字 'n' が残っていれば「ん」として取り込んでから確定する。
    pub(crate) fn commit(&mut self) -> Option<ConversionAction> {
        if !self.enabled || (self.conversion_result.is_empty() && !self.is_composing()) {
            return None;
        }

        // 末尾の未確定 'n' を「ん」に変換して表示を更新
        let action = if self.romaji_buffer == "n" {
            self.romaji_buffer.clear();
            self.hiragana_buffer.push('ん');
            if !self.candidates.is_empty() {
                // 候補選択中なら選択を維持したまま「ん」を足す
                let new_result = format!("{}ん", self.candidates[self.candidate_index]);
                self.apply_new_result(new_result)
            } else {
                self.update_conversion()
            }
        } else {
            None
        };

        // 学習: 確定した文全体を文節列に分解し、ユニグラム＋バイグラムを
        // 記録する。→ で部分確定済みの文節も連結して1文として学習する。
        // これによりライブ変換自体が使うほど賢くなり、語のつながり
        // （文全体の整合性）も学習される。
        let mut segments = std::mem::take(&mut self.committed_segments);
        segments.extend(self.segment_remaining());
        self.learn_from_segments(&segments);

        // Escでひらがなに戻した末尾は「この読みはひらがな優先」として学習。
        // 次回から その読みをひらがなのまま出しやすくする（例: したい）。
        if self.kana_tail_len > 0 {
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(self.kana_tail_len);
            let tail: String = self.hiragana_buffer.chars().skip(keep).collect();
            // 1文字（て・い 等の断片）は学習しない。単一かなをひらがな優先に
            // すると「ていけい→てい系」のように語頭が未変換になって壊れる。
            if tail.chars().count() >= 2 {
                let freq = if let Some(learning) = self.learning.as_ref() {
                    let _ = learning.record_hiragana_pref(&tail);
                    // その読みの漢字/カタカナ学習を忘れる（ひらがなを勝たせる）
                    let _ = learning.forget_reading(&tail);
                    learning.find_hiragana_pref(&tail).unwrap_or(1)
                } else {
                    0
                };
                if freq > 0 {
                    if let Some(conv) = self.converter.as_mut() {
                        conv.forget_reading(&tail);
                        conv.learn_hiragana(&tail, freq);
                    }
                }
            }
        }

        // 直近の確定テキストを文脈として蓄積（LLM変換に渡す。末尾60文字）
        let committed: String = segments.iter().map(|(_, s, _)| s.as_str()).collect();
        if !committed.is_empty() {
            self.recent_context.push_str(&committed);
            let chars: Vec<char> = self.recent_context.chars().collect();
            if chars.len() > 60 {
                self.recent_context = chars[chars.len() - 60..].iter().collect();
            }
        }
        // 次単語予測のため、最後の文節の表記を覚える
        if let Some((_, s, _)) = segments.last() {
            self.last_committed = s.clone();
        }

        // バッファをクリア（表示済みテキストはそのまま確定扱い）
        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.conversion_result.clear();
        self.last_sent_length = 0;
        self.committed_segments.clear();
        self.kana_tail_len = 0;
        self.generation = self.generation.wrapping_add(1);
        self.clear_candidates();
        // 確定直後は次単語予測を用意する
        self.update_predictions();

        action
    }

    /// キャンセル（Escキー）
    pub(crate) fn cancel(&mut self) -> Option<ConversionAction> {
        if !self.enabled {
            return None;
        }

        let delete_count = self.last_sent_length;

        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.conversion_result.clear();
        self.last_sent_length = 0;
        self.committed_segments.clear();
        self.kana_tail_len = 0;
        self.generation = self.generation.wrapping_add(1);
        self.clear_candidates();

        if delete_count > 0 {
            Some(ConversionAction {
                delete_count,
                insert_text: String::new(),
            })
        } else {
            None
        }
    }

    /// 変換が進行中かどうか
    pub(crate) fn is_composing(&self) -> bool {
        !self.romaji_buffer.is_empty() || !self.hiragana_buffer.is_empty()
    }
}

/// 変換アクション（何を削除して何を挿入するか）
pub(crate) struct ConversionAction {
    pub(crate) delete_count: usize,
    pub(crate) insert_text: String,
}

/// 単語エントリが「1文字の漢字」か（隣接漢字の結合判定に使う）
pub(crate) fn is_single_kanji_entry(e: &common::WordEntry) -> bool {
    let mut chars = e.surface.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => {
            ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
        }
        _ => false,
    }
}

/// 記号品詞（ギリシャ文字等の「記号-アルファベット」等）の候補に対する
/// コストペナルティ（候補並べ替え専用。辞書自体は変更しない）。
///
/// 例:「かい」でχ（記号-アルファベット, コスト1730）が、単語コストに
/// 1文字漢字ペナルティが乗った実在の漢字候補（介・階 等、7000超）より
/// 上位に来てしまう。IPA辞書は数式表記等での使用を想定した低コストで、
/// 通常の日本語入力で先頭に出てくるのは望ましくないため、候補一覧では
/// 大きく下げる（学習頻度があればそちらが優先されるので、意図して
/// よく使う場合は上位に出せる）。
pub(crate) fn symbol_entry_penalty(pos: &str) -> i32 {
    if pos.starts_with("記号") {
        20000
    } else {
        0
    }
}

/// 単語登録の読み欄として妥当か（ひらがな＋長音符のみ）
pub(crate) fn is_hiragana_reading(reading: &str) -> bool {
    reading
        .chars()
        .all(|c| ('\u{3041}'..='\u{3096}').contains(&c) || c == 'ー')
}

/// 表記に漢字（CJK統合漢字）が1文字以上含まれるか。
/// 隣接複合語の自動登録を「漢字を含む内容語どうし」に絞るための判定
/// （助詞やひらがなだけの語を巻き込まないため）。
pub(crate) fn contains_kanji(surface: &str) -> bool {
    surface.chars().any(|c| ('\u{4E00}'..='\u{9FAF}').contains(&c))
}

/// この (読み, 表記) ペアを学習してよいか
///
/// 誤学習で変換が悪化するのを防ぐガード:
/// - 助詞の読み（に・は・を 等）が漢字/数字に化けたもの（例 に→二）は
///   学習しない。助詞は常に既定表記であるべき。
/// - 英数字を含む表記（旧ローマ字バグ由来のゴミ等）は学習しない。
pub(crate) fn is_learnable_pair(reading: &str, surface: &str) -> bool {
    // 助詞・助動詞になりうる短い仮名の読み（これらが漢字化したら誤り）
    const PARTICLE_READINGS: &[&str] = &[
        "に", "は", "を", "へ", "が", "の", "と", "も", "や", "か",
        "で", "ね", "よ", "わ", "し", "ば", "な", "ぞ", "さ",
    ];
    if PARTICLE_READINGS.contains(&reading) && surface != reading {
        return false;
    }
    // 英数字混じりの表記は学習対象外（ゴミ・未変換ローマ字）
    if surface.chars().any(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let reading_len = reading.chars().count();
    // 1文字の読み→別表記は曖昧すぎる断片（例: じ→時）。誤変換の
    // 学習ループを招くので学習しない。
    if reading_len == 1 && surface != reading {
        return false;
    }
    // 短い読みを「その読みのカタカナ化」として学習しない（例: かん→カン）。
    // これはカタカナ・フォールバックの断片で、誤変換を強化してしまう。
    if reading_len <= 3 && surface == common::hiragana_to_katakana(reading) {
        return false;
    }
    true
}

#[cfg(test)]
mod build_candidates_tests {
    use super::*;

    /// 学習ユニグラム（頻出語プリセット含む）が、Tab候補一覧の並び順にも
    /// 反映されることを確認する。修正前は build_candidates が辞書コスト＋
    /// 1文字漢字ペナルティのみで並べていたため、ライブ変換の1-bestでは
    /// 正しく選べている頻出語が、たまたま辞書コストの低い稀な語に候補
    /// 一覧では負けてしまっていた。
    #[test]
    fn learned_unigram_bonus_ranks_common_word_first() {
        let mut dict = Dictionary::new();
        dict.matrix = common::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        // 「稀語」は辞書コストがたまたま低く設定されている想定（IPA辞書に
        // ありがちな癖の再現）。「常用語」は使用頻度は高いがコストは高め。
        dict.add_word(common::WordEntry {
            surface: "常用語".to_string(), reading: "きしゃ".to_string(),
            left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
        });
        dict.add_word(common::WordEntry {
            surface: "稀語".to_string(), reading: "きしゃ".to_string(),
            left_id: 1, right_id: 1, cost: 3000, pos: "名詞".to_string(),
        });
        let mut converter = ViterbiConverter::new(dict);
        // 「常用語」は既に何度か使われて学習済み（頻出語プリセットと同じ経路）
        converter.learn_unigram("きしゃ", "常用語", 3);

        let mut state = LiveConversionState::new();
        state.converter = Some(converter);
        state.hiragana_buffer = "きしゃ".to_string();

        let (candidates, _seg_reading, seg_surfaces, ..) = state.build_candidates();
        assert_eq!(seg_surfaces.first().map(String::as_str), Some("常用語"));
        assert_eq!(candidates.first().map(String::as_str), Some("常用語"));
    }

    /// 記号品詞（ギリシャ文字等）が、辞書コストの低さだけでTab候補一覧の
    /// 先頭に来ないことを確認する（例:「かい」でχが介・階等の実在の
    /// 漢字候補より上に出てしまっていた回帰）。
    #[test]
    fn symbol_entry_does_not_rank_above_real_kanji_word() {
        let mut dict = Dictionary::new();
        dict.matrix = common::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        // χ のように記号品詞かつ辞書コストが極端に低いエントリを再現
        dict.add_word(common::WordEntry {
            surface: "χ".to_string(), reading: "かい".to_string(),
            left_id: 1, right_id: 1, cost: 1730, pos: "記号-アルファベット-*-*".to_string(),
        });
        dict.add_word(common::WordEntry {
            surface: "介".to_string(), reading: "かい".to_string(),
            left_id: 1, right_id: 1, cost: 5608, pos: "名詞-サ変接続-*-*".to_string(),
        });
        let converter = ViterbiConverter::new(dict);

        let mut state = LiveConversionState::new();
        state.converter = Some(converter);
        state.hiragana_buffer = "かい".to_string();

        let (candidates, ..) = state.build_candidates();
        assert_ne!(candidates.first().map(String::as_str), Some("χ"), "記号が先頭に来てはいけない");
    }

    /// 前の文節との接続コストが候補一覧の並び順に反映されることを確認する
    /// （修正前は対象語だけの辞書コストで並べており、前後の文脈を一切
    /// 見ていなかった）。「硬貨」は単語コスト自体はわずかに安いが、直前の
    /// 「為替」との接続コストが「効果」よりずっと高い設定にしてあるため、
    /// 文全体では「効果」の方が自然（自動変換の1-bestも効果になる）。
    /// 候補一覧の1位もそれと一致するべき。
    #[test]
    fn context_connection_cost_affects_candidate_order() {
        let mut dict = Dictionary::new();
        dict.matrix = common::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        dict.matrix.set(1, 2, 50); // 為替→効果: 自然な接続
        dict.matrix.set(1, 3, 9000); // 為替→硬貨: 不自然な接続
        dict.add_word(common::WordEntry {
            surface: "為替".to_string(), reading: "かわせ".to_string(),
            left_id: 1, right_id: 1, cost: 3000, pos: "名詞".to_string(),
        });
        dict.add_word(common::WordEntry {
            surface: "効果".to_string(), reading: "こうか".to_string(),
            left_id: 2, right_id: 2, cost: 3000, pos: "名詞".to_string(),
        });
        dict.add_word(common::WordEntry {
            surface: "硬貨".to_string(), reading: "こうか".to_string(),
            left_id: 3, right_id: 3, cost: 2900, pos: "名詞".to_string(),
        });
        let converter = ViterbiConverter::new(dict);

        let mut state = LiveConversionState::new();
        state.hiragana_buffer = "かわせこうか".to_string();
        state.converter = Some(converter);

        // 自動変換（文全体の1-best）は「効果」を選ぶ
        let live = state.converter.as_ref().unwrap().convert_context_aware_to_string("かわせこうか");
        assert_eq!(live, "為替効果");

        // 候補一覧の1位も一致するべき（単語コスト単体では「硬貨」の方が
        // 安いが、文脈込みの合計コストでは「効果」が勝つ）
        let (candidates, _seg_reading, seg_surfaces, ..) = state.build_candidates();
        assert_eq!(seg_surfaces.first().map(String::as_str), Some("効果"));
        assert_eq!(candidates.first().map(String::as_str), Some("為替効果"));
    }
}

#[cfg(test)]
mod auto_compound_tests {
    use super::*;

    fn dict_without_compound() -> common::Dictionary {
        let mut dict = common::Dictionary::new();
        dict.add_word(common::WordEntry {
            surface: "再".to_string(), reading: "さい".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "接頭詞-名詞接続-*-*".to_string(),
        });
        dict.add_word(common::WordEntry {
            surface: "起動".to_string(), reading: "きどう".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-サ変接続-*-*".to_string(),
        });
        dict
    }

    /// 「再」+「起動」のように辞書に無い複合語（再起動）が3回続けて隣接して
    /// 確定されたら、辞書に単語として自動登録され、DBには source='auto' で
    /// 記録されることを確認する。1〜2回目ではまだ登録されない（反復を要求）。
    #[test]
    fn repeated_adjacent_kanji_pair_gets_auto_registered() {
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(dict_without_compound()));
        state.learning = common::LearningRepository::in_memory().ok();
        assert!(state.learning.is_some());

        let segments = vec![
            ("さい".to_string(), "再".to_string(), "接頭詞-名詞接続-*-*".to_string()),
            ("きどう".to_string(), "起動".to_string(), "名詞-サ変接続-*-*".to_string()),
        ];

        state.learn_from_segments(&segments);
        state.learn_from_segments(&segments);
        let not_yet = state
            .converter
            .as_ref()
            .unwrap()
            .dictionary
            .lookup("さいきどう")
            .is_none_or(|es| !es.iter().any(|e| e.surface == "再起動"));
        assert!(not_yet, "1〜2回目ではまだ自動登録されないはず");

        state.learn_from_segments(&segments);
        let conv = state.converter.as_ref().unwrap();
        assert!(
            conv.dictionary
                .lookup("さいきどう")
                .unwrap()
                .iter()
                .any(|e| e.surface == "再起動"),
            "3回目で辞書に自動登録されるはず"
        );

        let words = state.learning.as_ref().unwrap().get_all_user_words().unwrap();
        let auto = words
            .iter()
            .find(|w| w.reading == "さいきどう" && w.surface == "再起動")
            .expect("DBにも登録されているはず");
        assert_eq!(auto.source, "auto");
    }

    /// 助詞や単独ひらがなを挟むペアは自動登録しない
    #[test]
    fn particle_adjacent_pair_not_registered() {
        let mut dict = common::Dictionary::new();
        dict.add_word(common::WordEntry {
            surface: "検索".to_string(), reading: "けんさく".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-サ変接続-*-*".to_string(),
        });
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(dict));
        state.learning = common::LearningRepository::in_memory().ok();

        let segments = vec![
            ("けんさく".to_string(), "検索".to_string(), "名詞-サ変接続-*-*".to_string()),
            ("は".to_string(), "は".to_string(), "助詞-係助詞-*-*".to_string()),
        ];
        for _ in 0..5 {
            state.learn_from_segments(&segments);
        }
        let words = state.learning.as_ref().unwrap().get_all_user_words().unwrap();
        assert!(words.is_empty(), "助詞を挟むペアは自動登録されないはず");
    }
}

#[cfg(test)]
mod typo_learning_tests {
    use super::*;

    fn typo_test_dict() -> Dictionary {
        let mut dict = Dictionary::new();
        dict.add_word(common::WordEntry {
            surface: "今日".to_string(), reading: "きょう".to_string(),
            left_id: 1, right_id: 1, cost: 3000, pos: "名詞".to_string(),
        });
        dict.add_word(common::WordEntry {
            surface: "は".to_string(), reading: "は".to_string(),
            left_id: 2, right_id: 2, cost: 3000, pos: "助詞".to_string(),
        });
        dict
    }

    /// もしかしての先頭候補を確定すると誤字修正が学習され、次回同じ誤字を
    /// 打ったときに（辞書のあいまい検索をやり直さず）学習済み候補として
    /// 最優先で出てくること、さらに Delete でその学習だけをリセットできる
    /// ことを一連の流れで確認する。
    ///
    /// もしかしての初回検出手段（ローマ字取り残し repair 等）とは切り離して
    /// 「確定→学習→次回再利用→Deleteでリセット」のパイプラインだけを
    /// 検証するため、1回目の候補は実際の検出を通さず直接セットする
    /// （もしかしての一般的なあいまい検索はノイズ低減のため既に廃止済み。
    /// [[fuzzy-correction-approach]]参照。学習・再利用の仕組み自体は
    /// 検出手段が何であっても同じonce もしかしてとして出た候補に対して働く）。
    #[test]
    fn accepting_fuzzy_suggestion_learns_and_reapplies_next_time() {
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(typo_test_dict()));
        state.learning = common::LearningRepository::in_memory().ok();
        assert!(state.learning.is_some());

        // 1回目: 「きようは」（拗音の打ち間違い）に対して、もしかして候補
        // 「きょうは→今日は」が出ている状態を再現する。まだ学習していない
        // ので prediction_top_is_learned_typo は false。
        state.hiragana_buffer = "きようは".to_string();
        state.predictions = vec![("きょうは".to_string(), "今日は".to_string())];
        state.prediction_top_is_fuzzy = true;
        state.prediction_top_is_learned_typo = false;

        // その候補を確定する（＝ユーザーが修正を採用した）
        state.commit_prediction(0);

        // 学習DB・メモリ上の変換器の両方に誤字修正が記録されている
        let freq = state
            .learning
            .as_ref()
            .unwrap()
            .find_typo_correction_frequency("きようは", "きょうは", "今日は")
            .unwrap();
        assert_eq!(freq, 1);
        assert!(state
            .converter
            .as_ref()
            .unwrap()
            .typo_corrections
            .contains_key("きようは"));

        // 2回目: 同じ誤字「きようは」を打つと、あいまい検索をやり直さず
        // 学習済みの修正がそのまま最優先の候補として出る。
        state.hiragana_buffer = "きようは".to_string();
        state.update_predictions();
        assert!(state.prediction_top_is_fuzzy);
        assert!(state.prediction_top_is_learned_typo, "学習済み候補として出るべき");
        assert_eq!(state.predictions.first(), Some(&("きょうは".to_string(), "今日は".to_string())));

        // Delete で誤字修正の学習だけをリセットできる
        assert!(state.reset_prediction_learning());
        assert!(!state
            .converter
            .as_ref()
            .unwrap()
            .typo_corrections
            .contains_key("きようは"));
        let freq_after = state
            .learning
            .as_ref()
            .unwrap()
            .find_typo_correction_frequency("きようは", "きょうは", "今日は")
            .unwrap();
        assert_eq!(freq_after, 0);
    }
}

#[cfg(test)]
mod reading_validation_tests {
    use super::is_hiragana_reading;

    #[test]
    fn accepts_hiragana_and_long_vowel_mark() {
        assert!(is_hiragana_reading("ぷらいみんぐ"));
        assert!(is_hiragana_reading("そーと"));
    }

    #[test]
    fn rejects_kanji_reading() {
        // 実データで見つかった「読み欄に表記(漢字)を入れてしまった」誤登録
        // （例: 読み="死ぬ気" 表記="しぬき"）を弾けることを確認する。
        assert!(!is_hiragana_reading("死ぬ気"));
    }

    #[test]
    fn rejects_katakana_and_ascii() {
        assert!(!is_hiragana_reading("ソート"));
        assert!(!is_hiragana_reading("react"));
    }
}

