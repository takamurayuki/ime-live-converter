//! IME 変換エンジン検証用 CLI
//!
//! 要件定義書 13章 MVP・21章「次に着手するタスク」に基づく実装。
//! 標準入力からひらがな/ローマ字を受け取り、N-best変換結果と
//! 誤字補正・自動変換判定の結果を表示する。
//!
//! 変換本体は`common::LiveConversionState`（hook-dllの実運用エンジンそのもの、
//! `common`へ移設済み）を直接駆動する。以前は独立の`LiveConverter`を使って
//! いたが、`n_best`経路のみでfragment-repair post-passや学習連想リランクを
//! 通らず、CLIでの手動確認が実機の変換結果と食い違う既知のリスクがあった
//! （[[golden-test-harness]]）。これを解消するため、CLIも実機と同じ
//! `LiveConversionState::add_char`/`commit`を使う。

use anyhow::{Context, Result};
use common::{
    katakana_to_hiragana, should_auto_convert, ConversionAction, Dictionary, LearningRepository,
    LiveConversionState, RomajiConverter, TypoCorrector, ViterbiConverter,
};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

struct Cli {
    /// 実運用と同じ変換エンジン本体。
    converter: LiveConversionState,
    /// ひらがな/カタカナ入力の正規化用（`:nbest`/`:typo`/`:auto`はローマ字化を
    /// 経由せず直接この変換だけを使うため、`converter.romaji`とは独立に持つ）。
    romaji: RomajiConverter,
    typo: TypoCorrector,
    /// `:nbest`診断用に独立して持つViterbiConverter（`converter`内部のものとは
    /// 別インスタンス。以前からある設計をそのまま踏襲）。
    viterbi: Option<ViterbiConverter>,
    /// 直近の入力ひらがな（表示・:commit 用）
    last_reading: String,
    /// 学習DBのパス（履歴記録のため保持）
    learning_db_path: Option<PathBuf>,
}

impl Cli {
    fn new() -> Self {
        Self {
            converter: LiveConversionState::new(),
            romaji: RomajiConverter::new(),
            typo: TypoCorrector::new(),
            viterbi: None,
            last_reading: String::new(),
            learning_db_path: None,
        }
    }

    fn load_dictionary(&mut self, path: &Path) -> Result<()> {
        // `converter`と独立のViterbiConverterの両方にロードする（`:nbest`での
        // 直接診断用に後者を保持する。以前からある設計をそのまま踏襲）。
        let mut viterbi = ViterbiConverter::new(Dictionary::load(path)?);
        let priority_path = path.with_file_name("word_priority.tsv");
        match viterbi.load_word_priority_file(&priority_path) {
            Ok(count) if count > 0 => println!("優先語彙をロード: {}件", count),
            Ok(_) => {}
            Err(e) => println!("優先語彙のロードをスキップ: {}", e),
        }
        let corpus_lm_path = path.with_file_name("corpus_lm.dic");
        if corpus_lm_path.exists() {
            match common::CorpusLm::load(&corpus_lm_path) {
                Ok(lm) => {
                    println!(
                        "コーパスLMをロード: {} (unigram={}, bigram={})",
                        corpus_lm_path.display(),
                        lm.unigrams.len(),
                        lm.bigrams.len()
                    );
                    viterbi.load_corpus_lm(&lm);
                }
                Err(e) => println!("コーパスLMのロードに失敗: {}", e),
            }
        }
        self.viterbi = Some(viterbi);

        // `LiveConversionState::load_dictionary`は実運用（hook-dll）と全く
        // 同じ経路（word_priority/コーパスLMの読み込みも内部で行う）。
        if !self.converter.load_dictionary(path) {
            anyhow::bail!("辞書のロードに失敗: {}", path.display());
        }
        self.converter.inject_user_words();
        Ok(())
    }

    fn load_learning(&mut self, path: &Path) -> Result<()> {
        let learning = LearningRepository::open(path)
            .with_context(|| format!("学習DBのオープンに失敗: {}", path.display()))?;
        self.learning_db_path = Some(path.to_path_buf());
        self.converter.learning = Some(learning);
        self.converter.reload_learning_into_converter();
        self.converter.inject_user_words();
        Ok(())
    }

