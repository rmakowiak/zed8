//! The review actions of the Review panel: Agent review, Manual test and
//! Guided review. Each one runs in its own Claude Code terminal thread.
//!
//! Zed keeps the thread of each action and the hidden actions in
//! `~/.plannotator/zed-actions/<worktree>.json`. Each skill writes its
//! progress to `~/.plannotator/zed-actions/<worktree>/<action>.json`, in the
//! format of `ActionProgress`. Zed passes that path with `--state`.

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReviewAction {
    AgentReview,
    ManualTest,
    GuidedReview,
}

impl ReviewAction {
    pub const ALL: [ReviewAction; 3] = [Self::AgentReview, Self::ManualTest, Self::GuidedReview];

    pub fn key(self) -> &'static str {
        match self {
            Self::AgentReview => "agent-review",
            Self::ManualTest => "manual-test",
            Self::GuidedReview => "guided-review",
        }
    }

    /// The name of the card and of the Claude thread.
    pub fn title(self) -> &'static str {
        match self {
            Self::AgentReview => "Agent review",
            Self::ManualTest => "Manual test",
            Self::GuidedReview => "Guided review",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::AgentReview => "Claude reviews the PR: plan, lenses, findings. You approve each gate.",
            Self::ManualTest => "Claude writes a test charter, starts n8n and runs it in Chrome.",
            Self::GuidedReview => "Claude explains the change in chapters. You review the code.",
        }
    }

    /// The steps before Claude reports its own, so a new card shows them.
    pub fn default_steps(self) -> &'static [&'static str] {
        match self {
            Self::AgentReview => &["Plan", "Lenses", "Findings", "Post"],
            Self::ManualTest => &["Charter", "Instance", "Run", "Results"],
            Self::GuidedReview => &["Guide", "Chapters", "Feedback"],
        }
    }

    /// Agent review and Guided review read the GitHub PR. Manual test runs
    /// on the local branch.
    pub fn needs_pr(self) -> bool {
        !matches!(self, Self::ManualTest)
    }

    /// The first prompt of the thread.
    pub fn command(self, state_file: &Path) -> String {
        let skill = match self {
            Self::AgentReview => "/pr-review",
            Self::ManualTest => "/manual-test",
            Self::GuidedReview => "/zed-guided-review",
        };
        format!("{skill} --state {}", state_file.display())
    }
}

/// What Zed keeps for one worktree.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionStore {
    /// The terminal thread of each started action, by action key.
    #[serde(default)]
    pub threads: BTreeMap<String, String>,
    /// Hidden action keys, by PR number. Hiding is per PR.
    #[serde(default)]
    pub hidden: BTreeMap<String, BTreeSet<String>>,
    /// The `endedAt` of the manual test results that you opened, by target.
    /// Newer results go back into Needs you.
    #[serde(default)]
    pub seen_results: BTreeMap<String, String>,
    /// Flags picked for a branch before its PR exists, by branch. Create PR
    /// adds them as `flag:` labels.
    #[serde(default)]
    pub pending_flags: BTreeMap<String, Vec<String>>,
}

impl ActionStore {
    pub fn thread(&self, action: ReviewAction) -> Option<&str> {
        self.threads.get(action.key()).map(String::as_str)
    }

    pub fn is_hidden(&self, action: ReviewAction, pr: Option<u64>) -> bool {
        pr.and_then(|pr| self.hidden.get(&pr.to_string()))
            .is_some_and(|keys| keys.contains(action.key()))
    }

    pub fn set_hidden(&mut self, action: ReviewAction, pr: u64, hidden: bool) {
        let keys = self.hidden.entry(pr.to_string()).or_default();
        if hidden {
            keys.insert(action.key().to_string());
        } else {
            keys.remove(action.key());
        }
        if keys.is_empty() {
            self.hidden.remove(&pr.to_string());
        }
    }
}

/// The progress that a skill writes. Every field is optional, so a skill
/// can write only what it knows.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionProgress {
    /// The step names, such as Plan, Lenses, Findings, Post.
    #[serde(default)]
    pub steps: Vec<String>,
    /// The index of the current step in `steps`.
    pub step: Option<usize>,
    /// A short text for the current step, such as "Lenses 2/4".
    pub detail: Option<String>,
    /// One line under the card name.
    pub summary: Option<String>,
    /// Claude waits for an answer from the user.
    #[serde(default)]
    pub waiting: bool,
    /// The action is finished.
    #[serde(default)]
    pub done: bool,
}

/// The thread key of Create PR in `ActionStore::threads`. It is not a card.
pub const CREATE_PR_KEY: &str = "create-pr";

/// The first prompt of the Create PR thread.
pub fn create_pr_prompt(ticket: Option<&str>, flags: &[String]) -> String {
    let mut prompt = String::from(
        "Open a GitHub pull request for the branch that is checked out here. Push the branch first. \
         Write the title and the body from the commits and the diff, and follow the PR rules of this repo.",
    );
    if let Some(ticket) = ticket {
        prompt.push_str(&format!(" The Linear ticket is {ticket}: read it and link it in the body."));
    }
    if !flags.is_empty() {
        let labels: Vec<String> = flags.iter().map(|flag| format!("flag:{flag}")).collect();
        prompt.push_str(&format!(
            " Add the labels {} when you open the PR, so the first preview has these flags.",
            labels.join(", ")
        ));
    }
    prompt.push_str(" Show me the title, the body and the labels, and wait for my yes before you open the PR.");
    prompt
}

