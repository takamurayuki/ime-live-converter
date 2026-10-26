//! 英単語の自動判定: ローマ字の打鍵から英単語の候補を作る。
//!
//! 「konocodewo」は かな では「このこでを」だが、打鍵 `code` は英単語なので
//! 「このcodeを」とも読める。どちらにするかは判断層（`judge`）が日本語の候補と
//! 同じ物差しで比べて決める（`made`→まで、`kite`→来て は日本語のほうが
//! 自然なので日本語のまま、`code`→こで は日本語として不自然なので英語）。
//!
//! ここでは候補を作るための材料だけを持つ:
//! - 英単語の出現頻度（`english_words.tsv`、映画字幕コーパス由来）と
//!   IT・開発用語の補い（`english_tech.txt`）
//! - 辞書に無い英単語（kubectl 等）を採点する英字の3文字連鎖モデル
//! - かなの各片がどの打鍵から来たか（`RomajiPiece`）から、英単語として
//!   読める範囲を拾い出す処理（`EnglishLexicon::spans`）
use crate::{PosId, WordEntry};
use std::collections::HashMap;
use std::path::Path;

/// 英単語の候補の品詞名（判断層・学習はこれで英単語を見分ける）
pub const ENGLISH_POS: &str = "英単語";
/// Shift で直接入力した大文字に続く英単語の残り（Docker の ocker）の品詞名。
/// 表記は語の残りの部分だけなので、学習には使わない。
pub const ENGLISH_CONTINUATION_POS: &str = "英単語続き";
/// 英単語に当てる文脈ID（IPADic の 名詞-一般 / 名詞-サ変接続）。
/// 「codeを」「testする」のどちらの並びも自然に採点できるよう両方を出す。
const NOUN_GENERAL_ID: PosId = 1285;
const NOUN_SAHEN_ID: PosId = 1283;

/// 英単語の候補にする打鍵の文字数の範囲。2文字の英単語（no・to・do・me 等）は
/// ローマ字の1モーラ（の・と・ど・め）と同じ綴りで、日本語の文を壊す危険が
/// 大きすぎるので候補にしない。
const MIN_WORD_LETTERS: usize = 3;
const MAX_WORD_LETTERS: usize = 24;

/// IT・開発用語（`english_tech.txt`）を少なくともこの順位の語と同じ頻度として扱う
const TECH_WORD_RANK: usize = 3000;

/// 打鍵がすべてかなに直る（ローマ字としても打てる）英単語は、頻度表のこの順位
/// までだけ使う。それより下は人名やローマ字表記の日本語（marina・kane・mara・
/// moe・cho・nani・kono）が大半で、日本語の打鍵を英単語と取り違える元になる
/// （2000文の評価で英字に化けた文はすべてこの種の語だった）。かなに直らない
/// 英字を含む語（test・docker）は日本語の打鍵と取り違えないので順位を問わない。
/// 用語リスト（`english_tech.txt`）の語は常に使う（database 等）。
const CONVERTIBLE_MAX_RANK: usize = 5000;

/// 頻度表から使う語の順位の上限（かなに直らない英字を含む語にも掛ける）。これより
/// 下は人名・固有名詞・崩れた綴りがほとんどで（kuk 31558位 は seisakuk の打ち残し
/// k と一致して「精査kuk」を作った）、要る専門語は用語リストで補う。
const MAX_RANK: usize = 25000;

/// 英単語の対数確率の上限（頻度表のおよそ300位の語の値）。頻度表は映画字幕の
/// 英語の文での頻度で、you・are・the・one のような機能語が極端に高い。日本語の文に
/// 混ざる英単語はほぼ内容語（code・server・data）なので、機能語の頻度をそのまま
/// 使うと よう→you・あれ→are のように日本語の打鍵を英単語に取り違えやすくなる。
const MAX_WORD_LOGP: f32 = -8.0;

