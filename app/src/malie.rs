//! Malie engine (malie.exe, malie.ini, one big data.dat): LIBP archives encrypted with a modified
//! Camellia, each game with its own key, which is looked for in the game's own program.

use crate::pipeline::Line;
use crate::text::{wrap_words, JA};
use crate::{backup, extend_file, R};
use encoding_rs::SHIFT_JIS;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::LazyLock;

const DATA: &str = "data.dat";

// ---------- the cipher ----------

/// RFC 3713's first S-box. The other three are this one rotated.
const S1: [u8; 256] = [
    0x70, 0x82, 0x2c, 0xec, 0xb3, 0x27, 0xc0, 0xe5, 0xe4, 0x85, 0x57, 0x35, 0xea, 0x0c, 0xae, 0x41,
    0x23, 0xef, 0x6b, 0x93, 0x45, 0x19, 0xa5, 0x21, 0xed, 0x0e, 0x4f, 0x4e, 0x1d, 0x65, 0x92, 0xbd,
    0x86, 0xb8, 0xaf, 0x8f, 0x7c, 0xeb, 0x1f, 0xce, 0x3e, 0x30, 0xdc, 0x5f, 0x5e, 0xc5, 0x0b, 0x1a,
    0xa6, 0xe1, 0x39, 0xca, 0xd5, 0x47, 0x5d, 0x3d, 0xd9, 0x01, 0x5a, 0xd6, 0x51, 0x56, 0x6c, 0x4d,
    0x8b, 0x0d, 0x9a, 0x66, 0xfb, 0xcc, 0xb0, 0x2d, 0x74, 0x12, 0x2b, 0x20, 0xf0, 0xb1, 0x84, 0x99,
    0xdf, 0x4c, 0xcb, 0xc2, 0x34, 0x7e, 0x76, 0x05, 0x6d, 0xb7, 0xa9, 0x31, 0xd1, 0x17, 0x04, 0xd7,
    0x14, 0x58, 0x3a, 0x61, 0xde, 0x1b, 0x11, 0x1c, 0x32, 0x0f, 0x9c, 0x16, 0x53, 0x18, 0xf2, 0x22,
    0xfe, 0x44, 0xcf, 0xb2, 0xc3, 0xb5, 0x7a, 0x91, 0x24, 0x08, 0xe8, 0xa8, 0x60, 0xfc, 0x69, 0x50,
    0xaa, 0xd0, 0xa0, 0x7d, 0xa1, 0x89, 0x62, 0x97, 0x54, 0x5b, 0x1e, 0x95, 0xe0, 0xff, 0x64, 0xd2,
    0x10, 0xc4, 0x00, 0x48, 0xa3, 0xf7, 0x75, 0xdb, 0x8a, 0x03, 0xe6, 0xda, 0x09, 0x3f, 0xdd, 0x94,
    0x87, 0x5c, 0x83, 0x02, 0xcd, 0x4a, 0x90, 0x33, 0x73, 0x67, 0xf6, 0xf3, 0x9d, 0x7f, 0xbf, 0xe2,
    0x52, 0x9b, 0xd8, 0x26, 0xc8, 0x37, 0xc6, 0x3b, 0x81, 0x96, 0x6f, 0x4b, 0x13, 0xbe, 0x63, 0x2e,
    0xe9, 0x79, 0xa7, 0x8c, 0x9f, 0x6e, 0xbc, 0x8e, 0x29, 0xf5, 0xf9, 0xb6, 0x2f, 0xfd, 0xb4, 0x59,
    0x78, 0x98, 0x06, 0x6a, 0xe7, 0x46, 0x71, 0xba, 0xd4, 0x25, 0xab, 0x42, 0x88, 0xa2, 0x8d, 0xfa,
    0x72, 0x07, 0xb9, 0x55, 0xf8, 0xee, 0xac, 0x0a, 0x36, 0x49, 0x2a, 0x68, 0x3c, 0x38, 0xf1, 0xa4,
    0x40, 0x28, 0xd3, 0x7b, 0xbb, 0xc9, 0x43, 0xc1, 0x15, 0xe3, 0xad, 0xf4, 0x77, 0xc7, 0x80, 0x9e,
];
const SIGMA: [u32; 8] = [0xa09e667f, 0x3bcc908b, 0xb67ae858, 0x4caa73b2, 0xc6ef372f, 0xe94f82be, 0x54ff53a5, 0xf1d36f1c];

/// The S-boxes spread over four bytes: 1110, 0222, 3033 and 4404 in Camellia's usual names.
static SBOX: LazyLock<[[u32; 256]; 4]> = LazyLock::new(|| {
    let mut t = [[0; 256]; 4];
    for i in 0..256 {
        t[0][i] = S1[i] as u32 * 0x01010100;
        t[1][i] = S1[i].rotate_left(1) as u32 * 0x00010101;
        t[2][i] = S1[i].rotate_left(7) as u32 * 0x01000101;
        t[3][i] = S1[(i as u8).rotate_left(1) as usize] as u32 * 0x01010001;
    }
    t
});

fn mix(k0: u32, k1: u32, a: u32, b: u32) -> (u32, u32) {
    let s = &*SBOX;
    let byte = |x: u32, n: u32| (x >> n & 0xff) as usize;
    let t = k0 ^ a;
    let u = s[2][byte(t, 8)] ^ s[3][byte(t, 0)] ^ s[1][byte(t, 16)] ^ s[0][byte(t, 24)];
    let t = k1 ^ b;
    let d = s[3][byte(t, 8)] ^ s[0][byte(t, 0)] ^ s[2][byte(t, 16)] ^ s[1][byte(t, 24)];
    (u ^ d, u ^ d ^ u.rotate_right(8))
}

