//! The review draft in the Review panel: the composer pinned at the bottom,
//! the Verify card, the round line and the tool buttons of the header. See
//! `zed8/REVIEW_FLOW.md`.

use super::*;
use crate::{
    chapter_view::DraftChange,
    draft_view::DraftView,
    review_round::{
        self, Block, Decision, Destination, GuardContext, ItemResult, PrNow, Round, RoundState, Source,
        VerifyRecord, VerifyResults, VerifyRun, VerifyState,
    },
    review_send,
};
use editor::Editor;
use ui::{ButtonLike, PopoverMenuHandle, SplitButton};

/// The tab of the Review panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DockTab {
    #[default]
    Review,
    Tools,
}

/// The summary editor of the open round.
pub(crate) struct SummaryEditor {
    round: u32,
    editor: Entity<Editor>,
    _subscription: Subscription,
}

/// What the panel keeps for the review draft.
#[derive(Default)]
pub(crate) struct DraftUi {
    pub tab: DockTab,
    pub sending: Option<Task<()>>,
    pub summary: Option<SummaryEditor>,
    pub summary_save: Option<Task<()>>,
    pub decision_menu: PopoverMenuHandle<ContextMenu>,
    /// Draft events of every chapter and findings tab, by tab.
    pub tab_subscriptions: HashMap<EntityId, Subscription>,
    /// Verify started by itself for this round, so it does not start twice.
    pub auto_verified: HashSet<u32>,
}

