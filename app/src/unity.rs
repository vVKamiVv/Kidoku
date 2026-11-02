//! Unity: read and rewrite UnityFS asset bundles and the serialized files inside them. Objects are
//! walked with the type trees the bundles carry, so no game code is needed to find their strings.

use crate::{backup, files_under, original, replace_file, stamp, R};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

// ---------- byte reader ----------

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
    le: bool,
}

macro_rules! num {
    ($name:ident, $t:ty) => {
        fn $name(&mut self) -> R<$t> {
            let a = self.take(size_of::<$t>())?.try_into().unwrap();
            Ok(if self.le { <$t>::from_le_bytes(a) } else { <$t>::from_be_bytes(a) })
        }
    };
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8], le: bool) -> Self {
        Rd { b, p: 0, le }
    }
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let s = self.b.get(self.p..self.p + n).ok_or("unexpected end of Unity data")?;
        self.p += n;
        Ok(s)
    }
    num!(u16, u16);
    num!(u32, u32);
    num!(i32, i32);
    num!(i64, i64);
    num!(u64, u64);
    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    fn cstr(&mut self) -> R<String> {
        let len = self.b[self.p..].iter().position(|&c| c == 0).ok_or("unterminated string")?;
        let s = String::from_utf8_lossy(self.take(len)?).into_owned();
        self.p += 1;
        Ok(s)
    }
    fn align(&mut self, n: usize) {
        self.p = self.p.div_ceil(n) * n;
    }
}

fn pad(v: &mut Vec<u8>, n: usize) {
    v.resize(v.len().div_ceil(n) * n, 0);
}

// ---------- UnityFS bundles ----------

pub struct Bundle {
    version: u32,
    unity: String,
    revision: String,
    pub files: Vec<Node>,
}

pub struct Node {
    pub path: String,
    flags: u32,
    pub data: Vec<u8>,
}

fn decompress(b: &[u8], size: usize, kind: u32) -> R<Vec<u8>> {
    match kind {
        0 => Ok(b.to_vec()),
        2 | 3 => lz4_flex::block::decompress(b, size).map_err(|e| format!("LZ4: {e}")),
        k => Err(format!("unsupported Unity bundle compression {k} (1 = LZMA)")),
    }
}

pub fn read_bundle(b: &[u8]) -> R<Bundle> {
    let mut r = Rd::new(b, false);
    if r.cstr()? != "UnityFS" {
        return Err("not a UnityFS bundle".into());
    }
    let (version, unity, revision) = (r.u32()?, r.cstr()?, r.cstr()?);
    let (_size, csize, usize_, flags) = (r.i64()?, r.u32()? as usize, r.u32()? as usize, r.u32()?);
    if version >= 7 {
        r.align(16);
    }
    let raw_info = if flags & 0x80 != 0 { b.get(b.len() - csize..).ok_or("bad bundle")? } else { r.take(csize)? };
    let info = decompress(raw_info, usize_, flags & 0x3f)?;
    if flags & 0x200 != 0 {
        r.align(16);
    }
    let mut i = Rd::new(&info, false);
    i.take(16)?; // hash
    let mut data = Vec::new();
    for _ in 0..i.i32()? {
        let (u, c, f) = (i.u32()? as usize, i.u32()? as usize, i.u16()?);
        data.extend(decompress(r.take(c)?, u, (f & 0x3f) as u32)?);
    }
    let mut files = Vec::new();
    for _ in 0..i.i32()? {
        let (off, size, flags, path) = (i.i64()? as usize, i.i64()? as usize, i.u32()?, i.cstr()?);
        let data = data.get(off..off + size).ok_or("bundle file out of range")?.to_vec();
        files.push(Node { path, flags, data });
    }
    Ok(Bundle { version, unity, revision, files })
}

impl Bundle {
    /// The bundle again, LZ4-compressed in 128 KiB blocks like Unity builds them.
    pub fn write(&self) -> Vec<u8> {
        let data: Vec<u8> = self.files.iter().flat_map(|f| f.data.iter().copied()).collect();
        let mut info = vec![0u8; 16];
        let mut body = Vec::new();
        let chunks: Vec<&[u8]> = data.chunks(1 << 17).collect();
        info.extend((chunks.len() as i32).to_be_bytes());
        for c in chunks {
            let z = lz4_flex::block::compress(c);
            info.extend((c.len() as u32).to_be_bytes());
            info.extend((z.len() as u32).to_be_bytes());
            info.extend(2u16.to_be_bytes());
            body.extend(z);
        }
        info.extend((self.files.len() as i32).to_be_bytes());
        let mut off = 0i64;
        for f in &self.files {
            info.extend(off.to_be_bytes());
            info.extend((f.data.len() as i64).to_be_bytes());
            info.extend(f.flags.to_be_bytes());
            info.extend(f.path.as_bytes());
            info.push(0);
            off += f.data.len() as i64;
        }
        let mut out = b"UnityFS\0".to_vec();
        out.extend(self.version.to_be_bytes());
        for s in [&self.unity, &self.revision] {
            out.extend(s.as_bytes());
            out.push(0);
        }
        let size_at = out.len();
        out.extend(0i64.to_be_bytes());
        out.extend((info.len() as u32).to_be_bytes());
        out.extend((info.len() as u32).to_be_bytes());
        // directory info present, stored uncompressed; format 7 added the padding around it, flagged by 0x200
        let padded = self.version >= 7;
        out.extend((if padded { 0x240u32 } else { 0x40u32 }).to_be_bytes());
        if padded {
            pad(&mut out, 16);
        }
        out.extend(info);
        if padded {
            pad(&mut out, 16);
        }
        out.extend(body);
        let n = out.len() as i64;
        out[size_at..size_at + 8].copy_from_slice(&n.to_be_bytes());
        out
    }
}

