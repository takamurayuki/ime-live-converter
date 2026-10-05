//! 判断層（judge-lm）の学習: 1行1文のコーパスを vibrato(IPADic) で分かち書きし、
//! (表記, 読み, 文脈ID) 単位の Kneser-Ney 単語バイグラムと、そのバックオフ先の
//! 文法クラスモデル（文脈ID どうしの遷移確率 × クラス内の語の出現確率）を作る。
//!
//! - 文脈IDは IME の辞書（system.dic）と同じ MeCab IPADic の番号にそろえる。
//!   vibrato の配布モデルは文脈IDを独自に振り直しているため、トークンの
//!   (表記, 素性) を IPADic の CSV で引き直して IPADic の番号を得る。
//! - 句読点・括弧などの記号と、読みの無い語（英数字・未知語）は「文節の
//!   区切り」として扱い、そこで文頭(BOS)からやり直す。ライブ変換も句読点で
//!   自動確定して次の入力が文頭から始まるので、それに合わせている。
//! - 一定間隔の文は学習に使わず、評価用データ（読み→正解表記）に回す
//!   （`judge_eval` で従来エンジンとの正解率比較・パラメータ調整に使う）。

use anyhow::{Context, Result};
use common::judge::{vocab_key, JudgeLm, JudgeLmData, JudgeParams, LmCtx, BOS_ID, EOS_ID};
use encoding_rs::EUC_JP;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::mpsc;

pub struct TrainOptions {
    /// この回数未満しか出ない語は語彙に入れない
    pub min_unigram: u64,
    /// この回数未満のバイグラムは保存しない（バックオフで近似する）
    pub min_bigram: u32,
    /// この回数未満のトライグラムは保存しない（0 ならトライグラムを作らない）
    pub min_trigram: u32,
    /// N 文に1文を評価用に回す
    pub dev_every: usize,
    /// 評価用データの最大件数
    pub max_dev: usize,
    /// 学習に使う最大文数（None なら全部）
    pub max_sentences: Option<usize>,
    pub threads: usize,
}

/// 分かち書き済みの1文節（(語キー, 文脈ID) の列）
type Clause = Vec<(String, u16)>;

struct ChunkResult {
    clauses: Vec<Clause>,
    /// (文番号, 読み, 正解表記, 分かち書き)
    dev: Vec<(usize, String, String, String)>,
}

