//! A diff tab with only the files of one guide chapter, in guide order.
//!
//! The old side is each file at the merge base. The new side is the real
//! file buffer, so inline review comments work as in the branch diff.

use crate::{
    controls,
    review_round::{self, Item as DraftItem},
    findings::{self, ExternalAnnotation},
    panel::ReviewPanel,
    guide::{GuideDiff, GuideNote},
    session::Session,
    note_thread::{self, NoteThread, NoteThreadEvent, ThreadDraft},
};
use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use editor::{
    Anchor, Editor, EditorEvent, ExportedReviewComment, EditorSettings, HiddenUnstagedDiffHunkRenderer,
    SelectionEffects, SplittableEditor,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle},
    multibuffer_context_lines,
    scroll::Autoscroll,
};
use git_ui_core::file_diff_view::build_buffer_diff;
use gpui::{
    Action, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, Font, FontWeight,
    IntoElement, KeyBinding, Render, SharedString, Subscription, Task, WeakEntity, Window, actions, px,
};
use schemars::JsonSchema;
use serde::Deserialize;
use http_client::{HttpClient, Method};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use language::{Buffer, Capability, HighlightedText, OffsetRangeExt, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::{Project, ProjectPath};
use settings::Settings;
use std::{
    any::{Any, TypeId},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};
use ui::{Color, Icon, IconName, Tooltip, prelude::*};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, SaveOptions},
    searchable::SearchableItemHandle,
};

/// Jumps to an agent review finding by its number in the index.
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Action)]
#[action(namespace = plannotator_review)]
pub struct GoToFinding(pub usize);

actions!(
    plannotator_review,
    [
        /// Jumps to the next agent review finding.
        NextFinding,
        /// Jumps to the previous agent review finding.
        PreviousFinding
    ]
);

const FINDINGS_CONTEXT: &str = "PlannotatorFindings";

/// Alt-1 to Alt-9 jump to findings 1 to 9. Alt-] and Alt-[ step through them.
pub fn bind_finding_keys(cx: &mut App) {
    let context = Some(FINDINGS_CONTEXT);
    const KEYS: [&str; 9] = ["alt-1", "alt-2", "alt-3", "alt-4", "alt-5", "alt-6", "alt-7", "alt-8", "alt-9"];
    let mut bindings: Vec<KeyBinding> = KEYS
        .iter()
        .enumerate()
        .map(|(ix, key)| KeyBinding::new(key, GoToFinding(ix + 1), context))
        .collect();
    bindings.push(KeyBinding::new("alt-]", NextFinding, context));
    bindings.push(KeyBinding::new("alt-[", PreviousFinding, context));
    cx.bind_keys(bindings);
}

/// What a chapter view shows. Used to reuse an open tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChapterKey {
    pub guide_id: String,
    pub chapter: usize,
}

/// The chapter card above the diff: its place in the guide and its overview.
#[derive(Clone)]
pub struct ChapterIntro {
    pub index: usize,
    pub count: usize,
    pub title: String,
    /// Markdown from the guide. Mermaid blocks are drawn as text.
    pub overview: String,
    pub reviewed: bool,
}

/// What the buttons under the diff ask the Review panel to do.
pub enum ChapterNavEvent {
    Show(usize),
    ToggleReviewed(usize),
    SendFeedback,
    Approve,
    Draft(DraftChange),
}

/// A change to the review draft from a tab.
#[derive(Clone, Debug)]
pub enum DraftChange {
    Add(Vec<DraftItem>),
    Remove(String),
}

/// Gate 2 of the agent review: the tab shows the whole PR with its findings.
struct FindingsMode {
    session: Session,
    http: Arc<dyn HttpClient>,
    worktree: String,
    /// The review body and the findings that are not on a diff line.
    general: Vec<Entity<NoteThread>>,
    /// `Some(approved)` after Approve or Send feedback.
    sent: Option<bool>,
}

pub struct ChapterDiffView {
    editor: Entity<SplittableEditor>,
    workspace: WeakEntity<Workspace>,
    findings: Option<FindingsMode>,
    key: ChapterKey,
    title: SharedString,
    intro: ChapterIntro,
    overview: Option<Entity<Markdown>>,
    overview_open: bool,
    path_keys: Vec<(String, PathKey)>,
    /// Inline comments in the editor, without the replies to notes.
    editor_comment_count: usize,
    note_threads: Vec<Entity<NoteThread>>,
    /// Where each of `note_threads` sits in the diff.
    note_anchors: Vec<Option<Anchor>>,
    /// The last finding you jumped to, by its number.
    current_finding: Option<usize>,
    /// The Review panel that owns the guided review session. Its state sets
    /// the Send feedback and Approve bar of a guide chapter.
    panel: Option<(WeakEntity<ReviewPanel>, Subscription)>,
    /// The keys of the items in the review draft.
    draft_keys: std::collections::BTreeSet<String>,
    _subscriptions: Vec<Subscription>,
}

