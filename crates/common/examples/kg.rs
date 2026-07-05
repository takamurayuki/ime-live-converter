use common::{Dictionary, ViterbiConverter, LearningRepository};
use std::path::Path;
fn load(with_db: bool) -> ViterbiConverter {
    let mut v = ViterbiConverter::new(Dictionary::load(Path::new("dictionaries/system.dic")).unwrap());
    if with_db {
        if let Ok(l) = LearningRepository::open(Path::new("ime-learning.db")) {
            for (r,s,f) in l.all_unigrams().unwrap_or_default() { v.learn_unigram(&r,&s,f); }
            for (p,s,f) in l.all_bigrams().unwrap_or_default() { v.learn_bigram(&p,&s,f); }
            for (p,cc,f) in l.all_assocs().unwrap_or_default() { v.learn_assoc(&p,&cc,f); }
        }
    }
    v
}
fn main() {
    for (lbl, wdb) in [("辞書のみ", false), ("学習込み", true)] {
        let v = load(wdb);
        println!("=== {} ===", lbl);
        for s in ["かんがえる","かんがえたい","したい","べんきょうしたい","たべたい"] {
            println!("  {:16} -> {}", s, v.convert_context_aware_to_string(s));
        }
    }
    // 辞書エントリ確認
    let d = Dictionary::load(Path::new("dictionaries/system.dic")).unwrap();
    for r in ["かんがえる","かんが","したい"] {
        print!("[{}] ", r);
        match d.lookup(r){Some(ws)=>{let mut w:Vec<_>=ws.iter().collect();w.sort_by_key(|e|e.cost);for e in w.iter().take(4){print!("{}({}) ",e.surface,e.cost);}println!()},None=>println!("(無し)")}
    }
}
