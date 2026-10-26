use std::collections::HashMap;

/// 品詞ID
pub type PosId = u16;

/// 単語エントリ
#[derive(Debug, Clone)]
pub struct WordEntry {
    /// 表層形（漢字など）
    pub surface: String,
    /// 読み（ひらがな）
    pub reading: String,
    /// 品詞ID（左文脈）
    pub left_id: PosId,
    /// 品詞ID（右文脈）
    pub right_id: PosId,
    /// 単語コスト（低いほど出現しやすい）
    pub cost: i16,
    /// 品詞情報
    pub pos: String,
}

/// 辞書内の単語参照（バイナリ辞書用）
#[derive(Debug, Clone, Copy)]
pub struct WordRef {
    /// 単語データ配列へのオフセット
    pub offset: u32,
    /// データ長
    pub length: u16,
}

/// Trieノード（プレフィックス検索用）
#[derive(Debug, Clone, Default)]
pub struct TrieNode {
    /// 子ノードへのマッピング
    pub children: HashMap<char, Box<TrieNode>>,
    /// このノードで終わる単語のリスト
    pub entries: Vec<WordEntry>,
    /// 単語が存在するかどうか
    pub is_end: bool,
}

impl TrieNode {
    pub fn new() -> Self {
        Self::default()
    }

    /// 単語を挿入
    pub fn insert(&mut self, reading: &str, entry: WordEntry) {
        let mut node = self;
        for ch in reading.chars() {
            node = node
                .children
                .entry(ch)
                .or_insert_with(|| Box::new(TrieNode::new()));
        }
        node.is_end = true;
        node.entries.push(entry);
    }

    /// 読み＋表記が一致するエントリを削除する（単語登録の削除用）。消したら true。
    pub fn remove(&mut self, reading: &str, surface: &str) -> bool {
        let mut node = self;
        for ch in reading.chars() {
            match node.children.get_mut(&ch) {
                Some(child) => node = child,
                None => return false,
            }
        }
        let before = node.entries.len();
        node.entries.retain(|e| e.surface != surface);
        let removed = node.entries.len() != before;
        if node.entries.is_empty() {
            node.is_end = false;
        }
        removed
    }

    /// 読みで検索（完全一致）
    pub fn search(&self, reading: &str) -> Option<&Vec<WordEntry>> {
        let mut node = self;
        for ch in reading.chars() {
            match node.children.get(&ch) {
                Some(child) => node = child,
                None => return None,
            }
        }
        if node.is_end {
            Some(&node.entries)
        } else {
            None
        }
    }

    /// あいまい検索: 編集距離 max_dist 以内の「実在する読み」をすべて返す。
    /// 戻り値: (読み, 編集距離)。Trie の枝刈り付き Levenshtein DP なので、
    /// 全語走査せず候補周辺だけを辿る（辞書全体でも十分速い）。
    ///
    /// 誤字補正で「存在しない読みを組み立てる」のを避けるために使う。
    /// 実在語だけを候補に出せるので、一致率（1 - 距離/長さ）でランク付けできる。
    pub fn fuzzy_search(&self, target: &str, max_dist: usize) -> Vec<(String, usize)> {
        let chars: Vec<char> = target.chars().collect();
        let n = chars.len();
        // 初期行: 空プレフィックスに対する編集距離 0,1,2,...,n
        let first_row: Vec<usize> = (0..=n).collect();
        let mut results = Vec::new();
        let mut prefix = String::new();
        for (&ch, child) in &self.children {
            child.fuzzy_walk(ch, &chars, &first_row, max_dist, &mut prefix, &mut results);
        }
        results
    }

    /// fuzzy_search の再帰ヘルパー（現在ノード＝文字 ch を消費した後の状態）。
    fn fuzzy_walk(
        &self,
        ch: char,
        chars: &[char],
        prev_row: &[usize],
        max_dist: usize,
        prefix: &mut String,
        results: &mut Vec<(String, usize)>,
    ) {
        let n = chars.len();
        let mut cur = vec![0usize; n + 1];
        cur[0] = prev_row[0] + 1;
        for i in 1..=n {
            let cost = if chars[i - 1] == ch { 0 } else { 1 };
            cur[i] = (prev_row[i] + 1)
                .min(cur[i - 1] + 1)
                .min(prev_row[i - 1] + cost);
        }
        prefix.push(ch);
        if self.is_end && cur[n] <= max_dist {
            results.push((prefix.clone(), cur[n]));
        }
        // 行の最小値が max_dist を超えたら、この枝以降は絶対に距離内に入らない
        if cur.iter().min().copied().unwrap_or(usize::MAX) <= max_dist {
            for (&c, child) in &self.children {
                child.fuzzy_walk(c, chars, &cur, max_dist, prefix, results);
            }
        }
        prefix.pop();
    }

