//! Wikipedia ダンプ（pages-articles.xml.bz2）から本文の文を取り出す
//!
//! 判断層（judge-lm）の学習コーパス用。ウィキ記法を簡易的に取り除き、
//! 1行1文（句点で区切り、句点自体は含めない）のプレーンテキストにする。
//! 完璧なパーサではなく、テンプレート・表・脚注・画像などの「文でない部分」を
//! 落として、日本語の文として読める部分だけを残すことを目的にしている。

use anyhow::Result;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

/// ダンプを読み、抽出した文を `output` に書き出す。戻り値は (記事数, 文数)
pub fn extract(input: &Path, output: &Path, max_pages: Option<usize>) -> Result<(usize, usize)> {
    let file = File::open(input)?;
    let reader: Box<dyn BufRead> = if input.extension().map_or(false, |e| e == "bz2") {
        Box::new(BufReader::with_capacity(1 << 20, bzip2::read::MultiBzDecoder::new(file)))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, file))
    };
    let mut out = BufWriter::with_capacity(1 << 20, File::create(output)?);

    let mut pages = 0usize;
    let mut sentences = 0usize;
    let mut ns0 = false;
    let mut redirect = false;
    let mut in_text = false;
    let mut text = String::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim_start();
        if in_text {
            if let Some(end) = line.find("</text>") {
                text.push_str(&line[..end]);
                in_text = false;
            } else {
                text.push_str(&line);
                text.push('\n');
            }
            continue;
        }
        if trimmed.starts_with("<page>") {
            ns0 = false;
            redirect = false;
            text.clear();
        } else if trimmed.starts_with("<ns>") {
            ns0 = trimmed.starts_with("<ns>0</ns>");
        } else if trimmed.starts_with("<redirect") {
            redirect = true;
        } else if trimmed.starts_with("<text") {
            let Some(gt) = line.find('>') else { continue };
            if line[..gt].ends_with('/') {
                continue; // <text ... /> 空本文
            }
            let rest = &line[gt + 1..];
            if let Some(end) = rest.find("</text>") {
                text.push_str(&rest[..end]);
            } else {
                text.push_str(rest);
                text.push('\n');
                in_text = true;
            }
        } else if trimmed.starts_with("</page>") {
            if ns0 && !redirect && !text.is_empty() {
                for s in clean_wikitext(&text) {
                    out.write_all(s.as_bytes())?;
                    out.write_all(b"\n")?;
                    sentences += 1;
                }
                pages += 1;
                if pages % 10000 == 0 {
                    eprintln!("  {} 記事 / {} 文", pages, sentences);
                }
                if max_pages.map_or(false, |m| pages >= m) {
                    break;
                }
            }
            text.clear();
        }
    }
    out.flush()?;
    Ok((pages, sentences))
}

/// XML の文字参照を戻す（ダンプ本文は1回エスケープされている）
fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&amp;", "&")
}

/// `open`〜`close` の区間（入れ子対応）を取り除く
fn strip_nested(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    let mut i = 0usize;
    let b = s.as_bytes();
    while i < b.len() {
        if s[i..].starts_with(open) {
            depth += 1;
            i += open.len();
        } else if depth > 0 && s[i..].starts_with(close) {
            depth -= 1;
            i += close.len();
        } else {
            let ch_len = utf8_len(b[i]);
            if depth == 0 {
                out.push_str(&s[i..i + ch_len]);
            }
            i += ch_len;
        }
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
    .max(1)
}

/// `<tag ...>...</tag>` ブロックを丸ごと取り除く（大文字小文字は区別しない簡易版）
fn strip_tag_blocks(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find(&open) {
        let start = i + rel;
        // `<ref` が `<references` 等に誤一致しないよう、直後の文字を確認する
        let after = lower[start + open.len()..].chars().next();
        if !matches!(after, Some(' ') | Some('>') | Some('/') | Some('\t') | Some('\n')) {
            out.push_str(&s[i..start + open.len()]);
            i = start + open.len();
            continue;
        }
        out.push_str(&s[i..start]);
        let Some(gt_rel) = lower[start..].find('>') else {
            return out;
        };
        let gt = start + gt_rel;
        if lower[..gt].ends_with('/') {
            i = gt + 1; // 自己終了タグ
            continue;
        }
        match lower[gt..].find(&close) {
            Some(c) => i = gt + c + close.len(),
            None => return out,
        }
    }
    out.push_str(&s[i..]);
    out
}

/// 残ったタグ（`<br />` 等）を取り除く
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// 内部リンク `[[...]]` を表示文字列に置き換える（画像・カテゴリ・言語間リンクは削除）
fn replace_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while let Some(rel) = s[i..].find("[[") {
        let start = i + rel;
        out.push_str(&s[i..start]);
        // 入れ子を数えて対応する "]]" を探す
        let mut depth = 0usize;
        let mut j = start;
        let mut end = None;
        while j < s.len() {
            if s[j..].starts_with("[[") {
                depth += 1;
                j += 2;
            } else if s[j..].starts_with("]]") {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    end = Some(j);
                    break;
                }
            } else {
                j += utf8_len(s.as_bytes()[j]);
            }
        }
        let Some(end) = end else {
            return out; // 閉じていないリンク以降は捨てる
        };
        let inner = &s[start + 2..end - 2];
        let head = inner.split(['|', ':']).next().unwrap_or("");
        let is_namespace = inner.contains(':')
            && (matches!(
                head.trim().to_ascii_lowercase().as_str(),
                "file" | "image" | "ファイル" | "画像" | "category" | "カテゴリ" | "media" | "wikipedia" | "wp" | "template" | "help"
            ) || (head.len() <= 12 && head.chars().all(|c| c.is_ascii_lowercase() || c == '-')));
        if !is_namespace {
            let label = match inner.rfind('|') {
                Some(p) => &inner[p + 1..],
                None => inner,
            };
            out.push_str(&replace_links(label));
        }
        i = end;
    }
    out.push_str(&s[i..]);
    out
}

