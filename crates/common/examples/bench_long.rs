use common::{Dictionary, ViterbiConverter};
use std::path::Path;
use std::time::Instant;

fn main() {
    let dict = Dictionary::load(Path::new("dictionaries/system.dic")).expect("dict load failed");
    let conv = ViterbiConverter::new(dict);
    let base = "きょうはがっこうにいってべんきょうをしてかいしゃにかえってからばんごはんをたべておふろにはいってねました";
    for reps in [1usize, 5, 10] {
        let input: String = base.repeat(reps);
        let n = 30;
        let _ = conv.convert_context_aware(&input);
        let t = Instant::now();
        for _ in 0..n {
            let _ = conv.convert_context_aware(&input);
        }
        let per = t.elapsed() / n as u32;
        println!("len={:4} {:?}/call", input.chars().count(), per);
    }
}
