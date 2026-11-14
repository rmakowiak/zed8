//! Zed8: the state of each Claude Code terminal thread in the threads
//! sidebar: waits on you, running, stuck or done, with its tokens.
//!
//! Every few seconds each workspace reads, for each terminal thread of its
//! Agent panel, the Claude Code process file (`~/.claude/sessions/<pid>.json`,
//! busy or idle) and the new lines of the session transcript. The Review
//! panel reports Plannotator gates with [`set_gates`].

mod board_ui;
mod transcript;

pub use board_ui::{BoardGroup, BoardThread, decorate_row, footer, heading_badges};

use agent_ui::{AgentPanel, TerminalId};
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate, Utc};
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, SharedString, WeakEntity, Window,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use terminal::Terminal;
use transcript::{Spend, ToolCall, Transcript};
use util::ResultExt as _;
use workspace::Workspace;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// A busy thread whose transcript stays the same this long counts as stuck.
const STUCK_AFTER_MINUTES: i64 = 5;
/// Tool errors in a row that count as stuck.
const STUCK_ERROR_STREAK: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThreadState {
    Waiting,
    Running,
    Stuck,
    Done,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Period {
    #[default]
    Today,
    Week,
}

#[derive(Clone, Debug, PartialEq)]
struct ProcessState {
    busy: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadSnapshot {
    pub session_id: String,
    running: Option<ProcessState>,
    pub transcript_path: Option<PathBuf>,
    /// Main thread and subagents together.
    pub spend: Spend,
    pub daily: BTreeMap<NaiveDate, Spend>,
    pub model: Option<String>,
    pub context_tokens: u64,
    pub last_prompt: Option<(String, DateTime<Utc>)>,
    pub activity: Vec<ToolCall>,
    pub open_tool: Option<ToolCall>,
    pub error_streak: u32,
    pub last_error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
}

/// What a row shows for a thread at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadView {
    pub state: ThreadState,
    pub line: SharedString,
    /// How long the thread has been in this state.
    pub elapsed: Option<SharedString>,
}

#[derive(Default)]
struct TranscriptCache {
    sessions: HashMap<String, SessionFiles>,
}

#[derive(Default)]
struct SessionFiles {
    path: Option<PathBuf>,
    main: Transcript,
    subagents: HashMap<PathBuf, Transcript>,
}

pub struct ThreadBoard {
    threads: HashMap<TerminalId, ThreadSnapshot>,
    terminals: HashMap<TerminalId, (WeakEntity<Terminal>, WeakEntity<Workspace>)>,
    gates: HashMap<EntityId, HashMap<TerminalId, SharedString>>,
    gate_since: HashMap<(TerminalId, SharedString), DateTime<Utc>>,
    cache: Arc<Mutex<TranscriptCache>>,
    pub period: Period,
}

struct GlobalThreadBoard(Entity<ThreadBoard>);

impl Global for GlobalThreadBoard {}

pub fn init(cx: &mut App) {
    let board = cx.new(|_| ThreadBoard {
        threads: HashMap::default(),
        terminals: HashMap::default(),
        gates: HashMap::default(),
        gate_since: HashMap::default(),
        cache: Arc::default(),
        period: Period::default(),
    });
    cx.set_global(GlobalThreadBoard(board));
    cx.observe_new(|_workspace: &mut Workspace, window, cx| {
        if let Some(window) = window {
            start_polling(window, cx);
        }
    })
    .detach();
}

/// The Review panel `source` reports the gates that wait now, by thread.
pub fn set_gates(source: EntityId, gates: Vec<(TerminalId, SharedString)>, cx: &mut App) {
    let Some(board) = ThreadBoard::global(cx) else {
        return;
    };
    board.update(cx, |board, cx| {
        let gates: HashMap<_, _> = gates.into_iter().collect();
        if board.gates.get(&source) == Some(&gates) {
            return;
        }
        let now = Utc::now();
        for (terminal_id, label) in &gates {
            board
                .gate_since
                .entry((*terminal_id, label.clone()))
                .or_insert(now);
        }
        board.gates.insert(source, gates);
        let waiting: Vec<_> = board
            .gates
            .values()
            .flat_map(|gates| gates.iter().map(|(id, label)| (*id, label.clone())))
            .collect();
        board.gate_since.retain(|key, _| waiting.contains(key));
        cx.notify();
    });
}

impl ThreadBoard {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalThreadBoard>()
            .map(|global| global.0.clone())
    }

    pub fn snapshot(&self, terminal_id: TerminalId) -> Option<&ThreadSnapshot> {
        self.threads.get(&terminal_id)
    }

    pub fn view(
        &self,
        terminal_id: TerminalId,
        notified: bool,
        now: DateTime<Utc>,
    ) -> Option<ThreadView> {
        let snapshot = self.threads.get(&terminal_id)?;
        let gate = self
            .gates
            .values()
            .find_map(|gates| gates.get(&terminal_id));
        if let Some(label) = gate {
            let since = self.gate_since.get(&(terminal_id, label.clone())).copied();
            return Some(ThreadView::plain(
                ThreadState::Waiting,
                format!("Waits on you · {label}"),
                since.map(|since| short_duration(now - since)),
            ));
        }
        let busy = snapshot
            .running
            .as_ref()
            .is_some_and(|process| process.busy);
        if !busy {
            let line = match snapshot.last_event_at {
                Some(at) => format!("Done {}", ago(now - at)),
                None => "No messages yet".to_string(),
            };
            return Some(ThreadView::plain(ThreadState::Done, line, None));
        }
        if let Some(tool) = &snapshot.open_tool {
            if tool.name == "AskUserQuestion" {
                return Some(ThreadView::plain(
                    ThreadState::Waiting,
                    "Waits on you · Question".to_string(),
                    Some(short_duration(now - tool.at)),
                ));
            }
            // Claude Code rings the terminal bell when a tool call waits for
            // your permission.
            if notified {
                return Some(ThreadView::plain(
                    ThreadState::Waiting,
                    format!("Allow {}", tool.summary),
                    Some(short_duration(now - tool.at)),
                ));
            }
        }
        let turn_elapsed = snapshot
            .last_prompt
            .as_ref()
            .map(|(_, at)| short_duration(now - *at));
        if snapshot.error_streak >= STUCK_ERROR_STREAK {
            return Some(ThreadView::plain(
                ThreadState::Stuck,
                format!("Stuck · same error {} times", snapshot.error_streak),
                turn_elapsed,
            ));
        }
        if let Some(last) = snapshot.last_event_at
            && now - last >= ChronoDuration::minutes(STUCK_AFTER_MINUTES)
        {
            return Some(ThreadView::plain(
                ThreadState::Stuck,
                format!("Stuck · no output for {}", short_duration(now - last)),
                turn_elapsed,
            ));
        }
        let line = snapshot
            .open_tool
            .as_ref()
            .map(|tool| tool.summary.clone())
            .unwrap_or_else(|| "Thinking".to_string());
        Some(ThreadView::plain(ThreadState::Running, line, turn_elapsed))
    }

    /// Spend per thread in the chosen period.
    pub fn period_spend(&self, terminal_id: TerminalId, period: Period) -> Spend {
        let Some(snapshot) = self.threads.get(&terminal_id) else {
            return Spend::default();
        };
        let today = Local::now().date_naive();
        let first_day = match period {
            Period::Today => today,
            Period::Week => today - ChronoDuration::days(6),
        };
        let mut spend = Spend::default();
        for (_, day) in snapshot.daily.range(first_day..) {
            spend.add(day);
        }
        spend
    }

    fn apply(
        &mut self,
        results: Vec<(TerminalId, Option<ThreadSnapshot>)>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for (terminal_id, snapshot) in results {
            match snapshot {
                Some(snapshot) => {
                    if self.threads.get(&terminal_id) != Some(&snapshot) {
                        self.threads.insert(terminal_id, snapshot);
                        changed = true;
                    }
                }
                None => changed |= self.threads.remove(&terminal_id).is_some(),
            }
        }
        // Elapsed times and the stuck state move on without new data.
        let any_busy = self.threads.values().any(|snapshot| {
            snapshot
                .running
                .as_ref()
                .is_some_and(|process| process.busy)
        });
        if changed || any_busy {
            cx.notify();
        }
    }
}

