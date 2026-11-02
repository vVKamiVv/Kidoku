//! QLIE engine (GameData/data*.pack, FilePackVer3.1): files encrypted one by one with a key made from
//! the pack's own tail and each file's name, and sometimes packed.

use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, text_units, wrap_words, JA};
use crate::{backup, original, replace_file, R};
use encoding_rs::SHIFT_JIS;
use regex::Regex;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

fn u16_at(b: &[u8], p: usize) -> R<u16> {
    Ok(u16::from_le_bytes(b.get(p..p + 2).ok_or("truncated QLIE pack")?.try_into().unwrap()))
}

fn u32_at(b: &[u8], p: usize) -> R<u32> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated QLIE pack")?.try_into().unwrap()))
}

fn u64_at(b: &[u8], p: usize) -> R<u64> {
    Ok(u64::from_le_bytes(b.get(p..p + 8).ok_or("truncated QLIE pack")?.try_into().unwrap()))
}

// The engine's MMX arithmetic: additions and shifts in separate lanes of a 64 bit word.

fn add_d(a: u64, b: u64) -> u64 {
    let low = (a as u32).wrapping_add(b as u32) as u64;
    let high = ((a >> 32) as u32).wrapping_add((b >> 32) as u32) as u64;
    low | high << 32
}

fn add_w(a: u64, b: u64) -> u64 {
    (0..4).fold(0, |out, i| out | (((a >> (16 * i)) as u16).wrapping_add((b >> (16 * i)) as u16) as u64) << (16 * i))
}

fn add_b(a: u64, b: u64) -> u64 {
    (0..8).fold(0, |out, i| out | (((a >> (8 * i)) as u8).wrapping_add((b >> (8 * i)) as u8) as u64) << (8 * i))
}

/// Each lane shifted left, with what it loses gone.
fn left_d(a: u64, n: u32) -> u64 {
    (((a as u32) << n) as u64) | ((((a >> 32) as u32) << n) as u64) << 32
}

fn right_d(a: u64, n: u32) -> u64 {
    (((a as u32) >> n) as u64) | ((((a >> 32) as u32) >> n) as u64) << 32
}

/// The hash of the 256 bytes in a pack's tail, which every other key starts from.
fn hash(data: &[u8]) -> u32 {
    let (mut h, mut key) = (0u64, 0u64);
    for q in data.as_chunks::<8>().0 {
        h = add_w(h, 0xa357_93a7_a357_93a7);
        key = add_w(key, u64::from_le_bytes(*q) ^ h);
        key = left_d(key, 3) | right_d(key, 29);
    }
    let word = |n: u32| (key >> n) as u16 as i16 as i32;
    (word(0).wrapping_mul(word(32))).wrapping_add(word(16).wrapping_mul(word(48))) as u32
}

fn table(seed: u32, key: u32) -> [u32; 32] {
    let (mut t, mut seed) = ([0; 32], seed);
    for v in &mut t {
        let x = key as u64 * (seed ^ key) as u64;
        seed = ((x >> 32) + x) as u32;
        *v = seed;
    }
    t
}

fn key_data(file: Option<&[u8]>) -> Vec<u8> {
    let mut d = vec![0; 0x400];
    for i in 0..0x100i32 {
        let v = if i % 3 != 0 { (i + 7).wrapping_mul(-(i + 3)) } else { (i + 7).wrapping_mul(i + 3) };
        d[4 * i as usize..4 * i as usize + 4].copy_from_slice(&v.to_le_bytes());
    }
    if let Some(f) = file.filter(|f| f.len() >= 128) {
        let (mut k, l) = (f[49] as usize % 73 + 128, f[79] as usize % 7 + 7);
        for c in &mut d {
            k = (k + l) % f.len();
            *c ^= f[k];
        }
    }
    d
}

/// One file in a pack.
pub struct Item {
    pub name: String,
    place: u64,
    size: usize,
    packed: bool,
    method: u32,
    entry: usize,
}

/// A pack's index: the data stays in the file, as packs run to gigabytes.
pub struct Pack {
    arc_key: u32,
    index_at: u64,
    pub index: Vec<u8>,
    pub items: Vec<Item>,
    keyfile: Option<Vec<u8>>,
}

