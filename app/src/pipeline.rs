//! The translate command: official lines first, then the model for what is left, then install.

use crate::llm::{is_vntl, notes_fill_prompt, runaway, translate_batch, translate_vntl, trim_memory, Chat, SYSTEM};
use crate::menus::{paint_plates, read_plate};
use crate::ocr::Reader;
use crate::reference::{apply_reference, copy_reference_files, reference_style, sprites};
use crate::renpy::aliases;
use crate::vndb::{Found, Person};
use crate::renpy::{detect, extract, install, key, load_json, name_vars, save_json, tl_dir, Block, Cache, Dump, StringEntry};
use crate::text::{ascii_punctuation, markup_ok, same_markup, text_units, INTERP_FULL, JA, looks_like_name, MARKUP, META_REPLY, UNITY_MARKUP};
use crate::{liar, nscripter, tyrano, fvp, softpal, artemis, kirikiri, advhd, bgi, yuris, yarn, malie, rpgmaker, unity, wolf, majiro, catsystem, reallive, qlie, siglus};
use crate::R;
use regex::Regex;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

pub struct Options {
    pub game: PathBuf,
    pub lang: String,
    pub label: Option<String>,
    pub file: Option<String>,
    pub reference: Option<PathBuf>,
    /// with a label/file, also translate the interface (always on otherwise)
    pub ui: bool,
    pub redump: bool,
    pub batch: usize,
    pub model: String,
    /// Unity: let the model translate text the game already has in the target language
    /// (character names still come from the game, as the glossary)
    pub retranslate: bool,
    /// start over: set the saved translations aside and translate everything again
    pub fresh: bool,
    /// a vision model, to read and translate text painted into menu images; it takes instructions,
    /// so on RPG Maker it does the names and menu words too, and on Ren'Py when the main model is VNTL
    pub vision: Option<Box<dyn Chat + Send>>,
    /// repaint menu images, which needs `vision`
    pub images: bool,
    /// Ren'Py: also paint English over text in artwork, which needs `vision` and the text reader
    pub art: bool,
    /// earlier lines a VNTL model sees: shared, so the slider can change it during a run
    pub history: Arc<AtomicUsize>,
}

pub enum Event {
    Log(String),
    Progress(usize, usize),
    /// a line the model just finished: the original and its translation
    Line(String, String),
}

fn string_kind(kind: &str) -> &'static str {
    match kind {
        "menu" => "menu choice",
        "ui" => "interface text",
        _ => "in-game text",
    }
}

pub fn selected_blocks<'a>(dump: &'a Dump, label: Option<&str>, file: Option<&str>) -> Vec<&'a Block> {
    let stem = |f: &str| Path::new(f).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    dump.blocks
        .iter()
        .filter(|b| label.is_none_or(|l| b.label == l) && file.is_none_or(|f| stem(&b.file) == f))
        .collect()
}

/// {line key: speaker shown to the model}; `extend` continues the previous speaker's line.
pub fn speakers(dump: &Dump, who_name: &HashMap<String, String>) -> HashMap<String, String> {
    let (mut out, mut prev) = (HashMap::new(), "narration".to_string());
    for b in &dump.blocks {
        for (i, it) in b.items.iter().enumerate().filter(|(_, it)| it.is_say()) {
            if it.who.as_deref() != Some("extend") {
                prev = match &it.who {
                    Some(w) => who_name.get(w).cloned().unwrap_or_else(|| w.clone()),
                    None => "narration".into(),
                };
            }
            out.insert(key(b, i), prev.clone());
        }
    }
    out
}

/// Whether the saved reference.json came from this folder (`from` records the folder it was read from).
/// shortcut: a build updated in place at the same path goes unnoticed, --redump reads it again
fn reference_is_from(from: &Path, ref_dir: &Path) -> bool {
    std::fs::read_to_string(from).is_ok_and(|s| s == ref_dir.to_string_lossy())
}

/// Copy what an official translation already has: lines, strings, translated images, and its
/// English font and text size. The model only gets what is left.
fn apply_official(o: &Options, dump: &Dump, cache: &mut Cache, work: &Path, log: &dyn Fn(String)) -> R<()> {
    let ref_dir = std::path::absolute(o.reference.as_ref().unwrap()).map_err(|e| e.to_string())?;
    if detect(&ref_dir) != "renpy" {
        return Err(format!("Reference {} is not a Ren'Py game", ref_dir.display()));
    }
    let (ref_path, from) = (work.join("reference.json"), work.join("reference_from.txt"));
    let reference: Dump = if ref_path.exists() && reference_is_from(&from, &ref_dir) && !o.redump {
        load_json(&ref_path)?
    } else {
        log("Reading the official translation's script…".into());
        let read = extract(&ref_dir, work, "reference.json")?;
        std::fs::write(&from, ref_dir.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
        read
    };
    let hits = apply_reference(dump, &reference, cache, &selected_blocks(dump, o.label.as_deref(), o.file.as_deref()), &o.lang);
    let files = copy_reference_files(&o.game, &ref_dir, &o.lang)?;
    crate::menus::disown_images(work, &files)?;
    let (style, fonts) = reference_style(&o.game, &ref_dir, &reference, &o.lang)?;
    cache.style = style;
    if !files.is_empty() || !fonts.is_empty() {
        // lets "Remove translation" find them again
        let list: Vec<&String> = files.iter().chain(&fonts).collect();
        let text: String = list.iter().map(|f| format!("{f}\n")).collect();
        std::fs::write(tl_dir(&o.game, &o.lang).join("vntl_files.txt"), text).map_err(|e| e.to_string())?;
    }
    let style = match cache.style.code.is_empty() {
        true => "none".to_string(),
        false => format!("text size {}, fonts {fonts:?}", cache.style.text_size.map_or("unchanged".into(), |s| s.to_string())),
    };
    log(format!(
        "Official reference: {} lines by block id, {} by identical text, {} names/menus/interface strings, \
         {} translated images/files, English style: {style}",
        hits.by_id, hits.by_text, hits.strings, files.len()
    ));
    Ok(())
}

/// Character names first (incl. the MC's default name), so the model keeps its name choices
/// consistent. Returns (system prompt with a name glossary, {line key: speaker}, all name texts).
fn translate_names(chat: &dyn Chat, dump: &Dump, cache: &mut Cache, save: &dyn Fn(&Cache) -> R<()>)
    -> R<(String, HashMap<String, String>, BTreeSet<String>)> {
    let nv = name_vars(dump);
    let name_texts: BTreeSet<String> = dump.strings.iter().filter(|s| s.kind == "name").map(|s| s.text.clone())
        .chain(nv.values().flatten().cloned()).collect();
    let names: Vec<&String> = name_texts.iter().filter(|t| !cache.strings.contains_key(*t) && JA.is_match(t)).collect();
    if !names.is_empty() {
        let batch: Vec<(String, String)> = names.iter().map(|n| ("character name".to_string(), n.to_string())).collect();
        for (src, tr) in names.iter().zip(translate_batch(chat, SYSTEM, &MARKUP, &batch)?) {
            if let Some(tr) = tr {
                cache.strings.insert(src.to_string(), tr);
            }
        }
        save(cache)?;
    }
    let glossary: Vec<String> =
        name_texts.iter().filter_map(|s| cache.strings.get(s).map(|t| format!("{s} = {t}"))).collect();
    let system = if glossary.is_empty() {
        SYSTEM.to_string()
    } else {
        format!("{SYSTEM}\n\nCharacter names as you translated them earlier:\n{}", glossary.join("\n"))
    };
    // "[pc]" as a speaker resolves to the translated default name (e.g. Hayato).
    let dflt: HashMap<&String, String> = nv
        .iter()
        .filter_map(|(v, vals)| vals.last().map(|last| (v, cache.strings.get(last).unwrap_or(last).clone())))
        .collect();
    let who_name: HashMap<String, String> = dump
        .names
        .iter()
        .map(|(var, jp)| {
            let shown = cache.strings.get(jp).unwrap_or(jp);
            let resolved = INTERP_FULL.replace_all(shown, |c: &regex::Captures| {
                dflt.get(&c[1].to_string()).cloned().unwrap_or_else(|| c[0].to_string())
            });
            (var.clone(), resolved.into_owned())
        })
        .collect();
    Ok((system, speakers(dump, &who_name), name_texts))
}

/// Drop "<label> | " that an echoing model reply left in cached lines (only labels we send).
fn clean_echoes(dump: &Dump, cache: &mut Cache, spoken: &HashMap<String, String>) {
    for b in &dump.blocks {
        for (i, it) in b.items.iter().enumerate().filter(|(_, it)| it.is_say()) {
            let k = key(b, i);
            let Some(tr) = cache.lines.get_mut(&k) else { continue };
            let labels = [spoken.get(&k).cloned(), Some(it.who.clone().unwrap_or_else(|| "narration".into()))];
            for label in labels.into_iter().flatten() {
                let prefix = format!("{label} | ");
                if tr.starts_with(&prefix) && !it.what().starts_with(&prefix) {
                    *tr = tr[prefix.len()..].to_string();
                }
            }
        }
    }
}

static OPTION_MARK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:\d+[.)]|[-*•])\s*").unwrap());

/// Short interface labels must fit the space their Japanese label had (menus and names excluded).
/// cache.fit = {japanese: [english, english that fits, attempts]}; at most two attempts per label.
pub fn fit_labels(chat: &dyn Chat, system: &str, dump: &Dump, cache: &mut Cache, save: &dyn Fn(&Cache) -> R<()>,
                  log: &dyn Fn(String), cancel: &AtomicBool) -> R<()> {
    let kind: HashMap<&str, &str> = dump.strings.iter().map(|s| (s.text.as_str(), s.kind.as_str())).collect();
    // `default pc = "葵"` reaches the dump as a ui string, and install would shorten the name for good
    let name_values: HashSet<String> = name_vars(dump).into_values().flatten().collect();
    let is_label = |jp: &str| kind.get(jp) == Some(&"ui") && !name_values.contains(jp);
    cache.fit.retain(|jp, _| is_label(jp));
    // attempts so far for this English text (0 when it changed or is new)
    let attempts = |cache: &Cache, jp: &str, en: &str| -> u64 {
        match cache.fit.get(jp) {
            Some(f) if f.first().and_then(|v| v.as_str()) == Some(en) => f.get(2).and_then(|v| v.as_u64()).unwrap_or(1),
            _ => 0,
        }
    };
    let labels: Vec<(String, String)> = cache
        .strings
        .iter()
        .filter(|(jp, en)| {
            let tried = attempts(cache, jp, en);
            let failed = cache.fit.get(*jp).and_then(|f| f.get(1)?.as_str().map(|s| s == en.as_str())).unwrap_or(false);
            is_label(jp) && !jp.contains('\n') && text_units(jp) <= 24
                && text_units(en) > text_units(jp) && (tried == 0 || (failed && tried < 2))
        })
        .map(|(jp, en)| (jp.clone(), en.clone()))
        .collect();
    if !labels.is_empty() {
        log(format!("Shortening {} interface labels to fit the original layout", labels.len()));
    }
    for (jp, en) in labels {
        if cancel.load(Ordering::Relaxed) {
            return Err("Stopped".into());
        }
        let n = text_units(&jp);
        let reply = chat.chat(system, &format!(
            "This English interface label is too wide for its button: \"{en}\" (Japanese: {jp}). Give 5 shorter \
             versions with the same meaning, each at most {n} characters, one per line, nothing else."))?;
        // shortcut: a preamble above an unnumbered list still wins, skip it if real replies show one
        let numbered = reply.lines().any(|o| OPTION_MARK.is_match(o));
        let short = reply
            .lines()
            .filter(|o| !numbered || OPTION_MARK.is_match(o))
            .map(|o| OPTION_MARK.replace(o, "").trim().trim_matches('"').to_string())
            .find(|o| !o.is_empty() && text_units(o) <= n && markup_ok(&jp, o))
            .unwrap_or_else(|| en.clone());
        let tried = attempts(cache, &jp, &en);
        cache.fit.insert(jp, vec![en.into(), short.into(), (tried + 1).into()]);
        save(cache)?;
    }
    Ok(())
}

// ---------- character notes ----------

const CHARACTERS_HELP: &str = "# Notes the translation model gets about the characters, one per line, e.g.
# [character] Name: Hana (ハナ) | Gender: Female | Aliases: Hana-chan (ハナちゃん)
# Add what the text alone doesn't tell (genders above all). Lines starting with # are ignored.
# The window app's Characters list edits this file too.
";

/// One `[character]` line of the notes; `extra` keeps what else it says (aliases, role…).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Character {
    pub original: String,
    pub english: String,
    pub gender: String,
    pub extra: String,
}

impl Character {
    pub fn parse(line: &str) -> Option<Character> {
        let mut parts = line.trim().strip_prefix("[character] ")?.split(" | ");
        let name = parts.next()?.strip_prefix("Name: ")?.trim();
        let (english, original) = match name.strip_suffix(')').and_then(|n| n.rsplit_once(" (")) {
            Some((en, jp)) => (en.to_string(), jp.to_string()),
            None if JA.is_match(name) => (String::new(), name.to_string()),
            None => (name.to_string(), String::new()),
        };
        let (mut gender, mut extra) = (String::new(), Vec::new());
        for p in parts {
            match p.strip_prefix("Gender: ") {
                Some(g) => gender = g.trim().to_string(),
                None => extra.push(p),
            }
        }
        Some(Character { original, english, gender, extra: extra.join(" | ") })
    }

    pub fn line(&self) -> String {
        let name = match (self.english.trim(), self.original.trim()) {
            (en, "") => en.to_string(),
            ("", jp) => jp.to_string(),
            (en, jp) => format!("{en} ({jp})"),
        };
        let mut line = format!("[character] Name: {name}");
        for p in [(!self.gender.trim().is_empty()).then(|| format!("Gender: {}", self.gender.trim())), Some(self.extra.trim().to_string())] {
            if let Some(p) = p.filter(|p| !p.is_empty()) {
                line += &format!(" | {p}");
            }
        }
        line
    }
}

/// Where a game's saved translations are kept.
pub fn cache_file(game: &Path, lang: &str) -> PathBuf {
    game.join("vntl_work").join(format!("cache_{lang}.json"))
}

fn characters_path(game: &Path) -> PathBuf {
    game.join("vntl_work").join("characters.txt")
}

/// A file the user edits: an interruption leaves the old one, never half of it.
fn save_text(path: &Path, text: &str) -> R<()> {
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    crate::write_whole(path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// The list file's text; None if there is none yet. A file that can't be read is an error, not an
/// empty list: an empty list saved over it would erase the characters in it.
fn characters_text(game: &Path) -> R<Option<String>> {
    let path = characters_path(game);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}. Save it as UTF-8 to use it; it was left as it is.", path.display())),
    }
}

