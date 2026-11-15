//! The in-world disconnect dialog. When the connection drops in the world,
//! the client's fatal-error handler requests network state 0x15 (FFXI-2001..
//! 2100, including a lost POL connection) or 0x0C (FFXI-3001/4001..4006) on
//! its network manager. Main then focuses the "ok" menu (10014ad0) until OK is
//! confirmed, and then ends the game (states 0x0B/0x0D).
//!
//! This module logs that path once per change, tells the launcher the game is
//! ending on a lost connection, and, only when the launcher's Recovery
//! relaunches by itself, presses OK once: after the dialog has stayed focused
//! for a moment, never after the user's own input, never on another menu.

use std::{sync::{mpsc, Mutex}, thread, time::{Duration, Instant}};
use windows_sys::Win32::{System::Threading::GetCurrentThreadId, UI::WindowsAndMessaging::GetForegroundWindow};
use crate::{auto_login::{self, Key, Layout, Tap}, game_layout::{Input, Resolution}, lobby, log, mem::read};

const POLL: Duration = Duration::from_millis(200);
/// The dialog must stay focused and unchanged this long before OK.
const SETTLE: Duration = Duration::from_secs(1);
/// After OK the dialog must close within this.
const DISMISS: Duration = Duration::from_secs(5);
/// A game still running this long after the dialog closed is logged once.
const EXIT_NOTE: Duration = Duration::from_secs(30);

/// One sample of the state this reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct View {
    /// The client's "commands enabled" world flag.
    world: Option<bool>,
    /// The network manager's requested state and its last POL result.
    network: Option<(u32, i32)>,
    /// The focused, enabled "ok" menu.
    dialog: Option<usize>,
}

