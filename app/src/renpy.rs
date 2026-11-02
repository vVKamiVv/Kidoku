//! Ren'Py: reading a game's script through its own engine, and writing a standard
//! Ren'Py translation (game/tl/<lang>/vntl_*.rpy). Original files are never modified.

use crate::text::*;
use crate::R;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const TOKEN: &str = "\"@@VNTL@@\"";
const HOOK: &str = include_str!("../renpy/zz_vntl_dump.rpy");

/// What the hook dumps from a game (see renpy/zz_vntl_dump.rpy).
#[derive(Deserialize, Default)]
pub struct Dump {
    pub blocks: Vec<Block>,
    #[serde(default)]
    pub strings: Vec<StringEntry>,
    #[serde(default)]
    pub names: IndexMap<String, String>,
    #[serde(default)]
    pub string_tl: IndexMap<String, IndexMap<String, String>>,
    #[serde(default)]
    pub defaults: IndexMap<String, Vec<String>>,
    #[serde(default)]
    pub shown_vars: IndexMap<String, ShownVar>,
    #[serde(default)]
    pub nvl: Vec<String>,
    #[serde(default)]
    pub layout: Value,
    #[serde(default)]
    pub lang_python: IndexMap<String, Vec<String>>,
}

#[derive(Deserialize, Default)]
pub struct Block {
    pub id: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: i64,
    #[serde(default)]
    pub label: String,
    pub items: Vec<Item>,
    #[serde(default)]
    pub tl: IndexMap<String, Vec<String>>,
}

/// A dialogue line (`tpl` + `what`) or another statement kept as code (e.g. `nvl clear`).
#[derive(Deserialize, Default)]
pub struct Item {
    pub tpl: Option<String>,
    pub who: Option<String>,
    pub what: Option<String>,
    pub code: Option<String>,
}

impl Item {
    pub fn is_say(&self) -> bool {
        self.tpl.is_some()
    }
    pub fn what(&self) -> &str {
        self.what.as_deref().unwrap_or("")
    }
    pub fn is_nvl_clear(&self) -> bool {
        self.code.as_deref().is_some_and(|c| c.trim() == "nvl clear")
    }
}

#[derive(Deserialize, Default)]
pub struct StringEntry {
    pub text: String,
    pub kind: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: i64,
}

#[derive(Deserialize, Default)]
pub struct ShownVar {
    #[serde(default)]
    pub init: Option<String>,
    #[serde(default)]
    pub assigns: Vec<Assign>,
}

#[derive(Deserialize, Default)]
pub struct Assign {
    pub code: String,
}

/// Translation progress, saved after every batch (same format as the Python version's cache).
#[derive(Serialize, Deserialize, Default)]
pub struct Cache {
    #[serde(default)]
    pub lines: IndexMap<String, String>,
    #[serde(default)]
    pub strings: IndexMap<String, String>,
    /// Lines laid out by the official translators: never re-fitted.
    #[serde(default)]
    pub official: Vec<String>,
    /// {japanese: [english, english that fits, attempts]}
    #[serde(default)]
    pub fit: IndexMap<String, Vec<Value>>,
    #[serde(default)]
    pub style: Style,
}

/// An official translation's `translate <lang> python` code (e.g. its English font and size).
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Style {
    #[serde(default)]
    pub code: Vec<String>,
    #[serde(default)]
    pub text_size: Option<i64>,
}

pub fn key(b: &Block, i: usize) -> String {
    format!("{}#{}", b.id, i)
}

pub fn text_mode(item: &Item, nvl: &HashSet<String>) -> &'static str {
    if nvl.contains(item.who.as_deref().unwrap_or("narrator")) { "nvl" } else { "adv" }
}

// ---------- engine detection ----------

fn starts_with(path: &Path, magic: &[u8]) -> bool {
    let mut head = vec![0; magic.len()];
    fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && head == magic
}

/// The file names of an Electron archive, which it keeps as JSON after a 16 byte header.
fn asar_index(path: &Path) -> Option<Vec<u8>> {
    let mut f = fs::File::open(path).ok()?;
    let mut head = [0; 16];
    f.read_exact(&mut head).ok()?;
    let mut index = Vec::new();
    f.take(u32::from_le_bytes(head[12..].try_into().ok()?).into()).read_to_end(&mut index).ok()?;
    Some(index)
}

/// The file names of a zip, found from its end because it can be stuck on the end of a program.
fn zip_index(path: &Path) -> Option<Vec<u8>> {
    let mut f = fs::File::open(path).ok()?;
    let start = f.metadata().ok()?.len().saturating_sub(66_000); // fits the end record and the longest comment
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    let end = tail.windows(4).rposition(|w| w == b"PK\x05\x06")?;
    let size = u32::from_le_bytes(tail.get(end + 12..end + 16)?.try_into().ok()?);
    f.seek(SeekFrom::Start((start + end as u64).checked_sub(size.into())?)).ok()?;
    let mut index = Vec::new();
    f.take(size.into()).read_to_end(&mut index).ok()?;
    Some(index)
}

