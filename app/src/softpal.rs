//! Softpal engine (data.pac with SCRIPT.SRC, TEXT.DAT and POINT.DAT): code and text are apart, and the
//! code pushes a text's offset before calling the message function. New text goes on the end of
//! TEXT.DAT and only those offsets change, so nothing in the code moves.

use crate::kirikiri::box_width;
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, sjis, text_units, wrap_words, JA};
use crate::{original, replace_file, R};
use encoding_rs::SHIFT_JIS;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

const ARCHIVE: &str = "data.pac";
const INDEX: usize = 0x3FE;
const CODE: usize = 0xC;
/// shortcut: a guess for games with too few rows broken by hand; change it if lines break inside words.
const LINE_LETTERS: usize = 56;
const BREAK: &str = "<br>";
const KEY: u32 = 0x084D_F873 ^ 0xFF98_7DEE;

const MOV: u32 = 0x01;
const CALL: u32 = 0x0B;
const SYSCALL: u32 = 0x17;
const RET: u32 = 0x18;
const PUSH: u32 = 0x1F;
const ENTER: u32 = 0x20;
const ADD_CHOICE: u32 = 0x3A;
/// Syscall numbers of the choice and message functions.
const CHOICE_CALL: i32 = 0x60002;
const MESSAGE_CALLS: [i32; 6] = [0x20002, 0x2000F, 0x20010, 0x20011, 0x20012, 0x20013];
const LITERAL: u32 = 0;
const VARIABLE: u32 = 4;
const ARGUMENT: u32 = 8;

pub static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());

fn word(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}

/// TEXT.DAT past its header. A byte of each word turns by a growing amount, and the word is xored.
fn crypt(b: &mut [u8], encrypt: bool) {
    for (i, w) in b[16..].as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let turn = (4 + i as u32) % 8;
        if encrypt {
            *w = (u32::from_le_bytes(*w) ^ KEY).to_le_bytes();
            w[0] = w[0].rotate_right(turn);
        } else {
            w[0] = w[0].rotate_left(turn);
            *w = (u32::from_le_bytes(*w) ^ KEY).to_le_bytes();
        }
    }
}

// ---------- the code ----------

#[derive(Clone, Copy)]
struct Operand {
    at: usize,
    raw: i32,
}

impl Operand {
    fn kind(self) -> u32 {
        (self.raw as u32 >> 28) & 0xF
    }

    fn value(self) -> i32 {
        (self.raw << 4) >> 4
    }

    fn text(self) -> bool {
        self.kind() == LITERAL && self.value() >= 0
    }
}

fn operand_count(op: u32) -> usize {
    match op {
        0x01..=0x08 | 0x0A | 0x0C..=0x13 | 0x17 | 0x1A..=0x1C => 2,
        0x09 | 0x0B | 0x14 | 0x1D..=0x21 => 1,
        _ => 0,
    }
}

/// Runs `step` on each instruction: its offset, opcode and operands.
fn walk(code: &[u8], step: &mut dyn FnMut(usize, u32, &[Operand])) -> R<()> {
    let mut p = CODE;
    while p + 4 <= code.len() {
        let top = word(code, p);
        if top >> 16 != 1 {
            return Err(format!("SCRIPT.SRC: not an instruction at {p:#x}, so this Softpal version isn't supported."));
        }
        let op = top & 0xFFFF;
        let count = operand_count(op);
        if p + 4 + count * 4 > code.len() {
            return Err("SCRIPT.SRC ends inside an instruction.".into());
        }
        let operands: Vec<Operand> = (0..count).map(|i| Operand { at: p + 4 + i * 4, raw: word(code, p + 4 + i * 4) as i32 }).collect();
        step(p, op, &operands);
        p += 4 + count * 4;
    }
    Ok(())
}

fn is_message(op: u32, operands: &[Operand]) -> bool {
    matches!(op, 0x88 | 0x95..=0x99) || (op == SYSCALL && MESSAGE_CALLS.contains(&operands[0].raw))
}

/// What the code has pushed, following values just moved into variables.
#[derive(Default)]
struct Stack {
    items: Vec<Operand>,
    vars: HashMap<i32, Operand>,
}

impl Stack {
    fn clear(&mut self) {
        self.items.clear();
        self.vars.clear();
    }

    fn mov(&mut self, operands: &[Operand]) {
        if operands[0].kind() == VARIABLE {
            self.vars.insert(operands[0].value(), operands[1]);
        }
    }

    fn push(&mut self, o: Operand) {
        let known = (o.kind() == VARIABLE).then(|| self.vars.get(&o.value()).copied()).flatten();
        self.items.push(known.unwrap_or(o));
    }

