//! 自動変換の表示と候補一覧（`build_candidates`）の順位を突き合わせる再現ハーネス。
//!
//! 実運用と同じ `LiveConversionState::load_dictionary`（judge_lm.bin があれば判断層も
//! 同期読み込み）に、学習DBのコピーを付けて、標準入力の各行（ひらがな）を変換する。
//! 自動変換で表示される対象文節が候補一覧の何位にあるかを表示する。
//!
//! 使い方: printf 'かいの\n' | cargo run --release -p common --example candidate_check
//! 環境変数: IME_NO_LEARNING=1 で学習DBを付けない / IME_JUDGE=0 で判断層なし
//! IME_TYPE=1 なら各行をローマ字として1文字ずつ `add_char` に流し（実際のキー入力と同じ、
//! 先頭文節の固定も効く）、打鍵ごとの表示と `commit` 後の確定結果を出す。
use common::{LearningRepository, LiveConversionState};
use std::io::BufRead;
use std::path::Path;

fn main() {
    let mut state = LiveConversionState::new();
    if std::env::var("IME_NO_LEARNING").is_err() {
        let copy = std::env::temp_dir().join(format!("candidate_check_{}.db", std::process::id()));
        std::fs::copy("ime-learning.db", &copy).expect("学習DBのコピーに失敗");
        // WAL モードのため、未チェックポイントの最近の学習は -wal 側にしか無い
        let wal = std::path::PathBuf::from(format!("{}-wal", copy.display()));
        let _ = std::fs::remove_file(&wal);
        if Path::new("ime-learning.db-wal").exists() {
            std::fs::copy("ime-learning.db-wal", &wal).expect("学習DB(WAL)のコピーに失敗");
        }
        state.learning = Some(LearningRepository::open(&copy).expect("学習DBオープン失敗"));
    }
    assert!(state.load_dictionary(Path::new("dictionaries/system.dic")), "辞書ロード失敗");
    let judge_on = state.converter.as_ref().is_some_and(|c| c.judge.is_some());
    println!("判断層: {}", if judge_on { "あり" } else { "なし" });

    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let reading = line.trim();
        if reading.is_empty() {
            continue;
        }
        if std::env::var("IME_TYPE").is_ok() {
            let apply = |sim: &mut String, action: Option<common::ConversionAction>| {
                let Some(a) = action else { return };
                let keep = sim.chars().count().saturating_sub(a.delete_count);
                *sim = sim.chars().take(keep).collect();
                sim.push_str(&a.insert_text);
            };
            let mut sim = String::new();
            for ch in reading.chars() {
                let action = state.add_char(ch);
                apply(&mut sim, action);
                println!("  {ch} → {sim}");
            }
            let action = state.commit();
            apply(&mut sim, action);
            println!("{reading} ⇒ 確定「{sim}」");
            continue;
        }
        state.hiragana_buffer = reading.to_string();
        let shown: String = state.convert_buffer(reading).iter().map(|e| e.surface.as_str()).collect();
        let (cands, seg_reading, seg_surfaces, prefix, _, suffix, _) = state.build_candidates();
        let shown_seg = shown
            .strip_prefix(prefix.as_str())
            .and_then(|s| s.strip_suffix(suffix.as_str()))
            .unwrap_or("");
        let rank = seg_surfaces.iter().position(|s| s == shown_seg);
        println!(
            "{} → 表示「{}」 対象[{}]=「{}」 候補内の順位: {}",
            reading,
            shown,
            seg_reading,
            shown_seg,
            rank.map_or("なし".to_string(), |r| format!("{}位", r + 1))
        );
        println!("    候補: {}", seg_surfaces.iter().take(8).cloned().collect::<Vec<_>>().join(" / "));
        let _ = cands;
    }
}