    /// プレフィックス検索（すべてのマッチを返す）
    /// 戻り値: (読みの長さ, 単語エントリリスト)
    pub fn common_prefix_search(&self, text: &str) -> Vec<(usize, &Vec<WordEntry>)> {
        let mut results = Vec::new();
        let mut node = self;
        let mut len = 0;

        for ch in text.chars() {
            match node.children.get(&ch) {
                Some(child) => {
                    node = child;
                    len += ch.len_utf8();
                    if node.is_end {
                        results.push((len, &node.entries));
                    }
                }
                None => break,
            }
        }

        results
    }
}

/// 連接行列（品詞間の連接コスト）
#[derive(Debug, Clone)]
pub struct ConnectionMatrix {
    /// 左文脈IDの数
    pub left_size: u16,
    /// 右文脈IDの数
    pub right_size: u16,
    /// 連接コスト配列 [right_id * left_size + left_id]
    pub costs: Vec<i16>,
}

impl ConnectionMatrix {
    pub fn new(left_size: u16, right_size: u16) -> Self {
        let size = (left_size as usize) * (right_size as usize);
        Self {
            left_size,
            right_size,
            costs: vec![0; size],
        }
    }

    /// 連接コストを設定
    pub fn set(&mut self, left_id: PosId, right_id: PosId, cost: i16) {
        let idx = (right_id as usize) * (self.left_size as usize) + (left_id as usize);
        if idx < self.costs.len() {
            self.costs[idx] = cost;
        }
    }

    /// 連接コストを取得
    pub fn get(&self, left_id: PosId, right_id: PosId) -> i16 {
        let idx = (right_id as usize) * (self.left_size as usize) + (left_id as usize);
        self.costs.get(idx).copied().unwrap_or(0)
    }
}

/// メイン辞書構造
#[derive(Debug, Clone)]
pub struct Dictionary {
    /// 単語Trie
    pub trie: TrieNode,
    /// 連接行列
    pub matrix: ConnectionMatrix,
    /// BOS（文頭）の品詞ID
    pub bos_id: PosId,
    /// EOS（文末）の品詞ID
    pub eos_id: PosId,
}

impl Dictionary {
    pub fn new() -> Self {
        // デフォルトでは小さな連接行列を作成
        Self {
            trie: TrieNode::new(),
            matrix: ConnectionMatrix::new(1, 1),
            bos_id: 0,
            eos_id: 0,
        }
    }

    /// 単語を追加
    pub fn add_word(&mut self, entry: WordEntry) {
        let reading = entry.reading.clone();
        self.trie.insert(&reading, entry);
    }

    /// 読み＋表記が一致する単語を削除する（単語登録の削除用）。消したら true。
    pub fn remove_word(&mut self, reading: &str, surface: &str) -> bool {
        self.trie.remove(reading, surface)
    }

    /// 読みで単語を検索
    pub fn lookup(&self, reading: &str) -> Option<&Vec<WordEntry>> {
        self.trie.search(reading)
    }

    /// プレフィックス検索
    pub fn common_prefix_search(&self, text: &str) -> Vec<(usize, &Vec<WordEntry>)> {
        self.trie.common_prefix_search(text)
    }

    /// 編集距離 max_dist 以内の実在する読みを返す（(読み, 距離)）。
    pub fn fuzzy_readings(&self, target: &str, max_dist: usize) -> Vec<(String, usize)> {
        self.trie.fuzzy_search(target, max_dist)
    }

