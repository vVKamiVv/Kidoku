//! Liar-soft engine (2000s games with scr.xfl full of compiled .gsc scripts): XFL archives and
//! the string tables of GSC scripts. The bytecode refers to strings by index, so a table can be
//! rebuilt with new text without touching the code.

use crate::text::{ascii_punctuation, decoded, sjis};
use crate::{original, replace_file, R};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

fn u32_at(b: &[u8], p: usize) -> R<u32> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated Liar-soft file")?.try_into().unwrap()))
}

// ---------- XFL archives ----------

pub struct Entry {
    pub name: String,
    pub data: Vec<u8>,
}

pub fn read_xfl(b: &[u8]) -> R<Vec<Entry>> {
    if b.get(..4) != Some(b"LB\x01\0") {
        return Err("not a Liar-soft XFL archive".into());
    }
    let (index, count) = (u32_at(b, 4)? as usize, u32_at(b, 8)? as usize);
    let base = 12 + index;
    (0..count)
        .map(|i| {
            let at = 12 + 40 * i;
            let name = b.get(at..at + 32).ok_or("truncated XFL index")?;
            let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(32)]).into_owned();
            let (off, size) = (u32_at(b, at + 32)? as usize, u32_at(b, at + 36)? as usize);
            let data = b.get(base + off..base + off + size).ok_or("XFL entry out of range")?.to_vec();
            Ok(Entry { name, data })
        })
        .collect()
}

pub fn write_xfl(entries: &[Entry]) -> Vec<u8> {
    let mut out = b"LB\x01\0".to_vec();
    out.extend(((entries.len() * 40) as u32).to_le_bytes());
    out.extend((entries.len() as u32).to_le_bytes());
    let mut off = 0u32;
    for e in entries {
        let mut name = [0u8; 32];
        let n = e.name.len().min(31);
        name[..n].copy_from_slice(&e.name.as_bytes()[..n]);
        out.extend(name);
        out.extend(off.to_le_bytes());
        out.extend((e.data.len() as u32).to_le_bytes());
        off += e.data.len() as u32;
    }
    for e in entries {
        out.extend(&e.data);
    }
    out
}

// ---------- GSC scripts ----------

/// A compiled script: its string table, and everything around it kept as bytes.
pub struct Gsc {
    head: [u32; 9],
    code: Vec<u8>,
    pub strings: Vec<Vec<u8>>, // Shift-JIS, without the NUL
    rest: Vec<u8>,
}

pub fn read_gsc(b: &[u8]) -> R<Gsc> {
    let mut head = [0u32; 9];
    for (i, h) in head.iter_mut().enumerate() {
        *h = u32_at(b, 4 * i)?;
    }
    if head[1] != 36 || head[3] % 4 != 0 {
        return Err("not a Liar-soft GSC script".into());
    }
    let (code, index, data) = (36 + head[2] as usize, head[3] as usize, head[4] as usize);
    let table = b.get(code + index..code + index + data).ok_or("truncated GSC string table")?;
    let strings = (0..index / 4)
        .map(|k| {
            let tail = table.get(u32_at(b, code + 4 * k)? as usize..).ok_or("bad GSC string offset")?;
            Ok(tail[..tail.iter().position(|&c| c == 0).ok_or("unterminated GSC string")?].to_vec())
        })
        .collect::<R<_>>()?;
    Ok(Gsc { head, code: b[36..code].to_vec(), strings, rest: b[code + index + data..].to_vec() })
}

impl Gsc {
    pub fn write(&self) -> Vec<u8> {
        let (mut offsets, mut data) = (Vec::new(), Vec::new());
        for s in &self.strings {
            offsets.extend((data.len() as u32).to_le_bytes());
            data.extend(s);
            data.push(0);
        }
        let mut head = self.head;
        head[0] = (head[0] as i64 + data.len() as i64 - head[4] as i64) as u32; // the total moves with the table
        head[4] = data.len() as u32;
        let mut out: Vec<u8> = head.iter().flat_map(|h| h.to_le_bytes()).collect();
        out.extend(&self.code);
        out.extend(offsets);
        out.extend(data);
        out.extend(&self.rest);
        out
    }
}

// ---------- text ----------

// Codes in front of a message: who's speaking (^g123^n), colour (^cX), ^d12@3, choice number (<1>).
static PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:\^g\d+\^n|\^c[a-z]|\^d\d+(?:@\d+)?|<\d+>)*").unwrap());
static SPEAKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\^g(\d+)\^n").unwrap());
/// Codes the model must keep inside a message (its ^n line breaks travel as real newlines).
pub static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\^[a-z]").unwrap());

