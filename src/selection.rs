//! Picking accounts fast: paint over the checkboxes, Shift-click a range, the header checkbox,
//! the Select menu (All, None, Invert, in game, signed in, not signed in, saved groups) and the
//! keyboard's extend. The reducer here works on plain flags in Home order; the `App` half
//! applies it to the rows and saves `selected_accounts` once per gesture.

use crate::account_row::{row_state, RowState};
use crate::i18n::{t, tf};
use crate::App;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A saved set of accounts, picked again by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Group {
    pub name: String,
    pub keys: Vec<String>,
}

/// What the Select menu picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pick {
    All,
    None,
    Invert,
    InGame,
    SignedIn,
    NotSignedIn,
    Group(usize),
}

impl Pick {
    /// The menu's entry number: the six fixed entries, then the groups.
    pub(crate) fn from_entry(entry: usize) -> Self {
        match entry {
            0 => Pick::All,
            1 => Pick::None,
            2 => Pick::Invert,
            3 => Pick::InGame,
            4 => Pick::SignedIn,
            5 => Pick::NotSignedIn,
            n => Pick::Group(n - 6),
        }
    }
}

/// What a row's state says for the filters. Signed in includes in game.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub in_game: bool,
    pub signed_in: bool,
}

/// The header checkbox's three looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Header {
    None,
    Some,
    All,
}

pub(crate) fn header(picked: &[bool]) -> Header {
    match picked.iter().filter(|on| **on).count() {
        0 => Header::None,
        n if n == picked.len() => Header::All,
        _ => Header::Some,
    }
}

/// None or some become all; all becomes none.
pub(crate) fn header_click(picked: &mut [bool]) {
    let value = header(picked) != Header::All;
    picked.fill(value);
}

/// Sets rows `a` to `b` (either order, ends included) to `value`.
pub(crate) fn fill(picked: &mut [bool], a: usize, b: usize, value: bool) {
    let Some(last) = picked.len().checked_sub(1) else { return };
    for on in &mut picked[a.min(b).min(last)..=a.max(b).min(last)] {
        *on = value;
    }
}

/// Applies a Select menu entry: a plain click replaces the picks, `add` (Ctrl) adds to them.
/// Invert is the same either way. A group that no longer exists changes nothing.
pub(crate) fn pick(picked: &mut [bool], keys: &[String], facts: &[Facts], groups: &[Group], what: Pick, add: bool) {
    let hit: Vec<bool> = match what {
        Pick::All => vec![true; picked.len()],
        Pick::None => vec![false; picked.len()],
        Pick::Invert => picked.iter().map(|on| !on).collect(),
        Pick::InGame => facts.iter().map(|f| f.in_game).collect(),
        Pick::SignedIn => facts.iter().map(|f| f.signed_in).collect(),
        Pick::NotSignedIn => facts.iter().map(|f| !f.signed_in).collect(),
        Pick::Group(i) => match groups.get(i) {
            Some(group) => keys.iter().map(|key| group.keys.contains(key)).collect(),
            None => return,
        },
    };
    for (on, hit) in picked.iter_mut().zip(hit) {
        *on = if add && what != Pick::Invert { *on || hit } else { hit };
    }
}

/// A paint drag: every row crossed takes the value the first box changed to. `last` is the row
/// the pointer was over, so a fast drag fills the rows it skipped.
pub(crate) struct Paint {
    value: bool,
    last: usize,
    /// The picks before the press, for a drag that is cancelled.
    original: Vec<bool>,
}

impl Paint {
    /// Presses row `row`: it changes, and its new value is the one painted.
    pub(crate) fn start(picked: &mut [bool], row: usize) -> Self {
        let original = picked.to_vec();
        let value = !picked[row];
        picked[row] = value;
        Self { value, last: row, original }
    }

    pub(crate) fn over(&mut self, picked: &mut [bool], row: usize) {
        fill(picked, self.last, row, self.value);
        self.last = row;
    }
}

/// Saves `keys` under `name` (replacing a group of that name, any case). `None`: no name.
pub(crate) fn save_group(groups: &mut Vec<Group>, name: &str, keys: Vec<String>) -> Option<usize> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Some(i) = groups.iter().position(|group| group.name.eq_ignore_ascii_case(name)) {
        groups[i].keys = keys;
        return Some(i);
    }
    groups.push(Group { name: name.to_owned(), keys });
    Some(groups.len() - 1)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RenameError {
    Empty,
    Taken,
    Missing,
}

/// Renames a group; another group's name (any case) is refused.
pub(crate) fn rename_group(groups: &mut [Group], index: usize, name: &str) -> Result<(), RenameError> {
    let name = name.trim();
    if index >= groups.len() {
        return Err(RenameError::Missing);
    }
    if name.is_empty() {
        return Err(RenameError::Empty);
    }
    if groups.iter().enumerate().any(|(i, group)| i != index && group.name.eq_ignore_ascii_case(name)) {
        return Err(RenameError::Taken);
    }
    groups[index].name = name.to_owned();
    Ok(())
}

pub(crate) fn delete_group(groups: &mut Vec<Group>, index: usize) {
    if index < groups.len() {
        groups.remove(index);
    }
}

/// Drops accounts that are no longer saved (a group may end up empty; it stays).
pub(crate) fn prune(groups: &mut [Group], saved: &BTreeSet<String>) {
    for group in groups {
        group.keys.retain(|key| saved.contains(key));
    }
}

/// An account whose login changed keeps its place in every group.
pub(crate) fn rename_key(groups: &mut [Group], from: &str, to: &str) {
    for group in groups {
        for key in &mut group.keys {
            if key == from {
                *key = to.to_owned();
            }
        }
    }
}

