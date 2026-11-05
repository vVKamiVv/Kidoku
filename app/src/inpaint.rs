//! Filling in where text sat over a picture, with LaMa: a model that continues what's around a hole,
//! patterns and edges too, where the plain fill in menus.rs can only smear the edge colours inwards.
//! This one was trained further on anime and manga pictures (dreMaz/AnimeMangaInpainting), which on a
//! game's title art filled holes 8% closer to what was there than LaMa trained on photos.
//! It runs on the CPU through tract, takes a 512×512 window, and downloads once, on request.

use crate::liar::Image;
use crate::ocr::Area;
use crate::R;
use std::path::PathBuf;
use std::sync::Arc;
use tract_onnx::prelude::*;

/// File, address and exact size, at a fixed version.
const MODEL: (&str, &str, u64) = (
    "lama-manga.onnx",
    "https://huggingface.co/mayocream/lama-manga-onnx/resolve/b55497aadbfcb9740e1ed16f008268d71b4f3f79/lama-manga.onnx",
    207_482_644,
);
/// The photo-trained LaMa 0.1.6 and 0.1.7 downloaded, which the new one replaces.
const OLD_MODEL: &str = "lama_fp32.onnx";
const SIDE: usize = 512;
/// A hole this much wider than the row the reader found: lettering has a dark edge or a glow past
/// it, and what's left the model grows back into letters. 12 cleared a real tutorial screen; 3 didn't.
const MARGIN: usize = 12;
/// A hole wider than this goes in pieces, each with room in its window to see around it.
const PIECE: usize = 448;
/// How far a patch fades into the picture, so its edge doesn't show.
const FADE: usize = 4;

fn folder() -> PathBuf {
    crate::app_folder("inpaint")
}

fn models() -> Vec<(String, String, u64)> {
    vec![(MODEL.0.to_string(), MODEL.1.to_string(), MODEL.2)]
}

pub fn installed() -> bool {
    crate::have_files(&folder(), &models())
}

pub fn download(progress: &dyn Fn(u64, u64)) -> R<()> {
    crate::download_files(&folder(), &models(), progress)?;
    // 208 MB nothing uses any more
    let _ = std::fs::remove_file(folder().join(OLD_MODEL));
    Ok(())
}

/// The rows grown by the margin, within the picture, and cut to fit a window.
fn holes(rows: &[Area], (w, h): (usize, usize)) -> Vec<Area> {
    rows.iter().flat_map(|&(x0, y0, x1, y1)| {
        let (x0, y0, x1, y1) = (x0.saturating_sub(MARGIN), y0.saturating_sub(MARGIN), (x1 + MARGIN).min(w), (y1 + MARGIN).min(h));
        // shortcut: a row taller than a window only fills its top 512 px; game lettering is far smaller
        (x0..x1).step_by(PIECE).map(move |a| (a, y0, (a + PIECE).min(x1), y1.min(y0 + SIDE)))
    }).collect()
}

/// Windows with the holes each one fills: a hole joins the first window that still fits them all.
fn windows(holes: &[Area], (w, h): (usize, usize)) -> Vec<(Area, Vec<Area>)> {
    let mut groups: Vec<(Area, Vec<Area>)> = Vec::new();
    for &hole in holes {
        let joined = |b: Area| (b.0.min(hole.0), b.1.min(hole.1), b.2.max(hole.2), b.3.max(hole.3));
        match groups.iter_mut().find(|(b, _)| { let u = joined(*b); u.2 - u.0 <= SIDE && u.3 - u.1 <= SIDE }) {
            Some((b, members)) => {
                *b = joined(*b);
                members.push(hole);
            }
            None => groups.push((hole, vec![hole])),
        }
    }
    // each window centred on its holes, and inside the picture where the picture is big enough
    let place = |lo: usize, hi: usize, size: usize| ((lo + hi) / 2).saturating_sub(SIDE / 2).min(size.saturating_sub(SIDE));
    groups.into_iter().map(|(b, members)| {
        let (x, y) = (place(b.0, b.2, w), place(b.1, b.3, h));
        ((x, y, x + SIDE, y + SIDE), members)
    }).collect()
}

/// How much of the patch shows at `(x, y)`: all of it in a hole, fading out over FADE pixels.
fn share(holes: &[Area], x: usize, y: usize) -> f32 {
    holes.iter().map(|&(x0, y0, x1, y1)| {
        let dx = x0.saturating_sub(x).max((x + 1).saturating_sub(x1));
        let dy = y0.saturating_sub(y).max((y + 1).saturating_sub(y1));
        1.0 - dx.max(dy) as f32 / (FADE + 1) as f32
    }).fold(0.0, f32::max).max(0.0)
}

pub struct Painter {
    plan: Arc<TypedSimplePlan>,
}

