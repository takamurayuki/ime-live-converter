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
#[derive(Debug)]
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

    /// 読みで単語を検索
    pub fn lookup(&self, reading: &str) -> Option<&Vec<WordEntry>> {
        self.trie.search(reading)
    }

    /// プレフィックス検索
    pub fn common_prefix_search(&self, text: &str) -> Vec<(usize, &Vec<WordEntry>)> {
        self.trie.common_prefix_search(text)
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

#[cfg(test)]
mod tests {
    use super::*;

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
