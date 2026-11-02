//! RPG Maker XP, VX and VX Ace keep the same data as MV, but in Ruby's Marshal format and usually in
//! an encrypted archive. Marshal is read into the JSON shape rpgmaker.rs walks, with a second tree
//! remembering where each string sits. Only strings that changed are spliced back, so everything
//! else in a file stays byte for byte, object links included.

use crate::code::{shown_strings, with_english, RUBY};
use crate::{original, replace_file, R};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const DATABASE: [&str; 11] = ["Actors", "Classes", "Skills", "Items", "Weapons", "Armors", "Enemies", "States", "System", "Troops", "CommonEvents"];

// ---------- Marshal ----------

/// Where a parsed value's strings sit, in the same shape as its JSON.
#[derive(Clone)]
enum Place {
    /// `(start of its length, start of its text, end)`
    Text(usize, usize, usize),
    List(Vec<Place>),
    Fields(HashMap<String, Place>),
    None,
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
    symbols: Vec<String>,
    objects: Vec<Option<(Value, Place)>>,
}

impl Reader<'_> {
    fn byte(&mut self) -> R<u8> {
        let c = *self.b.get(self.p).ok_or("Marshal data ends early")?;
        self.p += 1;
        Ok(c)
    }

    /// Ruby's packed integer: small ones in the byte, others after a count of bytes.
    fn int(&mut self) -> R<i64> {
        let c = self.byte()? as i8 as i64;
        Ok(match c {
            0 => 0,
            5..=127 => c - 5,
            -128..=-5 => c + 5,
            1..=4 => {
                let mut v = 0i64;
                for i in 0..c {
                    v |= (self.byte()? as i64) << (8 * i);
                }
                v
            }
            _ => {
                let mut v = -1i64;
                for i in 0..-c {
                    v &= !(0xFF << (8 * i));
                    v |= (self.byte()? as i64) << (8 * i);
                }
                v
            }
        })
    }

    fn span(&mut self) -> R<(usize, usize)> {
        let n = self.int()?;
        let (start, end) = (self.p, self.p + usize::try_from(n).map_err(|_| "bad Marshal length")?);
        if end > self.b.len() {
            return Err("Marshal data ends early".into());
        }
        self.p = end;
        Ok((start, end))
    }

    fn symbol(&mut self) -> R<String> {
        match self.byte()? {
            b':' => {
                let (s, e) = self.span()?;
                let name = String::from_utf8_lossy(&self.b[s..e]).into_owned();
                self.symbols.push(name.clone());
                Ok(name)
            }
            b';' => {
                let i = self.int()? as usize;
                self.symbols.get(i).cloned().ok_or_else(|| "bad Marshal symbol link".to_string())
            }
            b'I' => {
                let name = self.symbol()?;
                for _ in 0..self.int()? {
                    self.symbol()?;
                    self.value()?;
                }
                Ok(name)
            }
            c => Err(format!("expected a Marshal symbol, found {c:#x}")),
        }
    }

    /// Takes the next object number, for links to find. It's filled in when the object is done.
    fn reserve(&mut self) -> usize {
        self.objects.push(None);
        self.objects.len() - 1
    }

    fn pairs(&mut self) -> R<(Vec<Value>, Vec<Place>)> {
        let (mut values, mut places) = (Vec::new(), Vec::new());
        for _ in 0..self.int()? {
            self.value()?;
            let (v, p) = self.value()?;
            values.push(v);
            places.push(p);
        }
        Ok((values, places))
    }

    fn value(&mut self) -> R<(Value, Place)> {
        let at = self.p;
        let done = |slot: Option<usize>, r: &mut Self, v: (Value, Place)| {
            if let Some(i) = slot {
                r.objects[i] = Some(v.clone());
            }
            Ok(v)
        };
        let tag = self.byte()?;
        match tag {
            b'0' => Ok((Value::Null, Place::None)),
            b'T' => Ok((true.into(), Place::None)),
            b'F' => Ok((false.into(), Place::None)),
            b'i' => Ok((self.int()?.into(), Place::None)),
            b':' | b';' => {
                self.p = at;
                Ok((self.symbol()?.into(), Place::None))
            }
            b'@' => {
                let i = self.int()? as usize;
                Ok(self.objects.get(i).cloned().flatten().unwrap_or((Value::Null, Place::None)))
            }
            b'"' => {
                let slot = self.reserve();
                let head = self.p;
                let (s, e) = self.span()?;
                let text = String::from_utf8_lossy(&self.b[s..e]).into_owned();
                done(Some(slot), self, (text.into(), Place::Text(head, s, e)))
            }
            b'f' | b'l' => {
                let slot = self.reserve();
                if tag == b'l' {
                    self.byte()?;
                    let words = self.int()? as usize;
                    self.p += words * 2;
                } else {
                    self.span()?;
                }
                done(Some(slot), self, (Value::Null, Place::None))
            }
            b'I' => {
                let v = self.value()?;
                for _ in 0..self.int()? {
                    self.symbol()?;
                    self.value()?;
                }
                Ok(v)
            }
            b'[' => {
                let slot = self.reserve();
                let (mut values, mut places) = (Vec::new(), Vec::new());
                for _ in 0..self.int()? {
                    let (v, p) = self.value()?;
                    values.push(v);
                    places.push(p);
                }
                done(Some(slot), self, (values.into(), Place::List(places)))
            }
            // rows by id: MV keeps these as a list, and nothing here needs the id
            b'{' => {
                let slot = self.reserve();
                let (values, places) = self.pairs()?;
                done(Some(slot), self, (values.into(), Place::List(places)))
            }
            b'}' => {
                let slot = self.reserve();
                let (values, places) = self.pairs()?;
                self.value()?;
                done(Some(slot), self, (values.into(), Place::List(places)))
            }
            b'o' => {
                let slot = self.reserve();
                self.symbol()?;
                let (mut fields, mut places) = (Map::new(), HashMap::new());
                for _ in 0..self.int()? {
                    let name = camel(&self.symbol()?);
                    let (v, p) = self.value()?;
                    fields.insert(name.clone(), v);
                    places.insert(name, p);
                }
                done(Some(slot), self, (fields.into(), Place::Fields(places)))
            }
            b'u' => {
                self.symbol()?;
                let slot = self.reserve();
                self.span()?;
                done(Some(slot), self, (Value::Null, Place::None))
            }
            c => Err(format!("Marshal type {c:#x} isn't supported")),
        }
    }
}

