//! 隣接コロケーションシード（[[seeded-adjacent-collocation]]）の候補生成時に、
//! LLM（または人手）へ渡す「実際の同音異義語一覧」を辞書から機械的に
//! 引くツール。
//!
//! 24グループを知識だけで生成したとき、「しょうか」に消化と消火の両方が
//! あることを見落とす等、実在するが想定していなかった同音異義語による
//! 失敗が最多だった。生成前にこのツールの出力をプロンプトに含めれば、
//! 「この中から『消化』だけを指す語を」という指示ができ、見落としを
//! 大幅に減らせる。
//!
//! 量産バッチ2（20/34合格）で見つかった追加の教訓を反映済み:
//! - **トリガー語自身の読みにも必ず通すこと**（「ひかく」のつもりが安い
//!   「非核」に負ける、等をターゲットの読みだけ見ていたら発見できなかった）。
//! - `読み=表記` の形で引数を渡すと、その表記を`word_assoc.tsv`の出力表記
//!   （target）にした場合に必要なボーナスと、採用可否（上限2500超で不採用）
//!   を`ViterbiConverter::compute_word_assoc_bonus`と全く同じロジックで
//!   事前に判定して表示する（本番の判定と食い違わない）。生成前にこれで
//!   弾けるものは候補にすら入れない、という運用にする。
//!
//! 量産バッチ4（15/18合格）で見つかった追加の教訓（2026-09-13）:
//! - **「最安」だけでは足りない。「単独最安」（同点の競合が無い）まで見る**
//!   （「回収」と「改修」がコスト完全同点で、トリガーとして使うとどちらが
//!   出るか安定しなかった）。
//! - **品詞も見る**。形容動詞語幹（「不要」等）は単独では最安でも、隣接語
//!   としてはうまく機能しないことがある（な形容詞は本来「〜な」を伴う
//!   活用語で、助詞を挟まない直接複合の一部として振る舞うのに適さない
//!   ため）。
//! これらは`トリガーとして使えるか`の判定にのみ関わる（ターゲットとして
//! シードする分には無関係。シードした瞬間にボーナスで確実に勝たせるため）。
//!
//! 使い方:
//!   cargo run -p common --example dump_homophones -- しょうか きせい=既製
//!   （引数を省略すると標準入力から1行1読みで読む。`=表記`は任意）

use common::{Dictionary, ViterbiConverter};
use std::io::BufRead;

fn is_content(pos: &str) -> bool {
    !(pos.starts_with("助詞")
        || pos.starts_with("助動詞")
        || pos.starts_with("記号")
        || pos.starts_with("未知語")
        || pos.starts_with("フィラー")
        || pos.starts_with("カタカナ"))
}

/// トリガー語として不向きな品詞か。
///
/// 形容動詞語幹（「不要」「自然」等の「〜な」で活用する語の語幹）は、
/// 単独の辞書コストが最安でも、助詞を挟まない直接隣接複合語の一部として
/// 機能させると期待通りに振る舞わないことを実測で確認した（「不要」＋
/// 「不急」→「不要不急」を狙ったが「フヨウ普及」になった）。原因は
/// 未特定（おそらく形容動詞語幹という接続クラス自体の接続コストの癖）
/// だが、実測で確認できた範囲でこの品詞をトリガーには使わないよう
/// 弾いておく。他の品詞クラスでも同様の問題が見つかれば追加すること。
fn is_risky_trigger_pos(pos: &str) -> bool {
    pos.contains("形容動詞語幹")
}

fn main() {
    let dict = Dictionary::load(std::path::Path::new("dictionaries/system.dic"))
        .unwrap_or_else(|e| panic!("実辞書の読み込みに失敗しました: {e}"));
    // ボーナス自動算出のロジックだけ借りる（実際のシードは投入しない）。
    let converter = ViterbiConverter::new(dict.clone());

    let args: Vec<String> = std::env::args().skip(1).collect();
    let inputs: Vec<String> = if args.is_empty() {
        std::io::stdin()
            .lock()
            .lines()
            .map_while(Result::ok)
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    } else {
        args
    };

    for input in inputs {
        let (reading, target) = match input.split_once('=') {
            Some((r, t)) => (r, Some(t)),
            None => (input.as_str(), None),
        };
        println!("=== {reading} ===");
        let Some(entries) = dict.lookup(reading) else {
            println!("  (辞書に無し)");
            continue;
        };
        let mut seen = std::collections::HashSet::new();
        let mut rows: Vec<&common::WordEntry> = entries
            .iter()
            .filter(|e| is_content(&e.pos))
            .filter(|e| seen.insert((e.surface.clone(), e.pos.clone())))
            .collect();
        rows.sort_by_key(|e| e.cost);
        for e in &rows {
            println!("  {}\tcost={}\tpos={}", e.surface, e.cost, e.pos);
        }
        if let Some(target) = target {
            match converter.compute_word_assoc_bonus(reading, target) {
                Some(bonus) if bonus <= common::viterbi::SEEDED_ASSOC_MARGIN => {
                    println!("  => ターゲットとして: 既定で最安（ボーナス0でも勝てる）。採用可");
                    // ここから「トリガーとして使えるか」の追加判定
                    // （単独最安か＝同点の競合が無いか、品詞は安全か）。
                    let target_cost = rows
                        .iter()
                        .filter(|e| e.surface == target)
                        .map(|e| e.cost)
                        .min();
                    let tie = target_cost.is_some_and(|tc| {
                        rows.iter().any(|e| e.surface != target && e.cost <= tc)
                    });
                    let risky_pos = rows
                        .iter()
                        .find(|e| e.surface == target)
                        .is_some_and(|e| is_risky_trigger_pos(&e.pos));
                    match (tie, risky_pos) {
                        (false, false) => println!(
                            "  => トリガーとして: 単独最安・品詞も問題なし。使って良い"
                        ),
                        (true, _) => println!(
                            "  => トリガーとして: 不可。同点コストの競合があり、どちらが\
                             出るか安定しない（「回収」＝「改修」と同じパターン）"
                        ),
                        (false, true) => println!(
                            "  => トリガーとして: 不可。品詞（形容動詞語幹）が隣接複合語に\
                             不向き（「不要」＋「不急」→「フヨウ普及」になった実例と同じ\
                             パターン）"
                        ),
                    }
                }
                Some(bonus) => println!(
                    "  => ターゲットとして: シードすれば採用可能（必要ボーナス={bonus}、上限内）。\
                     トリガーとして: 不可（何もしなければ既定では勝てない。それ自体が\
                     別の同音異義語に負ける）"
                ),
                None => {
                    let cheapest = rows.first().map(|e| (e.surface.as_str(), e.cost));
                    match (rows.iter().find(|e| e.surface == target), cheapest) {
                        (Some(t), Some((_, cheapest_cost))) => println!(
                            "  => 「{target}」(cost={}) は不採用: 最安候補との差が大きすぎる（上限超過、必要ボーナス概算={}）",
                            t.cost,
                            (t.cost as i32 - cheapest_cost as i32) + 300
                        ),
                        _ => println!(
                            "  => 「{target}」は不採用: この読みの候補一覧に見つからない（表記が間違っている可能性）"
                        ),
                    }
                }
            }
        }
    }
}
