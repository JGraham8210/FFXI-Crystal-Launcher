use crate::{account_import::{self, ImportBatch, ImportDraft, ImportSource}, backend::{Account, AccountMode}, i18n::t, App, Editor, Message};
use crate::effect::Effect;

#[derive(Default)]
pub(crate) struct State {
    pub open: bool,
    pub busy: bool,
    pub sources: Vec<ImportSource>,
    pub path: String,
    pub companion: String,
    pub use_companion: bool,
    pub region: u8,
    pub batch: Option<ImportBatch>,
    pub error: String,
    /// What an "Update existing account" added, shown once it is saved.
    pub result: String,
    generation: u64,
}

#[derive(Clone)]
pub(crate) enum Msg {
    Open, Close, Browse(bool), Picked(u64, bool, Option<std::path::PathBuf>),
    Sources(u64, Vec<ImportSource>), Source(usize), Path(String),
    Companion(bool), CompanionPath(String), Region(u8), Read,
    Loaded(u64, Result<ImportBatch, String>), Review(usize),
}

impl App {
    pub(crate) fn import_update(&mut self, msg: Msg) -> Effect<Message> {
        let state = &mut self.account_import;
        match msg {
            Msg::Open if self.vault_ready && !self.vault_saving => {
                state.open = true;
                state.busy = true;
                state.generation = state.generation.wrapping_add(1);
                let generation = state.generation;
                return Effect::perform(async {
                    tokio::task::spawn_blocking(account_import::detect_sources).await.unwrap_or_default()
                }, move |sources| Message::AccountImport(Msg::Sources(generation, sources)));
            }
            Msg::Close => { state.open = false; state.busy = false; state.batch = None; state.generation = state.generation.wrapping_add(1); }
            Msg::Sources(generation, sources) if state.open && generation == state.generation => {
                state.busy = false;
                if state.companion.is_empty() {
                    if let Some(source) = sources.iter().find(|s| s.kind == account_import::SourceKind::PlayOnline) {
                        state.companion = source.path.to_string_lossy().into_owned();
                    }
                }
                if state.path.is_empty() {
                    if let Some(source) = sources.first() {
                        state.path = source.path.to_string_lossy().into_owned();
                        state.region = source_region(source, &sources, &state.companion).unwrap_or(0);
                    }
                }
                state.sources = sources;
            }
            Msg::Source(index) if !state.busy => {
                if let Some(source) = state.sources.get(index) {
                    state.path = source.path.to_string_lossy().into_owned();
                    // A Forest file has no region of its own: keep the current one
                    // (or the companion's), which also decodes the companion file.
                    if let Some(region) = source_region(source, &state.sources, &state.companion) {
                        state.region = region;
                    }
                    state.batch = None;
                    state.error.clear();
                }
            }
            Msg::Path(path) if !state.busy => { state.path = path; state.batch = None; state.error.clear(); }
            Msg::CompanionPath(path) if !state.busy => { state.companion = path; state.batch = None; }
            Msg::Companion(on) if !state.busy => { state.use_companion = on; state.batch = None; }
            Msg::Region(region) if !state.busy && region <= 2 => { state.region = region; state.batch = None; }
            Msg::Browse(companion) if !state.busy => {
                state.busy = true;
                state.generation = state.generation.wrapping_add(1);
                let generation = state.generation;
                return Effect::perform(async move {
                    rfd::AsyncFileDialog::new().add_filter("PlayOnline / Forest", &["bin", "dat"])
                        .pick_file().await.map(|f| f.path().to_path_buf())
                }, move |path| Message::AccountImport(Msg::Picked(generation, companion, path)));
            }
            Msg::Picked(generation, companion, path) if state.open && generation == state.generation => {
                state.busy = false;
                if let Some(path) = path {
                    let path = path.to_string_lossy().into_owned();
                    if companion { state.companion = path; } else { state.path = path; }
                    state.batch = None;
                    state.error.clear();
                }
            }
            Msg::Read if state.open && !state.busy && !state.path.trim().is_empty() => {
                state.busy = true;
                state.batch = None;
                state.error.clear();
                state.generation = state.generation.wrapping_add(1);
                let generation = state.generation;
                let path = crate::plain_text_path(state.path.trim());
                let companion = state.use_companion.then(|| crate::plain_text_path(state.companion.trim()));
                let region = state.region;
                return Effect::perform(async move {
                    tokio::task::spawn_blocking(move || {
                        let mut batch = account_import::import_file(std::path::Path::new(&path), region)?;
                        if let Some(path) = companion {
                            let members = account_import::import_file(std::path::Path::new(&path), region)?;
                            account_import::fill_from_pol(&mut batch, &members);
                            batch.warnings.extend(members.warnings.iter().cloned());
                        }
                        Ok(batch)
                    }).await.unwrap_or_else(|_| Err(t("Account import could not complete.").into()))
                }, move |result| Message::AccountImport(Msg::Loaded(generation, result)));
            }
            Msg::Loaded(generation, result) if state.open && generation == state.generation => {
                state.busy = false;
                match result { Ok(batch) => state.batch = Some(batch), Err(error) => state.error = error }
            }
            Msg::Review(index) if state.open && !state.busy && self.vault_ready && !self.vault_saving => {
                if let Some(draft) = state.batch.as_ref().and_then(|b| b.drafts.get(index)) {
                    let mut matches = self.accounts.iter().filter(|row| matches_existing(draft, &row.account));
                    let existing = matches.next();
                    if matches.next().is_some() {
                        state.error = t("More than one saved account matches. Edit the saved account directly.").into();
                        return Effect::none();
                    }
                    if existing.is_some_and(|row| row.busy || row.pid.is_some()) {
                        state.error = t("Close this account's game and sign out before editing its login.").into();
                        return Effect::none();
                    }
                    let key = existing.map(|row| row.key.clone());
                    let account = merge_draft(draft, existing.map(|row| &row.account));
                    let auto_login = key.as_ref().and_then(|key| self.settings.account_auto_login.get(key)).copied()
                        .unwrap_or(crate::pol_less::AutoLogin { enabled: false, character_slot: draft.character_slot.unwrap_or(1) }).normalized();
                    self.editor = Some(Editor {
                        game_install: key.as_ref().and_then(|key| self.settings.account_installs.get(key)).cloned().unwrap_or_default(),
                        windower_path: key.as_ref().and_then(|key| self.settings.account_windower_paths.get(key)).cloned().unwrap_or_default(),
                        dat_overlays: key.as_ref().and_then(|key| self.settings.account_overlays.get(key)).cloned().unwrap_or_default(),
                        flags: crate::EditorFlags {
                            uses_otp: key.as_ref().is_some_and(|key| self.settings.account_otp.contains(key)),
                            sign_in_now: key.is_none(),
                            ..Default::default()
                        },
                        auto_login, key, account,
                    });
                    state.open = false;
                    let note = t("Review imported account details, complete missing fields, then save. Existing saved fields were kept.");
                    self.status = match &draft.key_status {
                        Some(line) => format!("{}. {note}", line.trim_end_matches('.')),
                        None => note.into(),
                    };
                }
            }
            _ => {}
        }
        Effect::none()
    }
}

