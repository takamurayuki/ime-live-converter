//! 実辞書(dictionaries/system.dic)を使った誤変換の回帰コーパステスト。
//!
//! `system.dic` は README の手順で各自ビルドする非コミット資産（.gitignore
//! 対象）のため、存在しない環境（CI 含む）では即座にスキップする。
//! タスク#583で見つかった誤変換パターン（research.md参照）のうち、
//! `status=fixed` の項目は厳格に assert し、`status=known_issue` の項目は
//! 全体の通過数が既知のベースラインを下回っていないかのみを検証する
//! （未対応であること自体をテスト失敗にしないが、将来の劣化は検知する）。

use common::{Dictionary, ViterbiConverter};
use std::path::Path;

/// known_issue 項目の非退行判定に使う下限（初回実測値）。
///
/// 実測方法: `cargo test --test conversion_corpus -- --nocapture` を実行し、
/// 出力される「known_issue 通過数」をそのまま転記する
/// （2026-08-14, system.dic 実測、known_issue 全28件中の通過数）。
const KNOWN_ISSUE_BASELINE_PASS: usize = 1;

struct CorpusEntry {
    category: String,
    reading: String,
    expected_substring: String,
    status: String,
}

fn parse_corpus(tsv: &str) -> Vec<CorpusEntry> {
    let mut lines = tsv.lines();
    lines.next(); // ヘッダー行をスキップ
    lines
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 4, "TSVの列数が不正: {:?}", line);
            CorpusEntry {
                category: cols[0].to_string(),
                reading: cols[1].to_string(),
                expected_substring: cols[2].to_string(),
                status: cols[3].to_string(),
            }
        })
        .collect()
}

#[test]
fn misconversion_corpus_regression() {
    let dict_path = Path::new("dictionaries/system.dic");
    if !dict_path.exists() {
        eprintln!(
            "system.dic が見つからないためスキップします（README の手順でビルドしてください）: {}",
            dict_path.display()
        );
        return;
    }

    let dict = Dictionary::load(dict_path).expect("system.dic のロードに失敗");
    let converter = ViterbiConverter::new(dict);

    let tsv = std::fs::read_to_string("tests/fixtures/misconversion_corpus.tsv")
        .expect("misconversion_corpus.tsv の読み込みに失敗");
    let entries = parse_corpus(&tsv);
    assert!(!entries.is_empty(), "コーパスが空");

    let mut fixed_failures: Vec<String> = Vec::new();
    let mut known_issue_pass = 0usize;
    let mut known_issue_total = 0usize;
    let mut category_counts: std::collections::BTreeMap<String, (usize, usize)> =
        std::collections::BTreeMap::new();

    for entry in &entries {
        let result = converter.convert_to_string(&entry.reading);
        let pass = result.contains(&entry.expected_substring);

        let counter = category_counts.entry(entry.category.clone()).or_insert((0, 0));
        counter.1 += 1;
        if pass {
            counter.0 += 1;
        }

        match entry.status.as_str() {
            "fixed" => {
                if !pass {
                    fixed_failures.push(format!(
                        "[{}] reading={:?} expected_substring={:?} actual={:?}",
                        entry.category, entry.reading, entry.expected_substring, result
                    ));
                }
            }
            "known_issue" => {
                known_issue_total += 1;
                if pass {
                    known_issue_pass += 1;
                }
            }
            other => panic!("未知のstatus: {}", other),
        }
    }

    eprintln!("=== カテゴリ別 通過率 ===");
    for (category, (pass, total)) in &category_counts {
        eprintln!("  {}: {}/{}", category, pass, total);
    }
    eprintln!(
        "known_issue 通過数: {}/{} (ベースライン: {})",
        known_issue_pass, known_issue_total, KNOWN_ISSUE_BASELINE_PASS
    );

    assert!(
        fixed_failures.is_empty(),
        "status=fixed の項目が失敗しました:\n{}",
        fixed_failures.join("\n")
    );

    assert!(
        known_issue_pass >= KNOWN_ISSUE_BASELINE_PASS,
        "known_issue の通過数がベースライン({})を下回りました: {}",
        KNOWN_ISSUE_BASELINE_PASS,
        known_issue_pass
    );
}
