//! BGI's compressed pictures, `CompressedBG___`. Version 1 is lossless (a byte filter, zero runs and a
//! Huffman code); version 2 is JPEG-like. Both are read; pictures are written back as version 1, which
//! every BGI that reads version 2 also reads.
//!
//! Decoding follows arc_unpacker's `dec/bgi/cbg` (Copyright (C) 2016 rr-, GPL-3.0-or-later).

const MAGIC: &[u8] = b"CompressedBG___\0";
const HEAD: usize = 0x30;

fn half(b: &[u8], p: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?) as usize)
}

fn word(b: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(p..p + 4)?.try_into().ok()?))
}

/// The running key of the encrypted part, as in BGI's other formats.
fn next_key(key: &mut u32) -> u8 {
    let a = 0x4E35u32.wrapping_mul(*key & 0xFFFF);
    let b = 0x4E35u32.wrapping_mul(*key >> 16);
    let t = 0x15Au32.wrapping_mul(*key).wrapping_add(b).wrapping_add(a >> 16);
    *key = (t << 16).wrapping_add(a & 0xFFFF).wrapping_add(1);
    t as u8
}

/// The encrypted part after the header: the key, its size, a sum and xor of the plain bytes.
fn decrypted(b: &[u8]) -> Option<(Vec<u8>, usize)> {
    let (mut key, size) = (word(b, 0x24)?, word(b, 0x28)? as usize);
    let plain: Vec<u8> = b.get(HEAD..HEAD + size)?.iter().map(|&c| c.wrapping_sub(next_key(&mut key))).collect();
    let sum = plain.iter().fold(0u8, |s, &c| s.wrapping_add(c));
    let xor = plain.iter().fold(0u8, |s, &c| s ^ c);
    (sum == b[0x2C] && xor == b[0x2D]).then_some((plain, HEAD + size))
}

