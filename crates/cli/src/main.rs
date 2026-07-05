//! IME 変換エンジン検証用 CLI
//!
//! 要件定義書 13章 MVP・21章「次に着手するタスク」に基づく実装。
//! 標準入力からひらがな/ローマ字を受け取り、N-best変換結果と
//! 誤字補正・自動変換判定の結果を表示する。

use anyhow::{Context, Result};
use common::{
    katakana_to_hiragana, should_auto_convert, Candidate, CandidateKind, Dictionary,
    LearningRepository, LiveConverter, RomajiConverter, TypoCorrector, ViterbiConverter,
};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

struct Cli {
    converter: LiveConverter,
    romaji: RomajiConverter,
    typo: TypoCorrector,
    viterbi: Option<ViterbiConverter>,
    /// 直近の変換候補（:commit N で参照）
    last_candidates: Vec<Candidate>,
    /// 直近の入力ひらがな
    last_reading: String,
    /// 学習DBのパス（履歴記録のため保持）
    learning_db_path: Option<PathBuf>,
}

impl Cli {
    fn new() -> Self {
        Self {
            converter: LiveConverter::new(),
            romaji: RomajiConverter::new(),
            typo: TypoCorrector::new(),
            viterbi: None,
            last_candidates: Vec::new(),
            last_reading: String::new(),
            learning_db_path: None,
        }
    }

    fn load_dictionary(&mut self, path: &Path) -> Result<()> {
        let dict = Dictionary::load(path)
            .with_context(|| format!("辞書のロードに失敗: {}", path.display()))?;
        // LiveConverter と独立の ViterbiConverter の両方にロード
        self.viterbi = Some(ViterbiConverter::new(Dictionary::load(path)?));
        self.converter.set_dictionary(dict);
        Ok(())
    }

    fn load_learning(&mut self, path: &Path) -> Result<()> {
        let learning = LearningRepository::open(path)
            .with_context(|| format!("学習DBのオープンに失敗: {}", path.display()))?;
        self.learning_db_path = Some(path.to_path_buf());
        self.converter.set_learning(learning);
        Ok(())
    }

    /// 入力文字列をひらがなに正規化
    fn normalize_input(&self, input: &str) -> String {
        // ASCII のみならローマ字としてひらがな化、それ以外はカタカナ→ひらがな変換
        if input.chars().all(|c| c.is_ascii()) {
            self.romaji.convert(input)
        } else {
            katakana_to_hiragana(input)
        }
    }

    fn convert(&mut self, input: &str) -> Result<()> {
        let hiragana = self.normalize_input(input);
        if hiragana.is_empty() {
            return Ok(());
        }

        self.last_reading = hiragana.clone();
        self.last_candidates = self.converter.generate_candidates(&hiragana);

        // 最も目立つ表示: 入力 → 変換結果
        let best = self
            .last_candidates
            .first()
            .map(|c| c.text.as_str())
            .unwrap_or(hiragana.as_str());

        if hiragana == input {
            println!("  {}  →  {}", input, best);
        } else {
            println!("  {}  →  {}  →  {}", input, hiragana, best);
        }

        if self.viterbi.is_none() {
            println!("  (辞書未ロード: 漢字変換は無効、カタカナ/ひらがなのみ)");
        }

        if self.last_candidates.is_empty() {
            println!("  (候補なし)");
            return Ok(());
        }

        println!("  候補:");
        for (i, c) in self.last_candidates.iter().enumerate().take(10) {
            println!(
                "    {}. {}  [{} score={:.1}]",
                i + 1,
                c.text,
                kind_label(&c.kind),
                c.score
            );
        }

        if should_auto_convert(&hiragana, 0) {
            println!("  (自動仮変換タイミング: 即時)");
        }
        Ok(())
    }

