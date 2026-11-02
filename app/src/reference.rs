//! Using an official translation (another Ren'Py build of the same game) as a reference.

use crate::renpy::{key, Block, Cache, Dump, Style};
use crate::text::{CJK, JA, VARS};
use crate::R;
use indexmap::IndexMap;
use regex::Regex;
use serde_pickle::{HashableValue, Value as Pickle};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static TAG_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{/?([a-z#]*)").unwrap());
static FONT_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)["']([^"'\n]+\.(?:ttf|otf|ttc))["']"#).unwrap());
static TEXT_SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"gui\.text_size\s*=\s*(\d+)").unwrap());
/// Text tags Ren'Py 7.4 understands; a reference line using anything else is left to the model.
const SAFE_TAGS: &[&str] = &["", "a", "alpha", "b", "color", "cps", "font", "i", "image", "k", "outlinecolor", "plain",
    "s", "size", "space", "u", "vspace", "w", "p", "nw", "fast", "done", "clear", "#"];

/// (japanese, english) when one side is Japanese and the other has no CJK/Hangul at all.
pub fn jp_en<'a>(a: &'a str, b: &'a str) -> Option<(&'a str, &'a str)> {
    if JA.is_match(a) && !CJK.is_match(b) {
        Some((a, b))
    } else if JA.is_match(b) && !CJK.is_match(a) {
        Some((b, a))
    } else {
        None
    }
}

/// An official line can be used as-is when it keeps the [variables] and only uses safe tags.
pub fn usable(src: &str, en: &str) -> bool {
    let vars = |s: &str| {
        let mut v: Vec<&str> = VARS.find_iter(s).map(|m| m.as_str()).collect();
        v.sort();
        v.into_iter().map(String::from).collect::<Vec<_>>()
    };
    !en.trim().is_empty() && vars(src) == vars(en) && TAG_NAME.captures_iter(en).all(|c| SAFE_TAGS.contains(&&c[1]))
}

pub struct Maps {
    pub by_id: IndexMap<String, String>,
    pub by_text: IndexMap<String, String>,
    pub strings: IndexMap<String, String>,
}

/// Pair Japanese/target-language lines from another build's tl/ layer, whichever is primary.
pub fn reference_maps(reference: &Dump, lang: &str) -> Maps {
    let wanted = |l: &str| [lang.to_lowercase().as_str(), "japanese", "ja", "jp"].contains(&l.to_lowercase().as_str());
    let mut m = Maps { by_id: IndexMap::new(), by_text: IndexMap::new(), strings: IndexMap::new() };
    for b in &reference.blocks {
        let src: Vec<&str> = b.items.iter().filter(|it| it.is_say()).map(|it| it.what()).collect();
        for (l, dst) in &b.tl {
            if !wanted(l) || dst.len() != src.len() {
                continue;
            }
            for (i, (s, d)) in src.iter().zip(dst).enumerate() {
                if let Some((jp, en)) = jp_en(s, d) {
                    m.by_id.insert(format!("{}#{i}", b.id), en.to_string());
                    m.by_text.entry(jp.to_string()).or_insert_with(|| en.to_string());
                }
            }
        }
    }
    for (l, table) in &reference.string_tl {
        if wanted(l) {
            for (old, new) in table {
                if let Some((jp, en)) = jp_en(old, new) {
                    m.strings.entry(jp.to_string()).or_insert_with(|| en.to_string());
                }
            }
        }
    }
    m
}

#[derive(Default, Debug, PartialEq)]
pub struct Hits {
    pub by_id: usize,
    pub by_text: usize,
    pub strings: usize,
}

/// Copy official lines and strings into the cache; the model only gets what is left.
pub fn apply_reference(dump: &Dump, reference: &Dump, cache: &mut Cache, blocks: &[&Block], lang: &str) -> Hits {
    let m = reference_maps(reference, lang);
    let mut hits = Hits::default();
    let mut official: IndexMap<String, ()> = cache.official.iter().map(|k| (k.clone(), ())).collect();
    for b in blocks {
        for (say, (i, it)) in b.items.iter().enumerate().filter(|(_, it)| it.is_say()).enumerate() {
            let (en, by_id) = match m.by_id.get(&format!("{}#{say}", b.id)) {
                Some(en) => (Some(en), true),
                None => (m.by_text.get(it.what()), false),
            };
            if let Some(en) = en.filter(|en| usable(it.what(), en)) {
                cache.lines.insert(key(b, i), en.clone());
                official.insert(key(b, i), ());
                if by_id { hits.by_id += 1 } else { hits.by_text += 1 }
            }
        }
    }
    let mut official: Vec<String> = official.into_keys().collect();
    official.sort();
    cache.official = official;
    // All official names/interface strings are kept, even ones our extraction missed.
    let ours: HashSet<&str> = dump.strings.iter().map(|s| s.text.as_str()).collect();
    for (jp, en) in &m.strings {
        if usable(jp, en) {
            cache.strings.insert(jp.clone(), en.clone());
            hits.strings += ours.contains(jp.as_str()) as usize;
        }
    }
    hits
}