impl View {
    /// The client asked to stop its network on a fatal error.
    fn stopped(self) -> bool {
        matches!(self.network, Some((lobby::MAIN_STOP_ERROR | lobby::MAIN_STOP_LOST, _)))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Act { Wait, Press(usize), Closed, Stop(&'static str) }

/// State in, action out. `enabled` is Recovery's automatic relaunch.
struct Dismiss {
    enabled: bool,
    in_world: bool,
    shown: Option<(usize, Duration)>,
    pressed: Option<Duration>,
    done: bool,
}

impl Dismiss {
    fn new(enabled: bool) -> Self { Self { enabled, in_world: false, shown: None, pressed: None, done: false } }

    /// A disconnect of a game that reached the world, with or without its dialog yet.
    fn disconnected(&self, view: View) -> bool { self.in_world && view.stopped() }

    /// `user_input`: a key or mouse button is held in the foreground game window.
    fn observe(&mut self, view: View, user_input: bool, now: Duration) -> Act {
        if view.world == Some(true) { self.in_world = true; }
        if self.done { return Act::Wait; }
        if let Some(pressed) = self.pressed {
            if view.dialog.is_none() { self.done = true; return Act::Closed; }
            if now.saturating_sub(pressed) > DISMISS { self.done = true; return Act::Stop("the dialog did not close after OK"); }
            return Act::Wait;
        }
        let Some(focus) = view.dialog.filter(|_| self.enabled && self.disconnected(view)) else {
            self.shown = None;
            return Act::Wait;
        };
        if user_input { self.done = true; return Act::Stop("user input on the dialog; it is left to the user"); }
        match self.shown {
            Some((shown, since)) if shown == focus => {
                if now.saturating_sub(since) >= SETTLE { Act::Press(focus) } else { Act::Wait }
            }
            _ => { self.shown = Some((focus, now)); Act::Wait }
        }
    }

    fn pressed(&mut self, now: Duration) { self.pressed = Some(now); }
}

/// What this module reads, resolved and verified for this game.
#[derive(Clone, Copy)]
struct Sources { layout: Layout, main: usize, world: usize }

impl Sources {
    fn resolve(resolution: &Resolution) -> Result<Self, String> {
        let layout = Layout::resolve(resolution)?;
        let main = resolution.diagnostics.as_ref().map_err(|reason| format!("network state: {reason}"))?.main;
        let world = resolution.shutdown.as_ref().map_err(|reason| format!("world state: {reason}"))?.enabled;
        Ok(Self { layout, main, world })
    }

    fn view(&self) -> View {
        let base = self.layout.base;
        View {
            world: read::<u32>(base + self.world).map(|flag| flag != 0),
            network: lobby::network(base, self.main),
            dialog: self.dialog(),
        }
    }

    /// The focused "ok" menu, enabled, read without a switch in between.
    fn dialog(&self) -> Option<usize> {
        let (focus, resource, name) = auto_login::focused_menu(&self.layout)?;
        (&name == auto_login::OK_MENU && read::<u8>(focus + auto_login::MENU_DISABLED)? == 0
            && auto_login::menu_unchanged(&self.layout, focus, resource, &name)).then_some(focus)
    }
}

/// A queued OK, consumed by the game thread's next keyboard frame.
struct Command { sources: Sources, keys: Input, window: usize, game_thread: u32, focus: usize, expires: Instant, result: mpsc::Sender<Tap> }
static COMMAND: Mutex<Option<Command>> = Mutex::new(None);

fn command() -> std::sync::MutexGuard<'static, Option<Command>> {
    COMMAND.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// From the shared keyboard hook (auto_login), on every keyboard frame.
pub(crate) fn on_input(data: *mut u8) {
    let Ok(mut pending) = COMMAND.try_lock() else { return };
    let Some(input) = pending.as_ref() else { return };
    let base = input.sources.layout.base;
    if unsafe { GetCurrentThreadId() } != input.game_thread
        || auto_login::frame_keyboard(base, input.keys.keyboard, data).is_none() {
        return;
    }
    let window = input.window as windows_sys::Win32::Foundation::HWND;
    let view = input.sources.view();
    let outcome = if input.expires < Instant::now() || crate::events::pipe_closed() {
        Tap::Cancelled
    } else if auto_login::game_window(&input.sources.layout) != Some((window, input.game_thread))
        || !auto_login::usable(window, input.game_thread)
        || !view.stopped() || view.dialog != Some(input.focus)
        || (unsafe { GetForegroundWindow() } == window && auto_login::keys_down())
        || read::<[u8; 256]>(base + input.keys.input).is_none_or(|keys| keys.iter().any(|key| key & 7 != 0)) {
        Tap::Skipped
    } else if auto_login::send(base, &input.keys, Key::Enter) { Tap::Sent } else { Tap::Failed };
    if let Some(input) = pending.take() { let _ = input.result.send(outcome); }
}

fn press(sources: Sources, keys: Input, focus: usize, stop: &mpsc::Receiver<()>) -> Tap {
    let Some((window, game_thread)) = auto_login::game_window(&sources.layout) else { return Tap::Skipped };
    let (result, receive) = mpsc::channel();
    let expires = Instant::now() + Duration::from_secs(2);
    *command() = Some(Command { sources, keys, window: window as usize, game_thread, focus, expires, result });
    loop {
        if let Ok(outcome) = receive.try_recv() { return outcome; }
        if stop.recv_timeout(Duration::from_millis(10)) != Err(mpsc::RecvTimeoutError::Timeout) {
            command().take();
            return Tap::Cancelled;
        }
        if Instant::now() >= expires { command().take(); return Tap::Failed; }
    }
}

/// The logged state: world flag, network state, POL result while negative,
/// and the focused menu while it matters (the network stopped, or "ok").
#[derive(Clone, PartialEq, Eq)]
struct Trace { world: Option<bool>, network: Option<u32>, pol: Option<i32>, menu: Option<String> }

fn trace(view: View, sources: &Sources) -> Trace {
    let menu = (view.stopped() || view.dialog.is_some()).then(|| auto_login::menu_name(&sources.layout));
    Trace { world: view.world, network: view.network.map(|(state, _)| state), pol: view.network.map(|(_, pol)| pol).filter(|pol| *pol < 0), menu }
}

pub(crate) struct Worker { stop: mpsc::Sender<()>, thread: Option<thread::JoinHandle<()>> }

impl Worker {
    /// `dismiss`: Recovery relaunches automatically, so OK may be pressed.
    pub(crate) fn start(dismiss: bool) -> Option<Self> {
        let (stop, stopping) = mpsc::channel();
        let thread = thread::Builder::new().name("crystal-disconnect".into()).spawn(move || run(dismiss, &stopping)).ok()?;
        Some(Self { stop, thread: Some(thread) })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(input) = command().take() { let _ = input.result.send(Tap::Cancelled); }
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

fn run(dismiss: bool, stopping: &mpsc::Receiver<()>) {
    let started = Instant::now();
    let mut prepared = None;
    let mut last = None;
    let mut reported = false;
    let mut closed = None;
    let mut decision = Dismiss::new(false);
    while stopping.recv_timeout(POLL) == Err(mpsc::RecvTimeoutError::Timeout) {
        if prepared.is_none() {
            let Some(resolution) = crate::game_layout::ffximain() else { continue };
            let sources = match Sources::resolve(resolution) {
                Ok(sources) => sources,
                Err(reason) => { log!("disconnect: unavailable for this game ({reason})"); return; }
            };
            // Diagnostics need no input; OK needs the verified key input and its hook.
            let input = if dismiss {
                match resolution.input.clone().map_err(|reason| format!("key input: {reason}"))
                    .and_then(|keys| auto_login::input_hook(resolution).map(|hook| (keys, hook))) {
                    Ok(input) => Some(input),
                    Err(reason) => { log!("disconnect: automatic OK unavailable ({reason})"); None }
                }
            } else { None };
            decision = Dismiss::new(input.is_some());
            log!("disconnect: watching this game (automatic OK {})", if input.is_some() { "on: Recovery relaunches automatically" } else { "off" });
            prepared = Some((sources, input));
        }
        let (sources, input) = prepared.as_ref().unwrap();
        let view = sources.view();
        let current = trace(view, sources);
        if last.as_ref() != Some(&current) {
            log!("disconnect: world={:?} network_state={:?} pol_result={:?} menu={}", current.world, current.network,
                current.pol, current.menu.as_deref().unwrap_or("-"));
            last = Some(current);
        }
        let user = auto_login::game_window(&sources.layout)
            .is_some_and(|(window, _)| unsafe { GetForegroundWindow() } == window && auto_login::keys_down());
        let now = started.elapsed();
        let act = decision.observe(view, user, now);
        if !reported && decision.disconnected(view) {
            reported = true;
            let (state, pol) = view.network.unwrap_or_default();
            log!("disconnect: FFXI lost its connection in the world (network state {state:#x}, POL result {pol}); its dialog follows");
            crate::events::send(crate::proto::game_disconnected_line(state, pol));
        }
        match act {
            Act::Wait => {}
            Act::Press(focus) => {
                let Some((keys, _)) = input else { continue };
                match press(*sources, *keys, focus, stopping) {
                    Tap::Sent => { decision.pressed(now); log!("disconnect: pressed OK on menu={}", auto_login::menu_name(&sources.layout)); }
                    Tap::Skipped => {}
                    Tap::Cancelled => return,
                    Tap::Failed => { log!("disconnect: the game did not take the OK key; the dialog is left to the user"); return; }
                }
            }
            Act::Closed => { closed = Some(now); log!("disconnect: dialog closed; waiting for FFXI to end"); }
            Act::Stop(reason) => log!("disconnect: no automatic OK: {reason}"),
        }
        if closed.is_some_and(|at| now.saturating_sub(at) >= EXIT_NOTE) {
            log!("disconnect: FFXI is still running {}s after its dialog closed", EXIT_NOTE.as_secs());
            closed = None;
        }
    }
    if let Some(pressed) = decision.pressed {
        log!("disconnect: GameStart returned {:.1}s after OK", started.elapsed().saturating_sub(pressed).as_secs_f32());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);
    fn view(world: bool, state: u32, dialog: Option<usize>) -> View {
        View { world: Some(world), network: Some((state, if state == lobby::MAIN_STOP_LOST { -2059 } else { 0 })), dialog }
    }
    fn lost(dialog: Option<usize>) -> View { view(false, lobby::MAIN_STOP_LOST, dialog) }

    #[test]
    fn presses_ok_once_after_the_dialog_settles_in_a_game_that_reached_the_world() {
        let mut d = Dismiss::new(true);
        assert_eq!(d.observe(view(true, 6, None), false, Duration::ZERO), Act::Wait);
        // The client's two fatal stop states, each with its focused "ok" menu.
        for state in [lobby::MAIN_STOP_LOST, lobby::MAIN_STOP_ERROR] {
            let mut d = Dismiss::new(true);
            d.observe(view(true, 6, None), false, Duration::ZERO);
            let dialog = view(false, state, Some(0x1000));
            assert!(d.disconnected(dialog));
            assert_eq!(d.observe(dialog, false, S), Act::Wait, "first sight starts the settle time");
            assert_eq!(d.observe(dialog, false, S + SETTLE / 2), Act::Wait);
            assert_eq!(d.observe(dialog, false, S + SETTLE), Act::Press(0x1000));
            d.pressed(S + SETTLE);
            assert_eq!(d.observe(dialog, false, S + SETTLE + S), Act::Wait, "never a second OK");
            assert_eq!(d.observe(lost(None), false, S * 3), Act::Closed);
            assert_eq!(d.observe(dialog, false, S * 4), Act::Wait, "finished for this game");
        }
        let _ = d;
    }

    #[test]
    fn off_unless_recovery_relaunches_automatically() {
        let mut d = Dismiss::new(false);
        d.observe(view(true, 6, None), false, Duration::ZERO);
        for t in 1..20 { assert_eq!(d.observe(lost(Some(0x1000)), false, S * t), Act::Wait); }
        assert!(d.disconnected(lost(None)), "the disconnect is still identified and reported");
    }

    #[test]
    fn never_acts_on_other_dialogs_states_or_before_the_world() {
        // An "ok" menu without a fatal stop (lobby errors, other in-game notices).
        let mut d = Dismiss::new(true);
        d.observe(view(true, 6, None), false, Duration::ZERO);
        for t in 1..20 { assert_eq!(d.observe(view(true, 6, Some(0x1000)), false, S * t), Act::Wait); }
        // A fatal stop before the character ever reached the world (login screens).
        let mut d = Dismiss::new(true);
        for t in 1..20 { assert_eq!(d.observe(lost(Some(0x1000)), false, S * t), Act::Wait); }
        assert!(!d.disconnected(lost(Some(0x1000))));
        // Unreadable state never counts.
        let mut d = Dismiss::new(true);
        d.observe(view(true, 6, None), false, Duration::ZERO);
        let unread = View { world: None, network: None, dialog: Some(0x1000) };
        for t in 1..20 { assert_eq!(d.observe(unread, false, S * t), Act::Wait); }
        // The stop without its dialog focused yet.
        for t in 20..40 { assert_eq!(d.observe(lost(None), false, S * t), Act::Wait); }
    }

    #[test]
    fn a_changed_dialog_restarts_the_settle_time() {
        let mut d = Dismiss::new(true);
        d.observe(view(true, 6, None), false, Duration::ZERO);
        d.observe(lost(Some(0x1000)), false, S);
        assert_eq!(d.observe(lost(Some(0x2000)), false, S + SETTLE), Act::Wait);
        assert_eq!(d.observe(lost(None), false, S * 3), Act::Wait);
        d.observe(lost(Some(0x2000)), false, S * 4);
        assert_eq!(d.observe(lost(Some(0x2000)), false, S * 4 + SETTLE), Act::Press(0x2000));
    }

    #[test]
    fn user_input_first_leaves_the_dialog_to_the_user() {
        let mut d = Dismiss::new(true);
        d.observe(view(true, 6, None), false, Duration::ZERO);
        d.observe(lost(Some(0x1000)), false, S);
        assert!(matches!(d.observe(lost(Some(0x1000)), true, S + SETTLE / 2), Act::Stop(_)));
        assert_eq!(d.observe(lost(Some(0x1000)), false, S * 5), Act::Wait, "not re-armed for this game");
    }

    #[test]
    fn a_dialog_that_stays_after_ok_is_reported_once() {
        let mut d = Dismiss::new(true);
        d.observe(view(true, 6, None), false, Duration::ZERO);
        d.observe(lost(Some(0x1000)), false, S);
        assert_eq!(d.observe(lost(Some(0x1000)), false, S + SETTLE), Act::Press(0x1000));
        d.pressed(S + SETTLE);
        assert!(matches!(d.observe(lost(Some(0x1000)), false, S + SETTLE + DISMISS + S), Act::Stop(_)));
        assert_eq!(d.observe(lost(Some(0x1000)), false, S * 20), Act::Wait);
    }

    #[test]
    fn the_trace_shows_menus_only_on_the_disconnect_path() {
        assert!(!view(true, 6, None).stopped());
        assert!(lost(None).stopped() && view(false, lobby::MAIN_STOP_ERROR, None).stopped());
        assert!(!View { world: None, network: None, dialog: None }.stopped());
    }
}
