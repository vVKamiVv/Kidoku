//! NScripter (nscript.dat): a script of plain lines, each byte xored. A line starting with a Japanese
//! character is text and every such line is a row on screen; `\` waits and starts a new page. Lines
//! of English start with a backquote, since one starting with a letter would be a command.

use crate::pipeline::Line;
use crate::text::{ascii_punctuation, looks_like_name, sjis, wrap_words, JA};
use crate::{original, replace_file, R};
use encoding_rs::SHIFT_JIS;
use regex::Regex;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

const SCRIPT: &str = "nscript.dat";
const KEY: u8 = 0x84;
/// `setwindow` gives letters and rows of a box; a game without one gets the usual size.
const DEFAULT_BOX: (usize, usize) = (23, 3);
/// Commands whose quoted strings are shown. Those that take a list go on over the next lines.
const SHOWN: &[&str] = &["select", "selgosub", "selnum", "rmenu"];

/// Text waits and colours, and values shown in the text.
pub static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"![swd]\d+|!s[df]?|#[0-9a-fA-F]{6}|[%$]\w+").unwrap());
/// `(base/reading)`
static RUBY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(([^()/]*)/[^()]*\)").unwrap());
static WINDOW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*setwindow\s+\d+\s*,\s*\d+\s*,\s*(\d+)\s*,\s*(\d+)").unwrap());
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]*)""#).unwrap());
/// A name in front of a quote, as these scripts write speech.
static SPOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([^「『%$!#]{1,12})[「『](.*)[」』]$").unwrap());

struct Row {
    /// Without its line feed.
    raw: Vec<u8>,
    text: String,
}

struct Message {
    /// Its lines, first to last.
    first: usize,
    last: usize,
    body: String,
    /// Who says it, when it's written `name「body」`.
    who: Option<String>,
    /// `\` if it waits at the end.
    end: &'static str,
}

impl Message {
    fn new(first: usize, last: usize, body: String, end: &'static str) -> Message {
        let said = SPOKEN.captures(&body).filter(|c| looks_like_name(&c[1])).map(|c| (c[1].to_string(), c[2].to_string()));
        match said {
            Some((who, text)) => Message { first, last, body: text, who: Some(who), end },
            None => Message { first, last, body, who: None, end },
        }
    }

    /// One that lost its name to the speaker field has a key of its own, as a translation saved
    /// before that still holds the name.
    fn key(&self) -> String {
        if self.who.is_some() { format!("{}:said", self.first) } else { self.first.to_string() }
    }
}

/// A quoted string to translate: its line and where its text sits in the line.
struct Quoted {
    line: usize,
    start: usize,
    end: usize,
}

pub struct Game {
    lines: Vec<Row>,
    messages: Vec<Message>,
    quoted: Vec<Quoted>,
    /// Half-width letters across, and rows.
    letters: usize,
    rows: usize,
}

fn is_text(t: &str) -> bool {
    t.starts_with('`') || t.chars().next().is_some_and(|c| !c.is_ascii())
}

/// A line's text without what only tells the engine how to show it.
fn clean(t: &str) -> String {
    let t = t.trim().trim_start_matches('`');
    let t = t.trim_end_matches(['\\', '/', '@']);
    RUBY.replace_all(t, "$1").replace(['_', '\\', '@'], "")
}

/// These mean something to the engine even inside English.
fn safe(text: &str) -> String {
    let mut out = ascii_punctuation(text);
    for (plain, wide) in [("\\", "￥"), ("@", "＠"), ("/", "／"), ("`", "'"), ("_", " ")] {
        out = out.replace(plain, wide);
    }
    out
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let plain: Vec<u8> = original(&game.join(SCRIPT))?.iter().map(|b| b ^ KEY).collect();
        let lines: Vec<Row> = plain.split(|&c| c == b'\n').map(|l| Row { raw: l.to_vec(), text: SHIFT_JIS.decode_without_bom_handling(l).0.trim_end_matches('\r').to_string() }).collect();
        let (mut messages, mut quoted) = (Vec::new(), Vec::new());
        let (mut box_size, mut list) = (DEFAULT_BOX, false);
        let mut n = 0;
        while n < lines.len() {
            let t = lines[n].text.trim();
            if let Some(c) = WINDOW.captures(&lines[n].text) {
                box_size = (c[1].parse().unwrap_or(DEFAULT_BOX.0), c[2].parse().unwrap_or(DEFAULT_BOX.1));
            }
            let word = t.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').next().unwrap_or("").to_lowercase();
            list = (list && t.starts_with('"')) || SHOWN.contains(&word.as_str());
            if list {
                for c in QUOTED.captures_iter(&lines[n].text).map(|c| c.get(1).unwrap()).filter(|c| JA.is_match(c.as_str())) {
                    quoted.push(Quoted { line: n, start: c.start(), end: c.end() });
                }
                list = t.ends_with(',');
            }
            if !is_text(t) {
                n += 1;
                continue;
            }
            let first = n;
            let (mut body, mut end) = (String::new(), "");
            while n < lines.len() && is_text(lines[n].text.trim()) {
                let line = lines[n].text.trim();
                body.push_str(&clean(line));
                n += 1;
                if line.ends_with('\\') {
                    end = "\\";
                    break;
                }
            }
            if JA.is_match(&body) {
                messages.push(Message::new(first, n - 1, body, end));
            }
        }
        Ok(Game { lines, messages, quoted, letters: crate::fit::letters(game, box_size.0 * 2), rows: crate::fit::rows(game, box_size.1) })
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out: Vec<Line> = self.messages.iter().map(|m| (m.key(), m.who.clone(), m.body.clone())).collect();
        out.extend(self.quoted.iter().map(|q| (format!("{}:{}", q.line, q.start), None, self.lines[q.line].text[q.start..q.end].to_string())));
        out
    }

    /// A message broken into rows, a page's wait after each full box.
    fn rows_of(&self, text: &str, end: &str) -> String {
        let rows = wrap_words(&safe(text), self.letters);
        let last = rows.len() - 1;
        let lines: Vec<String> = rows.iter().enumerate().map(|(i, r)| {
            let wait = if i == last { end } else if (i + 1) % self.rows == 0 { "\\" } else { "" };
            format!("`{r}{wait}")
        }).collect();
        lines.join("\n")
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the script and reads
/// it back. Returns how many texts were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let mut new: Vec<Option<Vec<u8>>> = g.lines.iter().map(|_| None).collect();
    let mut done = 0;
    for m in &g.messages {
        let Some(tr) = line(&m.key()) else { continue };
        (m.first..=m.last).for_each(|n| new[n] = Some(Vec::new()));
        let text = m.who.as_ref().map_or_else(|| tr.clone(), |who| format!("{} \"{}\"", name(who).unwrap_or_else(|| who.clone()), tr.trim_matches('"')));
        new[m.first] = Some(sjis(&g.rows_of(&text, m.end)));
        done += 1;
    }
    // from the far end of each line, so earlier offsets hold
    for q in g.quoted.iter().rev() {
        let Some(tr) = line(&format!("{}:{}", q.line, q.start)) else { continue };
        let row = new[q.line].take().map_or_else(|| g.lines[q.line].text.clone(), |b| SHIFT_JIS.decode_without_bom_handling(&b).0.into_owned());
        let spliced = format!("{}`{}{}", &row[..q.start], safe(&tr).replace('"', "'"), &row[q.end..]);
        new[q.line] = Some(sjis(&spliced));
        done += 1;
    }
    let mut plain = Vec::new();
    for (i, (row, changed)) in g.lines.iter().zip(&new).enumerate() {
        if i > 0 {
            plain.push(b'\n');
        }
        plain.extend(changed.as_ref().unwrap_or(&row.raw));
    }
    let bytes: Vec<u8> = plain.iter().map(|b| b ^ KEY).collect();
    let path = game.join(SCRIPT);
    replace_file(&path, &bytes)?;
    if fs::read(&path).map_err(|e| e.to_string())? != bytes {
        return Err(format!("{SCRIPT} didn't read back as written"));
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn game(script: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-nscripter-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SCRIPT), sjis(script).iter().map(|b| b ^ KEY).collect::<Vec<u8>>()).unwrap();
        dir
    }

    fn written(dir: &Path) -> String {
        let b: Vec<u8> = fs::read(dir.join(SCRIPT)).unwrap().iter().map(|b| b ^ KEY).collect();
        SHIFT_JIS.decode_without_bom_handling(&b).0.into_owned()
    }

    const SCRIPT_TEXT: &str = ";comment\n*define\nsetwindow 120,470,10,2,40,40,0,20\ncaption \"題名\"\ngame\n*start\nbg \"a.jpg\",1:wait 100\n一行目の文、_(漢字/かんじ)です\n二行目の文です\\\nselect \"はい\",*yes,\n\"いいえ\",*no\n地の文です\\\n";

    #[test]
    fn rows_up_to_a_wait_are_one_message_and_commands_are_not_text() {
        let dir = game(SCRIPT_TEXT);
        let g = Game::open(&dir).unwrap();
        assert_eq!(g.lines().iter().map(|l| l.2.as_str()).collect::<Vec<_>>(), ["一行目の文、漢字です二行目の文です", "地の文です", "はい", "いいえ"]);
        assert_eq!((g.letters, g.rows), (20, 2));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn english_starts_with_a_backquote_and_long_text_goes_on_to_a_new_page() {
        let dir = game(SCRIPT_TEXT);
        let lines = Game::open(&dir).unwrap().lines();
        let keys: Vec<_> = lines.iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.iter().position(|x| x == k).map(|i| match i {
            0 => "one two three four five six seven eight nine ten eleven twelve".to_string(),
            1 => "Say @ / \\".to_string(),
            2 => "Yes".to_string(),
            _ => "No".to_string(),
        });
        assert_eq!(install(&dir, &line, &|_| None).unwrap(), 4);
        let text = written(&dir);
        assert!(text.contains("`one two three four\n`five six seven eight\\\n`nine ten eleven\n`twelve\\\n"), "{text}");
        assert!(text.contains("`Say ＠ ／ ￥\\\n"), "{text}");
        assert!(text.contains("select \"`Yes\",*yes,\n\"`No\",*no"), "{text}");
        assert!(text.contains("bg \"a.jpg\",1:wait 100\n"));

        let first = fs::read(dir.join(SCRIPT)).unwrap();
        install(&dir, &line, &|_| None).unwrap();
        assert_eq!(fs::read(dir.join(SCRIPT)).unwrap(), first);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_name_in_front_of_a_quote_is_the_speaker() {
        let dir = game("*start\n太郎「こんにちは」\\\n彼は「はい」と言った\\\n朝、「はい」\\\n");
        let lines = Game::open(&dir).unwrap().lines();
        let shown: Vec<_> = lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect();
        assert_eq!(shown, [(Some("太郎"), "こんにちは"), (None, "彼は「はい」と言った"), (None, "朝、「はい」")], "only a name can speak");
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        install(&dir, &|k| (k == "1:said").then(|| "\"Hello\"".to_string()), &name).unwrap();
        assert!(written(&dir).contains("*start\n`Taro \"Hello\"\\\n彼は"), "{}", written(&dir));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_untranslated_script_comes_back_the_same() {
        let dir = game(SCRIPT_TEXT);
        install(&dir, &|_| None, &|_| None).unwrap();
        assert_eq!(written(&dir), SCRIPT_TEXT);
        let _ = fs::remove_dir_all(dir);
    }
}
