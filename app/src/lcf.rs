//! RPG Maker 2000 and 2003 (RPG_RT.ldb, Map*.lmu): nested chunks of `id, size, data`, Shift-JIS text.
//! Sizes change with the text, so each file is taken apart along the paths that hold text and put
//! back together. A file that doesn't come back byte for byte when nothing changed is refused.

use crate::text::decoded;
use crate::rpgmaker::{reflow, Text, Visit, DESCRIPTION, NAME, STORY, TERM};
use crate::text::{ascii_punctuation, sjis, JA};
use crate::{original, replace_file, R};
use std::fs;
use std::path::{Path, PathBuf};

const MESSAGE_LETTERS: usize = 50;
const FACE_LETTERS: usize = 38;
const PAGE_ROWS: usize = 4;

const SHOW_MESSAGE: u32 = 10110;
const MORE_MESSAGE: u32 = 20110;
const SET_FACE: u32 = 10130;
const SHOW_CHOICES: u32 = 10140;
const CHOICE: u32 = 20140;
const CHANGE_NAME: u32 = 10610;
const CHANGE_TITLE: u32 = 10620;

// ---------- the format ----------

/// A number in sevens, high bits first.
fn number(b: &[u8], p: &mut usize) -> R<u32> {
    let mut v = 0u32;
    loop {
        let c = *b.get(*p).ok_or("an RPG Maker file ends early")?;
        *p += 1;
        v = (v << 7) | (c & 0x7F) as u32;
        if c < 0x80 {
            return Ok(v);
        }
    }
}

fn put_number(out: &mut Vec<u8>, v: u32) {
    let mut bytes = vec![(v & 0x7F) as u8];
    let mut rest = v >> 7;
    while rest > 0 {
        bytes.push((rest & 0x7F) as u8 | 0x80);
        rest >>= 7;
    }
    out.extend(bytes.iter().rev());
}

struct Chunk {
    id: u32,
    data: Vec<u8>,
}

/// Chunks up to the zero that ends them, or the end of `b`.
fn chunks(b: &[u8], p: &mut usize) -> R<Vec<Chunk>> {
    Ok(chunks_and_end(b, p)?.0)
}

/// The same, and whether there was a zero: a database has none at its very end.
fn chunks_and_end(b: &[u8], p: &mut usize) -> R<(Vec<Chunk>, bool)> {
    let mut out = Vec::new();
    while *p < b.len() {
        let id = number(b, p)?;
        if id == 0 {
            return Ok((out, true));
        }
        let size = number(b, p)? as usize;
        let data = b.get(*p..*p + size).ok_or("an RPG Maker chunk runs past its file")?.to_vec();
        *p += size;
        out.push(Chunk { id, data });
    }
    Ok((out, false))
}

fn put_chunks(out: &mut Vec<u8>, list: &[Chunk]) {
    for c in list {
        put_number(out, c.id);
        put_number(out, c.data.len() as u32);
        out.extend(&c.data);
    }
    out.push(0);
}

/// Numbered rows of chunks, like a database list.
fn rows(b: &[u8]) -> R<Vec<(u32, Vec<Chunk>)>> {
    let mut p = 0;
    let mut out = Vec::new();
    for _ in 0..number(b, &mut p)? {
        let id = number(b, &mut p)?;
        out.push((id, chunks(b, &mut p)?));
    }
    Ok(out)
}

