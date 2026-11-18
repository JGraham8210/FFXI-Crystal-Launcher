//! Where Crystal's per-user data folder is, and the names inside it that the game host uses
//! too. One file for both crates: the launcher includes it (`src/storage.rs`, which documents
//! the whole layout), so the host's logs, locks and caches can never drift from the
//! launcher's. Only names and the root live here; nothing reads or writes files.
#![allow(dead_code)]

use std::path::PathBuf;

/// The folder under `%LOCALAPPDATA%`.
pub const APP_DIR: &str = "Crystal Launcher";
/// Replaces the whole data folder (tests, isolated runs). Inherited by every host a launcher
/// starts, so both sides resolve the same folder.
pub const ROOT_ENV: &str = "CRYSTAL_DATA_ROOT";

pub const LOGS: &str = "Logs";
pub const DATA: &str = "Data";
pub const BACKUPS: &str = "Backups";
pub const CACHE: &str = "Cache";
pub const RUNTIME: &str = "Runtime";

/// `Logs\crashdumps`: the game host's minidumps.
pub const CRASH_DUMPS: &str = "crashdumps";
/// `Runtime\locks`: machine-wide start turns and per-folder locks.
pub const LOCKS: &str = "locks";
/// `Cache\controller-discovery`: the controller scan's record.
pub const CONTROLLER_DISCOVERY: &str = "controller-discovery";
/// `Logs\crystal-loader.log`: every game host's shared log (numbered when held and full).
pub const LOADER_LOG: &str = "crystal-loader.log";
/// A crash dump's name ends with ` pol <pid>.dmp`, so the launcher finds a host's dump by PID.
pub const DUMP_TAG: &str = "pol";

/// The data folder: `CRYSTAL_DATA_ROOT`, else `%LOCALAPPDATA%\Crystal Launcher` (the temporary
/// folder's when there is none). A test build without `CRYSTAL_DATA_ROOT` gets a scratch folder:
/// a test never reads or replaces the user's files, whatever its environment.
pub fn app_root() -> PathBuf {
    if let Some(root) = std::env::var_os(ROOT_ENV) {
        return PathBuf::from(root);
    }
    #[cfg(test)]
    let local = std::env::temp_dir().join("crystal-launcher-test-localappdata");
    #[cfg(not(test))]
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    local.join(APP_DIR)
}

/// `Logs\`.
pub fn logs() -> PathBuf {
    app_root().join(LOGS)
}

/// `Logs\crashdumps\`.
pub fn crash_dumps() -> PathBuf {
    logs().join(CRASH_DUMPS)
}

/// `Runtime\locks\`.
pub fn locks() -> PathBuf {
    app_root().join(RUNTIME).join(LOCKS)
}

/// `Cache\controller-discovery\`.
pub fn controller_discovery() -> PathBuf {
    app_root().join(CACHE).join(CONTROLLER_DISCOVERY)
}
