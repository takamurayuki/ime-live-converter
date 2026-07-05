//! ローカルLLMによるかな漢字変換（要件 14.1「軽量LMによるrerank」）
//!
//! Ollama（<https://ollama.com>）のHTTP APIを叩き、ひらがな読みを
//! 自然な漢字かな交じり文に変換する。リアルタイム入力のメイン経路には
//! 入れず、ユーザーが明示的に要求したとき（Tab長押し）だけ呼ぶ。
//!
//! 設計方針:
//! - 失敗（LLM未起動・タイムアウト・不正応答）は全て `None` で返し、
//!   呼び出し側は通常変換を続行できること（要件 8.3 可用性）。
//! - 呼び出し側はこれをバックグラウンドスレッドで実行し、入力を
//!   ブロックしないこと。
//!
//! 環境変数で設定を上書きできる:
//! - `IME_LLM_URL`   : 生成APIのURL（既定 `http://localhost:11434/api/generate`）
//! - `IME_LLM_MODEL` : モデル名（既定 `qwen2.5:3b`）
//! - `IME_LLM_TIMEOUT_MS` : タイムアウト（既定 8000ms）

use std::time::Duration;

/// かな漢字変換を行うLLMバックエンドの抽象
///
/// 実装を差し替えることで、外部プロセス（Ollama）・組み込み推論
/// （llama.cpp / candle 等）・クラウドAPI を同じ呼び口で使える。
/// 失敗時は `None` を返し、呼び出し側は通常変換を継続する（要件 8.3）。
pub trait LlmBackend: Send + Sync {
    /// バックエンド名（ログ用）
    fn name(&self) -> &str;

    /// ひらがな読みを自然な日本語に変換する
    ///
    /// * `reading` - 変換対象のひらがな
    /// * `context` - 直前に確定したテキスト（文脈。空でも可）
    fn convert(&self, reading: &str, context: &str) -> Option<String>;
}

/// LLM変換の設定
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub url: String,
    pub model: String,
    pub timeout: Duration,
}