fn put_rows(list: &[(u32, Vec<Chunk>)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_number(&mut out, list.len() as u32);
    for (id, row) in list {
        put_number(&mut out, *id);
        put_chunks(&mut out, row);
    }
    out
}

/// Changes the rows held in chunk `id` of `list`, if there is one.
fn in_rows(list: &mut [Chunk], id: u32, change: &mut dyn FnMut(&mut Vec<Chunk>) -> R<()>) -> R<()> {
    if let Some(c) = list.iter_mut().find(|c| c.id == id) {
        let mut all = rows(&c.data)?;
        all.iter_mut().try_for_each(|(_, row)| change(row))?;
        c.data = put_rows(&all);
    }
    Ok(())
}

struct Command {
    code: u32,
    indent: u32,
    text: Vec<u8>,
    args: Vec<u32>,
}

fn commands(b: &[u8]) -> R<Vec<Command>> {
    let (mut p, mut out) = (0, Vec::new());
    while p < b.len() {
        let (code, indent) = (number(b, &mut p)?, number(b, &mut p)?);
        let size = number(b, &mut p)? as usize;
        let text = b.get(p..p + size).ok_or("an RPG Maker command runs past its list")?.to_vec();
        p += size;
        let count = number(b, &mut p)?;
        let args = (0..count).map(|_| number(b, &mut p)).collect::<R<Vec<u32>>>()?;
        out.push(Command { code, indent, text, args });
    }
    Ok(out)
}

fn put_commands(list: &[Command]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in list {
        put_number(&mut out, c.code);
        put_number(&mut out, c.indent);
        put_number(&mut out, c.text.len() as u32);
        out.extend(&c.text);
        put_number(&mut out, c.args.len() as u32);
        c.args.iter().for_each(|&a| put_number(&mut out, a));
    }
    out
}

// ---------- the text ----------

fn encode(text: &str) -> Vec<u8> {
    sjis(&ascii_punctuation(text))
}

/// A field that's a string: asks `f` for its translation.
fn field(data: &mut Vec<u8>, place: &str, f: &mut Visit) {
    let text = decoded(data);
    if JA.is_match(&text)
        && let Some(tr) = f(place, "", &text) {
        *data = encode(&tr);
    }
}

fn text_field(list: &mut [Chunk], id: u32, place: &str, f: &mut Visit) {
    if let Some(c) = list.iter_mut().find(|c| c.id == id) {
        field(&mut c.data, place, f);
    }
}

/// An event's commands: a message's rows become one text and are broken again for English, on as
/// many pages as that takes.
fn events(old: Vec<Command>, letters: usize, f: &mut Visit) -> Vec<Command> {
    let (mut out, mut face) = (Vec::with_capacity(old.len()), false);
    let mut list = old.into_iter().peekable();
    while let Some(mut c) = list.next() {
        match c.code {
            SHOW_MESSAGE | MORE_MESSAGE => {
                let (indent, args) = (c.indent, c.args.clone());
                let mut rows = vec![std::mem::take(&mut c.text)];
                while let Some(n) = list.next_if(|n| n.code == MORE_MESSAGE) {
                    rows.push(n.text);
                }
                let text: Vec<String> = rows.iter().map(|r| decoded(r)).collect();
                let lines: Vec<&str> = text.iter().map(String::as_str).collect();
                let max = if face { letters * FACE_LETTERS / MESSAGE_LETTERS } else { letters };
                match reflow(&lines, max, STORY, "", f) {
                    Some(new) => out.extend(new.iter().enumerate().map(|(i, row)| Command {
                        code: if i % PAGE_ROWS == 0 { SHOW_MESSAGE } else { MORE_MESSAGE },
                        indent,
                        text: encode(row),
                        args: if i % PAGE_ROWS == 0 { args.clone() } else { Vec::new() },
                    })),
                    None => out.extend(rows.into_iter().enumerate().map(|(i, text)| Command {
                        code: if i == 0 { c.code } else { MORE_MESSAGE },
                        indent,
                        text,
                        args: if i == 0 { args.clone() } else { Vec::new() },
                    })),
                }
                continue;
            }
            SET_FACE => face = !c.text.is_empty(),
            SHOW_CHOICES => {
                let parts: Vec<String> = decoded(&c.text).split('/').map(|o| {
                    if JA.is_match(o) { f(STORY, "", o).map_or(o.to_string(), |t| ascii_punctuation(&t).replace('/', " ")) } else { o.to_string() }
                }).collect();
                let joined = parts.join("/");
                if joined != decoded(&c.text) {
                    c.text = sjis(&joined);
                }
            }
            CHOICE => field(&mut c.text, STORY, f),
            CHANGE_NAME | CHANGE_TITLE => field(&mut c.text, NAME, f),
            _ => {}
        }
        out.push(c);
    }
    out
}

/// The commands held in chunk `body`, with `size` saying how many bytes they take.
fn command_chunk(row: &mut [Chunk], size: u32, body: u32, letters: usize, f: &mut Visit) -> R<()> {
    let Some(at) = row.iter().position(|c| c.id == body) else { return Ok(()) };
    let new = put_commands(&events(commands(&row[at].data)?, letters, f));
    if let Some(s) = row.iter_mut().find(|c| c.id == size) {
        s.data.clear();
        put_number(&mut s.data, new.len() as u32);
    }
    row[at].data = new;
    Ok(())
}

fn map(top: &mut [Chunk], letters: usize, f: &mut Visit) -> R<()> {
    in_rows(top, 0x51, &mut |event| in_rows(event, 0x05, &mut |page| command_chunk(page, 0x33, 0x34, letters, f)))
}

fn database(top: &mut [Chunk], letters: usize, f: &mut Visit) -> R<()> {
    in_rows(top, 0x0B, &mut |r| {
        text_field(r, 0x01, NAME, f);
        text_field(r, 0x02, "character profile", f);
        text_field(r, 0x43, TERM, f);
        Ok(())
    })?;
    in_rows(top, 0x0C, &mut |r| {
        text_field(r, 0x01, "item or skill name", f);
        text_field(r, 0x02, DESCRIPTION, f);
        (0x03..=0x04).for_each(|i| text_field(r, i, "battle message", f));
        Ok(())
    })?;
    in_rows(top, 0x0D, &mut |r| {
        text_field(r, 0x01, "item or skill name", f);
        text_field(r, 0x02, DESCRIPTION, f);
        Ok(())
    })?;
    for (id, place) in [(0x0E, "class or enemy name"), (0x11, TERM), (0x1E, "class or enemy name")] {
        in_rows(top, id, &mut |r| {
            text_field(r, 0x01, place, f);
            Ok(())
        })?;
    }
    in_rows(top, 0x0F, &mut |troop| in_rows(troop, 0x0B, &mut |page| command_chunk(page, 0x0B, 0x0C, letters, f)))?;
    in_rows(top, 0x12, &mut |r| {
        text_field(r, 0x01, "item or skill name", f);
        (0x33..=0x37).for_each(|i| text_field(r, i, "battle message", f));
        Ok(())
    })?;
    in_rows(top, 0x19, &mut |r| command_chunk(r, 0x15, 0x16, letters, f))?;
    if let Some(terms) = top.iter_mut().find(|c| c.id == 0x15) {
        let mut p = 0;
        let mut words = chunks(&terms.data, &mut p)?;
        words.iter_mut().for_each(|w| field(&mut w.data, TERM, f));
        terms.data.clear();
        put_chunks(&mut terms.data, &words);
    }
    Ok(())
}

/// A file with `f` asked about each text. Returns it put back together.
fn rewrite(bytes: &[u8], letters: usize, f: &mut Visit) -> R<Vec<u8>> {
    let start = size_of_header(bytes)?;
    let header = bytes.get(start.saturating_sub(11)..start).ok_or("not an RPG Maker file")?;
    let (mut top, ended) = chunks_and_end(bytes, &mut start.clone())?;
    match header {
        h if h.ends_with(b"LcfMapUnit") => map(&mut top, letters, f)?,
        h if h.ends_with(b"LcfDataBase") => database(&mut top, letters, f)?,
        _ => return Err("not an RPG Maker 2000 or 2003 file".into()),
    }
    let mut out = bytes[..start].to_vec();
    put_chunks(&mut out, &top);
    if !ended {
        out.pop();
    }
    Ok(out)
}

/// Where the chunks start, after the file's name for itself.
fn size_of_header(bytes: &[u8]) -> R<usize> {
    let mut p = 0;
    Ok(number(bytes, &mut p)? as usize + p)
}

/// The database and maps, each with its bytes. An unknown layout is an error here, not in the game.
fn load(game: &Path) -> R<Vec<(PathBuf, Vec<u8>)>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        name.ends_with(".ldb") || (name.starts_with("map") && name.ends_with(".lmu"))
    }).collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let bytes = original(&p)?;
        if rewrite(&bytes, MESSAGE_LETTERS, &mut |_, _, _| None).map_err(|e| format!("{}: {e}", p.display()))? != bytes {
            return Err(format!("{}: this file's layout isn't one we can rewrite safely.", p.display()));
        }
        out.push((p, bytes));
    }
    if out.is_empty() {
        return Err("No RPG Maker data files in this folder.".into());
    }
    Ok(out)
}

