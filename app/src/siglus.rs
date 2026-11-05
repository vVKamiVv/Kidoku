//! SiglusEngine (Scene.pck, Gameexe.dat): compiled scenes, LZ packed and XORed with a fixed table and,
//! on most games, a 16 byte key from the game's program. That key can be worked out from the scenes
//! themselves: each restarts it, and each starts with the same kind of header. Text sits in a table
//! the code refers to by number, so it can be replaced without touching the code. Gameexe.dat holds
//! the settings, the words of the right-click menu and the game's own dialogs among them.

use crate::code::{shown_strings, with_english, GAMEEXE};
use crate::pipeline::Line;
use crate::text::{wrap_words, JA};
use crate::{original, replace_file, R};
use regex::Regex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const PACK: &str = "Scene.pck";
const CONFIG: &str = "Gameexe.dat";

const SCENE_TABLE: [u8; 256] = [
    0x70, 0xf8, 0xa6, 0xb0, 0xa1, 0xa5, 0x28, 0x4f, 0xb5, 0x2f, 0x48, 0xfa, 0xe1, 0xe9, 0x4b, 0xde,
    0xb7, 0x4f, 0x62, 0x95, 0x8b, 0xe0, 0x03, 0x80, 0xe7, 0xcf, 0x0f, 0x6b, 0x92, 0x01, 0xeb, 0xf8,
    0xa2, 0x88, 0xce, 0x63, 0x04, 0x38, 0xd2, 0x6d, 0x8c, 0xd2, 0x88, 0x76, 0xa7, 0x92, 0x71, 0x8f,
    0x4e, 0xb6, 0x8d, 0x01, 0x79, 0x88, 0x83, 0x0a, 0xf9, 0xe9, 0x2c, 0xdb, 0x67, 0xdb, 0x91, 0x14,
    0xd5, 0x9a, 0x4e, 0x79, 0x17, 0x23, 0x08, 0x96, 0x0e, 0x1d, 0x15, 0xf9, 0xa5, 0xa0, 0x6f, 0x58,
    0x17, 0xc8, 0xa9, 0x46, 0xda, 0x22, 0xff, 0xfd, 0x87, 0x12, 0x42, 0xfb, 0xa9, 0xb8, 0x67, 0x6c,
    0x91, 0x67, 0x64, 0xf9, 0xd1, 0x1e, 0xe4, 0x50, 0x64, 0x6f, 0xf2, 0x0b, 0xde, 0x40, 0xe7, 0x47,
    0xf1, 0x03, 0xcc, 0x2a, 0xad, 0x7f, 0x34, 0x21, 0xa0, 0x64, 0x26, 0x98, 0x6c, 0xed, 0x69, 0xf4,
    0xb5, 0x23, 0x08, 0x6e, 0x7d, 0x92, 0xf6, 0xeb, 0x93, 0xf0, 0x7a, 0x89, 0x5e, 0xf9, 0xf8, 0x7a,
    0xaf, 0xe8, 0xa9, 0x48, 0xc2, 0xac, 0x11, 0x6b, 0x2b, 0x33, 0xa7, 0x40, 0x0d, 0xdc, 0x7d, 0xa7,
    0x5b, 0xcf, 0xc8, 0x31, 0xd1, 0x77, 0x52, 0x8d, 0x82, 0xac, 0x41, 0xb8, 0x73, 0xa5, 0x4f, 0x26,
    0x7c, 0x0f, 0x39, 0xda, 0x5b, 0x37, 0x4a, 0xde, 0xa4, 0x49, 0x0b, 0x7c, 0x17, 0xa3, 0x43, 0xae,
    0x77, 0x06, 0x64, 0x73, 0xc0, 0x43, 0xa3, 0x18, 0x5a, 0x0f, 0x9f, 0x02, 0x4c, 0x7e, 0x8b, 0x01,
    0x9f, 0x2d, 0xae, 0x72, 0x54, 0x13, 0xff, 0x96, 0xae, 0x0b, 0x34, 0x58, 0xcf, 0xe3, 0x00, 0x78,
    0xbe, 0xe3, 0xf5, 0x61, 0xe4, 0x87, 0x7c, 0xfc, 0x80, 0xaf, 0xc4, 0x8d, 0x46, 0x3a, 0x5d, 0xd0,
    0x36, 0xbc, 0xe5, 0x60, 0x77, 0x68, 0x08, 0x4f, 0xbb, 0xab, 0xe2, 0x78, 0x07, 0xe8, 0x73, 0xbf,
];

