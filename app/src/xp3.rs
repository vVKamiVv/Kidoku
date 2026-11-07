//! XP3 filters: the encryption some KiriKiri games put on their files. Each one is keyed by the file's
//! Adler-32 in the index, which is the checksum of the plain bytes, so a guess proves itself.
//!
//! The per-title XORs and cxdec settings follow arc_unpacker's `dec/kirikiri` (Copyright (C) 2016
//! rr-, GPL-3.0-or-later). The Adler-keyed rotating XOR and the hash-folded XOR follow CrySirror's
//! `xp3/adler_xor.rs` and `xp3/hash_fold_xor.rs` (MPL-2.0).

use crate::yuris::adler32;
use std::cell::Cell;
use std::path::Path;

/// A title's cxdec settings: where each file splits, and the order of the key-making steps.
struct Cx {
    split: (u32, u32),
    first: [u8; 3],
    one: [u8; 8],
    two: [u8; 6],
}

const CXDEC: &[Cx] = &[
    // Fate/hollow ataraxia
    Cx { split: (0x143, 0x787), first: [0, 1, 2], one: [0, 1, 2, 3, 4, 5, 6, 7], two: [0, 1, 2, 3, 4, 5] },
    // Comyu - Kuroi Ryuu to Yasashii Oukoku
    Cx { split: (0x1A3, 0x0B6), first: [0, 1, 2], one: [0, 7, 5, 6, 3, 1, 4, 2], two: [4, 3, 2, 1, 5, 0] },
    // Mahou Tsukai no Yoru
    Cx { split: (0x22A, 0x2A2), first: [1, 0, 2], one: [7, 6, 5, 1, 0, 3, 4, 2], two: [3, 2, 1, 4, 5, 0] },
    // Natsuzora Kanata
    Cx { split: (0x2F5, 0x6F0), first: [2, 0, 1], one: [7, 2, 3, 6, 1, 0, 5, 4], two: [2, 3, 4, 0, 1, 5] },
    // Tenshin Ranman - Lucky or Unlucky!?
    Cx { split: (0x167, 0x498), first: [1, 0, 2], one: [4, 2, 3, 5, 6, 1, 7, 0], two: [1, 0, 5, 4, 3, 2] },
    // Dracu-Riot!
    Cx { split: (0x2F0, 0x418), first: [2, 0, 1], one: [5, 3, 0, 2, 1, 4, 6, 7], two: [0, 3, 5, 4, 2, 1] },
    // Kourin no Machi, Lavender no Shoujo
    Cx { split: (0x181, 0x635), first: [2, 1, 0], one: [7, 5, 2, 3, 6, 1, 4, 0], two: [4, 0, 1, 5, 2, 3] },
    // Karakara
    Cx { split: (0x190, 0x4A7), first: [1, 0, 2], one: [2, 0, 7, 3, 5, 1, 4, 6], two: [2, 1, 0, 5, 4, 3] },
    // Ushinawareta Mirai o Motomete
    Cx { split: (0x23C, 0x60F), first: [2, 0, 1], one: [1, 5, 0, 3, 2, 7, 6, 4], two: [4, 5, 2, 1, 0, 3] },
];

/// cxdec's table of words, kept in the game's `.tpm` plugin.
const BLOCK_MAGIC: &[u8] = b" Encryption cont";
const BLOCK: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Xor,
    XorP1Neg,
    XorMix,
    Dieselmine,
    Moteyaba,
    Kamiyaba,
    Rebirth,
    Fsn,
    /// One byte from the folded checksum, with a bias.
    Fold(u8),
    /// 31 bytes from the checksum, by offset.
    Rotating,
    Cxdec(usize),
}

const SIMPLE: &[Kind] = &[Kind::Xor, Kind::XorP1Neg, Kind::XorMix, Kind::Dieselmine, Kind::Moteyaba, Kind::Kamiyaba, Kind::Rebirth, Kind::Fsn, Kind::Rotating];

#[derive(Clone, Debug)]
pub struct Crypt {
    kind: Kind,
    block: Vec<u8>,
    /// The byte a folded key of 0 stands for, once a file has shown it.
    zero: Cell<Option<u8>>,
}

