use super::*;


fn create_test_dictionary() -> Dictionary {
    let mut dict = Dictionary::new();
    
    // テスト用の単語を追加
    dict.add_word(WordEntry {
        surface: "今日".to_string(),
        reading: "きょう".to_string(),
        left_id: 1, right_id: 1, cost: 5000,
        pos: "名詞".to_string(),
    });
    
    dict.add_word(WordEntry {
        surface: "は".to_string(),
        reading: "は".to_string(),
        left_id: 2, right_id: 2, cost: 3000,
        pos: "助詞".to_string(),
    });
    
    dict.add_word(WordEntry {
        surface: "良い".to_string(),
        reading: "いい".to_string(),
        left_id: 3, right_id: 3, cost: 5500,
        pos: "形容詞".to_string(),
    });
    
    dict.add_word(WordEntry {
        surface: "天気".to_string(),
        reading: "てんき".to_string(),
        left_id: 1, right_id: 1, cost: 5200,
        pos: "名詞".to_string(),
    });
    
    dict.add_word(WordEntry {
        surface: "です".to_string(),
        reading: "です".to_string(),
        left_id: 4, right_id: 4, cost: 4000,
        pos: "助動詞".to_string(),
    });

    dict
}

#[test]
fn test_viterbi_conversion() {
    let dict = create_test_dictionary();
    let converter = ViterbiConverter::new(dict);
    
    // "きょうは" を変換
    let result = converter.convert_to_string("きょうは");
    assert!(result.contains("今日") || result.contains("は"));
}

#[test]
fn test_katakana_fallback_for_unknown() {
    // 辞書には「は」(助詞) のみ。「らすと」は未知語なのでカタカナ化されるはず。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.add_word(WordEntry {
        surface: "は".to_string(),
        reading: "は".to_string(),
        left_id: 4, right_id: 4, cost: 3000,
        pos: "助詞".to_string(),
    });

    let converter = ViterbiConverter::new(dict);
    let result = converter.convert_to_string("らすと");
    assert_eq!(result, "ラスト", "カタカナフォールバックが効いていない");
}

