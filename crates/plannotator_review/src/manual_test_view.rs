//! The Manual test results tab: every test case of the `manual-test` skill
//! with its proof. The skill writes `results.json` into the `manual-test`
//! folder of the review. The tab reads it, shows the evidence files, and
//! writes back the "Include in the PR review" box of each case.

use editor::{Editor, EditorEvent};
use gpui::{
    App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    ObjectFit, Render, Subscription, Window, img, px,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use ui::{Switch, ToggleState, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::item::{Item, ItemEvent};

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualTestResults {
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub sha: String,
    #[serde(default)]
    pub instance: String,
    #[serde(default)]
    pub ended_at: String,
    #[serde(default)]
    pub cases: Vec<TestCase>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCase {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub precondition: String,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub expected: String,
    #[serde(default)]
    pub observed: String,
    #[serde(default)]
    pub telemetry: String,
    /// `pass`, `fail` or `blocked`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: String,
    /// Paths relative to the `manual-test` folder.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Flagged for the PR: Post to PR sends it, and `pr-review` puts it in
    /// its review body until it is posted.
    #[serde(default)]
    pub include: bool,
    /// Your comment for the PR author.
    #[serde(default)]
    pub note: String,
    /// The evidence files that go with the comment. `None` means the first
    /// image or GIF of the case.
    #[serde(default)]
    pub attach: Option<Vec<String>>,
    /// When the Manual test thread posted the case on the PR.
    #[serde(default)]
    pub posted_at: String,
}

impl TestCase {
    pub fn failed(&self) -> bool {
        self.status != "pass"
    }

    /// The images and GIFs of the case, frames last.
    pub fn images(&self) -> Vec<&str> {
        let mut images: Vec<&str> = self.evidence.iter().map(String::as_str).filter(|path| is_image(Path::new(path))).collect();
        images.sort_by_key(|path| is_frame(path));
        images
    }

    /// The evidence files that go with the comment.
    pub fn attached(&self) -> Vec<String> {
        match &self.attach {
            Some(attach) => attach.clone(),
            None => self.images().first().map(|path| vec![path.to_string()]).unwrap_or_default(),
        }
    }

    /// Flagged and not yet posted.
    pub fn to_post(&self) -> bool {
        self.include && self.posted_at.is_empty()
    }
}

impl ManualTestResults {
    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |status: &str| self.cases.iter().filter(|case| case.status == status).count();
        (count("pass"), count("fail"), count("blocked"))
    }
}

pub fn load_results(path: &Path) -> Option<ManualTestResults> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes)
        .map_err(|error| log::warn!("plannotator: cannot read {}: {error}", path.display()))
        .ok()
}

