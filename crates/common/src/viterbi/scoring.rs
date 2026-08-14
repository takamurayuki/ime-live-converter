//! コスト調整: 定番語シード・ペナルティ・あいまい読みバリエーション

use super::*;

/// 品詞が内容語（助詞・助動詞・記号・未知語でない）か判定する
pub fn is_content_pos(pos: &str) -> bool {
    !(pos.starts_with("助詞")
        || pos.starts_with("助動詞")
        || pos.starts_with("記号")
        || pos.starts_with("未知語")
        || pos.starts_with("フィラー")
        || pos.starts_with("カタカナ"))
}

/// 頻出語の初期プリセット（読み, 表記）
///
/// IPA辞書は解析用のため、ごく一般的な語（会う・水 など）が稀な同音漢字
/// （遭う・瑞 など）より低コストなことがある。起動時にこれらを薄く
/// 「学習済み」として入れておき、初回から自然な変換にする。
/// ユーザーの実学習（頻度が上がる）が優先されるので上書きされる。
pub(crate) const COMMON_WORD_SEED: &[(&str, &str)] = &[
    ("あう", "会う"), ("みず", "水"), ("ひと", "人"), ("て", "手"),
    ("め", "目"), ("き", "木"), ("いえ", "家"), ("やま", "山"),
    ("うみ", "海"), ("そら", "空"), ("みる", "見る"), ("いう", "言う"),
    ("おもう", "思う"), ("きく", "聞く"), ("かく", "書く"), ("よむ", "読む"),
    ("はなす", "話す"), ("たべる", "食べる"), ("のむ", "飲む"), ("かう", "買う"),
    ("まつ", "待つ"), ("もつ", "持つ"), ("しる", "知る"), ("つくる", "作る"),
    ("つかう", "使う"), ("わかる", "分かる"), ("かえる", "帰る"), ("あるく", "歩く"),
    ("はしる", "走る"), ("たつ", "立つ"), ("すわる", "座る"), ("ある", "有る"),
    ("てんき", "天気"), ("しごと", "仕事"), ("じかん", "時間"), ("ばしょ", "場所"),
    ("ことば", "言葉"), ("かんがえ", "考え"), ("きもち", "気持ち"), ("せかい", "世界"),
    // 形容動詞語幹（文末で接続ペナルティを受け、稀な同音語に負けやすい）
    ("かのう", "可能"), ("じゅうよう", "重要"), ("ひつよう", "必要"), ("べんり", "便利"),
    ("たいせつ", "大切"), ("かんたん", "簡単"), ("じゅうぶん", "十分"), ("あんぜん", "安全"),
    ("じゆう", "自由"), ("とくべつ", "特別"), ("ゆうめい", "有名"), ("げんき", "元気"),
    ("だいじょうぶ", "大丈夫"), ("しんぱい", "心配"), ("べんきょう", "勉強"),
    ("せつめい", "説明"), ("じゅんび", "準備"), ("せいこう", "成功"), ("しっぱい", "失敗"),
    // IT・変換まわりで誤変換しやすい超頻出語
    ("かんじ", "漢字"), ("へんかん", "変換"), ("にゅうりょく", "入力"),
    ("しゅつりょく", "出力"), ("もじ", "文字"), ("たんご", "単語"), ("ぶんしょう", "文章"),
    ("けんさく", "検索"), ("せってい", "設定"), ("がめん", "画面"), ("そうさ", "操作"),
    ("もんだい", "問題"), ("ないよう", "内容"), ("かいけつ", "解決"),
    ("うしろ", "後ろ"), ("まえ", "前"), ("となり", "隣"), ("よこ", "横"),
    ("おなか", "お腹"),
    // 長い動詞は辞書コストが高く、安いカタカナ断片(カン 等)+助詞の分割に
    // 負けやすい。よく使う動詞を優先しておく。
    ("かんがえる", "考える"), ("かんがえた", "考えた"),
];

/// 頻出語プリセットのボーナス（moderate。実学習で上書きされる）
pub(crate) const COMMON_WORD_SEED_BONUS: i32 = 1500;

