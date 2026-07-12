//! コマンドモード: ターミナルでのコマンド学習・前方一致予測・エイリアス実行

use crate::*;

/// コマンドモードで現在ターミナルに打鍵中のコマンド行（前方一致補完に使う）。
/// ターミナルはエコーを自前で行うため、我々は打鍵を「観測」して溜めるだけ。
pub(crate) static COMMAND_LINE: Mutex<String> = Mutex::new(String::new());
/// 一覧で現在選択（ハイライト）中のインデックス。Tab/Shift+Tab で上下、Enter で決定。
/// 先頭(0)が既定の選択。↑↓はシェル履歴のため触らない。
pub(crate) static mut COMMAND_SEL: usize = 0;

// ============ コマンドモード ============

/// 母音(a,i,u,e,o)の長さ k の全組合せ（5^k 通り）を返す。ローマ字補正の母音補完/置換用。
pub(crate) fn vowel_combos(k: usize) -> Vec<Vec<char>> {
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

/// dictionaries/commands.csv から定番コマンドと説明を読み込み、DBへseedする。
/// 形式: `コマンド<TAB>説明`（TAB区切り、説明は省略可、# 行はコメント）。
/// 実行ディレクトリ直下と exe 隣接の両方を探す。
pub(crate) fn seed_commands_from_csv(learning: &LearningRepository) {
    let candidates = [
        std::path::PathBuf::from("dictionaries/commands.csv"),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("dictionaries/commands.csv")))
            .unwrap_or_default(),
    ];
    for path in candidates.iter() {
        if path.as_os_str().is_empty() || !path.exists() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let mut n = 0usize;
        for line in text.lines() {
            let line = line.trim_end_matches(['\r', '\n']);
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let (cmd, desc) = match line.split_once('\t') {
                Some((c, d)) => (c, d),
                None => (line, ""),
            };
            if learning.seed_command(cmd, desc).is_ok() {
                n += 1;
            }
        }
        println!("コマンド辞書を読込: {} ({} 件)", path.display(), n);
        return; // 最初に見つかった1つだけ読む
    }
}

/// フォアグラウンドの窓クラス名を返す。
pub(crate) fn foreground_class_name() -> String {
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return String::new();
        }
        let mut cls = [0u16; 128];
        let n = GetClassNameW(hwnd, &mut cls);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&cls[..n as usize])
    }
}

/// 窓クラス名が「純粋なターミナル窓」か。
pub(crate) fn is_terminal_class(class: &str) -> bool {
    matches!(
        class,
        "CASCADIA_HOSTING_WINDOW_CLASS"  // Windows Terminal
            | "ConsoleWindowClass"        // conhost / cmd / 旧PowerShell
            | "PseudoConsoleWindow"
    ) || class.starts_with("mintty") // Git Bash 等
}

/// フォアグラウンドがターミナル系か（コマンドモードの対象判定）。
/// 純粋な端末窓は窓クラスで即判定。VSCode等の統合ターミナルは窓クラスが
/// アプリ本体と同じ(Chrome_WidgetWin_1)なので、UIAポーラーがフォーカス要素の
/// クラス名(xterm等)で判定した FOCUSED_IS_TERMINAL フラグで見る。
pub(crate) fn is_terminal_focused() -> bool {
    if is_terminal_class(&foreground_class_name()) {
        return true;
    }
    FOCUSED_IS_TERMINAL.load(std::sync::atomic::Ordering::Relaxed)
}

