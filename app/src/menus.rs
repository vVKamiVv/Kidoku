//! Text painted into a game's interface images: find it, have the local model read and translate
//! it, repaint it in English. Only flat or transparent backgrounds qualify, so artwork is never touched.

use crate::fonts::{self, Sample, SAMPLES};
use crate::inpaint::{self, Painter};
use crate::liar::{self, Image};
use crate::llm::{parse_numbered, Chat};
use crate::ocr::{Area, Reader};
use crate::pipeline::Event;
use crate::reference::{archive_files, read_game_file};
use crate::renpy::{load_json, save_json, tl_dir};
use crate::{artemis, bgi, rpgmaker};
use crate::text::JA;
use crate::{files_under, original, replace_file, R};
use flate2::{write::ZlibEncoder, Compression, Crc};
use fontdue::{Font, FontSettings};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

/// Loanwords menus are full of, for the image model to read look-alike katakana by.
// short words it reads anyway are left out: given one, it answers it for an arrow or a dot
const MENU_WORDS: [&str; 13] = ["フルスクリーン", "キャンセル", "スキップ", "オート", "テキスト", "メッセージ", "バックログ", "システム", "クイック", "カーソル", "マウス", "エフェクト", "ムービー"];
static READ_PROMPT: LazyLock<String> = LazyLock::new(|| format!(
    "This image is one label from the menus of a Japanese visual novel: its settings, save, load or title screens. \
     Write out exactly the Japanese text it shows, nothing else, and leave out an English line printed under it as a subtitle. \
     Read each katakana letter with care: menu words are often loanwords such as {}. \
     If there is no text, or the text is not in Japanese, reply NONE.",
    MENU_WORDS.join(", ")
));
const LABELS_HELP: &str = "# Text read from the game's menu images and its English, one per line: readings = English
# Change the English to correct a label, then run the translation again. Lines starting with # are ignored.
# A label ending [short] is the English used where the full one doesn't fit.
";
/// What marks a label's short English in image_labels.txt.
const SHORT: &str = " [short]";
const MEMORY_HELP: &str = "# Menu labels whose English the story's model corrected, in any game: readings = English
# Used again wherever a label reads the same, and shown to the model as examples. Lines starting with # are ignored.
";
// shortcut: where Liar-soft games keep menus in the two games' worth of evidence we have; scan every
// archive once a game keeps them elsewhere (the flat-background test already protects artwork).
const ARCHIVES: [&str; 2] = ["grpo.xfl", "grps.xfl"];
const FONTS: [&str; 2] = ["C:/Windows/Fonts/trebucbd.ttf", "C:/Windows/Fonts/arialbd.ttf"];
const CLEAR: u32 = 0xff00_0000; // a fully transparent pixel, as images store it
const SS: usize = 4; // text is drawn this many times larger, then scaled down, for smooth edges
/// Taller than this, an image isn't just a button.
const BUTTON_HEIGHT: usize = 200;
/// On a bigger image only a small block can be a label. Art is one big connected shape, and treating
/// that as text would erase it, so anything larger stays as it is.
const TALLEST_LABEL: usize = 64;
/// English is drawn no smaller than this; past it, the label is shortened instead.
const SMALLEST: f32 = 9.0;
/// Between readings of one label, as the translation is shown them.
const BOTH: &str = " / ";

#[derive(Clone)]
struct Block {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    mask: Vec<bool>,
    group: usize,
}

struct Part {
    entry: usize,
    sprite: Option<usize>,
    at: (i32, i32), // where a container places it on screen
    name: String,
    img: Image,
    bg: u32,
    blocks: Vec<Block>,
}

struct Group {
    key: String,
    first: (usize, usize), // (part, block)
    second: Option<(usize, usize)>,
    reads: Vec<String>,
}

fn rgb(p: u32) -> [i32; 3] {
    [(p >> 16 & 255) as i32, (p >> 8 & 255) as i32, (p & 255) as i32]
}

fn differs(a: u32, b: u32) -> bool {
    if b == CLEAR {
        return a >> 24 < 195; // on nothing, only opacity counts: dark and light text must match
    }
    let (x, y) = (rgb(a), rgb(b));
    (0..3).map(|i| (x[i] - y[i]).abs()).sum::<i32>() + ((a >> 24) as i32 - (b >> 24) as i32).abs() > 60
}

/// The colour behind text, if the image has one: artwork has no single dominant colour.
fn flat_background(img: &Image) -> Option<u32> {
    if img.px.iter().filter(|p| **p >> 24 == 255).count() * 2 >= img.px.len() {
        return Some(CLEAR);
    }
    let mut count = HashMap::new();
    let visible = img.px.iter().filter(|p| **p >> 24 != 255);
    let total = visible.clone().count();
    visible.for_each(|p| *count.entry(*p).or_insert(0usize) += 1);
    let (bg, _) = count.into_iter().max_by_key(|(_, n)| *n)?;
    // a face can be two near colours in a fine pattern
    let n = img.px.iter().filter(|p| **p >> 24 != 255 && near(**p, bg)).count();
    (n * 100 >= total * 35).then_some(bg)
}

/// Colours a face's fine pattern mixes, as one.
fn near(a: u32, b: u32) -> bool {
    let (x, y) = (rgb(a), rgb(b));
    (0..3).map(|i| (x[i] - y[i]).abs()).sum::<i32>() + ((a >> 24) as i32 - (b >> 24) as i32).abs() <= 24
}

/// Each row's extent of the largest connected area of background: text lies inside it. A frame's
/// outer edge is often the same colour as the fill, but the frame itself cuts it off.
fn spans(img: &Image, bg: u32) -> Vec<Option<(usize, usize)>> {
    let (w, h) = (img.w, img.h);
    let back = |p: u32| if bg == CLEAR { !differs(p, CLEAR) } else { near(p, bg) };
    let (mut seen, mut inside) = (vec![false; w * h], Vec::new());
    for start in 0..w * h {
        if seen[start] || !back(img.px[start]) {
            continue;
        }
        seen[start] = true;
        let (mut stack, mut area) = (vec![start], Vec::new());
        while let Some(i) = stack.pop() {
            area.push(i);
            let (x, y) = (i % w, i / w);
            for (ok, n) in [(x > 0, i.wrapping_sub(1)), (x + 1 < w, i + 1), (y > 0, i.wrapping_sub(w)), (y + 1 < h, i + w)] {
                if ok && !seen[n] && back(img.px[n]) {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        if area.len() > inside.len() {
            inside = area;
        }
    }
    // on nothing, text may touch the image's edge, so it has whole rows; unless the most nothing is
    // shut in by a frame, as inside a pill-shaped tab, which stays
    let edge = |i: &usize| i.is_multiple_of(w) || i % w == w - 1 || i / w == 0 || i / w == h - 1;
    if bg == CLEAR && (inside.is_empty() || inside.iter().any(edge)) {
        return vec![Some((0, w - 1)); h];
    }
    let mut out: Vec<Option<(usize, usize)>> = vec![None; h];
    for i in inside {
        let (x, y) = (i % w, i / w);
        out[y] = Some(out[y].map_or((x, x), |(l, r)| (l.min(x), r.max(x))));
    }
    out
}

fn find_blocks(img: &Image, bg: u32) -> Vec<Block> {
    // on nothing, a shape this big and solid is a button with its text inside, not text
    shapes(img, bg).into_iter().filter(|b| !(bg == CLEAR && is_solid(b))).collect()
}

fn is_solid(b: &Block) -> bool {
    let (h, filled) = (b.y1 - b.y0, b.mask.iter().filter(|m| **m).count() * 100);
    // a bar with words on it is low, but letters on their own never fill a long row that full
    (h >= 24 && filled >= b.mask.len() * 70) || (h >= 10 && b.x1 - b.x0 >= 6 * h && filled >= b.mask.len() * 85)
}

fn shapes(img: &Image, bg: u32) -> Vec<Block> {
    let sp = spans(img, bg);
    let ink = |x: usize, y: usize| sp[y].is_some_and(|(l, r)| x >= l && x <= r && differs(img.px[y * img.w + x], bg));
    let mut out = Vec::new();
    let mut y = 0;
    while y < img.h {
        if !(0..img.w).any(|x| ink(x, y)) {
            y += 1;
            continue;
        }
        let top = y;
        while y < img.h && (0..img.w).any(|x| ink(x, y)) {
            y += 1;
        }
        let cols: Vec<usize> = (0..img.w).filter(|x| (top..y).any(|r| ink(*x, r))).collect();
        let mut start = cols[0];
        for (i, x) in cols.iter().enumerate() {
            let next = cols.get(i + 1);
            if next.is_none_or(|n| n - x > 18) {
                if x + 1 - start >= 14 && y - top >= 8 {
                    let mask: Vec<bool> = (top..y).flat_map(|r| (start..=*x).map(move |c| (c, r))).map(|(c, r)| ink(c, r)).collect();
                    out.push(Block { x0: start, y0: top, x1: x + 1, y1: y, mask, group: 0 });
                }
                start = next.copied().unwrap_or(0);
            }
        }
    }
    out
}

/// An image worth reading: it has a background, and something on it.
fn scan(entry: usize, sprite: Option<usize>, at: (i32, i32), name: String, mut img: Image) -> Option<Part> {
    // a sprite can be empty
    if img.w == 0 || img.h == 0 {
        return None;
    }
    // bold lettering cut close can outnumber the nothing around it, which its edges still show; a
    // button's face covers most of it, whatever its corners do
    let edge: Vec<u32> = (0..img.w).flat_map(|x| [img.px[x], img.px[(img.h - 1) * img.w + x]]).chain((0..img.h).flat_map(|y| [img.px[y * img.w], img.px[y * img.w + img.w - 1]])).collect();
    let on_nothing = edge.iter().filter(|p| **p >> 24 == 255).count() * 2 >= edge.len();
    let bg = match flat_background(&img) {
        Some(face) if face != CLEAR && on_nothing && img.px.iter().filter(|p| near(**p, face)).count() * 2 < img.px.len() => CLEAR,
        Some(bg) => bg,
        None if on_nothing => CLEAR,
        None => return None,
    };
    if bg == CLEAR {
        // transparent pixels can hide any colour; make them all the same
        img.px.iter_mut().filter(|p| **p >> 24 == 255).for_each(|p| *p = CLEAR);
    }
    let mut blocks = find_blocks(&img, bg);
    blocks.retain(|b| !is_box(b));
    (!blocks.is_empty()).then_some(Part { entry, sprite, at, name, img, bg, blocks })
}

/// An empty square, as a checkbox is: the reader takes it for 口, but it holds no text.
fn is_box(b: &Block) -> bool {
    let w = b.x1 - b.x0;
    let rows: Vec<usize> = (0..b.y1 - b.y0).filter(|&y| b.mask[y * w..(y + 1) * w].iter().any(|m| *m)).collect();
    let (Some(&top), Some(&bottom)) = (rows.first(), rows.last()) else { return false };
    let h = bottom + 1 - top;
    let filled = |x: usize, y: usize| b.mask[(top + y) * w + x];
    w >= 7 && w.abs_diff(h) <= 3 && (h / 4..h - h / 4).all(|y| (w / 4..w - w / 4).all(|x| !filled(x, y)))
        && (0..h).all(|y| filled(0, y) || filled(1, y)) && (0..w).all(|x| filled(x, 0) || filled(x, 1))
}

/// A button's states have the same letter shapes, whatever their colours.
fn same_text(a: &Block, b: &Block, share: usize) -> bool {
    let (aw, bw) = (a.x1 - a.x0, b.x1 - b.x0);
    if aw.abs_diff(bw) > 2 || (a.y1 - a.y0).abs_diff(b.y1 - b.y0) > 2 {
        return false;
    }
    // compare ink only: short labels are mostly blank, so blank pixels would make any two match
    let (w, h) = (aw.min(bw), (a.y1 - a.y0).min(b.y1 - b.y0));
    let (mut both, mut either) = (0, 0);
    for (x, y) in (0..h).flat_map(|y| (0..w).map(move |x| (x, y))) {
        let (p, q) = (a.mask[y * aw + x], b.mask[y * bw + x]);
        both += (p && q) as usize;
        either += (p || q) as usize;
    }
    both * 100 >= either * share
}

// ---------- reading ----------

/// A block enlarged, on solid paper.
fn block_image(part: &Part, b: &Block, scale: usize) -> Image {
    let img = &part.img;
    let (x0, y0) = (b.x0.saturating_sub(6), b.y0.saturating_sub(6));
    let (x1, y1) = ((b.x1 + 6).min(img.w), (b.y1 + 6).min(img.h));
    let (w, h) = ((x1 - x0) * scale, (y1 - y0) * scale);
    // the margin shows only background: a neighbour's edge there reads as a letter
    let px = |x: usize, y: usize| {
        let p = img.px[y * img.w + x];
        let outside = x < b.x0 || x >= b.x1 || y < b.y0 || y >= b.y1;
        if outside && differs(p, part.bg) { part.bg } else { p }
    };
    // transparent pixels need a backdrop: white under dark text, or the model sees nothing
    let seen: Vec<u32> = (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).map(|(x, y)| px(x, y)).filter(|p| p >> 24 < 255).collect();
    // a dimmed label is faint all over: its letters are judged, and shown, at full strength
    let strongest = seen.iter().map(|p| 255 - (p >> 24)).max().unwrap_or(255).max(1) as f32 / 255.0;
    let solid: Vec<i32> = seen.iter().filter(|p| (255 - (*p >> 24)) as f32 >= 127.0 * strongest).map(|p| rgb(*p).iter().sum()).collect();
    let mut paper = if solid.iter().sum::<i32>() < 382 * solid.len() as i32 { 255.0 } else { 0.0 };
    // letters in an outline read as themselves on paper of the outline's colour
    let bw = b.x1 - b.x0;
    let inked = |x: isize, y: isize| x >= 0 && y >= 0 && (x as usize) < bw && (y as usize) < b.y1 - b.y0 && b.mask[y as usize * bw + x as usize];
    let (mut rim, mut core) = (Vec::new(), Vec::new());
    for (i, _) in b.mask.iter().enumerate().filter(|(_, m)| **m) {
        let (x, y) = ((i % bw) as isize, (i / bw) as isize);
        let luma = rgb(img.px[(b.y0 + i / bw) * img.w + b.x0 + i % bw]).iter().sum::<i32>() / 3;
        let edge = ![(-1, 0), (1, 0), (0, -1), (0, 1)].iter().all(|&(dx, dy)| inked(x + dx, y + dy));
        if edge { rim.push(luma) } else { core.push(luma) }
    }
    let share = |v: &[i32], test: &dyn Fn(i32) -> bool| v.iter().filter(|l| test(**l)).count() * 100 / v.len().max(1);
    if share(&rim, &|l| l >= 200) >= 60 && share(&core, &|l| l < 100) >= 15 {
        paper = 255.0;
    } else if share(&rim, &|l| l < 55) >= 60 && share(&core, &|l| l > 155) >= 15 {
        paper = 0.0;
    }
    let at = |x: usize, y: usize, c: usize| -> f32 {
        let p = px(x, y);
        let a = ((255 - (p >> 24)) as f32 / 255.0 / strongest).min(1.0);
        rgb(p)[c] as f32 * a + paper * (1.0 - a)
    };
    let mut px = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = ((x as f32 + 0.5) / scale as f32 - 0.5 + x0 as f32, (y as f32 + 0.5) / scale as f32 - 0.5 + y0 as f32);
            let (l, t) = (fx.floor().max(x0 as f32) as usize, fy.floor().max(y0 as f32) as usize);
            let (r, b2) = ((l + 1).min(x1 - 1), (t + 1).min(y1 - 1));
            let (dx, dy) = ((fx - l as f32).clamp(0.0, 1.0), (fy - t as f32).clamp(0.0, 1.0));
            let mix = |c: usize| {
                let top = at(l, t, c) * (1.0 - dx) + at(r, t, c) * dx;
                let low = at(l, b2, c) * (1.0 - dx) + at(r, b2, c) * dx;
                (top * (1.0 - dy) + low * dy) as u32
            };
            px.push(mix(0) << 16 | mix(1) << 8 | mix(2));
        }
    }
    // pale lettering on a pale face, as a lit-up button's, is misread as drawn: it's shown black on white
    if part.bg >> 24 == 0 {
        let luma = |p: u32| rgb(p).iter().sum::<i32>() / 3;
        let face = luma(part.bg);
        let ink = b.mask.iter().enumerate().filter(|(_, m)| **m).map(|(i, _)| img.px[(b.y0 + i / bw) * img.w + b.x0 + i % bw]);
        let far = ink.map(|p| (luma(p) - face).abs()).max().unwrap_or(0);
        if (1..80).contains(&far) {
            for p in px.iter_mut() {
                let g = (255 - ((luma(*p) - face).abs() * 255 / far).min(255)) as u32;
                *p = g << 16 | g << 8 | g;
            }
        }
    }
    Image { w, h, px }
}

/// A PNG of unfiltered rows. `kind` is the PNG colour type: 2 is RGB, 6 is RGBA.
fn png_bytes(w: usize, h: usize, kind: u8, raw: &[u8]) -> R<Vec<u8>> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(raw).map_err(|e| e.to_string())?;
    let idat = z.finish().map_err(|e| e.to_string())?;
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |name: &[u8], body: &[u8]| {
        png.extend((body.len() as u32).to_be_bytes());
        let mut crc = Crc::new();
        crc.update(name);
        crc.update(body);
        png.extend(name);
        png.extend(body);
        png.extend(crc.sum().to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend((w as u32).to_be_bytes());
    ihdr.extend((h as u32).to_be_bytes());
    ihdr.extend([8, kind, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &idat);
    chunk(b"IEND", b"");
    Ok(png)
}

pub(crate) fn write_png(img: &Image) -> R<Vec<u8>> {
    let mut raw = Vec::with_capacity((img.w * 4 + 1) * img.h);
    for row in img.px.chunks(img.w) {
        raw.push(0);
        for p in row {
            let [r, g, b] = rgb(*p);
            raw.extend([r as u8, g as u8, b as u8, 255 - (*p >> 24) as u8]);
        }
    }
    png_bytes(img.w, img.h, 6, &raw)
}

pub(crate) const PNG: &[u8] = b"\x89PNG";

pub(crate) fn stored(c: &[u8]) -> u32 {
    ((255 - c[3] as u32) << 24) | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32
}

/// Any PNG, as the same stored pixels WCG images use.
pub fn read_png(data: &[u8]) -> R<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("PNG too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let px = match info.color_type {
        png::ColorType::Rgba => buf.as_chunks::<4>().0.iter().take(info.width as usize * info.height as usize).map(|c| stored(c)).collect(),
        png::ColorType::GrayscaleAlpha => buf.as_chunks::<2>().0.iter().take(info.width as usize * info.height as usize).map(|c| stored(&[c[0], c[0], c[0], c[1]])).collect(),
        other => return Err(format!("unexpected PNG colours {other:?}")),
    };
    Ok(Image { w: info.width as usize, h: info.height as usize, px })
}

/// A PNG, or the WebP, JPEG or AVIF a game keeps its pictures in instead.
pub fn read_picture(data: &[u8]) -> R<Image> {
    if data.starts_with(PNG) {
        return read_png(data);
    }
    if crate::avif::is_avif(data) {
        return crate::avif::read(data);
    }
    let img = image::load_from_memory(data).map_err(|e| e.to_string())?.into_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Ok(Image { w, h, px: img.as_chunks::<4>().0.iter().map(|c| stored(c)).collect() })
}

/// Whether a lower-case file name is a picture `read_picture` takes.
pub(crate) fn is_picture(name: &str) -> bool {
    [".png", ".webp", ".jpg", ".jpeg", ".avif"].iter().any(|e| name.ends_with(e))
}

fn picture_height(data: &[u8]) -> Option<u32> {
    if data.starts_with(PNG) {
        return Some(u32::from_be_bytes(data.get(20..24)?.try_into().ok()?));
    }
    if crate::avif::is_avif(data) {
        return crate::avif::height(data);
    }
    image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format().ok()?.into_dimensions().ok().map(|d| d.1)
}

/// A picture no taller than a button, because full screens are artwork. Those are most of a game's
/// images, so the header rules them out before the slow decode.
fn button_image(data: &[u8]) -> Option<Image> {
    (picture_height(data)? <= BUTTON_HEIGHT as u32).then(|| read_picture(data).ok()).flatten()
}

/// Labels don't end in punctuation: that's misreading the edge of the crop.
fn clean(text: String) -> String {
    // nor hold a checkbox: that's the box beside it, read as a square or a line's corner
    let boxes = |c: char| ['□', '■', '☐', '☑', '☒', '◻', '◼'].contains(&c) || ('\u{2500}'..='\u{257f}').contains(&c);
    // or the dots either side of a button's word
    text.chars().filter(|c| !boxes(*c)).collect::<String>().trim_matches('・').trim_end_matches(['？', '?', '！', '!', '。', '|']).to_string()
}

/// Which of two readings of a label the image shows, asked with the picture again: told what to
/// compare, a model looks at each letter as it doesn't when reading freely. Blank if neither, as
/// for an arrow. It leans to the first it's shown, so it's asked both ways round: None if its
/// answer follows the order.
fn choose(vision: &dyn Chat, png: &[u8], a: &str, b: &str) -> R<Option<String>> {
    let pick = |first: &str, second: &str| -> R<Option<String>> {
        let prompt = format!(
            "This image is one label from the menus of a Japanese visual novel. Two readings of it differ:\n1: {first}\n2: {second}\n\
             Compare each letter of them with the image. Reply with only the number of the reading that matches it exactly, \
             or 0 if the image shows no Japanese text at all."
        );
        Ok(match vision.read_image(png, &prompt)?.trim_start().chars().next() {
            Some('1') => Some(first.to_string()),
            Some('2') => Some(second.to_string()),
            Some('0') => Some(String::new()),
            _ => None,
        })
    };
    let (one, two) = (pick(a, b)?, pick(b, a)?);
    Ok(one.filter(|o| Some(o) == two.as_ref()))
}

/// Words from `words` a reading is one letter off, or two in a long one: the closest stretch of
/// any of them, if it isn't the reading itself. Short readings are left alone, as many words are a
/// letter apart.
fn nearest(words: &[Vec<char>], reading: &str) -> Option<String> {
    let p: Vec<char> = reading.chars().collect();
    let most = p.len() / 5;
    if most == 0 {
        return None;
    }
    let mut best: Option<(usize, &[char])> = None;
    let (mut col, mut next) = (vec![(0, 0); p.len() + 1], vec![(0, 0); p.len() + 1]);
    for w in words {
        // the cheapest edits turning `p` into some stretch of `w`, with where each stretch starts
        col.iter_mut().enumerate().for_each(|(i, c)| *c = (i, 0));
        for (j, c) in w.iter().enumerate() {
            next[0] = (0, j + 1);
            for i in 1..=p.len() {
                let swap = (col[i - 1].0 + usize::from(p[i - 1] != *c), col[i - 1].1);
                next[i] = [swap, (col[i].0 + 1, col[i].1), (next[i - 1].0 + 1, next[i - 1].1)].into_iter().min_by_key(|e| e.0).unwrap();
            }
            std::mem::swap(&mut col, &mut next);
            let (cost, start) = col[p.len()];
            if cost <= most && best.is_none_or(|b| cost < b.0) {
                best = Some((cost, &w[start..=j]));
            }
        }
    }
    best.filter(|b| b.0 > 0).map(|b| b.1.iter().collect())
}

/// Of two readings, the one the words model calls real Japanese asked of each on its own, when it
/// calls the other not: a model that takes the first of two either way round still knows
/// ハッピーテイ is no word. It calls a misreading real more often than that, so this is only for
/// readings a voicing mark apart.
fn real_of_two(vision: &dyn Chat, a: &str, b: &str) -> R<Option<String>> {
    let real = |t: &str| -> R<bool> {
        let prompt = format!("A label from a Japanese game's menus was read as: {t}\nIs that real Japanese, as a game would write it, with no letter misread? Reply YES or NO.");
        Ok(vision.chat("", &prompt)?.trim_start().to_uppercase().starts_with("YES"))
    };
    Ok(match (real(a)?, real(b)?) {
        (true, false) => Some(a.to_string()),
        (false, true) => Some(b.to_string()),
        _ => None,
    })
}

/// Which of two readings is real Japanese, asked without the picture but with the labels read
/// beside it, both ways round as `choose` asks. None if the model can't tell.
fn likelier(vision: &dyn Chat, a: &str, b: &str, beside: &[String]) -> R<Option<String>> {
    let others = if beside.is_empty() { String::new() } else { format!("Other labels on the same screen: {}.\n", beside.join(", ")) };
    let pick = |first: &str, second: &str| -> R<Option<String>> {
        let prompt = format!(
            "Two readings of one label from a Japanese game's menus differ, and one of them was misread:\n1: {first}\n2: {second}\n\
             {others}Which is the label, as a game menu would write it? Reply with only its number."
        );
        Ok(match vision.chat("", &prompt)?.trim_start().chars().next() {
            Some('1') => Some(first.to_string()),
            Some('2') => Some(second.to_string()),
            _ => None,
        })
    };
    let (one, two) = (pick(a, b)?, pick(b, a)?);
    Ok(one.filter(|o| Some(o) == two.as_ref()))
}

/// The speaker's name a short reading is near, for a name in lettering the model misreads: half its
/// letters may be off, as the picture is checked after.
fn nearest_name(people: &[Vec<char>], reading: &str) -> Option<String> {
    let p: Vec<char> = reading.chars().collect();
    if !(2..=5).contains(&p.len()) {
        return None;
    }
    let distance = |w: &[char]| {
        let mut row: Vec<usize> = (0..=w.len()).collect();
        for (i, a) in p.iter().enumerate() {
            let mut next = vec![i + 1];
            for (j, b) in w.iter().enumerate() {
                next.push((row[j] + usize::from(a != b)).min(row[j + 1] + 1).min(next[j] + 1));
            }
            row = next;
        }
        row[w.len()]
    };
    let (d, name) = people.iter().map(|w| (distance(w), w)).min_by_key(|(d, _)| *d)?;
    (d > 0 && d <= p.len() / 2).then(|| name.iter().collect())
}

/// Whether a picture has Japanese in it: a plain question the model answers well, though reading
/// freely it turns English and shapes into Japanese words.
fn shows_japanese(vision: &dyn Chat, png: &[u8]) -> R<bool> {
    Ok(text_kind(vision, png)?.starts_with("JAPANESE"))
}

/// What a picture's text is written in: JAPANESE, ENGLISH or NONE, as the model says it.
fn text_kind(vision: &dyn Chat, png: &[u8]) -> R<String> {
    let prompt = "This image is cut from the menus of a Japanese visual novel. Is any text in it written in Japanese letters \
                  (kana or kanji)? Reply JAPANESE if it is, ENGLISH if its only text is in Latin letters or digits, or NONE if it has no text.";
    Ok(vision.read_image(png, prompt)?.trim_start().to_uppercase())
}

/// What a name plate says, if it's Japanese: the text reader's reading, or the model's if it saw
/// nothing. A plate floats on nothing, so it's tried on black and on white, for either colour of
/// lettering. Letters that aren't Japanese are an answer: a small model asked again invents a name.
pub fn read_plate(reader: Option<&Reader>, vision: Option<&dyn Chat>, img: &Image) -> R<Option<String>> {
    let on = |paper: u32| {
        let mix = |p: u32, shift: u32| {
            let a = 255 - (p >> 24);
            (((p >> shift & 255) * a + (paper >> shift & 255) * (255 - a)) / 255) << shift
        };
        Image { w: img.w, h: img.h, px: img.px.iter().map(|&p| mix(p, 16) | mix(p, 8) | mix(p, 0)).collect() }
    };
    let mut saw_letters = false;
    if let Some(reader) = reader {
        for paper in [0, 0xff_ffff] {
            let text = clean(reader.line(&on(paper))?);
            if JA.is_match(&text) {
                return Ok(Some(text));
            }
            saw_letters |= !text.is_empty();
        }
    }
    let Some(vision) = vision.filter(|_| !saw_letters) else { return Ok(None) };
    let text: String = vision.read_image(&write_png(&on(0))?, &READ_PROMPT)?.split_whitespace().collect();
    Ok(Some(clean(text)).filter(|t| JA.is_match(t)))
}

fn read_block(vision: &dyn Chat, png: &[u8]) -> R<String> {
    Ok(vision.read_image(png, &READ_PROMPT)?.split_whitespace().collect::<String>())
}

// ---------- painting ----------

fn over(px: &mut u32, color: [i32; 3], a: f32) {
    let under = (255 - (*px >> 24)) as f32 / 255.0;
    let keep = under * (1.0 - a);
    let shown = a + keep;
    if shown <= 0.0 {
        return;
    }
    let old = rgb(*px);
    let mix = |c: usize| ((old[c] as f32 * keep + color[c] as f32 * a) / shown + 0.5) as u32;
    *px = (255 - (shown * 255.0 + 0.5) as u32) << 24 | mix(0) << 16 | mix(1) << 8 | mix(2);
}

fn text_width(font: &Font, text: &str, size: f32) -> f32 {
    text.chars().map(|c| font.metrics(c, size).advance_width).sum()
}

/// `anchor` is the x the text is aligned to, on a row centred on `cy`. `align` says how: 0 starts
/// there, 0.5 centres on it, 1 ends there.
#[allow(clippy::too_many_arguments)]
fn draw(img: &mut Image, font: &Font, text: &str, size: f32, anchor: f32, align: f32, cy: f32, fill: [i32; 3], stroke: Option<[i32; 3]>) -> R<()> {
    let (w, h) = (img.w * SS, img.h * SS);
    let big = size * SS as f32;
    let (mut cover, mut edge) = (vec![0u8; w * h], vec![0u8; w * h]);
    let width = text_width(font, text, big);
    let mut pen = anchor * SS as f32 - width * align;
    let line = font.horizontal_line_metrics(big).ok_or("the font has no line metrics")?;
    let baseline = cy * SS as f32 + (line.ascent + line.descent) / 2.0;
    let r = (2 * SS) as i32;
    let around: Vec<(i32, i32)> = (-r..=r).flat_map(|y| (-r..=r).map(move |x| (x, y))).filter(|(x, y)| x * x + y * y <= r * r).collect();
    for c in text.chars() {
        let (m, bitmap) = font.rasterize(c, big);
        let (left, top) = (pen as i32 + m.xmin, baseline as i32 - m.ymin - m.height as i32);
        for gy in 0..m.height {
            for gx in 0..m.width {
                let v = bitmap[gy * m.width + gx];
                if v == 0 {
                    continue;
                }
                let (x, y) = (left + gx as i32, top + gy as i32);
                let put = |buf: &mut [u8], x: i32, y: i32| {
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        let i = y as usize * w + x as usize;
                        buf[i] = buf[i].max(v);
                    }
                };
                put(&mut cover, x, y);
                if stroke.is_some() && v > 100 {
                    around.iter().for_each(|(dx, dy)| put(&mut edge, x + dx, y + dy));
                }
            }
        }
        pen += m.advance_width;
    }
    let shrink = |buf: &[u8], x: usize, y: usize| -> f32 {
        (0..SS).flat_map(|j| (0..SS).map(move |i| (i, j))).map(|(i, j)| buf[(y * SS + j) * w + x * SS + i] as f32).sum::<f32>() / (255 * SS * SS) as f32
    };
    for y in 0..img.h {
        for x in 0..img.w {
            let (e, c) = (shrink(&edge, x, y), shrink(&cover, x, y));
            if e > 0.0 {
                over(&mut img.px[y * img.w + x], stroke.unwrap_or_default(), e);
            }
            if c > 0.0 {
                over(&mut img.px[y * img.w + x], fill, c);
            }
        }
    }
    Ok(())
}

/// Erase a part's labelled blocks and draw their English, keeping the look of the original text.
fn repaint(part: &Part, labels: &[Option<&String>], font: &Font) -> R<Image> {
    let mut img = part.img.clone();
    let sp = spans(&img, part.bg);
    let bg = rgb(part.bg);
    let luma = (bg[0] + bg[1] + bg[2]) / 3;
    for (b, label) in part.blocks.iter().zip(labels) {
        let Some(text) = label else { continue };
        let (mut ink, mut rim) = (Vec::new(), Vec::new());
        let bw = b.x1 - b.x0;
        let inked = |x: usize, y: usize| x >= b.x0 && x < b.x1 && y >= b.y0 && y < b.y1 && b.mask[(y - b.y0) * bw + x - b.x0];
        #[allow(clippy::needless_range_loop)] // y also finds the row in the image and the mask
        for y in b.y0..b.y1 {
            for x in b.x0..b.x1 {
                if inked(x, y) {
                    ink.push(rgb(part.img.px[y * img.w + x]));
                    if !(inked(x.wrapping_sub(1), y) && inked(x + 1, y) && inked(x, y.wrapping_sub(1)) && inked(x, y + 1)) {
                        rim.push(*ink.last().unwrap());
                    }
                }
            }
            let Some((l, r)) = sp[y] else { continue };
            for x in b.x0.saturating_sub(2).max(l)..=(b.x1 + 1).min(r) {
                img.px[y * img.w + x] = part.bg;
            }
        }
        if part.bg == CLEAR {
            // the faint edge of letters on nothing reaches past their ink
            for y in b.y0.saturating_sub(3)..(b.y1 + 3).min(img.h) {
                for x in b.x0.saturating_sub(3)..(b.x1 + 3).min(img.w) {
                    let i = y * img.w + x;
                    if img.px[i] == part.img.px[i] && !differs(img.px[i], CLEAR) {
                        img.px[i] = CLEAR;
                    }
                }
            }
        }
        let share = |test: &dyn Fn(i32) -> bool| ink.iter().filter(|c| test((c[0] + c[1] + c[2]) / 3)).count() * 100 / ink.len().max(1);
        let rim_light = rim.iter().filter(|c| (c[0] + c[1] + c[2]) / 3 >= 200).count() * 100 / rim.len().max(1);
        // a thick light outline can leave thin dark letters a small share, but its edge is all light
        let outlined = (share(&|v| v >= 200) >= 15 && share(&|v| v <= 50) >= 15) || (90..=170).contains(&luma) || (rim_light >= 90 && share(&|v| v <= 50) >= 8);
        let clear = part.bg == CLEAR;
        // an outline is usually dark round light letters, and sometimes light round dark ones
        let light_edge = outlined && rim_light >= 50;
        // with nothing behind it, the text keeps its own colour; on a button, it contrasts with the fill.
        // an outlined label's own colour is what lies inside its outline
        let inner = |v: i32| if light_edge { v <= 50 } else { v >= 200 };
        let common = ink.iter().filter(|c| !outlined || inner((c[0] + c[1] + c[2]) / 3)).fold(HashMap::new(), |mut n: HashMap<[i32; 3], usize>, c| {
            *n.entry(*c).or_default() += 1;
            n
        });
        let own = common.into_iter().max_by_key(|(_, n)| *n).map_or([0; 3], |(c, _)| c);
        let fill = if clear || light_edge { own } else if outlined || luma < 128 { [255; 3] } else { [0; 3] };
        let (anchor, align, room) = place(part, labels, &sp, b);
        let stroke = if outlined { 4.0 } else { 0.0 };
        let mut size = ((b.y1 - b.y0) as f32 * 0.95).clamp(11.0, 26.0);
        while size > SMALLEST && text_width(font, text, size) + stroke > room as f32 {
            size -= 1.0;
        }
        draw(&mut img, font, text, size, anchor, align, (b.y0 + b.y1) as f32 / 2.0, fill, outlined.then_some(if light_edge { [255; 3] } else { [0; 3] }))?;
    }
    Ok(img)
}

/// Where a label's English goes: the x it's aligned to, how (as `draw` takes it), and how wide it may be.
fn place(part: &Part, labels: &[Option<&String>], sp: &[Option<(usize, usize)>], b: &Block) -> (f32, f32, usize) {
    let img = &part.img;
    let clear = part.bg == CLEAR;
    let (l, r) = sp[(b.y0 + b.y1) / 2].unwrap_or((0, img.w - 1));
    let alone = part.blocks.len() == 1;
    // on nothing, labels starting together are a column ranged left, whatever their ends do
    let left = clear && part.blocks.iter().zip(labels).filter(|(o, l)| l.is_some() && o.x0.abs_diff(b.x0) <= 3).count() >= 2;
    // only labels count: untranslated text that happens to line up is not a column
    let near: Vec<usize> = part.blocks.iter().zip(labels).filter(|(o, l)| l.is_some() && o.x1.abs_diff(b.x1) <= 8).map(|(o, _)| o.x1).collect();
    let right = (near.len() >= 3 && !alone && !left).then(|| near.into_iter().max().unwrap_or(b.x1));
    match right {
        Some(edge) => (edge as f32, 1.0, edge - l - 6),
        None if clear => {
            // no button to centre on: stay where it was, short of whatever comes next along
            let next = (b.x1..img.w).find(|&x| (b.y0..b.y1).any(|y| differs(img.px[y * img.w + x], part.bg))).map_or(img.w - 2, |x| x - 8);
            (b.x0 as f32, 0.0, next.max(b.x1) - b.x0)
        }
        None if alone => ((l + r + 1) as f32 / 2.0, 0.5, r - l - 10),
        None => ((b.x0 + b.x1) as f32 / 2.0, 0.5, b.x1 - b.x0 + 10), // something may sit right next to it
    }
}

// ---------- the steps ----------

/// A blank English is left out, so the next run asks for it again. An unreadable file is an error:
/// read as empty, the user's corrections in it would be overwritten.
fn load_labels(path: &Path) -> R<IndexMap<String, String>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e} (save it as UTF-8)", path.display()))?;
    let lines = text.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once(" = "));
    Ok(lines.map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).filter(|(_, v)| !v.is_empty()).collect())
}

