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

    // NEologd等のWeb由来の大規模データに混入する低品質エントリを除外する。
    // これらは通常のIPA辞書CSVには存在しないパターンなので、既存の基本
    // 辞書エントリには影響しない。
    let reading_len = reading.chars().count();
    // 1) 絵文字が表記に含まれるものは実用的な変換対象ではない
    //    （例: 読み「か」に絵文字「🉑」が割り当てられている等）。
    if surface.chars().any(|c| (c as u32) >= 0x1F000) {
        return None;
    }
    // 2) 表記に英字を含みながら読みが極端に短い（2文字以下）ものは、
    //    外国語の断片に誤って短い読みが割り当てられたゴミであることが
    //    多い（例: 表記"Squirtlings"の読みが「お」1文字 等）。
    if surface.chars().any(|c| c.is_ascii_alphabetic()) && reading_len <= 2 {
        return None;
    }
    // 4) 顔文字・SNSアカウント名的な装飾記号を含む表記は、実用的なかな
    //    漢字変換の対象ではない（例: "*メル*"/"＊メル＊" が読み「めるかり」
    //    の先頭候補になってしまう、"(^ω^)"系の顔文字が紛れ込む 等）。
    //    基本辞書（IPA辞書本体）にはこれらの文字を含む表記が一切無いこと
    //    を確認済みなので、正規の語を誤って弾く心配はない。
    // '-'/'－'（ハイフンマイナス）は "GENSHOU-現象-" のようなSNSアカウント名
    // 由来の表記に使われる。カタカナ語の長音符 'ー'（U+30FC）とは別の文字
    // なので、コーヒー等の正規語を弾く心配は無い。
    const DECORATIVE_SYMBOLS: &[char] = &[
        '(', ')', '（', '）', '~', '～', '^', '＾', '*', '＊', '#', '＃', '@', '＠', '_', '＿',
        '`', '´', '°', '-', '－',
    ];
    if surface.chars().any(|c| DECORATIVE_SYMBOLS.contains(&c)) {
        return None;
    }
    // 3) 固有名詞なのに読みが1文字は日本語としてまず現実的にありえない
    //    （実在の1文字読みの語は基本辞書に一般名詞・助詞等として収録済み）。
    if pos.starts_with("名詞-固有名詞") && reading_len <= 1 {
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

/// ディレクトリ内の全CSVファイルをパースして単語一覧を返す（辞書には未追加）
fn load_csv_dir_entries(dict_dir: &Path) -> Result<Vec<WordEntry>> {
    let csv_files: Vec<_> = fs::read_dir(dict_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map_or(false, |ext| ext == "csv")
        })
        .collect();

    if csv_files.is_empty() {
        anyhow::bail!("CSVファイルが見つかりません: {}", dict_dir.display());
    }

    let pb = ProgressBar::new(csv_files.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );

    let mut all_entries = Vec::new();
    for entry in csv_files {
        let path = entry.path();
        pb.set_message(format!("{}", path.file_name().unwrap_or_default().to_string_lossy()));

        // まずUTF-8として試す、失敗したらEUC-JPとして読む
        let entries = match load_utf8_csv_file(&path) {
            Ok(e) if !e.is_empty() => e,
            _ => load_csv_file(&path).unwrap_or_default(),
        };
        all_entries.extend(entries);

        pb.inc(1);
    }
    pb.finish_with_message(format!("完了: {} 単語", all_entries.len()));

    Ok(all_entries)
}

/// 補助辞書（NEologd等）の固有名詞に、出典不明の異常な低コストを補正するための
/// 底上げ・上乗せ。基本辞書に無い（＝補助辞書だけが持ち込む）語にのみ適用する。
///
/// NEologdのコスト推定は自動生成でIPA辞書ほど校正されておらず、固有名詞の
/// 一部（特に希少な地名・人名・組織名）が負値〜極端に低いコストを持つ
/// ことがある（実測で全体の1割超）。これにより「がっこ」→組織名"gacco"、
/// 「わた」→人名「和太」のように、ありふれた読みの一般語・動詞や
/// カタカナフォールバックを押しのけてしまう。
/// 一方、基本辞書（IPA辞書）に元々ある固有名詞（大阪・東京 等）は
/// 適切に校正済みで、この補正の対象外にする必要がある
/// （出典を区別せず一律ペナルティを掛けると、こうした正当で頻出な
/// 固有名詞まで壊れることを実測で確認済み）。
const SUPPLEMENT_PROPER_NOUN_MIN_COST: i32 = 3000;
const SUPPLEMENT_PROPER_NOUN_SURCHARGE: i32 = 2500;

