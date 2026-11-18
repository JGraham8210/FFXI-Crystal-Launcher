//! Background games don't read the pad. FFXiMain gets XInput through
//! GetProcAddress (the game folder's xinputdll.dll: cdecl `XInputGetState_`,
//! `XInputSetState_`, `XInputEnable_`, forwarding to xinput1_3). Its
//! GetProcAddress import is redirected so those lookups (and the standard
//! XInput names and ordinal 100, `XInputGetStateEx`) return the wrappers
//! here. Unless the game's "keep working in the background" flag is on or this
//! process owns the foreground window, pad state reads as connected and idle,
//! and vibration is not sent. Everything else is forwarded unchanged.
//!
//! A read that is passed on has the buttons Crystal's pad hotkeys take hidden
//! (`pad_hide`): each binding's trigger is cleared while all its modifiers are
//! held. The same in every game, in front or kept working in the background.

use crate::pad_hide::{Entry, GUIDE, LT, MAX_ENTRIES, RT};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use windows_sys::Win32::{
    Foundation::{GetLastError, SetLastError, HMODULE},
    System::Threading::GetCurrentProcessId,
    UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
};

const ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;

/// The redirected functions; each index is a slot in [`REAL`].
const GET_C: usize = 0;
const SET_C: usize = 1;
const GET: usize = 2;
const SET: usize = 3;
const GET_EX: usize = 4;

static GET_PROC_ADDRESS: AtomicUsize = AtomicUsize::new(0);
/// The real function behind each wrapper, from the game's latest lookup.
static REAL: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];
/// This mapping's `controller_settings::background_flag`; 0 until found.
static BACKGROUND_FLAG: AtomicUsize = AtomicUsize::new(0);

/// Once per FFXiMain mapping, before the game initializes its input.
pub(crate) fn install(base: usize) {
    BACKGROUND_FLAG.store(0, Ordering::Relaxed);
    // SAFETY: `base` is the loaded FFXiMain image (ffxi_settings).
    let slot = unsafe { crate::iat::import_slots(base, "KERNEL32.dll") }.into_iter().find(|s| s.name.as_deref() == Some("GetProcAddress"));
    match slot {
        Some(slot) if crate::boot_timing::replace(slot.slot as usize, &GET_PROC_ADDRESS, get_proc_address as *const () as usize) => {
            crate::log!("background pad: XInput lookups filtered");
        }
        Some(_) => crate::log!("note: background pad: GetProcAddress import is redirected by another module; left as is"),
        None => crate::log!("note: background pad: FFXiMain imports no GetProcAddress; XInput is not filtered"),
    }
}

/// Which wrapper answers a lookup of `name` (`Err` an ordinal) in the module
/// named `module`; None for everything but XInput's state functions.
fn wrapped(module: &str, name: Result<&[u8], u16>) -> Option<usize> {
    if !module.get(..6).is_some_and(|m| m.eq_ignore_ascii_case("xinput")) { return None; }
    match name {
        Ok(b"XInputGetState_") => Some(GET_C),
        Ok(b"XInputSetState_") => Some(SET_C),
        Ok(b"XInputGetState") => Some(GET),
        Ok(b"XInputSetState") => Some(SET),
        Err(100) => Some(GET_EX),
        _ => None,
    }
}

fn wrapper(kind: usize) -> usize {
    [get_state_c as *const (), set_state_c as *const (), get_state as *const (), set_state as *const (), get_state_ex as *const ()][kind] as usize
}

unsafe extern "system" fn get_proc_address(module: HMODULE, name: *const u8) -> usize {
    let real: unsafe extern "system" fn(HMODULE, *const u8) -> usize = unsafe { std::mem::transmute(GET_PROC_ADDRESS.load(Ordering::Acquire)) };
    let found = unsafe { real(module, name) };
    if found == 0 { return 0; }
    let lookup = if (name as usize) >> 16 == 0 { Err(name as usize as u16) } else { Ok(unsafe { std::ffi::CStr::from_ptr(name.cast()) }.to_bytes()) };
    // Cheap name test first; the module path only for XInput's names.
    if wrapped("xinput", lookup).is_none() { return found; }
    let saved = unsafe { GetLastError() };
    let file = crate::win::module_path(module).and_then(|p| Some(p.file_name()?.to_string_lossy().into_owned()));
    let kind = file.and_then(|file| wrapped(&file, lookup));
    unsafe { SetLastError(saved) };
    match kind {
        Some(kind) => { REAL[kind].store(found, Ordering::Release); wrapper(kind) }
        None => found,
    }
}

