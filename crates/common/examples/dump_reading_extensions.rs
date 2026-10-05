//! 指定した読みを「先頭一致」する、より長い辞書の実在語を全部列挙する。
//! trusted_phrase_bonus のような「短い読み全体に無条件で効くボーナス」が
//! その読みで始まるもっと長い正当な語を巻き込んで断片化していないかを
//! 総当たりで確認するためのツール。
//!
//! 使い方: cargo run -p common --example dump_reading_extensions -- いっ どうか だれ
use common::Dictionary;

fn collect<'a>(node: &'a common::TrieNode, prefix: &str, out: &mut Vec<(String, &'a common::WordEntry)>) {
    for e in &node.entries {
        out.push((prefix.to_string(), e));
    }
    for (ch, child) in &node.children {
        let mut next = prefix.to_string();
        next.push(*ch);
        collect(child, &next, out);
    }
}

fn find_node<'a>(root: &'a common::TrieNode, reading: &str) -> Option<&'a common::TrieNode> {
    let mut node = root;
    for ch in reading.chars() {
        node = node.children.get(&ch)?;
    }
    Some(node)
}

fn main() {
    let dict = Dictionary::load(std::path::Path::new("dictionaries/system.dic"))
        .unwrap_or_else(|e| panic!("辞書ロード失敗: {e}"));

    for reading in std::env::args().skip(1) {
        println!("=== 「{reading}」で始まるより長い語 ===");
        let Some(node) = find_node(&dict.trie, &reading) else {
            println!("  (この読みで始まる語なし)");
            continue;
        };
        let mut out = Vec::new();
        for (ch, child) in &node.children {
            let mut prefix = reading.clone();
            prefix.push(*ch);
            collect(child, &prefix, &mut out);
        }
        out.sort_by_key(|(_, e)| e.cost);
        let mut seen = std::collections::HashSet::new();
        for (r, e) in out.iter().take(400) {
            if !seen.insert((r.clone(), e.surface.clone())) {
                continue;
            }
            if !(e.pos.starts_with("名詞") || e.pos.starts_with("動詞") || e.pos.starts_with("形容詞")) {
                continue;
            }
            println!("  {r}\t{}\tcost={}\tpos={}", e.surface, e.cost, e.pos);
        }
    }
}
