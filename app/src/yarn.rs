//! Unity games built with Yarn Spinner, whose text sits in plain `.assets` files. These builds leave
//! out the field lists unity.rs walks bundles with, so strings are found by their shape: a length on a
//! 4-byte boundary, then that many bytes of text. Only Japanese counts, which no number and no other
//! data looks like by chance. Each object is rewritten string by string, as nothing in it points at
//! its own bytes. Lines are `Name: text`, or `*: text` with a mark the game reads and no name.
//!
//! Text the code holds is rewritten too, with the same English as any equal string in the objects, so
//! what the code compares stays equal. Pictures are read from their textures and go back whole.

use crate::artemis::painted;
use crate::liar::Image;
use crate::menus::{read_png, stored};
use crate::pipeline::Line;
use crate::text::{ascii_punctuation, looks_like_name, JA};
use crate::unity::{read_serialized, Serialized};
use crate::{original, replace_file, word, R};
use regex::Regex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const TEXTURE: i32 = 28;
const MONO_BEHAVIOUR: i32 = 114;
const MONO_SCRIPT: i32 = 115;
/// Where a MonoBehaviour's script sits, after its game object and flag, and then its name.
const SCRIPT_AT: usize = 20;
const NAME_AT: usize = 28;
/// Yarn Spinner's table of lines.
const LINES: &str = "Localization";
/// The letters a font holds, which aren't text to read.
const FONT: &str = "TMP_FontAsset";
/// Text on screen, which only gets displayed.
const SHOWN: &str = "TextMeshPro";
const CODE: &str = "il2cpp_data/Metadata/global-metadata.dat";
const LONGEST: usize = 40_000;
/// Hand-placed breaks, for Japanese. The text box wraps English at its spaces.
const BREAK: &str = "<br>";
/// The file holding the code's text, and the one layout of it known here, Unity 6.3's.
const CODE_MAGIC: [u8; 4] = [0xaf, 0x1b, 0xb1, 0xfa];
const CODE_LAYOUT: usize = 39;
/// Longer strings in the code are tables of letters for matching text, and single letters are parts
/// of such matching.
const CODE_TEXT: std::ops::RangeInclusive<usize> = 2..=100;
const RGB24: usize = 3;
const RGBA32: usize = 4;
const DXT1: usize = 10;
const DXT5: usize = 12;
const BC7: usize = 25;
const SMALLEST_PICTURE: usize = 64;
/// In a texture, from the streaming switch to the platform's data: its settings, all one size.
const TO_PLATFORM: usize = 48;

static SPOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([^:\[<{\n]{1,16}):\s*").unwrap());
static LEVEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^level\d+$").unwrap());
/// .NET's regular expressions, built into the code, keep Unicode blocks as two-letter strings of their
/// edges. No word holds these letters, and matching would break with them changed.
static BLOCK_EDGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\x{3040}\x{30a0}\x{3100}\x{3300}\x{3400}\x{4dc0}\x{a000}]").unwrap());

/// The Japanese strings in `blob` from `p` on, as the spans of their text.
fn strings(blob: &[u8], mut p: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    while p + 4 <= blob.len() {
        let n = word(blob, p);
        let text = (2..=LONGEST).contains(&n).then(|| blob.get(p + 4..p + 4 + n)).flatten().and_then(|b| std::str::from_utf8(b).ok());
        if text.is_some_and(|t| JA.is_match(t) && !t.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))) {
            out.push((p + 4, p + 4 + n));
            p = (p + 4 + n).next_multiple_of(4);
        } else {
            p += 4;
        }
    }
    out
}

/// A string to translate.
struct Spot {
    object: usize,
    start: usize,
    end: usize,
    key: String,
    who: Option<String>,
    /// What comes before the text and stays, like `*: `.
    mark: String,
    body: String,
    line: bool,
}

impl Spot {
    fn written(&self, tr: &str, name: &dyn Fn(&str) -> Option<String>) -> String {
        let tr = ascii_punctuation(tr);
        if !self.line {
            return tr;
        }
        // Yarn reads brackets as markup, and one left open stops the line
        let body = tr.replace('[', "(").replace(']', ")");
        match &self.who {
            Some(who) => format!("{}: {body}", name(who).unwrap_or_else(|| who.clone())),
            None => format!("{}{body}", self.mark),
        }
    }
}

struct File {
    path: PathBuf,
    file: Serialized,
    spots: Vec<Spot>,
    /// Objects showing text on screen.
    labels: HashSet<usize>,
}

/// The code's file, and where each string in it starts. A string ends where the next one starts, and
/// the last start is the end of the last string.
struct Code {
    path: PathBuf,
    raw: Vec<u8>,
    starts: Vec<usize>,
}

pub struct Game {
    files: Vec<File>,
    code: Option<Code>,
    /// The Japanese strings of `code`, by place, that can change.
    code_text: Vec<(usize, String)>,
}

fn string_at(blob: &[u8], p: usize) -> Option<&str> {
    let n = word(blob.get(..p + 4)?, p);
    std::str::from_utf8(blob.get(p + 4..p + 4 + n)?).ok()
}