/// IPADic の CSV を読み、"表記\t素性" → 文脈ID の表を作る（素性は CSV の
/// 5列目以降。vibrato のトークン素性と同じ並び）。IPADic は左右の文脈IDが
/// 常に同じなので左IDだけ持つ。
fn load_ipadic_ids(dir: &Path) -> Result<(HashMap<String, u16>, u16)> {
    let mut map = HashMap::new();
    let mut max_id = 0u16;
    for entry in std::fs::read_dir(dir).with_context(|| format!("IPADic ディレクトリを開けません: {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().map_or(true, |e| e != "csv") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let (text, _, _) = EUC_JP.decode(&bytes);
        for line in text.lines() {
            let mut it = line.splitn(5, ',');
            let (Some(surface), Some(left), Some(_right), Some(_cost), Some(feature)) =
                (it.next(), it.next(), it.next(), it.next(), it.next())
            else {
                continue;
            };
            let Ok(left) = left.parse::<u16>() else { continue };
            max_id = max_id.max(left);
            map.insert(format!("{}\t{}", surface, feature), left);
        }
    }
    anyhow::ensure!(!map.is_empty(), "IPADic の CSV が見つかりません: {}", dir.display());
    Ok((map, max_id))
}

fn katakana_to_hiragana(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{30A1}'..='\u{30F6}' => char::from_u32(c as u32 - 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

fn is_reading_char(c: char) -> bool {
    ('\u{3041}'..='\u{3096}').contains(&c) || c == 'ー'
}

/// 1文を分かち書きし、文節（記号・未知語で区切った語列）に分ける。
/// 各文節は (語列, 読み, 表記, 分かち書き表示) を返す。
fn tokenize_clauses(
    worker: &mut vibrato::tokenizer::worker::Worker<'_>,
    ids: &HashMap<String, u16>,
    sentence: &str,
) -> Vec<(Clause, String, String, Vec<String>)> {
    worker.reset_sentence(sentence);
    worker.tokenize();
    let mut out = Vec::new();
    let mut words: Clause = Vec::new();
    let mut reading = String::new();
    let mut surface = String::new();
    let mut shown: Vec<String> = Vec::new();
    let mut lookup = String::new();
    for i in 0..worker.num_tokens() {
        let t = worker.token(i);
        let feature = t.feature();
        let s = t.surface();
        let pos = feature.split(',').next().unwrap_or("");
        let r = feature.split(',').nth(7).map(katakana_to_hiragana).unwrap_or_default();
        lookup.clear();
        lookup.push_str(s);
        lookup.push('\t');
        lookup.push_str(feature);
        let class = ids.get(&lookup).copied();
        let usable = pos != "記号" && !r.is_empty() && r.chars().all(is_reading_char) && !s.trim().is_empty();
        match (usable, class) {
            (true, Some(class)) => {
                words.push((vocab_key(s, &r, class), class));
                reading.push_str(&r);
                surface.push_str(s);
                shown.push(format!("{}/{}", s, r));
            }
            _ => {
                if !words.is_empty() {
                    out.push((
                        std::mem::take(&mut words),
                        std::mem::take(&mut reading),
                        std::mem::take(&mut surface),
                        std::mem::take(&mut shown),
                    ));
                } else {
                    reading.clear();
                    surface.clear();
                    shown.clear();
                }
            }
        }
    }
    if !words.is_empty() {
        out.push((words, reading, surface, shown));
    }
    out
}

/// 評価用データに使える文節か（漢字を含み、長さが手頃）
fn is_dev_clause(words: &Clause, reading: &str, surface: &str) -> bool {
    let n = reading.chars().count();
    words.len() >= 2
        && (5..=30).contains(&n)
        && surface != reading
        && surface.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// 集計結果
struct Counts {
    words: Vec<String>,
    word_class: Vec<u16>,
    uni: Vec<u64>,
    bigrams: HashMap<u64, u32>,
    /// トライグラムの回数（語IDを21ビットずつ詰めたキー）
    trigrams: HashMap<u64, u32>,
    /// クラス遷移の回数（`R * n_class + L`）
    class_bi: Vec<u64>,
    n_class: usize,
}

pub fn train(
    vibrato_dict: &Path,
    ipadic_dir: &Path,
    corpus: &Path,
    output: &Path,
    dev_output: &Path,
    opts: &TrainOptions,
) -> Result<()> {
    eprintln!("IPADic の文脈ID表を読み込んでいます: {}", ipadic_dir.display());
    let (ids, max_id) = load_ipadic_ids(ipadic_dir)?;
    eprintln!("vibrato辞書を読み込んでいます: {}", vibrato_dict.display());
    let reader = zstd::Decoder::new(File::open(vibrato_dict)?)?;
    let tokenizer = vibrato::Tokenizer::new(vibrato::Dictionary::read(reader)?);

    let n_class = max_id as usize + 1;
    let mut counts = Counts {
        words: vec!["<s>".into(), "</s>".into()],
        word_class: vec![0, 0],
        uni: vec![0, 0],
        bigrams: HashMap::new(),
        trigrams: HashMap::new(),
        class_bi: vec![0; n_class * n_class],
        n_class,
    };
    let mut vocab: HashMap<String, u32> = HashMap::new();
    let mut dev: Vec<(usize, String, String, String)> = Vec::new();
    let mut total_tokens: u64 = 0;

    let file = BufReader::with_capacity(1 << 20, File::open(corpus)?);
    let (job_tx, job_rx) = mpsc::sync_channel::<(usize, Vec<String>)>(opts.threads * 2);
    let job_rx = std::sync::Mutex::new(job_rx);
    let (res_tx, res_rx) = mpsc::sync_channel::<ChunkResult>(opts.threads * 2);
    const CHUNK: usize = 5000;

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..opts.threads {
            let res_tx = res_tx.clone();
            let job_rx = &job_rx;
            let tokenizer = &tokenizer;
            let ids = &ids;
            let dev_every = opts.dev_every;
            scope.spawn(move || {
                let mut worker = tokenizer.new_worker();
                loop {
                    let job = job_rx.lock().unwrap().recv();
                    let Ok((base, lines)) = job else { break };
                    let mut result = ChunkResult { clauses: Vec::new(), dev: Vec::new() };
                    for (k, line) in lines.iter().enumerate() {
                        let idx = base + k;
                        let is_dev = dev_every > 0 && idx % dev_every == 0;
                        for (words, reading, surface, shown) in tokenize_clauses(&mut worker, ids, line) {
                            if is_dev {
                                if is_dev_clause(&words, &reading, &surface) {
                                    result.dev.push((idx, reading, surface, shown.join(" ")));
                                }
                            } else {
                                result.clauses.push(words);
                            }
                        }
                    }
                    if res_tx.send(result).is_err() {
                        break;
                    }
                }
            });
        }
        drop(res_tx);

        // 読み込みスレッド
        let max_sentences = opts.max_sentences;
        scope.spawn(move || -> Result<()> {
            let mut base = 0usize;
            let mut buf = Vec::with_capacity(CHUNK);
            for line in file.lines() {
                let line = line?;
                if max_sentences.map_or(false, |m| base + buf.len() >= m) {
                    break;
                }
                buf.push(line);
                if buf.len() == CHUNK {
                    let n = buf.len();
                    if job_tx.send((base, std::mem::replace(&mut buf, Vec::with_capacity(CHUNK)))).is_err() {
                        break;
                    }
                    base += n;
                }
            }
            if !buf.is_empty() {
                let _ = job_tx.send((base, buf));
            }
            Ok(())
        });

        // 集計（このスレッド）
        let mut chunks = 0usize;
        for result in res_rx {
            for clause in result.clauses {
                let mut prev2: Option<u32> = None;
                let mut prev = BOS_ID;
                let mut prev_class = 0usize;
                counts.uni[BOS_ID as usize] += 1;
                for (key, class) in clause {
                    let id = match vocab.get(&key) {
                        Some(&id) => id,
                        None => {
                            let id = counts.words.len() as u32;
                            vocab.insert(key.clone(), id);
                            counts.words.push(key);
                            counts.word_class.push(class);
                            counts.uni.push(0);
                            id
                        }
                    };
                    counts.uni[id as usize] += 1;
                    total_tokens += 1;
                    *counts.bigrams.entry(((prev as u64) << 32) | id as u64).or_insert(0) += 1;
                    if let (Some(u), true) = (prev2, opts.min_trigram > 0) {
                        if let Some(key) = tri_key(u, prev, id) {
                            *counts.trigrams.entry(key).or_insert(0) += 1;
                        }
                    }
                    counts.class_bi[prev_class * n_class + class as usize] += 1;
                    prev2 = Some(prev);
                    prev = id;
                    prev_class = class as usize;
                }
                if let (Some(u), true) = (prev2, opts.min_trigram > 0) {
                    if let Some(key) = tri_key(u, prev, EOS_ID) {
                        *counts.trigrams.entry(key).or_insert(0) += 1;
                    }
                }
                *counts.bigrams.entry(((prev as u64) << 32) | EOS_ID as u64).or_insert(0) += 1;
                counts.class_bi[prev_class * n_class] += 1;
                counts.uni[EOS_ID as usize] += 1;
            }
            dev.extend(result.dev);
            chunks += 1;
            if chunks % 200 == 0 {
                eprintln!(
                    "  {} 文 / {} 語 / 語彙 {} / バイグラム {} / トライグラム {}",
                    chunks * CHUNK,
                    total_tokens,
                    counts.words.len(),
                    counts.bigrams.len(),
                    counts.trigrams.len()
                );
            }
        }
        Ok(())
    })?;

    eprintln!(
        "集計完了: {} 語 / 語彙 {} / バイグラム {} / クラス {}",
        total_tokens,
        counts.words.len(),
        counts.bigrams.len(),
        n_class
    );

    let (mut data, new_id) = build_model(&counts, opts);
    let mut counts = counts;
    counts.bigrams = HashMap::new(); // 以降は不要。トライグラム構築前にメモリを空ける
    if opts.min_trigram > 0 {
        add_trigrams(&mut data, &counts.trigrams, &new_id, opts.min_trigram);
    }
    eprintln!(
        "モデル: 語彙 {} / バイグラム {} / トライグラム {}（割引・バックオフ重みは全件から算出）",
        data.vocab.len(),
        data.bi_next.len(),
        data.tri_next.len()
    );
    JudgeLm::save_data(&data, output)?;
    eprintln!("保存しました: {}", output.display());

    dev.sort_by_key(|d| d.0);
    dev.truncate(opts.max_dev);
    let mut w = BufWriter::new(File::create(dev_output)?);
    for (_, reading, surface, shown) in &dev {
        writeln!(w, "{}\t{}\t{}", reading, surface, shown)?;
    }
    w.flush()?;
    eprintln!("評価用データ {} 件: {}", dev.len(), dev_output.display());
    Ok(())
}

/// 集計結果から「Kneser-Ney 割引の単語バイグラム ＋ 文法クラスモデルへのバックオフ」を作る
///
/// P(w | v) = max(c(v,w) − D, 0) / c(v) + γ(v) · P(L(w) | R(v)) · P(w | L(w))
/// γ(v) = D · N1+(v・) / c(v)
fn build_model(c: &Counts, opts: &TrainOptions) -> (JudgeLmData, Vec<u32>) {
    let n = c.words.len();
    let nc = c.n_class;

    // 文脈としての回数 c(v) と異なり数 N1+(v・)、割引 D
    let mut ctx_count = vec![0u64; n];
    let mut ctx_types = vec![0u32; n];
    let (mut n1, mut n2) = (0u64, 0u64);
    for (&key, &cnt) in &c.bigrams {
        let v = (key >> 32) as usize;
        ctx_count[v] += cnt as u64;
        ctx_types[v] += 1;
        match cnt {
            1 => n1 += 1,
            2 => n2 += 1,
            _ => {}
        }
    }
    let discount = if n1 + n2 > 0 { n1 as f64 / (n1 as f64 + 2.0 * n2 as f64) } else { 0.75 };
    eprintln!("Kneser-Ney 割引 D = {:.3}", discount);

    // クラス遷移 P(L | R)（加算平滑化。文法的にありえない遷移ほど小さくなる）
    let alpha = 0.1f64;
    let mut class_logp = vec![0f32; nc * nc];
    for r in 0..nc {
        let row = &c.class_bi[r * nc..(r + 1) * nc];
        let total: u64 = row.iter().sum();
        let denom = total as f64 + alpha * nc as f64;
        for l in 0..nc {
            class_logp[r * nc + l] = ((row[l] as f64 + alpha) / denom).ln() as f32;
        }
    }

    // クラスごとの語の総数と、刈り込まれる語の総数・異なり数
    let mut class_tokens = vec![0u64; nc];
    let mut pruned_tokens = vec![0u64; nc];
    let mut pruned_types = vec![0u64; nc];
    for i in 2..n {
        let cls = c.word_class[i] as usize;
        class_tokens[cls] += c.uni[i];
        if c.uni[i] < opts.min_unigram {
            pruned_tokens[cls] += c.uni[i];
            pruned_types[cls] += 1;
        }
    }
    let unk_emit_logp: Vec<f32> = (0..nc)
        .map(|l| {
            if class_tokens[l] == 0 {
                -20.0
            } else {
                // 刈り込まれた語1つあたりの平均確率（そのクラスの「未知の語」の確率）
                (pruned_tokens[l].max(1) as f64 / class_tokens[l] as f64 / pruned_types[l].max(1) as f64).ln() as f32
            }
        })
        .collect();

    // 語彙の刈り込み（BOS/EOS は必ず残す）
    let mut new_id = vec![u32::MAX; n];
    let mut keep = Vec::new();
    for i in 0..n {
        if i < 2 || c.uni[i] >= opts.min_unigram {
            new_id[i] = keep.len() as u32;
            keep.push(i);
        }
    }
    let emit = |i: usize| -> f64 {
        if i < 2 {
            1.0
        } else {
            c.uni[i] as f64 / class_tokens[c.word_class[i] as usize].max(1) as f64
        }
    };
    let backoff = |v: usize| -> f64 {
        if ctx_count[v] > 0 {
            discount * ctx_types[v] as f64 / ctx_count[v] as f64
        } else {
            1.0
        }
    };

    let mut entries: Vec<(u32, u32, f32)> = Vec::new();
    for (&key, &cnt) in &c.bigrams {
        if cnt < opts.min_bigram {
            continue;
        }
        let (v, w) = ((key >> 32) as usize, (key & 0xFFFF_FFFF) as usize);
        let (nv, nw) = (new_id[v], new_id[w]);
        if nv == u32::MAX || nw == u32::MAX {
            continue;
        }
        let class_p = (class_logp[c.word_class[v] as usize * nc + c.word_class[w] as usize] as f64).exp();
        let p = (cnt as f64 - discount).max(0.0) / ctx_count[v] as f64 + backoff(v) * class_p * emit(w);
        entries.push((nv, nw, p.ln() as f32));
    }
    entries.sort_unstable_by_key(|e| (e.0, e.1));
    let v_size = keep.len();
    let mut bi_offsets = vec![0u32; v_size + 1];
    for e in &entries {
        bi_offsets[e.0 as usize + 1] += 1;
    }
    for i in 0..v_size {
        bi_offsets[i + 1] += bi_offsets[i];
    }

    let data = JudgeLmData {
        vocab: keep.iter().map(|&i| c.words[i].clone()).collect(),
        word_class: keep.iter().map(|&i| c.word_class[i]).collect(),
        emit_logp: keep.iter().map(|&i| emit(i).ln() as f32).collect(),
        backoff: keep.iter().map(|&i| backoff(i).ln() as f32).collect(),
        bi_offsets,
        bi_next: entries.iter().map(|e| e.1).collect(),
        bi_logp: entries.iter().map(|e| e.2).collect(),
        tri_offsets: Vec::new(),
        tri_next: Vec::new(),
        tri_logp: Vec::new(),
        tri_backoff: Vec::new(),
        n_class: nc as u32,
        class_logp,
        unk_emit_logp,
        params_json: serde_json::to_string(&JudgeParams::default()).unwrap_or_default(),
    };
    (data, new_id)
}

/// トライグラムのキー（語IDを21ビットずつ詰める。収まらない語IDなら None）
const TRI_BITS: u32 = 21;
fn tri_key(u: u32, v: u32, w: u32) -> Option<u64> {
    let max = 1u32 << TRI_BITS;
    (u < max && v < max && w < max).then(|| ((u as u64) << (2 * TRI_BITS)) | ((v as u64) << TRI_BITS) | w as u64)
}
fn tri_unkey(key: u64) -> (u32, u32, u32) {
    let mask = (1u64 << TRI_BITS) - 1;
    ((key >> (2 * TRI_BITS)) as u32, ((key >> TRI_BITS) & mask) as u32, (key & mask) as u32)
}

/// できあがったバイグラム＋クラスモデルの上にトライグラムを足す
///
/// P(w | u, v) = max(c(u,v,w) − D3, 0) / c(u,v) + γ(u,v) · P(w | v)
/// γ(u,v) = D3 · N1+(u,v,・) / c(u,v)
/// 文脈 (u, v) はバイグラム表の添字で表すので、保存されていないバイグラムを
/// 文脈とするトライグラムは持たない（そこではバイグラムに落ちる）。
fn add_trigrams(data: &mut JudgeLmData, trigrams: &HashMap<u64, u32>, new_id: &[u32], min_trigram: u32) {
    // 文脈 (u, v) ごとの回数と異なり数、割引 D3
    let mut ctx: HashMap<u64, (u64, u32)> = HashMap::new();
    let (mut n1, mut n2) = (0u64, 0u64);
    for (&key, &cnt) in trigrams {
        let (u, v, _) = tri_unkey(key);
        let e = ctx.entry(((u as u64) << 32) | v as u64).or_insert((0, 0));
        e.0 += cnt as u64;
        e.1 += 1;
        match cnt {
            1 => n1 += 1,
            2 => n2 += 1,
            _ => {}
        }
    }
    let discount = if n1 + n2 > 0 { n1 as f64 / (n1 as f64 + 2.0 * n2 as f64) } else { 0.75 };
    eprintln!("トライグラム: {} 種 / 文脈 {} / 割引 D3 = {:.3}", trigrams.len(), ctx.len(), discount);

    let lm = JudgeLm::from_data(std::mem::take(data));
    let class = |id: u32| lm.data().word_class[id as usize];
    let mut entries: Vec<(u32, u32, f32)> = Vec::new(); // (バイグラム添字, w, logp)
    for (&key, &cnt) in trigrams {
        if cnt < min_trigram {
            continue;
        }
        let (u, v, w) = tri_unkey(key);
        let (nu, nv, nw) = (new_id[u as usize], new_id[v as usize], new_id[w as usize]);
        if nu == u32::MAX || nv == u32::MAX || nw == u32::MAX {
            continue;
        }
        let Some(b) = lm.bigram_index(nu, nv) else { continue };
        let (c_uv, types) = ctx[&(((u as u64) << 32) | v as u64)];
        let gamma = discount * types as f64 / c_uv as f64;
        let p2 = lm.logp(LmCtx { word: Some(nv), class: class(nv) }, Some(nw), class(nw)) as f64;
        let p = (cnt as f64 - discount).max(0.0) / c_uv as f64 + gamma * p2.exp();
        entries.push((b as u32, nw, p.ln() as f32));
    }
    entries.sort_unstable_by_key(|e| (e.0, e.1));

    // 保存したトライグラムがある文脈だけバックオフ重みを持たせる（無い文脈は
    // バイグラムそのものを使う＝重み1）
    let mut gamma_of: HashMap<u32, f32> = HashMap::new();
    for (&key, &(c_uv, types)) in &ctx {
        let (u, v) = ((key >> 32) as u32, (key & 0xFFFF_FFFF) as u32);
        let (nu, nv) = (new_id[u as usize], new_id[v as usize]);
        if nu == u32::MAX || nv == u32::MAX {
            continue;
        }
        if let Some(b) = lm.bigram_index(nu, nv) {
            gamma_of.insert(b as u32, (discount * types as f64 / c_uv as f64).ln() as f32);
        }
    }

    let mut data_out = lm.into_data();
    let n_bi = data_out.bi_next.len();
    let mut tri_offsets = vec![0u32; n_bi + 1];
    for e in &entries {
        tri_offsets[e.0 as usize + 1] += 1;
    }
    for i in 0..n_bi {
        tri_offsets[i + 1] += tri_offsets[i];
    }
    let mut tri_backoff = vec![0f32; n_bi];
    for b in 0..n_bi {
        if tri_offsets[b + 1] > tri_offsets[b] {
            if let Some(&g) = gamma_of.get(&(b as u32)) {
                tri_backoff[b] = g;
            }
        }
    }
    data_out.tri_offsets = tri_offsets;
    data_out.tri_next = entries.iter().map(|e| e.1).collect();
    data_out.tri_logp = entries.iter().map(|e| e.2).collect();
    data_out.tri_backoff = tri_backoff;
    *data = data_out;
}
