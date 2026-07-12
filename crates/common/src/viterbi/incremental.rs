//! ライブ変換コンテキストとインクリメンタルViterbi（末尾追加の高速再計算）

use super::*;

/// ライブ変換用のコンテキスト
#[derive(Debug)]
pub struct LiveConversionContext {
    /// 変換エンジン
    converter: ViterbiConverter,
    /// 入力バッファ（ひらがな）
    input_buffer: String,
    /// 確定済みテキスト
    committed_text: String,
    /// 現在の変換候補
    current_conversion: String,
}

impl LiveConversionContext {
    pub fn new(converter: ViterbiConverter) -> Self {
        Self {
            converter,
            input_buffer: String::new(),
            committed_text: String::new(),
            current_conversion: String::new(),
        }
    }

    /// ひらがなを追加して再変換
    pub fn add_hiragana(&mut self, hiragana: &str) -> &str {
        self.input_buffer.push_str(hiragana);
        self.update_conversion();
        &self.current_conversion
    }

    /// バックスペース処理
    pub fn backspace(&mut self) -> &str {
        self.input_buffer.pop();
        self.update_conversion();
        &self.current_conversion
    }

    /// 変換を更新
    fn update_conversion(&mut self) {
        if self.input_buffer.is_empty() {
            self.current_conversion.clear();
        } else {
            self.current_conversion = self.converter.convert_to_string(&self.input_buffer);
        }
    }

    /// 現在の変換を確定
    pub fn commit(&mut self) -> String {
        let result = self.current_conversion.clone();
        self.committed_text.push_str(&result);
        self.input_buffer.clear();
        self.current_conversion.clear();
        result
    }

    /// 入力バッファをクリア
    pub fn clear(&mut self) {
        self.input_buffer.clear();
        self.current_conversion.clear();
    }

    /// 現在の入力バッファを取得
    pub fn get_input_buffer(&self) -> &str {
        &self.input_buffer
    }

    /// 現在の変換結果を取得
    pub fn get_conversion(&self) -> &str {
        &self.current_conversion
    }
}

/// 差分計算対応のライブ変換エンジン
/// 
/// 入力が末尾に追加された場合、前回のラティスを再利用して高速に変換
#[derive(Debug)]
pub struct IncrementalViterbi {
    /// 変換エンジン
    converter: ViterbiConverter,
    /// キャッシュされたラティス
    cached_lattice: Option<Lattice>,
    /// 前回の入力文字列
    cached_input: String,
    /// 前回の変換結果
    cached_result: Vec<WordEntry>,
    /// 前回の入力のバイト位置（文字境界）
    char_byte_positions: Vec<usize>,
}

impl IncrementalViterbi {
    pub fn new(converter: ViterbiConverter) -> Self {
        Self {
            converter,
            cached_lattice: None,
            cached_input: String::new(),
            cached_result: Vec::new(),
            char_byte_positions: Vec::new(),
        }
    }

    /// 入力を変換（差分計算対応）
    pub fn convert(&mut self, input: &str) -> &[WordEntry] {
        if input.is_empty() {
            self.clear_cache();
            return &self.cached_result;
        }

        // 入力が前回と同じ場合はキャッシュを返す
        if input == self.cached_input {
            return &self.cached_result;
        }

        // TODO: 差分計算は複雑なので、一旦無効化
        // 新しい文字が追加された場合、古い位置から始まる長い単語も
        // 検出する必要があるため、現在の実装では正しく動作しない
        // 
        // 例: "き" + "ょ" + "う" → "きょう" → "今日"
        // 「う」追加時に「き」の位置から始まる「きょう」を検出できない
        
        // 常に完全再構築
        self.rebuild_lattice(input);
        &self.cached_result
    }

    /// 変換結果を文字列として取得
    pub fn convert_to_string(&mut self, input: &str) -> String {
        self.convert(input)
            .iter()
            .map(|e| e.surface.as_str())
            .collect()
    }

