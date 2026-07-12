//! コマンド/エイリアス設定ウィンドウ（一覧・編集・検索・並び替え・即実行チェック）

use crate::*;

/// エイリアス設定ウィンドウのハンドル（なければ未生成）
pub(crate) static mut SETTINGS_HWND: Option<HWND> = None;
/// 設定ウィンドウが表示中か。表示中はコマンド予測ポップアップの最前面固定を
/// 止めて、設定ウィンドウを前面に保つ。
pub(crate) static mut SETTINGS_OPEN: bool = false;

// ============ エイリアス設定ウィンドウ ============

// 子コントロールのID
pub(crate) const ID_EDIT_ALIAS: i32 = 101;
pub(crate) const ID_EDIT_EXPANSION: i32 = 102;
pub(crate) const ID_EDIT_DESC: i32 = 103;
pub(crate) const ID_BTN_BROWSE: i32 = 104;
pub(crate) const ID_BTN_ADD: i32 = 105;
pub(crate) const ID_LIST: i32 = 106;
pub(crate) const ID_BTN_DELETE: i32 = 107;
pub(crate) const ID_BTN_CLOSE: i32 = 108;
pub(crate) const ID_EDIT_SEARCH: i32 = 109;
pub(crate) const ID_BTN_SORT: i32 = 110;
pub(crate) const ID_BTN_NEW: i32 = 111;
pub(crate) const ID_COMBO_FILTER: i32 = 112;
pub(crate) const ID_DETAIL: i32 = 113; // ホバー/選択した行の全文を出す詳細欄
pub(crate) const ID_CHK_AUTORUN: i32 = 114; // Enterで即実行するか（チェックボックス）

// 一覧の1行分（エイリアス or コマンド）。選択→フォーム反映・削除で使う。
#[derive(Clone)]
pub(crate) struct SettingsEntry {
    /// エイリアス名、またはコマンド本体（削除時のキー）
    pub(crate) name: String,
    /// 挿入/実行される内容（エイリアスは expansion、コマンドは同じ）
    pub(crate) target: String,
    pub(crate) description: String,
    pub(crate) is_alias: bool,
    pub(crate) frequency: u32,
    /// Enter で即実行するか（エイリアスのみ意味を持つ。コマンドは true）
    pub(crate) auto_run: bool,
}

/// 一覧に今表示している行（表示順）。リスト選択の index からこれで引く。
pub(crate) static SETTINGS_ENTRIES: Mutex<Vec<SettingsEntry>> = Mutex::new(Vec::new());
/// 直前に選択していた行（更新時に名前変更なら旧キーを消すため）。
pub(crate) static SETTINGS_SELECTED: Mutex<Option<(bool, String)>> = Mutex::new(None);
/// 並び順のソート列: -1=作成順、-2=使用頻度順、0=エイリアス/1=コマンド/2=説明の名前順。
/// 列ヘッダーのクリックで名前順、並び替えボタンで作成順⇔頻度順を切替。
pub(crate) static mut SETTINGS_SORT_COL: i32 = -1;
/// 名前順ソートの方向（同じ列を再クリックで反転）
pub(crate) static mut SETTINGS_SORT_DESC: bool = false;
/// 並び替えボタンの状態: false=作成順、true=使用頻度順
pub(crate) static mut SETTINGS_BTN_FREQ: bool = false;
/// 表示フィルタ: 0=すべて / 1=エイリアスのみ / 2=コマンドのみ
pub(crate) static mut SETTINGS_FILTER: i32 = 0;

pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- ListView (SysListView32, レポート表示) ヘルパ ----
pub(crate) const LVM_FIRST: u32 = 0x1000;
pub(crate) const LVM_DELETEALLITEMS: u32 = LVM_FIRST + 9;
pub(crate) const LVM_INSERTCOLUMNW: u32 = LVM_FIRST + 97;
pub(crate) const LVM_INSERTITEMW: u32 = LVM_FIRST + 77;
pub(crate) const LVM_SETITEMTEXTW: u32 = LVM_FIRST + 116;
pub(crate) const LVM_GETNEXTITEM: u32 = LVM_FIRST + 12;
pub(crate) const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = LVM_FIRST + 54;
pub(crate) const LVM_SETITEMSTATE: u32 = LVM_FIRST + 43;
pub(crate) const LVN_ITEMCHANGED: u32 = 0xFFFF_FF9B; // LVN_FIRST(-100) - 1
pub(crate) const LVN_COLUMNCLICK: u32 = 0xFFFF_FF94; // LVN_FIRST(-100) - 8
pub(crate) const LVN_GETINFOTIP: u32 = 0xFFFF_FF62; // LVN_FIRST(-100) - 58
pub(crate) const LVS_EX_INFOTIP: isize = 0x0000_0400; // ホバーでカーソル付近にツールチップ
pub(crate) const LVIS_SELECTED: u32 = 0x0002;
pub(crate) const LVNI_SELECTED: isize = 0x0002;
pub(crate) const LVS_EX_FULLROWSELECT: isize = 0x0020;
pub(crate) const LVS_EX_GRIDLINES: isize = 0x0001;

pub(crate) unsafe fn lv_insert_column(list: HWND, idx: i32, text: &str, width: i32) {
    use windows::Win32::UI::Controls::{LVCOLUMNW, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH};
    let mut t = to_wide(text);
    let col = LVCOLUMNW {
        mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
        cx: width,
        pszText: windows::core::PWSTR(t.as_mut_ptr()),
        iSubItem: idx,
        ..Default::default()
    };
    SendMessageW(list, LVM_INSERTCOLUMNW, WPARAM(idx as usize), LPARAM(&col as *const _ as isize));
}

pub(crate) unsafe fn lv_insert_row(list: HWND, idx: i32, text: &str) {
    use windows::Win32::UI::Controls::{LVITEMW, LVIF_TEXT};
    let mut t = to_wide(text);
    let it = LVITEMW {
        mask: LVIF_TEXT,
        iItem: idx,
        iSubItem: 0,
        pszText: windows::core::PWSTR(t.as_mut_ptr()),
        ..Default::default()
    };
    SendMessageW(list, LVM_INSERTITEMW, WPARAM(0), LPARAM(&it as *const _ as isize));
}

pub(crate) unsafe fn lv_set_sub(list: HWND, row: i32, sub: i32, text: &str) {
    use windows::Win32::UI::Controls::{LVITEMW, LVIF_TEXT};
    let mut t = to_wide(text);
    let it = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row,
        iSubItem: sub,
        pszText: windows::core::PWSTR(t.as_mut_ptr()),
        ..Default::default()
    };
    SendMessageW(list, LVM_SETITEMTEXTW, WPARAM(row as usize), LPARAM(&it as *const _ as isize));
}

/// 選択中の行インデックス（なければ None）
pub(crate) unsafe fn lv_selected(list: HWND) -> Option<usize> {
    let r = SendMessageW(
        list,
        LVM_GETNEXTITEM,
        WPARAM((-1i32) as isize as usize),
        LPARAM(LVNI_SELECTED),
    )
    .0;
    if r < 0 {
        None
    } else {
        Some(r as usize)
    }
}

