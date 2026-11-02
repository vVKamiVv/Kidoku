//! RPG Maker MV/MZ: all text is JSON in data/, so events, the database and the interface terms are
//! rewritten in place (originals kept as *.vntl-orig). A translation is found by its Japanese text,
//! which keeps names the same everywhere and survives game updates. No command is added or removed:
//! a save made in the middle of an event points at a command number. Extra lines go into the last
//! command as line breaks, which the engine accepts, and it opens a new page by itself when a
//! message runs past the box. Plugins' text is changed only where it is known to be text the player
//! reads: much of what looks like text in a plugin is looked up by its exact Japanese.

use crate::{lcf, rgss};
use crate::text::JA;
use crate::unity::{b64_decode, b64_encode};
use crate::{files_under, original, replace_file, R};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Control codes (\C[1], \N[\V[7]], \G, \{), %1 placeholders and, in plugin code, ${values}. Like the
/// engine, a code takes every letter after the backslash, so \G must not be followed directly by a word.
pub static MARKUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\[A-Za-z]+(?:\[(?:[^\[\]]|\[[^\]]*\])*\])?|\\[$.|^!<>{}\\]|%\d|\$\{[^{}]*\}").unwrap());
static NAMES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\[NnVv]\[(?:[^\[\]]|\[[^\]]*\])*\]").unwrap());
/// Glossary text and categories kept in database notes by SceneGlossary.
static GLOSSARY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<SG(説明\d*|カテゴリ\d*):([^<>]*)>").unwrap());
static TEMPLATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`(?:[^`\\\n]|\\.)*`").unwrap());
static QUOTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""(?:[^"\\\n`]|\\.)*"|'(?:[^'\\\n`]|\\.)*'"#).unwrap());
/// In a translation going into a JS string: a backslash with what it escapes if JS accepts that, any
/// other backslash, and quotes. An escape JS refuses is a syntax error that takes the whole plugin down.
static JS_TEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\\(?:[^ux0-9]|x[0-9a-fA-F]{2}|u[0-9a-fA-F]{4})|\\|["'`]"#).unwrap());
/// What comes before a string that is drawn: a drawing call, a variable named for text...
static DRAWN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:draw\w*|textWidth|addCommand|setText)\(|\b(?:const|let|var)\s+(?:\w*(?i:text|label|title|mess|names)\w*|\w*NAME\w*)\s*=[^(]*$").unwrap()
});
/// ...or a field of a list of commands, or the setting it falls back from.
static DRAWN_FIELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:\b(?:name|label|title):|\.ParamName\s*\|\|)\s*$").unwrap());
static GLUED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\$\{[^{}]*\})([A-Za-z])").unwrap());
static NOT_DRAWN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"console\.|throw |==|meta\[|registerCommand|\bcase ").unwrap());
static ACTOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\\[Nn]\[(\d+)\]$").unwrap());
static PLUGIN_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"KEY\s*=\s*"([^"]+)""#).unwrap());
static BOX_ADJUST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""name":"LL_MessageWindowAdjust","status":true.*"adjustNoFace":"(\d+)""#).unwrap());

pub const NAME: &str = "character name";
pub const STORY: &str = "story";
pub const DESCRIPTION: &str = "item or skill description";
pub const SCRIPT_TEXT: &str = "text in the game's code";
/// Text laid out narrower than this still gets this much room, in half-width letters.
const MIN_PICTURE_UNITS: usize = 40;
const COMMON_DAT: &str = "CommonEvents.dat";
pub(crate) const TERM: &str = "interface term";
/// Plugin settings the player reads. Everything else stays: `CategoryOrder` and `symbol` are here
/// because they must match translated text, which they do, as a text has one translation.
/// shortcut: known plugins only, so another game's plugin settings stay Japanese until added here.
const PLUGIN_TEXT: &[(&str, &[&str])] = &[
    ("AlchemySystem", &["MenuAlchemyText", "NeedMaterialText", "NeedPriceText", "TargetItemText"]),
    ("BattleVoiceMZ", &["Battle Voice Name at Option", "Volume Display Name"]),
    ("CustomizeErrorScreen", &["MainMessage"]),
    ("DoubleMagic", &["UseConstnuousMagicMessage"]),
    ("LL_MenuScreenCustom", &["symbol", "helpText", "leftBlockLabel", "rightBlockLabel"]),
    ("Mano_InputConfig", &["jp"]),
    ("NUUN_RandomItems", &["GetTextMessage"]),
    ("NUUN_Result", &["ResultName", "LevelUpResultHelpName", "ParamName"]),
    ("Ori_RecollectionExitButton", &["ButtonText"]),
    ("SaveLoadConfirm", &["termSave", "termLoad", "confirmOk", "confirmNg", "helpText"]),
    ("SceneGlossary", &["ConfirmUse", "ConfirmNoUse", "GlossaryHelp", "CategoryHelp", "CompleteMessage", "CategoryOrder"]),
    ("SimpleVoice", &["optionName"]),
    ("YKP_ShopManager", &["ItemCurrency"]),
];

/// One piece of Japanese where the game shows it. `speaker` is empty for narration.
pub struct Text {
    pub file: String,
    pub place: String,
    pub speaker: String,
    pub text: String,
}

pub(crate) type Visit<'a> = dyn FnMut(&str, &str, &str) -> Option<String> + 'a;

fn data_dir(game: &Path) -> PathBuf {
    let www = game.join("www").join("data");
    if www.is_dir() { www } else { game.join("data") }
}

fn parse(name: &str, bytes: &[u8]) -> R<Value> {
    // A BOM is fine by the game, whose text decoding drops it, but not by serde_json.
    serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)).map_err(|e| format!("{name}: {e}"))
}

// ---------- files ----------

