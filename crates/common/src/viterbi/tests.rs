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
    let converter = ViterbiConverter::new(create_test_dictionary());
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
    // ペナルティ(300)により 教(4800+300=5100) より 今日(5000) が勝つ
    assert_eq!(converter.convert_to_string("きょう"), "今日");
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