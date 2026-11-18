//! Optional navigation of the real FFXI login UI, including background clients.
//! Login keys enter the verified client's input handler on its own game thread.
//! An unresolved layout, menu, missing character, changed selection or timeout
//! leaves control with the user. No global key events, window activation or
//! menu writes.

use std::{sync::{mpsc, Arc, LazyLock, Mutex, atomic::{AtomicU64, AtomicUsize, Ordering}}, thread, time::{Duration, Instant}};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HWND},
    System::{LibraryLoader::{GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS},
        Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyboardState, IsWindowEnabled},
        WindowsAndMessaging::{GetClassNameW, GetForegroundWindow,
            GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible},
    },
};
use crate::{game_layout::{Input, Menus, Resolution}, key_log, log, proto::AutoLogin};

const POLL: Duration = Duration::from_millis(100);
const SETTLE: Duration = Duration::from_millis(250);
const LIMIT: Duration = Duration::from_secs(180);
// Menu and controller fields this reader uses. game_layout extracts each from
// the game's own code and requires these exact values.
pub(crate) const MENU_RESOURCE: usize = 4;
pub(crate) const MENU_NAME: usize = 0x46;
pub(crate) const MENU_CURSOR: usize = 0x4C;
pub(crate) const MENU_DISABLED: usize = 0x5D;
pub(crate) const CONTROLLER_MENU: usize = 8;
pub(crate) const HELP_MODE: usize = 0x2C;
pub(crate) const HELP_LOGIN: u32 = 0x11;
pub(crate) const HELP_DELETE: u32 = 0x12;
/// Character rows. The 0xb0 stride is asserted by the locators' literal
/// (x*5, x+2*5x, shl 4) instruction chains rather than extracted.
pub(crate) const ROW_SIZE: usize = 0xB0;
pub(crate) const ROW_OCCUPIED: usize = 0x68;
pub(crate) const ROW_MARKED: usize = 0x98;
pub(crate) const CONFIRM_CURSOR: usize = 0x14;
pub(crate) const CONFIRM_KIND: usize = 0x18;
pub(crate) const CONFIRM_LOGIN: u32 = 13;
pub(crate) const CONFIRM_VARIANT: usize = 0xA5E0;
/// The keyboard controller's GetKeyboardState buffer (lea ecx,[esi+0x298]).
pub(crate) const KEYBOARD_BUFFER: usize = 0x298;

use crate::mem::read;

fn pointer(address: usize) -> Option<usize> { crate::mem::pointer(address, 0x20000) }

fn operand_matches(base: usize, code: &[u8], prefix: &[u8], rva: usize, suffix: &[u8]) -> bool {
    let Some(address) = base.checked_add(rva).and_then(|n| u32::try_from(n).ok()) else { return false; };
    code.len() == prefix.len() + 4 + suffix.len()
        && code.starts_with(prefix) && code.ends_with(suffix)
        && code[prefix.len()..prefix.len() + 4] == address.to_le_bytes()
}

/// The menu globals of one loaded game. Built only from a resolution whose
/// lobby, character table and menus all resolved, and whose code sites still
/// hold the scanned instructions in live memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout { pub(crate) base: usize, menus: Menus, lobby: usize }

impl Layout {
    pub(crate) fn resolve(resolution: &Resolution) -> Result<Self, String> {
        let lobby = crate::lobby::verified_lobby(resolution)?;
        resolution.characters.clone().map_err(|reason| format!("lobby characters: {reason}"))?;
        let menus = resolution.menus.clone().map_err(|reason| format!("login menus: {reason}"))?;
        let layout = Self { base: resolution.base, menus, lobby: lobby.ptr };
        verified_layout(&layout).then_some(layout).ok_or_else(|| "live login menu code differs from the scanned image".into())
    }
}

fn verified_layout(layout: &Layout) -> bool {
    // The instructions first inspected at 1015fdf0 (focused UI), 10241842
    // (one-based UI row -> selected index), 10243699 (login help mode), 10015a57
    // (game window), 10243bba (existing character -> login confirmation 13) and
    // 10243c18 (help mode 0x11), now at their located sites and re-read live.
    // Relocation is checked too. Deletion reuses dbnamese with help mode 0x12.
    let (base, m) = (layout.base, &layout.menus);
    let at = |rva: usize| base.checked_add(rva);
    // `cmp eax,[FOCUS]; jne` (the branch distance is not checked, as in the scan).
    let Some(focus) = at(m.sites.focus).and_then(read::<[u8; 7]>) else { return false; };
    let Some(row) = at(m.sites.selected).and_then(read::<[u8; 10]>) else { return false; };
    let Some(help) = at(m.sites.help).and_then(read::<[u8; 13]>) else { return false; };
    let Some(window) = at(m.sites.window).and_then(read::<[u8; 6]>) else { return false; };
    operand_matches(base, &focus, &[0x3B, 0x05], m.focus, &[0x75])
        && operand_matches(base, &row, &[0x0F, 0xBF, 0x40, 0x4C, 0x48, 0xA3], m.selected, &[])
        && operand_matches(base, &help, &[0x8B, 0x0D], m.help, &[0xC7, 0x41, 0x14, 2, 0, 0, 0])
        && operand_matches(base, &window, &[0x8B, 0x0D], m.window, &[])
        && at(m.sites.confirm13).and_then(read::<[u8; 8]>) == Some([0x83, 0xC0, 0x44, 0x53, 0x50, 0x53, 0x6A, 0x0D])
        && at(m.sites.help11).and_then(read::<[u8; 7]>) == Some([0xC7, 0x42, 0x2C, 0x11, 0, 0, 0])
}

/// `call rel32` at `code[at]`, where `code` was read from RVA `site`, lands
/// on RVA `target`. Relative calls are the same at any load address.
pub(crate) fn call_lands(code: &[u8], site: usize, at: usize, target: usize) -> bool {
    code.get(at) == Some(&0xE8) && code.get(at + 1..at + 5).and_then(|rel| rel.try_into().ok()).is_some_and(|rel: [u8; 4]| {
        (site + at + 5).checked_add_signed(i32::from_le_bytes(rel) as isize) == Some(target)
    })
}

fn verified_input_layout(base: usize, keys: &Input) -> bool {
    use crate::game_layout::{KEY_EDGE, KEY_PRESSED, KEY_REPEAT};
    // The keyboard frame reads this exact buffer after updating the input bank.
    let at = |rva: usize| base.checked_add(rva);
    let Some(call) = at(keys.frame_call).and_then(read::<[u8; 6]>) else { return false; };
    let Some(controller) = at(keys.controller_call).and_then(read::<[u8; 6]>) else { return false; };
    // 10005ce3: the client's own first-key-down path calls the same three
    // handlers with this input bank as `this`, the object they write through.
    let Some(down) = at(keys.key_down).and_then(read::<[u8; 0x2F]>) else { return false; };
    let input_at = |offset: usize| operand_matches(base, &down[offset..offset + 5], &[0xB9], keys.input, &[]);
    operand_matches(base, &call, &[0xFF, 0x15], keys.iat_slot, &[])
        && operand_matches(base, &controller, &[0x8B, 0x0D], keys.keyboard, &[])
        && at(keys.buffer_site).and_then(read::<[u8; 7]>) == Some([0x8D, 0x8E, 0x98, 2, 0, 0, 0x51])
        && [0, 0xC, 0x1A, 0x25].into_iter().all(input_at)
        && call_lands(&down, keys.key_down, 0x14, keys.pressed)
        && call_lands(&down, keys.key_down, 0x1F, keys.edge)
        && call_lands(&down, keys.key_down, 0x2A, keys.repeat)
        && at(keys.edge).and_then(read::<[u8; 17]>) == Some(KEY_EDGE)
        && at(keys.repeat).and_then(read::<[u8; 17]>) == Some(KEY_REPEAT)
        && at(keys.pressed).and_then(read::<[u8; 28]>) == Some(KEY_PRESSED)
}

/// The client's one-button notice: lobby refusals (such as "still logged in")
/// and the in-world disconnect (disconnect.rs). Read live as these 16 bytes.
pub(crate) const OK_MENU: &[u8; 16] = b"menu    ok      ";

/// `Other` is a readable menu this module does not know; an unreadable focus
/// is no snapshot at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen { Warning, Title, Characters, Confirm, Dialog, Other }

fn screen(name: &[u8; 16]) -> Screen {
    match name {
        b"menu    ptc8lice" => Screen::Warning,
        b"menu    loby2win" => Screen::Title,
        b"menu    dbnamese" => Screen::Characters,
        b"menu    ptc6yesn" => Screen::Confirm,
        OK_MENU => Screen::Dialog,
        _ => Screen::Other,
    }
}

