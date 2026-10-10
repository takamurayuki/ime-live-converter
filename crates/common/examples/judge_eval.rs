//! 判断層（judge-lm）の評価とパラメータ調整。
//!
//! `dict-builder judge-train`（または `judge-cooc`）が出力した評価用データ（学習に
//! 使っていない Wikipedia の文節、`読み\t正解表記\t分かち書き[\t直前の文の名詞…]`）に
//! 対し、従来エンジン（判断層なし）と判断層ありの正解率を比べる。4列目があれば、
//! 同じ文で先に確定した文節の名詞として共起モデルの文脈に渡す。
//!
//! 総合スコアのうち並び順に効く重み（dict_scale・語彙外の文字数罰・共起の重み）を
//! 格子探索し、続けて確率の較正用に softmax の温度を正解候補の負の対数尤度が
//! 最小になるように決める。
//!
//! 使い方:
//!   cargo run --release -p common --example judge_eval -- <judge_lm.bin> <dev.tsv> [件数] [--write] [--cooc judge_cooc.bin]
//! `--write` を付けると決めたパラメータを judge_lm.bin に書き戻す。
//! 環境変数 JUDGE_SCALE（dict_scale）/ JUDGE_UNK（語彙外1文字あたりの罰）/ JUDGE_SEED
//! （コロケーション1組あたりの加点）/ JUDGE_LEARN / JUDGE_START_MIX / JUDGE_COOC_W
//! （共起の重み）/ JUDGE_COOC_CAP で採用値を固定できる。
//! 学習DBは読まない（誰の環境でも同じ結果になるように）。
use common::judge::{softmax, JudgeLm, JudgeParams};
use common::judge_cooc::JudgeCooc;
use common::{Dictionary, ViterbiConverter};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// 1候補の素点
struct Cand {
    surface: String,
    /// 重みを振らない部分: 語彙外罰を除く LM 対数確率＋コロケーション＋個人学習
    fixed: f32,
    unk_chars: u32,
    dict_cost: i32,
    /// 共起の加点（重み前）
    cooc: f32,
}

struct Case {
    reading: String,
    gold: String,
    baseline: String,
    cands: Vec<Cand>,
}

#[derive(Clone, Copy)]
struct Weights {
    alpha: f32,
    unk_char: f32,
    cooc: f32,
}

fn score(c: &Cand, w: Weights) -> f32 {
    c.fixed + w.unk_char * c.unk_chars as f32 - w.alpha * c.dict_cost as f32 + w.cooc * c.cooc
}

fn pick(case: &Case, w: Weights) -> &str {
    // 同点は先頭（従来の結果）を優先する
    let mut best = 0usize;
    let mut best_score = f32::NEG_INFINITY;
    for (i, c) in case.cands.iter().enumerate() {
        let s = score(c, w);
        if s > best_score {
            best_score = s;
            best = i;
        }
    }
    &case.cands[best].surface
}

