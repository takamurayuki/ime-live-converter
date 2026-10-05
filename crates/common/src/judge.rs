//! 判断層（judge-lm）: 変換候補を統計言語モデルで評価し、候補上の確率分布を返す。
//!
//! Jev（TypeSafe AI の「生成しない判断モデル」）と同じ考え方で、文章を
//! 生成せず「与えられた選択肢のどれが尤もらしいか」の較正済み確率だけを
//! 出す。中身は日本語 Wikipedia を IPADic で分かち書きして学習した言語モデル
//! （`dict-builder judge-train` で構築）で、計算は完全に決定論的。実行時は
//! メモリ上の表引きだけ（外部通信なし）。
//!
//! モデルは2層:
//! - 単語バイグラム（Kneser-Ney 割引）: 語の単位は (表記, 読み, 文脈ID)。
//!   文脈IDは IME の辞書（system.dic）と同じ MeCab IPADic の左右文脈ID で、
//!   品詞だけでなく活用型・活用形まで区別する。
//! - 文法クラスモデル（単語バイグラムのバックオフ先）:
//!   P(今の語のクラス | 直前の語のクラス) × P(語 | クラス)。
//!   「動詞連用形→助動詞『ます』連用形」のような文法的なつながりは、個々の
//!   語の組がコーパスに無くても（Wikipedia は「である」調で「買いました」が
//!   ほぼ出ない等）クラスの水準で正しく評価できる。
//!
//! 既存の Viterbi（手調整ガード込み）とは別エンジンとして並走させる設計で、
//! `ViterbiConverter::set_judge` で外せば従来の挙動に完全に戻る
//! （[[jev-style-judge-direction]]）。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use crate::WordEntry;

/// 文頭を表す語ID（学習時にも同じIDで文頭を数えている）
pub const BOS_ID: u32 = 0;
/// 文末を表す語ID
pub const EOS_ID: u32 = 1;
/// 文頭・文末の文脈ID（MeCab IPADic では 0）
pub const BOS_CLASS: u16 = 0;

/// 候補の総合スコアと確率化のパラメータ（`judge_eval` で開発用データから決める）
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(default)]
pub struct JudgeParams {
    /// LM 対数確率（自然対数）に掛ける重み
    pub lm_weight: f32,
    /// 辞書コスト（Viterbi と同じ単位）をこの値で割って対数尺度に揃える
    pub dict_scale: f32,
    /// softmax の温度（確率の較正用。スコアの並び順には影響しない）
    pub temperature: f32,
    /// 語彙外の語の読み1文字あたりの追加対数確率（負の値）。語彙外の語は
    /// 長いほど「辞書にない読みの塊をまとめて飲み込んだ」可能性が高い
    /// （カタカナ・フォールバックで「ぐんぐんすすむ→グングンススム」等）ので、
    /// 1語あたり一律ではなく読みの長さに比例して罰する。
    pub unk_char_logp: f32,
    /// 事前精査済みの隣接コロケーション（word_assoc.tsv）1組あたりの加点（対数尺度）。
    /// 「この2語は続けて使う」という人手の知識なので、確認済みのバイグラム
    /// 相当として LM 側の尺度で効かせる（辞書コスト換算のボーナスは
    /// 語コスト差ぎりぎりに作られていて、LM の差に比べて小さすぎるため）。
    pub seed_bonus: f32,
}

impl Default for JudgeParams {
    fn default() -> Self {
        Self { lm_weight: 1.0, dict_scale: 2000.0, temperature: 1.0, unk_char_logp: -2.0, seed_bonus: 4.0 }
    }
}

/// ディスク上の形式（bincode）。バイグラムは前の語ごとの CSR 形式で持つ
/// （`bi_offsets[v]..bi_offsets[v+1]` が語 v に続く語の範囲、`bi_next` は昇順）。
#[derive(Serialize, Deserialize, Default)]
pub struct JudgeLmData {
    /// 語彙（"表記\t読み\t文脈ID"）。添字が語ID。0/1 は BOS/EOS
    pub vocab: Vec<String>,
    /// 各語の文脈ID（IPADic は左右同じ）
    pub word_class: Vec<u16>,
    /// log P(語 | クラス)
    pub emit_logp: Vec<f32>,
    /// 各語を前文脈としたときのバックオフ重みの対数（log γ(v)）
    pub backoff: Vec<f32>,
    pub bi_offsets: Vec<u32>,
    pub bi_next: Vec<u32>,
    /// log P(w | v)（クラスモデルとの補間込み）
    pub bi_logp: Vec<f32>,
    /// クラス数（文脈IDの最大値+1）
    pub n_class: u32,
    /// log P(今のクラス L | 直前のクラス R)、添字は `R * n_class + L`
    pub class_logp: Vec<f32>,
    /// 語彙外の語の log P(語 | クラス)（クラスごと）
    pub unk_emit_logp: Vec<f32>,
    /// `JudgeParams` の JSON。パラメータの項目を増やしても学習済みのモデルを
    /// 作り直さずに読めるよう、bincode の固定レイアウトではなく JSON で持つ
    /// （無い項目は既定値になる）
    pub params_json: String,
}