/// The region a chosen source implies: its own (a PlayOnline install), else the
/// detected PlayOnline install the companion path names. Both files are decoded
/// with one region, since `fill_from_pol` only joins members of the same region.
fn source_region(source: &ImportSource, sources: &[ImportSource], companion: &str) -> Option<u8> {
    source.region.or_else(|| {
        sources.iter()
            .find(|s| s.kind == account_import::SourceKind::PlayOnline && s.path.to_string_lossy().eq_ignore_ascii_case(companion.trim()))
            .and_then(|s| s.region)
    })
}

fn matches_existing(draft: &ImportDraft, account: &Account) -> bool {
    if account.mode != AccountMode::Retail { return false; }
    if !draft.pol_id.is_empty() { return draft.pol_id.eq_ignore_ascii_case(&account.pol_id); }
    if account.region != draft.region { return false; }
    if !draft.se_id.is_empty() { return draft.se_id.eq_ignore_ascii_case(&account.se_id); }
    !draft.label.is_empty() && draft.label == account.label
}

fn merge_draft(draft: &ImportDraft, existing: Option<&Account>) -> Account {
    let mut account = existing.cloned().unwrap_or_else(crate::blank_account);
    if existing.is_none() { account.region = draft.region; account.label.clear(); }
    let mut source = crate::blank_account();
    source.label.clone_from(&draft.label); source.pol_id.clone_from(&draft.pol_id);
    source.pol_password.clone_from(&draft.pol_password); source.se_id.clone_from(&draft.se_id);
    source.se_password.clone_from(&draft.se_password); source.totp_key.clone_from(&draft.totp_key);
    source.launch_with = draft.launch_with;
    source.ashita_boot_ini.clone_from(&draft.ashita_boot_ini);
    source.windower_profile.clone_from(&draft.windower_profile);
    fill_empty(&mut account, &source);
    account
}