/// A packaged TyranoScript game keeps `data/scenario` in an Electron archive, in a `package.nw`,
/// or in a zip stuck on the end of the NW.js program.
fn packaged_tyranoscript(dir: &Path, names: &[String]) -> bool {
    let holds = |index: &[u8], word: &[u8]| index.windows(word.len()).any(|w| w == word);
    let tyrano = |index: Option<Vec<u8>>| index.is_some_and(|i| holds(&i, b"scenario") && holds(&i, b"tyrano"));
    let nwjs = names.iter().any(|n| n == "nw.pak");
    tyrano(asar_index(&dir.join("resources").join("app.asar")))
        || names.iter().any(|n| (n == "package.nw" || nwjs && n.ends_with(".exe")) && tyrano(zip_index(&dir.join(n))))
}

pub fn detect(dir: &Path) -> &'static str {
    let lowered = |folder: &Path| -> Vec<String> {
        fs::read_dir(folder)
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect())
            .unwrap_or_default()
    };
    let names = lowered(dir);
    let any = |f: &dyn Fn(&str) -> bool| names.iter().any(|n| f(n));
    let has = |name: &str| names.iter().any(|n| n == name);
    let inside = |folder: &str, ext: &str| lowered(&dir.join(folder)).iter().any(|n| n.ends_with(ext));
    let scripts = |ext: &str| dir.join("data").join(format!("scripts.{ext}")).is_file();
    if dir.join("renpy").is_dir() && dir.join("game").is_dir() {
        "renpy"
    } else if any(&|n| n.ends_with(".xp3")) {
        "kirikiri"
    } else if dir.join("www").join("data").is_dir() || dir.join("data").join("System.json").is_file() {
        "rpgmaker-mv/mz"
    } else if any(&|n| (n.contains(".rgss") && n.ends_with('a')) || n.starts_with("game.rgss"))
        || ["rxdata", "rvdata", "rvdata2"].iter().any(|e| scripts(e))
    {
        "rpgmaker-xp/vx/ace"
    } else if any(&|n| n == "rpg_rt.ldb" || n == "rpg_rt.lmt") {
        "rpgmaker-2000/2003" // the data files: a game can rename its RPG_RT.exe
    } else if dir.join("data").join("scenario").is_dir() || packaged_tyranoscript(dir, &names) {
        "tyranoscript"
    } else if ["nscript.dat", "0.txt", "arc.nsa"].iter().any(|n| dir.join(n).exists()) {
        "nscripter"
    } else if dir.join("scr.xfl").is_file() {
        "liarsoft"
    } else if any(&|n| n.ends_with("_data")) {
        "unity"
    } else if has("scene.pck") && has("gameexe.dat") {
        "siglusengine"
    } else if has("seen.txt") && has("gameexe.ini") {
        "reallive"
    } else if has("scene.int") {
        "catsystem2"
    } else if has("sysprg.arc") || has("bgi.gdb") {
        "bgi"
    } else if any(&|n| n.ends_with(".pfs")) {
        "artemis"
    } else if any(&|n| n.ends_with(".wolf")) || inside("data", ".wolf") {
        "wolf-rpg"
    } else if any(&|n| n.ends_with(".ypf")) || inside("pac", ".ypf") {
        "yu-ris"
    } else if inside("gamedata", ".pack") {
        "qlie"
    } else if starts_with(&dir.join("scenario.arc"), b"MajiroArc") {
        "majiro"
    } else if has("rio.arc") || has("advhd.exe") {
        "advhd"
    } else if any(&|n| n.ends_with(".hcb")) {
        "fvp"
    } else if has("malie.ini") || has("malie.exe") {
        "malie"
    } else if has("pal.dll") || inside("dll", "pal.dll") {
        "softpal"
    } else {
        "unknown"
    }
}

/// The folder that holds the game. Many games sit in a wrapper folder or keep a launcher beside a
/// data folder, so if `dir` isn't a game this looks a few levels down. With several games below,
/// it stays put: only the person choosing knows which one they mean.
pub fn game_root(dir: &Path) -> PathBuf {
    let mut queue = VecDeque::from([(dir.to_path_buf(), 0)]);
    let mut games = Vec::new();
    for _ in 0..64 { // a limit, so that picking a whole drive doesn't freeze the window
        let Some((folder, depth)) = queue.pop_front() else { break };
        if detect(&folder) != "unknown" {
            games.push(folder);
        } else if depth < 3 {
            let inner = fs::read_dir(&folder).into_iter().flatten().flatten().map(|e| e.path());
            queue.extend(inner.filter(|p| p.is_dir()).map(|p| (p, depth + 1)));
        }
    }
    match games.as_slice() {
        [game] => game.clone(),
        _ => dir.to_path_buf(),
    }
}

pub const SUPPORTED: &[&str] = &["renpy", "unity", "liarsoft", "rpgmaker-mv/mz", "malie", "wolf-rpg", "siglusengine", "qlie", "reallive", "catsystem2", "majiro", "yu-ris", "bgi", "advhd", "kirikiri", "artemis", "softpal", "fvp", "rpgmaker-xp/vx/ace", "rpgmaker-2000/2003", "tyranoscript", "nscripter"];

// ---------- running the game's engine ----------

