//! "Review PR": asks Claude Code, running in a Zed terminal of this worktree,
//! to write a guided review and start a Plannotator session. The skill
//! `zed-guided-review` in `~/.claude/skills/` holds the steps for Claude.

use gpui::{App, Context, Entity, Window, actions};
use serde::Deserialize;
use std::{path::Path, time::Duration};
use terminal::Terminal;
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
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
}

/// The open GitHub PR of the checked-out branch, from the `gh` CLI.
pub fn find_pull_request(root: &Path) -> Option<PullRequest> {
    let output = std::process::Command::new("gh")
        .current_dir(root)
        .args(["pr", "view", "--json", "number,url,title"])
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
    let roots: Vec<_> = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect();
    let Some(terminal) = claude_terminal(workspace, &roots, cx) else {
        crate::show_toast(
            workspace,
            "Open Claude Code in the Agent panel or in a terminal of this worktree first. Zed asks it there.",
            cx,
        );
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

/// A terminal that runs Claude Code: the terminal thread open in the Agent
/// panel, a terminal panel tab, or a center pane. One whose working directory
/// is inside the worktree comes first.
fn claude_terminal(workspace: &Workspace, roots: &[std::path::PathBuf], cx: &App) -> Option<Entity<Terminal>> {
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
            let live = terminal.pid().and_then(|pid| process_name(pid.as_u32()));
            log::info!("plannotator: terminal foreground process: cached {cached:?}, live {live:?}");
            [cached, live]
                .iter()
                .flatten()
                .any(|name| name.contains("claude"))
        })
        .collect();
    log::info!("plannotator: {} Claude terminals found", terminals.len());
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