/// バイグラム学習ボーナスの上限（接続コスト規模に合わせ、断片パスの暴走を防ぐ）
pub(crate) const BIGRAM_BONUS_CAP: i32 = 2500;

/// ユニグラム学習ボーナスの上限（短い語が長い語を分断するのを防ぐ）
pub(crate) const UNIGRAM_BONUS_CAP: i32 = 6000;

/// 「もしかして」補正候補の妥当性ゲート（1文字あたりコストの上限）。
///
/// 辞書に実在する語かどうか（未知語/カタカナ化が残らないか）だけでは、
/// NEologdなど巨大化した辞書では「実在はするが意味を成さない希少な
/// 当て字・複合語」まで通ってしまう。今日(≈1040/字)や日本(≈611/字)の
/// ような実語は通し、紗綾なら(≈1644/字)のような造語は「意味を成す
/// 補正が見つからなかった」として弾くためのしきい値。
/// ローマ字取り残し補正（hook-dll側）と共通のしきい値を使う。
pub const CORRECTION_PER_CHAR_COST_CEILING: i32 = 1300;

/// コストと文字数から、上記しきい値内に収まる（＝もっともらしい）かを判定する。
pub fn is_plausible_correction_cost(cost: i32, char_count: usize) -> bool {
    cost / (char_count.max(1) as i32) <= CORRECTION_PER_CHAR_COST_CEILING
}

/// この文節は「自動変換に失敗した」ものか。
/// 未知語（辞書に無くひらがなのまま）またはカタカナ・フォールバック
/// （きづついて→キヅツイテ のように読みが辞書語にならずカタカナ化されたもの）。
/// 正しく漢字・辞書語に変換できた文節は false（もしかしての対象外）。
pub(crate) fn is_failed_segment(e: &WordEntry) -> bool {
    e.pos.starts_with("未知語") || e.pos == "カタカナ"
}

/// 辞書上は「変換に成功」しているが、1文字あたりのコストが極端に高い
/// （＝実際にはほぼ使われない希少語）内容語かどうか。
///
/// もしかしては本来 `is_failed_segment`（未知語/カタカナ化）だけを対象に
/// していたが、それだと「線」+「セ」+「に」のように、断片それぞれは
/// 辞書に実在する語として変換に"成功"してしまい、全体としては意味を
/// 成さない誤字（例:「せんせに」←「先生に」の入力ミス）を拾えない。
/// 助詞・助動詞等（`is_content_pos`で除外）は読みが短いだけで正当に
/// コストが高いことが多いため対象外にし、内容語（名詞・動詞語幹等）に
/// 限定する。しきい値は一般的な内容語（今日≈1421/字・天気≈1483/字・
/// 仕事≈1486/字）を誤検出しない範囲で、希少な単独語（苧≈6471/字・
/// セ≈4792/字 等）だけを拾えるよう、実測に基づき安全側（高め）に設定。
const IMPLAUSIBLE_CONTENT_WORD_PER_CHAR_COST: i32 = 3800;

pub(crate) fn is_implausible_content_word(e: &WordEntry) -> bool {
    if !is_content_pos(&e.pos) {
        return false;
    }
    let char_count = e.reading.chars().count().max(1) as i32;
    (e.cost as i32) / char_count > IMPLAUSIBLE_CONTENT_WORD_PER_CHAR_COST
}

