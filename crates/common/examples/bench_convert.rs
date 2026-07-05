//! 変換レイテンシの簡易計測（要件 8.1: 50ms以内）
use common::{Dictionary, LiveConverter};
use std::path::Path;
use std::time::Instant;

fn main() {
    let t0 = Instant::now();
    let dict = Dictionary::load(Path::new("dictionaries/system.dic")).expect("辞書ロード失敗");
    println!("辞書ロード: {:?}", t0.elapsed());

    let mut lc = LiveConverter::new();
    lc.set_dictionary(dict);

    let inputs = [
        "きょう",
        "きょうはいいてんきです",
        "にほんごにゅうりょくつーるをつくります",
        "きょうはらすとでにほんごにゅうりょくをこうそくかします",
    ];

    for input in inputs {
        // ウォームアップ
        let _ = lc.generate_candidates(input);
        let n = 20;
        let t = Instant::now();
        for _ in 0..n {
            let _ = lc.generate_candidates(input);
        }
        let per = t.elapsed() / n;
        println!("{}文字 {:?}/回  ({})", input.chars().count(), per, input);
    }
}