/// Sets `postedAt` of the posted cases, keeping every other field.
fn write_posted(path: &Path, case_ids: &[String], at: &str) -> anyhow::Result<()> {
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if let Some(cases) = value.get_mut("cases").and_then(|cases| cases.as_array_mut()) {
        for entry in cases {
            let id = entry.get("id").and_then(|id| id.as_str()).map(str::to_string);
            if id.is_some_and(|id| case_ids.contains(&id)) {
                entry["postedAt"] = serde_json::Value::String(at.to_string());
            }
        }
    }
    std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

/// Writes your part of one case (`include`, `note`, `attach`) into the
/// file, and keeps every other field as the skill wrote it.
fn write_case(path: &Path, case: &TestCase) -> anyhow::Result<()> {
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let cases = value
        .get_mut("cases")
        .and_then(|cases| cases.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("no cases in {}", path.display()))?;
    for entry in cases {
        if entry.get("id").and_then(|id| id.as_str()) == Some(case.id.as_str()) {
            entry["include"] = serde_json::Value::Bool(case.include);
            entry["note"] = serde_json::Value::String(case.note.clone());
            if let Some(attach) = &case.attach {
                entry["attach"] = serde_json::json!(attach);
            }
        }
    }
    std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

/// One PR comment with the flagged cases that are not posted yet.
#[derive(Clone, Debug, PartialEq)]
pub struct PrComment {
    pub body: String,
    /// Absolute paths, as `gh pr comment --attach` takes them. The body
    /// links each one by the same path, and `gh` swaps in the uploaded URL.
    pub attachments: Vec<PathBuf>,
    pub case_ids: Vec<String>,
}

pub fn pr_comment(results: &ManualTestResults, folder: &Path) -> PrComment {
    let cases: Vec<&TestCase> = results.cases.iter().filter(|case| case.to_post()).collect();
    let sha = &results.sha[..results.sha.len().min(7)];
    let place = if results.instance.is_empty() || results.instance.contains("localhost") || results.instance.contains("127.0.0.1") {
        format!("a local build of `{sha}`")
    } else {
        format!("{} at `{sha}`", results.instance)
    };
    let count = match cases.len() {
        1 => "1 case to look at".to_string(),
        count => format!("{count} cases to look at"),
    };
    let mut parts = vec![format!("Manual test on {place}. {count}.")];
    let mut attachments = Vec::new();
    for case in &cases {
        parts.push(format!("### {} · {}", case.id, case.title));
        let outcome = [("Expected", &case.expected), ("Observed", &case.observed)]
            .into_iter()
            .filter(|(_, text)| !text.is_empty())
            .map(|(label, text)| format!("{label}: {text}"))
            .collect::<Vec<_>>()
            .join("<br>");
        let note = case.note.trim();
        let details_outcome = if note.is_empty() {
            if !outcome.is_empty() {
                parts.push(outcome.clone());
            }
            String::new()
        } else {
            parts.push(note.to_string());
            outcome
        };
        if !case.steps.is_empty() || !details_outcome.is_empty() {
            let mut details = "<details><summary>Steps</summary>\n\n".to_string();
            for (ix, step) in case.steps.iter().enumerate() {
                details.push_str(&format!("{}. {step}\n", ix + 1));
            }
            if !details_outcome.is_empty() {
                details.push_str(&format!("\n{details_outcome}\n"));
            }
            details.push_str("\n</details>");
            parts.push(details);
        }
        let images: Vec<String> = case
            .attached()
            .iter()
            .map(|file| {
                let path = folder.join(file);
                let link = format!("![{}](<{}>)", case.id, path.display());
                attachments.push(path);
                link
            })
            .collect();
        if !images.is_empty() {
            parts.push(images.join("\n"));
        }
    }
    PrComment {
        body: parts.join("\n\n") + "\n",
        attachments,
        case_ids: cases.iter().map(|case| case.id.clone()).collect(),
    }
}

enum Evidence {
    Image(PathBuf),
    Text { path: PathBuf, text: String },
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

fn is_frame(path: &str) -> bool {
    path.contains("-frame-")
}

/// Post comment in the preview: the Review panel posts it with `gh`.
pub enum ManualTestEvent {
    Post(PrComment),
}

enum PostState {
    Idle,
    Posting,
    Failed(String),
}

pub struct ManualTestView {
    path: PathBuf,
    results: ManualTestResults,
    selected: usize,
    /// The frame picked under the GIF, by its index in the frame list.
    frame: Option<usize>,
    /// Your comment on the selected case.
    note_editor: Entity<Editor>,
    /// The tab shows the PR comment instead of the cases.
    preview: bool,
    post_state: PostState,
    /// Set while the editor gets the text of a newly selected case, so that
    /// the text is not saved back as an edit.
    loading_note: bool,
    focus_handle: FocusHandle,
    _note_subscription: Subscription,
}

impl ManualTestView {
    pub fn new(path: PathBuf, results: ManualTestResults, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let selected = results.cases.iter().position(TestCase::failed).unwrap_or(0);
        let note_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 12, window, cx);
            editor.set_placeholder_text(
                "What should the PR author know about this case? The steps and the screenshots go with it.",
                window,
                cx,
            );
            editor
        });
        let note_subscription = cx.subscribe(&note_editor, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) && !this.loading_note {
                this.save_note(cx);
            }
        });
        let mut view = Self {
            path,
            results,
            selected,
            frame: None,
            note_editor,
            preview: false,
            post_state: PostState::Idle,
            loading_note: false,
            focus_handle: cx.focus_handle(),
            _note_subscription: note_subscription,
        };
        view.load_note(window, cx);
        view
    }

    fn load_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let note = self.results.cases.get(self.selected).map(|case| case.note.clone()).unwrap_or_default();
        self.loading_note = true;
        self.note_editor.update(cx, |editor, cx| editor.set_text(note, window, cx));
        self.loading_note = false;
    }

    fn save_note(&mut self, cx: &mut Context<Self>) {
        let note = self.note_editor.read(cx).text(cx);
        let Some(case) = self.results.cases.get_mut(self.selected) else {
            return;
        };
        if case.note == note {
            return;
        }
        case.note = note;
        write_case(&self.path, case).log_err();
        cx.notify();
    }

    fn toggle_attach(&mut self, file: String, cx: &mut Context<Self>) {
        let Some(case) = self.results.cases.get_mut(self.selected) else {
            return;
        };
        let mut attach = case.attached();
        match attach.iter().position(|attached| *attached == file) {
            Some(ix) => {
                attach.remove(ix);
            }
            None => attach.push(file),
        }
        case.attach = Some(attach);
        write_case(&self.path, case).log_err();
        cx.notify();
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file changed on disk. Your comment in the editor stays: it is
    /// the newest text, and the file has it a moment later.
    pub fn set_results(&mut self, mut results: ManualTestResults, cx: &mut Context<Self>) {
        if results != self.results {
            self.selected = self.selected.min(results.cases.len().saturating_sub(1));
            let note = self.note_editor.read(cx).text(cx);
            if let Some(case) = results.cases.get_mut(self.selected) {
                case.note = note;
            }
            self.results = results;
            // Posted from elsewhere, for example by Claude: the preview has
            // nothing left to show, so the tab goes back to the cases.
            if self.preview && !self.results.cases.iter().any(TestCase::to_post) {
                self.show_cases(cx);
            }
            cx.notify();
        }
    }

    /// Leaves the preview of the PR comment.
    pub fn show_cases(&mut self, cx: &mut Context<Self>) {
        self.preview = false;
        self.post_state = PostState::Idle;
        cx.notify();
    }

    fn folder(&self) -> PathBuf {
        self.path.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.save_note(cx);
        self.selected = ix;
        self.frame = None;
        self.load_note(window, cx);
        cx.notify();
    }

    fn toggle_include(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(case) = self.results.cases.get_mut(self.selected) else {
            return;
        };
        case.include = !case.include;
        let include = case.include;
        write_case(&self.path, case).log_err();
        if include {
            window.focus(&self.note_editor.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn post(&mut self, cx: &mut Context<Self>) {
        let comment = pr_comment(&self.results, &self.folder());
        if comment.case_ids.is_empty() {
            return;
        }
        self.post_state = PostState::Posting;
        cx.emit(ManualTestEvent::Post(comment));
        cx.notify();
    }

    /// The panel posted the comment, or failed to.
    pub fn post_finished(&mut self, case_ids: &[String], result: Result<(), String>, cx: &mut Context<Self>) {
        match result {
            Ok(()) => {
                let at = chrono::Utc::now().to_rfc3339();
                for case in self.results.cases.iter_mut().filter(|case| case_ids.contains(&case.id)) {
                    case.posted_at = at.clone();
                }
                write_posted(&self.path, case_ids, &at).log_err();
                self.preview = false;
                self.post_state = PostState::Idle;
            }
            Err(error) => self.post_state = PostState::Failed(error),
        }
        cx.notify();
    }

    fn copy_repro(&self, cx: &mut Context<Self>) {
        let Some(case) = self.results.cases.get(self.selected) else {
            return;
        };
        let mut text = format!("{} · {}\n\nPrecondition: {}\n\n", case.id, case.title, case.precondition);
        for (ix, step) in case.steps.iter().enumerate() {
            text.push_str(&format!("{}. {step}\n", ix + 1));
        }
        text.push_str(&format!("\nExpected: {}\nObserved: {}\n", case.expected, case.observed));
        if !case.error.is_empty() {
            text.push_str(&format!("Error: {}\n", case.error));
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn evidence(&self, case: &TestCase) -> (Option<Evidence>, Vec<PathBuf>) {
        let folder = self.folder();
        let frames: Vec<PathBuf> = case
            .evidence
            .iter()
            .filter(|path| is_frame(path))
            .map(|path| folder.join(path))
            .collect();
        let main = case.evidence.iter().filter(|path| !is_frame(path)).map(|path| folder.join(path)).find_map(
            |path| {
                if is_image(&path) {
                    return Some(Evidence::Image(path));
                }
                let text = std::fs::read_to_string(&path).ok()?;
                let text: String = text.chars().take(6000).collect();
                Some(Evidence::Text { path, text })
            },
        );
        let main = match self.frame.and_then(|ix| frames.get(ix)) {
            Some(frame) => Some(Evidence::Image(frame.clone())),
            None => main.or_else(|| frames.first().cloned().map(Evidence::Image)),
        };
        (main, frames)
    }

    fn status_label(status: &str) -> (&'static str, Color) {
        match status {
            "pass" => ("Pass", Color::Success),
            "fail" => ("Fail", Color::Error),
            "blocked" => ("Blocked", Color::Warning),
            _ => ("Pending", Color::Muted),
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (passed, failed, blocked) = self.results.counts();
        let sha = &self.results.sha[..self.results.sha.len().min(7)];
        let folder = self.folder();
        let flagged = self.results.cases.iter().filter(|case| case.to_post()).count();
        let posted = self.results.cases.iter().filter(|case| !case.posted_at.is_empty()).count();
        h_flex()
            .w_full()
            .px_4()
            .py_2()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().panel_background)
            .child(Label::new(format!("{passed} passed")).size(LabelSize::Small).color(Color::Success))
            .child(Label::new(format!("{failed} failed")).size(LabelSize::Small).color(if failed > 0 {
                Color::Error
            } else {
                Color::Muted
            }))
            .child(Label::new(format!("{blocked} blocked")).size(LabelSize::Small).color(if blocked > 0 {
                Color::Warning
            } else {
                Color::Muted
            }))
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(format!("{} · {sha}", self.results.instance))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .child(
                Button::new("evidence-folder", "Evidence folder")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .on_click(move |_, _, cx| cx.reveal_path(&folder)),
            )
            .child(div().h_4().w_px().bg(cx.theme().colors().border_variant))
            .child(
                Label::new(match (flagged, posted) {
                    (0, 0) => "Nothing in the review draft".to_string(),
                    (0, posted) => format!("✓ {posted} posted"),
                    (flagged, 0) => format!("{flagged} in the review draft"),
                    (flagged, posted) => format!("{flagged} in the review draft · ✓ {posted} posted"),
                })
                .size(LabelSize::Small)
                .color(if flagged > 0 { Color::Accent } else { Color::Muted }),
            )
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let selected_bg = colors.element_selected;
        let hover = colors.ghost_element_hover;
        let rows = self.results.cases.iter().enumerate().map(|(ix, case)| {
            let (status, color) = Self::status_label(&case.status);
            let kind = case
                .evidence
                .iter()
                .find(|path| !is_frame(path))
                .and_then(|path| Path::new(path).extension().and_then(|ext| ext.to_str()))
                .map(|ext| ext.to_uppercase())
                .unwrap_or_else(|| "none".to_string());
            h_flex()
                .id(("case", ix))
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .cursor_pointer()
                .when(ix == self.selected, |row| row.bg(selected_bg))
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(move |this, _, window, cx| this.select(ix, window, cx)))
                .child(div().w(px(40.)).child(Label::new(case.id.clone()).size(LabelSize::Small).color(Color::Muted)))
                .child(div().flex_1().min_w_0().child(Label::new(case.title.clone()).size(LabelSize::Small).truncate()))
                .child(Label::new(kind).size(LabelSize::XSmall).color(Color::Muted))
                .child(Label::new(status).size(LabelSize::XSmall).color(color))
                .child(div().w(px(12.)).flex_none().child(match (case.to_post(), case.posted_at.is_empty()) {
                    (_, false) => Label::new("✓").size(LabelSize::XSmall).color(Color::Success),
                    (true, true) => Label::new("⚑").size(LabelSize::XSmall).color(Color::Warning),
                    (false, true) => Label::new(""),
                }))
        });
        v_flex()
            .id("cases")
            .w(px(320.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(colors.border_variant)
            .child(
                div().px_3().pt_3().pb_1().child(
                    Label::new(format!("Test charter · {} cases", self.results.cases.len()))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .children(rows)
    }

    fn render_field(label: &str, body: impl IntoElement) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(Label::new(label.to_uppercase()).size(LabelSize::XSmall).color(Color::Muted))
            .child(body)
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(case) = self.results.cases.get(self.selected).cloned() else {
            return Label::new("No test cases yet.").color(Color::Muted).into_any_element();
        };
        let colors = cx.theme().colors().clone();
        let (status, color) = Self::status_label(&case.status);
        let (main, frames) = self.evidence(&case);
        let mut facts = v_flex().w(px(300.)).flex_none().gap_3();
        if !case.precondition.is_empty() {
            facts = facts.child(Self::render_field("Precondition", Label::new(case.precondition.clone()).size(LabelSize::Small)));
        }
        facts = facts.child(Self::render_field(
            "Steps",
            v_flex().gap_0p5().children(case.steps.iter().enumerate().map(|(ix, step)| {
                Label::new(format!("{}. {step}", ix + 1)).size(LabelSize::Small)
            })),
        ));
        facts = facts.child(Self::render_field("Expected", Label::new(case.expected.clone()).size(LabelSize::Small)));
        if !case.observed.is_empty() {
            facts = facts.child(Self::render_field(
                "Observed",
                Label::new(case.observed.clone()).size(LabelSize::Small).color(if case.failed() {
                    Color::Error
                } else {
                    Color::Default
                }),
            ));
        }
        if !case.error.is_empty() {
            facts = facts.child(Self::render_field(
                "Console and network",
                div()
                    .p_1p5()
                    .rounded_sm()
                    .bg(colors.editor_background)
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(Label::new(case.error.clone()).size(LabelSize::XSmall).color(Color::Error).buffer_font(cx)),
            ));
        }
        if !case.telemetry.is_empty() {
            facts = facts.child(Self::render_field(
                "Telemetry",
                Label::new(case.telemetry.clone()).size(LabelSize::Small).color(Color::Muted),
            ));
        }

        let viewer: AnyElement = match main {
            Some(Evidence::Image(path)) => {
                let open = path.clone();
                v_flex()
                    .id("evidence-image")
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .overflow_hidden()
                            .child(img(path.clone()).size_full().object_fit(ObjectFit::Contain)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(path.file_name().and_then(|name| name.to_str()).unwrap_or("").to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("open-evidence", "Open full size")
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::XSmall)
                                    .on_click(move |_, _, cx| cx.open_with_system(&open)),
                            ),
                    )
                    .into_any_element()
            }
            Some(Evidence::Text { path, text }) => v_flex()
                .id("evidence-text")
                .flex_1()
                .min_h_0()
                .gap_1()
                .child(
                    div()
                        .id("evidence-text-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_2()
                        .rounded_md()
                        .bg(colors.editor_background)
                        .border_1()
                        .border_color(colors.border)
                        .child(Label::new(text).size(LabelSize::XSmall).buffer_font(cx)),
                )
                .child(
                    Label::new(path.file_name().and_then(|name| name.to_str()).unwrap_or("").to_string())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(Label::new("This case has no evidence file.").color(Color::Warning))
                .into_any_element(),
        };
        let strip = (!frames.is_empty()).then(|| {
            h_flex().gap_1p5().children(frames.iter().enumerate().map(|(ix, frame)| {
                let picked = self.frame == Some(ix);
                div()
                    .id(("frame", ix))
                    .flex_1()
                    .h(px(56.))
                    .rounded_sm()
                    .overflow_hidden()
                    .border_1()
                    .border_color(if picked { colors.border_focused } else { colors.border })
                    .cursor_pointer()
                    .tooltip(Tooltip::text(format!("Frame {}", ix + 1)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.frame = if this.frame == Some(ix) { None } else { Some(ix) };
                        cx.notify();
                    }))
                    .child(img(frame.clone()).size_full().object_fit(ObjectFit::Cover))
            }))
        });

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                h_flex()
                    .px_4()
                    .py_2p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(Label::new(case.id.clone()).color(Color::Muted))
                    .child(div().flex_1().min_w_0().child(Label::new(case.title.clone()).weight(FontWeight::SEMIBOLD).truncate()))
                    .child(Label::new(status).size(LabelSize::Small).color(color))
                    .child(
                        Button::new("copy-repro", "Copy repro steps")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_repro(cx))),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .gap_4()
                    .p_4()
                    .child(div().id("facts").h_full().overflow_y_scroll().child(facts))
                    .child(v_flex().flex_1().min_w_0().h_full().gap_2().child(viewer).children(strip)),
            )
            .child(self.render_flag_card(&case, cx))
            .into_any_element()
    }
}

impl ManualTestView {
    /// "Flag for the PR": a switch, then your comment and the screenshots
    /// that go with it.
    fn render_flag_card(&self, case: &TestCase, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let warning = cx.theme().status().warning;
        let flagged = case.include;
        let posted = !case.posted_at.is_empty();
        let attached = case.attached();
        let folder = self.folder();
        let hint = match (flagged, posted) {
            (_, true) => format!("Posted on the PR at {}", short_time(&case.posted_at)),
            (true, false) => "In the review draft, with its steps and the screenshots you pick".to_string(),
            (false, false) => "Add a comment and screenshots for the PR author".to_string(),
        };
        let thumbnails = case.images().into_iter().enumerate().map(|(ix, file)| {
            let picked = attached.iter().any(|attached| attached == file);
            let file = file.to_string();
            let name = Path::new(&file).file_name().and_then(|name| name.to_str()).unwrap_or("").to_string();
            div()
                .id(("attach", ix))
                .relative()
                .w(px(96.))
                .h(px(60.))
                .flex_none()
                .rounded_md()
                .overflow_hidden()
                .border_2()
                .border_color(if picked { colors.border_focused } else { colors.border_transparent })
                .cursor_pointer()
                .when(!picked, |thumb| thumb.opacity(0.55))
                .hover(|style| style.opacity(1.))
                .tooltip(Tooltip::text(if picked { format!("{name}\nAttached. Click to remove.") } else { format!("{name}\nClick to attach") }))
                .child(img(folder.join(&file)).size_full().object_fit(ObjectFit::Cover))
                .when(picked, |thumb| {
                    thumb.child(
                        div()
                            .absolute()
                            .top_1()
                            .right_1()
                            .size_4()
                            .rounded_full()
                            .bg(colors.border_focused)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Default)),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_attach(file.clone(), cx)))
        });
        let images = case.images().len();
        v_flex()
            .mx_4()
            .mb_4()
            .p_3()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(if flagged && !posted { warning.opacity(0.6) } else { colors.border_variant })
            .bg(if flagged && !posted { warning.opacity(0.06) } else { colors.surface_background })
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("⚑").color(if flagged { Color::Warning } else { Color::Muted }))
                    .child(Label::new("Add to review draft").weight(FontWeight::SEMIBOLD))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(hint)
                                .size(LabelSize::Small)
                                .color(if posted { Color::Success } else { Color::Muted })
                                .truncate(),
                        ),
                    )
                    .child(
                        Switch::new("flag-case", if flagged { ToggleState::Selected } else { ToggleState::Unselected })
                            .on_click(cx.listener(|this, _: &ToggleState, window, cx| this.toggle_include(window, cx))),
                    ),
            )
            .when(flagged, |card| {
                card.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.editor_background)
                        .child(self.note_editor.clone()),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new(match images {
                                0 => "No screenshots in this case".to_string(),
                                _ => format!("SCREENSHOTS · {} OF {images} ATTACHED", attached.len()),
                            })
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(h_flex().flex_wrap().gap_1p5().children(thumbnails)),
                )
            })
    }
}

