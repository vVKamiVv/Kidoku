//! Reads text in images with meikiocr's two small models: one finds the rows on a page, the other
//! reads a row. They were trained on game text and run on the CPU through tract, so they take no GPU
//! memory, give the same answer every time and can't invent a word the way a chat model can. They
//! download once, on request, and whatever they can't read still goes to the image model.

use crate::liar::Image;
use crate::R;
use std::path::PathBuf;
use std::sync::Arc;
use tract_onnx::prelude::*;

/// File, address and exact size of each model, at a fixed version.
const MODELS: [(&str, &str, u64); 2] = [
    (FINDER, "https://huggingface.co/rtr46/meiki.text.detect.v0/resolve/a9cffa4f60cbf72ddb87edf19c6f98a01cd042e6/meiki.text.detect.v0.1.960x544.onnx", 14_503_825),
    (READER, "https://huggingface.co/rtr46/meiki.txt.recognition.v0/resolve/a28cf5874dc2438ebb1c86336be26bcec51e3375/meiki.text.rec.v0.960x32.onnx", 18_593_254),
];
const FINDER: &str = "meiki.text.detect.v0.1.960x544.onnx";
const READER: &str = "meiki.text.rec.v0.960x32.onnx";
const PAGE: (usize, usize) = (960, 544);
const ROW: (usize, usize) = (960, 32);
/// How sure the finder must be of a row, and the reader of a letter: meikiocr's own defaults.
const SURE_OF_ROW: f32 = 0.5;
const SURE_OF_LETTER: f32 = 0.1;
/// Two letters overlapping more than this are two guesses at one letter.
const SAME_LETTER: f32 = 0.3;
/// The reader's habit of reversing these rare words, as meikiocr corrects it.
const SWAPPED: [(&str, &str); 8] =
    [("儡傀", "傀儡"), ("談冗", "冗談"), ("汰淘", "淘汰"), ("沱滂", "滂沱"), ("攣痙", "痙攣"), ("酊酩", "酩酊"), ("麭麺", "麺麭"), ("哭慟", "慟哭")];

fn folder() -> PathBuf {
    crate::app_folder("ocr")
}

fn models() -> Vec<(String, String, u64)> {
    MODELS.iter().map(|(file, url, size)| (file.to_string(), url.to_string(), *size)).collect()
}

pub fn installed() -> bool {
    crate::have_files(&folder(), &models())
}

/// Download both models, calling `progress` with bytes done and the total.
pub fn download(progress: &dyn Fn(u64, u64)) -> R<()> {
    crate::download_files(&folder(), &models(), progress)
}

/// Whether two readings share at least three letters in four, in any order.
fn mostly_same(a: &str, b: &str) -> bool {
    let mut left: Vec<char> = b.chars().collect();
    let shared = a.chars().filter(|c| left.iter().position(|x| x == c).map(|i| left.remove(i)).is_some()).count();
    shared * 4 >= a.chars().count().max(left.len() + shared) * 3
}

/// One letter as the reader saw it, from `from` to `to` across the row.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Seen {
    letter: char,
    from: f32,
    to: f32,
    sure: f32,
}

/// The reader offers overlapping guesses: the surest of each overlap, left to right.
fn surest(mut seen: Vec<Seen>) -> String {
    seen.sort_by(|a, b| b.sure.total_cmp(&a.sure));
    let mut kept: Vec<Seen> = Vec::new();
    for s in seen {
        let clash = |k: &Seen| {
            let shared = s.to.min(k.to) - s.from.max(k.from);
            shared > 0.0 && shared / (s.to - s.from).min(k.to - k.from).max(1e-6) > SAME_LETTER
        };
        if !kept.iter().any(clash) {
            kept.push(s);
        }
    }
    kept.sort_by(|a, b| a.from.total_cmp(&b.from));
    let text: String = kept.iter().map(|s| s.letter).collect();
    SWAPPED.iter().fold(text, |t, (wrong, right)| t.replace(wrong, right))
}

pub type Area = (usize, usize, usize, usize);

type Plan = Arc<TypedSimplePlan>;

fn plan(file: &str, (wide, tall): (usize, usize)) -> TractResult<Plan> {
    // the shapes stored in the file name their batch size, and these runs fix it at one
    let mut model = tract_onnx::onnx().with_ignore_output_shapes(true).with_ignore_value_info(true).model_for_path(folder().join(file))?;
    model.set_input_fact(0, f32::fact([1, 3, tall, wide]).into())?;
    model.set_input_fact(1, i64::fact([1, 2]).into())?;
    model.into_optimized()?.into_runnable()
}