    fn commit(&mut self, n: usize) -> Result<()> {
        if n == 0 || n > self.last_candidates.len() {
            println!("候補番号が範囲外: {}", n);
            return Ok(());
        }
        let candidate = &self.last_candidates[n - 1];
        self.converter
            .record_commit(&self.last_reading, &candidate.text);
        println!(
            "確定: {} ({} → {})",
            candidate.text, self.last_reading, candidate.text
        );
        Ok(())
    }

    fn user_add(&mut self, reading: &str, surface: &str) -> Result<()> {
        match self.converter.add_user_word(reading, surface, None) {
            Ok(()) => println!("ユーザー辞書に登録: {} → {}", reading, surface),
            Err(e) => println!("登録失敗: {} (:learning <path> で学習DBをロードしてください)", e),
        }
        Ok(())
    }

    fn user_list(&self) -> Result<()> {
        if let Some(path) = &self.learning_db_path {
            let learning = LearningRepository::open(path)?;
            let entries = learning.get_all_user_words()?;
            if entries.is_empty() {
                println!("(ユーザー辞書は空です)");
            } else {
                println!("ユーザー辞書 ({}件):", entries.len());
                for e in entries {
                    println!("  {} → {}", e.reading, e.surface);
                }
            }
        } else {
            println!("学習DBが未ロード。:learning <path> でロードしてください。");
        }
        Ok(())
    }

    fn user_del(&self, reading: &str, surface: &str) -> Result<()> {
        if let Some(path) = &self.learning_db_path {
            let learning = LearningRepository::open(path)?;
            let removed = learning.remove_user_word(reading, surface)?;
            if removed {
                println!("削除: {} → {}", reading, surface);
            } else {
                println!("該当なし: {} → {}", reading, surface);
            }
        } else {
            println!("学習DBが未ロード");
        }
        Ok(())
    }

    fn show_typo(&self, input: &str) {
        let hiragana = self.normalize_input(input);
        let candidates = self.typo.correct(&hiragana);
        if candidates.is_empty() {
            println!("(誤字補正候補なし)");
            return;
        }
        println!("誤字補正候補 (上位5件):");
        for c in candidates.iter().take(5) {
            println!("  {} → {} (信頼度 {:.2})", c.original, c.corrected, c.confidence);
        }
    }

    fn show_auto(&self, input: &str) {
        let hiragana = self.normalize_input(input);
        for ms in [0u64, 100, 250, 500] {
            let result = should_auto_convert(&hiragana, ms);
            println!("  経過 {:>4}ms: {}", ms, if result { "変換実行" } else { "待機" });
        }
    }

    fn show_nbest(&self, input: &str, n: usize) {
        let hiragana = self.normalize_input(input);
        let Some(v) = &self.viterbi else {
            println!("辞書未ロード。:dict <path> でロードしてください。");
            return;
        };
        let results = v.n_best_strings(&hiragana, n);
        if results.is_empty() {
            println!("(候補なし)");
            return;
        }
        println!("Viterbi N-best ({}件):", results.len());
        for (i, s) in results.iter().enumerate() {
            println!("  {}. {}", i + 1, s);
        }
    }
}

/// ライブ変換モード
///
/// キー単位で入力を受け付け、macOSのライブ変換のように
/// 入力停止（250ms）や句読点・文節境界で自動的に仮変換する。
///
/// キー操作（要件 7.9）:
/// - a-z 等: ローマ字入力
/// - Space: 仮変換の実行 / 次候補
/// - Shift+Space: 前候補
/// - Enter: 確定（学習に記録）
/// - Esc: 仮変換をキャンセルしてひらがな表示に戻す（もう一度で入力破棄）
/// - Backspace: 1文字削除
/// - Ctrl+C: ライブモード終了
fn live_mode(cli: &mut Cli) -> Result<()> {
    use crossterm::terminal;

    println!();
    println!("=== ライブ変換モード ===");
    println!("そのままローマ字で入力してください。入力を止めると自動で仮変換されます。");
    println!("Space:次候補  Shift+Space:前候補  Enter:確定  Esc:かなに戻す  Ctrl+C:終了");
    println!();

    cli.converter.clear();
    terminal::enable_raw_mode()?;
    let result = live_loop(cli);
    terminal::disable_raw_mode()?;
    println!();
    result
}