const CONFIG_TABLE: [u8; 256] = [
    0xd8, 0x29, 0xb9, 0x16, 0x3d, 0x1a, 0x76, 0xd0, 0x87, 0x9b, 0x2d, 0x0c, 0x7b, 0xd1, 0xa9, 0x19,
    0x22, 0x9f, 0x91, 0x73, 0x6a, 0x35, 0xb1, 0x7e, 0xd1, 0xb5, 0xe7, 0xe6, 0xd5, 0xf5, 0x06, 0xd6,
    0xba, 0xbf, 0xf3, 0x45, 0x3f, 0xf1, 0x61, 0xdd, 0x4c, 0x67, 0x6a, 0x6f, 0x74, 0xec, 0x7a, 0x6f,
    0x26, 0x74, 0x0e, 0xdb, 0x27, 0x4c, 0xa5, 0xf1, 0x0e, 0x2d, 0x70, 0xc4, 0x40, 0x5d, 0x4f, 0xda,
    0x9e, 0xc5, 0x49, 0x7b, 0xbd, 0xe8, 0xdf, 0xee, 0xca, 0xf4, 0x92, 0xde, 0xe4, 0x76, 0x10, 0xdd,
    0x2a, 0x52, 0xdc, 0x73, 0x4e, 0x54, 0x8c, 0x30, 0x3d, 0x9a, 0xb2, 0x9b, 0xb8, 0x93, 0x29, 0x55,
    0xfa, 0x7a, 0xc9, 0xda, 0x10, 0x97, 0xe5, 0xb6, 0x23, 0x02, 0xdd, 0x38, 0x4c, 0x9b, 0x1f, 0x9a,
    0xd5, 0x49, 0xe9, 0x34, 0x0f, 0x28, 0x2d, 0x1b, 0x52, 0x39, 0x5c, 0x36, 0x89, 0x56, 0xa7, 0x96,
    0x14, 0xbe, 0x2e, 0xc5, 0x3e, 0x08, 0x5f, 0x47, 0xa9, 0xdf, 0x88, 0x9f, 0xd4, 0xcc, 0x69, 0x1f,
    0x30, 0x9f, 0xe7, 0xcd, 0x80, 0x45, 0xf3, 0xe7, 0x2a, 0x1d, 0x16, 0xb2, 0xf1, 0x54, 0xc8, 0x6c,
    0x2b, 0x0d, 0xd4, 0x65, 0xf7, 0xe3, 0x36, 0xd4, 0xa5, 0x3b, 0xd1, 0x79, 0x4c, 0x54, 0xf0, 0x2a,
    0xb4, 0xb2, 0x56, 0x45, 0x2e, 0xab, 0x7b, 0x88, 0xc5, 0xfa, 0x74, 0xad, 0x03, 0xb8, 0x9e, 0xd5,
    0xf5, 0x6f, 0xdc, 0xfa, 0x44, 0x49, 0x31, 0xf6, 0x83, 0x32, 0xff, 0xc2, 0xb1, 0xe9, 0xe1, 0x98,
    0x3d, 0x6f, 0x31, 0x0d, 0xac, 0xb1, 0x08, 0x83, 0x9d, 0x0d, 0x10, 0xd1, 0x41, 0xf9, 0x00, 0xba,
    0x1a, 0xcf, 0x13, 0x71, 0xe4, 0x86, 0x21, 0x2f, 0x23, 0x65, 0xc3, 0x45, 0xa0, 0xc3, 0x92, 0x48,
    0x9d, 0xea, 0xdd, 0x31, 0x2c, 0xe9, 0xe2, 0x10, 0x22, 0xaa, 0xe1, 0xad, 0x2c, 0xc4, 0x2d, 0x7f,
];

fn word(b: &[u8], p: usize) -> R<usize> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated Siglus file")?.try_into().unwrap()) as usize)
}

// ---------- packing ----------

/// Flag bytes, read from the low bit: 1 is a literal, 0 a 12 bit distance and 4 bit length.
fn unpack(b: &[u8]) -> R<Vec<u8>> {
    const BAD: &str = "bad packed Siglus data";
    let size = word(b, 4)?;
    let (mut out, mut p) = (Vec::with_capacity(size), 8);
    while out.len() < size {
        let flags = *b.get(p).ok_or(BAD)?;
        p += 1;
        for bit in 0..8 {
            if out.len() >= size {
                break;
            }
            if flags >> bit & 1 != 0 {
                out.push(*b.get(p).ok_or(BAD)?);
                p += 1;
            } else {
                let raw = u16::from_le_bytes(b.get(p..p + 2).ok_or(BAD)?.try_into().unwrap()) as usize;
                p += 2;
                let (back, len) = (raw >> 4, (raw & 15) + 2);
                if back == 0 || back > out.len() {
                    return Err(BAD.into());
                }
                for _ in 0..len.min(size - out.len()) {
                    out.push(out[out.len() - back]);
                }
            }
        }
    }
    Ok(out)
}

/// Literals only: a little bigger than the game's own, which doesn't matter to it.
fn stored(plain: &[u8]) -> Vec<u8> {
    let mut out = vec![0; 8];
    for chunk in plain.chunks(8) {
        out.push(0xff);
        out.extend(chunk);
    }
    let size = out.len() as u32;
    out[..4].copy_from_slice(&size.to_le_bytes());
    out[4..8].copy_from_slice(&(plain.len() as u32).to_le_bytes());
    out
}

fn crypt(b: &mut [u8], table: &[u8; 256], key: &[u8; 16]) {
    for (i, c) in b.iter_mut().enumerate() {
        *c ^= table[i % 256] ^ key[i % 16];
    }
}