fn save_labels(path: &Path, labels: &IndexMap<String, String>) -> R<()> {
    write_labels(path, LABELS_HELP, labels)
}

fn write_labels(path: &Path, help: &str, labels: &IndexMap<String, String>) -> R<()> {
    let body: String = labels.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
    crate::write_whole(path, format!("{help}{body}").as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// A label that is a character's name, or names together, in the story's English spelling.
fn spelled_names(reading: &str, names: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let each: Option<Vec<String>> = reading.split(['＆', '&', '・']).map(|n| names(n).filter(|e| !e.is_empty() && !JA.is_match(e))).collect();
    Some(each?.join(" & "))
}

/// What of a model's reply may be painted: its first line, if it has English. A blank would erase
/// the button's text and draw nothing, and the Latin font would draw Japanese as boxes.
fn paintable(reply: &str, reading: &str) -> Option<String> {
    // " / " is the model's second guess
    let first = reply.split("\\n").next()?.split(" / ").next()?.trim();
    // "Yes: Confirm" is the label and then what it does
    let first = if reading.contains([':', '：']) { first } else { first.split(": ").next()?.trim() };
    // brackets the label doesn't have are the model talking about it, unless short and plain, as
    // in "Auto Release (Right Click)"
    let talk = |inside: &str| {
        let hedges = ["likely", "probably", "possibly", "maybe", "unclear", "?", "\"", "literally", "e.g", "i.e", " or "];
        inside.split_whitespace().count() > 3 || hedges.iter().any(|h| inside.to_lowercase().contains(h))
    };
    let has = |b: [char; 2]| match first.find(b) {
        Some(at) if !reading.contains(b) => !talk(&first[at..]),
        _ => true,
    };
    // and one only repeating words before it, as "Load Complete (Load)", is said twice
    let first = match first.find(['(', '（']).filter(|_| !reading.contains(['(', '（'])) {
        Some(at) => {
            let outside = first[..at].to_lowercase();
            let again = first[at..].trim_matches(['(', ')', '（', '）', ' ']).split_whitespace().all(|w| outside.split_whitespace().any(|o| o.starts_with(&w.to_lowercase())));
            if again { first[..at].trim() } else { first }
        }
        None => first,
    };
    // a label with no accented letters has none in its English either, as a model writes "Harém"
    let accented = |c: char| ('À'..='ÿ').contains(&c) && c != '×' && c != '÷';
    let plain = |c: char| match c {
        'à'..='å' => 'a', 'è'..='ë' => 'e', 'ì'..='ï' => 'i', 'ò'..='ö' => 'o', 'ù'..='ü' => 'u', 'ç' => 'c', 'ñ' => 'n', 'ý' | 'ÿ' => 'y',
        'À'..='Å' => 'A', 'È'..='Ë' => 'E', 'Ì'..='Ï' => 'I', 'Ò'..='Ö' => 'O', 'Ù'..='Ü' => 'U', 'Ç' => 'C', 'Ñ' => 'N', 'Ý' => 'Y',
        c => c,
    };
    let first: String = if reading.chars().any(accented) { first.to_string() } else { first.chars().map(plain).collect() };
    // nor a full stop the label lacks, as a sentence ends: "Move painting left."
    let first = if !first.ends_with("..") && !reading.ends_with(['。', '.']) { first.trim_end_matches('.').to_string() } else { first };
    (!first.is_empty() && !JA.is_match(&first) && has(['(', '（']) && has(['[', '［'])).then_some(first)
}

/// A reading with its letters' voicing marks taken off, as ハッピーデイ to ハッピーテイ.
fn unvoiced(reading: &str) -> String {
    const VOICED: &str = "がぎぐげござじずぜぞだぢづでどばびぶべぼぱぴぷぺぽガギグゲゴザジズゼゾダヂヅデドバビブベボパピプペポヴ";
    const PLAIN: &str = "かきくけこさしすせそたちつてとはひふへほはひふへほカキクケコサシスセソタチツテトハヒフヘホハヒフヘホウ";
    reading.chars().map(|c| VOICED.chars().position(|v| v == c).and_then(|i| PLAIN.chars().nth(i)).unwrap_or(c)).collect()
}

/// Readings a letter away by a pair an image model mixes up in menu lettering: the round mark and the
/// two strokes, as パ and バ, and ノ for a slash after Japanese.
fn look_alikes(reading: &str) -> Vec<String> {
    const PAIRS: [(char, char); 10] = [('バ', 'パ'), ('ビ', 'ピ'), ('ブ', 'プ'), ('ベ', 'ペ'), ('ボ', 'ポ'), ('ば', 'ぱ'), ('び', 'ぴ'), ('ぶ', 'ぷ'), ('べ', 'ぺ'), ('ぼ', 'ぽ')];
    let chars: Vec<char> = reading.chars().collect();
    (0..chars.len()).filter_map(|i| {
        let after_japanese = i > 0 && JA.is_match(&chars[i - 1].to_string());
        let other = match chars[i] {
            '/' if after_japanese => 'ノ',
            c => PAIRS.iter().find_map(|&(a, b)| if c == a { Some(b) } else if c == b { Some(a) } else { None })?,
        };
        let mut v = chars.clone();
        v[i] = other;
        Some(v.into_iter().collect())
    }).collect()
}

/// Whether a reading could be drawn in its block: in one taller than wide, letters down a column at
/// least 0.6 of its width tall. A shape read as a word, as ♪ read システム, isn't. A wide block can
/// hold a line with a smaller one under it, so its height says little of its letters.
fn fits(b: &Block, reading: &str) -> bool {
    let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
    w >= h || reading.chars().count() as f32 * w as f32 * 0.6 <= h as f32
}

/// Labels in batches for the model, by the picture each is on: whole pictures together, up to 20
/// labels. A label's English then hangs on its own picture's labels, not on whichever others shared
/// its batch, and since a batch ends after a picture by its name, a change on one picture moves no
/// other's batch.
fn batches(pictures: &[&str]) -> Vec<std::ops::Range<usize>> {
    let ends = |name: &str| name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3)) % 4 == 0;
    let (mut out, mut start) = (Vec::new(), 0);
    for i in 0..pictures.len() {
        let last = i + 1 == pictures.len();
        let picture_done = last || pictures[i + 1] != pictures[i];
        // the next picture's labels, which may not fit in this batch
        let next = if picture_done && !last { pictures[i + 1..].iter().take_while(|p| **p == pictures[i + 1]).count() } else { 0 };
        let size = i + 1 - start;
        if last || size == 20 || (picture_done && (size + next > 20 || ends(pictures[i]))) {
            out.push(start..i + 1);
            start = i + 1;
        }
    }
    out
}

/// Translate some of a screen's labels together: each gives the others context, and the readings
/// of one label correct each other. `note` says more about them. None if the reply can't be read.
fn translate_screen(vision: &dyn Chat, keys: &[(String, usize, String)], note: &str) -> R<Option<Vec<String>>> {
    let list: Vec<String> = keys.iter().enumerate().map(|(i, (k, n, hint))| format!("{}: {k} ({hint}at most {n} characters)", i + 1)).collect();
    let prompt = format!(
        "These labels come from one screen of a Japanese game's interface, read by OCR, so they may have mistakes. Where a label has \
         several readings they are separated by ' / ' and are the same label: work out the real Japanese word from them and from the \
         other labels. They are the titles, buttons and settings of a game's menus, not its story. Translate each one into the \
         concise English a game menu would show for it, within its character limit: only the label's own words, nothing added to \
         explain it. Translate all of a label's words: a long one, as a line of credits, stays whole. \
         A reading that is no real word is a word misread by a letter or two: translate that word. \
         A name stays that name, in English letters. A letter and a dot before a word shorten another word, as Q. for Quick and A. for Auto. \
         {note}Reply with exactly {n} lines formatted '<number>: <English>' and nothing else.\n\n{}",
        list.join("\n"),
        n = keys.len()
    );
    Ok(parse_numbered(&vision.chat("", &prompt)?, keys.len()))
}

/// Whether `judge` takes `new` over `old` as the English of the label `jp`, asked both ways round:
/// a model leans to the first it's shown.
fn better(judge: &dyn Chat, jp: &str, old: &str, new: &str) -> R<bool> {
    let pick = |first: &str, second: &str| -> R<Option<bool>> {
        // asked as a menu shows it: on the button that quits the game, "Game Over" doesn't say what it does
        let prompt = format!(
            "A Japanese game's menu shows this label or button: {jp}\nTwo English versions of it:\n1: {first}\n2: {second}\n\
             Which is right on that menu: says what the label means, or what the button does, and all of it? Reply with only its number."
        );
        Ok(match judge.chat("", &prompt)?.trim_start().chars().next() {
            Some('1') => Some(first == new),
            Some('2') => Some(second == new),
            _ => None,
        })
    };
    Ok(pick(old, new)? == Some(true) && pick(new, old)? == Some(true))
}

/// How the story's model reads a label: a VNTL model in its own prompt, any other asked plainly.
fn story_english(story: &dyn Chat, jp: &str) -> R<Option<String>> {
    if crate::llm::is_vntl(story.model()) {
        // labels hold no markup to keep
        static NO_MARKUP: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"[^\s\S]").unwrap());
        return crate::llm::translate_vntl(story, &NO_MARKUP, &[], &[], None, jp);
    }
    Ok(Some(story.chat("", &format!("Translate this label from a Japanese game into English. Reply with the English only.\n\n{jp}"))?.trim().to_string()))
}