/// `@display_name` as MV spells it, `displayName`.
fn camel(field: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in field.trim_start_matches('@').chars() {
        if c == '_' {
            upper = true;
        } else {
            out.push(if upper { c.to_ascii_uppercase() } else { c });
            upper = false;
        }
    }
    out
}

fn parse(bytes: &[u8]) -> R<(Value, Place)> {
    if bytes.get(..2) != Some(&[4, 8]) {
        return Err("not Marshal 4.8 data".into());
    }
    Reader { b: bytes, p: 2, symbols: Vec::new(), objects: Vec::new() }.value()
}

fn packed(n: usize) -> Vec<u8> {
    match n {
        0 => vec![0],
        1..=122 => vec![n as u8 + 5],
        _ => {
            let raw = (n as u32).to_le_bytes();
            let used = raw.iter().rposition(|&c| c != 0).unwrap() + 1;
            [&[used as u8], &raw[..used]].concat()
        }
    }
}

/// Strings in `v` that differ from where `place` found them, as `(start, end, new)`.
fn changed(v: &Value, place: &Place, b: &[u8], out: &mut Vec<(usize, usize, String)>) {
    match (v, place) {
        (Value::String(s), &Place::Text(head, start, end)) if *s != String::from_utf8_lossy(&b[start..end]) => out.push((head, end, s.clone())),
        (Value::Array(values), Place::List(places)) => values.iter().zip(places).for_each(|(v, p)| changed(v, p, b, out)),
        (Value::Object(fields), Place::Fields(places)) => places.iter().for_each(|(k, p)| {
            if let Some(v) = fields.get(k) {
                changed(v, p, b, out);
            }
        }),
        _ => {}
    }
}

// ---------- the Ruby scripts ----------

// Lines that plainly show text: a draw call, a command list, a string given a name.
static SHOWN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"draw_text|set_text|add_command|Window_Command\.new|^\s*@?[A-Za-z_]\w*\s*=\s*\[?\s*["']|^\s*["'][^"']*["']\s*,?\s*\]?\s*$"#).unwrap()
});
static NOT_SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"==|!=|=~|=>|\bwhen\b|\bcase\b|include\?|Cache|Audio|Bitmap|\bload|require|\.index|/|\w\[["']"#).unwrap());
/// `Data/Scripts`: rows of `[id, name, packed source]`.
struct Code {
    source: Source,
    /// Where each script's packed source sits, as `Place::Text` has it.
    spots: Vec<(usize, usize, usize)>,
    scripts: Vec<String>,
    shown: Vec<(usize, usize, usize)>,
    edited: HashMap<usize, String>,
}

