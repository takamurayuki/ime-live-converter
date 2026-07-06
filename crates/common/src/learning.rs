//! ユーザー辞書と学習機能
//! 
//! 要件定義書 7.6 ユーザー辞書機能、7.7 変換履歴学習機能に基づく実装

use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::Path;

/// ユーザー辞書エントリ
#[derive(Debug, Clone)]
pub struct UserDictEntry {
    pub id: Option<i64>,
    /// 読み（ひらがな）
    pub reading: String,
    /// 表記（変換後）
    pub surface: String,
    /// 品詞（オプション）
    pub pos: Option<String>,
    /// コスト（低いほど優先）
    pub cost: i32,
    /// 作成日時
    pub created_at: String,
    /// 更新日時
    pub updated_at: String,
}

/// 変換履歴エントリ
#[derive(Debug, Clone)]
pub struct ConversionHistoryEntry {
    pub id: Option<i64>,
    /// 読み（ひらがな）
    pub reading: String,
    /// 表記（変換結果）
    pub surface: String,
    /// 使用回数
    pub frequency: u32,
    /// 最終使用日時
    pub last_used_at: String,
    /// アプリケーション名（オプション）
    pub app_name: Option<String>,
}

/// 学習リポジトリ
pub struct LearningRepository {
    conn: Connection,
}

