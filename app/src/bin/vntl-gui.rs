//! The window: pick a visual novel folder, pick a local model, translate.
#![windows_subsystem = "windows"] // no console window behind the app

slint::include_modules!();

use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use vntl::llm::{fetch, is_own, is_vntl, list_models, own_installed, Chat, Offer, Pair, Server, ALL_IN_ONE, IMAGE_MODEL, IMAGE_OFFERS, READING_MODEL, TEXT_OFFERS, VN_MODEL};
use vntl::pipeline::{cache_file, import_game_data, links, load_characters, load_descriptions, load_synopsis, save_characters, save_description, save_synopsis, set_unlinked, translate, Character, Event, Options};
use vntl::renpy::{detect, game_root, tl_dir, SUPPORTED};
use vntl::usage::Sampler;
use vntl::R;

/// The model writes English, so that's the slot in the game too. The command line can pick another.
const LANGUAGE: &str = "english";

/// The newest model list request, so a slower older reply can't overwrite its answer.
static LAST_REFRESH: AtomicUsize = AtomicUsize::new(0);
/// The Characters rows were changed in the window and characters.txt doesn't have that yet.
static TYPED: AtomicBool = AtomicBool::new(false);

/// The clock as hh:mm:ss for the log: the standard library can't tell the time zone.
#[cfg(windows)]
fn clock() -> String {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLocalTime(time: *mut [u16; 8]);
    }
    let mut t = [0u16; 8]; // year, month, weekday, day, hour, minute, second, millisecond
    // SAFETY: SYSTEMTIME is these eight u16, and the call only fills them in
    unsafe { GetLocalTime(&mut t) };
    format!("{:02}:{:02}:{:02}", t[4], t[5], t[6])
}

#[cfg(not(windows))]
fn clock() -> String {
    "--:--:--".into() // never empty: the window takes an empty time for the next row of a post
}

/// `text` in pieces no wider than `columns`, broken at spaces; Japanese has none and breaks anywhere.
fn folded(text: &str, columns: usize) -> Vec<String> {
    let (mut pieces, mut piece, mut used) = (Vec::new(), String::new(), 0);
    for c in text.chars() {
        let width = c.width().unwrap_or(0);
        if used + width > columns && !piece.is_empty() {
            let keep = piece.rfind(' ').filter(|&at| at > 0).unwrap_or(piece.len());
            let rest = piece[keep..].trim_start().to_string();
            piece.truncate(keep);
            pieces.push(std::mem::replace(&mut piece, rest));
            used = piece.width();
        }
        if c != ' ' || !piece.is_empty() || pieces.is_empty() {
            piece.push(c);
            used += width;
        }
    }
    pieces.push(piece);
    pieces
}

/// One numbered post: rows of (text, kind), folded to the log's width, with the time on the first.
fn post(ui: &AppWindow, lines: &[(&str, i32)]) {
    let model = ui.get_rows();
    let rows = model.as_any().downcast_ref::<VecModel<LogRow>>().expect("main sets a VecModel");
    let number = rows.row_data(rows.row_count().wrapping_sub(1)).map_or(5, |last| last.number + 1); // >>1 to >>4 are the tabs
    let columns = ui.get_log_columns().max(20) as usize;
    let mut time = SharedString::from(clock());
    for (text, kind) in lines {
        for piece in folded(&text.replace(['\r', '\n'], " "), columns) {
            ui.set_widest(ui.get_widest().max(piece.width() as i32));
            rows.push(LogRow { time: std::mem::take(&mut time), number, text: piece.into(), kind: *kind });
        }
    }
}

fn append_log(ui: &AppWindow, text: &str) {
    text.lines().for_each(|line| post(ui, &[(line, 0)]));
}

/// The log without its translated lines, numbered again.
fn without_lines(rows: impl Iterator<Item = LogRow>) -> Vec<LogRow> {
    let mut number = 4;
    rows.filter(|r| r.kind == 0)
        .map(|r| {
            number += !r.time.is_empty() as i32; // a post folded onto more rows keeps one number
            LogRow { number, ..r }
        })
        .collect()
}

/// Turning the translated lines off takes them off the screen too, for when someone walks in.
fn hide_lines(ui: &AppWindow) {
    let model = ui.get_rows();
    let rows = model.as_any().downcast_ref::<VecModel<LogRow>>().expect("main sets a VecModel");
    rows.set_vec(without_lines(rows.iter()));
}

fn time_left(took: Duration, lines: usize, remaining: usize) -> String {
    if lines == 0 || remaining == 0 {
        return String::new();
    }
    let minutes = (took.as_secs_f64() * remaining as f64 / lines as f64 / 60.0).ceil() as u64;
    match minutes {
        0 | 1 => "about a minute left".into(),
        2..60 => format!("about {minutes} min left"),
        _ => {
            let fives = minutes.div_ceil(5) * 5; // an hour off doesn't need counting in single minutes
            format!("about {} h {} min left", fives / 60, fives % 60)
        }
    }
}

/// Time left at the pace of the latest steps, so a change of model or memory shows up soon.
fn paced(steps: &mut VecDeque<(Instant, usize)>, done: usize, total: usize) -> String {
    const STEPS: usize = 15; // of 20 lines each: the last few minutes
    if steps.len() == STEPS {
        steps.pop_front();
    }
    steps.push_back((Instant::now(), done));
    let (since, from) = steps[0];
    time_left(since.elapsed(), done.saturating_sub(from), total.saturating_sub(done))
}

/// Windows' "Animation effects" switch, which the moving parts of the window obey.
#[cfg(windows)]
fn animations_off() -> bool {
    const SPI_GETCLIENTAREAANIMATION: u32 = 0x1042;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SystemParametersInfoW(action: u32, param: u32, value: *mut i32, ini: u32) -> i32;
    }
    let mut on = 1;
    // SAFETY: this action writes one BOOL through the pointer and reads nothing else
    unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, &mut on, 0) != 0 && on == 0 }
}

#[cfg(not(windows))]
fn animations_off() -> bool {
    false
}

fn clear_progress(ui: &AppWindow) {
    ui.set_progress(0.0);
    ui.set_percent("".into());
}

/// What each engine reads. The window shows only these, and a value left in a hidden one doesn't reach the run.
fn offers(engine: &str) -> EngineOptions {
    let (renpy, unity, liarsoft, rpgmaker) = (engine == "renpy", engine == "unity", engine == "liarsoft", engine == "rpgmaker-mv/mz");
    EngineOptions {
        reference: renpy,
        scene: renpy || rpgmaker,
        scene_is_file: rpgmaker,
        images: renpy || liarsoft || rpgmaker || ["artemis", "bgi", "siglusengine"].contains(&engine),
        art: renpy || unity,
        retranslate: unity,
        redump: renpy,
        fit: vntl::fit::MEASURED.contains(&engine),
    }
}

