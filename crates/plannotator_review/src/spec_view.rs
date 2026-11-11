//! The spec review tab: a markdown spec from a Plannotator annotate session,
//! with comments on blocks, answers to `:::question` blocks, the changes
//! since the last round, and Approve or Send feedback.

use crate::{
    panel::send_json,
    rich_text::render_mermaid,
    session::Session,
    spec_doc::{self, Answer, Question, QuestionKind, SpecBlock, SpecComment},
    spec_store::{self, PreviousComment, SpecRecord},
};
use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use futures::AsyncReadExt as _;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    IntoElement, Render, SharedString, Subscription, Task, Window, px,
};
use http_client::{AsyncBody, HttpClient, Method, Request};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use ui::{TintColor, Tooltip, prelude::*};
use workspace::{Item, item::ItemEvent};

pub enum SpecViewEvent {
    Finished,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanResponse {
    plan: String,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    previous_plan: Option<String>,
    #[serde(default)]
    version_info: Option<VersionInfo>,
    #[serde(default)]
    gate: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionInfo {
    version: u32,
    total_versions: u32,
}

struct Loaded {
    file_path: String,
    gate: bool,
    version: Option<(u32, u32)>,
    blocks: Vec<SpecBlock>,
    /// Rendered markdown for each block. For a question, its context lines.
    rendered: Vec<Entity<Markdown>>,
    changed: HashSet<usize>,
    previous: Vec<PreviousComment>,
    /// Text answers and notes, by question key.
    answer_inputs: HashMap<String, Entity<Editor>>,
}

enum Status {
    Loading,
    Failed(String),
    Ready,
    /// The session ended: the result went to Claude.
    Done(String),
}

pub struct SpecReviewView {
    session: Session,
    http: Arc<dyn HttpClient>,
    focus_handle: FocusHandle,
    status: Status,
    doc: Option<Loaded>,
    record_path: PathBuf,
    record: SpecRecord,
    /// The block that has the comment input open.
    comment_input: Option<(usize, Entity<Editor>)>,
    _subscriptions: Vec<Subscription>,
    _load: Task<()>,
}

impl SpecReviewView {
    pub fn new(
        session: Session,
        http: Arc<dyn HttpClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let target = session.target.clone().unwrap_or_default();
        let record_path = spec_store::record_path(&spec_store::drafts_dir(), &target);
        let load = cx.spawn_in(window, {
            let http = http.clone();
            let url = format!("{}/api/plan", session.url);
            async move |this, cx| {
                let result = fetch_plan(&http, &url).await;
                this.update_in(cx, |this, window, cx| match result {
                    Ok(plan) => this.loaded(plan, window, cx),
                    Err(error) => {
                        this.status = Status::Failed(format!("{error:#}"));
                        cx.notify();
                    }
                })
                .ok();
            }
        });
        Self {
            session,
            http,
            focus_handle: cx.focus_handle(),
            status: Status::Loading,
            doc: None,
            record: spec_store::load(&record_path),
            record_path,
            comment_input: None,
            _subscriptions: Vec::new(),
            _load: load,
        }
    }

    pub fn session_pid(&self) -> i32 {
        self.session.pid
    }

    pub fn target(&self) -> Option<&str> {
        self.session.target.as_deref()
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.status, Status::Done(_))
    }

    fn file_name(&self) -> String {
        let path = self
            .doc
            .as_ref()
            .map(|doc| doc.file_path.clone())
            .or_else(|| self.session.target.clone())
            .unwrap_or_default();
        Path::new(&path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("spec")
            .to_string()
    }

    fn loaded(&mut self, plan: PlanResponse, window: &mut Window, cx: &mut Context<Self>) {
        let blocks = spec_doc::parse(&plan.plan);
        let changed = spec_doc::changed_blocks(&blocks, plan.previous_plan.as_deref());
        let rendered = blocks
            .iter()
            .map(|block| {
                let source = match &block.question {
                    Some(question) => question.context.join("\n\n"),
                    None => render_mermaid(&block.text),
                };
                cx.new(|cx| Markdown::new(source.into(), None, None, cx))
            })
            .collect();
        self.record.reattach(&blocks);
        let previous = self.record.previous_comments(&blocks);

        let mut answer_inputs = HashMap::default();
        for question in blocks.iter().filter_map(|block| block.question.as_ref()) {
            let key = question.key.clone();
            let saved = self.record.answers.get(&key).cloned().unwrap_or_default();
            let (placeholder, initial) = if question.kind == QuestionKind::Text {
                ("Your answer…", saved.text)
            } else {
                ("Add a note (optional)…", saved.note)
            };
            let input = cx.new(|cx| {
                let mut editor = Editor::auto_height(1, 6, window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor.set_text(initial, window, cx);
                editor
            });
            let is_text = question.kind == QuestionKind::Text;
            self._subscriptions.push(cx.subscribe(&input, move |this, input, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    let value = input.read(cx).text(cx);
                    let answer = this.record.answers.entry(key.clone()).or_default();
                    if is_text {
                        answer.text = value;
                    } else {
                        answer.note = value;
                    }
                    this.save(cx);
                }
            }));
            answer_inputs.insert(question.key.clone(), input);
        }

        self.doc = Some(Loaded {
            file_path: plan.file_path.unwrap_or_else(|| self.session.target.clone().unwrap_or_default()),
            gate: plan.gate,
            version: plan.version_info.map(|info| (info.version, info.total_versions)),
            blocks,
            rendered,
            changed,
            previous,
            answer_inputs,
        });
        self.status = Status::Ready;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = spec_store::save(&self.record_path, &self.record) {
            log::error!("plannotator: cannot save spec drafts: {error:#}");
        }
        cx.notify();
    }

    fn open_comment_input(&mut self, block: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (input, subscriptions) =
            crate::input::single_line("Comment on this part…", window, cx, Self::submit_comment);
        self._subscriptions.extend(subscriptions);
        window.focus(&input.focus_handle(cx), cx);
        self.comment_input = Some((block, input));
        cx.notify();
    }

    fn submit_comment(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((block, input)) = self.comment_input.take() else {
            return;
        };
        let text = input.read(cx).text(cx).trim().to_string();
        let Some(doc) = &self.doc else {
            return;
        };
        if !text.is_empty() {
            self.record.comments.push(SpecComment {
                block,
                original_text: doc.blocks[block].text.clone(),
                text,
                created_at: crate::now_ms(),
            });
        }
        self.save(cx);
    }

    fn delete_comment(&mut self, created_at: u64, cx: &mut Context<Self>) {
        self.record.comments.retain(|comment| comment.created_at != created_at);
        self.save(cx);
    }

    fn toggle_choice(&mut self, question: &Question, label: &str, cx: &mut Context<Self>) {
        let answer = self.record.answers.entry(question.key.clone()).or_default();
        answer.skipped = false;
        let is_selected = answer.selected.iter().any(|selected| selected == label);
        match question.kind {
            QuestionKind::Multi if is_selected => answer.selected.retain(|selected| selected != label),
            QuestionKind::Multi => answer.selected.push(label.to_string()),
            _ if is_selected => answer.selected.clear(),
            _ => answer.selected = vec![label.to_string()],
        }
        self.save(cx);
    }

    fn toggle_skip(&mut self, key: &str, cx: &mut Context<Self>) {
        let answer = self.record.answers.entry(key.to_string()).or_default();
        answer.skipped = !answer.skipped;
        if answer.skipped {
            answer.selected.clear();
        }
        self.save(cx);
    }

    fn answered_count(&self) -> (usize, usize) {
        let Some(doc) = &self.doc else {
            return (0, 0);
        };
        let questions: Vec<&Question> = doc.blocks.iter().filter_map(|block| block.question.as_ref()).collect();
        let answered = questions
            .iter()
            .filter(|question| {
                self.record.answers.get(&question.key).is_some_and(|answer| {
                    !answer.selected.is_empty() || !answer.text.trim().is_empty() || answer.skipped
                })
            })
            .count();
        (answered, questions.len())
    }

    fn send_feedback(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        let feedback = spec_doc::export_feedback(&doc.blocks, &self.record.comments, &self.record.answers);
        let body = serde_json::json!({
            "feedback": feedback,
            "annotations": annotations(&doc.blocks, &self.record.comments, &self.record.answers),
            "codeAnnotations": [],
        });
        let url = format!("{}/api/feedback", self.session.url);
        self.submit(url, body, "Sent to Claude", false, cx);
    }

    fn approve(&mut self, cx: &mut Context<Self>) {
        let Some(doc) = &self.doc else {
            return;
        };
        // Without --gate the session has no approve route. Plain feedback then
        // tells Claude the same thing.
        let (url, body) = if doc.gate {
            (
                format!("{}/api/approve", self.session.url),
                serde_json::json!({ "feedback": "", "annotations": [], "codeAnnotations": [] }),
            )
        } else {
            (
                format!("{}/api/feedback", self.session.url),
                serde_json::json!({
                    "feedback": "Approved. I reviewed the document and have no changes.",
                    "annotations": [],
                    "codeAnnotations": [],
                }),
            )
        };
        self.submit(url, body, "Approved", true, cx);
    }

    fn submit(
        &mut self,
        url: String,
        body: serde_json::Value,
        done: &'static str,
        approved: bool,
        cx: &mut Context<Self>,
    ) {
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let result = send_json(&http, Method::POST, &url, &body).await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
                    if approved {
                        this.record.approved_at = Some(now.clone());
                    } else {
                        this.record.approved_at = None;
                    }
                    this.record.last_round = std::mem::take(&mut this.record.comments);
                    this.record.answers.clear();
                    this.save(cx);
                    this.status = Status::Done(format!("{done} at {now}."));
                    cx.emit(SpecViewEvent::Finished);
                    cx.notify();
                }
                Err(error) => {
                    log::error!("plannotator: spec submit failed: {error:#}");
                    this.status = Status::Failed(format!("Send failed: {error:#}. Your drafts are kept."));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let path = self.doc.as_ref().map(|doc| doc.file_path.clone()).unwrap_or_default();
        let version = self.doc.as_ref().and_then(|doc| doc.version);
        let approved = self.record.approved_at.clone();
        v_flex()
            .w_full()
            .px_4()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::FileMarkdown).color(Color::Muted))
                    .child(Label::new(self.file_name()).weight(FontWeight::MEDIUM))
                    .child(div().flex_1().min_w_0().child(Label::new(path).size(LabelSize::Small).color(Color::Muted).truncate()))
                    .when_some(version.filter(|(_, total)| *total > 1), |row, (version, total)| {
                        row.child(Label::new(format!("Round {version} of {total}")).size(LabelSize::Small).color(Color::Muted))
                    })
                    .when_some(approved, |row, approved| {
                        row.child(Label::new(format!("Approved {approved}")).size(LabelSize::Small).color(Color::Success))
                    }),
            )
            .when_some(self.doc.as_ref().filter(|doc| !doc.changed.is_empty()), |header, doc| {
                header.child(
                    Label::new(format!("{} parts changed since the last round. They have a blue bar.", doc.changed.len()))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
    }

    fn render_previous_round(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let doc = self.doc.as_ref()?;
        if doc.previous.is_empty() {
            return None;
        }
        let colors = cx.theme().colors().clone();
        let open = doc.previous.iter().filter(|previous| previous.open).count();
        let resolved = doc.previous.len() - open;
        Some(
            v_flex()
                .w_full()
                .p_3()
                .gap_1()
                .rounded_md()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.surface_background)
                .child(
                    Label::new(format!(
                        "Last round: {} comments · {resolved} resolved · {open} still open",
                        doc.previous.len()
                    ))
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM),
                )
                .children(doc.previous.iter().map(|previous| {
                    h_flex()
                        .gap_1p5()
                        .items_start()
                        .child(
                            Icon::new(if previous.open { IconName::Circle } else { IconName::Check })
                                .size(IconSize::XSmall)
                                .color(if previous.open { Color::Warning } else { Color::Success }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .text_color(colors.text_muted)
                                .child(format!(
                                    "{}: \"{}\" ({})",
                                    if previous.open { "Open" } else { "Resolved" },
                                    previous.comment.text,
                                    spec_doc::excerpt(&previous.comment.original_text)
                                )),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_block(&self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(doc) = &self.doc else {
            return div().into_any_element();
        };
        let block = &doc.blocks[ix];
        let colors = cx.theme().colors().clone();
        let accent = colors.text_accent;
        let changed = doc.changed.contains(&ix);
        let finished = self.is_finished();
        let comments: Vec<SpecComment> = self
            .record
            .comments
            .iter()
            .filter(|comment| comment.block == ix)
            .cloned()
            .collect();
        let input = self
            .comment_input
            .as_ref()
            .filter(|(block, _)| *block == ix)
            .map(|(_, input)| input.clone());

        let content = match &block.question {
            Some(question) => self.render_question(ix, question, window, cx),
            None => MarkdownElement::new(
                doc.rendered[ix].clone(),
                MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
            )
            .into_any_element(),
        };

        h_flex()
            .id(("spec-block", ix))
            .group("spec-block")
            .w_full()
            .items_start()
            .gap_2()
            .py_1()
            .child(
                div()
                    .w(px(36.))
                    .flex_none()
                    .pt_1()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(block.start_line.to_string()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .pl_3()
                    .border_l_2()
                    .border_color(if changed { accent } else { gpui::transparent_black() })
                    .when(changed, |content| content.bg(accent.opacity(0.05)))
                    .child(content)
                    .children(comments.into_iter().map(|comment| {
                        let created_at = comment.created_at;
                        h_flex()
                            .group("spec-comment")
                            .mt_1()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .items_start()
                            .rounded_sm()
                            .border_l_2()
                            .border_color(colors.border_focused)
                            .bg(colors.surface_background)
                            .child(Label::new("You").size(LabelSize::Small).weight(FontWeight::MEDIUM).color(Color::Muted))
                            .child(div().flex_1().min_w_0().text_sm().text_color(colors.text).child(comment.text))
                            .when(!finished, |row| {
                                row.child(
                                    div().invisible().group_hover("spec-comment", |style| style.visible()).child(
                                        IconButton::new(("delete-spec-comment", created_at as usize), IconName::Trash)
                                            .icon_size(IconSize::XSmall)
                                            .icon_color(Color::Muted)
                                            .tooltip(Tooltip::text("Delete comment"))
                                            .on_click(cx.listener(move |this, _, _, cx| this.delete_comment(created_at, cx))),
                                    ),
                                )
                            })
                    }))
                    .when_some(input, |content, input| {
                        content.child(
                            h_flex()
                                .mt_1()
                                .px_2()
                                .py_1()
                                .gap_2()
                                .rounded_sm()
                                .border_1()
                                .border_color(colors.border_focused)
                                .bg(colors.editor_background)
                                .child(div().flex_1().min_w_0().child(input))
                                .child(
                                    Button::new(("add-spec-comment", ix), "Comment")
                                        .style(ButtonStyle::Tinted(TintColor::Accent))
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|this, _, window, cx| this.submit_comment(window, cx))),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(28.))
                    .flex_none()
                    .when(!finished, |slot| {
                        slot.child(
                            div().invisible().group_hover("spec-block", |style| style.visible()).child(
                                IconButton::new(("comment-block", ix), IconName::Chat)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Comment on this part"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_comment_input(ix, window, cx)
                                    })),
                            ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_question(
        &self,
        ix: usize,
        question: &Question,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(doc) = &self.doc else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors().clone();
        let accent = colors.text_accent;
        let answer = self.record.answers.get(&question.key).cloned().unwrap_or_default();
        let finished = self.is_finished();
        let kind_label = match question.kind {
            QuestionKind::Single => "Pick one",
            QuestionKind::Multi => "Pick any",
            QuestionKind::Text => "Your answer",
        };
        let choices = question.choices.iter().enumerate().map(|(choice_ix, choice)| {
            let selected = answer.selected.iter().any(|label| label == &choice.label)
                || (answer.selected.is_empty() && !answer.skipped && choice.settled);
            let recommended = question.recommended.contains(&choice.label);
            let label = choice.label.clone();
            let kind = question.kind;
            let question = question.clone();
            h_flex()
                .id(("choice", ix * 1000 + choice_ix))
                .w_full()
                .px_2()
                .py_1p5()
                .gap_2()
                .items_start()
                .rounded_sm()
                .border_1()
                .border_color(if selected { colors.border_focused } else { colors.border_variant })
                .when(selected, |row| row.bg(accent.opacity(0.08)))
                .when(!finished, |row| {
                    row.cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_choice(&question, &label, cx)))
                })
                .child(choice_icon(kind, selected))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(Label::new(choice.label.clone()).weight(FontWeight::MEDIUM))
                                .when(recommended, |row| {
                                    row.child(Label::new("Recommended").size(LabelSize::XSmall).color(Color::Accent))
                                })
                                .when(choice.settled, |row| {
                                    row.child(Label::new("Settled").size(LabelSize::XSmall).color(Color::Muted))
                                }),
                        )
                        .when(!choice.description.is_empty(), |column| {
                            column.child(Label::new(choice.description.clone()).size(LabelSize::Small).color(Color::Muted))
                        }),
                )
        });
        let key = question.key.clone();
        v_flex()
            .w_full()
            .my_1()
            .p_3()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.surface_background)
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Question").size(LabelSize::XSmall).color(Color::Accent))
                    .child(Label::new(kind_label).size(LabelSize::XSmall).color(Color::Muted))
                    .child(div().flex_1())
                    .when(!finished, |row| {
                        row.child(
                            Button::new(("skip", ix), if answer.skipped { "Skipped" } else { "Skip" })
                                .label_size(LabelSize::Small)
                                .toggle_state(answer.skipped)
                                .on_click(cx.listener(move |this, _, _, cx| this.toggle_skip(&key, cx))),
                        )
                    }),
            )
            .child(Label::new(question.prompt.clone()).weight(FontWeight::SEMIBOLD))
            .when(!question.context.is_empty(), |card| {
                card.child(
                    div().text_color(colors.text_muted).child(MarkdownElement::new(
                        doc.rendered[ix].clone(),
                        MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
                    )),
                )
            })
            .children(choices)
            .when_some(question.suggested.clone(), |card, suggested| {
                card.child(Label::new(format!("Suggested: {suggested}")).size(LabelSize::Small).color(Color::Accent))
            })
            .when_some(doc.answer_inputs.get(&question.key).cloned(), |card, input| {
                card.child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(colors.border_variant)
                        .bg(colors.editor_background)
                        .child(input),
                )
            })
            .into_any_element()
    }

    fn render_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let (answered, questions) = self.answered_count();
        let comments = self.record.comments.len();
        let ready = matches!(self.status, Status::Ready);
        let mut summary = format!("{comments} comment{}", if comments == 1 { "" } else { "s" });
        if questions > 0 {
            summary.push_str(&format!(" · {answered} of {questions} questions answered"));
        }
        let has_feedback = comments > 0 || answered > 0;
        h_flex()
            .w_full()
            .px_4()
            .py_2()
            .gap_2()
            .border_t_1()
            .border_color(colors.border_variant)
            .child(div().flex_1().child(Label::new(summary).size(LabelSize::Small).color(Color::Muted)))
            .child(
                Button::new("spec-approve", "Approve")
                    .style(ButtonStyle::Filled)
                    .disabled(!ready)
                    .tooltip(Tooltip::text("Tell Claude the spec is approved"))
                    .on_click(cx.listener(|this, _, _, cx| this.approve(cx))),
            )
            .child(
                Button::new("spec-send", "Send feedback")
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .disabled(!ready || !has_feedback)
                    .on_click(cx.listener(|this, _, _, cx| this.send_feedback(cx))),
            )
    }
}

/// A round mark for pick one, a square for pick any.
fn choice_icon(kind: QuestionKind, selected: bool) -> AnyElement {
    let icon = match (kind, selected) {
        (_, true) => IconName::Check,
        (QuestionKind::Multi, false) => IconName::SquarePlus,
        _ => IconName::Circle,
    };
    Icon::new(icon)
        .size(IconSize::Small)
        .color(if selected { Color::Accent } else { Color::Muted })
        .into_any_element()
}

async fn fetch_plan(http: &Arc<dyn HttpClient>, url: &str) -> Result<PlanResponse> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .body(AsyncBody::empty())?;
    let mut response = http.send(request).await.context("Plannotator did not answer")?;
    anyhow::ensure!(response.status().is_success(), "Plannotator answered {}", response.status());
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    Ok(serde_json::from_str(&body)?)
}

/// The annotation records the browser UI would send. The server stores them
/// and Claude reads only `feedback`, so they matter for Plannotator's history.
fn annotations(
    blocks: &[SpecBlock],
    comments: &[SpecComment],
    answers: &HashMap<String, Answer>,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = comments
        .iter()
        .enumerate()
        .map(|(ix, comment)| {
            serde_json::json!({
                "id": format!("zed-{}-{ix}", comment.created_at),
                "blockId": format!("block-{}", comment.block),
                "startOffset": 0,
                "endOffset": comment.original_text.len(),
                "type": "COMMENT",
                "text": comment.text,
                "originalText": comment.original_text,
                "createdA": comment.created_at,
                "source": "zed",
            })
        })
        .collect();
    for (ix, block) in blocks.iter().enumerate() {
        let Some(question) = &block.question else {
            continue;
        };
        let Some(answer) = answers.get(&question.key).filter(|answer| !answer.is_empty()) else {
            continue;
        };
        let kind = match question.kind {
            QuestionKind::Single => "single",
            QuestionKind::Multi => "multi",
            QuestionKind::Text => "text",
        };
        out.push(serde_json::json!({
            "id": format!("ann-question-{}", question.key),
            "blockId": format!("block-{ix}"),
            "startOffset": 0,
            "endOffset": 0,
            "type": "COMMENT",
            "text": format!("Answer: {}", if answer.text.is_empty() { answer.selected.join(", ") } else { answer.text.clone() }),
            "originalText": question.prompt,
            "createdA": crate::now_ms(),
            "questionAnswer": {
                "v": 1,
                "key": question.key,
                "kind": kind,
                "prompt": question.prompt,
                "selected": answer.selected,
                "text": answer.text,
                "note": answer.note,
                "skipped": answer.skipped,
                "sourceLine": block.start_line,
            },
        }));
    }
    out
}

impl EventEmitter<SpecViewEvent> for SpecReviewView {}

impl Focusable for SpecReviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for SpecReviewView {
    type Event = SpecViewEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FileMarkdown).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Spec: {}", self.file_name()).into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        self.session.target.clone().map(Into::into)
    }

    fn to_item_events(_event: &SpecViewEvent, _f: &mut dyn FnMut(ItemEvent)) {}
}

