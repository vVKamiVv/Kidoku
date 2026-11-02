//! How wide a game's message box really is. Most engines don't say, so a screenshot settles it: the
//! rows as they appear on screen are read, and the app counts the letters. The text reader reads
//! Japanese rows; it drops the spaces of English ones, which the image model reads instead. Models
//! read well and judge layout badly: asked whether text spills or how many rows there are, one gave a
//! different answer per prompt, while the widest row held steady. So only width is measured. The
//! result is saved for the game in `vntl_work/box.txt`, where `rows=` can be set by hand, and the
//! next write uses it.

use crate::llm::Chat;
use crate::menus::read_picture;
use crate::ocr::Reader;
use crate::text::{text_units, JA};
use crate::R;
use std::path::{Path, PathBuf};

/// Engines that break lines themselves and so have a width to set. YU-RIS and TyranoScript wrap on
/// their own, Liar-soft measures its font, and Ren'Py and Unity lay their text out at run time.
pub const MEASURED: &[&str] = &[
    "rpgmaker-mv/mz", "rpgmaker-xp/vx/ace", "rpgmaker-2000/2003", "malie", "wolf-rpg", "siglusengine", "qlie", "reallive",
    "catsystem2", "majiro", "bgi", "advhd", "kirikiri", "artemis", "softpal", "fvp", "nscripter",
];

const PROMPT: &str = "This is a screenshot of a visual novel. Copy the dialogue or narration in the message box exactly as it \
appears, one line of your reply for each row of text on screen, breaking where the screen breaks. Leave out the speaker's name \
and any buttons or menus. Reply with nothing else.";

/// Off the widest row: a quote mark hangs the rows after it in by about this much.
const MARGIN: usize = 2;
/// A row shorter than this can't say how wide the box is.
const SHORTEST: usize = 12;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Fit {
    letters: Option<usize>,
    rows: Option<usize>,
}

fn path(game: &Path) -> PathBuf {
    game.join("vntl_work").join("box.txt")
}

fn load(game: &Path) -> Fit {
    let text = std::fs::read_to_string(path(game)).unwrap_or_default();
    let value = |key: &str| text.lines().find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('=')?.trim().parse().ok()).filter(|&n: &usize| n > 0);
    Fit { letters: value("letters"), rows: value("rows") }
}

fn save(game: &Path, fit: Fit) -> R<()> {
    let mut text = String::from("# How much fits the game's message box, in half-width letters and rows. Edit it if it looks wrong.\n");
    fit.letters.iter().for_each(|n| text += &format!("letters={n}\n"));
    fit.rows.iter().for_each(|n| text += &format!("rows={n}\n"));
    std::fs::create_dir_all(path(game).parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(path(game), text).map_err(|e| format!("{}: {e}", path(game).display()))
}

/// Letters to a row for this game: what was measured, or what the engine works out for itself.
pub fn letters(game: &Path, otherwise: usize) -> usize {
    load(game).letters.unwrap_or(otherwise)
}

/// Rows to a box for this game: what was measured, or the engine's own figure.
pub fn rows(game: &Path, otherwise: usize) -> usize {
    load(game).rows.unwrap_or(otherwise)
}

/// The rows of a reply.
fn reading(reply: &str) -> Vec<String> {
    reply.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("```")).map(String::from).collect()
}

/// What a screenshot says, on top of what's saved. A row can't be wider than the box, but it may have
/// been broken early, so a saved width only ever comes down: a row that reaches it says nothing new.
fn settled(saved: Fit, rows: &[String]) -> Fit {
    let widest = rows.iter().map(|r| text_units(r)).max().unwrap_or(0);
    Fit {
        letters: match saved.letters {
            Some(old) if widest >= old => Some(old),
            _ => Some(widest.saturating_sub(MARGIN)),
        },
        rows: saved.rows,
    }
}

/// The Japanese rows of a screenshot, if the text reader finds a long one. Only PNG can be decoded here.
fn japanese_rows(image: &[u8]) -> Option<Vec<String>> {
    let img = read_picture(image).ok()?;
    let rows = Reader::open().ok()?.page(&img).ok()?;
    let rows: Vec<String> = rows.into_iter().filter(|r| JA.is_match(r)).collect();
    long_enough(&rows).then_some(rows)
}

fn long_enough(rows: &[String]) -> bool {
    rows.iter().map(|r| text_units(r)).max().unwrap_or(0) >= SHORTEST
}

/// Reads a screenshot and saves what it shows. Returns a line for the user. `model` is the image
/// model, for what the text reader can't measure.
pub fn calibrate(game: &Path, model: Option<&dyn Chat>, image: &[u8]) -> R<String> {
    let rows = match (japanese_rows(image), model) {
        (Some(rows), _) => rows,
        (None, Some(model)) => reading(&model.read_image(image, PROMPT)?),
        (None, None) => return Err("The text reader only measures Japanese text in a PNG. For anything else, pick an image model on the Model tab.".into()),
    };
    if !long_enough(&rows) {
        return Err("The model found no long row of text there. Use a screenshot of a long message that fills the box's width.".into());
    }
    let fit = settled(load(game), &rows);
    save(game, fit)?;
    Ok(format!("Saved {} letters a row for this game. Press Translate to rewrite the text to fit.", fit.letters.unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::tests::Fake;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("vntl-fit-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rows_of(lengths: &[usize]) -> Vec<String> {
        lengths.iter().map(|&n| "a".repeat(n)).collect()
    }

    #[test]
    fn a_reply_gives_its_rows() {
        let reply = "```\nThat's right. The teacher\n\nan urgent matter\n```";
        assert_eq!(reading(reply), ["That's right. The teacher", "an urgent matter"]);
    }

    #[test]
    fn the_widest_row_sets_the_width_and_rows_set_by_hand_stay() {
        let fit = settled(Fit { letters: None, rows: Some(3) }, &rows_of(&[41, 20, 34, 12]));
        assert_eq!(fit, Fit { letters: Some(39), rows: Some(3) });
        // once saved, a row that merely reaches the width changes nothing, or each screenshot would shrink it
        assert_eq!(settled(fit, &rows_of(&[39, 30])), fit);
        assert_eq!(settled(fit, &rows_of(&[30, 30])).letters, Some(28));
    }

    #[test]
    fn full_width_text_counts_double() {
        let fit = settled(Fit::default(), &["あ".repeat(21)]);
        assert_eq!(fit, Fit { letters: Some(40), rows: None });
    }

    #[test]
    fn the_saved_box_is_used_and_can_be_edited() {
        let game = dir();
        assert_eq!((letters(&game, 60), rows(&game, 3)), (60, 3));
        let model = Fake(|_: &str| Ok(format!("{}\n{}\n{}", "a".repeat(41), "b".repeat(20), "c".repeat(34))));
        let said = calibrate(&game, Some(&model), b"image").unwrap();
        assert!(said.contains("39 letters"), "{said}");
        assert_eq!((letters(&game, 60), rows(&game, 4)), (39, 4));
        std::fs::write(path(&game), "letters=45\nrows=2\n").unwrap();
        assert_eq!((letters(&game, 60), rows(&game, 4)), (45, 2));
        let _ = std::fs::remove_dir_all(game);
    }

    #[test]
    fn a_screenshot_without_a_long_row_saves_nothing() {
        let game = dir();
        let model = Fake(|_: &str| Ok("short".to_string()));
        assert!(calibrate(&game, Some(&model), b"image").unwrap_err().contains("long"));
        assert!(calibrate(&game, None, b"image").unwrap_err().contains("image model"));
        assert!(!path(&game).exists());
        let _ = std::fs::remove_dir_all(game);
    }
}