/// 読みの1編集で誤字脱字の補正候補（変種）を生成する
///
/// - 取り違えやすいかなの相互置換（は↔わ・を↔お・へ↔え・づ↔ず・ぢ↔じ、
///   および 大きいかな↔小さいかな つ↔っ・や↔ゃ 等）
/// - 1文字削除（余分に打ってしまった打鍵）
pub(crate) fn fuzzy_variants(reading: &str) -> Vec<(String, bool)> {
    let chars: Vec<char> = reading.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    if n < 2 || n > 16 {
        return out;
    }
    // 相互に取り違えやすいペア（どちらの向きも試す）
    const SWAPS: &[(char, char)] = &[
        ('わ', 'は'), ('お', 'を'), ('え', 'へ'), ('づ', 'ず'), ('ぢ', 'じ'),
        // 促音・拗音（大きく打ってしまう誤り）
        ('つ', 'っ'), ('や', 'ゃ'), ('ゆ', 'ゅ'), ('よ', 'ょ'),
        ('あ', 'ぁ'), ('い', 'ぃ'), ('う', 'ぅ'), ('お', 'ぉ'),
    ];
    for i in 0..n {
        for &(a, b) in SWAPS {
            let repl = if chars[i] == a {
                Some(b)
            } else if chars[i] == b {
                Some(a)
            } else {
                None
            };
            if let Some(r) = repl {
                let mut v = chars.clone();
                v[i] = r;
                out.push((v.into_iter().collect(), false)); // 置換（同じ長さ）
            }
        }
    }
    // 1文字削除（余分な打鍵の除去）
    for i in 0..n {
        let mut v = chars.clone();
        v.remove(i);
        out.push((v.into_iter().collect(), true)); // 削除
    }
    // 小さいかなの挿入漏れ（脱字）: っ・ゃ・ゅ・ょ を各位置に挿入して試す。
    // 例: がこう→がっこう、きよう→きょう（は既に置換でも拾えるが挿入でも補完）。
    const SMALL: &[char] = &['っ', 'ゃ', 'ゅ', 'ょ'];
    if n <= 12 {
        for i in 1..=n {
            for &s in SMALL {
                let mut v = chars.clone();
                v.insert(i, s);
                out.push((v.into_iter().collect(), true)); // 挿入も削除同様に厳しめ扱い
            }
        }
    }
    out
}


/// 使用頻度をコスト減額（ボーナス）に変換する
///
/// 1回=1500、上限20000。同音語のIPAコスト差（数千）を数回の使用で
/// 覆せるスケール。ユーザーが選んだ語を確実に優先させ、
/// 「使うほど賢くなる」を実現する。(reading,surface) 単位のボーナスなので
/// 大きくても他の語には影響しない。
pub fn frequency_to_bonus(freq: u32) -> i32 {
    ((freq as i32) * 1500).min(20000)
}

/// カタカナ表記の辞書エントリに対するコストペナルティ
///
/// IPA辞書はカタカナ表記（ネコ・イヌ・ヤマ・ホン・ヲ 等）を低コストで
/// 持っており、「ねこ」→「ネコ」のように一般漢字語（猫）を押しのけてしまう。
/// 本来必要なカタカナ（外来語）は未知語のフォールバックで別途生成される
/// ので、辞書のカタカナ表記は実効コストを上げて漢字/かなを優先させる。
/// フォールバックで生成したカタカナ(pos=カタカナ)は対象外。
///
/// 非カタカナ表記の固有名詞（漢字・ローマ字表記）全般に一律ペナルティを
/// 掛ける案も検証したが、「おおさか」→大阪(cost 4257) のような base
/// IPADic 由来の正当で頻出な地名まで押しのけてしまう回帰が確認されたため
/// 見送った。NEologd由来の希少な固有名詞（がっこ→"gacco"、きづつ→木筒 等）
/// と、正当で頻出な固有名詞（大阪・東京等）はコストだけでは区別できず
/// （辞書に語の出典情報が無い）、静的な一律ペナルティでは安全に分離できない。
pub(crate) fn proper_noun_penalty(surface: &str, pos: &str) -> i32 {
    if !pos.contains("固有名詞") {
        return 0;
    }
    let all_katakana = !surface.is_empty()
        && surface.chars().all(|c| {
            ('\u{30A1}'..='\u{30FA}').contains(&c) || ('\u{30FC}'..='\u{30FF}').contains(&c)
        });
    if all_katakana {
        3000
    } else {
        0
    }
}

