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
    ("おなか", "お腹"), ("だれ", "誰"), ("くるま", "車"),
    // 動詞の音便形。IPA辞書は「いっ」の読みに対し稀な同音動詞（逝っ）を
    // 頻出動詞（行っ）より低コストで持っており、「〜に行った/行って」が
    // 「〜に逝った/逝って」に化ける（他の活用形=行く・行きます・行けば等は
    // 別の辞書行のため影響されない）。「いっ」自体は後続語との組み合わせで
    // 必要なボーナス幅が変わるため、`seed_common_words`側で個別に強めの
    // 値を設定している（このプリセットには含めない）。
    ("よん", "読ん"),
    // 単独漢字語だが、1文字漢字＋束縛形態素への断片化（本→穂+ん、
    // 線→畝+ん）を fragment_repair.rs が辞書語に置換する際、断片化前の
    // 語自体がここでシード済みであることを「安全に置換してよい語」の
    // 判定に使う（未シードの希少な当て字と区別するため）。
    ("ほん", "本"), ("せん", "線"),
    // 長い動詞は辞書コストが高く、安いカタカナ断片(カン 等)+助詞の分割に
    // 負けやすい。よく使う動詞を優先しておく。
    ("かんがえる", "考える"), ("かんがえた", "考えた"),
];

/// 頻出語プリセットのボーナス（moderate。実学習で上書きされる）
pub(crate) const COMMON_WORD_SEED_BONUS: i32 = 1500;

/// 自動算出ボーナスの安全マージン（コスト差ちょうどではなく少し上乗せする）。
pub const SEEDED_ASSOC_MARGIN: i32 = 300;

/// 自動算出ボーナスの上限。これを超える語（＝辞書上の生コスト差が元々
/// 大きい＝一般的には競合表記の方が正しい可能性が高い）は、シードで
/// 無理に押し退けるとリスクが大きいため採用しない
/// （実例: 「いし」で「意志」「石」を「医師」に勝たせるには2700〜3300
/// 必要だったが、医師が圧倒的に一般的なため不採用と判断した）。
pub const SEEDED_ASSOC_MAX_BONUS: i32 = 2500;

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

