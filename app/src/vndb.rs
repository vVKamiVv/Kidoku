//! Game data from VNDB (vndb.org), through its public API: a synopsis, a character list and their
//! descriptions to start from. It needs no account, and the title searched for is all that leaves the
//! PC. The data is free for non-commercial use. It can have names wrong, so it only fills in blanks.

use crate::R;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;
use std::time::Duration;

const API: &str = "https://api.vndb.org/kana";
/// The synopsis goes into every prompt, so it has to leave room there for the story.
const SYNOPSIS_LETTERS: usize = 1000;

static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\s*(?:https?://vndb\.org/)?(v\d+)\s*$").unwrap());
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\[/?[a-z]+(?:=[^\]]*)?\]").unwrap());
static CREDIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\[(?:edited |translated )?from [^\]]*\]").unwrap());

/// One character of a game. `original` is the name in the game's own script.
#[derive(Debug, Default, PartialEq)]
pub struct Person {
    pub name: String,
    pub original: String,
    pub gender: String,
    /// VNDB says the character turns out otherwise. `gender` stays as they appear, which is how
    /// most of a story speaks of them, and doesn't spoil anything in the list.
    pub twist: bool,
    pub aliases: Vec<String>,
    pub description: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct Found {
    pub id: String,
    pub title: String,
    pub synopsis: String,
    pub people: Vec<Person>,
    /// Other games the search matched.
    pub others: usize,
}

/// A description as one plain paragraph: VNDB marks its links and credits with codes.
fn plain(description: &str) -> String {
    let text = CODE.replace_all(description, "");
    let text = CREDIT.replace_all(&text, "");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn synopsis(description: &str) -> String {
    let text = plain(description);
    if text.chars().count() <= SYNOPSIS_LETTERS {
        return text;
    }
    let cut: String = text.chars().take(SYNOPSIS_LETTERS).collect();
    cut.rfind(". ").map_or(cut.clone(), |at| cut[..=at].to_string())
}

fn person(v: &Value) -> Option<Person> {
    let word = |c: &Value| match c.as_str() {
        Some("f") => "Female",
        Some("m") => "Male",
        Some("o") => "Non-binary",
        _ => "",
    };
    // gender is what the entry states; older entries may only have sex
    let pair = if v["gender"].is_array() { &v["gender"] } else { &v["sex"] };
    Some(Person {
        name: v["name"].as_str().unwrap_or("").to_string(),
        original: v["original"].as_str()?.to_string(),
        gender: word(&pair[0]).to_string(),
        twist: pair[1].is_string() && pair[0] != pair[1],
        aliases: v["aliases"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect(),
        description: plain(v["description"].as_str().unwrap_or("")),
    })
}

fn post(path: &str, body: &Value) -> R<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let mut resp = agent.post(format!("{API}/{path}")).send_json(body).map_err(|e| format!("VNDB: {e}"))?;
    resp.body_mut().read_json().map_err(|e| format!("VNDB: {e}"))
}

/// The game a title or a VNDB id (v17, or its address) names, with its characters.
pub fn fetch(query: &str) -> R<Found> {
    // searchrank only exists for a search: VNDB refuses it on anything else
    let search = match ID.captures(query) {
        Some(id) => json!({"filters": ["id", "=", id[1].to_lowercase()], "fields": "title, description"}),
        None => json!({"filters": ["search", "=", query.trim()], "fields": "title, description", "sort": "searchrank", "results": 5}),
    };
    let games = post("vn", &search)?;
    let games = games["results"].as_array().filter(|g| !g.is_empty()).ok_or(format!("VNDB has nothing called {}. Try its Japanese title, or its id, like v17.", query.trim()))?;
    let id = games[0]["id"].as_str().ok_or("VNDB: a game with no id")?.to_string();
    let mut people = Vec::new();
    for page in 1.. {
        let list = post("character", &json!({"filters": ["vn", "=", ["id", "=", id]], "fields": "name, original, aliases, sex, gender, description", "results": 100, "page": page}))?;
        people.extend(list["results"].as_array().into_iter().flatten().filter_map(person));
        if list["more"] != json!(true) {
            break;
        }
    }
    Ok(Found {
        id,
        title: games[0]["title"].as_str().unwrap_or("").to_string(),
        synopsis: synopsis(games[0]["description"].as_str().unwrap_or("")),
        people,
        others: games.len() - 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_comes_out_as_one_plain_paragraph() {
        let text = "A [url=https://example.com]rainy[/url] town.\n\n[b]Two[/b] friends meet.\n\n[From [url=https://example.com]the maker[/url]]";
        assert_eq!(plain(text), "A rainy town. Two friends meet.");
    }

    #[test]
    fn a_long_synopsis_stops_at_a_sentence() {
        let text = "One short sentence. ".repeat(100);
        let got = synopsis(&text);
        assert!(got.len() <= SYNOPSIS_LETTERS && got.ends_with('.'), "{got}");
    }

    #[test]
    fn a_character_keeps_the_gender_they_appear_as() {
        let got = person(&json!({"name": "Hana", "original": "ハナ", "aliases": ["Hanna"], "gender": ["m", "f"], "sex": ["m", "f"], "description": "A [b]kind[/b] girl."})).unwrap();
        assert_eq!(got, Person { name: "Hana".into(), original: "ハナ".into(), gender: "Male".into(), twist: true, aliases: vec!["Hanna".into()], description: "A kind girl.".into() });
        assert!(!person(&json!({"name": "Ken", "original": "ケン", "gender": ["m", "m"]})).unwrap().twist);
    }

    #[test]
    fn an_older_entry_with_only_a_sex_still_gives_a_gender() {
        assert_eq!(person(&json!({"name": "Ken", "original": "ケン", "gender": null, "sex": ["m", "m"]})).unwrap().gender, "Male");
    }

    #[test]
    fn a_character_without_an_original_name_cant_be_matched_to_the_game() {
        assert_eq!(person(&json!({"name": "Ken", "original": null})), None);
    }

    #[test]
    fn an_id_or_its_address_is_told_from_a_title() {
        assert_eq!(&ID.captures(" V33587 ").unwrap()[1], "V33587");
        assert_eq!(&ID.captures("https://vndb.org/v17").unwrap()[1], "v17");
        assert!(ID.captures("v for vendetta").is_none());
    }
}
