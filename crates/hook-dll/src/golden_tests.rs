//! ゴールデンテスト: 実運用エンジン（`LiveConversionState`）を、実際の
//! キー入力と同じ意味論（1文字ずつ`add_char`→最後に`commit`）で駆動し、
//! 既知の入力に対する変換結果と突き合わせる。
//!
//! 223件のユニットテストは状態機械の構造的な正しさを守るが、変換精度
//! （実際の文に対して正しい漢字が出るか）は一切測っていなかった。この
//! モジュールはその穴を埋める。P0/P1で予定している補正ロジックの変更が、
//! 既存の正しい変換をどれだけ壊したか可視化できるようにする。
//!
//! `LiveConversionState`はWin32非依存（本ファイルもWin32 APIを一切使わない）
//! だが`pub(crate)`のため、`examples/`や`tests/`（外部crateとして依存する
//! ためpub(crate)項目には届かない。加えて本クレートは`crate-type =
//! ["cdylib"]`のみでrlibを生成しないため、外部からの`use`自体が原理的に
//! 不可能）からは触れない。既存の`state_machine_tests`
//! （`conversion.rs`内`#[cfg(test)] mod`）と同じく、クレート内部の
//! `#[cfg(test)]`モジュールとして置く。
#![cfg(test)]

use crate::*;
use common::{Dictionary, ViterbiConverter};
use std::path::PathBuf;

const REGRESSIONS_TSV: &str = include_str!("golden/regressions.tsv");
const GENERAL_TSV: &str = include_str!("golden/general.tsv");
const CORRECTIONS_TSV: &str = include_str!("golden/corrections.tsv");
const COLLOCATIONS_TSV: &str = include_str!("golden/collocations.tsv");

/// 一般精度コーパスの現状の一致率。これを下回ったら退行とみなして失敗させる。
/// 初回実装時点では全件がスナップショット（実際の出力をそのまま期待値として
/// 記録）のため100%。今後、まだ直っていない曖昧なケースを「現状はこう」と
/// 記録せずに追加する場合は、この値を実測に合わせて下げること
/// （コーパスは今後増えていく前提で、多少の変動は許容しつつ後退だけを
/// 検知する。100%固定ではない）。
const GENERAL_ACCURACY_FLOOR: f64 = 1.0;

struct GoldenCase {
    input: String,
    expected: String,
}

fn parse_corpus(tsv: &str) -> Vec<GoldenCase> {
    tsv.lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut parts = l.splitn(2, '\t');
            let input = parts.next().unwrap_or("").to_string();
            let expected = parts.next().unwrap_or("").to_string();
            GoldenCase { input, expected }
        })
        .collect()
}

/// `cargo test -p hook-dll`のCWDはパッケージディレクトリ
/// （`crates/hook-dll`）であり、`conversion-service`/CLI/examplesが前提と
/// する「リポジトリルートからの実行」とは異なる。素朴な相対パスでは
/// 解決できないため、`CARGO_MANIFEST_DIR`から組み立てる。
fn real_dictionary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dictionaries/system.dic")
}

fn word_priority_path() -> PathBuf {
    real_dictionary_path().with_file_name("word_priority.tsv")
}

fn word_assoc_path() -> PathBuf {
    real_dictionary_path().with_file_name("word_assoc.tsv")
}

fn load_shared_dictionary() -> std::sync::Arc<Dictionary> {
    let path = real_dictionary_path();
    let dict = Dictionary::load(&path).unwrap_or_else(|e| {
        panic!(
            "実辞書の読み込みに失敗しました（{}）: {e}。\
             リポジトリ直下のdictionaries/system.dicが存在するか確認してください。",
            path.display()
        )
    });
    std::sync::Arc::new(dict)
}