fn bold_font() -> R<Font> {
    let bytes = FONTS.iter().find_map(|p| std::fs::read(p).ok()).ok_or("no bold font found in C:/Windows/Fonts")?;
    Font::from_bytes(bytes, FontSettings::default()).map_err(|e| e.to_string())
}

/// The font closest to some lettering, or the plain bold if nothing can be compared.
fn matched_font(game: &Path, kind: &str, samples: &[Sample]) -> R<Font> {
    if cfg!(test) {
        return bold_font(); // tests draw their own fixtures, whatever fonts this PC has
    }
    match fonts::matched(game, kind, samples) {
        Some(font) => Ok(font),
        None => bold_font(),
    }
}

fn sample(b: &Block, text: &str) -> Sample {
    Sample { w: b.x1 - b.x0, h: b.y1 - b.y0, ink: b.mask.clone(), text: text.to_string() }
}

/// A plate's lettering as one block: letters on a plate stand far apart, so `scan` sees several.
fn plate_part(img: Image) -> Option<Part> {
    let mut part = scan(0, None, (0, 0), String::new(), img)?;
    let (x0, x1) = (part.blocks.iter().map(|b| b.x0).min()?, part.blocks.iter().map(|b| b.x1).max()?);
    let (y0, y1) = (part.blocks.iter().map(|b| b.y0).min()?, part.blocks.iter().map(|b| b.y1).max()?);
    let mask = (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).map(|(x, y)| differs(part.img.px[y * part.img.w + x], part.bg)).collect();
    part.blocks = vec![Block { x0, y0, x1, y1, mask, group: 0 }];
    Some(part)
}

/// A name plate with `name` in place of the Japanese `jp`, in the lettering's own colour. The font
/// has no 【】, so the plate's own brackets stay: the right one moves to the end of the name.
fn paint_plate(part: &Part, jp: &str, name: &str, font: &Font) -> R<Image> {
    let (b, w) = (&part.blocks[0], part.img.w);
    let inked = |x: usize| (b.y0..b.y1).any(|y| differs(part.img.px[y * w + x], part.bg));
    let framed = jp.starts_with('【') && jp.ends_with('】');
    let left = if framed { (b.x0..b.x1).find(|&x| !inked(x)).unwrap_or(b.x0) } else { b.x0 };
    let right = if framed { (b.x0..b.x1).rev().find(|&x| !inked(x)).map_or(b.x1, |x| x + 1) } else { b.x1 };
    let mut colours: HashMap<u32, usize> = HashMap::new();
    (left..right).flat_map(|x| (b.y0..b.y1).map(move |y| y * w + x)).map(|i| part.img.px[i]).filter(|p| p >> 24 == 0).for_each(|p| *colours.entry(p).or_default() += 1);
    let fill = colours.into_iter().max_by_key(|(_, n)| *n).map_or([255; 3], |(p, _)| rgb(p));

    let mut out = part.img.clone();
    (0..out.h).flat_map(|y| (left..w).map(move |x| y * w + x)).for_each(|i| out.px[i] = part.bg);
    let at = left + if framed { 5 } else { 0 };
    let room = w.saturating_sub(at + 4 + b.x1 - right) as f32;
    let mut size = ((b.y1 - b.y0) as f32 * 1.1).clamp(11.0, 26.0);
    while size > 9.0 && text_width(font, name, size) > room {
        size -= 1.0;
    }
    draw(&mut out, font, name, size, at as f32, 0.0, (b.y0 + b.y1) as f32 / 2.0, fill, None)?;
    let end = (at + text_width(font, name, size).ceil() as usize + 4).min(w.saturating_sub(b.x1 - right));
    for (i, x) in (right..b.x1).enumerate() {
        (0..out.h).for_each(|y| out.px[y * w + end + i] = part.img.px[y * w + x]);
    }
    Ok(out)
}

/// Draw Liar-soft's name plates (`gf###.wcg` in grps.xfl) in English, in the font closest to their
/// lettering: `names` is {speaker code: (what the plate says, English)}. The name in the message box
/// is one of these pictures, so the scripts can't change it.
/// Each starts from the game's own plate, and everything else in the archive stays as it is now,
/// repainted menus included. Returns how many were drawn.
pub fn paint_plates(game: &Path, names: &HashMap<String, (String, String)>) -> R<usize> {
    let path = game.join("grps.xfl");
    if names.is_empty() || !path.is_file() {
        return Ok(0);
    }
    let shipped = liar::read_xfl(&original(&path)?)?;
    let mut now = liar::read_xfl(&std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?)?;
    let mut plates = Vec::new();
    for e in &shipped {
        let code = e.name.strip_prefix("gf").and_then(|n| n.strip_suffix(".wcg"));
        if let Some((jp, name)) = code.and_then(|c| names.get(c)) {
            plates.extend(plate_part(liar::read_wcg(&e.data)?).map(|part| (e, part, jp, name)));
        }
    }
    let samples: Vec<Sample> = plates.iter().take(SAMPLES).map(|(_, part, jp, _)| sample(&part.blocks[0], jp)).collect();
    let font = matched_font(game, "plates", &samples)?;
    let mut painted = 0;
    for (e, part, jp, name) in &plates {
        let Some(slot) = now.iter_mut().find(|n| n.name == e.name) else { continue };
        let img = paint_plate(part, jp, name, &font)?;
        let data = liar::write_wcg(&e.data, &img)?;
        if liar::read_wcg(&data).as_ref() != Ok(&img) {
            return Err(format!("{}: the repainted plate did not read back correctly; nothing was written", e.name));
        }
        slot.data = data;
        painted += 1;
    }
    let bytes = liar::write_xfl(&now);
    if liar::read_xfl(&bytes)?.len() != now.len() {
        return Err("the rebuilt grps.xfl did not read back correctly; nothing was written".into());
    }
    replace_file(&path, &bytes)?;
    Ok(painted)
}

/// What every engine shares: reading, translating and repainting, one screen at a time.
struct Job<'a> {
    game: PathBuf,
    /// `font` has been matched to this game's lettering, which needs a reading to compare.
    matched: bool,
    vision: &'a dyn Chat,
    reader: Option<Reader>,
    /// A picture's small shapes are rarely text, and the image model takes seconds to say so, so with
    /// this set a block the reader can't read is left alone.
    reader_only: bool,
    /// Lettering the text reader misreads, as round, outlined menu fonts are, goes to the image model.
    vision_first: bool,
    report: &'a dyn Fn(Event),
    cancel: &'a AtomicBool,
    reads_path: PathBuf,
    labels_path: PathBuf,
    cached: IndexMap<String, Vec<String>>,
    labels: IndexMap<String, String>,
    font: Font,
    /// What the image model made of each picture of a label this run: many are drawn the same twice
    read_before: HashMap<Vec<u8>, String>,
    /// Characters' names as the story spells them in English
    names: &'a dyn Fn(&str) -> Option<String>,
    /// Text the game writes elsewhere, which a label's reading may be a letter off
    words: Vec<Vec<char>>,
    /// Who speaks in the story, as the game writes them
    people: Vec<Vec<char>>,
    /// What the labels already read on the picture being read say
    nearby: Vec<String>,
    /// English the story's model corrected, in any game: used again, and shown as examples
    memory: IndexMap<String, String>,
    memory_path: PathBuf,
}

impl<'a> Job<'a> {
    fn new(game: &Path, vision: &'a dyn Chat, report: &'a dyn Fn(Event), cancel: &'a AtomicBool) -> R<Self> {
        let work = game.join("vntl_work");
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let (reads_path, labels_path) = (work.join("image_reads.json"), work.join("image_labels.txt"));
        let cached = if reads_path.exists() { load_json(&reads_path)? } else { IndexMap::new() };
        let labels = if labels_path.exists() { load_labels(&labels_path)? } else { IndexMap::new() };
        let font = bold_font()?;
        // tests draw their own fixtures and answer for the model, whatever this PC has installed
        let reader = if cfg!(test) { None } else { Reader::open().ok() };
        let memory_path = if cfg!(test) { work.join("label_memory.txt") } else { crate::app_folder("label_memory.txt") };
        let memory = if memory_path.exists() { load_labels(&memory_path)? } else { IndexMap::new() };
        Ok(Job { game: game.to_path_buf(), matched: false, vision, reader, reader_only: false, vision_first: false, report, cancel, reads_path, labels_path, cached, labels, font, read_before: HashMap::new(), names: &|_| None, words: Vec::new(), people: Vec::new(), nearby: Vec::new(), memory, memory_path })
    }

    /// The text reader's reading, or the image model's if it saw nothing, the label runs down, or
    /// `vision_first` is set. English it can read is an answer too: most blocks are that, and the
    /// model takes seconds each.
    fn read(&mut self, part: &Part, b: &Block, scale: usize) -> R<String> {
        if self.vision_first {
            // tiny lettering, as an 8 px label on a colour picker, is read as made-up words unless shown larger
            let img = block_image(part, b, scale.max(72 / (b.y1 - b.y0).max(1)));
            let png = write_png(&img)?;
            if let Some(text) = self.read_before.get(&png) {
                return Ok(text.clone());
            }
            let text = self.read_fresh(&img, &png)?;
            self.read_before.insert(png, text.clone());
            return Ok(text);
        }
        if let Some(reader) = self.reader.as_ref().filter(|_| b.x1 - b.x0 >= b.y1 - b.y0) {
            let text = reader.line(&block_image(part, b, scale))?;
            if !text.is_empty() {
                return Ok(text);
            }
            if self.reader_only {
                return Ok(text);
            }
        }
        read_block(self.vision, &write_png(&block_image(part, b, scale))?)
    }

    /// Whether a label's last line is English in smaller letters than its first, as "play" under 再生.
    fn english_under(&self, part: &Part, b: &Block) -> R<bool> {
        let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
        // any ink shown counts, as a greyed-out state's faint letters; a row with a pixel or two is
        // a gap, as an "l" can reach up to touch the line above
        let shown = |x: usize, y: usize| {
            let p = part.img.px[(b.y0 + y) * part.img.w + b.x0 + x];
            if part.bg == CLEAR { p >> 24 < 250 } else { differs(p, part.bg) }
        };
        let inked = |y: usize| (0..w).filter(|&x| shown(x, y)).count() >= 3;
        let mut lines: Vec<(usize, usize)> = Vec::new();
        for y in (0..h).filter(|&y| inked(y)) {
            match lines.last_mut() {
                Some(l) if l.1 == y => l.1 = y + 1,
                _ => lines.push((y, y + 1)),
            }
        }
        if lines.len() == 1 {
            // an icon beside both lines joins them, as ▶ beside 再生 over "play": the label is past it
            let tall = lines[0].1 - lines[0].0;
            return match first_mark(b).filter(|m| m.3 * 4 >= tall * 3) {
                Some((next, ..)) => self.english_under(part, &columns_of(b, next, w)),
                None => Ok(false),
            };
        }
        if lines.is_empty() {
            return Ok(false);
        }
        let ((t0, t1), (u0, u1)) = (lines[0], lines[lines.len() - 1]);
        if (u1 - u0) * 4 > (t1 - t0) * 3 {
            return Ok(false);
        }
        // the line with the tops and tails of its letters, from the line above it to the bottom
        let (u0, u1) = (lines[lines.len() - 2].1, h);
        // and alone: what's above it, faint as a greyed-out state's letters are, isn't shown
        let (mut img, iw) = (part.img.clone(), part.img.w);
        (b.y0..b.y0 + u0).flat_map(|y| (b.x0..b.x1).map(move |x| y * iw + x)).for_each(|i| img.px[i] = part.bg);
        let alone = Part { entry: part.entry, sprite: part.sprite, at: part.at, name: String::new(), img, bg: part.bg, blocks: Vec::new() };
        let line = block_image(&alone, &Block { y0: b.y0 + u0, y1: b.y0 + u1, mask: b.mask[u0 * w..u1 * w].to_vec(), ..b.clone() }, 4);
        // either reader seeing Latin letters will do: the model can call a clear "PAGE 1" no text
        let read = match &self.reader {
            Some(reader) => reader.line(&line)?,
            None => String::new(),
        };
        if read.chars().filter(char::is_ascii_alphabetic).count() >= 2 && !JA.is_match(&read) {
            return Ok(true);
        }
        Ok(text_kind(self.vision, &write_png(&line)?)?.starts_with("ENGLISH"))
    }

    /// The image model's reading, checked against the text reader's.
    fn read_fresh(&self, img: &Image, png: &[u8]) -> R<String> {
        let text = clean(read_block(self.vision, png)?);
        if !JA.is_match(&text) {
            return Ok(text);
        }
        // without the text reader, the checks below still stand on the model's own answers
        let seen = match &self.reader {
            Some(reader) => clean(reader.line(img)?.split_whitespace().collect()),
            None => String::new(),
        };
        // the model writes Japanese for what has none, as ボイス for Voice or a word for an arrow:
        // unless the reader saw that Japanese too, it's asked plainly which the picture has
        let shared = text.chars().filter(|c| seen.contains(*c)).count() * 2 >= text.chars().count();
        if !(JA.is_match(&seen) && shared) && !shows_japanese(self.vision, png)? {
            return Ok(String::new());
        }
        // a reading that is just the name of someone in the story is the game's own word, as ハナ that
        // the model takes for 八ナ: letters that alike, the picture can't settle
        let named = |t: &str| self.people.iter().any(|p| p.iter().copied().eq(t.chars()));
        if let Some(name) = [&text, &seen].into_iter().find(|t| named(t)) {
            return Ok(name.clone());
        }
        // past the check above, a "no Japanese here" from the comparison is a picture too small or
        // faint to compare letters on, as an 8 px label, not a reason to drop the reading
        let compare = |a: &str, b: &str| Ok::<_, String>(choose(self.vision, png, a, b)?.filter(|p| !p.is_empty()));
        // the model misreads a letter, as シャ for ジャ, which the game's own text or the reader may
        // have right: shown both, it finds which the picture says. Where it can't tell, the game's
        // own word stands, but not the reader's, which swaps letters as often as the model
        if let Some(word) = nearest(&self.words, &text).or_else(|| nearest_name(&self.people, &text)) {
            return Ok(compare(&word, &text)?.unwrap_or(word));
        }
        if seen == text || !JA.is_match(&seen) {
            return Ok(text);
        }
        // where the picture doesn't settle it, the real word beside the others on its screen does,
        // as いいえ beside はい; failing that, both go to the translation as readings of one label,
        // the reader's first: what neither settles, the model had made up, as レスキュー for いいえ
        let picked = match compare(&text, &seen)? {
            Some(p) => Some(p),
            None => likelier(self.vision, &text, &seen, &self.nearby)?,
        };
        // though not a reading with stray marks in it, as 一: for a word: that's the reader's own junk
        let junk = seen.chars().any(|c| c.is_ascii_punctuation() || "※［］".contains(c));
        Ok(picked.unwrap_or(if junk { format!("{text}{BOTH}{seen}") } else { format!("{seen}{BOTH}{text}") }))
    }

    /// Get English for `todo`, each label on the picture named in `pictures`, a few pictures at a time:
    /// one bad reply can't sink a whole folder of images. A label left without any keeps its Japanese,
    /// and the next run asks again.
    fn translate_labels(&mut self, screen: &str, todo: &[(String, usize, String)], pictures: &[&str]) -> R<()> {
        let stopped = || self.cancel.load(Ordering::Relaxed);
        let mut missed = 0;
        for chunk in batches(pictures).into_iter().map(|r| &todo[r]) {
            if stopped() {
                return Err("Stopped".into());
            }
            let note = self.remembered(chunk);
            let mut english = translate_screen(self.vision, chunk, &note)?.unwrap_or_default();
            if english.is_empty() {
                // one label at a time, as translate_batch retries a line
                for label in chunk {
                    if stopped() {
                        return Err("Stopped".into());
                    }
                    english.push(translate_screen(self.vision, std::slice::from_ref(label), &note)?.unwrap_or_default().concat());
                }
            }
            let good: Vec<(String, String)> = chunk.iter().zip(&english).filter_map(|((k, _, _), e)| Some((k.clone(), paintable(e, k)?))).collect();
            missed += chunk.len() - good.len();
            self.labels.extend(good);
            save_labels(&self.labels_path, &self.labels)?;
        }
        if missed > 0 {
            (self.report)(Event::Log(format!("{missed} labels in {screen} stay in Japanese; run again to retry them.")));
        }
        Ok(())
    }

    /// Corrections made before, in this game or another, sharing letters with `labels`: the model is
    /// shown them, to go by.
    fn remembered(&self, labels: &[(String, usize, String)]) -> String {
        let shares = |a: &str, b: &str| a.chars().collect::<Vec<_>>().windows(2).any(|w| b.contains(&w.iter().collect::<String>()));
        let found: Vec<String> = self.memory.iter().filter(|(k, _)| labels.iter().any(|(l, ..)| shares(k, l))).take(8).map(|(k, e)| format!("{k} = {e}")).collect();
        if found.is_empty() { String::new() } else { format!("Corrections made to earlier labels, to go by: {}. ", found.join("; ")) }
    }

    /// This run's English checked against the story's own model, trained on visual novels, which
    /// knows phrases an image model can get wrong, as ご機嫌な, cheerful, read as grumpy. Where the
    /// menu reading model, a third, takes the story model's reading over the English, the label is
    /// translated again with that reading shown, and the new English kept if the third model takes
    /// it too: in the labels, and in the memory, for every game. The story model slips as well, as
    /// "Game Over" for the button that quits the game, so without a third model to judge, nothing changes.
    fn check_with_story(&mut self, todo: &[(String, usize, String)]) -> R<()> {
        let (Some(story), Some(judge)) = (self.vision.story(), self.vision.reader()) else { return Ok(()) };
        let stopped = || if self.cancel.load(Ordering::Relaxed) { Err("Stopped".to_string()) } else { Ok(()) };
        // one model at a time, in turns, as two may not fit in the graphics memory together. Each
        // label once, and not one read several ways, whose first reading may be the wrong one
        let mut once = BTreeSet::new();
        let mut read = Vec::new();
        for t in todo.iter().filter(|t| !t.0.contains(BOTH) && once.insert(&t.0)) {
            stopped()?;
            let Some(en) = self.labels.get(&t.0).cloned() else { continue };
            if let Some(s) = story_english(story, &t.0)?.filter(|s| !s.is_empty() && *s != en) {
                read.push((t, en, s));
            }
        }
        // the menu reading model judges, each label on its own: the words model saw no contradiction
        // between "grumpy" and "cheerful" asked of the pair alone, and changed its verdicts with the
        // labels listed beside them
        let mut flagged = Vec::new();
        for (t, en, s) in read {
            stopped()?;
            if better(judge, &t.0, &en, &s)? {
                flagged.push((t, en, s));
            }
        }
        // the story model's reading put as a menu has it, in English alone: shown the Japanese, the words
        // model went back to what it believes the phrase means. Each on its own, as few are
        let mut redone = Vec::new();
        for ((k, n, _), old, s) in &flagged {
            stopped()?;
            let prompt = format!(
                "This is how a translator read a label from a Japanese game's menus: \"{s}\"\nWrite it as the concise English a \
                 game menu would show, at most {n} characters, keeping all it says. Reply with the English only."
            );
            if let Some(new) = paintable(&self.vision.chat("", &prompt)?, k) {
                redone.push((k, old, new));
            }
        }
        let mut taken = 0;
        for (k, old, new) in redone {
            stopped()?;
            if new != *old && better(judge, k, old, &new)? {
                self.labels.insert(k.clone(), new.clone());
                self.memory.insert(k.clone(), new);
                taken += 1;
            }
        }
        if taken > 0 {
            (self.report)(Event::Log(format!("{taken} labels' English corrected against the story's model.")));
            if let Some(dir) = self.memory_path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            write_labels(&self.memory_path, MEMORY_HELP, &self.memory)?;
        }
        save_labels(&self.labels_path, &self.labels)
    }

