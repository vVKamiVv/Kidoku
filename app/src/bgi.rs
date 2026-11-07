//! BGI/Ethornell engine (data*.arc, BURIKO ARC20 or the older PackFile): scripts packed with a keyed
//! Huffman and LZ scheme.
//! Compiled code refers to strings by offset into a block at the end of the script, so new text can
//! go on the end and only those offsets change.

use crate::artemis::painted;
use crate::liar::Image;
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, looks_like_name, sjis, wrap_words, JA};
use crate::{backup, extend_file, R};
use encoding_rs::SHIFT_JIS;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use regex::Regex;

/// Furigana, `<rreading>word</r>`: small Japanese over a word. English needs none, so only the word is kept.
static RUBY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<r[^>]*>(.*?)</r>").unwrap());

fn without_ruby(text: &str) -> String {
    RUBY.replace_all(text, "$1").into_owned()
}

fn word(b: &[u8], p: usize) -> R<usize> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated BGI file")?.try_into().unwrap()) as usize)
}

// ---------- packing ----------

struct Bits<'a> {
    b: &'a [u8],
    p: usize,
    bit: u32,
}

impl Bits<'_> {
    fn next(&mut self) -> R<usize> {
        let byte = *self.b.get(self.p).ok_or("truncated BGI data")?;
        let v = byte >> (7 - self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.p += 1;
        }
        Ok(v as usize)
    }

    fn take(&mut self, n: u32) -> R<usize> {
        (0..n).try_fold(0, |v, _| Ok(v << 1 | self.next()?))
    }
}

/// `DSC FORMAT 1.00`: 512 code lengths under a running key, then Huffman codes for bytes and copies.
fn unpack(b: &[u8]) -> R<Vec<u8>> {
    const BAD: &str = "bad packed BGI data";
    let magic = (u16::from_le_bytes(b.get(..2).ok_or(BAD)?.try_into().unwrap()) as u32) << 16;
    let (mut key, size, count) = (word(b, 0x10)? as u32, word(b, 0x14)?, word(b, 0x18)?);
    let mut next_key = || {
        let v0 = 20021u32.wrapping_mul(key & 0xffff);
        let v1 = (magic | key >> 16).wrapping_mul(20021).wrapping_add(key.wrapping_mul(346));
        let v1 = v1.wrapping_add(v0 >> 16) & 0xffff;
        key = (v1 << 16).wrapping_add(v0 & 0xffff).wrapping_add(1);
        v1 as u8
    };
    let mut codes: Vec<(u8, u16)> = Vec::new();
    for i in 0..512u16 {
        let depth = b.get(0x20 + i as usize).ok_or(BAD)?.wrapping_sub(next_key());
        if depth != 0 {
            codes.push((depth, i));
        }
    }
    codes.sort();
    // a tree in an array: (is it a parent, code or left child, right child)
    let mut nodes = vec![(false, 0usize, 0usize); 1023];
    let (mut level, mut next_level): (Vec<usize>, Vec<usize>) = (vec![0], Vec::new());
    let (mut next_node, mut depth, mut n) = (1, 0u8, 0);
    while n < codes.len() {
        let mut used = 0;
        while n < codes.len() && codes[n].0 == depth {
            *nodes.get_mut(*level.get(used).ok_or(BAD)?).ok_or(BAD)? = (false, codes[n].1 as usize, 0);
            used += 1;
            n += 1;
        }
        next_level.clear();
        for &slot in level.get(used..).ok_or(BAD)? {
            *nodes.get_mut(slot).ok_or(BAD)? = (true, next_node, next_node + 1);
            next_level.extend([next_node, next_node + 1]);
            next_node += 2;
        }
        std::mem::swap(&mut level, &mut next_level);
        depth += 1;
    }
    let mut bits = Bits { b, p: 0x220, bit: 0 };
    let mut out = Vec::with_capacity(size);
    for _ in 0..count {
        let mut at = 0;
        while nodes[at].0 {
            at = if bits.next()? == 0 { nodes[at].1 } else { nodes[at].2 };
        }
        let code = nodes[at].1;
        if code < 256 {
            out.push(code as u8);
        } else {
            let back = bits.take(12)? + 2;
            let from = out.len().checked_sub(back).ok_or(BAD)?;
            for i in 0..(code & 0xff) + 2 {
                out.push(out[from + i]);
            }
        }
    }
    if out.len() != size { Err(BAD.into()) } else { Ok(out) }
}

/// `BSE 1.0`: a 16-byte header, then the file's first 64 bytes shuffled under a seed.
const BSE: &[u8] = b"BSE 1.0\0";

/// The file inside a BSE wrapper, which the engine reads as well as the bare file, so ours go in bare.
fn unwrap_bse(b: &[u8]) -> Option<Vec<u8>> {
    let mut seed = i32::from_le_bytes(b.get(12..16)?.try_into().ok()?);
    let mut data = b.get(16..80)?.to_vec();
    let mut rand = || {
        let t = ((seed.wrapping_mul(257) >> 8).wrapping_add(seed.wrapping_mul(97)).wrapping_add(23)) as u32 ^ 0xA6CD_9B75;
        seed = t.rotate_right(16) as i32;
        (seed & 0x7FFF) as usize
    };
    let mut done = [false; 64];
    for _ in 0..64 {
        let mut i = rand() % 64;
        while done[i] {
            i = (i + 1) % 64;
        }
        let (shift, key) = (rand() % 8, rand());
        let v = data[i].wrapping_sub(rand() as u8);
        data[i] = if key & 1 == 1 { v.rotate_left(shift as u32) } else { v.rotate_right(shift as u32) };
        done[i] = true;
    }
    let sum = data.iter().fold(0u8, |s, &c| s.wrapping_add(c));
    let xor = data.iter().fold(0u8, |s, &c| s ^ c);
    (sum == b[10] && xor == b[11]).then(|| [&data, &b[80..]].concat())
}

