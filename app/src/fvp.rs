//! FVP engine (Favorite, *.hcb): one file of bytecode with text inline, so a longer line moves all
//! that follows and every jump has to follow it. Nothing marks a string as text, so text is told by
//! who receives it: the function most Japanese goes to, and functions that print what they're given.
//! Strings compared, joined or stored are the game's own business and stay.

use crate::word;
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, text_units, wrap_words, JA};
use crate::{original, replace_file, R};
use encoding_rs::SHIFT_JIS;
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// shortcut: a guess from the test game's longest line over three rows; change it if lines break
/// inside words or stop short of the box's edge.
const LINE_LETTERS: usize = 52;
/// Longer strings are name plates no more. No plate holds more than this.
const NAME_UNITS: usize = 24;
/// A string's length is one byte, NUL included.
const LONGEST: usize = 254;

const INIT_STACK: u8 = 1;
const CALL: u8 = 2;
const SYSCALL: u8 = 3;
const JUMP: u8 = 6;
const JUMP_IF_ZERO: u8 = 7;
const PUSH_NIL: u8 = 8;
const PUSH_STRING: u8 = 14;
const PUSH_LOCAL: u8 = 16;
const ADD: u8 = 26;

/// `[reading|base]`
static RUBY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]|]*\|([^\]]*)\]").unwrap());

fn half(b: &[u8], p: usize) -> usize {
    u16::from_le_bytes(b[p..p + 2].try_into().unwrap()) as usize
}

/// Bytes after an opcode; strings carry their own length.
fn operand_size(op: u8) -> Option<usize> {
    Some(match op {
        0 | 4 | 5 | 8 | 9 | 19 | 20 | 25..=39 => 0,
        12 | 16 | 18 | 22 | 24 => 1,
        1 | 3 | 11 | 15 | 17 | 21 | 23 => 2,
        2 | 6 | 7 | 10 | 13 => 4,
        _ => return None,
    })
}

/// A text the game shows.
struct Text {
    /// The position of its push instruction, its key.
    at: usize,
    who: Option<String>,
}

pub struct Game {
    path: PathBuf,
    code: Vec<u8>,
    /// Offset of the part after the code: entry point, globals, syscall names.
    desc: usize,
    /// Positions of addresses in the code.
    addresses: Vec<usize>,
    texts: Vec<Text>,
    names: Vec<usize>,
}

fn string_at(b: &[u8], at: usize) -> String {
    let raw = &b[at + 2..at + 2 + b[at + 1] as usize];
    SHIFT_JIS.decode_without_bom_handling(raw.strip_suffix(&[0]).unwrap_or(raw)).0.into_owned()
}

/// Which syscall number prints text.
fn print_call(b: &[u8], desc: usize) -> Option<usize> {
    let mut p = desc + 10;
    p += 1 + *b.get(p)? as usize;
    let count = half(b.get(..p + 2)?, p);
    p += 2;
    for i in 0..count {
        let len = *b.get(p + 1)? as usize;
        if b.get(p + 2..p + 2 + len)?.starts_with(b"TextPrint\0") {
            return Some(i);
        }
        p += 2 + len;
    }
    None
}

/// Where the code ends, its instructions as `(offset, opcode)`, and where its addresses sit.
type Walked = (usize, Vec<(usize, u8)>, Vec<usize>);