    /// The message and name a message call just took, last pushed first.
    fn message(&mut self) -> Option<(Operand, Operand)> {
        (self.items.len() >= 4).then(|| {
            self.items.pop();
            let name = self.items.pop().unwrap();
            (self.items.pop().unwrap(), name)
        })
    }
}

/// A game's own functions that wrap the message call: where they start, then arguments, and which
/// are the name and the message.
fn message_functions(code: &[u8]) -> R<HashMap<usize, (usize, usize, usize)>> {
    let (mut found, mut stack, mut current) = (HashMap::new(), Stack::default(), None);
    walk(code, &mut |at, op, operands| {
        if is_message(op, operands) {
            if let (Some((start, args)), Some((message, name))) = (current, stack.message())
                && name.kind() == ARGUMENT && message.kind() == ARGUMENT {
                found.insert(start, (args, name.value() as usize - 1, message.value() as usize - 1));
                current = None;
            }
            stack.clear();
            return;
        }
        match op {
            ENTER => {
                current = Some((at, operands[0].value() as usize));
                stack.clear();
            }
            MOV => stack.mov(operands),
            PUSH => stack.push(operands[0]),
            RET => {
                current = None;
                stack.clear();
            }
            _ => stack.clear(),
        }
    })?;
    Ok(found)
}

/// `(message, name)` operands, in script order; a choice has no name.
fn messages(code: &[u8], points: &[usize]) -> R<Vec<(Operand, Option<Operand>)>> {
    let functions = message_functions(code)?;
    let (mut out, mut stack) = (Vec::new(), Stack::default());
    walk(code, &mut |_, op, operands| {
        if is_message(op, operands) {
            if let Some((message, name)) = stack.message()
                && name.kind() == LITERAL && message.kind() == LITERAL && message.text() {
                out.push((message, name.text().then_some(name)));
            }
            stack.clear();
            return;
        }
        match op {
            MOV => stack.mov(operands),
            PUSH => stack.push(operands[0]),
            CALL => {
                let target = (operands[0].kind() == LITERAL).then(|| points.get((operands[0].value() as usize).wrapping_sub(1))).flatten();
                if let Some(&(args, name, message)) = target.and_then(|t| functions.get(t))
                    && stack.items.len() >= args {
                    let taken = &stack.items[stack.items.len() - args..];
                    if taken[message].text() {
                        out.push((taken[message], taken[name].text().then_some(taken[name])));
                    }
                }
                stack.clear();
            }
            ADD_CHOICE => {
                out.extend(stack.items.pop().map(|c| (c, None)));
                stack.clear();
            }
            SYSCALL if operands[0].raw == CHOICE_CALL => {
                out.extend(stack.items.pop().map(|c| (c, None)));
                stack.clear();
            }
            SYSCALL => stack.items.clear(),
            _ => stack.clear(),
        }
    })?;
    Ok(out)
}

// ---------- the game ----------

fn bad() -> String {
    format!("{ARCHIVE} isn't a Softpal archive")
}

/// How long names are in an archive as the game shipped it: the first file starts where rows end.
fn name_size(b: &[u8]) -> R<usize> {
    let count = word(b.get(..4).ok_or_else(bad)?, 0) as usize;
    [0x20, 0x10].into_iter().find(|n| b.get(INDEX + n + 4..INDEX + n + 8).is_some_and(|w| word(w, 0) as usize == INDEX + count * (n + 8))).ok_or_else(bad)
}

/// `(where its size and offset sit, offset, size)` by file name.
fn rows(b: &[u8], name: usize) -> R<HashMap<String, (usize, usize, usize)>> {
    let count = word(b.get(..4).ok_or_else(bad)?, 0) as usize;
    let mut out = HashMap::new();
    for i in 0..count {
        let p = INDEX + i * (name + 8);
        let row = b.get(p..p + name + 8).ok_or_else(bad)?;
        let end = row[..name].iter().position(|&c| c == 0).unwrap_or(name);
        let (size, at) = (word(row, name) as usize, word(row, name + 4) as usize);
        if at + size > b.len() {
            return Err(bad());
        }
        out.insert(String::from_utf8_lossy(&row[..end]).to_uppercase(), (p + name, at, size));
    }
    Ok(out)
}

