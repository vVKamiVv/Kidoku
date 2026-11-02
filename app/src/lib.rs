//! vntl - translate Japanese visual novels with a local LLM.
//!
//! Ren'Py: lines are dumped by the game's own engine (no decompiling), translated through any
//! OpenAI-compatible server (Ollama by default) and written as a normal Ren'Py translation in
//! game/tl/<lang>/vntl_*.rpy. Original files are never modified; delete those vntl_* files to go
//! back to Japanese. Every other engine has its text files rewritten instead, with the originals kept
//! next to them as *.vntl-orig. Progress is cached in <game>/vntl_work, so
//! translation can be stopped and resumed.

pub mod advhd;
pub mod artemis;
pub mod bgi;
pub mod blowfish;
pub mod catsystem;
pub mod code;
pub mod fit;
pub mod fonts;
pub mod fvp;
pub mod kirikiri;
pub mod lcf;
pub mod liar;
pub mod llm;
pub mod majiro;
pub mod malie;
pub mod menus;
pub mod nscripter;
pub mod ocr;
pub mod pipeline;
pub mod qlie;
pub mod reallive;
pub mod reference;
pub mod renpy;
pub mod siglus;
pub mod softpal;
pub mod rgss;
pub mod rpgmaker;
#[cfg(windows)]
pub mod setup;
pub mod text;
pub mod tyrano;
pub mod unity;
#[cfg(windows)]
pub mod update;
pub mod usage;
pub mod vndb;
pub mod wolf;
pub mod yuris;

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub type R<T> = Result<T, String>;

/// Where the game's own copy of a file we rewrite is kept.
pub(crate) fn backup(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.vntl-orig", p.display()))
}

/// Marks `p` as a file of ours that the game never had, so Remove translation takes it away.
pub(crate) fn ours(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.vntl-new", p.display()))
}

/// Where the sum of what we last wrote to a game file is kept, to tell a game update from our work.
pub(crate) fn stamp(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.vntl-sum", p.display()))
}

fn sum(bytes: &[u8]) -> String {
    let mut crc = flate2::Crc::new();
    crc.update(bytes);
    format!("{} {}", bytes.len(), crc.sum())
}

/// Whether something else replaced `p` after we wrote it, so that its backup is an older game.
/// Getting the backup's own bytes back doesn't count: that is Steam's "verify" undoing our file.
/// shortcut: a backup without a stamp (made by an older build) is trusted, update or not; nothing
/// else tells such a file from our own, so this holds until that file's next write.
fn updated(p: &Path) -> bool {
    let Ok(stamped) = fs::read_to_string(stamp(p)) else { return false };
    let Ok(now) = fs::read(p) else { return false };
    stamped != sum(&now) && fs::read(backup(p)).is_ok_and(|b| b != now)
}

/// The game's own file, also after we replaced it.
pub(crate) fn original(p: &Path) -> R<Vec<u8>> {
    if updated(p) {
        return Err(format!(
            "{}: changed since Kidoku wrote it, probably by a game update. Use Remove translation, then \
             translate again (with Start over for a Liar-soft game, whose saved lines go by position).",
            p.display()
        ));
    }
    let b = backup(p);
    fs::read(if b.exists() { &b } else { p }).map_err(|e| format!("{}: {e}", p.display()))
}

/// Disc installs leave game files read-only, which stops Windows replacing or opening them.
#[allow(clippy::permissions_set_readonly_false)]
fn make_writable(p: &Path) -> std::io::Result<()> {
    match fs::metadata(p) {
        Ok(meta) if meta.permissions().readonly() => {
            let mut perms = meta.permissions();
            perms.set_readonly(false);
            fs::set_permissions(p, perms)
        }
        _ => Ok(()),
    }
}

/// `to` all at once or not at all: `fill` makes it under a temp name, which is then renamed.
fn swap_in(to: &Path, fill: impl FnOnce(&Path) -> std::io::Result<()>) -> std::io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", to.display()));
    let done = fill(&tmp).and_then(|_| make_writable(to)).and_then(|_| fs::rename(&tmp, to));
    if done.is_err() {
        _ = fs::remove_file(&tmp);
    }
    done
}

