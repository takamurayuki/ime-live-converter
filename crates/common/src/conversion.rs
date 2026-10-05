//! ライブ変換の状態と変換ロジック（ローマ字→かな→漢字、候補生成、学習ガード）

use crate::*;
use std::path::Path;

/// 長さkの母音（あいうえお）の全組み合わせを列挙する（例: k=2なら"あい","あう",…全25通り）。
/// 語尾の母音長音バリエーション探索に使う。
///
/// `hook-dll`の`command_mode.rs`にも同一の実装がある（コマンドモードの
/// 母音バリエーション探索用）。小さく自己完結した純粋関数のため、
/// `common`へ移設して共有クレート越しに依存させるより、この程度の
/// 重複の方がリスクが低いと判断しそのままにしている。
fn vowel_combos(k: usize) -> Vec<Vec<char>> {
    const V: [char; 5] = ['a', 'i', 'u', 'e', 'o'];
    let mut result: Vec<Vec<char>> = vec![Vec::new()];
    for _ in 0..k {
        let mut next = Vec::with_capacity(result.len() * 5);
        for prefix in &result {
            for &v in &V {
                let mut c = prefix.clone();
                c.push(v);
                next.push(c);
            }
        }
        result = next;
    }
    result
}

/// 変換候補（Tab/Space で巡回する同音異義語）の最大数。
/// 番号キーではなく Tab/Space で選ぶため 9 に縛る必要はない。ポップアップは
/// 画面に収まる分だけスクロール表示するので、多めに集めても破綻しない。
pub const MAX_CONVERSION_CANDIDATES: usize = 50;

/// ユーザー登録単語の品詞（名詞-一般）と接続ID・既定コスト。
/// extend_from_csv（補助辞書）と同じ扱いにして、単独でも文中でも変換に出るようにする。
pub const USER_WORD_POS: &str = "名詞-一般-*-*";
pub const USER_WORD_POS_ID: crate::PosId = 1285;
/// ユーザー登録語のコスト。低めにして、学習済みの別解（例「思い＋で」）にも
/// 勝てるようにする。さらに登録時に学習ボーナスも付与する（下記）。
pub const USER_WORD_COST: i16 = 2000;
/// ユーザー登録語に与える擬似学習頻度。frequency_to_bonus で上限(6000)の
/// ボーナスが付き、「ユーザーが明示登録した語」を最優先で選ばせる。
pub const USER_WORD_LEARN_FREQ: u32 = 20;

/// 隣接する漢字複合語を自動登録するために必要な最小バイグラム頻度
/// （同じ隣接ペアがこの回数だけ確定されたら辞書に単語として登録する）。
/// 1回で登録すると誤変換をそのまま固定してしまう恐れがあるため、
/// 学習系ボーナスと同様に反復を要求する。
pub const AUTO_COMPOUND_MIN_FREQ: u32 = 3;
/// 自動登録する複合語の読みの最大文字数（暴走的に長い結合を防ぐ）。
pub const AUTO_COMPOUND_MAX_READING_LEN: usize = 8;

/// 判断層（judge-lm）を使うか。環境変数 `IME_JUDGE=0` で無効化できる
/// （既定は有効。`judge_lm.bin` が無ければいずれにせよ使われない）。
pub fn judge_enabled() -> bool {
    std::env::var("IME_JUDGE").map(|v| v != "0").unwrap_or(true)
}

/// `commit_prediction` で予測（誤字修正・履歴補完）を確定した1回分の学習データ。
/// フックの外へ遅延させて処理できるよう、DB/コンバータへの参照を持たない
/// 素のデータとして保持する。
pub struct PredictionLearning {
    /// (読み, 表記) のユニグラム。学習可能な組のときだけ Some。
    unigram: Option<(String, String)>,
    /// (直前に確定した表記, 今回の表記) のバイグラム。直前確定があるときだけ Some。
    bigram: Option<(String, String)>,
    /// もしかして（誤字補正）採用時のみ: (誤字そのものの読み, 修正後の読み, 表記)
    typo_correction: Option<(String, String, String)>,
}

/// ライブ変換の状態
pub struct LiveConversionState {
    /// ローマ字→ひらがな変換
    pub romaji: RomajiConverter,
    /// ひらがな→漢字変換
    pub converter: Option<ViterbiConverter>,
    /// 判断層（judge-lm）をバックグラウンドで読み込むか。フック常駐プロセスでは
    /// true にする（大きなモデルの読み込みで `LIVE_CONTEXT` のロックを長く握り、
    /// その間のキー入力がフックの制限時間を超えるのを避ける）。読み込みが
    /// 終わるまでは従来の変換のまま動く。
    pub load_judge_async: bool,
    /// バックグラウンド読み込み中の判断層（`attach_pending_judge` で取り付ける）
    pending_judge: Option<std::sync::mpsc::Receiver<Option<std::sync::Arc<crate::judge::JudgeLm>>>>,
    /// 現在のローマ字入力バッファ
    pub romaji_buffer: String,
    /// 現在のひらがなバッファ
    pub hiragana_buffer: String,
    /// 現在の変換結果
    pub conversion_result: String,
    /// 前回送信した文字数
    pub last_sent_length: usize,
    /// 現在の変換候補（Tab/Spaceで切替。バッファ変更でクリア）
    pub candidates: Vec<String>,
    /// 選択中の候補インデックス
    pub candidate_index: usize,
    /// 候補一覧が対象とする文節（＝直近に打った最後の文節）の読み
    pub cand_seg_reading: String,
    /// 候補一覧の各項目に対応する対象文節の表記（candidates と並行）
    pub cand_seg_surfaces: Vec<String>,
    /// 対象文節より前（確定扱いにしない、変えない部分）の変換済み表記
    pub cand_prefix_surface: String,
    /// 対象文節より前の読み（学習時に前半を再分解するために保持）
    pub cand_prefix_reading: String,
    /// 対象文節より後ろ（末尾の平仮名など）の変換済み表記
    pub cand_suffix_surface: String,
    /// 対象文節より後ろの読み
    pub cand_suffix_reading: String,
    /// この合成で → により部分確定済みの文節列（読み, 表記, 品詞）
    /// 最終確定時にユニグラム/バイグラム/内容語連想の学習へ使う。
    pub committed_segments: Vec<(String, String, String)>,
    /// 直近に確定したテキスト（LLM変換へ渡す前後文脈。末尾数十文字を保持）
    pub recent_context: String,
    /// 固定した先頭側の文節列（`crate::viterbi::stabilize`）。
    /// 読みが長くなったら文節の切れ目で先頭側を固定し、以降の再変換では
    /// この分解を強制する。後続入力で離れた前方の語が書き換わる
    /// 「変換の崩れ」を防ぐ。表示・候補一覧・Backspace・学習のすべてが
    /// `convert_buffer` を通してこの分解を共有する。確定・取消で空になる。
    pub pinned: Vec<crate::WordEntry>,
    /// 確定時の学習（DB書き込み）をフックの外へ遅延させるか。
    /// hook-dll の実運用（`install_hook`）では true。確定はキーボードフック内で
    /// 行われるため、学習の記録はここに溜めておき、フックから戻った後に
    /// メッセージループ側（`WM_APP_FLUSH_LEARNING`）で `flush_pending_learning`
    /// が処理する。テストや遅延先が無い場合は従来どおり同期で学習する。
    pub defer_learning: bool,
    /// `defer_learning`時に、遅延先（候補ウィンドウのメッセージループ）へ
    /// 「溜めた学習がある」と通知するコールバック。`true`を返せば通知成功
    /// （後で`flush_pending_learning`が呼ばれる前提で良い）、`false`または
    /// `None`なら同期学習にフォールバックする。
    ///
    /// `common`crateはWin32非依存に保つため、実際の通知手段（`PostMessageW`
    /// 等）は持たない。hook-dllの実運用では`install_hook`が
    /// `popup::request_deferred_learning`をここへ差し込む。テストや
    /// CLI・ゴールデンテストでは`defer_learning`自体を`false`のまま使うため
    /// （`new()`の既定値）、この関数ポインタが参照されることはない。
    pub request_deferred_learning: Option<fn() -> bool>,
    /// 遅延させた学習の文節列（確定1回分ごと）
    pub pending_learning: Vec<Vec<(String, String, String)>>,
    /// 遅延させた予測確定（`commit_prediction`）1回分ごとの学習データ。
    /// `pending_learning`（文節列）とは形が異なる（前接語とのバイグラム・
    /// 誤字修正の学習を含む）ため別キューに分けている。
    pub pending_prediction_learning: Vec<PredictionLearning>,
    /// 閉じ括弧の自動対応に使う確定済みテキスト（末尾数百文字）。
    /// `recent_context` より長く保持し、セリフのように長い括弧内でも
    /// 対応する開き括弧（「『【（ 等）の種類を見つけられるようにする。
    pub bracket_context: String,
    /// 予測変換の候補（読み, 表記）。読みが空なら「次単語予測（追記）」。
    /// 打鍵中は前方一致補完、確定直後は次単語予測を入れる。番号キーで選ぶ。
    pub predictions: Vec<(String, String)>,
    /// predictions[0] が「もしかして（誤字補正）」かどうか。表示ラベル用。
    pub prediction_top_is_fuzzy: bool,
    /// prediction_top_is_fuzzy が true のとき、その場で計算した誤字補正では
    /// なく、過去にユーザーが実際に確定した学習済みの誤字修正かどうか。
    /// Deleteキーでの「誤学習リセット」対象の判定に使う（その場計算のもの
    /// には消す学習が無い）。
    pub prediction_top_is_learned_typo: bool,
    /// 予測リスト内で選択中の位置（↑↓で移動。Enterでこれを確定）。
    pub prediction_index: usize,
    /// 直近に確定した表記（次単語予測・バイグラム記録に使う）
    pub last_committed: String,
    /// Escで「かなに戻した」末尾の読み文字数。update_conversion は末尾の
    /// この文字数分を変換せずひらがなのまま表示する。Escを押すたびに
    /// 一つ前の文節分だけ増え、前の変換も順にひらがなへ戻す。
    pub kana_tail_len: usize,
    /// 入力世代。入力・確定・取消のたびに増える。非同期のLLM結果が
    /// 発火時と同じ世代のときだけ適用し、古い結果が別の位置に誤って
    /// 差し込まれる（前の入力が壊れる）のを防ぐ。
    pub generation: u64,
    /// 学習リポジトリ（確定履歴の記録・候補の頻度順ソート）
    pub learning: Option<LearningRepository>,
    /// 変換が有効かどうか
    pub enabled: bool,
    /// 確定後の復元用リングバッファ（最新が末尾、最大`COMMIT_RING_CAPACITY`件）。
    ///
    /// 「壊れた入力を復元するIME」構想の中核。`commit()`が確定のたびに
    /// 読み・表記・文字数・n-best候補を積み、`invalidate_commit_ring()`で
    /// 無効化条件（フォーカス変更・マウスクリック・方向キー/Home/End/Page/
    /// Delete/Backspace・確定以外の経路での文字入力）を満たしたら丸ごと
    /// 空にする。無効化条件は保守的に倒す（復元できない場面が増えるのは
    /// 軽い不満だが、ズレた位置への復元は他の文章を破壊するため）。
    pub commit_ring: std::collections::VecDeque<CommittedEntry>,
    /// 確定後の復元ホットキーを押している最中の選択状態。`None`なら
    /// 復元操作は行われていない（通常状態）。ホットキーを押しても
    /// `commit_ring`自体は書き換えず、ここに候補一覧だけを用意する
    /// （選んで確定するまでは何も変更しない。誤発火してもEscで無傷）。
    pub restore_selection: Option<RestoreSelection>,
}

/// `commit_ring`の1エントリ（確定1回分）。
#[derive(Debug, Clone)]
pub struct CommittedEntry {
    /// 確定時の読み（かな列、全文節分を連結したもの）
    pub reading: String,
    /// 確定した表記（全文節分を連結したもの）
    pub surface: String,
    /// `surface`の文字数（Backspace送出に必要）
    pub char_count: usize,
    /// 確定時点でのn-best候補（表記のみ）。`converter.n_best_strings`を
    /// `reading`に対して呼んだ結果をそのまま保持する（後続文脈を使った
    /// 再ランキングは行わない、確定後の復元の最初の段階）。
    pub candidates: Vec<String>,
}

/// `commit_ring`が保持する最大件数。
const COMMIT_RING_CAPACITY: usize = 8;
/// 確定時に`commit_ring`のエントリへ保存するn-bestの件数。
const COMMIT_RING_N_BEST: usize = 9;

/// 確定後の復元用ホットキーを押している最中の選択状態
/// （[[commit-ring-buffer]]の復元ロジック）。
///
/// ホットキーを押しても`commit_ring`の内容は書き換えない
/// （候補ウィンドウを出すだけで、選んで確定するまでは何も変更しない設計。
/// 誤発火してもEscで無傷のまま抜けられる）。
#[derive(Debug, Clone)]
pub struct RestoreSelection {
    /// 復元対象の`commit_ring`エントリの候補一覧（表記のみ）
    pub candidates: Vec<String>,
    /// 選択中の候補インデックス
    pub selected: usize,
    /// 元の確定表記の文字数（確定を選んだときのBackspace送出数）
    pub char_count: usize,
}

/// 「もしかして」候補の根拠の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionSource {
    /// ユーザー本人が過去に確定した誤字修正（完全一致辞書）
    Learned,
    /// ローマ字取り残し（埋め込み英字）の補正
    RomajiRepair,
    /// ハードコードされた誤字ルール（`TypoCorrector::correct_sentence`）
    HardcodedRule,
}

/// 「もしかして」候補を出す3つの補正器（学習済み誤字修正/ローマ字取り残し/
/// ハードコード誤字ルール）を同じ尺度で比較するための統一形式。
///
/// 以前はこの3つを「先勝ち」（コードの記述順、学習済み→ローマ字取り残し→
/// ハードコードルールの順に試し、最初に見つかった候補で確定）で選んでいた。
/// 優先順位が確からしさではなく記述順で決まり、補正器を足すたびに
/// 「どこに挿すか」の議論が発生する問題があった。
///
/// `cost`はViterbiと同じ尺度（低いほど自然）にする。根拠の種類が違っても、
/// 「補正後の読みを実際に変換してみたときの自然さ」で横並びに比較できる
/// ようにするのが狙い（[[correction-cascade-unification]]）。
#[derive(Debug, Clone)]
pub struct CorrectionCandidate {
    /// 補正後の読み（かな列）
    pub reading: String,
    /// 補正後の表記
    pub surface: String,
    /// Viterbiと同じ尺度のコスト（低いほど採用されやすい）
    pub cost: i32,
    /// この候補を出した補正器
    pub source: CorrectionSource,
}

/// 「もしかして」候補を提案する補正器。採用するかどうかの判断はせず、
/// 候補を返すだけにする（呼び出し側が全補正器の結果をコストでマージする）。
pub trait Corrector {
    fn suggest(&self, state: &LiveConversionState) -> Vec<CorrectionCandidate>;
}

/// 4-A-1: 学習済み誤字修正。ユーザー本人が過去に確定した誤字修正が今回の
/// 読みに完全一致するなら候補にする。
pub struct LearnedTypoCorrector;

impl Corrector for LearnedTypoCorrector {
    fn suggest(&self, state: &LiveConversionState) -> Vec<CorrectionCandidate> {
        if !((state.romaji_buffer.is_empty() || state.is_stuck_romaji(&state.romaji_buffer))
            && state.kana_tail_len == 0)
        {
            return Vec::new();
        }
        let Some(converter) = state.converter.as_ref() else {
            return Vec::new();
        };
        let typo_input = format!("{}{}", state.hiragana_buffer, state.romaji_buffer);
        let Some((reading, surface, bonus)) = converter.typo_corrections.get(&typo_input) else {
            return Vec::new();
        };
        if surface == &state.conversion_result {
            return Vec::new();
        }
        // 学習済みの読み・表記はそのまま使う（ユーザー本人が過去に確定した
        // 組み合わせなので、辞書の現在の解釈で再導出しない）。コストだけは
        // 「その読みを実際に変換したときの自然さ」から学習ボーナス
        // （`learn_typo_correction`が頻度から計算済みの値）を引いて、他の
        // 補正器と同じ尺度に揃える。現在の辞書で綺麗に変換できなくなって
        // いた場合でも、完全一致という根拠自体は辞書の変化と無関係に有効
        // なので、候補自体は出す（コストは0を基準にする）。
        let base_cost = converter
            .clean_reading(reading)
            .map(|(_, cost)| cost)
            .unwrap_or(0);
        vec![CorrectionCandidate {
            reading: reading.clone(),
            surface: surface.clone(),
            cost: base_cost.saturating_sub(*bonus),
            source: CorrectionSource::Learned,
        }]
    }
}

/// 4-A-2: ローマ字取り残し（埋め込み英字）の補正。
pub struct RomajiRepairCorrector;

impl Corrector for RomajiRepairCorrector {
    fn suggest(&self, state: &LiveConversionState) -> Vec<CorrectionCandidate> {
        if state.kana_tail_len != 0 {
            return Vec::new();
        }
        match state.romaji_repair_suggest() {
            Some((reading, surface, cost)) => vec![CorrectionCandidate {
                reading,
                surface,
                cost,
                source: CorrectionSource::RomajiRepair,
            }],
            None => Vec::new(),
        }
    }
}

/// 4-A-3: ハードコードされた誤字ルール（`TypoCorrector::correct_sentence`）。
pub struct HardcodedRuleCorrector;

impl Corrector for HardcodedRuleCorrector {
    fn suggest(&self, state: &LiveConversionState) -> Vec<CorrectionCandidate> {
        if !(state.romaji_buffer.is_empty() && state.kana_tail_len == 0) {
            return Vec::new();
        }
        let Some(reading) = crate::TypoCorrector::correct_sentence(&state.hiragana_buffer) else {
            return Vec::new();
        };
        // 表記は従来どおり`convert_buffer`（固定済み先頭文節`pinned`を反映）で
        // 作る。コストは`clean_reading`（pinned非対応）で近似する。
        // ハードコードルールの候補は通常、現在編集中の短い範囲が対象で
        // pinnedの影響を受けにくいため、この近似で十分と判断している。
        // 辞書未ロード（converter無し）のときは`convert_buffer`が常に空を
        // 返すため、読みをそのまま表記として使う（従来の挙動）。
        let surface = if state.converter.is_some() {
            state
                .convert_buffer(&reading)
                .iter()
                .map(|e| e.surface.as_str())
                .collect()
        } else {
            reading.clone()
        };
        if surface == state.conversion_result {
            return Vec::new();
        }
        let cost = state
            .converter
            .as_ref()
            .and_then(|c| c.clean_reading(&reading))
            .map(|(_, cost)| cost)
            .unwrap_or(i32::MAX / 4);
        vec![CorrectionCandidate {
            reading,
            surface,
            cost,
            source: CorrectionSource::HardcodedRule,
        }]
    }
}

impl LiveConversionState {
    pub fn new() -> Self {
        Self {
            romaji: RomajiConverter::new(),
            converter: None,
            load_judge_async: false,
            pending_judge: None,
            romaji_buffer: String::new(),
            hiragana_buffer: String::new(),
            conversion_result: String::new(),
            last_sent_length: 0,
            candidates: Vec::new(),
            candidate_index: 0,
            cand_seg_reading: String::new(),
            cand_seg_surfaces: Vec::new(),
            cand_prefix_surface: String::new(),
            cand_prefix_reading: String::new(),
            cand_suffix_surface: String::new(),
            cand_suffix_reading: String::new(),
            committed_segments: Vec::new(),
            recent_context: String::new(),
            bracket_context: String::new(),
            defer_learning: false,
            request_deferred_learning: None,
            pending_learning: Vec::new(),
            pending_prediction_learning: Vec::new(),
            pinned: Vec::new(),
            predictions: Vec::new(),
            prediction_top_is_fuzzy: false,
            prediction_top_is_learned_typo: false,
            prediction_index: 0,
            last_committed: String::new(),
            kana_tail_len: 0,
            generation: 0,
            learning: None,
            enabled: true,
            commit_ring: std::collections::VecDeque::new(),
            restore_selection: None,
        }
    }

