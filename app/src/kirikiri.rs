//! KiriKiri engine (*.xp3, KAG scripts). Scripts are plain text, but each game names its own tags, so
//! text is found by shape: what sits between tags, outside code blocks, with Japanese in it. Rows
//! broken by hand become one message. New files go on the end of the archive and a new index after
//! them, which leaves every other file where it was.

use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, text_units, wrap_words, JA};
use crate::yuris::adler32;
use crate::{backup, extend_file, R};
use crate::code::{shown_strings, with_english, TJS};
use encoding_rs::SHIFT_JIS;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const MAGIC: &[u8] = b"XP3\r\n \n\x1a\x8bg\x01";
const HEAD: usize = 40;
/// shortcut: a guess for games with too few rows broken by hand; change it if lines break inside words.
const LINE_LETTERS: usize = 56;
/// How the index points at itself in newer archives: a marker block just past the header.
const CUSHION: u8 = 0x80;
const PACKED: u32 = 7;

/// Tags that sit inside a sentence and only change how it appears; they go when it's rewritten.
const SOFT: &[&str] = &["r", "rt", "l", "lr", "w", "wait", "ruby", "/ruby"];
/// Tags that stand for part of the sentence, so they stay in it for the model to place.
const INLINE: &[&str] = &["emb"];
/// Fewer rows broken by hand than this say nothing about the box.
const ENOUGH_ROWS: usize = 20;

pub static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]]*\]").unwrap());
static SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\b(text\d*|hint|jname|title|message|data)=("[^"]*"|'[^']*'|[^\s\]"']+)"#).unwrap());

fn u32_at(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

// ---------- the archive ----------

struct Entry {
    name: String,
    /// Its `File` chunk in the index.
    chunk: Range<usize>,
    /// `(packed, offset, stored size)`
    segments: Vec<(bool, u64, u64)>,
}

/// Inner chunks: four letters, a u64 size, then that many bytes.
fn chunks(b: &[u8]) -> Vec<(&[u8], Range<usize>)> {
    let (mut out, mut p) = (Vec::new(), 0);
    while p + 12 <= b.len() {
        let end = (p + 12).saturating_add(u64_at(b, p + 4) as usize);
        if end > b.len() {
            break;
        }
        out.push((&b[p..p + 4], p + 12..end));
        p = end;
    }
    out
}

fn entries(index: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    for (tag, body) in chunks(index) {
        if tag != b"File" {
            continue;
        }
        let (start, inner) = (body.start, &index[body.clone()]);
        let (mut name, mut segments) = (String::new(), Vec::new());
        for (tag, r) in chunks(inner) {
            let v = &inner[r];
            if tag == b"info" && v.len() >= 22 {
                let units: Vec<u16> = v[22..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take(u16::from_le_bytes([v[20], v[21]]) as usize).collect();
                name = String::from_utf16_lossy(&units);
            } else if tag == b"segm" {
                segments = v.as_chunks::<28>().0.iter().map(|s| (u32_at(s, 0) & PACKED == 1, u64_at(s, 4), u64_at(s, 20))).collect();
            }
        }
        out.push(Entry { name, chunk: start - 12..body.end, segments });
    }
    out
}

/// A `File` chunk describing `bytes`, stored whole at `at`.
fn file_chunk(chunk: &[u8], bytes: &[u8], at: u64) -> Vec<u8> {
    let size = (bytes.len() as u64).to_le_bytes();
    let mut body = Vec::new();
    for (tag, r) in chunks(&chunk[12..]) {
        let mut v = chunk[12..][r].to_vec();
        match tag {
            b"info" => {
                v[4..12].copy_from_slice(&size);
                v[12..20].copy_from_slice(&size);
            }
            b"segm" => {
                v = 0u32.to_le_bytes().to_vec();
                v.extend(at.to_le_bytes());
                v.extend(size);
                v.extend(size);
            }
            b"adlr" => v = adler32(bytes).to_le_bytes().to_vec(),
            _ => {}
        }
        body.extend(tag);
        body.extend((v.len() as u64).to_le_bytes());
        body.extend(v);
    }
    let mut out = b"File".to_vec();
    out.extend((body.len() as u64).to_le_bytes());
    out.extend(body);
    out
}

struct Archive {
    path: PathBuf,
    len: u64,
    head: Vec<u8>,
    /// Where in the head the index's offset is kept.
    pointer: usize,
    index: Vec<u8>,
    entries: Vec<Entry>,
}

impl Archive {
    /// None when the file isn't an XP3 archive. Index and offsets come from the game's own file.
    fn open(path: &Path) -> R<Option<Archive>> {
        let kept = backup(path);
        Archive::read(if kept.exists() { &kept } else { path }, path)
    }

    fn read(from: &Path, path: &Path) -> R<Option<Archive>> {
        let err = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut f = fs::File::open(from).map_err(err)?;
        let len = f.metadata().map_err(err)?.len();
        let mut head = vec![0; HEAD];
        if len < HEAD as u64 || f.read_exact(&mut head).is_err() || !head.starts_with(MAGIC) {
            return Ok(None);
        }
        let mut pointer = MAGIC.len();
        let mut at = u64_at(&head, pointer);
        let mut top = [0; 17];
        let mut read = |at: u64, top: &mut [u8; 17]| f.seek(SeekFrom::Start(at)).and_then(|_| f.read_exact(top));
        read(at, &mut top).map_err(err)?;
        if top[0] == CUSHION {
            pointer = at as usize + 9;
            if pointer + 8 > HEAD {
                return Err(format!("{}: this archive's header isn't one we know", path.display()));
            }
            at = u64_at(&head, pointer);
            read(at, &mut top).map_err(err)?;
        }
        let packed = top[0] & PACKED as u8 == 1;
        let stored = u64_at(&top, 1);
        let start = at + if packed { 17 } else { 9 };
        if start + stored > len {
            return Err(format!("{}: the index runs past the end of the file", path.display()));
        }
        f.seek(SeekFrom::Start(start)).map_err(err)?;
        let mut index = vec![0; stored as usize];
        f.read_exact(&mut index).map_err(err)?;
        if packed {
            let mut plain = Vec::new();
            ZlibDecoder::new(&index[..]).read_to_end(&mut plain).map_err(err)?;
            index = plain;
        }
        let entries = entries(&index);
        Ok(Some(Archive { path: path.to_path_buf(), len, head, pointer, index, entries }))
    }

    fn file(&self, f: &mut fs::File, e: &Entry) -> R<Vec<u8>> {
        let mut out = Vec::new();
        for &(packed, at, size) in &e.segments {
            let mut b = vec![0; size as usize];
            f.seek(SeekFrom::Start(at)).and_then(|_| f.read_exact(&mut b)).map_err(|err| format!("{}: {err}", e.name))?;
            if packed {
                ZlibDecoder::new(&b[..]).read_to_end(&mut out).map_err(|err| format!("{}: {err}", e.name))?;
            } else {
                out.extend(b);
            }
        }
        Ok(out)
    }

    /// New contents for some entries go on the end, then a new index.
    fn replace(&self, new: &[(usize, Vec<u8>)]) -> R<()> {
        let mut tail = Vec::new();
        let mut order: Vec<_> = new.iter().collect();
        order.sort_by_key(|(i, _)| self.entries[*i].chunk.start);
        let (mut index, mut copied) = (Vec::new(), 0);
        for (i, bytes) in order {
            let chunk = &self.entries[*i].chunk;
            index.extend(&self.index[copied..chunk.start]);
            index.extend(file_chunk(&self.index[chunk.clone()], bytes, self.len + tail.len() as u64));
            copied = chunk.end;
            tail.extend(bytes);
        }
        index.extend(&self.index[copied..]);
        let mut z = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&index).map_err(|e| e.to_string())?;
        let packed = z.finish().map_err(|e| e.to_string())?;
        let mut head = self.head.clone();
        head[self.pointer..self.pointer + 8].copy_from_slice(&(self.len + tail.len() as u64).to_le_bytes());
        tail.push(1);
        tail.extend((packed.len() as u64).to_le_bytes());
        tail.extend((index.len() as u64).to_le_bytes());
        tail.extend(packed);
        extend_file(&self.path, self.len, &self.head, &head, &tail)
    }
}

/// Every archive in the game, in the order the engine loads them: a later patch replaces earlier files.
fn archives(game: &Path) -> R<Vec<Archive>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xp3"))).collect();
    let rank = |p: &PathBuf| {
        let stem = p.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
        stem.strip_prefix("patch").map_or(0, |n| n.parse::<u32>().unwrap_or(1) + 1)
    };
    paths.sort_by_key(|p| (rank(p), p.clone()));
    let mut out = Vec::new();
    for p in paths {
        out.extend(Archive::open(&p)?);
    }
    Ok(out)
}

