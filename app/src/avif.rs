//! AVIF pictures, as newer Ren'Py games keep theirs: the AV1 data in the file decoded by rav1d, the
//! Rust port of dav1d, and turned into the RGB the rest of the app works in.

use crate::liar::Image;
use crate::R;
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::*;
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::*;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

/// Whether `data` is an AVIF file: its first box, `ftyp`, names the brand.
pub fn is_avif(data: &[u8]) -> bool {
    let size = data.get(..4).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize);
    data.get(4..8) == Some(b"ftyp") && data.get(8..size.min(data.len())).is_some_and(|b| b.chunks(4).any(|c| c == b"avif" || c == b"avis"))
}

/// The picture's height, from its headers alone.
pub fn height(data: &[u8]) -> Option<u32> {
    let avif = avif_parse::read_avif(&mut std::io::Cursor::new(data)).ok()?;
    Some(avif.primary_item_metadata().ok()?.max_frame_height.get())
}

pub fn read(data: &[u8]) -> R<Image> {
    let avif = avif_parse::read_avif(&mut std::io::Cursor::new(data)).map_err(|e| format!("AVIF: {e:?}"))?;
    let colour = decode(&avif.primary_item)?;
    let alpha = avif.alpha_item.as_ref().map(|a| decode(a)).transpose()?;
    Ok(to_image(&colour, alpha.as_ref(), avif.premultiplied_alpha))
}

/// One decoded AV1 picture: its samples, the chroma planes at their own size.
struct Planes {
    w: usize,
    h: usize,
    bits: u32,
    full_range: bool,
    matrix: Dav1dMatrixCoefficients,
    /// how far chroma is shrunk across and down, as a shift
    sub: (usize, usize),
    mono: bool,
    y: Vec<u16>,
    u: Vec<u16>,
    v: Vec<u16>,
}

fn decode(av1: &[u8]) -> R<Planes> {
    let check = |r: rav1d::Dav1dResult, what: &str| if r.0 == 0 { Ok(()) } else { Err(format!("AVIF: {what} failed ({})", r.0)) };
    // SAFETY: rav1d's C-style calls, each given live values of the types it asks for; the picture is
    // copied out before it is let go, and the context closed on every path
    unsafe {
        let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
        dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
        let mut settings = settings.assume_init();
        // one still picture: no frames in flight, so it comes out as soon as it's in
        settings.n_threads = 1;
        settings.max_frame_delay = 1;
        let mut ctx: Option<Dav1dContext> = None;
        check(dav1d_open(Some(NonNull::from(&mut ctx)), Some(NonNull::from(&mut settings))), "opening the decoder")?;
        let out = (|| {
            let mut data = Dav1dData::default();
            let buf = dav1d_data_create(Some(NonNull::from(&mut data)), av1.len());
            if buf.is_null() {
                return Err("AVIF: out of memory".to_string());
            }
            std::ptr::copy_nonoverlapping(av1.as_ptr(), buf, av1.len());
            let sent = dav1d_send_data(ctx, Some(NonNull::from(&mut data)));
            dav1d_data_unref(Some(NonNull::from(&mut data)));
            check(sent, "reading the picture")?;
            let mut pic = Dav1dPicture::default();
            check(dav1d_get_picture(ctx, Some(NonNull::from(&mut pic))), "decoding the picture")?;
            let planes = copy(&pic);
            dav1d_picture_unref(Some(NonNull::from(&mut pic)));
            planes
        })();
        dav1d_close(Some(NonNull::from(&mut ctx)));
        out
    }
}

/// The samples of a decoded picture, out of rav1d's buffers.
///
/// # Safety
/// `pic` must be a picture rav1d returned and hasn't let go of yet.
unsafe fn copy(pic: &Dav1dPicture) -> R<Planes> {
    let (w, h, bits) = (pic.p.w as usize, pic.p.h as usize, pic.p.bpc as u32);
    let seq = unsafe { pic.seq_hdr.ok_or("AVIF: no sequence header")?.as_ref() };
    let (sub, mono) = match pic.p.layout {
        DAV1D_PIXEL_LAYOUT_I400 => ((0, 0), true),
        DAV1D_PIXEL_LAYOUT_I420 => ((1, 1), false),
        DAV1D_PIXEL_LAYOUT_I422 => ((1, 0), false),
        DAV1D_PIXEL_LAYOUT_I444 => ((0, 0), false),
        other => return Err(format!("AVIF: unknown pixel layout {other}")),
    };
    let plane = |i: usize, pw: usize, ph: usize| -> R<Vec<u16>> {
        let base = pic.data[i].ok_or("AVIF: a plane is missing")?.as_ptr() as *const u8;
        let stride = pic.stride[i.min(1)];
        Ok((0..ph).flat_map(|y| (0..pw).map(move |x| (x, y))).map(|(x, y)| {
            // SAFETY: inside the plane, whose rows are `stride` bytes apart
            unsafe {
                let row = base.offset(y as isize * stride);
                if bits > 8 { (row as *const u16).add(x).read_unaligned() } else { *row.add(x) as u16 }
            }
        }).collect())
    };
    let (cw, ch) = ((w + sub.0) >> sub.0, (h + sub.1) >> sub.1);
    let (u, v) = if mono { (Vec::new(), Vec::new()) } else { (plane(1, cw, ch)?, plane(2, cw, ch)?) };
    Ok(Planes { w, h, bits, full_range: seq.color_range != 0, matrix: seq.mtrx, sub, mono, y: plane(0, w, h)?, u, v })
}

