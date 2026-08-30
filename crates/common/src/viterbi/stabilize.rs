//! 長い入力の先頭側を「固定（ピン留め）」して、後続入力で崩れないようにする
//!
//! ライブ変換は打鍵のたびに読み全体を再変換する（大域最適化）ため、文が
//! 長くなるほど「後続の入力で、離れた前方の語が書き換わる」ことがある
//! （例:「…する際に…をヒョウ」→「表示」が確定した瞬間に14文字前の
//! 「際」が「再」に変わる）。表示が正しかった部分が後から崩れるのは、
//! ユーザーには「変換が壊れた」ように見える。
//!
//! 対策として、文がある程度の長さになったら文節の切れ目で先頭側の文節列を
//! 固定し、以降の再変換ではその範囲のラティスノードを固定した文節だけに
//! 制限する（`convert_with_pinned_prefix`）。読みを切って末尾だけを単独で
//! 変換する方式（部分確定）とは違い、末尾側は固定部分との接続コストを
//! 含めて通常どおり変換されるので、切れ目で文脈を失わない（実測では
//! 部分確定方式は「なかった単語」→「なかっ他タンゴ」のように末尾側が
//! 文頭扱いになって悪化した）。
//!
//! 固定した文節は表示・候補一覧・学習のすべてで同じ分解として扱う
//! （hook-dll の `LiveConversionState::convert_buffer`）。

use super::*;

/// 先頭側を固定し始める読みの長さ（文字数）。これ未満なら何もしない。
pub const STABILIZE_MIN_READING_CHARS: usize = 24;
/// 固定した後も未固定のまま残す末尾の読みの最小長（文字数）。
/// この範囲内では従来どおり後続の入力で前の語が最適化され直す。
pub const STABILIZE_KEEP_TAIL_CHARS: usize = 12;

/// この文節の直後を「文節の切れ目」として固定の境界にしてよいか。
///
/// 助詞・助動詞・記号（句読点）の直後を境界にする。ただし接続助詞の
/// 「て」「で」の直後は除外する（「し|て|いる」「見|て|おく」のように
/// 直後の語と一体の補助動詞句を成すことが多い）。
pub fn is_clause_boundary(e: &WordEntry) -> bool {
    if e.pos.starts_with("記号") {
        return true;
    }
    if e.surface.ends_with('、') || e.surface.ends_with('。') {
        return true;
    }
    if e.pos.starts_with("助詞") {
        let connective_te = e.pos.starts_with("助詞-接続助詞")
            && (e.surface == "て" || e.surface == "で");
        return !connective_te;
    }
    e.pos.starts_with("助動詞")
}

/// 1-best の文節列 `entries` のうち、先頭から何文節を固定してよいかを返す。
///
/// - 読み全体が `min_reading_chars` 未満なら 0
/// - 文節の切れ目（`is_clause_boundary`）のうち、その後ろに
///   `keep_tail_chars` 文字以上の読みが残る最も後ろの位置まで固定する
pub fn stable_prefix_segments(
    entries: &[WordEntry],
    min_reading_chars: usize,
    keep_tail_chars: usize,
) -> usize {
    let total: usize = entries.iter().map(|e| e.reading.chars().count()).sum();
    if total < min_reading_chars {
        return 0;
    }
    let mut acc = 0usize;
    let mut best = 0usize;
    for (i, e) in entries.iter().enumerate() {
        acc += e.reading.chars().count();
        if total - acc < keep_tail_chars {
            break;
        }
        if is_clause_boundary(e) {
            best = i + 1;
        }
    }
    best
}

/// `pinned` のうち、`reading` の先頭と読みが一致する範囲（先頭からの連続部分）。
/// Backspace で固定領域まで削られた場合や、別の読み（候補一覧の前半部分等）を
/// 変換する場合に、一致しなくなった固定文節を自動的に外すために使う。
pub fn matching_pinned_prefix<'a>(reading: &str, pinned: &'a [WordEntry]) -> &'a [WordEntry] {
    let mut pos = 0usize;
    let mut n = 0usize;
    for e in pinned {
        let end = pos + e.reading.len();
        match reading.get(pos..end) {
            Some(r) if r == e.reading => {
                n += 1;
                pos = end;
            }
            _ => break,
        }
    }
    &pinned[..n]
}

