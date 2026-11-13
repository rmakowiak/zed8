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

/// The worktrees of each Zed window, for `zed8/plannotator-browser`.
pub fn windows_dir() -> PathBuf {
    plannotator_dir().join("zed-windows")
}

/// The Claude Code session of each terminal thread, so a restored thread resumes it.
pub fn claude_terminal_sessions_file() -> PathBuf {
    plannotator_dir().join("zed-claude-terminal-sessions.json")
}

/// The review folders of the `pr-review` and `manual-test` skills, one for
/// each target such as `pr-43508`.
pub fn reviews_dir() -> PathBuf {
    util::paths::home_dir().join("workspace").join("n8n-reviews")
}

/// The review actions of each worktree: their threads and their progress.
pub fn actions_dir() -> PathBuf {
    plannotator_dir().join("zed-actions")
}