impl Code {
    /// None if these aren't scripts as the makers write them: then they stay as they are.
    fn read(entry: usize, path: PathBuf, bytes: Vec<u8>) -> Option<Code> {
        let (_, Place::List(rows)) = parse(&bytes).ok()? else { return None };
        let (mut spots, mut scripts) = (Vec::new(), Vec::new());
        for row in rows {
            let Place::List(cells) = row else { return None };
            let &Place::Text(head, start, end) = cells.get(2)? else { return None };
            let mut ruby = String::new();
            ZlibDecoder::new(bytes.get(start..end)?).read_to_string(&mut ruby).ok()?;
            spots.push((head, start, end));
            scripts.push(ruby);
        }
        let shown = shown_strings(&scripts, &RUBY, &SHOWN, &NOT_SHOWN);
        Some(Code { source: Source { entry, path, bytes, place: Place::None }, spots, scripts, shown, edited: HashMap::new() })
    }

    /// The file with the edited scripts packed again.
    fn rebuilt(&self) -> R<Vec<u8>> {
        let (b, mut out, mut last) = (&self.source.bytes, Vec::new(), 0);
        for (n, &(head, _, end)) in self.spots.iter().enumerate() {
            let Some(ruby) = self.edited.get(&n) else { continue };
            let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
            let packed_ruby = z.write_all(ruby.as_bytes()).and_then(|_| z.finish()).map_err(|e| e.to_string())?;
            out.extend(&b[last..head]);
            out.extend(packed(packed_ruby.len()));
            out.extend(packed_ruby);
            last = end;
        }
        out.extend(&b[last..]);
        parse(&out)?;
        Ok(out)
    }
}

// ---------- the archives ----------

fn next_key(k: u32) -> u32 {
    k.wrapping_mul(7).wrapping_add(3)
}

/// File contents, and the same again to encrypt.
fn roll(data: &mut [u8], mut key: u32) {
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= (key >> (8 * (i % 4))) as u8;
        if i % 4 == 3 {
            key = next_key(key);
        }
    }
}

struct Entry {
    name: String,
    /// Where its offset and size fields sit, in archives with a table.
    row: usize,
    key: u32,
    start: usize,
    size: usize,
}

struct Archive {
    bytes: Vec<u8>,
    version: u8,
    /// What the table's numbers are xored with, in archives with a table.
    table_key: u32,
    entries: Vec<Entry>,
}

fn u32_at(b: &[u8], p: usize) -> R<u32> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("the archive ends early")?.try_into().unwrap()))
}

const KEY_V1: u32 = 0xDEAD_CAFE;

impl Archive {
    fn read(bytes: Vec<u8>) -> R<Archive> {
        let version = *bytes.get(7).ok_or("the archive is too short")?;
        if !bytes.starts_with(b"RGSSAD\0") {
            return Err("not an RGSS archive".into());
        }
        let (mut entries, mut table_key) = (Vec::new(), 0);
        match version {
            1 => {
                let (mut key, mut p) = (KEY_V1, 8);
                while p < bytes.len() {
                    let n = (u32_at(&bytes, p)? ^ key) as usize;
                    (key, p) = (next_key(key), p + 4);
                    let mut name = Vec::with_capacity(n);
                    for &c in bytes.get(p..p + n).ok_or("the archive ends early")? {
                        name.push(c ^ key as u8);
                        key = next_key(key);
                    }
                    p += n;
                    let size = (u32_at(&bytes, p)? ^ key) as usize;
                    (key, p) = (next_key(key), p + 4);
                    entries.push(Entry { name: String::from_utf8_lossy(&name).replace('\\', "/"), row: 0, key, start: p, size });
                    p += size;
                }
            }
            3 => {
                table_key = u32_at(&bytes, 8)?.wrapping_mul(9).wrapping_add(3);
                let mut p = 12;
                loop {
                    let start = (u32_at(&bytes, p)? ^ table_key) as usize;
                    if start == 0 {
                        break;
                    }
                    let (size, key, n) = (u32_at(&bytes, p + 4)? ^ table_key, u32_at(&bytes, p + 8)? ^ table_key, (u32_at(&bytes, p + 12)? ^ table_key) as usize);
                    let name: Vec<u8> = bytes.get(p + 16..p + 16 + n).ok_or("the archive ends early")?.iter().enumerate().map(|(i, c)| c ^ (table_key >> (8 * (i % 4))) as u8).collect();
                    entries.push(Entry { name: String::from_utf8_lossy(&name).replace('\\', "/"), row: p, key, start, size: size as usize });
                    p += 16 + n;
                }
            }
            v => return Err(format!("RGSS archive version {v} isn't supported")),
        }
        if entries.iter().any(|e| e.start + e.size > bytes.len()) {
            return Err("an RGSS archive entry runs past the end of the file".into());
        }
        Ok(Archive { bytes, version, table_key, entries })
    }