// ---------- serialized files ----------

const COMMON: &str = "AABB\0AnimationClip\0AnimationCurve\0AnimationState\0Array\0Base\0BitField\0bitset\0bool\0\
char\0ColorRGBA\0Component\0data\0deque\0double\0dynamic_array\0FastPropertyName\0first\0float\0Font\0GameObject\0\
Generic Mono\0GradientNEW\0GUID\0GUIStyle\0int\0list\0long long\0map\0Matrix4x4f\0MdFour\0MonoBehaviour\0MonoScript\0\
m_ByteSize\0m_Curve\0m_EditorClassIdentifier\0m_EditorHideFlags\0m_Enabled\0m_ExtensionPtr\0m_GameObject\0m_Index\0\
m_IsArray\0m_IsStatic\0m_MetaFlag\0m_Name\0m_ObjectHideFlags\0m_PrefabInternal\0m_PrefabParentObject\0m_Script\0\
m_StaticEditorFlags\0m_Type\0m_Version\0Object\0pair\0PPtr<Component>\0PPtr<GameObject>\0PPtr<Material>\0\
PPtr<MonoBehaviour>\0PPtr<MonoScript>\0PPtr<Object>\0PPtr<Prefab>\0PPtr<Sprite>\0PPtr<TextAsset>\0PPtr<Texture>\0\
PPtr<Texture2D>\0PPtr<Transform>\0Prefab\0Quaternionf\0Rectf\0RectInt\0RectOffset\0second\0set\0short\0size\0SInt16\0\
SInt32\0SInt64\0SInt8\0staticvector\0string\0TextAsset\0TextMesh\0Texture\0Texture2D\0Transform\0TypelessData\0\
UInt16\0UInt32\0UInt64\0UInt8\0unsigned int\0unsigned long long\0unsigned short\0vector\0Vector2f\0Vector3f\0\
Vector4f\0m_ScriptingClassIdentifier\0Gradient\0Type*\0int2_storage\0int3_storage\0BoundsInt\0\
m_CorrespondingSourceObject\0m_PrefabInstance\0m_PrefabAsset\0FileSize\0Hash128\0RenderingLayerMask\0";

/// A name from a type tree's own string buffer, or (high bit set) from Unity's built-in list.
fn tree_string(local: &[u8], off: u32) -> String {
    let (buf, at) = if off & 0x8000_0000 != 0 { (COMMON.as_bytes(), (off & 0x7fff_ffff) as usize) } else { (local, off as usize) };
    let s = buf.get(at..).unwrap_or_default();
    String::from_utf8_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]).into_owned()
}

pub struct TNode {
    pub level: u8,
    pub ty: String,
    pub name: String,
    size: i32,
    array: bool,
    align: bool,
}

pub struct Type {
    pub class_id: i32,
    pub nodes: Vec<TNode>,
}

pub struct Obj {
    pub path_id: i64,
    pub ty: usize,
    start: usize,
    size: usize,
    entry: usize, // where byteStart is written in the metadata
}

pub struct Serialized {
    raw: Vec<u8>,
    version: u32,
    pub le: bool,
    data_offset: usize,
    pub types: Vec<Type>,
    pub objects: Vec<Obj>,
}

