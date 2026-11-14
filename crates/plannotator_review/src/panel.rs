//! The Review dock panel: the guide of the current branch, its chapters with
//! reviewed ticks, the Plannotator session status, Approve and Send feedback.

use crate::{
    actions::{self, ReviewAction},
    chapter_view::{ChapterDiffView, ChapterIntro, ChapterKey, ChapterNavEvent},
    findings::{self, AnnotationsResponse},
    tools::{self, FlagState, LabelChange},
    manual_test_view::ManualTestView,
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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
/// How long a new thread may take before Claude runs in it.
const CLAUDE_START_TIME: Duration = Duration::from_secs(20);
#[derive(Clone)]
struct SpecResult {
    target: String,
    /// The session that got the answer. It finds the Claude thread.
    session: Session,
    approved: bool,
    /// The local time of the send, such as `23:03`.
    at: String,
}

/// The flag picker under Feature flags.
struct FlagPicker {
    /// Flags of PostHog Staging and the `flag:` labels of the repo.
    options: Vec<String>,
    loading: bool,
    query: Entity<editor::Editor>,
    /// The flag keys that the PR gets, with `=false` or `=<variant>` if any.
    picked: Vec<String>,
    /// The change waits for your yes, because it writes to GitHub.
    confirm: Option<LabelChange>,
    applying: bool,
    _query_subscription: Subscription,
}

/// Something that a Claude thread waits on you for.
#[derive(Clone)]
struct Gate {
    /// Stays the same while the gate waits, so each gate notifies once.
    key: String,
    /// The terminal thread that waits.
    owner: Option<agent_ui::TerminalId>,
    message: String,
    kind: GateKind,
}

#[derive(Clone)]
enum GateKind {
    Spec(Session),
    Findings(Session),
    Results,
    Question,
}

impl GateKind {
    /// The words after "Waits on you" in the threads sidebar.
    fn short_label(&self) -> SharedString {
        match self {
            GateKind::Spec(session) if session.mode == "plan" => "Plan".into(),
            GateKind::Spec(_) => "Spec".into(),
            GateKind::Findings(_) => "Findings".into(),
            GateKind::Results => "Test results".into(),
            GateKind::Question => "Question".into(),
        }
    }
}

/// The state of a review action card.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ActionStatus {
    NotStarted,
    Idle,
    Working,
    Waiting,
    Done,
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
    /// Action cards that show only their header.
    collapsed_cards: HashSet<ReviewAction>,
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
    /// When each action was started from this panel. Claude takes a few
    /// seconds to start in its new thread, and the card must not offer Start
    /// again meanwhile.
    started: HashMap<ReviewAction, Instant>,
    flag_picker: Option<FlagPicker>,
    /// The gates that already notified or opened.
    known_gates: HashSet<String>,
    handled_first_gates: bool,
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
                let Ok((reveal, workspace)) = this.update(cx, |this, cx| {
                    // A new guide or a new session means Claude just started a review.
                    let guide_id = |state: &PanelState| state.guide.as_ref().map(|guide| guide.id.clone());
                    let session_pid = |state: &PanelState| state.sessions.first().map(|session| session.pid);
                    // A new spec review opens only its tab: the panel adds nothing to it.
                    let reveal = !first_load
                        && ((guide_id(&state).is_some() && guide_id(&state) != guide_id(&this.state))
                            || (session_pid(&state).is_some() && session_pid(&state) != session_pid(&this.state)));
                    if session_pid(&state).is_some() && session_pid(&state) != session_pid(&this.state) {
                        this.code_sent = None;
                    }
                    this.state = state;
                    // Also follows Claude's working state, which lives in a terminal title.
                    cx.notify();
                    (reveal, this.workspace.clone())
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
                this.update(cx, |this, cx| this.refresh_results_tab(cx)).log_err();
                this.update_in(cx, |this, window, cx| this.handle_new_gates(window, cx)).log_err();
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
            collapsed_cards: HashSet::default(),
            comment_count: 0,
            overviews: HashMap::default(),
            spec_results: Vec::new(),
            code_sent: None,
            chosen_session: None,
            chosen_thread: None,
            started: HashMap::default(),
            flag_picker: None,
            known_gates: HashSet::default(),
            handled_first_gates: false,
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
                let avatar = workspace
                    .project()
                    .read(cx)
                    .user_store()
                    .read(cx)
                    .current_user()
                    .map(|user| user.avatar_uri.clone());
                let view = cx.new(|cx| SpecReviewView::new(spec.clone(), http, avatar, window, cx));
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
        let unseen = self.unseen_results();
        let gate2 = self.findings_session(&self.claude_terminals(cx));
        if self.state.specs.is_empty() && results.is_empty() && unseen.is_none() && gate2.is_none() {
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
                let owner = owner(spec);
                let action = owner.as_ref().and_then(|owner| self.action_of(owner));
                let (name, dir) = match (spec.mode.as_str(), action) {
                    ("plan", Some(action)) => (format!("{} plan", action.title()), String::new()),
                    ("plan", None) => ("Plan".to_string(), String::new()),
                    (_, Some(action)) => (format!("{} · {name}", action.title()), dir),
                    _ => (name, dir),
                };
                let waited = waited_label(&spec.started_at);
                let thread = Self::render_thread_label(("spec-thread", spec.pid as usize), owner.as_ref(), cx);
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
                    .child(Label::new(waited).size(LabelSize::XSmall).color(Color::Warning));
                v_flex().w_full().child(row).child(div().pl_6().child(thread)).into_any_element()
            })
            .collect();
        if let Some((total, failed)) = unseen {
            rows.insert(
                0,
                h_flex()
                    .id("manual-results")
                    .w_full()
                    .px_1()
                    .py_0p5()
                    .gap_1p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(|this, _, window, cx| this.open_results(window, cx)))
                    .child(Icon::new(IconName::PlayFilled).size(IconSize::Small).color(Color::Accent))
                    .child(div().flex_1().min_w_0().child(Label::new("Manual test results").size(LabelSize::Small)))
                    .child(
                        Label::new(if failed > 0 {
                            format!("{failed} of {total} failed")
                        } else {
                            format!("{total} passed")
                        })
                        .size(LabelSize::XSmall)
                        .color(if failed > 0 { Color::Error } else { Color::Success }),
                    )
                    .into_any_element(),
            );
        }
        let gate2_count = usize::from(gate2.is_some());
        if let Some(session) = gate2 {
            let waited = waited_label(&session.started_at);
            rows.insert(
                0,
                h_flex()
                    .id("gate2-findings")
                    .w_full()
                    .px_1()
                    .py_0p5()
                    .gap_1p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_findings(session.clone(), window, cx)))
                    .child(Icon::new(IconName::Sparkle).size(IconSize::Small).color(Color::Accent))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new("Agent review · review the findings").size(LabelSize::Small)),
                    )
                    .child(Label::new(waited).size(LabelSize::XSmall).color(Color::Warning))
                    .into_any_element(),
            );
        }
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
            Self::render_section(
                format!(
                    "Needs you · {}",
                    self.state.specs.len() + usize::from(unseen.is_some()) + gate2_count
                ),
                cx,
            )
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

    /// The review action whose thread is `terminal`.
    fn action_of(&self, terminal: &start::ClaudeTerminal) -> Option<ReviewAction> {
        let id = terminal.thread_id?.to_key_string();
        ReviewAction::ALL
            .into_iter()
            .find(|action| self.state.actions.thread(*action) == Some(id.as_str()))
    }

    fn action_terminal(
        &self,
        action: ReviewAction,
        terminals: &[start::ClaudeTerminal],
    ) -> Option<start::ClaudeTerminal> {
        let id = self.state.actions.thread(action)?;
        terminals
            .iter()
            .find(|terminal| terminal.thread_id.is_some_and(|thread| thread.to_key_string() == id))
            .cloned()
    }

    fn action_status(&self, action: ReviewAction, terminal: Option<&start::ClaudeTerminal>) -> ActionStatus {
        if self.state.actions.thread(action).is_none() {
            return ActionStatus::NotStarted;
        }
        let progress = self.state.progress.get(&action);
        let owns_gate = terminal.is_some_and(|terminal| {
            self.state.specs.iter().chain(&self.state.sessions).any(|session| terminal.owns(session))
        });
        if owns_gate || progress.is_some_and(|progress| progress.waiting) {
            ActionStatus::Waiting
        } else if progress.is_some_and(|progress| progress.done) {
            ActionStatus::Done
        } else if terminal.is_none()
            && self.started.get(&action).is_some_and(|started| started.elapsed() < CLAUDE_START_TIME)
        {
            ActionStatus::Working
        } else if terminal.is_none() {
            // The thread was closed, or Claude in it quit, before the action finished.
            ActionStatus::NotStarted
        } else if terminal.is_some_and(|terminal| terminal.busy) {
            ActionStatus::Working
        } else {
            ActionStatus::Idle
        }
    }

    /// Starts the action in a new Claude thread with the action's name.
    fn start_action(&mut self, action: ReviewAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.state.roots.first().cloned() else {
            return;
        };
        let dir = crate::paths::actions_dir();
        let progress_file = actions::progress_path(&dir, &root, action);
        // A new run starts without the progress of the last one.
        if let Err(error) = std::fs::remove_file(&progress_file)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("plannotator: cannot reset {}: {error}", progress_file.display());
        }
        if let Some(parent) = progress_file.parent() {
            std::fs::create_dir_all(parent).log_err();
        }
        let command = format!("claude {}", shell_quote(&action.command(&progress_file)));
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let thread = workspace.update(cx, |workspace, cx| {
            let agent_panel = workspace.panel::<agent_ui::AgentPanel>(cx)?;
            let thread = agent_panel.update(cx, |panel, cx| {
                panel.new_named_terminal(action.title().into(), command, Some(root.clone()), window, cx)
            })?;
            workspace.focus_panel::<agent_ui::AgentPanel>(window, cx);
            Some(thread)
        });
        let Some(thread) = thread else {
            workspace.update(cx, |workspace, cx| {
                crate::show_toast(workspace, "Zed cannot open a terminal thread in this project.", cx)
            });
            return;
        };
        self.started.insert(action, Instant::now());
        self.state.actions.threads.insert(action.key().to_string(), thread.to_key_string());
        actions::save_store(&dir, &root, &self.state.actions).log_err();
        cx.notify();
    }

    /// Everything that waits on you, with the thread that waits.
    fn gates(&self, cx: &App) -> Vec<Gate> {
        let terminals = self.claude_terminals(cx);
        let owner_of = |session: &Session| {
            terminals.iter().find(|terminal| terminal.owns(session)).and_then(|terminal| terminal.thread_id)
        };
        let title_of = |session: &Session| {
            terminals
                .iter()
                .find(|terminal| terminal.owns(session))
                .map(|terminal| terminal.title.clone())
                .unwrap_or_else(|| "Claude".to_string())
        };
        let mut gates: Vec<Gate> = self
            .state
            .specs
            .iter()
            .map(|spec| Gate {
                key: format!("spec:{}", spec.pid),
                owner: owner_of(spec),
                message: format!("{} waits on you: {}", title_of(spec), if spec.mode == "plan" { "a plan" } else { "a spec" }),
                kind: GateKind::Spec(spec.clone()),
            })
            .collect();
        if let Some(session) = self.findings_session(&terminals) {
            gates.push(Gate {
                key: format!("findings:{}", session.pid),
                owner: owner_of(&session),
                message: "Agent review waits on you: the findings".to_string(),
                kind: GateKind::Findings(session),
            });
        }
        if let (Some((total, failed)), Some((_, results))) = (self.unseen_results(), &self.state.manual_results) {
            gates.push(Gate {
                key: format!("results:{}:{}", results.target, results.ended_at),
                owner: self.action_terminal(ReviewAction::ManualTest, &terminals).and_then(|terminal| terminal.thread_id),
                message: format!("Manual test is done: {failed} of {total} failed"),
                kind: GateKind::Results,
            });
        }
        for (action, progress) in &self.state.progress {
            if progress.waiting {
                gates.push(Gate {
                    key: format!("question:{}:{}", action.key(), progress.summary.clone().unwrap_or_default()),
                    owner: self.action_terminal(*action, &terminals).and_then(|terminal| terminal.thread_id),
                    message: format!("{} waits on you in its thread", action.title()),
                    kind: GateKind::Question,
                });
            }
        }
        gates
    }

    /// A new gate of the thread that you use now opens its tab. Any other
    /// new gate marks its thread in the threads sidebar, pops up when Zed is
    /// in the background, and shows a toast. Gates that wait when Zed starts
    /// only show in Needs you.
    fn handle_new_gates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let gates = self.gates(cx);
        thread_board::set_gates(
            cx.entity_id(),
            gates
                .iter()
                .filter_map(|gate| Some((gate.owner?, gate.kind.short_label())))
                .collect(),
            cx,
        );
        let first = !self.handled_first_gates;
        self.handled_first_gates = true;
        let new: Vec<Gate> = gates.iter().filter(|gate| !self.known_gates.contains(&gate.key)).cloned().collect();
        self.known_gates = gates.into_iter().map(|gate| gate.key).collect();
        if first {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let agent_panel = workspace.read(cx).panel::<agent_ui::AgentPanel>(cx);
        let active = agent_panel.as_ref().and_then(|panel| panel.read(cx).active_terminal_id());
        for gate in new {
            let yours = gate.owner.is_some() && gate.owner == active;
            match (&gate.kind, yours || gate.owner.is_none()) {
                (GateKind::Spec(spec), true) => self.open_spec(spec.clone(), false, window, cx),
                (GateKind::Findings(session), true) if yours => self.open_findings(session.clone(), window, cx),
                _ => {
                    if let (Some(owner), Some(agent_panel)) = (gate.owner, agent_panel.as_ref()) {
                        agent_panel.update(cx, |panel, cx| panel.notify_terminal(owner, window, cx));
                    }
                    let message = gate.message.clone();
                    workspace.update(cx, |workspace, cx| crate::show_toast(workspace, message, cx));
                }
            }
        }
    }

    /// The gate 2 session of the agent review: a code review session that
    /// the "Agent review" thread started.
    fn findings_session(&self, terminals: &[start::ClaudeTerminal]) -> Option<Session> {
        let terminal = self.action_terminal(ReviewAction::AgentReview, terminals)?;
        self.state.sessions.iter().find(|session| terminal.owns(session)).cloned()
    }

    /// Opens the whole PR with the findings of the agent review, or switches to it.
    fn open_findings(&mut self, session: Session, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.state.roots.first().cloned() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let existing = workspace
            .read(cx)
            .items_of_type::<ChapterDiffView>(cx)
            .find(|view| view.read(cx).findings_session_pid() == Some(session.pid));
        if let Some(view) = existing {
            workspace.update(cx, |workspace, cx| workspace.activate_item(&view, true, true, window, cx));
            return;
        }
        let base = match self.state.pr.as_ref().map(|pr| pr.base_ref_name.as_str()) {
            Some(base) if !base.is_empty() => format!("origin/{base}"),
            _ => "origin/master".to_string(),
        };
        let title = match &self.state.pr {
            Some(pr) => format!("Findings · PR #{}", pr.number),
            None => "Findings".to_string(),
        };
        let http = self.http.clone();
        cx.spawn_in(window, async move |_, cx| {
            let url = format!("{}/api/external-annotations", session.url);
            let annotations = crate::client::get_json::<AnnotationsResponse>(&http, &url).await?.annotations;
            let files = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    let base = base.clone();
                    async move { findings::changed_files(&root, &base) }
                })
                .await?;
            let (diffs, general) = findings::split(&annotations, &files);
            let key = ChapterKey { guide_id: format!("findings-{}", session.pid), chapter: 0 };
            let intro = ChapterIntro {
                index: 0,
                count: 1,
                title: title.clone(),
                overview: String::new(),
                reviewed: false,
            };
            let task = workspace.update_in(cx, |workspace, window, cx| {
                ChapterDiffView::open(key, title, intro, root.clone(), base, diffs, workspace, window, cx)
            })?;
            let view = task.await?;
            let worktree = root.display().to_string();
            view.update_in(cx, |view, window, cx| view.set_findings(session, http, worktree, general, window, cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Results that you have not opened yet, for Needs you.
    fn unseen_results(&self) -> Option<(usize, usize)> {
        let (_, results) = self.state.manual_results.as_ref()?;
        let seen = self.state.actions.seen_results.get(&results.target);
        if seen == Some(&results.ended_at) || results.cases.is_empty() {
            return None;
        }
        let (_, failed, blocked) = results.counts();
        Some((results.cases.len(), failed + blocked))
    }

    fn open_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, results)) = self.state.manual_results.clone() else {
            return;
        };
        if let Some(root) = self.state.roots.first() {
            self.state.actions.seen_results.insert(results.target.clone(), results.ended_at.clone());
            actions::save_store(&crate::paths::actions_dir(), root, &self.state.actions).log_err();
        }
        self.workspace
            .update(cx, |workspace, cx| {
                let existing = workspace
                    .items_of_type::<ManualTestView>(cx)
                    .find(|view| view.read(cx).path() == path.as_path());
                if let Some(view) = existing {
                    workspace.activate_item(&view, true, true, window, cx);
                    return;
                }
                let view = cx.new(|cx| ManualTestView::new(path, results, cx));
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(view), true, true, None, window, cx);
                });
            })
            .log_err();
        cx.notify();
    }

    fn refresh_results_tab(&self, cx: &mut Context<Self>) {
        let Some((path, results)) = self.state.manual_results.clone() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let views: Vec<_> = workspace
            .read(cx)
            .items_of_type::<ManualTestView>(cx)
            .filter(|view| view.read(cx).path() == path.as_path())
            .collect();
        for view in views {
            view.update(cx, |view, cx| view.set_results(results.clone(), cx));
        }
    }

    fn set_hidden(&mut self, action: ReviewAction, hidden: bool, cx: &mut Context<Self>) {
        let (Some(root), Some(pr)) = (self.state.roots.first().cloned(), self.state.pr.as_ref()) else {
            return;
        };
        self.state.actions.set_hidden(action, pr.number, hidden);
        actions::save_store(&crate::paths::actions_dir(), &root, &self.state.actions).log_err();
        cx.notify();
    }

    fn render_actions(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terminals = self.claude_terminals(cx);
        let pr = self.state.pr.as_ref().map(|pr| pr.number);
        let mut cards = Vec::new();
        let mut hidden = Vec::new();
        for action in ReviewAction::ALL {
            if self.state.actions.is_hidden(action, pr) {
                hidden.push(action);
                continue;
            }
            cards.push(self.render_action_card(action, &terminals, window, cx));
        }
        let hidden_rows = hidden.into_iter().map(|action| {
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_dashed()
                .border_color(cx.theme().colors().border)
                .child(Icon::new(IconName::EyeOff).size(IconSize::XSmall).color(Color::Muted))
                .child(
                    div().flex_1().child(
                        Label::new(format!("{} is hidden for this PR", action.title()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    Button::new(SharedString::from(format!("show-{}", action.key())), "Show")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::XSmall)
                        .on_click(cx.listener(move |this, _, _, cx| this.set_hidden(action, false, cx))),
                )
                .into_any_element()
        });
        Self::render_section("Reviews".to_string(), cx)
            .child(v_flex().gap_2().children(cards).children(hidden_rows))
    }

    fn render_action_card(
        &self,
        action: ReviewAction,
        terminals: &[start::ClaudeTerminal],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors().clone();
        let collapsed = self.collapsed_cards.contains(&action);
        // A guide shows in the Guided review card also when you started it
        // without the panel.
        let guide = (action == ReviewAction::GuidedReview).then(|| self.state.guide.clone()).flatten();
        let warning = cx.theme().status().warning;
        let warning_border = cx.theme().status().warning_border;
        let terminal = self.action_terminal(action, terminals);
        let status = self.action_status(action, terminal.as_ref());
        let progress = self.state.progress.get(&action);
        let blocked = action.needs_pr() && self.state.pr.is_none();
        let icon = match action {
            ReviewAction::AgentReview => IconName::Sparkle,
            ReviewAction::ManualTest => IconName::PlayFilled,
            ReviewAction::GuidedReview => IconName::Book,
        };
        let summary = match (status, progress.and_then(|progress| progress.summary.clone())) {
            (ActionStatus::NotStarted, _) if blocked => "Needs a PR".to_string(),
            (ActionStatus::NotStarted, _) => action.description().to_string(),
            (_, Some(summary)) => summary,
            (ActionStatus::Waiting, None) => "Waits on you".to_string(),
            (ActionStatus::Done, None) => "Done".to_string(),
            (ActionStatus::Working, None) => "Claude is working".to_string(),
            (_, None) => "Claude waits for the next step".to_string(),
        };
        let chapters_pill = guide.as_ref().map(|guide| {
            let count = guide.envelope.guide.sections.len();
            let done = (0..count).filter(|ix| guide.is_reviewed(*ix)).count();
            format!("{done} of {count}")
        });
        let pill = match status {
            _ if chapters_pill.is_some() => None,
            ActionStatus::NotStarted => None,
            ActionStatus::Idle => Some(("Idle", Color::Muted)),
            ActionStatus::Working => Some(("Working", Color::Accent)),
            ActionStatus::Waiting => Some(("Waiting on you", Color::Warning)),
            ActionStatus::Done => Some(("Done", Color::Success)),
        };
        let key = action.key();
        let header = h_flex()
            .w_full()
            .gap_2()
            .child(
                div()
                    .size_7()
                    .flex_none()
                    .rounded_md()
                    .bg(colors.element_background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icon).size(IconSize::Small).color(match status {
                        ActionStatus::NotStarted => Color::Muted,
                        ActionStatus::Done => Color::Success,
                        _ => Color::Accent,
                    })),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(action.title()).weight(FontWeight::SEMIBOLD))
                    .child(Label::new(summary).size(LabelSize::XSmall).color(Color::Muted)),
            )
            .when_some(pill, |row, (text, color)| {
                row.child(
                    h_flex()
                        .gap_1()
                        .flex_none()
                        .when(status == ActionStatus::Working, |pill| {
                            pill.child(SpinnerLabel::new().size(LabelSize::XSmall).color(Color::Accent))
                        })
                        .when(status != ActionStatus::Working, |pill| {
                            pill.child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(color))
                        })
                        .child(Label::new(text).size(LabelSize::XSmall).color(color)),
                )
            })
            .when_some(chapters_pill, |row, text| {
                row.child(
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded_full()
                        .bg(warning.opacity(0.15))
                        .child(Label::new(text).size(LabelSize::XSmall).color(Color::Warning)),
                )
            })
            .when(
                (status == ActionStatus::NotStarted || status == ActionStatus::Done) && guide.is_none(),
                |row| {
                let label = if status == ActionStatus::Done { "Run again" } else { "Start" };
                row.child(
                    Button::new(SharedString::from(format!("start-{key}")), label)
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .disabled(blocked)
                        .tooltip(Tooltip::text(format!("Starts the Claude thread “{}”", action.title())))
                        .on_click(cx.listener(move |this, _, window, cx| this.start_action(action, window, cx))),
                )
                },
            )
            .when_some(
                (action == ReviewAction::AgentReview).then(|| self.findings_session(terminals)).flatten(),
                |row, session| {
                    row.child(
                        Button::new("open-findings", "Findings")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::text("Opens the PR with the findings of gate 2"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_findings(session.clone(), window, cx)
                            })),
                    )
                },
            )
            .when(action == ReviewAction::ManualTest && self.state.manual_results.is_some(), |row| {
                row.child(
                    Button::new("open-manual-results", "Results")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text("Opens every test case with its proof"))
                        .on_click(cx.listener(|this, _, window, cx| this.open_results(window, cx))),
                )
            })
            .when(self.state.pr.is_some(), |row| {
                row.child(
                    IconButton::new(SharedString::from(format!("hide-{key}")), IconName::EyeOff)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Hide for this PR"))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_hidden(action, true, cx))),
                )
            })
            .child(
                IconButton::new(
                    SharedString::from(format!("collapse-{key}")),
                    if collapsed { IconName::ChevronDown } else { IconName::ChevronUp },
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text(if collapsed { "Expand" } else { "Collapse" }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.collapsed_cards.remove(&action) {
                        this.collapsed_cards.insert(action);
                    }
                    cx.notify();
                })),
            );

        let steps: Vec<String> = match progress.filter(|progress| !progress.steps.is_empty()) {
            Some(progress) => progress.steps.clone(),
            None => action.default_steps().iter().map(|step| step.to_string()).collect(),
        };
        let current = progress.and_then(|progress| progress.step);
        let detail = progress.and_then(|progress| progress.detail.clone());
        let step_bar = (status != ActionStatus::NotStarted && guide.is_none()).then(|| {
            h_flex().w_full().gap_1().children(steps.iter().enumerate().map(|(ix, step)| {
                let done = status == ActionStatus::Done || current.is_some_and(|current| ix < current);
                let is_current = status != ActionStatus::Done && current == Some(ix);
                let bar = if done {
                    colors.version_control_added
                } else if is_current && status == ActionStatus::Waiting {
                    warning
                } else if is_current {
                    colors.text_accent
                } else {
                    colors.border
                };
                let label = match (&detail, is_current) {
                    (Some(detail), true) => detail.clone(),
                    _ => step.clone(),
                };
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().h(px(3.)).w_full().rounded_sm().bg(bar))
                    .child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(if is_current { Color::Default } else { Color::Muted })
                            .truncate(),
                    )
            }))
        });
        let thread = terminal
            .as_ref()
            .filter(|_| guide.is_none())
            .map(|terminal| Self::render_thread_label(SharedString::from(format!("thread-{key}")), Some(terminal), cx));
        // A code review session without a guide also gets its Send and
        // Approve here.
        let has_review = action == ReviewAction::GuidedReview && (guide.is_some() || !self.state.sessions.is_empty());
        let guided = has_review.then(|| {
            v_flex()
                .w_full()
                .gap_1p5()
                .children(guide.as_ref().map(|guide| self.render_chapters(guide, window, cx)))
                .child(self.render_guided_footer(cx))
        });

        v_flex()
            .w_full()
            .p_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(if status == ActionStatus::Waiting {
                warning_border
            } else {
                colors.border
            })
            .bg(colors.editor_background)
            .child(header)
            .when(!collapsed, |card| card.children(step_bar).children(thread).children(guided))
            .into_any_element()
    }

    /// Opens the flag picker with the flags of the PR picked. `confirm`
    /// shows the change at once, for a remove or a new preview.
    fn open_flag_picker(&mut self, picked: Vec<String>, confirm: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.state.roots.first().cloned() else {
            return;
        };
        let query = cx.new(|cx| {
            let mut editor = editor::Editor::single_line(window, cx);
            editor.set_placeholder_text("Search PostHog Staging flags", window, cx);
            editor
        });
        let subscription = cx.subscribe(&query, |_, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.notify();
            }
        });
        let confirm = confirm
            .then(|| self.state.tools.as_ref().map(|tools| tools.label_change(&picked)))
            .flatten();
        self.flag_picker = Some(FlagPicker {
            options: Vec::new(),
            loading: true,
            query,
            picked,
            confirm,
            applying: false,
            _query_subscription: subscription,
        });
        cx.spawn(async move |this, cx| {
            let options = cx.background_spawn(async move { tools::flag_options(&root) }).await;
            this.update(cx, |this, cx| {
                if let Some(picker) = this.flag_picker.as_mut() {
                    picker.options = options;
                    picker.loading = false;
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn current_flags(&self) -> Vec<String> {
        if self.state.pr.is_none() {
            return self
                .state
                .branch
                .as_ref()
                .and_then(|branch| self.state.actions.pending_flags.get(branch))
                .cloned()
                .unwrap_or_default();
        }
        self.state
            .tools
            .as_ref()
            .map(|tools| {
                tools
                    .labels
                    .iter()
                    .filter_map(|label| label.strip_prefix("flag:").map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn toggle_flag(&mut self, flag: String, cx: &mut Context<Self>) {
        if let Some(picker) = self.flag_picker.as_mut() {
            let key = flag.split('=').next().unwrap_or(&flag).to_string();
            let before = picker.picked.len();
            picker.picked.retain(|picked| picked.split('=').next() != Some(key.as_str()));
            if picker.picked.len() == before {
                picker.picked.push(flag);
            }
            picker.confirm = None;
            cx.notify();
        }
    }

    /// The first click shows the change. The second writes the labels.
    /// Without a PR, the flags wait in the store for Create PR.
    fn apply_flags(&mut self, cx: &mut Context<Self>) {
        if self.state.pr.is_none() {
            let (Some(root), Some(branch), Some(picker)) =
                (self.state.roots.first().cloned(), self.state.branch.clone(), self.flag_picker.take())
            else {
                return;
            };
            if picker.picked.is_empty() {
                self.state.actions.pending_flags.remove(&branch);
            } else {
                self.state.actions.pending_flags.insert(branch, picker.picked);
            }
            actions::save_store(&crate::paths::actions_dir(), &root, &self.state.actions).log_err();
            cx.notify();
            return;
        }
        let (Some(root), Some(pr), Some(tools)) =
            (self.state.roots.first().cloned(), self.state.pr.clone(), self.state.tools.clone())
        else {
            return;
        };
        let Some(picker) = self.flag_picker.as_mut() else {
            return;
        };
        let Some(change) = picker.confirm.clone() else {
            picker.confirm = Some(tools.label_change(&picker.picked));
            cx.notify();
            return;
        };
        if change.is_empty() {
            self.flag_picker = None;
            cx.notify();
            return;
        }
        picker.applying = true;
        let workspace = self.workspace.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { tools::apply_labels(&root, pr.number, &change) })
                .await;
            let message = match &result {
                Ok(()) => format!("Labels changed on #{}. A running preview restarts in about 2 minutes.", pr.number),
                Err(error) => format!("Labels not changed: {error:#}"),
            };
            this.update(cx, |this, cx| {
                if result.is_ok() {
                    this.flag_picker = None;
                    // The next poll reads the labels again.
                    this.state.pr_checked_at = None;
                } else if let Some(picker) = this.flag_picker.as_mut() {
                    picker.applying = false;
                }
                cx.notify();
            })?;
            workspace.update(cx, |workspace, cx| crate::show_toast(workspace, message, cx))
        })
        .detach_and_log_err(cx);
        cx.notify();
    }

    fn start_create_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(branch)) = (self.state.roots.first().cloned(), self.state.branch.clone()) else {
            return;
        };
        let flags = self.state.actions.pending_flags.get(&branch).cloned().unwrap_or_default();
        let ticket = actions::ticket_in_branch(&branch);
        let command = format!("claude {}", shell_quote(&actions::create_pr_prompt(ticket.as_deref(), &flags)));
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let thread = workspace.update(cx, |workspace, cx| {
            let agent_panel = workspace.panel::<agent_ui::AgentPanel>(cx)?;
            let thread = agent_panel.update(cx, |panel, cx| {
                panel.new_named_terminal("Create PR".into(), command, Some(root.clone()), window, cx)
            })?;
            workspace.focus_panel::<agent_ui::AgentPanel>(window, cx);
            Some(thread)
        });
        if let Some(thread) = thread {
            self.state.actions.threads.insert(actions::CREATE_PR_KEY.to_string(), thread.to_key_string());
            actions::save_store(&crate::paths::actions_dir(), &root, &self.state.actions).log_err();
            cx.notify();
        }
    }

    /// Instead of the PR: the branch, and a Create PR button that starts a
    /// Claude thread with that name.
    fn render_no_pr(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let branch = self.state.branch.clone()?;
        if self.state.pr.is_some() || matches!(branch.as_str(), "HEAD" | "main" | "master") {
            return None;
        }
        let colors = cx.theme().colors().clone();
        let (ahead, pushed) = self.state.branch_ahead.unwrap_or((0, false));
        let ticket = actions::ticket_in_branch(&branch);
        let terminals = self.claude_terminals(cx);
        let thread = self.state.actions.threads.get(actions::CREATE_PR_KEY).and_then(|id| {
            terminals
                .iter()
                .find(|terminal| terminal.thread_id.is_some_and(|thread| thread.to_key_string() == *id))
                .cloned()
        });
        let mut meta = format!("{ahead} commits ahead of master");
        meta.push_str(if pushed { " · pushed" } else { " · not pushed" });
        if let Some(ticket) = &ticket {
            meta.push_str(&format!(" · {ticket}"));
        }
        Some(
            v_flex()
                .m_3()
                .p_3()
                .gap_2()
                .items_center()
                .rounded_md()
                .border_1()
                .border_dashed()
                .border_color(colors.border)
                .child(Icon::new(IconName::GitBranch).size(IconSize::Medium).color(Color::Muted))
                .child(Label::new("No pull request for this branch").weight(FontWeight::SEMIBOLD))
                .child(
                    Label::new("Agent review and Guided review need a PR on GitHub. Manual test runs on the local branch.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    v_flex()
                        .w_full()
                        .p_1p5()
                        .rounded_sm()
                        .bg(colors.element_background)
                        .child(Label::new(branch).size(LabelSize::Small).buffer_font(cx))
                        .child(Label::new(meta).size(LabelSize::XSmall).color(Color::Muted)),
                )
                .map(|card| match thread {
                    Some(thread) => card.child(Self::render_thread_label("create-pr-thread", Some(&thread), cx)),
                    None => card.child(
                        Button::new("create-pr", "Create PR")
                            .style(ButtonStyle::Filled)
                            .tooltip(Tooltip::text("Starts the Claude thread “Create PR”. It asks before it opens the PR."))
                            .on_click(cx.listener(|this, _, window, cx| this.start_create_pr(window, cx))),
                    ),
                }),
        )
    }

    fn open_telemetry(&mut self, local: &tools::LocalN8n, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let command = local.telemetry_command();
        start::run_in_terminal(&workspace, "Telemetry", command, false, window, cx);
    }

    fn start_local_n8n(&mut self, local: &tools::LocalN8n, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let label = local.backend_task_label();
        start::run_in_terminal(&workspace, &label, local.backend_command(), true, window, cx);
    }

    fn render_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let tools = self.state.tools.clone();
        let has_pr = self.state.pr.is_some();
        let tool_row = |icon: IconName, name: &str, state: String, color: Color| {
            h_flex()
                .w_full()
                .gap_2()
                .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(Label::new(name.to_string()).size(LabelSize::Small))
                        .child(Label::new(state).size(LabelSize::XSmall).color(color).truncate()),
                )
        };

        let preview = tools.as_ref().and_then(|tools| tools.preview.clone());
        let deploying = tools.as_ref().is_some_and(|tools| tools.has_preview()) && preview.is_none();
        let preview_row = tool_row(
            IconName::Public,
            "Preview env",
            match (&preview, deploying, has_pr) {
                (Some(preview), _, _) => format!("Live · {}", preview.url),
                (None, true, _) => "Deploying".to_string(),
                (None, false, true) => "Not deployed".to_string(),
                (None, false, false) => "Needs a PR".to_string(),
            },
            if preview.is_some() { Color::Success } else if deploying { Color::Accent } else { Color::Muted },
        )
        .map(|row| match preview.clone() {
            Some(preview) => row.child(
                Button::new("open-preview", "Open")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .on_click(move |_, _, cx| cx.open_url(&preview.url)),
            ),
            None if !deploying => row.child(
                Button::new("create-preview", "Create")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .disabled(!has_pr || tools.is_none())
                    .on_click(cx.listener(|this, _, window, cx| {
                        let picked = this.current_flags();
                        this.open_flag_picker(picked, true, window, cx)
                    })),
            ),
            None => row,
        });

        let flags = tools.as_ref().map(|tools| tools.flag_labels()).unwrap_or_default();
        let pending = if has_pr { Vec::new() } else { self.current_flags() };
        let flags_row = tool_row(
            IconName::Settings,
            "Feature flags",
            match (has_pr, flags.is_empty(), pending.is_empty()) {
                (false, _, true) => "Pick flags now. Create PR adds them as labels.".to_string(),
                (false, _, false) => format!("For Create PR: {}", pending.join(", ")),
                (true, true, _) => "No flag: labels on the PR".to_string(),
                (true, false, _) => "PR labels · PostHog Staging".to_string(),
            },
            Color::Muted,
        )
        .when(self.flag_picker.is_none(), |row| {
            row.child(
                Button::new("edit-flags", "Edit")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .disabled(has_pr && tools.is_none())
                    .on_click(cx.listener(|this, _, window, cx| {
                        let picked = this.current_flags();
                        this.open_flag_picker(picked, false, window, cx)
                    })),
            )
        });
        let flag_rows = flags.into_iter().map(|(label, state)| {
            let (text, color) = match state {
                FlagState::InPreview => ("on in preview", Color::Success),
                FlagState::Pending => ("restarts, ~2 min", Color::Accent),
                FlagState::NoPreview => ("no preview yet", Color::Muted),
            };
            let key = label.strip_prefix("flag:").unwrap_or(&label).to_string();
            h_flex()
                .w_full()
                .pl_6()
                .gap_2()
                .child(div().flex_1().min_w_0().child(Label::new(label.clone()).size(LabelSize::XSmall).buffer_font(cx).truncate()))
                .child(Label::new(text).size(LabelSize::XSmall).color(color))
                .child(
                    IconButton::new(SharedString::from(format!("remove-{label}")), IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text(format!("Remove the label {label}")))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let picked: Vec<String> =
                                this.current_flags().into_iter().filter(|flag| *flag != key).collect();
                            this.open_flag_picker(picked, true, window, cx)
                        })),
                )
                .into_any_element()
        });

        let local = self.state.local_n8n.clone();
        let telemetry_row = local.clone().and_then(|local| {
            let capture = local.telemetry.clone()?;
            Some(
                tool_row(
                    IconName::Terminal,
                    "Telemetry",
                    if capture.running {
                        format!("Capture on port {} · this worktree only", capture.port)
                    } else {
                        "Capture stopped · Open starts it".to_string()
                    },
                    if capture.running { Color::Accent } else { Color::Muted },
                )
                .child(
                    Button::new("open-telemetry", "Open")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text(
                            "Opens a terminal with the PostHog events of this worktree's n8n",
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_telemetry(&local, window, cx)
                        })),
                ),
            )
        });
        let local_row = local.map(|local| {
            let start_local = local.clone();
            tool_row(
                IconName::Terminal,
                "Local n8n",
                if local.running {
                    format!("Running on port {}", local.port)
                } else {
                    format!("Stopped · port {}", local.port)
                },
                if local.running { Color::Accent } else { Color::Muted },
            )
            .when(local.running, |row| {
                let url = format!("http://localhost:{}", local.port);
                row.child(
                    Button::new("open-local-n8n", "Open")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
            .when(!local.running, |row| {
                row.child(
                    Button::new("start-local-n8n", "Start")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text("Runs the dev:be task of this worktree in a terminal"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.start_local_n8n(&start_local, window, cx)
                        })),
                )
            })
        });

        Self::render_section("Tools".to_string(), cx).child(
            v_flex()
                .w_full()
                .p_2()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .child(preview_row)
                .child(flags_row)
                .children(flag_rows)
                .children(self.render_flag_picker(cx))
                .children(local_row)
                .children(telemetry_row),
        )
    }

    fn render_flag_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.flag_picker.as_ref()?;
        let colors = cx.theme().colors().clone();
        let query = picker.query.read(cx).text(cx).trim().to_lowercase();
        let picked_keys: Vec<&str> = picker.picked.iter().map(|flag| flag.split('=').next().unwrap_or(flag)).collect();
        let mut shown: Vec<String> = picker.picked.clone();
        shown.extend(
            picker
                .options
                .iter()
                .filter(|option| !picked_keys.contains(&option.as_str()))
                .filter(|option| query.is_empty() || option.to_lowercase().contains(&query))
                .take(12)
                .cloned(),
        );
        // A key that is not in PostHog can be typed in, with `=false` or `=<variant>`.
        let typed = (!query.is_empty() && !shown.iter().any(|flag| flag.to_lowercase() == query))
            .then(|| picker.query.read(cx).text(cx).trim().to_string());
        let rows = shown.into_iter().chain(typed).map(|flag| {
            let key = flag.split('=').next().unwrap_or(&flag).to_string();
            let on = picked_keys.contains(&key.as_str());
            h_flex()
                .gap_1p5()
                .child(
                    Checkbox::new(SharedString::from(format!("flag-{flag}")), if on { ToggleState::Selected } else { ToggleState::Unselected })
                        .on_click(cx.listener({
                            let flag = flag.clone();
                            move |this, _, _, cx| this.toggle_flag(flag.clone(), cx)
                        })),
                )
                .child(Label::new(flag).size(LabelSize::XSmall).buffer_font(cx))
                .into_any_element()
        });
        let confirm = picker.confirm.clone();
        let confirm_text = confirm.as_ref().map(|change| {
            if change.is_empty() {
                "Nothing to change.".to_string()
            } else {
                let mut parts = Vec::new();
                if !change.add.is_empty() {
                    parts.push(format!("Add {}.", change.add.join(", ")));
                }
                if !change.remove.is_empty() {
                    parts.push(format!("Remove {}.", change.remove.join(", ")));
                }
                parts.push("The preview restarts with the new flags in about 2 minutes.".to_string());
                parts.join(" ")
            }
        });
        Some(
            v_flex()
                .w_full()
                .p_2()
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_color(colors.border)
                .bg(colors.editor_background)
                .child(
                    div()
                        .px_1()
                        .py_0p5()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_focused)
                        .child(picker.query.clone()),
                )
                .when(picker.loading, |list| {
                    list.child(Label::new("Loading flags…").size(LabelSize::XSmall).color(Color::Muted))
                })
                .children(rows)
                .child(
                    Label::new("Newest first, disabled flags too. Type key=false or key=variant by hand.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .when_some(confirm_text, |list, text| {
                    list.child(
                        div()
                            .p_1p5()
                            .rounded_sm()
                            .bg(colors.element_selected)
                            .child(Label::new(text).size(LabelSize::XSmall)),
                    )
                })
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("apply-flags", if confirm.is_some() { "Apply labels" } else { "Review change" })
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .disabled(picker.applying)
                                .on_click(cx.listener(|this, _, _, cx| this.apply_flags(cx))),
                        )
                        .child(
                            Button::new("cancel-flags", "Cancel")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.flag_picker = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
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

    /// The bottom of the Guided review card: the session, the Claude thread
    /// that gets the review, and Send feedback and Approve.
    fn render_guided_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::XSmall)
                    .truncate(true)
                    .start_icon(
                        Icon::new(if busy { IconName::LoadCircle } else { IconName::Terminal })
                            .size(IconSize::XSmall)
                            .color(if busy { Color::Accent } else { Color::Muted }),
                    )
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted)),
                Tooltip::text("The Claude thread that gets your review"),
            );

        let has_session = session.is_some();
        let unsent = match self.comment_count {
            0 => "No comments yet".to_string(),
            1 => "1 comment not sent".to_string(),
            count => format!("{count} comments not sent"),
        };
        v_flex()
            .w_full()
            .gap_1p5()
            .pt_1p5()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .child(if busy && code_sent.is_some() {
                        SpinnerLabel::new().size(LabelSize::XSmall).color(Color::Accent).into_any_element()
                    } else {
                        Icon::new(IconName::Circle).size(IconSize::XSmall).color(status_color).into_any_element()
                    })
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(status)
                                .size(LabelSize::XSmall)
                                .color(if status_color == Color::Muted { Color::Muted } else { status_color })
                                .truncate(),
                        ),
                    )
                    .child(div().flex_none().max_w(px(150.)).child(chip)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(unsent).size(LabelSize::Small).color(Color::Muted).truncate()),
                    )
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
                    })
                    .when(!can_start, |row| {
                        row.child(
                            Button::new("plannotator-send", "Send feedback")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .disabled(!has_session || self.comment_count == 0)
                                .tooltip(Tooltip::text(
                                    "Posts to the Plannotator session that Claude waits on. Claude decides whether to post on GitHub.",
                                ))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(SendReviewToAgent.boxed_clone(), cx);
                                }),
                        )
                        .child(
                            Button::new("plannotator-approve", "Approve")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .disabled(!has_session)
                                .on_click(cx.listener(|this, _, window, cx| this.approve(window, cx))),
                        )
                    }),
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
        let mut rows = Vec::with_capacity(sections.len());
        for (ix, section) in sections.iter().enumerate() {
            let overview = self.overviews.get(&(guide.id.clone(), ix)).cloned();
            rows.push(self.render_chapter(ix, section, overview, guide.is_reviewed(ix), window, cx));
        }
        v_flex().w_full().gap_0p5().children(rows)
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
        let notes: usize = section.diffs.iter().map(|diff| diff.notes.len()).sum();
        let (added, removed) = section.diffs.iter().fold((0, 0), |(added, removed), diff| {
            let stat = self.state.file_stats.get(&diff.file).copied().unwrap_or_default();
            (added + stat.added, removed + stat.removed)
        });

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
                div().flex_1().min_w_0().child(
                    Label::new(section.title.clone())
                        .color(if reviewed { Color::Muted } else { Color::Default })
                        .truncate(),
                ),
            )
            .when(notes > 0, |row| {
                row.child(Label::new(format!("✦ {notes}")).size(LabelSize::Small).color(Color::Muted))
            })
            .when(added > 0, |row| {
                row.child(Label::new(format!("+{added}")).size(LabelSize::Small).color(Color::VersionControlAdded))
            })
            .when(removed > 0, |row| {
                row.child(Label::new(format!("−{removed}")).size(LabelSize::Small).color(Color::VersionControlDeleted))
            })
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

/// How long a session has waited, such as `12 min`, from its RFC 3339 start.
fn waited_label(started_at: &str) -> String {
    let Ok(started) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return "waits".to_string();
    };
    let minutes = (chrono::Utc::now() - started.with_timezone(&chrono::Utc)).num_minutes().max(0);
    match minutes {
        0 => "now".to_string(),
        1..=59 => format!("{minutes} min"),
        _ => format!("{} h", minutes / 60),
    }
}

/// `text` as one single-quoted shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
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
        v_flex()
            .key_context("PlannotatorReviewPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .id("plannotator-review-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(self.render_specs(cx))
                    .children(self.render_no_pr(cx))
                    .child(self.render_actions(window, cx))
                    .child(self.render_tools(cx)),
            )
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
        Some("Review")
    }

    /// The count of what waits on you, on the dock button.
    fn icon_label(&self, _: &Window, cx: &App) -> Option<String> {
        let count = self.gates(cx).len();
        (count > 0).then(|| count.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        20
    }
}
