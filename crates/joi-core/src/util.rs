//! Small filesystem utilities shared across the domain.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Atomically replace `path`'s contents with `bytes`.
///
/// Writes to a temporary sibling in the **same directory**, flushes and `fsync`s it, then `rename`s
/// it over the target. Because the rename is atomic on a single filesystem, a crash or a concurrent
/// reader can never observe a half-written file — it sees either the old contents or the new ones,
/// never a truncated mix. Parent directories are created if missing.
///
/// This is the one write path for Joi's own config/state files (e.g. `config.json`), so a power loss
/// mid-save can't corrupt them.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = tmp_sibling(path);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        // Flush the userspace buffer, then force the bytes to disk before the rename so a crash
        // can't leave the renamed file pointing at unwritten data.
        file.flush()?;
        file.sync_all()?;
    }
    // If the rename fails, clean up the temp file rather than leaking it.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// A temp path in the same directory as `path` (so `rename` stays on one filesystem), tagged with
/// the pid to avoid colliding with a concurrent writer.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_overwrites_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state.json");
        // Creates parent dirs and the file.
        atomic_write(&path, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        // Overwrites in place, leaving no temp sibling behind.
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp sibling must not linger");
    }
}
