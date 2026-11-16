//! Files and GitHub calls for the review draft: the round files in the PR's
//! review folder, and the send to GitHub step by step. Each step saves the
//! round, so a crash leaves a round that reconcile can finish.

use crate::{
    manual_test_view::ManualTestResults,
    review_round::{self, GitHubReview, ManualCase, Plan, PlanContext, Round, RoundState, SendEvent},
};
use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
};

/// The review folder of a PR, shared with the `pr-review` and `manual-test` skills.
pub fn review_dir(pr: u64) -> PathBuf {
    crate::paths::reviews_dir().join(format!("pr-{pr}"))
}

/// Every round of the PR, oldest first.
pub fn load_rounds(review_dir: &Path) -> Vec<Round> {
    let Ok(entries) = std::fs::read_dir(review_dir.join("rounds")) else {
        return Vec::new();
    };
    let mut rounds: Vec<Round> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            if !(name.starts_with("round-") && name.ends_with(".json") && name.matches('.').count() == 1) {
                return None;
            }
            let text = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&text)
                .map_err(|error| log::warn!("plannotator: cannot read {}: {error}", path.display()))
                .ok()
        })
        .collect();
    rounds.sort_by_key(|round| round.number);
    rounds
}

pub fn save_round(review_dir: &Path, round: &Round) -> Result<()> {
    let path = review_round::round_path(review_dir, round.number);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write and rename, so a crash never leaves half a round.
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(round)?)?;
    std::fs::rename(&temp, &path).with_context(|| format!("save {}", path.display()))
}

/// The folder for the round's own copies of screenshots and its comment body.
pub fn round_files(review_dir: &Path, number: u32) -> PathBuf {
    review_dir.join("rounds").join(format!("round-{number}"))
}

/// The flagged cases of the manual test, with their screenshots copied into
/// the round folder.
pub fn manual_cases(results: &ManualTestResults, results_dir: &Path, copies: &Path) -> Vec<ManualCase> {
    results
        .cases
        .iter()
        .map(|case| {
            let attachments = if case.include && case.posted_at.is_empty() {
                case.attached()
                    .iter()
                    .filter_map(|file| copy_into(&results_dir.join(file), &copies.join(&case.id)))
                    .collect()
            } else {
                Vec::new()
            };
            ManualCase {
                id: case.id.clone(),
                title: format!("{} · {}", case.id, case.title),
                include: case.include,
                posted: !case.posted_at.is_empty(),
                body: case_body(case),
                attachments,
            }
        })
        .collect()
}

fn copy_into(source: &Path, folder: &Path) -> Option<PathBuf> {
    let target = folder.join(source.file_name()?);
    if target.exists() {
        return Some(target);
    }
    std::fs::create_dir_all(folder)
        .and_then(|()| std::fs::copy(source, &target))
        .map_err(|error| log::warn!("plannotator: cannot copy {}: {error}", source.display()))
        .ok()?;
    Some(target)
}

fn case_body(case: &crate::manual_test_view::TestCase) -> String {
    let mut parts = vec![format!("### {} · {}", case.id, case.title)];
    let outcome = [("Expected", &case.expected), ("Observed", &case.observed)]
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(label, text)| format!("{label}: {text}"))
        .collect::<Vec<_>>()
        .join("<br>");
    let note = case.note.trim();
    if note.is_empty() {
        parts.push(outcome.clone());
    } else {
        parts.push(note.to_string());
    }
    if !case.steps.is_empty() {
        let mut details = "<details><summary>Steps</summary>\n\n".to_string();
        for (ix, step) in case.steps.iter().enumerate() {
            details.push_str(&format!("{}. {step}\n", ix + 1));
        }
        if !note.is_empty() && !outcome.is_empty() {
            details.push_str(&format!("\n{outcome}\n"));
        }
        details.push_str("\n</details>");
        parts.push(details);
    }
    parts.retain(|part| !part.is_empty());
    parts.join("\n\n")
}

