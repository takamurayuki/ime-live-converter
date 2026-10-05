//! 判断層（judge-lm）の打鍵ごとの変換時間を測る（1文字ずつ伸ばして全接頭辞を変換）。
//!
//! 使い方: cargo run --release -p common --example judge_bench -- <judge_lm.bin> < 読みの行
use common::judge::JudgeLm;
use common::{Dictionary, ViterbiConverter};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let lm_path = std::env::args().nth(1).expect("judge_lm.bin を指定してください");
    let dict_path = Path::new("dictionaries/system.dic");
    let mut conv = ViterbiConverter::new(Dictionary::load(dict_path).expect("辞書ロード失敗"));
    let _ = conv.load_word_priority_file(&dict_path.with_file_name("word_priority.tsv"));
    let _ = conv.load_word_assoc_file(&dict_path.with_file_name("word_assoc.tsv"));
    let t = Instant::now();
    let judge = Arc::new(JudgeLm::load(Path::new(&lm_path)).expect("judge_lm ロード失敗"));
    println!("読み込み: {}ms", t.elapsed().as_millis());
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let chars: Vec<char> = line.trim().chars().collect();
        for (label, j) in [("従来", None), ("判断層", Some(Arc::clone(&judge)))] {
            conv.set_judge(j);
            let (mut max, mut sum) = (0u128, 0u128);
            for n in 1..=chars.len() {
                let prefix: String = chars[..n].iter().collect();
                let t = Instant::now();
                let _ = conv.convert_context_aware(&prefix);
                let us = t.elapsed().as_micros();
                max = max.max(us);
                sum += us;
            }
            println!(
                "{}文字 {}: 最大 {:.1}ms / 平均 {:.1}ms → {}",
                chars.len(),
                label,
                max as f64 / 1000.0,
                sum as f64 / chars.len() as f64 / 1000.0,
                conv.convert_context_aware_to_string(&chars.iter().collect::<String>())
            );
        }
    }
}
