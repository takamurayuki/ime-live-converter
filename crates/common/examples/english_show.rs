//! 英単語の候補が判断層でどう採点されるかを表示する調査用。
//! 使い方: echo konocodewo | cargo run --release -p common --example english_show
use common::english::{EnglishLexicon, RomajiPiece};
use common::{Dictionary, JudgeLm, RomajiConverter, ViterbiConverter};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

fn main() {
    let dir = Path::new("dictionaries");
    let mut conv = ViterbiConverter::new(Dictionary::load(&dir.join("system.dic")).unwrap());
    let _ = conv.load_word_priority_file(&dir.join("word_priority.tsv"));
    let _ = conv.load_word_assoc_file(&dir.join("word_assoc.tsv"));
    let mut judge = JudgeLm::load(&dir.join("judge_lm.bin")).unwrap();
    if let Ok(b) = std::env::var("IME_ENGLISH_BIAS") {
        let mut p = judge.params();
        p.english_bias = b.parse().unwrap();
        judge.set_params(p);
    }
    conv.set_judge(Some(Arc::new(judge)));
    let english = Arc::new(EnglishLexicon::load_dir(dir).unwrap());
    conv.set_english(Some(english.clone()));
    let romaji = RomajiConverter::new();
    // IME_SCORE="表記/読み/英 表記/読み ..." で語列1本を採点する（英=英単語の候補として）
    if let Ok(spec) = std::env::var("IME_SCORE") {
        let path: Vec<common::WordEntry> = spec
            .split(' ')
            .map(|w| {
                let parts: Vec<&str> = w.split('/').collect();
                if parts.get(2) == Some(&"英") {
                    let span = common::english::EnglishSpan {
                        start: 0,
                        end: 0,
                        word: parts[0].to_string(),
                        logp: english.word_logp(parts[0]).unwrap_or(-20.0),
                        continuation: false,
                    };
                    common::english::english_entries(&span, parts[1])[0].clone()
                } else {
                    conv.convert(parts[1]).into_iter().find(|e| e.surface == parts[0]).unwrap_or_else(|| {
                        conv.dictionary.lookup(parts[1]).unwrap().iter().find(|e| e.surface == parts[0]).unwrap().clone()
                    })
                }
            })
            .collect();
        println!("score={:?}", conv.judge_path_score(&path));
        return;
    }
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let raw = line.trim();
        let chars: Vec<char> = raw.chars().collect();
        let mut at = 0;
        let pieces: Vec<RomajiPiece> = romaji
            .convert_aligned(raw)
            .into_iter()
            .map(|(n, kana)| {
                let r: String = chars[at..at + n].iter().collect();
                at += n;
                RomajiPiece { kana, raw: r }
            })
            .collect();
        let reading: String = pieces.iter().map(|p| p.kana.as_str()).collect();
        println!("== {raw} → {reading}");
        for s in english.spans(&pieces) {
            println!("  span {:?}", s);
        }
        for s in english.spans(&pieces) {
            let reading_of: String = reading.chars().skip(s.start).take(s.end - s.start).collect();
            for e in common::english::english_entries(&s, &reading_of) {
                if s.start == 0 && s.end == reading.chars().count() {
                    let j = conv.judge.as_ref().unwrap();
                    let u = j.unit(&e);
                    println!(
                        "  whole {} id={} score={:?} lm={:.2} internal={:.2} costs={:?}",
                        e.surface, e.left_id, conv.judge_path_score(&[e.clone()]),
                        j.sequence_logp(None, &[e.clone()]), u.internal, conv.path_costs(&[e.clone()])
                    );
                }
            }
        }
        conv.set_romaji_hint(Some(pieces), &std::env::var("IME_CAPITAL").unwrap_or_default());
        let base = conv.convert(&reading);
        for c in conv.judge_candidates(&reading, &[], &base) {
            println!(
                "  {:.3} score={:.2} lm={:.2} dict={} {}",
                c.prob,
                c.score.score,
                c.score.lm_logp,
                c.score.dict_cost,
                c.entries.iter().map(|e| format!("{}/{}[{}]", e.surface, e.reading, e.pos)).collect::<Vec<_>>().join(" ")
            );
        }
    }
}