    /// バックグラウンドで読み込み終わった判断層があれば変換エンジンに取り付ける
    /// （待たない。まだなら次の変換で再確認する）
    fn attach_pending_judge(&mut self) {
        let Some(rx) = self.pending_judge.as_ref() else { return };
        match rx.try_recv() {
            Ok(judge) => {
                if let (Some(judge), Some(conv)) = (judge, self.converter.as_mut()) {
                    conv.set_judge(Some(judge));
                }
                self.pending_judge = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pending_judge = None,
        }
    }

    /// 確定後の復元用リングバッファを無効化する（丸ごと破棄）。
    ///
    /// フォーカス変更・マウスクリック・方向キー/Home/End/Page/Delete/
    /// Backspace・確定以外の経路での文字入力があったら呼ぶこと（呼び出し元は
    /// `hook-dll`側。`common`自体はWin32非依存に保つため、どのイベントが
    /// これに該当するかの判定はhook-dll側の責務）。復元先の位置がずれたまま
    /// 再変換すると無関係な文章を破壊するため、該当するかどうか迷ったら
    /// 呼ぶ（保守的に倒す）。
    pub fn invalidate_commit_ring(&mut self) {
        self.commit_ring.clear();
        self.restore_selection = None;
    }

    /// 復元ホットキーが押された。`commit_ring`の最新エントリ（最新1件のみ、
    /// [[commit-ring-buffer]]で「遡るほど誤発火時の被害が大きくなる」ため
    /// 意図的に1件に限定）を候補ウィンドウ表示用に取り出す。
    ///
    /// 既に復元選択中（2回目以降のホットキー押下）なら、`commit_ring`は
    /// 見ずに選択位置だけ進める（＝候補を巡回する。押すたびに次の候補が
    /// 「選択中」になるだけで、Enterで確定するまでは何も変更しない）。
    ///
    /// 戻り値は候補一覧と選択位置（ポップアップ表示用）。`commit_ring`が
    /// 空で、かつ復元選択中でもなければ`None`（表示するものが無い）。
    pub fn press_restore_hotkey(&mut self) -> Option<(Vec<String>, usize)> {
        if self.restore_selection.is_some() {
            return self.cycle_restore_candidate(false);
        }

        let entry = self.commit_ring.back()?;
        if entry.candidates.is_empty() {
            return None;
        }
        // 初回は「次の候補」（index 1）から見せる。index 0
        // （＝今まさに画面にある表記）を選んでも見た目上は無変化のため、
        // 最初から次善候補を提示した方が実用的（ユーザー指摘の設計通り）。
        let selected = if entry.candidates.len() > 1 { 1 } else { 0 };
        self.restore_selection = Some(RestoreSelection {
            candidates: entry.candidates.clone(),
            selected,
            char_count: entry.char_count,
        });
        let sel = self.restore_selection.as_ref().unwrap();
        Some((sel.candidates.clone(), sel.selected))
    }

    /// 復元選択中に候補を巡回する（ホットキー以外にも、標準IME同様の
    /// Space/↓＝次候補、↑＝前候補からも呼ぶ）。復元選択中でなければ`None`。
    pub fn cycle_restore_candidate(&mut self, backwards: bool) -> Option<(Vec<String>, usize)> {
        let sel = self.restore_selection.as_mut()?;
        if sel.candidates.len() > 1 {
            sel.selected = if backwards {
                (sel.selected + sel.candidates.len() - 1) % sel.candidates.len()
            } else {
                (sel.selected + 1) % sel.candidates.len()
            };
        }
        Some((sel.candidates.clone(), sel.selected))
    }

    /// 復元選択中の候補を確定する。`commit_ring`のBackspace対象文字数を
    /// 使って`ConversionAction`を組み立て、`commit_ring`の最新エントリを
    /// 新しい表記へ更新する（再度ホットキーを押したときの起点を今回選んだ
    /// 表記に揃えるため）。復元選択中でなければ`None`。
    pub fn confirm_restore(&mut self) -> Option<ConversionAction> {
        let sel = self.restore_selection.take()?;
        let new_surface = sel.candidates.get(sel.selected)?.clone();
        if let Some(entry) = self.commit_ring.back_mut() {
            entry.surface = new_surface.clone();
            entry.char_count = new_surface.chars().count();
        }
        Some(ConversionAction {
            delete_count: sel.char_count,
            insert_text: new_surface,
        })
    }

    /// 番号キー等で特定の候補を直接指定して確定する（標準IMEの候補一覧と
    /// 同じ「番号キー＝その場で選んで確定」の慣習に合わせる）。`index`が
    /// 範囲外、または復元選択中でなければ`None`（呼び出し側は状態を変えない）。
    pub fn confirm_restore_at(&mut self, index: usize) -> Option<ConversionAction> {
        let sel = self.restore_selection.as_mut()?;
        if index >= sel.candidates.len() {
            return None;
        }
        sel.selected = index;
        self.confirm_restore()
    }

    /// 復元選択を取り消す（`commit_ring`には触れない。画面には何も送らない）。
    pub fn cancel_restore(&mut self) {
        self.restore_selection = None;
    }

    /// 復元選択中かどうか（hook.rs側がこのキー入力を復元モード扱いにして
    /// よいかの判定に使う）。
    pub fn is_restoring(&self) -> bool {
        self.restore_selection.is_some()
    }

    /// 学習リポジトリの内容を変換エンジンのメモリへ一括ロードする
    ///
    /// 起動時と学習DB切替時に呼ぶ。これによりライブ変換が過去の
    /// 学習を反映する（使うほど賢くなる仕組みの土台）。
    pub fn reload_learning_into_converter(&mut self) {
        let (Some(conv), Some(learning)) = (self.converter.as_mut(), self.learning.as_ref())
        else {
            return;
        };
        conv.clear_learning();
        if let Ok(unigrams) = learning.all_unigrams() {
            for (reading, surface, freq) in unigrams {
                conv.learn_unigram(&reading, &surface, freq);
            }
        }
        if let Ok(bigrams) = learning.all_bigrams() {
            for (prev, surface, freq) in bigrams {
                conv.learn_bigram(&prev, &surface, freq);
            }
        }
        if let Ok(assocs) = learning.all_assocs() {
            for (prev, content, freq) in assocs {
                conv.learn_assoc(&prev, &content, freq);
            }
        }
        if let Ok(prefs) = learning.all_hiragana_prefs() {
            for (reading, freq) in prefs {
                conv.learn_hiragana(&reading, freq);
            }
        }
        if let Ok(typos) = learning.all_typo_corrections() {
            for (wrong, reading, surface, freq) in typos {
                conv.learn_typo_correction(&wrong, &reading, &surface, freq);
            }
        }
        debug_log!(
            "学習ロード: unigram={}, bigram={}, assoc={}, typo={}",
            conv.learned_unigram.len(),
            conv.learned_bigram.len(),
            conv.learned_assoc.len(),
            conv.typo_corrections.len()
        );
    }

    /// ユーザー登録の単語（user_dictionary）をライブ変換器の辞書へ注入する。
    /// 辞書ロード直後に呼ぶ。辞書に無い複合語を変換候補に出せるようにする。
    pub fn inject_user_words(&mut self) {
        // 先に読み出してから（learning の借用を落として）converter を可変借用する。
        let words = match self.learning.as_ref().and_then(|l| l.get_all_user_words().ok()) {
            Some(w) => w,
            None => return,
        };
        let n = words.len();
        if let Some(conv) = self.converter.as_mut() {
            for e in words {
                let reading = e.reading.clone();
                let surface = e.surface.clone();
                conv.overlay.add_word(crate::WordEntry {
                    surface: e.surface,
                    reading: e.reading,
                    left_id: USER_WORD_POS_ID,
                    right_id: USER_WORD_POS_ID,
                    cost: e.cost as i16,
                    pos: e.pos.unwrap_or_else(|| USER_WORD_POS.to_string()),
                });
                // 学習済みの別解（例「思い＋で」）に負けないよう、登録語にも
                // 学習ボーナスを与えて最優先で選ばせる。
                conv.learn_unigram(&reading, &surface, USER_WORD_LEARN_FREQ);
            }
        }
        debug_log!("ユーザー辞書を注入: {} 語", n);
    }

    /// 単語を登録する（DB へ保存＋ライブ変換器の辞書へ即注入）。
    /// 読みはひらがな、表記は任意。空や重複はそのまま上書き（INSERT OR REPLACE）。
    /// 成功したら true。
    pub fn register_user_word(&mut self, reading: &str, surface: &str) -> bool {
        let reading = reading.trim().to_string();
        let surface = surface.trim().to_string();
        if reading.is_empty() || surface.is_empty() {
            return false;
        }
        // 読みが漢字/カタカナ等（＝読みと表記を入力欄で取り違えた等）だと、
        // その読みで打っても一致しない死んだエントリになる。読みは常に
        // ひらがなである契約なので、そうでなければ登録を拒否する。
        if !is_hiragana_reading(&reading) {
            return false;
        }
        if let Some(learning) = self.learning.as_ref() {
            if learning
                .add_user_word(&reading, &surface, Some(USER_WORD_POS), USER_WORD_COST as i32)
                .is_err()
            {
                return false;
            }
        } else {
            return false;
        }
        if let Some(conv) = self.converter.as_mut() {
            conv.overlay.add_word(crate::WordEntry {
                surface: surface.clone(),
                reading: reading.clone(),
                left_id: USER_WORD_POS_ID,
                right_id: USER_WORD_POS_ID,
                cost: USER_WORD_COST,
                pos: USER_WORD_POS.to_string(),
            });
            // 学習済みの別解にも勝てるよう、登録語に学習ボーナスを付与する。
            conv.learn_unigram(&reading, &surface, USER_WORD_LEARN_FREQ);
        }
        true
    }

    /// 隣接する漢字複合語の反復使用から自動登録する（`register_user_word`と
    /// 同じ辞書注入だが、DBには`source='auto'`で記録し手動登録と区別する）。
    pub fn register_auto_compound(&mut self, reading: &str, surface: &str) -> bool {
        if reading.is_empty() || surface.is_empty() || !is_hiragana_reading(reading) {
            return false;
        }
        if let Some(learning) = self.learning.as_ref() {
            if learning
                .add_auto_compound_word(reading, surface, USER_WORD_COST as i32)
                .is_err()
            {
                return false;
            }
        } else {
            return false;
        }
        if let Some(conv) = self.converter.as_mut() {
            conv.overlay.add_word(crate::WordEntry {
                surface: surface.to_string(),
                reading: reading.to_string(),
                left_id: USER_WORD_POS_ID,
                right_id: USER_WORD_POS_ID,
                cost: USER_WORD_COST,
                pos: USER_WORD_POS.to_string(),
            });
            conv.learn_unigram(reading, surface, USER_WORD_LEARN_FREQ);
        }
        true
    }

    /// 登録済みの単語を削除する（DB とライブ辞書の両方から）。消したら true。
    pub fn delete_user_word(&mut self, reading: &str, surface: &str) -> bool {
        let ok = self
            .learning
            .as_ref()
            .map(|l| l.remove_user_word(reading, surface).unwrap_or(false))
            .unwrap_or(false);
        if let Some(conv) = self.converter.as_mut() {
            conv.overlay.remove_word(reading, surface);
        }
        ok
    }

    /// 登録済みの単語一覧（読み, 表記）を返す（設定画面用）。
    pub fn all_user_words(&self) -> Vec<(String, String)> {
        self.learning
            .as_ref()
            .and_then(|l| l.get_all_user_words().ok())
            .map(|v| v.into_iter().map(|e| (e.reading, e.surface)).collect())
            .unwrap_or_default()
    }

    /// ローマ字が変換されずに取り残された打ち間違いを補正して「もしかして」を返す。
    ///
    /// ローマ字変換は変換できない英字をそのまま結果に混ぜて先へ進むため、取り残し
    /// は末尾だけでなく **途中・先頭に埋め込まれる**（例: saynara→「さyなら」、
    /// gm…→「gm…」）。そこで「かな＋末尾ローマ字」を1本の文字列として見て、
    /// 埋め込まれた英字（＝行き止まりのローマ字）を母音補完/削除で直し、綺麗に
    /// 変換できればそれを提案する。母音待ちの正常な途中入力（末尾の k/sh 等）は
    /// 対象外（失敗ではないので出さない）。
    pub fn romaji_repair_suggest(&self) -> Option<(String, String, i32)> {
        let embedded = self.hiragana_buffer.chars().any(|c| c.is_ascii_alphabetic());
        let trailing_stuck =
            !self.romaji_buffer.is_empty() && self.is_stuck_romaji(&self.romaji_buffer);
        // 前方に誤入力があっても、末尾の正常な子音は次の母音を待つ。
        // 例:「gめんなさいk」のkまで補正して入力途中の語を奪わない。
        if !self.romaji_buffer.is_empty() && !trailing_stuck {
            return None;
        }
        if !embedded && !trailing_stuck {
            return None; // 取り残しなし（正常）
        }
        let converter = self.converter.as_ref()?;
        let full = format!("{}{}", self.hiragana_buffer, self.romaji_buffer);
        if full.chars().count() > 20 {
            return None;
        }

        // 綺麗に変換できる候補の中から、**最も自然（総コスト最小）**なものを選ぶ。
        // 母音の順番ではなくコストで選ぶことで、紗綾なら のような造語ではなく
        // さよなら のような意味の通る語が選ばれる。
        let mut best: Option<(String, String, i32)> = None;
        let mut alternative_cost = i32::MAX;
        for reading in self.romaji_repair_readings(&full) {
            if reading.chars().count() < 2 {
                continue;
            }
            if let Some((surface, cost)) = converter.clean_reading(&reading) {
                if best.as_ref().map_or(true, |(_, _, bc)| cost < *bc) {
                    if let Some((_, old_surface, old_cost)) = best.as_ref() {
                        if old_surface != &surface {
                            alternative_cost = alternative_cost.min(*old_cost);
                        }
                    }
                    best = Some((reading, surface, cost));
                } else if best.as_ref().is_some_and(|(_, s, _)| s != &surface) {
                    alternative_cost = alternative_cost.min(cost);
                }
            }
        }
        let (reading, surface, cost) = best?;
        // 異なる表記の候補が僅差なら推測を表示しない。
        if alternative_cost.saturating_sub(cost) < 500 {
            return None;
        }
        // 造語ガード: 1文字あたりのコストが高い（＝不自然な語の寄せ集め）なら
        // 「意味を成す補正が見つからなかった」として出さない。もしかして
        // （かな置換）側の best_word_correction と共通のしきい値を使う。
        if !crate::viterbi::is_plausible_correction_cost(cost, reading.chars().count()) {
            return None;
        }
        Some((reading, surface, cost))
    }

    /// 「かな＋英字」混在文字列から、英字（取り残しローマ字）を直した「読み」候補
    /// （latin を含まないかな列）を生成する。誤字パターンを幅広くカバーする:
    ///   A) 母音抜け: 各英字の直後に母音を入れる（先頭/中間/末尾・最大3英字の全組合せ）
    ///   B) 子音を母音で打ち間違え: 各英字を母音に置換
    ///   C) 打ちすぎ: 英字を1個ずつ削除
    ///   D) 入れ替え: 英字と隣接文字を入れ替え（タイプミスの転置）
    ///   E) フォールバック: 英字を全部削除
    /// これらを romaji 変換して「英字が残らず確定するもの」だけを候補にする。
    pub fn romaji_repair_readings(&self, full: &str) -> Vec<String> {
        let chars: Vec<char> = full.chars().collect();
        let latin_idx: Vec<usize> = chars
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_ascii_alphabetic())
            .map(|(i, _)| i)
            .collect();
        let k = latin_idx.len();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // パターンごとに専用の枠（バケツ）を用意し、そのパターンからの候補が
        // 尽きるまで他のバケツを侵食しない。以前は1本の`out`にA→B→C→D→Eの順で
        // 積んでから`truncate(80)`していたため、母音補完/置換（Bはk<=3のとき
        // 5^k通りの総当たり）が先に80件の枠を埋め切ってしまい、後段の転置(D)が
        // 実質的に一度も候補へ残らなかった（[[correction-cascade-unification]]
        // で発覚）。バケツを分けることで、各パターンが少なくとも
        // `PER_PATTERN_CAP`件までは他パターンの量に関係なく試される。
        // 合計の上限（clean_readingを呼ぶ回数＝レイテンシに直結）は
        // 4パターン×19+フォールバック1＝77件で、従来の80件を超えない。
        const PER_PATTERN_CAP: usize = 19;
        let mut insertions: Vec<String> = Vec::new(); // A) 母音補完
        let mut substitutions: Vec<String> = Vec::new(); // B) 母音置換
        let mut deletions: Vec<String> = Vec::new(); // C) 1文字削除
        let mut transpositions: Vec<String> = Vec::new(); // D) 隣接入れ替え
        let mut fallback: Vec<String> = Vec::new(); // E) 全削除

        let mut try_push = |edited: &[char], bucket: &mut Vec<String>, cap: usize| {
            if bucket.len() >= cap {
                return;
            }
            if let Some(r) = self.romaji_settle(edited) {
                if seen.insert(r.clone()) {
                    bucket.push(r);
                }
            }
        };

        // 「n」は行き止まりの英字の中でも特によく現れる（末尾で「ん」に
        // なり損ねる: nippon→にっぽn）ため、置換バケツの中で優先的に試す。
        for &i in &latin_idx {
            if chars[i] == 'n' {
                let mut edited = chars.clone();
                edited[i] = 'ん';
                try_push(&edited, &mut substitutions, PER_PATTERN_CAP);
            }
        }
        // B) 各英字を単独で母音に置換（他の英字はそのまま。複数の取り残しが
        // あるとき、1箇所だけが打ち間違いというケースを拾う）
        for &i in &latin_idx {
            for replacement in ['a', 'i', 'u', 'e', 'o'] {
                let mut edited = chars.clone();
                edited[i] = replacement;
                try_push(&edited, &mut substitutions, PER_PATTERN_CAP);
            }
        }
        // C) 各英字を1個削除
        for &i in &latin_idx {
            let mut edited = chars.clone();
            edited.remove(i);
            try_push(&edited, &mut deletions, PER_PATTERN_CAP);
        }
        // D) 英字と隣接文字を入れ替え（転置ミス）
        for &i in &latin_idx {
            if i + 1 < chars.len() {
                let mut edited = chars.clone();
                edited.swap(i, i + 1);
                try_push(&edited, &mut transpositions, PER_PATTERN_CAP);
            }
            if i > 0 {
                let mut edited = chars.clone();
                edited.swap(i - 1, i);
                try_push(&edited, &mut transpositions, PER_PATTERN_CAP);
            }
        }
        if (1..=3).contains(&k) {
            let combos = vowel_combos(k); // 5^k（k<=3 で最大125）
            // A) 母音補完（各英字の後ろに母音を挿入、全英字を同時に処理）
            for combo in &combos {
                let mut edited: Vec<char> = Vec::with_capacity(chars.len() + k);
                let mut li = 0;
                for (i, &ch) in chars.iter().enumerate() {
                    edited.push(ch);
                    if li < k && latin_idx[li] == i {
                        edited.push(combo[li]);
                        li += 1;
                    }
                }
                try_push(&edited, &mut insertions, PER_PATTERN_CAP);
            }
            // B) 母音置換（全英字を同時に、組合せ総当たりで置き換え）
            for combo in &combos {
                let mut edited = chars.clone();
                for (li, &idx) in latin_idx.iter().enumerate() {
                    edited[idx] = combo[li];
                }
                try_push(&edited, &mut substitutions, PER_PATTERN_CAP);
            }
        }
        // E) フォールバック: 英字を全部削除
        let no_latin: Vec<char> = chars
            .iter()
            .cloned()
            .filter(|c| !c.is_ascii_alphabetic())
            .collect();
        try_push(&no_latin, &mut fallback, 1);

        let mut out = Vec::with_capacity(
            insertions.len() + substitutions.len() + deletions.len() + transpositions.len() + fallback.len(),
        );
        out.extend(insertions);
        out.extend(substitutions);
        out.extend(deletions);
        out.extend(transpositions);
        out.extend(fallback);
        out
    }

    /// 編集後の（かな＋英字）列をローマ字変換し、英字が残らず2文字以上の
    /// かな列になったらそれを返す（＝ちゃんと確定した読み）。
    pub fn romaji_settle(&self, edited: &[char]) -> Option<String> {
        let s: String = edited.iter().collect();
        let conv = self.romaji.convert(&s);
        if conv.chars().any(|c| c.is_ascii_alphabetic()) || conv.chars().count() < 2 {
            return None;
        }
        Some(conv)
    }

    /// 末尾ローマ字が「行き止まり」（母音を足しても確定しない＝打ち間違い）か。
    pub fn is_stuck_romaji(&self, tail: &str) -> bool {
        if tail.is_empty() {
            return false;
        }
        for vowel in ['a', 'i', 'u', 'e', 'o'] {
            let mut r = tail.to_string();
            r.push(vowel);
            let (settled, pending) = self.romaji.split(&r);
            if pending.is_empty() && !settled.is_empty() {
                return false; // 母音で確定する＝正常な途中入力
            }
        }
        true
    }

    /// 予測変換の候補を更新する
    ///
    /// - 打鍵中（hiragana_buffer あり）: 読みが前方一致する確定履歴を補完候補に
    /// - 未入力・確定直後: 予測を表示しない
    pub fn update_predictions(&mut self) {
        self.update_predictions_with_completion(false);
    }

    /// Tabで要求されたときだけ履歴補完・続き予測を検索する。
    pub fn request_predictions(&mut self) {
        self.update_predictions_with_completion(true);
    }

