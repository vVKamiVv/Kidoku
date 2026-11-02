//! What the app and its models are using right now, for the Model tab. The model server says where
//! each loaded model sits, Windows counts processes, and nvidia-smi knows the graphics card. Whatever
//! can't be read is left out: other cards have no nvidia-smi, and other servers no `/api/ps`.

use serde_json::Value;
use std::time::{Duration, Instant};

const SERVER_NAMES: [&str; 2] = ["ollama", "llama-server"];
const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;
/// The window's state colours, for a meter.
const OK: i32 = 1;
const HIGH: i32 = 2;
const FULL: i32 = 3;
const NORMAL: i32 = 4;

/// One line of the Model tab.
#[derive(Debug, PartialEq)]
pub struct Row {
    pub label: String,
    pub value: String,
    /// How full its meter is, 0 to 1. None draws no meter.
    pub share: Option<f32>,
    pub state: i32,
}

#[derive(Debug, Default, PartialEq)]
pub struct Process {
    pub ram: u64,
    /// Share of the whole machine, as Task Manager shows it. None on the first reading.
    pub cpu: Option<f32>,
}

#[derive(Debug, PartialEq)]
pub struct Model {
    pub name: String,
    pub vram: u64,
    pub ram: u64,
}

#[derive(Debug, PartialEq)]
pub struct Card {
    pub used_mb: u64,
    pub total_mb: u64,
    pub busy: u32,
}

#[derive(Debug, Default, PartialEq)]
pub struct Usage {
    pub app: Process,
    /// None when no model server is running on this PC.
    pub server: Option<Process>,
    /// None when the server can't say.
    pub models: Option<Vec<Model>>,
    pub card: Option<Card>,
}

/// Keeps the last reading: CPU use is the time spent between two of them.
#[derive(Default)]
pub struct Sampler {
    last: Option<(Instant, u64, u64)>,
}

impl Sampler {
    pub fn read(&mut self, url: &str) -> Usage {
        let (app_ram, app_time) = system::measure(std::process::id()).unwrap_or_default();
        let servers: Vec<(u64, u64)> = system::named(&|n| SERVER_NAMES.iter().any(|s| n.starts_with(s))).into_iter().filter_map(system::measure).collect();
        let (server_ram, server_time) = servers.iter().fold((0, 0), |a, m| (a.0 + m.0, a.1 + m.1));
        let now = Instant::now();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
        let share = |was: u64, is: u64, since: Instant| {
            let seconds = now.duration_since(since).as_secs_f32().max(0.001);
            (is.saturating_sub(was) as f32 / 10_000_000.0 / seconds / cores * 100.0).min(100.0)
        };
        let before = self.last.replace((now, app_time, server_time));
        Usage {
            app: Process { ram: app_ram, cpu: before.map(|(at, app, _)| share(app, app_time, at)) },
            server: (!servers.is_empty()).then(|| Process { ram: server_ram, cpu: before.map(|(at, _, server)| share(server, server_time, at)) }),
            models: loaded_models(url),
            card: card(),
        }
    }
}

fn parse_models(reply: &Value) -> Option<Vec<Model>> {
    let number = |v: &Value, key: &str| v[key].as_u64().unwrap_or(0);
    Some(reply["models"].as_array()?.iter().map(|m| {
        let (size, vram) = (number(m, "size"), number(m, "size_vram"));
        Model { name: m["name"].as_str().unwrap_or("model").to_string(), vram, ram: size.saturating_sub(vram) }
    }).collect())
}

fn loaded_models(url: &str) -> Option<Vec<Model>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(2))).build().into();
    let mut reply = agent.get(format!("{}/api/ps", url.trim_end_matches('/'))).call().ok()?;
    parse_models(&reply.body_mut().read_json().ok()?)
}

/// `memory.used, memory.total, utilization.gpu` of the first card.
fn parse_card(line: &str) -> Option<Card> {
    let mut values = line.split(',').map(|v| v.trim().parse::<u64>().ok());
    Some(Card { used_mb: values.next()??, total_mb: values.next()??, busy: values.next()?? as u32 })
}

