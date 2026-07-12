//! N-best 経路探索（候補一覧用の上位N変換）

use super::*;

/// N-best探索で使うパーシャルパス
#[derive(Clone, Eq, PartialEq)]
struct PartialPath {
    /// f_cost = cost + head_node.total_cost (完成時の総コスト下界)
    f_cost: i64,
    /// バックワード走査でこれまでに積み上げたコスト (head_node → EOS)
    cost: i64,
    /// 現在のヘッドノード（バックワードに最も左にあるノード）
    head_node: usize,
    /// 訪問済みノードのインデックス列（EOS→...→head_node の順）
    path: Vec<usize>,
}

impl Ord for PartialPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeapは最大ヒープなので逆順に比較してmin-heapにする
        other.f_cost.cmp(&self.f_cost)
    }
}

impl PartialOrd for PartialPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// ラティスからN-bestパスを取り出す
pub(crate) fn n_best_from_lattice(lattice: &Lattice, dict: &Dictionary, n: usize) -> Vec<Vec<WordEntry>> {
    use std::collections::BinaryHeap;

    let mut heap: BinaryHeap<PartialPath> = BinaryHeap::new();
    heap.push(PartialPath {
        f_cost: lattice.nodes[lattice.eos_index].total_cost as i64,
        cost: 0,
        head_node: lattice.eos_index,
        path: vec![lattice.eos_index],
    });

    let mut results: Vec<Vec<WordEntry>> = Vec::new();
    let mut seen_surfaces: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 暴走を防ぐためにループ上限を設ける（候補数 × 1000）
    let max_iterations = n.saturating_mul(1000).max(10_000);
    let mut iterations = 0;

    while let Some(current) = heap.pop() {
        iterations += 1;
        if iterations > max_iterations {
            break;
        }
        if results.len() >= n {
            break;
        }

        // BOSに到達したら完成
        if current.head_node == lattice.bos_index {
            let entries: Vec<WordEntry> = current
                .path
                .iter()
                .rev()
                .filter_map(|&idx| lattice.nodes[idx].entry.clone())
                .collect();
            let surface: String = entries.iter().map(|e| e.surface.as_str()).collect();
            if seen_surfaces.insert(surface) {
                results.push(entries);
            }
            continue;
        }

        // ヘッドノードの前駆を展開
        let head = &lattice.nodes[current.head_node];
        let pos = head.start;
        let head_word_cost = head.word_cost as i64;
        let head_left_id = head.left_id;

        for &prev_idx in &lattice.nodes_ending_at[pos] {
            let prev = &lattice.nodes[prev_idx];
            if prev.total_cost == i32::MAX {
                continue;
            }

            let conn_cost = dict.matrix.get(prev.right_id, head_left_id) as i64;
            // step_cost(prev → head) = conn_cost + head.word_cost
            let step_cost = conn_cost + head_word_cost;
            let new_cost = current.cost + step_cost;
            let new_f = new_cost + prev.total_cost as i64;

            let mut new_path = current.path.clone();
            new_path.push(prev_idx);

            heap.push(PartialPath {
                f_cost: new_f,
                cost: new_cost,
                head_node: prev_idx,
                path: new_path,
            });
        }
    }

    results
}
