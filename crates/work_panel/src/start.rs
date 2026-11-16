use crate::chip;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Render, WeakEntity, Window,
};
use n8n_worktree_manager::{Manager, linear::IssueRow};
use notifications::status_toast::StatusToast;
use std::{path::PathBuf, sync::Arc, time::Duration};
use task::{RevealStrategy, SpawnInTerminal};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{Checkbox, ToggleState, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, MultiWorkspace, OpenMode, PathList, Workspace};

/// Opens or creates the worktree of an issue. An issue that has a worktree opens it, with no
/// question, because nothing is written. Otherwise a dialog asks first. `mine` is true for an
/// issue assigned to you: only then does the dialog offer the move to In Progress by default.
pub(crate) fn start_work(
    workspace: WeakEntity<Workspace>,
    manager: Arc<Manager>,
    row: IssueRow,
    mine: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(name) = row.worktree.clone() {
        // A setup that is not done resumes from its failed step.
        let provision = row.setup.as_deref().is_some_and(|setup| setup != "done");
        open_worktree(workspace, manager, name, provision, false, window, cx);
        return;
    }
    workspace
        .update(cx, |workspace, cx| {
            let handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |_, cx| StartWorkModal {
                focus_handle: cx.focus_handle(),
                workspace: handle,
                manager,
                move_issue: mine && !is_started(&row),
                row,
                claude_after_setup: true,
            });
        })
        .log_err();
}

/// Starts Claude Code in a new terminal of the current worktree, about an
/// issue, with no new worktree.
pub(crate) fn ask_claude(
    workspace: WeakEntity<Workspace>,
    row: &IssueRow,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let prompt = format!(
        "Read the Linear issue {} with the Linear MCP (get_issue and list_comments). Tell me what it asks for and what you would look at first in this code.",
        row.issue.identifier
    );
    let cwd = workspace.read_with(cx, |workspace, cx| {
        workspace
            .root_paths(cx)
            .into_iter()
            .next()
            .map(|path| path.to_path_buf())
    });
    spawn_claude(&workspace, cwd, Some(prompt), window, cx);
}

fn spawn_claude(
    workspace: &Entity<Workspace>,
    cwd: Option<PathBuf>,
    prompt: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(panel) = workspace.read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx)) else {
        toast(&workspace.downgrade(), "Zed cannot open a terminal in this project.", cx);
        return;
    };
    let task = SpawnInTerminal {
        label: "Claude Code".to_string(),
        full_label: "claude".to_string(),
        command_label: "claude".to_string(),
        command: Some("claude".to_string()),
        args: prompt.into_iter().collect(),
        cwd,
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Always,
        ..SpawnInTerminal::default()
    };
    let spawn = panel.update(cx, |panel, cx| panel.spawn_task(&task, window, cx));
    cx.spawn(async move |_| spawn.await.log_err()).detach();
}

/// The same rule as `Linear::start_issue`: a started issue never moves.
fn is_started(row: &IssueRow) -> bool {
    matches!(
        row.issue.status_type.as_str(),
        "started" | "completed" | "canceled"
    )
}

/// Creates the worktree on a background thread, and with `move_issue` moves
/// the issue to In Progress first. Then it opens the worktree and runs its
/// setup.
fn confirm_start(
    workspace: WeakEntity<Workspace>,
    manager: Arc<Manager>,
    row: IssueRow,
    move_issue: bool,
    claude_after_setup: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let key = row.issue.identifier.clone();
    toast(&workspace, format!("Creating the worktree for {key}…"), cx);
    window
        .spawn(cx, async move |cx| {
            let started = cx
                .background_spawn({
                    let manager = manager.clone();
                    let key = key.clone();
                    async move { manager.start(&manager.linear(), &key, move_issue) }
                })
                .await;
            match started {
                Ok(meta) => cx.update(|window, cx| {
                    open_worktree(workspace, manager, meta.name, true, claude_after_setup, window, cx)
                }),
                Err(error) => cx.update(|_, cx| {
                    toast(&workspace, format!("Could not create the worktree for {key}: {error:#}"), cx)
                }),
            }
        })
        .detach_and_log_err(cx);
}