impl ManualTestView {
    /// The PR comment as it will be posted: each flagged case with your
    /// comment and screenshots, then Back and Post comment.
    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let folder = self.folder();
        let comment = pr_comment(&self.results, &folder);
        let pr = self.results.target.strip_prefix("pr-").map(|number| format!("PR #{number}")).unwrap_or_else(|| "the PR".to_string());
        let cases: Vec<TestCase> = self.results.cases.iter().filter(|case| case.to_post()).cloned().collect();
        let sections = cases.iter().enumerate().map(|(ix, case)| {
            let (status, color) = Self::status_label(&case.status);
            let note = case.note.trim().to_string();
            let fallback = [("Expected", &case.expected), ("Observed", &case.observed)]
                .into_iter()
                .filter(|(_, text)| !text.is_empty())
                .map(|(label, text)| format!("{label}: {text}"))
                .collect::<Vec<_>>()
                .join("\n");
            let images = case.attached();
            let case_ix = self.results.cases.iter().position(|candidate| candidate.id == case.id).unwrap_or(0);
            v_flex()
                .gap_2()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.surface_background)
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(case.id.clone()).color(Color::Muted))
                        .child(div().flex_1().min_w_0().child(Label::new(case.title.clone()).weight(FontWeight::SEMIBOLD).truncate()))
                        .child(Label::new(status).size(LabelSize::Small).color(color))
                        .child(
                            Button::new(("edit-case", ix), "Edit")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.preview = false;
                                    this.select(case_ix, window, cx);
                                })),
                        ),
                )
                .child(if note.is_empty() {
                    v_flex()
                        .gap_0p5()
                        .child(Label::new(fallback).size(LabelSize::Small))
                        .child(Label::new("No comment from you: the expected and observed results stand in.").size(LabelSize::XSmall).color(Color::Warning))
                        .into_any_element()
                } else {
                    Label::new(note).into_any_element()
                })
                .when(!case.steps.is_empty(), |section| {
                    section.child(
                        Label::new(format!("▸ Steps ({}), expected and observed, folded", case.steps.len()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .when(!images.is_empty(), |section| {
                    section.child(h_flex().flex_wrap().gap_2().children(images.iter().map(|file| {
                        div()
                            .w(px(220.))
                            .h(px(138.))
                            .rounded_md()
                            .overflow_hidden()
                            .border_1()
                            .border_color(colors.border)
                            .child(img(folder.join(file)).size_full().object_fit(ObjectFit::Contain))
                    })))
                })
        });
        let (status, status_color) = match &self.post_state {
            PostState::Idle => (
                format!(
                    "One comment on {pr} with {} screenshot{}, posted with gh under your account.",
                    comment.attachments.len(),
                    if comment.attachments.len() == 1 { "" } else { "s" }
                ),
                Color::Muted,
            ),
            PostState::Posting => ("Uploading the screenshots and posting…".to_string(), Color::Accent),
            PostState::Failed(error) => (error.clone(), Color::Error),
        };
        let posting = matches!(self.post_state, PostState::Posting);
        let body = comment.body.clone();
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                div().id("pr-comment-preview").flex_1().min_h_0().overflow_y_scroll().child(
                    v_flex()
                        .max_w(px(860.))
                        .mx_auto()
                        .p_4()
                        .gap_3()
                        .child(
                            v_flex()
                                .gap_0p5()
                                .child(Label::new(format!("Comment for {pr}")).size(LabelSize::Large).weight(FontWeight::SEMIBOLD))
                                .child(Label::new(comment.body.lines().next().unwrap_or("").replace('`', "")).size(LabelSize::Small).color(Color::Muted)),
                        )
                        .children(sections),
                ),
            )
            .child(
                h_flex()
                    .w_full()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border_variant)
                    .bg(colors.title_bar_background)
                    .child(div().flex_1().min_w_0().child(Label::new(status).size(LabelSize::Small).color(status_color).truncate()))
                    .child(
                        Button::new("copy-comment", "Copy Markdown")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(body.clone()))),
                    )
                    .child(
                        Button::new("preview-back", "Back")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.show_cases(cx))),
                    )
                    .child(
                        Button::new("post-comment", if posting { "Posting…" } else { "Post comment" })
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Send).size(IconSize::XSmall))
                            .disabled(posting || cases.is_empty())
                            .on_click(cx.listener(|this, _, _, cx| this.post(cx))),
                    ),
            )
    }
}