/// The key of the plugin some games read an obscured CommonEvents.dat with.
/// shortcut: only base64 over a repeating XOR, as Ori_EncryptedCommonEvents does; add others as games need them.
fn dat_key(data: &Path) -> R<Vec<u8>> {
    let mut plugins = Vec::new();
    files_under(&data.parent().unwrap_or(data).join("js").join("plugins"), &mut plugins);
    plugins
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter(|code| code.contains(COMMON_DAT))
        .find_map(|code| Some(PLUGIN_KEY.captures(&code)?[1].as_bytes().to_vec()))
        .ok_or_else(|| format!("{COMMON_DAT}: no plugin in this game says how to read it"))
}

fn xor(bytes: &[u8], key: &[u8]) -> Vec<u8> {
    bytes.iter().zip(key.iter().cycle()).map(|(b, k)| b ^ k).collect()
}

fn decode_dat(text: &[u8], key: &[u8]) -> R<Value> {
    let text = String::from_utf8_lossy(text);
    let raw = b64_decode(text.trim()).ok_or_else(|| format!("{COMMON_DAT}: not base64"))?;
    parse(COMMON_DAT, &xor(&raw, key))
}

fn encode_dat(v: &Value, key: &[u8]) -> Vec<u8> {
    b64_encode(&xor(v.to_string().as_bytes(), key)).into_bytes()
}

