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

        // コマンド履歴テーブル（コマンドモード）
        // ターミナルで実行したコマンド行を記録し、次回の前方一致補完に使う。
        // description は commands.csv 由来の簡易説明（学習コマンドは空）。
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS command_history (
                command TEXT PRIMARY KEY,
                frequency INTEGER NOT NULL DEFAULT 1,
                description TEXT NOT NULL DEFAULT '',
                last_used_at TEXT NOT NULL DEFAULT ''
            )",
            [],
        )?;

        // エイリアステーブル（コマンドモード）
        // alias を打つと expansion（実コマンド or スクリプトパス）を挿入する。
        // is_script=1 はフォルダから選んだスクリプトファイル。
        // auto_run=1 は Enter で即実行、0 は挿入のみ（git commit -m "" 等の編集用）。
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS command_alias (
                alias TEXT PRIMARY KEY,
                expansion TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                is_script INTEGER NOT NULL DEFAULT 0,
                auto_run INTEGER NOT NULL DEFAULT 1
            )",
            [],
        )?;
        // 既存DBへの列追加（無ければ足す。あればエラーを無視）
        let _ = self.conn.execute(
            "ALTER TABLE command_alias ADD COLUMN auto_run INTEGER NOT NULL DEFAULT 1",
            [],
        );

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

    /// 誤学習をリセットする: 指定した (reading, surface) の確定履歴を削除する。
    ///
    /// 候補一覧で Delete したとき、その変換の学習だけを消すために使う。読み全体
    /// ではなく (reading, surface) の組だけを消すので、同じ読みの他の正しい学習は
    /// 残る（app_name はまたいで全て消す）。あわせて、その表記が絡むバイグラム
    /// （前後どちらでも）も消して誤変換の波及を断つ。履歴を消した行があれば true。
    pub fn forget_commit(&self, reading: &str, surface: &str) -> Result<bool> {
        if reading.is_empty() || surface.is_empty() {
            return Ok(false);
        }
        let n = self.conn.execute(
            "DELETE FROM conversion_history WHERE reading = ?1 AND surface = ?2",
            params![reading, surface],
        )?;
        // この表記が絡むバイグラム（prev/next どちらでも）も消す
        let _ = self.conn.execute(
            "DELETE FROM word_bigram WHERE prev_surface = ?1 OR surface = ?1",
            params![surface],
        );
        Ok(n > 0)
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

    // ============ 予測変換 ============

    /// 前方一致予測: 読みが prefix で始まる確定履歴を頻度順に返す
    ///
    /// 「かい」→ 会議/会社/開発 のように、打った読みで始まる過去に確定した
    /// 語を候補にする。prefix と完全一致する語（＝補完にならない）と、
    /// ひらがなそのままの語は除く。
    pub fn predict_by_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<(String, String, u32)>> {
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        // LIKE のワイルドカードをエスケープ
        let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{}%", escaped);
        let mut stmt = self.conn.prepare(
            "SELECT reading, surface, MAX(frequency) as f FROM conversion_history
             WHERE reading LIKE ?1 ESCAPE '\\' AND reading <> ?2 AND surface <> reading
             GROUP BY surface
             ORDER BY f DESC, length(reading) ASC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![pattern, prefix, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, u32>(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 次単語予測: prev_surface の直後に確定された表記を頻度順に返す
    pub fn predict_next(&self, prev_surface: &str, limit: usize) -> Result<Vec<(String, u32)>> {
        if prev_surface.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT surface, frequency FROM word_bigram
             WHERE prev_surface = ?1
             ORDER BY frequency DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![prev_surface, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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

    // ============ コマンド履歴（コマンドモード） ============

    /// 実行したコマンドを記録する（頻度+1）。既存の説明は保持する。
    /// 短すぎる/空のコマンドは無視する（ノイズ防止）。
    pub fn record_command(&self, command: &str) -> Result<()> {
        let cmd = command.trim();
        if cmd.chars().count() < 2 {
            return Ok(());
        }
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO command_history (command, frequency, description, last_used_at)
             VALUES (?1, 1, '', ?2)
             ON CONFLICT(command) DO UPDATE SET
                frequency = frequency + 1,
                last_used_at = ?2",
            params![cmd, now],
        )?;
        Ok(())
    }

    /// commands.csv 由来の定番コマンドと説明を登録する（頻度は増やさない）。
    /// 既に履歴にある場合は説明だけ更新し、頻度はそのまま。
    pub fn seed_command(&self, command: &str, description: &str) -> Result<()> {
        let cmd = command.trim();
        if cmd.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO command_history (command, frequency, description, last_used_at)
             VALUES (?1, 0, ?2, '')
             ON CONFLICT(command) DO UPDATE SET description = ?2",
            params![cmd, description.trim()],
        )?;
        Ok(())
    }

    /// 前方一致でコマンド候補を返す（頻度順、次に短い順）。
    /// 戻り値: (command, description, frequency)。
    /// prefix と完全一致する行は補完にならないため除外する。
    pub fn predict_command(&self, prefix: &str, limit: usize) -> Result<Vec<(String, String, u32)>> {
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{}%", escaped);
        let mut stmt = self.conn.prepare(
            "SELECT command, description, frequency FROM command_history
             WHERE command LIKE ?1 ESCAPE '\\' AND command <> ?2
             ORDER BY frequency DESC, length(command) ASC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![pattern, prefix, limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 登録済みコマンドを全件返す（command, description, frequency）。設定画面用。
    /// 作成順（rowid 昇順）で返す。
    pub fn all_commands(&self) -> Result<Vec<(String, String, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT command, description, frequency FROM command_history
             ORDER BY rowid ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// コマンドの説明を登録/更新する（無ければ freq0 で作成、有れば説明のみ更新）。
    pub fn upsert_command(&self, command: &str, description: &str) -> Result<()> {
        let c = command.trim();
        if c.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO command_history (command, frequency, description, last_used_at)
             VALUES (?1, 0, ?2, '')
             ON CONFLICT(command) DO UPDATE SET description = ?2",
            params![c, description.trim()],
        )?;
        Ok(())
    }

    /// コマンドを削除する。
    pub fn delete_command(&self, command: &str) -> Result<bool> {
        let rows = self.conn.execute(
            "DELETE FROM command_history WHERE command = ?1",
            params![command.trim()],
        )?;
        Ok(rows > 0)
    }

    // ============ エイリアス（コマンドモード） ============

    /// エイリアスを登録/更新する。alias を打つと expansion が挿入される。
    /// auto_run=true は Enter で即実行、false は挿入のみ（編集してから実行）。
    pub fn add_alias(
        &self,
        alias: &str,
        expansion: &str,
        description: &str,
        is_script: bool,
        auto_run: bool,
    ) -> Result<()> {
        let a = alias.trim();
        let e = expansion.trim();
        if a.is_empty() || e.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO command_alias (alias, expansion, description, is_script, auto_run)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(alias) DO UPDATE SET
                expansion = ?2, description = ?3, is_script = ?4, auto_run = ?5",
            params![a, e, description.trim(), is_script as i64, auto_run as i64],
        )?;
        Ok(())
    }

    /// エイリアスの auto_run（Enterで即実行するか）だけを更新する。
    pub fn set_alias_auto_run(&self, alias: &str, auto_run: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE command_alias SET auto_run = ?2 WHERE alias = ?1",
            params![alias.trim(), auto_run as i64],
        )?;
        Ok(())
    }

    /// エイリアスを削除する。
    pub fn delete_alias(&self, alias: &str) -> Result<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM command_alias WHERE alias = ?1", params![alias.trim()])?;
        Ok(rows > 0)
    }

    /// 全エイリアスを返す（alias, expansion, description, is_script, auto_run）。設定画面用。
    /// 作成順（rowid 昇順）で返す。
    pub fn all_aliases(&self) -> Result<Vec<(String, String, String, bool, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT alias, expansion, description, is_script, auto_run
             FROM command_alias ORDER BY rowid ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get::<_, i64>(4)? != 0,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 前方一致でエイリアスを返す（alias, expansion, description, auto_run）。候補用。
    pub fn predict_alias(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String, bool)>> {
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let escaped = prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("{}%", escaped);
        let mut stmt = self.conn.prepare(
            "SELECT alias, expansion, description, auto_run FROM command_alias
             WHERE alias LIKE ?1 ESCAPE '\\'
             ORDER BY length(alias) ASC, alias ASC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)? != 0,
                ))
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
    fn test_alias_roundtrip() -> Result<()> {
        let repo = LearningRepository::in_memory()?;
        repo.add_alias("gs", "git status", "変更状況", false, true)?;
        repo.add_alias("gc", "git commit -m \"\"", "コミット", false, false)?;
        // 前方一致で出る
        let hits = repo.predict_alias("gs", 5)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "gs");
        assert_eq!(hits[0].1, "git status");
        assert!(hits[0].3, "gs は即実行");
        // 編集用は auto_run=false
        let hits = repo.predict_alias("gc", 5)?;
        assert!(!hits[0].3, "gc は挿入のみ");
        // 更新（同じ alias）
        repo.add_alias("gs", "git switch", "ブランチ切替", false, true)?;
        let hits = repo.predict_alias("gs", 5)?;
        assert_eq!(hits[0].1, "git switch");
        // 一覧
        assert_eq!(repo.all_aliases()?.len(), 2);
        // 削除
        assert!(repo.delete_alias("gs")?);
        assert!(repo.predict_alias("gs", 5)?.is_empty());
        Ok(())
    }

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
    fn test_forget_commit_resets_learning() -> Result<()> {
        let repo = LearningRepository::in_memory()?;
        // 同じ読みに2つの表記を学習（「けん」→県/件）
        for _ in 0..5 { repo.record_commit("けん", "県", None)?; }
        repo.record_commit("けん", "件", None)?;
        assert_eq!(repo.find_frequency("けん", "県")?, 5);
        assert_eq!(repo.find_frequency("けん", "件")?, 1);

        // 誤学習「県」だけをリセット
        let removed = repo.forget_commit("けん", "県")?;
        assert!(removed);
        assert_eq!(repo.find_frequency("けん", "県")?, 0); // 消えた
        assert_eq!(repo.find_frequency("けん", "件")?, 1); // 他は残る

        // 存在しない組は false
        assert!(!repo.forget_commit("けん", "県")?);
        Ok(())
    }

    #[test]
    fn test_predict_by_prefix() -> Result<()> {
        let repo = LearningRepository::in_memory()?;
        for _ in 0..3 { repo.record_commit("かいぎ", "会議", None)?; }
        for _ in 0..5 { repo.record_commit("かいしゃ", "会社", None)?; }
        repo.record_commit("かいはつ", "開発", None)?;
        // 「かい」で前方一致 → 頻度順（会社5 > 会議3 > 開発1）
        let r = repo.predict_by_prefix("かい", 10)?;
        let surfaces: Vec<&str> = r.iter().map(|(_, s, _)| s.as_str()).collect();
        assert_eq!(surfaces, vec!["会社", "会議", "開発"]);
        // 完全一致（補完にならない）は除外
        let r2 = repo.predict_by_prefix("かいぎ", 10)?;
        assert!(r2.iter().all(|(rd, _, _)| rd != "かいぎ"));
        Ok(())
    }

    #[test]
    fn test_predict_next() -> Result<()> {
        let repo = LearningRepository::in_memory()?;
        for _ in 0..4 { repo.record_bigram("会議", "資料")?; }
        repo.record_bigram("会議", "室")?;
        let r = repo.predict_next("会議", 10)?;
        assert_eq!(r.first().map(|(s, _)| s.as_str()), Some("資料")); // 頻度順で先頭
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
