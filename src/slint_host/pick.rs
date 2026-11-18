//! The picking controls (ui/pick.slint): `wire` turns the header checkbox, the Select menu and
//! the paint drag into core messages (`selection::Command`), `sync` pushes the header's state,
//! the saved groups and the menu's words.
use super::sync::{put, strings_model};
use super::{send, MainWindow, PickStrings, Picking};
use crate::i18n::{t, Lang};
use crate::selection::{Command, Header, Pick};
use crate::{App, Message};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::rc::Rc;

/// What the callbacks act on, as of the last sync.
struct State {
    lang: Option<Lang>,
    /// The rows' keys in the order shown.
    keys: Vec<String>,
    groups: Rc<VecModel<SharedString>>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn act(f: impl FnOnce(&State) -> Option<Command>) {
    let command = STATE.with(|cell| cell.borrow().as_ref().and_then(f));
    if let Some(command) = command {
        send(Message::Selection(command));
    }
}

fn key(state: &State, row: i32) -> Option<String> {
    state.keys.get(usize::try_from(row).ok()?).cloned()
}

pub(super) fn wire(ui: &MainWindow) {
    let state = State { lang: None, keys: Vec::new(), groups: Rc::new(VecModel::default()) };
    let picking = ui.global::<Picking>();
    picking.set_groups(ModelRc::from(state.groups.clone()));
    STATE.with(|cell| *cell.borrow_mut() = Some(state));
    picking.on_press(|row, shift| act(|s| Some(Command::Press(key(s, row)?, shift))));
    picking.on_over(|row| act(|s| Some(Command::Over(key(s, row)?))));
    // The button went up on a checkbox: the paint drag ends (the window's own release listener
    // does the same when it was let go elsewhere).
    picking.on_release(|| send(Message::AccountDragFinish));
    picking.on_extend(|from, to| act(|s| Some(Command::Extend(key(s, from)?, key(s, to)?))));
    picking.on_pick(|entry, add| {
        if let Ok(entry) = usize::try_from(entry) {
            send(Message::Selection(Command::Pick(Pick::from_entry(entry), add)));
        }
    });
    picking.on_toggle_all(|| send(Message::Selection(Command::ToggleAll)));
    picking.on_save_group(|name| send(Message::Selection(Command::SaveGroup(name.to_string()))));
    picking.on_rename_group(|group, name| {
        if let Ok(group) = usize::try_from(group) {
            send(Message::Selection(Command::RenameGroup(group, name.to_string())));
        }
    });
    picking.on_delete_group(|group| {
        if let Ok(group) = usize::try_from(group) {
            send(Message::Selection(Command::DeleteGroup(group)));
        }
    });
}

pub(super) fn sync(ui: &MainWindow, app: &App) {
    let picking = ui.global::<Picking>();
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else { return };
        let lang = crate::i18n::current();
        if state.lang != Some(lang) {
            state.lang = Some(lang);
            put!(picking, get_strings, set_strings, strings());
        }
        state.keys = app.accounts.iter().map(|row| row.key.clone()).collect();
        strings_model(&state.groups, app.settings.account_groups.iter().map(|group| group.name.clone()));
        let header = match app.header_state() {
            Header::None => 0,
            Header::Some => 1,
            Header::All => 2,
        };
        put!(picking, get_state, set_state, header);
        put!(picking, get_painting, set_painting, app.selection.painting());
        // With no accounts there is nothing to select: the menu closes.
        if app.accounts.is_empty() && picking.get_open() {
            picking.set_open(false);
        }
    });
}

pub(super) fn forget() {
    STATE.with(|cell| cell.borrow_mut().take());
}

fn strings() -> PickStrings {
    PickStrings {
        select: t("Select").into(),
        all: t("All").into(),
        none: t("None").into(),
        invert: t("Invert").into(),
        in_game: t("In game").into(),
        signed_in: t("Signed in").into(),
        not_signed_in: t("Not signed in").into(),
        save_as: t("Save selection as…").into(),
        group_name: t("Group name").into(),
        save: t("Save").into(),
        rename: t("Rename").into(),
        delete: t("Delete").into(),
    }
}
