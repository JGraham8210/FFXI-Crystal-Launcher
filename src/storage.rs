//! Every file and folder Crystal keeps for a Windows user. All paths come from here.
//!
//! ```text
//! %LOCALAPPDATA%\Crystal Launcher\        CRYSTAL_DATA_ROOT replaces it; test builds use a scratch folder
//!   Logs\
//!     history.log, history.old            one line per launch and Crystal event; 1 MiB, one earlier file
//!     games\<YYYY-MM-DD HH-MM> <account>.log   one per game session; kept 14 days
//!     crystal-loader.log, .<n>.log, .log.old   the game hosts' shared log; kept 14 days
//!     crashdumps\<time> pol <pid>.dmp     the game host's minidumps; the newest 3
//!   Data\
//!     accounts.dpapi                      saved accounts (DPAPI, this Windows user)
//!     settings.json, features.json, directories.json, playtime.json,
//!     official-versions.json (+ .lock), menu-<vendor>.json, startup-task-exe.txt
//!     layout.txt                          the layout version; written once migration finished
//!     accounts\<hash>\                    one account's session, friends and delivery files
//!   Backups\
//!     archives\                           dated game-settings zips (features::backups)
//!     game-config\                        FFXI registry settings before Crystal changed them
//!     gamepad\                            the gamepad registry before Crystal changed it
//!   Cache\                                anything here can be deleted; it is rebuilt
//!     news-cache.json, news-cache.ja.json, startup-prefetch.json
//!     controller-discovery\, updates\ (self-update downloads), previews\ (design captures)
//!   Runtime\                              files games run from; rebuilt when missing
//!     host\                               the extracted game host (pol.exe), its dgVoodoo files and
//!                                         feature checks; <hash8>\ while an older one still runs
//!     dgvoodoo\<version>\                 the pinned dgVoodoo release
//!     reshade\                            ReShade, presets, hosts\<host>-<dll>\ per host build
//!     windower-startup\                   saved Windower startup snapshots
//!     locks\                              start turns and Windower profile locks
//!   official-installer*\                  game installer downloads (a chosen folder; never moved)
//! ```
//!
//! The installed launcher's own `crystal\pol.exe` (beside `Crystal-Launcher.exe`) runs in place;
//! its log goes to `Logs\` like every host's. Crystal 1.2.8 and older kept these files in
//! `iced-launcher\` and `test-launcher\data\`; `migrate` moves them once. Those versions do not
//! read the new layout.

pub(crate) mod migrate;
pub(crate) mod retention;

#[path = "../crystal-loader/src/data_paths.rs"]
pub(crate) mod names;

use std::path::{Path, PathBuf};

/// The layout version `Data\layout.txt` records once every legacy file has moved.
pub(crate) const LAYOUT_VERSION: &str = "2";

/// Crystal's data folder (`names::app_root`).
pub(crate) fn root() -> PathBuf {
    names::app_root()
}

/// At the window process's start, before settings are read: the one-time move into this
/// layout (`migrate`), its line in the history, then log retention on a thread of its own.
/// One Crystal at a time does this (a machine-wide mutex); nothing here can fail startup.
pub(crate) fn prepare_at_start() {
    let layout = Layout::current();
    {
        let _turn = MigrationTurn::take();
        let installed = std::env::current_exe().ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("crystal")))
            .filter(|dir| dir.is_dir());
        let report = migrate::run(layout.root(), installed.as_deref());
        if let Some(line) = report.line() {
            crate::launch_log::append_history(&layout.history(), "Crystal", &line, None);
        }
    }
    let _ = std::thread::Builder::new().name("log-retention".into())
        .spawn(move || { retention::prune(&layout, std::time::SystemTime::now()); });
}

/// `Local\CrystalLauncher-storage`, held while the layout is migrated.
struct MigrationTurn(windows_sys::Win32::Foundation::HANDLE);
impl MigrationTurn {
    fn take() -> Self {
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let name = crate::win::wide("Local\\CrystalLauncher-storage");
        // SAFETY: a NUL-terminated name; the handle is owned by the value and closed on drop.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if !handle.is_null() {
            // Abandoned or timed out, the migration still runs: it is safe to repeat.
            unsafe { WaitForSingleObject(handle, 30_000) };
        }
        Self(handle)
    }
}
impl Drop for MigrationTurn {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::ReleaseMutex};
        if self.0.is_null() { return; }
        // SAFETY: releasing a mutex this thread may own (a failed release is harmless) and closing its handle.
        unsafe { ReleaseMutex(self.0); CloseHandle(self.0); }
    }
}

/// The paths under one data folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    root: PathBuf,
}

