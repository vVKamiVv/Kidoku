//! Command line: vntl detect|extract|translate|write <game_dir> [options]. `vntl-gui` is the window.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Arc;
use vntl::llm::{fetch, Chat, Pair, Server, VN_MODEL};
use vntl::pipeline::{script_engine, VNTL_HISTORY, translate, Event, Options};
use vntl::renpy::{detect, extract, game_root, SUPPORTED};

#[derive(Parser)]
#[command(name = "vntl", about = "Translate Japanese visual novels with a local LLM")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show which engine a game uses
    Detect { game: PathBuf },
    /// Dump the game's script to <game>/vntl_work/dump.json (Unity: list its string tables)
    Extract { game: PathBuf },
    /// Translate (resumes where it left off) and install the translation
    Translate {
        game: PathBuf,
        /// only this Ren'Py label (scene)
        #[arg(long)]
        label: Option<String>,
        /// only this script file, e.g. 10 for 10.rpyc
        #[arg(long)]
        file: Option<String>,
        /// language name for folders and official text; the model always writes English
        #[arg(long, default_value = "english")]
        lang: String,
        /// OpenAI-compatible server (Ollama, llama-server, LM Studio)
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        url: String,
        #[arg(long, default_value = "gemma3:12b")]
        model: String,
        #[arg(long, default_value_t = 20)]
        batch: usize,
        #[arg(long, default_value_t = 0.3)]
        temperature: f64,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// another Ren'Py build of the same game with an official translation; matching lines are copied from it
        #[arg(long)]
        reference: Option<PathBuf>,
        /// with --label/--file, also translate the interface (always on otherwise)
        #[arg(long)]
        ui: bool,
        /// re-extract even if a dump exists
        #[arg(long)]
        redump: bool,
        /// Unity: retranslate text the game already has in --lang (names still come from the game)
        #[arg(long)]
        retranslate: bool,
        /// start over: set the saved translations aside and translate everything again
        #[arg(long)]
        fresh: bool,
        /// a vision model (e.g. gemma3:12b): also read, translate and repaint menu images
        #[arg(long)]
        vision_model: Option<String>,
        /// a model that reads menu pictures better (e.g. qwen2.5vl:7b); --vision-model still writes their English
        #[arg(long)]
        reading_model: Option<String>,
        /// leave menu images alone, whatever --vision-model is
        #[arg(long)]
        no_images: bool,
        /// Ren'Py: also paint English over text in artwork, which is slow and can look edited
        #[arg(long)]
        art: bool,
        /// earlier lines a VNTL model sees: more context, but slower
        #[arg(long, default_value_t = VNTL_HISTORY as u8, value_parser = clap::value_parser!(u8).range(6..=24))]
        history: u8,
    },
    /// Download a model into Ollama (the visual novel model by default)
    Pull {
        #[arg(default_value = VN_MODEL)]
        model: String,
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        url: String,
    },
    /// Measure the message box from a screenshot of a long line, for the next write
    Fit {
        game: PathBuf,
        image: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        url: String,
        #[arg(long, default_value = "gemma3:12b")]
        vision_model: String,
    },
    /// Fill in the game's synopsis, characters and descriptions from VNDB (sends only the title)
    Vndb {
        game: PathBuf,
        /// a title or a VNDB id like v17; the game's folder name by default
        query: Option<String>,
    },
    /// Write and check the translation from the cache, without the model
    Write {
        game: PathBuf,
        /// language name for folders and official text; the model always writes English
        #[arg(long, default_value = "english")]
        lang: String,
    },
}