fn card() -> Option<Card> {
    let mut command = std::process::Command::new("nvidia-smi");
    command.args(["--query-gpu=memory.used,memory.total,utilization.gpu", "--format=csv,noheader,nounits"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // no console window flashing up every reading
    }
    let out = command.output().ok().filter(|o| o.status.success())?;
    parse_card(String::from_utf8_lossy(&out.stdout).lines().next()?)
}

fn amount(bytes: u64) -> String {
    if bytes >= GB { format!("{:.1} GB", bytes as f64 / GB as f64) } else { format!("{} MB", bytes / MB) }
}

/// A load's colour: full deserves a look, and most of the way there a glance.
fn load_state(share: f32) -> i32 {
    if share >= 0.95 { FULL } else if share >= 0.8 { HIGH } else { NORMAL }
}

fn row(label: &str, value: String, share: Option<f32>, state: i32) -> Row {
    Row { label: label.to_string(), value, share, state }
}

fn process_row(label: &str, p: &Process) -> Row {
    let ram = amount(p.ram);
    let share = p.cpu.map(|c| c / 100.0);
    let value = p.cpu.map_or(format!("RAM {ram}"), |c| format!("RAM {ram}  ·  CPU {c:.0}%"));
    row(label, value, share, share.map_or(NORMAL, load_state))
}

/// A model's meter is how much of it is on the card: what spills into RAM runs far slower.
fn model_row(m: &Model) -> Row {
    let on_card = m.vram as f32 / (m.vram + m.ram).max(1) as f32;
    row(short_name(&m.name), format!("VRAM {}  ·  RAM {}", amount(m.vram), amount(m.ram)), Some(on_card), if m.ram == 0 { OK } else { HIGH })
}

/// A long name's last part: "hf.co/someone/model:tag" says little more than "model:tag".
fn short_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// The readings, in the order the Model tab lists them.
pub fn rows(u: &Usage) -> Vec<Row> {
    let mut out = vec![process_row("This app", &u.app)];
    out.push(u.server.as_ref().map_or(row("Model server", "Not running".to_string(), None, NORMAL), |p| process_row("Model server", p)));
    match &u.models {
        Some(models) if models.is_empty() => out.push(row("Models", "None loaded".to_string(), None, NORMAL)),
        Some(models) => out.extend(models.iter().map(model_row)),
        None => {}
    }
    if let Some(c) = &u.card {
        let share = c.used_mb as f32 / c.total_mb.max(1) as f32;
        out.push(row("Graphics card", format!("{} of {} VRAM  ·  {}% busy", amount(c.used_mb * MB), amount(c.total_mb * MB), c.busy), Some(share), load_state(share)));
    }
    out
}

#[cfg(windows)]
mod system {
    use std::ffi::c_void;

    const SNAPSHOT_PROCESSES: u32 = 2;
    const QUERY_LIMITED: u32 = 0x1000;
    const INVALID: *mut c_void = -1isize as *mut c_void;

    #[repr(C)]
    struct Entry {
        size: u32,
        usage: u32,
        pid: u32,
        heap: usize,
        module: u32,
        threads: u32,
        parent: u32,
        priority: i32,
        flags: u32,
        exe: [u16; 260],
    }

    #[repr(C)]
    #[derive(Default)]
    struct Memory {
        size: u32,
        faults: u32,
        peak: usize,
        working_set: usize,
        rest: [usize; 6],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, entry: *mut Entry) -> i32;
        fn Process32NextW(snapshot: *mut c_void, entry: *mut Entry) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetProcessTimes(process: *mut c_void, created: *mut u64, ended: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
        fn K32GetProcessMemoryInfo(process: *mut c_void, memory: *mut Memory, size: u32) -> i32;
    }

    /// The ids of processes whose lower-case file name passes `wanted`.
    pub fn named(wanted: &dyn Fn(&str) -> bool) -> Vec<u32> {
        let mut out = Vec::new();
        // SAFETY: the entry is sized as the call requires, is only read after a call filled it, and
        // the snapshot is closed once, after its last use
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(SNAPSHOT_PROCESSES, 0);
            if snapshot == INVALID {
                return out;
            }
            let mut entry: Entry = std::mem::zeroed();
            entry.size = size_of::<Entry>() as u32;
            let mut more = Process32FirstW(snapshot, &mut entry);
            while more != 0 {
                let end = entry.exe.iter().position(|&c| c == 0).unwrap_or(entry.exe.len());
                if wanted(&String::from_utf16_lossy(&entry.exe[..end]).to_lowercase()) {
                    out.push(entry.pid);
                }
                more = Process32NextW(snapshot, &mut entry);
            }
            CloseHandle(snapshot);
        }
        out
    }

    /// A process's working set in bytes and its CPU time so far in 100 ns ticks.
    pub fn measure(pid: u32) -> Option<(u64, u64)> {
        // SAFETY: the handle is checked before use and closed once; the calls only fill in the
        // out-values, which are all sized for them
        unsafe {
            let process = OpenProcess(QUERY_LIMITED, 0, pid);
            if process.is_null() {
                return None;
            }
            let (mut created, mut ended, mut kernel, mut user) = (0, 0, 0, 0);
            let mut memory = Memory { size: size_of::<Memory>() as u32, ..Default::default() };
            let good = GetProcessTimes(process, &mut created, &mut ended, &mut kernel, &mut user) != 0
                && K32GetProcessMemoryInfo(process, &mut memory, memory.size) != 0;
            CloseHandle(process);
            good.then_some((memory.working_set as u64, kernel + user))
        }
    }
}