// ---------- the scripts ----------

/// A line as stored, line ending included, and as text without its ending.
struct Row {
    raw: Vec<u8>,
    text: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Coding {
    Sjis,
    Utf8,
    Utf16,
}

fn rows(b: &[u8]) -> (Vec<Row>, Coding) {
    if let (Ok(text), false) = (std::str::from_utf8(b), b.is_ascii()) {
        let lines = text.split_inclusive('\n').map(|l| Row { raw: l.as_bytes().to_vec(), text: l.trim_end_matches(['\r', '\n']).trim_start_matches('\u{feff}').to_string() });
        return (lines.collect(), Coding::Utf8);
    }
    if b.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = b[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let text = String::from_utf16_lossy(&units);
        let mut lines: Vec<Row> = text.split_inclusive('\n').map(|l| Row { raw: l.encode_utf16().flat_map(u16::to_le_bytes).collect(), text: l.trim_end_matches(['\r', '\n']).to_string() }).collect();
        if let Some(first) = lines.first_mut() {
            first.raw.splice(0..0, [0xFF, 0xFE]);
        }
        return (lines, Coding::Utf16);
    }
    let lines = b.split_inclusive(|&c| c == b'\n').map(|l| Row {
        raw: l.to_vec(),
        text: SHIFT_JIS.decode_without_bom_handling(l).0.trim_end_matches(['\r', '\n']).to_string(),
    });
    (lines.collect(), Coding::Sjis)
}

/// A line's tags and the text between them, as `(start, end, is tag)`. None when a tag runs on past
/// the line. `[[` is a written bracket.
fn pieces(line: &str) -> Option<Vec<(usize, usize, bool)>> {
    let b = line.as_bytes();
    let (mut out, mut i, mut text) = (Vec::new(), 0, 0);
    while i < b.len() {
        if b[i] != b'[' {
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&b'[') {
            i += 2;
            continue;
        }
        if text < i {
            out.push((text, i, false));
        }
        let (mut end, mut quote) = (i + 1, 0);
        loop {
            let c = *b.get(end)?;
            if quote != 0 {
                quote = if c == quote { 0 } else { quote };
            } else if c == b'"' || c == b'\'' {
                quote = c;
            } else if c == b']' {
                break;
            }
            end += 1;
        }
        out.push((i, end + 1, true));
        i = end + 1;
        text = i;
    }
    if text < b.len() {
        out.push((text, b.len(), false));
    }
    Some(out)
}

fn tag_name(tag: &str) -> &str {
    tag[1..tag.len() - 1].split_whitespace().next().unwrap_or("")
}

fn closer(open: char) -> Option<char> {
    match open {
        '「' => Some('」'),
        '『' => Some('』'),
        '（' => Some('）'),
        _ => None,
    }
}

/// One thing to translate: pieces of lines that become one text.
struct Unit {
    /// `(line, start, end)`; the first gets the translation and the rest go.
    spans: Vec<(usize, usize, usize)>,
    /// Opening and closing quotes kept outside the spans by a tag next to them.
    quotes: Vec<(usize, usize, usize)>,
    /// Where the last span ends if the text doesn't carry on in the next line: before trailing tags.
    last_text: usize,
    /// A quote before the text, waiting for its partner.
    open: Option<char>,
    body: String,
    /// A tag's value rather than something shown in the message box.
    attribute: bool,
    /// A row of a value that runs over several lines, so inside its quotes.
    in_value: bool,
    /// From a tag named after the character, as in `[太郎]`.
    who: Option<String>,
}

static MACRO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^\s*\[macro\s+name\s*=\s*"?([^\s"\]]+)"#).unwrap());
static WHO_IS_NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)(?:name|speaker|chara)\w*\s*=.*\bcond\s*=\s*"mp\.(\w+)\s*==\s*'(\w+)'"#).unwrap());
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new("'([^']*)'").unwrap());

/// What a game's speaker tags stand for: in the script `[n n=Wal]`, and in the macro behind it
/// `[eval exp="f.name = '名前'" cond="mp.n == 'Wal'"]`, by tag and attribute. A code whose name comes
/// from a variable, like a hero the player names, stands for itself. This is only for the model: the
/// plates stay Japanese, as a macro can pick a picture by how long the name is.
#[derive(Default)]
pub(crate) struct Speakers(HashMap<(String, String), HashMap<String, String>>);

