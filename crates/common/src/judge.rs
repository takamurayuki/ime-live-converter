//! 判断層（judge-lm）: N-best 変換候補を統計言語モデルで再評価し、
//! 候補上の確率分布を返す。
//!
//! Jev（TypeSafe AI の「生成しない判断モデル」）と同じ考え方で、文章を
//! 生成せず「与えられた選択肢のどれが尤もらしいか」の較正済み確率だけを
//! 出す。中身は日本語 Wikipedia を IPADic で分かち書きして学習した
//! Kneser-Ney 平滑化つき単語バイグラム（`dict-builder judge-train` で構築）。
//! 計算は完全に決定論的で、実行時はメモリ上の表引きだけ（外部通信なし）。
//!
//! 単語の単位は (表記, 読み) の組。IME の辞書（system.dic）も学習時の
//! 分かち書き（vibrato + IPADic）も同じ IPADic 体系なので、活用語尾・
//! 助詞の切れ目がほぼ一致する。
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
    /// 語彙（"表記\t読み"）。添字が語ID。0/1 は BOS/EOS
    pub vocab: Vec<String>,
    /// 各語のユニグラム（継続確率）の対数確率
    pub uni_logp: Vec<f32>,
    /// 各語を前文脈としたときのバックオフ重みの対数
    pub backoff: Vec<f32>,
    pub bi_offsets: Vec<u32>,
    pub bi_next: Vec<u32>,
    pub bi_logp: Vec<f32>,
    /// 語彙外の語に与える対数確率（バックオフ重みに足して使う）
    pub unk_logp: f32,
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
    /// を LM の語彙単位に分解して採点するための索引（`segment`）
    by_surface: HashMap<String, Vec<(String, u32)>>,
}

