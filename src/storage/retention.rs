//! Bounded logs: game logs and game-host logs older than two weeks go, and only the newest
//! crash dumps stay. Runs once per start, off the window thread; a file in use is skipped.
//! `history.log` bounds itself (`launch_log::append_history`: 1 MiB and one earlier file).

use super::Layout;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Game logs and game-host logs are kept this long.
pub(crate) const KEEP_FOR: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Crash dumps kept, newest first.
pub(crate) const KEEP_DUMPS: usize = 3;

/// Deletes what is past its keep at `now`; the number of files deleted.
pub(crate) fn prune(layout: &Layout, now: SystemTime) -> usize {
    let mut deleted = 0;
    let old = |modified: SystemTime| now.duration_since(modified).is_ok_and(|age| age > KEEP_FOR);
    for (path, modified) in files(&layout.game_logs()) {
        if path.extension().is_some_and(|e| e == "log") && old(modified) && fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    let host_log = layout.loader_log();
    let stem = host_log.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for (path, modified) in files(&layout.logs()) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with(&stem) && old(modified) && fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    deleted + prune_dumps(&layout.crash_dumps(), KEEP_DUMPS)
}

/// Keeps the newest `keep` `.dmp` files in `dir`.
pub(crate) fn prune_dumps(dir: &Path, keep: usize) -> usize {
    let mut dumps: Vec<_> = files(dir).into_iter().filter(|(p, _)| p.extension().is_some_and(|e| e == "dmp")).collect();
    dumps.sort_by(|a, b| b.1.cmp(&a.1));
    dumps.into_iter().skip(keep).filter(|(path, _)| fs::remove_file(path).is_ok()).count()
}

/// The plain files directly in `dir`, with when each was last written.
fn files(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok().filter(fs::Metadata::is_file)?;
            Some((entry.path(), meta.modified().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aged(path: &Path, age: Duration) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
        fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn logs_past_two_weeks_go_and_only_three_dumps_stay() {
        let root = std::env::temp_dir().join(format!("crystal-retention-{}", crate::backend::unique_id().unwrap()));
        let layout = Layout::at(&root);
        let day = Duration::from_secs(24 * 60 * 60);
        let old_game = layout.game_logs().join("2026-10-01 10-00 Account 1.log");
        let new_game = layout.game_logs().join("2026-11-16 10-00 Account 1.log");
        let old_loader = layout.logs().join("crystal-loader.2.log");
        let legacy_loader = layout.logs().join("crystal-loader (extracted).log.old");
        let new_loader = layout.loader_log();
        let history = layout.history();
        let note = layout.game_logs().join("notes.txt");
        aged(&old_game, day * 15);
        aged(&new_game, day * 2);
        aged(&old_loader, day * 20);
        aged(&legacy_loader, day * 30);
        aged(&new_loader, day);
        aged(&history, day * 90);
        aged(&note, day * 90);
        for n in 0..5u64 {
            aged(&layout.crash_dumps().join(format!("2026-11-1{n} 10-00-00 pol 1{n}.dmp")), day * (10 - n as u32));
        }
        assert_eq!(prune(&layout, SystemTime::now()), 5);
        assert!(!old_game.exists() && !old_loader.exists() && !legacy_loader.exists());
        assert!(new_game.exists() && new_loader.exists());
        assert!(history.exists(), "history bounds itself by size");
        assert!(note.exists(), "only logs are aged out");
        let mut left: Vec<_> = fs::read_dir(layout.crash_dumps()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["2026-11-12 10-00-00 pol 12.dmp", "2026-11-13 10-00-00 pol 13.dmp", "2026-11-14 10-00-00 pol 14.dmp"]);
        fs::remove_dir_all(root).unwrap();
    }
}