/// 入力全体が助詞1文字か（単独助詞をひらがなのままにする判定）
///
/// これらは単独で打つと接続コストの都合で同音漢字（刃・賭・藻・戸 等）に
/// 負けやすいが、助詞としてはひらがなが正しい。文中では通常の変換に任せる
/// ため、reading 全体がちょうど1つの助詞のときだけ真を返す。
pub(crate) fn is_lone_particle(reading: &str) -> bool {
    matches!(
        reading,
        "は" | "を" | "が" | "に" | "へ" | "と" | "も" | "の" | "で" | "や"
    )
}

/// 記号（＆＠×￥ 等）表記へのコストペナルティ
///
/// IPA辞書は「と」→「＆」のように、かな読みに ASCII/全角記号を低コストで
/// 割り当てていることがある。かなを記号へ変換するのはほぼ誤りなので、
/// 記号品詞かつ ASCII/全角英数記号の表記に強いペナルティを付ける。
/// 句読点（、。「」等）は CJK 記号域なので対象外。
///
/// ASCII/全角記号（U+0021-007E, U+FF01-FF5E）に加え、Latin-1 Supplement
/// の記号（U+00A0-00FF、×¥¢£等）と全角/半角形の通貨記号ブロック
/// （U+FFE0-FFEE、￥￡￠￦等）もペナルティ対象に含める。「かける」の1位が
/// 乗算記号「×」になる、「えんしゅうりつ」の1位に「￥」が混入する等、
/// この2レンジの記号が高頻度語を押しのける実例が確認されたため
/// （タスク#583, research.md カテゴリA）。
///
/// ペナルティ値は旧来の5000から7000へ引き上げた。実辞書(system.dic)で
/// 「×」の生コストが-279と極端に低く、旧値では最有力候補の動詞「掛ける」
/// (5680)に対してペナルティ適用後も (-279+5000=4721 < 5680) で勝てず、
/// 「かける」の1位が「×」のままになる実例を確認したため
/// （タスク#583, research.md カテゴリA）。
pub(crate) fn symbol_penalty(surface: &str, pos: &str) -> i32 {
    if !pos.starts_with("記号") {
        return 0;
    }
    let is_ascii_symbol = !surface.is_empty()
        && surface.chars().all(|c| {
            ('\u{0021}'..='\u{007E}').contains(&c)
                || ('\u{FF01}'..='\u{FF5E}').contains(&c)
                || ('\u{00A0}'..='\u{00FF}').contains(&c)
                || ('\u{FFE0}'..='\u{FFEE}').contains(&c)
        });
    if is_ascii_symbol {
        7000
    } else {
        0
    }
}

/// 形容詞の終止形（〜い）に接続助詞「て」が直接続く不自然な接続への
/// コストペナルティ（例:「すいている」→「酸い」+「て」で「空いている」
/// が押しのけられるのを防ぐ）。
///
/// 現代日本語の文法では、形容詞に「て」が続く場合は連用形（〜くて。
/// 「酸くて」「高くて」等）を使うのが正しく、終止形（辞書見出し形。
/// 「酸い」「高い」等）に直接「て」が続くことはない（「酸いて」は誤り）。
/// IPA辞書の連接コストはこの終止形+てを一定のコストで許可してしまって
/// おり、たまたま安い同音の形容詞（「酸い」等）が「すいて」のような
/// 本来は動詞の音便形＋てで読むべき語（「空いて」等）を押しのけてしまう
/// ことがある。学習の有無に関わらず文法的に常に誤りなので、無条件で
/// ペナルティを掛ける。
pub(crate) fn adjective_terminal_then_te_penalty(prev: &WordEntry, cur: &WordEntry) -> i32 {
    if !prev.pos.starts_with("形容詞") || !prev.surface.ends_with('い') {
        return 0;
    }
    if cur.surface != "て" || !cur.pos.starts_with("助詞") {
        return 0;
    }
    5000
}

/// 1文字漢字の表記に対するコストペナルティを返す
///
/// 単独で使われることが稀な1文字漢字（教・卿・挟 など）が、IPA辞書の
/// 低コストのせいで一般的な複合語（今日・天気 など）より優先されるのを防ぐ。
/// ひらがな1文字（助詞 は・が・を 等）は対象外なので誤って下げない。
pub(crate) fn single_kanji_penalty(surface: &str, penalty: i32) -> i32 {
    let mut chars = surface.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return 0; // 2文字以上は対象外
    };
    let is_kanji =
        ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c);
    if is_kanji {
        penalty
    } else {
        0
    }
}