/// A Linear key in a branch name, such as `CONTEXT-433` in `context-433-skill-sync`.
pub fn ticket_in_branch(branch: &str) -> Option<String> {
    let parts: Vec<&str> = branch.rsplit('/').next()?.split('-').collect();
    parts.windows(2).find_map(|pair| {
        let (team, number) = (pair[0], pair[1]);
        // Team keys such as CONTEXT have up to ten characters.
        let team_ok = (2..=10).contains(&team.len())
            && team.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && team.chars().all(|c| c.is_ascii_alphanumeric());
        let number_ok = !number.is_empty() && number.len() <= 6 && number.chars().all(|c| c.is_ascii_digit());
        (team_ok && number_ok).then(|| format!("{}-{number}", team.to_ascii_uppercase()))
    })
}

/// The name of the review folder of a branch: `pr-<number>` with a PR, else
/// the branch with `/` replaced by `-`. The skills use the same rule.
pub fn review_target(pr: Option<u64>, branch: Option<&str>) -> Option<String> {
    match (pr, branch) {
        (Some(pr), _) => Some(format!("pr-{pr}")),
        (None, Some(branch)) if branch != "HEAD" => Some(branch.replace('/', "-")),
        _ => None,
    }
}

/// A worktree path as a file name: every character that is not a letter or
/// a digit becomes `-`.
fn worktree_key(root: &Path) -> String {
    let text = root.display().to_string();
    let key: String = text.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    key.trim_matches('-').to_string()
}

pub fn store_path(dir: &Path, root: &Path) -> PathBuf {
    dir.join(format!("{}.json", worktree_key(root)))
}

pub fn progress_path(dir: &Path, root: &Path, action: ReviewAction) -> PathBuf {
    dir.join(worktree_key(root)).join(format!("{}.json", action.key()))
}

pub fn load_store(dir: &Path, root: &Path) -> ActionStore {
    let path = store_path(dir, root);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            log::warn!("plannotator: cannot read {}: {error}", path.display());
            ActionStore::default()
        }),
        Err(_) => ActionStore::default(),
    }
}

pub fn save_store(dir: &Path, root: &Path, store: &ActionStore) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(store_path(dir, root), serde_json::to_vec_pretty(store)?)?;
    Ok(())
}

pub fn load_progress(dir: &Path, root: &Path) -> BTreeMap<ReviewAction, ActionProgress> {
    ReviewAction::ALL
        .into_iter()
        .filter_map(|action| {
            let bytes = std::fs::read(progress_path(dir, root, action)).ok()?;
            let progress = serde_json::from_slice(&bytes)
                .map_err(|error| log::warn!("plannotator: bad progress of {}: {error}", action.key()))
                .ok()?;
            Some((action, progress))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips_threads_and_hidden_actions() {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new("/Users/me/workspace/n8n-worktrees/n8n/pr-1/n8n");
        let mut store = ActionStore::default();
        store.threads.insert("agent-review".into(), "thread-1".into());
        store.set_hidden(ReviewAction::GuidedReview, 43508, true);
        save_store(dir.path(), root, &store).unwrap();

        let loaded = load_store(dir.path(), root);
        assert_eq!(loaded.thread(ReviewAction::AgentReview), Some("thread-1"));
        assert!(loaded.is_hidden(ReviewAction::GuidedReview, Some(43508)));
        assert!(!loaded.is_hidden(ReviewAction::GuidedReview, Some(1)));
        assert!(!loaded.is_hidden(ReviewAction::GuidedReview, None));
    }

    #[test]
    fn reads_the_progress_that_a_skill_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new("/tmp/worktree");
        let path = progress_path(dir.path(), root, ReviewAction::ManualTest);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"steps":["Charter","Instance","Run","Results"],"step":2,"detail":"Run 3/8","waiting":false,"extra":1}"#,
        )
        .unwrap();

        let progress = load_progress(dir.path(), root);
        let manual = &progress[&ReviewAction::ManualTest];
        assert_eq!(manual.step, Some(2));
        assert_eq!(manual.detail.as_deref(), Some("Run 3/8"));
        assert!(!progress.contains_key(&ReviewAction::AgentReview));
    }

    #[test]
    fn finds_the_ticket_in_a_branch_name() {
        assert_eq!(ticket_in_branch("context-433-skill-sync").as_deref(), Some("CONTEXT-433"));
        assert_eq!(ticket_in_branch("roman/ado-12-fix").as_deref(), Some("ADO-12"));
        assert_eq!(ticket_in_branch("fix-the-thing"), None);
        assert!(create_pr_prompt(Some("CONTEXT-433"), &["a".into()]).contains("flag:a"));
    }

    #[test]
    fn the_command_passes_the_progress_file() {
        let command = ReviewAction::AgentReview.command(Path::new("/tmp/a.json"));
        assert_eq!(command, "/pr-review --state /tmp/a.json");
    }
}
