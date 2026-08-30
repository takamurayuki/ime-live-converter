//! 「1文字漢字＋後続の漢字語」への断片化を辞書語で修復する後処理
//!
//! ライブ変換は打鍵のたびに読み全体を再変換するが、先頭の1〜2文字が
//! 学習ボーナス付きの1文字漢字（前・再・一・各・高 等）に変換された状態で
//! 後続が入力されると、Viterbi が「1文字漢字＋後続の漢字語」の断片パス
//! （前|核, 再|選炭, 一|欄, 高|佐久）を、辞書に実在する1語（全角, 最先端,
//! 一覧, 工作）より安く評価してしまうことがある。学習ボーナスは1文字漢字の
//! 単語コストを0近くまで下げるため、`scoring.rs` の接続コスト下限ガードだけ
//! では品詞の組み合わせごとにモグラ叩きになる（[[seed-override-pattern-limits]]）。
//!
//! ここでは接続コストの調整ではなく、1-best の結果そのものを検査する:
//! 1文字漢字（または読み2文字以下の短い漢字語。位置|欄 のように学習した
//! 短い語も同様に断片の先頭になる）の文節の直後に漢字を含む文節が続くとき、
//! - 結合した表記がそのまま辞書に載っているなら「意味のある複合語」として維持
//!   （左|側=左側, 二|階=二階）
//! - そうでなく、結合した読みに対応する辞書語があるなら、その辞書語に置換する
//!   （前|核→全角, 一|欄→一覧, 高|佐久→工作）
//!
//! 置換候補の安全条件（誤変換を別の誤変換で置き換えないため）:
//! - 表記2文字以上の変換語（廿 のような1文字語や、ひらがなのままの語は除く）
//! - 固有名詞・非内容語ではない
//! - 先頭が数詞・代名詞（三|県, 二|作, 何|X は生産的な組み合わせ）、または
//!   後続が接尾辞（二|重）の場合は、ユーザーが実際に確定した学習済みの語
//!   （一覧・一番 等）への置換に限る
//! - 学習の無い語への置換は、1文字あたりコストが妥当な範囲の語に限る
//!   （`is_implausible_content_word` の逆）

use super::*;

/// 修復対象にする文節数の上限（先頭の短い語 ＋ 後続の最大2文節）
const FRAGMENT_REPAIR_MAX_SPAN: usize = 3;

/// 断片の先頭になり得る短い語か: 1文字漢字（前・再・一・高）、または
/// 読みが2文字以下の漢字の名詞（位置(いち)・烏滸(おこ) 等。「いち」を
/// 「位置」と学習していると「いちらん」が「位置|欄」に割れる）。
/// 2文字以上の表記は名詞に限る（「無い|気」のような活用語＋名詞は文節の
/// 切れ目であって断片化ではない。内記 に置き換えてはいけない）。
fn is_fragment_head(e: &WordEntry) -> bool {
    if is_single_kanji_surface(&e.surface) {
        return true;
    }
    e.pos.starts_with("名詞") && contains_kanji(&e.surface) && e.reading.chars().count() <= 2
}

fn contains_kanji(surface: &str) -> bool {
    surface
        .chars()
        .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c))
}

impl ViterbiConverter {
    /// 1-best のパスから「1文字漢字＋後続の漢字語」の断片を辞書語に置換する。
    ///
    /// `convert_with_cost` が `extract_result` の直後に呼ぶ。総コストは
    /// ラティス上の最適パスのものをそのまま返す（誤字補正のコスト比較用で、
    /// 置換後の語のコストで置き換えると比較の基準がずれるため）。
    pub(crate) fn repair_single_kanji_fragments(&self, mut path: Vec<WordEntry>) -> Vec<WordEntry> {
        let mut i = 0;
        while i + 1 < path.len() {
            if is_fragment_head(&path[i]) {
                let max_span = FRAGMENT_REPAIR_MAX_SPAN.min(path.len() - i);
                // 長い範囲から試す（最先端 のように3文節に割れた語を優先して拾う）
                for span in (2..=max_span).rev() {
                    if let Some(word) = self.fragment_repair_word(&path[i..i + span]) {
                        path.splice(i..i + span, std::iter::once(word));
                        break;
                    }
                }
            }
            i += 1;
        }
        path
    }

    /// `group`（先頭は `is_fragment_head` の文節）を1語に置き換えるべき辞書語を返す。
    /// 置換不要・不可なら `None`。
    fn fragment_repair_word(&self, group: &[WordEntry]) -> Option<WordEntry> {
        let head = &group[0];
        let rest = &group[1..];
        // 後続に漢字を含む変換済み文節が無ければ対象外（助詞・ひらがな・
        // カタカナ化が続くのは断片化ではなく文節の切れ目）
        if !rest.iter().any(|e| contains_kanji(&e.surface)) {
            return None;
        }
        if rest.iter().any(|e| !is_content_pos(&e.pos)) {
            return None;
        }
        let reading: String = group.iter().map(|e| e.reading.as_str()).collect();
        let surface: String = group.iter().map(|e| e.surface.as_str()).collect();
        let alts = self.dictionary.lookup(&reading)?;
        // 結合した表記がそのまま辞書語なら「意味のある組み合わせ」なので触らない
        if alts.iter().any(|e| e.surface == surface) {
            return None;
        }
        // 数詞・代名詞＋名詞、名詞＋接尾辞は生産的な組み合わせ（三県・二作・
        // 何回・二重）なので、学習済みの語への置換だけを許す
        let restricted = head.pos.starts_with("名詞-数")
            || head.pos.starts_with("名詞-代名詞")
            || rest[0].pos.contains("接尾");

        let mut best: Option<(i32, i32, &WordEntry)> = None;
        for alt in alts {
            if alt.surface.chars().count() < 2 || alt.surface == alt.reading {
                continue;
            }
            if alt.pos.contains("固有名詞") || !is_content_pos(&alt.pos) {
                continue;
            }
            let key = (alt.reading.clone(), alt.surface.clone());
            let learned = self.learned_unigram.get(&key).copied().unwrap_or(0) > 0
                || self.trusted_phrase_bonus.get(&key).copied().unwrap_or(0) > 0;
            if !learned && (restricted || is_implausible_content_word(alt)) {
                continue;
            }
            let cand = (self.effective_word_cost(alt), alt.cost as i32, alt);
            if best.map_or(true, |b| (cand.0, cand.1) < (b.0, b.1)) {
                best = Some(cand);
            }
        }
        best.map(|(_, _, e)| e.clone())
    }
}
