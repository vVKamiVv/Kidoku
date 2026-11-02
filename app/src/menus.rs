//! Text painted into a game's interface images: find it, have the local model read and translate
//! it, repaint it in English. Only flat or transparent backgrounds qualify, so artwork is never touched.

use crate::fonts::{self, Sample, SAMPLES};
use crate::liar::{self, Image};
use crate::llm::{parse_numbered, Chat};
use crate::ocr::{Area, Reader};
use crate::pipeline::Event;
use crate::reference::{archive_files, read_game_file};
use crate::renpy::{load_json, save_json, tl_dir};
use crate::{artemis, rpgmaker};
use crate::text::JA;
use crate::{files_under, original, replace_file, R};
use flate2::{write::ZlibEncoder, Compression, Crc};
use fontdue::{Font, FontSettings};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const READ_PROMPT: &str = "This image is one label from a Japanese game menu. Write out exactly the Japanese text it shows, nothing else. If there is no text, or the text is not in Japanese, reply NONE.";
const LABELS_HELP: &str = "# Text read from the game's menu images and its English, one per line: readings = English
# Change the English to correct a label, then run the translation again. Lines starting with # are ignored.
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
    let (bg, n) = count.into_iter().max_by_key(|(_, n)| *n)?;
    (n * 100 >= total * 35).then_some(bg)
}

/// Each row's extent of the largest connected area of background: text lies inside it. A frame's
/// outer edge is often the same colour as the fill, but the frame itself cuts it off.
fn spans(img: &Image, bg: u32) -> Vec<Option<(usize, usize)>> {
    let (w, h) = (img.w, img.h);
    if bg == CLEAR {
        return vec![Some((0, w - 1)); h]; // no frame to protect, and text may touch the image's edge
    }
    let (mut seen, mut inside) = (vec![false; w * h], Vec::new());
    for start in 0..w * h {
        if seen[start] || img.px[start] != bg {
            continue;
        }
        seen[start] = true;
        let (mut stack, mut area) = (vec![start], Vec::new());
        while let Some(i) = stack.pop() {
            area.push(i);
            let (x, y) = (i % w, i / w);
            for (ok, n) in [(x > 0, i.wrapping_sub(1)), (x + 1 < w, i + 1), (y > 0, i.wrapping_sub(w)), (y + 1 < h, i + w)] {
                if ok && !seen[n] && img.px[n] == bg {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        if area.len() > inside.len() {
            inside = area;
        }
    }
    let mut out: Vec<Option<(usize, usize)>> = vec![None; h];
    for i in inside {
        let (x, y) = (i % w, i / w);
        out[y] = Some(out[y].map_or((x, x), |(l, r)| (l.min(x), r.max(x))));
    }
    out
}

fn find_blocks(img: &Image, bg: u32) -> Vec<Block> {
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
                    // on nothing, a shape this big and solid is a button with its text inside, not text
                    let solid = bg == CLEAR && y - top >= 24 && mask.iter().filter(|m| **m).count() * 100 >= mask.len() * 70;
                    if !solid {
                        out.push(Block { x0: start, y0: top, x1: x + 1, y1: y, mask, group: 0 });
                    }
                }
                start = next.copied().unwrap_or(0);
            }
        }
    }
    out
}

/// An image worth reading: it has a background, and something on it.
fn scan(entry: usize, sprite: Option<usize>, at: (i32, i32), name: String, mut img: Image) -> Option<Part> {
    let bg = flat_background(&img)?;
    if bg == CLEAR {
        // transparent pixels can hide any colour; make them all the same
        img.px.iter_mut().filter(|p| **p >> 24 == 255).for_each(|p| *p = CLEAR);
    }
    let blocks = find_blocks(&img, bg);
    (!blocks.is_empty()).then_some(Part { entry, sprite, at, name, img, bg, blocks })
}

/// A button's states have the same letter shapes, whatever their colours.
fn same_text(a: &Block, b: &Block) -> bool {
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
    both * 100 >= either * 80
}

// ---------- reading ----------

