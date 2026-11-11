//! Builds the body for Plannotator's `POST /api/feedback`.
//!
//! The agent reads `feedback` verbatim, so the markdown follows Plannotator's
//! own export (`packages/review-editor/utils/exportFeedback.ts`).

use editor::ExportedReviewComment;
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackRequest {
    pub approved: bool,
    pub feedback: String,
    pub annotations: Vec<CodeAnnotation>,
}

/// The subset of Plannotator's `CodeAnnotation` that Zed fills in.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeAnnotation {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub side: &'static str,
    pub text: String,
    pub created_at: u64,
    pub source: &'static str,
}

pub fn build_request(
    comments: &[ExportedReviewComment],
    worktree: &str,
    now_ms: u64,
) -> FeedbackRequest {
    FeedbackRequest {
        approved: false,
        feedback: markdown(comments, worktree),
        annotations: comments
            .iter()
            .enumerate()
            .map(|(index, comment)| CodeAnnotation {
                id: format!("zed-{now_ms}-{index}"),
                kind: "comment",
                file_path: comment.file_path.clone(),
                line_start: comment.line_start,
                line_end: comment.line_end,
                side: "new",
                text: comment.text.clone(),
                created_at: now_ms,
                source: "zed",
            })
            .collect(),
    }
}

fn markdown(comments: &[ExportedReviewComment], worktree: &str) -> String {
    if comments.is_empty() {
        return "# Code Review\n\nNo feedback provided.".to_string();
    }
    let mut output =
        format!("# Code Review Feedback\n\n**Diff:** Branch diff _(worktree: {worktree})_\n\n");

    // Group by file in first-seen order, then sort each file by line.
    let mut files: Vec<(&str, Vec<&ExportedReviewComment>)> = Vec::new();
    for comment in comments {
        match files.iter_mut().find(|(path, _)| *path == comment.file_path) {
            Some((_, group)) => group.push(comment),
            None => files.push((&comment.file_path, vec![comment])),
        }
    }
    for (path, mut group) in files {
        group.sort_by_key(|comment| comment.line_start);
        output.push_str(&format!("## {path}\n\n"));
        for comment in group {
            let range = if comment.line_start == comment.line_end {
                format!("Line {}", comment.line_start)
            } else {
                format!("Lines {}-{}", comment.line_start, comment.line_end)
            };
            output.push_str(&format!("### {range} (new)\n{}\n\n", comment.text));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(path: &str, start: u32, end: u32, text: &str) -> ExportedReviewComment {
        ExportedReviewComment {
            file_path: path.into(),
            line_start: start,
            line_end: end,
            text: text.into(),
        }
    }

    #[test]
    fn formats_like_plannotator_export() {
        let comments = [
            comment("b.ts", 9, 9, "Rename this."),
            comment("a.vue", 20, 24, "Why a watcher?"),
            comment("b.ts", 3, 3, "Unused import."),
        ];
        let request = build_request(&comments, "/w/n8n", 42);
        assert_eq!(
            request.feedback,
            "# Code Review Feedback\n\n\
             **Diff:** Branch diff _(worktree: /w/n8n)_\n\n\
             ## b.ts\n\n\
             ### Line 3 (new)\nUnused import.\n\n\
             ### Line 9 (new)\nRename this.\n\n\
             ## a.vue\n\n\
             ### Lines 20-24 (new)\nWhy a watcher?\n\n"
        );
        assert!(!request.approved);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["annotations"][1]["filePath"], "a.vue");
        assert_eq!(json["annotations"][1]["lineStart"], 20);
        assert_eq!(json["annotations"][1]["type"], "comment");
    }
}