    fn update_predictions_with_completion(&mut self, include_completion: bool) {
        self.predictions.clear();
        self.prediction_top_is_fuzzy = false;
        self.prediction_top_is_learned_typo = false;
        self.prediction_index = 0;

        if !self.enabled {
            return;
        }
        if self.hiragana_buffer.is_empty() && self.romaji_buffer.is_empty() {
            return;
        }

        // 「もしかして」候補（学習済み誤字修正/ローマ字取り残し/ハードコード
        // 誤字ルール）。3つの補正器（`Corrector`trait）を全て走らせ、返って
        // きた候補をコスト順にマージする（統一スコア空間、
        // [[correction-cascade-unification]]）。
        //
        // 以前は「先勝ち」（記述順に学習済み→ローマ字取り残し→ハードコード
        // ルールの順に試し、最初に見つかった候補で確定）で、優先順位が
        // 確からしさではなく記述順で決まっていた。この段階でマージ方式へ
        // 切り替える（構造だけ先に変えてゴールデンテストの完全一致を
        // 確認済み。リングバッファの「器→検証→復元」と同じ進め方）。
        //
        // 学習済み誤字修正（ユーザー本人が過去に確定した確実な修正）は
        // 対象外にしない。以前は「ローマ字は全部かなになったが辞書上意味を
        // 成さない断片」も fuzzy_suggest で拾っていたが、辞書のあいまい検索
        // による“その場の推測”は外れも多くノイズになるという実機フィード
        // バックにより対象から外した（[[fuzzy-correction-approach]]参照）。
        let mut correction_candidates: Vec<CorrectionCandidate> = Vec::new();
        correction_candidates.extend(LearnedTypoCorrector.suggest(self));
        correction_candidates.extend(RomajiRepairCorrector.suggest(self));
        // 修正箇所より前が固定済み文節（`self.pinned`）と一致する範囲は
        // 再変換せず使い回す（`HardcodedRuleCorrector`が内部で
        // `convert_buffer`を使う理由。`correct_sentence`は毎打鍵ごとに
        // 読み全体を返すため、素の`convert_context_aware`だと修正箇所を
        // 含む語がある間、打鍵のたびに読み全体のフルのViterbi変換が
        // 余分に走ってしまう）。
        correction_candidates.extend(HardcodedRuleCorrector.suggest(self));
        correction_candidates.sort_by_key(|c| c.cost);

        if let Some(best) = correction_candidates.first() {
            // 2位との差が僅差なら、どちらとも決めがたいとして出さない
            // （既存の閾値。従来はromaji_repair内の複数候補間だけの判定
            // だったが、統一スコア空間では根拠の異なる候補どうしの比較にも
            // そのまま広げる）。
            let ambiguous = correction_candidates
                .get(1)
                .is_some_and(|second| second.cost.saturating_sub(best.cost) < 500);
            // 造語ガード（1文字あたりコストの上限）は、`RomajiRepair`
            // （辞書に無数の候補読みを試す投機的な補正）のときだけ適用する。
            //
            // 実測で判明: `HardcodedRuleCorrector`（例:「わたしわ」→「私は」、
            // cost=5594、4文字で1398/字）のような、キュレーション済みで元々
            // 信頼度の高い候補にまでこのゲートを広げると、閾値
            // （1300/字）をわずかに超えるだけで正当な修正が非表示になって
            // しまう（[[correction-cascade-unification]]のStage B検証で
            // 発見）。`Learned`（ユーザー本人が過去に確定済み）・
            // `HardcodedRule`（人手でキュレーション済みの確実な置換）は
            // どちらも「その場の推測」ではないため、造語ガードの対象外と
            // する（従来もこの2つには一度もこのチェックが無かった）。
            let plausible = best.source != CorrectionSource::RomajiRepair
                || crate::viterbi::is_plausible_correction_cost(
                    best.cost,
                    best.reading.chars().count(),
                );
            if !ambiguous && plausible {
                let is_learned = best.source == CorrectionSource::Learned;
                self.predictions.push((best.reading.clone(), best.surface.clone()));
                self.prediction_top_is_fuzzy = true;
                self.prediction_top_is_learned_typo = is_learned;
            }
        }

        if !include_completion {
            return;
        }
        // ここから先（履歴による前方一致補完）は学習DBが要る。
        if self.learning.is_none() {
            return;
        }
        // 前方一致補完（履歴）のみ。2文字以上打った時だけ、打った読みより長い履歴を
        // 候補にする（頻度1以上）。前方一致なので無関係語は出ない。
        let prefix_len = self.hiragana_buffer.chars().count();
        if prefix_len >= 2 {
            let prefix_list = self
                .learning
                .as_ref()
                .and_then(|l| l.predict_by_prefix_in_context(&self.hiragana_buffer, &self.last_committed, 5).ok())
                .unwrap_or_default();
            for (reading, surface, freq) in prefix_list {
                // もしかしてと重複する表記は出さない
                let dup = self.predictions.iter().any(|(_, s)| *s == surface);
                if freq >= 1 && reading.chars().count() > prefix_len && !dup {
                    self.predictions.push((reading, surface));
                }
            }
        }

        // 文中の末尾語も補完する。表示と分割結果が一致するときだけ前半を保持し、
        // ユーザーが選び直した表記や未確定のローマ字を上書きしない。
        if self.romaji_buffer.is_empty() && self.kana_tail_len == 0 {
            let entries = self.convert_buffer(&self.hiragana_buffer);
            let displayed: String = entries.iter().map(|e| e.surface.as_str()).collect();
            if entries.len() >= 2 && displayed == self.conversion_result {
                let tail = entries.last().unwrap();
                // 助動詞「たい」「ます」や助詞は、文中で既に役割が決まっている。
                // 独立した単語の語頭として履歴検索すると「変換したい→変換したい○○」
                // のような無関係な補完になるため、末尾語の補完対象から除く。
                if tail.reading.chars().count() >= 2
                    && !tail.pos.starts_with("助動詞")
                    && !tail.pos.starts_with("助詞")
                    && !tail.pos.starts_with("名詞-接尾")
                    && !tail.pos.starts_with("記号")
                {
                    let before = &entries[..entries.len() - 1];
                    let prefix_reading: String = before.iter().map(|e| e.reading.as_str()).collect();
                    let prefix_surface: String = before.iter().map(|e| e.surface.as_str()).collect();
                    if let Some(learning) = self.learning.as_ref() {
                        if let Ok(list) = learning.predict_by_prefix_in_context(
                            &tail.reading, &before.last().unwrap().surface, 5,
                        ) {
                            for (reading, surface, _) in list {
                                let surface = format!("{}{}", prefix_surface, surface);
                                if !self.predictions.iter().any(|(_, s)| s == &surface) {
                                    self.predictions.push((format!("{}{}", prefix_reading, reading), surface));
                                }
                            }
                        }
                    }
                }
            }
        }

        // 元の表記をそのまま残して文字を足す補完は、誤変換も引き継ぐため
        // 提示しない。修正候補は対象外（脱字修正は文字の追加になり得る）。
        let fuzzy = self.prediction_top_is_fuzzy;
        let mut index = 0;
        self.predictions.retain(|(_, surface)| {
            let keep = (index == 0 && fuzzy)
                || !is_insertion_only(&self.conversion_result, surface);
            index += 1;
            keep
        });
    }

    /// 予測候補を選んで確定する（番号キー）
    ///
    /// 前方一致補完: 現在の表示を予測語の表記に置き換えて確定。
    /// 次単語予測(読み空): 現在の表示の後ろに追記して確定。
    pub fn commit_prediction(&mut self, index: usize) -> Option<ConversionAction> {
        if !self.enabled || index >= self.predictions.len() {
            return None;
        }
        let (reading, surface) = self.predictions[index].clone();
        // 誤字修正の学習用に、確定でクリアされる前の生の読み（誤字そのもの）を控える。
        let original_reading = format!("{}{}", self.hiragana_buffer, self.romaji_buffer);
        // 前方一致は現在表示を置換、次単語は追記
        let delete_count = if reading.is_empty() {
            0
        } else {
            self.conversion_result.chars().count()
        };
        let mut action = ConversionAction {
            delete_count,
            insert_text: surface.clone(),
        };

        // 追記候補でも、表示済みの本文を通常確定と同じ経路で学習・文脈保存する。
        if reading.is_empty() && !self.conversion_result.is_empty() {
            // 末尾に未確定の 'n' が残っていると commit() 内部で「ん」への
            // 確定アクションを返すことがある。ここで捨てると「ん」が画面に
            // 反映されないまま内部状態だけ確定してしまうため、今回の追記
            // アクションの前に連結する。
            if let Some(inner) = self.commit() {
                action.insert_text = format!("{}{}", inner.insert_text, action.insert_text);
                action.delete_count = action.delete_count.saturating_add(inner.delete_count);
            }
        }

        // 学習（確定として記録）。DB書き込みはフックの外へ遅延させる
        // （`commit()`と同じ理由。ここで同期に書くとフックの時間制限を
        // 超えて生キー漏れの原因になる）。
        let unigram = (!reading.is_empty() && is_learnable_pair(&reading, &surface))
            .then(|| (reading.clone(), surface.clone()));
        let bigram = (!self.last_committed.is_empty())
            .then(|| (self.last_committed.clone(), surface.clone()));
        // もしかして（誤字補正）の先頭候補を実際に選んだ＝その場で提示した
        // 修正をユーザーが確認・採用したということなので、誤字そのもの
        // （original_reading）→修正後(reading, surface) を学習する。
        // 次回同じ誤字を打ったとき、辞書のあいまい検索をやり直さずに
        // 即座に同じ修正を最優先で出せるようになる（使うほど賢くなる）。
        let typo_correction = (index == 0
            && self.prediction_top_is_fuzzy
            && original_reading != reading
            && !original_reading.is_empty()
            && is_learnable_pair(&reading, &surface))
        .then(|| (original_reading.clone(), reading.clone(), surface.clone()));
        if unigram.is_some() || bigram.is_some() || typo_correction.is_some() {
            self.learn_prediction_or_defer(PredictionLearning { unigram, bigram, typo_correction });
        }

        // 文脈・状態を更新して確定扱いにする
        self.recent_context.push_str(&surface);
        let chars: Vec<char> = self.recent_context.chars().collect();
        if chars.len() > 60 {
            self.recent_context = chars[chars.len() - 60..].iter().collect();
        }
        self.push_bracket_context(&surface);
        self.last_committed = surface;
        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.conversion_result.clear();
        self.last_sent_length = 0;
        self.committed_segments.clear();
        self.pinned.clear();
        self.kana_tail_len = 0;
        self.generation = self.generation.wrapping_add(1);
        self.clear_candidates();
        // 確定後は次単語予測を用意
        self.update_predictions();
        Some(action)
    }

    /// 予測候補の表示用文字列（番号は描画側で付く）。先頭が誤字補正なら
    /// 「もしかして」ラベルを付けて、履歴補完と区別する。読みが空（＝選ぶと
    /// 現在の表示に追記するモード）の候補には「→」を付け、置き換えと
    /// 区別できるようにする。
    pub fn prediction_display(&self) -> Vec<String> {
        self.predictions
            .iter()
            .enumerate()
            .map(|(i, (reading, s))| {
                let compact = if reading.is_empty() {
                    compact_prediction_text(s)
                } else {
                    prediction_change_label(&self.conversion_result, s)
                };
                if i == 0 && self.prediction_top_is_fuzzy {
                    format!("もしかして: {}", compact)
                } else if reading.is_empty() {
                    format!("→{}", compact)
                } else {
                    compact
                }
            })
            .collect()
    }

    /// 予測一覧で選択中の候補の「誤学習」をリセットする（Delete キー）。
    ///
    /// 「誤字を修正するう」のように、過去に誤って確定した内容がそのまま履歴補完
    /// として出てくる場合、その (読み, 表記) の学習だけを消す。先頭が「もしかして」
    /// の場合、その場で計算した誤字補正（学習由来ではない）なら消すものが無いので
    /// 何もしないが、学習済みの誤字修正（prediction_top_is_learned_typo）なら
    /// その学習を消す。リセットしたら true。
    pub fn reset_prediction_learning(&mut self) -> bool {
        if self.predictions.is_empty() {
            return false;
        }
        let idx = self.prediction_index.min(self.predictions.len() - 1);
        if self.prediction_top_is_fuzzy && idx == 0 {
            // 学習済みの誤字修正なら、その誤字学習だけを消す
            // （その場計算のもしかしてには消す学習が無いので何もしない）。
            if !self.prediction_top_is_learned_typo {
                return false;
            }
            let wrong_reading = format!("{}{}", self.hiragana_buffer, self.romaji_buffer);
            let (reading, surface) = self.predictions[0].clone();
            if wrong_reading.is_empty() {
                return false;
            }
            if let Some(learning) = self.learning.as_ref() {
                let _ = learning.forget_typo_correction(&wrong_reading, &reading, &surface);
            }
            if let Some(conv) = self.converter.as_mut() {
                conv.forget_typo_correction(&wrong_reading);
            }
            debug_log!(
                "誤字修正の学習リセット: '{}' → 読み='{}' 表記='{}'",
                wrong_reading, reading, surface
            );
            self.update_predictions();
            return true;
        }
        let (reading, surface) = self.predictions[idx].clone();
        if reading.is_empty() || surface.is_empty() {
            return false;
        }
        if let Some(learning) = self.learning.as_ref() {
            let _ = learning.forget_commit(&reading, &surface);
        }
        if let Some(conv) = self.converter.as_mut() {
            conv.forget_unigram(&reading, &surface);
        }
        debug_log!("予測の誤学習リセット: 読み='{}' 表記='{}'", reading, surface);
        // 消した結果で予測を作り直す
        self.update_predictions();
        true
    }

    /// 候補一覧の状態をすべてクリア
    pub fn clear_candidates(&mut self) {
        self.candidates.clear();
        self.candidate_index = 0;
        self.cand_seg_reading.clear();
        self.cand_seg_surfaces.clear();
        self.cand_prefix_surface.clear();
        self.cand_prefix_reading.clear();
        self.cand_suffix_surface.clear();
        self.cand_suffix_reading.clear();
    }

    /// 現在の未確定部分（hiragana_buffer）を文節列に分解する
    ///
    /// 候補選択中なら先頭文節はその選択表記、残りは1-best。
    /// 未選択なら全体を1-bestで分解する。学習の記録に使う。
    pub fn segment_remaining(&self) -> Vec<(String, String, String)> {
        if self.converter.is_none() || self.hiragana_buffer.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.cand_seg_surfaces.is_empty()
            && self.candidate_index < self.cand_seg_surfaces.len()
            && !self.cand_seg_reading.is_empty()
        {
            // 候補選択あり: 前半 + 選択した対象文節 + 後半 で分解
            for e in self.convert_buffer(&self.cand_prefix_reading) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
            let seg_surface = self.cand_seg_surfaces[self.candidate_index].clone();
            // 候補選択した語は内容語とみなす（品詞は名詞相当を既定に）
            out.push((self.cand_seg_reading.clone(), seg_surface, "名詞-一般".to_string()));
            for e in self.convert_buffer(&self.cand_suffix_reading) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
        } else if self.kana_tail_len > 0 {
            // Escで戻した末尾はかなのまま（学習しない）、前半だけ変換して学習
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(self.kana_tail_len);
            let prefix: String = self.hiragana_buffer.chars().take(keep).collect();
            for e in self.convert_buffer(&prefix) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
            let tail: String = self.hiragana_buffer.chars().skip(keep).collect();
            if !tail.is_empty() {
                out.push((tail.clone(), tail, "名詞-一般".to_string()));
            }
        } else {
            for e in self.convert_buffer(&self.hiragana_buffer) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
        }
        out
    }

    /// 確定した文節列から学習する（DB記録 + 変換エンジンのメモリ更新）
    ///
    /// - 各文節の (読み→表記) をユニグラムとして記録
    /// - 隣接する文節の (前表記→次表記) をバイグラムとして記録
    /// メモリも即時更新するので、次の変換からすぐ賢くなる。
    pub fn learn_from_segments(&mut self, segments: &[(String, String, String)]) {
        let Some(learning) = self.learning.as_ref() else {
            return;
        };
        // 1文分の記録を1トランザクションにまとめる（文ごとの fsync を避ける）
        learning.begin_batch();
        // ユニグラム（読み→表記）
        for (reading, surface, pos) in segments {
            if reading == surface {
                continue; // ひらがなそのままは学習しない
            }
            if !is_learnable_pair(reading, surface) {
                continue; // 助詞の漢字化・英字ゴミ等は学習しない
            }
            // 接尾辞（性・的・化 等）は単独ユニグラムとして学習しない。
            // 「可能性」→ 可能+性 のように複合語の一部で出るため、単独で
            // 学習すると「せい→性」が強まり「しんせい」が「しん性」に割れる。
            // 語のつながりは下のバイグラム（可能→性）で捕捉する。
            if pos.contains("接尾") {
                continue;
            }
            let _ = learning.record_commit(reading, surface, None);
            let freq = learning.find_frequency(reading, surface).unwrap_or(1);
            if let Some(conv) = self.converter.as_mut() {
                conv.learn_unigram(reading, surface, freq);
            }
        }
        // バイグラム（隣接する表記のつながり）
        // 収集だけしておき、実際の登録は関数末尾（learning の借用が終わった後）で行う
        // （register_auto_compound は &mut self で learning/converter 両方に触るため）。
        let mut auto_compound_candidates: Vec<(String, String)> = Vec::new();
        for pair in segments.windows(2) {
            let prev = &pair[0].1;
            let next = &pair[1].1;
            if prev.is_empty() || next.is_empty() {
                continue;
            }
            // 助詞の読みが絡む/英字ゴミのペアは学習しない
            if !is_learnable_pair(&pair[0].0, prev) || !is_learnable_pair(&pair[1].0, next) {
                continue;
            }
            let _ = learning.record_bigram(prev, next);
            let freq = learning.find_bigram_frequency(prev, next).unwrap_or(1);
            if let Some(conv) = self.converter.as_mut() {
                conv.learn_bigram(prev, next, freq);
            }
            // 隣接する複合語の自動登録判定（辞書に無く・両方とも漢字を
            // 含む、またはカタカナの内容語・反復して確定されている場合、
            // 1語として登録する）。カタカナも対象にするのは「メモ帳」の
            // ような外来語＋漢字の組み合わせが辞書に複合語として無い
            // ケースを拾うため（手作業での個別登録に頼らずに済むように
            // するための一般化）。
            if freq >= AUTO_COMPOUND_MIN_FREQ
                && crate::viterbi::is_content_pos(&pair[0].2)
                && crate::viterbi::is_content_pos(&pair[1].2)
                && is_registrable_compound_part(prev)
                && is_registrable_compound_part(next)
            {
                let combined_reading = format!("{}{}", pair[0].0, pair[1].0);
                if combined_reading.chars().count() <= AUTO_COMPOUND_MAX_READING_LEN {
                    let combined_surface = format!("{}{}", prev, next);
                    let existing = self
                        .converter
                        .as_ref()
                        .and_then(|c| c.merged_lookup(&combined_reading));
                    let already_exists = existing.as_ref()
                        .is_some_and(|entries| entries.iter().any(|e| e.surface == combined_surface));
                    // 同じ読みに、それらしい（1文字あたりコストが妥当な範囲の）
                    // 別表記の内容語が既に辞書に載っているなら登録しない。
                    // 「再」+「起動」の隣接確定が、過去のライブ変換の不具合で
                    // 本来「再起動」と読むべき語を「際」+「起動」に誤分割した
                    // ものだった場合、その誤分割を繰り返し確定しただけで
                    // 「際起動」が複合語として自動登録されてしまい、正しい
                    // 「再起動」と並んで予測変換に出続ける事故があった
                    // （実例）。
                    // `dictionary.lookup`は読みの完全一致を辞書全体から拾うため、
                    // 対象と無関係などこかの希少語・固有名詞がたまたま同じ読みを
                    // 持つだけでも一致してしまう。それだけで登録を諦めると、
                    // 頻繁に確定される正当な複合語の自動登録が無関係な希少語の
                    // せいで永久に効かなくなる。誤分割の事故が起きるのは対象の
                    // 読みに「それらしい（一般的に使われる）」既存語があるときに
                    // 限られるため、`is_implausible_content_word`で希少語・当て字
                    // を除外し、内容語（助詞等ではない）に絞る。
                    let has_other_coverage = existing.is_some_and(|entries| {
                        entries.iter().any(|e| {
                            e.surface != combined_surface
                                && crate::viterbi::is_content_pos(&e.pos)
                                && !crate::viterbi::is_implausible_content_word(e)
                        })
                    });
                    if !already_exists && !has_other_coverage {
                        auto_compound_candidates.push((combined_reading, combined_surface));
                    }
                }
            }
        }
        // 内容語連想（助詞・助動詞を飛ばした内容語どうしの繋がり）
        // 「会社…帰社」「新聞…記者」のように離れた語の関係を学習する。
        let mut last_content: Option<String> = None;
        for (reading, surface, pos) in segments {
            if !crate::viterbi::is_content_pos(pos) || surface == reading || surface.is_empty() {
                continue;
            }
            if !is_learnable_pair(reading, surface) {
                continue; // 助詞の漢字化・英字ゴミは連想学習しない
            }
            if let Some(prev) = &last_content {
                let _ = learning.record_assoc(prev, surface);
                let freq = learning.find_assoc_frequency(prev, surface).unwrap_or(1);
                if let Some(conv) = self.converter.as_mut() {
                    conv.learn_assoc(prev, surface, freq);
                }
            }
            last_content = Some(surface.clone());
        }
        for (reading, surface) in auto_compound_candidates {
            self.register_auto_compound(&reading, &surface);
        }
        if let Some(learning) = self.learning.as_ref() {
            learning.end_batch();
        }
    }

