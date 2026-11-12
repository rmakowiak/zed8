//! "Review PR": asks Claude Code, running in a Zed terminal of this worktree,
//! to write a guided review and start a Plannotator session. Also finds the
//! Claude terminals, and the one behind each session. The skill
//! `zed-guided-review` in `~/.claude/skills/` holds the steps for Claude.

use crate::{panel::ReviewPanel, session::Session};
use agent_ui::TerminalId;
use gpui::{App, Context, Entity, EntityId, Window, actions};
use serde::Deserialize;
use std::{path::Path, time::Duration};
use terminal::Terminal;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use task::{RevealStrategy, SpawnInTerminal};
use util::ResultExt as _;
use workspace::Workspace;

actions!(
    plannotator_review,
    [
        /// Asks Claude Code in a terminal to write a guided review of this branch's PR.
        ReviewPr,
        /// Asks Claude Code in a terminal to start a review session for the saved guide.
        StartSession
    ]
);

/// What Claude Code receives in the terminal.
const REVIEW_COMMAND: &str = "/zed-guided-review";

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub title: String,
    /// The last commit of the PR on GitHub.
    pub head_ref_oid: String,
}

/// The open GitHub PR of the checked-out branch, from the `gh` CLI.
pub fn find_pull_request(root: &Path) -> Option<PullRequest> {
    let output = std::process::Command::new("gh")
        .current_dir(root)
        .args(["pr", "view", "--json", "number,url,title,headRefOid"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn review_pr(workspace: &mut Workspace, _: &ReviewPr, window: &mut Window, cx: &mut Context<Workspace>) {
    ask_claude(
        workspace,
        REVIEW_COMMAND.to_string(),
        "Asked Claude for a guided review. The guide shows here when Claude imports it.",
        window,
        cx,
    );
}

fn start_session(
    workspace: &mut Workspace,
    _: &StartSession,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    ask_claude(
        workspace,
        format!("{REVIEW_COMMAND} session"),
        "Asked Claude to start a review session for the saved guide.",
        window,
        cx,
    );
}

fn ask_claude(
    workspace: &mut Workspace,
    command: String,
    done_message: &'static str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(target) = target_terminal(workspace, true, cx) else {
        start_claude(workspace, Some(command), window, cx);
        return;
    };
    let terminal = target.terminal.clone();
    terminal.update(cx, |terminal, _| terminal.input(command.into_bytes()));
    // Claude Code takes fast input as a paste. A separate Enter submits it.
    cx.spawn_in(window, async move |_, cx| {
        cx.background_executor().timer(Duration::from_millis(200)).await;
        terminal.update(cx, |terminal, _| terminal.input(&b"\r"[..]));
    })
    .detach();
    crate::show_toast(workspace, format!("{done_message} Thread: {}.", target.title), cx);
}

/// Starts Claude Code in a new terminal of the worktree, with `command` as
/// its first prompt if there is one.
pub(crate) fn start_claude(
    workspace: &mut Workspace,
    command: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<TerminalPanel>(cx) else {
        crate::show_toast(workspace, "Open Claude Code in a terminal of this worktree first.", cx);
        return;
    };
    let full_label = match &command {
        Some(command) => format!("claude {command}"),
        None => "claude".to_string(),
    };
    let task = SpawnInTerminal {
        label: "Claude Code".to_string(),
        full_label: full_label.clone(),
        command_label: full_label,
        command: Some("claude".to_string()),
        args: command.into_iter().collect(),
        cwd: crate::worktree_roots(workspace, cx).into_iter().next(),
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Always,
        ..SpawnInTerminal::default()
    };
    let spawn = panel.update(cx, |panel, cx| panel.spawn_task(&task, window, cx));
    cx.spawn(async move |_, _| spawn.await.log_err()).detach();
    crate::show_toast(workspace, "Started Claude Code in a new terminal.", cx);
}

/// A terminal that runs Claude Code.
#[derive(Clone)]
pub(crate) struct ClaudeTerminal {
    pub terminal: Entity<Terminal>,
    /// The terminal thread in the Agent panel, if the terminal is one.
    pub thread_id: Option<TerminalId>,
    /// The thread name, from the terminal title.
    pub title: String,
    /// Claude is working. Claude Code puts ✳ before the title when it is
    /// idle, and a spinner frame while it works.
    pub busy: bool,
    /// The foreground process, which is Claude itself.
    pid: Option<i32>,
}

impl ClaudeTerminal {
    pub fn id(&self) -> EntityId {
        self.terminal.entity_id()
    }

    /// Claude in this terminal started `session`: Plannotator runs below it
    /// in the process tree.
    pub fn owns(&self, session: &Session) -> bool {
        self.pid.is_some_and(|pid| session.ancestors.contains(&pid))
    }
}

/// The terminals that run Claude Code: terminal threads of the Agent panel
/// (the visible one first), terminal panel tabs and center panes. Terminals
/// whose working directory is inside the worktree come first.
///
/// With `live`, each terminal's foreground process is also read with `ps`,
/// because Zed's cached process info can lag behind. Rendering skips that.
pub(crate) fn claude_terminals(workspace: &Workspace, live: bool, cx: &App) -> Vec<ClaudeTerminal> {
    let roots = crate::worktree_roots(workspace, cx);
    let mut views: Vec<(Entity<TerminalView>, Option<TerminalId>)> = Vec::new();
    if let Some(agent_panel) = workspace.panel::<agent_ui::AgentPanel>(cx) {
        let agent_panel = agent_panel.read(cx);
        let visible = agent_panel.visible_terminal_view().map(|view| view.entity_id());
        let mut threads: Vec<_> = agent_panel
            .terminals(cx)
            .into_iter()
            .filter_map(|info| Some((agent_panel.terminal_view(info.id)?.clone(), Some(info.id))))
            .collect();
        threads.sort_by_key(|(view, _)| Some(view.entity_id()) != visible);
        views.extend(threads);
    }
    views.extend(workspace.items_of_type::<TerminalView>(cx).map(|view| (view, None)));
    if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
        for pane in panel.read(cx).panes() {
            views.extend(pane.read(cx).items_of_type::<TerminalView>().map(|view| (view, None)));
        }
    }
    let mut seen = Vec::new();
    let mut terminals: Vec<(bool, ClaudeTerminal)> = Vec::new();
    for (view, thread_id) in views {
        let terminal = view.read(cx).terminal().clone();
        if seen.contains(&terminal.entity_id()) {
            continue;
        }
        seen.push(terminal.entity_id());
        let state = terminal.read(cx);
        let pid = state.pid().map(|pid| pid.as_u32());
        let cached = state.foreground_process_command_name();
        let live_name = live.then(|| pid.and_then(process_name)).flatten();
        if ![cached, live_name].iter().flatten().any(|name| name.contains("claude")) {
            continue;
        }
        let in_worktree = state
            .working_directory()
            .is_some_and(|dir| roots.iter().any(|root| dir.starts_with(root)));
        let raw = if state.breadcrumb_text.is_empty() {
            state.title(true)
        } else {
            state.breadcrumb_text.clone()
        };
        let raw = raw.trim();
        let mark = raw.chars().next().filter(|c| !c.is_alphanumeric());
        let title = raw.trim_start_matches(|c: char| !c.is_alphanumeric()).trim().to_string();
        terminals.push((
            in_worktree,
            ClaudeTerminal {
                terminal: terminal.clone(),
                thread_id,
                title: if title.is_empty() { "Claude Code".to_string() } else { title },
                busy: mark.is_some_and(|mark| mark != '✳'),
                pid: pid.map(|pid| pid as i32),
            },
        ));
    }
    terminals.sort_by_key(|(in_worktree, _)| !in_worktree);
    terminals.into_iter().map(|(_, terminal)| terminal).collect()
}

/// The thread that gets "Review PR" and "Start session", and whose working
/// state the panel shows: the thread that started the current review
/// session, else the thread picked in the panel, else the first one.
pub(crate) fn pick_terminal<'a>(
    terminals: &'a [ClaudeTerminal],
    session: Option<&Session>,
    chosen: Option<EntityId>,
) -> Option<&'a ClaudeTerminal> {
    session
        .and_then(|session| terminals.iter().find(|terminal| terminal.owns(session)))
        .or_else(|| chosen.and_then(|id| terminals.iter().find(|terminal| terminal.id() == id)))
        .or(terminals.first())
}