/// `LiveConversionState::load_dictionary`相当を、ディスクからの再読み込み
/// 無しで行う。
///
/// コーパスのケースごとに新規`ViterbiConverter`を作るのは、学習系
/// `HashMap`（`learned_unigram`/`learned_bigram`/`learned_assoc`/
/// `learned_hiragana`、いずれも`ViterbiConverter`本体のフィールド）が
/// ケース間で共有・汚染されるのを防ぐため。同じインスタンスを使い回すと、
/// あるケースの確定が学習として次のケースの変換に影響し、コーパスの
/// 並び順に結果が依存する「テスト自体の自己汚染」が起きる
/// （[[learned-hiragana-fragments-longer-word-bug]]で確認済みの
/// 「自己強化ループ」と同種のリスク）。
///
/// 2026-09-13より`dict.clone()`（`Dictionary`のTrie全体を深くコピー、
/// 実測300〜400ms/回）を`ViterbiConverter::from_shared(Arc::clone(dict))`
/// （参照カウントを1つ増やすだけ、実質ゼロコスト）に置き換えた
/// （[[dictionary-arc-sharing]]）。基底辞書（`Arc`共有・不変）は使い回すが、
/// `ViterbiConverter`本体・上乗せ辞書（`overlay`）・学習系`HashMap`は
/// `from_shared`のたびに新規作成されるため、上記の「テスト自体の自己汚染」
/// を防ぐ効果は変わらない（`Arc`化したのは不変の基底辞書だけ）。
///
/// `learning`は意図的に`None`のまま（学習を完全に無効化）にする。
/// このゴールデンテストが測りたいのは「辞書＋アルゴリズム単体で正しい
/// 変換ができるか」であり、学習ボーナスが乗った状態だと「学習済みだから
/// 通っている」のか「そもそも辞書・アルゴリズムだけで正しいのか」が
/// 区別できなくなる。学習の有無で結果がどう変わるかを見たい場合は、
/// それ自体を別の指標として計測すること（このテストの役目ではない）。
fn build_state(dict: &std::sync::Arc<Dictionary>) -> LiveConversionState {
    let mut converter = ViterbiConverter::from_shared(std::sync::Arc::clone(dict));
    let _ = converter.load_word_priority_file(&word_priority_path());
    // `seeded_assoc`（隣接コロケーション、[[homophone-selection-is-not-fixable-without-lm]]）
    // は学習DBと無関係の辞書付随データなので、`learning: None`（学習無効化）
    // とは別に必ず読み込む。読み込まないと同音異義語コロケーションの
    // 効果がゴールデンテストで一切測れない。
    let _ = converter.load_word_assoc_file(&word_assoc_path());
    let mut state = LiveConversionState::new();
    state.converter = Some(converter);
    state
}

/// `ConversionAction{delete_count, insert_text}`を、`hook.rs::execute_action`
/// と同じ意味論（末尾`delete_count`文字をBackspaceで消してから
/// `insert_text`を追記）でシミュレート文書に反映する。
fn apply_action(sim: &mut String, action: Option<ConversionAction>) {
    let Some(action) = action else { return };
    if action.delete_count > 0 {
        let keep = sim.chars().count().saturating_sub(action.delete_count);
        *sim = sim.chars().take(keep).collect();
    }
    sim.push_str(&action.insert_text);
}

/// 1文字ずつ`add_char`（実際のキー入力と同じ）に流し込み、最後に`commit`
/// （Space/Enter相当）して最終確定させる。
fn run_case(state: &mut LiveConversionState, input: &str) -> String {
    let mut sim = String::new();
    for ch in input.chars() {
        let action = state.add_char(ch);
        apply_action(&mut sim, action);
    }
    let action = state.commit();
    apply_action(&mut sim, action);
    sim
}

/// 1文字ずつ`add_char`に流し込み、毎回`update_predictions`も呼ぶ（実際の
/// キー入力と同じ。hook.rsは`add_char`直後に必ずこれを呼ぶ）。全文字入力後、
/// 「もしかして」候補があれば`commit_prediction(0)`で採用し、無ければ通常の
/// `commit`にフォールバックする。
///
/// 通常の`run_case`（`golden_regressions`/`golden_general`が使う）は
/// `update_predictions`を呼ばないため、「もしかして」補正カスケード
/// （[[correction-cascade-unification]]）を一切通らない。この関数は
/// そのカスケード専用の駆動経路。
fn run_case_with_predictions(state: &mut LiveConversionState, input: &str) -> String {
    let mut sim = String::new();
    for ch in input.chars() {
        let action = state.add_char(ch);
        apply_action(&mut sim, action);
        state.update_predictions();
    }
    let action = if !state.predictions.is_empty() {
        state.commit_prediction(0)
    } else {
        state.commit()
    };
    apply_action(&mut sim, action);
    sim
}

