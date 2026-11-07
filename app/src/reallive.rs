//! RealLive engine (SEEN.TXT, Gameexe.ini): scenarios of compiled code, LZ packed and XORed with a fixed
//! mask. Text sits inside the code, so a new line moves everything after it: the code is walked whole,
//! noting every jump, and rebuilt with the jumps put right.

use crate::code::{shown_strings, with_english, GAMEEXE};
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, wrap_words, JA};
use crate::{original, replace_file, R};
use encoding_rs::SHIFT_JIS;
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

const ARCHIVE: &str = "SEEN.TXT";
const CONFIG: &str = "GAMEEXE.INI";
const SCENES: usize = 10_000;
const HEADER: usize = 0x1d0;

const MASK: [u8; 256] = [
    0x8b, 0xe5, 0x5d, 0xc3, 0xa1, 0xe0, 0x30, 0x44, 0x00, 0x85, 0xc0, 0x74, 0x09, 0x5f, 0x5e, 0x33,
    0xc0, 0x5b, 0x8b, 0xe5, 0x5d, 0xc3, 0x8b, 0x45, 0x0c, 0x85, 0xc0, 0x75, 0x14, 0x8b, 0x55, 0xec,
    0x83, 0xc2, 0x20, 0x52, 0x6a, 0x00, 0xe8, 0xf5, 0x28, 0x01, 0x00, 0x83, 0xc4, 0x08, 0x89, 0x45,
    0x0c, 0x8b, 0x45, 0xe4, 0x6a, 0x00, 0x6a, 0x00, 0x50, 0x53, 0xff, 0x15, 0x34, 0xb1, 0x43, 0x00,
    0x8b, 0x45, 0x10, 0x85, 0xc0, 0x74, 0x05, 0x8b, 0x4d, 0xec, 0x89, 0x08, 0x8a, 0x45, 0xf0, 0x84,
    0xc0, 0x75, 0x78, 0xa1, 0xe0, 0x30, 0x44, 0x00, 0x8b, 0x7d, 0xe8, 0x8b, 0x75, 0x0c, 0x85, 0xc0,
    0x75, 0x44, 0x8b, 0x1d, 0xd0, 0xb0, 0x43, 0x00, 0x85, 0xff, 0x76, 0x37, 0x81, 0xff, 0x00, 0x00,
    0x04, 0x00, 0x6a, 0x00, 0x76, 0x43, 0x8b, 0x45, 0xf8, 0x8d, 0x55, 0xfc, 0x52, 0x68, 0x00, 0x00,
    0x04, 0x00, 0x56, 0x50, 0xff, 0x15, 0x2c, 0xb1, 0x43, 0x00, 0x6a, 0x05, 0xff, 0xd3, 0xa1, 0xe0,
    0x30, 0x44, 0x00, 0x81, 0xef, 0x00, 0x00, 0x04, 0x00, 0x81, 0xc6, 0x00, 0x00, 0x04, 0x00, 0x85,
    0xc0, 0x74, 0xc5, 0x8b, 0x5d, 0xf8, 0x53, 0xe8, 0xf4, 0xfb, 0xff, 0xff, 0x8b, 0x45, 0x0c, 0x83,
    0xc4, 0x04, 0x5f, 0x5e, 0x5b, 0x8b, 0xe5, 0x5d, 0xc3, 0x8b, 0x55, 0xf8, 0x8d, 0x4d, 0xfc, 0x51,
    0x57, 0x56, 0x52, 0xff, 0x15, 0x2c, 0xb1, 0x43, 0x00, 0xeb, 0xd8, 0x8b, 0x45, 0xe8, 0x83, 0xc0,
    0x20, 0x50, 0x6a, 0x00, 0xe8, 0x47, 0x28, 0x01, 0x00, 0x8b, 0x7d, 0xe8, 0x89, 0x45, 0xf4, 0x8b,
    0xf0, 0xa1, 0xe0, 0x30, 0x44, 0x00, 0x83, 0xc4, 0x08, 0x85, 0xc0, 0x75, 0x56, 0x8b, 0x1d, 0xd0,
    0xb0, 0x43, 0x00, 0x85, 0xff, 0x76, 0x49, 0x81, 0xff, 0x00, 0x00, 0x04, 0x00, 0x6a, 0x00, 0x76,
];

fn word(b: &[u8], p: usize) -> R<usize> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated RealLive scene")?.try_into().unwrap()) as usize)
}

// ---------- packing ----------

fn unpack(src: &[u8], size: usize) -> R<Vec<u8>> {
    const BAD: &str = "bad packed RealLive scene";
    let mut b = src.get(8..).ok_or(BAD)?.to_vec();
    for (i, c) in b.iter_mut().enumerate() {
        *c ^= MASK[(i + 8) % 256];
    }
    let (mut out, mut p) = (Vec::with_capacity(size), 0);
    while out.len() < size && p < b.len() {
        let flags = b[p];
        p += 1;
        for bit in 0..8 {
            if out.len() >= size || p >= b.len() {
                break;
            }
            if flags >> bit & 1 != 0 {
                out.push(b[p]);
                p += 1;
            } else {
                let raw = u16::from_le_bytes(b.get(p..p + 2).ok_or(BAD)?.try_into().unwrap()) as usize;
                p += 2;
                let (back, len) = (raw >> 4, (raw & 15) + 2);
                if back == 0 || back > out.len() {
                    return Err(BAD.into());
                }
                for _ in 0..len.min(size - out.len()) {
                    out.push(out[out.len() - back]);
                }
            }
        }
    }
    if out.len() == size { Ok(out) } else { Err(BAD.into()) }
}

