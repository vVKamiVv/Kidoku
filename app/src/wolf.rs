//! Wolf RPG Editor games: Data.wolf, an old DX Archive encrypted with a key each editor version
//! shares, holding maps (.mps), common events and databases, with Shift-JIS strings.

use crate::pipeline::Line;
use crate::text::{looks_like_name, ascii_punctuation, sjis, text_units, wrap_words, JA};
use crate::{original, replace_file, R};
use regex::Regex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;
#[cfg(test)]
use std::fs;

const ARCHIVE: &str = "Data.wolf";

fn u32_at(b: &[u8], p: usize) -> R<u32> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated Wolf RPG file")?.try_into().unwrap()))
}

// ---------- the archive ----------

/// The keys of the editor versions that use this archive format, as the editor writes them.
const KEYS: [[u8; 12]; 2] = [
    [0x0f, 0x53, 0xe1, 0x3e, 0x04, 0x37, 0x12, 0x17, 0x60, 0x0f, 0x53, 0xe1], // 2.01
    [0x4c, 0xd9, 0x2a, 0xb7, 0x28, 0x9b, 0xac, 0x07, 0x3e, 0x77, 0xec, 0x4c], // 2.10
];

/// What the archive format does to a key before using it.
fn key_bytes(k: [u8; 12]) -> [u8; 12] {
    [
        !k[0],
        k[1].rotate_left(4),
        k[2] ^ 0x8a,
        !k[3].rotate_left(4),
        !k[4],
        k[5] ^ 0xac,
        !k[6],
        !k[7].rotate_right(3),
        k[8].rotate_right(5),
        k[9] ^ 0x7f,
        k[10].rotate_left(4) ^ 0xd6,
        k[11] ^ 0xcc,
    ]
}

/// The cipher is its own inverse. Below version 5 the place in the file picks the key byte.
fn xor(b: &mut [u8], at: u64, key: &[u8; 12]) {
    for (i, c) in b.iter_mut().enumerate() {
        *c ^= key[(at as usize + i) % 12];
    }
}

const HEAD: usize = 24;
const ENTRY: usize = 44; // a file's: name, attributes, three times, place, size, packed size
const DIRECTORY: u32 = 0x10;
const NOT_PACKED: u32 = 0xffff_ffff;

/// One file: where its entry is in the tables, and its path.
pub struct Item {
    pub path: String,
    entry: usize,
}

/// The tables at the end of the archive, decrypted.
pub struct Archive {
    key: [u8; 12],
    head: Vec<u8>,
    tables: Vec<u8>,
    files_at: usize,
    pub items: Vec<Item>,
}

fn field(b: &[u8], p: usize) -> R<usize> {
    Ok(u32_at(b, p)? as usize)
}

impl Archive {
    /// `data` is the whole archive, as the tables sit at its end.
    pub fn read(data: &[u8]) -> R<Archive> {
        let raw = data.get(..HEAD).ok_or("Data.wolf is too short")?;
        let key = KEYS.iter().map(|k| key_bytes(*k)).find(|k| raw[0] ^ k[0] == b'D' && raw[1] ^ k[1] == b'X');
        let key = key.ok_or("this Wolf RPG version's archive isn't supported yet")?;
        let mut head = raw.to_vec();
        xor(&mut head, 0, &key);
        if u16::from_le_bytes([head[2], head[3]]) >= 5 {
            return Err("this Wolf RPG version's archive isn't supported yet".into());
        }
        let (size, at) = (field(&head, 4)?, field(&head, 12)?);
        let mut tables = data.get(at..at + size).ok_or("bad Data.wolf tables")?.to_vec();
        xor(&mut tables, at as u64, &key);
        let (files_at, dirs_at) = (field(&head, 16)?, field(&head, 20)?);
        let mut a = Archive { key, head, tables, files_at, items: Vec::new() };
        a.walk(dirs_at, dirs_at, "", 0)?;
        Ok(a)
    }

    /// The file's name sits after two u16s and the upper-cased copy that takes the first u16 * 4 bytes.
    fn name(&self, at: usize) -> R<String> {
        let n = u16::from_le_bytes(self.tables.get(at..at + 2).ok_or("bad Data.wolf name")?.try_into().unwrap()) as usize;
        let tail = self.tables.get(at + 4 + 4 * n..).ok_or("bad Data.wolf name")?;
        let raw = &tail[..tail.iter().position(|&c| c == 0).unwrap_or(tail.len())];
        Ok(encoding_rs::SHIFT_JIS.decode_without_bom_handling(raw).0.into_owned())
    }

