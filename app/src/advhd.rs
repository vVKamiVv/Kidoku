//! AdvHD engine (WillPlus, Rio*.arc): compiled scripts with their text inline, so a longer line moves
//! everything after it and every jump has to follow. That takes a walk over all the code, and a walk
//! that doesn't end exactly on the script's last bytes means this game's version isn't one we know.

use crate::word;
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, wrap_words, JA};
use crate::{original, replace_file, R};
use encoding_rs::SHIFT_JIS;
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// shortcut: half the longest line of the test game, a guess at a box two lines tall. The script gives
/// no line breaks and no width; change this if lines still break inside words.
const LINE_LETTERS: usize = 62;
/// Bytes after the last instruction.
const TRAILER: usize = 8;

/// Tags the engine reads inside text, like %K%P that waits for a click.
pub static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"%[A-Z]+\d*").unwrap());
static WRAPPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^((?:%[A-Z]+\d*)*)(.*?)((?:%[A-Z]+\d*)*)$").unwrap());

// ---------- the code ----------

/// What follows an opcode: b, h, i and f are values, a is an address, s a string, and x* a count byte
/// then that many x. Scripts come in three eras; the older ones differ in a few operands.
fn operands(op: u8, era: u8) -> Option<&'static str> {
    Some(match (op, era) {
        (0x1C, 1) => "ssh",
        (0x11, 1..=2) => "sf",
        (0x14, 1..=2) => "iss",
        (0x15, 1..=2) => "s",
        (0x16, 1..=2) => "b",
        (0x1E, 1..=2) => "ssffhhb",
        (0x28, 1..=2) => "ssffhhbhhb",
        (0x78, 1..=2) => "ssbb",
        (0xC9, 1..=2) => "sshhh",
        (0x00 | 0x05 | 0x13 | 0x17 | 0x19 | 0x2E | 0x3E | 0x71 | 0x9A | 0xC8 | 0xCC | 0xE7 | 0xE8 | 0xF8 | 0xFA | 0xFD | 0xFF, _) => "",
        (0x01, _) => "bhfaa",
        (0x02 | 0x06, _) => "a",
        (0x04 | 0x07 | 0x1A | 0x2B | 0x2C | 0x32 | 0x37 | 0x3C | 0x43 | 0x5C | 0x5F | 0x62 | 0x66 | 0x6F | 0x7E | 0x80 | 0x90 | 0x9B | 0x9D | 0xD2 | 0xD3 | 0xFE, _) => "s",
        (0x08 | 0x0F | 0x1B | 0x64 | 0x68 | 0xCE | 0xF0 | 0xFB, _) => "b",
        (0x09, _) => "bhf",
        (0x0A, _) => "hf",
        (0x0B, _) => "hb",
        (0x0C, _) => "hbh*",
        (0x0D, _) => "hhf",
        (0x0E, _) => "hhb",
        (0x11, _) => "sbf",
        (0x12, _) => "sbs",
        (0x14, _) => "issb",
        (0x15 | 0x22 | 0x2D | 0x38 | 0x41 | 0x63 | 0x9E | 0x9F, _) => "sb",
        (0x16, _) => "bb",
        (0x18, _) => "bs",
        (0x1C, _) => "sshb",
        (0x1D | 0x3D | 0xFC, _) => "h",
        (0x1E, _) => "ssffhhbf",
        (0x1F | 0x29 | 0x7D | 0x87, _) => "sf",
        (0x20 | 0x2A, _) => "sfh",
        (0x21, _) => "shhh",
        (0x28, _) => "ssffhhbhhbf",
        (0x2F, _) => "shf",
        (0x33 | 0x34, _) => "ssbb",
        (0x35 | 0x78, _) => "ssbbb",
        (0x36, _) => "sfffffffbb",
        (0x39, _) => "sbbh*",
        (0x3A | 0xCB, _) => "sbb",
        (0x3B, _) => "sshhhffffffff",
        (0x3F, _) => "s*",
        (0x40 | 0x44 | 0x5D, _) => "ssb",
        (0x42 | 0x57 | 0x70 | 0xD0 | 0xD1, _) => "sh",
        (0x45, _) => "shffff",
        (0x46, _) => "shbffff",
        (0x47, _) => "sshbbfffffhf",
        (0x48, _) => "sshbbs",
        (0x49 | 0x54, _) => "sss",
        (0x4A | 0x53 | 0x55 | 0x58 | 0x6B | 0x6E | 0x74 | 0x75 | 0x7B | 0x8F | 0x9C | 0xCA, _) => "ss",
        (0x4B, _) => "shhffff",
        (0x4C, _) => "shhbffff",
        (0x4D, _) => "sshhbbfffffhf",
        (0x4E, _) => "sshhbbs",
        (0x4F, _) => "sshs",
        (0x50 | 0x59 | 0x73, _) => "ssh",
        (0x51, _) => "sshfb",
        (0x52, _) => "ssfhfbs",
        (0x56, _) => "sbhfffffffffffbffffbhshssf",
        (0x5A, _) => "sh*",
        (0x5B, _) => "shb",
        (0x5E | 0x6C, _) => "sff",
        (0x60, _) => "hhhh",
        (0x61, _) => "bffff",
        (0x65, _) => "hbffbs",
        (0x67, _) => "bbhfffffb",
        (0x69, _) => "sbbfffffhf",
        (0x6A, _) => "shbbs",
        (0x72, _) => "shhs",
        (0x79 | 0x7C | 0x82 | 0xCF, _) => "ssf",
        (0x7A, _) => "ssfbbs",
        (0x7F, _) => "sfffff",
        (0x81, _) => "sbsffb",
        (0x83, _) => "ssff",
        (0x84 | 0x88, _) => "sssfhf",
        (0x85, _) => "ssbf",
        (0x86, _) => "sfff",
        (0x8C, _) => "sssbb",
        (0x8D | 0x8E, _) => "issbbs",
        (0x96, _) => "hffff",
        (0x97, _) => "hbffff",
        (0x98, _) => "shbbfffffhf",
        (0x99, _) => "shbbs",
        (0xC9, _) => "sshhhh",
        (0xCD, _) => "sssssfb",
        (0xD4, _) => "shh",
        (0xE6, _) => "aa",
        (0xF9, _) => "bs",
        _ => return None,
    })
}

