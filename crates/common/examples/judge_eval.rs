//! 判断層（judge-lm）の評価とパラメータ調整。
//!
//! `dict-builder judge-train` が出力した評価用データ（学習に使っていない
//! Wikipedia の文節、`読み\t正解表記\t分かち書き`）に対し、従来エンジン
//! （判断層なし）と判断層ありの正解率を比べる。総合スコアは
//! `LM対数確率 − 辞書コスト/dict_scale` なので、並び順を決めるのは
//! dict_scale だけ。これを格子探索し、続けて確率の較正用に softmax の温度を
//! 正解候補の負の対数尤度が最小になるように決める。
//!
//! 使い方:
//!   cargo run --release -p common --example judge_eval -- <judge_lm.bin> <dev.tsv> [件数] [--write]
//! `--write` を付けると決めたパラメータを judge_lm.bin に書き戻す。
//! 環境変数 JUDGE_SCALE（dict_scale）/ JUDGE_UNK（語彙外1文字あたりの罰）/ JUDGE_SEED
//! （コロケーション1組あたりの加点）で採用値を固定できる。
//! 学習DBは読まない（誰の環境でも同じ結果になるように）。
use common::judge::{softmax, JudgeLm, JudgeParams};
use common::{Dictionary, ViterbiConverter};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

struct Case {
    reading: String,
    gold: String,
    baseline: String,
    /// (表記, 語彙外罰を除くLM対数確率＋コロケーション加点, 語彙外の文字数, 辞書コスト)
    cands: Vec<(String, f32, u32, i32)>,
}

fn score(c: &(String, f32, u32, i32), alpha: f32, unk_char: f32) -> f32 {
    c.1 + unk_char * c.2 as f32 - alpha * c.3 as f32
}