// ---------- the key ----------

/// What a scene's header says about itself, for as many of its 33 words as are known yet.
fn fits(out: &[u8], size: Option<usize>) -> bool {
    let w = |i: usize| word(out, 4 * i).ok();
    let same = |a: Option<usize>, b: Option<usize>| a.is_none() || b.is_none() || a == b;
    let after = |ofs: usize, cnt: usize, unit: usize, next: usize| {
        let reach = match (w(ofs), w(cnt)) {
            (Some(o), Some(c)) => Some(o.saturating_add(c.saturating_mul(unit))),
            _ => None,
        };
        same(reach, w(next))
    };
    w(0).is_none_or(|h| h == 132)
        && w(3).is_none_or(|h| h == 132)
        && same(w(4), w(6))
        && after(3, 4, 8, 5)
        && same(w(14), w(16))
        && same(w(14), w(18))
        && same(w(20), w(22))
        && same(w(20), w(24))
        && after(7, 8, 4, 9)
        && after(9, 10, 4, 11)
        && after(11, 12, 8, 13)
        && after(13, 14, 8, 15)
        && after(15, 16, 8, 17)
        && after(19, 20, 4, 21)
        && after(21, 22, 8, 23)
        && after(25, 26, 8, 27)
        && after(29, 30, 4, 31)
        && same(w(1).zip(w(2)).map(|(a, b)| a + b), w(7))
        && (1..32).step_by(2).all(|i| w(i).is_none_or(|o| o <= 1 << 28))
        && size.is_none_or(|s| w(31).zip(w(32)).is_none_or(|(o, c)| o + 4 * c == s))
}

/// As much of a scene's start as the known key bytes reach. None if no real stream looks like this.
fn prefix(m: &[u8], key: &[Option<u8>; 16], want: usize) -> Option<Vec<u8>> {
    let get = |i: usize| Some(m.get(i)? ^ key[i % 16]?);
    let (mut p, mut out) = (8, Vec::new());
    while out.len() < want {
        let Some(flags) = get(p) else { return Some(out) };
        p += 1;
        for bit in 0..8 {
            if out.len() >= want {
                break;
            }
            if flags >> bit & 1 != 0 {
                let Some(c) = get(p) else { return Some(out) };
                out.push(c);
                p += 1;
            } else {
                let (Some(a), Some(b)) = (get(p), get(p + 1)) else { return Some(out) };
                p += 2;
                let raw = u16::from_le_bytes([a, b]) as usize;
                let (back, len) = (raw >> 4, (raw & 15) + 2);
                if back == 0 || back > out.len() {
                    return None;
                }
                for _ in 0..len {
                    out.push(out[out.len() - back]);
                }
            }
        }
    }
    Some(out)
}

/// The likeliest values of each unknown key byte, tried best first. Only a whole key that unpacks
/// every scene into a sound scene counts, and the headers throw out wrong starts early.
fn solve(scenes: &[Vec<u8>], key: &mut [Option<u8>; 16], order: &[(usize, Vec<u8>)]) -> Option<[u8; 16]> {
    let Some(((at, values), rest)) = order.split_first() else {
        let full = key.map(|k| k.unwrap_or(0));
        let right = |m: &Vec<u8>| {
            let mut b = m.clone();
            b.iter_mut().enumerate().for_each(|(i, c)| *c ^= full[i % 16]);
            unpack(&b).is_ok_and(|out| fits(&out, Some(out.len())))
        };
        return scenes.iter().all(right).then_some(full);
    };
    for &value in values {
        key[*at] = Some(value);
        if scenes.iter().all(|m| prefix(m, key, 132).is_some_and(|o| fits(&o, None)))
            && let Some(done) = solve(scenes, key, rest)
        {
            return Some(done);
        }
    }
    key[*at] = None;
    None
}

/// The key from the scenes alone; `scenes` have had the fixed table taken off already. The first four
/// bytes are each stream's own known size. Every place in the key then sees the same mix of bytes,
/// shifted by its own key byte, so lining a place's mix up against those four gives its key byte.
fn find_key(scenes: &[Vec<u8>]) -> R<[u8; 16]> {
    const LOST: &str = "couldn't work out this Siglus game's key";
    const TRIES: usize = 3;
    let first = scenes.first().ok_or(LOST)?;
    let mut key = [None; 16];
    for (i, k) in key.iter_mut().enumerate().take(4) {
        *k = Some(first.get(i).ok_or(LOST)? ^ (first.len() as u32).to_le_bytes()[i]);
    }
    if scenes.iter().any(|m| (0..4).any(|i| Some(m[i] ^ (m.len() as u32).to_le_bytes()[i]) != key[i])) {
        return Err(LOST.into());
    }
    let mut seen = [[0u64; 256]; 16];
    for m in scenes {
        for (i, &c) in m.iter().enumerate().skip(8) {
            seen[i % 16][c as usize] += 1;
        }
    }
    let mut mix = [0u64; 256];
    for (place, known) in seen.iter().zip(key).take(4) {
        for (b, count) in place.iter().enumerate() {
            mix[b ^ known.unwrap() as usize] += count;
        }
    }
    let likely = |at: usize| {
        let score = |v: usize| (0..256).map(|b| mix[b] * seen[at][b ^ v]).sum::<u64>();
        let mut values: Vec<usize> = (0..256).collect();
        values.sort_by_key(|&v| std::cmp::Reverse(score(v)));
        (at, values[..TRIES].iter().map(|&v| v as u8).collect())
    };
    let order: Vec<(usize, Vec<u8>)> = [8, 9, 10, 11, 12, 13, 14, 15, 4, 5, 6, 7].map(likely).into();
    solve(scenes, &mut key, &order).ok_or(LOST.into())
}

