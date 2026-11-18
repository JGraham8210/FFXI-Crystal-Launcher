//! The pad hotkeys' reader (`features::padchord`): every connected controller, read only while a
//! Crystal game window is in front.
//!
//! - XInput slots 0-3 through `XInputGetStateEx` (xinput1_4's ordinal 100, which also reports
//!   Guide), or `XInputGetState` where that is missing. Connected slots are read every pass;
//!   empty ones are asked again every 2 s (a read of an empty slot is the slow one).
//! - Sony pads from their HID collection (`hid::HidPads`, an instance of its own, read-only),
//!   their list refreshed every 2 s.
//! - One pass every 8 ms while armed. Parked (no game in front, driven by the desktop thread's
//!   foreground hook, which arms the keyboard hook too) the thread waits on a condition
//!   variable and reads nothing.
//! - A binding that fires is posted to the desktop window (`WM_PAD_HOTKEY`), whose handler
//!   switches the window as a keyboard hotkey's does.
//!
//! A controller field's recording uses the same reader on a thread of its own (`Recording`).

use super::detect::hid::HidPads;
use super::detect::Btn;
use crate::features::padchord::{xinput_bit, Matcher, PadChord, Recorded, Recorder};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Posted to the desktop window: the binding's id in `wParam`.
pub(crate) const WM_PAD_HOTKEY: u32 = 0x8000 + 0x43; // WM_APP + 0x43

const PASS: Duration = Duration::from_millis(8);
const REPROBE: Duration = Duration::from_secs(2);

#[repr(C)]
#[derive(Default)]
struct XInputState {
    packet: u32,
    buttons: u16,
    left_trigger: u8,
    right_trigger: u8,
    sticks: [i16; 4],
}

type GetState = unsafe extern "system" fn(u32, *mut XInputState) -> u32;

/// XInput's state of one slot as `Btn::bit`s (triggers pressed at 128 of 255); None when the
/// slot has no controller.
fn read_slot(get: GetState, slot: u32) -> Option<u32> {
    let mut state = XInputState::default();
    // SAFETY: XInput's own signature, with a state of its size.
    if unsafe { get(slot, &mut state) } != 0 { return None; }
    let mut bits = Btn::ALL.into_iter().filter(|b| !matches!(b, Btn::LT | Btn::RT))
        .filter(|b| xinput_bit(*b).is_some_and(|x| u32::from(state.buttons) & x != 0)).fold(0, |all, b| all | b.bit());
    if state.left_trigger >= 128 { bits |= Btn::LT.bit(); }
    if state.right_trigger >= 128 { bits |= Btn::RT.bit(); }
    Some(bits)
}

/// The best XInput state read on this PC: (the function, it reports Guide).
fn load_xinput() -> Option<(GetState, bool)> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    for dll in ["xinput1_4.dll", "xinput1_3.dll", "xinput9_1_0.dll"] {
        // SAFETY: a system DLL by name from System32; kept loaded for the process.
        let module = unsafe { LoadLibraryExW(crate::win::wide(dll).as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if module.is_null() { continue; }
        // SAFETY: lookups in that module; ordinal 100 is XInputGetStateEx, same signature.
        if let Some(f) = unsafe { GetProcAddress(module, 100 as *const u8) } {
            return Some((unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, GetState>(f) }, true));
        }
        if let Some(f) = unsafe { GetProcAddress(module, c"XInputGetState".as_ptr().cast()) } {
            return Some((unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, GetState>(f) }, false));
        }
    }
    None
}

/// Every connected controller's buttons, keyed by XInput slot (0-3) or HID pad id.
pub(crate) struct Reader {
    xinput: Option<GetState>,
    connected: [bool; 4],
    probed: Option<Instant>,
    hid: HidPads,
    refreshed: Option<Instant>,
}

impl Reader {
    pub(crate) fn new() -> Self {
        Self { xinput: load_xinput().map(|(get, _)| get), connected: [false; 4], probed: None, hid: HidPads::default(), refreshed: None }
    }

    pub(crate) fn read(&mut self) -> Vec<(u64, u32)> {
        let now = Instant::now();
        let mut pads = Vec::new();
        if let Some(get) = self.xinput {
            let probe = self.probed.is_none_or(|at| now.duration_since(at) >= REPROBE);
            if probe { self.probed = Some(now); }
            for slot in 0..4u32 {
                if !self.connected[slot as usize] && !probe { continue; }
                let state = read_slot(get, slot);
                self.connected[slot as usize] = state.is_some();
                if let Some(bits) = state { pads.push((u64::from(slot), bits)); }
            }
        }
        if self.refreshed.is_none_or(|at| now.duration_since(at) >= REPROBE) {
            self.hid.refresh();
            self.refreshed = Some(now);
        }
        self.hid.poll();
        pads.extend(self.hid.buttons().map(|(id, bits)| (id as u64, bits)));
        pads
    }
}

struct Control {
    bindings: Vec<(i32, PadChord)>,
    /// Moves with each `set_bindings`, so the thread takes them once.
    generation: u64,
    armed: bool,
    stop: bool,
}

struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
}

/// The reader's thread. Dropping it ends the thread.
pub(crate) struct PadSwitch {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PadSwitch {
    /// Start the thread, parked. `window` receives `WM_PAD_HOTKEY`.
    pub(crate) fn start(window: isize, bindings: Vec<(i32, PadChord)>) -> Result<Self, String> {
        let shared = Arc::new(Shared { control: Mutex::new(Control { bindings, generation: 1, armed: false, stop: false }), wake: Condvar::new() });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new().name("features-pads".into()).stack_size(256 * 1024)
            .spawn(move || run(&thread_shared, window)).map_err(|e| e.to_string())?;
        Ok(Self { shared, thread: Some(thread) })
    }

