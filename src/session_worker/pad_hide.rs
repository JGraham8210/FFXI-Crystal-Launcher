//! The controller buttons a running game hides for Crystal's pad hotkeys, changed while it
//! runs: the window writes the whole list for the account's live worker, which sends it to its
//! loader as one `pad_hide` command (as `controller_settings` travels). The latest list wins;
//! nothing answers, and a loader that does not know the command ignores it.
use super::*;

const FILE: &str = "pad-hide.json";
const LOCK: &str = "pad-hide.lock";
/// More than Crystal ever sends (the loader refuses more).
const MAX_ENTRIES: usize = 64;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    worker: Identity,
    pad_hide: Vec<[u32; 2]>,
}

/// Give `account`'s running game this list now. Err when it has no live session.
pub(crate) fn request(config: &Config, account: &Account, list: &[[u32; 2]]) -> Result<(), String> {
    let session = find(config, account).ok_or("This account has no active game session.")?;
    request_for(&session, list)
}

fn request_for(session: &BackgroundSession, list: &[[u32; 2]]) -> Result<(), String> {
    if list.len() > MAX_ENTRIES { return Err("Too many controller hotkeys.".into()); }
    let directory = session.path.parent().ok_or("The game session has no account directory.")?;
    {
        let _lock = command_lock(&directory.join(LOCK)).ok_or("Could not access the controller hotkey command.")?;
        save_json(&directory.join(FILE), &Request { worker: session.worker, pad_hide: list.to_vec() })?;
    }
    wake::signal(session.worker);
    Ok(())
}

/// This worker's pending list as the loader's line, once.
pub(crate) fn take(directory: &Path) -> Option<Vec<u8>> {
    let path = directory.join(FILE);
    // Asked every tick: no lock while nothing is queued.
    if !path.exists() { return None; }
    let _lock = command_lock(&directory.join(LOCK))?;
    let bytes = read_bounded(&path, 16 * 1024)?;
    // Removed before it is sent: a denied deletion must never deliver it twice.
    fs::remove_file(&path).ok()?;
    let request: Request = serde_json::from_slice(&bytes).ok()?;
    if Some(request.worker) != identity(std::process::id()) || request.pad_hide.len() > MAX_ENTRIES { return None; }
    line(&request.pad_hide)
}

fn line(list: &[[u32; 2]]) -> Option<Vec<u8>> {
    let mut line = serde_json::to_vec(&serde_json::json!({ "type": "pad_hide", "pad_hide": list })).ok()?;
    line.push(b'\n');
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_list_reaches_this_workers_loader_once() {
        let directory = std::env::temp_dir().join(format!("crystal-pad-hide-{}-{}", std::process::id(), crate::backend::unique_id().unwrap()));
        fs::create_dir_all(&directory).unwrap();
        let session = BackgroundSession { path: directory.join("session.json"), worker: identity(std::process::id()).unwrap() };
        assert_eq!(take(&directory), None, "nothing queued");
        request_for(&session, &[[0, 0x20]]).unwrap();
        request_for(&session, &[[0, 0x400], [1 << 16, 0x20]]).unwrap();
        let sent = take(&directory).unwrap();
        assert_eq!(String::from_utf8(sent).unwrap(), "{\"pad_hide\":[[0,1024],[65536,32]],\"type\":\"pad_hide\"}\n");
        assert_eq!(take(&directory), None, "once");
        // An empty list clears the game's.
        request_for(&session, &[]).unwrap();
        assert_eq!(take(&directory).unwrap(), b"{\"pad_hide\":[],\"type\":\"pad_hide\"}\n");
        // Another worker's request is not this one's to send.
        let other = BackgroundSession { path: session.path.clone(), worker: Identity { pid: 4, created: 1 } };
        request_for(&other, &[[0, 1]]).unwrap();
        assert_eq!(take(&directory), None);
        assert!(!directory.join(FILE).exists(), "and is dropped");
        let _ = fs::remove_dir_all(&directory);
    }
}
