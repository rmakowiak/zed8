//! The Review dock panel: the guide of the current branch, its chapters with
//! reviewed ticks, the Plannotator session status, Approve and Send feedback.

use crate::{
    chapter_view::{ChapterDiffView, ChapterKey},
    guide::{self, LoadedGuide},
    session::{self, Session},
};
use anyhow::Result;
use editor::{EditorEvent, actions::SendReviewToAgent};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels,
    Render, Subscription, Task, WeakEntity, Window, actions, px,
};
use http_client::{AsyncBody, HttpClient, Method, Request};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use file_icons::FileIcons;
use ui::{Checkbox, Disclosure, TintColor, ToggleState, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    plannotator_review,
    [
        /// Opens or focuses the Plannotator review panel.
        ToggleFocus
    ]
);

const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Default, PartialEq)]
struct PanelState {
    roots: Vec<PathBuf>,
    branch: Option<String>,
    session: Option<Session>,
    guide: Option<LoadedGuide>,
    /// Git status and line counts of the guide's files, by path.
    file_stats: HashMap<String, FileStat>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct FileStat {
    /// The `git diff --name-status` letter: A, M, D or R.
    status: char,
    added: u32,
    removed: u32,
}

pub struct ReviewPanel {
    workspace: WeakEntity<Workspace>,
    http: Arc<dyn HttpClient>,
    focus_handle: FocusHandle,
    position: DockPosition,
    state: PanelState,
    /// The chapter whose tab was opened last.
    selected: Option<usize>,
    /// Chapters that show their overview and files.
    expanded: HashSet<usize>,
    /// Unsent inline comments in the open chapter tab.
    comment_count: usize,
    _chapter_subscription: Option<Subscription>,
    _poll: Task<()>,
}

impl ReviewPanel {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let workspace_handle = workspace.weak_handle();
        let http: Arc<dyn HttpClient> = workspace.client().http_client();
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(roots) = this.update(cx, |this, cx| this.worktree_roots(cx)) else {
                    return;
                };
                let state = cx
                    .background_executor()
                    .spawn(async move { load_state(roots) })
                    .await;
                let updated = this.update(cx, |this, cx| {
                    if this.state != state {
                        this.state = state;
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    return;
                }
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        });
        Self {
            workspace: workspace_handle,
            http,
            focus_handle: cx.focus_handle(),
            position: DockPosition::Right,
            state: PanelState::default(),
            selected: None,
            expanded: HashSet::default(),
            comment_count: 0,
            _chapter_subscription: None,
            _poll: poll,
        }
    }

    fn worktree_roots(&self, cx: &App) -> Vec<PathBuf> {
        self.workspace
            .upgrade()
            .map(|workspace| {
                workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn toggle_reviewed(&mut self, chapter: usize, cx: &mut Context<Self>) {
        let Some(guide) = self.state.guide.as_mut() else {
            return;
        };
        let mut reviewed = guide.reviewed_flags();
        let Some(flag) = reviewed.get_mut(chapter) else {
            return;
        };
        *flag = !*flag;
        guide.envelope.reviewed = reviewed.clone();
        cx.notify();

        let path = guide.path.clone();
        let id = guide.id.clone();
        let session_url = self.state.session.as_ref().map(|session| session.url.clone());
        let http = self.http.clone();
        cx.background_spawn(async move {
            // A live session owns the guide store, so it writes the file itself.
            match session_url {
                Some(url) => {
                    let body = serde_json::json!({ "reviewed": reviewed });
                    send_json(&http, Method::PUT, &format!("{url}/api/guide/saved:{id}/reviewed"), &body)
                        .await
                }
                None => guide::write_reviewed(&path, &reviewed),
            }
        })
        .detach_and_log_err(cx);
    }

    fn approve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.state.session.clone() else {
            return;
        };
        let http = self.http.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let body = serde_json::json!({ "approved": true, "feedback": "", "annotations": [] });
            let result =
                send_json(&http, Method::POST, &format!("{}/api/feedback", session.url), &body).await;
            let message = match result {
                Ok(()) => "Approved. Plannotator told Claude.".to_string(),
                Err(error) => format!("Approve failed: {error}"),
            };
            workspace
                .update(cx, |workspace, cx| crate::show_toast(workspace, message, cx))
                .log_err();
        })
        .detach();
    }

