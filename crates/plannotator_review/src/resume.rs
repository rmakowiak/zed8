//! Resumes Claude Code in restored terminal threads.
//!
//! Every few seconds, each workspace records which Claude Code session runs
//! in each of its terminal threads. Claude Code writes
//! `~/.claude/sessions/<pid>.json` with the session ID of each process.
//! When Zed restores a terminal thread, the Agent panel asks
//! `TerminalResumeSession` for the last session of that thread and runs
//! `claude --resume <session>`.

use agent_ui::{TerminalId, TerminalResumeSession};
use gpui::{App, Context, Window};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use util::ResultExt as _;
use workspace::Workspace;

const RECORD_INTERVAL: Duration = Duration::from_secs(10);

/// Serializes the read-modify-write of the sessions file across windows.
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn sessions_file() -> PathBuf {
    crate::paths::claude_terminal_sessions_file()
}

fn claude_dir() -> PathBuf {
    util::paths::home_dir().join(".claude")
}

pub fn init(cx: &mut App) {
    cx.set_global(TerminalResumeSession(Box::new(|terminal_id| {
        session_to_resume(&sessions_file(), &claude_dir(), terminal_id)
    })));
}

/// Starts the recorder of one workspace. It stops when the workspace closes.
pub fn start_recording(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.spawn_in(window, async move |workspace, cx| {
        loop {
            cx.background_executor().timer(RECORD_INTERVAL).await;
            let Ok(running) = workspace.update(cx, |workspace, cx| running_claude(workspace, cx))
            else {
                break;
            };
            if running.is_empty() {
                continue;
            }
            cx.background_executor()
                .spawn(async move { record(&sessions_file(), &claude_dir(), &running) })
                .await
                .log_err();
        }
    })
    .detach();
}

/// The terminal thread ID and the Claude process ID of each terminal thread
/// that runs Claude Code now.
fn running_claude(workspace: &Workspace, cx: &App) -> Vec<(String, i32)> {
    crate::start::claude_terminals(workspace, false, cx)
        .into_iter()
        .filter_map(|terminal| Some((terminal.thread_id?.to_key_string(), terminal.pid()?)))
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeProcess {
    session_id: String,
}

fn session_of_process(claude_dir: &Path, pid: i32) -> Option<String> {
    let path = claude_dir.join("sessions").join(format!("{pid}.json"));
    let process: ClaudeProcess = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some(process.session_id)
}

fn read_map(file: &Path) -> HashMap<String, String> {
    std::fs::read(file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn record(file: &Path, claude_dir: &Path, running: &[(String, i32)]) -> anyhow::Result<()> {
    let _lock = FILE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut map = read_map(file);
    let mut changed = false;
    for (terminal, pid) in running {
        let Some(session) = session_of_process(claude_dir, *pid) else {
            continue;
        };
        if map.get(terminal) != Some(&session) {
            map.insert(terminal.clone(), session);
            changed = true;
        }
    }
    if changed {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, serde_json::to_vec_pretty(&map)?)?;
    }
    Ok(())
}

/// The recorded session of a terminal thread, if Claude Code still has its
/// transcript. A session with no message has no transcript and cannot resume.
fn session_to_resume(file: &Path, claude_dir: &Path, terminal_id: TerminalId) -> Option<String> {
    let session = read_map(file).remove(&terminal_id.to_key_string())?;
    let transcript = format!("{session}.jsonl");
    let projects = std::fs::read_dir(claude_dir.join("projects")).ok()?;
    projects
        .flatten()
        .any(|project| project.path().join(&transcript).exists())
        .then_some(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_the_session_of_each_claude_process() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::create_dir_all(claude.join("sessions")).unwrap();
        std::fs::write(
            claude.join("sessions/41.json"),
            r#"{"pid":41,"sessionId":"session-a","cwd":"/n8n"}"#,
        )
        .unwrap();
        let file = dir.path().join("zed/sessions.json");

        record(
            &file,
            &claude,
            &[("thread-1".into(), 41), ("thread-2".into(), 42)],
        )
        .unwrap();

        let map = read_map(&file);
        assert_eq!(map.get("thread-1").map(String::as_str), Some("session-a"));
        assert_eq!(map.get("thread-2"), None);
    }

    #[test]
    fn resumes_only_a_session_with_a_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::create_dir_all(claude.join("projects/-n8n")).unwrap();
        std::fs::write(claude.join("projects/-n8n/session-a.jsonl"), "{}").unwrap();
        let file = dir.path().join("sessions.json");
        let id = |key: &str| TerminalId::from_key_string(key).unwrap();
        let with_transcript = id("00000000-0000-4000-8000-000000000001");
        let without_transcript = id("00000000-0000-4000-8000-000000000002");
        let unknown = id("00000000-0000-4000-8000-000000000003");
        std::fs::write(
            &file,
            serde_json::json!({
                with_transcript.to_key_string(): "session-a",
                without_transcript.to_key_string(): "session-b",
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(
            session_to_resume(&file, &claude, with_transcript).as_deref(),
            Some("session-a")
        );
        assert_eq!(session_to_resume(&file, &claude, without_transcript), None);
        assert_eq!(session_to_resume(&file, &claude, unknown), None);
    }
}
