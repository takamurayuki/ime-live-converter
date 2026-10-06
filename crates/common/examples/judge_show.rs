//! 判断層（judge-lm）の候補と内訳を表示する調査用ツール。
//!
//! 使い方: printf 'ぐんぐんすすむ\n' | cargo run --release -p common --example judge_show -- <judge_lm.bin>
//! 学習DBは読まない（IME_LEARN で個人学習を模擬できる）。JUDGE_COOC で共起モデルを使い、
//! 行を「前の文の読み|今の読み」にすると前の文を確定済みの文脈として扱う。各候補の 確率 / 総合スコア / LM対数確率 / 語彙外文字数 / 辞書コスト と、
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
    // IME_LEARN="よみ=表記,よみ=表記" で、その変換を1回確定した個人学習を模擬する
    for pair in std::env::var("IME_LEARN").unwrap_or_default().split(',').filter(|p| !p.is_empty()) {
        if let Some((r, s)) = pair.split_once('=') {
            conv.learn_unigram(r, s, 1);
        }
    }
    let judge = Arc::new(JudgeLm::load(Path::new(&lm_path)).expect("judge_lm ロード失敗"));
    // JUDGE_COOC=<judge_cooc.bin> で共起モデルも使う
    if let Ok(p) = std::env::var("JUDGE_COOC") {
        let cooc = common::judge_cooc::JudgeCooc::load(Path::new(&p)).expect("judge_cooc ロード失敗");
        conv.set_cooc(Some(Arc::new(cooc)));
    }
    println!("params: {:?}", judge.params());
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 「前の文の読み|今の読み」なら、前の文を変換・確定した扱いにして
        // その名詞を共起の文脈にする
        conv.clear_context();
        let reading = match line.split_once('|') {
            Some((prev, cur)) => {
                conv.set_judge(Some(Arc::clone(&judge)));
                let committed: Vec<(String, String, String)> = conv
                    .convert_context_aware(prev)
                    .into_iter()
                    .map(|e| (e.reading, e.surface, e.pos))
                    .collect();
                println!("(確定済み: {})", committed.iter().map(|c| c.1.as_str()).collect::<String>());
                conv.note_committed(&committed);
                cur
            }
            None => line,
        };
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
                "  {:.3} score={:.2} lm={:.2} unk={} dict={} learned={} seed={} cooc={:.2} {}{}",
                c.prob,
                c.score.score,
                c.score.lm_logp,
                c.score.unk_chars,
                c.score.dict_cost,
                c.score.learned,
                c.score.seed_hits,
                c.score.cooc,
                words.join(" "),
                if c.is_baseline { "  ←従来" } else { "" }
            );
        }
    }
}