fn feistel(s0: u32, s1: u32, s2: &mut u32, s3: &mut u32, k: usize) {
    let s = &*SBOX;
    let byte = |x: u32, n: u32| (x >> n & 0xff) as usize;
    let (t0, t1) = (s0 ^ SIGMA[k], s1 ^ SIGMA[k + 1]);
    let mut t2 = s[0][byte(t1, 0)] ^ s[3][byte(t1, 8)];
    let mut t3 = s[3][byte(t0, 0)] ^ s[2][byte(t0, 8)] ^ s[1][byte(t0, 16)] ^ s[0][byte(t0, 24)];
    t2 ^= t3;
    t3 = t3.rotate_right(8);
    t2 ^= s[2][byte(t1, 16)] ^ s[1][byte(t1, 24)];
    *s3 ^= t3 ^ t2;
    *s2 ^= t2;
}

fn left128(s: &mut [u32; 4], n: u32) {
    let t = s[0] >> (32 - n);
    s[0] = s[0] << n | s[1] >> (32 - n);
    s[1] = s[1] << n | s[2] >> (32 - n);
    s[2] = s[2] << n | s[3] >> (32 - n);
    s[3] = s[3] << n | t;
}

#[derive(Clone)]
pub struct Cipher {
    k: [u32; 52],
}

impl Cipher {
    /// The table the game itself uses, from the 16 byte key it is made of.
    pub fn from_key(key: &[u8]) -> Cipher {
        let be = |i: usize| u32::from_be_bytes(key[i..i + 4].try_into().unwrap());
        let kl = [be(0), be(4), be(8), be(12)];
        let [mut s0, mut s1, mut s2, mut s3] = kl;
        feistel(s0, s1, &mut s2, &mut s3, 0);
        feistel(s2, s3, &mut s0, &mut s1, 2);
        s0 ^= kl[0];
        s1 ^= kl[1];
        s2 ^= kl[2];
        s3 ^= kl[3];
        feistel(s0, s1, &mut s2, &mut s3, 4);
        feistel(s2, s3, &mut s0, &mut s1, 6);

        let mut k = [0u32; 52];
        let mut put = |at: usize, s: &[u32]| k[at..at + s.len()].copy_from_slice(s);
        put(48, &kl);
        let mut a = [s0, s1, s2, s3];
        put(44, &a);
        left128(&mut a, 15);
        put(36, &a);
        left128(&mut a, 15);
        put(32, &a);
        left128(&mut a, 15);
        put(24, &a[..2]);
        left128(&mut a, 15);
        put(20, &a);
        let mut r = [a[1], a[2], a[3], a[0]];
        left128(&mut r, 2);
        put(8, &r);
        left128(&mut r, 17);
        put(0, &r);
        let mut l = kl;
        left128(&mut l, 15);
        put(40, &l);
        left128(&mut l, 30);
        put(28, &l);
        left128(&mut l, 15);
        put(26, &l[2..]);
        left128(&mut l, 17);
        put(16, &l);
        left128(&mut l, 17);
        put(12, &l);
        left128(&mut l, 17);
        put(4, &l);
        Cipher { k }
    }

    /// `at`, the block's place in the file, decides how far its words were rotated.
    fn roll(at: u64) -> u32 {
        (at >> 4 & 0xf) as u32 + 16
    }

    pub fn decrypt(&self, at: u64, b: &mut [u8]) {
        let (k, roll) = (&self.k, Self::roll(at));
        let word = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let mut d = [
            word(0).rotate_left(roll).swap_bytes() ^ k[0],
            word(4).rotate_right(roll).swap_bytes() ^ k[1],
            word(8).rotate_left(roll).swap_bytes() ^ k[2],
            word(12).rotate_right(roll).swap_bytes() ^ k[3],
        ];
        let mut p = 4;
        for layer in 0..3 {
            for _ in 0..3 {
                let (x, y) = mix(k[p + 2], k[p + 3], d[0], d[1]);
                d[2] ^= x;
                d[3] ^= y;
                let (x, y) = mix(k[p], k[p + 1], d[2], d[3]);
                d[0] ^= x;
                d[1] ^= y;
                p += 4;
            }
            if layer < 2 {
                d[1] ^= (d[0] & k[p + 2]).rotate_left(1);
                d[0] ^= d[1] | k[p + 3];
                d[2] ^= d[3] | k[p + 1];
                d[3] ^= (d[2] & k[p]).rotate_left(1);
                p += 4;
            }
        }
        let out = [d[2] ^ k[p], d[3] ^ k[p + 1], d[0] ^ k[p + 2], d[1] ^ k[p + 3]];
        for (i, w) in out.iter().enumerate() {
            b[4 * i..4 * i + 4].copy_from_slice(&w.to_be_bytes());
        }
    }

