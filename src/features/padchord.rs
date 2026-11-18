//! Controller hotkeys: a button, or held buttons and one more (`PadChord`), that switch game
//! windows like the keyboard hotkeys (owner-approved design, 1.2.9).
//!
//! One rule, as the keyboard hotkeys follow: Crystal takes exactly what the binding names. A
//! single button never reaches the game; in a combo the modifiers always do, and the trigger
//! is hidden only while its modifiers are held (`hide_list`, applied in each game by the
//! loader's XInput filter). All are in-game hotkeys: they act only while a Crystal game window
//! is in front (`gamepad::padswitch` reads the pads then).
//!
//! - A binding fires on its trigger's down edge while all its modifiers are held on the same
//!   pad; other held buttons don't block it, and pressing the trigger before a modifier fires
//!   nothing (`Matcher`).
//! - The longest match wins: with `Back` and `LT+Back` both bound, Back with LT held is the
//!   second.
//! - One press is one switch: a binding fires again only after its trigger is released on
//!   every pad. That one latch also makes a Steam Input virtual pad and the physical pad it
//!   mirrors one press, without timers.
//! - Refused are only true contradictions (`plan`): the same chord twice, and a binding whose
//!   modifiers hold another binding's every button (that one fires first).

use super::hotkeys::{Action, Problem};
use crate::gamepad::detect::{Btn, Family};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Held modifiers (`Btn::bit`s) and the button that fires. Saved as text: `"Guide"`, `"LT+Back"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PadChord {
    pub modifiers: u32,
    pub trigger: Btn,
}

/// Each button's saved name (Xbox's, whatever the pad).
const NAMES: [(Btn, &str); 19] = [
    (Btn::South, "A"), (Btn::East, "B"), (Btn::West, "X"), (Btn::North, "Y"), (Btn::LB, "LB"), (Btn::RB, "RB"), (Btn::LT, "LT"),
    (Btn::RT, "RT"), (Btn::Back, "Back"), (Btn::Start, "Start"), (Btn::LS, "LS"), (Btn::RS, "RS"), (Btn::DUp, "Up"), (Btn::DDown, "Down"),
    (Btn::DLeft, "Left"), (Btn::DRight, "Right"), (Btn::Guide, "Guide"), (Btn::Touchpad, "Touchpad"), (Btn::Mute, "Mute"),
];

fn saved_name(b: Btn) -> &'static str { NAMES.iter().find(|(n, _)| *n == b).map_or("", |(_, s)| s) }

/// The buttons in `bits`, in `Btn::ALL` order.
pub(crate) fn buttons(bits: u32) -> impl Iterator<Item = Btn> { Btn::ALL.into_iter().filter(move |b| bits & b.bit() != 0) }

/// A button as the first connected pad names it (Xbox's names with none).
pub(crate) fn button_label(b: Btn, family: Option<Family>) -> String {
    use crate::i18n::t;
    let family = family.unwrap_or(Family::Xbox);
    let face = |xbox: &'static str, sony: &'static str, nintendo: &'static str| -> String {
        if family.is_playstation() { sony } else if family == Family::SwitchPro { nintendo } else { xbox }.to_owned()
    };
    match b {
        Btn::South => face("A", t("Cross"), "B"),
        Btn::East => face("B", t("Circle"), "A"),
        Btn::West => face("X", t("Square"), "Y"),
        Btn::North => face("Y", t("Triangle"), "X"),
        other => crate::gamepad::btn_label(family, other).to_owned(),
    }
}

/// Held buttons as a recording shows them: `"LT+…"`.
pub(crate) fn held_label(bits: u32, family: Option<Family>) -> String {
    buttons(bits).map(|b| format!("{}+", button_label(b, family))).collect::<String>() + "\u{2026}"
}

/// The XInput bit a button is (`wButtons`, with LT as bit 16 and RT as bit 17), as the loader's
/// filter reads it. The Sony-only touchpad click and mic button have none.
pub(crate) fn xinput_bit(b: Btn) -> Option<u32> {
    Some(match b {
        Btn::DUp => 0x0001, Btn::DDown => 0x0002, Btn::DLeft => 0x0004, Btn::DRight => 0x0008,
        Btn::Start => 0x0010, Btn::Back => 0x0020, Btn::LS => 0x0040, Btn::RS => 0x0080,
        Btn::LB => 0x0100, Btn::RB => 0x0200, Btn::Guide => 0x0400,
        Btn::South => 0x1000, Btn::East => 0x2000, Btn::West => 0x4000, Btn::North => 0x8000,
        Btn::LT => 1 << 16, Btn::RT => 1 << 17,
        Btn::Touchpad | Btn::Mute => return None,
    })
}

impl PadChord {
    #[cfg(test)]
    pub(crate) const fn single(trigger: Btn) -> Self { Self { modifiers: 0, trigger } }

    /// Every button the chord names.
    pub(crate) fn set(&self) -> u32 { self.modifiers | self.trigger.bit() }

    /// `"LT+Back"`: modifiers in `Btn::ALL` order, the trigger last; names ignore case.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let named = |part: &str| NAMES.iter().find(|(_, n)| n.eq_ignore_ascii_case(part)).map(|(b, _)| *b);
        let (last, mods) = parts.split_last()?;
        let trigger = named(last)?;
        let mut modifiers = 0;
        for part in mods {
            let bit = named(part)?.bit();
            if modifiers & bit != 0 || bit == trigger.bit() { return None; }
            modifiers |= bit;
        }
        Some(Self { modifiers, trigger })
    }

    /// The chord as the first connected pad names it.
    pub(crate) fn label(&self, family: Option<Family>) -> String {
        buttons(self.modifiers).chain([self.trigger]).map(|b| button_label(b, family)).collect::<Vec<_>>().join("+")
    }

    /// `[modifiers, trigger]` in XInput bits for the loader (`pad_hide`); None when a button
    /// has no XInput bit (a game reading XInput never sees it).
    pub(crate) fn hide_entry(&self) -> Option<[u32; 2]> {
        let mods = buttons(self.modifiers).try_fold(0, |all, b| Some(all | xinput_bit(b)?))?;
        Some([mods, xinput_bit(self.trigger)?])
    }
}

impl std::fmt::Display for PadChord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in buttons(self.modifiers) { write!(f, "{}+", saved_name(b))?; }
        f.write_str(saved_name(self.trigger))
    }
}

impl Serialize for PadChord {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_str(&self.to_string()) }
}
impl<'de> Deserialize<'de> for PadChord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        PadChord::parse(&String::deserialize(d)?).ok_or_else(|| serde::de::Error::custom("not a controller hotkey"))
    }
}

/// A chord that does not read is dropped on its own (`hotkeys::Settings`).
pub(crate) fn chord<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<PadChord>, D::Error> {
    Ok(serde_json::Value::deserialize(d)?.as_str().and_then(PadChord::parse))
}
pub(crate) fn chords<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, PadChord>, D::Error> {
    let all = BTreeMap::<String, serde_json::Value>::deserialize(d)?;
    Ok(all.into_iter().filter_map(|(key, v)| Some((key, PadChord::parse(v.as_str()?)?))).collect())
}

/// Pad hotkeys' ids start here, apart from the keyboard's.
pub(crate) const FIRST_ID: i32 = 1000;

/// A pad hotkey that can't act, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refused {
    pub action: Action,
    pub chord: PadChord,
    pub problem: Problem,
}

/// The pad hotkeys that act, each with an id, and the refused ones. Of two equal chords the
/// first wins (the cycle keys, then the accounts in key order); a chord whose modifiers hold
/// another chord's every button is refused, since that one fires first.
pub(crate) fn plan(settings: &super::hotkeys::Settings) -> (Vec<(i32, Action, PadChord)>, Vec<Refused>) {
    let mut wanted: Vec<(Action, PadChord)> = Vec::new();
    if let Some(c) = settings.pad_cycle { wanted.push((Action::Next, c)); }
    if let Some(c) = settings.pad_cycle_back { wanted.push((Action::Previous, c)); }
    wanted.extend(settings.pad_accounts.iter().map(|(k, c)| (Action::Account(k.clone()), *c)));
    let mut active = Vec::new();
    let mut refused = Vec::new();
    for (i, (action, chord)) in wanted.iter().enumerate() {
        let duplicate = wanted[..i].iter().any(|(_, other)| other == chord);
        let overlap = wanted.iter().any(|(_, other)| other != chord && other.set() & chord.modifiers == other.set());
        match (duplicate, overlap) {
            (true, _) => refused.push(Refused { action: action.clone(), chord: *chord, problem: Problem::Duplicate }),
            (_, true) => refused.push(Refused { action: action.clone(), chord: *chord, problem: Problem::Overlap }),
            _ => active.push((FIRST_ID + active.len() as i32, action.clone(), *chord)),
        }
    }
    (active, refused)
}

/// What each game hides for the hotkeys that act (`pad_hide`), sorted and without repeats.
pub(crate) fn hide_list(active: &[(i32, Action, PadChord)]) -> Vec<[u32; 2]> {
    let mut list: Vec<[u32; 2]> = active.iter().filter_map(|(_, _, chord)| chord.hide_entry()).collect();
    list.sort_unstable();
    list.dedup();
    list
}

/// Which bindings fire, from each pad's buttons on every pass (`Btn::bit`s, LT and RT pressed
/// at 128 of 255). Pads are told apart by a key of the reader's choosing.
#[derive(Debug, Default)]
pub(crate) struct Matcher {
    bindings: Vec<(i32, PadChord)>,
    /// Fired, waiting for their trigger to be released on every pad.
    latched: Vec<i32>,
    /// Each pad's buttons on the last pass.
    pads: Vec<(u64, u32)>,
}

impl Matcher {
    pub(crate) fn set_bindings(&mut self, bindings: Vec<(i32, PadChord)>) {
        self.latched.retain(|id| bindings.iter().any(|(i, _)| i == id));
        self.bindings = bindings;
    }

    /// Reading again after a spell without (no game in front): every button down now is held,
    /// not pressed.
    pub(crate) fn arm(&mut self, pads: &[(u64, u32)]) {
        self.release(pads);
        self.pads = pads.to_vec();
    }

    /// One pass: the ids that fire. A pad seen for the first time presses nothing.
    pub(crate) fn step(&mut self, pads: &[(u64, u32)]) -> Vec<i32> {
        self.release(pads);
        let mut fired = Vec::new();
        for &(key, now) in pads {
            let Some(&(_, before)) = self.pads.iter().find(|(k, _)| *k == key) else { continue };
            for trigger in buttons(now & !before) {
                let best = self.bindings.iter().filter(|(_, c)| c.trigger == trigger && now & c.modifiers == c.modifiers)
                    .max_by_key(|(_, c)| c.modifiers.count_ones());
                if let Some(&(id, _)) = best {
                    if !self.latched.contains(&id) {
                        self.latched.push(id);
                        fired.push(id);
                    }
                }
            }
        }
        self.pads = pads.to_vec();
        fired
    }

    /// A fired binding whose trigger is up on every pad may fire again.
    fn release(&mut self, pads: &[(u64, u32)]) {
        let bindings = &self.bindings;
        self.latched.retain(|id| bindings.iter().find(|(i, _)| i == id)
            .is_some_and(|(_, c)| pads.iter().any(|(_, now)| now & c.trigger.bit() != 0)));
    }
}

/// A controller field's recording: hold the modifiers, press the trigger; the chord is taken on
/// the first release, the last button pressed being the trigger. Buttons already down when it
/// starts count only once released and pressed again.
#[derive(Debug, Default)]
pub(crate) struct Recorder {
    ignore: u32,
    held: u32,
    last: Option<Btn>,
}

/// What a recording pass saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// The buttons held now changed.
    Held(u32),
    Done(PadChord),
}

impl Recorder {
    pub(crate) fn start(held: u32) -> Self { Self { ignore: held, ..Self::default() } }

    /// One pass over every pad's buttons together.
    pub(crate) fn step(&mut self, now: u32) -> Option<Recorded> {
        self.ignore &= now;
        let now = now & !self.ignore;
        if self.held & !now != 0 {
            if let Some(trigger) = self.last {
                return Some(Recorded::Done(PadChord { modifiers: self.held & !trigger.bit(), trigger }));
            }
        }
        if let Some(pressed) = buttons(now & !self.held).last() { self.last = Some(pressed); }
        let changed = now != self.held;
        self.held = now;
        changed.then_some(Recorded::Held(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::hotkeys::Settings;

    fn chord(text: &str) -> PadChord { PadChord::parse(text).unwrap_or_else(|| panic!("{text}")) }
    fn bits(list: &[Btn]) -> u32 { list.iter().fold(0, |all, b| all | b.bit()) }

    #[test]
    fn chords_are_saved_as_text_and_read_back() {
        assert_eq!(chord("Guide"), PadChord::single(Btn::Guide));
        assert_eq!(chord("lt + back"), PadChord { modifiers: Btn::LT.bit(), trigger: Btn::Back });
        assert_eq!(chord("Back+LT").to_string(), "Back+LT", "the trigger stays last");
        assert_eq!(chord("RB+LB+Up").to_string(), "LB+RB+Up", "modifiers in a fixed order");
        for text in ["A", "Start", "LS", "RS", "Down", "Touchpad", "Mute", "LT+RT+Y"] {
            assert_eq!(PadChord::parse(&chord(text).to_string()), Some(chord(text)), "{text}");
        }
        for bad in ["", "LT+", "Back+Back", "LT+LT+Back", "Nope", "Back+Nope", "LeftStickUp"] {
            assert_eq!(PadChord::parse(bad), None, "{bad}");
        }
        assert_eq!(serde_json::to_string(&chord("LT+Back")).unwrap(), "\"LT+Back\"");
        let settings: Settings = serde_json::from_str(r#"{"cycle":null,"cycle_back":null,"pad_cycle":"Guide","pad_cycle_back":"Bad","pad_accounts":{"a":"LT+Back","b":7}}"#).unwrap();
        assert_eq!((settings.pad_cycle, settings.pad_cycle_back), (Some(chord("Guide")), None), "a chord that doesn't read is dropped alone");
        assert_eq!(settings.pad_accounts.len(), 1);
    }

    #[test]
    fn labels_follow_the_pads_family() {
        assert_eq!(chord("LT+A").label(None), "LT+A");
        assert_eq!(chord("LT+A").label(Some(Family::DualSense)), "L2+Cross");
        assert_eq!(chord("Back").label(Some(Family::DualSense)), "Create");
        assert_eq!(chord("A").label(Some(Family::SwitchPro)), "B");
        assert_eq!(held_label(bits(&[Btn::LT, Btn::RB]), None), "RB+LT+\u{2026}");
    }

    #[test]
    fn the_loader_gets_xinput_bits_and_nothing_for_sony_only_buttons() {
        assert_eq!(chord("Back").hide_entry(), Some([0, 0x20]));
        assert_eq!(chord("LT+Back").hide_entry(), Some([1 << 16, 0x20]));
        assert_eq!(chord("Back+RT").hide_entry(), Some([0x20, 1 << 17]));
        assert_eq!(chord("Guide").hide_entry(), Some([0, 0x400]));
        assert_eq!(chord("Touchpad").hide_entry(), None);
        assert_eq!(chord("Mute+A").hide_entry(), None);
    }

    fn matcher(list: &[(i32, &str)]) -> Matcher {
        let mut m = Matcher::default();
        m.set_bindings(list.iter().map(|(id, text)| (*id, chord(text))).collect());
        m.arm(&[(0, 0)]);
        m
    }

    #[test]
    fn a_binding_fires_on_its_triggers_down_edge_once_per_press() {
        let mut m = matcher(&[(1, "Guide")]);
        assert_eq!(m.step(&[(0, Btn::Guide.bit())]), [1]);
        // Held: no repeat, however long.
        for _ in 0..100 { assert!(m.step(&[(0, Btn::Guide.bit())]).is_empty()); }
        assert!(m.step(&[(0, 0)]).is_empty());
        assert_eq!(m.step(&[(0, Btn::Guide.bit())]), [1], "released and pressed again");
        // Other held buttons don't block it.
        m.step(&[(0, 0)]);
        assert_eq!(m.step(&[(0, bits(&[Btn::Guide, Btn::South, Btn::LT]))]), [1]);
    }

    #[test]
    fn modifiers_first_and_the_longest_match_wins() {
        let mut m = matcher(&[(1, "Back"), (2, "LT+Back")]);
        assert_eq!(m.step(&[(0, Btn::Back.bit())]), [1], "bare Back");
        m.step(&[(0, 0)]);
        m.step(&[(0, Btn::LT.bit())]);
        assert_eq!(m.step(&[(0, bits(&[Btn::LT, Btn::Back]))]), [2], "with LT held: the longer chord only");
        m.step(&[(0, 0)]);
        // Trigger first, then the modifier: nothing more fires.
        assert_eq!(m.step(&[(0, Btn::Back.bit())]), [1]);
        assert!(m.step(&[(0, bits(&[Btn::LT, Btn::Back]))]).is_empty());
        // A modifier alone fires nothing.
        let mut m = matcher(&[(2, "LT+Back")]);
        assert!(m.step(&[(0, Btn::LT.bit())]).is_empty());
        assert!(m.step(&[(0, Btn::Back.bit())]).is_empty(), "Back with LT let go");
    }

    #[test]
    fn a_held_trigger_across_a_switch_does_not_fire_again() {
        let mut m = matcher(&[(1, "Back")]);
        assert_eq!(m.step(&[(0, Btn::Back.bit())]), [1]);
        // The switch parks and re-arms the reader (another window between games) with Back held.
        m.arm(&[(0, Btn::Back.bit())]);
        assert!(m.step(&[(0, Btn::Back.bit())]).is_empty());
        assert!(m.step(&[(0, 0)]).is_empty());
        assert_eq!(m.step(&[(0, Btn::Back.bit())]), [1]);
    }

    #[test]
    fn buttons_down_when_it_arms_or_a_pad_appears_are_held_not_pressed() {
        let mut m = Matcher::default();
        m.set_bindings(vec![(1, chord("Back"))]);
        m.arm(&[(0, Btn::Back.bit())]);
        assert!(m.step(&[(0, Btn::Back.bit())]).is_empty());
        // A pad that connects with Back down.
        assert!(m.step(&[(0, 0), (7, Btn::Back.bit())]).is_empty());
        assert!(m.step(&[(0, 0), (7, 0)]).is_empty());
        assert_eq!(m.step(&[(0, 0), (7, Btn::Back.bit())]), [1]);
    }

    #[test]
    fn two_pads_either_switches_and_a_mirrored_pad_is_one_press() {
        let mut m = matcher(&[(1, "Back"), (2, "LT+Back")]);
        m.arm(&[(0, 0), (1, 0)]);
        // Either pad switches.
        assert_eq!(m.step(&[(0, 0), (1, Btn::Back.bit())]), [1]);
        m.step(&[(0, 0), (1, 0)]);
        // Steam Input's virtual pad and the physical one: the same press, a pass apart.
        let lt_back = bits(&[Btn::LT, Btn::Back]);
        m.step(&[(0, Btn::LT.bit()), (1, Btn::LT.bit())]);
        assert_eq!(m.step(&[(0, lt_back), (1, Btn::LT.bit())]), [2]);
        assert!(m.step(&[(0, lt_back), (1, lt_back)]).is_empty(), "the mirror's edge is the same press, and not bare Back either");
        // Released on one pad only: still the same press.
        assert!(m.step(&[(0, Btn::LT.bit()), (1, lt_back)]).is_empty());
        assert!(m.step(&[(0, lt_back), (1, lt_back)]).is_empty());
        m.step(&[(0, 0), (1, 0)]);
        assert_eq!(m.step(&[(0, Btn::Back.bit()), (1, 0)]), [1]);
        // No combo across pads: LT on one, Back on the other is bare Back.
        m.step(&[(0, 0), (1, 0)]);
        assert_eq!(m.step(&[(0, Btn::LT.bit()), (1, Btn::Back.bit())]), [1]);
    }

    #[test]
    fn duplicates_and_overlaps_are_refused_and_the_rest_act() {
        let mut s = Settings { pad_cycle: Some(chord("Guide")), pad_cycle_back: Some(chord("Guide")), ..Settings::default() };
        s.pad_accounts.insert("a".into(), chord("Back"));
        s.pad_accounts.insert("b".into(), chord("LT+Back"));
        s.pad_accounts.insert("c".into(), chord("LT"));
        s.pad_accounts.insert("d".into(), chord("LT+RB+A"));
        let (active, refused) = plan(&s);
        assert_eq!(active.iter().map(|(id, a, _)| (*id, a.clone())).collect::<Vec<_>>(),
            [(FIRST_ID, Action::Next), (FIRST_ID + 1, Action::Account("a".into())), (FIRST_ID + 2, Action::Account("c".into()))]);
        assert_eq!(refused.iter().map(|r| (r.action.clone(), r.problem)).collect::<Vec<_>>(), [
            (Action::Previous, Problem::Duplicate),
            // LT alone fires as soon as LT is held for either.
            (Action::Account("b".into()), Problem::Overlap),
            (Action::Account("d".into()), Problem::Overlap),
        ]);
        // Back and LT+Back are no contradiction: the longer one wins while LT is held.
        let mut s = Settings::default();
        s.pad_accounts.insert("a".into(), chord("Back"));
        s.pad_accounts.insert("b".into(), chord("LT+Back"));
        s.pad_accounts.insert("c".into(), chord("Back+Start"));
        let (active, refused) = plan(&s);
        assert_eq!((active.len(), refused.len()), (2, 1), "Back+Start: Back fires first");
        assert_eq!(hide_list(&active), [[0, 0x20], [1 << 16, 0x20]]);
        assert_eq!(plan(&Settings::default()), (vec![], vec![]));
    }

    #[test]
    fn a_recording_takes_the_held_buttons_and_the_last_one_pressed() {
        let mut r = Recorder::start(0);
        assert_eq!(r.step(Btn::LT.bit()), Some(Recorded::Held(Btn::LT.bit())));
        assert_eq!(r.step(Btn::LT.bit()), None);
        assert_eq!(r.step(bits(&[Btn::LT, Btn::Back])), Some(Recorded::Held(bits(&[Btn::LT, Btn::Back]))));
        // Released in either order: LT+Back.
        assert_eq!(r.step(Btn::Back.bit()), Some(Recorded::Done(chord("LT+Back"))));
        let mut r = Recorder::start(0);
        r.step(Btn::Guide.bit());
        assert_eq!(r.step(0), Some(Recorded::Done(chord("Guide"))));
        // The button that started the recording (the controller's A on the field) is not part
        // of it until pressed again.
        let mut r = Recorder::start(Btn::South.bit());
        assert_eq!(r.step(Btn::South.bit()), None);
        assert_eq!(r.step(0), None, "its release ends nothing");
        r.step(Btn::South.bit());
        assert_eq!(r.step(0), Some(Recorded::Done(chord("A"))));
    }
}