#[test]
fn golden_corrections_must_all_pass() {
    let dict = load_shared_dictionary();
    let cases = parse_corpus(CORRECTIONS_TSV);
    assert!(!cases.is_empty(), "golden/corrections.tsv が空です");

    let mut failures = Vec::new();
    for case in &cases {
        let mut state = build_state(&dict);
        let actual = run_case_with_predictions(&mut state, &case.input);
        if actual != case.expected {
            failures.push(format!(
                "入力={} 期待={} 実際={}",
                case.input, case.expected, actual
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "「もしかして」補正カスケードの回帰を検出しました（{}/{}件）:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn golden_regressions_must_all_pass() {
    let dict = load_shared_dictionary();
    let cases = parse_corpus(REGRESSIONS_TSV);
    assert!(!cases.is_empty(), "golden/regressions.tsv が空です");

    let mut failures = Vec::new();
    for case in &cases {
        let mut state = build_state(&dict);
        let actual = run_case(&mut state, &case.input);
        if actual != case.expected {
            failures.push(format!(
                "入力={} 期待={} 実際={}",
                case.input, case.expected, actual
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "既知バグの回帰を検出しました（{}/{}件）:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn golden_general_accuracy_meets_baseline() {
    let dict = load_shared_dictionary();
    let cases = parse_corpus(GENERAL_TSV);
    assert!(!cases.is_empty(), "golden/general.tsv が空です");

    let mut pass = 0usize;
    let mut mismatches = Vec::new();
    for case in &cases {
        let mut state = build_state(&dict);
        let actual = run_case(&mut state, &case.input);
        if actual == case.expected {
            pass += 1;
        } else {
            mismatches.push(format!(
                "入力={} 期待={} 実際={}",
                case.input, case.expected, actual
            ));
        }
    }
    let rate = pass as f64 / cases.len() as f64;
    assert!(
        rate >= GENERAL_ACCURACY_FLOOR,
        "一般精度コーパスの一致率が閾値を下回りました: {:.1}% (閾値{:.1}%, {}/{}件)\n不一致:\n{}",
        rate * 100.0,
        GENERAL_ACCURACY_FLOOR * 100.0,
        pass,
        cases.len(),
        mismatches.join("\n")
    );
}

/// 隣接コロケーションシード（word_assoc.tsv）専用コーパスの全件チェック。
/// ケースごとに実辞書＋新規`ViterbiConverter`（＝`Dictionary`の深い
/// クローン）を作るため、量産で件数が増えるほど`cargo test`の所要時間が
/// 線形に伸びる（実測: general.tsvが99→129件になっただけでhook-dllの
/// テスト全体が76秒→131秒に増加）。既定の`cargo test`では代わりに
/// `golden_collocations_sample_must_pass`（3件に1件だけ実行）を使い、
/// こちらは`#[ignore]`にして明示的に（`cargo test -- --ignored`、または
/// 量産バッチ追加時・マージ前に）実行する。
#[test]
#[ignore]
fn golden_collocations_full_must_pass() {
    let dict = load_shared_dictionary();
    let cases = parse_corpus(COLLOCATIONS_TSV);
    assert!(!cases.is_empty(), "golden/collocations.tsv が空です");
    run_collocations_cases(&dict, &cases, 1);
}

/// `golden_collocations_full_must_pass`の高速版。3件に1件だけ実際に
/// 変換して確認する（既定の`cargo test`で実行される）。サンプリングは
/// コーパス内の並び順に対して決定的（`index % 3 == 0`）なので、
/// 新しいバッチを追記するたびにサンプル対象が偏らないよう、各バッチの
/// 実装時には必ず`--ignored`版を1回通してから採用すること。
#[test]
fn golden_collocations_sample_must_pass() {
    let dict = load_shared_dictionary();
    let cases = parse_corpus(COLLOCATIONS_TSV);
    assert!(!cases.is_empty(), "golden/collocations.tsv が空です");
    run_collocations_cases(&dict, &cases, 3);
}

/// `every_nth`件に1件（`index % every_nth == 0`）だけ実際に変換して
/// 期待表記と比較する。`every_nth == 1`なら全件。
fn run_collocations_cases(dict: &std::sync::Arc<Dictionary>, cases: &[GoldenCase], every_nth: usize) {
    let mut failures = Vec::new();
    let mut checked = 0usize;
    for (i, case) in cases.iter().enumerate() {
        if i % every_nth != 0 {
            continue;
        }
        checked += 1;
        let mut state = build_state(dict);
        let actual = run_case(&mut state, &case.input);
        if actual != case.expected {
            failures.push(format!(
                "入力={} 期待={} 実際={}",
                case.input, case.expected, actual
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "隣接コロケーションシードの回帰を検出しました（{}/{}件、{}件中{}件を検査）:\n{}",
        failures.len(),
        checked,
        cases.len(),
        checked,
        failures.join("\n")
    );
}
