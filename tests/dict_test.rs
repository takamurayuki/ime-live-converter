use common::Dictionary;
use std::path::Path;

fn main() {
    let dict = Dictionary::load(Path::new("dictionaries/sample.dic")).expect("辞書ロード失敗");
    
    // 「きょう」を検索
    let results = dict.trie.search("きょう");
    println!("きょう の検索結果: {:?}", results);
    
    // プレフィックス検索
    let prefix_results = dict.trie.common_prefix_search("きょうは");
    println!("きょうは のプレフィックス検索結果:");
    for (len, entries) in prefix_results {
        println!("  長さ {}: {:?}", len, entries.iter().map(|e| &e.surface).collect::<Vec<_>>());
    }
}