impl Speakers {
    fn learn(&mut self, script: &Script) {
        let mut inside = None;
        for row in &script.lines {
            let line = row.text.as_str();
            if let Some(c) = MACRO.captures(line) {
                inside = Some(c[1].to_string());
            } else if line.contains("[endmacro]") {
                inside = None;
            } else if let (Some(tag), Some(c), false) = (&inside, WHO_IS_NAMED.captures(line), line.trim_start().starts_with(';')) {
                let name = QUOTED.captures_iter(&line[..c.get(1).unwrap().start()]).map(|q| q[1].to_string()).find(|q| JA.is_match(q));
                let codes = self.0.entry((tag.clone(), c[1].to_string())).or_default();
                codes.entry(c[2].to_string()).or_insert_with(|| name.unwrap_or_else(|| c[2].to_string()));
            }
        }
    }

    /// Who the tag starting a line stands for. A name written into the tag itself comes first.
    fn of(&self, tag: &str) -> Option<String> {
        let name = tag_name(tag);
        let inner = &tag[1..tag.len() - 1];
        let value = |attribute: &str| inner.split_whitespace().find_map(|w| w.strip_prefix(attribute)?.strip_prefix('=')).map(|v| v.trim_matches(['"', '\'']));
        let codes = self.0.iter().filter(|((t, _), _)| t == name);
        let mut known = codes.filter_map(|((_, attribute), codes)| codes.get(value(attribute)?));
        let found = known.next()?;
        Some(value("name").filter(|v| JA.is_match(v)).map_or_else(|| found.clone(), str::to_string))
    }
}

/// A script in the KAG style. Artemis writes its own in the same shape.
pub(crate) struct Script {
    name: String,
    lines: Vec<Row>,
    coding: Coding,
    units: Vec<Unit>,
    /// `(line, start, end)` of names on lines of their own.
    names: Vec<(usize, usize, usize)>,
    /// The values speaker tags give each variable a name is printed from, across the game.
    said: BTreeMap<String, BTreeSet<String>>,
}

