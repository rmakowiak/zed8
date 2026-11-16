//! Gate 2 of the agent review in Zed: the findings that `pr-review` posts
//! into its Plannotator code review session as external annotations
//! (`GET /api/external-annotations`), and the message that Approve and Send
//! feedback send back. The message has the shape of Plannotator's own
//! export, which step 4 of `pr-review` reads.

use crate::guide::{GuideDiff, GuideNote};
use editor::ExportedReviewComment;
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};

/// The first line of Done at gate 2. `pr-review` reads it and posts nothing,
/// because the Zed8 review draft posts.
pub const ZED8_DRAFT_NOTE: &str = "ZED8-DRAFT: Gate 2 is done. The Zed8 review draft posts to GitHub, so do not post anything. Save the findings as step 4 says, mark the run done, and stop.";

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAnnotation {
    pub id: String,
    #[serde(default)]
    pub source: Option<String>,
    /// `line`, `file` or `general`.
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub line_start: Option<u32>,
    #[serde(default)]
    pub line_end: Option<u32>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub suggested_code: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AnnotationsResponse {
    #[serde(default)]
    pub annotations: Vec<ExternalAnnotation>,
}

impl ExternalAnnotation {
    fn is_inline(&self) -> bool {
        self.scope.as_deref().unwrap_or("line") == "line"
            && self.file_path.is_some()
            && self.line_start.is_some()
    }

    /// The text with the suggested code under it, as the browser shows it.
    fn full_text(&self) -> String {
        match self.suggested_code.as_deref().filter(|code| !code.trim().is_empty()) {
            Some(code) => format!("{}\n\n```suggestion\n{code}\n```", self.text),
            None => self.text.clone(),
        }
    }
}

/// The findings split into one `GuideDiff` for each changed file (inline
/// findings become notes) and the general ones (the review body and the
/// findings that are not on a diff line).
pub fn split(
    annotations: &[ExternalAnnotation],
    changed_files: &[String],
) -> (Vec<GuideDiff>, Vec<ExternalAnnotation>) {
    let mut notes: BTreeMap<&str, Vec<GuideNote>> = BTreeMap::new();
    let mut general = Vec::new();
    for annotation in annotations {
        match (annotation.is_inline(), annotation.file_path.as_deref()) {
            (true, Some(file)) if changed_files.iter().any(|changed| changed == file) => {
                let line_start = annotation.line_start.unwrap_or(1);
                notes.entry(file).or_default().push(GuideNote {
                    line_start,
                    line_end: annotation.line_end.unwrap_or(line_start),
                    side: annotation.side.clone(),
                    text: annotation.full_text(),
                    finding_id: Some(annotation.id.clone()),
                    severity: annotation.severity.clone(),
                });
            }
            _ => general.push(annotation.clone()),
        }
    }
    // Files with findings come first, so they are at the top of the tab.
    let mut diffs: Vec<GuideDiff> = changed_files
        .iter()
        .map(|file| GuideDiff {
            file: file.clone(),
            summary: String::new(),
            notes: notes.remove(file.as_str()).unwrap_or_default(),
        })
        .collect();
    diffs.sort_by_key(|diff| diff.notes.is_empty());
    (diffs, general)
}

/// A general finding as a note with no lines, for its thread.
pub fn general_note(annotation: &ExternalAnnotation) -> GuideNote {
    GuideNote {
        line_start: 0,
        line_end: 0,
        side: None,
        text: match annotation.file_path.as_deref() {
            Some(file) if !annotation.text.starts_with("Not on a diff line") => {
                format!("{file}: {}", annotation.full_text())
            }
            _ => annotation.full_text(),
        },
        finding_id: Some(annotation.id.clone()),
        severity: annotation.severity.clone(),
    }
}

/// 5 for Critical down to 1 for Nit, 0 for a severity the scale does not have.
pub fn severity_rank(severity: &str) -> u8 {
    match severity.to_lowercase().as_str() {
        "critical" => 5,
        "high" => 4,
        "medium" => 3,
        "low" => 2,
        "nit" => 1,
        _ => 0,
    }
}

/// The message for `POST /api/feedback`: the inline comments by file, then
/// the general comments, in the format of `feedback.rs`.
pub fn message(inline: &[ExportedReviewComment], general: &[String], worktree: &str) -> String {
    let mut output = crate::feedback::markdown(inline, worktree);
    if inline.is_empty() {
        output = format!("# Code Review Feedback\n\n**Diff:** Branch diff _(worktree: {worktree})_\n\n");
    }
    if !general.is_empty() {
        output.push_str("## General\n\n");
        for text in general {
            output.push_str(text);
            output.push_str("\n\n");
        }
    }
    output
}

/// The files that differ between the merge base of HEAD and `base`, in git order.
pub fn changed_files(root: &Path, base: &str) -> anyhow::Result<Vec<String>> {
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let output = std::process::Command::new("git").arg("-C").arg(root).args(args).output()?;
        anyhow::ensure!(output.status.success(), "git {} failed", args.join(" "));
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let merge_base = git(&["merge-base", "HEAD", base])?;
    Ok(git(&["diff", "--name-only", "--diff-filter=d", &merge_base])?
        .lines()
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annotation(json: &str) -> ExternalAnnotation {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn splits_inline_and_general_findings() {
        let annotations = vec![
            annotation(r#"{"id":"1","scope":"line","filePath":"a.ts","lineStart":4,"lineEnd":5,"side":"new","text":"Loop","suggestedCode":"break;"}"#),
            annotation(r#"{"id":"2","scope":"general","text":"Review body: Good."}"#),
            annotation(r#"{"id":"3","scope":"file","filePath":"b.ts","text":"Not on a diff line: owners"}"#),
            annotation(r#"{"id":"4","scope":"line","filePath":"gone.ts","lineStart":1,"text":"Not in the diff"}"#),
        ];
        let (diffs, general) = split(&annotations, &["b.ts".into(), "a.ts".into()]);
        assert_eq!(diffs[0].file, "a.ts");
        assert_eq!(diffs[0].notes[0].finding_id.as_deref(), Some("1"));
        assert!(diffs[0].notes[0].text.contains("```suggestion\nbreak;\n```"));
        assert_eq!(general.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), vec!["2", "3", "4"]);
        assert_eq!(general_note(&general[1]).text, "Not on a diff line: owners");
    }

    #[test]
    fn findings_keep_their_severity() {
        let annotations = vec![
            annotation(r#"{"id":"1","scope":"line","filePath":"a.ts","lineStart":4,"severity":"High","text":"Loop"}"#),
            annotation(r#"{"id":"2","scope":"line","filePath":"a.ts","lineStart":9,"text":"No severity"}"#),
            annotation(r#"{"id":"3","scope":"file","filePath":"a.ts","severity":"Low","text":"Not on a diff line: owners"}"#),
        ];
        let (diffs, general) = split(&annotations, &["a.ts".into()]);
        assert_eq!(diffs[0].notes[0].severity.as_deref(), Some("High"));
        assert_eq!(diffs[0].notes[1].severity, None);
        assert_eq!(general_note(&general[0]).severity.as_deref(), Some("Low"));
    }

    #[test]
    fn severities_rank_from_critical_to_nit() {
        let ranked: Vec<_> = ["nit", "Low", "Medium", "High", "Critical", "odd"]
            .into_iter()
            .map(severity_rank)
            .collect();
        assert_eq!(ranked, [1, 2, 3, 4, 5, 0]);
    }

    #[test]
    fn message_has_inline_then_general() {
        let inline = vec![ExportedReviewComment {
            file_path: "a.ts".into(),
            line_start: 4,
            line_end: 4,
            text: "Loop".into(),
        }];
        let text = message(&inline, &["Review body: Good.".into()], "/w");
        assert!(text.contains("## a.ts\n\n### Line 4 (new)\nLoop\n\n## General\n\nReview body: Good.\n\n"));
    }
}
