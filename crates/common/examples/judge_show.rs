//! 判断層（judge-lm）の候補と内訳を表示する調査用ツール。
//!
//! 使い方: printf 'ぐんぐんすすむ\n' | cargo run --release -p common --example judge_show -- <judge_lm.bin>
//! 学習DBは読まない。各候補の 確率 / 総合スコア / LM対数確率 / 語彙外文字数 / 辞書コスト と、
//! 語ごとの (表記/読み, 語彙内か) を出す。
use common::judge::JudgeLm;
use common::{Dictionary, ViterbiConverter};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

fn main() {
    let lm_path = std::env::args().nth(1).expect("judge_lm.bin を指定してください");
    let dict_path = Path::new("dictionaries/system.dic");
    let mut conv = ViterbiConverter::new(Dictionary::load(dict_path).expect("辞書ロード失敗"));
    let _ = conv.load_word_priority_file(&dict_path.with_file_name("word_priority.tsv"));
    let _ = conv.load_word_assoc_file(&dict_path.with_file_name("word_assoc.tsv"));
    let judge = Arc::new(JudgeLm::load(Path::new(&lm_path)).expect("judge_lm ロード失敗"));
    println!("params: {:?}", judge.params());
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let reading = line.trim();
        if reading.is_empty() {
            continue;
        }
        conv.set_judge(None);
        let base = conv.convert_context_aware(reading);
        conv.set_judge(Some(Arc::clone(&judge)));
        println!("== {}", reading);
        for c in conv.judge_candidates(reading, &[], &base).iter().take(8) {
            let words: Vec<String> = c
                .entries
                .iter()
                .map(|e| {
                    let known = judge.word_id(&e.surface, &e.reading, e.left_id as u16).is_some();
                    format!("{}/{}{}", e.surface, e.reading, if known { "" } else { "(?)" })
                })
                .collect();
            println!(
                "  {:.3} score={:.2} lm={:.2} unk={} dict={} seed={} {}{}",
                c.prob,
                c.score.score,
                c.score.lm_logp,
                c.score.unk_chars,
                c.score.dict_cost,
                c.score.seed_hits,
                words.join(" "),
                if c.is_baseline { "  ←従来" } else { "" }
            );
        }
    }
}
