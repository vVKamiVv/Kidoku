//! CatSystem2 engine (cs2.exe, scene.int): KIF archives, Blowfish encrypted with a key made from a
//! passphrase in the program's resources, holding .cst scenes: zlib packed tables of typed strings.

use crate::text::decoded;
use crate::blowfish::Blowfish;
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, text_units, wrap_words, JA};
use crate::{original, replace_file, R};
use encoding_rs::SHIFT_JIS;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

fn word(b: &[u8], p: usize) -> R<u32> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated CatSystem2 file")?.try_into().unwrap()))
}

fn short(b: &[u8], p: usize) -> R<u16> {
    Ok(u16::from_le_bytes(b.get(p..p + 2).ok_or("truncated CatSystem2 file")?.try_into().unwrap()))
}

// ---------- the key ----------

/// A resource of a Windows program, found by the names of its type and entry.
fn resource(exe: &[u8], kind: &str, name: &str) -> Option<Vec<u8>> {
    let pe = word(exe, 0x3c).ok()? as usize;
    let sections = short(exe, pe + 6).ok()? as usize;
    let table = pe + 24 + short(exe, pe + 20).ok()? as usize;
    let section = (0..sections).map(|i| table + 40 * i).find(|&s| exe.get(s..s + 5) == Some(b".rsrc"))?;
    let (rva, raw) = (word(exe, section + 12).ok()? as usize, word(exe, section + 20).ok()? as usize);
    let find = |dir: usize, wanted: &str| -> Option<u32> {
        let at = raw + dir;
        let (named, ids) = (short(exe, at + 12).ok()? as usize, short(exe, at + 14).ok()? as usize);
        (0..named + ids).find_map(|i| {
            let (id, to) = (word(exe, at + 16 + 8 * i).ok()?, word(exe, at + 20 + 8 * i).ok()?);
            if id & 0x8000_0000 == 0 {
                return None;
            }
            let s = raw + (id & 0x7fff_ffff) as usize;
            let n = short(exe, s).ok()? as usize;
            let units: Vec<u16> = exe.get(s + 2..s + 2 + 2 * n)?.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            (String::from_utf16_lossy(&units).eq_ignore_ascii_case(wanted)).then_some(to)
        })
    };
    let names = find(0, kind)? & 0x7fff_ffff;
    let langs = find(names as usize, name)? & 0x7fff_ffff;
    let leaf = raw + langs as usize;
    let data = raw + (word(exe, leaf + 20).ok()? & 0x7fff_ffff) as usize;
    let (at, size) = (word(exe, data).ok()? as usize, word(exe, data + 4).ok()? as usize);
    exe.get(at - rva + raw..at - rva + raw + size).map(<[u8]>::to_vec)
}

/// The passphrase's number: a CRC run from the top bit down.
fn pass_key(pass: &[u8]) -> u32 {
    let table = |n: u32| (0..8).fold(n << 24, |c, _| if c & 0x8000_0000 != 0 { c << 1 ^ 0x04c1_1db7 } else { c << 1 });
    pass.iter().fold(0xffff_ffff, |key, &c| !table(key >> 24 ^ c as u32) ^ key << 8)
}

/// The passphrase the program holds, encrypted under a key of its own or a default.
fn passphrase(exe: &[u8]) -> Option<Vec<u8>> {
    let mut code = resource(exe, "V_CODE2", "DATA")?;
    let key = resource(exe, "KEY_CODE", "KEY").map_or(b"windmill".to_vec(), |k| k.iter().map(|c| c ^ 0xcd).collect());
    Blowfish::new(&key).decrypt(&mut code);
    let n = code.iter().position(|&c| c == 0).unwrap_or(code.len());
    Some(code[..n].to_vec())
}

// ---------- the twister ----------

/// The engine's Mersenne Twister, which seeds differently from the usual one.
struct Twister {
    mt: [u32; 624],
    at: usize,
}

impl Twister {
    fn new(mut seed: u32) -> Twister {
        let mut mt = [0; 624];
        for v in &mut mt {
            let upper = seed & 0xffff_0000;
            seed = seed.wrapping_mul(69069).wrapping_add(1);
            *v = upper | seed >> 16;
            seed = seed.wrapping_mul(69069).wrapping_add(1);
        }
        Twister { mt, at: 624 }
    }