/// (codes in front, the text as the model sees it)
pub fn split(s: &str) -> (&str, String) {
    let n = PREFIX.find(s).map_or(0, |m| m.end());
    (&s[..n], s[n..].replace("^n", "\n"))
}

/// The code of who says a message, which names its plate in `plates`.
pub fn speaker_code(s: &str) -> Option<&str> {
    Some(SPEAKER.captures(&s[..PREFIX.find(s)?.end()])?.get(1)?.as_str())
}

/// The name plates drawn beside each speaker's messages, by speaker code. The scripts hold no names,
/// so these images are the only place a character is named.
pub fn plates(game: &Path) -> R<BTreeMap<String, Image>> {
    let path = game.join("grps.xfl");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let entries = read_xfl(&original(&path)?)?;
    let code = |name: &str| Some(name.strip_prefix("gf")?.strip_suffix(".wcg")?.to_string());
    entries.iter().filter_map(|e| Some((code(&e.name)?, e))).map(|(c, e)| Ok((c, read_wcg(&e.data).map_err(|err| format!("{}: {err}", e.name))?))).collect()
}

// shortcut: one game's text box, measured from a screenshot (24 px text, 480 px per line); make
// these per-game settings once a Liar-soft game with another box size turns up.
pub const LINE_PX: usize = 480;
const FULL_PX: usize = 24;
/// Advance of ' '..='~' in 24 px MS PGothic, the font patch_exe selects.
const ASCII_PX: [u8; 95] = [
    7, 5, 12, 12, 12, 12, 14, 5, 7, 7, 12, 12, 5, 12, 5, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 5, 5, 12, 12, 12, 11, 16,
    15, 15, 16, 16, 14, 13, 16, 15, 6, 13, 14, 13, 18, 15, 17, 15, 17, 15, 14, 14, 15, 15, 18, 14, 14, 14, 8, 12, 8, 10, 7, 10,
    11, 12, 12, 12, 12, 7, 11, 12, 5, 5, 11, 5, 18, 12, 12, 12, 12, 8, 11, 8, 12, 11, 16, 11, 11, 11, 6, 6, 6, 10,
];

fn px(s: &str) -> usize {
    s.chars().map(|c| if (' '..='~').contains(&c) { ASCII_PX[c as usize - 32] as usize } else { FULL_PX }).sum()
}

/// Break each paragraph between words so no line is wider than LINE_PX; the engine would
/// otherwise cut English words in half at its own line end.
pub fn wrap(text: &str) -> String {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            let mut word = word.to_string();
            loop {
                let joined = if line.is_empty() { word.clone() } else { format!("{line} {word}") };
                if px(&joined) <= LINE_PX {
                    line = joined;
                    break;
                }
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    continue;
                }
                // a single word wider than a line: cut it where the engine would
                let cut = word.char_indices().scan(0, |w, (i, c)| { *w += px(&c.to_string()); Some((i, *w)) })
                    .find(|(_, w)| *w > LINE_PX).map_or(word.len(), |(i, _)| i);
                lines.push(word[..cut].to_string());
                word = word[cut..].to_string();
                if word.is_empty() {
                    break;
                }
            }
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// The model's English back into a script string: line breaks as ^n, Shift-JIS bytes. Punctuation
/// and symbols become ASCII: the engine only recognises two-byte characters on a Japanese-locale
/// Windows, so `…` would come out as garbage elsewhere. Accented letters lose their accents, and
/// what Shift-JIS can't hold becomes '?'.
pub fn encode(prefix: &str, text: &str) -> Vec<u8> {
    sjis(&format!("{prefix}{}", wrap(&ascii_punctuation(text)).replace('\n', "^n")))
}

/// Whether `encode` had to put in a '?' the text doesn't have. Counting bytes is safe: 0x3F is never
/// the second byte of a Shift-JIS character.
pub fn lossy(text: &str) -> bool {
    encode("", text).iter().filter(|b| **b == b'?').count() > text.matches(['?', '？']).count()
}

// ---------- WCG images ----------

/// Pixels as stored: 0xAARRGGBB, with alpha inverted (0 is opaque).
#[derive(Clone, PartialEq, Debug)]
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

struct Bits<'a> {
    b: &'a [u8],
    p: usize,
    left: u32,
}