fn live_loop(cli: &mut Cli) -> Result<()> {
    use crossterm::cursor::MoveToColumn;
    use crossterm::event::{poll, read, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::execute;
    use crossterm::terminal::{Clear, ClearType};
    use std::io::Write;
    use std::time::Duration;

    let mut stdout = io::stdout();
    // この行に確定済みテキストを積んでいく
    let mut committed = String::new();
    // 仮変換表示中かどうか（false: ひらがな表示中）
    let mut showing_conversion = false;
    // 前回描画した行（点滅防止のため、変化したときだけ再描画する）
    let mut last_render = String::new();

    loop {
        // 表示を更新
        let composing_text = if showing_conversion {
            cli.converter.get_display_text().to_string()
        } else {
            cli.converter.get_hiragana_buffer()
        };
        let position = if showing_conversion {
            cli.converter
                .candidate_position()
                .map(|(i, n)| format!(" [{}/{}]", i + 1, n))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let marker = if showing_conversion { "◆" } else { "◇" };
        let render = format!("{}{}{}{}", committed, marker, composing_text, position);
        if render != last_render {
            execute!(stdout, Clear(ClearType::CurrentLine), MoveToColumn(0))?;
            write!(stdout, "{}", render)?;
            stdout.flush()?;
            last_render = render;
        }

        // キー入力待ち（30ms でタイムアウトして自動変換判定）
        if !poll(Duration::from_millis(30))? {
            // 入力停止・文節境界の判定（要件 7.4）
            if !showing_conversion
                && cli.converter.is_composing()
                && cli.converter.should_auto_convert()
            {
                showing_conversion = true;
            }
            continue;
        }

        let Event::Key(key) = read()? else { continue };
        // Windowsでは Press/Release 両方が来るので Press のみ処理
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // 終了（未確定分は破棄）
                break;
            }
            KeyCode::Char(' ') => {
                if cli.converter.is_composing() {
                    if !showing_conversion {
                        showing_conversion = true;
                    } else if key.modifiers.contains(KeyModifiers::SHIFT) {
                        cli.converter.prev_candidate();
                    } else {
                        cli.converter.next_candidate();
                    }
                } else {
                    committed.push(' ');
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                cli.converter.input_romaji(c);
                // 句読点などは即時に仮変換、それ以外は入力停止を待つ
                showing_conversion = cli.converter.should_auto_convert();
            }
            KeyCode::Enter => {
                if cli.converter.is_composing() {
                    // 仮変換中はその表示を、ひらがな表示中はひらがなを確定
                    let text = if showing_conversion {
                        cli.converter.commit()
                    } else {
                        cli.converter.cancel()
                    };
                    committed.push_str(&text);
                    showing_conversion = false;
                } else if !committed.is_empty() {
                    // 行を確定して次の行へ
                    execute!(stdout, Clear(ClearType::CurrentLine), MoveToColumn(0))?;
                    write!(stdout, "{}\r\n", committed)?;
                    stdout.flush()?;
                    committed.clear();
                }
            }
            KeyCode::Esc => {
                if showing_conversion {
                    // 仮変換をキャンセルしてひらがな表示に戻す
                    showing_conversion = false;
                } else if cli.converter.is_composing() {
                    // ひらがな表示中の Esc は入力自体を破棄
                    cli.converter.clear();
                } else {
                    break;
                }
            }
            KeyCode::Backspace => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    cli.converter.clear();
                } else if cli.converter.is_composing() {
                    cli.converter.backspace();
                    showing_conversion = false;
                } else {
                    committed.pop();
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn kind_label(kind: &CandidateKind) -> &'static str {
    match kind {
        CandidateKind::KanjiConversion => "漢字",
        CandidateKind::KatakanaConversion => "カナ",
        CandidateKind::Prediction => "予測",
        CandidateKind::TypoCorrection => "補正",
        CandidateKind::UserDictionary => "ユ辞",
        CandidateKind::RawKana => "かな",
    }
}

fn print_help() {
    println!();
    println!("=== IME 変換エンジン CLI ===");
    println!("入力をそのまま打つと変換候補を表示します。");
    println!("ASCIIはローマ字、それ以外はひらがな/カタカナとして扱います。");
    println!();
    println!("コマンド:");
    println!("  :live                       ライブ変換モード（自動仮変換を体験）");
    println!("  :help                       このヘルプ");
    println!("  :quit / :exit               終了");
    println!("  :dict <path>                辞書(.dic)をロード");
    println!("  :learning <path>            学習DB(SQLite)をオープン");
    println!("  :commit <N>                 直近のN番目の候補を確定して履歴に記録");
    println!("  :user add <読み> <表記>     ユーザー辞書に登録");
    println!("  :user list                  ユーザー辞書を一覧");
    println!("  :user del <読み> <表記>     ユーザー辞書から削除");
    println!("  :history clear              履歴をクリア");
    println!("  :typo <入力>                誤字補正候補のみ表示");
    println!("  :auto <入力>                自動変換タイミング判定");
    println!("  :nbest <入力> [N]           Viterbi N-best 結果のみ表示");
    println!();
}

fn run() -> Result<()> {
    let mut cli = Cli::new();
    let args: Vec<String> = std::env::args().collect();

    // 既定の辞書ロードを試みる（実行ファイル位置・cwd の両方から探索）
    // フル辞書 system.dic を優先し、なければ sample.dic にフォールバック
    let mut search_paths: Vec<PathBuf> = vec![
        PathBuf::from("dictionaries/system.dic"),
        PathBuf::from("dictionaries/sample.dic"),
        PathBuf::from("../dictionaries/system.dic"),
        PathBuf::from("../dictionaries/sample.dic"),
        PathBuf::from("../../dictionaries/system.dic"),
        PathBuf::from("../../dictionaries/sample.dic"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["system.dic", "sample.dic"] {
                search_paths.push(dir.join("dictionaries").join(name));
                search_paths.push(dir.join("../../dictionaries").join(name));
                search_paths.push(dir.join("../../../dictionaries").join(name));
            }
        }
    }

    let mut loaded_path: Option<PathBuf> = None;
    for path in &search_paths {
        if path.exists() {
            match cli.load_dictionary(path) {
                Ok(()) => {
                    loaded_path = Some(path.clone());
                    break;
                }
                Err(e) => eprintln!("辞書ロード試行失敗 ({}): {}", path.display(), e),
            }
        }
    }
    if let Some(p) = &loaded_path {
        println!("既定辞書をロード: {}", p.display());
    } else {
        eprintln!();
        eprintln!("⚠️  辞書が見つかりません (sample.dic / system.dic)。");
        eprintln!("    検索したパス:");
        for p in &search_paths {
            eprintln!("      - {}", p.display());
        }
        eprintln!("    現在の作業ディレクトリ: {}",
            std::env::current_dir().map(|p| p.display().to_string())
                .unwrap_or_else(|_| "(取得失敗)".into()));
        eprintln!("    回避策: `:dict <path>` でロード、または `--dict <path>` 引数で起動。");
        eprintln!();
    }

    // 引数で辞書指定があれば上書き
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--dict" | "-d" => {
                if let Some(p) = args.get(i + 1) {
                    cli.load_dictionary(Path::new(p))?;
                    println!("辞書をロード: {}", p);
                    i += 2;
                    continue;
                }
            }
            "--learning" | "-l" => {
                if let Some(p) = args.get(i + 1) {
                    cli.load_learning(Path::new(p))?;
                    println!("学習DBをロード: {}", p);
                    i += 2;
                    continue;
                }
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => {}
        }
        i += 1;
    }

    // 学習DBが未指定なら既定パスを自動オープン
    // （ユーザー辞書・変換履歴を :user add / :commit で使えるようにする）
    if cli.learning_db_path.is_none() {
        let default_db = PathBuf::from("ime-learning.db");
        match cli.load_learning(&default_db) {
            Ok(()) => println!("既定の学習DBをオープン: {}", default_db.display()),
            Err(e) => eprintln!("学習DBの自動オープンに失敗: {}", e),
        }
    }

    print_help();

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    print!("> ");
    stdout.flush().ok();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("入力エラー: {}", e);
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            print!("> ");
            stdout.flush().ok();
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix(':') {
            if !handle_command(&mut cli, rest)? {
                break;
            }
        } else {
            if let Err(e) = cli.convert(trimmed) {
                eprintln!("変換エラー: {}", e);
            }
        }

        print!("> ");
        stdout.flush().ok();
    }

    Ok(())
}

