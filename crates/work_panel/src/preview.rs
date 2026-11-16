use crate::{chip, is_ready, priority_label, section, start, status_chip, worktree_chip};
use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, Render, Task,
    WeakEntity, Window,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use n8n_worktree_manager::{
    Manager,
    github::{Checks, PrStatus},
    linear::{IssueRow, ProjectView},
};
use serde_json::Value;
use std::sync::Arc;
use ui::{ProgressBar, TintColor, prelude::*};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

/// A 15 px heading, like the mockups' `h2`.
fn heading(text: &'static str) -> Div {
    div()
        .text_size(px(15.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(text)
}

fn title(text: String) -> Div {
    div()
        .text_size(px(24.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(text)
}

/// A bordered card of the right column.
fn side_card(cx: &App) -> Div {
    v_flex()
        .gap_1p5()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().colors().border)
}

fn link(id: impl Into<ElementId>, text: String, url: String) -> Button {
    Button::new(id, text)
        .style(ButtonStyle::Transparent)
        .color(Color::Accent)
        .label_size(LabelSize::Small)
        .on_click(move |_, _, cx| cx.open_url(&url))
}

/// "Nov 30, 2026" from "2026-11-30".
fn long_date(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|date| date.format("%b %-d, %Y").to_string())
        .unwrap_or_else(|_| date.to_string())
}

fn short_date(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|date| date.format("%b %-d").to_string())
        .unwrap_or_else(|_| date.to_string())
}

/// "2d", "5h", from an RFC 3339 time.
fn age(time: &str) -> String {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(time) else {
        return String::new();
    };
    let minutes = (chrono::Utc::now() - time.with_timezone(&chrono::Utc)).num_minutes();
    match minutes {
        ..60 => format!("{}m", minutes.max(0)),
        60..1440 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}

/// One Linear issue in a tab of the center pane. The Work panel reuses the
/// tab for the next issue, like a preview tab.
pub struct IssuePreview {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    manager: Arc<Manager>,
    row: IssueRow,
    pr: Option<PrStatus>,
    detail: Option<IssueDetail>,
    error: Option<String>,
    _load: Task<()>,
}

struct IssueDetail {
    description: Option<Entity<Markdown>>,
    milestone: Option<(String, f64)>,
    assigned_to_me: bool,
    target_date: Option<String>,
    documents: Vec<(String, String)>,
    children: Vec<(String, String)>,
    comments: Vec<(String, String, Entity<Markdown>)>,
}

impl IssuePreview {
    pub fn new(
        row: IssueRow,
        pr: Option<PrStatus>,
        manager: Arc<Manager>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut preview = Self {
            focus_handle: cx.focus_handle(),
            workspace,
            manager,
            row: row.clone(),
            pr: None,
            detail: None,
            error: None,
            _load: Task::ready(()),
        };
        preview.show(row, pr, cx);
        preview
    }

    /// Shows another issue in this tab, or the same issue with fresh data.
    pub fn show(&mut self, row: IssueRow, pr: Option<PrStatus>, cx: &mut Context<Self>) {
        let key = row.issue.identifier.clone();
        let same = key == self.row.issue.identifier && self.detail.is_some();
        self.row = row;
        self.pr = pr;
        if same {
            cx.notify();
            return;
        }
        self.detail = None;
        self.error = None;
        let manager = self.manager.clone();
        let worktree = self.row.worktree.clone();
        self._load = cx.spawn(async move |this, cx| {
            let (answer, pr) = cx
                .background_spawn(async move {
                    let answer = manager.linear().issue(&key);
                    let pr = worktree.and_then(|name| manager.pr_status(&name).ok().flatten());
                    (answer, pr)
                })
                .await;
            this.update(cx, |preview, cx| {
                match answer {
                    Ok(issue) => preview.detail = Some(IssueDetail::new(&issue, cx)),
                    Err(error) => preview.error = Some(format!("{error:#}")),
                }
                if pr.is_some() {
                    preview.pr = pr;
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mine = self.detail.as_ref().is_some_and(|detail| detail.assigned_to_me);
        start::start_work(
            self.workspace.clone(),
            self.manager.clone(),
            self.row.clone(),
            mine,
            window,
            cx,
        );
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let url = self.row.issue.url.clone();
        let branch = self.row.issue.branch_name.clone();
        let row = self.row.clone();
        let start_label = if self.row.worktree.is_some() {
            "Open worktree ↵"
        } else {
            "Create worktree ↵"
        };
        h_flex()
            .gap_2()
            .child(
                Button::new("issue-preview-start", start_label)
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .on_click(cx.listener(|preview, _, window, cx| preview.start(window, cx))),
            )
            .child(
                Button::new("issue-preview-linear", "Open in Linear")
                    .style(ButtonStyle::Outlined)
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
            .when_some(branch, |this, branch| {
                this.child(
                    Button::new("issue-preview-branch", "Copy branch")
                        .style(ButtonStyle::Outlined)
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()))
                        }),
                )
            })
            .child(
                Button::new("issue-preview-claude", "Ask Claude about it")
                    .style(ButtonStyle::Outlined)
                    .on_click(cx.listener(move |preview, _, window, cx| {
                        start::ask_claude(preview.workspace.clone(), &row, window, cx);
                    })),
            )
    }

    fn render_main(&self, window: &Window, cx: &App) -> AnyElement {
        if let Some(error) = &self.error {
            return Label::new(error.clone()).color(Color::Error).into_any_element();
        }
        let Some(detail) = &self.detail else {
            return Label::new("Loading the issue from Linear…")
                .color(Color::Muted)
                .into_any_element();
        };
        let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        let mut main = v_flex().flex_1().min_w_0().gap_3();
        main = main.child(heading("Description")).child(match &detail.description {
            Some(description) => {
                MarkdownElement::new(description.clone(), style.clone()).into_any_element()
            }
            None => Label::new("No description.")
                .color(Color::Muted)
                .into_any_element(),
        });
        if !detail.children.is_empty() {
            main = main.child(div().mt_2().child(heading("Sub-issues"))).child(
                v_flex().gap_1p5().children(detail.children.iter().map(|(key, title)| {
                    h_flex()
                        .gap_1p5()
                        .child(Label::new("○").color(Color::Muted))
                        .child(
                            Label::new(key.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                        .child(Label::new(title.clone()))
                })),
            );
        }
        if !detail.comments.is_empty() {
            main = main.child(div().mt_2().child(heading("Latest comments"))).child(
                v_flex().gap_2().children(detail.comments.iter().map(|(author, at, body)| {
                    v_flex()
                        .gap_1()
                        .px_3()
                        .py_2p5()
                        .rounded_md()
                        .bg(cx.theme().colors().element_background)
                        .child(
                            Label::new(format!("{author} · {at}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(MarkdownElement::new(body.clone(), style.clone()))
                })),
            );
        }
        main.into_any_element()
    }

    fn render_side(&self, cx: &App) -> impl IntoElement {
        let issue = &self.row.issue;
        let detail = self.detail.as_ref();
        let mut project_line = Vec::new();
        if let Some(target) = detail.and_then(|detail| detail.target_date.as_deref()) {
            project_line.push(format!("Target {}", short_date(target)));
        }
        if let Some((name, percent)) = detail.and_then(|detail| detail.milestone.as_ref()) {
            let short = name.split(':').next().unwrap_or(name);
            project_line.push(format!("{short} {percent:.0}%"));
        }
        let documents = detail.map(|detail| detail.documents.clone()).unwrap_or_default();
        let pr_line = match &self.pr {
            Some(pr) => {
                let checks = match pr.checks {
                    Checks::Passing => "CI passing",
                    Checks::Failing => "CI failing",
                    Checks::Pending => "CI running",
                    Checks::None => "no CI",
                };
                let state = match pr.state.as_str() {
                    "MERGED" => "merged".to_string(),
                    "CLOSED" => "closed".to_string(),
                    _ => checks.to_string(),
                };
                Some((format!("PR #{} · {state}", pr.number), pr.url.clone()))
            }
            None => None,
        };
        v_flex()
            .gap_3()
            .when_some(issue.project.clone(), |this, project| {
                this.child(
                    side_card(cx)
                        .child(section("Project").p_0())
                        .child(Label::new(project))
                        .when(!project_line.is_empty(), |this| {
                            this.child(
                                Label::new(project_line.join(" · "))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        })
                        .children(documents.into_iter().enumerate().map(|(ix, (title, url))| {
                            div().child(link(("issue-preview-document", ix), title, url))
                        })),
                )
            })
            .child(
                side_card(cx)
                    .child(section("Work").p_0())
                    .child(match &self.row.worktree {
                        Some(name) => h_flex()
                            .gap_2()
                            .child(Label::new(format!("⎇ {name}")))
                            .children(worktree_chip(&self.row, cx).filter(|_| {
                                self.row.setup.as_deref().is_some_and(|setup| setup != "done")
                            }))
                            .into_any_element(),
                        None => Label::new("No worktree yet").color(Color::Muted).into_any_element(),
                    })
                    .child(match pr_line {
                        Some((text, url)) => div().child(link("issue-preview-pr", text, url)).into_any_element(),
                        None => Label::new("No PR yet").color(Color::Muted).into_any_element(),
                    }),
            )
    }
}

impl IssueDetail {
    fn new(issue: &Value, cx: &mut App) -> Self {
        let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
        let nodes = |value: &Value| value["nodes"].as_array().cloned().unwrap_or_default();
        let description = issue["description"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(|text| {
                let text = text.to_string();
                cx.new(|cx| Markdown::new(text.into(), None, None, cx))
            });
        let milestone = issue["projectMilestone"]["name"].as_str().map(|name| {
            (
                name.to_string(),
                issue["projectMilestone"]["progress"].as_f64().unwrap_or_default(),
            )
        });
        let children = nodes(&issue["children"])
            .iter()
            .map(|child| (text(&child["identifier"]), text(&child["title"])))
            .collect();
        let mut comments = nodes(&issue["comments"]);
        comments.sort_by(|a, b| text(&b["createdAt"]).cmp(&text(&a["createdAt"])));
        let comments = comments
            .iter()
            .take(5)
            .map(|comment| {
                let body = text(&comment["body"]);
                let author = comment["user"]["name"]
                    .as_str()
                    .unwrap_or("Someone")
                    .to_string();
                let at = age(&text(&comment["createdAt"]));
                (author, at, cx.new(|cx| Markdown::new(body.into(), None, None, cx)))
            })
            .collect();
        let documents = nodes(&issue["project"]["documents"])
            .iter()
            .map(|document| (text(&document["title"]), text(&document["url"])))
            .collect();
        Self {
            description,
            milestone,
            assigned_to_me: issue["assignee"]["isMe"].as_bool().unwrap_or_default(),
            target_date: issue["project"]["targetDate"].as_str().map(str::to_string),
            documents,
            children,
            comments,
        }
    }
}

impl Render for IssuePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let issue = self.row.issue.clone();
        let milestone = self
            .detail
            .as_ref()
            .and_then(|detail| detail.milestone.as_ref().map(|(name, _)| name.clone()));
        let assignee = match (&self.detail, &issue.assignee) {
            (Some(detail), _) if detail.assigned_to_me => "Assigned to you".to_string(),
            (_, Some(name)) => name.clone(),
            (_, None) => "Unassigned".to_string(),
        };
        v_flex()
            .id("issue-preview")
            .key_context("IssuePreview")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|preview, _: &crate::StartWork, window, cx| preview.start(window, cx)))
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .max_w(px(860.))
                    .px_10()
                    .py_7()
                    .gap_4()
                    .child(
                        v_flex()
                            .gap_2p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        Label::new(issue.identifier.clone())
                                            .color(Color::Muted)
                                            .buffer_font(cx),
                                    )
                                    .when_some(issue.project.clone(), |this, project| {
                                        this.child(Label::new("·").color(Color::Muted))
                                            .child(Label::new(project).color(Color::Muted))
                                    })
                                    .when_some(milestone, |this, milestone| {
                                        this.child(Label::new("›").color(Color::Muted))
                                            .child(Label::new(milestone).color(Color::Muted))
                                    }),
                            )
                            .child(title(issue.title.clone()))
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .flex_wrap()
                                    .child(status_chip(&issue.status, cx))
                                    .child(chip(assignee, None, cx))
                                    .child(chip(priority_label(issue.priority), None, cx))
                                    .when_some(issue.branch_name.clone(), |this, branch| {
                                        this.child(chip(branch, None, cx))
                                    }),
                            ),
                    )
                    .child(self.render_actions(cx))
                    .child(self.render_side(cx))
                    .child(self.render_main(window, cx)),
            )
    }
}

impl Focusable for IssuePreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for IssuePreview {}

impl Item for IssuePreview {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::SquareDot).color(Color::Custom(purple())))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.row.issue.identifier.clone().into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.row.issue.title.clone().into())
    }

    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
}

