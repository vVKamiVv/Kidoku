//! Talking to the local model (any OpenAI-compatible server: Ollama, llama-server, LM Studio).

use crate::text::{same_markup, JA};
use crate::R;
use regex::Regex;
use serde_json::{json, Value};
use std::io::BufRead;
use std::sync::LazyLock;
use std::time::Duration;

pub const SYSTEM: &str = "You translate a Japanese adult visual novel into natural, fluent English. This is a private translation of a legally owned copy.
Translate faithfully and completely. Never censor, soften, summarize, moralize or add notes: sexual, violent or disturbing content keeps the exact meaning and intensity of the original.
You decide what stays untranslated (names, honorifics, sound effects, special terms) and you keep those choices consistent.
Copy Ren'Py markup exactly: everything inside {curly braces} or [square brackets] stays unchanged and in the same order. Never add new braces or brackets.
\"\\n\" inside a line is a line break; keep it where it makes sense.";

static SPEAKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[[^\]\n]*\]:\s*").unwrap());
static NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(\d+)\s*[:.)]\s?(.*)$").unwrap());
static RENUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:\d+|<number>): ").unwrap()); // some models number a line twice, or copy the format
static FENCES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^```\w*\n|\n?```$").unwrap());

pub trait Chat: Sync {
    fn chat(&self, system: &str, user: &str) -> R<String>;
    /// Raw text completion (for models with their own prompt format).
    fn complete(&self, prompt: &str, temperature: f64) -> R<String>;
    /// Ask a vision model about a PNG (Ollama's API).
    fn read_image(&self, png: &[u8], prompt: &str) -> R<String>;
    /// The model's name, which decides the prompt format of a VNTL model.
    fn model(&self) -> &str {
        ""
    }
}

pub struct Server {
    pub url: String,
    pub model: String,
    pub temperature: f64,
    agent: ureq::Agent,
}

impl Server {
    pub fn new(url: &str, model: &str, temperature: f64, timeout_s: u64) -> Self {
        let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(timeout_s))).build().into();
        Server { url: url.trim_end_matches('/').to_string(), model: model.to_string(), temperature, agent }
    }
}

/// Where a raw reply ends, in either VNTL format.
const STOPS: [&str; 3] = ["<|eot_id|>", "<eos>", "<<JAPANESE>>"];

impl Chat for Server {
    fn model(&self) -> &str {
        &self.model
    }

