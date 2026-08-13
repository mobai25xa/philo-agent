//! Content-addressed artifact files for image bytes (ADR-0002).
//!
//! Image bytes are stored verbatim under `{session_dir}/artifacts/{sha256}`.
//! Writes go through a temp file, fsync, and an atomic rename, so a name
//! that exists is complete by construction; crash residue is a `*.tmp` file
//! that recovery reports as an orphan.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Directory holding a session's content-addressed artifacts.
pub(crate) const ARTIFACTS_DIR: &str = "artifacts";

/// Lowercase hex SHA-256 of `bytes`; the artifact file name.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Durably stores one artifact under its content hash.
///
/// The artifacts directory is created on demand (plain-text sessions never
/// get one). An already-present target file is skipped: renames are atomic,
/// so an existing name is a complete copy of the same content.
pub(crate) fn store_artifact(
    artifacts_dir: &Path,
    hash: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    let target = artifacts_dir.join(hash);
    if target.is_file() {
        return Ok(());
    }
    fs::create_dir_all(artifacts_dir)?;
    let temp = artifacts_dir.join(format!("{hash}.tmp"));
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, &target)?;
    Ok(())
}

/// Loads one referenced artifact, verifying recorded length and content hash.
/// Any mismatch is a data-integrity failure surfaced to the caller.
pub(crate) fn load_artifact(
    artifacts_dir: &Path,
    hash: &str,
    expected_len: u64,
) -> Result<Vec<u8>, String> {
    let path = artifacts_dir.join(hash);
    let bytes = fs::read(&path)
        .map_err(|error| format!("referenced artifact {hash} unreadable: {:?}", error.kind()))?;
    if bytes.len() as u64 != expected_len {
        return Err(format!(
            "artifact {hash} length {} does not match recorded length {expected_len}",
            bytes.len()
        ));
    }
    if sha256_hex(&bytes) != hash {
        return Err(format!("artifact {hash} content does not match its hash"));
    }
    Ok(bytes)
}