fn varint(b: &[u8], p: &mut usize) -> Option<usize> {
    let (mut v, mut shift) = (0usize, 0);
    loop {
        let c = *b.get(*p)?;
        *p += 1;
        v |= ((c & 0x7F) as usize).checked_shl(shift)?;
        shift += 7;
        if c & 0x80 == 0 {
            return Some(v);
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: usize) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A Huffman tree as the engine builds it: leaves first, then each parent joins the two lightest
/// nodes left, the first found on a tie. `greedy` (version 2) takes the first live node as the left
/// child before looking for a lighter one.
struct Tree {
    leaves: usize,
    kids: Vec<[usize; 2]>,
}

const NONE: usize = usize::MAX;

impl Tree {
    fn build(freq: &[usize], greedy: bool) -> Tree {
        let leaves = freq.len();
        let mut weight: Vec<usize> = freq.to_vec();
        let mut live: Vec<bool> = freq.iter().map(|&f| f > 0).collect();
        let mut kids: Vec<[usize; 2]> = (0..leaves).map(|i| [i, i]).collect();
        let total: usize = freq.iter().sum();
        for _ in 0..leaves {
            let mut pair = [NONE; 2];
            let mut sum = 0;
            for j in 0..2 {
                let mut min = u32::MAX as usize;
                let mut from = 0;
                if greedy {
                    if let Some(first) = live.iter().position(|&l| l) {
                        pair[j] = first;
                        min = weight[first];
                    }
                    from = j + 1;
                }
                for k in from..live.len() {
                    if live[k] && weight[k] < min {
                        min = weight[k];
                        pair[j] = k;
                    }
                }
                if pair[j] != NONE {
                    live[pair[j]] = false;
                    sum += weight[pair[j]];
                }
            }
            kids.push(pair);
            weight.push(sum);
            live.push(true);
            if sum >= total {
                break;
            }
        }
        Tree { leaves, kids }
    }

    fn leaf(&self, bits: &mut Bits) -> Option<usize> {
        let mut node = self.kids.len() - 1;
        while node >= self.leaves {
            node = *self.kids.get(node)?.get(bits.take(1)? as usize)?;
            if node == NONE {
                return None;
            }
        }
        Some(node)
    }

    /// Each leaf's code as (bits, length), read from the root down.
    fn codes(&self) -> Vec<(u64, u32)> {
        let mut out = vec![(0, 0); self.leaves];
        let mut todo = vec![(self.kids.len() - 1, 0u64, 0u32)];
        while let Some((node, code, len)) = todo.pop() {
            if node < self.leaves {
                out[node] = (code, len);
                continue;
            }
            for (bit, &kid) in self.kids[node].iter().enumerate() {
                if kid != NONE {
                    todo.push((kid, code << 1 | bit as u64, len + 1));
                }
            }
        }
        out
    }
}

/// Bits from the most significant down.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn take(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.b.get(self.pos / 8)?;
            v = v << 1 | (byte >> (7 - self.pos % 8) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }
}

/// A CBG picture as (width, height, bits per pixel, BGRA rows from the top with alpha 255 opaque).
/// Grey (8-bit) pictures come back as BGRA too.
pub fn decode(b: &[u8]) -> Option<(usize, usize, usize, Vec<u8>)> {
    if !b.starts_with(MAGIC) || b.len() < HEAD {
        return None;
    }
    let (w, h, bpp) = (half(b, 0x10)?, half(b, 0x12)?, word(b, 0x14)? as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let px = if half(b, 0x2E)? == 2 { decode2(b, w, h, bpp)? } else { decode1(b, w, h, bpp)? };
    Some((w, h, bpp, px))
}

fn decode1(b: &[u8], w: usize, h: usize, bpp: usize) -> Option<Vec<u8>> {
    let step = bpp / 8;
    if !matches!(step, 1 | 3 | 4) {
        return None;
    }
    let packed = word(b, 0x20)? as usize;
    let (table, start) = decrypted(b)?;
    let mut p = 0;
    let freq = (0..256).map(|_| varint(&table, &mut p)).collect::<Option<Vec<_>>>()?;
    let tree = Tree::build(&freq, false);
    let mut bits = Bits { b: &b[start..], pos: 0 };
    let runs = (0..packed).map(|_| tree.leaf(&mut bits).map(|v| v as u8)).collect::<Option<Vec<u8>>>()?;
    // runs of bytes as they are and runs of zeros, taking turns
    let size = w * h * step;
    let mut data = Vec::with_capacity(size);
    let (mut p, mut zeros) = (0, false);
    while p < runs.len() && data.len() < size {
        let n = varint(&runs, &mut p)?.min(size - data.len());
        if zeros {
            data.resize(data.len() + n, 0);
        } else {
            data.extend(runs.get(p..p + n)?);
            p += n;
        }
        zeros = !zeros;
    }
    data.resize(size, 0);
    // each byte was stored less the one to its left, above, or the average of the two
    let row = w * step;
    for i in step..size {
        let pred = if i < row {
            data[i - step]
        } else if i % row < step {
            data[i - row]
        } else {
            ((data[i - step] as u16 + data[i - row] as u16) >> 1) as u8
        };
        data[i] = data[i].wrapping_add(pred);
    }
    Some(data.chunks_exact(step).flat_map(|c| match step {
        1 => [c[0], c[0], c[0], 255],
        3 => [c[0], c[1], c[2], 255],
        _ => [c[0], c[1], c[2], c[3]],
    }).collect())
}

/// `w`×`h` pixels of BGRA (alpha 255 opaque) as a version 1 CBG of `bpp` 24 or 32.
pub fn encode(w: usize, h: usize, bpp: usize, bgra: &[u8]) -> Vec<u8> {
    let step = bpp / 8;
    let mut data: Vec<u8> = bgra.as_chunks::<4>().0.iter().flat_map(|c| c[..step].to_vec()).collect();
    let row = w * step;
    for i in (step..data.len()).rev() {
        let pred = if i < row {
            data[i - step]
        } else if i % row < step {
            data[i - row]
        } else {
            ((data[i - step] as u16 + data[i - row] as u16) >> 1) as u8
        };
        data[i] = data[i].wrapping_sub(pred);
    }
    let mut runs = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let start = i;
        while i < data.len() && data[i] != 0 {
            i += 1;
        }
        put_varint(&mut runs, i - start);
        runs.extend(&data[start..i]);
        let zeros = i;
        while i < data.len() && data[i] == 0 {
            i += 1;
        }
        put_varint(&mut runs, i - zeros);
    }
    let mut freq = vec![0usize; 256];
    runs.iter().for_each(|&c| freq[c as usize] += 1);
    let codes = Tree::build(&freq, false).codes();
    let (mut bits, mut acc, mut held) = (Vec::new(), 0u64, 0u32);
    for &c in &runs {
        let (code, len) = codes[c as usize];
        acc = acc << len | code;
        held += len;
        while held >= 8 {
            held -= 8;
            bits.push((acc >> held) as u8);
        }
    }
    if held > 0 {
        bits.push((acc << (8 - held)) as u8);
    }
    let mut table = Vec::new();
    freq.iter().for_each(|&f| put_varint(&mut table, f));
    let sum = table.iter().fold(0u8, |s, &c| s.wrapping_add(c));
    let xor = table.iter().fold(0u8, |s, &c| s ^ c);
    let mut key = 0x5EED_CB61u32;
    let mut out = MAGIC.to_vec();
    out.extend((w as u16).to_le_bytes());
    out.extend((h as u16).to_le_bytes());
    out.extend((bpp as u32).to_le_bytes());
    out.extend([0; 8]);
    out.extend((runs.len() as u32).to_le_bytes());
    out.extend(key.to_le_bytes());
    out.extend((table.len() as u32).to_le_bytes());
    out.extend([sum, xor]);
    out.extend(1u16.to_le_bytes());
    out.extend(table.iter().map(|&c| c.wrapping_add(next_key(&mut key))));
    out.extend(bits);
    out
}

// ---------- version 2 ----------

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// the engine's own table, digits as it has them
#[allow(clippy::approx_constant)]
const SCALE: [f32; 64] = [
    1.0, 1.38704, 1.30656, 1.17588, 1.0, 0.785695, 0.541196, 0.275899, 1.38704, 1.92388, 1.81225, 1.63099, 1.38704, 1.08979,
    0.750661, 0.382683, 1.30656, 1.81225, 1.70711, 1.53636, 1.30656, 1.02656, 0.707107, 0.36048, 1.17588, 1.63099, 1.53636,
    1.38268, 1.17588, 0.92388, 0.636379, 0.324423, 1.0, 1.38704, 1.30656, 1.17588, 1.0, 0.785695, 0.541196, 0.275899, 0.785695,
    1.08979, 1.02656, 0.92388, 0.785695, 0.617317, 0.425215, 0.216773, 0.541196, 0.750661, 0.707107, 0.636379, 0.541196,
    0.425215, 0.292893, 0.149316, 0.275899, 0.382683, 0.36048, 0.324423, 0.275899, 0.216773, 0.149316, 0.0761205,
];

fn ftoi(v: f32) -> f32 {
    let a = 0x80 + ((v as i32) >> 3);
    (if a < 0 { 0 } else if a < 0xFF { a } else if a < 0x180 { 0xFF } else { 0 }) as f32
}

/// The inverse DCT of one 8×8 block of coefficients, as the engine computes it in floats.
fn idct(coef: &[u16], mul: &[f32; 64]) -> [f32; 64] {
    let input: Vec<f32> = coef[..64].iter().zip(mul).map(|(&c, &m)| c as i16 as f32 * m).collect();
    let mut tp = [0f32; 64];
    for i in 0..8 {
        if (1..8).all(|r| coef[r * 8 + i] == 0) {
            for r in 0..8 {
                tp[r * 8 + i] = input[i];
            }
            continue;
        }
        let at = |r: usize| input[r * 8 + i];
        let (t10, t11) = (at(0) + at(4), at(0) - at(4));
        let t13 = at(2) + at(6);
        let t12 = (at(2) - at(6)) * std::f32::consts::SQRT_2 - t13;
        let (t0, t3, t1, t2) = (t10 + t13, t10 - t13, t11 + t12, t11 - t12);
        let (z13, z10, z11, z12) = (at(5) + at(3), at(5) - at(3), at(1) + at(7), at(1) - at(7));
        let t7 = z11 + z13;
        let t11b = (z11 - z13) * std::f32::consts::SQRT_2;
        let z5 = (z10 + z12) * 1.847_759;
        let t10b = z12 * 1.082_392_2 - z5;
        let t12b = z10 * -2.613_126 + z5;
        let t6 = t12b - t7;
        let t5 = t11b - t6;
        let t4 = t10b + t5;
        tp[i] = t0 + t7;
        tp[56 + i] = t0 - t7;
        tp[8 + i] = t1 + t6;
        tp[48 + i] = t1 - t6;
        tp[16 + i] = t2 + t5;
        tp[40 + i] = t2 - t5;
        tp[32 + i] = t3 + t4;
        tp[24 + i] = t3 - t4;
    }
    let mut out = [0f32; 64];
    for i in 0..8 {
        let r = &tp[i * 8..i * 8 + 8];
        let (t10, t11) = (r[0] + r[4], r[0] - r[4]);
        let t13 = r[2] + r[6];
        let t12 = (r[2] - r[6]) * std::f32::consts::SQRT_2 - t13;
        let (t0, t3, t1, t2) = (t10 + t13, t10 - t13, t11 + t12, t11 - t12);
        let (z13, z10, z11, z12) = (r[5] + r[3], r[5] - r[3], r[1] + r[7], r[1] - r[7]);
        let t7 = z11 + z13;
        let t11b = (z11 - z13) * std::f32::consts::SQRT_2;
        let z5 = (z10 + z12) * 1.847_759;
        let t10b = z5 - z12 * 1.082_392_2;
        let t12b = z5 - z10 * 2.613_126;
        let t6 = t12b - t7;
        let t5 = t11b - t6;
        let t4 = t10b - t5;
        let o = &mut out[i * 8..i * 8 + 8];
        o[0] = ftoi(t0 + t7);
        o[7] = ftoi(t0 - t7);
        o[1] = ftoi(t1 + t6);
        o[6] = ftoi(t1 - t6);
        o[2] = ftoi(t2 + t5);
        o[5] = ftoi(t2 - t5);
        o[3] = ftoi(t3 + t4);
        o[4] = ftoi(t3 - t4);
    }
    out
}

/// A JPEG-style value of `size` bits: a clear top bit means negative.
fn signed(v: u32, size: usize) -> i32 {
    if v & (1 << (size - 1)) == 0 { (u32::MAX << size | (v + 1)) as i32 } else { v as i32 }
}

/// One band of 8 rows: the DC values of every block, then their AC values.
fn coefficients(count: usize, data: &[u8], dc: &Tree, ac: &Tree) -> Option<Vec<u16>> {
    let mut out = vec![0u16; count];
    let mut bits = Bits { b: data, pos: 0 };
    let mut value = 0i32;
    for i in (0..count).step_by(64) {
        let size = dc.leaf(&mut bits)?;
        if size > 0 {
            value = value.wrapping_add(signed(bits.take(size)?, size));
        }
        out[i] = value as u16;
    }
    bits.pos = bits.pos.div_ceil(8) * 8;
    for i in (0..count).step_by(64) {
        let mut index = 1;
        while index < 64 {
            let mut size = ac.leaf(&mut bits)?;
            if size == 0 {
                break;
            }
            if size < 0xF {
                index += 0x10;
                continue;
            }
            index += size & 0xF;
            if index >= 64 {
                break;
            }
            size >>= 4;
            if size > 0 {
                out[i + ZIGZAG[index]] = signed(bits.take(size)?, size) as u16;
            }
            index += 1;
        }
    }
    Some(out)
}

fn decode2(b: &[u8], w: usize, h: usize, bpp: usize) -> Option<Vec<u8>> {
    let (table, start) = decrypted(b)?;
    let mut mul = [[0f32; 64]; 2];
    for (k, m) in mul.iter_mut().enumerate() {
        for (j, v) in m.iter_mut().enumerate() {
            *v = *table.get(k * 64 + j)? as f32 * SCALE[j];
        }
    }
    let raw = &b[start..];
    let (pw, ph) = (w.div_ceil(8) * 8, h.div_ceil(8) * 8);
    let bands = ph / 8;
    let mut p = 0;
    let dc = Tree::build(&(0..0x10).map(|_| varint(raw, &mut p)).collect::<Option<Vec<_>>>()?, true);
    let ac = Tree::build(&(0..0xB0).map(|_| varint(raw, &mut p)).collect::<Option<Vec<_>>>()?, true);
    let offsets = (0..=bands).map(|i| word(raw, p + i * 4).map(|o| o as usize)).collect::<Option<Vec<_>>>()?;
    let mut px = vec![0xFFu8; pw * ph * 4];
    let colour = bpp != 8;
    for band in 0..bands {
        let mut q = offsets[band] + pw.div_ceil(64);
        let count = varint(raw, &mut q)?;
        if count != pw * 8 * if colour { 3 } else { 1 } {
            return None;
        }
        // the last band's end may be written as -1
        let end = if (q..=raw.len()).contains(&offsets[band + 1]) { offsets[band + 1] } else { raw.len() };
        let coef = coefficients(count, raw.get(q..end)?, &dc, &ac)?;
        let out = &mut px[pw * 8 * 4 * band..];
        for i in 0..pw / 8 {
            if colour {
                let ycc: Vec<[f32; 64]> = (0..3).map(|c| idct(&coef[i * 64 + c * pw * 8..], &mul[(c > 0) as usize])).collect();
                for (o, ((&y, &cb), &cr)) in ycc[0].iter().zip(&ycc[1]).zip(&ycc[2]).enumerate() {
                    let r = y + 1.402 * cr - 178.956;
                    let g = y + 44.04992 - 0.34414 * cb + 91.90992 - 0.71414 * cr;
                    let bl = y + 1.772 * cb - 226.316;
                    let at = ((o / 8) * pw + i * 8 + o % 8) * 4;
                    out[at..at + 3].copy_from_slice(&[bl, g, r].map(|v| v.clamp(0.0, 255.0) as u8));
                }
            } else {
                let grey = idct(&coef[i * 64..], &mul[0]);
                for (o, &v) in grey.iter().enumerate() {
                    let at = ((o / 8) * pw + i * 8 + o % 8) * 4;
                    out[at..at + 3].fill(v as u8);
                }
            }
        }
    }
    if bpp == 32 && word(raw, offsets[bands]) == Some(1) {
        alpha(&mut px, &raw[offsets[bands] + 4..], pw);
    }
    Some((0..h).flat_map(|y| px[y * pw * 4..(y * pw + w) * 4].to_vec()).collect())
}

/// Alpha, as bytes and copies from nearby pixels, flagged by bits from the lowest.
fn alpha(px: &mut [u8], data: &[u8], w: usize) {
    let (mut p, mut at, mut mark, mut left) = (0, 3usize, 0u8, 0);
    while p < data.len() {
        if left == 0 {
            mark = data[p];
            p += 1;
            left = 8;
        }
        left -= 1;
        if mark & 1 << (7 - left) != 0 {
            let Some(v) = data.get(p..p + 2).map(|v| u16::from_le_bytes([v[0], v[1]]) as isize) else { return };
            p += 2;
            let back = if v & 0x3F > 0x1F { (v & 0x3F) - 0x40 } else { v & 0x3F };
            let up = if (v >> 6) & 7 != 0 { ((v >> 6) & 7) - 8 } else { 0 };
            let Some(mut from) = at.checked_add_signed((back + up * w as isize) * 4) else { return };
            for _ in 0..((v >> 9) & 0x7F) + 3 {
                let (Some(&a), true) = (px.get(from), at < px.len()) else { return };
                px[at] = a;
                from += 4;
                at += 4;
            }
        } else if p < data.len() && at < px.len() {
            px[at] = data[p];
            p += 1;
            at += 4;
        } else {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(w: usize, h: usize) -> Vec<u8> {
        (0..w * h).flat_map(|i| [(i * 7) as u8, (i / w * 30) as u8, if i % 5 == 0 { 0 } else { 200 }, (i % 3 * 120) as u8]).collect()
    }

    #[test]
    fn a_picture_written_as_version_1_reads_back_the_same() {
        for (w, h, bpp) in [(13, 7, 32), (9, 4, 24), (1, 1, 32)] {
            let px = picture(w, h);
            let back = decode(&encode(w, h, bpp, &px)).unwrap();
            let want: Vec<u8> = px.as_chunks::<4>().0.iter().flat_map(|c| [c[0], c[1], c[2], if bpp == 32 { c[3] } else { 255 }]).collect();
            assert_eq!(back, (w, h, bpp, want), "{w}x{h} at {bpp}");
        }
    }

    #[test]
    fn one_colour_has_a_tree_of_one_leaf_and_still_reads() {
        let px = [9, 9, 9, 255].repeat(20);
        assert_eq!(decode(&encode(5, 4, 32, &px)).unwrap().3, px);
    }

    #[test]
    fn a_broken_checksum_is_refused() {
        let mut b = encode(3, 3, 32, &picture(3, 3));
        b[0x2C] ^= 1;
        assert_eq!(decode(&b), None);
    }
}

