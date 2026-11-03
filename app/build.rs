use std::{env, fs, path::Path, process::Command, time::SystemTime};

fn main() {
    // fluent-dark: the Windows 11 look, dark.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    // what the About post shows: the commit built from, and the day, in UTC
    let commit = Command::new("git").args(["rev-parse", "--short", "HEAD"]).output().ok().filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    println!("cargo:rustc-env=KIDOKU_COMMIT={commit}");
    let days = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86_400) as i64;
    println!("cargo:rustc-env=KIDOKU_BUILT={}", date(days));
    println!("cargo:rerun-if-changed=../.git/logs/HEAD"); // a new commit

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let res = Path::new(&env::var("OUT_DIR").unwrap()).join("icon.res");
        fs::write(&res, icon_resources(&fs::read("ui/app-icon.ico").unwrap())).unwrap();
        println!("cargo:rustc-link-arg-bin=vntl-gui={}", res.display());
        println!("cargo:rerun-if-changed=ui/app-icon.ico");
    }
}

/// Days since 1970 as a calendar date (Howard Hinnant's method).
fn date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (day, month) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    format!("{}-{month:02}-{day:02}", yoe + era * 400 + i64::from(month <= 2))
}

/// One entry of a compiled resource file: a header of numbered type and name, then the data on a 4-byte boundary.
fn add_resource(res: &mut Vec<u8>, kind: u16, name: u16, data: &[u8]) {
    res.extend((data.len() as u32).to_le_bytes());
    res.extend(32u32.to_le_bytes());
    for id in [kind, name] {
        res.extend(0xFFFFu16.to_le_bytes());
        res.extend(id.to_le_bytes());
    }
    res.extend([0; 16]);
    res.extend(data);
    res.resize(res.len().next_multiple_of(4), 0);
}

/// The .ico as a compiled resource file, which link.exe takes as it is: the program's icon in Explorer,
/// without a resource compiler to install. Each picture is an RT_ICON, listed by one RT_GROUP_ICON.
fn icon_resources(ico: &[u8]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    let count = u16::from_le_bytes([ico[4], ico[5]]);
    let mut res = Vec::new();
    add_resource(&mut res, 0, 0, &[]); // the empty entry that marks a 32-bit resource file
    let mut group: Vec<u8> = [0, 1, count].iter().flat_map(|n: &u16| n.to_le_bytes()).collect();
    for i in 0..count {
        let entry = &ico[6 + i as usize * 16..][..16];
        let size = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let at = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        add_resource(&mut res, RT_ICON, i + 1, &ico[at..at + size]);
        group.extend(&entry[..12]);
        group.extend((i + 1).to_le_bytes());
    }
    add_resource(&mut res, RT_GROUP_ICON, 1, &group);
    res
}
