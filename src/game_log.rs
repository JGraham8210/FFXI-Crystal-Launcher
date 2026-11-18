//! One game session's log in `Logs\games\` (`storage`): its name and the header at its top.
//!
//! A log is named by when it started and the account's row on Home, never by the account's
//! name (owner decision 2026-11-17: logs are shared for support): `2026-11-17 19-54 Account 3.log`.
//! The header says what ran: Crystal and its game host, Windows, the tool and its version,
//! dgVoodoo and ReShade, the region and that row number. Launch timing goes into the same
//! file (`launch_timing::Timeline::append_to`): Crystal writes the whole log itself, so a
//! separate timing file only split one launch's story in two.

use std::path::{Path, PathBuf};

/// `YYYY-MM-DD HH-MM Account N.log`; `Account.log` for an account without a row (0).
pub(crate) fn file_name(started: &chrono::NaiveDateTime, number: u32) -> String {
    let account = if number == 0 { "Account".to_owned() } else { format!("Account {number}") };
    format!("{} {account}.log", started.format("%Y-%m-%d %H-%M"))
}

/// A free log path in `dir` for a session that starts now: the plain name, else ` (2)`, ` (3)`
/// ... when the same row started another game in the same minute.
pub(crate) fn path_in(dir: &Path, started: &chrono::NaiveDateTime, number: u32) -> PathBuf {
    let first = dir.join(file_name(started, number));
    if !first.exists() {
        return first;
    }
    let stem = first.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    (2..1000).map(|n| dir.join(format!("{stem} ({n}).log"))).find(|p| !p.exists()).unwrap_or(first)
}

/// What the header reports. Gathered in `pol_less::prepare`; no account name or ID.
pub(crate) struct Header {
    pub host: Option<PathBuf>,
    pub tool: &'static str,
    /// The tool's own file version (Windower's Hook.dll, Ashita-cli.exe); `None` for vanilla.
    pub tool_version: Option<String>,
    pub dgvoodoo: bool,
    pub reshade: bool,
    pub region: u8,
    pub number: u32,
}

impl Header {
    /// The header's lines, in the order they are written.
    pub(crate) fn lines(&self) -> Vec<String> {
        let host = self.host.as_deref().map_or_else(|| "none".to_owned(), |path| {
            std::fs::read(path).map_or_else(|_| "unreadable".to_owned(), |bytes| {
                crate::pol_less::tools::embedded::sha256_hex(&bytes)[..16].to_owned()
            })
        });
        let on = |value: bool| if value { "on" } else { "off" };
        let account = if self.number == 0 { "unknown".to_owned() } else { self.number.to_string() };
        vec![
            format!("Crystal {} · game host {host}", env!("CARGO_PKG_VERSION")),
            format!("Windows: {}", windows_version()),
            format!("Tool: {} {}", self.tool, self.tool_version.as_deref().unwrap_or("")).trim_end().to_owned(),
            format!("dgVoodoo {} · ReShade {}", on(self.dgvoodoo), on(self.reshade)),
            format!("Region: {} · Account row {account}", region_name(self.region)),
        ]
    }
}

fn region_name(region: u8) -> &'static str {
    match region {
        0 => "US",
        1 => "EU",
        2 => "JP",
        _ => "unknown",
    }
}

/// `Windows 11 Pro 24H2 (build 26100.2605)`, from the registry's CurrentVersion key. Windows 11
/// still names itself "Windows 10" there; a build from 22000 on is 11.
pub(crate) fn windows_version() -> String {
    use crate::win::reg::{Key, HKEY_LOCAL_MACHINE, KEY_READ};
    use windows_sys::Win32::System::Registry::RRF_RT_REG_SZ;
    let Ok(key) = Key::open(HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", KEY_READ) else {
        return "unknown".into();
    };
    let text = |name: &str| key.string(name, RRF_RT_REG_SZ, 256).map(|w| String::from_utf16_lossy(&w)).unwrap_or_default();
    let build = text("CurrentBuild");
    let mut product = text("ProductName");
    if build.parse::<u32>().is_ok_and(|b| b >= 22000) {
        product = product.replacen("Windows 10", "Windows 11", 1);
    }
    let release = text("DisplayVersion");
    let ubr = key.dword("UBR").map_or(String::new(), |ubr| format!(".{ubr}"));
    format!("{product} {release} (build {build}{ubr})").replace("  ", " ")
}