/// 外部リンク `[http://... 表示]` を表示文字列に置き換える
fn replace_external_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while let Some(rel) = s[i..].find("[http") {
        let start = i + rel;
        out.push_str(&s[i..start]);
        match s[start..].find(']') {
            Some(c) => {
                let inner = &s[start + 1..start + c];
                if let Some(sp) = inner.find(' ') {
                    out.push_str(&inner[sp + 1..]);
                }
                i = start + c + 1;
            }
            None => return out,
        }
    }
    out.push_str(&s[i..]);
    out
}

fn is_japanese_char(c: char) -> bool {
    matches!(c,
        '\u{3041}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '々' | '〆')
}

/// ウィキ記法の本文を文のリストにする
pub fn clean_wikitext(raw: &str) -> Vec<String> {
    let s = unescape_xml(raw);
    let s = s.replace("&nbsp;", " ");
    let s = strip_nested(&s, "<!--", "-->");
    let mut s = s;
    for tag in ["ref", "math", "gallery", "syntaxhighlight", "source", "pre", "code", "timeline", "score", "table", "imagemap", "chem", "poem", "references"] {
        s = strip_tag_blocks(&s, tag);
    }
    let s = strip_nested(&s, "{{", "}}");
    let s = strip_nested(&s, "{|", "|}");
    let s = replace_links(&s);
    let s = replace_external_links(&s);
    let s = strip_tags(&s);
    let s = s.replace("'''", "").replace("''", "");

    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with(['=', '*', '#', ':', ';', '|', '!', '{', '}', '_', '-'])
        {
            continue;
        }
        for sent in line.split(['。', '！', '？']) {
            let sent = sent.trim();
            let n = sent.chars().count();
            if n < 5 || n > 300 {
                continue;
            }
            let jp = sent.chars().filter(|&c| is_japanese_char(c)).count();
            let has_hiragana = sent.chars().any(|c| ('\u{3041}'..='\u{309F}').contains(&c));
            if has_hiragana && jp * 10 >= n * 6 {
                out.push(sent.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_common_markup() {
        let raw = "{{Infobox 人物|名前=テスト{{入れ子}}}}\n\
'''東京都'''（とうきょうと）は、[[日本]]の[[首都圏|首都]]である。<ref name=\"a\">出典の文章です。</ref>人口が多い。\n\
[[ファイル:Tokyo.jpg|thumb|東京の写真]]\n\
== 歴史 ==\n\
* 箇条書きは捨てる\n\
{| class=\"wikitable\"\n| 表の中身は捨てる\n|}\n\
江戸時代には[http://example.com 外部サイト]で知られていた。<!-- コメント -->\n\
[[Category:日本の都道府県]]";
        let sents = clean_wikitext(raw);
        assert_eq!(
            sents,
            vec![
                "東京都（とうきょうと）は、日本の首都である".to_string(),
                "人口が多い".to_string(),
                "江戸時代には外部サイトで知られていた".to_string(),
            ]
        );
    }

    #[test]
    fn self_closing_ref_and_entities() {
        let raw = "これは&lt;ref name=&quot;x&quot; /&gt;テストの文章です。";
        assert_eq!(clean_wikitext(raw), vec!["これはテストの文章です".to_string()]);
    }

    #[test]
    fn drops_non_japanese_lines() {
        assert!(clean_wikitext("This is an English sentence only.").is_empty());
    }
}