/// Whether pad input is withheld from this game now. Foreground first: the
/// common case costs one window query and never reads the settings.
fn withheld(in_front: impl FnOnce() -> bool, background: impl FnOnce() -> Option<bool>) -> bool {
    !in_front() && background() != Some(true)
}

fn this_process_in_front() -> bool {
    // SAFETY: plain window queries; a null window has no process.
    unsafe {
        let window = GetForegroundWindow();
        let mut pid = 0;
        !window.is_null() && GetWindowThreadProcessId(window, &mut pid) != 0 && pid == GetCurrentProcessId()
    }
}

fn withheld_now() -> bool {
    withheld(this_process_in_front, || background(background_flag(), crate::ffxi_settings::controller_background))
}

/// Where this mapping keeps the game's own flag (0: not found), looked up once.
fn background_flag() -> usize {
    match BACKGROUND_FLAG.load(Ordering::Relaxed) {
        0 => {
            let flag = crate::controller_settings::background_flag().unwrap_or(0);
            BACKGROUND_FLAG.store(flag, Ordering::Relaxed);
            flag
        }
        flag => flag,
    }
}

/// The game's own flag, the byte its pad frame tests: a launch that serves no
/// padmode000 (Vanilla, Ashita) follows the registry's or the tool's. The
/// served one when this build's controller code is not recognized.
fn background(flag: usize, served: impl FnOnce() -> Option<bool>) -> Option<bool> {
    crate::mem::read::<u8>(flag).map(|on| on != 0).or_else(served)
}

/// The real result, so connection changes stay honest; a withheld read is the
/// whole 16-byte XINPUT_STATE zeroed (packet number too, so a game comparing
/// packet numbers still sees held buttons released). Any other read goes
/// through `hide`.
fn get(real: impl FnOnce(*mut u8) -> u32, state: *mut u8, withheld: impl FnOnce() -> bool, hide: impl FnOnce(&mut [u8; 16])) -> u32 {
    let result = real(state);
    if result == 0 && !state.is_null() {
        if withheld() {
            // SAFETY: the caller's XINPUT_STATE, just filled by the real function.
            unsafe { state.write_bytes(0, 16) };
        } else {
            // SAFETY: as above; XINPUT_STATE is 16 bytes, byte-aligned here.
            hide(unsafe { &mut *state.cast::<[u8; 16]>() });
        }
    }
    result
}

/// The pad hotkeys' bindings: modifiers in the low half, the trigger in the high
/// half; the first `HIDE_COUNT` are live.
static HIDE: [AtomicU64; MAX_ENTRIES] = [const { AtomicU64::new(0) }; MAX_ENTRIES];
static HIDE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Replace the buttons Crystal's pad hotkeys take (session line, then each
/// `pad_hide` command). Reads during the swap hide nothing.
pub(crate) fn set_hide(entries: &[Entry]) {
    HIDE_COUNT.store(0, Ordering::Release);
    for (slot, [mods, trigger]) in HIDE.iter().zip(entries) {
        slot.store(u64::from(*mods) | u64::from(*trigger) << 32, Ordering::Relaxed);
    }
    HIDE_COUNT.store(entries.len().min(MAX_ENTRIES), Ordering::Release);
}

fn hide_entries() -> impl Iterator<Item = (u32, u32)> {
    HIDE[..HIDE_COUNT.load(Ordering::Acquire)].iter().map(|slot| {
        let packed = slot.load(Ordering::Relaxed);
        (packed as u32, (packed >> 32) as u32)
    })
}