impl ThreadView {
    fn plain(state: ThreadState, line: String, elapsed: Option<SharedString>) -> Self {
        Self {
            state,
            line: line.into(),
            elapsed,
        }
    }
}

/// "14m", "2h", "3d".
pub fn short_duration(duration: ChronoDuration) -> SharedString {
    let minutes = duration.num_minutes().max(1);
    if minutes < 60 {
        format!("{minutes}m").into()
    } else if minutes < 60 * 24 {
        format!("{}h", minutes / 60).into()
    } else {
        format!("{}d", minutes / (60 * 24)).into()
    }
}

/// "just now", "12m ago", "2h ago", "yesterday", "3d ago".
pub fn ago(duration: ChronoDuration) -> String {
    let minutes = duration.num_minutes();
    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 60 * 24 {
        format!("{}h ago", minutes / 60)
    } else if minutes < 60 * 48 {
        "yesterday".to_string()
    } else {
        format!("{}d ago", minutes / (60 * 24))
    }
}

/// "1.4M", "412k", "950".
pub fn format_tokens(tokens: u64) -> SharedString {
    if tokens >= 1_000_000 {
        let millions = format!("{:.1}", tokens as f64 / 1_000_000.0);
        format!("{}M", millions.trim_end_matches(".0")).into()
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000).into()
    } else {
        tokens.to_string().into()
    }
}

