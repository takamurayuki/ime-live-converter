//! 判断層の共起モデル（`common::judge_cooc`）の学習: Wikipedia の各文について、
//! 同音異義のある名詞（対象）と同じ文の他の名詞（文脈）の組を数え、PMI を作る。
//!
//! - 対象は IME の辞書（system.dic）で同じ読みに名詞の表記が2つ以上あるもの
//!   （汽車/記者、政策/製作 等）。変換で選び分けが必要な語だけ数えることで、
//!   組の数（メモリ）を抑える。
//! - 学習に使う文・評価用データに回す文の分け方は `judge-train` と同じ
//!   （N 文に1文を評価用）。評価用データには、同じ文の前の文節に出た名詞を
//!   「直前に確定した文の名詞」として4列目に付ける（`judge_eval` が共起の文脈に使う）。

use crate::judge_train::{is_dev_clause, is_reading_char, katakana_to_hiragana, load_ipadic_ids, Clause};
use anyhow::Result;
use common::judge::vocab_key;
use common::judge_cooc::{is_cooc_noun, is_cooc_target_entry, JudgeCooc, JudgeCoocData};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::mpsc;

pub struct CoocOptions {
    /// 組がこの回数未満なら捨てる
    pub min_pair: u32,
    /// 文脈語がこの文数未満にしか出ないなら捨てる
    pub min_ctx: u32,
    /// この PMI 未満の組は捨てる（弱い結びつきは加点しても効かない）
    pub min_pmi: f32,
    pub dev_every: usize,
    pub max_dev: usize,
    pub max_sentences: Option<usize>,
    pub threads: usize,
}

/// 1文あたりに数える名詞の上限（長い列挙文で組が爆発しないように）
const MAX_NOUNS_PER_SENTENCE: usize = 40;
/// 評価用データに付ける文脈名詞の上限（実行時の `CONTEXT_NOUNS_MAX` と同じ）
const MAX_CTX: usize = 12;

/// 1文の分かち書き結果: (文中の名詞（重複なし）, 評価用の文節列（文節, 読み, 表記, 表示, その文節より前の名詞）)
type SentenceOut = (Vec<String>, Vec<(Clause, String, String, Vec<String>, Vec<String>)>);

fn analyze(
    worker: &mut vibrato::tokenizer::worker::Worker<'_>,
    ids: &HashMap<String, u16>,
    sentence: &str,
) -> SentenceOut {
    worker.reset_sentence(sentence);
    worker.tokenize();
    let mut nouns: Vec<String> = Vec::new();
    let mut clauses = Vec::new();
    let mut words: Clause = Vec::new();
    let (mut reading, mut surface) = (String::new(), String::new());
    let mut shown: Vec<String> = Vec::new();
    // 現在の文節より前に確定した名詞（新しい順）
    let mut before: Vec<String> = Vec::new();
    let mut clause_nouns: Vec<String> = Vec::new();
    let mut lookup = String::new();
    let flush = |words: &mut Clause,
                 reading: &mut String,
                 surface: &mut String,
                 shown: &mut Vec<String>,
                 clause_nouns: &mut Vec<String>,
                 before: &mut Vec<String>,
                 clauses: &mut Vec<_>| {
        if !words.is_empty() {
            clauses.push((
                std::mem::take(words),
                std::mem::take(reading),
                std::mem::take(surface),
                std::mem::take(shown),
                before.clone(),
            ));
        }
        reading.clear();
        surface.clear();
        shown.clear();
        for n in clause_nouns.drain(..) {
            before.retain(|b| b != &n);
            before.insert(0, n);
        }
        before.truncate(MAX_CTX);
    };
    for i in 0..worker.num_tokens() {
        let t = worker.token(i);
        let feature = t.feature();
        let s = t.surface();
        let mut f = feature.split(',');
        let (pos1, pos2) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
        if is_cooc_noun(pos1, pos2, s) {
            if !nouns.iter().any(|n| n == s) && nouns.len() < MAX_NOUNS_PER_SENTENCE {
                nouns.push(s.to_string());
            }
            clause_nouns.push(s.to_string());
        }
        let r = feature.split(',').nth(7).map(katakana_to_hiragana).unwrap_or_default();
        lookup.clear();
        lookup.push_str(s);
        lookup.push('\t');
        lookup.push_str(feature);
        let class = ids.get(&lookup).copied();
        let usable = pos1 != "記号" && !r.is_empty() && r.chars().all(is_reading_char) && !s.trim().is_empty();
        match (usable, class) {
            (true, Some(class)) => {
                words.push((vocab_key(s, &r, class), class));
                reading.push_str(&r);
                surface.push_str(s);
                shown.push(format!("{}/{}", s, r));
            }
            _ => flush(&mut words, &mut reading, &mut surface, &mut shown, &mut clause_nouns, &mut before, &mut clauses),
        }
    }
    flush(&mut words, &mut reading, &mut surface, &mut shown, &mut clause_nouns, &mut before, &mut clauses);
    (nouns, clauses)
}