// ---------- Scene.pck ----------

/// Scene.pck: a header of 23 words, tables nobody needs here, then the scenes.
struct Pack {
    bytes: Vec<u8>,
    index: usize,
    start: usize,
    scenes: Vec<(usize, usize)>,
    key: [u8; 16],
}

impl Pack {
    /// `known` is the key, where it has been worked out: a pack we wrote has no byte mix left to go by.
    fn read(bytes: Vec<u8>, known: Option<[u8; 16]>) -> R<Pack> {
        const SCENE_INDEX: usize = 17 * 4;
        let (index, count, start) = (word(&bytes, SCENE_INDEX)?, word(&bytes, SCENE_INDEX + 4)?, word(&bytes, 19 * 4)?);
        let scenes: Vec<(usize, usize)> = (0..count).map(|i| Ok((word(&bytes, index + 8 * i)?, word(&bytes, index + 8 * i + 4)?))).collect::<R<_>>()?;
        let mut key = known.unwrap_or_default();
        if known.is_none() && word(&bytes, 21 * 4)? != 0 {
            let masked = scenes.iter().map(|&(at, size)| {
                let mut b = bytes.get(start + at..start + at + size).ok_or("bad Scene.pck index")?.to_vec();
                crypt(&mut b, &SCENE_TABLE, &[0; 16]);
                Ok(b)
            });
            key = find_key(&masked.collect::<R<Vec<_>>>()?)?;
        }
        Ok(Pack { bytes, index, start, scenes, key })
    }

    fn scene(&self, i: usize) -> R<Scene> {
        let (at, size) = self.scenes[i];
        let mut b = self.bytes.get(self.start + at..self.start + at + size).ok_or("bad Scene.pck index")?.to_vec();
        crypt(&mut b, &SCENE_TABLE, &self.key);
        Scene::read(unpack(&b)?)
    }

    /// The pack with `new` scenes in place of the old: (scene number, its data).
    fn with(&self, new: &[(usize, Vec<u8>)]) -> R<Vec<u8>> {
        let mut out = self.bytes[..self.start].to_vec();
        let end = self.scenes.iter().map(|&(at, size)| at + size).max().unwrap_or(0);
        let mut place = 0;
        for (i, &(at, size)) in self.scenes.iter().enumerate() {
            let body = match new.iter().find(|(n, _)| *n == i) {
                Some((_, data)) => {
                    let mut b = stored(data);
                    crypt(&mut b, &SCENE_TABLE, &self.key);
                    b
                }
                None => self.bytes[self.start + at..self.start + at + size].to_vec(),
            };
            out[self.index + 8 * i..][..4].copy_from_slice(&(place as u32).to_le_bytes());
            out[self.index + 8 * i + 4..][..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
            place += body.len();
            out.extend(body);
        }
        out.extend(&self.bytes[self.start + end..]);
        Ok(out)
    }
}

// ---------- scenes ----------

const STRINGS: usize = 3 * 4; // header word: the (place, length) list, then its size
const STRING_DATA: usize = 5 * 4;
const STRING_FORM: i32 = 20;
const LIST_FORM: i32 = -1;

/// A scene. Words 1 and 2 of its header place the code, 3 to 6 the strings.
struct Scene {
    data: Vec<u8>,
}

fn string_key(i: usize) -> u16 {
    (i as u32).wrapping_mul(28807) as u16
}

#[derive(Default)]
struct Flow {
    /// Message strings in story order, with the speaker's string.
    said: Vec<(usize, Option<usize>)>,
    /// Other strings the code pushes, which include the choices.
    pushed: Vec<usize>,
}

impl Scene {
    fn read(data: Vec<u8>) -> R<Scene> {
        if word(&data, 0)? != 132 || word(&data, 4)? + word(&data, 8)? > data.len() {
            return Err("not a Siglus scene".into());
        }
        Ok(Scene { data })
    }

