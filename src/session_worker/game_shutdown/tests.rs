use super::*;
use std::process::{Child, Stdio};
fn fixture(root: &Path) -> (Config, Account, PathBuf) {
    let config = Config {
        experiment: root.into(),
        repository: root.into(),
        game_install: None,
        dat_overlays: Vec::new(),
    };
    let mut account = crate::blank_account();
    account.pol_id = "TEST0001".into();
    let directory = config.account_directory(&account);
    fs::create_dir_all(&directory).unwrap();
    (config, account, directory)
}
struct Helper(Child);
impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "crystal-game-shutdown-{}",
            crate::pol_less::pipe::random_hex().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        assert!(self
            .0
            .canonicalize()
            .unwrap()
            .starts_with(std::env::temp_dir().canonicalize().unwrap()));
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn helper(root: &Path, role: &str, host: u32) -> Helper {
    Helper(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session_worker::game_shutdown::tests::game_shutdown_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("CRYSTAL_TEST_SHUTDOWN_ROOT", root)
            .env("CRYSTAL_TEST_SHUTDOWN_ROLE", role)
            .env("CRYSTAL_TEST_SHUTDOWN_HOST", host.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}
/// Short limits: `close_ms` for the in-world wait, 300 ms after a dialog's OK.
fn limits(close_ms: u64) -> Limits {
    Limits { ready: Duration::from_secs(1), dispatch: Duration::from_millis(500), close: Duration::from_millis(close_ms),
        dialog: Duration::from_millis(300), grace: Duration::from_millis(200) }
}
/// The fallback in tests that must never end a host.
fn never() -> Result<(), String> { panic!("this game must not be ended") }
fn wait_file(root: &Path, name: &str) {
    let until = Instant::now() + Duration::from_secs(5);
    while !root.join(name).exists() {
        assert!(Instant::now() < until, "fixture milestone {name}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
#[ignore = "Child of the isolated shutdown protocol test; no game or credentials are loaded"]
fn game_shutdown_helper() {
    let root = PathBuf::from(std::env::var_os("CRYSTAL_TEST_SHUTDOWN_ROOT").unwrap());
    let role = std::env::var("CRYSTAL_TEST_SHUTDOWN_ROLE").unwrap();
    if role == "host" {
        wait_file(&root, "close-host");
        return;
    }
    assert_eq!(role, "worker");
    let (config, account, directory) = fixture(&root);
    let worker = identity(std::process::id()).unwrap();
    let host = std::env::var("CRYSTAL_TEST_SHUTDOWN_HOST")
        .unwrap()
        .parse()
        .unwrap();
    save_record(
        &record_path(&config, &account),
        &Record {
            worker,
            state: Event::Started(Started {
                pid: host,
                detail: "fixture".into(),
                log: root.join("fixture.log"),
                timing: String::new(),
            }),
            social_only: false,
            authenticated: true,
            native_batch: false,
            otp_used: Some(false),
            launch_choice: None, launch_tool: Some(Tool::Vanilla),
        },
    )
    .unwrap();
    publish_friends(
        &directory,
        &crate::friends::Snapshot {
            game_running: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    publish(&directory, 0, 7, true, "", None).unwrap();
    fs::write(root.join("ready"), b"").unwrap();
    // As the worker's supervisor, once the loader pipe closes: how did the game end?
    let classify = || {
        let until = Instant::now() + Duration::from_secs(10);
        while !(directory.join(TERMINATE_FILE).exists() && identity(host).is_none()) {
            assert!(Instant::now() < until, "the host was not ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        let kind = if terminated(&directory) { "normal" } else { "unknown" };
        fs::write(root.join("classified"), kind).unwrap();
    };
    if root.join("terminate").exists() { return classify(); }
    let until = Instant::now() + Duration::from_secs(5);
    let frame = loop {
        if let Some(frame) = take(&directory) {
            break frame;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    };
    let frame: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    assert_eq!(frame["type"], "game_shutdown");
    assert_eq!(frame["game_id"], 7);
    assert!(frame.get("command").is_none());
    assert!(take(&directory).is_none());
    // The loader's answer: `answer` names it (a current loader), or the older loader's lines.
    let id = frame["request_id"].as_u64().unwrap();
    match fs::read_to_string(root.join("answer")).unwrap_or_default().as_str() {
        "" => publish(&directory, id, 7, true, "Requested", None).unwrap(),
        "silent" => {}
        "world" => publish(&directory, id, 7, true, "Requested", Some(Answer::World)).unwrap(),
        "dialog" => publish(&directory, id, 7, true, "Pressed OK", Some(Answer::Dialog)).unwrap(),
        "not_in_world" => publish(&directory, id, 7, false, "No character", Some(Answer::NotInWorld)).unwrap(),
        "old_not_in_world" => publish(&directory, id, 7, false, OLD_NOT_IN_WORLD, None).unwrap(),
        "old_refused" => publish(&directory, id, 7, false, "Shutdown was already requested. Wait for the game to close.", None).unwrap(),
        other => panic!("{other}"),
    }
    fs::write(root.join("dispatched"), b"").unwrap();
    if root.join("expect-end").exists() { return classify(); }
    if root.join("retain-pol").exists() {
        wait_file(&root, "end-game");
        ended(&directory, 7).unwrap();
    }
    wait_file(&root, "close-worker");
}
#[test]
fn instant_close_ends_only_the_verified_host_and_its_worker_records_a_normal_close() {
    let root = Root::new();
    fs::write(root.0.join("terminate"), b"").unwrap();
    let mut host = helper(&root.0, "host", 0);
    let mut worker = helper(&root.0, "worker", host.0.id());
    wait_file(&root.0, "ready");
    let (config, account, directory) = fixture(&root.0);
    let session = find(&config, &account).unwrap();
    // The real check refuses a process that is not a pol.exe started after the worker.
    assert!(terminate(&session).unwrap_err().contains("Could not close FFXI"));
    assert!(host.0.try_wait().unwrap().is_none() && !directory.join(TERMINATE_FILE).exists());
    let (host_pid, owner) = (host.0.id(), session.worker);
    terminate_within(&session, &|_, found, worker| found.pid == host_pid && worker == owner,
        Duration::from_secs(5), Duration::from_secs(5)).unwrap();
    assert!(host.0.try_wait().unwrap().is_some(), "the host was ended");
    assert!(worker.0.wait().unwrap().success());
    assert_eq!(fs::read_to_string(root.0.join("classified")).unwrap(), "normal");
    assert!(!directory.join(TERMINATE_FILE).exists() && !terminated(&directory), "consumed once");
}
#[test]
fn a_terminate_marker_for_another_worker_or_host_is_not_a_normal_close() {
    let root = Root::new();
    let (config, account, directory) = fixture(&root.0);
    let worker = identity(std::process::id()).unwrap();
    let started = |pid| Record {
        worker,
        state: Event::Started(Started { pid, detail: "fixture".into(), log: root.0.join("fixture.log"), timing: String::new() }),
        social_only: false, authenticated: true, native_batch: false, otp_used: Some(false),
        launch_choice: None, launch_tool: None,
    };
    save_record(&record_path(&config, &account), &started(worker.pid)).unwrap();
    let other = Identity { created: worker.created.wrapping_add(1), ..worker };
    for marker in [Terminate { worker: other, host: worker }, Terminate { worker, host: Identity { pid: worker.pid + 4, ..worker } }] {
        save_json(&directory.join(TERMINATE_FILE), &marker).unwrap();
        assert!(!terminated(&directory));
        assert!(directory.join(TERMINATE_FILE).exists(), "a foreign marker is left alone");
    }
    save_json(&directory.join(TERMINATE_FILE), &Terminate { worker, host: worker }).unwrap();
    assert!(terminated(&directory) && !terminated(&directory));
    let report = crate::launch_log::classify_loader_end(&crate::launch_log::LoaderEnd::Terminated, Duration::from_secs(1));
    assert_eq!(report.kind, crate::launch_log::ExitKind::Normal);
    assert_eq!(report.summary, "Game closed normally");
}
#[test]
fn retained_pol_completes_shutdown_only_for_the_ended_game_and_keeps_both_processes_alive() {
    let root = Root::new();
    fs::write(root.0.join("retain-pol"), b"").unwrap();
    let mut host = helper(&root.0, "host", 0);
    let mut worker = helper(&root.0, "worker", host.0.id());
    wait_file(&root.0, "ready");
    let (config, account, directory) = fixture(&root.0);
    let session = find(&config, &account).unwrap();
    let captured = session.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || tx.send(request_within(&captured, limits(4000), &never)).unwrap());
    wait_file(&root.0, "dispatched");
    for marker in [
        Ended { worker: Identity { created: session.worker.created.wrapping_add(1), ..session.worker }, game_id: 7 },
        Ended { worker: session.worker, game_id: 6 },
    ] {
        save_json(&directory.join("game-shutdown-ended.json"), &marker).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(75)).is_err(), "neither an ACK nor a stale marker completes shutdown");
    }
    fs::write(root.0.join("end-game"), b"").unwrap();
    rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    thread.join().unwrap();
    assert!(host.0.try_wait().unwrap().is_none() && worker.0.try_wait().unwrap().is_none());
    assert!(!game_ended(&directory, session.worker, 8), "an older game cannot complete a later request");
    fs::write(root.0.join("close-host"), b"").unwrap();
    fs::write(root.0.join("close-worker"), b"").unwrap();
    assert!(host.0.wait().unwrap().success() && worker.0.wait().unwrap().success());
}
#[test]
fn game_shutdown_ack_waits_for_both_processes_and_rejects_an_overlapping_request() {
    let root = Root::new();
    let mut host = helper(&root.0, "host", 0);
    let mut worker = helper(&root.0, "worker", host.0.id());
    wait_file(&root.0, "ready");
    let (config, account, _) = fixture(&root.0);
    let session = find(&config, &account).unwrap();
    let captured = session.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let request_thread = std::thread::spawn(move || tx.send(request_within(&captured, limits(4000), &never)).unwrap());
    wait_file(&root.0, "dispatched");
    assert!(rx.try_recv().is_err(), "an ACK is not a completed exit");
    let quick = Limits { ready: Duration::from_millis(10), dispatch: Duration::from_millis(20), ..limits(20) };
    assert!(request_within(&session, quick, &never).unwrap_err().message.contains("already"));
    fs::write(root.0.join("close-host"), b"").unwrap();
    assert!(host.0.wait().unwrap().success());
    assert!(
        rx.recv_timeout(Duration::from_millis(75)).is_err(),
        "POL's owner must finish too"
    );
    fs::write(root.0.join("close-worker"), b"").unwrap();
    assert!(worker.0.wait().unwrap().success());
    rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    request_thread.join().unwrap();
}
/// An in-world game (or an older loader's acceptance) still open after the close limit is
/// never ended by itself: the refusal asks for Close now, both processes stay.
#[test]
fn an_accepted_shutdown_that_times_out_offers_close_now_and_ends_nothing() {
    for answer in ["world", ""] {
        let root = Root::new();
        fs::write(root.0.join("answer"), answer).unwrap();
        let mut host = helper(&root.0, "host", 0);
        let mut worker = helper(&root.0, "worker", host.0.id());
        wait_file(&root.0, "ready");
        let (config, account, directory) = fixture(&root.0);
        let session = find(&config, &account).unwrap();
        let refusal = request_within(&session, limits(150), &never).unwrap_err();
        assert!(refusal.message.contains("still open") && refusal.close_now, "{answer:?}");
        assert!(host.0.try_wait().unwrap().is_none() && worker.0.try_wait().unwrap().is_none());
        assert!(!directory.join(TERMINATE_FILE).exists());
        fs::write(root.0.join("close-host"), b"").unwrap();
        fs::write(root.0.join("close-worker"), b"").unwrap();
        assert!(host.0.wait().unwrap().success());
        assert!(worker.0.wait().unwrap().success());
    }
}

/// Not in the world, no answer, or a fatal dialog's OK that left the game open: the verified
/// host is ended through the instant path, and its worker records a normal close.
#[test]
fn a_game_that_cannot_close_itself_is_ended_through_the_instant_path() {
    for answer in ["not_in_world", "old_not_in_world", "silent", "dialog"] {
        let root = Root::new();
        fs::write(root.0.join("answer"), answer).unwrap();
        fs::write(root.0.join("expect-end"), b"").unwrap();
        let mut host = helper(&root.0, "host", 0);
        let mut worker = helper(&root.0, "worker", host.0.id());
        wait_file(&root.0, "ready");
        let (config, account, _) = fixture(&root.0);
        let session = find(&config, &account).unwrap();
        let (host_pid, owner) = (host.0.id(), session.worker);
        let started = Instant::now();
        request_within(&session, limits(4000), &|| terminate_held(&session,
            &|_, found, worker| found.pid == host_pid && worker == owner, Duration::from_secs(5), Duration::from_secs(5))).unwrap();
        assert!(started.elapsed() < Duration::from_secs(4), "{answer}: never the in-world wait");
        assert!(host.0.try_wait().unwrap().is_some(), "{answer}: the host was ended");
        assert!(worker.0.wait().unwrap().success());
        assert_eq!(fs::read_to_string(root.0.join("classified")).unwrap(), "normal", "{answer}");
    }
}

/// A fatal dialog's OK that closes the game in time needs no instant path; an older loader's
/// other refusals stay today's failures and end nothing.
#[test]
fn a_dialog_that_closes_the_game_or_an_old_refusal_ends_nothing() {
    for answer in ["dialog", "old_refused"] {
        let root = Root::new();
        fs::write(root.0.join("answer"), answer).unwrap();
        let mut host = helper(&root.0, "host", 0);
        let mut worker = helper(&root.0, "worker", host.0.id());
        wait_file(&root.0, "ready");
        let (config, account, directory) = fixture(&root.0);
        let session = find(&config, &account).unwrap();
        let captured = session.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let patient = Limits { dialog: Duration::from_secs(4), ..limits(4000) };
        let thread = std::thread::spawn(move || tx.send(request_within(&captured, patient, &never)).unwrap());
        wait_file(&root.0, "dispatched");
        if answer == "dialog" {
            fs::write(root.0.join("close-host"), b"").unwrap();
            fs::write(root.0.join("close-worker"), b"").unwrap();
            rx.recv_timeout(Duration::from_secs(4)).unwrap().unwrap();
        } else {
            let refusal = rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap_err();
            assert!(refusal.message.contains("already requested") && !refusal.close_now);
            assert!(host.0.try_wait().unwrap().is_none());
            fs::write(root.0.join("close-host"), b"").unwrap();
            fs::write(root.0.join("close-worker"), b"").unwrap();
        }
        thread.join().unwrap();
        assert!(host.0.wait().unwrap().success() && worker.0.wait().unwrap().success());
        assert!(!directory.join(TERMINATE_FILE).exists(), "{answer}");
    }
}

/// The ladder's table: which answers wait, end the host, or fail.
#[test]
fn each_answer_takes_its_rung() {
    let l = limits(90_000);
    assert_eq!(next(true, Some(Answer::World), String::new(), l), Next::Wait(l.close, false));
    assert_eq!(next(true, None, "Requested".into(), l), Next::Wait(l.close, false), "an older loader's acceptance");
    assert_eq!(next(true, Some(Answer::Dialog), String::new(), l), Next::Wait(l.dialog, true));
    assert_eq!(next(false, Some(Answer::NotInWorld), "x".into(), l), Next::End);
    assert_eq!(next(false, None, OLD_NOT_IN_WORLD.into(), l), Next::End);
    assert_eq!(next(false, None, "Shutdown was already requested.".into(), l), Next::Fail("Shutdown was already requested.".into()));
    assert!(matches!(next(false, None, String::new(), l), Next::Fail(m) if !m.is_empty()));
    assert_eq!((LIMITS.close, LIMITS.dialog), (Duration::from_secs(90), Duration::from_secs(10)));
}
#[test]
fn game_shutdown_never_consumes_a_foreign_or_expired_game_request() {
    let root = Root::new();
    let (config, account, directory) = fixture(&root.0);
    let worker = identity(std::process::id()).unwrap();
    save_record(
        &record_path(&config, &account),
        &Record {
            worker,
            state: Event::Started(Started {
                pid: worker.pid,
                detail: "fixture".into(),
                log: root.0.join("fixture.log"),
                timing: String::new(),
            }),
            social_only: false,
            authenticated: true,
            native_batch: false,
            otp_used: Some(false),
            launch_choice: None, launch_tool: None,
        },
    )
    .unwrap();
    publish_friends(
        &directory,
        &crate::friends::Snapshot {
            game_running: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    publish(&directory, 0, 7, true, "", None).unwrap();
    for (owner, game, deadline) in [
        (worker, 8, request_clock_ms() + 1000),
        (worker, 7, request_clock_ms()),
        (
            Identity {
                created: worker.created + 1,
                ..worker
            },
            7,
            request_clock_ms() + 1000,
        ),
    ] {
        let path = directory.join("game-shutdown.json");
        save_json(
            &path,
            &Request {
                worker: owner,
                request_id: 1,
                game_id: game,
                deadline_ms: deadline,
            },
        )
        .unwrap();
        assert!(take(&directory).is_none());
        assert_eq!(
            path.exists(),
            owner != worker,
            "another worker's file is preserved"
        );
    }
}
