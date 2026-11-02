//! Majiro engine (scenario.arc full of .mjo scripts): compiled code, XORed with a CRC table when the
//! signature says so. Text sits inside the code, so the code is walked whole and rebuilt, with jumps
//! and the function index put right.

use crate::text::decoded;
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, wrap_words, JA};
use crate::{original, replace_file, R};
use encoding_rs::SHIFT_JIS;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

const ARCHIVE: &str = "scenario.arc";

fn word(b: &[u8], p: usize) -> R<usize> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated Majiro file")?.try_into().unwrap()) as usize)
}

fn short(b: &[u8], p: usize) -> R<usize> {
    Ok(u16::from_le_bytes(b.get(p..p + 2).ok_or("truncated Majiro file")?.try_into().unwrap()) as usize)
}

// ---------- the archive ----------

/// One script: its row in the table, and where its bytes are.
pub struct Item {
    pub name: String,
    row: usize,
    place: usize,
    size: usize,
}

fn items(arc: &[u8]) -> R<Vec<Item>> {
    if arc.get(..16) != Some(b"MajiroArcV3.000\0") {
        return Err("this Majiro archive version isn't supported yet".into());
    }
    let (count, names, data) = (word(arc, 16)?, word(arc, 20)?, word(arc, 24)?);
    let mut list = arc.get(names..data).ok_or("bad Majiro archive")?.split(|&c| c == 0);
    (0..count)
        .map(|i| {
            let row = 0x1c + 16 * i;
            let name = SHIFT_JIS.decode_without_bom_handling(list.next().ok_or("bad Majiro names")?).0.into_owned();
            Ok(Item { name, row, place: word(arc, row + 8)?, size: word(arc, row + 12)? })
        })
        .collect()
}

/// The archive with `new` scripts in place of the old, added on the end: (index in the list, bytes).
fn with(arc: &[u8], list: &[Item], new: &[(usize, Vec<u8>)]) -> Vec<u8> {
    let mut out = arc.to_vec();
    for (i, bytes) in new {
        let (row, place) = (list[*i].row, out.len() as u32);
        out[row + 8..row + 12].copy_from_slice(&place.to_le_bytes());
        out[row + 12..row + 16].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend(bytes);
    }
    out
}

// ---------- scripts ----------

/// Scripts are XORed with the bytes of the standard CRC-32 table.
fn crypt(b: &mut [u8]) {
    let table: Vec<u8> = (0..256u32).flat_map(|n| (0..8).fold(n, |c, _| if c & 1 != 0 { c >> 1 ^ 0xedb8_8320 } else { c >> 1 }).to_le_bytes()).collect();
    for (i, c) in b.iter_mut().enumerate() {
        *c ^= table[i % 1024];
    }
}

const TEXT: u16 = 0x840;
const CONTROL: u16 = 0x842;
const STRING: u16 = 0x801;

/// One instruction: where it is, and what's in it that can move.
#[derive(Debug, Clone)]
pub struct Op {
    pub code: u16,
    pub at: usize,
    pub end: usize,
    /// (place of an offset in the code, where it counts from)
    jumps: Vec<(usize, usize)>,
}

fn walk(code: &[u8]) -> R<Vec<Op>> {
    let (mut p, mut out) = (0, Vec::new());
    while p < code.len() {
        let at = p;
        let op = short(code, p)? as u16;
        p += 2;
        let mut jumps = Vec::new();
        match op {
            0x100..=0x1a9 | 0x82b | 0x82f | 0x83e | 0x83f | 0x841 | 0x844 | 0x846 => {}
            0x1b0..=0x207 | 0x210..=0x267 | 0x270..=0x327 | 0x802 | 0x837 => p += 8,
            0x800 | 0x803 => p += 4,
            STRING | TEXT | CONTROL => p += 2 + short(code, p)?,
            0x80f | 0x810 => p += 10,
            0x834 | 0x835 => p += 6,
            0x829 | 0x836 => p += 2 + short(code, p)?,
            0x83a => p += 2,
            0x82c..=0x82e | 0x830..=0x833 | 0x838 | 0x839 | 0x83b..=0x83d | 0x843 | 0x845 | 0x847 => {
                jumps.push((p, p + 4));
                p += 4;
            }
            0x850 => {
                let count = short(code, p)?;
                p += 2;
                for _ in 0..count {
                    jumps.push((p, p + 4));
                    p += 4;
                }
            }
            _ => return Err(format!("unknown Majiro opcode {op:#x} at {at:#x}")),
        }
        if p > code.len() {
            return Err("a Majiro instruction ran past the code".into());
        }
        out.push(Op { code: op, at, end: p, jumps });
    }
    Ok(out)
}

