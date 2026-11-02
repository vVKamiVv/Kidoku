//! Updating an installed copy. The app asks GitHub for the newest release and says so if there's a
//! newer one. Only when the user agrees does it fetch that release's Setup, check it against the sum
//! GitHub publishes, and hand over: a running program can't replace itself, so the new one does that
//! once this one has closed. No game text or anything else of the user's goes along.

use crate::setup::{self, Choices};
use crate::{download_files, R};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LATEST: &str = "https://api.github.com/repos/vVKamiVv/Kidoku/releases/latest";
/// Where a release's page and files have to be: a reply can't send us anywhere else.
const RELEASES: &str = "https://github.com/vVKamiVv/Kidoku/releases/";
const ASSET: &str = "Kidoku-Setup.exe";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Half-second tries for the old program to close, which only takes a moment.
const TRIES: usize = 60;

/// The newest release, as far as updating needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub page: String,
    url: String,
    size: u64,
    sum: String,
}

fn numbers(version: &str) -> Option<Vec<u32>> {
    version.trim_start_matches('v').split('.').map(|n| n.parse().ok()).collect()
}

/// Whether `version` comes after `current`. One that can't be read never does.
fn newer(version: &str, current: &str) -> bool {
    matches!((numbers(version), numbers(current)), (Some(a), Some(b)) if a > b)
}

/// A release as GitHub describes it. None without a Setup, a sum, or an address of ours.
pub fn read(text: &str) -> Option<Release> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let asset = v["assets"].as_array()?.iter().find(|a| a["name"] == ASSET)?;
    let page = v["html_url"].as_str().filter(|p| p.starts_with(RELEASES))?.to_string();
    let url = asset["browser_download_url"].as_str().filter(|u| u.starts_with(RELEASES))?.to_string();
    let sum = asset["digest"].as_str()?.strip_prefix("sha256:")?.to_lowercase();
    Some(Release { version: v["tag_name"].as_str()?.trim_start_matches('v').to_string(), page, url, size: asset["size"].as_u64()?, sum })
}

/// A release newer than this copy, if there is one.
pub fn check() -> R<Option<Release>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into();
    let request = agent.get(LATEST).header("User-Agent", format!("Kidoku/{CURRENT}")).header("Accept", "application/vnd.github+json");
    let text = request.call().map_err(|e| e.to_string())?.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let release = read(&text).ok_or("GitHub's answer has no Setup in it")?;
    Ok(newer(&release.version, CURRENT).then_some(release))
}

/// The program, when this copy is an installed one. A build run from its own folder has git instead.
pub fn installed_program() -> Option<PathBuf> {
    std::env::current_exe().ok().filter(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(setup::EXE)))
}

fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be,
        0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa,
        0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85,
        0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f,
        0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut tail = data[data.len() / 64 * 64..].to_vec();
    tail.push(0x80);
    tail.resize((tail.len() + 8).next_multiple_of(64) - 8, 0);
    tail.extend((data.len() as u64 * 8).to_be_bytes());
    for block in data.as_chunks::<64>().0.iter().chain(tail.as_chunks::<64>().0) {
        let mut w = [0u32; 64];
        for i in 0..64 {
            w[i] = if i < 16 {
                u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap())
            } else {
                let (a, b) = (w[i - 15], w[i - 2]);
                w[i - 16]
                    .wrapping_add(a.rotate_right(7) ^ a.rotate_right(18) ^ (a >> 3))
                    .wrapping_add(w[i - 7])
                    .wrapping_add(b.rotate_right(17) ^ b.rotate_right(19) ^ (b >> 10))
            };
        }
        let mut s = h;
        for i in 0..64 {
            let one = s[7]
                .wrapping_add(s[4].rotate_right(6) ^ s[4].rotate_right(11) ^ s[4].rotate_right(25))
                .wrapping_add((s[4] & s[5]) ^ (!s[4] & s[6]))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let two = (s[0].rotate_right(2) ^ s[0].rotate_right(13) ^ s[0].rotate_right(22)).wrapping_add((s[0] & s[1]) ^ (s[0] & s[2]) ^ (s[1] & s[2]));
            s = [one.wrapping_add(two), s[0], s[1], s[2], s[3].wrapping_add(one), s[4], s[5], s[6]];
        }
        for (sum, v) in h.iter_mut().zip(s) {
            *sum = sum.wrapping_add(v);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

fn work_folder() -> PathBuf {
    std::env::temp_dir().join("Kidoku-update")
}

/// What an update left behind, once it's done.
pub fn tidy() {
    let _ = fs::remove_dir_all(work_folder());
}

fn fetch_into(folder: &Path, release: &Release, progress: &dyn Fn(u64, u64)) -> R<PathBuf> {
    let _ = fs::remove_dir_all(folder);
    download_files(folder, &[(ASSET.to_string(), release.url.clone(), release.size)], progress)?;
    let path = folder.join(ASSET);
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if sha256(&bytes) != release.sum {
        let _ = fs::remove_dir_all(folder);
        return Err("The update didn't match its checksum, so it wasn't used.".into());
    }
    Ok(path)
}

/// The new Setup, downloaded and checked.
pub fn fetch(release: &Release, progress: &dyn Fn(u64, u64)) -> R<PathBuf> {
    fetch_into(&work_folder(), release, progress)
}

/// Starts the new Setup, which takes over `program` as soon as this copy closes.
pub fn hand_over(setup_file: &Path, program: &Path) -> R<()> {
    let folder = program.parent().ok_or("the program has no folder")?;
    std::process::Command::new(setup_file).arg("--update").arg(folder).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// What the new Setup does: waits for the old program to close, then takes its place. The entry in
/// Installed apps follows, if it's this copy's. Shortcuts already point here.
pub fn replace(from: &Path, folder: &Path) -> R<PathBuf> {
    let old = folder.join(setup::EXE);
    let open = || old.exists() && fs::OpenOptions::new().write(true).open(&old).is_err();
    for _ in 0..TRIES {
        if !open() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let register = setup::installed_folder().is_some_and(|f| f == folder);
    setup::install(from, &Choices { folder: folder.to_path_buf(), menu: false, desktop: false, register })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOWNLOAD: &str = "https://github.com/vVKamiVv/Kidoku/releases/download/v9.0.0/Kidoku-Setup.exe";

    fn reply(url: &str, digest: &str) -> String {
        serde_json::json!({
            "tag_name": "v9.0.0", "html_url": "https://github.com/vVKamiVv/Kidoku/releases/tag/v9.0.0",
            "assets": [{"name": "vntl.exe", "browser_download_url": "x", "size": 1, "digest": "sha256:00"},
                       {"name": ASSET, "browser_download_url": url, "size": 12, "digest": digest}],
        }).to_string()
    }

    #[test]
    fn versions_go_by_number_and_not_by_letter() {
        assert!(newer("v0.1.10", "0.1.9") && newer("0.2.0", "0.1.9") && newer("1.0", "0.9.9"));
        assert!(!newer("0.1.1", "0.1.1") && !newer("v0.1.0", "0.1.1"));
        assert!(!newer("nightly", "0.1.1"), "what can't be read isn't newer");
    }

    #[test]
    fn a_release_needs_a_setup_with_a_sum_at_an_address_of_ours() {
        let got = read(&reply(DOWNLOAD, "sha256:ABCD")).unwrap();
        assert_eq!((got.version.as_str(), got.url.as_str(), got.size, got.sum.as_str()), ("9.0.0", DOWNLOAD, 12, "abcd"));
        assert!(read(&reply("https://example.com/Kidoku-Setup.exe", "sha256:ab")).is_none(), "somewhere else");
        assert!(read(&reply(DOWNLOAD, "md5:ab")).is_none(), "no sum to check against");
        assert!(read("{\"tag_name\": \"v9.0.0\", \"assets\": []}").is_none());
        assert!(read("not json").is_none());
    }

    #[test]
    fn sha256_matches_the_published_examples() {
        assert_eq!(sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(sha256(long), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        assert_eq!(sha256(&[b'a'; 1000]), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
    }

    fn release(url: String, data: &[u8], sum: String) -> Release {
        Release { version: "9.0.0".into(), page: String::new(), url, size: data.len() as u64, sum: if sum.is_empty() { sha256(data) } else { sum } }
    }

    #[test]
    fn only_a_download_that_matches_its_sum_is_kept() {
        let dir = std::env::temp_dir().join(format!("vntl_update_{}", std::process::id()));
        let good = release(crate::tests::serve(b"new program"), b"new program", String::new());
        assert_eq!(fs::read(fetch_into(&dir, &good, &|_, _| {}).unwrap()).unwrap(), b"new program");
        let bad = release(crate::tests::serve(b"new program"), b"new program", "00".repeat(32));
        assert!(fetch_into(&dir, &bad, &|_, _| {}).unwrap_err().contains("checksum"));
        assert!(!dir.exists(), "nothing of it stays");
    }

    #[test]
    fn the_new_program_takes_the_old_ones_place() {
        let dir = std::env::temp_dir().join(format!("vntl_replace_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(setup::EXE), "old").unwrap();
        let from = dir.join(ASSET);
        fs::write(&from, "new").unwrap();
        let to = replace(&from, &dir).unwrap();
        assert_eq!(fs::read(to).unwrap(), b"new");
        fs::remove_dir_all(dir).unwrap();
    }
}
