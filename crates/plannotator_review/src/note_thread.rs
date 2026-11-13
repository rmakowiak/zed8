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
use ui::{TintColor, Tooltip, prelude::*};

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
    editing: bool,
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
            editing: false,
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
                Label::new(match &self.finding {
                    Some(finding) if finding.deleted => "✦ Agent review · deleted",
                    Some(finding) if finding.edited => "✦ Agent review · edited by you",
                    Some(_) => "✦ Agent review",
                    None => "✦ Claude note",
                })
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD)
                .color(Color::Accent),
            )
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
            .border_color(accent.opacity(0.35))
            .bg(accent.opacity(0.06))
            .shadow_xs()
            .child(header)
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
