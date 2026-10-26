//! 英単語の自動判定（`common::english`）の評価ハーネス。
//!
//! 実際のキー入力と同じく1文字ずつ `add_char` → `commit` で打ち、英単語の候補あり／なしの
//! 確定結果を比べる。ケースごとに変換エンジンを作り直す（確定の学習が次のケースに
//! 漏れないように。学習DBは使わない）。
//!
//! 使い方（リポジトリ直下で）:
//! - 誤判定の検査（日本語の文が英字に化けないか）:
//!   cargo run --release -p common --example english_eval -- fp <dev.tsv> [件数]
//!   dev.tsv は「読み<TAB>正解の表記<TAB>...」（judge_eval と同じ形式）。読みをローマ字に
//!   直して打つ。英単語の候補で結果が変わった文と、英字が混ざった文の数を出す。
//! - 英単語の例文の検査:
//!   cargo run --release -p common --example english_eval -- pos <cases.tsv>
//!   cases.tsv は「ローマ字の打鍵<TAB>期待する確定結果」。
//! 環境変数 IME_ENGLISH_BIAS で `JudgeParams::english_bias` を上書きできる。
use common::english::EnglishLexicon;
use common::{ConversionAction, Dictionary, JudgeLm, LiveConversionState, ViterbiConverter};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

struct Shared {
    dict: Arc<Dictionary>,
    judge: Arc<JudgeLm>,
    cooc: Option<Arc<common::judge_cooc::JudgeCooc>>,
    english: Arc<EnglishLexicon>,
}

fn state(shared: &Shared, english: bool) -> LiveConversionState {
    let dir = Path::new("dictionaries");
    let mut conv = ViterbiConverter::from_shared(Arc::clone(&shared.dict));
    let _ = conv.load_word_priority_file(&dir.join("word_priority.tsv"));
    let _ = conv.load_word_assoc_file(&dir.join("word_assoc.tsv"));
    conv.set_judge(Some(Arc::clone(&shared.judge)));
    conv.set_cooc(shared.cooc.clone());
    if english {
        conv.set_english(Some(Arc::clone(&shared.english)));
    }
    let mut state = LiveConversionState::new();
    state.converter = Some(conv);
    state
}

fn type_and_commit(state: &mut LiveConversionState, romaji: &str) -> String {
    let apply = |sim: &mut String, action: Option<ConversionAction>| {
        let Some(a) = action else { return };
        let keep = sim.chars().count().saturating_sub(a.delete_count);
        *sim = sim.chars().take(keep).collect();
        sim.push_str(&a.insert_text);
    };
    let mut sim = String::new();
    for ch in romaji.chars() {
        let action = state.add_char(ch);
        apply(&mut sim, action);
    }
    let action = state.commit();
    apply(&mut sim, action);
    sim
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let dir = Path::new("dictionaries");
    let dict = Arc::new(Dictionary::load(&dir.join("system.dic")).expect("辞書ロード失敗"));
    let mut judge = JudgeLm::load(&dir.join("judge_lm.bin")).expect("判断層ロード失敗");
    if let Ok(b) = std::env::var("IME_ENGLISH_BIAS") {
        let mut p = judge.params();
        p.english_bias = b.parse().expect("IME_ENGLISH_BIAS は数値");
        judge.set_params(p);
    }
    eprintln!("english_bias = {}", judge.params().english_bias);
    let cooc = common::judge_cooc::JudgeCooc::load(&dir.join("judge_cooc.bin")).ok().map(Arc::new);
    let t = std::time::Instant::now();
    let english = Arc::new(EnglishLexicon::load_dir(dir).expect("英単語の頻度表ロード失敗"));
    eprintln!("英単語 {}語（読み込み {:?}）", english.len(), t.elapsed());
    let shared = Shared { dict, judge: Arc::new(judge), cooc, english };

    match mode {
        "fp" => {
            let path = args.get(2).expect("dev.tsv を指定してください");
            let limit: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(500);
            let file = std::io::BufReader::new(std::fs::File::open(path).expect("dev.tsv が開けません"));
            let (mut n, mut changed, mut latin, mut ok_with, mut ok_without) = (0, 0, 0, 0, 0);
            for line in file.lines().map_while(Result::ok) {
                let mut cols = line.split('\t');
                let (Some(reading), Some(gold)) = (cols.next(), cols.next()) else { continue };
                // 読みにかな以外（英数字・記号）が入った文は対象外（ローマ字で打てない）
                if !reading.chars().all(|c| ('ぁ'..='ゖ').contains(&c) || c == 'ー') {
                    continue;
                }
                let romaji = common::report::kana_to_romaji(reading);
                let with = type_and_commit(&mut state(&shared, true), &romaji);
                let without = type_and_commit(&mut state(&shared, false), &romaji);
                n += 1;
                ok_with += (with == gold) as usize;
                ok_without += (without == gold) as usize;
                if with != without {
                    changed += 1;
                    let has_latin = with.chars().any(|c| c.is_ascii_alphabetic());
                    latin += has_latin as usize;
                    println!("{}\t{romaji}\t英語あり「{with}」\tなし「{without}」\t正解「{gold}」", if has_latin { "英字" } else { "変化" });
                }
                if n >= limit {
                    break;
                }
            }
            println!(
                "== {n}文: 結果が変わった {changed} / うち英字が混ざった {latin} / 正解一致 英語あり {ok_with} なし {ok_without}"
            );
        }
        "pos" => {
            let path = args.get(2).expect("cases.tsv を指定してください");
            let text = std::fs::read_to_string(path).expect("cases.tsv が開けません");
            let (mut n, mut ok) = (0, 0);
            for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
                let mut cols = line.split('\t');
                let (Some(romaji), Some(expected)) = (cols.next(), cols.next()) else { continue };
                let got = type_and_commit(&mut state(&shared, true), romaji);
                n += 1;
                let pass = got == expected;
                ok += pass as usize;
                println!("{}\t{romaji}\t→「{got}」{}", if pass { "OK" } else { "NG" }, if pass { String::new() } else { format!("（期待「{expected}」）") });
            }
            println!("== {ok}/{n} 一致");
        }
        "bench" => {
            // 残りの引数をローマ字の打鍵として打ち、1打鍵あたりの処理時間（最大・合計）を英単語の候補あり／なしで比べる
            for romaji in &args[2..] {
                for with in [false, true] {
                    let mut st = state(&shared, with);
                    let (mut max, mut total) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
                    for ch in romaji.chars() {
                        let t = std::time::Instant::now();
                        st.add_char(ch);
                        let d = t.elapsed();
                        max = max.max(d);
                        total += d;
                    }
                    println!("{} 英単語{}: 最大 {:?} / 合計 {:?}", romaji.len(), if with { "あり" } else { "なし" }, max, total);
                }
            }
        }
        "cand" => {
            // 残りの引数をローマ字の打鍵として打ち、表示と候補一覧（Tab）を出す
            for romaji in &args[2..] {
                let mut st = state(&shared, true);
                for ch in romaji.chars() {
                    st.add_char(ch);
                }
                let shown = st.conversion_result.clone();
                st.cycle_candidate(false);
                println!("{romaji} → 表示「{shown}」 候補: {}", st.candidates.join(" / "));
            }
        }
        _ => eprintln!("使い方: english_eval fp <dev.tsv> [件数] | english_eval pos <cases.tsv> | english_eval cand <打鍵>..."),
    }
}