/// `10:02` from an RFC 3339 time, else the text as it is.
fn short_time(time: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(time)
        .map(|time| time.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|_| time.to_string())
}

impl EventEmitter<ManualTestEvent> for ManualTestView {}

impl Focusable for ManualTestView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ManualTestView {
    type Event = ManualTestEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PlayFilled).color(Color::Accent))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Manual test · {}", self.results.target).into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.path.display().to_string().into())
    }

    fn to_item_events(_event: &ManualTestEvent, _f: &mut dyn FnMut(ItemEvent)) {}
}

impl Render for ManualTestView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PlannotatorManualTest")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .child(if self.preview && self.results.cases.iter().any(TestCase::to_post) {
                self.render_preview(cx).into_any_element()
            } else {
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_list(cx))
                    .child(self.render_detail(cx))
                    .into_any_element()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_is_written_back_and_other_fields_stay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.json");
        std::fs::write(
            &path,
            r#"{"target":"pr-1","cases":[{"id":"MT1","status":"fail","include":true,"extra":"kept"},{"id":"MT2","status":"pass"}]}"#,
        )
        .unwrap();
        let mut case = load_results(&path).unwrap().cases[0].clone();
        case.include = false;
        write_case(&path, &case).unwrap();
        let results = load_results(&path).unwrap();
        assert!(!results.cases[0].include);
        assert_eq!(results.counts(), (1, 1, 0));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"extra\": \"kept\""));
    }

    #[test]
    fn note_and_attachments_are_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.json");
        std::fs::write(
            &path,
            r#"{"cases":[{"id":"MT1","status":"pass","evidence":["evidence/MT1-frame-1.png","evidence/MT1.gif","evidence/MT1.txt"]}]}"#,
        )
        .unwrap();
        let mut case = load_results(&path).unwrap().cases[0].clone();
        assert_eq!(case.attached(), ["evidence/MT1.gif"]);
        case.include = true;
        case.note = "The empty state needs a hint.".into();
        case.attach = Some(vec!["evidence/MT1-frame-1.png".into()]);
        write_case(&path, &case).unwrap();
        let case = &load_results(&path).unwrap().cases[0];
        assert_eq!(case.note, "The empty state needs a hint.");
        assert_eq!(case.attached(), ["evidence/MT1-frame-1.png"]);
        assert!(case.to_post());
    }

    #[test]
    fn the_pr_comment_has_each_flagged_case_with_its_images() {
        let results: ManualTestResults = serde_json::from_str(
            r#"{"sha":"abc1234def","instance":"http://localhost:5679","cases":[
                {"id":"MT1","title":"Rename","status":"fail","include":true,"note":"Rename loses the text.",
                 "steps":["Open a skill.","Rename it."],"expected":"New name","observed":"Old name",
                 "evidence":["evidence/MT1.gif"]},
                {"id":"MT2","title":"Sync","status":"pass","include":false,"evidence":["evidence/MT2.png"]},
                {"id":"MT3","title":"Done","status":"fail","include":true,"postedAt":"2026-11-16T10:00:00Z"},
                {"id":"MT4","title":"Empty","status":"blocked","include":true,"expected":"A hint","observed":"Blank"}
            ]}"#,
        )
        .unwrap();
        let comment = pr_comment(&results, Path::new("/r/manual-test"));
        assert_eq!(comment.case_ids, ["MT1", "MT4"]);
        assert_eq!(comment.attachments, [PathBuf::from("/r/manual-test/evidence/MT1.gif")]);
        assert_eq!(
            comment.body,
            "Manual test on a local build of `abc1234`. 2 cases to look at.\n\n\
             ### MT1 · Rename\n\nRename loses the text.\n\n\
             <details><summary>Steps</summary>\n\n1. Open a skill.\n2. Rename it.\n\n\
             Expected: New name<br>Observed: Old name\n\n</details>\n\n\
             ![MT1](</r/manual-test/evidence/MT1.gif>)\n\n\
             ### MT4 · Empty\n\nExpected: A hint<br>Observed: Blank\n"
        );
    }

    #[test]
    fn a_posted_case_is_not_posted_again() {
        let case: TestCase =
            serde_json::from_str(r#"{"id":"MT1","include":true,"postedAt":"2026-11-16T10:00:00Z"}"#).unwrap();
        assert!(!case.to_post());
    }
}