/// `shown` is the folder the Characters list came from.
fn detect_game(ui: &AppWindow, shown: &RefCell<PathBuf>) {
    // Explorer's "Copy as path" adds quotes, and no Windows path can hold one
    if ui.get_game().contains('"') {
        ui.set_game(ui.get_game().replace('"', "").into());
    }
    let game = ui.get_game().trim().to_string();
    let engine = (!game.is_empty() && Path::new(&game).is_dir()).then(|| detect(Path::new(&game)));
    let ok = engine.is_some_and(|e| SUPPORTED.contains(&e));
    let (text, state) = match engine {
        None if game.is_empty() => ("No folder selected".to_string(), 0),
        None if Path::new(&game).is_file() => ("That's a file  ·  pick the folder that contains the .exe".to_string(), 2),
        None => ("Not a folder  ·  check the path".to_string(), 2),
        // the box isn't rewritten here: a typed path would jump back with every Backspace
        Some("unknown") => match game_root(Path::new(&game)).strip_prefix(&game) {
            Ok(inner) if !inner.as_os_str().is_empty() => (format!("The game is in a subfolder  ·  {}", inner.display()), 2),
            _ => ("No visual novel engine found in this folder".to_string(), 2),
        },
        Some(e) if ok => (format!("{e}  ·  ready"), 1),
        Some(e) => (format!("{e}  ·  not supported yet"), 2),
    };
    ui.set_engine_text(text.into());
    ui.set_engine_state(state);
    ui.set_offers(offers(engine.unwrap_or("")));
    ui.set_can_translate(ok);
    let status = ui.get_status_text();
    if ok && !ui.get_running() {
        ui.set_status_text("Ready to translate.".into());
    } else if !ok && (status == "Ready to translate." || status.starts_with("Done")) {
        // only these spoke for the folder that was here: a download or a failure still has to be read
        ui.set_status_text("Pick a game folder to start.".into());
        clear_progress(ui);
    }
    // reload for another folder only, but do that during a run too, or Translate would save the
    // old game's rows into this one
    if ok && shown.borrow().as_path() != Path::new(&game) {
        save_typed(ui, &shown.borrow());
        shown.replace(PathBuf::from(&game));
        // still unsaved here means no game was open when they were typed: they go to this one
        let typed = if TYPED.load(Ordering::Relaxed) { characters(ui) } else { Vec::new() };
        show_characters(ui, merged(typed, load_characters(Path::new(&game))));
        show_links(ui, Path::new(&game));
        show_game_data(ui, Path::new(&game));
        ui.set_vndb_query(Path::new(&game).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned()).into());
        if !ui.get_running() {
            clear_progress(ui);
        }
    }
}

/// Rows changed in the window exist only there, so write them before they are replaced or it closes.
fn save_typed(ui: &AppWindow, game: &Path) {
    if !game.as_os_str().is_empty()
        && TYPED.swap(false, Ordering::Relaxed)
        && let Err(e) = save_characters(game, &characters(ui))
    {
        append_log(ui, &format!("Couldn't save the character list: {e}"));
    }
}

/// `rows`, then the characters in `file` they don't list yet.
fn merged(mut rows: Vec<Character>, file: Vec<Character>) -> Vec<Character> {
    for c in file {
        if !rows.iter().any(|r| row_name(r) == row_name(&c)) {
            rows.push(c);
        }
    }
    rows
}

fn row_name(c: &Character) -> &str {
    Some(c.original.trim()).filter(|n| !n.is_empty()).unwrap_or(c.english.trim())
}

/// Genders a run worked out, for the rows that have none.
fn with_found_genders(mut rows: Vec<Character>, file: &[Character]) -> Vec<Character> {
    for row in rows.iter_mut().filter(|r| r.gender.is_empty()) {
        if let Some(c) = file.iter().find(|c| row_name(c) == row_name(row)) {
            row.gender = c.gender.clone();
        }
    }
    rows
}

/// The Characters list as the library's notes ("Not set" is no gender).
fn characters(ui: &AppWindow) -> Vec<Character> {
    let gender = |g: SharedString| if g == "Not set" { String::new() } else { g.to_string() };
    ui.get_characters()
        .iter()
        .map(|r| Character { original: r.original.into(), english: r.english.into(), gender: gender(r.gender), extra: r.extra.into() })
        .collect()
}

fn show_characters(ui: &AppWindow, list: Vec<Character>) {
    let rows: Vec<CharacterRow> = list
        .into_iter()
        .map(|c| CharacterRow { original: c.original.into(), english: c.english.into(), gender: c.gender.into(), extra: c.extra.into() })
        .collect();
    ui.set_characters(ModelRc::new(VecModel::from(rows)));
}

/// The names the game's script links, for the Characters tab.
fn show_links(ui: &AppWindow, game: &Path) {
    let rows: Vec<LinkRow> = links(game).into_iter().map(|l| LinkRow { original: l.original.into(), name: l.name.into(), main: l.main.into(), broken: l.broken }).collect();
    ui.set_links(ModelRc::new(VecModel::from(rows)));
}

/// How the Game Data tab names a character.
fn label(c: &Character) -> &str {
    if c.english.trim().is_empty() { &c.original } else { &c.english }
}

/// The synopsis, and the description of the character picked, or of the first that has one.
fn show_game_data(ui: &AppWindow, game: &Path) {
    ui.set_synopsis(load_synopsis(game).into());
    let about = load_descriptions(game);
    let list = load_characters(game);
    let described: Vec<&Character> = list.iter().filter(|c| about.contains_key(&c.original)).collect();
    let picked = ui.get_about_name();
    let shown = described.iter().find(|c| label(c) == picked.as_str()).or(described.first());
    let names: Vec<SharedString> = described.iter().map(|c| label(c).into()).collect();
    ui.set_about_names(ModelRc::new(VecModel::from(names)));
    ui.set_about_name(shown.map_or("", |c| label(c)).into());
    ui.set_about_text(shown.and_then(|c| about.get(&c.original)).map_or("", String::as_str).into());
}

/// The folder in the Game box, if it is one: saving for a half-typed path would create that path.
fn game_folder(ui: &AppWindow) -> Option<PathBuf> {
    std::path::absolute(ui.get_game().trim()).ok().filter(|p| p.is_dir())
}

/// Fills the game's data in from VNDB, off the window's thread. Rows typed so far go to the file
/// first, as that's what the import adds to.
fn import_vndb(ui: &AppWindow) {
    let Some(game) = game_folder(ui) else {
        ui.set_status_text("Pick a game folder first.".into());
        return;
    };
    save_typed(ui, &game);
    let query = ui.get_vndb_query().trim().to_string();
    let weak = ui.as_weak();
    ui.set_importing(true);
    ui.set_status_text("Asking VNDB…".into());
    std::thread::spawn(move || {
        let result = vntl::vndb::fetch(&query).and_then(|found| import_game_data(&game, &found));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_importing(false);
            match result {
                Ok(said) => {
                    show_characters(&ui, load_characters(&game));
                    show_links(&ui, &game);
                    show_game_data(&ui, &game);
                    append_log(&ui, &said);
                    ui.set_status_text("Imported from VNDB  ·  check it on the Game Data tab.".into());
                }
                Err(e) => {
                    append_log(&ui, &e);
                    ui.set_status_text("VNDB import failed  ·  see the log below.".into());
                }
            }
        });
    });
}

fn settings_path() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("VN Translator").join("settings.json")
}

