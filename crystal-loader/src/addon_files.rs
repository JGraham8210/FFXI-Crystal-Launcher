//! Add-on data files are opened by one Crystal host at a time.
//!
//! A batch starts its games together, and every add-on settings library
//! rewrites its file in place on load, truncate then write: Windower's
//! `libs/files.lua` (`io.open(path, 'w')`) under `libs/config.lua`, Ashita's
//! `libs/settings.lua` (saved unconditionally on load). A game reading the file
//! between the truncate and the write gets an empty or partial document:
//! Windower's add-on fails to load (`config.lua:99: XML error`), Ashita's falls
//! back to its defaults and saves them over the player's settings.
//!
//! So every open of an add-on data file (`.xml`, `.lua`, `.json` under the
//! tool's add-on trees) takes a named kernel mutex keyed by the case-folded
//! full path, shared by all Crystal hosts, and holds it until that handle or
//! stream is closed. A reader then sees the file before or after a write,
//! never during one. Logs and other long-lived files are out of scope.
//!
//! The hooked layer is the one the add-ons' Lua `io` and `loadfile` reach:
//! - Windower: `LuaCore.dll` links Lua and the CRT statically; `fopen` and
//!   `_close` end in its own KERNEL32 `CreateFileW` (or `A`, `2`) and
//!   `CloseHandle` imports.
//! - Ashita: `addons.dll` links LuaJIT statically over the UCRT DLL; Lua opens
//!   and closes files through its `fopen` / `_fsopen` / `_wfopen` / `fclose`
//!   imports. (`freopen_s` reuses its stream, which keeps its lock.)
//!
//! Hooks go in, by import slot, when the module loads beside a loaded
//! `Hook.dll` or `Ashita.dll`; the close first, so no lock is taken that its
//! close would not release. Vanilla games are never touched.
//!
//! Deadlock cannot outlast [`SAFETY_NET_MS`]: a mutex is reentrant on its
//! thread (a nested open of the same file passes), a host that dies holding
//! one abandons it to the next waiter, and a wait never exceeds the bound. The
//! bound is not how files are serialized: it fires only when another host
//! holds a file open far longer than any read or save takes (a leaked handle,
//! two files opened in opposite orders), and it is logged. A stream closed on
//! another thread is released by its opening thread at its next hooked call,
//! as a mutex must be.

use std::{
    collections::BTreeMap,
    ffi::c_void,
    path::Path,
    sync::{atomic::{AtomicBool, AtomicUsize, Ordering}, Mutex},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, SetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0},
    Globalization::{MultiByteToWideChar, CP_ACP},
    Storage::FileSystem::GetFullPathNameW,
    System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Threading::{CreateMutexExW, GetCurrentThreadId, ReleaseMutex, WaitForSingleObject, MUTEX_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE},
    },
};

/// The longest wait for another host's file: a deadlock safety net only.
const SAFETY_NET_MS: u32 = 5_000;
const EXTENSIONS: [&str; 3] = ["xml", "lua", "json"];

// The originals behind each wrapper, by index.
const CLOSE_HANDLE: usize = 0;
const CREATE_W: usize = 1;
const CREATE_A: usize = 2;
const CREATE_2: usize = 3;
const FCLOSE: usize = 4;
const FOPEN: usize = 5;
const FSOPEN: usize = 6;
const WFOPEN: usize = 7;
static REAL: [AtomicUsize; 8] = [const { AtomicUsize::new(0) }; 8];

fn wrapper(index: usize) -> usize {
    [close_handle as *const (), create_file_w as *const (), create_file_a as *const (), create_file_2 as *const (),
        fclose as *const (), fopen as *const (), fsopen as *const (), wfopen as *const ()][index] as usize
}

/// A tool's module whose imports carry its add-on file I/O.
struct Tool {
    module: &'static str,
    /// Loaded from the tool's root (the parent of the module's `plugins`).
    marker: &'static str,
    imports: &'static str,
    trees: &'static [&'static str],
    close: (&'static str, usize),
    opens: &'static [(&'static str, usize)],
}
const TOOLS: [Tool; 2] = [
    Tool { module: "LuaCore.dll", marker: "Hook.dll", imports: "KERNEL32.dll", trees: &["addons"],
        close: ("CloseHandle", CLOSE_HANDLE), opens: &[("CreateFileW", CREATE_W), ("CreateFileA", CREATE_A), ("CreateFile2", CREATE_2)] },
    Tool { module: "addons.dll", marker: "Ashita.dll", imports: "" /* the C runtime's stdio, by any of its DLL names */, trees: &["addons", "config\\addons"],
        close: ("fclose", FCLOSE), opens: &[("fopen", FOPEN), ("_fsopen", FSOPEN), ("_wfopen", WFOPEN)] },
];