fn gh(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("gh").current_dir(root).args(args).output().context("run gh")?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim().lines().last().unwrap_or("gh failed")
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Your GitHub login, asked once per run of Zed.
pub fn me(root: &Path) -> String {
    static LOGIN: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let cell = LOGIN.get_or_init(|| Mutex::new(None));
    let Ok(mut login) = cell.lock() else {
        return String::new();
    };
    if login.is_none() {
        *login = gh(root, &["api", "user", "--jq", ".login"])
            .map(|text| text.trim().to_string())
            .map_err(|error| log::warn!("plannotator: no GitHub login: {error:#}"))
            .ok();
    }
    login.clone().unwrap_or_default()
}

/// Whether this `gh` can upload screenshots (`--attach`, gh 2.99 or newer).
pub fn gh_attaches() -> bool {
    static ATTACHES: OnceLock<bool> = OnceLock::new();
    *ATTACHES.get_or_init(|| {
        Command::new("gh")
            .args(["pr", "comment", "--help"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("--attach"))
    })
}

#[derive(Deserialize)]
struct CompareResponse {
    status: String,
    #[serde(default)]
    ahead_by: u32,
}

/// How the PR head relates to the round's commit: the commits since, and
/// whether the commit is still in the PR's history (no force-push removed it).
pub fn compare(root: &Path, sha: &str, head: &str) -> Result<(u32, bool)> {
    if sha == head {
        return Ok((0, true));
    }
    let json = gh(root, &["api", &format!("repos/{{owner}}/{{repo}}/compare/{sha}...{head}")])?;
    let compare: CompareResponse = serde_json::from_str(&json)?;
    let in_history = matches!(compare.status.as_str(), "ahead" | "identical");
    Ok((compare.ahead_by, in_history))
}

/// The PR diff at the round's commit: from git when the worktree has the
/// commit, else from GitHub (the PR head).
fn patch_at(root: &Path, pr: u64, sha: &str, base: &str) -> Result<String> {
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git").current_dir(root).args(args).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let base = if base.is_empty() { "master" } else { base };
    if let Some(merge_base) = git(&["merge-base", &format!("origin/{base}"), sha])
        && let Some(patch) = git(&["diff", "--no-color", "--no-ext-diff", merge_base.trim(), sha])
    {
        return Ok(patch);
    }
    gh(root, &["pr", "diff", &pr.to_string(), "--color", "never"])
}

pub fn my_reviews(root: &Path, pr: u64) -> Result<Vec<GitHubReview>> {
    let json = gh(root, &["api", "--paginate", "--slurp", &format!("repos/{{owner}}/{{repo}}/pulls/{pr}/reviews")])?;
    let pages: Vec<Vec<GitHubReview>> = serde_json::from_str(&json)?;
    Ok(pages.into_iter().flatten().collect())
}

#[derive(Deserialize)]
struct IssueComment {
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
}

fn find_manual_comment(root: &Path, pr: u64, round: &Round) -> Option<String> {
    let marker = format!("<!-- zed8 pr-{} round-{} manual -->", round.pr, round.number);
    let json = gh(root, &["api", "--paginate", "--slurp", &format!("repos/{{owner}}/{{repo}}/issues/{pr}/comments")]).ok()?;
    let pages: Vec<Vec<IssueComment>> = serde_json::from_str(&json).ok()?;
    pages.into_iter().flatten().find(|comment| comment.body.contains(&marker)).map(|comment| comment.html_url)
}

#[derive(Deserialize)]
struct PostedReview {
    id: u64,
    #[serde(default)]
    html_url: String,
}

#[derive(Deserialize)]
struct ReviewComment {
    id: u64,
}

fn post_json(root: &Path, method: &str, endpoint: &str, body: &serde_json::Value, scratch: &Path) -> Result<String> {
    std::fs::create_dir_all(scratch)?;
    let file = scratch.join("request.json");
    std::fs::write(&file, serde_json::to_vec(body)?)?;
    let file = file.display().to_string();
    gh(root, &["api", "--method", method, endpoint, "--input", &file])
}

/// Looks for the round on GitHub by its marker, and sets its state from what
/// it finds. Returns the round unchanged when GitHub cannot be asked.
pub fn reconcile(root: &Path, mut round: Round) -> Round {
    let reviews = match my_reviews(root, round.pr) {
        Ok(reviews) => reviews,
        Err(error) => {
            log::warn!("plannotator: cannot reconcile round {}: {error:#}", round.number);
            return round;
        }
    };
    match review_round::find_review(&round, &reviews) {
        Some(review) => {
            let pending = review.state == "PENDING";
            round.state = review_round::next(round.state, &SendEvent::FoundOnGitHub { pending });
            round.sent.review_id = Some(review.id);
            round.sent.review_url = review.html_url.clone();
            if !pending && round.sent.at.is_empty() {
                round.sent.at = review.submitted_at.clone().unwrap_or_default();
                round.sent.decision = Some(match review.state.as_str() {
                    "APPROVED" => review_round::Decision::Approve,
                    "CHANGES_REQUESTED" => review_round::Decision::RequestChanges,
                    _ => review_round::Decision::Comment,
                });
            }
        }
        None => round.state = review_round::next(round.state, &SendEvent::NotOnGitHub),
    }
    round
}

/// Sends the round to GitHub: the manual test comment, then the review, then
/// the replies on carried threads. Calls `save` after each step.
pub fn send_to_github(
    root: &Path,
    review_dir: &Path,
    pr_head: &str,
    base: &str,
    mut round: Round,
    pending: bool,
    save: &dyn Fn(&Round),
) -> Round {
    // A retry never posts twice: GitHub may already have this round.
    if matches!(round.state, RoundState::Failed | RoundState::Partial | RoundState::Unknown)
        && let Ok(reviews) = my_reviews(root, round.pr)
        && let Some(review) = review_round::find_review(&round, &reviews)
    {
        let pending_found = review.state == "PENDING";
        round.sent.review_id = Some(review.id);
        round.sent.review_url = review.html_url.clone();
        if round.sent.replied.len() >= round.included().filter(|item| item.carried.is_some()).count() {
            round.state = if pending_found { RoundState::Pending } else { RoundState::Sent };
            round.error.clear();
            save(&round);
            return round;
        }
    }
    round.state = review_round::next(round.state, &SendEvent::Start);
    round.error.clear();
    save(&round);

    let files = round_files(review_dir, round.number);
    let fail = |mut round: Round, error: anyhow::Error, posted_something: bool| {
        let event = if posted_something { SendEvent::PartlyFailed } else { SendEvent::Failed };
        round.state = review_round::next(round.state, &event);
        round.error = format!("{error:#}");
        save(&round);
        round
    };

    let (_, sha_on_github) = compare(root, &round.sha, pr_head).unwrap_or((0, false));
    let patch = match patch_at(root, round.pr, &round.sha, base) {
        Ok(patch) => patch,
        Err(error) => return fail(round, error.context("read the PR diff"), false),
    };
    let commentable = review_round::commentable_lines(&patch);
    let plan: Plan = review_round::plan(&round, &PlanContext { commentable: &commentable, sha_on_github, pending });

    if let Some(manual) = plan.manual.as_ref().filter(|_| round.sent.manual_comment_url.is_empty()) {
        if let Some(url) = find_manual_comment(root, round.pr, &round) {
            round.sent.manual_comment_url = url;
        } else {
            let body_file = files.join("manual-comment.md");
            let posted = std::fs::create_dir_all(&files)
                .map_err(anyhow::Error::from)
                .and_then(|()| Ok(std::fs::write(&body_file, &manual.body)?))
                .and_then(|()| crate::tools::post_pr_comment(root, round.pr, &body_file, &manual.attachments));
            match posted {
                Ok(url) => round.sent.manual_comment_url = url,
                Err(error) => return fail(round, error.context("post the manual test cases"), false),
            }
        }
        save(&round);
    }

    let posted_before = !round.sent.manual_comment_url.is_empty();
    if round.sent.review_id.is_none() {
        let mut review = plan.review.clone();
        if posted_before {
            review.body = format!(
                "Manual test cases with screenshots: {}\n\n{}",
                round.sent.manual_comment_url, review.body
            );
        }
        let result = post_json(
            root,
            "POST",
            &format!("repos/{{owner}}/{{repo}}/pulls/{}/reviews", round.pr),
            &serde_json::to_value(&review).unwrap_or_default(),
            &files,
        )
        .and_then(|json| Ok(serde_json::from_str::<PostedReview>(&json)?));
        match result {
            Ok(posted) => {
                round.sent.review_id = Some(posted.id);
                round.sent.review_url = posted.html_url;
                if let Ok(json) = gh(
                    root,
                    &["api", &format!("repos/{{owner}}/{{repo}}/pulls/{}/reviews/{}/comments", round.pr, posted.id)],
                ) && let Ok(comments) = serde_json::from_str::<Vec<ReviewComment>>(&json)
                {
                    for (key, comment) in plan.inline_keys.iter().zip(comments) {
                        round.sent.comment_ids.insert(key.clone(), comment.id);
                    }
                }
                save(&round);
            }
            Err(error) => return fail(round, error.context("post the review"), posted_before),
        }
    }

    for reply in &plan.replies {
        if round.sent.replied.contains(&reply.key) {
            continue;
        }
        let result = post_json(
            root,
            "POST",
            &format!("repos/{{owner}}/{{repo}}/pulls/{}/comments/{}/replies", round.pr, reply.comment_id),
            &serde_json::json!({ "body": reply.body }),
            &files,
        );
        match result {
            Ok(_) => {
                round.sent.replied.insert(reply.key.clone());
                save(&round);
            }
            Err(error) => return fail(round, error.context("reply on a thread"), true),
        }
    }

    round.state = review_round::next(round.state, if pending { &SendEvent::SavedPending } else { &SendEvent::Succeeded });
    if !pending {
        round.sent.at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        round.sent.decision = Some(round.decision);
    }
    save(&round);
    round
}

/// Submits a pending review with the round's decision.
pub fn submit_pending(root: &Path, review_dir: &Path, mut round: Round, save: &dyn Fn(&Round)) -> Round {
    let Some(review_id) = round.sent.review_id else {
        return round;
    };
    let event = match round.decision {
        review_round::Decision::Comment => "COMMENT",
        review_round::Decision::Approve => "APPROVE",
        review_round::Decision::RequestChanges => "REQUEST_CHANGES",
    };
    let result = post_json(
        root,
        "POST",
        &format!("repos/{{owner}}/{{repo}}/pulls/{}/reviews/{review_id}/events", round.pr),
        &serde_json::json!({ "event": event }),
        &round_files(review_dir, round.number),
    );
    match result {
        Ok(_) => {
            round.state = RoundState::Sent;
            round.error.clear();
            round.sent.at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            round.sent.decision = Some(round.decision);
        }
        Err(error) => round.error = format!("Submit failed: {error:#}"),
    }
    save(&round);
    round
}