/// The footer's Shut down button: how many running games it ends (the picked ones).
pub(crate) fn shutdown_label(busy: bool, count: usize) -> String {
    if busy {
        return t("Shutting down…").into();
    }
    tf("Shut down {count}", &[("count", &count)])
}

/// The gestures the window sends (one message carries them all).
#[derive(Clone, Debug)]
pub(crate) enum Command {
    /// A checkbox was pressed (key, Shift held).
    Press(String, bool),
    /// The paint drag's pointer is over this row.
    Over(String),
    /// The keyboard extends the picks from the first row to the second.
    Extend(String, String),
    Pick(Pick, bool),
    /// The header checkbox.
    ToggleAll,
    SaveGroup(String),
    RenameGroup(usize, String),
    DeleteGroup(usize),
}

/// What the pointer and keyboard are doing to the picks.
#[derive(Default)]
pub(crate) struct State {
    /// The last toggled row, where a Shift-click's range starts.
    anchor: Option<String>,
    paint: Option<Paint>,
}

impl State {
    pub(crate) fn painting(&self) -> bool {
        self.paint.is_some()
    }
}

impl App {
    fn picked_flags(&self) -> Vec<bool> {
        self.accounts.iter().map(|row| row.selected).collect()
    }

    fn set_picked(&mut self, picked: &[bool]) {
        for (row, on) in self.accounts.iter_mut().zip(picked) {
            row.selected = *on;
        }
    }

    fn row_keys(&self) -> Vec<String> {
        self.accounts.iter().map(|row| row.key.clone()).collect()
    }

    fn row_index(&self, key: &str) -> Option<usize> {
        self.accounts.iter().position(|row| row.key == key)
    }

    /// Saves the picks as they are now (once per gesture).
    fn commit_selection(&mut self) {
        self.settings.selected_accounts = Some(self.accounts.iter().filter(|row| row.selected).map(|row| row.key.clone()).collect());
        self.persist_settings();
    }

    /// The header checkbox's look.
    pub(crate) fn header_state(&self) -> Header {
        header(&self.picked_flags())
    }

    /// One row's checkbox set to `value` (the keyboard, a number key, an assistive click).
    pub(crate) fn toggle_account(&mut self, key: String, value: bool) {
        if let Some(row) = self.accounts.iter_mut().find(|row| row.key == key) {
            row.selected = value;
        }
        self.selection.anchor = Some(key);
        self.commit_selection();
    }

    /// A paint drag ends: the picks are saved once.
    pub(crate) fn finish_paint(&mut self) {
        if self.selection.paint.take().is_some() {
            self.commit_selection();
        }
    }

    /// A paint drag is abandoned: the picks go back to what they were.
    pub(crate) fn cancel_paint(&mut self) {
        if let Some(paint) = self.selection.paint.take() {
            self.set_picked(&paint.original);
        }
    }

    fn facts(&self) -> Vec<Facts> {
        self.accounts
            .iter()
            .map(|row| {
                let state = row_state(self, row);
                let in_game = matches!(state, RowState::Playing | RowState::ConnectionLost);
                Facts { in_game, signed_in: in_game || state == RowState::SignedIn }
            })
            .collect()
    }

    pub(crate) fn selection_command(&mut self, command: Command) {
        match command {
            Command::Press(key, shift) => {
                // A reorder drag in progress ignores paint.
                if self.account_drag.source.is_some() || self.selection.paint.is_some() {
                    return;
                }
                let Some(row) = self.row_index(&key) else { return };
                let mut picked = self.picked_flags();
                let anchor = self.selection.anchor.as_deref().and_then(|anchor| self.row_index(anchor));
                match anchor.filter(|_| shift) {
                    Some(from) => {
                        let value = !picked[row];
                        fill(&mut picked, from, row, value);
                        self.set_picked(&picked);
                        self.selection.anchor = Some(key);
                        self.commit_selection();
                    }
                    None => {
                        self.selection.paint = Some(Paint::start(&mut picked, row));
                        self.set_picked(&picked);
                        self.selection.anchor = Some(key);
                    }
                }
            }
            Command::Over(key) => {
                let Some(row) = self.row_index(&key) else { return };
                let mut picked = self.picked_flags();
                let Some(paint) = self.selection.paint.as_mut() else { return };
                paint.over(&mut picked, row);
                self.set_picked(&picked);
                self.selection.anchor = Some(key);
            }
            Command::Extend(from, to) => {
                let (Some(from), Some(to)) = (self.row_index(&from), self.row_index(&to)) else { return };
                let mut picked = self.picked_flags();
                fill(&mut picked, from, to, true);
                self.set_picked(&picked);
                self.selection.anchor = Some(self.accounts[to].key.clone());
                self.commit_selection();
            }
            Command::Pick(what, add) => {
                let mut picked = self.picked_flags();
                pick(&mut picked, &self.row_keys(), &self.facts(), &self.settings.account_groups, what, add);
                self.set_picked(&picked);
                self.commit_selection();
            }
            Command::ToggleAll => {
                let mut picked = self.picked_flags();
                header_click(&mut picked);
                self.set_picked(&picked);
                self.commit_selection();
            }
            Command::SaveGroup(name) => {
                let keys = self.accounts.iter().filter(|row| row.selected).map(|row| row.key.clone()).collect();
                if save_group(&mut self.settings.account_groups, &name, keys).is_some() {
                    self.persist_settings();
                }
            }
            Command::RenameGroup(index, name) => match rename_group(&mut self.settings.account_groups, index, &name) {
                Ok(()) => self.persist_settings(),
                Err(RenameError::Taken) => self.status = tf("A group named {name} already exists.", &[("name", &name.trim())]),
                Err(_) => {}
            },
            Command::DeleteGroup(index) => {
                delete_group(&mut self.settings.account_groups, index);
                self.persist_settings();
            }
        }
    }
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