    fn next(&mut self) -> u32 {
        if self.at >= 624 {
            for k in 0..624 {
                let y = (self.mt[k] & 0x8000_0000) | (self.mt[(k + 1) % 624] & 0x7fff_ffff);
                self.mt[k] = self.mt[(k + 397) % 624] ^ y >> 1 ^ if y & 1 != 0 { 0x9908_b0df } else { 0 };
            }
            self.at = 0;
        }
        let mut y = self.mt[self.at];
        self.at += 1;
        y ^= y >> 11;
        y ^= y << 7 & 0x9d2c_5680;
        y ^= y << 15 & 0xefc6_0000;
        y ^ y >> 18
    }
}

// ---------- archives ----------

const ALPHABET: &[u8; 52] = b"zyxwvutsrqponmlkjihgfedcbaZYXWVUTSRQPONMLKJIHGFEDCBA";

fn name_of(raw: &[u8], key: u32) -> String {
    let first = (key >> 24).wrapping_add(key >> 16).wrapping_add(key >> 8).wrapping_add(key) as u8 as usize;
    let mut out = Vec::new();
    for (k, &c) in (first..).zip(raw.iter().take_while(|&&c| c != 0)) {
        out.push(match ALPHABET.iter().position(|&a| a == c) {
            Some(j) => ALPHABET[51 - (j + 52 - k % 52) % 52],
            None => c,
        });
    }
    SHIFT_JIS.decode_without_bom_handling(&out).0.into_owned()
}

/// One file of an archive: `entry` is its row in the table.
pub struct Item {
    pub name: String,
    entry: usize,
    place: usize,
    size: usize,
}

pub struct Archive {
    bytes: Vec<u8>,
    cipher: Blowfish,
    pub items: Vec<Item>,
}

const ROW: usize = 0x48;

impl Archive {
    fn read(bytes: Vec<u8>, main_key: u32) -> R<Archive> {
        if bytes.get(..4) != Some(b"KIF\0") {
            return Err("not a CatSystem2 archive".into());
        }
        if bytes.get(8..20) != Some(b"__key__.dat\0") {
            return Err("this CatSystem2 archive isn't encrypted the usual way".into());
        }
        let count = word(&bytes, 4)? as usize;
        let mut twister = Twister::new(word(&bytes, 8 + 0x44)?);
        let cipher = Blowfish::new(&twister.next().to_le_bytes());
        let mut items = Vec::with_capacity(count);
        for i in 1..count {
            let row = 8 + ROW * i;
            let raw = bytes.get(row..row + 0x40).ok_or("truncated CatSystem2 index")?;
            let (place, size) = cipher.decipher(word(&bytes, row + 0x40)?.wrapping_add(i as u32), word(&bytes, row + 0x44)?);
            let name = name_of(raw, Twister::new(main_key.wrapping_add(i as u32)).next());
            items.push(Item { name, entry: i, place: place as usize, size: size as usize });
        }
        Ok(Archive { bytes, cipher, items })
    }

    fn file(&self, item: &Item) -> R<Vec<u8>> {
        let mut b = self.bytes.get(item.place..item.place + item.size).ok_or("bad CatSystem2 entry")?.to_vec();
        self.cipher.decrypt(&mut b);
        Ok(b)
    }

    /// The archive with `new` files in place of the old, added on the end: (index in `items`, contents).
    fn with(&self, new: &[(usize, Vec<u8>)]) -> Vec<u8> {
        let mut out = self.bytes.clone();
        for (i, plain) in new {
            let item = &self.items[*i];
            let mut b = plain.clone();
            self.cipher.encrypt(&mut b);
            let (place, size) = self.cipher.encipher(out.len() as u32, b.len() as u32);
            let row = 8 + ROW * item.entry;
            out[row + 0x40..row + 0x44].copy_from_slice(&place.wrapping_sub(item.entry as u32).to_le_bytes());
            out[row + 0x44..row + 0x48].copy_from_slice(&size.to_le_bytes());
            out.extend(b);
        }
        out
    }
}

// ---------- scenes ----------

const MESSAGE: u16 = 0x2001;
const NAME: u16 = 0x2101;
const COMMAND: u16 = 0x3001;

/// A .cst, unpacked: a table of offsets into typed, zero-ended strings, after a block of other data.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    head: Vec<u8>,
    pub strings: Vec<(u16, Vec<u8>)>,
}