/// カタカナ語の直後に単独の接尾辞漢字が続く「アンバランスな」組み合わせへの
/// 接続コストペナルティ（例: 「じんせい」→「ジン性」のような無意味な変換を防ぐ）。
///
/// IPA辞書は「名詞+接尾辞」の接続コストを実際の相性に関わらず一律に安く
/// 見積もる傾向があり、たまたま低コストなカタカナ名詞（ジン＝gin 等）に、
/// 学習で優先度が上がった1文字漢字の接尾辞（性・制 等）がくっつくと、
/// 「人生」のような正しい一語の変換より安く見えてしまうことがある。
/// 「語・人・製・風・式・系・産・型・教・街・圏・流・調」は外来語（国名・
/// 地名・言語名等）に実際によく付く接尾辞なので例外として許容する。
/// 表記がちょうど1文字の漢字か
fn is_single_kanji_surface(surface: &str) -> bool {
    let mut chars = surface.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return false;
    };
    ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
}

pub(crate) fn katakana_kanji_suffix_penalty(prev: &WordEntry, cur: &WordEntry) -> i32 {
    if !cur.pos.contains("接尾") {
        return 0;
    }
    // 対象は1文字漢字の接尾辞のみ。語・人・製 等の代表的な接尾辞の多くは
    // 1文字であり、下の例外リストで個別に救済する。
    if !is_single_kanji_surface(&cur.surface) {
        return 0;
    }
    const ALLOWED_SUFFIXES: &[&str] =
        &["語", "人", "製", "風", "式", "系", "産", "型", "教", "街", "圏", "流", "調"];
    if ALLOWED_SUFFIXES.contains(&cur.surface.as_str()) {
        return 0;
    }
    let prev_katakana = prev.surface.chars().count() >= 2
        && prev.surface.chars().all(|c| {
            ('\u{30A1}'..='\u{30FA}').contains(&c)
                || c == 'ー'
                || ('\u{30FC}'..='\u{30FF}').contains(&c)
        });
    if !prev_katakana {
        return 0;
    }
    2000
}

/// 1文字漢字どうしの連接コストの下限（これより安くはしない）
const SINGLE_KANJI_PAIR_CONN_FLOOR: i32 = 300;

/// 学習ボーナスの乗った1文字漢字の形容詞語幹からの接続コストの下限。
/// 語幹自身のユニグラムボーナスで単語コストがほぼ0（上限 UNIGRAM_BONUS_CAP
/// =6000）まで下がり得るため、SINGLE_KANJI_PAIR_CONN_FLOORより大きい値にする。
const ADJECTIVE_STEM_CONN_FLOOR: i32 = 6000;

/// 1文字漢字が絡む不自然な接続コストに下限を設ける（例: 「まんなか」→
/// 「万中」のような無意味な変換を防ぐ）。学習バイグラムのボーナスも加味
/// した最終的な接続コストに適用する（下限を辞書由来のコストだけに掛けても、
/// 学習バイグラムの減額分でまた安くなり過ぎてしまうため）。
///
/// IPA辞書の連接コストは品詞IDの組だけで決まり、個々の単語の相性までは
/// 見ない。そのため「万」「中」のように単独では極めて頻出な1文字漢字が
/// たまたま安い品詞IDの組み合わせ（数十〜数千のコスト差）で隣接すると、
/// 学習でさらにコストが下がった状態と合わさって、正しい一語（真ん中 等）
/// より安く見えてしまうことがある。
///
/// ただし「英語」（英+語）のように、1文字漢字どうしの複合語で連接コストが
/// 大きく負なのは、学習と無関係にIPA辞書自体がその組み合わせを正しく
/// 高頻度と認識している場合も多い。そういう場合まで一律にフロアを掛けると
/// 逆に正しい変換を壊してしまう。危険なのは「学習ボーナスによって
/// どちらかの単語コストが不自然に下がっている」場合に限られるため、
/// `prev`/`cur` のどちらかに学習ボーナスが乗っているときだけ適用する。
pub(crate) fn clamp_single_kanji_pair_conn_cost(
    prev: Option<&WordEntry>,
    cur: Option<&WordEntry>,
    prev_bonus: i32,
    cur_bonus: i32,
    conn_cost: i32,
) -> i32 {
    if prev_bonus <= 0 && cur_bonus <= 0 {
        return conn_cost;
    }
    let prev_single = prev.is_some_and(|e| is_single_kanji_surface(&e.surface));
    let cur_single = cur.is_some_and(|e| is_single_kanji_surface(&e.surface));
    if prev_single && cur_single {
        return conn_cost.max(SINGLE_KANJI_PAIR_CONN_FLOOR);
    }
    conn_cost
}