fn pick(case: &Case, alpha: f32, unk_char: f32) -> &str {
    // 同点は先頭（従来の結果）を優先する
    let mut best = 0usize;
    let mut best_score = f32::NEG_INFINITY;
    for (i, c) in case.cands.iter().enumerate() {
        let s = score(c, alpha, unk_char);
        if s > best_score {
            best_score = s;
            best = i;
        }
    }
    &case.cands[best].0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("使い方: judge_eval <judge_lm.bin> <dev.tsv> [件数] [--write]");
        std::process::exit(2);
    }
    let lm_path = Path::new(&args[1]);
    let dev_path = Path::new(&args[2]);
    let limit: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3000);
    let write = args.iter().any(|a| a == "--write");

    let dict_path = Path::new("dictionaries/system.dic");
    let mut conv = ViterbiConverter::new(Dictionary::load(dict_path).expect("辞書ロード失敗"));
    let _ = conv.load_word_priority_file(&dict_path.with_file_name("word_priority.tsv"));
    let _ = conv.load_word_assoc_file(&dict_path.with_file_name("word_assoc.tsv"));

    let t = Instant::now();
    let mut judge = JudgeLm::load(lm_path).expect("judge_lm ロード失敗");
    eprintln!(
        "judge_lm: 語彙 {} / バイグラム {}（読み込み {}ms）",
        judge.vocab_len(),
        judge.bigram_len(),
        t.elapsed().as_millis()
    );

    let file = std::io::BufReader::new(std::fs::File::open(dev_path).expect("dev.tsv が開けません"));
    let rows: Vec<(String, String)> = file
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| {
            let mut it = l.split('\t');
            Some((it.next()?.to_string(), it.next()?.to_string()))
        })
        .take(limit)
        .collect();

    // 判断層を一時的に付けて候補を集める（パラメータに依存しない素の値を記録）
    let arc = Arc::new(JudgeLm::load(lm_path).expect("judge_lm ロード失敗"));
    let mut cases = Vec::with_capacity(rows.len());
    let mut base_time = 0u128;
    let mut judge_time = 0u128;
    for (reading, gold) in rows {
        conv.set_judge(None);
        let t = Instant::now();
        let base = conv.convert_context_aware(&reading);
        base_time += t.elapsed().as_micros();
        conv.set_judge(Some(Arc::clone(&arc)));
        let t = Instant::now();
        let judged = conv.judge_candidates(&reading, &[], &base);
        judge_time += t.elapsed().as_micros();
        // 従来の結果を先頭に置く（同点時の優先）
        let mut cands: Vec<(String, f32, u32, i32)> = Vec::new();
        for j in judged.iter().filter(|j| j.is_baseline).chain(judged.iter().filter(|j| !j.is_baseline)) {
            let seed = arc.params().seed_bonus * j.score.seed_hits as f32;
            cands.push((j.surface(), j.score.lm_base + seed, j.score.unk_chars, j.score.dict_cost));
        }
        cases.push(Case {
            reading,
            gold,
            baseline: base.iter().map(|e| e.surface.as_str()).collect(),
            cands,
        });
    }
    conv.set_judge(None);
    let n = cases.len().max(1) as f64;
    eprintln!(
        "{} 件 / 平均時間: 従来 {:.2}ms, 判断層の追加分 {:.2}ms",
        cases.len(),
        base_time as f64 / n / 1000.0,
        judge_time as f64 / n / 1000.0
    );

    let base_ok = cases.iter().filter(|c| c.baseline == c.gold).count();
    let oracle = cases.iter().filter(|c| c.cands.iter().any(|x| x.0 == c.gold)).count();
    println!("従来エンジン正解率: {:.1}% ({}/{})", 100.0 * base_ok as f64 / n, base_ok, cases.len());
    println!("候補内に正解がある率（上限）: {:.1}%", 100.0 * oracle as f64 / n);

    // dict_scale（= 1/alpha）と語彙外の文字数罰の格子探索。alpha=0 は LM だけで選ぶ
    let alphas = [0.0f32, 1.0 / 16000.0, 1.0 / 8000.0, 1.0 / 4000.0, 1.0 / 2000.0, 1.0 / 1000.0, 1.0 / 500.0];
    let unk_chars = [0.0f32, -0.5, -1.0, -2.0, -3.0, -5.0];
    let mut best = (0f32, 0f32, 0usize);
    for &alpha in &alphas {
        let scale = if alpha > 0.0 { format!("{:.0}", 1.0 / alpha) } else { "∞".into() };
        let mut line = format!("  dict_scale={:>6}:", scale);
        for &uc in &unk_chars {
            let ok = cases.iter().filter(|c| pick(c, alpha, uc) == c.gold).count();
            line.push_str(&format!("  unk{:+.1}={:.1}%", uc, 100.0 * ok as f64 / n));
            if ok > best.2 {
                best = (alpha, uc, ok);
            }
        }
        println!("{}", line);
    }
    // 環境変数で採用値を固定できる（開発用データは Wikipedia なので、
    // 辞書コストを捨てる方向に偏りやすい。実運用向けには辞書側の重みを残す）
    let alpha = std::env::var("JUDGE_SCALE").ok().and_then(|s| s.parse::<f32>().ok()).map(|v| 1.0 / v).unwrap_or(best.0);
    let unk_char = std::env::var("JUDGE_UNK").ok().and_then(|s| s.parse::<f32>().ok()).unwrap_or(best.1);
    let best = (alpha, unk_char, cases.iter().filter(|c| pick(c, alpha, unk_char) == c.gold).count());
    let fixed = cases.iter().filter(|c| c.baseline != c.gold && pick(c, alpha, unk_char) == c.gold).count();
    let broken = cases.iter().filter(|c| c.baseline == c.gold && pick(c, alpha, unk_char) != c.gold).count();
    println!(
        "採用: dict_scale={} unk_char={} → 正解率 {:.1}%（改善 {} 件 / 悪化 {} 件）",
        if alpha > 0.0 { format!("{:.0}", 1.0 / alpha) } else { "∞".into() },
        unk_char,
        100.0 * best.2 as f64 / n,
        fixed,
        broken
    );

    // 温度の較正（正解が候補内にあるケースの負の対数尤度を最小化）
    let mut best_t = (1.0f32, f64::INFINITY);
    for &temp in &[0.25f32, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0] {
        let mut nll = 0f64;
        for c in &cases {
            let Some(gi) = c.cands.iter().position(|x| x.0 == c.gold) else { continue };
            let scores: Vec<f32> = c.cands.iter().map(|x| score(x, alpha, unk_char)).collect();
            nll -= (softmax(&scores, temp)[gi].max(1e-9) as f64).ln();
        }
        if nll < best_t.1 {
            best_t = (temp, nll);
        }
    }
    println!("softmax 温度: {}", best_t.0);

    // 判断層による変化の例（改善・悪化それぞれ数件）
    let show = |label: &str, f: &dyn Fn(&Case) -> bool| {
        println!("--- {} の例 ---", label);
        for c in cases.iter().filter(|c| f(c)).take(12) {
            println!("  {}  正解={}  従来={}  判断層={}", c.reading, c.gold, c.baseline, pick(c, alpha, unk_char));
        }
    };
    show("改善", &|c| c.baseline != c.gold && pick(c, alpha, unk_char) == c.gold);
    show("悪化", &|c| c.baseline == c.gold && pick(c, alpha, unk_char) != c.gold);

    if write {
        let dict_scale = if alpha > 0.0 { 1.0 / alpha } else { 1e9 };
        let seed_bonus = std::env::var("JUDGE_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(judge.params().seed_bonus);
        judge.set_params(JudgeParams { lm_weight: 1.0, dict_scale, temperature: best_t.0, unk_char_logp: unk_char, seed_bonus });
        JudgeLm::save_data(judge.data(), lm_path).expect("保存失敗");
        println!("パラメータを書き戻しました: {}", lm_path.display());
    }
}
