//! YU-RIS engine (.ypf archives holding ysbin/*.ybn): compiled scripts whose arguments sit in a table
//! pointing into a block of data, XORed with a key of four bytes each game picks. Text can go on the
//! end of that block, with only the table changed.

use crate::text::decoded;
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, JA};
use crate::{backup, extend_file, R};
use encoding_rs::SHIFT_JIS;
use flate2::read::{DeflateDecoder, ZlibDecoder};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

fn word(b: &[u8], p: usize) -> R<usize> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated YU-RIS file")?.try_into().unwrap()) as usize)
}

fn short(b: &[u8], p: usize) -> R<usize> {
    Ok(u16::from_le_bytes(b.get(p..p + 2).ok_or("truncated YU-RIS file")?.try_into().unwrap()) as usize)
}

// ---------- archives ----------

const HEAD: usize = 0x20;

/// Name lengths are stored swapped in pairs, by a table that depends on the version.
fn swapped(version: usize, n: u8) -> u8 {
    const ALL: [u8; 24] = [
        0x03, 0x48, 0x06, 0x35, 0x0c, 0x10, 0x11, 0x19, 0x1c, 0x1e, 0x09, 0x0b, 0x0d, 0x13, 0x15, 0x1b, 0x20, 0x23, 0x26, 0x29, 0x2c, 0x2f, 0x2e, 0x32,
    ];
    let table = if version < 0x100 {
        &ALL[4..]
    } else if (0x12c..0x196).contains(&version) {
        &ALL[10..]
    } else {
        &ALL[..]
    };
    match table.iter().position(|&c| c == n) {
        Some(i) => table[i ^ 1],
        None => n,
    }
}

fn extra(version: usize) -> usize {
    if version >= 0x1d9 {
        4
    } else if version == 0xde {
        8
    } else {
        0
    }
}

/// One file of an archive; `meta` is where its numbers are in the index.
pub struct Item {
    pub path: String,
    meta: usize,
    packed: bool,
    size: usize,
    stored: usize,
    place: usize,
}

pub struct Archive {
    path: PathBuf,
    head: Vec<u8>,
    len: u64,
    pub items: Vec<Item>,
}

pub(crate) fn adler32(b: &[u8]) -> u32 {
    let (mut x, mut y) = (1u32, 0u32);
    for &c in b {
        x = (x + c as u32) % 65521;
        y = (y + x) % 65521;
    }
    y << 16 | x
}

fn murmur2(b: &[u8]) -> u32 {
    const M: u32 = 0x5bd1_e995;
    let mut h = b.len() as u32;
    let (blocks, tail) = b.as_chunks::<4>();
    for q in blocks {
        let mut k = u32::from_le_bytes(*q).wrapping_mul(M);
        k ^= k >> 24;
        h = h.wrapping_mul(M) ^ k.wrapping_mul(M);
    }
    if !tail.is_empty() {
        for (i, c) in tail.iter().enumerate() {
            h ^= (*c as u32) << (8 * i);
        }
        h = h.wrapping_mul(M);
    }
    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^ h >> 15
}

impl Archive {
    /// Always the game's own archive: the backup, once there is one.
    fn open(path: &Path) -> R<Archive> {
        let own = if backup(path).exists() { backup(path) } else { path.to_path_buf() };
        let mut f = fs::File::open(&own).map_err(|e| format!("{}: {e}", path.display()))?;
        read_index(&mut f, path)
    }
}

