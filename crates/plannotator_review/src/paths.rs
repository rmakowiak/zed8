//! Folders in Plannotator's data folder (`~/.plannotator`). Plannotator owns
//! `sessions` and `guides`. The `zed-*` folders belong to this crate.

use std::path::PathBuf;

fn plannotator_dir() -> PathBuf {
    util::paths::home_dir().join(".plannotator")
}

/// One `<pid>.json` for each running Plannotator server.
pub fn sessions_dir() -> PathBuf {
    plannotator_dir().join("sessions")
}

/// Saved guided reviews, in one folder for each repository.
pub fn guides_dir() -> PathBuf {
    plannotator_dir().join("guides")
}

/// Unsent replies to guide notes, and spec review drafts and rounds.
pub fn drafts_dir() -> PathBuf {
    plannotator_dir().join("zed-drafts")
}

/// Code review comments that could not be sent.
pub fn unsent_dir() -> PathBuf {
    plannotator_dir().join("zed-unsent")
}

/// The worktrees of each Zed window, for `fork/plannotator-browser`.
pub fn windows_dir() -> PathBuf {
    plannotator_dir().join("zed-windows")
}