/// Literals only: a little bigger than the game's own, which doesn't matter to it.
fn stored(plain: &[u8]) -> Vec<u8> {
    let mut out = vec![0; 8];
    for chunk in plain.chunks(8) {
        out.push(if chunk.len() == 8 { 0xff } else { (1u16 << chunk.len()) as u8 - 1 });
        out.extend(chunk);
    }
    let size = out.len() as u32;
    out[..4].copy_from_slice(&size.to_le_bytes());
    out[4..8].copy_from_slice(&(plain.len() as u32).to_le_bytes());
    for (i, c) in out.iter_mut().enumerate().skip(8) {
        *c ^= MASK[i % 256];
    }
    out
}

// ---------- the code ----------

fn is_lead(c: u8) -> bool {
    matches!(c, 0x81..=0x9f | 0xe0..=0xef)
}

fn char_len(c: u8) -> usize {
    if is_lead(c) { 2 } else { 1 }
}

/// One element of the code.
#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Text,
    Command(u8, u8, u16),
    Other,
}

#[derive(Debug, Clone)]
struct Element {
    at: usize,
    end: usize,
    kind: Kind,
    /// Places of the jump targets inside it.
    jumps: Vec<usize>,
    /// The options' text, for a select.
    options: Vec<(usize, usize)>,
}

/// A cursor over the code; every step checks it stays inside.
struct Walk<'a> {
    b: &'a [u8],
    p: usize,
}

