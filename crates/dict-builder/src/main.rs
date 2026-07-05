use anyhow::{Context, Result};
use common::{Dictionary, WordEntry, ConnectionMatrix};
use encoding_rs::EUC_JP;
use flate2::write::GzEncoder;
use flate2::Compression;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter};
use std::path::Path;

/// シリアライズ可能な辞書形式
#[derive(Serialize, Deserialize)]
struct SerializableDictionary {
    words: Vec<SerializableWordEntry>,
    matrix_left_size: u16,
    matrix_right_size: u16,
    matrix_costs: Vec<i16>,
    bos_id: u16,
    eos_id: u16,
}

#[derive(Serialize, Deserialize)]
struct SerializableWordEntry {
    surface: String,
    reading: String,
    left_id: u16,
    right_id: u16,
    cost: i16,
    pos: String,
}

/// IPA辞書のCSVエントリをパース
fn parse_csv_line(line: &str) -> Option<WordEntry> {
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 13 {
        return None;
    }

    let surface = parts[0].to_string();
    let left_id: u16 = parts[1].parse().ok()?;
    let right_id: u16 = parts[2].parse().ok()?;
    let cost: i16 = parts[3].parse().ok()?;
    let pos = format!("{}-{}-{}-{}", parts[4], parts[5], parts[6], parts[7]);
    
    // 読みをひらがなに変換（カタカナ→ひらがな）
    let reading_katakana = parts[11];
    let reading = katakana_to_hiragana(reading_katakana);

    // かな読みを持たないエントリは変換で引けないので除外する
    // （記号類は読みが "*" になっている）
    if reading.is_empty()
        || !reading
            .chars()
            .all(|c| ('\u{3041}'..='\u{3096}').contains(&c) || c == 'ー')
    {
        return None;
    }

    // 「読みをかな表記しただけ」の表記ゆれエントリを除外する
    // （例: ツクり/動詞, コンニチワ/感動詞）。
    // IPA辞書は解析用のため、こうした変種が正規表記より低コストな場合が
    // あり、読み→表層の逆引き（かな漢字変換）ではノイズになる。
    // ただし名詞はカタカナ語（ツール、テレビ等）が正規表記なので残す。
    let surface_as_hiragana = katakana_to_hiragana(&surface);
    if surface != reading && surface_as_hiragana == reading && !pos.starts_with("名詞") {
        return None;
    }

    Some(WordEntry {
        surface,
        reading,
        left_id,
        right_id,
        cost,
        pos,
    })
}

/// カタカナをひらがなに変換
fn katakana_to_hiragana(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if ('\u{30A1}'..='\u{30F6}').contains(&c) {
                // カタカナ→ひらがな
                char::from_u32(c as u32 - 0x60).unwrap_or(c)
            } else if c == '\u{30FC}' {
                // 長音符
                'ー'
            } else {
                c
            }
        })
        .collect()
}

/// matrix.defを読み込む
fn load_matrix(path: &Path) -> Result<ConnectionMatrix> {
    let file = File::open(path).context("matrix.defを開けません")?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    // 最初の行でサイズを取得
    let first_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("matrix.defが空です"))??;
    let sizes: Vec<u16> = first_line
        .split_whitespace()
        .filter_map(|s| s.parse::<u16>().ok())
        .collect();

    if sizes.len() < 2 {
        anyhow::bail!("matrix.defのフォーマットが不正です");
    }

    let left_size = sizes[0];
    let right_size = sizes[1];
    let mut matrix = ConnectionMatrix::new(left_size, right_size);

    println!("連接行列サイズ: {} x {}", left_size, right_size);

    // 連接コストを読み込む
    // matrix.def の各行は「前の語の右文脈ID 次の語の左文脈ID コスト」。
    // ConnectionMatrix::set/get も (前の右ID, 次の左ID) の順で受けるので
    // そのままの順で渡す（逆に渡すと行列が転置され変換精度が壊れる）。
    for line_result in lines {
        let line = line_result?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            if let (Ok(prev_right_id), Ok(next_left_id), Ok(cost)) = (
                parts[0].parse::<u16>(),
                parts[1].parse::<u16>(),
                parts[2].parse::<i16>(),
            ) {
                matrix.set(prev_right_id, next_left_id, cost);
            }
        }
    }

    Ok(matrix)
}