impl ChapterDiffView {
    /// Opens the chapter in a new tab and closes other chapter tabs that have
    /// no unsent review comments.
    pub fn open(
        key: ChapterKey,
        title: String,
        intro: ChapterIntro,
        root: PathBuf,
        base: String,
        diffs: Vec<GuideDiff>,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let project = workspace.project().clone();
        let workspace = workspace.weak_handle();
        let context_lines = multibuffer_context_lines(cx);

        window.spawn(cx, async move |cx| {
            let files: Vec<String> = diffs.iter().map(|diff| diff.file.clone()).collect();
            let old_texts = {
                let root = root.clone();
                cx.background_executor()
                    .spawn(async move { old_texts(&root, &base, &files) })
                    .await?
            };

            let mut entries = Vec::with_capacity(diffs.len());
            for (guide_diff, old_text) in diffs.into_iter().zip(old_texts) {
                let abs_path = root.join(&guide_diff.file);
                // A deleted file has no buffer to comment on, so the chapter skips it.
                let Some(new_buffer) = project
                    .update(cx, |project, cx| project.open_local_buffer(&abs_path, cx))
                    .await
                    .log_err()
                else {
                    continue;
                };
                let old_buffer = cx.new(|cx| Buffer::local(old_text, cx));
                let diff = build_buffer_diff(&old_buffer, &new_buffer, cx).await?;
                entries.push((guide_diff, new_buffer, old_buffer, diff));
            }

            workspace.update_in(cx, |workspace, window, cx| {
                let multibuffer = cx.new(|cx| {
                    let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
                    multibuffer.set_all_diff_hunks_expanded(cx);
                    multibuffer
                });
                let mut path_keys = Vec::with_capacity(entries.len());
                for (ix, (guide_diff, new_buffer, _, diff)) in entries.iter().enumerate() {
                    let path_key = path_key_for(ix, &guide_diff.file);
                    register_file(
                        &multibuffer,
                        &path_key,
                        new_buffer,
                        diff,
                        &guide_diff.notes,
                        context_lines,
                        cx,
                    );
                    path_keys.push((guide_diff.file.clone(), path_key));
                }
                let note_placements = note_placements(&multibuffer, &entries, cx);

                let workspace_handle = cx.entity();
                let view = cx.new(|cx| {
                    Self::new(
                        key,
                        title.into(),
                        intro,
                        multibuffer,
                        project.clone(),
                        path_keys,
                        workspace_handle,
                        window,
                        cx,
                    )
                });
                view.update(cx, |view, cx| view.add_note_threads(note_placements, window, cx));

                let old_views: Vec<Entity<Self>> = workspace
                    .items_of_type::<Self>(cx)
                    .filter(|old| old.read(cx).review_comment_count(cx) == 0)
                    .collect();
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
                });
                crate::close_items(workspace, old_views, window, cx);
                view
            })
        })
    }

    fn new(
        key: ChapterKey,
        title: SharedString,
        intro: ChapterIntro,
        multibuffer: Entity<MultiBuffer>,
        project: Entity<Project>,
        path_keys: Vec<(String, PathKey)>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_handle = workspace.downgrade();
        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer,
                project,
                workspace,
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenUnstagedDiffHunkRenderer)), cx);
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_show_diff_review_button(true, cx);
            });
            editor
        });

        let rhs_editor = editor.read(cx).rhs_editor().clone();
        let subscriptions = vec![
            cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
                if event == &(EditorEvent::SelectionsChanged { local: true }) {
                    cx.emit(event.clone())
                }
            }),
            cx.subscribe(&rhs_editor, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::ReviewCommentsChanged { total_count } = event {
                    this.editor_comment_count = *total_count;
                    this.emit_comment_count(cx);
                }
            }),
        ];

        let overview = (!intro.overview.trim().is_empty()).then(|| {
            let source = crate::rich_text::render_mermaid(&intro.overview);
            cx.new(|cx| Markdown::new(source.into(), None, None, cx))
        });
        Self {
            editor,
            workspace: workspace_handle,
            findings: None,
            key,
            title,
            intro,
            overview,
            overview_open: true,
            path_keys,
            editor_comment_count: 0,
            note_threads: Vec::new(),
            note_anchors: Vec::new(),
            current_finding: None,
            panel: None,
            draft_keys: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Unsent inline comments plus unsent replies to Claude's notes.
    pub fn review_comment_count(&self, cx: &App) -> usize {
        self.editor_comment_count
            + self
                .note_threads
                .iter()
                .map(|thread| thread.read(cx).unsent_count())
                .sum::<usize>()
    }

    fn emit_comment_count(&self, cx: &mut Context<Self>) {
        // The Review panel listens for the count.
        cx.emit(EditorEvent::ReviewCommentsChanged {
            total_count: self.review_comment_count(cx),
        });
    }

    fn add_note_threads(
        &mut self,
        placements: Vec<(BlockPlacement<Anchor>, String, GuideNote)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut drafts = load_drafts(&self.key.guide_id);
        let mut blocks = Vec::with_capacity(placements.len());
        let mut finding_count = 0;
        for (placement, file, note) in placements {
            let anchor = match &placement {
                BlockPlacement::Above(anchor) | BlockPlacement::Below(anchor) => Some(*anchor),
                _ => None,
            };
            let thread = match note.finding_id.clone() {
                Some(id) => {
                    finding_count += 1;
                    let index = finding_count;
                    cx.new(|cx| {
                        let mut thread = NoteThread::new_finding(file, note, id, window, cx);
                        thread.set_index(index, cx);
                        thread
                    })
                }
                None => {
                    let draft = drafts
                        .remove(&note_thread::draft_key(&file, &note))
                        .unwrap_or_default();
                    cx.new(|cx| NoteThread::new(file, note, draft, window, cx))
                }
            };
            self.subscribe_thread(&thread, cx);
            self.note_threads.push(thread.clone());
            self.note_anchors.push(anchor);
            blocks.push(BlockProperties {
                placement,
                // The editor measures the rendered thread and corrects the height.
                height: Some(1),
                style: BlockStyle::Flex,
                render: Arc::new(move |cx: &mut BlockContext| {
                    h_flex()
                        .w_full()
                        .pl(cx.margins.gutter.full_width())
                        .pr_4()
                        .child(thread.clone())
                        .into_any_element()
                }),
                priority: 0,
            });
        }
        self.rhs_editor(cx).update(cx, |editor, cx| {
            editor.insert_blocks(blocks, None, cx);
        });
        self.emit_comment_count(cx);
    }

    fn subscribe_thread(&mut self, thread: &Entity<NoteThread>, cx: &mut Context<Self>) {
        self._subscriptions.push(cx.subscribe(thread, |this, thread, event, cx| match event {
            NoteThreadEvent::RepliesChanged => {
                this.save_drafts(cx);
                this.emit_comment_count(cx);
            }
            NoteThreadEvent::FindingChanged { id, text } => this.sync_finding(id, text.clone(), cx),
            NoteThreadEvent::DraftToggled { add } => {
                let Some(item) = thread.read(cx).draft_item() else {
                    return;
                };
                let change = if *add { DraftChange::Add(vec![item]) } else { DraftChange::Remove(item.key) };
                cx.emit(ChapterNavEvent::Draft(change));
            }
        }));
    }

    /// Turns this tab into gate 2 of the agent review.
    pub fn set_findings(
        &mut self,
        session: Session,
        http: Arc<dyn HttpClient>,
        worktree: String,
        general: Vec<ExternalAnnotation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let threads: Vec<Entity<NoteThread>> = general
            .iter()
            .map(|annotation| {
                let note = findings::general_note(annotation);
                let file = annotation.file_path.clone().unwrap_or_default();
                cx.new(|cx| NoteThread::new_finding(file, note, annotation.id.clone(), window, cx))
            })
            .collect();
        for thread in &threads {
            self.subscribe_thread(thread, cx);
        }
        self.findings = Some(FindingsMode {
            session,
            http,
            worktree,
            general: threads,
            sent: None,
        });
        cx.notify();
    }

    pub fn findings_session_pid(&self) -> Option<i32> {
        self.findings.as_ref().map(|findings| findings.session.pid)
    }

    /// Keeps an edit in the Plannotator session, so it survives a restart of Zed.
    fn sync_finding(&self, id: &str, text: Option<String>, cx: &mut Context<Self>) {
        let Some(findings) = &self.findings else {
            return;
        };
        let Some(text) = text else {
            // A delete stays in Zed until you send, so Restore can undo it.
            return;
        };
        let http = findings.http.clone();
        let url = format!("{}/api/external-annotations?id={id}", findings.session.url);
        cx.background_spawn(async move {
            crate::client::send_json(&http, Method::PATCH, &url, &serde_json::json!({ "text": text })).await
        })
        .detach_and_log_err(cx);
    }

    /// Approve or Send feedback: the findings as you left them, plus your own
    /// inline comments, as one message to the Plannotator session.
    pub(crate) fn send_findings(&mut self, approved: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(findings) = &self.findings else {
            return;
        };
        let mut inline: Vec<ExportedReviewComment> = self
            .note_threads
            .iter()
            .filter_map(|thread| thread.read(cx).finding_export())
            .collect();
        inline.extend(self.rhs_editor(cx).update(cx, |editor, cx| editor.take_review_comments_for_export(cx)));
        let general: Vec<String> = findings
            .general
            .iter()
            .filter_map(|thread| thread.read(cx).finding_export())
            .map(|comment| comment.text)
            .collect();
        let count = inline.len() + general.len();
        let message = findings::message(&inline, &general, &findings.worktree);
        // The review draft posts now, so the skill stops after gate 2.
        let message = if approved { format!("{}\n\n{message}", findings::ZED8_DRAFT_NOTE) } else { message };
        let http = findings.http.clone();
        let url = format!("{}/api/feedback", findings.session.url);
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let body = serde_json::json!({ "approved": approved, "feedback": message, "annotations": [] });
            let result = crate::client::send_json(&http, Method::POST, &url, &body).await;
            let toast = match &result {
                Ok(()) if approved => "Gate 2 done. The agent review saves its findings and posts nothing.".to_string(),
                Ok(()) => format!("Sent {count} comments to the agent review."),
                Err(error) => format!("Send failed: {error}"),
            };
            if result.is_ok() {
                this.update(cx, |this, cx| {
                    if let Some(findings) = this.findings.as_mut() {
                        findings.sent = Some(approved);
                    }
                    cx.notify();
                })
                .log_err();
            }
            workspace
                .update(cx, |workspace, cx| crate::show_toast(workspace, toast, cx))
                .log_err();
        })
        .detach();
    }

    /// The inline findings in index order, with where each one sits.
    fn indexed_findings(&self, cx: &App) -> Vec<(Entity<NoteThread>, Anchor)> {
        self.note_threads
            .iter()
            .zip(&self.note_anchors)
            .filter(|(thread, _)| thread.read(cx).finding().is_some())
            .filter_map(|(thread, anchor)| Some((thread.clone(), (*anchor)?)))
            .collect()
    }

    /// Scrolls to finding `index` (from 1) and puts the cursor on it.
    fn go_to_finding(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let findings = self.indexed_findings(cx);
        let Some((thread, anchor)) = index.checked_sub(1).and_then(|ix| findings.get(ix)).cloned() else {
            return;
        };
        self.current_finding = Some(index);
        thread.update(cx, |thread, cx| thread.expand(cx));
        let rhs_editor = self.rhs_editor(cx);
        rhs_editor.update(cx, |editor, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_anchor_ranges([anchor..anchor]),
            );
        });
        window.focus(&rhs_editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn step_finding(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.indexed_findings(cx).len();
        if count == 0 {
            return;
        }
        let next = match (self.current_finding, forward) {
            (None, true) => 1,
            (None, false) => count,
            (Some(current), true) => current % count + 1,
            (Some(current), false) => (current + count - 2) % count + 1,
        };
        self.go_to_finding(next, window, cx);
    }

    /// One numbered chip for each inline finding. A click jumps to it.
    fn render_findings_index(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let findings = self.indexed_findings(cx);
        if findings.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let (selected_bg, hover_bg, border) =
            (colors.ghost_element_selected, colors.ghost_element_hover, colors.border_variant);
        let chips = findings.iter().enumerate().map(|(ix, (thread, _))| {
            let index = ix + 1;
            let thread = thread.read(cx);
            let deleted = thread.finding().is_some_and(|finding| finding.deleted);
            let file = thread.file().rsplit('/').next().unwrap_or(thread.file()).to_string();
            let shortcut = if index <= 9 { format!("  ⌥{index}") } else { String::new() };
            let tooltip = format!("{}:{}\n{}{shortcut}", thread.file(), thread.line_start(), thread.first_line());
            h_flex()
                .id(("finding-index", ix))
                .gap_1()
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .border_1()
                .border_color(border)
                .cursor_pointer()
                .when(self.current_finding == Some(index), |chip| chip.bg(selected_bg))
                .hover(move |style| style.bg(hover_bg))
                .tooltip(Tooltip::text(tooltip))
                .child(Label::new(index.to_string()).size(LabelSize::XSmall).weight(FontWeight::SEMIBOLD).color(Color::Accent))
                .children(thread.severity().map(|severity| controls::severity_chip(severity, cx)))
                .child(
                    Label::new(format!("{file}:{}", thread.line_start()))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .when(deleted, |label| label.strikethrough()),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.go_to_finding(index, window, cx)))
        });
        Some(
            v_flex()
                .px_3()
                .pb_2()
                .gap_1()
                .child(
                    Label::new("INDEX · ⌥1–⌥9 JUMP · ⌥] NEXT · ⌥[ PREVIOUS")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(h_flex().flex_wrap().gap_1().children(chips)),
        )
    }

    /// The review draft keys, so each finding shows whether it is ticked.
    pub fn set_draft_keys(&mut self, keys: std::collections::BTreeSet<String>, cx: &mut Context<Self>) {
        if self.draft_keys == keys {
            return;
        }
        for thread in self.note_threads.iter().chain(self.findings.iter().flat_map(|findings| findings.general.iter())) {
            let key = thread.read(cx).finding().map(|finding| format!("agent:{}", finding.id));
            let in_draft = key.is_some_and(|key| keys.contains(&key));
            thread.update(cx, |thread, cx| thread.set_in_draft(in_draft, cx));
        }
        self.draft_keys = keys;
        cx.notify();
    }

    fn finding_threads(&self) -> impl Iterator<Item = &Entity<NoteThread>> {
        self.note_threads.iter().chain(self.findings.iter().flat_map(|findings| findings.general.iter()))
    }

    /// The findings of gate 2: how many there are and how many are ticked.
    pub(crate) fn findings_counts(&self, cx: &App) -> Option<(usize, usize)> {
        self.findings.as_ref()?;
        let threads: Vec<&Entity<NoteThread>> =
            self.finding_threads().filter(|thread| thread.read(cx).finding().is_some()).collect();
        let ticked = threads.iter().filter(|thread| thread.read(cx).in_draft()).count();
        Some((threads.len(), ticked))
    }

    /// Whether gate 2 is answered, and how.
    pub(crate) fn findings_sent(&self) -> Option<bool> {
        self.findings.as_ref()?.sent
    }

    /// Ticks every finding of Medium or higher that is not ticked yet.
    fn tick_medium_and_up(&mut self, cx: &mut Context<Self>) {
        let items: Vec<DraftItem> = self
            .finding_threads()
            .filter(|thread| {
                let thread = thread.read(cx);
                !thread.in_draft() && thread.severity().is_some_and(|severity| findings::severity_rank(severity) >= 3)
            })
            .filter_map(|thread| thread.read(cx).draft_item())
            .collect();
        if !items.is_empty() {
            cx.emit(ChapterNavEvent::Draft(DraftChange::Add(items)));
        }
    }

    /// Moves your inline comments of this tab into the review draft.
    fn add_comments_to_draft(&mut self, cx: &mut Context<Self>) {
        let comments = self.rhs_editor(cx).update(cx, |editor, cx| editor.take_review_comments_for_export(cx));
        let items: Vec<DraftItem> = comments
            .into_iter()
            .map(|comment| DraftItem {
                key: review_round::your_key(&comment.file_path, comment.line_start, &comment.text),
                source: review_round::Source::You,
                line: Some(comment.line_start),
                line_end: (comment.line_end > comment.line_start).then_some(comment.line_end),
                path: Some(comment.file_path),
                body: comment.text,
                include: true,
                ..Default::default()
            })
            .collect();
        if !items.is_empty() {
            cx.emit(ChapterNavEvent::Draft(DraftChange::Add(items)));
        }
    }

    /// The bar of the tab: what is in the review draft, and Ask Claude and
    /// Done for the Claude session behind the tab. Nothing here posts.
    fn render_draft_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let colors = cx.theme().colors().clone();
        let your_comments = self.editor_comment_count;
        let (status, color, summary, ask, done, medium) = match &self.findings {
            Some(findings) => {
                let (total, ticked) = self.findings_counts(cx).unwrap_or_default();
                let (status, color) = match findings.sent {
                    Some(true) => ("Done", Color::Success),
                    Some(false) => ("Sent to the agent review", Color::Accent),
                    None => ("Gate 2 · waits on you", Color::Warning),
                };
                let untick_medium = self.finding_threads().any(|thread| {
                    let thread = thread.read(cx);
                    !thread.in_draft() && thread.severity().is_some_and(|severity| findings::severity_rank(severity) >= 3)
                });
                let open = findings.sent.is_none();
                (status.to_string(), color, format!("{ticked} of {total} in draft"), open, open, untick_medium)
            }
            None => {
                let panel = self.panel.as_ref()?.0.upgrade()?;
                let status = panel.read(cx).guided_status(cx);
                let count = self.review_comment_count(cx);
                let summary = match self.draft_keys.iter().filter(|key| key.starts_with("you:")).count() {
                    0 => "Tick comments into the draft, or ask Claude".to_string(),
                    count => format!("{count} of your comments in draft"),
                };
                (status.text, status.color, summary, status.has_session && count > 0, status.has_session, false)
            }
        };
        let this = cx.weak_entity();
        let findings_mode = self.findings.is_some();
        Some(
            h_flex()
                .w_full()
                .flex_wrap()
                .px_3()
                .py_2()
                .gap_2()
                .border_b_1()
                .border_color(colors.border_variant)
                .bg(colors.title_bar_background)
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(color))
                        .child(Label::new(status).size(LabelSize::Small).color(color)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(120.))
                        .child(Label::new(summary).size(LabelSize::Small).color(Color::Accent).truncate()),
                )
                .when(medium, |bar| {
                    bar.child(
                        Button::new("tick-medium", "Tick Medium and up")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.tick_medium_and_up(cx))),
                    )
                })
                .when(your_comments > 0, |bar| {
                    bar.child(
                        Button::new("add-comments-to-draft", format!("Add {your_comments} to draft"))
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                            .tooltip(Tooltip::text("Moves your inline comments of this tab into the review draft"))
                            .on_click(cx.listener(|this, _, _, cx| this.add_comments_to_draft(cx))),
                    )
                })
                .child(
                    Button::new("ask-claude", "Ask Claude")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Send).size(IconSize::XSmall))
                        .disabled(!ask)
                        .tooltip(Tooltip::text(if findings_mode {
                            "Sends your edits and comments to the agent review. It revises the findings."
                        } else {
                            "Sends your comments and replies to Claude in the review session. Claude answers there."
                        }))
                        .on_click({
                            let this = this.clone();
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                this.update(cx, |this, cx| {
                                    if this.findings.is_some() {
                                        this.send_findings(false, window, cx)
                                    } else {
                                        cx.emit(ChapterNavEvent::SendFeedback)
                                    }
                                })
                                .ok();
                            }
                        }),
                )
                .child(
                    Button::new("review-done", "Done")
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success))
                        .disabled(!done)
                        .tooltip(Tooltip::text(if findings_mode {
                            "Ends gate 2. The agent review saves its findings and posts nothing: the review draft posts."
                        } else {
                            "Tells Claude that you finished this review session."
                        }))
                        .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            this.update(cx, |this, cx| {
                                if this.findings.is_some() {
                                    this.send_findings(true, window, cx)
                                } else {
                                    cx.emit(ChapterNavEvent::Approve)
                                }
                            })
                            .ok();
                        }),
                )
                .into_any_element(),
        )
    }

    /// Follows the Review panel, for the guided review bar of this chapter.
    pub(crate) fn follow_panel(&mut self, panel: &Entity<ReviewPanel>, cx: &mut Context<Self>) {
        let subscription = cx.observe(panel, |_, _, cx| cx.notify());
        self.panel = Some((panel.downgrade(), subscription));
        cx.notify();
    }

    fn render_findings_header(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let findings = self.findings.as_ref()?;
        let colors = cx.theme().colors();
        Some(
            v_flex()
                .w_full()
                .border_b_1()
                .border_color(colors.border_variant)
                .bg(colors.surface_background)
                .children(self.render_draft_bar(cx))
                .children(self.render_findings_index(cx).map(|index| div().pt_2().child(index)))
                .when(!findings.general.is_empty(), |header| {
                    header.child(
                        v_flex()
                            .id("general-findings")
                            .px_3()
                            .pb_2()
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .child(
                                Label::new("REVIEW BODY AND COMMENTS NOT ON A DIFF LINE")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .children(findings.general.iter().cloned()),
                    )
                }),
        )
    }

    /// Writes this chapter's threads into the guide's drafts file. Threads of
    /// other chapters in the file stay as they are.
    fn save_drafts(&self, cx: &App) {
        let mut drafts = load_drafts(&self.key.guide_id);
        for thread in &self.note_threads {
            let thread = thread.read(cx);
            if thread.finding().is_some() {
                continue;
            }
            let draft = thread.draft();
            if draft.replies.is_empty() {
                drafts.remove(&thread.key());
            } else {
                drafts.insert(thread.key(), draft);
            }
        }
        let path = drafts_path(&self.key.guide_id);
        let result = std::fs::create_dir_all(crate::paths::drafts_dir())
            .map_err(anyhow::Error::from)
            .and_then(|()| Ok(std::fs::write(&path, serde_json::to_vec_pretty(&drafts)?)?));
        if let Err(error) = result {
            log::error!("plannotator: cannot save reply drafts to {}: {error:#}", path.display());
        }
    }

    /// Unsent replies to Claude's notes, one comment for each note. Marks them sent.
    pub fn take_replies_for_export(&mut self, cx: &mut Context<Self>) -> Vec<ExportedReviewComment> {
        let replies = self
            .note_threads
            .clone()
            .into_iter()
            .filter_map(|thread| thread.update(cx, |thread, cx| thread.take_unsent(cx)))
            .collect();
        self.emit_comment_count(cx);
        replies
    }

    pub fn set_reviewed(&mut self, reviewed: bool, cx: &mut Context<Self>) {
        if self.intro.reviewed != reviewed {
            self.intro.reviewed = reviewed;
            cx.notify();
        }
    }

    fn render_intro(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let mut style = MarkdownStyle::themed(MarkdownFont::Agent, window, cx);
        style.container_style.text.color = Some(colors.text_muted);
        let open = self.overview_open;
        v_flex()
            .mx_3()
            .mt_2()
            .mb_1()
            .px_3()
            .py_2()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.surface_background)
            .child(
                h_flex()
                    .id("chapter-intro-header")
                    .w_full()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.overview_open = !this.overview_open;
                        cx.notify();
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(format!(
                                    "CHAPTER {} OF {}",
                                    self.intro.index + 1,
                                    self.intro.count
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            )
                            .child(
                                Label::new(self.intro.title.clone())
                                    .size(LabelSize::Large)
                                    .weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .when(self.intro.reviewed, |row| {
                        row.child(
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::Check).size(IconSize::Small).color(Color::Success))
                                .child(Label::new("Reviewed").size(LabelSize::Small).color(Color::Success)),
                        )
                    })
                    .when(self.overview.is_some(), |row| {
                        row.child(
                            Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when_some(self.overview.clone().filter(|_| open), |card, overview| {
                card.child(
                    div()
                        .id("chapter-overview")
                        .max_h(px(260.))
                        .overflow_y_scroll()
                        .child(MarkdownElement::new(overview, style)),
                )
            })
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ix = self.intro.index;
        let last = ix + 1 >= self.intro.count;
        h_flex()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_2()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Button::new("chapter-previous", "Previous chapter")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::ArrowLeft).size(IconSize::XSmall))
                    .disabled(ix == 0)
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(ChapterNavEvent::Show(ix - 1)))),
            )
            .child(
                Button::new(
                    "chapter-reviewed",
                    if self.intro.reviewed { "Reviewed" } else { "Mark chapter reviewed" },
                )
                .style(if self.intro.reviewed {
                    ButtonStyle::Tinted(ui::TintColor::Success)
                } else {
                    ButtonStyle::Filled
                })
                .label_size(LabelSize::Small)
                .start_icon(Icon::new(IconName::Check).size(IconSize::XSmall))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(ChapterNavEvent::ToggleReviewed(ix)))),
            )
            .child(
                Button::new("chapter-next", "Next chapter")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .end_icon(Icon::new(IconName::ArrowRight).size(IconSize::XSmall))
                    .disabled(last)
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(ChapterNavEvent::Show(ix + 1)))),
            )
    }

    pub fn key(&self) -> &ChapterKey {
        &self.key
    }

    /// The editor that holds the inline review comments.
    pub fn rhs_editor(&self, cx: &App) -> Entity<Editor> {
        self.editor.read(cx).rhs_editor().clone()
    }

    pub fn scroll_to_file(&self, file: &str, window: &mut Window, cx: &mut App) {
        let Some((_, path_key)) = self.path_keys.iter().find(|(path, _)| path == file) else {
            return;
        };
        let rhs_editor = self.rhs_editor(cx);
        let multibuffer = rhs_editor.read(cx).buffer().clone();
        let Some(position) = multibuffer.read(cx).location_for_path(path_key, cx) else {
            return;
        };
        rhs_editor.update(cx, |editor, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::top_relative(0.)),
                window,
                cx,
                |selections| selections.select_ranges([position..position]),
            );
        });
        window.focus(&rhs_editor.focus_handle(cx), cx);
    }
}