/// 「1文字漢字＋1文字漢字の非自立名詞」で文が終わる接続コストに下限を
/// 設ける（例: 「まんなか」→「万中」で終わるのを防ぐ）。
///
/// 「方（ほう）」「他（ほか）」等、非自立名詞は単独の変換結果としても
/// 極めて頻出なので、非自立名詞というだけで文末接続を一律に不利にすると
/// それらを壊してしまう。危険なのは「直前もまた1文字漢字だった」場合
/// （＝1文字漢字どうしが連続してそのまま文が終わる）に限られるため、
/// 非自立名詞の語だけでなく、その前の語（祖先ノード）も1文字漢字かを
/// 確認してから適用する。さらに `clamp_single_kanji_pair_conn_cost` と
/// 同様、学習ボーナスが絡む場合だけに限定する（IPA辞書自体が正当に
/// 認識している組み合わせまで壊さないため）。
pub(crate) fn single_kanji_bound_noun_phrase_end_conn_cost(
    prev: Option<&WordEntry>,
    grandparent: Option<&WordEntry>,
    prev_bonus: i32,
    grandparent_bonus: i32,
    conn_cost: i32,
) -> i32 {
    if prev_bonus <= 0 && grandparent_bonus <= 0 {
        return conn_cost;
    }
    let Some(p) = prev else { return conn_cost };
    if !p.pos.contains("非自立") || !is_single_kanji_surface(&p.surface) {
        return conn_cost;
    }
    // 祖先ノードが存在する（＝文頭からその1語だけの単独変換ではない）
    // ことだけ確認する。祖先が1文字漢字でなくても（例:「お」+「中」）
    // 危険な組み合わせになり得るため、1文字漢字限定にはしない。
    if grandparent.is_none() {
        return conn_cost;
    }
    conn_cost.max(SINGLE_KANJI_PAIR_CONN_FLOOR)
}

/// 学習ボーナスの乗った「1文字漢字の非自立名詞／副詞可能名詞」が、助詞等を
/// 挟まず直接別の内容語（名詞・動詞語幹 等）に続く接続コストに下限を設ける
/// （例: 「さいきどう」→「際」+「起動」で「再起動」が押しのけられるのを
/// 防ぐ）。
///
/// 非自立名詞（「方」「他」等）や副詞可能名詞（「際」等）は文法上、助詞や
/// 活用語尾を伴って使われるのが基本で、別の内容語にそのまま連結して複合語を
/// 作ることはほとんど無い（「際に」「際は」は自然だが「際起動」は不自然）。
/// 「際」はIPA辞書上に「非自立」と「副詞可能」2つのPOS変種があり、どちらも
/// 同じ学習ボーナスを受け取るため両方を対象にする（片方だけ塞ぐと、
/// もう片方の変種経由で同じ問題が再発するため）。直後が内容語
/// （`is_content_pos`）の場合だけに絞れば、助詞への接続（「際に」等の
/// 正当な用法）は対象外のまま残せる。学習ボーナスが絡む場合のみに
/// 限定するのは他の1文字漢字系ペナルティと同様の理由。
pub(crate) fn single_kanji_bound_noun_then_content_word_conn_cost(
    prev: Option<&WordEntry>,
    cur: Option<&WordEntry>,
    prev_bonus: i32,
    conn_cost: i32,
) -> i32 {
    if prev_bonus <= 0 {
        return conn_cost;
    }
    let Some(p) = prev else { return conn_cost };
    let is_bound_like = p.pos.contains("非自立") || p.pos.contains("副詞可能");
    if !is_bound_like || !is_single_kanji_surface(&p.surface) {
        return conn_cost;
    }
    let Some(c) = cur else { return conn_cost };
    if !is_content_pos(&c.pos) {
        return conn_cost;
    }
    conn_cost.max(SINGLE_KANJI_PAIR_CONN_FLOOR)
}

