//! One of Claude's guide notes, shown inline in a chapter tab, with your
//! replies under it as a thread. Replies go to Claude with the review comments.
//!
//! A thread can also hold a finding of the agent review (gate 2). Then you
//! edit or delete the finding instead of replying to it.

use crate::guide::GuideNote;
use editor::{Editor, ExportedReviewComment};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    SharedString, Subscription, Window, px,
};
use ui::{Checkbox, TintColor, ToggleState, Tooltip, prelude::*};

/// The saved state of one thread, in `~/.plannotator/zed-drafts/<guide id>.json`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ThreadDraft {
    pub replies: Vec<String>,
    pub sent: usize,
}

pub enum NoteThreadEvent {
    RepliesChanged,
    /// A finding was edited, deleted or restored. `text` is `None` when it is deleted.
    FindingChanged { id: String, text: Option<String> },
    /// You ticked or unticked the finding for the review draft.
    DraftToggled { add: bool },
}

/// A finding of the agent review, from a Plannotator external annotation.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub id: String,
    pub deleted: bool,
    pub edited: bool,
}

pub struct NoteThread {
    file: String,
    note: GuideNote,
    lines: SharedString,
    text: SharedString,
    body: Entity<Markdown>,
    /// Every reply. The first `sent` are already with Claude.
    replies: Vec<String>,
    sent: usize,
    collapsed: bool,
    input: Entity<Editor>,
    _input_subscriptions: [Subscription; 2],
    finding: Option<Finding>,
    /// The number of the finding in the index of the findings tab.
    index: Option<usize>,
    editing: bool,
    /// The finding is ticked in the review draft.
    in_draft: bool,
}

impl NoteThread {
    pub fn new(
        file: String,
        note: GuideNote,
        draft: ThreadDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (input, input_subscriptions) =
            crate::input::single_line("Reply to Claude…", window, cx, Self::submit);
        let lines = if note.line_start == note.line_end {
            format!("line {}", note.line_start)
        } else {
            format!("lines {}–{}", note.line_start, note.line_end)
        };
        let body = cx.new(|cx| {
            Markdown::new(crate::rich_text::render_mermaid(&note.text).into(), None, None, cx)
        });
        Self {
            file,
            body,
            text: note.text.clone().into(),
            note,
            lines: lines.into(),
            sent: draft.sent.min(draft.replies.len()),
            replies: draft.replies,
            collapsed: false,
            input,
            _input_subscriptions: input_subscriptions,
            finding: None,
            index: None,
            editing: false,
            in_draft: false,
        }
    }

    /// A thread for a finding of the agent review.
    pub fn new_finding(
        file: String,
        note: GuideNote,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut thread = Self::new(file, note, ThreadDraft::default(), window, cx);
        thread.finding = Some(Finding { id, deleted: false, edited: false });
        thread
    }

    pub fn finding(&self) -> Option<&Finding> {
        self.finding.as_ref()
    }

    pub fn set_in_draft(&mut self, in_draft: bool, cx: &mut Context<Self>) {
        if self.in_draft != in_draft {
            self.in_draft = in_draft;
            cx.notify();
        }
    }

    pub fn in_draft(&self) -> bool {
        self.in_draft
    }

    /// The finding as an item of the review draft, with your edits.
    pub fn draft_item(&self) -> Option<crate::review_round::Item> {
        let finding = self.finding.as_ref().filter(|finding| !finding.deleted)?;
        let on_file = !self.file.is_empty();
        Some(crate::review_round::Item {
            key: format!("agent:{}", finding.id),
            source: crate::review_round::Source::Agent,
            path: on_file.then(|| self.file.clone()),
            line: (on_file && self.note.line_start > 0).then_some(self.note.line_start),
            line_end: (on_file && self.note.line_end > self.note.line_start).then_some(self.note.line_end),
            severity: self.note.severity.clone(),
            body: self.note.text.clone(),
            include: true,
            ..Default::default()
        })
    }

    pub fn set_index(&mut self, index: usize, cx: &mut Context<Self>) {
        self.index = Some(index);
        cx.notify();
    }

    pub fn file(&self) -> &str {
        &self.file
    }

    /// The severity of an agent review finding.
    pub fn severity(&self) -> Option<&str> {
        self.finding.as_ref().and(self.note.severity.as_deref())
    }

    pub fn line_start(&self) -> u32 {
        self.note.line_start
    }

    /// The first line of the note, for the index of the findings tab.
    pub fn first_line(&self) -> &str {
        self.note.text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim()
    }

