//! 隣接コロケーションシード（word_assoc.tsv、[[seeded-adjacent-collocation]]）
//! の候補バッチを、採用前に実際の変換で機械的に検証するツール。
//!
//! 24グループを手作業で生成したところ13グループが失敗した
//! （想定外の同音異義語衝突・表記ゆれ・ボーナス不足）経験から、
//! 「衝突検出器を通す→人手で意味的にレビューする→採用」という工程の
//! 前に「実際に変換して期待通りの表記が出るか」を機械的に確認する工程を
//! 挟むことにした。このツールがその工程。
//!
//! 候補ファイルの形式（TSV、`#`始まりの行・空行は無視）:
//!   読み<TAB>出力表記(target)<TAB>トリガー語(trigger)<TAB>確認用の読み（ひらがな）<TAB>期待される確定後の表記
//!
//! 「確認用の読み」は**ひらがなで書く**（ローマ字を手で書かない）。ツール側で
//! `kana_to_safe_romaji`が安全なローマ字へ機械的に変換してから`add_char`に
//! 1文字ずつ流し込み`commit`する。手書きローマ字では「ん」の逐次確定の罠
//! （`add_char`は"nn"の2文字目で即座に「ん」を確定するため、後続がな行なら
//! 3文字目の"n"が要る、[[correction-cascade-unification]]・
//! [[golden-test-harness]]で既知）を何度も踏んだため、ひらがな入力から
//! 常に安全なローマ字（「ん」は常に"nn"で統一）を自動生成する方式にした。
//!
//! 結果、「期待される確定後の表記」に含まれるかを見る（golden_tests.rsと
//! 同じ意味論）。合格した行だけ、`word_assoc.tsv`にそのまま追記できる形
//! （読み・target・trigger の3列、ボーナス自動算出）と、`golden/general.tsv`
//! に追記できる形（ローマ字・期待表記の2列）を標準出力に整形して出す。
//!
//! 使い方:
//!   cargo run -p common --example verify_word_assoc_candidates -- \
//!     dictionaries/word_assoc_candidates_draft.tsv
//!
//! 既存の`dictionaries/word_assoc.tsv`も一緒に読み込む（候補が既存の
//! グループと衝突していないかも同時に確認するため）。

use common::{Dictionary, LiveConversionState, ViterbiConverter};
use std::path::Path;

struct Candidate {
    reading: String,
    target: String,
    trigger: String,
    test_reading: String,
    test_romaji: String,
    expected: String,
}

fn parse_candidates(content: &str) -> Vec<Candidate> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let cols: Vec<&str> = l.split('\t').map(str::trim).collect();
            if cols.len() < 5 {
                eprintln!("列が足りない行をスキップ: {l:?}");
                return None;
            }
            Some(Candidate {
                reading: cols[0].to_string(),
                target: cols[1].to_string(),
                trigger: cols[2].to_string(),
                test_reading: cols[3].to_string(),
                test_romaji: kana_to_safe_romaji(cols[3]),
                expected: cols[4].to_string(),
            })
        })
        .collect()
}

/// ひらがな読みを、`add_char`の逐次確定にとって常に安全なローマ字へ変換する。
/// 「ん」は常に"nn"にする（後続が母音・な行でも即座に確定し、後続の文字が
/// 別の音として誤って結合しない）。促音「っ」は次の子音を重ねる。拗音
/// （きゃ等）は2文字まとめて変換する。未知の文字（かな以外）はそのまま
/// 通す（漢字混じりの読みを誤って渡した場合にすぐ気づけるよう、変換せず
/// 素通しする）。
fn kana_to_safe_romaji(kana: &str) -> String {
    const YOON: &[(&str, &str)] = &[
        ("きゃ", "kya"), ("きゅ", "kyu"), ("きょ", "kyo"),
        ("ぎゃ", "gya"), ("ぎゅ", "gyu"), ("ぎょ", "gyo"),
        ("しゃ", "sha"), ("しゅ", "shu"), ("しょ", "sho"),
        ("じゃ", "ja"), ("じゅ", "ju"), ("じょ", "jo"),
        ("ちゃ", "cha"), ("ちゅ", "chu"), ("ちょ", "cho"),
        ("にゃ", "nya"), ("にゅ", "nyu"), ("にょ", "nyo"),
        ("ひゃ", "hya"), ("ひゅ", "hyu"), ("ひょ", "hyo"),
        ("びゃ", "bya"), ("びゅ", "byu"), ("びょ", "byo"),
        ("ぴゃ", "pya"), ("ぴゅ", "pyu"), ("ぴょ", "pyo"),
        ("みゃ", "mya"), ("みゅ", "myu"), ("みょ", "myo"),
        ("りゃ", "rya"), ("りゅ", "ryu"), ("りょ", "ryo"),
    ];
    const MORA: &[(&str, &str)] = &[
        ("あ","a"),("い","i"),("う","u"),("え","e"),("お","o"),
        ("か","ka"),("き","ki"),("く","ku"),("け","ke"),("こ","ko"),
        ("が","ga"),("ぎ","gi"),("ぐ","gu"),("げ","ge"),("ご","go"),
        ("さ","sa"),("し","shi"),("す","su"),("せ","se"),("そ","so"),
        ("ざ","za"),("じ","ji"),("ず","zu"),("ぜ","ze"),("ぞ","zo"),
        ("た","ta"),("ち","chi"),("つ","tsu"),("て","te"),("と","to"),
        ("だ","da"),("ぢ","ji"),("づ","zu"),("で","de"),("ど","do"),
        ("な","na"),("に","ni"),("ぬ","nu"),("ね","ne"),("の","no"),
        ("は","ha"),("ひ","hi"),("ふ","fu"),("へ","he"),("ほ","ho"),
        ("ば","ba"),("び","bi"),("ぶ","bu"),("べ","be"),("ぼ","bo"),
        ("ぱ","pa"),("ぴ","pi"),("ぷ","pu"),("ぺ","pe"),("ぽ","po"),
        ("ま","ma"),("み","mi"),("む","mu"),("め","me"),("も","mo"),
        ("や","ya"),("ゆ","yu"),("よ","yo"),
        ("ら","ra"),("り","ri"),("る","ru"),("れ","re"),("ろ","ro"),
        ("わ","wa"),("を","wo"),
    ];
    let chars: Vec<char> = kana.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        // 拗音（2文字）を先に試す
        if i + 1 < chars.len() {
            let two: String = chars[i..i + 2].iter().collect();
            if let Some((_, romaji)) = YOON.iter().find(|(k, _)| *k == two) {
                out.push_str(romaji);
                i += 2;
                continue;
            }
        }
        let c = chars[i];
        if c == 'ん' {
            out.push_str("nn"); // 常に"nn"（[[golden-test-harness]]の罠を避ける）
            i += 1;
        } else if c == 'っ' {
            // 次のモーラの子音を重ねる（例: がっこう→gakkou）
            if i + 1 < chars.len() {
                let next: String = chars[i + 1..].iter().collect();
                let next_romaji = MORA
                    .iter()
                    .find(|(k, _)| next.starts_with(k))
                    .map(|(_, r)| *r)
                    .or_else(|| YOON.iter().find(|(k, _)| next.starts_with(k)).map(|(_, r)| *r));
                if let Some(r) = next_romaji {
                    if let Some(first) = r.chars().next() {
                        out.push(first);
                    }
                }
            }
            i += 1;
        } else if c == 'ー' {
            out.push('-');
            i += 1;
        } else if let Some((_, romaji)) = MORA.iter().find(|(k, _)| k.chars().next() == Some(c)) {
            out.push_str(romaji);
            i += 1;
        } else {
            // かな以外（漢字混じり等）はそのまま通す。誤って漢字を含む
            // 読みを渡した場合、変換結果が明らかにおかしくなって気づける。
            out.push(c);
            i += 1;
        }
    }
    out
}

