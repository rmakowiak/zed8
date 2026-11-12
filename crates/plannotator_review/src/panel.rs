//! The Review dock panel: the guide of the current branch, its chapters with
//! reviewed ticks, the Plannotator session status, Approve and Send feedback.

use crate::{
    chapter_view::{ChapterDiffView, ChapterIntro, ChapterKey, ChapterNavEvent},
    guide::{self, LoadedGuide},
    panel_state::{PanelState, announce_roots, load_state},
    session::Session,
    spec_view::{SpecReviewView, SpecViewEvent},
    start,
};
use editor::{EditorEvent, actions::SendReviewToAgent};
use gpui::{
    Action, App, Context, ElementId, Entity, EntityId, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels,
    Render, Subscription, Task, WeakEntity, Window, actions, px,
};
use http_client::{HttpClient, Method};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use file_icons::FileIcons;
use ui::{
    Checkbox, ContextMenu, Disclosure, IconPosition, PopoverMenu, SpinnerLabel, TintColor, ToggleState,
    Tooltip, prelude::*,
};
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
#[derive(Clone)]
struct SpecResult {
    target: String,
    /// The session that got the answer. It finds the Claude thread.
    session: Session,
    approved: bool,
    /// The local time of the send, such as `23:03`.
    at: String,
}

#[derive(Clone)]
pub(crate) struct CodeSent {
    pub message: String,
    pub approved: bool,
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
    /// Specs that went back to Claude, until their next round opens.
    spec_results: Vec<SpecResult>,
    /// Code review feedback sent from Zed, until the next session starts.
    code_sent: Option<CodeSent>,
    /// The review session picked in the panel, by pid. Without a pick, or
    /// after it ends, the newest session gets the feedback.
    chosen_session: Option<i32>,
    /// The Claude terminal picked in the panel, for a worktree without a
    /// review session.
    chosen_thread: Option<EntityId>,
    _spec_subscriptions: Vec<Subscription>,
    /// Rendered chapter overviews, by guide id and chapter.
    overviews: HashMap<(String, usize), Entity<Markdown>>,
    _chapter_subscriptions: Vec<Subscription>,
    _poll: Task<()>,
}