/// The part of `img` from `(x0, y0)` to `(x1, y1)`, stretched to `w`×`h`, at the top left of a
/// black `wide`×`tall` canvas: blue, green, red, 0 to 1, as the models were trained.
fn canvas(img: &Image, (x0, y0, x1, y1): (usize, usize, usize, usize), (w, h): (usize, usize), (wide, tall): (usize, usize)) -> Tensor {
    let mut out = tract_ndarray::Array4::<f32>::zeros((1, 3, tall, wide));
    let (fw, fh) = ((x1 - x0) as f32 / w as f32, (y1 - y0) as f32 / h as f32);
    for y in 0..h {
        let sy = ((y as f32 + 0.5) * fh - 0.5).max(0.0);
        let (t, dy) = (sy as usize, sy.fract());
        let b = (t + 1).min(y1 - y0 - 1);
        for x in 0..w {
            let sx = ((x as f32 + 0.5) * fw - 0.5).max(0.0);
            let (l, dx) = (sx as usize, sx.fract());
            let r = (l + 1).min(x1 - x0 - 1);
            let at = |i: usize, j: usize, c: usize| (img.px[(y0 + j) * img.w + x0 + i] >> (8 * c) & 255) as f32;
            for c in 0..3 {
                let top = at(l, t, c) * (1.0 - dx) + at(r, t, c) * dx;
                let low = at(l, b, c) * (1.0 - dx) + at(r, b, c) * dx;
                out[[0, c, y, x]] = (top * (1.0 - dy) + low * dy) / 255.0;
            }
        }
    }
    out.into()
}

fn size(w: i64, h: i64) -> TractResult<Tensor> {
    Ok(tract_ndarray::Array2::from_shape_vec((1, 2), vec![w, h])?.into())
}

pub struct Reader {
    finder: Plan,
    reader: Plan,
}

impl Reader {
    /// Loads the models, which takes about a second. Err if they aren't installed.
    pub fn open() -> R<Self> {
        if !installed() {
            return Err("the text reader isn't installed: get it from Model Download".into());
        }
        let load = |file, shape| plan(file, shape).map_err(|e| format!("{file}: {e}"));
        Ok(Reader { finder: load(FINDER, PAGE)?, reader: load(READER, ROW)? })
    }

    /// The text of the part of `img` given, which holds one row running left to right.
    fn row(&self, img: &Image, area: (usize, usize, usize, usize)) -> R<String> {
        let (w, h) = (area.2 - area.0, area.3 - area.1);
        let scaled = ((w as f32 * ROW.1 as f32 / h as f32).round() as usize).max(1);
        let (fit_w, fit_h) = if scaled > ROW.0 { (ROW.0, ((ROW.1 * ROW.0) as f32 / scaled as f32).round().max(1.0) as usize) } else { (scaled, ROW.1) };
        let run = || -> TractResult<_> {
            let out = self.reader.run(tvec!(canvas(img, area, (fit_w, fit_h), ROW).into(), size(ROW.0 as i64, ROW.1 as i64)?.into()))?;
            let (letters, boxes, sure) = (out[0].to_plain_array_view::<i32>()?, out[1].to_plain_array_view::<f32>()?, out[2].to_plain_array_view::<f32>()?);
            let across = |v: f32| v.min(fit_w as f32) / fit_w as f32 * w as f32;
            let seen = (0..letters.shape()[1]).filter_map(|i| {
                let (letter, left, right, sure) = (char::from_u32(letters[[0, i]] as u32)?, boxes[[0, i, 0]], boxes[[0, i, 2]], sure[[0, i]]);
                (sure >= SURE_OF_LETTER && left < fit_w as f32).then_some(Seen { letter, from: across(left), to: across(right), sure })
            });
            Ok(surest(seen.collect()))
        };
        run().map_err(|e| format!("text reader: {e}"))
    }

    /// Whether a row reads about the same with a little more around it. On a real game, shapes that
    /// only looked like text kept at most one letter of four from one crop to the next, and text at
    /// least three.
    pub fn agrees(&self, img: &Image, (x0, y0, x1, y1): Area, text: &str) -> R<bool> {
        const MARGIN: usize = 2;
        let wider = (x0.saturating_sub(MARGIN), y0.saturating_sub(MARGIN), (x1 + MARGIN).min(img.w), (y1 + MARGIN).min(img.h));
        Ok(mostly_same(text, &self.row(img, wider)?))
    }

