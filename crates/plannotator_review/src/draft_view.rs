//! The Draft tab: every item of a round in one place. The open round can be
//! ticked and unticked here. A sent round stays readable, with its link.

use crate::{
    panel::ReviewPanel,
    review_round::{ItemResult, Round, RoundState, Source},
};
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, ObjectFit, Render, SharedString,
    WeakEntity, Window, img, px,
};
use ui::{Checkbox, ToggleState, Tooltip, prelude::*};
use workspace::{Item, item::ItemEvent};

pub struct DraftView {
    pr: u64,
    rounds: Vec<Round>,
    /// The round on screen, by number. `None` shows the newest.
    shown: Option<u32>,
    panel: WeakEntity<ReviewPanel>,
    focus_handle: FocusHandle,
}

impl DraftView {
    pub fn new(pr: u64, rounds: Vec<Round>, panel: WeakEntity<ReviewPanel>, cx: &mut Context<Self>) -> Self {
        Self { pr, rounds, shown: None, panel, focus_handle: cx.focus_handle() }
    }

    pub fn set_rounds(&mut self, rounds: Vec<Round>, cx: &mut Context<Self>) {
        if self.rounds != rounds {
            self.rounds = rounds;
            cx.notify();
        }
    }

    fn shown_round(&self) -> Option<&Round> {
        match self.shown {
            Some(number) => self.rounds.iter().find(|round| round.number == number),
            None => self.rounds.last(),
        }
    }

    fn render_item(&self, round: &Round, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let item = &round.items[ix];
        let colors = cx.theme().colors().clone();
        let editable = round.is_open();
        let key = item.key.clone();
        let source = match item.source {
            Source::Agent => "agent",
            Source::Manual => "manual test",
            Source::You => "you",
        };
        let place = match (&item.path, item.line) {
            (Some(path), Some(line)) => format!("{path}:{line}"),
            (Some(path), None) => path.clone(),
            _ => "review body".to_string(),
        };
        let result = item.carried.as_ref().and_then(|carried| carried.result.map(|result| (result, carried.evidence.clone())));
        let result_chip = result.as_ref().map(|(result, _)| {
            let (text, color) = match result {
                ItemResult::Fixed => ("Fixed", Color::Success),
                ItemResult::Partly => ("Partly", Color::Warning),
                ItemResult::NotAddressed => ("Not addressed", Color::Error),
                ItemResult::WillNotFix => ("Will not fix", Color::Muted),
                ItemResult::ResolvedWithoutChange => ("Resolved without change", Color::Muted),
                ItemResult::NoAnswer => ("No answer", Color::Error),
            };
            div()
                .px_1p5()
                .rounded_full()
                .border_1()
                .border_color(colors.border_variant)
                .child(Label::new(text).size(LabelSize::XSmall).color(color))
        });
        let panel = self.panel.clone();
        let include = item.include;
        let checkbox = Checkbox::new(("draft-item", ix), if include { ToggleState::Selected } else { ToggleState::Unselected })
            .disabled(!editable)
            .on_click(move |_, _, cx| {
                let key = key.clone();
                panel.update(cx, |panel, cx| panel.set_item_included(&key, !include, cx)).ok();
            });
        let must_fix = {
            let panel = self.panel.clone();
            let key = item.key.clone();
            let must = item.must_fix;
            Button::new(("must-fix", ix), if must { "Must fix" } else { "Agent decides" })
                .style(if must { ButtonStyle::Tinted(ui::TintColor::Error) } else { ButtonStyle::Subtle })
                .label_size(LabelSize::XSmall)
                .disabled(!editable)
                .tooltip(Tooltip::text("For a coding agent: must fix, or the agent decides and gives a reason"))
                .on_click(move |_, _, cx| {
                    panel.update(cx, |panel, cx| panel.set_item_must_fix(&key, !must, cx)).ok();
                })
        };
        let comment_url = round
            .sent
            .comment_ids
            .get(&item.key)
            .map(|_| round.sent.review_url.clone())
            .filter(|url| !url.is_empty());
        v_flex()
            .w_full()
            .p_3()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(if include { colors.border } else { colors.border_variant })
            .bg(colors.editor_background)
            .when(!include, |card| card.opacity(0.6))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(checkbox)
                    .children(item.severity.as_deref().map(|severity| crate::controls::severity_chip(severity, cx)))
                    .child(Label::new(source).size(LabelSize::XSmall).color(Color::Muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(item.title.clone().unwrap_or(place)).size(LabelSize::Small).buffer_font(cx).truncate()),
                    )
                    .children(result_chip)
                    .when(round.destination == Some(crate::review_round::Destination::Agent), |row| row.child(must_fix))
                    .when(item.orphaned, |row| {
                        row.child(Label::new("source no longer has it").size(LabelSize::XSmall).color(Color::Warning))
                    })
                    .when_some(comment_url, |row, url| {
                        row.child(
                            Button::new(("view-comment", ix), "On GitHub")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    }),
            )
            .child(
                div()
                    .pl_6()
                    .text_sm()
                    .text_color(colors.text)
                    .child(SharedString::from(item.body.trim().to_string())),
            )
            .when_some(result.map(|(_, evidence)| evidence).filter(|evidence| !evidence.is_empty()), |card, evidence| {
                card.child(
                    div()
                        .ml_6()
                        .pl_2()
                        .border_l_2()
                        .border_color(colors.border)
                        .child(Label::new(format!("Verify: {evidence}")).size(LabelSize::Small).color(Color::Muted)),
                )
            })
            .when(!item.attachments.is_empty(), |card| {
                card.child(h_flex().pl_6().gap_2().flex_wrap().children(item.attachments.iter().map(|path| {
                    div()
                        .w(px(160.))
                        .h(px(100.))
                        .rounded_sm()
                        .overflow_hidden()
                        .border_1()
                        .border_color(colors.border_variant)
                        .child(img(path.clone()).size_full().object_fit(ObjectFit::Cover))
                })))
            })
            .into_any_element()
    }
}