/// A .mjo: the header, the function index and the code.
pub struct Script {
    encrypted: bool,
    entry: usize,
    marks: usize,
    functions: Vec<(u32, usize)>,
    pub code: Vec<u8>,
    pub ops: Vec<Op>,
}

impl Script {
    pub fn read(b: &[u8]) -> R<Script> {
        let encrypted = match b.get(..16) {
            Some(b"MajiroObjX1.000\0") => true,
            Some(b"MajiroObjV1.000\0") => false,
            _ => return Err("not a Majiro script".into()),
        };
        let (entry, marks, count) = (word(b, 16)?, word(b, 20)?, word(b, 24)?);
        let functions: Vec<(u32, usize)> = (0..count).map(|i| Ok((word(b, 28 + 8 * i)? as u32, word(b, 32 + 8 * i)?))).collect::<R<_>>()?;
        let at = 28 + 8 * count;
        let size = word(b, at)?;
        let mut code = b.get(at + 4..at + 4 + size).ok_or("truncated Majiro script")?.to_vec();
        if encrypted {
            crypt(&mut code);
        }
        let ops = walk(&code)?;
        Ok(Script { encrypted, entry, marks, functions, code, ops })
    }

    /// The string an instruction holds, with its size and terminator off.
    pub fn string(&self, op: &Op) -> &[u8] {
        &self.code[op.at + 4..op.end - 1]
    }

    /// The script as a file, with instructions replaced by `new` (by index), and every offset put right.
    fn with(&self, new: &HashMap<usize, Vec<u8>>) -> R<Vec<u8>> {
        let mut code = Vec::with_capacity(self.code.len() * 2);
        let mut moved = HashMap::new();
        let mut patches = Vec::new();
        for (i, op) in self.ops.iter().enumerate() {
            moved.insert(op.at, code.len());
            let start = code.len();
            match new.get(&i) {
                Some(bytes) => code.extend(bytes),
                None => {
                    code.extend(&self.code[op.at..op.end]);
                    for &(place, from) in &op.jumps {
                        let target = from as i64 + i32::from_le_bytes(self.code[place..place + 4].try_into().unwrap()) as i64;
                        patches.push((start + place - op.at, start + from - op.at, target));
                    }
                }
            }
        }
        moved.insert(self.code.len(), code.len());
        let at = |old: i64| moved.get(&(old as usize)).copied().ok_or("a Majiro jump lands inside an instruction".to_string());
        for (place, from, target) in patches {
            let offset = at(target)? as i64 - from as i64;
            code[place..place + 4].copy_from_slice(&(offset as i32).to_le_bytes());
        }
        let size = code.len() as u32;
        if self.encrypted {
            crypt(&mut code);
        }
        let mut out = if self.encrypted { b"MajiroObjX1.000\0".to_vec() } else { b"MajiroObjV1.000\0".to_vec() };
        out.extend((at(self.entry as i64)? as u32).to_le_bytes());
        out.extend((self.marks as u32).to_le_bytes());
        out.extend((self.functions.len() as u32).to_le_bytes());
        for (hash, place) in &self.functions {
            out.extend(hash.to_le_bytes());
            out.extend((at(*place as i64)? as u32).to_le_bytes());
        }
        out.extend(size.to_le_bytes());
        out.extend(code);
        Ok(out)
    }
}

// ---------- text ----------

// shortcut: the message box isn't in the files; a usual 26 full-width characters, until a game shows otherwise
const LINE_LETTERS: usize = 52;
const CALL: [u16; 4] = [0x80f, 0x810, 0x834, 0x835];
const FUNCTIONS: [&str; 7] = ["$select@MENU", "$select_add@MENU", "$select_ex@MENU", "$select_exx@MENU", "$novel_select@MENU", "$novel_select_ex@MENU", "$inter_select@MENU"];

/// A name in front of a quote: the engine draws what comes before `「` as the speaker.
static SPOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^([^「]{1,12})「(.*)」$").unwrap());

fn crc(name: &str) -> u32 {
    let mut c = flate2::Crc::new();
    c.update(name.as_bytes());
    c.sum()
}

/// An instruction that holds a string.
fn with_string(code: u16, text: &[u8]) -> Vec<u8> {
    let mut out = code.to_le_bytes().to_vec();
    out.extend((text.len() as u16 + 1).to_le_bytes());
    out.extend(text);
    out.push(0);
    out
}