    /// Labels on one picture that read differently but got the same English, as 停止 and 一時停止 both
    /// "Pause", go back to the model together, told they are different buttons and what the picture's
    /// other labels say. Only this run's (`todo`), and not a label's other states, which are drawn alike.
    fn tell_apart(&mut self, parts: &[Part], groups: &[Group], todo: &[(String, usize, String)]) -> R<()> {
        // the labels that clash, each set once: (their English, the other labels beside them)
        let mut sets: IndexMap<BTreeSet<String>, (String, BTreeSet<String>)> = IndexMap::new();
        for entry in parts.iter().map(|p| p.entry).collect::<BTreeSet<_>>() {
            let here: Vec<(&Block, String)> = parts.iter().filter(|p| p.entry == entry).flat_map(|p| &p.blocks)
                .map(|b| (b, groups[b.group].reads.join(BOTH))).filter(|(_, k)| self.labels.contains_key(k)).collect();
            let ours = |k: &String| todo.iter().any(|t| t.0 == *k);
            // labels read the same, one way at least, are one word drawn twice, as 彩度 small and large
            let shared = |ka: &str, kb: &str| ka.split(BOTH).any(|r| kb.split(BOTH).any(|s| s == r));
            let mut clash: IndexMap<String, BTreeSet<String>> = IndexMap::new();
            for (i, (a, ka)) in here.iter().enumerate() {
                for (b, kb) in &here[i + 1..] {
                    let en = &self.labels[ka];
                    if kb != ka && ours(ka) && ours(kb) && self.labels[kb].eq_ignore_ascii_case(en) && !same_text(a, b, 80) && !shared(ka, kb) {
                        clash.entry(en.clone()).or_default().extend([ka.clone(), kb.clone()]);
                    }
                }
            }
            for (en, keys) in clash {
                let others: Vec<String> = here.iter().map(|(_, k)| k).filter(|k| !keys.contains(*k)).cloned().collect();
                sets.entry(keys).or_insert((en, BTreeSet::new())).1.extend(others);
            }
        }
        for (keys, (en, others)) in sets {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Stopped".into());
            }
            let taken: BTreeSet<String> = others.iter().map(|k| self.labels[k].to_lowercase()).collect();
            let beside: Vec<String> = others.iter().map(|k| format!("{k} ({})", self.labels[k])).collect();
            let keys: Vec<String> = keys.into_iter().collect();
            let lines: Vec<String> = keys.iter().enumerate().map(|(i, k)| format!("{}: {k}", i + 1)).collect();
            let prompt = format!(
                "These labels sit on one screen of a game's menus. They are different buttons, yet each was given the same \
                 English, \"{en}\". Give each its own concise English, as a game menu would show it{}. \
                 Reply with exactly {} lines formatted '<number>: <English>' and nothing else.\n\n{}",
                if beside.is_empty() { String::new() } else { format!(", unlike the screen's other labels: {}", beside.join(", ")) },
                keys.len(),
                lines.join("\n")
            );
            let Some(reply) = parse_numbered(&self.vision.chat("", &prompt)?, keys.len()) else { continue };
            let new: Option<Vec<String>> = keys.iter().zip(&reply).map(|(k, e)| paintable(e, k)).collect();
            // each different, and none what another label beside them already says
            let lower: Option<BTreeSet<String>> = new.as_ref().map(|n| n.iter().map(|e| e.to_lowercase()).collect());
            let Some(new) = new.filter(|n| lower.is_some_and(|l| l.len() == n.len() && l.is_disjoint(&taken))) else { continue };
            self.labels.extend(keys.into_iter().zip(new));
        }
        save_labels(&self.labels_path, &self.labels)
    }

    /// Ask for shorter English for the labels in `long`, each to fit the characters given, kept beside
    /// the full English for the places it doesn't fit.
    fn shorten_labels(&mut self, long: &IndexMap<String, usize>) -> R<()> {
        let list: Vec<(&String, String, usize)> = long.iter().filter_map(|(k, n)| Some((k, self.labels.get(k)?.clone(), *n))).collect();
        for chunk in list.chunks(20) {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Stopped".into());
            }
            let lines: Vec<String> = chunk.iter().enumerate().map(|(i, (_, en, n))| format!("{}: {en} (at most {n} characters)", i + 1)).collect();
            let prompt = format!(
                "These English labels from a game's menus are too long for their buttons. Shorten each one to its number of \
                 characters, keeping what it means: leave out words a menu can do without, or use a common abbreviation. \
                 Reply with exactly {} lines formatted '<number>: <English>' and nothing else.\n\n{}",
                chunk.len(),
                lines.join("\n")
            );
            let Some(short) = parse_numbered(&self.vision.chat("", &prompt)?, chunk.len()) else { continue };
            for ((k, en, _), s) in chunk.iter().zip(short) {
                if let Some(s) = paintable(&s, k).filter(|s| s.chars().count() < en.chars().count()) {
                    self.labels.insert(format!("{k}{SHORT}"), s);
                }
            }
        }
        save_labels(&self.labels_path, &self.labels)
    }

    /// Repaint names drawn as pictures: the letters stand far apart, so each picture is read whole
    /// and spelled as `english` has that name. Adds them to `painted`, and returns how many.
    fn drawn_names(&mut self, files: Vec<(String, Vec<u8>)>, english: &dyn Fn(&str) -> Option<String>, painted: &mut Vec<(String, Vec<u8>)>) -> R<usize> {
        let mut drawn = 0;
        for (name, png) in files {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Stopped".into());
            }
            let img = read_png(&png)?;
            let key = format!("names|{name}");
            let read = match self.cached.get(&key) {
                Some(reads) => reads.first().cloned(),
                None => {
                    let read = read_plate(self.reader.as_ref(), Some(self.vision), &img)?;
                    self.cached.insert(key, read.iter().cloned().collect());
                    save_json(&self.reads_path, &self.cached)?;
                    read
                }
            };
            let (Some(jp), Some(part)) = (read, plate_part(img)) else { continue };
            let Some(en) = english(&jp) else { continue };
            painted.push((name, write_png(&paint_plate(&part, &jp, &en, &self.font)?)?));
            drawn += 1;
        }
        Ok(drawn)
    }

    /// The parts of one screen that got English: (index into `parts`, repainted image).
    fn screen(&mut self, screen: &str, parts: &mut [Part]) -> R<Vec<(usize, Image)>> {
        let mut groups: Vec<Group> = Vec::new();
        // a reading is kept by where its block sits in its picture, so when blocks are found
        // differently later, only those that moved are read again
        let mut taken: HashMap<String, usize> = HashMap::new();
        let keys: Vec<Vec<String>> = parts.iter().map(|p| p.blocks.iter().map(|b| {
            let at = format!("{screen}|{}@{},{},{}x{}", p.name, p.at.0 + b.x0 as i32, p.at.1 + b.y0 as i32, b.x1 - b.x0, b.y1 - b.y0);
            let n = taken.entry(at.clone()).or_default();
            *n += 1;
            if *n == 1 { at } else { format!("{at}#{n}") }
        }).collect()).collect();
        // what the image model reads, it reads well: only a near copy is taken as the same label
        let alike = if self.vision_first { 96 } else { 80 };
        for p in 0..parts.len() {
            for (b, key) in keys[p].iter().enumerate() {
                // one image never shows a label twice: its look-alikes are different words
                let found = groups.iter().position(|g| g.first.0 != p && same_text(&parts[g.first.0].blocks[g.first.1], &parts[p].blocks[b], alike));
                let id = match found {
                    Some(id) => {
                        groups[id].second.get_or_insert((p, b));
                        id
                    }
                    None => {
                        groups.push(Group { key: key.clone(), first: (p, b), second: None, reads: Vec::new() });
                        groups.len() - 1
                    }
                };
                parts[p].blocks[b].group = id;
            }
        }
        (self.report)(Event::Log(format!("Reading {} labels in {screen}", groups.len())));
        let mut split: Vec<(usize, usize, String)> = Vec::new();
        let mut read_on: HashMap<usize, Vec<String>> = HashMap::new();
        let mut fresh = Vec::new();
        for (i, g) in groups.iter_mut().enumerate() {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Stopped".into());
            }
            // what a label says is read on each picture it's drawn on, its look-alike's too
            self.nearby = read_on.get(&parts[g.first.0].entry).cloned().unwrap_or_default();
            let pictures: Vec<usize> = [Some(g.first), g.second].into_iter().flatten().map(|(p, _)| parts[p].entry).collect();
            let mut note = |reads: &[String]| pictures.iter().for_each(|e| read_on.entry(*e).or_default().extend(reads.iter().take(1).cloned()));
            // a look-alike the image model reads differently is another label, as 右 beside 左
            let other = g.second.filter(|_| self.vision_first).map(|(c, d)| (c, d, keys[c][d].clone()));
            if let Some(r) = self.cached.get(&g.key) {
                g.reads = r.clone();
                note(r);
                split.extend(other.filter(|o| self.cached.contains_key(&o.2)));
                continue;
            }
            let (a, b) = g.first;
            // a label with its own English printed under it, as 再生 over "play", needs no repaint
            if self.english_under(&parts[a], &parts[a].blocks[b])? {
                self.cached.insert(g.key.clone(), Vec::new());
                save_json(&self.reads_path, &self.cached)?;
                continue;
            }
            // a second look at the same text: the other state, or a different enlargement
            let first = self.read(&parts[a], &parts[a].blocks[b], 4)?;
            let mut second = match g.second {
                Some((c, d)) => self.read(&parts[c], &parts[c].blocks[d], 4)?,
                // the image model reads the same picture the same way twice
                None if self.vision_first => String::new(),
                None => self.read(&parts[a], &parts[a].blocks[b], 2)?,
            };
            if let Some((c, d, k)) = other.filter(|_| second != first && JA.is_match(&second)) {
                // only when the picture bears that out: else it's the same label misread
                let (own, theirs) = (second.split(BOTH).next().unwrap_or_default(), first.split(BOTH).next().unwrap_or_default());
                let png = write_png(&block_image(&parts[c], &parts[c].blocks[d], 4))?;
                if choose(self.vision, &png, own, theirs)?.as_deref() == Some(own) {
                    self.cached.insert(k.clone(), std::mem::take(&mut second).split(BOTH).map(|t| clean(t.to_string())).collect());
                    split.push((c, d, k));
                }
            }
            // on a picture a guess at a shape won't come out the same twice, as a label will
            let agreed = !self.reader_only || clean(first.clone()) == clean(second.clone());
            let block = &parts[a].blocks[b];
            g.reads = [first, second].iter().flat_map(|t| t.split(BOTH)).map(|t| clean(t.to_string())).filter(|t| agreed && JA.is_match(t) && fits(block, t)).collect();
            let mut once = BTreeSet::new();
            g.reads.retain(|r| once.insert(r.clone()));
            // a reading that is another plus extra characters is that reading with junk on the end
            let all = g.reads.clone();
            g.reads.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o.as_str())));
            note(&g.reads);
            self.cached.insert(g.key.clone(), g.reads.clone());
            save_json(&self.reads_path, &self.cached)?;
            fresh.push(i);
        }
        // a label left between two readings takes a word another label on its picture was read as, a
        // letter or so off, unless the picture says otherwise: a small 彩度 read 移園 / 彩思 beside a
        // larger 彩度. A long reading looks across the screen, and takes the other word where the picture,
        // or failing it the words model, says so: a pop-up's song title in decorative letters, read
        // 星降る夏に, beside a list of songs that has 星降る夜に plainly. Those go first:
        // a label read several ways, one of which another label was read as alone, is that word
        fresh.sort_by_key(|&i| groups[i].reads.len() != 1);
        for &i in &fresh {
            let entry = parts[groups[i].first.0].entry;
            // what the others were read as, less this label's own readings, which tell nothing
            let others = |here: bool| -> Vec<Vec<char>> {
                groups.iter().enumerate().filter(|(j, o)| *j != i && (!here || parts[o.first.0].entry == entry))
                    .flat_map(|(_, o)| o.reads.iter().filter(|r| !groups[i].reads.contains(r)).map(|r| r.chars().collect())).collect()
            };
            let reads = &groups[i].reads;
            if reads.len() > 1 {
                let alone: BTreeSet<&String> = groups.iter().enumerate().filter(|(j, o)| *j != i && o.reads.len() == 1).map(|(_, o)| &o.reads[0]).collect();
                if let Some(word) = reads.iter().find(|r| alone.contains(r)).cloned() {
                    groups[i].reads = vec![word];
                    self.cached.insert(groups[i].key.clone(), groups[i].reads.clone());
                    save_json(&self.reads_path, &self.cached)?;
                    continue;
                }
            }
            let found = match &reads[..] {
                [] => None,
                [one] => nearest(&others(false), one).map(|w| (w, one.clone())),
                reads => reads.iter().find_map(|r| Some((nearest_name(&others(true), r)?, r.clone()))),
            };
            let Some((word, reading)) = found else { continue };
            let (a, b) = groups[i].first;
            let block = &parts[a].blocks[b];
            let png = write_png(&block_image(&parts[a], block, 4.max(72 / (block.y1 - block.y0).max(1))))?;
            // the marks that voice a letter, as テ and デ, are too small for the picture to settle in
            // decorative lettering: there the words do
            let marks_only = unvoiced(&word) == unvoiced(&reading);
            let picked = if marks_only { None } else { choose(self.vision, &png, &word, &reading)? };
            let take = match (groups[i].reads.len(), picked) {
                (1, Some(p)) => p == word,
                (1, None) => {
                    let pick = match likelier(self.vision, &word, &reading, &[])? {
                        None if marks_only => real_of_two(self.vision, &word, &reading)?,
                        pick => pick,
                    };
                    pick.as_deref() == Some(word.as_str())
                }
                (_, picked) => picked.as_deref() != Some(reading.as_str()),
            };
            if take {
                groups[i].reads = vec![word];
                self.cached.insert(groups[i].key.clone(), groups[i].reads.clone());
                save_json(&self.reads_path, &self.cached)?;
            }
        }
        // letters the picture can't tell apart, as バ and パ or ノ and a slash, go by which makes real
        // Japanese beside the picture's other labels: a title read バイをやいた is パイをやいた
        for &i in &fresh {
            let [reading] = &groups[i].reads[..] else { continue };
            let reading = reading.clone();
            let entry = parts[groups[i].first.0].entry;
            let beside: Vec<String> = groups.iter().filter(|o| o.reads.len() == 1 && o.reads[0] != reading && parts[o.first.0].entry == entry).map(|o| o.reads[0].clone()).take(12).collect();
            for other in look_alikes(&reading) {
                if likelier(self.vision, &reading, &other, &beside)?.as_deref() == Some(other.as_str()) {
                    groups[i].reads = vec![other];
                    self.cached.insert(groups[i].key.clone(), groups[i].reads.clone());
                    save_json(&self.reads_path, &self.cached)?;
                    break;
                }
            }
        }
        for (c, d, key) in split {
            let reads = self.cached[&key].clone();
            parts[c].blocks[d].group = groups.len();
            groups.push(Group { key, first: (c, d), second: None, reads });
        }
        // and what was read where no block sits now goes
        let (used, prefix): (BTreeSet<&String>, String) = (keys.iter().flatten().collect(), format!("{screen}|"));
        let before = self.cached.len();
        self.cached.retain(|k, _| !k.starts_with(&prefix) || used.contains(k));
        if self.cached.len() != before {
            save_json(&self.reads_path, &self.cached)?;
        }

        let key = |g: &Group| g.reads.join(BOTH);
        // what the picture's width allows, or a long label's own, as a line of credits
        let room = |g: &Group| {
            let (p, b) = (&parts[g.first.0], &parts[g.first.0].blocks[g.first.1]);
            (p.img.w.min(320).max(b.x1 - b.x0) / 11).max(4)
        };
        // what a button is for: the leftmost label on its row is the setting it belongs to
        let hint = |g: &Group| -> String {
            let part = &parts[g.first.0];
            if part.sprite.is_none() {
                return String::new();
            }
            let centre = |p: &Part, b: &Block| p.at.1 + (b.y0 + b.y1) as i32 / 2;
            let block = &part.blocks[g.first.1];
            let (y, x) = (centre(part, block), part.at.0 + block.x0 as i32);
            let row = groups.iter().filter(|o| !o.reads.is_empty() && parts[o.first.0].entry == part.entry).filter_map(|o| {
                let (other, b) = (&parts[o.first.0], &parts[o.first.0].blocks[o.first.1]);
                let left = other.at.0 + b.x1 as i32;
                ((centre(other, b) - y).abs() <= 12 && left < x).then_some((left, &o.reads[0]))
            });
            row.min_by_key(|(left, _)| *left).map_or(String::new(), |(_, r)| format!("an option of the setting '{r}'; "))
        };
        // a character's name, or names together, is spelled as the story has it
        for g in groups.iter().filter(|g| g.reads.len() == 1) {
            if let Some(en) = spelled_names(&g.reads[0], self.names) {
                self.labels.entry(key(g)).or_insert(en);
            }
        }
        // and one corrected before, in this game or another, keeps its correction
        for g in groups.iter().filter(|g| !g.reads.is_empty()) {
            if let Some(en) = self.memory.get(&key(g)) {
                self.labels.entry(key(g)).or_insert(en.clone());
            }
        }
        let new: Vec<&Group> = groups.iter().filter(|g| !g.reads.is_empty() && !self.labels.contains_key(&key(g))).collect();
        let todo: Vec<(String, usize, String)> = new.iter().map(|g| (key(g), room(g), hint(g))).collect();
        let pictures: Vec<&str> = new.iter().map(|g| parts[g.first.0].name.as_str()).collect();
        self.translate_labels(screen, &todo, &pictures)?;
        self.check_with_story(&todo)?;
        self.tell_apart(parts, &groups, &todo)?;
        let samples: Vec<Sample> = groups.iter().filter(|g| !g.reads.is_empty()).take(SAMPLES).map(|g| sample(&parts[g.first.0].blocks[g.first.1], &g.reads[0])).collect();
        if !self.matched && !samples.is_empty() {
            self.font = matched_font(&self.game, "menus", &samples)?;
            self.matched = true;
        }
        // English too wide for its place even drawn small runs off it, so it gets a short form there.
        // Only this run's: a label the user corrected stays as they wrote it
        let sps: Vec<_> = parts.iter().map(|p| spans(&p.img, p.bg)).collect();
        // how many letters fit, if the English doesn't
        let too_wide = |font: &Font, p: &Part, text: &[Option<&String>], sp: &[Option<(usize, usize)>], b: &Block, t: &str| {
            let (wide, room) = (text_width(font, t, SMALLEST) + 4.0, place(p, text, sp, b).2 as f32);
            (wide > room).then(|| ((t.chars().count() as f32 * room / wide) as usize).max(3))
        };
        let mut long: IndexMap<String, usize> = IndexMap::new();
        for (p, sp) in parts.iter().zip(&sps) {
            let text: Vec<Option<&String>> = p.blocks.iter().map(|b| self.labels.get(&key(&groups[b.group]))).collect();
            for (b, t) in p.blocks.iter().zip(&text) {
                let k = key(&groups[b.group]);
                let Some(t) = t.filter(|_| todo.iter().any(|(d, _, _)| *d == k)) else { continue };
                if let Some(most) = too_wide(&self.font, p, &text, sp, b, t) {
                    long.entry(k).and_modify(|n| *n = (*n).min(most)).or_insert(most);
                }
            }
        }
        self.shorten_labels(&long)?;
        let mut changed = Vec::new();
        for (i, (p, sp)) in parts.iter().zip(&sps).enumerate() {
            let full: Vec<Option<&String>> = p.blocks.iter().map(|b| self.labels.get(&key(&groups[b.group]))).collect();
            // the short form only where the full English doesn't fit: a word in a small place and a large one
            let text: Vec<Option<&String>> = p.blocks.iter().zip(&full).map(|(b, t)| {
                let short = self.labels.get(&format!("{}{SHORT}", key(&groups[b.group])));
                match (t, short) {
                    (Some(t), Some(s)) if too_wide(&self.font, p, &full, sp, b, t).is_some() => Some(s),
                    _ => *t,
                }
            }).collect();
            if text.iter().any(|t| t.is_some()) {
                changed.push((i, repaint(p, &text, &self.font)?));
            }
        }
        Ok(changed)
    }
}