/// The strings' starts, if `raw` has the known layout. Its header lists each section as start, size
/// and count, and these two come first: the table of starts, then the bytes.
fn code_starts(raw: &[u8]) -> Option<Vec<usize>> {
    if raw.len() < 32 || raw[..4] != CODE_MAGIC || word(raw, 4) != CODE_LAYOUT {
        return None;
    }
    let (table, size) = (word(raw, 8), word(raw, 12));
    let starts: Vec<usize> = raw.get(table..table + size)?.as_chunks::<4>().0.iter().map(|c| word(c, 0)).collect();
    (starts.len() == word(raw, 16) && starts.is_sorted() && starts.last() == Some(&word(raw, 24))).then_some(starts)
}

impl Code {
    fn text(&self, i: usize) -> Option<&str> {
        let bytes = word(&self.raw, 20);
        std::str::from_utf8(self.raw.get(bytes + self.starts[i]..bytes + self.starts[i + 1])?).ok()
    }

    /// The file with `new` in place of the strings it has. The table stays where it is. The bytes move
    /// to the end, where they can be any length, and the header says so.
    fn with(&self, new: &HashMap<usize, String>) -> Vec<u8> {
        let mut out = self.raw.clone();
        let (table, bytes) = (word(&self.raw, 8), word(&self.raw, 20));
        let mut text: Vec<u8> = Vec::new();
        for i in 0..self.starts.len() - 1 {
            out[table + i * 4..table + i * 4 + 4].copy_from_slice(&(text.len() as u32).to_le_bytes());
            match new.get(&i) {
                Some(s) => text.extend(s.as_bytes()),
                None => text.extend(&self.raw[bytes + self.starts[i]..bytes + self.starts[i + 1]]),
            }
        }
        let last = table + (self.starts.len() - 1) * 4;
        out[last..last + 4].copy_from_slice(&(text.len() as u32).to_le_bytes());
        let moved = out.len() as u32;
        out[20..24].copy_from_slice(&moved.to_le_bytes());
        out[24..28].copy_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend(text);
        out
    }
}

/// Which of `texts` are also the whole of a string nothing here translates, like an object's name:
/// the code may find that object by it, so those stay.
fn found_elsewhere(files: &[File], texts: &[&str]) -> HashSet<String> {
    let mut out = HashSet::new();
    if texts.is_empty() {
        return out;
    }
    let exact = |t: &str| (t.len() as u32).to_le_bytes().iter().chain(t.as_bytes()).map(|b| format!(r"\x{b:02x}")).collect::<String>();
    let any = regex::bytes::Regex::new(&format!("(?s-u){}", texts.iter().map(|t| exact(t)).collect::<Vec<_>>().join("|"))).unwrap();
    for f in files {
        let ours: HashSet<(usize, usize)> = f.spots.iter().map(|s| (s.object, s.start)).collect();
        for (i, o) in f.file.objects.iter().enumerate().filter(|(_, o)| f.file.types[o.ty].class_id != TEXTURE) {
            for hit in any.find_iter(f.file.data(o)).filter(|m| !ours.contains(&(i, m.start() + 4))) {
                out.insert(String::from_utf8_lossy(&hit.as_bytes()[4..]).into_owned());
            }
        }
    }
    out
}

/// Lets text on screen shrink to fit its box, as English runs longer than what the box was laid out
/// for. The fields are found by their shape: the size twice, a weight, the switch, then its limits.
fn shrink_to_fit(blob: &mut [u8]) -> bool {
    let float = |b: &[u8], p| f32::from_le_bytes(b[p..p + 4].try_into().unwrap());
    let found = (NAME_AT..blob.len().saturating_sub(24)).step_by(4).find(|&p| {
        let size = float(blob, p);
        (1.0..=500.0).contains(&size) && size == float(blob, p + 4) && word(blob, p + 8).is_multiple_of(100) && (100..=900).contains(&word(blob, p + 8)) && word(blob, p + 12) < 2
    });
    let Some(p) = found else { return false };
    let size = float(blob, p);
    blob[p + 12..p + 16].copy_from_slice(&1u32.to_le_bytes());
    blob[p + 16..p + 20].copy_from_slice(&(size / 2.0).to_le_bytes());
    blob[p + 20..p + 24].copy_from_slice(&size.to_le_bytes());
    true
}

/// Where a texture keeps what matters. These builds have no field lists, so the walk is checked by
/// landing exactly on the end of the object.
struct Texture {
    /// Width, then height, size, stripped mipmaps, format and mipmap count.
    fields: usize,
    streamed: usize,
    /// The length of the pixels held inside the object.
    inside: usize,
    /// Where the pixels held outside it are named: offset, size, file.
    outside: usize,
}

impl Texture {
    fn read(blob: &[u8]) -> Option<Texture> {
        let fields = (4 + string_at(blob, 0)?.len()).next_multiple_of(4) + 4;
        let group = fields + 28;
        let streamed = (group + 4 + string_at(blob, group)?.len()).next_multiple_of(4);
        let platform = streamed + TO_PLATFORM;
        let inside = (platform + 4 + word(blob.get(..platform + 4)?, platform)).next_multiple_of(4);
        let outside = (inside + 4 + word(blob.get(..inside + 4)?, inside)).next_multiple_of(4);
        let file = string_at(blob, outside + 12)?;
        ((outside + 16 + file.len()).next_multiple_of(4) == blob.len()).then_some(Texture { fields, streamed, inside, outside })
    }