fn run(cli: Cli) -> vntl::R<()> {
    let picked = match &cli.command {
        Command::Detect { game } | Command::Extract { game } | Command::Translate { game, .. } | Command::Write { game, .. } | Command::Fit { game, .. } | Command::Vndb { game, .. } => {
            std::path::absolute(game).unwrap_or_else(|_| game.clone())
        }
        Command::Pull { model, url } => {
            let shown = |done: u64, total: u64| eprint!("\r{}%  ", done * 100 / total.max(1));
            return fetch(url, model, &shown).map(|_| println!("\nDownloaded {model}"));
        }
    };
    let game = game_root(&picked);
    if game != picked {
        println!("Game folder: {}", game.display());
    }
    let engine = detect(&game);
    println!("Engine: {engine}");
    if let Command::Detect { .. } = cli.command {
        return Ok(());
    }
    if let Command::Vndb { query, .. } = &cli.command {
        let folder = game.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        println!("{}", vntl::pipeline::import_game_data(&game, &vntl::vndb::fetch(query.as_ref().unwrap_or(&folder))?)?);
        return Ok(());
    }
    if !SUPPORTED.contains(&engine) {
        return Err(format!("{engine} is not supported yet (supported: {})", SUPPORTED.join(", ")));
    }
    if let (Command::Extract { .. }, Some(script)) = (&cli.command, script_engine(engine)) {
        let what = if engine == "wolf-rpg" { "different Japanese texts" } else { "Japanese lines" };
        println!("{} {what}", (script.lines)(&game)?);
        return Ok(());
    }
    let work = game.join("vntl_work");
    match cli.command {
        Command::Detect { .. } | Command::Pull { .. } | Command::Vndb { .. } => {}
        Command::Fit { image, url, vision_model, .. } => {
            let bytes = std::fs::read(&image).map_err(|e| format!("{}: {e}", image.display()))?;
            println!("{}", vntl::fit::calibrate(&game, Some(&Server::new(&url, &vision_model, 0.0, 600)), &bytes)?);
        }
        Command::Extract { .. } if engine == "unity" => {
            let aa = vntl::unity::addressables(&game).ok_or("no …_Data/StreamingAssets/aa folder")?;
            let mut by_locale = std::collections::BTreeMap::<String, (usize, usize)>::new();
            for t in vntl::unity::read_tables(&aa)? {
                let e = by_locale.entry(t.locale).or_default();
                *e = (e.0 + 1, e.1 + t.entries.len());
            }
            for (locale, (tables, entries)) in by_locale {
                println!("{locale}: {tables} string tables, {entries} entries");
            }
        }
        Command::Extract { .. } if engine == "liarsoft" => {
            let texts = vntl::liar::texts(&game)?;
            let ja = texts.iter().filter(|(_, _, s)| vntl::text::JA.is_match(s)).count();
            let scripts: std::collections::HashSet<_> = texts.iter().map(|(s, _, _)| s).collect();
            println!("{} scripts, {} strings, {ja} with Japanese", scripts.len(), texts.len());
        }
        Command::Extract { .. } if matches!(engine, "rpgmaker-mv/mz" | "rpgmaker-xp/vx/ace" | "rpgmaker-2000/2003") => {
            let texts = vntl::rpgmaker::texts(&game)?;
            let unique: std::collections::HashSet<_> = texts.iter().map(|t| &t.text).collect();
            println!("{} Japanese texts, {} different ones", texts.len(), unique.len());
        }
        Command::Extract { .. } => {
            let d = extract(&game, &work, "dump.json")?;
            let lines: usize = d.blocks.iter().map(|b| b.items.iter().filter(|i| i.is_say()).count()).sum();
            println!("Extracted {lines} lines, {} strings", d.strings.len());
        }
        Command::Translate { label, file, lang, url, model, batch, temperature, timeout, reference, ui, redump, retranslate, fresh, vision_model, reading_model, no_images, art, history, .. } => {
            let chat = Server::new(&url, &model, temperature, timeout);
            let server = |m: &str| Server::new(&url, m, 0.0, timeout);
            let vision = match (vision_model, reading_model) {
                (Some(v), Some(r)) if v != r => Some(Box::new(Pair { reads: server(&r), words: server(&v) }) as Box<dyn Chat + Send>),
                (v, r) => v.or(r).map(|m| Box::new(server(&m)) as Box<dyn Chat + Send>),
            };
            let o = Options { game, lang, label, file, reference, ui, redump, batch, model, retranslate, fresh, vision, images: !no_images, art, history: Arc::new(AtomicUsize::new(history.into())) };
            let report = |e: Event| {
                if let Event::Log(s) = e {
                    println!("{s}");
                }
            };
            translate(&o, &chat, &report, &AtomicBool::new(false))?;
        }
        Command::Write { lang, .. } => println!("{}", vntl::pipeline::write(&game, engine, &lang)?),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