/// Find, read, translate and repaint a Liar-soft game's menu images. Returns a summary line.
pub fn translate_menus(game: &Path, vision: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<String> {
    let mut job = Job::new(game, vision, report, cancel)?;
    let (mut painted, mut archives) = (0, 0);

    for arc in ARCHIVES.iter().filter(|a| game.join(a).is_file()) {
        let path = game.join(arc);
        let mut entries = liar::read_xfl(&original(&path)?)?;
        // a plain archive of images is one screen; each .lwg container is its own
        let mut screens: Vec<(String, Vec<Part>)> = Vec::new();
        let loose: Vec<Part> = entries.iter().enumerate().filter(|(_, e)| arc.starts_with("grpo") && e.name.ends_with(".wcg"))
            .filter_map(|(i, e)| scan(i, None, (0, 0), e.name.clone(), liar::read_wcg(&e.data).ok()?)).collect();
        if !loose.is_empty() {
            screens.push((arc.to_string(), loose));
        }
        for (i, e) in entries.iter().enumerate().filter(|(_, e)| e.name.ends_with(".lwg")) {
            let lwg = liar::read_lwg(&e.data).map_err(|err| format!("{}: {err}", e.name))?;
            let parts: Vec<Part> = lwg.sprites.iter().enumerate().filter_map(|(j, s)| scan(i, Some(j), (s.x, s.y), s.name(), liar::read_wcg(&s.data).ok()?)).collect();
            if !parts.is_empty() {
                screens.push((format!("{arc}/{}", e.name), parts));
            }
        }

        let mut changed: Vec<(usize, Option<usize>, Image)> = Vec::new();
        for (screen, parts) in &mut screens {
            for (i, img) in job.screen(screen, parts)? {
                changed.push((parts[i].entry, parts[i].sprite, img));
            }
        }

        // write everything back, the same way the images were found
        let mut failed = 0;
        let mut lwgs: IndexMap<usize, liar::Lwg> = IndexMap::new();
        let mut good = Vec::new();
        for (entry, sprite, img) in &changed {
            let head = match sprite {
                Some(s) => {
                    if !lwgs.contains_key(entry) {
                        lwgs.insert(*entry, liar::read_lwg(&entries[*entry].data)?);
                    }
                    lwgs[entry].sprites[*s].data.clone()
                }
                None => entries[*entry].data.clone(),
            };
            match liar::write_wcg(&head, img) {
                Ok(data) => {
                    match sprite {
                        Some(s) => lwgs[entry].sprites[*s].data = data,
                        None => entries[*entry].data = data,
                    }
                    good.push((*entry, *sprite, img));
                }
                Err(_) => failed += 1,
            }
        }
        for (entry, lwg) in &lwgs {
            entries[*entry].data = liar::write_lwg(lwg);
        }
        if failed > 0 {
            report(Event::Log(format!("{failed} images in {arc} have too many colours to save and stay as they were.")));
        }
        if good.is_empty() {
            continue;
        }
        let bytes = liar::write_xfl(&entries);
        let back = liar::read_xfl(&bytes)?;
        let intact = good.iter().all(|(entry, sprite, img)| {
            let data = match sprite {
                Some(s) => liar::read_lwg(&back[*entry].data).ok().map(|l| l.sprites[*s].data.clone()),
                None => Some(back[*entry].data.clone()),
            };
            data.and_then(|d| liar::read_wcg(&d).ok()).is_some_and(|i| i == **img)
        });
        if back.len() != entries.len() || !intact {
            return Err(format!("the rebuilt {arc} did not read back correctly; nothing was written"));
        }
        replace_file(&path, &bytes)?;
        painted += good.len();
        archives += 1;
    }
    Ok(format!(
        "Repainted {painted} menu images in {archives} archives (read back OK; originals kept as *.vntl-orig). \
         To correct a label, edit vntl_work/image_labels.txt and run again."
    ))
}

/// The same for a Ren'Py game. Its images are plain files and a translation can override them, so
/// nothing of the game's is changed: repainted copies go to game/tl/<lang>/. Images an official
/// translation already provides are left alone.
pub fn translate_renpy_menus(game: &Path, lang: &str, vision: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<String> {
    let mut job = Job::new(game, vision, report, cancel)?;
    let tl = tl_dir(game, lang);
    let ours_path = game.join("vntl_work").join("image_files.txt");
    let ours: BTreeSet<String> = std::fs::read_to_string(&ours_path).unwrap_or_default().lines().map(String::from).collect();
    let packed = archive_files(game)?;
    let mut paths: BTreeSet<String> = packed.keys().cloned().collect();
    let mut loose = Vec::new();
    let root = game.join("game");
    files_under(&root, &mut loose);
    paths.extend(loose.iter().filter_map(|p| Some(p.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/"))));
    let wanted = |p: &&String| {
        let low = p.to_lowercase();
        is_picture(&low) && !low.starts_with("tl/") && (ours.contains(*p) || !tl.join(p).exists())
    };
    let (gui, pictures): (Vec<&String>, Vec<&String>) = paths.iter().filter(wanted).partition(|p| p.to_lowercase().starts_with("gui/"));

    let mut parts = Vec::new();
    for path in gui {
        let Some(img) = read_game_file(game, &packed, path).and_then(|d| button_image(&d)) else { continue };
        parts.extend(scan(parts.len(), None, (0, 0), path.clone(), img));
    }
    let mut changed: Vec<(String, Image)> = job.screen("gui", &mut parts)?.into_iter().map(|(i, img)| (parts[i].name.clone(), img)).collect();

    report(Event::Log(format!("Looking through {} other images for labels", pictures.len())));
    let mut parts = Vec::new();
    for path in pictures {
        if cancel.load(Ordering::Relaxed) {
            return Err("Stopped".into());
        }
        let Some(img) = read_game_file(game, &packed, path).and_then(|d| read_picture(&d).ok()) else { continue };
        parts.extend(picture_part(parts.len(), path.clone(), img));
    }
    if !parts.is_empty() {
        job.reader_only = true;
        changed.extend(job.screen("pictures", &mut parts)?.into_iter().map(|(i, img)| (parts[i].name.clone(), img)));
    }
    write_overrides(&tl, &ours_path, ours, &changed)?;
    Ok(format!(
        "Repainted {} menu images into game/tl/{lang}/ (read back OK; the game's own files are unchanged). \
         To correct a label, edit vntl_work/image_labels.txt and run again.",
        changed.len()
    ))
}

/// Put repainted images where Ren'Py takes them over the game's own, and keep count of them. They're
/// always PNG, under the old name if that's a WebP, JPEG or AVIF: the engine goes by what's in a file.
fn write_overrides(tl: &Path, ours_path: &Path, mut ours: BTreeSet<String>, changed: &[(String, Image)]) -> R<()> {
    // listed before they are written: an unlisted file looks official, and Remove would never find it
    ours.extend(changed.iter().map(|(name, _)| name.clone()));
    let list: String = ours.iter().map(|p| format!("{p}\n")).collect();
    std::fs::write(ours_path, &list).map_err(|e| e.to_string())?;
    // lets "Remove translation" find them
    std::fs::create_dir_all(tl).map_err(|e| e.to_string())?;
    let manifest = tl.join("vntl_files.txt");
    let mut files: BTreeSet<String> = std::fs::read_to_string(&manifest).unwrap_or_default().lines().map(String::from).collect();
    files.extend(ours.iter().cloned());
    std::fs::write(&manifest, files.iter().map(|p| format!("{p}\n")).collect::<String>()).map_err(|e| e.to_string())?;
    for (name, img) in changed {
        let bytes = write_png(img)?;
        if read_png(&bytes).as_ref() != Ok(img) {
            return Err(format!("{name}: the repainted image did not read back correctly"));
        }
        let out = tl.join(name);
        std::fs::create_dir_all(out.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&out, bytes).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    Ok(())
}

// ---------- text over artwork ----------

/// Paint over `area` from the pixels around it, one ring at a time towards the middle. Nothing here
/// knows what the picture shows, so it suits skies and walls and leaves a smear over fine detail.
fn fill_in(img: &mut Image, (x0, y0, x1, y1): Area) {
    let w = img.w;
    let mut known = vec![true; img.px.len()];
    let mut todo: Vec<(usize, usize)> = (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).collect();
    todo.iter().for_each(|&(x, y)| known[y * w + x] = false);
    while !todo.is_empty() {
        let mut ring = Vec::new();
        todo.retain(|&(x, y)| {
            let around = (y.saturating_sub(1)..(y + 2).min(img.h)).flat_map(|j| (x.saturating_sub(1)..(x + 2).min(w)).map(move |i| j * w + i));
            let near: Vec<u32> = around.filter(|&i| known[i]).map(|i| img.px[i]).collect();
            if near.is_empty() {
                return true;
            }
            let mean = [24, 16, 8, 0].iter().fold(0, |p, s| p | ((near.iter().map(|n| n >> s & 255).sum::<u32>() / near.len() as u32) << s));
            ring.push((y * w + x, mean));
            false
        });
        if ring.is_empty() {
            return; // the area was the whole picture: nothing to go on
        }
        for (i, p) in ring {
            img.px[i] = p;
            known[i] = true;
        }
    }
}

/// English over the picture in place of `rows`: white with a dark edge, which reads on anything.
fn paint_over(img: &mut Image, rows: &[(Area, &String)], font: &Font, painter: Option<&Painter>) -> R<()> {
    let areas: Vec<Area> = rows.iter().map(|((x0, y0, x1, y1), _)| (x0.saturating_sub(3), y0.saturating_sub(3), (x1 + 3).min(img.w), (y1 + 3).min(img.h))).collect();
    match painter {
        Some(painter) => painter.fill(img, &areas)?,
        None => areas.iter().for_each(|&area| fill_in(img, area)),
    }
    for (&area, (_, text)) in areas.iter().zip(rows) {
        let room = (area.2 - area.0) as f32;
        let mut size = ((area.3 - area.1) as f32 * 0.8).clamp(11.0, 40.0);
        while size > 9.0 && text_width(font, text, size) > room {
            size -= 1.0;
        }
        draw(img, font, text, size, area.0 as f32, 0.0, (area.1 + area.3) as f32 / 2.0, [255; 3], Some([0; 3]))?;
    }
    Ok(())
}

/// Text painted over a Ren'Py game's artwork, for those who want it: the text reader looks through
/// every picture, which takes a while, and the English goes over a filled-in patch, which shows.
pub fn translate_art(game: &Path, lang: &str, vision: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<String> {
    let mut job = Job::new(game, vision, report, cancel)?;
    let tl = tl_dir(game, lang);
    let ours_path = game.join("vntl_work").join("image_files.txt");
    let ours: BTreeSet<String> = std::fs::read_to_string(&ours_path).unwrap_or_default().lines().map(String::from).collect();
    let packed = archive_files(game)?;
    let mut paths: BTreeSet<String> = packed.keys().cloned().collect();
    let (mut loose, root) = (Vec::new(), game.join("game"));
    files_under(&root, &mut loose);
    paths.extend(loose.iter().filter_map(|p| Some(p.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/"))));
    let wanted: Vec<String> = paths.into_iter().filter(|p| {
        let low = p.to_lowercase();
        is_picture(&low) && !low.starts_with("tl/") && !low.starts_with("gui/") && (ours.contains(p) || !tl.join(p).exists())
    }).collect();
    let read = |path: &str| read_game_file(game, &packed, path).and_then(|d| read_picture(&d).ok());
    // on top of what the menu step painted, if it painted this one
    let painted = |path: &str| {
        let own = if ours.contains(path) { std::fs::read(tl.join(path)).ok() } else { None };
        own.and_then(|d| read_picture(&d).ok()).or_else(|| read(path))
    };
    let changed = job.paint_art(&wanted, &read, &painted)?;
    write_overrides(&tl, &ours_path, ours, &changed)?;
    Ok(format!("Painted English over {} pictures into game/tl/{lang}/ (the game's own files are unchanged). To correct one, edit vntl_work/image_labels.txt and run again.", changed.len()))
}

/// The same for a Unity game with Yarn Spinner: what's repainted waits beside the translation, as the
/// next write puts it in the game's files.
pub fn translate_unity_art(game: &Path, vision: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<String> {
    let mut job = Job::new(game, vision, report, cancel)?;
    let g = crate::yarn::Game::open(game)?;
    let read = |name: &str| g.picture(name);
    let changed = job.paint_art(&g.pictures(), &read, &read)?;
    let painted = changed.iter().map(|(name, img)| Ok((name.clone(), write_png(img)?))).collect::<R<Vec<_>>>()?;
    artemis::keep_painted(game, &painted)?;
    Ok(format!("Painted English over {} pictures. To correct one, edit vntl_work/image_labels.txt and run again.", painted.len()))
}

impl Job<'_> {
    /// Looks through `wanted` for text, by what `read` gives, then paints English on what `base` gives.
    /// Rows found are kept, so a stopped run carries on. Characters on nothing are skipped, as they
    /// hold no text and their outlines read as letters.
    fn paint_art(&mut self, wanted: &[String], read: &dyn Fn(&str) -> Option<Image>, base: &dyn Fn(&str) -> Option<Image>) -> R<Vec<(String, Image)>> {
        let reader = Reader::open()?;
        // LaMa where it's installed, and the plain fill if it isn't or won't load
        let painter = (inpaint::installed() && !cfg!(test)).then(Painter::open).and_then(|p| p.inspect_err(|e| (self.report)(Event::Log(format!("{e}; using the plain fill")))).ok());
        let rows_path = self.game.join("vntl_work").join("art_rows.json");
        let mut seen: IndexMap<String, Vec<(Area, String)>> = if rows_path.exists() { load_json(&rows_path)? } else { IndexMap::new() };
        (self.report)(Event::Log(format!("Looking for text over artwork in {} pictures. This can take a while.", wanted.len())));
        for (n, path) in wanted.iter().enumerate() {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Stopped".into());
            }
            (self.report)(Event::Progress(n, wanted.len()));
            if seen.contains_key(path) {
                continue;
            }
            let Some(img) = read(path) else { continue };
            let rows = match flat_background(&img) {
                Some(CLEAR) => Vec::new(),
                _ => {
                    let mut kept = Vec::new();
                    for row in reader.rows(&img)?.into_iter().filter(|r| JA.is_match(&r.1)) {
                        if reader.agrees(&img, row.0, &row.1)? {
                            kept.push(row);
                        }
                    }
                    kept
                }
            };
            seen.insert(path.clone(), rows);
            save_json(&rows_path, &seen)?;
        }

        let shown = |t: &String| t.chars().count() >= 2;
        // each text once, with the first picture it's on
        let mut asked = BTreeSet::new();
        let (todo, pictures): (Vec<(String, usize, String)>, Vec<&str>) = wanted.iter()
            .flat_map(|p| seen.get(p).into_iter().flatten().map(move |r| (&r.1, p.as_str())))
            .filter(|(t, _)| shown(t) && !self.labels.contains_key(*t) && asked.insert(*t))
            .map(|(t, p)| ((t.clone(), t.chars().count() * 3, "text inside a picture of the story, such as a sign or a note; ".to_string()), p))
            .unzip();
        self.translate_labels("artwork", &todo, &pictures)?;
        let mut changed = Vec::new();
        for path in wanted {
            let rows: Vec<(Area, &String)> = seen.get(path).into_iter().flatten().filter_map(|(a, t)| Some((*a, self.labels.get(t)?))).collect();
            if rows.is_empty() {
                continue;
            }
            let Some(mut img) = base(path) else { continue };
            paint_over(&mut img, &rows, &self.font, painter.as_ref())?;
            changed.push((path.clone(), img));
        }
        Ok(changed)
    }
}

/// Images an official translation has just copied over ours stop being ours, so
/// translate_renpy_menus leaves them alone.
pub fn disown_images(work: &Path, files: &[String]) -> R<()> {
    let path = work.join("image_files.txt");
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(()) };
    let kept: String = text.lines().filter(|l| !files.iter().any(|f| f == l)).map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, kept).map_err(|e| e.to_string())
}

/// An image of any size, with only the blocks small enough to be labels if it's a big one.
fn label_part(entry: usize, name: String, img: Image) -> Option<Part> {
    let big = img.h > BUTTON_HEIGHT;
    let mut part = scan(entry, None, (0, 0), name, img)?;
    if big {
        part.blocks.retain(|b| b.y1 - b.y0 <= TALLEST_LABEL);
    }
    (!part.blocks.is_empty()).then_some(part)
}

/// A picture that can hold labels. A tall one on nothing is a character, whose small shapes could
/// pass for labels, so a tall one has to have a plain backdrop.
fn picture_part(entry: usize, name: String, img: Image) -> Option<Part> {
    let tall = img.h > BUTTON_HEIGHT;
    label_part(entry, name, img).filter(|p| !tall || p.bg != CLEAR)
}

/// The same for an Artemis game. The images stay in the archive, so what's repainted is kept beside
/// the translation and goes in with the next write. `english` spells the names drawn as pictures.
pub fn translate_artemis_menus(game: &Path, vision: &dyn Chat, english: &dyn Fn(&str) -> Option<String>, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<String> {
    let mut job = Job::new(game, vision, report, cancel)?;
    let files = artemis::interface_images(game)?;
    let mut parts: Vec<Part> = files.iter().enumerate().filter_map(|(i, (name, png))| label_part(i, name.clone(), read_png(png).ok()?)).collect();
    let changed = job.screen("menus", &mut parts)?;
    let mut painted = Vec::new();
    for (i, img) in &changed {
        let bytes = write_png(img)?;
        if read_png(&bytes).as_ref() != Ok(img) {
            return Err(format!("{}: the repainted image did not read back correctly", parts[*i].name));
        }
        painted.push((parts[*i].name.clone(), bytes));
    }
    let names = job.drawn_names(artemis::drawn_names(game)?, english, &mut painted)?;
    artemis::keep_painted(game, &painted)?;
    Ok(format!("Repainted {} of {} interface images with text on a plain background, and {names} names drawn as pictures. To correct a label, edit vntl_work/image_labels.txt and run again.", changed.len(), parts.len()))
}

/// On a picture on nothing, colours laid down in patches beside nothing, as the checks of a strip
/// under a heading or a plain panel are: paper, not ink. Text is strokes, and a button's face lies
/// inside its frame. Each comes with how far its pattern repeats.
fn papers(img: &Image) -> Vec<(u32, usize)> {
    let w = img.w;
    // pixels, inside a patch, at its edge, and beside nothing
    let mut count: HashMap<u32, (usize, usize, usize, usize)> = HashMap::new();
    for y in 2..img.h.saturating_sub(2) {
        for x in 2..w.saturating_sub(2) {
            let p = img.px[y * w + x];
            if p >> 24 != 0 {
                continue;
            }
            let at = |dx: isize, dy: isize| img.px[(y as isize + dy) as usize * w + (x as isize + dx) as usize];
            let c = count.entry(p).or_default();
            c.0 += 1;
            c.1 += [(-2, 0), (-1, 0), (1, 0), (2, 0), (0, -2), (0, -1), (0, 1), (0, 2)].iter().all(|&(dx, dy)| at(dx, dy) == p) as usize;
            let around = [(-1, 0), (1, 0), (0, -1), (0, 1)].map(|(dx, dy)| at(dx, dy));
            c.2 += around.iter().any(|q| *q != p) as usize;
            c.3 += around.iter().any(|q| q >> 24 == 255) as usize;
        }
    }
    // a share of what's drawn: a sheet can be mostly nothing
    let drawn = img.px.iter().filter(|p| **p >> 24 != 255).count();
    let paper = |(n, inside, edge, beside): (usize, usize, usize, usize)| n * 20 >= drawn && inside * 100 >= n * 40 && beside * 100 >= edge * 60;
    count.into_iter().filter(|(_, c)| paper(*c)).filter_map(|(p, _)| Some((p, period(img, p)?))).collect()
}

/// How far along a row paper's pattern repeats: 2 for a plain strip.
fn period(img: &Image, paper: u32) -> Option<usize> {
    let w = img.w;
    let text = |p: u32| p != paper && p >> 24 != 255;
    let runs: Vec<(usize, usize, usize)> = (0..img.h).filter_map(|y| {
        let row = &img.px[y * w..(y + 1) * w];
        Some((y, row.iter().position(|&p| p == paper)?, row.iter().rposition(|&p| p == paper)?))
    }).collect();
    (2..=64).find(|&step| {
        let (mut same, mut all) = (0, 0);
        for &(y, l, r) in &runs {
            for x in l..(r + 1).saturating_sub(step) {
                let (a, b) = (img.px[y * w + x], img.px[y * w + x + step]);
                if !text(a) && !text(b) {
                    all += 1;
                    same += ((a == paper) == (b == paper)) as usize;
                }
            }
        }
        all > 0 && same * 100 >= all * 98
    })
    .or_else(|| {
        // checks laid in short strips, one to a label, don't line up along a row: there squares as
        // wide as the gaps between them repeat every two
        let (mut lengths, mut gaps): (HashMap<usize, usize>, HashMap<usize, usize>) = (HashMap::new(), HashMap::new());
        for y in 0..img.h {
            let row = &img.px[y * w..(y + 1) * w];
            let (mut x, mut last_end) = (0, None);
            while x < w {
                if row[x] != paper {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < w && row[x] == paper {
                    x += 1;
                }
                *lengths.entry(x - start).or_default() += 1;
                if let Some(end) = last_end {
                    *gaps.entry(start - end).or_default() += 1;
                }
                last_end = Some(x);
            }
        }
        let most = |m: &HashMap<usize, usize>| m.iter().max_by_key(|(_, n)| **n).map(|(k, _)| *k);
        let (side, gap) = (most(&lengths)?, most(&gaps)?);
        (side == gap && (2..=32).contains(&side)).then_some(2 * side)
    })
}

/// Pairs of close colours a face alternates row by row, as a striped button's: (the commoner, the other).
fn stripes(img: &Image) -> Vec<(u32, u32)> {
    let w = img.w;
    let mut pairs: HashMap<(u32, u32), usize> = HashMap::new();
    for i in 0..w * img.h.saturating_sub(2) {
        let (a, b, c) = (img.px[i], img.px[i + w], img.px[i + 2 * w]);
        if a >> 24 == 0 && b >> 24 == 0 && a == c && a != b && !differs(a, b) {
            *pairs.entry((a.min(b), a.max(b))).or_default() += 1;
        }
    }
    let count = |p: u32| img.px.iter().filter(|q| **q == p).count();
    // a face's worth, not a few rows of a gradient
    pairs.into_iter().filter(|(_, n)| *n >= 1000).map(|((a, b), _)| if count(a) >= count(b) { (a, b) } else { (b, a) }).collect()
}

/// The face colour of `stripe` that row `y` shows nearest `x`, where the repaint left it.
fn row_stripe(img: &Image, stripe: (u32, u32), changed: &dyn Fn(usize, usize) -> bool, x: usize, y: usize) -> Option<u32> {
    let row = &img.px[y * img.w..(y + 1) * img.w];
    (1..img.w).flat_map(|d| [x.checked_sub(d), Some(x + d).filter(|&x| x < img.w)]).flatten()
        .find(|&i| !changed(i, y) && (row[i] == stripe.0 || row[i] == stripe.1)).map(|i| row[i])
}

/// What lies under text on paper: the pattern a whole number of steps along the row, from a pixel
/// the repaint left as it was (`changed` tells which it didn't).
fn under(img: &Image, papers: &[(u32, usize)], changed: &dyn Fn(usize, usize) -> bool, x: usize, y: usize) -> u32 {
    papers.iter().find_map(|&(paper, step)| under_one(img, paper, step, changed, x, y)).unwrap_or(CLEAR)
}

fn under_one(img: &Image, paper: u32, step: usize, changed: &dyn Fn(usize, usize) -> bool, x: usize, y: usize) -> Option<u32> {
    // only along the strip of paper x lies in: past its ends, nothing is no part of the pattern
    let reach = |yy: usize, dir: isize| {
        let row = &img.px[yy * img.w..(yy + 1) * img.w];
        let (mut last, mut i): (Option<isize>, isize) = (None, x as isize);
        while i >= 0 && (i as usize) < img.w && (i - last.unwrap_or(x as isize)).abs() <= 2 * step as isize {
            if row[i as usize] == paper {
                last = Some(i);
            }
            i += dir;
        }
        last.map(|l| l as usize)
    };
    // a strip's soft edge holds no paper of its own, so how far it goes comes from the rows beside it
    let ends = |yy: usize| {
        let (l, r) = (reach(yy, -1), reach(yy, 1));
        (l.is_some() || r.is_some()).then(|| (l.unwrap_or(x), r.unwrap_or(x)))
    };
    let (l, r) = [Some(y), y.checked_sub(1), Some(y + 1), y.checked_sub(2), Some(y + 2)].into_iter().flatten().filter(|&yy| yy < img.h).find_map(ends)?;
    let row = &img.px[y * img.w..(y + 1) * img.w];
    // letters are solid: what's neither them nor paper is the edge, see-through
    let shown = |x: usize| (x >= l && x <= r && !changed(x, y) && (row[x] == paper || row[x] >> 24 != 0)).then_some(row[x]);
    (1..=img.w / step).flat_map(|k| [x.checked_sub(k * step), Some(x + k * step).filter(|&x| x < img.w)]).flatten().find_map(shown)
}

/// A block's first mark, if a gap follows it: where the rest starts, and the mark's ink cut to its
/// own rows, with its width and height.
fn first_mark(b: &Block) -> Option<(usize, Vec<bool>, usize, usize)> {
    let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
    let inked = |x: usize| (0..h).any(|y| b.mask[y * w + x]);
    let gap = (0..w).find(|&x| !inked(x))?;
    let next = (gap..w).find(|&x| inked(x))?;
    let rows: Vec<usize> = (0..h).filter(|&y| (0..gap).any(|x| b.mask[y * w + x])).collect();
    let (top, bottom) = (*rows.first()?, *rows.last()? + 1);
    Some((next, (top..bottom).flat_map(|y| (0..gap).map(move |x| b.mask[y * w + x])).collect(), gap, bottom - top))
}

/// Columns `from..to` of a block.
fn columns_of(b: &Block, from: usize, to: usize) -> Block {
    let w = b.x1 - b.x0;
    let mask = (0..b.y1 - b.y0).flat_map(|y| b.mask[y * w + from..y * w + to].to_vec()).collect();
    Block { x0: b.x0 + from, y0: b.y0, x1: b.x0 + to, y1: b.y1, mask, group: b.group }
}

/// The block from column `next` on.
fn cut_block(b: &mut Block, next: usize) {
    *b = columns_of(b, next, b.x1 - b.x0);
}

/// A mark with nothing in its middle, as a checkbox has.
fn hollow(mask: &[bool], w: usize, h: usize) -> bool {
    w >= 7 && h >= 7 && (h / 4..h - h / 4).all(|y| (w / 4..w - w / 4).all(|x| !mask[y * w + x]))
}

/// How many of `marks` have this mark's size and nearly all its pixels.
fn alike(marks: &[&(usize, Vec<bool>, usize, usize)], mask: &[bool], w: usize, h: usize) -> usize {
    marks.iter().filter(|o| (o.2, o.3) == (w, h) && o.1.iter().zip(mask).filter(|(a, b)| a != b).count() * 20 <= mask.len()).count()
}

/// The block's dark strokes only: a box can touch the text's light edge, and stands apart in these.
fn dark_strokes(img: &Image, b: &Block) -> Block {
    let w = b.x1 - b.x0;
    let mask = b.mask.iter().enumerate().map(|(i, m)| *m && rgb(img.px[(b.y0 + i / w) * img.w + b.x0 + i % w]).iter().sum::<i32>() < 300).collect();
    Block { mask, ..b.clone() }
}

/// The columns of the checkboxes like those `marks` hold inside a block, past its first stroke.
fn boxes_in(b: &Block, marks: &[&(usize, Vec<bool>, usize, usize)]) -> Vec<(usize, usize)> {
    let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
    let inked = |x: usize| (0..h).any(|y| b.mask[y * w + x]);
    let mut out = Vec::new();
    let mut x = (0..w).find(|&x| !inked(x)).unwrap_or(w);
    while let Some(s) = (x..w).find(|&x| inked(x)) {
        x = (s..w).find(|&x| !inked(x)).unwrap_or(w);
        let rows: Vec<usize> = (0..h).filter(|&y| (s..x).any(|c| b.mask[y * w + c])).collect();
        let (top, bottom) = (rows[0], rows[rows.len() - 1] + 1);
        let mask: Vec<bool> = (top..bottom).flat_map(|y| (s..x).map(move |c| (y, c))).map(|(y, c)| b.mask[y * w + c]).collect();
        if hollow(&mask, x - s, bottom - top) && alike(marks, &mask, x - s, bottom - top) >= 3 {
            out.push((s, x));
        }
    }
    out
}

/// A block parted where a checkbox sits inside it, as in "□Auto load □Data lock": the text either
/// side is a label of its own, and the box stays as it is.
fn split_at_boxes(b: Block, img: &Image, marks: &[&(usize, Vec<bool>, usize, usize)]) -> Vec<Block> {
    let dark = dark_strokes(img, &b);
    let mut boxes = boxes_in(&b, marks);
    boxes.extend(boxes_in(&dark, marks));
    boxes.sort();
    let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
    let inked = |m: &[bool], x: usize| (0..h).any(|y| m[y * w + x]);
    let (mut out, mut from) = (Vec::new(), 0);
    for (s, e) in boxes.into_iter().filter(|&(s, _)| s > 0) {
        if s < from {
            continue;
        }
        if let Some(end) = (from..s).rfind(|&x| inked(&b.mask, x)) {
            out.push(columns_of(&b, from, end + 1));
        }
        // the next label starts at its letters, not at a light edge touching the box, which a repaint
        // would wipe the box's side with
        from = (e..w).find(|&x| inked(&dark.mask, x)).or_else(|| (e..w).find(|&x| inked(&b.mask, x))).unwrap_or(w);
    }
    if from < w {
        out.push(columns_of(&b, from, w));
    }
    out
}

/// Marks before a label's text stay as they are, and its block starts after them: anything in a
/// colour the text doesn't use (a star, a dotted line), and a hollow square that three or more
/// labels start with (a checkbox), counted over all `parts`, the pieces of one picture.
fn strip_marks(parts: &mut [Part]) {
    for part in parts.iter_mut() {
        let img = &part.img;
        let colours = |b: &Block, cols: std::ops::Range<usize>| {
            let w = b.x1 - b.x0;
            let mut n: HashMap<u32, usize> = HashMap::new();
            for y in 0..b.y1 - b.y0 {
                for x in cols.clone().filter(|&x| b.mask[y * w + x]) {
                    *n.entry(img.px[(b.y0 + y) * img.w + b.x0 + x] & 0xff_ffff).or_default() += 1;
                }
            }
            n
        };
        for b in part.blocks.iter_mut() {
            while let Some((next, mask, w, h)) = first_mark(b) {
                let (mark, rest) = (colours(b, 0..w), colours(b, next..b.x1 - b.x0));
                let Some((c, _)) = mark.into_iter().max_by_key(|(_, n)| *n) else { break };
                let total: usize = rest.values().sum();
                let bw = b.x1 - b.x0;
                let rest_h = (0..b.y1 - b.y0).filter(|&y| (next..bw).any(|x| b.mask[y * bw + x])).count();
                // a dot of a dotted line is far smaller than any letter
                let dot = h * 3 <= rest_h && w * 3 <= rest_h;
                // and an icon drawn solid, as ■, is no letter: a solid stroke is thin, as リ's first, or flat, as ー
                let solid = h >= 6 && h * 2 >= w && w * 2 >= h && mask.iter().filter(|m| **m).count() * 10 >= mask.len() * 9;
                if bw - next < 14 || (!dot && !solid && rest.get(&c).copied().unwrap_or(0) * 20 >= total) {
                    break;
                }
                cut_block(b, next);
            }
        }
    }
    // a box can touch the text's light edge, so it's looked for in the dark strokes too
    let dark: Vec<Vec<Block>> = parts.iter().map(|part| part.blocks.iter().map(|b| dark_strokes(&part.img, b)).collect()).collect();
    let normal: Vec<Vec<Block>> = parts.iter().map(|p| p.blocks.clone()).collect();
    let mut cuts: Vec<Vec<Option<usize>>> = parts.iter().map(|p| vec![None; p.blocks.len()]).collect();
    let marks: Vec<Vec<Vec<_>>> = [&normal, &dark].iter().map(|view| view.iter().map(|bs| bs.iter().map(first_mark).collect()).collect()).collect();
    for (view, marks) in [&normal, &dark].into_iter().zip(&marks) {
        let all: Vec<_> = marks.iter().flatten().flatten().collect();
        for (p, bs) in view.iter().enumerate() {
            for (i, b) in bs.iter().enumerate() {
                let Some((next, mask, w, h)) = marks[p][i].as_ref().filter(|_| cuts[p][i].is_none()) else { continue };
                let bw = b.x1 - b.x0;
                let rest = (0..b.y1 - b.y0).filter(|&y| (*next..bw).any(|x| b.mask[y * bw + x])).count();
                // a box taller than the text beside it is no letter
                if hollow(mask, *w, *h) && (alike(&all, mask, *w, *h) >= 3 || h * 10 >= rest * 13) {
                    cuts[p][i] = Some(*next);
                }
            }
        }
    }
    let all: Vec<_> = marks.iter().flatten().flatten().flatten().collect();
    for (part, cuts) in parts.iter_mut().zip(cuts) {
        let blocks: Vec<Block> = std::mem::take(&mut part.blocks).into_iter().zip(cuts).flat_map(|(mut b, next)| {
            if let Some(next) = next {
                cut_block(&mut b, next);
            }
            split_at_boxes(b, &part.img, &all)
        }).collect();
        part.blocks = blocks;
    }
}

/// Blocks split where whole rows hold no ink: labels stacked closer than the gap between a picture's
/// rows of shapes, each line cut to its own ink.
fn split_lines(blocks: Vec<Block>) -> Vec<Block> {
    blocks.into_iter().flat_map(|b| {
        let (w, h) = (b.x1 - b.x0, b.y1 - b.y0);
        let inked = |y: usize| b.mask[y * w..(y + 1) * w].iter().any(|m| *m);
        let (mut lines, mut y) = (Vec::new(), 0);
        while y < h {
            let top = y;
            while y < h && inked(y) {
                y += 1;
            }
            if y > top {
                lines.push((top, y));
            }
            y += 1;
        }
        if lines.len() < 2 || lines.iter().any(|(t, e)| e - t < 8) {
            return vec![b];
        }
        lines.into_iter().map(|(t, e)| {
            let col = |x: usize| (t..e).any(|y| b.mask[y * w + x]);
            let (l, r) = ((0..w).find(|&x| col(x)).unwrap_or(0), (0..w).rfind(|&x| col(x)).unwrap_or(0) + 1);
            let mask = (t..e).flat_map(|y| b.mask[y * w + l..y * w + r].to_vec()).collect();
            Block { x0: b.x0 + l, y0: b.y0 + t, x1: b.x0 + r, y1: b.y0 + e, mask, group: 0 }
        }).collect()
    }).collect()
}

/// Blocks one close above another, side by side, as one: a button's label in two lines.
fn join_lines(mut blocks: Vec<Block>) -> Vec<Block> {
    blocks.sort_by_key(|b| b.y0);
    let mut out: Vec<Block> = Vec::new();
    for b in blocks {
        let Some(a) = out.iter_mut().rev().find(|a| a.x0 < b.x1 && b.x0 < a.x1 && b.y0 <= a.y1 + (a.y1 - a.y0) / 2) else {
            out.push(b);
            continue;
        };
        let (x0, y0, x1, y1) = (a.x0.min(b.x0), a.y0.min(b.y0), a.x1.max(b.x1), a.y1.max(b.y1));
        let inked = |o: &Block, x: usize, y: usize| x >= o.x0 && x < o.x1 && y >= o.y0 && y < o.y1 && o.mask[(y - o.y0) * (o.x1 - o.x0) + x - o.x0];
        let mask = (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).map(|(x, y)| inked(a, x, y) || inked(&b, x, y)).collect();
        *a = Block { x0, y0, x1, y1, mask, group: 0 };
    }
    out
}

/// The pieces of a picture on nothing that can hold labels, `at` being where it sits in the whole:
/// labels lying on nothing, the faces of solid shapes (buttons, with their text inside), and the
/// same again inside any group of shapes too tall to be a label. Each comes before what's inside it.
fn sheet_parts(entry: usize, name: &str, img: Image, at: (usize, usize), parts: &mut Vec<Part>) {
    let cut = |img: &Image, (x0, y0, x1, y1): Area| Image { w: x1 - x0, h: y1 - y0, px: (y0..y1).flat_map(|y| img.px[y * img.w + x0..y * img.w + x1].to_vec()).collect() };
    // buttons side by side with nothing between them, cut out together, are each a piece of their own
    let pieces = apart(&img);
    if pieces.len() > 1 && pieces.iter().all(|&a| shapes(&cut(&img, a), CLEAR).iter().any(is_solid)) {
        for a in pieces {
            sheet_parts(entry, name, cut(&img, a), (at.0 + a.0, at.1 + a.1), parts);
        }
        return;
    }
    // nor is a row with a button in it one label, as a save screen's tabs and buttons side by side
    let buttons = |s: &Block| {
        let row = cut(&img, (s.x0, s.y0, s.x1, s.y1));
        apart(&row).into_iter().any(|a| shapes(&cut(&row, a), CLEAR).iter().any(is_solid))
    };
    let inside: Vec<Block> = shapes(&img, CLEAR).into_iter().filter(|s| (s.x1 - s.x0, s.y1 - s.y0) != (img.w, img.h) && (is_solid(s) || s.y1 - s.y0 > TALLEST_LABEL || buttons(s))).collect();
    // a sheet's options sit under their setting, not after it on the row, so none takes a hint from its row
    if let Some(mut p) = scan(entry, None, (at.0 as i32, at.1 as i32), name.to_string(), img.clone()) {
        // a block read as pieces of its own below, or in one, isn't read here, nor split into lines
        p.blocks.retain(|b| b.y1 - b.y0 <= TALLEST_LABEL && !inside.iter().any(|s| s.x0 <= b.x0 && b.x1 <= s.x1 && s.y0 <= b.y0 && b.y1 <= s.y1));
        // labels on nothing can stand one above another; inside a button, two lines are one label
        p.blocks = if p.bg == CLEAR { split_lines(std::mem::take(&mut p.blocks)) } else { join_lines(std::mem::take(&mut p.blocks)) };
        parts.extend((!p.blocks.is_empty()).then_some(p));
    }
    for s in inside {
        let crop = cut(&img, (s.x0, s.y0, s.x1, s.y1));
        // buttons side by side stand closer than words do, so a solid shape parts where nothing is between
        // and a tall group where a wide gap runs top to bottom, as between labels and a colour picker
        let wide = columns(&crop, 10);
        let boxes = if is_solid(&s) && wide.len() < 2 { apart(&crop) } else { wide };
        for b in boxes {
            sheet_parts(entry, name, cut(&crop, b), (at.0 + s.x0 + b.0, at.1 + s.y0 + b.1), parts);
        }
    }
}

/// Where a picture on nothing parts side by side, at `gap` or more columns holding nothing.
fn columns(img: &Image, gap: usize) -> Vec<Area> {
    let inked = |x: usize| (0..img.h).any(|y| img.px[y * img.w + x] >> 24 < 195);
    let mut out: Vec<Area> = Vec::new();
    for x in (0..img.w).filter(|&x| inked(x)) {
        match out.last_mut() {
            Some(last) if x - last.2 < gap => last.2 = x + 1,
            _ => out.push((x, 0, x + 1, img.h)),
        }
    }
    out
}

/// Where a picture on nothing parts into pieces with nothing at all between them: side by side, or
/// else one above another.
fn apart(img: &Image) -> Vec<Area> {
    let runs = |n: usize, inked: &dyn Fn(usize) -> bool| {
        let (mut out, mut i) = (Vec::new(), 0);
        while i < n {
            let start = i;
            while i < n && inked(i) {
                i += 1;
            }
            if i > start {
                out.push((start, i));
            }
            i += 1;
        }
        out
    };
    let seen = |x: usize, y: usize| img.px[y * img.w + x] >> 24 < 195;
    let cols = runs(img.w, &|x| (0..img.h).any(|y| seen(x, y)));
    if cols.len() > 1 {
        return cols.into_iter().map(|(l, r)| (l, 0, r, img.h)).collect();
    }
    runs(img.h, &|y| (0..img.w).any(|x| seen(x, y))).into_iter().map(|(t, b)| (0, t, img.w, b)).collect()
}

/// The same for a BGI game. Its menus are transparent sheets of labels and buttons, rewritten into
/// sysgrp.arc with the next write.
pub fn translate_bgi_menus(game: &Path, vision: &dyn Chat, english: &dyn Fn(&str) -> Option<String>, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<String> {
    let mut job = Job::new(game, vision, report, cancel)?;
    job.names = english;
    let text = bgi::Game::open(game)?;
    job.words = text.menu_words().iter().map(|w| w.chars().collect()).collect();
    job.people = text.speakers().iter().map(|w| w.chars().collect()).collect();
    let files = bgi::interface_pictures(game)?;
    let (mut parts, mut own) = (Vec::new(), Vec::new());
    for (i, (name, b)) in files.iter().enumerate() {
        let img = bgi::picture(b).ok_or(format!("{name}: not a picture"))?;
        // a striped face is read as its commoner colour, and its stripes go back where a repaint wipes them
        let stripes = stripes(&img);
        let mut bare = img.clone();
        bare.px.iter_mut().for_each(|q| *q = stripes.iter().find(|s| s.1 == *q).map_or(*q, |s| s.0));
        if flat_background(&bare) != Some(CLEAR) {
            parts.extend(label_part(i, name.clone(), bare));
            own.push((img, Vec::new(), stripes));
            continue;
        }
        // text is read with the paper taken away, and the paper's pattern goes back under the English
        let papers = papers(&img);
        bare.px.iter_mut().filter(|q| papers.iter().any(|p| p.0 == **q)).for_each(|q| *q = CLEAR);
        let start = parts.len();
        sheet_parts(i, name, bare, (0, 0), &mut parts);
        strip_marks(&mut parts[start..]);
        own.push((img, papers, stripes));
    }
    job.vision_first = true;
    let changed = job.screen("sysgrp", &mut parts)?;
    // each picture again, with only what a repaint changed, over the paper where there is some
    let mut new_px: IndexMap<usize, HashMap<usize, u32>> = IndexMap::new();
    for (i, img) in &changed {
        let p = &parts[*i];
        let w = own[p.entry].0.w;
        let n = new_px.entry(p.entry).or_default();
        let (x0, y0) = (p.at.0 as usize, p.at.1 as usize);
        for (y, x) in (0..img.h).flat_map(|y| (0..img.w).map(move |x| (y, x))) {
            let new = img.px[y * img.w + x];
            if new != p.img.px[y * img.w + x] {
                n.insert((y0 + y) * w + x0 + x, new);
            }
        }
    }
    let mut sheets: IndexMap<usize, Image> = IndexMap::new();
    for (entry, n) in new_px.iter_mut() {
        let (whole, papers, stripes) = &own[*entry];
        let mut sheet = whole.clone();
        // a light outline can be the paper's very colour, so it went with the paper and wasn't
        // erased with its letters: paper colour beside what was is the pattern again
        let (w, h) = (whole.w as isize, whole.h as isize);
        let beside: Vec<usize> = n.keys().flat_map(|&at| {
            let (x, y) = ((at % whole.w) as isize, (at / whole.w) as isize);
            (-3..=3).flat_map(move |dy| (-3..=3).map(move |dx| (x + dx, y + dy)))
        }).filter(|&(x, y)| x >= 0 && y >= 0 && x < w && y < h).map(|(x, y)| (y * w + x) as usize)
            .filter(|at| papers.iter().any(|p| p.0 == whole.px[*at])).collect();
        beside.into_iter().for_each(|at| { n.entry(at).or_insert(CLEAR); });
        let changed = |x: usize, y: usize| n.contains_key(&(y * whole.w + x));
        for (&at, &new) in n.iter() {
            let (x, y) = (at % whole.w, at / whole.w);
            // a stripe wiped down to the face shows its row's own colour again
            let new = stripes.iter().find(|s| s.0 == new).and_then(|&s| row_stripe(whole, s, &changed, x, y)).unwrap_or(new);
            let mut px = under(whole, papers, &changed, x, y);
            over(&mut px, rgb(new), (255 - (new >> 24)) as f32 / 255.0);
            sheet.px[at] = px;
        }
        sheets.insert(*entry, sheet);
    }
    let mut painted = Vec::new();
    for (entry, sheet) in &sheets {
        let (name, stored) = &files[*entry];
        let bytes = bgi::bitmap(stored, sheet);
        if bgi::picture(&bytes).as_ref() != Some(sheet) {
            return Err(format!("{name}: the repainted picture did not read back correctly"));
        }
        painted.push((name.clone(), bytes));
    }
    artemis::keep_painted(game, &painted)?;
    Ok(format!("Repainted {} of {} menu pictures. To correct a label, edit vntl_work/image_labels.txt and run again.", painted.len(), files.len()))
}

/// The same for an RPG Maker game: interface images are files, some encrypted, rewritten in place.
pub fn translate_rpgmaker_menus(game: &Path, vision: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<String> {
    let mut job = Job::new(game, vision, report, cancel)?;
    let key = rpgmaker::image_key(game)?;
    let paths = rpgmaker::interface_images(game);
    // shortcut: nothing taller than a button, because full screens are artwork, as for Ren'Py
    let mut parts = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let Some(img) = rpgmaker::read_image(path, &key).ok().and_then(|d| button_image(&d)) else { continue };
        let name = path.strip_prefix(game).unwrap_or(path).to_string_lossy().replace('\\', "/");
        parts.extend(scan(i, None, (0, 0), name, img));
    }
    let changed = job.screen("menus", &mut parts)?;
    for (i, img) in &changed {
        let bytes = write_png(img)?;
        if read_png(&bytes).as_ref() != Ok(img) {
            return Err(format!("{}: the repainted image did not read back correctly", parts[*i].name));
        }
        rpgmaker::write_image(&paths[parts[*i].entry], &key, &bytes)?;
    }
    // what an earlier run repainted and this one doesn't goes back
    let painted: BTreeSet<usize> = changed.iter().map(|(i, _)| parts[*i].entry).collect();
    for (_, path) in paths.iter().enumerate().filter(|(i, p)| !painted.contains(i) && crate::backup(p).exists()) {
        let own = original(path)?;
        if std::fs::read(path).map_err(|e| e.to_string())? != own {
            replace_file(path, &own)?;
        }
    }
    Ok(format!(
        "Repainted {} of {} menu images that have a plain background (read back OK; originals kept as *.vntl-orig). \
         To correct a label, edit vntl_work/image_labels.txt and run again.",
        changed.len(),
        parts.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plate of white letters far apart, on nothing, as WCG holds it.
    fn plate() -> (Image, Vec<u8>) {
        let mut img = Image { w: 200, h: 30, px: vec![CLEAR; 6000] };
        for (x, y) in (6..20).flat_map(|y| (10..24).chain(150..164).map(move |x| (x, y))) {
            img.px[y * 200 + x] = 0x00ff_ffff;
        }
        let mut head = b"WG\x41\0\0\0\0\0".to_vec();
        head.extend(200u32.to_le_bytes());
        head.extend(30u32.to_le_bytes());
        (img.clone(), liar::write_wcg(&head, &img).unwrap())
    }

    #[test]
    fn a_plate_loses_all_its_lettering_and_gets_the_name() {
        let (img, _) = plate();
        let got = paint_plate(&plate_part(img.clone()).unwrap(), "ハナ", "Hana", &bold_font().unwrap()).unwrap();
        assert_eq!(got.px[10 * 200 + 160] >> 24, 255, "the far letter, past the name, is gone");
        assert!(got.px.iter().filter(|p| **p >> 24 == 0).count() > 20, "the name is drawn");
        assert_eq!(got.px.iter().filter(|p| **p >> 24 == 0).map(|p| p & 0xffffff).max(), Some(0xffffff), "in the lettering's own colour");
    }

    #[test]
    fn a_plate_keeps_its_own_brackets_around_the_name() {
        let (img, _) = plate();
        let got = paint_plate(&plate_part(img.clone()).unwrap(), "【ハナ】", "Hana", &bold_font().unwrap()).unwrap();
        let column = |image: &Image, x: usize| (0..30).filter(|y| image.px[y * 200 + x] >> 24 == 0).count();
        assert_eq!(column(&got, 12), column(&img, 12), "the left one hasn't moved");
        assert_eq!(column(&got, 155), 0, "the right one has left its old place");
        let last = (0..200).rev().find(|&x| column(&got, x) > 0).unwrap();
        assert!((60..120).contains(&last), "and sits after the name, at {last}");
        assert_eq!(column(&got, last), column(&img, 163), "as the plate drew it");
    }

    #[test]
    fn plates_come_from_the_game_and_the_rest_of_the_archive_stays() {
        let game = std::env::temp_dir().join(format!("vntl_plates_{}", std::process::id()));
        std::fs::create_dir_all(&game).unwrap();
        let (_, data) = plate();
        let entry = |name: &str, data: &[u8]| liar::Entry { name: name.into(), data: data.to_vec() };
        let shipped = liar::write_xfl(&[entry("gf001.wcg", &data), entry("gf999.wcg", &data), entry("menu.wcg", b"game's")]);
        let path = game.join("grps.xfl");
        std::fs::write(&path, &shipped).unwrap();
        let names = HashMap::from([("001".to_string(), ("ハナ".to_string(), "Hana".to_string()))]);
        assert_eq!(paint_plates(&game, &names).unwrap(), 1);
        let first = std::fs::read(&path).unwrap();
        let back = liar::read_xfl(&first).unwrap();
        assert_ne!(back[0].data, data);
        assert_eq!((back[1].data.as_slice(), back[2].data.as_slice()), (data.as_slice(), b"game's".as_slice()));

        // a menu repainted in between stays, and painting again changes nothing else
        let mut menus = liar::read_xfl(&first).unwrap();
        menus[2].data = b"repainted".to_vec();
        replace_file(&path, &liar::write_xfl(&menus)).unwrap();
        paint_plates(&game, &names).unwrap();
        let back = liar::read_xfl(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back[2].data, b"repainted");
        assert_eq!(std::fs::read(game.join("grps.xfl.vntl-orig")).unwrap(), shipped, "the game's own stays safe");
        let _ = std::fs::remove_dir_all(&game);
    }

    #[test]
    fn a_plate_names_someone_only_if_it_says_something_japanese() {
        let plate = Image { w: 2, h: 1, px: vec![CLEAR, 0x00ff_ffff] };
        let read = |reply: &'static str| read_plate(None, Some(&crate::llm::tests::Scripted::new(&[reply])), &plate).unwrap();
        assert_eq!(read("田 中 。"), Some("田中".to_string()));
        assert_eq!(read("NONE"), None);
        assert_eq!(read_plate(None, None, &plate).unwrap(), None);
    }
    use crate::llm::tests::Scripted;
    use std::cell::RefCell;

    #[test]
    fn a_reply_that_talks_about_the_label_is_not_painted() {
        assert_eq!(paintable("Ri (likely a setting name, unclear)", "リ"), None);
        assert_eq!(paintable("Option / Settings", "設定").as_deref(), Some("Option"));
        assert_eq!(paintable("Text Speed", "文字速度").as_deref(), Some("Text Speed"));
        assert_eq!(paintable("設定", "設定"), None);
        assert_eq!(paintable("Save (Overwrite)", "セーブ（上書き）").as_deref(), Some("Save (Overwrite)"), "the label has brackets of its own");
        assert_eq!(paintable("Yes: Confirm", "はい").as_deref(), Some("Yes"));
        assert_eq!(paintable("Hue [X]", "色相［X］").as_deref(), Some("Hue [X]"));
        assert_eq!(paintable("Hue [maybe color]", "色相"), None);
        assert_eq!(paintable("Auto Release (Right Click)", "オートモード解除を右クリックのみにする").as_deref(), Some("Auto Release (Right Click)"));
        assert_eq!(paintable("Bike Back (likely \"B-back\")", "バイクバック"), None);
        assert_eq!(paintable("Load Complete (Load)", "ロード完了しました").as_deref(), Some("Load Complete"));
        assert_eq!(paintable("Scene Viewing (View)", "シーン鑑賞モード中です").as_deref(), Some("Scene Viewing"));
        assert_eq!(paintable("Harém", "ハーレム").as_deref(), Some("Harem"), "no accent the label doesn't have");
        assert_eq!(paintable("Move painting left.", "絵を左へ").as_deref(), Some("Move painting left"), "nor a full stop");
        assert_eq!(paintable("Wait...", "待って…").as_deref(), Some("Wait..."), "but dots stay");
        assert_eq!(paintable("Café Noël", "Café Noël").as_deref(), Some("Café Noël"), "and those it has stay");
    }

    #[test]
    fn a_name_on_a_menu_is_spelled_as_the_story_has_it() {
        let names = |jp: &str| [("花子", "Hanako"), ("太郎", "Taro")].iter().find(|n| n.0 == jp).map(|n| n.1.to_string());
        assert_eq!(spelled_names("花子＆太郎", &names).as_deref(), Some("Hanako & Taro"));
        assert_eq!(spelled_names("花子", &names).as_deref(), Some("Hanako"));
        assert_eq!(spelled_names("花子の部屋", &names), None, "not only a name");
    }

    #[test]
    fn a_wide_gap_top_to_bottom_parts_a_group_and_a_gap_between_letters_does_not() {
        let mut img = Image { w: 100, h: 40, px: vec![CLEAR; 100 * 40] };
        for (x, y) in (0..40).flat_map(|y| [2, 3, 6, 7, 30, 31, 32].map(|x| (x, y))) {
            img.px[y * 100 + x] = 0;
        }
        assert_eq!(columns(&img, 10), [(2, 0, 8, 40), (30, 0, 33, 40)]);
    }

    #[test]
    fn an_empty_sprite_has_nothing_to_read() {
        assert!(scan(0, Some(0), (0, 0), "empty".into(), Image { w: 0, h: 4, px: Vec::new() }).is_none());
        assert!(scan(0, Some(0), (0, 0), "empty".into(), Image { w: 4, h: 0, px: Vec::new() }).is_none());
    }

    #[test]
    fn a_long_low_bar_with_words_on_it_is_a_shape_and_its_words_are_not() {
        let block = |w: usize, h: usize, filled: usize| Block { x0: 0, y0: 0, x1: w, y1: h, mask: (0..w * h).map(|i| i % 100 < filled).collect(), group: 0 };
        assert!(is_solid(&block(300, 14, 95)));
        assert!(!is_solid(&block(300, 14, 60)), "letters leave gaps");
        assert!(!is_solid(&block(40, 14, 95)), "a short block is a word");
    }

    #[test]
    fn a_reading_a_letter_off_the_games_own_text_finds_it() {
        let words: Vec<Vec<char>> = ["クイックジャンプを実行します", "メッセージウィンドウ不透明率", "はい"].iter().map(|w| w.chars().collect()).collect();
        assert_eq!(nearest(&words, "クイックシャンプ").as_deref(), Some("クイックジャンプ"));
        assert_eq!(nearest(&words, "メッセージウィンドウ不明率").as_deref(), Some("メッセージウィンドウ不透明率"));
        assert_eq!(nearest(&words, "クイックジャンプ"), None, "already right");
        assert_eq!(nearest(&words, "はし"), None, "too short to tell");
        assert_eq!(nearest(&words, "システム設定"), None, "nothing near");
    }

    #[test]
    fn only_a_plain_yes_is_japanese() {
        let says = |reply: &'static str| shows_japanese(&crate::llm::tests::Scripted::new(&[reply]), b"").unwrap();
        assert!(says("JAPANESE") && says(" Japanese."));
        assert!(!says("ENGLISH") && !says("NONE. The image shows dots"));
    }

    #[test]
    fn of_two_readings_the_model_picks_the_one_the_picture_shows() {
        // asked both ways round: the second time いいえ is shown first
        let picks = |one: &'static str, two: &'static str| choose(&crate::llm::tests::Scripted::new(&[one, two]), b"", "はい", "いいえ").unwrap();
        assert_eq!(picks("2", "1.").as_deref(), Some("いいえ"));
        assert_eq!(picks("0", "0").as_deref(), Some(""), "an arrow read as a word");
        assert_eq!(picks("1", "1"), None, "it only follows the order");
        assert_eq!(picks("I can't tell", "2"), None);
    }

    #[test]
    fn a_checkbox_read_into_a_label_is_dropped() {
        assert_eq!(clean("オートロード└データロック".into()), "オートロードデータロック");
        assert_eq!(clean("□セーブ？".into()), "セーブ");
        assert_eq!(clean("・ハーレム・".into()), "ハーレム");
    }

    #[test]
    fn where_the_picture_doesnt_settle_it_the_word_that_fits_beside_the_others_does() {
        let asks = |replies: &[&'static str]| likelier(&crate::llm::tests::Scripted::new(replies), "レスキュー", "いいえ", &["はい".into()]).unwrap();
        assert_eq!(asks(&["2", "1"]).as_deref(), Some("いいえ"));
        assert_eq!(asks(&["1", "1"]), None, "it only follows the order");
        let real = |replies: &[&'static str]| real_of_two(&crate::llm::tests::Scripted::new(replies), "ハッピーテイ", "ハッピーデイ").unwrap();
        assert_eq!(real(&["NO", "YES"]).as_deref(), Some("ハッピーデイ"), "each asked of on its own");
        assert_eq!(real(&["YES", "YES"]), None, "and both real is no answer");
    }

    #[test]
    fn a_short_reading_near_one_speakers_name_is_offered_that_name() {
        let people: Vec<Vec<char>> = ["さくら", "美咲里", "花子"].iter().map(|w| w.chars().collect()).collect();
        assert_eq!(nearest_name(&people, "さくい").as_deref(), Some("さくら"));
        assert_eq!(nearest_name(&people, "美咲理").as_deref(), Some("美咲里"));
        assert_eq!(nearest_name(&people, "美咲里"), None, "already right");
        assert_eq!(nearest_name(&people, "システム設定"), None, "too long to be a name misread");
    }

    #[test]
    fn on_a_big_image_only_a_small_block_can_be_a_label() {
        // nothing, a tall figure with gaps in it, and below it a short label
        let (w, h) = (300, 400);
        let mut px = vec![CLEAR; w * h];
        for y in 20..300 {
            for x in 40..240 {
                if (x + y) % 3 != 0 {
                    px[y * w + x] = 0x0080_4020;
                }
            }
        }
        for y in 340..360 {
            for x in (60..120).step_by(2) {
                px[y * w + x] = 0x00ff_ffff;
            }
        }
        let part = label_part(0, "picker".into(), Image { w, h, px: px.clone() }).unwrap();
        assert_eq!(part.blocks.len(), 1);
        assert!(part.blocks[0].y0 >= 340, "the figure stays out of it");

        // on a button-sized image the same shapes aren't filtered, as before
        let short = Image { w, h: 190, px: px[..w * 190].to_vec() };
        assert!(label_part(0, "button".into(), short).is_some());
    }

    #[test]
    fn a_solid_button_on_nothing_is_not_text() {
        let (w, h) = (100, 40);
        let mut px = vec![CLEAR; w * h];
        for y in 5..35 {
            for x in 5..95 {
                px[y * w + x] = 0; // an opaque black button
            }
        }
        assert!(find_blocks(&Image { w, h, px: px.clone() }, CLEAR).is_empty());
        // strokes with gaps between them, as letters have
        for (i, p) in px.iter_mut().enumerate() {
            if i % w % 3 != 0 {
                *p = CLEAR;
            }
        }
        assert_eq!(find_blocks(&Image { w, h, px }, CLEAR).len(), 1);
    }

    #[test]
    fn a_button_on_a_sheet_of_nothing_is_read_on_its_own_face() {
        let (w, h) = (300, 100);
        let mut sheet = Image { w, h, px: vec![CLEAR; w * h] };
        let b = button(35);
        for y in 0..b.h {
            sheet.px[(30 + y) * w + 150..][..b.w].copy_from_slice(&b.px[y * b.w..][..b.w]);
        }
        let mut parts = Vec::new();
        sheet_parts(7, "s", sheet, (0, 0), &mut parts);
        assert_eq!(parts.iter().map(|p| (p.entry, p.name.as_str(), p.at, p.blocks.len(), p.bg)).collect::<Vec<_>>(), [(7, "s", (150, 30), 1, 0)]);
    }

    #[test]
    fn a_row_of_buttons_is_not_one_label_and_each_button_is_read_on_its_face() {
        let (w, h) = (300, 100);
        let mut sheet = Image { w, h, px: vec![CLEAR; w * h] };
        // a label on nothing, then two buttons with a narrow gap: all closer than words stand
        for (y, x) in (44..56).flat_map(|y| (12..67).filter(|x| x % 3 == 0).map(move |x| (y, x))) {
            sheet.px[y * w + x] = 0x00ff_ffff;
        }
        let b = button(35);
        for left in [78, 184] {
            for y in 0..b.h {
                sheet.px[(30 + y) * w + left..][..b.w].copy_from_slice(&b.px[y * b.w..][..b.w]);
            }
        }
        let mut parts = Vec::new();
        sheet_parts(0, "s", sheet, (0, 0), &mut parts);
        assert_eq!(parts.iter().map(|p| (p.at, p.bg, p.blocks.len())).collect::<Vec<_>>(), [((12, 30), CLEAR, 1), ((78, 30), 0, 1), ((184, 30), 0, 1)]);
    }

    #[test]
    fn two_lines_inside_a_button_are_one_label_and_a_label_beside_stays_apart() {
        let block = |x0: usize, y0: usize| Block { x0, y0, x1: x0 + 20, y1: y0 + 10, mask: vec![true; 200], group: 0 };
        let joined = join_lines(vec![block(10, 22), block(12, 10), block(60, 10)]);
        assert_eq!(joined.iter().map(|b| (b.x0, b.y0, b.x1, b.y1)).collect::<Vec<_>>(), [(10, 10, 32, 32), (60, 10, 80, 20)]);
        assert!(!joined[0].mask[11 * 22] && joined[0].mask[2], "the gap between the lines holds no ink");
    }

    #[test]
    fn a_checked_strip_is_paper_and_its_pattern_goes_back_under_text() {
        let (w, h, paper) = (200, 60, 0x00ef_edeb);
        let mut img = Image { w, h, px: vec![CLEAR; w * h] };
        for (y, x) in (15..45).flat_map(|y| (15..195).map(move |x| (y, x))) {
            if (x / 15 + y / 15) % 2 == 0 {
                img.px[y * w + x] = paper;
            }
        }
        for (y, x) in (26..34).flat_map(|y| (50..120).map(move |x| (y, x))) {
            img.px[y * w + x] = 0x0048_260f;
        }
        let found = papers(&img);
        assert_eq!(found, [(paper, 30)]);
        // checks in short strips, each starting where it likes
        let mut short = Image { w: 300, h: 30, px: vec![CLEAR; 300 * 30] };
        for (start, y) in [(5, 0), (130, 0), (5, 15), (137, 15)].iter().flat_map(|&(s, y0)| (y0..y0 + 15).map(move |y| (s, y))) {
            for x in (0..3).flat_map(|k| start + k * 30..start + k * 30 + 15) {
                short.px[y * 300 + x] = paper;
            }
        }
        assert_eq!(period(&short, paper), Some(30));
        // the same strip on a sheet that is mostly nothing
        let mut wide = Image { w: 1000, h: 200, px: vec![CLEAR; 1000 * 200] };
        (0..h).for_each(|y| wide.px[y * 1000..y * 1000 + w].copy_from_slice(&img.px[y * w..(y + 1) * w]));
        assert_eq!(papers(&wide), [(paper, 30)]);
        let none = |_: usize, _: usize| false;
        assert_eq!([under(&img, &found, &none, 60, 30), under(&img, &found, &none, 80, 30)], [paper, CLEAR], "the checks carry on under the text");
        assert_eq!(under(&img, &found, &none, 60, 55), CLEAR, "off the strip there is nothing");
        // the strip's soft top edge, half see-through, under text that reaches over it
        let edge = 0x80ef_edeb;
        (15..195).for_each(|x| img.px[14 * w + x] = edge);
        img.px[14 * w + 60] = 0x0048_260f;
        assert_eq!(under(&img, &found, &|x, y| (x, y) == (60, 14), 60, 14), edge, "the edge carries on too");
    }

    #[test]
    fn a_checkbox_several_labels_start_with_stays_out_of_the_text() {
        let (w, h, ink) = (120, 100, 0x0048_260f);
        let mut img = Image { w, h, px: vec![CLEAR; w * h] };
        for top in [10, 40, 70] {
            for (y, x) in (top..top + 12).flat_map(|y| (10..22).map(move |x| (y, x))) {
                let edge = !(12..20).contains(&x) || !(top + 2..top + 10).contains(&y);
                if edge {
                    img.px[y * w + x] = ink;
                }
            }
            for (y, x) in (top..top + 12).flat_map(|y| (28..80).filter(|x| x % 3 == 0).map(move |x| (y, x))) {
                img.px[y * w + x] = ink;
            }
        }
        let mut part = scan(0, None, (0, 0), "s".into(), img).unwrap();
        strip_marks(std::slice::from_mut(&mut part));
        assert_eq!(part.blocks.iter().map(|b| b.x0).collect::<Vec<_>>(), [30, 30, 30], "each starts at its first stroke");
        let mut lone = Image { w: 20, h: 20, px: vec![CLEAR; 400] };
        for (y, x) in (4..16).flat_map(|y| (4..16).map(move |x| (y, x))).filter(|&(y, x)| !(6..14).contains(&x) || !(6..14).contains(&y)) {
            lone.px[y * 20 + x] = ink;
        }
        assert!(scan(0, None, (0, 0), "box".into(), lone).is_none(), "an empty checkbox holds no text");
    }

    #[test]
    fn an_icon_drawn_solid_before_a_label_stays_out_of_it() {
        let (w, h, ink) = (120, 40, 0x0048_260f);
        let mut img = Image { w, h, px: vec![CLEAR; w * h] };
        // ■ in the text's own colour, then "text" of thin solid strokes, which are letters' strokes
        (10..22).flat_map(|y| (10..22).map(move |x| (y, x))).for_each(|(y, x)| img.px[y * w + x] = ink);
        for (y, x) in (10..24).flat_map(|y| (30..80).filter(|x| x % 3 == 0).map(move |x| (y, x))) {
            img.px[y * w + x] = ink;
        }
        let mut part = scan(0, None, (0, 0), "s".into(), img).unwrap();
        strip_marks(std::slice::from_mut(&mut part));
        assert_eq!(part.blocks.iter().map(|b| b.x0).collect::<Vec<_>>(), [30], "the text starts after the icon, with all its strokes");
    }

    #[test]
    fn a_checkbox_inside_a_row_starts_another_label() {
        let (w, h, ink) = (170, 100, 0x0048_260f);
        let mut img = Image { w, h, px: vec![CLEAR; w * h] };
        for top in [10, 40, 70] {
            for left in [10, 87] {
                for (y, x) in (top..top + 12).flat_map(|y| (left..left + 12).map(move |x| (y, x))) {
                    if !(left + 2..left + 10).contains(&x) || !(top + 2..top + 10).contains(&y) {
                        img.px[y * w + x] = ink;
                    }
                }
                for (y, x) in (top..top + 12).flat_map(|y| (left + 18..left + 70).filter(|x| x % 3 == 0).map(move |x| (y, x))) {
                    img.px[y * w + x] = ink;
                }
            }
        }
        let mut part = scan(0, None, (0, 0), "s".into(), img).unwrap();
        assert_eq!(part.blocks.len(), 3, "a row is one block until its boxes are found");
        strip_marks(std::slice::from_mut(&mut part));
        let spans: Vec<(usize, usize)> = part.blocks.iter().map(|b| (b.x0, b.x1)).collect();
        assert_eq!(spans, [(30, 79), (105, 157), (30, 79), (105, 157), (30, 79), (105, 157)], "two labels a row, the boxes left out");
    }

    /// A black button in a grey frame with a white bar of "text".
    fn button(text_x: usize) -> Image {
        let (w, h) = (100, 40);
        let mut px = vec![0x0080_8080u32; w * h];
        for y in 4..36 {
            for x in 4..96 {
                px[y * w + x] = 0;
            }
        }
        for y in 14..26 {
            for x in text_x..text_x + 30 {
                px[y * w + x] = 0x00ff_ffff;
            }
        }
        Image { w, h, px }
    }

    #[test]
    fn finds_text_inside_the_frame_and_ignores_artwork() {
        let img = button(35);
        let bg = flat_background(&img).unwrap();
        assert_eq!(bg, 0);
        let b = find_blocks(&img, bg);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].x0, b[0].y0, b[0].x1, b[0].y1), (35, 14, 65, 26));
        let noisy: Vec<u32> = (0..4000u32).map(|i| i.wrapping_mul(2654435761) & 0xffffff).collect();
        assert!(flat_background(&Image { w: 100, h: 40, px: noisy }).is_none());
    }

    #[test]
    fn a_frame_edge_in_the_background_colour_stays_outside_the_text_area() {
        // black outer edge, grey frame, black fill, and a label reaching close to the frame
        let (w, h) = (100, 40);
        let mut px = vec![0u32; w * h];
        for y in 1..39 {
            for x in 1..99 {
                px[y * w + x] = 0x0080_8080;
            }
        }
        for y in 4..36 {
            for x in 4..96 {
                px[y * w + x] = 0;
            }
        }
        for y in 14..26 {
            for x in 6..94 {
                px[y * w + x] = 0x00ff_ffff;
            }
        }
        let img = Image { w, h, px };
        assert_eq!(spans(&img, 0)[20], Some((4, 95)));
        let b = find_blocks(&img, 0);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].x0, b[0].x1), (6, 94));
    }

    #[test]
    fn text_on_a_transparent_background_is_found_and_keeps_its_place_and_colour() {
        let (w, h) = (100, 30);
        let mut px = vec![CLEAR | 0x12_3456; w * h]; // transparent, hiding a stray colour
        for y in 8..22 {
            for x in 10..40 {
                px[y * w + x] = 0x007b_7b7b; // solid grey text
            }
        }
        let part = scan(0, None, (0, 0), "t".into(), Image { w, h, px }).unwrap();
        assert_eq!(part.bg, CLEAR);
        assert_eq!((part.blocks[0].x0, part.blocks[0].x1), (10, 40));
        let bytes = FONTS.iter().find_map(|p| std::fs::read(p).ok()).unwrap();
        let font = Font::from_bytes(bytes, FontSettings::default()).unwrap();
        let label = "Hi".to_string();
        let img = repaint(&part, &[Some(&label)], &font).unwrap();
        let ink: Vec<usize> = (0..w * h).filter(|i| img.px[*i] >> 24 < 64).collect();
        assert!(ink.iter().all(|i| i % w >= 9), "text stays where the original started");
        assert!(ink.iter().any(|i| img.px[*i] & 0xff_ffff == 0x7b_7b7b), "and keeps its colour");
        assert!(img.px[5 * w + 80] == CLEAR, "the rest stays transparent");
    }

    #[test]
    fn text_touching_the_edge_of_a_transparent_image_is_fully_erased_and_states_match() {
        let (w, h) = (60, 30);
        let label = |color: u32| {
            let mut px = vec![CLEAR; w * h];
            for y in 0..26 {
                for x in (0..25).step_by(2) {
                    px[y * w + x] = color; // starts at the very first column and row
                }
            }
            scan(0, None, (0, 0), "t".into(), Image { w, h, px }).unwrap()
        };
        let (dark, grey) = (label(0), label(0x007b_7b7b));
        assert_eq!(dark.blocks[0].x0, 0);
        assert!(same_text(&dark.blocks[0], &grey.blocks[0], 96), "the same label in two colours");
        let bytes = FONTS.iter().find_map(|p| std::fs::read(p).ok()).unwrap();
        let font = Font::from_bytes(bytes, FontSettings::default()).unwrap();
        let img = repaint(&dark, &[Some(&"I".to_string())], &font).unwrap();
        assert!(img.px[24] == CLEAR && img.px[20 * w + 24] == CLEAR, "the old text is gone right up to its edge");
    }

    #[test]
    fn dark_text_gets_a_white_backdrop_for_the_model() {
        let part = scan(0, None, (0, 0), "t".into(), {
            let (w, h) = (40, 20);
            let mut px = vec![CLEAR; w * h];
            (5..15).for_each(|y| (5..30).for_each(|x| px[y * w + x] = 0));
            Image { w, h, px }
        }).unwrap();
        let shown = block_image(&part, &part.blocks[0], 1);
        assert_eq!(shown.px[0] & 0xff_ffff, 0xff_ffff, "transparent becomes white");
    }

    #[test]
    fn png_round_trip_keeps_pixels_and_transparency() {
        let img = Image { w: 2, h: 2, px: vec![CLEAR, 0x00ff_0000, 0x8000_ff00, 0x0000_00ff] };
        assert_eq!(read_png(&write_png(&img).unwrap()).unwrap(), img);
    }

    #[test]
    fn short_labels_of_the_same_size_stay_apart() {
        let shape = |rows: &[(usize, usize)]| {
            let mut mask = vec![false; 100];
            rows.iter().for_each(|(x, y)| mask[y * 20 + x] = true);
            Block { x0: 0, y0: 0, x1: 20, y1: 5, mask, group: 0 }
        };
        // nearly all blank, so a pixel-for-pixel comparison would call these the same
        let (a, b) = (shape(&[(1, 1), (2, 1), (3, 1)]), shape(&[(10, 3), (11, 3), (12, 3)]));
        assert!(!same_text(&a, &b, 80));
        assert!(same_text(&a, &shape(&[(1, 1), (2, 1), (3, 1)]), 80));
    }

    #[test]
    fn two_states_of_a_label_group_and_other_labels_do_not() {
        let same = find_blocks(&button(35), 0);
        let mut other = button(35);
        for y in 14..26 {
            for x in 45..60 {
                other.px[y * 100 + x] = 0; // a different shape in the same place
            }
        }
        let different = find_blocks(&other, 0);
        assert!(same_text(&same[0], &find_blocks(&button(35), 0)[0], 80));
        assert!(!same_text(&same[0], &different[0], 80));
    }

    #[test]
    fn labels_survive_a_round_trip_and_comments_are_skipped() {
        let path = std::env::temp_dir().join(format!("vntl_labels_{}.txt", std::process::id()));
        let labels: IndexMap<String, String> = [("普通 / 普遍".to_string(), "Normal".to_string())].into();
        save_labels(&path, &labels).unwrap();
        assert_eq!(load_labels(&path).unwrap(), labels);
        std::fs::write(&path, "開始 = \n").unwrap();
        assert!(load_labels(&path).unwrap().is_empty(), "a blank is asked for again");
        std::fs::write(&path, [0xff, 0xfe, 0x8b, 0x95]).unwrap();
        assert!(load_labels(&path).is_err(), "and a file saved as UTF-16 is not read as empty");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_bad_reply_costs_only_its_labels_and_nothing_unpaintable_is_kept() {
        let game = std::env::temp_dir().join(format!("vntl_label_job_{}", std::process::id()));
        // the first 20: an unreadable reply, then each alone, the last of them blank
        let mut replies = vec!["Sorry, no numbers here"];
        replies.extend(["1: Ok"; 19]);
        replies.extend(["1: ", "1: スタート\n2: Load\nNote: short"]);
        let (chat, cancel, log) = (Scripted::new(&replies), AtomicBool::new(false), RefCell::new(Vec::new()));
        let report = |e: Event| {
            if let Event::Log(m) = e {
                log.borrow_mut().push(m);
            }
        };
        let mut job = Job::new(&game, &chat, &report, &cancel).unwrap();
        let todo: Vec<(String, usize, String)> = (0..22).map(|i| (format!("k{i}"), 9, String::new())).collect();
        let one_picture = ["p"; 22];
        job.translate_labels("s", &todo, &one_picture).unwrap();
        assert_eq!((job.labels.len(), job.labels["k18"].as_str(), job.labels["k21"].as_str()), (20, "Ok", "Load"));
        assert!(!job.labels.contains_key("k19") && !job.labels.contains_key("k20"));
        assert_eq!(load_labels(&job.labels_path).unwrap(), job.labels, "saved as it goes");
        assert_eq!(log.borrow().as_slice(), ["2 labels in s stay in Japanese; run again to retry them."]);
        assert!(job.translate_labels("s", &todo, &one_picture).is_err(), "the server failing is not a bad reply");
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn labels_go_to_the_model_a_whole_picture_at_a_time_and_a_change_moves_no_other_batch() {
        let names: Vec<String> = (0..40).map(|i| format!("menu/{i}.png")).collect();
        let labels = |counts: &[usize]| -> Vec<&str> { names.iter().zip(counts).flat_map(|(n, c)| std::iter::repeat_n(n.as_str(), *c)).collect() };
        let counts = [3, 1, 2, 5, 4, 1, 1, 2, 3, 30, 2, 1, 2, 2, 3, 1, 1, 4, 2, 2];
        let pictures = labels(&counts);
        let got = batches(&pictures);
        assert_eq!(got.iter().map(|r| r.len()).sum::<usize>(), pictures.len());
        assert!(got.iter().all(|r| r.len() <= 20), "{got:?}");
        assert!(got.windows(2).all(|w| pictures[w[1].start] != pictures[w[1].start - 1] || w[0].len() == 20), "a picture is parted only past 20: {got:?}");
        // one more label on the fourth picture
        let mut more = counts;
        more[3] += 1;
        let after = batches(&labels(&more));
        let named = |b: &[std::ops::Range<usize>], p: &[&str]| -> Vec<Vec<String>> { b.iter().map(|r| p[r.clone()].iter().map(|s| s.to_string()).collect()).collect() };
        let (was, now) = (named(&got, &pictures), named(&after, &labels(&more)));
        let moved = was.iter().filter(|b| !now.contains(b)).count();
        assert!(moved <= 1, "only the batch holding that picture changes: {was:?} {now:?}");
    }


    #[test]
    fn different_labels_on_one_picture_get_english_of_their_own() {
        let game = std::env::temp_dir().join(format!("vntl_apart_{}", std::process::id()));
        // a reply for each run: a question for the look-alikes would use one up and fail the test
        let (chat, cancel) = (Scripted::new(&["1: Pause\n2: Yes", "1: Pause\n2: Stop"]), AtomicBool::new(false));
        let mut job = Job::new(&game, &chat, &|_| {}, &cancel).unwrap();
        let block = |x0: usize, mask: [bool; 4], group| Block { x0, y0: 0, x1: x0 + 4, y1: 1, mask: mask.to_vec(), group };
        let (a, b, c) = ([true, false, true, false], [false, true, false, true], [true, true, false, false]);
        let blocks = vec![block(0, a, 0), block(5, b, 1), block(10, c, 2), block(15, c, 3)];
        let part = Part { entry: 0, sprite: None, at: (0, 0), name: "p".into(), img: Image { w: 20, h: 1, px: vec![CLEAR; 20] }, bg: CLEAR, blocks };
        // はい and はぃ are one button's states, drawn alike: the same English is right for them
        let groups: Vec<Group> = ["一時停止", "停止", "はい", "はぃ"].iter().map(|r| Group { key: String::new(), first: (0, 0), second: None, reads: vec![r.to_string()] }).collect();
        for (r, en) in [("一時停止", "Pause"), ("停止", "Pause"), ("はい", "Yes"), ("はぃ", "Yes")] {
            job.labels.insert(r.into(), en.into());
        }
        let todo: Vec<(String, usize, String)> = groups.iter().map(|g| (g.reads[0].clone(), 9, String::new())).collect();
        let english = |job: &Job| -> Vec<String> { groups.iter().map(|g| job.labels[&g.reads[0]].clone()).collect() };
        job.tell_apart(std::slice::from_ref(&part), &groups, &todo).unwrap();
        assert_eq!(english(&job), ["Pause", "Pause", "Yes", "Yes"], "an answer another label already has is no answer");
        job.tell_apart(std::slice::from_ref(&part), &groups, &todo).unwrap();
        assert_eq!(english(&job), ["Pause", "Stop", "Yes", "Yes"]);
        std::fs::remove_dir_all(&game).unwrap();
    }

    /// A tall picture with a plain or an empty backdrop and one small label on it.
    fn tall_picture(backdrop: u32) -> Image {
        let (w, h) = (300, 300);
        let mut px = vec![backdrop; w * h];
        for y in 100..120 {
            for x in 100..160 {
                px[y * w + x] = 0x00ff_ffff;
            }
        }
        Image { w, h, px }
    }

    #[test]
    fn a_picture_with_a_plain_backdrop_can_hold_labels_but_a_character_can_not() {
        assert!(picture_part(0, "notice".into(), tall_picture(0x0020_2020)).is_some());
        assert!(picture_part(0, "sprite".into(), tall_picture(CLEAR)).is_none());
        assert!(picture_part(0, "badge".into(), Image { w: 100, h: 40, px: button(20).px }).is_some());
    }

    #[test]
    fn pictures_outside_gui_are_repainted_beside_the_translation_and_sprites_stay() {
        let game = std::env::temp_dir().join(format!("vntl_pictures_{}", std::process::id()));
        let images = game.join("game").join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join("notice.png"), write_png(&tall_picture(0x0020_2020)).unwrap()).unwrap();
        std::fs::write(images.join("sprite.png"), write_png(&tall_picture(CLEAR)).unwrap()).unwrap();
        std::fs::write(images.join("sign.webp"), webp(&tall_picture(0x0040_2020))).unwrap();
        std::fs::write(images.join("badge.avif"), include_bytes!("../testdata/button.avif")).unwrap();
        let model = crate::llm::tests::Fake(|p: &str| Ok(if p == *READ_PROMPT { "注意".to_string() } else { "1: Notice".to_string() }));
        let said = translate_renpy_menus(&game, "english", &model, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert!(said.starts_with("Repainted 3 menu images"), "{said}");
        let tl = game.join("game").join("tl").join("english").join("images");
        assert!(tl.join("notice.png").exists() && !tl.join("sprite.png").exists());
        for name in ["sign.webp", "badge.avif"] {
            assert!(read_png(&std::fs::read(tl.join(name)).unwrap()).is_ok(), "a PNG under the name the game asks for: {name}");
        }
        let again = translate_renpy_menus(&game, "english", &model, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert!(again.starts_with("Repainted 3 menu images"), "its own copy under tl/ isn't read as a picture: {again}");
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_label_with_its_own_english_under_it_is_asked_about_and_one_line_is_not() {
        let game = std::env::temp_dir().join(format!("vntl_english_under_{}", std::process::id()));
        let (w, h) = (40, 22);
        // a line of letters, and a smaller one under it
        let mask: Vec<bool> = (0..h).flat_map(|y| (0..w).map(move |x| y < 12 || ((15..21).contains(&y) && x < 20))).collect();
        let img = Image { w, h, px: mask.iter().map(|m| if *m { 0 } else { CLEAR }).collect() };
        let part = Part { entry: 0, sprite: None, at: (0, 0), name: "p".into(), img, bg: CLEAR, blocks: Vec::new() };
        let b = Block { x0: 0, y0: 0, x1: w, y1: h, mask, group: 0 };
        let (chat, cancel) = (Scripted::new(&["ENGLISH", "JAPANESE", "ENGLISH"]), AtomicBool::new(false));
        let job = Job::new(&game, &chat, &|_| {}, &cancel).unwrap();
        assert!(job.english_under(&part, &b).unwrap());
        assert!(!job.english_under(&part, &b).unwrap(), "a smaller line in Japanese is part of the label");
        // an icon running down beside both lines, as ▶ beside 再生 over "play"
        let iconed: Vec<bool> = (0..h).flat_map(|y| (0..w + 10).map(move |x| x < 6 || (x >= 10 && (y < 12 || ((15..21).contains(&y) && x < 30))))).collect();
        let wide = Part { img: Image { w: w + 10, h, px: iconed.iter().map(|m| if *m { 0 } else { CLEAR }).collect() }, ..part };
        assert!(job.english_under(&wide, &Block { x1: w + 10, mask: iconed, ..b.clone() }).unwrap(), "the lines are past the icon");
        let one_line = Block { y1: 12, mask: b.mask[..12 * w].to_vec(), ..b.clone() };
        assert!(!job.english_under(&wide, &one_line).unwrap(), "and one line isn't asked about: no reply is left");
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_label_left_between_readings_takes_the_word_another_label_on_its_picture_settled_on() {
        let game = std::env::temp_dir().join(format!("vntl_settle_{}", std::process::id()));
        let gui = game.join("game").join("gui");
        std::fs::create_dir_all(&gui).unwrap();
        let (w, h) = (240, 40);
        let mut px = vec![0x0010_1010u32; w * h];
        for x0 in [30, 150] {
            (14..26).flat_map(|y| (x0..x0 + 30).map(move |x| y * w + x)).for_each(|i| px[i] = 0x00ff_ffff);
        }
        std::fs::write(gui.join("picker.png"), write_png(&Image { w, h, px }).unwrap()).unwrap();
        // each label is read twice: the first plainly, the second two ways
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let model = crate::llm::tests::Fake(|p: &str| {
            if p == *READ_PROMPT {
                return Ok(["彩度", "彩度", "移園", "彩思"][reads.fetch_add(1, Ordering::Relaxed) % 4].to_string());
            }
            if p.contains("Two readings") {
                return Ok(if p.contains("1: 彩度") { "1" } else { "2" }.to_string());
            }
            Ok("1: Saturation".to_string())
        });
        translate_renpy_menus(&game, "english", &model, &|_| {}, &AtomicBool::new(false)).unwrap();
        let kept: IndexMap<String, Vec<String>> = load_json(&game.join("vntl_work").join("image_reads.json")).unwrap();
        assert_eq!(kept.values().cloned().collect::<Vec<_>>(), [vec!["彩度".to_string()], vec!["彩度".to_string()]]);
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn english_the_story_model_contradicts_is_redone_and_remembered_for_every_game() {
        let game = std::env::temp_dir().join(format!("vntl_story_check_{}", std::process::id()));
        struct Story;
        impl Chat for Story {
            fn chat(&self, _: &str, user: &str) -> R<String> {
                Ok(if user.contains("上機嫌なねこ") { "A cheerful cat" } else { "Back" }.to_string())
            }
            fn complete(&self, _: &str, _: f64) -> R<String> {
                Ok(String::new())
            }
            fn read_image(&self, _: &[u8], _: &str) -> R<String> {
                Ok(String::new())
            }
            fn model(&self) -> &str {
                "story"
            }
        }
        // the words model has the phrase backwards, and sees it once shown the story model's reading
        let words = crate::llm::tests::Fake(|p: &str| {
            Ok(if p.contains("contradict each other") {
                if p.contains("Grumpy") { "CONTRADICT" } else { "AGREE" }
            } else if p.contains("how a translator read") {
                "Cheerful Cat"
            } else {
                "1: Grumpy Cat\n2: Back"
            }.to_string())
        });
        // and the reading model, a third, takes the new English whichever way round it's shown
        let judge = crate::llm::tests::Fake(|p: &str| Ok(if p.contains("1: Cheerful") || p.contains("1: A cheerful") { "1" } else { "2" }.to_string()));
        let pair = crate::llm::Pair { reads: judge, words };
        let vision = crate::llm::Checked { vision: &pair, story: &Story };
        let cancel = AtomicBool::new(false);
        let mut job = Job::new(&game, &vision, &|_| {}, &cancel).unwrap();
        let todo: Vec<(String, usize, String)> = ["上機嫌なねこ", "戻る"].iter().map(|k| (k.to_string(), 20, String::new())).collect();
        job.translate_labels("s", &todo, &["p", "p"]).unwrap();
        job.check_with_story(&todo).unwrap();
        assert_eq!((job.labels["上機嫌なねこ"].as_str(), job.labels["戻る"].as_str()), ("Cheerful Cat", "Back"), "only what contradicts is redone");
        let again = Job::new(&game, &vision, &|_| {}, &cancel).unwrap();
        assert_eq!(again.memory.get("上機嫌なねこ").map(String::as_str), Some("Cheerful Cat"), "the correction is kept");
        assert!(again.remembered(&[("上機嫌ないぬ".to_string(), 20, String::new())]).contains("上機嫌なねこ = Cheerful Cat"), "and shown for a label like it");
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn look_alike_letters_are_tried_and_a_slash_only_after_japanese() {
        assert_eq!(look_alikes("バイ"), ["パイ"]);
        assert!(look_alikes("森のケモ/").contains(&"森のケモノ".to_string()));
        assert!(look_alikes("BGM/SE").is_empty());
        assert!(look_alikes("セーブ").contains(&"セープ".to_string()));
    }

    #[test]
    fn a_look_alike_letter_goes_by_which_makes_real_japanese_and_short_english_is_kept_apart() {
        let game = std::env::temp_dir().join(format!("vntl_look_alike_{}", std::process::id()));
        let gui = game.join("game").join("gui");
        std::fs::create_dir_all(&gui).unwrap();
        // a button too narrow for the full English
        let (w, h) = (110, 40);
        let mut px = vec![0x0010_1010u32; w * h];
        (14..26).flat_map(|y| (20..80).map(move |x| y * w + x)).for_each(|i| px[i] = 0x00ff_ffff);
        std::fs::write(gui.join("title.png"), write_png(&Image { w, h, px }).unwrap()).unwrap();
        let model = crate::llm::tests::Fake(|p: &str| {
            if p == *READ_PROMPT {
                return Ok("バイをやいた".to_string());
            }
            // the words know a pie burns, whichever is shown first
            if p.contains("as a game menu would write it") {
                return Ok(if p.contains("1: パイ") { "1" } else { "2" }.to_string());
            }
            if p.contains("too long for their buttons") {
                return Ok("1: Burnt".to_string());
            }
            Ok("1: The Pie Burned and Turned Black".to_string())
        });
        translate_renpy_menus(&game, "english", &model, &|_| {}, &AtomicBool::new(false)).unwrap();
        let work = game.join("vntl_work");
        let kept: IndexMap<String, Vec<String>> = load_json(&work.join("image_reads.json")).unwrap();
        assert_eq!(kept.values().cloned().collect::<Vec<_>>(), [vec!["パイをやいた".to_string()]]);
        let labels = load_labels(&work.join("image_labels.txt")).unwrap();
        assert_eq!(labels["パイをやいた"], "The Pie Burned and Turned Black", "the full English stays");
        assert_eq!(labels[&format!("パイをやいた{SHORT}")], "Burnt", "beside the short form for where it doesn't fit");
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_reading_has_to_fit_its_block() {
        let block = |w: usize, h: usize| Block { x0: 0, y0: 0, x1: w, y1: h, mask: vec![true; w * h], group: 0 };
        assert!(!fits(&block(14, 24), "システム"), "a note shape read as a word");
        assert!(fits(&block(14, 24), "済"));
        assert!(fits(&block(61, 41), "ページ3"), "a label with English under it is tall for its letters");
        assert!(fits(&block(20, 100), "セーブする"), "a label running down");
        assert!(fits(&block(335, 35), "右クリックでシステム画面に切り替える"));
    }

    #[test]
    fn a_long_reading_a_letter_off_another_on_the_screen_takes_the_real_word() {
        let game = std::env::temp_dir().join(format!("vntl_settle_far_{}", std::process::id()));
        let gui = game.join("game").join("gui");
        std::fs::create_dir_all(&gui).unwrap();
        // a title in the list, and the same title in a pop-up's letters, not alike enough to group
        for (name, wide) in [("a_list.png", 60), ("b_popup.png", 90)] {
            let (w, h) = (200, 40);
            let mut px = vec![0x0010_1010u32; w * h];
            (14..26).flat_map(|y| (40..40 + wide).map(move |x| y * w + x)).for_each(|i| px[i] = 0x00ff_ffff);
            std::fs::write(gui.join(name), write_png(&Image { w, h, px }).unwrap()).unwrap();
        }
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let model = crate::llm::tests::Fake(|p: &str| {
            if p == *READ_PROMPT {
                return Ok(["星降る夜に", "星降る夜に", "星降る夏に", "星降る夏に"][reads.fetch_add(1, Ordering::Relaxed) % 4].to_string());
            }
            // the picture can't tell, the first shown being taken either way round; the words can
            if p.contains("Compare each letter") {
                return Ok("1".to_string());
            }
            if p.contains("as a game menu would write it") {
                return Ok(if p.contains("1: 星降る夜") { "1" } else { "2" }.to_string());
            }
            Ok("1: On a Starry Night".to_string())
        });
        translate_renpy_menus(&game, "english", &model, &|_| {}, &AtomicBool::new(false)).unwrap();
        let kept: IndexMap<String, Vec<String>> = load_json(&game.join("vntl_work").join("image_reads.json")).unwrap();
        assert_eq!(kept.values().cloned().collect::<Vec<_>>(), [vec!["星降る夜に".to_string()], vec!["星降る夜に".to_string()]]);
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_reading_stays_with_its_label_when_another_is_found_before_it() {
        let game = std::env::temp_dir().join(format!("vntl_read_keys_{}", std::process::id()));
        let gui = game.join("game").join("gui");
        std::fs::create_dir_all(&gui).unwrap();
        let bars = |at: &[usize]| {
            let (w, h) = (240, 40);
            let mut px = vec![0x0010_1010u32; w * h];
            for &x0 in at {
                (14..26).flat_map(|y| (x0..x0 + 30).map(move |x| y * w + x)).for_each(|i| px[i] = 0x00ff_ffff);
            }
            write_png(&Image { w, h, px }).unwrap()
        };
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let model = crate::llm::tests::Fake(|p: &str| {
            if p == *READ_PROMPT {
                reads.fetch_add(1, Ordering::Relaxed);
                return Ok("注意".to_string());
            }
            Ok("1: Notice".to_string())
        });
        let run = |at: &[usize]| {
            std::fs::write(gui.join("bar.png"), bars(at)).unwrap();
            translate_renpy_menus(&game, "english", &model, &|_| {}, &AtomicBool::new(false)).unwrap();
            reads.load(Ordering::Relaxed)
        };
        let one = run(&[150]);
        assert_eq!(run(&[30, 150]), 2 * one, "only the new label is read");
        run(&[30]);
        let kept: IndexMap<String, Vec<String>> = load_json(&game.join("vntl_work").join("image_reads.json")).unwrap();
        assert_eq!(kept.len(), 1, "and what no longer has a label goes: {kept:?}");
        std::fs::remove_dir_all(&game).unwrap();
    }

    fn rgba(img: &Image) -> Vec<u8> {
        img.px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255 - (p >> 24) as u8]).collect()
    }

    fn webp(img: &Image) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut out).encode(&rgba(img), img.w as u32, img.h as u32, image::ExtendedColorType::Rgba8).unwrap();
        out
    }

    #[test]
    fn webp_and_jpeg_read_like_png() {
        let img = button(20);
        assert_eq!(read_picture(&webp(&img)).unwrap(), img);
        assert_eq!(read_picture(&write_png(&img).unwrap()).unwrap(), img);
        let plain: Vec<u8> = rgba(&img).as_chunks::<4>().0.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg).encode(&plain, img.w as u32, img.h as u32, image::ExtendedColorType::Rgb8).unwrap();
        let read = read_picture(&jpeg).unwrap();
        assert_eq!((read.w, read.h), (img.w, img.h));
        assert_eq!(picture_height(&jpeg), Some(img.h as u32));
        assert_eq!(picture_height(&webp(&img)), Some(img.h as u32));
        assert!(button_image(&jpeg).is_some());
        assert!(read_picture(b"not a picture").is_err());
    }

    #[test]
    fn a_name_drawn_as_a_picture_is_read_whole_and_spelled_as_listed() {
        let game = std::env::temp_dir().join(format!("vntl_drawn_names_{}", std::process::id()));
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let model = crate::llm::tests::Fake(|_: &str| {
            reads.fetch_add(1, Ordering::Relaxed);
            Ok("ハナ".to_string())
        });
        let (report, cancel) = (|_: Event| {}, AtomicBool::new(false));
        let mut job = Job::new(&game, &model, &report, &cancel).unwrap();
        let files = || vec![("image/name_hana.png".to_string(), write_png(&plate().0).unwrap())];
        let english = |jp: &str| (jp == "ハナ").then(|| "Hana".to_string());
        let mut painted = Vec::new();
        assert_eq!(job.drawn_names(files(), &english, &mut painted).unwrap(), 1);
        assert_ne!(painted[0].1, files()[0].1);
        assert_eq!(job.drawn_names(files(), &|_| None, &mut Vec::new()).unwrap(), 0, "a name with no spelling stays");
        assert_eq!(reads.load(Ordering::Relaxed), 1, "and a picture is only read once");
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_filled_in_patch_takes_its_colours_from_around_it() {
        let (w, h) = (20, 20);
        let mut px: Vec<u32> = (0..w * h).map(|i| if i % w < 10 { 0x0000_00ff } else { 0x00ff_0000 }).collect();
        (5..15).flat_map(|y| (5..15).map(move |x| y * w + x)).for_each(|i| px[i] = 0x00ff_ffff);
        let mut img = Image { w, h, px };
        fill_in(&mut img, (5, 5, 15, 15));
        assert!(img.px.iter().all(|p| *p != 0x00ff_ffff), "no white left");
        assert_eq!(img.px[10 * w + 5] & 0xff, 0xff, "the left of the patch is the blue beside it");
        assert_eq!(img.px[10 * w + 14] >> 16 & 0xff, 0xff, "and the right the red");
        assert_eq!(img.px[0], 0x0000_00ff, "and nothing outside moves");
    }

    #[test]
    fn a_patch_as_big_as_the_picture_stops() {
        let mut img = Image { w: 4, h: 4, px: vec![7; 16] };
        fill_in(&mut img, (0, 0, 4, 4));
        assert_eq!(img.px, vec![7; 16]);
    }

    #[test]
    fn english_replaces_the_row_and_stays_inside_the_picture() {
        let (w, h) = (120, 40);
        let mut px = vec![0x0020_4060u32; w * h];
        (12..28).flat_map(|y| (20..100).map(move |x| y * w + x)).for_each(|i| px[i] = 0x00ff_00ff);
        let mut img = Image { w, h, px };
        let text = "Closed for the day".to_string();
        paint_over(&mut img, &[((20, 12, 100, 28), &text)], &bold_font().unwrap(), None).unwrap();
        assert!(img.px.iter().all(|p| *p & 0xffffff != 0x00ff_00ff), "the Japanese is gone");
        assert!(img.px.iter().any(|p| *p & 0xffffff == 0x00ff_ffff), "and white letters are there");
        assert_eq!(img.px[0], 0x0020_4060);
    }

    #[test]
    fn only_what_fits_a_button_is_decoded() {
        let image = |h: usize| write_png(&Image { w: 1, h, px: vec![0; h] }).unwrap();
        assert!(button_image(&image(200)).is_some());
        assert!(button_image(&image(201)).is_none());
    }

    #[test]
    fn white_text_in_a_black_outline_stays_white() {
        let (w, h) = (100, 30);
        let mut px = vec![CLEAR; w * h];
        for y in 6..22 {
            for x in 10..40 {
                px[y * w + x] = if (12..15).contains(&y) && (12..38).contains(&x) { 0x00ff_ffff } else { 0 };
            }
        }
        let part = scan(0, None, (0, 0), "t".into(), Image { w, h, px }).unwrap();
        let bytes = FONTS.iter().find_map(|p| std::fs::read(p).ok()).unwrap();
        let font = Font::from_bytes(bytes, FontSettings::default()).unwrap();
        let img = repaint(&part, &[Some(&"Hi".to_string())], &font).unwrap();
        assert!(img.px.contains(&0x00ff_ffff), "the outline outnumbers the fill, but the fill is the text's colour");
    }

    #[test]
    fn thin_dark_text_in_a_thick_light_outline_stays_dark() {
        let (w, h, ink) = (100, 30, 0x0040_2010);
        let mut px = vec![CLEAR; w * h];
        for y in 6..22 {
            for x in 10..40 {
                px[y * w + x] = if (13..15).contains(&y) && (12..38).contains(&x) { ink } else { 0x00ff_ffff };
            }
        }
        let part = scan(0, None, (0, 0), "t".into(), Image { w, h, px }).unwrap();
        let bytes = FONTS.iter().find_map(|p| std::fs::read(p).ok()).unwrap();
        let font = Font::from_bytes(bytes, FontSettings::default()).unwrap();
        let img = repaint(&part, &[Some(&"Hi".to_string())], &font).unwrap();
        assert!(img.px.contains(&ink), "a tenth of the ink is the letters' own colour, and the light edge is the outline");
    }

    #[test]
    fn images_an_official_translation_took_over_are_no_longer_ours() {
        let work = std::env::temp_dir().join(format!("vntl_disown_{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        disown_images(&work, &["gui/a.png".to_string()]).unwrap();
        assert!(!work.join("image_files.txt").exists(), "no list, nothing to do");
        std::fs::write(work.join("image_files.txt"), "gui/a.png\ngui/b.png\n").unwrap();
        disown_images(&work, &["gui/a.png".to_string()]).unwrap();
        assert_eq!(std::fs::read_to_string(work.join("image_files.txt")).unwrap(), "gui/b.png\n");
        std::fs::remove_dir_all(&work).unwrap();
    }
}





