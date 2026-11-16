//! Send feedback and Approve, drawn the same in every review tab (a bar at
//! the top) and in the action cards of the Review panel (a row in the card).

use gpui::{AnyElement, App, ClickEvent, ElementId, IntoElement, SharedString, Window};
use ui::{Tooltip, prelude::*};

type Handler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct ReviewControls {
    id: SharedString,
    status: Option<(SharedString, Color)>,
    summary: SharedString,
    send: Option<Handler>,
    approve: Option<Handler>,
    send_tooltip: Option<SharedString>,
    approve_tooltip: Option<SharedString>,
    in_card: bool,
}

impl ReviewControls {
    /// `id` keeps the two buttons apart when a card shows more than one bar.
    pub fn new(id: impl Into<SharedString>, summary: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            status: None,
            summary: summary.into(),
            send: None,
            approve: None,
            send_tooltip: None,
            approve_tooltip: None,
            in_card: false,
        }
    }

    /// The row for an action card in the Review panel, not the bar of a tab.
    pub fn in_card(mut self) -> Self {
        self.in_card = true;
        self
    }

    pub fn status(mut self, text: impl Into<SharedString>, color: Color) -> Self {
        self.status = Some((text.into(), color));
        self
    }

    /// Without a handler, the button shows disabled.
    pub fn on_send(mut self, handler: Option<impl Fn(&ClickEvent, &mut Window, &mut App) + 'static>) -> Self {
        self.send = handler.map(|handler| Box::new(handler) as Handler);
        self
    }

    pub fn on_approve(mut self, handler: Option<impl Fn(&ClickEvent, &mut Window, &mut App) + 'static>) -> Self {
        self.approve = handler.map(|handler| Box::new(handler) as Handler);
        self
    }

    pub fn send_tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.send_tooltip = Some(text.into());
        self
    }

    pub fn approve_tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.approve_tooltip = Some(text.into());
        self
    }
}

impl RenderOnce for ReviewControls {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let button_id = |name: &str| ElementId::Name(format!("{}-{name}", self.id).into());
        let send = Button::new(button_id("send"), "Send feedback")
            .style(ButtonStyle::Outlined)
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::Send).size(IconSize::XSmall))
            .disabled(self.send.is_none())
            .when_some(self.send_tooltip, |button, text| button.tooltip(Tooltip::text(text)))
            .when_some(self.send, |button, handler| button.on_click(move |event, window, cx| handler(event, window, cx)));
        let approve = Button::new(button_id("approve"), "Approve")
            .style(ButtonStyle::Filled)
            .label_size(LabelSize::Small)
            .start_icon(Icon::new(IconName::Check).size(IconSize::XSmall).color(Color::Success))
            .disabled(self.approve.is_none())
            .when_some(self.approve_tooltip, |button, text| button.tooltip(Tooltip::text(text)))
            .when_some(self.approve, |button, handler| {
                button.on_click(move |event, window, cx| handler(event, window, cx))
            });
        let status: Option<AnyElement> = self.status.map(|(text, color)| {
            h_flex()
                .flex_none()
                .gap_1()
                .child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(color))
                .child(Label::new(text).size(LabelSize::Small).color(color))
                .into_any_element()
        });
        h_flex()
            .w_full()
            .gap_2()
            .when(!self.in_card, |bar| {
                bar.px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .bg(colors.title_bar_background)
            })
            .children(status)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(self.summary).size(LabelSize::Small).color(Color::Muted).truncate()),
            )
            .child(send)
            .child(approve)
    }
}

/// "No comments yet", "1 comment not sent", "3 comments not sent".
pub fn unsent_label(count: usize) -> String {
    match count {
        0 => "No comments yet".to_string(),
        1 => "1 comment not sent".to_string(),
        count => format!("{count} comments not sent"),
    }
}

/// A small chip with a finding's severity, colored from Critical (red) to Nit (muted).
pub fn severity_chip(severity: &str, cx: &App) -> impl IntoElement {
    let color = match crate::findings::severity_rank(severity) {
        5 | 4 => Color::Error,
        3 => Color::Warning,
        2 => Color::Accent,
        _ => Color::Muted,
    };
    let mut label = severity.to_string();
    if let Some(first) = label.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    let rank = crate::findings::severity_rank(severity);
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(color.color(cx).opacity(0.5))
        .when(rank == 5, |chip| chip.bg(color.color(cx).opacity(0.15)))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .weight(gpui::FontWeight::SEMIBOLD)
                .color(color),
        )
}
