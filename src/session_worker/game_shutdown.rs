//! One-shot, session-bound game shutdown. The graceful path asks the game: in the world its
//! own /shutdown and countdown, on a fatal-error dialog its OK. Where the game cannot close
//! itself (title screen, lobby, a dialog that did not end it, no answer) it falls back to the
//! instant path. The instant path ("Close games instantly") ends only this session's own game
//! host, by the exact process identity its worker recorded, like Windower's //terminate. An
//! in-world character is never ended by itself: its row offers Close now instead.
use super::*;
use std::os::windows::io::{AsRawHandle, OwnedHandle};

/// What Crystal's shut-down action does (Settings → System → "Close games instantly").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The game's own /shutdown command and logout countdown.
    Graceful,
    /// End the game host at once. Its PlayOnline session ends with it.
    Instant,
}
impl Mode {
    pub(crate) fn from_setting(instant: bool) -> Self {
        if instant { Self::Instant } else { Self::Graceful }
    }
    pub(crate) fn run(self, session: &BackgroundSession) -> Result<(), Refusal> {
        match self {
            Self::Graceful => request(session),
            Self::Instant => terminate(session).map_err(Refusal::from),
        }
    }
}

/// Why a shut-down did not end the game. `close_now`: the game accepted /shutdown in the world
/// but is still open after CLOSE_LIMIT; only the user's Close now ends it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub message: String,
    pub close_now: bool,
}
impl From<String> for Refusal {
    fn from(message: String) -> Self { Self { message, close_now: false } }
}
impl From<&str> for Refusal {
    fn from(message: &str) -> Self { message.to_owned().into() }
}

/// Which branch a current loader says answered a request (`path` / `reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Answer {
    /// `/shutdown` ran in the world.
    World,
    /// OK was pressed on a fatal-error dialog.
    Dialog,
    /// Refused: no character in the world and no fatal dialog to answer.
    NotInWorld,
}
impl Answer {
    /// `None` from an older loader, or for a name this build does not know.
    pub(crate) fn parse(ok: bool, path: Option<&str>, reason: Option<&str>) -> Option<Self> {
        match (ok, path, reason) {
            (true, Some("world"), _) => Some(Self::World),
            (true, Some("dialog"), _) => Some(Self::Dialog),
            (false, _, Some("not_in_world")) => Some(Self::NotInWorld),
            _ => None,
        }
    }
}

/// Written just before the instant path ends `host`: the worker reads it when its loader
/// pipe closes, so the end is a normal close and not a crash (`terminated`).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Terminate {
    worker: Identity,
    host: Identity,
}
const TERMINATE_FILE: &str = "game-terminate.json";
const HOST_EXIT_LIMIT: Duration = Duration::from_secs(5);
const WORKER_EXIT_LIMIT: Duration = Duration::from_secs(15);

/// Crystal's own game host for `worker`: the loader image (`pol.exe`), started after it.
fn crystal_host(process: &OwnedHandle, host: Identity, worker: Identity) -> bool {
    host.created > worker.created
        && crate::win::process::image_path_of(process).ok().is_some_and(|path| {
            path.file_name().is_some_and(|name| name.eq_ignore_ascii_case("pol.exe"))
        })
}

pub(crate) fn terminate(session: &BackgroundSession) -> Result<(), String> {
    terminate_within(session, &crystal_host, HOST_EXIT_LIMIT, WORKER_EXIT_LIMIT)
}

fn terminate_within(
    session: &BackgroundSession,
    owned: &dyn Fn(&OwnedHandle, Identity, Identity) -> bool,
    host_limit: Duration,
    worker_limit: Duration,
) -> Result<(), String> {
    let directory = session.path.parent().ok_or(t("This game session has no account directory."))?;
    let _delivery = exclusive(&directory.join("game-shutdown-delivery.lock"))
        .map_err(|_| t("Shutdown is already being requested for this account."))?;
    terminate_held(session, owned, host_limit, worker_limit)
}