fn path_key_for(ix: usize, file: &str) -> PathKey {
    let rel_path = RelPath::new(Path::new(file), PathStyle::Unix)
        .or_else(|_| RelPath::new(Path::new("untitled"), PathStyle::Unix))
        .expect("a plain file name is a valid relative path")
        .into_owned()
        .into();
    PathKey::with_sort_prefix(ix as u64, rel_path)
}

fn register_file(
    multibuffer: &Entity<MultiBuffer>,
    path_key: &PathKey,
    new_buffer: &Entity<Buffer>,
    diff: &Entity<BufferDiff>,
    notes: &[GuideNote],
    context_lines: u32,
    cx: &mut App,
) {
    let snapshot = new_buffer.read(cx).snapshot();
    let mut ranges: Vec<_> = diff
        .read(cx)
        .snapshot(cx)
        .hunks(&snapshot)
        .map(|hunk| hunk.buffer_range.to_point(&snapshot))
        .collect();
    // A note on unchanged lines needs those lines in view.
    let max_row = snapshot.max_point().row;
    for note in notes.iter().filter(|note| !is_old_side(note)) {
        let start = note.line_start.saturating_sub(1).min(max_row);
        let end = note.line_end.saturating_sub(1).min(max_row);
        ranges.push(Point::new(start, 0)..Point::new(end, snapshot.line_len(end)));
    }
    ranges.sort_by_key(|range| range.start);
    multibuffer.update(cx, |multibuffer, cx| {
        multibuffer.set_excerpts_for_path(
            path_key.clone(),
            new_buffer.clone(),
            ranges,
            context_lines,
            cx,
        );
        multibuffer.add_diff(diff.clone(), cx);
    });
}

