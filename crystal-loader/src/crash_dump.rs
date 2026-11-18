//! A minidump of a real crash, in Crystal's `Logs\crashdumps` (`data_paths`), the newest three
//! kept. Windows Error Reporting's LocalDumps would also catch crashes this cannot, but it is
//! configured per exe name: `pol.exe` would include the user's own PlayOnline, and it needs a
//! machine-wide registry entry. So the host writes its own.
//!
//! The filter runs on the crashing thread, which may have no stack left (a stack overflow) or
//! hold a lock: it only records the exception and wakes a thread started at install, which
//! writes the dump with dbghelp (loaded from System32 at install, never at crash time). The
//! filter then hands the exception on, so Windows' own crash handling still follows. A later
//! component that installs its own filter (a tool or the game) replaces this one.
//!
//! The dump is a small one: thread stacks, modules and the exception, no heap. Its name ends
//! with ` pol <pid>.dmp`, so the launcher can name it in the game's log.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Diagnostics::Debug::{SetUnhandledExceptionFilter, EXCEPTION_POINTERS};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, GetCurrentThreadId, SetEvent, WaitForSingleObject};

/// Dumps kept, newest first.
pub(crate) const KEEP: usize = 3;
/// How long the crashing thread waits for its dump.
const WRITE_WAIT_MS: u32 = 30_000;

/// MiniDumpWithThreadInfo | MiniDumpWithUnloadedModules: stacks and modules, no heap.
const DUMP_TYPE: i32 = 0x1000 | 0x20;

#[repr(C, packed(4))]
struct ExceptionInformation {
    thread_id: u32,
    pointers: *const EXCEPTION_POINTERS,
    client_pointers: i32,
}

type WriteDump = unsafe extern "system" fn(HANDLE, u32, HANDLE, i32, *const ExceptionInformation, *const core::ffi::c_void, *const core::ffi::c_void) -> i32;
type Filter = unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32;

struct Writer {
    write: WriteDump,
    dir: PathBuf,
    request: usize,
    done: usize,
}

static WRITER: OnceLock<Writer> = OnceLock::new();
static PREVIOUS: AtomicUsize = AtomicUsize::new(0);
static FIRED: AtomicBool = AtomicBool::new(false);
static POINTERS: AtomicPtr<EXCEPTION_POINTERS> = AtomicPtr::new(std::ptr::null_mut());
static THREAD: AtomicU32 = AtomicU32::new(0);

