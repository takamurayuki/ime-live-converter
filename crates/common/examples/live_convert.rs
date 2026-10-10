//! hook-dll と同じ初期化（辞書 + word_priority.tsv + 学習DB + ユーザー辞書注入）で
//! 標準入力の各行（ひらがな）をライブ変換と同じ `convert_context_aware` に通し、
//! 文節区切り付きで表示する再現ハーネス。
//!
//! 使い方: printf 'さいきどう\n' | cargo run --release -p common --example live_convert
//! 環境変数:
//! - IME_NO_LEARNING=1 : 学習DBを読まない
//! - IME_FLAG=1        : 「1文字漢字＋漢字語」の断片化候補箇所を報告する
//! - IME_PREFIX=1      : 1文字ずつ打鍵した場合のライブ表示を再現し、先頭側が
//!                       書き換わる打鍵（変換の崩れ）と変換時間の最大値を報告する
//!   - IME_PIN=1       : さらに hook-dll の「先頭文節の固定」（stabilize.rs）を模擬
//!   - IME_STABILIZE=1 : 「読みを切って末尾だけ再変換」（不採用の部分確定方式）を模擬
//!   - IME_STAB_MIN / IME_STAB_KEEP でしきい値を上書き
//! - IME_PROFILE=1     : ラティス規模と変換時間の内訳を表示する
//! - IME_NO_ASSOC=1    : 学習した内容語連想（rerank_by_assoc）を読まない
//! - IME_NO_CORPUS_LM=1: 同じディレクトリの corpus_lm.dic（あれば）を読まない
use common::{CorpusLm, Dictionary, LearningRepository, ViterbiConverter, WordEntry};
use std::io::BufRead;
use std::path::Path;

fn main() {
    let dict_path = Path::new("dictionaries/system.dic");
    let dict = Dictionary::load(dict_path).expect("辞書ロード失敗");
    let mut conv = ViterbiConverter::new(dict);
    let _ = conv.load_word_priority_file(&dict_path.with_file_name("word_priority.tsv"));
    let _ = conv.load_word_assoc_file(&dict_path.with_file_name("word_assoc.tsv"));
    if std::env::var("IME_NO_CORPUS_LM").is_err() {
        let corpus_lm_path = dict_path.with_file_name("corpus_lm.dic");
        if corpus_lm_path.exists() {
            match CorpusLm::load(&corpus_lm_path) {
                Ok(lm) => {
                    eprintln!(
                        "コーパスLMをロード: unigram={} bigram={}",
                        lm.unigrams.len(), lm.bigrams.len()
                    );
                    conv.load_corpus_lm(&lm);
                }
                Err(e) => eprintln!("コーパスLMのロードに失敗: {e}"),
            }
        }
    }
    if std::env::var("IME_NO_LEARNING").is_err() {
        let learning = LearningRepository::open("ime-learning.db").expect("学習DBオープン失敗");
        for (r, s, f) in learning.all_unigrams().unwrap_or_default() {
            conv.learn_unigram(&r, &s, f);
        }
        for (p, s, f) in learning.all_bigrams().unwrap_or_default() {
            conv.learn_bigram(&p, &s, f);
        }
        if std::env::var("IME_NO_ASSOC").is_err() {
            for (p, c, f) in learning.all_assocs().unwrap_or_default() {
                conv.learn_assoc(&p, &c, f);
            }
        }
        for (r, f) in learning.all_hiragana_prefs().unwrap_or_default() {
            conv.learn_hiragana(&r, f);
        }
        for e in learning.get_all_user_words().unwrap_or_default() {
            conv.overlay.add_word(WordEntry {
                surface: e.surface.clone(),
                reading: e.reading.clone(),
                left_id: 1285,
                right_id: 1285,
                cost: e.cost as i16,
                pos: e.pos.unwrap_or_else(|| "名詞-一般-*-*".to_string()),
            });
            conv.learn_unigram(&e.reading, &e.surface, 20);
        }
    }
    let flag = std::env::var("IME_FLAG").is_ok();
    let prefix_mode = std::env::var("IME_PREFIX").is_ok();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.unwrap_or_default();
        let reading = line.trim();
        if reading.is_empty() {
            continue;
        }
        if prefix_mode {
            if std::env::var("IME_STABILIZE").is_ok() {
                simulate_typing_stabilized(&conv, reading);
            } else if std::env::var("IME_PIN").is_ok() {
                simulate_typing_pinned(&conv, reading);
            } else {
                simulate_typing(&conv, reading);
            }
            continue;
        }
        if std::env::var("IME_PROFILE").is_ok() {
            profile(&conv, reading);
            continue;
        }
        let segs = conv.convert_context_aware(reading);
        let joined: Vec<String> = segs.iter().map(|e| e.surface.clone()).collect();
        println!("{}\t{}\t{}", reading, joined.concat(), joined.join("|"));
        if flag {
            flag_single_kanji_fragments(&conv, &segs);
        }
    }
}