// ---------- archives ----------

const TOP: usize = 0x10;

/// One file of an archive.
pub struct Item {
    pub name: String,
    row: usize,
    place: usize,
    size: usize,
}

pub struct Archive {
    path: PathBuf,
    /// Where a row's place and size sit: after the name, 0x60 bytes in ARC20 and 0x10 in PackFile.
    name_len: usize,
    head: Vec<u8>,
    len: u64,
    pub items: Vec<Item>,
}

impl Archive {
    /// Always the game's own archive: the backup, once there is one.
    fn open(path: &Path) -> R<Option<Archive>> {
        let own = if backup(path).exists() { backup(path) } else { path.to_path_buf() };
        Self::read(&own, path)
    }

    fn open_as_written(path: &Path) -> R<Archive> {
        Self::read(path, path)?.ok_or("the new archive doesn't open".to_string())
    }

    fn read(from: &Path, path: &Path) -> R<Option<Archive>> {
        let err = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut f = fs::File::open(from).map_err(err)?;
        let len = f.metadata().map_err(err)?.len();
        let mut top = [0; TOP];
        f.read_exact(&mut top).map_err(err)?;
        let (row_len, name_len) = match &top[..12] {
            b"BURIKO ARC20" => (0x80, 0x60),
            b"PackFile    " => (0x20, 0x10),
            _ => return Ok(None),
        };
        let count = word(&top, 12)?;
        let base = TOP + row_len * count;
        if base as u64 > len {
            return Err(format!("{}: the archive's index runs past its end", path.display()));
        }
        let mut head = top.to_vec();
        head.resize(base, 0);
        f.read_exact(&mut head[TOP..]).map_err(err)?;
        let mut items = Vec::with_capacity(count);
        for i in 0..count {
            let row = TOP + row_len * i;
            let raw = &head[row..row + name_len];
            let name = SHIFT_JIS.decode_without_bom_handling(&raw[..raw.iter().position(|&c| c == 0).unwrap_or(name_len)]).0.into_owned();
            items.push(Item { name, row, place: base + word(&head, row + name_len)?, size: word(&head, row + name_len + 4)? });
        }
        Ok(Some(Archive { path: path.to_path_buf(), name_len, head, len, items }))
    }

    fn file(&self, f: &mut fs::File, item: &Item) -> R<Vec<u8>> {
        let mut b = vec![0; item.size];
        f.seek(SeekFrom::Start(item.place as u64)).and_then(|_| f.read_exact(&mut b)).map_err(|e| format!("{}: {e}", item.name))?;
        let b = if b.starts_with(BSE) { unwrap_bse(&b).ok_or(format!("{}: bad BSE header", item.name))? } else { b };
        if b.starts_with(b"DSC FORMAT 1.00") { unpack(&b) } else { Ok(b) }
    }

    /// New files in place of the old, stored unpacked on the end, with the index rewritten last.
    fn replace(&self, new: &[(usize, Vec<u8>)]) -> R<()> {
        let mut head = self.head.clone();
        let (mut tail, mut place) = (Vec::new(), self.len as usize - self.head.len());
        for (i, bytes) in new {
            let at = self.items[*i].row + self.name_len;
            head[at..at + 4].copy_from_slice(&(place as u32).to_le_bytes());
            head[at + 4..at + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            tail.extend(bytes);
            place += bytes.len();
        }
        extend_file(&self.path, self.len, &self.head, &head, &tail)
    }
}

// ---------- scripts ----------

const MAGIC: &[u8] = b"BurikoCompiledScriptVer1.00\0";
const PUSH_STRING: u32 = 3;
const PUSH_FIRST: u32 = 0;
const PUSH_LAST: u32 = 3;
const MESSAGE: u32 = 0x140;
/// The largest file looked at as a script without a header.
const HEADERLESS_MAX: usize = 16 << 20;

/// Whether a file's first bytes could start a script without a header: an instruction, and not
/// the start of a sound (`bw  `).
fn maybe_code(top: &[u8]) -> bool {
    word(top, 0).is_ok_and(|op| op < 0x400) && top.get(4..8) != Some(b"bw  ")
}

/// The engine's selection calls, which take the choices' texts.
const CHOICES: std::ops::Range<u32> = 0x160..0x180;
// shortcut: the text box isn't in the files; the game's longest lines suggest about 28 full-width characters
const LINE_LETTERS: usize = 56;

/// A message: where the code pushes its text and speaker.
#[derive(Debug, PartialEq)]
struct Message {
    text: usize,
    name: Option<usize>,
}

/// A script, unpacked: offsets in the code count from `base`, and strings sit after `end`.
struct Script {
    archive: usize,
    item: usize,
    name: String,
    plain: Vec<u8>,
    base: usize,
    messages: Vec<Message>,
}

/// The text a push instruction points at.
fn string_at(plain: &[u8], base: usize, push: usize) -> String {
    let at = base + word(plain, push + 4).unwrap_or(0);
    let tail = plain.get(at..).unwrap_or(&[]);
    let raw = &tail[..tail.iter().position(|&c| c == 0).unwrap_or(tail.len())];
    SHIFT_JIS.decode_without_bom_handling(raw).0.into_owned()
}

fn opcode(b: &[u8], p: usize) -> u32 {
    word(b, p).unwrap_or(u32::MAX as usize) as u32
}

impl Script {
    fn read(archive: usize, item: usize, name: String, plain: Vec<u8>) -> R<Script> {
        let base = MAGIC.len() + word(&plain, MAGIC.len())?;
        Ok(Self::parse(archive, item, name, plain, base))
    }