/// 分解を試みる表記の最大文字数・1片の最大文字数
const SEGMENT_MAX_CHARS: usize = 16;
const SEGMENT_MAX_PIECE: usize = 8;

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
            if let Some((surface, reading)) = k.split_once('\t') {
                by_surface
                    .entry(surface.to_string())
                    .or_default()
                    .push((reading.to_string(), i as u32));
            }
        }
        let params = serde_json::from_str(&data.params_json).unwrap_or_default();
        Self { data, params, index, by_surface }
    }

    /// 語彙に無い (表記, 読み) を、語彙内の語の連なりに分解して採点する
    /// （表記と読みを同時に切り分ける動的計画法。直前の語 `prev` からの連鎖で
    /// 対数確率が最大になる分解を選ぶ）。戻り値: (対数確率, 最後の語ID)。
    /// 分解できなければ None（本当の語彙外）。
    pub fn segment(&self, prev: Option<u32>, surface: &str, reading: &str) -> Option<(f32, u32)> {
        let s: Vec<char> = surface.chars().collect();
        let r: Vec<char> = reading.chars().collect();
        if s.len() < 2 || s.len() > SEGMENT_MAX_CHARS {
            return None;
        }
        // best[i][j] = 表記 i 文字・読み j 文字まで消費したときの (対数確率, 最後の語ID)
        let mut best: Vec<Vec<Option<(f32, Option<u32>)>>> = vec![vec![None; r.len() + 1]; s.len() + 1];
        best[0][0] = Some((0.0, prev));
        for i in 0..s.len() {
            for j in 0..r.len() {
                let Some((score, last)) = best[i][j] else { continue };
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
                        let next = score + self.logp(last, Some(*id));
                        let slot = &mut best[i + a][j + pr_len];
                        if slot.map_or(true, |(b, _)| next > b) {
                            *slot = Some((next, Some(*id)));
                        }
                    }
                }
            }
        }
        best[s.len()][r.len()].and_then(|(score, last)| last.map(|l| (score, l)))
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
        anyhow::ensure!(
            data.vocab.len() == data.uni_logp.len()
                && data.vocab.len() == data.backoff.len()
                && data.bi_offsets.len() == data.vocab.len() + 1
                && data.bi_next.len() == data.bi_logp.len(),
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

    /// (表記, 読み) の語ID。語彙外なら None
    pub fn word_id(&self, surface: &str, reading: &str) -> Option<u32> {
        let mut key = String::with_capacity(surface.len() + reading.len() + 1);
        key.push_str(surface);
        key.push('\t');
        key.push_str(reading);
        self.index.get(&key).copied()
    }

    /// log P(w | v)。v が None（語彙外の前文脈）ならユニグラムに落とす
    pub fn logp(&self, v: Option<u32>, w: Option<u32>) -> f32 {
        let d = &self.data;
        match (v, w) {
            (Some(v), Some(w)) => {
                let (s, e) = (d.bi_offsets[v as usize] as usize, d.bi_offsets[v as usize + 1] as usize);
                match d.bi_next[s..e].binary_search(&w) {
                    Ok(i) => d.bi_logp[s + i],
                    Err(_) => d.backoff[v as usize] + d.uni_logp[w as usize],
                }
            }
            (None, Some(w)) => d.uni_logp[w as usize],
            (Some(v), None) => d.backoff[v as usize] + d.unk_logp,
            (None, None) => d.unk_logp,
        }
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
            Some(e) => self.word_id(&e.surface, &e.reading),
            None => Some(BOS_ID),
        };
        let mut total = 0.0f32;
        let mut unk_chars = 0u32;
        for e in words {
            let cur = self.word_id(&e.surface, &e.reading);
            if cur.is_none() {
                if let Some((logp, last)) = self.segment(prev, &e.surface, &e.reading) {
                    total += logp;
                    prev = Some(last);
                    continue;
                }
            }
            total += self.logp(prev, cur);
            if cur.is_none() {
                unk_chars += e.reading.chars().count() as u32;
            }
            prev = cur;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn w(surface: &str, reading: &str) -> WordEntry {
        WordEntry {
            surface: surface.to_string(),
            reading: reading.to_string(),
            left_id: 0,
            right_id: 0,
            cost: 0,
            pos: String::new(),
        }
    }

    /// 語彙: BOS, EOS, 今日, は, 京, 晴れ
    /// バイグラム: BOS→今日, 今日→は（京→は は無い＝バックオフ）
    fn tiny() -> JudgeLm {
        let vocab = vec!["<s>", "</s>", "今日\tきょう", "は\tは", "京\tきょう", "晴れ\tはれ"]
            .into_iter()
            .map(String::from)
            .collect();
        let data = JudgeLmData {
            vocab,
            uni_logp: vec![-9.0, -3.0, -5.0, -2.0, -8.0, -7.0],
            backoff: vec![-1.0, 0.0, -1.0, -1.0, -0.5, -1.0],
            // 0:BOS→{今日}, 1:EOS→{}, 2:今日→{は}, 3..5:{}
            bi_offsets: vec![0, 1, 1, 2, 2, 2, 2],
            bi_next: vec![2, 3],
            bi_logp: vec![-1.5, -0.3],
            unk_logp: -15.0,
            params_json: String::new(),
        };
        JudgeLm::from_data(data)
    }

    #[test]
    fn bigram_hit_and_backoff() {
        let lm = tiny();
        assert_eq!(lm.logp(Some(2), Some(3)), -0.3);
        // 京→は は未収録: backoff(京) + uni(は)
        assert_eq!(lm.logp(Some(4), Some(3)), -0.5 + -2.0);
        // 語彙外
        assert_eq!(lm.logp(Some(2), None), -1.0 + -15.0);
    }

    #[test]
    fn context_prefers_attested_sequence() {
        let lm = tiny();
        let good = lm.sequence_logp(None, &[w("今日", "きょう"), w("は", "は")]);
        let bad = lm.sequence_logp(None, &[w("京", "きょう"), w("は", "は")]);
        assert!(good > bad, "good={good} bad={bad}");
    }

    #[test]
    fn unknown_compound_is_scored_by_its_parts() {
        let lm = tiny();
        // 「今日晴れ」は語彙に無いが「今日/きょう」「晴れ/はれ」に分解できる
        let (logp, last) = lm.segment(Some(BOS_ID), "今日晴れ", "きょうはれ").unwrap();
        assert_eq!(last, 5);
        assert_eq!(logp, lm.logp(Some(BOS_ID), Some(2)) + lm.logp(Some(2), Some(5)));
        // 分解できたものは語彙外の罰を受けない
        let (_, unk) = lm.sequence_logp_parts(None, &[w("今日晴れ", "きょうはれ")]);
        assert_eq!(unk, 0);
        // 読みが合わない分解は採らない
        assert!(lm.segment(None, "今日晴れ", "こんにちはれ").is_none());
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
        assert_eq!(JudgeLm::load(&good).unwrap().vocab_len(), 6);
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

impl std::fmt::Debug for JudgeLm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeLm")
            .field("vocab", &self.data.vocab.len())
            .field("bigrams", &self.data.bi_next.len())
            .field("params", &self.params)
            .finish()
    }
}