    fn walk(&mut self, dirs_at: usize, dir: usize, root: &str, depth: usize) -> R<()> {
        if depth > 16 {
            return Err("bad Data.wolf folders".into());
        }
        let (count, first) = (field(&self.tables, dir + 8)?, field(&self.tables, dir + 12)?);
        for i in 0..count {
            let entry = self.files_at + first + i * ENTRY;
            let name = self.name(field(&self.tables, entry)?)?;
            let path = if root.is_empty() { name } else { format!("{root}/{name}") };
            if field(&self.tables, entry + 4)? as u32 & DIRECTORY != 0 {
                let inner = dirs_at + field(&self.tables, entry + 32)?;
                self.walk(dirs_at, inner, &path, depth + 1)?;
            } else {
                self.items.push(Item { path, entry });
            }
        }
        Ok(())
    }

    pub fn file(&self, data: &[u8], item: &Item) -> R<Vec<u8>> {
        let t = &self.tables;
        let (place, size, packed) = (field(t, item.entry + 32)?, field(t, item.entry + 36)?, u32_at(t, item.entry + 40)?);
        let at = field(&self.head, 8)? + place;
        let stored = if packed == NOT_PACKED { size } else { packed as usize };
        let mut b = data.get(at..at + stored).ok_or("bad Data.wolf entry")?.to_vec();
        xor(&mut b, at as u64, &self.key);
        if packed == NOT_PACKED { Ok(b) } else { unpack(&b, size) }
    }
}

/// The archive's LZ: a key byte starts a copy, and two of them stand for the byte itself.
fn unpack(b: &[u8], size: usize) -> R<Vec<u8>> {
    const BAD: &str = "bad packed data in Data.wolf";
    let end = field(b, 4)?;
    let key = *b.get(8).ok_or(BAD)?;
    let (mut p, mut out) = (9, Vec::with_capacity(size));
    let at = |p: usize| b.get(p).copied().ok_or(BAD);
    while p < end {
        if at(p)? != key {
            out.push(at(p)?);
            p += 1;
            continue;
        }
        if at(p + 1)? == key {
            out.push(key);
            p += 2;
            continue;
        }
        let code = at(p + 1)? as usize - (at(p + 1)? > key) as usize;
        p += 2;
        let mut run = code >> 3;
        if code & 4 != 0 {
            run |= (at(p)? as usize) << 5;
            p += 1;
        }
        run += 4;
        let n = (code & 3) + 1;
        let mut back = 0;
        for i in 0..n {
            back |= (at(p + i)? as usize) << (8 * i);
        }
        p += n;
        back += 1;
        let from = out.len().checked_sub(back).ok_or(BAD)?;
        for i in 0..run {
            out.push(out[from + i]);
        }
    }
    if out.len() != size {
        return Err(BAD.into());
    }
    Ok(out)
}

impl Archive {
    /// The archive with `new` files in place of the old: (index in `items`, contents). Their data
    /// and the tables go on the end, so everything else stays where the game expects it.
    pub fn with(&self, data: &[u8], new: &[(usize, Vec<u8>)]) -> R<Vec<u8>> {
        let mut out = data.to_vec();
        let mut tables = self.tables.clone();
        let start = field(&self.head, 8)?;
        let put = |tables: &mut [u8], entry: usize, value: usize| tables[entry..entry + 4].copy_from_slice(&(value as u32).to_le_bytes());
        for (i, bytes) in new {
            let entry = self.items[*i].entry;
            let at = out.len();
            let mut b = bytes.clone();
            xor(&mut b, at as u64, &self.key);
            out.extend(b);
            put(&mut tables, entry + 32, at - start);
            put(&mut tables, entry + 36, bytes.len());
            put(&mut tables, entry + 40, NOT_PACKED as usize);
        }
        let at = out.len();
        xor(&mut tables, at as u64, &self.key);
        out.extend(tables);
        let mut head = self.head.clone();
        put(&mut head, 12, at);
        xor(&mut head, 0, &self.key);
        out[..HEAD].copy_from_slice(&head);
        Ok(out)
    }
}