fn load_settings(ui: &AppWindow) {
    let Ok(text) = fs::read_to_string(settings_path()) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let text = |key: &str| v[key].as_str().map(SharedString::from);
    v["check_updates"].as_bool().into_iter().for_each(|on| ui.set_check_updates(on));
    let setters: [(&str, &dyn Fn(SharedString)); 6] = [
        ("url", &|s| ui.set_url(s)),
        ("model", &|s| ui.set_model(s)),
        ("vision_model", &|s| ui.set_vision_model(s)),
        ("reading_model", &|s| ui.set_reading_model(s)),
        ("game", &|s| ui.set_game(s)),
        ("reference", &|s| ui.set_reference(s)),
    ];
    setters.iter().for_each(|(key, set)| text(key).into_iter().for_each(set));
    v["images"].as_bool().into_iter().for_each(|on| ui.set_images(on));
    v["history"].as_i64().into_iter().for_each(|n| ui.set_history(n.clamp(6, 24) as i32));
    v["show_lines"].as_bool().into_iter().for_each(|on| ui.set_show_lines(on));
}

fn save_settings(ui: &AppWindow) {
    let path = settings_path();
    let v = serde_json::json!({
        "url": ui.get_url().as_str(), "model": ui.get_model().as_str(), "vision_model": ui.get_vision_model().as_str(),
        "reading_model": ui.get_reading_model().as_str(), "game": ui.get_game().as_str(), "reference": ui.get_reference().as_str(), "images": ui.get_images(), "history": ui.get_history(), "show_lines": ui.get_show_lines(),
        "check_updates": ui.get_check_updates(),
    });
    // losing the remembered settings is harmless, so a failure here isn't worth interrupting anyone
    let _ = path.parent().map(fs::create_dir_all);
    let _ = fs::write(&path, v.to_string());
}

fn refresh_models(ui: &AppWindow) {
    // read the choices now: until a list arrives, the boxes report an empty selection
    let (model, vision, reading) = (ui.get_model(), ui.get_vision_model(), ui.get_reading_model());
    let (url, weak) = (ui.get_url().to_string(), ui.as_weak());
    let id = LAST_REFRESH.fetch_add(1, Ordering::Relaxed) + 1;
    std::thread::spawn(move || {
        let result = list_models(&url);
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            _ if LAST_REFRESH.load(Ordering::Relaxed) != id => {}
            Ok(ids) => {
                let plural = if ids.len() == 1 { "" } else { "s" };
                ui.set_server_text(format!("Connected  ·  {} model{plural} available", ids.len()).into());
                ui.set_server_state(1);
                show_offers(&ui, &ids);
                // a new list resets both boxes to its first entry, so choose after setting it
                let listed: Vec<SharedString> = ids.iter().map(|i| i.as_str().into()).collect();
                ui.set_models(ModelRc::new(VecModel::from(listed)));
                let known = |name: &str| ids.iter().any(|i| i == name);
                ui.set_model(if known(&model) { model } else { ids.first().map_or("", |i| i.as_str()).into() });
                // menu images need a model that can see; VNTL models only read text
                let seeing = ids.iter().find(|i| !is_vntl(i)).map_or("", |i| i.as_str());
                ui.set_vision_model(if known(&vision) && !is_vntl(&vision) { vision } else { seeing.into() });
                let seeing: Vec<SharedString> = std::iter::once(ui.get_same_as_image()).chain(ids.iter().filter(|i| !is_vntl(i)).map(|i| i.as_str().into())).collect();
                ui.set_reading_choices(ModelRc::new(VecModel::from(seeing)));
                ui.set_reading_model(if known(&reading) && !is_vntl(&reading) { reading } else { SharedString::new() });
            }
            Err(_) => {
                ui.set_server_text(format!("Can't reach {url}").into());
                ui.set_server_state(3);
            }
        });
    });
}

/// The Model Download rows, given the models the server has. A row carries the name the server uses,
/// which is the one the model boxes hold.
fn offer_rows(offers: &[Offer], ids: &[String]) -> ModelRc<OfferRow> {
    let rows: Vec<OfferRow> = offers
        .iter()
        .map(|o| {
            let have = ids.iter().find(|i| i.eq_ignore_ascii_case(o.model));
            let always = is_own(o.model);
            OfferRow {
                title: o.title.into(),
                model: have.map_or(o.model, |i| i.as_str()).into(),
                size: o.size.into(),
                gpu: o.gpu.into(),
                note: o.note.into(),
                installed: have.is_some() || (always && own_installed(o.model)),
                always,
            }
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

fn show_offers(ui: &AppWindow, ids: &[String]) {
    ui.set_text_offers(offer_rows(&TEXT_OFFERS, ids));
    ui.set_image_offers(offer_rows(&IMAGE_OFFERS, ids));
    let have = |m: &str| if is_own(m) { own_installed(m) } else { ids.iter().any(|i| i.eq_ignore_ascii_case(m)) };
    ui.set_all_offer(OfferRow {
        title: "All in one".into(),
        model: ALL_IN_ONE_ROW.into(),
        size: "20.3 GB".into(),
        gpu: "GPU 12 GB".into(),
        note: "VNTL Llama 8B for the story, Gemma 3 12B and Qwen 2.5 VL 7B for menus, and the text reader, fonts and inpainting. Gets what's missing.".into(),
        installed: ALL_IN_ONE.iter().all(|m| have(m)),
        always: false,
    });
}

/// What the All in one row's Get asks for.
const ALL_IN_ONE_ROW: &str = "all-in-one";

fn pick_folder(set: impl Fn(SharedString)) {
    if let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder").pick_folder() {
        set(dir.to_string_lossy().as_ref().into());
    }
}

/// A panic in the worker would skip the code that ends the run, leaving the window on "Working…".
fn catch_panic(run: impl FnOnce() -> R<()>) -> R<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|p| {
        let why = p.downcast_ref::<String>().map(String::as_str).or(p.downcast_ref::<&str>().copied()).unwrap_or("unknown");
        Err(format!("Internal error (a bug in the app, not your model): {why}"))
    })
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// One reading for the Model tab, off the window's thread: it asks the server and starts nvidia-smi.
fn read_usage(ui: &AppWindow, sampler: Arc<Mutex<Sampler>>) {
    let (url, weak) = (ui.get_url().to_string(), ui.as_weak());
    std::thread::spawn(move || {
        let Ok(mut sampler) = sampler.try_lock() else { return }; // the last reading is still going
        let rows = vntl::usage::rows(&sampler.read(&url));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let rows: Vec<UsageRow> = rows.into_iter().map(|r| UsageRow { label: r.label.into(), value: r.value.into(), share: r.share.unwrap_or(-1.0), state: r.state }).collect();
            ui.set_usage(ModelRc::new(VecModel::from(rows)));
        });
    });
}

/// The image model, reading pictures with the menu reading model when there is one. Either alone
/// does both. At no temperature, a label read the same gets the same English every run.
fn vision_chat(url: &str, vision: &str, reading: &str) -> Option<Box<dyn Chat + Send>> {
    let server = |m: &str| Server::new(url, m, 0.0, 600);
    match (vision, reading) {
        ("", "") => None,
        (v, "") => Some(Box::new(server(v))),
        ("", r) => Some(Box::new(server(r))),
        (v, r) if v == r => Some(Box::new(server(v))),
        (v, r) => Some(Box::new(Pair { reads: server(r), words: server(v) })),
    }
}

