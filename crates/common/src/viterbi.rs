use crate::candidate::hiragana_to_katakana;
use crate::dictionary::{Dictionary, WordEntry, PosId};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

mod fragment_repair;
mod judge_select;
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
pub use judge_select::*;

/// Viterbi変換エンジン
#[derive(Debug)]
pub struct ViterbiConverter {
    /// 基底辞書（IPA辞書等、巨大で不変）。`Arc`で共有し、インスタンスごとに
    /// 深いコピーをしない（[[dictionary-arc-sharing]]参照。以前は
    /// `ViterbiConverter::new`のたびに数百MB規模のTrieを丸ごとクローンして
    /// おり、ゴールデンテストのようにケースごとに新規インスタンスを作る
    /// 用途で1回あたり300〜400msかかっていた）。
    pub dictionary: Arc<Dictionary>,
    /// 上乗せ辞書（ユーザー辞書・自動登録複合語・`seed_common_words`由来の
    /// 追加語）。基底辞書と違って小さく、インスタンスごとに独立して持つ
    /// 必要があるため`Arc`化しない。`add_word`/`remove_word`は必ずこちらに
    /// 対して行う（基底は不変のため書き込めない）。検索時は
    /// `merged_lookup`/`merged_prefix_search`/`merged_fuzzy_readings`が
    /// 両方を引いて合成する。
    pub overlay: Dictionary,
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
    /// `corpus_bigram` 版の `bigram_prev_surfaces`（`load_corpus_lm`が
    /// 丸ごと差し替える専用フィールドなので、`learned_bigram`側のように
    /// 個別の学習操作で再構築する必要が無く別枠にしている）。
    corpus_bigram_prev_surfaces: std::collections::HashSet<String>,
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
    /// 隣接コロケーションのシード: (語, 隣接語) → ボーナス（両方向で登録）
    ///
    /// `learned_assoc`は「駅の汽車／新聞の記者」のように助詞を挟んで離れた
    /// 内容語どうしの結びつきを見る設計で、直前・直後（助詞すら挟まない
    /// 隣接語）は明示的に除外している（[[rerank_by_assoc_from]]のコメント
    /// 参照。ユーザーの実学習ノイズが隣接語まで波及すると「際」×「起動」の
    /// ような誤った複合語読みを誘発するため）。
    ///
    /// しかし「経済＋政策」「かたい＋仕事」「はやい＋くるま」のような
    /// 複合名詞・形容詞＋名詞の同音異義語選択は、まさにその除外された
    /// 隣接構造でしか起きない。`learned_assoc`とは別枠にすることで、
    /// 学習側の安全設計（隣接語除外）に一切手を触れずに、事前に精査した
    /// 少数のコロケーションだけを隣接語にも適用できる
    /// （[[homophone-selection-is-not-fixable-without-lm]]参照）。
    pub seeded_assoc: HashMap<(String, String), i32>,
    /// 判断層（judge-lm、`crate::judge`）。`Some`のときだけ自動変換の最終候補を
    /// N-best＋統計言語モデルの確率で選び直す。`None`なら従来の挙動と完全に同じ
    /// （[[jev-style-judge-direction]]、`set_judge`で切り替える）。
    pub judge: Option<Arc<crate::judge::JudgeLm>>,
}

impl ViterbiConverter {
    /// 辞書を所有権ごと受け取り、内部で`Arc`に包む。呼び出し側で`Arc`を
    /// 共有したい場合（同じ基底辞書でインスタンスを何度も作る、
    /// [[golden-test-harness]]のようなケース）は`from_shared`を使うこと。
    pub fn new(dictionary: Dictionary) -> Self {
        Self::from_shared(Arc::new(dictionary))
    }

