//! Why a game ended, as its game host's process reports it: the exit status Windows kept, a
//! lost connection the host reported first, and the crash dump it wrote. Recorded in the
//! game's log and the history line, and the evidence that tells a deliberate quit from a
//! failure when the pipe closed without an exit line (`launch_log::classify_host_end`).

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a session end waits for its host process to finish exiting.
pub(crate) const EXIT_WAIT: Duration = Duration::from_secs(5);

/// A verified game host, opened at its hello so its exit status outlives its pipe.
pub(crate) struct Host {
    pid: u32,
    process: OwnedHandle,
}

impl Host {
    /// `None` when it cannot be opened, or is this process (a test's stand-in host).
    pub(crate) fn open(pid: u32) -> Option<Self> {
        use crate::win::process::{open, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
        if pid == std::process::id() { return None; }
        open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE).ok().map(|process| Self { pid, process })
    }
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }
    /// The exit status once the process has ended within `wait`; `None` while it still runs.
    pub(crate) fn exit_code(&self, wait: Duration) -> Option<u32> {
        use windows_sys::Win32::{Foundation::WAIT_OBJECT_0, System::Threading::{GetExitCodeProcess, WaitForSingleObject}};
        let raw = self.process.as_raw_handle() as _;
        let millis = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: a live process handle with SYNCHRONIZE and a limited-query right.
        unsafe {
            if WaitForSingleObject(raw, millis) != WAIT_OBJECT_0 { return None; }
            let mut code = 0u32;
            (GetExitCodeProcess(raw, &mut code) != 0).then_some(code)
        }
    }
}

/// What the end's reason is built from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct End {
    /// The process exit status; `None` when it could not be read or the host still ran.
    pub code: Option<u32>,
    /// `game_disconnected`: (network state, POL result).
    pub disconnected: Option<(u32, i32)>,
    /// The host's crash dump for this session.
    pub dump: Option<PathBuf>,
}

impl End {
    /// `closed (exit 0)`, `crashed (0xC0000005 access violation)`, `lost connection (network
    /// state 0x15, POL result -2059)`, `ended (exit status unknown)`; the dump's name after it.
    pub(crate) fn reason(&self) -> String {
        let mut text = match (self.disconnected, self.code) {
            (Some((state, code)), _) => format!("lost connection (network state {state:#x}, POL result {code})"),
            (None, Some(code)) if crashed(code) => match status_name(code) {
                Some(name) => format!("crashed (0x{code:08X} {name})"),
                None => format!("crashed (0x{code:08X})"),
            },
            (None, Some(code)) => format!("closed (exit {})", code as i32),
            (None, None) => "ended (exit status unknown)".to_owned(),
        };
        if let Some(name) = self.dump.as_deref().and_then(Path::file_name) {
            text.push_str(&format!("; crash dump {}", name.to_string_lossy()));
        }
        text
    }
}

/// An NTSTATUS warning or error (0x8xxxxxxx, 0xCxxxxxxx, and 0xE06D7363 for C++): the process
/// ended by an exception. Small negative codes (FFXI's -14, polcore's -1025) are its own exits.
pub(crate) fn crashed(code: u32) -> bool {
    (0x8000_0000..0xFFFF_0000).contains(&code)
}

/// The exit status a host left when its pipe closed without an exit line, and what it says.
/// Read from the binaries installed here (2026-11-17), never run:
/// - Windower 4 `//terminate` and `//game_terminate` (Hook.dll 0x1004513F):
///   `TerminateProcess(GetCurrentProcess(), 0)`; with an argument, that argument (`std::stoi`).
/// - Ashita v4 `/terminate` and `/exit` (Ashita.dll 0x100ED524), and its close prompt's
///   terminate (0x1013DE7F): `TerminateProcess(GetCurrentProcess(), 0)`.
/// - Task Manager's End task (Taskmgr.exe 0x140191E36): `TerminateProcess(process, 1)`;
///   `taskkill /F` measured 1, .NET `Process.Kill` measured -1.
/// - A crash: the exception code (0xC0000005...). Crystal's dump filter hands the exception on,
///   so Windows ends the process with it; Windower's own fatal path is `int3` (0x80000003)
///   then `TerminateProcess(..., -1)` (Hook.dll 0x10062AC0).
///
/// So only 0 is a deliberate quit; 1, -1 and anything else unexplained is offered, never
/// relaunched by itself, and an exception status is a crash.
pub(crate) const DELIBERATE_EXIT: u32 = 0;

