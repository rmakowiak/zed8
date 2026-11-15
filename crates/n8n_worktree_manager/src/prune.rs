use crate::Manager;
use crate::system::Command;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// A worktree whose branch has a merged PR on GitHub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MergedWorktree {
    pub name: String,
    pub branch: String,
    pub pr: u32,
    pub url: String,
    /// Why teardown would lose work, or `None` when it is safe.
    pub skip: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergedPr {
    number: u32,
    head_ref_oid: String,
    url: String,
}

impl Manager {
    /// The worktrees whose branch has a merged PR, sorted by name. Each one
    /// says whether a teardown is safe.
    pub fn merged(&self) -> Result<Vec<MergedWorktree>> {
        let mut merged = Vec::new();
        for meta in self.list()? {
            let checkout = self.checkout_dir(&meta.name).to_string_lossy().into_owned();
            let git = |args: &[&str]| {
                Command::new("git")
                    .arg("-C")
                    .arg(checkout.clone())
                    .args(args.iter().copied())
            };
            // The branch from git, because it can change after create. A
            // detached checkout falls back to the branch of meta.json.
            let current = self.system.run(&git(&["branch", "--show-current"]))?;
            let branch = match current.stdout.trim() {
                "" => meta.branch.clone(),
                branch => branch.to_string(),
            };
            let output = self.system.run(&Command::new("gh").args([
                "pr",
                "list",
                "--repo",
                "n8n-io/n8n",
                "--head",
                &branch,
                "--state",
                "merged",
                "--json",
                "number,headRefOid,url",
                "--limit",
                "1",
            ]))?;
            if !output.success {
                anyhow::bail!("gh pr list failed for {branch}: {}", output.stderr.trim());
            }
            let prs: Vec<MergedPr> = match output.stdout.trim() {
                "" => Vec::new(),
                text => serde_json::from_str(text)
                    .with_context(|| format!("reading gh pr list for {branch}"))?,
            };
            let Some(pr) = prs.into_iter().next() else {
                continue;
            };
            let status = if self.checkout_dir(&meta.name).is_dir() {
                Some(self.system.run(&git(&["status", "--porcelain"]))?)
            } else {
                // No checkout: the folder holds only generated files.
                None
            };
            let skip = if let Some(status) = status.as_ref().filter(|status| !status.success) {
                Some(format!("git status failed: {}", status.stderr.trim()))
            } else if let Some(status) =
                status.as_ref().filter(|status| !status.stdout.trim().is_empty())
            {
                Some(format!("uncommitted changes: {}", changed_paths(&status.stdout)))
            } else if status.is_none() {
                None
            } else if !self
                .system
                .run(&git(&["merge-base", "--is-ancestor", "HEAD", &pr.head_ref_oid]))?
                .success
            {
                // Also fails when the PR head is not in this repo, which
                // means HEAD was never pushed from here.
                Some("commits that are not in the merged PR".to_string())
            } else {
                None
            };
            merged.push(MergedWorktree {
                name: meta.name,
                branch,
                pr: pr.number,
                url: pr.url,
                skip,
            });
        }
        Ok(merged)
    }
}

/// The paths of `git status --porcelain`, at most three.
fn changed_paths(porcelain: &str) -> String {
    let paths: Vec<&str> = porcelain
        .lines()
        .filter_map(|line| line.get(3..))
        .collect();
    let mut text = paths.iter().take(3).copied().collect::<Vec<_>>().join(", ");
    if paths.len() > 3 {
        text.push_str(&format!(" and {} more", paths.len() - 3));
    }
    text
}