    /// 辞書をロード
    pub fn load_dictionary(&mut self, path: &Path) -> bool {
        debug_log!("Dictionary::load 呼び出し: {}", path.display());
        match Dictionary::load(path) {
            Ok(dict) => {
                debug_log!("辞書読み込み成功、ViterbiConverter作成中");
                let mut converter = ViterbiConverter::new(dict);
                // コーパス由来の語頻度データ（あれば）を読み込む。辞書と
                // 同じディレクトリの corpus_lm.dic を探す（無くても動作する
                // 任意のファイル。Stage 0の検証用資産）。
                let corpus_lm_path = path.with_file_name("corpus_lm.dic");
                if corpus_lm_path.exists() {
                    match crate::CorpusLm::load(&corpus_lm_path) {
                        Ok(lm) => {
                            converter.load_corpus_lm(&lm);
                            debug_log!(
                                "コーパスLMを読み込みました: {} (unigram={}, bigram={})",
                                corpus_lm_path.display(),
                                lm.unigrams.len(),
                                lm.bigrams.len()
                            );
                        }
                        Err(e) => {
                            debug_log!("コーパスLMのロードに失敗: {}", e);
                        }
                    }
                }
                self.converter = Some(converter);
                // 過去の学習をライブ変換エンジンに反映。
                //
                // 優先語彙ファイル（word_priority.tsv）より必ず先に行うこと。
                // `reload_learning_into_converter`は内部で`clear_learning`を
                // 呼び、`learned_unigram`を一度クリアしてから学習DBの内容を
                // 積み直す。`load_word_priority_file`も同じ`learned_unigram`
                // へ書き込むため、先に優先語彙を読んでからこれを呼ぶと、
                // 読み込んだ直後に丸ごと消えてしまう（起動直後から
                // word_priority.tsvが常に無効化されていた実バグ。
                // ゴールデンテストの学習有効/無効比較で発見。
                // [[golden-test-harness]]）。
                self.reload_learning_into_converter();
                // 優先語彙ファイル（辞書に既存の語同士の優先順位）を読み込む。
                // 上記の順序により、ユーザーの実学習値（DBから読み込み済み）は
                // `.or_insert`で上書きされず優先されたまま、未学習の語にだけ
                // ここでのボーナスが効く。不在・パースエラーでも起動を
                // 止めない（graceful-skip）。
                if let Some(conv) = self.converter.as_mut() {
                    let priority_path = path.with_file_name("word_priority.tsv");
                    match conv.load_word_priority_file(&priority_path) {
                        Ok(count) => {
                            debug_log!("優先語彙ファイルを読み込みました: {}件", count);
                        }
                        Err(e) => {
                            debug_log!("優先語彙ファイルの読み込みをスキップ: {}", e);
                        }
                    }
                    // 隣接コロケーションシード（`seeded_assoc`）。学習DBとは
                    // 無関係な辞書付随ファイルなので、word_priority.tsvと違い
                    // reload_learning_into_converterとの順序は問題にならない。
                    let assoc_path = path.with_file_name("word_assoc.tsv");
                    match conv.load_word_assoc_file(&assoc_path) {
                        Ok(count) => {
                            debug_log!("コロケーションシードを読み込みました: {}件", count);
                        }
                        Err(e) => {
                            debug_log!("コロケーションシードの読み込みをスキップ: {}", e);
                        }
                    }
                }
                // 判断層（judge-lm、[[jev-style-judge-direction]]）。辞書と同じ
                // ディレクトリの judge_lm.bin があれば読み込む。無効化は
                // 環境変数 IME_JUDGE=0、またはファイルを消す/リネームするだけ
                // （無ければ従来の変換と完全に同じ動作）。
                self.pending_judge = None;
                let judge_path = path.with_file_name("judge_lm.bin");
                if judge_enabled() && judge_path.exists() {
                    let load = move || match crate::judge::JudgeLm::load(&judge_path) {
                        Ok(judge) => {
                            debug_log!("判断層を読み込みました: {:?}", judge);
                            Some(std::sync::Arc::new(judge))
                        }
                        Err(e) => {
                            debug_log!("判断層の読み込みに失敗（従来の変換で続行）: {}", e);
                            None
                        }
                    };
                    if self.load_judge_async {
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(load());
                        });
                        self.pending_judge = Some(rx);
                    } else if let (Some(judge), Some(conv)) = (load(), self.converter.as_mut()) {
                        conv.set_judge(Some(judge));
                    }
                }
                // ユーザー登録の単語を辞書へ注入（辞書に無い複合語を変換可能に）
                self.inject_user_words();
                debug_log!("辞書をロードしました: {}", path.display());
                println!("辞書をロードしました: {}", path.display());
                true
            }
            Err(e) => {
                debug_log!("辞書のロードに失敗: {}", e);
                eprintln!("辞書のロードに失敗: {}", e);
                false
            }
        }
    }

    /// 文字を追加
    pub fn add_char(&mut self, ch: char) -> Option<ConversionAction> {
        if !self.enabled {
            return None;
        }

        // Escでひらがなに戻した内容は「確定済み」として扱う。
        // 新しい入力が来たらそこまでを確定し、戻したかなを再変換しない。
        // （表示済みテキストはそのまま。内部状態だけ確定して新規入力を始める）
        if self.kana_tail_len > 0 {
            self.commit();
        }

        // バッファが変わるので候補リストは無効化し、世代を進める
        self.clear_candidates();
        self.generation = self.generation.wrapping_add(1);

        self.romaji_buffer.push(ch);
        debug_log!("入力: '{}' → ローマ字バッファ: '{}'", ch, self.romaji_buffer);

        // ローマ字バッファを「確定ひらがな」と「保留ローマ字」に分割
        // （先頭に変換不能な英字が残っても以降が全てローマ字化しないよう、
        //  末尾の英字断片のみを保留にする）
        let (settled, pending) = self.romaji.split(&self.romaji_buffer);
        if !settled.is_empty() {
            // 閉じ括弧 `]`（→」）は、直前の未対応の開き括弧の種類に合わせる
            // （『 を選んでいれば 』、【 なら 】。開き括弧が無ければ 」のまま）
            let settled = self.match_closing_brackets(&settled);
            self.hiragana_buffer.push_str(&settled);
            self.romaji_buffer = pending;
        }

        debug_log!("現在の状態: ひらがな='{}', ローマ字='{}'", self.hiragana_buffer, self.romaji_buffer);

        // ひらがな→漢字変換（ライブ変換）
        self.update_conversion()
    }

    /// 入力された閉じ括弧「」」を、確定済みテキスト＋入力中の読みの中で
    /// まだ閉じられていない開き括弧に対応する閉じ括弧に置き換える。
    fn match_closing_brackets(&self, settled: &str) -> String {
        if !settled.contains('」') {
            return settled.to_string();
        }
        let mut text = format!("{}{}", self.bracket_context, self.hiragana_buffer);
        let mut out = String::with_capacity(settled.len());
        for c in settled.chars() {
            let mapped = if c == '」' {
                brackets::unmatched_opener(&text)
                    .and_then(brackets::matching_closer)
                    .unwrap_or(c)
            } else {
                c
            };
            text.push(mapped);
            out.push(mapped);
        }
        out
    }

    /// 確定1回分の学習を、遅延が有効なら溜めてフックの外で処理し、
    /// そうでなければその場で行う。遅延先（候補ウィンドウのメッセージ
    /// ループ）へ通知できなければ同期に戻す（学習を落とさない）。
    fn learn_segments_or_defer(&mut self, segments: &[(String, String, String)]) {
        if self.defer_learning {
            self.pending_learning.push(segments.to_vec());
            let notified = self.request_deferred_learning.map(|f| f()).unwrap_or(false);
            if notified {
                return;
            }
            self.pending_learning.pop();
        }
        self.learn_from_segments(segments);
    }

    /// 溜めておいた確定分の学習をまとめて処理する（メッセージループ側から呼ぶ）
    pub fn flush_pending_learning(&mut self) {
        let pending = std::mem::take(&mut self.pending_learning);
        for segments in pending {
            self.learn_from_segments(&segments);
        }
        let pending_predictions = std::mem::take(&mut self.pending_prediction_learning);
        for data in pending_predictions {
            self.apply_prediction_learning(&data);
        }
    }

    /// 予測確定1回分の学習を、遅延が有効なら溜めてフックの外で処理し、
    /// そうでなければその場で行う（`learn_segments_or_defer`と同じ考え方）。
    fn learn_prediction_or_defer(&mut self, data: PredictionLearning) {
        if self.defer_learning {
            self.pending_prediction_learning.push(data);
            let notified = self.request_deferred_learning.map(|f| f()).unwrap_or(false);
            if notified {
                return;
            }
            self.pending_prediction_learning.pop();
        } else {
            self.apply_prediction_learning(&data);
        }
    }

    /// `commit_prediction` で選んだ予測（誤字修正・履歴補完）を実際にDBへ記録する。
    fn apply_prediction_learning(&mut self, data: &PredictionLearning) {
        let Some(learning) = self.learning.as_ref() else {
            return;
        };
        if let Some((reading, surface)) = &data.unigram {
            let _ = learning.record_commit(reading, surface, None);
            let freq = learning.find_frequency(reading, surface).unwrap_or(1);
            if let Some(conv) = self.converter.as_mut() {
                conv.learn_unigram(reading, surface, freq);
            }
        }
        if let Some((prev_surface, surface)) = &data.bigram {
            let _ = learning.record_bigram(prev_surface, surface);
            let freq = learning
                .find_bigram_frequency(prev_surface, surface)
                .unwrap_or(1);
            if let Some(conv) = self.converter.as_mut() {
                conv.learn_bigram(prev_surface, surface, freq);
            }
        }
        if let Some((original_reading, reading, surface)) = &data.typo_correction {
            let _ = learning.record_typo_correction(original_reading, reading, surface);
            let freq = learning
                .find_typo_correction_frequency(original_reading, reading, surface)
                .unwrap_or(1);
            if let Some(conv) = self.converter.as_mut() {
                conv.learn_typo_correction(original_reading, reading, surface, freq);
            }
            debug_log!(
                "誤字修正を学習: '{}' → 読み='{}' 表記='{}'",
                original_reading, reading, surface
            );
        }
    }

    /// 確定したテキストを閉じ括弧の自動対応用に蓄積する（末尾500文字）
    fn push_bracket_context(&mut self, committed: &str) {
        if committed.is_empty() {
            return;
        }
        self.bracket_context.push_str(committed);
        const KEEP: usize = 500;
        let len = self.bracket_context.chars().count();
        if len > KEEP {
            self.bracket_context = self.bracket_context.chars().skip(len - KEEP).collect();
        }
    }

    /// バックスペース処理
    pub fn backspace(&mut self) -> Option<ConversionAction> {
        if !self.enabled {
            return None;
        }

        self.clear_candidates();
        self.generation = self.generation.wrapping_add(1);
        self.kana_tail_len = 0;

        if !self.romaji_buffer.is_empty() {
            // 入力途中のローマ字は1文字ずつ削除
            self.romaji_buffer.pop();
        } else if !self.hiragana_buffer.is_empty() {
            // 変換済みの部分は「最後の変換単語（文節）」ごと削除する
            let last_len = self
                .convert_buffer(&self.hiragana_buffer)
                .last()
                .map(|e| e.reading.chars().count())
                .filter(|&n| n > 0)
                .unwrap_or(1);
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(last_len);
            self.hiragana_buffer = self.hiragana_buffer.chars().take(keep).collect();
        } else {
            return None; // 削除するものがない
        }

        self.truncate_pinned_to_buffer();
        debug_log!("バックスペース後: ひらがな='{}', ローマ字='{}'", self.hiragana_buffer, self.romaji_buffer);
        self.update_conversion()
    }

    /// 読み `reading` をライブ表示と同じ分解で変換する（固定した先頭文節
    /// `pinned` を反映。`reading` の先頭と一致しない固定文節は無視される）。
    /// 表示・候補一覧・Backspace・学習のすべてがこの分解を共有する。
    pub fn convert_buffer(&self, reading: &str) -> Vec<crate::WordEntry> {
        match self.converter.as_ref() {
            Some(c) => c.convert_context_aware_pinned(reading, &self.pinned),
            None => Vec::new(),
        }
    }

    /// 固定した先頭文節を、現在の読みバッファの先頭と一致する範囲に切り詰める
    /// （Backspace で固定領域まで削った場合）
    fn truncate_pinned_to_buffer(&mut self) {
        let n = crate::viterbi::matching_pinned_prefix(&self.hiragana_buffer, &self.pinned).len();
        self.pinned.truncate(n);
    }

    /// 読みが長くなったら、文節の切れ目で先頭側の文節を固定する
    /// （`stable_prefix_segments`。固定済みより後ろまで伸びるときだけ更新）
    fn extend_pinned(&mut self, entries: &[crate::WordEntry]) {
        let n = crate::viterbi::stable_prefix_segments(
            entries,
            crate::viterbi::STABILIZE_MIN_READING_CHARS,
            crate::viterbi::STABILIZE_KEEP_TAIL_CHARS,
        );
        if n > self.pinned.len() {
            self.pinned = entries[..n].to_vec();
            debug_log!(
                "先頭文節を固定: '{}' ({}文節)",
                self.pinned.iter().map(|e| e.surface.as_str()).collect::<String>(),
                n
            );
        }
    }

    /// 変換を更新（macOS方式：ひらがな確定時のみ漢字変換）
    ///
    /// 共通プレフィックスを保持する差分計算で必要最小限の編集のみを送信する。
    /// これにより「今日」→「今日h」のように先頭が共通な場合は cursor を動かさず
    /// 末尾だけ更新できる。旧実装(毎回全削除→再挿入)は cursor が頻繁に左に飛び、
    /// 視覚的に「後の入力が前を上書きする」ように見える原因だった。
    pub fn update_conversion(&mut self) -> Option<ConversionAction> {
        self.attach_pending_judge();
        // ひらがなバッファのみを漢字変換
        // ローマ字バッファはそのまま末尾に追加
        let converted_hiragana = if self.converter.is_none() {
            // 辞書がない場合はひらがなのまま
            debug_log!("辞書なし: ひらがなのまま '{}'", self.hiragana_buffer);
            self.hiragana_buffer.clone()
        } else if self.hiragana_buffer.is_empty() {
            String::new()
        } else if self.kana_tail_len > 0 {
            // Escで戻した末尾はひらがなのまま、前半だけ変換する
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(self.kana_tail_len);
            let prefix: String = self.hiragana_buffer.chars().take(keep).collect();
            let tail: String = self.hiragana_buffer.chars().skip(keep).collect();
            let conv: String = if prefix.is_empty() {
                String::new()
            } else {
                self.convert_buffer(&prefix).iter().map(|e| e.surface.as_str()).collect()
            };
            format!("{}{}", conv, tail)
        } else {
            // 文脈（内容語の繋がり）と固定した先頭文節を考慮して変換する。
            // 読みが長くなったら先頭側の文節を固定し、以降の入力で前方が
            // 書き換わらないようにする（`stabilize.rs`）。
            let entries = self.convert_buffer(&self.hiragana_buffer);
            self.extend_pinned(&entries);
            entries.iter().map(|e| e.surface.as_str()).collect()
        };

        // 変換結果 + ローマ字（未確定）
        let new_result = format!("{}{}", converted_hiragana, self.romaji_buffer);
        self.apply_new_result(new_result)
    }

    /// 表示テキストを new_result に差し替えるための差分アクションを作る
    pub fn apply_new_result(&mut self, new_result: String) -> Option<ConversionAction> {
        if new_result == self.conversion_result {
            debug_log!("変化なし: アクションなし");
            return None;
        }

        // 共通プレフィックスを文字単位で計算
        let old_chars: Vec<char> = self.conversion_result.chars().collect();
        let new_chars: Vec<char> = new_result.chars().collect();
        let common = old_chars
            .iter()
            .zip(new_chars.iter())
            .take_while(|(a, b)| a == b)
            .count();

        let delete_count = old_chars.len() - common;
        let insert_text: String = new_chars[common..].iter().collect();

        debug_log!(
            "差分: '{}' → '{}' (共通={}, 削除={}, 挿入='{}')",
            self.conversion_result, new_result, common, delete_count, insert_text
        );

        self.conversion_result = new_result;
        self.last_sent_length = new_chars.len();

        if delete_count == 0 && insert_text.is_empty() {
            return None;
        }

        Some(ConversionAction {
            delete_count,
            insert_text,
        })
    }

    /// 次/前の変換候補に切り替える（Tab / Space）
    ///
    /// 初回呼び出し時に N-best 候補を生成し、以降は循環する。
    pub fn cycle_candidate(&mut self, backwards: bool) -> Option<ConversionAction> {
        if !self.enabled || self.hiragana_buffer.is_empty() || self.converter.is_none() {
            return None;
        }

        if self.candidates.is_empty() {
            // 一覧を初めて開いたとき: 候補1(index 0)を選択状態にして表示する
            // （表示中のライブ変換結果が候補1と一致しないことがあるため、
            //  最初の Tab で候補1へ切り替える。次の Tab から順送りになる）
            let (
                candidates,
                seg_reading,
                seg_surfaces,
                prefix_surface,
                prefix_reading,
                suffix_surface,
                suffix_reading,
            ) = self.build_candidates();
            if candidates.len() < 2 {
                return None; // 切り替える候補がない
            }
            self.candidates = candidates;
            self.cand_seg_reading = seg_reading;
            self.cand_seg_surfaces = seg_surfaces;
            self.cand_prefix_surface = prefix_surface;
            self.cand_prefix_reading = prefix_reading;
            self.cand_suffix_surface = suffix_surface;
            self.cand_suffix_reading = suffix_reading;
            self.candidate_index = 0;
            return self.select_candidate(0);
        }

        let len = self.candidates.len();
        let next = if backwards {
            (self.candidate_index + len - 1) % len
        } else {
            (self.candidate_index + 1) % len
        };
        self.select_candidate(next)
    }

    /// 候補一覧で選択中の候補について「誤学習」をリセットする（Delete キー）。
    ///
    /// その候補の対象文節の (読み, 表記) の学習だけを DB とメモリの両方から消し、
    /// 候補を作り直して並び順を更新する。過去に誤って確定して上位に居座っていた
    /// 変換を、その1件だけ取り消せる（同じ読みの他の正しい学習は残る）。
    /// 表示を更新するアクションを返す（候補が無くなれば None）。
    pub fn reset_learning_for_selected(&mut self) -> Option<ConversionAction> {
        if !self.enabled || self.candidates.is_empty() {
            return None;
        }
        let idx = self.candidate_index.min(self.candidates.len() - 1);
        let reading = self.cand_seg_reading.clone();
        let surface = self.cand_seg_surfaces.get(idx).cloned()?;
        if reading.is_empty() || surface.is_empty() {
            return None;
        }

        // DB（確定履歴＋バイグラム）とメモリ（ユニグラム等）の両方から消す。
        if let Some(learning) = self.learning.as_ref() {
            let _ = learning.forget_commit(&reading, &surface);
        }
        if let Some(conv) = self.converter.as_mut() {
            conv.forget_unigram(&reading, &surface);
        }
        debug_log!("誤学習リセット: 読み='{}' 表記='{}'", reading, surface);

        // 学習が変わったので候補を作り直す（並び順が更新される）。
        let (
            candidates,
            seg_reading,
            seg_surfaces,
            prefix_surface,
            prefix_reading,
            suffix_surface,
            suffix_reading,
        ) = self.build_candidates();
        if candidates.is_empty() {
            self.candidates.clear();
            return None;
        }
        self.candidates = candidates;
        self.cand_seg_reading = seg_reading;
        self.cand_seg_surfaces = seg_surfaces;
        self.cand_prefix_surface = prefix_surface;
        self.cand_prefix_reading = prefix_reading;
        self.cand_suffix_surface = suffix_surface;
        self.cand_suffix_reading = suffix_reading;
        // 先頭（学習リセット後の最有力候補）を選択して表示を更新する。
        self.candidate_index = 0;
        self.select_candidate(0)
    }

    /// Escで、まだ変換されている末尾の文節を一つ、ひらがなに戻す
    ///
    /// 呼ぶたびに末尾から一文節ずつ戻していく（累積）。戻す対象が
    /// 残っていれば表示を更新するアクションを返し、全てひらがなに
    /// 戻し終えていれば `None`（呼び出し側は取消にフォールバック）。
    pub fn extend_kana_revert(&mut self) -> Option<ConversionAction> {
        if !self.enabled || self.hiragana_buffer.is_empty() {
            return None;
        }
        let total = self.hiragana_buffer.chars().count();
        if self.kana_tail_len >= total {
            return None; // すべてひらがなに戻し済み
        }
        // まだ変換されている前半 = 先頭から (total - kana_tail_len) 文字
        let keep = total - self.kana_tail_len;
        let prefix: String = self.hiragana_buffer.chars().take(keep).collect();
        if self.converter.is_none() {
            return None;
        }
        let entries = self.convert_buffer(&prefix);
        // 前半の「最後の変換された文節」以降を、ひらがな末尾に加える
        let revert_len: usize = if let Some(idx) =
            entries.iter().rposition(|e| e.surface != e.reading)
        {
            entries[idx..].iter().map(|e| e.reading.chars().count()).sum()
        } else {
            // 変換済み文節が無ければ残り全部をかなに
            keep
        };
        self.kana_tail_len = (self.kana_tail_len + revert_len.max(1)).min(total);
        self.clear_candidates();
        self.update_conversion()
    }

    /// 候補一覧を組み立てる（直近＝最後の文節の同音語をコスト＋学習頻度順に）
    ///
    /// 候補の対象は「一番最後に打った文節」。前半（それより前）は
    /// 変換済みのまま固定し、最後の文節だけを差し替える。これにより
    /// Tab を押しても前の文が変わらず、直近で入力した語だけを選べる。
    ///
    /// 戻り値: (候補文字列, 対象文節の読み, 各候補の対象文節表記,
    ///          前半の変換済み表記, 前半の読み, 後半の変換済み表記, 後半の読み)
    pub fn build_candidates(
        &self,
    ) -> (Vec<String>, String, Vec<String>, String, String, String, String) {
        let empty = || {
            (
                Vec::new(),
                String::new(),
                Vec::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            )
        };
        let Some(converter) = self.converter.as_ref() else {
            return empty();
        };

        // ライブ表示と同じ分解にする（固定した先頭文節 `self.pinned` も反映）。
        // ここを素の`convert_context_aware`にすると、固定済みの先頭文節を
        // 無視して読み全体を再変換してしまい、`select_candidate`が表示中の
        // 固定文節と異なる語（同音語）で上書きしてしまうことがある。
        let entries = self.convert_buffer(&self.hiragana_buffer);
        if entries.is_empty() {
            return empty();
        }

        // 末尾が括弧なら、同音語ではなく括弧の種類（「」『』【】（）…）の
        // 切替候補を出す（`brackets.rs`）。選択時は `select_candidate` が
        // 読みバッファ内の括弧文字も書き換える。
        if let Some(current) = entries.last().and_then(|e| brackets::single_bracket_char(&e.surface)) {
            let n = entries.len() - 1;
            let prefix_surface: String = entries[..n].iter().map(|e| e.surface.as_str()).collect();
            let prefix_reading: String = entries[..n].iter().map(|e| e.reading.as_str()).collect();
            let seg_surfaces: Vec<String> = brackets::bracket_variants(current)
                .into_iter()
                .map(|c| c.to_string())
                .collect();
            let candidates = seg_surfaces
                .iter()
                .map(|s| format!("{}{}", prefix_surface, s))
                .collect();
            return (
                candidates,
                current.to_string(),
                seg_surfaces,
                prefix_surface,
                prefix_reading,
                String::new(),
                String::new(),
            );
        }

        // 対象は「最後の“変換された”文節」。表層==読み（＝ひらがなのまま）の
        // 文節は変換とみなさず飛ばす。主に 平仮名→漢字/カタカナ を拾うため。
        // 変換済みが1つも無ければ最後の文節を対象にする。
        let end = entries
            .iter()
            .rposition(|e| e.surface != e.reading)
            .unwrap_or(entries.len() - 1);
        // 隣接する1文字漢字は分割された複合語のことが多いので、対象を
        // 左へ広げて連続する1文字漢字の文節をまとめて変換対象にする
        //（例: 変+換 が別文節でも「へんかん」全体を対象にする）。
        let mut start = end;
        // 「５＋時」のような数詞＋助数詞も一単位として選び直す。
        // 「じ」だけを検索すると「ごじ→誤字」などの同音語が候補から消える。
        if start > 0 && entries[end].pos.starts_with("名詞-接尾-助数詞") {
            while start > 0 && entries[start - 1].pos.starts_with("名詞-数") {
                start -= 1;
            }
        }
        while start > 0
            && is_single_kanji_entry(&entries[start])
            && is_single_kanji_entry(&entries[start - 1])
        {
            start -= 1;
        }
        // 自動変換が記号や短い語に誤分割していても、読み全体に実在する
        // 複合語を候補一覧の対象にする（例: まんかい→万Χ ではなく満開）。
        // 先頭から最大4文節までに限定し、長文の無関係な結合を避ける。
        for candidate_start in 0..=end {
            if end - candidate_start > 3 {
                continue;
            }
            let reading: String = entries[candidate_start..=end]
                .iter().map(|e| e.reading.as_str()).collect();
            let has_real_word = converter.merged_lookup(&reading)
                .is_some_and(|words| words.iter().any(|w| !w.pos.starts_with("記号")));
            if has_real_word {
                start = candidate_start;
                break;
            }
        }
        // 対象span [start..=end] の結合読み
        let seg_reading: String =
            entries[start..=end].iter().map(|e| e.reading.as_str()).collect();

        // 前半（対象より前）と後半（対象より後ろ＝末尾の平仮名など）
        let prefix_surface: String =
            entries[..start].iter().map(|e| e.surface.as_str()).collect();
        let prefix_reading: String =
            entries[..start].iter().map(|e| e.reading.as_str()).collect();
        let suffix_surface: String =
            entries[end + 1..].iter().map(|e| e.surface.as_str()).collect();
        let suffix_reading: String =
            entries[end + 1..].iter().map(|e| e.reading.as_str()).collect();

        // 対象文節の同音語を集め、実際にその位置に置いたときの文全体コストで
        // 並べる。以前は対象語だけの辞書コスト＋ペナルティ－学習ボーナスという
        // 独自の簡易式（前後の語との接続コストを一切見ない）で並べていたため、
        // 自動変換（文全体のViterbi）の1-bestと候補一覧の1位が食い違ったり、
        // 接続コストの都合で本来は選ばれない低頻度語が候補一覧の上位に出る
        // ことがあった。ここでは`effective_word_cost`（学習ユニグラム・
        // コーパス頻度・1文字漢字ペナルティ込みの単語自身のコスト）に、
        // 前後の確定済み文節との接続コスト（`context_connection_cost`。
        // 学習/コーパスのバイグラムボーナスを含む、自動変換と同じ計算式）を
        // 加えた合計で並べる。これにより自動変換の1-bestと候補一覧の1位が
        // 構造的に一致する（ただし`scoring.rs`のガード関数群は含まない簡易版
        // であり、完全に同一のロジックではない）。
        struct Seg {
            surface: String,
            eff_cost: i32,
        }
        let prev_entry = if start > 0 { entries.get(start - 1) } else { None };
        let next_entry = entries.get(end + 1);
        let mut segs: Vec<Seg> = Vec::new();
        if let Some(words) = converter.merged_lookup(&seg_reading) {
            for w in words.iter() {
                // 記号品詞（ギリシャ文字等）は辞書コストが低くても実用語より
                // 優先させない（scoring.rsの`symbol_penalty`はASCII/全角記号
                // だけを対象にしており、χ等の記号POS全般はカバーしないため
                // ここで別途ペナルティを科す）。
                let penalty = symbol_entry_penalty(&w.pos);
                let own_cost = converter.effective_word_cost(w).saturating_add(penalty);
                let ctx_cost = converter.context_connection_cost(prev_entry, w, next_entry);
                segs.push(Seg {
                    surface: w.surface.clone(),
                    eff_cost: own_cost.saturating_add(ctx_cost),
                });
            }
        }
        // カタカナ・ひらがな表記も候補に含める（末尾寄り）
        let katakana = crate::hiragana_to_katakana(&seg_reading);
        if katakana != seg_reading {
            segs.push(Seg { surface: katakana, eff_cost: 50000 });
        }
        segs.push(Seg { surface: seg_reading.clone(), eff_cost: 52000 });

        segs.sort_by(|a, b| a.eff_cost.cmp(&b.eff_cost));

        // 前半（固定）+ 対象文節表記 + 後半（固定）で候補文字列を作る
        let mut seen = std::collections::HashSet::new();
        let mut candidates: Vec<String> = Vec::new();
        let mut seg_surfaces: Vec<String> = Vec::new();
        for seg in segs {
            let cand = format!("{}{}{}", prefix_surface, seg.surface, suffix_surface);
            if seen.insert(cand.clone()) {
                candidates.push(cand);
                seg_surfaces.push(seg.surface);
            }
            // 同音異義語は読みによっては20語以上あり（「けん」「こう」等）、
            // 9件では目的の字まで辿り着けないことが多い。多めに集めておき、
            // ポップアップ側は画面に収まる範囲をスクロールして全件を選べるようにする。
            if candidates.len() >= MAX_CONVERSION_CANDIDATES {
                break;
            }
        }
        (
            candidates,
            seg_reading,
            seg_surfaces,
            prefix_surface,
            prefix_reading,
            suffix_surface,
            suffix_reading,
        )
    }

    /// 指定インデックスの候補を選択して表示を更新する（番号キー選択）
    pub fn select_candidate(&mut self, index: usize) -> Option<ConversionAction> {
        if index >= self.candidates.len() {
            return None;
        }
        self.candidate_index = index;

        // 候補の対象文節が固定した先頭文節に掛かる場合は、そこから先の固定を外す
        // （固定分解と表示が食い違わないように）
        let keep = crate::viterbi::matching_pinned_prefix(&self.cand_prefix_reading, &self.pinned).len();
        self.pinned.truncate(keep);

        debug_log!(
            "候補選択: {}/{} '{}'",
            index + 1, self.candidates.len(), self.candidates[index]
        );

        // 括弧の種類切替: 表示だけでなく読みバッファ内の括弧文字も置き換える。
        // 括弧は変換を素通りする文字なので、バッファを書き換えれば以降の
        // 再変換（次の打鍵）でも選んだ種類が維持され、閉じ括弧の自動対応
        // （`match_closing_brackets`）もこの種類を見る。
        if brackets::single_bracket_char(&self.cand_seg_reading).is_some() {
            if let Some(new_c) = self
                .cand_seg_surfaces
                .get(index)
                .and_then(|s| brackets::single_bracket_char(s))
            {
                let pos = self.cand_prefix_reading.chars().count();
                let mut chars: Vec<char> = self.hiragana_buffer.chars().collect();
                if let Some(slot) = chars.get_mut(pos) {
                    if brackets::single_bracket_char(&slot.to_string()).is_some() {
                        *slot = new_c;
                        self.hiragana_buffer = chars.into_iter().collect();
                    }
                }
            }
        }

        let new_result = format!("{}{}", self.candidates[index], self.romaji_buffer);
        self.apply_new_result(new_result)
    }

    /// 先頭の1語を部分確定する。
    ///
    /// 「前半の変換は正しいが後半が違う」場合に、正しい前半を語単位で
    /// 順に確定していくための操作。現在は横矢印をカーソル移動に使うため
    /// 未割り当て（将来別キーに割り当てる可能性があるので残置）。
    #[allow(dead_code)]
    pub fn commit_first_word(&mut self) -> Vec<ConversionAction> {
        let mut actions = Vec::new();
        if !self.enabled || self.hiragana_buffer.is_empty() {
            return actions;
        }
        if self.converter.is_none() {
            return actions;
        }

        let entries = self.convert_buffer(&self.hiragana_buffer);
        let Some(first) = entries.first() else {
            return actions;
        };
        let surface = first.surface.clone();
        let reading_len = first.reading.chars().count();

        // 候補選択中など、表示が1-bestと異なる場合は一旦1-best表示に戻す
        // （そうしないと画面上の先頭と確定する語がずれる）
        if !self.conversion_result.starts_with(&surface) {
            let full: String = entries.iter().map(|e| e.surface.as_str()).collect();
            let full = format!("{}{}", full, self.romaji_buffer);
            if let Some(action) = self.apply_new_result(full) {
                actions.push(action);
            }
        }

        // 先頭語を管理対象（未確定領域）から外す
        self.conversion_result = self
            .conversion_result
            .strip_prefix(&surface)
            .unwrap_or("")
            .to_string();
        self.last_sent_length = self.conversion_result.chars().count();
        self.hiragana_buffer = self.hiragana_buffer.chars().skip(reading_len).collect();
        self.pinned.clear();
        self.clear_candidates();

        debug_log!(
            "部分確定: '{}' / 残り読み='{}'",
            surface, self.hiragana_buffer
        );

        // 部分確定した文節を記録（最終確定時にユニグラム/バイグラム/連想学習へ）
        self.committed_segments
            .push((first.reading.clone(), surface.clone(), first.pos.clone()));

        // 残り部分を単独で再変換（文脈が変わるため結果が変わり得る）
        if let Some(action) = self.update_conversion() {
            actions.push(action);
        }
        actions
    }

    /// 確定（Enter・句読点）
    ///
    /// 未確定のローマ字 'n' が残っていれば「ん」として取り込んでから確定する。
    pub fn commit(&mut self) -> Option<ConversionAction> {
        if !self.enabled || (self.conversion_result.is_empty() && !self.is_composing()) {
            return None;
        }

        // 末尾の未確定 'n' を「ん」に変換して表示を更新
        let action = if self.romaji_buffer == "n" {
            self.romaji_buffer.clear();
            self.hiragana_buffer.push('ん');
            if !self.candidates.is_empty() {
                // 候補選択中なら選択を維持したまま「ん」を足す
                let new_result = format!("{}ん", self.candidates[self.candidate_index]);
                self.apply_new_result(new_result)
            } else {
                self.update_conversion()
            }
        } else {
            None
        };

        // 学習: 確定した文全体を文節列に分解し、ユニグラム＋バイグラムを
        // 記録する。→ で部分確定済みの文節も連結して1文として学習する。
        // これによりライブ変換自体が使うほど賢くなり、語のつながり
        // （文全体の整合性）も学習される。
        let mut segments = std::mem::take(&mut self.committed_segments);
        segments.extend(self.segment_remaining());
        self.learn_segments_or_defer(&segments);

        // Escでひらがなに戻した末尾は「この読みはひらがな優先」として学習。
        // 次回から その読みをひらがなのまま出しやすくする（例: したい）。
        if self.kana_tail_len > 0 {
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(self.kana_tail_len);
            let tail: String = self.hiragana_buffer.chars().skip(keep).collect();
            // 1文字（て・い 等の断片）は学習しない。単一かなをひらがな優先に
            // すると「ていけい→てい系」のように語頭が未変換になって壊れる。
            if tail.chars().count() >= 2 {
                let freq = if let Some(learning) = self.learning.as_ref() {
                    let _ = learning.record_hiragana_pref(&tail);
                    // その読みの漢字/カタカナ学習を忘れる（ひらがなを勝たせる）
                    let _ = learning.forget_reading(&tail);
                    learning.find_hiragana_pref(&tail).unwrap_or(1)
                } else {
                    0
                };
                if freq > 0 {
                    if let Some(conv) = self.converter.as_mut() {
                        conv.forget_reading(&tail);
                        conv.learn_hiragana(&tail, freq);
                    }
                }
            }
        }

        // 直近の確定テキストを文脈として蓄積（LLM変換に渡す。末尾60文字）
        let committed: String = segments.iter().map(|(_, s, _)| s.as_str()).collect();
        if !committed.is_empty() {
            self.recent_context.push_str(&committed);
            let chars: Vec<char> = self.recent_context.chars().collect();
            if chars.len() > 60 {
                self.recent_context = chars[chars.len() - 60..].iter().collect();
            }

            // 確定後の復元用リングバッファへ積む。
            let reading: String = segments.iter().map(|(r, _, _)| r.as_str()).collect();
            // n-bestはreadingに対して独立に再計算する（後続文脈は使わない、
            // 復元ロジックの最初の段階。[[commit-ring-buffer]]）。
            let candidates = self
                .converter
                .as_ref()
                .map(|c| c.n_best_strings(&reading, COMMIT_RING_N_BEST))
                .unwrap_or_default();
            if self.commit_ring.len() >= COMMIT_RING_CAPACITY {
                self.commit_ring.pop_front();
            }
            self.commit_ring.push_back(CommittedEntry {
                char_count: committed.chars().count(),
                reading,
                surface: committed.clone(),
                candidates,
            });
        }
        self.push_bracket_context(&committed);
        // 次単語予測のため、最後の文節の表記を覚える
        if let Some((_, s, _)) = segments.last() {
            self.last_committed = s.clone();
        }

        // バッファをクリア（表示済みテキストはそのまま確定扱い）
        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.conversion_result.clear();
        self.last_sent_length = 0;
        self.committed_segments.clear();
        self.pinned.clear();
        self.kana_tail_len = 0;
        self.generation = self.generation.wrapping_add(1);
        self.clear_candidates();
        // 確定直後は次単語予測を用意する
        self.update_predictions();

        action
    }

    /// キャンセル（Escキー）
    pub fn cancel(&mut self) -> Option<ConversionAction> {
        if !self.enabled {
            return None;
        }

        let delete_count = self.last_sent_length;

        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.conversion_result.clear();
        self.last_sent_length = 0;
        self.committed_segments.clear();
        self.pinned.clear();
        self.kana_tail_len = 0;
        self.generation = self.generation.wrapping_add(1);
        self.clear_candidates();

        if delete_count > 0 {
            Some(ConversionAction {
                delete_count,
                insert_text: String::new(),
            })
        } else {
            None
        }
    }

    /// 変換が進行中かどうか
    pub fn is_composing(&self) -> bool {
        !self.romaji_buffer.is_empty() || !self.hiragana_buffer.is_empty()
    }
}