pub fn read_serialized(raw: Vec<u8>) -> R<Serialized> {
    let mut r = Rd::new(&raw, false);
    let (_meta, _size, version, mut data_offset) = (r.u32()?, r.u32()?, r.u32()?, r.u32()? as u64);
    if !(17..=22).contains(&version) {
        return Err(format!("unsupported Unity serialized file version {version}"));
    }
    let le = r.u8()? == 0;
    r.take(3)?;
    if version >= 22 {
        (_, _, data_offset, _) = (r.u32()?, r.u64()?, r.u64()?, r.u64()?);
    }
    r.le = le;
    let (_unity, _platform, has_tree) = (r.cstr()?, r.i32()?, r.u8()? != 0);
    if !has_tree {
        return Err("this Unity build has no type trees".into());
    }
    let mut types = Vec::new();
    for _ in 0..r.i32()? {
        let (class_id, _stripped, script) = (r.i32()?, r.u8()?, r.u16()? as i16);
        if class_id == 114 || script >= 0 {
            r.take(16)?; // script id
        }
        r.take(16)?; // type hash
        let (count, strings) = (r.i32()? as usize, r.i32()? as usize);
        let mut fields = Vec::new();
        for _ in 0..count {
            let (_v, level, flags, ty, name, size) = (r.u16()?, r.u8()?, r.u8()?, r.u32()?, r.u32()?, r.i32()?);
            let (_index, meta) = (r.i32()?, r.u32()?);
            if version >= 19 {
                r.u64()?;
            }
            fields.push((level, flags, ty, name, size, meta));
        }
        let local = r.take(strings)?;
        let nodes = fields
            .into_iter()
            .map(|(level, flags, ty, name, size, meta)| TNode {
                level,
                ty: tree_string(local, ty),
                name: tree_string(local, name),
                size,
                array: flags & 1 != 0,
                align: meta & 0x4000 != 0,
            })
            .collect();
        if version >= 21 {
            let deps = r.i32()? as usize;
            r.take(4 * deps)?;
        }
        types.push(Type { class_id, nodes });
    }
    let mut objects = Vec::new();
    for _ in 0..r.i32()? {
        r.align(4);
        let path_id = r.i64()?;
        let entry = r.p;
        let start = if version >= 22 { r.i64()? as u64 } else { r.u32()? as u64 };
        let (size, ty) = (r.u32()? as usize, r.i32()? as usize);
        objects.push(Obj { path_id, ty, start: (data_offset + start) as usize, size, entry });
    }
    Ok(Serialized { raw, version, le, data_offset: data_offset as usize, types, objects })
}

impl Serialized {
    pub fn data(&self, o: &Obj) -> &[u8] {
        &self.raw[o.start..o.start + o.size]
    }

    /// The file with some objects' data replaced (by index into `objects`).
    pub fn rebuild(&self, new: &HashMap<usize, Vec<u8>>) -> Vec<u8> {
        let mut out = self.raw[..self.data_offset].to_vec();
        let mut order: Vec<usize> = (0..self.objects.len()).collect();
        order.sort_by_key(|&i| self.objects[i].start);
        for i in order {
            let o = &self.objects[i];
            pad(&mut out, 8);
            let data = new.get(&i).map(Vec::as_slice).unwrap_or(self.data(o));
            let start = (out.len() - self.data_offset) as u64;
            let mut entry = if self.version >= 22 { self.num(start, 8) } else { self.num(start, 4) };
            entry.extend(self.num(data.len() as u64, 4));
            out[o.entry..o.entry + entry.len()].copy_from_slice(&entry);
            out.extend(data);
        }
        let size = out.len() as u64;
        if self.version >= 22 {
            out[24..32].copy_from_slice(&size.to_be_bytes());
        } else {
            out[4..8].copy_from_slice(&(size as u32).to_be_bytes());
        }
        out
    }

    fn num(&self, v: u64, width: usize) -> Vec<u8> {
        let b = if self.le { v.to_le_bytes() } else { v.to_be_bytes() };
        if self.le { b[..width].to_vec() } else { b[8 - width..].to_vec() }
    }
}

// ---------- strings inside objects ----------

/// One string field of an object: its path (e.g. `m_TableData[3].m_Localized`) and byte span.
pub struct Str {
    pub path: String,
    pub text: String,
    start: usize,
    end: usize,
}

struct Walk<'a> {
    n: &'a [TNode],
    r: Rd<'a>,
    out: Vec<Str>,
    ints: Vec<(String, i64)>,
    stop: bool,
}

impl Walk<'_> {
    fn kids(&self, i: usize) -> Vec<usize> {
        let level = self.n[i].level;
        (i + 1..self.n.len()).take_while(|&k| self.n[k].level > level).filter(|&k| self.n[k].level == level + 1).collect()
    }

    fn field(&mut self, i: usize, path: String) -> R<()> {
        let kids = self.kids(i);
        let node = &self.n[i];
        match node.ty.as_str() {
            "string" => {
                let start = self.r.p;
                let len = self.r.i32()? as usize;
                let text = String::from_utf8_lossy(self.r.take(len)?).into_owned();
                if !(node.align || kids.iter().any(|&k| self.n[k].align)) {
                    return Err(format!("unaligned string field {path}"));
                }
                self.r.align(4);
                self.out.push(Str { path, text, start, end: self.r.p });
                return Ok(());
            }
            // [SerializeReference] data; Unity always serializes it last, so the walk can stop here
            "ManagedReferencesRegistry" => self.stop = true,
            "TypelessData" => {
                let len = self.r.i32()? as usize;
                self.r.take(len)?;
            }
            "SInt64" if kids.is_empty() => {
                let v = self.r.i64()?;
                self.ints.push((path, v));
            }
            _ if kids.is_empty() => {
                self.r.take(node.size.max(0) as usize)?;
            }
            _ if kids.len() == 1 && self.n[kids[0]].array => self.array(kids[0], &path)?,
            _ if node.array => self.array(i, &path)?,
            _ => {
                for k in kids {
                    let name = &self.n[k].name;
                    self.field(k, if path.is_empty() { name.clone() } else { format!("{path}.{name}") })?;
                    if self.stop {
                        return Ok(());
                    }
                }
            }
        }
        if node.align {
            self.r.align(4);
        }
        Ok(())
    }

    fn array(&mut self, a: usize, path: &str) -> R<()> {
        let count = self.r.i32()? as usize;
        let elem = *self.kids(a).get(1).ok_or("array without element type")?;
        let e = &self.n[elem];
        if self.kids(elem).is_empty() && e.ty != "string" {
            self.r.take(count * e.size.max(0) as usize)?;
        } else {
            for j in 0..count {
                self.field(elem, format!("{path}[{j}]"))?;
                if self.stop {
                    return Ok(());
                }
            }
        }
        if self.n[a].align {
            self.r.align(4);
        }
        Ok(())
    }
}

