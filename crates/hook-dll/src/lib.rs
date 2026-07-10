use windows::Win32::{
    Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM, HMODULE},
    Graphics::Gdi::{
        BeginPaint, ClientToScreen, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW,
        EndPaint, FillRect, FrameRect, GetMonitorInfoW, InvalidateRect, MonitorFromPoint,
        SelectObject, SetBkMode, SetTextColor,
        DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HGDIOBJ, MONITORINFO,
        MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, TRANSPARENT,
    },
    UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, GetClientRect,
        GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics, GetWindowRect,
        GetWindowThreadProcessId, RegisterClassW,
        SendMessageW, SetWindowPos, SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx,
        GUITHREADINFO, HHOOK, HWND_TOPMOST, KBDLLHOOKSTRUCT, LLKHF_INJECTED, SM_CXSCREEN,
        SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_SHOWWINDOW, SW_HIDE, WINDOWS_HOOK_ID, WM_IME_CONTROL, WM_KEYDOWN, WM_KEYUP, WM_PAINT,
        WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_POPUP,
    },
    UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, INPUT_0,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        VK_BACK, VK_RETURN, VK_ESCAPE, VK_SPACE, VK_TAB, VK_RIGHT, VK_UP, VK_DOWN,
        VK_SHIFT, VK_CONTROL, VK_MENU,
        VIRTUAL_KEY,
    },
    UI::Input::Ime::{
        ImmGetDefaultIMEWnd,
        IME_CMODE_NATIVE, IME_CMODE_KATAKANA,
    },
    System::LibraryLoader::{GetModuleHandleW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT},
};
use windows::core::w;

use common::{RomajiConverter, Dictionary, ViterbiConverter, LearningRepository};
use std::path::Path;
use std::sync::Mutex;

// グローバル変数
static mut HOOK_HANDLE: Option<HHOOK> = None;
static mut LIVE_CONTEXT: Option<Mutex<LiveConversionState>> = None;
static mut IS_ENABLED: bool = false;
/// 我々がIMEとして動作中か。初期は false (まだ初回キー入力で未判定の意味も兼ねる)
static mut OUR_ACTIVE: bool = false;
/// 初回キー入力での MS-IME 状態確認を済ませたか
static mut INITIAL_CHECK_DONE: bool = false;
/// 候補一覧ウィンドウのハンドル（フックスレッドで生成・操作）
static mut CANDIDATE_HWND: Option<HWND> = None;
/// この Space 押下で既に LLM 変換を発火したか（オートリピートの二重発火防止）
static mut LLM_FIRED: bool = false;

/// UI Automation で取得したフォーカス入力欄の位置キャッシュ (x, y)
/// バックグラウンドスレッドが更新し、ポップアップ表示時に参照する。
/// ブラウザ・ターミナル等 Win32 キャレットを公開しないアプリ向け。
/// 値は (x, キャレット上端y, キャレット下端y)（画面座標）。
static UIA_ANCHOR: Mutex<Option<(i32, i32, i32)>> = Mutex::new(None);

/// UI Automation でフォーカス入力欄の位置を定期取得するスレッドを開始
///
/// クロスプロセスの同期 COM 呼び出しはブロックしうるため、フック
/// スレッドではなく専用スレッドで実行し、結果をキャッシュに置く。
fn start_uia_poller() {
    std::thread::spawn(|| unsafe {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};

        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let auto: IUIAutomation =
            match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(a) => a,
                Err(_) => return,
            };

        loop {
            let pos = uia_focused_anchor(&auto);
            if let Ok(mut c) = UIA_ANCHOR.lock() {
                *c = pos;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    });
}

/// フォーカス中の UIA 要素の入力位置を返す
///
/// まず TextPattern で実際のカーソル（選択範囲）位置を取得する。
/// これは Chrome・Electron 等の大きな入力欄でも正確。取得できない場合は
/// 要素の矩形（大きすぎる要素は除外）にフォールバックする。
unsafe fn uia_focused_anchor(
    auto: &windows::Win32::UI::Accessibility::IUIAutomation,
) -> Option<(i32, i32, i32)> {
    let elem = auto.GetFocusedElement().ok()?;

    // 1. TextPattern で選択（＝カーソル）位置を取得
    if let Some(pos) = uia_caret_from_textpattern(&elem) {
        return Some(pos);
    }

    // 2. 要素の矩形（単一行に近い入力欄向け。巨大要素は不採用）
    let r = elem.CurrentBoundingRectangle().ok()?;
    if r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    if (r.bottom - r.top) > 200 {
        return None;
    }
    Some((r.left + 2, r.top, r.bottom))
}

/// フォーカス要素からカーソルの画面座標 (x, 上端y, 下端y) を取得する
///
/// まず TextPattern2 の GetCaretRange でカーソルを直接取得する（Windows
/// Terminal など対応アプリで正確）。取れなければ TextPattern の選択範囲末尾を
/// カーソル位置とみなす。
unsafe fn uia_caret_from_textpattern(
    elem: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Option<(i32, i32, i32)> {
    use windows::Win32::UI::Accessibility::{
        IUIAutomationTextPattern, IUIAutomationTextPattern2, UIA_TextPattern2Id, UIA_TextPatternId,
    };

    // 1. TextPattern2::GetCaretRange（キャレットを直接取得）
    if let Ok(p2) = elem.GetCurrentPatternAs::<IUIAutomationTextPattern2>(UIA_TextPattern2Id) {
        let mut is_active = windows::Win32::Foundation::BOOL::default();
        if let Ok(range) = p2.GetCaretRange(&mut is_active) {
            if let Some(pos) = rect_from_text_range(&range) {
                return Some(pos);
            }
        }
    }

    // 2. TextPattern の選択範囲末尾（＝カーソル）
    let pattern: IUIAutomationTextPattern = elem.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
    let selection = pattern.GetSelection().ok()?;
    if selection.Length().ok()? < 1 {
        return None;
    }
    let range = selection.GetElement(0).ok()?;
    rect_from_text_range(&range)
}

/// テキスト範囲の境界矩形の末尾から (x, 上端y, 下端y) を取り出す
unsafe fn rect_from_text_range(
    range: &windows::Win32::UI::Accessibility::IUIAutomationTextRange,
) -> Option<(i32, i32, i32)> {
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
        SafeArrayUnaccessData,
    };

    let psa = range.GetBoundingRectangles().ok()?;
    if psa.is_null() {
        return None;
    }
    // SAFEARRAY of f64: 4個ずつ (left, top, width, height) の矩形群
    let result = (|| {
        let lb = SafeArrayGetLBound(psa, 1).ok()?;
        let ub = SafeArrayGetUBound(psa, 1).ok()?;
        let count = (ub - lb + 1).max(0) as usize;
        if count < 4 {
            return None;
        }
        let mut pdata: *mut core::ffi::c_void = std::ptr::null_mut();
        SafeArrayAccessData(psa, &mut pdata).ok()?;
        let data = std::slice::from_raw_parts(pdata as *const f64, count);
        // 最後の矩形（範囲末尾＝カーソル位置）の上端・下端
        let base = count - 4;
        let left = data[base];
        let top = data[base + 1];
        let height = data[base + 3];
        let pos = (left as i32 + 2, top as i32, (top + height) as i32);
        let _ = SafeArrayUnaccessData(psa);
        Some(pos)
    })();

    let _ = SafeArrayDestroy(psa);
    result
}

/// 候補一覧ウィンドウの表示内容（WndProc の描画と共有）
struct CandidateUi {
    items: Vec<String>,
    selected: usize,
    visible: bool,
    /// ステータス表示モード（LLM変換中などの通知。番号を付けず強調表示）
    status: bool,
}

static CANDIDATE_UI: Mutex<CandidateUi> = Mutex::new(CandidateUi {
    items: Vec::new(),
    selected: 0,
    visible: false,
    status: false,
});

/// 候補一覧の1行の高さ（px）
const CANDIDATE_LINE_HEIGHT: i32 = 24;

/// デバッグログが有効か（環境変数 IME_DEBUG_LOG=1 でオプトイン）
///
/// このログはシステム全体のキー入力を平文でファイルに残すため
/// （パスワード入力も含まれ得る）、既定では完全に無効。
/// 調査時のみ `IME_DEBUG_LOG=1` で起動して有効化すること。
fn debug_log_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("IME_DEBUG_LOG").map(|v| v == "1").unwrap_or(false)
    })
}