fn is_kanji(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
}

/// 1文字ずつ入力した場合のライブ表示を再現し、「末尾から4文字より前」が
/// 書き換わった打鍵（先頭側の変換が崩れた瞬間）と、変換時間の最大値を報告する。
fn simulate_typing(conv: &ViterbiConverter, reading: &str) {
    let chars: Vec<char> = reading.chars().collect();
    let mut prev = String::new();
    let mut max_ms = 0u128;
    let mut flips = Vec::new();
    for k in 1..=chars.len() {
        let input: String = chars[..k].iter().collect();
        let t0 = std::time::Instant::now();
        let cur = conv.convert_context_aware_to_string(&input);
        max_ms = max_ms.max(t0.elapsed().as_millis());
        let pc: Vec<char> = prev.chars().collect();
        let cc: Vec<char> = cur.chars().collect();
        let common = pc.iter().zip(cc.iter()).take_while(|(a, b)| a == b).count();
        if pc.len() >= 4 && common + 4 < pc.len() {
            flips.push(format!(
                "  k={} rewrite {} chars back: '{}' -> '{}'",
                k,
                pc.len() - common,
                prev,
                cur
            ));
        }
        prev = cur;
    }
    println!("{}\tmax={}ms\tlen={}\tflips={}", prev, max_ms, chars.len(), flips.len());
    for f in flips {
        println!("{}", f);
    }
}

/// hook-dll の「先頭文節の固定」を模擬する: 読みが長くなったら先頭側の文節を
/// 固定し、以降は `convert_context_aware_pinned` で全体を変換する。
fn simulate_typing_pinned(conv: &ViterbiConverter, reading: &str) {
    let min_chars: usize = std::env::var("IME_STAB_MIN").ok().and_then(|v| v.parse().ok())
        .unwrap_or(common::viterbi::STABILIZE_MIN_READING_CHARS);
    let keep_tail: usize = std::env::var("IME_STAB_KEEP").ok().and_then(|v| v.parse().ok())
        .unwrap_or(common::viterbi::STABILIZE_KEEP_TAIL_CHARS);
    let chars: Vec<char> = reading.chars().collect();
    let mut pinned: Vec<WordEntry> = Vec::new();
    let mut prev = String::new();
    let mut max_ms = 0u128;
    let mut flips = Vec::new();
    for k in 1..=chars.len() {
        let input: String = chars[..k].iter().collect();
        let t0 = std::time::Instant::now();
        let entries = conv.convert_context_aware_pinned(&input, &pinned);
        max_ms = max_ms.max(t0.elapsed().as_millis());
        let n = common::viterbi::stable_prefix_segments(&entries, min_chars, keep_tail);
        if n > pinned.len() {
            pinned = entries[..n].to_vec();
        }
        let cur: String = entries.iter().map(|e| e.surface.as_str()).collect();
        let pc: Vec<char> = prev.chars().collect();
        let cc: Vec<char> = cur.chars().collect();
        let common = pc.iter().zip(cc.iter()).take_while(|(a, b)| a == b).count();
        if pc.len() >= 4 && common + 4 < pc.len() {
            flips.push(format!(
                "  k={} rewrite {} chars back: '{}' -> '{}'",
                k,
                pc.len() - common,
                prev,
                cur
            ));
        }
        prev = cur;
    }
    println!("{}\tmax={}ms\tlen={}\tflips={}\tpinned={}", prev, max_ms, chars.len(), flips.len(), pinned.len());
    for f in flips {
        println!("{}", f);
    }
}

