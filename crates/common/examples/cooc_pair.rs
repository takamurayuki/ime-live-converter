//! 共起モデルの組の PMI を調べる調査用。
//!
//! 使い方: cargo run --release -p common --example cooc_pair -- <judge_cooc.bin> 汽車 駅 ホーム
//! （最初の語が対象、残りが文脈）
use common::judge_cooc::JudgeCooc;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cooc = JudgeCooc::load(Path::new(&args[1])).expect("judge_cooc ロード失敗");
    let target = &args[2];
    println!("対象 {} : ID {:?}", target, cooc.noun_id(target));
    for ctx in &args[3..] {
        let pmi = match (cooc.noun_id(target), cooc.noun_id(ctx)) {
            (Some(t), Some(c)) => cooc.pmi(t, c),
            _ => None,
        };
        println!("  文脈 {} : ID {:?} PMI {:?}", ctx, cooc.noun_id(ctx), pmi);
    }
}