// ---------- files inside .rpa archives ----------

/// {path: (archive, offset, length, prefix)} for files packed in game/*.rpa (RPA-3.0).
pub type Packed = IndexMap<String, (PathBuf, u64, u64, Vec<u8>)>;

pub fn archive_files(game: &Path) -> R<Packed> {
    let mut rpas: Vec<PathBuf> = fs::read_dir(game.join("game"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("rpa")))
        .collect();
    rpas.sort();
    let mut out = Packed::new();
    for rpa in rpas {
        let mut f = fs::File::open(&rpa).map_err(|e| e.to_string())?;
        let mut head = Vec::new();
        let mut byte = [0u8];
        while f.read(&mut byte).map_err(|e| e.to_string())? == 1 && byte[0] != b'\n' {
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        let parts: Vec<&str> = head.split_whitespace().collect();
        if parts.len() < 3 || parts[0] != "RPA-3.0" {
            continue;
        }
        let bad = |e: &dyn std::fmt::Display| format!("{}: {e}", rpa.display());
        let offset = u64::from_str_radix(parts[1], 16).map_err(|e| bad(&e))?;
        let xor = i64::from_str_radix(parts[2], 16).map_err(|e| bad(&e))?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| bad(&e))?;
        let mut index = Vec::new();
        flate2::read::ZlibDecoder::new(f).read_to_end(&mut index).map_err(|e| bad(&e))?;
        // An .rpa index is plain dicts/lists/tuples/bytes; serde-pickle only decodes data, it never runs code.
        let Pickle::Dict(entries) = serde_pickle::value_from_slice(&index, Default::default()).map_err(|e| bad(&e))? else {
            return Err(bad(&"unexpected archive index"));
        };
        for (name, value) in entries {
            let name = match name {
                HashableValue::String(s) => s,
                HashableValue::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
                _ => continue,
            };
            let first = match value {
                Pickle::List(v) | Pickle::Tuple(v) => v.into_iter().next(),
                _ => None,
            };
            let Some(Pickle::Tuple(fields) | Pickle::List(fields)) = first else { continue };
            let int = |v: Option<&Pickle>| match v {
                Some(Pickle::I64(n)) => Some(*n),
                _ => None,
            };
            let (Some(off), Some(len)) = (int(fields.first()), int(fields.get(1))) else { continue };
            let prefix = match fields.get(2) {
                Some(Pickle::Bytes(b)) => b.clone(),
                Some(Pickle::String(s)) => s.chars().map(|c| c as u8).collect(), // latin-1
                _ => Vec::new(),
            };
            out.insert(name, (rpa.clone(), (off ^ xor) as u64, (len ^ xor) as u64, prefix));
        }
    }
    Ok(out)
}

pub fn read_game_file(game: &Path, packed: &Packed, path: &str) -> Option<Vec<u8>> {
    let loose = game.join("game").join(path);
    if loose.is_file() {
        return fs::read(loose).ok();
    }
    let (rpa, off, len, prefix) = packed.get(path)?;
    let mut f = fs::File::open(rpa).ok()?;
    f.seek(SeekFrom::Start(*off)).ok()?;
    let mut data = prefix.clone();
    f.take(len.saturating_sub(prefix.len() as u64)).read_to_end(&mut data).ok()?;
    Some(data)
}

/// The speaker a picture's path belongs to: a folder of character art whose name ends in their code,
/// like `images/char/ch_07_ab/`. shortcut: the one layout seen so far; add others as games turn up.
pub(crate) fn sprite_code(path: &str) -> Option<&str> {
    let (folder, _) = path.rsplit_once('/').filter(|_| crate::menus::is_picture(path))?;
    folder.contains("char").then(|| folder.rsplit(['/', '_']).next())?
}