fn renpy_exe(game: &Path) -> R<PathBuf> {
    // A Ren'Py game ships <name>.py next to <name>.exe (and maybe <name>-32.exe).
    let mut stems: Vec<String> = fs::read_dir(game)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".py").map(str::to_string))
        .filter(|s| game.join(format!("{s}.exe")).exists())
        .collect();
    stems.sort();
    stems.first().map(|s| game.join(format!("{s}.exe"))).ok_or_else(|| {
        format!("No Ren'Py launcher (.exe next to a .py) found in {}", game.display())
    })
}

fn run_renpy(game: &Path, args: &[&std::ffi::OsStr]) -> R<()> {
    let reports = [game.join("traceback.txt"), game.join("errors.txt")];
    for f in &reports {
        let _ = fs::remove_file(f);
    }
    let status = Command::new(renpy_exe(game)?).arg(game).args(args).status().map_err(|e| e.to_string())?;
    let found: Vec<&PathBuf> = reports.iter().filter(|f| f.exists()).collect();
    if !status.success() || !found.is_empty() {
        let detail: String = found
            .iter()
            .map(|f| {
                let t = String::from_utf8_lossy(&fs::read(f).unwrap_or_default()).into_owned();
                t.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect::<String>()
            })
            .collect();
        return Err(format!("Ren'Py '{}' failed ({status}).\n{detail}", args[0].to_string_lossy()));
    }
    Ok(())
}

/// Loads the game (compiling our tl files) and quits. Never use Ren'Py's `compile` command
/// here: it renames every .rpyc without a .rpy source to .rpyc.bak, which breaks games
/// shipped without source.
pub fn check_renpy(game: &Path) -> R<()> {
    run_renpy(game, &["quit".as_ref()])
}

/// Dump the game's script with the hook, run inside the game's own engine (no decompiling).
pub fn extract(game: &Path, work: &Path, name: &str) -> R<Dump> {
    let hook = game.join("game").join("zz_vntl_dump.rpy");
    let out = game.join("vntl_dump.json"); // short path on purpose: Ren'Py 7's Python 2 has the 260-char limit
    fs::write(&hook, HOOK).map_err(|e| e.to_string())?;
    let result = run_renpy(game, &["vntl_dump".as_ref(), out.as_os_str()]);
    let _ = fs::remove_file(&hook);
    let _ = fs::remove_file(hook.with_extension("rpyc"));
    result?;
    fs::create_dir_all(work).map_err(|e| e.to_string())?;
    let dest = work.join(name);
    fs::copy(&out, &dest).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&out);
    load_json(&dest)
}

pub fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> R<T> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// All of it or none: a stop or a power cut mid-write must not cost hours of saved translations.
pub fn save_json<T: Serialize>(path: &Path, data: &T) -> R<()> {
    let json = serde_json::to_vec_pretty(data).map_err(|e| e.to_string())?;
    crate::write_whole(path, &json).map_err(|e| format!("{}: {e}", path.display()))
}

// ---------- the MC's name and similar variables ----------

/// {var: [japanese literals]} for [var]s that can hold a Japanese value, e.g. the MC's
/// name with its default/fallback. Player-typed names are never in this list.
/// Names that are one character, as {other name: main name}, from how a script names its speakers.
/// A code with a suffix is its base code in another guise, as `kd_q` is `kd` and `ym2` is `ym`. A
/// numbered name variable belongs with its base too, as `pc2` does with `pc`, and so does the speaker
/// that shows the variable: `[pc]`. shortcut: only these conventions, seen on one game; other games
/// that follow them are assumed to mean the same, so a wrong link shows in the log, not as an error.
pub fn aliases(dump: &Dump) -> HashMap<String, String> {
    let digits = |s: &str| s.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
    let mut out = HashMap::new();
    for (code, name) in &dump.names {
        let base = digits(code.split('_').next().unwrap_or(code));
        // ？？？ hides many people, so it says nothing about who this is
        if let Some(main) = dump.names.get(&base).filter(|m| base != *code && *m != name && name.chars().any(char::is_alphabetic)) {
            out.insert(name.clone(), main.clone());
        }
    }
    let vars = name_vars(dump);
    for (var, values) in &vars {
        let Some(main) = vars.get(&digits(var)).and_then(|v| v.first()) else { continue };
        out.extend(values.iter().filter(|v| *v != main).map(|v| (v.clone(), main.clone())));
        out.insert(format!("[{var}]"), main.clone());
    }
    out
}

pub fn name_vars(dump: &Dump) -> IndexMap<String, Vec<String>> {
    static QUOTED: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"["']([^"']+)["']"#).unwrap());
    let mut out: IndexMap<String, Vec<String>> = IndexMap::new();
    for (v, info) in &dump.shown_vars {
        let mut vals: Vec<String> = info.init.iter().filter(|s| !s.is_empty()).cloned().collect();
        vals.extend(info.assigns.iter().flat_map(|a| QUOTED.captures_iter(&a.code).map(|c| c[1].to_string())));
        let mut seen = HashSet::new();
        let vals: Vec<String> = vals.into_iter().filter(|x| JA.is_match(x) && seen.insert(x.clone())).collect();
        if !vals.is_empty() {
            out.insert(v.clone(), vals);
        }
    }
    for (k, vals) in &dump.defaults {
        // `default pc = "..."` style builds: "store.pc" -> "pc", "store.persistent.pc" -> "persistent.pc"
        let name = k.split_once('.').map_or(k.as_str(), |(_, rest)| rest);
        out.entry(name.to_string()).or_insert_with(|| vals.clone());
    }
    out
}

