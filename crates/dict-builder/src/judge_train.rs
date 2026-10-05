//! 判断層（judge-lm）の学習: 1行1文のコーパスを vibrato(IPADic) で分かち書きし、
//! (表記, 読み) 単位の Kneser-Ney 平滑化つき単語バイグラムを作る。
//!
//! - 句読点・括弧などの記号と、読みの無い語（英数字・未知語）は「文節の
//!   区切り」として扱い、そこで文頭(BOS)からやり直す。ライブ変換も句読点で
//!   自動確定して次の入力が文頭から始まるので、それに合わせている。
//! - 一定間隔の文は学習に使わず、評価用データ（読み→正解表記）に回す
//!   （`judge_eval` で従来エンジンとの正解率比較・パラメータ調整に使う）。

use anyhow::Result;
use common::judge::{JudgeLm, JudgeLmData, JudgeParams, BOS_ID, EOS_ID};
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
    /// N 文に1文を評価用に回す
    pub dev_every: usize,
    /// 評価用データの最大件数
    pub max_dev: usize,
    /// 学習に使う最大文数（None なら全部）
    pub max_sentences: Option<usize>,
    pub threads: usize,
}

/// 分かち書き済みの1文節（語キー列）
type Clause = Vec<String>;

struct ChunkResult {
    clauses: Vec<Clause>,
    /// (文番号, 読み, 正解表記, 分かち書き)
    dev: Vec<(usize, String, String, String)>,
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

/// 1文を分かち書きし、文節（記号・未知語で区切った語キー列）に分ける。
/// 各文節は (語キー列, 読み, 表記, 分かち書き表示) を返す。
fn tokenize_clauses(
    worker: &mut vibrato::tokenizer::worker::Worker<'_>,
    sentence: &str,
) -> Vec<(Clause, String, String, Vec<String>)> {
    worker.reset_sentence(sentence);
    worker.tokenize();
    let mut out = Vec::new();
    let mut keys: Clause = Vec::new();
    let mut reading = String::new();
    let mut surface = String::new();
    let mut shown: Vec<String> = Vec::new();
    let flush = |keys: &mut Clause, reading: &mut String, surface: &mut String, shown: &mut Vec<String>, out: &mut Vec<_>| {
        if !keys.is_empty() {
            out.push((std::mem::take(keys), std::mem::take(reading), std::mem::take(surface), std::mem::take(shown)));
        } else {
            reading.clear();
            surface.clear();
            shown.clear();
        }
    };
    for i in 0..worker.num_tokens() {
        let t = worker.token(i);
        let feature = t.feature();
        let mut fields = feature.split(',');
        let pos = fields.next().unwrap_or("");
        let reading_kana = feature.split(',').nth(7);
        let r = reading_kana.map(katakana_to_hiragana).unwrap_or_default();
        let s = t.surface();
        if pos == "記号" || r.is_empty() || !r.chars().all(is_reading_char) || s.trim().is_empty() {
            flush(&mut keys, &mut reading, &mut surface, &mut shown, &mut out);
            continue;
        }
        keys.push(format!("{}\t{}", s, r));
        reading.push_str(&r);
        surface.push_str(s);
        shown.push(format!("{}/{}", s, r));
    }
    flush(&mut keys, &mut reading, &mut surface, &mut shown, &mut out);
    out
}

/// 評価用データに使える文節か（漢字を含み、長さが手頃）
fn is_dev_clause(keys: &Clause, reading: &str, surface: &str) -> bool {
    let n = reading.chars().count();
    keys.len() >= 2 && (5..=30).contains(&n) && surface != reading && surface.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

pub fn train(
    vibrato_dict: &Path,
    corpus: &Path,
    output: &Path,
    dev_output: &Path,
    opts: &TrainOptions,
) -> Result<()> {
    eprintln!("vibrato辞書を読み込んでいます: {}", vibrato_dict.display());
    let reader = zstd::Decoder::new(File::open(vibrato_dict)?)?;
    let tokenizer = vibrato::Tokenizer::new(vibrato::Dictionary::read(reader)?);

    let mut vocab: HashMap<String, u32> = HashMap::new();
    let mut words: Vec<String> = vec!["<s>".into(), "</s>".into()];
    let mut uni: Vec<u64> = vec![0, 0];
    let mut bigrams: HashMap<u64, u32> = HashMap::new();
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
                        for (keys, reading, surface, shown) in tokenize_clauses(&mut worker, line) {
                            if is_dev {
                                if is_dev_clause(&keys, &reading, &surface) {
                                    result.dev.push((idx, reading, surface, shown.join(" ")));
                                }
                            } else {
                                result.clauses.push(keys);
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
                let mut prev = BOS_ID;
                uni[BOS_ID as usize] += 1;
                for key in clause {
                    let id = match vocab.get(&key) {
                        Some(&id) => id,
                        None => {
                            let id = words.len() as u32;
                            vocab.insert(key.clone(), id);
                            words.push(key);
                            uni.push(0);
                            id
                        }
                    };
                    uni[id as usize] += 1;
                    total_tokens += 1;
                    *bigrams.entry(((prev as u64) << 32) | id as u64).or_insert(0) += 1;
                    prev = id;
                }
                *bigrams.entry(((prev as u64) << 32) | EOS_ID as u64).or_insert(0) += 1;
                uni[EOS_ID as usize] += 1;
            }
            dev.extend(result.dev);
            chunks += 1;
            if chunks % 200 == 0 {
                eprintln!(
                    "  {} 文 / {} 語 / 語彙 {} / バイグラム {}",
                    chunks * CHUNK, total_tokens, words.len(), bigrams.len()
                );
            }
        }
        Ok(())
    })?;

