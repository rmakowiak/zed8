//! The spec review tab: a markdown spec from a Plannotator annotate session,
//! with comments on blocks, answers to `:::question` blocks, the changes
//! since the last round, and Approve or Send feedback.
//!
//! The question cards follow Plannotator's own (`QuestionBlock.tsx`): an
//! eyebrow with a status pill, radio or check rows, an "Other" row, a note
//! on request, and a footer with Add note, Skip and Accept recommended.

use crate::{
    rich_text::render_mermaid,
    session::Session,
    spec_doc::{self, Answer, Question, QuestionKind, SpecBlock, SpecComment},
    spec_store::{self, PreviousComment, SpecRecord},
};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    IntoElement, Render, SharedString, Subscription, Task, Window, px,
};
use http_client::{HttpClient, Method};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use theme::ThemeColors;
use ui::{SpinnerLabel, TintColor, Tooltip, prelude::*};
use workspace::{Item, item::ItemEvent};

pub enum SpecViewEvent {
    /// The result went to Claude. `approved` is false for feedback.
    Finished { approved: bool },
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

/// Which part of an answer an input edits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Field {
    Text,
    Other,
    Note,
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
    /// For each question block: its number and the question count.
    numbers: HashMap<usize, (usize, usize)>,
    inputs: HashMap<(String, Field), Entity<Editor>>,
}