/// A string's bytes in a script, end not counting its NUL.
type Span = (usize, usize);

struct Text {
    at: Span,
    who: Option<Span>,
}

struct Walk<'a> {
    b: &'a [u8],
    era: u8,
    at: usize,
    starts: HashSet<usize>,
    addresses: Vec<usize>,
    who: Option<Span>,
    names: Vec<Span>,
    texts: Vec<Text>,
}

impl Walk<'_> {
    fn take(&mut self, n: usize) -> Option<usize> {
        let at = self.at;
        self.at += n;
        (self.at <= self.b.len()).then_some(at)
    }

    fn string(&mut self) -> Option<Span> {
        let start = self.at;
        let end = start + self.b.get(start..)?.iter().position(|&c| c == 0)?;
        self.at = end + 1;
        Some((start, end))
    }

    /// Reads operands and returns their strings.
    fn operands(&mut self, template: &str) -> Option<Vec<Span>> {
        let mut strings = Vec::new();
        let mut kinds = template.bytes().peekable();
        while let Some(kind) = kinds.next() {
            let count = if kinds.next_if_eq(&b'*').is_some() { self.b[self.take(1)?] } else { 1 };
            for _ in 0..count {
                match kind {
                    b's' => strings.push(self.string()?),
                    b'b' => _ = self.take(1)?,
                    b'h' => _ = self.take(2)?,
                    b'a' => {
                        let at = self.take(4)?;
                        self.addresses.push(at);
                    }
                    _ => _ = self.take(4)?,
                }
            }
        }
        Some(strings)
    }

    fn instruction(&mut self) -> Option<()> {
        self.starts.insert(self.at);
        let op = self.b[self.take(1)?];
        let strings = self.operands(operands(op, self.era)?)?;
        let text = match op {
            // choices: each is its text, then a jump or a file to run
            0x0F => {
                for _ in 0..self.b[self.at - 1] {
                    let choice = self.operands("hsbh")?[0];
                    self.texts.push(Text { at: choice, who: None });
                    self.instruction()?;
                }
                None
            }
            0x14 => Some(strings[1]),
            0x15 => {
                self.who = Some(strings[0]).filter(|(start, end)| start != end);
                self.names.extend(self.who);
                None
            }
            0x18 => Some(strings[0]),
            // messages in the phone screen
            0x8D | 0x8E => Some(strings[1]),
            _ => None,
        };
        if let Some(at) = text {
            self.texts.push(Text { at, who: self.who });
        }
        Some(())
    }
}