/// The largest picture of each speaker a game's files name, by the name it shows them under, keyed
/// from `names`, its {code: name}. The largest is the whole figure and not a face or an effect.
pub fn sprites(game: &Path, names: &IndexMap<String, String>) -> R<HashMap<String, Vec<u8>>> {
    let packed = archive_files(game)?;
    let root = game.join("game");
    let mut loose = Vec::new();
    crate::files_under(&root, &mut loose);
    let on_disk = loose.iter().filter_map(|p| Some((p.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/"), p.metadata().ok()?.len())));
    let mut largest: HashMap<&String, (String, u64)> = HashMap::new();
    for (path, size) in packed.iter().map(|(p, e)| (p.clone(), e.2)).chain(on_disk) {
        let lower = path.to_lowercase();
        if let Some(name) = sprite_code(&lower).and_then(|code| names.get(code))
            && largest.get(name).is_none_or(|l| size > l.1)
        {
            largest.insert(name, (path, size));
        }
    }
    // through PNG, as not every image model takes a WebP
    let png = |d: Vec<u8>| if d.starts_with(crate::menus::PNG) { Some(d) } else { crate::menus::read_picture(&d).and_then(|img| crate::menus::write_png(&img)).ok() };
    Ok(largest.into_iter().filter_map(|(name, (path, _))| Some((name.clone(), png(read_game_file(game, &packed, &path)?)?))).collect())
}

fn write_file(dest: &Path, data: &[u8]) -> R<()> {
    fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(dest, data).map_err(|e| e.to_string())
}

/// Copy translated images/files from the reference's tl/<lang>/ when our original of that file
/// is byte-identical to theirs, so the translated version fits our layout exactly.
pub fn copy_reference_files(game: &Path, reference: &Path, lang: &str) -> R<Vec<String>> {
    let (ours, theirs) = (archive_files(game)?, archive_files(reference)?);
    let mut all: BTreeSet<String> = theirs.keys().cloned().collect();
    let (root, mut loose) = (reference.join("game"), Vec::new());
    crate::files_under(&root.join("tl"), &mut loose);
    all.extend(loose.iter().filter_map(|p| Some(p.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/"))));
    let mut copied = Vec::new();
    for path in all {
        let parts: Vec<&str> = path.splitn(3, '/').collect();
        if parts.len() < 3 || parts[0] != "tl" || !parts[1].eq_ignore_ascii_case(lang) {
            continue;
        }
        let rel = parts[2];
        if [".rpy", ".rpyc", ".rpym", ".rpymc"].iter().any(|x| rel.ends_with(x)) {
            continue;
        }
        let original = read_game_file(game, &ours, rel);
        if original.is_some()
            && original == read_game_file(reference, &theirs, rel)
            && let Some(data) = read_game_file(reference, &theirs, &path)
        {
            write_file(&crate::renpy::tl_dir(game, lang).join(rel), &data)?;
            copied.push(rel.to_string());
        }
    }
    Ok(copied)
}

/// The reference's `translate <lang> python` code (e.g. the font and text size the official
/// English uses) and the font files it names, copied next to our translation.
pub fn reference_style(game: &Path, ref_dir: &Path, reference: &Dump, lang: &str) -> R<(Style, Vec<String>)> {
    let langs: Vec<&String> = reference.lang_python.keys().filter(|l| l.eq_ignore_ascii_case(lang)).collect();
    let code: Vec<String> = langs.iter().flat_map(|l| reference.lang_python[*l].clone()).collect();
    if code.is_empty() {
        return Ok((Style::default(), Vec::new()));
    }
    let (ours, theirs) = (archive_files(game)?, archive_files(ref_dir)?);
    let all = code.join("\n");
    let fonts: BTreeSet<String> = FONT_FILE.captures_iter(&all).map(|c| c[1].to_string()).collect();
    let mut copied = Vec::new();
    for font in fonts {
        if read_game_file(game, &ours, &font).is_some() {
            continue;
        }
        let candidates = std::iter::once(font.clone()).chain(langs.iter().map(|l| format!("tl/{l}/{font}")));
        if let Some(data) = candidates.filter_map(|p| read_game_file(ref_dir, &theirs, &p)).next() {
            write_file(&crate::renpy::tl_dir(game, lang).join(&font), &data)?;
            copied.push(font);
        }
    }
    let text_size = TEXT_SIZE.captures(&all).and_then(|c| c[1].parse().ok());
    Ok((Style { code, text_size }, copied))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renpy::tests::{cache, dump, tmp};
    use serde_json::json;

    #[test]
    fn pairs_only_japanese_and_english() {
        assert_eq!(jp_en("ねえ", "Hey"), Some(("ねえ", "Hey")));
        assert_eq!(jp_en("Hey", "ねえ"), Some(("ねえ", "Hey")));
        assert_eq!(jp_en("設定", "Settings"), Some(("設定", "Settings")));
        assert_eq!(jp_en("アオイ", "아오이"), None);
        assert_eq!(jp_en("ハヤト", "隼人"), None);
        assert!(usable("[name]だ", "It's [name]") && !usable("だ", "{alt}It's") && !usable("[a]だ", "It's"));
    }

    #[test]
    fn applies_official_lines_by_id_then_text() {
        let ours = dump(json!({"blocks": [
            {"id": "a_1", "items": [{"code": "nvl clear"}, {"tpl": "", "what": "おはよう"}]},
            {"id": "a_2", "items": [{"tpl": "", "what": "カットされた"}]},
            {"id": "x_9", "items": [{"tpl": "", "what": "またね"}]}],
            "strings": [{"text": "はい", "kind": "menu"}]}));
        let jp_primary = dump(json!({"blocks": [
            {"id": "a_1", "items": [{"tpl": "", "what": "おはよう"}], "tl": {"English": ["Morning"], "Spanish": ["Buenos días"]}},
            {"id": "z_1", "items": [{"tpl": "", "what": "またね"}], "tl": {"English": ["See you"]}}],
            "string_tl": {"English": {"はい": "Yes", "設定": "Settings"}, "Spanish": {"いいえ": "No"}}}));
        let mut c = cache(json!({}));
        let blocks: Vec<&Block> = ours.blocks.iter().collect();
        assert_eq!(apply_reference(&ours, &jp_primary, &mut c, &blocks, "english"), Hits { by_id: 1, by_text: 1, strings: 1 });
        assert_eq!(c.lines.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(), [("a_1#1", "Morning"), ("x_9#0", "See you")]);
        assert_eq!(c.strings.len(), 2);
        assert_eq!(c.official, ["a_1#1", "x_9#0"]);
        let en_primary = dump(json!({"blocks": [{"id": "q", "items": [{"tpl": "", "what": "Morning"}], "tl": {"japanese": ["おはよう"]}}]}));
        let mut c = cache(json!({}));
        apply_reference(&ours, &en_primary, &mut c, &blocks, "english");
        assert_eq!(c.lines.get("a_1#1").map(String::as_str), Some("Morning"));
    }

    fn make_rpa(path: &Path, files: &[(&str, &[u8])]) {
        let (mut blob, mut index, start, xor) = (Vec::new(), std::collections::BTreeMap::new(), 34i64, 0x42i64);
        for (name, data) in files {
            let entry = Pickle::List(vec![Pickle::Tuple(vec![
                Pickle::I64((start + blob.len() as i64) ^ xor),
                Pickle::I64(data.len() as i64 ^ xor),
                Pickle::Bytes(Vec::new()),
            ])]);
            index.insert(HashableValue::Bytes(name.as_bytes().to_vec()), entry);
            blob.extend_from_slice(data);
        }
        let pickled = serde_pickle::value_to_vec(&Pickle::Dict(index), Default::default()).unwrap();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, &pickled).unwrap();
        let mut out = format!("RPA-3.0 {:016x} {:08x}\n", start + blob.len() as i64, xor).into_bytes();
        out.extend(blob);
        out.extend(z.finish().unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, out).unwrap();
    }

    #[test]
    fn copies_identical_layout_files_and_official_style() {
        let d = tmp();
        let (ours, theirs) = (d.join("ours"), d.join("ref"));
        make_rpa(&ours.join("game/archive.rpa"), &[("gui/a.png", b"JP-A"), ("gui/b.png", b"OLD-B")]);
        make_rpa(&theirs.join("game/archive.rpa"), &[("gui/a.png", b"JP-A"), ("gui/b.png", b"NEW-B"), ("en.ttf", b"FONT"),
            ("tl/English/gui/a.png", b"EN-A"), ("tl/English/gui/b.png", b"EN-B")]);
        assert_eq!(read_game_file(&ours, &archive_files(&ours).unwrap(), "gui/b.png").unwrap(), b"OLD-B");
        assert_eq!(copy_reference_files(&ours, &theirs, "english").unwrap(), ["gui/a.png"]);
        assert_eq!(fs::read(ours.join("game/tl/english/gui/a.png")).unwrap(), b"EN-A");
        assert!(!ours.join("game/tl/english/gui/b.png").exists()); // layout differs: not copied
        let reference = dump(json!({"blocks": [], "lang_python": {
            "English": ["gui.text_size = 27\ngui.text_font = \"en.ttf\""], "Korean": ["gui.text_font = \"ko.ttf\""]}}));
        let (style, fonts) = reference_style(&ours, &theirs, &reference, "english").unwrap();
        assert_eq!((style.text_size, fonts, style.code.len()), (Some(27), vec!["en.ttf".to_string()], 1));
        assert_eq!(fs::read(ours.join("game/tl/english/en.ttf")).unwrap(), b"FONT");
        let _ = fs::remove_dir_all(d);
    }
}
