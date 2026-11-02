//! Installing Kidoku for this Windows user. The installer is the app itself under another name, so
//! there's one program to build: it copies itself to a folder of the user's, adds shortcuts and an
//! entry in Installed apps, and needs no administrator rights.

use crate::R;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const EXE: &str = "Kidoku.exe";
pub const UNINSTALL_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kidoku";
pub const OLLAMA_PAGE: &str = "https://ollama.com/download";
const SHORTCUT: &str = "Kidoku.lnk";
/// No console flashing up behind the window.
const NO_WINDOW: u32 = 0x0800_0000;

/// What to do beyond copying the program.
pub struct Choices {
    pub folder: PathBuf,
    pub menu: bool,
    pub desktop: bool,
    pub register: bool,
}

pub fn default_folder() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Programs").join("Kidoku")
}

/// A Windows tool by its full path: Setup runs from a downloads folder, where a file of the same name
/// could be sitting.
fn system(name: &str) -> Command {
    let mut c = Command::new(PathBuf::from(std::env::var_os("SystemRoot").unwrap_or("C:\\Windows".into())).join("System32").join(name));
    c.creation_flags(NO_WINDOW);
    c
}

fn run(mut c: Command) -> R<()> {
    let out = c.output().map_err(|e| format!("{:?}: {e}", c.get_program()))?;
    let said = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    if out.status.success() { Ok(()) } else { Err(format!("{} {}", said(&out.stderr), said(&out.stdout)).trim().to_string()) }
}

/// Where a shortcut goes, as PowerShell finds it: the desktop can live in OneDrive.
enum Place<'a> {
    Menu,
    Desktop,
    #[cfg_attr(not(test), allow(dead_code))]
    Folder(&'a Path),
}

fn quoted(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}

impl Place<'_> {
    fn script(&self) -> String {
        match self {
            Place::Menu => format!("(Join-Path ([Environment]::GetFolderPath('Programs')) '{SHORTCUT}')"),
            Place::Desktop => format!("(Join-Path ([Environment]::GetFolderPath('Desktop')) '{SHORTCUT}')"),
            Place::Folder(p) => format!("(Join-Path {} '{SHORTCUT}')", quoted(p)),
        }
    }
}

fn powershell(script: String) -> R<()> {
    let mut c = system(r"WindowsPowerShell\v1.0\powershell.exe");
    c.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    run(c)
}

fn add_shortcut(place: &Place, program: &Path) -> R<()> {
    let target = quoted(program);
    let folder = quoted(program.parent().unwrap_or(Path::new("")));
    powershell(format!("$s=(New-Object -ComObject WScript.Shell).CreateShortcut({});$s.TargetPath={target};$s.WorkingDirectory={folder};$s.Save()", place.script()))
}

fn remove_shortcut(place: &Place) -> R<()> {
    // Remove-Item fails on a file that isn't there, and most people have only one of the two shortcuts
    powershell(format!("$p={};if(Test-Path -LiteralPath $p){{Remove-Item -LiteralPath $p}}", place.script()))
}

fn reg_value(key: &str, name: &str, kind: &str, value: &str) -> R<()> {
    let mut c = system("reg.exe");
    c.args(["add", key, "/v", name, "/t", kind, "/d", value, "/f"]);
    run(c)
}

fn register(key: &str, program: &Path) -> R<()> {
    let folder = program.parent().unwrap_or(Path::new(""));
    let size = fs::metadata(program).map_err(|e| e.to_string())?.len() / 1024;
    for (name, value) in [
        ("DisplayName", "Kidoku".to_string()),
        ("DisplayVersion", env!("CARGO_PKG_VERSION").to_string()),
        ("Publisher", "Kidoku".to_string()),
        ("DisplayIcon", program.display().to_string()),
        ("InstallLocation", folder.display().to_string()),
        ("UninstallString", format!("\"{}\" --uninstall", program.display())),
    ] {
        reg_value(key, name, "REG_SZ", &value)?;
    }
    reg_value(key, "EstimatedSize", "REG_DWORD", &size.to_string())?;
    reg_value(key, "NoModify", "REG_DWORD", "1")?;
    reg_value(key, "NoRepair", "REG_DWORD", "1")
}