/// A block enlarged, on solid paper.
fn block_image(img: &Image, b: &Block, scale: usize) -> Image {
    let (x0, y0) = (b.x0.saturating_sub(6), b.y0.saturating_sub(6));
    let (x1, y1) = ((b.x1 + 6).min(img.w), (b.y1 + 6).min(img.h));
    let (w, h) = ((x1 - x0) * scale, (y1 - y0) * scale);
    // transparent pixels need a backdrop: white under dark text, or the model sees nothing
    let solid: Vec<i32> = (y0..y1).flat_map(|y| (x0..x1).map(move |x| img.px[y * img.w + x])).filter(|p| p >> 24 < 128).map(|p| rgb(p).iter().sum()).collect();
    let paper = if solid.iter().sum::<i32>() < 382 * solid.len() as i32 { 255.0 } else { 0.0 };
    let at = |x: usize, y: usize, c: usize| -> f32 {
        let p = img.px[y * img.w + x];
        let a = (255 - (p >> 24)) as f32 / 255.0;
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

/// A PNG, or the WebP or JPEG a game keeps its pictures in instead.
pub fn read_picture(data: &[u8]) -> R<Image> {
    if data.starts_with(PNG) {
        return read_png(data);
    }
    let img = image::load_from_memory(data).map_err(|e| e.to_string())?.into_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    Ok(Image { w, h, px: img.as_chunks::<4>().0.iter().map(|c| stored(c)).collect() })
}

/// Whether a lower-case file name is a picture `read_picture` takes.
pub(crate) fn is_picture(name: &str) -> bool {
    [".png", ".webp", ".jpg", ".jpeg"].iter().any(|e| name.ends_with(e))
}

fn picture_height(data: &[u8]) -> Option<u32> {
    if data.starts_with(PNG) {
        return Some(u32::from_be_bytes(data.get(20..24)?.try_into().ok()?));
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
    text.trim_end_matches(['？', '?', '！', '!', '。', '|']).to_string()
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
    let text: String = vision.read_image(&write_png(&on(0))?, READ_PROMPT)?.split_whitespace().collect();
    Ok(Some(clean(text)).filter(|t| JA.is_match(t)))
}

fn read_block(vision: &dyn Chat, img: &Image, b: &Block, scale: usize) -> R<String> {
    let text = vision.read_image(&write_png(&block_image(img, b, scale))?, READ_PROMPT)?;
    Ok(text.split_whitespace().collect::<String>())
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
fn draw(img: &mut Image, font: &Font, text: &str, size: f32, anchor: f32, align: f32, cy: f32, fill: [i32; 3], stroke: bool) -> R<()> {
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
                if stroke && v > 100 {
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
                over(&mut img.px[y * img.w + x], [0, 0, 0], e);
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
    let right_edge = |b: &Block| {
        // only labels count: untranslated text that happens to line up is not a column
        let near: Vec<usize> = part.blocks.iter().zip(labels).filter(|(o, l)| l.is_some() && o.x1.abs_diff(b.x1) <= 8).map(|(o, _)| o.x1).collect();
        (near.len() >= 3).then(|| near.into_iter().max().unwrap_or(b.x1))
    };
    let bg = rgb(part.bg);
    let luma = (bg[0] + bg[1] + bg[2]) / 3;
    for (b, label) in part.blocks.iter().zip(labels) {
        let Some(text) = label else { continue };
        let mut ink = Vec::new();
        #[allow(clippy::needless_range_loop)] // y also finds the row in the image and the mask
        for y in b.y0..b.y1 {
            for x in b.x0..b.x1 {
                if b.mask[(y - b.y0) * (b.x1 - b.x0) + x - b.x0] {
                    ink.push(rgb(part.img.px[y * img.w + x]));
                }
            }
            let Some((l, r)) = sp[y] else { continue };
            for x in b.x0.saturating_sub(2).max(l)..=(b.x1 + 1).min(r) {
                img.px[y * img.w + x] = part.bg;
            }
        }
        let share = |test: &dyn Fn(i32) -> bool| ink.iter().filter(|c| test((c[0] + c[1] + c[2]) / 3)).count() * 100 / ink.len().max(1);
        let outlined = (share(&|v| v >= 200) >= 15 && share(&|v| v <= 50) >= 15) || (90..=170).contains(&luma);
        let clear = part.bg == CLEAR;
        // with nothing behind it, the text keeps its own colour; on a button, it contrasts with the fill.
        // the stroke drawn is always black, so an outlined label's own colour is its light ink
        let common = ink.iter().filter(|c| !outlined || (c[0] + c[1] + c[2]) / 3 >= 200).fold(HashMap::new(), |mut n: HashMap<[i32; 3], usize>, c| {
            *n.entry(*c).or_default() += 1;
            n
        });
        let own = common.into_iter().max_by_key(|(_, n)| *n).map_or([0; 3], |(c, _)| c);
        let fill = if clear { own } else if outlined || luma < 128 { [255; 3] } else { [0; 3] };
        let (l, r) = sp[(b.y0 + b.y1) / 2].unwrap_or((0, img.w - 1));
        let alone = part.blocks.len() == 1;
        let right = right_edge(b).filter(|_| !alone);
        let (anchor, align, room) = match right {
            Some(edge) => (edge as f32, 1.0, edge - l - 6),
            None if clear => (b.x0 as f32, 0.0, img.w - b.x0 - 2), // no button to centre on: stay where it was
            None if alone => ((l + r + 1) as f32 / 2.0, 0.5, r - l - 10),
            None => ((b.x0 + b.x1) as f32 / 2.0, 0.5, b.x1 - b.x0 + 10), // something may sit right next to it
        };
        let stroke = if outlined { 4.0 } else { 0.0 };
        let mut size = ((b.y1 - b.y0) as f32 * 0.95).clamp(11.0, 26.0);
        while size > 9.0 && text_width(font, text, size) + stroke > room as f32 {
            size -= 1.0;
        }
        draw(&mut img, font, text, size, anchor, align, (b.y0 + b.y1) as f32 / 2.0, fill, outlined)?;
    }
    Ok(img)
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
    let body: String = labels.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
    crate::write_whole(path, format!("{LABELS_HELP}{body}").as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// What of a model's reply may be painted: its first line, if it has English. A blank would erase
/// the button's text and draw nothing, and the Latin font would draw Japanese as boxes.
fn paintable(reply: &str) -> Option<String> {
    // brackets are the model talking about a label it couldn't make out, and " / " its second guess
    let first = reply.split("\\n").next()?.split(" / ").next()?.trim();
    (!first.is_empty() && !JA.is_match(first) && !first.contains(['(', '[', '（'])).then(|| first.to_string())
}

/// Translate some of a screen's labels together: each gives the others context, and the readings
/// of one label correct each other. None if the reply can't be read.
fn translate_screen(vision: &dyn Chat, keys: &[(String, usize, String)]) -> R<Option<Vec<String>>> {
    let list: Vec<String> = keys.iter().enumerate().map(|(i, (k, n, hint))| format!("{}: {k} ({hint}at most {n} characters)", i + 1)).collect();
    let prompt = format!(
        "These labels come from one screen of a Japanese game's interface, read by OCR, so they may have mistakes. Where a label has \
         several readings they are separated by ' / ' and are the same label: work out the real Japanese word from them and from the \
         other labels. They are buttons and settings the player clicks, so say what each one does: a game's menu, not its story. \
         Translate each into natural, concise English as used on game menus, within its character limit. \
         Reply with exactly {n} lines formatted '<number>: <English>' and nothing else.\n\n{}",
        list.join("\n"),
        n = keys.len()
    );
    Ok(parse_numbered(&vision.chat("", &prompt)?, keys.len()))
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
    draw(&mut out, font, name, size, at as f32, 0.0, (b.y0 + b.y1) as f32 / 2.0, fill, false)?;
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
    report: &'a dyn Fn(Event),
    cancel: &'a AtomicBool,
    reads_path: PathBuf,
    labels_path: PathBuf,
    cached: IndexMap<String, Vec<String>>,
    labels: IndexMap<String, String>,
    font: Font,
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
        Ok(Job { game: game.to_path_buf(), matched: false, vision, reader, reader_only: false, report, cancel, reads_path, labels_path, cached, labels, font })
    }

    /// The text reader's reading, or the image model's if it saw nothing or the label runs down.
    /// English it can read is an answer too: most blocks are that, and the model takes seconds each.
    fn read(&self, img: &Image, b: &Block, scale: usize) -> R<String> {
        if let Some(reader) = self.reader.as_ref().filter(|_| b.x1 - b.x0 >= b.y1 - b.y0) {
            let text = reader.line(&block_image(img, b, scale))?;
            if !text.is_empty() {
                return Ok(text);
            }
            if self.reader_only {
                return Ok(text);
            }
        }
        read_block(self.vision, img, b, scale)
    }

    /// Get English for `todo`, 20 at a time as story lines go: one bad reply can't sink a whole folder
    /// of images. A label left without any keeps its Japanese, and the next run asks again.
    fn translate_labels(&mut self, screen: &str, todo: &[(String, usize, String)]) -> R<()> {
        let stopped = || self.cancel.load(Ordering::Relaxed);
        let mut missed = 0;
        for chunk in todo.chunks(20) {
            if stopped() {
                return Err("Stopped".into());
            }
            let mut english = translate_screen(self.vision, chunk)?.unwrap_or_default();
            if english.is_empty() {
                // one label at a time, as translate_batch retries a line
                for label in chunk {
                    if stopped() {
                        return Err("Stopped".into());
                    }
                    english.push(translate_screen(self.vision, std::slice::from_ref(label))?.unwrap_or_default().concat());
                }
            }
            let good: Vec<(String, String)> = chunk.iter().zip(&english).filter_map(|((k, _, _), e)| Some((k.clone(), paintable(e)?))).collect();
            missed += chunk.len() - good.len();
            self.labels.extend(good);
            save_labels(&self.labels_path, &self.labels)?;
        }
        if missed > 0 {
            (self.report)(Event::Log(format!("{missed} labels in {screen} stay in Japanese; run again to retry them.")));
        }
        Ok(())
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
        for p in 0..parts.len() {
            for b in 0..parts[p].blocks.len() {
                // one image never shows a label twice: its look-alikes are different words
                let found = groups.iter().position(|g| g.first.0 != p && same_text(&parts[g.first.0].blocks[g.first.1], &parts[p].blocks[b]));
                let id = match found {
                    Some(id) => {
                        groups[id].second.get_or_insert((p, b));
                        id
                    }
                    None => {
                        groups.push(Group { key: format!("{screen}|{}#{b}", parts[p].name), first: (p, b), second: None, reads: Vec::new() });
                        groups.len() - 1
                    }
                };
                parts[p].blocks[b].group = id;
            }
        }
        (self.report)(Event::Log(format!("Reading {} labels in {screen}", groups.len())));
        for g in groups.iter_mut() {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Stopped".into());
            }
            if let Some(r) = self.cached.get(&g.key) {
                g.reads = r.clone();
                continue;
            }
            // a second look at the same text: the other state, or a different enlargement
            let (a, b) = g.first;
            let first = self.read(&parts[a].img, &parts[a].blocks[b], 4)?;
            let second = match g.second {
                Some((c, d)) => self.read(&parts[c].img, &parts[c].blocks[d], 4)?,
                None => self.read(&parts[a].img, &parts[a].blocks[b], 2)?,
            };
            // on a picture a guess at a shape won't come out the same twice, as a label will
            let agreed = !self.reader_only || clean(first.clone()) == clean(second.clone());
            g.reads = [first, second].into_iter().map(clean).filter(|t| agreed && JA.is_match(t)).collect();
            g.reads.dedup();
            // a reading that is another plus extra characters is that reading with junk on the end
            let all = g.reads.clone();
            g.reads.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o.as_str())));
            self.cached.insert(g.key.clone(), g.reads.clone());
            save_json(&self.reads_path, &self.cached)?;
        }

        let key = |g: &Group| g.reads.join(" / ");
        let room = |g: &Group| (parts[g.first.0].img.w.min(320) / 11).max(4);
        // what a button is for: the leftmost label on its row is the setting it belongs to
        let hint = |g: &Group| -> String {
            let part = &parts[g.first.0];
            if part.sprite.is_none() {
                return String::new();
            }
            let centre = |p: &Part, b: &Block| p.at.1 + (b.y0 + b.y1) as i32 / 2;
            let block = &part.blocks[g.first.1];
            let (y, x) = (centre(part, block), part.at.0 + block.x0 as i32);
            let row = groups.iter().filter(|o| !o.reads.is_empty()).filter_map(|o| {
                let (other, b) = (&parts[o.first.0], &parts[o.first.0].blocks[o.first.1]);
                let left = other.at.0 + b.x1 as i32;
                ((centre(other, b) - y).abs() <= 12 && left < x).then_some((left, &o.reads[0]))
            });
            row.min_by_key(|(left, _)| *left).map_or(String::new(), |(_, r)| format!("an option of the setting '{r}'; "))
        };
        let todo: Vec<(String, usize, String)> =
            groups.iter().filter(|g| !g.reads.is_empty() && !self.labels.contains_key(&key(g))).map(|g| (key(g), room(g), hint(g))).collect();
        self.translate_labels(screen, &todo)?;
        let samples: Vec<Sample> = groups.iter().filter(|g| !g.reads.is_empty()).take(SAMPLES).map(|g| sample(&parts[g.first.0].blocks[g.first.1], &g.reads[0])).collect();
        if !self.matched && !samples.is_empty() {
            self.font = matched_font(&self.game, "menus", &samples)?;
            self.matched = true;
        }
        let mut changed = Vec::new();
        for (i, p) in parts.iter().enumerate() {
            let text: Vec<Option<&String>> = p.blocks.iter().map(|b| self.labels.get(&key(&groups[b.group]))).collect();
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
/// always PNG, under the old name if that's a WebP or a JPEG: the engine goes by what's in a file.
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
fn paint_over(img: &mut Image, rows: &[(Area, &String)], font: &Font) -> R<()> {
    for ((x0, y0, x1, y1), text) in rows {
        let area = (x0.saturating_sub(3), y0.saturating_sub(3), (x1 + 3).min(img.w), (y1 + 3).min(img.h));
        fill_in(img, area);
        let room = (area.2 - area.0) as f32;
        let mut size = ((area.3 - area.1) as f32 * 0.8).clamp(11.0, 40.0);
        while size > 9.0 && text_width(font, text, size) > room {
            size -= 1.0;
        }
        draw(img, font, text, size, area.0 as f32, 0.0, (area.1 + area.3) as f32 / 2.0, [255; 3], true)?;
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
        let texts: BTreeSet<&String> = wanted.iter().filter_map(|p| seen.get(p)).flatten().map(|r| &r.1).filter(|t| shown(t)).collect();
        let todo: Vec<(String, usize, String)> = texts.iter().filter(|t| !self.labels.contains_key(**t)).map(|t| ((*t).clone(), t.chars().count() * 3, "text inside a picture of the story, such as a sign or a note; ".to_string())).collect();
        self.translate_labels("artwork", &todo)?;
        let mut changed = Vec::new();
        for path in wanted {
            let rows: Vec<(Area, &String)> = seen.get(path).into_iter().flatten().filter_map(|(a, t)| Some((*a, self.labels.get(t)?))).collect();
            if rows.is_empty() {
                continue;
            }
            let Some(mut img) = base(path) else { continue };
            paint_over(&mut img, &rows, &self.font)?;
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
        assert_eq!(paintable("Ri (likely a setting name, unclear)"), None);
        assert_eq!(paintable("Option / Settings").as_deref(), Some("Option"));
        assert_eq!(paintable("Text Speed").as_deref(), Some("Text Speed"));
        assert_eq!(paintable("設定"), None);
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
        assert!(same_text(&dark.blocks[0], &grey.blocks[0]), "the same label in two colours");
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
        let shown = block_image(&part.img, &part.blocks[0], 1);
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
        assert!(!same_text(&a, &b));
        assert!(same_text(&a, &shape(&[(1, 1), (2, 1), (3, 1)])));
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
        assert!(same_text(&same[0], &find_blocks(&button(35), 0)[0]));
        assert!(!same_text(&same[0], &different[0]));
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
        job.translate_labels("s", &todo).unwrap();
        assert_eq!((job.labels.len(), job.labels["k18"].as_str(), job.labels["k21"].as_str()), (20, "Ok", "Load"));
        assert!(!job.labels.contains_key("k19") && !job.labels.contains_key("k20"));
        assert_eq!(load_labels(&job.labels_path).unwrap(), job.labels, "saved as it goes");
        assert_eq!(log.borrow().as_slice(), ["2 labels in s stay in Japanese; run again to retry them."]);
        assert!(job.translate_labels("s", &todo).is_err(), "the server failing is not a bad reply");
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
        let model = crate::llm::tests::Fake(|p: &str| Ok(if p == READ_PROMPT { "注意".to_string() } else { "1: Notice".to_string() }));
        let said = translate_renpy_menus(&game, "english", &model, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert!(said.starts_with("Repainted 2 menu images"), "{said}");
        let tl = game.join("game").join("tl").join("english").join("images");
        assert!(tl.join("notice.png").exists() && !tl.join("sprite.png").exists());
        assert!(read_png(&std::fs::read(tl.join("sign.webp")).unwrap()).is_ok(), "a PNG under the name the game asks for");
        let again = translate_renpy_menus(&game, "english", &model, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert!(again.starts_with("Repainted 2 menu images"), "its own copy under tl/ isn't read as a picture: {again}");
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
        paint_over(&mut img, &[((20, 12, 100, 28), &text)], &bold_font().unwrap()).unwrap();
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