fn adjust_supplement_entry(mut entry: WordEntry) -> WordEntry {
    if entry.pos.starts_with("名詞-固有名詞") {
        let adjusted = (entry.cost as i32)
            .max(SUPPLEMENT_PROPER_NOUN_MIN_COST)
            .saturating_add(SUPPLEMENT_PROPER_NOUN_SURCHARGE)
            .min(i16::MAX as i32);
        entry.cost = adjusted as i16;
    }
    entry
}

/// IPA辞書ディレクトリ（+ 任意で補助辞書ディレクトリ）から辞書を構築
///
/// 補助辞書（NEologd等）は、基本辞書に既に存在する語（読み+表記が一致）は
/// 重複追加せずスキップし（基本辞書側の校正済みコストを優先）、基本辞書に
/// 無い語だけを `adjust_supplement_entry` で補正のうえ追加する。
fn build_dictionary(dict_dir: &Path, supplement_dir: Option<&Path>) -> Result<Dictionary> {
    let mut dict = Dictionary::new();

    // matrix.defを読み込む
    let matrix_path = dict_dir.join("matrix.def");
    if matrix_path.exists() {
        dict.matrix = load_matrix(&matrix_path)?;
        println!("連接行列を読み込みました");
    } else {
        println!("警告: matrix.defが見つかりません。デフォルトの連接行列を使用します。");
    }

    println!("基本辞書を読み込んでいます: {}", dict_dir.display());
    let base_entries = load_csv_dir_entries(dict_dir)?;
    let mut base_keys: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::with_capacity(base_entries.len());
    for entry in base_entries {
        base_keys.insert((entry.reading.clone(), entry.surface.clone()));
        dict.add_word(entry);
    }
    println!("基本辞書: {} 単語", base_keys.len());

    if let Some(supp_dir) = supplement_dir {
        println!("補助辞書を読み込んでいます: {}", supp_dir.display());
        let supp_entries = load_csv_dir_entries(supp_dir)?;
        let mut added = 0u64;
        let mut skipped_dup = 0u64;
        let mut surcharged = 0u64;
        for entry in supp_entries {
            if base_keys.contains(&(entry.reading.clone(), entry.surface.clone())) {
                skipped_dup += 1;
                continue;
            }
            let is_proper_noun = entry.pos.starts_with("名詞-固有名詞");
            let entry = adjust_supplement_entry(entry);
            if is_proper_noun {
                surcharged += 1;
            }
            dict.add_word(entry);
            added += 1;
        }
        println!(
            "補助辞書: {} 単語追加（うち固有名詞に補正適用 {} 語）、基本辞書と重複のため {} 語スキップ",
            added, surcharged, skipped_dup
        );
    }

    Ok(dict)
}

/// 同一 (reading, left_id, right_id) グループでカタカナ表記と漢字を含む
/// 表記が共存する場合の、カタカナ側への上乗せコストのマージン。
///
/// COMMON_WORD_SEED_BONUS(1500の学習1回)で逆転しない程度に小さく、辞書
/// コストの通常のばらつき（数十〜数百）より大きい値として選定。
const KATAKANA_KANJI_COST_MARGIN: i32 = 500;

fn is_all_katakana_surface(surface: &str) -> bool {
    !surface.is_empty()
        && surface.chars().all(|c| {
            ('\u{30A1}'..='\u{30FA}').contains(&c) || ('\u{30FC}'..='\u{30FF}').contains(&c)
        })
}

fn contains_kanji(surface: &str) -> bool {
    surface
        .chars()
        .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c))
}

