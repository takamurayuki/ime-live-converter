use crate::candidate::hiragana_to_katakana;
use crate::dictionary::{Dictionary, WordEntry, PosId};
use std::collections::HashMap;

mod fragment_repair;
mod incremental;
mod lattice;
mod nbest;
mod scoring;
mod stabilize;
#[cfg(test)]
mod tests;

pub use incremental::*;
pub use lattice::*;
pub(crate) use nbest::*;
pub use scoring::*;
pub use stabilize::*;

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
    /// `learned_bigram` に「前の表記」として登録されている表記の集合。
    /// `base_connection_cost` は辺ごとに呼ばれるため、(String, String) の
    /// キーを毎回作ってから引くと 5万辺規模の長文で無視できない
    /// アロケーションになる。まず &str でこの集合を引き、無ければ
    /// キーを作らずに済ませる。
    bigram_prev_surfaces: std::collections::HashSet<String>,
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
    /// 手動で精査済みの「絶対に勝たせたい」語: (読み, 表記) → コスト減額
    ///
    /// `learned_unigram`は`effective_word_cost`で `.max(0)` に floor される
    /// （ユーザーの実学習が暴走してコストを無限に負にし、無関係な文脈まで
    /// 壊すのを防ぐため）。しかし「もんだいない→問題ない」のように、
    /// base IPADic自体の連接コストの癖で断片解釈（揉ん+だ+以内）の合計
    /// コストが元から負になるケースは、対抗する語のコストを0にするだけ
    /// では勝てない。このマップは`seed_common_words`が明示的に登録する、
    /// 恒久的に正しいと分かっている語限定で、0未満まで割り引くことを許す
    /// （ユーザーの偶発的な誤学習では入らないため暴走の心配がない）。
    pub trusted_phrase_bonus: HashMap<(String, String), i32>,
    /// 学習した誤字修正: 誤字の読み → (修正後の読み, 修正後の表記, ボーナス)
    ///
    /// 「もしかして（誤字補正）」の先頭候補をユーザーが実際に選んだ（＝提示した
    /// 修正を採用した）ときに記録する。ラティスには関与せず、ライブ変換側が
    /// 打鍵中の読みで直接引く単純な完全一致辞書として使う。辞書のあいまい
    /// 検索やコスト計算をやり直さずに、ユーザー本人が過去に確認済みの修正を
    /// 即座に最優先で出せる（使うほど賢くなる／誤字パターンの個人最適化）。
    pub typo_corrections: HashMap<String, (String, String, i32)>,
    /// コーパス由来のユニグラム: (読み, 表記) → コスト減額
    ///
    /// `learned_unigram`がそのユーザー自身の変換履歴だけから作られるのに
    /// 対し、こちらは実際の日本語コーパス（`corpus_lm.dic`、
    /// `dict-builder corpus`で構築）から集計した語頻度に基づく。個人学習の
    /// リセット系操作（`clear_learning`・Delete誤学習リセット等）では
    /// 消えない別チャネルとして扱う。「よく使う語ほど無関係な文脈でも
    /// 学習ボーナスで勝ちやすくなる」ため個別の対抗語パッチが繰り返し
    /// 必要になっていた問題を、統計的な語頻度に置き換えることで
    /// 根本的に減らすためのもの。
    pub corpus_unigram: HashMap<(String, String), i32>,
    /// コーパス由来のバイグラム: (前の表記, 次の表記) → 接続コスト減額
    pub corpus_bigram: HashMap<(String, String), i32>,
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
            single_kanji_penalty: 400,
            learned_unigram: HashMap::new(),
            learned_bigram: HashMap::new(),
            bigram_prev_surfaces: std::collections::HashSet::new(),
            learned_assoc: HashMap::new(),
            learned_hiragana: HashMap::new(),
            trusted_phrase_bonus: HashMap::new(),
            typo_corrections: HashMap::new(),
            corpus_unigram: HashMap::new(),
            corpus_bigram: HashMap::new(),
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
        // 「揉ん+だ+以内」の分割(合計コストが極端に低い)に負ける。しかも
        // 「以内」は「3日以内」等の正当な用法で単独の学習ボーナスも
        // 受けやすく、分割側のコストは学習が無くても既に負（実測cost≈-3708、
        // 断片個別のユニグラム学習も乗ると≈-6708）になり得る。`learned_unigram`
        // の減額は`effective_word_cost`で0未満にならないよう floor されるため
        // （ユーザーの暴走学習を無関係な文脈まで波及させないためのガード）、
        // どれだけボーナスを盛っても「問題ない」のコストは0止まりで
        // 分割の負コストに勝てない。`trusted_phrase_bonus`はこの語に限り
        // 0未満まで割り引くための専用の仕組み（暴走の心配がない、
        // 手動で精査済みの固定エントリのみ入る）。
        self.trusted_phrase_bonus
            .insert(("もんだいない".to_string(), "問題ない".to_string()), 12000);
        // 「ごじ」は ご(５)+じ(時) の数字+助数詞で「５時」に割れる。誤字校正が
        // 主目的なので「誤字」を優先する（時間は文脈/学習で選び直せる）。
        self.learned_unigram
            .insert(("ごじ".to_string(), "誤字".to_string()), 4000);
        // 「空い」（空く の音便形）は「漉い・鋤い・梳い・抄い・透い」等の
        // 稀な同音動詞より辞書コストが高く負けやすいが、実際は
        // 「お腹/電車が空いている」のように圧倒的に高頻度な語なので優先する。
        self.learned_unigram
            .insert(("すい".to_string(), "空い".to_string()), 2500);
        // 「いちがい」も、「位置」（無関係な文脈で単語コストが学習ボーナスで
        // ほぼ0まで下がりやすい）+「が」+「胃」の分割（合計コストが正だが
        // 「一概」の生コストより低くなり得る）に負けやすい。「位置」は
        // 接頭詞ではなく通常の名詞のため、接頭詞向けの既存ガードが効かない。
        // 「一概」単独では負のコストにする必要はなく、通常のユニグラム
        // ボーナス（0未満にならない）で十分勝てるため trusted_phrase_bonus
        // は使わない。
        self.learned_unigram
            .insert(("いちがい".to_string(), "一概".to_string()), 6000);
        // 「すいよう」も、「酸い」（無関係な文脈で単語コストが学習ボーナスで
        // 下がりやすい）+「よう」の分割に負けやすい。「よう」はIPA辞書上
        // 同じ表記に9種類ものPOS（名詞-非自立-助動詞語幹・形容詞-非自立・
        // 助詞-終助詞・動詞-自立・感動詞 等）が重複登録されており、個別の
        // 接続コストガードでは1つ塞いでも別のPOSバリアント経由ですり抜けて
        // しまう（実測で複数バリアントを確認）。対抗語「水曜」自体を
        // 直接優先する方が確実で、他の語への副作用も無い。
        // 「揉んだ以内→問題ない」と同様、断片解釈の合計コストが学習無しでも
        // 既に負（実測cost≈-2517）のため、`learned_unigram`の0未満不可floor
        // では勝てず、0未満まで割り引ける`trusted_phrase_bonus`を使う。
        self.trusted_phrase_bonus
            .insert(("すいよう".to_string(), "水曜".to_string()), 10000);
        // 「きょうこ」は「今日」（頻出語のため無関係な文脈で学習ボーナスが
        // 上限6000まで乗りやすく、実質コスト0まで下がる）＋稀な単漢字「鼓」
        // （こ、cost=5435）の分割が、「強固」（cost=4685）はもちろん学習の
        // 乗っていない「京子」（cost=6792、姓としての接続コストが有利で
        // 学習なしでも「強固」に僅差で勝つ）にまで勝ってしまう
        // （実測: 今日鼓）。この手のケースは短い語のガード（読み2文字以下・
        // 文頭限定の接続floor）の対象外（「今日」は読み3文字の保護対象語で、
        // 「今日は/今日が」を壊さないためフロアを掛けられない）なので、
        // 「一概」「水曜」と同様に対抗語自体を直接優先する。断片解釈の
        // コストは学習が無くても正のため trusted_phrase_bonus は不要。
        self.learned_unigram
            .insert(("きょうこ".to_string(), "強固".to_string()), 3000);
        // 「おしえる」も、「押し」（頻出語のため無関係な文脈で学習ボーナスが
        // 上限6000まで乗りやすく、実質コスト0まで下がる）＋「える」の
        // 読みを持つ稀な固有名詞「エル」（人名、cost=4914）の分割が、
        // 「教える」（cost=6842）に勝ってしまう（実測: 押しエル）。
        // 「きょうこ」と同型（bonused_short_prev_conn_floor_at_utterance_start
        // の対象だが、curが実在の辞書語＝カタカナのフォールバックノードでは
        // ないため接続コストの底上げだけでは検出できない）なので、対抗語
        // 自体を直接優先する。
        self.learned_unigram
            .insert(("おしえる".to_string(), "教える".to_string()), 4000);
        // 「きょうかん」も、「今日」（頻出語のため学習ボーナスが乗りやすい）
        // ＋「感」（接尾的な名詞、cost低め）の分割が「共感」（cost=4402）に
        // 勝ってしまう（実測: 今日感）。同型のため対抗語を直接優先する。
        self.learned_unigram
            .insert(("きょうかん".to_string(), "共感".to_string()), 4000);
        // 「きょういく」も同型だが、「今日」＋「行く」は実際に「今日行く」
        // という自然な文としても使われるため「今日→行く」のバイグラムまで
        // 学習されやすく（実測+1500）、かつ元の接続コスト自体もサ変接続語
        // 一般の癖で負（実測-1881）なので、分割側の合計コストが学習無しでも
        // 既に負（実測cost≈-1881+マイナス方向）になり得る。「教育」
        // （cost=1448）を単なるユニグラムボーナス（0未満不可floor）で
        // 優先するだけでは勝てなかった（実測でboundary値6000超が必要、
        // 通常のUNIGRAM_BONUS_CAPと同水準では不足）ため、「水曜」
        // 「問題ない」と同様に0未満まで割り引ける trusted_phrase_bonus を使う。
        self.trusted_phrase_bonus
            .insert(("きょういく".to_string(), "教育".to_string()), 8000);

        // 既定でひらがな優先にする読み。漢字表記(慕い)が稀で、ひらがな
        // (助動詞「〜したい」)の方が圧倒的に多い。ユーザーが Esc で戻さ
        // なくても最初からひらがなで出るようにする。
        //（「たい」等の短い読みは「対象」などを壊すため入れない）
        self.learned_hiragana
            .entry("したい".to_string())
            .or_insert(COMMON_WORD_SEED_BONUS);
    }

    /// 優先語彙ファイル（`word_priority.tsv`）を読み込み、learned_unigram に
    /// シードボーナスとして投入する。
    ///
    /// `COMMON_WORD_SEED`（Rustのハードコード配列）と同じ仕組みだが、
    /// データファイル化することでコード変更・再コンパイルなしに辞書に
    /// 既存の同音語同士の優先順位を追加できるようにする。フォーマットは
    /// `読み\t表記\tボーナス(省略可)`。`#`始まりの行・空行は無視する。
    /// ユーザーの実学習値を優先するため `.entry().or_insert()` で書き込む
    /// （`seed_common_words` の既定シードと同じ規約）。
    pub fn load_word_priority_file(&mut self, path: &std::path::Path) -> std::io::Result<usize> {
        let content = std::fs::read_to_string(path)?;
        let mut count = 0;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split('\t');
            let (Some(reading), Some(surface)) = (parts.next(), parts.next()) else {
                continue;
            };
            let reading = reading.trim();
            let surface = surface.trim();
            if reading.is_empty() || surface.is_empty() {
                continue;
            }
            let bonus = parts
                .next()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .unwrap_or(COMMON_WORD_SEED_BONUS);
            self.learned_unigram
                .entry((reading.to_string(), surface.to_string()))
                .or_insert(bonus);
            count += 1;
        }
        Ok(count)
    }

    /// 学習データをすべてクリア（頻出語プリセットは残す）
    pub fn clear_learning(&mut self) {
        self.learned_unigram.clear();
        self.learned_bigram.clear();
        self.bigram_prev_surfaces.clear();
        self.learned_assoc.clear();
        self.learned_hiragana.clear();
        self.typo_corrections.clear();
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
        self.bigram_prev_surfaces = self.learned_bigram.keys().map(|(p, _)| p.clone()).collect();
        self.learned_assoc.retain(|(p, c), _| p != surface && c != surface);
    }

    /// 誤字修正（誤字の読み→修正後の読み・表記）を学習する。
    ///
    /// 「もしかして」の先頭候補をユーザーが実際に確定したときに呼ぶ。
    /// 使うほどボーナスが増える（`frequency_to_bonus`。ただし
    /// `UNIGRAM_BONUS_CAP`で頭打ち）が、これはラティスの単語コストではなく
    /// 単なる完全一致辞書のエントリなので、他の学習ボーナスのような
    /// 「無関係な文脈への波及」の心配は無い（打鍵中の読みが誤字とバイト単位で
    /// 完全一致したときだけ参照される）。
    pub fn learn_typo_correction(&mut self, wrong_reading: &str, correct_reading: &str, correct_surface: &str, freq: u32) {
        let bonus = frequency_to_bonus(freq).min(UNIGRAM_BONUS_CAP);
        self.typo_corrections.insert(
            wrong_reading.to_string(),
            (correct_reading.to_string(), correct_surface.to_string(), bonus),
        );
    }

    /// 学習した誤字修正を1件忘れる（候補一覧の Delete によるリセット用）。
    pub fn forget_typo_correction(&mut self, wrong_reading: &str) {
        self.typo_corrections.remove(wrong_reading);
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
        self.bigram_prev_surfaces.insert(prev_surface.to_string());
    }

    /// コーパス由来の語頻度データ（`corpus_lm.dic`）を読み込む
    ///
    /// `learned_unigram`/`learned_bigram`（個人の変換履歴）とは別チャネル
    /// （`corpus_unigram`/`corpus_bigram`）に格納する。何度呼んでも直前の
    /// 内容を置き換える（差し替えて再読み込みしても重複・蓄積しない）。
    pub fn load_corpus_lm(&mut self, lm: &crate::corpus_lm::CorpusLm) {
        self.corpus_unigram.clear();
        for (reading, surface, freq) in &lm.unigrams {
            let bonus = corpus_unigram_bonus(*freq);
            if bonus != 0 {
                self.corpus_unigram
                    .insert((reading.clone(), surface.clone()), bonus);
            }
        }
        self.corpus_bigram.clear();
        for (prev_surface, surface, freq) in &lm.bigrams {
            let bonus = corpus_bigram_bonus(*freq);
            if bonus != 0 {
                self.corpus_bigram
                    .insert((prev_surface.clone(), surface.clone()), bonus);
            }
        }
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
    fn rerank_by_assoc(&self, base: Vec<WordEntry>) -> Vec<WordEntry> {
        self.rerank_by_assoc_from(base, 0)
    }

    /// `rerank_by_assoc` の、先頭 `first_mutable` 文節を差し替え対象にしない版
    /// （固定した先頭文節は文脈として参照するだけで書き換えない。
    ///  `convert_context_aware_pinned` から使う）。
    pub(crate) fn rerank_by_assoc_from(&self, mut base: Vec<WordEntry>, first_mutable: usize) -> Vec<WordEntry> {
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
            if i < first_mutable {
                continue;
            }
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
                // 文中の他の内容語との連想スコア（前後両方向）。
                // ただし直前・直後（間に助詞すら挟まない隣接語）は対象外。
                // 連想学習は本来「駅の汽車／新聞の記者」のように助詞を挟んで
                // 離れた内容語どうしの結びつきを見るための仕組みで、隣接する
                // 語どうしの相性は通常の接続コストが既に見ている。ここで
                // 隣接語まで対象にすると、「際」×「起動」のように別の文脈で
                // 強く学習された連想が、隣接して「際起動」のような複合語に
                // 誤読される形で1-bestを上書きしてしまうことがある。
                let mut assoc = 0i32;
                for (j, w) in &content {
                    if *j == i || j.abs_diff(i) == 1 {
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
    ///
    /// 候補が複数（同じ編集距離）ある場合は、前後の確定済み文節との
    /// 学習済みバイグラム／内容語連想（`learned_bigram`/`learned_assoc`）で
    /// 文脈に合う方を選ぶ。誤字の直後に続けて正しく入力された内容（右文脈）
    /// も使えるため、「打ち直し」なしに後から誤字だけ直る場合がある。
    pub fn fuzzy_suggest(&self, reading: &str) -> Option<(String, String)> {
        let total_n = reading.chars().count();
        if total_n < 2 || total_n > 32 {
            return None;
        }
        let segments = self.convert_context_aware(reading);
        self.fuzzy_suggest_from_segments(&segments, reading)
    }

    /// `fuzzy_suggest` の本体。呼び出し側が表示用などで直前に既に
    /// `convert_context_aware(reading)` 済みの segments を持っている場合は
    /// こちらを直接呼ぶと、同じ読みへの Viterbi 変換の重複を避けられる
    /// （毎打鍵呼ばれるためこの重複コストは無視できない）。
    pub fn fuzzy_suggest_from_segments(
        &self,
        segments: &[WordEntry],
        reading: &str,
    ) -> Option<(String, String)> {
        let total_n = reading.chars().count();
        if total_n < 2 || total_n > 32 {
            return None;
        }

        // 変換に失敗した文節（未知語 / カタカナ・フォールバック）に加え、
        // 辞書上は成功していても1文字あたりのコストが極端に高い希少な
        // 内容語（`is_implausible_content_word`）も対象に含めて連続でまとめ、
        // グループごとに (開始文節index, 終了index排他, 読み) を得る。
        // 断片それぞれは辞書に実在するため`is_failed_segment`だけでは
        // 拾えない、全体として意味を成さない誤変換（例:「せんせに」→
        // 「線」+「セ」+「に」）を補正候補の探索対象にするため。
        let is_target = |e: &WordEntry| is_failed_segment(e) || is_implausible_content_word(e);
        let mut groups: Vec<(usize, usize, String)> = Vec::new();
        let mut i = 0;
        while i < segments.len() {
            if is_target(&segments[i]) {
                let start = i;
                let mut r = String::new();
                while i < segments.len() && is_target(&segments[i]) {
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

        // 前後の確定済み文節（左文脈・右文脈）を文脈ボーナスの手がかりにする。
        let prev_surface = (gs > 0).then(|| segments[gs - 1].surface.as_str());
        let next_surface = (ge < segments.len()).then(|| segments[ge].surface.as_str());
        let context_words: Vec<String> = segments
            .iter()
            .enumerate()
            .filter(|(idx, e)| !(gs..ge).contains(idx) && is_content_pos(&e.pos) && e.surface != e.reading)
            .map(|(_, e)| e.surface.clone())
            .collect();

        let corrected =
            self.best_word_correction(&target, prev_surface, next_surface, &context_words)?;

        // 全体の読みを組み立て直し（対象グループだけ差し替え）
        let mut new_reading = String::new();
        for (idx, seg) in segments.iter().enumerate() {
            if idx < gs || idx >= ge {
                new_reading.push_str(&seg.reading);
            } else if idx == gs {
                new_reading.push_str(&corrected);
            }
        }

        let base_surface: String = segments.iter().map(|e| e.surface.as_str()).collect();
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
    /// さらに `is_plausible_correction_cost` で1文字あたりコストが妥当な
    /// 範囲かも見る（実在はするが希少な当て字・造語まで通さないため）。
    /// 編集距離が最小、次に前後の文脈ボーナスを引いた実効コストが最小のものを選ぶ。
    ///
    /// 性能: 毎打鍵でフックスレッドから呼ばれるため軽さが最優先。Trie検索は
    /// 距離1のみ（距離2は辞書全体で 100ms超になりフックが無視され生キーが漏れる）。
    /// 文脈ボーナスも候補ごとに文全体を再変換せず、学習済みマップの参照のみ
    /// （O(1)）で済ませ、この制約に影響しないようにしている。
    fn best_word_correction(
        &self,
        word: &str,
        prev_surface: Option<&str>,
        next_surface: Option<&str>,
        context_words: &[String],
    ) -> Option<String> {
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
        // 候補ごとに文字列全体を再変換（build_lattice+find_best_path）するため、
        // 候補数がそのまま毎打鍵のコストに直結する。短い読みほど辞書Trieの
        // 距離1あいまい検索がヒットしやすく、2文字の読みで178件に達すること
        // も実測で確認済み。全件評価すると単純な誤字補正1回で数十ms〜100ms超
        // かかりフック遅延（生キー漏れ）を招くため、上限で打ち切る。
        const MAX_CORRECTION_CANDIDATES: usize = 30;
        cands.truncate(MAX_CORRECTION_CANDIDATES);

        // 距離が小さいほど、次に実効コスト（変換コスト − 文脈ボーナス）が
        // 低いほど良い。
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
            let bonus = self.context_bonus(&path, prev_surface, next_surface, context_words);
            let net_cost = cost.saturating_sub(bonus);
            // 実在チェックだけでは希少な当て字・造語まで通ってしまうため、
            // 1文字あたりコストの妥当性ゲートも課す（is_plausible_correction_cost）。
            if !is_plausible_correction_cost(net_cost, v.chars().count()) {
                continue;
            }
            let better = match &best {
                None => true,
                Some((_, bd, bc)) => dist < *bd || (dist == *bd && net_cost < *bc),
            };
            if better {
                best = Some((v, dist, net_cost));
            }
        }
        best.map(|(v, _, _)| v)
    }

    /// 誤字補正候補が前後の文脈（左＝直前の確定文節、右＝直後に続けて
    /// 入力済みの文節、その他の内容語）とどれだけ馴染むかをボーナスで返す。
    /// 学習済み `learned_bigram`/`learned_assoc` の参照のみで計算する
    /// （文全体の再変換はしない軽量な近似）。
    fn context_bonus(
        &self,
        path: &[WordEntry],
        prev_surface: Option<&str>,
        next_surface: Option<&str>,
        context_words: &[String],
    ) -> i32 {
        let mut bonus = 0i32;
        if let (Some(prev), Some(first)) = (prev_surface, path.first()) {
            bonus += self
                .learned_bigram
                .get(&(prev.to_string(), first.surface.clone()))
                .copied()
                .unwrap_or(0);
        }
        if let (Some(next), Some(last)) = (next_surface, path.last()) {
            bonus += self
                .learned_bigram
                .get(&(last.surface.clone(), next.to_string()))
                .copied()
                .unwrap_or(0);
        }
        for e in path.iter().filter(|e| is_content_pos(&e.pos) && e.surface != e.reading) {
            for w in context_words {
                bonus += self
                    .learned_assoc
                    .get(&(w.clone(), e.surface.clone()))
                    .copied()
                    .unwrap_or(0);
                bonus += self
                    .learned_assoc
                    .get(&(e.surface.clone(), w.clone()))
                    .copied()
                    .unwrap_or(0);
            }
        }
        bonus
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

    /// 単語の実効コスト（1文字漢字ペナルティ・学習ユニグラム・コーパス頻度を反映）。
    /// `build_lattice`が各ラティスノードに適用するのと同じ計算式。候補一覧
    /// （`hook-dll`の`build_candidates`）からも参照するため`pub(crate)`にしている。
    pub fn effective_word_cost(&self, e: &WordEntry) -> i32 {
        let pen = single_kanji_penalty(&e.surface, self.single_kanji_penalty);
        let key = (e.reading.clone(), e.surface.clone());
        let bonus = self.learned_unigram.get(&key).copied().unwrap_or(0);
        let trusted_bonus = self.trusted_phrase_bonus.get(&key).copied().unwrap_or(0);
        let corpus_bonus = self.corpus_unigram.get(&key).copied().unwrap_or(0);
        let floor = if trusted_bonus != 0 { -20000 } else { 0 };
        (e.cost as i32)
            .saturating_add(pen)
            .saturating_sub(bonus)
            .saturating_sub(trusted_bonus)
            .saturating_sub(corpus_bonus)
            .max(floor)
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
        // 「1文字漢字＋後続の漢字語」に割れた断片を辞書語に置換する
        // （`fragment_repair.rs`。総コストはラティス上の最適パスのまま）
        let path = self.repair_single_kanji_fragments(self.extract_result(&lattice));
        (path, cost)
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
                            + proper_noun_penalty(&entry.surface, &entry.pos)
                            + symbol_penalty(&entry.surface, &entry.pos);
                        // 学習したユニグラムはコストを減額（優先度を上げる）
                        let key = (entry.reading.clone(), entry.surface.clone());
                        let bonus = self.learned_unigram.get(&key).copied().unwrap_or(0);
                        let trusted_bonus = self.trusted_phrase_bonus.get(&key).copied().unwrap_or(0);
                        // コーパス由来のユニグラム頻度も同じ「使うほど優先」の
                        // 発想の減額だが、個人の学習履歴とは別チャネル（詳細は
                        // `corpus_unigram`フィールドのコメント）。
                        let corpus_bonus = self.corpus_unigram.get(&key).copied().unwrap_or(0);
                        if penalty != 0 || bonus != 0 || trusted_bonus != 0 || corpus_bonus != 0 {
                            // ボーナスでコストを負にはしない。負コストは「使うほど得」に
                            // なってしまい、無関係な文脈で学習した短い語が、その語とは
                            // 関係ない後続語のコストまで一方的に相殺できてしまう
                            // （例: 「効果」を単独でよく使い学習しても、その学習が
                            //  「こうかい」→「効果位」のような無関係な誤分割を後押しして
                            //  しまってはならない）。ただし`trusted_phrase_bonus`は
                            // ユーザーの暴走学習が入り得ない、手動精査済みの固定
                            // エントリだけなので、この語に限り0未満まで許可する。
                            let floor = if trusted_bonus != 0 { -20000 } else { 0 };
                            lattice.nodes[idx].word_cost = lattice.nodes[idx]
                                .word_cost
                                .saturating_add(penalty)
                                .saturating_sub(bonus)
                                .saturating_sub(trusted_bonus)
                                .saturating_sub(corpus_bonus)
                                .max(floor);
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

        // 各ノードの学習ユニグラムボーナスをここで1回だけ引いておく
        // （`find_best_path` のガード群が辺ごとに引き直さないため）
        if !self.learned_unigram.is_empty() {
            for node in lattice.nodes.iter_mut() {
                if let Some(e) = &node.entry {
                    node.learned_bonus = self
                        .learned_unigram
                        .get(&(e.reading.clone(), e.surface.clone()))
                        .copied()
                        .unwrap_or(0);
                }
            }
        }

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
                // 同じ開始位置により長い辞書語（実在の複合語）があるなら、
                // このひらがな優先ノードは足さない。例:「さい」をEscで
                // ひらがなに戻した学習があっても、同じ「さい」で始まる
                // 「再起動」のような正当な複合語まで、その先頭2文字だけを
                // 理由に断片化してしまうのは意図と異なる（この学習は
                // 「さい」単体・末尾としての出現にだけ適用したい）。
                let breaks_longer_word = lattice.nodes_starting_at[start].iter().any(|&idx| {
                    let n = &lattice.nodes[idx];
                    n.entry.is_some() && n.end > end
                });
                if breaks_longer_word {
                    from = start + reading.chars().next().unwrap().len_utf8();
                    continue;
                }
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

    /// 2語間の接続コスト（連接行列 － 学習/コーパスのバイグラムボーナス）。
    /// `find_best_path`のホットパスと、候補一覧（`hook-dll`の
    /// `build_candidates`）の両方から使う共通部分。`scoring.rs`のガード
    /// 関数群（暴走学習対策）はここには含まない（`find_best_path`側で別途
    /// 適用する。候補一覧側は意図的に簡易版のまま、詳細は呼び出し元参照）。
    /// `prev_surface`が`None`ならBOS相当（バイグラムボーナスの対象外）。
    /// 戻り値: (接続コスト, 学習/コーパスのバイグラムボーナス合計)
    pub fn base_connection_cost(
        &self,
        prev_right_id: PosId,
        prev_surface: Option<&str>,
        cur_left_id: PosId,
        cur_surface: &str,
    ) -> (i32, i32) {
        let mut conn_cost = self.dictionary.matrix.get(prev_right_id, cur_left_id) as i32;
        let mut bigram_bonus = 0i32;
        if let Some(prev_surface) = prev_surface {
            if !self.learned_bigram.is_empty() && self.bigram_prev_surfaces.contains(prev_surface) {
                if let Some(bonus) = self
                    .learned_bigram
                    .get(&(prev_surface.to_string(), cur_surface.to_string()))
                {
                    bigram_bonus = *bonus;
                    conn_cost = conn_cost.saturating_sub(bigram_bonus);
                }
            }
            if !self.corpus_bigram.is_empty() {
                if let Some(bonus) = self
                    .corpus_bigram
                    .get(&(prev_surface.to_string(), cur_surface.to_string()))
                {
                    conn_cost = conn_cost.saturating_sub(*bonus);
                }
            }
        }
        (conn_cost, bigram_bonus)
    }

    /// 候補語 `cur` を `prev`/`next` の間に置いたときの前後接続コストの合計。
    /// `prev`/`next`が`None`ならBOS/EOSとして扱う。`base_connection_cost`の
    /// 薄いラッパーで、`scoring.rs`のガード関数群は含まない簡易版
    /// （候補一覧の並び替え専用。文全体の厳密な1-bestは`find_best_path`）。
    pub fn context_connection_cost(
        &self,
        prev: Option<&WordEntry>,
        cur: &WordEntry,
        next: Option<&WordEntry>,
    ) -> i32 {
        let prev_right_id = prev.map(|e| e.right_id).unwrap_or(self.dictionary.bos_id);
        let prev_surface = prev.map(|e| e.surface.as_str());
        let (before, _) =
            self.base_connection_cost(prev_right_id, prev_surface, cur.left_id, &cur.surface);

        let next_left_id = next.map(|e| e.left_id).unwrap_or(self.dictionary.eos_id);
        let next_surface = next.map(|e| e.surface.as_str()).unwrap_or("");
        let (after, _) =
            self.base_connection_cost(cur.right_id, Some(&cur.surface), next_left_id, next_surface);

        before.saturating_add(after)
    }

    /// Viterbiアルゴリズムで最適パスを探索（pub for IncrementalViterbi）
    pub fn find_best_path(&self, lattice: &mut Lattice) {
        let input_len = lattice.input.len();

        // 各位置を左から右に処理
        for pos in 0..=input_len {
            // この位置で始まるノードを処理
            let starting_indices: Vec<usize> = lattice.nodes_starting_at[pos].clone();
            
            // この位置で終わるノード（開始ノードごとにクローンし直さない）
            let ending_indices: Vec<usize> = lattice.nodes_ending_at[pos].clone();

            for &node_idx in &starting_indices {
                
                let mut best_cost = i32::MAX;
                let mut best_prev: Option<usize> = None;

                for &prev_idx in &ending_indices {
                    let prev_node = &lattice.nodes[prev_idx];
                    if prev_node.total_cost == i32::MAX {
                        continue;
                    }

                    let current_node = &lattice.nodes[node_idx];

                    // 連接コスト（辞書の連接行列 － 学習/コーパスのバイグラム
                    // ボーナス）。候補一覧の`context_connection_cost`とも
                    // 共有する共通部分。
                    let (mut conn_cost, bigram_bonus) = self.base_connection_cost(
                        prev_node.right_id,
                        prev_node.entry.as_ref().map(|e| e.surface.as_str()),
                        current_node.left_id,
                        current_node
                            .entry
                            .as_ref()
                            .map(|e| e.surface.as_str())
                            .unwrap_or(""),
                    );

                    if let (Some(pe), Some(ce)) = (&prev_node.entry, &current_node.entry) {
                        // カタカナ語+単独の接尾辞漢字という不自然な組み合わせは
                        // 接続コストを上乗せする（例外は語・人・製 等の外来語に
                        // 実際に付く接尾辞のみ）
                        conn_cost = conn_cost
                            .saturating_add(katakana_kanji_suffix_penalty(pe, ce));
                        // 形容詞の終止形（〜い）に「て」が直接続くのは文法的に
                        // 常に誤り（正しくは連用形〜くて）なので無条件でペナルティ
                        conn_cost = adjective_terminal_then_te_penalty(pe, ce, conn_cost);
                        // 1文字漢字の表記かつ読みが単独助詞と一致する語（野・葉 等）
                        // が文頭以外に出現するのは不自然なので無条件でペナルティ
                        conn_cost = conn_cost
                            .saturating_add(single_kanji_lone_particle_reading_penalty(ce));
                    }
                    // 1文字漢字が絡む不自然な接続コスト（学習バイグラム込み）には
                    // 下限を設ける。ただし学習ボーナスでどちらかの単語コストが
                    // 不自然に下がっている場合のみ（「英語」等、IPA辞書自体が
                    // 正当に認識している複合語まで壊さないため）。文末（EOS）
                    // への接続も対象に含めるため、どちらかが BOS/EOS
                    // （entry なし）でも呼び出す。
                    // 文末で、助詞と紛らわしい1文字の読み（に等）が助詞
                    // ではなく内容語（二・荷 等）に化けている場合にも
                    // ペナルティを科す（例:「わたしはに」→「私は二」で
                    // 助詞「に」が押しのけられるのを防ぐ）。学習の有無に
                    // 関わらず常に対象なので、下のボーナス系ガードとは別枠。
                    conn_cost = conn_cost.saturating_add(
                        content_word_particle_reading_before_eos_penalty(
                            prev_node.entry.as_ref(),
                            node_idx == lattice.eos_index,
                        ),
                    );

                    // 以下はすべて「学習ボーナスでどちらかの単語コストが不自然に
                    // 下がっている場合だけ」介入するガード群。bonus_of は
                    // (読み,表記) の String クローン＋HashMapルックアップを伴うため、
                    // 各ガードごとに呼ぶと1辺あたり最大10回にもなり、もしかして
                    // 補正（1候補ごとに文字列全体を再変換）のような大量呼び出しで
                    // 無視できないコストになる（実測: 短い候補文字列でも1回の
                    // 変換に数ms〜10ms超）。ここで1辺につき最大3回（前・現在・
                    // 祖先）だけ計算し、全て0ならガード群を丸ごとスキップする。
                    // （現在は `build_lattice` がノードごとに1回だけ引いて
                    //  `learned_bonus` に持たせているので、ここでは参照するだけ）
                    let prev_bonus = prev_node.learned_bonus;
                    let cur_bonus = current_node.learned_bonus;
                    let grandparent_node = prev_node.prev_node.map(|gp_idx| &lattice.nodes[gp_idx]);
                    let grandparent_entry = grandparent_node.and_then(|n| n.entry.as_ref());
                    let grandparent_bonus = grandparent_node.map_or(0, |n| n.learned_bonus);
                    let prev_is_utterance_start = prev_node.prev_node == Some(lattice.bos_index);
                    if prev_bonus != 0 || cur_bonus != 0 || grandparent_bonus != 0 || bigram_bonus != 0 {
                        conn_cost = clamp_single_kanji_pair_conn_cost(
                            prev_node.entry.as_ref(),
                            current_node.entry.as_ref(),
                            prev_bonus,
                            cur_bonus,
                            conn_cost,
                        );
                        // 「1文字漢字＋1文字漢字の非自立名詞」で文が終わる場合にも
                        // 下限を設ける（祖先ノードも1文字漢字の場合のみ。祖先ノードは
                        // 既に確定済みなので参照できる）。
                        conn_cost = single_kanji_bound_noun_phrase_end_conn_cost(
                            prev_node.entry.as_ref(),
                            grandparent_entry,
                            prev_bonus,
                            grandparent_bonus,
                            conn_cost,
                        );
                        // 学習ボーナスの乗った1文字漢字の非自立名詞が、助詞等を
                        // 挟まず直接別の内容語に続く場合にも下限を設ける
                        // （例: 「際」+「起動」で「再起動」が押しのけられるのを防ぐ）。
                        conn_cost = single_kanji_bound_noun_then_content_word_conn_cost(
                            prev_node.entry.as_ref(),
                            current_node.entry.as_ref(),
                            prev_bonus,
                            conn_cost,
                        );
                        // 学習ボーナスの乗った1文字漢字の形容詞語幹（「多い」でなく
                        // 「多」単体 等）が、助詞を挟まず直接別の内容語に続く場合にも
                        // 下限を設ける（例:「おお」+「さか」で「大阪」が
                        // 押しのけられるのを防ぐ）。
                        conn_cost = bonused_adjective_stem_then_content_word_conn_cost(
                            prev_node.entry.as_ref(),
                            prev_bonus,
                            conn_cost,
                        );
                        // フィラーの直後に学習ボーナスの乗った語が続く場合にも
                        // 下限を設ける
                        conn_cost = filler_then_bonused_word_conn_cost(
                            prev_node.entry.as_ref(),
                            cur_bonus,
                            conn_cost,
                        );
                        // 学習（ユニグラム・バイグラム）が乗った活用語が、極端に
                        // 安い活用接続（形容詞連用形+ない 等）を通じて無関係な
                        // 同音異義語を押しのける場合にも下限を設ける
                        // （例:「酸く」+「ない」で「少ない」が押しのけられるのを防ぐ）。
                        conn_cost = bonused_adjective_inflection_conn_floor(
                            prev_node.entry.as_ref(),
                            current_node.entry.as_ref(),
                            prev_bonus,
                            bigram_bonus,
                            conn_cost,
                        );
                        // 文頭の、読みが2文字以下の短い語（接頭詞・副詞・
                        // 短い名詞/動詞・形容詞語幹 等、POSを問わない）の
                        // 直後への接続に学習ボーナスが絡んでいる場合の
                        // 下限を設ける（例:「お」+「中」で「お腹」、
                        // 「どう」+「か」で「同化」、「位置」+「が」で
                        // 「一概」が押しのけられるのを防ぐ。個別のPOSに
                        // 限定した複数のガードを一般化して統合したもの）。
                        conn_cost = bonused_short_prev_conn_floor_at_utterance_start(
                            prev_node.entry.as_ref(),
                            current_node.entry.as_ref(),
                            prev_is_utterance_start,
                            prev_bonus,
                            cur_bonus,
                            bigram_bonus,
                            conn_cost,
                        );
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