struct Script {
    plain: Vec<u8>,
    addresses: Vec<usize>,
    names: Vec<Span>,
    texts: Vec<Text>,
}

impl Script {
    /// None when the walk fails or a jump lands inside an instruction: wrong era, or not this format.
    fn read(plain: Vec<u8>, era: u8) -> Option<Script> {
        let end = plain.len().checked_sub(TRAILER)?;
        let mut walk = Walk { b: &plain, era, at: 0, starts: HashSet::new(), addresses: Vec::new(), who: None, names: Vec::new(), texts: Vec::new() };
        while walk.at < end {
            walk.instruction()?;
        }
        let good = walk.at == end && walk.addresses.iter().all(|&p| matches!(word(&plain, p), 0) || word(&plain, p) == plain.len() || walk.starts.contains(&word(&plain, p)));
        let (addresses, names, texts) = (walk.addresses, walk.names, walk.texts);
        good.then_some(Script { plain, addresses, names, texts })
    }

    fn string(&self, (start, end): Span) -> String {
        SHIFT_JIS.decode_without_bom_handling(&self.plain[start..end]).0.into_owned()
    }

    fn who(&self, at: Span) -> String {
        let name = self.string(at);
        ["%LC", "%LF", "%LR"].iter().find_map(|tag| name.strip_prefix(tag)).unwrap_or(&name).to_string()
    }

    fn lines(&self, script: &str, out: &mut Vec<Line>) {
        for t in &self.texts {
            let text = self.string(t.at);
            let body = WRAPPED.captures(&text).map_or("", |c| c.get(2).unwrap().as_str()).replace("\\n", "");
            if JA.is_match(&body) {
                out.push((format!("{script}:{}", t.at.0), t.who.map(|w| self.who(w)).filter(|w| JA.is_match(w)), body));
            }
        }
    }

    /// The script with translations in place and the jumps following what moved.
    fn with(&self, script: &str, width: usize, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> (Vec<u8>, usize) {
        let mut edits = Vec::new();
        for t in &self.texts {
            let Some(tr) = line(&format!("{script}:{}", t.at.0)) else { continue };
            let text = self.string(t.at);
            let caps = WRAPPED.captures(&text).unwrap();
            // a space stays before the break, or the line's last letter is lost as text builds up
            let body = wrap_words(&ascii_punctuation(&tr).replace('%', "％"), width).join(" \\n");
            edits.push((t.at, sjis(&format!("{}{body}{}", &caps[1], &caps[3]))));
        }
        for &at in &self.names {
            if let Some(en) = name(&self.who(at)) {
                let whole = self.string(at);
                let tag = &whole[..whole.len() - self.who(at).len()];
                edits.push((at, sjis(&format!("{tag}{en}"))));
            }
        }
        edits.sort_by_key(|e| e.0);
        let mut out = Vec::with_capacity(self.plain.len());
        let (mut last, mut shifts) = (0, Vec::new());
        let mut shift = 0isize;
        for ((start, end), bytes) in &edits {
            out.extend(&self.plain[last..*start]);
            out.extend(bytes);
            last = *end;
            shift += bytes.len() as isize - (end - start) as isize;
            shifts.push((*start, shift));
        }
        out.extend(&self.plain[last..]);
        let moved = |old: usize| {
            let passed = shifts.partition_point(|s| s.0 < old);
            old.wrapping_add_signed(passed.checked_sub(1).map_or(0, |i| shifts[i].1))
        };
        for &p in &self.addresses {
            let to = word(&self.plain, p);
            if to != 0 {
                let at = moved(p);
                out[at..at + 4].copy_from_slice(&(moved(to) as u32).to_le_bytes());
            }
        }
        (out, edits.len())
    }
}

// ---------- compiled Lua ----------

const LUA_HEAD: &[u8] = b"\x1bLuaS\0\x19\x93\r\n\x1a\n\x04\x04\x04\x08\x08";
/// The two numbers that follow the head as a check, and the count of upvalues.
const LUA_START: usize = LUA_HEAD.len() + 17;
const LUA_SHORT: usize = 40;

/// Where a string constant sits: its type byte, then its text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Constant {
    tag: usize,
    text: Span,
}