/// 実行時の判断モデル
pub struct JudgeLm {
    data: JudgeLmData,
    params: JudgeParams,
    index: HashMap<String, u32>,
    /// 表記 → [(読み, 語ID)]。IME 辞書にしかない複合語（「問題ない」「英数」等。
    /// IPADic の分かち書きでは「問題/ない」「英/数」に割れるため LM の語彙に無い）
    /// を LM の語彙単位に分解して採点するための索引（`segment_ids`）
    by_surface: HashMap<String, Vec<(String, u32)>>,
}

/// 分解を試みる表記の最大文字数・1片の最大文字数
const SEGMENT_MAX_CHARS: usize = 16;
const SEGMENT_MAX_PIECE: usize = 8;

/// 直前の文脈（LM 語ID と右文脈ID）
#[derive(Clone, Copy, Debug)]
pub struct LmCtx {
    pub word: Option<u32>,
    pub class: u16,
}

impl LmCtx {
    pub const BOS: LmCtx = LmCtx { word: Some(BOS_ID), class: BOS_CLASS };
}

/// 1語を LM の単位に写したもの。語彙内ならその語1つ、IME 辞書にしかない
/// 複合語は語彙内の語の連なり（`segment_ids`）、それも無理なら語彙外。
#[derive(Clone, Debug)]
pub struct LmUnit {
    /// 最初の LM 語ID（語彙外なら None）と、その左文脈ID
    pub first: Option<u32>,
    pub first_class: u16,
    /// 次の語の文脈になる最後の LM 語ID（語彙外なら None）と、その右文脈ID
    pub last: LmCtx,
    /// 分解した片どうしの内部の対数確率の合計
    pub internal: f32,
    /// 語彙外のときの読みの文字数（語彙内・分解できたときは 0）
    pub unk_chars: u32,
}

/// 1候補の判断結果
#[derive(Clone, Debug)]
pub struct JudgeScore {
    /// LM の対数確率（文頭からの合計、自然対数。語彙外の文字数罰を含む）
    pub lm_logp: f32,
    /// `lm_logp` から語彙外の文字数罰を除いたもの
    pub lm_base: f32,
    /// 語彙外の語の読みの文字数
    pub unk_chars: u32,
    /// 辞書コスト（`ViterbiConverter::path_dict_cost`）
    pub dict_cost: i32,
    /// 隣接コロケーション（word_assoc.tsv）に当たった組の数
    pub seed_hits: u32,
    /// 総合スコア（大きいほど良い）
    pub score: f32,
}

/// 語彙のキー（"表記\t読み\t文脈ID"）
pub fn vocab_key(surface: &str, reading: &str, class: u16) -> String {
    format!("{}\t{}\t{}", surface, reading, class)
}