impl LearningRepository {
    /// データベースを開く（または作成）
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        let repo = Self { conn };
        repo.initialize()?;
        Ok(repo)
    }

    /// インメモリデータベースを作成
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let repo = Self { conn };
        repo.initialize()?;
        Ok(repo)
    }

    /// テーブルを初期化
    fn initialize(&self) -> Result<()> {
        // ユーザー辞書テーブル
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS user_dictionary (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                reading TEXT NOT NULL,
                surface TEXT NOT NULL,
                pos TEXT,
                cost INTEGER DEFAULT 50,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(reading, surface)
            )",
            [],
        )?;

        // 変換履歴テーブル
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS conversion_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                reading TEXT NOT NULL,
                surface TEXT NOT NULL,
                frequency INTEGER NOT NULL DEFAULT 1,
                last_used_at TEXT NOT NULL,
                app_name TEXT,
                UNIQUE(reading, surface, app_name)
            )",
            [],
        )?;

        // 単語バイグラム（文脈学習）テーブル
        // prev_surface の次に surface が確定された回数を記録し、
        // 文全体の整合性（語のつながり）を学習する。
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS word_bigram (
                prev_surface TEXT NOT NULL,
                surface TEXT NOT NULL,
                frequency INTEGER NOT NULL DEFAULT 1,
                UNIQUE(prev_surface, surface)
            )",
            [],
        )?;

        // 内容語連想テーブル（助詞を飛ばした内容語どうしの結びつき）
        // 「新聞…記者」「駅…汽車」のように離れた語の関係を学習し、
        // 前後の文脈から尤もらしい変換を選ぶのに使う。
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS word_assoc (
                prev_content TEXT NOT NULL,
                content TEXT NOT NULL,
                frequency INTEGER NOT NULL DEFAULT 1,
                UNIQUE(prev_content, content)
            )",
            [],
        )?;

        // ひらがな優先テーブル（Escでひらがなに戻した読みを覚える）
        // 例: 「したい」を「慕い」にせずひらがなのまま出すため。
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS hiragana_pref (
                reading TEXT NOT NULL,
                frequency INTEGER NOT NULL DEFAULT 1,
                UNIQUE(reading)
            )",
            [],
        )?;

        // インデックス作成
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_user_dict_reading ON user_dictionary(reading)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_history_reading ON conversion_history(reading)",
            [],
        )?;
        self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_bigram_prev ON word_bigram(prev_surface)",
            [],
        )?;

        Ok(())
    }

    // ============ ユーザー辞書 ============

    /// ユーザー辞書に登録
    pub fn add_user_word(&self, reading: &str, surface: &str, pos: Option<&str>, cost: i32) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        
        self.conn.execute(
            "INSERT OR REPLACE INTO user_dictionary (reading, surface, pos, cost, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, COALESCE((SELECT created_at FROM user_dictionary WHERE reading = ?1 AND surface = ?2), ?5), ?5)",
            params![reading, surface, pos, cost, now],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// ユーザー辞書から削除
    pub fn remove_user_word(&self, reading: &str, surface: &str) -> Result<bool> {
        let rows = self.conn.execute(
            "DELETE FROM user_dictionary WHERE reading = ?1 AND surface = ?2",
            params![reading, surface],
        )?;
        Ok(rows > 0)
    }

    /// 読みで検索
    pub fn find_user_words(&self, reading: &str) -> Result<Vec<UserDictEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, reading, surface, pos, cost, created_at, updated_at
             FROM user_dictionary
             WHERE reading = ?1
             ORDER BY cost ASC"
        )?;

        let entries = stmt.query_map(params![reading], |row| {
            Ok(UserDictEntry {
                id: Some(row.get(0)?),
                reading: row.get(1)?,
                surface: row.get(2)?,
                pos: row.get(3)?,
                cost: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// 前方一致で検索
    pub fn find_user_words_prefix(&self, prefix: &str) -> Result<Vec<UserDictEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, reading, surface, pos, cost, created_at, updated_at
             FROM user_dictionary
             WHERE reading LIKE ?1
             ORDER BY cost ASC"
        )?;

        let pattern = format!("{}%", prefix);
        let entries = stmt.query_map(params![pattern], |row| {
            Ok(UserDictEntry {
                id: Some(row.get(0)?),
                reading: row.get(1)?,
                surface: row.get(2)?,
                pos: row.get(3)?,
                cost: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// 全ユーザー辞書を取得
    pub fn get_all_user_words(&self) -> Result<Vec<UserDictEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, reading, surface, pos, cost, created_at, updated_at
             FROM user_dictionary
             ORDER BY reading ASC, cost ASC"
        )?;

        let entries = stmt.query_map([], |row| {
            Ok(UserDictEntry {
                id: Some(row.get(0)?),
                reading: row.get(1)?,
                surface: row.get(2)?,
                pos: row.get(3)?,
                cost: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    // ============ 変換履歴 ============

    /// 変換を記録（確定時に呼ぶ）
    pub fn record_commit(&self, reading: &str, surface: &str, app_name: Option<&str>) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let app = app_name.unwrap_or(""); // NULLの代わりに空文字列を使用

        self.conn.execute(
            "INSERT INTO conversion_history (reading, surface, frequency, last_used_at, app_name)
             VALUES (?1, ?2, 1, ?3, ?4)
             ON CONFLICT(reading, surface, app_name) DO UPDATE SET
                frequency = frequency + 1,
                last_used_at = ?3",
            params![reading, surface, now, app],
        )?;

        Ok(())
    }

    /// 使用頻度を取得
    pub fn find_frequency(&self, reading: &str, surface: &str) -> Result<u32> {
        let result: std::result::Result<u32, _> = self.conn.query_row(
            "SELECT COALESCE(SUM(frequency), 0) FROM conversion_history
             WHERE reading = ?1 AND surface = ?2",
            params![reading, surface],
            |row| row.get(0),
        );

        Ok(result.unwrap_or(0))
    }

    /// 全ユニグラム（読み→表記の頻度合計）を取得
    ///
    /// 起動時に変換エンジンのメモリへ一括ロードするために使う。
    /// app_name はまたいで合算する。
    pub fn all_unigrams(&self) -> Result<Vec<(String, String, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT reading, surface, SUM(frequency)
             FROM conversion_history GROUP BY reading, surface",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ============ 単語バイグラム（文脈学習）============

    /// 連続する2語の並びを記録する（確定時に隣接ペアごとに呼ぶ）
    pub fn record_bigram(&self, prev_surface: &str, surface: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO word_bigram (prev_surface, surface, frequency)
             VALUES (?1, ?2, 1)
             ON CONFLICT(prev_surface, surface) DO UPDATE SET frequency = frequency + 1",
            params![prev_surface, surface],
        )?;
        Ok(())
    }

    /// バイグラムの頻度を取得
    pub fn find_bigram_frequency(&self, prev_surface: &str, surface: &str) -> Result<u32> {
        let result: std::result::Result<u32, _> = self.conn.query_row(
            "SELECT frequency FROM word_bigram WHERE prev_surface = ?1 AND surface = ?2",
            params![prev_surface, surface],
            |row| row.get(0),
        );
        Ok(result.unwrap_or(0))
    }

    /// 全バイグラム（前語, 次語, 頻度）を取得（起動時の一括ロード用）
    pub fn all_bigrams(&self) -> Result<Vec<(String, String, u32)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT prev_surface, surface, frequency FROM word_bigram")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ============ 内容語連想（文脈学習）============

    /// 内容語どうしの結びつきを記録する（助詞を飛ばした前後の内容語ペア）
    pub fn record_assoc(&self, prev_content: &str, content: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO word_assoc (prev_content, content, frequency)
             VALUES (?1, ?2, 1)
             ON CONFLICT(prev_content, content) DO UPDATE SET frequency = frequency + 1",
            params![prev_content, content],
        )?;
        Ok(())
    }

    /// 内容語連想の頻度を取得
    pub fn find_assoc_frequency(&self, prev_content: &str, content: &str) -> Result<u32> {
        let result: std::result::Result<u32, _> = self.conn.query_row(
            "SELECT frequency FROM word_assoc WHERE prev_content = ?1 AND content = ?2",
            params![prev_content, content],
            |row| row.get(0),
        );
        Ok(result.unwrap_or(0))
    }

    /// 全内容語連想（前語, 次語, 頻度）を取得（起動時の一括ロード用）
    pub fn all_assocs(&self) -> Result<Vec<(String, String, u32)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT prev_content, content, frequency FROM word_assoc")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ============ ひらがな優先（Escで戻した読み） ============

    /// ひらがな優先を記録（頻度+1）
    pub fn record_hiragana_pref(&self, reading: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO hiragana_pref (reading, frequency) VALUES (?1, 1)
             ON CONFLICT(reading) DO UPDATE SET frequency = frequency + 1",
            params![reading],
        )?;
        Ok(())
    }

    /// ひらがな優先の頻度を取得
    pub fn find_hiragana_pref(&self, reading: &str) -> Result<u32> {
        let freq: Option<u32> = self
            .conn
            .query_row(
                "SELECT frequency FROM hiragana_pref WHERE reading = ?1",
                params![reading],
                |row| row.get(0),
            )
            .ok();
        Ok(freq.unwrap_or(0))
    }

    /// 指定した読みの変換履歴（ひらがな以外の表記）を削除する
    ///
    /// Esc でひらがなに戻したとき、その読みで学習済みの漢字/カタカナ表記の
    /// 記録を消し、ひらがなが選ばれるようにする。
    pub fn forget_reading(&self, reading: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM conversion_history WHERE reading = ?1 AND surface <> ?1",
            params![reading],
        )?;
        Ok(())
    }

    /// 全ひらがな優先（読み, 頻度）を取得（起動時の一括ロード用）
    pub fn all_hiragana_prefs(&self) -> Result<Vec<(String, u32)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT reading, frequency FROM hiragana_pref")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 読みの変換履歴を取得（頻度順）
    pub fn find_history(&self, reading: &str) -> Result<Vec<ConversionHistoryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, reading, surface, frequency, last_used_at, app_name
             FROM conversion_history
             WHERE reading = ?1
             ORDER BY frequency DESC, last_used_at DESC"
        )?;

        let entries = stmt.query_map(params![reading], |row| {
            Ok(ConversionHistoryEntry {
                id: Some(row.get(0)?),
                reading: row.get(1)?,
                surface: row.get(2)?,
                frequency: row.get(3)?,
                last_used_at: row.get(4)?,
                app_name: row.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    /// 履歴をクリア
    pub fn clear_history(&self) -> Result<()> {
        self.conn.execute("DELETE FROM conversion_history", [])?;
        Ok(())
    }

    /// 古い履歴を削除（指定日数より古いもの）
    pub fn prune_old_history(&self, days: i64) -> Result<usize> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        let cutoff_str = cutoff.to_rfc3339();

        let rows = self.conn.execute(
            "DELETE FROM conversion_history WHERE last_used_at < ?1",
            params![cutoff_str],
        )?;

        Ok(rows)
    }

    // ============ スコア計算 ============

    /// 候補のスコアボーナスを計算（頻度に基づく）
    pub fn calculate_frequency_bonus(&self, reading: &str, surface: &str) -> Result<f32> {
        let frequency = self.find_frequency(reading, surface)?;
        
        // 頻度が高いほどボーナス（最大-5000）
        // frequency=1 → -100, frequency=10 → -1000, frequency=50+ → -5000
        let bonus = (frequency as f32 * 100.0).min(5000.0);
        Ok(-bonus)
    }

    /// ユーザー辞書の優先度ボーナス
    pub fn calculate_user_dict_bonus(&self, reading: &str, surface: &str) -> Result<f32> {
        let entries = self.find_user_words(reading)?;
        
        for entry in entries {
            if entry.surface == surface {
                // ユーザー辞書にある場合は大幅ボーナス
                return Ok(-10000.0 + entry.cost as f32);
            }
        }
        
        Ok(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_dictionary() -> Result<()> {
        let repo = LearningRepository::in_memory()?;

        // 登録
        repo.add_user_word("らすと", "Rust", Some("名詞"), 50)?;
        repo.add_user_word("ねくすと", "Next.js", Some("名詞"), 50)?;

        // 検索
        let entries = repo.find_user_words("らすと")?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].surface, "Rust");

        // 削除
        repo.remove_user_word("らすと", "Rust")?;
        let entries = repo.find_user_words("らすと")?;
        assert!(entries.is_empty());

        Ok(())
    }

    #[test]
    fn test_conversion_history() -> Result<()> {
        let repo = LearningRepository::in_memory()?;

        // 記録
        repo.record_commit("きょう", "今日", None)?;
        repo.record_commit("きょう", "今日", None)?;
        repo.record_commit("きょう", "今日", None)?;

        // 頻度確認
        let freq = repo.find_frequency("きょう", "今日")?;
        assert_eq!(freq, 3);

        // 履歴取得（同じ読み+表記はマージされる）
        let history = repo.find_history("きょう")?;
        assert_eq!(history.len(), 1); // ユニークなエントリは1つ
        assert_eq!(history[0].frequency, 3); // 頻度は3

        Ok(())
    }

    #[test]
    fn test_frequency_bonus() -> Result<()> {
        let repo = LearningRepository::in_memory()?;

        // 10回記録
        for _ in 0..10 {
            repo.record_commit("てすと", "テスト", None)?;
        }

        let bonus = repo.calculate_frequency_bonus("てすと", "テスト")?;
        assert!(bonus < 0.0); // ボーナスはマイナス（スコアを下げる）

        Ok(())
    }
}