/// Copies this program to `folder`, whole or not at all, and does what `choices` ask. Returns the
/// installed program.
pub fn install(from: &Path, choices: &Choices) -> R<PathBuf> {
    let err = |e: std::io::Error| format!("{}: {e}", choices.folder.display());
    fs::create_dir_all(&choices.folder).map_err(err)?;
    let to = choices.folder.join(EXE);
    let part = to.with_extension("part");
    fs::copy(from, &part).map_err(err)?;
    if let Err(e) = fs::rename(&part, &to) {
        let _ = fs::remove_file(&part);
        return Err(format!("{}: {e}. If Kidoku is open, close it and try again.", to.display()));
    }
    if choices.menu {
        add_shortcut(&Place::Menu, &to)?;
    }
    if choices.desktop {
        add_shortcut(&Place::Desktop, &to)?;
    }
    if choices.register {
        register(UNINSTALL_KEY, &to)?;
    }
    Ok(to)
}

/// Takes the shortcuts and the entry away and, if asked, what the app downloaded for itself. The
/// program can't delete itself while it runs, so a small command does that just after it has closed.
/// Only our own file goes, and the folder only if that leaves it empty. Translations inside games
/// aren't touched: they have Remove translation.
pub fn uninstall(program: &Path, with_downloads: bool) -> R<()> {
    remove_shortcut(&Place::Menu)?;
    remove_shortcut(&Place::Desktop)?;
    let mut c = system("reg.exe");
    c.args(["delete", UNINSTALL_KEY, "/f"]);
    let _ = run(c); // not there if only the files were installed
    if with_downloads {
        let data = crate::app_folder("");
        if data.exists() {
            fs::remove_dir_all(&data).map_err(|e| format!("{}: {e}", data.display()))?;
        }
    }
    let folder = program.parent().unwrap_or(Path::new(""));
    let mut c = system("cmd.exe");
    c.current_dir(std::env::temp_dir());
    c.raw_arg(format!("/c \"ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"\"", program.display(), folder.display()));
    c.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Whether Ollama is on this PC: where its installer puts it, or on the path.
pub fn ollama_found() -> bool {
    let own = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Programs").join("Ollama");
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::iter::once(own).chain(std::env::split_paths(&path)).any(|d| d.join("ollama.exe").is_file())
}

pub fn open_page(url: &str) {
    let mut c = system("rundll32.exe");
    c.args(["url.dll,FileProtocolHandler", url]);
    let _ = c.spawn();
}

/// Starts the installed program, on its own.
pub fn start(program: &Path) -> R<()> {
    Command::new(program).current_dir(program.parent().unwrap_or(Path::new(""))).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl setup's {name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn files_only(folder: PathBuf) -> Choices {
        Choices { folder, menu: false, desktop: false, register: false }
    }

    #[test]
    fn the_program_goes_in_under_its_own_name_and_a_new_version_replaces_it() {
        let dir = temp("copy");
        let from = dir.join("Kidoku-Setup.exe");
        fs::write(&from, "first").unwrap();
        let choices = files_only(dir.join("nested").join("Kidoku"));
        let to = install(&from, &choices).unwrap();
        assert_eq!(to, choices.folder.join(EXE));
        assert_eq!(fs::read(&to).unwrap(), b"first");
        fs::write(&from, "second").unwrap();
        install(&from, &choices).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"second");
        assert_eq!(fs::read_dir(&choices.folder).unwrap().count(), 1, "nothing left over");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_program_that_is_missing_stops_the_install_with_no_part_left() {
        let dir = temp("missing");
        let choices = files_only(dir.join("Kidoku"));
        assert!(install(&dir.join("gone.exe"), &choices).is_err());
        assert_eq!(fs::read_dir(&choices.folder).unwrap().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_shortcut_survives_a_folder_with_a_quote_and_japanese_in_its_name() {
        let dir = temp("ショートカット");
        let program = dir.join(EXE);
        fs::write(&program, "x").unwrap();
        add_shortcut(&Place::Folder(&dir), &program).unwrap();
        assert!(dir.join(SHORTCUT).is_file());
        remove_shortcut(&Place::Folder(&dir)).unwrap();
        assert!(!dir.join(SHORTCUT).exists());
        remove_shortcut(&Place::Folder(&dir)).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_entry_says_how_to_uninstall() {
        let dir = temp("entry");
        let program = dir.join(EXE);
        fs::write(&program, vec![0; 4096]).unwrap();
        let key = format!(r"HKCU\Software\Kidoku-test-{}", std::process::id());
        register(&key, &program).unwrap();
        let out = system("reg.exe").args(["query", &key]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut c = system("reg.exe");
        c.args(["delete", &key, "/f"]);
        run(c).unwrap();
        assert!(text.contains("DisplayName") && text.contains("Kidoku"), "{text}");
        assert!(text.contains(&format!("\"{}\" --uninstall", program.display())), "{text}");
        assert!(text.contains("EstimatedSize") && text.contains("0x4"), "{text}");
        fs::remove_dir_all(dir).unwrap();
    }
}