/// The game's own data files with text in them, in the order the model should see them.
fn load(data: &Path) -> R<Vec<(String, Value)>> {
    let mut maps: Vec<String> = std::fs::read_dir(data)
        .map_err(|e| format!("{}: {e}", data.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json") && n.strip_prefix("Map").is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit())))
        .collect();
    maps.sort();
    let dat = !data.join("CommonEvents.json").exists() && data.join(COMMON_DAT).exists();
    let common = if dat { COMMON_DAT } else { "CommonEvents.json" };
    let db = ["Actors", "Classes", "Skills", "Items", "Weapons", "Armors", "Enemies", "States", "System", "Troops"];
    let mut files = Vec::new();
    for name in db.iter().map(|n| format!("{n}.json")).chain([common.to_string()]).chain(maps) {
        let bytes = original(&data.join(&name))?;
        let value = if name == COMMON_DAT { decode_dat(&bytes, &dat_key(data)?)? } else { parse(&name, &bytes)? };
        files.push((name, value));
    }
    Ok(files)
}

// ---------- layout ----------

/// Half-width letters in a line of a screen-wide window, `narrowed` px less wide on each side: it has
/// 4 px of margin on each side, 12 of padding and 4 before the text. shortcut: counts half an em per
/// letter, exact for the default M+ 1m and wider than proportional fonts, so those wrap a little early;
/// measure the font if that shows.
fn window_units(system: &Value, narrowed: usize) -> usize {
    let advanced = &system["advanced"]; // MV has no such block
    let width = advanced["uiAreaWidth"].as_u64().unwrap_or(816) as usize;
    let font = advanced["fontSize"].as_u64().unwrap_or(28) as usize;
    width.saturating_sub(36 + 2 * narrowed) / (font / 2)
}

/// Px a plugin takes off each side of the message box. shortcut: only LL_MessageWindowAdjust (face-less
/// messages), the one seen so far; a plugin that narrows the box some other way clips long lines.
fn box_narrowed(data: &Path) -> usize {
    let plugins = std::fs::read_to_string(data.parent().unwrap_or(data).join("js").join("plugins.js")).unwrap_or_default();
    BOX_ADJUST.captures(&plugins).and_then(|c| c[1].parse().ok()).unwrap_or(0)
}

/// How wide a line looks: codes draw nothing, a name or number takes about 8 letters, and Japanese
/// fonts draw everything outside ASCII full width, symbols like … and curly quotes too.
fn units(line: &str) -> usize {
    let shown = NAMES.replace_all(line, "XXXXXXXX");
    MARKUP.replace_all(&shown, "").chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// Break at spaces so no line is wider than `max`.
fn wrap(text: &str, max: usize) -> Vec<String> {
    let (mut lines, mut cur) = (Vec::new(), String::new());
    for word in text.split_whitespace() {
        if !cur.is_empty() && units(&cur) + 1 + units(word) > max {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    lines.push(cur);
    lines
}

/// The hand-broken `lines` as one text; its translation, reflowed. None if there is none.
pub(crate) fn reflow(lines: &[&str], max: usize, place: &str, speaker: &str, f: &mut Visit) -> Option<Vec<String>> {
    let text = lines.concat();
    JA.is_match(&text).then(|| f(place, speaker, &text)).flatten().map(|tr| wrap(&tr, max))
}

// ---------- walking the data ----------
// Fields are reached with get_mut: indexing a Value for writing would add what is missing.

fn each(v: Option<&mut Value>) -> impl Iterator<Item = &mut Value> {
    v.and_then(Value::as_array_mut).into_iter().flatten()
}

fn field(v: Option<&mut Value>, place: &str, f: &mut Visit) {
    let Some(v) = v else { return };
    let Some(s) = v.as_str().filter(|s| JA.is_match(s)) else { return };
    if let Some(tr) = f(place, "", s) {
        *v = tr.into();
    }
}

/// Japanese that goes on from a name: "(name)を手に入れた！". English needs a space there, but not before 's.
fn fragment(v: Option<&mut Value>, place: &str, f: &mut Visit) {
    let Some(v) = v else { return };
    let goes_on = v.as_str().is_some_and(|s| s.starts_with(['を', 'が', 'は', 'に', 'の', 'で', 'と']));
    field(Some(v), place, f);
    if let (true, Some(tr)) = (goes_on, v.as_str().filter(|t| !JA.is_match(t) && !t.starts_with(['\'', '’']))) {
        *v = format!(" {tr}").into();
    }
}

/// A database text shown in a window, with its own line breaks to make.
fn paragraph(v: Option<&mut Value>, max: usize, place: &str, f: &mut Visit) {
    let Some(v) = v else { return };
    let Some(s) = v.as_str() else { return };
    let lines: Vec<&str> = s.split('\n').collect();
    if let Some(tr) = reflow(&lines, max, place, "", f) {
        *v = tr.join("\n").into();
    }
}

/// A message: the commands from `start` that have `code`. Returns the command after them.
fn message(list: &mut [Value], start: usize, code: i64, max: usize, speaker: &str, f: &mut Visit) -> usize {
    let n = list[start..].iter().take_while(|c| c["code"] == code).count();
    let lines: Vec<&str> = list[start..start + n].iter().map(|c| c["parameters"][0].as_str().unwrap_or("")).collect();
    if let Some(out) = reflow(&lines, max, STORY, speaker, f) {
        for (k, c) in list[start..start + n].iter_mut().enumerate() {
            let s = if k + 1 == n { out.get(k..).unwrap_or(&[]).join("\n") } else { out.get(k).cloned().unwrap_or_default() };
            c["parameters"][0] = s.into();
        }
    }
    start + n
}

/// DTextPicture text: paragraphs between blank lines, kept as wide as the original's widest line.
fn picture_text(v: Option<&mut Value>, f: &mut Visit) {
    let Some(v) = v else { return };
    let Some(s) = v.as_str().map(String::from) else { return };
    let max = s.split('\n').map(units).max().unwrap_or(0).max(MIN_PICTURE_UNITS);
    let (mut out, mut para): (Vec<String>, Vec<&str>) = (Vec::new(), Vec::new());
    let mut flush = |para: &mut Vec<&str>, out: &mut Vec<String>| {
        match reflow(para, max, STORY, "", f) {
            Some(tr) => out.extend(tr),
            None => out.extend(para.iter().map(|l| l.to_string())),
        }
        para.clear();
    };
    for line in s.split('\n') {
        if line.trim().is_empty() {
            flush(&mut para, &mut out);
            out.push(line.to_string());
        } else {
            para.push(line);
        }
    }
    flush(&mut para, &mut out);
    *v = out.join("\n").into();
}

/// SceneGlossary's text in a database note. A category must match CategoryOrder, and does.
fn note(v: Option<&mut Value>, f: &mut Visit) {
    let Some(v) = v else { return };
    let Some(s) = v.as_str() else { return };
    let new = GLOSSARY.replace_all(s, |c: &regex::Captures| {
        let mut text = Value::from(&c[2]);
        match c[1].starts_with("説明") {
            true => {
                picture_text(Some(&mut text), f);
                // The first '>' ends the tag, and the Japanese can't have one but the English can.
                text = text.as_str().unwrap_or("").replace('<', "＜").replace('>', "＞").into();
            }
            false => field(Some(&mut text), TERM, f),
        }
        format!("<SG{}:{}>", &c[1], text.as_str().unwrap_or(""))
    });
    let new = new.into_owned();
    *v = new.into();
}

fn speaker_of(name: &str, actors: &[String]) -> String {
    match ACTOR.captures(name) {
        Some(c) => c[1].parse::<usize>().ok().and_then(|i| actors.get(i).cloned()).unwrap_or_default(),
        None if JA.is_match(name) => name.to_string(),
        None => String::new(), // \N[\V[7]] depends on the player's choices
    }
}

/// An event's commands. Comments, labels, scripts and sound names stay: they aren't shown, and
/// events and plugins look some of them up by their exact text.
fn events(list: Option<&mut Value>, max: usize, actors: &[String], f: &mut Visit) {
    let Some(list) = list.and_then(Value::as_array_mut) else { return };
    let mut i = 0;
    while i < list.len() {
        let code = list[i]["code"].as_i64().unwrap_or(0);
        let Some(p) = list[i].get_mut("parameters") else {
            i += 1;
            continue;
        };
        match code {
            // XP keeps a message's first line in the command itself
            101 if p.as_array().is_some_and(|a| a.len() == 1) => {
                list[i]["code"] = 401.into();
                i = message(list, i, 401, max, "", f);
                continue;
            }
            101 => {
                let speaker = speaker_of(p[4].as_str().unwrap_or(""), actors);
                let face = p[0].as_str().is_some_and(|s| !s.is_empty());
                field(p.get_mut(4), NAME, f);
                i = message(list, i + 1, 401, if face { max * 5 / 6 } else { max }, &speaker, f);
                continue;
            }
            105 => {
                i = message(list, i + 1, 405, max, "", f);
                continue;
            }
            102 => each(p.get_mut(0)).for_each(|c| field(Some(c), STORY, f)),
            320 | 324 | 325 => field(p.get_mut(1), "character profile", f),
            357 if p[0] == "DTextPicture" && p[1] == "dText" => picture_text(p.get_mut(3).and_then(|a| a.get_mut("text")), f),
            _ => {}
        }
        i += 1;
    }
}

fn pages(pages: Option<&mut Value>, max: usize, actors: &[String], f: &mut Visit) {
    each(pages).for_each(|p| events(p.get_mut("list"), max, actors, f));
}

fn system(v: &mut Value, full: usize, f: &mut Visit) {
    field(v.get_mut("gameTitle"), "game title", f);
    field(v.get_mut("currencyUnit"), TERM, f);
    for key in ["elements", "skillTypes", "weaponTypes", "armorTypes", "equipTypes"] {
        each(v.get_mut(key)).for_each(|s| field(Some(s), TERM, f));
    }
    // XP and VX keep each word in a field of its own
    for key in ["words", "terms"] {
        let fields = v.get_mut(key).and_then(Value::as_object_mut).into_iter().flatten();
        fields.filter(|(_, s)| s.is_string()).for_each(|(_, s)| field(Some(s), TERM, f));
    }
    let Some(terms) = v.get_mut("terms") else { return };
    for key in ["basic", "commands", "params", "etypes"] {
        each(terms.get_mut(key)).for_each(|s| field(Some(s), TERM, f));
    }
    let messages = terms.get_mut("messages").and_then(Value::as_object_mut);
    messages.into_iter().flatten().for_each(|(_, s)| paragraph(Some(s), full, "battle message", f));
}

/// Every text of one file, in order. What `f` returns for a text replaces it. `full` is the width of
/// a screen-wide window, for the database's help texts; `max` that of a message box.
fn walk(name: &str, v: &mut Value, full: usize, max: usize, actors: &[String], f: &mut Visit) {
    match name.trim_end_matches(".json") {
        "Actors" => each(Some(v)).for_each(|r| {
            field(r.get_mut("name"), NAME, f);
            field(r.get_mut("nickname"), "character profile", f);
            let about = if r.get("profile").is_some() { "profile" } else { "description" };
            paragraph(r.get_mut(about), full, "character profile", f);
        }),
        "Classes" | "Enemies" => each(Some(v)).for_each(|r| {
            field(r.get_mut("name"), "class or enemy name", f);
            note(r.get_mut("note"), f);
        }),
        "Items" | "Weapons" | "Armors" | "Skills" | "States" => each(Some(v)).for_each(|r| {
            field(r.get_mut("name"), "item or skill name", f);
            paragraph(r.get_mut("description"), full, DESCRIPTION, f);
            for m in ["message1", "message2", "message3", "message4"] {
                fragment(r.get_mut(m), "battle message", f);
            }
            note(r.get_mut("note"), f);
        }),
        "Troops" => each(Some(v)).for_each(|r| pages(r.get_mut("pages"), max, actors, f)),
        "CommonEvents" | COMMON_DAT => each(Some(v)).for_each(|r| events(r.get_mut("list"), max, actors, f)),
        "System" => system(v, full, f),
        _ => {
            field(v.get_mut("displayName"), "place name", f);
            each(v.get_mut("events")).for_each(|e| pages(e.get_mut("pages"), max, actors, f));
        }
    }
}

// ---------- images ----------

/// The key of encrypted images, if the game has one.
pub fn image_key(game: &Path) -> R<Vec<u8>> {
    let data = data_dir(game);
    let system = parse("System.json", &original(&data.join("System.json"))?)?;
    let hex = system["encryptionKey"].as_str().unwrap_or("");
    Ok((0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()).collect())
}

/// Where interface images are. Pictures may be encrypted (.png_, .rpgmvp) whatever System.json says,
/// so the file's own extension decides.
pub fn interface_images(game: &Path) -> Vec<PathBuf> {
    let img = data_dir(game).parent().unwrap_or(game).join("img");
    let mut out = Vec::new();
    ["system", "titles2", "pictures"].iter().for_each(|d| files_under(&img.join(d), &mut out));
    out.retain(|p| p.extension().is_some_and(|e| ["png", "png_", "rpgmvp"].iter().any(|x| e == *x)));
    out.sort();
    out
}

fn encrypted(path: &Path) -> bool {
    path.extension().is_none_or(|e| e != "png")
}

/// Only the first 16 bytes of the image are encrypted, behind a 16-byte header.
fn unmask(bytes: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let mut png = bytes.get(16..)?.to_vec();
    (!key.is_empty() && png.len() >= key.len()).then(|| {
        png.iter_mut().zip(key).for_each(|(b, k)| *b ^= k);
        png
    })
}

/// An image file as a plain PNG.
pub fn read_image(path: &Path, key: &[u8]) -> R<Vec<u8>> {
    let bytes = original(path)?;
    match encrypted(path) {
        true => unmask(&bytes, key).ok_or_else(|| format!("{}: cannot be decrypted", path.display())),
        false => Ok(bytes),
    }
}

/// Replace an image file with a plain PNG, encrypted again if it was.
pub fn write_image(path: &Path, key: &[u8], png: &[u8]) -> R<()> {
    if !encrypted(path) {
        return replace_file(path, png);
    }
    let header = original(path)?.get(..16).ok_or("image too short")?.to_vec();
    let body = unmask(&[&header[..], png].concat(), key).ok_or("image too short")?; // XOR undoes itself
    replace_file(path, &[header, body].concat())
}

// ---------- plugins ----------

/// A setting's value, which may hold JSON inside a string, inside a string.
fn plugin_setting(v: &mut Value, key: &str, keys: &[&str], f: &mut Visit) {
    match v {
        Value::Array(a) => a.iter_mut().for_each(|x| plugin_setting(x, key, keys, f)),
        Value::Object(o) => o.iter_mut().for_each(|(k, x)| plugin_setting(x, k, keys, f)),
        Value::String(s) if s.starts_with(['[', '{', '"']) && serde_json::from_str::<Value>(s).is_ok() => {
            let Ok(mut inner) = serde_json::from_str::<Value>(s) else { return };
            let before = inner.clone();
            plugin_setting(&mut inner, key, keys, f);
            if inner != before {
                *s = inner.to_string();
            }
        }
        Value::String(_) if keys.contains(&key) => fragment(Some(v), TERM, f),
        _ => {}
    }
}

/// Strings in one line of code that are drawn, translated. `re` finds strings with their quotes.
fn drawn_strings(line: &str, previous: &str, re: &Regex, f: &mut Visit) -> String {
    let (mut out, mut last) = (String::new(), 0);
    for m in re.find_iter(line) {
        let (raw, before) = (&m.as_str()[1..m.len() - 1], &line[..m.start()]);
        let returned = line.trim_start().strip_prefix("return").is_some_and(|rest| rest.starts_with([' ', '`']) && !before.contains('('));
        let drawn = returned || DRAWN.is_match(before) || DRAWN_FIELD.is_match(before) || previous.trim_end().ends_with(".add(");
        out.push_str(&line[last..m.start()]);
        last = m.end();
        match drawn.then(|| JA.is_match(raw)).filter(|ja| *ja).and_then(|_| f(TERM, "", raw)) {
            Some(tr) => {
                let quote = &m.as_str()[..1];
                // The text is JS source, so \\C[1] stays as it came. shortcut: a ${...} is only as sound as
                // same_markup keeps it, parse the JS if a model ever breaks one.
                let tr = GLUED.replace_all(&tr, "$1 $2").replace(['\r', '\n', '\u{2028}', '\u{2029}'], "\\n");
                let tr = JS_TEXT.replace_all(&tr, |c: &regex::Captures| match &c[0] {
                    "\\" => r"\\".to_string(),
                    q if q == quote || q == "`" => format!("\\{q}"), // this string may be inside a template
                    kept => kept.to_string(),
                });
                out.push_str(&format!("{quote}{tr}{quote}"));
            }
            None => out.push_str(m.as_str()),
        }
    }
    out + &line[last..]
}

/// A plugin's code with its drawn strings translated. Whole templates go first, and strings inside
/// their ${...} after, which is why a template's own text is found before those change.
fn plugin_code(code: &str, f: &mut Visit) -> String {
    let (mut comment, mut previous, mut out) = (false, "", String::with_capacity(code.len()));
    for line in code.split_inclusive('\n') {
        let t = line.trim_start();
        let opens = t.starts_with("/*");
        let skip = comment || opens || t.starts_with("//") || t.starts_with('*') || NOT_DRAWN.is_match(line);
        if comment || opens {
            comment = !t.contains("*/");
        }
        if skip {
            out.push_str(line);
        } else {
            out += &drawn_strings(&drawn_strings(line, previous, &TEMPLATE, f), previous, &QUOTED, f);
        }
        previous = line;
    }
    out
}

/// Write a file only if it differs. What an earlier run changed and this one doesn't goes back too.
fn update(path: &Path, new: &str) -> R<()> {
    match std::fs::read(path).is_ok_and(|now| now == new.as_bytes()) {
        true => Ok(()),
        false => replace_file(path, new.as_bytes()),
    }
}

/// Every text in the plugins that are on, with what `f` returns; written back if `write`.
fn plugins(data: &Path, write: bool, f: &mut Visit) -> R<()> {
    let js = data.parent().unwrap_or(data).join("js");
    let list_path = js.join("plugins.js");
    if !list_path.exists() {
        return Ok(());
    }
    let bytes = original(&list_path)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (Some(start), Some(end)) = (text.find('['), text.rfind(']')) else { return Ok(()) };
    let mut list = parse("plugins.js", &text.as_bytes()[start..=end])?;
    let before = list.clone();
    let mut on = Vec::new();
    for p in each(Some(&mut list)).filter(|p| p["status"] == true) {
        let name = p["name"].as_str().unwrap_or("").to_string();
        let keys = PLUGIN_TEXT.iter().find(|(n, _)| *n == name);
        if let (Some((_, keys)), Some(settings)) = (keys, p.get_mut("parameters")) {
            plugin_setting(settings, "", keys, f);
        }
        on.push(name);
    }
    if write {
        let rows: Vec<String> = list.as_array().into_iter().flatten().map(Value::to_string).collect();
        let new = if list == before { text.clone() } else { format!("{}[\n{}\n]{}", &text[..start], rows.join(",\n"), &text[end + 1..]) };
        update(&list_path, &new)?;
    }
    for name in on {
        let path = js.join("plugins").join(format!("{name}.js"));
        if !path.exists() {
            continue;
        }
        let Ok(code) = String::from_utf8(original(&path)?) else { continue };
        let new = plugin_code(&code, f);
        if write {
            update(&path, &new)?;
        }
    }
    Ok(())
}

/// (width of a screen-wide window, width of a message box).
fn widths(files: &[(String, Value)], data: &Path) -> (usize, usize) {
    let system = files.iter().find(|(n, _)| n == "System.json").map_or(&Value::Null, |(_, v)| v);
    (window_units(system, 0), window_units(system, box_narrowed(data)))
}

fn actor_names(files: &[(String, Value)]) -> Vec<String> {
    let actors = files.iter().find(|(n, _)| n == "Actors.json");
    let rows = actors.and_then(|(_, v)| v.as_array()).into_iter().flatten();
    rows.map(|a| a["name"].as_str().unwrap_or("").to_string()).collect()
}

/// Whether a description fits the help window, which has two lines.
pub fn description_fits(game: &Path, text: &str) -> R<bool> {
    let data = data_dir(game);
    let system = parse("System.json", &original(&data.join("System.json"))?)?;
    Ok(wrap(text, window_units(&system, 0)).len() <= 2)
}

/// All the Japanese the game shows, in game order, repeats included.
pub fn texts(game: &Path) -> R<Vec<Text>> {
    if lcf::present(game) {
        return lcf::texts(game);
    }
    if let Some(mut g) = rgss::Game::open(game)? {
        let ((full, max), actors) = (g.widths(), actor_names(&g.files));
        let mut out = Vec::new();
        for (name, v) in &mut g.files {
            let mut note = |place: &str, speaker: &str, text: &str| -> Option<String> {
                out.push(Text { file: name.clone(), place: place.into(), speaker: speaker.into(), text: text.into() });
                None
            };
            walk(name, v, full, max, &actors, &mut note);
        }
        out.extend(g.script_texts().into_iter().map(|text| Text { file: "Scripts.json".into(), place: SCRIPT_TEXT.into(), speaker: String::new(), text }));
        return Ok(out);
    }
    let data = data_dir(game);
    let mut files = load(&data)?;
    let ((full, max), actors) = (widths(&files, &data), actor_names(&files));
    let mut out = Vec::new();
    for (name, v) in &mut files {
        let mut note = |place: &str, speaker: &str, text: &str| -> Option<String> {
            out.push(Text { file: name.clone(), place: place.into(), speaker: speaker.into(), text: text.into() });
            None
        };
        walk(name, v, full, max, &actors, &mut note);
    }
    let mut note = |place: &str, speaker: &str, text: &str| -> Option<String> {
        out.push(Text { file: "plugins".into(), place: place.into(), speaker: speaker.into(), text: text.into() });
        None
    };
    plugins(&data, false, &mut note)?;
    Ok(out)
}

/// Rewrite the data files from the game's originals. Returns (texts replaced, files changed).
pub fn install(game: &Path, lookup: &dyn Fn(&str) -> Option<String>) -> R<(usize, usize)> {
    if lcf::present(game) {
        return lcf::install(game, lookup);
    }
    if let Some(mut g) = rgss::Game::open(game)? {
        let ((full, max), actors) = (g.widths(), actor_names(&g.files));
        let max = crate::fit::letters(game, max);
        let mut replaced = 0;
        for (name, v) in &mut g.files {
            let mut swap = |_: &str, _: &str, text: &str| -> Option<String> {
                let tr = lookup(text);
                replaced += tr.is_some() as usize;
                tr
            };
            walk(name, v, full, max, &actors, &mut swap);
        }
        replaced += g.translate_scripts(lookup);
        return Ok((replaced, g.save()?));
    }
    let data = data_dir(game);
    let mut files = load(&data)?;
    let ((full, max), actors) = (widths(&files, &data), actor_names(&files));
    let max = crate::fit::letters(game, max);
    let (mut replaced, mut changed) = (0, 0);
    for (name, v) in &mut files {
        let mut count = 0;
        let mut swap = |_: &str, _: &str, text: &str| -> Option<String> {
            let tr = lookup(text);
            count += tr.is_some() as usize;
            tr
        };
        walk(name, v, full, max, &actors, &mut swap);
        if count > 0 {
            let dat = name == COMMON_DAT;
            let bytes = if dat { encode_dat(v, &dat_key(&data)?) } else { v.to_string().into_bytes() };
            let back = if dat { decode_dat(&bytes, &dat_key(&data)?)? } else { parse(name, &bytes)? };
            if back != *v {
                return Err(format!("{name}: the rewritten file did not read back correctly"));
            }
            replace_file(&data.join(name.as_str()), &bytes)?;
            (replaced, changed) = (replaced + count, changed + 1);
        }
    }
    let mut count = 0;
    let mut swap = |_: &str, _: &str, text: &str| -> Option<String> {
        let tr = lookup(text);
        count += tr.is_some() as usize;
        tr
    };
    plugins(&data, true, &mut swap)?;
    Ok((replaced + count, changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd(code: i64, parameters: Value) -> Value {
        json!({"code": code, "indent": 0, "parameters": parameters})
    }

    fn swap(pairs: &'static [(&'static str, &'static str)]) -> impl FnMut(&str, &str, &str) -> Option<String> {
        move |_, _, t| pairs.iter().find(|(jp, _)| *jp == t).map(|(_, en)| en.to_string())
    }

    #[test]
    fn markup_covers_nested_codes_and_stops_at_the_text() {
        let found = |s| MARKUP.find_iter(s).map(|m| m.as_str()).collect::<Vec<_>>();
        assert_eq!(found(r"\N[\V[7]]です\C[2]赤\{\G%1"), [r"\N[\V[7]]", r"\C[2]", r"\{", r"\G", "%1"]);
        assert_eq!(found(r"\}\}FEVER"), [r"\}", r"\}"]);
    }

    #[test]
    fn a_message_keeps_its_commands_however_long_the_english_is() {
        let mut list = json!([cmd(101, json!(["", 0, 0, 2, r"\N[1]"])), cmd(401, json!(["あい"])), cmd(401, json!(["うえ"])), cmd(0, json!([]))]);
        let actors = ["".to_string(), "蜜宵".to_string()];
        let mut seen = Vec::new();
        events(Some(&mut list), 12, &actors, &mut |_, sp, t| {
            seen.push((sp.to_string(), t.to_string()));
            Some("one two three four five six seven".into())
        });
        assert_eq!(seen, [("蜜宵".to_string(), "あいうえ".to_string())]);
        assert_eq!(list.as_array().unwrap().len(), 4);
        assert_eq!(list[1]["parameters"][0], "one two");
        assert_eq!(list[2]["parameters"][0], "three four\nfive six\nseven");
    }

    #[test]
    fn short_english_blanks_the_spare_commands_and_unknown_text_stays() {
        let mut list = json!([cmd(101, json!(["", 0, 0, 2, ""])), cmd(401, json!(["あ"])), cmd(401, json!(["い"])),
                              cmd(101, json!(["", 0, 0, 2, ""])), cmd(401, json!(["う"]))]);
        events(Some(&mut list), 40, &[], &mut swap(&[("あい", "Hi")]));
        assert_eq!((&list[1]["parameters"][0], &list[2]["parameters"][0], &list[4]["parameters"][0]), (&json!("Hi"), &json!(""), &json!("う")));
    }

    #[test]
    fn hidden_text_is_left_alone_and_shown_text_is_not() {
        let mut list = json!([cmd(102, json!([["はい", "いいえ"], 1, 0, 2, 0])), cmd(402, json!([0, "はい"])), cmd(108, json!(["はい"])),
                              cmd(118, json!(["はい"])), cmd(355, json!(["はい"])), cmd(357, json!(["SimpleVoice", "PLAY_VOICE", "はい", {"text": "はい"}]))]);
        let mut n = 0;
        events(Some(&mut list), 40, &[], &mut |_, _, _| {
            n += 1;
            Some("Yes".into())
        });
        assert_eq!(n, 2); // the two choices
        assert_eq!(list[0]["parameters"][0], json!(["Yes", "Yes"]));
        assert_eq!(list[1]["parameters"][1], "はい");
    }

    #[test]
    fn mv_messages_have_no_speaker_and_null_rows_stay_null() {
        let mut list = json!([cmd(101, json!(["", 0, 0, 2])), cmd(401, json!(["あ"]))]);
        events(Some(&mut list), 40, &[], &mut swap(&[("あ", "Ah")]));
        assert_eq!(list[1]["parameters"][0], "Ah");
        let mut rows = json!([null, {"name": "薬"}]);
        walk("Items.json", &mut rows, 40, 40, &[], &mut swap(&[("薬", "Potion")]));
        assert_eq!(rows, json!([null, {"name": "Potion"}])); // no fields added either
    }

    #[test]
    fn picture_text_goes_by_paragraph_at_its_original_width() {
        let mut v = json!("いろはにほへとちりぬるをわかよたれそつねならむ\nうい\n\nあ");
        let long = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut f = |_: &str, _: &str, t: &str| -> Option<String> { Some(if t == "あ" { "Ah".into() } else { long.into() }) };
        picture_text(Some(&mut v), &mut f);
        assert_eq!(v, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\nAh");
        let mut v = json!("あい\nう");
        picture_text(Some(&mut v), &mut swap(&[]));
        assert_eq!(v, "あい\nう"); // untranslated: its own line breaks
    }

    #[test]
    fn plugin_settings_change_only_listed_keys_even_inside_json_strings() {
        let inner = json!({"symbol": "アイテム", "memo": "アイテム"}).to_string();
        let mut v = json!({"menu": json!([inner]).to_string(), "other": "アイテム", "broken": "[アイテム"});
        plugin_setting(&mut v, "", &["symbol", "broken"], &mut swap(&[("アイテム", "Items")]));
        let menu: Value = serde_json::from_str(v["menu"].as_str().unwrap()).unwrap();
        let row: Value = serde_json::from_str(menu[0].as_str().unwrap()).unwrap();
        assert_eq!(row, json!({"symbol": "Items", "memo": "アイテム"}));
        assert_eq!((&v["other"], &v["broken"]), (&json!("アイテム"), &json!("[アイテム")));
        let mut v = json!({"after": "を得た！", "alone": "得た"});
        plugin_setting(&mut v, "", &["after", "alone"], &mut swap(&[("を得た！", "obtained!"), ("得た", "Got")]));
        assert_eq!(v, json!({"after": " obtained!", "alone": "Got"}));
        let mut v = json!({"help": "[決定]で保存"});
        plugin_setting(&mut v, "", &["help"], &mut swap(&[("[決定]で保存", "[OK] saves")]));
        assert_eq!(v, json!({"help": "[OK] saves"})); // looks like JSON, but is a listed text
    }

    #[test]
    fn battle_messages_that_go_on_from_a_name_get_a_space() {
        let mut rows = json!([{"message1": "は毒にかかった！", "message2": "の攻撃！", "message3": "は眠った…", "message4": "毒だ"}]);
        let pairs = &[("は毒にかかった！", "was poisoned!"), ("の攻撃！", "'s attack!"), ("は眠った…", "fell asleep…"), ("毒だ", "Poison")];
        walk("States.json", &mut rows, 40, 40, &[], &mut swap(pairs));
        assert_eq!(rows[0], json!({"message1": " was poisoned!", "message2": "'s attack!", "message3": " fell asleep…", "message4": "Poison"}));
    }

    #[test]
    fn nothing_a_model_writes_can_break_a_plugin_string() {
        let drawn = |code: &str, tr: &str| plugin_code(code, &mut |_: &str, _: &str, _: &str| Some(tr.to_string()));
        let single = |tr: &str| drawn("drawText('あ');\n", tr);
        assert_eq!(single("a\nb"), "drawText('a\\nb');\n");
        assert_eq!(single("a\rb"), "drawText('a\\nb');\n");
        assert_eq!(single("a\u{2028}b"), "drawText('a\\nb');\n");
        assert_eq!(single("it's"), "drawText('it\\'s');\n");
        assert_eq!(single(r"it\'s"), "drawText('it\\'s');\n"); // already escaped, and not twice
        assert_eq!(single(r"a\\'"), r"drawText('a\\\'');".to_string() + "\n");
        assert_eq!(single(r"a\"), r"drawText('a\\');".to_string() + "\n"); // would eat the closing quote
        assert_eq!(single(r"\u12 \x4 \1"), r"drawText('\\u12 \\x4 \\1');".to_string() + "\n"); // refused by JS
        assert_eq!(single(r"\\C[1]\u3042"), r"drawText('\\C[1]\u3042');".to_string() + "\n"); // the plugin's own
        assert_eq!(drawn("drawText(`あ`);\n", "a\nb"), "drawText(`a\\nb`);\n");
        assert_eq!(drawn("drawText(\"あ\");\n", "a`b"), "drawText(\"a\\`b\");\n");
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_plugin_string() {
        let mut seen = Vec::new();
        plugin_code("drawText(\"あ\\\"い\");\n", &mut |_, _, t| {
            seen.push(t.to_string());
            None
        });
        assert_eq!(seen, ["あ\\\"い"]);
    }

    #[test]
    fn plugin_code_changes_drawn_strings_and_nothing_that_is_looked_up() {
        let code = concat!(
            "/*: @help\n return \"あ\";\n*/\n",
            "this.drawText(\"あ\", 0, 0);\n",
            "this._help.setText(n ? `い${n}` : \"あ\");\n",
            "const label = `${e || \"あ\"}　あ`;\n",
            "const ITEM_NAME_ON = \"あ\";\n",
            "return `${v} あ`;\n",
            "return meta(x, [\"あ\", \"b\"]);\n",
            "if (a === \"あ\") this.drawText(\"あ\");\n",
            "item.meta[\"あ\"];\n",
            "const pattern = \"あ\";\n",
            "$gameMessage.add(\n`あ`\n);\n",
            "// this.drawText(\"あ\");\n",
            "const keke_TextColor = Number(parameters[\"あ\"]);\n",
            "const message = find(['あ', 'b']) || x;\n",
            "const HEAL_NAME = String(p.x || \"あ\");\n",
            "const text = `${e}あ`;\n",
        );
        let mut f = |_: &str, _: &str, t: &str| -> Option<String> { Some(t.replace('あ', "it's").replace('い', "a`b")) };
        let out = plugin_code(code, &mut f);
        let want = concat!(
            "/*: @help\n return \"あ\";\n*/\n",
            "this.drawText(\"it's\", 0, 0);\n",
            "this._help.setText(n ? `a\\`b${n}` : \"it's\");\n",
            "const label = `${e || \"it's\"}　it's`;\n",
            "const ITEM_NAME_ON = \"it's\";\n",
            "return `${v} it's`;\n",
            "return meta(x, [\"あ\", \"b\"]);\n",
            "if (a === \"あ\") this.drawText(\"あ\");\n",
            "item.meta[\"あ\"];\n",
            "const pattern = \"あ\";\n",
            "$gameMessage.add(\n`it's`\n);\n",
            "// this.drawText(\"あ\");\n",
            "const keke_TextColor = Number(parameters[\"あ\"]);\n",
            "const message = find(['あ', 'b']) || x;\n",
            "const HEAL_NAME = String(p.x || \"あ\");\n",
            "const text = `${e} it's`;\n",
        );
        assert_eq!(out, want);
    }

    #[test]
    fn glossary_notes_translate_their_text_and_keep_other_tags() {
        let mut v = json!("<SG説明:あ\nい>\n<SGカテゴリ:薬>\n<HPバーX:5>\n<SG種別:2>");
        note(Some(&mut v), &mut swap(&[("あい", "Text"), ("薬", "Drugs")]));
        assert_eq!(v, "<SG説明:Text>\n<SGカテゴリ:Drugs>\n<HPバーX:5>\n<SG種別:2>");
        let mut v = json!("<SG説明:あ><SGカテゴリ:薬>");
        note(Some(&mut v), &mut swap(&[("あ", "<Fire> -> 20%"), ("薬", "A<B")]));
        assert_eq!(v, "<SG説明:＜Fire＞ -＞ 20%><SGカテゴリ:A<B>"); // a category must keep matching CategoryOrder
    }

    #[test]
    fn encrypted_images_keep_their_header_and_only_16_bytes_change() {
        let key: Vec<u8> = (1..=16).collect();
        let png: Vec<u8> = (0..40).collect();
        let file = [b"RPGMV\0\0\0\0\x03\x01\0\0\0\0\0".to_vec(), unmask(&[vec![0; 16], png.clone()].concat(), &key).unwrap()].concat();
        assert_eq!(unmask(&file, &key).unwrap(), png);
        assert_eq!(file[32..], png[16..]);
        assert_eq!(unmask(&file, &[]), None);
        let game = std::env::temp_dir().join(format!("vntl_img_{}", std::process::id()));
        let path = game.join("a.png_");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(&path, &file).unwrap();
        assert_eq!(read_image(&path, &key).unwrap(), png);
        write_image(&path, &key, &[9; 30]).unwrap();
        let new = std::fs::read(&path).unwrap();
        assert_eq!((&new[..5], &new[32..]), (&b"RPGMV"[..], &[9u8; 14][..]));
        assert_eq!(unmask(&new, &key).unwrap(), [9; 30]);
        assert_eq!(std::fs::read(crate::backup(&path)).unwrap(), file); // the game's own copy is kept
        std::fs::remove_dir_all(&game).unwrap();
    }

    #[test]
    fn long_text_wraps_to_more_lines_than_a_help_window_has() {
        assert_eq!(wrap("aaaa bbbb cccc", 9).len(), 2);
        assert_eq!(wrap("aaaa bbbb cccc", 4).len(), 3);
    }

    #[test]
    fn line_width_ignores_codes_and_counts_names() {
        assert_eq!(units(r"\C[2]ab\C[0]"), 2);
        assert_eq!(units(r"\N[\V[7]]"), 8);
        assert_eq!(units("ab…"), 4);
        assert_eq!(units("[Head] DEF+5"), 12); // brackets are plain text here
        let system = json!({"advanced": {"uiAreaWidth": 1080, "fontSize": 26}});
        assert_eq!(window_units(&system, 0), 80);
        assert_eq!(window_units(&system, 80), 68); // the 912 px box of 蜜宵のファミリア
        assert_eq!(window_units(&json!({}), 0), 55);
    }

    #[test]
    fn the_box_narrowing_plugin_counts_only_when_it_is_on() {
        let dir = std::env::temp_dir().join(format!("vntl_rm_{}", std::process::id()));
        let js = dir.join("js");
        std::fs::create_dir_all(&js).unwrap();
        let line = |on: bool| format!(r#"{{"name":"LL_MessageWindowAdjust","status":{on},"description":"","parameters":{{"adjustOnFace":"0","adjustNoFace":"80"}}}},"#);
        std::fs::write(js.join("plugins.js"), format!("var $plugins =\n[\n{}\n{{\"name\":\"Other\",\"status\":true}}\n];", line(true))).unwrap();
        assert_eq!(box_narrowed(&dir.join("data")), 80);
        std::fs::write(js.join("plugins.js"), line(false)).unwrap();
        assert_eq!(box_narrowed(&dir.join("data")), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn database_and_dat_round_trip() {
        let mut items = json!([null, {"name": "薬", "description": "よく\n効く", "note": "<薬>", "message1": ""}]);
        walk("Items.json", &mut items, 40, 40, &[], &mut swap(&[("薬", "Potion"), ("よく効く", "Works well")]));
        assert_eq!(items[1], json!({"name": "Potion", "description": "Works well", "note": "<薬>", "message1": ""}));
        let common = json!([null, {"id": 1, "list": []}]);
        assert_eq!(decode_dat(&encode_dat(&common, b"key"), b"key").unwrap(), common);
        assert!(decode_dat(&encode_dat(&common, b"key"), b"other").is_err());
        assert_eq!(parse("Map001.json", b"\xEF\xBB\xBF[1]").unwrap(), json!([1]));
    }
}
