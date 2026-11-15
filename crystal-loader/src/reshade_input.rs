//! Windower's key and click blocking under ReShade.
//!
//! Hook.dll (4.7.9.8) hooks the game thread from its DirectInput CreateDevice
//! wrapper: `SetWindowsHookExA(WH_KEYBOARD or WH_MOUSE, proc, NULL,
//! GetCurrentThreadId())`. To keep a bound key or a click on its own windows
//! from the game, a proc returns 1 without calling the next hook.
//!
//! ReShade (6.8.0, `source/input_windows.cpp`) detours SetWindowsHookEx inline.
//! While it tracks no live hook of a kind on a thread, it installs its
//! `handle_windows_hook` in place of the proc: that skips the proc while the
//! overlay takes the input, and otherwise calls it but answers a non-zero
//! result with `CallNextHookEx`, so the event reaches the game as well (Win+1
//! also typed into chat). Later procs of that kind go in unwrapped, until the
//! tracked hook is unhooked.
//!
//! Hook.dll's import is redirected here. With ReShade loaded, Windower's proc
//! goes in behind [`relay`], which ReShade wraps, after a [`backstop`] hook
//! which it does not. The relay notes a non-zero result; ReShade's
//! `CallNextHookEx` then reaches the backstop, which returns the 1 ReShade
//! dropped. ReShade still keeps the overlay's input from Windower and still
//! sees every event Windower lets through. Without ReShade the call is
//! Windower's own.

use std::{cell::Cell, path::Path, ptr::null_mut, sync::atomic::{AtomicUsize, Ordering}};
use windows_sys::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::WindowsAndMessaging::{CallNextHookEx, UnhookWindowsHookEx, HHOOK, WH_KEYBOARD, WH_MOUSE},
};

type Proc = unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT;
type SetHook = unsafe extern "system" fn(i32, Option<Proc>, HINSTANCE, u32) -> HHOOK;

/// Hook.dll's own import target.
static SET_HOOK: AtomicUsize = AtomicUsize::new(0);

/// One hook kind on the thread that owns the hooks.
struct Kind {
    windower: Cell<Option<Proc>>,
    /// The backstop hook is in place.
    backstop: Cell<bool>,
    /// Windower's proc returned non-zero for the event being handled.
    swallowed: Cell<bool>,
}

thread_local! {
    static KINDS: [Kind; 2] = const { [const { Kind { windower: Cell::new(None), backstop: Cell::new(false), swallowed: Cell::new(false) } }; 2] };
}

const HOOKS: [(i32, Proc, Proc, &str); 2] = [(WH_KEYBOARD, relay::<0>, backstop::<0>, "keyboard"), (WH_MOUSE, relay::<1>, backstop::<1>, "mouse")];

/// Once Hook.dll is loaded, before the game creates its input devices.
pub(crate) fn install() {
    let Some(base) = crate::hook_layout::hook_base() else { return };
    // SAFETY: the loaded Hook.dll image.
    let slot = unsafe { crate::iat::import_slots(base, "USER32.dll") }.into_iter().find(|s| s.name.as_deref() == Some("SetWindowsHookExA"));
    match slot {
        Some(slot) if crate::boot_timing::replace(slot.slot as usize, &SET_HOOK, set_windows_hook_ex_a as *const () as usize) => {}
        Some(_) => crate::log!("note: ReShade input: Hook.dll's SetWindowsHookExA import is redirected by another module; left as is"),
        None => crate::log!("note: ReShade input: Hook.dll imports no SetWindowsHookExA; Windower's blocking is not kept under ReShade"),
    }
}

fn loaded(module: &Path) -> bool {
    // SAFETY: NUL-terminated path; a full path matches only that file's mapping.
    !unsafe { GetModuleHandleW(crate::win::wide_os(module).as_ptr()) }.is_null()
}

/// ReShade as Crystal installs it: `dxgi.dll` beside this host.
fn reshade_loaded() -> bool {
    std::env::current_exe().is_ok_and(|host| loaded(&host.with_file_name("dxgi.dll")))
}

unsafe extern "system" fn set_windows_hook_ex_a(id: i32, proc: Option<Proc>, module: HINSTANCE, thread: u32) -> HHOOK {
    let set: SetHook = unsafe { std::mem::transmute(SET_HOOK.load(Ordering::Acquire)) };
    unsafe { hook(set, reshade_loaded, id, proc, module, thread) }
}

/// Hook.dll's SetWindowsHookExA call, made through `set`.
unsafe fn hook(set: SetHook, reshade: impl FnOnce() -> bool, id: i32, proc: Option<Proc>, module: HINSTANCE, thread: u32) -> HHOOK {
    let kind = HOOKS.iter().position(|h| h.0 == id).filter(|_| proc.is_some() && thread == unsafe { GetCurrentThreadId() });
    let Some(kind) = kind.filter(|_| reshade()) else { return unsafe { set(id, proc, module, thread) } };
    let (_, relay, backstop, name) = HOOKS[kind];
    KINDS.with(|kinds| unsafe {
        let state = &kinds[kind];
        if !state.backstop.get() {
            // The first backstop takes ReShade's wrapper and the second goes in
            // plain; unhooking the first frees the wrapper for the relay.
            let wrapped = set(id, Some(backstop), null_mut(), thread);
            state.backstop.set(!set(id, Some(backstop), null_mut(), thread).is_null());
            if !wrapped.is_null() { UnhookWindowsHookEx(wrapped); }
            if state.backstop.get() {
                crate::log!("ReShade input: Windower's {name} hook keeps its blocking");
            } else {
                crate::log!("note: ReShade input: no backstop hook; Windower's {name} blocking is not kept");
            }
        }
        state.windower.set(proc);
        set(id, Some(relay), module, thread)
    })
}