pub(crate) fn write_whole(to: &Path, bytes: &[u8]) -> std::io::Result<()> {
    swap_in(to, |tmp| fs::File::create(tmp).and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all())))
}

/// Keep the game's own copy of `p` as *.vntl-orig, the first time. It appears whole or not at all:
/// restoring a copy that a full disk or a crash had cut short would ruin the game.
fn keep_original(p: &Path) -> std::io::Result<()> {
    if backup(p).exists() {
        return Ok(());
    }
    swap_in(&backup(p), |tmp| {
        // a copy keeps the read-only mark of a disc install, which stops it being flushed or redone
        make_writable(tmp)?;
        fs::copy(p, tmp)?;
        make_writable(tmp)?;
        fs::OpenOptions::new().write(true).open(tmp)?.sync_all()
    })
}

/// Replace a game file with `bytes`, keeping the game's own copy as *.vntl-orig the first time. An
/// interruption never leaves half of either. The stamp is gone during the swap: a missing one only
/// skips the update check, while a stale one would call our own file a game update.
pub(crate) fn replace_file(p: &Path, bytes: &[u8]) -> R<()> {
    let err = |e: std::io::Error| format!("{}: {e}", p.display());
    keep_original(p).map_err(err)?;
    if stamp(p).exists() {
        fs::remove_file(stamp(p)).map_err(err)?;
    }
    write_whole(p, bytes).map_err(err)?;
    _ = write_whole(&stamp(p), sum(bytes).as_bytes());
    Ok(())
}

/// For archives too big to read whole: `new_head` over the start of the file and `tail` on the end
/// of it, with the game's own file (`len` bytes, starting with `old_head`) as the base, so a second
/// run starts from the original. The head goes last: a run cut short leaves a game that still plays.
pub(crate) fn extend_file(p: &Path, len: u64, old_head: &[u8], new_head: &[u8], tail: &[u8]) -> R<()> {
    let err = |e: std::io::Error| format!("{}: {e}", p.display());
    keep_original(p).map_err(err)?;
    make_writable(p).map_err(err)?;
    let mut f = fs::OpenOptions::new().write(true).open(p).map_err(err)?;
    f.write_all(old_head).map_err(err)?;
    f.set_len(len).map_err(err)?;
    f.seek(SeekFrom::Start(len)).map_err(err)?;
    f.write_all(tail).map_err(err)?;
    f.sync_all().map_err(err)?;
    f.seek(SeekFrom::Start(0)).map_err(err)?;
    f.write_all(new_head).map_err(err)?;
    f.sync_all().map_err(err)
}

/// Where the app keeps what it downloads for itself.
pub(crate) fn app_folder(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("VN Translator").join(name)
}

/// Whether `files`, as `(file, address, exact size)`, are all in `folder`.
pub(crate) fn have_files(folder: &Path, files: &[(String, String, u64)]) -> bool {
    files.iter().all(|(file, _, size)| fs::metadata(folder.join(file)).is_ok_and(|m| m.len() == *size))
}

/// Download `files`, calling `progress` with bytes done and the total. A file only takes its name
/// once all of it has arrived.
pub(crate) fn download_files(folder: &Path, files: &[(String, String, u64)], progress: &dyn Fn(u64, u64)) -> R<()> {
    let total = files.iter().map(|f| f.2).sum();
    fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    let mut done = 0;
    for (file, url, size) in files {
        let path = folder.join(file);
        if fs::metadata(&path).is_ok_and(|m| m.len() == *size) {
            done += size;
            continue;
        }
        let part = path.with_extension("part");
        let mut resp = ureq::get(url).call().map_err(|e| format!("{file}: {e}"))?;
        let mut body = resp.body_mut().with_config().limit(size + 1).reader();
        let mut out = fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        let (mut buf, mut got) = (vec![0; 1 << 16], 0);
        loop {
            let n = body.read(&mut buf).map_err(|e| format!("{file}: {e}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| format!("{}: {e}", part.display()))?;
            got += n as u64;
            progress(done + got, total);
        }
        drop(out);
        if got != *size {
            let _ = fs::remove_file(&part);
            return Err(format!("{file}: the download stopped at {got} of {size} bytes"));
        }
        fs::rename(&part, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        done += size;
    }
    Ok(())
}

/// The 32-bit number at `p`, in the little-endian order of every engine's files.
pub(crate) fn word(b: &[u8], p: usize) -> usize {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) as usize
}

pub(crate) fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for p in fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()) {
        if p.is_dir() {
            files_under(&p, out)
        } else {
            out.push(p)
        }
    }
}