/// Every string and 64-bit id in an object, read with its type tree.
#[allow(clippy::type_complexity)]
pub fn fields(ty: &Type, data: &[u8], le: bool) -> R<(Vec<Str>, Vec<(String, i64)>)> {
    let mut w = Walk { n: &ty.nodes, r: Rd::new(data, le), out: Vec::new(), ints: Vec::new(), stop: false };
    if !w.n.is_empty() {
        w.field(0, String::new())?;
    }
    Ok((w.out, w.ints))
}

/// The object with some strings replaced; later fields shift but keep their 4-byte alignment.
pub fn replace(data: &[u8], strs: &[Str], new: &[(usize, String)], le: bool) -> Vec<u8> {
    let mut edits: Vec<&(usize, String)> = new.iter().collect();
    edits.sort_by_key(|(i, _)| strs[*i].start);
    let (mut out, mut at) = (Vec::with_capacity(data.len()), 0);
    for (i, text) in edits {
        let s = &strs[*i];
        out.extend(&data[at..s.start]);
        let len = text.len() as u32;
        out.extend(if le { len.to_le_bytes() } else { len.to_be_bytes() });
        out.extend(text.as_bytes());
        pad(&mut out, 4);
        at = s.end;
    }
    out.extend(&data[at..]);
    out
}

// ---------- Unity Localization string tables ----------

/// One locale's string table: {entry id: text}. `collection` is the name shared by all locales.
pub struct Table {
    pub collection: String,
    pub locale: String,
    pub shared: i64,
    pub entries: indexmap::IndexMap<i64, String>,
    /// an asset table: its entries point to per-language images instead of holding text
    pub images: bool,
    at: HashMap<i64, usize>, // entry id -> index of its string in the object's fields
}

impl Table {
    /// "ja" for "ja-JP", "en" for "en", "zh" for "zh-Hans".
    pub fn lang(&self) -> String {
        self.locale.split(['-', '_', '(']).next().unwrap_or("").to_lowercase()
    }
}

/// The Addressables folder (<name>_Data/StreamingAssets/aa) where Unity Localization keeps its tables.
pub fn addressables(game: &Path) -> Option<PathBuf> {
    fs::read_dir(game).ok()?.flatten().map(|e| e.path().join("StreamingAssets").join("aa")).find(|p| p.is_dir())
}

// shortcut: only bundles named like Unity Localization's default groups ("…string-tables…",
// "…asset-tables…" for images);
// scan every bundle once a game turns up that renames them.
fn table_bundles(aa: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    files_under(aa, &mut all);
    all.retain(|p| {
        let n = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        n.ends_with(".bundle") && (n.contains("string-tables") || n.contains("asset-tables"))
    });
    all.sort();
    all
}

fn is_images(p: &Path) -> bool {
    p.file_name().unwrap_or_default().to_string_lossy().to_lowercase().contains("asset-tables")
}

fn is_serialized(n: &Node) -> bool {
    !(n.path.ends_with(".resS") || n.path.ends_with(".resource"))
}

/// Index j of a field path like `m_TableData[j].m_Id`.
fn entry_index(path: &str, field: &str) -> Option<usize> {
    path.strip_prefix("m_TableData[")?.strip_suffix(field)?.strip_suffix(']')?.parse().ok()
}

