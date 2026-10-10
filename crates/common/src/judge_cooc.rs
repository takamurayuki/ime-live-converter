//! 判断層（judge-lm）の共起モデル: 同じ文の中に一緒に出てくる名詞どうしの
//! 結びつきの強さ（自己相互情報量, PMI）。
//!
//! トライグラムは直前2語しか見ないため、「駅のホームで待っていた**きしゃ**」の
//! 「駅」「ホーム」のように離れた語は汽車/記者の判断に届かない。ここでは
//! Wikipedia の各文について「同音異義のある名詞（対象）」と「同じ文の他の名詞
//! （文脈）」の組を数え、
//!
//!   PMI(文脈 c, 対象 t) = log( P(c と t が同じ文に出る) / (P(c)·P(t)) )
//!
//! を持つ。正なら「c がある文では t が普段より出やすい」。判断層は候補中の
//! 対象語ごとに、文中の他の名詞と直前に確定した文の名詞の中で最も強い PMI を
//! 加点する（`ViterbiConverter::path_cooc_bonus`）。`dict-builder judge-cooc` で
//! 構築し、`judge_cooc.bin` が無ければ何もしない（判断層本体とは独立に外せる）。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

fn is_kanji(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c) || c == '々'
}

/// 共起の文脈にする名詞か（IPADic の品詞の大分類・細分類1と表記で判定）。
/// 学習（vibrato の素性）と実行時（IME 辞書の品詞）で同じ判定を使う。
/// ひらがなだけの語は結びつきが曖昧なので除く。1文字の語は漢字1字の名詞
/// （駅・車・船 等。話題の手がかりとして強い）だけ文脈に使う。
pub fn is_cooc_noun(pos1: &str, pos2: &str, surface: &str) -> bool {
    let n = surface.chars().count();
    pos1 == "名詞"
        && matches!(pos2, "一般" | "サ変接続" | "固有名詞" | "形容動詞語幹" | "副詞可能" | "ナイ形容詞語幹")
        && n >= 1
        && if n == 1 {
            surface.chars().all(is_kanji)
        } else {
            surface.chars().any(|c| is_kanji(c) || ('\u{30A1}'..='\u{30FA}').contains(&c))
        }
}

/// 共起の対象（加点される側）にする名詞か。文脈の条件に加えて2文字以上に限る
/// （1文字の漢字に加点すると、読みを1文字ずつに割る誤変換を後押ししかねない）。
pub fn is_cooc_target(pos1: &str, pos2: &str, surface: &str) -> bool {
    is_cooc_noun(pos1, pos2, surface) && surface.chars().count() >= 2
}

fn split_pos(pos: &str) -> (&str, &str) {
    let mut it = pos.split('-');
    (it.next().unwrap_or(""), it.next().unwrap_or(""))
}

/// IME 辞書の品詞文字列（"名詞-一般-*-*"）で `is_cooc_noun` を判定する
pub fn is_cooc_noun_entry(pos: &str, surface: &str) -> bool {
    let (p1, p2) = split_pos(pos);
    is_cooc_noun(p1, p2, surface)
}

/// IME 辞書の品詞文字列で `is_cooc_target` を判定する
pub fn is_cooc_target_entry(pos: &str, surface: &str) -> bool {
    let (p1, p2) = split_pos(pos);
    is_cooc_target(p1, p2, surface)
}

/// ディスク上の形式（bincode）。対象語ごとの CSR（`offsets[t]..offsets[t+1]` が
/// 対象 t の文脈語の範囲、`ctx` は昇順）。
#[derive(Serialize, Deserialize, Default)]
pub struct JudgeCoocData {
    /// 名詞の表記。添字が名詞ID
    pub nouns: Vec<String>,
    pub offsets: Vec<u32>,
    pub ctx: Vec<u32>,
    pub pmi: Vec<f32>,
}

/// 実行時の共起モデル
pub struct JudgeCooc {
    data: JudgeCoocData,
    index: HashMap<String, u32>,
}

impl JudgeCooc {
    pub fn from_data(data: JudgeCoocData) -> Self {
        let index = data.nouns.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect();
        Self { data, index }
    }