/// A walk over a Lua 5.3 chunk. A string given to a named field is text, like a line the settings
/// screen shows as a sample. Any other use may be a key or a list of fonts, so it stays, everywhere.
struct LuaWalk<'a> {
    b: &'a [u8],
    at: usize,
    fields: Vec<Constant>,
    others: HashSet<&'a [u8]>,
}

impl<'a> LuaWalk<'a> {
    fn take(&mut self, n: usize) -> Option<usize> {
        let start = self.at;
        self.at = start.checked_add(n).filter(|&end| end <= self.b.len())?;
        Some(start)
    }

    fn count(&mut self) -> Option<usize> {
        let at = self.take(4)?;
        Some(word(self.b, at))
    }

    fn string(&mut self) -> Option<Span> {
        let first = self.b[self.take(1)?] as usize;
        let size = if first == 0xFF {
            let at = self.take(8)?;
            u64::from_le_bytes(self.b[at..at + 8].try_into().ok()?) as usize
        } else {
            first
        };
        let start = self.take(size.saturating_sub(1))?;
        Some((start, self.at))
    }

    fn function(&mut self) -> Option<()> {
        self.string()?;
        self.take(11)?; // first and last line, parameters, vararg, stack
        let instructions = self.count()?;
        let code = self.take(instructions.checked_mul(4)?)?;
        let mut constants = Vec::new();
        for _ in 0..self.count()? {
            let tag = self.take(1)?;
            constants.push(match self.b[tag] {
                0 => None,
                1 => self.take(1).map(|_| None)?,
                3 | 0x13 => self.take(8).map(|_| None)?,
                4 | 0x14 => Some(Constant { tag, text: self.string()? }),
                _ => return None,
            });
        }
        for i in 0..instructions {
            let ins = word(self.b, code + i * 4);
            let (op, c, b) = (ins & 63, ins >> 14 & 511, ins >> 23);
            let constant = |slot: usize| slot.checked_sub(256).and_then(|k| constants.get(k).copied().flatten());
            let used: &[usize] = match op {
                1 => &[(ins >> 14) + 256],
                6 | 7 | 12 => &[c],
                8 | 10 if b >= 256 => {
                    self.fields.extend(constant(c));
                    &[b]
                }
                8 | 10 | 31..=33 => &[b, c],
                _ => &[],
            };
            let code_bytes = self.b;
            for k in used.iter().filter_map(|&slot| constant(slot)) {
                self.others.insert(&code_bytes[k.text.0..k.text.1]);
            }
        }
        let upvalues = self.count()?;
        self.take(upvalues.checked_mul(2)?)?;
        for _ in 0..self.count()? {
            self.function()?;
        }
        let lines = self.count()?;
        self.take(lines.checked_mul(4)?)?;
        for _ in 0..self.count()? {
            self.string()?;
            self.take(8)?;
        }
        for _ in 0..self.count()? {
            self.string()?;
        }
        Some(())
    }
}

fn lua_text(b: &[u8]) -> String {
    std::str::from_utf8(b).map_or_else(|_| SHIFT_JIS.decode_without_bom_handling(b).0.into_owned(), String::from)
}

struct Chunk {
    bytes: Vec<u8>,
    texts: Vec<Constant>,
}

impl Chunk {
    /// None if this isn't a chunk we know, or the walk doesn't end on its last byte.
    fn read(bytes: Vec<u8>) -> Option<Chunk> {
        if !bytes.starts_with(LUA_HEAD) {
            return None;
        }
        let mut walk = LuaWalk { b: &bytes, at: LUA_START, fields: Vec::new(), others: HashSet::new() };
        walk.function()?;
        if walk.at != bytes.len() {
            return None;
        }
        let LuaWalk { mut fields, others, .. } = walk;
        fields.retain(|k| JA.is_match(&lua_text(&bytes[k.text.0..k.text.1])) && !others.contains(&bytes[k.text.0..k.text.1]));
        fields.sort();
        fields.dedup();
        drop(others);
        Some(Chunk { texts: fields, bytes })
    }