/// Copies each of `source`'s values into the same field of `target` only where `target`'s is
/// empty, and names the fields that gained a value. Never overwrites.
fn fill_empty(target: &mut Account, source: &Account) -> Vec<&'static str> {
    let mut added = Vec::new();
    for (name, to, from) in [(t("name"), &mut target.label, &source.label), (t("PlayOnline ID"), &mut target.pol_id, &source.pol_id),
        (t("PlayOnline password"), &mut target.pol_password, &source.pol_password), (t("Square Enix ID"), &mut target.se_id, &source.se_id),
        (t("Square Enix password"), &mut target.se_password, &source.se_password), (t("authenticator key"), &mut target.totp_key, &source.totp_key)] {
        if to.is_empty() && !from.is_empty() { to.clone_from(from); added.push(name); }
    }
    if target.launch_with.is_none() && source.launch_with.is_some() { target.launch_with = source.launch_with; added.push(t("launcher")); }
    if target.ashita_boot_ini.is_none() && source.ashita_boot_ini.is_some() { target.ashita_boot_ini.clone_from(&source.ashita_boot_ini); added.push(t("Ashita configuration")); }
    if target.windower_profile.is_none() && source.windower_profile.is_some() { target.windower_profile.clone_from(&source.windower_profile); added.push(t("Windower profile")); }
    added
}

impl App {
    /// The saved account a new (unsaved) sheet would duplicate, when it can be updated now.
    pub(crate) fn merge_target(&self, editor: &Editor) -> Option<usize> {
        if editor.key.is_some() || editor.account.mode != AccountMode::Retail || !self.vault_ready || self.vault_saving { return None; }
        let key = crate::backend::account_key(&editor.account);
        self.accounts.iter().position(|row| row.key == key && !row.busy && row.pid.is_none())
    }