    /// 外来語の末尾の長音を省いた表記（ブラウザー→ブラウザ）を補う。追加した語数を返す。
    ///
    /// IPA辞書の外来語は「ブラウザー」「アドベンチャー」のように末尾に長音の
    /// 付いた表記しか持たないことが多い。IT 用語などで一般的な長音なしの
    /// 「ぶらうざ」と打つと、辞書に1語として無いため「ブラ＋宇座」のような
    /// カタカナ断片＋漢字断片に割れてしまう。JIS Z 8301 の慣例（3音以上の語は
    /// 末尾の長音を省いてよい）に合わせ、次の条件の語だけ長音なし表記を補う。
    /// - 品詞が名詞-一般／名詞-サ変接続（固有名詞は長音を省く慣習が無く、
    ///   「イチロー→いちろ」のような読みが「一路」等を乗っ取るだけなので除く）
    /// - 表記が全てカタカナで、長音を除いた読みが3音以上
    /// - 同じ読み・表記が既に辞書に無い
    ///
    /// コストは元の語に `LONG_VOWEL_VARIANT_PENALTY` を足す（同じ読みの別の語や、
    /// 読みを丸ごと覆う別の分割より優先しすぎないように）。
    pub fn add_long_vowel_variants(&mut self) -> usize {
        let mut variants = Vec::new();
        collect_long_vowel_variants(&self.trie, &mut variants);
        let mut added = 0;
        for variant in variants {
            let exists = self
                .lookup(&variant.reading)
                .is_some_and(|es| es.iter().any(|e| e.surface == variant.surface));
            if !exists {
                self.add_word(variant);
                added += 1;
            }
        }
        added
    }

    /// 単語が1つも登録されていないか（`ViterbiConverter`の上乗せ辞書
    /// `overlay`が「一度もadd_wordされていない」状態かの判定に使う。
    /// 空なら検索を丸ごと省略でき、基底辞書（`Arc`共有、不変）だけを
    /// 引く既存コードと同じコストで済む）。
    pub fn is_empty(&self) -> bool {
        self.trie.children.is_empty() && self.trie.entries.is_empty()
    }
}

impl Default for Dictionary {
    fn default() -> Self {
        Self::new()
    }
}

/// 未知語処理のための文字種
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharCategory {
    /// ひらがな
    Hiragana,
    /// カタカナ
    Katakana,
    /// 漢字
    Kanji,
    /// ASCII文字
    Ascii,
    /// 数字
    Digit,
    /// 記号
    Symbol,
    /// その他
    Other,
}

impl CharCategory {
    /// 文字の種類を判定
    pub fn of(ch: char) -> Self {
        match ch {
            '\u{3041}'..='\u{3096}' | '\u{3099}'..='\u{309F}' => CharCategory::Hiragana,
            '\u{30A1}'..='\u{30FA}' | '\u{30FC}'..='\u{30FF}' => CharCategory::Katakana,
            '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' => CharCategory::Kanji,
            'A'..='Z' | 'a'..='z' => CharCategory::Ascii,
            '0'..='9' => CharCategory::Digit,
            _ if ch.is_ascii_punctuation() => CharCategory::Symbol,
            _ => CharCategory::Other,
        }
    }
}

/// 長音なし表記に付けるコストの上乗せ（`Dictionary::add_long_vowel_variants`）
pub const LONG_VOWEL_VARIANT_PENALTY: i16 = 1000;

fn collect_long_vowel_variants(node: &TrieNode, out: &mut Vec<WordEntry>) {
    out.extend(node.entries.iter().filter_map(long_vowel_variant));
    for child in node.children.values() {
        collect_long_vowel_variants(child, out);
    }
}

/// 拍の数（拗音・小書きの母音は前の文字と合わせて1拍、長音・促音・撥音は1拍）
fn mora_count(kana: &str) -> usize {
    kana.chars()
        .filter(|c| !matches!(c, 'ァ' | 'ィ' | 'ゥ' | 'ェ' | 'ォ' | 'ャ' | 'ュ' | 'ョ' | 'ヮ'
            | 'ぁ' | 'ぃ' | 'ぅ' | 'ぇ' | 'ぉ' | 'ゃ' | 'ゅ' | 'ょ' | 'ゎ'))
        .count()
}