    fn key(script: &str, k: &Constant) -> String {
        format!("{script}:{}", k.tag)
    }

    fn lines(&self, script: &str, out: &mut Vec<Line>) {
        out.extend(self.texts.iter().map(|k| (Chunk::key(script, k), None, lua_text(&self.bytes[k.text.0..k.text.1]))));
    }

    /// The chunk with translations in place, each string in the coding it had.
    fn with(&self, script: &str, line: &dyn Fn(&str) -> Option<String>) -> (Vec<u8>, usize) {
        let (mut out, mut last, mut done) = (Vec::with_capacity(self.bytes.len()), 0, 0);
        for k in &self.texts {
            let Some(tr) = line(&Chunk::key(script, k)) else { continue };
            let utf8 = std::str::from_utf8(&self.bytes[k.text.0..k.text.1]).is_ok();
            let text = if utf8 { tr.into_bytes() } else { sjis(&ascii_punctuation(&tr)) };
            out.extend(&self.bytes[last..k.tag]);
            out.push(if text.len() <= LUA_SHORT { 4 } else { 0x14 });
            if text.len() + 1 < 0xFF {
                out.push(text.len() as u8 + 1);
            } else {
                out.push(0xFF);
                out.extend((text.len() as u64 + 1).to_le_bytes());
            }
            out.extend(text);
            last = k.text.1;
            done += 1;
        }
        out.extend(&self.bytes[last..]);
        (out, done)
    }
}

// ---------- the archives ----------

struct Item {
    name: String,
    /// Where its size and offset sit in the index.
    row: usize,
    start: usize,
    size: usize,
}

/// `u32 count, u32 index size`, then rows of `u32 size, u32 offset, UTF-16 name`; offsets count from
/// the index's end.
fn items(b: &[u8]) -> Option<Vec<Item>> {
    let (count, base) = (word(b.get(..4)?, 0), word(b.get(..8)?, 4) + 8);
    let mut rows = Vec::new();
    let mut p = 8;
    for _ in 0..count {
        let row = p;
        let (size, at) = (word(b.get(p..p + 4)?, 0), word(b.get(p + 4..p + 8)?, 0));
        p += 8;
        let units: Vec<u16> = b.get(p..base)?.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&c| c != 0).collect();
        p += (units.len() + 1) * 2;
        rows.push(Item { name: String::from_utf16_lossy(&units), row, start: base + at, size });
    }
    rows.iter().all(|i| i.start + i.size <= b.len()).then_some(rows)
}

/// The script archives of a game: `Rio.arc`, `Rio1.arc` and so on, and `Script.arc` for its Lua.
fn archives(game: &Path) -> R<Vec<PathBuf>> {
    let mut all: Vec<PathBuf> = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy().to_lowercase();
            n.starts_with("rio") && n.ends_with(".arc") || n == "script.arc"
        }))
        .collect();
    // another game's Script.arc may be some other kind of file: its Lua is only a bonus
    all.retain(|p| !p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("script.arc")) || original(p).is_ok_and(|b| items(&b).is_some()));
    all.sort();
    Ok(all)
}

fn ws2(name: &str) -> bool {
    name.to_lowercase().ends_with(".ws2")
}

/// Script bytes as stored, and in the clear: each byte is rotated by two bits, unless the game has
/// been unpacked and patched by other tools.
fn clear(raw: &[u8], rotated: bool) -> Vec<u8> {
    if rotated { raw.iter().map(|b| b.rotate_right(2)).collect() } else { raw.to_vec() }
}

// ---------- the game ----------