/// IME 辞書で同じ読みに名詞の表記が2つ以上ある名詞（共起の対象）
fn homophone_nouns(dict_path: &Path) -> Result<HashSet<String>> {
    let dict = common::Dictionary::load(dict_path)?;
    let mut by_reading: HashMap<String, HashSet<String>> = HashMap::new();
    let mut stack: Vec<&common::TrieNode> = vec![&dict.trie];
    while let Some(node) = stack.pop() {
        for e in &node.entries {
            if is_cooc_target_entry(&e.pos, &e.surface) {
                by_reading.entry(e.reading.clone()).or_default().insert(e.surface.clone());
            }
        }
        stack.extend(node.children.values().map(|c| c.as_ref()));
    }
    Ok(by_reading.into_values().filter(|s| s.len() >= 2).flatten().collect())
}

pub fn train(
    vibrato_dict: &Path,
    ipadic_dir: &Path,
    system_dic: &Path,
    corpus: &Path,
    output: &Path,
    dev_output: &Path,
    opts: &CoocOptions,
) -> Result<()> {
    let targets = homophone_nouns(system_dic)?;
    eprintln!("共起の対象（同音異義のある名詞）: {} 語", targets.len());
    let (ids, _) = load_ipadic_ids(ipadic_dir)?;
    let reader = zstd::Decoder::new(File::open(vibrato_dict)?)?;
    let tokenizer = vibrato::Tokenizer::new(vibrato::Dictionary::read(reader)?);

    let mut noun_index: HashMap<String, u32> = HashMap::new();
    let mut nouns: Vec<String> = Vec::new();
    let mut is_target: Vec<bool> = Vec::new();
    let mut sent_freq: Vec<u32> = Vec::new();
    let mut pairs: HashMap<u64, u32> = HashMap::new();
    let mut n_sentences: u64 = 0;
    let mut dev: Vec<(usize, String, String, String, String)> = Vec::new();

    let file = BufReader::with_capacity(1 << 20, File::open(corpus)?);
    let (job_tx, job_rx) = mpsc::sync_channel::<(usize, Vec<String>)>(opts.threads * 2);
    let job_rx = std::sync::Mutex::new(job_rx);
    type Batch = (Vec<Vec<String>>, Vec<(usize, String, String, String, String)>);
    let (res_tx, res_rx) = mpsc::sync_channel::<Batch>(opts.threads * 2);
    const CHUNK: usize = 5000;

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..opts.threads {
            let res_tx = res_tx.clone();
            let (job_rx, tokenizer, ids) = (&job_rx, &tokenizer, &ids);
            let dev_every = opts.dev_every;
            scope.spawn(move || {
                let mut worker = tokenizer.new_worker();
                loop {
                    let job = job_rx.lock().unwrap().recv();
                    let Ok((base, lines)) = job else { break };
                    let mut sents = Vec::new();
                    let mut devs = Vec::new();
                    for (k, line) in lines.iter().enumerate() {
                        let idx = base + k;
                        let (nouns, clauses) = analyze(&mut worker, ids, line);
                        if dev_every > 0 && idx % dev_every == 0 {
                            for (words, reading, surface, shown, before) in clauses {
                                if is_dev_clause(&words, &reading, &surface) {
                                    devs.push((idx, reading, surface, shown.join(" "), before.join(" ")));
                                }
                            }
                        } else if nouns.len() >= 2 {
                            sents.push(nouns);
                        }
                    }
                    if res_tx.send((sents, devs)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(res_tx);

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

        let mut chunks = 0usize;
        let mut sent_ids: Vec<u32> = Vec::new();
        for (sents, devs) in res_rx {
            for sent in sents {
                sent_ids.clear();
                for n in sent {
                    let id = match noun_index.get(&n) {
                        Some(&id) => id,
                        None => {
                            let id = nouns.len() as u32;
                            is_target.push(targets.contains(&n));
                            noun_index.insert(n.clone(), id);
                            nouns.push(n);
                            sent_freq.push(0);
                            id
                        }
                    };
                    sent_freq[id as usize] += 1;
                    sent_ids.push(id);
                }
                n_sentences += 1;
                for &t in &sent_ids {
                    if !is_target[t as usize] {
                        continue;
                    }
                    for &c in &sent_ids {
                        if c != t {
                            *pairs.entry(((t as u64) << 32) | c as u64).or_insert(0) += 1;
                        }
                    }
                }
            }
            dev.extend(devs);
            chunks += 1;
            if chunks % 200 == 0 {
                eprintln!("  {} 文 / 名詞 {} / 組 {}", chunks * CHUNK, nouns.len(), pairs.len());
            }
        }
        Ok(())
    })?;
    eprintln!("集計完了: 名詞を2つ以上含む文 {} / 名詞 {} / 組 {}", n_sentences, nouns.len(), pairs.len());

    // PMI を計算して残す組を決める
    let n = n_sentences.max(1) as f64;
    let mut kept: Vec<(u32, u32, f32)> = Vec::new();
    for (&key, &cnt) in &pairs {
        if cnt < opts.min_pair {
            continue;
        }
        let (t, c) = ((key >> 32) as u32, (key & 0xFFFF_FFFF) as u32);
        let (ft, fc) = (sent_freq[t as usize] as f64, sent_freq[c as usize] as f64);
        if fc < opts.min_ctx as f64 {
            continue;
        }
        let pmi = (cnt as f64 * n / (ft * fc)).ln() as f32;
        if pmi >= opts.min_pmi {
            kept.push((t, c, pmi));
        }
    }
    drop(pairs);
    // 使う名詞だけに詰め直す
    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut out_nouns: Vec<String> = Vec::new();
    for &(t, c, _) in &kept {
        for id in [t, c] {
            remap.entry(id).or_insert_with(|| {
                out_nouns.push(nouns[id as usize].clone());
                (out_nouns.len() - 1) as u32
            });
        }
    }
    let mut entries: Vec<(u32, u32, f32)> = kept.iter().map(|&(t, c, p)| (remap[&t], remap[&c], p)).collect();
    entries.sort_unstable_by_key(|e| (e.0, e.1));
    let mut offsets = vec![0u32; out_nouns.len() + 1];
    for e in &entries {
        offsets[e.0 as usize + 1] += 1;
    }
    for i in 0..out_nouns.len() {
        offsets[i + 1] += offsets[i];
    }
    let data = JudgeCoocData {
        nouns: out_nouns,
        offsets,
        ctx: entries.iter().map(|e| e.1).collect(),
        pmi: entries.iter().map(|e| e.2).collect(),
    };
    eprintln!("モデル: 名詞 {} / 組 {}", data.nouns.len(), data.ctx.len());
    JudgeCooc::save_data(&data, output)?;
    eprintln!("保存しました: {}", output.display());

    dev.sort_by_key(|d| d.0);
    dev.truncate(opts.max_dev);
    let mut w = BufWriter::new(File::create(dev_output)?);
    for (_, reading, surface, shown, before) in &dev {
        writeln!(w, "{}\t{}\t{}\t{}", reading, surface, shown, before)?;
    }
    w.flush()?;
    eprintln!("評価用データ {} 件: {}", dev.len(), dev_output.display());
    Ok(())
}
