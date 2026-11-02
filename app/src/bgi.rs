//! BGI/Ethornell engine (data*.arc, BURIKO ARC20): scripts packed with a keyed Huffman and LZ scheme.
//! Compiled code refers to strings by offset into a block at the end of the script, so new text can
//! go on the end and only those offsets change.

use crate::pipeline::Line;
use crate::text::{ascii_punctuation, looks_like_name, sjis, wrap_words, JA};
use crate::{backup, extend_file, R};
use encoding_rs::SHIFT_JIS;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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

// ---------- archives ----------

const ROW: usize = 0x80;
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
        if &top[..12] != b"BURIKO ARC20" {
            return Ok(None);
        }
        let count = word(&top, 12)?;
        let mut head = top.to_vec();
        head.resize(TOP + ROW * count, 0);
        f.read_exact(&mut head[TOP..]).map_err(err)?;
        let base = TOP + ROW * count;
        let mut items = Vec::with_capacity(count);
        for i in 0..count {
            let row = TOP + ROW * i;
            let raw = &head[row..row + 0x60];
            let name = SHIFT_JIS.decode_without_bom_handling(&raw[..raw.iter().position(|&c| c == 0).unwrap_or(0x60)]).0.into_owned();
            items.push(Item { name, row, place: base + word(&head, row + 0x60)?, size: word(&head, row + 0x64)? });
        }
        Ok(Some(Archive { path: path.to_path_buf(), head, len, items }))
    }

    fn file(&self, f: &mut fs::File, item: &Item) -> R<Vec<u8>> {
        let mut b = vec![0; item.size];
        f.seek(SeekFrom::Start(item.place as u64)).and_then(|_| f.read_exact(&mut b)).map_err(|e| format!("{}: {e}", item.name))?;
        if b.starts_with(b"DSC FORMAT 1.00") { unpack(&b) } else { Ok(b) }
    }

    /// New files in place of the old, stored unpacked on the end, with the index rewritten last.
    fn replace(&self, new: &[(usize, Vec<u8>)]) -> R<()> {
        let mut head = self.head.clone();
        let (mut tail, mut place) = (Vec::new(), self.len as usize - self.head.len());
        for (i, bytes) in new {
            let row = self.items[*i].row;
            head[row + 0x60..row + 0x64].copy_from_slice(&(place as u32).to_le_bytes());
            head[row + 0x64..row + 0x68].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
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
                }
                i = p + 8;
            } else {
                i += 4;
            }
        }
        Ok(Script { archive, item, name, plain, base, messages })
    }

    fn string(&self, push: usize) -> String {
        string_at(&self.plain, self.base, push)
    }

    fn lines(&self, out: &mut Vec<Line>) {
        for m in &self.messages {
            let text = self.string(m.text);
            if JA.is_match(&text) {
                let who = m.name.map(|n| self.string(n)).filter(|n| JA.is_match(n));
                out.push((format!("{}:{}", self.name, m.text), who, text.replace('\n', "")));
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
            put(&mut out, m.text, &wrap_words(&ascii_punctuation(&tr), width).join("\n"));
            done += 1;
            if let Some((n, en)) = m.name.and_then(|n| Some((n, name(&self.string(n))?))) {
                put(&mut out, n, &en);
                done += 1;
            }
        }
        (out, done)
    }
}

// ---------- the game ----------

pub struct Game {
    archives: Vec<Archive>,
    scripts: Vec<Script>,
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let (mut archives, mut scripts) = (Vec::new(), Vec::new());
        let mut paths: Vec<PathBuf> = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("arc"))).collect();
        paths.sort();
        for path in paths {
            let Some(a) = Archive::open(&path)? else { continue };
            let mut f = fs::File::open(if backup(&path).exists() { backup(&path) } else { path.clone() }).map_err(|e| e.to_string())?;
            for (i, item) in a.items.iter().enumerate() {
                let mut top = [0; 16];
                f.seek(SeekFrom::Start(item.place as u64)).and_then(|_| f.read_exact(&mut top)).map_err(|e| e.to_string())?;
                if top.starts_with(b"DSC FORMAT 1.00") || top.starts_with(&MAGIC[..16]) {
                    let plain = a.file(&mut f, item).map_err(|e| format!("{}: {e}", item.name))?;
                    if plain.starts_with(MAGIC) {
                        scripts.push(Script::read(archives.len(), i, item.name.clone(), plain)?);
                    }
                }
            }
            archives.push(a);
        }
        Ok(Game { archives, scripts })
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|s| s.lines(&mut out));
        out
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the archives and reads
/// them back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let mut done = 0;
    for (n, a) in g.archives.iter().enumerate() {
        let mut new = Vec::new();
        for s in g.scripts.iter().filter(|s| s.archive == n) {
            let (bytes, count) = s.with(crate::fit::letters(game, LINE_LETTERS), line, name);
            if count > 0 {
                new.push((s.item, bytes));
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
        let mut out = b"BURIKO ARC20".to_vec();
        out.extend((files.len() as u32).to_le_bytes());
        let mut place = 0u32;
        for (name, b) in files {
            let mut row = [0u8; ROW];
            row[..name.len()].copy_from_slice(name.as_bytes());
            row[0x60..0x64].copy_from_slice(&place.to_le_bytes());
            row[0x64..0x68].copy_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend(row);
            place += b.len() as u32;
        }
        files.iter().for_each(|(_, b)| out.extend(b));
        out
    }

    fn game(files: &[(&str, Vec<u8>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-bgi-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data01000.arc"), archive(files)).unwrap();
        dir
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