/// カタカナ表記が同一読みの漢字表記より辞書コストで勝ってしまう構造的な
/// 誤変換（カテゴリC）を辞書構築時に機械的に補正する。
///
/// IPA辞書は口語的なカタカナ強調表記（クルマ・アタマ 等）を、同じ読み・
/// 同じ品詞ID（＝同じ接続コストを共有する）の漢字表記（車・頭 等）より
/// 低コストに調整していることがある。同一 (reading, left_id, right_id)
/// でグルーピングし、グループ内に漢字を含む表記が存在する場合のみ、
/// カタカナ側のコストを「漢字側の最安値+マージン」まで引き上げる。
/// 同一読みに漢字表記が存在しない外来語カタカナ（コーヒー・テレビ 等）は
/// グループ内に漢字表記が無いため対象外のまま変化しない。
fn correct_katakana_kanji_cost_collisions(dict: &mut Dictionary) -> usize {
    let mut words = Vec::new();
    collect_words_from_trie(&dict.trie, &mut words);

    let mut groups: std::collections::HashMap<(String, u16, u16), Vec<WordEntry>> =
        std::collections::HashMap::new();
    for w in words {
        groups.entry((w.reading.clone(), w.left_id, w.right_id)).or_default().push(w);
    }

    let mut corrected = 0usize;
    for ((reading, _left_id, _right_id), entries) in groups {
        let kanji_min_cost =
            entries.iter().filter(|e| contains_kanji(&e.surface)).map(|e| e.cost as i32).min();
        let Some(kanji_min_cost) = kanji_min_cost else {
            continue; // グループ内に漢字表記が無い→対象外（外来語カタカナ等）
        };
        let floor = (kanji_min_cost + KATAKANA_KANJI_COST_MARGIN).min(i16::MAX as i32);
        for e in &entries {
            if !is_all_katakana_surface(&e.surface) || (e.cost as i32) >= floor {
                continue;
            }
            dict.remove_word(&reading, &e.surface);
            let mut fixed = e.clone();
            fixed.cost = floor as i16;
            dict.add_word(fixed);
            corrected += 1;
        }
    }
    corrected
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
        // 同じ読み+表記が既にあれば入れ替える（再実行してもコスト更新のみで
        // 重複エントリが積み重ならないようにする）
        dict.remove_word(&reading, &surface);
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

/// 青空文庫のXHTML（Shift_JIS）から地の文だけを取り出す。
///
/// 取り除くもの: `<rp>`/`<rt>`（ふりがな本体。`<rb>`側の本文だけ残す）、
/// その他の全HTMLタグ、`［＃…］`/`[#…]`形式の校正者注記、本文範囲外
/// （`main_text`より前後のヘッダ・底本情報等）。`<br />`は改行に変換して
/// 段落構造を残す（文分割・バイグラムの文境界判定に使うため）。
fn clean_aozora_html(shift_jis_bytes: &[u8]) -> String {
    let (decoded, _, _) = encoding_rs::SHIFT_JIS.decode(shift_jis_bytes);

    // 本文（main_text）の範囲だけを対象にする。無ければ全体を対象にする
    // （card*.html等、本文divが無い索引ページは呼び出し側で除外される想定）。
    let text = decoded.as_ref();
    let body = match text.find("class=\"main_text\"") {
        Some(start) => {
            let after = &text[start..];
            let end = after
                .find("class=\"bibliographical_information\"")
                .or_else(|| after.find("</body>"))
                .unwrap_or(after.len());
            &after[..end]
        }
        None => text,
    };

    // バイト位置ベースで前から走査する（Vec<char>への全文字収集や、位置ごとの
    // 残り文字列の再collectはしない。文書長に対してO(n)にするため。以前の
    // 実装は各文字位置で残り全体を毎回collectしておりO(n^2)だった）。
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while !rest.is_empty() {
        let Some(tag_start) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..tag_start]);
        rest = &rest[tag_start..];

        if rest.starts_with("<rp") {
            // <rp>...</rp>（ふりがな囲みの記号）の中身ごと捨てる
            match rest.find("</rp>") {
                Some(end) => rest = &rest[end + "</rp>".len()..],
                None => break,
            }
            continue;
        }
        if rest.starts_with("<rt") {
            // <rt>...</rt>（ふりがな本体）の中身ごと捨てる
            match rest.find("</rt>") {
                Some(end) => rest = &rest[end + "</rt>".len()..],
                None => break,
            }
            continue;
        }
        if rest[1..].to_ascii_lowercase().starts_with("br") {
            out.push('\n');
        }
        // それ以外のタグはタグ自体（<...>）だけを読み飛ばす
        match rest.find('>') {
            Some(tag_end) => rest = &rest[tag_end + 1..],
            None => break,
        }
    }

    // 校正者注記 ［＃…］ / [#…] を中身ごと除去
    let mut cleaned = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '［' || c == '[' {
            let close = if c == '［' { '］' } else { ']' };
            for c2 in chars.by_ref() {
                if c2 == close {
                    break;
                }
            }
            continue;
        }
        cleaned.push(c);
    }
    cleaned
}