/// 列の見出しテキストを更新する（ソート矢印の付与に使う）。
pub(crate) unsafe fn lv_set_column_text(list: HWND, col: i32, text: &str) {
    use windows::Win32::UI::Controls::{LVCOLUMNW, LVCF_TEXT};
    let mut t = to_wide(text);
    let c = LVCOLUMNW {
        mask: LVCF_TEXT,
        pszText: windows::core::PWSTR(t.as_mut_ptr()),
        ..Default::default()
    };
    // LVM_SETCOLUMNW = LVM_FIRST + 96
    SendMessageW(list, LVM_FIRST + 96, WPARAM(col as usize), LPARAM(&c as *const _ as isize));
}

/// 列幅を設定する。
pub(crate) unsafe fn lv_set_column_width(list: HWND, col: i32, width: i32) {
    SendMessageW(list, LVM_FIRST + 30, WPARAM(col as usize), LPARAM(width as isize)); // LVM_SETCOLUMNWIDTH
}

/// 列幅を取得する。
pub(crate) unsafe fn lv_get_column_width(list: HWND, col: i32) -> i32 {
    SendMessageW(list, LVM_FIRST + 29, WPARAM(col as usize), LPARAM(0)).0 as i32 // LVM_GETCOLUMNWIDTH
}

/// 説明列を一覧の残り幅いっぱいまで伸ばして、右側の空きをなくす
/// （最終列の「Enter」は固定幅のまま）。
pub(crate) unsafe fn lv_stretch_desc_column(list: HWND) {
    let mut rc = RECT::default();
    let _ = GetClientRect(list, &mut rc);
    let w0 = lv_get_column_width(list, 0);
    let w1 = lv_get_column_width(list, 1);
    let w3 = lv_get_column_width(list, 3);
    let w2 = (rc.right - w0 - w1 - w3).max(80);
    lv_set_column_width(list, 2, w2);
}

/// 指定行を選択状態にする（LVN_ITEMCHANGED が飛び、フォーム/チェックボックスへ同期される）
pub(crate) unsafe fn lv_select_row(list: HWND, row: usize) {
    use windows::Win32::UI::Controls::LVITEMW;
    const LVIS_FOCUSED: u32 = 0x0001;
    let it = LVITEMW {
        state: windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED | LVIS_FOCUSED),
        stateMask: windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED | LVIS_FOCUSED),
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        WPARAM(row),
        LPARAM(&it as *const _ as isize),
    );
    // 選択行が見える位置までスクロール（LVM_ENSUREVISIBLE = LVM_FIRST + 19）
    SendMessageW(list, LVM_FIRST + 19, WPARAM(row), LPARAM(0));
}

/// 全行の選択を解除する
pub(crate) unsafe fn lv_clear_selection(list: HWND) {
    use windows::Win32::UI::Controls::LVITEMW;
    // state=0, stateMask=LVIS_SELECTED を全行(-1)に適用
    let it = LVITEMW {
        stateMask: windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED),
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        WPARAM((-1i32) as isize as usize),
        LPARAM(&it as *const _ as isize),
    );
}

pub(crate) unsafe fn settings_get_text(parent: HWND, id: i32) -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetDlgItem, GetWindowTextW};
    let Ok(ctrl) = GetDlgItem(parent, id) else { return String::new() };
    let mut buf = [0u16; 1024];
    let n = GetWindowTextW(ctrl, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub(crate) unsafe fn settings_set_text(parent: HWND, id: i32, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{GetDlgItem, SetWindowTextW};
    if let Ok(ctrl) = GetDlgItem(parent, id) {
        let w = to_wide(text);
        let _ = SetWindowTextW(ctrl, windows::core::PCWSTR(w.as_ptr()));
    }
}

/// チェックボックスの状態を取得（BM_GETCHECK=0x00F0, BST_CHECKED=1）。
pub(crate) unsafe fn settings_get_check(parent: HWND, id: i32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    if let Ok(c) = GetDlgItem(parent, id) {
        SendMessageW(c, 0x00F0, WPARAM(0), LPARAM(0)).0 == 1
    } else {
        true
    }
}

/// チェックボックスの状態を設定（BM_SETCHECK=0x00F1）。
pub(crate) unsafe fn settings_set_check(parent: HWND, id: i32, checked: bool) {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    if let Ok(c) = GetDlgItem(parent, id) {
        SendMessageW(c, 0x00F1, WPARAM(if checked { 1 } else { 0 }), LPARAM(0));
    }
}

/// エイリアス＋コマンドを検索・並び替えして一覧に読み直し、SETTINGS_ENTRIES に保存。
pub(crate) unsafe fn settings_refresh_list(parent: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{GetDlgItem, SendMessageW};
    let Ok(list) = GetDlgItem(parent, ID_LIST) else { return };

    // DBからエイリアスとコマンドを読み込む
    let (aliases, commands) = {
        if let Some(context_mutex) = LIVE_CONTEXT.get() {
            if let Ok(context) = context_mutex.lock() {
                let l = context.learning.as_ref();
                (
                    l.and_then(|l| l.all_aliases().ok()).unwrap_or_default(),
                    l.and_then(|l| l.all_commands().ok()).unwrap_or_default(),
                )
            } else {
                (Vec::new(), Vec::new())
            }
        } else {
            (Vec::new(), Vec::new())
        }
    };

    let mut entries: Vec<SettingsEntry> = Vec::new();
    for (alias, expansion, desc, _is_script, auto_run) in aliases {
        entries.push(SettingsEntry {
            name: alias,
            target: expansion,
            description: desc,
            is_alias: true,
            frequency: u32::MAX, // エイリアスは頻度順でも上に来るように
            auto_run,
        });
    }
    for (command, desc, freq) in commands {
        entries.push(SettingsEntry {
            name: command.clone(),
            target: command,
            description: desc,
            is_alias: false,
            frequency: freq,
            auto_run: true,
        });
    }

    // 表示フィルタ: 1=エイリアスのみ / 2=コマンドのみ / 0=すべて
    match SETTINGS_FILTER {
        1 => entries.retain(|e| e.is_alias),
        2 => entries.retain(|e| !e.is_alias),
        _ => {}
    }

    // 検索フィルタ（名前・内容・説明のいずれかに含まれる。大文字小文字無視）
    let query = settings_get_text(parent, ID_EDIT_SEARCH).trim().to_lowercase();
    if !query.is_empty() {
        entries.retain(|e| {
            e.name.to_lowercase().contains(&query)
                || e.target.to_lowercase().contains(&query)
                || e.description.to_lowercase().contains(&query)
        });
    }

    // 並び替え:
    //  -1 = 作成順（DBのrowid順のまま。既定）
    //  -2 = 使用頻度順（頻度降順。エイリアスは freq=MAX で上）
    //  0/1/2 = その列で名前順（列ヘッダークリック。再クリックで降順）
    let col = SETTINGS_SORT_COL;
    if col == -2 {
        entries.sort_by(|a, b| {
            b.frequency
                .cmp(&a.frequency)
                .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    } else if col >= 0 {
        entries.sort_by(|a, b| {
            let ka = match col {
                0 => if a.is_alias { a.name.to_lowercase() } else { String::new() },
                1 => a.target.to_lowercase(),
                3 => autorun_label(a.auto_run).to_string(),
                _ => a.description.to_lowercase(),
            };
            let kb = match col {
                0 => if b.is_alias { b.name.to_lowercase() } else { String::new() },
                1 => b.target.to_lowercase(),
                3 => autorun_label(b.auto_run).to_string(),
                _ => b.description.to_lowercase(),
            };
            ka.cmp(&kb)
        });
        if SETTINGS_SORT_DESC {
            entries.reverse();
        }
    }

    // 見出しにソート矢印を付けて、どの列で並んでいるか分かるようにする
    let arrow = |c: i32| -> &'static str {
        if SETTINGS_SORT_COL == c {
            if SETTINGS_SORT_DESC { " \u{25BC}" } else { " \u{25B2}" }
        } else {
            ""
        }
    };
    lv_set_column_text(list, 0, &format!("エイリアス{}", arrow(0)));
    lv_set_column_text(list, 1, &format!("コマンド{}", arrow(1)));
    lv_set_column_text(list, 2, &format!("説明{}", arrow(2)));
    lv_set_column_text(list, 3, &format!("Enter{}", arrow(3)));

    // ListView 再描画（エイリアス / コマンド / 説明 / Enter の4列）。
    // エイリアス列はエイリアス名（コマンド行は空欄）、コマンド列は実体。
    // Enter列は「Enterで即実行」チェックの状態（即実行 / ✎挿入のみ）。
    SendMessageW(list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));
    for (i, e) in entries.iter().enumerate() {
        let alias_col = if e.is_alias { e.name.as_str() } else { "" };
        lv_insert_row(list, i as i32, alias_col);
        lv_set_sub(list, i as i32, 1, &e.target);
        lv_set_sub(list, i as i32, 2, &e.description);
        lv_set_sub(list, i as i32, 3, autorun_label(e.auto_run));
    }
    if let Ok(mut store) = SETTINGS_ENTRIES.lock() {
        *store = entries;
    }
    // 説明列を残り幅まで伸ばして、右側の余計な空きをなくす
    lv_stretch_desc_column(list);

    // 再読込（検索・並び替え・登録後など）で選択が消えると、チェックボックス等の
    // フォームだけが前の項目の状態のまま残って食い違うため、編集中だった項目が
    // 一覧に残っていれば選択し直して同期を保つ。
    // （lv_select_row → LVN_ITEMCHANGED → settings_on_select がフォームへ反映する）
    let sel_key = SETTINGS_SELECTED.lock().ok().and_then(|s| s.clone());
    if let Some((is_alias, key)) = sel_key {
        let idx = SETTINGS_ENTRIES
            .lock()
            .ok()
            .and_then(|v| v.iter().position(|e| e.is_alias == is_alias && e.name == key));
        if let Some(i) = idx {
            lv_select_row(list, i);
        }
    }
}

/// 一覧の Enter 列に出す「Enterで即実行」チェック状態の表示文字列。
pub(crate) fn autorun_label(auto_run: bool) -> &'static str {
    if auto_run {
        "即実行"
    } else {
        "✎挿入のみ"
    }
}