/// StringTable objects in a serialized file: (object index, table, its string fields).
fn file_tables(s: &Serialized, images: bool) -> R<Vec<(usize, Table, Vec<Str>)>> {
    let mut out = Vec::new();
    for (i, o) in s.objects.iter().enumerate() {
        let ty = &s.types[o.ty];
        let has = |f: &str| ty.nodes.iter().any(|n| n.name == f);
        if ty.class_id != 114 || !has("m_TableData") || !has("m_LocaleId") {
            continue;
        }
        let (strs, ints) = fields(ty, s.data(o), s.le)?;
        let get = |p: &str| strs.iter().find(|x| x.path == p).map(|x| x.text.clone()).unwrap_or_default();
        let (name, locale) = (get("m_Name"), get("m_LocaleId.m_Code"));
        let ids: HashMap<usize, i64> = ints.iter().filter_map(|(p, v)| Some((entry_index(p, ".m_Id")?, *v))).collect();
        let (mut entries, mut at) = (indexmap::IndexMap::new(), HashMap::new());
        for (k, x) in strs.iter().enumerate() {
            if let Some(id) = entry_index(&x.path, ".m_Localized").and_then(|j| ids.get(&j)) {
                entries.insert(*id, x.text.clone());
                at.insert(*id, k);
            }
        }
        let shared = ints.iter().find(|(p, _)| p == "m_SharedData.m_PathID").map_or(0, |(_, v)| *v);
        let collection = name.strip_suffix(&format!("_{locale}")).unwrap_or(&name).to_string();
        out.push((i, Table { collection, locale, shared, entries, images, at }, strs));
    }
    Ok(out)
}

fn bundle_tables(b: &Bundle, images: bool) -> R<Vec<Table>> {
    let mut out = Vec::new();
    for f in b.files.iter().filter(|f| is_serialized(f)) {
        out.extend(file_tables(&read_serialized(f.data.clone())?, images)?.into_iter().map(|(_, t, _)| t));
    }
    Ok(out)
}

/// Every string table of the game, as shipped (our backups are read instead of our own files).
pub fn read_tables(aa: &Path) -> R<Vec<Table>> {
    let mut out = Vec::new();
    for p in table_bundles(aa) {
        let tables = read_bundle(&original(&p)?).and_then(|b| bundle_tables(&b, is_images(&p)));
        out.extend(tables.map_err(|e| format!("{}: {e}", p.display()))?);
    }
    Ok(out)
}

/// Write `text(table, id)` into the tables, always starting from the game's original bundles.
/// Originals are kept as *.vntl-orig, every written bundle is read back and checked first, and
/// the catalog's bundle CRCs are zeroed before the first one changes: a CRC of 0 passes changed and
/// unchanged bundles alike, so stopping anywhere leaves a game that loads. Returns entries written.
pub fn install(aa: &Path, text: &dyn Fn(&Table, i64) -> Option<String>) -> R<usize> {
    let mut total = 0;
    for p in table_bundles(aa) {
        let err = |e: String| format!("{}: {e}", p.display());
        let mut b = read_bundle(&original(&p)?).map_err(err)?;
        let images = is_images(&p);
        let before = bundle_tables(&b, images).map_err(err)?;
        let mut want = Vec::new(); // (collection, locale, entry index, text)
        for f in b.files.iter_mut().filter(|f| is_serialized(f)) {
            let s = read_serialized(f.data.clone()).map_err(err)?;
            let mut new = HashMap::new();
            for (i, t, strs) in file_tables(&s, images).map_err(err)? {
                let mut edits = Vec::new();
                for (j, (id, old)) in t.entries.iter().enumerate() {
                    if let Some(n) = text(&t, *id).filter(|n| n != old) {
                        want.push((t.collection.clone(), t.locale.clone(), j, n.clone()));
                        edits.push((t.at[id], n));
                    }
                }
                if !edits.is_empty() {
                    new.insert(i, replace(s.data(&s.objects[i]), &strs, &edits, s.le));
                }
            }
            if !new.is_empty() {
                f.data = s.rebuild(&new);
            }
        }
        if want.is_empty() {
            if backup(&p).exists() {
                fs::rename(backup(&p), &p).map_err(|e| err(e.to_string()))?; // nothing of ours left in it
                _ = fs::remove_file(stamp(&p));
            }
            continue;
        }
        let bytes = b.write();
        // read it back: same tables and entries, and every new text in place
        let after = read_bundle(&bytes).and_then(|b| bundle_tables(&b, images)).map_err(err)?;
        let shape = |ts: &[Table]| ts.iter().map(|t| (t.collection.clone(), t.locale.clone(), t.entries.len())).collect::<Vec<_>>();
        let placed = want.iter().all(|(c, l, j, n)| {
            after.iter().any(|t| &t.collection == c && &t.locale == l && t.entries.get_index(*j).is_some_and(|(_, v)| v == n))
        });
        if shape(&before) != shape(&after) || !placed {
            return Err(err("the rebuilt bundle did not read back correctly; this bundle was not written".into()));
        }
        if total == 0 {
            zero_catalog_crcs(aa)?;
        }
        replace_file(&p, &bytes)?;
        total += want.len();
    }
    Ok(total)
}

