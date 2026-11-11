//! What Zed keeps for each spec file in `~/.plannotator/zed-drafts/`: the
//! unsent comments and answers, the comments of the last sent round, and
//! the approval date.

use crate::spec_doc::{Answer, SpecBlock, SpecComment};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpecRecord {
    #[serde(default)]
    pub comments: Vec<SpecComment>,
    #[serde(default)]
    pub answers: HashMap<String, Answer>,
    /// The comments sent in the last round.
    #[serde(default)]
    pub last_round: Vec<SpecComment>,
    /// The local date and time of the approval, such as `2026-11-11 22:50`.
    #[serde(default)]
    pub approved_at: Option<String>,
}

/// A comment of the last round, checked against the new text.
#[derive(Clone, Debug, PartialEq)]
pub struct PreviousComment {
    pub comment: SpecComment,
    /// The commented block is still in the spec, unchanged.
    pub open: bool,
}

impl SpecRecord {
    pub fn previous_comments(&self, blocks: &[SpecBlock]) -> Vec<PreviousComment> {
        self.last_round
            .iter()
            .map(|comment| PreviousComment {
                comment: comment.clone(),
                open: blocks.iter().any(|block| block.text == comment.original_text),
            })
            .collect()
    }

    /// Keeps only drafts that still match a block, and moves comments to the
    /// block that now holds their text.
    pub fn reattach(&mut self, blocks: &[SpecBlock]) {
        self.comments.retain_mut(|comment| {
            match blocks.iter().position(|block| block.text == comment.original_text) {
                Some(ix) => {
                    comment.block = ix;
                    true
                }
                None => false,
            }
        });
    }
}

pub fn record_path(dir: &Path, spec_path: &str) -> PathBuf {
    let hash = spec_path
        .bytes()
        .fold(0xcbf29ce484222325u64, |hash, byte| (hash ^ byte as u64).wrapping_mul(0x100000001b3));
    let name = Path::new(spec_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("spec");
    dir.join(format!("spec-{name}-{hash:016x}.json"))
}

pub fn load(path: &Path) -> SpecRecord {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(path: &Path, record: &SpecRecord) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(record)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec_doc::parse;

    #[test]
    fn saves_and_checks_last_round() {
        let dir = tempfile::tempdir().unwrap();
        let path = record_path(dir.path(), "/tmp/a/spec.md");
        assert!(path.file_name().unwrap().to_str().unwrap().starts_with("spec-spec-"));
        assert_eq!(load(&path), SpecRecord::default());

        let comment = |text: &str| SpecComment {
            block: 0,
            original_text: text.into(),
            text: "Why?".into(),
            created_at: 1,
        };
        let record = SpecRecord {
            comments: vec![comment("Kept."), comment("Gone.")],
            last_round: vec![comment("Kept."), comment("Gone.")],
            ..SpecRecord::default()
        };
        save(&path, &record).unwrap();
        let mut record = load(&path);

        let blocks = parse("# T\n\nKept.\n");
        let open: Vec<bool> = record.previous_comments(&blocks).iter().map(|c| c.open).collect();
        assert_eq!(open, vec![true, false]);

        record.reattach(&blocks);
        assert_eq!(record.comments.len(), 1);
        assert_eq!(record.comments[0].block, 1);
    }
}