impl Walk<'_> {
    fn peek(&self) -> u8 {
        self.b.get(self.p).copied().unwrap_or(0)
    }

    fn eat(&mut self, c: u8) -> R<()> {
        if self.peek() == c {
            self.p += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at {:#x}", c as char, self.p))
        }
    }

    fn token(&mut self) -> R<()> {
        match self.peek() {
            0xff => self.p += 5,
            0xc8 => self.p += 1,
            _ if self.b.get(self.p + 1) == Some(&b'[') => {
                self.p += 2;
                self.expression()?;
                self.eat(b']')?;
            }
            c => return Err(format!("unknown token {c:#x} at {:#x}", self.p)),
        }
        Ok(())
    }

    fn term(&mut self) -> R<()> {
        match self.peek() {
            b'$' => {
                self.p += 1;
                self.token()
            }
            b'\\' if matches!(self.b.get(self.p + 1), Some(0 | 1)) => {
                self.p += 2;
                self.term()
            }
            b'(' => {
                self.p += 1;
                self.expression()?;
                self.eat(b')')
            }
            c => Err(format!("unknown term {c:#x} at {:#x}", self.p)),
        }
    }

    fn tail(&mut self) -> R<()> {
        while self.peek() == b'\\' {
            self.p += 2;
            self.term()?;
        }
        Ok(())
    }

    fn expression(&mut self) -> R<()> {
        self.term()?;
        self.tail()
    }

    fn quote(&self, start: usize, at: usize) -> bool {
        self.b.get(at) == Some(&b'"') && (at == start || self.b[at - 1] != b'\\')
    }

    /// A string in a parameter, which may be quoted, and may hold a `###PRINT(expr)`.
    fn string(&mut self) -> R<()> {
        let (start, mut quoted) = (self.p, false);
        while self.p < self.b.len() {
            let c = self.b[self.p];
            if quoted {
                if self.quote(start, self.p) {
                    self.p += 1;
                    return Ok(());
                }
            } else {
                quoted = self.quote(start, self.p);
                if self.b[self.p..].starts_with(b"###PRINT(") {
                    self.p += 9;
                    self.expression()?;
                    self.p += 1;
                    continue;
                }
                let tag = c == b'a' && self.b.get(self.p + 1).is_some_and(|&n| n < 0x20);
                if tag || !(is_lead(c) || c.is_ascii_alphanumeric() || matches!(c, b' ' | b'?' | b'_' | b'"' | b'\\')) {
                    return Ok(());
                }
            }
            self.p += char_len(c);
        }
        Ok(())
    }

    fn string_start(&self) -> bool {
        let c = self.peek();
        is_lead(c) || c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, b' ' | b'?' | b'_' | b'"') || self.b[self.p..].starts_with(b"###PRINT(")
    }

    fn data(&mut self) -> R<()> {
        match self.peek() {
            b',' => {
                self.p += 1;
                return self.data();
            }
            b'\n' => {
                self.p += 3;
                return self.data();
            }
            _ => {}
        }
        if self.string_start() {
            return self.string();
        }
        if matches!(self.peek(), b'a' | b'(') {
            if self.peek() == b'a' {
                self.p += 2;
                if self.peek() == b'a' {
                    self.p += 2;
                }
                if self.peek() != b'(' {
                    return self.data();
                }
            }
            self.p += 1;
            self.list()?;
            return self.tail();
        }
        self.expression()
    }

    /// Parameters up to and past the closing bracket.
    fn list(&mut self) -> R<()> {
        loop {
            match self.peek() {
                b',' => self.p += 1,
                b'\n' => self.p += 3,
                b')' => {
                    self.p += 1;
                    return Ok(());
                }
                _ if self.p >= self.b.len() => return Err("unterminated parameters".into()),
                _ => self.data()?,
            }
        }
    }

    fn params(&mut self) -> R<()> {
        if self.peek() == b'(' {
            self.p += 1;
            self.list()?;
        }
        Ok(())
    }

    fn text(&mut self) {
        let (mut end, mut quoted) = (self.p, false);
        while end < self.b.len() {
            let mut c = self.b[end];
            if quoted {
                quoted = c != b'"';
                if c == b'\\' && self.b.get(end + 1) == Some(&b'"') {
                    end += 1;
                }
            } else {
                if c == b',' {
                    end += 1;
                    c = self.b.get(end).copied().unwrap_or(0);
                }
                quoted = c == b'"';
                if matches!(c, 0 | b'#' | b'$' | b'\n' | b'@' | b'!') {
                    break;
                }
            }
            end += char_len(self.b.get(end).copied().unwrap_or(0));
        }
        self.p = end.min(self.b.len());
    }

    fn jump(&mut self, jumps: &mut Vec<usize>) {
        jumps.push(self.p);
        self.p += 4;
    }

    /// The flow commands have a layout of their own. Numbers differ by module.
    fn command(&mut self) -> R<Element> {
        let at = self.p;
        let h = self.b.get(at..at + 8).ok_or("truncated command")?;
        let (modtype, module, opcode, argc) = (h[1], h[2], u16::from_le_bytes([h[3], h[4]]), u16::from_le_bytes([h[5], h[6]]) as usize);
        self.p += 8;
        let (mut jumps, mut options) = (Vec::new(), Vec::new());
        let flow = if modtype == 0 { (module, opcode) } else { (0xff, 0) };
        match flow {
            (1, 0 | 5) | (5 | 6, 1 | 5) => self.jump(&mut jumps),
            (1, 1 | 2 | 6 | 7) | (5, 2 | 6 | 7) | (6, 0 | 2 | 6 | 7) => {
                if self.peek() == b'(' {
                    self.p += 1;
                    self.expression()?;
                    self.eat(b')')?;
                }
                self.jump(&mut jumps);
            }
            (1 | 5 | 6, 3 | 8) => {
                self.expression()?;
                self.eat(b'{')?;
                (0..argc).for_each(|_| self.jump(&mut jumps));
                self.eat(b'}')?;
            }
            (1 | 5 | 6, 4 | 9) => {
                self.expression()?;
                self.eat(b'{')?;
                for _ in 0..argc {
                    self.eat(b'(')?;
                    if self.peek() != b')' {
                        self.expression()?;
                    }
                    self.eat(b')')?;
                    self.jump(&mut jumps);
                }
                self.eat(b'}')?;
            }
            (1 | 6, 16) => {
                self.params()?;
                self.jump(&mut jumps);
            }
            (2, 0..=3 | 16) => self.select(argc, &mut options)?,
            _ => self.params()?,
        }
        Ok(Element { at, end: self.p, kind: Kind::Command(modtype, module, opcode), jumps, options })
    }

    fn select(&mut self, argc: usize, options: &mut Vec<(usize, usize)>) -> R<()> {
        if self.peek() == b'(' {
            self.p += 1;
            self.expression()?;
            self.eat(b')')?;
        }
        self.eat(b'{')?;
        if self.peek() == b'\n' {
            self.p += 3;
        }
        for _ in 0..argc {
            while self.peek() == b',' {
                self.p += 1;
            }
            if self.peek() == b'(' {
                self.p += 1;
                while self.peek() != b')' {
                    if self.p >= self.b.len() {
                        return Err("unterminated select".into());
                    }
                    if self.peek() == b'(' {
                        self.expression()?;
                    }
                    let effect = self.peek();
                    self.p += 1;
                    if !matches!(effect, b'2' | b'3') && self.peek() != b')' && !self.peek().is_ascii_digit() {
                        self.expression()?;
                    }
                }
                self.p += 1;
            }
            let at = self.p;
            self.string()?;
            options.push((at, self.p));
            self.eat(b'\n')?;
            self.p += 2;
        }
        while self.peek() == b'\n' {
            self.p += 3;
        }
        self.eat(b'}')
    }

    fn element(&mut self) -> R<Element> {
        let at = self.p;
        let other = |p: usize| Element { at, end: p, kind: Kind::Other, jumps: Vec::new(), options: Vec::new() };
        match self.peek() {
            0 | b',' => Ok(other(at + 1)),
            b'\n' | b'@' | b'!' => Ok(other(at + 3)),
            b'$' => {
                self.term()?;
                self.tail()?;
                Ok(other(self.p))
            }
            b'#' => self.command(),
            _ => {
                self.text();
                Ok(Element { at, end: self.p.max(at + 1), kind: Kind::Text, jumps: Vec::new(), options: Vec::new() })
            }
        }
    }
}