// ---------- fitting English into the original layout ----------

pub struct Limits {
    /// (units per line, max lines)
    pub adv: (Option<usize>, usize),
    /// (units per line, max lines, max page lines)
    pub nvl: (Option<usize>, usize, usize),
}

/// Smallest value that at least `share` of `values` do not exceed.
fn covering(values: &[usize], share: f64) -> usize {
    let mut vs = values.to_vec();
    vs.sort_unstable();
    if vs.is_empty() {
        return 0;
    }
    let i = ((vs.len() as f64 * share).ceil() as usize).saturating_sub(1).min(vs.len() - 1);
    vs[i]
}

/// Limits for English, from what (nearly) all lines and pages of a layout known to fit use:
/// the official English (`trusted`, {line key: text}) when there is one, else the original
/// Japanese. The few longest are ignored as they usually overflow in the original too.
/// `en_size` is the English text size when the translation changes it.
pub fn layout_limits(dump: &Dump, en_size: Option<i64>, trusted: &IndexMap<String, String>) -> Limits {
    let lay = &dump.layout;
    let gui = &lay["gui"];
    let screen_w = lay["screen"][0].as_f64().unwrap_or(1280.0);
    let num = |v: &Value| v.as_f64().filter(|x| *x != 0.0);
    // (Japanese units per line, English units per line, Japanese/English line-height ratio)
    let metrics = |style: &str, gui_key: &str| -> (Option<usize>, Option<usize>, f64) {
        let st = &lay[style];
        let (Some(size), Some(width)) = (num(&st["size"]).or(num(&gui["text_size"])), num(&st["xmaximum"]).or(num(&gui[gui_key])))
        else {
            return (None, None, 1.0);
        };
        if size <= 0.0 {
            return (None, None, 1.0);
        }
        let width = if width <= 1.0 { width * screen_w } else { width };
        let en = en_size.map(|s| s as f64).unwrap_or(size);
        let lead = st["line_leading"].as_f64().unwrap_or(0.0);
        let units = |sz: f64| ((width / (sz / 2.0)) as usize).max(10);
        (Some(units(size)), Some(units(en)), (size + lead) / (en + lead))
    };
    let m = [metrics("say_dialogue", "dialogue_width"), metrics("nvl_dialogue", "nvl_text_width")];
    let idx = |mode: &str| (mode == "nvl") as usize;
    // Measure the trusted English at English width, or the Japanese at Japanese width.
    let per_line = |mode: &str| if trusted.is_empty() { m[idx(mode)].0 } else { m[idx(mode)].1 };
    let nvl: HashSet<String> = dump.nvl.iter().cloned().collect();
    let (mut used, mut pages, mut page, mut whole) = ([Vec::new(), Vec::new()], Vec::new(), 0, true);
    for b in &dump.blocks {
        for (i, it) in b.items.iter().enumerate() {
            if it.is_nvl_clear() {
                if whole {
                    pages.push(page); // only pages made entirely of trusted lines
                }
                (page, whole) = (0, true);
            }
            let mode = text_mode(it, &nvl);
            let Some(units) = per_line(mode).filter(|_| it.is_say()) else { continue };
            let text = if trusted.is_empty() { Some(it.what()) } else { trusted.get(&key(b, i)).map(String::as_str) };
            let Some(text) = text else {
                whole = whole && mode != "nvl";
                continue;
            };
            let shown = format!("{}{}", leading_newlines(it.what()), text.trim_start_matches('\n'));
            let n = wrap_lines(&shown, units);
            used[idx(mode)].push(n);
            if mode == "nvl" {
                page += n;
            }
        }
    }
    if whole {
        pages.push(page);
    }
    pages.retain(|p| *p > 0);
    // Same box height in English lines: more lines fit when the English text is smaller.
    let fit = |mode: &str, n: usize| -> usize {
        if n == 0 {
            0
        } else if !trusted.is_empty() {
            n
        } else {
            ((n as f64 * m[idx(mode)].2 + 1e-9) as usize).max(1)
        }
    };
    let mut l = Limits {
        adv: (m[0].1, fit("adv", covering(&used[0], 0.999))),
        nvl: (m[1].1, fit("nvl", covering(&used[1], 0.999)), fit("nvl", covering(&pages, 0.99))),
    };
    // A partial reference can leave a mode without trusted lines or whole pages, and a limit of 0
    // would put every line on its own page.
    // shortcut: only 0 falls back, so a few short trusted lines still set a tight limit; add a
    // minimum sample once that shows up on a real reference.
    if !trusted.is_empty() && (l.adv.1 == 0 || l.nvl.1 == 0 || l.nvl.2 == 0) {
        let ja = layout_limits(dump, en_size, &IndexMap::new());
        for (limit, from_ja) in [(&mut l.adv.1, ja.adv.1), (&mut l.nvl.1, ja.nvl.1), (&mut l.nvl.2, ja.nvl.2)] {
            if *limit == 0 {
                *limit = from_ja;
            }
        }
    }
    l
}

// ---------- writing the translation ----------