    fn strings(&self) -> R<Vec<Vec<u16>>> {
        let (list, count, data) = (word(&self.data, STRINGS)?, word(&self.data, STRINGS + 4)?, word(&self.data, STRING_DATA)?);
        (0..count)
            .map(|i| {
                let (at, len) = (word(&self.data, list + 8 * i)?, word(&self.data, list + 8 * i + 4)?);
                let raw = self.data.get(data + 2 * at..data + 2 * (at + len)).ok_or("bad Siglus string")?;
                Ok(raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]]) ^ string_key(i)).collect())
            })
            .collect()
    }

    /// The same scene with `strings` in place of its own. They go on the end, so that nothing else moves.
    fn with(&self, strings: &[Vec<u16>]) -> R<Vec<u8>> {
        let mut out = self.data.clone();
        let (list, data) = (word(&out, STRINGS)?, out.len());
        let (mut at, mut new) = (0, Vec::new());
        for (i, s) in strings.iter().enumerate() {
            out[list + 8 * i..][..4].copy_from_slice(&(at as u32).to_le_bytes());
            out[list + 8 * i + 4..][..4].copy_from_slice(&(s.len() as u32).to_le_bytes());
            new.extend(s.iter().flat_map(|u| (u ^ string_key(i)).to_le_bytes()));
            at += s.len();
        }
        out.extend(new);
        out[STRING_DATA..STRING_DATA + 4].copy_from_slice(&(data as u32).to_le_bytes());
        Ok(out)
    }

    /// Walks the code, which has no gaps, so an opcode this doesn't know is an error. A command that
    /// marks text as read carries the next read flag after it, which the list of flags' lines tells
    /// from the next instruction.
    fn flow(&self) -> R<Flow> {
        let (at, size) = (word(&self.data, 4)?, word(&self.data, 8)?);
        let code = self.data.get(at..at + size).ok_or("bad Siglus code range")?;
        let (flags_at, flags) = (word(&self.data, 31 * 4)?, word(&self.data, 32 * 4)?);
        let line_of = |flag: usize| (flag < flags).then(|| word(&self.data, flags_at + 4 * flag).ok()).flatten();
        let (mut p, mut flow, mut pushed, mut name) = (0, Flow::default(), None, None);
        let (mut line, mut flag) = (0, 0);
        let int = |p: &mut usize| {
            *p += 4;
            Ok::<_, String>(word(code, *p - 4)? as u32 as i32)
        };
        fn forms(p: &mut usize, code: &[u8]) -> R<()> {
            let count = word(code, *p)?;
            *p += 4;
            for _ in 0..count {
                let form = word(code, *p)? as u32 as i32;
                *p += 4;
                if form == LIST_FORM {
                    forms(p, code)?;
                }
            }
            Ok(())
        }
        while p < code.len() {
            let op = code[p];
            p += 1;
            match op {
                5 | 6 | 8 | 9 | 0x16 | 0x33 | 0x34 => {}
                1 => {
                    line = word(code, p)?;
                    p += 4;
                }
                3 | 4 | 0x10..=0x12 => p += 4,
                2 => {
                    let form = int(&mut p)?;
                    let value = int(&mut p)?;
                    if form == STRING_FORM && let Some(old) = pushed.replace(value as usize) {
                        flow.pushed.push(old);
                    }
                }
                7 => p += 8,
                0x13 | 0x14 => {
                    p += 4;
                    forms(&mut p, code)?;
                }
                0x15 => forms(&mut p, code)?,
                0x20 => p += 12,
                0x21 => p += 5,
                0x22 => p += 9,
                0x30 => {
                    p += 4;
                    forms(&mut p, code)?;
                    let named = int(&mut p)?;
                    p += 4 * named.max(0) as usize + 4;
                    flow.pushed.extend(pushed.take());
                    // the next instruction can look like a flag, but then what follows isn't an opcode
                    let opcode = |b: u8| matches!(b, 1..=9 | 0x10..=0x16 | 0x20..=0x22 | 0x30..=0x34);
                    if word(code, p).ok() == Some(flag) && line_of(flag) == Some(line) && code.get(p + 4).is_none_or(|b| opcode(*b)) {
                        p += 4;
                        flag += 1;
                    }
                }
                0x31 => {
                    flag = word(code, p)? + 1;
                    p += 4;
                    if let Some(text) = pushed.take() {
                        flow.said.push((text, name.take()));
                    }
                }
                0x32 => name = pushed.take(),
                _ => return Err(format!("unknown Siglus opcode {op:#x} at {p}")),
            }
        }
        Ok(flow)
    }
}

// ---------- the game ----------

/// Names of things the scenes load.
static NOT_TEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.\w{2,4}$|^[\x20-\x7e]*$").unwrap());

pub struct Game {
    pack: Pack,
    scenes: Vec<(usize, Scene, Flow, Vec<Vec<u16>>)>,
    width: usize,
    /// Gameexe.dat, and where the words a player reads are in it.
    settings: String,
    shown: Vec<(usize, usize)>,
}

fn text(s: &[u16]) -> String {
    String::from_utf16_lossy(s)
}

// ---------- settings ----------

/// Settings a player reads. Others name things, like the voices of `#CHRKOE`.
static SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#(?:SYSCOMMENU|DIALOG|WARNINGINFO|SAVEINFO|LOADINFO|SAVE\.NO_DATA_STR|DUMMY_CHECK)").unwrap());
static NOT_SHOWN: LazyLock<Regex> = LazyLock::new(|| Regex::new("$^").unwrap());
const CONFIG_HEAD: usize = 8;