/// What a file's name is XORed with, for a name of `n` characters.
fn name_stream(n: usize, arc_key: u32) -> Vec<u16> {
    let (count, word) = (n as i32, arc_key as i32);
    let start = ((count.wrapping_mul(count) ^ count) ^ 0x3e13 ^ (word >> 16) ^ word) & 0xffff;
    let mut key = start;
    (0..count)
        .map(|i| {
            key = start.wrapping_add(i).wrapping_add(key.wrapping_mul(8));
            key as u16
        })
        .collect()
}

fn name_of(raw: &[u8], arc_key: u32) -> String {
    let units = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c));
    String::from_utf16_lossy(&units.zip(name_stream(raw.len() / 2, arc_key)).map(|(u, k)| u ^ k).collect::<Vec<_>>())
}

fn crypt(data: &mut [u8], item: &Item, arc_key: u32, keyfile: Option<&[u8]>, encrypting: bool) {
    if item.method == 0 || data.len() < 8 {
        return;
    }
    let v2 = item.method != 1;
    let (mut h, mut seed): (u32, u32) = if v2 { (0x86f7e2, 0x4437f1) } else { (0x85f532, 0x33f641) };
    for (i, c) in item.name.encode_utf16().enumerate() {
        h = h.wrapping_add((c as u32) << (i & 7));
        seed ^= h;
    }
    let len = data.len() as u32;
    let (mul, xor) = if v2 { (13u32, 0x56e213u32) } else { (7, 0x8f32dc) };
    let mix = mul.wrapping_mul(len & 0xffffff).wrapping_add(len).wrapping_add(h).wrapping_add(h ^ len ^ xor);
    seed = seed.wrapping_add(arc_key ^ mix);
    seed = (if v2 { 13u32 } else { 9 }).wrapping_mul(seed & 0xffffff);
    let t = table(seed, if v2 { 0x8a77f473 } else { 0x8df21431 });
    let extra = key_data(keyfile);
    let pair = |i: usize| t[i] as u64 | (t[i + 1] as u64) << 32;
    let mut step = if v2 { (8 * (t[8] & 0xd) as usize) & 0x7f } else { 2 * (t[0xd] & 0xf) as usize };
    let mut chain = pair(6);
    for q in data.as_chunks_mut::<8>().0 {
        let tv = if v2 {
            pair(2 * (step & 0xf)) ^ u64::from_le_bytes(extra[8 * step..8 * step + 8].try_into().unwrap())
        } else {
            pair(step)
        };
        chain = add_d(chain ^ tv, tv);
        let word = u64::from_le_bytes(*q);
        let plain = if encrypting { word } else { word ^ chain };
        *q = (word ^ chain).to_le_bytes();
        chain = add_b(chain, plain) ^ plain;
        chain = add_w(left_d(chain, 1), plain);
        step = if v2 { (step + 1) & 0x7f } else { (step + 2) & 0x1f };
    }
}

/// The engine's byte-pair compression.
fn decompress(b: &[u8]) -> R<Vec<u8>> {
    const BAD: &str = "bad packed QLIE data";
    if u32_at(b, 0)? != 0xff43_5031 {
        return Err(BAD.into());
    }
    let wide = b[4] & 1 == 0;
    let size = u32_at(b, 8)? as usize;
    let (mut out, mut p) = (Vec::with_capacity(size), 12);
    let next = |p: &mut usize| {
        *p += 1;
        b.get(*p - 1).copied().ok_or(BAD)
    };
    while p < b.len() {
        let mut node = [[0u8; 256]; 2];
        for (i, n) in node[0].iter_mut().enumerate() {
            *n = i as u8;
        }
        let mut i = 0usize;
        while i < 256 {
            let mut count = next(&mut p)? as usize;
            if count > 127 {
                i += count - 127;
                count = 0;
            }
            if i > 255 {
                break;
            }
            for _ in 0..count + 1 {
                node[0][i] = next(&mut p)?;
                if node[0][i] as usize != i {
                    node[1][i] = next(&mut p)?;
                }
                i += 1;
            }
        }
        let mut count = if wide { u32_at(b, p)? as usize } else { u16_at(b, p)? as usize };
        p += if wide { 4 } else { 2 };
        let mut stack = Vec::with_capacity(256);
        loop {
            let index = match stack.pop() {
                Some(v) => v,
                None if count == 0 => break,
                None => {
                    count -= 1;
                    next(&mut p)?
                }
            };
            if node[0][index as usize] == index {
                out.push(index);
            } else {
                stack.push(node[1][index as usize]);
                stack.push(node[0][index as usize]);
            }
        }
    }
    if out.len() != size { Err(BAD.into()) } else { Ok(out) }
}