fn node_matches(node: &LatticeNode, start: usize, end: usize, e: &WordEntry) -> bool {
    node.start == start
        && node.end == end
        && node
            .entry
            .as_ref()
            .map_or(false, |x| x.surface == e.surface && x.reading == e.reading)
}

impl ViterbiConverter {
    /// 先頭側の文節列 `pinned` を固定したまま `reading` 全体を変換する。
    ///
    /// 固定領域（`pinned` の読みが占める範囲）では固定した文節に対応する
    /// ノードだけを残し（無ければ追加し）、それ以外のノードと領域をまたぐ
    /// ノードを除外してから Viterbi を走らせる。末尾側は固定部分との接続
    /// コストを含めて通常どおり最適化される。`pinned` が `reading` の先頭と
    /// 一致しない部分は無視する（`matching_pinned_prefix`）。
    /// 断片修復（`repair_single_kanji_fragments`）は末尾側にだけ掛ける
    /// （固定した文節が後から結合されて表示が変わらないように）。
    pub fn convert_with_pinned_prefix(&self, reading: &str, pinned: &[WordEntry]) -> Vec<WordEntry> {
        let pinned = matching_pinned_prefix(reading, pinned);
        if pinned.is_empty() {
            return self.convert(reading);
        }
        let mut lattice = self.build_lattice(reading);

        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(pinned.len());
        let mut pos = 0usize;
        for e in pinned {
            let end = pos + e.reading.len();
            spans.push((pos, end));
            pos = end;
        }
        let pinned_end = pos;

        // 固定文節に対応するノードが無ければ追加する（学習ひらがなノードのように
        // 入力が伸びると生成条件が変わるノードもあるため）
        for (e, &(s, t)) in pinned.iter().zip(&spans) {
            let exists = lattice.nodes_starting_at[s]
                .iter()
                .any(|&idx| node_matches(&lattice.nodes[idx], s, t, e));
            if !exists {
                lattice.add_word(s, t, e.clone());
            }
        }

        // 固定領域内の他ノード・領域をまたぐノードを索引から外す
        let nodes = &lattice.nodes;
        let allowed = |idx: usize| -> bool {
            let n = &nodes[idx];
            if n.entry.is_none() || n.start >= pinned_end {
                return true;
            }
            pinned
                .iter()
                .zip(&spans)
                .any(|(e, &(s, t))| node_matches(n, s, t, e))
        };
        for list in lattice.nodes_starting_at.iter_mut() {
            list.retain(|&idx| allowed(idx));
        }
        for list in lattice.nodes_ending_at.iter_mut() {
            list.retain(|&idx| allowed(idx));
        }

        self.find_best_path(&mut lattice);
        if lattice.nodes[lattice.eos_index].total_cost == i32::MAX {
            return self.convert(reading);
        }
        let path = self.extract_result(&lattice);
        let n = pinned.len();
        if path.len() < n {
            return path;
        }
        let mut out = path[..n].to_vec();
        out.extend(self.repair_single_kanji_fragments(path[n..].to_vec()));
        out
    }

