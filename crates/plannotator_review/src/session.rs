//! Finds live Plannotator sessions: the code review of a worktree, and the
//! spec reviews (annotate sessions on a markdown file).
//!
//! Each Plannotator server writes `~/.plannotator/sessions/<pid>.json`. A file
//! can outlive its process, so a session counts only while its pid runs.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub pid: i32,
    pub url: String,
    pub mode: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub started_at: String,
}

/// The newest live review session whose target is one of `roots`, or inside one.
pub fn find_review_session(dir: &Path, roots: &[PathBuf]) -> Option<Session> {
    find_review_session_with(dir, roots, process_is_alive)
}

fn find_review_session_with(
    dir: &Path,
    roots: &[PathBuf],
    is_alive: impl Fn(i32) -> bool,
) -> Option<Session> {
    let roots: Vec<PathBuf> = roots.iter().map(|root| canonical(root)).collect();
    read_sessions(dir)
        .into_iter()
        .filter(|session| session.mode == "review")
        .filter(|session| {
            session.target.as_deref().is_some_and(|target| {
                let target = canonical(Path::new(target));
                roots.iter().any(|root| target.starts_with(root))
            })
        })
        .filter(|session| is_alive(session.pid))
        .max_by(|a, b| a.started_at.cmp(&b.started_at))
}

/// Live annotate sessions on a markdown file, oldest first. A spec can live
/// anywhere (often in Claude's scratchpad), so there is no worktree filter.
pub fn find_spec_sessions(dir: &Path) -> Vec<Session> {
    find_spec_sessions_with(dir, process_is_alive)
}

fn find_spec_sessions_with(dir: &Path, is_alive: impl Fn(i32) -> bool) -> Vec<Session> {
    let mut sessions: Vec<Session> = read_sessions(dir)
        .into_iter()
        .filter(|session| session.mode == "annotate")
        .filter(|session| {
            session.target.as_deref().is_some_and(|target| {
                matches!(
                    Path::new(target).extension().and_then(|ext| ext.to_str()),
                    Some("md" | "mdx" | "markdown")
                )
            })
        })
        .filter(|session| is_alive(session.pid))
        .collect();
    sessions.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    sessions
}

/// Every session file that parses. Dead sessions are still in the list.
fn read_sessions(dir: &Path) -> Vec<Session> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "json" {
                return None;
            }
            serde_json::from_slice::<Session>(&std::fs::read(&path).ok()?).ok()
        })
        .collect()
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Signal 0 checks existence only. EPERM still means the process exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, pid: i32, mode: &str, target: &Path, started_at: &str) {
        let json = serde_json::json!({
            "pid": pid,
            "port": 1000 + pid,
            "url": format!("http://localhost:{}", 1000 + pid),
            "mode": mode,
            "target": target,
            "startedAt": started_at,
        });
        std::fs::write(dir.join(format!("{pid}.json")), json.to_string()).unwrap();
    }

    #[test]
    fn picks_newest_live_review_session_for_the_worktree() {
        let sessions = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let w = worktree.path();
        write(sessions.path(), 1, "review", w, "2026-11-11T10:00:00Z");
        write(sessions.path(), 2, "review", w, "2026-11-11T12:00:00Z");
        write(sessions.path(), 3, "review", w, "2026-11-11T13:00:00Z"); // dead
        write(sessions.path(), 4, "annotate", w, "2026-11-11T14:00:00Z");
        write(sessions.path(), 5, "review", other.path(), "2026-11-11T15:00:00Z");
        std::fs::write(sessions.path().join("junk.json"), "not json").unwrap();

        let found =
            find_review_session_with(sessions.path(), &[w.to_path_buf()], |pid| pid != 3).unwrap();
        assert_eq!(found.pid, 2);
    }

    #[test]
    fn returns_none_without_a_matching_session() {
        let sessions = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        write(sessions.path(), 1, "review", other.path(), "2026-11-11T10:00:00Z");
        assert!(
            find_review_session_with(sessions.path(), &[worktree.path().to_path_buf()], |_| true)
                .is_none()
        );
    }

    #[test]
    fn finds_live_spec_sessions_on_markdown() {
        let sessions = tempfile::tempdir().unwrap();
        let docs = tempfile::tempdir().unwrap();
        write(sessions.path(), 1, "annotate", &docs.path().join("spec.md"), "2026-01-02");
        write(sessions.path(), 2, "annotate", &docs.path().join("page.html"), "2026-01-03");
        write(sessions.path(), 3, "annotate", &docs.path().join("old.md"), "2026-01-01");
        write(sessions.path(), 4, "review", docs.path(), "2026-01-04");
        let found = find_spec_sessions_with(sessions.path(), |pid| pid != 3);
        assert_eq!(found.iter().map(|session| session.pid).collect::<Vec<_>>(), vec![1]);
    }
}