fn run_case(state: &mut LiveConversionState, input: &str) -> String {
    let mut sim = String::new();
    for ch in input.chars() {
        if let Some(action) = state.add_char(ch) {
            apply(&mut sim, action);
        }
    }
    if let Some(action) = state.commit() {
        apply(&mut sim, action);
    }
    sim
}

fn apply(sim: &mut String, action: common::ConversionAction) {
    if action.delete_count > 0 {
        let keep = sim.chars().count().saturating_sub(action.delete_count);
        *sim = sim.chars().take(keep).collect();
    }
    sim.push_str(&action.insert_text);
}

fn main() {
    let candidates_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "dictionaries/word_assoc_candidates_draft.tsv".to_string());
    let candidates_content = std::fs::read_to_string(&candidates_path).unwrap_or_else(|e| {
        panic!("候補ファイルを読めませんでした（{candidates_path}）: {e}");
    });
    let candidates = parse_candidates(&candidates_content);
    if candidates.is_empty() {
        eprintln!("候補が1件もありません: {candidates_path}");
        return;
    }

    let dict_path = Path::new("dictionaries/system.dic");
    let dict = Dictionary::load(dict_path).unwrap_or_else(|e| {
        panic!("実辞書の読み込みに失敗しました: {e}");
    });
    // 基底辞書はArcで共有し、候補ごとにTrie全体をクローンしない
    // （[[dictionary-arc-sharing]]、実測で1回あたり300〜400ms→180μs前後）。
    let dict = std::sync::Arc::new(dict);

    let mut pass = Vec::new();
    let mut fail = Vec::new();

    for c in &candidates {
        let mut converter = ViterbiConverter::from_shared(std::sync::Arc::clone(&dict));
        let _ = converter.load_word_priority_file(Path::new("dictionaries/word_priority.tsv"));
        let _ = converter.load_word_assoc_file(Path::new("dictionaries/word_assoc.tsv"));
        // この候補1件だけを追加で読み込む（他の候補からは独立に検証する）。
        let candidate_line = format!("{}\t{}\t{}\n", c.reading, c.target, c.trigger);
        converter.load_word_assoc_str(&candidate_line);
        let mut state = LiveConversionState::new();
        state.converter = Some(converter);

        let result = run_case(&mut state, &c.test_romaji);
        if result.contains(&c.expected) {
            pass.push((c, result));
        } else {
            fail.push((c, result));
        }
    }

    println!("=== 結果: {}/{} 件が期待通り ===\n", pass.len(), candidates.len());

    if !fail.is_empty() {
        println!("--- 失敗（採用しない）---");
        for (c, actual) in &fail {
            println!(
                "  {}\t{}\t{}\t読み={}(romaji={})\t期待={} 実際={actual:?}",
                c.reading, c.target, c.trigger, c.test_reading, c.test_romaji, c.expected
            );
        }
        println!();
    }

    if !pass.is_empty() {
        println!("--- 合格。dictionaries/word_assoc.tsv に追記できる形 ---");
        for (c, _) in &pass {
            println!("{}\t{}\t{}", c.reading, c.target, c.trigger);
        }
        println!("\n--- 合格。golden/general.tsv に追記できる形（順方向のみ。逆方向は別途手動で確認すること）---");
        for (c, actual) in &pass {
            println!("{}\t{}", c.test_romaji, actual);
        }
    }
}
