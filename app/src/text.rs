//! Text helpers: Japanese detection, Ren'Py markup, and how much room text takes on screen.

use encoding_rs::SHIFT_JIS;
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;
use unicode_width::UnicodeWidthChar;

pub fn decoded(b: &[u8]) -> String {
    SHIFT_JIS.decode_without_bom_handling(b).0.into_owned()
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        pub static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}

// Kanji alone counts (設定, 終了): Chinese/Korean layers are excluded by language name instead.
re!(JA, r"[\x{3040}-\x{30ff}\x{3400}-\x{9fff}]");
re!(CJK, r"[\x{3040}-\x{30ff}\x{3400}-\x{9fff}\x{1100}-\x{11ff}\x{3130}-\x{318f}\x{ac00}-\x{d7af}]"); // incl. Hangul
re!(MARKUP, r"\{[^{}]*\}|\[[^\[\]]*\]");
// Unity: Smart String placeholders and rich-text tags; [brackets] are plain text there
re!(UNITY_MARKUP, r"\{[^{}]*\}|<[^<>]*>");
re!(VARS, r"\[[^\[\]]*\]");
re!(TAGS, r"\{[^{}]*\}");
re!(INTERP_FULL, r"\[([A-Za-z_][\w.]*)(?:!(\w+))?\]");
re!(SENTENCE_END, r#"(?:[.!?…]+["'”’)\]]*|\{[wp](?:=[\d.]+)?\})\s*"#);
re!(CLAUSE_END, r"[,;:—]\s+");
re!(TRAILING_PAUSE, r"(?:\s|\{[wp](?:=[\d.]+)?\})+$");
re!(TAG_TOKEN, r"\{(/?)(\w+)[^{}]*\}");
re!(CLOSE_TAG, r"\{/(\w+)");
// A chat model talking about the job instead of doing it. Characters apologize and refuse too
// ("Sorry.", "I can't."), so a match has to name the job, not just open like a refusal. Games say
// "request", "adult content" and "translate this ancient text" too, so those need the refusal around them.
// shortcut: the wording comes from how chat models usually refuse, not from logs, add what gets through
// shortcut: a line worded exactly like a refusal ("I cannot fulfill your request, my lady.") stays Japanese, accept that until a log shows one
re!(META_REPLY, concat!(
    r"(?i)\bas an ai\b|\bai assistant\b|\blanguage model\b|\bi(?:['’]?m| am) programmed\b",
    r"|\b(?:safety|content|ethical) (?:guidelines|polic)|\bmy guidelines\b",
    r"|\bi(?: can['’]?t| can ?not| won['’]?t|(?:['’]?m| am) unable to)\b[^.!?]*\b(?:explicit|sexual|adult) content\b",
    r"|\b(?:can['’]?t|can ?not|unable to) (?:translate(?: (?:this|that)(?: text| request)?)?\W*$|provide (?:a |the |any )?translation\b",
    r"|assist with (?:this|that)\b|(?:fulfill|comply with|help with) (?:this|that|your) request\b)",
    r"|^\W*(?:i['’]?m sorry,? but i can(?:['’]?t|not) help with (?:this|that)\b",
    r"|(?:sure|certainly|of course)\b.*\btranslation\b|here(?:['’]?s| is) (?:the|your|my) (?:\w+ )?translation\b|translation:)"
));

/// Same as renpy.translation.encode_say_string.
pub fn encode_say_string(s: &str) -> String {
    let s = s.replace('\\', "\\\\").replace('\n', "\\n").replace('"', "\\\"");
    let mut out = String::from("\"");
    let mut prev = None;
    for c in s.chars() {
        // A space after a space would be collapsed by Ren'Py's lexer, so it is escaped.
        if c == ' ' && prev == Some(' ') {
            out.push_str("\\ ");
        } else {
            out.push(c);
        }
        prev = Some(c);
    }
    out.push('"');
    out
}

/// The translation must keep the original's {tags} and [variables] in the same order.
pub fn markup_ok(src: &str, dst: &str) -> bool {
    same_markup(&MARKUP, src, dst)
}

/// Not empty, the same tags (as `markup` defines them) in the same order, and really a translation:
/// not the model talking, nor its source handed back. "……", "・・・" and "[na]" have nothing to translate.
pub fn same_markup(markup: &Regex, src: &str, dst: &str) -> bool {
    let copied = dst.trim() == src.trim() && JA.is_match(&markup.replace_all(src, "").replace(['・', 'ー'], ""));
    !dst.trim().is_empty() && !copied && !META_REPLY.is_match(&markup.replace_all(dst, ""))
        && markup.find_iter(src).map(|m| m.as_str()).eq(markup.find_iter(dst).map(|m| m.as_str()))
}

/// [na] -> [na!t]: Ren'Py then runs the value through the string table, so a Japanese
/// default shows in English while a name the player typed is shown unchanged.
pub fn add_t(text: &str, vars: &HashSet<String>) -> String {
    INTERP_FULL
        .replace_all(text, |c: &regex::Captures| {
            let flags = c.get(2).map_or("", |m| m.as_str());
            if vars.contains(&c[1]) && !flags.contains('t') {
                format!("[{}!{}t]", &c[1], flags)
            } else {
                c[0].to_string()
            }
        })
        .into_owned()
}

/// Display width in half-width units: a Japanese character counts as two letters.
pub fn text_units(s: &str) -> usize {
    let s = TAGS.replace_all(s, "");
    let s = VARS.replace_all(&s, "XXXXXXXX"); // [na] -> about a name
    s.chars().map(|c| if c.width() == Some(2) { 2 } else { 1 }).sum()
}

/// Punctuation and symbols as ASCII, for engines that only recognise two-byte characters on a
/// Japanese-locale Windows, where `…` would come out as garbage. Accented letters lose their accents.
pub fn ascii_punctuation(text: &str) -> String {
    text.trim()
        .replace("……", "…")
        .replace('…', "...")
        .replace('°', " deg")
        .chars()
        .map(|c| match c {
            '‘' | '’' => '\'',
            '“' | '”' | '「' | '」' | '『' | '』' => '"',
            '—' | '–' | '─' | '―' => '-',
            '。' => '.',
            '、' => ',',
            '☆' | '★' | '※' | '●' | '◆' | '■' => '*',
            '♪' => '~',
            '○' => 'o',
            '×' => 'x',
            '→' => '>',
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'A',
            'Ç' => 'C',
            'È' | 'É' | 'Ê' | 'Ë' => 'E',
            'Ì' | 'Í' | 'Î' | 'Ï' => 'I',
            'Ñ' => 'N',
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => 'O',
            'Ù' | 'Ú' | 'Û' | 'Ü' => 'U',
            'Ý' => 'Y',
            '\u{3000}' => ' ',
            '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap(), // full-width ASCII
            c => c,
        })
        .collect()
}

/// Shift-JIS bytes; what it can't hold becomes '?'.
pub fn sjis(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for c in text.chars() {
        let mut buf = [0u8; 4];
        let (bytes, _, unmappable) = SHIFT_JIS.encode(c.encode_utf8(&mut buf));
        if unmappable { out.push(b'?') } else { out.extend_from_slice(&bytes) }
    }
    out
}

/// Whether a speaker field holds a name and not a line of text that landed there. `？？？` is a name;
/// a sentence has sentence marks, or runs too long.
pub fn looks_like_name(s: &str) -> bool {
    s.chars().count() <= 16 && !s.contains(['、', '。', '…', '‥', '「', '」', '『', '』']) && !(JA.is_match(s) && s.contains(['！', '？', '!', '?', '―', '─']))
}

/// Break at spaces so no line is over `max` letters, for engines that would cut words at the box's
/// edge; line breaks already in `text` stay.
pub fn wrap_words(text: &str, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > max {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

/// Lines `text` takes word-wrapped at `units`; Japanese wraps anywhere, like one long word.
pub fn wrap_lines(text: &str, units: usize) -> usize {
    let mut total = 0;
    for para in text.split('\n') {
        let (mut n, mut cur) = (1, 0);
        for word in para.split(' ') {
            let w = text_units(word);
            if cur > 0 && cur + 1 + w <= units {
                cur += 1 + w;
                continue;
            }
            if cur > 0 {
                n += 1;
            }
            n += w.saturating_sub(1) / units;
            cur = if !w.is_multiple_of(units) { w % units } else if w > 0 { units } else { 0 };
        }
        total += n;
    }
    total
}

/// Leading newlines are layout (e.g. pushing NVL text below a frame); English keeps the original's.
pub fn leading_newlines(japanese: &str) -> &str {
    &japanese[..japanese.len() - japanese.trim_start_matches('\n').len()]
}

/// Split an English line that needs more lines than the box has at sentence ends
/// (then clauses), never inside paired tags like {i}...{/i}.
pub fn split_to_fit(text: &str, units: Option<usize>, max_lines: usize) -> Vec<String> {
    let Some(units) = units else { return vec![text.to_string()] };
    if max_lines == 0 || wrap_lines(text, units) <= max_lines {
        return vec![text.to_string()];
    }
    let paired: HashSet<&str> = CLOSE_TAG.captures_iter(text).map(|c| c.get(1).unwrap().as_str()).collect();
    let tags: Vec<(usize, bool, &str)> = TAG_TOKEN
        .captures_iter(text)
        .map(|c| (c.get(0).unwrap().end(), !c[1].is_empty(), c.get(2).unwrap().as_str()))
        .collect();
    let depth = |pos: usize| -> i64 {
        tags.iter()
            .filter(|t| t.0 <= pos)
            .map(|&(_, close, name)| if close { -1 } else { paired.contains(name) as i64 })
            .sum()
    };
    let pieces = |s: &str, re: &Regex, base: usize| -> Vec<String> {
        let (mut out, mut start) = (Vec::new(), 0);
        for m in re.find_iter(s) {
            if m.end() < s.len() && depth(base + m.end()) == 0 {
                out.push(s[start..m.end()].to_string());
                start = m.end();
            }
        }
        out.push(s[start..].to_string());
        out
    };

    let (mut parts, mut off) = (Vec::new(), 0);
    for p in pieces(text, &SENTENCE_END, 0) {
        if wrap_lines(&p, units) > max_lines {
            parts.extend(pieces(&p, &CLAUSE_END, off));
        } else {
            parts.push(p.clone());
        }
        off += p.len();
    }
    let (mut out, mut cur) = (Vec::new(), String::new());
    for p in parts {
        let joined = format!("{cur}{p}");
        if !cur.is_empty() && wrap_lines(&TRAILING_PAUSE.replace(&joined, ""), units) > max_lines {
            out.push(std::mem::replace(&mut cur, p));
        } else {
            cur = joined;
        }
    }
    out.push(cur);
    out.iter()
        .map(|c| TRAILING_PAUSE.replace(c, "").trim_start_matches(' ').to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_told_from_lines_of_text() {
        for name in ["太郎", "？？？", "クラスメイトA", "ＣＭの声"] {
            assert!(looks_like_name(name), "{name}");
        }
        for line in ["邪魔になるから……", "――ピンポーン", "待って！お願い", "帰りますね。", "本当に？", "これは十七文字もある長い文章になっています"] {
            assert!(!looks_like_name(line), "{line}");
        }
    }

    #[test]
    fn say_string_quoting() {
        assert_eq!(encode_say_string("a \"b\"\\\nc  d"), r#""a \"b\"\\\nc \ d""#);
    }

    #[test]
    fn markup() {
        assert!(markup_ok("{i}あ{/i}[name]", "{i}Ah{/i} [name]"));
        assert!(!markup_ok("あ", "[sigh] Ah"));
        assert!(!markup_ok("あ", "  "));
    }

    #[test]
    fn replies_that_are_no_translation() {
        for bad in ["I'm sorry, but I can't help with that.", "I cannot fulfill this request.", "As an AI, I must decline.",
                    "Sure! Here is the translation:", "Here's the English translation", "\"I can’t translate this.\"", "1あ",
                    "I cannot generate explicit content.", "I apologize, but I cannot provide a translation of this text.",
                    "I'm sorry, but this goes against my guidelines."] {
            assert!(!markup_ok("1あ", bad), "{bad}");
        }
        assert!(!markup_ok("{i}あ{/i}", "{i}I'm sorry, but I can't help with that.{/i}"));
        for fine in ["Sorry.", "I'm sorry.", "I can't.", "Sure.", "Sure!", "Here's your tea.", "I'm sorry, I can't go with you.",
                     "I cannot fulfill that promise.", "I can't help with that.", "This game contains sexual content. 18+ only.",
                     "Minors cannot view this adult content.", "Sorry, but this request has already been taken.",
                     "Sorry for the sudden request.", "I can't translate this ancient text."] {
            assert!(markup_ok("あ", fine), "{fine}");
        }
        for same in ["……", "！？", "・・・", "ーー！", "[名前]"] {
            assert!(markup_ok(same, same), "{same}"); // nothing in it to translate
        }
    }

    #[test]
    fn name_variables_get_t() {
        let vars: HashSet<String> = ["na".to_string()].into();
        assert_eq!(add_t("[na], [count] [na!u] [na!t]", &vars), "[na!t], [count] [na!ut] [na!t]");
    }

    #[test]
    fn widths_and_wrapping() {
        assert_eq!(text_units("あいう abc"), 10);
        assert_eq!(text_units("{i}ab{/i}"), 2);
        assert_eq!(wrap_lines("aaaa bbbb cccc", 9), 2);
        assert_eq!(wrap_lines("あいうえおかきくけこ", 10), 2);
    }

    #[test]
    fn splits_at_sentences_never_inside_tags() {
        let long = "One two three. Four five six! {i}Seven eight. Nine ten.{/i} Eleven{w} twelve.";
        let parts = split_to_fit(long, Some(16), 2);
        assert_eq!(parts, ["One two three. Four five six!", "{i}Seven eight. Nine ten.{/i} Eleven", "twelve."]);
        assert!(parts.iter().all(|p| p.matches("{i}").count() == p.matches("{/i}").count()));
        assert_eq!(split_to_fit("Short.", Some(16), 2), ["Short."]);
    }
}