    pub fn expand(&mut self, cx: &mut Context<Self>) {
        if self.collapsed {
            self.collapsed = false;
            cx.notify();
        }
    }

    /// The finding as you want it sent, or `None` when you deleted it.
    pub fn finding_export(&self) -> Option<ExportedReviewComment> {
        let finding = self.finding.as_ref()?;
        (!finding.deleted).then(|| ExportedReviewComment {
            file_path: self.file.clone(),
            line_start: self.note.line_start,
            line_end: self.note.line_end,
            text: self.note.text.clone(),
        })
    }

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = true;
        self.collapsed = false;
        let text = self.note.text.clone();
        self.input.update(cx, |input, cx| input.set_text(text, window, cx));
        window.focus(&self.input.focus_handle(cx), cx);
        cx.notify();
    }

    fn toggle_deleted(&mut self, cx: &mut Context<Self>) {
        let Some(finding) = self.finding.as_mut() else {
            return;
        };
        finding.deleted = !finding.deleted;
        let text = (!finding.deleted).then(|| self.note.text.clone());
        cx.emit(NoteThreadEvent::FindingChanged { id: finding.id.clone(), text });
        cx.notify();
    }

    fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx).trim().to_string();
        self.editing = false;
        self.input.update(cx, |input, cx| input.clear(window, cx));
        let Some(finding) = self.finding.as_mut() else {
            return;
        };
        if text.is_empty() || text == self.note.text {
            cx.notify();
            return;
        }
        finding.edited = true;
        self.note.text = text.clone();
        self.text = text.clone().into();
        self.body = cx.new(|cx| Markdown::new(crate::rich_text::render_mermaid(&text).into(), None, None, cx));
        cx.emit(NoteThreadEvent::FindingChanged { id: finding.id.clone(), text: Some(text) });
        cx.notify();
    }

    /// Identifies the note in the drafts file.
    pub fn key(&self) -> String {
        draft_key(&self.file, &self.note)
    }

    pub fn draft(&self) -> ThreadDraft {
        ThreadDraft {
            replies: self.replies.clone(),
            sent: self.sent,
        }
    }

    pub fn unsent_count(&self) -> usize {
        self.replies.len() - self.sent
    }

    /// The unsent replies as one review comment that quotes the note. Marks them sent.
    pub fn take_unsent(&mut self, cx: &mut Context<Self>) -> Option<ExportedReviewComment> {
        if self.unsent_count() == 0 {
            return None;
        }
        let quote = self
            .note
            .text
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let replies = self.replies[self.sent..].join("\n\n");
        self.sent = self.replies.len();
        cx.emit(NoteThreadEvent::RepliesChanged);
        cx.notify();
        Some(ExportedReviewComment {
            file_path: self.file.clone(),
            line_start: self.note.line_start,
            line_end: self.note.line_end,
            text: format!("Reply to your guide note:\n{quote}\n\n{replies}"),
        })
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.finding.is_some() {
            self.save_edit(window, cx);
            return;
        }
        let text = self.input.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        self.replies.push(text);
        self.input.update(cx, |input, cx| input.clear(window, cx));
        cx.emit(NoteThreadEvent::RepliesChanged);
        cx.notify();
    }

    fn delete_reply(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.sent && ix < self.replies.len() {
            self.replies.remove(ix);
            cx.emit(NoteThreadEvent::RepliesChanged);
            cx.notify();
        }
    }

    fn render_reply(&self, ix: usize, reply: &str, cx: &mut Context<Self>) -> AnyElement {
        let sent = ix < self.sent;
        h_flex()
            .group("reply")
            .w_full()
            .items_start()
            .gap_2()
            .pt_1p5()
            .child(
                Label::new("You")
                    .size(LabelSize::Small)
                    .weight(FontWeight::MEDIUM)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().colors().text)
                    .child(reply.to_string()),
            )
            .child(if sent {
                Label::new("sent")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                div()
                    .invisible()
                    .group_hover("reply", |style| style.visible())
                    .child(
                        IconButton::new(("delete-reply", ix), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Delete reply"))
                            .on_click(cx.listener(move |this, _, _, cx| this.delete_reply(ix, cx))),
                    )
                    .into_any_element()
            })
            .into_any_element()
    }
}

impl EventEmitter<NoteThreadEvent> for NoteThread {}

impl Focusable for NoteThread {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for NoteThread {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body_style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        let colors = cx.theme().colors();
        let accent = colors.text_accent;
        let (border_variant, editor_background, text_muted, text_color) = (
            colors.border_variant,
            colors.editor_background,
            colors.text_muted,
            colors.text,
        );
        let collapsed = self.collapsed;
        let reply_count = self.replies.len();

