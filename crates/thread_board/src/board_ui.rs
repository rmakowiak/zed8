//! The pieces the threads sidebar draws: the state line of a row, the
//! counts on a worktree heading and the footer with today's tokens.

use crate::{
    Period, ThreadBoard, ThreadSnapshot, ThreadState, ThreadView, format_tokens, transcript::Spend,
};
use agent_ui::TerminalId;
use chrono::Utc;
use gpui::{
    Anchor, App, DismissEvent, EventEmitter, FocusHandle, Focusable, Hsla, SharedString, Window,
    prelude::*, px, relative,
};
use ui::{
    Button, ButtonLike, ButtonSize, ButtonStyle, Color, Label, LabelCommon as _, LabelSize,
    PopoverMenu, StyledExt as _, ThreadItem, Tooltip, h_flex, prelude::*, v_flex,
};

/// The terminal threads under one worktree heading of the sidebar.
#[derive(Clone)]
pub struct BoardGroup {
    pub name: SharedString,
    pub threads: Vec<BoardThread>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardThread {
    pub terminal_id: TerminalId,
    pub title: SharedString,
    pub notified: bool,
}

fn state_color(state: ThreadState, cx: &App) -> Hsla {
    let status = cx.theme().status();
    match state {
        ThreadState::Waiting => status.warning,
        ThreadState::Running => status.info,
        ThreadState::Stuck => status.error,
        ThreadState::Done => status.success,
    }
}

fn dot(color: Hsla) -> impl IntoElement {
    div().flex_none().size(px(6.)).rounded_full().bg(color)
}

fn view_of(
    terminal_id: TerminalId,
    notified: bool,
    cx: &App,
) -> Option<(ThreadView, ThreadSnapshot)> {
    let board = ThreadBoard::global(cx)?;
    let board = board.read(cx);
    let view = board.view(terminal_id, notified, Utc::now())?;
    let snapshot = board.snapshot(terminal_id)?.clone();
    Some((view, snapshot))
}

/// Adds the state line, the elapsed time and the tokens to a terminal row.
pub fn decorate_row(
    item: ThreadItem,
    terminal_id: TerminalId,
    notified: bool,
    cx: &App,
) -> ThreadItem {
    let Some((view, snapshot)) = view_of(terminal_id, notified, cx) else {
        return item;
    };
    let color = state_color(view.state, cx);
    let text_color = match view.state {
        ThreadState::Done => Color::Muted,
        _ => Color::Custom(color),
    };
    let tokens = snapshot.spend.tokens.total();
    let line = h_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(dot(color))
        .child(
            div().flex_1().min_w_0().child(
                Label::new(view.line.clone())
                    .size(LabelSize::Small)
                    .color(text_color)
                    .truncate(),
            ),
        )
        .when(tokens > 0, |this| {
            this.child(
                Label::new(format_tokens(tokens))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx),
            )
        });
    item.icon(IconName::Terminal)
        .icon_tile()
        .metadata_slot(line)
        .when_some(view.elapsed, |item, elapsed| {
            item.end_slot(
                Label::new(elapsed)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
        .when(view.state == ThreadState::Done, |item| {
            item.title_label_color(Color::Muted)
        })
}

/// "$0.37", "$11.20", "$29".
fn format_price(cost: f64) -> String {
    if cost < 100.0 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.0}")
    }
}

fn section_label(text: impl Into<SharedString>) -> Label {
    Label::new(text).size(LabelSize::XSmall).color(Color::Muted)
}

fn count_pill(count: usize, color: Hsla, label: String, cx: &App) -> impl IntoElement {
    div()
        .id(SharedString::from(label.clone()))
        .flex_none()
        .min_w(px(16.))
        .h(px(16.))
        .px_1()
        .rounded_full()
        .bg(color)
        .flex()
        .items_center()
        .justify_center()
        .child(
            Label::new(count.to_string())
                .size(LabelSize::XSmall)
                .color(Color::Custom(cx.theme().colors().panel_background)),
        )
        .tooltip(Tooltip::text(label))
}

/// The counts on a worktree heading: threads that wait on you, stuck
/// threads, and the tokens of the group. None when no thread has a state.
pub fn heading_badges(threads: &[BoardThread], cx: &App) -> Option<AnyElement> {
    let board = ThreadBoard::global(cx)?;
    let board = board.read(cx);
    let now = Utc::now();
    let (mut waiting, mut stuck) = (0, 0);
    for thread in threads {
        match board
            .view(thread.terminal_id, thread.notified, now)
            .map(|view| view.state)
        {
            Some(ThreadState::Waiting) => waiting += 1,
            Some(ThreadState::Stuck) => stuck += 1,
            _ => {}
        }
    }
    if waiting == 0 && stuck == 0 {
        return None;
    }
    let status = cx.theme().status();
    Some(
        h_flex()
            .flex_none()
            .gap_1()
            .when(waiting > 0, |this| {
                this.child(count_pill(
                    waiting,
                    status.warning,
                    format!("{waiting} waiting on you"),
                    cx,
                ))
            })
            .when(stuck > 0, |this| {
                this.child(count_pill(
                    stuck,
                    status.error,
                    format!("{stuck} stuck"),
                    cx,
                ))
            })
            .into_any_element(),
    )
}

/// The footer: how many threads run, wait and are stuck, and a button with
/// today's tokens that opens the breakdown.
pub fn footer(groups: Vec<BoardGroup>, cx: &App) -> Option<AnyElement> {
    let board = ThreadBoard::global(cx)?;
    let read = board.read(cx);
    let now = Utc::now();
    let (mut running, mut waiting, mut stuck) = (0, 0, 0);
    for thread in groups.iter().flat_map(|group| &group.threads) {
        match read
            .view(thread.terminal_id, thread.notified, now)
            .map(|view| view.state)
        {
            Some(ThreadState::Running) => running += 1,
            Some(ThreadState::Waiting) => waiting += 1,
            Some(ThreadState::Stuck) => stuck += 1,
            _ => {}
        }
    }
    let today: u64 = read
        .threads
        .keys()
        .map(|id| read.period_spend(*id, Period::Today).tokens.total())
        .sum();
    let week_price: f64 = read
        .threads
        .keys()
        .map(|id| read.period_spend(*id, Period::Week).cost)
        .sum();
    let status = cx.theme().status();
    let count = |count: usize, color: Hsla, label: &'static str| {
        h_flex()
            .id(label)
            .gap_1()
            .child(dot(color))
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .tooltip(Tooltip::text(label))
    };
    let trigger = ButtonLike::new("thread-board-today")
        .child(
            h_flex()
                .gap_1()
                .px_1()
                .child(
                    Label::new("Today")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(format_tokens(today))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx),
                )
                .child(
                    Label::new("· 7 days")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(format!("≈ {}", format_price(week_price)))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx),
                ),
        )
        .tooltip(Tooltip::text(
            "Tokens today, and the price of the last 7 days at API prices",
        ));
    Some(
        h_flex()
            .gap_2()
            .when(running > 0, |this| {
                this.child(count(running, status.info, "Running"))
            })
            .when(waiting > 0, |this| {
                this.child(count(waiting, status.warning, "Waiting on you"))
            })
            .when(stuck > 0, |this| {
                this.child(count(stuck, status.error, "Stuck"))
            })
            .child(
                PopoverMenu::new("thread-board-tokens")
                    .trigger(trigger)
                    .anchor(Anchor::BottomRight)
                    .menu(move |window, cx| {
                        let groups = groups.clone();
                        Some(cx.new(|cx| TokensPopover::new(groups, window, cx)))
                    }),
            )
            .into_any_element(),
    )
}