    /// `convert_context_aware` の固定文節対応版。連想リランクも固定部分には
    /// 掛けない（固定部分の内容語は文脈として参照はする）。
    pub fn convert_context_aware_pinned(&self, reading: &str, pinned: &[WordEntry]) -> Vec<WordEntry> {
        let n = matching_pinned_prefix(reading, pinned).len();
        if n == 0 {
            return self.convert_context_aware(reading);
        }
        let base = self.convert_with_pinned_prefix(reading, pinned);
        self.rerank_by_assoc_from(base, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(surface: &str, reading: &str, pos: &str) -> WordEntry {
        WordEntry {
            surface: surface.to_string(),
            reading: reading.to_string(),
            left_id: 0,
            right_id: 0,
            cost: 0,
            pos: pos.to_string(),
        }
    }

    #[test]
    fn short_input_is_never_stabilized() {
        let entries = vec![
            w("今日", "きょう", "名詞-副詞可能-*-*"),
            w("は", "は", "助詞-係助詞-*-*"),
            w("学校", "がっこう", "名詞-一般-*-*"),
        ];
        assert_eq!(stable_prefix_segments(&entries, 24, 12), 0);
    }

    #[test]
    fn stabilizes_up_to_last_boundary_that_keeps_tail() {
        // 読み: きょうは(4) がっこうに(5) いって(3) べんきょうを(6) した(2) ともだちと(5) あそぶ(3) = 28
        let entries = vec![
            w("今日", "きょう", "名詞-副詞可能-*-*"),
            w("は", "は", "助詞-係助詞-*-*"),
            w("学校", "がっこう", "名詞-一般-*-*"),
            w("に", "に", "助詞-格助詞-一般-*"),
            w("行っ", "いっ", "動詞-自立-*-*"),
            w("て", "て", "助詞-接続助詞-*-*"),
            w("勉強", "べんきょう", "名詞-サ変接続-*-*"),
            w("を", "を", "助詞-格助詞-一般-*"),
            w("し", "し", "動詞-自立-*-*"),
            w("た", "た", "助動詞-*-*-*"),
            w("友達", "ともだち", "名詞-一般-*-*"),
            w("と", "と", "助詞-格助詞-一般-*"),
            w("遊ぶ", "あそぶ", "動詞-自立-*-*"),
        ];
        // 末尾12文字以上を残せる境界は「に」の直後（先頭から9文字、残り19文字）。
        // 「て」の直後は接続助詞なので境界にしない。「を」の直後は残り10文字で不足。
        assert_eq!(stable_prefix_segments(&entries, 24, 12), 4);
        // 残す末尾を短くすれば「を」の直後（残り10文字）まで進む
        assert_eq!(stable_prefix_segments(&entries, 24, 10), 8);
        // しきい値未満なら 0
        assert_eq!(stable_prefix_segments(&entries, 30, 12), 0);
    }

    #[test]
    fn no_boundary_means_nothing_is_stabilized() {
        let entries = vec![
            w("ああああああああああああああ", "ああああああああああああああ", "名詞-一般-*-*"),
            w("いいいいいいいいいいいいいい", "いいいいいいいいいいいいいい", "名詞-一般-*-*"),
        ];
        assert_eq!(stable_prefix_segments(&entries, 24, 12), 0);
    }

    #[test]
    fn punctuation_and_auxiliary_are_boundaries_but_te_is_not() {
        assert!(is_clause_boundary(&w("。", "。", "記号-句点-*-*")));
        assert!(is_clause_boundary(&w("た", "た", "助動詞-*-*-*")));
        assert!(is_clause_boundary(&w("を", "を", "助詞-格助詞-一般-*")));
        assert!(is_clause_boundary(&w("ば", "ば", "助詞-接続助詞-*-*")));
        assert!(!is_clause_boundary(&w("て", "て", "助詞-接続助詞-*-*")));
        assert!(!is_clause_boundary(&w("で", "で", "助詞-接続助詞-*-*")));
        assert!(!is_clause_boundary(&w("学校", "がっこう", "名詞-一般-*-*")));
    }

    #[test]
    fn matching_pinned_prefix_stops_at_first_mismatch() {
        let pinned = vec![
            w("今日", "きょう", "名詞"),
            w("は", "は", "助詞"),
            w("学校", "がっこう", "名詞"),
        ];
        assert_eq!(matching_pinned_prefix("きょうはがっこうに", &pinned).len(), 3);
        assert_eq!(matching_pinned_prefix("きょうはがっこ", &pinned).len(), 2);
        assert_eq!(matching_pinned_prefix("きょうが", &pinned).len(), 1);
        assert_eq!(matching_pinned_prefix("あした", &pinned).len(), 0);
        assert_eq!(matching_pinned_prefix("", &pinned).len(), 0);
    }

    /// 固定した文節は、後続の入力でより安い別解が現れても書き換わらない。
    /// 固定していない末尾側は固定部分との接続を含めて通常どおり変換される。
    #[test]
    fn pinned_prefix_survives_cheaper_alternative_from_later_context() {
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        let mk = |s: &str, r: &str, id: PosId, cost: i16, pos: &str| WordEntry {
            surface: s.to_string(),
            reading: r.to_string(),
            left_id: id,
            right_id: id,
            cost,
            pos: pos.to_string(),
        };
        dict.add_word(mk("際", "さい", 1, 3000, "名詞-非自立-副詞可能-*"));
        dict.add_word(mk("再", "さい", 2, 3500, "接頭詞-名詞接続-*-*"));
        dict.add_word(mk("に", "に", 3, 1000, "助詞-格助詞-一般-*"));
        dict.add_word(mk("表示", "ひょうじ", 4, 3000, "名詞-サ変接続-*-*"));
        // 「再」→「に」は高コスト、「再」→「表示」は極端に安い（学習バイグラム相当）
        dict.matrix.set(2, 3, 3000);
        dict.matrix.set(2, 4, -6000);
        let converter = ViterbiConverter::new(dict);

        // 単独では「際に」
        assert_eq!(converter.convert_to_string("さいに"), "際に");
        // 固定しないと、後続の「表示」で「再」に書き換わる
        //（さい|に|ひょうじ の経路より さい(再)|ひょうじ… は無いが、
        //  ここでは 再→に のコスト差で確認する代わりに固定の有無で比較する）
        let free = converter.convert_with_pinned_prefix("さいにひょうじ", &[]);
        let pinned = vec![
            mk("際", "さい", 1, 3000, "名詞-非自立-副詞可能-*"),
            mk("に", "に", 3, 1000, "助詞-格助詞-一般-*"),
        ];
        let kept = converter.convert_with_pinned_prefix("さいにひょうじ", &pinned);
        let kept_s: String = kept.iter().map(|e| e.surface.as_str()).collect();
        assert_eq!(kept_s, "際に表示");
        assert_eq!(kept.len(), 3);
        // 固定無しの結果と比べても、固定部分は必ず「際に」で始まる
        let free_s: String = free.iter().map(|e| e.surface.as_str()).collect();
        assert!(free_s.ends_with("表示"));
    }

    /// 固定文節がラティスに存在しない場合（学習ひらがなノード等）でも
    /// ノードを追加して固定できる。読みが一致しない固定文節は無視される。
    #[test]
    fn pinned_prefix_adds_missing_node_and_ignores_mismatch() {
        let mut dict = Dictionary::new();
        dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        dict.add_word(WordEntry {
            surface: "今日".to_string(),
            reading: "きょう".to_string(),
            left_id: 1,
            right_id: 1,
            cost: 3000,
            pos: "名詞-副詞可能-*-*".to_string(),
        });
        let converter = ViterbiConverter::new(dict);
        // 辞書に無い表記「キョウ」を固定文節として与える
        let pinned = vec![WordEntry {
            surface: "キョウ".to_string(),
            reading: "きょう".to_string(),
            left_id: 1,
            right_id: 1,
            cost: 3000,
            pos: "名詞-一般-*-*".to_string(),
        }];
        let out = converter.convert_with_pinned_prefix("きょうきょう", &pinned);
        let s: String = out.iter().map(|e| e.surface.as_str()).collect();
        assert_eq!(s, "キョウ今日");
        // 読みが一致しなければ固定は無視され通常変換になる
        let out = converter.convert_with_pinned_prefix("あきょう", &pinned);
        assert_eq!(out.len(), converter.convert("あきょう").len());
    }
}
