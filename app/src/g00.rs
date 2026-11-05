//! Pictures of SiglusEngine and RealLive games (.g00): 24-bit BGR, a palette, or cuts of BGRA pieces,
//! each LZ-packed. Each cut is a picture of its own, as a button's states are.

use crate::liar::Image;
use crate::R;

fn half(b: &[u8], p: usize) -> R<usize> {
    b.get(p..p + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize).ok_or_else(|| "truncated picture".into())
}

fn word(b: &[u8], p: usize) -> R<usize> {
    b.get(p..p + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize).ok_or_else(|| "truncated picture".into())
}

/// An LZ stream: after its packed and unpacked sizes, a flag byte for each 8 items, a set bit for
/// `unit` bytes as they are (made `wide` with opaque alpha), a clear one for a 16-bit copy of what came
/// before: 12 bits of distance and 4 of count, both in items, the count from `least`.
fn unpack(b: &[u8], least: usize, unit: usize, wide: usize) -> R<Vec<u8>> {
    let (packed, size) = (word(b, 0)?, word(b, 4)?);
    let end = packed.min(b.len());
    let mut out = Vec::with_capacity(size);
    let mut p = 8;
    while out.len() < size && p < end {
        let flags = b[p];
        p += 1;
        for bit in 0..8 {
            if out.len() >= size || p >= end {
                break;
            }
            if flags >> bit & 1 == 1 {
                out.extend_from_slice(b.get(p..p + unit).ok_or("truncated picture")?);
                out.resize(out.len() + wide - unit, 255);
                p += unit;
            } else {
                let v = half(b, p)?;
                p += 2;
                let (back, count) = ((v >> 4) * wide, ((v & 15) + least) * wide);
                if back == 0 || back > out.len() {
                    return Err("bad picture data".into());
                }
                let from = out.len() - back;
                (0..count).for_each(|i| out.push(out[from + i]));
            }
        }
    }
    if out.len() < size {
        return Err("truncated picture".into());
    }
    out.truncate(size);
    Ok(out)
}

