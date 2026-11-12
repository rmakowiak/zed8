//! Plannotator review inside Zed (personal fork).
//!
//! "Send review" in a diff view posts the inline review comments to the
//! Plannotator review session of this worktree. The agent that started the
//! session (Claude Code in the terminal) receives them as review feedback.
//! The Review panel shows the saved guide of the branch with its chapters.

mod chapter_view;
mod client;
mod feedback;
mod guide;
mod input;
mod note_thread;
mod panel;
mod panel_state;
mod paths;
mod rich_text;
mod session;
mod spec_doc;
mod spec_store;
mod spec_view;
mod start;

use anyhow::Result;
use editor::{Editor, ExportedReviewComment, actions::SendReviewToAgent};
use git_ui::{branch_diff::BranchDiff, project_diff::ProjectDiff};
use gpui::{App, AppContext as _, Context, Entity, Window};
use http_client::{HttpClient, Method};
use notifications::status_toast::StatusToast;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use util::ResultExt as _;
use workspace::Workspace;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        start::init(workspace);
        workspace
            .register_action(send_review)
            .register_action(|workspace, _: &panel::ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<panel::ReviewPanel>(window, cx);
            });
        let Some(window) = window else {
            return;
        };
        let review_panel = cx.new(|cx| panel::ReviewPanel::new(workspace, window, cx));
        workspace.add_panel(review_panel, window, cx);
    })
    .detach();
}

fn send_review(
    workspace: &mut Workspace,
    _: &SendReviewToAgent,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = active_diff_editor(workspace, cx) else {
        return;
    };
    let roots = worktree_roots(workspace, cx);

    // Look up the session before taking the comments, so nothing is lost
    // when no session is running.
    // The session picked in the Review panel, else the newest one.
    let session = workspace
        .panel::<panel::ReviewPanel>(cx)
        .and_then(|panel| panel.read(cx).session().cloned())
        .or_else(|| session::find_review_sessions(&paths::sessions_dir(), &roots).into_iter().next());
    let Some(session) = session else {
        show_toast(
            workspace,
            "No Plannotator review for this worktree. Ask Claude to start one.",
            cx,
        );
        return;
    };

    let mut comments = editor.update(cx, |editor, cx| editor.take_review_comments_for_export(cx));
    if let Some(chapter) = workspace
        .active_item(cx)
        .and_then(|item| item.downcast::<chapter_view::ChapterDiffView>())
    {
        comments.extend(chapter.update(cx, |chapter, cx| chapter.take_replies_for_export(cx)));
    }
    if comments.is_empty() {
        return;
    }
    let worktree = roots
        .first()
        .map(|root| root.display().to_string())
        .unwrap_or_default();
    let http: Arc<dyn HttpClient> = workspace.client().http_client();
    let count = comments.len();

    cx.spawn_in(window, async move |workspace, cx| {
        let result = post_feedback(http, &session.url, &comments, &worktree).await;
        if result.is_ok() {
            workspace
                .update(cx, |workspace, cx| {
                    if let Some(panel) = workspace.panel::<panel::ReviewPanel>(cx) {
                        let at = chrono::Local::now().format("%H:%M");
                        let sent = panel::CodeSent {
                            message: format!("Sent {count} comments at {at}"),
                            approved: false,
                        };
                        panel.update(cx, |panel, cx| panel.mark_code_sent(sent, cx));
                    }
                })
                .log_err();
        }
        let message = match &result {
            Ok(()) => format!("Sent {count} review comments to Plannotator"),
            Err(error) => {
                log::error!("plannotator: send failed: {error:#}");
                match save_unsent(&comments, &worktree) {
                    Ok(path) => format!("Send failed. Comments saved to {}", path.display()),
                    Err(_) => format!("Send failed: {error}"),
                }
            }
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .log_err();
    })
    .detach();
}

/// The editor of the active branch diff or project diff tab.
fn active_diff_editor(workspace: &Workspace, cx: &App) -> Option<Entity<Editor>> {
    let item = workspace.active_item(cx)?;
    let splittable = if let Some(diff) = item.downcast::<BranchDiff>() {
        diff.read(cx).editor(cx)
    } else if let Some(diff) = item.downcast::<ProjectDiff>() {
        diff.read(cx).editor(cx)
    } else if let Some(chapter) = item.downcast::<chapter_view::ChapterDiffView>() {
        return Some(chapter.read(cx).rhs_editor(cx));
    } else {
        return None;
    };
    Some(splittable.read(cx).rhs_editor().clone())
}

async fn post_feedback(
    http: Arc<dyn HttpClient>,
    session_url: &str,
    comments: &[ExportedReviewComment],
    worktree: &str,
) -> Result<()> {
    let body = feedback::build_request(comments, worktree, now_ms());
    client::send_json(&http, Method::POST, &format!("{session_url}/api/feedback"), &body).await
}

/// Keeps the comments as markdown when the send fails.
fn save_unsent(comments: &[ExportedReviewComment], worktree: &str) -> Result<PathBuf> {
    let dir = paths::unsent_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", now_ms()));
    let body = feedback::build_request(comments, worktree, now_ms());
    std::fs::write(&path, body.feedback)?;
    Ok(path)
}

/// Closes `items` in every pane, without a save prompt.
pub(crate) fn close_items<T: workspace::Item>(
    workspace: &mut Workspace,
    items: Vec<Entity<T>>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    for item in items {
        for pane in workspace.panes().to_vec() {
            pane.update(cx, |pane, cx| {
                if pane.index_for_item(&item).is_some() {
                    pane.close_item_by_id(item.entity_id(), workspace::SaveIntent::Skip, window, cx)
                        .detach();
                }
            });
        }
    }
}

/// The root folders of the worktrees open in this window.
pub(crate) fn worktree_roots(workspace: &Workspace, cx: &App) -> Vec<PathBuf> {
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

pub(crate) fn show_toast(workspace: &mut Workspace, message: impl Into<String>, cx: &mut Context<Workspace>) {
    let toast = StatusToast::new(message.into(), cx, |this, _| this);
    workspace.toggle_status_toast(toast, cx);
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
