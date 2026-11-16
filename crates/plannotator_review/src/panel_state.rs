//! What the Review panel shows, loaded off the main thread: the branch, the
//! PR, the review sessions, the spec reviews, the guide and its file stats.

use crate::{
    actions::{self, ActionProgress, ActionStore, ReviewAction},
    manual_test_view::{self, ManualTestResults},
    review_round::{self, Round, RoundState},
    review_send,
    tools::{self, LocalN8n, PrTools},
    guide::{self, LoadedGuide},
    session::{self, Session},
    start::{self, PullRequest},
};
use util::ResultExt as _;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    time::{Duration, Instant},
};

/// `gh` calls GitHub, so the PR is looked up less often than local state.
const PR_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Default, PartialEq)]
pub struct PanelState {
    pub roots: Vec<PathBuf>,
    pub branch: Option<String>,
    /// Live review sessions of the worktree, newest first.
    pub sessions: Vec<Session>,
    pub guide: Option<LoadedGuide>,
    /// Git status and line counts of the guide's files, by path.
    pub file_stats: HashMap<String, FileStat>,
    /// Live spec reviews (annotate sessions on markdown), oldest first.
    pub specs: Vec<Session>,
    /// The local HEAD commit.
    pub head: Option<String>,
    /// The GitHub PR of the branch, from `gh`. See `PR_REFRESH_INTERVAL`.
    pub pr: Option<PullRequest>,
    pub pr_checked_at: Option<Instant>,
    /// When `gh` last read the PR, to tell an old review request from a new one.
    pub pr_fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The threads and hidden actions of the worktree.
    pub actions: ActionStore,
    /// The progress that each started action reports.
    pub progress: BTreeMap<ReviewAction, ActionProgress>,
    /// The `results.json` of the manual test of this branch, and its path.
    pub manual_results: Option<(PathBuf, ManualTestResults)>,
    /// The labels and the preview of the PR. Refreshed with the PR.
    pub tools: Option<PrTools>,
    pub local_n8n: Option<LocalN8n>,
    /// The Linear issue key of the worktree, such as CONTEXT-429.
    pub linear_issue: Option<String>,
    /// Without a PR: the commits ahead of the base and whether the branch
    /// has an upstream.
    pub branch_ahead: Option<(u32, bool)>,
    /// The review folder of the PR, with the rounds of the review draft.
    pub review_dir: Option<PathBuf>,
    /// Every round of the PR, oldest first.
    pub rounds: Vec<Round>,
    /// Your GitHub login.
    pub me: String,
    /// For the newest sent round: commits on the PR since its commit, and
    /// whether that commit is still in the PR. Refreshed with the PR.
    pub since_sent: Option<(String, u32, bool)>,
    /// The fix thread wrote its answers for the newest sent round.
    pub answers: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FileStat {
    /// The `git diff --name-status` letter: A, M, D or R.
    pub status: char,
    pub added: u32,
    pub removed: u32,
}

/// Tells the browser hook (`zed8/plannotator-browser`) which worktrees this
/// window has open, so Plannotator sessions there stay in Zed.
pub fn announce_roots(window_key: &str, roots: &[PathBuf]) {
    let dir = crate::paths::windows_dir();
    let body = serde_json::json!({ "pid": std::process::id(), "roots": roots });
    let result = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(format!("{window_key}.json")), body.to_string()));
    if let Err(error) = result {
        log::warn!("plannotator: cannot announce worktrees: {error}");
    }
}