#[test]
fn test_learned_unigram_changes_live_conversion() {
    // 学習したユニグラムがライブ変換の1-bestを変える
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    // 「きしゃ」に 記者(低コスト) と 汽車(高コスト)
    dict.add_word(WordEntry {
        surface: "記者".to_string(), reading: "きしゃ".to_string(),
        left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "汽車".to_string(), reading: "きしゃ".to_string(),
        left_id: 1, right_id: 1, cost: 5500, pos: "名詞".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    // 学習前は 記者
    assert_eq!(converter.convert_to_string("きしゃ"), "記者");
    // 「きしゃ→汽車」を5回使ったことにする
    converter.learn_unigram("きしゃ", "汽車", 5);
    // 学習後は 汽車 が勝つ
    assert_eq!(converter.convert_to_string("きしゃ"), "汽車");
}

#[test]
fn test_context_assoc_disambiguates() {
    // 内容語連想により、助詞を挟んだ文脈で同音語を正しく選ぶ
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    // きしゃ = 記者/汽車（同コスト）、の = 助詞、しんぶん=新聞、えき=駅
    dict.add_word(WordEntry { surface: "記者".into(), reading: "きしゃ".into(), left_id: 1, right_id: 1, cost: 5000, pos: "名詞-一般".into() });
    dict.add_word(WordEntry { surface: "汽車".into(), reading: "きしゃ".into(), left_id: 1, right_id: 1, cost: 5000, pos: "名詞-一般".into() });
    dict.add_word(WordEntry { surface: "新聞".into(), reading: "しんぶん".into(), left_id: 1, right_id: 1, cost: 5000, pos: "名詞-一般".into() });
    dict.add_word(WordEntry { surface: "駅".into(), reading: "えき".into(), left_id: 1, right_id: 1, cost: 5000, pos: "名詞-一般".into() });
    dict.add_word(WordEntry { surface: "の".into(), reading: "の".into(), left_id: 2, right_id: 2, cost: 3000, pos: "助詞-連体化".into() });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false; // 連想の検証にカタカナ候補は不要

    // 学習: 新聞…記者 / 駅…汽車（助詞「の」を挟んだ内容語連想）
    converter.learn_assoc("新聞", "記者", 5);
    converter.learn_assoc("駅", "汽車", 5);

    // 文脈で選び分けられる
    assert!(converter.convert_context_aware_to_string("しんぶんのきしゃ").contains("記者"));
    assert!(converter.convert_context_aware_to_string("えきのきしゃ").contains("汽車"));
}

#[test]
fn test_learned_bigram_improves_consistency() {
    // バイグラム学習が語のつながりを優先する
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.add_word(WordEntry {
        surface: "貴社".to_string(), reading: "きしゃ".to_string(),
        left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "記者".to_string(), reading: "きしゃ".to_string(),
        left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "会見".to_string(), reading: "かいけん".to_string(),
        left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    // 「記者」の後に「会見」が来るつながりを学習
    converter.learn_bigram("記者", "会見", 5);
    // きしゃかいけん → 記者会見（貴社ではなく記者が選ばれる）
    let result = converter.convert_to_string("きしゃかいけん");
    assert!(result.contains("記者"), "bigram学習が効いていない: {}", result);
}

#[test]
fn test_wo_stays_particle_not_katakana() {
    // IPA辞書由来の「ヲ」(カタカナ,低コスト)より助詞「を」を優先する
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 500);
        }
    }
    dict.add_word(WordEntry {
        surface: "ヲ".into(), reading: "を".into(),
        left_id: 1, right_id: 1, cost: 3733, pos: "名詞-固有名詞-一般-*".into(),
    });
    dict.add_word(WordEntry {
        surface: "を".into(), reading: "を".into(),
        left_id: 4, right_id: 4, cost: 4183, pos: "助詞-格助詞-一般-*".into(),
    });
    let converter = ViterbiConverter::new(dict);
    assert_eq!(converter.convert_to_string("を"), "を");
}

#[test]
fn test_add_and_remove_user_word() {
    // 単語登録の土台: add_word で辞書に無い複合語を変換可能にし、
    // remove_word で元に戻せることを確認する（ライブ登録・削除の中核）。
    let mut dict = create_test_dictionary();
    // 登録前は「おもいでのしな」は1語として存在しない
    assert!(dict.lookup("おもいでのしな").is_none());
    dict.add_word(WordEntry {
        surface: "思い出の品".to_string(),
        reading: "おもいでのしな".to_string(),
        left_id: 1285, right_id: 1285, cost: 3800,
        pos: "名詞-一般-*-*".to_string(),
    });
    let conv = ViterbiConverter::new(dict);
    let s = conv.convert_to_string("おもいでのしな");
    assert!(s.contains("思い出の品"), "登録語が変換に出るべき: {}", s);

    // 削除すると辞書から消える（別辞書で remove_word 単体を検証）
    let mut dict2 = create_test_dictionary();
    dict2.add_word(WordEntry {
        surface: "思い出の品".to_string(),
        reading: "おもいでのしな".to_string(),
        left_id: 1285, right_id: 1285, cost: 3800,
        pos: "名詞-一般-*-*".to_string(),
    });
    assert!(dict2.lookup("おもいでのしな").is_some());
    assert!(dict2.remove_word("おもいでのしな", "思い出の品"));
    assert!(dict2.lookup("おもいでのしな").is_none());
}

#[test]
fn test_fuzzy_suggest() {
    // 実コスト規模（1文字あたり妥当性ゲート is_plausible_correction_cost）に
    // 合わせた実語コストの辞書を使う（共有 create_test_dictionary の
    // 今日=5000 は他テスト向けの相対比較用でゲートの想定より高すぎるため）。
    let mut dict = Dictionary::new();
    dict.add_word(WordEntry {
        surface: "今日".to_string(), reading: "きょう".to_string(),
        left_id: 1, right_id: 1, cost: 3000,
        pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "は".to_string(), reading: "は".to_string(),
        left_id: 2, right_id: 2, cost: 3000,
        pos: "助詞".to_string(),
    });
    let converter = ViterbiConverter::new(dict);
    // きよう(拗音の打ち間違い) → きょう → 今日 を提案
    let r = converter.fuzzy_suggest("きようは");
    assert!(r.is_some(), "誤字補正が出るべき");
    let (reading, surface) = r.unwrap();
    assert_eq!(reading, "きょうは");
    assert!(surface.contains("今日"), "補正後に今日を含むべき: {}", surface);
    // 正しく変換できる入力には出さない（自動変換成功＝もしかして不要）
    assert!(converter.fuzzy_suggest("きょうは").is_none());
}

#[test]
fn test_fuzzy_suggest_catches_implausible_content_word_split() {
    // 「せんせに」→「線」+「セ」+「に」のように、断片それぞれは辞書に
    // 実在する語として変換に"成功"してしまい、is_failed_segment（未知語/
    // カタカナ化）だけでは拾えない誤変換の回帰テスト。
    // 「う」「い」は単独では実在するが1文字あたりコストが極端に高い
    // （希少語）内容語とし、正しい語「あい」への1文字の変換違い
    // （うい→あい）を拾えるか検証する。
    let mut dict = Dictionary::new();
    dict.add_word(WordEntry {
        surface: "CWORD".to_string(), reading: "あい".to_string(),
        left_id: 1, right_id: 1, cost: 2000,
        pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "AWORD".to_string(), reading: "う".to_string(),
        left_id: 1, right_id: 1, cost: 9000,
        pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "BWORD".to_string(), reading: "い".to_string(),
        left_id: 1, right_id: 1, cost: 9000,
        pos: "名詞".to_string(),
    });
    let converter = ViterbiConverter::new(dict);
    // 「うい」は辞書上「AWORD」+「BWORD」に変換"成功"するが、どちらも
    // 実際にはほぼ使われない希少語なので、補正候補として拾われるべき。
    let r = converter.fuzzy_suggest("うい");
    assert!(r.is_some(), "希少語分割でも誤字補正が出るべき");
    let (reading, surface) = r.unwrap();
    assert_eq!(reading, "あい");
    assert_eq!(surface, "CWORD");
}

#[test]
fn test_fuzzy_suggest_rejects_implausible_high_cost_correction() {
    // 実在はするが1文字あたりコストが高すぎる（＝希少な当て字・造語）候補
    // しか無い場合、is_plausible_correction_cost のゲートで補正を出さない。
    // NEologd等で辞書が巨大化すると「変換に失敗せず実在語で構成される」
    // だけの判定では希少語まで通ってしまうため、その回帰防止。
    let mut dict = Dictionary::new();
    dict.add_word(WordEntry {
        surface: "希少語".to_string(), reading: "けごれ".to_string(),
        left_id: 1, right_id: 1, cost: 5000,
        pos: "名詞-固有名詞-一般-*".to_string(),
    });
    let converter = ViterbiConverter::new(dict);
    // 「こごれ」は辞書に無くカタカナ化される。「けごれ」は1文字違いで
    // 辞書に実在するが、1文字あたりコスト(約1667)がゲート(1300)を超えるため
    // 補正候補にしない。
    assert!(converter.fuzzy_suggest("こごれ").is_none());
}

#[test]
fn test_fuzzy_suggest_uses_right_context() {
    // 誤字「ぁかぃ」は1文字置換で AWORD("あかぃ") にも BWORD("ぁかい") にも
    // 補正できる（編集距離は同点）。文脈が無ければコストが低い BWORD が
    // 勝つが、直後に続けて入力済みの CWORD との学習済みバイグラムがあれば、
    // コストが高くても右文脈に合う AWORD が選ばれるべき＝誤字の直後に
    // 正しく打ち続けた内容から、前の誤字を直せることの検証。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 100);
        }
    }
    // is_plausible_correction_cost の1文字あたり上限に収まる実コスト規模にする
    dict.add_word(WordEntry { surface: "AWORD".into(), reading: "あかぃ".into(), left_id: 1, right_id: 1, cost: 2500, pos: "名詞-一般".into() });
    dict.add_word(WordEntry { surface: "BWORD".into(), reading: "ぁかい".into(), left_id: 1, right_id: 1, cost: 1500, pos: "名詞-一般".into() });
    dict.add_word(WordEntry { surface: "CWORD".into(), reading: "こご".into(), left_id: 1, right_id: 1, cost: 1000, pos: "名詞-一般".into() });
    let mut converter = ViterbiConverter::new(dict);
    converter.katakana_max_len = 3;

    // 文脈なし: コスト最安の BWORD が勝つ
    let before = converter.fuzzy_suggest("ぁかぃこご");
    assert!(before.is_some());
    let (_, surface_before) = before.unwrap();
    assert!(surface_before.starts_with("BWORD"), "文脈なしではコスト最安のBWORDが勝つはず: {}", surface_before);

    // AWORD の後に CWORD が続く学習
    converter.learn_bigram("AWORD", "CWORD", 5);

    // 文脈あり: コストが高くても右文脈（次のCWORD）に合う AWORD が勝つ
    let after = converter.fuzzy_suggest("ぁかぃこご");
    assert!(after.is_some());
    let (_, surface_after) = after.unwrap();
    assert!(surface_after.starts_with("AWORD"), "右文脈のバイグラムでAWORDが選ばれるべき: {}", surface_after);
}