/// 辞書に無い英単語（英字の3文字連鎖モデルで採点する）への追加の減点（自然対数）。
/// 頻度表にある語より不確かなので、その分だけ不利にする。
const UNKNOWN_WORD_LOGP: f32 = -4.0;
/// 辞書に無い英単語として候補にするのに要る、かなに直せない英字の数。打ち間違い
/// （kaqsha の q、gomennasai を gmenn と打った g、末尾に残った k）は迷子の英字が
/// 1つだけ残る形で、綴りの採点（kuk -3.1/文字・gmen -2.7/文字）では実在の語
/// （kubectl -3.4/文字）と区別できない。実在の未知の専門語は kubectl（c・t・l）・
/// webpack（b・c・k）・zustand（s・t・d）のように複数残ることが多い。
/// 1つしか残らない専門語（nginx・figma・axios）は用語リストに入れる。
const UNKNOWN_MIN_UNCONVERTIBLE: usize = 2;

/// 辞書に無い英単語として候補にする、1文字あたりの対数確率の下限（語末記号を含む）。
/// 英語らしくない並びまで英単語の候補にしない。実測（`english_spell`）で
/// 実在の専門語は docker -1.8・kubectl -3.3・nginx -3.7、ローマ字の打ち残しや
/// 無意味な並びは ctlwo -4.8・sttwo -4.6・xkqz -6.6。
const UNKNOWN_MIN_CHAR_LOGP: f32 = -4.0;

/// かなの1片と、それを作った打鍵（`RomajiConverter::convert_aligned` の単位）。
/// `kyo`→「きょ」のように1片が2文字以上のかなになることもある。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomajiPiece {
    pub kana: String,
    pub raw: String,
}

impl RomajiPiece {
    /// かなに直せずに英字のまま残った片か（test → て s t の s・t）
    fn is_unconvertible(&self) -> bool {
        !self.kana.is_empty() && self.kana.chars().all(|c| c.is_ascii_alphabetic())
    }
}

/// 英単語として読める範囲1つ（`spans` の結果）
#[derive(Debug, Clone, PartialEq)]
pub struct EnglishSpan {
    /// 読み（かな）の文字位置 [start, end)
    pub start: usize,
    pub end: usize,
    /// 表示する英字（打鍵どおり。用語リストに正しい表記があればその表記: GitHub）
    pub word: String,
    /// その英単語の対数確率（頻度表、無ければ英字の連鎖モデル＋減点）
    pub logp: f32,
    /// 直前に直接入力した大文字の続き（`word` は語の残りの部分）か
    pub continuation: bool,
}

/// 英単語の頻度表と英字の連鎖モデル
pub struct EnglishLexicon {
    /// 英単語 → log P(語)
    words: HashMap<String, f32>,
    /// 英字の3文字連鎖（直前2文字 → 次の文字）の対数確率。語頭・語末は '^' / '$'
    trigram: HashMap<[u8; 3], f32>,
    /// 直前2文字の文脈ごとの、見たことのない次の文字の対数確率
    unseen: HashMap<[u8; 2], f32>,
    /// 用語リスト（`english_tech.txt`）の語（小文字）。文頭の大文字化（`capitalize_at_sentence_start`）
    /// の対象外にする（kubectl・npm・git のようなコマンド名は文頭でも小文字のまま）
    tech: std::collections::HashSet<String>,
    /// 用語リストに大文字を含めて書いた語の正しい表記（小文字 → GitHub・macOS・JSON）。
    /// 文中のどこでもこの表記にする
    casing: HashMap<String, String>,
}

impl std::fmt::Debug for EnglishLexicon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EnglishLexicon {{ words: {} }}", self.words.len())
    }
}

