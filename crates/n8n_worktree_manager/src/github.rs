use crate::Manager;
use crate::system::Command;
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The PR of a worktree's branch on GitHub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatus {
    pub number: u32,
    pub url: String,
    /// OPEN, MERGED or CLOSED.
    pub state: String,
    pub checks: Checks,
}

/// All checks of a PR, summed up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Checks {
    Passing,
    Failing,
    Pending,
    None,
}

impl Manager {
    /// The PR of the worktree: of the branch its checkout is on, or of the
    /// branch it was created with. An open PR wins over a merged one, and a
    /// merged one over a closed one.
    pub fn pr_status(&self, name: &str) -> Result<Option<PrStatus>> {
        let meta = self.show(name)?;
        let current = self.system.run(
            &Command::new("git")
                .arg("-C")
                .arg(self.checkout_dir(&meta.name).to_string_lossy())
                .args(["branch", "--show-current"]),
        )?;
        let mut branches = vec![current.stdout.trim().to_string(), meta.branch];
        branches.retain(|branch| !branch.is_empty());
        branches.dedup();
        let mut prs = Vec::new();
        for branch in &branches {
            prs.extend(self.prs_of_branch(branch)?);
        }
        let rank = |pr: &PrStatus| match pr.state.as_str() {
            "OPEN" => 0,
            "MERGED" => 1,
            _ => 2,
        };
        // A stable sort keeps the newest first within a state.
        prs.sort_by_key(rank);
        Ok(prs.into_iter().next())
    }

    /// The PRs of a branch, newest first.
    fn prs_of_branch(&self, branch: &str) -> Result<Vec<PrStatus>> {
        let output = self.system.run(&Command::new("gh").args([
            "pr",
            "list",
            "--repo",
            "n8n-io/n8n",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,url,state,statusCheckRollup",
            "--limit",
            "5",
        ]))?;
        if !output.success {
            bail!("gh pr list failed for {branch}: {}", output.stderr.trim());
        }
        let prs: Vec<Value> = match output.stdout.trim() {
            "" => Vec::new(),
            text => serde_json::from_str(text)
                .with_context(|| format!("reading gh pr list for {branch}"))?,
        };
        Ok(prs
            .iter()
            .map(|pr| PrStatus {
                number: pr["number"]
                    .as_u64()
                    .and_then(|number| u32::try_from(number).ok())
                    .unwrap_or_default(),
                url: pr["url"].as_str().unwrap_or_default().to_string(),
                state: pr["state"].as_str().unwrap_or_default().to_string(),
                checks: sum_checks(
                    pr["statusCheckRollup"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                ),
            })
            .collect())
    }
}

/// A failed check wins, then a check that still runs. Skipped and neutral
/// checks count as passed.
fn sum_checks(checks: &[Value]) -> Checks {
    if checks.is_empty() {
        return Checks::None;
    }
    let mut pending = false;
    for check in checks {
        // A CheckRun has status and conclusion. A StatusContext has state.
        let result = check["conclusion"]
            .as_str()
            .filter(|conclusion| !conclusion.is_empty())
            .or_else(|| check["state"].as_str())
            .unwrap_or_default();
        match result {
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => return Checks::Failing,
            "SUCCESS" | "SKIPPED" | "NEUTRAL" => {}
            _ => pending = true,
        }
    }
    if pending { Checks::Pending } else { Checks::Passing }
}
