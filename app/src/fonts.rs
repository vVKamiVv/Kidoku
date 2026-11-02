//! Which font on this PC is closest to the lettering drawn into a game's images. The app knows what
//! that lettering says, so it redraws the same Japanese in every font that has kana and compares the
//! shapes. Only Japanese can be compared with Japanese, but a Japanese font's English letters are
//! designed to sit beside its kana, so they match the look too. The answer is saved for the game.

use crate::R;
use fontdue::{Font, FontSettings};
use std::path::{Path, PathBuf};

const FOLDER: &str = "C:/Windows/Fonts";
/// Free Japanese fonts (SIL Open Font License) for lettering the PC's own don't come close to: folder
/// and file in Google Fonts' collection, at a fixed version, and exact size.
const COLLECTION: &str = "https://raw.githubusercontent.com/google/fonts/6c1dbe27dcf7d817d4b62cec033ba77fa8e2a5ec/ofl";
const PACK: [(&str, &str, u64); 20] = [
    ("notosansjp", "NotoSansJP[wght].ttf", 9_589_900),
    ("notoserifjp", "NotoSerifJP[wght].ttf", 13_574_352),
    ("mplusrounded1c", "MPLUSRounded1c-Bold.ttf", 3_542_592),
    ("zenmarugothic", "ZenMaruGothic-Bold.ttf", 3_779_008),
    ("shipporimincho", "ShipporiMincho-Bold.ttf", 8_563_788),
    ("delagothicone", "DelaGothicOne-Regular.ttf", 2_508_848),
    ("reggaeone", "ReggaeOne-Regular.ttf", 2_153_256),
    ("rocknrollone", "RocknRollOne-Regular.ttf", 2_682_824),
    ("yujisyuku", "YujiSyuku-Regular.ttf", 8_430_348),
    ("yujiboku", "YujiBoku-Regular.ttf", 8_525_588),
    ("pottaone", "PottaOne-Regular.ttf", 4_918_516),
    ("hachimarupop", "HachiMaruPop-Regular.ttf", 4_385_624),
    ("kaiseidecol", "KaiseiDecol-Bold.ttf", 4_286_896),
    ("kleeone", "KleeOne-SemiBold.ttf", 8_905_128),
    ("zenantique", "ZenAntique-Regular.ttf", 5_507_920),
    ("zenkurenaido", "ZenKurenaido-Regular.ttf", 4_303_112),
    ("rampartone", "RampartOne-Regular.ttf", 3_722_352),
    ("mochiypopone", "MochiyPopOne-Regular.ttf", 5_163_948),
    ("yomogi", "Yomogi-Regular.ttf", 4_045_904),
    ("yuseimagic", "YuseiMagic-Regular.ttf", 3_134_968),
];
/// Faces to try in a font collection: MS Gothic's closest cousin is its third.
const FACES: u32 = 3;
/// Enough lettering to tell fonts apart. Each more is about a second per font.
pub const SAMPLES: usize = 8;

/// Lettering from a game: the ink inside its bounds, row by row, and what it says.
pub struct Sample {
    pub w: usize,
    pub h: usize,
    pub ink: Vec<bool>,
    pub text: String,
}

/// `text` as ink at `size`, with its letters spread so it's `wide` across, as a game's plates are.
fn drawn(font: &Font, text: &str, size: f32, wide: usize) -> Option<Sample> {
    let glyphs: Vec<_> = text.chars().map(|c| font.rasterize(c, size)).collect();
    let natural: f32 = glyphs.iter().map(|g| g.0.advance_width).sum();
    let spare = (wide as f32 - natural) / (glyphs.len().max(2) - 1) as f32;
    let (cw, ch, base) = (wide + size as usize * 3, size as usize * 3, size as i32 * 2);
    let mut grid = vec![false; cw * ch];
    let mut pen = size;
    for (m, bits) in &glyphs {
        for gy in 0..m.height {
            for gx in 0..m.width {
                let (x, y) = (pen as i32 + m.xmin + gx as i32, base - m.ymin - m.height as i32 + gy as i32);
                if bits[gy * m.width + gx] > 127 && x >= 0 && y >= 0 && (x as usize) < cw && (y as usize) < ch {
                    grid[y as usize * cw + x as usize] = true;
                }
            }
        }
        pen += m.advance_width + spare;
    }
    let cols: Vec<usize> = (0..cw).filter(|x| (0..ch).any(|y| grid[y * cw + x])).collect();
    let rows: Vec<usize> = (0..ch).filter(|y| (0..cw).any(|x| grid[y * cw + x])).collect();
    let (x0, x1, y0, y1) = (*cols.first()?, *cols.last()? + 1, *rows.first()?, *rows.last()? + 1);
    let ink = (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).map(|(x, y)| grid[y * cw + x]).collect();
    Some(Sample { w: x1 - x0, h: y1 - y0, ink, text: String::new() })
}

/// How much two shapes overlap, centred on each other: 1 is the same shape.
fn overlap(a: &Sample, b: &Sample) -> f32 {
    let (w, h) = (a.w.max(b.w), a.h.max(b.h));
    let at = |s: &Sample, x: usize, y: usize| {
        let (ox, oy) = ((w - s.w) / 2, (h - s.h) / 2);
        x >= ox && y >= oy && x - ox < s.w && y - oy < s.h && s.ink[(y - oy) * s.w + x - ox]
    };
    let (mut both, mut either) = (0, 0);
    for (x, y) in (0..h).flat_map(|y| (0..w).map(move |x| (x, y))) {
        let (p, q) = (at(a, x, y), at(b, x, y));
        both += (p && q) as usize;
        either += (p || q) as usize;
    }
    both as f32 / either.max(1) as f32
}