impl EnglishLexicon {
    /// 頻度表（単語<TAB>回数）と用語リスト（空白区切りの単語）の文字列から作る
    pub fn from_strs(freq_tsv: &str, tech_words: &str) -> Self {
        let mut counts: Vec<(String, f64)> = Vec::new();
        for line in freq_tsv.lines() {
            if line.starts_with('#') {
                continue;
            }
            let mut cols = line.split('\t');
            let (Some(w), Some(c)) = (cols.next(), cols.next()) else { continue };
            let Ok(c) = c.trim().parse::<f64>() else { continue };
            if is_word(w) {
                counts.push((w.to_string(), c));
            }
        }
        counts.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let tech_count = counts.get(TECH_WORD_RANK.min(counts.len().saturating_sub(1))).map_or(1.0, |c| c.1);
        let romaji = crate::RomajiConverter::new();
        let mut tech: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut casing: HashMap<String, String> = HashMap::new();
        let mut map: HashMap<String, f64> = counts
            .into_iter()
            .enumerate()
            .filter(|(rank, (w, _))| {
                *rank < CONVERTIBLE_MAX_RANK
                    || (*rank < MAX_RANK && romaji.convert(w).chars().any(|c| c.is_ascii_alphabetic()))
            })
            .map(|(_, wc)| wc)
            .collect();
        for w in tech_words
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .flat_map(|l| l.split_whitespace())
        {
            let lower = w.to_ascii_lowercase();
            if is_word(&lower) {
                if w != lower {
                    casing.insert(lower.clone(), w.to_string());
                }
                tech.insert(lower.clone());
                let c = map.entry(lower).or_insert(0.0);
                *c = c.max(tech_count);
            }
        }
        let total: f64 = map.values().sum::<f64>().max(1.0);
        let words: HashMap<String, f32> =
            map.iter().map(|(w, c)| (w.clone(), ((c / total).ln() as f32).min(MAX_WORD_LOGP))).collect();

        // 英字の3文字連鎖は語の種類ごとに1回ずつ数える（頻度で重みを付けると
        // the・you 等の機能語の綴りに偏り、辞書に無い専門語の採点に向かない）
        let mut tri: HashMap<[u8; 3], u32> = HashMap::new();
        let mut ctx: HashMap<[u8; 2], u32> = HashMap::new();
        for w in map.keys() {
            let b = padded(w);
            for t in b.windows(3) {
                *tri.entry([t[0], t[1], t[2]]).or_insert(0) += 1;
                *ctx.entry([t[0], t[1]]).or_insert(0) += 1;
            }
        }
        // 加算スムージング（次の文字は a-z と語末の27通り）
        const K: f32 = 0.1;
        let trigram = tri
            .iter()
            .map(|(t, &c)| {
                let n = ctx[&[t[0], t[1]]] as f32;
                (*t, ((c as f32 + K) / (n + K * 27.0)).ln())
            })
            .collect();
        let unseen = ctx.iter().map(|(c, &n)| (*c, (K / (n as f32 + K * 27.0)).ln())).collect();
        Self { words, trigram, unseen, tech, casing }
    }

