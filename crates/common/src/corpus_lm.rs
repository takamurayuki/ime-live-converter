use anyhow::Result;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

/// 実コーパスから集計した語頻度データ。
///
/// (読み, 表記, 頻度) のユニグラムと (前表記, 表記, 頻度) のバイグラムのみを
/// 保持する薄いデータ形式。個人の学習（`learned_unigram`/`learned_bigram`,
/// SQLiteの`ime-learning.db`由来）とは別チャネルとして`ViterbiConverter`へ
/// 読み込まれる（`clear_learning`等の個人学習リセットで消えない、配布・
/// 差し替えが独立にできる）。
#[derive(Serialize, Deserialize, Default)]
pub struct CorpusLm {
    pub unigrams: Vec<(String, String, u32)>,
    pub bigrams: Vec<(String, String, u32)>,
}

impl CorpusLm {
    pub fn save(&self, path: &Path) -> Result<()> {
        let file = File::create(path)?;
        let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
        bincode::serialize_into(encoder, self)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let decoder = GzDecoder::new(BufReader::new(file));
        let lm: CorpusLm = bincode::deserialize_from(decoder)?;
        Ok(lm)
    }
}