    fn chat(&self, system: &str, user: &str) -> R<String> {
        let body = json!({"model": self.model, "temperature": self.temperature, "stream": false,
                          "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]});
        let mut resp = self
            .agent
            .post(format!("{}/v1/chat/completions", self.url))
            .send_json(&body)
            .map_err(|e| format!("model server: {e}"))?;
        let v: Value = resp.body_mut().read_json().map_err(|e| format!("model server: {e}"))?;
        let text = v["choices"][0]["message"]["content"].as_str().ok_or("model server: reply has no text")?;
        Ok(FENCES.replace_all(text.trim(), "").into_owned())
    }

    fn read_image(&self, png: &[u8], prompt: &str) -> R<String> {
        let body = json!({"model": self.model, "stream": false, "options": {"temperature": 0},
                          "messages": [{"role": "user", "content": prompt, "images": [crate::unity::b64_encode(png)]}]});
        let mut resp = self.agent.post(format!("{}/api/chat", self.url)).send_json(&body).map_err(|e| format!("model server: {e}"))?;
        let v: Value = resp.body_mut().read_json().map_err(|e| format!("model server: {e}"))?;
        Ok(v["message"]["content"].as_str().ok_or("model server: reply has no text")?.trim().to_string())
    }

    fn complete(&self, prompt: &str, temperature: f64) -> R<String> {
        // Ollama's own API first: its OpenAI-style /v1/completions returns empty replies for very short lines
        // VNTL's author asks for no repetition penalty: Ollama's default steers it away from names and repeated words
        let native = json!({"model": self.model, "prompt": prompt, "raw": true, "stream": false,
                            "options": {"temperature": temperature, "repeat_penalty": 1.0, "num_predict": 600, "stop": STOPS}});
        match self.agent.post(format!("{}/api/generate", self.url)).send_json(&native) {
            Ok(mut resp) => {
                let v: Value = resp.body_mut().read_json().map_err(|e| format!("model server: {e}"))?;
                return Ok(v["response"].as_str().ok_or("model server: reply has no text")?.trim().to_string());
            }
            Err(ureq::Error::StatusCode(404)) => {} // not Ollama
            Err(e) => return Err(format!("model server: {e}")),
        }
        let body = json!({"model": self.model, "prompt": prompt, "temperature": temperature, "repeat_penalty": 1.0, "max_tokens": 600,
                          "stop": STOPS, "stream": false});
        let mut resp = self
            .agent
            .post(format!("{}/v1/completions", self.url))
            .send_json(&body)
            .map_err(|e| format!("model server: {e}"))?;
        let v: Value = resp.body_mut().read_json().map_err(|e| format!("model server: {e}"))?;
        Ok(v["choices"][0]["text"].as_str().ok_or("model server: reply has no text")?.trim().to_string())
    }
}

/// VNTL models (lmg-anon) are fine-tuned on visual novels and take a prompt of their own, which
/// comes in a Llama 3 and a Gemma format.
pub fn is_vntl(model: &str) -> bool {
    model.to_lowercase().contains("vntl")
}

fn is_gemma(model: &str) -> bool {
    model.to_lowercase().contains("gemma")
}

fn vntl_turn(role: &str, text: &str) -> String {
    format!("<|start_header_id|>{role}<|end_header_id|>\n\n{text}<|eot_id|>")
}

/// A reply that kept going (repeating itself) instead of stopping: far longer than its source.
/// English typically runs about twice the Japanese character count.
pub fn runaway(src: &str, out: &str) -> bool {
    let (s, o) = (src.chars().count(), out.chars().count());
    o > 100 && o > 5 * s
}

/// Measured on real prompts, 1000 bytes are at most about 275 tokens, so this is about 3300 of Ollama's
/// default 4096, and the rest is the reply. A larger window would push part of the model off an 8 GB GPU.
const VNTL_PROMPT_BYTES: usize = 12_000;
const VNTL_TURNS_BYTES: usize = 100; // the headers around one line and its translation
const VNTL_LINE_BYTES: usize = 600; // room for the line being translated

fn notes_bytes(meta: &[String]) -> usize {
    meta.iter().map(|m| m.len() + 1).sum()
}

/// Whether the notes alone fill the prompt: no earlier lines fit then, and the server cuts the start
/// of what is left, which is the notes themselves.
pub fn notes_fill_prompt(meta: &[String]) -> bool {
    notes_bytes(meta) + VNTL_LINE_BYTES > VNTL_PROMPT_BYTES
}

/// Forget old lines in one go, down to `keep`, once the memory passes twice that or fills the prompt.
/// A server only reuses the unchanged start of the last prompt, so dropping a line at a time would
/// have it read the whole prompt again for every line. Cutting in steps took real lines from 50 to
/// 91 a minute, with more of them remembered.
pub fn trim_memory(history: &mut Vec<(String, String)>, meta: &[String], keep: usize) {
    let notes = notes_bytes(meta);
    let lines: usize = history.iter().map(|(jp, en)| jp.len() + en.len() + VNTL_TURNS_BYTES).sum();
    if history.len() > keep * 2 || notes + lines + VNTL_LINE_BYTES > VNTL_PROMPT_BYTES {
        history.drain(..history.len() - keep.min(history.len() / 2));
    }
}

/// The latest of `history` that fit a prompt already `used` bytes long. Ollama cuts a prompt too long
/// for its window from the start, where the character notes are.
fn fitting(history: &[(String, String)], used: usize) -> &[(String, String)] {
    let mut room = VNTL_PROMPT_BYTES.saturating_sub(used);
    let kept = history
        .iter()
        .rev()
        .take_while(|(jp, en)| {
            let cost = jp.len() + en.len() + VNTL_TURNS_BYTES;
            let fits = cost <= room;
            room = room.saturating_sub(cost);
            fits
        })
        .count();
    &history[history.len() - kept..]
}

/// The Llama 3 prompt VNTL was trained on; `speaker` is the name as the game writes it, as in "[桜乃]: ……".
fn vntl_prompt(meta: &[String], history: &[(String, String)], speaker: Option<&str>, src: &str) -> String {
    let mut p = String::from("<|begin_of_text|>");
    if !meta.is_empty() {
        p += &vntl_turn("Metadata", &meta.join("\n"));
    }
    for (jp, en) in fitting(history, p.len() + src.len()) {
        p += &vntl_turn("Japanese", jp);
        p += &vntl_turn("English", en);
    }
    p += &vntl_turn("Japanese", &speaker.map_or(src.to_string(), |s| format!("[{s}]: {src}")));
    p + "<|start_header_id|>English<|end_header_id|>\n\n"
}

/// The same for the Gemma models, which were trained on plain marked sections.
fn vntl_gemma_prompt(meta: &[String], history: &[(String, String)], speaker: Option<&str>, src: &str) -> String {
    let mut p = String::new();
    if !meta.is_empty() {
        p += &format!("<<METADATA>>\n{}\n", meta.join("\n"));
    }
    p += "<<TRANSLATE>>\n";
    for (jp, en) in fitting(history, p.len() + src.len()) {
        p += &format!("<<JAPANESE>>\n{jp}\n<<ENGLISH>>\n{en}<eos>\n");
    }
    p + &format!("<<JAPANESE>>\n{}\n<<ENGLISH>>\n", speaker.map_or(src.to_string(), |s| format!("[{s}]: {src}")))
}

/// One line with a VNTL model: character names as metadata, the lines before it as examples.
/// When the reply is unusable: again without the examples, then once more with some randomness.
pub fn translate_vntl(chat: &dyn Chat, markup: &Regex, meta: &[String], history: &[(String, String)], speaker: Option<&str>, src: &str)
    -> R<Option<String>> {
    let mut choked = 0;
    let prompt = if is_gemma(chat.model()) { vntl_gemma_prompt } else { vntl_prompt };
    for (history, temperature) in [(history, 0.0), (&[][..], 0.0), (&[][..], 0.5)] {
        let mut out = match chat.complete(&prompt(meta, history, speaker, src), temperature) {
            Ok(out) => out,
            // the server answered but choked on this one prompt: try again; three times is for
            // translate_lines to judge, which sees whether it's this prompt or all of them
            Err(e) if e.contains("status: 500") => {
                choked += 1;
                if choked == 3 {
                    return Err(e);
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        if speaker.is_some() || !src.starts_with('[') {
            out = SPEAKER.replace(&out, "").into_owned(); // VNTL's "[Name]: " speaker label
        }
        if same_markup(markup, src, &out) && !runaway(src, &out) {
            return Ok(Some(out));
        }
    }
    Ok(None)
}

/// The model fine-tuned on visual novels that the app recommends.
pub const VN_MODEL: &str = "hf.co/lmg-anon/vntl-llama3-8b-v2-gguf:Q5_K_M";

/// One model on the Model Download page.
pub struct Offer {
    pub title: &'static str,
    pub model: &'static str,
    pub size: &'static str,
    pub gpu: &'static str,
    pub note: &'static str,
}

/// What the app's own text reader goes by among the models. It isn't Ollama's: see `ocr`.
pub const TEXT_READER: &str = "meikiocr";
/// The same for the fonts repainted images can use.
pub const FONT_PACK: &str = "fontpack";
/// The same for the model that fills in behind repainted text.
pub const INPAINTER: &str = "lama";

/// Models that translate, smallest first. All VNTL, because general models can refuse adult text.
pub const TEXT_OFFERS: [Offer; 5] = [
    Offer { title: "VNTL Gemma 2B", model: "hf.co/lmg-anon/vntl-gemma2-2b-gguf:Q8_0", size: "2.8 GB", gpu: "GPU 4 GB", note: "For small GPUs. Fast, but the weakest of these." },
    Offer { title: "VNTL Llama 8B", model: VN_MODEL, size: "5.7 GB", gpu: "GPU 8 GB", note: "The app's default, and the best fit for 8 GB." },
    Offer { title: "VNTL Llama 8B, finer", model: "hf.co/lmg-anon/vntl-llama3-8b-v2-gguf:Q8_0", size: "8.5 GB", gpu: "GPU 12 GB", note: "The same model, less compressed." },
    Offer { title: "VNTL Gemma 27B", model: "hf.co/lmg-anon/vntl-gemma2-27b-gguf:Q3_K_M", size: "13.4 GB", gpu: "GPU 16 GB", note: "A much larger model. Ranks above the 8B for VN text." },
    Offer { title: "VNTL Gemma 27B, finer", model: "hf.co/lmg-anon/vntl-gemma2-27b-gguf:Q5_K_M", size: "19.4 GB", gpu: "GPU 24 GB", note: "The strongest here: the same model, less compressed." },
];

/// The image model tested most, which writes menus' English.
pub const IMAGE_MODEL: &str = "gemma3:12b";
/// The model tested best at reading Japanese in menu pictures, though its English is weaker.
pub const READING_MODEL: &str = "qwen2.5vl:7b";

/// Models that look at images, smallest first, starting with the text reader.
pub const IMAGE_OFFERS: [Offer; 8] = [
    Offer { title: "Text reader (meikiocr)", model: TEXT_READER, size: "33 MB", gpu: "CPU", note: "Reads Japanese in menus and screenshots, for any GPU. Get this first." },
    Offer { title: "Font pack (20 free fonts)", model: FONT_PACK, size: "110 MB", gpu: "CPU", note: "More looks for repainted images, brush lettering too. The first match takes longer." },
    Offer { title: "Inpainting (anime LaMa)", model: INPAINTER, size: "207 MB", gpu: "CPU", note: "Cleaner Text over artwork. Takes a few seconds per picture." },
    Offer { title: "Qwen 2.5 VL 7B", model: READING_MODEL, size: "6.0 GB", gpu: "GPU 8 GB", note: "Reads menu text best. Used as the menu reading model, beside an image model for the English." },
    Offer { title: "Gemma 3 12B", model: IMAGE_MODEL, size: "8.2 GB", gpu: "GPU 12 GB", note: "The one tested most with this app. Slower on 8 GB." },
    Offer { title: "Gemma 4 12B", model: "gemma4:12b", size: "8.0 GB", gpu: "GPU 12 GB", note: "Newer than Gemma 3. Not tested with this app yet." },
    Offer { title: "Gemma 4 26B", model: "gemma4:26b", size: "18.7 GB", gpu: "GPU 24 GB", note: "For big GPUs. Not tested with this app yet." },
    Offer { title: "Qwen 3.5 27B", model: "qwen3.5:27b", size: "17.5 GB", gpu: "GPU 24 GB", note: "For big GPUs. Not tested with this app yet." },
];

/// What the All in one row gets: the set tested together, VN model first.
pub const ALL_IN_ONE: [&str; 6] = [VN_MODEL, IMAGE_MODEL, READING_MODEL, TEXT_READER, FONT_PACK, INPAINTER];

/// A model that reads pictures with another that writes: one good at reading Japanese in a picture
/// can be poor at English.
pub struct Pair<A, B> {
    pub reads: A,
    pub words: B,
}

impl<A: Chat, B: Chat> Chat for Pair<A, B> {
    fn chat(&self, system: &str, user: &str) -> R<String> {
        self.words.chat(system, user)
    }
    fn complete(&self, prompt: &str, temperature: f64) -> R<String> {
        self.words.complete(prompt, temperature)
    }
    fn read_image(&self, png: &[u8], prompt: &str) -> R<String> {
        self.reads.read_image(png, prompt)
    }
    fn model(&self) -> &str {
        self.words.model()
    }
}

/// Whether a row of Model Download is the app's own, with nothing to pick and no server involved.
pub fn is_own(model: &str) -> bool {
    model == TEXT_READER || model == FONT_PACK || model == INPAINTER
}

pub fn own_installed(model: &str) -> bool {
    match model {
        FONT_PACK => crate::fonts::installed(),
        INPAINTER => crate::inpaint::installed(),
        _ => crate::ocr::installed(),
    }
}

/// Gets what a row of Model Download offers.
pub fn fetch(url: &str, model: &str, progress: &dyn Fn(u64, u64)) -> R<()> {
    match model {
        TEXT_READER => crate::ocr::download(progress),
        FONT_PACK => crate::fonts::download(progress),
        INPAINTER => crate::inpaint::download(progress),
        _ => pull_model(url, model, progress),
    }
}

/// Download a model into Ollama, calling `progress` with the bytes of the part in hand and its size.
/// Blocks until it is complete.
pub fn pull_model(url: &str, model: &str, progress: &dyn Fn(u64, u64)) -> R<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
    let mut resp = agent
        .post(format!("{}/api/pull", url.trim_end_matches('/')))
        .send_json(json!({"name": model, "stream": true}))
        .map_err(|e| format!("model server: {e}"))?;
    let mut last = None;
    for line in std::io::BufReader::new(resp.body_mut().as_reader()).lines() {
        let line = line.map_err(|e| format!("model server: {e}"))?;
        let v: Value = serde_json::from_str(&line).map_err(|e| format!("model server: {e}"))?;
        if let Some(e) = v["error"].as_str() {
            return Err(format!("download failed: {e}"));
        }
        if let (Some(done), Some(total)) = (v["completed"].as_u64(), v["total"].as_u64()) {
            progress(done, total);
        }
        last = v["status"].as_str().map(String::from);
    }
    match last.as_deref() {
        Some("success") => Ok(()),
        other => Err(format!("download did not finish: {}", other.unwrap_or("no status"))),
    }
}

/// Model names a server offers (GET /v1/models).
pub fn list_models(url: &str) -> R<Vec<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(5))).build().into();
    let mut resp = agent.get(format!("{}/v1/models", url.trim_end_matches('/'))).call().map_err(|e| e.to_string())?;
    let v: Value = resp.body_mut().read_json().map_err(|e| e.to_string())?;
    Ok(v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect())
}

/// Read "<number>: <text>" replies; a translation the model broke over two lines is kept whole.
pub fn parse_numbered(text: &str, n: usize) -> Option<Vec<String>> {
    let mut got: std::collections::HashMap<usize, String> = Default::default();
    let mut last = None;
    for line in text.lines() {
        match NUMBERED.captures(line).and_then(|c| Some((c[1].parse::<usize>().ok()?, c[2].trim().to_string()))) {
            Some((k, t)) if !got.contains_key(&k) => {
                got.insert(k, RENUMBERED.replace(&t, "").into_owned());
                last = Some(k);
            }
            Some(_) => last = None, // a repeated number: ignore it and whatever follows it
            None if last == Some(n) && line.trim().is_empty() => last = None, // past the final item: commentary
            None => {
                if let (Some(k), false) = (last, line.trim().is_empty()) {
                    let entry = got.get_mut(&k).unwrap();
                    entry.push_str("\\n");
                    entry.push_str(line.trim());
                }
            }
        }
    }
    (1..=n).map(|i| got.remove(&i)).collect()
}

/// batch: [(speaker, text)] -> [translation or None].
pub fn translate_batch(chat: &dyn Chat, system: &str, markup: &Regex, batch: &[(String, String)]) -> R<Vec<Option<String>>> {
    let flat: Vec<String> = batch.iter().map(|(_, t)| t.replace('\n', "\\n")).collect();
    let listing: Vec<String> = batch.iter().zip(&flat).enumerate().map(|(i, ((sp, _), t))| format!("{}: {sp} | {t}", i + 1)).collect();
    let user = format!(
        "Translate these {n} consecutive lines. The speaker before '|' is context only.\n\
         Reply with exactly {n} lines formatted '<number>: <English>' and nothing else.\n\n{}",
        listing.join("\n"),
        n = batch.len()
    );
    let out = parse_numbered(&chat.chat(system, &user)?, batch.len());
    let mut result = Vec::new();
    for (i, ((sp, src), f)) in batch.iter().zip(&flat).enumerate() {
        let mut tr = match out.as_ref().map(|o| o[i].clone()) {
            Some(t) if same_markup(markup, src, &t) && !runaway(src, &t) => t,
            // Retry one line on its own when the batch reply was unusable for it.
            _ => chat.chat(system, &format!("Translate this line ({sp} speaking). Reply with the English line only.\n\n{f}"))?,
        };
        tr = tr.replace("\\n", "\n");
        if let Some(rest) = tr.strip_prefix(&format!("{sp} | ")) {
            tr = rest.to_string(); // the model sometimes echoes the context label
        }
        result.push(if same_markup(markup, src, &tr) && !runaway(src, &tr) { Some(tr) } else { translate_pieces(chat, system, markup, sp, src)? });
    }
    Ok(result)
}

/// Last resort when the model keeps changing a line's markup: translate only the text
/// between tags and put the original tags back where they were.
pub fn translate_pieces(chat: &dyn Chat, system: &str, markup: &Regex, sp: &str, src: &str) -> R<Option<String>> {
    let mut parts: Vec<String> = markup.split(src).map(String::from).collect();
    let tags: Vec<&str> = markup.find_iter(src).map(|m| m.as_str()).collect();
    let todo: Vec<usize> = (0..parts.len()).filter(|&i| JA.is_match(&parts[i])).collect();
    if todo.is_empty() {
        return Ok(None);
    }
    let listing: Vec<String> = todo.iter().enumerate().map(|(n, &i)| format!("{}: {}", n + 1, parts[i].replace('\n', "\\n"))).collect();
    let user = format!(
        "This line ({sp} speaking) is split into numbered pieces at its formatting marks:\n{}\n\n\
         Translate each piece so the pieces read naturally in order. Do not add brackets or braces.\n\
         Reply with exactly {} lines formatted '<number>: <English>' and nothing else.\n\n{}",
        src.replace('\n', "\\n"),
        todo.len(),
        listing.join("\n")
    );
    let Some(out) = parse_numbered(&chat.chat(system, &user)?, todo.len()) else { return Ok(None) };
    // one blank piece can be a dropped particle ("[na]は" -> "[na]"); all of them is no translation
    if out.iter().all(|t| t.trim().is_empty()) {
        return Ok(None);
    }
    for (n, (&i, t)) in todo.iter().zip(out).enumerate() {
        let t = t.replace("\\n", "\n");
        // Japanese needs no space between sentences; English does.
        let first = t.chars().next();
        let needs_space = n > 0 && (first.is_some_and(char::is_alphanumeric) || first.is_none_or(|c| "\"'(".contains(c)));
        parts[i] = if needs_space { format!(" {t}") } else { t };
    }
    let rebuilt: String = parts.iter().enumerate().map(|(k, p)| format!("{p}{}", tags.get(k).unwrap_or(&""))).collect();
    Ok(same_markup(markup, src, &rebuilt).then_some(rebuilt))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::text::MARKUP;
    use std::sync::Mutex;

    #[test]
    fn notes_too_long_for_the_prompt_are_noticed() {
        assert!(!notes_fill_prompt(&[]));
        assert!(!notes_fill_prompt(&["a cast of a normal size".repeat(100)]));
        assert!(notes_fill_prompt(&["a whole novel of notes".repeat(600)]));
    }

    #[test]
    fn the_gemma_prompt_follows_its_model_card() {
        let meta = ["[character] Name: Taro".to_string()];
        let history = [("[太郎]: やあ".to_string(), "[Taro]: Hi".to_string())];
        let p = vntl_gemma_prompt(&meta, &history, Some("太郎"), "雨だ");
        assert_eq!(p, "<<METADATA>>\n[character] Name: Taro\n<<TRANSLATE>>\n<<JAPANESE>>\n[太郎]: やあ\n<<ENGLISH>>\n[Taro]: Hi<eos>\n<<JAPANESE>>\n[太郎]: 雨だ\n<<ENGLISH>>\n");
        assert_eq!(vntl_gemma_prompt(&[], &[], None, "雨だ"), "<<TRANSLATE>>\n<<JAPANESE>>\n雨だ\n<<ENGLISH>>\n");
    }

    #[test]
    fn a_gemma_model_gets_its_own_prompt_and_any_other_the_llama_one() {
        assert!(is_gemma("hf.co/lmg-anon/vntl-gemma2-2b-gguf:Q8_0"));
        assert!(!is_gemma(VN_MODEL));
    }

    #[test]
    fn every_text_offer_is_a_vntl_model_and_the_default_is_among_them() {
        assert!(TEXT_OFFERS.iter().all(|o| is_vntl(o.model)));
        assert!(TEXT_OFFERS.iter().any(|o| o.model == VN_MODEL));
        assert!(IMAGE_OFFERS.iter().all(|o| !is_vntl(o.model)));
        assert_eq!(IMAGE_OFFERS[0].model, TEXT_READER);
        // All in one gets only offered models, so each download shows its name and size
        assert!(ALL_IN_ONE.iter().all(|m| TEXT_OFFERS.iter().chain(&IMAGE_OFFERS).any(|o| o.model == *m)));
    }

    #[test]
    fn a_pair_reads_with_one_model_and_writes_with_the_other() {
        let pair = Pair { reads: Fake(|_: &str| Ok("read".to_string())), words: Fake(|_: &str| Ok("words".to_string())) };
        assert_eq!(pair.read_image(b"", "p").unwrap(), "read");
        assert_eq!(pair.chat("", "p").unwrap(), "words");
        assert_eq!(pair.complete("p", 0.0).unwrap(), "words");
    }

    /// Fake model: returns the given replies in order.
    pub struct Scripted(pub Mutex<Vec<&'static str>>);

    impl Scripted {
        pub fn new(replies: &[&'static str]) -> Self {
            Scripted(Mutex::new(replies.iter().rev().copied().collect()))
        }
    }

    impl Chat for Scripted {
        fn chat(&self, _: &str, _: &str) -> R<String> {
            self.0.lock().unwrap().pop().map(String::from).ok_or_else(|| "no scripted reply left".into())
        }
        fn complete(&self, p: &str, _: f64) -> R<String> {
            self.chat("", p)
        }
        fn read_image(&self, _: &[u8], p: &str) -> R<String> {
            self.chat("", p)
        }
    }

    /// Fake model for what Scripted can't do: the closure answers each prompt.
    pub struct Fake<F>(pub F);

    impl<F: Fn(&str) -> R<String> + Sync> Chat for Fake<F> {
        fn chat(&self, _: &str, user: &str) -> R<String> {
            (self.0)(user)
        }
        fn complete(&self, prompt: &str, _: f64) -> R<String> {
            (self.0)(prompt)
        }
        fn read_image(&self, _: &[u8], prompt: &str) -> R<String> {
            (self.0)(prompt)
        }
    }

    fn batch(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn numbered_replies() {
        assert_eq!(parse_numbered("1: Hi\n2: Yo", 2).unwrap(), ["Hi", "Yo"]);
        assert_eq!(parse_numbered("1: Hi", 2), None);
        assert_eq!(parse_numbered("1: Hi\nthere\n2: Yo", 2).unwrap(), ["Hi\\nthere", "Yo"]);
        assert_eq!(parse_numbered("1: Hi\n1: again\nlost\n2: Yo", 2).unwrap(), ["Hi", "Yo"]);
        assert_eq!(parse_numbered("1: 18: Weapon\n2: <number>: Race\n3: 3 apples", 3).unwrap(), ["Weapon", "Race", "3 apples"]);
        assert_eq!(parse_numbered("1: Hi\n2: Yo\n\nLet me know!", 2).unwrap(), ["Hi", "Yo"]);
        assert_eq!(parse_numbered("1: Hi\n\nthere\n2: Yo", 2).unwrap(), ["Hi\\nthere", "Yo"]);
    }

    #[test]
    fn refusals_are_retried_and_dialogue_that_sounds_like_one_is_kept() {
        let refused = "I'm sorry, but I can't help with that.";
        let chat = Scripted::new(&[refused, refused, refused]);
        assert_eq!(translate_batch(&chat, "", &MARKUP, &batch(&[("x", "やめて")])).unwrap(), [None]);
        // one refusal inside a good batch gets the single-line retry
        let chat = Scripted::new(&["1: Hi\n2: I cannot fulfill this request.", "Stop it"]);
        assert_eq!(translate_batch(&chat, "", &MARKUP, &batch(&[("x", "やあ"), ("x", "やめて")])).unwrap(),
                   [Some("Hi".into()), Some("Stop it".into())]);
        let chat = Scripted::new(&["1: Sorry.\n2: I can't.\n3: Sure.\n4: Here's your tea.\n5: ……"]);
        let got = translate_batch(&chat, "", &MARKUP, &batch(&[("x", "ごめん"), ("x", "無理"), ("x", "いいよ"), ("x", "どうぞ"), ("x", "……")])).unwrap();
        assert_eq!(got, ["Sorry.", "I can't.", "Sure.", "Here's your tea.", "……"].map(|t| Some(t.to_string())));
    }

    #[test]
    fn a_rambling_batch_reply_gets_the_single_line_retry() {
        let ramble = "1: Yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes";
        assert_eq!(translate_batch(&Scripted::new(&[ramble, "Yes"]), "", &MARKUP, &batch(&[("x", "はい")])).unwrap(), [Some("Yes".into())]);
    }

    #[test]
    fn vntl_gives_up_on_a_line_only_when_every_attempt_got_a_500() {
        let e500 = "model server: http status: 500".to_string();
        let vntl = |replies: Vec<R<String>>| {
            let replies = Mutex::new(replies);
            translate_vntl(&Fake(|_: &str| replies.lock().unwrap().remove(0)), &MARKUP, &[], &[], None, "やあ")
        };
        assert_eq!(vntl(vec![Err(e500.clone()), Err(e500.clone()), Err(e500.clone())]), Err(e500.clone()));
        assert_eq!(vntl(vec![Err(e500.clone()), Err(e500.clone()), Ok("Hi".into())]).unwrap().as_deref(), Some("Hi"));
        assert_eq!(vntl(vec![Err(e500.clone()), Err(e500.clone()), Ok("  ".into())]).unwrap(), None);
    }

    #[test]
    fn echo_retry_and_tag_fallback() {
        let chat = Scripted::new(&["1: Yumi | Hello\n2: Bye", "Sou | {i}See you{/i}"]);
        let got = translate_batch(&chat, "", &MARKUP, &batch(&[("Yumi", "こんにちは"), ("Sou", "{i}またね{/i}")])).unwrap();
        assert_eq!(got, [Some("Hello".into()), Some("{i}See you{/i}".into())]);
        let chat = Scripted::new(&["1: [sigh] Bye", "[sigh] Bye", "1: Bye"]);
        assert_eq!(translate_batch(&chat, "", &MARKUP, &batch(&[("Sou", "{w}またね")])).unwrap(), [Some("{w}Bye".into())]);
    }

    #[test]
    fn vntl_retries_without_examples_when_markup_breaks() {
        let chat = Scripted::new(&["Hi there", "[Yumi]: {w}Hi there"]);
        let got = translate_vntl(&chat, &MARKUP, &["[character] Name: Yumi (真矢)".into()], &[("あ".into(), "Ah".into())], None, "{w}やあ").unwrap();
        assert_eq!(got.as_deref(), Some("{w}Hi there"));
        assert_eq!(translate_vntl(&Scripted::new(&["x", "y", ""]), &MARKUP, &[], &[], None, "{w}やあ").unwrap(), None);
        // Unity: [brackets] are text, <tags> must stay
        let unity = &crate::text::UNITY_MARKUP;
        assert_eq!(translate_vntl(&Scripted::new(&["[Call] Hello"]), unity, &[], &[], None, "[着信]もしもし").unwrap().as_deref(), Some("[Call] Hello"));
        assert_eq!(translate_vntl(&Scripted::new(&["Hi", "Hi", "<b>Hi</b>"]), unity, &[], &[], None, "<b>やあ</b>").unwrap().as_deref(), Some("<b>Hi</b>"));
    }

    #[test]
    fn vntl_gets_the_speaker_and_loses_its_label_even_on_lines_opening_with_a_variable() {
        let p = vntl_prompt(&[], &[], Some("桜乃"), "[na]、ごめん");
        assert!(p.contains("[桜乃]: [na]、ごめん<|eot_id|>"));
        assert!(!vntl_prompt(&[], &[], None, "やあ").contains("]: "));
        let got = translate_vntl(&Scripted::new(&["[Sakuno]: [na], sorry."]), &MARKUP, &[], &[], Some("桜乃"), "[na]、ごめん").unwrap();
        assert_eq!(got.as_deref(), Some("[na], sorry."));
    }

    #[test]
    fn a_memory_full_of_long_lines_is_cut_before_the_prompt_is_full() {
        let mut history: Vec<_> = (0..6).map(|i| (format!("jp{i}"), "x".repeat(2000))).collect();
        trim_memory(&mut history, &[], 12);
        assert_eq!(history.len(), 3);
        trim_memory(&mut history, &[], 12);
        assert_eq!(history.len(), 3); // the rest stays, so the start of the prompt does too
    }

    #[test]
    fn a_long_memory_gives_way_before_the_character_notes_do() {
        let line = |i: usize| (format!("jp{i}"), "x".repeat(2000));
        let history: Vec<_> = (0..24).map(line).collect();
        let notes = ["[character] Name: Hana (ハナ)".to_string()];
        let p = vntl_prompt(&notes, &history, None, "やあ");
        assert!(p.contains("Hana") && p.contains("jp23") && p.contains("jp19"));
        assert!(!p.contains("jp18\n") && !p.contains("jp18<")); // 5 lines of 2 KB fit in 12 KB, the oldest go
        assert!(p.len() < VNTL_PROMPT_BYTES + 200);
        let short: Vec<_> = (0..24).map(|i| (format!("jp{i}"), "x".to_string())).collect();
        assert!(vntl_prompt(&notes, &short, None, "やあ").contains("jp0<")); // a normal memory is all kept
    }

    #[test]
    fn unity_tags_are_protected_with_chat_models_and_brackets_are_plain_text() {
        let unity = &crate::text::UNITY_MARKUP;
        let chat = Scripted::new(&["1: Hi", "<b>Hi</b>"]); // the batch dropped the tag, the retry kept it
        assert_eq!(translate_batch(&chat, "", unity, &batch(&[("x", "<b>やあ</b>")])).unwrap(), [Some("<b>Hi</b>".into())]);
        let chat = Scripted::new(&["1: [Call] Hello"]);
        assert_eq!(translate_batch(&chat, "", unity, &batch(&[("x", "[着信]もしもし")])).unwrap(), [Some("[Call] Hello".into())]);
    }

    #[test]
    fn pieces_keep_tags_and_spacing() {
        let chat = Scripted::new(&["1: Hello. \n2: [sigh] Bye."]);
        assert_eq!(translate_pieces(&chat, "", &MARKUP, "x", "こんにちは。{w}{i}またね{/i}").unwrap(), None);
        let chat = Scripted::new(&["1: Hello. \n2: Bye."]);
        assert_eq!(translate_pieces(&chat, "", &MARKUP, "x", "こんにちは。{w}{i}またね{/i}").unwrap().unwrap(), "Hello.{w}{i} Bye.{/i}");
        let chat = Scripted::new(&["1: Wait\n2: ...Huh!"]);
        assert_eq!(translate_pieces(&chat, "", &MARKUP, "x", "待って{w}えっ！").unwrap().unwrap(), "Wait{w}...Huh!");
        assert_eq!(translate_pieces(&Scripted::new(&[]), "", &MARKUP, "x", "{w}……").unwrap(), None);
        assert_eq!(translate_pieces(&Scripted::new(&["1:"]), "", &MARKUP, "x", "{i}あ{/i}").unwrap(), None);
        assert_eq!(translate_pieces(&Scripted::new(&["1:\n2: laughed"]), "", &MARKUP, "x", "[na]は{w}笑った").unwrap().unwrap(), "[na]{w} laughed");
    }
}