/// Opens the worktree as a group of this window and focuses it. It never
/// opens a new window. With `provision`, the setup runs in a terminal there,
/// and with `claude_after_setup`, Claude Code starts when the setup succeeds.
pub(crate) fn open_worktree(
    workspace: WeakEntity<Workspace>,
    manager: Arc<Manager>,
    name: String,
    provision: bool,
    claude_after_setup: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let checkout = manager.checkout_dir(&name);
    let Some(multi_workspace) = window.root::<MultiWorkspace>().flatten() else {
        toast(&workspace, "This window cannot hold another worktree.", cx);
        return;
    };
    let opened = multi_workspace.update(cx, |multi_workspace, cx| {
        multi_workspace.find_or_create_local_workspace(
            PathList::new(&[checkout.clone()]),
            None,
            None,
            OpenMode::Activate,
            Some(workspace.clone()),
            window,
            cx,
        )
    });
    window
        .spawn(cx, async move |cx| {
            let opened = opened.await?;
            if !provision {
                return Ok(());
            }
            // A new workspace loads its panels after it opens.
            let mut panel = None;
            for _ in 0..50 {
                panel = cx.update(|_, cx| opened.read(cx).panel::<TerminalPanel>(cx))?;
                if panel.is_some() {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(100)).await;
            }
            let Some(panel) = panel else {
                anyhow::bail!(
                    "The terminal of {name} did not open. Run: n8n-worktree-manager provision {name}"
                );
            };
            let task = provision_task(&manager, &name);
            let spawn = cx.update(|window, cx| {
                panel.update(cx, |panel, cx| panel.spawn_task(&task, window, cx))
            })?;
            let terminal = spawn.await?;
            if !claude_after_setup {
                return Ok(());
            }
            let Some(terminal) = terminal.upgrade() else {
                return Ok(());
            };
            let finished = cx.update(|_, cx| terminal.read(cx).wait_for_completed_task(cx))?;
            let status = finished.await;
            if status.is_some_and(|status| status.success()) {
                cx.update(|window, cx| spawn_claude(&opened, Some(checkout), None, window, cx))?;
            }
            Ok(())
        })
        .detach_and_log_err(cx);
}

fn provision_task(manager: &Manager, name: &str) -> SpawnInTerminal {
    let binary = manager_binary();
    let command = format!("{} provision {name}", binary.display());
    SpawnInTerminal {
        label: format!("Setup: {name}"),
        full_label: command.clone(),
        command_label: command,
        command: Some(binary.display().to_string()),
        args: vec!["provision".to_string(), name.to_string()],
        cwd: Some(manager.checkout_dir(name)),
        use_new_terminal: false,
        allow_concurrent_runs: false,
        reveal: RevealStrategy::Always,
        ..SpawnInTerminal::default()
    }
}

/// The binary next to the Zed8 executable, as `bundle.sh` puts it, else the
/// one on PATH.
fn manager_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("n8n-worktree-manager")))
        .filter(|binary| binary.is_file())
        .unwrap_or_else(|| PathBuf::from("n8n-worktree-manager"))
}

fn toast(workspace: &WeakEntity<Workspace>, message: impl Into<String>, cx: &mut App) {
    let message = message.into();
    workspace
        .update(cx, |workspace, cx| {
            let toast = StatusToast::new(message, cx, |this, _| this);
            workspace.toggle_status_toast(toast, cx);
        })
        .log_err();
}

struct StartWorkModal {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    manager: Arc<Manager>,
    row: IssueRow,
    move_issue: bool,
    claude_after_setup: bool,
}

impl StartWorkModal {
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        confirm_start(
            self.workspace.clone(),
            self.manager.clone(),
            self.row.clone(),
            self.move_issue,
            self.claude_after_setup,
            window,
            cx,
        );
        cx.emit(DismissEvent);
    }

    fn worktree_name(&self) -> String {
        self.row.issue.identifier.to_lowercase()
    }
}

/// A numbered step of the dialog.
fn step(number: &'static str, cx: &App) -> Div {
    div()
        .size_5()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(cx.theme().colors().element_selected)
        .child(
            Label::new(number)
                .size(LabelSize::XSmall)
                .weight(gpui::FontWeight::SEMIBOLD)
                .color(Color::Accent),
        )
}