/// The instant path's work, with the account's delivery lease already held (by
/// `terminate_within`, or by the graceful path falling back to it).
fn terminate_held(
    session: &BackgroundSession,
    owned: &dyn Fn(&OwnedHandle, Identity, Identity) -> bool,
    host_limit: Duration,
    worker_limit: Duration,
) -> Result<(), String> {
    use crate::win::process::{created, open, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
    use windows_sys::Win32::System::Threading::{TerminateProcess, PROCESS_TERMINATE};
    let refused = || t("Could not close FFXI.").to_owned();
    // Refuses a parked or idle host with no game.
    current(session)?;
    let record = read_record(&session.path)
        .filter(|record| record.worker == session.worker)
        .ok_or(t("The selected game session ended before shutdown could be requested."))?;
    let Event::Started(started) = record.state else {
        return Err(t("Wait for FFXI to finish starting before requesting shutdown.").into());
    };
    let directory = session.path.parent().ok_or(t("This game session has no account directory."))?;
    // One handle is checked and terminated: a reused PID can never be the one ended.
    let process = open(started.pid, PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)
        .map_err(|_| refused())?;
    let host = Identity { pid: started.pid, created: created(&process).ok_or_else(refused)? };
    if !owned(&process, host, session.worker) {
        return Err(refused());
    }
    let host_watch = ProcessWatch(process);
    if host_watch.ended()? {
        return Ok(());
    }
    let worker_watch = ProcessWatch::open(session.worker)?;
    let marker = directory.join(TERMINATE_FILE);
    save_json(&marker, &Terminate { worker: session.worker, host })?;
    let forget = || { let _ = fs::remove_file(&marker); };
    if let Err(error) = current(session) {
        forget();
        return Err(error);
    }
    if unsafe { TerminateProcess(host_watch.0.as_raw_handle() as _, 0) } == 0 && !host_watch.ended()? {
        forget();
        return Err(refused());
    }
    // The marker stays for the worker: it is bound to this worker and this host only.
    let until = Instant::now() + host_limit;
    while !host_watch.ended()? {
        if Instant::now() >= until { return Err(refused()); }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The worker records the normal end and closes; the window's monitor shows it either way.
    let until = Instant::now() + worker_limit;
    while !worker_watch.ended()? && session.active() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

/// In the worker: whether this session's game host was ended by the instant path. True at
/// most once, and only for this worker and the host its record names.
pub(crate) fn terminated(directory: &Path) -> bool {
    let path = directory.join(TERMINATE_FILE);
    let Some(marker) = read_bounded(&path, FRAME_LIMIT).and_then(|bytes| serde_json::from_slice::<Terminate>(&bytes).ok())
    else { return false };
    let Some(worker) = identity(std::process::id()) else { return false };
    let host = read_record(&directory.join("session.json"))
        .filter(|record| record.worker == worker)
        .and_then(|record| match record.state { Event::Started(started) => Some(started.pid), _ => None });
    let ours = marker.worker == worker && host == Some(marker.host.pid);
    if ours {
        let _ = fs::remove_file(&path);
    }
    ours
}

const FRAME_LIMIT: usize = 2048;
/// How long /shutdown in the world may take (the game's own countdown, then POL).
const CLOSE_LIMIT: Duration = Duration::from_secs(90);
/// How long a game whose fatal dialog got its OK may take to close before its host is ended.
const DIALOG_CLOSE_LIMIT: Duration = Duration::from_secs(10);
/// A reply in flight at the request's deadline still counts (loader → worker → file): the
/// loader never runs /shutdown after the deadline, so no answer by then plus this means none.
const REPLY_GRACE: Duration = Duration::from_secs(2);
/// An older loader's refusal outside the world, before replies named their branch.
const OLD_NOT_IN_WORLD: &str = "Log into a character before requesting shutdown from Crystal.";

/// The graceful path's timings; the tests shorten them.
#[derive(Clone, Copy)]
struct Limits { ready: Duration, dispatch: Duration, close: Duration, dialog: Duration, grace: Duration }
const LIMITS: Limits = Limits { ready: ONE_SHOT_READY_LIMIT, dispatch: ONE_SHOT_DISPATCH_LIMIT, close: CLOSE_LIMIT,
    dialog: DIALOG_CLOSE_LIMIT, grace: REPLY_GRACE };

/// What a request's reply means for the ladder.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// Wait this long for the game to close; then end the host (`true`) or offer Close now.
    Wait(Duration, bool),
    /// End the host now: the game cannot close itself on this screen.
    End,
    /// Stop with this message; the game stays as it is.
    Fail(String),
}
fn next(ok: bool, answer: Option<Answer>, message: String, limits: Limits) -> Next {
    match (ok, answer) {
        (true, Some(Answer::Dialog)) => Next::Wait(limits.dialog, true),
        // In the world, or an older loader's acceptance: never ended by itself.
        (true, _) => Next::Wait(limits.close, false),
        (false, Some(Answer::NotInWorld)) => Next::End,
        (false, None) if message == OLD_NOT_IN_WORLD => Next::End,
        (false, _) if message.is_empty() => Next::Fail(t("FFXI could not accept the shutdown request.").into()),
        (false, _) => Next::Fail(message),
    }
}
const UNSUPPORTED: &str = "Background shutdown is unavailable for this session. Close FFXI and launch it again with the current Crystal build.";

struct ProcessWatch(std::os::windows::io::OwnedHandle);
impl ProcessWatch {
    fn open(expected: Identity) -> Result<Self, String> {
        // Deliberately no running check: a process that already exited is watched as ended.
        let (handle, identity) = open_identity(expected.pid)
            .ok_or_else(|| t("Could not watch the selected game's shutdown.").to_owned())?;
        if identity != expected {
            return Err(t("The selected game session ended before shutdown could be requested.").into());
        }
        Ok(Self(handle))
    }
    fn ended(&self) -> Result<bool, String> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::WaitForSingleObject,
        };
        match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(t("Could not confirm that FFXI and PlayOnline closed.").into()),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    worker: Identity,
    request_id: u64,
    game_id: u64,
    deadline_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    worker: Identity,
    request_id: u64,
    game_id: u64,
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer: Option<Answer>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ended {
    worker: Identity,
    game_id: u64,
}

/// Published only from the verified loader after GameStart and its callbacks
/// have returned. The native POL owner may remain signed in for another game.
pub(crate) fn ended(directory: &Path, game_id: u64) -> Result<(), String> {
    let session = owned_session(directory).ok_or("The game belongs to an inactive session.")?;
    if game_id == 0 { return Err("The ended game has no identity.".into()); }
    save_json(&directory.join("game-shutdown-ended.json"), &Ended { worker: session.worker, game_id })
}

fn game_ended(directory: &Path, worker: Identity, game_id: u64) -> bool {
    read_bounded(&directory.join("game-shutdown-ended.json"), FRAME_LIMIT)
        .and_then(|bytes| serde_json::from_slice::<Ended>(&bytes).ok())
        .is_some_and(|ended| ended.worker == worker && ended.game_id >= game_id)
}

fn reply_path(directory: &Path, id: u64) -> PathBuf {
    one_shot_reply_path(directory, "game-shutdown", id)
}
fn reply(directory: &Path, worker: Identity, id: u64) -> Option<Reply> {
    let reply: Reply =
        serde_json::from_slice(&read_bounded(&reply_path(directory, id), FRAME_LIMIT)?).ok()?;
    (reply.worker == worker
        && reply.request_id == id
        && reply.game_id != 0
        && one_shot_message_valid(&reply.message))
    .then_some(reply)
}
fn current(session: &BackgroundSession) -> Result<(), String> {
    if !session.active() {
        return Err(
            t("The selected game session ended before shutdown could be requested.").into(),
        );
    }
    if session.game_running() != Some(true) {
        return Err(t("This account has no running FFXI client.").into());
    }
    Ok(())
}
struct Pending {
    directory: PathBuf,
    worker: Identity,
    id: u64,
}
impl Drop for Pending {
    fn drop(&mut self) {
        let Some(_lock) = command_lock(&self.directory.join("game-shutdown.lock")) else {
            return;
        };
        let path = self.directory.join("game-shutdown.json");
        let owned = read_bounded(&path, FRAME_LIMIT)
            .and_then(|bytes| serde_json::from_slice::<Request>(&bytes).ok())
            .is_some_and(|request| request.worker == self.worker && request.request_id == self.id);
        if owned {
            let _ = fs::remove_file(path);
        }
        if reply(&self.directory, self.worker, self.id).is_some() {
            let _ = fs::remove_file(reply_path(&self.directory, self.id));
        }
    }
}

pub(crate) fn request(session: &BackgroundSession) -> Result<(), Refusal> {
    request_within(session, LIMITS, &|| terminate_held(session, &crystal_host, HOST_EXIT_LIMIT, WORKER_EXIT_LIMIT))
}
/// The graceful request, then the ladder: the game closes itself, or `end` (the instant path,
/// under the lease this holds) ends a host that is not in the world, never one that is.
fn request_within(
    session: &BackgroundSession,
    limits: Limits,
    end: &dyn Fn() -> Result<(), String>,
) -> Result<(), Refusal> {
    current(session)?;
    let record = read_record(&session.path)
        .filter(|record| record.worker == session.worker)
        .ok_or(t(
            "The selected game session ended before shutdown could be requested.",
        ))?;
    let Event::Started(started) = record.state else {
        return Err(t("Wait for FFXI to finish starting before requesting shutdown.").into());
    };
    let host = identity(started.pid).ok_or(t("This account has no running FFXI client."))?;
    let host_watch = ProcessWatch::open(host)?;
    let worker_watch = ProcessWatch::open(session.worker)?;
    let directory = session
        .path
        .parent()
        .ok_or(t("This game session has no account directory."))?;
    // Retain this lease through the logout countdown. Another Crystal window
    // must not repeat /shutdown and accidentally cancel the first countdown.
    let _delivery = exclusive(&directory.join("game-shutdown-delivery.lock"))
        .map_err(|_| t("Shutdown is already being requested for this account."))?;
    let ready_until = Instant::now() + limits.ready;
    let game = loop {
        current(session)?;
        if let Some(ready) = reply(directory, session.worker, 0) {
            if !ready.ok {
                return Err(t(UNSUPPORTED).into());
            }
            break ready.game_id;
        }
        if Instant::now() >= ready_until {
            return Err(t(UNSUPPORTED).into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let id = one_shot_id().ok_or(t("Could not identify the shutdown request."))?;
    let deadline_ms = request_clock_ms()
        .checked_add(limits.dispatch.min(ONE_SHOT_DISPATCH_LIMIT).as_millis() as u64)
        .ok_or(t("Could not time the shutdown request."))?;
    let _pending = Pending {
        directory: directory.into(),
        worker: session.worker,
        id,
    };
    {
        let _lock = exclusive(&directory.join("game-shutdown.lock"))
            .map_err(|_| t("Another shutdown request is being submitted."))?;
        current(session)?;
        let ready = reply(directory, session.worker, 0)
            .filter(|ready| ready.ok && ready.game_id == game)
            .ok_or(t(UNSUPPORTED))?;
        save_json(
            &directory.join("game-shutdown.json"),
            &Request {
                worker: session.worker,
                request_id: id,
                game_id: ready.game_id,
                deadline_ms,
            },
        )?;
    }
    wake::signal(session.worker);
    // Both exact process identities must be gone, or the retained session's game ended. A new
    // game or a reused PID is never touched, and a dispatch ACK is not a completed exit.
    let closed = || -> Result<bool, String> {
        Ok(host_watch.ended()? && worker_watch.ended()? || game_ended(directory, session.worker, game))
    };
    // The instant path; a game that closed meanwhile is done too.
    let end_host = || -> Result<(), Refusal> {
        end().or_else(|error| if closed()? { Ok(()) } else { Err(error.into()) })
    };
    let grace_ms = limits.grace.as_millis() as u64;
    let mut waiting: Option<(Instant, bool)> = None;
    loop {
        if closed()? { return Ok(()); }
        match waiting {
            None => {
                if let Some(result) = reply(directory, session.worker, id).filter(|result| result.game_id == game) {
                    match next(result.ok, result.answer, result.message, limits) {
                        Next::Wait(limit, then_end) => waiting = Some((Instant::now() + limit, then_end)),
                        Next::End => return end_host(),
                        Next::Fail(message) => return Err(message.into()),
                    }
                } else if request_clock_ms() >= deadline_ms.saturating_add(grace_ms) {
                    // No answer: the game is not running its input frames (title, hung, gone).
                    return end_host();
                }
            }
            Some((until, true)) if Instant::now() >= until => return end_host(),
            Some((until, false)) if Instant::now() >= until => return Err(Refusal {
                message: t("FFXI or PlayOnline is still open. The game may have refused or cancelled shutdown; finish it in game, then retry.").into(),
                close_now: true,
            }),
            Some(_) => {}
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub(crate) fn take(directory: &Path) -> Option<Vec<u8>> {
    let path = directory.join("game-shutdown.json");
    if !path.exists() { return None; } // As take_controller_settings.
    let _lock = exclusive(&directory.join("game-shutdown.lock")).ok()?;
    let request: Request = serde_json::from_slice(&read_bounded(&path, FRAME_LIMIT)?).ok()?;
    let session = owned_session(directory)?;
    if request.worker != session.worker {
        return None;
    }
    let valid = request.request_id != 0
        && request.deadline_ms > request_clock_ms()
        && request.deadline_ms.saturating_sub(request_clock_ms()) <= ONE_SHOT_DISPATCH_LIMIT.as_millis() as u64
        && reply(directory, session.worker, 0)
            .is_some_and(|ready| ready.ok && ready.game_id == request.game_id);
    if !valid {
        let _ = fs::remove_file(path);
        return None;
    }
    current(&session).ok()?;
    let mut frame = serde_json::to_vec(
        &serde_json::json!({"type":"game_shutdown", "request_id":request.request_id,
        "game_id":request.game_id,"deadline_ms":request.deadline_ms}),
    )
    .ok()?;
    fs::remove_file(path).ok()?; // A denied delete must never duplicate dispatch.
    current(&session).ok()?;
    frame.push(b'\n');
    Some(frame)
}
pub(crate) fn publish(
    directory: &Path,
    id: u64,
    game: u64,
    ok: bool,
    message: &str,
    answer: Option<Answer>,
) -> Result<(), String> {
    if game == 0 || !one_shot_message_valid(message) {
        return Err("Invalid game shutdown result.".into());
    }
    let session =
        owned_session(directory).ok_or("The shutdown result belongs to an inactive session.")?;
    // A request's reply goes under the command lock, like the requester's cleanup and the
    // controller channel. The id-0 capability announcement sweeps nothing and is written
    // without it, so a busy lock never makes a capable session look unsupported.
    let _lock = if id != 0 {
        let lock = command_lock(&directory.join("game-shutdown.lock"))
            .ok_or("Could not access the game shutdown command.")?;
        sweep_results(directory, "game-shutdown", id);
        Some(lock)
    } else {
        None
    };
    save_json(
        &reply_path(directory, id),
        &Reply {
            worker: session.worker,
            request_id: id,
            game_id: game,
            ok,
            message: message.into(),
            answer,
        },
    )
}

#[cfg(test)]
mod tests;