pub fn load_state(roots: Vec<PathBuf>, previous: PanelState) -> PanelState {
    let root = roots.first().cloned();
    let git = |args: &[&str]| -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root.as_ref()?)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let head = git(&["rev-parse", "HEAD"]);
    let mut sessions = session::find_review_sessions(&crate::paths::sessions_dir(), &roots);
    let mut specs = session::find_spec_sessions(&crate::paths::sessions_dir());
    session::add_ancestors(&mut sessions);
    session::add_ancestors(&mut specs);
    specs.retain(|spec| session::spec_belongs_to(spec, &roots));
    let guide = guide::find_guide(&crate::paths::guides_dir(), branch.as_deref(), head.as_deref());
    let mut file_stats = HashMap::default();
    let base = guide
        .as_ref()
        .and_then(|guide| guide.envelope.review.as_ref()?.base.clone())
        .unwrap_or_else(|| "origin/master".to_string());
    if guide.is_some()
        && let Some(merge_base) = git(&["merge-base", "HEAD", &base])
    {
        for line in git(&["diff", "--name-status", &merge_base]).unwrap_or_default().lines() {
            let mut parts = line.split('\t');
            let (Some(status), Some(path)) = (parts.next(), parts.last()) else {
                continue;
            };
            file_stats.entry(path.to_string()).or_insert(FileStat::default()).status =
                status.chars().next().unwrap_or('M');
        }
        for line in git(&["diff", "--numstat", &merge_base]).unwrap_or_default().lines() {
            let mut parts = line.split('\t');
            let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.last())
            else {
                continue;
            };
            let stat = file_stats.entry(path.to_string()).or_insert(FileStat::default());
            stat.added = added.parse().unwrap_or(0);
            stat.removed = removed.parse().unwrap_or(0);
        }
    }
    let pr_is_fresh = branch.is_some()
        && branch == previous.branch
        && head == previous.head
        && previous
            .pr_checked_at
            .is_some_and(|checked| checked.elapsed() < PR_REFRESH_INTERVAL);
    let pr_fetched_at = if pr_is_fresh { previous.pr_fetched_at } else { Some(chrono::Utc::now()) };
    let (pr, pr_checked_at, tools) = if pr_is_fresh {
        (previous.pr, previous.pr_checked_at, previous.tools)
    } else {
        let pr = root.as_deref().and_then(start::find_pull_request);
        let tools = match (root.as_deref(), pr.as_ref()) {
            (Some(root), Some(pr)) => tools::fetch_pr_tools(root, pr.number)
                .map_err(|error| log::warn!("plannotator: no PR labels: {error:#}"))
                .ok(),
            _ => None,
        };
        (pr, Some(Instant::now()), tools)
    };
    let local_n8n = root.as_deref().and_then(tools::local_n8n);
    let linear_issue = tools::linear_issue(
        root.as_deref(),
        branch.as_deref(),
        pr.as_ref().map(|pr| pr.title.as_str()),
    );
    let branch_ahead = pr.is_none().then(|| {
        let ahead = git(&["rev-list", "--count", "origin/master..HEAD"])
            .and_then(|count| count.parse().ok())
            .unwrap_or(0);
        let pushed = git(&["rev-parse", "--abbrev-ref", "@{u}"]).is_some();
        (ahead, pushed)
    });
    let (actions, progress) = match root.as_deref() {
        Some(root) => {
            let dir = crate::paths::actions_dir();
            (actions::load_store(&dir, root), actions::load_progress(&dir, root))
        }
        None => Default::default(),
    };
    let manual_results = actions::review_target(pr.as_ref().map(|pr| pr.number), branch.as_deref())
        .map(|target| crate::paths::reviews_dir().join(target).join("manual-test").join("results.json"))
        .and_then(|path| Some((path.clone(), manual_test_view::load_results(&path)?)));
    let me = root.as_deref().filter(|_| pr.is_some()).map(review_send::me).unwrap_or_default();
    let review_dir = pr.as_ref().map(|pr| review_send::review_dir(pr.number));
    let mut rounds = review_dir.as_deref().map(review_send::load_rounds).unwrap_or_default();
    if let (Some(dir), Some(pr), Some((results_path, results))) = (&review_dir, &pr, &manual_results) {
        sync_manual_items(dir, pr, &mut rounds, results_path, results);
    }
    // Pending and unknown rounds follow GitHub: you may submit or delete a
    // pending review in the browser.
    if !pr_is_fresh && let (Some(root), Some(dir)) = (root.as_deref(), &review_dir) {
        for round in rounds.iter_mut().filter(|round| matches!(round.state, RoundState::Pending | RoundState::Unknown)) {
            let reconciled = review_send::reconcile(root, round.clone());
            if &reconciled != round {
                review_send::save_round(dir, &reconciled).log_err();
                *round = reconciled;
            }
        }
    }
    let last_sent = rounds.iter().rev().find(|round| round.state == RoundState::Sent);
    let since_sent = match (last_sent, &pr, root.as_deref()) {
        (Some(round), Some(pr), Some(root)) => {
            let cached = previous
                .since_sent
                .filter(|(sha, _, _)| pr_is_fresh && *sha == round.sha);
            cached.or_else(|| {
                let (count, in_history) = review_send::compare(root, &round.sha, &pr.head_ref_oid)
                    .map_err(|error| log::warn!("plannotator: cannot compare commits: {error:#}"))
                    .ok()?;
                Some((round.sha.clone(), count, in_history))
            })
        }
        _ => None,
    };
    let answers = match (last_sent, &review_dir) {
        (Some(round), Some(dir)) => dir.join("rounds").join(format!("round-{}.answers.json", round.number)).exists(),
        _ => false,
    };
    PanelState {
        review_dir,
        rounds,
        me,
        since_sent,
        answers,
        branch_ahead,
        tools,
        local_n8n,
        linear_issue,
        manual_results,
        actions,
        progress,
        roots,
        branch,
        sessions,
        guide,
        file_stats,
        specs,
        head,
        pr,
        pr_checked_at,
        pr_fetched_at,
    }
}



/// Keeps the manual items of the open round equal to the flagged cases. A
/// flagged case starts round 1 when no round is open.
fn sync_manual_items(
    dir: &std::path::Path,
    pr: &PullRequest,
    rounds: &mut Vec<Round>,
    results_path: &std::path::Path,
    results: &ManualTestResults,
) {
    let flagged = results.cases.iter().any(|case| case.to_post());
    let open = rounds.last().is_some_and(Round::is_open);
    if !open && !flagged {
        return;
    }
    if !open {
        if rounds.last().is_some_and(|round| round.state != RoundState::Sent) {
            // A round that is sending, failed or pending takes no new items.
            return;
        }
        let number = rounds.last().map_or(1, |round| round.number + 1);
        rounds.push(Round::new(pr.number, number, pr.head_ref_oid.clone()));
    }
    let Some(round) = rounds.last_mut() else {
        return;
    };
    let results_dir = results_path.parent().unwrap_or(dir);
    let copies = review_send::round_files(dir, round.number);
    let before = round.clone();
    review_round::sync_manual(round, &review_send::manual_cases(results, results_dir, &copies));
    if *round != before {
        review_send::save_round(dir, round).log_err();
    }
}