/// The header and index of the archive open in `f`, which goes by the name `path`.
fn read_index(f: &mut fs::File, path: &Path) -> R<Archive> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let len = f.metadata().map_err(err)?.len();
    let mut top = [0; HEAD];
    f.read_exact(&mut top).map_err(err)?;
    if &top[..4] != b"YPF\0" {
        return Err("not a YU-RIS archive".into());
    }
    let (version, count, size) = (word(&top, 4)?, word(&top, 8)?, word(&top, 12)?);
    let mut head = top.to_vec();
    head.resize(HEAD + size, 0);
    f.read_exact(&mut head[HEAD..]).map_err(err)?;
    let meta_size = 0x12 + extra(version);
    let (mut items, mut p, mut key) = (Vec::with_capacity(count), HEAD, None);
    for _ in 0..count {
        let n = swapped(version, !*head.get(p + 4).ok_or("bad YU-RIS index")?) as usize;
        let start = p + 5;
        let raw = head.get(start..start + n).ok_or("bad YU-RIS index")?;
        // names end in an extension, which gives away the key they are XORed with
        let k = *key.get_or_insert(if n >= 4 { raw[n - 4] ^ b'.' } else { 0xff });
        let name: Vec<u8> = raw.iter().map(|c| c ^ k).collect();
        let meta = start + n;
        items.push(Item {
            path: SHIFT_JIS.decode_without_bom_handling(&name).0.into_owned(),
            meta,
            packed: *head.get(meta + 1).ok_or("bad YU-RIS index")? != 0,
            size: word(&head, meta + 2)?,
            stored: word(&head, meta + 6)?,
            place: word(&head, meta + 10)?,
        });
        p = meta + meta_size;
    }
    Ok(Archive { path: path.to_path_buf(), head, len, items })
}

impl Archive {
    pub fn file(&self, f: &mut fs::File, item: &Item) -> R<Vec<u8>> {
        let mut b = vec![0; item.stored];
        f.seek(SeekFrom::Start(item.place as u64)).and_then(|_| f.read_exact(&mut b)).map_err(|e| format!("{}: {e}", item.path))?;
        if !item.packed {
            return Ok(b);
        }
        let mut out = Vec::with_capacity(item.size);
        if ZlibDecoder::new(&b[..]).read_to_end(&mut out).is_err() {
            out.clear();
            DeflateDecoder::new(&b[..]).read_to_end(&mut out).map_err(|e| format!("{}: {e}", item.path))?;
        }
        Ok(out)
    }

    fn version(&self) -> usize {
        word(&self.head, 4).unwrap_or(0)
    }

    /// New files in place of the old: (index in `items`, contents). They go on the end, stored as they
    /// are, and the index is rewritten last, so a run cut short leaves a game that still plays.
    fn replace(&self, new: &[(usize, Vec<u8>)]) -> R<()> {
        let mut head = self.head.clone();
        let (mut tail, mut place) = (Vec::new(), self.len);
        for (i, bytes) in new {
            let meta = self.items[*i].meta;
            let sum = if self.version() < 479 { adler32(bytes) } else { murmur2(bytes) };
            head[meta + 1] = 0;
            for (at, v) in [(2, bytes.len() as u32), (6, bytes.len() as u32), (10, place as u32), (14, sum)] {
                head[meta + at..meta + at + 4].copy_from_slice(&v.to_le_bytes());
            }
            tail.extend(bytes);
            place += bytes.len() as u64;
        }
        if place > u32::MAX as u64 {
            return Err("this YU-RIS archive would grow past 4 GB".into());
        }
        extend_file(&self.path, self.len, &self.head, &head, &tail)
    }
}