    fn file(&self, e: &Entry) -> Vec<u8> {
        let mut data = self.bytes[e.start..e.start + e.size].to_vec();
        roll(&mut data, e.key);
        data
    }

    /// The archive with new contents for some entries.
    fn with(&self, new: &HashMap<usize, Vec<u8>>) -> Vec<u8> {
        if self.version == 1 {
            // a chain: each entry's sizes depend on nothing but position, so it's rewritten in order
            let mut out = self.bytes[..8].to_vec();
            let mut key = KEY_V1;
            for (i, e) in self.entries.iter().enumerate() {
                let raw: Vec<u8> = e.name.replace('/', "\\").into_bytes();
                out.extend((raw.len() as u32 ^ key).to_le_bytes());
                key = next_key(key);
                for c in &raw {
                    out.push(c ^ key as u8);
                    key = next_key(key);
                }
                let size = new.get(&i).map_or(e.size, Vec::len);
                out.extend((size as u32 ^ key).to_le_bytes());
                key = next_key(key);
                match new.get(&i) {
                    Some(plain) => {
                        let mut data = plain.clone();
                        roll(&mut data, key);
                        out.extend(data);
                    }
                    None => out.extend(&self.bytes[e.start..e.start + e.size]),
                }
            }
            return out;
        }
        let mut out = self.bytes.clone();
        for (i, plain) in new {
            let e = &self.entries[*i];
            let mut data = plain.clone();
            roll(&mut data, e.key);
            let at = out.len() as u32 ^ self.table_key;
            out[e.row..e.row + 4].copy_from_slice(&at.to_le_bytes());
            out[e.row + 4..e.row + 8].copy_from_slice(&(data.len() as u32 ^ self.table_key).to_le_bytes());
            out.extend(data);
        }
        out
    }
}

// ---------- the game ----------

#[derive(PartialEq)]
enum Maker {
    Xp,
    Vx,
    Ace,
}

struct Source {
    /// Its number in the archive, if it's in one.
    entry: usize,
    path: PathBuf,
    bytes: Vec<u8>,
    place: Place,
}

pub struct Game {
    /// In the shape rpgmaker.rs walks.
    pub files: Vec<(String, Value)>,
    sources: Vec<Source>,
    /// `(path, archive)`
    archive: Option<(PathBuf, Archive)>,
    maker: Maker,
    code: Option<Code>,
}