impl Bits<'_> {
    fn read(&mut self, mut n: u32) -> R<usize> {
        let mut v = 0usize;
        while n > 0 {
            let byte = *self.b.get(self.p).ok_or("truncated WCG data")? as usize;
            let take = n.min(self.left);
            v = (v << take) | ((byte & ((1 << self.left) - 1)) >> (self.left - take));
            self.left -= take;
            n -= take;
            if self.left == 0 {
                self.p += 1;
                self.left = 8;
            }
        }
        Ok(v)
    }

    /// A table index. Tag 1 is 0-1; tags 2..=6 are the next power-of-two ranges; tag 7 escapes to a
    /// run of 1 bits (each adds a bit of length) ending in 0, for indexes from 64 to 4095.
    fn index(&mut self, tag: usize) -> R<usize> {
        match tag {
            1 => self.read(1),
            2 => Ok(self.read(1)? | 2),
            3..=6 => Ok(self.read(tag as u32 - 1)? | (1 << (tag - 1))),
            _ => {
                for n in 6..=11 {
                    if self.read(1)? == 0 {
                        return Ok(self.read(n)? | (1 << n));
                    }
                }
                Err("bad WCG code".into())
            }
        }
    }
}

fn read_plane(d: &[u8], at: &mut usize) -> R<Vec<u16>> {
    let (slots, size, tn) = (u32_at(d, *at)? as usize, u32_at(d, *at + 4)? as usize, (u32_at(d, *at + 8)? & 0xffff) as usize);
    if tn > 0x1000 {
        return Err("WCG table too large".into()); // the engine has a second code set for these
    }
    let table: Vec<u16> = d.get(*at + 12..*at + 12 + tn * 2).ok_or("truncated WCG table")?.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let start = *at + 12 + tn * 2;
    let mut bits = Bits { b: d.get(start..start + size).ok_or("truncated WCG data")?, p: 0, left: 8 };
    let mut out = Vec::with_capacity(slots / 2);
    while out.len() < slots / 2 {
        let tag = bits.read(3)?;
        let (run, tag) = if tag == 0 { (bits.read(4)? + 2, bits.read(3)?) } else { (1, tag) };
        let v = *table.get(bits.index(tag)?).ok_or("bad WCG index")?;
        out.extend(std::iter::repeat_n(v, run));
    }
    *at = start + size;
    Ok(out)
}

pub fn read_wcg(d: &[u8]) -> R<Image> {
    let flags = d.get(2..4).map(|f| u16::from_le_bytes([f[0], f[1]])).ok_or("truncated WCG")?;
    if d.get(..2) != Some(b"WG") || flags & 0xf != 1 || flags & 0x1c0 != 0x40 {
        return Err("not a WCG image this app reads".into());
    }
    let (w, h) = (u32_at(d, 8)? as usize, u32_at(d, 12)? as usize);
    let mut at = 16;
    let (hi, lo) = (read_plane(d, &mut at)?, read_plane(d, &mut at)?);
    if hi.len() < w * h || lo.len() < w * h || at != d.len() {
        return Err("WCG size mismatch".into());
    }
    Ok(Image { w, h, px: (0..w * h).map(|i| lo[i] as u32 | (hi[i] as u32) << 16).collect() })
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    cur: u32,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, v: usize, bits: u32) {
        for i in (0..bits).rev() {
            self.cur = self.cur << 1 | (v >> i & 1) as u32;
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.cur as u8);
                (self.cur, self.n) = (0, 0);
            }
        }
    }

    fn tag_and_index(&mut self, i: usize) {
        if i < 4 {
            self.put(if i < 2 { 1 } else { 2 }, 3);
            self.put(i & 1, 1);
        } else if i < 64 {
            let n = i.ilog2();
            self.put(n as usize + 1, 3);
            self.put(i - (1 << n), n);
        } else {
            let (n, ones) = (i.ilog2(), i.ilog2() - 6);
            self.put(7, 3);
            self.put(((1 << ones) - 1) << 1, ones + 1); // that many 1 bits, then a 0
            self.put(i - (1 << n), n);
        }
    }
}