/// 変換アクション（何を削除して何を挿入するか）
pub struct ConversionAction {
    pub delete_count: usize,
    pub insert_text: String,
}

/// 単語エントリが「1文字の漢字」か（隣接漢字の結合判定に使う）
pub fn is_single_kanji_entry(e: &crate::WordEntry) -> bool {
    let mut chars = e.surface.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => {
            ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
        }
        _ => false,
    }
}

/// 記号品詞（ギリシャ文字等の「記号-アルファベット」等）の候補に対する
/// コストペナルティ（候補並べ替え専用。辞書自体は変更しない）。
///
/// 例:「かい」でχ（記号-アルファベット, コスト1730）が、単語コストに
/// 1文字漢字ペナルティが乗った実在の漢字候補（介・階 等、7000超）より
/// 上位に来てしまう。IPA辞書は数式表記等での使用を想定した低コストで、
/// 通常の日本語入力で先頭に出てくるのは望ましくないため、候補一覧では
/// 大きく下げる（学習頻度があればそちらが優先されるので、意図して
/// よく使う場合は上位に出せる）。
pub fn symbol_entry_penalty(pos: &str) -> i32 {
    if pos.starts_with("記号") {
        20000
    } else {
        0
    }
}

/// 単語登録の読み欄として妥当か（ひらがな＋長音符のみ）
pub fn is_hiragana_reading(reading: &str) -> bool {
    reading
        .chars()
        .all(|c| ('\u{3041}'..='\u{3096}').contains(&c) || c == 'ー')
}

/// 表記に漢字（CJK統合漢字）が1文字以上含まれるか。
/// 隣接複合語の自動登録を「漢字を含む内容語どうし」に絞るための判定
/// （助詞やひらがなだけの語を巻き込まないため）。
pub fn contains_kanji(surface: &str) -> bool {
    surface.chars().any(|c| ('\u{4E00}'..='\u{9FAF}').contains(&c))
}

/// 表記が（1文字以上の）カタカナだけで構成されているか。
fn is_all_katakana(surface: &str) -> bool {
    !surface.is_empty()
        && surface
            .chars()
            .all(|c| ('\u{30A1}'..='\u{30FA}').contains(&c) || c == 'ー')
}

