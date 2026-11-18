//! The Hotkeys page's controller fields (ui/features.slint `PadField`): each row's chord and
//! what to know about it (`features::pad_view::line`), and the recording the core asks for
//! (`features_view.pad_recording`). While one records, the pad hotkeys' reader runs on a thread
//! of its own (`gamepad::padswitch::Recording`), the field shows the buttons held, and the
//! chord goes to the core on the first release. The recording stops when the field loses the
//! focus, the pointer goes down anywhere else, the window loses the foreground, or the page or
//! its section goes. Each leaves a line in the launch history (`hotkeys: record: controller ...`).
use super::features::{row_of, target_at};
use super::{send, Features, HotkeyLine, MainWindow};
use crate::features::hotkeys::Action;
use crate::features::pad_view::{self, Facts, PadMsg};
use crate::features::padchord::{self, PadChord, Recorded};
use crate::features::ui::{Msg, Section, Target};
use crate::gamepad::detect::Family;
use crate::gamepad::padswitch::Recording;
use crate::i18n::t;
use crate::{App, Message};
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};

/// A controller field's recording, on this side.
struct Active {
    /// The field: 0 next window, 1 previous window, 2 + n the n-th account.
    row: i32,
    target: Target,
    /// Tells this recording's events from an earlier one's still on their way.
    token: u64,
    /// The reader's thread (none if it could not start: the field waits for Esc).
    reader: Option<Recording>,
    /// The pointer press being handled went down on this field.
    claimed: bool,
    family: Option<Family>,
    log: Option<crate::features::Runtime>,
}

impl Active {
    fn note(&self, text: &str) { if let Some(log) = &self.log { log.record_note(&format!("controller {text}")); } }
}

thread_local! {
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    static TOKEN: Cell<u64> = const { Cell::new(0) };
    static WINDOW: RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
}

fn pad(msg: PadMsg) { send(Message::Feature(Msg::Pad(msg))); }

fn active_row() -> Option<i32> { ACTIVE.with(|cell| cell.try_borrow().ok().and_then(|slot| slot.as_ref().map(|a| a.row))) }

/// Whether a controller field records.
pub(super) fn recording() -> bool { active_row().is_some() }

/// The window closed: a recording (and its reader) goes with it.
pub(super) fn forget() {
    ACTIVE.with(|cell| cell.borrow_mut().take());
    WINDOW.with(|cell| cell.borrow_mut().take());
}

fn stop(why: &str) {
    ACTIVE.with(|cell| { if let Ok(slot) = cell.try_borrow() { if let Some(active) = slot.as_ref() { active.note(why); } } });
    pad(PadMsg::Cancel);
}

/// The window lost the foreground: a recording stops.
pub(super) fn window_left() { if recording() { stop("stopped: the window is no longer in front"); } }

/// The pointer went down somewhere in the window, before the page hears of it.
pub(super) fn pointer_pressed() {
    let active = ACTIVE.with(|cell| cell.try_borrow_mut().is_ok_and(|mut slot| slot.as_mut().map(|a| a.claimed = false).is_some()));
    if active {
        let _ = slint::invoke_from_event_loop(|| {
            if ACTIVE.with(|cell| cell.borrow().as_ref().is_some_and(|a| !a.claimed)) { stop("stopped: a press outside the field"); }
        });
    }
}

pub(super) fn wire(ui: &MainWindow) {
    let g = ui.global::<Features>();
    WINDOW.with(|cell| *cell.borrow_mut() = Some(ui.as_weak()));
    g.on_pad_record(|row| { if let Some(target) = target_at(row) { pad(PadMsg::Record(target)); } });
    g.on_pad_cancel(|row| { if active_row() == Some(row) { stop("stopped: Esc, Tab or the field lost the focus"); } });
    g.on_pad_clear(|row| { if let Some(target) = target_at(row) { pad(PadMsg::Set(target, None)); } });
    g.on_pad_pointer(|row| ACTIVE.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() { if let Some(active) = slot.as_mut() { active.claimed = active.row == row; } }
    }));
}

/// What the recording field shows: the prompt, or the buttons held.
fn show(g: &Features, held: u32, family: Option<Family>) {
    let text = if held == 0 { t("Press a button\u{2026}").to_owned() } else { padchord::held_label(held, family) };
    if g.get_pad_recording_text() != text.as_str() { g.set_pad_recording_text(text.into()); }
    if g.get_pad_recording_prompt() != (held == 0) { g.set_pad_recording_prompt(held == 0); }
}

/// Starts or stops the recording the core asks for: its field must be on screen.
pub(super) fn sync(g: &Features, app: &App, accounts: &[String]) {
    let view = &app.features_view;
    let wanted = view.pad_recording.as_ref().filter(|_| view.open && view.section == Section::Hotkeys)
        .and_then(|target| row_of(target, accounts).map(|row| (row, target)));
    ACTIVE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().map(|a| &a.target) == wanted.map(|(_, target)| target) { return; }
        // The one before ends here, its reader with it.
        if let Some(ended) = slot.take() { ended.note("ended"); }
        let Some((row, target)) = wanted else { return };
        let token = TOKEN.get().wrapping_add(1);
        TOKEN.set(token);
        let reader = Recording::start(move |event| { let _ = slint::invoke_from_event_loop(move || recorded(token, event)); });
        let family = app.gamepad.first_family();
        let active = Active { row, target: target.clone(), token, reader: reader.ok(), claimed: false, family, log: app.features.clone() };
        active.note(if active.reader.is_some() { "started" } else { "started, but the controller reader could not start" });
        *slot = Some(active);
        show(g, 0, family);
    });
    let row = wanted.map_or(-1, |(row, _)| row);
    if g.get_pad_recording() != row { g.set_pad_recording(row); }
}

/// One event of the recording `token`'s reader.
fn recorded(token: u64, event: Recorded) {
    let Some((target, family)) = ACTIVE.with(|cell| cell.borrow().as_ref().filter(|a| a.token == token).map(|a| (a.target.clone(), a.family))) else { return };
    match event {
        Recorded::Held(held) => {
            if let Some(ui) = WINDOW.with(|cell| cell.borrow().as_ref().and_then(|weak| weak.upgrade())) { show(&ui.global::<Features>(), held, family); }
        }
        Recorded::Done(chord) => {
            ACTIVE.with(|cell| { if let Some(active) = cell.borrow().as_ref() { active.note(&format!("recorded {chord}")); } });
            pad(PadMsg::Set(target, Some(chord)));
        }
    }
}

/// Tests: an event of the recording there is, as its reader would send it.
#[cfg(test)]
pub(super) fn feed(event: Recorded) {
    if let Some(token) = ACTIVE.with(|cell| cell.borrow().as_ref().map(|a| a.token)) { recorded(token, event); }
}

/// `line`'s controller half: the chord set for `action` and what to know about it.
pub(super) fn decorate(line: &mut HotkeyLine, chord: Option<PadChord>, action: &Action, refused: &[padchord::Refused], facts: &Facts) {
    let pad = pad_view::line(chord, action, refused, facts);
    line.pad_text = pad.text.into();
    line.pad_problem = pad.problem.into();
    line.pad_warn = pad.warn;
    line.pad_active = pad.active;
}
