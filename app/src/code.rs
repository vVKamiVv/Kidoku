//! Text in a game's own program code. A wrong edit here stops the game, and a string can as well be a
//! file name or a key as text, so each engine says which lines plainly show text, and a string used
//! as anything else anywhere stays Japanese everywhere: it has to go on matching.

use crate::text::JA;
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// How a language writes its comments.
pub struct Syntax {
    pub line: &'static str,
    pub block: (&'static str, &'static str),
}

pub const RUBY: Syntax = Syntax { line: "#", block: ("=begin", "=end") };
pub const TJS: Syntax = Syntax { line: "//", block: ("/*", "*/") };
pub const GAMEEXE: Syntax = Syntax { line: ";", block: ("/*", "*/") };

static FORMAT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"%[-+0-9.]*[a-zA-Z]").unwrap());

/// Each quoted string in `code` as (start, end, start of its line, end of its line). A string that
/// runs over a line break isn't one a window shows, and judging lines alone keeps a stray quote in
/// a regular expression from throwing off the rest of the file.
fn literals(code: &str, syntax: &Syntax) -> Vec<(usize, usize, usize, usize)> {
    let (mut out, mut at, mut comment) = (Vec::new(), 0, false);
    for line in code.split_inclusive('\n') {
        let (b, mut open, mut i) = (line.as_bytes(), None, 0);
        while i < b.len() {
            if comment {
                let end = syntax.block.1.as_bytes();
                let Some(n) = b[i..].windows(end.len()).position(|w| w == end) else { break };
                i += n + end.len();
                comment = false;
                continue;
            }
            match (open, b[i]) {
                (None, _) if b[i..].starts_with(syntax.line.as_bytes()) => break,
                (None, _) if b[i..].starts_with(syntax.block.0.as_bytes()) => {
                    comment = true;
                    i += syntax.block.0.len();
                    continue;
                }
                (None, q @ (b'"' | b'\'')) => open = Some((q, i + 1)),
                (Some(_), b'\\') => i += 1,
                (Some((q, start)), c) if c == q => {
                    out.push((at + start, at + i, at, at + line.len()));
                    open = None;
                }
                _ => {}
            }
            i += 1;
        }
        at += line.len();
    }
    out
}

/// The strings across `sources` that are safe to change, as (source, start, end).
pub fn shown_strings(sources: &[String], syntax: &Syntax, shown: &Regex, not_shown: &Regex) -> Vec<(usize, usize, usize)> {
    let (mut found, mut keys) = (Vec::new(), HashSet::new());
    for (n, code) in sources.iter().enumerate() {
        for (start, end, from, to) in literals(code, syntax) {
            let (text, line) = (&code[start..end], code[from..to].trim_end());
            if !JA.is_match(text) {
                continue;
            }
            let embedded = start >= 2 && code.as_bytes()[start - 2] == b'@'; // TJS fills these in
            let plain = !embedded && !text.contains('\\') && !text.contains("#{") && !text.starts_with('*');
            if plain && shown.is_match(line) && !not_shown.is_match(line) {
                found.push((n, start, end));
            } else {
                keys.insert(text);
            }
        }
    }
    found.retain(|&(n, start, end)| !keys.contains(&sources[n][start..end]));
    found
}

/// `code` with English at `spots`, and how many took it. A reply that lost a `%s` the code fills in,
/// or has something that means more than text in a string, leaves its string as it is.
pub fn with_english(code: &str, spots: &[(usize, usize)], lookup: &dyn Fn(usize, &str) -> Option<String>) -> (String, usize) {
    let formats = |t: &str| FORMAT.find_iter(t).map(|m| m.as_str().to_string()).collect::<Vec<_>>();
    let (mut new, mut last, mut done) = (String::new(), 0, 0);
    for &(start, end) in spots {
        let old = &code[start..end];
        let Some(tr) = lookup(start, old).filter(|t| formats(t) == formats(old) && !t.contains(['\n', '#', '\\'])) else { continue };
        new.push_str(&code[last..start]);
        new.push_str(&if code.as_bytes()[start - 1] == b'"' { tr.replace('"', "'") } else { tr.replace('\'', "\\'") });
        last = end;
        done += 1;
    }
    new.push_str(&code[last..]);
    (new, done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(code: &str, syntax: &Syntax) -> Vec<String> {
        let (any, never) = (Regex::new("").unwrap(), Regex::new("$^").unwrap());
        let sources = [code.to_string()];
        shown_strings(&sources, syntax, &any, &never).iter().map(|&(_, s, e)| code[s..e].to_string()).collect()
    }

    #[test]
    fn comments_of_either_kind_hide_their_strings() {
        assert_eq!(found("a = \"表\" // \"裏\"\n/* \"隠す\"\n\"隠す\" */ b = \"出す\"\n", &TJS), ["表", "出す"]);
        assert_eq!(found("a = \"表\" # \"裏\"\n=begin\n\"隠す\"\n=end\nb = \"出す\"\n", &RUBY), ["表", "出す"]);
    }

    #[test]
    fn strings_that_stand_for_something_are_left_out() {
        assert!(found("a = @\"埋め込み&x;\"\n", &TJS).is_empty());
        assert!(found("a = \"*ラベル\"\n", &TJS).is_empty());
        assert!(found("a = \"行\\n二行\"\n", &TJS).is_empty());
    }

    #[test]
    fn english_is_quoted_for_its_string() {
        let code = "a = \"表\"; b = '裏'; c = \"残り%d\"\n";
        let spots: Vec<(usize, usize)> = literals(code, &TJS).iter().map(|l| (l.0, l.1)).collect();
        let (new, done) = with_english(code, &spots, &|_, t| Some(if t == "表" { "Say \"hi\"" } else if t == "裏" { "Don't" } else { "Left" }.to_string()));
        assert_eq!(new, "a = \"Say 'hi'\"; b = 'Don\\'t'; c = \"残り%d\"\n");
        assert_eq!(done, 2, "a lost %d keeps the Japanese");
    }
}