/// リストの選択インデックスを返す（ListView の選択行）。
pub(crate) unsafe fn settings_list_selection(parent: HWND) -> Option<usize> {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    let list = GetDlgItem(parent, ID_LIST).ok()?;
    lv_selected(list)
}

/// 指定行の全文（エイリアス/コマンド/説明）を下部の詳細欄に表示する。
/// フローティングのツールチップだと最前面設定の裏に隠れるため、モーダル内に出す。
pub(crate) unsafe fn settings_show_detail(parent: HWND, row: usize) {
    let text = SETTINGS_ENTRIES.lock().ok().and_then(|v| {
        v.get(row).map(|e| {
            let a = if e.is_alias {
                format!("エイリアス: {}\r\n", e.name)
            } else {
                String::new()
            };
            let run = if e.is_alias {
                if e.auto_run {
                    "\r\nEnter: 即実行"
                } else {
                    "\r\nEnter: 挿入のみ（編集してから実行）"
                }
            } else {
                ""
            };
            format!("{}コマンド: {}\r\n説明: {}{}", a, e.target, e.description, run)
        })
    });
    if let Some(text) = text {
        settings_set_text(parent, ID_DETAIL, &text);
    }
}

/// 選択状態に応じて「追加/更新」ボタンのラベルを切り替える
/// （選択あり=更新、なし=新規）。
pub(crate) unsafe fn settings_update_addbtn_label(parent: HWND) {
    let label = if settings_list_selection(parent).is_some() {
        "更新"
    } else {
        "新規"
    };
    settings_set_text(parent, ID_BTN_ADD, label);
}

/// 一覧で選択された行の内容をフォームに反映する（編集用）。
pub(crate) unsafe fn settings_on_select(parent: HWND) {
    let Some(idx) = settings_list_selection(parent) else { return };
    let entry = SETTINGS_ENTRIES.lock().ok().and_then(|v| v.get(idx).cloned());
    let Some(e) = entry else { return };
    if e.is_alias {
        settings_set_text(parent, ID_EDIT_ALIAS, &e.name);
        settings_set_text(parent, ID_EDIT_EXPANSION, &e.target);
    } else {
        // コマンドはエイリアス名を持たない → エイリアス欄は空、コマンド欄に本体
        settings_set_text(parent, ID_EDIT_ALIAS, "");
        settings_set_text(parent, ID_EDIT_EXPANSION, &e.target);
    }
    settings_set_text(parent, ID_EDIT_DESC, &e.description);
    // 即実行チェックを反映（コマンドは常に実行なのでオン）
    settings_set_check(parent, ID_CHK_AUTORUN, e.auto_run);
    if let Ok(mut sel) = SETTINGS_SELECTED.lock() {
        *sel = Some((e.is_alias, e.name.clone()));
    }
}

/// ファイル選択ダイアログでスクリプトを選ばせ、パスを返す。
pub(crate) unsafe fn settings_browse_script(owner: HWND) -> Option<String> {
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OPENFILENAMEW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
    };
    let mut file_buf = [0u16; 1024];
    // フィルタ: "スクリプト\0*.ps1;*.bat;*.cmd;*.sh;*.py;*.exe\0すべて\0*.*\0\0"
    let filter: Vec<u16> = "スクリプト\0*.ps1;*.bat;*.cmd;*.sh;*.py;*.exe\0すべてのファイル\0*.*\0\0"
        .encode_utf16()
        .collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: windows::core::PCWSTR(filter.as_ptr()),
        lpstrFile: windows::core::PWSTR(file_buf.as_mut_ptr()),
        nMaxFile: file_buf.len() as u32,
        // OFN_NOCHANGEDIR: ダイアログが作業ディレクトリを変えないように
        // （変わると相対パスの ime-learning.db 等が壊れる）。
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    if GetOpenFileNameW(&mut ofn).as_bool() {
        let end = file_buf.iter().position(|&c| c == 0).unwrap_or(0);
        let path = String::from_utf16_lossy(&file_buf[..end]);
        if path.is_empty() {
            None
        } else {
            Some(path)
        }
    } else {
        None
    }
}