impl Screen {
    /// Fixed category names; boot_timing uses them as checkpoint names.
    fn label(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Title => "title",
            Self::Characters => "characters",
            Self::Confirm => "confirmation",
            Self::Dialog | Self::Other => "other",
        }
    }

    /// One of the login menus the navigator drives.
    fn login(self) -> bool { !matches!(self, Self::Dialog | Self::Other) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Snapshot {
    screen: Screen,
    focus: usize,
    cursor: i32,
    character_count: i32,
    target_available: bool,
    login_context: bool,
    selected: i32,
    login_confirmation: bool,
}

/// The focused menu, its resource and its resource name.
pub(crate) fn focused_menu(layout: &Layout) -> Option<(usize, usize, [u8; 16])> {
    let focus = pointer(layout.base + layout.menus.focus)?;
    let resource = pointer(focus + MENU_RESOURCE)?;
    Some((focus, resource, read::<[u8; 16]>(resource + MENU_NAME)?))
}

/// The same menu is still focused: no switch happened during a read.
pub(crate) fn menu_unchanged(layout: &Layout, focus: usize, resource: usize, name: &[u8; 16]) -> bool {
    pointer(layout.base + layout.menus.focus) == Some(focus) && read::<[u8; 16]>(resource + MENU_NAME).as_ref() == Some(name)
}

fn snapshot(layout: &Layout, target: u8) -> Option<Snapshot> {
    let (base, m) = (layout.base, &layout.menus);
    let (focus, resource, name) = focused_menu(layout)?;
    let screen = screen(&name);
    let controller_rva = match screen {
        Screen::Warning => m.license,
        Screen::Title => m.title,
        Screen::Characters => m.characters,
        Screen::Confirm => m.confirm,
        Screen::Dialog | Screen::Other => 0,
    };
    let mut state = Snapshot { screen, focus, cursor: -1, character_count: 0,
        target_available: false, login_context: false, selected: -1, login_confirmation: false };
    // A disabled dialog takes no input (as disconnect.rs requires of it).
    if screen == Screen::Dialog && read::<u8>(focus + MENU_DISABLED)? != 0 { return None; }
    if controller_rva != 0 {
        let controller = pointer(base + controller_rva)?;
        // The focused menu must be this controller's live UI, not a background
        // character list behind a modal or a reused, stale controller.
        if pointer(controller + CONTROLLER_MENU)? != focus || read::<u8>(focus + MENU_DISABLED)? != 0 { return None; }
        state.cursor = i32::from(read::<i16>(focus + MENU_CURSOR)?) - 1;
        if matches!(screen, Screen::Characters | Screen::Confirm) {
            let help = pointer(base + m.help)?;
            let characters = pointer(base + m.characters)?;
            let lobby = pointer(base + layout.lobby)?;
            state.login_context = read::<u32>(help + HELP_MODE)? == HELP_LOGIN;
            // 100efca0 reads 0x8c-byte lobby character entries at +0x13824;
            // their count immediately precedes them. Never read an absent row.
            state.character_count = read::<i32>(lobby + crate::lobby::LOBBY_CHARACTER_COUNT)?;
            let slots = crate::lobby::LOBBY_CHARACTER_SLOTS as i32;
            if (1..=slots).contains(&state.character_count) && i32::from(target) < state.character_count {
                let row = characters + usize::from(target) * ROW_SIZE;
                state.target_available = read::<u32>(row + ROW_OCCUPIED)? != 0
                    && read::<u32>(row + ROW_MARKED)? == 0;
            }
            state.selected = read::<i32>(base + m.selected)?;
            if screen == Screen::Confirm {
                // The generic yes/no menu also serves delete/restore/rename.
                // Only the normal Play confirmation (13, ordinary variant) is safe.
                state.login_confirmation = read::<u32>(controller + CONFIRM_KIND)? == CONFIRM_LOGIN
                    && read::<u8>(controller + CONFIRM_VARIANT)? == 0;
                state.cursor = read::<i32>(controller + CONFIRM_CURSOR)?;
            }
        }
    }
    // Reject a menu switch during this multi-field read.
    menu_unchanged(layout, focus, resource, &name).then_some(state)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key { Enter, Up, Down }
impl Key {
    fn scan(self) -> u32 { match self { Self::Enter => 0x1C, Self::Up => 0xC8, Self::Down => 0xD0 } }
}

/// `Refused` and `Rearm` report a still-logged-in refusal read and its dialog
/// closed, once each; neither sends anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action { Wait, Tap(Key), Finished, Stop(&'static str), Refused, Rearm }

#[derive(Clone, Copy, Debug)]
struct Pending { before: Snapshot, key: Key, sent: Duration }

struct Navigator {
    target: u8,
    pending: Option<Pending>,
    selected: bool,
    confirmed: bool,
    taps: u8,
}

impl Navigator {
    fn new(slot: u8) -> Self { Self { target: slot - 1, pending: None, selected: false, confirmed: false, taps: 0 } }

    fn action(&mut self, state: Snapshot, now: Duration) -> Action {
        if self.taps >= 24 { return Action::Stop("input limit reached; finish login manually"); }
        if let Some(pending) = self.pending {
            // Enter on the license warning or the title connects to the lobby first; the
            // game focuses an unrecognised menu until the next login screen appears.
            let in_transit = !state.screen.login() && pending.key == Key::Enter
                && matches!(pending.before.screen, Screen::Warning | Screen::Title);
            if in_transit {
                return if now.saturating_sub(pending.sent) > Duration::from_secs(60) {
                    Action::Stop("game did not advance from the login menu")
                } else { Action::Wait };
            }
            if state.screen == pending.before.screen && state.focus == pending.before.focus {
                if matches!(pending.key, Key::Up | Key::Down) {
                    let expected = pending.before.cursor + if pending.key == Key::Up { -1 } else { 1 };
                    if state.cursor == expected { self.pending = None; }
                    else if state.cursor != pending.before.cursor { return Action::Stop("selection changed unexpectedly"); }
                    else if now.saturating_sub(pending.sent) > Duration::from_secs(3) { return Action::Stop("game did not accept navigation input"); }
                    else { return Action::Wait; }
                } else if now.saturating_sub(pending.sent) > Duration::from_secs(60) {
                    return Action::Stop("game did not advance from the login menu");
                } else { return Action::Wait; }
            } else {
                self.pending = None;
                match (pending.before.screen, state.screen) {
                    (Screen::Warning, Screen::Title | Screen::Characters)
                    | (Screen::Title, Screen::Characters)
                    | (Screen::Characters, Screen::Confirm) => {}
                    (Screen::Confirm, _) if self.confirmed => return Action::Finished,
                    _ => return Action::Stop("unexpected login screen; finish login manually"),
                }
            }
        }
        match state.screen {
            // A retained POL host can show an initialization menu before the
            // new game's title screen. Wait for a recognized login menu before
            // sending any input; a later unexpected transition still stops us.
            Screen::Dialog | Screen::Other if self.taps == 0 => Action::Wait,
            Screen::Warning | Screen::Title if !self.selected => {
                if state.cursor == 0 { Action::Tap(Key::Enter) }
                else { Action::Stop("login menu selection was changed; finish login manually") }
            }
            Screen::Characters if !self.selected => {
                if !state.login_context { return Action::Stop("character menu is not the Play screen"); }
                if !(1..=16).contains(&state.character_count) || i32::from(self.target) >= state.character_count || !state.target_available {
                    return Action::Stop("chosen character slot is empty or unavailable");
                }
                if state.cursor < 0 || state.cursor >= state.character_count { return Action::Stop("character cursor is outside the list"); }
                match state.cursor.cmp(&i32::from(self.target)) {
                    std::cmp::Ordering::Less => Action::Tap(Key::Down),
                    std::cmp::Ordering::Greater => Action::Tap(Key::Up),
                    std::cmp::Ordering::Equal => Action::Tap(Key::Enter),
                }
            }
            Screen::Confirm if self.selected && !self.confirmed => {
                if !state.login_context || !state.login_confirmation || state.selected != i32::from(self.target) || !state.target_available {
                    return Action::Stop("confirmation does not match the chosen character login");
                }
                match state.cursor { 0 => Action::Tap(Key::Enter), 1 => Action::Tap(Key::Up),
                    _ => Action::Stop("unexpected confirmation selection") }
            }
            _ => Action::Stop("unexpected login screen; finish login manually"),
        }
    }

    fn sent(&mut self, state: Snapshot, key: Key, now: Duration) {
        self.taps += 1;
        if key == Key::Enter {
            if state.screen == Screen::Characters { self.selected = true; }
            if state.screen == Screen::Confirm { self.confirmed = true; }
        }
        self.pending = Some(Pending { before: state, key, sent: now });
    }
}

/// The lobby's answer to the character selection, as GetLobbyError (100ef920)
/// reads it: a nonzero tcp protocol error wins over the lobby error field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply { Unknown, Pending, StillLoggedIn, Failed(i32) }

/// Raw codes the client shows as 3000 + code. The client's own error table
/// (table 7, dialog disposal) maps 3201 and 3307 to the same "Same character is
/// already logged in" message. 201 arrives as the tcp protocol error (observed
/// live after a disconnect); 307 is the lobby server's variant of that message.
const STILL_LOGGED_IN: [i32; 2] = [201, 307];

fn reply(lobby: Option<crate::lobby::Snapshot>) -> Reply {
    let Some(lobby) = lobby else { return Reply::Unknown; };
    // tcp_process ends a failed request with the operation idle and the reply -1.
    if lobby.operation != 0 || lobby.reply_state != -1 { return Reply::Pending; }
    let code = if lobby.protocol_error != 0 { lobby.protocol_error } else { lobby.lobby_error };
    if STILL_LOGGED_IN.contains(&code) { Reply::StillLoggedIn } else { Reply::Failed(code) }
}

/// The server keeps a dropped session for a few minutes (one seen live
/// released after ~30 s). Retries stop at this window or count.
const RELEASE_WINDOW: Duration = Duration::from_secs(300);
const MAX_RETRIES: u32 = 40;
/// After a confirmation, how long to watch for the selection's answer.
const WATCH: Duration = Duration::from_secs(60);
/// The dialog must hold focus, unchanged, this long before Enter: the wait
/// disconnect.rs gives the same menu before its OK.
const DIALOG_SETTLE: Duration = Duration::from_secs(1);
/// A dialog still in focus this long after Enter gets another, PRESSES at most.
const REPRESS: Duration = Duration::from_secs(2);
const PRESSES: u8 = 3;
/// A refusal must bring its dialog into focus, and a dialog that took Enter
/// must give way to a login menu, within this.
const DIALOG_FOCUS: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// A confirmed selection's answer is outstanding.
    Watch,
    /// The selection was refused. `since`: when the refusal was read, or when
    /// Enter last went to its dialog. `presses`: Enters sent to it.
    Refused { since: Duration, presses: u8 },
    /// The dialog closed; the navigator is logging the slot in again.
    Login,
}

/// After a confirmed character login: answers each "still logged in" dialog
/// and re-arms the ordinary navigator, which logs the same slot in again.
struct Retry {
    phase: Phase,
    /// The last confirmation.
    since: Duration,
    /// The first refusal.
    first: Option<Duration>,
    /// Refusals answered with Enter.
    retries: u32,
    /// The game keeps an ended request's reply until its next lobby request,
    /// so a failure in memory is old news until a reply without one is read.
    stale: bool,
}

impl Retry {
    fn new(now: Duration) -> Self { Self { phase: Phase::Watch, since: now, first: None, retries: 0, stale: true } }

    /// The worker keeps running until this time since its start.
    fn limit(&self) -> Duration { self.first.map_or(self.since + WATCH, |first| first + RELEASE_WINDOW) }

    /// The navigator confirmed the character again.
    fn confirmed(&mut self, now: Duration) { self.phase = Phase::Watch; self.since = now; }

    /// Every poll's reply, whatever the menu: a request can start and end
    /// while no menu is settled.
    fn observe(&mut self, reply: Reply) { if reply == Reply::Pending { self.stale = false; } }

    /// Whether this reply is the retry's to handle rather than the navigator's.
    fn owns(&self, reply: Reply) -> bool {
        self.phase != Phase::Login || (reply == Reply::StillLoggedIn && !self.stale)
    }

    /// `held`: how long `menu` has been in focus, unchanged.
    fn action(&mut self, reply: Reply, menu: Snapshot, held: Duration, now: Duration) -> Action {
        const OTHER_ERROR: &str = "character login failed with another lobby error; finish login manually";
        let Phase::Refused { since, presses } = self.phase else {
            return match reply {
                Reply::Unknown | Reply::Pending => Action::Wait,
                _ if self.stale => Action::Wait,
                Reply::Failed(_) => Action::Stop(OTHER_ERROR),
                Reply::StillLoggedIn => {
                    let first = *self.first.get_or_insert(now);
                    if self.retries >= MAX_RETRIES || now.saturating_sub(first) >= RELEASE_WINDOW {
                        return Action::Stop("character is still logged in after the retry window; finish login manually");
                    }
                    self.phase = Phase::Refused { since: now, presses: 0 };
                    Action::Refused
                }
            };
        };
        // After our Enter the dialog closed once a login menu has focus (its
        // reply stays in memory until the next request), or a new request is out.
        if presses > 0 && (menu.screen.login() || reply == Reply::Pending) {
            self.phase = Phase::Login;
            return Action::Rearm;
        }
        match reply {
            Reply::Unknown => return Action::Wait,
            Reply::Failed(_) => return Action::Stop(OTHER_ERROR),
            Reply::Pending => return Action::Stop("still-logged-in dialog was closed by other input; finish login manually"),
            Reply::StillLoggedIn => {}
        }
        let waited = now.saturating_sub(since);
        match menu.screen {
            Screen::Dialog if held < DIALOG_SETTLE || (presses > 0 && waited < REPRESS) => Action::Wait,
            Screen::Dialog if presses >= PRESSES =>
                Action::Stop("still-logged-in dialog stayed open after Enter; finish login manually"),
            Screen::Dialog => Action::Tap(Key::Enter),
            _ if waited > DIALOG_FOCUS =>
                Action::Stop("no still-logged-in dialog or login menu in focus; finish login manually"),
            _ => Action::Wait,
        }
    }

    /// The game took Enter on the dialog. Returns which press of it this was.
    fn sent(&mut self, now: Duration) -> u8 {
        // Answered: what stays in memory is no longer news.
        self.stale = true;
        let Phase::Refused { since, presses } = &mut self.phase else { return 0; };
        (*since, *presses) = (now, *presses + 1);
        if *presses == 1 { self.retries += 1; }
        *presses
    }
}

/// What a key the game took was, for the log. `Dismiss`: which refusal, which
/// Enter on its dialog, and how long after the first refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sent { Key, Dismiss { retry: u32, press: u8, after: Duration }, Confirmed { again: bool }, Finished }

/// Every login decision, made from what each poll read. It reads and sends
/// nothing itself, so a recorded run replays through it exactly.
struct Login {
    slot: u8,
    /// The lobby reply fields are checked by the diagnostics group; without
    /// it a still-logged-in reply is left to the user.
    replies: bool,
    navigator: Navigator,
    retry: Option<Retry>,
    /// The focused menu, and since when it has read the same.
    menu: Option<Snapshot>,
    since: Duration,
}

impl Login {
    fn new(slot: u8) -> Self {
        Self { slot, replies: false, navigator: Navigator::new(slot), retry: None, menu: None, since: Duration::ZERO }
    }

    /// One poll. `menu`: the focused menu, or None while it is unreadable or
    /// the window or held keys rule input out. Returns the menu once it has
    /// settled, and what to do on it.
    fn poll(&mut self, menu: Option<Snapshot>, answer: Reply, now: Duration) -> Option<(Snapshot, Action)> {
        if let Some(retry) = &mut self.retry { retry.observe(answer); }
        if menu != self.menu { (self.menu, self.since) = (menu, now); return None; }
        let held = now.saturating_sub(self.since);
        let menu = menu.filter(|_| held >= SETTLE)?;
        let action = match &mut self.retry {
            Some(retry) if retry.owns(answer) => retry.action(answer, menu, held, now),
            _ => self.navigator.action(menu, now),
        };
        // The dialog closed: log the same slot in again from whichever login
        // menu the game shows next.
        if action == Action::Rearm { self.navigator = Navigator::new(self.slot); }
        Some((menu, action))
    }

    /// A key was queued: the menu settles again before the next decision.
    fn unsettle(&mut self) { self.menu = None; }

    /// The game took `key` on `menu`.
    fn sent(&mut self, menu: Snapshot, key: Key, now: Duration) -> Sent {
        // Only the retry sends a key to the dialog.
        if let (Screen::Dialog, Some(retry)) = (menu.screen, &mut self.retry) {
            let press = retry.sent(now);
            return Sent::Dismiss { retry: retry.retries, press, after: now.saturating_sub(retry.first.unwrap_or(now)) };
        }
        self.navigator.sent(menu, key, now);
        if !self.navigator.confirmed { return Sent::Key; }
        match &mut self.retry {
            Some(retry) => { retry.confirmed(now); Sent::Confirmed { again: true } }
            None if self.replies => { self.retry = Some(Retry::new(now)); Sent::Confirmed { again: false } }
            None => Sent::Finished,
        }
    }
}

/// A menu resource name for the log (fixed game resource names).
fn printable(name: &[u8; 16]) -> String {
    name.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' }).collect()
}

/// The focused menu's resource name for the log.
pub(crate) fn menu_name(layout: &Layout) -> String {
    focused_menu(layout).map_or_else(|| "unreadable".into(), |(_, _, name)| printable(&name))
}

pub(crate) fn game_window(layout: &Layout) -> Option<(HWND, u32)> {
    // GameMain may create its UI on a different thread from GameStart. Read its
    // actual HWND (also passed to DestroyWindow at 10015a57), then validate it.
    let window = read::<u32>(layout.base + layout.menus.window)? as usize as HWND;
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    (thread != 0 && pid == std::process::id() && owned_window(window, thread)).then_some((window, thread))
}

/// The keyboard controller, when `data` is its GetKeyboardState buffer: the
/// keyboard frame of this game, not another caller of the import.
pub(crate) fn frame_keyboard(base: usize, keyboard_rva: usize, data: *mut u8) -> Option<usize> {
    let keyboard = pointer(base.checked_add(keyboard_rva)?)?;
    (keyboard.checked_add(KEYBOARD_BUFFER) == Some(data as usize)).then_some(keyboard)
}

/// `window` is this process's game window (class FFXiClass) on `game_thread`.
pub(crate) fn owned_window(window: HWND, game_thread: u32) -> bool {
    if window.is_null() { return false; }
    let mut pid = 0;
    let mut class = [0u16; 32];
    // No title/name matching: game thread, host process and the game class.
    unsafe {
        IsWindow(window) != 0 && GetWindowThreadProcessId(window, &mut pid) == game_thread
            && pid == std::process::id()
            && GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) == 9
            && class[..9] == [70, 70, 88, 105, 67, 108, 97, 115, 115]
    }
}