/// vibratoのfeature文字列（IPADic互換CSV）から読み（カタカナ、フィールド8番目）を取り出す
fn feature_reading(feature: &str) -> Option<&str> {
    feature.split(',').nth(7)
}

fn feature_pos(feature: &str) -> &str {
    feature.split(',').next().unwrap_or("")
}

/// コーパステキストを分かち書きし、(読み,表記)ユニグラム頻度と
/// (前表記,表記)バイグラム頻度を集計する。
///
/// バイグラムは文（。！？で区切る）をまたがない。記号・空白トークンは
/// 集計対象にせず、直後のバイグラムの「前」もリセットする（不自然な
/// 語同士の結びつきを学習しないため）。
fn tokenize_and_aggregate(
    tokenizer: &vibrato::Tokenizer,
    text: &str,
    unigrams: &mut std::collections::HashMap<(String, String), u64>,
    bigrams: &mut std::collections::HashMap<(String, String), u64>,
) {
    let mut worker = tokenizer.new_worker();
    for line in text.split(['\n', '。', '！', '？']) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        worker.reset_sentence(line);
        worker.tokenize();
        let mut prev_surface: Option<String> = None;
        for i in 0..worker.num_tokens() {
            let t = worker.token(i);
            let feature = t.feature();
            let pos = feature_pos(feature);
            if pos == "記号" || pos.is_empty() {
                prev_surface = None;
                continue;
            }
            let Some(reading_kana) = feature_reading(feature) else {
                prev_surface = None;
                continue;
            };
            let reading = katakana_to_hiragana(reading_kana);
            if reading.is_empty()
                || !reading
                    .chars()
                    .all(|c| ('\u{3041}'..='\u{3096}').contains(&c) || c == 'ー')
            {
                prev_surface = None;
                continue;
            }
            let surface = t.surface().to_string();
            *unigrams.entry((reading, surface.clone())).or_insert(0) += 1;
            if let Some(prev) = prev_surface.take() {
                *bigrams.entry((prev, surface.clone())).or_insert(0) += 1;
            }
            prev_surface = Some(surface);
        }
    }
}