fn local_time(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|time| time.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

impl ReviewPanel {
    /// The round that takes items: the newest one, unless it was sent.
    pub(crate) fn open_round(&self) -> Option<&Round> {
        self.state.rounds.last().filter(|round| round.state != RoundState::Sent)
    }

    fn last_sent(&self) -> Option<&Round> {
        self.state.rounds.iter().rev().find(|round| round.state == RoundState::Sent)
    }

    fn own_pr(&self) -> bool {
        self.state
            .pr
            .as_ref()
            .is_some_and(|pr| !self.state.me.is_empty() && pr.author_login().eq_ignore_ascii_case(&self.state.me))
    }

    fn destination_of(&self, round: Option<&Round>) -> Destination {
        let author = self.state.pr.as_ref().map(|pr| pr.author_login()).unwrap_or_default();
        review_round::destination(author, &self.state.me, round.and_then(|round| round.destination))
    }

    fn toast(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.workspace
            .update(cx, |workspace, cx| crate::show_toast(workspace, message, cx))
            .log_err();
    }

    /// Changes the open round, or starts the next one, and saves it.
    fn edit_draft(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Round)) -> bool {
        let (Some(pr), Some(dir)) = (self.state.pr.clone(), self.state.review_dir.clone()) else {
            self.toast("The review draft needs a PR.", cx);
            return false;
        };
        match self.state.rounds.last() {
            Some(round) if round.is_open() => {}
            Some(round) if round.state != RoundState::Sent => {
                let message = format!(
                    "Round {} is {}. Finish it in the Review panel first.",
                    round.number,
                    state_word(round.state)
                );
                self.toast(message, cx);
                return false;
            }
            last => {
                let number = last.map_or(1, |round| round.number + 1);
                self.state.rounds.push(Round::new(pr.number, number, pr.head_ref_oid));
            }
        }
        let Some(round) = self.state.rounds.last_mut() else {
            return false;
        };
        change(round);
        if let Err(error) = review_send::save_round(&dir, round) {
            self.toast(format!("Cannot save the review draft: {error:#}"), cx);
        }
        self.push_draft_keys(cx);
        cx.notify();
        true
    }

    pub(crate) fn apply_draft_change(&mut self, change: &DraftChange, cx: &mut Context<Self>) {
        match change {
            DraftChange::Add(items) => {
                let count = items.len();
                let added = self.edit_draft(cx, |round| {
                    for item in items {
                        round.add(item.clone());
                    }
                });
                if added && items.iter().any(|item| item.source == Source::You) {
                    self.toast(
                        match count {
                            1 => "Added 1 comment to the review draft.".to_string(),
                            count => format!("Added {count} comments to the review draft."),
                        },
                        cx,
                    );
                }
            }
            DraftChange::Remove(key) => {
                self.edit_draft(cx, |round| round.remove(key));
            }
        }
    }

    /// The keys of the ticked items, for the checkboxes of every tab.
    pub(crate) fn push_draft_keys(&self, cx: &mut Context<Self>) {
        let keys: std::collections::BTreeSet<String> = self
            .open_round()
            .map(|round| round.included().map(|item| item.key.clone()).collect())
            .unwrap_or_default();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let views: Vec<Entity<ChapterDiffView>> = workspace.read(cx).items_of_type::<ChapterDiffView>(cx).collect();
        for view in views {
            view.update(cx, |view, cx| view.set_draft_keys(keys.clone(), cx));
        }
        let draft_views: Vec<Entity<DraftView>> = workspace.read(cx).items_of_type::<DraftView>(cx).collect();
        let rounds = self.state.rounds.clone();
        for view in draft_views {
            view.update(cx, |view, cx| view.set_rounds(rounds.clone(), cx));
        }
    }

    /// Follows the draft events of every chapter and findings tab.
    pub(crate) fn watch_draft_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let views: Vec<Entity<ChapterDiffView>> = workspace.read(cx).items_of_type::<ChapterDiffView>(cx).collect();
        let open: HashSet<EntityId> = views.iter().map(|view| view.entity_id()).collect();
        self.draft.tab_subscriptions.retain(|id, _| open.contains(id));
        let mut new_tab = false;
        for view in views {
            if self.draft.tab_subscriptions.contains_key(&view.entity_id()) {
                continue;
            }
            new_tab = true;
            let subscription = cx.subscribe_in(&view, window, |this, _, event: &ChapterNavEvent, _, cx| {
                if let ChapterNavEvent::Draft(change) = event {
                    this.apply_draft_change(change, cx);
                }
            });
            self.draft.tab_subscriptions.insert(view.entity_id(), subscription);
        }
        if new_tab {
            self.push_draft_keys(cx);
        }
    }

    fn summary_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<Editor>> {
        let round = self.open_round()?.clone();
        if let Some(summary) = &self.draft.summary
            && summary.round == round.number
        {
            return Some(summary.editor.clone());
        }
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_placeholder_text("Summary for the top of the review", window, cx);
            editor.set_text(round.summary.clone(), window, cx);
            editor
        });
        let subscription = cx.subscribe(&editor, |this, editor, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                let text = editor.read(cx).text(cx);
                // Saves after you stop typing, not on each key.
                this.draft.summary_save = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(400)).await;
                    this.update(cx, |this, cx| {
                        if this.open_round().is_some_and(|round| round.summary != text) {
                            this.edit_draft(cx, |round| round.summary = text);
                        }
                    })
                    .log_err();
                }));
            }
        });
        self.draft.summary = Some(SummaryEditor { round: round.number, editor: editor.clone(), _subscription: subscription });
        Some(editor)
    }

    fn set_decision(&mut self, decision: Decision, cx: &mut Context<Self>) {
        self.edit_draft(cx, |round| round.decision = decision);
    }

    fn set_destination(&mut self, destination: Destination, cx: &mut Context<Self>) {
        self.edit_draft(cx, |round| {
            round.destination = Some(destination);
            if destination == Destination::Agent {
                round.decision = Decision::Comment;
            }
        });
    }

    fn replace_round(&mut self, round: Round, cx: &mut Context<Self>) {
        match self.state.rounds.iter_mut().find(|existing| existing.number == round.number) {
            Some(existing) => *existing = round,
            None => self.state.rounds.push(round),
        }
        self.push_draft_keys(cx);
        cx.notify();
    }

    /// Sends the open round to its destination, or saves it as pending.
    pub(crate) fn send_draft(&mut self, pending: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.draft.sending.is_some() {
            return;
        }
        let (Some(mut round), Some(pr), Some(dir), Some(root)) = (
            self.open_round().cloned(),
            self.state.pr.clone(),
            self.state.review_dir.clone(),
            self.state.roots.first().cloned(),
        ) else {
            return;
        };
        if let Some(summary) = &self.draft.summary
            && summary.round == round.number
        {
            round.summary = summary.editor.read(cx).text(cx);
        }
        let destination = self.destination_of(Some(&round));
        let context = GuardContext {
            pr_open: pr.is_open(),
            destination,
            gh_attaches: review_send::gh_attaches(),
            own_pr: self.own_pr(),
        };
        if let Err(block) = review_round::guard(&round, &context) {
            self.show_block(block, cx);
            return;
        }
        if destination == Destination::Agent {
            self.send_to_agent(round, &dir, window, cx);
            return;
        }
        round.state = RoundState::Sending;
        self.replace_round(round.clone(), cx);
        let head = pr.head_ref_oid.clone();
        let base = pr.base_ref_name;
        self.draft.sending = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let save_dir = dir.clone();
                    let save = move |round: &Round| {
                        review_send::save_round(&save_dir, round).log_err();
                    };
                    review_send::send_to_github(&root, &dir, &head, &base, round, pending, &save)
                })
                .await;
            this.update(cx, |this, cx| {
                this.draft.sending = None;
                let message = match result.state {
                    RoundState::Sent => format!(
                        "Round {} sent to GitHub: {}.",
                        result.number,
                        result.sent.decision.unwrap_or(result.decision).label()
                    ),
                    RoundState::Pending => "Saved as a pending review. Only you see it on GitHub.".to_string(),
                    RoundState::Partial => format!("Partly sent: {}", result.error),
                    _ => format!("Send failed: {}", result.error),
                };
                // The PR info is old now: GitHub dropped your review request.
                this.state.pr_checked_at = None;
                this.replace_round(result, cx);
                this.toast(message, cx);
            })
            .log_err();
        }));
        cx.notify();
    }

    fn show_block(&mut self, block: Block, cx: &mut Context<Self>) {
        let message = block.message().to_string();
        self.edit_draft(cx, |round| round.error = message);
    }

    fn send_to_agent(&mut self, mut round: Round, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let path = review_round::round_path(dir, round.number);
        round.state = RoundState::Sending;
        review_send::save_round(dir, &round).log_err();
        let started = self.start_round_action(ReviewAction::Fix, &path, true, window, cx);
        if started {
            round.state = RoundState::Sent;
            round.error.clear();
            round.sent.at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            round.sent.thread = ReviewAction::Fix.title().to_string();
        } else {
            round.state = RoundState::Failed;
            round.error = "Zed cannot open a Claude thread for the fix.".to_string();
        }
        review_send::save_round(dir, &round).log_err();
        self.replace_round(round, cx);
    }

    fn submit_pending(&mut self, cx: &mut Context<Self>) {
        let (Some(round), Some(dir), Some(root)) =
            (self.open_round().cloned(), self.state.review_dir.clone(), self.state.roots.first().cloned())
        else {
            return;
        };
        self.draft.sending = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let save_dir = dir.clone();
                    let save = move |round: &Round| {
                        review_send::save_round(&save_dir, round).log_err();
                    };
                    review_send::submit_pending(&root, &dir, round, &save)
                })
                .await;
            this.update(cx, |this, cx| {
                this.draft.sending = None;
                this.state.pr_checked_at = None;
                this.replace_round(result, cx);
            })
            .log_err();
        }));
    }

    /// Starts Verify or Fix in its own thread, with the round file.
    pub(crate) fn start_round_action(
        &mut self,
        action: ReviewAction,
        round_path: &Path,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(root) = self.state.roots.first().cloned() else {
            return false;
        };
        let progress_file = actions::progress_path(&crate::paths::actions_dir(), &root, action);
        if let Err(error) = std::fs::remove_file(&progress_file)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("plannotator: cannot reset {}: {error}", progress_file.display());
        }
        if let Some(parent) = progress_file.parent() {
            std::fs::create_dir_all(parent).log_err();
        }
        let prompt = format!("{} {}", action.command(&progress_file), round_path.display());
        let command = format!("claude {}", shell_quote(&prompt));
        self.start_thread_with_focus(action, command, focus, window, cx)
    }

    fn thread_running(&self, action: ReviewAction, cx: &App) -> bool {
        let terminals = self.claude_terminals(cx);
        let terminal = self.action_terminal(action, &terminals);
        matches!(
            self.action_status(action, terminal.as_ref()),
            ActionStatus::Working | ActionStatus::Idle | ActionStatus::Waiting
        )
    }

    fn verify_results(&self, round: &Round) -> Option<VerifyResults> {
        let dir = self.state.review_dir.as_ref()?;
        let path = dir.join("rounds").join(format!("round-{}.verify.json", round.number));
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    fn pr_now(&self, round: &Round, cx: &App) -> PrNow {
        let Some(pr) = self.state.pr.as_ref() else {
            return PrNow::default();
        };
        // Right after a send, the PR info may still list your old review request.
        let fetched_after_send = match (
            self.state.pr_fetched_at,
            chrono::DateTime::parse_from_rfc3339(&round.sent.at),
        ) {
            (Some(fetched), Ok(sent)) => fetched > sent.with_timezone(&chrono::Utc) + chrono::Duration::seconds(5),
            _ => false,
        };
        let commits_since = self
            .state
            .since_sent
            .as_ref()
            .filter(|(sha, _, _)| *sha == round.sha)
            .map_or(0, |(_, count, _)| *count);
        PrNow {
            open: pr.is_open(),
            head: pr.head_ref_oid.clone(),
            asked_again: fetched_after_send && pr.asks(&self.state.me),
            commits_since,
            answers: self.state.answers,
            fix_thread_running: self.thread_running(ReviewAction::Fix, cx),
            verify_thread_running: self.thread_running(ReviewAction::Verify, cx),
        }
    }

    fn verify_card_state(&self, cx: &App) -> Option<(Round, VerifyState)> {
        let round = self.last_sent()?.clone();
        // A newer round that left Zed8 replaces the one before.
        if self.state.rounds.last().is_some_and(|last| last.number > round.number && last.state != RoundState::Draft) {
            return None;
        }
        let destination = self.destination_of(Some(&round));
        let state = review_round::verify_state(&round, destination, &self.pr_now(&round, cx));
        (state != VerifyState::None).then_some((round, state))
    }

    /// Starts Verify for the newest sent round.
    pub(crate) fn start_verify(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(mut round), Some(dir), Some(pr)) =
            (self.last_sent().cloned(), self.state.review_dir.clone(), self.state.pr.clone())
        else {
            return;
        };
        let path = review_round::round_path(&dir, round.number);
        if !self.start_round_action(ReviewAction::Verify, &path, focus, window, cx) {
            return;
        }
        round.verify = Some(VerifyRecord {
            run: VerifyRun::Running,
            head: pr.head_ref_oid,
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            thread: ReviewAction::Verify.title().to_string(),
            error: String::new(),
        });
        review_send::save_round(&dir, &round).log_err();
        self.replace_round(round, cx);
    }

    /// Runs on each poll: starts Verify on a clear signal, and turns a
    /// finished Verify into the next round.
    pub(crate) fn follow_verify(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((round, state)) = self.verify_card_state(cx) else {
            return;
        };
        let done = self.state.progress.get(&ReviewAction::Verify).is_some_and(|progress| progress.done);
        if matches!(round.verify.as_ref().map(|verify| verify.run), Some(VerifyRun::Running))
            && done
            && let Some(results) = self.verify_results(&round)
        {
            self.finish_verify(round, results, cx);
            return;
        }
        if state == VerifyState::Start && self.draft.auto_verified.insert(round.number) {
            self.start_verify(false, window, cx);
        }
    }

    fn finish_verify(&mut self, mut round: Round, results: VerifyResults, cx: &mut Context<Self>) {
        let Some(dir) = self.state.review_dir.clone() else {
            return;
        };
        if let Some(verify) = round.verify.as_mut() {
            verify.run = VerifyRun::Done;
            if !results.head.is_empty() {
                verify.head = results.head.clone();
            }
        }
        review_send::save_round(&dir, &round).log_err();
        let next = review_round::next_round(&round, &results);
        let has_next = self.state.rounds.iter().any(|existing| existing.number == next.number);
        self.replace_round(round, cx);
        if !has_next {
            review_send::save_round(&dir, &next).log_err();
            let open = next.included().count();
            self.replace_round(next, cx);
            self.toast(
                match open {
                    0 => "Verify is done. Nothing is open: Approve is ready in the draft.".to_string(),
                    open => format!("Verify is done. {open} open items are in the next round's draft."),
                },
                cx,
            );
        }
    }

    /// A round left in Sending by an earlier run of Zed becomes Unknown, so
    /// the next PR refresh asks GitHub what happened.
    pub(crate) fn recover_sending(&mut self, cx: &mut Context<Self>) {
        if self.draft.sending.is_some() {
            return;
        }
        let Some(dir) = self.state.review_dir.clone() else {
            return;
        };
        let mut changed = false;
        for round in self.state.rounds.iter_mut().filter(|round| round.state == RoundState::Sending) {
            if round.destination == Some(Destination::Agent) {
                continue;
            }
            round.state = review_round::next(round.state, &review_round::SendEvent::Restarted);
            review_send::save_round(&dir, round).log_err();
            changed = true;
        }
        if changed {
            self.state.pr_checked_at = None;
            cx.notify();
        }
    }

    pub(crate) fn render_round_line(&self) -> Option<AnyElement> {
        let last = self.state.rounds.last()?;
        let sent = self.last_sent();
        let commits = self.state.since_sent.as_ref().map_or(0, |(_, count, _)| *count);
        let (text, color) = match (last.state, sent) {
            (RoundState::Draft, Some(sent)) | (RoundState::Failed, Some(sent)) => (
                format!(
                    "Round {} sent {} · {} · round {} in draft",
                    sent.number,
                    local_time(&sent.sent.at),
                    sent.sent.decision.map_or("to the agent", Decision::label),
                    last.number
                ),
                Color::Muted,
            ),
            (RoundState::Sent, _) => (
                format!(
                    "Round {} sent {} · {}{}",
                    last.number,
                    local_time(&last.sent.at),
                    last.sent.decision.map_or("to the agent", Decision::label),
                    match commits {
                        0 => String::new(),
                        1 => " · 1 new commit".to_string(),
                        count => format!(" · {count} new commits"),
                    }
                ),
                if commits > 0 { Color::Warning } else { Color::Muted },
            ),
            (RoundState::Pending, _) => (format!("Round {} · pending on GitHub, only you see it", last.number), Color::Accent),
            (RoundState::Sending, _) => (format!("Round {} · sending", last.number), Color::Accent),
            (RoundState::Partial, _) => (format!("Round {} · partly sent", last.number), Color::Error),
            (RoundState::Unknown, _) => (format!("Round {} · checking GitHub", last.number), Color::Warning),
            (_, None) => {
                let moved = self.state.pr.as_ref().is_some_and(|pr| pr.head_ref_oid != last.sha);
                (
                    if moved {
                        format!("Round {} · drafting · the PR moved since {}", last.number, short(&last.sha))
                    } else {
                        format!("Round {} · drafting at {}", last.number, short(&last.sha))
                    },
                    if moved { Color::Warning } else { Color::Muted },
                )
            }
        };
        let url = sent.map(|sent| sent.sent.review_url.clone()).filter(|url| !url.is_empty());
        Some(
            h_flex()
                .gap_1()
                .child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(color))
                .child(Label::new(text).size(LabelSize::Small).color(color).truncate())
                .when_some(url, |row, url| {
                    row.child(
                        Button::new("open-sent-review", "View")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text("Opens the sent review on GitHub"))
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                })
                .into_any_element(),
        )
    }

    /// Review and Tools, as the Work panel has Up next and Project.
    pub(crate) fn render_dock_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab_button = |id: &'static str, label: &'static str, tab: DockTab, cx: &mut Context<Self>| {
            let selected = self.draft.tab == tab;
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .style(if selected { ButtonStyle::Outlined } else { ButtonStyle::Subtle })
                .when(!selected, |button| button.color(Color::Muted))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.draft.tab = tab;
                    cx.notify();
                }))
        };
        let running = self.state.local_n8n.as_ref().is_some_and(|local| local.running)
            || self.state.tools.as_ref().is_some_and(|tools| tools.preview.is_some());
        h_flex()
            .w_full()
            .pt_1p5()
            .gap_1()
            .child(tab_button("dock-tab-review", "Review", DockTab::Review, cx))
            .child(
                h_flex()
                    .gap_0p5()
                    .child(tab_button("dock-tab-tools", "Tools", DockTab::Tools, cx))
                    .when(running, |tab| {
                        tab.child(Icon::new(IconName::Circle).size(IconSize::XSmall).color(Color::Success))
                    }),
            )
            .into_any_element()
    }

    pub(crate) fn render_verify_card(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (round, state) = self.verify_card_state(cx)?;
        let colors = cx.theme().colors().clone();
        let warning = cx.theme().status().warning;
        let terminals = self.claude_terminals(cx);
        let progress = self.state.progress.get(&ReviewAction::Verify);
        let results = self.verify_results(&round);
        let commits = self.state.since_sent.as_ref().map_or(0, |(_, count, _)| *count);
        let (summary, color, button): (String, Color, Option<(&str, bool)>) = match &state {
            VerifyState::Waiting => (
                format!("Waiting for fixes · round {} at {}", round.number, short(&round.sha)),
                Color::Muted,
                Some(("Verify now", false)),
            ),
            VerifyState::Ready => (
                match commits {
                    1 => "1 new commit · nobody asked you again yet".to_string(),
                    count => format!("{count} new commits · nobody asked you again yet"),
                },
                Color::Accent,
                Some(("Verify now", true)),
            ),
            VerifyState::NoAnswers => (
                "The fix thread ended without answers".to_string(),
                Color::Warning,
                Some(("Verify anyway", true)),
            ),
            VerifyState::Start => ("Starting".to_string(), Color::Accent, None),
            VerifyState::Running => (
                progress.and_then(|progress| progress.summary.clone()).unwrap_or_else(|| "Claude checks each item".to_string()),
                Color::Accent,
                None,
            ),
            VerifyState::Done => (
                results
                    .as_ref()
                    .map(|results| format!("Done at {} · open items are in the next draft", short(&results.head)))
                    .unwrap_or_else(|| "Done".to_string()),
                Color::Success,
                None,
            ),
            VerifyState::Stale => (
                "New commits since Verify. The results below are older.".to_string(),
                Color::Warning,
                Some(("Run again", true)),
            ),
            VerifyState::Interrupted => (
                "Verify stopped before it finished".to_string(),
                Color::Warning,
                Some(("Retry", true)),
            ),
            VerifyState::Failed(error) => (format!("Verify failed: {error}"), Color::Error, Some(("Retry", true))),
            VerifyState::Off => ("The PR is merged or closed. Nothing to verify.".to_string(), Color::Muted, None),
            VerifyState::None => return None,
        };
        let running = matches!(state, VerifyState::Running | VerifyState::Start);
        let header = h_flex()
            .w_full()
            .gap_2()
            .child(
                div()
                    .size_7()
                    .flex_none()
                    .rounded_md()
                    .bg(colors.element_background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Check).size(IconSize::Small).color(color)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(format!("Verify round {}", round.number)).weight(FontWeight::SEMIBOLD))
                    .child(Label::new(summary).size(LabelSize::XSmall).color(Color::Muted).truncate()),
            )
            .when(running, |row| row.child(SpinnerLabel::new().size(LabelSize::XSmall).color(Color::Accent)))
            .when_some(button, |row, (label, filled)| {
                row.child(
                    Button::new("verify-start", label)
                        .style(if filled { ButtonStyle::Filled } else { ButtonStyle::Outlined })
                        .label_size(LabelSize::Small)
                        .tooltip(Tooltip::text(
                            "Claude reads the round, the new commits and the replies, and checks each item. It only reads.",
                        ))
                        .on_click(cx.listener(|this, _, window, cx| this.start_verify(true, window, cx))),
                )
            })
            .when(matches!(state, VerifyState::Done | VerifyState::Stale), |row| {
                row.child(
                    Button::new("verify-results", "Results")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| this.open_draft_tab(window, cx))),
                )
            });
        let steps = (running || matches!(state, VerifyState::Done)).then(|| {
            let steps: Vec<String> = match progress.filter(|progress| !progress.steps.is_empty()) {
                Some(progress) => progress.steps.clone(),
                None => ReviewAction::Verify.default_steps().iter().map(|step| step.to_string()).collect(),
            };
            let current = progress.and_then(|progress| progress.step);
            let done = matches!(state, VerifyState::Done);
            h_flex().w_full().gap_1().children(steps.into_iter().enumerate().map(move |(ix, step)| {
                let finished = done || current.is_some_and(|current| ix < current);
                let bar = if finished {
                    colors.version_control_added
                } else if current == Some(ix) {
                    colors.text_accent
                } else {
                    colors.border
                };
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().h(px(3.)).w_full().rounded_sm().bg(bar))
                    .child(Label::new(step).size(LabelSize::XSmall).color(Color::Muted).truncate())
            }))
        });
        let chips = results.filter(|_| matches!(state, VerifyState::Done | VerifyState::Stale)).map(|results| {
            let mut parts = vec![
                format!("{} fixed", results.count(ItemResult::Fixed)),
                format!("{} partly", results.count(ItemResult::Partly)),
                format!("{} open", results.count(ItemResult::NotAddressed) + results.count(ItemResult::NoAnswer)),
                format!(
                    "{} with a reason",
                    results.count(ItemResult::WillNotFix) + results.count(ItemResult::ResolvedWithoutChange)
                ),
            ];
            if !results.uncovered.is_empty() {
                parts.push(format!("{} files no item covers", results.uncovered.len()));
            }
            Label::new(parts.join(" · ")).size(LabelSize::Small)
        });
        let thread = self
            .action_terminal(ReviewAction::Verify, &terminals)
            .filter(|_| running)
            .map(|terminal| Self::render_thread_label("thread-verify", Some(&terminal), cx));
        Some(
            v_flex()
                .w_full()
                .p_2()
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_color(if matches!(state, VerifyState::Ready | VerifyState::NoAnswers | VerifyState::Stale) {
                    warning.opacity(0.6)
                } else {
                    colors.border
                })
                .bg(colors.editor_background)
                .child(header)
                .children(steps)
                .children(chips)
                .children(thread)
                .into_any_element(),
        )
    }

    pub(crate) fn open_draft_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pr), Some(workspace)) = (self.state.pr.clone(), self.workspace.upgrade()) else {
            return;
        };
        let rounds = self.state.rounds.clone();
        let panel = cx.weak_entity();
        workspace.update(cx, |workspace, cx| {
            let existing = workspace.items_of_type::<DraftView>(cx).next();
            if let Some(view) = existing {
                view.update(cx, |view, cx| view.set_rounds(rounds, cx));
                workspace.activate_item(&view, true, true, window, cx);
                return;
            }
            let view = cx.new(|cx| DraftView::new(pr.number, rounds, panel, cx));
            workspace.active_pane().update(cx, |pane, cx| {
                pane.add_item(Box::new(view), true, true, None, window, cx);
            });
        });
    }

    pub(crate) fn set_item_included(&mut self, key: &str, include: bool, cx: &mut Context<Self>) {
        self.edit_draft(cx, |round| {
            if let Some(item) = round.items.iter_mut().find(|item| item.key == key) {
                item.include = include;
            }
        });
    }

    pub(crate) fn set_item_must_fix(&mut self, key: &str, must_fix: bool, cx: &mut Context<Self>) {
        self.edit_draft(cx, |round| {
            if let Some(item) = round.items.iter_mut().find(|item| item.key == key) {
                item.must_fix = must_fix;
            }
        });
    }

    pub(crate) fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pr = self.state.pr.clone()?;
        let colors = cx.theme().colors().clone();
        let round = self.open_round().cloned();
        let destination = self.destination_of(round.as_ref());
        let own_pr = self.own_pr();
        let author = pr.author_login().to_string();
        let number = round.as_ref().map_or_else(|| self.state.rounds.last().map_or(1, |round| round.number + 1), |round| round.number);
        let label = match destination {
            Destination::GitHub if author.is_empty() => format!("DRAFT · ROUND {number} · GITHUB REVIEW"),
            Destination::GitHub => format!("DRAFT · ROUND {number} · GITHUB REVIEW FOR {}", author.to_uppercase()),
            Destination::Agent => format!("DRAFT · ROUND {number} · FIX LIST FOR A CODING AGENT"),
        };
        let items: Vec<&review_round::Item> = round.as_ref().map(|round| round.included().collect()).unwrap_or_default();
        let count = |source: Source| items.iter().filter(|item| item.source == source).count();
        let screenshots: usize = items.iter().map(|item| item.attachments.len()).sum();
        let mut chips = Vec::new();
        if count(Source::Agent) > 0 {
            chips.push(format!("{} agent", count(Source::Agent)));
        }
        if count(Source::Manual) > 0 {
            chips.push(match screenshots {
                0 => format!("{} manual", count(Source::Manual)),
                shots => format!("{} manual · {shots} screenshots", count(Source::Manual)),
            });
        }
        if count(Source::You) > 0 {
            chips.push(format!("{} yours", count(Source::You)));
        }
        let carried = items.iter().filter(|item| item.carried.is_some()).count();
        if carried > 0 {
            chips.push(format!("{carried} carried over"));
        }
        let state = round.as_ref().map_or(RoundState::Draft, |round| round.state);
        let sending = self.draft.sending.is_some() || state == RoundState::Sending;
        let decision = round.as_ref().map_or(Decision::RequestChanges, |round| round.decision);
        let decision = if own_pr && destination == Destination::GitHub { Decision::Comment } else { decision };
        let primary_label = match (destination, state) {
            (_, _) if sending => "Sending…".to_string(),
            (Destination::Agent, RoundState::Failed) => "Retry: send to agent".to_string(),
            (Destination::Agent, _) => "Send to agent".to_string(),
            (Destination::GitHub, RoundState::Pending) => format!("Submit: {}", decision.label().to_lowercase()),
            (Destination::GitHub, RoundState::Failed | RoundState::Partial | RoundState::Unknown) => {
                format!("Retry: {}", decision.label().to_lowercase())
            }
            (Destination::GitHub, _) => decision.label().to_string(),
        };
        let error = round.as_ref().map(|round| round.error.clone()).filter(|error| !error.is_empty());
        let summary = if round.is_some() { self.summary_editor(window, cx) } else { None };
        let moved = round.as_ref().is_some_and(|round| round.sha != pr.head_ref_oid && round.is_open());
        let pending = state == RoundState::Pending;
        let empty_note = round.is_none() || items.is_empty();

        let menu_handle = self.draft.decision_menu.clone();
        let panel = cx.weak_entity();
        let menu_open = menu_handle.is_deployed();
        let menu = PopoverMenu::new("draft-decision-menu")
            .trigger(
                ButtonLike::new_rounded_right("draft-decision-trigger")
                    .layer(ui::ElevationIndex::ModalSurface)
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .width(rems_from_px(20_f32))
                    .height(rems_from_px(20_f32).into())
                    .disabled(sending)
                    .child(Icon::new(if menu_open { IconName::ChevronUp } else { IconName::ChevronDown }).size(IconSize::XSmall)),
            )
            .with_handle(menu_handle)
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    if destination == Destination::GitHub {
                        let decisions: &[Decision] = if own_pr {
                            &[Decision::Comment]
                        } else {
                            &[Decision::Comment, Decision::Approve, Decision::RequestChanges]
                        };
                        for &choice in decisions {
                            let panel = panel.clone();
                            menu = menu.toggleable_entry(choice.label(), choice == decision, IconPosition::Start, None, move |_, cx| {
                                panel.update(cx, |panel, cx| panel.set_decision(choice, cx)).ok();
                            });
                        }
                        menu = menu.separator();
                        let save_panel = panel.clone();
                        menu = menu.entry("Save as pending review", None, move |window, cx| {
                            save_panel.update(cx, |panel, cx| panel.send_draft(true, window, cx)).ok();
                        });
                        let switch_panel = panel;
                        menu = menu.entry("Send to a coding agent instead", None, move |_, cx| {
                            switch_panel.update(cx, |panel, cx| panel.set_destination(Destination::Agent, cx)).ok();
                        });
                    } else {
                        let switch_panel = panel;
                        menu = menu.entry("Comment on GitHub instead", None, move |_, cx| {
                            switch_panel.update(cx, |panel, cx| panel.set_destination(Destination::GitHub, cx)).ok();
                        });
                    }
                    menu
                }))
            });
        let primary_color = match (destination, decision) {
            (Destination::GitHub, Decision::RequestChanges) => Color::Error,
            (Destination::GitHub, Decision::Approve) => Color::Success,
            _ => Color::Accent,
        };
        let primary = ButtonLike::new_rounded_left("draft-send")
            .layer(ui::ElevationIndex::ModalSurface)
            .size(ButtonSize::Compact)
            .disabled(sending)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(match destination {
                            Destination::GitHub => IconName::Send,
                            Destination::Agent => IconName::Terminal,
                        })
                        .size(IconSize::XSmall)
                        .color(primary_color),
                    )
                    .child(Label::new(primary_label).size(LabelSize::Small).mr_0p5()),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                if pending {
                    this.submit_pending(cx)
                } else {
                    this.send_draft(false, window, cx)
                }
            }));

        Some(
            v_flex()
                .w_full()
                .flex_none()
                .px_3()
                .pt_2()
                .pb_3()
                .gap_2()
                .border_t_1()
                .border_color(colors.border)
                .bg(colors.title_bar_background)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(Label::new(label).size(LabelSize::XSmall).color(Color::Muted).truncate()))
                        .child(
                            Button::new("open-draft-tab", "Open draft")
                                .style(ButtonStyle::Outlined)
                                .size(ButtonSize::Compact)
                                .label_size(LabelSize::Small)
                                .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall))
                                .on_click(cx.listener(|this, _, window, cx| this.open_draft_tab(window, cx))),
                        ),
                )
                .child(
                    Label::new(if empty_note {
                        "Nothing in the draft yet. Tick findings, flag manual test cases, or add your comments."
                            .to_string()
                    } else {
                        chips.join(" · ")
                    })
                    .size(LabelSize::Small)
                    .color(if empty_note { Color::Muted } else { Color::Default }),
                )
                .when(destination == Destination::Agent, |composer| {
                    composer.child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border_variant)
                            .child(Label::new("Thread").size(LabelSize::XSmall).color(Color::Muted))
                            .child(Label::new(format!("New: Fix round {number}")).size(LabelSize::Small)),
                    )
                })
                .when_some(summary, |composer, editor| {
                    composer.child(
                        div()
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.border_variant)
                            .bg(colors.editor_background)
                            .child(editor),
                    )
                })
                .when(moved, |composer| {
                    composer.child(
                        Label::new(format!(
                            "The PR moved since you drafted. Comments point at {}.",
                            short(&round.as_ref().map(|round| round.sha.clone()).unwrap_or_default())
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                    )
                })
                .when_some(error, |composer, error| {
                    composer.child(Label::new(error).size(LabelSize::XSmall).color(Color::Error))
                })
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .when(destination == Destination::GitHub && !pending, |row| {
                            row.child(
                                Button::new("draft-save-pending", "Save as pending")
                                    .style(ButtonStyle::Outlined)
                                    .size(ButtonSize::Compact)
                                    .label_size(LabelSize::Small)
                                    .disabled(sending || round.is_none())
                                    .tooltip(Tooltip::text("Saves a pending review on GitHub. Only you see it until you submit."))
                                    .on_click(cx.listener(|this, _, window, cx| this.send_draft(true, window, cx))),
                            )
                        })
                        .child(div().flex_1())
                        .child(SplitButton::new(primary, menu.into_any_element())),
                )
                .child(
                    Label::new(match destination {
                        Destination::GitHub if own_pr => "Your own PR: GitHub allows only a comment. The menu sends to a coding agent instead.",
                        Destination::GitHub => "One GitHub review: inline comments, the summary, and the manual test cases.",
                        Destination::Agent => "A new Claude thread fixes the items and answers each one.",
                    })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
                .into_any_element(),
        )
    }
}

fn state_word(state: RoundState) -> &'static str {
    match state {
        RoundState::Draft => "a draft",
        RoundState::Pending => "pending on GitHub",
        RoundState::Sending => "sending",
        RoundState::Sent => "sent",
        RoundState::Failed => "failed",
        RoundState::Partial => "partly sent",
        RoundState::Unknown => "being checked on GitHub",
    }
}