/// A file's fixed version (`4.3.2.1`) from its version resource, read without loading it.
pub(crate) fn file_version(path: &Path) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO};
    let name = crate::win::wide_path(path);
    // SAFETY: a NUL-terminated path, a buffer of the size Windows asked for, and a query into it.
    unsafe {
        let size = GetFileVersionInfoSizeW(name.as_ptr(), std::ptr::null_mut());
        if size == 0 { return None; }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(name.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 { return None; }
        let root = crate::win::wide("\\");
        let (mut ptr, mut len): (*mut core::ffi::c_void, u32) = (std::ptr::null_mut(), 0);
        if VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut ptr, &mut len) == 0
            || ptr.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() {
            return None;
        }
        let fixed = std::ptr::read_unaligned(ptr as *const VS_FIXEDFILEINFO);
        let (ms, ls) = (fixed.dwFileVersionMS, fixed.dwFileVersionLS);
        Some(format!("{}.{}.{}.{}", ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn logs_are_named_by_time_and_row_never_by_account_name() {
        assert_eq!(file_name(&at("2026-11-17 19:54:31"), 3), "2026-11-17 19-54 Account 3.log");
        assert_eq!(file_name(&at("2026-01-02 03:04:00"), 0), "2026-01-02 03-04 Account.log");
    }

    #[test]
    fn a_second_game_in_the_same_minute_gets_its_own_file() {
        let dir = std::env::temp_dir().join(format!("crystal-game-log-{}", crate::backend::unique_id().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = at("2026-11-17 19:54:31");
        let first = path_in(&dir, &now, 2);
        assert_eq!(first, dir.join("2026-11-17 19-54 Account 2.log"));
        std::fs::write(&first, "x").unwrap();
        let second = path_in(&dir, &now, 2);
        assert_eq!(second, dir.join("2026-11-17 19-54 Account 2 (2).log"));
        std::fs::write(&second, "x").unwrap();
        assert_eq!(path_in(&dir, &now, 2), dir.join("2026-11-17 19-54 Account 2 (3).log"));
        assert_eq!(path_in(&dir, &now, 1), dir.join("2026-11-17 19-54 Account 1.log"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_header_names_versions_tool_graphics_region_and_row() {
        let exe = std::env::current_exe().unwrap();
        let header = Header { host: Some(exe.clone()), tool: "Windower", tool_version: Some("4.3.6.2".into()),
            dgvoodoo: true, reshade: false, region: 1, number: 4 };
        let lines = header.lines();
        let expected_hash = &crate::pol_less::tools::embedded::sha256_hex(&std::fs::read(exe).unwrap())[..16];
        assert_eq!(lines[0], format!("Crystal {} · game host {expected_hash}", env!("CARGO_PKG_VERSION")));
        assert!(lines[1].starts_with("Windows: ") && lines[1].contains("(build "), "{}", lines[1]);
        assert_eq!(lines[2], "Tool: Windower 4.3.6.2");
        assert_eq!(lines[3], "dgVoodoo on · ReShade off");
        assert_eq!(lines[4], "Region: EU · Account row 4");
        let vanilla = Header { host: None, tool: "Vanilla", tool_version: None, dgvoodoo: false, reshade: false, region: 0, number: 0 }.lines();
        assert_eq!((vanilla[0].ends_with("game host none"), vanilla[2].as_str(), vanilla[4].as_str()),
            (true, "Tool: Vanilla", "Region: US · Account row unknown"));
    }

    #[test]
    fn a_files_fixed_version_is_read_from_its_resource() {
        let kernel = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\kernel32.dll");
        let version = file_version(&kernel).unwrap();
        assert_eq!(version.split('.').count(), 4, "{version}");
        assert!(file_version(Path::new(r"C:\no\such\file.dll")).is_none());
    }
}