        let header = h_flex()
            .id("note-header")
            .w_full()
            .gap_1p5()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.collapsed = !this.collapsed;
                cx.notify();
            }))
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::Small)
                .color(Color::Muted),
            )
            .child(
                Label::new({
                    let label = match &self.finding {
                        Some(finding) if finding.deleted => "Agent review · deleted",
                        Some(finding) if finding.edited => "Agent review · edited by you",
                        Some(_) => "Agent review",
                        None => "Claude note",
                    };
                    match self.index {
                        Some(index) => format!("✦ {index} · {label}"),
                        None => format!("✦ {label}"),
                    }
                })
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD)
                .color(Color::Accent),
            )
            .children(self.severity().map(|severity| crate::controls::severity_chip(severity, cx)))
            .when(!collapsed, |header| {
                header
                    .child(div().flex_1())
                    .child(Label::new(self.lines.clone()).size(LabelSize::XSmall).color(Color::Muted))
            })
            .when(collapsed, |header| {
                header
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(text_muted)
                            .child(self.text.clone()),
                    )
                    .child(Label::new(self.lines.clone()).size(LabelSize::XSmall).color(Color::Muted))
                    .when(reply_count > 0, |header| {
                        header.child(
                            Label::new(format!("{reply_count} replies"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
            });

        let replies: Vec<AnyElement> = self
            .replies
            .iter()
            .enumerate()
            .map(|(ix, reply)| self.render_reply(ix, reply, cx))
            .collect();

        let finding = self.finding.clone();
        let deleted = finding.as_ref().is_some_and(|finding| finding.deleted);
        let editing = self.editing;
        let finding_buttons = finding.as_ref().map(|finding| {
            h_flex()
                .pl_5()
                .gap_1()
                .when(!finding.deleted && !editing, |row| {
                    row.child(
                        Button::new("edit-finding", "Edit")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|this, _, window, cx| this.start_edit(window, cx))),
                    )
                })
                .when(!editing, |row| {
                    row.child(
                        Button::new("delete-finding", if finding.deleted { "Restore" } else { "Delete" })
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_deleted(cx))),
                    )
                })
        });
        let in_draft = self.in_draft;
        let tick = finding.as_ref().filter(|finding| !finding.deleted).map(|_| {
            Checkbox::new("finding-in-draft", if in_draft { ToggleState::Selected } else { ToggleState::Unselected })
                .tooltip(Tooltip::text(if in_draft {
                    "In the review draft. Untick to take it out."
                } else {
                    "Add to the review draft"
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.in_draft = !in_draft;
                    cx.emit(NoteThreadEvent::DraftToggled { add: !in_draft });
                    cx.notify();
                }))
        });
        let border = if in_draft { accent.opacity(0.7) } else { accent.opacity(0.35) };
        v_flex()
            .max_w(px(820.))
            .flex_1()
            .my_1()
            .when(deleted, |thread| thread.opacity(0.5))
            .px_3()
            .py_2()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(accent.opacity(if in_draft { 0.1 } else { 0.06 }))
            .shadow_xs()
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .children(tick)
                    .child(div().flex_1().min_w_0().child(header))
                    .when(in_draft, |row| {
                        row.child(Label::new("in draft").size(LabelSize::XSmall).color(Color::Accent))
                    }),
            )
            .when(!collapsed, |thread| {
                thread
                    .child(
                        div()
                            .pl_5()
                            .text_color(text_color)
                            .when(deleted, |body| body.line_through())
                            .child(MarkdownElement::new(self.body.clone(), body_style)),
                    )
                    .children(finding_buttons)
                    .when(!replies.is_empty(), |thread| {
                        thread.child(
                            v_flex()
                                .ml_5()
                                .mt_1()
                                .border_t_1()
                                .border_color(border_variant)
                                .children(replies),
                        )
                    })
                    .when(finding.is_none() || editing, |thread| thread.child(
                        h_flex()
                            .ml_5()
                            .mt_1()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(border_variant)
                            .bg(editor_background)
                            .child(div().flex_1().min_w_0().child(self.input.clone()))
                            .child(
                                Button::new("reply", if editing { "Save" } else { "Reply" })
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit(window, cx)
                                    })),
                            ),
                    ))
            })
    }
}

pub fn draft_key(file: &str, note: &GuideNote) -> String {
    format!("{file}:{}-{}", note.line_start, note.line_end)
}