/// `[print data="$ '【' + chara + '】'"]`: a name printed from what a speaker tag set, often at size 0
/// so only the log keeps it. The value stays Japanese, as the game compares it, so the print picks
/// the English by it.
static NAME_PRINT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\[print\s+data="\$\s*'([^'"]*)'\s*\+\s*(\w+)\s*\+\s*'([^'"]*)'"\s*\]"#).unwrap());

/// Lengths of the rows the writers broke by hand, for how wide the box is.
fn hand_rows(line: &str, parts: &[(usize, usize, bool)], out: &mut Vec<usize>) {
    let mut row = String::new();
    for &(start, end, tag) in parts {
        let piece = &line[start..end];
        if !tag {
            row.push_str(piece);
        } else if matches!(tag_name(piece), "r" | "rt" | "lr") {
            if JA.is_match(&row) {
                out.push(text_units(&row));
            }
            row.clear();
        }
    }
}

/// How English is laid out in a game's message box.
pub(crate) struct Layout {
    /// Half-width letters across.
    pub width: usize,
    pub row_break: &'static str,
    /// Rows to a box, and what ends one and starts the next; usize::MAX rows if the box scrolls or grows.
    pub rows: usize,
    pub page_break: &'static str,
}

/// How wide the box is from the rows broken by hand, or `guess` if there are too few of them.
pub(crate) fn box_width(mut rows: Vec<usize>, guess: usize) -> usize {
    rows.sort();
    if rows.len() < ENOUGH_ROWS { guess } else { rows[rows.len() * 99 / 100] }
}

impl Script {
    pub(crate) fn read(name: String, bytes: &[u8], widths: &mut Vec<usize>) -> Script {
        Script::parse(name, bytes, widths, false)
    }

    /// TyranoScript's scenes: a line `#name` says who speaks next.
    pub(crate) fn read_with_names(name: String, bytes: &[u8]) -> Script {
        Script::parse(name, bytes, &mut Vec::new(), true)
    }

    fn parse(name: String, bytes: &[u8], widths: &mut Vec<usize>, name_lines: bool) -> Script {
        let (lines, coding) = rows(bytes);
        let mut units: Vec<Unit> = Vec::new();
        let (mut names, mut speaking): (Vec<(usize, usize, usize)>, Option<String>) = (Vec::new(), None);
        let mut going: Option<Unit> = None; // text that may carry on in the next line
        let (mut code, mut spread) = (false, None::<String>);
        for (n, row) in lines.iter().enumerate() {
            let line = row.text.as_str();
            let t = line.trim_start();
            if let Some(tag) = spread.take() {
                let closed = t.contains(']');
                // a dialog's message runs on as plain lines; any other tag's lines hold values
                if tag == "dialog" && !closed && JA.is_match(t) {
                    let (start, end) = (line.len() - t.len(), line.trim_end().len());
                    units.push(Unit { spans: vec![(n, start, end)], quotes: Vec::new(), last_text: end, open: None, body: t.trim_end().to_string(), attribute: false, in_value: true, who: None });
                } else if tag != "dialog" {
                    shown_values(line, n, 0, &tag, line, &mut units);
                }
                spread = (!closed).then_some(tag);
                continue;
            }
            let command = t.strip_prefix(['[', '@']).unwrap_or("");
            if command.starts_with("iscript") || command.starts_with("html") || command.starts_with("lua") {
                code = true;
            }
            if name_lines && !code && t.starts_with('#') {
                units.extend(finish(going.take()));
                let start = line.len() - t.len() + 1;
                let who = t[1..].split(':').next().unwrap_or("").trim();
                speaking = JA.is_match(who).then(|| who.to_string());
                if speaking.is_some() {
                    names.push((n, start, start + t[1..].split(':').next().unwrap_or("").trim_end().len()));
                }
                continue;
            }
            if code || t.is_empty() || t.starts_with([';', '*', '@']) || t.starts_with("//") {
                if command.starts_with("endscript") || command.starts_with("endhtml") || command.starts_with("/lua") {
                    code = false;
                }
                units.extend(finish(going.take()));
                continue;
            }
            let Some(parts) = pieces(line) else {
                // a tag running on over lines, as a choice with one value a line
                units.extend(finish(going.take()));
                let open = line.rfind('[').unwrap_or(0);
                let tag = line[open + 1..].split_whitespace().next().unwrap_or("").to_string();
                shown_values(line, n, open, &tag, &line[open..], &mut units);
                spread = Some(tag);
                continue;
            };
            // what follows a tag can be a comment
            let parts: Vec<_> = parts.into_iter().take_while(|&(s, e, tag)| tag || !line[s..e].trim_start().starts_with("//")).collect();
            hand_rows(line, &parts, widths);
            if parts.iter().any(|&(s, e, tag)| tag && tag_name(&line[s..e]) == "iscript") {
                code = !parts.iter().any(|&(s, e, tag)| tag && tag_name(&line[s..e]) == "endscript");
                units.extend(finish(going.take()));
                continue;
            }
            let mut run: Vec<(usize, usize, bool)> = Vec::new();
            let mut quote: Option<(char, usize, usize)> = None;
            let mut starts_line = true;
            let who = parts.first().filter(|p| p.2).map(|p| tag_name(&line[p.0..p.1])).filter(|n| JA.is_match(n));
            for (i, &(start, end, tag)) in parts.iter().enumerate() {
                let piece = &line[start..end];
                let flows = !tag || SOFT.contains(&tag_name(piece)) || INLINE.contains(&tag_name(piece));
                if flows {
                    run.push((start, end, tag));
                }
                if flows && i + 1 < parts.len() {
                    continue;
                }
                let at_end = flows && i + 1 == parts.len();
                let (found, leftover) = unit(line, n, &run, quote, at_end);
                match found {
                    Some(mut u) => {
                        u.who = who.map(str::to_string).or_else(|| speaking.clone());
                        let carried = starts_line && run.first().is_some_and(|f| !f.2 && f.0 == line.len() - t.len());
                        if let (true, Some(mut g)) = (carried, going.take()) {
                            let last = g.spans.len() - 1;
                            g.body.push_str(&u.body);
                            g.spans.append(&mut u.spans);
                            g.quotes.append(&mut u.quotes);
                            g.spans[last].2 = g.spans[last].2.max(g.last_text);
                            g.last_text = u.last_text;
                            u = g;
                        } else {
                            units.extend(finish(going.take()));
                        }
                        if at_end {
                            going = Some(u);
                        } else {
                            units.extend(finish(Some(u)));
                        }
                    }
                    None => units.extend(finish(going.take())),
                }
                starts_line = false;
                quote = leftover;
                run.clear();
                if !flows {
                    shown_values(line, n, start, tag_name(piece), piece, &mut units);
                }
            }
        }
        units.extend(finish(going));
        Script { name, lines, coding, units, names, said: BTreeMap::new() }
    }

    /// Fills in who speaks where the tag starting a line stands for someone.
    fn name_speakers(&mut self, speakers: &Speakers) {
        for u in self.units.iter_mut().filter(|u| u.who.is_none()) {
            let line = self.lines[u.spans[0].0].text.as_str();
            let first = pieces(line).and_then(|p| p.first().copied()).filter(|p| p.2);
            u.who = first.and_then(|p| speakers.of(&line[p.0..p.1]));
        }
    }

    pub(crate) fn lines(&self, out: &mut Vec<Line>) {
        for u in &self.units {
            let (line, start, _) = u.spans[0];
            out.push((format!("{}:{line}:{start}", self.name), u.who.clone(), u.body.clone()));
        }
    }

    /// The keys of values shown from tags, as choices are: each on one row.
    pub(crate) fn value_keys(&self) -> Vec<String> {
        self.units.iter().filter(|u| u.attribute).map(|u| format!("{}:{}:{}", self.name, u.spans[0].0, u.spans[0].1)).collect()
    }

    /// The variables names are printed from, like `chara` in NAME_PRINT.
    pub(crate) fn printed_names(&self) -> Vec<String> {
        self.lines.iter().flat_map(|r| NAME_PRINT.captures_iter(&r.text).map(|c| c[2].to_string()).collect::<Vec<_>>()).collect()
    }

    /// The Japanese values this script's tags give `var`, like 太郎 in `[speak chara="太郎"]`.
    pub(crate) fn values_of(&self, var: &str) -> Vec<String> {
        let given = Regex::new(&format!(r#"\b{}="([^"']*)""#, regex::escape(var))).unwrap();
        self.lines.iter().flat_map(|r| given.captures_iter(&r.text).map(|c| c[1].to_string()).collect::<Vec<_>>()).filter(|v| JA.is_match(v)).collect()
    }

    pub(crate) fn set_said(&mut self, said: BTreeMap<String, BTreeSet<String>>) {
        self.said = said;
    }

    /// The script with translations in, unchanged lines exactly as they were.
    pub(crate) fn with(&self, layout: &Layout, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> Option<(Vec<u8>, usize)> {
        let mut edits: Vec<(usize, usize, usize, String)> = Vec::new();
        let mut done = 0;
        for (l, row) in self.lines.iter().enumerate() {
            for c in NAME_PRINT.captures_iter(&row.text) {
                let (whole, before, var, after) = (c.get(0).unwrap(), &c[1], &c[2], &c[3]);
                let spelled: Vec<(&String, String)> = self.said.get(var).into_iter().flatten().filter_map(|jp| Some((jp, name(jp).filter(|en| !en.is_empty())?))).collect();
                if spelled.is_empty() {
                    continue;
                }
                let mut chain = String::new();
                for (i, (jp, en)) in spelled.iter().enumerate() {
                    let tag = if i == 0 { "if" } else { "elseif" };
                    chain += &format!("[{tag} estimate=\"${var} == '{jp}'\"][print data=\"{before}{}{after}\"]", ascii_punctuation(en).replace(['"', '\''], ""));
                }
                chain += &format!("[else]{}[/if]", whole.as_str());
                edits.push((l, whole.start(), whole.end(), chain));
                done += 1;
            }
        }
        for u in &self.units {
            let (l, s, e) = u.spans[0];
            // a value that's a character's name keeps the spelling the list has for them
            let spelled = u.attribute.then(|| name(&u.body)).flatten();
            let Some(tr) = spelled.or_else(|| line(&format!("{}:{l}:{s}", self.name))) else { continue };
            let text = ascii_punctuation(&tr);
            let text = if u.attribute {
                format!("\"{}\"", text.replace('"', "'"))
            } else if u.in_value {
                text.replace('"', "'")
            } else {
                let bare = if u.quotes.is_empty() { text.as_str() } else { text.trim_matches('"') };
                let rows = wrap_words(bare, layout.width);
                let pages: Vec<String> = rows.chunks(layout.rows).map(|p| p.join(layout.row_break)).collect();
                let mut body = pages.join(layout.page_break);
                // these would turn the line into a label, a command or a comment
                if self.lines[l].text[..s].trim().is_empty() {
                    for (plain, wide) in [('*', "＊"), ('@', "＠"), (';', "；")] {
                        if body.starts_with(plain) {
                            body.replace_range(..1, wide);
                        }
                    }
                }
                body
            };
            edits.push((l, s, e, text));
            edits.extend(u.spans[1..].iter().map(|&(l, s, e)| (l, s, e, String::new())));
            edits.extend(u.quotes.iter().map(|&(l, s, e)| (l, s, e, ascii_punctuation(&self.lines[l].text[s..e]))));
            done += 1;
        }
        for &(l, s, e) in &self.names {
            if let Some(en) = name(&self.lines[l].text[s..e]) {
                edits.push((l, s, e, en));
                done += 1;
            }
        }
        if done == 0 {
            return None;
        }
        // from the far end, so earlier offsets hold
        edits.sort_by_key(|e| std::cmp::Reverse((e.0, e.1)));
        let mut changed: Vec<Option<String>> = (0..self.lines.len()).map(|_| None).collect();
        for (l, s, e, new) in edits {
            changed[l].get_or_insert_with(|| self.lines[l].text.clone()).replace_range(s..e, &new);
        }
        let mut out = Vec::new();
        for (row, new) in self.lines.iter().zip(changed) {
            match new {
                None => out.extend(&row.raw),
                Some(text) => {
                    let wide = self.coding == Coding::Utf16;
                    let ending = if !row.raw.ends_with(if wide { &[b'\n', 0][..] } else { b"\n" }) {
                        ""
                    } else if row.raw.ends_with(if wide { &[b'\r', 0, b'\n', 0][..] } else { b"\r\n" }) {
                        "\r\n"
                    } else {
                        "\n"
                    };
                    let whole = text + ending;
                    match self.coding {
                        Coding::Utf16 => {
                            if out.is_empty() {
                                out.extend([0xFF, 0xFE]);
                            }
                            out.extend(whole.encode_utf16().flat_map(u16::to_le_bytes));
                        }
                        Coding::Utf8 => out.extend(whole.as_bytes()),
                        Coding::Sjis => out.extend(sjis(&whole)),
                    }
                }
            }
        }
        Some((out, done))
    }
}

/// Text in a run of pieces, if any, and a quote left standing for the next run: `「[i]text」`.
fn unit(line: &str, n: usize, run: &[(usize, usize, bool)], quote: Option<(char, usize, usize)>, at_end: bool) -> (Option<Unit>, Option<(char, usize, usize)>) {
    let text = |p: &(usize, usize, bool)| &line[p.0..p.1];
    let Some(first) = run.iter().position(|p| !p.2 && JA.is_match(text(p))) else {
        let lone = run.iter().rev().find(|p| !p.2).filter(|p| closer(text(p).trim_end().chars().last().unwrap_or(' ')).is_some());
        return (None, lone.map(|p| {
            let t = text(p).trim_end();
            let c = t.chars().last().unwrap();
            (c, p.0 + t.len() - c.len_utf8(), p.0 + t.len())
        }));
    };
    let last = run.iter().rposition(|p| !p.2).unwrap();
    let mut body = String::new();
    for p in &run[first..=last] {
        if !p.2 || INLINE.contains(&tag_name(text(p))) {
            body.push_str(text(p));
        }
    }
    let mut u = Unit { spans: vec![(n, run[first].0, run[last].1)], quotes: Vec::new(), last_text: run[last].1, open: None, body, attribute: false, in_value: false, who: None };
    if let Some((c, s, e)) = quote {
        u.open = Some(c);
        u.quotes.push((n, s, e));
        u.body.insert(0, c);
    }
    if at_end {
        u.spans[0].2 = run.last().unwrap().1;
    }
    (Some(u), None)
}

/// The block is whole: settle its closing quote and its last span.
fn finish(u: Option<Unit>) -> Option<Unit> {
    let mut u = u?;
    let last = u.spans.len() - 1;
    u.spans[last].2 = u.last_text;
    if let Some(close) = u.open.and_then(closer) {
        if u.body.ends_with(close) {
            let (l, _, e) = u.spans[last];
            u.spans[last].2 = e - close.len_utf8();
            u.quotes.push((l, e - close.len_utf8(), e));
        } else {
            u.body.push(close);
        }
    }
    Some(u)
}

/// Values that show on screen; the rest of a tag is the engine's, which may compare it, so it stays.
/// `&` and `$` start code, not text.
fn shown_values(line: &str, n: usize, at: usize, name: &str, tag: &str, units: &mut Vec<Unit>) {
    if matches!(name, "ruby" | "eval" | "emb" | "if" | "elsif" | "ignore") {
        return;
    }
    for c in SHOWN.captures_iter(tag) {
        let v = c.get(2).unwrap();
        let body = v.as_str().trim_matches(['"', '\'']);
        let (s, e) = (at + v.start(), at + v.end());
        debug_assert!(line.is_char_boundary(s));
        if !JA.is_match(body) || body.starts_with(['&', '$']) {
            continue;
        }
        if matches!((name, c[1].trim_end_matches(|d: char| d.is_ascii_digit())), (_, "text" | "hint" | "jname") | ("dialog", "title" | "message") | ("print" | "caption", "data")) {
            units.push(Unit { spans: vec![(n, s, e)], quotes: Vec::new(), last_text: e, open: None, body: body.to_string(), attribute: true, in_value: false, who: None });
        }
    }
}

// ---------- menus and dialogs in .tjs files ----------

// Lines that plainly show text: a menu item, a dialog, a window's caption.
static TJS_SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"MenuItem\(|askYesNo\(|System\.inform\(|[Cc]aption\s*[:=]|\.hint\s*=|System\.title\s*=").unwrap());
static TJS_NOT_SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"==|!=|\bcase\b|\bdm\(|Debug|throw|Exception|\.storage|\.(?:ks|tjs|png|jpg|ogg|wav|txt)\b|/").unwrap());

/// A `.tjs` file in one string. None if it wouldn't go back to the same bytes as it stands.
fn tjs_text(b: &[u8]) -> Option<(String, Coding)> {
    let (text, coding) = if b.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = b[2..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        (String::from_utf16_lossy(&units), Coding::Utf16)
    } else if let Ok(text) = std::str::from_utf8(b) {
        (text.to_string(), Coding::Utf8)
    } else {
        (SHIFT_JIS.decode_without_bom_handling(b).0.into_owned(), Coding::Sjis)
    };
    (tjs_bytes(&text, coding) == b).then_some((text, coding))
}

fn tjs_bytes(text: &str, coding: Coding) -> Vec<u8> {
    match coding {
        Coding::Utf8 => text.as_bytes().to_vec(),
        Coding::Sjis => sjis(text),
        Coding::Utf16 => [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect(),
    }
}

/// A `.tjs` file that shows text: where it is, and its text.
struct Code {
    archive: usize,
    entry: usize,
    name: String,
    coding: Coding,
    text: String,
    spots: Vec<(usize, usize)>,
}

impl Code {
    fn key(&self, start: usize) -> String {
        format!("tjs:{}:{start}", self.name)
    }

    fn lines(&self, out: &mut Vec<Line>) {
        out.extend(self.spots.iter().map(|&(start, end)| (self.key(start), None, self.text[start..end].to_string())));
    }
}

// ---------- the game ----------

pub struct Game {
    archives: Vec<Archive>,
    /// `(archive, entry, script)`
    scripts: Vec<(usize, usize, Script)>,
    code: Vec<Code>,
    width: usize,
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let archives = archives(game)?;
        let mut found: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
        for (a, archive) in archives.iter().enumerate() {
            for (i, e) in archive.entries.iter().enumerate() {
                let low = e.name.to_lowercase();
                if low.ends_with(".ks") || low.ends_with(".tjs") {
                    found.insert(low, (a, i));
                }
            }
        }
        let (mut scripts, mut widths, mut files) = (Vec::new(), Vec::new(), Vec::new());
        for (name, (a, i)) in found {
            let archive = &archives[a];
            let from = if backup(&archive.path).exists() { backup(&archive.path) } else { archive.path.clone() };
            let mut f = fs::File::open(from).map_err(|e| e.to_string())?;
            let bytes = archive.file(&mut f, &archive.entries[i])?;
            if name.ends_with(".ks") {
                scripts.push((a, i, Script::read(name, &bytes, &mut widths)));
            } else if let Some((text, coding)) = tjs_text(&bytes) {
                files.push(Code { archive: a, entry: i, name, coding, text, spots: Vec::new() });
            }
        }
        let sources: Vec<String> = files.iter().map(|c| c.text.clone()).collect();
        for (n, start, end) in shown_strings(&sources, &TJS, &TJS_SHOWN, &TJS_NOT_SHOWN) {
            files[n].spots.push((start, end));
        }
        files.retain(|c| !c.spots.is_empty());
        let mut speakers = Speakers::default();
        scripts.iter().for_each(|s: &(usize, usize, Script)| speakers.learn(&s.2));
        scripts.iter_mut().for_each(|s| s.2.name_speakers(&speakers));
        if scripts.iter().all(|s| s.2.units.is_empty()) {
            return Err("No text found in this KiriKiri game's .ks scripts: they may be encrypted, or compiled to .scn.".into());
        }
        Ok(Game { archives, scripts, code: files, width: crate::fit::letters(game, box_width(widths, LINE_LETTERS)) })
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|s| s.2.lines(&mut out));
        self.code.iter().for_each(|c| c.lines(&mut out));
        out
    }
}