/// The game's archives.
fn archives(game: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in [game.to_path_buf(), game.join("pac")] {
        out.extend(fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ypf"))));
    }
    out.sort();
    out
}

/// The key of a script, from its table: an entry for text starts with four zero bytes, so what's
/// there as stored is the key itself, and entries agree on it.
fn script_key(b: &[u8]) -> u32 {
    let (Ok(code), Ok(args), Ok(data)) = (word(b, 0x0c), word(b, 0x10), word(b, 0x14)) else { return 0 };
    let start = HEAD + code;
    let mut votes = std::collections::HashMap::<u32, usize>::new();
    for at in (start..start + args).step_by(12) {
        let (Ok(k), Ok(size), Ok(offset)) = (word(b, at), word(b, at + 4), word(b, at + 8)) else { break };
        let (size, offset) = (size ^ k, offset ^ k);
        if size > 0 && size <= 4096 && offset <= data && size <= data - offset {
            *votes.entry(k as u32).or_default() += 1;
        }
    }
    votes.into_iter().max_by_key(|v| v.1).map_or(0, |v| v.0)
}

/// Scripts XOR each section on its own, restarting the key.
fn crypt(b: &mut [u8], key: u32) {
    let k = key.to_le_bytes();
    let sizes = [0x0c, 0x10, 0x14, 0x18].map(|at| word(b, at).unwrap_or(0));
    let mut p = HEAD;
    for size in sizes {
        for (i, c) in b[p..p + size].iter_mut().enumerate() {
            *c ^= k[i % 4];
        }
        p += size;
    }
}

// ---------- scripts ----------

const ENTRY: usize = 12;
/// Ruby marks inside text.
const RUBY: &[u8] = b"\x87\x55";
/// The argument that announces choices: the name of the command, as a string.
const SELECT: &[u8] = b"\x4d\x0c\x00\x22ES.SEL.SET\x22";

/// A name in front of a quote, as these scripts write speech.
static SPOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^([^「]{1,12})「(.*)」$").unwrap());

/// A text in a script: where its table entry is, and whether it is a choice.
#[derive(Debug, PartialEq)]
struct Text {
    entry: usize,
    choice: bool,
    text: String,
}

/// A script, decrypted: the header, then the code, the table, the data and the line numbers.
struct Script {
    item: usize,
    key: u32,
    name: String,
    plain: Vec<u8>,
    texts: Vec<Text>,
}

fn without_ruby(b: &[u8]) -> Vec<u8> {
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        if b[i..].starts_with(RUBY) {
            i += 2;
        } else {
            let n = if matches!(b[i], 0x81..=0x9f | 0xe0..=0xef) { 2 } else { 1 };
            out.extend(&b[i..(i + n).min(b.len())]);
            i += n;
        }
    }
    out
}

impl Script {
    fn read(item: usize, name: String, bytes: &[u8]) -> R<Script> {
        let key = script_key(bytes);
        let mut plain = bytes.to_vec();
        crypt(&mut plain, key);
        let (code, args, data) = (word(&plain, 0x0c)?, word(&plain, 0x10)?, word(&plain, 0x14)?);
        let (table, block) = (HEAD + code, HEAD + code + args);
        let (mut texts, mut options) = (Vec::new(), false);
        for entry in (table..table + args).step_by(ENTRY) {
            let (id, kind) = (short(&plain, entry)?, short(&plain, entry + 2)?);
            let (size, offset) = (word(&plain, entry + 4)?, word(&plain, entry + 8)?);
            if size == 0 || size > 4096 || offset + size > data {
                options = false;
                continue;
            }
            let d = &plain[block + offset..block + offset + size];
            if options {
                // `M`, a length, then the text in quotes
                options = d.len() > 5 && d[0] == 0x4d;
                if options {
                    let text = decoded(&d[4..d.len() - 1]);
                    if JA.is_match(&text) {
                        texts.push(Text { entry, choice: true, text });
                    }
                }
            } else if kind == 3 && d == SELECT {
                options = true;
            } else if id == 0 && kind == 0 && d[0] != 0x4d && !d.starts_with(b"H\x03") && !d.contains(&0) && !d.starts_with(b"cg") {
                let text = decoded(&without_ruby(d));
                if JA.is_match(&text) {
                    texts.push(Text { entry, choice: false, text });
                }
            }
        }
        Ok(Script { item, key, name, plain, texts })
    }

    fn lines(&self, out: &mut Vec<Line>) {
        for t in &self.texts {
            let key = format!("{}:{}", self.name, t.entry);
            match SPOKEN.captures(&t.text).filter(|_| !t.choice) {
                Some(c) => out.push((key, Some(c[1].to_string()), c[2].to_string())),
                None => out.push((key, None, t.text.clone())),
            }
        }
    }