/// `data`, items of `wide` bytes, as `unpack` reads it back: each run of items found again in the
/// last 4095 is copied from there, the longest of the 64 latest places its first items were seen.
fn pack(data: &[u8], least: usize, unit: usize, wide: usize) -> Vec<u8> {
    let items = data.len() / wide;
    let item = |i: usize| &data[i * wide..(i + 1) * wide];
    // where the `least` items from each place were last seen, and before that
    let key = |i: usize| &data[i * wide..(i + least) * wide];
    let mut head: std::collections::HashMap<&[u8], usize> = std::collections::HashMap::new();
    let mut prev = vec![usize::MAX; items];
    let mut out = vec![0; 8];
    let (mut i, mut flag_at, mut bit) = (0, 0, 8);
    while i < items {
        if bit == 8 {
            flag_at = out.len();
            out.push(0);
            bit = 0;
        }
        let (mut best, mut back) = (0, 0);
        if i + least <= items {
            let mut at = head.get(key(i)).copied();
            for _ in 0..64 {
                let Some(j) = at.filter(|&j| i - j < 4096) else { break };
                let mut n = 0;
                while n < least + 15 && i + n < items && item(j + n) == item(i + n) {
                    n += 1;
                }
                if n > best {
                    (best, back) = (n, i - j);
                }
                at = Some(prev[j]).filter(|&p| p != usize::MAX);
            }
        }
        let n = if best >= least {
            out.extend(((back << 4 | (best - least)) as u16).to_le_bytes());
            best
        } else {
            out[flag_at] |= 1 << bit;
            out.extend(&item(i)[..unit]);
            1
        };
        for k in (i..i + n).filter(|k| k + least <= items) {
            if let Some(old) = head.insert(key(k), k) {
                prev[k] = old;
            }
        }
        i += n;
        bit += 1;
    }
    let packed = out.len() as u32;
    out[..4].copy_from_slice(&packed.to_le_bytes());
    out[4..8].copy_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn pixel(b: u8, g: u8, r: u8, a: u8) -> u32 {
    u32::from_le_bytes([b, g, r, 255 - a])
}

const REGIONS: usize = 9; // the type, the size, then how many cuts
const REGION: usize = 24; // left, top, right, bottom (inclusive), and a centre point
const BLOCK: usize = 4 + 0x70; // a cut's head: type, how many pieces, then what we don't need
const PIECE: usize = 10 + 0x52; // a piece's head: where, a flag, its size, then what we don't need

/// The cuts of a type 2 picture: where each sits in the file's table, and its size.
fn regions(b: &[u8]) -> R<Vec<(usize, usize, usize)>> {
    let n = word(b, 5)?;
    (0..n).map(|i| {
        let at = REGIONS + i * REGION;
        let (x0, y0, x1, y1) = (word(b, at)?, word(b, at + 4)?, word(b, at + 8)?, word(b, at + 12)?);
        if x1 < x0 || y1 < y0 || x1 - x0 > 8192 || y1 - y0 > 8192 {
            return Err("bad picture cut".into());
        }
        Ok((at, x1 - x0 + 1, y1 - y0 + 1))
    }).collect()
}

/// Each cut of a picture, as a picture of its own.
pub fn cuts(b: &[u8]) -> R<Vec<Image>> {
    let (w, h) = (half(b, 1)?, half(b, 3)?);
    match b.first() {
        Some(0) => {
            let raw = unpack(b.get(5..).ok_or("truncated picture")?, 1, 3, 4)?;
            Ok(vec![Image { w, h, px: raw.as_chunks::<4>().0.iter().map(|c| pixel(c[0], c[1], c[2], 255)).take(w * h).collect() }])
        }
        Some(1) => {
            let raw = unpack(b.get(5..).ok_or("truncated picture")?, 2, 1, 1)?;
            let colours = half(&raw, 0)?;
            let palette: Vec<u32> = (0..colours).map(|i| raw.get(2 + i * 4..6 + i * 4).map(|c| pixel(c[0], c[1], c[2], c[3])).ok_or_else(|| "truncated picture".to_string())).collect::<R<_>>()?;
            let px = raw.get(2 + colours * 4..).unwrap_or(&[]).iter().take(w * h).map(|&i| palette.get(i as usize).copied().unwrap_or(pixel(0, 0, 0, 0))).collect::<Vec<_>>();
            if px.len() < w * h {
                return Err("truncated picture".into());
            }
            Ok(vec![Image { w, h, px }])
        }
        Some(2) => {
            let regions = regions(b)?;
            let raw = unpack(b.get(REGIONS + regions.len() * REGION..).ok_or("truncated picture")?, 2, 1, 1)?;
            regions.iter().enumerate().map(|(i, &(_, w, h))| {
                let mut img = Image { w, h, px: vec![pixel(0, 0, 0, 0); w * h] };
                let (at, len) = (word(&raw, 4 + i * 8)?, word(&raw, 8 + i * 8)?);
                if len == 0 {
                    return Ok(img);
                }
                let pieces = half(&raw, at + 2)?;
                let mut p = at + BLOCK;
                for _ in 0..pieces {
                    let (x, y, pw, ph) = (half(&raw, p)?, half(&raw, p + 2)?, half(&raw, p + 6)?, half(&raw, p + 8)?);
                    p += PIECE;
                    if x + pw > w || y + ph > h {
                        return Err("picture piece outside its cut".into());
                    }
                    let rows = raw.get(p..p + pw * ph * 4).ok_or("truncated picture")?;
                    for (r, row) in rows.chunks_exact(pw * 4).enumerate() {
                        for (c, q) in row.as_chunks::<4>().0.iter().enumerate() {
                            img.px[(y + r) * w + x + c] = pixel(q[0], q[1], q[2], q[3]);
                        }
                    }
                    p += pw * ph * 4;
                }
                Ok(img)
            }).collect()
        }
        _ => Err("not a picture this app writes".into()),
    }
}

/// Where a cut has something drawn: bands of rows, at most as tall as the game's own pieces run, each
/// as wide as what's drawn in it.
fn bands(img: &Image) -> Vec<(usize, usize, usize, usize)> {
    let drawn = |y: usize| img.px[y * img.w..(y + 1) * img.w].iter().position(|p| p >> 24 != 255);
    let mut out: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut y = 0;
    while y < img.h {
        if drawn(y).is_none() {
            y += 1;
            continue;
        }
        let top = y;
        while y < img.h && y - top < 128 && drawn(y).is_some() {
            y += 1;
        }
        let row = |r: usize| &img.px[r * img.w..(r + 1) * img.w];
        let x0 = (top..y).filter_map(drawn).min().unwrap_or(0);
        let x1 = (top..y).filter_map(|r| row(r).iter().rposition(|p| p >> 24 != 255)).max().unwrap_or(0) + 1;
        out.push((x0, top, x1, y));
    }
    out
}

/// Whether `cuts` can be written back as this kind of picture.
pub fn writable(b: &[u8]) -> bool {
    matches!(b.first(), Some(0 | 2)) && cuts(b).is_ok()
}

/// The picture `b` with its cuts replaced by `new`, each sized as before. A cut of BGRA pieces is
/// written as the game writes it: a piece for each band of rows with something drawn, as wide as what's
/// drawn there, so English wider than the Japanese isn't cut off.
pub fn with_cuts(b: &[u8], new: &[Image]) -> R<Vec<u8>> {
    let bgra = |px: &mut dyn Iterator<Item = &u32>| px.flat_map(|p| {
        let [b, g, r, a] = p.to_le_bytes();
        [b, g, r, 255 - a]
    }).collect::<Vec<u8>>();
    match b.first() {
        Some(0) => {
            let img = new.first().ok_or("no picture")?;
            if img.px.iter().any(|p| p >> 24 != 0) {
                return Err("a picture without transparency can't take it".into());
            }
            Ok([&b[..5], &pack(&bgra(&mut img.px.iter()), 1, 3, 4)].concat())
        }
        Some(2) => {
            let regions = regions(b)?;
            let head = REGIONS + regions.len() * REGION;
            let raw = unpack(&b[head..], 2, 1, 1)?;
            // the heads of the game's own cut and piece, to copy what we don't know
            let first = (0..regions.len()).find(|&i| word(&raw, 8 + i * 8).is_ok_and(|n| n >= BLOCK + PIECE)).ok_or("a picture with nothing in it")?;
            let at = word(&raw, 4 + first * 8)?;
            let (block, piece) = (raw.get(at..at + BLOCK).ok_or("truncated picture")?, raw.get(at + BLOCK..at + BLOCK + PIECE).ok_or("truncated picture")?);
            let mut out = (regions.len() as u32).to_le_bytes().to_vec();
            out.resize(4 + regions.len() * 8, 0);
            for (i, &(_, w, h)) in regions.iter().enumerate() {
                let img = new.get(i).ok_or("a cut is missing")?;
                if (img.w, img.h) != (w, h) {
                    return Err("a cut changed size".into());
                }
                let pieces = bands(img);
                if pieces.is_empty() {
                    continue;
                }
                let start = out.len();
                out.extend(&block[..2]);
                out.extend((pieces.len() as u16).to_le_bytes());
                out.extend(&block[4..]);
                for (x0, y0, x1, y1) in pieces {
                    let px = || (y0..y1).flat_map(move |y| img.px[y * w + x0..y * w + x1].iter());
                    // as the game marks them: 0 for a piece with no transparency, 1 for one with some
                    let clear = px().any(|p| p >> 24 != 0) as u16;
                    for v in [x0 as u16, y0 as u16, clear, (x1 - x0) as u16, (y1 - y0) as u16] {
                        out.extend(v.to_le_bytes());
                    }
                    out.extend(&piece[10..]);
                    out.extend(bgra(&mut px()));
                }
                out[4 + i * 8..8 + i * 8].copy_from_slice(&(start as u32).to_le_bytes());
                let len = (out.len() - start) as u32;
                out[8 + i * 8..12 + i * 8].copy_from_slice(&len.to_le_bytes());
            }
            Ok([&b[..head], &pack(&out, 2, 1, 1)].concat())
        }
        _ => Err("not a picture this app writes".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_reads_back() {
        let data: Vec<u8> = (0..1000u32).flat_map(|i| if i % 50 < 30 { [0, 0, 0, 0] } else { (i * 7).to_le_bytes() }).collect();
        assert_eq!(unpack(&pack(&data, 2, 1, 1), 2, 1, 1).unwrap(), data);
        let bgra: Vec<u8> = (0..1200u32).map(|i| if i % 4 == 3 { 255 } else if i < 600 { 9 } else { (i % 251) as u8 }).collect();
        assert_eq!(unpack(&pack(&bgra, 1, 3, 4), 1, 3, 4).unwrap(), bgra);
        assert!(pack(&data, 2, 1, 1).len() < data.len());
    }

    /// A type 2 picture of 6 by 4 with two cuts, the first of one piece of 2 by 2 at (1, 1), the second empty.
    fn sample() -> Vec<u8> {
        let mut raw = 2u32.to_le_bytes().to_vec();
        raw.extend([20, 0, 0, 0]); // the first cut's place, then its length below
        raw.extend([0; 12]);
        let mut block = vec![1, 0, 1, 0];
        block.resize(BLOCK, 0);
        let mut piece = vec![1, 0, 1, 0, 1, 0, 2, 0, 2, 0];
        piece.resize(PIECE, 0);
        let len = (block.len() + piece.len() + 16) as u32;
        raw[8..12].copy_from_slice(&len.to_le_bytes());
        raw.extend(block);
        raw.extend(piece);
        raw.extend([10, 20, 30, 255, 1, 2, 3, 128, 0, 0, 0, 0, 7, 7, 7, 255]);
        let mut b = vec![2, 6, 0, 4, 0, 2, 0, 0, 0];
        for _ in 0..2 {
            for v in [0u32, 0, 5, 3, 0, 0] {
                b.extend(v.to_le_bytes());
            }
        }
        b.extend(pack(&raw, 2, 1, 1));
        b
    }

    #[test]
    fn cuts_read_and_write_back() {
        let b = sample();
        let mut cuts = cuts(&b).unwrap();
        assert_eq!(cuts.len(), 2);
        assert_eq!((cuts[0].w, cuts[0].h), (6, 4));
        assert_eq!(cuts[0].px[7], pixel(10, 20, 30, 255));
        assert_eq!(cuts[0].px[8], pixel(1, 2, 3, 128));
        assert_eq!(cuts[0].px[0], pixel(0, 0, 0, 0));
        assert!(cuts[1].px.iter().all(|p| p >> 24 == 255));
        // English past where the Japanese was is kept
        cuts[0].px[23] = pixel(255, 255, 255, 255);
        let written = with_cuts(&b, &cuts).unwrap();
        assert_eq!(super::cuts(&written).unwrap(), cuts);
        assert!(writable(&written));
    }
}