    /// `decrypt` run backwards, step by step.
    pub fn encrypt(&self, at: u64, b: &mut [u8]) {
        let (k, roll) = (&self.k, Self::roll(at));
        let word = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
        let mut d = [word(8) ^ k[50], word(12) ^ k[51], word(0) ^ k[48], word(4) ^ k[49]];
        let mut p = 48;
        for layer in (0..3).rev() {
            if layer < 2 {
                p -= 4;
                d[3] ^= (d[2] & k[p]).rotate_left(1);
                d[2] ^= d[3] | k[p + 1];
                d[0] ^= d[1] | k[p + 3];
                d[1] ^= (d[0] & k[p + 2]).rotate_left(1);
            }
            for _ in 0..3 {
                p -= 4;
                let (x, y) = mix(k[p], k[p + 1], d[2], d[3]);
                d[0] ^= x;
                d[1] ^= y;
                let (x, y) = mix(k[p + 2], k[p + 3], d[0], d[1]);
                d[2] ^= x;
                d[3] ^= y;
            }
        }
        let out = [
            (d[0] ^ k[0]).swap_bytes().rotate_right(roll),
            (d[1] ^ k[1]).swap_bytes().rotate_left(roll),
            (d[2] ^ k[2]).swap_bytes().rotate_right(roll),
            (d[3] ^ k[3]).swap_bytes().rotate_left(roll),
        ];
        for (i, w) in out.iter().enumerate() {
            b[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
    }
}

/// The game's key, found by trying every place in its program: the 16 bytes of a key, or the 52 words
/// of a finished table. `head` is the first block of the archive, which has to come out as LIBP.
pub fn find_key(exe: &[u8], head: &[u8]) -> R<Cipher> {
    let opens = |c: &Cipher| {
        let mut b = [0u8; 16];
        b.copy_from_slice(&head[..16]);
        c.decrypt(0, &mut b);
        &b[..4] == b"LIBP" || &b[..4] == b"LIBU"
    };
    for w in exe.windows(16) {
        let c = Cipher::from_key(w);
        if opens(&c) {
            return Ok(c);
        }
    }
    for w in exe.windows(52 * 4).step_by(4) {
        let mut k = [0u32; 52];
        for (v, b) in k.iter_mut().zip(w.as_chunks::<4>().0) {
            *v = u32::from_le_bytes(*b);
        }
        let c = Cipher { k };
        if opens(&c) {
            return Ok(c);
        }
    }
    Err("the key of this Malie game isn't in its program".into())
}

// ---------- LIBP archives ----------

fn u32_at(b: &[u8], p: usize) -> R<u32> {
    Ok(u32::from_le_bytes(b.get(p..p + 4).ok_or("truncated Malie file")?.try_into().unwrap()))
}

/// `len` bytes from `at`, whose place in the file need not start a block.
fn read_at(f: &mut fs::File, c: &Cipher, at: u64, len: usize) -> R<Vec<u8>> {
    let (start, lead) = (at & !15, (at & 15) as usize);
    let mut b = vec![0; (lead + len + 15) & !15];
    f.seek(SeekFrom::Start(start)).and_then(|_| f.read_exact(&mut b)).map_err(|e| format!("data.dat: {e}"))?;
    for (i, block) in b.as_chunks_mut::<16>().0.iter_mut().enumerate() {
        c.decrypt(start + 16 * i as u64, block);
    }
    b.drain(..lead);
    b.truncate(len);
    Ok(b)
}

/// One file of an archive.
pub struct Item {
    pub path: String,
    /// Where its entry sits in the index, and its slot in the table of offsets.
    entry: usize,
    slot: usize,
    pub size: u32,
}

/// An archive's index, decrypted. The data stays in the file: these can run to several gigabytes.
pub struct Lib {
    cipher: Cipher,
    index: Vec<u8>,
    offsets: Vec<u8>,
    /// Where the data starts; in a LIBU archive, where the index ends.
    base: u64,
    /// The older LIBU: a flat list of 80-byte entries, each with its own place in the file.
    libu: bool,
    pub items: Vec<Item>,
}

/// A LIBU entry: a UTF-16 name, then the size and the place in the file.
const LIBU_ROW: usize = 80;

impl Lib {
    pub fn open(f: &mut fs::File, cipher: Cipher) -> R<Lib> {
        let head = read_at(f, &cipher, 0, 16)?;
        if &head[..4] == b"LIBU" {
            let count = u32_at(&head, 8)? as usize;
            let index = read_at(f, &cipher, 16, LIBU_ROW * count)?;
            let mut items = Vec::with_capacity(count);
            for (entry, e) in index.as_chunks::<LIBU_ROW>().0.iter().enumerate() {
                let units: Vec<u16> = e[..68].as_chunks::<2>().0.iter().map(|&u| u16::from_le_bytes(u)).take_while(|&u| u != 0).collect();
                items.push(Item { path: String::from_utf16_lossy(&units), entry, slot: 0, size: u32_at(e, 68)? });
            }
            let base = (16 + (LIBU_ROW * count) as u64 + 15) & !15;
            return Ok(Lib { cipher, index, offsets: Vec::new(), base, libu: true, items });
        }
        if &head[..4] != b"LIBP" {
            return Err("this Malie archive isn't a LIBP or LIBU one".into());
        }
        let (count, slots) = (u32_at(&head, 4)? as usize, u32_at(&head, 8)? as usize);
        let index = read_at(f, &cipher, 16, 32 * count)?;
        let offsets = read_at(f, &cipher, 16 + 32 * count as u64, 4 * slots)?;
        let base = 16 + 32 * count as u64 + 4 * slots as u64;
        let mut lib = Lib { cipher, index, offsets, base: (base + 0x3ff) & !0x3ff, libu: false, items: Vec::new() };
        lib.walk("", 0, 1)?;
        // games align their data to 1 or 4 KB; only the right one finds a real script
        let script = lib.items.iter().find(|i| is_exec(&i.path)).ok_or("this Malie game has no exec.dat")?;
        if Exec::read(&lib.read(f, script)?).is_ok() {
            return Ok(lib);
        }
        lib.base = (base + 0xfff) & !0xfff;
        Ok(lib)
    }

    fn walk(&mut self, root: &str, first: usize, count: usize) -> R<()> {
        for entry in first..first + count {
            let e = self.index.get(32 * entry..32 * entry + 32).ok_or("bad LIBP index")?;
            let name = &e[..22];
            let name = SHIFT_JIS.decode_without_bom_handling(&name[..name.iter().position(|&c| c == 0).unwrap_or(22)]).0;
            let path = if root.is_empty() { name.into_owned() } else { format!("{root}/{name}") };
            let (flags, at, size) = (u16::from_le_bytes([e[22], e[23]]), u32_at(e, 24)? as usize, u32_at(e, 28)?);
            if flags & 3 != 0 {
                self.items.push(Item { path, entry, slot: at, size });
            } else if at > entry {
                self.walk(&path, at, size as usize)?;
            }
        }
        Ok(())
    }

    fn place(&self, item: &Item) -> R<u64> {
        if self.libu {
            return Ok(u32_at(&self.index, LIBU_ROW * item.entry + 72)? as u64);
        }
        Ok(self.base + ((u32_at(&self.offsets, 4 * item.slot)? as u64) << 10))
    }

    pub fn read(&self, f: &mut fs::File, item: &Item) -> R<Vec<u8>> {
        read_at(f, &self.cipher, self.place(item)?, item.size as usize)
    }
}

fn is_exec(path: &str) -> bool {
    path.to_lowercase().ends_with("exec.dat")
}

// ---------- the compiled script ----------

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl Cursor<'_> {
    fn u32(&mut self) -> R<u32> {
        self.p += 4;
        u32_at(self.b, self.p - 4)
    }

    fn bytes(&mut self, n: usize) -> R<&[u8]> {
        self.p += n;
        self.b.get(self.p - n..self.p).ok_or("truncated exec.dat".into())
    }

    /// A length, with the top bit set for UTF-16, then the name and its terminator.
    fn name(&mut self) -> R<String> {
        let n = self.u32()?;
        let (wide, n) = (n & 0x8000_0000 != 0, (n & 0x7fff_ffff) as usize);
        if n > self.b.len() {
            return Err("bad name in exec.dat".into());
        }
        let s = self.bytes(n)?;
        Ok(if wide {
            String::from_utf16_lossy(&units(&s[..n.saturating_sub(2)]))
        } else {
            SHIFT_JIS.decode_without_bom_handling(&s[..n.saturating_sub(1)]).0.into_owned()
        })
    }
}

fn units(b: &[u8]) -> Vec<u16> {
    b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
}

/// exec.dat: symbols, functions and labels, then the string segment, the code, and the messages.
/// Messages are referred to by number, so their table can be rebuilt without touching the code.
/// Names and choices live in the string segment, referred to by offset from the code.
pub struct Exec {
    head: Vec<u8>,
    pub strings: Vec<u8>,
    pub code: Vec<u8>,
    pub messages: Vec<Vec<u16>>,
    /// Function names, by the number the code calls them by.
    pub functions: Vec<String>,
    /// Functions that take an extra byte after the call.
    pub cdecl: HashSet<String>,
}

impl Exec {
    pub fn read(b: &[u8]) -> R<Exec> {
        let mut c = Cursor { b, p: 0 };
        let mut cdecl = HashSet::new();
        for _ in 0..c.u32()? {
            let name = c.name()?;
            while c.u32()? != 0 {
                c.u32()?;
            }
            c.bytes(12)?;
            if c.u32()? == 1 {
                cdecl.insert(name);
            }
        }
        c.u32()?;
        let mut functions = Vec::new();
        for _ in 0..c.u32()? {
            functions.push(c.name()?);
            c.bytes(12)?;
        }
        for _ in 0..c.u32()? {
            c.name()?;
            c.u32()?;
        }
        let head = b[..c.p].to_vec();
        let n = c.u32()? as usize;
        let strings = c.bytes(n)?.to_vec();
        let n = c.u32()? as usize;
        let code = c.bytes(n)?.to_vec();
        let n = c.u32()? as usize;
        let table: Vec<(usize, usize)> = (0..n).map(|_| Ok((c.u32()? as usize, c.u32()? as usize))).collect::<R<_>>()?;
        let n = c.u32()? as usize;
        let data = c.bytes(n)?;
        let messages = table.iter().map(|&(at, len)| Ok(units(data.get(at..at + len).ok_or("bad message in exec.dat")?))).collect::<R<_>>()?;
        if c.p != b.len() {
            return Err("exec.dat has data after its messages".into());
        }
        Ok(Exec { head, strings, code, messages, functions, cdecl })
    }

