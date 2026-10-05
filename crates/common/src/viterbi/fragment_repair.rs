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

/// 断片の先頭になり得る短い語か: 1文字漢字（前・再・一・高）、1文字の
/// カタカナ表記（ホ・セ 等。辞書に単独の読みとして実在し、後続の束縛
/// モーラ（ン 等）と組んで「本(ほん)」「線(せん)」のような2モーラ語を
/// 割ってしまう）、または読みが2文字以下の漢字の名詞（位置(いち)・
/// 烏滸(おこ) 等。「いち」を「位置」と学習していると「いちらん」が
/// 「位置|欄」に割れる）。2文字以上の表記は名詞に限る（「無い|気」の
/// ような活用語＋名詞は文節の切れ目であって断片化ではない。内記 に
/// 置き換えてはいけない）。
fn is_fragment_head(e: &WordEntry) -> bool {
    if is_single_kanji_surface(&e.surface) || is_single_katakana_surface(&e.surface) {
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
                        let before: String =
                            path[i..i + span].iter().map(|e| e.surface.as_str()).collect();
                        crate::debug_log!(
                            "fragment_repair: {}→{}（読み={}）",
                            before, word.surface, word.reading
                        );
                        path.splice(i..i + span, std::iter::once(word));
                        break;
                    }
                }
            }
            i += 1;
        }
        self.repair_surname_given_name_verb_collision(path)
    }

    /// 「人名-姓」+「人名-名」の断片が、実は活用語の連用形＋助詞
    /// （行ける+か 等）の読みを跨いで分割してしまったものかを検査し、
    /// 該当すれば [活用語, 助詞] に置き換える。
    ///
    /// IPA辞書の姓→名の接続コストは実際のフルネームの統計的頻度を反映して
    /// 極端に有利（実測-7009）なため、姓の読みが偶然、活用語の連用形と
    /// 一致し、かつ名の読みの一部が助詞と一致すると、正しい分割
    /// （行ける+か）より姓+名の断片解釈（池+ルカ）が安くなってしまう
    /// （実測:「いけるか」→「池ルカ」、「たべるか」→「田部ルカ」）。
    /// 名の読みの残り全体が実在の助詞になる場合に限定し（活用語側も
    /// 実在の動詞/形容詞であることを要求）、本物の姓名入力（結合読みが
    /// たまたま他の語と一致しない）には影響しない。
    pub(crate) fn repair_surname_given_name_verb_collision(
        &self,
        mut path: Vec<WordEntry>,
    ) -> Vec<WordEntry> {
        let mut i = 0;
        while i + 1 < path.len() {
            let is_pair = path[i].pos.starts_with("名詞-固有名詞-人名-姓")
                && path[i + 1].pos.starts_with("名詞-固有名詞-人名-名");
            if is_pair {
                if let Some((verb, particle)) =
                    self.find_verb_particle_split(&path[i], &path[i + 1])
                {
                    crate::debug_log!(
                        "fragment_repair(姓名衝突): {}{}→{}{}",
                        path[i].surface, path[i + 1].surface, verb.surface, particle.surface
                    );
                    path.splice(i..i + 2, [verb, particle]);
                }
            }
            i += 1;
        }
        path
    }

    /// `surname`＋`given_name`の結合読みを、末尾から1文字ずつ助詞として
    /// 切り出しながら、残りが実在の動詞/形容詞になる分割を探す。
    fn find_verb_particle_split(
        &self,
        surname: &WordEntry,
        given_name: &WordEntry,
    ) -> Option<(WordEntry, WordEntry)> {
        let given_chars: Vec<char> = given_name.reading.chars().collect();
        for k in (1..given_chars.len()).rev() {
            let verb_reading: String = surname
                .reading
                .chars()
                .chain(given_chars[..k].iter().copied())
                .collect();
            let particle_reading: String = given_chars[k..].iter().collect();
            let Some(particle_entries) = self.merged_lookup(&particle_reading) else {
                continue;
            };
            let Some(particle) = particle_entries
                .iter()
                .find(|e| e.pos.starts_with("助詞") && e.surface == e.reading)
                .cloned()
            else {
                continue;
            };
            let verb_alts = self.merged_lookup(&verb_reading);
            let best_verb = verb_alts
                .iter()
                .flat_map(|c| c.iter())
                .filter(|e| e.pos.starts_with("動詞") || e.pos.starts_with("形容詞"))
                .filter(|e| !is_implausible_content_word(e))
                .min_by_key(|e| self.effective_word_cost(e));
            if let Some(verb) = best_verb {
                return Some((verb.clone(), particle));
            }
        }
        None
    }

    /// `group`（先頭は `is_fragment_head` の文節）を1語に置き換えるべき辞書語を返す。
    /// 置換不要・不可なら `None`。
    fn fragment_repair_word(&self, group: &[WordEntry]) -> Option<WordEntry> {
        let head = &group[0];
        let rest = &group[1..];
        // 後続に漢字を含む変換済み文節が続くか、または「ん」等の1モーラの
        // 非自立名詞（束縛形態素）が単独で続く（本|ん→本, 線|ん→線 のように、
        // 先頭の1文字漢字が読みの1モーラを奪い、残りのモーラが助詞の直前で
        // 非自立名詞として単独文節になったもの）場合だけ対象にする。
        // それ以外（助詞・ひらがな・カタカナ化が続く）は文節の切れ目であって
        // 断片化ではないので対象外。
        let tail_is_kanji_word = rest.iter().any(|e| contains_kanji(&e.surface));
        let tail_is_bound_mora = rest.len() == 1 && is_bound_mora_tail(&rest[0]);
        let tail_is_single_content_word = rest.len() == 1 && is_content_pos(&rest[0].pos);
        if !tail_is_kanji_word && !tail_is_bound_mora && !tail_is_single_content_word {
            return None;
        }
        if rest.iter().any(|e| !is_content_pos(&e.pos)) {
            return None;
        }
        let reading: String = group.iter().map(|e| e.reading.as_str()).collect();
        let surface: String = group.iter().map(|e| e.surface.as_str()).collect();
        let alts = self.merged_lookup(&reading)?;
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
        for alt in alts.iter() {
            if alt.surface == alt.reading {
                continue;
            }
            if alt.pos.contains("固有名詞") || !is_content_pos(&alt.pos) {
                continue;
            }
            let key = (alt.reading.clone(), alt.surface.clone());
            let learned = self.learned_unigram.get(&key).copied().unwrap_or(0) > 0
                || self.trusted_phrase_bonus.get(&key).copied().unwrap_or(0) > 0;
            // 表記1文字の候補（水・本・線 等）は、IPA辞書のコスト/文字数だけ
            // では希少な当て字（廿=にじゅう 等）と見分けが付かない（実測で
            // 両者ともplausibleの閾値を通ってしまう）。学習・シード済み
            // （＝`COMMON_WORD_SEED`等で人手または実利用で安全性を確認済み）
            // の語に限って許可することで、この既存の信頼リストをそのまま
            // 安全装置として使う。
            if alt.surface.chars().count() < 2 && !learned {
                continue;
            }
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

/// 読み1モーラの束縛的な形態素（「ん」等）で、表記もそのままか判定。
/// 先頭の1文字漢字/カタカナに読みの1モーラを奪われ、残りのモーラだけが
/// 助詞の直前で単独文節になった典型形（本(ほん)→穂+ん、線(せん)→畝+ん、
/// 空(そら)→ソ+ら）を狙い撃ちするため、読みが1文字（1モーラ）の場合だけに
/// 限定する。対象の品詞は「名詞-非自立」（〜んです等）に加え「名詞-接尾」
/// （ら等。単独では使われず、常に何かに付属する点で非自立と同じ性質）・
/// 「助動詞」（行かん等）も含む。いずれも同型の断片化が実測で確認されて
/// いる（POSバリアントごとのもぐら叩きを避けるため、性質で一般化している）。
fn is_bound_mora_tail(e: &WordEntry) -> bool {
    let is_dependent_pos = e.pos.starts_with("名詞-非自立")
        || e.pos.starts_with("名詞-接尾")
        || e.pos.starts_with("助動詞");
    // 表記がひらがな（例: ん）だけでなく、同じ束縛モーラのカタカナ表記
    // （例: ん→ン）も対象にする。IPA辞書には同じ読み・同じ品詞で
    // ひらがな/カタカナ両方の表記が別エントリとして存在し、カタカナ側
    // だけを除外すると同じ断片化パターンをすり抜ける（実測: 本(ほん)→
    // 穂+ん は直っても、ホ+ン は直らなかった）。
    is_dependent_pos
        && e.reading.chars().count() == 1
        && (e.surface == e.reading || e.surface == hiragana_to_katakana(&e.reading))
}