fn env_f32(name: &str) -> Option<f32> {
    std::env::var(name).ok().and_then(|s| s.parse().ok())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("使い方: judge_eval <judge_lm.bin> <dev.tsv> [件数] [--write] [--cooc judge_cooc.bin]");
        std::process::exit(2);
    }
    let lm_path = Path::new(&args[1]);
    let dev_path = Path::new(&args[2]);
    let limit: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3000);
    let write = args.iter().any(|a| a == "--write");
    let cooc_path = args.iter().position(|a| a == "--cooc").and_then(|i| args.get(i + 1));

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
    if let Some(p) = cooc_path {
        let cooc = JudgeCooc::load(Path::new(p)).expect("judge_cooc ロード失敗");
        eprintln!("judge_cooc: 名詞 {} / 組 {}", cooc.noun_count(), cooc.pair_count());
        conv.set_cooc(Some(Arc::new(cooc)));
    }

    let file = std::io::BufReader::new(std::fs::File::open(dev_path).expect("dev.tsv が開けません"));
    let rows: Vec<(String, String, Vec<String>)> = file
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| {
            let mut it = l.split('\t');
            let reading = it.next()?.to_string();
            let gold = it.next()?.to_string();
            let _shown = it.next();
            let ctx = it.next().map(|c| c.split(' ').filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default();
            Some((reading, gold, ctx))
        })
        .take(limit)
        .collect();
    let with_ctx = rows.iter().filter(|r| !r.2.is_empty()).count();

    // 候補集めは共起の重みを大きめにして、共起で浮上する経路も候補に入れる
    // （素点は重みを除いて記録し、並べ替えの格子探索で重みを振る）
    let mut gen_params = judge.params();
    gen_params.cooc_weight = env_f32("JUDGE_COOC_GEN").unwrap_or(2.0);
    let mut gen_judge = JudgeLm::load(lm_path).expect("judge_lm ロード失敗");
    gen_judge.set_params(gen_params);
    let arc = Arc::new(gen_judge);
    let mut cases = Vec::with_capacity(rows.len());
    let mut base_time = 0u128;
    let mut judge_time = 0u128;
    for (reading, gold, ctx) in rows {
        conv.context_nouns = ctx;
        conv.set_judge(None);
        let t = Instant::now();
        let base = conv.convert_context_aware(&reading);
        base_time += t.elapsed().as_micros();
        conv.set_judge(Some(Arc::clone(&arc)));
        let t = Instant::now();
        let judged = conv.judge_candidates(&reading, &[], &base);
        judge_time += t.elapsed().as_micros();
        // 従来の結果を先頭に置く（同点時の優先）
        let p = arc.params();
        let cands = judged
            .iter()
            .filter(|j| j.is_baseline)
            .chain(judged.iter().filter(|j| !j.is_baseline))
            .map(|j| Cand {
                surface: j.surface(),
                fixed: j.score.lm_base + p.seed_bonus * j.score.seed_hits as f32 + j.score.learned as f32 / p.learn_scale,
                unk_chars: j.score.unk_chars,
                dict_cost: j.score.dict_cost,
                cooc: j.score.cooc,
            })
            .collect();
        cases.push(Case { reading, gold, baseline: base.iter().map(|e| e.surface.as_str()).collect(), cands });
    }
    conv.set_judge(None);
    let n = cases.len().max(1) as f64;
    eprintln!(
        "{} 件（うち前の文節の文脈つき {} 件）/ 平均時間: 従来 {:.2}ms, 判断層の追加分 {:.2}ms",
        cases.len(),
        with_ctx,
        base_time as f64 / n / 1000.0,
        judge_time as f64 / n / 1000.0
    );

    let accuracy = |w: Weights| cases.iter().filter(|c| pick(c, w) == c.gold).count();
    let base_ok = cases.iter().filter(|c| c.baseline == c.gold).count();
    let oracle = cases.iter().filter(|c| c.cands.iter().any(|x| x.surface == c.gold)).count();
    println!("従来エンジン正解率: {:.1}% ({}/{})", 100.0 * base_ok as f64 / n, base_ok, cases.len());
    println!("候補内に正解がある率（上限）: {:.1}%", 100.0 * oracle as f64 / n);

    // dict_scale（= 1/alpha）と語彙外の文字数罰の格子探索（共起の重みは現在値）
    let cooc_now = env_f32("JUDGE_COOC_W").unwrap_or(judge.params().cooc_weight);
    let alphas = [0.0f32, 1.0 / 16000.0, 1.0 / 8000.0, 1.0 / 4000.0, 1.0 / 2000.0, 1.0 / 1000.0, 1.0 / 500.0];
    let unk_chars = [0.0f32, -0.5, -1.0, -2.0, -3.0, -5.0];
    let mut best = (Weights { alpha: 0.0, unk_char: 0.0, cooc: cooc_now }, 0usize);
    for &alpha in &alphas {
        let scale = if alpha > 0.0 { format!("{:.0}", 1.0 / alpha) } else { "∞".into() };
        let mut line = format!("  dict_scale={:>6}:", scale);
        for &uc in &unk_chars {
            let w = Weights { alpha, unk_char: uc, cooc: cooc_now };
            let ok = accuracy(w);
            line.push_str(&format!("  unk{:+.1}={:.1}%", uc, 100.0 * ok as f64 / n));
            if ok > best.1 {
                best = (w, ok);
            }
        }
        println!("{}", line);
    }
    // 環境変数で採用値を固定できる（開発用データは Wikipedia なので、
    // 辞書コストを捨てる方向に偏りやすい。実運用向けには辞書側の重みを残す）
    let mut w = best.0;
    if let Some(v) = env_f32("JUDGE_SCALE") {
        w.alpha = 1.0 / v;
    }
    if let Some(v) = env_f32("JUDGE_UNK") {
        w.unk_char = v;
    }
    // 共起の重みの探索
    if conv.cooc.is_some() {
        let mut line = String::from("  共起の重み:");
        let mut best_c = (w.cooc, 0usize);
        for &cw in &[0.0f32, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0] {
            let ok = accuracy(Weights { cooc: cw, ..w });
            line.push_str(&format!("  {:.2}={:.2}%", cw, 100.0 * ok as f64 / n));
            if ok > best_c.1 {
                best_c = (cw, ok);
            }
        }
        println!("{}", line);
        w.cooc = env_f32("JUDGE_COOC_W").unwrap_or(best_c.0);
    }
    let ok = accuracy(w);
    let fixed = cases.iter().filter(|c| c.baseline != c.gold && pick(c, w) == c.gold).count();
    let broken = cases.iter().filter(|c| c.baseline == c.gold && pick(c, w) != c.gold).count();
    println!(
        "採用: dict_scale={} unk_char={} 共起={} → 正解率 {:.1}%（改善 {} 件 / 悪化 {} 件）",
        if w.alpha > 0.0 { format!("{:.0}", 1.0 / w.alpha) } else { "∞".into() },
        w.unk_char,
        w.cooc,
        100.0 * ok as f64 / n,
        fixed,
        broken
    );
    if conv.cooc.is_some() {
        let no_cooc = Weights { cooc: 0.0, ..w };
        let gained = cases.iter().filter(|c| pick(c, no_cooc) != c.gold && pick(c, w) == c.gold).count();
        let lost = cases.iter().filter(|c| pick(c, no_cooc) == c.gold && pick(c, w) != c.gold).count();
        println!("  共起による変化: 改善 {} 件 / 悪化 {} 件", gained, lost);
        println!("--- 共起で改善 の例 ---");
        for c in cases.iter().filter(|c| pick(c, no_cooc) != c.gold && pick(c, w) == c.gold).take(12) {
            println!("  {}  正解={}  共起なし={}", c.reading, c.gold, pick(c, no_cooc));
        }
        println!("--- 共起で悪化 の例 ---");
        for c in cases.iter().filter(|c| pick(c, no_cooc) == c.gold && pick(c, w) != c.gold).take(12) {
            println!("  {}  正解={}  共起あり={}", c.reading, c.gold, pick(c, w));
        }
    }

    // 温度の較正（正解が候補内にあるケースの負の対数尤度を最小化）
    let mut best_t = (1.0f32, f64::INFINITY);
    for &temp in &[0.25f32, 0.5, 0.75, 1.0, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0] {
        let mut nll = 0f64;
        for c in &cases {
            let Some(gi) = c.cands.iter().position(|x| x.surface == c.gold) else { continue };
            let scores: Vec<f32> = c.cands.iter().map(|x| score(x, w)).collect();
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
            println!("  {}  正解={}  従来={}  判断層={}", c.reading, c.gold, c.baseline, pick(c, w));
        }
    };
    show("改善", &|c| c.baseline != c.gold && pick(c, w) == c.gold);
    show("悪化", &|c| c.baseline == c.gold && pick(c, w) != c.gold);

    if write {
        let p = judge.params();
        judge.set_params(JudgeParams {
            lm_weight: 1.0,
            dict_scale: if w.alpha > 0.0 { 1.0 / w.alpha } else { 1e9 },
            temperature: best_t.0,
            unk_char_logp: w.unk_char,
            seed_bonus: env_f32("JUDGE_SEED").unwrap_or(p.seed_bonus),
            learn_scale: env_f32("JUDGE_LEARN").unwrap_or(p.learn_scale),
            start_mix: env_f32("JUDGE_START_MIX").unwrap_or(p.start_mix),
            cooc_weight: w.cooc,
            cooc_cap: env_f32("JUDGE_COOC_CAP").unwrap_or(p.cooc_cap),
        });
        JudgeLm::save_data(judge.data(), lm_path).expect("保存失敗");
        println!("パラメータを書き戻しました: {}", lm_path.display());
    }
}