impl JudgeLm {
    pub fn from_data(data: JudgeLmData) -> Self {
        let index = data
            .vocab
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), i as u32))
            .collect();
        let mut by_surface: HashMap<String, Vec<(String, u32)>> = HashMap::new();
        for (i, k) in data.vocab.iter().enumerate().skip(2) {
            let mut it = k.split('\t');
            if let (Some(surface), Some(reading)) = (it.next(), it.next()) {
                by_surface
                    .entry(surface.to_string())
                    .or_default()
                    .push((reading.to_string(), i as u32));
            }
        }
        let params = serde_json::from_str(&data.params_json).unwrap_or_default();
        Self { data, params, index, by_surface }
    }

    pub fn load(path: &Path) -> Result<Self> {
        use bincode::Options;
        let file = File::open(path)?;
        // 読み込み量をファイルサイズで制限する。形式の古い/壊れたファイルを
        // 読んだときに長さフィールドを誤読して巨大な確保を試み、プロセスごと
        // 落ちる（フック常駐プロセスでは IME 全体が止まる）のを防ぎ、通常の
        // エラーとして返す（呼び出し側は判断層なしで続行する）。
        let limit = file.metadata()?.len() + 1024;
        let data: JudgeLmData = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .allow_trailing_bytes()
            .with_limit(limit)
            .deserialize_from(BufReader::with_capacity(1 << 20, file))?;
        let n = data.vocab.len();
        anyhow::ensure!(
            n >= 2
                && data.word_class.len() == n
                && data.emit_logp.len() == n
                && data.backoff.len() == n
                && data.bi_offsets.len() == n + 1
                && data.bi_next.len() == data.bi_logp.len()
                && data.bi_offsets.last().map_or(false, |&e| e as usize == data.bi_next.len())
                && data.bi_offsets.windows(2).all(|w| w[0] <= w[1])
                && data.bi_next.iter().all(|&w| (w as usize) < n)
                && data.class_logp.len() == (data.n_class as usize) * (data.n_class as usize)
                && data.unk_emit_logp.len() == data.n_class as usize
                && data.word_class.iter().all(|&c| (c as u32) < data.n_class),
            "judge_lm の形式が不正です"
        );
        Ok(Self::from_data(data))
    }

    pub fn save_data(data: &JudgeLmData, path: &Path) -> Result<()> {
        let file = File::create(path)?;
        bincode::serialize_into(BufWriter::with_capacity(1 << 20, file), data)?;
        Ok(())
    }

    pub fn params(&self) -> JudgeParams {
        self.params
    }

    pub fn set_params(&mut self, params: JudgeParams) {
        self.params = params;
        self.data.params_json = serde_json::to_string(&params).unwrap_or_default();
    }

    pub fn data(&self) -> &JudgeLmData {
        &self.data
    }

    pub fn vocab_len(&self) -> usize {
        self.data.vocab.len()
    }

    pub fn bigram_len(&self) -> usize {
        self.data.bi_next.len()
    }

    /// (表記, 読み, 文脈ID) の語ID。語彙外なら None
    pub fn word_id(&self, surface: &str, reading: &str, class: u16) -> Option<u32> {
        self.index.get(&vocab_key(surface, reading, class)).copied()
    }

    /// log P(今のクラス | 直前のクラス)。範囲外のクラス（辞書の作り直しで
    /// 文脈IDが増えた等）は一様分布として扱う。
    fn class_logp(&self, prev: u16, cur: u16) -> f32 {
        let n = self.data.n_class as usize;
        if (prev as usize) < n && (cur as usize) < n {
            self.data.class_logp[prev as usize * n + cur as usize]
        } else {
            -(n.max(2) as f32).ln()
        }
    }

    fn unk_emit(&self, class: u16) -> f32 {
        self.data.unk_emit_logp.get(class as usize).copied().unwrap_or(-20.0)
    }

    /// log P(今の語 | 直前の文脈)
    ///
    /// 単語バイグラムがあればそれ（クラスモデルとの補間込み）、無ければ
    /// バックオフ重み × クラスモデル。直前の語が語彙外ならクラスモデルだけ。
    /// 今の語が語彙外なら「そのクラスの未知の語」として採点する。
    pub fn logp(&self, prev: LmCtx, cur: Option<u32>, cur_class: u16) -> f32 {
        let d = &self.data;
        if let (Some(v), Some(w)) = (prev.word, cur) {
            let (s, e) = (d.bi_offsets[v as usize] as usize, d.bi_offsets[v as usize + 1] as usize);
            if let Ok(i) = d.bi_next[s..e].binary_search(&w) {
                return d.bi_logp[s + i];
            }
        }
        let backoff = prev.word.map_or(0.0, |v| d.backoff[v as usize]);
        let emit = match cur {
            Some(w) => d.emit_logp[w as usize],
            None => self.unk_emit(cur_class),
        };
        backoff + self.class_logp(prev.class, cur_class) + emit
    }

    fn ctx_of(&self, word: u32) -> LmCtx {
        LmCtx { word: Some(word), class: self.data.word_class[word as usize] }
    }

    /// 語彙に無い (表記, 読み) を、語彙内の語の連なりに分解する（表記と読みを
    /// 同時に切り分ける動的計画法。先頭片は文頭からの、以降は片どうしの
    /// 対数確率が最大になる分解を選ぶ）。分解できなければ None（本当の語彙外）。
    /// 直前の語に依らない分解にしているのは、ラティス上の Viterbi
    /// （`judge_select.rs`）でノードごとに1回だけ計算して使い回すため。
    pub fn segment_ids(&self, surface: &str, reading: &str) -> Option<Vec<u32>> {
        let s: Vec<char> = surface.chars().collect();
        let r: Vec<char> = reading.chars().collect();
        if s.len() < 2 || s.len() > SEGMENT_MAX_CHARS {
            return None;
        }
        // best[i][j] = 表記 i 文字・読み j 文字まで消費したときの (対数確率, 片の列)
        let mut best: Vec<Vec<Option<(f32, Vec<u32>)>>> = vec![vec![None; r.len() + 1]; s.len() + 1];
        best[0][0] = Some((0.0, Vec::new()));
        for i in 0..s.len() {
            for j in 0..r.len() {
                let Some((score, ids)) = best[i][j].clone() else { continue };
                let ctx = ids.last().map_or(LmCtx::BOS, |&v| self.ctx_of(v));
                for a in 1..=SEGMENT_MAX_PIECE.min(s.len() - i) {
                    // 1片で表記全体を覆う分解は元の語そのもの（語彙外）なので除く
                    if i == 0 && a == s.len() {
                        continue;
                    }
                    let piece: String = s[i..i + a].iter().collect();
                    let Some(cands) = self.by_surface.get(&piece) else { continue };
                    for (pr, id) in cands {
                        let pr_len = pr.chars().count();
                        if j + pr_len > r.len() || !r[j..j + pr_len].iter().copied().eq(pr.chars()) {
                            continue;
                        }
                        let next = score + self.logp(ctx, Some(*id), self.data.word_class[*id as usize]);
                        let slot = &mut best[i + a][j + pr_len];
                        if slot.as_ref().map_or(true, |(b, _)| next > *b) {
                            let mut v = ids.clone();
                            v.push(*id);
                            *slot = Some((next, v));
                        }
                    }
                }
            }
        }
        best[s.len()][r.len()].take().map(|(_, ids)| ids)
    }

    /// 同じ (表記, 読み) で文脈IDだけが違う語彙内の語のうち、最も出現確率の
    /// 高いもの。IME 辞書（extra.csv 等で品詞を付け直した語を含む）と学習時の
    /// 分かち書きとで同じ語に別の文脈IDが付いていることがあり、そのまま
    /// 語彙外扱いにすると不当に罰してしまうため（例: 機能 が IME 辞書では
    /// 名詞一般、IPADic の分かち書きではサ変接続）。
    fn word_id_any_class(&self, surface: &str, reading: &str) -> Option<u32> {
        self.by_surface
            .get(surface)?
            .iter()
            .filter(|(r, _)| r == reading)
            .map(|&(_, id)| id)
            .max_by(|&a, &b| {
                self.data.emit_logp[a as usize]
                    .partial_cmp(&self.data.emit_logp[b as usize])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// 1語（IME 辞書のエントリ）を LM の単位に写す
    pub fn unit(&self, e: &WordEntry) -> LmUnit {
        let (left, right) = (e.left_id as u16, e.right_id as u16);
        if let Some(id) = self.word_id(&e.surface, &e.reading, left) {
            return LmUnit {
                first: Some(id),
                first_class: left,
                last: LmCtx { word: Some(id), class: right },
                internal: 0.0,
                unk_chars: 0,
            };
        }
        if let Some(id) = self.word_id_any_class(&e.surface, &e.reading) {
            let class = self.data.word_class[id as usize];
            return LmUnit {
                first: Some(id),
                first_class: class,
                last: LmCtx { word: Some(id), class },
                internal: 0.0,
                unk_chars: 0,
            };
        }
        if let Some(ids) = self.segment_ids(&e.surface, &e.reading) {
            let internal = ids
                .windows(2)
                .map(|w| self.logp(self.ctx_of(w[0]), Some(w[1]), self.data.word_class[w[1] as usize]))
                .sum();
            let (first, last) = (ids[0], ids[ids.len() - 1]);
            return LmUnit {
                first: Some(first),
                first_class: self.data.word_class[first as usize],
                last: self.ctx_of(last),
                internal,
                unk_chars: 0,
            };
        }
        LmUnit {
            first: None,
            first_class: left,
            last: LmCtx { word: None, class: right },
            internal: 0.0,
            unk_chars: e.reading.chars().count() as u32,
        }
    }

    /// 直前の文脈から `u` へ進むときの対数確率
    /// （語彙外の文字数罰は含まない。罰は `LmUnit::unk_chars` で別に数える）
    pub fn edge_logp(&self, prev: LmCtx, u: &LmUnit) -> f32 {
        self.logp(prev, u.first, u.first_class) + u.internal
    }

    /// 語列の対数確率。`left` は直前の文脈語（無ければ文頭 BOS）。
    /// 入力途中の文を評価するため文末 EOS は付けない。
    pub fn sequence_logp(&self, left: Option<&WordEntry>, words: &[WordEntry]) -> f32 {
        let (logp, unk_chars) = self.sequence_logp_parts(left, words);
        logp + self.params.unk_char_logp * unk_chars as f32
    }

    /// `sequence_logp` の内訳: (語彙外の文字数罰を除いた対数確率, 語彙外の語の読みの文字数)。
    /// パラメータ調整（`judge_eval`）で文字数罰の重みを振るために分けて返す。
    pub fn sequence_logp_parts(&self, left: Option<&WordEntry>, words: &[WordEntry]) -> (f32, u32) {
        let mut prev = match left {
            Some(e) => self.unit(e).last,
            None => LmCtx::BOS,
        };
        let mut total = 0.0f32;
        let mut unk_chars = 0u32;
        for e in words {
            let u = self.unit(e);
            total += self.edge_logp(prev, &u);
            unk_chars += u.unk_chars;
            prev = u.last;
        }
        (total, unk_chars)
    }

    /// LM 対数確率・辞書コスト・コロケーション一致数から総合スコアを出す
    pub fn combine(&self, lm_logp: f32, dict_cost: i32, seed_hits: u32) -> f32 {
        let p = self.params;
        p.lm_weight * lm_logp - dict_cost as f32 / p.dict_scale + p.seed_bonus * seed_hits as f32
    }

    /// スコア列を確率分布にする（softmax、温度つき）
    pub fn distribution(&self, scores: &[f32]) -> Vec<f32> {
        softmax(scores, self.params.temperature)
    }
}

/// 温度つき softmax（数値安定化のため最大値を引く）
pub fn softmax(scores: &[f32], temperature: f32) -> Vec<f32> {
    if scores.is_empty() {
        return Vec::new();
    }
    let t = if temperature > 0.0 { temperature } else { 1.0 };
    let max = scores.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = scores.iter().map(|s| ((s - max) / t).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

impl std::fmt::Debug for JudgeLm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeLm")
            .field("vocab", &self.data.vocab.len())
            .field("bigrams", &self.data.bi_next.len())
            .field("classes", &self.data.n_class)
            .field("params", &self.params)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(surface: &str, reading: &str, class: u16) -> WordEntry {
        WordEntry {
            surface: surface.to_string(),
            reading: reading.to_string(),
            left_id: class,
            right_id: class,
            cost: 0,
            pos: String::new(),
        }
    }

    // クラス: 0=文頭文末, 1=名詞, 2=助詞, 3=動詞連用形, 4=助動詞ます
    const N: u16 = 1;
    const P: u16 = 2;
    const V: u16 = 3;
    const M: u16 = 4;

    /// 語彙: BOS, EOS, 今日(名), は(助), 京(名), 晴れ(名), 買い(動), まし(助動)
    /// バイグラム: BOS→今日, 今日→は（京→は・買い→まし は無い＝バックオフ）
    fn tiny() -> JudgeLm {
        let words: [(&str, &str, u16); 8] = [
            ("<s>", "", 0),
            ("</s>", "", 0),
            ("今日", "きょう", N),
            ("は", "は", P),
            ("京", "きょう", N),
            ("晴れ", "はれ", N),
            ("買い", "かい", V),
            ("まし", "まし", M),
        ];
        let vocab: Vec<String> = words
            .iter()
            .enumerate()
            .map(|(i, (s, r, c))| if i < 2 { s.to_string() } else { vocab_key(s, r, *c) })
            .collect();
        // 既定は起こりにくい遷移、文法的な遷移だけ高くする
        let mut class_logp = vec![-8.0f32; 25];
        class_logp[0 * 5 + N as usize] = -0.5;
        class_logp[N as usize * 5 + P as usize] = -0.7;
        class_logp[V as usize * 5 + M as usize] = -0.3;
        let data = JudgeLmData {
            vocab,
            word_class: words.iter().map(|w| w.2).collect(),
            emit_logp: vec![0.0, 0.0, -5.0, -1.0, -9.0, -7.0, -6.0, -0.5],
            backoff: vec![-1.0, 0.0, -1.0, -1.0, -0.5, -1.0, -1.0, -1.0],
            // 0:BOS→{今日}, 1:EOS→{}, 2:今日→{は}, 3..7:{}
            bi_offsets: vec![0, 1, 1, 2, 2, 2, 2, 2, 2],
            bi_next: vec![2, 3],
            bi_logp: vec![-1.5, -0.3],
            n_class: 5,
            class_logp,
            unk_emit_logp: vec![-12.0; 5],
            params_json: String::new(),
        };
        JudgeLm::from_data(data)
    }

    #[test]
    fn bigram_hit_and_class_backoff() {
        let lm = tiny();
        assert_eq!(lm.logp(lm.ctx_of(2), Some(3), P), -0.3);
        // 京→は は未収録: backoff(京) + P(助詞|名詞) + P(は|助詞)
        assert_eq!(lm.logp(lm.ctx_of(4), Some(3), P), -0.5 + -0.7 + -1.0);
        // 語彙外の語はそのクラスの未知語として
        assert_eq!(lm.logp(lm.ctx_of(2), None, P), -1.0 + -0.7 + -12.0);
        // 直前が語彙外ならクラスモデルだけ
        assert_eq!(lm.logp(LmCtx { word: None, class: N }, Some(3), P), -0.7 + -1.0);
    }

    #[test]
    fn grammar_class_scores_unseen_word_pairs() {
        // 「買い→まし」は単語バイグラムに無いが、動詞連用形→助動詞ますは文法的に
        // 自然なので、クラスの水準で高く評価される
        let lm = tiny();
        let grammatical = lm.logp(lm.ctx_of(6), Some(7), M);
        let odd = lm.logp(lm.ctx_of(6), Some(5), N);
        assert!(grammatical > odd + 5.0, "grammatical={grammatical} odd={odd}");
    }

    #[test]
    fn context_prefers_attested_sequence() {
        let lm = tiny();
        let good = lm.sequence_logp(None, &[w("今日", "きょう", N), w("は", "は", P)]);
        let bad = lm.sequence_logp(None, &[w("京", "きょう", N), w("は", "は", P)]);
        assert!(good > bad, "good={good} bad={bad}");
    }

    #[test]
    fn unknown_compound_is_scored_by_its_parts() {
        let lm = tiny();
        // 「今日晴れ」は語彙に無いが「今日/きょう」「晴れ/はれ」に分解できる
        assert_eq!(lm.segment_ids("今日晴れ", "きょうはれ"), Some(vec![2, 5]));
        let u = lm.unit(&w("今日晴れ", "きょうはれ", N));
        assert_eq!((u.first, u.last.word, u.unk_chars), (Some(2), Some(5), 0));
        assert_eq!(
            lm.edge_logp(LmCtx::BOS, &u),
            lm.logp(LmCtx::BOS, Some(2), N) + lm.logp(lm.ctx_of(2), Some(5), N)
        );
        // 分解できたものは語彙外の罰を受けない
        let (_, unk) = lm.sequence_logp_parts(None, &[w("今日晴れ", "きょうはれ", N)]);
        assert_eq!(unk, 0);
        // 読みが合わない分解は採らない
        assert!(lm.segment_ids("今日晴れ", "こんにちはれ").is_none());
        assert_eq!(lm.unit(&w("今日晴れ", "こんにちはれ", N)).unk_chars, 6);
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_crash() {
        let dir = std::env::temp_dir().join(format!("judge_lm_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.bin");
        // 先頭の長さフィールドが巨大な値になるゴミ
        std::fs::write(&path, [0xFFu8; 64]).unwrap();
        assert!(JudgeLm::load(&path).is_err());
        // 正しく保存したものは読める
        let good = dir.join("good.bin");
        JudgeLm::save_data(tiny().data(), &good).unwrap();
        assert_eq!(JudgeLm::load(&good).unwrap().vocab_len(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn softmax_is_normalized_and_ordered() {
        let p = softmax(&[2.0, 1.0, -1.0], 1.0);
        let sum: f32 = p.iter().sum();
        assert!((sum - 1.0).abs() < 1e-5);
        assert!(p[0] > p[1] && p[1] > p[2]);
        // 温度を上げると分布が平らになる
        let flat = softmax(&[2.0, 1.0, -1.0], 10.0);
        assert!(flat[0] < p[0]);
    }
}