impl ReviewPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_handle = workspace.weak_handle();
        let http: Arc<dyn HttpClient> = workspace.client().http_client();
        let poll = cx.spawn_in(window, async move |this, cx| {
            let mut first_load = true;
            let Ok(window_key) = this.update(cx, |_, cx| format!("{}-{}", std::process::id(), cx.entity_id()))
            else {
                return;
            };
            loop {
                let Ok((roots, previous)) = this.update(cx, |this, cx| {
                    (this.worktree_roots(cx), this.state.clone())
                }) else {
                    return;
                };
                let state = cx
                    .background_executor()
                    .spawn({
                        let window_key = window_key.clone();
                        async move {
                        announce_roots(&window_key, &roots);
                        load_state(roots, previous)
                    }})
                    .await;
                let Ok((reveal, new_specs, workspace)) = this.update(cx, |this, cx| {
                    // A new guide or a new session means Claude just started a review.
                    let guide_id = |state: &PanelState| state.guide.as_ref().map(|guide| guide.id.clone());
                    let session_pid = |state: &PanelState| state.sessions.first().map(|session| session.pid);
                    // On the first load too: a spec that waits when Zed starts opens.
                    let new_specs: Vec<Session> = state
                        .specs
                        .iter()
                        .filter(|spec| !this.state.specs.iter().any(|old| old.pid == spec.pid))
                        .cloned()
                        .collect();
                    let reveal = !new_specs.is_empty()
                        || !first_load
                            && ((guide_id(&state).is_some() && guide_id(&state) != guide_id(&this.state))
                                || (session_pid(&state).is_some()
                                    && session_pid(&state) != session_pid(&this.state)));
                    if session_pid(&state).is_some() && session_pid(&state) != session_pid(&this.state) {
                        this.code_sent = None;
                    }
                    this.state = state;
                    // Also follows Claude's working state, which lives in a terminal title.
                    cx.notify();
                    (reveal, new_specs, this.workspace.clone())
                }) else {
                    return;
                };
                first_load = false;
                if reveal {
                    // Opens the dock without taking focus from the Claude terminal.
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.reveal_panel::<ReviewPanel>(window, cx)
                        })
                        .log_err();
                }
                for spec in new_specs {
                    this.update_in(cx, |this, window, cx| this.open_spec(spec, false, window, cx))
                        .log_err();
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
            overviews: HashMap::default(),
            spec_results: Vec::new(),
            code_sent: None,
            chosen_session: None,
            chosen_thread: None,
            _spec_subscriptions: Vec::new(),
            _chapter_subscriptions: Vec::new(),
            _poll: poll,
        }
    }

    fn worktree_roots(&self, cx: &App) -> Vec<PathBuf> {
        self.workspace
            .upgrade()
            .map(|workspace| crate::worktree_roots(workspace.read(cx), cx))
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
        let now_reviewed = *flag;
        guide.envelope.reviewed = reviewed.clone();
        let guide_id = guide.id.clone();
        cx.notify();
        if let Some(workspace) = self.workspace.upgrade() {
            let views: Vec<Entity<ChapterDiffView>> = workspace
                .read(cx)
                .items_of_type::<ChapterDiffView>(cx)
                .filter(|view| {
                    let key = view.read(cx).key();
                    key.guide_id == guide_id && key.chapter == chapter
                })
                .collect();
            for view in views {
                view.update(cx, |view, cx| view.set_reviewed(now_reviewed, cx));
            }
        }

        let path = guide.path.clone();
        let id = guide.id.clone();
        let session_url = self.session().map(|session| session.url.clone());
        let http = self.http.clone();
        cx.background_spawn(async move {
            // A live session owns the guide store, so it writes the file itself.
            match session_url {
                Some(url) => {
                    let body = serde_json::json!({ "reviewed": reviewed });
                    crate::client::send_json(&http, Method::PUT, &format!("{url}/api/guide/saved:{id}/reviewed"), &body)
                        .await
                }
                None => guide::write_reviewed(&path, &reviewed),
            }
        })
        .detach_and_log_err(cx);
    }

    fn approve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session().cloned() else {
            return;
        };
        let http = self.http.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let body = serde_json::json!({ "approved": true, "feedback": "", "annotations": [] });
            let result =
                crate::client::send_json(&http, Method::POST, &format!("{}/api/feedback", session.url), &body).await;
            let message = match &result {
                Ok(()) => "Approved. Plannotator told Claude.".to_string(),
                Err(error) => format!("Approve failed: {error}"),
            };
            if result.is_ok() {
                this.update(cx, |this, cx| {
                    let at = chrono::Local::now().format("%H:%M");
                    this.mark_code_sent(CodeSent { message: format!("Approved at {at}"), approved: true }, cx)
                })
                .log_err();
            }
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
        let intro = ChapterIntro {
            index: chapter,
            count: guide.envelope.guide.sections.len(),
            title: section.title.clone(),
            overview: section.overview.clone(),
            reviewed: guide.is_reviewed(chapter),
        };
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
                let open = ChapterDiffView::open(key, title, intro, root, base, diffs, workspace, window, cx);
                cx.spawn_in(window, async move |_, cx| {
                    let view = open.await?;
                    panel.update_in(cx, |panel, window, cx| panel.watch_chapter(&view, window, cx))?;
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
            self.watch_chapter(&view, window, cx);
        }
    }

    /// Opens the spec review tab of `spec`, or activates it if it is open.
    /// Claude starting a spec review opens it without focus.
    fn open_spec(&mut self, spec: Session, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let http = self.http.clone();
        let target = spec.target.clone().unwrap_or_default();
        let session = spec.clone();
        let opened = self
            .workspace
            .update(cx, |workspace, cx| {
                let existing = workspace
                    .items_of_type::<SpecReviewView>(cx)
                    .find(|view| view.read(cx).session_pid() == spec.pid);
                if let Some(view) = existing {
                    workspace.activate_item(&view, true, focus, window, cx);
                    return None;
                }
                // Only the latest round of a file stays open. Drafts are on disk.
                let earlier_rounds: Vec<_> = workspace
                    .items_of_type::<SpecReviewView>(cx)
                    .filter(|view| view.read(cx).target() == spec.target.as_deref())
                    .collect();
                // An earlier round that still waits would keep its Claude
                // waiting. Dismiss it, as closing it in Plannotator does.
                for earlier in &earlier_rounds {
                    let earlier = earlier.read(cx);
                    if earlier.session_pid() != spec.pid {
                        let url = format!("{}/api/exit", earlier.session_url());
                        let http = http.clone();
                        cx.background_spawn(async move {
                            crate::client::send_json(&http, Method::POST, &url, &serde_json::json!({})).await
                        })
                        .detach();
                    }
                }
                let view = cx.new(|cx| SpecReviewView::new(spec.clone(), http, window, cx));
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(view.clone()), true, focus, None, window, cx);
                });
                crate::close_items(workspace, earlier_rounds, window, cx);
                Some(view)
            })
            .log_err()
            .flatten();
        let Some(view) = opened else {
            return;
        };
        self.spec_results.retain(|result| result.target != target);
        self._spec_subscriptions.push(cx.subscribe(&view, move |this, _, event, cx| {
            let SpecViewEvent::Finished { approved } = event;
            this.spec_results.retain(|result| result.target != target);
            this.spec_results.push(SpecResult {
                target: target.clone(),
                session: session.clone(),
                approved: *approved,
                at: chrono::Local::now().format("%H:%M").to_string(),
            });
            cx.notify();
        }));
        cx.notify();
    }

    /// Code review feedback left Zed. The panel shows it until a new session.
    pub(crate) fn mark_code_sent(&mut self, sent: CodeSent, cx: &mut Context<Self>) {
        self.code_sent = Some(sent);
        cx.notify();
    }

    /// The review session that gets Send feedback and Approve.
    pub(crate) fn session(&self) -> Option<&Session> {
        let sessions = &self.state.sessions;
        self.chosen_session
            .and_then(|pid| sessions.iter().find(|session| session.pid == pid))
            .or(sessions.first())
    }

    pub(crate) fn chosen_thread(&self) -> Option<EntityId> {
        self.chosen_thread
    }

    fn claude_terminals(&self, cx: &App) -> Vec<start::ClaudeTerminal> {
        self.workspace
            .upgrade()
            .map(|workspace| start::claude_terminals(workspace.read(cx), false, cx))
            .unwrap_or_default()
    }

    /// Shows the Claude thread in the Agent panel.
    fn open_thread(&self, target: &start::ClaudeTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let target = target.clone();
        self.workspace
            .update(cx, |workspace, cx| start::open_terminal(workspace, &target, window, cx))
            .log_err();
    }

    /// A small, muted row with the name of the Claude thread behind a session.
    fn render_thread_label(
        id: impl Into<ElementId>,
        owner: Option<&start::ClaudeTerminal>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(owner) = owner.cloned() else {
            return Label::new("Thread not open in this window")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element();
        };
        let hover = cx.theme().colors().ghost_element_hover;
        h_flex()
            .id(id.into())
            .gap_1()
            .min_w_0()
            .px_0p5()
            .rounded_sm()
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .tooltip(Tooltip::text("Open this Claude thread"))
            .child(Icon::new(IconName::Terminal).size(IconSize::XSmall).color(Color::Muted))
            .child(Label::new(owner.title.clone()).size(LabelSize::XSmall).color(Color::Muted).truncate())
            .when(owner.busy, |row| {
                row.child(Label::new("working").size(LabelSize::XSmall).color(Color::Accent))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(&owner, window, cx)))
            .into_any_element()
    }

    fn render_specs(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let results: Vec<SpecResult> = self
            .spec_results
            .iter()
            .filter(|result| {
                !self.state.specs.iter().any(|spec| spec.target.as_deref() == Some(&result.target))
            })
            .cloned()
            .collect();
        if self.state.specs.is_empty() && results.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let hover = colors.ghost_element_hover;
        let terminals = self.claude_terminals(cx);
        let owner = |session: &Session| terminals.iter().find(|terminal| terminal.owns(session)).cloned();
        let name_and_dir = |target: &str| {
            let path = Path::new(target);
            (
                path.file_name().and_then(|name| name.to_str()).unwrap_or("spec").to_string(),
                path.parent().map(|dir| dir.display().to_string()).unwrap_or_default(),
            )
        };
        let mut rows: Vec<AnyElement> = self
            .state
            .specs
            .iter()
            .map(|spec| {
                let target = spec.target.clone().unwrap_or_default();
                let (name, dir) = name_and_dir(&target);
                let thread = Self::render_thread_label(("spec-thread", spec.pid as usize), owner(spec).as_ref(), cx);
                let spec = spec.clone();
                let row = h_flex()
                    .id(("spec", spec.pid as usize))
                    .w_full()
                    .px_1()
                    .py_0p5()
                    .gap_1p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .tooltip(Tooltip::text(target.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_spec(spec.clone(), true, window, cx)
                    }))
                    .child(Icon::new(IconName::FileMarkdown).size(IconSize::Small).color(Color::Accent))
                    .child(Label::new(name).size(LabelSize::Small))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(dir).size(LabelSize::XSmall).color(Color::Muted).truncate_start()),
                    )
                    .child(Label::new("your turn").size(LabelSize::XSmall).color(Color::Accent));
                v_flex().w_full().child(row).child(div().pl_6().child(thread)).into_any_element()
            })
            .collect();
        for result in results {
            let (name, _) = name_and_dir(&result.target);
            let busy = owner(&result.session).is_some_and(|owner| owner.busy);
            let (mark, text, color): (AnyElement, String, Color) = if result.approved {
                (
                    Icon::new(IconName::Check).size(IconSize::Small).color(Color::Success).into_any_element(),
                    format!("Approved at {}", result.at),
                    Color::Success,
                )
            } else if busy {
                (
                    SpinnerLabel::new().size(LabelSize::Small).color(Color::Accent).into_any_element(),
                    format!("Sent at {}. Claude is working on the next round.", result.at),
                    Color::Accent,
                )
            } else {
                (
                    Icon::new(IconName::Check).size(IconSize::Small).color(Color::Muted).into_any_element(),
                    format!("Sent at {}. Waiting for the next round.", result.at),
                    Color::Muted,
                )
            };
            let target = result.target.clone();
            rows.push(
                h_flex()
                    .w_full()
                    .px_1()
                    .py_0p5()
                    .gap_1p5()
                    .child(mark)
                    .child(Label::new(name).size(LabelSize::Small))
                    .child(div().flex_1().min_w_0().child(Label::new(text).size(LabelSize::XSmall).color(color).truncate()))
                    .child(
                        IconButton::new(SharedString::from(format!("dismiss-{target}")), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Hide"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.spec_results.retain(|result| result.target != target);
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        Some(
            Self::render_section("Spec review".to_string(), cx)
                .child(v_flex().gap_0p5().children(rows))
                .into_any_element(),
        )
    }

    /// Keeps the Send feedback count in step with the open chapter tab, and
    /// follows its Previous, Next and Mark reviewed buttons.
    fn watch_chapter(&mut self, view: &Entity<ChapterDiffView>, window: &mut Window, cx: &mut Context<Self>) {
        self.comment_count = view.read(cx).review_comment_count(cx);
        self._chapter_subscriptions = vec![
            cx.subscribe(view, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::ReviewCommentsChanged { total_count } = event {
                    this.comment_count = *total_count;
                    cx.notify();
                }
            }),
            cx.subscribe_in(view, window, |this, _, event: &ChapterNavEvent, window, cx| match event {
                ChapterNavEvent::Show(chapter) => this.show_chapter(*chapter, None, window, cx),
                ChapterNavEvent::ToggleReviewed(chapter) => this.toggle_reviewed(*chapter, cx),
            }),
        ];
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
                // The PR number and title read best, as in the mock.
                let title = match &self.state.pr {
                    Some(pr) => format!("#{} {}", pr.number, pr.title),
                    None => guide.envelope.guide.title.clone(),
                };
                (
                    title,
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
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(Label::new(title).weight(FontWeight::MEDIUM)))
                    .when(self.state.guide.is_some() && self.state.pr.is_some(), |row| {
                        row.child(
                            IconButton::new("new-guide", IconName::RotateCw)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Ask Claude for a new guide"))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(start::ReviewPr), cx)
                                }),
                        )
                    }),
            )
            .child(Label::new(meta).size(LabelSize::Small).color(Color::Muted))
            // The title bar shows the branch. Without a guide, it helps here.
            .when(self.state.guide.is_none() && !branch.is_empty(), |header| {
                header.child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall).color(Color::Muted))
                        .child(Label::new(branch).size(LabelSize::Small).color(Color::Muted)),
                )
            })
            .when_some(self.head_mismatch(), |header, pr_head| {
                header.child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(IconName::Warning).size(IconSize::XSmall).color(Color::Warning))
                        .child(
                            Label::new(format!(
                                "Local HEAD is not the PR head ({}). Pull or push first.",
                                &pr_head[..pr_head.len().min(7)]
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                        ),
                )
            })
    }

    /// The PR head commit when it differs from the local HEAD.
    fn head_mismatch(&self) -> Option<String> {
        let pr = self.state.pr.as_ref()?;
        let head = self.state.head.as_ref()?;
        (&pr.head_ref_oid != head).then(|| pr.head_ref_oid.clone())
    }

    /// The empty state: a "Review PR" button, or why there is none.
    fn render_start(&self, cx: &App) -> impl IntoElement {
        let section = Self::render_section("Chapters".to_string(), cx);
        match &self.state.pr {
            Some(pr) => section
                .child(
                    Label::new(format!("No guide yet for PR #{}: {}", pr.number, pr.title))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Button::new("review-pr", format!("Review PR #{}", pr.number))
                        .style(ButtonStyle::Filled)
                        .full_width()
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(start::ReviewPr), cx)),
                )
                .child(
                    Label::new("Claude Code in a Zed terminal writes the guide.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            None => section
                .child(Button::new("review-pr", "Review PR").full_width().disabled(true))
                .child(
                    Label::new("This branch has no open GitHub PR.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        }
    }

    fn render_session(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let terminals = self.claude_terminals(cx);
        let session = self.session().cloned();
        let target = start::pick_terminal(&terminals, session.as_ref(), self.chosen_thread).cloned();
        // With a session, the chip names the thread that started it.
        let chip_thread = match &session {
            Some(session) => terminals.iter().find(|terminal| terminal.owns(session)).cloned(),
            None => target.clone(),
        };
        let busy = chip_thread.as_ref().is_some_and(|thread| thread.busy);
        let code_sent = self.code_sent.clone().filter(|_| session.is_none());
        let can_start = session.is_none() && self.state.guide.is_some() && code_sent.is_none();

        let (status_color, status): (Color, String) = match (&code_sent, &session) {
            (Some(sent), _) if sent.approved => (Color::Success, sent.message.clone()),
            (Some(sent), _) if busy => (Color::Accent, format!("{}. Claude is working on it.", sent.message)),
            (Some(sent), _) => (Color::Accent, format!("{}. Claude has it.", sent.message)),
            (None, Some(session)) => {
                let others = self.state.sessions.len().saturating_sub(1);
                let mut text = format!("Plannotator session {} · Claude waits for you", session.port());
                if others > 0 {
                    text.push_str(&format!(" · {others} more"));
                }
                (Color::Success, text)
            }
            (None, None) => (Color::Muted, "No review session".to_string()),
        };

        let chip_label = match (&chip_thread, &session) {
            (Some(thread), _) => thread.title.clone(),
            (None, Some(_)) => "Thread not open in this window".to_string(),
            (None, None) => "No Claude thread".to_string(),
        };
        let menu = self.thread_menu(&terminals, session.as_ref(), target.as_ref(), cx);
        let chip = PopoverMenu::new("claude-thread-picker")
            .menu(move |window, cx| Some(menu(window, cx)))
            .trigger_with_tooltip(
                Button::new("claude-thread-chip", chip_label)
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .truncate(true)
                    .start_icon(
                        Icon::new(if busy { IconName::LoadCircle } else { IconName::Terminal })
                            .size(IconSize::XSmall)
                            .color(if busy { Color::Accent } else { Color::Muted }),
                    )
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                Tooltip::text("The Claude thread that gets your review"),
            );

        Self::render_section("Sends to".to_string(), cx)
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .child(div().flex_1().min_w_0().child(chip))
                    .when(can_start, |row| {
                        row.child(
                            Button::new("start-session", "Start session")
                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text("Ask Claude to start a review session for this guide"))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(start::StartSession), cx)
                                }),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(if busy && code_sent.is_some() {
                        SpinnerLabel::new().size(LabelSize::XSmall).color(Color::Accent).into_any_element()
                    } else {
                        Icon::new(IconName::Circle).size(IconSize::XSmall).color(status_color).into_any_element()
                    })
                    .child(
                        Label::new(status)
                            .size(LabelSize::XSmall)
                            .color(if status_color == Color::Muted { Color::Muted } else { status_color })
                            .truncate(),
                    ),
            )
    }

    /// The menu behind the thread chip. With review sessions, each entry is
    /// a session and its thread. Without one, each entry is a Claude thread.
    fn thread_menu(
        &self,
        terminals: &[start::ClaudeTerminal],
        session: Option<&Session>,
        target: Option<&start::ClaudeTerminal>,
        cx: &mut Context<Self>,
    ) -> impl Fn(&mut Window, &mut App) -> Entity<ContextMenu> + 'static {
        let panel = cx.weak_entity();
        let workspace = self.workspace.clone();
        let sessions: Vec<(i32, String, bool)> = self
            .state
            .sessions
            .iter()
            .map(|candidate| {
                let owner = terminals.iter().find(|terminal| terminal.owns(candidate));
                let label = format!(
                    "{} · session {}, {}",
                    owner.map(|owner| owner.title.as_str()).unwrap_or("Thread not open here"),
                    candidate.port(),
                    candidate.started_label()
                );
                (candidate.pid, label, session.is_some_and(|session| session.pid == candidate.pid))
            })
            .collect();
        let threads: Vec<(start::ClaudeTerminal, bool)> = terminals
            .iter()
            .map(|terminal| (terminal.clone(), target.is_some_and(|target| target.id() == terminal.id())))
            .collect();
        let open = session
            .and_then(|session| terminals.iter().find(|terminal| terminal.owns(session)))
            .or(target)
            .cloned();
        move |window, cx| {
            let (panel, workspace, sessions, threads, open) =
                (panel.clone(), workspace.clone(), sessions.clone(), threads.clone(), open.clone());
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                if !sessions.is_empty() {
                    menu = menu.header("Review sessions in this worktree");
                    for (pid, label, chosen) in sessions {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(label, chosen, IconPosition::Start, None, move |_, cx| {
                            panel
                                .update(cx, |panel, cx| {
                                    panel.chosen_session = Some(pid);
                                    cx.notify();
                                })
                                .ok();
                        });
                    }
                } else if !threads.is_empty() {
                    menu = menu.header("Claude threads");
                    for (thread, chosen) in threads {
                        let panel = panel.clone();
                        let id = thread.id();
                        let label = if thread.busy {
                            format!("{} · working", thread.title)
                        } else {
                            thread.title.clone()
                        };
                        menu = menu.toggleable_entry(label, chosen, IconPosition::Start, None, move |_, cx| {
                            panel
                                .update(cx, |panel, cx| {
                                    panel.chosen_thread = Some(id);
                                    cx.notify();
                                })
                                .ok();
                        });
                    }
                }
                menu = menu.separator();
                if let Some(open) = open {
                    let workspace = workspace.clone();
                    menu = menu.entry("Open this thread", None, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| start::open_terminal(workspace, &open, window, cx))
                            .ok();
                    });
                }
                let workspace = workspace.clone();
                menu.entry("New Claude thread", None, move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| start::start_claude(workspace, None, window, cx))
                        .ok();
                })
            })
        }
    }

    fn render_chapters(
        &self,
        guide: &LoadedGuide,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sections = &guide.envelope.guide.sections;
        let done = (0..sections.len()).filter(|ix| guide.is_reviewed(*ix)).count();
        let mut rows = Vec::with_capacity(sections.len());
        for (ix, section) in sections.iter().enumerate() {
            let overview = self.overviews.get(&(guide.id.clone(), ix)).cloned();
            rows.push(self.render_chapter(ix, section, overview, guide.is_reviewed(ix), window, cx));
        }
        Self::render_section(
            format!("Chapters · {done} of {} reviewed", sections.len()),
            cx,
        )
        .child(v_flex().gap_0p5().children(rows))
    }

    fn render_chapter(
        &self,
        ix: usize,
        section: &guide::GuideSection,
        overview: Option<Entity<Markdown>>,
        reviewed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let overview_style = small_muted_markdown(window, cx);
        let selected = self.selected == Some(ix);
        let expanded = self.expanded.contains(&ix);
        let colors = cx.theme().colors();
        let text_muted = colors.text_muted;
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

        let selected_bg = colors.ghost_element_selected;
        let hover_bg = colors.ghost_element_hover;
        let row = h_flex()
            .id(("chapter", ix))
            .w_full()
            .items_start()
            .gap_2()
            .px_1p5()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |row| row.bg(selected_bg))
            .when(!selected, |row| row.hover(move |style| style.bg(hover_bg)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.show_chapter(ix, None, window, cx);
            }))
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
                div()
                    .w(px(14.))
                    .flex_none()
                    .child(Label::new((ix + 1).to_string()).color(Color::Muted)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(section.title.clone())
                            .color(if reviewed { Color::Muted } else { Color::Default })
                            .when(reviewed, |label| label.strikethrough()),
                    )
                    .child(Label::new(meta).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .child(
                Disclosure::new(("chapter-disclosure", ix), expanded).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.stop_propagation();
                        if !this.expanded.remove(&ix) {
                            this.expanded.insert(ix);
                        }
                        cx.notify();
                    },
                )),
            );

        // The overview and the files, under the title.
        let details = expanded.then(|| {
            v_flex()
                .pl(px(50.))
                .pr_1p5()
                .pt_0p5()
                .pb_2()
                .gap_1()
                .when_some(overview, |el, overview| {
                    el.child(
                        div()
                            .pb_1()
                            .text_color(text_muted)
                            .child(MarkdownElement::new(overview, overview_style)),
                    )
                })
                .children(section.diffs.iter().enumerate().map(|(file_ix, diff)| {
                    self.render_file_row(ix, file_ix, diff, cx)
                }))
        });

        v_flex().w_full().child(row).children(details).into_any_element()
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
        let has_session = self.session().is_some();
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
                                .style(ButtonStyle::Outlined)
                                .start_icon(Icon::new(IconName::Check).size(IconSize::Small).color(Color::Success))
                                .disabled(!has_session)
                                .on_click(cx.listener(|this, _, window, cx| this.approve(window, cx))),
                        ),
                    )
                    .child(
                        div().flex_1().child(
                            Button::new("plannotator-send", send_label)
                                .full_width()
                                .style(ButtonStyle::Filled)
                                .start_icon(Icon::new(IconName::Send).size(IconSize::Small).color(Color::Accent))
                                .disabled(!has_session || self.comment_count == 0)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(SendReviewToAgent.boxed_clone(), cx);
                                }),
                        ),
                    ),
            )
            .child(
                Label::new("Posts to the Plannotator session that Claude waits on. Claude decides whether to post on GitHub.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

/// Markdown in the panel's muted text, at the overview size.
fn small_muted_markdown(window: &Window, cx: &App) -> MarkdownStyle {
    let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_muted_text(cx);
    // The size of `text_sm`, which the overview had as plain text.
    let size: gpui::AbsoluteLength = gpui::rems(0.875).into();
    style.base_text_style.font_size = size;
    style.base_text_style.line_height = gpui::relative(1.45);
    style.container_style.text.font_size = Some(size);
    style.container_style.text.color = Some(cx.theme().colors().text_muted);
    style.paragraph_line_height = gpui::relative(1.45);
    style
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

impl Render for ReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(guide) = &self.state.guide {
            for (ix, section) in guide.envelope.guide.sections.iter().enumerate() {
                if section.overview.is_empty() {
                    continue;
                }
                self.overviews.entry((guide.id.clone(), ix)).or_insert_with(|| {
                    let source = crate::rich_text::render_mermaid(&section.overview);
                    cx.new(|cx| Markdown::new(source.into(), None, None, cx))
                });
            }
        }
        let body = match self.state.guide.clone() {
            Some(guide) => self.render_chapters(&guide, window, cx).into_any_element(),
            None => self.render_start(cx).into_any_element(),
        };
        v_flex()
            .key_context("PlannotatorReviewPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .children(self.render_specs(cx))
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
