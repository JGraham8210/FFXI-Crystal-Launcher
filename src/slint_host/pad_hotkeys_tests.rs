//! The Hotkeys page's controller fields, on the headless window of `features`' tests.
use super::*;
use crate::features::pad_view::PadMsg;
use crate::features::padchord::{PadChord, Recorded};
use crate::gamepad::detect::Btn;
use crate::slint_host::pad_hotkeys::feed;

fn pad_chords(app: &App) -> (Option<PadChord>, Option<PadChord>) {
    let hotkeys = app.features.as_ref().unwrap().settings().hotkeys;
    (hotkeys.pad_cycle, hotkeys.pad_cycle_back)
}

/// Click (or Return, or the controller's A) starts recording; the buttons held show; the first
/// release saves the chord; Esc cancels and Backspace clears, as the keyboard's field does.
#[test]
fn a_controller_hotkey_is_recorded_by_pressing_it() {
    let mut app = crate::tests::app();
    let (ui, _) = hotkeys_page(&mut app);
    let g = ui.global::<Features>();
    assert_eq!(g.get_cycle_hotkeys().row_data(0).unwrap().pad_text, "");
    g.invoke_pad_record(0);
    assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::Pad(PadMsg::Record(Target::Next)))]));
    assert_eq!((g.get_pad_recording(), g.get_pad_recording_text().as_str(), g.get_pad_recording_prompt()), (0, "Press a button\u{2026}", true));
    assert!(recording() && g.get_recording() == -1, "the keyboard field is not recording");
    feed(Recorded::Held(Btn::LT.bit()));
    assert_eq!((g.get_pad_recording_text().as_str(), g.get_pad_recording_prompt()), ("LT+\u{2026}", false));
    feed(Recorded::Done(PadChord::parse("LT+Back").unwrap()));
    assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::Pad(PadMsg::Set(Target::Next, Some(_))))]));
    assert_eq!(pad_chords(&app).0, PadChord::parse("LT+Back"));
    let line = g.get_cycle_hotkeys().row_data(0).unwrap();
    assert_eq!((line.pad_text.as_str(), g.get_pad_recording()), ("LT+View", -1), "Xbox's names with no controller connected");
    assert_eq!(line.text, "Ctrl+Tab", "the keyboard's hotkey is untouched");
    assert!(!recording());
    // Esc: nothing changes. Backspace: cleared.
    g.invoke_pad_record(1);
    pump(&mut app, &ui);
    assert_eq!(g.get_pad_recording(), 1);
    g.invoke_pad_cancel(1);
    assert!(matches!(pump(&mut app, &ui).as_slice(), [Message::Feature(Msg::Pad(PadMsg::Cancel))]));
    assert_eq!((g.get_pad_recording(), pad_chords(&app).1), (-1, None));
    g.invoke_pad_clear(0);
    pump(&mut app, &ui);
    assert_eq!(pad_chords(&app).0, None);
    assert_eq!(g.get_cycle_hotkeys().row_data(0).unwrap().pad_text, "");
}

/// The same chord twice is refused on the second row, in red; Duplicate's text says why.
#[test]
fn a_refused_controller_hotkey_says_why() {
    let mut app = crate::tests::app();
    let (ui, _) = hotkeys_page(&mut app);
    let g = ui.global::<Features>();
    for target in [Target::Next, Target::Previous] {
        drop(app.feature_update(Msg::Pad(PadMsg::Set(target, PadChord::parse("Guide")))));
    }
    sync(&ui, &app);
    let second = g.get_cycle_hotkeys().row_data(1).unwrap();
    assert_eq!((second.pad_text.as_str(), second.pad_warn, second.pad_active), ("Guide", false, false));
    assert_eq!(second.pad_problem, crate::features::hotkeys::Problem::Duplicate.text());
    assert!(g.get_cycle_hotkeys().row_data(0).unwrap().pad_active);
}