/// 自動複合語登録の対象になり得る表記か（漢字を含む、またはカタカナの
/// 外来語）。「メモ帳」のようにカタカナ語＋漢字の組み合わせが辞書に
/// 複合語として無いケースを拾うため、漢字限定だった判定にカタカナを
/// 追加している。ひらがなだけの語（助詞・連体詞「この」等）は対象外の
/// まま（「この」+特定の名詞をペアごとに複合語登録するのは、際限なく
/// 語彙が増える上に本来の問題＝連体詞の接続コストを直さないため）。
pub fn is_registrable_compound_part(surface: &str) -> bool {
    contains_kanji(surface) || is_all_katakana(surface)
}

/// この (読み, 表記) ペアを学習してよいか（`crate::is_learnable_pair`。
/// `crates/dict-builder`のコーパスLM集計とも共有する）。
pub use crate::is_learnable_pair;

/// ポップアップの一候補を最大24文字に収める（UTF-8の途中で切らない）。
fn compact_prediction_text(text: &str) -> String {
    if text.chars().count() <= 24 {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(23).collect::<String>())
    }
}

/// 共通の前後を除いて変更部分だけを示す。採用用の全文はpredictionsに保持する。
fn is_insertion_only(original: &str, proposed: &str) -> bool {
    let prefix_bytes: usize = original.chars().zip(proposed.chars())
        .take_while(|(a, b)| a == b).map(|(c, _)| c.len_utf8()).sum();
    proposed.len() > original.len() && proposed[prefix_bytes..].ends_with(&original[prefix_bytes..])
}

fn prediction_change_label(original: &str, proposed: &str) -> String {
    let old: Vec<char> = original.chars().collect();
    let new: Vec<char> = proposed.chars().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..].iter().rev().zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b).count();
    let before: String = old[prefix..old.len() - suffix].iter().collect();
    let after: String = new[prefix..new.len() - suffix].iter().collect();
    if before.is_empty() {
        format!("＋{}", compact_prediction_text(&after))
    } else if after.is_empty() {
        format!("{} を削除", compact_prediction_text(&before))
    } else {
        format!("{} → {}", compact_prediction_text(&before), compact_prediction_text(&after))
    }
}

#[cfg(test)]
mod build_candidates_tests {
    use super::*;

    /// 学習ユニグラム（頻出語プリセット含む）が、Tab候補一覧の並び順にも
    /// 反映されることを確認する。修正前は build_candidates が辞書コスト＋
    /// 1文字漢字ペナルティのみで並べていたため、ライブ変換の1-bestでは
    /// 正しく選べている頻出語が、たまたま辞書コストの低い稀な語に候補
    /// 一覧では負けてしまっていた。
    #[test]
    fn learned_unigram_bonus_ranks_common_word_first() {
        let mut dict = Dictionary::new();
        dict.matrix = crate::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        // 「稀語」は辞書コストがたまたま低く設定されている想定（IPA辞書に
        // ありがちな癖の再現）。「常用語」は使用頻度は高いがコストは高め。
        dict.add_word(crate::WordEntry {
            surface: "常用語".to_string(), reading: "きしゃ".to_string(),
            left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "稀語".to_string(), reading: "きしゃ".to_string(),
            left_id: 1, right_id: 1, cost: 3000, pos: "名詞".to_string(),
        });
        let mut converter = ViterbiConverter::new(dict);
        // 「常用語」は既に何度か使われて学習済み（頻出語プリセットと同じ経路）
        converter.learn_unigram("きしゃ", "常用語", 3);

        let mut state = LiveConversionState::new();
        state.converter = Some(converter);
        state.hiragana_buffer = "きしゃ".to_string();

        let (candidates, _seg_reading, seg_surfaces, ..) = state.build_candidates();
        assert_eq!(seg_surfaces.first().map(String::as_str), Some("常用語"));
        assert_eq!(candidates.first().map(String::as_str), Some("常用語"));
    }

    /// 記号品詞（ギリシャ文字等）が、辞書コストの低さだけでTab候補一覧の
    /// 先頭に来ないことを確認する（例:「かい」でχが介・階等の実在の
    /// 漢字候補より上に出てしまっていた回帰）。
    #[test]
    fn symbol_entry_does_not_rank_above_real_kanji_word() {
        let mut dict = Dictionary::new();
        dict.matrix = crate::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        // χ のように記号品詞かつ辞書コストが極端に低いエントリを再現
        dict.add_word(crate::WordEntry {
            surface: "χ".to_string(), reading: "かい".to_string(),
            left_id: 1, right_id: 1, cost: 1730, pos: "記号-アルファベット-*-*".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "介".to_string(), reading: "かい".to_string(),
            left_id: 1, right_id: 1, cost: 5608, pos: "名詞-サ変接続-*-*".to_string(),
        });
        let converter = ViterbiConverter::new(dict);

        let mut state = LiveConversionState::new();
        state.converter = Some(converter);
        state.hiragana_buffer = "かい".to_string();

        let (candidates, ..) = state.build_candidates();
        assert_ne!(candidates.first().map(String::as_str), Some("χ"), "記号が先頭に来てはいけない");
    }

    /// 前の文節との接続コストが候補一覧の並び順に反映されることを確認する
    /// （修正前は対象語だけの辞書コストで並べており、前後の文脈を一切
    /// 見ていなかった）。「硬貨」は単語コスト自体はわずかに安いが、直前の
    /// 「為替」との接続コストが「効果」よりずっと高い設定にしてあるため、
    /// 文全体では「効果」の方が自然（自動変換の1-bestも効果になる）。
    /// 候補一覧の1位もそれと一致するべき。
    #[test]
    fn context_connection_cost_affects_candidate_order() {
        let mut dict = Dictionary::new();
        dict.matrix = crate::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        dict.matrix.set(1, 2, 50); // 為替→効果: 自然な接続
        dict.matrix.set(1, 3, 9000); // 為替→硬貨: 不自然な接続
        dict.add_word(crate::WordEntry {
            surface: "為替".to_string(), reading: "かわせ".to_string(),
            left_id: 1, right_id: 1, cost: 3000, pos: "名詞".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "効果".to_string(), reading: "こうか".to_string(),
            left_id: 2, right_id: 2, cost: 3000, pos: "名詞".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "硬貨".to_string(), reading: "こうか".to_string(),
            left_id: 3, right_id: 3, cost: 2900, pos: "名詞".to_string(),
        });
        let converter = ViterbiConverter::new(dict);

        let mut state = LiveConversionState::new();
        state.hiragana_buffer = "かわせこうか".to_string();
        state.converter = Some(converter);

        // 自動変換（文全体の1-best）は「効果」を選ぶ
        let live = state.converter.as_ref().unwrap().convert_context_aware_to_string("かわせこうか");
        assert_eq!(live, "為替効果");

        // 候補一覧の1位も一致するべき（単語コスト単体では「硬貨」の方が
        // 安いが、文脈込みの合計コストでは「効果」が勝つ）
        let (candidates, _seg_reading, seg_surfaces, ..) = state.build_candidates();
        assert_eq!(seg_surfaces.first().map(String::as_str), Some("効果"));
        assert_eq!(candidates.first().map(String::as_str), Some("為替効果"));
    }
}

#[cfg(test)]
mod auto_compound_tests {
    use super::*;

    fn dict_without_compound() -> crate::Dictionary {
        let mut dict = crate::Dictionary::new();
        dict.add_word(crate::WordEntry {
            surface: "再".to_string(), reading: "さい".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "接頭詞-名詞接続-*-*".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "起動".to_string(), reading: "きどう".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-サ変接続-*-*".to_string(),
        });
        dict
    }

    /// 「再」+「起動」のように辞書に無い複合語（再起動）が3回続けて隣接して
    /// 確定されたら、辞書に単語として自動登録され、DBには source='auto' で
    /// 記録されることを確認する。1〜2回目ではまだ登録されない（反復を要求）。
    #[test]
    fn repeated_adjacent_kanji_pair_gets_auto_registered() {
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(dict_without_compound()));
        state.learning = crate::LearningRepository::in_memory().ok();
        assert!(state.learning.is_some());

        let segments = vec![
            ("さい".to_string(), "再".to_string(), "接頭詞-名詞接続-*-*".to_string()),
            ("きどう".to_string(), "起動".to_string(), "名詞-サ変接続-*-*".to_string()),
        ];

        state.learn_from_segments(&segments);
        state.learn_from_segments(&segments);
        let not_yet = state
            .converter
            .as_ref()
            .unwrap()
            .merged_lookup("さいきどう")
            .is_none_or(|es| !es.iter().any(|e| e.surface == "再起動"));
        assert!(not_yet, "1〜2回目ではまだ自動登録されないはず");

        state.learn_from_segments(&segments);
        let conv = state.converter.as_ref().unwrap();
        assert!(
            conv.merged_lookup("さいきどう")
                .unwrap()
                .iter()
                .any(|e| e.surface == "再起動"),
            "3回目で辞書に自動登録されるはず"
        );

        let words = state.learning.as_ref().unwrap().get_all_user_words().unwrap();
        let auto = words
            .iter()
            .find(|w| w.reading == "さいきどう" && w.surface == "再起動")
            .expect("DBにも登録されているはず");
        assert_eq!(auto.source, "auto");
    }

    /// カタカナ＋漢字（メモ＋帳）の組み合わせも、漢字＋漢字と同様に
    /// 繰り返し確定で自動登録される（「メモ帳」のような外来語＋漢字の
    /// 複合語が辞書に無いケースを、手作業の個別登録に頼らず拾うため）。
    #[test]
    fn repeated_adjacent_katakana_kanji_pair_gets_auto_registered() {
        let mut dict = crate::Dictionary::new();
        dict.add_word(crate::WordEntry {
            surface: "メモ".to_string(), reading: "めも".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-一般-*-*".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "帳".to_string(), reading: "ちょう".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-接尾-一般-*".to_string(),
        });
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(dict));
        state.learning = crate::LearningRepository::in_memory().ok();

        let segments = vec![
            ("めも".to_string(), "メモ".to_string(), "名詞-一般-*-*".to_string()),
            ("ちょう".to_string(), "帳".to_string(), "名詞-接尾-一般-*".to_string()),
        ];
        for _ in 0..3 {
            state.learn_from_segments(&segments);
        }
        let conv = state.converter.as_ref().unwrap();
        assert!(
            conv.merged_lookup("めもちょう")
                .unwrap()
                .iter()
                .any(|e| e.surface == "メモ帳"),
            "3回目で辞書に自動登録されるはず"
        );
    }

    /// 実例:「再」+「起動」の隣接確定を繰り返した読み(さいきどう)に、既に
    /// 別表記（再起動。ユーザー登録語や過去の自動登録で辞書に入っている
    /// 想定）が存在するなら、同じ読みへ2つ目の複合語（際起動）を自動登録
    /// しない。過去のライブ変換の不具合で「再起動」を「際」+「起動」に
    /// 誤分割したまま繰り返し確定してしまうと、正しい「再起動」と並んで
    /// 「際起動」が予測変換に出続けてしまう事故があった。
    #[test]
    fn auto_compound_skipped_when_reading_already_has_other_surface() {
        let mut dict = crate::Dictionary::new();
        dict.add_word(crate::WordEntry {
            surface: "際".to_string(), reading: "さい".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-一般-*-*".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "起動".to_string(), reading: "きどう".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-サ変接続-*-*".to_string(),
        });
        // 既に正しい表記が辞書にある（ユーザー登録語や過去の自動登録を想定）
        dict.add_word(crate::WordEntry {
            surface: "再起動".to_string(), reading: "さいきどう".to_string(),
            left_id: 1, right_id: 1, cost: 2000, pos: "名詞-一般-*-*".to_string(),
        });
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(dict));
        state.learning = crate::LearningRepository::in_memory().ok();

        let segments = vec![
            ("さい".to_string(), "際".to_string(), "名詞-一般-*-*".to_string()),
            ("きどう".to_string(), "起動".to_string(), "名詞-サ変接続-*-*".to_string()),
        ];
        for _ in 0..5 {
            state.learn_from_segments(&segments);
        }

        let has_bad_entry = state
            .converter
            .as_ref()
            .unwrap()
            .dictionary
            .lookup("さいきどう")
            .unwrap()
            .iter()
            .any(|e| e.surface == "際起動");
        assert!(!has_bad_entry, "既存の「再起動」と並ぶ「際起動」を自動登録してはいけない");

        let words = state.learning.as_ref().unwrap().get_all_user_words().unwrap();
        assert!(
            !words.iter().any(|w| w.surface == "際起動"),
            "DBにも「際起動」を登録してはいけない"
        );
    }

    /// 同じ読みに、無関係などこかの希少語（1文字あたりコストが極端に高い）が
    /// たまたま存在するだけでは自動登録を諦めない。`dictionary.lookup`は
    /// 読みの完全一致を辞書全体から拾うため、対象と無関係などこかの希少語・
    /// 固有名詞が同じ読みを持つだけで自動登録が永久に効かなくなるのを防ぐ
    /// （「際起動」事故を防ぐガードが広すぎた回帰）。
    #[test]
    fn auto_compound_registers_despite_unrelated_rare_homophone() {
        let mut dict = crate::Dictionary::new();
        dict.add_word(crate::WordEntry {
            surface: "検".to_string(), reading: "けん".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-一般-*-*".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "査定".to_string(), reading: "さてい".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-サ変接続-*-*".to_string(),
        });
        // 「けんさてい」と全く無関係などこかの希少な当て字（1文字あたり
        // コストが極端に高い＝実際にはほぼ使われない語）がたまたま同じ読み
        // を持つ。これは「検査定」の誤分割とは無関係なので登録を妨げない。
        dict.add_word(crate::WordEntry {
            surface: "献砂綴".to_string(), reading: "けんさてい".to_string(),
            left_id: 1, right_id: 1, cost: 20000, pos: "名詞-一般-*-*".to_string(),
        });
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(dict));
        state.learning = crate::LearningRepository::in_memory().ok();

        let segments = vec![
            ("けん".to_string(), "検".to_string(), "名詞-一般-*-*".to_string()),
            ("さてい".to_string(), "査定".to_string(), "名詞-サ変接続-*-*".to_string()),
        ];
        for _ in 0..5 {
            state.learn_from_segments(&segments);
        }

        let registered = state
            .converter
            .as_ref()
            .unwrap()
            .merged_lookup("けんさてい")
            .unwrap()
            .iter()
            .any(|e| e.surface == "検査定");
        assert!(registered, "無関係な希少語のせいで自動登録が妨げられている");
    }

    /// 助詞や単独ひらがなを挟むペアは自動登録しない
    #[test]
    fn particle_adjacent_pair_not_registered() {
        let mut dict = crate::Dictionary::new();
        dict.add_word(crate::WordEntry {
            surface: "検索".to_string(), reading: "けんさく".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞-サ変接続-*-*".to_string(),
        });
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(dict));
        state.learning = crate::LearningRepository::in_memory().ok();

        let segments = vec![
            ("けんさく".to_string(), "検索".to_string(), "名詞-サ変接続-*-*".to_string()),
            ("は".to_string(), "は".to_string(), "助詞-係助詞-*-*".to_string()),
        ];
        for _ in 0..5 {
            state.learn_from_segments(&segments);
        }
        let words = state.learning.as_ref().unwrap().get_all_user_words().unwrap();
        assert!(words.is_empty(), "助詞を挟むペアは自動登録されないはず");
    }
}

#[cfg(test)]
mod typo_learning_tests {
    use super::*;

    fn typo_test_dict() -> Dictionary {
        let mut dict = Dictionary::new();
        dict.add_word(crate::WordEntry {
            surface: "今日".to_string(), reading: "きょう".to_string(),
            left_id: 1, right_id: 1, cost: 3000, pos: "名詞".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "は".to_string(), reading: "は".to_string(),
            left_id: 2, right_id: 2, cost: 3000, pos: "助詞".to_string(),
        });
        dict
    }

    /// もしかしての先頭候補を確定すると誤字修正が学習され、次回同じ誤字を
    /// 打ったときに（辞書のあいまい検索をやり直さず）学習済み候補として
    /// 最優先で出てくること、さらに Delete でその学習だけをリセットできる
    /// ことを一連の流れで確認する。
    ///
    /// もしかしての初回検出手段（ローマ字取り残し repair 等）とは切り離して
    /// 「確定→学習→次回再利用→Deleteでリセット」のパイプラインだけを
    /// 検証するため、1回目の候補は実際の検出を通さず直接セットする
    /// （もしかしての一般的なあいまい検索はノイズ低減のため既に廃止済み。
    /// [[fuzzy-correction-approach]]参照。学習・再利用の仕組み自体は
    /// 検出手段が何であっても同じonce もしかしてとして出た候補に対して働く）。
    #[test]
    fn accepting_fuzzy_suggestion_learns_and_reapplies_next_time() {
        let mut state = LiveConversionState::new();
        state.converter = Some(ViterbiConverter::new(typo_test_dict()));
        state.learning = crate::LearningRepository::in_memory().ok();
        assert!(state.learning.is_some());

        // 1回目: 「きようは」（拗音の打ち間違い）に対して、もしかして候補
        // 「きょうは→今日は」が出ている状態を再現する。まだ学習していない
        // ので prediction_top_is_learned_typo は false。
        state.hiragana_buffer = "きようは".to_string();
        state.predictions = vec![("きょうは".to_string(), "今日は".to_string())];
        state.prediction_top_is_fuzzy = true;
        state.prediction_top_is_learned_typo = false;

        // その候補を確定する（＝ユーザーが修正を採用した）
        state.commit_prediction(0);

        // 学習DB・メモリ上の変換器の両方に誤字修正が記録されている
        let freq = state
            .learning
            .as_ref()
            .unwrap()
            .find_typo_correction_frequency("きようは", "きょうは", "今日は")
            .unwrap();
        assert_eq!(freq, 1);
        assert!(state
            .converter
            .as_ref()
            .unwrap()
            .typo_corrections
            .contains_key("きようは"));

        // 2回目: 同じ誤字「きようは」を打つと、あいまい検索をやり直さず
        // 学習済みの修正がそのまま最優先の候補として出る。
        state.hiragana_buffer = "きようは".to_string();
        state.update_predictions();
        assert!(state.prediction_top_is_fuzzy);
        assert!(state.prediction_top_is_learned_typo, "学習済み候補として出るべき");
        assert_eq!(state.predictions.first(), Some(&("きょうは".to_string(), "今日は".to_string())));

        // Delete で誤字修正の学習だけをリセットできる
        assert!(state.reset_prediction_learning());
        assert!(!state
            .converter
            .as_ref()
            .unwrap()
            .typo_corrections
            .contains_key("きようは"));
        let freq_after = state
            .learning
            .as_ref()
            .unwrap()
            .find_typo_correction_frequency("きようは", "きょうは", "今日は")
            .unwrap();
        assert_eq!(freq_after, 0);
    }
}

#[cfg(test)]
mod reading_validation_tests {
    use super::is_hiragana_reading;

    #[test]
    fn accepts_hiragana_and_long_vowel_mark() {
        assert!(is_hiragana_reading("ぷらいみんぐ"));
        assert!(is_hiragana_reading("そーと"));
    }

    #[test]
    fn rejects_kanji_reading() {
        // 実データで見つかった「読み欄に表記(漢字)を入れてしまった」誤登録
        // （例: 読み="死ぬ気" 表記="しぬき"）を弾けることを確認する。
        assert!(!is_hiragana_reading("死ぬ気"));
    }

    #[test]
    fn rejects_katakana_and_ascii() {
        assert!(!is_hiragana_reading("ソート"));
        assert!(!is_hiragana_reading("react"));
    }
}

#[cfg(test)]
mod state_machine_tests {
    use super::*;

    /// 「きょう」に「今日」「強」の同音語、「がっこう」に「学校」を割り当てた
    /// テスト用辞書＋変換器。commit_first_word/extend_kana_revert/cycle_candidate
    /// など複数文節にまたがる状態遷移テストで共有する。
    fn two_segment_test_converter() -> ViterbiConverter {
        let mut dict = Dictionary::new();
        dict.matrix = crate::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        dict.add_word(crate::WordEntry {
            surface: "今日".to_string(), reading: "きょう".to_string(),
            left_id: 1, right_id: 1, cost: 3000, pos: "名詞".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "強".to_string(), reading: "きょう".to_string(),
            left_id: 1, right_id: 1, cost: 4000, pos: "名詞".to_string(),
        });
        dict.add_word(crate::WordEntry {
            surface: "学校".to_string(), reading: "がっこう".to_string(),
            left_id: 1, right_id: 1, cost: 3000, pos: "名詞".to_string(),
        });
        ViterbiConverter::new(dict)
    }

    fn state_with_two_segment_dict() -> LiveConversionState {
        let mut state = LiveConversionState::new();
        state.converter = Some(two_segment_test_converter());
        state
    }

    fn type_str(state: &mut LiveConversionState, s: &str) {
        for ch in s.chars() {
            let _ = state.add_char(ch);
        }
    }