/// Asks for a screenshot of a long line, and measures the box from it.
fn fit_box(ui: &AppWindow) {
    if ui.get_vision_model().is_empty() && !vntl::ocr::installed() {
        ui.set_status_text("Fitting text needs the text reader or an image model, from Model Download.".into());
        return;
    }
    let dialog = rfd::FileDialog::new().set_title("Choose a screenshot of a long message").add_filter("Screenshot", &["png", "jpg", "jpeg"]);
    let Some(image) = dialog.pick_file() else { return };
    let game = std::path::absolute(ui.get_game().trim()).unwrap_or_default();
    // a screenshot is read by the menu reading model when there is one
    let reading = Some(ui.get_reading_model()).filter(|r| !r.is_empty()).unwrap_or(ui.get_vision_model());
    let (url, model, weak) = (ui.get_url().to_string(), reading.to_string(), ui.as_weak());
    ui.set_status_text("Reading the screenshot…".into());
    std::thread::spawn(move || {
        let said = std::fs::read(&image).map_err(|e| format!("{}: {e}", image.display()))
            .and_then(|bytes| {
                let server = Server::new(&url, &model, 0.0, 600);
                vntl::fit::calibrate(&game, (!model.is_empty()).then_some(&server as &dyn Chat), &bytes)
            })
            .unwrap_or_else(|e| e);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            append_log(&ui, &said);
            ui.set_status_text(said.into());
        });
    });
}

fn start(ui: &AppWindow, cancel: &Arc<AtomicBool>, memory: &Arc<AtomicUsize>) {
    let optional = |s: SharedString| Some(s.trim().trim_matches('"').to_string()).filter(|t| !t.is_empty());
    memory.store(ui.get_history().clamp(6, 24) as usize, Ordering::Relaxed);
    let game = std::path::absolute(ui.get_game().trim()).unwrap_or_default();
    let engine = detect(&game);
    let offered = offers(engine);
    let o = Options {
        game,
        lang: LANGUAGE.to_string(),
        label: optional(ui.get_label()).filter(|_| offered.scene),
        file: None,
        reference: optional(ui.get_reference()).filter(|_| offered.reference).map(PathBuf::from),
        ui: false,
        redump: ui.get_redump() && offered.redump,
        batch: 20,
        model: ui.get_model().to_string(),
        retranslate: ui.get_retranslate() && offered.retranslate,
        fresh: ui.get_fresh(),
        vision: vision_chat(ui.get_url().as_str(), ui.get_vision_model().as_str(), ui.get_reading_model().as_str()),
        images: ui.get_images() && offered.images,
        art: ui.get_art() && offered.art,
        history: memory.clone(),
    };
    // these two hold for one run, but only one that got to them: see the end of the run
    let (cache, dump) = (cache_file(&o.game, &o.lang), o.game.join("vntl_work").join("dump.json"));
    let (cache_was, dump_was) = (modified(&cache), modified(&dump));
    // rows nobody changed are older than the file: a hand edit in it must survive this save
    let typed = TYPED.load(Ordering::Relaxed);
    let list = if typed { characters(ui) } else { load_characters(&o.game) };
    match save_characters(&o.game, &list) {
        Ok(()) if typed => TYPED.store(false, Ordering::Relaxed),
        Ok(()) => show_characters(ui, list),
        Err(e) => append_log(ui, &format!("Couldn't save the character list: {e}")),
    }
    let (url, model) = (ui.get_url().to_string(), ui.get_model().to_string());
    save_settings(ui);
    refresh_models(ui); // a run reports nothing about the server, which may have started since the last check
    cancel.store(false, Ordering::Relaxed);
    ui.set_running(true);
    clear_progress(ui);
    ui.set_status_text("Working…".into());
    append_log(ui, &format!("> translate {}", o.game.display()));

    let (weak, cancel) = (ui.as_weak(), cancel.clone());
    std::thread::spawn(move || {
        let steps = RefCell::new(VecDeque::new());
        let report = |e: Event| {
            let left = match e {
                Event::Progress(done, total) => paced(&mut steps.borrow_mut(), done, total),
                _ => String::new(),
            };
            let _ = weak.upgrade_in_event_loop(move |ui| match e {
                Event::Log(s) => append_log(&ui, &s),
                Event::Line(original, english) if ui.get_show_lines() => post(&ui, &[(&original, 1), (&english, 2)]),
                Event::Line(..) => {}
                Event::Progress(done, total) => {
                    // the game is only written after the last line, so just the end of the run says 100%
                    ui.set_progress((done as f32 / total.max(1) as f32).min(0.99));
                    ui.set_percent(format!("{done} / {total}  ·  {}%", (done * 100 / total.max(1)).min(99)).into());
                    ui.set_time_left(left.into());
                    ui.set_status_text(if done < total {
                        format!("Translating with the model  ·  {done} / {total} lines").into()
                    } else {
                        "Finishing up…".into()
                    });
                }
            });
        };
        let result = catch_panic(|| translate(&o, &Server::new(&url, &model, 0.3, 600), &report, &cancel));
        let stopped = cancel.load(Ordering::Relaxed);
        let game = o.game.clone();
        // a run that failed before it reached the cache or the script still has that to do, and
        // neither step logs that it happened when there was nothing to replace, so ask the files
        let started_over = o.fresh && (result.is_ok() || modified(&cache) != cache_was);
        let read_again = o.redump && (result.is_ok() || modified(&dump) != dump_was);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_running(false);
            ui.set_time_left("".into());
            if started_over {
                ui.set_fresh(false);
            }
            if read_again {
                ui.set_redump(false);
            }
            // a run, stopped or not, adds the names it found to the file. Rows edited meanwhile stay
            let rows = characters(&ui);
            let file = load_characters(&game);
            let list = merged(with_found_genders(rows.clone(), &file), file);
            show_links(&ui, &game); // a run is what first reads a Ren'Py game's script
            if list != rows {
                show_characters(&ui, list); // only then: rebuilding takes the focus from a row
            }
            match result {
                Ok(()) => {
                    ui.set_progress(1.0);
                    ui.set_percent("100%".into());
                    ui.set_status_text("Done  ·  start the game to play it translated.".into());
                }
                Err(e) => {
                    append_log(&ui, &e);
                    ui.set_status_text(
                        if stopped { "Stopped  ·  Translate again to resume." } else { "Failed  ·  see the log below." }.into(),
                    );
                }
            }
        });
    });
}

/// Shows download progress on the bar, from another thread.
fn progress_bar(weak: slint::Weak<AppWindow>) -> impl Fn(u64, u64) {
    move |done: u64, total: u64| {
        let share = done as f32 / total.max(1) as f32;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_progress(share);
            ui.set_percent(format!("{}%", (share * 100.0) as u32).into());
        });
    }
}