impl Render for StartWorkModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let issue = self.row.issue.clone();
        let branch = issue
            .branch_name
            .clone()
            .unwrap_or_else(|| self.worktree_name());
        let note = |text: String| Label::new(text).size(LabelSize::Small).color(Color::Muted);
        let started = is_started(&self.row);
        v_flex()
            .key_context("StartWorkModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|modal, _: &menu::Confirm, window, cx| modal.confirm(window, cx)))
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .w(px(560.))
            .px_5()
            .py_5()
            .gap_4()
            .elevation_3(cx)
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(issue.identifier.clone())
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(format!("Create a worktree for “{}”", issue.title)),
                    ),
            )
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .items_start()
                            .gap_2p5()
                            .child(step("1", cx))
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(if started {
                                        h_flex()
                                            .gap_1()
                                            .flex_wrap()
                                            .child(Label::new("Keep the status"))
                                            .child(
                                                Label::new(issue.status.clone())
                                                    .weight(gpui::FontWeight::SEMIBOLD),
                                            )
                                            .into_any_element()
                                    } else {
                                        Checkbox::new("start-work-move", ToggleState::from(self.move_issue))
                                            .label("Move the issue to In Progress")
                                            .on_click(cx.listener(|modal, state: &ToggleState, _, cx| {
                                                modal.move_issue = state.selected();
                                                cx.notify();
                                            }))
                                            .into_any_element()
                                    })
                                    .child(note(if started {
                                        "The issue is started already, so nothing is written to Linear."
                                            .to_string()
                                    } else if self.move_issue {
                                        format!("Linear write. Today: {}.", issue.status)
                                    } else {
                                        format!(
                                            "Nothing is written to Linear. The issue stays {}.",
                                            issue.status
                                        )
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_start()
                            .gap_2p5()
                            .child(step("2", cx))
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .flex_wrap()
                                            .child(Label::new("Create worktree"))
                                            .child(Label::new(self.worktree_name()).buffer_font(cx))
                                            .child(Label::new("on the Linear branch")),
                                    )
                                    .child(div().child(chip(branch, None, cx)))
                                    .child(note(
                                        "n8n-worktree-manager makes the branch from origin/master, starts the DB, installs and builds. About 4 minutes."
                                            .to_string(),
                                    )),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_start()
                            .gap_2p5()
                            .child(step("3", cx))
                            .child(
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .flex_wrap()
                                            .child(Label::new("Open it in"))
                                            .child(Label::new("this window").weight(gpui::FontWeight::SEMIBOLD))
                                            .child(Label::new("and focus it")),
                                    )
                                    .child(note(
                                        "A new worktree group in the threads sidebar. The setup runs in a terminal tab there."
                                            .to_string(),
                                    )),
                            ),
                    ),
            )
            .child(
                Checkbox::new(
                    "start-work-claude",
                    ToggleState::from(self.claude_after_setup),
                )
                .label("Start a Claude thread when the setup ends")
                .on_click(cx.listener(|modal, state: &ToggleState, _, cx| {
                    modal.claude_after_setup = state.selected();
                    cx.notify();
                })),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("start-work-cancel", "Cancel")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("start-work-confirm", "Create worktree ↵")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .on_click(cx.listener(|modal, _, window, cx| modal.confirm(window, cx))),
                    ),
            )
    }
}

impl Focusable for StartWorkModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for StartWorkModal {}

impl ModalView for StartWorkModal {}

/// Makes the `pr-<n>` worktree on the PR's branch, then opens it with its
/// setup, for a review of someone else's PR.
pub(crate) fn start_review(
    workspace: WeakEntity<Workspace>,
    manager: Arc<Manager>,
    number: u64,
    branch: String,
    window: &mut Window,
    cx: &mut App,
) {
    let name = format!("pr-{number}");
    toast(&workspace, format!("Creating the worktree {name}…"), cx);
    window
        .spawn(cx, async move |cx| {
            let created = cx
                .background_spawn({
                    let manager = manager.clone();
                    let name = name.clone();
                    async move {
                        let mut options = n8n_worktree_manager::CreateOptions::new(&name);
                        options.branch = Some(branch);
                        manager.create(options)
                    }
                })
                .await;
            cx.update(|window, cx| match created {
                Ok(meta) => open_worktree(workspace, manager, meta.name, true, false, window, cx),
                Err(error) => toast(&workspace, format!("Cannot create {name}: {error:#}"), cx),
            })
        })
        .detach_and_log_err(cx);
}