    pub fn write(&self) -> Vec<u8> {
        let mut out = self.head.clone();
        out.extend((self.strings.len() as u32).to_le_bytes());
        out.extend(&self.strings);
        out.extend((self.code.len() as u32).to_le_bytes());
        out.extend(&self.code);
        out.extend((self.messages.len() as u32).to_le_bytes());
        let mut data = Vec::new();
        for m in &self.messages {
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((2 * m.len() as u32).to_le_bytes());
            data.extend(m.iter().flat_map(|u| u.to_le_bytes()));
            data.extend([0, 0]); // not counted in the table, but the game's own text has it
        }
        out.extend((data.len() as u32).to_le_bytes());
        out.extend(data);
        out
    }
}

/// What the code does with messages and names, in the order it does it.
#[derive(Default)]
pub struct Flow {
    /// Message numbers, with the speaker named just before each.
    pub said: Vec<(usize, Option<String>)>,
    /// Where the code loads a name or a choice: (operand's place, operand's size, the text).
    pub shown: Vec<(usize, usize, String)>,
}

impl Exec {
    fn string(&self, at: usize) -> R<String> {
        let tail = units(self.strings.get(at..).ok_or("bad string offset in exec.dat")?);
        Ok(String::from_utf16_lossy(&tail[..tail.iter().position(|&c| c == 0).unwrap_or(tail.len())]))
    }

