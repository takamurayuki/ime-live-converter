use crate::{Dictionary, WordEntry, ConnectionMatrix, TrieNode};
use anyhow::Result;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

/// シリアライズ可能な辞書形式
#[derive(Serialize, Deserialize)]
pub struct SerializableDictionary {
    pub words: Vec<SerializableWordEntry>,
    pub matrix_left_size: u16,
    pub matrix_right_size: u16,
    pub matrix_costs: Vec<i16>,
    pub bos_id: u16,
    pub eos_id: u16,
}

#[derive(Serialize, Deserialize)]
pub struct SerializableWordEntry {
    pub surface: String,
    pub reading: String,
    pub left_id: u16,
    pub right_id: u16,
    pub cost: i16,
    pub pos: String,
}

impl Dictionary {
    /// 辞書をバイナリファイルに保存
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut words = Vec::new();
        collect_words_from_trie(&self.trie, &mut words);

        let serializable = SerializableDictionary {
            words: words
                .into_iter()
                .map(|w| SerializableWordEntry {
                    surface: w.surface,
                    reading: w.reading,
                    left_id: w.left_id,
                    right_id: w.right_id,
                    cost: w.cost,
                    pos: w.pos,
                })
                .collect(),
            matrix_left_size: self.matrix.left_size,
            matrix_right_size: self.matrix.right_size,
            matrix_costs: self.matrix.costs.clone(),
            bos_id: self.bos_id,
            eos_id: self.eos_id,
        };

        let file = File::create(path)?;
        let encoder = GzEncoder::new(BufWriter::new(file), Compression::default());
        bincode::serialize_into(encoder, &serializable)?;

        Ok(())
    }

    /// バイナリファイルから辞書を読み込み
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let decoder = GzDecoder::new(BufReader::new(file));
        let serializable: SerializableDictionary = bincode::deserialize_from(decoder)?;

        let mut dict = Dictionary::new();
        dict.matrix = ConnectionMatrix {
            left_size: serializable.matrix_left_size,
            right_size: serializable.matrix_right_size,
            costs: serializable.matrix_costs,
        };
        dict.bos_id = serializable.bos_id;
        dict.eos_id = serializable.eos_id;

        for entry in serializable.words {
            dict.add_word(WordEntry {
                surface: entry.surface,
                reading: entry.reading,
                left_id: entry.left_id,
                right_id: entry.right_id,
                cost: entry.cost,
                pos: entry.pos,
            });
        }

        Ok(dict)
    }
}

/// Trieから全単語を収集
fn collect_words_from_trie(node: &TrieNode, words: &mut Vec<WordEntry>) {
    for entry in &node.entries {
        words.push(entry.clone());
    }
    for child in node.children.values() {
        collect_words_from_trie(child, words);
    }
}