fn walk(code: &[u8]) -> R<Vec<Element>> {
    let mut w = Walk { b: code, p: 0 };
    let mut out = Vec::new();
    while w.p < code.len() {
        let e = w.element().map_err(|e| format!("code at {:#x}: {e}", w.p))?;
        w.p = e.end;
        out.push(e);
    }
    if w.p != code.len() {
        return Err("the code ran past its end".into());
    }
    Ok(out)
}

// ---------- text ----------

const MESSAGE: u8 = 3;
const BREAK: u16 = 201;

/// `【name】` as it is written in front of a line.
static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^【([^】]*)】").unwrap());

/// What ends a message in the window: a wait for a click, or a new paragraph or page.
fn ends_message(kind: &Kind) -> bool {
    matches!(kind, Kind::Command(0, MESSAGE, 3 | 15 | 17 | 202 | 205 | 206 | 207 | 210))
}

/// Text with its quotes and separating commas gone.
fn unquoted(raw: &[u8]) -> Option<String> {
    if raw.windows(9).any(|w| w == b"###PRINT(") {
        return None; // a value the game fills in, which the model can't be handed
    }
    let (mut out, mut quoted, mut i) = (Vec::with_capacity(raw.len()), false, 0);
    while i < raw.len() {
        let c = raw[i];
        if c == b'"' {
            quoted = !quoted;
            i += 1;
        } else if !quoted && c == b',' {
            i += 1;
        } else if quoted && c == b'\\' && raw.get(i + 1) == Some(&b'"') {
            out.push(b'"');
            i += 2;
        } else {
            let n = char_len(c).min(raw.len() - i);
            out.extend(&raw[i..i + n]);
            i += n;
        }
    }
    Some(SHIFT_JIS.decode_without_bom_handling(&out).0.into_owned())
}

/// One message: where its pieces of text are, and any line breaks between them.
struct Message {
    pieces: Vec<usize>,
    breaks: Vec<usize>,
    speaker: Option<String>,
    text: String,
}

fn messages(code: &[u8], elements: &[Element]) -> Vec<Message> {
    let (mut out, mut pieces, mut breaks) = (Vec::new(), Vec::new(), Vec::new());
    let mut finish = |pieces: &mut Vec<usize>, breaks: &mut Vec<usize>| {
        let joined: Option<Vec<String>> = pieces.iter().map(|&i: &usize| unquoted(&code[elements[i].at..elements[i].end])).collect();
        let (taken, between) = (std::mem::take(pieces), std::mem::take(breaks));
        let Some(text) = joined.map(|j| j.concat()).filter(|t| JA.is_match(t)) else { return };
        let speaker = NAME.captures(&text).map(|c| c[1].to_string());
        let body = NAME.replace(&text, "").into_owned();
        out.push(Message { pieces: taken, breaks: between, speaker, text: body });
    };
    for (i, e) in elements.iter().enumerate() {
        match &e.kind {
            Kind::Text => pieces.push(i),
            Kind::Command(0, MESSAGE, BREAK) if !pieces.is_empty() => breaks.push(i),
            k if ends_message(k) || !e.jumps.is_empty() || !e.options.is_empty() => finish(&mut pieces, &mut breaks),
            _ => {}
        }
    }
    finish(&mut pieces, &mut breaks);
    out
}

/// English as the code holds it: quoted, so that nothing in it reads as code.
fn quoted(text: &str) -> Vec<u8> {
    let mut out = b"\"".to_vec();
    out.extend(sjis(&text.replace('\\', "").replace('"', "\\\"")));
    out.push(b'"');
    out
}

/// Where the second-level key of compilers 110002 and 1110002 lies in the unpacked code.
const KEYED: std::ops::Range<usize> = 256..1024;

/// Whether a scene's compiler XORs part of its code a second time, with a key of the game's.
fn second_level(b: &[u8]) -> R<bool> {
    match word(b, 4)? {
        10002 => Ok(false),
        110002 | 1110002 => Ok(true),
        _ => Err("this RealLive compiler's scenes aren't supported yet".into()),
    }
}

/// Undoes, or puts back, the second-level XOR: one key byte for each place in `KEYED`.
fn apply_key(code: &mut [u8], key: &[u8]) {
    for (c, k) in code.iter_mut().skip(KEYED.start).zip(key) {
        *c ^= k;
    }
}

/// The second-level key, worked out from the scenes' code: past the keyed part, code bytes have a
/// steady spread, so each keyed place gets the XOR that makes its bytes across all scenes look most
/// like that. Ported from CrySirror's `derive_xor2_key` (MPL-2.0).
fn derive_key(codes: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut counts = [1f64; 256];
    codes.iter().flat_map(|c| c.iter().skip(KEYED.end)).for_each(|&b| counts[b as usize] += 1.0);
    let total: f64 = counts.iter().sum();
    if total < 10_000.0 {
        return None;
    }
    let log_p = counts.map(|c| (c / total).ln());
    let mut key = vec![0u8; KEYED.len()];
    for pos in KEYED {
        let samples: Vec<u8> = codes.iter().filter_map(|c| c.get(pos).copied()).collect();
        if samples.len() < 16 {
            break;
        }
        let score = |x: u8| samples.iter().map(|&s| log_p[(s ^ x) as usize]).sum::<f64>();
        let best = (0..=255u8).max_by(|&a, &b| score(a).total_cmp(&score(b)))?;
        if best != 0 && score(best) - score(0) > 0.5 * samples.len() as f64 {
            key[pos - KEYED.start] = best;
        }
    }
    if key.iter().all(|&k| k == 0) {
        return None;
    }
    // the key repeats every 16 bytes, so a place between two that agree takes their value
    for i in 16..key.len() - 16 {
        if key[i - 16] == key[i + 16] && key[i] != key[i - 16] {
            key[i] = key[i - 16];
        }
    }
    Some(key)
}

