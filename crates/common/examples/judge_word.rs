//! 判断層の語ごとの統計（語彙内か、P(語|クラス)、文頭からの対数確率）を表示する調査用。
//!
//! 使い方: cargo run --release -p common --example judge_word -- <judge_lm.bin> けいき
use common::judge::{JudgeLm, LmCtx};
use common::Dictionary;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let judge = JudgeLm::load(Path::new(&args[1])).expect("judge_lm ロード失敗");
    let dict = Dictionary::load(Path::new("dictionaries/system.dic")).expect("辞書ロード失敗");
    for reading in &args[2..] {
        println!("== {}", reading);
        for e in dict.lookup(reading).map(|v| v.to_vec()).unwrap_or_default() {
            let u = judge.unit(&e);
            let emit = u.first.map(|w| judge.data().emit_logp[w as usize]);
            let from_bos = judge.edge_logp(None, LmCtx::BOS, &u);
            println!(
                "  {}\tcost={}\t{}\temit={:?}\tP(語|文頭)={:.2}",
                e.surface, e.cost, e.pos, emit, from_bos
            );
        }
    }
}