    /// Opens the chapter's diff tab, or switches to it, and scrolls to `file`.
    fn show_chapter(
        &mut self,
        chapter: usize,
        file: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(guide), Some(root)) = (self.state.guide.as_ref(), self.state.roots.first()) else {
            return;
        };
        let Some(section) = guide.envelope.guide.sections.get(chapter) else {
            return;
        };
        let key = ChapterKey {
            guide_id: guide.id.clone(),
            chapter,
        };
        let title = format!("{}. {}", chapter + 1, section.title);
        let diffs = section.diffs.clone();
        let base = guide
            .envelope
            .review
            .as_ref()
            .and_then(|review| review.base.clone())
            .unwrap_or_else(|| "origin/master".to_string());
        let root = root.clone();
        self.selected = Some(chapter);
        self.expanded.insert(chapter);
        let panel = cx.weak_entity();
        let reused = self
            .workspace
            .update(cx, |workspace, cx| {
                let existing = workspace
                    .items_of_type::<ChapterDiffView>(cx)
                    .find(|view| view.read(cx).key() == &key);
                if let Some(view) = existing {
                    workspace.activate_item(&view, true, true, window, cx);
                    if let Some(file) = file {
                        view.update(cx, |view, cx| view.scroll_to_file(&file, window, cx));
                    }
                    return Some(view);
                }
                let open = ChapterDiffView::open(key, title, root, base, diffs, workspace, window, cx);
                cx.spawn_in(window, async move |_, cx| {
                    let view = open.await?;
                    panel.update(cx, |panel, cx| panel.watch_chapter(&view, cx))?;
                    if let Some(file) = file {
                        view.update_in(cx, |view, window, cx| view.scroll_to_file(&file, window, cx))?;
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
                None
            })
            .log_err()
            .flatten();
        // Outside the workspace update: this panel is already being updated here.
        if let Some(view) = reused {
            self.watch_chapter(&view, cx);
        }
    }

    /// Keeps the Send feedback count in step with the open chapter tab.
    fn watch_chapter(&mut self, view: &Entity<ChapterDiffView>, cx: &mut Context<Self>) {
        self.comment_count = view.read(cx).review_comment_count(cx);
        self._chapter_subscription = Some(cx.subscribe(view, |this, _, event: &EditorEvent, cx| {
            if let EditorEvent::ReviewCommentsChanged { total_count } = event {
                this.comment_count = *total_count;
                cx.notify();
            }
        }));
        cx.notify();
    }

    fn render_section(label: String, cx: &App) -> Div {
        v_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_1p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(label.to_uppercase())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let branch = self.state.branch.clone().unwrap_or_default();
        let (title, meta) = match &self.state.guide {
            Some(guide) => {
                let engine = match guide.envelope.engine.as_deref() {
                    Some("claude") | None => "Claude".to_string(),
                    Some(other) => other.to_string(),
                };
                let base = guide
                    .envelope
                    .review
                    .as_ref()
                    .and_then(|review| review.base.clone())
                    .unwrap_or_else(|| "origin/master".to_string());
                (
                    guide.envelope.guide.title.clone(),
                    format!(
                        "Guide by {engine} · {} · base {base}",
                        relative_time(guide.envelope.saved_at)
                    ),
                )
            }
            None => ("Review".to_string(), "No guide yet".to_string()),
        };
        v_flex()
            .w_full()
            .px_3()
            .pt_2p5()
            .pb_2()
            .gap_0p5()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(Label::new(title).weight(FontWeight::MEDIUM))
            .child(Label::new(meta).size(LabelSize::Small).color(Color::Muted))
            .child(
                h_flex()
                    .gap_1()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new(branch).size(LabelSize::Small).color(Color::Muted)),
            )
    }

    fn render_session(&self, cx: &App) -> impl IntoElement {
        let (color, text) = match &self.state.session {
            Some(session) => (
                Color::Success,
                format!(
                    "Claude, through Plannotator ({})",
                    session.url.trim_start_matches("http://localhost:")
                ),
            ),
            None => (
                Color::Muted,
                "No review session. Ask Claude to start one.".to_string(),
            ),
        };
        Self::render_section("Sends to".to_string(), cx).child(
            h_flex()
                .gap_1p5()
                .child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(color))
                .child(Label::new(text).size(LabelSize::Small)),
        )
    }

    fn render_chapters(&self, guide: &LoadedGuide, cx: &mut Context<Self>) -> impl IntoElement {
        let sections = &guide.envelope.guide.sections;
        let done = (0..sections.len()).filter(|ix| guide.is_reviewed(*ix)).count();
        let mut rows = Vec::with_capacity(sections.len());
        for (ix, section) in sections.iter().enumerate() {
            rows.push(self.render_chapter(ix, section, guide.is_reviewed(ix), cx));
        }
        Self::render_section(
            format!("Chapters · {done} of {} reviewed", sections.len()),
            cx,
        )
        .child(v_flex().gap_1p5().children(rows))
    }