    /// Older games' scripts have no header and start with code. Taken only if it has messages, since
    /// nothing else marks a file as one.
    fn headerless(archive: usize, item: usize, name: String, plain: Vec<u8>) -> Option<Script> {
        let s = Self::parse(archive, item, name, plain, 0);
        let mut lines = Vec::new();
        s.lines(&mut lines);
        (!lines.is_empty()).then_some(s)
    }

    fn parse(archive: usize, item: usize, name: String, plain: Vec<u8>, base: usize) -> Script {
        // code has runs of zeros, and text doesn't
        let mut end = plain.len();
        while end >= 3 && plain[end - 3..end] != [0, 0, 0] {
            end -= 1;
        }
        let string = |i: usize| {
            let at = base + word(&plain, i + 4).ok()?;
            (opcode(&plain, i) == PUSH_STRING && at >= end && at < plain.len() && plain[at - 1] == 0).then_some(at)
        };
        let mut messages = Vec::new();
        let mut i = base;
        while i + 8 <= end {
            if string(i).is_some() {
                // every value the call takes is pushed first, so find what comes after them
                let (mut p, mut strings) = (i, vec![i]);
                while p + 16 <= end && (PUSH_FIRST..=PUSH_LAST).contains(&opcode(&plain, p + 8)) {
                    p += 8;
                    if string(p).is_some() {
                        strings.push(p);
                    }
                }
                if opcode(&plain, p + 8) == MESSAGE {
                    let text_at = strings.len().min(2) - 1;
                    let name = (text_at == 1).then_some(strings[0]);
                    match name.filter(|&n| !looks_like_name(&string_at(&plain, base, n))) {
                        // a few calls pass two texts, and a name that reads like a sentence is one
                        Some(first) => messages.extend([first, strings[1]].map(|text| Message { text, name: None })),
                        None => messages.push(Message { text: strings[text_at], name }),
                    }
                } else if CHOICES.contains(&opcode(&plain, p + 8)) {
                    messages.extend(strings.iter().map(|&text| Message { text, name: None }));
                }
                i = p + 8;
            } else {
                i += 4;
            }
        }
        Script { archive, item, name, plain, base, messages }
    }

    fn string(&self, push: usize) -> String {
        string_at(&self.plain, self.base, push)
    }

    fn lines(&self, out: &mut Vec<Line>) {
        for m in &self.messages {
            let text = self.string(m.text);
            if JA.is_match(&text) {
                let who = m.name.map(|n| self.string(n)).filter(|n| JA.is_match(n));
                out.push((format!("{}:{}", self.name, m.text), who, without_ruby(&text.replace('\n', ""))));
            }
        }
    }