/// The characters listed in <game>/vntl_work/characters.txt.
pub fn load_characters(game: &Path) -> Vec<Character> {
    std::fs::read_to_string(characters_path(game)).unwrap_or_default().lines().filter_map(Character::parse).collect()
}

/// {japanese: english} of the list: a name spelled there goes into the game as written. A Japanese
/// "english" is a model's echo that character_notes added, not a spelling.
fn listed_names(game: &Path) -> HashMap<String, String> {
    load_characters(game)
        .into_iter()
        .filter(|c| JA.is_match(&c.original) && !c.english.is_empty() && !JA.is_match(&c.english))
        .map(|c| (c.original, c.english))
        .collect()
}

/// One link the script makes between two listed names, as the Characters tab shows it. `original`
/// is the other name as the game writes it; `name` and `main` are as the list spells them.
#[derive(Debug, PartialEq)]
pub struct Link {
    pub original: String,
    pub name: String,
    pub main: String,
    pub broken: bool,
}

fn unlinked_path(game: &Path) -> PathBuf {
    game.join("vntl_work").join("unlinked.txt")
}

/// Names the user has told apart from the character the script links them to.
fn unlinked(game: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(unlinked_path(game)).unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// The script's links, less the ones the user has broken.
fn kept_aliases(game: &Path, dump: &Dump) -> HashMap<String, String> {
    let broken = unlinked(game);
    let mut links = aliases(dump);
    links.retain(|name, _| !broken.contains(name));
    links
}

/// The links between this game's listed characters, broken or not. Only Ren'Py saves a script to
/// read them from, and only once it has been translated or dumped.
pub fn links(game: &Path) -> Vec<Link> {
    let Ok(dump) = load_json::<Dump>(&game.join("vntl_work").join("dump.json")) else { return Vec::new() };
    let (list, all, broken) = (load_characters(game), aliases(&dump), unlinked(game));
    let shown = |jp: &String| list.iter().find(|c| c.original == *jp).map(|c| if c.english.is_empty() { c.original.clone() } else { c.english.clone() });
    list.iter()
        .filter_map(|c| {
            let main = shown(all.get(&c.original)?)?;
            Some(Link { original: c.original.clone(), name: shown(&c.original)?, main, broken: broken.contains(&c.original) })
        })
        .collect()
}

/// Break a link, or mend it. The next run works out genders and notes without it.
pub fn set_unlinked(game: &Path, original: &str, broken: bool) -> R<()> {
    let mut names = unlinked(game);
    match broken {
        true => names.insert(original.to_string()),
        false => names.remove(original),
    };
    save_text(&unlinked_path(game), &names.iter().map(|n| format!("{n}\n")).collect::<String>())
}

/// The game's synopsis is one line of the character notes, so every model reads it with them.
const SYNOPSIS: &str = "[synopsis] ";

pub fn load_synopsis(game: &Path) -> String {
    std::fs::read_to_string(characters_path(game)).unwrap_or_default().lines().find_map(|l| l.strip_prefix(SYNOPSIS)).unwrap_or("").to_string()
}

pub fn save_synopsis(game: &Path, text: &str) -> R<()> {
    let old = characters_text(game)?.unwrap_or_else(|| CHARACTERS_HELP.to_string());
    let mut lines: Vec<String> = old.lines().filter(|l| !l.starts_with(SYNOPSIS)).map(String::from).collect();
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !one.is_empty() {
        let at = lines.iter().position(|l| Character::parse(l).is_some()).unwrap_or(lines.len());
        lines.insert(at, format!("{SYNOPSIS}{one}"));
    }
    save_text(&characters_path(game), &(lines.join("\n") + "\n"))
}

fn descriptions_path(game: &Path) -> PathBuf {
    game.join("vntl_work").join("descriptions.json")
}

/// {original name: description} for reading on the Game Data tab. These stay out of the notes: a
/// whole cast's worth would take the room in the prompt that the story needs.
pub fn load_descriptions(game: &Path) -> indexmap::IndexMap<String, String> {
    load_json(&descriptions_path(game)).unwrap_or_default()
}

pub fn save_description(game: &Path, original: &str, text: &str) -> R<()> {
    let mut about = load_descriptions(game);
    about.insert(original.to_string(), text.to_string());
    std::fs::create_dir_all(game.join("vntl_work")).map_err(|e| e.to_string())?;
    save_json(&descriptions_path(game), &about)
}

/// Whether a row is this person: the same name, one of their aliases, or a part of their full name,
/// as a game's ソウ is ソウ・タチバナ.
fn same_person(c: &Character, p: &Person) -> bool {
    !c.original.is_empty()
        && (c.original == p.original || p.aliases.contains(&c.original) || p.original.split(['・', '＝', '=', ' ', '　']).any(|part| part == c.original))
}

/// Add what VNDB knows to the game's data. It only fills blanks: a spelling, gender or synopsis
/// that's there already may be the official one or the user's, and VNDB can have names wrong.
pub fn import_game_data(game: &Path, found: &Found) -> R<String> {
    let mut list = load_characters(game);
    let mut about = load_descriptions(game);
    let (mut added, mut genders) = (0, 0);
    for p in &found.people {
        let mut rows: Vec<usize> = (0..list.len()).filter(|&i| same_person(&list[i], p)).collect();
        if rows.is_empty() {
            list.push(Character { original: p.original.clone(), ..Default::default() });
            rows.push(list.len() - 1);
            added += 1;
        }
        for i in rows {
            let c = &mut list[i];
            if c.english.trim().is_empty() {
                c.english = p.name.clone();
            }
            if c.gender.trim().is_empty() && !p.gender.is_empty() {
                c.gender = p.gender.clone();
                genders += 1;
            }
            if c.extra.is_empty() && !p.aliases.is_empty() {
                c.extra = format!("Aliases: {}", p.aliases.join(", "));
            }
            if !p.description.is_empty() {
                about.entry(c.original.clone()).or_insert_with(|| p.description.clone());
            }
        }
    }
    save_characters(game, &list)?;
    std::fs::create_dir_all(game.join("vntl_work")).map_err(|e| e.to_string())?;
    save_json(&descriptions_path(game), &about)?;
    if load_synopsis(game).is_empty() {
        save_synopsis(game, &found.synopsis)?;
    }
    let mut said = format!("Imported {} ({}) from VNDB: {added} characters added, {genders} genders filled in.", found.title, found.id);
    if found.others > 0 {
        said += &format!(" Other games matched too ({}): if this is the wrong one, import by id, like v17.", found.others);
    }
    let twists = found.people.iter().filter(|p| p.twist).count();
    if twists > 0 {
        said += &format!(" {twists} characters turn out differently on VNDB, so the list has them as they appear.");
    }
    Ok(said)
}

/// Write the list back, keeping the file's comments and any notes that aren't character lines.
pub fn save_characters(game: &Path, list: &[Character]) -> R<()> {
    let old = characters_text(game)?.unwrap_or_else(|| CHARACTERS_HELP.to_string());
    let mut out: String = old.lines().filter(|l| Character::parse(l).is_none()).map(|l| format!("{l}\n")).collect();
    out.extend(list.iter().filter(|c| !(c.original.trim().is_empty() && c.english.trim().is_empty())).map(|c| c.line() + "\n"));
    save_text(&characters_path(game), &out)
}

/// The notes as the model should see them: one character, with their other names as aliases.
fn with_aliases(notes: Vec<String>, aliases: &HashMap<String, String>) -> Vec<String> {
    let rows: Vec<Option<Character>> = notes.iter().map(|n| Character::parse(n)).collect();
    let listed = |name: &String| rows.iter().flatten().any(|c| c.original == *name);
    notes.iter().zip(&rows).filter_map(|(note, row)| {
        let Some(c) = row else { return Some(note.clone()) };
        if aliases.get(&c.original).is_some_and(listed) {
            return None;
        }
        let others: Vec<String> = rows.iter().flatten().filter(|o| aliases.get(&o.original) == Some(&c.original)).map(|o| match o.english.as_str() {
            "" => o.original.clone(),
            en => format!("{en} ({})", o.original),
        }).collect();
        if others.is_empty() {
            return Some(note.clone());
        }
        let extra = [c.extra.clone(), format!("Aliases: {}", others.join(", "))].into_iter().filter(|e| !e.is_empty()).collect::<Vec<_>>().join(" | ");
        Some(Character { extra, ..c.clone() }.line())
    }).collect()
}

/// The notes the model gets. Names the game gave us (`names`: original, English) that the file
/// doesn't list yet are added to it first, so genders and aliases can be added to them by hand.
fn character_notes(game: &Path, names: &[(String, String)]) -> R<Vec<String>> {
    let mut list = load_characters(game);
    let new: Vec<Character> = names
        .iter()
        .filter(|(jp, _)| !list.iter().any(|c| &c.original == jp))
        .map(|(jp, en)| Character { original: jp.clone(), english: en.clone(), ..Default::default() })
        .collect();
    if !new.is_empty() || !characters_path(game).exists() && !names.is_empty() {
        list.extend(new);
        save_characters(game, &list)?;
    }
    let text = characters_text(game)?.unwrap_or_default();
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect())
}

// ---------- genders ----------

/// Lines a character needs before their gender is worth asking about.
const MIN_SPOKEN: usize = 5;
/// Moments of a character's lines the model reads, spread over the whole story.
const GENDER_MOMENTS: usize = 12;
/// Lines it also reads on each side of one, for how others talk to and about them.
const AROUND: usize = 2;
const NARRATION: &str = "narration";
const GENDER_SYSTEM: &str = "You read lines from a Japanese adult visual novel and work out one character's gender. Judge by how \
they speak (first-person words like 僕, 俺, 私, あたし, and sentence endings) and by how others address and describe them (くん, ちゃん, \
彼, 彼女, 兄, 姉). Sexual or violent lines are fine; you only answer the question. Reply with exactly one word: Female, Male, or \
Unknown if the lines don't make it clear.";

const SPRITE_PROMPT: &str = "This image is a character sprite from a Japanese visual novel. Judging only from how the character is drawn \
(body, clothes, hair, face), is this character male or female? Reply with exactly one word: Male, Female, or Unknown.";

static GENDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(female|male|unknown)\b").unwrap());

fn gender_in(reply: &str) -> Option<&'static str> {
    match GENDER.find(reply)?.as_str().to_lowercase().as_str() {
        "female" => Some("Female"),
        "male" => Some("Male"),
        _ => None,
    }
}

/// What a game's own files say about its characters, beyond their lines. Both are by original name:
/// the picture of each, and `aliases`, {other name: main name}.
#[derive(Default)]
struct Looks {
    sprites: HashMap<String, Vec<u8>>,
    aliases: HashMap<String, String>,
}

/// VNTL only translates, so the image model, which follows instructions, reads a sample of each
/// listed character's lines and fills in blank genders. `story` is `(speaker, text)` in game order.
/// Where `looks` has the character's picture it's asked about that too, and the picture wins: in
/// testing it was wrong less often, and it says Unknown when a figure could be either. Other names
/// of one character are answered once, with all of their lines, and take that answer, so a disguise
/// follows the sprite and not its label. A gender already set stays, and an unclear answer stays
/// blank to be asked again.
fn guess_genders(game: &Path, model: &dyn Chat, story: &[(String, String)], looks: &Looks, log: &dyn Fn(String), cancel: &AtomicBool) -> R<()> {
    let mut list = load_characters(game);
    let mut found = 0;
    let english: HashMap<String, String> = list.iter().map(|c| (c.original.clone(), c.english.clone())).collect();
    let main_of = |name: &String| looks.aliases.get(name).filter(|m| english.contains_key(*m));
    for c in list.iter_mut().filter(|c| c.gender.trim().is_empty() && main_of(&c.original).is_none()) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let others: Vec<&String> = looks.aliases.iter().filter(|(_, main)| **main == c.original).map(|(other, _)| other).collect();
        let who = |s: &String| !s.is_empty() && (*s == c.original || *s == c.english || others.iter().any(|o| *o == s || english.get(*o) == Some(s)));
        let spoken: Vec<usize> = story.iter().enumerate().filter(|(_, l)| who(&l.0)).map(|(i, _)| i).collect();
        let sprite = looks.sprites.get(&c.original).or_else(|| others.iter().find_map(|o| looks.sprites.get(*o)));
        if spoken.len() < MIN_SPOKEN && sprite.is_none() {
            continue;
        }
        let moments = spoken.len().min(GENDER_MOMENTS);
        let read: BTreeSet<usize> = (0..moments)
            .map(|m| spoken[m * spoken.len() / moments])
            .flat_map(|i| i.saturating_sub(AROUND)..=(i + AROUND).min(story.len() - 1))
            .collect();
        let lines: Vec<String> = read.iter().map(|&i| format!("{}: {}", story[i].0, story[i].1)).collect();
        let name = if c.original.is_empty() { c.english.clone() } else { c.original.clone() };
        let drawn = sprite.map(|png| model.read_image(png, SPRITE_PROMPT)).transpose();
        let told = (spoken.len() >= MIN_SPOKEN).then(|| model.chat(GENDER_SYSTEM, &format!("Character: {name}\n\nLines:\n{}", lines.join("\n")))).transpose();
        let (drawn, told) = match (drawn, told) {
            (Ok(d), Ok(t)) => (d.as_deref().and_then(gender_in), t.as_deref().and_then(gender_in)),
            (Err(e), _) | (_, Err(e)) => {
                log(format!("Couldn't work out genders: {e}"));
                break;
            }
        };
        if let (Some(d), Some(t)) = (drawn, told)
            && d != t
        {
            log(format!("{name}: the sprite says {d} and the lines say {t}, so going with the sprite. Worth a check in the Characters list."));
        }
        let Some(gender) = drawn.or(told) else { continue };
        c.gender = gender.into();
        log(format!("{name}: {} (worked out from the story; change it in the Characters list if it's wrong)", c.gender));
        found += 1;
    }
    let genders: HashMap<String, String> = list.iter().map(|c| (c.original.clone(), c.gender.clone())).collect();
    for c in list.iter_mut().filter(|c| c.gender.trim().is_empty()) {
        if let Some(main) = main_of(&c.original).filter(|m| !genders[*m].trim().is_empty()) {
            c.gender = genders[main].clone();
            log(format!("{}: {}, as the game treats this as another name of {}", c.original, c.gender, main));
            found += 1;
        }
    }
    if found > 0 {
        save_characters(game, &list)?;
    }
    Ok(())
}

/// The notes for a run, with blank genders worked out first if there's a model for it. `story` is
/// empty when no story is left to translate, since then the answer would go unused.
fn notes_with_genders(o: &Options, names: &[(String, String)], story: &[(String, String)], looks: &Looks, log: &dyn Fn(String), cancel: &AtomicBool) -> R<Vec<String>> {
    character_notes(&o.game, names)?;
    if let Some(model) = &o.vision {
        guess_genders(&o.game, model.as_ref(), story, looks, log, cancel)?;
    }
    Ok(with_aliases(character_notes(&o.game, names)?, &looks.aliases))
}

