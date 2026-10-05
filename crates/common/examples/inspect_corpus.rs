use common::{Dictionary, ViterbiConverter};
use std::io::{BufRead, Write};
use std::path::Path;

fn main() {
    let conv = ViterbiConverter::new(Dictionary::load(Path::new("dictionaries/system.dic")).unwrap());
    let f = std::fs::File::open(std::env::args().nth(1).unwrap()).unwrap();
    let out = std::io::stdout();
    let mut out = out.lock();
    for line in std::io::BufReader::new(f).lines() {
        let line = line.unwrap();
        let input = line.split('\t').next().unwrap_or(&line).trim();
        if input.is_empty() { continue; }
        let path = conv.convert(input);
        let surface: String = path.iter().map(|e| e.surface.as_str()).collect();
        let seg: String = path.iter().map(|e| e.surface.as_str()).collect::<Vec<_>>().join("|");
        writeln!(out, "{input}\t{surface}\t{seg}").ok();
    }
}
