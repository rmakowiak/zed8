//! Reads Plannotator's saved guided reviews from `~/.plannotator/guides/`.
//!
//! Each repo has a folder (for example `github.com__n8n-io__n8n`) with one
//! `<id>.json` envelope and one `<id>.patch` for each guide.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GuideEnvelope {
    #[serde(default)]
    pub saved_at: u64,
    #[serde(default)]
    pub head_sha: Option<String>,
    /// The agent that wrote the guide, such as `claude`.
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub review: Option<GuideReview>,
    pub guide: Guide,
    #[serde(default)]
    pub reviewed: Vec<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GuideReview {
    #[serde(default)]
    pub git_ref: Option<String>,
    /// The compare target, such as `origin/master`.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub source: Option<GuideSource>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GuideSource {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub head_sha: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Guide {
    pub title: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub sections: Vec<GuideSection>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct GuideSection {
    pub title: String,
    #[serde(default)]
    pub overview: String,
    #[serde(default)]
    pub diffs: Vec<GuideDiff>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct GuideDiff {
    pub file: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub notes: Vec<GuideNote>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GuideNote {
    pub line_start: u32,
    pub line_end: u32,
    #[serde(default)]
    pub side: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedGuide {
    /// The file stem. A session knows it as `saved:<id>`.
    pub id: String,
    pub path: PathBuf,
    pub envelope: GuideEnvelope,
}

impl LoadedGuide {
    pub fn is_reviewed(&self, chapter: usize) -> bool {
        self.envelope.reviewed.get(chapter).copied().unwrap_or(false)
    }

    /// The reviewed flags with one entry for each chapter.
    pub fn reviewed_flags(&self) -> Vec<bool> {
        (0..self.envelope.guide.sections.len())
            .map(|chapter| self.is_reviewed(chapter))
            .collect()
    }
}

/// The newest saved guide made for `branch` or for the commit `head_sha`.
pub fn find_guide(dir: &Path, branch: Option<&str>, head_sha: Option<&str>) -> Option<LoadedGuide> {
    let repo_dirs = std::fs::read_dir(dir).ok()?;
    repo_dirs
        .filter_map(|entry| std::fs::read_dir(entry.ok()?.path()).ok())
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "json" {
                return None;
            }
            let envelope = serde_json::from_slice::<GuideEnvelope>(&std::fs::read(&path).ok()?).ok()?;
            let id = path.file_stem()?.to_str()?.to_string();
            Some(LoadedGuide { id, path, envelope })
        })
        .filter(|guide| matches(&guide.envelope, branch, head_sha))
        .max_by_key(|guide| guide.envelope.saved_at)
}

fn matches(envelope: &GuideEnvelope, branch: Option<&str>, head_sha: Option<&str>) -> bool {
    let review = envelope.review.clone().unwrap_or_default();
    let source = review.source.unwrap_or_default();
    let same_branch = branch.is_some_and(|branch| {
        source.branch.as_deref() == Some(branch)
            || review
                .git_ref
                .as_deref()
                .is_some_and(|git_ref| git_ref.split(':').next() == Some(branch))
    });
    let same_head = head_sha.is_some_and(|head| {
        envelope.head_sha.as_deref() == Some(head) || source.head_sha.as_deref() == Some(head)
    });
    same_branch || same_head
}

/// Writes the reviewed flags into the saved envelope. Other fields stay as they are.
pub fn write_reviewed(path: &Path, reviewed: &[bool]) -> Result<()> {
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).context("read guide")?)?;
    envelope["reviewed"] = serde_json::json!(reviewed);
    std::fs::write(path, serde_json::to_vec_pretty(&envelope)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_guide(dir: &Path, name: &str, branch: &str, saved_at: u64) {
        let json = serde_json::json!({
            "version": 1,
            "savedAt": saved_at,
            "headSha": "abc",
            "review": { "gitRef": format!("{branch}: A"), "source": { "kind": "local", "branch": branch } },
            "guide": {
                "title": format!("Guide {name}"),
                "intent": "Why",
                "sections": [
                    { "title": "One", "overview": "First", "diffs": [
                        { "file": "a.ts", "summary": "Adds a", "notes": [
                            { "lineStart": 3, "lineEnd": 5, "side": "new", "text": "Note" }
                        ] }
                    ] },
                    { "title": "Two", "overview": "Second", "diffs": [] }
                ]
            },
            "reviewed": [true]
        });
        std::fs::write(dir.join(format!("{name}.json")), json.to_string()).unwrap();
    }

    #[test]
    fn finds_newest_guide_for_branch_and_writes_reviewed() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("github.com__acme__app");
        std::fs::create_dir(&repo).unwrap();
        write_guide(&repo, "old", "feature", 1);
        write_guide(&repo, "new", "feature", 2);
        write_guide(&repo, "other", "main", 3);

        let guide = find_guide(root.path(), Some("feature"), None).unwrap();
        assert_eq!(guide.id, "new");
        assert_eq!(guide.reviewed_flags(), vec![true, false]);
        assert_eq!(guide.envelope.guide.sections[0].diffs[0].notes[0].line_start, 3);

        write_reviewed(&guide.path, &[true, true]).unwrap();
        let again = find_guide(root.path(), Some("feature"), None).unwrap();
        assert_eq!(again.reviewed_flags(), vec![true, true]);
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&again.path).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);

        assert!(find_guide(root.path(), Some("nope"), None).is_none());
        assert_eq!(find_guide(root.path(), None, Some("abc")).unwrap().id, "other");
    }
}
