//! The Hotkeys page's controller fields, toolkit-agnostic: their messages (`PadMsg`, carried by
//! `Msg::Pad`), how the core answers them, and what each field says (`PadLine`). A field records
//! like the keyboard's: click it, hold the modifiers, press the trigger (`padchord::Recorder`);
//! Esc cancels, Backspace or Delete clears it.
//!
//! What a field says is a warning, never a refusal, except the two contradictions `padchord::plan`
//! refuses (the same chord twice; a chord another fires first on):
//! - FFXI's function on the button Crystal hides, when the game reads the pad through XInput;
//! - through DirectInput nothing can be hidden, so the game also gets the button;
//! - Windows' Game Bar opens on Guide while that setting is on.

use super::hotkeys::Action;
#[cfg(test)]
use super::hotkeys::Problem;
use super::padchord::{self, PadChord};
use super::ui::{State, Target};
use crate::gamepad::detect::{Btn, Family};
use crate::i18n::{t, tf};

/// A controller field's messages.
#[derive(Clone, Debug)]
pub(crate) enum PadMsg {
    /// Start recording into this field.
    Record(Target),
    /// Stop without a change.
    Cancel,
    /// The chord recorded (None clears it).
    Set(Target, Option<PadChord>),
}

pub(crate) fn update(runtime: &super::Runtime, view: &mut State, msg: PadMsg) {
    match msg {
        PadMsg::Record(target) => {
            view.recording = None;
            view.pad_recording = Some(target);
        }
        PadMsg::Cancel => view.pad_recording = None,
        PadMsg::Set(target, chord) => {
            view.pad_recording = None;
            let saved = runtime.update(|s| {
                let hotkeys = &mut s.hotkeys;
                match &target {
                    Target::Next => hotkeys.pad_cycle = chord,
                    Target::Previous => hotkeys.pad_cycle_back = chord,
                    Target::Account(key) => match chord {
                        Some(chord) => { hotkeys.pad_accounts.insert(key.clone(), chord); }
                        None => { hotkeys.pad_accounts.remove(key); }
                    },
                }
            });
            runtime.record_note(match (&saved, chord) {
                (Err(_), _) => "controller: set, but the settings file could not be written",
                (Ok(()), Some(_)) => "controller: saved",
                (Ok(()), None) => "controller: cleared",
            });
            if let Err(error) = saved { view.notice = Some((true, error)); }
        }
    }
}

/// What the page knows of the controllers and FFXI's pad settings (Settings > Controller).
pub(crate) struct Facts {
    /// The first connected pad's family: its button names (Xbox's with none).
    pub family: Option<Family>,
    /// FFXI reads the pad through XInput: Crystal can hide a button from it.
    pub xinput: bool,
    /// The FFXI function bound to each button (XInput layout).
    pub functions: Vec<(Btn, &'static str)>,
    /// Windows opens Game Bar with Guide.
    pub game_bar: bool,
}

impl Facts {
    pub(crate) fn of(gamepad: &crate::gamepad::State) -> Self {
        Self {
            family: gamepad.first_family(),
            xinput: gamepad.ffxi_xinput(),
            functions: Btn::ALL.into_iter().filter_map(|b| Some((b, gamepad.ffxi_function(b)?))).collect(),
            game_bar: crate::gamepad::pad_facts::game_bar_on_guide(),
        }
    }
}

/// A controller field: its chord, and what to know about it (`warn`: it works).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PadLine {
    pub text: String,
    pub problem: String,
    pub warn: bool,
    pub active: bool,
}

/// `action`'s controller field, with the chord set for it.
pub(crate) fn line(chord: Option<PadChord>, action: &Action, refused: &[padchord::Refused], facts: &Facts) -> PadLine {
    let Some(chord) = chord else { return PadLine::default() };
    let text = chord.label(facts.family);
    if let Some(refused) = refused.iter().find(|r| &r.action == action && r.chord == chord) {
        return PadLine { text, problem: refused.problem.text(), warn: false, active: false };
    }
    let mut said: Vec<String> = Vec::new();
    if !facts.xinput {
        said.push(t("FFXI reads the controller through DirectInput and also gets this button.").to_owned());
    } else if chord.hide_entry().is_some() {
        if let Some((_, function)) = facts.functions.iter().find(|(b, _)| *b == chord.trigger) {
            said.push(tf("FFXI's {function} is on this button. Crystal takes it.", &[("function", function)]));
        }
    }
    if facts.game_bar && chord.set() & Btn::Guide.bit() != 0 {
        said.push(t("Windows opens Game Bar with this button. Turn it off in Settings > Gaming > Game Bar.").to_owned());
    }
    PadLine { text, warn: !said.is_empty(), problem: said.join(" "), active: true }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(xinput: bool, game_bar: bool) -> Facts {
        Facts { family: None, xinput, functions: vec![(Btn::Back, "Hide windows"), (Btn::Start, "Log out")], game_bar }
    }
    fn chord(text: &str) -> Option<PadChord> { PadChord::parse(text) }

    #[test]
    fn a_field_says_what_crystal_takes_and_never_refuses_for_it() {
        let action = Action::Next;
        assert_eq!(line(None, &action, &[], &facts(true, true)), PadLine::default());
        let back = line(chord("Back"), &action, &[], &facts(true, false));
        assert_eq!((back.text.as_str(), back.problem.as_str(), back.warn, back.active), ("View","FFXI's Hide windows is on this button. Crystal takes it.", true, true));
        // A combo's trigger is the button named.
        assert!(line(chord("LT+Start"), &action, &[], &facts(true, false)).problem.contains("Log out"));
        assert_eq!(line(chord("A"), &action, &[], &facts(true, false)).problem, "");
        // DirectInput: nothing hidden.
        assert_eq!(line(chord("Back"), &action, &[], &facts(false, false)).problem, "FFXI reads the controller through DirectInput and also gets this button.");
        // Guide while Game Bar has it.
        let guide = line(chord("Guide"), &action, &[], &facts(true, true));
        assert_eq!((guide.problem.as_str(), guide.warn), ("Windows opens Game Bar with this button. Turn it off in Settings > Gaming > Game Bar.", true));
        assert_eq!(line(chord("Guide"), &action, &[], &facts(true, false)).problem, "");
        // A refused chord says why, in red.
        let refused = [padchord::Refused { action: action.clone(), chord: chord("Back").unwrap(), problem: Problem::Overlap }];
        let overlap = line(chord("Back"), &action, &refused, &facts(true, true));
        assert_eq!((overlap.problem, overlap.warn, overlap.active), (Problem::Overlap.text(), false, false));
    }
}