    /// Walks the code. An opcode nobody documented means the walk has lost its place, so that's an error.
    pub fn flow(&self) -> R<Flow> {
        let (code, mut p) = (&self.code, 0);
        let (mut flow, mut pushed, mut speaker, mut loaded) = (Flow::default(), 0u32, None, None);
        let bad = || "unknown code in exec.dat".to_string();
        let word = |p: usize| Ok::<_, String>(u32::from_le_bytes(code.get(p..p + 4).ok_or_else(bad)?.try_into().unwrap()));
        while p < code.len() {
            let op = code[p];
            p += 1;
            match op {
                0 | 1 | 2 | 8 | 0x31 => p += 4,
                3 => p += 5,
                4 => p += 2,
                0x0d => {
                    pushed = word(p)?;
                    p += 4;
                }
                0x0f => pushed = 0,
                0x11 => {
                    pushed = *code.get(p).ok_or_else(bad)? as u32;
                    p += 1;
                }
                0x33 => p += 1,
                9 | 0x0a | 0x0c => {
                    let size = [1, 2, 0, 4][op as usize - 9];
                    let mut v = [0; 4];
                    v[..size].copy_from_slice(code.get(p..p + size).ok_or_else(bad)?);
                    loaded = Some((p, size, u32::from_le_bytes(v) as usize));
                    p += size;
                }
                0x2d => {
                    let name = self.functions.get(word(p)? as usize).ok_or_else(bad)?;
                    p += 4 + self.cdecl.contains(name) as usize;
                    let string = loaded.take().map(|(at, size, off)| Ok::<_, String>((at, size, self.string(off)?))).transpose()?;
                    match (name.as_str(), string) {
                        ("_ms_message", _) => flow.said.push((pushed as usize, speaker.take())),
                        ("MALIE_NAME", Some(s)) => {
                            speaker = Some(s.2.clone());
                            flow.shown.push(s);
                        }
                        // a choice: MALIE_ADDSELECT is a macro for this call
                        ("FrameLayer_SendMessage", Some(s)) if JA.is_match(&s.2) => flow.shown.push(s),
                        _ => {}
                    }
                }
                5..=7 | 0x0b | 0x0e | 0x10 | 0x12..=0x2c | 0x2e..=0x30 | 0x32 => {}
                _ => return Err(bad()),
            }
        }
        if p != code.len() {
            return Err(bad());
        }
        Ok(flow)
    }
}

// ---------- text ----------

/// A message is text and control codes. `raw` keeps the text with its ruby, for what isn't translated.
#[derive(Debug, PartialEq)]
pub enum Part {
    Code(Vec<u16>),
    Text { plain: String, raw: Vec<u16> },
}

const RUBY: u16 = 1;
const VOICE: u16 = 8;
const ALIGN: u16 = 2;
const FONT: u16 = 13;

/// How many units the code at `m[i]` takes, for codes that aren't text-shaped.
fn code_len(m: &[u16], i: usize) -> usize {
    let until_nul = |from: usize| m[from..].iter().position(|&c| c == 0).map_or(m.len() - from, |n| n + 1);
    match (m[i], m.get(i + 1).copied()) {
        (7, Some(VOICE)) => 2 + until_nul(i + 2).min(m.len() - i - 2),
        (7, Some(ALIGN | FONT)) => 3,
        (7, _) => 2,
        (1, _) => 4,
        _ => 1,
    }
}

/// Text between codes. Ruby comes out as its base text, with the reading left behind, and line breaks
/// go: the English gets its own.
pub fn parts(m: &[u16]) -> Vec<Part> {
    let (mut out, mut plain, mut raw, mut i) = (Vec::new(), Vec::new(), Vec::new(), 0);
    let flush = |out: &mut Vec<Part>, plain: &mut Vec<u16>, raw: &mut Vec<u16>| {
        if !raw.is_empty() {
            out.push(Part::Text { plain: String::from_utf16_lossy(plain), raw: std::mem::take(raw) });
            plain.clear();
        }
    };
    while i < m.len() {
        if m[i] == 7 && m.get(i + 1) == Some(&RUBY) {
            let end = m[i..].iter().position(|&c| c == 0).map_or(m.len(), |n| i + n + 1);
            plain.extend(m[i + 2..end].iter().take_while(|&&c| c != 10));
            raw.extend(&m[i..end]);
            i = end;
        } else if m[i] == 10 || m[i] == 13 {
            raw.push(m[i]);
            i += 1;
        } else if m[i] < 32 {
            flush(&mut out, &mut plain, &mut raw);
            let end = (i + code_len(m, i)).min(m.len());
            out.push(Part::Code(m[i..end].to_vec()));
            i = end;
        } else {
            plain.push(m[i]);
            raw.push(m[i]);
            i += 1;
        }
    }
    flush(&mut out, &mut plain, &mut raw);
    out
}

// shortcut: the box isn't known from the files, so this is what the game's longest lines suggest for
// its 30 or so full-width characters; make it per game once one shows otherwise.
const LINE_LETTERS: usize = 56;

fn with_breaks(text: &str, width: usize) -> Vec<u16> {
    wrap_words(text, width).join("\n").encode_utf16().collect()
}

/// The game's script: where its messages sit, and its lines as the pipeline wants them: (key, speaker
/// as the game writes it, text). Keys are `message.piece`.
pub struct Game {
    file: fs::File,
    lib: Lib,
    script: usize,
    pub exec: Exec,
    len: u64,
}

impl Game {
    /// Always the game's own text: the backup, once there is one.
    pub fn open(game: &Path) -> R<Game> {
        let path = game.join(DATA);
        let own = if backup(&path).exists() { backup(&path) } else { path };
        let mut file = fs::File::open(&own).map_err(|e| format!("{}: {e}", own.display()))?;
        let len = file.metadata().map_err(|e| e.to_string())?.len();
        let mut head = [0; 16];
        file.read_exact(&mut head).map_err(|e| format!("{}: {e}", own.display()))?;
        let mut cipher = Err("this Malie game has no program".to_string());
        for e in fs::read_dir(game).map_err(|e| e.to_string())?.flatten() {
            if cipher.is_err() && e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")) {
                cipher = find_key(&fs::read(e.path()).map_err(|e| e.to_string())?, &head);
            }
        }
        let lib = Lib::open(&mut file, cipher?)?;
        let script = lib.items.iter().position(|i| is_exec(&i.path)).ok_or("this Malie game has no exec.dat")?;
        let exec = Exec::read(&lib.read(&mut file, &lib.items[script])?)?;
        Ok(Game { file, lib, script, exec, len })
    }

