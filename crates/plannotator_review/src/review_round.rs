//! The review draft and its rounds (`zed8/REVIEW_FLOW.md`). One draft per PR
//! collects items from the agent review, the manual test and your own
//! comments. Only the draft sends, and each send is a round. Everything here
//! is plain data, so the rules are tested without GitHub or a window.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    #[default]
    Agent,
    Manual,
    /// Your own inline comments from a guide chapter or the findings tab.
    You,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Destination {
    GitHub,
    Agent,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    Comment,
    Approve,
    #[default]
    RequestChanges,
}

impl Decision {
    pub fn label(self) -> &'static str {
        match self {
            Decision::Comment => "Comment",
            Decision::Approve => "Approve",
            Decision::RequestChanges => "Request changes",
        }
    }

    fn github_event(self) -> &'static str {
        match self {
            Decision::Comment => "COMMENT",
            Decision::Approve => "APPROVE",
            Decision::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RoundState {
    #[default]
    Draft,
    Pending,
    Sending,
    Sent,
    Failed,
    Partial,
    Unknown,
}

/// What Verify found for one item of the round before.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemResult {
    Fixed,
    Partly,
    NotAddressed,
    WillNotFix,
    ResolvedWithoutChange,
    NoAnswer,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Carried {
    pub round: u32,
    /// The GitHub review comment of the item, for a reply on its thread.
    #[serde(default)]
    pub comment_id: Option<u64>,
    #[serde(default)]
    pub result: Option<ItemResult>,
    #[serde(default)]
    pub evidence: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    /// Stable across reruns of the source: `agent:<finding>`, `manual:<case>`,
    /// or `you:<file>:<line>:<hash>`.
    pub key: String,
    pub source: Source,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub line_end: Option<u32>,
    #[serde(default)]
    pub severity: Option<String>,
    /// A manual test case: "MT4 · Blocked update cannot be ticked".
    #[serde(default)]
    pub title: Option<String>,
    pub body: String,
    /// Copies in the round folder, so a new manual test run cannot remove them.
    #[serde(default)]
    pub attachments: Vec<PathBuf>,
    #[serde(default = "yes")]
    pub include: bool,
    /// For a coding agent: must fix, or the agent decides.
    #[serde(default)]
    pub must_fix: bool,
    /// You changed the text in the draft, so a source refresh keeps it.
    #[serde(default)]
    pub edited: bool,
    /// The source no longer has it, after a rerun.
    #[serde(default)]
    pub orphaned: bool,
    #[serde(default)]
    pub carried: Option<Carried>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SentInfo {
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub decision: Option<Decision>,
    #[serde(default)]
    pub review_id: Option<u64>,
    #[serde(default)]
    pub review_url: String,
    /// The manual test cases, posted as their own comment before the review.
    #[serde(default)]
    pub manual_comment_url: String,
    /// GitHub review comment of each inline item, by item key.
    #[serde(default)]
    pub comment_ids: BTreeMap<String, u64>,
    /// Carried items whose reply is posted.
    #[serde(default)]
    pub replied: BTreeSet<String>,
    /// The terminal title of the fix thread.
    #[serde(default)]
    pub thread: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VerifyRun {
    #[default]
    NotStarted,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRecord {
    pub run: VerifyRun,
    /// The PR head that Verify checked.
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub started_at: String,
    /// The process of the Verify thread, to tell Running from Interrupted.
    #[serde(default)]
    pub thread: String,
    #[serde(default)]
    pub error: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Round {
    pub pr: u64,
    pub number: u32,
    /// The commit the items point at: the PR head when the draft began.
    pub sha: String,
    /// Set when you switch the destination by hand.
    #[serde(default)]
    pub destination: Option<Destination>,
    #[serde(default)]
    pub decision: Decision,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub state: RoundState,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub sent: SentInfo,
    #[serde(default)]
    pub verify: Option<VerifyRecord>,
}

/// The hidden line in every review body and manual test comment of a round,
/// so Zed8 finds its own posts after a crash or a timeout.
pub fn marker(pr: u64, round: u32) -> String {
    format!("<!-- zed8 pr-{pr} round-{round} -->")
}

fn manual_marker(pr: u64, round: u32) -> String {
    format!("<!-- zed8 pr-{pr} round-{round} manual -->")
}

impl Round {
    pub fn new(pr: u64, number: u32, sha: impl Into<String>) -> Self {
        Self { pr, number, sha: sha.into(), ..Default::default() }
    }

    pub fn included(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|item| item.include)
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, RoundState::Draft | RoundState::Failed)
    }

    /// Ticks an item. An item already in the draft keeps your edits.
    pub fn add(&mut self, item: Item) {
        match self.items.iter_mut().find(|existing| existing.key == item.key) {
            Some(existing) => existing.include = true,
            None => self.items.push(item),
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.items.retain(|item| item.key != key);
    }
}

/// A manual test case as the draft sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ManualCase {
    pub id: String,
    pub title: String,
    pub include: bool,
    pub posted: bool,
    /// The case as it reads in the comment: note or outcome, then steps.
    pub body: String,
    pub attachments: Vec<PathBuf>,
}

/// Keeps the manual items of an open round equal to the flagged cases.
/// Attachments are copied into `folder` by the caller; here they are
/// already the copies.
pub fn sync_manual(round: &mut Round, cases: &[ManualCase]) {
    let wanted: Vec<&ManualCase> = cases.iter().filter(|case| case.include && !case.posted).collect();
    round.items.retain(|item| {
        item.source != Source::Manual
            || item.carried.is_some()
            || wanted.iter().any(|case| item.key == format!("manual:{}", case.id))
    });
    for case in wanted {
        let key = format!("manual:{}", case.id);
        match round.items.iter_mut().find(|item| item.key == key) {
            Some(item) => {
                item.include = true;
                item.title = Some(case.title.clone());
                item.attachments = case.attachments.clone();
                if !item.edited {
                    item.body = case.body.clone();
                }
            }
            None => round.items.push(Item {
                key,
                source: Source::Manual,
                title: Some(case.title.clone()),
                body: case.body.clone(),
                attachments: case.attachments.clone(),
                include: true,
                ..Default::default()
            }),
        }
    }
}

/// Your comment's key: the same comment added twice is one item.
pub fn your_key(path: &str, line: u32, text: &str) -> String {
    // FNV-1a: stable across builds, unlike the standard hasher.
    let hash = text.trim().bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("you:{path}:{line}:{:08x}", hash as u32)
}

/// GitHub for someone else's PR, the coding agent for your own. Your pick wins.
pub fn destination(author: &str, me: &str, picked: Option<Destination>) -> Destination {
    match picked {
        Some(picked) => picked,
        None if !me.is_empty() && author.eq_ignore_ascii_case(me) => Destination::Agent,
        None => Destination::GitHub,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    PrClosed,
    Busy,
    Empty,
    NeedsSummary,
    OldGh,
    OwnPrDecision,
}

impl Block {
    pub fn message(&self) -> &'static str {
        match self {
            Block::PrClosed => "The PR is merged or closed.",
            Block::Busy => "This round is already sending.",
            Block::Empty => "Add an item or a summary first.",
            Block::NeedsSummary => "Request changes needs a summary or an item.",
            Block::OldGh => "This gh is too old to attach screenshots. Update gh to 2.99 or newer.",
            Block::OwnPrDecision => "GitHub does not allow Approve or Request changes on your own PR.",
        }
    }
}

pub struct GuardContext {
    pub pr_open: bool,
    pub destination: Destination,
    pub gh_attaches: bool,
    pub own_pr: bool,
}

pub fn guard(round: &Round, context: &GuardContext) -> Result<(), Block> {
    if round.state == RoundState::Sending {
        return Err(Block::Busy);
    }
    if !context.pr_open {
        return Err(Block::PrClosed);
    }
    let empty = round.included().next().is_none() && round.summary.trim().is_empty();
    if context.destination == Destination::Agent {
        return if empty { Err(Block::Empty) } else { Ok(()) };
    }
    if context.own_pr && round.decision != Decision::Comment {
        return Err(Block::OwnPrDecision);
    }
    if empty {
        return match round.decision {
            Decision::Approve => Ok(()),
            Decision::RequestChanges => Err(Block::NeedsSummary),
            Decision::Comment => Err(Block::Empty),
        };
    }
    let attaches = round.included().any(|item| !item.attachments.is_empty());
    if attaches && !context.gh_attaches {
        return Err(Block::OldGh);
    }
    Ok(())
}

/// The lines of each file that a GitHub review comment can sit on: the
/// added and context lines of the PR diff, on the new side.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commentable(BTreeMap<String, BTreeSet<u32>>);

impl Commentable {
    pub fn contains(&self, path: &str, line: u32) -> bool {
        self.0.get(path).is_some_and(|lines| lines.contains(&line))
    }
}

pub fn commentable_lines(patch: &str) -> Commentable {
    let mut lines: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut file: Option<String> = None;
    let mut in_hunk = false;
    let mut new_line = 0u32;
    for text in patch.lines() {
        if text.starts_with("diff --git ") {
            file = None;
            in_hunk = false;
        } else if !in_hunk && let Some(path) = text.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").map(str::to_string);
        } else if let Some(header) = text.strip_prefix("@@ ") {
            // "-1,3 +1,4 @@ context": the new side starts at 1.
            new_line = header
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .and_then(|range| range.split(',').next()?.parse().ok())
                .unwrap_or(1);
            in_hunk = true;
        } else if in_hunk && let Some(path) = &file {
            match text.chars().next() {
                Some(' ') | Some('+') | None => {
                    lines.entry(path.clone()).or_default().insert(new_line);
                    new_line += 1;
                }
                _ => {}
            }
        }
    }
    Commentable(lines)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    pub side: &'static str,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReviewRequest {
    pub commit_id: String,
    pub body: String,
    /// `None` saves a pending review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<&'static str>,
    pub comments: Vec<InlineComment>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ManualComment {
    pub body: String,
    pub attachments: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub key: String,
    pub comment_id: u64,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Posted first, because the review body cannot carry uploads (O1).
    pub manual: Option<ManualComment>,
    pub review: ReviewRequest,
    /// Replies on the threads of carried items, after the review.
    pub replies: Vec<Reply>,
    /// Item keys keyed by the index of their inline comment.
    pub inline_keys: Vec<String>,
    /// Items that could not sit on a diff line and went into the body.
    pub moved_to_body: Vec<String>,
}

pub struct PlanContext<'a> {
    pub commentable: &'a Commentable,
    /// GitHub still has the round's commit (no force-push removed it).
    pub sha_on_github: bool,
    pub pending: bool,
}

pub fn plan(round: &Round, context: &PlanContext) -> Plan {
    let included: Vec<&Item> = round.included().collect();
    let manual: Vec<&Item> = included.iter().copied().filter(|item| item.source == Source::Manual).collect();
    let manual_alone = manual.iter().any(|item| !item.attachments.is_empty());
    let mut comments = Vec::new();
    let mut inline_keys = Vec::new();
    let mut replies = Vec::new();
    let mut moved_to_body = Vec::new();
    let mut body_parts: Vec<String> = Vec::new();
    if !round.summary.trim().is_empty() {
        body_parts.push(round.summary.trim().to_string());
    }
    for item in &included {
        if item.source == Source::Manual {
            if !manual_alone {
                body_parts.push(item.body.trim().to_string());
            }
            continue;
        }
        if let Some(comment_id) = item.carried.as_ref().and_then(|carried| carried.comment_id) {
            replies.push(Reply { key: item.key.clone(), comment_id, body: item.body.clone() });
            continue;
        }
        match (item.path.as_deref(), item.line) {
            (Some(path), Some(line)) if context.sha_on_github && context.commentable.contains(path, line) => {
                let start_line = item
                    .line_end
                    .filter(|end| *end > line && context.commentable.contains(path, *end))
                    .map(|_| line);
                comments.push(InlineComment {
                    path: path.to_string(),
                    line: start_line.and(item.line_end).unwrap_or(line),
                    start_line,
                    side: "RIGHT",
                    body: item.body.clone(),
                });
                inline_keys.push(item.key.clone());
            }
            (Some(path), line) => {
                let place = line.map(|line| format!("{path}:{line}")).unwrap_or_else(|| path.to_string());
                body_parts.push(format!("`{place}`\n{}", item.body.trim()));
                moved_to_body.push(item.key.clone());
            }
            (None, _) => body_parts.push(item.body.trim().to_string()),
        }
    }
    body_parts.push(marker(round.pr, round.number));
    let manual = manual_alone.then(|| {
        let mut parts = vec![match manual.len() {
            1 => "Manual test: 1 case to look at.".to_string(),
            count => format!("Manual test: {count} cases to look at."),
        }];
        let mut attachments = Vec::new();
        for item in &manual {
            parts.push(item.body.trim().to_string());
            let id = item.key.trim_start_matches("manual:");
            let images: Vec<String> = item
                .attachments
                .iter()
                .map(|path| {
                    attachments.push(path.clone());
                    format!("![{id}](<{}>)", path.display())
                })
                .collect();
            if !images.is_empty() {
                parts.push(images.join("\n"));
            }
        }
        parts.push(manual_marker(round.pr, round.number));
        ManualComment { body: parts.join("\n\n"), attachments }
    });
    Plan {
        manual,
        review: ReviewRequest {
            commit_id: round.sha.clone(),
            body: body_parts.join("\n\n"),
            event: (!context.pending).then(|| round.decision.github_event()),
            comments,
        },
        replies,
        inline_keys,
        moved_to_body,
    }
}

/// Something that happened to a send.
#[derive(Clone, Debug, PartialEq)]
pub enum SendEvent {
    Start,
    SavedPending,
    Succeeded,
    /// Nothing reached GitHub or the agent.
    Failed,
    /// Some steps posted, the rest did not.
    PartlyFailed,
    /// Zed8 started and found the round still sending.
    Restarted,
    /// Reconcile found the review with the round's marker.
    FoundOnGitHub { pending: bool },
    /// Reconcile found nothing.
    NotOnGitHub,
}

pub fn next(state: RoundState, event: &SendEvent) -> RoundState {
    use RoundState::*;
    match (state, event) {
        (Draft | Failed | Partial, SendEvent::Start) => Sending,
        (Sending, SendEvent::SavedPending) => Pending,
        (Sending | Unknown, SendEvent::Succeeded) => Sent,
        (Sending, SendEvent::Failed) => Failed,
        (Sending, SendEvent::PartlyFailed) => Partial,
        (Sending, SendEvent::Restarted) => Unknown,
        (Sending | Unknown | Pending | Failed, SendEvent::FoundOnGitHub { pending }) => {
            if *pending { Pending } else { Sent }
        }
        (Unknown, SendEvent::NotOnGitHub) => Failed,
        (Pending, SendEvent::NotOnGitHub) => Draft,
        (state, _) => state,
    }
}

/// One of your reviews on the PR, from `GET /pulls/{n}/reviews`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct GitHubReview {
    pub id: u64,
    #[serde(default)]
    pub body: String,
    /// `PENDING`, `COMMENTED`, `APPROVED`, `CHANGES_REQUESTED` or `DISMISSED`.
    pub state: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub submitted_at: Option<String>,
}

/// The review of `round` on GitHub, found by its marker.
pub fn find_review<'a>(round: &Round, reviews: &'a [GitHubReview]) -> Option<&'a GitHubReview> {
    let marker = marker(round.pr, round.number);
    reviews.iter().rev().find(|review| review.body.contains(&marker))
}