impl Render for SpecReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let body: AnyElement = match &self.status {
            Status::Loading => Label::new("Loading the spec…").color(Color::Muted).into_any_element(),
            Status::Failed(error) if self.doc.is_none() => Label::new(error.clone()).color(Color::Error).into_any_element(),
            _ => {
                let count = self.doc.as_ref().map_or(0, |doc| doc.blocks.len());
                let blocks: Vec<AnyElement> = (0..count).map(|ix| self.render_block(ix, window, cx)).collect();
                v_flex()
                    .w_full()
                    .gap_1()
                    .children(self.render_previous_round(cx))
                    .children(blocks)
                    .into_any_element()
            }
        };
        let banner = match &self.status {
            Status::Done(message) => Some((message.clone(), Color::Success)),
            Status::Failed(error) if self.doc.is_some() => Some((error.clone(), Color::Error)),
            _ => None,
        };
        v_flex()
            .key_context("PlannotatorSpecReview")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(self.render_header(cx))
            .when_some(banner, |view, (message, color)| {
                view.child(
                    div()
                        .px_4()
                        .py_1p5()
                        .border_b_1()
                        .border_color(colors.border_variant)
                        .child(Label::new(message).size(LabelSize::Small).color(color)),
                )
            })
            .child(
                div()
                    .id("spec-review-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(div().max_w(px(900.)).px_4().py_3().child(body)),
            )
            .child(self.render_footer(cx))
    }
}