    /// `[` の直後に Tab（cycle_candidate）で括弧の種類を切り替えると、表示だけで
    /// なく読みバッファ内の括弧文字も置き換わり、以降の打鍵で元に戻らない。
    /// さらに `]` はその種類に対応する閉じ括弧（『→』）になる。
    #[test]
    fn bracket_variant_selection_persists_and_closer_matches() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "[");
        assert_eq!(state.conversion_result, "「");

        // 一覧を開く（候補1=「 が選択状態）→ 次候補 『
        let _ = state.cycle_candidate(false);
        assert_eq!(state.candidates[0], "「");
        assert_eq!(state.candidates[1], "『");
        let _ = state.cycle_candidate(false);
        assert_eq!(state.conversion_result, "『");
        assert_eq!(state.hiragana_buffer, "『");

        // 続けて打鍵しても『のまま（バッファ自体が『なので再変換で戻らない）
        type_str(&mut state, "kyou");
        assert_eq!(state.conversion_result, "『今日");

        // 閉じ括弧は『に対応する』になる
        type_str(&mut state, "]");
        assert_eq!(state.conversion_result, "『今日』");
    }

    /// 確定を挟んでも（開き括弧が確定済みテキスト側にあっても）閉じ括弧は
    /// 対応する種類になる。入れ子（【…「…」…】）も内側から順に閉じる。
    #[test]
    fn closing_bracket_matches_opener_across_commit_and_nesting() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "[");
        let _ = state.cycle_candidate(false);
        let _ = state.select_candidate(2); // 【
        assert_eq!(state.conversion_result, "【");
        let _ = state.commit();

        type_str(&mut state, "kyou[");
        assert_eq!(state.conversion_result, "今日「");
        let _ = state.commit();
        type_str(&mut state, "kyou]");
        assert_eq!(state.conversion_result, "今日」");
        let _ = state.commit();
        type_str(&mut state, "]");
        assert_eq!(state.conversion_result, "】");
    }

    /// 助詞を含むテスト用辞書（先頭文節の固定は文節の切れ目＝助詞の直後で行う）
    fn particle_test_converter() -> ViterbiConverter {
        let mut dict = Dictionary::new();
        dict.matrix = crate::ConnectionMatrix::new(10, 10);
        for i in 0..10 {
            for j in 0..10 {
                dict.matrix.set(i, j, 200);
            }
        }
        let mk = |s: &str, r: &str, id: crate::PosId, cost: i16, pos: &str| crate::WordEntry {
            surface: s.to_string(), reading: r.to_string(),
            left_id: id, right_id: id, cost, pos: pos.to_string(),
        };
        dict.add_word(mk("今日", "きょう", 1, 3000, "名詞-副詞可能-*-*"));
        dict.add_word(mk("強", "きょう", 1, 4000, "名詞-一般-*-*"));
        dict.add_word(mk("学校", "がっこう", 1, 3000, "名詞-一般-*-*"));
        dict.add_word(mk("は", "は", 2, 1000, "助詞-係助詞-*-*"));
        dict.add_word(mk("に", "に", 2, 1000, "助詞-格助詞-一般-*"));
        ViterbiConverter::new(dict)
    }

    /// 画面上のテキストを模擬してアクションを適用する
    fn apply_to_screen(screen: &mut String, action: Option<ConversionAction>) {
        if let Some(a) = action {
            let keep = screen.chars().count().saturating_sub(a.delete_count);
            *screen = screen.chars().take(keep).collect();
            screen.push_str(&a.insert_text);
        }
    }

    /// 読みが長くなると先頭側の文節が固定され、後から辞書側の事情（学習）が
    /// 変わっても固定部分は書き換わらない。未固定の末尾は通常どおり変換される。
    /// 固定は表示の流れを変えない（画面テキスト＝表示結果）。
    #[test]
    fn long_input_pins_leading_clauses_and_keeps_them_stable() {
        let mut state = LiveConversionState::new();
        state.converter = Some(particle_test_converter());
        let mut screen = String::new();
        // きょうはがっこうに ×3 = 27文字（固定のしきい値24文字以上）
        for _ in 0..3 {
            for ch in "kyouhagakkouni".chars() {
                let action = state.add_char(ch);
                apply_to_screen(&mut screen, action);
            }
        }
        assert_eq!(state.conversion_result, "今日は学校に今日は学校に今日は学校に");
        assert_eq!(screen, state.conversion_result);
        // 末尾12文字以上を残せる最後の切れ目（2つ目の「は」の直後）まで固定
        let pinned: String = state.pinned.iter().map(|e| e.surface.as_str()).collect();
        assert_eq!(pinned, "今日は学校に今日は");

        // 以後「きょう」は「強」が優先される学習が入っても、固定部分は「今日」のまま。
        // 未固定の末尾（3つ目の「きょう」）だけが「強」になる。
        state.converter.as_mut().unwrap().learn_unigram("きょう", "強", 10);
        for ch in "ni".chars() {
            let action = state.add_char(ch);
            apply_to_screen(&mut screen, action);
        }
        assert_eq!(state.conversion_result, "今日は学校に今日は学校に強は学校にに");
        assert_eq!(screen, state.conversion_result);

        // Backspace で固定領域まで削ると、固定は読みと一致する範囲に縮む
        for _ in 0..8 {
            let action = state.backspace();
            apply_to_screen(&mut screen, action);
        }
        assert!(state.pinned.len() < 6, "pinned={:?}", state.pinned);
        assert_eq!(screen, state.conversion_result);

        // 確定で固定は解除され、学習用の文節列は固定部分も含めて揃う
        let _ = state.commit();
        assert!(state.pinned.is_empty());
        assert!(state.hiragana_buffer.is_empty());
    }

    /// 遅延させた学習は flush でまとめてDBに記録される（確定時は溜めるだけ）
    #[test]
    fn deferred_learning_is_recorded_on_flush() {
        let mut state = LiveConversionState::new();
        state.converter = Some(particle_test_converter());
        state.learning = Some(LearningRepository::in_memory().unwrap());
        // 遅延先（候補ウィンドウ）はテストには無いので、溜めた文節列を直接 flush する
        state.pending_learning.push(vec![
            ("きょう".to_string(), "今日".to_string(), "名詞-副詞可能-*-*".to_string()),
            ("は".to_string(), "は".to_string(), "助詞-係助詞-*-*".to_string()),
            ("がっこう".to_string(), "学校".to_string(), "名詞-一般-*-*".to_string()),
        ]);
        state.flush_pending_learning();
        assert!(state.pending_learning.is_empty());
        let learning = state.learning.as_ref().unwrap();
        assert_eq!(learning.find_frequency("きょう", "今日").unwrap(), 1);
        assert_eq!(learning.find_frequency("がっこう", "学校").unwrap(), 1);
        assert_eq!(learning.find_bigram_frequency("は", "学校").unwrap(), 1);
    }

    /// `commit_prediction` の学習（ユニグラム・バイグラム・誤字修正）も
    /// `commit()` と同じく遅延キューに溜めて後から flush できる
    /// （フック内で同期にDB書き込みしないため）。
    #[test]
    fn deferred_prediction_learning_is_recorded_on_flush() {
        let mut state = LiveConversionState::new();
        state.converter = Some(particle_test_converter());
        state.learning = Some(LearningRepository::in_memory().unwrap());
        state.pending_prediction_learning.push(PredictionLearning {
            unigram: Some(("きょう".to_string(), "今日".to_string())),
            bigram: Some(("学校".to_string(), "今日".to_string())),
            typo_correction: Some((
                "きよう".to_string(),
                "きょう".to_string(),
                "今日".to_string(),
            )),
        });
        state.flush_pending_learning();
        assert!(state.pending_prediction_learning.is_empty());
        let learning = state.learning.as_ref().unwrap();
        assert_eq!(learning.find_frequency("きょう", "今日").unwrap(), 1);
        assert_eq!(learning.find_bigram_frequency("学校", "今日").unwrap(), 1);
        assert_eq!(
            learning
                .find_typo_correction_frequency("きよう", "きょう", "今日")
                .unwrap(),
            1
        );
    }

    /// `commit_prediction` で番号キー確定した学習も、通常の `commit()` と
    /// 同様にDB書き込みを直接行わず `PredictionLearning` を経由する
    /// （実装の詳細だが、フック内で同期書き込みが復活していないことの
    /// リグレッション検知として直接呼び出しの経路を確認する）。
    #[test]
    fn commit_prediction_records_learning_through_deferred_helper() {
        let mut state = state_with_two_segment_dict();
        state.learning = Some(LearningRepository::in_memory().unwrap());
        state.last_committed = "今日".to_string();
        state.predictions = vec![("がっこう".to_string(), "学校".to_string())];
        let action = state.commit_prediction(0).unwrap();
        assert_eq!(action.insert_text, "学校");
        let learning = state.learning.as_ref().unwrap();
        assert_eq!(learning.find_frequency("がっこう", "学校").unwrap(), 1);
        assert_eq!(learning.find_bigram_frequency("今日", "学校").unwrap(), 1);
    }

    /// Tab候補一覧（`build_candidates`）もライブ表示と同じ固定済み先頭文節
    /// （`self.pinned`）を使う。無関係な学習で先頭の同音語のコストが変わっても、
    /// 固定済みの部分を候補一覧が無視して再変換し、選択時に上書きしてしまわない。
    #[test]
    fn build_candidates_reflects_pinned_prefix_after_unrelated_learning() {
        let mut state = LiveConversionState::new();
        state.converter = Some(particle_test_converter());
        for _ in 0..3 {
            for ch in "kyouhagakkouni".chars() {
                let _ = state.add_char(ch);
            }
        }
        let pinned_surface: String = state.pinned.iter().map(|e| e.surface.as_str()).collect();
        assert!(!pinned_surface.is_empty());
        // 無関係な学習で「きょう」が「強」に変わっても、固定済みの先頭文節は
        // build_candidates（Tab候補一覧）でも変わらない。
        state.converter.as_mut().unwrap().learn_unigram("きょう", "強", 10);
        let (_, _, _, prefix_surface, _, _, _) = state.build_candidates();
        assert!(
            prefix_surface.starts_with(&pinned_surface),
            "prefix_surface={prefix_surface:?} pinned_surface={pinned_surface:?}"
        );
    }

    /// しきい値未満の短い入力では固定しない
    #[test]
    fn short_input_is_not_pinned() {
        let mut state = LiveConversionState::new();
        state.converter = Some(particle_test_converter());
        for ch in "kyouhagakkouni".chars() {
            let _ = state.add_char(ch);
        }
        assert_eq!(state.conversion_result, "今日は学校に");
        assert!(state.pinned.is_empty());
    }

    /// 対応する開き括弧が無ければ `]` は既定の 」 のまま
    #[test]
    fn closing_bracket_defaults_without_opener() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou]");
        assert_eq!(state.conversion_result, "今日」");
    }

    /// 未確定のローマ字が残っているときのBackspaceは1文字ずつ削除する。
    #[test]
    fn backspace_on_pending_romaji_removes_one_char_at_a_time() {
        let mut state = state_with_two_segment_dict();
        for ch in "ky".chars() {
            state.add_char(ch);
        }
        assert_eq!(state.romaji_buffer, "ky");

        state.backspace();
        assert_eq!(state.romaji_buffer, "k");

        state.backspace();
        assert_eq!(state.romaji_buffer, "");
        assert!(!state.is_composing());
    }

    /// 変換済み（ひらがな確定済み）部分へのBackspaceは、1文字ずつではなく
    /// 最後の変換単語（文節）ごと削除する（`convert_buffer`が返す末尾語の
    /// 読み文字数分をまとめて消す。実際のIMEの挙動に合わせた設計であり、
    /// 単純な1文字ポップではない点に注意）。
    #[test]
    fn backspace_on_converted_hiragana_removes_the_last_word_at_once() {
        let mut state = state_with_two_segment_dict();
        for ch in "kyou".chars() {
            state.add_char(ch);
        }
        assert_eq!(state.hiragana_buffer, "きょう");
        assert!(state.romaji_buffer.is_empty());

        state.backspace();
        assert_eq!(state.hiragana_buffer, "");
        assert!(!state.is_composing());
    }

    /// ローマ字 "kyou" を1文字ずつ add_char に通した後 commit() すると、
    /// 表示バッファ一式がクリアされ入力世代(generation)が進むことを確認する
    /// （修正前は commit 後もバッファが残り、次の入力に前回の変換が混入し得た）。
    #[test]
    fn add_char_then_commit_clears_buffers_and_advances_generation() {
        let mut state = state_with_two_segment_dict();
        for ch in "kyou".chars() {
            state.add_char(ch);
        }
        assert_eq!(state.hiragana_buffer, "きょう");
        assert!(state.is_composing());

        let gen_before = state.generation;
        let action = state.commit();
        // 表示は逐次更新で既に確定形と一致しているため、commit自体は
        // 追加の差分アクションを返さない（末尾 'n' の特殊処理のみ例外）。
        assert!(action.is_none());
        assert!(!state.is_composing());
        assert_eq!(state.hiragana_buffer, "");
        assert_eq!(state.romaji_buffer, "");
        assert_eq!(state.conversion_result, "");
        assert_eq!(state.kana_tail_len, 0);
        assert_eq!(state.generation, gen_before + 1);
    }

    /// Tab連打による同音候補巡回が、末尾で先頭へ・先頭で末尾へ折り返す
    /// （境界のインデックス計算を回帰対象化する）。
    #[test]
    fn cycle_candidate_wraps_around_index_boundaries() {
        let mut state = state_with_two_segment_dict();
        state.hiragana_buffer = "きょう".to_string();

        // 初回のTabは一覧生成＋先頭(index 0)選択のみ
        let first = state.cycle_candidate(false);
        assert!(first.is_some());
        assert_eq!(state.candidate_index, 0);
        let candidate_count = state.candidates.len();
        assert!(candidate_count >= 2, "「今日」「強」の同音語が候補にあるはず");

        // 末尾まで進める
        for _ in 1..candidate_count {
            state.cycle_candidate(false);
        }
        assert_eq!(state.candidate_index, candidate_count - 1);

        // 末尾からさらに進めると先頭へ折り返す
        state.cycle_candidate(false);
        assert_eq!(state.candidate_index, 0);

        // 先頭から逆方向へ進めると末尾へ折り返す
        state.cycle_candidate(true);
        assert_eq!(state.candidate_index, candidate_count - 1);
    }

    /// Escを繰り返すと末尾の文節から順に一つずつひらがなへ戻り、
    /// 全て戻し終えたら None（呼び出し側は取消にフォールバック）を返す。
    #[test]
    fn extend_kana_revert_reverts_one_segment_at_a_time_then_stops() {
        let mut state = state_with_two_segment_dict();
        state.hiragana_buffer = "きょうがっこう".to_string();
        let total = state.hiragana_buffer.chars().count();
        assert_eq!(total, 7);

        // 1回目: 末尾の文節（学校＝がっこう、4文字）だけをひらがなに戻す
        let first = state.extend_kana_revert();
        assert!(first.is_some());
        assert_eq!(state.kana_tail_len, 4);

        // 2回目: 残っていた前半（今日＝きょう）も戻り、全体が戻し終わる
        let second = state.extend_kana_revert();
        assert!(second.is_some());
        assert_eq!(state.kana_tail_len, 7);

        // 3回目: これ以上戻すものが無い
        let third = state.extend_kana_revert();
        assert!(third.is_none());
    }

    /// generation は add_char / backspace / commit / cancel のいずれでも
    /// 単調に増加し続ける（非同期結果を世代で棄却する仕組みの前提）。
    /// 逆行・据え置きが起きたらここで検出する。
    #[test]
    fn generation_counter_strictly_increases_across_mutations() {
        let mut state = state_with_two_segment_dict();
        let g0 = state.generation;

        state.add_char('k');
        let g1 = state.generation;
        assert!(g1 > g0);

        state.backspace();
        let g2 = state.generation;
        assert!(g2 > g1);

        state.add_char('k');
        let g3 = state.generation;
        let _ = state.commit();
        let g4 = state.generation;
        assert!(g4 > g3);

        let _ = state.cancel();
        let g5 = state.generation;
        assert!(g5 > g4);
    }

    /// commit_first_word は 1-best の先頭語だけを確定済み扱いにし、
    /// 残りの読みは未変換のまま保持する（前半のみ正しい場合の部分確定用）。
    #[test]
    fn commit_first_word_partially_commits_leading_word_only() {
        let mut state = state_with_two_segment_dict();
        state.hiragana_buffer = "きょうがっこう".to_string();
        // 表示は既に1-bestと一致している状態を模す
        state.conversion_result = "今日学校".to_string();

        state.commit_first_word();

        assert_eq!(state.hiragana_buffer, "がっこう");
        assert_eq!(state.conversion_result, "学校");
        assert_eq!(
            state.committed_segments,
            vec![("きょう".to_string(), "今日".to_string(), "名詞".to_string())]
        );
    }

    /// 予測変換（前方一致補完）の確定: 現在の表示を丸ごと予測表記に
    /// 置き換えるアクションを返し、last_committed を更新する。
    #[test]
    fn commit_prediction_replaces_display_for_prefix_completion() {
        let mut state = LiveConversionState::new();
        state.conversion_result = "きょ".to_string();
        state.hiragana_buffer = "きょ".to_string();
        state.predictions = vec![("きょう".to_string(), "今日".to_string())];

        let gen_before = state.generation;
        let action = state
            .commit_prediction(0)
            .expect("前方一致予測の確定はアクションを返す");
        assert_eq!(action.delete_count, 2); // 表示中の "きょ"（2文字）を全削除
        assert_eq!(action.insert_text, "今日");
        assert_eq!(state.last_committed, "今日");
        assert!(state.hiragana_buffer.is_empty());
        assert!(state.predictions.is_empty());
        assert_eq!(state.generation, gen_before + 1);
    }

    /// 予測変換（次単語予測・読み空）の確定: 表示の末尾に追記するだけで
    /// 削除は発生しない。
    #[test]
    fn commit_prediction_appends_for_next_word_prediction() {
        let mut state = LiveConversionState::new();
        state.recent_context = "今日".to_string();
        state.last_committed = "今日".to_string();
        state.predictions = vec![(String::new(), "は".to_string())];

        let action = state
            .commit_prediction(0)
            .expect("次単語予測の確定はアクションを返す");
        assert_eq!(action.delete_count, 0);
        assert_eq!(action.insert_text, "は");
        assert_eq!(state.last_committed, "は");
    }

    /// 範囲外インデックスの確定は None（消費しない）。
    #[test]
    fn commit_prediction_out_of_range_index_returns_none() {
        let mut state = LiveConversionState::new();
        state.predictions = vec![("きょう".to_string(), "今日".to_string())];
        assert!(state.commit_prediction(5).is_none());
    }

    #[test]
    fn predictions_use_context_but_stay_hidden_without_input() {
        let mut state = LiveConversionState::new();
        let learning = LearningRepository::in_memory().unwrap();
        for _ in 0..5 {
            learning.record_commit("かいしゃ", "会社", None).unwrap();
        }
        learning.record_commit("かいぎ", "会議", None).unwrap();
        for _ in 0..2 {
            learning.record_bigram("次の", "会議").unwrap();
        }
        state.learning = Some(learning);
        state.last_committed = "次の".into();
        state.hiragana_buffer = "かい".into();
        state.conversion_result = "かい".into();
        state.update_predictions();
        assert!(state.predictions.is_empty());
        state.request_predictions();
        assert_eq!(state.predictions[0], ("かいぎ".into(), "会議".into()));
        state.update_predictions();
        assert!(state.predictions.is_empty());
        state.hiragana_buffer.clear();
        state.request_predictions();
        assert!(state.predictions.is_empty());
        state.romaji_buffer = "k".into();
        state.update_predictions();
        assert!(state.predictions.is_empty());
    }

    #[test]
    fn phrase_append_preserves_composing_context_and_learning() {
        let mut state = state_with_two_segment_dict();
        state.learning = Some(LearningRepository::in_memory().unwrap());
        type_str(&mut state, "kyou");
        state.predictions = vec![(String::new(), "です".into())];
        let action = state.commit_prediction(0).unwrap();
        assert_eq!(action.delete_count, 0);
        assert_eq!(action.insert_text, "です");
        assert_eq!(state.recent_context, "今日です");
        assert_eq!(state.learning.as_ref().unwrap()
            .find_bigram_frequency("今日", "です").unwrap(), 1);
    }

    #[test]
    fn mid_sentence_completion_preserves_prefix_and_selected_surface() {
        let mut state = state_with_two_segment_dict();
        let learning = LearningRepository::in_memory().unwrap();
        learning.record_commit("がっこうせいかつ", "学校生活", None).unwrap();
        state.learning = Some(learning);
        type_str(&mut state, "kyougakkou");
        state.request_predictions();
        assert!(!state.predictions.contains(&("きょうがっこうせいかつ".into(), "今日学校生活".into())));
        state.conversion_result = "強学校".into();
        state.request_predictions();
        assert!(!state.predictions.iter().any(|(_, s)| s == "今日学校生活"));
    }

    #[test]
    fn auxiliary_tai_is_not_expanded_as_an_independent_word() {
        let mut state = state_with_two_segment_dict();
        for (reading, surface, pos) in [
            ("へんかん", "変換", "名詞-サ変接続"),
            ("し", "し", "動詞-自立"),
            ("たい", "たい", "助動詞"),
        ] {
            state.converter.as_mut().unwrap().overlay.add_word(crate::WordEntry {
                reading: reading.into(), surface: surface.into(), pos: pos.into(),
                left_id: 1, right_id: 1, cost: 100,
            });
        }
        let learning = LearningRepository::in_memory().unwrap();
        learning.record_commit("たいまるまる", "たい丸々", None).unwrap();
        state.learning = Some(learning);
        type_str(&mut state, "henkansitai");
        assert_eq!(state.conversion_result, "変換したい");
        state.request_predictions();
        assert!(state.predictions.is_empty(), "{:?}", state.predictions);
    }

    #[test]
    fn additive_completions_and_phrase_tails_are_not_offered() {
        let mut state = state_with_two_segment_dict();
        let learning = LearningRepository::in_memory().unwrap();
        learning.record_commit("きょう", "今日", None).unwrap();
        learning.record_commit("きょうじゅう", "今日中", None).unwrap();
        for _ in 0..5 {
            learning.record_bigram("今日", "です").unwrap();
        }
        state.learning = Some(learning);
        type_str(&mut state, "kyou");
        state.request_predictions();
        assert!(state.predictions.is_empty(), "{:?}", state.predictions);
        assert!(is_insertion_only("今日学校", "今日の学校"));
        assert!(is_insertion_only("今日", "今日中"));
        assert!(!is_insertion_only("会社", "学校"));
        assert!(!is_insertion_only("今日", "今日"));
    }

    #[test]
    fn numeric_counter_candidates_include_whole_reading_homophones() {
        let mut state = state_with_two_segment_dict();
        for (reading, surface, pos, cost) in [
            ("ご", "５", "名詞-数", 100),
            ("じ", "時", "名詞-接尾-助数詞", 100),
            ("ごじ", "誤字", "名詞-一般", 9000),
        ] {
            state.converter.as_mut().unwrap().overlay.add_word(crate::WordEntry {
                reading: reading.into(), surface: surface.into(), pos: pos.into(),
                left_id: 1, right_id: 1, cost,
            });
        }
        state.hiragana_buffer = "ごじ".into();
        state.conversion_result = "５時".into();
        let (candidates, reading, _, _, _, _, _) = state.build_candidates();
        assert_eq!(reading, "ごじ");
        assert!(candidates.contains(&"誤字".to_string()), "{candidates:?}");
    }

    #[test]
    fn compound_candidate_repairs_symbol_split() {
        let mut state = state_with_two_segment_dict();
        state.converter.as_mut().unwrap().overlay.add_word(crate::WordEntry {
            reading: "まん".into(), surface: "万".into(), left_id: 1, right_id: 1,
            cost: 100, pos: "名詞-数".into(),
        });
        state.converter.as_mut().unwrap().overlay.add_word(crate::WordEntry {
            reading: "かい".into(), surface: "Χ".into(), left_id: 1, right_id: 1,
            cost: 100, pos: "記号-アルファベット".into(),
        });
        state.converter.as_mut().unwrap().overlay.add_word(crate::WordEntry {
            reading: "まんかい".into(), surface: "満開".into(), left_id: 1, right_id: 1,
            cost: 5000, pos: "名詞-一般".into(),
        });
        state.hiragana_buffer = "まんかい".into();
        state.conversion_result = "万Χ".into();
        let (candidates, reading, _, _, _, _, _) = state.build_candidates();
        assert_eq!(reading, "まんかい");
        assert!(candidates.contains(&"満開".to_string()), "{candidates:?}");
    }

    #[test]
    fn phrase_tail_does_not_use_a_different_homophone() {
        let mut state = state_with_two_segment_dict();
        let learning = LearningRepository::in_memory().unwrap();
        learning.record_commit("きょう", "強", None).unwrap();
        for _ in 0..5 {
            learning.record_bigram("強", "です").unwrap();
        }
        state.learning = Some(learning);
        type_str(&mut state, "kyou");
        state.update_predictions();
        assert!(!state.predictions.iter().any(|(r, _)| r.is_empty()));
    }

    #[test]
    fn sentence_correction_is_optional_and_can_be_accepted_and_learned() {
        let mut state = LiveConversionState::new();
        state.learning = Some(LearningRepository::in_memory().unwrap());
        state.hiragana_buffer = "おねがいしまうす".into();
        state.conversion_result = state.hiragana_buffer.clone();
        state.update_predictions();
        assert!(state.prediction_top_is_fuzzy);
        assert_eq!(state.prediction_index, 0);
        assert_eq!(state.conversion_result, "おねがいしまうす");
        assert_eq!(state.predictions[0], ("おねがいします".into(), "おねがいします".into()));
        let action = state.commit_prediction(0).unwrap();
        assert_eq!(action.delete_count, 8);
        assert_eq!(action.insert_text, "おねがいします");
        assert_eq!(state.learning.as_ref().unwrap().find_typo_correction_frequency(
            "おねがいしまうす", "おねがいします", "おねがいします").unwrap(), 1);
    }

    #[test]
    fn sentence_correction_waits_for_romaji_and_respects_kana_revert() {
        let mut state = LiveConversionState::new();
        state.hiragana_buffer = "こんにちわ".into();
        state.conversion_result = "こんにちわ".into();
        state.romaji_buffer = "t".into();
        state.update_predictions();
        assert!(state.predictions.is_empty());
        state.romaji_buffer.clear();
        state.kana_tail_len = 1;
        state.update_predictions();
        assert!(state.predictions.is_empty());
    }

    #[test]
    fn compact_prediction_shows_change_but_commits_full_text() {
        let mut state = LiveConversionState::new();
        state.hiragana_buffer = "かくにんをおねがいしまうす".into();
        state.conversion_result = "確認をおねがいしまうす".into();
        state.predictions = vec![("かくにんをおねがいします".into(), "確認をおねがいします".into())];
        state.prediction_top_is_fuzzy = true;
        assert_eq!(state.prediction_display(), vec!["もしかして: う を削除"]);
        let action = state.commit_prediction(0).unwrap();
        assert_eq!(action.insert_text, "確認をおねがいします");
        assert_eq!(action.delete_count, "確認をおねがいしまうす".chars().count());
        assert!(state.predictions.is_empty());
        assert_eq!(prediction_change_label("今日は学校", "今日は学校生活"), "＋生活");
        assert_eq!(prediction_change_label("今日は会社へ", "今日は学校へ"), "会社 → 学校");
        assert_eq!(compact_prediction_text(&"あ".repeat(100)).chars().count(), 24);
    }

    #[test]
    fn learned_correction_waits_for_pending_romaji_and_kana_revert() {
        let mut state = state_with_two_segment_dict();
        state.converter.as_mut().unwrap().learn_typo_correction("きょお", "きょう", "今日", 3);
        state.hiragana_buffer = "きょお".into();
        state.conversion_result = "きょお".into();
        state.romaji_buffer = "t".into();
        state.update_predictions();
        assert!(state.predictions.is_empty());
        state.romaji_buffer.clear();
        state.kana_tail_len = 1;
        state.update_predictions();
        assert!(state.predictions.is_empty());
    }

    #[test]
    fn stranded_alphabet_is_repaired_using_dictionary_and_can_be_accepted() {
        let mut state = LiveConversionState::new();
        let mut dict = crate::Dictionary::new();
        dict.add_word(crate::WordEntry {
            reading: "ごめんなさい".into(), surface: "ごめんなさい".into(),
            left_id: 1, right_id: 1, cost: 100, pos: "感動詞".into(),
        });
        state.converter = Some(ViterbiConverter::new(dict));
        state.hiragana_buffer = "gめんなさい".into();
        state.conversion_result = "gめんなさい".into();
        state.update_predictions();
        assert_eq!(state.predictions.first(), Some(&("ごめんなさい".into(), "ごめんなさい".into())));
        let action = state.commit_prediction(0).unwrap();
        assert_eq!(action.insert_text, "ごめんなさい");
        assert!(!action.insert_text.chars().any(|c| c.is_ascii_alphabetic()));
        assert!(state.predictions.is_empty());
    }

    #[test]
    fn alphabet_repair_preserves_pending_input_and_small_edits() {
        let mut state = state_with_two_segment_dict();
        assert!(state.romaji_repair_readings("きょうx").contains(&"きょう".to_string()));
        assert!(state.romaji_repair_readings("にっぽn").contains(&"にっぽん".to_string()));
        state.hiragana_buffer = "gめんなさい".into();
        for pending in ["k", "ky", "n"] {
            state.romaji_buffer = pending.into();
            assert!(state.romaji_repair_suggest().is_none(), "{pending}");
        }
    }

    /// 統一スコア空間（[[correction-cascade-unification]]）: 学習済み誤字
    /// 修正（4-A-1）とハードコードルール（4-A-3）が同じ入力に対して両方
    /// 候補を出す場合、頻度由来ボーナスで割り引いた学習済みの方がコストで
    /// 勝つ（実測: 未学習のハードコードのみ=1970、freq=1の学習済み=470）。
    /// マージ方式が「先勝ち」の記述順ではなくコストで選んでいることの直接的な
    /// 確認。
    ///
    /// 「両方候補を出す場合」を最終結果（predictions）だけで確認すると、
    /// 将来どちらかが空を返すよう壊れても最終的な当選候補が変わらなければ
    /// 検出できない（例えばHardcodedRuleCorrectorが常に空を返すようになって
    /// もLearnedTypoCorrectorだけで同じ結果になり得る）。そのため各
    /// Correctorの`suggest`を直接呼び、実際に両方が非空を返すこと
    /// （＝マージが本当に競合状態で機能していること）も明示的に確認する。
    #[test]
    fn merged_correction_prefers_lower_cost_learned_over_hardcoded_rule() {
        let mut dict = crate::Dictionary::new();
        dict.add_word(crate::WordEntry {
            reading: "こんにちは".into(), surface: "こんにちは".into(),
            left_id: 1, right_id: 1, cost: 100, pos: "感動詞".into(),
        });
        let mut converter = ViterbiConverter::new(dict);
        converter.learn_typo_correction("こんにちわ", "こんにちは", "こんにちは", 1);

        let mut state = LiveConversionState::new();
        state.converter = Some(converter);
        state.hiragana_buffer = "こんにちわ".into();
        state.conversion_result = "こんにちわ".into();

        let learned = LearnedTypoCorrector.suggest(&state);
        let hardcoded = HardcodedRuleCorrector.suggest(&state);
        assert!(!learned.is_empty(), "学習済み誤字修正が候補を出していない");
        assert!(!hardcoded.is_empty(), "ハードコードルールが候補を出していない");
        assert!(
            learned[0].cost < hardcoded[0].cost,
            "学習済み({})がハードコード({})よりコストで勝つはず",
            learned[0].cost, hardcoded[0].cost
        );

        state.update_predictions();
        assert_eq!(
            state.predictions.first(),
            Some(&("こんにちは".to_string(), "こんにちは".to_string()))
        );
        assert!(
            state.prediction_top_is_learned_typo,
            "学習済み（4-A-1）が勝ったことがフラグからも分かるはず"
        );
    }

    /// 候補一覧が空のときは誤学習リセットも None（消費しない）。
    #[test]
    fn reset_learning_for_selected_returns_none_without_candidates() {
        let mut state = state_with_two_segment_dict();
        assert!(state.reset_learning_for_selected().is_none());
    }

    /// Deleteキーによる誤学習リセット後、候補一覧が再構築され
    /// 選択位置が先頭（最有力候補）へ戻ることを確認する。
    #[test]
    fn reset_learning_for_selected_rebuilds_candidates_and_resets_index() {
        let mut state = state_with_two_segment_dict();
        state.hiragana_buffer = "きょう".to_string();
        state.cycle_candidate(false); // 候補一覧生成＋先頭選択
        let candidate_count = state.candidates.len();
        assert!(candidate_count >= 2);
        state.select_candidate(candidate_count - 1); // 末尾候補を選択＆表示に反映

        let action = state.reset_learning_for_selected();
        assert!(action.is_some());
        assert_eq!(state.candidate_index, 0);
        assert!(!state.candidates.is_empty());
    }

    /// 確定すると、その読み・表記・文字数がリングバッファへ積まれる
    /// （確定後の復元用リングバッファ、器のみの段階。[[commit-ring-buffer]]）。
    #[test]
    fn commit_pushes_reading_and_surface_onto_commit_ring() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();

        assert_eq!(state.commit_ring.len(), 1);
        let entry = state.commit_ring.back().unwrap();
        assert_eq!(entry.reading, "きょう");
        assert_eq!(entry.surface, "今日");
        assert_eq!(entry.char_count, entry.surface.chars().count());
    }

    /// リングバッファは最大8件で、超えたら最も古いものから捨てる。
    #[test]
    fn commit_ring_evicts_oldest_entry_beyond_capacity() {
        let mut state = state_with_two_segment_dict();
        for _ in 0..9 {
            type_str(&mut state, "kyou");
            state.commit();
        }
        assert_eq!(state.commit_ring.len(), 8, "8件を超えて溜め続けてはいけない");
    }

    /// `invalidate_commit_ring`はリングを丸ごと空にする
    /// （フォーカス変更・マウスクリック・方向キー等の無効化条件で
    /// `hook-dll`側から呼ばれる。呼び出し箇所自体は`common`の外）。
    #[test]
    fn invalidate_commit_ring_clears_all_entries() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();
        assert!(!state.commit_ring.is_empty());

        state.invalidate_commit_ring();
        assert!(state.commit_ring.is_empty());
    }

    /// 復元ホットキーを押すと、リング最新エントリのn-bestが返り、初回は
    /// index 0（今まさに画面にある表記）ではなくindex 1（次善候補）が
    /// 選択される（ユーザー指摘の設計: 選んでも無変化になる候補を最初から
    /// 見せない）。
    #[test]
    fn press_restore_hotkey_selects_second_candidate_first() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();

        let (candidates, selected) = state.press_restore_hotkey().expect("直近の確定があるはず");
        assert!(candidates.len() >= 2, "きょうは今日/強の2候補以上あるはず");
        assert_eq!(selected, 1);
        assert!(state.is_restoring());
    }

    /// 復元中にもう一度ホットキーを押すと、選択が次の候補へ進む
    /// （Enterで確定するまでは`commit_ring`自体は変更されない）。
    #[test]
    fn pressing_restore_hotkey_again_cycles_to_next_candidate() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();
        let ring_before = state.commit_ring.back().unwrap().surface.clone();

        let (candidates, first) = state.press_restore_hotkey().unwrap();
        let (_, second) = state.press_restore_hotkey().unwrap();
        assert_eq!(second, (first + 1) % candidates.len());
        // 確定するまではリングの表記は変わらない。
        assert_eq!(state.commit_ring.back().unwrap().surface, ring_before);
    }

    /// 復元候補を確定すると、選んだ候補分のBackspace+挿入アクションが返り、
    /// リング最新エントリがその表記に更新される（再度ホットキーを押した
    /// ときの起点を今回選んだ表記に揃えるため）。
    #[test]
    fn confirm_restore_returns_action_and_updates_ring_entry() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();
        let original_char_count = state.commit_ring.back().unwrap().char_count;

        let (candidates, selected) = state.press_restore_hotkey().unwrap();
        let chosen = candidates[selected].clone();

        let action = state.confirm_restore().expect("復元選択中のはず");
        assert_eq!(action.delete_count, original_char_count);
        assert_eq!(action.insert_text, chosen);
        assert!(!state.is_restoring(), "確定後は復元選択が終わっているはず");
        assert_eq!(state.commit_ring.back().unwrap().surface, chosen);
    }

    /// 復元選択のキャンセルは選択状態だけを消し、`commit_ring`には触れない
    /// （Escで無傷のまま抜けられる設計）。
    #[test]
    fn cancel_restore_clears_selection_without_touching_ring() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();
        let ring_before = state.commit_ring.back().unwrap().surface.clone();

        state.press_restore_hotkey().unwrap();
        state.cancel_restore();

        assert!(!state.is_restoring());
        assert_eq!(state.commit_ring.back().unwrap().surface, ring_before);
    }

    /// リングが空のときにホットキーを押しても何も起きない。
    #[test]
    fn press_restore_hotkey_on_empty_ring_returns_none() {
        let mut state = state_with_two_segment_dict();
        assert!(state.press_restore_hotkey().is_none());
        assert!(!state.is_restoring());
    }

    /// Space/↓相当（`cycle_restore_candidate(false)`）は次候補へ、
    /// ↑相当（`cycle_restore_candidate(true)`）は前候補へ進む。標準IMEと
    /// 同じ挙動を復元選択中にも持たせるための機能（ユーザー指摘）。
    #[test]
    fn cycle_restore_candidate_moves_forward_and_backward() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();

        let (candidates, first) = state.press_restore_hotkey().unwrap();
        let len = candidates.len();
        assert!(len >= 2);

        let (_, forward) = state.cycle_restore_candidate(false).unwrap();
        assert_eq!(forward, (first + 1) % len);

        let (_, back_to_first) = state.cycle_restore_candidate(true).unwrap();
        assert_eq!(back_to_first, first);
    }

    /// 復元選択中でなければ巡回は何もしない。
    #[test]
    fn cycle_restore_candidate_does_nothing_when_not_restoring() {
        let mut state = state_with_two_segment_dict();
        assert!(state.cycle_restore_candidate(false).is_none());
    }

    /// 番号キーでの直接確定（`confirm_restore_at`）は、巡回を経由せず
    /// 指定インデックスをそのまま確定する。範囲外なら状態を変えずNone。
    #[test]
    fn confirm_restore_at_selects_and_commits_directly() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();
        let (candidates, _) = state.press_restore_hotkey().unwrap();
        let original_char_count = state.commit_ring.back().unwrap().char_count;
        let target = candidates.len() - 1;
        let chosen = candidates[target].clone();

        let action = state.confirm_restore_at(target).expect("有効なインデックスのはず");
        assert_eq!(action.delete_count, original_char_count);
        assert_eq!(action.insert_text, chosen);
        assert!(!state.is_restoring());
        assert_eq!(state.commit_ring.back().unwrap().surface, chosen);
    }

    /// 範囲外のインデックスを指定した場合は何も変更せずNoneを返す
    /// （復元選択中のまま、ポップアップも維持される想定）。
    #[test]
    fn confirm_restore_at_out_of_range_returns_none_and_keeps_state() {
        let mut state = state_with_two_segment_dict();
        type_str(&mut state, "kyou");
        state.commit();
        let (candidates, _) = state.press_restore_hotkey().unwrap();

        assert!(state.confirm_restore_at(candidates.len() + 5).is_none());
        assert!(state.is_restoring(), "無効な選択では復元状態が壊れてはいけない");
    }
}