// デバッグログ（UTF-8 BOM付きで出力、IME_DEBUG_LOG=1 のときのみ）
#[allow(unused_macros)]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if crate::debug_log_enabled() {
            use std::io::Write;
            // ファイルオープン時にBOMを追加
            let path = "C:\\Projects\\ime-live-converter\\hook_debug.log";
            let needs_bom = !std::path::Path::new(path).exists();
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                if needs_bom {
                    let _ = file.write_all(&[0xEF, 0xBB, 0xBF]); // UTF-8 BOM
                }
                let _ = writeln!(file, "[IME] {}", format!($($arg)*));
            }
        }
    };
}

/// ライブ変換の状態
struct LiveConversionState {
    /// ローマ字→ひらがな変換
    romaji: RomajiConverter,
    /// ひらがな→漢字変換
    converter: Option<ViterbiConverter>,
    /// 現在のローマ字入力バッファ
    romaji_buffer: String,
    /// 現在のひらがなバッファ
    hiragana_buffer: String,
    /// 現在の変換結果
    conversion_result: String,
    /// 前回送信した文字数
    last_sent_length: usize,
    /// 現在の変換候補（Tab/Spaceで切替。バッファ変更でクリア）
    candidates: Vec<String>,
    /// 選択中の候補インデックス
    candidate_index: usize,
    /// 候補一覧が対象とする文節（＝直近に打った最後の文節）の読み
    cand_seg_reading: String,
    /// 候補一覧の各項目に対応する対象文節の表記（candidates と並行）
    cand_seg_surfaces: Vec<String>,
    /// 対象文節より前（確定扱いにしない、変えない部分）の変換済み表記
    cand_prefix_surface: String,
    /// 対象文節より前の読み（学習時に前半を再分解するために保持）
    cand_prefix_reading: String,
    /// 対象文節より後ろ（末尾の平仮名など）の変換済み表記
    cand_suffix_surface: String,
    /// 対象文節より後ろの読み
    cand_suffix_reading: String,
    /// この合成で → により部分確定済みの文節列（読み, 表記, 品詞）
    /// 最終確定時にユニグラム/バイグラム/内容語連想の学習へ使う。
    committed_segments: Vec<(String, String, String)>,
    /// 直近に確定したテキスト（LLM変換へ渡す前後文脈。末尾数十文字を保持）
    recent_context: String,
    /// 予測変換の候補（読み, 表記）。読みが空なら「次単語予測（追記）」。
    /// 打鍵中は前方一致補完、確定直後は次単語予測を入れる。番号キーで選ぶ。
    predictions: Vec<(String, String)>,
    /// 直近に確定した表記（次単語予測・バイグラム記録に使う）
    last_committed: String,
    /// Escで「かなに戻した」末尾の読み文字数。update_conversion は末尾の
    /// この文字数分を変換せずひらがなのまま表示する。Escを押すたびに
    /// 一つ前の文節分だけ増え、前の変換も順にひらがなへ戻す。
    kana_tail_len: usize,
    /// 入力世代。入力・確定・取消のたびに増える。非同期のLLM結果が
    /// 発火時と同じ世代のときだけ適用し、古い結果が別の位置に誤って
    /// 差し込まれる（前の入力が壊れる）のを防ぐ。
    generation: u64,
    /// 学習リポジトリ（確定履歴の記録・候補の頻度順ソート）
    learning: Option<LearningRepository>,
    /// 変換が有効かどうか
    enabled: bool,
}

impl LiveConversionState {
    fn new() -> Self {
        Self {
            romaji: RomajiConverter::new(),
            converter: None,
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
            predictions: Vec::new(),
            last_committed: String::new(),
            kana_tail_len: 0,
            generation: 0,
            learning: None,
            enabled: true,
        }
    }

    /// 学習リポジトリの内容を変換エンジンのメモリへ一括ロードする
    ///
    /// 起動時と学習DB切替時に呼ぶ。これによりライブ変換が過去の
    /// 学習を反映する（使うほど賢くなる仕組みの土台）。
    fn reload_learning_into_converter(&mut self) {
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
        debug_log!(
            "学習ロード: unigram={}, bigram={}, assoc={}",
            conv.learned_unigram.len(),
            conv.learned_bigram.len(),
            conv.learned_assoc.len()
        );
    }

    /// 予測変換の候補を更新する
    ///
    /// - 打鍵中（hiragana_buffer あり）: 読みが前方一致する確定履歴を補完候補に
    /// - 確定直後（buffer 空・last_committed あり）: 次単語をバイグラムから予測
    fn update_predictions(&mut self) {
        self.predictions.clear();
        let Some(learning) = self.learning.as_ref() else {
            return;
        };
        if !self.hiragana_buffer.is_empty() {
            if let Ok(list) = learning.predict_by_prefix(&self.hiragana_buffer, 6) {
                for (reading, surface, _f) in list {
                    self.predictions.push((reading, surface));
                }
            }
        } else if !self.last_committed.is_empty() {
            if let Ok(list) = learning.predict_next(&self.last_committed, 6) {
                for (surface, _f) in list {
                    // 1文字ひらがなの断片（さ・し 等）は予測から除く
                    let frag = surface.chars().count() == 1
                        && surface.chars().all(|c| ('\u{3041}'..='\u{3096}').contains(&c));
                    if !frag {
                        self.predictions.push((String::new(), surface));
                    }
                }
            }
        }
    }

    /// 予測候補を選んで確定する（番号キー）
    ///
    /// 前方一致補完: 現在の表示を予測語の表記に置き換えて確定。
    /// 次単語予測(読み空): 現在の表示の後ろに追記して確定。
    fn commit_prediction(&mut self, index: usize) -> Option<ConversionAction> {
        if !self.enabled || index >= self.predictions.len() {
            return None;
        }
        let (reading, surface) = self.predictions[index].clone();
        // 前方一致は現在表示を置換、次単語は追記
        let delete_count = if reading.is_empty() {
            0
        } else {
            self.conversion_result.chars().count()
        };
        let action = ConversionAction {
            delete_count,
            insert_text: surface.clone(),
        };

        // 学習（確定として記録）
        if let Some(learning) = self.learning.as_ref() {
            if !reading.is_empty() && is_learnable_pair(&reading, &surface) {
                let _ = learning.record_commit(&reading, &surface, None);
                let freq = learning.find_frequency(&reading, &surface).unwrap_or(1);
                if let Some(conv) = self.converter.as_mut() {
                    conv.learn_unigram(&reading, &surface, freq);
                }
            }
            if !self.last_committed.is_empty() {
                let _ = learning.record_bigram(&self.last_committed, &surface);
                let freq = learning
                    .find_bigram_frequency(&self.last_committed, &surface)
                    .unwrap_or(1);
                if let Some(conv) = self.converter.as_mut() {
                    conv.learn_bigram(&self.last_committed, &surface, freq);
                }
            }
        }

        // 文脈・状態を更新して確定扱いにする
        self.recent_context.push_str(&surface);
        let chars: Vec<char> = self.recent_context.chars().collect();
        if chars.len() > 60 {
            self.recent_context = chars[chars.len() - 60..].iter().collect();
        }
        self.last_committed = surface;
        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.conversion_result.clear();
        self.last_sent_length = 0;
        self.committed_segments.clear();
        self.kana_tail_len = 0;
        self.generation = self.generation.wrapping_add(1);
        self.clear_candidates();
        // 確定後は次単語予測を用意
        self.update_predictions();
        Some(action)
    }

    /// 予測候補の表示用文字列（番号は描画側で付くので表記のみ）
    fn prediction_display(&self) -> Vec<String> {
        self.predictions.iter().map(|(_, s)| s.clone()).collect()
    }

