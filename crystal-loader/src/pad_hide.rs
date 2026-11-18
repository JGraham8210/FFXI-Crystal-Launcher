//! The controller buttons Crystal's pad hotkeys take from this game (`pad_hide`).
//!
//! Each entry is `[modifiers, trigger]` in XInput bits: `wButtons`' bits, with LT as bit 16
//! and RT as bit 17 (a trigger counts as pressed at 128 of 255, as in the launcher). While
//! every modifier is held, the trigger is hidden from the game's XInput reads
//! (`xinput_filter`); the modifiers always reach it. A single button has no modifiers and is
//! always hidden. The list comes in the session or credentials line at launch and in the
//! `pad_hide` service command whenever the bindings change; a missing field hides nothing.

use crate::json::Value;
use crate::proto::ProtoError;

/// LT and RT, beside `wButtons`' 16 bits.
pub const LT: u32 = 1 << 16;
pub const RT: u32 = 1 << 17;
/// XInput's Guide button (only `XInputGetStateEx` reports it).
pub const GUIDE: u32 = 0x0400;
/// Every bit a binding may name: `wButtons`' bits but 0x0800, which XInput leaves unused,
/// and the two triggers.
const VALID: u32 = 0xF7FF | LT | RT;
/// More than Crystal ever sends (two cycle keys and an account each).
pub const MAX_ENTRIES: usize = 64;

/// `[modifiers, trigger]`.
pub type Entry = [u32; 2];

/// `pad_hide` from a session, credentials or service line. Absent or null: no hide.
pub fn parse(value: Option<&Value<'_>>) -> Result<Vec<Entry>, ProtoError> {
    const KEY: &str = "pad_hide";
    let items = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(ProtoError::Field(KEY, "must be a list")),
    };
    if items.len() > MAX_ENTRIES { return Err(ProtoError::Field(KEY, "too many entries")); }
    let bits = |value: &Value<'_>| value.as_i64().filter(|n| (0..=i64::from(VALID)).contains(n)).map(|n| n as u32);
    items.iter().map(|item| {
        let Value::Array(pair) = item else { return Err(ProtoError::Field(KEY, "entries must be pairs")) };
        let [mods, trigger] = pair.as_slice() else { return Err(ProtoError::Field(KEY, "entries must be pairs")) };
        let (mods, trigger) = (bits(mods), bits(trigger));
        match (mods, trigger) {
            (Some(mods), Some(trigger)) if mods & !VALID == 0 && trigger.count_ones() == 1 && trigger & VALID != 0 && mods & trigger == 0 =>
                Ok([mods, trigger]),
            _ => Err(ProtoError::Field(KEY, "invalid buttons")),
        }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn read(text: &str) -> Result<Vec<Entry>, ProtoError> {
        let line = format!("{{\"pad_hide\":{text}}}");
        let root = json::parse(line.as_bytes()).unwrap();
        parse(root.get("pad_hide"))
    }

    #[test]
    fn entries_are_modifier_and_trigger_pairs() {
        assert_eq!(read("[]"), Ok(vec![]));
        assert_eq!(read("null"), Ok(vec![]));
        assert_eq!(parse(None), Ok(vec![]), "a missing field hides nothing");
        // Guide alone; Back alone; LT+Back; Back+RT.
        assert_eq!(read("[[0,1024],[0,32],[65536,32],[32,131072]]"), Ok(vec![[0, GUIDE], [0, 0x20], [LT, 0x20], [0x20, RT]]));
        for bad in ["1", "[1]", "[[1]]", "[[0,0]]", "[[0,3]]", "[[32,32]]", "[[0,2048]]", "[[2048,1]]", "[[0,262144]]", "[[-1,1]]", "[[0,1.5]]",
            "[[0,\"1\"]]", "[[0,1,2]]"] {
            assert!(read(bad).is_err(), "{bad}");
        }
        let many = format!("[{}]", vec!["[0,1]"; MAX_ENTRIES + 1].join(","));
        assert!(read(&many).is_err());
    }
}