// ---------- the data files ----------

/// A string of a data file: where it sits, length field included, and what it is.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    at: usize,
    end: usize,
    pub text: String,
    /// Shown in the message window, so word wrap applies.
    pub message: bool,
    /// Might be a name other things look up, so names from the .project files stay.
    pub lookup: bool,
}

struct Scan<'a> {
    b: &'a [u8],
    p: usize,
    found: Vec<Found>,
}

impl Scan<'_> {
    fn u8(&mut self) -> R<u8> {
        self.p += 1;
        self.b.get(self.p - 1).copied().ok_or("truncated Wolf RPG file".into())
    }

    fn u32(&mut self) -> R<usize> {
        self.p += 4;
        field(self.b, self.p - 4)
    }

    fn skip(&mut self, n: usize) -> R<()> {
        self.p += n;
        if self.p > self.b.len() { Err("truncated Wolf RPG file".into()) } else { Ok(()) }
    }

    fn expect(&mut self, what: u8) -> R<()> {
        if self.u8()? == what { Ok(()) } else { Err(format!("unexpected data in a Wolf RPG file at {}", self.p - 1)) }
    }

    fn string(&mut self) -> R<(usize, usize)> {
        let at = self.p;
        let n = self.u32()?;
        self.skip(n)?;
        Ok((at, self.p))
    }

    fn strings(&mut self) -> R<Vec<(usize, usize)>> {
        (0..self.u32()?).map(|_| self.string()).collect()
    }

    fn note(&mut self, (at, end): (usize, usize), message: bool, lookup: bool) {
        let raw = &self.b[at + 4..end.saturating_sub(1).max(at + 4)];
        let text = encoding_rs::SHIFT_JIS.decode_without_bom_handling(raw).0.into_owned();
        self.found.push(Found { at, end, text, message, lookup });
    }

    /// A route: a code, its numbers, and two bytes of terminator.
    fn route(&mut self) -> R<()> {
        for _ in 0..self.u32()? {
            self.skip(1)?;
            let n = self.u8()? as usize;
            self.skip(4 * n + 2)?;
        }
        Ok(())
    }

    fn command(&mut self) -> R<()> {
        const MESSAGE: usize = 101;
        const CHOICES: usize = 102;
        const STRING_CONDITION: usize = 112;
        const SET_STRING: usize = 122;
        const PICTURE: usize = 150;
        let args = self.u8()? as usize - 1;
        let code = self.u32()?;
        let first = if args > 0 { field(self.b, self.p)? } else { 0 };
        self.skip(4 * args + 1)?;
        let strings: Vec<_> = (0..self.u8()?).map(|_| self.string()).collect::<R<_>>()?;
        match self.u8()? {
            0 => {}
            1 => {
                self.skip(6)?;
                self.route()?;
            }
            _ => return Err(format!("unexpected command end in a Wolf RPG file at {}", self.p - 1)),
        }
        for (i, s) in strings.into_iter().enumerate() {
            match code {
                MESSAGE if i == 0 => self.note(s, true, false),
                CHOICES => self.note(s, false, false),
                PICTURE if i == 0 && first >> 4 & 7 == 2 => self.note(s, false, false),
                SET_STRING | STRING_CONDITION if i == 0 => self.note(s, false, true),
                _ => {}
            }
        }
        Ok(())
    }

    fn commands(&mut self) -> R<()> {
        for _ in 0..self.u32()? {
            self.command()?;
        }
        Ok(())
    }

    fn page(&mut self) -> R<()> {
        self.u32()?;
        self.string()?;
        self.skip(4 + 37 + 4 + 2)?;
        self.route()?;
        self.commands()?;
        let features = self.u32()?;
        self.skip(3 + (features > 3) as usize)?;
        self.expect(0x7a)
    }

    fn event(&mut self) -> R<()> {
        self.skip(8)?;
        self.string()?;
        self.skip(8)?;
        let pages = self.u32()?;
        self.skip(4)?;
        let mut seen = 0;
        while self.u8()? == 0x79 {
            self.page()?;
            seen += 1;
        }
        if seen != pages || self.b[self.p - 1] != 0x70 {
            return Err("bad event in a Wolf RPG map".into());
        }
        Ok(())
    }

    fn map(&mut self) -> R<()> {
        self.skip(20)?;
        let version = self.u32()?;
        if version >= 0x65 {
            return Err("this Wolf RPG version's maps aren't supported yet".into());
        }
        self.skip(1)?;
        self.string()?;
        self.u32()?;
        let (w, h, events) = (self.u32()?, self.u32()?, self.u32()?);
        self.skip(w * h * 12)?;
        let mut seen = 0;
        while self.u8()? == 0x6f {
            self.event()?;
            seen += 1;
        }
        if seen != events || self.b[self.p - 1] != 0x66 || self.p != self.b.len() {
            return Err("bad Wolf RPG map".into());
        }
        Ok(())
    }

    fn common_event(&mut self) -> R<()> {
        self.expect(0x8e)?;
        self.skip(4 + 4 + 7)?;
        self.string()?;
        self.commands()?;
        self.string()?;
        self.string()?;
        self.expect(0x8f)?;
        self.strings()?;
        let n = self.u32()?;
        self.skip(n)?;
        for _ in 0..self.u32()? {
            self.strings()?;
        }
        for _ in 0..self.u32()? {
            let n = self.u32()?;
            self.skip(4 * n)?;
        }
        self.skip(0x1d)?;
        for _ in 0..100 {
            self.string()?;
        }
        self.expect(0x91)?;
        self.string()?;
        match self.u8()? {
            0x91 => Ok(()),
            0x92 => {
                self.string()?;
                self.skip(4)?;
                self.expect(0x92)
            }
            _ => Err("bad common event".into()),
        }
    }

    fn common_events(&mut self) -> R<()> {
        self.skip(11)?;
        for _ in 0..self.u32()? {
            self.common_event()?;
        }
        self.skip(1)?;
        if self.p == self.b.len() { Ok(()) } else { Err("CommonEvent.dat has data after its end".into()) }
    }

    /// A database's values: a list of field kinds, then for each row its numbers and its strings.
    fn database(&mut self) -> R<()> {
        const STRING_START: usize = 0x7d0;
        const NAMED: usize = 0x1d4c0;
        self.skip(10)?;
        let version = self.b[10];
        self.skip(1)?;
        for _ in 0..self.u32()? {
            self.skip(4)?;
            let kind = self.u32()?;
            let fields = self.u32()?;
            if kind == NAMED {
                self.string()?;
            }
            let mut strings = 0;
            for _ in 0..fields {
                strings += (self.u32()? >= STRING_START) as usize;
            }
            for _ in 0..self.u32()? {
                self.skip(4 * (fields - strings))?;
                for _ in 0..strings {
                    let s = self.string()?;
                    self.note(s, false, true);
                }
            }
        }
        self.expect(version)?;
        if self.p == self.b.len() { Ok(()) } else { Err("a Wolf RPG database has data after its end".into()) }
    }
}

