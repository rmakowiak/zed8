//! The Manual test results tab: every test case of the `manual-test` skill
//! with its proof. The skill writes `results.json` into the `manual-test`
//! folder of the review. The tab reads it, shows the evidence files, and
//! writes back the "Include in the PR review" box of each case.

use gpui::{
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, FontWeight, ObjectFit, Render, Window,
    img, px,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use ui::{Checkbox, ToggleState, Tooltip, prelude::*};
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
    #[serde(default)]
    pub include: bool,
}

impl TestCase {
    pub fn failed(&self) -> bool {
        self.status != "pass"
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

/// Sets `include` of one case in the file, and keeps every other field as
/// the skill wrote it.
fn write_include(path: &Path, case_id: &str, include: bool) -> anyhow::Result<()> {
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let cases = value
        .get_mut("cases")
        .and_then(|cases| cases.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("no cases in {}", path.display()))?;
    for case in cases {
        if case.get("id").and_then(|id| id.as_str()) == Some(case_id) {
            case["include"] = serde_json::Value::Bool(include);
        }
    }
    std::fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
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

pub struct ManualTestView {
    path: PathBuf,
    results: ManualTestResults,
    selected: usize,
    /// The frame picked under the GIF, by its index in the frame list.
    frame: Option<usize>,
    focus_handle: FocusHandle,
}

impl ManualTestView {
    pub fn new(path: PathBuf, results: ManualTestResults, cx: &mut Context<Self>) -> Self {
        let selected = results.cases.iter().position(TestCase::failed).unwrap_or(0);
        Self {
            path,
            results,
            selected,
            frame: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_results(&mut self, results: ManualTestResults, cx: &mut Context<Self>) {
        if results != self.results {
            self.selected = self.selected.min(results.cases.len().saturating_sub(1));
            self.results = results;
            cx.notify();
        }
    }

    fn folder(&self) -> PathBuf {
        self.path.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = ix;
        self.frame = None;
        cx.notify();
    }

    fn toggle_include(&mut self, cx: &mut Context<Self>) {
        let Some(case) = self.results.cases.get_mut(self.selected) else {
            return;
        };
        case.include = !case.include;
        write_include(&self.path, &case.id, case.include).log_err();
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
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .on_click(move |_, _, cx| cx.reveal_path(&folder)),
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
                .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
                .child(div().w(px(40.)).child(Label::new(case.id.clone()).size(LabelSize::Small).color(Color::Muted)))
                .child(div().flex_1().min_w_0().child(Label::new(case.title.clone()).size(LabelSize::Small).truncate()))
                .child(Label::new(kind).size(LabelSize::XSmall).color(Color::Muted))
                .child(Label::new(status).size(LabelSize::XSmall).color(color))
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

        let include = case.include;
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
            .when(case.failed(), |detail| {
                detail.child(
                    h_flex()
                        .px_4()
                        .py_2()
                        .gap_2()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .child(
                            Checkbox::new("include-case", if include { ToggleState::Selected } else { ToggleState::Unselected })
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_include(cx))),
                        )
                        .child(
                            Label::new(
                                "Include in the PR review. It goes into the review body under “Manual test”, never as an inline comment.",
                            )
                            .size(LabelSize::Small),
                        ),
                )
            })
            .into_any_element()
    }
}

impl EventEmitter<()> for ManualTestView {}

impl Focusable for ManualTestView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ManualTestView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PlayFilled).color(Color::Accent))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Manual test · {}", self.results.target).into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.path.display().to_string().into())
    }

    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
}

impl Render for ManualTestView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PlannotatorManualTest")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_toolbar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_list(cx))
                    .child(self.render_detail(cx)),
            )
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
        write_include(&path, "MT1", false).unwrap();
        let results = load_results(&path).unwrap();
        assert!(!results.cases[0].include);
        assert_eq!(results.counts(), (1, 1, 0));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"extra\": \"kept\""));
    }
}
