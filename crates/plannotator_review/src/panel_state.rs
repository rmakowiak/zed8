//! What the Review panel shows, loaded off the main thread: the branch, the
//! PR, the review sessions, the spec reviews, the guide and its file stats.

use crate::{
    guide::{self, LoadedGuide},
    session::{self, Session},
    start::{self, PullRequest},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

/// `gh` calls GitHub, so the PR is looked up less often than local state.
const PR_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Default, PartialEq)]
pub struct PanelState {
    pub roots: Vec<PathBuf>,
    pub branch: Option<String>,
    /// Live review sessions of the worktree, newest first.
    pub sessions: Vec<Session>,
    pub guide: Option<LoadedGuide>,
    /// Git status and line counts of the guide's files, by path.
    pub file_stats: HashMap<String, FileStat>,
    /// Live spec reviews (annotate sessions on markdown), oldest first.
    pub specs: Vec<Session>,
    /// The local HEAD commit.
    pub head: Option<String>,
    /// The GitHub PR of the branch, from `gh`. See `PR_REFRESH_INTERVAL`.
    pub pr: Option<PullRequest>,
    pub pr_checked_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FileStat {
    /// The `git diff --name-status` letter: A, M, D or R.
    pub status: char,
    pub added: u32,
    pub removed: u32,
}

/// Tells the browser hook (`zed8/plannotator-browser`) which worktrees this
/// window has open, so Plannotator sessions there stay in Zed.
pub fn announce_roots(window_key: &str, roots: &[PathBuf]) {
    let dir = crate::paths::windows_dir();
    let body = serde_json::json!({ "pid": std::process::id(), "roots": roots });
    let result = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(format!("{window_key}.json")), body.to_string()));
    if let Err(error) = result {
        log::warn!("plannotator: cannot announce worktrees: {error}");
    }
}

pub fn load_state(roots: Vec<PathBuf>, previous: PanelState) -> PanelState {
    let root = roots.first().cloned();
    let git = |args: &[&str]| -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root.as_ref()?)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let head = git(&["rev-parse", "HEAD"]);
    let mut sessions = session::find_review_sessions(&crate::paths::sessions_dir(), &roots);
    let mut specs = session::find_spec_sessions(&crate::paths::sessions_dir());
    session::add_ancestors(&mut sessions);
    session::add_ancestors(&mut specs);
    let guide = guide::find_guide(&crate::paths::guides_dir(), branch.as_deref(), head.as_deref());
    let mut file_stats = HashMap::default();
    let base = guide
        .as_ref()
        .and_then(|guide| guide.envelope.review.as_ref()?.base.clone())
        .unwrap_or_else(|| "origin/master".to_string());
    if guide.is_some()
        && let Some(merge_base) = git(&["merge-base", "HEAD", &base])
    {
        for line in git(&["diff", "--name-status", &merge_base]).unwrap_or_default().lines() {
            let mut parts = line.split('\t');
            let (Some(status), Some(path)) = (parts.next(), parts.last()) else {
                continue;
            };
            file_stats.entry(path.to_string()).or_insert(FileStat::default()).status =
                status.chars().next().unwrap_or('M');
        }
        for line in git(&["diff", "--numstat", &merge_base]).unwrap_or_default().lines() {
            let mut parts = line.split('\t');
            let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.last())
            else {
                continue;
            };
            let stat = file_stats.entry(path.to_string()).or_insert(FileStat::default());
            stat.added = added.parse().unwrap_or(0);
            stat.removed = removed.parse().unwrap_or(0);
        }
    }
    let pr_is_fresh = branch.is_some()
        && branch == previous.branch
        && head == previous.head
        && previous
            .pr_checked_at
            .is_some_and(|checked| checked.elapsed() < PR_REFRESH_INTERVAL);
    let (pr, pr_checked_at) = if pr_is_fresh {
        (previous.pr, previous.pr_checked_at)
    } else {
        (root.as_deref().and_then(start::find_pull_request), Some(Instant::now()))
    };
    PanelState {
        roots,
        branch,
        sessions,
        guide,
        file_stats,
        specs,
        head,
        pr,
        pr_checked_at,
    }
}