fn fold(h: u32) -> u8 {
    (h ^ (h >> 8) ^ (h >> 16) ^ (h >> 24)) as u8
}

fn rotating(h: u32) -> [u8; 31] {
    let mut state = (h << 31) | (h & 0x7fff_ffff);
    std::array::from_fn(|_| {
        let byte = state as u8;
        state = ((state & 0xffff_fffe) << 23) | (state >> 8);
        byte
    })
}

/// The engine builds a short routine per file from a seeded LCG, giving up past 128 bytes of code;
/// only the length of that code matters here.
struct Deriver<'a> {
    cx: &'a Cx,
    block: &'a [u8],
    seed: u32,
    parameter: u32,
    code: usize,
}

impl Deriver<'_> {
    fn add(&mut self, n: usize) -> Option<()> {
        self.code += n;
        (self.code <= 128).then_some(())
    }

    fn rand(&mut self) -> u32 {
        let old = self.seed;
        self.seed = old.wrapping_mul(0x41C6_4E6D).wrapping_add(12345);
        self.seed ^ (old << 16) ^ (old >> 16)
    }

    fn word(&self, i: u32) -> u32 {
        let p = (i as usize & 0x3FF) * 4;
        u32::from_le_bytes(self.block[p..p + 4].try_into().unwrap())
    }

    fn derive(&mut self, seed: u32, parameter: u32) -> Option<u32> {
        (self.seed, self.parameter) = (seed, parameter);
        (1..=5).rev().find_map(|stage| {
            self.code = 0;
            self.add(9)?;
            let v = self.two(stage)?;
            self.add(6)?;
            Some(v)
        })
    }

    fn first(&mut self) -> Option<u32> {
        match self.cx.first[(self.rand() % 3) as usize] {
            0 => {
                self.add(1)?;
                let v = self.rand();
                self.add(4)?;
                Some(v)
            }
            1 => {
                self.add(2)?;
                Some(self.parameter)
            }
            _ => {
                self.add(7)?;
                let at = self.rand();
                self.add(4)?;
                Some(self.word(at))
            }
        }
    }

    fn either(&mut self, stage: u32) -> Option<u32> {
        if self.rand() & 1 != 0 { self.two(stage) } else { self.one(stage) }
    }

    fn one(&mut self, stage: u32) -> Option<u32> {
        if stage == 1 {
            return self.first();
        }
        let eax = self.either(stage - 1)?;
        Some(match self.cx.one[(self.rand() % 8) as usize] {
            0 => self.add(2).map(|_| !eax)?,
            1 => self.add(1).map(|_| eax.wrapping_sub(1))?,
            2 => self.add(2).map(|_| eax.wrapping_neg())?,
            3 => self.add(1).map(|_| eax.wrapping_add(1))?,
            4 => self.add(13).map(|_| self.word(eax))?,
            5 => self.add(21).map(|_| ((eax & 0xAAAA_AAAA) >> 1) | ((eax & 0x5555_5555) << 1))?,
            6 => {
                self.add(1)?;
                let v = self.rand();
                self.add(4)?;
                eax ^ v
            }
            _ => {
                let add = self.rand() & 1 != 0;
                self.add(1)?;
                let v = self.rand();
                self.add(4)?;
                if add { eax.wrapping_add(v) } else { eax.wrapping_sub(v) }
            }
        })
    }

    fn two(&mut self, stage: u32) -> Option<u32> {
        if stage == 1 {
            return self.first();
        }
        self.add(1)?;
        let ebx = self.either(stage - 1)?;
        self.add(2)?;
        let eax = self.either(stage - 1)?;
        let eax = match self.cx.two[(self.rand() % 6) as usize] {
            0 => self.add(9).map(|_| eax >> (ebx & 15))?,
            1 => self.add(9).map(|_| eax << (ebx & 15))?,
            2 => self.add(2).map(|_| eax.wrapping_add(ebx))?,
            3 => self.add(4).map(|_| ebx.wrapping_sub(eax))?,
            4 => self.add(3).map(|_| eax.wrapping_mul(ebx))?,
            _ => self.add(2).map(|_| eax.wrapping_sub(ebx))?,
        };
        self.add(1)?;
        Some(eax)
    }
}