    fn size(&self, blob: &[u8]) -> (usize, usize) {
        (word(blob, self.fields), word(blob, self.fields + 4))
    }

    fn format(&self, blob: &[u8]) -> usize {
        word(blob, self.fields + 16)
    }

    /// Whether this holds a picture that can be read, and could hold text worth reading.
    fn readable(&self, blob: &[u8]) -> bool {
        let (w, h) = self.size(blob);
        [RGB24, RGBA32, DXT1, DXT5, BC7].contains(&self.format(blob)) && w.min(h) >= SMALLEST_PICTURE
    }

    /// The first mipmap's bytes, which come first, from the object or the file beside `assets`.
    fn pixels(&self, blob: &[u8], assets: &Path) -> Option<Vec<u8>> {
        let inside = word(blob, self.inside);
        if inside > 0 {
            return blob.get(self.inside + 4..self.inside + 4 + inside).map(<[u8]>::to_vec);
        }
        let offset = u64::from_le_bytes(blob[self.outside..self.outside + 8].try_into().unwrap());
        let mut file = fs::File::open(assets.with_file_name(string_at(blob, self.outside + 12)?)).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut out = vec![0; word(blob, self.outside + 8)];
        file.read_exact(&mut out).ok()?;
        Some(out)
    }

    /// The picture, top row first as images go. Textures keep the bottom row first.
    fn image(&self, blob: &[u8], assets: &Path) -> Option<Image> {
        let (w, h) = self.size(blob);
        let raw = self.pixels(blob, assets)?;
        let mut px = vec![0; w * h];
        let decoded = match self.format(blob) {
            DXT1 => texture2ddecoder::decode_bc1(&raw, w, h, &mut px),
            DXT5 => texture2ddecoder::decode_bc3(&raw, w, h, &mut px),
            BC7 => texture2ddecoder::decode_bc7(&raw, w, h, &mut px),
            RGBA32 => {
                px = raw.get(..w * h * 4)?.as_chunks::<4>().0.iter().map(|c| stored(c) ^ OPAQUE).collect();
                Ok(())
            }
            RGB24 => {
                px = raw.get(..w * h * 3)?.as_chunks::<3>().0.iter().map(|&[r, g, b]| stored(&[r, g, b, 255]) ^ OPAQUE).collect();
                Ok(())
            }
            _ => return None,
        };
        decoded.ok()?;
        Some(Image { w, h, px: px.chunks(w).rev().flatten().map(|p| p ^ OPAQUE).collect() })
    }
}

/// Decoders give alpha as it is, and images keep it upside down: this turns one into the other.
const OPAQUE: u32 = 0xff00_0000;