fn write_plane(vals: &[u16]) -> R<Vec<u8>> {
    let mut freq = std::collections::HashMap::new();
    vals.iter().for_each(|v| *freq.entry(*v).or_insert(0usize) += 1);
    let mut table: Vec<u16> = freq.keys().copied().collect();
    table.sort_by_key(|v| (std::cmp::Reverse(freq[v]), *v)); // short codes for common values
    if table.len() > 0x1000 {
        return Err("too many colours for a WCG table".into());
    }
    let at: std::collections::HashMap<u16, usize> = table.iter().enumerate().map(|(i, v)| (*v, i)).collect();
    let mut w = BitWriter::default();
    let mut i = 0;
    while i < vals.len() {
        let run = vals[i..].iter().take(17).take_while(|v| **v == vals[i]).count();
        if run >= 2 {
            w.put(0, 3);
            w.put(run - 2, 4);
        }
        w.tag_and_index(at[&vals[i]]);
        i += run;
    }
    if w.n > 0 {
        w.put(0, 8 - w.n);
    }
    let mut out = Vec::new();
    for v in [vals.len() as u32 * 2, w.out.len() as u32, table.len() as u32] {
        out.extend(v.to_le_bytes());
    }
    table.iter().for_each(|v| out.extend(v.to_le_bytes()));
    out.extend(w.out);
    Ok(out)
}

/// `head` is the original file, whose 16-byte header is kept.
pub fn write_wcg(head: &[u8], img: &Image) -> R<Vec<u8>> {
    let mut out = head.get(..16).ok_or("truncated WCG")?.to_vec();
    out.extend(write_plane(&img.px.iter().map(|p| (p >> 16) as u16).collect::<Vec<_>>())?);
    out.extend(write_plane(&img.px.iter().map(|p| *p as u16).collect::<Vec<_>>())?);
    Ok(out)
}

// ---------- LWG containers ----------

/// A screen's images with their positions.
pub struct Lwg {
    head: Vec<u8>,
    pub sprites: Vec<Sprite>,
}

pub struct Sprite {
    pub x: i32,
    pub y: i32,
    flag: u8,
    name_raw: Vec<u8>,
    pub data: Vec<u8>,
}

impl Sprite {
    pub fn name(&self) -> String {
        decoded(&self.name_raw)
    }
}

pub fn read_lwg(d: &[u8]) -> R<Lwg> {
    if d.get(..4) != Some(b"LG\x01\0") {
        return Err("not an LWG container".into());
    }
    let (count, index) = (u32_at(d, 12)? as usize, u32_at(d, 0x14)? as usize);
    let base = 0x18 + index + 4; // the data section starts with its own length
    let mut at = 0x18;
    let mut sprites = Vec::new();
    for _ in 0..count {
        let e = d.get(at..at + 18).ok_or("truncated LWG index")?;
        let (x, y, off, size) = (u32_at(e, 0)? as i32, u32_at(e, 4)? as i32, u32_at(e, 9)? as usize, u32_at(e, 13)? as usize);
        let len = e[17] as usize;
        let name_raw = d.get(at + 18..at + 18 + len).ok_or("truncated LWG index")?.to_vec();
        at += 18 + len;
        sprites.push(Sprite { x, y, flag: e[8], name_raw, data: d.get(base + off..base + off + size).ok_or("LWG entry out of range")?.to_vec() });
    }
    Ok(Lwg { head: d[..0x18].to_vec(), sprites })
}

pub fn write_lwg(l: &Lwg) -> Vec<u8> {
    let (mut index, mut data): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
    for s in &l.sprites {
        index.extend(s.x.to_le_bytes());
        index.extend(s.y.to_le_bytes());
        index.push(s.flag);
        index.extend((data.len() as u32).to_le_bytes());
        index.extend((s.data.len() as u32).to_le_bytes());
        index.push(s.name_raw.len() as u8);
        index.extend(&s.name_raw);
        data.extend(&s.data);
    }
    let mut out = l.head.clone();
    out[0x14..0x18].copy_from_slice(&(index.len() as u32).to_le_bytes());
    out.extend(index);
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    out
}

// ---------- the engine ----------

// In the message window's glyph routine: `mov ax,[esp+34h]; shr ax,1; mov [esi+18h],ax` forces every
// single-byte character to half the font size, over the real advance Windows just gave it.
const HALF_WIDTH: [u8; 12] = [0x66, 0x8b, 0x44, 0x24, 0x34, 0x66, 0xd1, 0xe8, 0x66, 0x89, 0x46, 0x18];
const JMP_OVER_IT: [u8; 2] = [0xeb, 0x0a];
// Shift-JIS "ＭＳ ゴシック" and "ＭＳ 明朝": meaningless to a non-Japanese Windows, which substitutes
// a font of its own choosing, so name one font in ASCII to get the same widths everywhere.
const FONT_NAMES: [&[u8]; 2] = [b"\x82\x6c\x82\x72\x20\x83\x53\x83\x56\x83\x62\x83\x4e\0", b"\x82\x6c\x82\x72\x20\x96\xbe\x92\xa9\0"];
const FONT: &[u8] = b"MS PGothic\0";