/// Gameexe.dat's text, from the game's own file.
fn config_text(game: &Path, key: &[u8; 16]) -> R<String> {
    let mut b = original(&game.join(CONFIG))?;
    let body = b.get_mut(CONFIG_HEAD..).ok_or("truncated Gameexe.dat")?;
    crypt(body, &CONFIG_TABLE, key);
    let plain = unpack(body)?;
    Ok(String::from_utf16_lossy(&plain.chunks(2).map(|c| u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])).collect::<Vec<_>>()))
}

fn config_key(start: usize) -> String {
    format!("settings:{start}")
}

fn write_config(game: &Path, key: &[u8; 16], text: &str) -> R<()> {
    let plain: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut body = stored(&plain);
    crypt(&mut body, &CONFIG_TABLE, key);
    let own = original(&game.join(CONFIG))?;
    replace_file(&game.join(CONFIG), &[own.get(..CONFIG_HEAD).ok_or("truncated Gameexe.dat")?, &body].concat())
}

impl Game {
    pub fn open(game: &Path) -> R<Game> {
        let pack = Pack::read(original(&game.join(PACK))?, None)?;
        let settings = config_text(game, &pack.key).unwrap_or_default();
        let width = crate::fit::letters(game, window(&settings).unwrap_or(LINE_LETTERS));
        let shown = shown_strings(std::slice::from_ref(&settings), &GAMEEXE, &SHOWN, &NOT_SHOWN).into_iter().map(|s| (s.1, s.2)).collect();
        let mut scenes = Vec::new();
        for i in 0..pack.scenes.len() {
            let scene = pack.scene(i).map_err(|e| format!("scene {i}: {e}"))?;
            let flow = scene.flow().map_err(|e| format!("scene {i}: {e}"))?;
            let strings = scene.strings().map_err(|e| format!("scene {i}: {e}"))?;
            scenes.push((i, scene, flow, strings));
        }
        Ok(Game { pack, scenes, width, settings, shown })
    }

    /// Counts, for a look at a new game.
    pub fn summary(&self) -> String {
        let said: usize = self.scenes.iter().map(|s| s.2.said.len()).sum();
        let pushed: usize = self.scenes.iter().map(|s| s.2.pushed.len()).sum();
        format!("{} scenes, {said} messages, {pushed} other strings pushed, {} letters a line", self.scenes.len(), self.width)
    }

    /// Lines in story order: (scene.string, speaker, text). Choices follow their scene.
    pub fn lines(&self) -> Vec<Line> {
        let mut out = Vec::new();
        for (n, _, flow, strings) in &self.scenes {
            let get = |i: usize| strings.get(i).map(|s| text(s));
            for &(i, who) in &flow.said {
                if let Some(t) = get(i).filter(|t| JA.is_match(t)) {
                    out.push((format!("{n}.{i}"), who.and_then(get), t));
                }
            }
            let said: HashSet<usize> = flow.said.iter().map(|s| s.0).chain(flow.said.iter().filter_map(|s| s.1)).collect();
            let mut seen = HashSet::new();
            for &i in flow.pushed.iter().filter(|i| !said.contains(i)) {
                if let Some(t) = get(i).filter(|t| seen.insert(i) && JA.is_match(t) && !NOT_TEXT.is_match(t)) {
                    out.push((format!("{n}.{i}"), None, t));
                }
            }
        }
        out.extend(self.shown.iter().map(|&(start, end)| (config_key(start), None, self.settings[start..end].to_string())));
        out
    }
}

/// The system pictures that can be written back: (path, the game's own bytes). They're those
/// Gameexe.dat names, which the engine draws itself (the message window's frame, the right-click
/// menu's), and those with "sys" in the name, as the test game's own menus are (`__sys_…`).
pub fn interface_pictures(game: &Path) -> R<Vec<(PathBuf, Vec<u8>)>> {
    static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""([^"]+)""#).unwrap());
    // without them, the pictures named "sys" still are
    let settings = original(&game.join(PACK)).and_then(|b| Pack::read(b, None)).and_then(|p| config_text(game, &p.key)).unwrap_or_default();
    let named: HashSet<String> = QUOTED.captures_iter(&settings).map(|c| c[1].to_lowercase()).collect();
    let wanted = |stem: String| stem.contains("sys") || named.contains(&stem);    let mut paths: Vec<PathBuf> = std::fs::read_dir(game.join("g00")).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("g00")) && p.file_stem().is_some_and(|n| wanted(n.to_string_lossy().to_lowercase())))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let b = original(&p)?;
        if crate::g00::writable(&b) {
            out.push((p, b));
        }
    }
    Ok(out)
}

// shortcut: for a game whose Gameexe.dat doesn't say; most of the box sizes seen run to about this
const LINE_LETTERS: usize = 56;

