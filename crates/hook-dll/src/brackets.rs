//! 括弧の種類切替と閉じ括弧の自動対応
//!
//! `[` `]` キーは RomajiConverter で 「 」 になるが、セリフの「」だけでなく
//! 『』（作品名・引用）や【】（見出し・強調）、（）〈〉《》〔〕［］｛｝ を
//! 使い分けたい。括弧は変換を素通りする文字なので、
//! - 括弧を打った直後に Tab を押すと同じ側（開き/閉じ）の全種類を候補に出し
//! - 選んだ種類は読みバッファ内の括弧文字そのものを書き換えて維持し
//! - 閉じ括弧 `]` は直前の未対応の開き括弧の種類に自動で合わせる
//!   （『 を選んでいれば `]` で 』 が出る。入れ子も対応）

/// 対応する括弧の組（開き, 閉じ）。候補一覧はこの順で並ぶ。
pub(crate) const BRACKET_PAIRS: &[(char, char)] = &[
    ('「', '」'),
    ('『', '』'),
    ('【', '】'),
    ('（', '）'),
    ('〈', '〉'),
    ('《', '》'),
    ('〔', '〕'),
    ('［', '］'),
    ('｛', '｝'),
];

pub(crate) fn is_opening_bracket(c: char) -> bool {
    BRACKET_PAIRS.iter().any(|&(o, _)| o == c)
}

pub(crate) fn is_closing_bracket(c: char) -> bool {
    BRACKET_PAIRS.iter().any(|&(_, cl)| cl == c)
}

/// 開き括弧に対応する閉じ括弧
pub(crate) fn matching_closer(open: char) -> Option<char> {
    BRACKET_PAIRS.iter().find(|&&(o, _)| o == open).map(|&(_, cl)| cl)
}

/// 文字列がちょうど1文字の括弧（開き or 閉じ）ならその文字
pub(crate) fn single_bracket_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if is_opening_bracket(c) || is_closing_bracket(c) => Some(c),
        _ => None,
    }
}

/// `text` の末尾から見て、まだ閉じられていない最も内側の開き括弧
pub(crate) fn unmatched_opener(text: &str) -> Option<char> {
    let mut pending_closers = 0usize;
    for c in text.chars().rev() {
        if is_closing_bracket(c) {
            pending_closers += 1;
        } else if is_opening_bracket(c) {
            if pending_closers == 0 {
                return Some(c);
            }
            pending_closers -= 1;
        }
    }
    None
}

/// 括弧の種類切替の候補: 現在の括弧を先頭に、同じ側（開き/閉じ）の全種類
pub(crate) fn bracket_variants(current: char) -> Vec<char> {
    let opening = is_opening_bracket(current);
    let mut variants: Vec<char> = BRACKET_PAIRS
        .iter()
        .map(|&(o, c)| if opening { o } else { c })
        .collect();
    if let Some(pos) = variants.iter().position(|&c| c == current) {
        variants.remove(pos);
        variants.insert(0, current);
    }
    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmatched_opener_finds_innermost_open_bracket() {
        assert_eq!(unmatched_opener("「あ『い"), Some('『'));
        assert_eq!(unmatched_opener("「あ『い』う"), Some('「'));
        assert_eq!(unmatched_opener("「あ」"), None);
        assert_eq!(unmatched_opener("あいう"), None);
        assert_eq!(unmatched_opener("【見出し】『本"), Some('『'));
    }

    #[test]
    fn bracket_variants_put_current_first_and_keep_side() {
        let v = bracket_variants('【');
        assert_eq!(v[0], '【');
        assert_eq!(v.len(), BRACKET_PAIRS.len());
        assert!(v.iter().all(|&c| is_opening_bracket(c)));
        let v = bracket_variants('』');
        assert_eq!(v[0], '』');
        assert!(v.iter().all(|&c| is_closing_bracket(c)));
    }

    #[test]
    fn single_bracket_char_rejects_non_brackets() {
        assert_eq!(single_bracket_char("「"), Some('「'));
        assert_eq!(single_bracket_char("」"), Some('」'));
        assert_eq!(single_bracket_char("「」"), None);
        assert_eq!(single_bracket_char("あ"), None);
        assert_eq!(single_bracket_char(""), None);
    }
}