    /// The script with new strings on the end and the code's offsets pointed at them.
    fn with(&self, width: usize, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> (Vec<u8>, usize) {
        let mut out = self.plain.clone();
        let mut done = 0;
        let put = |out: &mut Vec<u8>, push: usize, text: &str| {
            let at = (out.len() - self.base) as u32;
            out[push + 4..push + 8].copy_from_slice(&at.to_le_bytes());
            out.extend(sjis(text));
            out.push(0);
        };
        for m in &self.messages {
            let Some(tr) = line(&format!("{}:{}", self.name, m.text)) else { continue };
            put(&mut out, m.text, &wrap_words(&ascii_punctuation(&without_ruby(&tr)), width).join("\n"));
            done += 1;
            if let Some((n, en)) = m.name.and_then(|n| Some((n, name(&self.string(n))?))) {
                put(&mut out, n, &en);
                done += 1;
            }
        }
        (out, done)
    }
}

// ---------- system programs ----------

/// The archive of system programs: the menus, dialogs and settings screens.
const PROGRAMS: &str = "sysprg.arc";
/// In a system program, pushes the string as far from itself as the signed 16-bit value after it says.
const PUSH_TEXT: u8 = 0x05;

/// Where a system program's code pushes text, found by following the code from its start as the
/// engine runs it: a 0x05 inside another instruction's operand is no push. Functions only reached
/// through tables start right after a return, by taking stack room (0x10). None if the code holds an
/// instruction this doesn't know, and then nothing in it is changed.
fn text_pushes(plain: &[u8]) -> Option<Vec<usize>> {
    let (mut todo, mut seen, mut out, mut rets) = (vec![16usize], BTreeSet::new(), Vec::new(), Vec::new());
    loop {
        if todo.is_empty() {
            todo.extend(rets.drain(..).map(|r: usize| r + 1).filter(|&f| !seen.contains(&f) && plain.get(f) == Some(&0x10)));
            if todo.is_empty() {
                break;
            }
        }
        let Some(mut p) = todo.pop() else { break };
        while seen.insert(p) {
            let op = *plain.get(p)?;
            let next = match op {
                PUSH_TEXT => {
                    out.push(p);
                    p + 3
                }
                // a code address: a jump, a branch, a call or just a value, by what follows it
                0x06 => {
                    let to = p as isize + i16::from_le_bytes([*plain.get(p + 1)?, *plain.get(p + 2)?]) as isize;
                    todo.push(usize::try_from(to).ok()?);
                    match *plain.get(p + 3)? {
                        0x14 => break,
                        0x15 => {
                            todo.push(p + 5);
                            break;
                        }
                        0x16 => p + 4,
                        _ => p + 3,
                    }
                }
                0x17 => {
                    rets.push(p);
                    break;
                }
                0x00 | 0x08..=0x0a | 0x80 | 0x81 | 0x90..=0x92 | 0xa0 | 0xb0 | 0xc0 | 0xff => p + 2,
                0x01 | 0x04 | 0x0c => p + 3,
                0x02 => p + 5,
                0x0b => p + 2 + *plain.get(p + 1)? as usize,
                0x10 | 0x11 | 0x14..=0x16 | 0x20..=0x2b | 0x30..=0x35 | 0x38..=0x3a | 0x40 | 0x42..=0x45 | 0x48 | 0x49 | 0x50..=0x5b | 0x5d..=0x71 | 0x74 | 0x75 | 0x77..=0x7f | 0xd0 | 0xe0 => p + 1,
                _ => return None,
            };
            p = next;
        }
    }
    Some(out)
}

/// In font names, which go to Windows and have to stay as Windows knows them.
const FONT_WORDS: [&str; 2] = ["ゴシック", "明朝"];

/// What a string in a system program reads as, if it's Japanese a player would see: not a font name
/// and not a file.
fn shown_text(raw: &[u8]) -> Option<String> {
    let (text, _, bad) = SHIFT_JIS.decode(raw);
    let looks_like_file = text.contains(['\\', '/']) || text.rsplit_once('.').is_some_and(|(_, ext)| (1..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    let ok = !bad && JA.is_match(&text) && !text.chars().any(|c| c.is_control() && c != '\n') && !FONT_WORDS.iter().any(|f| text.contains(f)) && !looks_like_file;
    ok.then(|| text.into_owned())
}

/// A system program (`._bp`): a 16-byte header giving the size of the rest, then code with its
/// strings at the end, each reached by PUSH_TEXT.
struct Program {
    archive: usize,
    item: usize,
    name: String,
    plain: Vec<u8>,
    /// Each Japanese string's start, and the pushes that reach it.
    strings: BTreeMap<usize, Vec<usize>>,
}

impl Program {
    fn read(archive: usize, item: usize, name: String, plain: Vec<u8>) -> Option<Program> {
        if plain.len() <= 16 || word(&plain, 0).ok()? != 16 || word(&plain, 4).ok()? != plain.len() - 16 {
            return None;
        }
        let mut strings: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for q in text_pushes(&plain).unwrap_or_default() {
            let at = q as isize + i16::from_le_bytes([plain[q + 1], plain[q + 2]]) as isize;
            let at = usize::try_from(at).ok().filter(|&at| at > q + 3 && at < plain.len());
            if let Some(at) = at.filter(|&at| shown_text(Self::raw(&plain, at)).is_some()) {
                strings.entry(at).or_default().push(q);
            }
        }
        Some(Program { archive, item, name, plain, strings })
    }

    fn raw(plain: &[u8], at: usize) -> &[u8] {
        let tail = &plain[at..];
        &tail[..tail.iter().position(|&c| c == 0).unwrap_or(tail.len())]
    }

    fn text(&self, at: usize) -> String {
        shown_text(Self::raw(&self.plain, at)).unwrap_or_default()
    }

    fn key(&self, at: usize) -> String {
        format!("{}:{at}", self.name)
    }

    fn lines(&self, out: &mut Vec<Line>) {
        out.extend(self.strings.keys().map(|&at| (self.key(at), None, self.text(at))));
    }

    /// The program with new strings on the end and their pushes pointed at them. A character's name
    /// keeps the spelling it has in the story, so a name the menus compare still matches.
    fn with(&self, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> (Vec<u8>, usize) {
        let mut out = self.plain.clone();
        let mut done = 0;
        for (&at, pushes) in &self.strings {
            let jp = self.text(at);
            let Some(tr) = name(&jp).or_else(|| line(&self.key(at))) else { continue };
            let new_at = out.len();
            // shortcut: a push reaches 32 KB ahead; past that a string keeps its Japanese
            if pushes.iter().any(|&q| new_at - q > i16::MAX as usize) {
                continue;
            }
            for &q in pushes {
                out[q + 1..q + 3].copy_from_slice(&((new_at - q) as i16).to_le_bytes());
            }
            out.extend(sjis(&ascii_punctuation(&tr)));
            out.push(0);
            done += 1;
        }
        let rest = (out.len() - 16) as u32;
        out[4..8].copy_from_slice(&rest.to_le_bytes());
        (out, done)
    }
}

// ---------- interface pictures ----------

/// The archive of the menus' pictures.
const PICTURES: &str = "sysgrp.arc";

/// A picture as BGI stores it: width, height and bits per pixel (24 or 32) in a 16-byte header, then
/// BGR(A) rows from the top; or compressed (CBG).
pub fn picture(b: &[u8]) -> Option<Image> {
    if let Some((w, h, bpp, bgra)) = crate::cbg::decode(b) {
        let px = bgra.as_chunks::<4>().0.iter().map(|&[b, g, r, a]| u32::from_le_bytes([b, g, r, 255 - a])).collect();
        return (bpp == 24 || bpp == 32).then_some(Image { w, h, px });
    }
    let half = |p: usize| u16::from_le_bytes([b[p], b[p + 1]]) as usize;
    if b.len() < 16 || b[6..16].iter().any(|&c| c != 0) {
        return None;
    }
    let (w, h, bpp) = (half(0), half(2), half(4));
    let step = bpp / 8;
    if w == 0 || h == 0 || !(bpp == 24 || bpp == 32) || b.len() != 16 + w * h * step {
        return None;
    }
    // stored alpha is inverted, 0 being opaque
    let px = b[16..].chunks_exact(step).map(|c| u32::from_le_bytes([c[0], c[1], c[2], 255 - c.get(3).unwrap_or(&255)])).collect();
    Some(Image { w, h, px })
}

/// `img` stored as `like` is: the same header, so the same bits per pixel. A compressed picture goes
/// back compressed, as version 1.
pub fn bitmap(like: &[u8], img: &Image) -> Vec<u8> {
    if let Some((_, _, bpp, _)) = crate::cbg::decode(like) {
        let bgra: Vec<u8> = img.px.iter().flat_map(|p| { let [b, g, r, a] = p.to_le_bytes(); [b, g, r, 255 - a] }).collect();
        return crate::cbg::encode(img.w, img.h, bpp, &bgra);
    }
    let step = like[4] as usize / 8;
    let mut out = like[..16].to_vec();
    for p in &img.px {
        let [b, g, r, a] = p.to_le_bytes();
        out.extend(&[b, g, r, 255 - a][..step]);
    }
    out
}

/// The menus' pictures as the game has them: (name, stored bytes).
pub fn interface_pictures(game: &Path) -> R<Vec<(String, Vec<u8>)>> {
    let path = game.join(PICTURES);
    let Some(a) = Archive::open(&path)? else { return Ok(Vec::new()) };
    let mut f = fs::File::open(if backup(&path).exists() { backup(&path) } else { path.clone() }).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for item in &a.items {
        let b = a.file(&mut f, item)?;
        if picture(&b).is_some() {
            out.push((item.name.clone(), b));
        }
    }
    Ok(out)
}

// ---------- the game ----------

pub struct Game {
    archives: Vec<Archive>,
    scripts: Vec<Script>,
    programs: Vec<Program>,
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let (mut archives, mut scripts, mut programs) = (Vec::new(), Vec::new(), Vec::new());
        let mut paths: Vec<PathBuf> = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("arc"))).collect();
        paths.sort();
        for path in paths {
            let Some(a) = Archive::open(&path)? else { continue };
            let mut f = fs::File::open(if backup(&path).exists() { backup(&path) } else { path.clone() }).map_err(|e| e.to_string())?;
            if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(PROGRAMS)) {
                for (i, item) in a.items.iter().enumerate() {
                    let plain = a.file(&mut f, item).map_err(|e| format!("{}: {e}", item.name))?;
                    programs.extend(Program::read(archives.len(), i, item.name.clone(), plain));
                }
                archives.push(a);
                continue;
            }
            for (i, item) in a.items.iter().enumerate() {
                let mut top = [0; 16];
                f.seek(SeekFrom::Start(item.place as u64)).and_then(|_| f.read_exact(&mut top)).map_err(|e| e.to_string())?;
                let wrapped = top.starts_with(b"DSC FORMAT 1.00") || top.starts_with(BSE);
                if !wrapped && !top.starts_with(&MAGIC[..16]) && (item.size > HEADERLESS_MAX || !maybe_code(&top)) {
                    continue;
                }
                let plain = a.file(&mut f, item).map_err(|e| format!("{}: {e}", item.name))?;
                if plain.starts_with(MAGIC) {
                    scripts.push(Script::read(archives.len(), i, item.name.clone(), plain)?);
                } else if plain.len() <= HEADERLESS_MAX && plain.get(..16).is_some_and(maybe_code) {
                    scripts.extend(Script::headerless(archives.len(), i, item.name.clone(), plain));
                }
            }
            archives.push(a);
        }
        Ok(Game { archives, scripts, programs })
    }

