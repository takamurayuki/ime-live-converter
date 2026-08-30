//! 学習DBへの書き込み時間を測る（確定1回分＝文節ごとのユニグラム/バイグラム/連想記録）
use common::LearningRepository;
fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "ime-learning.db".to_string());
    let db = LearningRepository::open(&path).expect("open");
    let segs = ["今日", "は", "学校", "に", "行っ", "て", "勉強", "を", "し", "た", "友達", "と", "遊ん", "だ", "。"];
    let t0 = std::time::Instant::now();
    for (i, s) in segs.iter().enumerate() {
        let _ = db.record_commit(&format!("よみ{}", i), s, None);
        if i > 0 {
            let _ = db.record_bigram(segs[i - 1], s);
            let _ = db.record_assoc(segs[i - 1], s);
        }
    }
    println!("commit of {} segments: {} ms", segs.len(), t0.elapsed().as_millis());
}
