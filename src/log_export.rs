//! Settings → System → Logs: open the `Logs` folder, and export the logs a support request
//! needs as one zip on the Desktop.
//!
//! The zip holds `history.log` (and its earlier file), the newest game logs and the game
//! host's logs. Never the account vault, settings, backups or crash dumps (a dump holds raw
//! memory). The exported copies carry no names (owner decision 2026-11-17): each account's
//! label, PlayOnline and Square Enix IDs and its characters' names become `Account N` (its row
//! on Home), and the Windows user's folder becomes `%USERPROFILE%`. The files in `Logs\` are
//! left as they are.

use crate::effect::Effect;
use crate::i18n::{t, tf};
use crate::storage::Layout;
use crate::{App, Message};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The newest game logs an export includes.
const GAME_LOGS: usize = 20;
/// Larger files are cut to their last part.
const MAX_FILE: usize = 8 << 20;

#[derive(Clone, Debug)]
pub(crate) enum Msg {
    Open,
    Export,
    Exported(Result<PathBuf, String>),
}

pub(crate) fn update(app: &mut App, msg: Msg) -> Effect<Message> {
    match msg {
        Msg::Open => {
            let logs = app.config.layout().logs();
            let opened = std::fs::create_dir_all(&logs).map_err(|_| ())
                .and_then(|()| crate::shell::shell_open(&logs.to_string_lossy()));
            if opened.is_err() { app.status = t("Windows could not open the logs folder.").into(); }
        }
        Msg::Export => {
            if app.preview { return Effect::none(); }
            app.status = t("Exporting logs…").into();
            let (layout, names) = (app.config.layout(), Anonymizer::for_app(app));
            return Effect::perform(async move {
                tokio::task::spawn_blocking(move || {
                    let desktop = crate::win::known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Desktop)
                        .map_err(|_| t("Could not find the Desktop.").to_owned())?;
                    export(&layout, &names, &desktop, &chrono::Local::now().naive_local())
                }).await.unwrap_or_else(|_| Err(t("Could not export the logs.").into()))
            }, |result| Message::Logs(Msg::Exported(result)));
        }
        Msg::Exported(Ok(path)) => app.status = tf("Logs exported to {path}", &[("path", &path.display())]),
        Msg::Exported(Err(error)) => app.status = error,
    }
    Effect::none()
}

/// The files an export takes from `Logs\`, oldest game logs left out.
pub(crate) fn chosen(layout: &Layout) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    for path in [layout.history(), layout.history().with_extension("old")] {
        if path.is_file() { files.push((file_name(&path), path)); }
    }
    let mut games: Vec<_> = std::fs::read_dir(layout.game_logs()).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    games.sort_by(|a, b| b.0.cmp(&a.0));
    files.extend(games.into_iter().take(GAME_LOGS).map(|(_, path)| (format!("games/{}", file_name(&path)), path)));
    // Every generation of the host log: `crystal-loader.log`, `.2.log`, `.log.old`, legacy copies.
    let host = layout.loader_log();
    let stem = host.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut hosts: Vec<_> = std::fs::read_dir(layout.logs()).into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && file_name(p).starts_with(&stem))
        .collect();
    hosts.sort();
    files.extend(hosts.into_iter().map(|path| (file_name(&path), path)));
    files
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Writes `Crystal logs <date time>.zip` into `dir`; its path.
pub(crate) fn export(layout: &Layout, names: &Anonymizer, dir: &Path, now: &chrono::NaiveDateTime) -> Result<PathBuf, String> {
    let failed = |_| t("Could not export the logs.").to_owned();
    let path = dir.join(format!("Crystal logs {}.zip", now.format("%Y-%m-%d %H-%M-%S")));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(failed)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, source) in chosen(layout) {
        let Ok(bytes) = std::fs::read(&source) else { continue };
        let tail = &bytes[bytes.len().saturating_sub(MAX_FILE)..];
        let text = names.apply(&String::from_utf8_lossy(tail));
        zip.start_file(names.apply(&name), options).map_err(|_| t("Could not export the logs.").to_owned())?;
        zip.write_all(text.as_bytes()).map_err(failed)?;
    }
    zip.finish().map_err(|_| t("Could not export the logs.").to_owned())?;
    Ok(path)
}

/// What an export replaces: identifying words with `Account N`, the user folder with
/// `%USERPROFILE%`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Anonymizer {
    /// (word, stand-in), longest words first.
    words: Vec<(String, String)>,
    profile: Option<String>,
    user: Option<String>,
}

impl Anonymizer {
    /// From the accounts on Home, in row order, with the characters play time knows for each.
    pub(crate) fn for_app(app: &App) -> Self {
        let characters = app.features.as_ref().map(|f| f.playtime()).unwrap_or_default();
        let rows = app.accounts.iter().map(|row| {
            let mut words = vec![row.account.label.clone(), row.account.pol_id.clone(), row.account.se_id.clone()];
            words.extend(characters.iter().filter(|c| c.key.account == row.key).filter_map(|c| c.character.clone()));
            words
        }).collect::<Vec<_>>();
        Self::new(&rows, crate::privacy::profile_dir(), crate::privacy::user_name())
    }