// Input fields show their default text as-is; run it through the string table so a
// Japanese default (e.g. the MC's name) is offered in English. Typed text is untouched.
const INPUT_RPY: &str = r#"
init 999 python:
    def _vntl_patch_input():
        Input = renpy.display.behavior.Input
        original = Input.__init__
        def __init__(self, *args, **kwargs):
            original(self, *args, **kwargs)
            if isinstance(kwargs.get("replaces"), Input):
                return
            text = renpy.translate_string(self.content)
            if text != self.content:
                self.default = text
                self.update_text(text, self.editable)
                self.caret_pos = len(text)
        Input.__init__ = __init__
    _vntl_patch_input()
"#;

pub fn tl_dir(game: &Path, lang: &str) -> PathBuf {
    game.join("game").join("tl").join(lang)
}

pub fn remove_written(tl: &Path) {
    for e in fs::read_dir(tl).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("vntl_") && (name.ends_with(".rpy") || name.ends_with(".rpyc")) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Write the translation and check that Ren'Py loads it. If it does not, take the translation
/// out again so the game still starts, and report the error.
pub fn install(game: &Path, dump: &Dump, cache: &Cache, lang: &str) -> R<usize> {
    install_with(game, dump, cache, lang, check_renpy)
}

pub fn install_with(game: &Path, dump: &Dump, cache: &Cache, lang: &str, check: impl Fn(&Path) -> R<()>) -> R<usize> {
    let n = write_renpy(game, dump, cache, lang)?;
    if let Err(e) = check(game) {
        remove_written(&tl_dir(game, lang));
        return Err(e);
    }
    Ok(n)
}

pub fn write_renpy(game: &Path, dump: &Dump, cache: &Cache, lang: &str) -> R<usize> {
    let tl = tl_dir(game, lang);
    fs::create_dir_all(&tl).map_err(|e| e.to_string())?;
    remove_written(&tl);
    let nv: HashSet<String> = name_vars(dump).into_keys().collect();
    let official: HashSet<&String> = cache.official.iter().collect();
    let trusted: IndexMap<String, String> =
        cache.official.iter().filter_map(|k| cache.lines.get(k).map(|t| (k.clone(), t.clone()))).collect();
    let limits = layout_limits(dump, cache.style.text_size, &trusted);
    let nvl: HashSet<String> = dump.nvl.iter().cloned().collect();
    let (mut page, mut written) = (0usize, 0usize);
    let mut per_file: IndexMap<String, Vec<String>> = IndexMap::new();

    'blocks: for b in &dump.blocks {
        if b.items.iter().any(Item::is_nvl_clear) {
            page = 0;
        }
        if !b.items.iter().any(Item::is_say) {
            continue; // e.g. a lone `nvl clear`, which Ren'Py also treats as translatable
        }
        let mut lines = Vec::new();
        for (i, it) in b.items.iter().enumerate() {
            let Some(tpl) = &it.tpl else {
                lines.push(it.code.clone().unwrap_or_default());
                continue;
            };
            let k = key(b, i);
            let Some(tr) = cache.lines.get(&k) else { continue 'blocks }; // partly translated: leave it in Japanese
            let mode = text_mode(it, &nvl);
            let (units, max_lines) = if mode == "nvl" { (limits.nvl.0, limits.nvl.1) } else { limits.adv };
            let (lead, core) = (leading_newlines(it.what()), tr.trim_start_matches('\n')); // the original's lead goes on every part
            // Official lines keep the official translators' layout; `extend` adds to the line already
            // shown, so splitting it would not help. Only the model's lines are fitted.
            let fitted = !official.contains(&k);
            let parts = if fitted && it.who.as_deref() != Some("extend") {
                split_to_fit(core, units, max_lines.saturating_sub(lead.len()).max(1))
            } else {
                vec![core.to_string()]
            };
            for p in parts {
                let part = format!("{lead}{p}");
                if let (Some(u), "nvl") = (units, mode) {
                    let n = wrap_lines(&part, u);
                    if fitted && page > 0 && page + n > limits.nvl.2 {
                        lines.push("nvl clear".into()); // English fills NVL pages faster than Japanese
                        page = 0;
                    }
                    page += n;
                }
                lines.push(tpl.replace(TOKEN, &encode_say_string(&add_t(&part, &nv))));
            }
        }
        let body: String = lines.iter().map(|l| format!("    {l}\n")).collect();
        let stem = Path::new(&b.file).file_stem().map_or(String::new(), |s| s.to_string_lossy().into_owned());
        per_file.entry(stem).or_default().push(format!(
            "# {}:{}\ntranslate {lang} {}:\n\n{body}\n",
            b.file,
            b.line,
            b.id.replace('.', "_")
        ));
        written += 1;
    }
    let write = |name: &str, text: String| fs::write(tl.join(name), text).map_err(|e| e.to_string());
    for (stem, chunks) in &per_file {
        write(&format!("vntl_{stem}.rpy"), chunks.concat())?;
    }

    // Shortened interface labels replace the long text only while it is unchanged.
    let mut strings: IndexMap<String, String> = cache
        .strings
        .iter()
        .map(|(jp, en)| {
            let fit = cache.fit.get(jp).filter(|f| f.first().and_then(Value::as_str) == Some(en.as_str()));
            (jp.clone(), fit.and_then(|f| f.get(1)?.as_str()).unwrap_or(en).to_string())
        })
        .collect();
    for name in dump.names.values() {
        if add_t(name, &nv) != *name {
            let cur = strings.get(name).cloned().unwrap_or_else(|| name.clone());
            strings.insert(name.clone(), add_t(&cur, &nv)); // e.g. the speaker "[pc]" -> "[pc!t]"
        }
    }
    if !strings.is_empty() {
        // String tables are read back as plain Python literals (Ren'Py's quote_unicode/parse_string),
        // not like dialogue, so no "\ " space escaping here: it would stay in the key and never match.
        let q = |s: &str| serde_json::to_string(s).unwrap();
        let mut sorted: Vec<_> = strings.iter().collect();
        sorted.sort();
        let body: String =
            sorted.iter().map(|(o, n)| format!("    old {}\n    new {}\n\n", q(o), q(&add_t(n, &nv)))).collect();
        write("vntl_strings.rpy", format!("translate {lang} strings:\n\n{body}"))?;
    }
    write("vntl_language.rpy", format!("define config.language = \"{lang}\"\n{INPUT_RPY}"))?;
    if !cache.style.code.is_empty() {
        // e.g. the official translation's English font and text size
        let body: String = cache
            .style
            .code
            .iter()
            .flat_map(|src| src.lines())
            .map(|l| if l.trim().is_empty() { "\n".to_string() } else { format!("    {l}\n") })
            .collect();
        write("vntl_style.rpy", format!("translate {lang} python:\n{body}"))?;
    }
    Ok(written)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn dump(v: Value) -> Dump {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_script_says_which_names_are_one_character() {
        let d = dump(json!({"blocks": [], "names": {
            "kd": "ミサキ", "kd_q": "店員", "ym": "ユミ", "ym2": "先生", "so": "ソウ", "so_b": "ソウ", "so_q": "？？？",
            "ex1": "客A", "ex2": "客B", "pl": "[pc]"},
            "shown_vars": {"pc": {"assigns": [{"code": "pc = \"ハヤト\""}]}, "pc2": {"assigns": [{"code": "pc2 = \"アオイ\""}, {"code": "pc2 = \"リナ\""}]}}}));
        let got = aliases(&d);
        let of = |n: &str| got.get(n).map(String::as_str);
        assert_eq!((of("店員"), of("先生")), (Some("ミサキ"), Some("ユミ")));
        assert_eq!((of("アオイ"), of("リナ"), of("[pc]"), of("[pc2]")), (Some("ハヤト"), Some("ハヤト"), Some("ハヤト"), Some("ハヤト")));
        assert_eq!(got.len(), 6, "the same name, ？？？ and the numbered extras link nothing");
    }

    pub fn cache(v: Value) -> Cache {
        serde_json::from_value(v).unwrap()
    }

    pub fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("vntl-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn read(p: PathBuf) -> String {
        fs::read_to_string(p).unwrap()
    }

    #[test]
    fn rpg_maker_2000_and_2003_are_told_by_their_data_files() {
        let game = tmp();
        assert_eq!(detect(&game), "unknown");
        fs::write(game.join("RPG_RT.exe"), "").unwrap();
        assert_eq!(detect(&game), "unknown"); // a program's name alone proves nothing
        fs::write(game.join("RPG_RT.LDB"), "").unwrap(); // old games often have upper-case names
        assert_eq!(detect(&game), "rpgmaker-2000/2003");
    }

    fn put(game: &Path, file: &str, bytes: &[u8]) {
        let path = game.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn engines_are_told_by_their_files() {
        for (engine, files) in [
            ("siglusengine", &["Scene.pck", "Gameexe.dat"][..]),
            ("reallive", &["SEEN.TXT", "GAMEEXE.INI"]),
            ("catsystem2", &["scene.int"]),
            ("bgi", &["sysprg.arc"]),
            ("artemis", &["root.pfs"]),
            ("wolf-rpg", &["Data.wolf"]),
            ("wolf-rpg", &["Data/BasicData.wolf"]),
            ("yu-ris", &["data.ypf"]),
            ("yu-ris", &["pac/bn.ypf"]),
            ("qlie", &["GameData/data0.pack"]),
            ("advhd", &["Rio.arc"]),
            ("fvp", &["game.hcb"]),
            ("malie", &["malie.ini"]),
            ("softpal", &["dll/PAL.dll"]),
            ("rpgmaker-xp/vx/ace", &["Data/Scripts.rvdata"]),
        ] {
            let game = tmp();
            files.iter().for_each(|f| put(&game, f, b""));
            assert_eq!(detect(&game), engine, "{files:?}");
        }
    }

    #[test]
    fn a_common_archive_name_needs_the_majiro_mark() {
        let game = tmp();
        put(&game, "scenario.arc", b"something else");
        assert_eq!(detect(&game), "unknown");
        put(&game, "scenario.arc", b"MajiroArcV3.000");
        assert_eq!(detect(&game), "majiro");
    }

    #[test]
    fn packaged_tyranoscript_is_found_by_the_file_names_inside() {
        let asar = |index: &[u8]| [&[0; 12], &(index.len() as u32).to_le_bytes()[..], index].concat();
        let game = tmp();
        put(&game, "resources/app.asar", &asar(br#"{"files":{"main.js":{}}}"#));
        assert_eq!(detect(&game), "unknown"); // Electron alone is no engine
        put(&game, "resources/app.asar", &asar(br#"{"files":{"data":{"files":{"scenario":{}}},"tyrano":{}}}"#));
        assert_eq!(detect(&game), "tyranoscript");

        let names = b"data/scenario/first.ks tyrano/tyrano.js";
        let size = (names.len() as u32).to_le_bytes();
        let exe = [&b"MZ program"[..], names, b"PK\x05\x06", &[0; 8], &size, &[0; 6]].concat();
        let game = tmp();
        put(&game, "game.exe", &exe);
        assert_eq!(detect(&game), "unknown"); // an .exe is only read beside the NW.js files
        put(&game, "nw.pak", b"");
        assert_eq!(detect(&game), "tyranoscript");
    }

    #[test]
    fn the_game_is_found_below_a_wrapper_folder() {
        let dir = tmp();
        assert_eq!(game_root(&dir), dir); // nothing here
        put(&dir, "launcher.exe", b"");
        put(&dir, "wrapper/one/data/RPG_RT.ldb", b"");
        assert_eq!(game_root(&dir), dir.join("wrapper/one/data"));
        put(&dir, "wrapper/two/root.pfs", b"");
        assert_eq!(game_root(&dir), dir); // two games: not ours to choose
        assert_eq!(game_root(&dir.join("wrapper/two")), dir.join("wrapper/two"));
    }

    #[test]
    fn writes_blocks_strings_and_skips_partial_or_empty_blocks() {
        let d = tmp();
        let mut dm = dump(json!({"blocks": [
            {"id": "start_1", "file": "game/10.rpyc", "line": 5, "label": "start",
             "items": [{"code": "nvl clear"}, {"tpl": "e \"@@VNTL@@\"", "who": "e", "what": "こんにちは"}]},
            {"id": "start_2", "file": "game/10.rpyc", "line": 6, "label": "start",
             "items": [{"tpl": "\"@@VNTL@@\"", "who": null, "what": "未訳"}]},
            {"id": "start_3", "file": "game/10.rpyc", "line": 7, "label": "start", "items": [{"code": "nvl clear"}]}],
            "strings": [{"text": "はい", "kind": "menu"}], "names": {}}));
        let c = cache(json!({"lines": {"start_1#1": "Hello \"you\""}, "strings": {"はい": "Yes"}}));
        assert_eq!(write_renpy(&d, &dm, &c, "english").unwrap(), 1);
        let out = read(tl_dir(&d, "english").join("vntl_10.rpy"));
        assert!(out.contains("translate english start_1:\n\n    nvl clear\n    e \"Hello \\\"you\\\"\"\n"), "{out}");
        assert!(!out.contains("start_2") && !out.contains("start_3"));
        assert!(read(tl_dir(&d, "english").join("vntl_strings.rpy")).contains("new \"Yes\""));
        assert!(read(tl_dir(&d, "english").join("vntl_language.rpy")).contains("Input.__init__ = __init__"));
        dm.blocks.clear();
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn mc_name_gets_t_and_string_keys_keep_spaces() {
        let d = tmp();
        let dm = dump(json!({"blocks": [{"id": "b", "file": "game/10.rpyc", "line": 1,
            "items": [{"tpl": "m \"@@VNTL@@\"", "who": "m", "what": "[pc]、起きて"}]}],
            "names": {"m": "[pc]"}, "strings": [],
            "shown_vars": {"pc": {"init": null, "assigns": [{"code": "pc = (name)"}, {"code": "pc = \"ハヤト\""}]},
                           "count": {"init": null, "assigns": [{"code": "count = 3"}]}}}));
        assert_eq!(name_vars(&dm).into_iter().collect::<Vec<_>>(), [("pc".to_string(), vec!["ハヤト".to_string()])]);
        let c = cache(json!({"lines": {"b#0": "[pc], wake up"}, "strings": {"ハヤト": "Hayato", "注      意": "WARNING"}}));
        write_renpy(&d, &dm, &c, "english").unwrap();
        assert!(read(tl_dir(&d, "english").join("vntl_10.rpy")).contains("m \"[pc!t], wake up\""));
        let s = read(tl_dir(&d, "english").join("vntl_strings.rpy"));
        assert!(s.contains("old \"[pc]\"\n    new \"[pc!t]\"") && s.contains("old \"ハヤト\"\n    new \"Hayato\""));
        assert!(s.contains("old \"注      意\""), "{s}");
        let _ = fs::remove_dir_all(d);
    }

    fn lay_dump() -> Dump {
        dump(json!({"layout": {"say_dialogue": {"size": 20, "xmaximum": 100}, "nvl_dialogue": {"size": 20, "xmaximum": 100},
                               "screen": [800, 600]}, "nvl": ["n"], "names": {}, "strings": [],
            "blocks": [{"id": "x1", "file": "game/1.rpyc", "line": 1, "items": [
                {"tpl": "a \"@@VNTL@@\"", "who": "a", "what": "あいうえおかきくけこ"},
                {"tpl": "n \"@@VNTL@@\"", "who": "n", "what": "あいうえお"},
                {"tpl": "n \"@@VNTL@@\"", "who": "n", "what": "あいうえお"}]}]}))
    }

    #[test]
    fn limits_from_japanese_english_size_and_official_lines() {
        let l = layout_limits(&lay_dump(), None, &IndexMap::new());
        assert_eq!((l.adv, l.nvl), ((Some(10), 2), (Some(10), 1, 2)));
        assert_eq!(layout_limits(&lay_dump(), Some(10), &IndexMap::new()).adv, (Some(20), 4));
        let trusted: IndexMap<String, String> =
            [("x1#0", "a ".repeat(25)), ("x1#1", "b".into()), ("x1#2", "c".into())].map(|(k, v)| (k.to_string(), v)).into();
        let l = layout_limits(&lay_dump(), Some(10), &trusted);
        assert_eq!((l.adv, l.nvl), ((Some(20), 3), (Some(20), 1, 2)));
    }

    #[test]
    fn mode_without_official_lines_keeps_japanese_limits() {
        let adv_only: IndexMap<String, String> = [("x1#0".to_string(), "a".to_string())].into();
        assert_eq!(layout_limits(&lay_dump(), Some(10), &adv_only).nvl, (Some(20), 2, 4));
        let nvl_only: IndexMap<String, String> = [("x1#1", "b"), ("x1#2", "c")].map(|(k, v)| (k.to_string(), v.to_string())).into();
        assert_eq!(layout_limits(&lay_dump(), None, &nvl_only).adv, (Some(10), 2));

        let d = tmp();
        let c = cache(json!({"lines": {"x1#0": "Aaaa.", "x1#1": "Bbbb.", "x1#2": "Cccc."}, "strings": {}, "official": ["x1#0"]}));
        write_renpy(&d, &lay_dump(), &c, "english").unwrap();
        let out = read(tl_dir(&d, "english").join("vntl_1.rpy"));
        assert!(out.contains("    n \"Bbbb.\"\n    n \"Cccc.\"\n"), "{out}");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn model_lines_split_and_page_breaks_official_lines_untouched() {
        let d = tmp();
        let lines = json!({"x1#0": "Aaaa bbbb. Cccc dddd. Eeee.", "x1#1": "Ffff gggg. Hhhh.", "x1#2": "Iiii."});
        write_renpy(&d, &lay_dump(), &cache(json!({"lines": lines, "strings": {}})), "english").unwrap();
        let out = read(tl_dir(&d, "english").join("vntl_1.rpy"));
        assert!(out.contains("    a \"Aaaa bbbb. Cccc dddd.\"\n    a \"Eeee.\"\n"), "{out}");
        assert!(out.contains("    n \"Ffff gggg.\"\n    n \"Hhhh.\"\n    nvl clear\n    n \"Iiii.\"\n"), "{out}");
        let c = cache(json!({"lines": lines, "strings": {}, "official": ["x1#0", "x1#1", "x1#2"]}));
        write_renpy(&d, &lay_dump(), &c, "english").unwrap();
        let out = read(tl_dir(&d, "english").join("vntl_1.rpy"));
        assert!(out.contains("    a \"Aaaa bbbb. Cccc dddd. Eeee.\"\n    n \"Ffff gggg. Hhhh.\"\n    n \"Iiii.\"\n"), "{out}");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn keeps_leading_newline_and_shortened_labels() {
        let d = tmp();
        let dm = dump(json!({"layout": {"say_dialogue": {"size": 20, "xmaximum": 100}, "screen": [800, 600]},
            "nvl": [], "names": {}, "strings": [], "blocks": [{"id": "y1", "file": "game/2.rpyc", "line": 1,
            "items": [{"tpl": "a \"@@VNTL@@\"", "who": "a", "what": "\nあいうえおかきくけこあいうえおかきくけこ"}]}]}));
        let c = cache(json!({"lines": {"y1#0": "Aaaa bbbb. Cccc dddd."},
            "strings": {"タイトルに戻る": "Return to Title", "設定": "Settings"},
            "fit": {"タイトルに戻る": ["Return to Title", "Back to Title", 1], "設定": ["Config", "Cfg"]}}));
        write_renpy(&d, &dm, &c, "english").unwrap();
        let out = read(tl_dir(&d, "english").join("vntl_2.rpy"));
        assert!(out.contains("a \"\\nAaaa bbbb.") , "{out}");
        let s = read(tl_dir(&d, "english").join("vntl_strings.rpy"));
        assert!(s.contains("new \"Back to Title\"") && s.contains("new \"Settings\""));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn failed_check_takes_translation_out_and_save_is_whole() {
        let d = tmp();
        let dm = dump(json!({"blocks": [], "names": {}, "strings": []}));
        let c = cache(json!({"lines": {}, "strings": {"設定": "Cfg"}}));
        let r = install_with(&d, &dm, &c, "english", |_| Err("Ren'Py 'quit' failed".into()));
        assert!(r.is_err());
        let left: Vec<_> = fs::read_dir(tl_dir(&d, "english")).unwrap().flatten().collect();
        assert!(left.is_empty());
        let p = d.join("cache.json");
        save_json(&p, &c).unwrap();
        let back: Cache = load_json(&p).unwrap();
        assert_eq!(back.strings["設定"], "Cfg");
        assert!(!d.join("cache.json.tmp").exists());
        let _ = fs::remove_dir_all(d);
    }
}