/// 学習ボーナスの乗った1文字漢字の語幹・固有名詞表記（読みが2文字以上
/// ある、複数の音を1文字の漢字に圧縮する読み方。例:「おお」→形容詞語幹
/// 「多」、「おお」→固有名詞の地名表記「多」等）が、次に何が続く場合
/// でも接続コストに下限を設ける（例:「おおさか」→「おお」を「多」と
/// 学習しているせいで「多」+「さか」に分割され、「大阪」が押しのけ
/// られるのを防ぐ）。
///
/// この手の1文字漢字は、同じ読み+表記の組が複数の品詞バリエーション
/// （形容詞語幹・固有名詞の地名表記 等）で辞書に登録されていることが
/// あり、どのバリエーション経由でも同じ学習ユニグラムボーナスを受け
/// 取ってしまう。ただし「非自立」「副詞可能」（際・方・他 等の
/// 単独でも頻出な語）は `single_kanji_bound_noun_then_content_word_conn_cost`
/// が既により小さい下限で個別に対応済みのため対象外にする（対象に含めると
/// そちらの正しい既定動作を上書きして壊れることを実測で確認済み）。
/// 学習ユニグラムでこの語が安くなっている場合のみ下限を設け、学習と
/// 無関係な辞書本来の複合語形成には影響しない
/// （他の1文字漢字系ペナルティと同じ考え方）。
pub(crate) fn bonused_adjective_stem_then_content_word_conn_cost(
    prev: Option<&WordEntry>,
    prev_bonus: i32,
    conn_cost: i32,
) -> i32 {
    if prev_bonus <= 0 {
        return conn_cost;
    }
    let Some(p) = prev else { return conn_cost };
    let is_targeted_pos = (p.pos.starts_with("形容詞") && !p.surface.ends_with('い'))
        || p.pos.starts_with("名詞-固有名詞");
    if p.reading.chars().count() < 2 || !is_single_kanji_surface(&p.surface) || !is_targeted_pos {
        return conn_cost;
    }
    // この語自身のユニグラムボーナスで単語コストがほぼ0まで下がり得る
    // （上限 UNIGRAM_BONUS_CAP=6000）ため、下限も同程度の規模にしないと
    // 抑止しきれない（次の語自身が別の学習ボーナスを受けている場合も
    // 含めて、実測で SINGLE_KANJI_PAIR_CONN_FLOOR=300 では不十分だった）。
    conn_cost.max(ADJECTIVE_STEM_CONN_FLOOR)
}

/// フィラー（「え」「あの」等の感動詞的な断片）の直後に学習ボーナスの乗った
/// 語が続く接続コストに下限を設ける（例: 「えいご」→「え」+「以後」で
/// 「英語」が押しのけられるのを防ぐ）。
///
/// フィラーは文頭に来やすいため IPA辞書上は文頭接続が安く、そこに無関係な
/// 文脈で学習した語（「以後」等）がたまたま後続すると、正しい複合語
/// （英語 等）より安く見えてしまうことがある。フィラーは定義上「あの」
/// 「えっと」等の感動詞的な断片で、実在の複合語の構成要素になることは
/// ほぼ無いため、フィラーが絡む接続だけに絞れば正当な変換を壊す心配は
/// 小さい。
pub(crate) fn filler_then_bonused_word_conn_cost(
    prev: Option<&WordEntry>,
    cur_bonus: i32,
    conn_cost: i32,
) -> i32 {
    if cur_bonus <= 0 {
        return conn_cost;
    }
    if !prev.is_some_and(|e| e.pos.starts_with("フィラー")) {
        return conn_cost;
    }
    conn_cost.max(SINGLE_KANJI_PAIR_CONN_FLOOR)
}