/// The ◆ color of issue tabs in the mockups.
fn purple() -> Hsla {
    gpui::rgb(0xc678dd).into()
}

/// One Linear project in a tab of the center pane: milestones, the
/// description, the latest update, documents and your work in it. The Work
/// panel reuses the tab.
pub struct ProjectPreview {
    focus_handle: FocusHandle,
    project: ProjectView,
    rows: Vec<IssueRow>,
    description: Option<Entity<Markdown>>,
    update: Option<Entity<Markdown>>,
}

impl ProjectPreview {
    pub fn new(project: ProjectView, rows: Vec<IssueRow>, cx: &mut Context<Self>) -> Self {
        let mut preview = Self {
            focus_handle: cx.focus_handle(),
            project: project.clone(),
            rows: rows.clone(),
            description: None,
            update: None,
        };
        preview.show(project, rows, cx);
        preview
    }

    pub fn show(&mut self, project: ProjectView, rows: Vec<IssueRow>, cx: &mut Context<Self>) {
        let markdown = |text: String, cx: &mut Context<Self>| {
            cx.new(|cx| Markdown::new(text.into(), None, None, cx))
        };
        self.description = project.description.clone().map(|text| markdown(text, cx));
        self.update = project
            .latest_update
            .as_ref()
            .map(|update| markdown(update.body.clone(), cx));
        self.project = project;
        self.rows = rows;
        cx.notify();
    }
}

