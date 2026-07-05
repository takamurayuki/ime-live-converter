//! 誤字補正機能
//! 
//! 要件定義書 7.5 誤字補正機能に基づく実装

use std::collections::HashMap;

/// 誤字補正候補
#[derive(Debug, Clone)]
pub struct TypoCandidate {
    /// 補正後のテキスト
    pub corrected: String,
    /// 元のテキスト
    pub original: String,
    /// 信頼度（0.0〜1.0）
    pub confidence: f32,
}

/// 誤字補正エンジン
pub struct TypoCorrector {
    /// ルールベースの補正マッピング
    rules: HashMap<&'static str, (&'static str, f32)>,
    /// キーボード隣接マッピング（ローマ字）
    adjacent_keys: HashMap<char, Vec<char>>,
}

impl TypoCorrector {
    pub fn new() -> Self {
        let mut rules = HashMap::new();
        
        // よくある誤字パターン（要件定義書の例）
        rules.insert("きょお", ("きょう", 0.9));
        rules.insert("こんにちわ", ("こんにちは", 0.95));
        rules.insert("ありがとお", ("ありがとう", 0.9));
        rules.insert("おねがいしｍす", ("おねがいします", 0.8));
        
        // 長音の誤り
        rules.insert("おおきい", ("おおきい", 1.0)); // 正しい
        rules.insert("おーきい", ("おおきい", 0.85));
        rules.insert("とおい", ("とおい", 1.0)); // 正しい
        rules.insert("とーい", ("とおい", 0.85));
        
        // 促音の誤り
        rules.insert("いて", ("いって", 0.7)); // 文脈依存
        rules.insert("もて", ("もって", 0.7));
        rules.insert("かて", ("かって", 0.7));
        
        // 「は」と「わ」の混同
        rules.insert("わたしわ", ("わたしは", 0.95));
        rules.insert("これわ", ("これは", 0.95));
        rules.insert("それわ", ("それは", 0.95));
        rules.insert("あれわ", ("あれは", 0.95));
        
        // 「を」と「お」の混同
        rules.insert("りんごお", ("りんごを", 0.85));
        rules.insert("ほんお", ("ほんを", 0.85));
        
        // 「ず」と「づ」の混同
        rules.insert("つづく", ("つづく", 1.0)); // 正しい
        rules.insert("つずく", ("つづく", 0.9));
        
        // 「じ」と「ぢ」の混同
        rules.insert("ちぢむ", ("ちぢむ", 1.0)); // 正しい
        rules.insert("ちじむ", ("ちぢむ", 0.9));

        // キーボード隣接（QWERTY配列）
        let mut adjacent_keys = HashMap::new();
        adjacent_keys.insert('a', vec!['s', 'q', 'w', 'z']);
        adjacent_keys.insert('s', vec!['a', 'd', 'w', 'e', 'x', 'z']);
        adjacent_keys.insert('d', vec!['s', 'f', 'e', 'r', 'c', 'x']);
        adjacent_keys.insert('f', vec!['d', 'g', 'r', 't', 'v', 'c']);
        adjacent_keys.insert('g', vec!['f', 'h', 't', 'y', 'b', 'v']);
        adjacent_keys.insert('h', vec!['g', 'j', 'y', 'u', 'n', 'b']);
        adjacent_keys.insert('j', vec!['h', 'k', 'u', 'i', 'm', 'n']);
        adjacent_keys.insert('k', vec!['j', 'l', 'i', 'o', 'm']);
        adjacent_keys.insert('l', vec!['k', 'o', 'p']);
        adjacent_keys.insert('q', vec!['w', 'a']);
        adjacent_keys.insert('w', vec!['q', 'e', 'a', 's']);
        adjacent_keys.insert('e', vec!['w', 'r', 's', 'd']);
        adjacent_keys.insert('r', vec!['e', 't', 'd', 'f']);
        adjacent_keys.insert('t', vec!['r', 'y', 'f', 'g']);
        adjacent_keys.insert('y', vec!['t', 'u', 'g', 'h']);
        adjacent_keys.insert('u', vec!['y', 'i', 'h', 'j']);
        adjacent_keys.insert('i', vec!['u', 'o', 'j', 'k']);
        adjacent_keys.insert('o', vec!['i', 'p', 'k', 'l']);
        adjacent_keys.insert('p', vec!['o', 'l']);
        adjacent_keys.insert('z', vec!['a', 's', 'x']);
        adjacent_keys.insert('x', vec!['z', 's', 'd', 'c']);
        adjacent_keys.insert('c', vec!['x', 'd', 'f', 'v']);
        adjacent_keys.insert('v', vec!['c', 'f', 'g', 'b']);
        adjacent_keys.insert('b', vec!['v', 'g', 'h', 'n']);
        adjacent_keys.insert('n', vec!['b', 'h', 'j', 'm']);
        adjacent_keys.insert('m', vec!['n', 'j', 'k']);

        Self { rules, adjacent_keys }
    }

