//! 外来語カタカナ語の「長音あり/なし」表記ゆれを辞書全体で洗い出す調査ツール。
//!
//! IPADic は「ブラウザー」のように末尾に長音の付いた表記しか持たない語が多く、
//! 長音を打たずに「ぶらうざ」と入力すると辞書に1語として存在しないため、
//! 「ブラ＋宇座」のようなカタカナ断片＋漢字断片に分割されてしまう。
//! 辞書のカタカナ一般名詞（末尾が長音、長音を除いて3音以上）それぞれについて、長音を除いた読みを実運用と
//! 同じ `LiveConversionState`（判断層あり・学習なし）で変換し、長音なしの
//! カタカナ表記にならない語を列挙する。
//!
//! 使い方: cargo run --release -p common --example katakana_variant_sweep [-- 最大表示件数]
use common::{LiveConversionState, TrieNode, WordEntry};
use std::collections::BTreeMap;
use std::path::Path;

fn collect(node: &TrieNode, out: &mut Vec<WordEntry>) {
    out.extend(node.entries.iter().cloned());
    for child in node.children.values() {
        collect(child, out);
    }
}

fn is_katakana_word(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| ('ァ'..='ヶ').contains(&c) || c == 'ー' || c == 'ヴ')
}

fn main() {
    let limit: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200);
    let mut state = LiveConversionState::new();
    assert!(state.load_dictionary(Path::new("dictionaries/system.dic")), "辞書ロード失敗");
    let conv = state.converter.as_ref().unwrap();
    println!("判断層: {}", if conv.judge.is_some() { "あり" } else { "なし" });

    let mut words = Vec::new();
    collect(&conv.dictionary.trie, &mut words);
    // 長音を除いた読み → 期待する表記（同じ読みに複数あれば最小コストのもの）
    let mut targets: BTreeMap<String, (String, i16)> = BTreeMap::new();
    for w in &words {
        // 固有名詞は長音を省く慣習が無いので対象外（Dictionary::add_long_vowel_variants と同じ条件）
        let general = w.pos.starts_with("名詞-一般") || w.pos.starts_with("名詞-サ変接続");
        if !general || !is_katakana_word(&w.surface) {
            continue;
        }
        let (Some(surface), Some(reading)) = (w.surface.strip_suffix('ー'), w.reading.strip_suffix('ー')) else {
            continue;
        };
        let morae = surface.chars().filter(|c| !"ァィゥェォャュョヮ".contains(*c)).count();
        if morae < 3 || surface.ends_with(['ー', 'ッ', 'ン']) {
            continue;
        }
        let e = targets.entry(reading.to_string()).or_insert((surface.to_string(), w.cost));
        if w.cost < e.1 {
            *e = (surface.to_string(), w.cost);
        }
    }

    let total = targets.len();
    let mut failures = Vec::new();
    for (reading, (expected, _)) in &targets {
        state.hiragana_buffer = reading.clone();
        let segs = state.convert_buffer(reading);
        let shown: String = segs.iter().map(|e| e.surface.as_str()).collect();
        if &shown != expected {
            let split: Vec<&str> = segs.iter().map(|e| e.surface.as_str()).collect();
            failures.push((reading.clone(), expected.clone(), split.join("|")));
        }
    }
    let mixed = failures
        .iter()
        .filter(|(_, _, got)| got.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)))
        .count();
    println!(
        "対象 {total} 語 / 長音なし表記にならない {} 語（うち漢字が混ざる {mixed} 語）",
        failures.len()
    );
    for (reading, expected, got) in failures.iter().take(limit) {
        println!("{reading}\t期待 {expected}\t実際 {got}");
    }
}