pub(crate) fn usable(window: HWND, game_thread: u32) -> bool {
    owned_window(window, game_thread) && unsafe {
        IsWindowVisible(window) != 0
            && IsWindowEnabled(window) != 0 && IsIconic(window) == 0
    }
}

pub(crate) fn keys_down() -> bool {
    // Never combine an automated key with held modifiers, manual navigation,
    // confirmation, Escape or mouse buttons. It can resume once they are up.
    [1, 2, 0x0D, 0x10, 0x11, 0x12, 0x1B, 0x25, 0x26, 0x27, 0x28, 0x5B, 0x5C]
        .iter().any(|key| unsafe { GetAsyncKeyState(*key) } < 0)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tap { Sent, Skipped, Cancelled, Failed }

/// The GameStart this host is running, numbered from 1. A key belongs to one
/// generation; 0 means no login worker is active and nothing may be sent.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The generation whose worker may still send keys; 0 once it has stopped,
/// finished, reached the world or been dropped. Never re-armed for that game.
static ACTIVE: AtomicU64 = AtomicU64::new(0);

/// The client's world-entry "commands enabled" DWORD (game_layout's shutdown
/// group). Nonzero means a character is in the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct World { flag: usize }
impl World {
    fn entered(self, base: usize) -> Option<bool> {
        base.checked_add(self.flag).and_then(read::<u32>).map(|value| value != 0)
    }
}

/// Every condition, apart from the live screen, under which a queued key may
/// still reach the game: the same GameStart, its worker still active, and a
/// readable world flag that says the character is not in the world.
fn admitted(generation: u64, base: usize, world: World) -> Result<(), &'static str> {
    if generation == 0 || GENERATION.load(Ordering::Acquire) != generation {
        return Err("key belongs to an earlier game");
    }
    if ACTIVE.load(Ordering::Acquire) != generation { return Err("login input already stopped for this game"); }
    world_clear(world.entered(base))
}

/// The world flag must be readable and clear: an unreadable or set flag is
/// never treated as a login screen.
fn world_clear(entered: Option<bool>) -> Result<(), &'static str> {
    match entered {
        Some(false) => Ok(()),
        Some(true) => Err("character is in the world"),
        None => Err("world state unreadable"),
    }
}

struct Command {
    layout: Layout, keys: Input, window: usize, game_thread: u32, target: u8,
    expected: Snapshot, key: Key, expires: Instant, result: mpsc::Sender<Tap>,
    generation: u64, world: World,
}

/// A read-only timing probe, independent of the optional input worker. It
/// observes the focused title menu in this host's visible game window.
pub(crate) struct TitleProbe { layout: Layout }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TitleObservation {
    pub(crate) menu: &'static str,
    pub(crate) menu_live: bool,
    pub(crate) window: &'static str,
}
impl TitleObservation {
    pub(crate) fn ready(self) -> bool { self.menu_live && self.window == "visible" }
}

impl TitleProbe {
    /// Read-only, so it needs the menu layout only, never the key input group.
    pub(crate) fn new(resolution: &Resolution) -> Result<Self, String> {
        Layout::resolve(resolution).map(|layout| Self { layout })
    }

    pub(crate) fn observe(&self) -> TitleObservation {
        let window = match game_window(&self.layout) {
            None => "pending",
            Some((window, _)) => unsafe {
                if IsWindowVisible(window) == 0 { "hidden" }
                else if IsIconic(window) != 0 { "minimized" }
                else if IsWindowEnabled(window) == 0 { "disabled" }
                else { "visible" }
            },
        };
        TitleObservation {
            menu: title_focus(&self.layout),
            menu_live: title_menu(&self.layout),
            window,
        }
    }
}

// Fixed menu categories only. This explains a missing timing marker without
// reading names, character rows, menu text or changing the input worker.
fn title_focus(layout: &Layout) -> &'static str {
    match focused_menu(layout) {
        Some((focus, resource, name)) if menu_unchanged(layout, focus, resource, &name) => screen(&name).label(),
        _ => "pending",
    }
}

fn title_menu(layout: &Layout) -> bool {
    snapshot(layout, 0).is_some_and(|state| state.screen == Screen::Title)
}
static COMMAND: Mutex<Option<Command>> = Mutex::new(None);
static ORIGINAL_KEYBOARD: AtomicUsize = AtomicUsize::new(0);

fn command() -> std::sync::MutexGuard<'static, Option<Command>> {
    COMMAND.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(target_arch = "x86")]
type KeyHandler = unsafe extern "thiscall" fn(usize, u32);

#[cfg(target_arch = "x86")]
unsafe fn dispatch_key(context: usize, key: Key, handlers: [KeyHandler; 3]) {
    // Same three calls as the client's ordinary first-key-down path at 10005cf7.
    // Its following keyboard frame performs release; no held Windows key exists.
    for handler in handlers { handler(context, key.scan()); }
}

/// One key through the client's own three key-down handlers on its input
/// bank. Only from the hooked keyboard frame on the game thread, after the
/// caller's live checks.
pub(crate) fn send(base: usize, keys: &Input, key: Key) -> bool {
    #[cfg(target_arch = "x86")]
    unsafe {
        // Only addresses the scan resolved (and a known build's table
        // confirmed), re-checked byte for byte before the hook was installed.
        type Setter = unsafe extern "thiscall" fn(usize, u32);
        dispatch_key(base + keys.input, key,
            [std::mem::transmute::<usize, Setter>(base + keys.pressed),
             std::mem::transmute::<usize, Setter>(base + keys.edge),
             std::mem::transmute::<usize, Setter>(base + keys.repeat)]);
        true
    }
    #[cfg(not(target_arch = "x86"))]
    { let _ = (base, keys, key); false }
}

unsafe extern "system" fn keyboard_state(data: *mut u8) -> i32 {
    let original = ORIGINAL_KEYBOARD.load(Ordering::Acquire);
    let original: unsafe extern "system" fn(*mut u8) -> i32 =
        std::mem::transmute(if original == 0 { GetKeyboardState as *const () as usize } else { original });
    let result = original(data);
    crate::controller_settings::on_input(data);
    crate::game_shutdown::on_input(data);
    crate::disconnect::on_input(data);
    let mut pending = command();
    let Some(input) = pending.as_ref() else { return result; };
    let outcome = deliver(input, data, |_, key| send(input.layout.base, &input.keys, key));
    if let Some(outcome) = outcome {
        let input = pending.take().unwrap();
        let _ = input.result.send(outcome);
    }
    result
}

fn deliver(input: &Command, data: *mut u8, dispatch: impl FnOnce(usize, Key) -> bool) -> Option<Tap> {
    // Imports can be called by other threads too. Only the real game thread
    // may consume this command, directly after its ordinary keyboard update.
    let base = input.layout.base;
    if unsafe { GetCurrentThreadId() } != input.game_thread
        || frame_keyboard(base, input.keys.keyboard, data).is_none() {
        return None;
    }
    let window = input.window as HWND;
    let outcome = if input.expires < Instant::now() || crate::events::pipe_closed() {
        Tap::Cancelled
    } else if let Err(reason) = admitted(input.generation, base, input.world) {
        // Fail closed and permanently: this game's login input is over.
        ACTIVE.store(0, Ordering::Release);
        log!("auto-login: gen={} {:?} key={:?} refused on the game thread: {reason}",
            input.generation, input.expected.screen, input.key);
        Tap::Cancelled
    } else if game_window(&input.layout) != Some((window, input.game_thread))
        || !usable(window, input.game_thread)
        || snapshot(&input.layout, input.target) != Some(input.expected)
        // The dialog gets Enter only while the lobby still reports its refusal.
        || (input.expected.screen == Screen::Dialog
            && reply(crate::lobby::snapshot(base, input.layout.lobby)) != Reply::StillLoggedIn)
        || (unsafe { GetForegroundWindow() } == window && keys_down())
        || read::<[u8; 256]>(base + input.keys.input).is_none_or(|keys| keys.iter().any(|key| key & 7 != 0)) {
        Tap::Skipped
    } else {
        if dispatch(base + input.keys.input, input.key) { Tap::Sent } else { Tap::Failed }
    };
    Some(outcome)
}

struct KeyboardImport { slot: usize, original: usize, users: usize }
type SharedImport = Arc<Mutex<Option<KeyboardImport>>>;
static SHARED_KEYBOARD: LazyLock<SharedImport> = LazyLock::new(|| Arc::new(Mutex::new(None)));

/// A login or live-controller lease on the game's input import is still held.
pub(crate) fn keyboard_leased() -> bool {
    SHARED_KEYBOARD.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|import| import.users != 0)
}

/// FFXiMain.dll was unloaded after its game (game_modules): forget the old
/// image's import slot, so the next mapping's first lease records its own.
pub(crate) fn forget_keyboard_import() {
    let mut guard = SHARED_KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
    if guard.as_ref().is_some_and(|import| import.users == 0) { *guard = None; }
}

/// A lease on the one input import used by login and live controller settings.
/// A later injector can retain our forwarding wrapper after every lease ends;
/// in that case its saved original is never replaced or recursively chained.
pub(crate) struct KeyboardHook { shared: SharedImport, module: Option<ModuleLease> }

struct ModuleLease(usize);
impl ModuleLease {
    fn retain(base: usize) -> Option<Self> {
        let mut module = std::ptr::null_mut();
        // Keep the loaded image alive through import parsing and restoration,
        // including a concurrent GameStart return. This is a counted reference,
        // released with the last hook lease, not a permanent module pin.
        let ok = unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            base as *const u16, &mut module) };
        if ok == 0 { return None; }
        let lease = Self(module as usize);
        (module as usize == base).then_some(lease)
    }
}
impl Drop for ModuleLease {
    fn drop(&mut self) { unsafe { FreeLibrary(self.0 as _); } }
}
impl KeyboardHook {
    fn install(base: usize, keys: &Input) -> Option<Self> {
        // Redirect this game's one import. Keep an injector's prior function;
        // no system-wide hooks or executable-instruction patches are needed.
        let imports = unsafe { crate::iat::import_slots(base, "user32.dll") };
        let slot = imports.iter().find(|slot| slot.name.as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case("GetKeyboardState")))?.slot as usize;
        // The named import must be the slot the verified keyboard frame calls.
        if Some(slot) != base.checked_add(keys.iat_slot) { return None; }
        if !slot.is_multiple_of(std::mem::align_of::<AtomicUsize>()) { return None; }
        Self::acquire(Arc::clone(&SHARED_KEYBOARD), slot, true)
    }
    fn acquire(shared: SharedImport, slot: usize, publish: bool) -> Option<Self> {
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        let current = read::<usize>(slot)?;
        let wrapper = keyboard_state as *const () as usize;
        if let Some(import) = guard.as_mut() {
            if import.slot != slot { return None; }
            if current == wrapper {
                import.users = import.users.checked_add(1)?;
            } else if import.users == 0 && current == import.original {
                if !import.replace(current, wrapper) { return None; }
                import.users = 1;
            } else {
                // The import changed after us. It may still chain to this
                // wrapper, so saving it as our original would create a cycle.
                return None;
            }
        } else {
            if current == 0 || current == wrapper { return None; }
            let import = KeyboardImport { slot, original: current, users: 1 };
            if publish { ORIGINAL_KEYBOARD.store(current, Ordering::Release); }
            if !import.replace(current, wrapper) { return None; }
            *guard = Some(import);
        }
        drop(guard);
        Some(Self { shared, module: None })
    }
}
impl KeyboardImport {
    fn replace(&self, expected: usize, value: usize) -> bool {
        crate::iat::swap(self.slot, expected, value).is_ok_and(|done| done.changed)
    }
}
impl Drop for KeyboardHook {
    fn drop(&mut self) {
        let mut guard = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let Some(import) = guard.as_mut() else { return; };
        import.users = import.users.saturating_sub(1);
        if import.users == 0 {
            if let Some(input) = command().take() { let _ = input.result.send(Tap::Cancelled); }
            let _ = import.replace(keyboard_state as *const () as usize, import.original);
        }
    }
}

pub(crate) fn input_hook(resolution: &Resolution) -> Result<KeyboardHook, String> {
    let module = ModuleLease::retain(resolution.base).ok_or("the game input module is no longer available")?;
    let keys = resolution.input.clone().map_err(|reason| format!("key input: {reason}"))?;
    if !verified_input_layout(resolution.base, &keys) {
        return Err("live key input code differs from the scanned image".into());
    }
    let mut hook = KeyboardHook::install(resolution.base, &keys).ok_or("game input could not be prepared")?;
    hook.module = Some(module);
    Ok(hook)
}