/// Every string in a .project file, which is every name a script can look a table up by.
fn project_names(b: &[u8]) -> HashSet<String> {
    // shortcut: a length that happens to fit picks up junk too, which only leaves a few lines Japanese
    let (mut names, mut p) = (HashSet::new(), 0);
    while p + 5 < b.len() {
        let n = field(b, p).unwrap_or(0);
        if (2..200).contains(&n) && p + 4 + n <= b.len() && b[p + 3 + n] == 0 {
            names.insert(encoding_rs::SHIFT_JIS.decode_without_bom_handling(&b[p + 4..p + 3 + n]).0.into_owned());
            p += 4 + n;
        } else {
            p += 1;
        }
    }
    names
}

fn scan(path: &str, b: &[u8]) -> R<Vec<Found>> {
    let mut s = Scan { b, p: 0, found: Vec::new() };
    let low = path.to_lowercase();
    let done = if low.ends_with(".mps") {
        s.map()
    } else if low.ends_with("commonevent.dat") {
        s.common_events()
    } else {
        s.database()
    };
    done.map_err(|e| format!("{path}: {e}"))?;
    Ok(s.found)
}

fn is_data(path: &str) -> bool {
    let low = path.to_lowercase();
    low.ends_with(".mps") || ["commonevent.dat", "database.dat", "cdatabase.dat", "sysdatabase.dat"].iter().any(|n| low.ends_with(n))
}

/// A path, or a font's name: things the game loads rather than shows.
static NOT_TEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[/\\][^\\]*\.\w{2,4}$|\.(?:png|jpe?g|bmp|ogg|mp3|wav|mid|txt|ttf|otf)$|^[\w/ ]+/|ゴシック$|明朝$").unwrap());