/// The saved translations. Starting over sets them aside (cache_<lang>.before-N.json) instead of
/// deleting them, and begins empty.
fn load_cache(path: &Path, fresh: bool, log: &dyn Fn(String)) -> R<Cache> {
    if fresh && path.exists() {
        let aside = (1..).map(|n| path.with_extension(format!("before-{n}.json"))).find(|p| !p.exists()).unwrap();
        std::fs::rename(path, &aside).map_err(|e| format!("{}: {e}", path.display()))?;
        log(format!("Starting over; the previous translations are kept in {}", aside.display()));
    }
    let mut cache: Cache = if path.exists() { load_json(path)? } else { Cache::default() };
    // saved before the checks caught them; official lines were written by people, whatever they say
    let before = cache.lines.len() + cache.strings.len();
    // regex first: official is a Vec, scanning it for every line is quadratic
    cache.lines.retain(|k, t| !META_REPLY.is_match(t) || cache.official.contains(k));
    cache.strings.retain(|_, t| !META_REPLY.is_match(t));
    let dropped = before - cache.lines.len() - cache.strings.len();
    if dropped > 0 {
        log(format!("{dropped} saved replies were the model refusing or talking, not translations; they will be translated again"));
    }
    Ok(cache)
}

/// Forget cached replies where the model kept going instead of stopping, or that kept furigana (BGI's
/// `<r…>`) the line no longer has, so they're translated again.
fn drop_runaways<'a>(cache: &mut Cache, sources: impl Iterator<Item = (&'a String, &'a String)>, log: &dyn Fn(String)) {
    let stale = |src: &str, tr: &str| runaway(src, tr) || (tr.contains("<r") && !src.contains("<r"));
    let bad: Vec<String> = sources
        .filter(|(k, src)| cache.lines.get(*k).is_some_and(|tr| stale(src, tr)))
        .map(|(k, _)| k.clone())
        .collect();
    if !bad.is_empty() {
        log(format!("{} saved translations will be translated again: far longer than their Japanese, or holding furigana", bad.len()));
        bad.iter().for_each(|k| _ = cache.lines.shift_remove(k));
    }
}

/// The end-of-run checks: what a reader would notice first.
fn health(cache: &Cache, log: &dyn Fn(String)) {
    let count = |bad: &dyn Fn(&String) -> bool| cache.lines.values().filter(|t| bad(t)).count();
    let (ja, empty) = (count(&|t| JA.is_match(t)), count(&|t| t.trim().is_empty()));
    log(format!("Checks: {} lines; {ja} still contain Japanese; {empty} are empty", cache.lines.len()));
}

/// The system prompt with the character notes, for models that take instructions.
fn with_notes(system: &str, notes: &[String]) -> String {
    match notes.is_empty() {
        true => system.to_string(),
        false => format!("{system}\n\nCharacters:\n{}", notes.join("\n")),
    }
}

/// Earlier lines a VNTL model sees by default: enough of a conversation to follow who is talking and how.
pub const VNTL_HISTORY: usize = 12;

/// (key, context shown to the model, japanese) in batches into cache.lines, saved after each batch.
/// VNTL models get one line at a time, with the names and the last lines of the same context.
/// `speakers` maps the English speaker in `todo` to the name as the game writes it, where lines have
/// speakers; without it the context is a table or script, and a new one starts a new history.
#[allow(clippy::too_many_arguments)]
fn translate_lines(o: &Options, chat: &dyn Chat, system: &str, markup: &Regex, notes: &[String], speakers: Option<&HashMap<String, String>>,
                   todo: &[(String, String, String)],
                   cache: &mut Cache, save: &dyn Fn(&Cache) -> R<()>, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<()> {
    let (total, size) = (todo.len(), o.batch.max(1));
    if is_vntl(&o.model) && !todo.is_empty() && notes_fill_prompt(notes) {
        report(Event::Log("The synopsis and character notes fill the model's whole prompt, so it can't see earlier lines and part of the notes get lost. Shorten them on the Game Data tab.".into()));
    }
    let mut history: Vec<(String, String)> = Vec::new();
    let mut context = "";
    let mut failing = 0; // lines in a row the server answered with a 500
    for (done, chunk) in todo.chunks(size).enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Stopped".into());
        }
        if is_vntl(&o.model) {
            for (k, sp, t) in chunk {
                if cancel.load(Ordering::Relaxed) {
                    save(cache)?;
                    return Err("Stopped".into());
                }
                if speakers.is_none() && sp != context {
                    (context, history) = (sp, Vec::new());
                }
                let speaker = speakers.filter(|_| sp != "narration").map(|names| names.get(sp).map_or(sp.as_str(), String::as_str));
                trim_memory(&mut history, notes, o.history.load(Ordering::Relaxed));
                let reply = match translate_vntl(chat, markup, notes, &history, speaker, t) {
                    Ok(reply) => {
                        failing = 0;
                        reply
                    }
                    // a prompt the server can't take costs one line; the tenth in a row means the server is down
                    Err(e) if e.contains("status: 500") && failing < 9 => {
                        failing += 1;
                        None
                    }
                    Err(e) => {
                        _ = save(cache); // the lines this chunk already finished
                        return Err(match e.contains("status: 500") {
                            true => format!("The model server keeps failing ({e}): it may have crashed or run out of memory. Restart Ollama, then Translate again to resume."),
                            false => e,
                        });
                    }
                };
                if let Some(tr) = reply {
                    cache.lines.insert(k.clone(), tr.clone());
                    report(Event::Line(t.clone(), tr.clone()));
                    history.push(match speaker {
                        Some(name) => (format!("[{name}]: {t}"), format!("[{sp}]: {tr}")),
                        None => (t.clone(), tr),
                    });
                }
            }
        } else {
            let batch: Vec<(String, String)> = chunk.iter().map(|(_, sp, t)| (sp.clone(), t.clone())).collect();
            for ((k, _, t), tr) in chunk.iter().zip(translate_batch(chat, system, markup, &batch)?) {
                if let Some(tr) = tr {
                    report(Event::Line(t.clone(), tr.clone()));
                    cache.lines.insert(k.clone(), tr);
                }
            }
        }
        save(cache)?;
        let n = (done * size + chunk.len()).min(total);
        report(Event::Progress(n, total));
        report(Event::Log(format!("  {n}/{total}")));
    }
    Ok(())
}

/// Unity Localization locale code for a target language name.
fn locale_code(lang: &str) -> String {
    let l = lang.to_lowercase();
    let code = match l.as_str() {
        "english" => "en",
        "chinese" => "zh",
        "korean" => "ko",
        "spanish" => "es",
        "french" => "fr",
        "german" => "de",
        "russian" => "ru",
        "portuguese" => "pt",
        "italian" => "it",
        _ => &l,
    };
    code.to_string()
}

/// Which tables get the translation: the Japanese ones, since many games (and trial builds) have
/// no language menu and always show Japanese, and the target language's own tables if there are any.
fn unity_destination(t: &unity::Table, code: &str) -> bool {
    t.lang() == "ja" || t.lang() == code
}

/// Write cached Unity translations into the game. Returns a summary line.
const NO_READER: &str = "Text over artwork needs the text reader, from Model Download.";

fn write_unity(game: &Path, lang: &str, cache: &Cache) -> R<String> {
    let Some(aa) = unity::addressables(game) else { return write_yarn(game, cache) };
    let tables = unity::read_tables(&aa)?;
    let code = locale_code(lang);
    let n = unity::install(&aa, &|t, id| {
        if t.images {
            // Japanese mode shows the game's own images for the target language (e.g. translated signs)
            let own = tables.iter().find(|d| d.images && d.shared == t.shared && d.lang() == code);
            return (t.lang() == "ja").then(|| own?.entries.get(&id).cloned()).flatten();
        }
        unity_destination(t, &code).then(|| cache.lines.get(&format!("{}#{id}", t.collection)).map(|s| ascii_punctuation(s))).flatten()
    })?;
    let target = tables.iter().any(|t| t.lang() == code);
    Ok(format!(
        "Wrote {n} translated entries (every changed bundle read back OK; originals kept as *.vntl-orig). \
         The game's Japanese setting{} now shows the translation.",
        if target { format!(" and its {lang} setting") } else { String::new() }
    ))
}

/// Yarn Spinner's text goes back into the game's own files.
fn write_yarn(game: &Path, cache: &Cache) -> R<String> {
    let listed = listed_names(game);
    let n = yarn::install(game, &|k| cache.lines.get(k).cloned(), &|name| english_name(&listed, cache, name))?;
    Ok(format!("Wrote {n} translated strings into the game's .assets files (read back OK; the originals are kept as *.vntl-orig)."))
}

/// A name corrected in the Characters list goes into the game too, over the game's own English.
/// `sources`: (cache key, japanese) of every entry; name tables have "name" in their key.
fn correct_unity_names(cache: &mut Cache, sources: &[(String, String)], listed: &HashMap<String, String>) {
    for (k, jp) in sources.iter().filter(|(k, _)| k.to_lowercase().contains("name")) {
        if let Some(en) = listed.get(jp) {
            cache.lines.insert(k.clone(), en.clone());
        }
    }
}

/// Unity games using Unity's Localization package: the game's own target-language text counts
/// as the official translation; the model translates entries that are missing or still Japanese.
/// A game without that package's folder goes through Yarn Spinner's table instead.
fn translate_unity(o: &Options, chat: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<()> {
    let log = |s: String| report(Event::Log(s));
    let Some(aa) = unity::addressables(&o.game) else {
        let lines = yarn::Game::open(&o.game)?.lines();
        translate_script(o, chat, report, cancel, "Unity with Yarn Spinner", &lines, &UNITY_MARKUP, &|cache| write_yarn(&o.game, cache))?;
        if let Some(vision) = o.vision.as_ref().filter(|_| o.art) {
            if !crate::ocr::installed() {
                log(NO_READER.into());
                return Ok(());
            }
            log(crate::menus::translate_unity_art(&o.game, vision.as_ref(), report, cancel)?);
            log(write_yarn(&o.game, &load_json(&cache_file(&o.game, &o.lang))?)?);
        }
        return Ok(());
    };
    if o.art {
        log("Text over artwork only works on Unity games with Yarn Spinner so far.".into());
    }
    let tables = unity::read_tables(&aa)?;
    let code = locale_code(&o.lang);
    let mut src: Vec<&unity::Table> = tables.iter().filter(|t| t.lang() == "ja").collect();
    if src.is_empty() {
        return Err("No Japanese Unity Localization string tables found in this game".into());
    }
    src.sort_by_key(|t| !t.collection.to_lowercase().contains("name")); // names first: they become the glossary
    if o.reference.is_some() || o.label.is_some() || o.file.is_some() {
        log("Unity: the official-translation folder and scene filter are not used; the game's own tables are.".into());
    }
    let work = o.game.join("vntl_work");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let cache_path = cache_file(&o.game, &o.lang);
    let mut cache = load_cache(&cache_path, o.fresh, &log)?;
    let save = |c: &Cache| save_json(&cache_path, c);

    if o.retranslate {
        let official: HashSet<String> = std::mem::take(&mut cache.official).into_iter().collect();
        cache.lines.retain(|k, _| !official.contains(k));
    }
    let sources: Vec<(String, String)> =
        src.iter().flat_map(|t| t.entries.iter().map(|(id, jp)| (format!("{}#{id}", t.collection), jp.clone()))).collect();
    drop_runaways(&mut cache, sources.iter().map(|(k, jp)| (k, jp)), &log);
    let (mut names, mut all) = (Vec::new(), Vec::new());
    for t in &src {
        let is_names = t.collection.to_lowercase().contains("name");
        let own = tables.iter().find(|d| d.shared == t.shared && d.lang() == code).filter(|_| is_names || !o.retranslate);
        for (id, jp) in t.entries.iter().filter(|(_, jp)| JA.is_match(jp)) {
            let k = format!("{}#{id}", t.collection);
            if !is_names {
                all.push((k.clone(), format!("{} text", t.collection), jp.clone()));
            }
            if cache.lines.contains_key(&k) {
                continue;
            }
            match own.and_then(|d| d.entries.get(id)).filter(|en| !en.trim().is_empty() && !JA.is_match(en)) {
                Some(en) => {
                    cache.lines.insert(k.clone(), en.clone());
                    cache.official.push(k);
                }
                None if is_names => names.push((k, "character name".into(), jp.clone())),
                None => {}
            }
        }
    }
    fill_repeats(&all, &mut cache);
    let todo = still_to_ask(&all, &cache);
    save(&cache)?;
    log(format!(
        "Unity Localization: {} Japanese tables; {} entries use the game's own {} text; {} entries to translate with {}",
        src.len(), cache.official.len(), o.lang, names.len() + todo.len(), o.model
    ));
    let system = SYSTEM.replace("Copy Ren'Py markup", "Copy markup").replace("or [square brackets]", "or <angle brackets>");
    translate_lines(o, chat, &system, &UNITY_MARKUP, &[], None, &names, &mut cache, &save, &|_| {}, cancel)?;
    correct_unity_names(&mut cache, &sources, &listed_names(&o.game));
    let glossary: Vec<(String, String)> = src
        .iter()
        .filter(|t| t.collection.to_lowercase().contains("name"))
        .flat_map(|t| t.entries.iter().filter_map(|(id, jp)| Some((jp.clone(), cache.lines.get(&format!("{}#{id}", t.collection))?.clone()))))
        .collect();
    let notes = character_notes(&o.game, &glossary)?;
    let system = with_notes(&system, &notes);
    log(format!("Character notes for the model: {}", work.join("characters.txt").display()));
    let run = translate_lines(o, chat, &system, &UNITY_MARKUP, &notes, None, &todo, &mut cache, &save, report, cancel);
    fill_repeats(&all, &mut cache);
    save(&cache)?;
    run?;
    if cancel.load(Ordering::Relaxed) {
        return Err("Stopped".into());
    }
    let missing = names.iter().chain(&all).filter(|(k, _, _)| !cache.lines.contains_key(k)).count();
    log(write_unity(&o.game, &o.lang, &cache)?);
    health(&cache, &log);
    if missing > 0 {
        let kept = match tables.iter().any(|t| t.lang() == code) {
            true => format!("the game's own {} text", o.lang),
            false => "Japanese".to_string(),
        };
        log(format!("{missing} entries keep {kept}: the model's replies failed the checks."));
    }
    Ok(())
}

/// Write cached Liar-soft translations into scr.xfl. Returns a summary line.
fn write_liar(game: &Path, cache: &Cache) -> R<String> {
    let n = liar::install(game, &|script, i, shipped| {
        let (prefix, body) = liar::split(shipped);
        match cache.lines.get(&format!("{script}#{i}")) {
            // more rows than the box shows stops the game: that message keeps its Japanese
            Some(tr) if !liar::fits(tr) => None,
            Some(tr) => Some(liar::encode(prefix, tr)),
            // only symbols or full-width letters: nothing to translate, but they have to be ASCII too
            None if !JA.is_match(shipped) && !shipped.is_ascii() => Some(liar::encode(prefix, &body)),
            None => None,
        }
    })?;
    let exe = match liar::patch_exe(game)? {
        true => "patched the engine for English letter widths",
        false => "the engine is a build this app doesn't know, so English may look unevenly spaced",
    };
    let lossy = match cache.lines.values().filter(|t| liar::lossy(t)).count() {
        0 => String::new(),
        lost => format!(" {lost} lines have characters the game can't show, written as '?'."),
    };
    let plates = match write_plates(game, cache)? {
        0 => String::new(),
        drawn => format!(" {drawn} name plates drawn in English."),
    };
    let long = match cache.lines.values().filter(|t| !liar::fits(t)).count() {
        0 => String::new(),
        long => format!(" {long} messages are too long for the text box and stay in Japanese until they're shortened."),
    };
    Ok(format!("Wrote {n} translated strings into scr.xfl (read back OK; originals kept as *.vntl-orig); {exe}.{lossy}{plates}{long}"))
}

/// How English too long for where it's shown is made to fit: what fits, and at most how many
/// characters to ask for, from the English and a share in tenths.
struct Fit<'a> {
    fits: &'a dyn Fn(&str) -> bool,
    most: &'a dyn Fn(&str, usize) -> usize,
    markup: &'a Regex,
}