pub fn is_implausible_content_word(e: &WordEntry) -> bool {
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

/// コーパスのユニグラム学習ボーナスの上限（`UNIGRAM_BONUS_CAP`と同規模。
/// 同じ`.max(0)` floorを共有する既存の統合ポイントにそのまま乗せるため）
pub(crate) const CORPUS_UNIGRAM_BONUS_CAP: i32 = UNIGRAM_BONUS_CAP;
/// コーパスのバイグラム学習ボーナスの上限（`BIGRAM_BONUS_CAP`と同規模）
pub(crate) const CORPUS_BIGRAM_BONUS_CAP: i32 = BIGRAM_BONUS_CAP;

/// コーパスの出現回数をコスト減額（ボーナス）に変換する
///
/// `frequency_to_bonus`（1回=1500、個人の変換履歴向け）をそのまま使うと、
/// コーパスの出現回数は数十〜数万に達するため大半が即座に上限に張り付き、
/// 語ごとの相対的な頻度差（＝どちらがより自然か）という肝心の情報が
/// 失われる。対数スケールにすることで、頻度が低い語同士は差が付き、
/// 極端に頻度が高い語だけが上限で頭打ちになるようにする。
fn corpus_frequency_to_bonus(freq: u32, cap: i32) -> i32 {
    if freq == 0 {
        return 0;
    }
    (((freq as f64).ln() * 1500.0).round() as i32).min(cap)
}

pub fn corpus_unigram_bonus(freq: u32) -> i32 {
    corpus_frequency_to_bonus(freq, CORPUS_UNIGRAM_BONUS_CAP)
}

pub fn corpus_bigram_bonus(freq: u32) -> i32 {
    corpus_frequency_to_bonus(freq, CORPUS_BIGRAM_BONUS_CAP)
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

/// 文末に単独の内容語コスト（下限）を科す、助詞と紛らわしい1文字の
/// 内容語コスト下限（文末接続に限定した助詞紛らわしい語のペナルティ）。
///
/// 「わたしはに」→「私は二」のように、末尾の「に」が助詞ではなく数詞
/// 「二」(cost 2914 < 助詞「に」cost 4304) に化けてしまうことがある。
/// `is_lone_particle`は入力全体がちょうど1助詞のときしか救えない
/// （「わたしはに」は全体が1助詞ではないため対象外）。かといって
/// 「に」自身の単語コストを下げて助詞を優先すると、「にげた→逃げた」
/// のように「に」で始まる別の正当な語の頭を奪ってしまう（実測で確認
/// 済み）。文末（EOS直前）でだけ、助詞と読みが同じ内容語（二・荷・煮 等）
/// にペナルティを科せば、文中の語形成には影響せず、この文末限定の
/// 誤変換だけを防げる。
pub(crate) fn content_word_particle_reading_before_eos_penalty(
    prev: Option<&WordEntry>,
    is_eos: bool,
) -> i32 {
    if !is_eos {
        return 0;
    }
    let Some(p) = prev else { return 0 };
    if p.pos.starts_with("助詞") {
        return 0;
    }
    if p.reading.chars().count() != 1 || !is_lone_particle(&p.reading) {
        return 0;
    }
    3000
}

/// 記号（＆＠×÷ 等）表記へのコストペナルティ
///
/// IPA辞書は「と」→「＆」「かける」→「×」のように、かな読みに記号を
/// 低コスト（「×」はcost=-279と負！）で割り当てていることがある。
/// かなを記号へ変換するのはほぼ誤りなので、記号品詞かつ非日本語表記の
/// 語に強いペナルティを付ける。句読点（、。「」等）はCJK記号域なので
/// 対象外。
///
/// 以前はASCII/全角英数記号とラテン文字だけを対象にしていたが、「×」
/// （U+00D7、Latin-1 Supplement）のようなそのどちらの範囲にも入らない
/// 記号を見落としていた（実測:「でんわをかける」→「電話を×」）。
/// 「日本語表記（ひらがな/カタカナ/漢字）か」で判定を反転し、
/// CJK句読点だけを明示的に除外することで、記号品詞の非日本語表記を
/// 網羅的に拾う。
pub(crate) fn symbol_penalty(surface: &str, pos: &str) -> i32 {
    if !pos.starts_with("記号") {
        return 0;
    }
    let is_cjk_punctuation =
        !surface.is_empty() && surface.chars().all(|c| ('\u{3000}'..='\u{303F}').contains(&c));
    if is_cjk_punctuation {
        return 0;
    }
    let is_japanese_script = !surface.is_empty()
        && surface.chars().all(|c| {
            ('あ'..='ん').contains(&c) || ('ァ'..='ヺ').contains(&c) || ('一'..='龯').contains(&c)
        });
    if is_japanese_script {
        0
    } else {
        5000
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
///
/// 固定加算(+5000)では実辞書のコスト差（形容詞と動詞連用形の語彙コスト差
/// ＋接続コスト差、実測で4335超）を相殺しきれない場合があったため、他の
/// `*_conn_floor` 系ガードと同様の下限（floor）方式にする。
pub(crate) fn adjective_terminal_then_te_penalty(
    prev: &WordEntry,
    cur: &WordEntry,
    conn_cost: i32,
) -> i32 {
    const FLOOR: i32 = 9000;
    if !prev.pos.starts_with("形容詞") || !prev.surface.ends_with('い') {
        return conn_cost;
    }
    if cur.surface != "て" || !cur.pos.starts_with("助詞") {
        return conn_cost;
    }
    conn_cost.max(FLOOR)
}

/// イ形容詞の基本形に格助詞・副詞化の「に」を直接つなげて名詞を
/// 分断しない（淡い＋匂い → 淡い＋に＋追い）。禁止ではなくコスト下限。
pub(super) fn adjective_terminal_then_ni_penalty(prev: &WordEntry, cur: &WordEntry, cost: i32) -> i32 {
    if prev.pos.starts_with("形容詞") && prev.reading.ends_with('い')
        && cur.surface == "に"
        && cur.pos.starts_with("助詞")
    {
        cost.max(9000)
    } else {
        cost
    }
}

/// 1文字漢字の表記かつ読みが単独助詞と一致する語（野・葉・尾 等）が、
/// 文頭以外の位置に出現することへのコストペナルティ（例:「よこのみち」→
/// 「横」+「野」+「三智」で「横」+「の」+「みち」が押しのけられるのを防ぐ）。
///
/// 「の」「は」等の単独助詞と同じ読みを持つ1文字漢字の表記（野・葉・羽 等）
/// が辞書に実在し、直前の語との接続コストの都合で、正しい助詞としての
/// 分割より安く見えてしまうことがある。この呼び出しは `find_best_path` の
/// 無条件ガード節（`prev_node.entry` が `Some` の場合のみ成立する節）内で
/// 行うため、`cur` が文頭語のケースは構造的に対象外になる（大阪等の
/// 複合表記固有名詞は2文字以上のため `is_single_kanji_surface` で
/// そもそも対象外）。
pub(crate) fn single_kanji_lone_particle_reading_penalty(cur: &WordEntry) -> i32 {
    const PENALTY: i32 = 2500;
    if is_single_kanji_surface(&cur.surface) && is_lone_particle(&cur.reading) {
        PENALTY
    } else {
        0
    }
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
pub(crate) fn is_single_kanji_surface(surface: &str) -> bool {
    let mut chars = surface.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return false;
    };
    ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c)
}

/// 表記がちょうど1文字のカタカナか（`is_single_kanji_surface`のカタカナ版）
pub(crate) fn is_single_katakana_surface(surface: &str) -> bool {
    let mut chars = surface.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return false;
    };
    ('\u{30A1}'..='\u{30FA}').contains(&c) || c == 'ー'
}

/// 束縛モーラ「ん」（名詞-非自立、読み1文字。ひらがな/カタカナ両表記）が
/// 助詞に続く接続コストは、「言うんです」「したんだ」等の正当な用法では
/// 実測で大きく負（-5000超）と、統計的にきわめて有利。この有利さは、
/// 「ん」の直前が単独の1文字漢字/カタカナ（動詞・形容詞ではなく音の
/// 断片）であるときには、「本(ほん)」のような2モーラ語が「ホ+ン」に
/// 割れて助詞へ安く逃げる抜け道になってしまう（実測: 本を読んだ→
/// ホンを読んだ）。学習ボーナスの有無を問わず base 辞書だけで起きるため、
/// 他の`bonused_*`系ガードとは異なり無条件で適用する。祖先ノード
/// （「ん」の前の語）が単独の1文字漢字/カタカナのときだけ下限を設けるので、
/// 正当な動詞・形容詞からの「〜んです/んだ」は対象外のまま残る。
pub(crate) fn single_char_bound_mora_then_particle_conn_floor(
    grandparent: Option<&WordEntry>,
    prev: &WordEntry,
    cur: &WordEntry,
    conn_cost: i32,
) -> i32 {
    const FLOOR: i32 = 0;
    // 「ん」は複数のPOSバリアントで辞書に登録されており（名詞-非自立
    // 「〜んです」、助動詞「行かん」等）、どちらも後続の助詞への接続
    // コストが有利。1つのPOSだけ塞いでも別バリアント経由ですり抜ける
    // （実測: 名詞-非自立を塞いだ後も助動詞バリアント経由で「線」が
    // 「せ」+「ん」に割れ続けた）ため両方を対象にするが、表記は「ん/ン」
    // に限定する（「た」等の他の1文字助動詞は「見た|が」のような正当な
    // 「動詞+た+助詞」を無数に含むため対象にできない）。
    let is_bound_mora_pos =
        prev.pos.starts_with("名詞-非自立") || prev.pos.starts_with("助動詞");
    if !(is_bound_mora_pos && (prev.surface == "ん" || prev.surface == "ン")) {
        return conn_cost;
    }
    if !cur.pos.starts_with("助詞") {
        return conn_cost;
    }
    let Some(gp) = grandparent else { return conn_cost };
    // 祖先ノードは読みが1モーラの断片（表記の種類は問わない: 単独漢字・
    // カタカナに加え、ひらがなの音そのまま（例:「せ」）も対象。単語全体の
    // 1〜2モーラを1文字漢字/カタカナ表記で奪われた残りが「ん」に化ける
    // のと同型で、表記の種類で区別する理由が無い）。
    if gp.reading.chars().count() != 1 {
        return conn_cost;
    }
    conn_cost.max(FLOOR)
}

/// 「いっ」→「行っ」（行くの音便形）を、稀な同音動詞「逝っ」に対して
/// 確実に勝たせるための下限。元は`trusted_phrase_bonus`で読み・後続語を
/// 一切問わず`word_cost`を無条件に-15000相当まで割り引いていたが、これは
/// 「いっ」で始まる無関係な語（一貫性→行っ完成、実測cost比較で正しい
/// 分割が4600以上安いのに逆転していた）まで巻き込む「モグラ叩き」の
/// 典型事故だった（2026-09-13発見）。
///
/// 本来の導入意図（コメント参照）は「いってらっしゃい/いってきます/
/// いってしまった/行った」のように**直後が「て」「た」の音便接続の場合
/// だけ**確実に勝たせたい、というもの。ここでは意図をそのままedge単位の
/// 条件（`prev`が「いっ/行っ」かつ`cur`の表記が「て」「た」）として実装し、
/// 該当しない後続語（「性」等）には一切効かないようにする。
pub(crate) fn iitte_verb_conn_floor(
    prev: Option<&WordEntry>,
    cur: Option<&WordEntry>,
    conn_cost: i32,
) -> i32 {
    const BONUS: i32 = 15000;
    let Some(prev) = prev else { return conn_cost };
    if prev.reading != "いっ" || prev.surface != "行っ" {
        return conn_cost;
    }
    let Some(cur) = cur else { return conn_cost };
    if cur.surface != "て" && cur.surface != "た" {
        return conn_cost;
    }
    conn_cost.saturating_sub(BONUS)
}

/// `katakana_kanji_suffix_penalty`のうち`cur`（現在ノード）だけで決まる判定。
/// 辺（prevとの組）に関わらず同じ結果になるため、`find_best_path`の内側
/// ループ（prevについてのループ）の外でnode_idxにつき1回だけ呼び、
/// 結果を使い回す（同じ判定を辺の数だけ繰り返さないため）。
pub(crate) fn is_unallowed_single_kanji_suffix(cur: &WordEntry) -> bool {
    if !cur.pos.contains("接尾") {
        return false;
    }
    // 対象は1文字漢字の接尾辞のみ。語・人・製 等の代表的な接尾辞の多くは
    // 1文字であり、下の例外リストで個別に救済する。
    if !is_single_kanji_surface(&cur.surface) {
        return false;
    }
    const ALLOWED_SUFFIXES: &[&str] =
        &["語", "人", "製", "風", "式", "系", "産", "型", "教", "街", "圏", "流", "調"];
    !ALLOWED_SUFFIXES.contains(&cur.surface.as_str())
}

pub(crate) fn prev_is_katakana_word(prev: &WordEntry) -> bool {
    prev.surface.chars().count() >= 2
        && prev.surface.chars().all(|c| {
            ('\u{30A1}'..='\u{30FA}').contains(&c)
                || c == 'ー'
                || ('\u{30FC}'..='\u{30FF}').contains(&c)
        })
}

pub(crate) fn katakana_kanji_suffix_penalty(prev: &WordEntry, cur: &WordEntry) -> i32 {
    if !is_unallowed_single_kanji_suffix(cur) || !prev_is_katakana_word(prev) {
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

/// **文頭の、読みが2文字以下の短い語**の直後への接続に、学習ボーナスが
/// 絡んでいる場合の下限を設ける（もぐら叩きの根本対策として一般化した
/// ガード。2026-08-06）。
///
/// このセッションだけで、以下のように「短い機能語寄りの語（接頭詞・副詞・
/// 短い動詞/形容詞語幹・名詞）が無関係な文脈で単語コスト・バイグラム
/// ボーナスを積み上げ、同じ読みのアトミックな1語を押しのける」という
/// 全く同型のバグが繰り返し見つかった:
/// - 「さいてきか」→「再」(接頭詞,2文字)+「摘果」で「最適化」が負ける
/// - 「ぜんしゃ」→「前」(接頭詞,2文字)+カタカナで「前者」が負ける
/// - 「いちがい」→「位置」(名詞,2文字)+「が」+「胃」で「一概」が負ける
/// - 「どうか」→「どう」(副詞,2文字)+「か」で「同化」が負ける
/// - 「とおして」→「賭」(接頭詞的な当て字,1文字="と")+「押して」で
///   「通して」が負ける
/// - 「へいがい」→「へ」(助詞,1文字)+「以外」で「弊害」が負ける
///
/// 都度そのケースのPOS（接頭詞・副詞 等）に限定したガードを追加していたが、
/// 際限なく新しいPOSの組み合わせで再発した（もぐら叩き）。実測した全事例で
/// 共通していたのは「原因となる prev の読みが1〜2文字」という点で、逆に
/// 正当に守るべき文頭語（検索・問題・時間・場所・今日・仕事 等）は
/// いずれも読み3文字以上だった。そこでPOSによる限定をやめ、
/// 「読みが2文字以下」という語彙非依存の条件に一般化した。
///
/// 文中（文頭ではない）の同じ組み合わせは対象外にする。「行けるか
/// どうか」「〜かどうか」のような頻出構文まで壊すことが実測で確認
/// 済みのため（祖先ノードが無い＝この語がその読み全体の先頭に来ている
/// 場合のみ対象）。
pub(crate) fn bonused_short_prev_conn_floor_at_utterance_start(
    prev: Option<&WordEntry>,
    cur: Option<&WordEntry>,
    prev_is_utterance_start: bool,
    prev_unigram_bonus: i32,
    cur_unigram_bonus: i32,
    bigram_bonus: i32,
    prev_has_untrusted_pos_homograph: bool,
    conn_cost: i32,
) -> i32 {
    const FLOOR: i32 = 0;
    if !prev_is_utterance_start
        || (prev_unigram_bonus <= 0 && cur_unigram_bonus <= 0 && bigram_bonus <= 0)
    {
        return conn_cost;
    }
    // curがEOS（文末）なら、prevは「文頭から始まる短い語1つだけの発話」
    // そのものであり、断片化の余地が無い（後続の語と組んで誤った複合語を
    // 作りようがない）。この場合まで下限を掛けると、正しく優先したい
    // 短い語（例:「前」を学習ボーナス付きで優先している時に単独で
    // 「まえ」とだけ打った場合）が、逆に不当なペナルティを受けて
    // フィラー等の断片解釈に負けてしまう（実測: まえ→ま+え）。
    if cur.is_none() {
        return conn_cost;
    }
    let Some(p) = prev else { return conn_cost };
    // 連体詞（この・その・あの・どの・わが 等）は定義上つねに後続の語を
    // 伴う語で、単独で使われることも、他の断片と組み合わさって置き換え
    // られることもない。このガードが想定する「短い語が断片解釈に
    // 押しのけられる」競合が原理的に起こり得ないため対象外にする。
    // 除外しないと、後続語（cur）が無関係な文脈で学習ユニグラムボーナスを
    // 持つだけで、この→空のような辞書本来は有利な接続まで不当にfloorされ、
    // 結果として「この」自体が「こ」+「の」に分割されてしまう
    // （実測:「このそらをみあげて」→「股の空を見上げて」。学習バイグラム
    // 「の→空」を修正した[[bigram-atomic-word-split-exploit-fix]]とは別の
    // 原因で同じ症状が再発したケース。ここはcurの学習ユニグラムボーナス
    // だけで発生し、バイグラムは無関係）。
    if p.pos.starts_with("連体詞") {
        return conn_cost;
    }
    // prevと同じ読み・同じ接続クラス（left_id/right_id一致）だが学習
    // ボーナスの乗っていない別表記（多くはひらがな）が辞書に存在するなら
    // 対象外にする。conn_costへの下限は表記に関係なく「その接続クラス」
    // 全体に効くわけではなく、あくまでprev（ボーナス付きの表記）のノード
    // にだけ乗る。つまりこの下限は「学習ボーナスで有利になった表記」を
    // 狙い撃ちで不利にし、無関係な無学習の同表記に道を譲るだけになって
    // しまう。これは「他」(ほか)を学習で優先していても「他の/他が/他に」
    // で無学習の「ほか」（同じ読み・同じPOS、辞書上ずっと安い）に負ける
    // 形で実測した（他を/単独 は正しく勝てる＝この下限が掛からない
    // エッジでは問題ない）。「位置→が」「意味→の」のような本来の不正な
    // 断片化ケースでは、同じ読みに同じ接続クラスの無学習な別表記が
    // 存在しないため、この条件では誤って除外されない。
    if prev_has_untrusted_pos_homograph {
        return conn_cost;
    }
    if p.reading.chars().count() > 2 {
        return conn_cost;
    }
    // prev側だけにボーナスがあり、curには無い場合（「いちがい」→「位置」
    // +「が」、「いみのある」→「意味」+「の」等）に限り、辞書本来の接続
    // コスト（学習バイグラムのボーナスを差し引く前の値）が既に十分安い
    // ときだけ介入する。「いちがい」では位置→がの接続コストが学習
    // バイグラム抜きでも元から大きく負（実測約-4600）で、断片解釈を
    // 不当に有利にしていた。一方「いみのある」→「意味」+「の」は辞書
    // 本来の接続コストはふつうの正の値（実測+259）で、`conn_cost`が負に
    // 見えるのは「意味→の」という正当なバイグラムを日常的によく使って
    // いる学習ボーナス（実測+2500）が差し引かれた結果に過ぎない。
    // `conn_cost`（バイグラム減算後）で判定すると、この正当な学習による
    // バイグラムボーナスと「いちがい」のような辞書本来の負コストを
    // 区別できず、「意味」のような日常語（「位置」と同じ品詞・文字数の
    // ため他の条件では区別できない）の自然な文の続きまで壊してしまう
    // （実測: いみのある→異ミのある）。
    // ただしcur側にボーナスがある場合（「とおして」→「賭」+「押して」で
    // 「通して」が負ける等）や、curが辞書に実在語が見つからず生成された
    // カタカナ・フォールバック（実例:「おしえる」→「押し」+「エル」で
    // 「教える」が負ける。prevの単語コストがボーナスでほぼ0になり、
    // 残りの読みに対応する実在語が辞書検索で見つからなかった/選ばれな
    // かった結果カタカナ化したもの。「本来なら実在語になるはずの場所が
    // カタカナで埋まっている」こと自体がその分割が不自然である強いシグナル
    // なので、辞書本来の接続コストの符号を問わず常にfloorする）の場合は、
    // この確認をしない。この系統の原因はcur側の要因（単語コストの割引、
    // または実在語が見つからないこと）であり、prevとcurの辞書本来の接続
    // コストの符号とは無関係（実測: 賭→押しの接続コストは+200と正で、
    // この確認を適用すると誤ってガードをすり抜けてしまう）。
    let cur_is_katakana_fallback = cur.is_some_and(|c| c.pos == "カタカナ");
    if prev_unigram_bonus > 0 && cur_unigram_bonus <= 0 && !cur_is_katakana_fallback {
        const SUSPICIOUS_CONN_THRESHOLD: i32 = 0;
        let raw_conn_cost = conn_cost.saturating_add(bigram_bonus);
        if raw_conn_cost >= SUSPICIOUS_CONN_THRESHOLD {
            return conn_cost;
        }
    }
    // prev または cur 自身に学習ユニグラムボーナスが乗っている場合は、
    // その語の単語コスト自体がほぼ0まで下がり得るため、接続コストの
    // 下限0だけでは打ち消せない（実例:「とおして」→「賭」+「押して」で
    // 「通して」が押しのけられる。ここは cur「押し」側に無関係な文脈
    // （「強く押した」等）からの学習ボーナスが乗っており、同じ活用型を
    // 共有する同音語「通し」との単語コスト差（実測 約7000）を接続コスト
    // の下限0では埋められなかった）。どちらかの単語コストが実質0まで
    // 下がっている場合は、`bonused_adjective_stem_then_content_word_conn_cost`
    // と同程度の下限まで引き上げる。どちらにもボーナスが乗っていない
    // （＝バイグラムだけの場合、または双方とも生コストのまま）場合は、
    // 正当な「接頭詞+内容語」複合語（お茶 等）を壊さないよう、
    // 従来どおり下限0のまま。
    let floor = if prev_unigram_bonus > 0 || cur_unigram_bonus > 0 {
        ADJECTIVE_STEM_CONN_FLOOR
    } else {
        FLOOR
    };
    conn_cost.max(floor)
}

/// span が全てひらがな（または長音符「ー」）か判定
///
/// 「らーめん」のような長音符入りの外来語表記を一括でカタカナ化
/// できるよう、長音符も許容する。
pub(crate) fn is_all_hiragana(chars: &[char]) -> bool {
    chars.iter().all(|&c| ('\u{3041}'..='\u{3096}').contains(&c) || c == 'ー')
}