pub struct Game {
    code: Vec<u8>,
    text: Vec<u8>,
    messages: Vec<(Operand, Option<Operand>)>,
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let arc = original(&game.join(ARCHIVE))?;
        let rows = rows(&arc, name_size(&arc)?)?;
        let file = |name: &str| rows.get(name).map(|&(_, at, size)| arc[at..at + size].to_vec()).ok_or(format!("{ARCHIVE} has no {name}"));
        let (code, mut text, points) = (file("SCRIPT.SRC")?, file("TEXT.DAT")?, file("POINT.DAT")?);
        if !code.starts_with(b"Sv20") {
            return Err("SCRIPT.SRC is a Softpal version that isn't supported yet.".into());
        }
        if text.first() == Some(&b'$') {
            crypt(&mut text, false);
        }
        let mut points: Vec<usize> = points.get(16..).unwrap_or(&[]).as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w) as usize + CODE).collect();
        points.reverse();
        let messages = messages(&code, &points)?;
        Ok(Game { code, text, messages })
    }

    fn string(&self, o: Operand) -> String {
        let tail = self.text.get(o.value() as usize + 4..).unwrap_or(&[]);
        SHIFT_JIS.decode_without_bom_handling(&tail[..tail.iter().position(|&c| c == 0).unwrap_or(tail.len())]).0.into_owned()
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        for (message, name) in &self.messages {
            let text = self.string(*message).replace(BREAK, "");
            if JA.is_match(&text) {
                out.push((message.at.to_string(), name.map(|n| self.string(n)).filter(|n| JA.is_match(n)), text));
            }
        }
        out
    }

    /// The widest rows the writers broke by hand.
    fn width(&self) -> usize {
        let mut rows = Vec::new();
        for (message, _) in &self.messages {
            let text = self.string(*message);
            let parts: Vec<_> = text.split(BREAK).collect();
            rows.extend(parts[..parts.len() - 1].iter().filter(|r| JA.is_match(r)).map(|r| text_units(r)));
        }
        box_width(rows, LINE_LETTERS)
    }
}