/// What the PR looks like now, for the Verify card.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrNow {
    pub open: bool,
    pub head: String,
    /// The author asked you to review again: you are in `reviewRequests`.
    pub asked_again: bool,
    /// Commits on the PR since the round's commit.
    pub commits_since: u32,
    /// The fix thread wrote its answers file (own PR).
    pub answers: bool,
    /// The fix thread is still running (own PR).
    pub fix_thread_running: bool,
    /// The Verify thread is still running.
    pub verify_thread_running: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VerifyState {
    None,
    Waiting,
    Ready,
    /// The fix thread stopped without an answers file.
    NoAnswers,
    Start,
    Running,
    Done,
    Stale,
    Interrupted,
    Failed(String),
    Off,
}

/// The Verify card for the newest round that left Zed8.
pub fn verify_state(round: &Round, destination: Destination, pr: &PrNow) -> VerifyState {
    if round.state != RoundState::Sent || round.sent.decision == Some(Decision::Approve) {
        return VerifyState::None;
    }
    if !pr.open {
        return VerifyState::Off;
    }
    if let Some(verify) = &round.verify {
        match verify.run {
            VerifyRun::Running if pr.verify_thread_running => return VerifyState::Running,
            VerifyRun::Running => return VerifyState::Interrupted,
            VerifyRun::Done if pr.head != verify.head => return VerifyState::Stale,
            VerifyRun::Done => return VerifyState::Done,
            VerifyRun::Failed => return VerifyState::Failed(verify.error.clone()),
            VerifyRun::NotStarted => {}
        }
    }
    match destination {
        Destination::GitHub if pr.asked_again => VerifyState::Start,
        Destination::GitHub if pr.commits_since > 0 => VerifyState::Ready,
        Destination::GitHub => VerifyState::Waiting,
        Destination::Agent if pr.answers => VerifyState::Start,
        Destination::Agent if pr.fix_thread_running || round.sent.thread.is_empty() => VerifyState::Waiting,
        Destination::Agent => VerifyState::NoAnswers,
    }
}