    fn render_chapter(
        &self,
        ix: usize,
        section: &guide::GuideSection,
        reviewed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected == Some(ix);
        let expanded = self.expanded.contains(&ix);
        let colors = cx.theme().colors();
        let text_muted = colors.text_muted;
        let divider = colors.border_variant;
        let this = cx.weak_entity();
        let files = section.diffs.len();
        let notes: usize = section.diffs.iter().map(|diff| diff.notes.len()).sum();
        let mut meta = if files == 1 {
            "1 file".to_string()
        } else {
            format!("{files} files")
        };
        if notes > 0 {
            meta.push_str(&format!(" · ✦ {notes}"));
        }

        let border = if selected {
            colors.border_focused
        } else {
            colors.border_variant
        };
        let surface = colors.surface_background;
        let row = h_flex()
            .w_full()
            .items_start()
            .gap_2()
            .px_1p5()
            .py_1p5()
            .child(
                div().pt_0p5().child(
                    Disclosure::new(("chapter-disclosure", ix), expanded).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if !this.expanded.remove(&ix) {
                                this.expanded.insert(ix);
                            }
                            cx.notify();
                        },
                    )),
                ),
            )
            .child(
                div().pt_0p5().child(
                    Checkbox::new(("chapter-reviewed", ix), ToggleState::from(reviewed)).on_click(
                        move |_, _, cx| {
                            this.update(cx, |this, cx| this.toggle_reviewed(ix, cx)).ok();
                        },
                    ),
                ),
            )
            .child(
                h_flex()
                    .id(("chapter", ix))
                    .flex_1()
                    .min_w_0()
                    .items_start()
                    .gap_1p5()
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .child(
                                Label::new((ix + 1).to_string()).color(Color::Muted),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(section.title.clone())
                                    .weight(FontWeight::MEDIUM)
                                    .color(if reviewed { Color::Muted } else { Color::Default }),
                            )
                            .child(Label::new(meta).size(LabelSize::XSmall).color(Color::Muted)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show_chapter(ix, None, window, cx);
                    })),
            );

        let details = expanded.then(|| {
            v_flex()
                .mx_1p5()
                .pt_1p5()
                .pb_1p5()
                .gap_1()
                .border_t_1()
                .border_color(divider)
                .when(!section.overview.is_empty(), |el| {
                    el.child(
                        div()
                            .pb_1()
                            .text_sm()
                            .line_height(px(19.))
                            .text_color(text_muted)
                            .child(section.overview.clone()),
                    )
                })
                .children(section.diffs.iter().enumerate().map(|(file_ix, diff)| {
                    self.render_file_row(ix, file_ix, diff, cx)
                }))
        });

        v_flex()
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(surface)
            .child(row)
            .children(details)
            .into_any_element()
    }

    /// One file, drawn like a row of the git panel: icon, name in its git
    /// status color, folder path, line counts and the note count.
    fn render_file_row(
        &self,
        chapter: usize,
        file_ix: usize,
        diff: &guide::GuideDiff,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let file = diff.file.clone();
        let (dir, name) = match file.rsplit_once('/') {
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => (String::new(), file.clone()),
        };
        let stat = self.state.file_stats.get(&file).copied().unwrap_or_default();
        let name_color = match stat.status {
            'A' => Color::VersionControlAdded,
            'D' => Color::Disabled,
            'M' | 'R' => Color::VersionControlModified,
            _ => Color::Default,
        };
        let icon = FileIcons::get_icon(Path::new(&file), cx)
            .map(Icon::from_path)
            .unwrap_or_else(|| Icon::new(IconName::File))
            .size(IconSize::Small)
            .color(Color::Muted);
        let notes = diff.notes.len();
        h_flex()
            .id(("chapter-file", chapter * 1000 + file_ix))
            .w_full()
            .h(px(24.))
            .gap_1p5()
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(colors.ghost_element_hover))
            .tooltip(ui::Tooltip::text(format!("{file}\n\n{}", diff.summary)))
            .child(icon)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        Label::new(name)
                            .color(name_color)
                            .when(stat.status == 'D', |label| label.strikethrough()),
                    )
                    .when(!dir.is_empty(), |row| {
                        row.child(Label::new(dir).color(Color::Muted).truncate_start())
                    }),
            )
            .when(stat.added > 0 || stat.removed > 0, |row| {
                row.child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .when(stat.added > 0, |el| {
                            el.child(
                                Label::new(format!("+{}", stat.added))
                                    .size(LabelSize::Small)
                                    .color(Color::VersionControlAdded),
                            )
                        })
                        .when(stat.removed > 0, |el| {
                            el.child(
                                Label::new(format!("−{}", stat.removed))
                                    .size(LabelSize::Small)
                                    .color(Color::VersionControlDeleted),
                            )
                        }),
                )
            })
            .when(notes > 0, |row| {
                row.child(
                    Label::new(format!("✦ {notes}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.show_chapter(chapter, Some(file.clone()), window, cx);
            }))
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_session = self.state.session.is_some();
        let send_label = if self.comment_count > 0 {
            format!("Send feedback ({})", self.comment_count)
        } else {
            "Send feedback".to_string()
        };
        v_flex()
            .w_full()
            .px_3()
            .py_2p5()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div().flex_1().child(
                            Button::new("plannotator-approve", "Approve")
                                .full_width()
                                .style(ButtonStyle::Filled)
                                .disabled(!has_session)
                                .on_click(cx.listener(|this, _, window, cx| this.approve(window, cx))),
                        ),
                    )
                    .child(
                        div().flex_1().child(
                            Button::new("plannotator-send", send_label)
                                .full_width()
                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                .disabled(!has_session || self.comment_count == 0)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(SendReviewToAgent.boxed_clone(), cx);
                                }),
                        ),
                    ),
            )
            .child(
                Label::new("Posts to the Plannotator session that Claude waits on.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

fn relative_time(saved_at_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let minutes = now_ms.saturating_sub(saved_at_ms) / 60_000;
    match minutes {
        0 => "just now".to_string(),
        1..=59 => format!("{minutes} min ago"),
        60..=1439 => format!("{} h ago", minutes / 60),
        _ => format!("{} d ago", minutes / 1440),
    }
}

fn load_state(roots: Vec<PathBuf>) -> PanelState {
    let root = roots.first().cloned();
    let git = |args: &[&str]| -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root.as_ref()?)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let head = git(&["rev-parse", "HEAD"]);
    let session = session::find_review_session(&session::sessions_dir(), &roots);
    let guide = guide::find_guide(&guide::guides_dir(), branch.as_deref(), head.as_deref());
    let mut file_stats = HashMap::default();
    let base = guide
        .as_ref()
        .and_then(|guide| guide.envelope.review.as_ref()?.base.clone())
        .unwrap_or_else(|| "origin/master".to_string());
    if guide.is_some()
        && let Some(merge_base) = git(&["merge-base", "HEAD", &base])
    {
        for line in git(&["diff", "--name-status", &merge_base]).unwrap_or_default().lines() {
            let mut parts = line.split('\t');
            let (Some(status), Some(path)) = (parts.next(), parts.last()) else {
                continue;
            };
            file_stats.entry(path.to_string()).or_insert(FileStat::default()).status =
                status.chars().next().unwrap_or('M');
        }
        for line in git(&["diff", "--numstat", &merge_base]).unwrap_or_default().lines() {
            let mut parts = line.split('\t');
            let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.last())
            else {
                continue;
            };
            let stat = file_stats.entry(path.to_string()).or_insert(FileStat::default());
            stat.added = added.parse().unwrap_or(0);
            stat.removed = removed.parse().unwrap_or(0);
        }
    }
    PanelState {
        roots,
        branch,
        session,
        guide,
        file_stats,
    }
}

pub(crate) async fn send_json(
    http: &Arc<dyn HttpClient>,
    method: Method,
    url: &str,
    body: &serde_json::Value,
) -> Result<()> {
    let request = Request::builder()
        .method(method)
        .uri(url)
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(serde_json::to_string(body)?))?;
    let response = http.send(request).await?;
    anyhow::ensure!(response.status().is_success(), "Plannotator answered {}", response.status());
    Ok(())
}

impl Render for ReviewPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.state.guide.clone() {
            Some(guide) => self.render_chapters(&guide, cx).into_any_element(),
            None => Self::render_section("Chapters".to_string(), cx)
                .child(
                    Label::new("No guide for this branch yet. Ask Claude to write one with Plannotator.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        };
        v_flex()
            .key_context("PlannotatorReviewPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(self.render_session(cx))
            .child(
                v_flex()
                    .id("plannotator-review-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(self.render_footer(cx))
    }
}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ReviewPanel {}

impl Panel for ReviewPanel {
    fn persistent_name() -> &'static str {
        "PlannotatorReviewPanel"
    }

    fn panel_key() -> &'static str {
        "PlannotatorReviewPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(340.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Plannotator Review")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        20
    }
}
