//! Finds live Plannotator sessions: the code reviews of a worktree, and the
//! spec reviews (annotate sessions on a markdown file). Each session also
//! records its parent processes, so Zed can find the Claude thread behind it.
//!
//! Each Plannotator server writes `~/.plannotator/sessions/<pid>.json`. A file
//! can outlive its process, so a session counts only while its pid runs.

use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

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
    /// The parent, grandparent and further processes of the server. The
    /// Claude Code process that started the session is one of them.
    #[serde(skip)]
    pub ancestors: Vec<i32>,
    /// The working folder of each process in `ancestors`, where known.
    #[serde(skip)]
    pub ancestor_cwds: Vec<PathBuf>,
}

impl Session {
    /// The port, which is short enough to tell sessions apart in the panel.
    pub fn port(&self) -> &str {
        self.url.rsplit(':').next().unwrap_or(&self.url)
    }

    /// The local start time, such as `14:05`.
    pub fn started_label(&self) -> String {
        chrono::DateTime::parse_from_rfc3339(&self.started_at)
            .map(|time| time.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default()
    }
}

/// The live review sessions whose target is one of `roots`, or inside one,
/// newest first. Several Claude threads can each run one on the same worktree.
pub fn find_review_sessions(dir: &Path, roots: &[PathBuf]) -> Vec<Session> {
    find_review_sessions_with(dir, roots, process_is_alive)
}

fn find_review_sessions_with(
    dir: &Path,
    roots: &[PathBuf],
    is_alive: impl Fn(i32) -> bool,
) -> Vec<Session> {
    let roots: Vec<PathBuf> = roots.iter().map(|root| canonical(root)).collect();
    let mut sessions: Vec<Session> = read_sessions(dir)
        .into_iter()
        .filter(|session| session.mode == "review")
        .filter(|session| {
            session.target.as_deref().is_some_and(|target| {
                let target = canonical(Path::new(target));
                roots.iter().any(|root| target.starts_with(root))
            })
        })
        .filter(|session| is_alive(session.pid))
        .collect();
    sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    sessions
}

/// Fills in `ancestors` of each session from one `ps` call.
pub fn add_ancestors(sessions: &mut [Session]) {
    if sessions.is_empty() {
        return;
    }
    let parents = process_parents();
    for session in sessions {
        session.ancestors = ancestors(&parents, session.pid);
        session.ancestor_cwds = session.ancestors.iter().filter_map(|pid| process_cwd(*pid)).collect();
    }
}

/// Whether a spec review belongs to a window with `roots` open. A spec file
/// often lives outside the worktree (in Claude's scratchpad), so the folder
/// that the Claude process behind the session runs in decides. Without this,
/// a spec from one worktree also opens in the windows of other worktrees.
pub fn spec_belongs_to(session: &Session, roots: &[PathBuf]) -> bool {
    let roots: Vec<PathBuf> = roots.iter().map(|root| canonical(root)).collect();
    let inside = |path: &Path| {
        let path = canonical(path);
        roots.iter().any(|root| path.starts_with(root))
    };
    session.target.as_deref().is_some_and(|target| inside(Path::new(target)))
        || session.ancestor_cwds.iter().any(|cwd| inside(cwd))
}

#[cfg(target_os = "macos")]
fn process_cwd(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast::<libc::c_void>(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let path = info.pvi_cdir.vip_path.as_flattened();
    let bytes: Vec<u8> = path.iter().take_while(|byte| **byte != 0).map(|byte| *byte as u8).collect();
    // "/" is the folder of processes started by launchd, such as Zed itself.
    (bytes.len() > 1).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
}

#[cfg(not(target_os = "macos"))]
fn process_cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The parent of each running process, by pid.
fn process_parents() -> HashMap<i32, i32> {
    let Ok(output) = std::process::Command::new("ps").args(["-axo", "pid=,ppid="]).output() else {
        return HashMap::default();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .collect()
}

fn ancestors(parents: &HashMap<i32, i32>, pid: i32) -> Vec<i32> {
    let mut result = Vec::new();
    let mut current = pid;
    while let Some(&parent) = parents.get(&current) {
        if parent <= 1 || result.contains(&parent) {
            break;
        }
        result.push(parent);
        current = parent;
    }
    result
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

/// Resolves links such as macOS `/var` to `/private/var`. A file that is gone
/// still resolves through its folder, so it compares like the roots.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .ok()
        .or_else(|| Some(std::fs::canonicalize(path.parent()?).ok()?.join(path.file_name()?)))
        .unwrap_or_else(|| path.to_path_buf())
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

        let found = find_review_sessions_with(sessions.path(), &[w.to_path_buf()], |pid| pid != 3);
        assert_eq!(found.iter().map(|session| session.pid).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn returns_none_without_a_matching_session() {
        let sessions = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        write(sessions.path(), 1, "review", other.path(), "2026-11-11T10:00:00Z");
        assert!(
            find_review_sessions_with(sessions.path(), &[worktree.path().to_path_buf()], |_| true)
                .is_empty()
        );
    }

    #[test]
    fn spec_belongs_to_the_worktree_of_its_claude_process() {
        let worktree = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let scratchpad = tempfile::tempdir().unwrap();
        let spec = |target: &Path, cwds: Vec<PathBuf>| Session {
            pid: 1,
            url: String::new(),
            mode: "annotate".into(),
            target: Some(target.display().to_string()),
            started_at: String::new(),
            ancestors: Vec::new(),
            ancestor_cwds: cwds,
        };
        let roots = [worktree.path().to_path_buf()];
        let in_scratchpad = scratchpad.path().join("spec.md");
        std::fs::create_dir(worktree.path().join("sub")).unwrap();
        assert!(spec_belongs_to(&spec(&in_scratchpad, vec![worktree.path().join("sub")]), &roots));
        assert!(spec_belongs_to(&spec(&worktree.path().join("spec.md"), Vec::new()), &roots));
        assert!(!spec_belongs_to(&spec(&in_scratchpad, vec![other.path().to_path_buf()]), &roots));
        assert!(!spec_belongs_to(&spec(&in_scratchpad, Vec::new()), &roots));
    }

    #[test]
    fn reads_the_folder_of_this_process() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(process_cwd(std::process::id() as i32).map(|path| canonical(&path)), Some(canonical(&cwd)));
    }

    #[test]
    fn walks_up_the_process_tree() {
        let parents = HashMap::from_iter([(50, 40), (40, 30), (30, 1), (7, 7)]);
        assert_eq!(ancestors(&parents, 50), vec![40, 30]);
        assert_eq!(ancestors(&parents, 7), vec![7]);
        assert!(ancestors(&parents, 99).is_empty());
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