/// Clear each binding's trigger whose modifiers are all held, in a filled
/// XINPUT_STATE (`wButtons` at 4, the triggers at 6 and 7; a trigger counts as
/// held at 128, and a hidden one reads 0). Modifiers are never touched. Guide
/// only with `ex` (`XInputGetStateEx`): no other read reports it.
fn hide(state: &mut [u8; 16], entries: impl Iterator<Item = (u32, u32)>, ex: bool) {
    let buttons = u16::from_le_bytes([state[4], state[5]]);
    let held = u32::from(buttons) | if state[6] >= 128 { LT } else { 0 } | if state[7] >= 128 { RT } else { 0 };
    let clear = entries.filter(|(mods, trigger)| held & mods == *mods && (ex || *trigger != GUIDE)).fold(0, |all, (_, trigger)| all | trigger);
    if clear == 0 { return; }
    state[4..6].copy_from_slice(&(buttons & !(clear as u16)).to_le_bytes());
    if clear & LT != 0 { state[6] = 0; }
    if clear & RT != 0 { state[7] = 0; }
}

fn hide_now(state: &mut [u8; 16], ex: bool) {
    if HIDE_COUNT.load(Ordering::Acquire) != 0 { hide(state, hide_entries(), ex); }
}

/// Whether vibration this game sent may still be running.
static RUMBLING: AtomicBool = AtomicBool::new(false);

/// A withheld request is not sent, as XInput itself does while disabled: the
/// game repeats its request (zero included) every few frames, and the pad's
/// motors are shared, so motors-off from each background game would cut the
/// focused game's vibration. Only vibration this game left running is
/// switched off, once (XINPUT_VIBRATION is two u16 speeds).
fn set(real: impl FnOnce(*mut u16) -> u32, vibration: *mut u16, withheld: impl FnOnce() -> bool, rumbling: &AtomicBool) -> u32 {
    if withheld() {
        return if rumbling.swap(false, Ordering::Relaxed) { real([0u16; 2].as_mut_ptr()) } else { 0 };
    }
    // SAFETY: the caller's XINPUT_VIBRATION, when it passed one.
    rumbling.store(!vibration.is_null() && unsafe { [*vibration, *vibration.add(1)] } != [0, 0], Ordering::Relaxed);
    real(vibration)
}

fn real_fn(kind: usize) -> usize { REAL[kind].load(Ordering::Acquire) }

macro_rules! wrappers {
    ($abi:literal, $get:ident, $get_kind:expr, $set:ident, $set_kind:expr) => {
        unsafe extern $abi fn $get(user: u32, state: *mut u8) -> u32 {
            let f = real_fn($get_kind);
            if f == 0 { return ERROR_DEVICE_NOT_CONNECTED; }
            let f: unsafe extern $abi fn(u32, *mut u8) -> u32 = unsafe { std::mem::transmute(f) };
            get(|s| unsafe { f(user, s) }, state, withheld_now, |s| hide_now(s, false))
        }
        unsafe extern $abi fn $set(user: u32, vibration: *mut u16) -> u32 {
            let f = real_fn($set_kind);
            if f == 0 { return ERROR_DEVICE_NOT_CONNECTED; }
            let f: unsafe extern $abi fn(u32, *mut u16) -> u32 = unsafe { std::mem::transmute(f) };
            set(|v| unsafe { f(user, v) }, vibration, withheld_now, &RUMBLING)
        }
    };
}
// xinputdll.dll's exports are cdecl (`ret`, caller pops); XInput's own are stdcall.
wrappers!("C", get_state_c, GET_C, set_state_c, SET_C);
wrappers!("system", get_state, GET, set_state, SET);