    /// ラティスを拡張（差分計算）
    ///
    /// 注意: 現在は convert() から呼ばれていない（末尾追加時に既存位置から
    /// 始まる長い単語を検出できない問題が未解決のため）。将来の差分計算
    /// 実装の土台として残している。
    #[allow(dead_code)]
    fn extend_lattice(&mut self, full_input: &str, added: &str) {
        let lattice = self.cached_lattice.as_mut().unwrap();
        let old_len = self.cached_input.len();
        let new_len = full_input.len();

        // 入力文字列を更新
        lattice.input = full_input.to_string();

        // EOSノードを更新
        lattice.nodes[lattice.eos_index].start = new_len;
        lattice.nodes[lattice.eos_index].end = new_len;

        // ノード配列を拡張
        let old_nodes_len = lattice.nodes_starting_at.len();
        lattice.nodes_starting_at.resize(new_len + 1, Vec::new());
        lattice.nodes_ending_at.resize(new_len + 1, Vec::new());

        // 古いEOSの位置からノードを削除
        if old_len < old_nodes_len {
            lattice.nodes_starting_at[old_len].retain(|&idx| idx != lattice.eos_index);
        }
        // 新しいEOSの位置にノードを追加
        lattice.nodes_starting_at[new_len].push(lattice.eos_index);

        // 追加された部分に対してノードを追加
        let added_chars: Vec<char> = added.chars().collect();
        let mut byte_pos = old_len;

        for ch in added_chars.iter() {
            let remaining = &full_input[byte_pos..];

            // 辞書からプレフィックス検索
            let matches = self.converter.dictionary.common_prefix_search(remaining);

            if matches.is_empty() {
                // 未知語として1文字を追加
                let end_pos = byte_pos + ch.len_utf8();
                let node = LatticeNode::unknown(
                    byte_pos,
                    end_pos,
                    ch.to_string(),
                    self.converter.unknown_id,
                    self.converter.unknown_cost,
                );
                let index = lattice.nodes.len();
                lattice.nodes.push(node);
                lattice.nodes_starting_at[byte_pos].push(index);
                lattice.nodes_ending_at[end_pos].push(index);
            } else {
                // マッチした単語をすべて追加
                for (len, entries) in &matches {
                    let end_pos = byte_pos + len;
                    for entry in entries.iter() {
                        let node = LatticeNode::word(byte_pos, end_pos, entry.clone());
                        let index = lattice.nodes.len();
                        lattice.nodes.push(node);
                        lattice.nodes_starting_at[byte_pos].push(index);
                        lattice.nodes_ending_at[end_pos].push(index);
                    }
                }

                // 未知語も追加
                let end_pos = byte_pos + ch.len_utf8();
                let node = LatticeNode::unknown(
                    byte_pos,
                    end_pos,
                    ch.to_string(),
                    self.converter.unknown_id,
                    self.converter.unknown_cost + 5000,
                );
                let index = lattice.nodes.len();
                lattice.nodes.push(node);
                lattice.nodes_starting_at[byte_pos].push(index);
                lattice.nodes_ending_at[end_pos].push(index);
            }

            // バイト位置を記録
            self.char_byte_positions.push(byte_pos);
            byte_pos += ch.len_utf8();
        }

        // 新しい部分からViterbiを再計算
        self.recompute_viterbi_from(old_len);

        // 結果を抽出
        self.cached_result = self.extract_result();
        self.cached_input = full_input.to_string();
    }

    /// 指定位置からViterbiを再計算
    #[allow(dead_code)]
    fn recompute_viterbi_from(&mut self, start_pos: usize) {
        let lattice = self.cached_lattice.as_mut().unwrap();
        let input_len = lattice.input.len();

        // 新しいノードのコストをリセット
        for node in &mut lattice.nodes {
            if node.start >= start_pos {
                node.total_cost = i32::MAX;
                node.prev_node = None;
            }
        }
        // EOSもリセット
        lattice.nodes[lattice.eos_index].total_cost = i32::MAX;
        lattice.nodes[lattice.eos_index].prev_node = None;

        // start_posから再計算
        for pos in start_pos..=input_len {
            let starting_indices: Vec<usize> = lattice.nodes_starting_at[pos].clone();

            for &node_idx in &starting_indices {
                let ending_indices: Vec<usize> = lattice.nodes_ending_at[pos].clone();

                let mut best_cost = i32::MAX;
                let mut best_prev: Option<usize> = None;

                for &prev_idx in &ending_indices {
                    let prev_node = &lattice.nodes[prev_idx];
                    if prev_node.total_cost == i32::MAX {
                        continue;
                    }

                    let current_node = &lattice.nodes[node_idx];

                    let conn_cost = self.converter.dictionary.matrix.get(
                        prev_node.right_id,
                        current_node.left_id,
                    ) as i32;

                    let total = prev_node.total_cost
                        .saturating_add(conn_cost)
                        .saturating_add(current_node.word_cost);

                    if total < best_cost {
                        best_cost = total;
                        best_prev = Some(prev_idx);
                    }
                }

                if best_cost < i32::MAX {
                    lattice.nodes[node_idx].total_cost = best_cost;
                    lattice.nodes[node_idx].prev_node = best_prev;
                }
            }
        }
    }

    /// ラティスを完全再構築
    fn rebuild_lattice(&mut self, input: &str) {
        let mut lattice = self.converter.build_lattice(input);
        self.converter.find_best_path(&mut lattice);
        
        self.cached_result = self.converter.extract_result(&lattice);
        self.cached_lattice = Some(lattice);
        self.cached_input = input.to_string();

        // バイト位置を記録
        self.char_byte_positions.clear();
        let mut byte_pos = 0;
        for ch in input.chars() {
            self.char_byte_positions.push(byte_pos);
            byte_pos += ch.len_utf8();
        }
    }

    /// 結果を抽出
    #[allow(dead_code)]
    fn extract_result(&self) -> Vec<WordEntry> {
        let lattice = self.cached_lattice.as_ref().unwrap();
        self.converter.extract_result(lattice)
    }

    /// キャッシュをクリア
    pub fn clear_cache(&mut self) {
        self.cached_lattice = None;
        self.cached_input.clear();
        self.cached_result.clear();
        self.char_byte_positions.clear();
    }

    /// バックスペース処理（1文字削除）
    pub fn backspace(&mut self) -> &[WordEntry] {
        if self.cached_input.is_empty() {
            return &self.cached_result;
        }

        // 最後の文字を削除した新しい入力を作成
        let mut chars: Vec<char> = self.cached_input.chars().collect();
        chars.pop();
        let new_input: String = chars.into_iter().collect();

        // 簡略化のため、バックスペース時は再構築
        // （将来的には部分的な再計算も可能）
        if new_input.is_empty() {
            self.clear_cache();
        } else {
            self.rebuild_lattice(&new_input);
        }

        &self.cached_result
    }
}