    /// `rows[i]`'s words become `Account i+1`.
    pub(crate) fn new(rows: &[Vec<String>], profile: Option<String>, user: Option<String>) -> Self {
        let mut words: Vec<(String, String)> = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            for word in row.iter().map(|w| w.trim()).filter(|w| w.chars().count() > 1) {
                if !words.iter().any(|(known, _)| known == word) {
                    words.push((word.to_owned(), format!("Account {}", i + 1)));
                }
            }
        }
        words.sort_by_key(|(word, _)| std::cmp::Reverse(word.chars().count()));
        Self { words, profile: profile.map(|p| p.trim_end_matches(['\\', '/']).to_owned()), user }
    }

    /// `text` with the user folder and every known word replaced. Words match whole and
    /// exactly as written (a label "Main" leaves "main menu" alone).
    pub(crate) fn apply(&self, text: &str) -> String {
        let mut text = match &self.profile {
            Some(profile) => replace_ignoring_case(text, profile, "%USERPROFILE%"),
            None => text.to_owned(),
        };
        let user = self.user.iter().map(|u| (u.clone(), "%USERNAME%".to_owned()));
        for (word, stand_in) in self.words.iter().cloned().chain(user) {
            text = replace_words(&text, &word, &stand_in);
        }
        text
    }
}

/// `needle` replaced wherever it appears, in any letter case.
fn replace_ignoring_case(text: &str, needle: &str, with: &str) -> String {
    let (lower, find) = (text.to_lowercase(), needle.to_lowercase());
    if find.is_empty() || lower.len() != text.len() { return text.replace(needle, with); }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = lower[at..].find(&find) {
        out.push_str(&text[at..at + found]);
        out.push_str(with);
        at += found + find.len();
    }
    out.push_str(&text[at..]);
    out
}

/// `word` replaced where it stands alone: not inside a longer word (letters, digits, `_`).
fn replace_words(text: &str, word: &str, with: &str) -> String {
    let joins = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(found) = text[at..].find(word) {
        let start = at + found;
        let end = start + word.len();
        out.push_str(&text[at..start]);
        if joins(text[..start].chars().next_back()) || joins(text[end..].chars().next()) {
            out.push_str(word);
        } else {
            out.push_str(with);
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn names() -> Anonymizer {
        Anonymizer::new(&[vec!["Main".into(), "ABCD1234".into(), String::new(), "Alyria".into()],
            vec!["Mule".into(), "WXYZ9876".into(), "se-id".into()]],
            Some(r"C:\Users\Jane".into()), Some("Jane".into()))
    }

    #[test]
    fn names_ids_characters_and_the_user_folder_leave_the_export() {
        let text = r"2026-11-17 19:54:00 | Main | Game closed normally · closed (exit 0) | C:\users\jane\AppData\Local\Crystal Launcher\Logs\games\a.log
Alyria logged in; main menu; Mule's turn; WXYZ9876 refused; Mainframe; se-id; Jane";
        let out = names().apply(text);
        assert_eq!(out, r"2026-11-17 19:54:00 | Account 1 | Game closed normally · closed (exit 0) | %USERPROFILE%\AppData\Local\Crystal Launcher\Logs\games\a.log
Account 1 logged in; main menu; Account 2's turn; Account 2 refused; Mainframe; Account 2; %USERNAME%");
    }

    #[test]
    fn an_export_zips_history_game_and_host_logs_anonymised_and_nothing_else() {
        let root = std::env::temp_dir().join(format!("crystal-export-{}", crate::backend::unique_id().unwrap()));
        let layout = Layout::at(&root);
        let put = |path: PathBuf, text: &str| { std::fs::create_dir_all(path.parent().unwrap()).unwrap(); std::fs::write(path, text).unwrap(); };
        put(layout.history(), "x | Main | Game closed normally\n");
        put(layout.game_logs().join("2026-11-17 19-54 Account 1.log"), "Crystal 1.2.9\n[host] Alyria\n");
        put(layout.loader_log(), "stage: done (exit 0)\n");
        put(layout.vault(), "secret");
        put(layout.settings(), "{}");
        put(layout.archives().join("a.zip"), "zip");
        put(layout.crash_dumps().join("2026-11-17 19-54-01 pol 7.dmp"), "MDMP");
        let out = root.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let now = chrono::NaiveDateTime::parse_from_str("2026-11-17 20:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let zip_path = export(&layout, &names(), &out, &now).unwrap();
        assert_eq!(zip_path, out.join("Crystal logs 2026-11-17 20-00-00.zip"));
        let mut zip = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
        let mut entries: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_owned()).collect();
        entries.sort();
        assert_eq!(entries, ["crystal-loader.log", "games/2026-11-17 19-54 Account 1.log", "history.log"]);
        let mut text = String::new();
        zip.by_name("history.log").unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "x | Account 1 | Game closed normally\n");
        text.clear();
        zip.by_name("games/2026-11-17 19-54 Account 1.log").unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "Crystal 1.2.9\n[host] Account 1\n");
        assert_eq!(std::fs::read_to_string(layout.history()).unwrap(), "x | Main | Game closed normally\n", "the original is untouched");
        assert!(export(&layout, &names(), &out, &now).is_err(), "never over an existing export");
        std::fs::remove_dir_all(root).unwrap();
    }
}