/// Windower's proc, as ReShade's wrapper (or Windows, when nothing wraps it) calls it.
unsafe extern "system" fn relay<const K: usize>(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    KINDS.with(|kinds| unsafe {
        let Some(windower) = kinds[K].windower.get() else { return CallNextHookEx(null_mut(), code, wparam, lparam) };
        // Cleared first: a proc that lets the event through calls the backstop itself.
        kinds[K].swallowed.set(false);
        let result = windower(code, wparam, lparam);
        kinds[K].swallowed.set(result != 0);
        result
    })
}

/// The hook after the relay: returns the non-zero result ReShade's wrapper dropped.
unsafe extern "system" fn backstop<const K: usize>(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if KINDS.with(|kinds| kinds[K].swallowed.replace(false)) { 1 } else { unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowsHookExA, WH_GETMESSAGE};

    unsafe extern "system" fn swallows(_: i32, _: WPARAM, _: LPARAM) -> LRESULT { 1 }
    /// Hook.dll's procs call the next hook when they let an event through: the backstop.
    unsafe extern "system" fn passes(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT { unsafe { backstop::<1>(code, wparam, lparam) } }

    /// ReShade 6.8.0's `handle_windows_hook` around the relay, the backstop next in the chain.
    fn through_reshade(overlay_takes_input: bool) -> LRESULT {
        unsafe {
            if !overlay_takes_input && relay::<1>(0, 1, 0) == 0 { return 0; }
            backstop::<1>(0, 1, 0)
        }
    }

    #[test]
    fn what_windower_swallows_stays_swallowed_through_reshades_wrapper() {
        let windower = |proc: Proc| KINDS.with(|kinds| kinds[1].windower.set(Some(proc)));
        windower(swallows);
        assert_eq!(through_reshade(false), 1);
        assert_eq!(through_reshade(true), 0, "overlay input skips Windower and swallows nothing");
        windower(passes);
        assert_eq!(through_reshade(false), 0);
        // Unwrapped (ReShade's input hooks off), Windows honours the 1 itself and never
        // calls the backstop: the next event must not inherit it.
        windower(swallows);
        assert_eq!(unsafe { relay::<1>(0, 1, 0) }, 1);
        windower(passes);
        assert_eq!(unsafe { relay::<1>(0, 1, 0) }, 0);
    }

    thread_local! { static CALLS: RefCell<Vec<(i32, usize)>> = const { RefCell::new(Vec::new()) }; }

    unsafe extern "system" fn recorded(id: i32, proc: Option<Proc>, module: HINSTANCE, thread: u32) -> HHOOK {
        CALLS.with(|calls| calls.borrow_mut().push((id, proc.map_or(0, |p| p as usize))));
        unsafe { SetWindowsHookExA(id, proc, module, thread) }
    }

    /// Real hooks on this test's thread, which never reads input.
    #[test]
    fn windowers_hooks_go_in_behind_the_relay_only_under_reshade() {
        let this = unsafe { GetCurrentThreadId() };
        let calls = |reshade: bool, id: i32, thread: u32| {
            let hook = unsafe { hook(recorded, || reshade, id, Some(swallows), null_mut(), thread) };
            // Hook.dll unhooks when the game releases the input device.
            if !hook.is_null() { unsafe { UnhookWindowsHookEx(hook) }; }
            CALLS.with(|calls| std::mem::take(&mut *calls.borrow_mut()))
        };
        let at = |proc: Proc| proc as usize;
        // No ReShade, another hook type, a thread other than the caller's: Windower's own call.
        assert_eq!(calls(false, WH_KEYBOARD, this), [(WH_KEYBOARD, at(swallows))]);
        assert_eq!(calls(true, WH_GETMESSAGE, this), [(WH_GETMESSAGE, at(swallows))]);
        assert_eq!(calls(true, WH_KEYBOARD, 0), [(WH_KEYBOARD, at(swallows))]);
        // Under ReShade: the backstop twice (wrapped, then plain), then the relay.
        let (key, stop) = (at(relay::<0>), at(backstop::<0>));
        assert_eq!(calls(true, WH_KEYBOARD, this), [(WH_KEYBOARD, stop), (WH_KEYBOARD, stop), (WH_KEYBOARD, key)]);
        assert_eq!(KINDS.with(|kinds| kinds[0].windower.get()).map(at), Some(at(swallows)));
        // A later device on the same thread: the backstop is already there.
        assert_eq!(calls(true, WH_KEYBOARD, this), [(WH_KEYBOARD, key)]);
        let (click, stop) = (at(relay::<1>), at(backstop::<1>));
        assert_eq!(calls(true, WH_MOUSE, this), [(WH_MOUSE, stop), (WH_MOUSE, stop), (WH_MOUSE, click)]);
    }

    #[test]
    fn reshade_is_the_dxgi_mapped_from_beside_the_host() {
        let host = std::env::current_exe().unwrap();
        assert!(loaded(&host), "a full path finds the module mapped from it");
        assert!(!loaded(&host.with_file_name("ntdll.dll")), "and not a same-named module from elsewhere");
        assert!(!reshade_loaded());
    }
}