#[test]
fn test_learned_hiragana_preference() {
    // Escで戻した読みを学習すると、その読みがひらがなで出るようになる
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    // 「こい」に漢字「恋」だけ登録（プリセット外の読みを使う）
    dict.add_word(WordEntry {
        surface: "恋".into(), reading: "こい".into(),
        left_id: 1, right_id: 1, cost: 4000, pos: "名詞-一般".into(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false; // カタカナ候補を除外して判定を明確に
    // 学習前は漢字
    assert_eq!(converter.convert_to_string("こい"), "恋");
    // ひらがな優先を学習
    converter.learn_hiragana("こい", 1);
    // 学習後はひらがな
    assert_eq!(converter.convert_to_string("こい"), "こい");
}

#[test]
fn test_common_word_seed_applied() {
    // 頻出語プリセットが空でなく、代表語が入っている
    let converter = ViterbiConverter::new(create_test_dictionary());
    assert!(converter.learned_unigram.contains_key(&("あう".to_string(), "会う".to_string())));
    // を の強優先も入っている
    assert_eq!(
        converter.learned_unigram.get(&("を".to_string(), "を".to_string())),
        Some(&8000)
    );
}

#[test]
fn test_load_word_priority_file_applies_bonus_with_default_and_override() {
    let path = std::env::temp_dir()
        .join(format!("ime_test_word_priority_{}.tsv", std::process::id()));
    std::fs::write(
        &path,
        "# コメント行\n\nはたち\t二十歳\t3000\nらいしゅう\t来週\n",
    )
    .unwrap();
    let mut converter = ViterbiConverter::new(create_test_dictionary());
    let count = converter.load_word_priority_file(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(count, 2);
    // コスト列を明示した行はその値が使われる
    assert_eq!(
        converter.learned_unigram.get(&("はたち".to_string(), "二十歳".to_string())),
        Some(&3000)
    );
    // コスト列省略時は COMMON_WORD_SEED_BONUS(1500) が既定値になる
    assert_eq!(
        converter.learned_unigram.get(&("らいしゅう".to_string(), "来週".to_string())),
        Some(&COMMON_WORD_SEED_BONUS)
    );
}

#[test]
fn test_load_word_priority_file_does_not_override_user_learning() {
    // ユーザーの実学習値が既にある場合はそれを優先する
    // （`.entry().or_insert()` の既定シードと同じ規約）。
    let path = std::env::temp_dir()
        .join(format!("ime_test_word_priority_no_override_{}.tsv", std::process::id()));
    std::fs::write(&path, "はたち\t二十歳\t3000\n").unwrap();
    let mut converter = ViterbiConverter::new(create_test_dictionary());
    converter.learn_unigram("はたち", "二十歳", 1); // ユーザー学習で500相当
    let before = *converter
        .learned_unigram
        .get(&("はたち".to_string(), "二十歳".to_string()))
        .unwrap();
    converter.load_word_priority_file(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(
        converter.learned_unigram.get(&("はたち".to_string(), "二十歳".to_string())),
        Some(&before)
    );
}

#[test]
fn test_load_word_priority_file_missing_path_is_graceful_skip() {
    let mut converter = ViterbiConverter::new(create_test_dictionary());
    let missing = std::env::temp_dir().join("ime_test_word_priority_does_not_exist.tsv");
    assert!(converter.load_word_priority_file(&missing).is_err());
}

#[test]
fn test_single_kanji_penalty_prefers_common_word() {
    // 1文字漢字ペナルティにより、同音の複合語が優先される
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    // 「きょう」に対し 今日(2文字) と 教(1文字) を登録。教の方が低コスト。
    dict.add_word(WordEntry {
        surface: "今日".to_string(), reading: "きょう".to_string(),
        left_id: 1, right_id: 1, cost: 5000, pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "教".to_string(), reading: "きょう".to_string(),
        left_id: 1, right_id: 1, cost: 4800, pos: "名詞".to_string(),
    });
    let converter = ViterbiConverter::new(dict);
    // ペナルティ(400)により 教(4800+400=5200) より 今日(5000) が勝つ
    assert_eq!(converter.convert_to_string("きょう"), "今日");
}

#[test]
fn test_learned_unigram_bonus_cannot_go_negative() {
    // 学習ユニグラムのボーナスは実効コストを0未満（負）にしてはならない。
    //
    // 実例: ユーザーが「効果」を「こうか」で何度も確定していると
    // learned_unigram のボーナスが上限(6000)に達する。この時、辞書上の
    // 「効果」の生コストが低ければ実効コストが負になり得る。負コストの
    // 語は「使えば使うほど得」になってしまい、たまたま後続に接続コストの
    // 安い1文字漢字（例: 接尾語的な語）が続くだけで、無関係な文脈で学習
    // したボーナスが正しい一続きの語（例: 「後悔」）の総コストを一方的に
    // 上回ってしまう（「こうかい」→「効果位」のような意味のない変換）。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    // 「こうか」→「効果」の直後に「い」→「位」が続く接続だけ特別に安く
    // する（IPA辞書にある「名詞+接尾語」パターンの実際の癖を再現）。
    dict.matrix.set(1, 2, -1500);
    dict.add_word(WordEntry {
        surface: "後悔".to_string(), reading: "こうかい".to_string(),
        left_id: 3, right_id: 3, cost: 4300, pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "効果".to_string(), reading: "こうか".to_string(),
        left_id: 1, right_id: 1, cost: 5200, pos: "名詞".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "位".to_string(), reading: "い".to_string(),
        left_id: 2, right_id: 2, cost: 6000, pos: "名詞".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    // 「こうか」→「効果」を無関係な文脈で何度も確定した想定（ボーナス上限到達）
    converter.learn_unigram("こうか", "効果", 10);
    // 「効果」が単独では正しく勝てることを確認（学習が効いている証拠）
    assert_eq!(converter.convert_to_string("こうか"), "効果");
    // にもかかわらず、「こうかい」は無関係な学習に引きずられて
    // 「効果」+「位」に分割されず、正しく1語の「後悔」になる
    assert_eq!(converter.convert_to_string("こうかい"), "後悔");
}

#[test]
fn test_katakana_kanji_suffix_penalty_exempts_allowed_suffixes() {
    // 語・人・製 等、外来語に実際に付く接尾辞は例外としてペナルティ対象外。
    let katakana = WordEntry {
        surface: "ジン".to_string(), reading: "じん".to_string(),
        left_id: 1, right_id: 1, cost: 3000, pos: "名詞-一般-*-*".to_string(),
    };
    let go = WordEntry {
        surface: "語".to_string(), reading: "ご".to_string(),
        left_id: 2, right_id: 2, cost: 3000, pos: "名詞-接尾-一般-*".to_string(),
    };
    let sei = WordEntry {
        surface: "性".to_string(), reading: "せい".to_string(),
        left_id: 2, right_id: 2, cost: 3000, pos: "名詞-接尾-一般-*".to_string(),
    };
    assert_eq!(katakana_kanji_suffix_penalty(&katakana, &go), 0);
    assert_eq!(katakana_kanji_suffix_penalty(&katakana, &sei), 2000);
}

#[test]
fn test_katakana_kanji_suffix_prevents_unbalanced_split() {
    // 実例: 「じんせい」が「ジン」(gin)+「性」に誤分割される
    // （「人生」という正しい1語があるにもかかわらず）。
    //
    // ユーザーが「性」を接尾辞として何度も使っていると learned_unigram の
    // ボーナスが乗り、さらに IPA辞書の「名詞+接尾辞」接続コストが実際の
    // 相性を無視して安いため、無関係なカタカナ名詞「ジン」に「性」が
    // くっついた「ジン性」の総コストが「人生」を下回ってしまうことがある。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    // 「ジン」→「性」の接続だけ特別に安くする（名詞+接尾辞パターンの癖）。
    dict.matrix.set(1, 2, -600);
    dict.add_word(WordEntry {
        surface: "人生".to_string(), reading: "じんせい".to_string(),
        left_id: 3, right_id: 3, cost: 4000, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "ジン".to_string(), reading: "じん".to_string(),
        left_id: 1, right_id: 1, cost: 3000, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "性".to_string(), reading: "せい".to_string(),
        left_id: 2, right_id: 2, cost: 3800, pos: "名詞-接尾-一般-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    // 「性」を接尾辞として何度も使った想定（ボーナスがほぼ上限に達する）
    converter.learn_unigram("せい", "性", 3);
    // 「性」は単独では正しく勝てることを確認（学習が効いている証拠）
    assert_eq!(converter.convert_to_string("せい"), "性");
    // にもかかわらず「じんせい」は「ジン」+「性」に分割されず、
    // 正しく1語の「人生」になる
    assert_eq!(converter.convert_to_string("じんせい"), "人生");
}

#[test]
fn test_single_kanji_pair_prevents_unbalanced_split() {
    // 実例: 「まんなか」が「万」+「中」に誤分割される
    // （「真ん中」という正しい1語があるにもかかわらず）。
    //
    // 「万」「中」はどちらも単独では極めて頻出な1文字漢字で、無関係な
    // 文脈での使用によりユニグラム学習のボーナスが上限近くまで達する。
    // さらに IPA辞書の「数+非自立名詞」「非自立名詞+文末」の接続コストは
    // 実際の相性を無視して極端に安いため、これらが合わさると「万中」の
    // 総コストが「真ん中」を下回ってしまうことがある。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(0, 1, 570); // BOS→万
    dict.matrix.set(1, 2, -1419); // 万→中（数+非自立名詞パターンの癖）
    dict.matrix.set(2, 0, -2484); // 中→EOS（非自立名詞の文末接続の癖）
    dict.matrix.set(0, 3, -283); // BOS→真ん中
    dict.matrix.set(3, 0, -573); // 真ん中→EOS
    dict.add_word(WordEntry {
        surface: "万".to_string(), reading: "まん".to_string(),
        left_id: 1, right_id: 1, cost: 3465, pos: "名詞-数-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "中".to_string(), reading: "なか".to_string(),
        left_id: 2, right_id: 2, cost: 6528, pos: "名詞-非自立-副詞可能-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "真ん中".to_string(), reading: "まんなか".to_string(),
        left_id: 3, right_id: 3, cost: 5609, pos: "名詞-一般-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    // 「万」「中」を無関係な文脈で何度も確定した想定（ボーナス上限到達）
    converter.learn_unigram("まん", "万", 10);
    converter.learn_unigram("なか", "中", 10);
    // 「真ん中」も一度は正しく確定した想定（適度なボーナス）
    converter.learn_unigram("まんなか", "真ん中", 3);
    // にもかかわらず「まんなか」は「万」+「中」に分割されず、
    // 正しく1語の「真ん中」になる
    assert_eq!(converter.convert_to_string("まんなか"), "真ん中");
}

#[test]
fn test_single_kanji_bound_noun_alone_not_penalized() {
    // 「方（ほう）」「他（ほか）」等の非自立名詞は、単独の変換結果としても
    // 極めて頻出。直前が1文字漢字でなければ（＝文頭からいきなりその語だけ
    // なら）文末接続のペナルティは掛からず、正しく変換できることを確認する
    // （single_kanji_pair系のペナルティが非自立名詞というだけで
    //  一律に不利にしてしまう回帰を防ぐ）。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(2, 0, -2484); // 中→EOS（非自立名詞の文末接続の癖）
    dict.add_word(WordEntry {
        surface: "中".to_string(), reading: "なか".to_string(),
        left_id: 2, right_id: 2, cost: 6528, pos: "名詞-非自立-副詞可能-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "仲".to_string(), reading: "なか".to_string(),
        left_id: 4, right_id: 4, cost: 7000, pos: "名詞-一般-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.learn_unigram("なか", "中", 10);
    // 直前に1文字漢字が無い（文頭から単独）ので、文末ペナルティは掛からず
    // 学習ボーナスどおり「中」が勝つ
    assert_eq!(converter.convert_to_string("なか"), "中");
}

#[test]
fn test_single_kanji_bound_noun_prevents_content_word_split() {
    // 実例: 「さいきどう」が「際」+「起動」に誤分割される
    // （「再起動」という正しい語があるにもかかわらず）。
    //
    // 「際」を無関係な文脈で何度も使っていると learned_unigram のボーナスが
    // 上限に達し、後ろに実在の内容語「起動」が続くだけで「再起動」より
    // 安く見えてしまうことがある。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(1, 2, -1400); // 際→起動（品詞IDの組の癖）
    dict.add_word(WordEntry {
        surface: "再起動".to_string(), reading: "さいきどう".to_string(),
        left_id: 3, right_id: 3, cost: 4000, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "際".to_string(), reading: "さい".to_string(),
        left_id: 1, right_id: 1, cost: 5562, pos: "名詞-非自立-副詞可能-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "起動".to_string(), reading: "きどう".to_string(),
        left_id: 2, right_id: 2, cost: 4210, pos: "名詞-サ変接続-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    converter.learn_unigram("さい", "際", 10);
    // にもかかわらず「さいきどう」は「際」+「起動」に分割されず、
    // 正しく1語の「再起動」になる
    assert_eq!(converter.convert_to_string("さいきどう"), "再起動");
}

#[test]
fn test_bonused_adjective_inflection_prevents_unbalanced_split() {
    // 実例: 「すくない」が「酸く」+「ない」に誤分割される
    // （「少ない」という正しい1語があるにもかかわらず）。
    //
    // 「形容詞連用形+ない」（高くない・安くない 等）の接続はIPA辞書上
    // 非常に安い。これ自体は正しい言語頻度なので学習が無ければ問題ない
    // （このテストの後半で確認する）が、無関係な文脈で「酸く」（酸い＝
    // 酸っぱいの連用形）を使った学習が乗ると、「少ない」より安く見えて
    // しまうことがある。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(1, 2, -10000); // 形容詞連用形→ない（活用接続の癖）
    dict.add_word(WordEntry {
        surface: "少ない".to_string(), reading: "すくない".to_string(),
        left_id: 3, right_id: 3, cost: 4692, pos: "形容詞-自立-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "酸く".to_string(), reading: "すく".to_string(),
        left_id: 1, right_id: 1, cost: 4758, pos: "形容詞-自立-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "ない".to_string(), reading: "ない".to_string(),
        left_id: 2, right_id: 2, cost: 8159, pos: "助動詞-*-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    // 「少ない」も一度は正しく確定した想定（適度なボーナス）
    converter.learn_unigram("すくない", "少ない", 1);
    // 「酸く」を無関係な文脈で何度か使い、「酸く→ない」の繋がりも
    // 一緒に確定した想定（ユニグラム・バイグラム双方に学習が乗る）
    converter.learn_unigram("すく", "酸く", 2);
    converter.learn_bigram("酸く", "ない", 5);
    // にもかかわらず「すくない」は「酸く」+「ない」に分割されず、
    // 正しく1語の「少ない」になる
    assert_eq!(converter.convert_to_string("すくない"), "少ない");

    // 学習が無ければ、極端に安い活用接続はそのまま（=通常の形容詞否定を
    // 壊さない）ことも確認する
    let mut fresh = ViterbiConverter::new(Dictionary::new());
    fresh.dictionary.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            fresh.dictionary.matrix.set(i, j, 200);
        }
    }
    fresh.dictionary.matrix.set(1, 2, -10000);
    fresh.dictionary.add_word(WordEntry {
        surface: "高い".to_string(), reading: "たかい".to_string(),
        left_id: 3, right_id: 3, cost: 4692, pos: "形容詞-自立-*-*".to_string(),
    });
    fresh.dictionary.add_word(WordEntry {
        surface: "高く".to_string(), reading: "たかく".to_string(),
        left_id: 1, right_id: 1, cost: 4000, pos: "形容詞-自立-*-*".to_string(),
    });
    fresh.dictionary.add_word(WordEntry {
        surface: "ない".to_string(), reading: "ない".to_string(),
        left_id: 2, right_id: 2, cost: 8159, pos: "助動詞-*-*-*".to_string(),
    });
    fresh.enable_katakana_fallback = false;
    assert_eq!(fresh.convert_to_string("たかくない"), "高くない");
}

#[test]
fn test_bonused_word_after_prefix_prevents_unbalanced_split() {
    // 実例: 「おなか」が「お」+「中」に誤分割される
    // （「お腹」という正しい1語があるにもかかわらず）。
    //
    // 「中」を無関係な文脈で何度も使っていると learned_unigram のボーナスが
    // 上限に達し、接頭詞「お」に直接続くだけで「お腹」より安く見えて
    // しまうことがある。祖先ノードが1文字漢字でなくても（「お」はひらがな）
    // 危険な組み合わせになり得るため、祖先の有無だけを見る。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(1, 2, -3000); // お→中（品詞IDの組の癖）
    dict.matrix.set(2, 0, -2000); // 中→EOS（非自立名詞の文末接続の癖）
    dict.add_word(WordEntry {
        surface: "お腹".to_string(), reading: "おなか".to_string(),
        left_id: 3, right_id: 3, cost: 5000, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "お".to_string(), reading: "お".to_string(),
        left_id: 1, right_id: 1, cost: 4000, pos: "接頭詞-名詞接続-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "中".to_string(), reading: "なか".to_string(),
        left_id: 2, right_id: 2, cost: 5000, pos: "名詞-非自立-副詞可能-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    converter.learn_unigram("なか", "中", 10);
    // にもかかわらず「おなか」は「お」+「中」に分割されず、
    // 正しく1語の「お腹」になる
    assert_eq!(converter.convert_to_string("おなか"), "お腹");
}

#[test]
fn test_adjective_terminal_then_te_penalty() {
    // 実例: 「すいて」が形容詞終止形「酸い」+「て」に誤解釈される
    // （現代日本語の文法では形容詞+ては連用形「〜くて」が正しく、
    //  終止形に直接「て」が続く「酸いて」は誤り）。
    //
    // 学習の有無に関わらず常に文法的な誤りなので、無条件でペナルティが
    // 掛かり、動詞の音便形「梳い」+「て」が選ばれることを確認する
    // （実在のCOMMON_WORD_SEEDと衝突しない語を使い、この機能単体を検証する）。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(1, 3, -4000); // 形容詞終止形→て（IPA辞書が許してしまう癖）
    dict.matrix.set(2, 3, -4000); // 動詞音便形→て
    dict.add_word(WordEntry {
        surface: "酸い".to_string(), reading: "すい".to_string(),
        left_id: 1, right_id: 1, cost: 3000, pos: "形容詞-自立-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "梳い".to_string(), reading: "すい".to_string(),
        left_id: 2, right_id: 2, cost: 3200, pos: "動詞-自立-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "て".to_string(), reading: "て".to_string(),
        left_id: 3, right_id: 3, cost: 3000, pos: "助詞-接続助詞-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    // 「酸い」の方が生コストは低いが、終止形+てにはペナルティが掛かるため
    // 「梳い」が選ばれる
    assert_eq!(converter.convert_to_string("すいて"), "梳いて");
}

#[test]
fn test_adjective_terminal_then_te_penalty_floor_overcomes_large_cost_gap() {
    // 実辞書相当の大きなコスト差（形容詞側の接続コストが極端に有利）では、
    // 旧実装の固定加算(+5000)では相殺しきれず「酸いて」系が勝ったままになる
    // 回帰を防ぐ。floor方式（下限9000）なら、接続コストがどれだけ有利でも
    // 「形容詞終止形+て」の合計コストが必ず高くなることを確認する。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(1, 3, -8500); // 形容詞終止形→て（実辞書相当の極端に有利な接続）
    dict.matrix.set(2, 3, -3200); // 動詞音便形→て
    dict.add_word(WordEntry {
        surface: "酸い".to_string(), reading: "すい".to_string(),
        left_id: 1, right_id: 1, cost: 3000, pos: "形容詞-自立-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "梳い".to_string(), reading: "すい".to_string(),
        left_id: 2, right_id: 2, cost: 3200, pos: "動詞-自立-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "て".to_string(), reading: "て".to_string(),
        left_id: 3, right_id: 3, cost: 3000, pos: "助詞-接続助詞-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    // 固定加算(+5000)では 3000-8500+5000=-500 < 3200-3200=0 のため
    // 「酸いて」が勝ってしまうが、floor(9000)では 3000+9000=12000 > 0 となり
    // 「梳いて」が選ばれる
    assert_eq!(converter.convert_to_string("すいて"), "梳いて");
}

#[test]
fn test_single_kanji_lone_particle_reading_penalty_fixes_mid_sentence_split() {
    // 実例: 「よこのみち」が「横」+「野」(1文字漢字, 読み「の」)に誤分割
    // され、正しい助詞分割「横」+「の」を押しのけてしまう問題を再現する。
    // 「野」が文頭以外（直前に「横」という実語がある）に出現する場合、
    // ペナルティにより正しい助詞分割が選ばれることを確認する。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.add_word(WordEntry {
        surface: "横".to_string(), reading: "よこ".to_string(),
        left_id: 1, right_id: 1, cost: 3000, pos: "名詞-固有名詞-地域-一般".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "野".to_string(), reading: "の".to_string(),
        left_id: 2, right_id: 2, cost: 1500, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "の".to_string(), reading: "の".to_string(),
        left_id: 3, right_id: 3, cost: 2200, pos: "助詞-連体化-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "みち".to_string(), reading: "みち".to_string(),
        left_id: 4, right_id: 4, cost: 3000, pos: "名詞-一般-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    // 「横」は COMMON_WORD_SEED に含まれる語のため、そのままだと無関係な
    // 学習ボーナス由来のfloor(bonused_adjective_stem_then_content_word_conn_cost)
    // が発火し本テストの検証対象と混線する。本テストの対象外なので消しておく。
    converter.forget_unigram("よこ", "横");
    // ペナルティが無ければ「野」(単語コスト1500+単独漢字ペナルティ400=1900)が
    // 「の」(2200)より安いため「横野みち」が勝つが、「横」(実語)の直後に
    // 「野」が来る接続にペナルティ(2500)が掛かるため逆転し「横のみち」が
    // 選ばれる
    assert_eq!(converter.convert_to_string("よこのみち"), "横のみち");
}

#[test]
fn test_single_kanji_lone_particle_reading_penalty_not_applied_at_sentence_start() {
    // 「野」が文頭（直前に実語が無い）に出現する場合はペナルティが
    // 適用されないことを確認する（無条件ガード節が `prev_node.entry`
    // が `Some` の場合のみ成立することによる、追加フラグ不要の設計）。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.add_word(WordEntry {
        surface: "野".to_string(), reading: "の".to_string(),
        left_id: 2, right_id: 2, cost: 1500, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "の".to_string(), reading: "の".to_string(),
        left_id: 3, right_id: 3, cost: 2200, pos: "助詞-連体化-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "みち".to_string(), reading: "みち".to_string(),
        left_id: 4, right_id: 4, cost: 3000, pos: "名詞-一般-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    // 文頭では「野」(実効コスト1900)が「の」(2200)より安いままなので、
    // ペナルティ非適用により「野みち」が選ばれる
    assert_eq!(converter.convert_to_string("のみち"), "野みち");
}

#[test]
fn test_n_best_applies_same_unconditional_guards_as_find_best_path() {
    // LiveConverter::generate_candidates は convert_to_string ではなく
    // n_best を使う（候補一覧・実際の変換結果の生成経路）。n_best は
    // 従来 dict.matrix の生の連接コストしか見ておらず、find_best_path が
    // 適用する無条件ガード（カテゴリF/G等、学習非依存のもの）が反映
    // されない実装差異があった。この回帰を防ぐため、n_best 経由でも
    // 同じ結果になることを確認する。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.add_word(WordEntry {
        surface: "扉".to_string(), reading: "とびら".to_string(),
        left_id: 1, right_id: 1, cost: 3000, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "野".to_string(), reading: "の".to_string(),
        left_id: 2, right_id: 2, cost: 1500, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "の".to_string(), reading: "の".to_string(),
        left_id: 3, right_id: 3, cost: 2200, pos: "助詞-連体化-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "みち".to_string(), reading: "みち".to_string(),
        left_id: 4, right_id: 4, cost: 3000, pos: "名詞-一般-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    let n_best = converter.n_best_strings("とびらのみち", 5);
    assert_eq!(n_best[0], "扉のみち", "n_best={:?}", n_best);
}

#[test]
fn test_single_kanji_bound_noun_particle_attachment_not_penalized() {
    // 「際に」「際は」のような助詞への接続は正当な用法なので、
    // 学習ボーナスが乗っていてもペナルティの対象にならないことを確認する
    // （bound_noun_then_content_word 系のペナルティが is_content_pos で
    //  助詞を除外していることの回帰防止）。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(1, 5, -1400); // 際→に
    dict.add_word(WordEntry {
        surface: "際".to_string(), reading: "さい".to_string(),
        left_id: 1, right_id: 1, cost: 5562, pos: "名詞-非自立-副詞可能-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "に".to_string(), reading: "に".to_string(),
        left_id: 5, right_id: 5, cost: 3000, pos: "助詞-格助詞-一般-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.learn_unigram("さい", "際", 10);
    assert_eq!(converter.convert_to_string("さいに"), "際に");
}

#[test]
fn test_filler_then_bonused_word_prevents_unbalanced_split() {
    // 実例: 「えいご」が「え」(フィラー)+「以後」に誤分割される
    // （「英語」という正しい語があるにもかかわらず）。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.matrix.set(1, 2, -1600); // え(フィラー)→以後
    dict.add_word(WordEntry {
        surface: "英語".to_string(), reading: "えいご".to_string(),
        left_id: 3, right_id: 3, cost: 3462, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "え".to_string(), reading: "え".to_string(),
        left_id: 1, right_id: 1, cost: 3031, pos: "フィラー-*-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "以後".to_string(), reading: "いご".to_string(),
        left_id: 2, right_id: 2, cost: 6487, pos: "名詞-非自立-副詞可能-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    converter.learn_unigram("いご", "以後", 10);
    assert_eq!(converter.convert_to_string("えいご"), "英語");
}

#[test]
fn test_single_kanji_pair_without_bonus_not_penalized() {
    // 「英語」（英+語）のように、学習と無関係にIPA辞書自体が正しく認識
    // している1文字漢字どうしの複合語は、ボーナスが乗っていなければ
    // ペナルティの対象にしてはいけない（single_kanji_pair 系のペナルティを
    // ボーナスの有無で絞り込んだことの回帰防止）。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    // 「英」「語」は単独では高コストだが、複合語としての接続が非常に安い
    // （IPA辞書が「英語」という組み合わせ自体を正当に高頻度と認識）。
    dict.matrix.set(1, 2, -10000);
    dict.add_word(WordEntry {
        surface: "英".to_string(), reading: "えい".to_string(),
        left_id: 1, right_id: 1, cost: 9010, pos: "名詞-一般-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "語".to_string(), reading: "ご".to_string(),
        left_id: 2, right_id: 2, cost: 8088, pos: "名詞-接尾-一般-*".to_string(),
    });
    // 学習ボーナスは一切与えない
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    assert_eq!(converter.convert_to_string("えいご"), "英語");
}

#[test]
fn test_rerank_by_assoc_ignores_directly_adjacent_content_words() {
    // 実例: 「さいきどう」の1-bestが正しく「再起動」でも、rerank_by_assoc が
    // 内容語連想で「再」を「際」に差し替えてしまい「際起動」になる。
    //
    // learned_assoc は本来「駅の汽車／新聞の記者」のように助詞を挟んで
    // 離れた内容語どうしの結びつきを見る仕組み。直前・直後（間に助詞すら
    // 挟まない隣接語）は通常の接続コストが既に見ているので、連想の対象から
    // 除外しないと、無関係な文脈で強く学習された連想が隣接語を巻き込んで
    // 誤った複合語を作ってしまう。
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    dict.add_word(WordEntry {
        surface: "再".to_string(), reading: "さい".to_string(),
        left_id: 1, right_id: 1, cost: 5787, pos: "接頭詞-名詞接続-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "際".to_string(), reading: "さい".to_string(),
        left_id: 1, right_id: 1, cost: 7036, pos: "名詞-副詞可能-*-*".to_string(),
    });
    dict.add_word(WordEntry {
        surface: "起動".to_string(), reading: "きどう".to_string(),
        left_id: 2, right_id: 2, cost: 4210, pos: "名詞-サ変接続-*-*".to_string(),
    });
    let mut converter = ViterbiConverter::new(dict);
    // 別の文脈で「際」と「起動」を何度も一緒に使った想定（強い内容語連想）
    converter.learn_assoc("起動", "際", 10);
    converter.learn_assoc("際", "起動", 10);
    // 1-best は「再」（連想の影響を受けない）
    assert_eq!(converter.convert_to_string("さいきどう"), "再起動");
    // 隣接していても rerank_by_assoc に上書きされない
    assert_eq!(converter.convert_context_aware_to_string("さいきどう"), "再起動");
}

#[test]
fn test_katakana_fallback_with_long_vowel_mark() {
    // 長音符「ー」を含む外来語表記も一括カタカナ化できる
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    for i in 0..10 {
        for j in 0..10 {
            dict.matrix.set(i, j, 200);
        }
    }
    let converter = ViterbiConverter::new(dict);
    let result = converter.convert_to_string("らーめん");
    assert_eq!(result, "ラーメン");
}

#[test]
fn test_katakana_fallback_with_particle() {
    // 「きょうはらすと」: きょう=今日、は=助詞、らすと=ラスト を期待
    let dict = create_test_dictionary();
    let converter = ViterbiConverter::new(dict);
    let result = converter.convert_to_string("きょうはらすと");
    // 「らすと」部分が「ラスト」になっていること
    assert!(result.contains("ラスト"), "カタカナ化未実施: {}", result);
    // 辞書ヒットは保たれる
    assert!(result.contains("今日") || result.contains("きょう"));
}

#[test]
fn test_katakana_fallback_disabled() {
    let mut dict = Dictionary::new();
    dict.matrix = crate::dictionary::ConnectionMatrix::new(10, 10);
    let mut converter = ViterbiConverter::new(dict);
    converter.enable_katakana_fallback = false;
    let result = converter.convert_to_string("らすと");
    // フォールバック無効ならひらがなのまま（未知語列）
    assert_eq!(result, "らすと");
}

#[test]
fn test_n_best() {
    let dict = create_test_dictionary();
    let converter = ViterbiConverter::new(dict);

    let results = converter.n_best_strings("きょうはいいてんきです", 3);
    assert!(!results.is_empty(), "N-best候補が0件");
    // 第一候補に「今日」が含まれる
    assert!(results[0].contains("今日"));
    // 重複なし
    let unique: std::collections::HashSet<_> = results.iter().collect();
    assert_eq!(unique.len(), results.len());
}

#[test]
fn test_n_best_empty_input() {
    let dict = create_test_dictionary();
    let converter = ViterbiConverter::new(dict);
    assert!(converter.n_best("", 5).is_empty());
    assert!(converter.n_best("きょう", 0).is_empty());
}

#[test]
fn test_live_conversion_context() {
    let dict = create_test_dictionary();
    let converter = ViterbiConverter::new(dict);
    let mut context = LiveConversionContext::new(converter);
    
    context.add_hiragana("きょう");
    assert!(!context.get_conversion().is_empty());
    
    context.add_hiragana("は");
    let conversion = context.get_conversion();
    assert!(conversion.contains("今日") || conversion.contains("は"));
}