/// All the Japanese the game shows, in game order, repeats included.
pub fn texts(game: &Path) -> R<Vec<Text>> {
    let mut out = Vec::new();
    for (path, bytes) in load(game)? {
        let file = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        rewrite(&bytes, MESSAGE_LETTERS, &mut |place, speaker, text| {
            out.push(Text { file: file.clone(), place: place.into(), speaker: speaker.into(), text: text.into() });
            None
        })?;
    }
    Ok(out)
}

/// Rewrite the data files from the game's originals. Returns (texts replaced, files changed).
pub fn install(game: &Path, lookup: &dyn Fn(&str) -> Option<String>) -> R<(usize, usize)> {
    let (mut replaced, mut changed) = (0, 0);
    for (path, bytes) in load(game)? {
        let mut count = 0;
        let new = rewrite(&bytes, crate::fit::letters(game, MESSAGE_LETTERS), &mut |_, _, text| {
            let tr = lookup(text);
            count += tr.is_some() as usize;
            tr
        })?;
        if count > 0 {
            rewrite(&new, MESSAGE_LETTERS, &mut |_, _, _| None)?;
            replace_file(&path, &new)?;
            (replaced, changed) = (replaced + count, changed + 1);
        }
    }
    Ok((replaced, changed))
}

/// Whether a folder holds this format, so callers can choose the path.
pub fn present(game: &Path) -> bool {
    fs::read_dir(game).into_iter().flatten().flatten().any(|e| e.file_name().to_string_lossy().to_lowercase().ends_with(".ldb"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: u32, data: &[u8]) -> Chunk {
        Chunk { id, data: data.to_vec() }
    }

    fn command(code: u32, text: &str) -> Command {
        Command { code, indent: 0, text: sjis(text), args: Vec::new() }
    }

    fn page(list: &[Command]) -> Vec<Chunk> {
        let body = put_commands(list);
        let mut size = Vec::new();
        put_number(&mut size, body.len() as u32);
        vec![chunk(0x33, &size), chunk(0x34, &body)]
    }

    fn file(header: &str, top: &[Chunk]) -> Vec<u8> {
        let mut out = vec![header.len() as u8];
        out.extend(header.as_bytes());
        put_chunks(&mut out, top);
        out
    }

    fn lmu() -> Vec<u8> {
        let list = [
            command(SHOW_MESSAGE, "こんにちは"),
            command(MORE_MESSAGE, "元気ですか"),
            command(SHOW_CHOICES, "はい/いいえ"),
            command(CHOICE, "はい"),
            command(CHOICE, "いいえ"),
            command(0, ""),
        ];
        let pages = put_rows(&[(1, page(&list))]);
        let events = put_rows(&[(1, vec![chunk(0x01, b"EV0001"), chunk(0x05, &pages)])]);
        file("LcfMapUnit", &[chunk(0x01, &[1]), chunk(0x51, &events)])
    }

    fn ldb() -> Vec<u8> {
        let actors = put_rows(&[(1, vec![chunk(0x01, &sjis("太郎")), chunk(0x03, b"Face1")])]);
        let items = put_rows(&[(1, vec![chunk(0x01, &sjis("薬")), chunk(0x02, &sjis("傷を治す"))])]);
        let mut words = Vec::new();
        put_chunks(&mut words, &[chunk(0x01, &sjis("敵が現れた"))]);
        file("LcfDataBase", &[chunk(0x0B, &actors), chunk(0x0D, &items), chunk(0x15, &words)])
    }

    fn seen(bytes: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        rewrite(bytes, MESSAGE_LETTERS, &mut |place, _, text| {
            out.push((place.to_string(), text.to_string()));
            None
        }).unwrap();
        out
    }

    #[test]
    fn numbers_read_back() {
        for n in [0u32, 1, 127, 128, 300, 16_384, 2_000_000] {
            let mut b = Vec::new();
            put_number(&mut b, n);
            assert_eq!(number(&b, &mut 0).unwrap(), n);
        }
    }

    #[test]
    fn files_come_back_byte_for_byte_when_nothing_changes() {
        for b in [lmu(), ldb()] {
            assert_eq!(rewrite(&b, MESSAGE_LETTERS, &mut |_, _, _| None).unwrap(), b);
        }
    }

    #[test]
    fn map_text_and_database_text_are_found() {
        let shown: Vec<String> = seen(&lmu()).into_iter().map(|t| t.1).collect();
        assert_eq!(shown, ["こんにちは元気ですか", "はい", "いいえ", "はい", "いいえ"]);
        let shown: Vec<String> = seen(&ldb()).into_iter().map(|t| t.1).collect();
        assert_eq!(shown, ["太郎", "薬", "傷を治す", "敵が現れた"]);
    }

    #[test]
    fn a_message_is_broken_again_onto_new_pages_and_the_sizes_follow() {
        let long = "word ".repeat(60);
        let new = rewrite(&lmu(), MESSAGE_LETTERS, &mut |_, _, text| Some(if text.contains("元気") { long.clone() } else { "Yes".into() })).unwrap();
        let mut p = size_of_header(&new).unwrap();
        let top = chunks(&new, &mut p).unwrap();
        let events = rows(&top.iter().find(|c| c.id == 0x51).unwrap().data).unwrap();
        let pages = rows(&events[0].1.iter().find(|c| c.id == 5).unwrap().data).unwrap();
        let body = &pages[0].1.iter().find(|c| c.id == 0x34).unwrap().data;
        let size = number(&pages[0].1.iter().find(|c| c.id == 0x33).unwrap().data, &mut 0).unwrap();
        assert_eq!(size as usize, body.len());
        let list = commands(body).unwrap();
        let rows: Vec<&Command> = list.iter().filter(|c| matches!(c.code, SHOW_MESSAGE | MORE_MESSAGE)).collect();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows.iter().map(|c| c.code == SHOW_MESSAGE).collect::<Vec<_>>(), [true, false, false, false, true, false]);
        assert!(rows.iter().all(|c| c.text.len() <= MESSAGE_LETTERS));
        assert_eq!(list.iter().find(|c| c.code == SHOW_CHOICES).unwrap().text, b"Yes/Yes");
    }

    #[test]
    fn install_starts_from_the_game_own_file() {
        let dir = std::env::temp_dir().join(format!("vntl-lcf-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("RPG_RT.ldb"), ldb()).unwrap();
        fs::write(dir.join("Map0001.lmu"), lmu()).unwrap();
        let lookup = |t: &str| (t == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &lookup).unwrap(), (1, 1));
        let first = fs::read(dir.join("RPG_RT.ldb")).unwrap();
        assert_eq!(seen(&first).len(), 3);
        install(&dir, &lookup).unwrap();
        assert_eq!(fs::read(dir.join("RPG_RT.ldb")).unwrap(), first);
        assert_eq!(fs::read(dir.join("Map0001.lmu")).unwrap(), lmu());
        let _ = fs::remove_dir_all(dir);
    }
}
