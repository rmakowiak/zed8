//! The Reviews tab: the open n8n PRs that you review or wrote, grouped by
//! who they wait on. The state of each row comes from the review draft's
//! round files in `~/workspace/n8n-reviews/pr-<n>/rounds/`.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::{path::Path, process::Command};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewGroup {
    WaitsOnYou,
    WaitsOnAuthor,
    Yours,
}

impl ReviewGroup {
    pub fn title(self) -> &'static str {
        match self {
            ReviewGroup::WaitsOnYou => "Waits on you",
            ReviewGroup::WaitsOnAuthor => "Waits on the author",
            ReviewGroup::Yours => "Your PRs",
        }
    }
}

/// Why the PR is in the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// GitHub asks you for a review, now.
    Requested,
    /// You reviewed it before, and GitHub does not ask you now.
    Reviewed,
    Authored,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReviewRow {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub group: ReviewGroup,
    pub state: String,
    pub branch: String,
    /// The local worktree on the PR's branch.
    pub worktree: Option<String>,
}

/// The parts of a round file that the tab reads.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RoundBrief {
    pub number: u32,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub items: Vec<ItemBrief>,
    #[serde(default)]
    pub sent: SentBrief,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemBrief {
    #[serde(default)]
    pub include: bool,
    #[serde(default)]
    pub carried: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SentBrief {
    #[serde(default)]
    pub decision: Option<String>,
}

fn decision_text(decision: Option<&str>) -> &'static str {
    match decision {
        Some("approve") => "approved",
        Some("requestChanges") => "changes requested",
        Some("comment") => "commented",
        _ => "sent to the agent",
    }
}

/// Where a PR goes in the tab, and the line under its title.
pub fn classify(relation: Relation, rounds: &[RoundBrief]) -> (ReviewGroup, String) {
    let last = rounds.last();
    let included = |round: &RoundBrief| round.items.iter().filter(|item| item.include).count();
    let carried = |round: &RoundBrief| round.items.iter().filter(|item| item.include && item.carried.is_some()).count();
    let sent = rounds.iter().rev().find(|round| round.state == "sent");
    if let Some(round) = last {
        match round.state.as_str() {
            "failed" | "partial" | "unknown" => {
                return (ReviewGroup::WaitsOnYou, format!("Round {} · send failed, retry in the Review panel", round.number));
            }
            "pending" => return (ReviewGroup::WaitsOnYou, format!("Round {} · pending on GitHub, submit it", round.number)),
            _ => {}
        }
    }
    match relation {
        Relation::Authored => match (last, sent) {
            (Some(round), _) if round.state == "draft" && carried(round) > 0 => {
                (ReviewGroup::Yours, format!("Verify done · {} open", carried(round)))
            }
            (_, Some(sent)) if sent.destination.as_deref() == Some("agent") => {
                (ReviewGroup::Yours, format!("Round {} · with the fix thread", sent.number))
            }
            (Some(round), _) if round.state == "draft" && included(round) > 0 => {
                (ReviewGroup::Yours, format!("Round {} · {} items in draft", round.number, included(round)))
            }
            _ => (ReviewGroup::Yours, "In review".to_string()),
        },
        Relation::Requested => match (last, sent) {
            (None, _) => (ReviewGroup::WaitsOnYou, "Review asked · not started".to_string()),
            (Some(round), _) if round.state == "draft" && carried(round) > 0 => (
                ReviewGroup::WaitsOnYou,
                format!("Round {} · Verify done · {} carried over", round.number, carried(round)),
            ),
            (Some(round), Some(sent)) if round.state == "sent" => {
                (ReviewGroup::WaitsOnYou, format!("Asked again after round {}", sent.number))
            }
            (Some(round), _) => (ReviewGroup::WaitsOnYou, format!("Round {} · {} items in draft", round.number, included(round))),
        },
        Relation::Reviewed => match (last, sent) {
            (Some(round), _) if round.state == "draft" && carried(round) > 0 => (
                ReviewGroup::WaitsOnYou,
                format!("Round {} · Verify done · {} carried over", round.number, carried(round)),
            ),
            (_, Some(sent)) => (
                ReviewGroup::WaitsOnAuthor,
                format!("Round {} {}", sent.number, decision_text(sent.sent.decision.as_deref())),
            ),
            _ => (ReviewGroup::WaitsOnAuthor, "Your review is out".to_string()),
        },
    }
}