    pub fn lines(&self) -> R<Vec<Line>> {
        let mut out = Vec::new();
        for (n, speaker) in self.exec.flow()?.said {
            let pieces = self.exec.messages.get(n).map(|m| parts(m)).unwrap_or_default();
            let texts = pieces.iter().filter_map(|p| if let Part::Text { plain, .. } = p { Some(plain) } else { None });
            for (i, plain) in texts.enumerate().filter(|(_, t)| JA.is_match(t)) {
                out.push((format!("{n}.{i}"), speaker.clone(), plain.clone()));
            }
        }
        Ok(out)
    }

    /// Puts the translations in `exec`: `line` by key, `name` by the game's own text. Returns how many.
    fn apply(&mut self, width: usize, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
        let mut done = 0;
        let flow = self.exec.flow()?;
        for (n, _) in flow.said {
            let Some(m) = self.exec.messages.get(n) else { continue };
            let (mut out, mut piece) = (Vec::new(), 0);
            for part in parts(m) {
                match part {
                    Part::Code(c) => out.extend(c),
                    Part::Text { raw, .. } => {
                        match line(&format!("{n}.{piece}")) {
                            Some(tr) => {
                                out.extend(with_breaks(&tr, width));
                                done += 1;
                            }
                            None => out.extend(raw),
                        }
                        piece += 1;
                    }
                }
            }
            self.exec.messages[n] = out;
        }
        // New strings go on the end, so that every old offset stays good. Short operands can only
        // reach so far, and the code can't grow: a name out of reach stays Japanese.
        let mut placed = HashMap::new();
        for (at, size, old) in flow.shown {
            let Some(tr) = name(&old) else { continue };
            let offset = *placed.entry(old).or_insert_with(|| {
                let offset = self.exec.strings.len();
                self.exec.strings.extend(tr.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()));
                offset
            });
            if offset < 1 << (8 * size).min(31) {
                self.exec.code[at..at + size].copy_from_slice(&offset.to_le_bytes()[..size]);
                done += 1;
            }
        }
        Ok(done)
    }

    /// The new script goes on the end of the archive, and the index is pointed at it last, so a run
    /// cut short leaves a game that still plays. shortcut: the archive can be many gigabytes, so this
    /// neither reads it whole nor stamps it, and a game update isn't told from our own file.
    fn write(&mut self, game: &Path) -> R<()> {
        let bytes = self.exec.write();
        let item = &self.lib.items[self.script];
        let (base, cipher) = (self.lib.base, &self.lib.cipher);
        let mut head = read_at(&mut self.file, cipher, 0, base as usize)?;
        let original = head.clone();
        let at = base + ((self.len - base + 0x3ff) & !0x3ff);
        if self.lib.libu {
            let row = 16 + LIBU_ROW * item.entry;
            let at = u32::try_from(at).map_err(|_| "this LIBU archive is past 4 GB, where its index can't point")?;
            head[row + 68..][..4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            head[row + 72..][..4].copy_from_slice(&at.to_le_bytes());
        } else {
            let slots = 16 + self.lib.index.len();
            head[slots + 4 * item.slot..][..4].copy_from_slice(&(((at - base) >> 10) as u32).to_le_bytes());
            head[16 + 32 * item.entry + 28..][..4].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        }
        let encrypted = |plain: &[u8], from: u64| {
            let mut b = plain.to_vec();
            b.resize((plain.len() + 15) & !15, 0);
            for (i, block) in b.as_chunks_mut::<16>().0.iter_mut().enumerate() {
                cipher.encrypt(from + 16 * i as u64, block);
            }
            b
        };
        extend_file(&game.join(DATA), at, &encrypted(&original, 0), &encrypted(&head, 0), &encrypted(&bytes, at))
    }
}

