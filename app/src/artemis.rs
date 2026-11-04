//! Artemis engine (root.pfs, .iet scripts). The scripts are KAG in all but name, so kirikiri reads
//! them. In a version 6 archive new files go on the end and only their rows in the index change: the
//! index sits at the front and keeps its size. Version 8 hides its files with a key made from the
//! index, so changing a row would mean redoing every file. There our files go in an archive of their
//! own, which the engine reads after the game's and takes over them.

use crate::word;
use crate::kirikiri::{box_width, Layout, Script};
use crate::pipeline::Line;
use crate::{backup, extend_file, files_under, ours, write_whole, R};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const HEAD: usize = 7;
/// shortcut: measured in play on one game, whose box holds about 39 letters, for scripts that break
/// too few rows by hand to tell. Fonts differ per game, so another may want its own.
const LINE_LETTERS: usize = 38;
const ROW_BREAK: &str = "[rt]";
const BOX_ROWS: usize = 3;
/// Waits for a click and clears the text. The scripts' own `[np]` would clear the name plate too.
const PAGE_BREAK: &str = "@[rp]";
/// Interface images sit directly in here; folders below it hold scene art.
const IMAGES: &str = "image/";
/// Drawn names are spaced so wide that each letter reads as its own label, so they're read whole.
const DRAWN_NAMES: &str = "name_";

struct Item {
    name: String,
    /// Its name as stored.
    raw: Vec<u8>,
    /// Where its offset and size sit in the head.
    row: usize,
    start: u64,
    size: usize,
}

struct Archive {
    path: PathBuf,
    len: u64,
    /// Everything before the files: `pf6`, u32 index size, the index.
    head: Vec<u8>,
    items: Vec<Item>,
    /// Version 8: what each file's bytes are mixed with, from its first byte.
    key: Option<[u8; 20]>,
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut padded = data.to_vec();
    padded.push(0x80);
    padded.resize((padded.len() + 8).next_multiple_of(64) - 8, 0);
    padded.extend((data.len() as u64 * 8).to_be_bytes());
    for block in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for i in 0..80 {
            w[i] = if i < 16 { u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap()) } else { (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1) };
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((b & c) | (!b & d), 0x5A827999),
                1 => (b ^ c ^ d, 0x6ED9EBA1),
                2 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let next = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*word);
            (e, d, c, b, a) = (d, c, b.rotate_left(30), a, next);
        }
        for (sum, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *sum = sum.wrapping_add(v);
        }
    }
    let mut out = [0; 20];
    out.as_chunks_mut::<4>().0.iter_mut().zip(h).for_each(|(o, v)| *o = v.to_be_bytes());
    out
}

fn mixed(b: &mut [u8], key: &[u8; 20]) {
    b.iter_mut().zip(key.iter().cycle()).for_each(|(b, k)| *b ^= k);
}

/// A version 8 archive of `files`, by stored name. After the rows comes where to find each size,
/// counted from byte 15, and where that list starts.
fn pf8(files: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let rows: usize = files.keys().map(|n| 4 + n.len() + 12).sum();
    let list = 4 + rows;
    let size = list + 4 + (files.len() + 1) * 8 + 4;
    let mut index = (files.len() as u32).to_le_bytes().to_vec();
    let (mut at, mut sizes) = (HEAD + size, Vec::new());
    for (name, bytes) in files {
        index.extend((name.len() as u32).to_le_bytes());
        index.extend(name);
        index.extend(0u32.to_le_bytes());
        index.extend((at as u32).to_le_bytes());
        sizes.push((HEAD + index.len() - 15) as u64);
        index.extend((bytes.len() as u32).to_le_bytes());
        at += bytes.len();
    }
    index.extend((files.len() as u32 + 1).to_le_bytes());
    sizes.iter().chain(&[0]).for_each(|s| index.extend(s.to_le_bytes()));
    index.extend((list as u32).to_le_bytes());
    let key = sha1(&index);
    let mut out = b"pf8".to_vec();
    out.extend((index.len() as u32).to_le_bytes());
    out.extend(index);
    for bytes in files.values() {
        let start = out.len();
        out.extend(bytes);
        mixed(&mut out[start..], &key);
    }
    out
}