/// The object with `img` for its pixels, plain and held inside it, as any Unity reads them.
fn with_picture(blob: &[u8], t: &Texture, img: &Image) -> Vec<u8> {
    let mut out = blob[..t.inside].to_vec();
    let size = (img.w * img.h * 4) as u32;
    out[t.fields + 8..t.fields + 24].copy_from_slice(&[size, 0, RGBA32 as u32, 1].map(u32::to_le_bytes).concat());
    out[t.streamed..t.streamed + 4].fill(0);
    out.extend(size.to_le_bytes());
    for p in img.px.chunks(img.w).rev().flatten() {
        out.extend([(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255 - (p >> 24) as u8]);
    }
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend([0; 16]); // nothing outside
    out
}

fn picture_name(file: &Path, path_id: i64) -> String {
    format!("{}/{path_id}.png", file.file_name().unwrap_or_default().to_string_lossy())
}

/// The scripts of all `files`, by path id, as `(name, whether it's Yarn Spinner's)`.
fn scripts(files: &[(PathBuf, Serialized)]) -> HashMap<i64, (String, bool)> {
    let mut out = HashMap::new();
    for (_, file) in files {
        for o in file.objects.iter().filter(|o| file.types[o.ty].class_id == MONO_SCRIPT) {
            let blob = file.data(o);
            if let Some(name) = string_at(blob, 0) {
                out.insert(o.path_id, (name.to_string(), blob.windows(4).any(|w| w == b"Yarn")));
            }
        }
    }
    out
}

fn line_spot(file_name: &str, path_id: i64, object: usize, (start, end): (usize, usize), text: &str) -> Option<Spot> {
    let found = SPOKEN.captures(text);
    let name = found.as_ref().map(|c| c[1].to_string());
    let who = name.clone().filter(|n| JA.is_match(n) && looks_like_name(n));
    // a sentence with a colon in it isn't a speaker
    let kept = who.is_some() || name.is_some_and(|n| !JA.is_match(&n));
    let mark = found.filter(|_| kept).map_or(String::new(), |c| c[0].to_string());
    let body = text[mark.len()..].replace(BREAK, "");
    // a name over nothing but `...` has no text for the model, and the name still changes
    (JA.is_match(&body) || who.is_some()).then(|| Spot { object, start, end, key: format!("yarn:{file_name}:{path_id}:{start}"), who, mark, body, line: true })
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let folder = fs::read_dir(game).map_err(|e| e.to_string())?.flatten().map(|e| e.path())
            .find(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with("_data")))
            .ok_or("This Unity game has no _Data folder.")?;
        let mut paths: Vec<PathBuf> = fs::read_dir(&folder).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            name.ends_with(".assets") || LEVEL.is_match(&name)
        }).collect();
        paths.sort();
        let mut read = Vec::new();
        for p in paths {
            let file = read_serialized(original(&p)?).map_err(|e| format!("{}: {e}", p.display()))?;
            read.push((p, file));
        }
        let scripts = scripts(&read);
        let code_path = folder.join(CODE);
        let raw = if code_path.exists() { original(&code_path)? } else { Vec::new() };
        let starts = code_starts(&raw);
        let unread = String::from_utf8_lossy(&raw).into_owned();
        let mut files = Vec::new();
        for (path, file) in read {
            let file_name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let (mut spots, mut labels) = (Vec::new(), HashSet::new());
            for (i, o) in file.objects.iter().enumerate().filter(|(_, o)| file.types[o.ty].class_id == MONO_BEHAVIOUR) {
                let blob = file.data(o);
                let Some(script) = blob.get(SCRIPT_AT..NAME_AT).map(|b| i64::from_le_bytes(b.try_into().unwrap())) else { continue };
                let Some((name, yarn)) = scripts.get(&script) else { continue };
                let Some(own) = string_at(blob, NAME_AT) else { continue };
                let after = (NAME_AT + 4 + own.len()).next_multiple_of(4);
                let text = |&(s, e): &(usize, usize)| String::from_utf8_lossy(&blob[s..e]).into_owned();
                if name.starts_with(SHOWN) {
                    labels.insert(i);
                }
                if name == LINES && *yarn {
                    spots.extend(strings(blob, after).iter().filter_map(|span| line_spot(&file_name, o.path_id, i, *span, &text(span))));
                } else if name != FONT {
                    // what code of an unknown layout holds may be a key it looks up, and can't change with it
                    for span in strings(blob, after).iter().filter(|s| starts.is_some() || name.starts_with(SHOWN) || !unread.contains(&text(s))) {
                        let body = text(span);
                        spots.push(Spot { object: i, start: span.0, end: span.1, key: format!("asset:{body}"), who: None, mark: String::new(), body, line: false });
                    }
                }
            }
            files.push(File { path, file, spots, labels });
        }
        if !files.iter().flat_map(|f| &f.spots).any(|s| s.line) {
            return Err("This Unity game keeps its text some other way: only Unity's Localization package and Yarn Spinner are supported so far.".into());
        }
        let code = starts.map(|starts| Code { path: code_path, raw, starts });
        let code_text: Vec<(usize, String)> = code.iter().flat_map(|c| (0..c.starts.len() - 1).filter_map(move |i| {
            let text = c.text(i).filter(|t| JA.is_match(t) && !BLOCK_EDGE.is_match(t) && CODE_TEXT.contains(&t.chars().count()))?;
            Some((i, text.to_string()))
        })).collect();
        let held = found_elsewhere(&files, &code_text.iter().map(|t| t.1.as_str()).collect::<Vec<_>>());
        files.iter_mut().for_each(|f| f.spots.retain(|s| s.line || !held.contains(&s.body)));
        Ok(Game { files, code, code_text: code_text.into_iter().filter(|t| !held.contains(&t.1)).collect() })
    }

    /// The story, then the rest. Equal strings outside the story stay equal, in the code too.
    pub fn lines(&self) -> Vec<Line> {
        let spots = || self.files.iter().flat_map(|f| &f.spots);
        let mut out: Vec<Line> = spots().filter(|s| s.line && JA.is_match(&s.body)).map(|s| (s.key.clone(), s.who.clone(), s.body.clone())).collect();
        let mut seen = HashSet::new();
        let rest = spots().filter(|s| !s.line).map(|s| s.body.as_str()).chain(self.code_text.iter().map(|t| t.1.as_str()));
        out.extend(rest.filter(|t| seen.insert(*t)).map(|t| (format!("asset:{t}"), None, t.to_string())));
        out
    }

    /// The pictures worth looking through, by the names their repainted copies go by.
    pub fn pictures(&self) -> Vec<String> {
        self.files.iter().flat_map(|f| f.file.objects.iter().filter(|o| {
            f.file.types[o.ty].class_id == TEXTURE && Texture::read(f.file.data(o)).is_some_and(|t| t.readable(f.file.data(o)))
        }).map(|o| picture_name(&f.path, o.path_id))).collect()
    }

    /// The picture `name`, decoded.
    pub fn picture(&self, name: &str) -> Option<Image> {
        self.files.iter().find_map(|f| {
            let o = f.file.objects.iter().find(|o| f.file.types[o.ty].class_id == TEXTURE && picture_name(&f.path, o.path_id) == name)?;
            let blob = f.file.data(o);
            Texture::read(blob)?.image(blob, &f.path)
        })
    }
}