/// Installs the filter once per process. A host that cannot (no dbghelp, no thread) runs as
/// before and says why in its log.
pub(crate) fn install() {
    if WRITER.get().is_some() { return; }
    let dbghelp: Vec<u16> = "dbghelp.dll\0".encode_utf16().collect();
    // SAFETY: a NUL-terminated name, loaded from System32 only; the module stays loaded.
    let module = unsafe { LoadLibraryExW(dbghelp.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() { return crate::log!("note: crash dumps unavailable (dbghelp.dll did not load)"); }
    // SAFETY: a live module and a NUL-terminated export name; the export has this signature.
    let Some(write) = (unsafe { GetProcAddress(module, c"MiniDumpWriteDump".as_ptr().cast()) }) else {
        return crate::log!("note: crash dumps unavailable (no MiniDumpWriteDump)");
    };
    // SAFETY: two auto-reset events, owned for the process's life.
    let (request, done) = unsafe { (CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()), CreateEventW(std::ptr::null(), 0, 0, std::ptr::null())) };
    if request.is_null() || done.is_null() { return crate::log!("note: crash dumps unavailable (no events)"); }
    // SAFETY: the export's documented signature.
    let write: WriteDump = unsafe { std::mem::transmute(write) };
    let writer = Writer { write, dir: crate::data_paths::crash_dumps(), request: request as usize, done: done as usize };
    if WRITER.set(writer).is_err() { return; }
    if std::thread::Builder::new().name("crystal-crash-dump".into()).stack_size(256 * 1024).spawn(serve).is_err() {
        return crate::log!("note: crash dumps unavailable (no writer thread)");
    }
    // SAFETY: installs a filter that only touches statics and the events above.
    let previous = unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    PREVIOUS.store(previous.map_or(0, |f| f as usize), Ordering::Release);
}

/// The crashing thread: record, wake the writer, wait, hand on.
unsafe extern "system" fn filter(pointers: *const EXCEPTION_POINTERS) -> i32 {
    if let (false, Some(writer)) = (FIRED.swap(true, Ordering::AcqRel), WRITER.get()) {
        POINTERS.store(pointers.cast_mut(), Ordering::Release);
        THREAD.store(GetCurrentThreadId(), Ordering::Release);
        SetEvent(writer.request as HANDLE);
        WaitForSingleObject(writer.done as HANDLE, WRITE_WAIT_MS);
    }
    match PREVIOUS.load(Ordering::Acquire) {
        0 => 0, // EXCEPTION_CONTINUE_SEARCH: Windows' own handling follows.
        previous => {
            let previous: Filter = std::mem::transmute(previous);
            previous(pointers)
        }
    }
}

/// The writer thread: waits for a crash, writes its dump, keeps the newest `KEEP`.
fn serve() {
    let Some(writer) = WRITER.get() else { return };
    // SAFETY: an event this module owns.
    unsafe { WaitForSingleObject(writer.request as HANDLE, u32::MAX) };
    let written = write_dump(writer);
    // SAFETY: as above; the crashing thread continues once this is set.
    unsafe { SetEvent(writer.done as HANDLE) };
    match written {
        Some(path) => crate::log!("error: the game host crashed; crash dump {}", path.display()),
        None => crate::log!("error: the game host crashed; the crash dump could not be written"),
    }
}

fn write_dump(writer: &Writer) -> Option<PathBuf> {
    std::fs::create_dir_all(&writer.dir).ok()?;
    let path = writer.dir.join(file_name(local_stamp(), std::process::id()));
    let wide: Vec<u16> = path.as_os_str().encode_wide_nul();
    // SAFETY: a NUL-terminated path; the handle is closed below.
    let file = unsafe { CreateFileW(wide.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut()) };
    if file == INVALID_HANDLE_VALUE { return None; }
    let info = ExceptionInformation { thread_id: THREAD.load(Ordering::Acquire), pointers: POINTERS.load(Ordering::Acquire), client_pointers: 0 };
    // SAFETY: this process, an open file, and exception pointers the crashing thread keeps alive while it waits.
    let ok = unsafe {
        let ok = (writer.write)(GetCurrentProcess(), std::process::id(), file, DUMP_TYPE, &info, std::ptr::null(), std::ptr::null());
        CloseHandle(file);
        ok != 0
    };
    if !ok { let _ = std::fs::remove_file(&path); return None; }
    prune(&writer.dir, KEEP);
    Some(path)
}

/// `2026-11-17 19-54-01 pol 1234.dmp`.
pub(crate) fn file_name((y, mo, d, h, mi, s): (u16, u16, u16, u16, u16, u16), pid: u32) -> String {
    format!("{y:04}-{mo:02}-{d:02} {h:02}-{mi:02}-{s:02} {} {pid}.dmp", crate::data_paths::DUMP_TAG)
}

fn local_stamp() -> (u16, u16, u16, u16, u16, u16) {
    // SAFETY: GetLocalTime only writes the SYSTEMTIME it is given.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        t
    };
    let t: windows_sys::Win32::Foundation::SYSTEMTIME = t;
    (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

/// Keeps the newest `keep` `.dmp` files in `dir`.
pub(crate) fn prune(dir: &Path, keep: usize) {
    let mut dumps: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "dmp"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    dumps.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in dumps.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

trait WideNul { fn encode_wide_nul(&self) -> Vec<u16>; }
impl WideNul for std::ffi::OsStr {
    fn encode_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The child half of `a_real_crash_writes_one_dump`: install, then crash for real.
    #[test]
    #[ignore = "child process of a_real_crash_writes_one_dump"]
    fn crash_child() {
        if std::env::var_os("CRYSTAL_CRASH_CHILD").is_none() { return; }
        // No Windows error box for this deliberate crash.
        unsafe { windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode(0x0002) };
        install();
        // SAFETY: none; this test process is meant to die here with an access violation.
        unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u32>(), 1) };
    }

    #[test]
    fn a_real_crash_writes_one_dump() {
        let root = std::env::temp_dir().join(format!("crystal-crash-root-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&root);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_dump::tests::crash_child", "--ignored", "--nocapture", "--test-threads=1"])
            .env("CRYSTAL_CRASH_CHILD", "1").env(crate::data_paths::ROOT_ENV, &root)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().unwrap();
        assert_eq!(status.code().map(|c| c as u32), Some(0xC000_0005), "the child died of its access violation");
        let dumps: Vec<_> = std::fs::read_dir(root.join("Logs").join("crashdumps")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(dumps.len(), 1, "{dumps:?}");
        let name = dumps[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with(".dmp") && name.contains(" pol "), "{name}");
        let bytes = std::fs::read(&dumps[0]).unwrap();
        assert_eq!(&bytes[..4], b"MDMP", "a minidump");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dumps_are_named_by_time_and_host_pid_and_only_three_stay() {
        assert_eq!(file_name((2026, 11, 17, 19, 54, 1), 1234), "2026-11-17 19-54-01 pol 1234.dmp");
        let dir = std::env::temp_dir().join(format!("crystal-crash-dumps-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..5u64 {
            let path = dir.join(format!("dump {n}.dmp"));
            std::fs::write(&path, b"x").unwrap();
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(100 - n);
            std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(when).unwrap();
        }
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        prune(&dir, KEEP);
        let mut left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["dump 2.dmp", "dump 3.dmp", "dump 4.dmp", "notes.txt"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
