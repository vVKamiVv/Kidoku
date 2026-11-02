//! TyranoScript: KAG-style scenes in UTF-8, read by kirikiri's parser. They sit in a folder, in an
//! Electron archive, or in a zip that may be stuck on the end of the NW.js program. Lines aren't
//! broken by hand here, since the page wraps English at its spaces. The engine's own prompts sit in
//! its code: the words in lang.js, and a confirm box in libs.js.

use crate::code::{shown_strings, with_english, TJS};
use crate::word;
use crate::kirikiri::{Layout, Script};
use crate::pipeline::Line;
use crate::{files_under, original, replace_file, R};
use regex::Regex;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Wider than any line, so nothing gets a row break.
const NO_BREAKS: usize = 1 << 20;
const ROW_BREAK: &str = "[r]";
const FOLDER: &str = "data/scenario";
const CODE: [&str; 2] = ["tyrano/lang.js", "tyrano/libs.js"];

static SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""\w+"\s*:\s*"|\bconfirm\("#).unwrap());
static NOT_SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new("$^").unwrap());

fn scene(name: &str) -> bool {
    let name = name.replace('\\', "/").to_lowercase();
    name.contains(FOLDER) && name.ends_with(".ks")
}

fn engine_code(name: &str) -> bool {
    let name = name.replace('\\', "/").to_lowercase();
    CODE.iter().any(|c| name.ends_with(c))
}

fn wanted(name: &str) -> bool {
    scene(name) || engine_code(name)
}

// ---------- Electron's archive ----------

/// `u32 4, u32 header size, u32 header size - 4, u32 text size`, the text, padding; files follow.
fn asar_header(b: &[u8]) -> R<(Value, usize)> {
    let bad = "this app.asar isn't one we can read";
    let (base, size) = (8 + word(b.get(..8).ok_or(bad)?, 4), word(b.get(..16).ok_or(bad)?, 12));
    let text = b.get(16..16 + size).ok_or(bad)?;
    Ok((serde_json::from_slice(text).map_err(|e| format!("app.asar: {e}"))?, base))
}

/// Every file below `node` as its path.
fn asar_files(node: &Value, prefix: &str, out: &mut Vec<String>) {
    for (name, v) in node["files"].as_object().into_iter().flatten() {
        let path = format!("{prefix}{name}");
        if v.get("files").is_some() {
            asar_files(v, &format!("{path}/"), out);
        } else {
            out.push(path);
        }
    }
}

fn asar_entry<'a>(root: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    path.split('/').try_fold(root, |node, part| node.get_mut("files")?.get_mut(part))
}

fn asar_with(b: &[u8], new: &[(String, Vec<u8>)]) -> R<Vec<u8>> {
    let (mut root, base) = asar_header(b)?;
    let mut tail: Vec<u8> = Vec::new();
    let old_end = b.len() - base;
    for (path, bytes) in new {
        let entry = asar_entry(&mut root, path).ok_or("a scene went missing from app.asar")?;
        entry["offset"] = (old_end + tail.len()).to_string().into();
        entry["size"] = bytes.len().into();
        if let Some(fields) = entry.as_object_mut() {
            fields.remove("integrity"); // of the old file
        }
        tail.extend(bytes);
    }
    let text = root.to_string().into_bytes();
    let padded = text.len().next_multiple_of(4);
    let mut out = Vec::with_capacity(16 + padded + b.len() - base + tail.len());
    for n in [4, padded + 8, padded + 4, text.len()] {
        out.extend((n as u32).to_le_bytes());
    }
    out.extend(&text);
    out.resize(16 + padded, 0);
    out.extend(&b[base..]);
    out.extend(tail);
    Ok(out)
}

// ---------- zip ----------

struct ZipEntry {
    name: String,
    /// Where its row starts in the central directory.
    row: usize,
    method: u16,
    packed: usize,
    /// Of its local header, in the file.
    local: usize,
}

struct Zip {
    /// Where the central directory and its end record start.
    central: usize,
    end: usize,
    /// How far the zip sits from the start of the file: stored offsets don't count the program.
    delta: usize,
    entries: Vec<ZipEntry>,
}