/// `load_dictionary`が実ファイル（`dictionaries/word_priority.tsv`）と
/// 学習DBの両方を正しい順序で扱うかの回帰テスト。
///
/// 一度、`load_word_priority_file`を`reload_learning_into_converter`より
/// 先に呼んでいたため、優先語彙ファイルの内容が直後の`clear_learning`で
/// 毎回消えてしまうバグがあった（ゴールデンテストを学習有効/無効の両方で
/// 比較して発見。[[golden-test-harness]]）。実運用の起動シーケンス
/// （`install_hook`→`load_dictionary`）は必ず学習DBを先に設定してから
/// `load_dictionary`を呼ぶため、この順序バグは起動直後から常に踏んでいた。
#[cfg(test)]
mod load_dictionary_ordering_tests {
    use super::*;

    fn real_dictionary_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../dictionaries/system.dic")
    }

    #[test]
    fn word_priority_bonus_survives_reload_learning_into_converter() {
        let mut state = LiveConversionState::new();
        // 実運用と同じ順序: 学習DBを先に設定してからload_dictionaryを呼ぶ。
        state.learning = Some(
            crate::LearningRepository::in_memory().expect("in-memory学習DBの作成に失敗"),
        );
        assert!(
            state.load_dictionary(&real_dictionary_path()),
            "実辞書の読み込みに失敗しました（dictionaries/system.dicの存在を確認）"
        );

        // word_priority.tsvに実在するエントリ（はたち→二十歳、ボーナス3000）が
        // load_dictionary完了後も残っていること。
        let bonus = state
            .converter
            .as_ref()
            .unwrap()
            .learned_unigram
            .get(&("はたち".to_string(), "二十歳".to_string()))
            .copied();
        assert_eq!(
            bonus,
            Some(3000),
            "word_priority.tsvの「はたち→二十歳」ボーナスがload_dictionary後に消えている\
             （reload_learning_into_converterがload_word_priority_fileより後に呼ばれているか確認）"
        );
    }
}