pub struct Game {
    /// `(archive path, item, name, script)`
    scripts: Vec<(PathBuf, usize, String, Script)>,
    /// The same for Lua.
    chunks: Vec<(PathBuf, usize, String, Chunk)>,
    era: u8,
    rotated: bool,
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let (mut raw, mut chunks) = (Vec::new(), Vec::new());
        for path in archives(game)? {
            let b = original(&path)?;
            let rows = items(&b).ok_or_else(|| format!("{}: not an AdvHD archive", path.display()))?;
            for (i, item) in rows.iter().enumerate().filter(|(_, item)| ws2(&item.name)) {
                raw.push((path.clone(), i, item.name.clone(), b[item.start..item.start + item.size].to_vec()));
            }
            for (i, item) in rows.iter().enumerate().filter(|(_, item)| item.name.to_lowercase().ends_with(".lua")) {
                let chunk = Chunk::read(b[item.start..item.start + item.size].to_vec());
                chunks.extend(chunk.filter(|c| !c.texts.is_empty()).map(|c| (path.clone(), i, item.name.clone(), c)));
            }
        }
        if raw.is_empty() {
            return Err("No AdvHD scripts found (Rio*.arc).".into());
        }
        for era in [3, 2, 1] {
            for rotated in [true, false] {
                let scripts: Option<Vec<_>> = raw.iter().map(|(path, i, name, bytes)| Some((path.clone(), *i, name.clone(), Script::read(clear(bytes, rotated), era)?))).collect();
                if let Some(scripts) = scripts {
                    return Ok(Game { scripts, chunks, era, rotated });
                }
            }
        }
        Err("This AdvHD game uses a script version that isn't supported yet.".into())
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|(path, _, name, s)| s.lines(&key(path, name), &mut out));
        self.chunks.iter().for_each(|(path, _, name, c)| c.lines(&key(path, name), &mut out));
        out
    }
}