/// Writes the translations into the game and reads them back. Returns how many were written.
pub fn install(game: &Path, line: &dyn Fn(&str) -> Option<String>, name: &dyn Fn(&str) -> Option<String>) -> R<usize> {
    let mut g = Game::open(game)?;
    let done = g.apply(crate::fit::letters(game, LINE_LETTERS), line, name)?;
    g.write(game)?;
    let mut f = fs::File::open(game.join(DATA)).map_err(|e| e.to_string())?;
    let lib = Lib::open(&mut f, g.lib.cipher.clone())?;
    let script = lib.items.iter().find(|i| is_exec(&i.path)).ok_or("the new archive lost its exec.dat")?;
    if lib.read(&mut f, script)? != g.exec.write() {
        return Err("the script read back from data.dat isn't what was written; use Remove translation".into());
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    const KEY: &[u8; 16] = b"invented test ke";

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn name(s: &str) -> Vec<u8> {
        let mut b: Vec<u8> = (s.len() as u32 + 1).to_le_bytes().to_vec();
        b.extend(s.as_bytes());
        b.push(0);
        b
    }

    /// A tiny script: "太郎" says message 0, then nobody says message 1.
    fn script(first: &str, second: &str) -> Vec<u8> {
        let mut b = vec![0; 8]; // no symbols, and the word after them
        b.extend(3u32.to_le_bytes());
        for f in ["MALIE_NAME", "_ms_message", "other"] {
            b.extend(name(f));
            b.extend([0; 12]);
        }
        b.extend(0u32.to_le_bytes());
        let strings: Vec<u8> = utf16("太郎\0").iter().flat_map(|u| u.to_le_bytes()).collect();
        b.extend((strings.len() as u32).to_le_bytes());
        b.extend(strings);
        let mut code = vec![0x0a, 0, 0, 0x2d, 0, 0, 0, 0, 0x11, 0, 0x2d, 1, 0, 0, 0];
        code.extend([0x11, 1, 0x2d, 1, 0, 0, 0, 5]);
        b.extend((code.len() as u32).to_le_bytes());
        b.extend(code);
        let messages = [utf16(first), utf16(second)];
        b.extend(2u32.to_le_bytes());
        let mut data = Vec::new();
        for m in &messages {
            b.extend((data.len() as u32).to_le_bytes());
            b.extend((2 * m.len() as u32).to_le_bytes());
            data.extend(m.iter().flat_map(|u| u.to_le_bytes()));
            data.extend([0, 0]);
        }
        b.extend((data.len() as u32).to_le_bytes());
        b.extend(data);
        b
    }

    /// A game folder: a program holding the key, and an archive holding only `exec`.
    fn game(exec: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl-malie-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("malie.exe"), [&[9u8; 32][..], KEY].concat()).unwrap();
        let mut plain = b"LIBP\x02\0\0\0\x01\0\0\0\0\0\0\0".to_vec();
        let entry = |name: &str, flags: u16, at: u32, size: u32| {
            let mut e = [0u8; 32];
            e[..name.len()].copy_from_slice(name.as_bytes());
            e[22..24].copy_from_slice(&flags.to_le_bytes());
            e[24..28].copy_from_slice(&at.to_le_bytes());
            e[28..32].copy_from_slice(&size.to_le_bytes());
            e
        };
        plain.extend(entry("", 0, 1, 1));
        plain.extend(entry("exec.dat", 1, 0, exec.len() as u32));
        plain.extend(0u32.to_le_bytes());
        plain.resize(0x400, 0);
        plain.extend(exec);
        plain.resize((plain.len() + 0x3ff) & !0x3ff, 0);
        let c = Cipher::from_key(KEY);
        for (i, block) in plain.as_chunks_mut::<16>().0.iter_mut().enumerate() {
            c.encrypt(16 * i as u64, block);
        }
        fs::write(dir.join(DATA), plain).unwrap();
        dir
    }

    /// An older LIBU archive holding `exec`, after a file before it.
    fn libu_game(exec: &[u8]) -> PathBuf {
        let dir = game(exec);
        let mut plain = b"LIBU\0\0\x01\0\x02\0\0\0\0\0\0\0".to_vec();
        let entry = |name: &str, size: usize, at: usize| {
            let mut e = [0u8; LIBU_ROW];
            for (i, u) in name.encode_utf16().enumerate() {
                e[2 * i..2 * i + 2].copy_from_slice(&u.to_le_bytes());
            }
            e[68..72].copy_from_slice(&(size as u32).to_le_bytes());
            e[72..76].copy_from_slice(&(at as u32).to_le_bytes());
            e
        };
        let first = 16 + 2 * LIBU_ROW;
        plain.extend(entry("other.dat", 5, first));
        plain.extend(entry("data/exec.dat", exec.len(), first + 16));
        plain.extend(b"hello");
        plain.resize(first + 16, 0);
        plain.extend(exec);
        plain.resize((plain.len() + 15) & !15, 0);
        let c = Cipher::from_key(KEY);
        for (i, block) in plain.as_chunks_mut::<16>().0.iter_mut().enumerate() {
            c.encrypt(16 * i as u64, block);
        }
        fs::write(dir.join(DATA), plain).unwrap();
        dir
    }

    #[test]
    fn an_older_libu_archive_takes_its_translation_too() {
        let dir = libu_game(&script("こんにちは\u{7}\u{6}", "さようなら\u{7}\u{6}"));
        assert_eq!(Game::open(&dir).unwrap().lines().unwrap().len(), 2);
        let line = |k: &str| (k == "0.0").then(|| "Hello.".to_string());
        assert_eq!(install(&dir, &line, &|_| None).unwrap(), 1);
        let g = Game::open(&dir).unwrap();
        let mut f = fs::File::open(dir.join(DATA)).unwrap();
        let lib = Lib::open(&mut f, g.lib.cipher.clone()).unwrap();
        assert_eq!(lib.read(&mut f, &lib.items[0]).unwrap(), b"hello", "the file before it stays");
        let ours = Exec::read(&lib.read(&mut f, &lib.items[1]).unwrap()).unwrap();
        assert_eq!(ours.messages[0], utf16("Hello.\u{7}\u{6}"));
    }

    #[test]
    fn a_script_rebuilds_to_the_same_bytes() {
        let b = script("一", "二");
        assert_eq!(Exec::read(&b).unwrap().write(), b);
    }

    #[test]
    fn lines_come_in_story_order_with_their_speakers() {
        let dir = game(&script("こんにちは", "\u{7}\u{8}voice\0さようなら\u{7}\u{6}"));
        let lines = Game::open(&dir).unwrap().lines().unwrap();
        assert_eq!(lines, [("0.0".into(), Some("太郎".into()), "こんにちは".into()), ("1.0".into(), None, "さようなら".into())]);
    }

    #[test]
    fn ruby_gives_the_model_its_base_and_codes_stay_out() {
        let m = utf16("\u{7}\u{8}v1\0前\u{7}\u{1}漢字\nかんじ\0後\u{7}\u{4}次\u{7}\u{6}");
        let p = parts(&m);
        let plain: Vec<&str> = p.iter().filter_map(|x| if let Part::Text { plain, .. } = x { Some(plain.as_str()) } else { None }).collect();
        assert_eq!(plain, ["前漢字後", "次"]);
        let back: Vec<u16> = p.into_iter().flat_map(|x| match x { Part::Code(c) => c, Part::Text { raw, .. } => raw }).collect();
        assert_eq!(back, m);
    }

    #[test]
    fn translations_go_in_and_read_back_and_the_original_is_kept() {
        let dir = game(&script("こんにちは\u{7}\u{6}", "さようなら\u{7}\u{6}"));
        let before = fs::read(dir.join(DATA)).unwrap();
        let line = |k: &str| (k == "0.0").then(|| "Hello.".to_string());
        let name = |n: &str| (n == "太郎").then(|| "Taro".to_string());
        assert_eq!(install(&dir, &line, &name).unwrap(), 2);
        assert_eq!(fs::read(backup(&dir.join(DATA))).unwrap(), before);

        let once = fs::read(dir.join(DATA)).unwrap();
        let g = Game::open(&dir).unwrap();
        assert_eq!(g.exec.messages[0], utf16("こんにちは\u{7}\u{6}")); // the game's own text, not ours
        let mut f = fs::File::open(dir.join(DATA)).unwrap();
        let lib = Lib::open(&mut f, g.lib.cipher.clone()).unwrap();
        let ours = Exec::read(&lib.read(&mut f, &lib.items[0]).unwrap()).unwrap();
        assert_eq!(ours.messages[0], utf16("Hello.\u{7}\u{6}"));
        assert_eq!(ours.messages[1], utf16("さようなら\u{7}\u{6}"));
        assert_eq!(ours.flow().unwrap().shown[0].2, "Taro");

        install(&dir, &line, &name).unwrap(); // a second run starts from the original, not from itself
        assert_eq!(fs::read(dir.join(DATA)).unwrap(), once);
    }

    #[test]
    fn a_name_out_of_reach_stays_japanese() {
        let mut e = Exec::read(&script("一", "二")).unwrap();
        e.strings.resize(e.strings.len() + 0x10000, 1);
        let bytes = e.write();
        let dir = game(&bytes);
        let name = |_: &str| Some("Taro".to_string());
        assert_eq!(install(&dir, &|_| None, &name).unwrap(), 0);
        assert_eq!(Exec::read(&bytes).unwrap().flow().unwrap().shown[0].2, "太郎");
    }

    #[test]
    fn a_block_comes_back_at_every_position() {
        let c = Cipher::from_key(b"invented test ke");
        for at in (0..256).step_by(16) {
            let plain: Vec<u8> = (0..16).map(|i| (i as u8 * 7) ^ at as u8).collect();
            let mut b = plain.clone();
            c.encrypt(at, &mut b);
            assert_ne!(b, plain);
            c.decrypt(at, &mut b);
            assert_eq!(b, plain);
        }
    }

    #[test]
    fn the_key_is_found_in_the_program() {
        let key = b"invented test ke";
        let mut head = *b"LIBP\x10\0\0\0\x08\0\0\0\0\0\0\0";
        Cipher::from_key(key).encrypt(0, &mut head);
        let exe = [&[1u8; 40][..], key, &[2; 40]].concat();
        assert!(find_key(&exe, &head).is_ok());
        assert!(find_key(&[3; 200], &head).is_err());
    }
}
