use common::Dictionary;
use std::path::Path;

fn main() {
    let dict_path = std::env::args().nth(1).unwrap_or_else(|| "dictionaries/sample.dic".to_string());
    
    println!("辞書をロード: {}", dict_path);
    let dict = match Dictionary::load(Path::new(&dict_path)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("辞書ロード失敗: {}", e);
            return;
        }
    };
    
    // 「きょう」を検索
    let results = dict.trie.search("きょう");
    println!("\n「きょう」の検索結果:");
    match results {
        Some(entries) => {
            for entry in entries {
                println!("  {} (読み: {}, コスト: {})", entry.surface, entry.reading, entry.cost);
            }
        }
        None => println!("  見つかりません"),
    }
    
    // プレフィックス検索
    let prefix_results = dict.trie.common_prefix_search("きょうは");
    println!("\n「きょうは」のプレフィックス検索結果:");
    for (len, entries) in &prefix_results {
        println!("  長さ {} バイト:", len);
        for entry in entries.iter() {
            println!("    {} (読み: {})", entry.surface, entry.reading);
        }
    }
    
    // いくつかのテスト単語
    let test_words = ["こんにちは", "ありがとう", "わたし", "あ", "い"];
    println!("\nその他の検索:");
    for word in &test_words {
        let result = dict.trie.search(word);
        match result {
            Some(entries) => {
                println!("  {} → {}", word, entries.iter().map(|e| e.surface.as_str()).collect::<Vec<_>>().join(", "));
            }
            None => println!("  {} → 見つかりません", word),
        }
    }
}