impl Archive {
    /// None when the file isn't a pfs archive. Comes from the game's own file, so reruns match.
    fn open(path: &Path) -> R<Option<Archive>> {
        let kept = backup(path);
        Archive::read(if kept.exists() { &kept } else { path }, path)
    }

    fn read(from: &Path, path: &Path) -> R<Option<Archive>> {
        let err = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut f = fs::File::open(from).map_err(err)?;
        let len = f.metadata().map_err(err)?.len();
        let mut top = [0; HEAD];
        if f.read_exact(&mut top).is_err() || !top.starts_with(b"pf") {
            return Ok(None);
        }
        if top[2] != b'6' && top[2] != b'8' {
            return Err(format!("This Artemis archive is version {}, and only versions 6 and 8 are supported so far.", top[2] as char));
        }
        let mut head = top.to_vec();
        head.resize(HEAD + word(&top, 3), 0);
        f.read_exact(&mut head[HEAD..]).map_err(err)?;
        // `u32 count`, then rows of `u32 name size, name, u32 0, u32 offset, u32 size`
        let (mut items, mut p) = (Vec::new(), HEAD + 4);
        for _ in 0..word(&head, HEAD) {
            let name_size = word(&head, p);
            let raw = head[p + 4..p + 4 + name_size].to_vec();
            let name = match std::str::from_utf8(&raw) {
                Ok(name) => name.to_string(),
                Err(_) => encoding_rs::SHIFT_JIS.decode_without_bom_handling(&raw).0.into_owned(),
            }.replace('\\', "/");
            p += 4 + name_size + 4;
            items.push(Item { name, raw, row: p, start: word(&head, p) as u64, size: word(&head, p + 4) });
            p += 8;
        }
        let key = (top[2] == b'8').then(|| sha1(&head[HEAD..]));
        Ok(Some(Archive { path: path.to_path_buf(), len, head, items, key }))
    }

    fn file(&self, f: &mut fs::File, item: &Item) -> R<Vec<u8>> {
        let mut b = vec![0; item.size];
        f.seek(SeekFrom::Start(item.start)).and_then(|_| f.read_exact(&mut b)).map_err(|e| format!("{}: {e}", item.name))?;
        if let Some(key) = &self.key {
            mixed(&mut b, key);
        }
        Ok(b)
    }

    fn replace(&self, new: &[(usize, Vec<u8>)]) -> R<()> {
        let (mut head, mut tail) = (self.head.clone(), Vec::new());
        for (i, bytes) in new {
            let row = self.items[*i].row;
            let at = self.len + tail.len() as u64;
            let at = u32::try_from(at).map_err(|_| "the archive would pass 4 GB")?;
            head[row..row + 4].copy_from_slice(&at.to_le_bytes());
            head[row + 4..row + 8].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            tail.extend(bytes);
        }
        extend_file(&self.path, self.len, &self.head, &head, &tail)
    }
}

fn archive_paths(game: &Path) -> R<Vec<PathBuf>> {
    Ok(fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        name.ends_with(".pfs") || name.rsplit_once('.').is_some_and(|(stem, n)| stem.ends_with(".pfs") && n.parse::<u32>().is_ok())
    }).collect())
}

/// The next number after the game's last archive.
fn patch_path(last: &Path) -> PathBuf {
    let name = last.to_string_lossy();
    match name.rsplit_once('.').and_then(|(stem, n)| Some((stem, n.parse::<u32>().ok()?))) {
        Some((stem, n)) => PathBuf::from(format!("{stem}.{:03}", n + 1)),
        None => PathBuf::from(format!("{name}.000")),
    }
}

/// Writes our own archive after the game's and reads it back. It's marked before it's written: an
/// unmarked file would look like the game's own.
fn write_patch(last: &Path, files: &BTreeMap<Vec<u8>, Vec<u8>>) -> R<()> {
    let path = patch_path(last);
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    fs::write(ours(&path), "").map_err(err)?;
    write_whole(&path, &pf8(files)).map_err(err)?;
    let back = Archive::read(&path, &path)?.ok_or("our archive didn't read back")?;
    let mut f = fs::File::open(&path).map_err(err)?;
    for item in &back.items {
        if files.get(&item.raw).is_none_or(|b| back.file(&mut f, item).ok().as_ref() != Some(b)) {
            return Err(format!("{} didn't read back as written", item.name));
        }
    }
    Ok(())
}