    /// 入力文字列をひらがなに正規化（`:nbest`/`:typo`/`:auto`専用。
    /// これらは`converter`を経由せず直接ViterbiConverter/TypoCorrectorを叩く
    /// 診断コマンドのため、ローマ字・カタカナのどちらで入力しても良い）。
    fn normalize_input(&self, input: &str) -> String {
        if input.chars().all(|c| c.is_ascii()) {
            self.romaji.convert(input)
        } else {
            katakana_to_hiragana(input)
        }
    }

    /// 入力（ローマ字）を1文字ずつ`converter`に流し込む。実際のキー入力と
    /// 同じ意味論（`crates/hook-dll/src/hook.rs`の文字キー処理、
    /// `crates/hook-dll/src/golden_tests.rs`と同一のパターン）。
    fn convert(&mut self, input: &str) -> Result<()> {
        // 前の行の未確定状態を持ち越さない。
        if self.converter.is_composing() {
            self.converter.cancel();
        }
        for ch in input.chars() {
            self.converter.add_char(ch);
        }
        self.last_reading = self.converter.hiragana_buffer.clone();

        // `candidates`はTab相当（`cycle_candidate`）を呼ぶまで空のまま
        // （バッファ変更のたびにクリアされる設計）。ここで明示的に開く。
        // 候補が1件しかない場合は何もせず空のままなので、その場合は
        // `conversion_result`（現在の単独変換結果）を使う。
        self.converter.cycle_candidate(false);

        let best = if !self.converter.candidates.is_empty() {
            self.converter.candidates[self.converter.candidate_index].clone()
        } else {
            self.converter.conversion_result.clone()
        };

        if self.last_reading == input {
            println!("  {}  →  {}", input, best);
        } else {
            println!("  {}  →  {}  →  {}", input, self.last_reading, best);
        }

        if self.converter.converter.is_none() {
            println!("  (辞書未ロード: 漢字変換は無効、カタカナ/ひらがなのみ)");
        }

        if self.converter.candidates.is_empty() {
            println!("  (他の候補なし)");
        } else {
            println!("  候補:");
            for (i, c) in self.converter.candidates.iter().enumerate().take(10) {
                println!("    {}. {}", i + 1, c);
            }
        }

        if should_auto_convert(&self.last_reading, 0) {
            println!("  (自動仮変換タイミング: 即時)");
        }
        Ok(())
    }

    /// 直近の`convert`が出した候補のN番目を選択して実際に確定する
    /// （`select_candidate`+`commit`。学習にも記録される、実運用と同じ経路）。
    /// 候補一覧が無い（1件しかない）場合は N=1 のときだけ現在の結果を確定する。
    fn commit(&mut self, n: usize) -> Result<()> {
        if self.converter.candidates.is_empty() {
            if n != 1 || self.converter.conversion_result.is_empty() {
                println!("候補番号が範囲外: {}", n);
                return Ok(());
            }
            let candidate = self.converter.conversion_result.clone();
            self.converter.commit();
            println!("確定: {} ({} → {})", candidate, self.last_reading, candidate);
            return Ok(());
        }
        if n == 0 || n > self.converter.candidates.len() {
            println!("候補番号が範囲外: {}", n);
            return Ok(());
        }
        let candidate = self.converter.candidates[n - 1].clone();
        self.converter.select_candidate(n - 1);
        self.converter.commit();
        println!("確定: {} ({} → {})", candidate, self.last_reading, candidate);
        Ok(())
    }

    fn user_add(&mut self, reading: &str, surface: &str) -> Result<()> {
        let Some(learning) = self.converter.learning.as_ref() else {
            println!("登録失敗: 学習DBが未設定です (:learning <path> で学習DBをロードしてください)");
            return Ok(());
        };
        learning.add_user_word(reading, surface, None, 50)?;
        self.converter.inject_user_words();
        println!("ユーザー辞書に登録: {} → {}", reading, surface);
        Ok(())
    }