fn zip_read(b: &[u8]) -> Option<Zip> {
    let from = b.len().saturating_sub(66_000);
    let end = from + b[from..].windows(4).rposition(|w| w == b"PK\x05\x06")?;
    let count = u16::from_le_bytes(b.get(end + 10..end + 12)?.try_into().ok()?) as usize;
    let (size, stored) = (word(b.get(..end + 16)?, end + 12), word(b.get(..end + 20)?, end + 16));
    let central = end.checked_sub(size)?;
    let delta = central.checked_sub(stored)?;
    let (mut p, mut entries) = (central, Vec::with_capacity(count));
    for _ in 0..count {
        let row = b.get(p..p + 46)?;
        let field = |at: usize| u16::from_le_bytes([row[at], row[at + 1]]) as usize;
        let (name_size, extra, comment) = (field(28), field(30), field(32));
        let name = String::from_utf8_lossy(b.get(p + 46..p + 46 + name_size)?).into_owned();
        entries.push(ZipEntry { name, row: p, method: field(10) as u16, packed: word(row, 20), local: word(row, 42) + delta });
        p += 46 + name_size + extra + comment;
    }
    Some(Zip { central, end, delta, entries })
}

fn zip_file(b: &[u8], e: &ZipEntry) -> R<Vec<u8>> {
    let field = |at: usize| u16::from_le_bytes([b[e.local + at], b[e.local + at + 1]]) as usize;
    let start = e.local + 30 + field(26) + field(28);
    let raw = b.get(start..start + e.packed).ok_or_else(|| format!("{} runs past the zip", e.name))?;
    match e.method {
        0 => Ok(raw.to_vec()),
        8 => {
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut flate2::read::DeflateDecoder::new(raw), &mut out).map_err(|err| format!("{}: {err}", e.name))?;
            Ok(out)
        }
        m => Err(format!("{}: zip method {m} isn't supported", e.name)),
    }
}