/// `root.pfs`, then `root.pfs.000` and on: a later one replaces earlier files.
fn archives(game: &Path) -> R<Vec<Archive>> {
    let mut paths = archive_paths(game)?;
    paths.retain(|p| !ours(p).exists());
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        out.extend(Archive::open(&p)?);
    }
    Ok(out)
}

pub struct Game {
    archives: Vec<Archive>,
    /// `(archive, item, script)`
    scripts: Vec<(usize, usize, Script)>,
    width: usize,
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let archives = archives(game)?;
        let mut found: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
        for (a, archive) in archives.iter().enumerate() {
            for (i, item) in archive.items.iter().enumerate() {
                if item.name.to_lowercase().ends_with(".iet") {
                    found.insert(item.name.to_lowercase(), (a, i));
                }
            }
        }
        if found.is_empty() {
            return Err("No .iet scripts in this Artemis game; other script types aren't supported yet.".into());
        }
        let (mut scripts, mut widths) = (Vec::new(), Vec::new());
        for (name, (a, i)) in found {
            let archive = &archives[a];
            let from = if backup(&archive.path).exists() { backup(&archive.path) } else { archive.path.clone() };
            let mut f = fs::File::open(from).map_err(|e| e.to_string())?;
            let bytes = archive.file(&mut f, &archive.items[i])?;
            scripts.push((a, i, Script::read(name, &bytes, &mut widths)));
        }
        Ok(Game { archives, scripts, width: crate::fit::letters(game, box_width(widths, LINE_LETTERS)) })
    }

    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        self.scripts.iter().for_each(|s| s.2.lines(&mut out));
        out
    }

    /// The keys of values the game shows on one row, as choices, and how many letters a row holds.
    pub fn one_row(&self) -> (Vec<String>, usize) {
        (self.scripts.iter().flat_map(|s| s.2.value_keys()).collect(), self.width)
    }
}

/// Every interface image as `(name, PNG)`, from the game's own archives.
pub fn interface_images(game: &Path) -> R<Vec<(String, Vec<u8>)>> {
    images_where(game, &|n| !n.starts_with(DRAWN_NAMES))
}

/// The pictures of names, for the same treatment.
pub fn drawn_names(game: &Path) -> R<Vec<(String, Vec<u8>)>> {
    images_where(game, &|n| n.starts_with(DRAWN_NAMES))
}

fn images_where(game: &Path, wanted: &dyn Fn(&str) -> bool) -> R<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    for archive in archives(game)? {
        let from = if backup(&archive.path).exists() { backup(&archive.path) } else { archive.path.clone() };
        let mut f = fs::File::open(from).map_err(|e| e.to_string())?;
        for item in &archive.items {
            let name = item.name.to_lowercase();
            if name.ends_with(".png") && name.strip_prefix(IMAGES).is_some_and(|n| !n.contains('/') && wanted(n)) {
                out.push((item.name.clone(), archive.file(&mut f, item)?));
            }
        }
    }
    Ok(out)
}

/// Repainted images wait here, since each write rebuilds the archive from the game's own: that way
/// a later write of the text keeps them.
fn painted_dir(game: &Path) -> PathBuf {
    game.join("vntl_work").join("painted")
}