/// Case-folded `<root>\<tree>\` prefixes whose data files are locked.
static TREES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Once a game host has Windower or Ashita: protect their Lua modules as they
/// load (and any already loaded). A vanilla host is left alone.
pub(crate) fn install() {
    static ONCE: AtomicBool = AtomicBool::new(false);
    let tool = crate::hook_layout::hook_base().is_some() || crate::win::module_base("Ashita.dll") != 0;
    if !tool || ONCE.swap(true, Ordering::SeqCst) {
        return;
    }
    let ntdll = crate::win::module_base("ntdll.dll") as _;
    // SAFETY: ntdll's export, with its documented signature; the cookie is kept for the process.
    let registered = unsafe { GetProcAddress(ntdll, c"LdrRegisterDllNotification".as_ptr().cast()) }.is_some_and(|register| unsafe {
        let register: crate::windower_parallel::Register = std::mem::transmute(register);
        let mut cookie = std::ptr::null_mut();
        register(0, loaded, std::ptr::null_mut(), &mut cookie) >= 0
    });
    if !registered {
        crate::log!("note: add-on files: DLL load notifications unavailable; only modules already loaded are protected");
    }
    for tool in &TOOLS {
        let base = crate::win::module_base(tool.module);
        if let Some(path) = (base != 0).then(|| crate::win::module_path(base as _)).flatten() {
            adopt(base, &path);
        }
    }
}

/// Runs under the loader lock after the module's imports are bound and before
/// its initialization: it loads nothing and waits on nothing.
unsafe extern "system" fn loaded(reason: u32, data: *const crate::windower_parallel::Loaded, _: *mut c_void) {
    let name = |s: *const crate::windower_parallel::UnicodeString| unsafe {
        let s = s.as_ref().filter(|s| !s.buffer.is_null())?;
        Some(String::from_utf16_lossy(std::slice::from_raw_parts(s.buffer, s.length as usize / 2)))
    };
    // SAFETY: the loader's notification data, valid during the call.
    let Some(data) = (reason == 1).then_some(unsafe { data.as_ref() }).flatten() else { return };
    if name(data.base_name).is_some_and(|n| TOOLS.iter().any(|t| n.eq_ignore_ascii_case(t.module))) {
        if let Some(path) = name(data.full_name) {
            adopt(data.base as usize, Path::new(&path));
        }
    }
}

/// Routes one tool module's file opens and closes through the lock.
fn adopt(base: usize, path: &Path) {
    let Some(tool) = path.file_name().and_then(|f| TOOLS.iter().find(|t| f.eq_ignore_ascii_case(t.module))) else { return };
    let Some(root) = path.parent().and_then(Path::parent) else { return };
    // SAFETY: a NUL-terminated full path matches only the module mapped from it.
    if unsafe { GetModuleHandleW(crate::win::wide_os(root.join(tool.marker)).as_ptr()) }.is_null() {
        return;
    }
    // SAFETY: a module the loader just mapped, or one loaded in this process.
    let slots = unsafe { crate::iat::import_slots(base, tool.imports) };
    let route = |(name, index): (&str, usize)| {
        slots.iter().find(|s| s.name.as_deref() == Some(name))
            .is_some_and(|s| crate::boot_timing::replace(s.slot as usize, &REAL[index], wrapper(index)))
    };
    if !route(tool.close) {
        crate::log!("note: add-on files: {} {} import missing or redirected by another module; its add-on files are not locked", tool.module, tool.close.0);
        return;
    }
    {
        let mut trees = TREES.lock().unwrap_or_else(|e| e.into_inner());
        for tree in tool.trees {
            let prefix = format!("{}\\{tree}\\", root.display()).to_lowercase();
            if !trees.contains(&prefix) { trees.push(prefix); }
        }
    }
    let routed: Vec<&str> = tool.opens.iter().filter(|open| route(**open)).map(|open| open.0).collect();
    crate::log!("add-on files: {} opens ({}) take a per-file lock shared by Crystal hosts", tool.module, routed.join(", "));
}

