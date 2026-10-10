//! 読みの辞書エントリを文脈ID付きで表示する（判断層のクラスモデルが使う
//! IPADic の左右文脈IDを、学習側の vibrato と突き合わせる調査用）。
//!
//! 使い方: cargo run --release -p common --example dump_entries -- かい まし
use common::Dictionary;
use std::path::Path;

fn main() {
    let dict = Dictionary::load(Path::new("dictionaries/system.dic")).expect("辞書ロード失敗");
    println!("bos_id={} eos_id={}", dict.bos_id, dict.eos_id);
    for reading in std::env::args().skip(1) {
        for e in dict.lookup(&reading).map(|v| v.to_vec()).unwrap_or_default() {
            println!("{}\t{}\t{}\t{}\t{}", e.surface, e.left_id, e.right_id, e.cost, e.pos);
        }
    }
}