/// Every instruction as `(offset, opcode)`, with where the code ends and where its addresses sit.
/// An error unless each one, and each address, lands right.
fn walk(code: &[u8]) -> R<Walked> {
    let unknown = "the code doesn't walk cleanly, so this FVP version isn't supported.";
    let desc = word(code.get(..4).ok_or(unknown)?, 0);
    if desc + 4 > code.len() {
        return Err(unknown.into());
    }
    let (mut ops, mut at, mut addresses) = (Vec::new(), 4, Vec::new());
    while at < desc {
        let op = code[at];
        ops.push((at, op));
        at += 1 + match op {
            PUSH_STRING => 1 + *code.get(at + 1).ok_or(unknown)? as usize,
            _ => operand_size(op).ok_or(unknown)?,
        };
        if matches!(op, CALL | JUMP | JUMP_IF_ZERO) && at <= desc {
            addresses.push(at - 4);
        }
    }
    let starts: HashSet<usize> = ops.iter().map(|o| o.0).collect();
    if at != desc || !addresses.iter().all(|&p| starts.contains(&word(code, p))) || !starts.contains(&word(code, desc)) {
        return Err(unknown.into());
    }
    Ok((desc, ops, addresses))
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let path = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("hcb"))).ok_or("No .hcb script in this folder.")?;
        Game::read(path.clone(), original(&path)?)
    }

    fn read(path: PathBuf, code: Vec<u8>) -> R<Game> {
        let (desc, ops, addresses) = walk(&code)?;

        // who takes each Japanese string as an argument
        let takers: Vec<(usize, usize)> = ops.iter().enumerate().filter(|(_, o)| o.1 == PUSH_STRING && JA.is_match(&string_at(&code, o.0))).filter_map(|(i, _)| {
            let n = (i + 1..ops.len()).find(|&n| !(PUSH_NIL..=PUSH_LOCAL).contains(&ops[n].1))?;
            let target = (ops[n].1 == CALL).then(|| word(&code, ops[n].0 + 1))?;
            (code[target] == INIT_STACK && n - i <= code[target + 1] as usize).then_some((i, target))
        }).collect();
        let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
        takers.iter().for_each(|t| *counts.entry(t.1).or_default() += 1);
        let main = counts.iter().max_by_key(|c| *c.1).map(|c| *c.0).ok_or("No text calls found in this FVP script.")?;

        // which function each instruction sits in, and the ones that print
        let print = print_call(&code, desc).ok_or("This FVP script has no TextPrint call.")?;
        let mut function = vec![0; ops.len()];
        let mut printers = HashSet::new();
        let mut current = 0;
        for (i, &(at, op)) in ops.iter().enumerate() {
            if op == INIT_STACK {
                current = at;
            }
            function[i] = current;
            if op == SYSCALL && half(&code, at + 1) == print {
                printers.insert(current);
            }
        }

        // a printer of short strings only is the name plate
        let plates: HashSet<usize> = printers.iter().copied().filter(|&p| {
            let mut sizes = takers.iter().filter(|t| t.1 == p).map(|t| text_units(&string_at(&code, ops[t.0].0))).peekable();
            sizes.peek().is_some() && sizes.all(|u| u <= NAME_UNITS)
        }).collect();
        let mut speaker: HashMap<usize, String> = HashMap::new();
        let mut names = Vec::new();
        let (mut shown, mut taken) = (Vec::new(), HashMap::new());
        for &(i, target) in &takers {
            if plates.contains(&target) {
                names.push(ops[i].0);
                speaker.insert(function[i], string_at(&code, ops[i].0));
            } else if !string_at(&code, ops[i].0).contains('.') {
                // whatever else takes Japanese shows it too, as save titles do. A dot means a file name
                shown.push(i);
                taken.insert(i, target);
            }
        }

        // the speaker is whoever was called last before a line
        let (mut texts, mut who, mut next) = (Vec::new(), None, 0);
        for (i, &(at, op)) in ops.iter().enumerate() {
            if op == CALL
                && let Some(name) = speaker.get(&word(&code, at + 1)) {
                who = Some(name.clone());
            }
            if shown.get(next) == Some(&i) {
                next += 1;
                let name = if taken[&i] == main { who.take() } else { None };
                texts.push(Text { at, who: name });
            }
        }
        Ok(Game { path, code, desc, addresses, texts, names })
    }

    pub fn lines(&self) -> Vec<Line> {
        self.texts.iter().map(|t| (t.at.to_string(), t.who.clone(), RUBY.replace_all(&string_at(&self.code, t.at), "$1").into_owned())).collect()
    }
}