    /// ここから先の学習DB操作は、パスから毎回`LearningRepository::open`で
    /// 開き直すのではなく、`converter.learning`（既に開いている接続）を直接
    /// 使う。以前はファイルパス経由での再オープンだった（WALモードなら
    /// 同じファイルへの書き込みは他接続からも見えるため動いてはいた）が、
    /// それだと既定が一時DB（インメモリ、パスを持たない）のときに動かない。
    fn user_list(&self) -> Result<()> {
        let Some(learning) = self.converter.learning.as_ref() else {
            println!("学習DBが未ロード。:learning <path> でロードしてください。");
            return Ok(());
        };
        let entries = learning.get_all_user_words()?;
        if entries.is_empty() {
            println!("(ユーザー辞書は空です)");
        } else {
            println!("ユーザー辞書 ({}件):", entries.len());
            for e in entries {
                println!("  {} → {}", e.reading, e.surface);
            }
        }
        Ok(())
    }

    fn user_del(&self, reading: &str, surface: &str) -> Result<()> {
        let Some(learning) = self.converter.learning.as_ref() else {
            println!("学習DBが未ロード");
            return Ok(());
        };
        let removed = learning.remove_user_word(reading, surface)?;
        if removed {
            println!("削除: {} → {}", reading, surface);
        } else {
            println!("該当なし: {} → {}", reading, surface);
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

    fn show_phrase(&self, reading: &str) {
        let Some(learning) = self.converter.learning.as_ref() else {
            println!("学習DB未ロード。:learning <path> でロードしてください。");
            return;
        };
        match learning.predict_phrase_tail(reading) {
            Ok(Some((surface, tail))) => println!("{} → {}{}", reading, surface, tail),
            Ok(None) => println!("(定型句の続き予測なし)"),
            Err(e) => println!("エラー: {}", e),
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

/// `ConversionAction{delete_count, insert_text}`を、実運用の
/// `hook.rs::execute_action`と同じ意味論（末尾`delete_count`文字を消してから
/// `insert_text`を追記）で文書に反映する。
fn apply_conversion_action(doc: &mut String, action: Option<ConversionAction>) {
    let Some(action) = action else { return };
    if action.delete_count > 0 {
        let keep = doc.chars().count().saturating_sub(action.delete_count);
        *doc = doc.chars().take(keep).collect();
    }
    doc.push_str(&action.insert_text);
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

    if cli.converter.is_composing() {
        cli.converter.cancel();
    }
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
    use std::time::{Duration, Instant};

    let mut stdout = io::stdout();
    // この行に確定済みテキストを積んでいく
    let mut committed = String::new();
    // 仮変換表示中かどうか（false: ひらがな表示中）
    let mut showing_conversion = false;
    // 前回描画した行（点滅防止のため、変化したときだけ再描画する）
    let mut last_render = String::new();
    // 直近の入力停止判定用（`LiveConversionState`自体はキー入力の実時刻を
    // 追わないため、実際のキー入力タイミングを追うのはCLI側の責務）。
    let mut last_input_at: Option<Instant> = None;

    // 現在のひらがな（確定済み+未確定ローマ字の仮変換）を組み立てる。
    // `hook.rs`はキー入力ごとにこれを毎回描画し直す代わりに実テキストへ
    // 差分送信するが、CLIは端末に直接文字列として描画するのでこの形で良い。
    let current_hiragana = |cli: &Cli| -> String {
        format!(
            "{}{}",
            cli.converter.hiragana_buffer,
            cli.converter.romaji.convert(&cli.converter.romaji_buffer)
        )
    };

    loop {
        // 表示を更新。`candidates`はTab/2回目以降のSpace（`cycle_candidate`）を
        // 呼ぶまで空のまま（バッファ変更のたびにクリアされる設計）なので、
        // 最初のSpaceを押した直後は`conversion_result`（現在の単独変換結果）
        // にフォールバックする。
        let composing_text = if showing_conversion {
            cli.converter
                .candidates
                .get(cli.converter.candidate_index)
                .cloned()
                .unwrap_or_else(|| cli.converter.conversion_result.clone())
        } else {
            current_hiragana(cli)
        };
        let position = if showing_conversion && !cli.converter.candidates.is_empty() {
            format!(
                " [{}/{}]",
                cli.converter.candidate_index + 1,
                cli.converter.candidates.len()
            )
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
            if !showing_conversion && cli.converter.is_composing() {
                let elapsed = last_input_at
                    .map(|t| t.elapsed().as_millis() as u64)
                    .unwrap_or(u64::MAX);
                if should_auto_convert(&current_hiragana(cli), elapsed) {
                    showing_conversion = true;
                }
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
                        cli.converter.cycle_candidate(true);
                    } else {
                        cli.converter.cycle_candidate(false);
                    }
                } else {
                    committed.push(' ');
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                cli.converter.add_char(c);
                last_input_at = Some(Instant::now());
                // 句読点などは即時に仮変換、それ以外は入力停止を待つ
                showing_conversion = should_auto_convert(&current_hiragana(cli), 0);
            }
            KeyCode::Enter => {
                if cli.converter.is_composing() {
                    // 仮変換中はその表示を、ひらがな表示中はひらがなを確定
                    let action = if showing_conversion {
                        cli.converter.commit()
                    } else {
                        cli.converter.cancel()
                    };
                    apply_conversion_action(&mut committed, action);
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
                    cli.converter.cancel();
                } else {
                    break;
                }
            }
            KeyCode::Backspace => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    cli.converter.cancel();
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

fn print_help() {
    println!();
    println!("=== IME 変換エンジン CLI ===");
    println!("入力をそのまま打つとローマ字として変換候補を表示します（実機と同じエンジン）。");
    println!();
    println!("コマンド:");
    println!("  :live                       ライブ変換モード（自動仮変換を体験）");
    println!("  :help                       このヘルプ");
    println!("  :quit / :exit               終了");
    println!("  :dict <path>                辞書(.dic)をロード");
    println!("  :learning <path>            学習DB(SQLite)をオープン");
    println!("  :commit <N>                 直近のN番目の候補を確定して学習に記録");
    println!("  :user add <読み> <表記>     ユーザー辞書に登録");
    println!("  :user list                  ユーザー辞書を一覧");
    println!("  :user del <読み> <表記>     ユーザー辞書から削除");
    println!("  :history clear              履歴をクリア");
    println!("  :typo <入力>                誤字補正候補のみ表示（ひらがな/カタカナ可）");
    println!("  :auto <入力>                自動変換タイミング判定（ひらがな/カタカナ可）");
    println!("  :nbest <入力> [N]           Viterbi N-best 結果のみ表示（ひらがな/カタカナ可）");
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

    // 学習DBが未指定なら、既定で一時DB（インメモリ）を使う。本番の
    // ime-learning.db（conversion-serviceが実際に使うファイル）へは
    // 決して自動で触らない。危険な方（本番ファイル）をデフォルトにすると、
    // 動作確認のつもりの:commit/:user addが本番の学習データを書き換えて
    // しまう（実際にこの事故が起きた）。本番DBを使いたい場合は
    // `--learning ime-learning.db`（または`:learning ime-learning.db`）で
    // 明示すること。
    if cli.learning_db_path.is_none() {
        match LearningRepository::in_memory() {
            Ok(learning) => {
                cli.converter.learning = Some(learning);
                println!(
                    "学習DB: 一時DB（インメモリ）を使用中。本番ime-learning.dbには触れません。\
                     本番DBを使うには --learning ime-learning.db (または :learning ime-learning.db) を指定してください。"
                );
            }
            Err(e) => eprintln!("一時学習DBの作成に失敗: {}", e),
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
                if let Some(learning) = cli.converter.learning.as_ref() {
                    learning.clear_history()?;
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
        "phrase" => {
            if let Some(reading) = parts.get(1) {
                cli.show_phrase(reading);
            } else {
                println!("使い方: :phrase <読み>");
            }
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