#[cfg(not(windows))]
mod system {
    pub fn named(_: &dyn Fn(&str) -> bool) -> Vec<u32> {
        Vec::new()
    }

    pub fn measure(_: u32) -> Option<(u64, u64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_model_is_split_between_the_card_and_memory() {
        let reply = json!({"models": [{"name": "gemma3:12b", "size": 8_000_000_000u64, "size_vram": 5_000_000_000u64}]});
        assert_eq!(parse_models(&reply).unwrap(), [Model { name: "gemma3:12b".into(), vram: 5_000_000_000, ram: 3_000_000_000 }]);
        assert_eq!(parse_models(&json!({"models": []})).unwrap(), []);
        assert_eq!(parse_models(&json!({"error": "no such route"})), None);
    }

    #[test]
    fn the_card_line_is_read_and_a_bad_one_is_dropped() {
        assert_eq!(parse_card("7647, 8192, 16"), Some(Card { used_mb: 7647, total_mb: 8192, busy: 16 }));
        assert_eq!(parse_card("7647, 8192, [N/A]"), None);
    }

    #[test]
    fn rows_say_what_is_known_and_leave_out_the_rest() {
        let usage = Usage {
            app: Process { ram: 120 * MB, cpu: Some(2.4) },
            server: Some(Process { ram: 3 * GB, cpu: None }),
            models: Some(vec![Model { name: "hf.co/someone/model:Q5".into(), vram: 5 * GB, ram: 512 * MB }]),
            card: Some(Card { used_mb: 7168, total_mb: 8192, busy: 16 }),
        };
        let shown = rows(&usage);
        assert_eq!(shown.iter().map(|r| (r.label.as_str(), r.value.as_str())).collect::<Vec<_>>(), [
            ("This app", "RAM 120 MB  ·  CPU 2%"),
            ("Model server", "RAM 3.0 GB"),
            ("model:Q5", "VRAM 5.0 GB  ·  RAM 512 MB"),
            ("Graphics card", "7.0 GB of 8.0 GB VRAM  ·  16% busy"),
        ]);
        let nothing = rows(&Usage::default());
        assert_eq!(nothing[1].value, "Not running");
        assert_eq!(nothing.len(), 2);
        assert_eq!(rows(&Usage { models: Some(Vec::new()), ..Default::default() })[2].value, "None loaded");
    }

    #[test]
    fn meters_show_load_and_how_much_of_a_model_is_on_the_card() {
        let model = |vram, ram| model_row(&Model { name: "m".into(), vram, ram });
        assert_eq!((model(6 * GB, 0).share, model(6 * GB, 0).state), (Some(1.0), OK));
        assert_eq!((model(3 * GB, GB).share, model(3 * GB, GB).state), (Some(0.75), HIGH));
        assert_eq!([load_state(0.5), load_state(0.85), load_state(0.97)], [NORMAL, HIGH, FULL]);
        let quiet = process_row("App", &Process { ram: MB, cpu: None });
        assert_eq!((quiet.share, quiet.state), (None, NORMAL));
    }

    #[test]
    fn this_process_can_be_measured_and_its_cpu_shows_on_the_second_reading() {
        let mut s = Sampler::default();
        let url = "http://127.0.0.1:1";
        let first = s.read(url);
        assert!(first.app.ram > 0 && first.app.cpu.is_none());
        assert!(s.read(url).app.cpu.is_some());
    }
}
