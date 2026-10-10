//! 変換レイテンシの簡易計測（要件 8.1: 50ms以内）
//!
//! 実運用と同じ`LiveConversionState`を、実際のキー入力と同じ意味論
//! （1文字ずつ`add_char`）で駆動して計測する。以前は独立の`LiveConverter`の
//! `generate_candidates(hiragana)`を1回で呼んでいたが、実運用は必ず1文字ずつ
//! 打鍵される（インクリメンタルViterbiのキャッシュ等が効く前提）ため、
//! こちらの方が実際のレイテンシに近い。
use common::{Dictionary, LiveConversionState, ViterbiConverter};
use std::path::Path;
use std::time::Instant;

fn main() {
    let t0 = Instant::now();
    let dict = Dictionary::load(Path::new("dictionaries/system.dic")).expect("辞書ロード失敗");
    println!("辞書ロード: {:?}", t0.elapsed());

    let mut state = LiveConversionState::new();
    state.converter = Some(ViterbiConverter::new(dict));

    let inputs = [
        "きょう",
        "きょうはいいてんきです",
        "にほんごにゅうりょくつーるをつくります",
        "きょうはらすとでにほんごにゅうりょくをこうそくかします",
    ];

    for input in inputs {
        // ウォームアップ
        state.cancel();
        for ch in input.chars() {
            state.add_char(ch);
        }

        let n = 20;
        let t = Instant::now();
        for _ in 0..n {
            state.cancel();
            for ch in input.chars() {
                state.add_char(ch);
            }
        }
        let per = t.elapsed() / n;
        println!("{}文字 {:?}/回  ({})", input.chars().count(), per, input);
    }
}
