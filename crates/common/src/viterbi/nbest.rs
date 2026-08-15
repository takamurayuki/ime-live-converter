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
        let head_entry = head.entry.as_ref();

        for &prev_idx in &lattice.nodes_ending_at[pos] {
            let prev = &lattice.nodes[prev_idx];
            if prev.total_cost == i32::MAX {
                continue;
            }

            let mut conn_cost = dict.matrix.get(prev.right_id, head_left_id) as i32;
            // find_best_path と同じ無条件ガード（学習ボーナス非依存のもの）を
            // ここにも適用する。N-best（候補一覧・LiveConverterが実際に使う
            // 経路）は本来 find_best_path と同じ接続コストで探索すべきだが、
            // 従来は辞書の生の連接行列のみを見ており、この無条件ガード群が
            // 反映されていなかった（カテゴリF/Gの修正が候補一覧に出ない
            // 原因）。学習ボーナス依存のfloor系（clamp_single_kanji_pair_conn_cost
            // 等）は ViterbiConverter の学習状態が必要なため対象外のまま。
            if let (Some(pe), Some(ce)) = (&prev.entry, head_entry) {
                conn_cost = conn_cost.saturating_add(katakana_kanji_suffix_penalty(pe, ce));
                conn_cost = adjective_terminal_then_te_penalty(pe, ce, conn_cost);
                conn_cost =
                    conn_cost.saturating_add(single_kanji_lone_particle_reading_penalty(ce));
            }
            let conn_cost = conn_cost as i64;
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