    /// The story, then the menus and dialogs.
    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|s| s.lines(&mut out));
        self.programs.iter().for_each(|p| p.lines(&mut out));
        out
    }

    /// What the menus may say, as the game writes it: the system programs' text.
    pub fn menu_words(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.programs.iter().for_each(|p| p.lines(&mut out));
        out.into_iter().map(|l| l.2).collect()
    }

    /// Who speaks in the story.
    pub fn speakers(&self) -> BTreeSet<String> {
        let mut story = Vec::new();
        self.scripts.iter().for_each(|s| s.lines(&mut story));
        story.into_iter().filter_map(|l| l.1).collect()
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the archives and reads
/// them back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    // repainted pictures wait beside the translation, since each write starts from the game's own archive
    let pictures = painted(game)?;
    let mut done = 0;
    for (n, a) in g.archives.iter().enumerate() {
        let mut new = Vec::new();
        if a.path.file_name().is_some_and(|f| f.eq_ignore_ascii_case(PICTURES)) {
            new.extend(pictures.iter().filter_map(|(name, b)| Some((a.items.iter().position(|i| i.name == *name)?, b.clone()))));
        }
        for s in g.scripts.iter().filter(|s| s.archive == n) {
            let (bytes, count) = s.with(crate::fit::letters(game, LINE_LETTERS), line, name);
            if count > 0 {
                new.push((s.item, bytes));
                done += count;
            }
        }
        for p in g.programs.iter().filter(|p| p.archive == n) {
            let (bytes, count) = p.with(line, name);
            if count > 0 {
                new.push((p.item, bytes));
                done += count;
            }
        }
        if new.is_empty() {
            continue;
        }
        a.replace(&new)?;
        let back = Archive::open_as_written(&a.path)?;
        let mut f = fs::File::open(&a.path).map_err(|e| e.to_string())?;
        for (i, bytes) in &new {
            if back.file(&mut f, &back.items[*i])? != *bytes {
                return Err(format!("{} didn't read back as written", back.items[*i].name));
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A script: a named message, a plain one, and a string that isn't text.
    fn script() -> Vec<u8> {
        let strings = ["太郎", "こんにちは", "地の文です", "H:\\project\\script.bs5"];
        let (mut block, mut at) = (Vec::new(), Vec::new());
        for t in strings {
            at.push(block.len() as u32);
            block.extend(sjis(t));
            block.push(0);
        }
        let mut code = Vec::new();
        let mut op = |v: &[u32]| v.iter().for_each(|w| code.extend(w.to_le_bytes()));
        op(&[3, at[0], 3, at[1], MESSAGE]);
        op(&[0, 0, 3, at[2], MESSAGE]);
        op(&[3, at[3], 0xf9, 0, 0, 0]);
        let shift = code.len() as u32;
        let code: Vec<u8> = code.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).enumerate().flat_map(|(i, w)| {
            let is_offset = [1, 3, 8, 11].contains(&i);
            (if is_offset { w + shift } else { w }).to_le_bytes()
        }).collect();
        [MAGIC, &4u32.to_le_bytes(), &code, &block].concat()
    }

    fn archive(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        archive_as(b"BURIKO ARC20", 0x80, 0x60, files)
    }

    fn packfile(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        archive_as(b"PackFile    ", 0x20, 0x10, files)
    }

    fn archive_as(magic: &[u8], row_len: usize, name_len: usize, files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = magic.to_vec();
        out.extend((files.len() as u32).to_le_bytes());
        let mut place = 0u32;
        for (name, b) in files {
            let mut row = vec![0u8; row_len];
            row[..name.len()].copy_from_slice(name.as_bytes());
            row[name_len..name_len + 4].copy_from_slice(&place.to_le_bytes());
            row[name_len + 4..name_len + 8].copy_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend(row);
            place += b.len() as u32;
        }
        files.iter().for_each(|(_, b)| out.extend(b));
        out
    }

    /// `plain` in a BSE wrapper: each of the 64 shuffled bytes undone in the engine's order.
    fn bse(plain: &[u8], seed: i32) -> Vec<u8> {
        let mut data = plain[..64].to_vec();
        let mut s = seed;
        let mut rand = || {
            let t = ((s.wrapping_mul(257) >> 8).wrapping_add(s.wrapping_mul(97)).wrapping_add(23)) as u32 ^ 0xA6CD_9B75;
            s = t.rotate_right(16) as i32;
            (s & 0x7FFF) as usize
        };
        let mut done = [false; 64];
        for _ in 0..64 {
            let mut i = rand() % 64;
            while done[i] {
                i = (i + 1) % 64;
            }
            let (shift, key, add) = (rand() % 8, rand(), rand() as u8);
            let v = if key & 1 == 1 { data[i].rotate_right(shift as u32) } else { data[i].rotate_left(shift as u32) };
            data[i] = v.wrapping_add(add);
            done[i] = true;
        }
        let sum = plain[..64].iter().fold(0u8, |a, &c| a.wrapping_add(c));
        let xor = plain[..64].iter().fold(0u8, |a, &c| a ^ c);
        [BSE, &[0, 0, sum, xor], &seed.to_le_bytes(), &data, &plain[64..]].concat()
    }

    #[test]
    fn an_older_packfile_archive_takes_its_translation_too() {
        let dir = game(&[]);
        fs::write(dir.join("data01000.arc"), packfile(&[("a", script())])).unwrap();
        let lines = Game::open(&dir).unwrap().lines();
        assert_eq!(lines.len(), 2);
        let key = lines[0].0.clone();
        assert_eq!(install(&dir, &|k| (k == key).then(|| "Hi".to_string()), &|_| None).unwrap(), 1);
        let a = Archive::open_as_written(&dir.join("data01000.arc")).unwrap();
        let s = Script::read(0, 0, "a".into(), a.file(&mut fs::File::open(dir.join("data01000.arc")).unwrap(), &a.items[0]).unwrap()).unwrap();
        assert_eq!(s.string(s.messages[0].text), "Hi");
    }

    #[test]
    fn a_file_in_a_bse_wrapper_is_read_and_written_back_bare() {
        let own = script();
        let wrapped = bse(&own, 0x1234_5678);
        assert_ne!(wrapped[16..80], own[..64]);
        assert_eq!(unwrap_bse(&wrapped).unwrap(), own);
        let dir = game(&[("a", wrapped)]);
        let key = Game::open(&dir).unwrap().lines()[0].0.clone();
        install(&dir, &|k| (k == key).then(|| "Hi".to_string()), &|_| None).unwrap();
        let a = Archive::open_as_written(&dir.join("data01000.arc")).unwrap();
        let back = a.file(&mut fs::File::open(dir.join("data01000.arc")).unwrap(), &a.items[0]).unwrap();
        assert!(back.starts_with(MAGIC), "stored bare");
    }

    #[test]
    fn a_script_without_a_header_is_found_by_its_messages_and_a_picture_is_not() {
        let with_header = script();
        let bare = with_header[MAGIC.len() + 4..].to_vec();
        let mut picture = [4u16, 1, 24].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        picture.extend([0; 10]);
        picture.extend([7; 12]);
        let dir = game(&[("old", bare), ("pic", picture)]);
        let lines = Game::open(&dir).unwrap().lines();
        assert_eq!(lines.iter().map(|l| l.2.as_str()).collect::<Vec<_>>(), ["こんにちは", "地の文です"]);
        let key = lines[1].0.clone();
        install(&dir, &|k| (k == key).then(|| "Narration".to_string()), &|_| None).unwrap();
        let a = Archive::open_as_written(&dir.join("data01000.arc")).unwrap();
        let s = Script::headerless(0, 0, "old".into(), a.file(&mut fs::File::open(dir.join("data01000.arc")).unwrap(), &a.items[0]).unwrap()).unwrap();
        assert_eq!(s.string(s.messages[1].text), "Narration");
    }

    #[test]
    fn choices_given_to_a_selection_call_are_lines() {
        let strings = ["右へ行く", "左へ行く", "sel01"];
        let (mut block, mut at) = (Vec::new(), Vec::new());
        for t in strings {
            at.push(block.len() as u32);
            block.extend(sjis(t));
            block.push(0);
        }
        let code: Vec<u32> = vec![3, at[0], 3, at[1], 3, at[2], 0, 2, 0x160, 0, 0, 0];
        let shift = (code.len() * 4) as u32;
        let code: Vec<u8> = code.iter().enumerate().flat_map(|(i, w)| (if [1, 3, 5].contains(&i) { w + shift } else { *w }).to_le_bytes()).collect();
        let s = Script::read(0, 0, "a".into(), [MAGIC, &4u32.to_le_bytes(), &code, &block].concat()).unwrap();
        let mut lines = Vec::new();
        s.lines(&mut lines);
        assert_eq!(lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect::<Vec<_>>(), [(None, strings[0]), (None, strings[1])]);
    }

    #[test]
    fn a_compressed_menu_picture_is_repainted_and_stays_compressed() {
        let own = crate::cbg::encode(2, 1, 32, &[1, 2, 3, 255, 4, 5, 6, 0]);
        let img = picture(&own).unwrap();
        assert_eq!(img.px, [0x00_03_02_01, 0xff_06_05_04], "BGRA, alpha turned to the app's way");
        let new = bitmap(&own, &Image { w: 2, h: 1, px: vec![0x00_10_20_30, 0x00_40_50_60] });
        assert!(new.starts_with(b"CompressedBG___"));
        assert_eq!(picture(&new).unwrap().px, [0x00_10_20_30, 0x00_40_50_60]);
    }

    fn game(files: &[(&str, Vec<u8>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-bgi-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data01000.arc"), archive(files)).unwrap();
        dir
    }

    /// A system program: pushes of the first string, a font name and a second string, an array whose
    /// bytes look like a push of the first string, the first string again, and the strings after the code.
    fn program() -> Vec<u8> {
        let strings = [sjis("はじめから"), sjis("ＭＳ ゴシック"), sjis("設定")];
        let mut at = vec![34usize];
        for s in &strings[..2] {
            at.push(at[at.len() - 1] + s.len() + 1);
        }
        let push = |q: usize, to: usize| [vec![PUSH_TEXT], ((to - q) as i16).to_le_bytes().to_vec()].concat();
        let mut body = [push(16, at[0]), push(19, at[1]), push(22, at[2]), vec![0x0b, 3], push(27, at[0]), push(30, at[0]), vec![0x17]].concat();
        strings.iter().for_each(|s| { body.extend(s); body.push(0); });
        [16u32.to_le_bytes().to_vec(), (body.len() as u32).to_le_bytes().to_vec(), vec![0; 8], body].concat()
    }

    #[test]
    fn a_system_program_gives_its_menu_text_and_takes_english_at_every_push() {
        let own = program();
        let p = Program::read(0, 0, "title._bp".into(), own.clone()).unwrap();
        let mut lines = Vec::new();
        p.lines(&mut lines);
        assert_eq!(lines.iter().map(|l| l.2.as_str()).collect::<Vec<_>>(), ["はじめから", "設定"], "the font name stays out");
        let english = |k: &str| match lines.iter().position(|l| l.0 == k) { Some(0) => Some("Start".to_string()), Some(1) => Some("Settings".to_string()), _ => None };
        let (out, done) = p.with(&english, &|_| None);
        assert_eq!(done, 2);
        let reached = |q: usize| SHIFT_JIS.decode(Program::raw(&out, q + i16::from_le_bytes([out[q + 1], out[q + 2]]) as usize)).0.into_owned();
        assert_eq!([reached(16), reached(30), reached(22)], ["Start", "Start", "Settings"], "both pushes of a string follow it");
        assert_eq!(reached(19), "ＭＳ ゴシック");
        assert_eq!(out[25..30], own[25..30], "what only looks like a push inside the array is left alone");
        assert_eq!(word(&out, 4).unwrap(), out.len() - 16, "the header has the new size");
    }

    #[test]
    fn a_program_with_an_instruction_not_known_is_left_alone() {
        let mut own = program();
        own[19] = 0x03;
        assert!(Program::read(0, 0, "title._bp".into(), own).unwrap().strings.is_empty());
    }

    #[test]
    fn a_repainted_picture_goes_into_sysgrp_with_every_write() {
        let mut own = [2u16, 1, 32].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
        own.extend([0; 10]);
        own.extend([1, 2, 3, 255, 4, 5, 6, 0]);
        let img = picture(&own).unwrap();
        assert_eq!(img.px, [0x00_03_02_01, 0xff_06_05_04], "BGRA, alpha inverted");
        assert_eq!(bitmap(&own, &img), own);

        let dir = game(&[("a", script())]);
        fs::write(dir.join(PICTURES), archive(&[("SGButton", own.clone())])).unwrap();
        let new = bitmap(&own, &Image { w: 2, h: 1, px: vec![0, 0] });
        crate::artemis::keep_painted(&dir, &[("SGButton".into(), new.clone())]).unwrap();
        install(&dir, &|_| None, &|_| None).unwrap();
        assert_eq!(interface_pictures(&dir).unwrap()[0].1, own, "the game's own stays readable");
        let a = Archive::open_as_written(&dir.join(PICTURES)).unwrap();
        assert_eq!(a.file(&mut fs::File::open(dir.join(PICTURES)).unwrap(), &a.items[0]).unwrap(), new);
    }

    #[test]
    fn furigana_leaves_only_its_word() {
        assert_eq!(without_ruby("<rかみ>神</r>様と<rあくま>悪魔</r>"), "神様と悪魔");
        assert_eq!(without_ruby("Plain <b>text</b>"), "Plain <b>text</b>");
    }

    #[test]
    fn messages_are_found_with_their_speakers_and_build_paths_are_not() {
        let s = Script::read(0, 0, "a".into(), script()).unwrap();
        let mut lines = Vec::new();
        s.lines(&mut lines);
        assert_eq!(lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect::<Vec<_>>(), [(Some("太郎"), "こんにちは"), (None, "地の文です")]);
    }

    #[test]
    fn a_sentence_in_the_name_slot_is_text_and_not_a_speaker() {
        let strings = ["邪魔になるから、帰りますね。", "同じ文です"];
        let (mut block, mut at) = (Vec::new(), Vec::new());
        for t in strings {
            at.push(block.len() as u32);
            block.extend(sjis(t));
            block.push(0);
        }
        let code: Vec<u32> = vec![3, at[0], 3, at[1], MESSAGE, 0, 0];
        let shift = (code.len() * 4) as u32;
        let code: Vec<u8> = code.iter().enumerate().flat_map(|(i, w)| (if [1, 3].contains(&i) { w + shift } else { *w }).to_le_bytes()).collect();
        let s = Script::read(0, 0, "a".into(), [MAGIC, &4u32.to_le_bytes(), &code, &block].concat()).unwrap();
        let mut lines = Vec::new();
        s.lines(&mut lines);
        assert_eq!(lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect::<Vec<_>>(), [(None, strings[0]), (None, strings[1])]);
    }

    #[test]
    fn translations_go_on_the_end_and_read_back() {
        let dir = game(&[("a", script()), ("image", b"CompressedBG___untouched".to_vec())]);
        let lines = Game::open(&dir).unwrap().lines();
        let key = lines[0].0.clone();
        let line = |k: &str| (k == key).then(|| "Hello there".to_string());
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 2);

        let path = dir.join("data01000.arc");
        let a = Archive::open_as_written(&path).unwrap();
        let mut f = fs::File::open(&path).unwrap();
        let s = Script::read(0, 0, "a".into(), a.file(&mut f, &a.items[0]).unwrap()).unwrap();
        assert_eq!(s.string(s.messages[0].text), "Hello there");
        assert_eq!(s.string(s.messages[0].name.unwrap()), "Taro");
        assert_eq!(s.string(s.messages[1].text), "地の文です");
        assert_eq!(a.file(&mut f, &a.items[1]).unwrap(), b"CompressedBG___untouched");

        drop(f);
        let once = fs::read(&path).unwrap();
        install(&dir, &line, &name).unwrap(); // a second run starts from the original, not from itself
        assert_eq!(fs::read(&path).unwrap(), once);
    }
}
