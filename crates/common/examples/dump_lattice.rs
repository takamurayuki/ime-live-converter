//! 指定した読みについて、実際の `build_lattice`+`find_best_path` を走らせ、
//! 各ノード（表記・品詞・単語コスト・最終累計コスト）を全部ダンプする。
//! 「なぜこの分割が選ばれたか」を机上のDPではなく本物のエンジンの内部
//! 状態で確認するためのツール。
//!
//! 使い方: cargo run -p common --example dump_lattice -- いっかんせい
use common::{Dictionary, ViterbiConverter};

fn main() {
    let dict = Dictionary::load(std::path::Path::new("dictionaries/system.dic"))
        .unwrap_or_else(|e| panic!("辞書ロード失敗: {e}"));
    let conv = ViterbiConverter::new(dict);
    let reading = std::env::args().nth(1).unwrap_or_else(|| "いっかんせい".to_string());

    let mut lattice = conv.build_lattice(&reading);
    conv.find_best_path(&mut lattice);

    for (idx, node) in lattice.nodes.iter().enumerate() {
        let label = node
            .entry
            .as_ref()
            .map(|e| format!("{}/{}(cost={})", e.surface, e.pos, e.cost))
            .unwrap_or_else(|| if idx == lattice.bos_index { "BOS".to_string() } else if idx == lattice.eos_index { "EOS".to_string() } else { "?".to_string() });
        println!(
            "node[{idx}] {label} start={} end={} word_cost={} total_cost={} prev={:?}",
            node.start, node.end, node.word_cost, node.total_cost, node.prev_node
        );
    }

    println!("\n=== 最適パス ===");
    let result = conv.extract_result(&lattice);
    for e in &result {
        println!("  {}/{}(cost={})", e.surface, e.pos, e.cost);
    }
}
