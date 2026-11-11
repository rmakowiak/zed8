//! A diff tab with only the files of one guide chapter, in guide order.
//!
//! The old side is each file at the merge base. The new side is the real
//! file buffer, so inline review comments work as in the branch diff.

use crate::{
    guide::{GuideDiff, GuideNote},
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
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Font,
    IntoElement, Render, SharedString, Subscription, Task, Window,
};
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
use ui::{Color, Icon, IconName, prelude::*};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, SaveOptions},
    searchable::SearchableItemHandle,
};

/// What a chapter view shows. Used to reuse an open tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChapterKey {
    pub guide_id: String,
    pub chapter: usize,
}

pub struct ChapterDiffView {
    editor: Entity<SplittableEditor>,
    key: ChapterKey,
    title: SharedString,
    path_keys: Vec<(String, PathKey)>,
    /// Inline comments in the editor, without the replies to notes.
    editor_comment_count: usize,
    note_threads: Vec<Entity<NoteThread>>,
    _subscriptions: Vec<Subscription>,
}

impl ChapterDiffView {
    /// Opens the chapter in a new tab and closes other chapter tabs that have
    /// no unsent review comments.
    pub fn open(
        key: ChapterKey,
        title: String,
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
        multibuffer: Entity<MultiBuffer>,
        project: Entity<Project>,
        path_keys: Vec<(String, PathKey)>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
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

        Self {
            editor,
            key,
            title,
            path_keys,
            editor_comment_count: 0,
            note_threads: Vec::new(),
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
        for (placement, file, note) in placements {
            let draft = drafts
                .remove(&note_thread::draft_key(&file, &note))
                .unwrap_or_default();
            let thread = cx.new(|cx| NoteThread::new(file, note, draft, window, cx));
            self._subscriptions.push(cx.subscribe(&thread, |this, _, event, cx| match event {
                NoteThreadEvent::RepliesChanged => {
                    this.save_drafts(cx);
                    this.emit_comment_count(cx);
                }
            }));
            self.note_threads.push(thread.clone());
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

    /// Writes this chapter's threads into the guide's drafts file. Threads of
    /// other chapters in the file stay as they are.
    fn save_drafts(&self, cx: &App) {
        let mut drafts = load_drafts(&self.key.guide_id);
        for thread in &self.note_threads {
            let thread = thread.read(cx);
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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}