/// 学習（ユニグラム・バイグラム）が乗った活用語（形容詞連用形 等）が、
/// 極端に安い活用接続（〜くない 等）を通じて無関係な同音異義語（別の1語）
/// を押しのけてしまうのを防ぐ（例:「すくない」→「酸くない」で「少ない」
/// が押しのけられるのを防ぐ）。
///
/// 「形容詞連用形+ない」のような活用接続はIPA辞書上非常に安い
/// （-1万前後）。これは正しい言語的頻度（高くない・安くない 等は極めて
/// 高頻度）を反映しており、学習が無ければ壊してはいけない。しかし
/// 学習（その形容詞自体のユニグラム、またはその活用接続のバイグラム）が
/// 乗った稀な形容詞（「酸い」等）がこの極端な安さと組み合わさると、
/// 対象語（少ない 等）より安く見えてしまうことがある。学習が絡んでいる
/// 場合に限り、接続コストが極端に安い（下限未満）ケースだけ下限を設ける。
pub(crate) fn bonused_adjective_inflection_conn_floor(
    prev: Option<&WordEntry>,
    cur: Option<&WordEntry>,
    prev_unigram_bonus: i32,
    bigram_bonus: i32,
    conn_cost: i32,
) -> i32 {
    const FLOOR: i32 = -6000;
    if prev_unigram_bonus <= 0 && bigram_bonus <= 0 {
        return conn_cost;
    }
    if conn_cost >= FLOOR {
        return conn_cost;
    }
    let Some(p) = prev else { return conn_cost };
    let Some(c) = cur else { return conn_cost };
    if !p.pos.starts_with("形容詞") || !c.pos.starts_with("助動詞") {
        return conn_cost;
    }
    FLOOR
}

/// 学習（ユニグラム・バイグラム）が乗った語が、接頭詞（お・ご 等）に
/// 直接続く接続コストに下限を設ける（例:「おなか」→「お」+「中」で
/// 「お腹」が押しのけられるのを防ぐ）。
///
/// 「接頭詞+名詞」（お花・お茶・ご飯 等）の接続はIPA辞書上非常に安い。
/// これは正しい言語的頻度を反映しており、学習が無ければ壊してはいけない
/// （このため `symbol_penalty` 等と違い品詞だけで一律には弾かない）。
/// しかし無関係な文脈で強く学習した語（「中」等）がこの接続の後に来ると、
/// 対象語（お腹 等）より安く見えてしまうことがある。学習が絡んでいる
/// 場合に限り、接続コストが極端に安い（下限未満）ケースだけ下限を設ける。
pub(crate) fn bonused_word_after_prefix_conn_floor(
    prev: Option<&WordEntry>,
    cur_unigram_bonus: i32,
    bigram_bonus: i32,
    conn_cost: i32,
) -> i32 {
    const FLOOR: i32 = 0;
    if cur_unigram_bonus <= 0 && bigram_bonus <= 0 {
        return conn_cost;
    }
    if conn_cost >= FLOOR {
        return conn_cost;
    }
    if !prev.is_some_and(|p| p.pos.starts_with("接頭詞")) {
        return conn_cost;
    }
    FLOOR
}

/// span が全てひらがな（または長音符「ー」）か判定
///
/// 「らーめん」のような長音符入りの外来語表記を一括でカタカナ化
/// できるよう、長音符も許容する。
pub(crate) fn is_all_hiragana(chars: &[char]) -> bool {
    chars.iter().all(|&c| ('\u{3041}'..='\u{3096}').contains(&c) || c == 'ー')
}