/// A scene: its header and tables, unpacked code and elements.
struct Scene {
    number: usize,
    head: Vec<u8>,
    code: Vec<u8>,
    elements: Vec<Element>,
    messages: Vec<Message>,
    keyed: bool,
    /// Read from its own SEENnnnn.TXT, which the engine takes over SEEN.TXT's.
    loose: bool,
}

impl Scene {
    /// The code as stored, under the fixed mask only.
    fn packed_code(b: &[u8]) -> R<Vec<u8>> {
        if word(b, 0)? != HEADER {
            return Err("not a RealLive scene".into());
        }
        let (at, size) = (word(b, 0x20)?, word(b, 0x24)?);
        unpack(b.get(at..at + word(b, 0x28)?).ok_or("bad RealLive scene")?, size)
    }

    fn read(number: usize, b: &[u8], key: &[u8]) -> R<Scene> {
        let mut code = Self::packed_code(b)?;
        let keyed = second_level(b)?;
        if keyed {
            if key.is_empty() {
                return Err("this game's second-level key couldn't be worked out".into());
            }
            apply_key(&mut code, key);
        }
        let elements = walk(&code)?;
        let messages = messages(&code, &elements);
        Ok(Scene { number, head: b[..word(b, 0x20)?].to_vec(), code, elements, messages, keyed, loose: false })
    }

    fn options(&self) -> Vec<(usize, usize, String)> {
        let mut out = Vec::new();
        for (i, e) in self.elements.iter().enumerate() {
            for (n, &(from, to)) in e.options.iter().enumerate() {
                if let Some(text) = unquoted(&self.code[from..to]).filter(|t| JA.is_match(t)) {
                    out.push((i, n, text));
                }
            }
        }
        out
    }

    fn lines(&self, out: &mut Vec<Line>) {
        for m in &self.messages {
            out.push((format!("{}.{}", self.number, m.pieces[0]), m.speaker.clone(), m.text.clone()));
        }
        for (i, n, text) in self.options() {
            out.push((format!("{}.{i}.{n}", self.number), None, text));
        }
    }

    /// The scene as a file, with translations in and every jump put right.
    fn with(&self, width: usize, key: &[u8], line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<(Vec<u8>, usize)> {
        let mut new: HashMap<usize, Vec<u8>> = HashMap::new();
        let mut done = 0;
        for m in &self.messages {
            let Some(tr) = line(&format!("{}.{}", self.number, m.pieces[0])) else { continue };
            let who = m.speaker.as_ref().and_then(|s| name(s).or(Some(s.clone())));
            let lines = wrap_words(&ascii_punctuation(&tr), width);
            let mut bytes = quoted(&who.map_or(String::new(), |w| format!("【{w}】")));
            for (i, l) in lines.iter().enumerate() {
                if i > 0 {
                    bytes.extend(b"#\0\x03\xc9\0\0\0\0"); // a line break
                }
                bytes.extend(quoted(l));
            }
            new.insert(m.pieces[0], bytes);
            m.pieces[1..].iter().chain(&m.breaks).for_each(|&i| _ = new.insert(i, Vec::new()));
            done += 1;
        }
        let mut chosen: HashMap<(usize, usize), Vec<u8>> = HashMap::new();
        for (i, n, _) in self.options() {
            if let Some(tr) = line(&format!("{}.{i}.{n}", self.number)) {
                chosen.insert((i, n), quoted(&ascii_punctuation(&tr)));
                done += 1;
            }
        }
        if done == 0 {
            return Ok((Vec::new(), 0));
        }
        let (mut code, mut moved, mut patches) = (Vec::with_capacity(self.code.len() * 2), HashMap::new(), Vec::new());
        for (i, e) in self.elements.iter().enumerate() {
            moved.insert(e.at, code.len());
            if let Some(bytes) = new.get(&i) {
                code.extend(bytes);
                continue;
            }
            let start = code.len();
            let mut from = e.at;
            for (n, &(a, b)) in e.options.iter().enumerate() {
                if let Some(bytes) = chosen.get(&(i, n)) {
                    code.extend(&self.code[from..a]);
                    code.extend(bytes);
                    from = b;
                }
            }
            code.extend(&self.code[from..e.end]);
            for &j in &e.jumps {
                patches.push((start + j - e.at, word(&self.code, j)?));
            }
        }
        moved.insert(self.code.len(), code.len());
        for (at, old) in patches {
            let target = *moved.get(&old).ok_or(format!("a jump in scene {} lands inside an instruction", self.number))?;
            code[at..at + 4].copy_from_slice(&(target as u32).to_le_bytes());
        }
        if self.keyed {
            apply_key(&mut code, key);
        }
        let packed = stored(&code);
        let mut out = self.head.clone();
        out[0x24..0x28].copy_from_slice(&(code.len() as u32).to_le_bytes());
        out[0x28..0x2c].copy_from_slice(&(packed.len() as u32).to_le_bytes());
        out.extend(packed);
        Ok((out, done))
    }
}

// ---------- the game ----------

pub struct Game {
    archive: Vec<u8>,
    scenes: Vec<Scene>,
    /// The second-level key, empty when no scene needs one.
    key: Vec<u8>,
    width: usize,
    /// Gameexe.ini, and where the words a player reads are in it.
    settings: String,
    shown: Vec<(usize, usize)>,
}

// ---------- settings ----------

/// Settings a player reads: the right-click menu and the save, load and quit dialogs. Others name
/// things the scripts look up, as `#NAMAE`'s names and `#KOEONOFF`'s voices.
static SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#(?:SYSCOM\.|SAVE_NODATA|SYSTEM_SAVELOADMESSAGE_STR|SAVEMESSAGE_|LOADMESSAGE_|DLGSAVEMESSAGE_|DLGLOADMESSAGE_|GAME_END_MESS_STR)").unwrap());
static NOT_SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new("$^").unwrap());