/// CSVファイルから単語を読み込む（EUC-JP対応）
fn load_csv_file(path: &Path) -> Result<Vec<WordEntry>> {
    let content = fs::read(path)?;
    
    // EUC-JPからUTF-8に変換
    let (decoded, _, had_errors) = EUC_JP.decode(&content);
    if had_errors {
        eprintln!("警告: {} のデコード中にエラーがありました", path.display());
    }

    let mut entries = Vec::new();
    for line in decoded.lines() {
        if let Some(entry) = parse_csv_line(line) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

/// UTF-8のCSVファイルから単語を読み込む
fn load_utf8_csv_file(path: &Path) -> Result<Vec<WordEntry>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();

    for line_result in reader.lines() {
        let line = line_result?;
        if let Some(entry) = parse_csv_line(&line) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

/// IPA辞書ディレクトリから辞書を構築
fn build_dictionary(dict_dir: &Path) -> Result<Dictionary> {
    let mut dict = Dictionary::new();
    let mut total_words = 0;

    // matrix.defを読み込む
    let matrix_path = dict_dir.join("matrix.def");
    if matrix_path.exists() {
        dict.matrix = load_matrix(&matrix_path)?;
        println!("連接行列を読み込みました");
    } else {
        println!("警告: matrix.defが見つかりません。デフォルトの連接行列を使用します。");
    }

    // CSVファイルを読み込む
    let csv_files: Vec<_> = fs::read_dir(dict_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map_or(false, |ext| ext == "csv")
        })
        .collect();

    if csv_files.is_empty() {
        anyhow::bail!("CSVファイルが見つかりません");
    }

    let pb = ProgressBar::new(csv_files.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );

    for entry in csv_files {
        let path = entry.path();
        pb.set_message(format!("{}", path.file_name().unwrap_or_default().to_string_lossy()));

        // まずUTF-8として試す、失敗したらEUC-JPとして読む
        let entries = match load_utf8_csv_file(&path) {
            Ok(e) if !e.is_empty() => e,
            _ => load_csv_file(&path).unwrap_or_default(),
        };

        for entry in entries {
            dict.add_word(entry);
            total_words += 1;
        }

        pb.inc(1);
    }

    pb.finish_with_message(format!("完了: {} 単語", total_words));
    println!("総単語数: {}", total_words);

    Ok(dict)
}

/// 辞書をバイナリ形式で保存
fn save_dictionary(dict: &Dictionary, output_path: &Path) -> Result<()> {
    // Trieから全単語を抽出
    let mut words = Vec::new();
    collect_words_from_trie(&dict.trie, &mut words);

    let serializable = SerializableDictionary {
        words: words
            .into_iter()
            .map(|w| SerializableWordEntry {
                surface: w.surface,
                reading: w.reading,
                left_id: w.left_id,
                right_id: w.right_id,
                cost: w.cost,
                pos: w.pos,
            })
            .collect(),
        matrix_left_size: dict.matrix.left_size,
        matrix_right_size: dict.matrix.right_size,
        matrix_costs: dict.matrix.costs.clone(),
        bos_id: dict.bos_id,
        eos_id: dict.eos_id,
    };

    // bincode + gzip で圧縮保存
    let file = File::create(output_path)?;
    let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
    bincode::serialize_into(encoder, &serializable)?;

    println!("辞書を保存しました: {}", output_path.display());
    Ok(())
}

/// Trieから全単語を収集
fn collect_words_from_trie(node: &common::TrieNode, words: &mut Vec<WordEntry>) {
    for entry in &node.entries {
        words.push(entry.clone());
    }
    for child in node.children.values() {
        collect_words_from_trie(child, words);
    }
}

/// 辞書をバイナリファイルから読み込む
pub fn load_dictionary(path: &Path) -> Result<Dictionary> {
    let file = File::open(path)?;
    let decoder = flate2::read::GzDecoder::new(BufReader::new(file));
    let serializable: SerializableDictionary = bincode::deserialize_from(decoder)?;

    let mut dict = Dictionary::new();
    dict.matrix = ConnectionMatrix {
        left_size: serializable.matrix_left_size,
        right_size: serializable.matrix_right_size,
        costs: serializable.matrix_costs,
    };
    dict.bos_id = serializable.bos_id;
    dict.eos_id = serializable.eos_id;

    for entry in serializable.words {
        dict.add_word(WordEntry {
            surface: entry.surface,
            reading: entry.reading,
            left_id: entry.left_id,
            right_id: entry.right_id,
            cost: entry.cost,
            pos: entry.pos,
        });
    }

    Ok(dict)
}

/// 補助CSV（読み,表記[,コスト]）から辞書に語を追加する
///
/// 名詞一般（左右文脈ID=1285）として登録し、既定コストは 4000（一般語より
/// 少し優先）。既に同じ読み+表記があっても重複して追加されるが、Viterbi 上は
/// 低コスト側が使われるので実害はない。
fn extend_from_csv(dict: &mut Dictionary, csv_path: &Path) -> Result<usize> {
    use common::WordEntry;
    let content = fs::read_to_string(csv_path)
        .with_context(|| format!("補助CSVを読めません: {}", csv_path.display()))?;
    let mut added = 0;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if cols.len() < 2 || cols[0].is_empty() || cols[1].is_empty() {
            continue;
        }
        let reading = cols[0].to_string();
        let surface = cols[1].to_string();
        let cost: i16 = cols
            .get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or(4000);
        dict.add_word(WordEntry {
            surface,
            reading,
            left_id: 1285, // 名詞-一般
            right_id: 1285,
            cost,
            pos: "名詞-一般-*-*".to_string(),
        });
        added += 1;
    }
    Ok(added)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        println!("使い方:");
        println!("  {} build <IPA辞書ディレクトリ> [出力ファイル]", args[0]);
        println!("  {} extend <辞書ファイル> <補助CSV>  (常用語を追加)", args[0]);
        println!("  {} test <辞書ファイル>", args[0]);
        println!();
        println!("例:");
        println!("  {} build ./ipadic ./dictionaries/system.dic", args[0]);
        println!("  {} extend ./dictionaries/system.dic ./dictionaries/extra.csv", args[0]);
        println!("  {} test ./dictionaries/system.dic", args[0]);
        return Ok(());
    }

    match args[1].as_str() {
        "build" => {
            if args.len() < 3 {
                anyhow::bail!("IPA辞書ディレクトリを指定してください");
            }
            let dict_dir = Path::new(&args[2]);
            let output_path = if args.len() >= 4 {
                Path::new(&args[3]).to_path_buf()
            } else {
                Path::new("dictionaries/system.dic").to_path_buf()
            };

            // 出力ディレクトリを作成
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }

            println!("IPA辞書を読み込んでいます: {}", dict_dir.display());
            let dict = build_dictionary(dict_dir)?;
            save_dictionary(&dict, &output_path)?;
        }
        "extend" => {
            if args.len() < 4 {
                anyhow::bail!("使い方: extend <辞書ファイル> <補助CSV>");
            }
            let dict_path = Path::new(&args[2]);
            let csv_path = Path::new(&args[3]);
            let mut dict = load_dictionary(dict_path)?;
            let added = extend_from_csv(&mut dict, csv_path)?;
            save_dictionary(&dict, dict_path)?;
            println!("補助辞書から {} 語を追加しました: {}", added, dict_path.display());
        }
        "test" => {
            if args.len() < 3 {
                anyhow::bail!("辞書ファイルを指定してください");
            }
            let dict_path = Path::new(&args[2]);
            println!("辞書を読み込んでいます: {}", dict_path.display());
            let dict = load_dictionary(dict_path)?;

            // テスト変換
            use common::ViterbiConverter;
            let converter = ViterbiConverter::new(dict);

            let test_cases = [
                "きょう",
                "きょうは",
                "こんにちは",
                "ありがとう",
            ];

            println!("\n=== 変換テスト ===");
            for input in &test_cases {
                let result = converter.convert_to_string(input);
                println!("{} → {}", input, result);
            }
        }
        _ => {
            anyhow::bail!("不明なコマンド: {}", args[1]);
        }
    }

    Ok(())
}