/// A name on the line above a quote, as these games write speech.
static SPOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^([^\n\\「『（]{1,12})\n([「『（].*)$").unwrap());

/// Who says a message and what, when it's written that way.
fn spoken(f: &Found) -> Option<(String, String)> {
    SPOKEN.captures(&f.text).filter(|c| f.message && looks_like_name(&c[1])).map(|c| (c[1].to_string(), c[2].to_string()))
}

/// One that lost its name to the speaker field has a key of its own, as a translation saved before
/// that still holds the name.
fn said_key(text: &str) -> String {
    format!("said:{text}")
}

/// What the model translates, if `f` is worth it: Japanese, not a file, and not a name scripts look up.
fn shown(f: &Found, names: &HashSet<String>) -> bool {
    JA.is_match(&f.text) && !NOT_TEXT.is_match(&f.text) && !(f.lookup && names.contains(&f.text))
}

/// `\c[2]`, `\E`, `\f[20]` and friends, which the model has to leave alone.
pub static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\[A-Za-z]+(?:\[[^\]]*\])?|\\[.!<>^|]|@\d+|<[CLR]>").unwrap());

/// The game: its archive, as shipped, and what's in it.
pub struct Game {
    data: Vec<u8>,
    archive: Archive,
    names: HashSet<String>,
    files: Vec<(usize, Vec<u8>, Vec<Found>)>,
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let data = original(&game.join(ARCHIVE))?;
        let archive = Archive::read(&data)?;
        let mut names = HashSet::new();
        let mut files = Vec::new();
        for (i, item) in archive.items.iter().enumerate() {
            if item.path.to_lowercase().ends_with(".project") {
                names.extend(project_names(&archive.file(&data, item)?));
            } else if is_data(&item.path) {
                let b = archive.file(&data, item)?;
                let found = scan(&item.path, &b)?;
                files.push((i, b, found));
            }
        }
        Ok(Game { data, archive, names, files })
    }

    /// Each text once, in the order the game holds them: equal texts must stay equal, or a script
    /// comparing two of them would stop matching.
    pub fn lines(&self) -> Vec<Line> {
        let mut seen = HashSet::new();
        let found = self.files.iter().flat_map(|(_, _, f)| f).filter(|f| shown(f, &self.names));
        found.filter(|f| seen.insert(&f.text)).map(|f| match spoken(f) {
            Some((who, text)) => (said_key(&f.text), Some(who), text),
            None => (f.text.clone(), None, f.text.clone()),
        }).collect()
    }

    /// How wide the game's own messages run, in half-width letters: its window's width, near enough.
    fn window(&self) -> usize {
        let plain = |line: &str| text_units(&MARKUP.replace_all(line, ""));
        let messages = self.files.iter().flat_map(|(_, _, f)| f).filter(|f| f.message);
        messages.flat_map(|f| f.text.lines().map(plain).collect::<Vec<_>>()).max().unwrap_or(50).max(30)
    }
}

/// The string as the file stores it.
fn stored(text: &str) -> Vec<u8> {
    let bytes = sjis(text);
    let mut out = (bytes.len() as u32 + 1).to_le_bytes().to_vec();
    out.extend(bytes);
    out.push(0);
    out
}