impl Layout {
    pub(crate) fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    /// The layout of this user's data folder.
    pub(crate) fn current() -> Self {
        Self::at(root())
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    // Logs
    pub(crate) fn logs(&self) -> PathBuf { self.root.join(names::LOGS) }
    pub(crate) fn history(&self) -> PathBuf { self.logs().join("history.log") }
    pub(crate) fn game_logs(&self) -> PathBuf { self.logs().join("games") }
    pub(crate) fn crash_dumps(&self) -> PathBuf { self.logs().join(names::CRASH_DUMPS) }
    pub(crate) fn loader_log(&self) -> PathBuf { self.logs().join(names::LOADER_LOG) }

    // Data
    pub(crate) fn data(&self) -> PathBuf { self.root.join(names::DATA) }
    pub(crate) fn vault(&self) -> PathBuf { self.data().join("accounts.dpapi") }
    pub(crate) fn settings(&self) -> PathBuf { self.data().join("settings.json") }
    pub(crate) fn features(&self) -> PathBuf { self.data().join("features.json") }
    pub(crate) fn directories(&self) -> PathBuf { self.data().join("directories.json") }
    pub(crate) fn playtime(&self) -> PathBuf { self.data().join("playtime.json") }
    pub(crate) fn official_versions(&self) -> PathBuf { self.data().join("official-versions.json") }
    pub(crate) fn startup_task_record(&self) -> PathBuf { self.data().join("startup-task-exe.txt") }
    /// The retail menu preference for a region's vendor (`playonlineus`, ...).
    pub(crate) fn menu(&self, vendor: &str) -> PathBuf {
        self.data().join(format!("menu-{}.json", vendor.to_ascii_lowercase()))
    }
    pub(crate) fn layout_marker(&self) -> PathBuf { self.data().join("layout.txt") }
    pub(crate) fn accounts(&self) -> PathBuf { self.data().join("accounts") }
    /// One account's folder; `name` is its 40-hex directory key.
    pub(crate) fn account(&self, name: &str) -> PathBuf { self.accounts().join(name) }

    // Backups
    pub(crate) fn backups(&self) -> PathBuf { self.root.join(names::BACKUPS) }
    pub(crate) fn archives(&self) -> PathBuf { self.backups().join("archives") }
    pub(crate) fn game_config_backups(&self) -> PathBuf { self.backups().join("game-config") }
    pub(crate) fn gamepad_backup(&self) -> PathBuf { self.backups().join("gamepad") }

    // Cache
    pub(crate) fn cache(&self) -> PathBuf { self.root.join(names::CACHE) }
    /// The English headline cache; `services::cache_path` names the other languages' beside it.
    pub(crate) fn news_cache(&self) -> PathBuf { self.cache().join("news-cache.json") }
    pub(crate) fn startup_prefetch(&self) -> PathBuf { self.cache().join("startup-prefetch.json") }
    pub(crate) fn controller_discovery(&self) -> PathBuf { self.cache().join(names::CONTROLLER_DISCOVERY) }
    pub(crate) fn updates(&self) -> PathBuf { self.cache().join("updates") }
    pub(crate) fn previews(&self) -> PathBuf { self.cache().join("previews") }

    // Runtime
    pub(crate) fn runtime(&self) -> PathBuf { self.root.join(names::RUNTIME) }
    pub(crate) fn locks(&self) -> PathBuf { self.runtime().join(names::LOCKS) }
    pub(crate) fn windower_startup(&self) -> PathBuf { self.runtime().join("windower-startup") }

    /// A game installer download folder at the root (`official-installer`, `-EU`, `-JP`).
    pub(crate) fn installer_downloads(&self, name: &str) -> PathBuf { self.root.join(name) }
    /// The bundled native client's folder, used only when no game install is known.
    pub(crate) fn bundled_client(&self) -> PathBuf { self.root.join("client") }
}

/// Inside a `Runtime` folder: where the extracted game host runs.
pub(crate) fn host_dir(runtime: &Path) -> PathBuf {
    runtime.join("host")
}
/// Inside a `Runtime` folder: the pinned dgVoodoo release `version`.
pub(crate) fn dgvoodoo_dir(runtime: &Path, version: &str) -> PathBuf {
    runtime.join("dgvoodoo").join(version)
}
/// Inside a `Runtime` folder: ReShade's runtime, config and presets.
pub(crate) fn reshade_dir(runtime: &Path) -> PathBuf {
    runtime.join("reshade")
}
/// Inside a ReShade folder: one host folder per host and ReShade build.
pub(crate) fn reshade_hosts(reshade: &Path) -> PathBuf {
    reshade.join("hosts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_sits_in_its_top_level_folder() {
        let layout = Layout::at(r"C:\D");
        let under = |path: PathBuf, top: &str| assert!(path.starts_with(Path::new(r"C:\D").join(top)), "{}", path.display());
        for path in [layout.history(), layout.game_logs(), layout.crash_dumps(), layout.loader_log()] { under(path, "Logs"); }
        for path in [layout.vault(), layout.settings(), layout.features(), layout.directories(), layout.playtime(),
            layout.official_versions(), layout.startup_task_record(), layout.menu("PlayOnlineUS"), layout.layout_marker(), layout.account("ab")] {
            under(path, "Data");
        }
        for path in [layout.archives(), layout.game_config_backups(), layout.gamepad_backup()] { under(path, "Backups"); }
        for path in [layout.news_cache(), layout.startup_prefetch(), layout.controller_discovery(), layout.updates(), layout.previews()] { under(path, "Cache"); }
        for path in [layout.locks(), layout.windower_startup(), host_dir(&layout.runtime()), reshade_dir(&layout.runtime())] { under(path, "Runtime"); }
        assert_eq!(layout.menu("PlayOnlineUS"), Path::new(r"C:\D\Data\menu-playonlineus.json"));
        assert_eq!(layout.history(), Path::new(r"C:\D\Logs\history.log"));
    }

    /// The host resolves its log, locks and controller record from the same names.
    #[test]
    fn the_game_host_resolves_the_same_folders() {
        let layout = Layout::current();
        assert_eq!(names::logs(), layout.logs());
        assert_eq!(names::locks(), layout.locks());
        assert_eq!(names::controller_discovery(), layout.controller_discovery());
        assert_eq!(names::crash_dumps(), layout.crash_dumps());
    }

    #[test]
    fn a_test_build_never_resolves_the_users_folder() {
        let Some(real) = std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join(names::APP_DIR)) else { return };
        assert!(!root().starts_with(&real), "{}", root().display());
    }
}