    /// 辞書と同じディレクトリの `english_words.tsv`（必須）と `english_tech.txt`（任意）を読む
    pub fn load_dir(dir: &Path) -> std::io::Result<Self> {
        let freq = std::fs::read_to_string(dir.join("english_words.tsv"))?;
        let tech = std::fs::read_to_string(dir.join("english_tech.txt")).unwrap_or_default();
        Ok(Self::from_strs(&freq, &tech))
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// 英単語（小文字）の表示する表記。用語リストに大文字を含めて書いた語はその表記
    /// （github → GitHub）、それ以外はそのまま
    pub fn display_form(&self, lower: &str) -> String {
        self.casing.get(lower).cloned().unwrap_or_else(|| lower.to_string())
    }

    /// 文の先頭の英単語の頭文字を大文字にしてよいか（用語リストの語は対象外。
    /// 正しい表記は `display_form` が決め、kubectl 等のコマンド名は小文字のまま）
    pub fn capitalize_at_sentence_start(&self, word: &str) -> bool {
        let lower = word.trim_start().to_ascii_lowercase();
        !self.tech.contains(&lower) && lower.bytes().all(|b| b.is_ascii_lowercase())
    }

    /// 頻度表にある英単語の対数確率
    pub fn word_logp(&self, word: &str) -> Option<f32> {
        self.words.get(word).copied()
    }

    /// 英単語の対数確率（頻度表、無ければ綴りの採点＋減点）。候補一覧に打鍵どおりの
    /// 英字を出すときの採点用（`spans` の採点と同じ式）。
    pub fn score(&self, word: &str) -> f32 {
        let w = word.to_ascii_lowercase();
        self.word_logp(&w).unwrap_or_else(|| self.spelling_logp(&w) + UNKNOWN_WORD_LOGP)
    }

    /// 英字の3文字連鎖モデルによる綴りの対数確率（語末記号を含む）
    pub fn spelling_logp(&self, word: &str) -> f32 {
        let b = padded(word);
        b.windows(3)
            .map(|t| {
                self.trigram
                    .get(&[t[0], t[1], t[2]])
                    .copied()
                    .or_else(|| self.unseen.get(&[t[0], t[1]]).copied())
                    .unwrap_or((1.0f32 / 27.0).ln() * 2.0)
            })
            .sum()
    }

    /// 打鍵の並び（かなの片の列）のうち、英単語として読める範囲をすべて返す。
    /// 範囲の端は片の境目だけ（「きょ」の途中では切らない）。
    /// - 頻度表にある英単語: 打鍵が3文字以上
    /// - 頻度表に無い英単語: かなに直せない英字（kubectl の c・t・l）を
    ///   `UNKNOWN_MIN_UNCONVERTIBLE` 個以上含み、綴りが英語らしいもの
    pub fn spans(&self, pieces: &[RomajiPiece]) -> Vec<EnglishSpan> {
        let mut kana_pos = Vec::with_capacity(pieces.len() + 1);
        let mut pos = 0usize;
        for p in pieces {
            kana_pos.push(pos);
            pos += p.kana.chars().count();
        }
        kana_pos.push(pos);

        let mut known: Vec<(usize, usize, String, f32)> = Vec::new();
        let mut unknown: Vec<(usize, usize, String, f32)> = Vec::new();
        for i in 0..pieces.len() {
            let mut word = String::new();
            let mut unconvertible = 0usize;
            for j in i..pieces.len() {
                let p = &pieces[j];
                if p.raw.is_empty() || !p.raw.chars().all(|c| c.is_ascii_alphabetic()) {
                    break;
                }
                word.push_str(&p.raw.to_ascii_lowercase());
                unconvertible += p.is_unconvertible() as usize;
                let n = word.len();
                if n > MAX_WORD_LETTERS {
                    break;
                }
                if n < MIN_WORD_LETTERS {
                    continue;
                }
                match self.word_logp(&word) {
                    Some(lp) => known.push((i, j + 1, word.clone(), lp)),
                    None if unconvertible >= UNKNOWN_MIN_UNCONVERTIBLE => {
                        let spell = self.spelling_logp(&word);
                        // 語末記号の分も1文字と数える
                        if spell / (n + 1) as f32 >= UNKNOWN_MIN_CHAR_LOGP {
                            unknown.push((i, j + 1, word.clone(), spell + UNKNOWN_WORD_LOGP));
                        }
                    }
                    None => {}
                }
            }
        }
        // 辞書に無い英単語の候補のうち、かなに直せない英字がすべて内側の既知の
        // 英単語で説明できるものは作らない（「serverniaccess」= server＋に＋access
        // を丸ごと1つの未知語として飲み込まないように。kubectl は内側の bec が
        // c しか説明できないので残る）
        unknown.retain(|&(i, j, _, _)| {
            !(i..j)
                .filter(|&k| pieces[k].is_unconvertible())
                .all(|k| known.iter().any(|&(a, b, _, _)| i <= a && b <= j && a <= k && k < b))
        });
        known
            .into_iter()
            .chain(unknown)
            .map(|(i, j, word, logp)| EnglishSpan {
                start: kana_pos[i],
                end: kana_pos[j],
                word: self.display_form(&word),
                logp,
                continuation: false,
            })
            .collect()
    }

    /// Shift で大文字を1文字直接入力した直後の入力（Docker = D ＋ ocker、GitHub = G ＋
    /// it）で、先頭からの範囲を「その大文字で始まる英単語の残り」として読む候補。
    /// 語全体（docker・git）で頻度表を引き、無ければかなに直せない英字を含むときだけ
    /// 綴りで採点する（Aさん・Bの のように大文字の後に日本語が続く普通の書き方を
    /// 英単語にしないため）。`capital` が2文字以上（API 等の頭字語）なら何もしない。
    pub fn continuation_spans(&self, pieces: &[RomajiPiece], capital: &str) -> Vec<EnglishSpan> {
        let mut out = Vec::new();
        if capital.chars().count() != 1 || !capital.chars().all(|c| c.is_ascii_alphabetic()) {
            return out;
        }
        let mut rest = String::new();
        let mut kana_end = 0usize;
        let mut has_unconvertible = false;
        for p in pieces {
            if p.raw.is_empty() || !p.raw.chars().all(|c| c.is_ascii_alphabetic()) {
                break;
            }
            rest.push_str(&p.raw.to_ascii_lowercase());
            kana_end += p.kana.chars().count();
            has_unconvertible |= p.is_unconvertible();
            let word = format!("{}{}", capital.to_ascii_lowercase(), rest);
            if word.len() > MAX_WORD_LETTERS {
                break;
            }
            let logp = match self.word_logp(&word) {
                Some(lp) if word.len() >= MIN_WORD_LETTERS => Some(lp),
                None if has_unconvertible && word.len() >= MIN_WORD_LETTERS => {
                    let spell = self.spelling_logp(&word);
                    (spell / (word.len() + 1) as f32 >= UNKNOWN_MIN_CHAR_LOGP).then_some(spell + UNKNOWN_WORD_LOGP)
                }
                _ => None,
            };
            if let Some(logp) = logp {
                // 正しい表記があれば、直接入力した大文字の後ろをその表記から取る（G ＋ itHub）
                let form = self.display_form(&word);
                let rest_form = if form.len() == word.len() { form[capital.len()..].to_string() } else { rest.clone() };
                out.push(EnglishSpan { start: 0, end: kana_end, word: rest_form, logp, continuation: true });
            }
        }
        out
    }
}

/// 英単語の候補の語（名詞-一般 と 名詞-サ変接続 の2つ）。`logp` は語の
/// `cost` に 1/100 nats 単位で入れて運ぶ（`english_logp` で取り出す。英単語の
/// 辞書コストは判断層が別の定数で扱う）。
pub fn english_entries(span: &EnglishSpan, reading: &str) -> [WordEntry; 2] {
    let cost = (-span.logp * 100.0).round().clamp(0.0, i16::MAX as f32) as i16;
    let make = |id: PosId| WordEntry {
        surface: span.word.clone(),
        reading: reading.to_string(),
        left_id: id,
        right_id: id,
        cost,
        pos: if span.continuation { ENGLISH_CONTINUATION_POS } else { ENGLISH_POS }.to_string(),
    };
    [make(NOUN_GENERAL_ID), make(NOUN_SAHEN_ID)]
}

/// 続けて並んだ英単語の間に空白を入れる（pull・request → 「pull request」）。
/// 2語目の表記の先頭に半角空白を足す（読みはそのまま）。
pub fn space_between_english_words(entries: &mut [WordEntry]) {
    for i in 1..entries.len() {
        if is_english_entry(&entries[i - 1]) && is_english_entry(&entries[i]) && !entries[i].surface.starts_with(' ') {
            entries[i].surface.insert(0, ' ');
        }
    }
}

/// 英単語の候補の語か（大文字の続きを含む）
pub fn is_english_entry(e: &WordEntry) -> bool {
    is_english_pos(&e.pos)
}

/// 英単語の候補の品詞名か（大文字の続きを含む）
pub fn is_english_pos(pos: &str) -> bool {
    pos == ENGLISH_POS || pos == ENGLISH_CONTINUATION_POS
}

/// 英単語の候補の語の対数確率（`english_entries` が `cost` に入れたもの）
pub fn english_logp(e: &WordEntry) -> f32 {
    -(e.cost as f32) / 100.0
}

fn is_word(w: &str) -> bool {
    !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase())
}