fn find_once(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let mut hits = hay.windows(needle.len()).enumerate().filter(|(_, w)| *w == needle).map(|(i, _)| i);
    hits.next().filter(|_| hits.next().is_none())
}

/// Give English letters their real widths. False, with nothing changed, if this isn't an engine
/// build we know.
fn fix_engine(exe: &mut [u8]) -> bool {
    let Some(width) = find_once(exe, &HALF_WIDTH) else { return false };
    let mut names = Vec::new();
    for old in FONT_NAMES {
        // a longer new name only fits over NUL padding
        let fits = |at: usize| exe.get(at + old.len().min(FONT.len())..at + FONT.len()).is_some_and(|pad| pad.iter().all(|&b| b == 0));
        match find_once(exe, old) {
            Some(at) if fits(at) => names.push(at),
            _ => return false,
        }
    }
    exe[width..width + 2].copy_from_slice(&JMP_OVER_IT);
    for at in names {
        exe[at..at + FONT.len()].copy_from_slice(FONT);
    }
    true
}

/// Patch the game's .exe (the original is kept as *.vntl-orig). Returns whether one was patched.
pub fn patch_exe(game: &Path) -> R<bool> {
    for e in std::fs::read_dir(game).map_err(|e| e.to_string())?.flatten() {
        let path = e.path();
        if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")) {
            let mut exe = original(&path)?;
            if fix_engine(&mut exe) {
                replace_file(&path, &exe)?;
                return Ok(true);
            }
        }
    }
    Ok(false)
}

// ---------- the game ----------

/// Every string of the game's scripts as shipped: (script, index, text).
pub fn texts(game: &Path) -> R<Vec<(String, usize, String)>> {
    let mut out = Vec::new();
    for e in read_xfl(&original(&game.join("scr.xfl"))?)?.into_iter().filter(|e| e.name.ends_with(".gsc")) {
        let g = read_gsc(&e.data).map_err(|err| format!("{}: {err}", e.name))?;
        out.extend(g.strings.iter().enumerate().map(|(i, s)| (e.name.clone(), i, decoded(s))));
    }
    Ok(out)
}