fn score(font: &Font, samples: &[Sample]) -> f32 {
    samples.iter().map(|s| {
        let sizes = s.h * 8 / 10..=s.h * 16 / 10;
        sizes.filter_map(|size| drawn(font, &s.text, size as f32, s.w)).map(|d| overlap(s, &d)).fold(0.0, f32::max)
    }).sum()
}

fn pack_folder() -> PathBuf {
    crate::app_folder("fonts")
}

fn pack() -> Vec<(String, String, u64)> {
    PACK.iter().map(|(folder, file, size)| (file.to_string(), format!("{COLLECTION}/{folder}/{}", file.replace('[', "%5B").replace(']', "%5D")), *size)).collect()
}

pub fn installed() -> bool {
    crate::have_files(&pack_folder(), &pack())
}

pub fn download(progress: &dyn Fn(u64, u64)) -> R<()> {
    crate::download_files(&pack_folder(), &pack(), progress)
}

/// The fonts of Windows, those installed for this user alone, and the pack.
fn font_folders() -> [PathBuf; 3] {
    let own = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Microsoft").join("Windows").join("Fonts");
    [PathBuf::from(FOLDER), own, pack_folder()]
}

/// Whether the pack arrived after a match was saved, so that it wasn't among the choices.
fn pack_is_newer(saved: &Path) -> bool {
    let changed = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    matches!((changed(saved), changed(&pack_folder())), (Some(match_made), Some(pack)) if pack > match_made)
}

fn face(path: &str, index: u32) -> Option<Font> {
    Font::from_bytes(std::fs::read(path).ok()?, FontSettings { collection_index: index, ..Default::default() }).ok()
}

/// The saved font for this kind of lettering, or the best match, saved for next time. None if
/// there's nothing to compare, or no font here has kana.
pub fn matched(game: &Path, kind: &str, samples: &[Sample]) -> Option<Font> {
    let saved = game.join("vntl_work").join(format!("font_{kind}.txt"));
    let known = std::fs::read_to_string(&saved).ok();
    if let Some(font) = known.as_deref().and_then(|t| t.trim().rsplit_once('#')).and_then(|(path, index)| face(path, index.parse().ok()?)).filter(|_| samples.is_empty() || !pack_is_newer(&saved)) {
        return Some(font);
    }
    if samples.is_empty() {
        return None;
    }
    let mut best: Option<(f32, String, Font)> = None;
    let files = font_folders().into_iter().flat_map(|f| std::fs::read_dir(f).into_iter().flatten().flatten());
    for path in files.map(|e| e.path().to_string_lossy().replace('\\', "/")) {
        let low = path.to_lowercase();
        if !(low.ends_with(".ttf") || low.ends_with(".ttc") || low.ends_with(".otf")) {
            continue;
        }
        for index in 0..FACES {
            let Some(font) = face(&path, index) else { break };
            if font.lookup_glyph_index('ア') == 0 || font.lookup_glyph_index('a') == 0 {
                break;
            }
            let points = score(&font, samples);
            if best.as_ref().is_none_or(|b| points > b.0) {
                best = Some((points, format!("{path}#{index}"), font));
            }
        }
    }
    let (_, name, font) = best?;
    let _ = std::fs::write(&saved, name); // losing it only means working it out again
    Some(font)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font(path: &str) -> Font {
        face(path, 0).unwrap()
    }

    #[test]
    fn a_shape_matches_itself_and_not_another() {
        let f = font("C:/Windows/Fonts/msgothic.ttc");
        let a = drawn(&f, "アイウ", 24.0, 80).unwrap();
        let b = drawn(&f, "カキク", 24.0, 80).unwrap();
        assert_eq!(overlap(&a, &a), 1.0);
        assert!(overlap(&a, &b) < 0.6);
    }

    #[test]
    fn letters_spread_to_the_width_of_the_lettering() {
        let wide = drawn(&font("C:/Windows/Fonts/msgothic.ttc"), "アイウ", 24.0, 120).unwrap();
        assert!(wide.w.abs_diff(120) <= 6, "{}", wide.w);
    }

    #[test]
    fn lettering_scores_best_in_its_own_font() {
        let (thin, bold) = (font("C:/Windows/Fonts/msgothic.ttc"), font("C:/Windows/Fonts/YuGothB.ttc"));
        let Sample { w, h, ink, .. } = drawn(&thin, "アイウエオ", 24.0, 130).unwrap();
        let samples = [Sample { w, h, ink, text: "アイウエオ".into() }];
        assert!(score(&thin, &samples) > score(&bold, &samples));
    }

    #[test]
    fn the_pack_asks_for_each_file_by_a_proper_address() {
        let files = pack();
        assert_eq!(files[0].1, format!("{COLLECTION}/notosansjp/NotoSansJP%5Bwght%5D.ttf"));
        assert!(files.iter().all(|(file, url, _)| !url.contains(['[', ']']) && !file.contains('/')));
    }

    #[test]
    fn nothing_to_compare_gives_no_font() {
        assert!(matched(Path::new("no such game"), "plates", &[]).is_none());
    }
}