    eprintln!("集計完了: {} 語 / 語彙 {} / バイグラム {}", total_tokens, words.len(), bigrams.len());

    let data = build_model(&words, &uni, &bigrams, total_tokens, opts);
    eprintln!(
        "モデル: 語彙 {} / バイグラム {}（D・バックオフは全件から算出）",
        data.vocab.len(),
        data.bi_next.len()
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

/// 集計結果から補間 Kneser-Ney バイグラムを作る
fn build_model(
    words: &[String],
    uni: &[u64],
    bigrams: &HashMap<u64, u32>,
    total_tokens: u64,
    opts: &TrainOptions,
) -> JudgeLmData {
    let n = words.len();
    let mut ctx_count = vec![0u64; n]; // c(v)
    let mut ctx_types = vec![0u32; n]; // N1+(v・)
    let mut cont_types = vec![0u32; n]; // N1+(・w)
    let (mut n1, mut n2) = (0u64, 0u64);
    for (&key, &c) in bigrams {
        let (v, w) = ((key >> 32) as usize, (key & 0xFFFF_FFFF) as usize);
        ctx_count[v] += c as u64;
        ctx_types[v] += 1;
        cont_types[w] += 1;
        match c {
            1 => n1 += 1,
            2 => n2 += 1,
            _ => {}
        }
    }
    let discount = if n1 + n2 > 0 { n1 as f64 / (n1 as f64 + 2.0 * n2 as f64) } else { 0.75 };
    eprintln!("Kneser-Ney 割引 D = {:.3}", discount);

    // 語彙の刈り込み（BOS/EOS は必ず残す）
    let mut new_id = vec![u32::MAX; n];
    let mut vocab = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if i < 2 || uni[i] >= opts.min_unigram {
            new_id[i] = vocab.len() as u32;
            vocab.push(w.clone());
        }
    }
    let v_size = vocab.len();
    let total_types: f64 = bigrams.len() as f64;
    let eps = 1e-4f64;
    let mut p_uni = vec![0f64; v_size];
    for (old, &nid) in new_id.iter().enumerate() {
        if nid != u32::MAX {
            p_uni[nid as usize] = (1.0 - eps) * cont_types[old] as f64 / total_types + eps / v_size as f64;
        }
    }
    let mut backoff = vec![0f64; v_size];
    for (old, &nid) in new_id.iter().enumerate() {
        if nid != u32::MAX {
            backoff[nid as usize] = if ctx_count[old] > 0 {
                discount * ctx_types[old] as f64 / ctx_count[old] as f64
            } else {
                1.0
            };
        }
    }

    let mut entries: Vec<(u32, u32, f32)> = Vec::new();
    for (&key, &c) in bigrams {
        if c < opts.min_bigram {
            continue;
        }
        let (v, w) = ((key >> 32) as usize, (key & 0xFFFF_FFFF) as usize);
        let (nv, nw) = (new_id[v], new_id[w]);
        if nv == u32::MAX || nw == u32::MAX {
            continue;
        }
        let p = (c as f64 - discount).max(0.0) / ctx_count[v] as f64
            + backoff[nv as usize] * p_uni[nw as usize];
        entries.push((nv, nw, p.ln() as f32));
    }
    entries.sort_unstable_by_key(|e| (e.0, e.1));
    let mut bi_offsets = vec![0u32; v_size + 1];
    for e in &entries {
        bi_offsets[e.0 as usize + 1] += 1;
    }
    for i in 0..v_size {
        bi_offsets[i + 1] += bi_offsets[i];
    }

    JudgeLmData {
        uni_logp: p_uni.iter().map(|p| p.ln() as f32).collect(),
        backoff: backoff.iter().map(|b| b.ln() as f32).collect(),
        bi_offsets,
        bi_next: entries.iter().map(|e| e.1).collect(),
        bi_logp: entries.iter().map(|e| e.2).collect(),
        // 語彙に入らなかった語（刈り込み閾値未満の頻度）相当の確率
        unk_logp: (opts.min_unigram as f64 / total_tokens.max(1) as f64).ln() as f32,
        vocab,
        params_json: serde_json::to_string(&JudgeParams::default()).unwrap_or_default(),
    }
}