/// How many letters fit a line, from the game's own settings.
fn window(settings: &str) -> Option<usize> {
    static MOJI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?im)^#MWND\.000\.MOJI_CNT\s*=\s*(\d+)").unwrap());
    MOJI.captures(settings).and_then(|c| c[1].parse::<usize>().ok()).map(|n| n * 2)
}

/// Writes the translations (`line` by key, `name` by the game's own text) into Scene.pck and reads it
/// back. Returns how many strings were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let g = Game::open(game)?;
    let (mut new, mut done) = (Vec::new(), 0);
    for (n, scene, flow, strings) in &g.scenes {
        let mut changed = strings.clone();
        let mut count = 0;
        for (i, s) in strings.iter().enumerate() {
            let Some(tr) = line(&format!("{n}.{i}")) else { continue };
            if JA.is_match(&text(s)) {
                changed[i] = wrap_words(&tr, g.width).join("\n").encode_utf16().collect();
                count += 1;
            }
        }
        for who in flow.said.iter().filter_map(|s| s.1) {
            if let Some(tr) = strings.get(who).and_then(|s| name(&text(s))) {
                changed[who] = tr.encode_utf16().collect();
                count += 1;
            }
        }
        if count > 0 {
            new.push((*n, scene.with(&changed)?));
            done += count;
        }
    }
    let (settings, words) = with_english(&g.settings, &g.shown, &|start, _| line(&config_key(start)));
    let out = g.pack.with(&new)?;
    let back = Pack::read(out.clone(), Some(g.pack.key))?;
    for (n, data) in &new {
        if back.scene(*n)?.data != *data {
            return Err(format!("scene {n} didn't read back as written"));
        }
    }
    replace_file(&game.join(PACK), &out)?;
    if words > 0 {
        write_config(game, &g.pack.key, &settings)?;
        let mut back = std::fs::read(game.join(CONFIG)).map_err(|e| e.to_string())?;
        let body = back.get_mut(CONFIG_HEAD..).ok_or("truncated Gameexe.dat")?;
        crypt(body, &CONFIG_TABLE, &g.pack.key);
        let plain: Vec<u8> = settings.encode_utf16().flat_map(u16::to_le_bytes).collect();
        if unpack(body)? != plain {
            return Err("Gameexe.dat didn't read back as written".into());
        }
    }
    Ok(done + words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const KEY: [u8; 16] = [0x36, 0x0f, 0xc9, 0x6e, 0x2e, 0xd3, 0x8b, 0x0f, 0x66, 0xa5, 0x70, 0xc3, 0x23, 0xa3, 0x6e, 0x94];

    fn le(v: &[i32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// A scene of `strings` whose code speaks each, the first with the second as its speaker.
    fn scene(strings: &[&str]) -> Vec<u8> {
        let units: Vec<Vec<u16>> = strings.iter().map(|s| s.encode_utf16().collect()).collect();
        let mut code = Vec::new();
        code.push(2);
        code.extend(le(&[STRING_FORM, 1]));
        code.push(0x32);
        code.push(2);
        code.extend(le(&[STRING_FORM, 0]));
        code.push(0x31);
        code.extend(le(&[7]));
        for i in 2..strings.len() {
            code.push(2);
            code.extend(le(&[STRING_FORM, i as i32]));
            code.push(0x31);
            code.extend(le(&[9]));
        }
        code.extend([0x16]);
        let count = units.len() as i32;
        let index_at = 132;
        let data_at = index_at + 8 * count;
        let total: i32 = units.iter().map(|u| u.len() as i32).sum();
        let code_at = data_at + 2 * total;
        let end = code_at + code.len() as i32;
        let mut d = vec![0i32; 33];
        d[0] = 132;
        (d[1], d[2]) = (code_at, code.len() as i32);
        (d[3], d[4], d[5], d[6]) = (index_at, count, data_at, count);
        for i in (7..=31).step_by(2) {
            d[i] = end;
        }
        let mut b = le(&d);
        let mut at = 0;
        for u in &units {
            b.extend(le(&[at, u.len() as i32]));
            at += u.len() as i32;
        }
        for (i, u) in units.iter().enumerate() {
            b.extend(u.iter().flat_map(|c| (c ^ string_key(i)).to_le_bytes()));
        }
        b.extend(code);
        b
    }

    /// With `key`, as most games are; without, as the small test scenes need, having no byte mix to go by.
    fn pack(scenes: &[Vec<u8>], key: Option<[u8; 16]>) -> Vec<u8> {
        let mut bodies = Vec::new();
        let mut index = Vec::new();
        let mut at = 0;
        for s in scenes {
            let mut b = stored(s);
            crypt(&mut b, &SCENE_TABLE, &key.unwrap_or_default());
            index.extend(le(&[at, b.len() as i32]));
            at += b.len() as i32;
            bodies.extend(b);
        }
        let mut head = vec![0i32; 23];
        (head[17], head[18]) = (92, scenes.len() as i32);
        (head[19], head[20]) = (92 + index.len() as i32, scenes.len() as i32);
        head[21] = key.is_some() as i32;
        [le(&head), index, bodies, b"trailing".to_vec()].concat()
    }

    fn game(scenes: &[Vec<u8>], key: Option<[u8; 16]>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-siglus-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(PACK), pack(scenes, key)).unwrap();
        dir
    }

    const SETTINGS: &str = "\u{feff};\r\n#MWND.000.MOJI_CNT = 26\r\n#SYSCOMMENU.SAVE = 1, 1, \"保存\"\r\n#CHRKOE.000 = \"太郎\", 1\r\n; \"説明\"\r\n#DIALOG.SLEEP.STR = \"眠る\"\r\n#SAVEINFO.WARNING_STR = \"上書き\\n確認\"\r\n";

    fn settings_file(text: &str) -> Vec<u8> {
        let mut body = stored(&text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
        crypt(&mut body, &CONFIG_TABLE, &[0; 16]);
        [&[7, 0, 0, 0, 0, 0, 0, 0][..], &body].concat()
    }

    #[test]
    fn the_words_of_the_menus_change_and_the_other_settings_stay() {
        let dir = game(&[scene(&["こんにちは", "太郎"])], None);
        fs::write(dir.join(CONFIG), settings_file(SETTINGS)).unwrap();
        let g = Game::open(&dir).unwrap();
        assert_eq!(g.width, 52, "the window size still comes from the file");
        let lines = g.lines();
        let shown: Vec<&str> = lines.iter().filter(|l| l.0.starts_with("settings:")).map(|l| l.2.as_str()).collect();
        assert_eq!(shown, ["保存", "眠る"], "a voice's name, a comment and a string with a code stay");

        let line = |k: &str| k.starts_with("settings:").then(|| "Save \"now\"".to_string());
        assert_eq!(install(&dir, &line, &|_| None).unwrap(), 2);
        let written = fs::read(dir.join(CONFIG)).unwrap();
        assert_eq!(written[..CONFIG_HEAD], [7, 0, 0, 0, 0, 0, 0, 0]);
        let mut body = written[CONFIG_HEAD..].to_vec();
        crypt(&mut body, &CONFIG_TABLE, &[0; 16]);
        let units: Vec<u16> = unpack(&body).unwrap().chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), SETTINGS.replace("保存", "Save 'now'").replace("眠る", "Save 'now'"));

        assert_eq!(Game::open(&dir).unwrap().lines().iter().filter(|l| l.0.starts_with("settings:")).count(), 2, "reads the game's own");
        install(&dir, &line, &|_| None).unwrap();
        assert_eq!(fs::read(dir.join(CONFIG)).unwrap(), written);
        assert_eq!(fs::read(crate::backup(&dir.join(CONFIG))).unwrap(), settings_file(SETTINGS));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn packed_data_comes_back() {
        let plain = b"some bytes, more than eight of them".to_vec();
        assert_eq!(unpack(&stored(&plain)).unwrap(), plain);
        let b = [&(16u32.to_le_bytes())[..], &6u32.to_le_bytes(), &[0b0000_0111, b'a', b'b', b'c', 3 << 4 | 1, 0]].concat();
        assert_eq!(unpack(&b).unwrap(), b"abcabc");
    }

    #[test]
    fn the_key_comes_from_the_scenes_alone() {
        // long enough to have a mix of bytes, as real scenes do
        let words = "吾輩は猫である、名前はまだ無い。どこで生れたか頓と見当がつかぬ。".repeat(40);
        let scenes: Vec<Vec<u8>> = (0..6).map(|i| scene(&[&format!("{i}{words}"), "太郎", "ありがとう"])).collect();
        let dir = game(&scenes, Some(KEY));
        let pack = Pack::read(fs::read(dir.join(PACK)).unwrap(), None).unwrap();
        assert_eq!(pack.key, KEY);
        assert_eq!(pack.scene(3).unwrap().data, scenes[3]);
    }

    #[test]
    fn lines_have_speakers_and_translations_go_in() {
        let dir = game(&[scene(&["こんにちは", "太郎", "さようなら"]), scene(&["いいえ", "花子"])], None);
        let lines = Game::open(&dir).unwrap().lines();
        assert_eq!(lines[0], ("0.0".to_string(), Some("太郎".to_string()), "こんにちは".to_string()));
        assert_eq!(lines[1], ("0.2".to_string(), None, "さようなら".to_string()));

        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        let line = |k: &str| (k == "0.0").then(|| "word ".repeat(20));
        assert_eq!(install(&dir, &line, &name).unwrap(), 2);

        let out = fs::read(dir.join(PACK)).unwrap();
        assert!(out.ends_with(b"trailing"));
        let p = Pack::read(out, None).unwrap();
        let s = p.scene(0).unwrap().strings().unwrap();
        assert!(text(&s[0]).lines().count() > 1 && text(&s[0]).lines().all(|l| l.len() <= LINE_LETTERS));
        assert_eq!(text(&s[1]), "Taro");
        assert_eq!(text(&s[2]), "さようなら");
        assert_eq!(p.scene(1).unwrap().strings().unwrap()[0], "いいえ".encode_utf16().collect::<Vec<_>>());
    }
}