/// Writes the translations (`line` by key, `name` by the game's own text) into the game's files and
/// reads them back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let art: HashMap<String, Vec<u8>> = painted(game)?.into_iter().collect();
    let mut done = 0;
    for f in &g.files {
        let mut objects: BTreeMap<usize, Vec<&Spot>> = f.labels.iter().map(|&i| (i, Vec::new())).collect();
        f.spots.iter().for_each(|s| objects.entry(s.object).or_default().push(s));
        let mut new = HashMap::new();
        for (object, spots) in objects {
            let mut blob = f.file.data(&f.file.objects[object]).to_vec();
            let fits = f.labels.contains(&object) && shrink_to_fit(&mut blob);
            let (mut out, mut last, mut count) = (Vec::with_capacity(blob.len()), 0, 0);
            for s in spots {
                let tr = if JA.is_match(&s.body) { line(&s.key) } else { Some(s.body.clone()) };
                let Some(tr) = tr else { continue };
                let text = s.written(&tr, name).into_bytes();
                out.extend(&blob[last..s.start - 4]);
                out.extend((text.len() as u32).to_le_bytes());
                out.extend(text);
                out.resize(out.len().next_multiple_of(4), 0);
                last = s.end.next_multiple_of(4);
                count += 1;
            }
            if count > 0 || fits {
                out.extend(&blob[last..]);
                new.insert(object, out);
                done += count;
            }
        }
        for (i, o) in f.file.objects.iter().enumerate().filter(|(_, o)| f.file.types[o.ty].class_id == TEXTURE) {
            let Some(png) = art.get(&picture_name(&f.path, o.path_id)) else { continue };
            let blob = f.file.data(o);
            let t = Texture::read(blob).ok_or_else(|| format!("{}: a picture no longer reads", f.path.display()))?;
            new.insert(i, with_picture(blob, &t, &read_png(png)?));
        }
        if new.is_empty() {
            continue;
        }
        let bytes = f.file.rebuild(&new);
        let back = read_serialized(bytes.clone()).map_err(|e| format!("{}: {e}", f.path.display()))?;
        if new.iter().any(|(i, b)| back.data(&back.objects[*i]) != b.as_slice()) {
            return Err(format!("{} didn't read back as written", f.path.display()));
        }
        replace_file(&f.path, &bytes)?;
    }
    if let Some(code) = &g.code {
        let new: HashMap<usize, String> = g.code_text.iter().filter_map(|(i, text)| Some((*i, ascii_punctuation(&line(&format!("asset:{text}"))?)))).collect();
        let bytes = code.with(&new);
        let back = Code { path: code.path.clone(), starts: code_starts(&bytes).ok_or("the code's text didn't read back")?, raw: bytes };
        if (0..code.starts.len() - 1).any(|i| back.text(i) != new.get(&i).map(String::as_str).or(code.text(i))) {
            return Err(format!("{} didn't read back as written", code.path.display()));
        }
        if !new.is_empty() {
            replace_file(&code.path, &back.raw)?;
            done += new.len();
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(out: &mut Vec<u8>, text: &str) {
        out.extend((text.len() as u32).to_le_bytes());
        out.extend(text.as_bytes());
        out.resize(out.len().next_multiple_of(4), 0);
    }

    /// A serialized file with no field lists.
    fn assets(objects: &[(i64, i32, Vec<u8>)]) -> Vec<u8> {
        let classes: Vec<i32> = { let mut c: Vec<i32> = objects.iter().map(|o| o.1).collect(); c.sort(); c.dedup(); c };
        let mut meta = b"6000.0.0f1\0".to_vec();
        meta.extend(19i32.to_le_bytes());
        meta.push(0);
        meta.extend((classes.len() as i32).to_le_bytes());
        for c in &classes {
            meta.extend(c.to_le_bytes());
            meta.push(0);
            meta.extend((-1i16).to_le_bytes());
            meta.extend(vec![0; if *c == MONO_BEHAVIOUR { 32 } else { 16 }]);
        }
        meta.extend((objects.len() as i32).to_le_bytes());
        let (mut body, mut rows) = (Vec::new(), Vec::new());
        for (id, class, blob) in objects {
            body.resize(body.len().next_multiple_of(8), 0);
            rows.push((*id, body.len() as u64, blob.len() as u32, classes.iter().position(|c| c == class).unwrap() as i32));
            body.extend(blob);
        }
        const HEAD: usize = 48;
        for (id, at, size, ty) in rows {
            meta.resize((HEAD + meta.len()).next_multiple_of(4) - HEAD, 0);
            meta.extend(id.to_le_bytes());
            meta.extend(at.to_le_bytes());
            meta.extend(size.to_le_bytes());
            meta.extend(ty.to_le_bytes());
        }
        let offset = (HEAD + meta.len()).next_multiple_of(16);
        let mut out = vec![0; 8];
        out.extend(22u32.to_be_bytes());
        out.extend([0; 8]);
        out.extend((meta.len() as u32).to_be_bytes());
        out.extend(((offset + body.len()) as u64).to_be_bytes());
        out.extend((offset as u64).to_be_bytes());
        out.extend([0; 8]);
        out.extend(meta);
        out.resize(offset, 0);
        out.extend(body);
        out
    }

    fn script(name: &str, space: &str) -> Vec<u8> {
        let mut b = Vec::new();
        string(&mut b, name);
        b.extend([0; 8]);
        string(&mut b, name);
        string(&mut b, space);
        b
    }

    fn behaviour(script: i64, name: &str, strings: &[&str]) -> Vec<u8> {
        let mut b = vec![0; SCRIPT_AT];
        b.extend(script.to_le_bytes());
        string(&mut b, name);
        b.extend(7u32.to_le_bytes());
        strings.iter().for_each(|s| string(&mut b, s));
        b
    }

    const LINE_TABLE: i64 = 1;
    const LABEL: i64 = 2;
    const LIST: i64 = 3;
    const FONT_ASSET: i64 = 4;

    fn game() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-yarn-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        let data = dir.join("Sample_Data");
        fs::create_dir_all(data.join("il2cpp_data/Metadata")).unwrap();
        fs::write(data.join(CODE), "code holds 鍵の言葉 here".as_bytes()).unwrap();
        fs::write(data.join("globalgamemanagers.assets"), assets(&[
            (LINE_TABLE, MONO_SCRIPT, script(LINES, "Yarn.Unity")),
            (LABEL, MONO_SCRIPT, script("TextMeshProUGUI", "TMPro")),
            (LIST, MONO_SCRIPT, script("TipCatalog", "")),
            (FONT_ASSET, MONO_SCRIPT, script(FONT, "TMPro")),
        ])).unwrap();
        fs::write(data.join("sharedassets0.assets"), assets(&[
            (10, MONO_BEHAVIOUR, behaviour(LINE_TABLE, "ja", &["line:0001", "太郎: こんにちは<br>元気？", "*: 風が吹いた", "風の音だけがした", "途中、まで: 続きの文", "太郎: ……", "line only"])),
            (11, MONO_BEHAVIOUR, behaviour(FONT_ASSET, "font", &["あいうえお"])),
            (12, 28, vec![1, 2, 3, 4]),
        ])).unwrap();
        fs::write(data.join("level0"), assets(&[
            (20, MONO_BEHAVIOUR, behaviour(LABEL, "名前の物", &["始める", "鍵の言葉"])),
            (21, MONO_BEHAVIOUR, behaviour(LIST, "list", &["始める", "役立つ話", "鍵の言葉"])),
        ])).unwrap();
        dir
    }

    fn shown(lines: &[Line]) -> Vec<(Option<&str>, &str)> {
        lines.iter().map(|l| (l.1.as_deref(), l.2.as_str())).collect()
    }

    #[test]
    fn lines_give_up_their_speakers_and_the_rest_follows_once_each() {
        let dir = game();
        let lines = Game::open(&dir).unwrap().lines();
        assert_eq!(shown(&lines), [
            (Some("太郎"), "こんにちは元気？"),
            (None, "風が吹いた"),
            (None, "風の音だけがした"),
            (None, "途中、まで: 続きの文"),
            (None, "始める"),
            (None, "鍵の言葉"),
            (None, "役立つ話"),
        ], "a name, a mark, a plain line, a colon that isn't a speaker; on screen text, the same word once, and no font letters or object names");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn what_the_code_also_holds_stays_outside_the_text_on_screen() {
        let dir = game();
        install(&dir, &|k| k.starts_with("asset:").then(|| "Word".to_string()), &|_| None).unwrap();
        let file = read_serialized(fs::read(dir.join("Sample_Data/level0")).unwrap()).unwrap();
        let text = |i: usize| String::from_utf8_lossy(file.data(&file.objects[i])).into_owned();
        assert!(!text(0).contains("鍵の言葉"), "only ever displayed");
        assert!(text(1).contains("鍵の言葉") && !text(1).contains("始める"), "the code may look this one up");
        assert!(text(0).contains("名前の物"), "an object's own name stays");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn translations_go_in_at_any_length_and_the_game_is_read_from_its_own_files() {
        let dir = game();
        let path = dir.join("Sample_Data/sharedassets0.assets");
        let before = fs::read(&path).unwrap();
        let lines = Game::open(&dir).unwrap().lines();
        let keys: Vec<String> = lines.iter().map(|l| l.0.clone()).collect();
        let line = |k: &str| keys.iter().position(|x| x == k).filter(|i| *i < 4).map(|i| ["“Hello” [there]", "The wind’s blew", "Only the wind, and much longer than the Japanese was", "a heading"][i].to_string());
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 5);

        let written = fs::read(&path).unwrap();
        let file = read_serialized(written.clone()).unwrap();
        let table = file.data(&file.objects[0]);
        let mut p = NAME_AT + 4 + 2 + 2 + 4;
        let mut seen = Vec::new();
        while p < table.len() {
            let text = string_at(table, p).unwrap().to_string();
            p = (p + 4 + text.len()).next_multiple_of(4);
            seen.push(text);
        }
        assert_eq!(seen, ["line:0001", "Taro: \"Hello\" (there)", "*: The wind's blew", "Only the wind, and much longer than the Japanese was", "a heading", "Taro: ...", "line only"]);
        assert_eq!(file.objects[2].path_id, 12);
        assert_eq!(file.data(&file.objects[2]), [1, 2, 3, 4], "what comes after moved and stayed whole");

        assert_eq!(Game::open(&dir).unwrap().lines().len(), lines.len(), "reads the game's own text again");
        install(&dir, &line, &name).unwrap();
        assert_eq!(fs::read(&path).unwrap(), written);
        assert_eq!(crate::restore(&dir).unwrap().0, 1);
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = fs::remove_dir_all(dir);
    }

    /// The code's file in the known layout: its strings, and bytes after them that must stay put.
    fn code(texts: &[&str]) -> Vec<u8> {
        const HEADER: usize = 380;
        let mut starts = vec![0u32];
        texts.iter().for_each(|t| starts.push(starts[starts.len() - 1] + t.len() as u32));
        let (table, bytes) = (HEADER, HEADER + starts.len() * 4);
        let mut out = CODE_MAGIC.to_vec();
        for n in [CODE_LAYOUT, table, starts.len() * 4, starts.len(), bytes, starts[texts.len()] as usize, texts.len()] {
            out.extend((n as u32).to_le_bytes());
        }
        out.resize(HEADER, 0);
        starts.iter().for_each(|s| out.extend(s.to_le_bytes()));
        texts.iter().for_each(|t| out.extend(t.as_bytes()));
        out.extend(b"rest of the code");
        out
    }

    #[test]
    fn the_code_keeps_its_strings_in_step_with_the_objects() {
        let dir = game();
        let path = dir.join("Sample_Data").join(CODE);
        const ALL: [&str; 7] = ["ascii", "鍵の言葉", "{0}回目", "字", "始", "使う物", "\u{4e00}\u{a000}"];
        fs::write(&path, code(&ALL)).unwrap();
        fs::write(dir.join("Sample_Data/level1"), assets(&[(30, 1, { let mut b = Vec::new(); string(&mut b, "使う物"); b })])).unwrap();
        let lines = Game::open(&dir).unwrap().lines();
        let asked: Vec<&str> = lines.iter().filter(|l| l.0.starts_with("asset:")).map(|l| l.2.as_str()).collect();
        assert!(asked.contains(&"鍵の言葉") && asked.contains(&"{0}回目"));
        assert!(!asked.contains(&"字") && !asked.contains(&"使う物") && !asked.contains(&ALL[6]), "a lone letter, an object's name, which the code may use to find it, and a block's edges");

        let n = install(&dir, &|k| (k == "asset:鍵の言葉").then(|| "Key’s word".to_string()).or((k == "asset:{0}回目").then(|| "Try {0}".to_string())), &|_| None).unwrap();
        assert!(n >= 3);
        let raw = fs::read(&path).unwrap();
        let back = Code { path: path.clone(), starts: code_starts(&raw).unwrap(), raw };
        let texts: Vec<&str> = (0..7).map(|i| back.text(i).unwrap()).collect();
        assert_eq!(texts, ["ascii", "Key's word", "Try {0}", "字", "始", "使う物", ALL[6]]);
        assert!(back.raw.windows(16).any(|w| w == b"rest of the code"));
        let file = read_serialized(fs::read(dir.join("Sample_Data/level0")).unwrap()).unwrap();
        assert!(String::from_utf8_lossy(file.data(&file.objects[1])).contains("Key's word"), "the object says what the code says");

        crate::restore(&dir).unwrap();
        assert_eq!(fs::read(&path).unwrap(), code(&ALL));
        let _ = fs::remove_dir_all(dir);
    }

    /// An on-screen text object, with the fields around its size as they sit in one.
    fn label(script: i64, text: &str, size: f32) -> Vec<u8> {
        let mut b = behaviour(script, "", &[text]);
        b.extend([0; 12]);
        b.extend(size.to_le_bytes());
        b.extend(size.to_le_bytes());
        b.extend(400u32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(18f32.to_le_bytes());
        b.extend(72f32.to_le_bytes());
        b.extend([0; 40]);
        b
    }

    #[test]
    fn text_on_screen_may_shrink_to_its_box_with_or_without_japanese() {
        let dir = game();
        fs::write(dir.join("Sample_Data/level1"), assets(&[(40, MONO_BEHAVIOUR, label(LABEL, "始める", 32.0)), (41, MONO_BEHAVIOUR, label(LABEL, "0000", 24.0))])).unwrap();
        install(&dir, &|_| Some("A very long start".to_string()), &|_| None).unwrap();
        let file = read_serialized(fs::read(dir.join("Sample_Data/level1")).unwrap()).unwrap();
        for (i, size) in [(0, 32.0f32), (1, 24.0)] {
            let b = file.data(&file.objects[i]);
            let at = b.windows(4).position(|w| w == 400u32.to_le_bytes()).unwrap();
            assert_eq!(word(b, at + 4), 1, "switched on");
            assert_eq!(&b[at + 8..at + 16], [(size / 2.0).to_le_bytes(), size.to_le_bytes()].concat(), "from half the size up to the size it had");
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// A texture object as Unity 6 lays it out, with `pixels` inside it, or named in the file beside it.
    fn texture(w: u32, h: u32, format: u32, pixels: &[u8], beside: Option<&str>) -> Vec<u8> {
        let mut b = Vec::new();
        string(&mut b, "tex");
        b.extend(0u32.to_le_bytes());
        for n in [w, h, pixels.len() as u32, 0, format, 3] {
            b.extend(n.to_le_bytes());
        }
        b.extend(0u32.to_le_bytes()); // flags
        string(&mut b, "");
        b.extend(1u32.to_le_bytes()); // streamed
        b.extend([0; TO_PLATFORM - 4]);
        b.extend(0u32.to_le_bytes()); // platform data
        match beside {
            Some(file) => {
                b.extend(0u32.to_le_bytes());
                b.extend(2u64.to_le_bytes());
                b.extend((pixels.len() as u32).to_le_bytes());
                string(&mut b, file);
            }
            None => {
                b.extend((pixels.len() as u32).to_le_bytes());
                b.extend(pixels);
                b.resize(b.len().next_multiple_of(4), 0);
                b.extend([0; 16]);
            }
        }
        b
    }

    #[test]
    fn pictures_read_upright_from_either_place_and_go_back_inside_the_object() {
        let dir = game();
        let data = dir.join("Sample_Data");
        // bottom row first: red then green under blue then see-through white
        let mut pixels = Vec::new();
        for y in 0..64u8 {
            for x in 0..64u8 {
                pixels.extend(if y < 32 { if x < 32 { [255, 0, 0, 255] } else { [0, 255, 0, 255] } } else if x < 32 { [0, 0, 255, 255] } else { [255, 255, 255, 0] });
            }
        }
        fs::write(data.join("sharedassets0.assets.resS"), [&[9, 9], pixels.as_slice()].concat()).unwrap();
        fs::write(data.join("sharedassets1.assets"), assets(&[
            (50, TEXTURE, texture(64, 64, RGBA32 as u32, &pixels, None)),
            (51, TEXTURE, texture(64, 64, RGBA32 as u32, &pixels, Some("sharedassets0.assets.resS"))),
            (52, TEXTURE, texture(64, 64, 1, &[0; 16], None)),
            (53, TEXTURE, texture(32, 32, RGBA32 as u32, &[0; 16], None)),
        ])).unwrap();
        let g = Game::open(&dir).unwrap();
        assert_eq!(g.pictures(), ["sharedassets1.assets/50.png", "sharedassets1.assets/51.png"], "no mask or font sheet, and nothing too small to hold text");
        let img = g.picture("sharedassets1.assets/51.png").unwrap();
        assert_eq!(img, g.picture("sharedassets1.assets/50.png").unwrap());
        assert_eq!(img.px[0], 0x0000_00ff, "blue is on top");
        assert_eq!(img.px[63], 0xffff_ffff, "and white is see-through");
        assert_eq!(img.px[63 * 64], 0x00ff_0000, "red is at the bottom");

        let mut painted = img.clone();
        painted.px[0] = 0x0012_3456;
        let work = dir.join("vntl_work");
        crate::artemis::keep_painted(&dir, &[("sharedassets1.assets/51.png".to_string(), crate::menus::write_png(&painted).unwrap())]).unwrap();
        install(&dir, &|_| None, &|_| None).unwrap();
        let g = Game::open(&dir).unwrap();
        // the game's own picture is what the reader goes through again, and what's in the file is ours
        let file = read_serialized(fs::read(data.join("sharedassets1.assets")).unwrap()).unwrap();
        let blob = file.data(&file.objects[1]);
        let t = Texture::read(blob).unwrap();
        assert_eq!((t.format(blob), word(blob, t.fields + 8), word(blob, t.fields + 20), word(blob, t.streamed)), (RGBA32, 64 * 64 * 4, 1, 0));
        assert_eq!(t.image(blob, &data.join("sharedassets1.assets")).unwrap(), painted);
        assert_eq!(g.picture("sharedassets1.assets/51.png").unwrap(), img);
        let _ = fs::remove_dir_all(work);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn square_compression_decodes_with_the_top_row_first() {
        let dir = game();
        let data = dir.join("Sample_Data");
        // a row of black squares, then red ones
        let red = [0x00, 0xf8, 0x00, 0xf8, 0, 0, 0, 0];
        let black = [0; 8];
        let blocks: Vec<u8> = (0..16 * 16).flat_map(|i| if i < 16 { black } else { red }).collect();
        fs::write(data.join("sharedassets1.assets"), assets(&[(60, TEXTURE, texture(64, 64, DXT1 as u32, &blocks, None))])).unwrap();
        let img = Game::open(&dir).unwrap().picture("sharedassets1.assets/60.png").unwrap();
        assert_eq!(img.px[0] & 0xff_ffff, 0xff_0000, "red stays red");
        assert_eq!(img.px[63 * 64] & 0xff_ffff, 0, "the first block in the file is the bottom of the picture");
        assert_eq!(img.px[0] >> 24, 0, "and it's solid");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_game_with_no_yarn_table_is_refused_by_name() {
        let dir = game();
        fs::write(dir.join("Sample_Data/sharedassets0.assets"), assets(&[(12, 28, vec![1, 2, 3, 4])])).unwrap();
        assert!(Game::open(&dir).err().unwrap().contains("Yarn Spinner"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn only_a_length_with_japanese_after_it_is_a_string() {
        let mut b = vec![0; 8];
        string(&mut b, "ascii only");
        b.extend(9000u32.to_le_bytes());
        string(&mut b, "日本語");
        string(&mut b, "改\n行");
        let found: Vec<&[u8]> = strings(&b, 0).iter().map(|&(s, e)| &b[s..e]).collect();
        assert_eq!(found, ["日本語".as_bytes(), "改\n行".as_bytes()]);
    }
}