fn config_key(start: usize) -> String {
    format!("settings:{start}")
}

/// A scene the game keeps on its own beside SEEN.TXT.
fn loose_name(i: usize) -> String {
    format!("SEEN{i:04}.TXT")
}

fn toc(b: &[u8], i: usize) -> R<(usize, usize)> {
    Ok((word(b, 8 * i)?, word(b, 8 * i + 4)?))
}

// shortcut: for a game whose settings don't say; RealLive's windows are usually about this size
const LINE_LETTERS: usize = 48;

/// How many letters fit a line, from `#WINDOW.000.MOJI_CNT=24,3`.
fn window(game: &Path) -> usize {
    static MOJI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?im)^#WINDOW\.000\.MOJI_CNT\s*=\s*(\d+)").unwrap());
    let text = std::fs::read(game.join(CONFIG)).map(|b| SHIFT_JIS.decode_without_bom_handling(&b).0.into_owned()).unwrap_or_default();
    crate::fit::letters(game, MOJI.captures(&text).and_then(|c| c[1].parse::<usize>().ok()).map_or(LINE_LETTERS, |n| 2 * n))
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let archive = original(&game.join(ARCHIVE))?;
        let mut stored = Vec::new();
        for i in 0..SCENES {
            let loose = game.join(loose_name(i));
            if loose.is_file() {
                stored.push((i, original(&loose)?, true));
                continue;
            }
            let (at, size) = toc(&archive, i)?;
            if at != 0 {
                stored.push((i, archive.get(at..at + size).ok_or("bad SEEN.TXT index")?.to_vec(), false));
            }
        }
        let mut key = Vec::new();
        if stored.iter().any(|(_, b, _)| second_level(b).unwrap_or(false)) {
            // shortcut: the key is worked out from up to 128 MB of code, plenty for any game seen
            let (mut codes, mut size) = (Vec::new(), 0);
            for (_, b, _) in &stored {
                if size < 128 << 20 && let Ok(c) = Scene::packed_code(b) {
                    size += c.len();
                    codes.push(c);
                }
            }
            key = derive_key(&codes).unwrap_or_default();
        }
        let mut scenes = Vec::new();
        for (i, b, loose) in &stored {
            let mut s = Scene::read(*i, b, &key).map_err(|e| format!("scene {i}: {e}"))?;
            s.loose = *loose;
            scenes.push(s);
        }
        let settings = original(&game.join(CONFIG)).map(|b| SHIFT_JIS.decode_without_bom_handling(&b).0.into_owned()).unwrap_or_default();
        let shown = shown_strings(std::slice::from_ref(&settings), &GAMEEXE, &SHOWN, &NOT_SHOWN).into_iter().map(|s| (s.1, s.2)).collect();
        Ok(Game { archive, scenes, key, width: window(game), settings, shown })
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scenes.iter().for_each(|s| s.lines(&mut out));
        out.extend(self.shown.iter().map(|&(start, end)| (config_key(start), None, self.settings[start..end].to_string())));
        out
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into SEEN.TXT and reads it
/// back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let mut out = vec![0; 8 * SCENES];
    let mut done = 0;
    let mut loose = Vec::new();
    for s in &g.scenes {
        let (new, n) = s.with(g.width, &g.key, line, name)?;
        done += n;
        if n > 0 {
            Scene::read(s.number, &new, &g.key).map_err(|e| format!("scene {} didn't read back: {e}", s.number))?;
        }
        if s.loose {
            if n > 0 {
                loose.push((s.number, new));
            }
            continue;
        }
        let (at, size) = toc(&g.archive, s.number)?;
        let bytes = if n > 0 { new.as_slice() } else { &g.archive[at..at + size] };
        let place = out.len() as u32;
        out[8 * s.number..8 * s.number + 4].copy_from_slice(&place.to_le_bytes());
        out[8 * s.number + 4..8 * s.number + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend(bytes);
    }
    // a scene in SEEN.TXT that a loose file overrides stays as the game has it
    for n in (0..SCENES).filter(|&n| g.scenes.iter().any(|s| s.number == n && s.loose)) {
        let (at, size) = toc(&g.archive, n)?;
        if at != 0 {
            let place = out.len() as u32;
            out[8 * n..8 * n + 4].copy_from_slice(&place.to_le_bytes());
            out[8 * n + 4..8 * n + 8].copy_from_slice(&(size as u32).to_le_bytes());
            out.extend(&g.archive[at..at + size]);
        }
    }
    replace_file(&game.join(ARCHIVE), &out)?;
    for (n, bytes) in loose {
        replace_file(&game.join(loose_name(n)), &bytes)?;
    }
    let (settings, words) = with_english(&g.settings, &g.shown, &|start, _| line(&config_key(start)).map(|t| ascii_punctuation(&t)));
    if words > 0 {
        let (bytes, _, unmapped) = SHIFT_JIS.encode(&settings);
        if unmapped || SHIFT_JIS.decode_without_bom_handling(&bytes).0 != settings {
            return Err("Gameexe.ini didn't read back as written".into());
        }
        replace_file(&game.join(CONFIG), &bytes)?;
    }
    Ok(done + words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn line_mark(n: u16) -> Vec<u8> {
        [&b"\n"[..], &n.to_le_bytes()].concat()
    }

    fn command(module: u8, opcode: u16, argc: u16) -> Vec<u8> {
        [&[b'#', 0, module][..], &opcode.to_le_bytes(), &argc.to_le_bytes(), &[0]].concat()
    }

    /// A jump over a spoken message split by a line break, then a choice.
    fn code() -> Vec<u8> {
        let mut c = command(1, 0, 0);
        let target = c.len();
        c.extend([0; 4]);
        let spoken = sjis("【太郎】「こんに");
        c.extend(&spoken);
        c.extend(command(MESSAGE, BREAK, 0));
        c.extend(sjis("ちは」"));
        c.extend(command(MESSAGE, 17, 0));
        let landing = c.len();
        c[target..target + 4].copy_from_slice(&(landing as u32).to_le_bytes());
        c.extend(line_mark(5));
        c.extend(sjis("地の文です"));
        c.extend(command(MESSAGE, 17, 0));
        c.extend(command(2, 1, 2));
        c.push(b'{');
        c.extend(line_mark(6));
        c.extend(sjis("はい"));
        c.extend(line_mark(7));
        c.extend(sjis("いいえ"));
        c.extend(line_mark(8));
        c.extend(line_mark(9));
        c.push(b'}');
        c
    }

    fn scene(code: &[u8]) -> Vec<u8> {
        scene_by(10002, code)
    }

    fn scene_by(compiler: usize, code: &[u8]) -> Vec<u8> {
        let mut b = vec![0; HEADER];
        let packed = stored(code);
        for (at, v) in [(0, HEADER), (4, compiler), (0x20, HEADER), (0x24, code.len()), (0x28, packed.len())] {
            b[at..at + 4].copy_from_slice(&(v as u32).to_le_bytes());
        }
        b.extend(packed);
        b
    }

    fn game(scenes: &[(usize, Vec<u8>)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-reallive-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut out = vec![0; 8 * SCENES];
        for (n, b) in scenes {
            let place = out.len() as u32;
            out[8 * n..8 * n + 4].copy_from_slice(&place.to_le_bytes());
            out[8 * n + 4..8 * n + 8].copy_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend(b);
        }
        fs::write(dir.join(ARCHIVE), out).unwrap();
        dir
    }

    #[test]
    fn the_menu_and_dialogs_in_the_settings_change_and_names_stay() {
        let dir = game(&[(20, scene(&sjis("ASCII only")))]);
        let ini = "#SYSCOM.000=U:\"セーブ\"\n#NAMAE.000=\"太郎\"\n#SAVEMESSAGE_MESS_STR=\"セーブしますか？\"\n";
        fs::write(dir.join(CONFIG), SHIFT_JIS.encode(ini).0).unwrap();
        let lines = Game::open(&dir).unwrap().lines();
        let texts: Vec<&str> = lines.iter().map(|l| l.2.as_str()).collect();
        assert_eq!(texts, ["セーブ", "セーブしますか？"]);
        let english: HashMap<String, &str> = lines.iter().map(|l| l.0.clone()).zip(["Save", "Save the \"game\"?"]).collect();
        assert_eq!(install(&dir, &|k| english.get(k).map(|s| s.to_string()), &|_| None).unwrap(), 2);
        let written = SHIFT_JIS.decode(&fs::read(dir.join(CONFIG)).unwrap()).0.into_owned();
        assert_eq!(written, "#SYSCOM.000=U:\"Save\"\n#NAMAE.000=\"太郎\"\n#SAVEMESSAGE_MESS_STR=\"Save the 'game'?\"\n");
        assert_eq!(Game::open(&dir).unwrap().lines().len(), 2, "read from the kept original");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn packed_code_comes_back() {
        let plain = b"some code, more than eight bytes".to_vec();
        let p = stored(&plain);
        assert_eq!(unpack(&p, plain.len()).unwrap(), plain);
    }

    #[test]
    fn the_walk_finds_messages_and_options() {
        let c = code();
        let e = walk(&c).unwrap();
        let m = messages(&c, &e);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].speaker.as_deref(), Some("太郎"));
        assert_eq!(m[0].text, "「こんにちは」");
        assert_eq!(m[0].pieces.len(), 2);
        assert_eq!(m[0].breaks.len(), 1);
        assert_eq!(m[1].text, "地の文です");
        assert_eq!(e.iter().flat_map(|x| &x.options).count(), 2);
    }

    #[test]
    fn a_value_the_game_fills_in_keeps_its_line_japanese() {
        assert_eq!(unquoted(b"###PRINT($\x12[$\xff\0\0\0\0])"), None);
        assert_eq!(unquoted(b"\"a,b\",c"), Some("a,bc".to_string()));
    }

    #[test]
    fn translations_move_the_code_and_jumps_follow() {
        let dir = game(&[(10, scene(&code())), (20, scene(&sjis("ASCII only")))]);
        let g = Game::open(&dir).unwrap();
        let lines = g.lines();
        assert_eq!(lines.len(), 4);
        let keys: Vec<String> = lines.iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.iter().position(|x| x == k).map(|i| ["\"Hello there, friend.\"", "It says \"hi\".", "Yes", "No"][i].to_string());
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 4);

        let out = fs::read(dir.join(ARCHIVE)).unwrap();
        let (at, size) = toc(&out, 10).unwrap();
        let s = Scene::read(10, &out[at..at + size], &[]).unwrap();
        let text = String::from_utf8_lossy(&s.code).into_owned();
        assert!(text.contains("Hello there") && text.contains("Taro") && text.contains("It says \\\"hi\\\"."));
        assert!(text.contains("\"Yes\"\n") && !text.contains("ASCII"));
        // the jump still lands on the line mark in front of the second message
        let target = word(&s.code, 8).unwrap();
        assert_eq!(&s.code[target..target + 3], &line_mark(5)[..]);
        let (at, size) = toc(&out, 20).unwrap();
        assert_eq!(Scene::read(20, &out[at..at + size], &[]).unwrap().code, sjis("ASCII only"));
    }

    #[test]
    fn the_second_level_key_is_worked_out_from_the_scenes() {
        let key: Vec<u8> = (0..16).map(|i| 0x31 + i * 7).collect();
        let mut seed = 7u32;
        let codes: Vec<Vec<u8>> = (0..24).map(|_| (0..3000).map(|_| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            [b'"', 0, 1, b'#', 0x82, 0xa0, b'a', 0x0a][(seed >> 16) as usize % 8]
        }).collect()).collect();
        let keyed: Vec<Vec<u8>> = codes.iter().map(|c| {
            let mut c = c.clone();
            (256..513).for_each(|i| c[i] ^= key[i % 16]);
            c
        }).collect();
        let found = derive_key(&keyed).unwrap();
        assert!((0..257).all(|i| found[i] == key[i % 16]), "the keyed part");
        assert!(found[257..].iter().all(|&k| k == 0), "and nothing past it");
        let mut back = keyed[0].clone();
        apply_key(&mut back, &found);
        assert_eq!(back, codes[0]);
    }

    #[test]
    fn a_scene_of_a_newer_compiler_comes_back_under_its_key() {
        let key: Vec<u8> = (0..KEYED.len()).map(|i| (i * 13 + 5) as u8).collect();
        let mut plain = code();
        plain.extend(sjis(&"padding ".repeat(80)));
        let mut stored_code = plain.clone();
        apply_key(&mut stored_code, &key);
        let s = Scene::read(3, &scene_by(110002, &stored_code), &key).unwrap();
        assert_eq!(s.code, plain);
        assert!(Scene::read(3, &scene_by(110002, &stored_code), &[]).is_err(), "no key, no reading");
        let (new, n) = s.with(48, &key, &|_| Some("Yes".into()), &|_| None).unwrap();
        assert!(n > 0);
        let back = Scene::read(3, &new, &key).unwrap();
        assert!(String::from_utf8_lossy(&back.code).contains("\"Yes\""));
        assert_eq!(word(&new, 4).unwrap(), 110002, "the compiler stays");
    }

    #[test]
    fn a_loose_scene_file_is_read_and_written_in_place_of_seen_txt() {
        let dir = game(&[(10, scene(&sjis("ASCII only")))]);
        fs::write(dir.join("SEEN0010.TXT"), scene(&code())).unwrap();
        let lines = Game::open(&dir).unwrap().lines();
        assert_eq!(lines.len(), 4, "the loose one is what the engine shows");
        let own = fs::read(dir.join(ARCHIVE)).unwrap();
        install(&dir, &|_| Some("Yes".into()), &|_| None).unwrap();
        let loose = fs::read(dir.join("SEEN0010.TXT")).unwrap();
        assert!(String::from_utf8_lossy(&Scene::read(10, &loose, &[]).unwrap().code).contains("\"Yes\""));
        let out = fs::read(dir.join(ARCHIVE)).unwrap();
        let ((a, n), (b, m)) = (toc(&out, 10).unwrap(), toc(&own, 10).unwrap());
        assert_eq!(out[a..a + n], own[b..b + m], "SEEN.TXT's copy stays as it was");
        fs::remove_dir_all(&dir).unwrap();
    }
}