/// Sets the repainted images, replacing any from an earlier run.
pub fn keep_painted(game: &Path, images: &[(String, Vec<u8>)]) -> R<()> {
    let dir = painted_dir(game);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    for (name, png) in images {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap_or(&dir)).map_err(|e| e.to_string())?;
        fs::write(&path, png).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn painted(game: &Path) -> R<Vec<(String, Vec<u8>)>> {
    let (dir, mut paths) = (painted_dir(game), Vec::new());
    files_under(&dir, &mut paths);
    paths.sort();
    paths.into_iter().map(|p| {
        let name = p.strip_prefix(&dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        Ok((name, fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?))
    }).collect()
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the archives and reads
/// them back. Returns how many texts were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let mut g = Game::open(game)?;
    // speaker tags anywhere in the game, for the name a macro prints into the log
    let printed: BTreeSet<String> = g.scripts.iter().flat_map(|s| s.2.printed_names()).collect();
    let said: BTreeMap<String, BTreeSet<String>> = printed.into_iter().map(|v| {
        let values = g.scripts.iter().flat_map(|s| s.2.values_of(&v)).collect();
        (v, values)
    }).collect();
    g.scripts.iter_mut().for_each(|s| s.2.set_said(said.clone()));
    let layout = Layout { width: g.width, row_break: ROW_BREAK, rows: crate::fit::rows(game, BOX_ROWS), page_break: PAGE_BREAK };
    let images = painted(game)?;
    // a choice that would wrap stays Japanese: it would push the ones after it out of their boxes
    let (one_row, width) = g.one_row();
    let one_row: BTreeSet<String> = one_row.into_iter().collect();
    let fits = |k: &str| line(k).filter(|en| !one_row.contains(k) || crate::text::wrap_words(&crate::text::ascii_punctuation(en), width).len() <= 1);
    let line: &dyn Fn(&str) -> Option<String> = &fits;
    let (mut done, mut patch) = (0, BTreeMap::new());
    for (a, archive) in g.archives.iter().enumerate() {
        let mut new = Vec::new();
        for (_, i, s) in g.scripts.iter().filter(|s| s.0 == a) {
            if let Some((bytes, count)) = s.with(&layout, line, name) {
                new.push((*i, bytes));
                done += count;
            }
        }
        for (name, png) in &images {
            new.extend(archive.items.iter().position(|item| item.name == *name).map(|i| (i, png.clone())));
        }
        if new.is_empty() {
            continue;
        }
        if archive.key.is_some() {
            patch.extend(new.into_iter().map(|(i, bytes)| (archive.items[i].raw.clone(), bytes)));
            continue;
        }
        archive.replace(&new)?;
        let back = Archive::read(&archive.path, &archive.path)?.ok_or("the archive didn't read back")?;
        let mut f = fs::File::open(&archive.path).map_err(|e| e.to_string())?;
        for (i, bytes) in &new {
            if back.file(&mut f, &back.items[*i])? != *bytes {
                return Err(format!("{} didn't read back as written", back.items[*i].name));
            }
        }
    }
    if let (false, Some(last)) = (patch.is_empty(), g.archives.last()) {
        write_patch(&last.path, &patch)?;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::sjis;

    fn pfs(version: u8, files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let size = 4 + files.iter().map(|(n, _)| 4 + n.len() + 12).sum::<usize>() + 8;
        let mut at = (HEAD + size) as u32;
        let mut out = vec![b'p', b'f', version];
        out.extend((size as u32).to_le_bytes());
        out.extend((files.len() as u32).to_le_bytes());
        for (name, b) in files {
            out.extend((name.len() as u32).to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(at.to_le_bytes());
            out.extend((b.len() as u32).to_le_bytes());
            at += b.len() as u32;
        }
        out.extend([0; 8]);
        files.iter().for_each(|(_, b)| out.extend(b));
        out
    }

    fn game(name: &str, bytes: Vec<u8>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-artemis-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), bytes).unwrap();
        dir
    }

    fn script() -> Vec<u8> {
        sjis("// 説明\r\n*ラベル\r\n[太郎][「]台詞です[」]\r\n地の文です[np]\r\n")
    }

    #[test]
    fn a_tag_named_after_a_character_is_the_speaker() {
        let dir = game("root.pfs", pfs(b'6', &[("scenario\\a.iet", script())]));
        let lines = Game::open(&dir).unwrap().lines();
        let shown: Vec<_> = lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect();
        assert_eq!(shown, [(Some("太郎"), "台詞です"), (None, "地の文です")]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn translations_go_on_the_end_and_the_index_keeps_its_size() {
        let dir = game("root.pfs", pfs(b'6', &[("scenario\\a.iet", script()), ("image.png", b"picture".to_vec())]));
        let path = dir.join("root.pfs");
        let before = fs::read(&path).unwrap();
        let keys: Vec<_> = Game::open(&dir).unwrap().lines().iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.contains(&k.to_string()).then(|| "A line of English".to_string());
        assert_eq!(install(&dir, &line, &|_| None).unwrap(), 2);

        let written = fs::read(&path).unwrap();
        let a = Archive::read(&path, &path).unwrap().unwrap();
        assert_eq!(a.items[0].start, before.len() as u64);
        let mut f = fs::File::open(&path).unwrap();
        assert_eq!(a.file(&mut f, &a.items[0]).unwrap(), sjis("// 説明\r\n*ラベル\r\n[太郎][「]A line of English[」]\r\nA line of English[np]\r\n"));
        assert_eq!(a.file(&mut f, &a.items[1]).unwrap(), b"picture");
        drop(f);

        install(&dir, &line, &|_| None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), written);
        assert_eq!(fs::read(backup(&path)).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_measured_box_decides_where_text_breaks() {
        let dir = game("root.pfs", pfs(b'6', &[("a.iet", sjis("地の文です[np]\r\n"))]));
        std::fs::create_dir_all(dir.join("vntl_work")).unwrap();
        fs::write(dir.join("vntl_work/box.txt"), "letters=10\nrows=2\n").unwrap();
        install(&dir, &|_| Some("aaaa bbbb cccc dddd eeee".to_string()), &|_| None).unwrap();
        let path = dir.join("root.pfs");
        let a = Archive::read(&path, &path).unwrap().unwrap();
        let mut f = fs::File::open(&path).unwrap();
        assert_eq!(a.file(&mut f, &a.items[0]).unwrap(), b"aaaa bbbb[rt]cccc dddd@[rp]eeee[np]\r\n");
        drop(f);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_choice_too_long_for_one_row_stays_japanese() {
        let choice = "[select_scene num=\"2\"\r\n\ttext1=\"はい\" label1=\"1\"\r\n\ttext2=\"いいえ\" label2=\"2\"]\r\n";
        let dir = game("root.pfs", pfs(b'6', &[("a.iet", sjis(choice))]));
        std::fs::create_dir_all(dir.join("vntl_work")).unwrap();
        fs::write(dir.join("vntl_work/box.txt"), "letters=10\n").unwrap();
        let (keys, width) = Game::open(&dir).unwrap().one_row();
        assert_eq!((keys.len(), width), (2, 10));
        let line = |k: &str| Some(if k == keys[0] { "Yes" } else { "No, not at all" }.to_string());
        install(&dir, &line, &|_| None).unwrap();
        let path = dir.join("root.pfs");
        let a = Archive::read(&path, &path).unwrap().unwrap();
        let written = encoding_rs::SHIFT_JIS.decode(&a.file(&mut fs::File::open(&path).unwrap(), &a.items[0]).unwrap()).0.into_owned();
        assert!(written.contains("text1=\"Yes\"") && written.contains("text2=\"いいえ\""), "{written}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn repainted_images_go_in_with_every_write_and_scene_art_is_not_offered() {
        let files = [("a.iet".to_string(), script()), ("image/t-start.png".to_string(), b"button".to_vec()), ("image/eyec/scene.png".to_string(), b"art".to_vec()), ("image/name_a.png".to_string(), b"name".to_vec())];
        let named: Vec<(&str, Vec<u8>)> = files.iter().map(|(n, b)| (n.as_str(), b.clone())).collect();
        let dir = game("root.pfs", pfs(b'6', &named));
        let names: Vec<String> = interface_images(&dir).unwrap().into_iter().map(|i| i.0).collect();
        assert_eq!(names, ["image/t-start.png"]);

        keep_painted(&dir, &[("image/t-start.png".to_string(), b"English button".to_vec())]).unwrap();
        for _ in 0..2 {
            install(&dir, &|_| Some("A line".to_string()), &|_| None).unwrap();
            let path = dir.join("root.pfs");
            let a = Archive::read(&path, &path).unwrap().unwrap();
            let mut f = fs::File::open(&path).unwrap();
            assert_eq!(a.file(&mut f, &a.items[1]).unwrap(), b"English button");
            assert_eq!(a.file(&mut f, &a.items[2]).unwrap(), b"art");
        }
        assert_eq!(interface_images(&dir).unwrap()[0].1, b"button", "reads the game's own");

        keep_painted(&dir, &[]).unwrap();
        install(&dir, &|_| Some("A line".to_string()), &|_| None).unwrap();
        let path = dir.join("root.pfs");
        let a = Archive::read(&path, &path).unwrap().unwrap();
        let mut f = fs::File::open(&path).unwrap();
        assert_eq!(a.file(&mut f, &a.items[1]).unwrap(), b"button", "a dropped label goes back");
        drop(f);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn other_archive_versions_are_refused_by_name() {
        let dir = game("root.pfs", pfs(b'2', &[("a.iet", script())]));
        assert!(Game::open(&dir).err().unwrap().contains("version 2"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sha1_matches_the_published_examples() {
        let hex = |b: [u8; 20]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(hex(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(sha1(&[b'a'; 1000])), "291e9a6c66994949b57ba5e650361e98fc36b1ba");
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn a_version_8_game_keeps_its_archive_and_gets_one_of_ours_that_goes_away_again() {
        let files = BTreeMap::from([(b"scenario\\a.iet".to_vec(), script()), (b"image\\t-start.png".to_vec(), b"button".to_vec())]);
        let dir = game("root.pfs", pf8(&files));
        let (root, patch) = (dir.join("root.pfs"), dir.join("root.pfs.000"));
        let before = fs::read(&root).unwrap();
        fs::create_dir_all(dir.join("vntl_work")).unwrap();
        assert_eq!(Game::open(&dir).unwrap().lines().len(), 2, "the script reads through the key");
        assert_eq!(interface_images(&dir).unwrap()[0].1, b"button");

        keep_painted(&dir, &[("image/t-start.png".to_string(), b"English button".to_vec())]).unwrap();
        for _ in 0..2 {
            assert_eq!(install(&dir, &|_| Some("A line".to_string()), &|_| None).unwrap(), 2);
            assert_eq!(fs::read(&root).unwrap(), before);
            let a = Archive::read(&patch, &patch).unwrap().unwrap();
            assert_eq!(a.items.iter().map(|i| i.raw.clone()).collect::<Vec<_>>(), files.keys().cloned().collect::<Vec<_>>());
            let mut f = fs::File::open(&patch).unwrap();
            assert_eq!(a.file(&mut f, &a.items[0]).unwrap(), b"English button");
            assert_eq!(a.file(&mut f, &a.items[1]).unwrap(), sjis("// 説明\r\n*ラベル\r\n[太郎][「]A line[」]\r\nA line[np]\r\n"));
        }
        assert_eq!(Game::open(&dir).unwrap().lines()[0].2, "台詞です", "ours isn't read as the game's");
        assert!(!dir.join("root.pfs.001").exists(), "a second run writes over the first");

        assert_eq!(crate::restore(&dir).unwrap(), (1, 0));
        assert!(!patch.exists() && !ours(&patch).exists() && crate::backups(&dir).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn our_archive_comes_after_the_games_last() {
        assert_eq!(patch_path(Path::new("g/root.pfs")), Path::new("g/root.pfs.000"));
        assert_eq!(patch_path(Path::new("g/root.pfs.009")), Path::new("g/root.pfs.010"));
    }

    #[test]
    fn the_list_after_the_rows_finds_each_size() {
        let b = pf8(&BTreeMap::from([(b"a".to_vec(), b"xy".to_vec()), (b"bcd".to_vec(), b"z".to_vec())]));
        let list = word(&b, b.len() - 3 - 4);
        let at = HEAD + word(&b, 3);
        assert_eq!(word(&b, at - 4), list, "where the list starts");
        assert_eq!(word(&b, HEAD + list), 3, "one more than the files");
        let first = u64::from_le_bytes(b[HEAD + list + 4..HEAD + list + 12].try_into().unwrap()) as usize;
        assert_eq!(word(&b, first + 15), 2);
    }
}