/// Downloads a recommended model and puts it to use. The bar shows the part being fetched: a model
/// comes in several, and only one is big.
fn get_model(ui: &AppWindow, model: String) {
    let about = |m: &str| TEXT_OFFERS.iter().chain(&IMAGE_OFFERS).find(|o| o.model == m).map_or(("the model", ""), |o| (o.title, o.size));
    let all = model == ALL_IN_ONE_ROW;
    // the set gets only what's missing: an Ollama model already there would be checked over again
    let have: Vec<String> = ui.get_models().iter().map(|m| m.to_string()).collect();
    let missing = |m: &&&str| if is_own(m) { !own_installed(m) } else { !have.iter().any(|h| h.eq_ignore_ascii_case(m)) };
    let todo: Vec<String> = if all { ALL_IN_ONE.iter().filter(missing).map(|m| m.to_string()).collect() } else { vec![model.clone()] };
    let (url, weak) = (ui.get_url().to_string(), ui.as_weak());
    ui.set_fetching(true);
    clear_progress(ui);
    std::thread::spawn(move || {
        let shown = progress_bar(weak.clone());
        let mut result = Ok(());
        for (n, m) in todo.iter().enumerate() {
            let (title, size) = about(m);
            let said = if all { format!("Downloading {} of {}: {title} ({size})…", n + 1, todo.len()) } else { format!("Downloading {title} ({size})…") };
            let _ = weak.upgrade_in_event_loop(move |ui| ui.set_status_text(said.into()));
            result = fetch(&url, m, &shown);
            if result.is_err() {
                break;
            }
        }
        let title = if all { "All in one" } else { about(&model).0 };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_fetching(false);
            clear_progress(&ui);
            ui.set_status_text(if result.is_ok() { format!("Downloaded {title}.") } else { "Download failed  ·  see the log below.".to_string() }.into());
            match result {
                Ok(()) if all => {
                    ui.set_model(VN_MODEL.into());
                    ui.set_vision_model(IMAGE_MODEL.into());
                    ui.set_reading_model(READING_MODEL.into());
                    refresh_models(&ui);
                }
                Ok(()) => {
                    if TEXT_OFFERS.iter().any(|o| o.model == model) {
                        ui.set_model(model.into());
                    } else if model == READING_MODEL {
                        ui.set_reading_model(model.into());
                    } else if !is_own(&model) {
                        ui.set_vision_model(model.into());
                    }
                    refresh_models(&ui);
                }
                Err(e) => append_log(&ui, &e),
            }
        });
    });
}

/// The log's own lines, each post whole again with its time, and the game's lines left out.
fn log_lines(rows: impl Iterator<Item = LogRow>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for r in rows.filter(|r| r.kind == 0) {
        match lines.last_mut() {
            // a post folded onto more rows has its time on the first
            Some(last) if r.time.is_empty() => {
                last.push(' ');
                last.push_str(&r.text);
            }
            _ => lines.push(format!("{}  {}", r.time, r.text)),
        }
    }
    lines
}

/// The log as a text file for a bug report, the PC, models and engine on top. The game's own lines
/// stay out, and paths lose the user's name, since the file is meant to be posted.
fn save_log(ui: &AppWindow) {
    let dialog = rfd::FileDialog::new().set_title("Save the log for a bug report").set_file_name("kidoku-log.txt").add_filter("Text", &["txt"]);
    let Some(path) = dialog.save_file() else { return };
    let lines = log_lines(ui.get_rows().iter());
    let named = |m: SharedString| if m.is_empty() { "none".to_string() } else { m.to_string() };
    let picked = format!(
        "Model: {}\nImage model: {}\nMenu reading model: {}\nEngine: {}",
        named(ui.get_model()), named(ui.get_vision_model()), named(ui.get_reading_model()), ui.get_engine_text()
    );
    let url = ui.get_url().to_string();
    let weak = ui.as_weak();
    ui.set_status_text("Saving the log…".into());
    std::thread::spawn(move || {
        let text = format!("Kidoku {}\n{}\n{picked}\n\n{}\n", version_text(), vntl::usage::about_pc(&url), lines.join("\n"));
        let text = match std::env::var("USERPROFILE") {
            Ok(home) if !home.is_empty() => text.replace(&home, "%USERPROFILE%"),
            _ => text,
        };
        let said = match std::fs::write(&path, text) {
            Ok(()) => format!("Saved the log as {}. Attach it to an issue on GitHub.", path.display()),
            Err(e) => format!("Couldn't save the log: {e}"),
        };
        let _ = weak.upgrade_in_event_loop(move |ui| ui.set_status_text(said.into()));
    });
}

/// Forget what was read in the game's menu images and their English, after asking, so the next
/// Translate reads and translates them again.
fn forget_menus(ui: &AppWindow) {
    let Some(game) = game_folder(ui) else {
        ui.set_status_text("Pick a game folder first.".into());
        return;
    };
    let files = vntl::menus::menu_files(&game);
    if files.is_empty() {
        ui.set_status_text("The menu images haven't been read yet.".into());
        return;
    }
    let answer = rfd::MessageDialog::new()
        .set_title("Read menus again")
        .set_description(
            "Forget what was read in this game's menu images, and their English?\n\nThe next Translate reads and translates them \
             again. Corrections you made in image_labels.txt go too.",
        )
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();
    if answer != rfd::MessageDialogResult::Yes {
        return;
    }
    for file in &files {
        if let Err(e) = fs::remove_file(file) {
            append_log(ui, &format!("{}: {e}", file.display()));
            return;
        }
    }
    append_log(ui, "Forgot the menu images' readings and English: the next Translate reads them again.");
    ui.set_status_text("Menus will be read again.".into());
}

fn remove_backups(ui: &AppWindow, game: &Path) {
    let files = vntl::backups(game);
    if files.is_empty() {
        rfd::MessageDialog::new().set_title("Remove translation").set_description("There is no Kidoku translation in this game.").show();
        return;
    }
    let answer = rfd::MessageDialog::new()
        .set_title("Remove translation")
        .set_description(format!(
            "Put back the game's {} original file(s)?\n\nSaved progress in vntl_work is kept, so translating again is quick.",
            files.len()
        ))
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();
    if answer == rfd::MessageDialogResult::Yes {
        match vntl::restore(game) {
            Ok((n, kept)) => {
                append_log(ui, &format!("Restored {n} original file(s)."));
                if kept > 0 {
                    append_log(ui, &format!(
                        "{kept} file(s) changed after vntl wrote them, probably by a game update, so they stay as they are. \
                         Their older originals are kept as *.vntl-orig.old-N."
                    ));
                }
                ui.set_status_text("Translation removed.".into());
                clear_progress(ui);
            }
            Err(e) => append_log(ui, &e),
        }
    }
}

/// Delete `files`: how many went, and why the others didn't. The manifest goes last and only if
/// nothing failed, because a retry needs it to find the copied files that are left.
fn delete_files(files: &mut [PathBuf]) -> (usize, Vec<String>) {
    files.sort_by_key(|p| p.ends_with("vntl_files.txt"));
    let (mut removed, mut failed) = (0, Vec::new());
    for p in files.iter() {
        if p.ends_with("vntl_files.txt") && !failed.is_empty() {
            break;
        }
        match fs::remove_file(p) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => failed.push(format!("{}: {e}", p.display())),
        }
    }
    (removed, failed)
}