    pub fn load(path: &Path) -> Result<Self> {
        use bincode::Options;
        let file = File::open(path)?;
        // judge_lm.bin と同じく、壊れたファイルで巨大な確保をして落ちないよう制限する
        let limit = file.metadata()?.len() + 1024;
        let data: JudgeCoocData = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .allow_trailing_bytes()
            .with_limit(limit)
            .deserialize_from(BufReader::with_capacity(1 << 20, file))?;
        let n = data.nouns.len();
        anyhow::ensure!(
            data.offsets.len() == n + 1
                && data.ctx.len() == data.pmi.len()
                && data.offsets.last().map_or(false, |&e| e as usize == data.ctx.len())
                && data.offsets.windows(2).all(|w| w[0] <= w[1])
                && data.ctx.iter().all(|&c| (c as usize) < n),
            "judge_cooc の形式が不正です"
        );
        Ok(Self::from_data(data))
    }

    pub fn save_data(data: &JudgeCoocData, path: &Path) -> Result<()> {
        let file = File::create(path)?;
        bincode::serialize_into(BufWriter::with_capacity(1 << 20, file), data)?;
        Ok(())
    }

    pub fn noun_id(&self, surface: &str) -> Option<u32> {
        self.index.get(surface).copied()
    }

    pub fn noun_count(&self) -> usize {
        self.data.nouns.len()
    }

    pub fn pair_count(&self) -> usize {
        self.data.ctx.len()
    }

    /// 対象語 `target` が文脈語 `ctx` と一緒に出るときの PMI（記録が無ければ None）
    pub fn pmi(&self, target: u32, ctx: u32) -> Option<f32> {
        let d = &self.data;
        let (s, e) = (d.offsets[target as usize] as usize, d.offsets[target as usize + 1] as usize);
        d.ctx[s..e].binary_search(&ctx).ok().map(|i| d.pmi[s + i])
    }

    /// 対象語が文脈語の集まりの中で最も強く結びつく PMI（0〜`cap` に収める）。
    /// 対象語が共起の記録を持たなければ 0。
    pub fn assoc(&self, target: &str, ctx: &[u32], cap: f32) -> f32 {
        let Some(t) = self.noun_id(target) else { return 0.0 };
        ctx.iter()
            .filter(|&&c| c != t)
            .filter_map(|&c| self.pmi(t, c))
            .fold(0.0f32, f32::max)
            .min(cap)
    }
}

impl std::fmt::Debug for JudgeCooc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeCooc")
            .field("nouns", &self.data.nouns.len())
            .field("pairs", &self.data.ctx.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny() -> JudgeCooc {
        // 名詞: 0=汽車 1=記者 2=駅 3=新聞
        // 汽車: 駅と強く結びつく / 記者: 新聞と強く結びつく
        JudgeCooc::from_data(JudgeCoocData {
            nouns: ["汽車", "記者", "駅", "新聞"].iter().map(|s| s.to_string()).collect(),
            offsets: vec![0, 1, 2, 2, 2],
            ctx: vec![2, 3],
            pmi: vec![3.0, 2.5],
        })
    }

    #[test]
    fn assoc_picks_the_word_that_fits_the_context() {
        let c = tiny();
        let station = [c.noun_id("駅").unwrap()];
        let paper = [c.noun_id("新聞").unwrap()];
        assert!(c.assoc("汽車", &station, 5.0) > c.assoc("記者", &station, 5.0));
        assert!(c.assoc("記者", &paper, 5.0) > c.assoc("汽車", &paper, 5.0));
        // 上限で頭打ち、記録が無い語は 0
        assert_eq!(c.assoc("汽車", &station, 1.0), 1.0);
        assert_eq!(c.assoc("未知語", &station, 5.0), 0.0);
    }

    #[test]
    fn noun_filter() {
        assert!(is_cooc_noun_entry("名詞-一般-*-*", "汽車"));
        assert!(is_cooc_noun_entry("名詞-サ変接続-*-*", "政策"));
        assert!(!is_cooc_noun_entry("名詞-非自立-*-*", "事"));
        // 漢字1字の名詞は文脈には使うが、対象にはしない
        assert!(is_cooc_noun_entry("名詞-一般-*-*", "駅"));
        assert!(!is_cooc_target_entry("名詞-一般-*-*", "駅"));
        assert!(is_cooc_target_entry("名詞-一般-*-*", "汽車"));
        assert!(!is_cooc_noun_entry("名詞-一般-*-*", "ミ")); // カタカナ1字
        assert!(!is_cooc_noun_entry("動詞-自立-*-*", "走る"));
        assert!(!is_cooc_noun_entry("名詞-一般-*-*", "こころ")); // ひらがなだけ
    }
}