fn unpack(b: &[u8]) -> R<Vec<u8>> {
    if b.get(..8) != Some(b"CatScene") {
        return Err("not a CatSystem2 scene".into());
    }
    let mut out = Vec::with_capacity(word(b, 12)? as usize);
    ZlibDecoder::new(&b[16..]).read_to_end(&mut out).map_err(|e| format!("CatSystem2 scene: {e}"))?;
    Ok(out)
}

impl Scene {
    pub fn read(plain: &[u8]) -> R<Scene> {
        let (offsets, strings) = (word(plain, 8)? as usize + 16, word(plain, 12)? as usize + 16);
        let count = strings.checked_sub(offsets).ok_or("bad CatSystem2 scene")? / 4;
        let mut list = Vec::with_capacity(count);
        for i in 0..count {
            let at = strings + word(plain, offsets + 4 * i)? as usize;
            let tail = plain.get(at + 2..).ok_or("bad CatSystem2 string")?;
            let end = tail.iter().position(|&c| c == 0).ok_or("unterminated CatSystem2 string")?;
            list.push((short(plain, at)?, tail[..end].to_vec()));
        }
        Ok(Scene { head: plain[..offsets].to_vec(), strings: list })
    }

    pub fn write(&self) -> Vec<u8> {
        let (mut offsets, mut data) = (Vec::new(), Vec::new());
        for (kind, s) in &self.strings {
            offsets.extend((data.len() as u32).to_le_bytes());
            data.extend(kind.to_le_bytes());
            data.extend(s);
            data.push(0);
        }
        let mut out = self.head.clone();
        let strings = out.len() - 16 + offsets.len();
        out[12..16].copy_from_slice(&(strings as u32).to_le_bytes());
        out.extend(offsets);
        out.extend(data);
        let size = (out.len() - 16) as u32;
        out[..4].copy_from_slice(&size.to_le_bytes());
        out
    }
}

fn packed(plain: &[u8]) -> R<Vec<u8>> {
    let mut z = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(plain).map_err(|e| e.to_string())?;
    let body = z.finish().map_err(|e| e.to_string())?;
    let mut out = b"CatScene".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend((plain.len() as u32).to_le_bytes());
    out.extend(body);
    Ok(out)
}

// ---------- text ----------

/// `[word/reading]`, which comes out as the word.
static RUBY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^/\]]*)/[^\]]*\]").unwrap());
/// A choice: its number, the scene it leads to, and what it says, in brackets if it has spaces.
static CHOICE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+ \S+ )(?:\(:(.*)\)|(.*))$").unwrap());

/// A message: its lines of text, each a string of its own but for the `\n` that ends all but the last.
#[derive(Debug, PartialEq)]
struct Message {
    rows: Vec<usize>,
    speaker: Option<usize>,
    text: String,
}

