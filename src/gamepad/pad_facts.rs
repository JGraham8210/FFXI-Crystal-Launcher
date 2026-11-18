//! What the Hotkeys page says about a controller hotkey, from what Settings > Controller knows:
//! the first connected pad's family (its button names), whether FFXI reads the pad through
//! XInput (only then does Crystal hide a button), and which FFXI function a button is bound to.
use super::detect::{Btn, Family};
use super::ffxi::{Control, Flag, Layout, Row, ROWS};

impl super::State {
    /// The first connected pad's family: the button names the page uses (Xbox's with none).
    pub(crate) fn first_family(&self) -> Option<Family> { self.pads.first().map(|p| p.family) }

    /// FFXI reads the pad through XInput (`padmode000`'s last flag), so a hidden button never
    /// reaches it. Through DirectInput the game also gets every bound button.
    pub(crate) fn ffxi_xinput(&self) -> bool { self.retail.saved.flag(Flag::XInput) }

    /// The FFXI function bound to `b` in the saved XInput layout (`padsin000`), if one is.
    pub(crate) fn ffxi_function(&self, b: Btn) -> Option<&'static str> { function(&self.retail.saved.sin, b) }
}

fn function(sin: &[i32], b: Btn) -> Option<&'static str> {
    ROWS.iter().find(|row| {
        let slots = match **row { Row::Single(i) => i..i + 1, Row::Group(i) => i..i + 4 };
        sin.get(slots).is_some_and(|ids| ids.iter().any(|id| Layout::XInput.decode(*id) == Control::Button(b)))
    }).map(|row| row.label())
}

/// Windows opens Game Bar with the Guide button (Settings > Gaming > Game Bar, "Open Game Bar
/// using this button on a controller"). Believed to be `UseNexusForGameBarEnabled` under
/// HKCU\Software\Microsoft\GameBar, on when absent (the Windows default).
pub(crate) fn game_bar_on_guide() -> bool {
    use crate::win::reg::{Key, HKEY_CURRENT_USER, KEY_QUERY_VALUE};
    Key::open(HKEY_CURRENT_USER, r"Software\Microsoft\GameBar", KEY_QUERY_VALUE).ok()
        .and_then(|key| key.dword("UseNexusForGameBarEnabled")).is_none_or(|on| on != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffxis_default_xinput_layout_names_the_function_on_each_button() {
        let sin = super::super::ffxi::DEFAULT_XINPUT_SIN;
        assert_eq!(function(&sin, Btn::Back), Some("Hide windows"));
        assert_eq!(function(&sin, Btn::Start), Some("Log out"));
        assert_eq!(function(&sin, Btn::DUp), Some("Menu cursor / target"));
        assert_eq!(function(&sin, Btn::Guide), None, "FFXI doesn't read Guide through XInput");
        let mut unbound = sin;
        unbound[9] = -1;
        assert_eq!(function(&unbound, Btn::Back), None);
    }
}
