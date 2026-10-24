//! Atomic file replacement shared by every persistence path.
//!
//! One mechanism, several consumers: `store.rs` (profiles.json, the
//! downloaded snapshots) and `runtime.rs` (runtime.yaml) all land their
//! artifacts through [`write_atomic`]. Every one of those files carries
//! credentials, so the mode is fixed to 0600 while the bytes are still only
//! the tmp name — the real name never becomes visible with looser
//! permissions — and every write is a same-directory tmp file plus
//! `rename`: a crash mid-write leaves the previous content intact instead
//! of a truncated file. A failed write takes its tmp sibling back down
//! (best effort), so no `*.tmp` clutter is ever left next to the target.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};

/// Overwrite `path` atomically: write a sibling tmp file, chmod it 0600,
/// then rename it over the target. `Paths::ensure` must have run first,
/// so the target's directory exists.
pub(crate) fn write_atomic(path: &Path, data: &str) -> Result<()> {
    let dir = path.parent().context("target path has no parent")?;
    let name = path.file_name().context("target path has no file name")?;
    let tmp = dir.join(format!("{}.tmp", name.to_string_lossy()));

    // The whole sequence is one transaction from the caller's point of view:
    // any failure also takes the tmp sibling back down, so a failed write
    // never leaves residue next to the target.
    let outcome = (|| {
        fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
        let mut permissions = fs::metadata(&tmp)?.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&tmp, permissions)
            .with_context(|| format!("restricting {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("moving {} into place", tmp.display()))?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    outcome
}