// ---------- the Addressables catalog ----------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in s.bytes().filter(|&c| c != b'=') {
        acc = ((acc << 6) | B64.iter().position(|&x| x == c)? as u32) & 0xffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub(crate) fn b64_encode(b: &[u8]) -> String {
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = c.iter().fold(0u32, |a, &x| (a << 8) | x as u32) << (8 * (3 - c.len()));
        for i in 0..4 {
            s.push(if i <= c.len() { B64[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    s
}

/// `"m_Crc":123` -> `"m_Crc":0  ` in the catalog's UTF-16 bundle options: same length, so the
/// binary layout around it stays valid. Unity skips the CRC check for a CRC of 0.
fn zero_crcs(extra: &mut [u8]) -> usize {
    let pat: Vec<u8> = "\"m_Crc\":".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let (mut n, mut i) = (0, 0);
    while let Some(at) = extra[i..].windows(pat.len()).position(|w| w == pat.as_slice()) {
        let first = i + at + pat.len();
        let mut j = first;
        while j + 1 < extra.len() && extra[j].is_ascii_digit() && extra[j + 1] == 0 {
            j += 2;
        }
        if j > first && !(j == first + 2 && extra[first] == b'0') {
            n += 1;
            extra[first] = b'0';
            (first + 2..j).step_by(2).for_each(|k| extra[k] = b' ');
        }
        i = j;
    }
    n
}

fn zero_catalog_crcs(aa: &Path) -> R<()> {
    let path = aa.join("catalog.json");
    // shortcut: binary catalogs (catalog.bin, Addressables 2) are not patched; add that when a game needs it
    let Ok(text) = fs::read_to_string(&path) else { return Ok(()) };
    const KEY: &str = "\"m_ExtraDataString\":\"";
    let Some(start) = text.find(KEY).map(|i| i + KEY.len()) else { return Ok(()) };
    let end = start + text[start..].find('"').ok_or("bad catalog.json")?;
    let mut extra = b64_decode(&text[start..end]).ok_or("bad catalog.json")?;
    if zero_crcs(&mut extra) == 0 {
        return Ok(());
    }
    original(&path)?; // stops on a catalog a game update replaced: its backup is the older game's
    replace_file(&path, format!("{}{}{}", &text[..start], b64_encode(&extra), &text[end..]).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip_and_crcs_zeroed_in_place() {
        for s in [&b""[..], b"a", b"ab", b"abc", b"abcd\xff\x00"] {
            assert_eq!(b64_decode(&b64_encode(s)).unwrap(), s);
        }
        let mut b: Vec<u8> = "{\"m_Crc\":950,\"x\":1}{\"m_Crc\":0}".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(zero_crcs(&mut b), 1);
        let back = String::from_utf16(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).unwrap();
        assert_eq!(back, "{\"m_Crc\":0  ,\"x\":1}{\"m_Crc\":0}");
    }

    #[test]
    fn replaced_strings_keep_later_fields_aligned() {
        // "ab" (4+2+2 pad) then an int 7; replace with "abcdef" (4+6+2 pad)
        let data = [2, 0, 0, 0, b'a', b'b', 0, 0, 7, 0, 0, 0];
        let strs = vec![Str { path: "s".into(), text: "ab".into(), start: 0, end: 8 }];
        let out = replace(&data, &strs, &[(0, "abcdef".into())], true);
        assert_eq!(out, [6, 0, 0, 0, b'a', b'b', b'c', b'd', b'e', b'f', 0, 0, 7, 0, 0, 0]);
    }

    #[test]
    fn bundles_round_trip_across_blocks_in_both_header_formats() {
        let big: Vec<u8> = (0..50_000u32).flat_map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()).collect();
        for (version, flags) in [(6, 0x40u32), (7, 0x240)] {
            let files = vec![
                Node { path: "CAB-a".into(), flags: 4, data: big.clone() },
                Node { path: "CAB-a.resS".into(), flags: 0, data: vec![1, 2, 3] },
            ];
            let bytes = Bundle { version, unity: "5.x.x".into(), revision: "2019.4.40f1".into(), files }.write();
            let flags_at = "UnityFS\0".len() + 4 + "5.x.x\0".len() + "2019.4.40f1\0".len() + 8 + 4 + 4;
            assert_eq!(bytes[flags_at..flags_at + 4], flags.to_be_bytes());
            let back = read_bundle(&bytes).unwrap();
            let got: Vec<_> = back.files.iter().map(|f| (f.path.as_str(), f.flags, f.data.len())).collect();
            assert_eq!(got, [("CAB-a", 4, big.len()), ("CAB-a.resS", 0, 3)]);
            assert_eq!(back.files[0].data, big);
            assert_eq!(back.files[1].data, [1, 2, 3]);
        }
    }

    const PLAIN: [u8; 5] = [1, 2, 3, 4, 5];
    // level, array flag, type, name, size, aligned
    const TREE: [(u8, u8, &str, &str, i32, bool); 13] = [
        (0, 0, "MonoBehaviour", "Base", -1, false),
        (1, 0, "string", "m_Name", -1, true),
        (1, 0, "LocaleIdentifier", "m_LocaleId", -1, false),
        (2, 0, "string", "m_Code", -1, true),
        (1, 0, "PPtr", "m_SharedData", 12, false),
        (2, 0, "int", "m_FileID", 4, false),
        (2, 0, "SInt64", "m_PathID", 8, false),
        (1, 0, "vector", "m_TableData", -1, false),
        (2, 1, "Array", "Array", -1, false),
        (3, 0, "int", "size", 4, false),
        (3, 0, "TableEntryData", "data", -1, false),
        (4, 0, "SInt64", "m_Id", 8, false),
        (4, 0, "string", "m_Localized", -1, true),
    ];

    /// A hand-made serialized file: a StringTable object, then PLAIN to show what moves behind it.
    fn serialized(v: u32, name: &str, locale: &str, entries: &[(i64, &str)]) -> Vec<u8> {
        let text = |out: &mut Vec<u8>, s: &str| {
            out.extend((s.len() as i32).to_le_bytes());
            out.extend(s.as_bytes());
            pad(out, 4);
        };
        let mut table = Vec::new();
        text(&mut table, name);
        text(&mut table, locale);
        table.extend(0i32.to_le_bytes());
        table.extend(77i64.to_le_bytes());
        table.extend((entries.len() as i32).to_le_bytes());
        for (id, s) in entries {
            table.extend(id.to_le_bytes());
            text(&mut table, s);
        }

        let (mut nodes, mut names) = (Vec::new(), Vec::new());
        for (level, array, ty, field, size, align) in TREE {
            nodes.extend([0, 0, level, array]);
            for s in [ty, field] {
                nodes.extend((names.len() as u32).to_le_bytes());
                names.extend(s.as_bytes());
                names.push(0);
            }
            nodes.extend(size.to_le_bytes());
            nodes.extend(0i32.to_le_bytes());
            nodes.extend((if align { 0x4000u32 } else { 0 }).to_le_bytes());
            if v >= 19 {
                nodes.extend(0u64.to_le_bytes());
            }
        }
        let deps = if v >= 21 { 0i32.to_le_bytes().to_vec() } else { Vec::new() };

        let mut o = vec![0u8; if v >= 22 { 48 } else { 20 }];
        o[8..12].copy_from_slice(&v.to_be_bytes());
        o.extend(b"2021.3.1f1\0");
        o.extend(5i32.to_le_bytes());
        o.push(1); // has type trees
        o.extend(2i32.to_le_bytes());
        o.extend(114i32.to_le_bytes());
        o.extend([0, 0xff, 0xff]); // not stripped, script index -1
        o.extend([0u8; 32]); // script id and type hash
        o.extend((TREE.len() as i32).to_le_bytes());
        o.extend((names.len() as i32).to_le_bytes());
        o.extend(nodes);
        o.extend(names);
        o.extend(&deps);
        o.extend(1i32.to_le_bytes());
        o.extend([0, 0xff, 0xff]);
        o.extend([0u8; 16]); // type hash
        o.extend([0u8; 8]); // no fields, no strings
        o.extend(&deps);
        o.extend(2i32.to_le_bytes());
        let second = table.len().div_ceil(8) * 8;
        for (i, (start, size)) in [(0, table.len()), (second, PLAIN.len())].into_iter().enumerate() {
            pad(&mut o, 4);
            o.extend((i as i64 + 1).to_le_bytes());
            match v >= 22 {
                true => o.extend((start as u64).to_le_bytes()),
                false => o.extend((start as u32).to_le_bytes()),
            }
            o.extend((size as u32).to_le_bytes());
            o.extend((i as i32).to_le_bytes());
        }
        pad(&mut o, 16);
        match v >= 22 {
            true => { let at = o.len() as u64; o[32..40].copy_from_slice(&at.to_be_bytes()) }
            false => { let at = o.len() as u32; o[12..16].copy_from_slice(&at.to_be_bytes()) }
        }
        o.extend(table);
        pad(&mut o, 8);
        o.extend(PLAIN);
        o
    }

    #[test]
    fn serialized_files_read_by_type_tree_and_rebuild_with_later_objects_moved() {
        for v in [17, 19, 21, 22] {
            let s = read_serialized(serialized(v, "T_ja", "ja", &[(5, "あ"), (9, "い")])).unwrap();
            let (strs, ints) = fields(&s.types[0], s.data(&s.objects[0]), s.le).unwrap();
            let got: Vec<_> = strs.iter().map(|x| (x.path.as_str(), x.text.as_str())).collect();
            assert_eq!(got, [("m_Name", "T_ja"), ("m_LocaleId.m_Code", "ja"), ("m_TableData[0].m_Localized", "あ"), ("m_TableData[1].m_Localized", "い")]);
            assert_eq!((strs[2].start, strs[2].end), (40, 48)); // 8 + 8 + 12 + 4 + 8, then 4 + 3 + 1 pad
            let got: Vec<_> = ints.iter().map(|(p, n)| (p.as_str(), *n)).collect();
            assert_eq!(got, [("m_SharedData.m_PathID", 77), ("m_TableData[0].m_Id", 5), ("m_TableData[1].m_Id", 9)]);

            let longer = replace(s.data(&s.objects[0]), &strs, &[(2, "a much longer line of text".to_string())], s.le);
            let out = s.rebuild(&HashMap::from([(0, longer)]));
            let size = match v >= 22 {
                true => u64::from_be_bytes(out[24..32].try_into().unwrap()),
                false => u32::from_be_bytes(out[4..8].try_into().unwrap()) as u64,
            };
            assert_eq!(size as usize, out.len());
            let back = read_serialized(out).unwrap();
            assert!(back.objects[1].start > s.objects[1].start);
            assert_eq!(back.data(&back.objects[1]), PLAIN);
            let strs = fields(&back.types[0], back.data(&back.objects[0]), back.le).unwrap().0;
            assert_eq!((strs[2].text.as_str(), strs[3].text.as_str()), ("a much longer line of text", "い"));
        }
    }

    fn catalog(extra: &str) -> String {
        let extra: Vec<u8> = extra.encode_utf16().flat_map(u16::to_le_bytes).collect();
        format!("{{\"m_ExtraDataString\":\"{}\"}}", b64_encode(&extra))
    }

    /// A game folder with an English and a Japanese table bundle: (folder, the two bundles).
    fn game(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let aa = std::env::temp_dir().join(format!("vntl_unity_{name}_{}", std::process::id()));
        fs::create_dir_all(&aa).unwrap();
        let bundle = |name: &str, locale: &str, entries: &[(i64, &str)]| {
            let data = serialized(22, name, locale, entries);
            let files = vec![Node { path: "CAB-t".into(), flags: 4, data }];
            Bundle { version: 7, unity: "5.x.x".into(), revision: "2021.3.1f1".into(), files }.write()
        };
        let (en, ja) = (aa.join("a_string-tables.bundle"), aa.join("b_string-tables.bundle"));
        fs::write(&en, bundle("T_en", "en", &[(1, "")])).unwrap();
        fs::write(&ja, bundle("T_ja", "ja", &[(1, "こんにちは")])).unwrap();
        fs::write(aa.join("catalog.json"), catalog("{\"m_Crc\":950}")).unwrap();
        (aa, en, ja)
    }

    fn entry(path: &Path) -> String {
        bundle_tables(&read_bundle(&fs::read(path).unwrap()).unwrap(), false).unwrap()[0].entries[&1].clone()
    }

    #[test]
    fn install_writes_the_tables_and_restore_undoes_all_of_it() {
        let (aa, en, ja) = game("install");
        let shipped = [fs::read(&en).unwrap(), fs::read(&ja).unwrap()];
        assert_eq!(read_tables(&aa).unwrap().len(), 2);
        assert_eq!(install(&aa, &|t, _| Some(format!("Hello {}", t.locale))).unwrap(), 2);
        assert_eq!((entry(&en), entry(&ja)), ("Hello en".to_string(), "Hello ja".to_string()));
        assert_eq!(fs::read_to_string(aa.join("catalog.json")).unwrap(), catalog("{\"m_Crc\":0  }"));
        assert_eq!(read_tables(&aa).unwrap()[1].entries[&1], "こんにちは"); // the shipped text, not ours
        assert_eq!(crate::restore(&aa), Ok((3, 0)));
        assert_eq!([fs::read(&en).unwrap(), fs::read(&ja).unwrap()], shipped);
        assert_eq!(fs::read_to_string(aa.join("catalog.json")).unwrap(), catalog("{\"m_Crc\":950}"));
        fs::remove_dir_all(&aa).unwrap();
    }

    #[test]
    fn a_bundle_that_fails_midway_leaves_the_crcs_zeroed() {
        let (aa, en, ja) = game("midway");
        let shipped = fs::read(&ja).unwrap();
        fs::create_dir(format!("{}.tmp", ja.display())).unwrap(); // the second write can't happen
        assert!(install(&aa, &|_, _| Some("Hello".into())).is_err());
        assert_eq!(entry(&en), "Hello");
        assert_eq!(fs::read(&ja).unwrap(), shipped);
        assert_eq!(fs::read_to_string(aa.join("catalog.json")).unwrap(), catalog("{\"m_Crc\":0  }"));
        fs::remove_dir_all(&aa).unwrap();
    }

    #[test]
    fn a_catalog_from_a_game_update_stops_before_any_bundle_changes() {
        let (aa, en, _) = game("update");
        install(&aa, &|_, _| Some("Hello".into())).unwrap();
        fs::write(aa.join("catalog.json"), catalog("{\"m_Crc\":123}")).unwrap();
        assert!(install(&aa, &|_, _| Some("Goodbye".into())).unwrap_err().contains("game update"));
        assert_eq!(entry(&en), "Hello");
        assert_eq!(fs::read_to_string(aa.join("catalog.json")).unwrap(), catalog("{\"m_Crc\":123}"));
        fs::remove_dir_all(&aa).unwrap();
    }
}