/// The case-folded full path of `name` when it is an add-on data file.
fn scoped(name: &[u16]) -> Option<String> {
    let file = name.rsplit(|&c| c == b'\\' as u16 || c == b'/' as u16).next()?;
    let extension = &file[file.iter().rposition(|&c| c == b'.' as u16)? + 1..];
    if !EXTENSIONS.iter().any(|e| String::from_utf16_lossy(extension).eq_ignore_ascii_case(e)) {
        return None;
    }
    let z: Vec<u16> = name.iter().copied().chain(Some(0)).collect();
    let mut full = vec![0u16; 1024];
    loop {
        // SAFETY: NUL-terminated input; the buffer is valid for its length.
        let n = unsafe { GetFullPathNameW(z.as_ptr(), full.len() as u32, full.as_mut_ptr(), std::ptr::null_mut()) } as usize;
        if n == 0 { return None; }
        if n < full.len() { full.truncate(n); break; }
        full.resize(n, 0);
    }
    let full = String::from_utf16_lossy(&full).to_lowercase();
    let full = full.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(full);
    TREES.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|t| full.starts_with(t.as_str())).then_some(full)
}

/// The shared mutex for a case-folded path, opened once per host.
fn mutex_for(path: &str) -> Option<usize> {
    static OPEN: Mutex<BTreeMap<u64, usize>> = Mutex::new(BTreeMap::new());
    let key = path.encode_utf16().fold(0xcbf2_9ce4_8422_2325u64, |h, c| (h ^ u64::from(c)).wrapping_mul(0x0100_0000_01b3));
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&mutex) = open.get(&key) { return Some(mutex); }
    // CreateMutexExW: Windower renames CreateMutexW objects per game, which
    // would separate the hosts (see windower_plugin_settings.rs).
    let name = crate::win::wide(&format!("Local\\CrystalLauncher.AddonFile.v1.{key:016x}"));
    // SAFETY: NUL-terminated name, default security; kept for the process.
    let mutex = unsafe { CreateMutexExW(std::ptr::null(), name.as_ptr(), 0, MUTEX_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE) } as usize;
    if mutex == 0 {
        crate::log!("note: add-on files: cannot create a file lock: {}", std::io::Error::last_os_error());
        return None;
    }
    open.insert(key, mutex);
    Some(mutex)
}

/// Waits for `path`'s mutex; None when the safety net fired.
fn acquire(path: &str, timeout: u32) -> Option<usize> {
    release_pending();
    let mutex = mutex_for(path)?;
    // SAFETY: a mutex handle this host keeps open.
    match unsafe { WaitForSingleObject(mutex as HANDLE, timeout) } {
        WAIT_OBJECT_0 => Some(mutex),
        WAIT_ABANDONED => {
            crate::log!("note: add-on files: a host exited holding {}; the file may hold its partial write", file_name(path));
            Some(mutex)
        }
        status => {
            crate::log!("note: add-on files: {} still held by another host after {timeout} ms (wait {status:#x}); opened without the lock", file_name(path));
            None
        }
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

/// A lock taken by an open, until its handle or stream is closed.
struct Held { key: usize, mutex: usize, thread: u32 }
static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());
/// Closed on another thread: released by `thread` itself.
static PENDING: Mutex<Vec<Held>> = Mutex::new(Vec::new());

fn release(held: Held) {
    // SAFETY: the current thread's id.
    if held.thread != unsafe { GetCurrentThreadId() } {
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).push(held);
    } else if unsafe { ReleaseMutex(held.mutex as HANDLE) } == 0 {
        crate::log!("note: add-on files: lock release failed: {}", std::io::Error::last_os_error());
    }
}

fn release_pending() {
    // SAFETY: the current thread's id.
    let me = unsafe { GetCurrentThreadId() };
    let mut mine = Vec::new();
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).retain(|h| if h.thread == me { mine.push(h.mutex); false } else { true });
    for mutex in mine {
        release(Held { key: 0, mutex, thread: me });
    }
}