/// コーパスLM(`corpus_lm.dic`)をバイナリ形式で保存（system.dicと同じ
/// bincode+gzip方式。`crates/common/src/corpus_lm.rs`のCorpusLmと同一形式）
fn save_corpus_lm(
    unigrams: &std::collections::HashMap<(String, String), u64>,
    bigrams: &std::collections::HashMap<(String, String), u64>,
    min_freq: u64,
    output_path: &Path,
) -> Result<()> {
    let lm = common::CorpusLm {
        unigrams: unigrams
            .iter()
            .filter(|(_, &freq)| freq >= min_freq)
            .map(|((reading, surface), &freq)| (reading.clone(), surface.clone(), freq as u32))
            .collect(),
        bigrams: bigrams
            .iter()
            .filter(|(_, &freq)| freq >= min_freq)
            .map(|((prev, surface), &freq)| (prev.clone(), surface.clone(), freq as u32))
            .collect(),
    };
    println!(
        "コーパスLM: ユニグラム {} 語 / バイグラム {} 組（頻度{}未満は除外、除外前は{}/{}）",
        lm.unigrams.len(),
        lm.bigrams.len(),
        min_freq,
        unigrams.len(),
        bigrams.len()
    );
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    lm.save(output_path)?;
    println!("コーパスLMを保存しました: {}", output_path.display());
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        println!("使い方:");
        println!("  {} build <IPA辞書ディレクトリ> [出力ファイル] [補助辞書ディレクトリ]", args[0]);
        println!("  {} extend <辞書ファイル> <補助CSV>  (常用語を追加)", args[0]);
        println!("  {} test <辞書ファイル>", args[0]);
        println!("  {} corpus <vibrato辞書.dic.zst> <コーパステキストディレクトリ> <出力corpus_lm.dic> [最小頻度]", args[0]);
        println!();
        println!("補助辞書ディレクトリ（NEologd等）は、基本辞書に無い語だけを取り込み、");
        println!("固有名詞には出典不明の異常な低コストを補正するペナルティを掛ける。");
        println!();
        println!("corpus は実コーパス（青空文庫のXHTML等）を分かち書きし、語頻度から");
        println!("ViterbiConverter用のコーパス統計データ(corpus_lm.dic)を構築する。");
        println!("分かち書きにはvibrato（IPADic互換モデル、別途ダウンロードが必要）を使う。");
        println!();
        println!("例:");
        println!("  {} build ./ipadic ./dictionaries/system.dic", args[0]);
        println!("  {} build ./ipadic ./dictionaries/system.dic ./neologd", args[0]);
        println!("  {} extend ./dictionaries/system.dic ./dictionaries/extra.csv", args[0]);
        println!("  {} test ./dictionaries/system.dic", args[0]);
        println!("  {} corpus ./ipadic-mecab-2_7_0/system.dic.zst ./corpus_raw ./dictionaries/corpus_lm.dic", args[0]);
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
            let supplement_dir = if args.len() >= 5 {
                Some(Path::new(&args[4]))
            } else {
                None
            };

            // 出力ディレクトリを作成
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }

            println!("IPA辞書を読み込んでいます: {}", dict_dir.display());
            let mut dict = build_dictionary(dict_dir, supplement_dir)?;
            let corrected = correct_katakana_kanji_cost_collisions(&mut dict);
            println!("カタカナ/漢字コスト衝突を補正しました: {} 語", corrected);
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
        "corpus" => {
            if args.len() < 5 {
                anyhow::bail!("使い方: corpus <vibrato辞書.dic.zst> <コーパステキストディレクトリ> <出力corpus_lm.dic> [最小頻度]");
            }
            let vibrato_dict_path = Path::new(&args[2]);
            let corpus_dir = Path::new(&args[3]);
            let output_path = Path::new(&args[4]);
            let min_freq: u64 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(3);

            println!("vibrato辞書を読み込んでいます: {}", vibrato_dict_path.display());
            let reader = zstd::Decoder::new(File::open(vibrato_dict_path)?)?;
            let vibrato_dict = vibrato::Dictionary::read(reader)?;
            let tokenizer = vibrato::Tokenizer::new(vibrato_dict);

            let files: Vec<_> = fs::read_dir(corpus_dir)
                .with_context(|| format!("コーパスディレクトリを開けません: {}", corpus_dir.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map_or(false, |ext| ext == "html" || ext == "txt"))
                .collect();
            if files.is_empty() {
                anyhow::bail!("コーパステキストが見つかりません: {}", corpus_dir.display());
            }

            let pb = ProgressBar::new(files.len() as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                    .unwrap()
                    .progress_chars("#>-"),
            );

            let mut unigrams: std::collections::HashMap<(String, String), u64> = std::collections::HashMap::new();
            let mut bigrams: std::collections::HashMap<(String, String), u64> = std::collections::HashMap::new();
            for path in &files {
                pb.set_message(format!("{}", path.file_name().unwrap_or_default().to_string_lossy()));
                let bytes = fs::read(path)?;
                let text = clean_aozora_html(&bytes);
                tokenize_and_aggregate(&tokenizer, &text, &mut unigrams, &mut bigrams);
                pb.inc(1);
            }
            pb.finish_with_message(format!(
                "完了: {} ファイル、ユニグラム{}語/バイグラム{}組（頻度フィルタ前）",
                files.len(), unigrams.len(), bigrams.len()
            ));

            save_corpus_lm(&unigrams, &bigrams, min_freq, output_path)?;
        }
        _ => {
            anyhow::bail!("不明なコマンド: {}", args[1]);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(surface: &str, reading: &str, left_id: u16, right_id: u16, cost: i16, pos: &str) -> WordEntry {
        WordEntry {
            surface: surface.to_string(),
            reading: reading.to_string(),
            left_id,
            right_id,
            cost,
            pos: pos.to_string(),
        }
    }

    #[test]
    fn test_katakana_kanji_collision_is_corrected() {
        // 「くるま」: カタカナ「クルマ」(3630) が漢字「車」(6918) より
        // 同一品詞IDで低コスト → カタカナ側を 6918+500=7418 まで引き上げる
        let mut dict = Dictionary::new();
        dict.add_word(word("クルマ", "くるま", 1285, 1285, 3630, "名詞-一般-*-*"));
        dict.add_word(word("車", "くるま", 1285, 1285, 6918, "名詞-一般-*-*"));

        let corrected = correct_katakana_kanji_cost_collisions(&mut dict);
        assert_eq!(corrected, 1);

        let entries = dict.lookup("くるま").unwrap();
        let katakana = entries.iter().find(|e| e.surface == "クルマ").unwrap();
        let kanji = entries.iter().find(|e| e.surface == "車").unwrap();
        assert_eq!(katakana.cost, 7418);
        assert_eq!(kanji.cost, 6918); // 漢字側は変化しない
    }

    #[test]
    fn test_katakana_without_kanji_counterpart_is_not_touched() {
        // 「こーひー」: 漢字表記が同一読みグループに無い外来語カタカナは対象外
        let mut dict = Dictionary::new();
        dict.add_word(word("コーヒー", "こーひー", 1285, 1285, 4000, "名詞-一般-*-*"));

        let corrected = correct_katakana_kanji_cost_collisions(&mut dict);
        assert_eq!(corrected, 0);

        let entries = dict.lookup("こーひー").unwrap();
        assert_eq!(entries.iter().find(|e| e.surface == "コーヒー").unwrap().cost, 4000);
    }

    #[test]
    fn test_katakana_already_above_floor_is_not_touched() {
        // カタカナ側が既に漢字側+マージン以上のコストなら補正不要（冪等性）
        let mut dict = Dictionary::new();
        dict.add_word(word("クルマ", "くるま", 1285, 1285, 8000, "名詞-一般-*-*"));
        dict.add_word(word("車", "くるま", 1285, 1285, 6918, "名詞-一般-*-*"));

        let corrected = correct_katakana_kanji_cost_collisions(&mut dict);
        assert_eq!(corrected, 0);

        let entries = dict.lookup("くるま").unwrap();
        assert_eq!(entries.iter().find(|e| e.surface == "クルマ").unwrap().cost, 8000);
    }

    #[test]
    fn test_katakana_kanji_collision_different_pos_id_not_grouped() {
        // (reading,left_id,right_id) が完全一致しない場合はグルーピング対象外
        // （品詞IDが異なる=活用形違い等は本補正の対象外という設計の確認）
        let mut dict = Dictionary::new();
        dict.add_word(word("クルマ", "くるま", 1285, 1285, 3630, "名詞-一般-*-*"));
        dict.add_word(word("車", "くるま", 999, 999, 6918, "名詞-一般-*-*"));

        let corrected = correct_katakana_kanji_cost_collisions(&mut dict);
        assert_eq!(corrected, 0);

        let entries = dict.lookup("くるま").unwrap();
        assert_eq!(entries.iter().find(|e| e.surface == "クルマ").unwrap().cost, 3630);
    }
}