/// A push of `text`. The length is one byte, so past that it's pushed in parts and added up, as the
/// game itself does with strings.
fn push(text: &str) -> Vec<u8> {
    let (mut parts, mut part) = (Vec::new(), Vec::new());
    for c in text.chars() {
        let bytes = sjis(c.encode_utf8(&mut [0; 4]));
        if part.len() + bytes.len() > LONGEST {
            parts.push(std::mem::take(&mut part));
        }
        part.extend(bytes);
    }
    parts.push(part);
    let mut out = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        out.extend([PUSH_STRING, p.len() as u8 + 1]);
        out.extend(p);
        out.push(0);
        if i > 0 {
            out.push(ADD);
        }
    }
    out
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the script and reads
/// it back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let width = crate::fit::letters(game, LINE_LETTERS);
    let mut edits: Vec<(usize, Vec<u8>)> = Vec::new();
    for t in &g.texts {
        if let Some(tr) = line(&t.at.to_string()) {
            edits.push((t.at, push(&wrap_words(&ascii_punctuation(&tr), width).join("\n"))));
        }
    }
    for &at in &g.names {
        if let Some(en) = name(&string_at(&g.code, at)) {
            edits.push((at, push(&en)));
        }
    }
    edits.sort_by_key(|e| e.0);
    let mut out = Vec::with_capacity(g.code.len());
    let (mut last, mut shift, mut shifts) = (0, 0isize, Vec::new());
    for (at, bytes) in &edits {
        let end = at + 2 + g.code[at + 1] as usize;
        out.extend(&g.code[last..*at]);
        out.extend(bytes);
        last = end;
        shift += bytes.len() as isize - (end - at) as isize;
        shifts.push((*at, shift));
    }
    out.extend(&g.code[last..]);
    let moved = |old: usize| old.wrapping_add_signed(shifts.partition_point(|s| s.0 < old).checked_sub(1).map_or(0, |i| shifts[i].1));
    for &p in g.addresses.iter().chain([&g.desc]) {
        let at = moved(p);
        out[at..at + 4].copy_from_slice(&(moved(word(&g.code, p)) as u32).to_le_bytes());
    }
    out[..4].copy_from_slice(&(moved(g.desc) as u32).to_le_bytes());
    replace_file(&g.path, &out)?;
    let back = fs::read(&g.path).map_err(|e| e.to_string())?;
    if back != out || walk(&back)?.2.len() != g.addresses.len() {
        return Err(format!("{} didn't read back as written", g.path.display()));
    }
    Ok(edits.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text goes to a function that queues it, a name to one that prints it, a compared string stays.
    struct Script {
        b: Vec<u8>,
        patches: Vec<(usize, usize)>,
    }

    impl Script {
        fn new() -> Script {
            Script { b: vec![0; 4], patches: Vec::new() }
        }

        fn string(&mut self, t: &str) {
            let raw = sjis(t);
            self.b.extend([PUSH_STRING, raw.len() as u8 + 1]);
            self.b.extend(raw);
            self.b.push(0);
        }

        fn nils(&mut self, n: usize) {
            self.b.extend(vec![PUSH_NIL; n]);
        }

        fn function(&mut self, args: u8) -> usize {
            let at = self.b.len();
            self.b.extend([INIT_STACK, args, 0]);
            at
        }

        fn to(&mut self, op: u8, target: usize) {
            self.b.push(op);
            self.b.extend((target as u32).to_le_bytes());
        }

        fn jump_later(&mut self, op: u8) -> usize {
            self.b.extend([op, 0, 0, 0, 0]);
            self.b.len() - 4
        }
    }

    fn hcb() -> Vec<u8> {
        let mut s = Script::new();
        let queue = s.function(4);
        s.b.push(4);
        let plate = s.function(1);
        s.b.extend([SYSCALL, 0, 0, 4]);
        let long_print = s.function(1);
        s.b.extend([SYSCALL, 0, 0, 4]);
        let taro = s.function(0);
        s.string("太郎");
        s.to(CALL, plate);
        s.b.push(4);
        let entry = s.function(0);
        s.to(CALL, taro);
        s.string("こんにちは");
        s.nils(3);
        s.to(CALL, queue);
        let over = s.jump_later(JUMP);
        s.string("地の文です[かな|漢字]");
        s.nils(3);
        s.to(CALL, queue);
        s.string("これは名札にならない長さの文章です");
        s.to(CALL, long_print);
        let land = s.b.len();
        s.patches.push((over, land));
        s.string("比較用");
        s.string("比較用");
        s.b.push(34);
        s.b.push(4);
        let desc = s.b.len();
        s.b[..4].copy_from_slice(&(desc as u32).to_le_bytes());
        for (p, to) in s.patches.clone() {
            s.b[p..p + 4].copy_from_slice(&(to as u32).to_le_bytes());
        }
        s.b.extend((entry as u32).to_le_bytes());
        s.b.extend([0; 6]);
        s.b.push(0); // title
        s.b.extend(1u16.to_le_bytes());
        s.b.extend([1, 10]);
        s.b.extend(b"TextPrint\0");
        s.b.extend(0u16.to_le_bytes());
        s.b
    }

    fn game() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-fvp-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("game.hcb"), hcb()).unwrap();
        dir
    }

    #[test]
    fn text_is_told_by_who_takes_it() {
        let dir = game();
        let lines = Game::open(&dir).unwrap().lines();
        let shown: Vec<_> = lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect();
        assert_eq!(shown, [(Some("太郎"), "こんにちは"), (None, "地の文です漢字"), (None, "これは名札にならない長さの文章です")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn jumps_follow_the_text_and_it_reads_back() {
        let dir = game();
        let keys: Vec<_> = Game::open(&dir).unwrap().lines().iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.iter().position(|x| x == k).map(|i| format!("English line number {i}"));
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 4);

        let path = dir.join("game.hcb");
        let written = fs::read(&path).unwrap();
        let (desc, ops, addresses) = walk(&written).unwrap();
        let strings: Vec<_> = ops.iter().filter(|o| o.1 == PUSH_STRING).map(|o| string_at(&written, o.0)).collect();
        assert_eq!(strings[0], "Taro");
        let jump = *addresses.iter().find(|&&p| written[p - 1] == JUMP).unwrap();
        assert_eq!(string_at(&written, word(&written, jump)), "比較用", "the jump lands where it did");
        assert_eq!(written[word(&written, desc)], INIT_STACK, "so does the entry point");

        install(&dir, &line, &name).unwrap();
        assert_eq!(fs::read(&path).unwrap(), written);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_text_too_long_for_one_string_is_pushed_in_parts() {
        let code = push(&"word ".repeat(80));
        assert_eq!(code[0], PUSH_STRING);
        assert_eq!(code[1] as usize, LONGEST + 1);
        assert_eq!(*code.last().unwrap(), ADD);
        assert_eq!(code.iter().filter(|&&c| c == PUSH_STRING).count(), 2);
    }

    #[test]
    fn code_that_does_not_walk_is_refused() {
        let mut b = hcb();
        let desc = word(&b, 0);
        b[desc - 1] = 0xEE;
        assert!(Game::read(PathBuf::from("x.hcb"), b).err().unwrap().contains("isn't supported"));
    }
}