/// After an open: keep the lock with what it returned, or release it.
fn opened(key: usize, mutex: Option<usize>, ok: bool) {
    let Some(mutex) = mutex else { return };
    // SAFETY: the current thread's id.
    let held = Held { key, mutex, thread: unsafe { GetCurrentThreadId() } };
    if !ok { return release(held); }
    let stale = {
        let mut all = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let stale = all.iter().position(|h| h.key == key).map(|at| all.swap_remove(at));
        all.push(held);
        stale
    };
    // The value came back from a new open, so the old one was closed outside
    // the hooked module: its lock is due.
    if let Some(stale) = stale { release(stale); }
}

/// After a close: release the lock its open took.
fn closed(key: usize) {
    release_pending();
    let held = {
        let mut all = HELD.lock().unwrap_or_else(|e| e.into_inner());
        all.iter().position(|h| h.key == key).map(|at| all.swap_remove(at))
    };
    if let Some(held) = held { release(held); }
}

/// Crystal's own bookkeeping never changes the caller's last error.
fn quiet<T>(work: impl FnOnce() -> T) -> T {
    // SAFETY: thread-local error value.
    let saved = unsafe { GetLastError() };
    let result = work();
    unsafe { SetLastError(saved) };
    result
}

unsafe fn wide_name<'a>(name: *const u16) -> Option<&'a [u16]> {
    if name.is_null() { return None; }
    let length = (0..32_768).find(|&at| unsafe { *name.add(at) } == 0)?;
    Some(unsafe { std::slice::from_raw_parts(name, length) })
}

unsafe fn ansi_name(name: *const u8) -> Option<Vec<u16>> {
    if name.is_null() { return None; }
    let mut wide = vec![0u16; 32_768];
    // SAFETY: NUL-terminated input; the buffer is valid for its length.
    let n = unsafe { MultiByteToWideChar(CP_ACP, 0, name, -1, wide.as_mut_ptr(), wide.len() as i32) };
    (n > 0).then(|| { wide.truncate(n as usize - 1); wide })
}

fn lock_wide(name: Option<&[u16]>) -> Option<usize> {
    quiet(|| acquire(&scoped(name?)?, SAFETY_NET_MS))
}

fn real<F: Copy>(index: usize) -> F {
    let address = REAL[index].load(Ordering::Acquire);
    // SAFETY: F is the function-pointer type of slot `index`, stored before its wrapper was installed.
    unsafe { std::mem::transmute_copy(&address) }
}

type CreateW = unsafe extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, HANDLE) -> HANDLE;
type CreateA = unsafe extern "system" fn(*const u8, u32, u32, *const c_void, u32, u32, HANDLE) -> HANDLE;
type Create2 = unsafe extern "system" fn(*const u16, u32, u32, u32, *const c_void) -> HANDLE;
type Close = unsafe extern "system" fn(HANDLE) -> i32;
type Fopen = unsafe extern "C" fn(*const u8, *const u8) -> *mut c_void;
type Fsopen = unsafe extern "C" fn(*const u8, *const u8, i32) -> *mut c_void;
type Wfopen = unsafe extern "C" fn(*const u16, *const u16) -> *mut c_void;
type Fclose = unsafe extern "C" fn(*mut c_void) -> i32;

unsafe extern "system" fn create_file_w(name: *const u16, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32, template: HANDLE) -> HANDLE {
    let lock = lock_wide(unsafe { wide_name(name) });
    let handle = unsafe { real::<CreateW>(CREATE_W)(name, access, share, security, disposition, flags, template) };
    quiet(|| opened(handle as usize, lock, handle != INVALID_HANDLE_VALUE));
    handle
}

unsafe extern "system" fn create_file_a(name: *const u8, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32, template: HANDLE) -> HANDLE {
    let lock = lock_wide(unsafe { ansi_name(name) }.as_deref());
    let handle = unsafe { real::<CreateA>(CREATE_A)(name, access, share, security, disposition, flags, template) };
    quiet(|| opened(handle as usize, lock, handle != INVALID_HANDLE_VALUE));
    handle
}

unsafe extern "system" fn create_file_2(name: *const u16, access: u32, share: u32, disposition: u32, parameters: *const c_void) -> HANDLE {
    let lock = lock_wide(unsafe { wide_name(name) });
    let handle = unsafe { real::<Create2>(CREATE_2)(name, access, share, disposition, parameters) };
    quiet(|| opened(handle as usize, lock, handle != INVALID_HANDLE_VALUE));
    handle
}