/// Lines the English made longer than where they're shown holds. A model that takes instructions
/// shortens them, two tries each per run.
fn shorten(model: &dyn Chat, lines: &[Line], cache: &mut Cache, fit: &Fit, log: &dyn Fn(String), cancel: &AtomicBool) -> R<()> {
    let long: Vec<(&String, &String)> = lines.iter().filter(|(k, _, _)| cache.lines.get(k).is_some_and(|en| !(fit.fits)(en))).map(|(k, _, jp)| (k, jp)).collect();
    if long.is_empty() {
        return Ok(());
    }
    log(format!("Shortening {} lines too long for where the game shows them", long.len()));
    let mut fixed = 0;
    for (key, jp) in long {
        let en = cache.lines[key].clone();
        for share in [9, 7] {
            if cancel.load(Ordering::Relaxed) {
                return Err("Stopped".into());
            }
            let most = (fit.most)(&en, share);
            let reply = model.chat(SYSTEM, &format!(
                "This line from a Japanese visual novel is too long for its text box: \"{en}\" (Japanese: {jp}). Rewrite it in \
                 natural English in at most {most} characters, keeping what it says, its tone and any names. Reply with the line only."))?;
            let short = reply.trim().trim_matches('"').to_string();
            if same_markup(fit.markup, jp, &short) && (fit.fits)(&short) {
                cache.lines.insert(key.clone(), short);
                fixed += 1;
                break;
            }
        }
    }
    log(format!("  {fixed} shortened"));
    Ok(())
}

/// Shortens what doesn't fit among `lines`, if the models can, and writes again.
fn shorten_and_write(o: &Options, chat: &dyn Chat, lines: &[Line], fit: &Fit, write: &dyn Fn(&Cache) -> R<String>, log: &dyn Fn(String), cancel: &AtomicBool) -> R<()> {
    let path = cache_file(&o.game, &o.lang);
    let mut cache: Cache = load_json(&path)?;
    if !lines.iter().any(|l| cache.lines.get(&l.0).is_some_and(|en| !(fit.fits)(en))) {
        return Ok(());
    }
    let words = instruction_model(o, chat);
    if is_vntl(words.model()) {
        log("Long lines can be shortened by a model that takes instructions: pick an image model, then Translate again.".into());
        return Ok(());
    }
    let run = shorten(words, lines, &mut cache, fit, log, cancel);
    save_json(&path, &cache)?;
    run?;
    log(write(&cache)?);
    Ok(())
}

fn plates_path(game: &Path) -> PathBuf {
    game.join("vntl_work").join("plates.json")
}

/// Draw the name plates a run read, in English, as the Characters list or the model spell them.
fn write_plates(game: &Path, cache: &Cache) -> R<usize> {
    let read: HashMap<String, String> = load_json(&plates_path(game)).unwrap_or_default();
    let listed = listed_names(game);
    let english = |jp: &String| english_name(&listed, cache, jp).map(|n| n.trim_matches(['【', '】']).to_string()).filter(|n| !n.is_empty() && !JA.is_match(n));
    paint_plates(game, &read.iter().filter_map(|(code, jp)| Some((code.clone(), (jp.clone(), english(jp)?)))).collect())
}

/// What each name plate says, by speaker code. A plate that says nothing Japanese names nobody: the
/// narrator's is one.
fn plate_names(o: &Options, log: &dyn Fn(String)) -> R<HashMap<String, String>> {
    let plates = liar::plates(&o.game)?;
    let reader = (!plates.is_empty()).then(Reader::open).and_then(Result::ok);
    let vision = o.vision.as_deref().map(|v| v as &dyn Chat);
    if !plates.is_empty() && reader.is_none() && vision.is_none() {
        log("Liar-soft: speakers stay unknown to the model. Get the text reader from Model Download, or pick an image model.".into());
        return Ok(HashMap::new());
    }
    let mut names = HashMap::new();
    for (code, img) in &plates {
        names.extend(read_plate(reader.as_ref(), vision, img)?.map(|name| (code.clone(), name)));
    }
    std::fs::create_dir_all(o.game.join("vntl_work")).map_err(|e| e.to_string())?;
    save_json(&plates_path(&o.game), &names)?; // `write` needs them, and may run without a reader
    Ok(names)
}

/// The Japanese strings of a Liar-soft game as lines, codes kept out of the model's way.
fn liar_lines(texts: Vec<(String, usize, String)>, names: &HashMap<String, String>) -> Vec<Line> {
    let lines = texts.into_iter().filter(|(_, _, s)| JA.is_match(s));
    lines.map(|(script, i, s)| (format!("{script}#{i}"), liar::speaker_code(&s).and_then(|c| names.get(c)).cloned(), liar::split(&s).1)).collect()
}

fn translate_liar(o: &Options, chat: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<()> {
    let log = |s: String| report(Event::Log(s));
    let lines = liar_lines(liar::texts(&o.game)?, &plate_names(o, &log)?);
    translate_script(o, chat, report, cancel, "Liar-soft", &lines, &liar::MARKUP, &|cache| write_liar(&o.game, cache))?;
    // what still runs over stays Japanese, as the engine stops on it
    let most = |en: &str, share: usize| en.chars().count() * liar::ROWS * share / (liar::rows(en) * 10);
    shorten_and_write(o, chat, &lines, &Fit { fits: &liar::fits, most: &most, markup: &liar::MARKUP }, &|cache| write_liar(&o.game, cache), &log, cancel)?;
    if let Some(vision) = o.vision.as_ref().filter(|_| o.images) {
        log(crate::menus::translate_menus(&o.game, vision.as_ref(), report, cancel)?);
        // menus are rebuilt from the game's own archive, which has the Japanese plates
        write_plates(&o.game, &load_json(&cache_file(&o.game, &o.lang))?)?;
    }
    Ok(())
}

/// A line of a compiled script: (key in the cache, speaker as the game writes it, text).
pub type Line = (String, Option<String>, String);

// nothing in these scripts' text is markup: their codes are taken out before the model sees it
static NO_MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new("$^").unwrap());

fn name_key(name: &str) -> String {
    format!("name:{name}")
}

/// A speaker in English: as the Characters list spells it, or as the model gave it. VNTL was trained
/// on `[Name]` tags, so it can answer a bare name in one, and the rest of the list then follows.
fn english_name(listed: &HashMap<String, String>, cache: &Cache, name: &str) -> Option<String> {
    listed.get(name).cloned().or_else(|| Some(cache.lines.get(&name_key(name))?.trim_matches(['[', ']']).to_string()))
}

/// A line this long means the same wherever it comes. A short one like はい changes with the scene.
const REPEAT_LETTERS: usize = 12;

/// A long line the same speaker says again takes the translation it already has, so it reads the
/// same both times and the model isn't asked twice. Returns how many were filled in.
fn fill_repeats(all: &[(String, String, String)], cache: &mut Cache) -> usize {
    let long = || all.iter().filter(|l| l.2.chars().count() >= REPEAT_LETTERS);
    let known: HashMap<(&str, &str), String> = long().rev().filter_map(|(k, sp, t)| Some(((sp.as_str(), t.as_str()), cache.lines.get(k)?.clone()))).collect();
    let mut filled = 0;
    for (k, sp, t) in long() {
        if let (false, Some(tr)) = (cache.lines.contains_key(k), known.get(&(sp.as_str(), t.as_str()))) {
            cache.lines.insert(k.clone(), tr.clone());
            filled += 1;
        }
    }
    filled
}

/// What the model still has to be asked: what the cache lacks, and a long line only once.
fn still_to_ask(all: &[(String, String, String)], cache: &Cache) -> Vec<(String, String, String)> {
    let mut asked = HashSet::new();
    all.iter().filter(|(k, sp, t)| !cache.lines.contains_key(k) && (t.chars().count() < REPEAT_LETTERS || asked.insert((sp, t)))).cloned().collect()
}

/// Engines whose compiled script the app reads itself: names first, as those go in the notes the
/// lines are translated with, then every line in story order. `write` installs what's in the cache.
#[allow(clippy::too_many_arguments)]
fn translate_script(o: &Options, chat: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool, engine: &str, lines: &[Line], markup: &Regex, write: &dyn Fn(&Cache) -> R<String>) -> R<()> {
    let log = |s: String| report(Event::Log(s));
    if o.reference.is_some() || o.label.is_some() || o.file.is_some() {
        log(format!("{engine}: the official-translation folder and scene filter are not used."));
    }
    let work = o.game.join("vntl_work");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let cache_path = cache_file(&o.game, &o.lang);
    let mut cache = load_cache(&cache_path, o.fresh, &log)?;
    let save = |c: &Cache| save_json(&cache_path, c);

    let listed = listed_names(&o.game);
    let names: BTreeSet<&String> = lines.iter().filter_map(|l| l.1.as_ref()).filter(|n| looks_like_name(n)).collect();
    let todo: Vec<(String, String, String)> = names
        .iter()
        .filter(|n| JA.is_match(n) && english_name(&listed, &cache, n).is_none())
        .map(|n| (name_key(n), "character name".to_string(), (*n).clone()))
        .collect();
    let notes = character_notes(&o.game, &[])?;
    translate_lines(o, chat, &with_notes(SYSTEM, &notes), markup, &notes, None, &todo, &mut cache, &save, report, cancel)?;
    let pairs: Vec<(String, String)> = names.iter().filter_map(|n| Some(((*n).clone(), english_name(&listed, &cache, n)?))).collect();
    let who: HashMap<String, String> = pairs.iter().map(|(jp, en)| (en.clone(), jp.clone())).collect();
    let left = lines.iter().any(|l| !cache.lines.contains_key(&l.0));
    let story: Vec<(String, String)> = lines.iter().filter(|_| left).map(|l| (l.1.clone().unwrap_or_else(|| NARRATION.into()), l.2.clone())).collect();
    let notes = notes_with_genders(o, &pairs, &story, &Looks::default(), &log, cancel)?;
    let system = with_notes(SYSTEM, &notes);

    let all: Vec<(String, String, String)> = lines
        .iter()
        .map(|(k, n, t)| {
            let speaker = n.as_ref().map_or("narration".to_string(), |n| english_name(&listed, &cache, n).unwrap_or_else(|| n.clone()));
            (k.clone(), speaker, t.clone())
        })
        .collect();
    drop_runaways(&mut cache, all.iter().map(|(k, _, src)| (k, src)), &log);
    fill_repeats(&all, &mut cache);
    let todo = still_to_ask(&all, &cache);
    log(format!("{engine} script: {} lines; {} to translate with {}", lines.len(), todo.len(), o.model));
    let run = translate_lines(o, chat, &system, markup, &notes, Some(&who), &todo, &mut cache, &save, report, cancel);
    // before the stop is reported: a partial `write` should have these too
    let filled = fill_repeats(&all, &mut cache);
    save(&cache)?;
    run?;
    if cancel.load(Ordering::Relaxed) {
        return Err("Stopped".into());
    }
    if filled > 0 {
        log(format!("{filled} repeated lines reused a translation"));
    }
    let missing = all.iter().filter(|(k, _, _)| !cache.lines.contains_key(k)).count();
    log(write(&cache)?);
    if missing > 0 {
        log(format!("{missing} lines kept in Japanese: the model's replies failed the checks."));
    }
    health(&cache, &log);
    Ok(())
}

/// Write cached Malie translations into data.dat. Returns a summary line.
fn write_malie(game: &Path, cache: &Cache) -> R<String> {
    let listed = listed_names(game);
    let n = malie::install(game, &|k| cache.lines.get(k).cloned(), &|name| english_name(&listed, cache, name))?;
    Ok(format!("Wrote {n} translated texts into data.dat (read back OK; the original is kept as data.dat.vntl-orig)."))
}