impl Scene {
    fn messages(&self) -> Vec<Message> {
        let (mut out, mut speaker, mut i) = (Vec::new(), None, 0);
        while i < self.strings.len() {
            match self.strings[i].0 {
                NAME => speaker = Some(i),
                MESSAGE => {
                    let start = i;
                    while self.strings.get(i + 1).is_some_and(|s| s.0 == MESSAGE) {
                        i += 1;
                    }
                    let joined: String = self.strings[start..=i].iter().map(|s| decoded(&s.1)).collect();
                    let text = RUBY.replace_all(&joined, "$1").replace("\\n", "").replace("\\@", "");
                    if JA.is_match(&text) {
                        out.push(Message { rows: (start..=i).collect(), speaker, text });
                    }
                    speaker = None;
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// Rows that offer a choice, which follow an `fselect`, with what they say.
    fn choices(&self) -> Vec<(usize, String)> {
        let (mut out, mut open) = (Vec::new(), false);
        for (i, (_, b)) in self.strings.iter().enumerate().filter(|(_, s)| s.0 == COMMAND) {
            let t = decoded(b);
            if t.starts_with("fselect") {
                open = true;
            } else if open {
                match CHOICE.captures(&t) {
                    Some(c) => out.extend(c.get(2).or(c.get(3)).filter(|m| JA.is_match(m.as_str())).map(|m| (i, m.as_str().to_string()))),
                    None => open = false,
                }
            }
        }
        out
    }

    /// Lines go back into the rows the message had, so the rest of the scene still lines up. A
    /// shorter message leaves rows empty, and a longer one breaks lines inside its last row.
    fn with(&self, stem: &str, width: usize, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> (Scene, usize) {
        let mut out = self.clone();
        let mut done = 0;
        for m in self.messages() {
            let Some(tr) = line(&format!("{stem}.{}", m.rows[0])) else { continue };
            let lines = wrap_words(&ascii_punctuation(&tr), width);
            let pause = m.rows.iter().any(|&r| decoded(&self.strings[r].1).ends_with("\\@"));
            let mut rest = lines.iter().map(String::as_str);
            for (n, &row) in m.rows.iter().enumerate() {
                let last = n + 1 == m.rows.len();
                let text = if last { rest.by_ref().collect::<Vec<_>>().join("\\n") } else { rest.next().unwrap_or("").to_string() };
                let more = !last && rest.clone().next().is_some();
                out.strings[row].1 = sjis(&format!("{text}{}{}", if more { "\\n" } else { "" }, if pause && last { "\\@" } else { "" }));
            }
            done += 1;
            if let Some((who, en)) = m.speaker.and_then(|w| Some((w, name(&decoded(&self.strings[w].1))?))) {
                out.strings[who].1 = sjis(&en);
                done += 1;
            }
        }
        for (i, _) in self.choices() {
            if let Some(tr) = line(&format!("{stem}.{i}")) {
                let t = decoded(&self.strings[i].1);
                let head = CHOICE.captures(&t).map_or(String::new(), |c| c[1].to_string());
                out.strings[i].1 = sjis(&format!("{head}(:{})", ascii_punctuation(&tr).replace(')', "")));
                done += 1;
            }
        }
        (out, done)
    }
}

// ---------- the game ----------

/// An archive, and the scenes in it with their places in its list.
type Loaded = (PathBuf, Archive, Vec<(usize, Scene)>);

pub struct Game {
    archives: Vec<Loaded>,
}

fn find_key(game: &Path) -> R<u32> {
    for e in fs::read_dir(game).map_err(|e| e.to_string())?.flatten() {
        if e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")) {
            let exe = fs::read(e.path()).map_err(|e| e.to_string())?;
            if let Some(pass) = passphrase(&exe) {
                return Ok(pass_key(&pass));
            }
        }
    }
    Err("this CatSystem2 game's program doesn't hold its passphrase".into())
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |n| n.0)
}

impl Game {
    /// scene.int, and any update archives, whose scenes replace those of the same name.
    pub fn open(game: &Path) -> R<Game> {
        let key = find_key(game)?;
        let mut paths = vec![game.join("scene.int")];
        let mut updates: Vec<PathBuf> = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            name.starts_with("update") && name.ends_with(".int")
        }).collect();
        updates.sort();
        paths.extend(updates);
        let mut archives = Vec::new();
        for path in paths {
            let archive = Archive::read(original(&path)?, key)?;
            let mut scenes = Vec::new();
            for (i, item) in archive.items.iter().enumerate().filter(|(_, i)| i.name.to_lowercase().ends_with(".cst")) {
                let plain = unpack(&archive.file(item)?).map_err(|e| format!("{}: {e}", item.name))?;
                scenes.push((i, Scene::read(&plain).map_err(|e| format!("{}: {e}", item.name))?));
            }
            archives.push((path, archive, scenes));
        }
        Ok(Game { archives })
    }

    /// Each scene's lines once, when an update repeats it.
    pub fn lines(&self) -> Vec<Line> {
        let (mut out, mut seen): (Vec<Line>, HashSet<String>) = (Vec::new(), HashSet::new());
        for (_, archive, scenes) in self.archives.iter().rev() {
            for (i, s) in scenes {
                let name = stem(&archive.items[*i].name);
                if !seen.insert(name.to_lowercase()) {
                    continue;
                }
                for m in s.messages() {
                    let who = m.speaker.map(|w| decoded(&s.strings[w].1)).filter(|w| JA.is_match(w));
                    out.push((format!("{name}.{}", m.rows[0]), who, m.text));
                }
                out.extend(s.choices().into_iter().map(|(n, t)| (format!("{name}.{n}"), None, t)));
            }
        }
        out
    }

    /// How wide the game's text runs, in half-width letters: nearly all lines, as a few are exceptions.
    fn width(&self) -> usize {
        let mut widths: Vec<usize> = Vec::new();
        for (_, _, scenes) in &self.archives {
            for (_, s) in scenes {
                widths.extend(s.strings.iter().filter(|r| r.0 == MESSAGE).map(|r| text_units(&decoded(&r.1).replace("\\n", "").replace("\\@", ""))));
            }
        }
        widths.sort();
        widths.get(widths.len() * 99 / 100).copied().unwrap_or(52).max(30)
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the archives and reads
/// them back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let width = crate::fit::letters(game, g.width());
    let mut done = 0;
    for (path, archive, scenes) in &g.archives {
        let mut new = Vec::new();
        for (i, s) in scenes {
            let (changed, n) = s.with(stem(&archive.items[*i].name), width, line, name);
            if n > 0 {
                new.push((*i, packed(&changed.write())?));
                done += n;
            }
        }
        if new.is_empty() {
            continue;
        }
        let out = archive.with(&new);
        let key = find_key(game)?;
        let back = Archive::read(out.clone(), key)?;
        for (i, bytes) in &new {
            if back.file(&back.items[*i])?.len() != bytes.len() {
                return Err(format!("{} didn't read back as written", back.items[*i].name));
            }
            let plain = unpack(&back.file(&back.items[*i])?)?;
            Scene::read(&plain)?;
        }
        replace_file(path, &out)?;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engines_twister_is_the_usual_one_but_for_its_seeding() {
        let mut t = Twister::new(1);
        let first = t.next();
        assert_ne!(first, t.next());
        assert_eq!(Twister::new(1).next(), first);
    }

    #[test]
    fn a_name_comes_back_through_the_alphabet() {
        // the letter that shifts onto "s" at key 0, whose first shift is 0
        let plain = b"scene.cst";
        let raw: Vec<u8> = plain.iter().enumerate().map(|(i, &c)| match ALPHABET.iter().position(|&a| a == c) {
            Some(j) => ALPHABET[(51 - j + i) % 52],
            None => c,
        }).collect();
        assert_eq!(name_of(&raw, 0), "scene.cst");
    }

    fn scene(rows: &[(u16, &str)]) -> Scene {
        Scene { head: vec![0; 16], strings: rows.iter().map(|(k, t)| (*k, sjis(t))).collect() }
    }

    #[test]
    fn a_scene_rebuilds_and_reads_back() {
        let s = scene(&[(0x0201, ""), (NAME, "太郎"), (MESSAGE, "「こんにちは\\n"), (MESSAGE, "元気？」\\@"), (COMMAND, "wait 100")]);
        let plain = s.write();
        assert_eq!(Scene::read(&plain).unwrap().strings, s.strings);
        assert_eq!(unpack(&packed(&plain).unwrap()).unwrap(), plain);
    }

    #[test]
    fn lines_join_and_translations_keep_the_rows() {
        let s = scene(&[(NAME, "太郎"), (MESSAGE, "「[漢字/かんじ]\\n"), (MESSAGE, "です」\\@"), (MESSAGE, ""), (COMMAND, "wait 1"), (MESSAGE, "地の文")]);
        let m = s.messages();
        assert_eq!(m[0].text, "「漢字です」");
        assert_eq!(m[0].speaker, Some(0));
        assert_eq!(m[1].speaker, None);

        let (out, n) = s.with("a", 30, &|k: &str| (k == "a.1").then(|| "Some English that runs to more lines than there were rows in the scene, by far, as it goes on and on and on".to_string()), &|_: &str| Some("Taro".to_string()));
        assert_eq!(n, 2);
        assert_eq!(out.strings.len(), s.strings.len());
        assert_eq!(decoded(&out.strings[0].1), "Taro");
        assert!(decoded(&out.strings[1].1).ends_with("\\n"));
        assert!(decoded(&out.strings[3].1).contains("\\n") && decoded(&out.strings[3].1).ends_with("\\@"));
        assert_eq!(decoded(&out.strings[5].1), "地の文");
    }

    #[test]
    fn choices_keep_their_number_and_target() {
        let s = scene(&[(COMMAND, "fselect"), (COMMAND, "0 01_a はい"), (COMMAND, "1 01_b (:いいえ)"), (COMMAND, "wait 1")]);
        assert_eq!(s.choices(), [(1, "はい".to_string()), (2, "いいえ".to_string())]);
        let (out, _) = s.with("a", 40, &|k: &str| (k == "a.1").then(|| "Yes I do".to_string()), &|_: &str| None);
        assert_eq!(decoded(&out.strings[1].1), "0 01_a (:Yes I do)");
    }
}
