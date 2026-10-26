//! 英単語の頻度表の調査用。
//! - 引数に英字を並べると、英字の連鎖モデルによる綴りの採点（1文字あたり）を表示する:
//!   cargo run --release -p common --example english_spell -- kubectl nginx xkqz
//! - `--convertible [開始順位] [件数]` で、打鍵がすべてかなに直る（ローマ字としても
//!   打てる）英単語を順位つきで表示する（日本語の打鍵と取り違えうる語の確認用）
use common::RomajiConverter;

fn main() {
    let tsv = std::fs::read_to_string("dictionaries/english_words.tsv").unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--convertible") {
        let from: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let count: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
        let romaji = RomajiConverter::new();
        let words = tsv.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split('\t').next());
        for (rank, w) in words.enumerate().skip(from).filter(|(_, w)| w.len() >= 3).take(count * 20) {
            let kana = romaji.convert(w);
            if !kana.chars().any(|c| c.is_ascii_alphabetic()) {
                println!("{rank}\t{w}\t{kana}");
            }
        }
        return;
    }
    let lex = common::english::EnglishLexicon::from_strs(&tsv, "");
    for w in args {
        let lp = lex.spelling_logp(&w);
        println!("{w}\t{lp:.2}\t{:.2}/文字", lp / (w.len() + 1) as f32);
    }
}
