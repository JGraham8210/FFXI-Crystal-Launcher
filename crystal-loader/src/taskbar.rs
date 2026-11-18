//! One taskbar identity for every game host.
//!
//! The taskbar groups buttons by application ID. A process that sets none gets the path of its
//! executable as its ID, and the host runs from more than one path: the installed
//! `crystal\pol.exe`, the copy a portable launcher extracts, the copy a new version writes while
//! an older one still runs a game (`crystal\<hash>\pol.exe`), and the copy beside ReShade
//! (`reshade\hosts\<host hash>-<dll hash>\pol.exe`, a new folder with every Crystal update).
//! Games whose hosts ran from two of these came up as two groups of buttons (seen 2026-11-16:
//! "...\hosts\018a5cb2...\pol.exe", 1 window, beside "...\hosts\b730d9ea...\pol.exe", 4 windows).
//!
//! With one explicit ID, set before the process has a window, every game is in one group
//! wherever its host runs from. Only this process's own ID is set: no button is added, removed
//! or moved, and no window is touched.

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

/// The application ID of every Crystal game host, whatever folder it runs from. Not the
/// launcher's (its window keeps its own button). Changing it splits the games of two Crystal
/// versions into two groups while both run.
pub(crate) const APP_ID: &str = "CrystalLauncher.GameHost";

/// A shell32 export by name. shell32 is found in the system directory only and stays loaded:
/// the Viewer's own modules import it, so the process has it for its whole life either way.
/// Resolved at run time, so the host's static import list stays as it is.
fn shell32(name: &std::ffi::CStr) -> Option<unsafe extern "system" fn() -> isize> {
    let library = crate::win::wide("shell32.dll");
    // SAFETY: NUL-terminated names; a null module or missing export is `None`.
    unsafe {
        let module = LoadLibraryExW(library.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
        if module.is_null() { return None; }
        GetProcAddress(module, name.as_ptr().cast())
    }
}

/// Gives this process `APP_ID` (SetCurrentProcessExplicitAppUserModelID). Call before the
/// process creates a window: the taskbar reads the ID when it makes a window's button.
pub(crate) fn set_app_id() -> Result<(), String> {
    type Set = unsafe extern "system" fn(*const u16) -> i32;
    let set = shell32(c"SetCurrentProcessExplicitAppUserModelID").ok_or("shell32 has no SetCurrentProcessExplicitAppUserModelID")?;
    let id = crate::win::wide(APP_ID);
    // SAFETY: the export's documented signature (PCWSTR in, HRESULT out); `id` is NUL-terminated.
    let result = unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Set>(set)(id.as_ptr()) };
    if result < 0 { Err(format!("HRESULT {result:#010X}")) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This process's explicit application ID as the shell reports it; None when it has none.
    fn current_app_id() -> Option<String> {
        type Get = unsafe extern "system" fn(*mut *mut u16) -> i32;
        let get = shell32(c"GetCurrentProcessExplicitAppUserModelID")?;
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: the export's documented signature; the string it allocates is read to its
        // NUL and freed with CoTaskMemFree, as documented.
        unsafe {
            if std::mem::transmute::<unsafe extern "system" fn() -> isize, Get>(get)(&mut text) < 0 || text.is_null() { return None; }
            let length = (0..).take_while(|&i| *text.add(i) != 0).count();
            let id = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            windows_sys::Win32::System::Com::CoTaskMemFree(text.cast());
            Some(id)
        }
    }

    #[test]
    fn every_host_takes_the_same_explicit_application_id_whatever_its_path() {
        // What Windows accepts as an ID: at most 128 characters, no spaces.
        assert!(!APP_ID.is_empty() && APP_ID.len() <= 128 && APP_ID.is_ascii() && !APP_ID.contains(' '), "{APP_ID}");
        // A constant: nothing of the host's path, version or launch tool is in it, so hosts of
        // two Crystal versions, with and without ReShade, share one taskbar group.
        assert_eq!(APP_ID, "CrystalLauncher.GameHost");
        set_app_id().expect("the shell takes the ID");
        assert_eq!(current_app_id().as_deref(), Some(APP_ID), "the process reports the ID the taskbar groups by");
        // Setting it again (it never is) changes nothing.
        set_app_id().expect("idempotent");
        assert_eq!(current_app_id().as_deref(), Some(APP_ID));
    }
}
