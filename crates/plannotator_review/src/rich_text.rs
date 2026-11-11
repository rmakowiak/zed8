//! Text from Claude (guide notes, chapter overviews, specs) is markdown.
//! Mermaid blocks become text diagrams through the local `mermaid-ascii` tool.

use std::{
    io::Write as _,
    process::{Command, Stdio},
};

/// Replaces each ```mermaid block with a plain code block that holds the
/// diagram drawn as text. A block stays as it is if the tool fails.
pub fn render_mermaid(markdown: &str) -> String {
    if !markdown.contains("```mermaid") {
        return markdown.to_string();
    }
    let mut out = String::with_capacity(markdown.len());
    let mut lines = markdown.lines();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("```mermaid") {
            let source: Vec<&str> = lines.by_ref().take_while(|line| line.trim() != "```").collect();
            let source = source.join("\n");
            match draw(&source) {
                Some(diagram) => out.push_str(&format!("```text\n{diagram}\n```\n")),
                None => out.push_str(&format!("```mermaid\n{source}\n```\n")),
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn draw(source: &str) -> Option<String> {
    let tool = util::paths::home_dir().join(".local/bin/mermaid-ascii");
    let mut child = Command::new(if tool.exists() { tool.as_os_str() } else { "mermaid-ascii".as_ref() })
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(source.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    let diagram = String::from_utf8(output.stdout).ok()?;
    (output.status.success() && !diagram.trim().is_empty()).then(|| diagram.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_text_without_mermaid() {
        assert_eq!(render_mermaid("Plain `code`."), "Plain `code`.");
    }

    #[test]
    fn replaces_or_keeps_a_mermaid_block() {
        let text = "Before\n```mermaid\ngraph LR\nA --> B\n```\nAfter";
        let out = render_mermaid(text);
        assert!(out.starts_with("Before\n```"));
        assert!(out.ends_with("```\nAfter\n"));
        assert!(!out.contains("graph LR") || out.contains("```mermaid"));
    }
}