/// Our backups (*.vntl-orig), and marks of files we added, anywhere in the game folder.
pub fn backups(game: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    files_under(game, &mut all);
    all.retain(|p| p.to_string_lossy().ends_with(".vntl-orig") || p.to_string_lossy().ends_with(".vntl-new"));
    all
}

/// Put the game's original files back. Returns how many were restored, and how many stayed because a
/// game update changed them; their older originals move to *.vntl-orig.old-N, never deleted.
pub fn restore(game: &Path) -> R<(usize, usize)> {
    let (mut restored, mut kept) = (0, 0);
    for b in backups(game) {
        let name = b.to_string_lossy();
        if let Some(added) = name.strip_suffix(".vntl-new") {
            _ = fs::remove_file(added);
            fs::remove_file(&b).map_err(|e| format!("{name}: {e}"))?;
            restored += 1;
            continue;
        }
        let p = Path::new(name.strip_suffix(".vntl-orig").unwrap());
        let to = if updated(p) {
            kept += 1;
            (1..).map(|n| PathBuf::from(format!("{name}.old-{n}"))).find(|o| !o.exists()).unwrap()
        } else {
            restored += 1;
            p.to_path_buf()
        };
        make_writable(&to).and_then(|_| fs::rename(&b, to)).map_err(|e| format!("{name}: {e}"))?;
        _ = fs::remove_file(stamp(p));
    }
    Ok((restored, kept))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vntl_lib_{name}_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Answers one request on this machine with `body`, and gives the address.
    pub(crate) fn serve(body: &'static [u8]) -> String {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("http://{}/file", server.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut peer, _) = server.accept().unwrap();
            let mut request = [0; 2048];
            let _ = peer.read(&mut request);
            let _ = peer.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes());
            let _ = peer.write_all(body);
        });
        at
    }

    #[test]
    fn a_download_takes_its_name_only_when_it_is_all_there() {
        let dir = temp_dir("download");
        let whole = [("a.bin".to_string(), serve(b"12345"), 5)];
        assert!(!have_files(&dir, &whole));
        download_files(&dir, &whole, &|_, _| {}).unwrap();
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), b"12345");
        assert!(have_files(&dir, &whole));
        download_files(&dir, &whole, &|_, _| {}).unwrap(); // the server has gone: nothing was fetched

        let short = [("b.bin".to_string(), serve(b"123"), 5)];
        assert!(download_files(&dir, &short, &|_, _| {}).unwrap_err().contains("stopped at 3 of 5"));
        assert!(!dir.join("b.bin").exists() && !dir.join("b.part").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_big_file_keeps_a_whole_backup_whatever_a_crash_left() {
        let dir = temp_dir("extend");
        let f = dir.join("data.xp3");
        let tmp = format!("{}.tmp", backup(&f).display());
        fs::write(&f, "head body").unwrap();
        fs::write(&tmp, "he").unwrap(); // a copy a crash cut short
        extend_file(&f, 9, b"head", b"HEAD", b"+tail").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"HEAD body+tail");
        assert_eq!(fs::read(backup(&f)).unwrap(), b"head body");
        assert!(!Path::new(&tmp).exists());
        extend_file(&f, 9, b"head", b"AGAIN", b"+more").unwrap();
        assert_eq!(fs::read(backup(&f)).unwrap(), b"head body", "only the first run keeps a copy");
        restore(&dir).unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"head body");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_only_game_files_can_be_replaced_and_restored() {
        let dir = temp_dir("read_only");
        let f = dir.join("rio.arc");
        fs::write(&f, "orig").unwrap();
        let mut perms = fs::metadata(&f).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&f, perms).unwrap();
        replace_file(&f, b"one").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"one");
        assert_eq!(restore(&dir), Ok((1, 0)));
        assert_eq!(fs::read(&f).unwrap(), b"orig");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_first_original_survives_rewrites_and_comes_back() {
        let dir = temp_dir("restore");
        let f = dir.join("scr.xfl");
        fs::write(&f, "orig").unwrap();
        replace_file(&f, b"one").unwrap();
        replace_file(&f, b"two").unwrap();
        assert_eq!(fs::read(backup(&f)).unwrap(), b"orig");
        assert_eq!(fs::read(&f).unwrap(), b"two");
        assert_eq!(original(&f).unwrap(), b"orig");
        assert_eq!(restore(&dir), Ok((1, 0)));
        assert_eq!(fs::read(&f).unwrap(), b"orig");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1); // no backup, stamp or temp file left
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_backup_cut_off_by_a_crash_is_not_the_original() {
        let dir = temp_dir("cut");
        let f = dir.join("scr.xfl");
        fs::write(&f, "whole file").unwrap();
        fs::write(format!("{}.tmp", backup(&f).display()), "whole").unwrap();
        assert!(backups(&dir).is_empty());
        assert_eq!(original(&f).unwrap(), b"whole file");
        replace_file(&f, b"new").unwrap();
        assert_eq!(fs::read(backup(&f)).unwrap(), b"whole file");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_write_keeps_the_game_file_and_no_temp_file() {
        let dir = temp_dir("failed");
        let f = dir.join("scr.xfl");
        fs::write(&f, "orig").unwrap();
        fs::create_dir(format!("{}.tmp", backup(&f).display())).unwrap(); // the backup can't be written
        assert!(replace_file(&f, b"new").is_err());
        assert_eq!(fs::read(&f).unwrap(), b"orig");
        assert!(!backup(&f).exists());
        let occupied = dir.join("sub");
        fs::create_dir_all(occupied.join("x")).unwrap(); // the rename can't happen
        assert!(write_whole(&occupied, b"new").is_err());
        assert!(!dir.join("sub.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_game_update_stops_the_run_and_is_never_overwritten() {
        let dir = temp_dir("update");
        let f = dir.join("Map001.json");
        fs::write(&f, "v1").unwrap();
        replace_file(&f, b"v1 translated").unwrap();

        fs::write(&f, "v1").unwrap(); // Steam's verify: the game's own file again, nothing to report
        assert_eq!(original(&f).unwrap(), b"v1");
        replace_file(&f, b"v1 translated").unwrap();

        fs::write(&f, "v2").unwrap();
        assert!(original(&f).unwrap_err().contains("game update"));
        assert_eq!(restore(&dir), Ok((0, 1)));
        assert_eq!(fs::read(&f).unwrap(), b"v2");
        assert_eq!(fs::read(format!("{}.old-1", backup(&f).display())).unwrap(), b"v1");
        assert_eq!(original(&f).unwrap(), b"v2");

        replace_file(&f, b"v2 translated").unwrap();
        fs::write(&f, "v3").unwrap();
        assert_eq!(restore(&dir), Ok((0, 1)));
        assert_eq!(fs::read(format!("{}.old-1", backup(&f).display())).unwrap(), b"v1");
        assert_eq!(fs::read(format!("{}.old-2", backup(&f).display())).unwrap(), b"v2");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_backup_from_before_stamps_is_trusted() {
        let dir = temp_dir("old");
        let f = dir.join("scr.xfl");
        fs::write(&f, "translated").unwrap();
        fs::write(backup(&f), "orig").unwrap();
        assert_eq!(original(&f).unwrap(), b"orig");
        assert_eq!(restore(&dir), Ok((1, 0)));
        fs::remove_dir_all(&dir).unwrap();
    }
}