/// What the Verify thread writes to `round-<n>.verify.json`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResults {
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub items: Vec<VerifyItem>,
    /// Changed files that no item covers.
    #[serde(default)]
    pub uncovered: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyItem {
    pub key: String,
    pub result: ItemResult,
    #[serde(default)]
    pub evidence: String,
}

impl VerifyResults {
    pub fn count(&self, result: ItemResult) -> usize {
        self.items.iter().filter(|item| item.result == result).count()
    }
}

/// The next round after Verify: every item that is not fixed, carried with
/// its result. Partly fixed, not addressed and unanswered items are ticked.
/// Items with a reason or resolved by the author wait for you, not ticked.
pub fn next_round(sent: &Round, results: &VerifyResults) -> Round {
    let mut next = Round::new(sent.pr, sent.number + 1, results.head.clone());
    next.destination = sent.destination;
    for item in sent.included() {
        let Some(found) = results.items.iter().find(|found| found.key == item.key) else {
            continue;
        };
        let include = match found.result {
            ItemResult::Fixed => continue,
            ItemResult::Partly | ItemResult::NotAddressed | ItemResult::NoAnswer => true,
            ItemResult::WillNotFix | ItemResult::ResolvedWithoutChange => false,
        };
        let comment_id = sent
            .sent
            .comment_ids
            .get(&item.key)
            .copied()
            .or_else(|| item.carried.as_ref().and_then(|carried| carried.comment_id));
        next.items.push(Item {
            include,
            carried: Some(Carried {
                round: sent.number,
                comment_id,
                result: Some(found.result),
                evidence: found.evidence.clone(),
            }),
            ..item.clone()
        });
    }
    next.decision = if next.included().next().is_some() { Decision::RequestChanges } else { Decision::Approve };
    next
}