/// The common crash codes' names.
fn status_name(code: u32) -> Option<&'static str> {
    Some(match code {
        0x8000_0003 => "breakpoint",
        0xC000_0005 => "access violation",
        0xC000_001D => "illegal instruction",
        0xC000_0094 => "integer divide by zero",
        0xC000_00FD => "stack overflow",
        0xC000_0135 => "DLL not found",
        0xC000_0142 => "DLL initialization failed",
        0xC000_0374 => "heap corruption",
        0xC000_0409 => "stack buffer overrun",
        0xC000_0602 => "fail fast",
        0xE06D_7363 => "C++ exception",
        _ => return None,
    })
}

/// The host's crash dump in `dir`: the one whose name ends with ` pol <pid>.dmp`.
pub(crate) fn find_dump(dir: &Path, pid: u32) -> Option<PathBuf> {
    let suffix = format!(" {} {pid}.dmp", crate::storage::names::DUMP_TAG);
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path())
        .find(|path| path.file_name().is_some_and(|name| name.to_string_lossy().ends_with(&suffix)))
}

/// The end of a host that has closed its pipe: its exit status (waiting `wait` for it) and the
/// dump it wrote, if any: a filter after Crystal's may end a crashed process with any status.
pub(crate) fn observe(host: Option<&Host>, disconnected: Option<(u32, i32)>, wait: Duration) -> End {
    let code = host.and_then(|host| host.exit_code(wait));
    let dump = match (host, code) {
        (Some(host), Some(_)) => find_dump(&crate::storage::Layout::current().crash_dumps(), host.pid()),
        _ => None,
    };
    End { code, disconnected, dump }
}

impl End {
    /// What classification reads from it (`launch_log::classify_host_end`).
    pub(crate) fn evidence(&self) -> crate::launch_log::HostExit {
        crate::launch_log::HostExit { status: self.code, dump: self.dump.is_some(), disconnected: self.disconnected.is_some() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_end_says_why_in_plain_words() {
        let end = |code, disconnected, dump: Option<&str>| End { code, disconnected, dump: dump.map(PathBuf::from) }.reason();
        assert_eq!(end(Some(0), None, None), "closed (exit 0)");
        assert_eq!(end(Some(0xFFFF_FFF2), None, None), "closed (exit -14)");
        assert_eq!(end(Some(0xC000_0005), None, None), "crashed (0xC0000005 access violation)");
        assert_eq!(end(Some(0xC000_0409), None, None), "crashed (0xC0000409 stack buffer overrun)");
        assert_eq!(end(Some(0xC0AB_0001), None, None), "crashed (0xC0AB0001)");
        assert_eq!(end(Some(0), Some((0x15, -2059)), None), "lost connection (network state 0x15, POL result -2059)");
        assert_eq!(end(None, None, None), "ended (exit status unknown)");
        assert_eq!(end(Some(0xC000_0005), None, Some(r"C:\L\crashdumps\2026-11-17 19-54-01 pol 77.dmp")),
            "crashed (0xC0000005 access violation); crash dump 2026-11-17 19-54-01 pol 77.dmp");
    }

    #[test]
    fn a_hosts_dump_is_found_by_its_pid() {
        let dir = std::env::temp_dir().join(format!("crystal-host-dump-{}", crate::backend::unique_id().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-11-17 19-54-01 pol 77.dmp"), b"").unwrap();
        std::fs::write(dir.join("2026-11-17 19-54-01 pol 777.dmp"), b"").unwrap();
        assert_eq!(find_dump(&dir, 77), Some(dir.join("2026-11-17 19-54-01 pol 77.dmp")));
        assert_eq!(find_dump(&dir, 7), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_finished_process_reports_its_exit_status() {
        let mut child = std::process::Command::new("cmd").args(["/C", "exit 3"]).spawn().unwrap();
        let host = Host::open(child.id()).unwrap();
        child.wait().unwrap();
        assert_eq!(host.exit_code(Duration::from_secs(5)), Some(3));
        assert_eq!(observe(Some(&host), None, Duration::from_secs(5)).reason(), "closed (exit 3)");
        let running = std::process::Command::new("cmd").args(["/C", "ping -n 3 127.0.0.1 >NUL"]).spawn().unwrap();
        let host = Host::open(running.id()).unwrap();
        assert_eq!(host.exit_code(Duration::ZERO), None, "a running host has no exit status yet");
        drop(running); // ends by itself within two seconds
        assert!(Host::open(std::process::id()).is_none(), "the launcher is never its own host");
    }
}