    /// The script with new text on the end of the data block, and the table pointed at it.
    fn with(&self, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<(Vec<u8>, usize)> {
        let (code, args, data) = (word(&self.plain, 0x0c)?, word(&self.plain, 0x10)?, word(&self.plain, 0x14)?);
        let block = HEAD + code + args;
        let mut out = self.plain.clone();
        let (mut extra, mut done) = (Vec::new(), 0);
        for t in &self.texts {
            let Some(tr) = line(&format!("{}:{}", self.name, t.entry)) else { continue };
            let body = ascii_punctuation(&tr);
            let spoken = SPOKEN.captures(&t.text).filter(|_| !t.choice).map(|c| name(&c[1]).unwrap_or(c[1].to_string()));
            let bytes = if t.choice {
                let inner = sjis(&body);
                let mut b = vec![0x4d];
                b.extend((inner.len() as u16 + 2).to_le_bytes());
                b.push(b'"');
                b.extend(inner);
                b.push(b'"');
                b
            } else {
                sjis(&spoken.as_ref().map_or(body.clone(), |who| format!("{who}「{}」", body.trim_matches('"'))))
            };
            out[t.entry + 4..t.entry + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            out[t.entry + 8..t.entry + 12].copy_from_slice(&((data + extra.len()) as u32).to_le_bytes());
            extra.extend(bytes);
            extra.push(0);
            done += 1 + spoken.is_some() as usize;
        }
        if done == 0 {
            return Ok((Vec::new(), 0));
        }
        out.splice(block + data..block + data, extra.iter().copied());
        out[0x14..0x18].copy_from_slice(&((data + extra.len()) as u32).to_le_bytes());
        crypt(&mut out, self.key);
        Ok((out, done))
    }
}

// ---------- the game ----------

pub struct Game {
    archive: Archive,
    scripts: Vec<Script>,
}

impl Game {
    /// The archive that holds the scripts.
    pub fn open(game: &Path) -> R<Game> {
        for path in archives(game) {
            let archive = Archive::open(&path)?;
            if !archive.items.iter().any(|i| i.path.to_lowercase().ends_with(".ybn")) {
                continue;
            }
            let mut f = fs::File::open(if backup(&path).exists() { backup(&path) } else { path.clone() }).map_err(|e| e.to_string())?;
            let mut scripts = Vec::new();
            for (i, item) in archive.items.iter().enumerate().filter(|(_, i)| i.path.to_lowercase().ends_with(".ybn")) {
                let b = archive.file(&mut f, item)?;
                if b.starts_with(b"YSTB") {
                    scripts.push(Script::read(i, item.path.clone(), &b).map_err(|e| format!("{}: {e}", item.path))?);
                }
            }
            return Ok(Game { archive, scripts });
        }
        Err("this YU-RIS game has no scripts in its archives".into())
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|s| s.lines(&mut out));
        out
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the archive and reads
/// it back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let (mut new, mut done) = (Vec::new(), 0);
    for s in &g.scripts {
        let (bytes, n) = s.with(line, name)?;
        if n > 0 {
            Script::read(s.item, s.name.clone(), &bytes)?;
            new.push((s.item, bytes));
            done += n;
        }
    }
    g.archive.replace(&new)?;
    let wanted: HashMap<usize, &Vec<u8>> = new.iter().map(|(i, b)| (*i, b)).collect();
    let mut head = fs::File::open(&g.archive.path).map_err(|e| e.to_string())?;
    let back = read_index(&mut head, &g.archive.path)?;
    for (i, bytes) in wanted {
        if back.file(&mut head, &back.items[i])? != *bytes {
            return Err(format!("{} didn't read back as written", back.items[i].path));
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: u32 = 0xd3_6f_a1_5c;

    #[test]
    fn the_checksums_give_their_published_values() {
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
        assert_eq!(murmur2(b""), 0);
    }

    /// A script: entries of (id, kind, data).
    fn script(args: &[(u16, u16, &[u8])]) -> Vec<u8> {
        let (mut table, mut data) = (Vec::new(), Vec::new());
        for (id, kind, d) in args {
            table.extend(id.to_le_bytes());
            table.extend(kind.to_le_bytes());
            table.extend((d.len() as u32).to_le_bytes());
            table.extend((data.len() as u32).to_le_bytes());
            data.extend(*d);
            data.push(0);
        }
        let mut b = b"YSTB".to_vec();
        b.extend(0x1c3u32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        for v in [4usize, table.len(), data.len(), 4] {
            b.extend((v as u32).to_le_bytes());
        }
        b.resize(HEAD, 0);
        b.extend([1, 2, 3, 4]);
        b.extend(table);
        b.extend(data);
        b.extend([5, 6, 7, 8]);
        crypt(&mut b, KEY);
        b
    }

    /// An archive of (name, contents) at version 0x1c3.
    fn archive(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        const NAME_KEY: u8 = 0x40;
        let mut index = Vec::new();
        let mut metas = Vec::new();
        for (name, b) in files {
            index.extend(0u32.to_le_bytes());
            index.push(!swapped(0x1c3, name.len() as u8));
            index.extend(name.bytes().map(|c| c ^ NAME_KEY));
            metas.push(index.len());
            index.extend([0, 0]);
            index.extend((b.len() as u32).to_le_bytes());
            index.extend((b.len() as u32).to_le_bytes());
            index.extend(0u32.to_le_bytes());
            index.extend(adler32(b).to_le_bytes());
        }
        let mut place = HEAD + index.len();
        for (m, (_, b)) in metas.iter().zip(files) {
            index[m + 10..m + 14].copy_from_slice(&(place as u32).to_le_bytes());
            place += b.len();
        }
        let mut out = b"YPF\0".to_vec();
        for v in [0x1c3usize, files.len(), index.len()] {
            out.extend((v as u32).to_le_bytes());
        }
        out.resize(HEAD, 0);
        out.extend(index);
        files.iter().for_each(|(_, b)| out.extend(b));
        out
    }

    fn game(files: &[(&str, Vec<u8>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-yuris-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("pac")).unwrap();
        fs::write(dir.join("pac/ysbin.ypf"), archive(files)).unwrap();
        dir
    }

    fn sample() -> Vec<u8> {
        let spoken = sjis("太郎「こんにちは」");
        let plain = sjis("地の文です");
        let option = [&b"\x4d\x08\x00\x22"[..], &sjis("はい"), b"\x22"].concat();
        script(&[(0, 0, &spoken), (0, 0, &plain), (1, 2, b"cg01"), (0, 3, SELECT), (0, 0, &option)])
    }

    #[test]
    fn the_key_comes_from_the_table() {
        let b = sample();
        assert_eq!(script_key(&b), KEY);
    }

    #[test]
    fn lines_have_speakers_and_translations_go_on_the_end() {
        let dir = game(&[("ysbin/yst00000.ybn", sample()), ("other.dat", b"untouched".to_vec())]);
        let lines = Game::open(&dir).unwrap().lines();
        assert_eq!(lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect::<Vec<_>>(), [(Some("太郎"), "こんにちは"), (None, "地の文です"), (None, "はい")]);

        let keys: Vec<String> = lines.iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.iter().position(|x| x == k).map(|i| ["\"Hello.\"", "Narration.", "Yes"][i].to_string());
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 4);

        let path = dir.join("pac/ysbin.ypf");
        let mut f = fs::File::open(&path).unwrap();
        let back = read_index(&mut f, &path).unwrap();
        assert_eq!(back.file(&mut f, &back.items[1]).unwrap(), b"untouched");
        let s = Script::read(0, "a".into(), &back.file(&mut f, &back.items[0]).unwrap()).unwrap();
        let block = String::from_utf8_lossy(&s.plain).into_owned();
        assert!(block.contains("Narration.") && block.contains("\"Yes\""));

        // a second run starts from the original, not from itself
        drop(f);
        let once = fs::read(&path).unwrap();
        install(&dir, &line, &name).unwrap();
        assert_eq!(fs::read(&path).unwrap(), once);
    }
}