fn remove(ui: &AppWindow) {
    let game = PathBuf::from(ui.get_game().trim());
    if detect(&game) != "renpy" {
        return remove_backups(ui, &game);
    }
    let tl = tl_dir(&game, LANGUAGE);
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in [tl.clone(), game.join("game")] {
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if (dir == tl && name.starts_with("vntl_")) || name.starts_with("zz_vntl_dump.rpy") {
                files.push(e.path());
            }
        }
    }
    // images/fonts copied from an official translation
    let manifest = fs::read_to_string(tl.join("vntl_files.txt")).unwrap_or_default();
    files.extend(manifest.lines().filter(|l| !l.is_empty()).map(|rel| tl.join(rel)).filter(|p| p.is_file()));
    if files.is_empty() {
        rfd::MessageDialog::new().set_title("Remove translation").set_description("There is no Kidoku translation in this game.").show();
        return;
    }
    let answer = rfd::MessageDialog::new()
        .set_title("Remove translation")
        .set_description(format!(
            "Delete {} translation file(s) from\n{}?\n\nThe game goes back to its original text. Saved progress in \
             vntl_work is kept, so translating again is quick.",
            files.len(),
            tl.display()
        ))
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();
    if answer == rfd::MessageDialogResult::Yes {
        let (removed, failed) = delete_files(&mut files);
        append_log(ui, &format!("Removed {removed} file(s)."));
        failed.iter().for_each(|f| append_log(ui, f));
        if failed.is_empty() {
            clear_progress(ui);
        }
        ui.set_status_text(if failed.is_empty() {
            "Translation removed.".into()
        } else {
            format!("Could not remove {} file(s)  ·  see the log below.", failed.len()).into()
        });
    }
}

/// The window of the program named as its installer.
#[cfg(windows)]
fn run_setup() -> Result<(), slint::PlatformError> {
    use vntl::setup::{default_folder, install, installed_folder, ollama_found, open_page, start, Choices, OLLAMA_PAGE};
    let ui = SetupWindow::new()?;
    ui.set_folder(installed_folder().unwrap_or_else(default_folder).display().to_string().into());
    ui.set_server_found(ollama_found());
    let w = ui.as_weak();
    ui.on_browse(move || {
        let ui = w.unwrap();
        pick_folder(|d| ui.set_folder(d));
    });
    ui.on_get_server(|| open_page(OLLAMA_PAGE));
    let installed: Arc<Mutex<Option<PathBuf>>> = Arc::default();
    let (w, done) = (ui.as_weak(), installed.clone());
    ui.on_install(move || {
        let ui = w.unwrap();
        let choices = Choices { folder: PathBuf::from(ui.get_folder().as_str()), menu: true, desktop: ui.get_desktop(), register: true };
        ui.set_step(1);
        ui.set_status("Installing…".into());
        let (w, done) = (ui.as_weak(), done.clone());
        std::thread::spawn(move || {
            let result = std::env::current_exe().map_err(|e| e.to_string()).and_then(|from| install(&from, &choices));
            let _ = w.upgrade_in_event_loop(move |ui| match result {
                Ok(program) => {
                    *done.lock().unwrap() = Some(program);
                    ui.set_step(2);
                    ui.set_status("Installed. You'll find Kidoku in the Start menu.".into());
                }
                Err(e) => {
                    ui.set_step(3);
                    ui.set_status(e.into());
                }
            });
        });
    });
    let w = ui.as_weak();
    ui.on_finish(move || {
        if let (true, Some(program)) = (w.unwrap().get_launch(), installed.lock().unwrap().as_ref()) {
            let _ = start(program);
        }
        let _ = slint::quit_event_loop();
    });
    ui.run()
}

/// What `--uninstall` does, from the entry in Installed apps. `--quiet` asks nothing and keeps downloads.
#[cfg(windows)]
fn run_uninstall(quiet: bool) {
    let Ok(program) = std::env::current_exe() else { return };
    let ask = |text: &str| {
        rfd::MessageDialog::new().set_title("Remove Kidoku").set_description(text).set_buttons(rfd::MessageButtons::YesNo).show() == rfd::MessageDialogResult::Yes
    };
    if !quiet && !ask("Remove Kidoku from this PC? Games you translated keep their translation: use Remove translation in Kidoku first if you want them back in Japanese.") {
        return;
    }
    let downloads = !quiet && ask("Also delete the text reader and fonts Kidoku downloaded? Models in Ollama stay either way.");
    if let (false, Err(e)) = (quiet, vntl::setup::uninstall(&program, downloads)) {
        rfd::MessageDialog::new().set_title("Remove Kidoku").set_description(e).show();
    }
}

/// What the new Setup does when an installed copy hands over to it: `--update <folder>`.
/// `--no-start` leaves the new version closed and reports by exit code, for a script.
#[cfg(windows)]
fn run_update(folder: &str, quiet: bool) -> ! {
    let done = std::env::current_exe().map_err(|e| e.to_string()).and_then(|from| vntl::update::replace(&from, Path::new(folder)));
    match done {
        Ok(program) if !quiet => {
            let _ = vntl::setup::start(&program);
        }
        Ok(_) => {}
        Err(_) if quiet => std::process::exit(1),
        Err(e) => {
            let text = format!("The update didn't finish, and your translations are safe. You can run the new Setup from the releases page.\n\n{e}");
            rfd::MessageDialog::new().set_title("Update Kidoku").set_description(text).show();
        }
    }
    std::process::exit(0)
}

/// Asks GitHub what's newest, for the notice. A check nobody asked for keeps quiet when it fails.
#[cfg(windows)]
fn check_for_update(ui: &AppWindow, found: Arc<Mutex<Option<vntl::update::Release>>>, asked: bool) {
    let w = ui.as_weak();
    std::thread::spawn(move || {
        let result = vntl::update::check();
        let _ = w.upgrade_in_event_loop(move |ui| match result {
            Ok(Some(release)) => {
                ui.set_update_version(release.version.as_str().into());
                ui.set_news(release.notes.as_str().into());
                *found.lock().unwrap() = Some(release);
            }
            Ok(None) if asked => ui.set_update_note("You're up to date.".into()),
            Err(e) if asked => ui.set_update_note(format!("Couldn't ask GitHub: {e}").into()),
            _ => {}
        });
    });
}

/// The notice, the setting and the button that fetches the new version and hands over to it.
#[cfg(windows)]
fn wire_updates(ui: &AppWindow) {
    use vntl::update::{fetch, hand_over, installed_program, tidy, Release};
    let found: Arc<Mutex<Option<Release>>> = Arc::default();
    let program = installed_program();
    ui.set_can_update(program.is_some());
    if program.is_none() {
        ui.set_update_note("This copy runs from its own folder, so it updates with git.".into());
    }
    let (w, f) = (ui.as_weak(), found.clone());
    ui.on_check_now(move || check_for_update(&w.unwrap(), f.clone(), true));
    let w = ui.as_weak();
    ui.on_update_later(move || w.unwrap().set_update_version("".into()));
    // the About tab's links, which only ever go to the project's own pages
    ui.on_open_page(|page| {
        if page.starts_with("https://github.com/vVKamiVv/Kidoku") {
            vntl::setup::open_page(&page);
        }
    });
    let (w, f, installed) = (ui.as_weak(), found.clone(), program.clone());
    ui.on_update_now(move || {
        let ui = w.unwrap();
        let (Some(release), Some(program)) = (f.lock().unwrap().clone(), installed.clone()) else { return };
        ui.set_fetching(true);
        clear_progress(&ui);
        ui.set_status_text(format!("Downloading Kidoku {}…", release.version).into());
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let shown = progress_bar(weak.clone());
            let result = fetch(&release, &shown).and_then(|setup| hand_over(&setup, &program));
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_fetching(false);
                clear_progress(&ui);
                match result {
                    // the new Setup is waiting for this copy to close
                    Ok(()) => drop(slint::quit_event_loop()),
                    Err(e) => {
                        append_log(&ui, &e);
                        ui.set_status_text("Update failed  ·  see the log below.".into());
                    }
                }
            });
        });
    });
    if program.is_some() {
        tidy();
        if ui.get_check_updates() {
            check_for_update(ui, found, false);
        }
    }
}