/// `add_long_vowel_variants` の対象なら、末尾の長音を除いた語を返す
fn long_vowel_variant(w: &WordEntry) -> Option<WordEntry> {
    if !(w.pos.starts_with("名詞-一般") || w.pos.starts_with("名詞-サ変接続")) {
        return None;
    }
    if !w.surface.chars().all(|c| ('ァ'..='ヶ').contains(&c) || c == 'ー') {
        return None;
    }
    let surface = w.surface.strip_suffix('ー')?;
    let reading = w.reading.strip_suffix('ー')?;
    // 「ー」の連続や促音・撥音で終わる形（アッー等）は語として不自然
    if surface.ends_with(['ー', 'ッ', 'ン']) || mora_count(surface) < 3 {
        return None;
    }
    Some(WordEntry {
        surface: surface.to_string(),
        reading: reading.to_string(),
        cost: w.cost.saturating_add(LONG_VOWEL_VARIANT_PENALTY),
        ..w.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noun(surface: &str, reading: &str, pos: &str, cost: i16) -> WordEntry {
        WordEntry {
            surface: surface.to_string(),
            reading: reading.to_string(),
            left_id: 1285,
            right_id: 1285,
            cost,
            pos: pos.to_string(),
        }
    }

    #[test]
    fn long_vowel_variants_cover_general_loanwords_only() {
        let mut dict = Dictionary::new();
        dict.add_word(noun("ブラウザー", "ぶらうざー", "名詞-一般-*-*", 3657));
        dict.add_word(noun("コピー", "こぴー", "名詞-サ変接続-*-*", 3000)); // 2音は長音を省かない
        dict.add_word(noun("イチロー", "いちろー", "名詞-固有名詞-人名-名", 3000)); // 固有名詞
        dict.add_word(noun("サーバー", "さーばー", "名詞-一般-*-*", 3000));
        dict.add_word(noun("サーバ", "さーば", "名詞-一般-*-*", 3500)); // 既に長音なしがある
        assert_eq!(dict.add_long_vowel_variants(), 1);

        let added = dict.lookup("ぶらうざ").expect("ブラウザが補われる");
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].surface, "ブラウザ");
        assert_eq!(added[0].cost, 3657 + LONG_VOWEL_VARIANT_PENALTY);
        assert!(dict.lookup("こぴ").is_none());
        assert!(dict.lookup("いちろ").is_none());
        assert_eq!(dict.lookup("さーば").unwrap().len(), 1);
        // 2回呼んでも重複しない
        assert_eq!(dict.add_long_vowel_variants(), 0);
    }

    #[test]
    fn test_trie_insert_and_search() {
        let mut trie = TrieNode::new();
        
        let entry = WordEntry {
            surface: "今日".to_string(),
            reading: "きょう".to_string(),
            left_id: 1,
            right_id: 1,
            cost: 5000,
            pos: "名詞".to_string(),
        };
        
        trie.insert("きょう", entry);
        
        let result = trie.search("きょう");
        assert!(result.is_some());
        assert_eq!(result.unwrap()[0].surface, "今日");
    }

    #[test]
    fn test_common_prefix_search() {
        let mut trie = TrieNode::new();
        
        trie.insert("き", WordEntry {
            surface: "木".to_string(),
            reading: "き".to_string(),
            left_id: 1, right_id: 1, cost: 6000,
            pos: "名詞".to_string(),
        });
        
        trie.insert("きょ", WordEntry {
            surface: "虚".to_string(),
            reading: "きょ".to_string(),
            left_id: 1, right_id: 1, cost: 8000,
            pos: "名詞".to_string(),
        });
        
        trie.insert("きょう", WordEntry {
            surface: "今日".to_string(),
            reading: "きょう".to_string(),
            left_id: 1, right_id: 1, cost: 5000,
            pos: "名詞".to_string(),
        });
        
        let results = trie.common_prefix_search("きょうは");
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_char_category() {
        assert_eq!(CharCategory::of('あ'), CharCategory::Hiragana);
        assert_eq!(CharCategory::of('ア'), CharCategory::Katakana);
        assert_eq!(CharCategory::of('漢'), CharCategory::Kanji);
        assert_eq!(CharCategory::of('a'), CharCategory::Ascii);
        assert_eq!(CharCategory::of('5'), CharCategory::Digit);
    }
}