/// コマンド文字列を追跡するための VK→ASCII 変換（US配列想定）。
/// 前方一致に使うだけなので、英数字・空白・よく使う記号のみ拾う。
/// （JIS配列だと一部記号がずれるが、英字・数字・空白・. / - は共通）
pub(crate) fn vk_to_ascii(vk: u32, shift: bool) -> Option<char> {
    if (0x41..=0x5A).contains(&vk) {
        let base = (vk as u8 - 0x41) + b'a';
        return Some(if shift { (base - 32) as char } else { base as char });
    }
    if (0x30..=0x39).contains(&vk) {
        if !shift {
            return Some((b'0' + (vk as u8 - 0x30)) as char);
        }
        return match vk {
            0x31 => Some('!'), 0x32 => Some('@'), 0x33 => Some('#'), 0x34 => Some('$'),
            0x35 => Some('%'), 0x36 => Some('^'), 0x37 => Some('&'), 0x38 => Some('*'),
            0x39 => Some('('), 0x30 => Some(')'),
            _ => None,
        };
    }
    if (0x60..=0x69).contains(&vk) {
        return Some((b'0' + (vk as u8 - 0x60)) as char); // テンキー 0-9
    }
    if vk == VK_SPACE.0 as u32 {
        return Some(' ');
    }
    let ch = match vk {
        0xBC => if shift { '<' } else { ',' },
        0xBE => if shift { '>' } else { '.' },
        0xBD => if shift { '_' } else { '-' },
        0xBB => if shift { '+' } else { '=' },
        0xBF => if shift { '?' } else { '/' },
        0xC0 => if shift { '~' } else { '`' },
        0xDB => if shift { '{' } else { '[' },
        0xDD => if shift { '}' } else { ']' },
        0xDC => if shift { '|' } else { '\\' },
        0xBA => if shift { ':' } else { ';' },
        0xDE => if shift { '"' } else { '\'' },
        0x6A => '*', 0x6B => '+', 0x6D => '-', 0x6F => '/', 0x6E => '.',
        _ => return None,
    };
    Some(ch)
}

/// カーソル移動・行編集キーか（押されるとコマンド行を正確に追えなくなる）。
pub(crate) fn is_line_edit_vk(vk: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_DELETE, VK_END, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_PRIOR,
    };
    vk == VK_LEFT.0 as u32
        || vk == VK_RIGHT.0 as u32
        || vk == VK_UP.0 as u32
        || vk == VK_DOWN.0 as u32
        || vk == VK_HOME.0 as u32
        || vk == VK_END.0 as u32
        || vk == VK_DELETE.0 as u32
        || vk == VK_PRIOR.0 as u32
        || vk == VK_NEXT.0 as u32
        || vk == VK_INSERT.0 as u32
}

/// コマンドモードの候補: (display 表示, target 挿入文字列, description, is_alias)。
/// エイリアス（is_alias=true。選ぶと expansion を挿入して**実行**）を優先し、
/// 続いてコマンド履歴（is_alias=false。挿入のみ）を並べる。
/// 候補: (display, target 挿入文字列, description, is_alias, auto_run 即実行か)。
pub(crate) fn command_predictions(prefix: &str, limit: usize) -> Vec<(String, String, String, bool, bool)> {
    if prefix.trim().is_empty() {
        return Vec::new();
    }
    if let Some(context_mutex) = LIVE_CONTEXT.get() {
        if let Ok(context) = context_mutex.try_lock() {
            if let Some(learning) = context.learning.as_ref() {
                let mut out: Vec<(String, String, String, bool, bool)> = Vec::new();
                // エイリアス優先（is_alias=true, auto_run はDBの値）
                if let Ok(aliases) = learning.predict_alias(prefix, limit) {
                    for (a, exp, desc, auto_run) in aliases {
                        let tag = if auto_run { "" } else { "  ✎編集" };
                        let shown = if desc.is_empty() {
                            format!("\u{2192} {}{}", exp, tag)
                        } else {
                            format!("\u{2192} {}   ({}){}", exp, desc, tag)
                        };
                        out.push((a, exp, shown, true, auto_run));
                    }
                }
                // コマンド履歴（is_alias=false, 即実行=true）
                if let Ok(cmds) = learning.predict_command(prefix, limit) {
                    for (c, desc, _freq) in cmds {
                        if !out.iter().any(|(d, _, _, _, _)| d == &c) {
                            out.push((c.clone(), c, desc, false, true));
                        }
                    }
                }
                out.truncate(limit);
                return out;
            }
        }
    }
    Vec::new()
}