/// A name in `Data/` that's worth reading: the database, or a map.
fn wanted(stem: &str) -> bool {
    DATABASE.contains(&stem) || stem.strip_prefix("Map").is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

fn maker_of(name: &str) -> Option<Maker> {
    let name = name.to_lowercase();
    match name.rsplit('.').next()? {
        "rxdata" => Some(Maker::Xp),
        "rvdata" => Some(Maker::Vx),
        "rvdata2" => Some(Maker::Ace),
        _ => None,
    }
}

impl Game {
    /// None when the game has no archive and no Ruby data: then it isn't one of these makers.
    pub fn open(game: &Path) -> R<Option<Game>> {
        let list: Vec<PathBuf> = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
        let archive = list.iter().find(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            n.ends_with(".rgssad") || n.ends_with(".rgss2a") || n.ends_with(".rgss3a")
        });
        let (mut raw, mut maker, mut code) = (Vec::new(), None, None);
        let archive = match archive {
            Some(path) => {
                let a = Archive::read(original(path)?)?;
                for (i, e) in a.entries.iter().enumerate() {
                    let Some((stem, _)) = e.name.strip_prefix("Data/").and_then(|n| n.rsplit_once('.')) else { continue };
                    if stem == "Scripts" && maker_of(&e.name).is_some() {
                        code = Code::read(i, path.clone(), a.file(e));
                    }
                    if wanted(stem) && maker_of(&e.name).is_some() {
                        maker = maker_of(&e.name);
                        raw.push((stem.to_string(), i, path.clone(), a.file(e)));
                    }
                }
                Some((path.clone(), a))
            }
            None => {
                for path in fs::read_dir(game.join("Data")).into_iter().flatten().flatten().map(|e| e.path()) {
                    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let Some((stem, _)) = name.rsplit_once('.') else { continue };
                    if stem == "Scripts" && maker_of(&name).is_some() {
                        code = Code::read(0, path.clone(), original(&path)?);
                    }
                    if wanted(stem) && maker_of(&name).is_some() && !name.ends_with(".vntl-orig") {
                        maker = maker_of(&name);
                        raw.push((stem.to_string(), 0, path.clone(), original(&path)?));
                    }
                }
                None
            }
        };
        let Some(maker) = maker else { return Ok(None) };
        raw.sort_by(|a, b| a.0.cmp(&b.0));
        let (mut files, mut sources) = (Vec::new(), Vec::new());
        for (stem, entry, path, bytes) in raw {
            let (value, place) = parse(&bytes).map_err(|e| format!("{stem}: {e}"))?;
            files.push((format!("{stem}.json"), value));
            sources.push(Source { entry, path, bytes, place });
        }
        Ok(Some(Game { files, sources, archive, maker, code }))
    }

    /// The Japanese the scripts show, as they write it.
    pub fn script_texts(&self) -> Vec<String> {
        let shown = self.code.iter().flat_map(|c| c.shown.iter().map(|&(n, start, end)| c.scripts[n][start..end].to_string()));
        shown.collect()
    }

    /// Puts English in the scripts, for the next `save`. Returns how many strings took it.
    pub fn translate_scripts(&mut self, lookup: &dyn Fn(&str) -> Option<String>) -> usize {
        let Some(code) = &mut self.code else { return 0 };
        let mut done = 0;
        for (n, ruby) in code.scripts.iter().enumerate() {
            let spots: Vec<(usize, usize)> = code.shown.iter().filter(|s| s.0 == n).map(|s| (s.1, s.2)).collect();
            let (new, count) = with_english(ruby, &spots, &|_, old| lookup(old));
            if count > 0 {
                code.edited.insert(n, new);
                done += count;
            }
        }
        done
    }

    /// Half-width letters across a screen-wide window, and across the message box: the screen less
    /// padding, over half the font's size. XP's box is narrower than its screen.
    pub fn widths(&self) -> (usize, usize) {
        match self.maker {
            Maker::Xp => (55, 40),
            Maker::Vx => (51, 51),
            Maker::Ace => (42, 42),
        }
    }

    /// Puts what changed in `files` back. Returns how many files changed.
    pub fn save(&self) -> R<usize> {
        let mut new = Vec::new();
        for ((_, value), s) in self.files.iter().zip(&self.sources) {
            let mut edits = Vec::new();
            changed(value, &s.place, &s.bytes, &mut edits);
            if edits.is_empty() {
                continue;
            }
            edits.sort();
            edits.dedup_by_key(|e| e.0); // a string linked from two places
            let (mut out, mut last) = (Vec::with_capacity(s.bytes.len()), 0);
            for (start, end, text) in &edits {
                // VX ends a row on \0 and draws \n as a box; the others take \n
                let text = if self.maker == Maker::Vx { text.replace('\n', "\0") } else { text.clone() };
                out.extend(&s.bytes[last..*start]);
                out.extend(packed(text.len()));
                out.extend(text.as_bytes());
                last = *end;
            }
            out.extend(&s.bytes[last..]);
            parse(&out)?;
            new.push((s, out));
        }
        if let Some(code) = self.code.as_ref().filter(|c| !c.edited.is_empty()) {
            new.push((&code.source, code.rebuilt()?));
        }
        match &self.archive {
            None => new.iter().try_for_each(|(s, out)| replace_file(&s.path, out))?,
            Some((path, a)) => {
                let by_entry: HashMap<usize, Vec<u8>> = new.iter().map(|(s, out)| (s.entry, out.clone())).collect();
                replace_file(path, &a.with(&by_entry))?;
                let back = Archive::read(fs::read(path).map_err(|e| e.to_string())?)?;
                for (i, bytes) in &by_entry {
                    if back.file(&back.entries[*i]) != *bytes {
                        return Err(format!("{} didn't read back as written", back.entries[*i].name));
                    }
                }
            }
        }
        Ok(new.len())
    }
}

