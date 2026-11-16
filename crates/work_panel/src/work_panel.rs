//! The Work panel (Zed8): your Linear issues, with the worktree that works on
//! each one. The data comes from the `n8n_worktree_manager` crate, the same
//! source as the CLI and Raycast Workbench.

mod panel;
mod preview;
mod start;

use gpui::{App, AppContext as _, KeyBinding, transparent_black};
use n8n_worktree_manager::{
    github::{Checks, PrStatus},
    linear::IssueRow,
};
use ui::{Chip, prelude::*};
use workspace::Workspace;

pub use panel::{
    AskClaude, CopyBranch, CopyKeyAndTitle, CopyUrl, LinearStatus, OpenInLinear, OpenPreview,
    StartWork, ToggleActions, ToggleFocus, WorkPanel,
};

/// The key context of the issue list. The filter box is outside it, so
/// Space and Enter type there as usual.
const LIST_CONTEXT: &str = "WorkPanelList";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", menu::SelectPrevious, Some(LIST_CONTEXT)),
        KeyBinding::new("down", menu::SelectNext, Some(LIST_CONTEXT)),
        KeyBinding::new("enter", StartWork, Some(LIST_CONTEXT)),
        KeyBinding::new("space", OpenPreview, Some(LIST_CONTEXT)),
        KeyBinding::new("cmd-k", ToggleActions, Some(LIST_CONTEXT)),
        KeyBinding::new("cmd-o", OpenInLinear, Some(LIST_CONTEXT)),
        KeyBinding::new("cmd-j", AskClaude, Some(LIST_CONTEXT)),
        KeyBinding::new("cmd-shift-c", CopyBranch, Some(LIST_CONTEXT)),
        KeyBinding::new("cmd-shift-.", CopyKeyAndTitle, Some(LIST_CONTEXT)),
        KeyBinding::new("cmd-shift-,", CopyUrl, Some(LIST_CONTEXT)),
        KeyBinding::new("enter", StartWork, Some("IssuePreview")),
    ]);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<WorkPanel>(window, cx);
        });
        let Some(window) = window else {
            return;
        };
        let handle = cx.entity().downgrade();
        let panel = cx.new(|cx| WorkPanel::new(handle, window, cx));
        let status = cx.new(|cx| LinearStatus::new(panel.clone(), cx));
        workspace.add_panel(panel, window, cx);
        workspace
            .status_bar()
            .update(cx, |bar, cx| bar.add_right_item(status, window, cx));
    })
    .detach();
}

/// The order of the status groups. Statuses that are not here come after,
/// in the order Linear returned them.
const STATUS_ORDER: &[&str] = &[
    "Ready for pick up",
    "In Progress",
    "Testing & Review",
    "To be released",
    "Draft",
    "Backlog",
    "Triage",
];

/// These groups show even when they are empty.
const ALWAYS_SHOWN: &[&str] = &["Ready for pick up", "In Progress"];

/// The issues grouped by status, in `STATUS_ORDER`. The groups in
/// `ALWAYS_SHOWN` are there even with no issue.
pub(crate) fn group_by_status(rows: &[IssueRow]) -> Vec<(String, Vec<IssueRow>)> {
    let mut groups: Vec<(String, Vec<IssueRow>)> = ALWAYS_SHOWN
        .iter()
        .map(|status| (status.to_string(), Vec::new()))
        .collect();
    for row in rows {
        match groups
            .iter_mut()
            .find(|(status, _)| status.eq_ignore_ascii_case(&row.issue.status))
        {
            Some((_, group)) => group.push(row.clone()),
            None => groups.push((row.issue.status.clone(), vec![row.clone()])),
        }
    }
    let rank = |status: &str| {
        STATUS_ORDER
            .iter()
            .position(|known| known.eq_ignore_ascii_case(status))
            .unwrap_or(STATUS_ORDER.len())
    };
    // A stable sort keeps the Linear order among unknown statuses.
    groups.sort_by_key(|(status, _)| rank(status));
    groups
}

pub(crate) fn is_ready(status: &str) -> bool {
    status.eq_ignore_ascii_case("Ready for pick up")
}

pub(crate) fn priority_label(priority: u8) -> &'static str {
    match priority {
        1 => "Urgent",
        2 => "High",
        3 => "Medium",
        4 => "Low",
        _ => "No priority",
    }
}