impl Script {
    /// Strings handed to a function that puts up a choice.
    fn choices(&self) -> Vec<usize> {
        let select: Vec<u32> = FUNCTIONS.iter().map(|f| crc(f)).collect();
        let mut out = Vec::new();
        for (i, op) in self.ops.iter().enumerate().filter(|(_, o)| o.code == STRING) {
            let call = self.ops[i + 1..].iter().take(12).find(|o| CALL.contains(&o.code));
            let hash = call.and_then(|o| word(&self.code, o.at + 2).ok()).map(|h| h as u32);
            if hash.is_some_and(|h| select.contains(&h)) && JA.is_match(&decoded(self.string(op))) {
                out.push(i);
            }
        }
        out
    }

    fn lines(&self, name: &str, out: &mut Vec<Line>) {
        for (i, op) in self.ops.iter().enumerate().filter(|(_, o)| o.code == TEXT) {
            let text = decoded(self.string(op));
            if !JA.is_match(&text) {
                continue;
            }
            let (who, body) = match SPOKEN.captures(&text) {
                Some(c) => (Some(c[1].to_string()), c[2].to_string()),
                None => (None, text),
            };
            out.push((format!("{name}:{i}"), who, body));
        }
        for i in self.choices() {
            out.push((format!("{name}:{i}"), None, decoded(self.string(&self.ops[i]))));
        }
    }

    /// Long messages become several text instructions with line breaks between, as the game itself writes one.
    fn translated(&self, stem: &str, width: usize, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<(Vec<u8>, usize)> {
        let (mut new, mut done) = (HashMap::new(), 0);
        for (i, op) in self.ops.iter().enumerate().filter(|(_, o)| o.code == TEXT) {
            let Some(tr) = line(&format!("{stem}:{i}")) else { continue };
            let text = decoded(self.string(op));
            let body = ascii_punctuation(&tr);
            let spoken = SPOKEN.captures(&text).map(|c| name(&c[1]).unwrap_or(c[1].to_string()));
            let whole = match &spoken {
                Some(who) => format!("{who}「{}」", body.trim_matches('"')),
                None => body,
            };
            let mut bytes = Vec::new();
            for (n, l) in wrap_words(&whole, width).iter().enumerate() {
                if n > 0 {
                    bytes.extend(0x841u16.to_le_bytes());
                    bytes.extend(with_string(CONTROL, b"n"));
                }
                bytes.extend(with_string(TEXT, &sjis(l)));
            }
            new.insert(i, bytes);
            done += 1 + spoken.is_some() as usize;
        }
        for i in self.choices() {
            if let Some(tr) = line(&format!("{stem}:{i}")) {
                new.insert(i, with_string(STRING, &sjis(&ascii_punctuation(&tr))));
                done += 1;
            }
        }
        if done == 0 {
            return Ok((Vec::new(), 0));
        }
        Ok((self.with(&new)?, done))
    }
}

// ---------- the game ----------

pub struct Game {
    arc: Vec<u8>,
    items: Vec<Item>,
    scripts: Vec<(usize, Script)>,
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let arc = original(&game.join(ARCHIVE))?;
        let items = items(&arc)?;
        let mut scripts = Vec::new();
        for (i, item) in items.iter().enumerate().filter(|(_, i)| i.name.to_lowercase().ends_with(".mjo")) {
            let b = arc.get(item.place..item.place + item.size).ok_or("bad Majiro entry")?;
            scripts.push((i, Script::read(b).map_err(|e| format!("{}: {e}", item.name))?));
        }
        Ok(Game { arc, items, scripts })
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|(i, s)| s.lines(&self.items[*i].name, &mut out));
        out
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into scenario.arc and reads
/// it back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let (mut new, mut done) = (Vec::new(), 0);
    for (i, s) in &g.scripts {
        let (bytes, n) = s.translated(&g.items[*i].name, crate::fit::letters(game, LINE_LETTERS), line, name)?;
        if n > 0 {
            Script::read(&bytes).map_err(|e| format!("{}: {e}", g.items[*i].name))?;
            new.push((*i, bytes));
            done += n;
        }
    }
    let out = with(&g.arc, &g.items, &new);
    let back = items(&out)?;
    for (i, bytes) in &new {
        if out.get(back[*i].place..back[*i].place + back[*i].size) != Some(bytes.as_slice()) {
            return Err(format!("{} didn't read back as written", back[*i].name));
        }
    }
    replace_file(&game.join(ARCHIVE), &out)?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn the_table_starts_as_the_crc_table_does() {
        let mut b = [0u8; 8];
        crypt(&mut b);
        assert_eq!(&b[4..8], &0x7707_3096u32.to_le_bytes());
    }