#[allow(clippy::too_many_arguments)]
fn tap(window: HWND, game_thread: u32, layout: Layout, keys: Input, target: u8, expected: Snapshot, key: Key,
    generation: u64, world: World, stop: &mpsc::Receiver<()>) -> Tap {
    if stop.try_recv() != Err(mpsc::TryRecvError::Empty) { return Tap::Cancelled; }
    if admitted(generation, layout.base, world).is_err() { return Tap::Cancelled; }
    let (result, receive) = mpsc::channel();
    let expires = Instant::now() + Duration::from_secs(5);
    *command() = Some(Command { layout, keys, window: window as usize, game_thread, target, expected, key, expires, result, generation, world });
    loop {
        if let Ok(outcome) = receive.try_recv() { return outcome; }
        if stop.recv_timeout(Duration::from_millis(10)) != Err(mpsc::RecvTimeoutError::Timeout) {
            command().take(); return Tap::Cancelled;
        }
        if Instant::now() >= expires { command().take(); return Tap::Failed; }
    }
}

/// The menu layout plus key input, both verified live; then the hook. The
/// world flag is required: without it the world cannot be ruled out, so no
/// login key is ever sent (fail closed).
fn prepare(resolution: &Resolution) -> Result<(Layout, Input, World, KeyboardHook), String> {
    let layout = Layout::resolve(resolution)?;
    let keys = resolution.input.clone().map_err(|reason| format!("key input: {reason}"))?;
    let world = World { flag: resolution.shutdown.as_ref().map_err(|reason| format!("world state: {reason}"))?.enabled };
    let hook = input_hook(resolution)?;
    Ok((layout, keys, world, hook))
}

/// Tests that touch the process-wide input state (generation, queued command,
/// keyboard hook) take this so they run one at a time, as games do.
#[cfg(test)]
pub(crate) fn input_globals() -> std::sync::MutexGuard<'static, ()> {
    static GLOBALS: Mutex<()> = Mutex::new(());
    GLOBALS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts this GameStart's generation: every key queued by an earlier game is
/// discarded and can never be consumed by this one.
fn begin_generation() -> u64 {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    ACTIVE.store(0, Ordering::Release);
    if let Some(stale) = command().take() { let _ = stale.result.send(Tap::Cancelled); }
    generation
}

/// Ends `generation`'s input for good (finished, stopped, dropped, panicked).
struct Ended(u64);
impl Drop for Ended {
    fn drop(&mut self) {
        let _ = ACTIVE.compare_exchange(self.0, 0, Ordering::AcqRel, Ordering::Acquire);
        let mut pending = command();
        if pending.as_ref().is_some_and(|input| input.generation == self.0) {
            if let Some(input) = pending.take() { let _ = input.result.send(Tap::Cancelled); }
        }
    }
}


