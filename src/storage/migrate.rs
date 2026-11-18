//! The one-time move from Crystal 1.2.8's folders (`iced-launcher\`, `test-launcher\data\` and
//! a few strays at the root) into the layout `storage` documents. It runs at every start until
//! `Data\layout.txt` records the layout, and costs one file read after that.
//!
//! Safety:
//! - Only names listed here are touched. Anything else (another program's folder, the user's
//!   own files, a folder Crystal no longer uses) stays exactly where it is, and so does its
//!   parent folder.
//! - Moves are same-volume renames: each item is in its old place or its new one, never half.
//! - `accounts.dpapi` and `settings.json` are copied to a staging file, flushed, read back and
//!   compared byte for byte, decoded (the vault) or parsed (settings), renamed into place, and
//!   only then is the original deleted. A crash before the rename leaves the original; after
//!   it, both are equal and the next start deletes the original.
//! - An item that cannot move (a running game holds it, a session worker holds its folder, a
//!   lock is held) is left for the next start and named in the report. Startup never fails.
//! - On a name that already exists in the new place, saved data is never overwritten: the old
//!   item stays where it is. Rebuildable files (caches, the game host) are dropped instead.
//! - The marker is written only when nothing legacy is left to move, so an interrupted or
//!   deferred run simply runs again.

use super::{host_dir, reshade_dir, reshade_hosts, Layout, LAYOUT_VERSION};
use std::fs;
use std::path::{Path, PathBuf};

/// Crystal 1.2.8's paths under a data folder.
pub(crate) struct Legacy {
    root: PathBuf,
}

impl Legacy {
    pub(crate) fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn iced(&self) -> PathBuf { self.root.join("iced-launcher") }
    fn crystal(&self) -> PathBuf { self.iced().join("crystal") }
    fn test(&self) -> PathBuf { self.root.join("test-launcher") }
    fn accounts(&self) -> PathBuf { self.test().join("data") }
    /// One account's 1.2.8 folder.
    pub(crate) fn account(&self, name: &str) -> PathBuf { self.accounts().join(name) }
}

/// What a run did, for the history line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub moved: usize,
    pub deleted: usize,
    /// Items left for the next start, and why.
    pub deferred: Vec<String>,
    /// Old items kept because the new place already holds different saved data.
    pub kept: Vec<String>,
    /// The marker was found or written: nothing legacy is left.
    pub complete: bool,
}

impl Report {
    /// One line for `history.log`; `None` when there was nothing to do.
    pub(crate) fn line(&self) -> Option<String> {
        if self.moved == 0 && self.deleted == 0 && self.deferred.is_empty() && self.kept.is_empty() {
            return None;
        }
        let mut line = format!("storage: moved {} and removed {} legacy items", self.moved, self.deleted);
        if !self.deferred.is_empty() { line.push_str(&format!("; left for the next start: {}", self.deferred.join(", "))); }
        if !self.kept.is_empty() { line.push_str(&format!("; kept in place (newer data exists): {}", self.kept.join(", "))); }
        Some(line)
    }
}

/// Saved data never overwrites; rebuildable files give way to what is already there.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind { Data, Rebuildable }

/// Moves `root`'s legacy files into its layout. `installed_host`: the installed launcher's
/// `crystal\` folder, whose old `crystal-loader` logs go to `Logs\` too.
pub(crate) fn run(root: &Path, installed_host: Option<&Path>) -> Report {
    let layout = Layout::at(root);
    let mut report = Report::default();
    if fs::read_to_string(layout.layout_marker()).is_ok_and(|text| text.trim() == LAYOUT_VERSION) {
        report.complete = true;
        return report;
    }
    let legacy = Legacy::at(root);
    let mut run = Run { report: &mut report, root };
    run.critical(&legacy, &layout);
    run.iced(&legacy, &layout);
    run.accounts(&legacy, &layout);
    run.strays(&layout);
    if let Some(host) = installed_host {
        run.loader_logs(host, &layout, "installed");
    }
    if report.deferred.is_empty() && fs::create_dir_all(layout.data()).is_ok()
        && crate::fsutil::write_atomic(&layout.layout_marker(), format!("{LAYOUT_VERSION}\n").as_bytes()).is_ok() {
        report.complete = true;
    }
    report
}