/// The round file of round `number`, in the review folder of the PR.
pub fn round_path(review_dir: &Path, number: u32) -> PathBuf {
    review_dir.join("rounds").join(format!("round-{number}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, path: &str, line: u32) -> Item {
        Item {
            key: format!("agent:{id}"),
            source: Source::Agent,
            path: Some(path.into()),
            line: Some(line),
            severity: Some("Medium".into()),
            body: format!("Finding {id}"),
            include: true,
            ..Default::default()
        }
    }

    fn case(id: &str, include: bool) -> ManualCase {
        ManualCase {
            id: id.into(),
            title: format!("Case {id}"),
            include,
            posted: false,
            body: format!("### {id}\n\nNote for {id}"),
            attachments: vec![PathBuf::from(format!("/r/rounds/round-1/{id}.jpg"))],
        }
    }

    fn sent_round() -> Round {
        let mut round = Round::new(43539, 1, "a3ede3e");
        round.add(agent("f1", "a.ts", 3));
        round.state = RoundState::Sent;
        round
    }

    fn pr(head: &str) -> PrNow {
        PrNow { open: true, head: head.into(), ..Default::default() }
    }

    // Collect

    #[test]
    fn a_ticked_finding_joins_the_draft_once() {
        let mut round = Round::new(1, 1, "abc");
        round.add(agent("f1", "a.ts", 3));
        round.add(agent("f1", "a.ts", 3));
        assert_eq!(round.items.len(), 1);
        assert!(round.items[0].include);
    }

    #[test]
    fn unticking_before_the_send_removes_the_item() {
        let mut round = Round::new(1, 1, "abc");
        round.add(agent("f1", "a.ts", 3));
        round.remove("agent:f1");
        assert!(round.items.is_empty());
    }

    #[test]
    fn ticking_again_keeps_your_edit() {
        let mut round = Round::new(1, 1, "abc");
        round.add(agent("f1", "a.ts", 3));
        round.items[0].body = "My words".into();
        round.items[0].edited = true;
        round.add(agent("f1", "a.ts", 3));
        assert_eq!(round.items[0].body, "My words");
    }

    #[test]
    fn flagged_cases_become_items_and_unflagged_ones_leave() {
        let mut round = Round::new(1, 1, "abc");
        sync_manual(&mut round, &[case("MT4", true), case("MT10", true), case("MT1", false)]);
        let keys: Vec<&str> = round.items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(keys, ["manual:MT4", "manual:MT10"]);
        assert_eq!(round.items[0].title.as_deref(), Some("Case MT4"));
        assert_eq!(round.items[0].attachments, [PathBuf::from("/r/rounds/round-1/MT4.jpg")]);

        sync_manual(&mut round, &[case("MT4", false), case("MT10", true)]);
        let keys: Vec<&str> = round.items.iter().map(|item| item.key.as_str()).collect();
        assert_eq!(keys, ["manual:MT10"]);
    }

    #[test]
    fn a_new_note_updates_the_item_unless_you_edited_it_in_the_draft() {
        let mut round = Round::new(1, 1, "abc");
        sync_manual(&mut round, &[case("MT4", true)]);
        let mut changed = case("MT4", true);
        changed.body = "### MT4\n\nNew note".into();
        sync_manual(&mut round, &[changed.clone()]);
        assert_eq!(round.items[0].body, "### MT4\n\nNew note");

        round.items[0].body = "Mine".into();
        round.items[0].edited = true;
        changed.body = "### MT4\n\nNewer note".into();
        sync_manual(&mut round, &[changed]);
        assert_eq!(round.items[0].body, "Mine");
    }

    #[test]
    fn a_case_posted_by_hand_does_not_join_the_draft() {
        let mut round = Round::new(1, 1, "abc");
        let mut posted = case("MT4", true);
        posted.posted = true;
        sync_manual(&mut round, &[posted]);
        assert!(round.items.is_empty());
    }

    #[test]
    fn your_comment_has_one_key_for_the_same_text_and_line() {
        assert_eq!(your_key("a.ts", 3, "Why?"), your_key("a.ts", 3, "Why?"));
        assert_ne!(your_key("a.ts", 3, "Why?"), your_key("a.ts", 4, "Why?"));
        assert!(your_key("a.ts", 3, "Why?").starts_with("you:a.ts:3:"));
    }

    // Decide

    #[test]
    fn someone_elses_pr_goes_to_github_and_yours_to_the_agent() {
        assert_eq!(destination("arvin", "roman", None), Destination::GitHub);
        assert_eq!(destination("roman", "roman", None), Destination::Agent);
        assert_eq!(destination("Roman", "roman", None), Destination::Agent);
        assert_eq!(destination("arvin", "roman", Some(Destination::Agent)), Destination::Agent);
    }

    fn context() -> GuardContext {
        GuardContext { pr_open: true, destination: Destination::GitHub, gh_attaches: true, own_pr: false }
    }

    #[test]
    fn an_empty_approve_can_send_but_an_empty_request_cannot() {
        let mut round = Round::new(1, 1, "abc");
        round.decision = Decision::Approve;
        assert_eq!(guard(&round, &context()), Ok(()));
        round.decision = Decision::RequestChanges;
        assert_eq!(guard(&round, &context()), Err(Block::NeedsSummary));
        round.decision = Decision::Comment;
        assert_eq!(guard(&round, &context()), Err(Block::Empty));
        round.summary = "Looks fine.".into();
        assert_eq!(guard(&round, &context()), Ok(()));
    }

    #[test]
    fn a_closed_pr_a_busy_round_and_an_old_gh_block_the_send() {
        let mut round = Round::new(1, 1, "abc");
        round.add(agent("f1", "a.ts", 3));
        assert_eq!(guard(&round, &GuardContext { pr_open: false, ..context() }), Err(Block::PrClosed));
        round.state = RoundState::Sending;
        assert_eq!(guard(&round, &context()), Err(Block::Busy));
        round.state = RoundState::Draft;
        round.items.push(Item {
            key: "manual:MT4".into(),
            source: Source::Manual,
            body: "x".into(),
            attachments: vec!["/x.jpg".into()],
            include: true,
            ..Default::default()
        });
        assert_eq!(guard(&round, &GuardContext { gh_attaches: false, ..context() }), Err(Block::OldGh));
    }

    #[test]
    fn the_agent_needs_no_decision_and_your_own_pr_cannot_approve_on_github() {
        let mut round = Round::new(1, 1, "abc");
        round.add(agent("f1", "a.ts", 3));
        let agent_context = GuardContext { destination: Destination::Agent, own_pr: true, ..context() };
        assert_eq!(guard(&round, &agent_context), Ok(()));
        let own_github = GuardContext { own_pr: true, ..context() };
        assert_eq!(guard(&round, &own_github), Err(Block::OwnPrDecision));
        round.decision = Decision::Comment;
        assert_eq!(guard(&round, &own_github), Ok(()));
    }

    // Send plan

    const PATCH: &str = "diff --git a/a.ts b/a.ts\n\
--- a/a.ts\n\
+++ b/a.ts\n\
@@ -1,3 +1,4 @@\n\
\x20one\n\
+two\n\
\x20three\n\
-gone\n\
+four\n\
diff --git a/b.ts b/b.ts\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/b.ts\n\
@@ -0,0 +1,2 @@\n\
+x\n\
+y\n";

    #[test]
    fn commentable_lines_are_the_new_side_of_each_hunk() {
        let lines = commentable_lines(PATCH);
        assert!(lines.contains("a.ts", 1));
        assert!(lines.contains("a.ts", 2));
        assert!(lines.contains("a.ts", 4));
        assert!(!lines.contains("a.ts", 9));
        assert!(lines.contains("b.ts", 2));
        assert!(!lines.contains("c.ts", 1));
    }

    fn plan_context(commentable: &Commentable) -> PlanContext<'_> {
        PlanContext { commentable, sha_on_github: true, pending: false }
    }

    #[test]
    fn items_on_diff_lines_go_inline_and_the_rest_into_the_body() {
        let commentable = commentable_lines(PATCH);
        let mut round = Round::new(43539, 1, "a3ede3e");
        round.summary = "Two things.".into();
        round.add(agent("f1", "a.ts", 2));
        round.add(agent("f2", "a.ts", 9));
        round.add(Item { key: "agent:g".into(), body: "General note".into(), include: true, ..Default::default() });
        let plan = plan(&round, &plan_context(&commentable));
        assert_eq!(plan.review.commit_id, "a3ede3e");
        assert_eq!(plan.review.event, Some("REQUEST_CHANGES"));
        assert_eq!(plan.review.comments.len(), 1);
        assert_eq!(plan.review.comments[0].path, "a.ts");
        assert_eq!(plan.review.comments[0].line, 2);
        assert_eq!(plan.review.comments[0].side, "RIGHT");
        assert_eq!(plan.review.comments[0].body, "Finding f1");
        assert_eq!(plan.inline_keys, ["agent:f1"]);
        assert_eq!(plan.moved_to_body, ["agent:f2"]);
        assert!(plan.review.body.starts_with("Two things."));
        assert!(plan.review.body.contains("`a.ts:9`"));
        assert!(plan.review.body.contains("Finding f2"));
        assert!(plan.review.body.contains("General note"));
        assert!(plan.review.body.ends_with(&marker(43539, 1)));
    }

    #[test]
    fn a_force_push_moves_every_inline_item_into_the_body() {
        let commentable = commentable_lines(PATCH);
        let mut round = Round::new(1, 1, "gone");
        round.add(agent("f1", "a.ts", 2));
        let plan = plan(&round, &PlanContext { sha_on_github: false, ..plan_context(&commentable) });
        assert!(plan.review.comments.is_empty());
        assert_eq!(plan.moved_to_body, ["agent:f1"]);
    }

    #[test]
    fn manual_cases_with_screenshots_post_first_as_their_own_comment() {
        let commentable = commentable_lines(PATCH);
        let mut round = Round::new(43539, 1, "a3ede3e");
        sync_manual(&mut round, &[case("MT4", true)]);
        let plan = plan(&round, &plan_context(&commentable));
        let manual = plan.manual.expect("manual comment");
        assert!(manual.body.contains("### MT4"));
        assert!(manual.body.contains("![MT4](</r/rounds/round-1/MT4.jpg>)"));
        assert!(manual.body.ends_with(&manual_marker(43539, 1)));
        assert_eq!(manual.attachments, [PathBuf::from("/r/rounds/round-1/MT4.jpg")]);
        assert!(!plan.review.body.contains("### MT4"));
    }

    #[test]
    fn unticked_items_stay_out_and_pending_has_no_event() {
        let commentable = commentable_lines(PATCH);
        let mut round = Round::new(1, 1, "abc");
        round.add(agent("f1", "a.ts", 2));
        round.items[0].include = false;
        round.decision = Decision::Approve;
        let plan = plan(&round, &PlanContext { pending: true, ..plan_context(&commentable) });
        assert!(plan.review.comments.is_empty());
        assert_eq!(plan.review.event, None);
    }

    #[test]
    fn a_carried_item_with_a_thread_becomes_a_reply() {
        let commentable = commentable_lines(PATCH);
        let mut round = Round::new(1, 2, "def");
        let mut item = agent("f1", "a.ts", 2);
        item.carried = Some(Carried { round: 1, comment_id: Some(77), ..Default::default() });
        round.add(item);
        let plan = plan(&round, &plan_context(&commentable));
        assert!(plan.review.comments.is_empty());
        assert_eq!(plan.replies, [Reply { key: "agent:f1".into(), comment_id: 77, body: "Finding f1".into() }]);
    }

    // Next round

    #[test]
    fn the_next_round_carries_what_is_not_fixed() {
        let mut sent = sent_round();
        sent.add(agent("f2", "a.ts", 9));
        sent.add(agent("f3", "b.ts", 1));
        sent.add(agent("f4", "b.ts", 2));
        sent.sent.comment_ids.insert("agent:f2".into(), 77);
        let result = |key: &str, result| VerifyItem { key: key.into(), result, evidence: format!("about {key}") };
        let results = VerifyResults {
            head: "new".into(),
            items: vec![
                result("agent:f1", ItemResult::Fixed),
                result("agent:f2", ItemResult::Partly),
                result("agent:f3", ItemResult::WillNotFix),
                result("agent:f4", ItemResult::NotAddressed),
            ],
            uncovered: Vec::new(),
        };
        let next = next_round(&sent, &results);
        assert_eq!(next.number, 2);
        assert_eq!(next.sha, "new");
        assert_eq!(next.state, RoundState::Draft);
        let keys: Vec<(&str, bool)> = next.items.iter().map(|item| (item.key.as_str(), item.include)).collect();
        assert_eq!(keys, [("agent:f2", true), ("agent:f3", false), ("agent:f4", true)]);
        let carried = next.items[0].carried.clone().expect("carried");
        assert_eq!(carried.round, 1);
        assert_eq!(carried.comment_id, Some(77));
        assert_eq!(carried.result, Some(ItemResult::Partly));
        assert_eq!(carried.evidence, "about agent:f2");
        assert_eq!(next.decision, Decision::RequestChanges);
    }

    #[test]
    fn nothing_open_after_verify_makes_approve_the_default() {
        let sent = sent_round();
        let results = VerifyResults {
            head: "new".into(),
            items: vec![VerifyItem { key: "agent:f1".into(), result: ItemResult::Fixed, evidence: String::new() }],
            uncovered: Vec::new(),
        };
        let next = next_round(&sent, &results);
        assert!(next.items.is_empty());
        assert_eq!(next.decision, Decision::Approve);
    }

    // Round state

    #[test]
    fn a_send_moves_through_its_states() {
        use RoundState::*;
        assert_eq!(next(Draft, &SendEvent::Start), Sending);
        assert_eq!(next(Failed, &SendEvent::Start), Sending);
        assert_eq!(next(Partial, &SendEvent::Start), Sending);
        assert_eq!(next(Sending, &SendEvent::Succeeded), Sent);
        assert_eq!(next(Sending, &SendEvent::SavedPending), Pending);
        assert_eq!(next(Sending, &SendEvent::Failed), Failed);
        assert_eq!(next(Sending, &SendEvent::PartlyFailed), Partial);
        assert_eq!(next(Sending, &SendEvent::Restarted), Unknown);
        assert_eq!(next(Unknown, &SendEvent::FoundOnGitHub { pending: false }), Sent);
        assert_eq!(next(Unknown, &SendEvent::NotOnGitHub), Failed);
        assert_eq!(next(Pending, &SendEvent::FoundOnGitHub { pending: false }), Sent);
        assert_eq!(next(Pending, &SendEvent::NotOnGitHub), Draft);
        assert_eq!(next(Pending, &SendEvent::FoundOnGitHub { pending: true }), Pending);
    }

    #[test]
    fn a_sent_round_and_a_busy_one_ignore_a_second_send() {
        use RoundState::*;
        assert_eq!(next(Sent, &SendEvent::Start), Sent);
        assert_eq!(next(Sending, &SendEvent::Start), Sending);
        assert_eq!(next(Sent, &SendEvent::Failed), Sent);
    }

    #[test]
    fn the_review_is_found_by_its_marker() {
        let round = Round::new(43539, 1, "abc");
        let reviews = [
            GitHubReview { id: 1, body: "Other".into(), state: "COMMENTED".into(), html_url: String::new(), submitted_at: None },
            GitHubReview {
                id: 2,
                body: format!("Text\n\n{}", marker(43539, 1)),
                state: "CHANGES_REQUESTED".into(),
                html_url: "u".into(),
                submitted_at: None,
            },
            GitHubReview { id: 3, body: marker(43539, 2), state: "COMMENTED".into(), html_url: String::new(), submitted_at: None },
        ];
        assert_eq!(find_review(&round, &reviews).map(|review| review.id), Some(2));
        assert_eq!(find_review(&Round::new(43539, 3, "x"), &reviews), None);
    }

    // Verify

    #[test]
    fn only_a_sent_round_shows_verify() {
        for state in [RoundState::Draft, RoundState::Pending, RoundState::Sending, RoundState::Failed, RoundState::Partial, RoundState::Unknown] {
            let mut round = sent_round();
            round.state = state;
            let now = PrNow { asked_again: true, commits_since: 3, ..pr("new") };
            assert_eq!(verify_state(&round, Destination::GitHub, &now), VerifyState::None, "{state:?}");
        }
    }

    #[test]
    fn a_plain_approve_never_shows_verify() {
        let mut round = sent_round();
        round.sent.decision = Some(Decision::Approve);
        let now = PrNow { asked_again: true, commits_since: 3, ..pr("new") };
        assert_eq!(verify_state(&round, Destination::GitHub, &now), VerifyState::None);
    }

    #[test]
    fn commits_alone_make_verify_ready_and_asking_again_starts_it() {
        let round = sent_round();
        assert_eq!(verify_state(&round, Destination::GitHub, &pr("a3ede3e")), VerifyState::Waiting);
        let pushed = PrNow { commits_since: 3, ..pr("new") };
        assert_eq!(verify_state(&round, Destination::GitHub, &pushed), VerifyState::Ready);
        let asked = PrNow { asked_again: true, ..pushed };
        assert_eq!(verify_state(&round, Destination::GitHub, &asked), VerifyState::Start);
        let asked_without_commits = PrNow { asked_again: true, ..pr("a3ede3e") };
        assert_eq!(verify_state(&round, Destination::GitHub, &asked_without_commits), VerifyState::Start);
    }

    #[test]
    fn your_own_pr_starts_verify_on_the_answers_file() {
        let mut round = sent_round();
        round.sent.thread = "Fix: round 1".into();
        let running = PrNow { fix_thread_running: true, ..pr("a3ede3e") };
        assert_eq!(verify_state(&round, Destination::Agent, &running), VerifyState::Waiting);
        let answered = PrNow { answers: true, ..pr("a3ede3e") };
        assert_eq!(verify_state(&round, Destination::Agent, &answered), VerifyState::Start);
        let stopped = pr("a3ede3e");
        assert_eq!(verify_state(&round, Destination::Agent, &stopped), VerifyState::NoAnswers);
    }

    #[test]
    fn a_running_verify_is_not_started_again_and_a_dead_one_is_interrupted() {
        let mut round = sent_round();
        round.verify = Some(VerifyRecord { run: VerifyRun::Running, head: "new".into(), ..Default::default() });
        let asked = PrNow { asked_again: true, commits_since: 3, verify_thread_running: true, ..pr("new") };
        assert_eq!(verify_state(&round, Destination::GitHub, &asked), VerifyState::Running);
        let dead = PrNow { verify_thread_running: false, ..asked };
        assert_eq!(verify_state(&round, Destination::GitHub, &dead), VerifyState::Interrupted);
    }

    #[test]
    fn new_commits_after_verify_make_it_stale() {
        let mut round = sent_round();
        round.verify = Some(VerifyRecord { run: VerifyRun::Done, head: "new".into(), ..Default::default() });
        let same = PrNow { commits_since: 3, ..pr("new") };
        assert_eq!(verify_state(&round, Destination::GitHub, &same), VerifyState::Done);
        let more = PrNow { commits_since: 5, ..pr("newer") };
        assert_eq!(verify_state(&round, Destination::GitHub, &more), VerifyState::Stale);
    }

    #[test]
    fn a_failed_verify_shows_its_error_and_a_closed_pr_turns_verify_off() {
        let mut round = sent_round();
        round.verify = Some(VerifyRecord { run: VerifyRun::Failed, error: "limit".into(), ..Default::default() });
        assert_eq!(verify_state(&round, Destination::GitHub, &pr("x")), VerifyState::Failed("limit".into()));
        let closed = PrNow { open: false, asked_again: true, ..pr("x") };
        assert_eq!(verify_state(&round, Destination::GitHub, &closed), VerifyState::Off);
    }
}