pub(crate) extern "system" fn settings_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, ShowWindow, HMENU, SW_HIDE, WM_CLOSE, WM_COMMAND, WM_CREATE,
        WINDOW_EX_STYLE, WINDOW_STYLE, WS_BORDER, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
    };
    unsafe {
        match msg {
            WM_CREATE => {
                let hinst = GetModuleHandleW(None).unwrap_or_default();
                // モダンなフォント（Meiryo UI）を全コントロールに適用してリッチに見せる
                let font = CreateFontW(
                    -15, 0, 0, 0, 400, 0, 0, 0,
                    1, 0, 0, 5, 0,
                    w!("Meiryo UI"),
                );
                let mk = |class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: i32| {
                    let cw = to_wide(class);
                    let tw = to_wide(text);
                    if let Ok(ctrl) = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        windows::core::PCWSTR(cw.as_ptr()),
                        windows::core::PCWSTR(tw.as_ptr()),
                        WINDOW_STYLE(style),
                        x, y, w, h,
                        hwnd,
                        HMENU(id as isize as *mut core::ffi::c_void),
                        hinst,
                        None,
                    ) {
                        // WM_SETFONT=0x0030, 再描画あり(1)
                        windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                            ctrl, 0x0030, WPARAM(font.0 as usize), LPARAM(1),
                        );
                    }
                };
                // コモンコントロール（ListView）を有効化
                {
                    use windows::Win32::UI::Controls::{
                        InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX,
                    };
                    let icc = INITCOMMONCONTROLSEX {
                        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                        dwICC: ICC_LISTVIEW_CLASSES,
                    };
                    let _ = InitCommonControlsEx(&icc);
                }
                let vis = WS_VISIBLE.0 | WS_CHILD.0;
                let edit = vis | WS_BORDER.0 | WS_TABSTOP.0 | 0x0080; // ES_AUTOHSCROLL
                let btn = vis | WS_TABSTOP.0; // BS_PUSHBUTTON=0

                // クライアント幅から左右均一の余白・パディングで動的にレイアウトする。
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let cw = rc.right;
                let ch = rc.bottom;
                const M: i32 = 14; // 枠の外側余白（左右均一）
                const P: i32 = 10; // 枠の内側パディング（左右均一。枠見出しの字下げに合わせる）
                let gx = M; // 枠 左
                let gw = cw - 2 * M; // 枠 幅（右余白も M で均一）
                let il = gx + P; // 内側 左（ラベル開始＝枠見出しと揃う）
                let ir = gx + gw - P; // 内側 右端
                let labelw = 168; // ラベル列幅（最長「コマンド/スクリプト:」が収まる幅）
                let gap = 10; // ラベルと入力欄の間隔（検索/表示ラベルと同様の余白）
                let ex = il + labelw + gap; // 入力欄 左（3欄で統一）
                let ew = ir - ex; // 入力欄 幅

                // 枠（子より先に作り背面へ）。見出しはラベル開始位置(il)に合わせ、
                // 先頭空白は入れない（左に余計な背景余白を出さないため）。
                let form_h = 172;
                mk("BUTTON", "登録内容", vis | 0x7 /*BS_GROUPBOX*/, gx, 6, gw, form_h, 0);
                // 入力フォーム
                mk("STATIC", "エイリアス:", vis, il, 34, labelw, 20, 0);
                mk("EDIT", "", edit, ex, 32, ew, 26, ID_EDIT_ALIAS);
                mk("STATIC", "コマンド/スクリプト:", vis, il, 68, labelw, 20, 0);
                mk("EDIT", "", edit, ex, 66, ew - 90, 26, ID_EDIT_EXPANSION);
                mk("BUTTON", "参照...", btn, ir - 82, 66, 82, 26, ID_BTN_BROWSE);
                mk("STATIC", "説明:", vis, il, 102, labelw, 20, 0);
                mk("EDIT", "", edit, ex, 100, ew, 26, ID_EDIT_DESC);
                // 追加/更新・削除・クリアを右寄せで隣接配置（右端＝ir）
                let bw = 86;
                let bgap = 6;
                let btn_left = ir - 3 * bw - 2 * bgap;
                // Enter即実行チェック（オフ＝挿入のみで編集してから実行）。既定オン。
                mk(
                    "BUTTON",
                    "Enterで即実行（オフ＝挿入のみ）",
                    vis | WS_TABSTOP.0 | 0x0003, // BS_AUTOCHECKBOX
                    il, 141, btn_left - il - 12, 24, ID_CHK_AUTORUN,
                );
                mk("BUTTON", "クリア", btn, ir - bw, 138, bw, 30, ID_BTN_NEW);
                mk("BUTTON", "削除", btn, ir - 2 * bw - bgap, 138, bw, 30, ID_BTN_DELETE);
                mk("BUTTON", "新規", btn, btn_left, 138, bw, 30, ID_BTN_ADD);

                // 一覧セクション枠
                let list_gy = 6 + form_h + 10;
                let close_h = 34;
                let list_gh = ch - list_gy - close_h - 8;
                mk("BUTTON", "コマンド一覧", vis | 0x7, gx, list_gy, gw, list_gh, 0);
                // 1段目: 表示フィルタ（コンボ）＋並び替えボタン
                let row_a = list_gy + 26;
                mk("STATIC", "表示:", vis, il, row_a + 3, 44, 20, 0);
                // ComboBox（CBS_DROPDOWNLIST=0x0003 | WS_VSCROLL）
                mk(
                    "COMBOBOX",
                    "",
                    vis | WS_TABSTOP.0 | 0x0003 | 0x0200,
                    il + 48, row_a, 170, 160, ID_COMBO_FILTER,
                );
                mk("BUTTON", "並び替え: 作成順", btn, ir - 160, row_a, 160, 26, ID_BTN_SORT);
                // 2段目: 検索
                let row_b = row_a + 34;
                mk("STATIC", "検索:", vis, il, row_b + 3, 44, 20, 0);
                mk("EDIT", "", edit, il + 48, row_b, ir - (il + 48), 26, ID_EDIT_SEARCH);
                // 下部に「詳細（全文）」欄を設け、その上に ListView を置く。
                let lvx = il;
                let lvy = row_b + 34;
                let lvw = ir - il;
                let detail_h = 96; // 説明含む3行＋長文の折り返しが見えるよう高めに
                let lvh = (list_gy + list_gh) - lvy - P - detail_h - 8;
                // LVS_REPORT|LVS_SINGLESEL|LVS_SHOWSELALWAYS(0x0008)。
                // ※ 0x8000 は LVS_NOSORTHEADER なので使わない（ヘッダークリックが無効になる）。
                let list_style = vis | WS_BORDER.0 | WS_TABSTOP.0
                    | 0x0001 /*LVS_REPORT*/ | 0x0004 /*LVS_SINGLESEL*/ | 0x0008 /*LVS_SHOWSELALWAYS*/;
                let lvcls = to_wide("SysListView32");
                if let Ok(list) = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    windows::core::PCWSTR(lvcls.as_ptr()),
                    windows::core::PCWSTR(to_wide("").as_ptr()),
                    WINDOW_STYLE(list_style),
                    lvx, lvy, lvw, lvh,
                    hwnd,
                    HMENU(ID_LIST as isize as *mut core::ffi::c_void),
                    hinst,
                    None,
                ) {
                    windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                        list, 0x0030, WPARAM(font.0 as usize), LPARAM(1),
                    ); // WM_SETFONT
                    // 行全体選択・グリッド線・ホバー通知(INFOTIP)。ただし全文は
                    // フローティングでなくモーダル内の詳細欄に出す（背面に隠れないよう）。
                    SendMessageW(
                        list,
                        LVM_SETEXTENDEDLISTVIEWSTYLE,
                        WPARAM(0),
                        LPARAM(LVS_EX_FULLROWSELECT | LVS_EX_INFOTIP | LVS_EX_GRIDLINES),
                    );
                    let avail = lvw - 22;
                    lv_insert_column(list, 0, "エイリアス", avail * 22 / 100);
                    lv_insert_column(list, 1, "コマンド", avail * 40 / 100);
                    lv_insert_column(list, 2, "説明", avail * 23 / 100);
                    // 「Enterで即実行」チェックの状態を一覧でも見えるようにする列
                    lv_insert_column(list, 3, "Enter", avail * 15 / 100);
                }
                // 詳細欄（読み取り専用・複数行・自動折り返し＋縦スクロール）:
                // 選択/ホバー行の エイリアス/コマンド/説明 の全文を表示。長文は
                // 折り返し（ES_AUTOHSCROLL を付けない）＋縦スクロールで全部読める。
                let detail_style = vis | WS_BORDER.0
                    | 0x0004 /*ES_MULTILINE*/ | 0x0800 /*ES_READONLY*/
                    | 0x0040 /*ES_AUTOVSCROLL*/ | 0x0020_0000 /*WS_VSCROLL*/;
                mk(
                    "EDIT",
                    "行にマウスを合わせる/選択すると、ここに エイリアス・コマンド・説明 の全文が表示されます",
                    detail_style,
                    lvx, lvy + lvh + 8, lvw, detail_h, ID_DETAIL,
                );
                // フィルタ・コンボの項目を設定（CB_ADDSTRING=0x0143, CB_SETCURSEL=0x014E）
                if let Ok(combo) =
                    windows::Win32::UI::WindowsAndMessaging::GetDlgItem(hwnd, ID_COMBO_FILTER)
                {
                    for item in ["すべて", "エイリアスのみ", "コマンドのみ"] {
                        let w = to_wide(item);
                        SendMessageW(combo, 0x0143, WPARAM(0), LPARAM(w.as_ptr() as isize));
                    }
                    SendMessageW(combo, 0x014E, WPARAM(SETTINGS_FILTER as usize), LPARAM(0));
                }
                // 即実行チェックは既定オン
                settings_set_check(hwnd, ID_CHK_AUTORUN, true);
                mk("BUTTON", "閉じる", btn, ir - 84, list_gy + list_gh + 8, 84, 30, ID_BTN_CLOSE);
                settings_refresh_list(hwnd);
                settings_update_addbtn_label(hwnd);
                LRESULT(0)
            }
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const windows::Win32::UI::Controls::NMHDR;
                if !nmhdr.is_null() && (*nmhdr).idFrom == ID_LIST as usize {
                    let code = (*nmhdr).code;
                    let nmlv = lparam.0 as *const windows::Win32::UI::Controls::NMLISTVIEW;
                    if code == LVN_ITEMCHANGED && !nmlv.is_null() {
                        let ns = (*nmlv).uNewState;
                        let os = (*nmlv).uOldState;
                        // 新たに選択された行だけフォームと詳細欄へ反映
                        if (ns & LVIS_SELECTED) != 0 && (os & LVIS_SELECTED) == 0 {
                            settings_on_select(hwnd);
                            let row = (*nmlv).iItem;
                            if row >= 0 {
                                settings_show_detail(hwnd, row as usize);
                            }
                        }
                        settings_update_addbtn_label(hwnd);
                    } else if code == LVN_COLUMNCLICK && !nmlv.is_null() {
                        // 列ヘッダークリック → その列で名前順ソート（再クリックで反転）
                        let col = (*nmlv).iSubItem;
                        if SETTINGS_SORT_COL == col {
                            SETTINGS_SORT_DESC = !SETTINGS_SORT_DESC;
                        } else {
                            SETTINGS_SORT_COL = col;
                            SETTINGS_SORT_DESC = false;
                        }
                        settings_refresh_list(hwnd);
                    } else if code == LVN_GETINFOTIP {
                        // ホバー時: フローティングのツールチップは出さず（空文字にする）、
                        // モーダル内の詳細欄に全文を表示する（最前面設定の裏に隠れないため）。
                        let tip = lparam.0 as *mut windows::Win32::UI::Controls::NMLVGETINFOTIPW;
                        if !tip.is_null() {
                            let row = (*tip).iItem;
                            if row >= 0 {
                                settings_show_detail(hwnd, row as usize);
                            }
                            // 空文字を入れてフローティング表示を抑止
                            if (*tip).cchTextMax > 0 {
                                *(*tip).pszText.0 = 0;
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
                // 検索ボックス変更 → フィルタ更新（EN_CHANGE=0x0300）
                if id == ID_EDIT_SEARCH && code == 0x0300 {
                    settings_refresh_list(hwnd);
                    return LRESULT(0);
                }
                // 表示フィルタのコンボ変更（CBN_SELCHANGE=1）
                if id == ID_COMBO_FILTER && code == 1 {
                    if let Ok(combo) =
                        windows::Win32::UI::WindowsAndMessaging::GetDlgItem(hwnd, ID_COMBO_FILTER)
                    {
                        // CB_GETCURSEL=0x0147
                        let sel = SendMessageW(combo, 0x0147, WPARAM(0), LPARAM(0)).0 as i32;
                        SETTINGS_FILTER = sel.max(0);
                        settings_refresh_list(hwnd);
                    }
                    return LRESULT(0);
                }
                // 「Enterで即実行」チェックの切替（BN_CLICKED=0）→ 選択中エイリアスに即保存
                if id == ID_CHK_AUTORUN && code == 0 {
                    let checked = settings_get_check(hwnd, ID_CHK_AUTORUN);
                    let sel = SETTINGS_SELECTED.lock().ok().and_then(|s| s.clone());
                    if let Some((is_alias, key)) = sel {
                        if is_alias {
                            if let Some(context_mutex) = LIVE_CONTEXT.get() {
                                if let Ok(context) = context_mutex.lock() {
                                    if let Some(learning) = context.learning.as_ref() {
                                        let _ = learning.set_alias_auto_run(&key, checked);
                                    }
                                }
                            }
                            // 一覧の保持データ・Enter列・詳細欄を即反映
                            if let Ok(mut v) = SETTINGS_ENTRIES.lock() {
                                if let Some(e) = v.iter_mut().find(|e| e.is_alias && e.name == key) {
                                    e.auto_run = checked;
                                }
                            }
                            if let Some(idx) = settings_list_selection(hwnd) {
                                if let Ok(list) =
                                    windows::Win32::UI::WindowsAndMessaging::GetDlgItem(hwnd, ID_LIST)
                                {
                                    lv_set_sub(list, idx as i32, 3, autorun_label(checked));
                                }
                                settings_show_detail(hwnd, idx);
                            }
                        }
                    }
                    return LRESULT(0);
                }
                // （ListView の選択は WM_NOTIFY/LVN_ITEMCHANGED で処理）
                match id {
                    ID_BTN_BROWSE => {
                        if let Some(path) = settings_browse_script(hwnd) {
                            let exp = if path.contains(' ') {
                                format!("\"{}\"", path)
                            } else {
                                path
                            };
                            settings_set_text(hwnd, ID_EDIT_EXPANSION, &exp);
                            if settings_get_text(hwnd, ID_EDIT_ALIAS).trim().is_empty() {
                                let stem = exp
                                    .trim_matches('"')
                                    .rsplit(['\\', '/'])
                                    .next()
                                    .and_then(|f| f.split('.').next())
                                    .unwrap_or("")
                                    .to_string();
                                settings_set_text(hwnd, ID_EDIT_ALIAS, &stem);
                            }
                        }
                        LRESULT(0)
                    }
                    ID_BTN_SORT => {
                        // 作成順 ⇔ 使用頻度順 を切り替える（名前順は列ヘッダーで）
                        SETTINGS_BTN_FREQ = !SETTINGS_BTN_FREQ;
                        SETTINGS_SORT_COL = if SETTINGS_BTN_FREQ { -2 } else { -1 };
                        SETTINGS_SORT_DESC = false;
                        let label = if SETTINGS_BTN_FREQ {
                            "並び替え: 頻度順"
                        } else {
                            "並び替え: 作成順"
                        };
                        settings_set_text(hwnd, ID_BTN_SORT, label);
                        settings_refresh_list(hwnd);
                        LRESULT(0)
                    }
                    ID_BTN_NEW => {
                        settings_set_text(hwnd, ID_EDIT_ALIAS, "");
                        settings_set_text(hwnd, ID_EDIT_EXPANSION, "");
                        settings_set_text(hwnd, ID_EDIT_DESC, "");
                        settings_set_check(hwnd, ID_CHK_AUTORUN, true);
                        if let Ok(mut sel) = SETTINGS_SELECTED.lock() {
                            *sel = None;
                        }
                        // 一覧の選択も解除して「新規」表示に戻す
                        if let Ok(list) = windows::Win32::UI::WindowsAndMessaging::GetDlgItem(hwnd, ID_LIST) {
                            lv_clear_selection(list);
                        }
                        settings_update_addbtn_label(hwnd);
                        LRESULT(0)
                    }
                    ID_BTN_ADD => {
                        let alias = settings_get_text(hwnd, ID_EDIT_ALIAS).trim().to_string();
                        let expansion = settings_get_text(hwnd, ID_EDIT_EXPANSION).trim().to_string();
                        let desc = settings_get_text(hwnd, ID_EDIT_DESC);
                        if expansion.is_empty() {
                            return LRESULT(0);
                        }
                        // エイリアス欄が空ならコマンド登録、あればエイリアス登録
                        let new_is_alias = !alias.is_empty();
                        let new_key = if new_is_alias { alias.clone() } else { expansion.clone() };
                        let is_script = ["\u{2e}ps1", ".bat", ".cmd", ".sh", ".py", ".exe"]
                            .iter()
                            .any(|e| expansion.to_lowercase().contains(e));
                        let auto_run = settings_get_check(hwnd, ID_CHK_AUTORUN);
                        let old = SETTINGS_SELECTED.lock().ok().and_then(|mut s| s.take());
                        if let Some(context_mutex) = LIVE_CONTEXT.get() {
                            if let Ok(context) = context_mutex.lock() {
                                if let Some(learning) = context.learning.as_ref() {
                                    // 編集でキー/種別が変わったら旧エントリを消す
                                    if let Some((old_alias, old_key)) = &old {
                                        if *old_alias != new_is_alias || old_key != &new_key {
                                            if *old_alias {
                                                let _ = learning.delete_alias(old_key);
                                            } else {
                                                let _ = learning.delete_command(old_key);
                                            }
                                        }
                                    }
                                    let r = if new_is_alias {
                                        learning.add_alias(&alias, &expansion, &desc, is_script, auto_run)
                                    } else {
                                        learning.upsert_command(&expansion, &desc)
                                    };
                                    if let Err(e) = r {
                                        eprintln!("登録失敗: {}", e);
                                    }
                                }
                            }
                        }
                        // 保存した項目を選択したままにする（フォーム・チェックボックスが
                        // 保存内容と食い違わないように。次を続けて登録するときは「クリア」）。
                        // refresh_list が SETTINGS_SELECTED の項目を再選択し、
                        // settings_on_select がフォームへ反映する。
                        if let Ok(mut sel) = SETTINGS_SELECTED.lock() {
                            *sel = Some((new_is_alias, new_key.clone()));
                        }
                        settings_refresh_list(hwnd);
                        settings_update_addbtn_label(hwnd);
                        LRESULT(0)
                    }
                    ID_BTN_DELETE => {
                        let entry = settings_list_selection(hwnd)
                            .and_then(|i| SETTINGS_ENTRIES.lock().ok().and_then(|v| v.get(i).cloned()));
                        if let Some(e) = entry {
                            if let Some(context_mutex) = LIVE_CONTEXT.get() {
                                if let Ok(context) = context_mutex.lock() {
                                    if let Some(learning) = context.learning.as_ref() {
                                        if e.is_alias {
                                            let _ = learning.delete_alias(&e.name);
                                        } else {
                                            let _ = learning.delete_command(&e.name);
                                        }
                                    }
                                }
                            }
                            settings_set_text(hwnd, ID_EDIT_ALIAS, "");
                            settings_set_text(hwnd, ID_EDIT_EXPANSION, "");
                            settings_set_text(hwnd, ID_EDIT_DESC, "");
                            // チェックボックスも既定（即実行）へ戻す（前の項目の状態を残さない）
                            settings_set_check(hwnd, ID_CHK_AUTORUN, true);
                            if let Ok(mut sel) = SETTINGS_SELECTED.lock() {
                                *sel = None;
                            }
                            settings_refresh_list(hwnd);
                            settings_update_addbtn_label(hwnd);
                        }
                        LRESULT(0)
                    }
                    ID_BTN_CLOSE => {
                        SETTINGS_OPEN = false;
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        LRESULT(0)
                    }
                    _ => LRESULT(0),
                }
            }
            WM_CLOSE => {
                // 破棄せず隠す（次回すぐ開けるように）
                SETTINGS_OPEN = false;
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// エイリアス設定ウィンドウを開く（なければ作る）。ホットキー Ctrl+Alt+A から呼ぶ。
pub(crate) unsafe fn open_settings_window() {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, RegisterClassW, SetForegroundWindow, ShowWindow, SW_SHOW,
        WNDCLASSW, WS_CAPTION, WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
    };
    // コマンド予測ポップアップは閉じない。設定表示中はポップアップの最前面固定を
    // 止め（SETTINGS_OPEN）、設定ウィンドウ側を TOPMOST にして前面へ。
    SETTINGS_OPEN = true;
    // いま出ているポップアップを即座に非最前面へ落として設定の下に置く
    if let Some(pop) = CANDIDATE_HWND {
        let _ = SetWindowPos(
            pop,
            HWND_NOTOPMOST,
            0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    if let Some(hwnd) = SETTINGS_HWND {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        settings_refresh_list(hwnd);
        return;
    }
    let Ok(hinst) = GetModuleHandleW(None) else { return };
    let class_name = w!("ImeAliasSettings");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(settings_wndproc),
        hInstance: hinst.into(),
        lpszClassName: class_name,
        hCursor: windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
            None,
            windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
        )
        .unwrap_or_default(),
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
            (16 + 1) as *mut core::ffi::c_void, // COLOR_BTNFACE+1
        ),
        ..Default::default()
    };
    RegisterClassW(&wc);
    let title = w!("コマンド設定");
    // TOPMOST にする（コマンド予測ポップアップ=TOPMOST より前面に出すため）。
    // ホバー説明はカーソル付近のツールチップ(INFOTIP)、参照ダイアログはモーダルで
    // 前面に出るので、TOPMOST でも隠れない。
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST,
        class_name,
        title,
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        150, 50, 640, 748,
        None,
        None,
        hinst,
        None,
    ) else {
        return;
    };
    SETTINGS_HWND = Some(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}

/// フォアグラウンドが自プロセスのウィンドウ（設定画面・単語登録・ファイルダイアログ等）か。
/// その場合フックは何もしない（自分のUIへの入力を横取りしないため）。
pub(crate) unsafe fn foreground_is_ours() -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    // static mut への参照を避けるため値コピー（Option<HWND> は Copy）
    let cmd_hwnd = SETTINGS_HWND;
    let word_hwnd = WORD_SETTINGS_HWND;
    if cmd_hwnd.is_none() && word_hwnd.is_none() {
        return false;
    }
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(fg, Some(&mut pid));
    pid == GetCurrentProcessId()
}

/// コマンド設定・単語登録のいずれかの設定ウィンドウが表示中か。
/// 表示中は候補ポップアップの最前面固定を止めて設定側を前面に保つ。
pub(crate) unsafe fn any_settings_open() -> bool {
    SETTINGS_OPEN || WORD_SETTINGS_OPEN
}

// ============ 単語登録ウィンドウ（日本語変換モード用） ============
//
// コマンド設定ウィンドウと同じデザイン（フォーム＋一覧＋閉じる）で、
// 「読み → 表記」のユーザー単語を登録・削除する。登録した語は user_dictionary(DB)に
// 保存され、ライブ変換器の辞書へ即注入される（辞書に無い複合語を変換可能に）。

pub(crate) static mut WORD_SETTINGS_HWND: Option<HWND> = None;
/// 単語登録ウィンドウが表示中か（コマンド設定の SETTINGS_OPEN と同様、
/// 表示中はポップアップの最前面固定を止めて登録ウィンドウを前面に保つ）。
pub(crate) static mut WORD_SETTINGS_OPEN: bool = false;
/// 一覧に今表示している (読み, 表記)。リスト選択の index からこれで引く。
pub(crate) static WORD_ENTRIES: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub(crate) const ID_WORD_READING: i32 = 201;
pub(crate) const ID_WORD_SURFACE: i32 = 202;
pub(crate) const ID_WORD_ADD: i32 = 203;
pub(crate) const ID_WORD_CLEAR: i32 = 204;
pub(crate) const ID_WORD_DELETE: i32 = 205;
pub(crate) const ID_WORD_LIST: i32 = 206;
pub(crate) const ID_WORD_CLOSE: i32 = 207;
pub(crate) const ID_WORD_SEARCH: i32 = 208;

/// user_dictionary から一覧を読み直して ListView に反映する（検索フィルタ対応）。
pub(crate) unsafe fn word_refresh_list(parent: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    let Ok(list) = GetDlgItem(parent, ID_WORD_LIST) else { return };

    let mut entries: Vec<(String, String)> = LIVE_CONTEXT
        .get()
        .and_then(|ctx| ctx.lock().ok().map(|c| c.all_user_words()))
        .unwrap_or_default();

    // 検索フィルタ（読み・表記のいずれかに含まれる。大文字小文字無視）
    let query = settings_get_text(parent, ID_WORD_SEARCH).trim().to_lowercase();
    if !query.is_empty() {
        entries.retain(|(r, s)| {
            r.to_lowercase().contains(&query) || s.to_lowercase().contains(&query)
        });
    }
    entries.sort();

    SendMessageW(list, LVM_DELETEALLITEMS, WPARAM(0), LPARAM(0));
    for (i, (r, s)) in entries.iter().enumerate() {
        lv_insert_row(list, i as i32, r);
        lv_set_sub(list, i as i32, 1, s);
    }
    if let Ok(mut store) = WORD_ENTRIES.lock() {
        *store = entries;
    }
    // 2列（読み|表記）を一覧幅に合わせる
    let mut rc = RECT::default();
    let _ = GetClientRect(list, &mut rc);
    let w0 = (rc.right * 42 / 100).max(80);
    lv_set_column_width(list, 0, w0);
    lv_set_column_width(list, 1, (rc.right - w0).max(80));
}

/// 一覧で選択した行の (読み, 表記) をフォームに反映する（編集・再登録用）。
pub(crate) unsafe fn word_on_select(parent: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    let Ok(list) = GetDlgItem(parent, ID_WORD_LIST) else { return };
    let Some(idx) = lv_selected(list) else { return };
    let entry = WORD_ENTRIES.lock().ok().and_then(|v| v.get(idx).cloned());
    if let Some((reading, surface)) = entry {
        settings_set_text(parent, ID_WORD_READING, &reading);
        settings_set_text(parent, ID_WORD_SURFACE, &surface);
    }
}

pub(crate) extern "system" fn word_settings_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, GetDlgItem, ShowWindow, HMENU, SW_HIDE, WM_CLOSE, WM_COMMAND, WM_CREATE,
        WINDOW_EX_STYLE, WINDOW_STYLE, WS_BORDER, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
    };
    unsafe {
        match msg {
            WM_CREATE => {
                let hinst = GetModuleHandleW(None).unwrap_or_default();
                let font = CreateFontW(-15, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Meiryo UI"));
                let mk = |class: &str, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: i32| {
                    let cw = to_wide(class);
                    let tw = to_wide(text);
                    if let Ok(ctrl) = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        windows::core::PCWSTR(cw.as_ptr()),
                        windows::core::PCWSTR(tw.as_ptr()),
                        WINDOW_STYLE(style),
                        x, y, w, h,
                        hwnd,
                        HMENU(id as isize as *mut core::ffi::c_void),
                        hinst,
                        None,
                    ) {
                        SendMessageW(ctrl, 0x0030, WPARAM(font.0 as usize), LPARAM(1)); // WM_SETFONT
                    }
                };
                {
                    use windows::Win32::UI::Controls::{
                        InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX,
                    };
                    let icc = INITCOMMONCONTROLSEX {
                        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                        dwICC: ICC_LISTVIEW_CLASSES,
                    };
                    let _ = InitCommonControlsEx(&icc);
                }
                let vis = WS_VISIBLE.0 | WS_CHILD.0;
                let edit = vis | WS_BORDER.0 | WS_TABSTOP.0 | 0x0080; // ES_AUTOHSCROLL
                let btn = vis | WS_TABSTOP.0;

                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let cw = rc.right;
                let ch = rc.bottom;
                const M: i32 = 14;
                const P: i32 = 10;
                let gx = M;
                let gw = cw - 2 * M;
                let il = gx + P;
                let ir = gx + gw - P;
                let labelw = 56;
                let gap = 10;
                let ex = il + labelw + gap;
                let ew = ir - ex;

                // 入力フォーム枠（登録/削除/クリアのボタンも枠内に収める）
                let form_h = 160;
                mk("BUTTON", "単語登録", vis | 0x7 /*BS_GROUPBOX*/, gx, 6, gw, form_h, 0);
                mk("STATIC", "読み:", vis, il, 34, labelw, 20, 0);
                mk("EDIT", "", edit, ex, 32, ew, 26, ID_WORD_READING);
                mk("STATIC", "表記:", vis, il, 68, labelw, 20, 0);
                mk("EDIT", "", edit, ex, 66, ew, 26, ID_WORD_SURFACE);
                mk("STATIC", "（読みはひらがな。表記はその読みで出したい語）", vis, il, 98, gw - 2 * P, 20, 0);
                // ボタン: 登録 / 削除 / クリア を右寄せ（枠の下辺=6+form_h の内側に配置）
                let bw = 86;
                let bgap = 6;
                let by = 6 + form_h - 30 - P; // 枠下辺から内側パディング分だけ上
                mk("BUTTON", "登録", btn, ir - 3 * bw - 2 * bgap, by, bw, 30, ID_WORD_ADD);
                mk("BUTTON", "削除", btn, ir - 2 * bw - bgap, by, bw, 30, ID_WORD_DELETE);
                mk("BUTTON", "クリア", btn, ir - bw, by, bw, 30, ID_WORD_CLEAR);

                // 一覧セクション
                let list_gy = 6 + form_h + 10;
                let close_h = 34;
                let list_gh = ch - list_gy - close_h - 8;
                mk("BUTTON", "登録済みの単語", vis | 0x7, gx, list_gy, gw, list_gh, 0);
                let row_a = list_gy + 26;
                mk("STATIC", "検索:", vis, il, row_a + 3, 44, 20, 0);
                mk("EDIT", "", edit, il + 48, row_a, ir - (il + 48), 26, ID_WORD_SEARCH);
                let lvx = il;
                let lvy = row_a + 34;
                let lvw = ir - il;
                let lvh = (list_gy + list_gh) - lvy - P;
                let list_style = vis | WS_BORDER.0 | WS_TABSTOP.0
                    | 0x0001 /*LVS_REPORT*/ | 0x0004 /*LVS_SINGLESEL*/ | 0x0008 /*LVS_SHOWSELALWAYS*/;
                let lvcls = to_wide("SysListView32");
                if let Ok(list) = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    windows::core::PCWSTR(lvcls.as_ptr()),
                    windows::core::PCWSTR(to_wide("").as_ptr()),
                    WINDOW_STYLE(list_style),
                    lvx, lvy, lvw, lvh,
                    hwnd,
                    HMENU(ID_WORD_LIST as isize as *mut core::ffi::c_void),
                    hinst,
                    None,
                ) {
                    SendMessageW(list, 0x0030, WPARAM(font.0 as usize), LPARAM(1));
                    SendMessageW(
                        list,
                        LVM_SETEXTENDEDLISTVIEWSTYLE,
                        WPARAM(0),
                        LPARAM(LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES),
                    );
                    let avail = lvw - 22;
                    lv_insert_column(list, 0, "読み", avail * 42 / 100);
                    lv_insert_column(list, 1, "表記", avail * 58 / 100);
                }
                mk("BUTTON", "閉じる", btn, ir - 84, list_gy + list_gh + 8, 84, 30, ID_WORD_CLOSE);
                word_refresh_list(hwnd);
                let _ = GetDlgItem(hwnd, ID_WORD_READING);
                LRESULT(0)
            }
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const windows::Win32::UI::Controls::NMHDR;
                if !nmhdr.is_null() && (*nmhdr).idFrom == ID_WORD_LIST as usize {
                    let code = (*nmhdr).code;
                    let nmlv = lparam.0 as *const windows::Win32::UI::Controls::NMLISTVIEW;
                    if code == LVN_ITEMCHANGED && !nmlv.is_null() {
                        let ns = (*nmlv).uNewState;
                        let os = (*nmlv).uOldState;
                        if (ns & LVIS_SELECTED) != 0 && (os & LVIS_SELECTED) == 0 {
                            word_on_select(hwnd);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    ID_WORD_ADD => {
                        let reading = settings_get_text(hwnd, ID_WORD_READING).trim().to_string();
                        let surface = settings_get_text(hwnd, ID_WORD_SURFACE).trim().to_string();
                        if !reading.is_empty() && !surface.is_empty() {
                            if let Some(ctx) = LIVE_CONTEXT.get() {
                                if let Ok(mut c) = ctx.lock() {
                                    c.register_user_word(&reading, &surface);
                                }
                            }
                            settings_set_text(hwnd, ID_WORD_READING, "");
                            settings_set_text(hwnd, ID_WORD_SURFACE, "");
                            word_refresh_list(hwnd);
                        }
                        LRESULT(0)
                    }
                    ID_WORD_DELETE => {
                        use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
                        let entry = GetDlgItem(hwnd, ID_WORD_LIST)
                            .ok()
                            .and_then(|list| lv_selected(list))
                            .and_then(|i| WORD_ENTRIES.lock().ok().and_then(|v| v.get(i).cloned()));
                        if let Some((reading, surface)) = entry {
                            if let Some(ctx) = LIVE_CONTEXT.get() {
                                if let Ok(mut c) = ctx.lock() {
                                    c.delete_user_word(&reading, &surface);
                                }
                            }
                            settings_set_text(hwnd, ID_WORD_READING, "");
                            settings_set_text(hwnd, ID_WORD_SURFACE, "");
                            word_refresh_list(hwnd);
                        }
                        LRESULT(0)
                    }
                    ID_WORD_CLEAR => {
                        settings_set_text(hwnd, ID_WORD_READING, "");
                        settings_set_text(hwnd, ID_WORD_SURFACE, "");
                        LRESULT(0)
                    }
                    ID_WORD_SEARCH => {
                        // EN_CHANGE=0x0300 で検索フィルタ更新
                        let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
                        if code == 0x0300 {
                            word_refresh_list(hwnd);
                        }
                        LRESULT(0)
                    }
                    ID_WORD_CLOSE => {
                        WORD_SETTINGS_OPEN = false;
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        LRESULT(0)
                    }
                    _ => LRESULT(0),
                }
            }
            WM_CLOSE => {
                WORD_SETTINGS_OPEN = false;
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 単語登録ウィンドウを開く（なければ作る）。日本語変換モードの Ctrl+Alt+A から呼ぶ。
pub(crate) unsafe fn open_word_settings_window() {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, RegisterClassW, SetForegroundWindow, ShowWindow, SW_SHOW,
        WNDCLASSW, WS_CAPTION, WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
    };
    WORD_SETTINGS_OPEN = true;
    // 変換候補ポップアップを下げて、登録ウィンドウを前面に
    if let Some(pop) = CANDIDATE_HWND {
        let _ = SetWindowPos(
            pop, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    if let Some(hwnd) = WORD_SETTINGS_HWND {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        word_refresh_list(hwnd);
        return;
    }
    let Ok(hinst) = GetModuleHandleW(None) else { return };
    let class_name = w!("ImeWordSettings");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(word_settings_wndproc),
        hInstance: hinst.into(),
        lpszClassName: class_name,
        hCursor: windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
            None,
            windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
        )
        .unwrap_or_default(),
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH((16 + 1) as *mut core::ffi::c_void),
        ..Default::default()
    };
    RegisterClassW(&wc);
    let title = w!("単語登録（日本語変換）");
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST,
        class_name,
        title,
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        180, 60, 560, 560,
        None,
        None,
        hinst,
        None,
    ) else {
        return;
    };
    WORD_SETTINGS_HWND = Some(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}