    /// A jump over two messages, the second spoken, then a choice.
    fn code() -> Vec<u8> {
        let mut c = 0x82cu16.to_le_bytes().to_vec();
        let jump = c.len();
        c.extend([0; 4]);
        let from = c.len();
        c.extend(with_string(TEXT, &sjis("地の文です")));
        c.extend(0x841u16.to_le_bytes());
        c.extend(with_string(TEXT, &sjis("太郎「こんにちは」")));
        c.extend(0x841u16.to_le_bytes());
        let target = c.len();
        c[jump..jump + 4].copy_from_slice(&((target - from) as i32).to_le_bytes());
        c.extend(with_string(STRING, &sjis("はい")));
        c.extend(with_string(STRING, &sjis("背景画像")));
        c.extend(0x810u16.to_le_bytes());
        c.extend(crc("$disp_back@PIC").to_le_bytes());
        c.extend([0; 6]);
        c.extend(with_string(STRING, &sjis("いいえ")));
        c.extend(0x810u16.to_le_bytes());
        c.extend(crc("$select@MENU").to_le_bytes());
        c.extend([0; 6]);
        c.extend(0x82bu16.to_le_bytes());
        c
    }

    fn script(encrypted: bool, code: &[u8]) -> Vec<u8> {
        let mut out = if encrypted { b"MajiroObjX1.000\0".to_vec() } else { b"MajiroObjV1.000\0".to_vec() };
        out.extend([0; 4]);
        out.extend([0; 4]);
        out.extend(1u32.to_le_bytes());
        out.extend(7u32.to_le_bytes());
        out.extend((code.len() as u32 - 2).to_le_bytes());
        out.extend((code.len() as u32).to_le_bytes());
        let mut c = code.to_vec();
        if encrypted {
            crypt(&mut c);
        }
        out.extend(c);
        out
    }

    fn game(scripts: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-majiro-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let names: Vec<u8> = scripts.iter().flat_map(|(n, _)| n.bytes().chain([0])).collect();
        let table = 0x1c + 16 * scripts.len();
        let data = table + names.len();
        let mut out = b"MajiroArcV3.000\0".to_vec();
        for v in [scripts.len(), table, data] {
            out.extend((v as u32).to_le_bytes());
        }
        let mut place = data;
        for (_, b) in scripts {
            out.extend([0; 8]);
            out.extend((place as u32).to_le_bytes());
            out.extend((b.len() as u32).to_le_bytes());
            place += b.len();
        }
        out.extend(names);
        scripts.iter().for_each(|(_, b)| out.extend(b));
        fs::write(dir.join(ARCHIVE), out).unwrap();
        dir
    }

    #[test]
    fn lines_and_choices_are_found_but_not_file_names() {
        let dir = game(&[("a.mjo", script(true, &code()))]);
        let lines = Game::open(&dir).unwrap().lines();
        let shown: Vec<(Option<&str>, &str)> = lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect();
        assert_eq!(shown, [(None, "地の文です"), (Some("太郎"), "こんにちは"), (None, "いいえ")]);
    }

    #[test]
    fn translations_move_the_code_and_offsets_follow() {
        let dir = game(&[("a.mjo", script(true, &code()))]);
        let lines = Game::open(&dir).unwrap().lines();
        let keys: Vec<String> = lines.iter().map(|l| l.0.clone()).collect();
        let long = "word ".repeat(16);
        let line = |k: &str| keys.iter().position(|x| x == k).map(|i| [long.as_str(), "\"Hello.\"", "No"][i].to_string());
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 4);

        let out = fs::read(dir.join(ARCHIVE)).unwrap();
        let it = &items(&out).unwrap()[0];
        let s = Script::read(&out[it.place..it.place + it.size]).unwrap();
        let texts: Vec<String> = s.ops.iter().filter(|o| o.code == TEXT).map(|o| decoded(s.string(o))).collect();
        assert!(texts.len() >= 3 && texts.last().unwrap() == "Taro「Hello.」");
        // the jump still lands on the first string after the messages
        let j = s.ops[0].end + i32::from_le_bytes(s.code[2..6].try_into().unwrap()) as usize;
        let landed = s.ops.iter().find(|o| o.at == j).unwrap();
        assert_eq!(landed.code, STRING);
        assert_eq!(decoded(s.string(landed)), "はい");
        // the function index moved with the end of the code
        let last = s.ops.last().unwrap();
        assert_eq!(s.functions[0].1, last.at);
        assert_eq!(s.entry, 0);
    }
}