/// Rebuild scr.xfl from the game's original with `text(script, index, shipped text)` wherever it
/// gives new bytes. The new archive is read back and checked before it replaces the game's (kept as
/// scr.xfl.vntl-orig). Returns how many strings were written.
#[allow(clippy::type_complexity)]
pub fn install(game: &Path, text: &dyn Fn(&str, usize, &str) -> Option<Vec<u8>>) -> R<usize> {
    let path = game.join("scr.xfl");
    let mut entries = read_xfl(&original(&path)?)?;
    let mut want = Vec::new(); // (entry, string index, bytes)
    let mut counts = Vec::new();
    for (k, e) in entries.iter_mut().enumerate().filter(|(_, e)| e.name.ends_with(".gsc")) {
        let mut g = read_gsc(&e.data).map_err(|err| format!("{}: {err}", e.name))?;
        counts.push((k, g.strings.len()));
        let mut changed = false;
        for (i, s) in g.strings.iter_mut().enumerate() {
            if let Some(new) = text(&e.name, i, &decoded(s)).filter(|n| n != s) {
                want.push((k, i, new.clone()));
                *s = new;
                changed = true;
            }
        }
        if changed {
            e.data = g.write();
        }
    }
    if want.is_empty() {
        return Ok(0);
    }
    let bytes = write_xfl(&entries);
    let back = read_xfl(&bytes)?;
    let scripts: Vec<Option<Gsc>> = back.iter().map(|e| read_gsc(&e.data).ok()).collect();
    let same_shape = back.len() == entries.len()
        && counts.iter().all(|(k, n)| scripts[*k].as_ref().is_some_and(|g| g.strings.len() == *n));
    let placed = want.iter().all(|(k, i, n)| scripts[*k].as_ref().is_some_and(|g| g.strings.get(*i) == Some(n)));
    if !same_shape || !placed {
        return Err("the rebuilt scr.xfl did not read back correctly; nothing was written".into());
    }
    replace_file(&path, &bytes)?;
    Ok(want.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gsc(strings: &[&[u8]]) -> Vec<u8> {
        let (mut idx, mut data) = (Vec::new(), Vec::new());
        for s in strings {
            idx.extend((data.len() as u32).to_le_bytes());
            data.extend(*s);
            data.push(0);
        }
        let (code, rest) = (vec![7u8; 8], vec![9u8; 5]);
        let total = 36 + code.len() + idx.len() + data.len() + 8 + rest.len();
        let head = [total as u32, 36, code.len() as u32, idx.len() as u32, data.len() as u32, 4, 0, 4, 0];
        let mut out: Vec<u8> = head.iter().flat_map(|h| h.to_le_bytes()).collect();
        out.extend(code);
        out.extend(idx);
        out.extend(data);
        out.extend(rest);
        out
    }

    #[test]
    fn archives_and_scripts_rebuild_byte_for_byte_and_take_new_text() {
        let raw = gsc(&[b"bg01", b"^g001^nabc"]);
        let xfl = write_xfl(&[Entry { name: "0001.gsc".into(), data: raw.clone() }]);
        let back = read_xfl(&xfl).unwrap();
        assert_eq!((back[0].name.as_str(), &back[0].data), ("0001.gsc", &raw));
        assert_eq!(read_gsc(&raw).unwrap().write(), raw);
        let mut g = read_gsc(&raw).unwrap();
        g.strings[1] = b"^g001^nHello there".to_vec();
        let longer = g.write();
        assert_eq!(read_gsc(&longer).unwrap().strings[1], b"^g001^nHello there");
        assert_eq!(u32_at(&longer, 0).unwrap(), u32_at(&raw, 0).unwrap() + 8); // the total grew with the text
        assert_eq!(&longer[longer.len() - 5..], [9u8; 5]); // what follows the table is untouched
    }

    #[test]
    fn codes_stay_out_of_the_model_and_come_back() {
        let (prefix, text) = split("^g012^nこんにちは^nまたね");
        assert_eq!((prefix, text.as_str()), ("^g012^n", "こんにちは\nまたね"));
        assert_eq!(encode(prefix, "“Hi” — it’s me\nBye"), b"^g012^n\"Hi\" - it's me^nBye");
        assert_eq!(encode("", "café Zoë"), b"cafe Zoe");
        assert_eq!(encode("", "a😀"), b"a?");
        assert_eq!(encode("", "★3 × 4 → ●"), b"*3 x 4 > *");
        assert_eq!(encode("", "37°"), b"37 deg");
        assert!(lossy("a😀") && !lossy("what? ？") && !lossy("café"));
        assert_eq!(encode("", "Well… ♪"), b"Well... ~");
        assert_eq!(encode("^g012^n", "……！？"), b"^g012^n...!?");
        assert_eq!(encode("", "ＬＯＡＤ　ａ１"), b"LOAD a1");
    }

    #[test]
    fn engine_fix_skips_the_half_width_and_names_one_font() {
        let mut exe = vec![0x90u8; 4];
        exe.extend(HALF_WIDTH);
        exe.extend(FONT_NAMES[0]);
        exe.extend([0, 0]);
        exe.extend(FONT_NAMES[1]);
        exe.extend([0, 0]);
        let original = exe.clone();
        assert!(fix_engine(&mut exe));
        assert_eq!(exe[4..6], JMP_OVER_IT);
        assert_eq!(&exe[6..16], &HALF_WIDTH[2..]); // only the jump changed
        assert_eq!(exe.windows(FONT.len()).filter(|w| *w == FONT).count(), 2);
        assert_eq!(exe.len(), original.len());
        // an unknown build is left alone
        let mut other = vec![0u8; 32];
        assert!(!fix_engine(&mut other));
        assert_eq!(other, vec![0u8; 32]);
        // no room for the new name: nothing changes
        let mut tight = original.clone();
        let at = find_once(&tight, FONT_NAMES[1]).unwrap();
        tight[at + FONT_NAMES[1].len()] = 0x41;
        let before = tight.clone();
        assert!(!fix_engine(&mut tight));
        assert_eq!(tight, before);
    }

    #[test]
    fn english_wraps_between_words_within_the_line() {
        let text = "I think I'm smiling. As much as I can, just like always. And then I sit.";
        let wrapped = wrap(text);
        assert!(wrapped.lines().all(|l| px(l) <= LINE_PX));
        assert_eq!(wrapped.replace('\n', " "), text); // nothing lost, no word cut
        assert_eq!(wrap("short\nnew paragraph"), "short\nnew paragraph");
        assert!(wrap(&"m".repeat(50)).lines().all(|l| px(l) <= LINE_PX));
        assert_eq!(px("I think I'm smiling. As much as I can, just"), 402); // 471 with the engine's half-width bug
    }
}