    /// 誤字補正候補を生成
    ///
    /// ルールベース（完全一致 + 3文字以上のパターンの部分一致）と
    /// かな混同ペアによる編集距離1候補のみを返す。
    /// 混同ペア由来の候補は信頼度が低い（0.6）ので、呼び出し側で
    /// 変換コスト比較などの検証をしてから提示すること。
    pub fn correct(&self, input: &str) -> Vec<TypoCandidate> {
        let mut candidates: Vec<TypoCandidate> = Vec::new();

        // ルールベースの補正（完全一致）
        if let Some(&(corrected, confidence)) = self.rules.get(input) {
            if corrected != input {
                candidates.push(TypoCandidate {
                    corrected: corrected.to_string(),
                    original: input.to_string(),
                    confidence,
                });
            }
        }

        // 部分一致の補正
        // 2文字以下のパターン（「いて」→「いって」等）は誤爆しやすいので
        // 完全一致でのみ適用する
        for (&pattern, &(corrected, confidence)) in &self.rules {
            if pattern.chars().count() < 3 {
                continue;
            }
            if input.contains(pattern) && pattern != input {
                let fixed = input.replace(pattern, corrected);
                if fixed != input && !candidates.iter().any(|c| c.corrected == fixed) {
                    candidates.push(TypoCandidate {
                        corrected: fixed,
                        original: input.to_string(),
                        confidence: confidence * 0.9, // 部分一致は信頼度を下げる
                    });
                }
            }
        }

        // かな混同ペアによる置換候補
        for cand in self.confusion_variants(input) {
            if !candidates.iter().any(|c| c.corrected == cand.corrected) {
                candidates.push(cand);
            }
        }

        // 信頼度でソート
        candidates.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        candidates
    }

    /// 日本語入力で実際に混同されやすいかなペアによる置換候補を生成
    ///
    /// (誤りがちな文字, 正しい文字) の組。網羅的な編集距離1の全生成は
    /// 候補爆発するため行わず、実際の誤りパターンに限定する。
    fn confusion_variants(&self, input: &str) -> Vec<TypoCandidate> {
        // (typo側, 訂正側)
        const KANA_CONFUSIONS: &[(char, char)] = &[
            ('お', 'う'), // きょお → きょう（長音の誤り）
            ('わ', 'は'), // こんにちわ → こんにちは
            ('お', 'を'), // ほんお → ほんを
            ('ず', 'づ'), // つずく → つづく
            ('じ', 'ぢ'), // ちじむ → ちぢむ
            ('え', 'へ'), // がっこうえ → がっこうへ
            ('ー', 'う'), // すごーい 等の長音符
        ];

        let chars: Vec<char> = input.chars().collect();
        let mut results = Vec::new();

        for (i, &ch) in chars.iter().enumerate() {
            for &(typo, fix) in KANA_CONFUSIONS {
                if ch == typo {
                    let mut new_chars = chars.clone();
                    new_chars[i] = fix;
                    let corrected: String = new_chars.into_iter().collect();
                    results.push(TypoCandidate {
                        corrected,
                        original: input.to_string(),
                        confidence: 0.6,
                    });
                }
            }
        }

        results
    }

    /// ローマ字入力に対し、キーボード隣接ミスを置換した候補を返す。
    ///
    /// 1文字を隣接キーに置換した編集距離1の候補のみを返す。
    /// 用途: ローマ字バッファをひらがなに変換する前段で別解を作るときの素材。
    pub fn romaji_adjacent_variants(&self, romaji: &str) -> Vec<String> {
        let mut results = Vec::new();
        let chars: Vec<char> = romaji.chars().collect();

        for (i, ch) in chars.iter().enumerate() {
            if let Some(neighbors) = self.adjacent_keys.get(ch) {
                for &n in neighbors {
                    let mut variant = chars.clone();
                    variant[i] = n;
                    let s: String = variant.into_iter().collect();
                    if s != romaji {
                        results.push(s);
                    }
                }
            }
        }

        // 重複除去
        results.sort();
        results.dedup();
        results
    }

    /// 2つの文字列の編集距離を計算
    pub fn edit_distance(s1: &str, s2: &str) -> usize {
        let chars1: Vec<char> = s1.chars().collect();
        let chars2: Vec<char> = s2.chars().collect();
        let len1 = chars1.len();
        let len2 = chars2.len();

        let mut dp = vec![vec![0; len2 + 1]; len1 + 1];

        for (i, row) in dp.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in dp[0].iter_mut().enumerate() {
            *cell = j;
        }

        for i in 1..=len1 {
            for j in 1..=len2 {
                let cost = if chars1[i - 1] == chars2[j - 1] { 0 } else { 1 };
                dp[i][j] = (dp[i - 1][j] + 1)
                    .min(dp[i][j - 1] + 1)
                    .min(dp[i - 1][j - 1] + cost);
            }
        }

        dp[len1][len2]
    }
}

impl Default for TypoCorrector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_typo_correction_rules() {
        let corrector = TypoCorrector::new();
        
        let candidates = corrector.correct("こんにちわ");
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].corrected, "こんにちは");
    }

    #[test]
    fn test_edit_distance() {
        assert_eq!(TypoCorrector::edit_distance("きょう", "きょう"), 0);
        assert_eq!(TypoCorrector::edit_distance("きょう", "きょお"), 1);
        assert_eq!(TypoCorrector::edit_distance("abc", "abd"), 1);
    }

    #[test]
    fn test_romaji_adjacent_variants() {
        let corrector = TypoCorrector::new();
        // "ka" のkの隣接 (s/j/i/l/m, ...) で置換した候補を含む
        let variants = corrector.romaji_adjacent_variants("ka");
        assert!(!variants.is_empty());
        assert!(variants.iter().all(|v| v != "ka"));
        // 全部編集距離1
        for v in &variants {
            assert_eq!(TypoCorrector::edit_distance("ka", v), 1);
        }
    }

    #[test]
    fn test_partial_correction() {
        let corrector = TypoCorrector::new();
        
        let candidates = corrector.correct("わたしわげんきです");
        let has_correction = candidates.iter().any(|c| c.corrected.contains("わたしは"));
        assert!(has_correction);
    }
}