    /// 候補一覧の状態をすべてクリア
    fn clear_candidates(&mut self) {
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
    fn segment_remaining(&self) -> Vec<(String, String, String)> {
        let Some(conv) = self.converter.as_ref() else {
            return Vec::new();
        };
        if self.hiragana_buffer.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.cand_seg_surfaces.is_empty()
            && self.candidate_index < self.cand_seg_surfaces.len()
            && !self.cand_seg_reading.is_empty()
        {
            // 候補選択あり: 前半 + 選択した対象文節 + 後半 で分解
            for e in conv.convert(&self.cand_prefix_reading) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
            let seg_surface = self.cand_seg_surfaces[self.candidate_index].clone();
            // 候補選択した語は内容語とみなす（品詞は名詞相当を既定に）
            out.push((self.cand_seg_reading.clone(), seg_surface, "名詞-一般".to_string()));
            for e in conv.convert(&self.cand_suffix_reading) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
        } else if self.kana_tail_len > 0 {
            // Escで戻した末尾はかなのまま（学習しない）、前半だけ変換して学習
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(self.kana_tail_len);
            let prefix: String = self.hiragana_buffer.chars().take(keep).collect();
            for e in conv.convert(&prefix) {
                out.push((e.reading.clone(), e.surface.clone(), e.pos.clone()));
            }
            let tail: String = self.hiragana_buffer.chars().skip(keep).collect();
            if !tail.is_empty() {
                out.push((tail.clone(), tail, "名詞-一般".to_string()));
            }
        } else {
            for e in conv.convert(&self.hiragana_buffer) {
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
    fn learn_from_segments(&mut self, segments: &[(String, String, String)]) {
        let Some(learning) = self.learning.as_ref() else {
            return;
        };
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
        }
        // 内容語連想（助詞・助動詞を飛ばした内容語どうしの繋がり）
        // 「会社…帰社」「新聞…記者」のように離れた語の関係を学習する。
        let mut last_content: Option<String> = None;
        for (reading, surface, pos) in segments {
            if !common::viterbi::is_content_pos(pos) || surface == reading || surface.is_empty() {
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
    }

    /// 辞書をロード
    fn load_dictionary(&mut self, path: &Path) -> bool {
        debug_log!("Dictionary::load 呼び出し: {}", path.display());
        match Dictionary::load(path) {
            Ok(dict) => {
                debug_log!("辞書読み込み成功、ViterbiConverter作成中");
                self.converter = Some(ViterbiConverter::new(dict));
                // 過去の学習をライブ変換エンジンに反映
                self.reload_learning_into_converter();
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
    fn add_char(&mut self, ch: char) -> Option<ConversionAction> {
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
            self.hiragana_buffer.push_str(&settled);
            self.romaji_buffer = pending;
        }

        debug_log!("現在の状態: ひらがな='{}', ローマ字='{}'", self.hiragana_buffer, self.romaji_buffer);

        // ひらがな→漢字変換（ライブ変換）
        self.update_conversion()
    }

    /// バックスペース処理
    fn backspace(&mut self) -> Option<ConversionAction> {
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
                .converter
                .as_ref()
                .and_then(|c| c.convert(&self.hiragana_buffer).last().map(|e| e.reading.chars().count()))
                .filter(|&n| n > 0)
                .unwrap_or(1);
            let total = self.hiragana_buffer.chars().count();
            let keep = total.saturating_sub(last_len);
            self.hiragana_buffer = self.hiragana_buffer.chars().take(keep).collect();
        } else {
            return None; // 削除するものがない
        }

        debug_log!("バックスペース後: ひらがな='{}', ローマ字='{}'", self.hiragana_buffer, self.romaji_buffer);
        self.update_conversion()
    }

    /// 変換を更新（macOS方式：ひらがな確定時のみ漢字変換）
    ///
    /// 共通プレフィックスを保持する差分計算で必要最小限の編集のみを送信する。
    /// これにより「今日」→「今日h」のように先頭が共通な場合は cursor を動かさず
    /// 末尾だけ更新できる。旧実装(毎回全削除→再挿入)は cursor が頻繁に左に飛び、
    /// 視覚的に「後の入力が前を上書きする」ように見える原因だった。
    fn update_conversion(&mut self) -> Option<ConversionAction> {
        // ひらがなバッファのみを漢字変換
        // ローマ字バッファはそのまま末尾に追加
        let converted_hiragana = if let Some(converter) = &self.converter {
            if self.hiragana_buffer.is_empty() {
                String::new()
            } else if self.kana_tail_len > 0 {
                // Escで戻した末尾はひらがなのまま、前半だけ変換する
                let total = self.hiragana_buffer.chars().count();
                let keep = total.saturating_sub(self.kana_tail_len);
                let prefix: String = self.hiragana_buffer.chars().take(keep).collect();
                let tail: String = self.hiragana_buffer.chars().skip(keep).collect();
                let conv = if prefix.is_empty() {
                    String::new()
                } else {
                    converter.convert_context_aware_to_string(&prefix)
                };
                format!("{}{}", conv, tail)
            } else {
                // 文脈（内容語の繋がり）を考慮して尤もらしい変換を選ぶ
                converter.convert_context_aware_to_string(&self.hiragana_buffer)
            }
        } else {
            // 辞書がない場合はひらがなのまま
            debug_log!("辞書なし: ひらがなのまま '{}'", self.hiragana_buffer);
            self.hiragana_buffer.clone()
        };

        // 変換結果 + ローマ字（未確定）
        let new_result = format!("{}{}", converted_hiragana, self.romaji_buffer);
        self.apply_new_result(new_result)
    }

    /// 表示テキストを new_result に差し替えるための差分アクションを作る
    fn apply_new_result(&mut self, new_result: String) -> Option<ConversionAction> {
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
    fn cycle_candidate(&mut self, backwards: bool) -> Option<ConversionAction> {
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

    /// Escで、まだ変換されている末尾の文節を一つ、ひらがなに戻す
    ///
    /// 呼ぶたびに末尾から一文節ずつ戻していく（累積）。戻す対象が
    /// 残っていれば表示を更新するアクションを返し、全てひらがなに
    /// 戻し終えていれば `None`（呼び出し側は取消にフォールバック）。
    fn extend_kana_revert(&mut self) -> Option<ConversionAction> {
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
        let Some(converter) = self.converter.as_ref() else {
            return None;
        };
        let entries = converter.convert(&prefix);
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
    fn build_candidates(
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

        // ライブ表示と同じ context_aware で分解する。
        let entries = converter.convert_context_aware(&self.hiragana_buffer);
        if entries.is_empty() {
            return empty();
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
        while start > 0
            && is_single_kanji_entry(&entries[start])
            && is_single_kanji_entry(&entries[start - 1])
        {
            start -= 1;
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

        // 対象文節の同音語を集め、「実際の使いやすさ」を近似したキーで並べる
        // 並び順: 学習頻度が高い順 → 実効コストが低い順
        //   実効コスト = 辞書コスト + 1文字漢字ペナルティ
        //   （1文字の漢字は単独語として使われることが稀で、IPA辞書の
        //    コストが実際の出現頻度より低く出るため補正する）
        struct Seg {
            surface: String,
            freq: u32,
            eff_cost: i32,
        }
        let mut segs: Vec<Seg> = Vec::new();
        if let Some(words) = converter.dictionary.lookup(&seg_reading) {
            for w in words {
                let freq = self
                    .learning
                    .as_ref()
                    .and_then(|l| l.find_frequency(&seg_reading, &w.surface).ok())
                    .unwrap_or(0);
                let penalty = single_kanji_penalty(&w.surface);
                segs.push(Seg {
                    surface: w.surface.clone(),
                    freq,
                    eff_cost: w.cost as i32 + penalty,
                });
            }
        }
        // カタカナ・ひらがな表記も候補に含める（末尾寄り）
        let katakana = common::hiragana_to_katakana(&seg_reading);
        if katakana != seg_reading {
            segs.push(Seg { surface: katakana, freq: 0, eff_cost: 30000 });
        }
        segs.push(Seg { surface: seg_reading.clone(), freq: 0, eff_cost: 32000 });

        // 学習頻度が高い順 → 実効コストが低い順
        // （文全体の 1-best は IPA コストの癖で 1文字漢字を選ぶことがあるため、
        //  ここでは 1-best を先頭固定せず、ペナルティ込みコスト順に任せる）
        segs.sort_by(|a, b| b.freq.cmp(&a.freq).then(a.eff_cost.cmp(&b.eff_cost)));

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
            if candidates.len() >= 9 {
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
    fn select_candidate(&mut self, index: usize) -> Option<ConversionAction> {
        if index >= self.candidates.len() {
            return None;
        }
        self.candidate_index = index;

        debug_log!(
            "候補選択: {}/{} '{}'",
            index + 1, self.candidates.len(), self.candidates[index]
        );

        let new_result = format!("{}{}", self.candidates[index], self.romaji_buffer);
        self.apply_new_result(new_result)
    }

    /// 先頭の1語を部分確定する（→キー）
    ///
    /// 「前半の変換は正しいが後半が違う」場合に、正しい前半を語単位で
    /// 順に確定していくための操作。確定した分はひらがなバッファから
    /// 外れるため、以降の Tab 候補切替は残り部分だけに効く。
    fn commit_first_word(&mut self) -> Vec<ConversionAction> {
        let mut actions = Vec::new();
        if !self.enabled || self.hiragana_buffer.is_empty() {
            return actions;
        }
        let Some(converter) = &self.converter else {
            return actions;
        };

        let entries = converter.convert(&self.hiragana_buffer);
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
    fn commit(&mut self) -> Option<ConversionAction> {
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
        self.learn_from_segments(&segments);

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
        }
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
        self.kana_tail_len = 0;
        self.generation = self.generation.wrapping_add(1);
        self.clear_candidates();
        // 確定直後は次単語予測を用意する
        self.update_predictions();

        action
    }

    /// キャンセル（Escキー）
    fn cancel(&mut self) -> Option<ConversionAction> {
        if !self.enabled {
            return None;
        }

        let delete_count = self.last_sent_length;

        self.romaji_buffer.clear();
        self.hiragana_buffer.clear();
        self.conversion_result.clear();
        self.last_sent_length = 0;
        self.committed_segments.clear();
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
    fn is_composing(&self) -> bool {
        !self.romaji_buffer.is_empty() || !self.hiragana_buffer.is_empty()
    }
}

/// 変換アクション（何を削除して何を挿入するか）
struct ConversionAction {
    delete_count: usize,
    insert_text: String,
}

/// 単語エントリが「1文字の漢字」か（隣接漢字の結合判定に使う）
fn is_single_kanji_entry(e: &common::WordEntry) -> bool {
    let mut chars = e.surface.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => {
            ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
        }
        _ => false,
    }
}

/// 1文字の漢字表記に対するコストペナルティ
///
/// IPA辞書は解析用のため、1文字漢字の名詞（教・卿・挟 など）が
/// 単独語として実際の出現頻度より低コストに設定されていることが多い。
/// かな漢字変換の候補一覧ではこれらが上位に来ると邪魔なので、
/// 候補並べ替え用に実効コストを底上げする（辞書自体は変更しない）。
fn single_kanji_penalty(surface: &str) -> i32 {
    let mut chars = surface.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return 0; // 2文字以上は対象外
    };
    let is_kanji = ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c);
    if is_kanji {
        2000
    } else {
        0
    }
}

// ============ 候補一覧ウィンドウ ============

/// 候補一覧ウィンドウの WndProc
/// 最前面維持タイマーのID
const TOPMOST_TIMER_ID: usize = 1;

extern "system" fn candidate_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, WINDOWPOS, WM_TIMER, WM_WINDOWPOSCHANGING, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER,
    };
    match msg {
        WM_PAINT => {
            unsafe { paint_candidates(hwnd) };
            LRESULT(0)
        }
        // Z順が変更されるたびに「最前面(HWND_TOPMOST)」を強制し、他ウィンドウに
        // 前面を奪われないようにする（環境によって背面に回るのを防ぐ）。
        WM_WINDOWPOSCHANGING => {
            unsafe {
                let wp = lparam.0 as *mut WINDOWPOS;
                if !wp.is_null() {
                    (*wp).hwndInsertAfter = HWND_TOPMOST;
                    (*wp).flags &= !SWP_NOZORDER;
                }
            }
            LRESULT(0)
        }
        // 表示中は定期的に最前面へ再指定（他アプリが後から前面化しても復帰）
        WM_TIMER => {
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0, 0, 0, 0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 候補一覧を描画
unsafe fn paint_candidates(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let (items, selected, status) = match CANDIDATE_UI.lock() {
        Ok(ui) => (ui.items.clone(), ui.selected, ui.status),
        Err(_) => {
            let _ = EndPaint(hwnd, &ps);
            return;
        }
    };

    let mut rc_client = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc_client);

    // 背景と枠（ステータス表示中はアクセント色の背景で目立たせる）
    let bg_color = if status { COLORREF(0x00D77800) } else { COLORREF(0x00FFFFFF) };
    let bg = CreateSolidBrush(bg_color);
    FillRect(hdc, &rc_client, bg);
    let _ = DeleteObject(HGDIOBJ::from(bg));
    let frame = CreateSolidBrush(COLORREF(0x00999999));
    FrameRect(hdc, &rc_client, frame);
    let _ = DeleteObject(HGDIOBJ::from(frame));

    // 日本語が読みやすいフォント
    // (charset=DEFAULT_CHARSET(1), precision/clip=default(0),
    //  quality=CLEARTYPE_QUALITY(5), pitch/family=default(0))
    let font = CreateFontW(
        -16, 0, 0, 0, 400, 0, 0, 0,
        1, 0, 0, 5, 0,
        w!("Meiryo UI"),
    );
    let old_font = SelectObject(hdc, HGDIOBJ::from(font));
    SetBkMode(hdc, TRANSPARENT);

    for (i, item) in items.iter().enumerate() {
        let top = 4 + (i as i32) * CANDIDATE_LINE_HEIGHT;
        let mut rc = RECT {
            left: 4,
            top,
            right: rc_client.right - 4,
            bottom: top + CANDIDATE_LINE_HEIGHT,
        };

        // ステータス表示: 番号を付けず白文字でそのまま描画
        if status {
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
            let mut wide: Vec<u16> = item.encode_utf16().collect();
            rc.left += 8;
            DrawTextW(hdc, &mut wide, &mut rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            continue;
        }

        if i == selected {
            // 選択行はアクセント色で強調 (RGB 0,120,215 / COLORREF は 0x00BBGGRR)
            let hl = CreateSolidBrush(COLORREF(0x00D77800));
            FillRect(hdc, &rc, hl);
            let _ = DeleteObject(HGDIOBJ::from(hl));
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
        } else {
            SetTextColor(hdc, COLORREF(0x00000000));
        }

        let text = format!("{}  {}", i + 1, item);
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        rc.left += 8;
        DrawTextW(hdc, &mut wide, &mut rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }

    SelectObject(hdc, old_font);
    let _ = DeleteObject(HGDIOBJ::from(font));
    let _ = EndPaint(hwnd, &ps);
}

/// 候補一覧ウィンドウを（なければ作って）返す
///
/// フックを張ったスレッド（conversion-service のメインスレッド）で
/// 呼ばれるため、そのスレッドの既存メッセージループが描画を駆動する。
unsafe fn ensure_candidate_window() -> Option<HWND> {
    if let Some(hwnd) = CANDIDATE_HWND {
        return Some(hwnd);
    }

    let hinstance = GetModuleHandleW(None).ok()?;
    let class_name = w!("ImeLiveCandidateList");

    let wc = WNDCLASSW {
        lpfnWndProc: Some(candidate_wndproc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    // 二重登録はエラーになるが、その場合も既存クラスが使えるので無視
    RegisterClassW(&wc);

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        class_name,
        w!(""),
        WS_POPUP,
        0, 0, 10, 10,
        None,
        None,
        hinstance,
        None,
    )
    .ok()?;

    CANDIDATE_HWND = Some(hwnd);
    Some(hwnd)
}

/// 候補ウィンドウの表示位置を決める（入力位置の近く）
///
/// 優先順位（いずれもブロックしない軽量 API のみ）:
/// 1. テキストキャレットの真下（メモ帳・conhost など Win32 が
///    キャレット位置を公開するアプリ）
/// 2. フォアグラウンドウィンドウの下部・左寄り
///    （Windows Terminal 等はキャレットを公開しないが、ターミナルは
///    出力が上へ流れて入力行が下端付近にあるため、ここが入力位置に近い）
///
/// マウスカーソル位置は使わない（打鍵中に動かず、たまたまある場所に
/// 出て「右上に飛ぶ」ように見えるため）。
///
/// 注: UI Automation でブラウザ内の入力要素を正確に狙うことも可能だが、
/// クロスプロセスの同期 COM 呼び出しはフックスレッドをブロックして
/// 候補ウィンドウの描画を止めてしまうため使わない。
///
/// 戻り値は (x, キャレット上端y, キャレット下端y)（画面座標）。
/// place_popup がこの上下端を見て、真下に余白があれば下、無ければ真上に出す
/// （既存IMEと同様の出し分け）。
fn caret_screen_pos() -> (i32, i32, i32) {
    unsafe {
        let hwnd_fg = GetForegroundWindow();
        if !hwnd_fg.0.is_null() {
            let tid = GetWindowThreadProcessId(hwnd_fg, None);
            let mut gti = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            // 1. テキストキャレット（メモ帳・多くの Win32 エディタ）
            if GetGUIThreadInfo(tid, &mut gti).is_ok()
                && !gti.hwndCaret.0.is_null()
                && (gti.rcCaret.bottom > gti.rcCaret.top || gti.rcCaret.right > gti.rcCaret.left)
            {
                let mut top = POINT { x: gti.rcCaret.left, y: gti.rcCaret.top };
                let mut bot = POINT { x: gti.rcCaret.left, y: gti.rcCaret.bottom };
                let _ = ClientToScreen(gti.hwndCaret, &mut top);
                let _ = ClientToScreen(gti.hwndCaret, &mut bot);
                return (top.x, top.y, bot.y);
            }

            // 2. UI Automation で取得した入力欄の位置（ブラウザ・ターミナル等）
            if let Ok(cache) = UIA_ANCHOR.lock() {
                if let Some((x, t, b)) = *cache {
                    return (x, t, b);
                }
            }

            // 3. フォアグラウンドウィンドウの最下行を「入力行」とみなす。
            //    ターミナル等はキャレットを公開せずここに来る。最下行を
            //    1行分の高さのキャレットとして扱い、その上に出す。
            let mut rc = RECT::default();
            if GetWindowRect(hwnd_fg, &mut rc).is_ok() && rc.bottom > rc.top {
                let x = rc.left + 24;
                return (x, rc.bottom - CANDIDATE_LINE_HEIGHT, rc.bottom);
            }
        }
        // 取得できない場合は画面左下寄りに固定表示（少なくとも見える）
        let h = GetSystemMetrics(SM_CYSCREEN);
        (80, h - CANDIDATE_LINE_HEIGHT, h)
    }
}

/// 候補一覧ウィンドウを表示・更新
fn show_candidate_window(items: &[String], selected: usize) {
    if items.is_empty() {
        hide_candidate_window();
        return;
    }
    if let Ok(mut ui) = CANDIDATE_UI.lock() {
        ui.items = items.to_vec();
        ui.selected = selected;
        ui.visible = true;
        ui.status = false;
    }
    let max_len = items.iter().map(|s| s.chars().count()).max().unwrap_or(1);
    place_popup(max_len, items.len() as i32);
}

/// 予測変換の候補をカーソル付近に表示する（番号キーで選択）
fn show_prediction_popup(items: &[String]) {
    if items.is_empty() {
        hide_candidate_window();
        return;
    }
    // 予測はどれも「選択中」ではないので usize::MAX でハイライトなし
    show_candidate_window(items, usize::MAX);
}

/// LLM変換中のステータスをカーソル付近に表示する（変換エフェクト）
fn show_llm_status(text: &str) {
    if let Ok(mut ui) = CANDIDATE_UI.lock() {
        ui.items = vec![text.to_string()];
        ui.selected = 0;
        ui.visible = true;
        ui.status = true;
    }
    place_popup(text.chars().count(), 1);
}

/// ポップアップ（候補/ステータス）を組み立ててカーソル付近に配置・表示する
fn place_popup(max_len_chars: usize, line_count: i32) {
    unsafe {
        let Some(hwnd) = ensure_candidate_window() else { return };

        let width = ((max_len_chars + 4) * 16 + 24).min(640) as i32;
        let height = 8 + line_count * CANDIDATE_LINE_HEIGHT;
        let (mut x, caret_top, caret_bottom) = caret_screen_pos();

        // 基点が乗っているモニターの作業領域を取得してクランプする
        // （SM_CXSCREEN は主モニターのみなので、マルチモニターだと
        //  副モニターの座標が主モニター右端に丸められ「右上」に飛ぶ）
        let mon = MonitorFromPoint(POINT { x, y: caret_bottom }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let (left, top, right, bottom) = if GetMonitorInfoW(mon, &mut mi).as_bool() {
            (mi.rcWork.left, mi.rcWork.top, mi.rcWork.right, mi.rcWork.bottom)
        } else {
            (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        };

        // 既存IMEと同様: キャレットの真下に余白があれば下、無ければ真上に出す。
        // これで入力文字（カーソル）にリストが被らない。
        let below_top = caret_bottom + 2;
        let above_top = caret_top - height - 2;
        let mut y = if below_top + height <= bottom {
            below_top
        } else {
            above_top
        };

        x = x.clamp(left, (right - width).max(left));
        y = y.clamp(top, (bottom - height).max(top));

        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x, y, width, height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        // 前面固定を確実にするため最前面を再指定（位置・サイズは維持）
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0, 0, 0, 0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
        // 表示中は定期的に最前面へ戻すタイマーを起動（他アプリに前面を
        // 奪われても復帰させる。環境依存の背面化対策）
        let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
            hwnd,
            TOPMOST_TIMER_ID,
            200,
            None,
        );
        let _ = InvalidateRect(hwnd, None, true);
    }
}

/// 候補一覧ウィンドウを隠す
fn hide_candidate_window() {
    unsafe {
        let was_visible = match CANDIDATE_UI.lock() {
            Ok(mut ui) => {
                let v = ui.visible;
                ui.visible = false;
                ui.status = false;
                v
            }
            Err(_) => false,
        };
        if was_visible {
            if let Some(hwnd) = CANDIDATE_HWND {
                let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(hwnd, TOPMOST_TIMER_ID);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

/// 候補一覧が表示中か
fn candidate_window_visible() -> bool {
    CANDIDATE_UI.lock().map(|ui| ui.visible).unwrap_or(false)
}

/// VKコード→文字変換
///
/// 返した文字は RomajiConverter に渡り、句読点は
/// `,`→、 `.`→。 `-`→ー `?`→？ `!`→！ `[`→「 `]`→」 に変換される。
fn vk_to_char(vk_code: u32, _scan_code: u32, shift_pressed: bool) -> Option<char> {
    // A-Z はローマ字入力用にそのまま英字を返す
    if (0x41..=0x5A).contains(&vk_code) {
        let base = (vk_code as u8 - 0x41) + b'a';
        return if shift_pressed {
            Some((base - 32) as char) // 大文字（romaji側で小文字化される）
        } else {
            Some(base as char)
        };
    }

    // 日本語入力でよく使う記号のみ、レイアウト非依存の OEM VK で拾う。
    // 返した記号は add_char → RomajiConverter で全角化される
    // （, . - ? ! [ ] のみ。& @ / 等は None を返して半角のまま素通し）。
    match vk_code {
        0xBC if !shift_pressed => Some(','), // VK_OEM_COMMA → 、
        0xBE if !shift_pressed => Some('.'), // VK_OEM_PERIOD → 。
        0xBD if !shift_pressed => Some('-'), // VK_OEM_MINUS → ー
        0xBF if shift_pressed => Some('?'),  // VK_OEM_2 shift → ？
        0x31 if shift_pressed => Some('!'),  // '1' shift → ！
        0xDB if !shift_pressed => Some('['), // VK_OEM_4 → 「
        0xDD if !shift_pressed => Some(']'), // VK_OEM_6 → 」
        _ => None,
    }
}

/// 入力を確定させる文字（句読点・終端記号）か
///
/// 日本語IMEの一般的な挙動に合わせ、句読点の入力でそれまでの
/// 変換を自動確定する。
fn is_commit_char(ch: char) -> bool {
    matches!(ch, ',' | '.' | '?' | '!')
}

/// この (読み, 表記) ペアを学習してよいか
///
/// 誤学習で変換が悪化するのを防ぐガード:
/// - 助詞の読み（に・は・を 等）が漢字/数字に化けたもの（例 に→二）は
///   学習しない。助詞は常に既定表記であるべき。
/// - 英数字を含む表記（旧ローマ字バグ由来のゴミ等）は学習しない。
fn is_learnable_pair(reading: &str, surface: &str) -> bool {
    // 助詞・助動詞になりうる短い仮名の読み（これらが漢字化したら誤り）
    const PARTICLE_READINGS: &[&str] = &[
        "に", "は", "を", "へ", "が", "の", "と", "も", "や", "か",
        "で", "ね", "よ", "わ", "し", "ば", "な", "ぞ", "さ",
    ];
    if PARTICLE_READINGS.contains(&reading) && surface != reading {
        return false;
    }
    // 英数字混じりの表記は学習対象外（ゴミ・未変換ローマ字）
    if surface.chars().any(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let reading_len = reading.chars().count();
    // 1文字の読み→別表記は曖昧すぎる断片（例: じ→時）。誤変換の
    // 学習ループを招くので学習しない。
    if reading_len == 1 && surface != reading {
        return false;
    }
    // 短い読みを「その読みのカタカナ化」として学習しない（例: かん→カン）。
    // これはカタカナ・フォールバックの断片で、誤変換を強化してしまう。
    if reading_len <= 3 && surface == common::hiragana_to_katakana(reading) {
        return false;
    }
    true
}

/// Shiftキーが押されているか確認
fn is_shift_pressed() -> bool {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        GetAsyncKeyState(VK_SHIFT.0 as i32) < 0
    }
}

/// Ctrlキーが押されているか確認
fn is_ctrl_pressed() -> bool {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        GetAsyncKeyState(VK_CONTROL.0 as i32) < 0
    }
}

/// Altキーが押されているか確認
fn is_alt_pressed() -> bool {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        GetAsyncKeyState(VK_MENU.0 as i32) < 0
    }
}

// WM_IME_CONTROL wparam 定数 (windows crate に未定義のため手動定義)
// https://learn.microsoft.com/en-us/windows/win32/intl/wm-ime-control
const IMC_GETCONVERSIONMODE: usize = 0x0001;
const IMC_GETOPENSTATUS: usize = 0x0005;
const IMC_SETOPENSTATUS: usize = 0x0006;

/// 半角/全角キー (IME mode toggle) の vkCode 群
///
/// 環境によって発火する vk が異なるため複数を OR で見る:
/// - 0x19  : VK_KANJI / VK_HANJA  漢字キー
/// - 0xF3  : VK_DBE_DBCSCHAR / VK_OEM_AUTO  全角化キー
/// - 0xF4  : VK_DBE_SBCSCHAR / VK_OEM_ENLW  半角化キー
fn is_ime_toggle_vk(vk: u32) -> bool {
    matches!(vk, 0x19 | 0xF3 | 0xF4)
}

/// フォアグラウンドウィンドウの MS-IME を強制的に閉じる
///
/// 我々が SendInput KEYEVENTF_UNICODE で送る文字を MS-IME が
/// composition として取り込むのを防ぐ。IMC_SETOPENSTATUS=0 で
/// IME 全体を閉じる(=半角英数モード相当)。
fn close_ms_ime_for_foreground() {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG};
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return;
        }
        let ime_wnd = ImmGetDefaultIMEWnd(hwnd);
        if ime_wnd.0.is_null() {
            return;
        }
        // ブロックしないようタイムアウト付きで送る（相手が固まっても最大30ms）
        let _ = SendMessageTimeoutW(
            ime_wnd,
            WM_IME_CONTROL,
            WPARAM(IMC_SETOPENSTATUS),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            30,
            None,
        );
    }
}

/// 我々のアクティブ状態をトグルする
///
/// アクティブ化時は MS-IME を閉じてコンポジションを無効化。
/// 非アクティブ化時はそのまま閉じた状態を維持 (ユーザーは英数モードに戻りたいはず)。
fn toggle_our_active() {
    unsafe {
        OUR_ACTIVE = !OUR_ACTIVE;
        debug_log!("OUR_ACTIVE トグル: {}", OUR_ACTIVE);
        hide_candidate_window();
        if OUR_ACTIVE {
            close_ms_ime_for_foreground();
            // 入力中だった composition バッファをクリア
            if let Some(context_mutex) = &mut LIVE_CONTEXT {
                if let Ok(mut c) = context_mutex.try_lock() {
                    c.romaji_buffer.clear();
                    c.hiragana_buffer.clear();
                    c.conversion_result.clear();
                    c.last_sent_length = 0;
                }
            }
        }
    }
}

/// フォアグラウンドアプリのIMEがひらがな入力モードか判定する
///
/// Low-Level Keyboard Hook は別スレッドで動くため `ImmGetContext` が常にNULLになる。
/// 代わりに `ImmGetDefaultIMEWnd(hwnd)` で IME ウィンドウを取得し、
/// `SendMessageW(WM_IME_CONTROL, IMC_GETOPENSTATUS / IMC_GETCONVERSIONMODE)` で
/// IME 本体スレッドに問い合わせる（スレッドセーフな経路）。
///
/// 判定:
/// - IME ウィンドウが取れない → 「IMEなしのアプリ(コンソール等)」と判断、パススルー(false)
/// - IMC_GETOPENSTATUS が FALSE → IME OFF (半角英数モード相当) → パススルー
/// - IMC_GETCONVERSIONMODE で IME_CMODE_NATIVE が立っていない → 英数モード → パススルー
/// - IME_CMODE_KATAKANA が立っている → カタカナモード → パススルー
/// - それ以外 (NATIVE && !KATAKANA) → ひらがなモード → 変換ON
fn is_ime_hiragana_mode() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            debug_log!("IMEモード: フォアグラウンドウィンドウなし - パススルー");
            return false;
        }

        let ime_wnd = ImmGetDefaultIMEWnd(hwnd);
        if ime_wnd.0.is_null() {
            // IMEを持たないアプリ。半角英数のキーをそのまま打ちたいケースなのでパススルー。
            debug_log!("IMEモード: IMEウィンドウなし(IME非対応アプリ) - パススルー");
            return false;
        }

        // STEP 1: IME が開いているか
        let open_status = SendMessageW(
            ime_wnd,
            WM_IME_CONTROL,
            WPARAM(IMC_GETOPENSTATUS),
            LPARAM(0),
        );
        if open_status.0 == 0 {
            debug_log!("IMEモード: IME OFF (半角英数モード) - パススルー");
            return false;
        }

        // STEP 2: 変換モードを取得
        let conv_mode = SendMessageW(
            ime_wnd,
            WM_IME_CONTROL,
            WPARAM(IMC_GETCONVERSIONMODE),
            LPARAM(0),
        );
        let mode_bits = conv_mode.0 as u32;
        let is_native = (mode_bits & IME_CMODE_NATIVE.0) != 0;
        let is_katakana = (mode_bits & IME_CMODE_KATAKANA.0) != 0;

        // ひらがなモード = NATIVE 立ち & KATAKANA 立たず
        let is_hiragana = is_native && !is_katakana;

        debug_log!(
            "IMEモード: conv=0x{:X}, native={}, katakana={}, hiragana_mode={}",
            mode_bits, is_native, is_katakana, is_hiragana
        );
        is_hiragana
    }
}

/// 変換アクションを 1 回の SendInput でアトミックに実行
///
/// 削除(BS)と挿入(KEYEVENTF_UNICODE)を別々の SendInput 呼び出しにすると
/// 呼び出し間で IME がコンポジション状態を変えてしまい、後の挿入が前を
/// 上書きするように見える。Vec<INPUT> をまとめて 1 回で渡せば BS と
/// 挿入の間に他処理が割り込めない。
/// Tab 長押しで LLM 変換をバックグラウンド発火する
///
/// 現在の読み（ひらがなバッファ）をスナップショットし、別スレッドで
/// ローカルLLM（Ollama）に変換させる。入力はブロックしない。
/// 結果が返った時点で読みが変わっていなければ、合成中テキストを
/// LLM の結果に差し替える。LLM 未起動・失敗時は何もしない。
fn trigger_llm_conversion() {
    // 読み・統計変換の下書き・N-best候補・文脈・世代をスナップショット
    let (reading, draft, candidates, context, gen) = unsafe {
        match &LIVE_CONTEXT {
            Some(mtx) => match mtx.try_lock() {
                Ok(ctx) => {
                    if ctx.hiragana_buffer.is_empty() {
                        return;
                    }
                    let Some(conv) = ctx.converter.as_ref() else { return };
                    // 統計エンジンの結果を下書き、N-bestを参考候補としてLLMに渡す
                    let draft = conv.convert_context_aware_to_string(&ctx.hiragana_buffer);
                    let cands = conv.n_best_strings(&ctx.hiragana_buffer, 6);
                    (
                        ctx.hiragana_buffer.clone(),
                        draft,
                        cands,
                        ctx.recent_context.clone(),
                        ctx.generation,
                    )
                }
                Err(_) => return,
            },
            None => return,
        }
    };

    debug_log!("LLM校正 発火: 読み='{}' 下書き='{}'", reading, draft);
    show_llm_status("🤖 LLMが校正中…");

    std::thread::spawn(move || {
        let cfg = common::LlmConfig::from_env();
        // 読み＋下書き＋参考候補を渡し、誤字・文法を直させる（厳密に正しい文へ）
        let corrected = common::llm_correct(&cfg, &reading, &draft, &candidates, &context);
        hide_candidate_window();

        let Some(corrected) = corrected else {
            debug_log!("LLM校正 応答なし（下書きを維持）");
            return;
        };
        debug_log!("LLM校正 結果: '{}'", corrected);

        // 校正結果を反映（読みが変わっていなければ）
        unsafe {
            let Some(mtx) = &LIVE_CONTEXT else { return };
            let Ok(mut ctx) = mtx.lock() else { return };
            // 発火時から入力世代が変わっていたら破棄（その間に入力・確定・
            // 取消があった → 適用すると別位置に差し込まれ前の入力を壊す）
            if !ctx.enabled || ctx.generation != gen || ctx.hiragana_buffer != reading {
                debug_log!("LLM校正: 世代不一致のため破棄");
                return;
            }
            if let Some(action) = ctx.apply_new_result(corrected) {
                drop(ctx);
                execute_action(action);
            }
        }
    });
}

fn execute_action(action: ConversionAction) {
    if action.delete_count == 0 && action.insert_text.is_empty() {
        return;
    }

    // 文字を送り込む直前に MS-IME を閉じ、注入文字が MS-IME に
    // コンポジションとして拾われる（二重変換・競合）のを防ぐ。
    close_ms_ime_for_foreground();

    let mut inputs: Vec<INPUT> = Vec::with_capacity(
        action.delete_count * 2 + action.insert_text.chars().count() * 2,
    );

    // 1. BS x delete_count
    for _ in 0..action.delete_count {
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_BACK,
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_BACK,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }

    // 2. UNICODE で挿入文字を送信
    for ch in action.insert_text.chars() {
        // BMP 外文字はサロゲートペアになるが、KEYEVENTF_UNICODE の wScan は
        // UTF-16 code unit を渡すので 1 文字 = 1〜2 INPUT イベント。
        let mut buf = [0u16; 2];
        let units = ch.encode_utf16(&mut buf);
        for &unit in units.iter() {
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
        }
    }

    unsafe {
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != inputs.len() {
            debug_log!("SendInput 一部失敗: sent={} / total={}", sent, inputs.len());
        }
    }
}

/// キーボードフックのコールバック関数
#[no_mangle]
pub extern "system" fn LowLevelKeyboardProc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if code < 0 {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let kb = *(lparam.0 as *const KBDLLHOOKSTRUCT);
        let event = wparam.0 as u32;
        
        // デバッグ: キー入力をログ
        if event == WM_KEYDOWN || event == WM_SYSKEYDOWN {
            debug_log!("キー入力検出: vkCode={}, flags={}", kb.vkCode, kb.flags.0);
        }
        
        // 自分が送信したキーは無視
        if (kb.flags.0 & LLKHF_INJECTED.0) != 0 {
            debug_log!("自己送信キーをスキップ");
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // 有効でない場合はパススルー
        if !IS_ENABLED {
            debug_log!("変換無効: パススルー");
            return CallNextHookEx(None, code, wparam, lparam);
        }

        // Space のキーアップ: LLM二重発火ガードをリセット
        if (event == WM_KEYUP || event == WM_SYSKEYUP) && kb.vkCode == VK_SPACE.0 as u32 {
            let fired = LLM_FIRED;
            LLM_FIRED = false;
            // 我々が消費した Space の keyup はアプリに空白を送らないよう消費
            if OUR_ACTIVE && fired {
                return LRESULT(1);
            }
        }

        if event == WM_KEYDOWN || event == WM_SYSKEYDOWN {
            let vk_code = kb.vkCode;

            // 半角/全角キー: OUR_ACTIVE をトグル (MS-IME には届かせない)
            if is_ime_toggle_vk(vk_code) {
                toggle_our_active();
                return LRESULT(1); // 元のキーは MS-IME に渡さず消費
            }

            // Ctrl+Space: ライブ変換全体のトグル (緊急 OFF 用)
            if is_ctrl_pressed() && vk_code == VK_SPACE.0 as u32 {
                IS_ENABLED = !IS_ENABLED;
                debug_log!("ライブ変換トグル: {}", if IS_ENABLED { "有効" } else { "無効" });
                if !IS_ENABLED {
                    OUR_ACTIVE = false;
                    hide_candidate_window();
                    if let Some(context_mutex) = &mut LIVE_CONTEXT {
                        if let Ok(mut context) = context_mutex.try_lock() {
                            context.romaji_buffer.clear();
                            context.hiragana_buffer.clear();
                            context.conversion_result.clear();
                            context.last_sent_length = 0;
                        }
                    }
                }
                return LRESULT(1);
            }

            // 初回キーストロークで MS-IME の初期状態を読む
            if !INITIAL_CHECK_DONE {
                INITIAL_CHECK_DONE = true;
                if is_ime_hiragana_mode() {
                    OUR_ACTIVE = true;
                    close_ms_ime_for_foreground();
                    debug_log!("初期判定: MS-IME ひらがな → OUR_ACTIVE=true");
                } else {
                    OUR_ACTIVE = false;
                    debug_log!("初期判定: 非ひらがな → OUR_ACTIVE=false");
                }
            }

            // 我々が非アクティブならパススルー
            if !OUR_ACTIVE {
                return CallNextHookEx(None, code, wparam, lparam);
            }

            // 念のため: MS-IME が外部要因で再オープンされていたら毎回閉じ直す
            // (ユーザーがタスクトレイ等から触った場合の保険)
            // ※ パフォーマンス劣化を避けるため、特定キーだけにしてもよいが
            //   今はシンプルに毎回呼ぶ。
            // close_ms_ime_for_foreground();  // 必要なら有効化

            // 修飾キー組み合わせはパススルー
            if is_ctrl_pressed() || is_alt_pressed() {
                return CallNextHookEx(None, code, wparam, lparam);
            }

            if let Some(context_mutex) = &mut LIVE_CONTEXT {
                // try_lockでブロッキングを回避
                if let Ok(mut context) = context_mutex.try_lock() {
                    // 数字キー 1-9: 候補一覧の表示中は番号で直接選択して確定
                    // （選んだ = その変換が正しい、として学習にも記録される）
                    if (0x31..=0x39).contains(&vk_code)
                        && !is_shift_pressed()
                        && candidate_window_visible()
                        && !context.candidates.is_empty()
                    {
                        let index = (vk_code - 0x31) as usize;
                        if index < context.candidates.len() {
                            let mut actions = Vec::new();
                            if let Some(action) = context.select_candidate(index) {
                                actions.push(action);
                            }
                            if let Some(action) = context.commit() {
                                actions.push(action);
                            }
                            drop(context);
                            hide_candidate_window();
                            for action in actions {
                                execute_action(action);
                            }
                            return LRESULT(1);
                        }
                    }

                    // 数字キー 1-9: 予測変換の表示中（同音候補一覧は出ていない）は
                    // 番号で予測語を確定する（前方一致補完・次単語予測）。
                    if (0x31..=0x39).contains(&vk_code)
                        && !is_shift_pressed()
                        && candidate_window_visible()
                        && context.candidates.is_empty()
                        && !context.predictions.is_empty()
                    {
                        let index = (vk_code - 0x31) as usize;
                        if index < context.predictions.len() {
                            let action = context.commit_prediction(index);
                            // 確定後の次単語予測を用意（commit_prediction 内で更新済み）
                            let preds = context.prediction_display();
                            drop(context);
                            hide_candidate_window();
                            if let Some(action) = action {
                                execute_action(action);
                            }
                            show_prediction_popup(&preds);
                            return LRESULT(1);
                        }
                    }

                    // Shift+英字 = その1文字だけ英大文字を直接入力（かな変換しない）。
                    //   頭字語(API)や「PCで」のような直後のかな入力が予測どおり動く。
                    //   小文字始まりの英単語を続けたい場合は 半角/全角 で英数モードへ。
                    if (0x41..=0x5A).contains(&vk_code) && is_shift_pressed() {
                        let action = if context.is_composing() {
                            context.commit()
                        } else {
                            None
                        };
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        hide_candidate_window();
                        // 元の Shift+英字をアプリに渡して大文字を入力させる
                        return CallNextHookEx(None, code, wparam, lparam);
                    }

                    // アルファベット・句読点キー
                    if let Some(ch) = vk_to_char(vk_code, kb.scanCode, is_shift_pressed()) {
                        let mut actions = Vec::new();

                        // 候補一覧から選択中に次の入力が来たら、選択中の候補を
                        // 確定してから新しい入力を始める（選択 = 確定の解釈）
                        if candidate_window_visible() && !context.candidates.is_empty() {
                            if let Some(action) = context.commit() {
                                actions.push(action);
                            }
                        }

                        if let Some(action) = context.add_char(ch) {
                            actions.push(action);
                        }
                        // 句読点は入力後に自動確定（日本語IMEの標準動作）
                        if is_commit_char(ch) {
                            if let Some(action) = context.commit() {
                                actions.push(action);
                            }
                        }
                        // 予測変換を更新して表示（番号キーで選べる）
                        context.update_predictions();
                        let preds = context.prediction_display();
                        // ロックを解放してからアクションを実行
                        drop(context);
                        for action in actions {
                            execute_action(action);
                        }
                        show_prediction_popup(&preds);
                        // 元のキー入力を抑制
                        return LRESULT(1);
                    }

                    // バックスペース
                    if vk_code == VK_BACK.0 as u32 && context.is_composing() {
                        let action = context.backspace();
                        context.update_predictions();
                        let preds = context.prediction_display();
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        show_prediction_popup(&preds);
                        return LRESULT(1);
                    }

                    // Tab: LLM変換専用（責務の分離）
                    //   1回押すとローカルLLMによる変換をバックグラウンド発火する。
                    //   オートリピート（押しっぱなし）では二重発火しないよう
                    //   TAB_LLM_FIRED でガードし、keyup でリセットする。
                    // Space: 変換中は「確定して空白を入れる」。
                    //   Shift+Space のときだけ LLM 校正を発火する（責務分離）。
                    if vk_code == VK_SPACE.0 as u32 && context.is_composing() {
                        if is_shift_pressed() {
                            // Shift+Space → LLM校正（二重発火は LLM_FIRED でガード）
                            drop(context);
                            if !LLM_FIRED {
                                LLM_FIRED = true;
                                trigger_llm_conversion();
                            }
                            return LRESULT(1);
                        }
                        // Space → 現在の変換を確定してから空白を通す
                        let action = context.commit();
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        hide_candidate_window();
                        // 元の Space をアプリに渡して空白を入力させる
                        return CallNextHookEx(None, code, wparam, lparam);
                    }

                    // 通常変換（候補一覧）:
                    //   Tab / ↓ : 次候補（Shift+Tab は前へ）
                    //   ↑       : 前候補
                    // 変換中に押すと候補一覧を表示し、選択を移動する。
                    let is_next_key = vk_code == VK_TAB.0 as u32
                        || vk_code == VK_DOWN.0 as u32;
                    let is_prev_key = vk_code == VK_UP.0 as u32;
                    if (is_next_key || is_prev_key) && context.is_composing() {
                        let backwards = is_prev_key || is_shift_pressed();
                        let action = context.cycle_candidate(backwards);
                        // 一覧には直近（最後）の文節の候補だけを表示する
                        let items = context.cand_seg_surfaces.clone();
                        let selected = context.candidate_index;
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        show_candidate_window(&items, selected);
                        // 変換中の移動キーは候補切替として消費
                        return LRESULT(1);
                    }

                    // →: 先頭の1語を部分確定
                    // （前半が正しく後半が違うとき、正しい部分から順に確定する）
                    if vk_code == VK_RIGHT.0 as u32 && context.is_composing() {
                        let actions = context.commit_first_word();
                        drop(context);
                        hide_candidate_window();
                        for action in actions {
                            execute_action(action);
                        }
                        return LRESULT(1);
                    }

                    // Enter: 確定のみ（IME標準動作: 変換中のEnterは改行しない）
                    if vk_code == VK_RETURN.0 as u32 && context.is_composing() {
                        // 確定後は次単語予測を表示（commit 内で更新済み）
                        let action = context.commit();
                        let preds = context.prediction_display();
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        show_prediction_popup(&preds);
                        return LRESULT(1);
                    }

                    // Escape:
                    //   末尾から一文節ずつひらがなに戻す（押すたびに前へ）。
                    //   全て戻し終えていたら入力を取り消す。
                    if vk_code == VK_ESCAPE.0 as u32 && context.is_composing() {
                        let action = match context.extend_kana_revert() {
                            Some(a) => Some(a),        // 末尾の文節をかなに戻した
                            None => context.cancel(),  // 戻すものが無い → 取消
                        };
                        drop(context);
                        if let Some(action) = action {
                            execute_action(action);
                        }
                        hide_candidate_window();
                        return LRESULT(1);
                    }
                }
                // ロックが取れない場合はパススルー
            }
        }

        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// フックをインストール
#[no_mangle]
pub extern "C" fn install_hook() -> bool {
    unsafe {
        // コンテキストを初期化
        let mut state = LiveConversionState::new();
        // 学習DBをオープン（CLIと共有。失敗しても変換は継続できる）
        match LearningRepository::open("ime-learning.db") {
            Ok(learning) => {
                state.learning = Some(learning);
                println!("学習DBをオープン: ime-learning.db");
            }
            Err(e) => {
                eprintln!("学習DBのオープンに失敗（学習なしで継続）: {}", e);
            }
        }
        LIVE_CONTEXT = Some(Mutex::new(state));
        IS_ENABLED = true;
        OUR_ACTIVE = false;

        // 入力欄の位置を追う UI Automation ポーラーを開始（ポップアップ位置用）
        start_uia_poller();

        // LLM（Ollama）の接続状態をバックグラウンドで確認して表示
        // （Tab長押し変換が使えるかの目安。未接続でも通常変換は動く）
        std::thread::spawn(|| {
            let cfg = common::LlmConfig::from_env();
            if common::llm::ollama_available(&cfg) {
                println!("LLM: 利用可能 (model={}) — モデルを準備中…", cfg.model);
                // モデルをメモリに載せておく（初回のTab長押し変換を高速化）
                if common::warm_up(&cfg) {
                    println!("LLM: 準備完了 — Tab長押しでLLM変換できます");
                } else {
                    println!("LLM: ウォームアップに失敗（初回変換は時間がかかる場合があります）");
                }
            } else {
                println!(
                    "LLM: 未接続 — Tab長押し変換は無効です（`docker compose up -d` で起動できます）"
                );
            }
        });
        INITIAL_CHECK_DONE = false;
        
        // DLLのHINSTANCEを取得
        let mut hmodule: HMODULE = HMODULE::default();
        let proc_addr = install_hook as *const ();
        let result = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(proc_addr as *const u16),
            &mut hmodule,
        );
        
        let hinstance = if result.is_ok() {
            HINSTANCE(hmodule.0)
        } else {
            println!("Warning: Could not get DLL HINSTANCE, using default");
            HINSTANCE::default()
        };
        
        println!("Installing hook with HINSTANCE: {:?}", hinstance);
        
        let hook = SetWindowsHookExW(
            WINDOWS_HOOK_ID(13), // WH_KEYBOARD_LL (14はWH_MOUSE_LL)
            Some(LowLevelKeyboardProc),
            hinstance,
            0,
        );

        match hook {
            Ok(h) => {
                HOOK_HANDLE = Some(h);
                println!("Keyboard hook installed successfully");
                true
            }
            Err(e) => {
                eprintln!("Failed to install hook: {:?}", e);
                false
            }
        }
    }
}

/// フックをアンインストール
#[no_mangle]
pub extern "C" fn uninstall_hook() -> bool {
    unsafe {
        IS_ENABLED = false;
        OUR_ACTIVE = false;
        INITIAL_CHECK_DONE = false;
        hide_candidate_window();
        if let Some(hwnd) = CANDIDATE_HWND.take() {
            use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
            let _ = DestroyWindow(hwnd);
        }

        if let Some(hook) = HOOK_HANDLE {
            let result = UnhookWindowsHookEx(hook);
            HOOK_HANDLE = None;
            LIVE_CONTEXT = None;
            println!("Keyboard hook uninstalled");
            result.is_ok()
        } else {
            false
        }
    }
}

/// 辞書をロード
#[no_mangle]
pub extern "C" fn load_dictionary(path_ptr: *const u8, path_len: usize) -> bool {
    unsafe {
        debug_log!("辞書ロード開始: ptr={:?}, len={}", path_ptr, path_len);
        
        if path_ptr.is_null() || path_len == 0 {
            debug_log!("辞書ロード失敗: パスが無効");
            return false;
        }

        let path_bytes = std::slice::from_raw_parts(path_ptr, path_len);
        let path_str = match std::str::from_utf8(path_bytes) {
            Ok(s) => s,
            Err(e) => {
                debug_log!("辞書ロード失敗: UTF-8エラー: {:?}", e);
                return false;
            }
        };
        
        debug_log!("辞書パス: {}", path_str);

        if let Some(context_mutex) = &mut LIVE_CONTEXT {
            if let Ok(mut context) = context_mutex.lock() {
                let result = context.load_dictionary(Path::new(path_str));
                debug_log!("辞書ロード結果: {}", result);
                return result;
            } else {
                debug_log!("辞書ロード失敗: コンテキストロック失敗");
            }
        } else {
            debug_log!("辞書ロード失敗: コンテキストなし");
        }

        false
    }
}

/// 変換を有効/無効にする
#[no_mangle]
pub extern "C" fn set_enabled(enabled: bool) {
    unsafe {
        IS_ENABLED = enabled;
        if let Some(context_mutex) = &mut LIVE_CONTEXT {
            if let Ok(mut context) = context_mutex.lock() {
                context.enabled = enabled;
            }
        }
    }
}

/// 変換が有効かどうかを取得
#[no_mangle]
pub extern "C" fn is_enabled() -> bool {
    unsafe { IS_ENABLED }
}