    fn control(&self) -> std::sync::MutexGuard<'_, Control> { self.shared.control.lock().unwrap_or_else(|e| e.into_inner()) }

    pub(crate) fn set_bindings(&self, bindings: Vec<(i32, PadChord)>) {
        let mut control = self.control();
        control.bindings = bindings;
        control.generation += 1;
        self.shared.wake.notify_all();
    }

    /// Read (a game window is in front) or park.
    pub(crate) fn arm(&self, on: bool) {
        self.control().armed = on;
        self.shared.wake.notify_all();
    }
}

impl Drop for PadSwitch {
    fn drop(&mut self) {
        self.control().stop = true;
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

fn run(shared: &Shared, window: isize) {
    let mut reader: Option<Reader> = None;
    let mut matcher = Matcher::default();
    let mut generation = 0;
    let mut reading = false;
    loop {
        {
            let mut control = shared.control.lock().unwrap_or_else(|e| e.into_inner());
            while !control.stop && !(control.armed && !control.bindings.is_empty()) {
                reading = false;
                control = shared.wake.wait(control).unwrap_or_else(|e| e.into_inner());
            }
            if control.stop { return; }
            if control.generation != generation {
                generation = control.generation;
                matcher.set_bindings(control.bindings.clone());
            }
        }
        let pads = reader.get_or_insert_with(Reader::new).read();
        if reading {
            for id in matcher.step(&pads) {
                // SAFETY: posting to the desktop thread's window; fails harmlessly once it is gone.
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(window as _, WM_PAD_HOTKEY, id as usize, 0) };
            }
        } else {
            // Armed just now: what is held belongs to whatever had it.
            matcher.arm(&pads);
            reading = true;
        }
        let control = shared.control.lock().unwrap_or_else(|e| e.into_inner());
        if !control.stop { let _ = shared.wake.wait_timeout(control, PASS); }
    }
}

/// Recordings running now: the launcher's controller navigation stands aside meanwhile.
static RECORDINGS: AtomicUsize = AtomicUsize::new(0);

/// Whether a controller field records now.
pub(crate) fn recording() -> bool { RECORDINGS.load(Ordering::Relaxed) != 0 }

/// A controller field's recording: the buttons held as they change, then the chord. Ends with
/// its chord or when dropped.
pub(crate) struct Recording {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recording {
    pub(crate) fn start(sink: impl Fn(Recorded) + Send + 'static) -> Result<Self, String> {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        RECORDINGS.fetch_add(1, Ordering::Relaxed);
        let thread = std::thread::Builder::new().name("features-pad-record".into()).stack_size(256 * 1024).spawn(move || {
            let mut reader = Reader::new();
            let all = |pads: &[(u64, u32)]| pads.iter().fold(0, |all, (_, bits)| all | bits);
            let mut recorder = Recorder::start(all(&reader.read()));
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(PASS);
                match recorder.step(all(&reader.read())) {
                    Some(done @ Recorded::Done(_)) => { sink(done); return; }
                    Some(held) => sink(held),
                    None => {}
                }
            }
        });
        match thread {
            Ok(thread) => Ok(Self { stop, thread: Some(thread) }),
            Err(error) => { RECORDINGS.fetch_sub(1, Ordering::Relaxed); Err(error.to_string()) }
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            if thread.thread().id() != std::thread::current().id() { let _ = thread.join(); }
        }
        RECORDINGS.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// XInputGetStateEx as a fake: slot 0 has Guide, A and LT at 200, RT at 127; slot 1 is empty.
    unsafe extern "system" fn fake(slot: u32, state: *mut XInputState) -> u32 {
        if slot != 0 { return 1167; } // ERROR_DEVICE_NOT_CONNECTED
        let state = unsafe { &mut *state };
        (state.buttons, state.left_trigger, state.right_trigger) = (0x0400 | 0x1000, 200, 127);
        0
    }

    #[test]
    fn the_guide_read_path_gives_guide_and_counts_triggers_from_128() {
        assert_eq!(read_slot(fake, 0), Some(Btn::Guide.bit() | Btn::South.bit() | Btn::LT.bit()));
        assert_eq!(read_slot(fake, 1), None);
        let mut reader = Reader { xinput: Some(fake), connected: [false; 4], probed: None, hid: HidPads::default(), refreshed: Some(Instant::now()) };
        let first = reader.read();
        assert_eq!(first.first(), Some(&(0, Btn::Guide.bit() | Btn::South.bit() | Btn::LT.bit())));
        assert_eq!(reader.connected, [true, false, false, false], "empty slots wait for the next probe");
    }

    #[test]
    fn this_pcs_xinput_loads() {
        // Every supported Windows has xinput1_4 with its ordinal 100.
        assert!(matches!(load_xinput(), Some((_, true))));
    }

    #[test]
    fn the_thread_parks_reads_and_ends() {
        let pads = PadSwitch::start(0, vec![(1000, PadChord::single(Btn::Guide))]).unwrap();
        pads.arm(true);
        std::thread::sleep(Duration::from_millis(30));
        pads.set_bindings(vec![]);
        pads.arm(false);
        drop(pads);
        assert!(!recording());
        let recording = Recording::start(|_| {}).unwrap();
        assert!(super::recording());
        drop(recording);
        assert!(!super::recording());
    }
}