/// 実行された1行を学習（コマンド履歴に記録）する。
pub(crate) fn record_command_line(line: &str) {
    if let Some(context_mutex) = LIVE_CONTEXT.get() {
        if let Ok(context) = context_mutex.try_lock() {
            if let Some(learning) = context.learning.as_ref() {
                let _ = learning.record_command(line);
            }
        }
    }
}

/// 現在のコマンド行に応じてコマンド候補ポップアップを更新する。
/// メニュー方式: 打鍵で内容が変わったら選択を先頭(0)に戻す。先頭が既に選択状態。
pub(crate) fn update_command_suggestions() {
    unsafe {
        COMMAND_SEL = 0;
    }
    let buf = COMMAND_LINE.lock().map(|b| b.clone()).unwrap_or_default();
    let flashing = CANDIDATE_UI.lock().map(|ui| ui.status).unwrap_or(false);
    if buf.trim().is_empty() {
        if !flashing {
            hide_candidate_window();
        }
        return;
    }
    let preds = command_predictions(&buf, 8);
    if preds.is_empty() {
        if !flashing {
            hide_candidate_window();
        }
        return;
    }
    let items: Vec<String> = preds.iter().map(|(disp, _, _, _, _)| disp.clone()).collect();
    let descs: Vec<String> = preds.iter().map(|(_, _, d, _, _)| d.clone()).collect();
    let runs: Vec<bool> = preds.iter().map(|(_, _, _, _, r)| *r).collect();
    show_command_popup(&items, &descs, &runs, 0); // 先頭を選択状態で表示
}

/// Tab/Shift+Tab: 一覧の**選択（ハイライト）だけ**を上下に動かす（挿入はしない）。
/// ↑↓はシェル履歴のため触らない。決定・実行は Enter。処理したら true。
pub(crate) unsafe fn command_move(forward: bool) -> bool {
    let buf = COMMAND_LINE.lock().map(|x| x.clone()).unwrap_or_default();
    if buf.trim().is_empty() {
        return false;
    }
    let preds = command_predictions(&buf, 8);
    if preds.is_empty() {
        return false;
    }
    let n = preds.len();
    if forward {
        COMMAND_SEL = (COMMAND_SEL + 1) % n;
    } else {
        COMMAND_SEL = (COMMAND_SEL + n - 1) % n;
    }
    let items: Vec<String> = preds.iter().map(|(d, _, _, _, _)| d.clone()).collect();
    let descs: Vec<String> = preds.iter().map(|(_, _, d, _, _)| d.clone()).collect();
    let runs: Vec<bool> = preds.iter().map(|(_, _, _, _, r)| *r).collect();
    show_command_popup(&items, &descs, &runs, COMMAND_SEL);
    true
}

/// Enter: 選択中の候補を挿入して実行（auto_run に従う）。候補が無ければ打った行を実行。
/// 戻り値: Some(LRESULT) 消費 / None パススルー（そのままシェルで実行）。
pub(crate) unsafe fn command_commit() -> Option<LRESULT> {
    let buf = COMMAND_LINE.lock().map(|x| x.clone()).unwrap_or_default();
    let preds = command_predictions(&buf, 24);
    if buf.trim().is_empty() || preds.is_empty() || COMMAND_SEL >= preds.len() {
        // 候補なし: 打った行をそのままシェルへ渡して実行
        let line = COMMAND_LINE
            .lock()
            .map(|mut b| std::mem::take(&mut *b))
            .unwrap_or_default();
        record_command_line(&line);
        hide_candidate_window();
        return None;
    }
    let (_disp, target, _desc, _is_alias, auto_run) = preds[COMMAND_SEL].clone();
    // 打った内容を選択候補で置き換える
    execute_action(ConversionAction {
        delete_count: buf.chars().count(),
        insert_text: target.clone(),
    });
    if auto_run {
        // 挿入＋実行（Enter を送る。元の Enter は消費）
        send_vk(VK_RETURN);
        record_command_line(&target);
        if let Ok(mut b) = COMMAND_LINE.lock() {
            b.clear();
        }
    } else {
        // 挿入のみ（編集してから実行）。行は保持。
        if let Ok(mut b) = COMMAND_LINE.lock() {
            *b = target;
        }
    }
    hide_candidate_window();
    Some(LRESULT(1))
}