    /// 既に`Arc`化された基底辞書を共有してインスタンスを作る。基底辞書は
    /// 一切クローンしないため、同じ`Arc<Dictionary>`から何個作っても
    /// `Dictionary::clone()`のコスト（実測300〜400ms/回、Trie全体の
    /// 深いコピー）は1回も発生しない。上乗せ辞書（`overlay`）は各
    /// インスタンスが独立して持つ（ユーザー辞書等はインスタンスごとに
    /// 違いうるため）。
    pub fn from_shared(dictionary: Arc<Dictionary>) -> Self {
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
            overlay: Dictionary::new(),
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
            corpus_bigram_prev_surfaces: std::collections::HashSet::new(),
            learned_assoc: HashMap::new(),
            learned_hiragana: HashMap::new(),
            trusted_phrase_bonus: HashMap::new(),
            typo_corrections: HashMap::new(),
            corpus_unigram: HashMap::new(),
            corpus_bigram: HashMap::new(),
            seeded_assoc: HashMap::new(),
            judge: None,
        };
        conv.seed_common_words();
        conv
    }

    /// 頻出語プリセットを learned_unigram に薄く入れる（初回変換の品質向上）
    /// 読みで単語を検索する（基底辞書＋上乗せ辞書の合成）。
    ///
    /// 上乗せ辞書にこの読みのエントリが無ければ、基底辞書の借用をそのまま
    /// 返す（`Cow::Borrowed`、クローン無し＝これまでの`self.dictionary.lookup`
    /// と全く同じコスト）。上乗せ辞書にエントリがある場合（ユーザー辞書・
    /// 自動登録複合語・`seed_common_words`由来の追加語）だけ、両方を
    /// 連結した新しい`Vec`を作る（`Cow::Owned`）。この分岐により、
    /// 上乗せ辞書が空（大多数のケース）ではホットパスの性能が変わらない。
    pub fn merged_lookup(&self, reading: &str) -> Option<Cow<'_, [WordEntry]>> {
        let overlay_hit = self.overlay.lookup(reading);
        match overlay_hit {
            None => self
                .dictionary
                .lookup(reading)
                .map(|v| Cow::Borrowed(v.as_slice())),
            Some(overlay_entries) => {
                let mut merged: Vec<WordEntry> = self
                    .dictionary
                    .lookup(reading)
                    .map(|v| v.clone())
                    .unwrap_or_default();
                merged.extend(overlay_entries.iter().cloned());
                Some(Cow::Owned(merged))
            }
        }
    }

    /// プレフィックス検索（基底辞書＋上乗せ辞書の合成）。
    ///
    /// `merged_lookup`と同じ考え方: 上乗せ辞書がそもそも空（`is_empty`、
    /// ユーザー辞書登録も`seed_common_words`の追加も無い状態）なら基底の
    /// 結果をそのまま借用で返す（`build_lattice`のホットパスで1文字ごとに
    /// 呼ばれるため、ここの分岐が最も重要）。上乗せ辞書に何かあっても
    /// この読み出しに一致が無ければ同様に借用のまま返す。両方に一致が
    /// あるプレフィックス長だけ、その長さの分だけ結合した`Vec`を作る。
    pub fn merged_prefix_search(&self, text: &str) -> Vec<(usize, Cow<'_, [WordEntry]>)> {
        let base_hits = self.dictionary.common_prefix_search(text);
        if self.overlay.is_empty() {
            return base_hits
                .into_iter()
                .map(|(len, v)| (len, Cow::Borrowed(v.as_slice())))
                .collect();
        }
        let overlay_hits = self.overlay.common_prefix_search(text);
        if overlay_hits.is_empty() {
            return base_hits
                .into_iter()
                .map(|(len, v)| (len, Cow::Borrowed(v.as_slice())))
                .collect();
        }
        let mut by_len: std::collections::BTreeMap<usize, Vec<WordEntry>> =
            std::collections::BTreeMap::new();
        for (len, v) in &base_hits {
            by_len.entry(*len).or_default().extend(v.iter().cloned());
        }
        for (len, v) in &overlay_hits {
            by_len.entry(*len).or_default().extend(v.iter().cloned());
        }
        by_len
            .into_iter()
            .map(|(len, v)| (len, Cow::Owned(v)))
            .collect()
    }

    /// 編集距離によるあいまい読み検索（基底辞書＋上乗せ辞書の合成）。
    pub fn merged_fuzzy_readings(&self, target: &str, max_dist: usize) -> Vec<(String, usize)> {
        let mut results = self.dictionary.fuzzy_readings(target, max_dist);
        if !self.overlay.is_empty() {
            results.extend(self.overlay.fuzzy_readings(target, max_dist));
        }
        results
    }

    fn seed_common_words(&mut self) {
        // IPA/追加辞書に欠ける定型語。地名「コ・コン」のコストを全体で
        // 変えるのではなく「古今東西」の読み全体に一致する候補を補う。
        // 既存判定は基底＋上乗せ両方を見る（`merged_lookup`）。上乗せだけ
        // 見ると`clear_learning`経由でこの関数が再度呼ばれたときに
        // 二重登録してしまう（基底＝Arc共有・不変は変化しないため、
        // 上乗せに前回追加済みでも基底だけ見ると「無い」と誤判定する）。
        if !self.merged_lookup("ここんとうざい")
            .is_some_and(|entries| entries.iter().any(|e| e.surface == "古今東西"))
        {
            self.overlay.add_word(WordEntry {
                reading: "ここんとうざい".into(), surface: "古今東西".into(),
                left_id: 1285, right_id: 1285, cost: 3000,
                pos: "名詞-一般-*-*".into(),
            });
        }
        // 「メモ帳」も辞書に複合語として無く、「帳」が接尾辞（付属語向けの
        // 高いコスト）でしか登録されていないため、単独名詞の「チョウ」に
        // 負けて「めも帳/眼も寵」のように割れる。読み全体に一致する複合語
        // を直接補う。
        if !self.merged_lookup("めもちょう")
            .is_some_and(|entries| entries.iter().any(|e| e.surface == "メモ帳"))
        {
            self.overlay.add_word(WordEntry {
                reading: "めもちょう".into(), surface: "メモ帳".into(),
                left_id: 1285, right_id: 1285, cost: 3000,
                pos: "名詞-一般-*-*".into(),
            });
        }
        // 謝罪の定型表現を「ゴメン＋な＋際」に分割しない。
        // 分割語の個人学習で接続込みコストが負になるため通常の0下限では不足する。
        self.trusted_phrase_bonus
            .insert(("ごめんなさい".to_string(), "ごめんなさい".to_string()), 12000);
        self.trusted_phrase_bonus
            .insert(("だれ".to_string(), "誰".to_string()), 5000);
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
        // 「車」はIPA辞書で単独名詞としてのコストが高く(6918)、同じ読みの
        // カタカナ表記「クルマ」(3630、コスト差3288)に負ける。COMMON_WORD_SEED
        // の一律1500では足りないため個別に強めのボーナスを設定する
        // （はやいくるまがはしる→速い車が走る、で発覚。[[correction-cascade-unification]]
        // の精度課題調査の一環）。
        self.learned_unigram
            .insert(("くるま".to_string(), "車".to_string()), 4000);
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
        // 「本」「線」は通常のプリセットボーナス(1500)では、辞書のカタカナ
        // 表記「ホン」「セン」（固有名詞-人名-姓としても実在）や1文字漢字
        // への断片化に負ける（実測: 本を読んだ→ホンを読んだ、線を弾いた→
        // センを弾いた）。カタカナ表記全般へのペナルティは「めも→眼も」の
        // ような別の実在語まで壊す（コスト差だけでは正当な外来語と区別
        // できない）ため見送り、代わりにこの2語だけ強めに優先する。
        self.learned_unigram.insert(("ほん".to_string(), "本".to_string()), 3000);
        // 「線」は「本」と同様の断片化(せ+ん)を狙ってボーナスを試したが、
        // 「新幹線」のような複合語の内部分解を壊す新たな副作用が出た上、
        // 断片化自体も解消しきれなかった（残る接続コスト差がさらに大きい）
        // ため、このボーナスは見送る（[[atomic-word-particle-connection-quirk]]
        // 参照。せんをひいた→線を弾いた、は既知の未解決課題として残す）。
        // 「いっ」（行くの音便形）は「いってらっしゃい/いってきます/
        // いってしまった」のように後続の補助動詞（らっしゃる・くる・しまう
        // 等）ごとに「て」以降の接続コストが変わり、通常のユニグラム
        // ボーナス（0未満不可）では稀な同音動詞「逝っ」に勝ちきれない
        // 組み合わせが複数ある（実測: 補助動詞が続く文頭の「いって」で
        // 顕著）。行くは日常会話で圧倒的に高頻度、逝くは文語的・訃報等の
        // 限定的な場面でしか使わないため、`trusted_phrase_bonus`で
        // 0未満まで割り引いて確実に優先する。
        self.trusted_phrase_bonus
            .insert(("いっ".to_string(), "行っ".to_string()), 15000);
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
        // 「どうか」は辞書に副詞としての実在エントリがある（cost=6752）ので
        // 合成ひらがなノードではなく通常のユニグラムボーナス経路で優先する。
        // 漢字表記「同化」（同化政策・文化の同化 等）は限定的な場面でしか
        // 使わないのに対し、「どうか」（お願い・様子伺い・〜かどうか の
        // 一部）は圧倒的に高頻度。学習が無い状態でも「同化」が既定で
        // 選ばれてしまう（実測: どうかしましたか→同化しましたか、
        // どうかしている→同化している）。「同化」がサ変接続名詞として
        // 「し」に直接続く接続コストが本来かなり有利なため、通常の
        // ユニグラムボーナス（0未満不可floor）では勝ちきれず、「問題ない」
        // 「水曜」と同様に0未満まで割り引ける trusted_phrase_bonus を使う
        // （[[adverb-particle-bigram-pollution]]の未修正事例）。
        self.trusted_phrase_bonus
            .insert(("どうか".to_string(), "どうか".to_string()), 8000);
        // 「いくらか」（幾らか、「いくらかもらった」等）が単独/文末では
        // 「イクラ」（cost=3261、いくら/幾ら と同じ品詞クラス）+「か」に
        // 分割されてしまう（実測: いくらか→イクラか）。「いくらか」が
        // 後続語を伴う場合（いくらかもらった 等）は既に辞書本来のコストで
        // 正しく選ばれるため、この場合だけ通常のユニグラムボーナスで
        // 十分（0未満不可floorで足りる、trusted_phrase_bonusは不要）。
        self.learned_unigram
            .entry(("いくらか".to_string(), "幾らか".to_string()))
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

    /// 隣接コロケーションのシード（`seeded_assoc`）を外部ファイルから読み込む。
    /// フォーマットは `読み\t出力表記\tトリガー語\tボーナス(省略可)`。
    /// `#`始まりの行・空行は無視。
    ///
    /// ボーナスは省略時、`読み`の辞書エントリから`出力表記`と競合表記の
    /// 生コスト差を自動算出する（`compute_word_assoc_bonus`、
    /// [[seeded-adjacent-collocation]]）。算出結果が上限
    /// （`SEEDED_ASSOC_MAX_BONUS`）を超える、または`読み`が辞書に無い場合は
    /// そのグループを採用しない（辞書上の生コスト差が元々大きい＝一般的には
    /// 競合表記の方が正しい可能性が高く、無理に押し退けるとリスクが大きい
    /// と判断するため）。4列目に数値を明示すればその値で上書きできる
    /// （手動ピン留め用）。
    ///
    /// 両方向（出力表記→トリガー語、トリガー語→出力表記）に同じボーナスで
    /// 登録する（`rerank_by_seeded_collocation_from`はどちらの並び順の
    /// 隣接語からも引けるようにするため）。`.entry().or_insert()`で書き込む
    /// が、`seeded_assoc`はユーザー学習では書き換わらない専用のマップなので
    /// 実質的には常に新規挿入になる。
    pub fn load_word_assoc_file(&mut self, path: &std::path::Path) -> std::io::Result<usize> {
        let content = std::fs::read_to_string(path)?;
        Ok(self.load_word_assoc_str(&content))
    }

    /// `load_word_assoc_file`のファイルI/Oを伴わない版。候補バッチの検証
    /// ツール（`examples/verify_word_assoc_candidates.rs`）のように、まだ
    /// ディスク上のファイルになっていない候補データを直接読み込みたい
    /// 場合に使う。
    pub fn load_word_assoc_str(&mut self, content: &str) -> usize {
        for collision in find_word_assoc_collisions(content) {
            crate::debug_log!("word_assoc.tsv の衝突を検出（読み込みは継続）: {collision}");
        }
        let mut count = 0;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split('\t');
            let (Some(reading), Some(target), Some(trigger)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (reading, target, trigger) = (reading.trim(), target.trim(), trigger.trim());
            if reading.is_empty() || target.is_empty() || trigger.is_empty() {
                continue;
            }
            let explicit_bonus = parts.next().and_then(|s| s.trim().parse::<i32>().ok());
            let bonus = match explicit_bonus {
                Some(b) => b,
                None => match self.compute_word_assoc_bonus(reading, target) {
                    Some(b) => b,
                    None => {
                        crate::debug_log!(
                            "word_assoc.tsv: 「{reading}」→「{target}」は必要ボーナスが上限を超えるか辞書に無いため不採用（トリガー={trigger}）"
                        );
                        continue;
                    }
                },
            };
            self.seeded_assoc
                .entry((target.to_string(), trigger.to_string()))
                .or_insert(bonus);
            self.seeded_assoc
                .entry((trigger.to_string(), target.to_string()))
                .or_insert(bonus);
            count += 1;
        }
        count
    }

    /// `word_assoc.tsv`のローダーと候補生成ツール（`examples/dump_homophones.rs`
    /// の`=target`指定、`examples/verify_word_assoc_candidates.rs`）の両方が
    /// 使う共通ロジック。生成時に「このボーナスは上限を超えて不採用になる」
    /// ことを事前に知りたい場合はこの関数を直接呼べる（`pub`化済み）。
    ///
    /// `target`（`reading`の変換候補の1つ）が、同じ読みの他候補に競り勝つのに
    /// 必要なボーナスを、辞書の生コストの差から自動算出する。
    ///
    /// `rerank_by_seeded_collocation_from`の純利得計算が
    /// `effective_word_cost`（接続コストを含まない語コストのみ）で比較して
    /// いるのに合わせ、ここでも接続コストは無視し語コストの差だけを見る
    /// （近似の精度をそちらと揃えることが目的で、他の要因を見落としている
    /// わけではない）。
    ///
    /// `target`より安い競合が無ければ`Some(SEEDED_ASSOC_MARGIN)`（既に有利
    /// なので最小限のマージンのみ）。競合が`target`より安ければ
    /// `差 + SEEDED_ASSOC_MARGIN`。この必要量が`SEEDED_ASSOC_MAX_BONUS`を
    /// 超える場合は`None`（採用しない）。超える＝辞書上の生コスト差が
    /// 元々大きい＝一般にはその競合表記の方が正しい可能性が高く、シードで
    /// 押し退けるリスクが大きいと判断する（ユーザー指定の方針）。
    pub fn compute_word_assoc_bonus(&self, reading: &str, target: &str) -> Option<i32> {
        let entries = self.merged_lookup(reading)?;
        // 同じ表記が品詞違いで複数エントリを持つことがある（例: 「工事」が
        // 名詞-サ変接続(cost=1056)と名詞-固有名詞-人名-名(cost=6502)の両方に
        // 存在する）。`.find()`で最初に見つかった方（辞書内の格納順、コスト
        // 順ではない）を使うと、たまたま高コスト側を掴んで「最安の競合にも
        // 負けている」と誤判定し、実際には最安のはずの語を不採用にしてしまう
        // （実際にこのバグで「こうじ」の「工事」が誤って不採用と判定された）。
        // 同じ表記の中では常に最安のコストを使う。
        let target_cost = entries
            .iter()
            .filter(|e| e.surface == target)
            .map(|e| e.cost as i32)
            .min()?;
        let cheapest_competitor = entries
            .iter()
            .filter(|e| e.surface != target)
            .map(|e| e.cost as i32)
            .min();
        let required = match cheapest_competitor {
            Some(c) if c < target_cost => (target_cost - c) + SEEDED_ASSOC_MARGIN,
            _ => SEEDED_ASSOC_MARGIN,
        };
        (required <= SEEDED_ASSOC_MAX_BONUS).then_some(required)
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
    ///
    /// `learn_unigram`（漢字変換側の学習）と同じ頻度→ボーナス換算・上限を
    /// 使う。以前はここだけ上限を1500〜3000に抑えていたが、それだと
    /// 「Escで戻した回数」がどれだけ多くても、漢字側の学習
    /// （最大6000まで伸びる）に対して常に不利になり、ユーザーが繰り返し
    /// ひらがなを選び直しているのに漢字変換が優先され続けてしまう
    /// （確定のたびに`forget_reading`で漢字側の学習は消しているので、
    /// 残るのは辞書本来のコストとの勝負になる）。断片化については
    /// ボーナスの大小ではなく`breaks_longer_word`（同じ開始位置により
    /// 長い実在語があれば、そもそもひらがなノードを足さない）という
    /// 構造的なガードで別途守られているため、上限をそろえても
    /// 「さいきどう→さい+起動」のような回帰は起きない
    /// （回帰防止テストで確認済み）。
    pub fn learn_hiragana(&mut self, reading: &str, freq: u32) {
        let bonus = frequency_to_bonus(freq).min(UNIGRAM_BONUS_CAP);
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
        self.corpus_bigram_prev_surfaces.clear();
        for (prev_surface, surface, freq) in &lm.bigrams {
            let bonus = corpus_bigram_bonus(*freq);
            if bonus != 0 {
                self.corpus_bigram
                    .insert((prev_surface.clone(), surface.clone()), bonus);
                // `base_connection_cost`が辺ごとに(String,String)を確保して
                // HashMapを引く前に、この安価な集合でほぼ確実に外れる
                // prev_surfaceを弾けるようにする（`learned_bigram`と
                // `bigram_prev_surfaces`の関係と同じ）。`load_corpus_lm`が
                // 唯一の書き込み口（丸ごと差し替え）なので、`bigram_prev_surfaces`
                // のように他の変更（`forget_unigram`等）で再構築される心配が無い
                // 専用のフィールドにしている。
                self.corpus_bigram_prev_surfaces.insert(prev_surface.clone());
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
        // シード（隣接コロケーション）→学習（非隣接連想）の順で適用する。
        // 学習が後に来ることで、両方が同じ語について異なる差し替えを
        // 示唆した場合に必ず学習側が最終的に勝つ（ユーザー本人の実際の
        // 使い方が一般的な傾向より優先されるべきという方針、
        // [[homophone-selection-is-not-fixable-without-lm]]）。
        let base = self.rerank_by_seeded_collocation_from(base, 0);
        let base = self.rerank_by_assoc_from(base, 0);
        self.judge_or_keep(reading, &[], base)
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
            let Some(alts) = self.merged_lookup(&cur.reading) else {
                continue;
            };
            let cur_cost = self.effective_word_cost(&cur);

            let mut best: Option<WordEntry> = None;
            let mut best_gain = 0i32;
            for alt in alts.iter() {
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
                crate::debug_log!(
                    "rerank_by_assoc: {}→{}（連想利得={}）",
                    base[i].surface, alt.surface, best_gain
                );
                base[i] = alt;
            }
        }
        base
    }

    /// シード済み隣接コロケーション（`seeded_assoc`）で1-bestを微調整する。
    ///
    /// `rerank_by_assoc_from`とは逆に、直前・直後（間に助詞すら挟まない
    /// 隣接語）だけを見る。「経済＋政策」「かたい＋仕事」「はやい＋くるま」
    /// のような複合名詞・形容詞＋名詞の同音異義語選択は、まさにこの
    /// 隣接構造でしか起きないため（[[homophone-selection-is-not-fixable-without-lm]]）。
    /// `learned_assoc`の隣接語除外は「駅の汽車」型の連想向けの安全設計
    /// なのでそのまま残し、こちらは事前精査済みの`seeded_assoc`だけを
    /// 対象にする別関数にすることで、その安全設計に触れずに済ませている。
    ///
    /// `first_mutable`より前の文節は書き換えない（`convert_context_aware_pinned`
    /// から使う。`rerank_by_assoc_from`と同じ規約）。
    pub(crate) fn rerank_by_seeded_collocation_from(
        &self,
        mut base: Vec<WordEntry>,
        first_mutable: usize,
    ) -> Vec<WordEntry> {
        if self.seeded_assoc.is_empty() || base.len() < 2 {
            return base;
        }

        // 隣接語の表記は、この関数に入った時点の元の1-bestで固定する
        // （スナップショット）。`base[i]`をその場で書き換えながらループすると、
        // 後続の判定が「元の1-best」ではなく「直前の判定で書き換わった後の
        // 表記」を隣接語として参照してしまい、1回のパスの中で書き換えが
        // 連鎖する（実測: 「はやい→速い」への書き換えが先に起きると、
        // その「速い」を隣接語として直後の「くるま/車」判定が再評価され、
        // 別途修正済みだった「くるま→車」が巻き戻る。
        // [[seeded-adjacent-collocation]]で発見）。読み（インデックス）だけを
        // 見る1パス構造にすることで、シードを何千件に増やしても各語の
        // 判定が他の語の書き換え結果に依存しないことを保証する。
        let original_surfaces: Vec<String> = base.iter().map(|e| e.surface.clone()).collect();

        for i in first_mutable..base.len() {
            if !is_content_pos(&base[i].pos) || base[i].surface == base[i].reading {
                continue;
            }
            let cur = base[i].clone();
            let Some(alts) = self.merged_lookup(&cur.reading) else {
                continue;
            };
            let cur_cost = self.effective_word_cost(&cur);

            // 直前・直後の表記だけを見る（`rerank_by_assoc_from`と正反対）。
            // 必ずスナップショット（`original_surfaces`）から読む。
            let mut neighbors: Vec<&str> = Vec::with_capacity(2);
            if i > 0 {
                neighbors.push(original_surfaces[i - 1].as_str());
            }
            if i + 1 < base.len() {
                neighbors.push(original_surfaces[i + 1].as_str());
            }
            if neighbors.is_empty() {
                continue;
            }

            let mut best: Option<WordEntry> = None;
            let mut best_gain = 0i32;
            for alt in alts.iter() {
                if alt.surface == cur.surface {
                    continue;
                }
                let mut assoc = 0i32;
                for &nb in &neighbors {
                    assoc += self
                        .seeded_assoc
                        .get(&(nb.to_string(), alt.surface.clone()))
                        .copied()
                        .unwrap_or(0);
                }
                if assoc == 0 {
                    continue;
                }
                let net = assoc.saturating_sub(self.effective_word_cost(alt) - cur_cost);
                if net > best_gain {
                    best_gain = net;
                    best = Some(alt.clone());
                }
            }
            if let Some(alt) = best {
                crate::debug_log!(
                    "rerank_by_seeded_collocation: {}→{}（隣接語利得={}）",
                    base[i].surface, alt.surface, best_gain
                );
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
        for (v, dist) in self.merged_fuzzy_readings(word, 1) {
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

    /// 学習バイグラムのボーナスが、祖先ノードとprevを結合すれば実在する
    /// 1語になる場合にだけ効いている（＝そのボーナスを受け取るためだけに
    /// 正当な1語を分割している）かを判定する。読み2文字以下の短い語同士の
    /// 組み合わせに限定する（既存の`bonused_short_prev_conn_floor_at_utterance_start`
    /// と同じ「もぐら叩きを避けるための語彙非依存の条件」の考え方）。
    fn bigram_bonus_splits_atomic_word(
        &self,
        grandparent: Option<&WordEntry>,
        prev: Option<&WordEntry>,
    ) -> bool {
        let Some(gp) = grandparent else { return false };
        let Some(p) = prev else { return false };
        if gp.reading.chars().count() > 2 || p.reading.chars().count() > 2 {
            return false;
        }
        let combined_reading = format!("{}{}", gp.reading, p.reading);
        let Some(entries) = self.merged_lookup(&combined_reading) else {
            return false;
        };
        entries.iter().any(|e| is_content_pos(&e.pos))
    }

    /// `prev`と同じ読み・同じ接続クラス（left_id/right_id一致）だが学習
    /// ボーナスの乗っていない別表記（ひらがな等）が辞書に存在するか。
    /// `bonused_short_prev_conn_floor_at_utterance_start`が、学習で優先
    /// したい表記自身の接続コストだけを狙い撃ちで不利にし、無学習の
    /// 同義語に道を譲ってしまう副作用を避けるために使う
    /// （実測:「他」を学習していても「他の/他が/他に」で無学習の
    /// 「ほか」に負ける）。
    fn prev_has_untrusted_pos_homograph(&self, prev: &WordEntry) -> bool {
        let Some(entries) = self.merged_lookup(&prev.reading) else {
            return false;
        };
        entries.iter().any(|e| {
            e.surface != prev.surface
                && e.left_id == prev.left_id
                && e.right_id == prev.right_id
                && !self.is_bonused_entry(e)
        })
    }

    fn is_bonused_entry(&self, e: &WordEntry) -> bool {
        let key = (e.reading.clone(), e.surface.clone());
        self.learned_unigram.get(&key).copied().unwrap_or(0) > 0
            || self.trusted_phrase_bonus.get(&key).copied().unwrap_or(0) > 0
    }

    /// 単語の実効コスト（1文字漢字ペナルティ・学習ユニグラム・コーパス頻度を反映）。
    /// `build_lattice`が各ラティスノードに適用するのと同じ計算式。候補一覧
    /// （`hook-dll`の`build_candidates`）からも参照するため`pub(crate)`にしている。
    pub fn effective_word_cost(&self, e: &WordEntry) -> i32 {
        // `build_lattice`がラティスの各ノードに適用するのと同じ3つのペナルティ
        // （1文字漢字・カタカナ固有名詞・記号）を揃える。ここが揃っていないと、
        // これを使う`rerank_by_assoc_from`（連想再ランク）や候補一覧が、
        // find_best_pathの実際のラティスコストより語を過小評価し、
        // find_best_pathなら選ばないはずの固有名詞・記号語を選んでしまう。
        let pen = single_kanji_penalty(&e.surface, self.single_kanji_penalty)
            + proper_noun_penalty(&e.surface, &e.pos)
            + symbol_penalty(&e.surface, &e.pos);
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
            let matches = self.merged_prefix_search(remaining);
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
                        // 「いっ」→「行っ」は読み・後続語を問わない無条件適用にすると
                        // 無関係な語（一貫性→行っ完成）まで巻き込む（2026-09-13発見、
                        // [[trusted-phrase-bonus-blast-radius]]）。この1組だけ
                        // node単位の適用から除外し、`iitte_verb_conn_floor`で
                        // 後続が「て」「た」のときだけedge単位で効かせる。
                        let trusted_bonus = if key.0 == "いっ" && key.1 == "行っ" {
                            0
                        } else {
                            self.trusted_phrase_bonus.get(&key).copied().unwrap_or(0)
                        };
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
                // 同じ終了位置でより手前から始まる実在の内容語（例:「毎回」が
                // 「まい」+「かい」の「かい」側だけを見た場合、「毎回」は
                // このひらがなノードより手前の位置から始まり同じ終了位置で
                // 終わる）があるなら、このひらがな優先ノードは足さない。
                // 上のチェックは「同じ開始位置からより長い語」しか見ないため、
                // このケース（自分より手前から始まる語）は素通りしてしまう
                // （実測:「まいかい」で「かい」をEscひらがな優先していると、
                // 学習ユニグラムの乗った「毎回」自体より、断片「舞い」+
                // ひらがな「かい」の方が安くなり「舞いかい」に化ける。
                // ひらがな優先ノードはコストが学習ボーナス次第で負になり得る
                // （`5000 - bonus`）ため、学習ユニグラムの0下限しかない
                // 「毎回」を通常のコスト勝負では負かせてしまう）。
                let breaks_longer_word_from_before = lattice.nodes_ending_at[end].iter().any(|&idx| {
                    let n = &lattice.nodes[idx];
                    n.start < start && n.entry.as_ref().is_some_and(|e| is_content_pos(&e.pos))
                });
                if breaks_longer_word_from_before {
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
        // 2パスに分ける: 1パス目は`lattice.input`を不変借用するだけで、
        // 追加すべき長さの上限（`chosen_max_len`）だけを決める。2パス目で
        // `lattice.add_word`（可変借用）を呼ぶ。以前は`add_word`と競合しない
        // よう`lattice.input`（入力全体）をこの関数の呼び出しごと（＝1文字
        // 位置ごと）に丸ごとcloneしていたが、これは文字数に対してO(n²)の
        // 無駄なコピーになる（実測: 長い入力ほど`build_lattice`の所要時間が
        // 文字数に対して超線形に伸びる一因）。`lattice.input`は関数内で
        // 変更されないので、2パスに分けるだけでcloneせずに済む。
        let mut chosen_max_len = self.katakana_min_len.saturating_sub(1);
        {
            let input = lattice.input.as_str();
            let cheapest_at = |char_pos: usize| -> Option<i16> {
                let bp = byte_positions[char_pos];
                self.dictionary
                    .common_prefix_search(&input[bp..])
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
                chosen_max_len = len;
            }
        }
        for len in self.katakana_min_len..=chosen_max_len {
            let end_idx = start_idx + len;
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
            if !self.corpus_bigram.is_empty()
                && self.corpus_bigram_prev_surfaces.contains(prev_surface)
            {
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
            .saturating_add(prev.map_or(0, |p| adjective_terminal_then_ni_penalty(p, cur, before) - before))
            .saturating_add(next.map_or(0, |n| adjective_terminal_then_ni_penalty(cur, n, after) - after))
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
                let current_node = &lattice.nodes[node_idx];
                let is_eos = node_idx == lattice.eos_index;
                // `current_node`（ce）だけで決まるペナルティは prev_idx に依存しない
                // ので、内側ループの外（この node_idx につき1回）で計算しておく。
                // 以前は内側ループ内で毎回呼んでおり、同じ結果を
                // `ending_indices.len()`回（＝辺の数だけ）再計算していた。
                let cur_lone_particle_penalty = current_node
                    .entry
                    .as_ref()
                    .map_or(0, single_kanji_lone_particle_reading_penalty);
                let cur_is_unallowed_suffix = current_node
                    .entry
                    .as_ref()
                    .is_some_and(is_unallowed_single_kanji_suffix);

                let mut best_cost = i32::MAX;
                let mut best_prev: Option<usize> = None;

                for &prev_idx in &ending_indices {
                    let prev_node = &lattice.nodes[prev_idx];
                    if prev_node.total_cost == i32::MAX {
                        continue;
                    }

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
                        if cur_is_unallowed_suffix && prev_is_katakana_word(pe) {
                            conn_cost = conn_cost.saturating_add(2000);
                        }
                        // 形容詞の終止形（〜い）に「て」が直接続くのは文法的に
                        // 常に誤り（正しくは連用形〜くて）なので無条件でペナルティ
                        conn_cost = adjective_terminal_then_te_penalty(pe, ce, conn_cost);
                        conn_cost = adjective_terminal_then_ni_penalty(pe, ce, conn_cost);
                        // 1文字漢字の表記かつ読みが単独助詞と一致する語（野・葉 等）
                        // が文頭以外に出現するのは不自然なので無条件でペナルティ
                        conn_cost = conn_cost.saturating_add(cur_lone_particle_penalty);
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
                            is_eos,
                        ),
                    );
                    // 「いっ」→「行っ」（行くの音便形）は、直後が「て」「た」
                    // （行って/行った）のときだけ稀な同音動詞「逝っ」に対して
                    // 確実に勝たせる。学習の有無を問わず常に対象（元は
                    // `trusted_phrase_bonus`でnode単位・無条件だったものを
                    // edge単位・条件付きに変更、2026-09-13）。
                    conn_cost = iitte_verb_conn_floor(
                        prev_node.entry.as_ref(),
                        current_node.entry.as_ref(),
                        conn_cost,
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
                    // 学習バイグラムのボーナスが「祖先ノード＋prevを結合すれば実在する
                    // 1語になる」場合にだけ効いているなら、そのボーナスを打ち消す。
                    // バイグラムは表記の完全一致キーのため、正当な1語（例:「この」）を
                    // 敢えて分割（「こ」+「の」）しないと受け取れないボーナス
                    // （例: 学習バイグラム「の→空」）が、分割してでも受け取る方を
                    // 有利にしてしまう抜け道になる（実測:「このそらをみあげて」→
                    // 「股の空を見上げて」。[[atomic-word-particle-connection-quirk]]と
                    // 同系統）。既存の「文頭限定」ガードは、prev自身が文頭語のときしか
                    // 守れない（このケースはprevが2語目の「の」で対象外）ため、
                    // 「祖先＋prevの結合読みが辞書に実在するか」という語彙非依存の
                    // 別条件で捕捉する。
                    if bigram_bonus != 0 && self.bigram_bonus_splits_atomic_word(grandparent_entry, prev_node.entry.as_ref()) {
                        conn_cost = conn_cost.saturating_add(bigram_bonus);
                    }
                    // 束縛モーラ「ん」+助詞の有利な接続コストが、単独の1文字漢字/
                    // カタカナ直後だと断片化の抜け道になる問題は学習の有無を
                    // 問わないため、他のガード群（学習ボーナス絡み限定）とは別に
                    // 無条件で適用する。
                    if let (Some(pe), Some(ce)) = (&prev_node.entry, &current_node.entry) {
                        conn_cost = single_char_bound_mora_then_particle_conn_floor(
                            grandparent_entry,
                            pe,
                            ce,
                            conn_cost,
                        );
                    }
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
                        let prev_has_untrusted_pos_homograph = prev_bonus != 0
                            && prev_is_utterance_start
                            && prev_node
                                .entry
                                .as_ref()
                                .is_some_and(|p| self.prev_has_untrusted_pos_homograph(p));
                        conn_cost = bonused_short_prev_conn_floor_at_utterance_start(
                            prev_node.entry.as_ref(),
                            current_node.entry.as_ref(),
                            prev_is_utterance_start,
                            prev_bonus,
                            cur_bonus,
                            bigram_bonus,
                            prev_has_untrusted_pos_homograph,
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

        // `convert_with_cost`と同じ断片修復を各候補パスにも適用する
        // （`ime-cli`の`convert`はこちらを経由するため、これが無いと
        // CLIと実際のライブ変換で断片修復の有無が食い違ってしまう）。
        n_best_from_lattice(&lattice, &self.dictionary, n)
            .into_iter()
            .map(|path| self.repair_single_kanji_fragments(path))
            .collect()
    }

    /// N-best候補を表層形の文字列として取得
    pub fn n_best_strings(&self, hiragana: &str, n: usize) -> Vec<String> {
        self.n_best(hiragana, n)
            .into_iter()
            .map(|entries| entries.iter().map(|e| e.surface.as_str()).collect())
            .collect()
    }
}

/// `word_assoc.tsv`（`seeded_assoc`のシード元、[[seeded-adjacent-collocation]]）
/// の1列目（出力表記）が、別グループの2列目（トリガー語）としても
/// 使われていないかを検出する。
///
/// `rerank_by_seeded_collocation_from`は現在スナップショット方式（同一パス
/// 内での連鎖を防ぐ設計）だが、それとは別に、あるグループの出力語が
/// 別の無関係なグループのトリガー語としても登録されていること自体は
/// データ品質上のリスクを示す（実際に踏んだ事例: 「速い」のトリガー語に
/// 「クルマ」を含めたところ、別途修正済みだった「くるま→車」を巻き戻す
/// 結果になった）。数千件規模にシードを増やすと目視での確認が現実的で
/// なくなるため、読み込み時に機械的に検出する。呼び出し元（`ViterbiConverter`
/// の状態を使わない）から独立してテストできるよう純粋関数にしている。
///
/// 戻り値は衝突ごとの説明文（衝突が無ければ空）。
pub fn find_word_assoc_collisions(content: &str) -> Vec<String> {
    let mut targets: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut lines: Vec<(String, String)> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('\t');
        // フォーマット: 読み<TAB>出力表記(target)<TAB>トリガー語(trigger)<TAB>ボーナス(省略可)
        let (Some(_reading), Some(target), Some(trigger)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (target, trigger) = (target.trim(), trigger.trim());
        if target.is_empty() || trigger.is_empty() {
            continue;
        }
        targets.insert(target.to_string());
        lines.push((target.to_string(), trigger.to_string()));
    }

    let mut collisions: Vec<String> = lines
        .iter()
        .filter(|(target, trigger)| trigger != target && targets.contains(trigger))
        .map(|(target, trigger)| {
            format!(
                "「{trigger}」は「{target}」のトリガー語だが、「{trigger}」自体も別グループの出力表記として登録されている"
            )
        })
        .collect();
    collisions.sort();
    collisions.dedup();
    collisions
}
