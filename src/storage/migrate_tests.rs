use super::*;
use std::os::windows::fs::OpenOptionsExt;

const ACCOUNT: &str = "084168f587b37a59aa918463def0651ec31ecf22";

fn scratch(name: &str) -> PathBuf {
    let base = std::env::var_os("CRYSTAL_DATA_ROOT").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("crystal-migrate-{name}-{}", crate::backend::unique_id().unwrap()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn put(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A real vault this Windows user can read, as 1.2.8 wrote it.
fn vault_bytes() -> Vec<u8> {
    let dir = scratch("vault");
    let config = crate::backend::Config { experiment: dir.clone(), repository: dir.clone(), game_install: None, dat_overlays: Vec::new() };
    let account = crate::backend::Account {
        label: "Fixture".into(), mode: crate::backend::AccountMode::Private, server: "127.0.0.1".into(),
        pol_id: "fixture".into(), pol_password: "synthetic".into(), se_id: String::new(), se_password: String::new(),
        region: 0, launch_with: None, ashita_boot_ini: None, windower_profile: None, pol_handle_names: String::new(),
        totp_key: Default::default(),
    };
    crate::backend::save_accounts(&config, &[account]).unwrap();
    let bytes = fs::read(config.layout().vault()).unwrap();
    fs::remove_dir_all(dir).unwrap();
    bytes
}

/// 1.2.8's folder as found on a user's PC, plus things Crystal must never touch.
fn old_layout(root: &Path) -> Vec<u8> {
    let iced = root.join("iced-launcher");
    let vault = vault_bytes();
    put(&iced.join("accounts.dpapi"), &vault);
    put(&iced.join("settings.json"), br#"{"language":"en"}"#);
    for name in ["features.json", "directories.json", "playtime.json", "official-versions.json", "news-cache.json",
        "news-cache.ja.json", "startup-prefetch.json", "menu-playonlineus.json"] {
        put(&iced.join(name), name.as_bytes());
    }
    put(&iced.join("startup-task-exe.txt"), b"C:\\x\\Crystal-Launcher.exe");
    put(&iced.join("launch-history.log"), b"old history\n");
    put(&iced.join("official-versions.json.lock"), b"");
    put(&iced.join("backups/2026-11-13_000807-before-update.zip"), b"zip");
    put(&iced.join("game-config-backups/retail-playonlineus-20261019-140009.json"), b"{}");
    put(&iced.join("gamepad-backup/registry-PlayOnlineUS.reg"), b"reg");
    put(&iced.join("locks/windower-profile-00dbe654.lock"), b"");
    let crystal = iced.join("crystal");
    for name in ["pol.exe", "D3D8.dll", "dgVoodoo.conf", "crystal-dgvoodoo.txt", "crystal-features.json", "crystal-pol.json",
        "crystal-loader.log", "crystal-loader.2.log", "crystal-loader.log.old"] {
        put(&crystal.join(name), name.as_bytes());
    }
    put(&crystal.join("dgvoodoo/2.86.4/D3D8.dll"), b"cache");
    put(&crystal.join("windower-startup/snapshot.dpapi"), b"snap");
    put(&crystal.join("0badc0de/pol.exe"), b"old host");
    let old_reshade = crystal.join("reshade");
    let preset = old_reshade.join("presets").join("0123-mine.ini");
    put(&preset, b"Techniques=x");
    put(&old_reshade.join("ReShade.ini"), format!("[GENERAL]\r\nPresetPath={}\r\n", preset.display()).as_bytes());
    put(&old_reshade.join("hosts/00000000000000aa-0123456789abcdef/ReShade.ini"), b"[INSTALL]");
    let account = root.join("test-launcher/data").join(ACCOUNT);
    put(&account.join("session.json"), b"{}");
    put(&account.join("logs/1762000000000000000-100-0.crystal.log"), b"game");
    put(&account.join("logs/1762000000000000000-100-0.crystal.timing.log"), b"timing");
    put(&root.join("locks/ashita.lock"), b"");
    put(&root.join("locks/windower.lock"), b"");
    put(&root.join("controller-discovery/attached.bin"), b"pads");
    fs::create_dir_all(root.join("launcher-updates")).unwrap();
    put(&root.join("Crystal Launcher/locks/windower.lock"), b"");
    put(&root.join("ui-perf.log"), b"perf");
    // Never Crystal's to move: the owner's signing folder, someone's notes, an old snapshot folder.
    put(&root.join("Release Signing/key.bin"), b"secret");
    put(&iced.join("my-notes.txt"), b"mine");
    put(&root.join("locks/windower-bootstrap/8bd2.dpapi"), b"old");
    vault
}

#[test]
fn an_old_install_moves_into_the_new_layout_and_leaves_everything_else_alone() {
    let root = scratch("full");
    let installed = root.join("Programs/crystal");
    put(&installed.join("crystal-loader.log"), b"installed host log");
    put(&installed.join("pol.exe"), b"installed host");
    let vault = old_layout(&root);
    let report = run(&root, Some(&installed));
    assert!(report.deferred.is_empty() && report.kept.is_empty() && report.complete, "{report:?}");
    let layout = Layout::at(&root);
    assert_eq!(fs::read(layout.vault()).unwrap(), vault);
    assert_eq!(fs::read(layout.settings()).unwrap(), br#"{"language":"en"}"#);
    for (path, bytes) in [(layout.features(), "features.json"), (layout.directories(), "directories.json"),
        (layout.playtime(), "playtime.json"), (layout.official_versions(), "official-versions.json"),
        (layout.news_cache(), "news-cache.json"), (layout.startup_prefetch(), "startup-prefetch.json"),
        (layout.menu("PlayOnlineUS"), "menu-playonlineus.json")] {
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes, "{}", path.display());
    }
    assert_eq!(fs::read_to_string(layout.history()).unwrap(), "old history\n");
    assert!(layout.archives().join("2026-11-13_000807-before-update.zip").is_file());
    assert!(layout.game_config_backups().join("retail-playonlineus-20261019-140009.json").is_file());
    assert!(layout.gamepad_backup().join("registry-PlayOnlineUS.reg").is_file());
    let runtime = layout.runtime();
    assert!(host_dir(&runtime).join("pol.exe").is_file() && host_dir(&runtime).join("dgVoodoo.conf").is_file());
    assert!(!host_dir(&runtime).join("0badc0de").exists(), "a stale older host is removed");
    assert!(runtime.join("dgvoodoo/2.86.4/D3D8.dll").is_file());
    assert!(layout.windower_startup().join("snapshot.dpapi").is_file());
    let reshade = reshade_dir(&runtime);
    let config = fs::read_to_string(reshade.join("ReShade.ini")).unwrap();
    assert_eq!(config, format!("[GENERAL]\r\nPresetPath={}\r\n", reshade.join("presets").join("0123-mine.ini").display()));
    assert!(!reshade_hosts(&reshade).exists(), "host folders name the old path; they are rebuilt");
    for name in ["crystal-loader (extracted).log", "crystal-loader (extracted).2.log", "crystal-loader (extracted).log.old",
        "crystal-loader (installed).log"] {
        assert!(layout.logs().join(name).is_file(), "{name}");
    }
    assert!(installed.join("pol.exe").is_file(), "the installed host itself stays");
    assert!(layout.account(ACCOUNT).join("session.json").is_file());
    assert!(layout.game_logs().join("1762000000000000000-100-0.crystal.log").is_file());
    assert!(layout.game_logs().join("1762000000000000000-100-0.crystal.timing.log").is_file());
    assert!(layout.controller_discovery().join("attached.bin").is_file());
    for gone in ["test-launcher", "launcher-updates", "Crystal Launcher", "ui-perf.log", "iced-launcher/crystal",
        "iced-launcher/locks", "locks/ashita.lock", "locks/windower.lock", "iced-launcher/official-versions.json.lock"] {
        assert!(!root.join(gone).exists(), "{gone}");
    }
    // Unrecognised items stay exactly where they were, with the folders holding them.
    assert_eq!(fs::read(root.join("Release Signing/key.bin")).unwrap(), b"secret");
    assert_eq!(fs::read(root.join("iced-launcher/my-notes.txt")).unwrap(), b"mine");
    assert!(root.join("locks/windower-bootstrap/8bd2.dpapi").is_file());
    assert_eq!(fs::read_dir(root.join("iced-launcher")).unwrap().count(), 1);
    assert_eq!(fs::read_to_string(layout.layout_marker()).unwrap().trim(), LAYOUT_VERSION);
    // Idempotent: the next start reads the marker and does nothing.
    assert_eq!(run(&root, Some(&installed)), Report { complete: true, ..Report::default() });
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_crash_at_any_step_of_the_verified_copy_leaves_a_usable_vault() {
    let root = scratch("crash");
    let layout = Layout::at(&root);
    let old = root.join("iced-launcher/accounts.dpapi");
    let vault = vault_bytes();
    // Crashed while copying: a partial staging file, no new vault. The copy starts over.
    put(&old, &vault);
    put(&layout.vault().with_extension("migrating"), &vault[..vault.len() / 2]);
    let report = run(&root, None);
    assert!(report.complete, "{report:?}");
    assert_eq!(fs::read(layout.vault()).unwrap(), vault);
    assert!(!old.exists() && !layout.vault().with_extension("migrating").exists());
    // Crashed after the rename, before the original went: both are equal; the original goes.
    fs::remove_file(layout.layout_marker()).unwrap();
    put(&old, &vault);
    assert!(run(&root, None).complete);
    assert!(!old.exists());
    assert_eq!(fs::read(layout.vault()).unwrap(), vault);
    // The new vault differs (the new layout saved since): the old one is kept, never copied over it.
    fs::remove_file(layout.layout_marker()).unwrap();
    put(&old, b"older vault");
    let report = run(&root, None);
    assert_eq!(report.kept, [Path::new("iced-launcher").join("accounts.dpapi").display().to_string()]);
    assert_eq!(fs::read(layout.vault()).unwrap(), vault);
    assert_eq!(fs::read(&old).unwrap(), b"older vault");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_vault_that_does_not_decode_is_copied_and_also_kept() {
    let root = scratch("undecodable");
    let old = root.join("iced-launcher/accounts.dpapi");
    put(&old, b"not a DPAPI blob");
    let report = run(&root, None);
    assert_eq!(fs::read(Layout::at(&root).vault()).unwrap(), b"not a DPAPI blob");
    assert!(old.exists() && report.kept.len() == 1, "{report:?}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_test_two_vault_is_carried_over_when_there_is_no_other() {
    let root = scratch("test2");
    let vault = vault_bytes();
    put(&root.join("test-launcher/accounts.dpapi"), &vault);
    assert!(run(&root, None).complete);
    assert_eq!(fs::read(Layout::at(&root).vault()).unwrap(), vault);
    assert!(!root.join("test-launcher").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn files_in_use_wait_for_the_next_start_and_nothing_fails() {
    let root = scratch("in-use");
    old_layout(&root);
    let layout = Layout::at(&root);
    // A session worker holds a file in its account folder; a running host holds a lock.
    let session = root.join("test-launcher/data").join(ACCOUNT).join("session.json");
    let held = fs::OpenOptions::new().read(true).share_mode(1).open(&session).unwrap();
    let lock = fs::OpenOptions::new().read(true).write(true).open(root.join("locks/ashita.lock")).unwrap();
    let report = run(&root, None);
    assert!(!report.complete && !layout.layout_marker().exists(), "{report:?}");
    assert!(report.deferred.iter().any(|d| d.contains(ACCOUNT)), "{report:?}");
    assert!(report.deferred.iter().any(|d| d.contains("ashita.lock")), "{report:?}");
    assert!(session.is_file() && root.join("locks/ashita.lock").is_file());
    // Everything else moved meanwhile, the saved data included.
    assert!(layout.vault().is_file() && layout.settings().is_file() && layout.history().is_file());
    assert!(layout.game_logs().join("1762000000000000000-100-0.crystal.log").is_file());
    drop((held, lock));
    let report = run(&root, None);
    assert!(report.complete && report.deferred.is_empty(), "{report:?}");
    assert!(layout.account(ACCOUNT).join("session.json").is_file());
    assert!(!root.join("test-launcher").exists() && !root.join("locks/ashita.lock").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn saved_data_already_in_the_new_place_is_never_overwritten() {
    let root = scratch("conflict");
    let layout = Layout::at(&root);
    put(&root.join("iced-launcher/features.json"), b"old");
    put(&layout.features(), b"new");
    put(&root.join("iced-launcher/news-cache.json"), b"old cache");
    put(&layout.news_cache(), b"new cache");
    let report = run(&root, None);
    assert_eq!(fs::read(layout.features()).unwrap(), b"new");
    assert_eq!(fs::read(root.join("iced-launcher/features.json")).unwrap(), b"old", "kept where it was");
    assert_eq!(fs::read(layout.news_cache()).unwrap(), b"new cache");
    assert!(!root.join("iced-launcher/news-cache.json").exists(), "a rebuildable cache gives way");
    assert_eq!(report.kept.len(), 1, "{report:?}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_fresh_install_just_records_the_layout() {
    let root = scratch("fresh");
    let report = run(&root, None);
    assert!(report.complete && report.line().is_none(), "{report:?}");
    assert!(Layout::at(&root).layout_marker().is_file());
    fs::remove_dir_all(root).unwrap();
}