pub fn load_rounds(review_dir: &Path) -> Vec<RoundBrief> {
    let Ok(entries) = std::fs::read_dir(review_dir.join("rounds")) else {
        return Vec::new();
    };
    let mut rounds: Vec<RoundBrief> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            if !(name.starts_with("round-") && name.ends_with(".json") && name.matches('.').count() == 1) {
                return None;
            }
            serde_json::from_slice(&std::fs::read(&path).ok()?).ok()
        })
        .collect();
    rounds.sort_by_key(|round| round.number);
    rounds
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchHit {
    number: u64,
    title: String,
    url: String,
    #[serde(default)]
    author: Option<Author>,
    #[serde(default)]
    head_ref_name: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

fn search(query: &str) -> Result<Vec<SearchHit>> {
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            "n8n-io/n8n",
            "--state",
            "open",
            "--search",
            query,
            "--json",
            "number,title,url,author,headRefName",
            "--limit",
            "30",
        ])
        .output()
        .context("run gh pr list")?;
    anyhow::ensure!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr).trim());
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// The rows of the tab, from GitHub and the round files.
/// `worktrees` are the local worktrees as (name, branch).
pub fn load(reviews_dir: &Path, worktrees: &[(String, String)]) -> Result<Vec<ReviewRow>> {
    let requested = search("review-requested:@me")?;
    let reviewed = search("reviewed-by:@me")?;
    let authored = search("author:@me")?;
    let mut rows: Vec<ReviewRow> = Vec::new();
    let sources = [
        (Relation::Requested, requested),
        (Relation::Authored, authored),
        (Relation::Reviewed, reviewed),
    ];
    for (relation, hits) in sources {
        for hit in hits {
            if rows.iter().any(|row| row.number == hit.number) {
                continue;
            }
            let rounds = load_rounds(&reviews_dir.join(format!("pr-{}", hit.number)));
            let (group, state) = classify(relation, &rounds);
            rows.push(ReviewRow {
                number: hit.number,
                title: hit.title,
                url: hit.url,
                author: hit.author.map(|author| author.login).unwrap_or_default(),
                group,
                state,
                worktree: worktrees
                    .iter()
                    .find(|(_, branch)| *branch == hit.head_ref_name)
                    .map(|(name, _)| name.clone()),
                branch: hit.head_ref_name,
            });
        }
    }
    rows.sort_by_key(|row| row.group);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(number: u32, state: &str, items: &[(bool, bool)]) -> RoundBrief {
        RoundBrief {
            number,
            state: state.into(),
            destination: None,
            items: items
                .iter()
                .map(|(include, carried)| ItemBrief { include: *include, carried: carried.then(|| serde_json::json!({})) })
                .collect(),
            sent: SentBrief { decision: (state == "sent").then(|| "requestChanges".into()) },
        }
    }

    #[test]
    fn a_new_review_request_waits_on_you() {
        assert_eq!(classify(Relation::Requested, &[]), (ReviewGroup::WaitsOnYou, "Review asked · not started".into()));
    }

    #[test]
    fn a_sent_round_waits_on_the_author_until_verify_carries_items() {
        let sent = round(1, "sent", &[(true, false)]);
        assert_eq!(
            classify(Relation::Reviewed, std::slice::from_ref(&sent)),
            (ReviewGroup::WaitsOnAuthor, "Round 1 changes requested".into())
        );
        let carried = round(2, "draft", &[(true, true), (false, true)]);
        assert_eq!(
            classify(Relation::Reviewed, &[sent.clone(), carried]),
            (ReviewGroup::WaitsOnYou, "Round 2 · Verify done · 1 carried over".into())
        );
        assert_eq!(classify(Relation::Requested, &[sent]), (ReviewGroup::WaitsOnYou, "Asked again after round 1".into()));
    }

    #[test]
    fn a_failed_send_waits_on_you_whatever_the_relation() {
        for relation in [Relation::Requested, Relation::Reviewed, Relation::Authored] {
            let (group, state) = classify(relation, &[round(1, "failed", &[(true, false)])]);
            assert_eq!(group, ReviewGroup::WaitsOnYou);
            assert!(state.contains("send failed"));
        }
    }

    #[test]
    fn your_pr_with_the_fix_thread_says_so() {
        let mut sent = round(1, "sent", &[(true, false)]);
        sent.destination = Some("agent".into());
        assert_eq!(classify(Relation::Authored, &[sent]), (ReviewGroup::Yours, "Round 1 · with the fix thread".into()));
        assert_eq!(classify(Relation::Authored, &[]), (ReviewGroup::Yours, "In review".into()));
    }
}