pub struct TokensPopover {
    groups: Vec<BoardGroup>,
    focus_handle: FocusHandle,
}

impl TokensPopover {
    fn new(groups: Vec<BoardGroup>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        if let Some(board) = ThreadBoard::global(cx) {
            cx.observe(&board, |_, _, cx| cx.notify()).detach();
        }
        Self {
            groups,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl EventEmitter<DismissEvent> for TokensPopover {}

impl Focusable for TokensPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TokensPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(board) = ThreadBoard::global(cx) else {
            return div().into_any_element();
        };
        let read = board.read(cx);
        let period = read.period;
        let spend_of = |id: TerminalId| read.period_spend(id, period);

        let mut total = Spend::default();
        let mut by_group: Vec<(SharedString, u64)> = Vec::new();
        let mut by_thread: Vec<(SharedString, u64)> = Vec::new();
        for group in &self.groups {
            let mut group_tokens = 0;
            for thread in &group.threads {
                let spend = spend_of(thread.terminal_id);
                total.add(&spend);
                group_tokens += spend.tokens.total();
                if spend.tokens.total() > 0 {
                    let title = thread
                        .title
                        .trim_start_matches(|c: char| !c.is_alphanumeric());
                    by_thread.push((SharedString::from(title.to_string()), spend.tokens.total()));
                }
            }
            if group_tokens > 0 {
                by_group.push((group.name.clone(), group_tokens));
            }
        }
        by_group.sort_by_key(|group| std::cmp::Reverse(group.1));
        by_thread.sort_by_key(|thread| std::cmp::Reverse(thread.1));
        by_thread.truncate(3);
        let max_group = by_group
            .first()
            .map(|(_, tokens)| *tokens)
            .unwrap_or(1)
            .max(1);

        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let tokens = total.tokens;
        let all = tokens.total().max(1) as f32;
        let kinds = [
            ("Cache read", tokens.cache_read, status.info.opacity(0.45)),
            ("Cache write", tokens.cache_write, status.info),
            ("Output", tokens.output, status.warning),
            ("Input", tokens.input, colors.text_muted),
        ];

        let period_button = |id: &'static str, label: &'static str, value: Period| {
            Button::new(id, label)
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .label_size(LabelSize::Small)
                .toggle_state(period == value)
                .on_click({
                    let board = board.clone();
                    move |_, _, cx| {
                        board.update(cx, |board, cx| {
                            board.period = value;
                            cx.notify();
                        })
                    }
                })
        };

        v_flex()
            .w(px(284.))
            .p_3()
            .gap_3()
            .elevation_2(cx)
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .gap_0p5()
                    .p_0p5()
                    .rounded_sm()
                    .bg(colors.panel_background)
                    .self_start()
                    .child(period_button("thread-board-period-today", "Today", Period::Today))
                    .child(period_button("thread-board-period-week", "7 days", Period::Week)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(
                        Label::new(format_tokens(tokens.total()))
                            .size(LabelSize::Large)
                            .buffer_font(cx),
                    )
                    .child(
                        Label::new(if total.cost > 0.0 {
                            format!("tokens · ≈ {} at API prices", format_price(total.cost))
                        } else {
                            "tokens".to_string()
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
            .when(!by_group.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1p5()
                        .child(section_label("BY WORKTREE"))
                        .children(by_group.into_iter().map(|(name, group_tokens)| {
                            v_flex()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .child(Label::new(name).size(LabelSize::Small).truncate()),
                                        )
                                        .child(
                                            Label::new(format_tokens(group_tokens))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .buffer_font(cx),
                                        ),
                                )
                                .child(
                                    div()
                                        .h(px(4.))
                                        .w_full()
                                        .rounded_full()
                                        .bg(colors.panel_background)
                                        .child(
                                            div()
                                                .h_full()
                                                .rounded_full()
                                                .bg(status.info)
                                                .w(relative(group_tokens as f32 / max_group as f32)),
                                        ),
                                )
                        })),
                )
            })
            .when(tokens.total() > 0, |this| {
                this.child(
                    v_flex()
                        .gap_1p5()
                        .child(section_label("KIND OF TOKEN"))
                        .child(
                            h_flex()
                                .h(px(6.))
                                .w_full()
                                .rounded_full()
                                .overflow_hidden()
                                .bg(colors.panel_background)
                                .children(kinds.iter().filter(|(_, count, _)| *count > 0).map(
                                    |(_, count, color)| {
                                        div().h_full().bg(*color).w(relative(*count as f32 / all))
                                    },
                                )),
                        )
                        .child(h_flex().flex_wrap().gap_x_3().gap_y_1().children(kinds.iter().map(
                            |(label, count, color)| {
                                h_flex()
                                    .gap_1()
                                    .child(div().size(px(8.)).rounded_xs().bg(*color))
                                    .child(
                                        Label::new(format!(
                                            "{label} {:.0}%",
                                            *count as f32 * 100.0 / all
                                        ))
                                        .size(LabelSize::XSmall),
                                    )
                            },
                        ))),
                )
            })
            .when(!by_thread.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(section_label("TOP THREADS"))
                        .children(by_thread.into_iter().map(|(title, thread_tokens)| {
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(Label::new(title).size(LabelSize::Small).truncate()),
                                )
                                .child(
                                    Label::new(format_tokens(thread_tokens))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .buffer_font(cx),
                                )
                        })),
                )
            })
            .child(
                Label::new("From the transcripts in ~/.claude/projects. Subagent tokens count toward their thread.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
}