/// Released after the close (whatever it returns: a held value that fails to
/// close was closed elsewhere), so the next host reads the finished file.
unsafe extern "system" fn close_handle(handle: HANDLE) -> i32 {
    let result = unsafe { real::<Close>(CLOSE_HANDLE)(handle) };
    quiet(|| closed(handle as usize));
    result
}

unsafe extern "C" fn fopen(name: *const u8, mode: *const u8) -> *mut c_void {
    let lock = lock_wide(unsafe { ansi_name(name) }.as_deref());
    let file = unsafe { real::<Fopen>(FOPEN)(name, mode) };
    quiet(|| opened(file as usize, lock, !file.is_null()));
    file
}

unsafe extern "C" fn fsopen(name: *const u8, mode: *const u8, share: i32) -> *mut c_void {
    let lock = lock_wide(unsafe { ansi_name(name) }.as_deref());
    let file = unsafe { real::<Fsopen>(FSOPEN)(name, mode, share) };
    quiet(|| opened(file as usize, lock, !file.is_null()));
    file
}

unsafe extern "C" fn wfopen(name: *const u16, mode: *const u16) -> *mut c_void {
    let lock = lock_wide(unsafe { wide_name(name) });
    let file = unsafe { real::<Wfopen>(WFOPEN)(name, mode) };
    quiet(|| opened(file as usize, lock, !file.is_null()));
    file
}