/// A string on the end of TEXT.DAT, and the code's operand pointed at it.
fn put(code: &mut [u8], text: &mut Vec<u8>, o: Operand, s: &str) {
    code[o.at..o.at + 4].copy_from_slice(&(text.len() as u32).to_le_bytes());
    text.extend([0; 4]);
    text.extend(sjis(s));
    text.push(0);
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the archive and reads
/// them back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let width = crate::fit::letters(game, g.width());
    let (mut code, mut text, mut done) = (g.code.clone(), g.text.clone(), 0);
    for (message, who) in &g.messages {
        let Some(tr) = line(&message.at.to_string()) else { continue };
        put(&mut code, &mut text, *message, &wrap_words(&ascii_punctuation(&tr), width).join(BREAK));
        done += 1;
        if let Some((o, en)) = who.and_then(|o| Some((o, name(&g.string(o))?))) {
            put(&mut code, &mut text, o, &en);
            done += 1;
        }
    }
    text.resize(text.len().next_multiple_of(4), 0);
    crypt(&mut text, true);

    let path = game.join(ARCHIVE);
    let mut out = original(&path)?;
    let name_size = name_size(&out)?;
    let index = rows(&out, name_size)?;
    // archives end in a marker, which stays last
    let footer = out.ends_with(b"EOF ").then(|| out.split_off(out.len() - 4));
    for (file, bytes) in [("SCRIPT.SRC", &code), ("TEXT.DAT", &text)] {
        let row = index[file].0;
        let at = (out.len() as u32).to_le_bytes();
        out[row..row + 4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        out[row + 4..row + 8].copy_from_slice(&at);
        out.extend(bytes.iter());
    }
    out.extend(footer.unwrap_or_default());
    replace_file(&path, &out)?;

    let back = fs::read(&path).map_err(|e| e.to_string())?;
    let rows = rows(&back, name_size)?;
    for (file, bytes) in [("SCRIPT.SRC", &code), ("TEXT.DAT", &text)] {
        let (_, at, size) = rows[file];
        if back[at..at + size] != bytes[..] {
            return Err(format!("{file} didn't read back as written"));
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn op(code: &mut Vec<u8>, op: u32, operands: &[u32]) {
        code.extend((0x10000 | op).to_le_bytes());
        operands.iter().for_each(|v| code.extend(v.to_le_bytes()));
    }

    /// A push of a number, message, name and a number, then the message syscall.
    fn say(code: &mut Vec<u8>, message: u32, name: u32) {
        [0, message, name, 0].iter().for_each(|&v| op(code, PUSH, &[v]));
        op(code, SYSCALL, &[0x20002, 0]);
    }

    /// Text at three offsets, code that says two lines, a wrapper function and a choice.
    fn files() -> (Vec<u8>, Vec<u8>, Vec<u8>, [u32; 5]) {
        let mut text = b"$TEXT_LIST__\0\0\0\0".to_vec();
        let mut at = [0; 5];
        for (i, t) in ["太郎", "こんにちは<br>元気です", "地の文です", "選択肢", "関数の文"].iter().enumerate() {
            at[i] = text.len() as u32;
            text.extend([0; 4]);
            text.extend(sjis(t));
            text.push(0);
        }
        text.resize(text.len().next_multiple_of(4), 0);
        crypt(&mut text, true);

        let mut code = b"Sv20\0\0\0\0\0\0\0\0".to_vec();
        say(&mut code, at[1], at[0]);
        say(&mut code, at[2], 0x0FFF_FFFF);
        op(&mut code, PUSH, &[at[3]]);
        op(&mut code, ADD_CHOICE, &[]);
        // the wrapper: `(number, name, message)`
        let function = (code.len() - CODE) as u32;
        op(&mut code, ENTER, &[3]);
        op(&mut code, PUSH, &[0]);
        [3u32, 2, 1].iter().for_each(|&v| op(&mut code, PUSH, &[ARGUMENT << 28 | v]));
        op(&mut code, 0x88, &[]);
        op(&mut code, RET, &[]);
        [0, at[0], at[4]].iter().for_each(|&v| op(&mut code, PUSH, &[v]));
        op(&mut code, CALL, &[1]);

        let mut points = b"$POINT_LIST_****".to_vec();
        points.extend(function.to_le_bytes());
        (code, text, points, at)
    }

    fn pac(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = (files.len() as u32).to_le_bytes().to_vec();
        out.resize(INDEX, 0);
        let mut at = INDEX + files.len() * 0x28;
        for (name, b) in files {
            let mut row = [0; 0x28];
            row[..name.len()].copy_from_slice(name.as_bytes());
            row[0x20..0x24].copy_from_slice(&(b.len() as u32).to_le_bytes());
            row[0x24..0x28].copy_from_slice(&(at as u32).to_le_bytes());
            out.extend(row);
            at += b.len();
        }
        files.iter().for_each(|(_, b)| out.extend(b));
        out
    }

    fn game() -> PathBuf {
        let (code, text, points, _) = files();
        let dir = std::env::temp_dir().join(format!("vntl-softpal-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(ARCHIVE), pac(&[("SCRIPT.SRC", code), ("TEXT.DAT", text), ("POINT.DAT", points), ("TITLE.ANI", b"other".to_vec())])).unwrap();
        dir
    }

    #[test]
    fn encryption_turns_back() {
        let mut b = b"$TEXT_LIST__0000some text words!".to_vec();
        let plain = b.clone();
        crypt(&mut b, true);
        assert_ne!(b, plain);
        assert_eq!(b[..16], plain[..16]);
        crypt(&mut b, false);
        assert_eq!(b, plain);
    }

    #[test]
    fn messages_choices_and_wrapped_messages_are_found() {
        let dir = game();
        let lines = Game::open(&dir).unwrap().lines();
        let shown: Vec<_> = lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect();
        assert_eq!(shown, [(Some("太郎"), "こんにちは元気です"), (None, "地の文です"), (None, "選択肢"), (Some("太郎"), "関数の文")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn translations_go_on_the_end_of_the_text_and_read_back() {
        let dir = game();
        let path = dir.join(ARCHIVE);
        let keys: Vec<_> = Game::open(&dir).unwrap().lines().iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.iter().position(|x| x == k).map(|i| format!("English line {i}"));
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 6);

        let lines = Game::open_written(&dir);
        let shown: Vec<_> = lines.iter().map(|l| l.as_str()).collect();
        assert_eq!(shown, ["Taro: English line 0", "English line 1", "English line 2", "Taro: English line 3"]);
        let b = fs::read(&path).unwrap();
        let r = rows(&b, 0x20).unwrap();
        assert_eq!(&b[r["TITLE.ANI"].1..r["TITLE.ANI"].1 + 5], b"other");
        install(&dir, &line, &name).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b);
        let _ = fs::remove_dir_all(dir);
    }

    impl Game {
        /// The archive as written, read the way a player's copy would be.
        fn open_written(dir: &Path) -> Vec<String> {
            let arc = fs::read(dir.join(ARCHIVE)).unwrap();
            let rows = rows(&arc, 0x20).unwrap();
            let file = |n: &str| arc[rows[n].1..rows[n].1 + rows[n].2].to_vec();
            let (code, mut text, points) = (file("SCRIPT.SRC"), file("TEXT.DAT"), file("POINT.DAT"));
            crypt(&mut text, false);
            let mut points: Vec<usize> = points[16..].as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w) as usize + CODE).collect();
            points.reverse();
            let messages = messages(&code, &points).unwrap();
            let g = Game { code, text, messages };
            g.messages.iter().map(|(m, n)| match n {
                Some(n) => format!("{}: {}", g.string(*n), g.string(*m)),
                None => g.string(*m),
            }).collect()
        }
    }
}