unsafe extern "system" fn get_state_ex(user: u32, state: *mut u8) -> u32 {
    let f = real_fn(GET_EX);
    if f == 0 { return ERROR_DEVICE_NOT_CONNECTED; }
    let f: unsafe extern "system" fn(u32, *mut u8) -> u32 = unsafe { std::mem::transmute(f) };
    get(|s| unsafe { f(user, s) }, state, withheld_now, |s| hide_now(s, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};

    #[test]
    fn input_is_withheld_only_in_the_background_without_the_flag() {
        assert!(!withheld(|| true, || panic!("the flag is not read in front")));
        assert!(withheld(|| false, || Some(false)));
        assert!(withheld(|| false, || None), "unknown flag: the game's default, off");
        assert!(!withheld(|| false, || Some(true)));
    }

    #[test]
    fn the_games_own_flag_decides_before_the_served_one() {
        let (on, off) = (1u8, 0u8);
        let at = |flag: &u8| flag as *const u8 as usize;
        // Vanilla and Ashita launches serve no padmode000.
        assert_eq!(background(at(&on), || None), Some(true));
        assert!(!withheld(|| false, || background(at(&on), || None)));
        assert_eq!(background(at(&off), || Some(true)), Some(false));
        // A build whose controller code is not recognized.
        assert_eq!(background(0, || Some(true)), Some(true));
        assert_eq!(background(0, || None), None);
    }

    #[test]
    fn background_reads_are_connected_and_idle() {
        let fill = |s: *mut u8| { unsafe { s.write_bytes(0xAB, 16) }; 0 };
        let mut state = [0xCDu8; 20];
        assert_eq!(get(fill, state.as_mut_ptr(), || true, |_| panic!("a withheld read hides nothing more")), 0);
        assert_eq!(state[..16], [0; 16]);
        assert_eq!(state[16..], [0xCD; 4], "only XINPUT_STATE is written");
        assert_eq!(get(fill, state.as_mut_ptr(), || false, |_| {}), 0);
        assert_eq!(state[..16], [0xAB; 16]);
        // A missing pad stays missing.
        let mut state = [0xABu8; 16];
        assert_eq!(get(|_| ERROR_DEVICE_NOT_CONNECTED, state.as_mut_ptr(), || panic!("not asked"), |_| panic!("not asked")), ERROR_DEVICE_NOT_CONNECTED);
        assert_eq!(state, [0xAB; 16]);
    }

    /// An XINPUT_STATE with these buttons and trigger bytes (packet 7, sticks set).
    fn pad(buttons: u16, lt: u8, rt: u8) -> [u8; 16] {
        let mut s = [0x11u8; 16];
        s[..4].copy_from_slice(&7u32.to_le_bytes());
        s[4..6].copy_from_slice(&buttons.to_le_bytes());
        (s[6], s[7]) = (lt, rt);
        s
    }

    const BACK: u32 = 0x20;
    const START: u32 = 0x10;
    const A: u32 = 0x1000;

    fn hidden(state: [u8; 16], entries: &[(u32, u32)], ex: bool) -> [u8; 16] {
        let mut out = state;
        hide(&mut out, entries.iter().copied(), ex);
        out
    }

    #[test]
    fn a_single_button_is_always_hidden_and_a_combo_only_while_its_modifiers_are_held() {
        // Back alone (FFXI's Hide windows by default) and LT+Start.
        let entries = [(0, BACK), (LT, START)];
        assert_eq!(hidden(pad(0x20 | 0x1000, 0, 0), &entries, false), pad(0x1000, 0, 0), "Back gone, A kept");
        // Start without LT reaches the game; with LT held (128 and up) it does not, and LT does.
        assert_eq!(hidden(pad(0x10, 0, 0), &entries, false), pad(0x10, 0, 0));
        assert_eq!(hidden(pad(0x10, 127, 0), &entries, false), pad(0x10, 127, 0), "below the threshold LT is not held");
        assert_eq!(hidden(pad(0x10, 128, 0), &entries, false), pad(0, 128, 0));
        assert_eq!(hidden(pad(0x10, 255, 9), &entries, false), pad(0, 255, 9));
        // Nothing bound: untouched, packet number and sticks too.
        assert_eq!(hidden(pad(0xFFFF, 255, 255), &[], true), pad(0xFFFF, 255, 255));
    }

    #[test]
    fn a_trigger_as_the_bound_button_reads_released() {
        // RT alone; A+LT.
        let entries = [(0, RT), (A, LT)];
        assert_eq!(hidden(pad(0, 200, 200), &entries, false), pad(0, 200, 0));
        assert_eq!(hidden(pad(0x1000, 200, 200), &entries, false), pad(0x1000, 0, 0));
        // RT as a modifier stays; its combo's trigger goes.
        assert_eq!(hidden(pad(0x20, 0, 255), &[(RT, BACK)], false), pad(0, 0, 255));
    }

    #[test]
    fn guide_is_hidden_only_from_the_read_that_reports_it() {
        let entries = [(0, GUIDE), (BACK, GUIDE)];
        assert_eq!(hidden(pad(0x0400 | 0x1000, 0, 0), &entries, true), pad(0x1000, 0, 0));
        assert_eq!(hidden(pad(0x0400, 0, 0), &entries, false), pad(0x0400, 0, 0), "XInputGetState never reports Guide");
    }

    #[test]
    fn background_zeroing_comes_first_and_hiding_follows_in_front() {
        let fill = |s: *mut u8| { unsafe { s.copy_from(pad(0x20 | 0x10, 0, 0).as_ptr(), 16) }; 0 };
        let entries = [(0, BACK)];
        let mut state = [0u8; 16];
        // In the background the whole read is zeroed, as before.
        assert_eq!(get(fill, state.as_mut_ptr(), || true, |s| hide(s, entries.iter().copied(), false)), 0);
        assert_eq!(state, [0; 16]);
        // In front: Back hidden, Start kept.
        assert_eq!(get(fill, state.as_mut_ptr(), || false, |s| hide(s, entries.iter().copied(), false)), 0);
        assert_eq!(state, pad(0x10, 0, 0));
    }

    /// The live table: the session's list, replaced whole by each command.
    #[test]
    fn the_hide_list_is_replaced_whole() {
        set_hide(&[[0, BACK], [LT, START]]);
        assert_eq!(hide_entries().collect::<Vec<_>>(), [(0, BACK), (LT, START)]);
        let mut state = pad(0x30, 0, 0);
        hide_now(&mut state, false);
        assert_eq!(state, pad(0x10, 0, 0));
        set_hide(&[[0, GUIDE]]);
        assert_eq!(hide_entries().collect::<Vec<_>>(), [(0, GUIDE)]);
        set_hide(&[]);
        let mut state = pad(0x30, 0, 0);
        hide_now(&mut state, true);
        assert_eq!(state, pad(0x30, 0, 0), "an empty list hides nothing");
    }

    #[test]
    fn background_vibration_is_not_sent_and_only_this_games_own_is_stopped() {
        let rumbling = AtomicBool::new(false);
        let sent = std::cell::RefCell::new(Vec::new());
        let record = |v: *mut u16| { sent.borrow_mut().push(unsafe { [*v, *v.add(1)] }); 7 };
        let (mut on, mut off) = ([1000u16, 2000], [0u16; 2]);
        // In the background the pad is another game's: nothing is sent.
        assert_eq!(set(record, on.as_mut_ptr(), || true, &rumbling), 0);
        assert_eq!(set(record, off.as_mut_ptr(), || true, &rumbling), 0);
        assert!(sent.borrow().is_empty());
        // In front it is sent; leaving the front stops it once.
        assert_eq!(set(record, on.as_mut_ptr(), || false, &rumbling), 7);
        assert_eq!(set(record, on.as_mut_ptr(), || true, &rumbling), 7);
        assert_eq!(set(record, on.as_mut_ptr(), || true, &rumbling), 0);
        assert_eq!(*sent.borrow(), [[1000, 2000], [0, 0]]);
        // Vibration the game ended itself needs no stop.
        assert_eq!(set(record, on.as_mut_ptr(), || false, &rumbling), 7);
        assert_eq!(set(record, off.as_mut_ptr(), || false, &rumbling), 7);
        assert_eq!(set(record, on.as_mut_ptr(), || true, &rumbling), 0);
        assert_eq!(sent.borrow().len(), 4);
    }

    /// The game's own path: cdecl wrappers over xinputdll.dll's cdecl
    /// functions, arguments in order (a failed read is never filtered).
    #[test]
    fn the_games_lookups_are_cdecl_wrappers() {
        unsafe extern "C" fn missing(user: u32, state: *mut u8) -> u32 { unsafe { *state = 0xEE }; 1000 + user }
        REAL[GET_C].store(missing as *const () as usize, Ordering::Release);
        let read: unsafe extern "C" fn(u32, *mut u8) -> u32 = get_state_c;
        assert_eq!(read as *const () as usize, wrapper(GET_C));
        let mut state = [0u8; 16];
        assert_eq!(unsafe { read(3, state.as_mut_ptr()) }, 1003);
        assert_eq!(state[0], 0xEE);
        let write: unsafe extern "C" fn(u32, *mut u16) -> u32 = set_state_c;
        assert_eq!(write as *const () as usize, wrapper(SET_C));
    }

    #[test]
    fn only_xinput_state_lookups_are_wrapped() {
        for module in ["xinputdll.dll", "XINPUT1_3.dll", "xinput1_4.dll", "xinput9_1_0.dll"] {
            assert_eq!(wrapped(module, Ok(b"XInputGetState_")), Some(GET_C));
            assert_eq!(wrapped(module, Ok(b"XInputSetState_")), Some(SET_C));
            assert_eq!(wrapped(module, Ok(b"XInputGetState")), Some(GET));
            assert_eq!(wrapped(module, Ok(b"XInputSetState")), Some(SET));
            assert_eq!(wrapped(module, Err(100)), Some(GET_EX));
            for other in [&b"XInputEnable_"[..], b"XInputEnable", b"XInputGetCapabilities", b"XInputGetStateEx"] {
                assert_eq!(wrapped(module, Ok(other)), None);
            }
            assert_eq!(wrapped(module, Err(2)), None);
        }
        assert_eq!(wrapped("kernel32.dll", Err(100)), None);
        assert_eq!(wrapped("other.dll", Ok(b"XInputGetState")), None);
        assert_eq!(wrapped("xinp", Ok(b"XInputGetState")), None);
    }

    /// Through the real redirect, against the system xinput1_4.dll and kernel32.
    #[test]
    fn the_redirect_returns_wrappers_for_xinput_and_the_real_function_otherwise() {
        GET_PROC_ADDRESS.store(GetProcAddress as *const () as usize, Ordering::Release);
        let path = crate::win::wide_os(crate::win::system_dir().unwrap().join("xinput1_4.dll"));
        let xinput = unsafe { LoadLibraryW(path.as_ptr()) };
        assert!(!xinput.is_null());
        let kernel32 = unsafe { GetModuleHandleW(crate::win::wide("kernel32.dll").as_ptr()) };
        let real = |module, name: *const u8| unsafe { GetProcAddress(module, name) }.map_or(0, |f| f as usize);
        let ours = |module, name: *const u8| unsafe { get_proc_address(module, name) };
        let by_name = |n: &std::ffi::CStr| n.as_ptr().cast::<u8>();

        assert_eq!(ours(xinput, by_name(c"XInputGetState")), wrapper(GET));
        assert_eq!(real_fn(GET), real(xinput, by_name(c"XInputGetState")));
        assert_eq!(ours(xinput, by_name(c"XInputSetState")), wrapper(SET));
        assert_eq!(real_fn(SET), real(xinput, by_name(c"XInputSetState")));
        assert_eq!(ours(xinput, 100 as *const u8), wrapper(GET_EX));
        assert_eq!(real_fn(GET_EX), real(xinput, 100 as *const u8));
        for name in [c"XInputEnable", c"XInputGetCapabilities"] {
            assert_eq!(ours(xinput, by_name(name)), real(xinput, by_name(name)));
        }
        assert_eq!(ours(xinput, by_name(c"XInputGetState_")), 0, "missing stays missing");
        assert_eq!(ours(kernel32, by_name(c"GetTickCount")), real(kernel32, by_name(c"GetTickCount")));
        assert_eq!(ours(kernel32, 100 as *const u8), real(kernel32, 100 as *const u8));

        // The wrapper reaches the real function (in front, or same idle answer).
        let wrapper: unsafe extern "system" fn(u32, *mut u8) -> u32 = unsafe { std::mem::transmute(wrapper(GET)) };
        let mut state = [0u8; 16];
        let direct: unsafe extern "system" fn(u32, *mut u8) -> u32 = unsafe { std::mem::transmute(real_fn(GET)) };
        assert_eq!(unsafe { wrapper(0, state.as_mut_ptr()) }, unsafe { direct(0, state.as_mut_ptr()) });
    }
}