/// RGB from the planes, as the AV1 header says they were made, with the alpha picture's own planes.
fn to_image(c: &Planes, alpha: Option<&Planes>, premultiplied: bool) -> Image {
    // a sample as 0 to 1, or -0.5 to 0.5 for chroma
    let level = |p: &Planes, s: u16, chroma: bool| {
        let (s, k) = (s as f32, (1u32 << (p.bits - 8)) as f32);
        match (p.full_range, chroma) {
            (true, false) => s / ((1u32 << p.bits) - 1) as f32,
            (true, true) => (s - (1u32 << (p.bits - 1)) as f32) / ((1u32 << p.bits) - 1) as f32,
            (false, false) => (s - 16.0 * k) / (219.0 * k),
            (false, true) => (s - 128.0 * k) / (224.0 * k),
        }
    };
    // the matrix's weights for red and blue; unspecified is taken as BT.601, as libavif does
    let (kr, kb) = match c.matrix {
        DAV1D_MC_BT709 => (0.2126, 0.0722),
        DAV1D_MC_BT2020_NCL | DAV1D_MC_BT2020_CL => (0.2627, 0.0593),
        DAV1D_MC_FCC => (0.30, 0.11),
        DAV1D_MC_SMPTE240 => (0.212, 0.087),
        _ => (0.299, 0.114),
    };
    let byte = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u8;
    let px = (0..c.h).flat_map(|y| (0..c.w).map(move |x| (x, y))).map(|(x, y)| {
        let i = y * c.w + x;
        let luma = level(c, c.y[i], false);
        let [mut r, mut g, mut b] = if c.mono {
            [luma; 3]
        } else {
            let ci = (y >> c.sub.1) * ((c.w + c.sub.0) >> c.sub.0) + (x >> c.sub.0);
            if c.matrix == DAV1D_MC_IDENTITY {
                // the planes are green, blue and red themselves
                [level(c, c.v[ci], false), luma, level(c, c.u[ci], false)]
            } else {
                let (cb, cr) = (level(c, c.u[ci], true), level(c, c.v[ci], true));
                let (r, b) = (luma + 2.0 * (1.0 - kr) * cr, luma + 2.0 * (1.0 - kb) * cb);
                [r, (luma - kr * r - kb * b) / (1.0 - kr - kb), b]
            }
        };
        let a = alpha.map_or(1.0, |a| level(a, a.y[(y * a.h / c.h) * a.w + x * a.w / c.w], false).clamp(0.0, 1.0));
        if premultiplied && a > 0.0 {
            [r, g, b] = [r / a, g / a, b / a];
        }
        crate::menus::stored(&[byte(r), byte(g), byte(b), byte(a)])
    }).collect();
    Image { w: c.w, h: c.h, px }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 48×16 button drawn with ImageMagick and saved as AVIF at quality 90: a blue face with round
    /// corners on nothing, a white bar and a red one. 8 bits with chroma at half size, and 10 bits
    /// with chroma at full size.
    const BUTTONS: [(&[u8], u8, bool); 2] =
        [(include_bytes!("../testdata/button.avif"), 8, true), (include_bytes!("../testdata/button10.avif"), 10, false)];

    #[test]
    fn an_avif_picture_reads_with_its_colours_and_its_see_through_corners() {
        assert!(!is_avif(b"\x89PNG\r\n\x1a\n"));
        for (data, bits, halved) in BUTTONS {
            let meta = avif_parse::read_avif(&mut std::io::Cursor::new(data)).unwrap().primary_item_metadata().unwrap();
            assert_eq!((meta.bit_depth, meta.chroma_subsampling.0), (bits, halved), "the fixture is what it says");
            assert!(is_avif(data));
            assert_eq!(height(data), Some(16));
            let img = read(data).unwrap();
            assert_eq!((img.w, img.h), (48, 16));
            let rgba = |x: usize, y: usize| {
                let p = img.px[y * img.w + x];
                [(p >> 16 & 255) as i32, (p >> 8 & 255) as i32, (p & 255) as i32, 255 - (p >> 24) as i32]
            };
            let near = |got: [i32; 4], want: [i32; 4]| got.iter().zip(want).all(|(g, w)| (g - w).abs() <= 24);
            assert!(near(rgba(3, 13), [0x20, 0x30, 0x60, 255]), "{bits} bits, the face: {:?}", rgba(3, 13));
            assert!(near(rgba(20, 8), [255, 255, 255, 255]), "{bits} bits, the white bar: {:?}", rgba(20, 8));
            assert!(near(rgba(42, 7), [0xe0, 0x40, 0x20, 255]), "{bits} bits, the red bar: {:?}", rgba(42, 7));
            assert!(rgba(0, 0)[3] < 64, "{bits} bits, a round corner shows through: {:?}", rgba(0, 0));
        }
    }
}
