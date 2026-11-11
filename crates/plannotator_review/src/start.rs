//! "Review PR": asks Claude Code, running in a Zed terminal of this worktree,
//! to write a guided review and start a Plannotator session. The skill
//! `zed-guided-review` in `~/.claude/skills/` holds the steps for Claude.

use gpui::{App, Context, Entity, Window, actions};
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
    let Some(terminal) = claude_terminal(workspace, true, cx) else {
        start_claude(workspace, command, window, cx);
        return;
    };
    terminal.update(cx, |terminal, _| terminal.input(command.into_bytes()));
    // Claude Code takes fast input as a paste. A separate Enter submits it.
    cx.spawn_in(window, async move |_, cx| {
        cx.background_executor().timer(Duration::from_millis(200)).await;
        terminal.update(cx, |terminal, _| terminal.input(&b"\r"[..]));
    })
    .detach();
    crate::show_toast(workspace, done_message, cx);
}

/// Starts Claude Code with `command` as its first prompt, in a new terminal
/// of the worktree. Used when no Claude terminal is open.
fn start_claude(
    workspace: &mut Workspace,
    command: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = workspace.panel::<TerminalPanel>(cx) else {
        crate::show_toast(workspace, "Open Claude Code in a terminal of this worktree first.", cx);
        return;
    };
    let task = SpawnInTerminal {
        label: "Claude Code".to_string(),
        full_label: format!("claude {command}"),
        command_label: format!("claude {command}"),
        command: Some("claude".to_string()),
        args: vec![command],
        cwd: crate::worktree_roots(workspace, cx).into_iter().next(),
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Always,
        ..SpawnInTerminal::default()
    };
    let spawn = panel.update(cx, |panel, cx| panel.spawn_task(&task, window, cx));
    cx.spawn(async move |_, _| spawn.await.log_err()).detach();
    crate::show_toast(workspace, "Started Claude Code in a new terminal with the review request.", cx);
}

/// A terminal that runs Claude Code: the terminal thread open in the Agent
/// panel, a terminal panel tab, or a center pane. One whose working directory
/// is inside the worktree comes first.
///
/// With `live`, each terminal's foreground process is also read with `ps`,
/// because Zed's cached process info can lag behind. Rendering skips that.
fn claude_terminal(workspace: &Workspace, live: bool, cx: &App) -> Option<Entity<Terminal>> {
    let roots = crate::worktree_roots(workspace, cx);
    let mut views: Vec<Entity<TerminalView>> = Vec::new();
    if let Some(agent_panel) = workspace.panel::<agent_ui::AgentPanel>(cx)
        && let Some(view) = agent_panel.read(cx).visible_terminal_view()
    {
        views.push(view.clone());
    }
    views.extend(workspace.items_of_type::<TerminalView>(cx));
    if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
        for pane in panel.read(cx).panes() {
            views.extend(pane.read(cx).items_of_type::<TerminalView>());
        }
    }
    let terminals: Vec<Entity<Terminal>> = views
        .into_iter()
        .map(|view| view.read(cx).terminal().clone())
        .filter(|terminal| {
            let terminal = terminal.read(cx);
            let cached = terminal.foreground_process_command_name();
            let live = live.then(|| terminal.pid().and_then(|pid| process_name(pid.as_u32()))).flatten();
            log::debug!("plannotator: terminal foreground process: cached {cached:?}, live {live:?}");
            [cached, live]
                .iter()
                .flatten()
                .any(|name| name.contains("claude"))
        })
        .collect();
    let in_worktree = |terminal: &Entity<Terminal>| {
        terminal
            .read(cx)
            .working_directory()
            .is_some_and(|dir| roots.iter().any(|root| dir.starts_with(root)))
    };
    terminals
        .iter()
        .find(|terminal| in_worktree(terminal))
        .or(terminals.first())
        .cloned()
}

/// The title that Claude Code gives its terminal, such as the thread name.
pub(crate) fn claude_thread_title(workspace: &Workspace, cx: &App) -> Option<String> {
    let terminal = claude_terminal(workspace, false, cx)?;
    let terminal = terminal.read(cx);
    let title = if terminal.breadcrumb_text.is_empty() {
        terminal.title(true)
    } else {
        terminal.breadcrumb_text.clone()
    };
    // Claude Code puts a spinner or a mark before its title.
    let title = title.trim_start_matches(|c: char| !c.is_alphanumeric()).trim().to_string();
    (!title.is_empty()).then_some(title)
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