impl Pack {
    pub fn open(f: &mut fs::File) -> R<Pack> {
        let err = |e: std::io::Error| format!("QLIE pack: {e}");
        let len = f.metadata().map_err(err)?.len();
        let mut tail = vec![0; 0x41c];
        f.seek(SeekFrom::Start(len.checked_sub(0x41c).ok_or("QLIE pack too short")?)).map_err(err)?;
        f.read_exact(&mut tail).map_err(err)?;
        let foot = &tail[0x41c - 0x1c..];
        if &foot[..14] != b"FilePackVer3.1" {
            return Err("this QLIE pack version isn't supported yet".into());
        }
        let (count, index_at) = (u32_at(foot, 0x10)? as usize, u64_at(foot, 0x14)?);
        let arc_key = hash(&tail[..0x100]) & 0x0fff_ffff;
        f.seek(SeekFrom::Start(index_at)).map_err(err)?;
        let mut index = vec![0; (len - index_at) as usize];
        f.read_exact(&mut index).map_err(err)?;
        let (mut items, mut p, mut keyfile) = (Vec::with_capacity(count), 0, None);
        for _ in 0..count {
            let n = u16_at(&index, p)? as usize * 2;
            let name = name_of(index.get(p + 2..p + 2 + n).ok_or("bad QLIE index")?, arc_key);
            let entry = p + 2 + n;
            let item = Item {
                name,
                place: u64_at(&index, entry)?,
                size: u32_at(&index, entry + 8)? as usize,
                packed: u32_at(&index, entry + 16)? != 0,
                method: u32_at(&index, entry + 20)?,
                entry,
            };
            p = entry + 28;
            // the key file comes first in the index, and the files after it use it
            if keyfile.is_none() && item.name.contains("pack_keyfile") {
                keyfile = Some(Self::read_with(f, &item, arc_key, None)?);
            }
            items.push(item);
        }
        Ok(Pack { arc_key, index_at, index, items, keyfile })
    }

    fn read_with(f: &mut fs::File, item: &Item, arc_key: u32, keyfile: Option<&[u8]>) -> R<Vec<u8>> {
        let mut b = vec![0; item.size];
        f.seek(SeekFrom::Start(item.place)).and_then(|_| f.read_exact(&mut b)).map_err(|e| format!("{}: {e}", item.name))?;
        crypt(&mut b, item, arc_key, keyfile, false);
        if item.packed { decompress(&b) } else { Ok(b) }
    }

    pub fn read(&self, f: &mut fs::File, item: &Item) -> R<Vec<u8>> {
        Self::read_with(f, item, self.arc_key, self.keyfile.as_deref())
    }

    /// The pack with `new` files in place of the old: (index in `items`, plain contents). They go after
    /// the old data, ahead of the index and the tail, which stay last as the engine looks for them there.
    fn with(&self, pack: &[u8], new: &[(usize, Vec<u8>)]) -> R<Vec<u8>> {
        let mut out = pack[..self.index_at as usize].to_vec();
        let mut index = self.index.clone();
        for (i, plain) in new {
            let item = &self.items[*i];
            let mut b = plain.clone();
            crypt(&mut b, item, self.arc_key, self.keyfile.as_deref(), true);
            index[item.entry..item.entry + 8].copy_from_slice(&(out.len() as u64).to_le_bytes());
            index[item.entry + 8..item.entry + 12].copy_from_slice(&(b.len() as u32).to_le_bytes());
            index[item.entry + 12..item.entry + 16].copy_from_slice(&(plain.len() as u32).to_le_bytes());
            index[item.entry + 16..item.entry + 20].copy_from_slice(&0u32.to_le_bytes());
            index[item.entry + 24..item.entry + 28].copy_from_slice(&hash(&b).to_le_bytes());
            out.extend(b);
        }
        let at = index.len() - 0x1c + 0x14;
        index[at..at + 8].copy_from_slice(&(out.len() as u64).to_le_bytes());
        out.extend(index);
        Ok(out)
    }
}