fn cxdec(cx: &Cx, block: &[u8], d: &mut [u8], h: u32) {
    let split = d.len().min(((h & cx.split.0) + cx.split.1) as usize);
    let (head, tail) = d.split_at_mut(split);
    for (part, hash, base) in [(head, h, 0), (tail, (h >> 16) ^ h, split)] {
        let mut k = Deriver { cx, block, seed: 0, parameter: 0, code: 0 };
        let (seed, hash) = (hash & 0x7F, hash >> 7);
        let (Some(a), Some(b)) = (k.derive(seed, hash), k.derive(seed, !hash)) else { return };
        for (at, x) in [((b >> 16) as usize, (a >> 8) as u8), ((b & 0xFFFF) as usize, (a >> 16) as u8)] {
            if let Some(byte) = at.checked_sub(base).and_then(|i| part.get_mut(i)) {
                *byte ^= x;
            }
        }
        let x = if a as u8 == 0 { 1 } else { a as u8 };
        part.iter_mut().for_each(|b| *b ^= x);
    }
}

/// The game's cxdec table, from a `.tpm` in its folder or one below.
fn control_block(game: &Path) -> Option<Vec<u8>> {
    let mut dirs = vec![game.to_path_buf()];
    dirs.extend(std::fs::read_dir(game).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    dirs.iter().flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("tpm"))).find_map(|p| {
        let b = std::fs::read(p).ok()?;
        let at = b.windows(BLOCK_MAGIC.len()).position(|w| w == BLOCK_MAGIC)?;
        b.get(at..at + BLOCK).map(<[u8]>::to_vec)
    })
}

impl Crypt {
    fn new(kind: Kind, block: &[u8]) -> Crypt {
        Crypt { kind, block: block.to_vec(), zero: Cell::new(None) }
    }

    /// The filter that turns every sample (`(checksum, stored bytes)`) back into bytes with that
    /// checksum, if one does. `game` is searched for cxdec's table.
    pub fn detect(samples: &[(u32, Vec<u8>)], game: &Path) -> Option<Crypt> {
        let (h, raw) = samples.first()?;
        let fits = |c: &Crypt| samples.iter().all(|(h, raw)| c.open(raw, *h).is_some());
        let folds = (0..=255u8).find(|&k| adler32(&raw.iter().map(|b| b ^ k).collect::<Vec<_>>()) == *h).map(|k| Kind::Fold(fold(*h) ^ k));
        let found = SIMPLE.iter().copied().chain(folds).map(|k| Crypt::new(k, &[])).find(fits);
        found.or_else(|| {
            let block = control_block(game)?;
            (0..CXDEC.len()).map(|i| Crypt::new(Kind::Cxdec(i), &block)).find(fits)
        })
    }

    fn apply(&self, d: &mut [u8], h: u32, seal: bool, zero: u8) {
        let x = |d: &mut [u8], k: u8| d.iter_mut().for_each(|b| *b ^= k);
        let k = h as u8;
        match self.kind {
            Kind::Xor => x(d, k),
            Kind::XorP1Neg => x(d, (h.wrapping_add(1) ^ 0xFF) as u8),
            Kind::XorMix => d.iter_mut().enumerate().for_each(|(i, b)| *b ^= if i % 2 == 0 { k } else { i as u8 }),
            Kind::Dieselmine => d.iter_mut().enumerate().for_each(|(i, b)| {
                let (xor, by) = match i {
                    0..0x7B => (true, 21),
                    0x7B..0xF6 => (false, 32),
                    0xF6..0x171 => (true, 43),
                    _ => (false, 54),
                };
                let n = k.wrapping_mul(by);
                *b = if xor { *b ^ n } else if seal { b.wrapping_add(n) } else { b.wrapping_sub(n) };
            }),
            Kind::Moteyaba => x(d, 0xCD ^ k),
            Kind::Kamiyaba => x(d, 0xCD),
            Kind::Rebirth => x(d.get_mut(5..).unwrap_or_default(), (h >> 12) as u8),
            Kind::Fsn => {
                x(d, 0x36);
                for (at, v) in [(0x2EA29, 3), (0x13, 1)] {
                    if let Some(b) = d.get_mut(at) {
                        *b ^= v;
                    }
                }
            }
            Kind::Fold(bias) => x(d, match fold(h) ^ bias { 0 => zero, k => k }),
            Kind::Rotating => {
                let key = rotating(h);
                d.iter_mut().enumerate().for_each(|(i, b)| *b ^= key[i % 31]);
            }
            Kind::Cxdec(i) => cxdec(&CXDEC[i], &self.block, d, h),
        }
    }