/// fclose ends the stream even when its flush fails.
unsafe extern "C" fn fclose(file: *mut c_void) -> i32 {
    let result = unsafe { real::<Fclose>(FCLOSE)(file) };
    quiet(|| closed(file as usize));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, process::Command, thread};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, CREATE_ALWAYS, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::LibraryLoader::LoadLibraryW,
    };
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;

    /// A fresh tool root with its add-on tree in scope, and the kernel32 originals.
    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("crystal-addon-files-{name}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("addons\\demo\\data")).unwrap();
        TREES.lock().unwrap().push(format!("{}\\addons\\", root.display()).to_lowercase());
        REAL[CREATE_W].store(CreateFileW as *const () as usize, Ordering::Release);
        REAL[CLOSE_HANDLE].store(CloseHandle as *const () as usize, Ordering::Release);
        root
    }

    fn free_elsewhere(path: &str) -> bool {
        let path = path.to_owned();
        thread::spawn(move || acquire(&path, 0).map(|m| unsafe { ReleaseMutex(m as HANDLE) }).is_some()).join().unwrap()
    }

    fn wide(path: &Path) -> Vec<u16> { crate::win::wide_os(path) }

    #[test]
    fn only_data_files_under_the_add_on_trees_are_locked() {
        let root = root("scope");
        let folded = |p: &Path| scoped(&wide(p)[..wide(p).len() - 1]);
        let settings = root.join("addons/demo/data/Settings.XML");
        assert_eq!(folded(&settings).as_deref(), Some(format!("{}\\addons\\demo\\data\\settings.xml", root.display()).to_lowercase().as_str()));
        assert!(folded(&root.join("addons\\demo\\demo.lua")).is_some());
        assert!(folded(&root.join("addons\\demo\\data\\..\\cache.json")).is_some());
        for other in ["addons\\demo\\data\\chat.log", "addons\\demo\\data\\notes.txt", "addons\\demo\\data", "scripts\\init.lua", "plugins\\demo.xml"] {
            assert_eq!(folded(&root.join(other)), None, "{other}");
        }
        assert_eq!(folded(&root.with_file_name("elsewhere").join("addons\\a.xml")), None);
    }

    #[test]
    fn a_lock_excludes_other_holders_and_reenters_on_its_thread() {
        let path = format!("{}\\addons\\demo\\data\\settings.xml", root("reenter").display()).to_lowercase();
        let first = acquire(&path, 0).unwrap();
        assert_eq!(acquire(&path, 0), Some(first), "a nested open of the same file passes");
        assert!(!free_elsewhere(&path));
        unsafe { ReleaseMutex(first as HANDLE) };
        assert!(!free_elsewhere(&path), "until every nested open is closed");
        unsafe { ReleaseMutex(first as HANDLE) };
        assert!(free_elsewhere(&path));
        // A different file is a different lock.
        let other = acquire(&path.replace("settings", "other"), 0).unwrap();
        assert!(free_elsewhere(&path));
        unsafe { ReleaseMutex(other as HANDLE) };
    }

    #[test]
    fn a_handle_holds_its_files_lock_until_it_is_closed() {
        let root = root("handle");
        let file = root.join("addons\\demo\\data\\settings.xml");
        let path = format!("{}", file.display()).to_lowercase();
        let name = wide(&file);
        let open = |disposition| unsafe { create_file_w(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), disposition, 0, std::ptr::null_mut()) };
        // A failed open (no file yet) keeps no lock and keeps its error.
        assert_eq!(open(OPEN_EXISTING), INVALID_HANDLE_VALUE);
        assert_eq!(unsafe { GetLastError() }, 2);
        assert!(free_elsewhere(&path));
        let (a, b) = (open(CREATE_ALWAYS), open(OPEN_EXISTING));
        assert!(a != INVALID_HANDLE_VALUE && b != INVALID_HANDLE_VALUE);
        unsafe { close_handle(a) };
        assert!(!free_elsewhere(&path), "the second handle still holds it");
        unsafe { close_handle(b) };
        assert!(free_elsewhere(&path));
        // A log beside it never takes a lock.
        let log = wide(&root.join("addons\\demo\\data\\chat.log"));
        let handle = unsafe { create_file_w(log.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_ALWAYS, 0, std::ptr::null_mut()) };
        assert!(HELD.lock().unwrap().iter().all(|h| h.key != handle as usize));
        unsafe { close_handle(handle) };
    }

    #[test]
    fn a_crt_stream_holds_its_files_lock_until_fclose() {
        let root = root("stream");
        let file = root.join("addons\\demo\\data\\stream.lua");
        let path = format!("{}", file.display()).to_lowercase();
        // Ashita's addons.dll gets these from the UCRT DLL.
        let ucrt = unsafe { LoadLibraryW(crate::win::wide("ucrtbase.dll").as_ptr()) };
        for (name, index) in [(c"fopen", FOPEN), (c"_wfopen", WFOPEN), (c"fclose", FCLOSE)] {
            let f = unsafe { GetProcAddress(ucrt, name.as_ptr().cast()) }.unwrap();
            REAL[index].store(f as usize, Ordering::Release);
        }
        let narrow = std::ffi::CString::new(file.to_str().unwrap()).unwrap();
        let stream = unsafe { fopen(narrow.as_ptr().cast(), c"w+".as_ptr().cast()) };
        assert!(!stream.is_null());
        assert!(!free_elsewhere(&path));
        let reread = unsafe { wfopen(wide(&file).as_ptr(), crate::win::wide("r").as_ptr()) };
        assert!(!reread.is_null(), "the same thread reopens the file it holds");
        unsafe { fclose(stream) };
        assert!(!free_elsewhere(&path));
        unsafe { fclose(reread) };
        assert!(free_elsewhere(&path));
    }

    #[test]
    fn a_close_on_another_thread_is_released_by_the_opener() {
        let root = root("cross");
        let file = root.join("addons\\demo\\data\\cross.json");
        let path = format!("{}", file.display()).to_lowercase();
        let name = wide(&file);
        let handle = unsafe { create_file_w(name.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_ALWAYS, 0, std::ptr::null_mut()) } as usize;
        thread::spawn(move || unsafe { close_handle(handle as HANDLE) }).join().unwrap();
        assert!(!free_elsewhere(&path), "a mutex is released only by its owner");
        // The opener's next hooked call releases it.
        unsafe { close_handle(std::ptr::null_mut()) };
        assert!(free_elsewhere(&path));
    }

    /// The installed modules (mapped as data, nothing runs) import every
    /// function their tool's hooks expect.
    #[test]
    #[ignore = "reads the installed Windower and Ashita; run alone"]
    fn installed_tool_modules_import_the_hooked_functions() {
        use windows_sys::Win32::System::LibraryLoader::{LoadLibraryExW, DONT_RESOLVE_DLL_REFERENCES};
        let games = Path::new(r"C:\Games\Final Fantasy XI Programs");
        for (tool, root) in TOOLS.iter().zip(["Windower 4", "AshitaV4"]) {
            let path = games.join(root).join("plugins").join(tool.module);
            let base = unsafe { LoadLibraryExW(wide(&path).as_ptr(), std::ptr::null_mut(), DONT_RESOLVE_DLL_REFERENCES) } as usize;
            assert_ne!(base, 0, "{}", path.display());
            let names: Vec<_> = unsafe { crate::iat::import_slots(base, tool.imports) }.into_iter().filter_map(|s| s.name).collect();
            for (name, _) in std::iter::once(&tool.close).chain(tool.opens) {
                assert!(names.iter().any(|n| n == name), "{} imports no {name}", tool.module);
            }
        }
    }

    /// One simulated game: `rounds` times, read the settings whole (counting
    /// torn reads), then save them as config.lua does (truncate, then write).
    #[test]
    fn race_child() {
        let Ok(spec) = std::env::var("CRYSTAL_ADDON_RACE") else { return };
        let [dir, locked, rounds]: [&str; 3] = spec.split('|').collect::<Vec<_>>().try_into().unwrap();
        let file = Path::new(dir).join("addons\\demo\\data\\settings.xml");
        REAL[CREATE_W].store(CreateFileW as *const () as usize, Ordering::Release);
        REAL[CLOSE_HANDLE].store(CloseHandle as *const () as usize, Ordering::Release);
        if locked == "1" {
            TREES.lock().unwrap().push(format!("{dir}\\addons\\").to_lowercase());
        }
        let name = wide(&file);
        let open = |access, disposition| unsafe { create_file_w(name.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), disposition, 0, std::ptr::null_mut()) };
        let mut torn = 0;
        for _ in 0..rounds.parse::<u32>().unwrap() {
            let handle = open(GENERIC_READ, OPEN_EXISTING);
            let mut buffer = vec![0u8; 4096];
            let mut read = 0u32;
            unsafe { ReadFile(handle, buffer.as_mut_ptr(), buffer.len() as u32, &mut read, std::ptr::null_mut()) };
            unsafe { close_handle(handle) };
            torn += u32::from(&buffer[..read as usize] != SEED.as_bytes());
            let handle = open(GENERIC_WRITE, CREATE_ALWAYS);
            let (head, tail) = SEED.as_bytes().split_at(SEED.len() / 2);
            for part in [head, tail] {
                unsafe { WriteFile(handle, part.as_ptr(), part.len() as u32, &mut read, std::ptr::null_mut()) };
            }
            unsafe { close_handle(handle) };
        }
        println!("torn={torn}");
    }

    const SEED: &str = "<?xml version=\"1.1\" ?>\n<settings>\n    <global>\n        <pos><x>640</x><y>480</y></pos>\n        <enabled>true</enabled>\n    </global>\n</settings>\n";

    /// Torn reads seen by `games` simultaneous processes.
    fn race(games: usize, locked: bool, rounds: u32) -> u32 {
        let dir = std::env::temp_dir().join(format!("crystal-addon-race-{}-{locked}", std::process::id()));
        std::fs::create_dir_all(dir.join("addons\\demo\\data")).unwrap();
        std::fs::write(dir.join("addons\\demo\\data\\settings.xml"), SEED).unwrap();
        let spec = format!("{}|{}|{rounds}", dir.display(), u8::from(locked));
        let children: Vec<_> = (0..games).map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "addon_files::tests::race_child", "--nocapture", "--test-threads=1"])
                .env("CRYSTAL_ADDON_RACE", &spec)
                .stdout(std::process::Stdio::piped())
                .spawn().unwrap()
        }).collect();
        let torn = children.into_iter().map(|child| {
            let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
            let count = out.split("torn=").nth(1).expect("child result");
            count[..count.find(|c: char| !c.is_ascii_digit()).unwrap_or(count.len())].parse::<u32>().unwrap()
        }).sum();
        std::fs::remove_dir_all(dir).unwrap();
        torn
    }

    #[test]
    fn simultaneous_games_never_read_a_half_written_settings_file() {
        assert_eq!(race(4, true, 1000), 0);
    }

    #[test]
    #[ignore = "comparison: the same race without the lock; run alone with --nocapture"]
    fn simultaneous_games_without_the_lock() {
        let (unlocked, locked) = (race(6, false, 2000), race(6, true, 2000));
        println!("torn reads over 6 games x 2000 loads: unlocked {unlocked}, locked {locked}");
        assert_eq!(locked, 0);
    }
}
