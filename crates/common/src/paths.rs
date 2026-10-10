//! ファイルの置き場所（docs/release-and-versioning.md「4. ファイルの置き場所」）。
//!
//! - プログラムと同梱データ（辞書・モデル）: exe の隣。開発時（リポジトリ内で
//!   `cargo run` / `target\release\*.exe` を起動）はカレントディレクトリにも
//!   フォールバックする。
//! - ユーザーデータ（学習DB・ログ）: `%LOCALAPPDATA%\IMELiveConverter\`。
//!   更新でプログラムを置き換えても消えないよう、インストール先とは分ける。
//!   環境変数 `IME_DATA_DIR` で上書きできる（検証・開発用）。
use std::path::{Path, PathBuf};

/// ユーザーデータのフォルダ名（`%LOCALAPPDATA%` 直下）
pub const APP_DIR_NAME: &str = "IMELiveConverter";
/// 学習DBのファイル名
pub const LEARNING_DB_FILE: &str = "ime-learning.db";

/// 実行中の exe のあるフォルダ（DLL から呼んでも、DLL を読み込んだ exe のフォルダ）
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// 同梱データのファイル（`dictionaries\system.dic` 等、exe からの相対パス）を探す。
/// exe の隣 → カレントディレクトリの順で、最初に存在したものを返す。
pub fn find_bundled(relative: &str) -> Option<PathBuf> {
    exe_dir()
        .into_iter()
        .chain(std::env::current_dir().ok())
        .map(|dir| dir.join(relative))
        .find(|p| p.exists())
}

/// ユーザーデータのフォルダ（無ければ作る）。作れなければカレントディレクトリ。
pub fn data_dir() -> PathBuf {
    let dir = std::env::var_os("IME_DATA_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(APP_DIR_NAME)));
    match dir {
        Some(dir) if std::fs::create_dir_all(&dir).is_ok() => dir,
        _ => PathBuf::from("."),
    }
}

/// ユーザーデータのフォルダ内のファイル（ログ等）
pub fn data_file(name: &str) -> PathBuf {
    data_dir().join(name)
}

/// 学習DBのパス
pub fn learning_db_path() -> PathBuf {
    data_file(LEARNING_DB_FILE)
}

/// 学習DBの移行元の候補（1.0.0 より前は exe の起動フォルダに置いていた）
pub fn legacy_learning_db_candidates() -> Vec<PathBuf> {
    std::env::current_dir()
        .ok()
        .into_iter()
        .chain(exe_dir())
        .map(|dir| dir.join(LEARNING_DB_FILE))
        .collect()
}
