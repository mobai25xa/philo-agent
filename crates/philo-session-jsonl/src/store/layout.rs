//! Stable filesystem layout, session-directory codec, locking, and directory
//! durability helpers.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use philo_session::SessionId;

use crate::error::{JsonlOpenError, io_error};

pub(super) const LOG_FILE: &str = "log.jsonl";
const LOCK_FILE: &str = "lock";
/// Sidecar cache of the session's resolved display title. Written by the
/// actor under the session lock; a missing or stale file only costs the
/// listing its title (readers fall back to the id), never correctness.
pub(super) const TITLE_FILE: &str = "title";

/// Deterministic, reversible, collision-free directory encoding for a
/// session id. Pinned by the golden format tests.
pub(super) fn session_dir_name(session_id: &SessionId) -> String {
    let mut name = String::from("s-");
    for byte in session_id.as_str().bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => name.push(char::from(byte)),
            other => {
                name.push('%');
                name.push_str(&format!("{other:02X}"));
            }
        }
    }
    name
}

/// Decodes only canonical session directory names.
pub(super) fn decode_session_dir_name(name: &str) -> Option<SessionId> {
    let encoded = name.strip_prefix("s-")?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let high = hex_value(*bytes.get(index + 1)?)?;
                let low = hex_value(*bytes.get(index + 2)?)?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte @ (b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_') => {
                decoded.push(byte);
                index += 1;
            }
            _ => return None,
        }
    }
    let session_id = SessionId::new(String::from_utf8(decoded).ok()?);
    (session_dir_name(&session_id) == name).then_some(session_id)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Takes the session's OS advisory lock; the returned handle owns the lock.
pub(super) fn acquire_lock(dir: &Path) -> Result<File, JsonlOpenError> {
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| io_error("opening lock file", &error))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(JsonlOpenError::Locked { path }),
        Err(TryLockError::Error(error)) => Err(io_error("acquiring session lock", &error)),
    }
}

/// Reads the title sidecar, if present and usable.
pub(super) fn read_title_file(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(TITLE_FILE)).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Reads the log's last modification as unix seconds. Failures are
/// non-fatal: the timestamp is an advisory listing hint, not a durable fact.
pub(super) fn read_updated_at(dir: &Path) -> Option<u64> {
    let modified = std::fs::metadata(dir.join(LOG_FILE))
        .ok()?
        .modified()
        .ok()?;
    let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since_epoch.as_secs())
}

/// Atomically rewrites (or clears) the title sidecar. Failures are
/// non-fatal: the sidecar is a listing cache, not a durable fact.
pub(super) fn write_title_file(dir: &Path, title: Option<&str>) {
    let target = dir.join(TITLE_FILE);
    match title {
        Some(title) => {
            let temp = dir.join(format!("{TITLE_FILE}.tmp"));
            if std::fs::write(&temp, title.as_bytes())
                .and_then(|()| std::fs::rename(&temp, &target))
                .is_err()
            {
                let _ = std::fs::remove_file(&temp);
            }
        }
        None => {
            if target.exists() {
                let _ = std::fs::remove_file(&target);
            }
        }
    }
}

#[cfg(unix)]
pub(super) fn fsync_dir(path: &Path) -> Result<(), JsonlOpenError> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| io_error("syncing directory", &error))
}

/// Windows has no user-mode directory fsync; NTFS journals metadata itself.
#[cfg(not(unix))]
pub(super) fn fsync_dir(_path: &Path) -> Result<(), JsonlOpenError> {
    Ok(())
}