/// `simulate_typing` に加えて、hook-dll の自動部分確定（`stable_prefix_segments`）
/// を模擬する: 読みが長くなったら先頭側の文節を確定扱いにして再変換対象から外す。
/// IME_STAB_MIN / IME_STAB_KEEP でしきい値を上書きできる。
fn simulate_typing_stabilized(conv: &ViterbiConverter, reading: &str) {
    let min_chars: usize = std::env::var("IME_STAB_MIN").ok().and_then(|v| v.parse().ok())
        .unwrap_or(common::viterbi::STABILIZE_MIN_READING_CHARS);
    let keep_tail: usize = std::env::var("IME_STAB_KEEP").ok().and_then(|v| v.parse().ok())
        .unwrap_or(common::viterbi::STABILIZE_KEEP_TAIL_CHARS);
    let chars: Vec<char> = reading.chars().collect();
    let mut fixed = String::new();
    let mut buffer = String::new();
    let mut prev = String::new();
    let mut max_ms = 0u128;
    let mut flips = Vec::new();
    let mut cuts = 0usize;
    for (k, ch) in chars.iter().enumerate() {
        buffer.push(*ch);
        let t0 = std::time::Instant::now();
        let mut entries = conv.convert_context_aware(&buffer);
        let n = common::viterbi::stable_prefix_segments(&entries, min_chars, keep_tail);
        if n > 0 {
            let fixed_reading: usize = entries[..n].iter().map(|e| e.reading.chars().count()).sum();
            for e in &entries[..n] {
                fixed.push_str(&e.surface);
            }
            buffer = buffer.chars().skip(fixed_reading).collect();
            entries = conv.convert_context_aware(&buffer);
            cuts += 1;
        }
        max_ms = max_ms.max(t0.elapsed().as_millis());
        let tail: String = entries.iter().map(|e| e.surface.as_str()).collect();
        let cur = format!("{}{}", fixed, tail);
        let pc: Vec<char> = prev.chars().collect();
        let cc: Vec<char> = cur.chars().collect();
        let common = pc.iter().zip(cc.iter()).take_while(|(a, b)| a == b).count();
        if pc.len() >= 4 && common + 4 < pc.len() {
            flips.push(format!(
                "  k={} rewrite {} chars back: '{}' -> '{}'",
                k + 1,
                pc.len() - common,
                prev,
                cur
            ));
        }
        prev = cur;
    }
    println!("{}\tmax={}ms\tlen={}\tflips={}\tcuts={}", prev, max_ms, chars.len(), flips.len(), cuts);
    for f in flips {
        println!("{}", f);
    }
}

/// 1文字漢字の文節 A の直後に漢字を含む文節 B が続き、A+B の表記が辞書に無いのに
/// A+B（または A+B+C）の読みに対応する辞書語がある箇所を報告する。
fn flag_single_kanji_fragments(conv: &ViterbiConverter, segs: &[WordEntry]) {
    for i in 0..segs.len().saturating_sub(1) {
        let a = &segs[i];
        let mut ch = a.surface.chars();
        let single = matches!((ch.next(), ch.next()), (Some(c), None) if is_kanji(c));
        if !single {
            continue;
        }
        for span in 2..=3usize.min(segs.len() - i) {
            let group = &segs[i..i + span];
            if !group[1..].iter().any(|e| e.surface.chars().any(is_kanji)) {
                continue;
            }
            let r: String = group.iter().map(|e| e.reading.as_str()).collect();
            let s: String = group.iter().map(|e| e.surface.as_str()).collect();
            let Some(alts) = conv.dictionary.lookup(&r) else { continue };
            if alts.iter().any(|e| e.surface == s) {
                println!("  compound-ok: {} ({})", s, r);
                continue;
            }
            let mut alts: Vec<_> = alts
                .iter()
                .map(|e| (conv.effective_word_cost(e), e.cost, e.surface.clone(), e.pos.clone()))
                .collect();
            alts.sort();
            let path_cost: i32 = group.iter().map(|e| conv.effective_word_cost(e)).sum();
            let detail: Vec<String> = group
                .iter()
                .map(|e| {
                    format!(
                        "{}[{} raw={} eff={}]",
                        e.surface,
                        e.pos,
                        e.cost,
                        conv.effective_word_cost(e)
                    )
                })
                .collect();
            println!("  path: {}", detail.join(" + "));
            println!(
                "  FLAG: {} ({}) path_word_cost={} -> {:?}",
                s,
                r,
                path_cost,
                &alts[..alts.len().min(4)]
            );
        }
    }
}

/// 変換時間の内訳（ラティス構築 / Viterbi / 連想リランク）とラティス規模を表示する
fn profile(conv: &ViterbiConverter, reading: &str) {
    let t0 = std::time::Instant::now();
    let mut lattice = conv.build_lattice(reading);
    let t_build = t0.elapsed();
    let nodes = lattice.nodes.len();
    let edges: usize = (0..=reading.len())
        .map(|p| lattice.nodes_starting_at[p].len() * lattice.nodes_ending_at[p].len())
        .sum();
    let t1 = std::time::Instant::now();
    conv.find_best_path(&mut lattice);
    let t_path = t1.elapsed();
    let t2 = std::time::Instant::now();
    let _ = conv.convert_context_aware(reading);
    let t_all = t2.elapsed();
    println!(
        "len={} nodes={} edges={} build={}ms path={}ms full(convert_context_aware)={}ms",
        reading.chars().count(), nodes, edges, t_build.as_millis(), t_path.as_millis(), t_all.as_millis()
    );
}