    /// The text of an image holding one row.
    pub fn line(&self, img: &Image) -> R<String> {
        self.row(img, (0, 0, img.w, img.h))
    }

    /// The rows of text across a page, top to bottom.
    pub fn page(&self, img: &Image) -> R<Vec<String>> {
        Ok(self.rows(img)?.into_iter().map(|r| r.1).collect())
    }

    /// The same, with where each row sits: `(left, top, right, bottom)`. shortcut: rows running down
    /// the page are left out; they need meikiocr's third model, for when a screen has vertical text.
    pub fn rows(&self, img: &Image) -> R<Vec<(Area, String)>> {
        let scale = (PAGE.0 as f32 / img.w as f32).min(PAGE.1 as f32 / img.h as f32);
        let fit = ((img.w as f32 * scale) as usize, (img.h as f32 * scale) as usize);
        let run = || -> TractResult<_> {
            let whole = (PAGE.0 as f32 / scale) as i64;
            let out = self.finder.run(tvec!(canvas(img, (0, 0, img.w, img.h), fit, PAGE).into(), size(whole, (PAGE.1 as f32 / scale) as i64)?.into()))?;
            let (boxes, sure) = (out[1].to_plain_array_view::<f32>()?, out[2].to_plain_array_view::<f32>()?);
            let found: Vec<_> = (0..sure.shape()[1]).filter(|&i| sure[[0, i]] > SURE_OF_ROW).map(|i| [0, 1, 2, 3].map(|n| boxes[[0, i, n]])).collect();
            Ok(found)
        };
        let mut found = run().map_err(|e| format!("text reader: {e}"))?;
        found.sort_by(|a, b| a[3].total_cmp(&b[3]));
        let area = |[x0, y0, x1, y1]: [f32; 4]| (x0.max(0.0) as usize, y0.max(0.0) as usize, (x1 as usize).min(img.w), (y1 as usize).min(img.h));
        found.into_iter().map(area).filter(|&(x0, y0, x1, y1)| x1 > x0 && y1 > y0 && y1 - y0 <= x1 - x0).map(|a| Ok((a, self.row(img, a)?))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(letter: char, from: f32, to: f32, sure: f32) -> Seen {
        Seen { letter, from, to, sure }
    }

    #[test]
    fn two_reads_of_text_agree_and_two_reads_of_a_shape_do_not() {
        assert!(mostly_same("注意書き", "注意書き"));
        assert!(mostly_same("注意書き", "注意書さ"));
        assert!(!mostly_same("注意書き", "注意火山"));
        assert!(!mostly_same("注意書き", "川山田林"));
        assert!(!mostly_same("注意", "注意書き火山"), "the second is much longer");
    }

    #[test]
    fn the_surest_of_overlapping_guesses_stays_in_reading_order() {
        let row = vec![seen('く', 12.0, 22.0, 0.9), seen('は', 0.0, 10.0, 0.8), seen('ば', 1.0, 11.0, 0.3), seen('へ', 13.0, 21.0, 0.2)];
        assert_eq!(surest(row), "はく");
    }

    #[test]
    fn letters_that_only_touch_are_both_kept() {
        assert_eq!(surest(vec![seen('一', 0.0, 10.0, 0.9), seen('二', 9.0, 19.0, 0.5), seen('三', 19.0, 29.0, 0.5)]), "一二三");
    }

    #[test]
    fn reversed_words_are_put_right() {
        assert_eq!(surest(vec![seen('談', 0.0, 10.0, 0.9), seen('冗', 10.0, 20.0, 0.9)]), "冗談");
    }

    #[test]
    fn a_canvas_is_the_picture_in_blue_green_red_with_black_around_it() {
        let img = Image { w: 2, h: 1, px: vec![0x00ff_0000, 0x0000_00ff] }; // red, blue
        let t = canvas(&img, (0, 0, 2, 1), (2, 1), (3, 2));
        let v = t.to_plain_array_view::<f32>().unwrap();
        assert_eq!(v.shape(), [1, 3, 2, 3]);
        assert_eq!((v[[0, 2, 0, 0]], v[[0, 0, 0, 0]]), (1.0, 0.0));
        assert_eq!((v[[0, 0, 0, 1]], v[[0, 2, 0, 1]]), (1.0, 0.0));
        assert_eq!(v[[0, 0, 0, 2]] + v[[0, 1, 1, 0]], 0.0);
    }
}