impl EventEmitter<ItemEvent> for DraftView {}

impl Focusable for DraftView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for DraftView {
    type Event = ItemEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Send).color(Color::Accent))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("Review draft · #{}", self.pr).into()
    }

    fn to_item_events(event: &ItemEvent, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

impl Render for DraftView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let shown = self.shown_round().map(|round| round.number);
        let round_tabs = self.rounds.iter().map(|round| {
            let number = round.number;
            let state = match round.state {
                RoundState::Draft => "draft",
                RoundState::Pending => "pending",
                RoundState::Sending => "sending",
                RoundState::Sent => "sent",
                RoundState::Failed => "failed",
                RoundState::Partial => "partly sent",
                RoundState::Unknown => "checking",
            };
            Button::new(("round-tab", number as usize), format!("Round {number} · {state}"))
                .style(if Some(number) == shown { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                .label_size(LabelSize::Small)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.shown = Some(number);
                    cx.notify();
                }))
        }).collect::<Vec<_>>();
        let round = self.shown_round().cloned();
        let body = match &round {
            None => v_flex()
                .p_6()
                .child(Label::new("No round yet. Tick findings, flag manual test cases, or add your comments.").color(Color::Muted))
                .into_any_element(),
            Some(round) => {
                let items: Vec<AnyElement> = (0..round.items.len()).map(|ix| self.render_item(round, ix, cx)).collect();
                let summary = round.summary.trim().to_string();
                v_flex()
                    .id("draft-items")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .gap_3()
                    .max_w(px(980.))
                    .when(!round.is_open(), |list| {
                        list.child(
                            Label::new(match round.state {
                                RoundState::Sent => "This round is sent. It stays here as it went out.",
                                RoundState::Pending => "Pending on GitHub. Submit it from the Review panel.",
                                _ => "This round is on its way. Its items cannot change now.",
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new("SUMMARY").size(LabelSize::XSmall).color(Color::Muted))
                            .child(
                                div()
                                    .p_2()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(colors.border_variant)
                                    .text_sm()
                                    .text_color(if summary.is_empty() { colors.text_muted } else { colors.text })
                                    .child(SharedString::from(if summary.is_empty() {
                                        "No summary. Write it in the Review panel.".to_string()
                                    } else {
                                        summary
                                    })),
                            ),
                    )
                    .child(Label::new(format!("ITEMS · {}", round.items.len())).size(LabelSize::XSmall).color(Color::Muted))
                    .children(items)
                    .into_any_element()
            }
        };
        let link = round.as_ref().map(|round| round.sent.review_url.clone()).filter(|url| !url.is_empty());
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .w_full()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .flex_wrap()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .bg(colors.title_bar_background)
                    .child(Label::new(format!("Review draft · PR #{}", self.pr)).weight(FontWeight::SEMIBOLD))
                    .child(div().flex_1())
                    .children(round_tabs)
                    .when_some(link, |row, url| {
                        row.child(
                            Button::new("draft-view-on-github", "View on GitHub")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        )
                    }),
            )
            .child(body)
    }
}