/// Writes the translations (`line` by key) into the archives and reads them back. Returns how many
/// texts were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let mut done = 0;
    for (a, archive) in g.archives.iter().enumerate() {
        let mut new = Vec::new();
        for (_, i, s) in g.scripts.iter().filter(|s| s.0 == a) {
            if let Some((bytes, count)) = s.with(&Layout { width: g.width, row_break: "[r]", rows: usize::MAX, page_break: "" }, line, &|_| None) {
                new.push((*i, bytes));
                done += count;
            }
        }
        for c in g.code.iter().filter(|c| c.archive == a) {
            let (text, count) = with_english(&c.text, &c.spots, &|start, _| line(&c.key(start)));
            if count > 0 {
                new.push((c.entry, tjs_bytes(&text, c.coding)));
                done += count;
            }
        }
        if new.is_empty() {
            continue;
        }
        archive.replace(&new)?;
        let back = Archive::read(&archive.path, &archive.path)?.ok_or("the archive didn't read back")?;
        let mut f = fs::File::open(&archive.path).map_err(|e| e.to_string())?;
        for (i, bytes) in &new {
            if back.file(&mut f, &back.entries[*i])? != *bytes {
                return Err(format!("{} didn't read back as written", back.entries[*i].name));
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, at: u64, plain: &[u8], stored: u64, flags: u32) -> Vec<u8> {
        let chunk = |tag: &[u8], v: Vec<u8>| [tag, &(v.len() as u64).to_le_bytes(), &v].concat();
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let info = [&0x8000_0000u32.to_le_bytes()[..], &(plain.len() as u64).to_le_bytes(), &stored.to_le_bytes(), &(name.len() as u16).to_le_bytes(), &units].concat();
        let segm = [&flags.to_le_bytes()[..], &at.to_le_bytes(), &(plain.len() as u64).to_le_bytes(), &stored.to_le_bytes()].concat();
        chunk(b"File", [chunk(b"info", info), chunk(b"segm", segm), chunk(b"adlr", adler32(plain).to_le_bytes().to_vec())].concat())
    }

    fn pack(b: &[u8]) -> Vec<u8> {
        let mut z = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(b).unwrap();
        z.finish().unwrap()
    }

    /// An archive with the newer header, a packed script and an untouched file.
    fn xp3(script: &[u8]) -> Vec<u8> {
        xp3_with(script, b"")
    }

    fn xp3_with(script: &[u8], tjs: &[u8]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend(0x17u64.to_le_bytes());
        out.extend(1u32.to_le_bytes());
        out.push(CUSHION);
        out.extend(0u64.to_le_bytes());
        out.extend(0u64.to_le_bytes());
        let stored = pack(script);
        let mut index = entry("scenario/a.ks", out.len() as u64, script, stored.len() as u64, 1);
        out.extend(&stored);
        index.extend(entry("image.png", out.len() as u64, b"picture", 7, 0));
        out.extend(b"picture");
        if !tjs.is_empty() {
            let stored = pack(tjs);
            index.extend(entry("system/menu.tjs", out.len() as u64, tjs, stored.len() as u64, 1));
            out.extend(&stored);
        }
        let at = out.len() as u64;
        out[32..40].copy_from_slice(&at.to_le_bytes());
        let packed = pack(&index);
        out.push(1);
        out.extend((packed.len() as u64).to_le_bytes());
        out.extend((index.len() as u64).to_le_bytes());
        out.extend(packed);
        out
    }

    #[test]
    fn a_speaker_tag_is_read_through_the_macro_behind_it() {
        let macros = "[macro name=n]\n[eval exp=\"f.name = '太郎', tf.c = '0x1'\" cond=\"mp.n == 'Tar'\"]\n[eval exp=\"f.name = sf.name\" cond=\"mp.n == 'Me'\"]\n\
            [eval exp=\"f.name = mp.name\" cond=\"mp.n == 'inp'\"]\n[endmacro]\n[macro name=FG]\n[image storage=\"a\" cond=\"mp.ch == 'b'\"]\n[endmacro]\n";
        let scene = "[n n=Tar]「やあ」[p]\n[n n=Me]「どうも」[p]\n[n n=inp name='花子']「はい」[p]\n[FG ch=b]地の文[p]\n[n]地の文です[p]\n[n n=Who]「だれ」[p]\n";
        let mut speakers = Speakers::default();
        speakers.learn(&Script::read("macro.ks".into(), macros.as_bytes(), &mut Vec::new()));
        let mut s = Script::read("a.ks".into(), scene.as_bytes(), &mut Vec::new());
        s.name_speakers(&speakers);
        let mut out = Vec::new();
        s.lines(&mut out);
        let who: Vec<Option<&str>> = out.iter().map(|l| l.1.as_deref()).collect();
        assert_eq!(who, [Some("太郎"), Some("Me"), Some("花子"), None, None, None], "a tag about pictures, or a code nobody defined, says nothing");
    }

    const MENU: &str = "// \"コメント\"\nmenu.add(new KAGMenuItem(this, \"メッセージを消す\", 0));\nif(kind == \"火\") dm(\"ログ\");\nkag.fore.caption = \"火\";\ncaption = \"セーブ\";\n";

    #[test]
    fn a_tjs_file_gives_up_its_menu_text_and_keeps_the_rest() {
        let dir = game("こんにちは[p]\n");
        let bytes = xp3_with(&sjis("こんにちは[p]\n"), &sjis(MENU));
        fs::write(dir.join("data.xp3"), &bytes).unwrap();
        let lines = Game::open(&dir).unwrap().lines();
        let texts: Vec<&str> = lines.iter().map(|l| l.2.as_str()).collect();
        assert_eq!(texts, ["こんにちは", "メッセージを消す", "セーブ"], "a string compared anywhere stays, in a comment too");
        let english = |k: &str| k.starts_with("tjs:").then(|| "Hide \"text\"".to_string());
        assert_eq!(install(&dir, &english).unwrap(), 2);
        let g = Archive::read(&dir.join("data.xp3"), &dir.join("data.xp3")).unwrap().unwrap();
        let mut f = fs::File::open(dir.join("data.xp3")).unwrap();
        let entry = g.entries.iter().find(|e| e.name.ends_with(".tjs")).unwrap();
        let text = SHIFT_JIS.decode_without_bom_handling(&g.file(&mut f, entry).unwrap()).0.into_owned();
        assert_eq!(text, MENU.replace("メッセージを消す", "Hide 'text'").replace("セーブ", "Hide 'text'"));
        assert_eq!(Game::open(&dir).unwrap().lines().len(), 3, "the game's own file is read again");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tjs_file_that_would_not_come_back_the_same_is_left_alone() {
        assert!(tjs_text(&[0x81]).is_none(), "half a letter");
        for (text, bytes) in [("表", &[0xFF, 0xFE, 0x68, 0x88][..]), ("a", b"a")] {
            assert_eq!(tjs_text(bytes).unwrap().0, text);
        }
    }

    fn game(script: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-kirikiri-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data.xp3"), xp3(&sjis(script))).unwrap();
        dir
    }

    fn read(script: &str) -> Vec<String> {
        let mut widths = Vec::new();
        let s = Script::read("a".into(), &sjis(script), &mut widths);
        let mut out = Vec::new();
        s.lines(&mut out);
        out.into_iter().map(|l| l.2).collect()
    }

    #[test]
    fn a_choice_tag_over_several_lines_gives_each_shown_value_unwrapped() {
        let script = "[select_scene num=\"2\"\r\n\ttext1=\"はい\" file1=\"a\" label1=\"1\"\r\n\ttext2=\"いいえ\" file2=\"a\" label2=\"2\"]\r\n地の文です\r\n";
        assert_eq!(read(script), ["はい", "いいえ", "地の文です"]);
        let s = Script::read("a".into(), &sjis(script), &mut Vec::new());
        let layout = Layout { width: 6, row_break: "[rt]", rows: 3, page_break: "@[rp]" };
        let (bytes, done) = s.with(&layout, &|_| Some("Yes I will go".into()), &|_| None).unwrap();
        assert_eq!(done, 3);
        let out = SHIFT_JIS.decode(&bytes).0;
        assert!(out.contains("text1=\"Yes I will go\" file1") && out.contains("text2=\"Yes I will go\" file2"), "{out}");
        assert!(out.ends_with("Yes I[rt]will[rt]go\r\n"), "only the story line is wrapped: {out}");
    }

    #[test]
    fn code_comments_labels_and_tags_are_not_text() {
        let script = "; 説明です\r\n*ラベル|見出し\r\n@jump storage=\"次\"\r\n[iscript]\r\nvar a = \"日本語\";\r\n[endscript]\r\n[eval exp=\"f.a='名前'\"]\r\n地の文です[p]\r\n";
        assert_eq!(read(script), ["地の文です"]);
    }

    #[test]
    fn rows_broken_by_hand_are_one_message() {
        assert_eq!(read("一行目です[r]\r\n二行目です[lr]\r\n三行目です[eh]\r\n次の文[p]\r\n"), ["一行目です二行目です三行目です", "次の文"]);
    }

    #[test]
    fn soft_tags_go_and_named_values_stay_in_the_sentence() {
        assert_eq!(read("前半[w]後半[emb exp=\"f.name\"]さん[p]\r\n"), ["前半後半[emb exp=\"f.name\"]さん"]);
    }

    #[test]
    fn a_quote_cut_off_by_a_tag_travels_with_its_text() {
        assert_eq!(read("[n n=x]「[i]台詞です[w]続きです」[Pe]\r\n"), ["「台詞です続きです」"]);
    }

    #[test]
    fn brackets_inside_quoted_values_do_not_end_a_tag_and_shown_values_are_found() {
        assert_eq!(read("[btext text=\"選ぶ\" exp=\"f.a[1]+1\"]\r\n"), ["選ぶ"]);
    }

    #[test]
    fn translations_go_on_the_end_with_a_new_index() {
        let script = "*top\r\n[n n=x]「[i]台詞です」[Pe]\r\n; メモ\r\n一行目[r]\r\n二行目[p]\r\n";
        let dir = game(script);
        let before = fs::read(dir.join("data.xp3")).unwrap();
        let g = Game::open(&dir).unwrap();
        let lines = g.lines();
        assert_eq!(lines.len(), 2);
        let keys: Vec<_> = lines.iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.iter().position(|x| x == k).map(|i| if i == 0 { "\"A line.\"".to_string() } else { "*sigh* Two rows.".to_string() });
        assert_eq!(install(&dir, &line).unwrap(), 2);

        let path = dir.join("data.xp3");
        let written = fs::read(&path).unwrap();
        assert_eq!(fs::read(backup(&path)).unwrap(), before);
        assert_eq!(&written[..before.len()][HEAD..], &before[HEAD..], "nothing before the new data moves");
        let a = Archive::read(&path, &path).unwrap().unwrap();
        let mut f = fs::File::open(&path).unwrap();
        let text = a.file(&mut f, &a.entries[0]).unwrap();
        assert_eq!(text, sjis("*top\r\n[n n=x]\"[i]A line.\"[Pe]\r\n; メモ\r\n＊sigh* Two rows.\r\n[p]\r\n"));
        assert_eq!(a.file(&mut f, &a.entries[1]).unwrap(), b"picture");
        drop(f);

        // a second run starts from the game's own file and gives the same bytes
        install(&dir, &line).unwrap();
        assert_eq!(fs::read(&path).unwrap(), written);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn width_comes_from_rows_broken_by_hand() {
        let row = "あ".repeat(30);
        let mut script = String::new();
        (0..30).for_each(|_| script += &format!("{row}[r]\r\n{}[p]\r\n", "い".repeat(90)));
        let dir = game(&script);
        assert_eq!(Game::open(&dir).unwrap().width, 60);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn utf8_scripts_and_name_lines() {
        let script = "#太郎:smile\n台詞です[p]\n#\n地の文です[p]\n";
        let s = Script::read_with_names("a".into(), script.as_bytes());
        let mut out = Vec::new();
        s.lines(&mut out);
        assert_eq!(out.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect::<Vec<_>>(), [(Some("太郎"), "台詞です"), (None, "地の文です")]);
        let keys: Vec<_> = out.iter().map(|l| l.0.clone()).collect();
        let layout = Layout { width: 1000, row_break: "[r]", rows: usize::MAX, page_break: "" };
        let (bytes, count) = s.with(&layout, &|k| keys.iter().position(|x| x == k).map(|i| format!("Line {i}")), &|n| (n == "太郎").then(|| "Taro".to_string())).unwrap();
        assert_eq!(count, 3);
        assert_eq!(String::from_utf8(bytes).unwrap(), "#Taro:smile\nLine 0[p]\n#\nLine 1[p]\n");
    }

    #[test]
    fn dialogs_and_printed_values_are_found_and_compared_values_stay() {
        let script = "[dialog title=\"確認\" message=\"保存します\"]\n[print data=\"$ '【' + chara + '】'\"]\n[print data=\"表示文\"]\n[debugprint data=\"開発用\"]\n[speak chara=\"太郎\"]\n[if estimate=\"$chara == '太郎'\"]\n[print data=\"太郎\"]\n";
        let s = Script::read("a".into(), &sjis(script), &mut Vec::new());
        let mut out = Vec::new();
        s.lines(&mut out);
        assert_eq!(out.iter().map(|l| l.2.as_str()).collect::<Vec<_>>(), ["確認", "保存します", "表示文", "太郎"]);
        let keys: Vec<_> = out.iter().map(|l| l.0.clone()).collect();
        let layout = Layout { width: 40, row_break: "[rt]", rows: 3, page_break: "@[rp]" };
        let (bytes, count) = s.with(&layout, &|k| keys.contains(&k.to_string()).then(|| "Yes".to_string()), &|n| (n == "太郎").then(|| "Taro".to_string())).unwrap();
        assert_eq!(count, 4);
        let text = SHIFT_JIS.decode_without_bom_handling(&bytes).0.into_owned();
        assert!(text.contains("[speak chara=\"太郎\"]") && text.contains("$chara == '太郎'"), "the macro must still match: {text}");
        assert!(text.contains("[print data=\"Taro\"]") && text.contains("[dialog title=\"Yes\" message=\"Yes\"]") && text.contains("[debugprint data=\"開発用\"]"), "{text}");
    }

    #[test]
    fn a_name_printed_for_the_log_picks_its_english_while_the_tag_keeps_its_japanese() {
        let macros = "*speak\n[font size=0][print data=\"$ '【' + chara + '】'\"][/font]\n[return]\n";
        let scene = "[speak chara=\"太郎\"]「はい」\n[speak chara=\"花子\"]「ええ」\n";
        let (mut m, s) = (Script::read("macro".into(), &sjis(macros), &mut Vec::new()), Script::read("scene".into(), &sjis(scene), &mut Vec::new()));
        assert_eq!(m.printed_names(), ["chara"]);
        let said: BTreeMap<String, BTreeSet<String>> = [("chara".to_string(), s.values_of("chara").into_iter().collect())].into();
        m.set_said(said);
        let layout = Layout { width: 40, row_break: "[r]", rows: usize::MAX, page_break: "" };
        let (bytes, count) = m.with(&layout, &|_| None, &|n| (n == "太郎").then(|| "Taro".to_string())).unwrap();
        let text = SHIFT_JIS.decode_without_bom_handling(&bytes).0.into_owned();
        assert_eq!(count, 1);
        assert!(text.contains("[if estimate=\"$chara == '太郎'\"][print data=\"【Taro】\"][else][print data=\"$ '【' + chara + '】'\"][/if]"), "{text}");
        assert!(!text.contains("花子"), "a name with no English keeps the game's own print");
    }

    #[test]
    fn lua_blocks_and_comments_after_tags_are_not_text() {
        assert_eq!(read("[lua]\r\n-- 説明\r\ne:tag{data=(\"日本語\")}\r\n[/lua]\r\n[wait]// 待つ処理\r\n地の文です[np]\r\n"), ["地の文です"]);
    }

    #[test]
    fn a_dialog_message_over_several_lines_keeps_its_quotes() {
        let script = "[dialog title=\"help\" message=\"\n一行目の説明\n\n二行目の説明\n\"]\n";
        let s = Script::read("a".into(), &sjis(script), &mut Vec::new());
        let mut out = Vec::new();
        s.lines(&mut out);
        assert_eq!(out.iter().map(|l| l.2.as_str()).collect::<Vec<_>>(), ["一行目の説明", "二行目の説明"]);
        let layout = Layout { width: 1000, row_break: "[r]", rows: usize::MAX, page_break: "" };
        let (bytes, _) = s.with(&layout, &|_| Some("Say \"hi\" now".to_string()), &|_| None).unwrap();
        assert_eq!(bytes, sjis("[dialog title=\"help\" message=\"\nSay 'hi' now\n\nSay 'hi' now\n\"]\n"));
    }

    #[test]
    fn a_full_box_goes_on_to_the_next_page() {
        let s = Script::read("a".into(), &sjis("地の文です[np]\n"), &mut Vec::new());
        let layout = Layout { width: 10, row_break: "[rt]", rows: 2, page_break: "@[rp]" };
        let long = |_: &str| Some("aaaa bbbb cccc dddd eeee ffff gggg".to_string());
        let (bytes, _) = s.with(&layout, &long, &|_| None).unwrap();
        assert_eq!(bytes, b"aaaa bbbb[rt]cccc dddd@[rp]eeee ffff[rt]gggg[np]\n");
    }

    #[test]
    fn a_later_patch_wins() {
        let dir = game("元の文[p]\r\n");
        fs::write(dir.join("patch.xp3"), xp3(&sjis("直した文[p]\r\n"))).unwrap();
        let lines = Game::open(&dir).unwrap().lines();
        assert_eq!(lines.iter().map(|l| l.2.as_str()).collect::<Vec<_>>(), ["直した文"]);
        let _ = fs::remove_dir_all(dir);
    }
}