fn translate_malie(o: &Options, chat: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<()> {
    let lines = malie::Game::open(&o.game)?.lines()?;
    translate_script(o, chat, report, cancel, "Malie", &lines, &NO_MARKUP, &|cache| write_malie(&o.game, cache))
}

/// Write cached Artemis translations into the archives. Returns a summary line.
fn write_artemis(game: &Path, cache: &Cache) -> R<String> {
    let listed = listed_names(game);
    let n = artemis::install(game, &|k| cache.lines.get(k).cloned(), &|name| english_name(&listed, cache, name))?;
    Ok(format!("Wrote {n} translated texts into the game's archives (read back OK; the originals are kept as *.vntl-orig)."))
}

fn translate_artemis(o: &Options, chat: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<()> {
    let lines = artemis::Game::open(&o.game)?.lines();
    translate_script(o, chat, report, cancel, "Artemis", &lines, &kirikiri::MARKUP, &|cache| write_artemis(&o.game, cache))?;
    // a choice that wraps pushes the ones after it out of their boxes
    let (keys, width) = artemis::Game::open(&o.game)?.one_row();
    let row: Vec<Line> = lines.iter().filter(|l| keys.contains(&l.0)).cloned().collect();
    // measured as the write measures it, with … as three dots
    let fits = |en: &str| crate::text::wrap_words(&crate::text::ascii_punctuation(en), width).len() <= 1;
    let most = |_: &str, share: usize| width * share / 10;
    shorten_and_write(o, chat, &row, &Fit { fits: &fits, most: &most, markup: &kirikiri::MARKUP }, &|cache| write_artemis(&o.game, cache), &|s| report(Event::Log(s)), cancel)?;
    if let Some(vision) = o.vision.as_ref().filter(|_| o.images) {
        let cache: Cache = load_json(&cache_file(&o.game, &o.lang))?;
        let listed = listed_names(&o.game);
        let english = |jp: &str| english_name(&listed, &cache, jp).filter(|n| !n.is_empty() && !JA.is_match(n));
        report(Event::Log(crate::menus::translate_artemis_menus(&o.game, vision.as_ref(), &english, report, cancel)?));
        report(Event::Log(write_artemis(&o.game, &cache)?));
    }
    Ok(())
}

/// Write cached BGI translations into the archives, with any repainted menu pictures. Returns a summary line.
fn write_bgi(game: &Path, cache: &Cache) -> R<String> {
    let listed = listed_names(game);
    let n = bgi::install(game, &|k| cache.lines.get(k).cloned(), &|name| english_name(&listed, cache, name))?;
    Ok(format!("Wrote {n} translated strings into the game's archives (read back OK; the originals are kept as *.vntl-orig)."))
}

fn translate_bgi(o: &Options, chat: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<()> {
    let lines = bgi::Game::open(&o.game)?.lines();
    translate_script(o, chat, report, cancel, "BGI", &lines, &NO_MARKUP, &|cache| write_bgi(&o.game, cache))?;
    if let Some(vision) = o.vision.as_ref().filter(|_| o.images) {
        let cache: Cache = load_json(&cache_file(&o.game, &o.lang))?;
        let listed = listed_names(&o.game);
        let english = |jp: &str| english_name(&listed, &cache, jp);
        report(Event::Log(crate::menus::translate_bgi_menus(&o.game, vision.as_ref(), &english, report, cancel)?));
        report(Event::Log(write_bgi(&o.game, &cache)?));
    }
    Ok(())
}

type Run = fn(&Options, &dyn Chat, &dyn Fn(Event), &AtomicBool) -> R<()>;

/// An engine whose compiled script the app reads itself, by the name `detect` gives it.
pub struct ScriptEngine {
    id: &'static str,
    translate: Run,
    write: fn(&Path, &Cache) -> R<String>,
    /// how many Japanese lines the game has
    pub lines: fn(&Path) -> R<usize>,
}

/// A row of `SCRIPT_ENGINES`. `$done` ends the sentence that says what was written. With `named`, the
/// engine's `install` also takes the speakers' names, as the Characters list or the model spell them.
macro_rules! engine_row {
    ($id:literal, $module:ident, $title:literal, $markup:expr, $done:literal, named) => {
        engine_row!(@row $id, $module, $title, $markup, $done, |game, cache| {
            let listed = listed_names(game);
            $module::install(game, &|k| cache.lines.get(k).cloned(), &|name| english_name(&listed, cache, name))
        })
    };
    ($id:literal, $module:ident, $title:literal, $markup:expr, $done:literal) => {
        engine_row!(@row $id, $module, $title, $markup, $done, |game, cache| $module::install(game, &|k| cache.lines.get(k).cloned()))
    };
    (@row $id:literal, $module:ident, $title:literal, $markup:expr, $done:literal, $install:expr) => {{
        fn write(game: &Path, cache: &Cache) -> R<String> {
            let install: fn(&Path, &Cache) -> R<usize> = $install;
            Ok(format!("Wrote {} translated {}", install(game, cache)?, $done))
        }
        ScriptEngine {
            id: $id,
            translate: |o, chat, report, cancel| {
                let lines = $module::Game::open(&o.game)?.lines();
                translate_script(o, chat, report, cancel, $title, &lines, $markup, &|cache| write(&o.game, cache))
            },
            write,
            lines: |game| Ok($module::Game::open(game)?.lines().len()),
        }
    }};
}

static SCRIPT_ENGINES: [ScriptEngine; 16] = [
    ScriptEngine { id: "malie", translate: translate_malie, write: write_malie, lines: |game| Ok(malie::Game::open(game)?.lines()?.len()) },
    ScriptEngine { id: "artemis", translate: translate_artemis, write: write_artemis, lines: |game| Ok(artemis::Game::open(game)?.lines().len()) },
    engine_row!("wolf-rpg", wolf, "Wolf RPG", &wolf::MARKUP, "strings into Data.wolf (read back OK; the original is kept as Data.wolf.vntl-orig).", named),
    engine_row!("kirikiri", kirikiri, "KiriKiri", &kirikiri::MARKUP, "texts into the game's archives (read back OK; the originals are kept as *.vntl-orig)."),
    engine_row!("nscripter", nscripter, "NScripter", &nscripter::MARKUP, "strings into nscript.dat (read back OK; the original is kept as nscript.dat.vntl-orig).", named),
    engine_row!("siglusengine", siglus, "SiglusEngine", &NO_MARKUP, "strings into Scene.pck (read back OK; the original is kept as Scene.pck.vntl-orig).", named),
    engine_row!("qlie", qlie, "QLIE", &NO_MARKUP, "strings into the game's script pack (read back OK; the originals are kept as *.vntl-orig).", named),
    engine_row!("reallive", reallive, "RealLive", &NO_MARKUP, "strings into SEEN.TXT (read back OK; the original is kept as SEEN.TXT.vntl-orig).", named),
    engine_row!("catsystem2", catsystem, "CatSystem2", &NO_MARKUP, "strings into scene.int (read back OK; the original is kept as scene.int.vntl-orig).", named),
    engine_row!("majiro", majiro, "Majiro", &NO_MARKUP, "strings into scenario.arc (read back OK; the original is kept as scenario.arc.vntl-orig).", named),
    engine_row!("yu-ris", yuris, "YU-RIS", &NO_MARKUP, "strings into the script archive (read back OK; the original is kept as *.vntl-orig).", named),
    ScriptEngine { id: "bgi", translate: translate_bgi, write: write_bgi, lines: |game| Ok(bgi::Game::open(game)?.lines().len()) },
    engine_row!("advhd", advhd, "AdvHD", &advhd::MARKUP, "strings into Rio*.arc (read back OK; the original is kept as Rio*.arc.vntl-orig).", named),
    engine_row!("softpal", softpal, "Softpal", &softpal::MARKUP, "strings into data.pac (read back OK; the original is kept as data.pac.vntl-orig).", named),
    engine_row!("fvp", fvp, "FVP", &NO_MARKUP, "strings into the .hcb script (read back OK; the original is kept as *.hcb.vntl-orig).", named),
    engine_row!("tyranoscript", tyrano, "TyranoScript", &kirikiri::MARKUP, "strings into the game (read back OK; the originals are kept as *.vntl-orig).", named),
];

pub fn script_engine(id: &str) -> Option<&'static ScriptEngine> {
    SCRIPT_ENGINES.iter().find(|e| e.id == id)
}

/// Write the saved translations into the game, without the model. Returns a summary line.
pub fn write(game: &Path, engine: &str, lang: &str) -> R<String> {
    let cache: Cache = load_json(&cache_file(game, lang))?;
    match engine {
        "liarsoft" => write_liar(game, &cache),
        "unity" => write_unity(game, lang, &cache),
        "rpgmaker-mv/mz" | "rpgmaker-xp/vx/ace" | "rpgmaker-2000/2003" => write_rpgmaker(game, &cache),
        _ => match script_engine(engine) {
            Some(e) => (e.write)(game, &cache),
            None => Ok(format!("Wrote {} blocks (Ren'Py load check OK)", install(game, &load_json(&game.join("vntl_work").join("dump.json"))?, &cache, lang)?)),
        },
    }
}

/// Write cached RPG Maker translations into the data files. Returns a summary line.
fn write_rpgmaker(game: &Path, cache: &Cache) -> R<String> {
    let (texts, files) = rpgmaker::install(game, &|jp| cache.lines.get(jp).cloned())?;
    Ok(format!("Wrote {texts} translated texts into {files} data files (read back OK; originals kept as *.vntl-orig)."))
}

const NAMES_HINT: &str = "These are the names of a game's characters. Spell a name by how it sounds in Japanese, the way an official \
English release would. Translate words that are titles or jobs instead of names.";
const TERMS_HINT: &str = "These are the menu commands and terms of an RPG, not story text. Give only the label itself, in the words \
RPG menus usually use and as short as the Japanese, never an explanation of it. A lone counter word or a text starting with a particle \
goes right after a number or a name: give just the English fragment, like \"pcs\" or \"obtained!\".";

/// Names and menu words: a model that follows instructions gets these right more often than VNTL, if there is one.
#[allow(clippy::too_many_arguments)]
fn translate_labels(o: &Options, chat: &dyn Chat, system: &str, hint: &str, todo: &[(String, String, String)], cache: &mut Cache,
                    save: &dyn Fn(&Cache) -> R<()>, cancel: &AtomicBool) -> R<()> {
    let Some(model) = &o.vision else {
        return translate_lines(o, chat, system, &rpgmaker::MARKUP, &[], None, todo, cache, save, &|_| {}, cancel);
    };
    let system = format!("{system}\n\n{hint}");
    for chunk in todo.chunks(o.batch.max(1)) {
        if cancel.load(Ordering::Relaxed) {
            return Err("Stopped".into());
        }
        let batch: Vec<(String, String)> = chunk.iter().map(|(_, sp, t)| (sp.clone(), t.clone())).collect();
        for ((k, _, _), tr) in chunk.iter().zip(translate_batch(model.as_ref(), &system, &rpgmaker::MARKUP, &batch)?) {
            if let Some(tr) = tr {
                cache.lines.insert(k.clone(), tr);
            }
        }
        save(cache)?;
    }
    Ok(())
}

/// Descriptions the English made too long for the help window. One attempt per run: what is still
/// too long stays as it is, cut off rather than wrong.
#[allow(clippy::too_many_arguments)]
fn shorten_descriptions(o: &Options, model: &dyn Chat, system: &str, chosen: &[&rpgmaker::Text], cache: &mut Cache,
                        save: &dyn Fn(&Cache) -> R<()>, log: &dyn Fn(String), cancel: &AtomicBool) -> R<()> {
    let mut long = Vec::new();
    for t in chosen.iter().filter(|t| t.place == rpgmaker::DESCRIPTION) {
        if let Some(en) = cache.lines.get(&t.text).filter(|en| !rpgmaker::description_fits(&o.game, en).unwrap_or(true)) {
            long.push((t.text.clone(), en.clone()));
        }
    }
    if long.is_empty() {
        return Ok(());
    }
    log(format!("Shortening {} descriptions to fit the help window", long.len()));
    let mut fixed = 0;
    for (jp, en) in long {
        if cancel.load(Ordering::Relaxed) {
            return Err("Stopped".into());
        }
        let reply = model.chat(system, &format!(
            "This RPG item or skill description is too long for its window: \"{en}\" (Japanese: {jp}). Rewrite it in at most 130 \
             characters with the same effects and numbers. Reply with the description only."))?;
        let short = reply.trim().trim_matches('"').to_string();
        if same_markup(&rpgmaker::MARKUP, &jp, &short) && rpgmaker::description_fits(&o.game, &short)? {
            cache.lines.insert(jp, short);
            fixed += 1;
        }
    }
    save(cache)?;
    log(format!("  {fixed} shortened"));
    Ok(())
}

/// RPG Maker MV/MZ: names first, for the character notes, then the story, then the interface. With
/// a file or label ("Map012", "Items") only that file is translated, which makes a quick sample.
fn translate_rpgmaker(o: &Options, chat: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<()> {
    let log = |s: String| report(Event::Log(s));
    if o.reference.is_some() {
        log("RPG Maker: the official-translation folder is not used.".into());
    }
    let work = o.game.join("vntl_work");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let texts = rpgmaker::texts(&o.game)?;
    let only = o.file.as_ref().or(o.label.as_ref());
    if let Some(f) = only.filter(|f| !texts.iter().any(|t| t.file.trim_end_matches(".json") == f.as_str())) {
        return Err(format!("No Japanese text in a file called '{f}'"));
    }
    let cache_path = cache_file(&o.game, &o.lang);
    let mut cache = load_cache(&cache_path, o.fresh, &log)?;
    let save = |c: &Cache| save_json(&cache_path, c);
    let wanted = |t: &&rpgmaker::Text| only.is_none_or(|f| t.file.trim_end_matches(".json") == f || t.place == rpgmaker::NAME);
    drop_runaways(&mut cache, texts.iter().map(|t| (&t.text, &t.text)), &log);

    let mut seen = HashSet::new();
    let chosen: Vec<&rpgmaker::Text> = texts.iter().filter(wanted).filter(|t| seen.insert(&t.text)).collect();
    let todo = |place: &dyn Fn(&str) -> bool, cache: &Cache| -> Vec<(String, String, String)> {
        chosen
            .iter()
            .filter(|t| place(&t.place) && !cache.lines.contains_key(&t.text))
            .map(|t| (t.text.clone(), match t.speaker.as_str() {
                "" if place(rpgmaker::STORY) => "narration".to_string(),
                "" => t.place.clone(),
                jp => cache.lines.get(jp).cloned().unwrap_or_else(|| jp.to_string()),
            }, t.text.clone()))
            .collect()
    };
    let is_name = |p: &str| p == rpgmaker::NAME;
    let is_story = |p: &str| p == rpgmaker::STORY;
    let names = todo(&is_name, &cache);
    let system = SYSTEM
        .replace("Copy Ren'Py markup exactly: everything inside {curly braces} or [square brackets] stays unchanged and in the same order. Never add new braces or brackets.",
                 "Copy control codes exactly: anything starting with a backslash, like \\C[1] or \\V[3], and %1, stays unchanged and in the same order. Never add new ones.");
    translate_labels(o, chat, &system, NAMES_HINT, &names, &mut cache, &save, cancel)?;
    let glossary: Vec<(String, String)> = chosen
        .iter()
        .filter(|t| is_name(&t.place))
        .filter_map(|t| Some((t.text.clone(), cache.lines.get(&t.text)?.clone())))
        .collect();
    let left = chosen.iter().any(|t| is_story(&t.place) && !cache.lines.contains_key(&t.text));
    let story: Vec<(String, String)> = texts.iter().filter(|t| left && is_story(&t.place))
        .map(|t| (if t.speaker.is_empty() { NARRATION.to_string() } else { t.speaker.clone() }, t.text.clone()))
        .collect();
    let notes = notes_with_genders(o, &glossary, &story, &Looks::default(), &log, cancel)?;
    let system = with_notes(&system, &notes);
    log(format!("Character notes for the model: {}", work.join("characters.txt").display()));
    // a name corrected in the Characters list goes into the game too
    for c in load_characters(&o.game).into_iter().filter(|c| !c.english.is_empty()) {
        if let Some(en) = cache.lines.get_mut(&c.original) {
            *en = c.english;
        }
    }
    let written_as: HashMap<String, String> =
        chosen.iter().filter(|t| is_name(&t.place)).filter_map(|t| Some((cache.lines.get(&t.text)?.clone(), t.text.clone()))).collect();
    let is_term = |p: &str| p == "interface term" || p == "game title";
    let terms = todo(&is_term, &cache);
    let (story, interface) = (todo(&is_story, &cache), todo(&|p| !is_name(p) && !is_story(p) && !is_term(p), &cache));
    log(format!("RPG Maker: {} different texts; {} story and {} interface texts to translate with {}", chosen.len(), story.len(), interface.len() + terms.len(), o.model));
    translate_labels(o, chat, &system, TERMS_HINT, &terms, &mut cache, &save, cancel)?;
    // one bar for both: the interface carries on from the story
    let whole = story.len() + interface.len();
    let carry_on = |before: usize, e: Event| report(if let Event::Progress(done, _) = e { Event::Progress(before + done, whole) } else { e });
    translate_lines(o, chat, &system, &rpgmaker::MARKUP, &notes, Some(&written_as), &story, &mut cache, &save, &|e| carry_on(0, e), cancel)?;
    translate_lines(o, chat, &system, &rpgmaker::MARKUP, &notes, None, &interface, &mut cache, &save, &|e| carry_on(story.len(), e), cancel)?;
    // what VNTL couldn't keep the codes of, or left Japanese in; the other model's path splits at the codes
    let retry: Vec<(String, String, String)> = chosen
        .iter()
        .filter(|t| cache.lines.get(&t.text).is_none_or(|tr| JA.is_match(tr)))
        .map(|t| (t.text.clone(), t.place.clone(), t.text.clone()))
        .collect();
    if o.vision.is_some() && !retry.is_empty() {
        log(format!("Trying {} texts again with the image model", retry.len()));
        translate_labels(o, chat, &system, "", &retry, &mut cache, &save, cancel)?;
    }
    if let Some(model) = &o.vision {
        shorten_descriptions(o, model.as_ref(), &system, &chosen, &mut cache, &save, &log, cancel)?;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("Stopped".into());
    }
    log(write_rpgmaker(&o.game, &cache)?);
    let missing = chosen.iter().filter(|t| !cache.lines.contains_key(&t.text)).count();
    if missing > 0 {
        log(format!("{missing} texts kept in Japanese: the model's replies failed the checks."));
    }
    health(&cache, &log);
    if let Some(vision) = o.vision.as_ref().filter(|_| o.images && !crate::rgss::present(&o.game) && !crate::lcf::present(&o.game)) {
        log(crate::menus::translate_rpgmaker_menus(&o.game, vision.as_ref(), report, cancel)?);
    }
    Ok(())
}

/// Who does Ren'Py's names, menus and labels: they are instructions, which VNTL doesn't take, so the
/// other model if there is one.
fn instruction_model<'a>(o: &'a Options, chat: &'a dyn Chat) -> &'a dyn Chat {
    match &o.vision {
        Some(model) if is_vntl(&o.model) => model.as_ref(),
        _ => chat,
    }
}

pub fn translate(o: &Options, chat: &dyn Chat, report: &dyn Fn(Event), cancel: &AtomicBool) -> R<()> {
    let engine = detect(&o.game);
    if o.images && o.vision.is_none() && engine != "unity" {
        report(Event::Log("No image model chosen: menu images are left as they are.".into()));
    }
    match engine {
        "unity" => return translate_unity(o, chat, report, cancel),
        "liarsoft" => return translate_liar(o, chat, report, cancel),
        "rpgmaker-mv/mz" | "rpgmaker-xp/vx/ace" | "rpgmaker-2000/2003" => return translate_rpgmaker(o, chat, report, cancel),
        _ => {}
    }
    if let Some(e) = script_engine(engine) {
        return (e.translate)(o, chat, report, cancel);
    }
    let log = |s: String| report(Event::Log(s));
    let stopped = || cancel.load(Ordering::Relaxed);
    let work = o.game.join("vntl_work");
    let dump: Dump = if work.join("dump.json").exists() && !o.redump {
        load_json(&work.join("dump.json"))?
    } else {
        log("Reading the game's script (the game starts briefly)…".into());
        extract(&o.game, &work, "dump.json")?
    };
    let scene = o.label.as_ref().or(o.file.as_ref());
    if let Some(s) = scene.filter(|_| selected_blocks(&dump, o.label.as_deref(), o.file.as_deref()).is_empty()) {
        return Err(format!("No scene matches '{s}' (labels are case-sensitive)"));
    }
    let cache_path = cache_file(&o.game, &o.lang);
    let mut cache = load_cache(&cache_path, o.fresh, &log)?;
    let save = |c: &Cache| save_json(&cache_path, c);
    // Strings without Japanese (e.g. "[pc]") have nothing to translate; entries starting with
    // one of our own context labels came from an echoing model reply.
    let echoed: Vec<String> = ["menu choice", "interface text", "in-game text", "character name"].map(|l| format!("{l} | ")).into();
    cache.strings.retain(|k, v| JA.is_match(k) && !echoed.iter().any(|e| v.starts_with(e)));
    // a name corrected in the Characters list goes into the game too; an official one replaces it below
    cache.strings.extend(listed_names(&o.game));

    if o.reference.is_some() {
        apply_official(o, &dump, &mut cache, &work, &log)?;
        save(&cache)?;
    }
    let words = instruction_model(o, chat);
    let (system, spoken, name_texts) = translate_names(words, &dump, &mut cache, &save)?;
    clean_echoes(&dump, &mut cache, &spoken);

    let blocks = selected_blocks(&dump, o.label.as_deref(), o.file.as_deref());
    let lines: Vec<(String, &str)> = blocks
        .iter()
        .flat_map(|b| b.items.iter().enumerate().filter(|(_, it)| it.is_say()).map(move |(i, it)| (key(b, i), it.what())))
        .collect();
    let all: Vec<(String, String, String)> = lines.iter().map(|(k, what)| (k.clone(), spoken[k].clone(), what.to_string())).collect();
    fill_repeats(&all, &mut cache);
    let todo = still_to_ask(&all, &cache);
    let files: HashSet<&str> = blocks.iter().map(|b| b.file.as_str()).collect();
    let lo = blocks.iter().map(|b| b.line).min().unwrap_or(0);
    let hi = blocks.iter().map(|b| b.line).max().unwrap_or(0);
    // names were handled above; menus and in-script text follow the selected scenes
    let in_scope = |s: &StringEntry| -> bool {
        if name_texts.contains(&s.text) {
            false
        } else if s.kind == "ui" {
            o.ui || (o.label.is_none() && o.file.is_none())
        } else {
            s.kind != "name" && files.contains(s.file.as_str()) && (o.label.is_none() || (lo <= s.line && s.line <= hi))
        }
    };
    let pending: Vec<&StringEntry> =
        dump.strings.iter().filter(|s| !cache.strings.contains_key(&s.text) && JA.is_match(&s.text) && in_scope(s)).collect();

    log(format!("{} lines and {} menu/interface strings to translate with {}", todo.len(), pending.len(), o.model));
    let names: Vec<(String, String)> =
        name_texts.iter().filter_map(|s| Some((s.clone(), cache.strings.get(s)?.clone()))).collect();
    let story: Vec<(String, String)> = lines.iter().filter(|_| !todo.is_empty()).map(|(k, what)| (spoken[k].clone(), what.to_string())).collect();
    // a game with no pictures to go on still has its lines
    let sprites = if o.vision.is_some() && !story.is_empty() { sprites(&o.game, &dump.names).unwrap_or_default() } else { HashMap::new() };
    let notes = notes_with_genders(o, &names, &story, &Looks { sprites, aliases: kept_aliases(&o.game, &dump) }, &log, cancel)?;
    let system = with_notes(&system, &notes);
    let written_as: HashMap<String, String> = names.iter().map(|(jp, en)| (en.clone(), jp.clone())).collect();
    let run = translate_lines(o, chat, &system, &MARKUP, &notes, Some(&written_as), &todo, &mut cache, &save, report, cancel);
    let filled = fill_repeats(&all, &mut cache);
    save(&cache)?;
    run?;
    if filled > 0 {
        log(format!("{filled} repeated lines reused a translation"));
    }
    for chunk in pending.chunks(o.batch.max(1)) {
        if stopped() {
            return Err("Stopped".into());
        }
        let batch: Vec<(String, String)> = chunk.iter().map(|s| (string_kind(&s.kind).to_string(), s.text.clone())).collect();
        for (s, tr) in chunk.iter().zip(translate_batch(words, &system, &MARKUP, &batch)?) {
            if let Some(tr) = tr.filter(|t| !t.is_empty()) {
                cache.strings.insert(s.text.clone(), tr);
            }
        }
        save(&cache)?;
    }

    fit_labels(words, &system, &dump, &mut cache, &save, &log, cancel)?;
    if stopped() {
        return Err("Stopped".into());
    }
    let n = install(&o.game, &dump, &cache, &o.lang)?;
    let missing = lines.iter().filter(|(k, _)| !cache.lines.contains_key(k)).count();
    log(format!(
        "Wrote {n} translated blocks to game/tl/{}/ (Ren'Py load check OK). {missing} lines kept in Japanese: the model's replies failed the checks.",
        o.lang
    ));
    if let Some(vision) = o.vision.as_ref().filter(|_| o.images) {
        log(crate::menus::translate_renpy_menus(&o.game, &o.lang, vision.as_ref(), report, cancel)?);
    }
    if let Some(vision) = o.vision.as_ref().filter(|_| o.art) {
        match crate::ocr::installed() {
            true => log(crate::menus::translate_art(&o.game, &o.lang, vision.as_ref(), report, cancel)?),
            false => log(NO_READER.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_supported_engine_has_a_way_to_run_and_no_row_is_left_over() {
        let own = ["renpy", "unity", "liarsoft", "rpgmaker-mv/mz", "rpgmaker-xp/vx/ace", "rpgmaker-2000/2003"];
        for id in crate::renpy::SUPPORTED {
            assert!(own.contains(id) || script_engine(id).is_some(), "{id}");
        }
        assert_eq!(own.len() + SCRIPT_ENGINES.len(), crate::renpy::SUPPORTED.len());
    }

    #[test]
    fn a_name_the_model_put_in_its_tag_brackets_comes_out_bare() {
        let mut cache = Cache::default();
        cache.lines.insert(name_key("太郎"), "[Taro]".to_string());
        assert_eq!(english_name(&HashMap::new(), &cache, "太郎").as_deref(), Some("Taro"));
        let listed = HashMap::from([("太郎".to_string(), "Tarou".to_string())]);
        assert_eq!(english_name(&listed, &cache, "太郎").as_deref(), Some("Tarou"));
    }

    #[test]
    fn a_liar_soft_message_too_long_for_the_box_is_shortened_or_kept_out() {
        const LONG: &str = "This is a sentence that goes on and on, far past the four rows the box can show at once, \
                            and then keeps going with more words, and more, until the engine would stop on it, \
                            which is exactly what this test is about, so it keeps going a little longer still.";
        let lines: Vec<Line> = ["a#1", "a#2", "a#3"].map(|k| (k.to_string(), None, "長い話".to_string())).into();
        let mut cache = Cache::default();
        for (k, en) in [("a#1", LONG), ("a#2", LONG), ("a#3", "Short.")] {
            cache.lines.insert(k.into(), en.into());
        }
        let model = Scripted::new(&["A shorter line.", LONG, LONG]);
        let most = |en: &str, share: usize| en.chars().count() * liar::ROWS * share / (liar::rows(en) * 10);
        shorten(&model, &lines, &mut cache, &Fit { fits: &liar::fits, most: &most, markup: &liar::MARKUP }, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(cache.lines["a#1"], "A shorter line.");
        assert_eq!(cache.lines["a#2"], LONG, "two tries, then it stays as it was, and the write leaves it Japanese");
        assert_eq!(cache.lines["a#3"], "Short.", "what fits isn't asked about");
    }

    #[test]
    fn a_liar_soft_line_gets_the_name_on_its_plate() {
        let names = HashMap::from([("001".to_string(), "太郎".to_string())]);
        let texts = [("a.gsc", 0, "^g001^nおはよう"), ("a.gsc", 1, "^g999^n雨だった"), ("a.gsc", 2, "<1>はい"), ("a.gsc", 3, "SE01")];
        let lines = liar_lines(texts.iter().map(|(s, i, t)| (s.to_string(), *i, t.to_string())).collect(), &names);
        assert_eq!(lines, [
            ("a.gsc#0".to_string(), Some("太郎".to_string()), "おはよう".to_string()),
            ("a.gsc#1".to_string(), None, "雨だった".to_string()),
            ("a.gsc#2".to_string(), None, "はい".to_string()),
        ]);
    }
    use crate::llm::tests::{Fake, Scripted};
    use crate::renpy::tests::{cache, dump};
    use serde_json::json;
    use std::cell::Cell;

    fn options(game: &Path, model: &str) -> Options {
        Options { game: game.into(), lang: "english".into(), label: None, file: None, reference: None, ui: false, redump: false,
                  batch: 20, model: model.into(), retranslate: false, fresh: false, vision: None, images: false, art: false, history: Arc::new(AtomicUsize::new(VNTL_HISTORY)) }
    }

    fn todo(lines: &[&str]) -> Vec<(String, String, String)> {
        lines.iter().enumerate().map(|(i, t)| (format!("k{i}"), "narration".to_string(), t.to_string())).collect()
    }

    fn guessed(list: &[Character], story: &[(String, String)], reply: &'static str) -> Vec<String> {
        let game = temp_game(&format!("genders_{:?}", std::thread::current().id()).replace(['(', ')'], ""));
        save_characters(&game, list).unwrap();
        let model = Fake(|prompt: &str| Ok(if prompt.contains("Character: ユミ") { "Female.".to_string() } else { reply.to_string() }));
        guess_genders(&game, &model, story, &Looks::default(), &|_| {}, &AtomicBool::new(false)).unwrap();
        let out = load_characters(&game).into_iter().map(|c| c.gender).collect();
        let _ = std::fs::remove_dir_all(&game);
        out
    }

    fn says(who: &str, n: usize) -> Vec<(String, String)> {
        (0..n).map(|i| (who.to_string(), format!("台詞{i}"))).collect()
    }

    #[test]
    fn blank_genders_are_filled_and_set_ones_stay() {
        let row = |jp: &str, gender: &str| Character { original: jp.into(), gender: gender.into(), ..Default::default() };
        let story = [says("ユミ", 6), says("ケン", 6), says("ハナ", 6)].concat();
        let list = [row("ユミ", ""), row("ケン", "Non-binary"), row("ハナ", "")];
        assert_eq!(guessed(&list, &story, "Male"), ["Female", "Non-binary", "Male"]);
        assert_eq!(guessed(&list, &story, "It is hard to say. Unknown."), ["Female", "Non-binary", ""]);
    }

    #[test]
    fn a_character_with_few_lines_or_an_english_label_is_handled() {
        let few = [Character { original: "ユミ".into(), ..Default::default() }];
        assert_eq!(guessed(&few, &says("ユミ", MIN_SPOKEN - 1), "Male"), [""]);
        let english = [Character { original: "ハナ".into(), english: "Hana".into(), ..Default::default() }];
        assert_eq!(guessed(&english, &says("Hana", MIN_SPOKEN), "Male"), ["Male"]);
    }

    /// What the sprite and the lines are answered with, and what comes of it: the gender and the log.
    fn drawn_and_told(drawn: &'static str, told: &'static str, lines: usize) -> (String, Vec<String>) {
        let game = temp_game(&format!("sprites_{drawn}_{told}_{lines}"));
        save_characters(&game, &[Character { original: "ユミ".into(), ..Default::default() }]).unwrap();
        let model = Fake(|prompt: &str| Ok(if prompt.contains("sprite") { drawn } else { told }.to_string()));
        let logged = std::sync::Mutex::new(Vec::new());
        let looks = Looks { sprites: HashMap::from([("ユミ".to_string(), vec![1u8])]), ..Default::default() };
        guess_genders(&game, &model, &says("ユミ", lines), &looks, &|l| logged.lock().unwrap().push(l), &AtomicBool::new(false)).unwrap();
        let gender = load_characters(&game).remove(0).gender;
        let _ = std::fs::remove_dir_all(&game);
        (gender, logged.into_inner().unwrap())
    }

    #[test]
    fn the_sprite_beats_the_lines_and_the_clash_is_flagged() {
        let (gender, log) = drawn_and_told("Female", "Male", 6);
        assert_eq!(gender, "Female");
        assert!(log.iter().any(|l| l.contains("the sprite says Female and the lines say Male")));
        assert!(!drawn_and_told("Female", "Female", 6).1.iter().any(|l| l.contains("the sprite says")));
    }

    #[test]
    fn the_lines_decide_when_the_sprite_could_be_either() {
        assert_eq!(drawn_and_told("Unknown", "Male", 6).0, "Male");
        assert_eq!(drawn_and_told("Unknown", "Unknown", 6).0, "");
    }

    #[test]
    fn a_sprite_is_enough_for_a_character_with_few_lines() {
        assert_eq!(drawn_and_told("Male", "Female", MIN_SPOKEN - 1).0, "Male");
    }

    #[test]
    fn another_name_of_a_character_takes_their_answer_and_their_sprite_counts() {
        let game = temp_game("aliases");
        let row = |jp: &str, gender: &str| Character { original: jp.into(), gender: gender.into(), ..Default::default() };
        save_characters(&game, &[row("ソウ", ""), row("白髪の男", ""), row("ハヤト", ""), row("アオイ", ""), row("ハナ", "Female"), row("偽者", "Male")]).unwrap();
        let aliases = [("白髪の男", "ソウ"), ("アオイ", "ハヤト"), ("[pc]", "ハヤト"), ("偽者", "ハナ")].map(|(a, m)| (a.to_string(), m.to_string()));
        // the protagonist's picture is filed under the name variable the game shows
        let looks = Looks { sprites: HashMap::from([("ソウ".to_string(), vec![1u8]), ("[pc]".to_string(), vec![2u8])]), aliases: aliases.into_iter().collect() };
        let asked = AtomicUsize::new(0);
        let model = Fake(|p: &str| {
            asked.fetch_add(1, Ordering::Relaxed);
            Ok(if p.contains("sprite") { "Female" } else { "Male" }.to_string())
        });
        let story = [says("白髪の男", 6), says("アオイ", 6)].concat();
        guess_genders(&game, &model, &story, &looks, &|_| {}, &AtomicBool::new(false)).unwrap();
        let got: Vec<String> = load_characters(&game).into_iter().map(|c| c.gender).collect();
        // the disguise follows the sprite, and a gender set by hand stays
        assert_eq!(got, ["Female", "Female", "Female", "Female", "Female", "Male"]);
        assert_eq!(asked.load(Ordering::Relaxed), 4, "a sprite and the lines for each character, not each name");
        let _ = std::fs::remove_dir_all(&game);
    }

    #[test]
    fn an_import_fills_blanks_and_leaves_what_is_there() {
        let game = temp_game("import");
        let row = |jp: &str, en: &str, gender: &str| Character { original: jp.into(), english: en.into(), gender: gender.into(), ..Default::default() };
        save_characters(&game, &[row("ソウ", "", ""), row("ユミ", "Yumi", "Female")]).unwrap();
        let person = |name: &str, jp: &str, gender: &str, about: &str| Person { name: name.into(), original: jp.into(), gender: gender.into(), description: about.into(), ..Default::default() };
        let found = Found {
            id: "v1".into(), title: "Test".into(), synopsis: "A rainy town.".into(),
            people: vec![person("Sou Tachibana", "ソウ・タチバナ", "Female", "First."), person("Yumie", "ユミ", "Male", ""), person("Ken", "ケン", "Male", "Second.")],
            ..Default::default()
        };
        let said = import_game_data(&game, &found).unwrap();
        assert!(said.contains("1 characters added, 2 genders filled in"), "{said}");
        let rows: Vec<(String, String, String)> = load_characters(&game).into_iter().map(|c| (c.original, c.english, c.gender)).collect();
        let want = |a: &str, b: &str, c: &str| (a.to_string(), b.to_string(), c.to_string());
        assert_eq!(rows, [want("ソウ", "Sou Tachibana", "Female"), want("ユミ", "Yumi", "Female"), want("ケン", "Ken", "Male")]);
        assert_eq!(load_descriptions(&game)["ソウ"], "First.");

        // edits survive importing again
        save_description(&game, "ソウ", "My own.").unwrap();
        save_synopsis(&game, "My synopsis.").unwrap();
        import_game_data(&game, &found).unwrap();
        assert_eq!((load_descriptions(&game)["ソウ"].as_str(), load_synopsis(&game).as_str()), ("My own.", "My synopsis."));
        assert_eq!(load_characters(&game).len(), 3, "no duplicates");
        let _ = std::fs::remove_dir_all(&game);
    }

    #[test]
    fn the_synopsis_reaches_the_notes_and_survives_saving_the_list() {
        let game = temp_game("synopsis");
        save_characters(&game, &[Character { original: "ユミ".into(), ..Default::default() }]).unwrap();
        save_synopsis(&game, "Two lines\nbecome one.").unwrap();
        save_characters(&game, &load_characters(&game)).unwrap();
        assert_eq!(load_synopsis(&game), "Two lines become one.");
        let notes = character_notes(&game, &[]).unwrap();
        assert!(notes.iter().any(|n| n == "[synopsis] Two lines become one."), "{notes:?}");
        save_synopsis(&game, "").unwrap();
        assert_eq!(load_synopsis(&game), "");
        assert_eq!(load_characters(&game).len(), 1);
        let _ = std::fs::remove_dir_all(&game);
    }

    #[test]
    fn a_broken_link_stays_broken_until_it_is_mended() {
        let game = temp_game("links");
        let row = |jp: &str, en: &str| Character { original: jp.into(), english: en.into(), ..Default::default() };
        save_characters(&game, &[row("ハヤト", "Hayato"), row("アオイ", "Aoi"), row("リナ", "Rina"), row("ハナ", "Hana")]).unwrap();
        let script = json!({"blocks": [], "names": {"pl": "[pc]"}, "shown_vars": {
            "pc": {"assigns": [{"code": "pc = \"ハヤト\""}]}, "pc2": {"assigns": [{"code": "pc2 = \"アオイ\""}, {"code": "pc2 = \"リナ\""}]}}});
        std::fs::write(game.join("vntl_work").join("dump.json"), script.to_string()).unwrap();
        let seen = || links(&game).into_iter().map(|l| (l.name, l.main, l.broken)).collect::<Vec<_>>();
        let link = |name: &str, broken| (name.to_string(), "Hayato".to_string(), broken);
        assert_eq!(seen(), [link("Aoi", false), link("Rina", false)]);

        set_unlinked(&game, "リナ", true).unwrap();
        assert_eq!(seen(), [link("Aoi", false), link("Rina", true)], "still listed, so it can be mended");
        let dump: Dump = load_json(&game.join("vntl_work").join("dump.json")).unwrap();
        let kept = kept_aliases(&game, &dump);
        assert!(kept.contains_key("アオイ") && !kept.contains_key("リナ"));

        set_unlinked(&game, "リナ", false).unwrap();
        assert_eq!(seen(), [link("Aoi", false), link("Rina", false)]);
        let _ = std::fs::remove_dir_all(&game);
    }

    #[test]
    fn a_game_without_a_saved_script_has_no_links() {
        assert!(links(Path::new("no such game")).is_empty());
    }

    #[test]
    fn the_model_sees_one_character_with_their_other_names() {
        let notes = ["[character] Name: Hayato (ハヤト) | Gender: Male", "[character] Name: Aoi (アオイ) | Gender: Male", "[character] Name: リナ", "[character] Name: Hana (ハナ)", "a note of my own"].map(String::from).to_vec();
        let aliases = [("アオイ", "ハヤト"), ("リナ", "ハヤト"), ("偽者", "ハナ"), ("ケン", "いない人")].map(|(a, m)| (a.to_string(), m.to_string())).into_iter().collect();
        assert_eq!(with_aliases(notes, &aliases), ["[character] Name: Hayato (ハヤト) | Gender: Male | Aliases: Aoi (アオイ), リナ", "[character] Name: Hana (ハナ)", "a note of my own"]);
    }

    #[test]
    fn a_sprite_is_found_by_the_folder_named_after_its_speaker() {
        assert_eq!(crate::reference::sprite_code("images/char/ch_07_ab/ab_smile.png"), Some("ab"));
        assert_eq!(crate::reference::sprite_code("images/char/ch_07_ab/ab_smile.webp"), Some("ab"));
        assert_eq!(crate::reference::sprite_code("images/char/ch_07_ab/ab.txt"), None);
        assert_eq!(crate::reference::sprite_code("images/bg_ab/park.png"), None);
        assert_eq!(crate::reference::sprite_code("ab.png"), None);
    }

    #[test]
    fn the_model_reads_moments_from_the_whole_story_with_the_lines_around_them() {
        let game = temp_game("gender_sample");
        save_characters(&game, &[Character { original: "ユミ".into(), ..Default::default() }]).unwrap();
        let mut story = Vec::new();
        for i in 0..200 {
            story.push((NARRATION.to_string(), format!("地の文{i}。")));
            story.push(("ユミ".to_string(), format!("台詞{i}。")));
        }
        let seen = std::sync::Mutex::new(String::new());
        let model = Fake(|prompt: &str| {
            *seen.lock().unwrap() = prompt.to_string();
            Ok("Female".to_string())
        });
        guess_genders(&game, &model, &story, &Looks::default(), &|_| {}, &AtomicBool::new(false)).unwrap();
        let prompt = seen.into_inner().unwrap();
        assert!(prompt.contains("ユミ: 台詞0。") && prompt.contains("ユミ: 台詞183。"), "spread over the story");
        assert!(prompt.contains("narration: 地の文1。"), "with the lines around");
        assert!(prompt.lines().count() < 70);
        let _ = std::fs::remove_dir_all(&game);
    }

    fn temp_game(name: &str) -> PathBuf {
        let game = std::env::temp_dir().join(format!("vntl_{name}_{}", std::process::id()));
        std::fs::create_dir_all(game.join("vntl_work")).unwrap();
        game
    }

    #[test]
    fn labels_shortened_with_two_attempts_and_menus_left_alone() {
        let ui = dump(json!({"blocks": [], "strings": [{"text": "設定", "kind": "ui"}, {"text": "終了", "kind": "ui"},
                                                       {"text": "はい", "kind": "menu"}]}));
        let mut c = cache(json!({"strings": {"設定": "Settings", "終了": "Exit Game", "はい": "Yes!!"}}));
        let (save, log, go) = (|_: &Cache| Ok(()), |_: String| (), AtomicBool::new(false));
        fit_labels(&Scripted::new(&["1. Config\n2. Cfg", "1. Quit game"]), "", &ui, &mut c, &save, &log, &go).unwrap();
        assert_eq!(c.fit["設定"], [json!("Settings"), json!("Cfg"), json!(1)]);
        assert_eq!(c.fit["終了"], [json!("Exit Game"), json!("Exit Game"), json!(1)]);
        assert!(!c.fit.contains_key("はい"));
        fit_labels(&Scripted::new(&["1. Leave now"]), "", &ui, &mut c, &save, &log, &go).unwrap(); // second attempt for 終了 only
        fit_labels(&Scripted::new(&[]), "", &ui, &mut c, &save, &log, &go).unwrap(); // no third attempt
        assert_eq!(c.fit["終了"], [json!("Exit Game"), json!("Exit Game"), json!(2)]);
    }

    #[test]
    fn labels_stop_on_request_and_keep_nothing_half_done() {
        let ui = dump(json!({"blocks": [], "strings": [{"text": "設定", "kind": "ui"}]}));
        let mut c = cache(json!({"strings": {"設定": "Settings"}}));
        let got = fit_labels(&Scripted::new(&[]), "", &ui, &mut c, &|_| Ok(()), &|_| (), &AtomicBool::new(true));
        assert_eq!(got, Err("Stopped".to_string()));
        assert!(c.fit.is_empty());
    }

    #[test]
    fn only_interface_labels_are_shortened_and_a_preamble_is_no_label() {
        let ui = dump(json!({"blocks": [], "defaults": {"store.na": ["葵"]},
                             "strings": [{"text": "葵", "kind": "ui"}, {"text": "オプション", "kind": "ui"}, {"text": "ファイル", "kind": "ui"}]}));
        // 葵 is a name variable, 美咲 an official string the dump never saw: neither is a button
        let mut c = cache(json!({"strings": {"葵": "Aoi", "美咲": "Misaki", "オプション": "Preferences", "ファイル": "Save files"},
                                 "fit": {"葵": ["Aoi", "Ao", 1], "美咲": ["Misaki", "Misa", 1]}}));
        fit_labels(&Scripted::new(&["Sure!\n1. Options", "Files\nFile"]), "", &ui, &mut c, &|_| Ok(()), &|_| (), &AtomicBool::new(false)).unwrap();
        assert_eq!(c.fit.keys().collect::<Vec<_>>(), ["オプション", "ファイル"]);
        assert_eq!(c.fit["オプション"][1], json!("Options"));
        assert_eq!(c.fit["ファイル"][1], json!("Files")); // an unnumbered list is what the prompt asks for
    }

    #[test]
    fn a_dead_server_stops_the_run_but_bad_lines_are_only_skipped() {
        let o = options(Path::new("."), "vntl");
        let model = Fake(|p: &str| match p.contains('壊') {
            true => Err("model server: http status: 500".to_string()),
            false => Ok("Hello".to_string()),
        });
        let run = |lines: &[&str]| {
            let mut c = Cache::default();
            let got = translate_lines(&o, &model, "", &MARKUP, &[], None, &todo(lines), &mut c, &|_| Ok(()), &|_| {}, &AtomicBool::new(false));
            (got, c.lines.len())
        };
        let lines = [["壊"; 9].as_slice(), &["やあ"], &["壊"; 9]].concat();
        assert_eq!(run(&lines), (Ok(()), 1));
        assert!(run(&["壊"; 10]).0.unwrap_err().contains("keeps failing"));
    }

    #[test]
    fn each_finished_line_is_reported_and_a_lost_one_is_not() {
        let o = options(Path::new("."), "vntl");
        let model = Fake(|p: &str| if p.contains('壊') { Err("model server: http status: 500".to_string()) } else { Ok("Hello".to_string()) });
        let lines = std::cell::RefCell::new(Vec::new());
        let report = |e: Event| {
            if let Event::Line(original, english) = e {
                lines.borrow_mut().push((original, english));
            }
        };
        translate_lines(&o, &model, "", &MARKUP, &[], None, &todo(&["やあ", "壊", "どうも"]), &mut Cache::default(), &|_| Ok(()), &report, &AtomicBool::new(false)).unwrap();
        let hello = |jp: &str| (jp.to_string(), "Hello".to_string());
        assert_eq!(*lines.borrow(), [hello("やあ"), hello("どうも")]);
    }

    #[test]
    fn the_memory_can_change_during_a_run() {
        let o = options(Path::new("."), "vntl");
        o.history.store(3, Ordering::Relaxed);
        let seen = std::sync::Mutex::new(Vec::new());
        let model = Fake(|p: &str| {
            let mut seen = seen.lock().unwrap();
            seen.push(p.matches("Japanese<|end_header_id|>").count() - 1);
            match seen.len() {
                5 => o.history.store(1, Ordering::Relaxed),
                6 => o.history.store(4, Ordering::Relaxed),
                _ => (),
            }
            Ok("Hi".to_string())
        });
        translate_lines(&o, &model, "", &MARKUP, &[], None, &todo(&["やあ"; 9]), &mut Cache::default(), &|_| Ok(()), &|_| {}, &AtomicBool::new(false)).unwrap();
        // lowering it applies to the very next line; raising it fills up as lines are translated
        assert_eq!(*seen.lock().unwrap(), [0, 1, 2, 3, 4, 1, 2, 3, 4]);
    }

    #[test]
    fn the_memory_is_cut_in_one_go_so_the_prompt_keeps_its_start() {
        let o = options(Path::new("."), "vntl");
        o.history.store(2, Ordering::Relaxed);
        let seen = std::sync::Mutex::new(Vec::new());
        let model = Fake(|p: &str| {
            seen.lock().unwrap().push(p.matches("Japanese<|end_header_id|>").count() - 1);
            Ok("Hi".to_string())
        });
        translate_lines(&o, &model, "", &MARKUP, &[], None, &todo(&["やあ"; 9]), &mut Cache::default(), &|_| Ok(()), &|_| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(*seen.lock().unwrap(), [0, 1, 2, 3, 4, 2, 3, 4, 2]);
    }

    #[test]
    fn a_long_line_said_again_by_the_same_speaker_is_translated_once() {
        let game = std::env::temp_dir().join(format!("vntl_repeats_{}", std::process::id()));
        std::fs::create_dir_all(&game).unwrap();
        let long = "これは十二文字より長い同じ行です".to_string();
        let line = |k: &str, who: Option<&str>, t: &str| (k.to_string(), who.map(String::from), t.to_string());
        let lines = [line("a", None, &long), line("b", None, &long), line("c", Some("太郎"), &long), line("d", None, "はい"), line("e", None, "はい")];
        let asked = AtomicUsize::new(0);
        let model = Fake(|_: &str| {
            asked.fetch_add(1, Ordering::Relaxed);
            Ok("Hello".to_string())
        });
        translate_script(&options(&game, "vntl"), &model, &|_| {}, &AtomicBool::new(false), "Test", &lines, &NO_MARKUP, &|c| {
            assert!(lines.iter().all(|l| c.lines.contains_key(&l.0)));
            Ok(String::new())
        }).unwrap();
        // the name, the two speakers' long lines, and both short ones
        assert_eq!(asked.load(Ordering::Relaxed), 5);
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_repeat_of_a_line_saved_by_an_earlier_run_is_filled_in() {
        let long = "これは十二文字より長い同じ行です".to_string();
        let row = |k: &str, sp: &str, t: &str| (k.to_string(), sp.to_string(), t.to_string());
        let all = [row("a", "narration", &long), row("b", "narration", &long), row("c", "Taro", &long), row("d", "narration", "はい"), row("e", "narration", "はい")];
        let mut c = Cache::default();
        c.lines.insert("b".into(), "Saved".into());
        c.lines.insert("d".into(), "Yes".into());
        assert_eq!(fill_repeats(&all, &mut c), 1);
        assert_eq!(c.lines["a"], "Saved");
        assert!(!c.lines.contains_key("c") && !c.lines.contains_key("e"));
        let left = still_to_ask(&all, &c);
        assert_eq!(left.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(), ["c", "e"]);
        let fresh = still_to_ask(&all, &Cache::default());
        assert_eq!(fresh.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(), ["a", "c", "d", "e"], "a long line once for each speaker, a short one every time");
    }

    #[test]
    fn an_error_or_a_stop_mid_chunk_keeps_the_finished_lines() {
        let o = options(Path::new("."), "vntl");
        let saved = Cell::new(0);
        let save = |c: &Cache| {
            saved.set(c.lines.len());
            Ok(())
        };
        let model = Fake(|p: &str| if p.contains('切') { Err("model server: timed out".to_string()) } else { Ok("Hi".to_string()) });
        let got = translate_lines(&o, &model, "", &MARKUP, &[], None, &todo(&["やあ", "切"]), &mut Cache::default(), &save, &|_| {}, &AtomicBool::new(false));
        assert_eq!((got, saved.get()), (Err("model server: timed out".to_string()), 1));

        saved.set(0);
        let stop = AtomicBool::new(false);
        let model = Fake(|_: &str| {
            stop.store(true, Ordering::Relaxed);
            Ok("Hi".to_string())
        });
        let got = translate_lines(&o, &model, "", &MARKUP, &[], None, &todo(&["やあ", "どうも"]), &mut Cache::default(), &save, &|_| {}, &stop);
        assert_eq!((got, saved.get()), (Err("Stopped".to_string()), 1));
    }

    #[test]
    fn saved_refusals_are_forgotten_but_official_lines_are_not() {
        let game = temp_game("refusals");
        let path = game.join("cache.json");
        std::fs::write(&path, json!({"lines": {"a": "I cannot fulfill this request.", "b": "I'm sorry.", "c": "As an AI, I obey."},
                                     "strings": {"あ": "As an AI, I must decline."}, "official": ["c"]}).to_string()).unwrap();
        let c = load_cache(&path, false, &|_| ()).unwrap();
        assert_eq!(c.lines.keys().collect::<Vec<_>>(), ["b", "c"]);
        assert!(c.strings.is_empty());
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn starting_over_sets_saved_translations_aside_and_never_over_each_other() {
        let game = temp_game("fresh");
        let path = game.join("cache_en.json");
        for text in ["first", "second"] {
            std::fs::write(&path, text).unwrap();
            assert!(load_cache(&path, true, &|_| ()).unwrap().lines.is_empty());
        }
        assert_eq!(std::fs::read_to_string(game.join("cache_en.before-1.json")).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(game.join("cache_en.before-2.json")).unwrap(), "second");
        assert!(!path.exists());
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_reference_is_reused_only_for_the_folder_it_came_from() {
        let game = temp_game("reference");
        let from = game.join("reference_from.txt");
        assert!(!reference_is_from(&from, Path::new("H:/build A"))); // saved before the folder was recorded
        std::fs::write(&from, "H:/build A").unwrap();
        assert!(reference_is_from(&from, Path::new("H:/build A")));
        assert!(!reference_is_from(&from, Path::new("H:/build B")));
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_scene_filter_that_matches_nothing_is_an_error_before_any_work() {
        let game = temp_game("scene");
        std::fs::create_dir_all(game.join("renpy")).unwrap();
        std::fs::create_dir_all(game.join("game")).unwrap();
        let script = json!({"blocks": [{"id": "s", "label": "day_0", "items": []}]}).to_string();
        std::fs::write(game.join("vntl_work").join("dump.json"), script).unwrap();
        let o = Options { label: Some("Day_0".into()), ..options(&game, "x") };
        let got = translate(&o, &Scripted::new(&[]), &|_| {}, &AtomicBool::new(false));
        assert!(got.unwrap_err().contains("No scene matches 'Day_0'"));
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn an_rpgmaker_file_filter_is_checked_by_file_name() {
        let game = temp_game("rpgm");
        let data = game.join("data");
        std::fs::create_dir_all(&data).unwrap();
        for f in ["Classes", "Skills", "Items", "Weapons", "Armors", "Enemies", "States", "Troops", "CommonEvents"] {
            std::fs::write(data.join(format!("{f}.json")), "[]").unwrap();
        }
        std::fs::write(data.join("System.json"), "{}").unwrap();
        std::fs::write(data.join("Actors.json"), json!([null, {"name": "ハナ"}]).to_string()).unwrap();
        let file = |f: &str| Options { file: Some(f.into()), ..options(&game, "x") };
        let got = translate(&file("Map012"), &Scripted::new(&[]), &|_| {}, &AtomicBool::new(false));
        assert!(got.unwrap_err().contains("'Map012'"));
        // a real file whose only text is names still runs
        translate(&file("Actors"), &Scripted::new(&["1: Hana"]), &|_| {}, &AtomicBool::new(false)).unwrap();
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn with_vntl_the_words_go_to_the_model_that_takes_instructions() {
        let main = Scripted::new(&["main", "main", "main"]);
        let reply = |o: &Options| instruction_model(o, &main).chat("", "").unwrap();
        let vision = || Some(Box::new(Scripted::new(&["vision"])) as Box<dyn Chat + Send>);
        assert_eq!(reply(&Options { vision: vision(), ..options(Path::new("."), "vntl-llama3") }), "vision");
        assert_eq!(reply(&Options { vision: vision(), ..options(Path::new("."), "gemma3:12b") }), "main");
        assert_eq!(reply(&options(Path::new("."), "vntl-llama3")), "main");
    }

    #[test]
    fn listed_spellings_exclude_rows_that_are_no_spelling() {
        let game = temp_game("listed");
        let row = |jp: &str, en: &str| Character { original: jp.into(), english: en.into(), ..Default::default() };
        save_characters(&game, &[row("ハナ", "Hanna"), row("", "Bob"), row("ユミ", "ユミ"), row("ケン", "")]).unwrap();
        let listed = listed_names(&game);
        assert_eq!(listed, [("ハナ".to_string(), "Hanna".to_string())].into());
        std::fs::remove_dir_all(&game).unwrap();

        let mut c = cache(json!({"lines": {"CharacterNames#1": "Hana", "Story#1": "Hana"}}));
        let sources = [("CharacterNames#1".to_string(), "ハナ".to_string()), ("Story#1".to_string(), "ハナ".to_string())];
        correct_unity_names(&mut c, &sources, &listed);
        assert_eq!((c.lines["CharacterNames#1"].as_str(), c.lines["Story#1"].as_str()), ("Hanna", "Hana"));
    }

    #[test]
    fn extend_keeps_the_previous_speaker() {
        let d = dump(json!({"blocks": [{"id": "s", "items": [{"tpl": "", "who": "m", "what": "あ"},
            {"tpl": "", "who": "extend", "what": "い"}, {"tpl": "", "who": null, "what": "う"}]}]}));
        let who: HashMap<String, String> = [("m".to_string(), "Yumi".to_string())].into();
        let s = speakers(&d, &who);
        assert_eq!((s["s#0"].as_str(), s["s#1"].as_str(), s["s#2"].as_str()), ("Yumi", "Yumi", "narration"));
    }

    #[test]
    fn character_lines_round_trip_and_game_names_are_added_once() {
        let you = Character::parse("[character] Name: You (あなた) | Gender: Female | Role: the protagonist").unwrap();
        assert_eq!((you.original.as_str(), you.english.as_str(), you.gender.as_str(), you.extra.as_str()),
                   ("あなた", "You", "Female", "Role: the protagonist"));
        assert_eq!(you.line(), "[character] Name: You (あなた) | Gender: Female | Role: the protagonist");
        assert_eq!(Character::parse("[character] Name: ハナ").unwrap().original, "ハナ");
        let maya = Character { original: "ユミ".into(), gender: "Male".into(), ..Default::default() };
        assert_eq!(maya.line(), "[character] Name: ユミ | Gender: Male");
        let game = std::env::temp_dir().join(format!("vntl_chars_{}", std::process::id()));
        save_characters(&game, &[you]).unwrap();
        let names = [("あなた".to_string(), "You".to_string()), ("ハナ".to_string(), "Hana".to_string())];
        let notes = character_notes(&game, &names).unwrap();
        assert_eq!(notes, ["[character] Name: You (あなた) | Gender: Female | Role: the protagonist", "[character] Name: Hana (ハナ)"]);
        assert_eq!(character_notes(&game, &names).unwrap().len(), 2); // added once
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn a_list_file_that_is_not_utf8_is_never_overwritten() {
        let game = std::env::temp_dir().join(format!("vntl_chars_sjis_{}", std::process::id()));
        std::fs::create_dir_all(game.join("vntl_work")).unwrap();
        let shift_jis = b"[character] Name: \x83\x8a\x83\x41 | Gender: Female\n";
        std::fs::write(characters_path(&game), shift_jis).unwrap();
        assert!(load_characters(&game).is_empty());
        assert!(save_characters(&game, &[]).unwrap_err().contains("UTF-8"));
        assert!(character_notes(&game, &[("ハナ".to_string(), "Hana".to_string())]).is_err());
        assert_eq!(std::fs::read(characters_path(&game)).unwrap(), shift_jis);
        std::fs::remove_dir_all(&game).unwrap();
    }
}