/// The program and its zip, with new files stored after the old data and a new directory after those.
fn zip_with(b: &[u8], zip: &Zip, new: &[(usize, Vec<u8>)]) -> Vec<u8> {
    let mut directory = b[zip.central..zip.end].to_vec();
    let mut out = b[..zip.central].to_vec();
    for (i, bytes) in new {
        let e = &zip.entries[*i];
        let mut crc = flate2::Crc::new();
        crc.update(bytes);
        let (name, sum, size) = (e.name.as_bytes(), crc.sum().to_le_bytes(), (bytes.len() as u32).to_le_bytes());
        let row = e.row - zip.central;
        directory[row + 10..row + 12].copy_from_slice(&[0, 0]); // stored, not packed
        directory[row + 16..row + 20].copy_from_slice(&sum);
        directory[row + 20..row + 24].copy_from_slice(&size);
        directory[row + 24..row + 28].copy_from_slice(&size);
        directory[row + 42..row + 46].copy_from_slice(&((out.len() - zip.delta) as u32).to_le_bytes());
        out.extend(b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0");
        out.extend(sum);
        out.extend(size);
        out.extend(size);
        out.extend((name.len() as u16).to_le_bytes());
        out.extend([0, 0]);
        out.extend(name);
        out.extend(bytes);
    }
    let central = out.len();
    out.extend(&directory);
    let at = ((central - zip.delta) as u32).to_le_bytes();
    let tail = out.len();
    out.extend(&b[zip.end..]);
    out[tail + 16..tail + 20].copy_from_slice(&at);
    out
}

// ---------- the game ----------

enum Store {
    Folder,
    Asar(PathBuf),
    Zip(PathBuf),
}

/// A file of the engine's code that shows text.
struct Code {
    name: String,
    text: String,
    spots: Vec<(usize, usize)>,
}

impl Code {
    fn key(&self, start: usize) -> String {
        format!("js:{}:{start}", self.name)
    }
}

pub struct Game {
    store: Store,
    /// `(where it is, script)`: a path in the folder or archive, or a zip entry's number.
    scripts: Vec<(String, Script)>,
    code: Vec<Code>,
}

fn holder(game: &Path) -> R<Store> {
    if game.join(FOLDER).is_dir() {
        return Ok(Store::Folder);
    }
    let asar = game.join("resources").join("app.asar");
    if asar.is_file() {
        return Ok(Store::Asar(asar));
    }
    let mut paths: Vec<PathBuf> = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
    paths.sort();
    let zip = paths.into_iter().find(|p| {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        (name == "package.nw" || name.ends_with(".exe")) && original(p).ok().and_then(|b| zip_read(&b)).is_some_and(|z| z.entries.iter().any(|e| scene(&e.name)))
    });
    zip.map(Store::Zip).ok_or_else(|| "No TyranoScript scenes found in this folder.".to_string())
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let store = holder(game)?;
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        match &store {
            Store::Folder => {
                let mut paths = Vec::new();
                files_under(&game.join(FOLDER), &mut paths);
                paths.extend(CODE.iter().map(|c| game.join(c)).filter(|p| p.is_file()));
                for p in paths.into_iter().filter(|p| p.extension().is_some_and(|e| e == "ks") || engine_code(&p.to_string_lossy())) {
                    files.push((p.strip_prefix(game).unwrap_or(&p).to_string_lossy().into_owned(), original(&p)?));
                }
            }
            Store::Asar(path) => {
                let b = original(path)?;
                let (root, base) = asar_header(&b)?;
                let mut names = Vec::new();
                asar_files(&root, "", &mut names);
                for name in names.into_iter().filter(|n| wanted(n)) {
                    let mut tree = root.clone();
                    let e = asar_entry(&mut tree, &name).ok_or("a scene went missing from app.asar")?;
                    let start = base + e["offset"].as_str().and_then(|o| o.parse::<usize>().ok()).ok_or("a scene has no offset in app.asar")?;
                    let size = e["size"].as_u64().unwrap_or(0) as usize;
                    files.push((name, b.get(start..start + size).ok_or("a scene runs past app.asar")?.to_vec()));
                }
            }
            Store::Zip(path) => {
                let b = original(path)?;
                let zip = zip_read(&b).ok_or("this zip can't be read")?;
                for e in zip.entries.iter().filter(|e| wanted(&e.name)) {
                    files.push((e.name.clone(), zip_file(&b, e)?));
                }
            }
        }
        let (code, scenes): (Vec<_>, Vec<_>) = files.into_iter().partition(|(name, _)| engine_code(name));
        let scripts = scenes.into_iter().map(|(name, bytes)| (name.clone(), Script::read_with_names(name, &bytes))).collect();
        let mut code: Vec<Code> = code.into_iter().filter_map(|(name, b)| Some(Code { name, text: String::from_utf8(b).ok()?, spots: Vec::new() })).collect();
        let sources: Vec<String> = code.iter().map(|c| c.text.clone()).collect();
        for (n, start, end) in shown_strings(&sources, &TJS, &SHOWN, &NOT_SHOWN) {
            code[n].spots.push((start, end));
        }
        Ok(Game { store, scripts, code })
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|s| s.1.lines(&mut out));
        for c in &self.code {
            out.extend(c.spots.iter().map(|&(start, end)| (c.key(start), None, c.text[start..end].to_string())));
        }
        out
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) and reads them back.
/// Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let (mut done, mut new) = (0, Vec::new());
    for (file, s) in &g.scripts {
        if let Some((bytes, count)) = s.with(&Layout { width: NO_BREAKS, row_break: ROW_BREAK, rows: usize::MAX, page_break: "" }, line, name) {
            new.push((file.clone(), bytes));
            done += count;
        }
    }
    for c in &g.code {
        let (text, count) = with_english(&c.text, &c.spots, &|start, _| line(&c.key(start)));
        if count > 0 {
            new.push((c.name.clone(), text.into_bytes()));
            done += count;
        }
    }
    match &g.store {
        Store::Folder => {
            for (file, bytes) in &new {
                replace_file(&game.join(file), bytes)?;
            }
        }
        Store::Asar(path) => {
            let out = asar_with(&original(path)?, &new)?;
            replace_file(path, &out)?;
            let back = fs::read(path).map_err(|e| e.to_string())?;
            let (mut root, base) = asar_header(&back)?;
            for (file, bytes) in &new {
                let e = asar_entry(&mut root, file).ok_or("a scene went missing from app.asar")?;
                let start = base + e["offset"].as_str().and_then(|o| o.parse::<usize>().ok()).ok_or("a scene has no offset in app.asar")?;
                if back.get(start..start + bytes.len()) != Some(&bytes[..]) {
                    return Err(format!("{file} didn't read back as written"));
                }
            }
        }
        Store::Zip(path) => {
            let b = original(path)?;
            let zip = zip_read(&b).ok_or("this zip can't be read")?;
            let by_index: Vec<(usize, Vec<u8>)> = new.iter().filter_map(|(file, bytes)| Some((zip.entries.iter().position(|e| e.name == *file)?, bytes.clone()))).collect();
            replace_file(path, &zip_with(&b, &zip, &by_index))?;
            let back = fs::read(path).map_err(|e| e.to_string())?;
            let written = zip_read(&back).ok_or("the zip didn't read back")?;
            for (i, bytes) in &by_index {
                if zip_file(&back, &written.entries[*i])? != *bytes {
                    return Err(format!("{} didn't read back as written", written.entries[*i].name));
                }
            }
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENE: &str = "#太郎\n台詞です[p]\n[glink text=\"選ぶ\" target=*a]\n";

    fn lookups() -> (impl Fn(&str) -> Option<String>, impl Fn(&str) -> Option<String>) {
        (|_: &str| Some("English".to_string()), |n: &str| (n == "太郎").then(|| "Taro".to_string()))
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("vntl-tyrano-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn asar() -> Vec<u8> {
        let text = format!(r#"{{"files":{{"data":{{"files":{{"scenario":{{"files":{{"a.ks":{{"size":{},"offset":"7"}}}}}}}}}},"main.js":{{"size":7,"offset":"0"}}}}}}"#, SCENE.len());
        let padded = text.len().next_multiple_of(4);
        let mut out = Vec::new();
        for n in [4, padded + 8, padded + 4, text.len()] {
            out.extend((n as u32).to_le_bytes());
        }
        out.extend(text.as_bytes());
        out.resize(16 + padded, 0);
        out.extend(b"program");
        out.extend(SCENE.as_bytes());
        out
    }

    /// A program with a zip on its end: a packed scene and a stored file, offsets not counting the program.
    fn exe() -> Vec<u8> {
        let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, SCENE.as_bytes()).unwrap();
        let packed = z.finish().unwrap();
        let program = b"MZ program".to_vec();
        let mut out = program.clone();
        let mut directory = Vec::new();
        let mut add = |out: &mut Vec<u8>, name: &str, method: u16, stored: &[u8], plain: usize| {
            let offset = (out.len() - program.len()) as u32;
            out.extend(b"PK\x03\x04\x14\0\0\0");
            out.extend(method.to_le_bytes());
            out.extend([0; 8]);
            out.extend((stored.len() as u32).to_le_bytes());
            out.extend((plain as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend([0, 0]);
            out.extend(name.as_bytes());
            out.extend(stored);
            directory.extend(b"PK\x01\x02\x14\0\x14\0\0\0");
            directory.extend(method.to_le_bytes());
            directory.extend([0; 8]);
            directory.extend((stored.len() as u32).to_le_bytes());
            directory.extend((plain as u32).to_le_bytes());
            directory.extend((name.len() as u16).to_le_bytes());
            directory.extend([0; 12]);
            directory.extend(offset.to_le_bytes());
            directory.extend(name.as_bytes());
        };
        add(&mut out, "data/scenario/a.ks", 8, &packed, SCENE.len());
        add(&mut out, "main.js", 0, b"code", 4);
        let at = (out.len() - program.len()) as u32;
        out.extend(&directory);
        out.extend(b"PK\x05\x06\0\0\0\0\x02\0\x02\0");
        out.extend((directory.len() as u32).to_le_bytes());
        out.extend(at.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    const WRITTEN: &str = "#Taro\nEnglish[p]\n[glink text=\"English\" target=*a]\n";

    #[test]
    fn scenes_in_a_folder() {
        let d = dir();
        fs::create_dir_all(d.join(FOLDER)).unwrap();
        fs::write(d.join(FOLDER).join("a.ks"), SCENE).unwrap();
        let lines = Game::open(&d).unwrap().lines();
        assert_eq!(lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect::<Vec<_>>(), [(Some("太郎"), "台詞です"), (None, "選ぶ")]);
        let (line, name) = lookups();
        assert_eq!(install(&d, &line, &name).unwrap(), 3);
        assert_eq!(fs::read_to_string(d.join(FOLDER).join("a.ks")).unwrap(), WRITTEN);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn the_engines_prompts_change_and_the_rest_of_its_code_does_not() {
        let d = dir();
        fs::create_dir_all(d.join(FOLDER)).unwrap();
        fs::create_dir_all(d.join("tyrano")).unwrap();
        fs::write(d.join(FOLDER).join("a.ks"), "地の文[p]\n").unwrap();
        fs::write(d.join("tyrano/lang.js"), "window.tyrano_lang={\"word\":{\"go_title\":\"戻りますか？\",\"tag\":\"見本\"}};").unwrap();
        fs::write(d.join("tyrano/libs.js"), "// \"説明\"\nif(confirm('終わりますか？')){x();}\nstr += day + \"日\";\nalert(\"見本\");\n").unwrap();
        let lines = Game::open(&d).unwrap().lines();
        let texts: Vec<&str> = lines.iter().map(|l| l.2.as_str()).collect();
        assert_eq!(texts, ["地の文", "戻りますか？", "終わりますか？"], "a string used elsewhere stays everywhere");
        install(&d, &|_| Some("Don't stop?".to_string()), &|_| None).unwrap();
        assert_eq!(fs::read_to_string(d.join("tyrano/lang.js")).unwrap(), "window.tyrano_lang={\"word\":{\"go_title\":\"Don't stop?\",\"tag\":\"見本\"}};");
        assert_eq!(fs::read_to_string(d.join("tyrano/libs.js")).unwrap(), "// \"説明\"\nif(confirm('Don\\'t stop?')){x();}\nstr += day + \"日\";\nalert(\"見本\");\n");
        assert_eq!(Game::open(&d).unwrap().lines().len(), 3, "reads the game's own");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn scenes_in_an_electron_archive() {
        let d = dir();
        fs::create_dir_all(d.join("resources")).unwrap();
        let path = d.join("resources/app.asar");
        fs::write(&path, asar()).unwrap();
        assert_eq!(Game::open(&d).unwrap().lines().len(), 2);
        let (line, name) = lookups();
        install(&d, &line, &name).unwrap();
        let g = Game::open_written(&path);
        assert_eq!(g, WRITTEN);
        let first = fs::read(&path).unwrap();
        install(&d, &line, &name).unwrap();
        assert_eq!(fs::read(&path).unwrap(), first);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn scenes_in_a_zip_on_the_end_of_the_program() {
        let d = dir();
        let path = d.join("game.exe");
        fs::write(&path, exe()).unwrap();
        assert_eq!(Game::open(&d).unwrap().lines().len(), 2);
        let (line, name) = lookups();
        install(&d, &line, &name).unwrap();
        let b = fs::read(&path).unwrap();
        let zip = zip_read(&b).unwrap();
        assert_eq!(zip.delta, 10);
        assert_eq!(zip_file(&b, &zip.entries[0]).unwrap(), WRITTEN.as_bytes());
        assert_eq!(zip_file(&b, &zip.entries[1]).unwrap(), b"code");
        assert!(b.starts_with(b"MZ program"));
        install(&d, &line, &name).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b);
        let _ = fs::remove_dir_all(d);
    }

    impl Game {
        /// A scene as written to the archive at `path`.
        fn open_written(path: &Path) -> String {
            let b = fs::read(path).unwrap();
            let (mut root, base) = asar_header(&b).unwrap();
            let e = asar_entry(&mut root, "data/scenario/a.ks").unwrap();
            let start = base + e["offset"].as_str().unwrap().parse::<usize>().unwrap();
            String::from_utf8(b[start..start + e["size"].as_u64().unwrap() as usize].to_vec()).unwrap()
        }
    }
}