/// Two archives could hold a script of the same name.
fn key(path: &Path, name: &str) -> String {
    format!("{}/{name}", path.file_stem().unwrap_or_default().to_string_lossy())
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the archives and reads
/// them back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let mut done = 0;
    for path in archives(game)? {
        let mut out = original(&path)?;
        let rows = items(&out).ok_or("not an AdvHD archive")?;
        let base = word(&out, 4) + 8;
        let mut new = Vec::new();
        for (_, i, item, s) in g.scripts.iter().filter(|(p, ..)| *p == path) {
            let (plain, count) = s.with(&key(&path, item), crate::fit::letters(game, LINE_LETTERS), line, name);
            if count > 0 {
                new.push((*i, plain));
                done += count;
            }
        }
        let mut lua = Vec::new();
        for (_, i, item, c) in g.chunks.iter().filter(|(p, ..)| *p == path) {
            let (bytes, count) = c.with(&key(&path, item), line);
            if count > 0 {
                lua.push((*i, bytes));
                done += count;
            }
        }
        if new.is_empty() && lua.is_empty() {
            continue;
        }
        let stored = new.iter().map(|(i, plain)| (*i, plain.iter().map(|b| if g.rotated { b.rotate_left(2) } else { *b }).collect()));
        for (i, bytes) in stored.chain(lua.iter().cloned()).collect::<Vec<(usize, Vec<u8>)>>() {
            let row = rows[i].row;
            let (size, at) = ((bytes.len() as u32).to_le_bytes(), ((out.len() - base) as u32).to_le_bytes());
            out[row..row + 4].copy_from_slice(&size);
            out[row + 4..row + 8].copy_from_slice(&at);
            out.extend(bytes);
        }
        replace_file(&path, &out)?;
        let back = fs::read(&path).map_err(|e| e.to_string())?;
        let rows = items(&back).ok_or("not an AdvHD archive")?;
        for (i, plain) in new {
            let item = &rows[i];
            let bytes = clear(&back[item.start..item.start + item.size], g.rotated);
            if bytes != plain || Script::read(bytes, g.era).is_none() {
                return Err(format!("{} didn't read back as written", item.name));
            }
        }
        for (i, bytes) in lua {
            let item = &rows[i];
            if back[item.start..item.start + item.size] != bytes[..] || Chunk::read(bytes).is_none() {
                return Err(format!("{} didn't read back as written", item.name));
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A named line, a jump over a plain line, a phone message and a choice, then the end.
    fn script() -> Vec<u8> {
        let mut b = Vec::new();
        let string = |b: &mut Vec<u8>, t: &str| {
            b.extend(sjis(t));
            b.push(0);
        };
        b.push(0x15);
        string(&mut b, "%LC太郎");
        b.push(0);
        b.extend([0x14, 1, 0, 0, 0]);
        string(&mut b, "char");
        string(&mut b, "こんにちは%K%P");
        b.push(0);
        b.extend([0x15, 0, 0]);
        b.push(0x06);
        let jump = b.len();
        b.extend([0; 4]);
        b.extend([0x14, 2, 0, 0, 0]);
        string(&mut b, "char");
        string(&mut b, "%XS21地の文です%XE%K%P");
        b.push(0);
        let land = b.len() as u32;
        b.extend([0x8D, 3, 0, 0, 0, 0]);
        string(&mut b, "携帯の文");
        b.extend([1, 0, 0]);
        b.extend([0x0F, 1, 1, 0]);
        string(&mut b, "選択肢");
        b.extend([0, 0, 0, 0]);
        b.push(0x06);
        b.extend(land.to_le_bytes());
        b.push(0xFF);
        b[jump..jump + 4].copy_from_slice(&land.to_le_bytes());
        b.extend([0; TRAILER]);
        b
    }

    fn archive(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut index = Vec::new();
        let mut at = 0u32;
        for (name, b) in files {
            index.extend((b.len() as u32).to_le_bytes());
            index.extend(at.to_le_bytes());
            name.encode_utf16().chain([0]).for_each(|c| index.extend(c.to_le_bytes()));
            at += b.len() as u32;
        }
        let mut out = (files.len() as u32).to_le_bytes().to_vec();
        out.extend((index.len() as u32).to_le_bytes());
        out.extend(index);
        files.iter().for_each(|(_, b)| out.extend(b));
        out
    }

    fn game(files: &[(&str, Vec<u8>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-advhd-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Rio.arc"), archive(files)).unwrap();
        dir
    }

    fn rotated(b: &[u8]) -> Vec<u8> {
        b.iter().map(|c| c.rotate_left(2)).collect()
    }

    #[test]
    fn text_comes_without_its_tags_and_with_its_speaker() {
        let dir = game(&[("a.ws2", rotated(&script())), ("b.dat", b"untouched".to_vec())]);
        let lines = Game::open(&dir).unwrap().lines();
        let shown: Vec<_> = lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect();
        assert_eq!(shown, [(Some("太郎"), "こんにちは"), (None, "地の文です"), (None, "携帯の文"), (None, "選択肢")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn jumps_follow_the_text_and_it_reads_back() {
        let dir = game(&[("a.ws2", rotated(&script())), ("b.dat", b"untouched".to_vec())]);
        let lines = Game::open(&dir).unwrap().lines();
        let keys: Vec<_> = lines.iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.iter().position(|x| x == k).map(|i| format!("A much longer English line number {i}"));
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 5);

        let g = Game::open(&dir).unwrap();
        let written = fs::read(dir.join("Rio.arc")).unwrap();
        let rows = items(&written).unwrap();
        assert_eq!(&written[rows[1].start..rows[1].start + rows[1].size], b"untouched");
        let plain = clear(&written[rows[0].start..rows[0].start + rows[0].size], true);
        let s = Script::read(plain, g.era).unwrap();
        assert!(s.string(s.names[0]).starts_with("%LCTaro"));
        let first = s.string(s.texts[0].at);
        assert!(first.ends_with("%K%P") && first.starts_with("A much longer"));
        assert!(s.string(s.texts[1].at).starts_with("%XS21A much"));
        let phone = s.texts[2].at.0 - 6;
        assert!(s.addresses.iter().all(|&p| word(&s.plain, p) == phone), "both jumps land on the phone message");

        // a second run starts from the game's own file and gives the same bytes
        install(&dir, &line, &name).unwrap();
        assert_eq!(fs::read(dir.join("Rio.arc")).unwrap(), written);
        let _ = fs::remove_dir_all(dir);
    }

    fn lua_string(out: &mut Vec<u8>, text: &[u8]) {
        out.push(4);
        out.push(text.len() as u8 + 1);
        out.extend(text);
    }

    /// `a = "見本の文"; b = "鍵の文"; c = t["鍵の文"]; f("読み込む")`, with the first in Shift-JIS.
    fn chunk() -> Vec<u8> {
        let mut b = LUA_HEAD.to_vec();
        b.extend([0; 16]);
        b.push(0);
        b.push(0); // no source name
        b.extend([0; 11]);
        let code: [u32; 5] = [
            8 | 257 << 14 | 256 << 23,
            8 | 259 << 14 | 258 << 23,
            7 | 259 << 14,
            1 | 4 << 14,
            38,
        ];
        b.extend((code.len() as u32).to_le_bytes());
        code.iter().for_each(|c| b.extend(c.to_le_bytes()));
        b.extend(6u32.to_le_bytes());
        for text in [&b"a"[..], &sjis("見本の文"), b"b", "鍵の文".as_bytes(), "読み込む".as_bytes()] {
            lua_string(&mut b, text);
        }
        b.extend([1, 1]); // true, which isn't a string
        b.extend([0; 4]); // upvalues
        b.extend([0; 4]); // functions inside
        b.extend([0; 12]); // lines, locals, upvalue names
        b
    }

    #[test]
    fn only_a_string_given_to_a_named_field_is_text_in_lua() {
        let c = Chunk::read(chunk()).unwrap();
        let mut lines = Vec::new();
        c.lines("s", &mut lines);
        assert_eq!(lines.iter().map(|l| l.2.as_str()).collect::<Vec<_>>(), ["見本の文"]);
        let (bytes, done) = c.with("s", &|_| Some("x".repeat(45)));
        assert_eq!(done, 1);
        let again = Chunk::read(bytes).expect("the walk still ends on the last byte");
        assert_eq!(again.texts.len(), 0, "English isn't Japanese");
        let (bytes, _) = c.with("s", &|_| Some("x".repeat(300)));
        assert!(Chunk::read(bytes).is_some(), "a size over a byte long");
    }

    #[test]
    fn a_lua_chunk_that_loses_its_place_is_left_alone() {
        let mut b = chunk();
        b.push(0);
        assert!(Chunk::read(b).is_none());
        let mut b = chunk();
        b.truncate(b.len() - 4);
        assert!(Chunk::read(b).is_none());
        assert!(Chunk::read(b"\x1bLuaQ".to_vec()).is_none(), "another Lua");
    }

    #[test]
    fn lua_goes_back_into_its_archive_as_it_was_stored() {
        let dir = game(&[("a.ws2", rotated(&script()))]);
        fs::write(dir.join("Script.arc"), archive(&[("menu.lua", chunk()), ("other.lua", b"not a chunk".to_vec())])).unwrap();
        let g = Game::open(&dir).unwrap();
        assert_eq!(g.lines().last().unwrap().2, "見本の文");
        let line = |k: &str| k.starts_with("Script/").then(|| "Sample".to_string());
        assert_eq!(install(&dir, &line, &|_| None).unwrap(), 1);
        let written = fs::read(dir.join("Script.arc")).unwrap();
        let rows = items(&written).unwrap();
        let bytes = &written[rows[0].start..rows[0].start + rows[0].size];
        assert!(bytes.starts_with(LUA_HEAD), "not rotated");
        assert!(bytes.windows(6).any(|w| w == b"Sample"));
        assert_eq!(&written[rows[1].start..rows[1].start + rows[1].size], b"not a chunk");
        assert_eq!(Game::open(&dir).unwrap().lines().last().unwrap().2, "見本の文", "reads the game's own");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_walk_that_loses_its_place_is_refused() {
        let mut b = script();
        let end = b.len() - TRAILER - 1;
        b[end] = 0x02;
        assert!(Script::read(b, 3).is_none());
        let dir = game(&[("a.ws2", vec![0xEE; 40])]);
        assert!(Game::open(&dir).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn long_lines_break_with_a_space_kept_before_the_break() {
        let dir = game(&[("a.ws2", rotated(&script()))]);
        let g = Game::open(&dir).unwrap();
        let (_, _, name, s) = &g.scripts[0];
        let long = "word ".repeat(30);
        let (out, _) = s.with(name, LINE_LETTERS, &|_| Some(long.clone()), &|_| None);
        let again = Script::read(out, 3).unwrap();
        assert!(again.string(again.texts[0].at).contains(" \\n"));
        let _ = fs::remove_dir_all(dir);
    }
}