/// `--install <folder> [--files-only]` installs with no window, for a script. The exit code tells.
#[cfg(windows)]
fn run_quiet_install(folder: &str, files_only: bool) -> ! {
    use vntl::setup::{install, Choices};
    let choices = Choices { folder: folder.into(), menu: !files_only, desktop: false, register: !files_only };
    let done = std::env::current_exe().map_err(|e| e.to_string()).and_then(|from| install(&from, &choices));
    std::process::exit(if done.is_ok() { 0 } else { 1 })
}

/// The version, and which commit and day the build came from, as `build.rs` found them.
fn version_text() -> String {
    let commit = env!("KIDOKU_COMMIT");
    let from = if commit.is_empty() { String::new() } else { format!("  ·  build {commit}") };
    format!("{}{from}  ·  {}", env!("CARGO_PKG_VERSION"), env!("KIDOKU_BUILT"))
}

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(windows)]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let has = |flag: &str| args.iter().any(|a| a == flag);
        if has("--uninstall") {
            run_uninstall(has("--quiet"));
            return Ok(());
        }
        match (args.first().map(String::as_str), args.get(1)) {
            (Some("--install"), Some(folder)) => run_quiet_install(folder, has("--files-only")),
            (Some("--update"), Some(folder)) => run_update(folder, has("--no-start")),
            _ => {}
        }
        let named = std::env::current_exe().ok().and_then(|p| Some(p.file_stem()?.to_string_lossy().to_lowercase()));
        if named.is_some_and(|n| n.contains("setup")) {
            return run_setup();
        }
    }
    let ui = AppWindow::new()?;
    ui.set_calm(animations_off());
    ui.set_version_text(version_text().into());
    ui.set_rows(ModelRc::new(VecModel::<LogRow>::default()));
    let w = ui.as_weak();
    ui.on_lines_switched(move |on| {
        if !on {
            hide_lines(&w.unwrap());
        }
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let memory = Arc::new(AtomicUsize::new(0)); // what the running translation reads for each line
    let shown = Rc::new(RefCell::new(PathBuf::new()));

    let (w, s) = (ui.as_weak(), shown.clone());
    ui.on_game_edited(move || detect_game(&w.unwrap(), &s));
    let (w, s) = (ui.as_weak(), shown.clone());
    ui.on_browse_game(move || {
        let ui = w.unwrap();
        // picking a folder always reloads its list; cancelling leaves everything as it is
        pick_folder(|d| {
            ui.set_game(game_root(Path::new(d.as_str())).display().to_string().into());
            save_typed(&ui, &s.borrow());
            let old = s.take();
            detect_game(&ui, &s);
            // a folder that can't be translated loads nothing, so the rows are still the old game's
            if s.borrow().as_os_str().is_empty() {
                s.replace(old);
            }
        });
    });
    let w = ui.as_weak();
    ui.on_browse_reference(move || pick_folder(|d| w.unwrap().set_reference(d)));
    let (w, timer) = (ui.as_weak(), Timer::default());
    // the Server box fires this per keystroke: wait for a pause so typing a URL makes one request
    ui.on_refresh_models(move || {
        let w = w.clone();
        timer.start(TimerMode::SingleShot, Duration::from_millis(400), move || refresh_models(&w.unwrap()));
    });
    let w = ui.as_weak();
    ui.on_get_model(move |model| get_model(&w.unwrap(), model.into()));
    let (w, c, m) = (ui.as_weak(), cancel.clone(), memory.clone());
    ui.on_translate(move || start(&w.unwrap(), &c, &m));
    ui.on_memory_moved(move |lines| memory.store(lines.clamp(6, 24) as usize, Ordering::Relaxed));
    let (w, c) = (ui.as_weak(), cancel.clone());
    ui.on_stop(move || {
        c.store(true, Ordering::Relaxed);
        w.unwrap().set_status_text("Stopping after the current step…".into());
    });
    let w = ui.as_weak();
    ui.on_remove(move || remove(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_forget_menus(move || forget_menus(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_save_log(move || save_log(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_fit_box(move || fit_box(&w.unwrap()));
    let (w, sampler) = (ui.as_weak(), Arc::new(Mutex::new(Sampler::default())));
    ui.on_read_usage(move || read_usage(&w.unwrap(), sampler.clone()));
    let w = ui.as_weak();
    ui.on_import_vndb(move || import_vndb(&w.unwrap()));
    let w = ui.as_weak();
    ui.on_synopsis_edited(move |text| {
        let ui = w.unwrap();
        let Some(game) = game_folder(&ui) else { return };
        if let Err(e) = save_synopsis(&game, &text) {
            append_log(&ui, &format!("Couldn't save the synopsis: {e}"));
        }
    });
    let w = ui.as_weak();
    ui.on_about_picked(move |name| {
        let ui = w.unwrap();
        ui.set_about_name(name);
        if let Some(game) = game_folder(&ui) {
            show_game_data(&ui, &game);
        }
    });
    let w = ui.as_weak();
    ui.on_about_edited(move |text| {
        let ui = w.unwrap();
        let Some(game) = game_folder(&ui) else { return };
        let list = load_characters(&game);
        let Some(c) = list.iter().find(|c| label(c) == ui.get_about_name().as_str()) else { return };
        if let Err(e) = save_description(&game, &c.original, &text) {
            append_log(&ui, &format!("Couldn't save the description: {e}"));
        }
    });
    let w = ui.as_weak();
    ui.on_link_flipped(move |i| {
        let ui = w.unwrap();
        let Some(game) = game_folder(&ui) else { return };
        let Some(link) = ui.get_links().row_data(i as usize) else { return };
        match set_unlinked(&game, &link.original, !link.broken) {
            Ok(()) => show_links(&ui, &game),
            Err(e) => append_log(&ui, &format!("Couldn't save that link: {e}")),
        }
    });
    let w = ui.as_weak();
    ui.on_add_character(move || {
        TYPED.store(true, Ordering::Relaxed);
        let ui = w.unwrap();
        let mut list = characters(&ui);
        list.push(Character::default());
        show_characters(&ui, list);
    });
    let w = ui.as_weak();
    ui.on_remove_character(move |i| {
        TYPED.store(true, Ordering::Relaxed);
        let ui = w.unwrap();
        let mut list = characters(&ui);
        list.remove(i as usize);
        show_characters(&ui, list);
    });
    let w = ui.as_weak();
    ui.on_character_edited(move |i, field, value| {
        TYPED.store(true, Ordering::Relaxed);
        // update the row in place: rebuilding the list would take the focus away while typing
        let rows = w.unwrap().get_characters();
        if let Some(mut r) = rows.row_data(i as usize) {
            match field.as_str() {
                "original" => r.original = value,
                "english" => r.english = value,
                _ => r.gender = value,
            }
            rows.set_row_data(i as usize, r);
        }
    });

    ui.set_all_picks(ModelRc::new(VecModel::from(vec![SharedString::from(VN_MODEL), IMAGE_MODEL.into(), READING_MODEL.into()])));
    load_settings(&ui);
    detect_game(&ui, &shown);
    show_offers(&ui, &[]);
    refresh_models(&ui);
    #[cfg(windows)]
    wire_updates(&ui);
    let result = ui.run();
    save_typed(&ui, &shown.borrow());
    save_settings(&ui);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reading_model_reads_and_the_image_model_writes_and_either_alone_does_both() {
        let url = "http://127.0.0.1:1";
        assert!(vision_chat(url, "", "").is_none());
        // the words go to the image model, whose name decides prompt formats
        assert_eq!(vision_chat(url, "gemma3:12b", "qwen2.5vl:7b").unwrap().model(), "gemma3:12b");
        assert_eq!(vision_chat(url, "", "qwen2.5vl:7b").unwrap().model(), "qwen2.5vl:7b");
        assert_eq!(vision_chat(url, "gemma3:12b", "").unwrap().model(), "gemma3:12b");
    }

    #[test]
    fn each_engine_is_offered_only_what_it_reads() {
        // (reference, scene, images, retranslate, redump)
        let row = |engine| {
            let o = offers(engine);
            (o.reference, o.scene, o.images, o.retranslate, o.redump)
        };
        assert_eq!(row("renpy"), (true, true, true, false, true));
        assert_eq!(row("unity"), (false, false, false, true, false));
        assert_eq!(row("liarsoft"), (false, false, true, false, false));
        assert_eq!(row("rpgmaker-mv/mz"), (false, true, true, false, false));
        assert!(offers("rpgmaker-mv/mz").scene_is_file && !offers("renpy").scene_is_file);
        // no folder, or an engine the app can't translate, offers nothing of its own
        for engine in ["", "unknown", "tyranoscript", "yu-ris"] {
            assert_eq!(offers(engine), EngineOptions::default());
        }
        for engine in ["kirikiri", "rpgmaker-2000/2003", "rpgmaker-xp/vx/ace"] {
            assert_eq!(offers(engine), EngineOptions { fit: true, ..Default::default() });
        }
        // the engines read through translate_script have nothing to set
        for engine in ["renpy", "unity", "liarsoft", "rpgmaker-mv/mz"] {
            assert_ne!(offers(engine), EngineOptions::default(), "{engine}");
        }
    }

    #[test]
    fn hiding_the_translated_lines_keeps_the_log_and_numbers_it_again() {
        let row = |time: &str, number, text: &str, kind| LogRow { time: time.into(), number, text: text.into(), kind };
        let rows = [row("21:00", 5, "start", 0), row("21:01", 6, "やあ", 1), row("", 6, "Hi", 2), row("21:02", 7, "a long", 0), row("", 7, "error", 0)];
        let kept = without_lines(rows.clone().into_iter());
        assert_eq!(kept.iter().map(|r| (r.number, r.text.as_str())).collect::<Vec<_>>(), [(5, "start"), (6, "a long"), (6, "error")]);
        // a saved log has each post on one line, without the game's lines
        assert_eq!(log_lines(rows.into_iter()), ["21:00  start", "21:02  a long error"]);
    }

    #[test]
    fn long_lines_fold_at_spaces_and_japanese_anywhere() {
        assert_eq!(folded("short", 20), ["short"]);
        assert_eq!(folded("", 20), [""]);
        assert_eq!(folded("Good morning, dear senpai.", 14), ["Good morning,", "dear senpai."]);
        assert_eq!(folded("おはようございます", 8), ["おはよう", "ございま", "す"]);
        assert_eq!(folded("Unbreakableword", 6), ["Unbrea", "kablew", "ord"]);
        assert_eq!(folded("  2100/2767", 20), ["  2100/2767"]); // the log's own indent stays
        for piece in folded("ねえ、この長い文章は画面の端まで届く long English words mixed in with it", 12) {
            assert!(piece.width() <= 12, "{piece}");
        }
    }

    #[test]
    fn time_left_rounds_up_and_says_nothing_without_a_pace() {
        let minute = Duration::from_secs(60);
        assert_eq!(time_left(minute, 100, 50), "about a minute left");
        assert_eq!(time_left(minute, 100, 1000), "about 10 min left");
        assert_eq!(time_left(minute, 100, 1001), "about 11 min left");
        assert_eq!(time_left(minute, 100, 6200), "about 1 h 5 min left");
        assert_eq!(time_left(minute, 100, 12000), "about 2 h 0 min left");
        assert_eq!(time_left(minute, 0, 500), "");
        assert_eq!(time_left(minute, 100, 0), "");
    }

    #[test]
    fn the_pace_follows_the_latest_steps_only() {
        let mut steps = VecDeque::new();
        assert_eq!(paced(&mut steps, 20, 100), ""); // one step is no pace yet
        for done in 2..=30 {
            paced(&mut steps, done * 20, 2000);
        }
        assert_eq!(steps.len(), 15);
        assert_eq!(steps[0].1, 320);
    }

    #[test]
    fn a_panic_ends_the_run_as_an_error_with_its_text() {
        assert!(catch_panic(|| panic!("bad {}", "table")).unwrap_err().contains("bad table"));
        assert!(catch_panic(|| panic!("plain")).unwrap_err().contains("plain"));
        assert_eq!(catch_panic(|| Err("no model".into())), Err("no model".to_string()));
    }

    #[test]
    fn a_gender_a_run_found_fills_a_blank_row_only() {
        let c = |original: &str, gender: &str| Character { original: original.into(), gender: gender.into(), ..Default::default() };
        let filled = with_found_genders(vec![c("ハナ", ""), c("ケン", "Non-binary")], &[c("ハナ", "Female"), c("ケン", "Male")]);
        assert_eq!(filled, [c("ハナ", "Female"), c("ケン", "Non-binary")]);
    }

    #[test]
    fn merging_keeps_the_rows_and_adds_only_new_names() {
        let c = |original: &str, english: &str, gender: &str| Character {
            original: original.into(), english: english.into(), gender: gender.into(), extra: String::new(),
        };
        let rows = vec![c("ハナ", "Hanna", "Female"), c("", "Bob", ""), c("", "", "")];
        let file = vec![c(" ハナ", "Hana", ""), c("", "Bob", "Male"), c("ケン", "", ""), c("", "Amy", "")];
        let got = merged(rows.clone(), file);
        assert_eq!(got[..3], rows[..]);
        assert_eq!(got[3..], [c("ケン", "", ""), c("", "Amy", "")]);
    }

    #[test]
    fn a_failed_delete_is_reported_and_keeps_the_manifest() {
        let dir = std::env::temp_dir().join(format!("vntl_gui_{}", std::process::id()));
        let (stuck, manifest, font) = (dir.join("stuck"), dir.join("vntl_files.txt"), dir.join("font.ttf"));
        fs::create_dir_all(&stuck).unwrap(); // remove_file refuses a folder
        fs::write(&manifest, "font.ttf").unwrap();
        fs::write(&font, "").unwrap();
        let (removed, failed) = delete_files(&mut [manifest.clone(), stuck.clone(), font.clone(), dir.join("gone")]);
        assert_eq!((removed, failed.len()), (1, 1));
        assert!(manifest.exists() && !font.exists());
        assert_eq!(delete_files(&mut [manifest.clone()]), (1, vec![]));
        assert!(!manifest.exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