/// `pick_terminal` with the choices of this workspace's Review panel.
fn target_terminal(workspace: &Workspace, live: bool, cx: &App) -> Option<ClaudeTerminal> {
    let terminals = claude_terminals(workspace, live, cx);
    let panel = workspace.panel::<ReviewPanel>(cx);
    let panel = panel.as_ref().map(|panel| panel.read(cx));
    pick_terminal(
        &terminals,
        panel.and_then(|panel| panel.session()),
        panel.and_then(|panel| panel.chosen_thread()),
    )
    .cloned()
}

/// Shows a terminal thread in the Agent panel, or a terminal tab.
pub(crate) fn open_terminal(
    workspace: &mut Workspace,
    target: &ClaudeTerminal,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(thread_id) = target.thread_id
        && let Some(agent_panel) = workspace.panel::<agent_ui::AgentPanel>(cx)
    {
        agent_panel.update(cx, |panel, cx| panel.activate_terminal(thread_id, true, window, cx));
        workspace.focus_panel::<agent_ui::AgentPanel>(window, cx);
        return;
    }
    let item = workspace
        .items_of_type::<TerminalView>(cx)
        .find(|view| view.read(cx).terminal().entity_id() == target.id());
    match item {
        Some(view) => {
            workspace.activate_item(&view, true, true, window, cx);
        }
        None => {
            workspace.focus_panel::<TerminalPanel>(window, cx);
        }
    }
}

/// The command name of a running process, read with `ps`. Zed's cached
/// process info can lag behind the terminal.
fn process_name(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let name = name.rsplit('/').next().unwrap_or(&name).to_string();
    (!name.is_empty()).then_some(name)
}

pub fn init(workspace: &mut Workspace) {
    workspace.register_action(review_pr).register_action(start_session);
}