fn drafts_path(guide_id: &str) -> PathBuf {
    crate::paths::drafts_dir().join(format!("{guide_id}.json"))
}

fn load_drafts(guide_id: &str) -> std::collections::BTreeMap<String, ThreadDraft> {
    std::fs::read(drafts_path(guide_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn is_old_side(note: &GuideNote) -> bool {
    note.side.as_deref() == Some("old")
}

/// Where each of Claude's notes goes. A new-side note goes below its last
/// line. An old-side note goes above the diff hunk that removed its lines.
fn note_placements(
    multibuffer: &Entity<MultiBuffer>,
    entries: &[(GuideDiff, Entity<Buffer>, Entity<Buffer>, Entity<BufferDiff>)],
    cx: &App,
) -> Vec<(BlockPlacement<Anchor>, String, GuideNote)> {
    let multibuffer = multibuffer.read(cx).snapshot(cx);
    let mut placements = Vec::new();
    for (guide_diff, new_buffer, old_buffer, diff) in entries {
        let new_snapshot = new_buffer.read(cx).snapshot();
        let old_snapshot = old_buffer.read(cx).snapshot();
        let diff_snapshot = diff.read(cx).snapshot(cx);
        for note in &guide_diff.notes {
            let placement = if is_old_side(note) {
                let old_row = note.line_start.saturating_sub(1).min(old_snapshot.max_point().row);
                let old_offset = old_snapshot.point_to_offset(Point::new(old_row, 0));
                diff_snapshot
                    .hunks(&new_snapshot)
                    .find(|hunk| {
                        hunk.diff_base_byte_range.start <= old_offset
                            && old_offset <= hunk.diff_base_byte_range.end
                    })
                    .and_then(|hunk| multibuffer.anchor_in_excerpt(hunk.buffer_range.start))
                    .map(BlockPlacement::Above)
            } else {
                let row = note.line_end.saturating_sub(1).min(new_snapshot.max_point().row);
                let point = Point::new(row, new_snapshot.line_len(row));
                multibuffer
                    .anchor_in_excerpt(new_snapshot.anchor_after(point))
                    .map(BlockPlacement::Below)
            };
            match placement {
                Some(placement) => placements.push((placement, guide_diff.file.clone(), note.clone())),
                None => log::warn!("no place for the guide note on {}:{}", guide_diff.file, note.line_start),
            }
        }
    }
    placements
}

/// Each file's text at the merge base of HEAD and `base`. Empty for an added file.
fn old_texts(root: &Path, base: &str, files: &[String]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "HEAD", base])
        .output()
        .context("run git merge-base")?;
    anyhow::ensure!(output.status.success(), "git merge-base HEAD {base} failed");
    let merge_base = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(files
        .iter()
        .map(|file| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["show", &format!("{merge_base}:{file}")])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default()
        })
        .collect())
}

impl EventEmitter<EditorEvent> for ChapterDiffView {}
impl EventEmitter<ChapterNavEvent> for ChapterDiffView {}

impl Focusable for ChapterDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for ChapterDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.title.clone())
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            });
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }
}

impl Render for ChapterDiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().colors().editor_background;
        if let Some(header) = self.render_findings_header(cx).map(IntoElement::into_any_element) {
            return v_flex()
                .key_context(FINDINGS_CONTEXT)
                .on_action(cx.listener(|this, action: &GoToFinding, window, cx| {
                    this.go_to_finding(action.0, window, cx)
                }))
                .on_action(cx.listener(|this, _: &NextFinding, window, cx| this.step_finding(true, window, cx)))
                .on_action(cx.listener(|this, _: &PreviousFinding, window, cx| {
                    this.step_finding(false, window, cx)
                }))
                .size_full()
                .bg(background)
                .child(header)
                .child(div().flex_1().min_h_0().child(self.editor.clone()));
        }
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .children(self.render_draft_bar(cx))
            .child(self.render_intro(window, cx))
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
            .child(self.render_nav(cx))
    }
}