/// An uppercase section title, like the mockups' `.sec`.
pub(crate) fn section(text: impl Into<String>) -> Div {
    h_flex().px_3().pt_3().pb_1().child(
        Label::new(text.into().to_uppercase())
            .size(LabelSize::XSmall)
            .weight(gpui::FontWeight::SEMIBOLD)
            .color(Color::Muted),
    )
}

/// A small outlined chip. A color tints the text and the border.
pub(crate) fn chip(text: impl Into<SharedString>, color: Option<Color>, cx: &App) -> Chip {
    let chip = Chip::new(text).bg_color(transparent_black());
    match color {
        Some(color) => chip
            .label_color(color)
            .border_color(color.color(cx).opacity(0.45)),
        None => chip,
    }
}

/// The status chip: ○ for not started, ◐ for started, with the colors of the
/// mockups.
pub(crate) fn status_chip(status: &str, cx: &App) -> Chip {
    let lower = status.to_lowercase();
    let (glyph, color) = if is_ready(status) || lower == "todo" || lower == "backlog" {
        ("○", Some(Color::Warning))
    } else if lower.contains("progress") {
        ("◐", Some(Color::Warning))
    } else if lower.contains("done") || lower.contains("released") {
        ("●", Some(Color::Success))
    } else {
        ("◐", None)
    };
    chip(format!("{glyph} {status}"), color, cx)
}

/// "PR ✓ CI", "PR ✕ CI" and so on.
pub(crate) fn pr_chip(pr: &PrStatus, cx: &App) -> Chip {
    if pr.state == "MERGED" {
        return chip("PR merged", Some(Color::Accent), cx);
    }
    if pr.state == "CLOSED" {
        return chip("PR closed", Some(Color::Muted), cx);
    }
    match pr.checks {
        Checks::Passing => chip("PR ✓ CI", Some(Color::Success), cx),
        Checks::Failing => chip("PR ✕ CI", Some(Color::Error), cx),
        Checks::Pending => chip("PR ● CI", Some(Color::Warning), cx),
        Checks::None => chip("PR", None, cx),
    }
}

/// "⎇ worktree", with the setup step while it runs or where it failed.
pub(crate) fn worktree_chip(row: &IssueRow, cx: &App) -> Option<Chip> {
    row.worktree.as_ref()?;
    let setup = row.setup.as_deref().unwrap_or("done");
    Some(if setup.starts_with("failed") {
        chip(format!("⎇ worktree · setup {setup}"), Some(Color::Error), cx)
    } else if setup == "done" {
        chip("⎇ worktree", None, cx)
    } else {
        let (done, total) = row.setup_progress.unwrap_or_default();
        chip(
            format!("⎇ worktree · setting up {done}/{total}"),
            Some(Color::Accent),
            cx,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use n8n_worktree_manager::linear::IssueSummary;

    fn row(key: &str, status: &str) -> IssueRow {
        IssueRow {
            issue: IssueSummary {
                identifier: key.to_string(),
                title: String::new(),
                status: status.to_string(),
                status_type: String::new(),
                project: None,
                url: String::new(),
                branch_name: None,
                assignee: None,
                priority: 0,
            },
            worktree: None,
            setup: None,
            setup_progress: None,
        }
    }

    fn keys(rows: &[IssueRow]) -> Vec<(String, Vec<String>)> {
        group_by_status(rows)
            .into_iter()
            .map(|(status, rows)| {
                let keys = rows.into_iter().map(|row| row.issue.identifier).collect();
                (status, keys)
            })
            .collect()
    }

    #[test]
    fn groups_follow_the_status_order_and_unknown_statuses_come_last() {
        let rows = [
            row("A-1", "Testing & Review"),
            row("A-2", "Blocked"),
            row("A-3", "Ready for pick up"),
            row("A-4", "Testing & Review"),
            row("A-5", "In Progress"),
        ];

        assert_eq!(
            keys(&rows),
            [
                ("Ready for pick up".to_string(), vec!["A-3".to_string()]),
                ("In Progress".to_string(), vec!["A-5".to_string()]),
                (
                    "Testing & Review".to_string(),
                    vec!["A-1".to_string(), "A-4".to_string()]
                ),
                ("Blocked".to_string(), vec!["A-2".to_string()]),
            ]
        );
    }

    #[test]
    fn ready_and_in_progress_show_even_when_empty() {
        let rows = [row("A-1", "Testing & Review")];

        assert_eq!(
            keys(&rows),
            [
                ("Ready for pick up".to_string(), vec![]),
                ("In Progress".to_string(), vec![]),
                ("Testing & Review".to_string(), vec!["A-1".to_string()]),
            ]
        );
    }
}