    /// "Update existing account": the sheet's values fill that account's empty fields, then it
    /// saves through the ordinary edit path, so its launcher settings are kept.
    pub(crate) fn merge_account(&mut self) -> Effect<Message> {
        let Some(index) = self.editor.as_ref().and_then(|editor| self.merge_target(editor)) else { return Effect::none() };
        let mut source = self.editor.as_ref().map(|editor| editor.account.clone()).unwrap_or_else(crate::blank_account);
        if !source.totp_key.trim().is_empty() {
            match crate::totp::normalize(&source.totp_key) {
                Ok(key) => source.totp_key = key.to_string(),
                Err(error) => { self.status = error.message().into(); return Effect::none(); }
            }
        }
        let row = &self.accounts[index];
        let key = row.key.clone();
        let mut account = row.account.clone();
        let added = fill_empty(&mut account, &source);
        if added.is_empty() {
            self.editor = None;
            if self.account_import.batch.is_some() { self.account_import.open = true; }
            self.status = t("Nothing to add").into();
            return Effect::none();
        }
        let list = added.join(", ");
        self.account_import.result = crate::i18n::tf("Added: {fields}", &[("fields", &list)]);
        let settings = &self.settings;
        self.editor = Some(Editor {
            game_install: settings.account_installs.get(&key).cloned().unwrap_or_default(),
            windower_path: settings.account_windower_paths.get(&key).cloned().unwrap_or_default(),
            dat_overlays: settings.account_overlays.get(&key).cloned().unwrap_or_default(),
            flags: crate::EditorFlags { uses_otp: settings.account_otp.contains(&key), ..Default::default() },
            auto_login: settings.account_auto_login.get(&key).copied().unwrap_or_default().normalized(),
            key: Some(key), account,
        });
        self.update(Message::SaveAccount)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_import_preserves_saved_credentials_and_launch_preferences() {
        let mut saved = crate::blank_account();
        saved.pol_id = "ABCD1234".into(); saved.pol_password = "saved-pol".into(); saved.se_id = "savedse".into();
        saved.se_password = "saved-se-password".into(); saved.windower_profile = Some("My profile".into());
        let mut draft = ImportDraft::default();
        draft.pol_id = "ABCD1234".into(); draft.se_password = "imported".into();
        draft.windower_profile = Some("Imported profile".into());
        assert!(matches_existing(&draft, &saved));
        let result = merge_draft(&draft, Some(&saved));
        assert_eq!(result.pol_password, saved.pol_password);
        assert_eq!(result.se_password, saved.se_password);
        assert_eq!(result.windower_profile, saved.windower_profile);
        assert_eq!(result.se_id, saved.se_id);
        draft.region = 2;
        assert!(matches_existing(&draft, &saved), "POL identity does not depend on the import region");
        assert_eq!(merge_draft(&draft, Some(&saved)).region, saved.region);
    }

    /// Review, then Save, for a Forest profile that came with an authenticator key: a new
    /// account is saved with it, and a saved account that took typed codes gains it. Either
    /// then signs in without a typed code.
    #[test]
    fn a_reviewed_forest_profile_saves_its_authenticator_key() {
        use crate::{backend, Message};
        const KEY: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        crate::i18n::set(crate::i18n::Lang::En);
        let mut app = crate::tests::app();
        app.preview = false;
        app.launches = Some(Vec::new());
        app.config.experiment = std::env::temp_dir().join(format!("crystal-import-key-{}", backend::unique_id().unwrap()));
        for row in &mut app.accounts { row.busy = false; row.pid = None; }
        let saved_id = app.accounts[0].account.pol_id.clone();
        let saved_key = app.accounts[0].key.clone();
        app.settings.note_sign_in(&saved_key, true);
        assert!(crate::account_row::needs_code(&app, &app.accounts[0]), "typed codes so far");
        let draft = |pol_id: &str| {
            let mut draft = ImportDraft::default();
            draft.label = "Tank".into(); draft.pol_id = pol_id.into(); draft.pol_password = "synthetic".into();
            draft.se_id = "sample09".into(); draft.se_password = "synthetic".into(); draft.totp_key = KEY.into();
            draft.key_status = Some("Authenticator key imported".into());
            draft
        };
        for (pol_id, existing) in [("NEW00001", false), (saved_id.as_str(), true)] {
            app.account_import.open = true;
            app.account_import.batch = Some(ImportBatch { drafts: vec![draft(pol_id)], warnings: Vec::new() });
            let _ = app.update(Message::AccountImport(Msg::Review(0)));
            assert!(app.status.starts_with("Authenticator key imported. "), "the sheet keeps the key status: {}", app.status);
            let editor = app.editor.as_ref().expect("the review opens the account sheet");
            assert_eq!((editor.key.is_some(), editor.account.totp_key.as_str()), (existing, KEY));
            assert!(app.update(Message::SaveAccount).is_async(), "{}", app.status);
            // What that save writes, read back from the vault.
            let edited = app.editor.as_ref().unwrap();
            let mut values: Vec<Account> = app.accounts.iter()
                .map(|row| if Some(&row.key) == edited.key.as_ref() { edited.account.clone() } else { row.account.clone() }).collect();
            if edited.key.is_none() { values.push(edited.account.clone()); }
            backend::save_accounts(&app.config, &values).unwrap();
            let _ = app.update(Message::AccountsSaved(backend::load_accounts(&app.config)));
            let row = app.accounts.iter().find(|row| row.account.pol_id == pol_id).expect("saved");
            assert_eq!(row.account.totp_key, KEY, "existing account: {existing}");
            assert!(row.authenticator_ready() && !crate::account_row::needs_code(&app, row), "existing account: {existing}");
        }
        let _ = std::fs::remove_dir_all(&app.config.experiment);
    }

    #[test]
    fn the_first_selected_source_is_forest_then_the_newest_file() {
        use account_import::SourceKind;
        let root = std::env::temp_dir().join(format!("crystal-source-{}", crate::backend::unique_id().unwrap()));
        let file = |folder: &str| { let dir = root.join(folder); std::fs::create_dir_all(&dir).unwrap(); let p = dir.join("creds.dat"); std::fs::write(&p, "{}").unwrap(); p };
        std::fs::create_dir_all(&root).unwrap();
        let pol = root.join("login_w.bin"); std::fs::write(&pol, "x").unwrap();
        let (auto, forest) = (file("POLAutoLogin"), file("Forest"));
        let source = |kind, path: &std::path::PathBuf| ImportSource { kind, path: path.clone(), region: None };
        let sources = vec![source(SourceKind::PlayOnline, &pol), source(SourceKind::Forest, &auto), source(SourceKind::Forest, &forest)];
        assert_eq!(account_import::default_source(&sources).map(|s| &s.path), Some(&forest), "Forest over POLAutoLogin, whatever the order");
        // Two Forest-kind files outside a Forest folder: the newer one.
        let (old, new) = (file("A"), file("B"));
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600)).unwrap();
        let sources = vec![source(SourceKind::PlayOnline, &pol), source(SourceKind::Forest, &old), source(SourceKind::Forest, &new)];
        assert_eq!(account_import::default_source(&sources).map(|s| &s.path), Some(&new));
        // No Forest file: the first source.
        assert_eq!(account_import::default_source(&sources[..1]).map(|s| &s.path), Some(&pol));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn updating_an_existing_account_fills_only_its_empty_fields() {
        const KEY: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
        crate::i18n::set(crate::i18n::Lang::En);
        let mut saved = crate::blank_account();
        saved.pol_id = "ABCD1234".into(); saved.se_password = "saved-se".into(); saved.windower_profile = Some("Mine".into());
        let mut typed = crate::blank_account();
        typed.pol_id = "abcd1234".into(); typed.se_password = "typed-se".into(); typed.se_id = "typedse".into();
        typed.totp_key = KEY.into(); typed.windower_profile = Some("Other".into()); typed.ashita_boot_ini = Some("a.ini".into());
        let mut result = saved.clone();
        let added = fill_empty(&mut result, &typed);
        assert_eq!(added, ["Square Enix ID", "authenticator key", "Ashita configuration"]);
        assert_eq!((result.se_password.as_str(), result.windower_profile.as_deref()), ("saved-se", Some("Mine")), "never overwritten");
        assert_eq!((result.se_id.as_str(), result.totp_key.as_str(), result.ashita_boot_ini.as_deref()), ("typedse", KEY, Some("a.ini")));
        assert!(fill_empty(&mut result, &typed).is_empty(), "a second pass adds nothing");

        // The sheet button: a new account for a saved login updates that account in place.
        let mut app = crate::tests::app();
        app.preview = false;
        app.config.experiment = std::env::temp_dir().join(format!("crystal-merge-{}", crate::backend::unique_id().unwrap()));
        for row in &mut app.accounts { row.busy = false; row.pid = None; row.account.totp_key.clear(); }
        let existing = app.accounts[0].account.clone();
        let existing_key = app.accounts[0].key.clone();
        let sheet = |totp: &str, se_id: &str| {
            let mut account = crate::blank_account();
            account.pol_id = existing.pol_id.clone(); account.totp_key = totp.into(); account.se_id = se_id.into();
            account.se_password = "typed-over".into();
            Editor { key: None, account, game_install: String::new(), dat_overlays: Vec::new(),
                auto_login: crate::pol_less::AutoLogin::default(), flags: Default::default() }
        };
        app.editor = Some(sheet("jbsw y3dp ehpk 3pxp", "x"));
        assert!(app.merge_target(app.editor.as_ref().unwrap()).is_some());
        assert!(app.merge_account().is_async(), "{}", app.status);
        let editor = app.editor.as_ref().unwrap();
        assert_eq!(editor.key.as_ref(), Some(&existing_key), "saves as an edit of the saved account");
        assert_eq!(editor.account.totp_key, "JBSWY3DPEHPK3PXP");
        assert_eq!(editor.account.se_password, existing.se_password, "a saved password is kept");
        assert_eq!(editor.account.se_id, if existing.se_id.is_empty() { "x" } else { existing.se_id.as_str() });
        assert!(app.account_import.result.starts_with("Added: "), "{}", app.account_import.result);
        assert!(app.account_import.result.contains("authenticator key"));
        let _ = std::fs::remove_dir_all(&app.config.experiment);

        // Everything already set: nothing is saved.
        app.vault_saving = false;
        app.accounts[0].account.totp_key = KEY.into();
        let mut full = sheet(KEY, "");
        full.account.se_password.clear();
        app.editor = Some(full);
        app.account_import.result.clear();
        let _ = app.merge_account();
        assert!(app.editor.is_none() && !app.vault_saving);
        assert_eq!(app.status, "Nothing to add");
    }

    #[test]
    fn a_forest_source_takes_the_companion_install_region_not_us() {
        use account_import::SourceKind;
        let source = |kind, path: &str, region| ImportSource { kind, path: path.into(), region };
        let sources = vec![
            source(SourceKind::PlayOnline, r"C:\POL\JP\usr\all\login_w.bin", Some(2)),
            source(SourceKind::Forest, r"C:\Users\u\AppData\Roaming\Forest\creds.dat", None),
        ];
        assert_eq!(source_region(&sources[0], &sources, ""), Some(2));
        assert_eq!(source_region(&sources[1], &sources, r"c:\pol\jp\usr\all\login_w.bin"), Some(2));
        assert_eq!(source_region(&sources[1], &sources, r"D:\elsewhere\login_w.bin"), None, "keeps the chosen region");
    }
}
