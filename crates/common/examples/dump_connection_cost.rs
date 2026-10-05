//! 固定した分割（読みのリスト）について、各位置の全品詞バリアントを
//! 総当たりし、BOS→EOSまでの真の最小コスト経路と内訳を表示するツール。
//! 誤変換の原因が「単語コストの合計」ではなく「接続コスト行列」にあるかを
//! 数値で切り分けるために使う（`surface`を指定しなければ全候補から
//! 総当たりで最安を探す。指定すればその表記に絞る）。
//!
//! 使い方:
//!   cargo run -p common --example dump_connection_cost -- いっ かんせい
//!   cargo run -p common --example dump_connection_cost -- いっかん せい
//!   （読みだけを空白区切りで並べる。特定の表記に固定したい場合は
//!    読み=表記 の形で書く）
use common::{Dictionary, WordEntry};

fn candidates_for<'a>(dict: &'a Dictionary, spec: &str) -> Vec<&'a WordEntry> {
    let (reading, surface) = spec.split_once('=').map_or((spec, None), |(r, s)| (r, Some(s)));
    let Some(entries) = dict.lookup(reading) else {
        panic!("辞書に無い読み: {reading}");
    };
    let out: Vec<&WordEntry> = entries
        .iter()
        .filter(|e| surface.is_none_or(|s| e.surface == s))
        .collect();
    if out.is_empty() {
        panic!("候補が見つからない: {spec}");
    }
    out
}

fn main() {
    let dict = Dictionary::load(std::path::Path::new("dictionaries/system.dic"))
        .unwrap_or_else(|e| panic!("辞書ロード失敗: {e}"));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let positions: Vec<Vec<&WordEntry>> = args.iter().map(|a| candidates_for(&dict, a)).collect();

    // dp[position][candidate_index] = (BOSからの累計最小コスト, その位置までの経路（各位置の候補index）)
    let mut dp: Vec<Vec<(i64, Vec<usize>)>> = Vec::with_capacity(positions.len());

    for (pos_idx, cands) in positions.iter().enumerate() {
        let mut layer = Vec::with_capacity(cands.len());
        for (cand_idx, e) in cands.iter().enumerate() {
            if pos_idx == 0 {
                let conn = dict.matrix.get(dict.bos_id, e.left_id) as i64;
                layer.push((conn + e.cost as i64, vec![cand_idx]));
            } else {
                let prev_layer = &dp[pos_idx - 1];
                let prev_cands = &positions[pos_idx - 1];
                let mut best: Option<(i64, Vec<usize>)> = None;
                for (prev_cand_idx, (prev_cost, prev_path)) in prev_layer.iter().enumerate() {
                    let prev_entry = prev_cands[prev_cand_idx];
                    let conn = dict.matrix.get(prev_entry.right_id, e.left_id) as i64;
                    let total = prev_cost + conn + e.cost as i64;
                    if best.as_ref().is_none_or(|(b, _)| total < *b) {
                        let mut path = prev_path.clone();
                        path.push(cand_idx);
                        best = Some((total, path));
                    }
                }
                layer.push(best.unwrap());
            }
        }
        dp.push(layer);
    }

    let last_idx = positions.len() - 1;
    let mut best: Option<(i64, Vec<usize>)> = None;
    for (cand_idx, (cost, path)) in dp[last_idx].iter().enumerate() {
        let e = positions[last_idx][cand_idx];
        let total = cost + dict.matrix.get(e.right_id, dict.eos_id) as i64;
        if best.as_ref().is_none_or(|(b, _)| total < *b) {
            best = Some((total, path.clone()));
        }
    }
    let (total, path) = best.unwrap();

    println!("BOS(id={})", dict.bos_id);
    let mut prev_right_id = dict.bos_id;
    let mut running = 0i64;
    for (pos_idx, &cand_idx) in path.iter().enumerate() {
        let e = positions[pos_idx][cand_idx];
        let conn = dict.matrix.get(prev_right_id, e.left_id) as i64;
        running += conn + e.cost as i64;
        println!(
            "  接続コスト({}→{})={conn}, 単語コスト({}/{})={}, 累計={running}",
            prev_right_id, e.left_id, e.surface, e.pos, e.cost
        );
        prev_right_id = e.right_id;
    }
    let conn = dict.matrix.get(prev_right_id, dict.eos_id) as i64;
    running += conn;
    println!("  接続コスト({}→EOS)={conn}, 累計={running}", prev_right_id);
    println!("=== 真の最小合計コスト（この分割内での全品詞総当たり）: {total} ===");
}