fn padded(word: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(word.len() + 3);
    b.extend_from_slice(b"^^");
    b.extend(word.bytes().map(|c| c.to_ascii_lowercase()));
    b.push(b'$');
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RomajiConverter;

    fn lexicon() -> EnglishLexicon {
        EnglishLexicon::from_strs("the\t1000\ncode\t50\nmade\t80\ntest\t40\nmake\t60\n", "docker\n")
    }

    fn pieces(romaji: &str) -> Vec<RomajiPiece> {
        let conv = RomajiConverter::new();
        let chars: Vec<char> = romaji.chars().collect();
        let mut at = 0;
        conv.convert_aligned(romaji)
            .into_iter()
            .map(|(n, kana)| {
                let raw: String = chars[at..at + n].iter().collect();
                at += n;
                RomajiPiece { kana, raw }
            })
            .collect()
    }

    #[test]
    fn aligned_conversion_matches_plain_conversion() {
        let conv = RomajiConverter::new();
        for r in ["konocodewo", "kyouhagakkou", "testwo", "dockerwotukau", "kitte", "sannin", "ra-menn"] {
            let joined: String = conv.convert_aligned(r).into_iter().map(|(_, k)| k).collect();
            assert_eq!(joined, conv.convert(r), "{r}");
        }
        assert_eq!(conv.convert_aligned("tte"), vec![(1, "っ".to_string()), (2, "て".to_string())]);
    }

    #[test]
    fn finds_english_words_inside_romaji() {
        let lex = lexicon();
        // こ の こ で を → code は「こで」（文字位置 2..4）
        let spans = lex.spans(&pieces("konocodewo"));
        assert!(spans.iter().any(|s| s.word == "code" && (s.start, s.end) == (2, 4)), "{spans:?}");
        // 用語リストの語は頻度表に無くても頻度表の語として扱う
        assert!(lex.word_logp("docker").is_some());
    }

    #[test]
    fn unknown_words_need_unconvertible_letters() {
        // 頻度表に無い語は、かなに直せない英字を含めば候補になる（綴りの採点に
        // 英字の連鎖モデルを使うので、実際の頻度表で確かめる）
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dictionaries");
        let lex = EnglishLexicon::load_dir(&dir).expect("dictionaries/english_words.tsv");
        assert!(lex.word_logp("kubectl").is_some() || lex.spelling_logp("kubectl") > -30.0);
        let lex = EnglishLexicon::from_strs(&std::fs::read_to_string(dir.join("english_words.tsv")).unwrap(), "");
        assert!(lex.word_logp("kubectl").is_none());
        let spans = lex.spans(&pieces("kubectlwo"));
        assert!(spans.iter().any(|s| s.word == "kubectl"), "{spans:?}");
        // かなに直せる打鍵だけの並び（このこで）からは、頻度表に無い語の候補を作らない
        assert!(lex.spans(&pieces("konokodehanai")).iter().all(|s| lex.word_logp(&s.word).is_some()));
    }

    #[test]
    fn typo_leftovers_are_not_unknown_words() {
        // 打ち間違いで迷子の英字が1つだけ残った並びは英単語の候補にしない
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dictionaries");
        let lex = EnglishLexicon::from_strs(&std::fs::read_to_string(dir.join("english_words.tsv")).unwrap(), "");
        for typo in ["kaqsha", "seiqsaku", "gmennnasai", "seisakuk"] {
            let spans = lex.spans(&pieces(typo));
            assert!(spans.iter().all(|s| lex.word_logp(&s.word).is_some()), "{typo}: {spans:?}");
        }
    }

    #[test]
    fn unknown_span_does_not_swallow_known_words() {
        // 既知の英単語で説明できる英字を、未知語として丸ごと飲み込まない
        let lex = EnglishLexicon::from_strs("server\t50\naccess\t50\n", "");
        let spans = lex.spans(&pieces("serverniaccesssuru"));
        assert!(spans.iter().all(|s| s.word != "serverniaccess"), "{spans:?}");
        assert!(spans.iter().any(|s| s.word == "server") && spans.iter().any(|s| s.word == "access"));
    }

    #[test]
    fn rare_romaji_spellable_words_are_dropped() {
        // 打鍵がかなに直る語は上位 CONVERTIBLE_MAX_RANK 語まで。直らない語は順位を問わない
        let mut tsv = String::new();
        for i in 0..CONVERTIBLE_MAX_RANK {
            tsv.push_str(&format!("{}	{}
", filler(i), 100_000 - i));
        }
        tsv.push_str("mara	5
zzzt	5
database	5
");
        let lex = EnglishLexicon::from_strs(&tsv, "database
");
        assert!(lex.word_logp("mara").is_none());
        assert!(lex.word_logp("zzzt").is_some());
        assert!(lex.word_logp("database").is_some());
    }

    /// 重ならない英字だけの埋め草の語（順位を埋めるため）
    fn filler(i: usize) -> String {
        let mut n = i + 1;
        let mut w = String::from("q");
        while n > 0 {
            w.push((b'a' + (n % 26) as u8) as char);
            n /= 26;
        }
        w
    }

    #[test]
    fn capital_letter_continues_into_the_composition() {
        let lex = EnglishLexicon::from_strs("git	50
docker	50
san	50
", "");
        // G（直接入力）＋ it → git の残り「it」
        let spans = lex.continuation_spans(&pieces("itwo"), "G");
        assert!(spans.iter().any(|s| s.word == "it" && s.continuation && (s.start, s.end) == (0, 2)), "{spans:?}");
        assert!(lex.continuation_spans(&pieces("ockerwo"), "D").iter().any(|s| s.word == "ocker"));
        // 頭字語（API）の後は続きとして読まない
        assert!(lex.continuation_spans(&pieces("itwo"), "AP").is_empty());
    }

    #[test]
    fn tech_words_keep_their_written_casing() {
        let lex = EnglishLexicon::from_strs("code	50
", "GitHub kubectl
");
        let spans = lex.spans(&pieces("githubnipush"));
        assert!(spans.iter().any(|s| s.word == "GitHub"), "{spans:?}");
        assert_eq!(lex.display_form("kubectl"), "kubectl");
        // G を直接入力した後の「ithub」は GitHub の残り
        assert!(lex.continuation_spans(&pieces("ithub"), "G").iter().any(|s| s.word == "itHub"));
        // 文頭の大文字化は用語リストに無い普通の語だけ
        assert!(lex.capitalize_at_sentence_start("code"));
        assert!(!lex.capitalize_at_sentence_start("kubectl"));
        assert!(!lex.capitalize_at_sentence_start("GitHub"));
    }

    #[test]
    fn two_letter_words_are_never_candidates() {
        let lex = EnglishLexicon::from_strs("no\t1000\nto\t1000\n", "");
        assert!(lex.spans(&pieces("notowo")).iter().all(|s| s.word.len() >= 3));
    }

    #[test]
    fn spans_do_not_cross_symbols() {
        let lex = lexicon();
        // ー（-）をまたいだ範囲は英単語にしない
        assert!(lex.spans(&pieces("co-de")).iter().all(|s| s.word != "code"));
    }

    #[test]
    fn logp_survives_the_cost_round_trip() {
        let span = EnglishSpan { start: 0, end: 2, word: "code".into(), logp: -9.87, continuation: false };
        let [a, b] = english_entries(&span, "こで");
        assert!((english_logp(&a) - -9.87).abs() < 0.01);
        assert_ne!(a.left_id, b.left_id);
        assert!(is_english_entry(&a));
    }
}