struct Probe {
    terminal_id: TerminalId,
    /// The Claude process, when one runs in the terminal now.
    pid: Option<i32>,
}

fn start_polling(window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(board) = ThreadBoard::global(cx).map(|board| board.downgrade()) else {
        return;
    };
    cx.spawn_in(window, async move |workspace, cx| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let Ok(probes) = workspace.update(cx, |workspace, cx| {
                let probes = probe_terminals(workspace, cx);
                if let Some(board) = board.upgrade() {
                    let weak_workspace = cx.entity().downgrade();
                    board.update(cx, |board, _| {
                        for (terminal_id, terminal) in &probes.1 {
                            board
                                .terminals
                                .insert(*terminal_id, (terminal.clone(), weak_workspace.clone()));
                        }
                    });
                }
                probes.0
            }) else {
                break;
            };
            if probes.is_empty() {
                continue;
            }
            let Ok(cache) = board.read_with(cx, |board, _| board.cache.clone()) else {
                break;
            };
            let results = cx
                .background_spawn(async move {
                    read_threads(&cache, &probes, &claude_dir(), &sessions_map_file())
                })
                .await;
            if board
                .update(cx, |board, cx| board.apply(results, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

type TerminalHandles = Vec<(TerminalId, WeakEntity<Terminal>)>;

fn probe_terminals(workspace: &Workspace, cx: &App) -> (Vec<Probe>, TerminalHandles) {
    let Some(agent_panel) = workspace.panel::<AgentPanel>(cx) else {
        return (Vec::new(), Vec::new());
    };
    let agent_panel = agent_panel.read(cx);
    let mut probes = Vec::new();
    let mut handles = Vec::new();
    for info in agent_panel.terminals(cx) {
        let Some(view) = agent_panel.terminal_view(info.id) else {
            continue;
        };
        let terminal = view.read(cx).terminal().clone();
        let state = terminal.read(cx);
        let runs_claude = state
            .foreground_process_command_name()
            .is_some_and(|name| name.contains("claude"));
        let pid = runs_claude
            .then(|| state.pid().map(|pid| pid.as_u32() as i32))
            .flatten();
        probes.push(Probe {
            terminal_id: info.id,
            pid,
        });
        handles.push((info.id, terminal.downgrade()));
    }
    (probes, handles)
}

fn claude_dir() -> PathBuf {
    util::paths::home_dir().join(".claude")
}

/// Written by the Review crate's session recorder, so that a thread whose
/// Claude has exited still shows its last session.
fn sessions_map_file() -> PathBuf {
    util::paths::home_dir()
        .join(".plannotator")
        .join("zed-claude-terminal-sessions.json")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeProcess {
    session_id: String,
    status: Option<String>,
}

fn read_process(claude_dir: &Path, pid: i32) -> Option<ClaudeProcess> {
    let path = claude_dir.join("sessions").join(format!("{pid}.json"));
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn find_transcript(claude_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    std::fs::read_dir(claude_dir.join("projects"))
        .ok()?
        .flatten()
        .map(|project| project.path().join(&file_name))
        .find(|path| path.exists())
}

fn read_threads(
    cache: &Mutex<TranscriptCache>,
    probes: &[Probe],
    claude_dir: &Path,
    map_file: &Path,
) -> Vec<(TerminalId, Option<ThreadSnapshot>)> {
    let recorded: HashMap<String, String> = std::fs::read(map_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    probes
        .iter()
        .map(|probe| {
            let process = probe.pid.and_then(|pid| read_process(claude_dir, pid));
            let session_id = process
                .as_ref()
                .map(|process| process.session_id.clone())
                .or_else(|| recorded.get(&probe.terminal_id.to_key_string()).cloned());
            let snapshot = session_id.map(|session_id| {
                let running = process.map(|process| ProcessState {
                    busy: process.status.as_deref() == Some("busy"),
                });
                snapshot_of(&mut cache, claude_dir, session_id, running)
            });
            (probe.terminal_id, snapshot)
        })
        .collect()
}

fn snapshot_of(
    cache: &mut TranscriptCache,
    claude_dir: &Path,
    session_id: String,
    running: Option<ProcessState>,
) -> ThreadSnapshot {
    let files = cache.sessions.entry(session_id.clone()).or_default();
    if files.path.is_none() {
        files.path = find_transcript(claude_dir, &session_id);
    }
    let mut spend = Spend::default();
    let mut daily = BTreeMap::new();
    if let Some(path) = &files.path {
        files.main.read_new(path).log_err();
        spend.add(&files.main.spend);
        daily = files.main.daily.clone();
        let subagents_dir = path.with_extension("").join("subagents");
        if let Ok(entries) = std::fs::read_dir(&subagents_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                    continue;
                }
                let subagent = files.subagents.entry(path.clone()).or_default();
                subagent.read_new(&path).log_err();
            }
        }
        for subagent in files.subagents.values() {
            spend.add(&subagent.spend);
            for (day, day_spend) in &subagent.daily {
                daily
                    .entry(*day)
                    .or_insert_with(Spend::default)
                    .add(day_spend);
            }
        }
    }
    let main = &files.main;
    ThreadSnapshot {
        session_id,
        running,
        transcript_path: files.path.clone(),
        spend,
        daily,
        model: main.model.clone(),
        context_tokens: main.context_tokens,
        last_prompt: main.last_prompt.clone(),
        activity: main.activity.iter().cloned().collect(),
        open_tool: main.open_tool().cloned(),
        error_streak: main.error_streak,
        last_error: main.last_error.clone(),
        started_at: main.started_at,
        last_event_at: main.last_event_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn board_with(snapshot: ThreadSnapshot) -> (ThreadBoard, TerminalId) {
        let terminal_id =
            TerminalId::from_key_string("00000000-0000-4000-8000-000000000001").unwrap();
        let mut threads = HashMap::default();
        threads.insert(terminal_id, snapshot);
        (
            ThreadBoard {
                threads,
                terminals: HashMap::default(),
                gates: HashMap::default(),
                gate_since: HashMap::default(),
                cache: Arc::default(),
                period: Period::default(),
            },
            terminal_id,
        )
    }

    fn snapshot(
        busy: Option<bool>,
        last_event_minutes_ago: i64,
        open_tool: Option<&str>,
    ) -> ThreadSnapshot {
        let now = Utc::now();
        ThreadSnapshot {
            session_id: "s".into(),
            running: busy.map(|busy| ProcessState { busy }),
            transcript_path: None,
            spend: Spend::default(),
            daily: BTreeMap::new(),
            model: None,
            context_tokens: 0,
            last_prompt: Some(("go".into(), now - ChronoDuration::minutes(30))),
            activity: Vec::new(),
            open_tool: open_tool.map(|name| ToolCall {
                id: "t1".into(),
                name: name.into(),
                summary: format!("{name} pnpm test"),
                at: now - ChronoDuration::minutes(last_event_minutes_ago),
            }),
            error_streak: 0,
            last_error: None,
            started_at: None,
            last_event_at: Some(now - ChronoDuration::minutes(last_event_minutes_ago)),
        }
    }

    #[test]
    fn picks_the_state_of_a_thread() {
        let state = |snapshot, notified| {
            let (board, id) = board_with(snapshot);
            board
                .view(id, notified, Utc::now())
                .map(|view| (view.state, view.line.to_string()))
        };
        assert_eq!(
            state(snapshot(Some(true), 0, Some("Bash")), false),
            Some((ThreadState::Running, "Bash pnpm test".into()))
        );
        assert_eq!(
            state(snapshot(Some(true), 0, Some("Bash")), true),
            Some((ThreadState::Waiting, "Allow Bash pnpm test".into()))
        );
        assert_eq!(
            state(snapshot(Some(true), 9, Some("Bash")), false),
            Some((ThreadState::Stuck, "Stuck · no output for 9m".into()))
        );
        assert_eq!(
            state(snapshot(Some(false), 40, None), false),
            Some((ThreadState::Done, "Done 40m ago".into()))
        );
        assert_eq!(
            state(snapshot(None, 60 * 30, None), false),
            Some((ThreadState::Done, "Done yesterday".into()))
        );
        assert_eq!(
            state(snapshot(Some(true), 0, Some("AskUserQuestion")), false).map(|(state, _)| state),
            Some(ThreadState::Waiting)
        );
    }

    #[test]
    fn reads_running_and_recorded_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        std::fs::create_dir_all(claude.join("sessions")).unwrap();
        std::fs::create_dir_all(claude.join("projects/-n8n/session-a/subagents")).unwrap();
        std::fs::write(
            claude.join("sessions/41.json"),
            r#"{"sessionId":"session-a","status":"busy"}"#,
        )
        .unwrap();
        let line = |id: &str| {
            json!({"type": "assistant", "timestamp": "2026-11-14T08:00:00Z", "message": {"id": id, "model": "claude-opus-5-5", "content": [], "usage": {"input_tokens": 10, "output_tokens": 5}}}).to_string() + "\n"
        };
        std::fs::write(claude.join("projects/-n8n/session-a.jsonl"), line("m1")).unwrap();
        std::fs::write(
            claude.join("projects/-n8n/session-a/subagents/agent-1.jsonl"),
            line("m2"),
        )
        .unwrap();
        let map = dir.path().join("map.json");
        let recorded = TerminalId::from_key_string("00000000-0000-4000-8000-000000000002").unwrap();
        std::fs::write(
            &map,
            json!({recorded.to_key_string(): "session-a"}).to_string(),
        )
        .unwrap();
        let running = TerminalId::from_key_string("00000000-0000-4000-8000-000000000001").unwrap();
        let plain = TerminalId::from_key_string("00000000-0000-4000-8000-000000000003").unwrap();

        let cache = Mutex::new(TranscriptCache::default());
        let results = read_threads(
            &cache,
            &[
                Probe {
                    terminal_id: running,
                    pid: Some(41),
                },
                Probe {
                    terminal_id: recorded,
                    pid: None,
                },
                Probe {
                    terminal_id: plain,
                    pid: None,
                },
            ],
            &claude,
            &map,
        );

        let running_snapshot = results[0].1.as_ref().unwrap();
        assert_eq!(running_snapshot.running, Some(ProcessState { busy: true }));
        assert_eq!(running_snapshot.spend.tokens.total(), 30);
        assert_eq!(
            results[1]
                .1
                .as_ref()
                .map(|snapshot| snapshot.running.clone()),
            Some(None)
        );
        assert!(results[2].1.is_none());
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(format_tokens(1_400_000).as_ref(), "1.4M");
        assert_eq!(format_tokens(412_345).as_ref(), "412k");
        assert_eq!(format_tokens(950).as_ref(), "950");
        assert_eq!(format_tokens(1_000_000).as_ref(), "1M");
        assert_eq!(short_duration(ChronoDuration::minutes(36)).as_ref(), "36m");
        assert_eq!(short_duration(ChronoDuration::minutes(130)).as_ref(), "2h");
    }
}