pub(crate) struct Worker { stop: mpsc::Sender<()>, worker: Option<thread::JoinHandle<()>> }
impl Worker {
    pub(crate) fn start(config: AutoLogin) -> Option<Self> {
        // Every GameStart starts a new generation, even with auto-login off.
        let generation = begin_generation();
        if !config.enabled || !(1..=16).contains(&config.character_slot) { return None; }
        let (stop, receiver) = mpsc::channel();
        ACTIVE.store(generation, Ordering::Release);
        let worker = thread::Builder::new().name("crystal-auto-login".into()).spawn(move || {
            let _ended = Ended(generation);
            let started = Instant::now();
            let mut prepared = None;
            let mut login = Login::new(config.character_slot);
            let target = login.navigator.target;
            log!("auto-login: gen={generation} waiting for this game's login screen (slot {}; background input enabled)", config.character_slot);
            while started.elapsed() < login.retry.as_ref().map_or(LIMIT, Retry::limit) {
                if receiver.recv_timeout(POLL) != Err(mpsc::RecvTimeoutError::Timeout) { return; }
                if crate::events::pipe_closed() { return; }
                if ACTIVE.load(Ordering::Acquire) != generation { log!("auto-login: gen={generation} input stopped"); return; }
                if prepared.is_none() {
                    // None until FFXiMain is loaded and initialized; keep polling.
                    let Some(resolution) = crate::game_layout::ffximain() else { continue; };
                    match prepare(resolution) {
                        Ok(ready) => {
                            log!("auto-login: gen={generation} FFXiMain at {:#x}; world flag at start: {:?}",
                                resolution.base, ready.2.entered(resolution.base));
                            login.replies = match &resolution.diagnostics {
                                Ok(_) => true,
                                Err(reason) => { key_log!("auto-login: gen={generation} still-logged-in retry unavailable ({reason})"); false }
                            };
                            prepared = Some(ready);
                        }
                        Err(reason) => { log!("auto-login: gen={generation} unsupported game layout ({reason}); finish login manually"); return; }
                    }
                }
                let (layout, keys, world, _) = prepared.as_ref().unwrap();
                let (layout, keys, world) = (*layout, *keys, *world);
                // A host that kept FFXiMain loaded from an earlier game may
                // carry the flag over: checked before every poll.
                if let Err(reason) = world_clear(world.entered(layout.base)) {
                    match &login.retry {
                        Some(retry) => key_log!("auto-login: gen={generation} {reason} after {} still-logged-in retries; input finished", retry.retries),
                        None => log!("auto-login: gen={generation} {reason}; login input stopped for this game"),
                    }
                    return;
                }
                let now = started.elapsed();
                // Read on every poll: the reply moves while no menu is settled.
                let lobby = login.retry.as_ref().and_then(|_| crate::lobby::snapshot(layout.base, layout.lobby));
                // No menu counts while the window cannot take input or the user holds a key.
                let window = game_window(&layout).filter(|&(window, game_thread)| usable(window, game_thread)
                    && !(unsafe { GetForegroundWindow() } == window && keys_down()));
                let menu = window.and_then(|_| snapshot(&layout, target));
                let (Some((window, game_thread)), Some((current, action))) = (window, login.poll(menu, reply(lobby), now)) else { continue; };
                match action {
                    Action::Wait => {},
                    Action::Finished => { log!("auto-login: gen={generation} character login confirmed; input finished"); return; }
                    Action::Stop(reason) => {
                        log!("auto-login: gen={generation} {reason} (screen={:?}, menu={}, cursor={}, slot={}, count={}, lobby={lobby:?})",
                            current.screen, menu_name(&layout), current.cursor, config.character_slot, current.character_count);
                        return;
                    }
                    Action::Refused => log!("auto-login: gen={generation} character still logged in; Enter follows once its dialog has held focus for {}s (screen={:?}, menu={}, lobby={lobby:?})",
                        DIALOG_SETTLE.as_secs(), current.screen, menu_name(&layout)),
                    Action::Rearm => key_log!("auto-login: gen={generation} still-logged-in dialog closed; logging in again (menu={})", menu_name(&layout)),
                    Action::Tap(key) => {
                        let outcome = tap(window, game_thread, layout, keys, target, current, key, generation, world, &receiver);
                        if outcome != Tap::Skipped {
                            log!("auto-login: gen={generation} {:?} key={key:?} cursor={} result={outcome:?}",
                                current.screen, current.cursor);
                        }
                        match outcome {
                            Tap::Sent => match login.sent(current, key, started.elapsed()) {
                                Sent::Key => {}
                                // The menu is certain: the game thread re-read it before taking the key.
                                Sent::Dismiss { retry, press, after } => key_log!("auto-login: gen={generation} character still logged in; retry {retry}/{MAX_RETRIES}: Enter {press}/{PRESSES} on menu={} {:.1}s after the first refusal",
                                    printable(OK_MENU), after.as_secs_f32()),
                                Sent::Confirmed { again: false } => log!("auto-login: gen={generation} login confirmation submitted; watching for a still-logged-in reply"),
                                Sent::Confirmed { again: true } => log!("auto-login: gen={generation} login confirmation submitted again; watching the lobby reply"),
                                Sent::Finished => { log!("auto-login: gen={generation} login confirmation submitted; input finished"); return; }
                            },
                            Tap::Skipped => {},
                            Tap::Cancelled => return,
                            Tap::Failed => { log!("auto-login: gen={generation} game did not consume login input; finish login manually"); return; }
                        }
                        login.unsettle();
                    }
                }
            }
            match &login.retry {
                None => log!("auto-login: gen={generation} timed out; finish login manually"),
                Some(Retry { first: None, .. }) => key_log!("auto-login: gen={generation} no still-logged-in reply; input finished"),
                Some(retry) => key_log!("auto-login: gen={generation} retry window ended after {} still-logged-in retries; finish login manually", retry.retries),
            }
            drop(prepared);
        });
        match worker {
            Ok(worker) => Some(Self { stop, worker: Some(worker) }),
            Err(_) => { ACTIVE.store(0, Ordering::Release); None }
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // Stop input before joining: a key already queued is refused on the
        // game thread from here on, whatever the worker is doing.
        ACTIVE.store(0, Ordering::Release);
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_layout::BUILD_6A995428 as OLD;

    const FOCUS_RVA: usize = OLD.menus.focus;
    const LICENSE_RVA: usize = OLD.menus.license;
    const TITLE_RVA: usize = OLD.menus.title;
    const CHARACTERS_RVA: usize = OLD.menus.characters;
    const HELP_RVA: usize = OLD.menus.help;
    const CONFIRM_RVA: usize = OLD.menus.confirm;
    const SELECTED_RVA: usize = OLD.menus.selected;
    const WINDOW_RVA: usize = OLD.menus.window;
    const LOBBY_PTR_RVA: usize = OLD.lobby.ptr;
    /// A world flag inside the fixture, clear of every other field it uses.
    const WORLD_RVA: usize = 0x9800;

    fn keys() -> Input { OLD.input }

    // Owned bytes exercise the real reader without loading FFXI or emitting keys.
    struct Fixture(Vec<u8>);
    impl Fixture {
        fn new() -> Self {
            let mut fixture = Self(vec![0; 0x66_9100]);
            fixture.ptr(FOCUS_RVA, 0x1000);
            fixture.ptr(0x1004, 0x2000);
            for (global, local) in [(LICENSE_RVA, 0x3000), (TITLE_RVA, 0x4000),
                (CHARACTERS_RVA, 0x5000), (HELP_RVA, 0x7000), (CONFIRM_RVA, 0x8000),
                (LOBBY_PTR_RVA, 0x20000)] {
                fixture.ptr(global, local);
                fixture.ptr(local + 8, 0x1000);
            }
            fixture.u32(0x33820, 3);
            fixture.u32(0x7000 + 0x2C, 0x11);
            fixture.u32(0x5000 + 0xB0 + 0x68, 1);
            fixture.u32(SELECTED_RVA, 1);
            fixture.u32(0x8000 + 0x18, 13);
            fixture.ptr(keys().keyboard, 0x9000);
            fixture.menu(b"menu    dbnamese", 2);
            fixture
        }
        fn base(&self) -> usize { self.0.as_ptr() as usize }
        fn layout(&self) -> Layout { Layout { base: self.base(), menus: OLD.menus, lobby: LOBBY_PTR_RVA } }
        fn u32(&mut self, offset: usize, value: u32) { self.0[offset..offset + 4].copy_from_slice(&value.to_le_bytes()); }
        fn ptr(&mut self, offset: usize, local: usize) { self.u32(offset, (self.base() + local).try_into().unwrap()); }
        fn menu(&mut self, name: &[u8; 16], cursor: i16) {
            self.0[0x2046..0x2056].copy_from_slice(name);
            self.0[0x104C..0x104E].copy_from_slice(&cursor.to_le_bytes());
        }
        /// The lobby reply, where the real reader looks for it.
        fn lobby(&mut self, operation: u32, reply_state: i32, protocol_error: i32) {
            use crate::lobby::{CONTEXT_PROTOCOL_ERROR, LOBBY_REPLY, LOBBY_TCP, TCP_CONTEXT, TCP_OPERATION};
            self.ptr(0x20000 + LOBBY_TCP, 0x40000);
            self.ptr(0x40000 + TCP_CONTEXT, 0x90000);
            self.u32(0x40000 + TCP_OPERATION, operation);
            self.u32(0x20000 + LOBBY_REPLY, reply_state as u32);
            self.u32(0x90000 + CONTEXT_PROTOCOL_ERROR, protocol_error as u32);
        }
        fn snapshot(&self) -> Option<Snapshot> { snapshot(&self.layout(), 1) }
    }

    fn state(screen: Screen, cursor: i32) -> Snapshot {
        Snapshot { screen, focus: 123, cursor, character_count: 3, target_available: true,
            login_context: true, selected: 1, login_confirmation: true }
    }
    #[test]
    fn selects_requested_existing_row_and_only_its_login_confirmation() {
        let mut nav = Navigator::new(2);
        let first = state(Screen::Characters, 0);
        assert_eq!(nav.action(first, Duration::ZERO), Action::Tap(Key::Down));
        nav.sent(first, Key::Down, Duration::ZERO);
        assert_eq!(nav.action(first, Duration::from_secs(1)), Action::Wait);
        let target = state(Screen::Characters, 1);
        assert_eq!(nav.action(target, Duration::from_secs(1)), Action::Tap(Key::Enter));
        nav.sent(target, Key::Enter, Duration::from_secs(1));
        let confirm = Snapshot { focus: 456, ..state(Screen::Confirm, 1) };
        assert_eq!(nav.action(confirm, Duration::from_secs(2)), Action::Tap(Key::Up));
        nav.sent(confirm, Key::Up, Duration::from_secs(2));
        let yes = Snapshot { cursor: 0, ..confirm };
        assert_eq!(nav.action(yes, Duration::from_secs(3)), Action::Tap(Key::Enter));
        nav.sent(yes, Key::Enter, Duration::from_secs(3));
        assert_eq!(nav.action(state(Screen::Other, 0), Duration::from_secs(4)), Action::Finished);
    }
    #[test]
    fn refuses_delete_menu_generic_confirmation_empty_slot_and_changed_cursor() {
        for invalid in [Snapshot { login_context: false, ..state(Screen::Characters, 0) },
            Snapshot { target_available: false, ..state(Screen::Characters, 0) },
            Snapshot { character_count: 1, ..state(Screen::Characters, 0) },
            state(Screen::Confirm, 0), state(Screen::Title, 1)] {
            assert!(matches!(Navigator::new(2).action(invalid, Duration::ZERO), Action::Stop(_)));
        }
        let mut nav = Navigator::new(2);
        nav.sent(state(Screen::Characters, 1), Key::Enter, Duration::ZERO);
        let wrong = Snapshot { focus: 456, login_confirmation: false, ..state(Screen::Confirm, 0) };
        assert!(matches!(nav.action(wrong, Duration::from_secs(1)), Action::Stop(_)));
        let mut nav = Navigator::new(2);
        nav.sent(state(Screen::Characters, 0), Key::Down, Duration::ZERO);
        assert!(matches!(nav.action(state(Screen::Characters, 2), Duration::from_secs(1)), Action::Stop(_)));
    }
    #[test]
    fn retained_host_waits_for_the_new_login_screen_before_first_input() {
        let mut nav = Navigator::new(1);
        let initializing = state(Screen::Other, -1);
        assert_eq!(nav.action(initializing, Duration::from_secs(12)), Action::Wait);
        assert_eq!(nav.action(initializing, Duration::from_secs(16)), Action::Wait);
        let title = state(Screen::Title, 0);
        assert_eq!(nav.action(title, Duration::from_secs(17)), Action::Tap(Key::Enter));
        nav.sent(title, Key::Enter, Duration::from_secs(17));
        assert_eq!(nav.action(initializing, Duration::from_secs(18)), Action::Wait);
        let characters = state(Screen::Characters, 0);
        assert_eq!(nav.action(characters, Duration::from_secs(19)), Action::Tap(Key::Enter));
        nav.sent(characters, Key::Enter, Duration::from_secs(19));
        assert!(matches!(nav.action(initializing, Duration::from_secs(20)), Action::Stop(_)),
            "an unexpected screen after choosing a character still requires manual login");
    }
    #[test]
    fn waits_through_the_lobby_connection_between_login_screens() {
        let mut nav = Navigator::new(1);
        let warning = Snapshot { focus: 1, ..state(Screen::Warning, 0) };
        assert_eq!(nav.action(warning, Duration::ZERO), Action::Tap(Key::Enter));
        nav.sent(warning, Key::Enter, Duration::ZERO);
        let connecting = Snapshot { focus: 2, ..state(Screen::Other, -1) };
        assert_eq!(nav.action(connecting, Duration::from_secs(1)), Action::Wait);
        assert_eq!(nav.action(connecting, Duration::from_secs(5)), Action::Wait);
        let title = Snapshot { focus: 3, ..state(Screen::Title, 0) };
        assert_eq!(nav.action(title, Duration::from_secs(5)), Action::Tap(Key::Enter));
        nav.sent(title, Key::Enter, Duration::from_secs(5));
        assert_eq!(nav.action(connecting, Duration::from_secs(6)), Action::Wait);
        let characters = Snapshot { focus: 4, ..state(Screen::Characters, 0) };
        assert_eq!(nav.action(characters, Duration::from_secs(7)), Action::Tap(Key::Enter));
        let mut stuck = Navigator::new(1);
        stuck.sent(warning, Key::Enter, Duration::ZERO);
        assert!(matches!(stuck.action(connecting, Duration::from_secs(61)), Action::Stop(_)));
    }
    #[test]
    fn never_repeats_unacknowledged_input_and_stops_on_timeout() {
        let mut nav = Navigator::new(1);
        let title = state(Screen::Title, 0);
        nav.sent(title, Key::Enter, Duration::ZERO);
        assert_eq!(nav.action(title, Duration::from_secs(10)), Action::Wait);
        assert!(matches!(nav.action(title, Duration::from_secs(61)), Action::Stop(_)));
        let mut nav = Navigator::new(2);
        let row = state(Screen::Characters, 0);
        nav.sent(row, Key::Down, Duration::ZERO);
        assert!(matches!(nav.action(row, Duration::from_secs(4)), Action::Stop(_)));
    }
    #[test]
    fn signatures_include_relocated_operands_and_directinput_arrow_codes() {
        for base in [0x1000_0000usize, 0x1800_0000] {
            let mut bytes = vec![0x3B, 0x05];
            bytes.extend_from_slice(&((base + FOCUS_RVA) as u32).to_le_bytes());
            bytes.extend_from_slice(&[0x75, 0x09]);
            assert!(operand_matches(base, &bytes, &[0x3B, 0x05], FOCUS_RVA, &[0x75, 0x09]));
            assert!(!operand_matches(base + 4, &bytes, &[0x3B, 0x05], FOCUS_RVA, &[0x75, 0x09]));
        }
        assert_eq!(Key::Enter.scan(), 0x1C);
        assert_eq!(Key::Up.scan(), 0xC8);
        assert_eq!(Key::Down.scan(), 0xD0);
        assert!(!owned_window(std::ptr::null_mut(), 0));
        assert_eq!(read::<u32>(0), None);
    }

    #[test]
    fn live_code_must_hold_the_scanned_instructions_at_their_sites() {
        // A fixture laid out like the old build's code at its own sites.
        let mut image = vec![0u8; 0x24_3D00];
        let base = image.as_ptr() as usize;
        let m = OLD.menus;
        let mut put = |at: usize, bytes: &[u8]| image[at..at + bytes.len()].copy_from_slice(bytes);
        let abs = |rva: usize| ((base + rva) as u32).to_le_bytes();
        put(m.sites.focus, &[[0x3B, 0x05].as_slice(), &abs(m.focus), &[0x75, 0x09]].concat());
        put(m.sites.selected, &[[0x0F, 0xBF, 0x40, 0x4C, 0x48, 0xA3].as_slice(), &abs(m.selected)].concat());
        put(m.sites.help, &[[0x8B, 0x0D].as_slice(), &abs(m.help), &[0xC7, 0x41, 0x14, 2, 0, 0, 0]].concat());
        put(m.sites.window, &[[0x8B, 0x0D].as_slice(), &abs(m.window)].concat());
        put(m.sites.confirm13, &[0x83, 0xC0, 0x44, 0x53, 0x50, 0x53, 0x6A, 0x0D]);
        put(m.sites.help11, &[0xC7, 0x42, 0x2C, 0x11, 0, 0, 0]);
        let layout = Layout { base, menus: m, lobby: LOBBY_PTR_RVA };
        assert!(verified_layout(&layout));
        // The new build's sites moved +0x20; the old sites no longer match there.
        let moved = Layout { menus: crate::game_layout::BUILD_6AC37846.menus, ..layout };
        assert!(!verified_layout(&moved));
        let elsewhere = Layout { base: base + 0x1_0000, ..layout };
        assert!(!verified_layout(&elsewhere), "operands must be relocated to this base");
        assert!(!verified_layout(&Layout { base: 0, ..layout }), "unreadable code is never trusted");
    }

    #[test]
    fn live_key_input_code_must_name_the_input_bank_it_writes_through() {
        use crate::game_layout::{KEY_EDGE, KEY_PRESSED, KEY_REPEAT};
        let k = keys();
        let mut image = vec![0u8; 0x12_4000];
        let base = image.as_ptr() as usize;
        let abs = |rva: usize| ((base + rva) as u32).to_le_bytes();
        let rel = |from: usize, to: usize| (to.wrapping_sub(from + 5) as u32).to_le_bytes();
        let mut put = |at: usize, bytes: &[u8]| image[at..at + bytes.len()].copy_from_slice(bytes);
        put(k.frame_call, &[[0xFF, 0x15].as_slice(), &abs(k.iat_slot)].concat());
        put(k.controller_call, &[[0x8B, 0x0D].as_slice(), &abs(k.keyboard)].concat());
        put(k.buffer_site, &[0x8D, 0x8E, 0x98, 2, 0, 0, 0x51]);
        put(k.pressed, &KEY_PRESSED);
        put(k.edge, &KEY_EDGE);
        put(k.repeat, &KEY_REPEAT);
        let d = k.key_down;
        for at in [0, 0xC, 0x1A, 0x25] {
            put(d + at, &[[0xB9].as_slice(), &abs(k.input)].concat());
        }
        for (at, target) in [(0x14, k.pressed), (0x1F, k.edge), (0x2A, k.repeat)] {
            put(d + at, &[[0xE8].as_slice(), &rel(d + at, target)].concat());
        }
        assert!(verified_input_layout(base, &k));
        // A table that copied INPUT from an older build, with the bank moved:
        // every other check passes, but the client's own code names another object.
        let moved = Input { input: k.input + 0x40, ..k };
        assert!(!verified_input_layout(base, &moved), "the input bank must be the one the game passes");
        // One of the four operands names another object: refused.
        image[d + 0x26..d + 0x2A].copy_from_slice(&abs(k.input + 0x40));
        assert!(!verified_input_layout(base, &k));
        image[d + 0x26..d + 0x2A].copy_from_slice(&abs(k.input));
        assert!(verified_input_layout(base, &k));
        let other_handler = Input { repeat: k.edge, ..k };
        assert!(!verified_input_layout(base, &other_handler), "each call must land on its handler");
        assert!(!verified_input_layout(base + 0x1_0000, &k), "operands must be relocated to this base");
    }

    #[test]
    fn cancellation_before_queuing_does_not_leave_input() {
        let _one = generations();
        let (send, receive) = mpsc::channel();
        send.send(()).unwrap();
        let layout = Layout { base: 0, menus: OLD.menus, lobby: LOBBY_PTR_RVA };
        let world = World { flag: WORLD_RVA };
        assert_eq!(tap(std::ptr::null_mut(), 0, layout, keys(), 0, state(Screen::Title, 0), Key::Enter, 1, world, &receive), Tap::Cancelled);
        drop(send);
        assert_eq!(tap(std::ptr::null_mut(), 0, layout, keys(), 0, state(Screen::Title, 0), Key::Enter, 1, world, &receive), Tap::Cancelled);
        assert!(command().is_none());
    }

    /// GENERATION/ACTIVE/COMMAND are process-wide, as in the real host; the
    /// tests that drive them run one at a time.
    fn generations() -> std::sync::MutexGuard<'static, ()> { super::input_globals() }

    /// A Command for the fixture's dbnamese screen, as the worker would queue it.
    fn queued(fixture: &Fixture, generation: u64, key: Key) -> (Command, mpsc::Receiver<Tap>) {
        let (result, receive) = mpsc::channel();
        (Command { layout: fixture.layout(), keys: keys(), window: 0,
            game_thread: unsafe { GetCurrentThreadId() }, target: 1,
            expected: fixture.snapshot().unwrap(), key, expires: Instant::now() + Duration::from_secs(30),
            result, generation, world: World { flag: WORLD_RVA } }, receive)
    }
    fn buffer(fixture: &Fixture) -> *mut u8 { (fixture.base() + 0x9298) as *mut u8 }

    #[test]
    fn relaunch_in_the_same_process_never_delivers_the_first_games_key() {
        let _one = generations();
        let fixture = Fixture::new();
        let first = begin_generation();
        ACTIVE.store(first, Ordering::Release);
        let (input, _receive) = queued(&fixture, first, Key::Down);
        // Game 1 returned and game 2's GameStart began in this same host.
        let second = begin_generation();
        ACTIVE.store(second, Ordering::Release);
        assert!(second > first);
        assert_eq!(deliver(&input, buffer(&fixture), |_, _| panic!("game 1's key reached game 2")), Some(Tap::Cancelled));
        assert_eq!(ACTIVE.load(Ordering::Acquire), 0, "a refused key ends this game's login input for good");
        // Even the current generation sends nothing once its input stopped.
        let (current, _receive) = queued(&fixture, second, Key::Enter);
        assert_eq!(deliver(&current, buffer(&fixture), |_, _| panic!("stopped input was re-armed")), Some(Tap::Cancelled));
    }

    #[test]
    fn a_pending_enter_is_discarded_at_the_next_gamestart() {
        let _one = generations();
        let fixture = Fixture::new();
        let first = begin_generation();
        ACTIVE.store(first, Ordering::Release);
        let (input, receive) = queued(&fixture, first, Key::Enter);
        *command() = Some(input);
        let second = begin_generation();
        assert_eq!(receive.try_recv(), Ok(Tap::Cancelled), "the queued Enter is answered, not kept");
        assert!(command().is_none(), "nothing from game 1 is left for game 2's keyboard frame");
        // A worker that ends drops only its own generation's command.
        ACTIVE.store(second, Ordering::Release);
        let (mine, receive) = queued(&fixture, second, Key::Enter);
        *command() = Some(mine);
        drop(Ended(first));
        assert!(command().is_some(), "an older worker cannot cancel the current game's command");
        assert_eq!(ACTIVE.load(Ordering::Acquire), second);
        drop(Ended(second));
        assert_eq!(receive.try_recv(), Ok(Tap::Cancelled));
        assert!(command().is_none());
        assert_eq!(ACTIVE.load(Ordering::Acquire), 0);
    }

    #[test]
    fn no_key_is_sent_once_the_character_is_in_the_world() {
        let _one = generations();
        let mut fixture = Fixture::new();
        let generation = begin_generation();
        ACTIVE.store(generation, Ordering::Release);
        let world = World { flag: WORLD_RVA };
        assert_eq!(admitted(generation, fixture.base(), world), Ok(()));
        // World entry sets the flag. Even a screen that still reads as a login
        // menu gets nothing, and the game's own thread is the one that refuses.
        fixture.u32(WORLD_RVA, 1);
        assert_eq!(world_clear(world.entered(fixture.base())), Err("character is in the world"));
        let (input, _receive) = queued(&fixture, generation, Key::Enter);
        assert_eq!(deliver(&input, buffer(&fixture), |_, _| panic!("key sent in the world")), Some(Tap::Cancelled));
        assert_eq!(ACTIVE.load(Ordering::Acquire), 0);
        // Clearing the flag later (a logout) does not re-arm this game.
        fixture.u32(WORLD_RVA, 0);
        assert!(admitted(generation, fixture.base(), world).is_err());
        // An unreadable flag is never taken as "not in the world".
        ACTIVE.store(generation, Ordering::Release);
        assert_eq!(admitted(generation, 0, World { flag: 0 }), Err("world state unreadable"));
        assert_eq!(world_clear(None), Err("world state unreadable"));
        ACTIVE.store(0, Ordering::Release);
    }

    #[test]
    fn misidentified_or_unknown_screens_never_produce_a_key() {
        // Every in-world or unrecognised menu is Screen::Other.
        for name in [b"menu    mogmenu0", b"menu    jobchang", b"menu    ptc6yes0", b"menu    loby2wi ", b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"] {
            assert_eq!(screen(name), Screen::Other);
        }
        // The navigator never answers a dialog: it is one more unknown screen to it.
        assert_eq!(screen(OK_MENU), Screen::Dialog);
        for other in [state(Screen::Other, 2), bare(Screen::Dialog, 0x900)] {
            // Before any input: wait, never send.
            assert_eq!(Navigator::new(1).action(other, Duration::ZERO), Action::Wait);
            // After the character is chosen, an unknown screen stops for good.
            let mut nav = Navigator::new(1);
            nav.sent(state(Screen::Characters, 0), Key::Enter, Duration::ZERO);
            assert!(matches!(nav.action(other, Duration::from_secs(1)), Action::Stop(_)));
        }
        let other = state(Screen::Other, 2);
        // After the confirmation, anything is the end of login.
        let mut nav = Navigator::new(1);
        nav.sent(state(Screen::Characters, 0), Key::Enter, Duration::ZERO);
        nav.sent(state(Screen::Confirm, 0), Key::Enter, Duration::from_secs(1));
        assert!(matches!(nav.action(state(Screen::Characters, 0), Duration::from_secs(2)), Action::Finished | Action::Stop(_)));
        // The 60 s lobby wait after Title/Warning Enter sends nothing and then stops.
        let mut nav = Navigator::new(1);
        nav.sent(state(Screen::Title, 0), Key::Enter, Duration::ZERO);
        for second in 1..=60 { assert_eq!(nav.action(other, Duration::from_secs(second)), Action::Wait); }
        assert!(matches!(nav.action(other, Duration::from_secs(61)), Action::Stop(_)));
        // A queued key whose screen no longer reads back exactly is skipped on
        // the game thread (the window and screen checks are both required).
        let _one = generations();
        let mut fixture = Fixture::new();
        let generation = begin_generation();
        ACTIVE.store(generation, Ordering::Release);
        let (input, _receive) = queued(&fixture, generation, Key::Down);
        fixture.menu(b"menu    mogmenu0", 2);
        assert_eq!(deliver(&input, buffer(&fixture), |_, _| panic!("key sent to an unknown menu")), Some(Tap::Skipped));
        ACTIVE.store(0, Ordering::Release);
    }

    #[test]
    fn slot_one_login_is_enter_only() {
        // The whole slot-1 sequence: no arrow key unless the game's own
        // confirmation cursor sits on "No".
        let mut nav = Navigator::new(1);
        let mut keys = Vec::new();
        for (screen, focus) in [(Screen::Warning, 1), (Screen::Title, 2), (Screen::Characters, 3), (Screen::Confirm, 4)] {
            let current = Snapshot { focus, selected: 0, ..state(screen, 0) };
            let Action::Tap(key) = nav.action(current, Duration::ZERO) else { panic!("{screen:?}") };
            nav.sent(current, key, Duration::ZERO);
            keys.push(key);
        }
        assert_eq!(keys, [Key::Enter; 4]);
        assert!(nav.confirmed);
    }

    #[cfg(target_pointer_width = "32")]
    fn lobby(operation: u32, reply_state: i32, lobby_error: i32, protocol_error: i32) -> Option<crate::lobby::Snapshot> {
        // Built through the real reader's layout: only the decision fields matter.
        let mut lobby = vec![0u8; 0x13700];
        let mut tcp = vec![0u8; 0x4F200];
        let mut context = vec![0u8; 0x140];
        let put = |bytes: &mut Vec<u8>, at: usize, value: u32| bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        put(&mut tcp, crate::lobby::TCP_OPERATION, operation);
        put(&mut tcp, crate::lobby::TCP_CONTEXT, context.as_mut_ptr() as usize as u32);
        put(&mut context, crate::lobby::CONTEXT_PROTOCOL_ERROR, protocol_error as u32);
        put(&mut lobby, crate::lobby::LOBBY_TCP, tcp.as_ptr() as usize as u32);
        put(&mut lobby, crate::lobby::LOBBY_REPLY, reply_state as u32);
        put(&mut lobby, crate::lobby::LOBBY_ERROR, lobby_error as u32);
        let root = lobby.as_ptr() as usize as u32;
        let base = (&root as *const u32 as usize).checked_sub(LOBBY_PTR_RVA).unwrap();
        crate::lobby::snapshot(base, LOBBY_PTR_RVA)
    }

    #[test]
    #[cfg(target_pointer_width = "32")]
    fn lobby_reply_follows_the_games_error_getter() {
        // Live: op=0 reply_state=-1 lobby_error=0 protocol_error=201 after a disconnect.
        assert_eq!(reply(lobby(0, -1, 0, 201)), Reply::StillLoggedIn);
        assert_eq!(reply(lobby(0, -1, 307, 0)), Reply::StillLoggedIn);
        assert_eq!(reply(lobby(0, -1, 201, 100)), Reply::Failed(100), "the protocol error wins, as in 100ef920");
        assert_eq!(reply(lobby(0, -1, 313, 0)), Reply::Failed(313));
        assert_eq!(reply(lobby(5, 1, 0, 0)), Reply::Pending, "character selection outstanding");
        assert_eq!(reply(lobby(5, -1, 0, 201)), Reply::Pending, "the request has not ended yet");
        assert_eq!(reply(lobby(0, 0, 0, 0)), Reply::Pending);
        assert_eq!(reply(None), Reply::Unknown);
    }

    /// A menu without a controller, as the real reader returns it.
    fn bare(screen: Screen, focus: usize) -> Snapshot {
        Snapshot { screen, focus, cursor: -1, character_count: 0, target_available: false,
            login_context: false, selected: -1, login_confirmation: false }
    }

    #[test]
    fn still_logged_in_presses_enter_only_on_its_settled_dialog() {
        let s = Duration::from_secs;
        let (dialog, other) = (bare(Screen::Dialog, 0x900), bare(Screen::Other, 0x700));
        let mut retry = Retry::new(s(10));
        assert!(retry.owns(Reply::Pending), "the confirmation's answer belongs to the retry");
        retry.observe(Reply::Pending);
        assert_eq!(retry.action(Reply::Pending, other, s(2), s(12)), Action::Wait);
        assert_eq!(retry.action(Reply::Unknown, other, s(3), s(13)), Action::Wait);
        // The refusal is read before its dialog has focus: reported once, and
        // no other menu ever gets its Enter.
        assert_eq!(retry.action(Reply::StillLoggedIn, other, s(5), s(15)), Action::Refused);
        for menu in [other, state(Screen::Confirm, 0), state(Screen::Characters, 0), state(Screen::Title, 0)] {
            assert_eq!(retry.action(Reply::StillLoggedIn, menu, s(5), s(16)), Action::Wait, "{:?}", menu.screen);
        }
        // The dialog, once it has held focus for the settle time.
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, SETTLE, s(16)), Action::Wait);
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, DIALOG_SETTLE - POLL, s(17)), Action::Wait);
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, DIALOG_SETTLE, s(17)), Action::Tap(Key::Enter));
        assert_eq!((retry.sent(s(17)), retry.retries), (1, 1));
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, DIALOG_SETTLE, s(18)), Action::Wait, "no second Enter yet");
        // A login menu in focus: the dialog closed, though its reply stays in memory.
        assert_eq!(retry.action(Reply::StillLoggedIn, state(Screen::Title, 0), SETTLE, s(18)), Action::Rearm);
        for reply in [Reply::StillLoggedIn, Reply::Pending, Reply::Failed(313), Reply::Unknown] {
            assert!(!retry.owns(reply), "{reply:?} leaves the re-armed navigator alone");
        }
        // The navigator confirmed again. The old reply is not this selection's
        // answer until its request has been seen.
        retry.confirmed(s(21));
        assert!(retry.owns(Reply::StillLoggedIn));
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, s(5), s(22)), Action::Wait);
        retry.observe(Reply::Pending);
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, s(5), s(27)), Action::Refused);
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, s(5), s(27)), Action::Tap(Key::Enter));
        assert_eq!((retry.sent(s(27)), retry.retries), (1, 2));
        assert_eq!(retry.limit(), s(15) + RELEASE_WINDOW, "bounded from the first refusal");
        // A new request after our Enter also means the dialog closed, and a
        // refusal that follows without a navigator confirmation is the retry's.
        retry.observe(Reply::Pending);
        assert_eq!(retry.action(Reply::Pending, other, s(1), s(28)), Action::Rearm);
        assert!(!retry.owns(Reply::Pending) && retry.owns(Reply::StillLoggedIn));
    }

    #[test]
    fn still_logged_in_retries_are_bounded_and_hand_back_on_other_input_or_errors() {
        let s = Duration::from_secs;
        let (dialog, other, characters) = (bare(Screen::Dialog, 0x900), bare(Screen::Other, 0x700), state(Screen::Characters, 0));
        let stops = |retry: &mut Retry, reply: Reply, menu: Snapshot, now: Duration|
            matches!(retry.action(reply, menu, s(5), now), Action::Stop(_));
        // A retry whose refusal was read at `at`.
        let refused = |at: Duration| {
            let mut retry = Retry::new(s(0));
            retry.observe(Reply::Pending);
            assert_eq!(retry.action(Reply::StillLoggedIn, other, s(5), at), Action::Refused);
            retry
        };
        assert_eq!(Retry::new(s(0)).limit(), WATCH, "no refusal: watch the confirmation only");
        // A failure already in memory at the confirmation is not its answer.
        let mut retry = Retry::new(s(0));
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, s(5), s(1)), Action::Wait);
        assert_eq!(retry.action(Reply::Failed(313), other, s(5), s(1)), Action::Wait);
        // The window from the first refusal.
        let mut retry = refused(s(5));
        assert_eq!(retry.action(Reply::StillLoggedIn, dialog, s(5), s(6)), Action::Tap(Key::Enter));
        retry.sent(s(6));
        retry.observe(Reply::Pending);
        assert_eq!(retry.action(Reply::Pending, other, s(5), s(7)), Action::Rearm);
        assert!(stops(&mut retry, Reply::StillLoggedIn, dialog, s(5) + RELEASE_WINDOW));
        // The count.
        let mut retry = Retry::new(s(0));
        retry.observe(Reply::Pending);
        retry.retries = MAX_RETRIES;
        assert!(stops(&mut retry, Reply::StillLoggedIn, dialog, s(1)));
        // The user closed the dialog themselves and went on.
        let mut retry = refused(s(1));
        assert!(stops(&mut retry, Reply::Pending, characters, s(2)));
        // The dialog never takes focus.
        let mut retry = refused(s(1));
        assert_eq!(retry.action(Reply::StillLoggedIn, characters, s(5), s(1) + DIALOG_FOCUS), Action::Wait);
        assert!(stops(&mut retry, Reply::StillLoggedIn, characters, s(2) + DIALOG_FOCUS));
        // After Enter, neither the dialog nor a login menu takes focus.
        let mut retry = refused(s(1));
        retry.sent(s(2));
        assert_eq!(retry.action(Reply::StillLoggedIn, other, s(5), s(2) + DIALOG_FOCUS), Action::Wait);
        assert!(stops(&mut retry, Reply::StillLoggedIn, other, s(3) + DIALOG_FOCUS));
        // Closed is read from the menu: an unreadable reply does not hide it.
        let mut retry = refused(s(1));
        retry.sent(s(2));
        assert_eq!(retry.action(Reply::Unknown, dialog, s(5), s(5)), Action::Wait);
        assert_eq!(retry.action(Reply::Unknown, state(Screen::Title, 0), s(5), s(5)), Action::Rearm);
        // Any other lobby error is left to the user: as the answer, or after the refusal.
        let mut retry = Retry::new(s(0));
        retry.observe(Reply::Pending);
        assert!(stops(&mut retry, Reply::Failed(313), other, s(3)));
        let mut retry = refused(s(1));
        retry.sent(s(2));
        assert!(stops(&mut retry, Reply::Failed(313), dialog, s(3)));
    }

    #[test]
    fn nothing_is_decided_until_a_readable_menu_has_settled() {
        let ms = Duration::from_millis;
        let title = Snapshot { cursor: 0, ..bare(Screen::Title, 0x200) };
        let mut login = Login::new(1);
        for at in [0, 100, 200] { assert_eq!(login.poll(Some(title), Reply::Unknown, ms(at)), None); }
        // Unreadable, or the window or held keys ruling input out: the wait starts again.
        assert_eq!(login.poll(None, Reply::Unknown, ms(300)), None);
        assert_eq!(login.poll(None, Reply::Unknown, ms(900)), None);
        for at in [1000, 1100, 1200] { assert_eq!(login.poll(Some(title), Reply::Unknown, ms(at)), None); }
        assert_eq!(login.poll(Some(title), Reply::Unknown, ms(1000) + SETTLE), Some((title, Action::Tap(Key::Enter))));
        // A queued key, taken or not, makes the menu settle again.
        login.unsettle();
        assert_eq!(login.poll(Some(title), Reply::Unknown, ms(1300)), None);
        assert_eq!(login.poll(Some(title), Reply::Unknown, ms(1300) + SETTLE), Some((title, Action::Tap(Key::Enter))));
        // Without readable lobby replies the confirmation ends the input, as before.
        login.sent(state(Screen::Characters, 0), Key::Enter, ms(2000));
        assert_eq!(login.sent(state(Screen::Confirm, 0), Key::Enter, ms(2400)), Sent::Finished);
    }

    /// Recorded runs replayed through the worker's own decisions at its poll
    /// rate, with the lobby states read by the real reader.
    #[cfg(target_pointer_width = "32")]
    mod replay {
        use super::*;

        /// What the game read from a moment on: the focused menu, or the lobby's
        /// operation, reply state, lobby error and protocol error.
        #[derive(Clone, Copy)]
        enum Read { Menu(Option<Snapshot>), Lobby(u32, i32, i32, i32) }

        const ENTER: Action = Action::Tap(Key::Enter);

        /// A log time, "hh:mm:ss.mmm".
        fn clock(stamp: &str) -> Duration {
            let n: Vec<u64> = stamp.split([':', '.']).map(|part| part.parse().unwrap()).collect();
            Duration::from_millis(((n[0] * 60 + n[1]) * 60 + n[2]) * 1000 + n[3])
        }

        /// A slot-1 worker whose confirmation the game took at `at`.
        fn confirmed(at: Duration) -> Login {
            let mut login = Login::new(1);
            login.replies = true;
            login.sent(Snapshot { selected: 0, ..state(Screen::Characters, 0) }, Key::Enter, at);
            assert_eq!(login.sent(Snapshot { selected: 0, ..state(Screen::Confirm, 0) }, Key::Enter, at), Sent::Confirmed { again: false });
            login
        }

        /// Polls every POLL from `from` to `until`, or to a stop. `game` answers
        /// each key it took with what it reads afterwards. Returns every
        /// decision other than Wait: its time, the menu it was made on, and it.
        fn run(login: &mut Login, mut script: Vec<(Duration, Read)>, from: Duration, until: Duration,
            mut game: impl FnMut(Duration, Snapshot) -> Vec<(Duration, Read)>) -> Vec<(Duration, Screen, Action)> {
            let (mut menu, mut answer) = (None, None);
            let mut done = Vec::new();
            let mut now = from;
            while now < until {
                script.sort_by_key(|(at, _)| *at);
                while script.first().is_some_and(|(at, _)| *at <= now) {
                    match script.remove(0).1 {
                        Read::Menu(read) => menu = read,
                        Read::Lobby(operation, reply_state, lobby_error, protocol_error) =>
                            answer = lobby(operation, reply_state, lobby_error, protocol_error),
                    }
                }
                if let Some((current, action)) = login.poll(menu, reply(answer), now) {
                    if action != Action::Wait { done.push((now, current.screen, action)); }
                    match action {
                        Action::Tap(key) => {
                            script.extend(game(now, current));
                            login.sent(current, key, now);
                            login.unsettle();
                        }
                        Action::Stop(_) | Action::Finished => break,
                        _ => {}
                    }
                }
                now += POLL;
            }
            done
        }

        #[test]
        fn enter_waits_for_the_dialog_that_follows_the_refusal() {
            // Live, v1.2.8-test10, PID 30256: the worker started at 01:01:51.336
            // and the game took its confirmation at 01:02:00.896.
            let t = |stamp: &str| clock(stamp) - clock("01:01:51.336");
            let (other, dialog) = (bare(Screen::Other, 0x700), bare(Screen::Dialog, 0x900));
            let recorded = |gap: bool| {
                let mut script = vec![
                    // An unrecognised menu holds focus while the selection is out.
                    (t("01:02:00.896"), Read::Menu(Some(other))),
                    (t("01:02:00.896"), Read::Lobby(0, 0, 0, 0)),
                    (t("01:02:02.565"), Read::Lobby(9, 0, 0, 0)),
                    (t("01:02:02.815"), Read::Lobby(5, 1, 0, 0)),
                    // The worker's poll that read the refusal, that menu still settled.
                    // The old code sent Enter here and the game took it, to no effect.
                    (t("01:02:07.949"), Read::Lobby(0, -1, 0, 201)),
                    // No menu was readable right after; the dialog had focus by 01:02:08.395.
                    (t("01:02:07.950"), Read::Menu(None)),
                    (t("01:02:08.395"), Read::Menu(Some(dialog))),
                ];
                // Without the gap, that menu stays settled until the dialog replaces it.
                if !gap { script.retain(|(_, read)| !matches!(read, Read::Menu(None))); }
                script
            };
            // The dialog stays in focus, as it did live, whatever is sent.
            let mut login = confirmed(t("01:02:00.896"));
            let done = run(&mut login, recorded(true), t("01:02:00.949"), t("01:02:20.000"), |_, _| vec![]);
            assert_eq!(done, [
                (t("01:02:07.949"), Screen::Other, Action::Refused),
                (t("01:02:09.449"), Screen::Dialog, ENTER),
                (t("01:02:11.449"), Screen::Dialog, ENTER),
                (t("01:02:13.449"), Screen::Dialog, ENTER),
                (t("01:02:15.449"), Screen::Dialog, Action::Stop("still-logged-in dialog stayed open after Enter; finish login manually")),
            ]);
            assert_eq!(login.retry.as_ref().map(|retry| retry.retries), Some(1), "one refusal, however many presses");
            // Whatever the poll phase: a key goes only to the dialog, and only
            // once it has held focus for the settle time.
            for (phase, gap) in (0..100).flat_map(|phase| [(phase, true), (phase, false)]) {
                let mut login = confirmed(t("01:02:00.896"));
                let from = t("01:02:00.897") + Duration::from_millis(phase);
                let done = run(&mut login, recorded(gap), from, t("01:02:12.000"), |_, _| vec![]);
                let keys: Vec<_> = done.iter().filter(|(_, _, action)| *action == ENTER).collect();
                assert_eq!(keys.len(), 2, "phase {phase}");
                for (at, screen, _) in keys {
                    assert!(*screen == Screen::Dialog && *at >= t("01:02:08.395") + DIALOG_SETTLE, "phase {phase}: {at:?} on {screen:?}");
                }
            }
            // The first Enter is lost and the second taken. The dialog closes as
            // it did in test9: gone 124 ms after its Enter, the title menu next.
            let title = Snapshot { cursor: 0, ..bare(Screen::Title, 0x200) };
            let mut keys = 0;
            let mut login = confirmed(t("01:02:00.896"));
            let done = run(&mut login, recorded(true), t("01:02:00.949"), t("01:02:12.500"), |now, _| {
                keys += 1;
                if keys == 2 { vec![(now + Duration::from_millis(124), Read::Menu(Some(title)))] } else { vec![] }
            });
            assert_eq!(done[1..], [
                (t("01:02:09.449"), Screen::Dialog, ENTER),
                (t("01:02:11.449"), Screen::Dialog, ENTER),
                (t("01:02:11.949"), Screen::Title, Action::Rearm),
                (t("01:02:12.049"), Screen::Title, ENTER),
            ]);
        }

        #[test]
        fn the_dismissed_refusal_left_in_memory_is_not_a_new_one() {
            // Live, v1.2.8-test9, PID 20344: the worker started at 00:52:53.679
            // and the game took its confirmation at 00:53:03.334.
            let t = |stamp: &str| clock(stamp) - clock("00:52:53.679");
            let (other, dialog) = (bare(Screen::Other, 0x700), bare(Screen::Dialog, 0x900));
            let title = Snapshot { cursor: 0, ..bare(Screen::Title, 0x200) };
            let characters = Snapshot { focus: 0x300, selected: 0, ..state(Screen::Characters, 0) };
            let confirm = Snapshot { focus: 0x400, selected: 0, ..state(Screen::Confirm, 0) };
            let recorded = vec![
                (t("00:53:03.334"), Read::Menu(Some(other))),
                (t("00:53:03.334"), Read::Lobby(0, 0, 0, 0)),
                (t("00:53:05.163"), Read::Lobby(5, 0, 0, 0)),
                (t("00:53:05.413"), Read::Lobby(5, 1, 0, 0)),
                // The refusal. No menu settled until its dialog, in focus by 00:53:10.742.
                (t("00:53:10.425"), Read::Lobby(0, -1, 0, 201)),
                (t("00:53:10.425"), Read::Menu(None)),
                (t("00:53:10.742"), Read::Menu(Some(dialog))),
            ];
            let mut confirmations = 0;
            let game = |now: Duration, menu: Snapshot| {
                let after = |delay: u64, read: Read| (now + Duration::from_millis(delay), read);
                match menu.screen {
                    // Recorded: the dialog was gone 124 ms after the Enter it took. The
                    // title menu then held focus with the refusal still in memory,
                    // where the old code gave up (00:53:16.051).
                    Screen::Dialog => vec![after(124, Read::Menu(Some(title)))],
                    // Not yet seen live after a refusal: the title's Enter is modelled
                    // on this run's own first connection and login.
                    Screen::Title => vec![after(100, Read::Menu(Some(other))), after(590, Read::Lobby(2, 1, 0, 0)),
                        after(893, Read::Lobby(0, 0, 0, 0)), after(1900, Read::Menu(Some(characters)))],
                    Screen::Characters => vec![after(100, Read::Menu(Some(confirm)))],
                    // Recorded after this run's confirmation: the selection at +1.829 s
                    // and +2.079 s, its refusal at +7.091 s, the dialog at +7.408 s.
                    Screen::Confirm => {
                        confirmations += 1;
                        let mut reads = vec![after(100, Read::Menu(Some(other))),
                            after(1829, Read::Lobby(5, 0, 0, 0)), after(2079, Read::Lobby(5, 1, 0, 0))];
                        if confirmations == 1 {
                            reads.extend([after(7091, Read::Lobby(0, -1, 0, 201)), after(7091, Read::Menu(None)),
                                after(7408, Read::Menu(Some(dialog)))]);
                        } else {
                            // The server released the session: accepted, as in PID 6184.
                            reads.push(after(2330, Read::Lobby(14, 0, 0, 0)));
                        }
                        reads
                    }
                    _ => vec![],
                }
            };
            let mut login = confirmed(t("00:53:03.334"));
            let done = run(&mut login, recorded, t("00:53:03.421"), t("00:53:40.000"), game);
            assert_eq!(done[..4], [
                (t("00:53:11.121"), Screen::Dialog, Action::Refused),
                (t("00:53:11.821"), Screen::Dialog, ENTER),
                (t("00:53:12.321"), Screen::Title, Action::Rearm),
                // The old reply (201) is still in memory: the navigator acts all the same.
                (t("00:53:12.421"), Screen::Title, ENTER),
            ]);
            let steps: Vec<_> = done.iter().map(|(_, screen, action)| (*screen, *action)).collect();
            let attempt = [(Screen::Dialog, Action::Refused), (Screen::Dialog, ENTER), (Screen::Title, Action::Rearm),
                (Screen::Title, ENTER), (Screen::Characters, ENTER), (Screen::Confirm, ENTER)];
            assert_eq!(steps, [attempt, attempt].concat(), "refused twice, then accepted: nothing more is sent");
            let retry = login.retry.as_ref().unwrap();
            assert_eq!((retry.retries, retry.phase, retry.first), (2, Phase::Watch, Some(t("00:53:11.121"))));
        }

        #[test]
        fn a_request_that_runs_while_no_menu_is_settled_still_makes_its_refusal_new() {
            let ms = Duration::from_millis;
            let (dialog, again) = (bare(Screen::Dialog, 0x900), bare(Screen::Dialog, 0x980));
            let title = Snapshot { cursor: 0, ..bare(Screen::Title, 0x200) };
            let script = vec![(ms(0), Read::Menu(Some(dialog))), (ms(0), Read::Lobby(5, 1, 0, 0)), (ms(500), Read::Lobby(0, -1, 0, 201))];
            let mut titles = 0;
            let mut login = confirmed(ms(0));
            let done = run(&mut login, script, ms(100), ms(9000), |now, menu| match menu.screen {
                Screen::Dialog => vec![(now + ms(124), Read::Menu(Some(title)))],
                // After the title's first Enter the whole next selection runs,
                // and is refused, with no menu readable.
                Screen::Title => {
                    titles += 1;
                    if titles > 1 { return vec![]; }
                    vec![(now + ms(50), Read::Menu(None)), (now + ms(150), Read::Lobby(5, 1, 0, 0)),
                        (now + ms(450), Read::Lobby(0, -1, 0, 201)), (now + ms(500), Read::Menu(Some(again)))]
                }
                _ => vec![],
            });
            let keys: Vec<_> = done.iter().filter(|(_, _, action)| *action == ENTER).map(|(_, screen, _)| *screen).collect();
            assert_eq!(keys, [Screen::Dialog, Screen::Title, Screen::Dialog, Screen::Title]);
            assert_eq!(login.retry.as_ref().map(|retry| retry.retries), Some(2));
        }
    }

    #[test]
    fn keyboard_hook_restores_prior_import_and_preserves_later_hooks() {
        let _one = generations(); // its last drop clears the shared queued key
        let slot = AtomicUsize::new(123);
        let shared = Arc::new(Mutex::new(None));
        let first = KeyboardHook::acquire(Arc::clone(&shared), &slot as *const _ as usize, false).unwrap();
        let second = KeyboardHook::acquire(Arc::clone(&shared), &slot as *const _ as usize, false).unwrap();
        drop(first);
        assert_eq!(slot.load(Ordering::Acquire), keyboard_state as *const () as usize,
            "finishing login must not retire the controller callback");
        drop(second);
        assert_eq!(slot.load(Ordering::Acquire), 123);
        let hook = KeyboardHook::acquire(Arc::clone(&shared), &slot as *const _ as usize, false).unwrap();
        slot.store(456, Ordering::Release);
        drop(hook);
        assert_eq!(slot.load(Ordering::Acquire), 456);
        assert!(KeyboardHook::acquire(shared, &slot as *const _ as usize, false).is_none(),
            "a later hook must never become our original on the next GameStart");
    }

    #[test]
    fn unresolved_key_input_never_prepares_a_hook() {
        let unsupported = Resolution::unsupported(0x1000_0000, "test");
        assert!(prepare(&unsupported).is_err());
        assert!(TitleProbe::new(&unsupported).is_err(), "unresolved images are not sampled");
        assert!(KeyboardHook::install(0, &keys()).is_none());
    }

    #[test]
    #[ignore = "requires CRYSTAL_AUTO_LOGIN_DLL; loads the installed DLL without login"]
    fn installed_game_passes_all_login_and_input_layout_checks() {
        use windows_sys::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH};
        let path = std::env::var("CRYSTAL_AUTO_LOGIN_DLL").unwrap();
        let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let base = unsafe { LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) } as usize;
        assert_ne!(base, 0, "{}", std::io::Error::last_os_error());
        let resolution = crate::game_layout::resolve_module(base).expect("the loaded DLL is initialized");
        let layout = Layout::resolve(&resolution).expect("login menus resolve");
        assert!(TitleProbe::new(&resolution).is_ok());
        let keys = resolution.input.clone().expect("key input resolves");
        assert!(verified_input_layout(layout.base, &keys));
        assert!(prepare(&resolution).is_ok(), "the whole auto-login preparation succeeds");
        let hook = KeyboardHook::install(base, &keys).expect("keyboard import can be redirected");
        let (slot, original) = {
            let guard = hook.shared.lock().unwrap();
            let import = guard.as_ref().unwrap();
            (import.slot, import.original)
        };
        assert_eq!(read::<usize>(slot), Some(keyboard_state as *const () as usize));
        drop(hook);
        assert_eq!(read::<usize>(slot), Some(original));
    }

    #[cfg(target_arch = "x86")]
    #[test]
    fn background_frame_receives_only_its_own_login_key_without_activation() {
        use windows_sys::Win32::{System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{RegisterClassW, CreateWindowExW, DestroyWindow, DefWindowProcW,
                WNDCLASSW, WS_EX_NOACTIVATE, WS_POPUP, WS_VISIBLE}};
        unsafe extern "thiscall" fn held(context: usize, scan: u32) {
            *((context + scan as usize) as *mut u8) |= 4;
            *((context + 256) as *mut u8) = 1;
        }
        unsafe extern "thiscall" fn edge(context: usize, scan: u32) { *((context + scan as usize) as *mut u8) |= 2; }
        unsafe extern "thiscall" fn repeat(context: usize, scan: u32) { *((context + scan as usize) as *mut u8) |= 1; }
        struct Window(HWND);
        impl Drop for Window { fn drop(&mut self) { unsafe { DestroyWindow(self.0); } } }
        let class: Vec<u16> = "FFXiClass\0".encode_utf16().collect();
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let wc = WNDCLASSW { lpfnWndProc: Some(DefWindowProcW), hInstance: instance,
            lpszClassName: class.as_ptr(), ..unsafe { std::mem::zeroed() } };
        assert_ne!(unsafe { RegisterClassW(&wc) }, 0);
        let window = Window(unsafe { CreateWindowExW(WS_EX_NOACTIVATE, class.as_ptr(), class.as_ptr(),
            WS_POPUP | WS_VISIBLE, -30000, -30000, 1, 1, std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null()) });
        assert!(!window.0.is_null());
        let before = unsafe { GetForegroundWindow() };
        assert_ne!(before, window.0);
        let mut fixture = Fixture::new();
        fixture.u32(WINDOW_RVA, window.0 as usize as u32);
        let (result, _receive) = mpsc::channel();
        let input_rva = keys().input;
        let _one = generations();
        let generation = begin_generation();
        ACTIVE.store(generation, Ordering::Release);
        let mut input = Command { layout: fixture.layout(), keys: keys(), window: window.0 as usize,
            game_thread: unsafe { GetCurrentThreadId() }, target: 1, expected: fixture.snapshot().unwrap(),
            key: Key::Enter, expires: Instant::now() + Duration::from_secs(30), result,
            generation, world: World { flag: WORLD_RVA } };
        let buffer = (fixture.base() + 0x9298) as *mut u8;
        let dispatch = |context, key| { unsafe { dispatch_key(context, key, [held, edge, repeat]); } true };
        assert_eq!(deliver(&input, buffer, dispatch), Some(Tap::Sent));
        assert_eq!(fixture.0[input_rva + 0x1C], 7);
        assert_eq!(fixture.0[input_rva + 256], 1);
        assert_eq!(unsafe { GetForegroundWindow() }, before);
        fixture.0[input_rva..input_rva + 257].fill(0);
        // Foreign buffers, threads and windows cannot receive this command.
        assert_eq!(deliver(&input, std::ptr::null_mut(), |_, _| panic!()), None);
        input.game_thread += 1;
        assert_eq!(deliver(&input, buffer, |_, _| panic!()), None);
        input.game_thread -= 1;
        input.window = 0;
        assert_eq!(deliver(&input, buffer, |_, _| panic!()), Some(Tap::Skipped));
        input.window = window.0 as usize;
        fixture.menu(b"menu    ptc6yesn", 1);
        assert_eq!(deliver(&input, buffer, |_, _| panic!()), Some(Tap::Skipped));
        fixture.menu(b"menu    dbnamese", 2);
        fixture.0[input_rva + 0x01] = 2; // Real Escape wins over automation.
        assert_eq!(deliver(&input, buffer, |_, _| panic!()), Some(Tap::Skipped));
        fixture.0[input_rva + 0x01] = 0;
        // The dialog's Enter: only while the lobby still reports the refusal.
        fixture.menu(OK_MENU, 0);
        input.expected = fixture.snapshot().unwrap();
        assert_eq!(input.expected.screen, Screen::Dialog);
        assert_eq!(deliver(&input, buffer, |_, _| panic!()), Some(Tap::Skipped), "no readable reply");
        fixture.lobby(5, 1, 0);
        assert_eq!(deliver(&input, buffer, |_, _| panic!()), Some(Tap::Skipped), "the next request is already out");
        fixture.lobby(0, -1, 201);
        assert_eq!(deliver(&input, buffer, dispatch), Some(Tap::Sent));
        fixture.0[input_rva..input_rva + 257].fill(0);
        fixture.menu(b"menu    loby2win", 1);
        assert_eq!(deliver(&input, buffer, |_, _| panic!()), Some(Tap::Skipped), "the dialog closed");
        input.expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(deliver(&input, buffer, |_, _| panic!()), Some(Tap::Cancelled));
    }

    #[test]
    fn real_reader_requires_focused_controller_login_context_and_occupied_slot() {
        let mut fixture = Fixture::new();
        let ready = fixture.snapshot().unwrap();
        assert_eq!(ready.cursor, 1);
        assert_eq!(ready.character_count, 3);
        assert_eq!(Navigator::new(2).action(ready, Duration::ZERO), Action::Tap(Key::Enter));
        fixture.u32(0x7000 + 0x2C, 0x12);
        assert!(matches!(Navigator::new(2).action(fixture.snapshot().unwrap(), Duration::ZERO), Action::Stop(_)));
        fixture.u32(0x7000 + 0x2C, 0x11);
        fixture.u32(0x5000 + 0xB0 + 0x68, 0);
        assert!(!fixture.snapshot().unwrap().target_available);
        fixture.u32(0x5000 + 0xB0 + 0x68, 1);
        fixture.u32(0x5000 + 0xB0 + 0x98, 1);
        assert!(!fixture.snapshot().unwrap().target_available);
        fixture.u32(0x33820, 1);
        assert!(!fixture.snapshot().unwrap().target_available);
        fixture.ptr(0x5008, 0x1100);
        assert_eq!(fixture.snapshot(), None, "background list is never navigated");
        fixture.ptr(0x5008, 0x1000);
        fixture.0[0x105D] = 1;
        assert_eq!(fixture.snapshot(), None, "disabled menus are never navigated");
    }

    #[test]
    fn title_timing_requires_the_live_focused_title_menu_without_changing_input() {
        let mut fixture = Fixture::new();
        for menu in [b"menu    ptc8lice", b"menu    dbnamese", b"menu    ptc6yesn", b"menu    helpwind"] {
            fixture.menu(menu, 1);
            assert!(!title_menu(&fixture.layout()));
        }
        fixture.menu(b"menu    loby2win", 1);
        let original = fixture.0.clone();
        assert!(title_menu(&fixture.layout()));
        assert_eq!(fixture.0, original, "timing never writes menus or input");
        fixture.ptr(0x4008, 0x1100);
        assert!(!title_menu(&fixture.layout()), "a stale or background title is not ready");
        fixture.ptr(0x4008, 0x1000);
        fixture.0[0x105D] = 1;
        assert!(!title_menu(&fixture.layout()), "a disabled menu is not ready");
    }

    #[test]
    fn title_observation_reports_fixed_categories_and_requires_a_usable_window() {
        let mut fixture = Fixture::new();
        let probe = TitleProbe { layout: fixture.layout() };
        for (menu, category) in [(b"menu    ptc8lice", "warning"),
            (b"menu    loby2win", "title"), (b"menu    dbnamese", "characters"),
            (b"menu    ptc6yesn", "confirmation"), (b"menu    helpwind", "other"), (OK_MENU, "other")] {
            fixture.menu(menu, 1);
            assert_eq!(probe.observe().menu, category);
        }
        fixture.menu(b"menu    loby2win", 1);
        let before = fixture.0.clone();
        let observed = probe.observe();
        assert!(observed.menu_live);
        assert_eq!(observed.window, "pending");
        assert!(!observed.ready());
        for window in ["pending", "hidden", "minimized", "disabled"] {
            assert!(!TitleObservation { window, ..observed }.ready());
        }
        assert!(TitleObservation { window: "visible", ..observed }.ready());
        assert!(!TitleObservation { menu_live: false, window: "visible", ..observed }.ready());
        assert_eq!(fixture.0, before, "observing a title never writes to the game");
        fixture.u32(FOCUS_RVA, 0);
        assert_eq!(probe.observe().menu, "pending");
        assert!(!probe.observe().ready());
    }

    #[test]
    fn real_reader_identifies_only_the_ordinary_login_confirmation() {
        let mut fixture = Fixture::new();
        fixture.menu(b"menu    ptc6yesn", 1);
        fixture.u32(0x8000 + 0x14, 1);
        let confirm = fixture.snapshot().unwrap();
        assert_eq!(confirm.screen, Screen::Confirm);
        assert_eq!(confirm.cursor, 1);
        assert!(confirm.login_confirmation && confirm.target_available);
        fixture.u32(0x8000 + 0x18, 14);
        assert!(!fixture.snapshot().unwrap().login_confirmation);
        fixture.u32(0x8000 + 0x18, 13);
        fixture.0[0x8000 + 0xA5E0] = 1;
        assert!(!fixture.snapshot().unwrap().login_confirmation);
        fixture.u32(FOCUS_RVA, u32::MAX);
        assert_eq!(fixture.snapshot(), None, "invalid pointers fail closed without overflow");
    }

    #[test]
    fn real_reader_tells_the_ok_dialog_from_every_other_menu() {
        let mut fixture = Fixture::new();
        fixture.menu(OK_MENU, 0);
        assert_eq!(fixture.snapshot(), Some(bare(Screen::Dialog, fixture.base() + 0x1000)));
        fixture.0[0x105D] = 1;
        assert_eq!(fixture.snapshot(), None, "a disabled dialog is never answered");
        fixture.0[0x105D] = 0;
        // Only the exact resource name, as read live: "menu", four spaces, "ok", six spaces.
        for name in [b"menu    ok     \0", b"menu    ok2     ", b"menu     ok     ", b"menu    okcancel", b"MENU    OK      "] {
            fixture.menu(name, 0);
            assert_eq!(fixture.snapshot().map(|state| state.screen), Some(Screen::Other));
        }
        fixture.u32(FOCUS_RVA, 0);
        assert_eq!(fixture.snapshot(), None, "no focused menu is not a screen");
    }
}