/// コマンドを処理。false を返したら終了
fn handle_command(cli: &mut Cli, cmd: &str) -> Result<bool> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(true);
    }

    match parts[0] {
        "help" | "h" => print_help(),
        "quit" | "exit" | "q" => return Ok(false),
        "dict" => {
            if let Some(p) = parts.get(1) {
                cli.load_dictionary(Path::new(p))?;
                println!("辞書をロード: {}", p);
            } else {
                println!("使い方: :dict <path>");
            }
        }
        "learning" => {
            if let Some(p) = parts.get(1) {
                cli.load_learning(Path::new(p))?;
                println!("学習DBをオープン: {}", p);
            } else {
                println!("使い方: :learning <path>");
            }
        }
        "commit" => {
            if let Some(n) = parts.get(1).and_then(|s| s.parse::<usize>().ok()) {
                cli.commit(n)?;
            } else {
                println!("使い方: :commit <候補番号>");
            }
        }
        "user" => {
            match parts.get(1).copied() {
                Some("add") => {
                    if let (Some(r), Some(s)) = (parts.get(2), parts.get(3)) {
                        cli.user_add(r, s)?;
                    } else {
                        println!("使い方: :user add <読み> <表記>");
                    }
                }
                Some("list") => cli.user_list()?,
                Some("del") => {
                    if let (Some(r), Some(s)) = (parts.get(2), parts.get(3)) {
                        cli.user_del(r, s)?;
                    } else {
                        println!("使い方: :user del <読み> <表記>");
                    }
                }
                _ => println!("使い方: :user add|list|del ..."),
            }
        }
        "history" => {
            if parts.get(1).copied() == Some("clear") {
                if let Some(p) = &cli.learning_db_path {
                    let l = LearningRepository::open(p)?;
                    l.clear_history()?;
                    println!("履歴をクリアしました");
                } else {
                    println!("学習DBが未ロード");
                }
            } else {
                println!("使い方: :history clear");
            }
        }
        "typo" => {
            if let Some(input) = parts.get(1) {
                cli.show_typo(input);
            } else {
                println!("使い方: :typo <入力>");
            }
        }
        "auto" => {
            if let Some(input) = parts.get(1) {
                cli.show_auto(input);
            } else {
                println!("使い方: :auto <入力>");
            }
        }
        "live" => {
            live_mode(cli)?;
        }
        "nbest" => {
            if let Some(input) = parts.get(1) {
                let n = parts.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(5);
                cli.show_nbest(input, n);
            } else {
                println!("使い方: :nbest <入力> [N]");
            }
        }
        other => println!("不明なコマンド: {} (:help でヘルプ)", other),
    }

    Ok(true)
}

fn main() -> Result<()> {
    run()
}