/// Delete: 一覧で選択中の「コマンド履歴」を削除する（学習DBから消す）。
/// 履歴コマンド（is_alias=false）のみ対象。エイリアスは設定ウィンドウで管理。
/// 削除したら true（＝キーを消費）。対象が無ければ false（従来の行編集扱いへ）。
pub(crate) unsafe fn command_delete_selected() -> bool {
    let buf = COMMAND_LINE.lock().map(|x| x.clone()).unwrap_or_default();
    if buf.trim().is_empty() {
        return false;
    }
    let preds = command_predictions(&buf, 8);
    if preds.is_empty() || COMMAND_SEL >= preds.len() {
        return false;
    }
    let (_disp, target, _desc, is_alias, _auto) = preds[COMMAND_SEL].clone();
    if is_alias {
        // エイリアスは誤操作で消さない（設定ウィンドウで管理する）
        return false;
    }
    // 学習DB（コマンド履歴）から削除
    let mut removed = false;
    if let Some(ctx) = LIVE_CONTEXT.get() {
        if let Ok(c) = ctx.try_lock() {
            if let Some(learning) = c.learning.as_ref() {
                removed = learning.delete_command(&target).unwrap_or(false);
            }
        }
    }
    if !removed {
        return false;
    }
    debug_log!("コマンド履歴を削除: '{}'", target);
    // 削除後の候補で一覧を出し直す（無くなれば閉じる）。選択は先頭へ。
    update_command_suggestions();
    true
}

/// コマンドモードのキー処理。Some(_) を返したらキーを消費、None ならターミナルへ渡す。
pub(crate) unsafe fn handle_command_mode(vk_code: u32, shift: bool) -> Option<LRESULT> {
    // Enter: 選択中の候補を挿入＆実行（auto_run に従う）。候補が無ければ打った行を実行。
    if vk_code == VK_RETURN.0 as u32 {
        return command_commit();
    }
    // Delete: 一覧で選択中のコマンド履歴を削除する（履歴のみ。消せたらキーを消費）。
    // 消せない（候補なし/エイリアス選択）ときは後段の行編集扱いへ落とす。
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_DELETE;
        if vk_code == VK_DELETE.0 as u32 && command_delete_selected() {
            return Some(LRESULT(1));
        }
    }
    // Tab/Shift+Tab: 一覧の選択（ハイライト）を動かすだけ（挿入・実行はしない）。
    //   ↑↓はシェル履歴のため触らない。候補が無ければシェルの補完へ渡す。
    if vk_code == VK_TAB.0 as u32 {
        if command_move(!shift) {
            return Some(LRESULT(1));
        }
        return None;
    }
    // Backspace: バッファ末尾を削除（キーはターミナルへ渡す）
    if vk_code == VK_BACK.0 as u32 {
        if let Ok(mut b) = COMMAND_LINE.lock() {
            b.pop();
        }
        update_command_suggestions();
        return None;
    }
    // Esc: 行取消 → バッファを捨ててポップアップを閉じる（キーは渡す）
    if vk_code == VK_ESCAPE.0 as u32 {
        if let Ok(mut b) = COMMAND_LINE.lock() {
            b.clear();
        }
        hide_candidate_window();
        return None;
    }
    // カーソル移動・編集キー: 行内容を正確に追えなくなるので追跡を中断
    if is_line_edit_vk(vk_code) {
        if let Ok(mut b) = COMMAND_LINE.lock() {
            b.clear();
        }
        hide_candidate_window();
        return None;
    }
    // 文字キー: バッファに追記して候補更新（キーはターミナルへ渡してエコー）
    if let Some(ch) = vk_to_ascii(vk_code, shift) {
        if let Ok(mut b) = COMMAND_LINE.lock() {
            b.push(ch);
        }
        update_command_suggestions();
        return None;
    }
    None
}
