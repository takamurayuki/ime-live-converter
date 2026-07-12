//! ラティス（単語候補グラフ）とノード定義

use super::*;

/// ラティス上のノード
#[derive(Debug, Clone)]
pub struct LatticeNode {
    /// 開始位置（バイト単位）
    pub start: usize,
    /// 終了位置（バイト単位）
    pub end: usize,
    /// 単語エントリ（BOSとEOSはNone）
    pub entry: Option<WordEntry>,
    /// 左文脈ID
    pub left_id: PosId,
    /// 右文脈ID
    pub right_id: PosId,
    /// 単語コスト
    pub word_cost: i32,
    /// 累積コスト（このノードまでの最小コスト）
    pub total_cost: i32,
    /// 最適パスの前のノードのインデックス
    pub prev_node: Option<usize>,
}

impl LatticeNode {
    /// BOS（文頭）ノードを作成
    pub fn bos(bos_id: PosId) -> Self {
        Self {
            start: 0,
            end: 0,
            entry: None,
            left_id: bos_id,
            right_id: bos_id,
            word_cost: 0,
            total_cost: 0,
            prev_node: None,
        }
    }

    /// EOS（文末）ノードを作成
    pub fn eos(pos: usize, eos_id: PosId) -> Self {
        Self {
            start: pos,
            end: pos,
            entry: None,
            left_id: eos_id,
            right_id: eos_id,
            word_cost: 0,
            total_cost: i32::MAX,
            prev_node: None,
        }
    }

    /// 単語ノードを作成
    pub fn word(start: usize, end: usize, entry: WordEntry) -> Self {
        Self {
            start,
            end,
            left_id: entry.left_id,
            right_id: entry.right_id,
            word_cost: entry.cost as i32,
            total_cost: i32::MAX,
            prev_node: None,
            entry: Some(entry),
        }
    }

    /// 未知語ノードを作成
    pub fn unknown(start: usize, end: usize, surface: String, default_id: PosId, cost: i32) -> Self {
        Self {
            start,
            end,
            entry: Some(WordEntry {
                surface: surface.clone(),
                reading: surface,
                left_id: default_id,
                right_id: default_id,
                cost: cost as i16,
                pos: "未知語".to_string(),
            }),
            left_id: default_id,
            right_id: default_id,
            word_cost: cost,
            total_cost: i32::MAX,
            prev_node: None,
        }
    }
}

/// ラティス（単語候補のグラフ）
#[derive(Debug)]
pub struct Lattice {
    /// 入力テキスト
    pub input: String,
    /// 各位置で始まるノードのリスト
    pub nodes_starting_at: Vec<Vec<usize>>,
    /// 各位置で終わるノードのリスト
    pub nodes_ending_at: Vec<Vec<usize>>,
    /// 全ノードの配列
    pub nodes: Vec<LatticeNode>,
    /// BOSノードのインデックス
    pub bos_index: usize,
    /// EOSノードのインデックス
    pub eos_index: usize,
}

impl Lattice {
    /// 新しいラティスを作成
    pub fn new(input: &str, bos_id: PosId, eos_id: PosId) -> Self {
        let len = input.len();
        let mut lattice = Self {
            input: input.to_string(),
            nodes_starting_at: vec![Vec::new(); len + 1],
            nodes_ending_at: vec![Vec::new(); len + 1],
            nodes: Vec::new(),
            bos_index: 0,
            eos_index: 0,
        };

        // BOSノードを追加
        lattice.bos_index = lattice.add_node(LatticeNode::bos(bos_id));
        lattice.nodes_ending_at[0].push(lattice.bos_index);

        // EOSノードを追加
        lattice.eos_index = lattice.add_node(LatticeNode::eos(len, eos_id));
        lattice.nodes_starting_at[len].push(lattice.eos_index);

        lattice
    }

    /// ノードを追加
    fn add_node(&mut self, node: LatticeNode) -> usize {
        let index = self.nodes.len();
        self.nodes.push(node);
        index
    }

    /// 単語ノードを追加
    pub fn add_word(&mut self, start: usize, end: usize, entry: WordEntry) {
        let node = LatticeNode::word(start, end, entry);
        let index = self.add_node(node);
        self.nodes_starting_at[start].push(index);
        self.nodes_ending_at[end].push(index);
    }

    /// 未知語ノードを追加
    pub fn add_unknown(&mut self, start: usize, end: usize, surface: String, default_id: PosId, cost: i32) {
        let node = LatticeNode::unknown(start, end, surface, default_id, cost);
        let index = self.add_node(node);
        self.nodes_starting_at[start].push(index);
        self.nodes_ending_at[end].push(index);
    }
}