enum Status {
    Loading,
    Failed(String),
    Ready,
    /// The session ended: the result went to Claude at `at`.
    Done { approved: bool, at: String },
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
    /// Questions whose note field is open.
    notes_open: HashSet<String>,
    /// What went to Claude, shown while Claude works on the next round.
    sent: Option<(Vec<SpecComment>, HashMap<String, Answer>)>,
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
        let record_path = spec_store::record_path(&crate::paths::drafts_dir(), &target);
        let load = cx.spawn_in(window, {
            let http = http.clone();
            let url = format!("{}/api/plan", session.url);
            async move |this, cx| {
                let result = crate::client::get_json::<PlanResponse>(&http, &url).await;
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
            notes_open: HashSet::default(),
            sent: None,
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

    fn is_finished(&self) -> bool {
        matches!(self.status, Status::Done { .. })
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

        let question_blocks: Vec<usize> = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.question.is_some())
            .map(|(ix, _)| ix)
            .collect();
        let numbers = question_blocks
            .iter()
            .enumerate()
            .map(|(number, ix)| (*ix, (number + 1, question_blocks.len())))
            .collect();

        let mut inputs = HashMap::default();
        for question in blocks.iter().filter_map(|block| block.question.as_ref()) {
            let saved = self.record.answers.get(&question.key).cloned().unwrap_or_default();
            let mut fields = vec![(Field::Note, saved.note, "Context for the agent…", 2)];
            if question.kind == QuestionKind::Text {
                fields.push((Field::Text, saved.text, "Your answer…", 2));
            } else {
                fields.push((Field::Other, saved.other, "Other…", 1));
            }
            for (field, initial, placeholder, min_lines) in fields {
                if field == Field::Note && !initial.trim().is_empty() {
                    self.notes_open.insert(question.key.clone());
                }
                let input = cx.new(|cx| {
                    let mut editor = Editor::auto_height(min_lines, 8, window, cx);
                    editor.set_placeholder_text(placeholder, window, cx);
                    editor.set_text(initial, window, cx);
                    editor
                });
                let key = question.key.clone();
                self._subscriptions
                    .push(cx.subscribe(&input, move |this, input, event: &EditorEvent, cx| {
                        if matches!(event, EditorEvent::BufferEdited) {
                            let value = input.read(cx).text(cx);
                            this.set_field(&key, field, value, cx);
                        }
                    }));
                inputs.insert((question.key.clone(), field), input);
            }
        }

        self.doc = Some(Loaded {
            file_path: plan
                .file_path
                .unwrap_or_else(|| self.session.target.clone().unwrap_or_default()),
            gate: plan.gate,
            version: plan.version_info.map(|info| (info.version, info.total_versions)),
            blocks,
            rendered,
            changed,
            previous,
            numbers,
            inputs,
        });
        self.status = Status::Ready;
        cx.notify();
    }

    fn set_field(&mut self, key: &str, field: Field, value: String, cx: &mut Context<Self>) {
        let answer = self.record.answers.entry(key.to_string()).or_default();
        match field {
            Field::Text => answer.text = value,
            Field::Other => answer.other = value,
            Field::Note => answer.note = value,
        }
        if field != Field::Note && answer.is_answered() {
            answer.skipped = false;
        }
        self.save(cx);
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

    fn accept_recommended(&mut self, question: &Question, window: &mut Window, cx: &mut Context<Self>) {
        if question.recommended.is_empty() {
            // A suggestion that names no choice goes into the answer field.
            let Some(suggested) = question.suggested.clone() else {
                return;
            };
            let field = if question.kind == QuestionKind::Text { Field::Text } else { Field::Other };
            if let Some(input) = self.input(&question.key, field) {
                input.update(cx, |input, cx| input.set_text(suggested, window, cx));
            }
            return;
        }
        let answer = self.record.answers.entry(question.key.clone()).or_default();
        answer.skipped = false;
        answer.selected = question.recommended.clone();
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

    fn toggle_note(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.notes_open.remove(key) {
            self.notes_open.insert(key.to_string());
            if let Some(input) = self.input(key, Field::Note) {
                window.focus(&input.focus_handle(cx), cx);
            }
        }
        cx.notify();
    }

    fn input(&self, key: &str, field: Field) -> Option<Entity<Editor>> {
        self.doc
            .as_ref()?
            .inputs
            .get(&(key.to_string(), field))
            .cloned()
    }

    fn answer(&self, key: &str) -> Answer {
        let answers = match &self.sent {
            Some((_, answers)) => answers,
            None => &self.record.answers,
        };
        answers.get(key).cloned().unwrap_or_default()
    }

    fn comments(&self) -> &[SpecComment] {
        match &self.sent {
            Some((comments, _)) => comments,
            None => &self.record.comments,
        }
    }

    fn answered_count(&self) -> (usize, usize) {
        let Some(doc) = &self.doc else {
            return (0, 0);
        };
        let questions: Vec<&Question> = doc
            .blocks
            .iter()
            .filter_map(|block| block.question.as_ref())
            .collect();
        let answered = questions
            .iter()
            .filter(|question| {
                let answer = self.answer(&question.key);
                answer.is_answered() || answer.skipped
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
        self.submit(url, body, false, cx);
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
        self.submit(url, body, true, cx);
    }

    fn submit(&mut self, url: String, body: serde_json::Value, approved: bool, cx: &mut Context<Self>) {
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let result = crate::client::send_json(&http, Method::POST, &url, &body).await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    let now = chrono::Local::now();
                    this.record.approved_at = approved.then(|| now.format("%Y-%m-%d %H:%M").to_string());
                    this.sent = Some((this.record.comments.clone(), this.record.answers.clone()));
                    this.record.last_round = std::mem::take(&mut this.record.comments);
                    this.record.answers.clear();
                    this.save(cx);
                    this.status = Status::Done {
                        approved,
                        at: now.format("%H:%M").to_string(),
                    };
                    cx.emit(SpecViewEvent::Finished { approved });
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
        let changed = self.doc.as_ref().map_or(0, |doc| doc.changed.len());
        h_flex()
            .w_full()
            .px_4()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .bg(colors.title_bar_background)
            .child(Icon::new(IconName::FileMarkdown).color(Color::Accent))
            .child(Label::new(self.file_name()).weight(FontWeight::SEMIBOLD))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(path).size(LabelSize::Small).color(Color::Muted).truncate_start()),
            )
            .when(changed > 0, |row| {
                row.child(pill(format!("{changed} changed"), Tone::Warning, &colors))
            })
            .when_some(version.filter(|(_, total)| *total > 1), |row, (version, total)| {
                row.child(pill(format!("Round {version} of {total}"), Tone::Accent, &colors))
            })

    }

    /// After a send: what happens now.
    fn render_banner(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let colors = cx.theme().colors().clone();
        let status = cx.theme().status().clone();
        let (content, background): (AnyElement, Hsla) = match &self.status {
            Status::Done { approved: false, at } => (
                h_flex()
                    .gap_2()
                    .child(SpinnerLabel::new().color(Color::Accent))
                    .child(Label::new(format!(
                        "Sent at {at}. Claude is working on your feedback. The next round opens here and replaces this tab."
                    )).size(LabelSize::Small))
                    .into_any_element(),
                colors.text_accent.opacity(0.1),
            ),
            Status::Done { approved: true, at } => (
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Check).size(IconSize::Small).color(Color::Success))
                    .child(Label::new(format!("Approved at {at}. Claude has the approval.")).size(LabelSize::Small))
                    .into_any_element(),
                status.success_background.opacity(0.4),
            ),
            Status::Failed(error) if self.doc.is_some() => (
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::Warning).size(IconSize::Small).color(Color::Error))
                    .child(Label::new(error.clone()).size(LabelSize::Small).color(Color::Error))
                    .into_any_element(),
                status.error_background.opacity(0.4),
            ),
            _ => return None,
        };
        Some(
            div()
                .w_full()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(colors.border_variant)
                .bg(background)
                .child(content)
                .into_any_element(),
        )
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
                .mb_3()
                .px_3()
                .py_2p5()
                .gap_1p5()
                .rounded(px(10.))
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.surface_background)
                .child(
                    h_flex()
                        .gap_2()
                        .child(eyebrow("Last round"))
                        .child(div().flex_1())
                        .child(pill(format!("{resolved} resolved"), Tone::Success, &colors))
                        .when(open > 0, |row| row.child(pill(format!("{open} still open"), Tone::Warning, &colors))),
                )
                .children(doc.previous.iter().map(|previous| {
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(
                            div().pt(px(3.)).child(
                                Icon::new(if previous.open { IconName::Circle } else { IconName::Check })
                                    .size(IconSize::XSmall)
                                    .color(if previous.open { Color::Warning } else { Color::Success }),
                            ),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().text_color(colors.text).child(previous.comment.text.clone()))
                                .child(
                                    Label::new(spec_doc::excerpt(&previous.comment.original_text))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
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
            .comments()
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
            .child(
                div()
                    .w(px(32.))
                    .flex_none()
                    .pt(px(6.))
                    .text_xs()
                    .text_color(colors.text_muted.opacity(0.7))
                    .child(block.start_line.to_string()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .py_1()
                    .rounded_r_md()
                    .border_l_2()
                    .border_color(if changed { accent } else { gpui::transparent_black() })
                    .when(changed, |content| content.bg(accent.opacity(0.06)))
                    .when(!finished && block.question.is_none(), |content| {
                        content.hover(|style| style.bg(colors.ghost_element_hover.opacity(0.5)))
                    })
                    .child(content)
                    .children(comments.into_iter().map(|comment| self.render_comment(comment, &colors, cx)))
                    .when_some(input, |content, input| {
                        content.child(
                            h_flex()
                                .mt_2()
                                .gap_2()
                                .child(div().flex_1().min_w_0().child(field_box(input, true, &colors)))
                                .child(
                                    Button::new(("add-spec-comment", ix), "Comment")
                                        .style(ButtonStyle::Filled)
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
                    .pt_1()
                    .when(!finished, |slot| {
                        slot.child(
                            div().invisible().group_hover("spec-block", |style| style.visible()).child(
                                IconButton::new(("comment-block", ix), IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Accent)
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
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

    fn render_comment(&self, comment: SpecComment, colors: &ThemeColors, cx: &mut Context<Self>) -> AnyElement {
        let created_at = comment.created_at;
        let finished = self.is_finished();
        h_flex()
            .group("spec-comment")
            .mt_2()
            .px_2p5()
            .py_1p5()
            .gap_2()
            .items_start()
            .rounded_md()
            .border_l_2()
            .border_color(colors.text_accent)
            .bg(colors.text_accent.opacity(0.08))
            .child(Label::new("You").size(LabelSize::Small).weight(FontWeight::SEMIBOLD).color(Color::Accent))
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
        let answer = self.answer(&question.key);
        let finished = self.is_finished();
        let answered = answer.is_answered();
        let (number, count) = doc.numbers.get(&ix).copied().unwrap_or((1, 1));
        let status = if answer.skipped {
            pill("Skipped", Tone::Muted, &colors)
        } else if answered || (question.is_settled() && answer.selected.is_empty()) {
            pill("Answered", Tone::Success, &colors)
        } else {
            pill("Open", Tone::Dashed, &colors)
        };
        let mut context_style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx).with_muted_text(cx);
        context_style.container_style.text.color = Some(colors.text_muted);

        let choices: Vec<AnyElement> = question
            .choices
            .iter()
            .enumerate()
            .map(|(choice_ix, choice)| {
                let selected = answer.selected.iter().any(|label| label == &choice.label)
                    || (answer.selected.is_empty() && !answer.skipped && choice.settled);
                let recommended = question.recommended.contains(&choice.label);
                let label = choice.label.clone();
                let kind = question.kind;
                let question = question.clone();
                h_flex()
                    .id(("choice", ix * 1000 + choice_ix))
                    .w_full()
                    .px(px(9.))
                    .py_1p5()
                    .gap_2p5()
                    .items_start()
                    .rounded(px(7.))
                    .when(selected, |row| row.bg(accent.opacity(0.12)))
                    .when(!finished, |row| {
                        row.cursor_pointer()
                            .when(!selected, |row| row.hover(|style| style.bg(colors.ghost_element_hover)))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_choice(&question, &label, cx)))
                    })
                    .child(div().pt(px(3.)).child(control(kind, selected, false, &colors)))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .flex_wrap()
                            .gap_x_1()
                            .child(Label::new(choice.label.clone()).weight(FontWeight::SEMIBOLD))
                            .when(!choice.description.is_empty(), |text| {
                                text.child(Label::new(format!("— {}", choice.description)).color(Color::Muted))
                            })
                            .when(recommended, |text| text.child(pill("Recommended", Tone::Accent, &colors)))
                            .when(choice.settled, |text| text.child(pill("Settled", Tone::Muted, &colors))),
                    )
                    .into_any_element()
            })
            .collect();

        let other_row = (question.kind != QuestionKind::Text)
            .then(|| self.input(&question.key, Field::Other))
            .flatten()
            .map(|input| {
                let filled = !answer.other.trim().is_empty();
                h_flex()
                    .w_full()
                    .px(px(9.))
                    .py(px(5.))
                    .gap_2p5()
                    .rounded(px(7.))
                    .when(filled, |row| row.bg(accent.opacity(0.12)))
                    .child(control(question.kind, filled, true, &colors))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .border_b_1()
                            .border_color(if input.focus_handle(cx).is_focused(window) {
                                accent
                            } else {
                                gpui::transparent_black()
                            })
                            .child(input),
                    )
            });

        let text_answer = (question.kind == QuestionKind::Text)
            .then(|| self.input(&question.key, Field::Text))
            .flatten()
            .map(|input| {
                let focused = input.focus_handle(cx).is_focused(window);
                div().mt_2().child(field_box(input, focused, &colors))
            });

        let note = (self.notes_open.contains(&question.key))
            .then(|| self.input(&question.key, Field::Note))
            .flatten()
            .map(|input| {
                let focused = input.focus_handle(cx).is_focused(window);
                v_flex()
                    .mt_2()
                    .gap_1()
                    .child(eyebrow("Note"))
                    .child(field_box(input, focused, &colors))
            });

        let can_accept = !finished
            && (question.recommended.is_empty() && question.suggested.is_some()
                || !question.recommended.is_empty() && {
                    let mut selected = answer.selected.clone();
                    let mut recommended = question.recommended.clone();
                    selected.sort();
                    recommended.sort();
                    selected != recommended
                });
        let key = question.key.clone();
        let note_key = question.key.clone();
        let accept_question = question.clone();
        let note_open = self.notes_open.contains(&question.key);

        v_flex()
            .w_full()
            .my_2()
            .px(px(14.))
            .pt_3()
            .pb_2p5()
            .rounded(px(10.))
            .border_1()
            .border_color(if answered { accent.opacity(0.45) } else { colors.border })
            .bg(colors.surface_background)
            .child(
                h_flex()
                    .mb_1p5()
                    .gap_2()
                    .child(eyebrow(&match question.kind {
                        QuestionKind::Single => format!("Question {number} of {count} · pick one"),
                        QuestionKind::Multi => format!("Question {number} of {count} · pick any"),
                        QuestionKind::Text => format!("Question {number} of {count}"),
                    }))
                    .child(div().flex_1())
                    .child(status),
            )
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child(question.prompt.clone()),
            )
            .when(!question.context.is_empty(), |card| {
                card.child(div().mt_1().child(MarkdownElement::new(doc.rendered[ix].clone(), context_style)))
            })
            .when(!choices.is_empty() || other_row.is_some(), |card| {
                card.child(
                    v_flex()
                        .mt_2p5()
                        .gap_0p5()
                        .when(answer.skipped, |list| list.opacity(0.55))
                        .children(choices)
                        .children(other_row),
                )
            })
            .when_some(question.suggested.clone(), |card, suggested| {
                card.child(
                    h_flex()
                        .mt_2()
                        .gap_1()
                        .child(Label::new("Suggested:").size(LabelSize::Small).color(Color::Muted))
                        .child(Label::new(suggested).size(LabelSize::Small).color(Color::Accent)),
                )
            })
            .children(text_answer)
            .children(note)
            .when(!finished, |card| {
                card.child(
                    h_flex()
                        .mt_2()
                        .pt_1p5()
                        .gap_1()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .child(
                            Button::new(("note", ix), if note_open { "Hide note" } else { "Add note" })
                                .label_size(LabelSize::Small)
                                .color(Color::Muted)
                                .on_click(cx.listener(move |this, _, window, cx| this.toggle_note(&note_key, window, cx))),
                        )
                        .child(
                            Button::new(("skip", ix), if answer.skipped { "Unskip" } else { "Skip" })
                                .label_size(LabelSize::Small)
                                .color(Color::Muted)
                                .on_click(cx.listener(move |this, _, _, cx| this.toggle_skip(&key, cx))),
                        )
                        .child(div().flex_1())
                        .when(can_accept, |footer| {
                            footer.child(
                                Button::new(("accept", ix), "Accept recommended")
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.accept_recommended(&accept_question, window, cx)
                                    })),
                            )
                        }),
                )
            })
            .into_any_element()
    }

    fn render_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let (answered, questions) = self.answered_count();
        let comments = self.comments().len();
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
            .bg(colors.title_bar_background)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(summary).size(LabelSize::Small).color(Color::Muted).truncate()),
            )
            .child(
                Button::new("spec-approve", "Approve")
                    .style(ButtonStyle::Tinted(TintColor::Success))
                    .start_icon(Icon::new(IconName::Check).size(IconSize::Small))
                    .disabled(!ready)
                    .tooltip(Tooltip::text("Tell Claude the spec is approved"))
                    .on_click(cx.listener(|this, _, _, cx| this.approve(cx))),
            )
            .child(
                Button::new("spec-send", "Send feedback")
                    .style(ButtonStyle::Filled)
                    .start_icon(Icon::new(IconName::Send).size(IconSize::Small))
                    .disabled(!ready || !has_feedback)
                    .on_click(cx.listener(|this, _, _, cx| this.send_feedback(cx))),
            )
    }
}

#[derive(Clone, Copy)]
enum Tone {
    Accent,
    Success,
    Warning,
    Muted,
    /// An open item: a dashed outline, no fill.
    Dashed,
}

/// A small rounded tag, like Plannotator's `Tag`.
fn pill(text: impl Into<SharedString>, tone: Tone, colors: &ThemeColors) -> AnyElement {
    let (foreground, background) = match tone {
        Tone::Accent => (colors.text_accent, colors.text_accent.opacity(0.15)),
        Tone::Success => (colors.version_control_added, colors.version_control_added.opacity(0.15)),
        Tone::Warning => (colors.version_control_modified, colors.version_control_modified.opacity(0.15)),
        Tone::Muted | Tone::Dashed => (colors.text_muted, colors.element_background),
    };
    div()
        .flex_none()
        .h(px(18.))
        .px_1p5()
        .flex()
        .items_center()
        .rounded_sm()
        .border_1()
        .when(matches!(tone, Tone::Dashed), |tag| tag.border_dashed().border_color(colors.border))
        .when(!matches!(tone, Tone::Dashed), |tag| {
            tag.border_color(gpui::transparent_black()).bg(background)
        })
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(foreground)
        .child(text.into())
        .into_any_element()
}

/// The small upper-case label over a card or a field.
fn eyebrow(text: &str) -> impl IntoElement {
    Label::new(text.to_uppercase())
        .size(LabelSize::XSmall)
        .weight(FontWeight::SEMIBOLD)
        .color(Color::Muted)
}

/// A radio mark for pick one, a check box for pick any. `other` draws the
/// dashed "+" mark of an empty Other row.
fn control(kind: QuestionKind, selected: bool, other: bool, colors: &ThemeColors) -> AnyElement {
    let accent = colors.text_accent;
    let round = kind != QuestionKind::Multi;
    let mark = div()
        .size(px(15.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .when(round, |mark| mark.rounded_full())
        .when(!round, |mark| mark.rounded(px(3.)));
    if selected && round && !other {
        mark.border_color(accent)
            .child(div().size(px(7.)).rounded_full().bg(accent))
            .into_any_element()
    } else if selected {
        mark.border_color(accent)
            .bg(accent)
            .child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Custom(colors.editor_background)))
            .into_any_element()
    } else if other {
        mark.border_dashed()
            .border_color(colors.border)
            .text_size(px(11.))
            .text_color(colors.text_muted)
            .child("+")
            .into_any_element()
    } else {
        mark.border_color(colors.text_muted.opacity(0.6)).into_any_element()
    }
}

/// A bordered input box. The border takes the accent color while it has focus.
fn field_box(input: Entity<Editor>, focused: bool, colors: &ThemeColors) -> AnyElement {
    div()
        .w_full()
        .px_2p5()
        .py_2()
        .rounded(px(7.))
        .border_1()
        .border_color(if focused { colors.text_accent } else { colors.border })
        .bg(colors.editor_background)
        .child(input)
        .into_any_element()
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
        let mut picks = answer.selected.clone();
        if !answer.other.trim().is_empty() {
            picks.push(format!("Other: {}", answer.other.trim()));
        }
        out.push(serde_json::json!({
            "id": format!("ann-question-{}", question.key),
            "blockId": format!("block-{ix}"),
            "startOffset": 0,
            "endOffset": 0,
            "type": "COMMENT",
            "text": format!("Answer: {}", if answer.text.is_empty() { picks.join(", ") } else { answer.text.clone() }),
            "originalText": question.prompt,
            "createdA": crate::now_ms(),
            "questionAnswer": {
                "v": 1,
                "key": question.key,
                "kind": kind,
                "prompt": question.prompt,
                "selected": answer.selected,
                "other": answer.other,
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
        Some(Icon::new(IconName::FileMarkdown).color(Color::Accent))
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
            Status::Failed(error) if self.doc.is_none() => {
                Label::new(error.clone()).color(Color::Error).into_any_element()
            }
            _ => {
                let count = self.doc.as_ref().map_or(0, |doc| doc.blocks.len());
                let blocks: Vec<AnyElement> =
                    (0..count).map(|ix| self.render_block(ix, window, cx)).collect();
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .when(self.is_finished(), |body| body.opacity(0.6))
                    .children(self.render_previous_round(cx))
                    .children(blocks)
                    .into_any_element()
            }
        };
        v_flex()
            .key_context("PlannotatorSpecReview")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(self.render_header(cx))
            .children(self.render_banner(cx))
            .child(
                div()
                    .id("spec-review-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(div().max_w(px(900.)).px_4().py_4().child(body)),
            )
            .child(self.render_footer(cx))
    }
}