/// The file with translated strings spliced in. Nothing in these files counts bytes, so that's safe.
fn spliced(b: &[u8], found: &[Found], names: &HashSet<String>, width: usize, tr: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> (Vec<u8>, usize) {
    let (mut out, mut from, mut done) = (Vec::with_capacity(b.len()), 0, 0);
    for f in found.iter().filter(|f| shown(f, names)) {
        let said = spoken(f);
        let Some(new) = tr(&said.as_ref().map_or_else(|| f.text.clone(), |_| said_key(&f.text))) else { continue };
        out.extend(&b[from..f.at]);
        let new = ascii_punctuation(&new);
        let text = if f.message { wrap_words(&new, width).join("\n") } else { new };
        out.extend(stored(&said.map_or_else(|| text.clone(), |(who, _)| format!("{}\n{text}", name(&who).unwrap_or(who)))));
        from = f.end;
        done += 1;
    }
    out.extend(&b[from..]);
    (out, done)
}

/// Rebuilds Data.wolf from the game's own, with `tr` wherever it has a translation and `name` for the
/// speakers, and checks every changed file still reads. Returns how many strings were written.
pub fn install(game: &Path, tr: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let width = crate::fit::letters(game, g.window());
    let (mut new, mut done) = (Vec::new(), 0);
    for (i, b, found) in &g.files {
        let (bytes, n) = spliced(b, found, &g.names, width, tr, name);
        if n > 0 {
            scan(&g.archive.items[*i].path, &bytes)?;
            new.push((*i, bytes));
            done += n;
        }
    }
    let path = game.join(ARCHIVE);
    let out = g.archive.with(&g.data, &new)?;
    let back = Archive::read(&out)?;
    for (i, bytes) in &new {
        if back.file(&out, &back.items[*i])? != *bytes {
            return Err(format!("{}: a file didn't read back as written", back.items[*i].path));
        }
    }
    replace_file(&path, &out)?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Vec<u8> {
        stored(text)
    }

    /// One command: a code, its number arguments, and its strings.
    fn command(code: u32, args: &[u32], strings: &[&str]) -> Vec<u8> {
        let mut b = vec![args.len() as u8 + 1];
        b.extend(code.to_le_bytes());
        args.iter().for_each(|a| b.extend(a.to_le_bytes()));
        b.push(0);
        b.push(strings.len() as u8);
        strings.iter().for_each(|t| b.extend(s(t)));
        b.push(0);
        b
    }

    fn map(commands: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![0; 20];
        b.extend(0x64u32.to_le_bytes());
        b.push(0);
        b.extend(s(""));
        b.extend(0u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend(1u32.to_le_bytes());
        b.extend([0; 12]);
        b.push(0x6f);
        b.extend([0; 8]);
        b.extend(s("event"));
        b.extend([0; 8]);
        b.extend(1u32.to_le_bytes());
        b.extend([0; 4]);
        b.push(0x79);
        b.extend([0; 4]);
        b.extend(s(""));
        b.extend([0; 47]);
        b.extend(0u32.to_le_bytes());
        b.extend((commands.len() as u32).to_le_bytes());
        commands.iter().for_each(|c| b.extend(c));
        b.extend(0u32.to_le_bytes());
        b.extend([0; 3]);
        b.push(0x7a);
        b.push(0x70);
        b.push(0x66);
        b
    }

    /// An archive with these files in its one folder, packed with the 2.10 key.
    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let key = key_bytes(KEYS[1]);
        let mut names = Vec::new();
        let mut name_at = Vec::new();
        for (n, _) in files {
            name_at.push(names.len() as u32);
            let bytes = n.as_bytes();
            let packs = bytes.len().div_ceil(4).max(1);
            names.extend((packs as u16).to_le_bytes());
            names.extend([0, 0]);
            let mut pad = bytes.to_vec();
            pad.resize(packs * 4, 0);
            names.extend(&pad);
            names.extend(bytes);
            names.push(0);
        }
        names.resize(names.len().next_multiple_of(4), 0);
        let (mut data, mut entries) = (Vec::new(), Vec::new());
        let mut place = 0u32;
        for (i, (_, b)) in files.iter().enumerate() {
            entries.extend(name_at[i].to_le_bytes());
            entries.extend(0x20u32.to_le_bytes());
            entries.extend([0; 24]);
            entries.extend(place.to_le_bytes());
            entries.extend((b.len() as u32).to_le_bytes());
            entries.extend(NOT_PACKED.to_le_bytes());
            let mut c = b.to_vec();
            xor(&mut c, HEAD as u64 + place as u64, &key);
            data.extend(c);
            place += b.len() as u32;
        }
        let mut dir = vec![0xff; 8];
        dir.extend((files.len() as u32).to_le_bytes());
        dir.extend(0u32.to_le_bytes());
        let mut tables = names.clone();
        let files_at = tables.len() as u32;
        tables.extend(&entries);
        let dirs_at = tables.len() as u32;
        tables.extend(&dir);
        let at = HEAD + data.len();
        let mut head = b"DX\x03\0".to_vec();
        for v in [tables.len() as u32, HEAD as u32, at as u32, files_at, dirs_at] {
            head.extend(v.to_le_bytes());
        }
        xor(&mut head, 0, &key);
        xor(&mut tables, at as u64, &key);
        [head, data, tables].concat()
    }

    fn game(files: &[(&str, &[u8])]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-wolf-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(ARCHIVE), archive(files)).unwrap();
        dir
    }

    #[test]
    fn packed_data_comes_back() {
        // "abcabcabcabc": three letters, then a copy of nine from three back, and a key byte kept
        let run = 9 - 4;
        let code = run << 3;
        let body = [b'a', b'b', b'c', 0xf0, code as u8, 2, 0xf0, 0xf0];
        let mut b = (13u32).to_le_bytes().to_vec();
        b.extend((9 + body.len() as u32).to_le_bytes());
        b.push(0xf0);
        b.extend(body);
        assert_eq!(unpack(&b, 13).unwrap(), b"abcabcabcabc\xf0");
    }

    #[test]
    fn messages_and_choices_are_found_and_files_are_not() {
        let m = map(&[
            command(101, &[], &["こんにちは"]),
            command(102, &[], &["はい", "いいえ"]),
            command(103, &[], &["注釈"]),
            command(150, &[0x20], &["画面の文字"]),
            command(150, &[0x00], &["画像/立ち絵.png"]),
            command(122, &[], &["画像/顔.png"]),
        ]);
        let found = scan("MapData/a.mps", &m).unwrap();
        let shown: Vec<&str> = found.iter().filter(|f| shown(f, &HashSet::new())).map(|f| f.text.as_str()).collect();
        assert_eq!(shown, ["こんにちは", "はい", "いいえ", "画面の文字"]);
    }

    #[test]
    fn a_name_scripts_look_up_stays_and_the_same_words_stay_equal() {
        let m = map(&[command(122, &[], &["道具"]), command(122, &[], &["はい"]), command(102, &[], &["はい"])]);
        let dir = game(&[("BasicData/DataBase.project", &s("道具")), ("MapData/a.mps", &m)]);
        let g = Game::open(&dir).unwrap();
        assert_eq!(g.lines(), [("はい".to_string(), None, "はい".to_string())]);
    }

    #[test]
    fn a_name_above_a_quote_is_the_speaker() {
        let m = map(&[command(101, &[], &["太郎\n「やあ」"]), command(101, &[], &["地の文の\n続きです"]), command(101, &[], &["\\cself[1]\n「やあ」"])]);
        let dir = game(&[("MapData/a.mps", &m)]);
        let lines = Game::open(&dir).unwrap().lines();
        let shown: Vec<_> = lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect();
        assert_eq!(shown, [(Some("太郎"), "「やあ」"), (None, "地の文の\n続きです"), (None, "\\cself[1]\n「やあ」")]);
        let tr = |k: &str| k.starts_with("said:").then(|| "\"Hi\"".to_string());
        assert_eq!(install(&dir, &tr, &|n| (n == "太郎").then(|| "Taro".to_string())).unwrap(), 1);
        let out = fs::read(dir.join(ARCHIVE)).unwrap();
        let a = Archive::read(&out).unwrap();
        let found = scan("MapData/a.mps", &a.file(&out, &a.items[0]).unwrap()).unwrap();
        assert_eq!(found[0].text, "Taro\n\"Hi\"");
    }

    #[test]
    fn translations_go_in_wrapped_and_the_archive_reads_back() {
        let m = map(&[command(101, &[], &["あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほ"]), command(102, &[], &["はい"])]);
        let other: &[u8] = b"an untouched image";
        let dir = game(&[("Picture/a.png", other), ("MapData/a.mps", &m)]);
        let tr = |t: &str| Some(if t == "はい" { "Yes…".to_string() } else { "word ".repeat(20) });
        assert_eq!(install(&dir, &tr, &|_| None).unwrap(), 2);

        let out = fs::read(dir.join(ARCHIVE)).unwrap();
        let a = Archive::read(&out).unwrap();
        assert_eq!(a.file(&out, &a.items[0]).unwrap(), other);
        let b = a.file(&out, &a.items[1]).unwrap();
        let found = scan("MapData/a.mps", &b).unwrap();
        assert_eq!(found[1].text, "Yes...");
        assert!(found[0].text.lines().count() > 1 && found[0].text.lines().all(|l| l.len() <= 60));
        assert!(!original(&dir.join(ARCHIVE)).unwrap().is_empty());
    }
}