impl LlmConfig {
    /// 環境変数から設定を読む（未設定は既定値）
    pub fn from_env() -> Self {
        let url = std::env::var("IME_LLM_URL")
            .unwrap_or_else(|_| "http://localhost:11434/api/generate".to_string());
        // 既定は日本語特化モデル(ime-jp = 日本語版 Gemma 2 2B)。
        // 短縮プロンプトで校正精度が上がり、8Bより速い(~5秒)ため既定にする。
        // 校正力を最大にしたい場合は IME_LLM_MODEL=elyza-jp（8B, ~10秒）。
        let model = std::env::var("IME_LLM_MODEL").unwrap_or_else(|_| "ime-jp".to_string());
        // 初回はモデルのメモリ読込に十数秒かかるため既定を長めに取る
        let timeout_ms = std::env::var("IME_LLM_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30000);
        Self {
            url,
            model,
            timeout: Duration::from_millis(timeout_ms),
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Ollama（ローカルLLMサーバー）を使うバックエンド
pub struct OllamaBackend {
    cfg: LlmConfig,
}

impl OllamaBackend {
    pub fn new(cfg: LlmConfig) -> Self {
        Self { cfg }
    }

    /// 環境変数から設定して生成
    pub fn from_env() -> Self {
        Self::new(LlmConfig::from_env())
    }
}

impl LlmBackend for OllamaBackend {
    fn name(&self) -> &str {
        "ollama"
    }

    fn convert(&self, reading: &str, context: &str) -> Option<String> {
        if reading.trim().is_empty() {
            return None;
        }

        let prompt = build_prompt(reading, context);
        let body = serde_json::json!({
            "model": self.cfg.model,
            "prompt": prompt,
            "stream": false,
            // 変換は決定的にしたいので温度は低め
            "options": { "temperature": 0.1 },
            // モデルをメモリに保持して次回変換を高速化（IME用途）
            "keep_alive": "30m"
        });

        let agent = ureq::AgentBuilder::new().timeout(self.cfg.timeout).build();
        let resp = agent.post(&self.cfg.url).send_json(body).ok()?;
        let json: serde_json::Value = resp.into_json().ok()?;
        let text = json.get("response")?.as_str()?;

        let cleaned = clean_output(text);
        if cleaned.is_empty() {
            return None;
        }
        // モデルが読みをそのまま返した（＝変換できず平仮名エコー）場合は失敗扱い。
        // これを適用すると統計変換の良い結果が平仮名で上書きされてしまう。
        if cleaned == reading || is_all_hiragana(&cleaned) {
            return None;
        }
        Some(cleaned)
    }
}

/// Ollama サーバーに接続できるか（起動状態の確認）を短時間で調べる
///
/// 生成URLからタグ一覧URLを導出して GET する。到達できれば true。
pub fn ollama_available(cfg: &LlmConfig) -> bool {
    let tags_url = if cfg.url.contains("/api/generate") {
        cfg.url.replace("/api/generate", "/api/tags")
    } else {
        // 想定外のURL形でも一応叩いてみる
        cfg.url.clone()
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(1500))
        .build();
    agent.get(&tags_url).call().is_ok()
}

/// モデルをメモリに読み込ませて温めておく（初回変換の待ち時間を短縮）
///
/// 起動時にバックグラウンドで呼ぶ想定。成否は返すが失敗しても無害。
pub fn warm_up(cfg: &LlmConfig) -> bool {
    // ごく短い生成でモデルをロードさせる
    let body = serde_json::json!({
        "model": cfg.model,
        "prompt": "あ",
        "stream": false,
        "options": { "num_predict": 1, "temperature": 0.0 },
        "keep_alive": "30m"
    });
    let agent = ureq::AgentBuilder::new()
        .timeout(cfg.timeout.max(std::time::Duration::from_secs(60)))
        .build();
    agent.post(&cfg.url).send_json(body).is_ok()
}

/// ひらがな読みをLLMで自然な日本語に変換する（既定バックエンド = Ollama）
///
/// * `reading` - 変換対象のひらがな
/// * `context` - 直前に確定したテキスト（文脈。空でも可）
///
/// 成功時は変換後テキスト、失敗時は `None`。
/// ネットワーク・LLMを使うため呼び出し側はバックグラウンドで実行すること。
pub fn llm_convert(reading: &str, context: &str) -> Option<String> {
    OllamaBackend::from_env().convert(reading, context)
}

/// 設定を指定してLLM変換する（Ollama）
pub fn llm_convert_with(cfg: &LlmConfig, reading: &str, context: &str) -> Option<String> {
    OllamaBackend::new(cfg.clone()).convert(reading, context)
}

/// 統計変換の下書きを、LLMで校正して厳密に正しい日本語にする
///
/// 「読み（＝内容の正解）」と「下書き（＝統計変換の結果）」の両方を渡し、
/// 誤字脱字・文法（助詞のわ/は、お/を 等）の誤りだけを直させる。
/// 読みを制約に使うので、言い換えや意味の改変が起きにくい。
///
/// 成功時は校正後テキスト。失敗・平仮名エコー時は `None`
///（呼び出し側は下書きのまま維持すればよい）。
pub fn llm_correct(
    cfg: &LlmConfig,
    reading: &str,
    draft: &str,
    candidates: &[String],
    context: &str,
) -> Option<String> {
    if reading.trim().is_empty() || draft.trim().is_empty() {
        return None;
    }
    let ctx = if context.trim().is_empty() {
        String::new()
    } else {
        format!("これまでの文章: 「{}」\n", context.trim())
    };
    // 統計エンジンの候補を少数だけ参考として渡す（多いと推論が遅くなる）
    let cand_block = if candidates.is_empty() {
        String::new()
    } else {
        let mut s = String::from("参考候補: ");
        for c in candidates.iter().take(3) {
            s.push_str(&format!("{} / ", c));
        }
        s.push('\n');
        s
    };
    // 出力上限は読みの文字数+αで十分（短くして推論を速くする）
    let predict = (reading.chars().count() as i64 + 16).clamp(24, 96);
    let prompt = format!(
        "次の日本語の下書きから、誤字・文法（は/へ/を、送り仮名 等）だけを直し、\
         正しい一文にしてください。意味・語順・語尾は変えない。読みに無い語を足さない。\
         既に正しければそのまま。出力は一文のみ。\n\
         例: 今日わ会議がある → 今日は会議がある\n\
         例: 資料お作る → 資料を作る\n\
         {ctx}{cand_block}下書き: {draft} → "
    );
    let body = serde_json::json!({
        "model": cfg.model,
        "prompt": prompt,
        "stream": false,
        "options": { "temperature": 0.0, "num_predict": predict },
        "keep_alive": "30m"
    });
    let agent = ureq::AgentBuilder::new().timeout(cfg.timeout).build();
    let resp = agent.post(&cfg.url).send_json(body).ok()?;
    let json: serde_json::Value = resp.into_json().ok()?;
    let text = json.get("response")?.as_str()?;

    let cleaned = clean_output(text);
    if cleaned.is_empty() || is_all_hiragana(&cleaned) || looks_like_meta(&cleaned) {
        return None; // 失敗・平仮名エコー・指示文の混入は下書きを維持
    }
    // ハルシネーション対策: 校正がカタカナを増やしたら破棄する。
    // 良い校正は誤字・文法を直すだけでカタカナを足さない。「天気」→「テン気」
    // のように下書きに無いカタカナを生む出力は悪化なので下書きを維持する。
    if count_katakana(&cleaned) > count_katakana(draft) {
        return None;
    }
    Some(cleaned)
}

/// カタカナ文字数を数える（校正がカタカナを増やす=ハルシネーション判定用）
fn count_katakana(s: &str) -> usize {
    s.chars()
        .filter(|&c| ('\u{30A1}'..='\u{30FA}').contains(&c))
        .count()
}

/// LLMの出力が「答え」でなく指示文・前置きの漏れかを判定する
///
/// モデルが稀に「# 指示に従い…修正します。」のような前置きを出すため、
/// それを検出して破棄する（適用すると変な文が入ってしまう）。
fn looks_like_meta(s: &str) -> bool {
    s.starts_with('#')
        || s.contains("指示")
        || s.contains("下書き")
        || s.contains("修正します")
        || s.contains("以下")
        || s.contains("```")
}

/// 変換候補の中から、文脈上最も自然なものをLLMに選ばせる（rerank）
///
/// LLMに「生成」ではなく「選択」だけをさせる。候補は統計エンジン由来で
/// 読みが保証されているため、平仮名化・言い換え・語の欠落が起きない。
/// 小型モデルでも数字1つを答えるだけなので安定する。
///
/// 戻り値: 選ばれた候補のインデックス。失敗時は `None`
///（呼び出し側は統計1位=index 0 にフォールバックすればよい）。
pub fn llm_rerank(cfg: &LlmConfig, reading: &str, context: &str, candidates: &[String]) -> Option<usize> {
    if candidates.len() < 2 {
        return None;
    }
    // 候補は最大9件まで（番号1〜9で答えさせる）
    let n = candidates.len().min(9);
    let mut list = String::new();
    for (i, c) in candidates.iter().take(n).enumerate() {
        list.push_str(&format!("{}. {}\n", i + 1, c));
    }
    let ctx = if context.trim().is_empty() {
        String::new()
    } else {
        format!("これまでの文章: 「{}」\n", context.trim())
    };
    let prompt = format!(
        "次はひらがな「{reading}」を漢字かな交じりに変換した候補です。\n\
         {ctx}文脈に最も合う自然で正しい日本語はどれですか。\
         番号だけを半角数字1文字で答えてください（説明不要）。\n\
         {list}答え: "
    );
    let body = serde_json::json!({
        "model": cfg.model,
        "prompt": prompt,
        "stream": false,
        "options": { "temperature": 0.0, "num_predict": 4 },
        "keep_alive": "30m"
    });

    let agent = ureq::AgentBuilder::new().timeout(cfg.timeout).build();
    let resp = agent.post(&cfg.url).send_json(body).ok()?;
    let json: serde_json::Value = resp.into_json().ok()?;
    let text = json.get("response")?.as_str()?;

    // 応答から最初の数字(1〜9)を拾ってインデックスに変換
    let digit = text.chars().find_map(|c| c.to_digit(10))?;
    let idx = (digit as usize).checked_sub(1)?;
    if idx < n {
        Some(idx)
    } else {
        None
    }
}

/// 変換用プロンプトを組み立てる（かな漢字変換に厳格化）
///
/// 小型LLMは「理解して言い換える」傾向が強く、読みを勝手に要約・翻訳
/// してしまう。そこで「音を一切変えない書き換え」という transliteration
/// タスクとして強く枠づけし、禁止事項と few-shot 例で逸脱を抑える。
fn build_prompt(reading: &str, context: &str) -> String {
    let ctx = if context.trim().is_empty() {
        String::new()
    } else {
        format!("直前までの文章（文脈のみ。出力に含めない）: 「{}」\n", context.trim())
    };
    format!(
        "# 指示\n\
         あなたはかな漢字変換器です。次のひらがなの読みを、音を一切変えずに\
         漢字かな交じり表記へ書き換えます。\n\
         禁止: 語の追加・削除、言い換え、要約、翻訳、英語の使用。\
         読みの音は入力と完全に一致させる。助詞「は/へ/を」は文法どおりに表記する。\n\
         必ず適切な漢字に変換する。ひらがなのまま出力してはいけない。\n\
         出力は変換後の1行のみ。説明・引用符・記号の装飾を付けない。\n\
         # 例\n\
         にゅうりょく → 入力\n\
         かいぎのしりょうをつくる → 会議の資料を作る\n\
         えきできしゃをまつ → 駅で汽車を待つ\n\
         しんぶんきしゃにあう → 新聞記者に会う\n\
         きょうはいいてんきです → 今日はいい天気です\n\
         # 変換\n\
         {ctx}{reading} → "
    )
}

/// 文字列が全てひらがな（＋長音符・句読点）か
///
/// LLMが変換せず平仮名を返したかの判定に使う。漢字・カタカナが
/// 1文字でもあれば false。
fn is_all_hiragana(s: &str) -> bool {
    let mut has_kana = false;
    for c in s.chars() {
        if ('\u{3041}'..='\u{3096}').contains(&c) {
            has_kana = true;
        } else if matches!(c, 'ー' | '、' | '。' | '！' | '？' | '　' | ' ') {
            // 許容（変換に無関係な記号）
        } else {
            return false; // 漢字・カタカナ・英字などがある
        }
    }
    has_kana
}

/// LLM出力から余計な装飾を取り除く
///
/// モデルは時々引用符・前置き・改行を付けるので、最初の行の
/// 中身だけを取り出す。
fn clean_output(text: &str) -> String {
    // 前置き行（#... や「以下…」等）を飛ばし、最初の中身らしい行を採用
    let mut line = text
        .trim()
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.contains("以下"))
        .unwrap_or("");
    // モデルが「→ 結果」と矢印を残す場合は矢印以降を採用
    if let Some(pos) = line.rfind('→') {
        line = line[pos + '→'.len_utf8()..].trim();
    }
    // 前後の引用符・記号を剥がす
    let trimmed = line
        .trim_matches(|c| matches!(c, '「' | '」' | '『' | '』' | '"' | '\'' | '`' | ' ' | '　'));
    // モデルが混入させる絵文字・記号を除去（📖 等の幻覚対策）
    trimmed.chars().filter(|&c| !is_emoji_or_symbol(c)).collect()
}

/// 絵文字・装飾記号か（通常の日本語文には現れないもの）
fn is_emoji_or_symbol(c: char) -> bool {
    let u = c as u32;
    (0x1F000..=0x1FAFF).contains(&u)   // 絵文字（補助多言語面）
        || (0x2600..=0x27BF).contains(&u)  // その他記号・装飾記号
        || (0x2B00..=0x2BFF).contains(&u)
        || (0xFE00..=0xFE0F).contains(&u)  // 異体字セレクタ
        || u == 0x200D                     // ZWJ
        || (0x2190..=0x21FF).contains(&u)  // 矢印類（→ は前段で処理済み）
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_output() {
        assert_eq!(clean_output("今日はいい天気です"), "今日はいい天気です");
        assert_eq!(clean_output("「今日はいい天気です」"), "今日はいい天気です");
        assert_eq!(clean_output("今日はいい天気です\n(説明)"), "今日はいい天気です");
        assert_eq!(clean_output("  出力  "), "出力");
    }

    #[test]
    fn test_build_prompt_includes_reading() {
        let p = build_prompt("きょう", "");
        assert!(p.contains("きょう"));
        let p2 = build_prompt("です", "今日はいい天気");
        assert!(p2.contains("今日はいい天気"));
        assert!(p2.contains("です"));
    }

    #[test]
    fn test_empty_reading_returns_none() {
        assert!(llm_convert("", "").is_none());
        assert!(llm_convert("   ", "").is_none());
    }

    #[test]
    fn test_is_all_hiragana() {
        assert!(is_all_hiragana("こんにちは"));
        assert!(is_all_hiragana("きょうは、いいてんき。"));
        assert!(is_all_hiragana("らーめん"));
        assert!(!is_all_hiragana("今日")); // 漢字
        assert!(!is_all_hiragana("ラーメン")); // カタカナ
        assert!(!is_all_hiragana("PC")); // 英字
        assert!(!is_all_hiragana("")); // 空はかな無し
    }

    #[test]
    fn test_clean_output_strips_arrow() {
        // モデルが「→ 結果」と矢印を残す場合は矢印以降を採用
        assert_eq!(clean_output("→ 今日は晴れ"), "今日は晴れ");
        assert_eq!(clean_output("入力 → 今日は晴れ"), "今日は晴れ");
    }

    #[test]
    fn test_clean_output_strips_emoji() {
        // 絵文字・装飾記号を除去（📖 等の幻覚対策）
        assert_eq!(clean_output("私📖は本を読む"), "私は本を読む");
        assert_eq!(clean_output("今日は晴れ✨"), "今日は晴れ");
    }

    #[test]
    fn test_clean_output_skips_meta_line() {
        // #で始まる前置き行は飛ばして次の中身を採用
        assert_eq!(clean_output("# 指示\n今日は晴れ"), "今日は晴れ");
    }

    #[test]
    fn test_count_katakana() {
        assert_eq!(count_katakana("テン気"), 2);
        assert_eq!(count_katakana("天気"), 0);
        assert_eq!(count_katakana("ラーメン"), 3); // ー は対象外だが ラメン=3
    }
}