struct Run<'a> {
    report: &'a mut Report,
    root: &'a Path,
}

impl Run<'_> {
    /// `path` relative to the data folder, for the report.
    fn name(&self, path: &Path) -> String {
        path.strip_prefix(self.root).unwrap_or(path).display().to_string()
    }
    fn defer(&mut self, path: &Path, error: &std::io::Error) {
        let line = format!("{} ({})", self.name(path), error.kind());
        self.report.deferred.push(line);
    }

    /// The vault (1.2.8's, else the Test 2 one in `test-launcher\`) and settings.json.
    fn critical(&mut self, legacy: &Legacy, layout: &Layout) {
        let vault = legacy.iced().join("accounts.dpapi");
        let test_vault = legacy.test().join("accounts.dpapi");
        let vault_source = if vault.exists() || !test_vault.exists() { vault } else { test_vault };
        self.verified(&vault_source, &layout.vault(), crate::backend::vault_readable);
        self.verified(&legacy.iced().join("settings.json"), &layout.settings(),
            |bytes| serde_json::from_slice::<serde_json::Value>(bytes).is_ok());
    }

    /// Copy, flush, read back, compare, check, rename into place, then delete the original.
    fn verified(&mut self, old: &Path, new: &Path, check: fn(&[u8]) -> bool) {
        let bytes = match fs::read(old) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => return self.defer(old, &error),
        };
        if new.exists() {
            if fs::read(new).is_ok_and(|current| current == bytes) {
                self.remove_file(old);
            } else {
                let name = self.name(old);
                self.report.kept.push(name);
            }
            return;
        }
        let staging = new.with_extension("migrating");
        let copied = (|| -> std::io::Result<bool> {
            fs::create_dir_all(new.parent().unwrap_or(Path::new(".")))?;
            {
                use std::io::Write;
                let mut file = fs::File::create(&staging)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            let back = fs::read(&staging)?;
            if back != bytes { return Err(std::io::Error::other("copy differs")); }
            let usable = check(&back);
            fs::rename(&staging, new)?;
            Ok(usable)
        })();
        match copied {
            // The original goes only once its verified copy is in place.
            Ok(true) => { self.report.moved += 1; self.remove_file(old); }
            // A file that does not decode was copied faithfully; the original stays as well.
            Ok(false) => { self.report.moved += 1; let name = self.name(old); self.report.kept.push(name); }
            Err(error) => { let _ = fs::remove_file(&staging); self.defer(old, &error); }
        }
    }

    fn iced(&mut self, legacy: &Legacy, layout: &Layout) {
        let iced = legacy.iced();
        if !iced.is_dir() { return; }
        for (name, to, kind) in [
            ("features.json", layout.features(), Kind::Data),
            ("directories.json", layout.directories(), Kind::Data),
            ("playtime.json", layout.playtime(), Kind::Data),
            ("official-versions.json", layout.official_versions(), Kind::Data),
            ("startup-task-exe.txt", layout.startup_task_record(), Kind::Data),
            ("launch-history.log", layout.history(), Kind::Data),
            ("launch-history.old", layout.history().with_extension("old"), Kind::Data),
            ("news-cache.json", layout.news_cache(), Kind::Rebuildable),
            ("news-cache.ja.json", layout.news_cache().with_extension("ja.json"), Kind::Rebuildable),
            ("startup-prefetch.json", layout.startup_prefetch(), Kind::Rebuildable),
            ("backups", layout.archives(), Kind::Data),
            ("game-config-backups", layout.game_config_backups(), Kind::Data),
            ("gamepad-backup", layout.gamepad_backup(), Kind::Data),
        ] {
            self.place(&iced.join(name), &to, kind);
        }
        for entry in entries(&iced) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("menu-") && name.ends_with(".json") {
                self.place(&entry.path(), &layout.data().join(&name), Kind::Data);
            }
        }
        self.drop_unheld_lock(&iced.join("official-versions.json.lock"));
        self.drop_locks(&iced.join("locks"));
        self.crystal(legacy, layout);
        self.remove_empty_dir(&iced);
    }

    /// `iced-launcher\crystal`: the extracted host and its caches.
    fn crystal(&mut self, legacy: &Legacy, layout: &Layout) {
        let crystal = legacy.crystal();
        if !crystal.is_dir() { return; }
        let runtime = layout.runtime();
        // ReShade host folders hold their old folder's path; they are rebuilt at the next launch.
        let old_reshade = reshade_dir(&crystal);
        for entry in entries(&reshade_hosts(&old_reshade)) {
            self.remove_host_folder(&entry.path());
        }
        self.remove_empty_dir(&reshade_hosts(&old_reshade));
        self.place(&old_reshade, &reshade_dir(&runtime), Kind::Data);
        rewrite_reshade_config(&reshade_dir(&runtime).join("ReShade.ini"), &old_reshade, &reshade_dir(&runtime));
        self.place(&crystal.join("dgvoodoo"), &runtime.join("dgvoodoo"), Kind::Rebuildable);
        self.place(&crystal.join("windower-startup"), &layout.windower_startup(), Kind::Rebuildable);
        self.loader_logs(&crystal, layout, "extracted");
        let host = host_dir(&runtime);
        for name in ["pol.exe", "D3D8.dll", "D3DImm.dll", "DDraw.dll", "dgVoodoo.conf", "crystal-dgvoodoo.txt",
            "crystal-features.json", "crystal-pol.json"] {
            self.place(&crystal.join(name), &host.join(name), Kind::Rebuildable);
        }
        // Older hosts kept while a game ran (`<hash8>\`): stale by now unless one still runs.
        for entry in entries(&crystal) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.len() == 8 && name.bytes().all(|b| b.is_ascii_hexdigit()) && plain_dir(&entry.path()) {
                self.remove_host_folder(&entry.path());
            }
        }
        self.remove_empty_dir(&crystal);
    }

    /// `crystal-loader.log`, `.<n>.log` and `.log.old` from a host folder, as
    /// `crystal-loader (<source>)...` in `Logs\` (retention ages them out).
    fn loader_logs(&mut self, from: &Path, layout: &Layout, source: &str) {
        for entry in entries(from) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(rest) = name.strip_prefix("crystal-loader") else { continue };
            if !(rest == ".log" || rest == ".log.old" || rest.ends_with(".log") && rest[1..rest.len() - 4].parse::<u32>().is_ok()) {
                continue;
            }
            self.place(&entry.path(), &layout.logs().join(format!("crystal-loader ({source}){rest}")), Kind::Rebuildable);
        }
    }

    /// `test-launcher\data\<hash>` to `Data\accounts\<hash>`; each one's old `logs\` to `Logs\games`.
    fn accounts(&mut self, legacy: &Legacy, layout: &Layout) {
        for entry in entries(&legacy.accounts()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.len() != 40 || !name.bytes().all(|b| b.is_ascii_hexdigit()) || !plain_dir(&entry.path()) {
                continue;
            }
            let logs = entry.path().join("logs");
            for log in entries(&logs) {
                let file = log.file_name().to_string_lossy().into_owned();
                if file.ends_with(".log") {
                    self.place(&log.path(), &layout.game_logs().join(&file), Kind::Rebuildable);
                }
            }
            self.remove_empty_dir(&logs);
            self.place(&entry.path(), &layout.account(&name), Kind::Data);
        }
        self.remove_empty_dir(&legacy.accounts());
        self.remove_empty_dir(&legacy.test());
    }

    /// The root's strays: lock folders, the controller record, update staging, `ui-perf.log`.
    fn strays(&mut self, layout: &Layout) {
        let root = self.root.to_path_buf();
        self.drop_locks(&root.join("locks"));
        self.place(&root.join("controller-discovery"), &layout.controller_discovery(), Kind::Rebuildable);
        self.place(&root.join("launcher-updates"), &layout.updates(), Kind::Rebuildable);
        // A dev run once pointed CRYSTAL_DATA_ROOT at this folder: `Crystal Launcher\locks` inside it.
        let nested = root.join(super::names::APP_DIR);
        self.drop_locks(&nested.join("locks"));
        self.remove_empty_dir(&nested);
        let perf = root.join("ui-perf.log");
        if perf.is_file() { self.remove_file(&perf); }
    }

    /// Moves `from` to `to`. A folder whose new place exists already is merged item by item.
    fn place(&mut self, from: &Path, to: &Path, kind: Kind) {
        let Ok(meta) = fs::symlink_metadata(from) else { return };
        if meta.file_type().is_symlink() { return; }
        if !to.exists() {
            if let Err(error) = to.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::rename(from, to)) {
                return self.defer(from, &error);
            }
            self.report.moved += 1;
            return;
        }
        if meta.is_dir() && to.is_dir() {
            for entry in entries(from) {
                self.place(&entry.path(), &to.join(entry.file_name()), kind);
            }
            self.remove_empty_dir(from);
        } else if kind == Kind::Rebuildable {
            if meta.is_dir() {
                if let Err(error) = fs::remove_dir_all(from) { return self.defer(from, &error); }
                self.report.deleted += 1;
            } else {
                self.remove_file(from);
            }
        } else {
            let name = self.name(from);
            self.report.kept.push(name);
        }
    }

    fn remove_file(&mut self, path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => self.report.deleted += 1,
            Err(error) => self.defer(path, &error),
        }
    }

    /// Deletes an old host folder, its `pol.exe` first: Windows refuses that while it runs.
    fn remove_host_folder(&mut self, path: &Path) {
        if !plain_dir(path) { return; }
        let host = path.join("pol.exe");
        if host.exists() {
            if let Err(error) = fs::remove_file(&host) { return self.defer(path, &error); }
        }
        match fs::remove_dir_all(path) {
            Ok(()) => self.report.deleted += 1,
            Err(error) => self.defer(path, &error),
        }
    }

    /// The `*.lock` files in `dir` that nobody holds, then `dir` when that empties it.
    fn drop_locks(&mut self, dir: &Path) {
        for entry in entries(dir) {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "lock") && path.is_file() {
                self.drop_unheld_lock(&path);
            }
        }
        self.remove_empty_dir(dir);
    }

    fn drop_unheld_lock(&mut self, path: &Path) {
        if !path.is_file() { return; }
        use std::os::windows::fs::OpenOptionsExt;
        // Opening without sharing fails while anyone has the file open.
        match fs::OpenOptions::new().read(true).share_mode(0).open(path) {
            Ok(file) => { drop(file); self.remove_file(path); }
            Err(error) => self.defer(path, &error),
        }
    }

    /// Removes `dir` only when it is empty (never its contents).
    fn remove_empty_dir(&mut self, dir: &Path) {
        if plain_dir(dir) && fs::read_dir(dir).is_ok_and(|mut list| list.next().is_none()) && fs::remove_dir(dir).is_ok() {
            self.report.deleted += 1;
        }
    }
}

/// The entries of `dir`, or none.
fn entries(dir: &Path) -> Vec<fs::DirEntry> {
    fs::read_dir(dir).map(|list| list.flatten().collect()).unwrap_or_default()
}

/// A real folder, not a link or junction out of Crystal's own.
fn plain_dir(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink() && m.file_attributes() & 0x400 == 0)
}

/// ReShade's config names its preset by full path: point any path under the old folder at the
/// new one. Other text is left as it was.
fn rewrite_reshade_config(config: &Path, old: &Path, new: &Path) {
    let Ok(text) = fs::read_to_string(config) else { return };
    let (old, new) = (old.to_string_lossy(), new.to_string_lossy());
    let lower = text.to_lowercase();
    let needle = old.to_lowercase();
    if needle.is_empty() || !lower.contains(&needle) || lower.len() != text.len() { return; }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = lower[at..].find(&needle) {
        out.push_str(&text[at..at + found]);
        out.push_str(&new);
        at += found + needle.len();
    }
    out.push_str(&text[at..]);
    let _ = crate::fsutil::write_atomic(config, out.as_bytes());
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