impl Painter {
    /// Loads the model, which takes a few seconds. Err if it isn't installed.
    pub fn open() -> R<Self> {
        if !installed() {
            return Err("the inpainting model isn't installed: get it from Model Download".into());
        }
        let load = || -> TractResult<Arc<TypedSimplePlan>> {
            let mut model = tract_onnx::onnx().model_for_path(folder().join(MODEL.0))?;
            model.set_input_fact(0, f32::fact([1, 3, SIDE, SIDE]).into())?;
            model.set_input_fact(1, f32::fact([1, 1, SIDE, SIDE]).into())?;
            model.into_optimized()?.into_runnable()
        };
        Ok(Painter { plan: load().map_err(|e| format!("inpainting model: {e}"))? })
    }

    /// `img` with `rows` filled from the picture around them, a window at a time.
    pub fn fill(&self, img: &mut Image, rows: &[Area]) -> R<()> {
        let (w, h) = (img.w, img.h);
        let all = holes(rows, (w, h));
        for ((wx, wy, ..), members) in windows(&all, (w, h)) {
            let at = |x: usize, y: usize| img.px[(wy + y).min(h - 1) * w + (wx + x).min(w - 1)];
            let image: Tensor = tract_ndarray::Array4::from_shape_fn((1, 3, SIDE, SIDE), |(_, c, y, x)| (at(x, y) >> (16 - 8 * c) & 255) as f32 / 255.0).into();
            // every hole is masked, or a neighbour's letters in the window get copied into this one
            let mask: Tensor = tract_ndarray::Array4::from_shape_fn((1, 1, SIDE, SIDE), |(_, _, y, x)| share(&all, wx + x, wy + y).floor()).into();
            let out = self.plan.run(tvec!(image.into(), mask.into())).map_err(|e| format!("inpainting model: {e}"))?;
            let out = out[0].to_plain_array_view::<f32>().map_err(|e| format!("inpainting model: {e}"))?;
            let (x0, y0) = (members.iter().map(|m| m.0).min().unwrap_or(0).saturating_sub(FADE), members.iter().map(|m| m.1).min().unwrap_or(0).saturating_sub(FADE));
            let (x1, y1) = (members.iter().map(|m| m.2).max().unwrap_or(0) + FADE, members.iter().map(|m| m.3).max().unwrap_or(0) + FADE);
            for y in y0.max(wy)..y1.min(h).min(wy + SIDE) {
                for x in x0.max(wx)..x1.min(w).min(wx + SIDE) {
                    let s = share(&members, x, y);
                    if s <= 0.0 {
                        continue;
                    }
                    let old = img.px[y * w + x];
                    let mix = |c: usize| {
                        // the model gives 0 to 1
                        let new = (out[[0, c, y - wy, x - wx]] * 255.0).clamp(0.0, 255.0);
                        (new * s + (old >> (16 - 8 * c) & 255) as f32 * (1.0 - s)).round() as u32
                    };
                    img.px[y * w + x] = (old & 0xff00_0000) | mix(0) << 16 | mix(1) << 8 | mix(2);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_grow_past_their_lettering_and_a_wide_one_goes_in_pieces() {
        let got = holes(&[(100, 50, 140, 80), (0, 200, 1000, 230)], (960, 540));
        assert_eq!(got[0], (88, 38, 152, 92), "the margin all round");
        assert_eq!(&got[1..], [(0, 188, 448, 242), (448, 188, 896, 242), (896, 188, 960, 242)], "within the picture, in window-sized pieces");
    }

    #[test]
    fn rows_near_each_other_share_a_window_and_each_fits_its_own() {
        let holes = [(100, 40, 400, 90), (120, 150, 500, 200), (100, 450, 300, 500), (700, 40, 900, 90)];
        let got = windows(&holes, (1230, 707));
        assert_eq!(got.len(), 2, "the first three fit in one window, the far one doesn't join them");
        for ((wx, wy, wx1, wy1), members) in &got {
            assert_eq!((wx1 - wx, wy1 - wy), (SIDE, SIDE));
            assert!(members.iter().all(|m| m.0 >= *wx && m.1 >= *wy && m.2 <= *wx1 && m.3 <= *wy1), "{members:?} in {wx},{wy}");
        }
        assert!(got.iter().all(|(w, _)| w.2 <= 1230 && w.3 <= 707), "inside the picture");
    }

    #[test]
    fn a_patch_fades_out_round_its_hole() {
        let hole = [(10, 10, 20, 20)];
        assert_eq!(share(&hole, 15, 15), 1.0);
        assert!(share(&hole, 21, 15) > 0.0 && share(&hole, 21, 15) < 1.0);
        assert_eq!(share(&hole, 19 + FADE + 1, 15), 0.0);
    }
}
