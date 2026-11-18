//! The built game host run from two folders, as it is when two Crystal versions run games at
//! once, or one game runs beside ReShade and one without: the taskbar would group their windows
//! by the two paths. Both hosts take the one application ID before they read a session, so their
//! games share one group of buttons. No game, sign-in or network is started: the test is the
//! launcher's end of the pipe, and each host ends when that closes.

#![cfg(target_arch = "x86")]

use crystal_loader::pipe::{random_pipe_path, LineReader, Pipe};
use std::{process::Command, time::{Duration, Instant}};

#[test]
fn hosts_in_two_folders_take_one_taskbar_identity_before_any_session() {
    let root = std::env::temp_dir().join(format!("crystal-taskbar-identity-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    for folder in ["crystal", r"reshade\hosts\0123456789abcdef-fedcba9876543210"] {
        let directory = root.join(folder);
        std::fs::create_dir_all(&directory).unwrap();
        let host = directory.join("pol.exe");
        std::fs::copy(env!("CARGO_BIN_EXE_pol"), &host).unwrap();
        let pipe = random_pipe_path();
        let server = Pipe::create_server(&pipe).unwrap();
        let data = directory.join("data");
        let mut child = Command::new(&host).args(["--crystal-pipe", &pipe, "--crystal-launcher-pid", &std::process::id().to_string()])
            .env(crystal_loader::data_paths::ROOT_ENV, &data).spawn().unwrap();
        server.accept().unwrap();
        let hello = String::from_utf8_lossy(LineReader::default().read_line(&server, Some(Duration::from_secs(20))).unwrap()).into_owned();
        assert!(hello.contains("\"type\":\"hello\""), "{folder}: {hello}");
        // No session: the host's read ends with the pipe, and the host with it.
        drop(server);
        let until = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() { break status; }
            if Instant::now() >= until {
                let _ = child.kill(); // this test's own child only
                panic!("{folder}: the host did not end when its pipe closed");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(crystal_loader::EXIT_PIPE), "{folder}");
        // The host's log in Crystal's Logs folder, never beside it: the ID was set, after the
        // hello and before the session read.
        assert!(!directory.join("crystal-loader.log").exists(), "{folder}");
        let log = std::fs::read_to_string(data.join("Logs").join("crystal-loader.log")).unwrap();
        let at = |needle: &str| log.find(needle).unwrap_or_else(|| panic!("{folder}: no {needle:?} in the host's log:\n{log}"));
        let set = at("stage: taskbar application ID CrystalLauncher.GameHost ");
        assert!(at("stage: connected") < set && set < at("error: reading the session"), "{folder}:\n{log}");
        assert!(!log.contains("taskbar application ID not set"), "{folder}:\n{log}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