/// The game's packs, by name.
fn packs(game: &Path) -> R<Vec<PathBuf>> {
    let mut out: Vec<_> = fs::read_dir(game.join("GameData")).map_err(|e| e.to_string())?.flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pack"))).collect();
    out.sort();
    Ok(out)
}

// ---------- scripts ----------

/// Ruby, `[rb,word,reading]`, which comes out as the word.
static RUBY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[rb,([^,\]]*),[^\]]*\]").unwrap());

/// A run of text lines: a voice id, then `【name】`, then the message over one or more lines.
#[derive(Debug, PartialEq)]
struct Block {
    /// Lines of the message.
    from: usize,
    to: usize,
    /// The line holding the name.
    name: Option<usize>,
}

fn is_text(line: &str) -> bool {
    line.starts_with("[n]") || line.chars().next().is_some_and(|c| !c.is_ascii())
}

/// A wide mark, then an id like `har01_yut0001`.
fn is_voice(line: &str) -> bool {
    line.contains('_') && line.chars().skip(1).all(|c| c.is_ascii())
}

fn speaker(line: &str) -> Option<&str> {
    line.strip_prefix('【')?.strip_suffix('】')
}

fn blocks(lines: &[String]) -> Vec<Block> {
    let (mut out, mut i) = (Vec::new(), 0);
    while i < lines.len() {
        if !is_text(&lines[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && is_text(&lines[i]) {
            i += 1;
        }
        let mut from = start + lines[start..i].iter().take_while(|l| is_voice(l)).count();
        let name = (from < i && speaker(&lines[from]).is_some()).then_some(from);
        from += name.is_some() as usize;
        if from < i {
            out.push(Block { from, to: i, name });
        }
    }
    out
}

fn plain(lines: &[String]) -> String {
    RUBY.replace_all(&lines.iter().map(|l| l.strip_prefix("[n]").unwrap_or(l)).collect::<String>(), "$1").into_owned()
}

/// The options of a `^select` line.
fn options(line: &str) -> Option<Vec<&str>> {
    Some(line.strip_prefix("^select,")?.split(',').collect())
}

/// A script: its lines as stored, decoded for reading.
struct Script {
    pack: usize,
    item: usize,
    name: String,
    raw: Vec<Vec<u8>>,
    lines: Vec<String>,
    blocks: Vec<Block>,
}

impl Script {
    fn new(pack: usize, item: usize, name: String, bytes: &[u8]) -> Script {
        let raw: Vec<Vec<u8>> = bytes.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec()).collect();
        let lines: Vec<String> = raw.iter().map(|l| SHIFT_JIS.decode_without_bom_handling(l).0.into_owned()).collect();
        let blocks = blocks(&lines);
        Script { pack, item, name, raw, lines, blocks }
    }

    fn texts(&self, out: &mut Vec<Line>) {
        for b in &self.blocks {
            let text = plain(&self.lines[b.from..b.to]);
            if JA.is_match(&text) {
                let who = b.name.and_then(|n| speaker(&self.lines[n])).map(String::from);
                out.push((format!("{}:{}", self.name, b.from), who, text));
            }
        }
        for (n, line) in self.lines.iter().enumerate() {
            for (i, option) in options(line).unwrap_or_default().iter().enumerate().filter(|(_, o)| JA.is_match(o)) {
                out.push((format!("{}:{n}.{i}", self.name), None, option.to_string()));
            }
        }
    }

    /// The script with translations in. Lines the translation doesn't touch stay byte for byte.
    fn with(&self, width: usize, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> (Vec<u8>, usize) {
        let mut new: Vec<Option<Vec<Vec<u8>>>> = vec![None; self.raw.len()];
        let mut done = 0;
        for b in &self.blocks {
            let Some(tr) = line(&format!("{}:{}", self.name, b.from)) else { continue };
            new[b.from] = Some(wrap_words(&ascii_punctuation(&tr), width).iter().map(|l| sjis(l)).collect());
            for slot in &mut new[b.from + 1..b.to] {
                *slot = Some(Vec::new());
            }
            done += 1;
            if let Some((n, en)) = b.name.and_then(|n| Some((n, name(speaker(&self.lines[n])?)?))) {
                new[n] = Some(vec![sjis(&format!("【{en}】"))]);
                done += 1;
            }
        }
        for (n, l) in self.lines.iter().enumerate() {
            let Some(list) = options(l) else { continue };
            let parts: Vec<(String, bool)> = list
                .iter()
                .enumerate()
                .map(|(i, o)| match line(&format!("{}:{n}.{i}", self.name)) {
                    // a comma would start another option
                    Some(tr) => (ascii_punctuation(&tr).replace(',', ";"), true),
                    None => (o.to_string(), false),
                })
                .collect();
            if parts.iter().any(|p| p.1) {
                done += parts.iter().filter(|p| p.1).count();
                let joined: Vec<&str> = parts.iter().map(|p| p.0.as_str()).collect();
                new[n] = Some(vec![sjis(&format!("^select,{}", joined.join(",")))]);
            }
        }
        let mut out = Vec::new();
        let last = self.raw.len() - 1;
        for (i, raw) in self.raw.iter().enumerate() {
            let group: &[Vec<u8>] = new[i].as_deref().unwrap_or(std::slice::from_ref(raw));
            for l in group {
                out.extend(l);
                if i < last {
                    out.extend(b"\r\n");
                }
            }
        }
        (out, done)
    }
}

pub struct Game {
    packs: Vec<(PathBuf, Pack)>,
    scripts: Vec<Script>,
}

/// The pack as the game shipped it, once we have replaced it.
fn own(path: &Path) -> PathBuf {
    if backup(path).exists() { backup(path) } else { path.to_path_buf() }
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let (mut read, mut scripts) = (Vec::new(), Vec::new());
        for path in packs(game)? {
            let mut f = fs::File::open(own(&path)).map_err(|e| format!("{}: {e}", path.display()))?;
            let pack = Pack::open(&mut f)?;
            for (i, item) in pack.items.iter().enumerate().filter(|(_, i)| i.name.to_lowercase().ends_with(".s")) {
                let script = Script::new(read.len(), i, item.name.clone(), &pack.read(&mut f, item)?);
                if !script.blocks.is_empty() {
                    scripts.push(script);
                }
            }
            read.push((path, pack));
        }
        Ok(Game { packs: read, scripts })
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|s| s.texts(&mut out));
        out
    }

    /// How wide the game's text runs, in half-width letters: nearly all lines, as a few are exceptions.
    fn width(&self) -> usize {
        let mut widths: Vec<usize> = self.scripts.iter().flat_map(|s| s.blocks.iter().flat_map(|b| s.lines[b.from..b.to].iter().map(|l| text_units(l)))).collect();
        widths.sort();
        widths.get(widths.len() * 99 / 100).copied().unwrap_or(52).max(30)
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the packs and reads
/// them back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let width = crate::fit::letters(game, g.width());
    let mut done = 0;
    for (n, (path, pack)) in g.packs.iter().enumerate() {
        let mut new = Vec::new();
        for s in g.scripts.iter().filter(|s| s.pack == n) {
            let (bytes, count) = s.with(width, line, name);
            if count > 0 {
                new.push((s.item, bytes));
                done += count;
            }
        }
        if new.is_empty() {
            continue;
        }
        let out = pack.with(&original(path)?, &new)?;
        let check = path.with_extension("vntl-new");
        fs::write(&check, &out).map_err(|e| e.to_string())?;
        let read_back = (|| {
            let mut f = fs::File::open(&check).map_err(|e| e.to_string())?;
            let back = Pack::open(&mut f)?;
            new.iter().try_for_each(|(i, bytes)| {
                if back.read(&mut f, &back.items[*i])? == *bytes { Ok(()) } else { Err(format!("{} didn't read back as written", back.items[*i].name)) }
            })
        })();
        _ = fs::remove_file(&check);
        read_back?;
        replace_file(path, &out)?;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encrypted_name(name: &str, arc_key: u32) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let stream = name_stream(units.len(), arc_key);
        units.iter().zip(&stream).flat_map(|(u, k)| (u ^ k).to_le_bytes()).collect()
    }

    /// A pack of (name, contents), all encrypted the first way.
    fn pack(files: &[(&str, &[u8])]) -> Vec<u8> {
        let tail_key: Vec<u8> = (0..0x100u32).map(|i| (i * 7) as u8).collect();
        let arc_key = hash(&tail_key) & 0x0fff_ffff;
        let (mut data, mut index) = (Vec::new(), Vec::new());
        for (name, bytes) in files {
            let item = Item { name: name.to_string(), place: 0, size: bytes.len(), packed: false, method: 1, entry: 0 };
            let mut b = bytes.to_vec();
            crypt(&mut b, &item, arc_key, None, true);
            index.extend((name.encode_utf16().count() as u16).to_le_bytes());
            index.extend(encrypted_name(name, arc_key));
            index.extend((data.len() as u64).to_le_bytes());
            index.extend((b.len() as u32).to_le_bytes());
            index.extend((bytes.len() as u32).to_le_bytes());
            index.extend(0u32.to_le_bytes());
            index.extend(1u32.to_le_bytes());
            index.extend(hash(&b).to_le_bytes());
            data.extend(b);
        }
        let mut tail = tail_key;
        tail.resize(0x41c - 0x1c, 0);
        let mut foot = b"FilePackVer3.1\0\0".to_vec();
        foot.extend((files.len() as u32).to_le_bytes());
        foot.extend((data.len() as u64).to_le_bytes());
        [data, index, tail, foot].concat()
    }

    fn game(files: &[(&str, &[u8])]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-qlie-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("GameData")).unwrap();
        fs::write(dir.join("GameData/data0.pack"), pack(files)).unwrap();
        dir
    }

    fn script(text: &str) -> Vec<u8> {
        sjis(&text.replace('\n', "\r\n"))
    }

    #[test]
    fn blocks_tell_voices_names_and_text_apart() {
        let lines: Vec<String> = ["^bg01,a", "％tar01_a0001", "【太郎】", "「こんにちは", "[n]元気？」", "", "地の文です", "^select,はい,いいえ"].map(String::from).into();
        let found = blocks(&lines);
        assert_eq!(found, [Block { from: 3, to: 5, name: Some(2) }, Block { from: 6, to: 7, name: None }]);
        assert_eq!(plain(&lines[3..5]), "「こんにちは元気？」");
        assert_eq!(options(&lines[7]).unwrap(), ["はい", "いいえ"]);
        assert_eq!(plain(&["[rb,漢字,かんじ]です".to_string()]), "漢字です");
    }

    #[test]
    fn the_encryption_comes_back_both_ways() {
        for method in [1, 2] {
            let item = Item { name: "a\\b.s".to_string(), place: 0, size: 0, packed: false, method, entry: 0 };
            let plain: Vec<u8> = (0..64).collect();
            let mut b = plain.clone();
            crypt(&mut b, &item, 0x1234, None, true);
            assert_ne!(b, plain);
            crypt(&mut b, &item, 0x1234, None, false);
            assert_eq!(b, plain);
        }
    }

    #[test]
    fn translations_go_in_and_the_pack_reads_back() {
        let s = script("^bg01,a\n％tar01_a0001\n【太郎】\n「こんにちは」\n\n地の文です\n^select,はい,いいえ\n");
        let untouched: &[u8] = b"an image";
        let dir = game(&[("a.png", untouched), ("scene\\one.s", &s)]);
        let lines = Game::open(&dir).unwrap().lines();
        assert_eq!(lines[0], ("scene\\one.s:3".to_string(), Some("太郎".to_string()), "「こんにちは」".to_string()));
        assert_eq!(lines.len(), 4);

        let line = |k: &str| match k.rsplit(':').next().unwrap() {
            "3" => Some("\"Hello.\"".to_string()),
            "6.0" => Some("Yes, please".to_string()),
            _ => None,
        };
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 3);

        let path = dir.join("GameData/data0.pack");
        let mut f = fs::File::open(&path).unwrap();
        let p = Pack::open(&mut f).unwrap();
        assert_eq!(p.read(&mut f, &p.items[0]).unwrap(), untouched);
        let text = String::from_utf8_lossy(&p.read(&mut f, &p.items[1]).unwrap()).into_owned();
        assert!(text.contains("\"Hello.\"\r\n") && text.contains("Taro") && text.contains("^bg01,a\r\n"));
        assert!(text.contains("^select,Yes; please,"));
        drop(f);

        let once = fs::read(&path).unwrap();
        install(&dir, &line, &name).unwrap(); // a second run starts from the original, not from itself
        assert_eq!(fs::read(&path).unwrap(), once);
    }
}