    fn zero_key(&self, h: u32) -> bool {
        matches!(self.kind, Kind::Fold(bias) if fold(h) == bias)
    }

    /// The plain bytes, when this filter gives back the checksum `h`.
    pub fn open(&self, raw: &[u8], h: u32) -> Option<Vec<u8>> {
        let try_with = |zero: u8| {
            let mut d = raw.to_vec();
            self.apply(&mut d, h, false, zero);
            (adler32(&d) == h).then_some(d)
        };
        if !self.zero_key(h) {
            return try_with(0);
        }
        let found = match self.zero.get() {
            Some(z) => try_with(z).map(|d| (z, d)),
            None => (0..=255).find_map(|z| try_with(z).map(|d| (z, d))),
        };
        found.map(|(z, d)| {
            self.zero.set(Some(z));
            d
        })
    }

    /// `plain` as the game stores it under the checksum `h`. None when that key isn't known yet.
    pub fn seal(&self, plain: &[u8], h: u32) -> Option<Vec<u8>> {
        let zero = if self.zero_key(h) { self.zero.get()? } else { 0 };
        let mut d = plain.to_vec();
        self.apply(&mut d, h, true, zero);
        Some(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_filter_opens_what_it_sealed_and_is_found_from_samples() {
        let block: Vec<u8> = (0..BLOCK as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let mut all: Vec<Crypt> = SIMPLE.iter().map(|&k| Crypt::new(k, &[])).collect();
        all.push(Crypt::new(Kind::Fold(0x5A), &[]));
        all.extend((0..CXDEC.len()).map(|i| Crypt::new(Kind::Cxdec(i), &block)));
        let texts: Vec<Vec<u8>> = (0..3u32).map(|n| (0..3000u32).map(|i| (i * 7 + n * 13) as u8 ^ 0x21).collect()).collect();
        let game = std::env::temp_dir().join("kidoku-xp3-filters");
        std::fs::create_dir_all(&game).unwrap();
        let tpm = [b"head".as_slice(), BLOCK_MAGIC, &block[BLOCK_MAGIC.len()..]].concat();
        let block = tpm[4..4 + BLOCK].to_vec();
        std::fs::write(game.join("plugin.tpm"), &tpm).unwrap();
        for c in all.iter().map(|c| Crypt { block: if c.block.is_empty() { Vec::new() } else { block.clone() }, ..c.clone() }) {
            let samples: Vec<(u32, Vec<u8>)> = texts.iter().map(|t| (adler32(t), c.seal(t, adler32(t)).unwrap())).collect();
            for ((h, raw), t) in samples.iter().zip(&texts) {
                assert_ne!(raw, t, "{:?}", c.kind);
                assert_eq!(c.open(raw, *h).as_ref(), Some(t), "{:?}", c.kind);
            }
            let found = Crypt::detect(&samples, &game).unwrap_or_else(|| panic!("{:?} not found", c.kind));
            for ((h, raw), t) in samples.iter().zip(&texts) {
                assert_eq!(found.open(raw, *h).as_ref(), Some(t), "{:?} found as {:?}", c.kind, found.kind);
            }
        }
        std::fs::remove_dir_all(&game).ok();
    }

    #[test]
    fn the_rotating_key_steps_a_byte_at_a_time() {
        assert_eq!(rotating(0x8765_4321)[..2], [0x21, 0x43]);
    }
}