impl Render for ProjectPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self.project.clone();
        let style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        let state_chip = match project.summary.state.as_str() {
            "started" => chip("◐ In Progress", Some(Color::Warning), cx),
            "completed" => chip("● Completed", Some(Color::Success), cx),
            "planned" => chip("○ Planned", None, cx),
            "paused" => chip("‖ Paused", Some(Color::Muted), cx),
            other => chip(other.to_string(), None, cx),
        };
        let lead = if project.summary.lead_is_me {
            Some("Lead: you".to_string())
        } else {
            project.lead.clone().map(|lead| format!("Lead: {lead}"))
        };
        let worktree_keys: Vec<String> = self
            .rows
            .iter()
            .filter(|row| row.worktree.is_some())
            .map(|row| row.issue.identifier.clone())
            .collect();
        let ready = self.rows.iter().filter(|row| is_ready(&row.issue.status)).count();
        let success = cx.theme().status().success;
        let accent = cx.theme().colors().text_accent;
        let track = cx.theme().colors().border;
        let mut breadcrumb = Vec::new();
        if let Some(team) = &project.summary.team {
            breadcrumb.push(team.clone());
        }
        breadcrumb.push("Project".to_string());
        v_flex()
            .id("project-preview")
            .key_context("ProjectPreview")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .px_10()
                    .py_7()
                    .gap_5()
                    .child(
                        v_flex()
                            .gap_2p5()
                            .child(Label::new(breadcrumb.join(" · ")).color(Color::Muted))
                            .child(title(project.name.clone()))
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .flex_wrap()
                                    .child(state_chip)
                                    .when_some(lead, |this, lead| this.child(chip(lead, None, cx)))
                                    .when_some(project.target_date.clone(), |this, target| {
                                        this.child(chip(format!("Target {}", long_date(&target)), None, cx))
                                    }),
                            ),
                    )
                    .child(
                        h_flex()
                            .items_start()
                            .gap_7()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_3p5()
                                    .when(!project.milestones.is_empty(), |this| {
                                        this.child(heading("Milestones")).child(
                                            v_flex().gap_1p5().children(
                                                project.milestones.iter().enumerate().map(|(ix, milestone)| {
                                                    let done = milestone.percent >= 100.0;
                                                    h_flex()
                                                        .gap_3()
                                                        .child(
                                                            div().w(px(280.)).child(
                                                                Label::new(milestone.name.clone()),
                                                            ),
                                                        )
                                                        .child(
                                                            div().flex_1().child(
                                                                ProgressBar::new(
                                                                    ("project-milestone", ix),
                                                                    milestone.percent as f32,
                                                                    100.,
                                                                    cx,
                                                                )
                                                                .fg_color(if done { success } else { accent })
                                                                .bg_color(track),
                                                            ),
                                                        )
                                                        .child(
                                                            div().w(px(40.)).flex().justify_end().child(
                                                                Label::new(format!("{:.0}%", milestone.percent))
                                                                    .color(Color::Muted),
                                                            ),
                                                        )
                                                }),
                                            ),
                                        )
                                    })
                                    .when_some(self.description.clone(), |this, description| {
                                        this.child(
                                            div()
                                                .px_3p5()
                                                .py_3()
                                                .rounded_lg()
                                                .bg(cx.theme().colors().element_background)
                                                .child(MarkdownElement::new(description, style.clone())),
                                        )
                                    })
                                    .child(div().mt_1p5().child(heading("Latest update")))
                                    .child(
                                        match self.update.clone().zip(project.latest_update.clone()) {
                                            Some((body, update)) => v_flex()
                                                .gap_1()
                                                .child(
                                                    Label::new(format!("{} · {}", update.author, age(&update.created_at)))
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                                .child(MarkdownElement::new(body, style.clone()))
                                                .into_any_element(),
                                            None => Label::new("No update yet.")
                                                .color(Color::Muted)
                                                .into_any_element(),
                                        },
                                    ),
                            )
                            .child(
                                v_flex()
                                    .w(px(260.))
                                    .flex_none()
                                    .gap_3()
                                    .when(!project.documents.is_empty(), |this| {
                                        this.child(
                                            side_card(cx).gap_2().child(section("Documents").p_0()).children(
                                                project.documents.iter().enumerate().map(|(ix, document)| {
                                                    div().child(link(
                                                        ("project-preview-document", ix),
                                                        document.title.clone(),
                                                        document.url.clone(),
                                                    ))
                                                }),
                                            ),
                                        )
                                    })
                                    .child(
                                        side_card(cx)
                                            .child(section("My work here").p_0())
                                            .child(Label::new(match worktree_keys.len() {
                                                0 => "No worktree".to_string(),
                                                1 => format!("1 worktree · {}", worktree_keys.join(", ")),
                                                count => format!("{count} worktrees · {}", worktree_keys.join(", ")),
                                            }))
                                            .child(
                                                Label::new(format!(
                                                    "{ready} issue{} ready for pick up",
                                                    if ready == 1 { "" } else { "s" }
                                                ))
                                                .color(Color::Muted),
                                            ),
                                    ),
                            ),
                    ),
            )
    }
}

impl Focusable for ProjectPreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for ProjectPreview {}

impl Item for ProjectPreview {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ListTree).color(Color::Accent))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.project.name.clone().into()
    }

    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
}