/// Whether a game's folder has this maker's archive or data, for callers that only need to know.
pub fn present(game: &Path) -> bool {
    fs::read_dir(game).into_iter().flatten().flatten().any(|e| e.file_name().to_string_lossy().to_lowercase().contains(".rgss"))
        || fs::read_dir(game.join("Data")).into_iter().flatten().flatten().any(|e| maker_of(&e.file_name().to_string_lossy()).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    // a small Marshal writer for fixtures: objects keep their order, so a link can be tested too
    fn symbol(out: &mut Vec<u8>, seen: &mut Vec<String>, name: &str) {
        match seen.iter().position(|s| s == name) {
            Some(i) => {
                out.push(b';');
                out.extend(packed(i));
            }
            None => {
                seen.push(name.into());
                out.push(b':');
                out.extend(packed(name.len()));
                out.extend(name.as_bytes());
            }
        }
    }

    fn string(out: &mut Vec<u8>, t: &str) {
        out.push(b'"');
        out.extend(packed(t.len()));
        out.extend(t.as_bytes());
    }

    fn command(out: &mut Vec<u8>, seen: &mut Vec<String>, code: u8, text: Option<&str>) {
        out.push(b'o');
        symbol(out, seen, "RPG::EventCommand");
        out.push(7); // 2 fields
        symbol(out, seen, "@code");
        out.extend([b'i', code + 5]);
        symbol(out, seen, "@parameters");
        out.extend([b'[', if text.is_some() { 6 } else { 0 }]);
        if let Some(t) = text {
            string(out, t);
        }
    }

    /// `{ display_name, events: { 1: event } }`, one page, a message of two lines.
    fn small_map() -> Vec<u8> {
        let (mut b, mut seen) = (vec![4, 8], Vec::new());
        b.push(b'o');
        symbol(&mut b, &mut seen, "RPG::Map");
        b.push(7);
        symbol(&mut b, &mut seen, "@display_name");
        string(&mut b, "村");
        symbol(&mut b, &mut seen, "@events");
        b.extend([b'{', 6, b'i', 6]);
        b.push(b'o');
        symbol(&mut b, &mut seen, "RPG::Event");
        b.push(6);
        symbol(&mut b, &mut seen, "@pages");
        b.extend([b'[', 6]);
        b.push(b'o');
        symbol(&mut b, &mut seen, "RPG::Event::Page");
        b.push(7);
        symbol(&mut b, &mut seen, "@list");
        b.extend([b'[', 7]);
        command(&mut b, &mut seen, 41, Some("こんにちは"));
        command(&mut b, &mut seen, 41, Some("元気です"));
        symbol(&mut b, &mut seen, "@again");
        b.extend([b'@', 17]); // object 12, the second line
        b
    }

    fn v3(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let seed = 0x1234u32;
        let key = seed.wrapping_mul(9).wrapping_add(3);
        let table: usize = files.iter().map(|(n, _)| 16 + n.len()).sum::<usize>() + 16;
        let mut out = b"RGSSAD\0\x03".to_vec();
        out.extend(seed.to_le_bytes());
        let mut at = 12 + table;
        for (i, (name, b)) in files.iter().enumerate() {
            out.extend((at as u32 ^ key).to_le_bytes());
            out.extend((b.len() as u32 ^ key).to_le_bytes());
            out.extend(((i as u32 + 7) ^ key).to_le_bytes());
            out.extend((name.len() as u32 ^ key).to_le_bytes());
            out.extend(name.bytes().enumerate().map(|(k, c)| c ^ (key >> (8 * (k % 4))) as u8));
            at += b.len();
        }
        out.extend(key.to_le_bytes());
        out.extend([0; 12]);
        for (i, (_, b)) in files.iter().enumerate() {
            let mut data = b.clone();
            roll(&mut data, i as u32 + 7);
            out.extend(data);
        }
        out
    }

    fn v1(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = b"RGSSAD\0\x01".to_vec();
        let mut key = KEY_V1;
        for (name, b) in files {
            out.extend((name.len() as u32 ^ key).to_le_bytes());
            key = next_key(key);
            for c in name.bytes() {
                out.push(c ^ key as u8);
                key = next_key(key);
            }
            out.extend((b.len() as u32 ^ key).to_le_bytes());
            key = next_key(key);
            let mut data = b.clone();
            roll(&mut data, key);
            out.extend(data);
        }
        out
    }

    fn game(archive: &str, bytes: Vec<u8>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-rgss-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(archive), bytes).unwrap();
        dir
    }

    fn list(v: &Value) -> Vec<&str> {
        v["events"][0]["pages"][0]["list"].as_array().unwrap().iter().map(|c| c["parameters"][0].as_str().unwrap()).collect()
    }

    #[test]
    fn packed_numbers_read_back() {
        for n in [0usize, 1, 122, 123, 255, 256, 70_000] {
            let b = packed(n);
            let mut r = Reader { b: &b, p: 0, symbols: Vec::new(), objects: Vec::new() };
            assert_eq!(r.int().unwrap(), n as i64);
        }
        let mut r = Reader { b: &[0xFE, 0x10, 0xFF], p: 0, symbols: Vec::new(), objects: Vec::new() };
        assert_eq!(r.int().unwrap(), -240);
    }

    #[test]
    fn marshal_reads_in_mv_shape() {
        let (v, _) = parse(&small_map()).unwrap();
        assert_eq!(v["displayName"], "村");
        assert_eq!(list(&v), ["こんにちは", "元気です"]);
    }

    #[test]
    fn archives_read_and_a_changed_string_grows_in_place() {
        for (name, archive) in [("Game.rgssad", v1(&[("Data\\Map001.rxdata", small_map()), ("Graphics\\a.png", b"art".to_vec())])), ("Game.rgss3a", v3(&[("Data\\Map001.rvdata2", small_map()), ("Graphics\\a.png", b"art".to_vec())]))] {
            let dir = game(name, archive);
            let mut g = Game::open(&dir).unwrap().unwrap();
            let v = &mut g.files[0].1;
            v["displayName"] = "The village".into();
            v["events"][0]["pages"][0]["list"][1]["parameters"][0] = "A much longer second line of English".into();
            assert_eq!(g.save().unwrap(), 1);

            let path = dir.join(name);
            let written = fs::read(&path).unwrap();
            let a = Archive::read(written.clone()).unwrap();
            assert_eq!(a.file(&a.entries[1]), b"art");
            let (v, _) = parse(&a.file(&a.entries[0])).unwrap();
            assert_eq!(v["displayName"], "The village");
            assert_eq!(list(&v), ["こんにちは", "A much longer second line of English"]);
            assert_eq!(v["events"][0]["pages"][0]["again"], "A much longer second line of English", "a link still finds its string");

            // a second run starts from the game's own file and gives the same bytes
            let mut g = Game::open(&dir).unwrap().unwrap();
            assert_eq!(list(&g.files[0].1), ["こんにちは", "元気です"]);
            let v = &mut g.files[0].1;
            v["displayName"] = "The village".into();
            v["events"][0]["pages"][0]["list"][1]["parameters"][0] = "A much longer second line of English".into();
            g.save().unwrap();
            assert_eq!(fs::read(&path).unwrap(), written);
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn loose_data_is_used_when_there_is_no_archive() {
        let dir = game("Game.ini", b"[Game]".to_vec());
        fs::create_dir_all(dir.join("Data")).unwrap();
        fs::write(dir.join("Data/Map001.rvdata"), small_map()).unwrap();
        fs::write(dir.join("Data/Scripts.rvdata"), b"code stays").unwrap();
        let mut g = Game::open(&dir).unwrap().unwrap();
        assert_eq!(g.files.len(), 1);
        g.files[0].1["events"][0]["pages"][0]["list"][0]["parameters"][0] = "One\nTwo".into();
        assert_eq!(g.save().unwrap(), 1);
        let (v, _) = parse(&fs::read(dir.join("Data/Map001.rvdata")).unwrap()).unwrap();
        assert_eq!(list(&v)[0], "One\0Two", "VX ends a row with a NUL");
        let _ = fs::remove_dir_all(dir);
    }

    const SCRIPT: &str = r##"# 見出し "コメント"
class Window_Test
  Buy = "購入する"
  def refresh
    self.contents.draw_text(0, 0, 100, 32, "所持金")
    s1 = '開始'
    if @kind == "火"
    @kind = "火"
    Audio.se_play("Audio/SE/決定")
    text = "#{@name}の番"
    note = "%sが勝った！"
    line = "二行\n目"
=begin
    s2 = "書かれない"
=end
  end
end
"##;

    fn scripts_file(sources: &[&str]) -> Vec<u8> {
        let mut out = vec![4, 8, b'['];
        out.extend(packed(sources.len()));
        for (i, ruby) in sources.iter().enumerate() {
            out.push(b'[');
            out.extend(packed(3));
            out.push(b'i');
            out.extend(packed(i));
            string(&mut out, "Script");
            let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
            z.write_all(ruby.as_bytes()).unwrap();
            let body = z.finish().unwrap();
            out.push(b'"');
            out.extend(packed(body.len()));
            out.extend(body);
        }
        out
    }

    fn with_scripts(sources: &[&str]) -> PathBuf {
        let dir = game("Game.ini", b"[Game]".to_vec());
        fs::create_dir_all(dir.join("Data")).unwrap();
        fs::write(dir.join("Data/Map001.rvdata2"), small_map()).unwrap();
        fs::write(dir.join("Data/Scripts.rvdata2"), scripts_file(sources)).unwrap();
        dir
    }

    #[test]
    fn only_strings_a_script_plainly_shows_are_offered() {
        let dir = with_scripts(&[SCRIPT]);
        let g = Game::open(&dir).unwrap().unwrap();
        assert_eq!(g.script_texts(), ["購入する", "所持金", "開始", "%sが勝った！"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_string_used_as_a_key_in_any_script_stays_in_all_of_them() {
        let dir = with_scripts(&["s1 = \"魔法\"\n", "if name == \"魔法\"\n"]);
        assert!(Game::open(&dir).unwrap().unwrap().script_texts().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn english_goes_into_the_scripts_quoted_and_a_bad_reply_stays_japanese() {
        let dir = with_scripts(&[SCRIPT]);
        let mut g = Game::open(&dir).unwrap().unwrap();
        let done = g.translate_scripts(&|jp| match jp {
            "購入する" => Some("Say \"buy\"".to_string()),
            "所持金" => Some("Gold #1".to_string()),
            "開始" => Some("Don't".to_string()),
            _ => Some("won".to_string()),
        });
        assert_eq!(done, 2, "a stray # and a lost %s cost their own strings only");
        g.save().unwrap();
        let again = Game::open(&dir).unwrap();
        assert_eq!(fs::read(dir.join("Data/Scripts.rvdata2.vntl-orig")).unwrap(), scripts_file(&[SCRIPT]));
        let (v, place) = parse(&fs::read(dir.join("Data/Scripts.rvdata2")).unwrap()).unwrap();
        let Place::List(rows) = place else { panic!() };
        let Place::List(cells) = &rows[0] else { panic!() };
        let &Place::Text(_, start, end) = &cells[2] else { panic!() };
        let bytes = fs::read(dir.join("Data/Scripts.rvdata2")).unwrap();
        let mut ruby = String::new();
        ZlibDecoder::new(&bytes[start..end]).read_to_string(&mut ruby).unwrap();
        assert!(ruby.contains("Buy = \"Say 'buy'\""), "{ruby}");
        assert!(ruby.contains("s1 = 'Don\\'t'"), "{ruby}");
        assert!(ruby.contains("\"所持金\"") && ruby.contains("\"%sが勝った！\""), "{ruby}");
        assert!(ruby.contains("Audio/SE/決定"), "{ruby}");
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(again.unwrap().script_texts().len(), 4, "the game's own scripts are read again");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scripts_that_are_not_in_the_makers_shape_are_left_alone() {
        let dir = with_scripts(&[]);
        fs::write(dir.join("Data/Scripts.rvdata2"), b"not marshal").unwrap();
        let mut g = Game::open(&dir).unwrap().unwrap();
        assert!(g.script_texts().is_empty());
        assert_eq!(g.translate_scripts(&|_| Some("x".into())), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_game_without_ruby_data_is_not_this_engine() {
        let dir = game("Game.exe", b"x".to_vec());
        assert!(Game::open(&dir).unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
