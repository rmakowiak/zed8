//! A spec under review: its markdown split into blocks, the `:::question`
//! blocks parsed into questions, and the feedback text for Claude.
//!
//! The feedback follows Plannotator's own export (`exportAnnotations` in
//! `packages/ui/utils/parser.ts` and `formatQuestionAnswersSection` in
//! `packages/core/question-block.ts`), because Claude reads it verbatim.

use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct SpecBlock {
    /// 1-based line numbers in the file.
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub question: Option<Question>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionKind {
    Single,
    Multi,
    Text,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub label: String,
    pub description: String,
    /// `- [x]`: the agent already settled this choice.
    pub settled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub kind: QuestionKind,
    pub prompt: String,
    pub context: Vec<String>,
    pub choices: Vec<Choice>,
    pub recommended: Vec<String>,
    /// A `Recommended:` line that names no choice.
    pub suggested: Option<String>,
    pub key: String,
}

impl Question {
    pub fn is_settled(&self) -> bool {
        self.choices.iter().any(|choice| choice.settled)
    }
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Answer {
    pub selected: Vec<String>,
    /// The free-text answer of a text question.
    pub text: String,
    pub note: String,
    pub skipped: bool,
}

impl Answer {
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
            && self.text.trim().is_empty()
            && self.note.trim().is_empty()
            && !self.skipped
    }

    fn is_answered(&self) -> bool {
        !self.selected.is_empty() || !self.text.trim().is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpecComment {
    /// The index of the block in `parse`'s output.
    pub block: usize,
    /// The block text when the comment was made. A later round uses it to
    /// tell a resolved comment from an open one.
    pub original_text: String,
    pub text: String,
    pub created_at: u64,
}

/// Splits markdown into blocks: paragraphs and lists (ended by a blank
/// line), headings, fenced code, and `:::` directives.
pub fn parse(markdown: &str) -> Vec<SpecBlock> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut blocks = Vec::new();
    let mut ix = 0;
    while ix < lines.len() {
        let line = lines[ix];
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            ix += 1;
            continue;
        }
        let start = ix;
        if let Some(fence) = fence_marker(trimmed) {
            ix += 1;
            while ix < lines.len() && !lines[ix].trim_start().starts_with(fence) {
                ix += 1;
            }
            ix = (ix + 1).min(lines.len());
        } else if trimmed.starts_with(":::") {
            ix += 1;
            while ix < lines.len() && lines[ix].trim() != ":::" {
                ix += 1;
            }
            ix = (ix + 1).min(lines.len());
        } else if trimmed.starts_with('#') {
            ix += 1;
        } else {
            ix += 1;
            while ix < lines.len() {
                let next = lines[ix].trim_start();
                if next.is_empty() || next.starts_with('#') || fence_marker(next).is_some() || next.starts_with(":::") {
                    break;
                }
                ix += 1;
            }
        }
        let text = lines[start..ix].join("\n");
        let question = parse_question(&text);
        blocks.push(SpecBlock {
            start_line: start + 1,
            end_line: ix,
            text,
            question,
        });
    }
    blocks
}

fn fence_marker(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn parse_question(block: &str) -> Option<Question> {
    let mut lines = block.lines();
    let header = lines.next()?.trim();
    let kind = match header.strip_prefix(":::")?.trim() {
        "question" => QuestionKind::Single,
        "question-multi" => QuestionKind::Multi,
        "question-text" => QuestionKind::Text,
        _ => return None,
    };
    let body: Vec<&str> = lines.filter(|line| line.trim() != ":::").collect();

    let mut prompt = None;
    let mut context = Vec::new();
    let mut checkbox_choices = Vec::new();
    let mut plain_bullets = Vec::new();
    let mut recommendation = None;
    for line in &body {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(value) = recommendation_value(line) {
            recommendation = Some(value.to_string());
        } else if let Some((settled, rest)) = checkbox_item(line) {
            checkbox_choices.push(choice(rest, settled));
        } else if let Some(rest) = bullet_item(line) {
            plain_bullets.push(choice(rest, false));
        } else if prompt.is_none() && !is_id_label(line) {
            prompt = Some(line.to_string());
        } else {
            context.push(line.to_string());
        }
    }
    let prompt = prompt?;
    let mut choices = if checkbox_choices.is_empty() && kind != QuestionKind::Text {
        plain_bullets
    } else {
        context.extend(plain_bullets.into_iter().map(|choice| format!("- {}", choice.label)));
        checkbox_choices
    };
    let kind = if choices.is_empty() { QuestionKind::Text } else { kind };
    if kind == QuestionKind::Text {
        choices.clear();
    }

    let mut recommended = Vec::new();
    let mut suggested = None;
    if let Some(value) = recommendation {
        let names: Vec<&str> = if kind == QuestionKind::Multi {
            value.split([',', ';']).collect()
        } else {
            vec![value.as_str()]
        };
        for name in names {
            match choices.iter().find(|choice| names_choice(name, &choice.label)) {
                Some(choice) => recommended.push(choice.label.clone()),
                None => {}
            }
        }
        if recommended.is_empty() {
            suggested = Some(value);
        }
    }

    let kind_word = match kind {
        QuestionKind::Single => "single",
        QuestionKind::Multi => "multi",
        QuestionKind::Text => "text",
    };
    let key = format!("q-{:08x}", fnv1a32(&format!("{kind_word}\n{}", normalize(&prompt))));
    Some(Question {
        kind,
        prompt,
        context,
        choices,
        recommended,
        suggested,
        key,
    })
}

fn recommendation_value(line: &str) -> Option<&str> {
    ["Recommended:", "Recommendation:", "->", "=>", "→", "➡️"]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        .map(str::trim)
}

fn checkbox_item(line: &str) -> Option<(bool, &str)> {
    let rest = bullet_item(line)?;
    let rest = rest.strip_prefix('[')?;
    let (mark, rest) = rest.split_once(']')?;
    match mark {
        " " => Some((false, rest.trim())),
        "x" | "X" => Some((true, rest.trim())),
        _ => None,
    }
}

fn bullet_item(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }
    let (number, rest) = line.split_once(". ")?;
    number.chars().all(|c| c.is_ascii_digit()).then(|| rest.trim())
}

fn choice(text: &str, settled: bool) -> Choice {
    let (label, description) = [" — ", " – ", " - "]
        .iter()
        .find_map(|separator| text.split_once(separator))
        .unwrap_or((text, ""));
    Choice {
        label: label.trim().to_string(),
        description: description.trim().to_string(),
        settled,
    }
}

/// A short lower-case `id:` or `key:` label before the prompt.
fn is_id_label(line: &str) -> bool {
    let Some((name, value)) = line.split_once(':') else {
        return false;
    };
    let value = value.trim();
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c == '-')
        && (name.ends_with("id") || name.ends_with("key"))
        && !value.is_empty()
        && !value.contains(' ')
        && !value.ends_with('?')
}

fn names_choice(name: &str, label: &str) -> bool {
    let name = normalize(name);
    let label = normalize(label);
    name == label
        || [" —", " –", " -", ":", " ("]
            .iter()
            .any(|tail| name.starts_with(&format!("{label}{tail}")))
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn fnv1a32(text: &str) -> u32 {
    text.bytes().fold(0x811c9dc5u32, |hash, byte| {
        (hash ^ byte as u32).wrapping_mul(0x0100_0193)
    })
}

/// Blocks whose text was not in the previous round. Empty without one.
pub fn changed_blocks(blocks: &[SpecBlock], previous: Option<&str>) -> HashSet<usize> {
    let Some(previous) = previous else {
        return HashSet::new();
    };
    let old: HashSet<String> = parse(previous).into_iter().map(|block| block.text).collect();
    blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| !old.contains(&block.text))
        .map(|(ix, _)| ix)
        .collect()
}

fn line_label(block: &SpecBlock) -> String {
    if block.start_line == block.end_line {
        format!("line {}", block.start_line)
    } else {
        format!("lines {}–{}", block.start_line, block.end_line)
    }
}

/// The first line of a block, short enough to quote.
pub fn excerpt(text: &str) -> String {
    let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
    if first.chars().count() > 120 {
        format!("{}…", first.chars().take(120).collect::<String>())
    } else {
        first.to_string()
    }
}

/// The markdown Claude receives, in Plannotator's export format.
pub fn export_feedback(
    blocks: &[SpecBlock],
    comments: &[SpecComment],
    answers: &HashMap<String, Answer>,
) -> String {
    let answers_section = answers_section(blocks, answers);
    if comments.is_empty() && answers_section.is_empty() {
        return "No changes detected.".to_string();
    }
    let mut out = "# File Feedback\n\n".to_string();
    out.push_str(&answers_section);
    let mut sorted: Vec<&SpecComment> = comments.iter().collect();
    sorted.sort_by_key(|comment| (comment.block, comment.created_at));
    if !sorted.is_empty() {
        let count = sorted.len();
        out.push_str(&format!(
            "I've reviewed this file and have {count} piece{} of feedback:\n\n",
            if count > 1 { "s" } else { "" }
        ));
    }
    for (ix, comment) in sorted.iter().enumerate() {
        let label = blocks
            .get(comment.block)
            .map(|block| format!("({}) ", line_label(block)))
            .unwrap_or_default();
        out.push_str(&format!(
            "## {}. {label}Feedback on: \"{}\"\n> {}\n\n",
            ix + 1,
            excerpt(&comment.original_text),
            comment.text.trim().replace('\n', "\n> ")
        ));
    }
    out.push_str("---\n");
    out
}

fn answers_section(blocks: &[SpecBlock], answers: &HashMap<String, Answer>) -> String {
    let questions: Vec<(usize, &SpecBlock, &Question)> = blocks
        .iter()
        .filter_map(|block| Some((block, block.question.as_ref()?)))
        .enumerate()
        .map(|(ix, (block, question))| (ix + 1, block, question))
        .collect();
    let answer_for = |question: &Question| answers.get(&question.key).filter(|answer| !answer.is_empty());
    if questions.iter().all(|(_, _, question)| answer_for(question).is_none()) {
        return String::new();
    }
    let open: Vec<_> = questions
        .iter()
        .filter(|(_, _, question)| !question.is_settled() || answer_for(question).is_some())
        .collect();
    let answered = open
        .iter()
        .filter(|(_, _, question)| {
            answer_for(question).is_some_and(|answer| answer.is_answered() || (question.is_settled() && !answer.skipped))
        })
        .count();
    let mut out = "## Answers to your questions\n\n".to_string();
    out.push_str(&format!(
        "{answered} of {} question{} answered.",
        open.len(),
        if open.len() == 1 { "" } else { "s" }
    ));
    let settled_untouched = questions.len() - open.len();
    if settled_untouched > 0 {
        out.push_str(&format!(
            " {settled_untouched} already settled in the document {} left as is.",
            if settled_untouched == 1 { "was" } else { "were" }
        ));
    }
    out.push_str("\n\n");
    let mut unanswered = Vec::new();
    for (number, block, question) in &open {
        let Some(answer) = answer_for(question) else {
            unanswered.push(format!("- Q{number}. {} (line {})\n", question.prompt, block.start_line));
            continue;
        };
        out.push_str(&format!("### Q{number}. {} (line {})\n", question.prompt, block.start_line));
        out.push_str(&answer_lines(question, answer));
        out.push('\n');
    }
    if !unanswered.is_empty() {
        out.push_str("### Unanswered\n");
        out.extend(unanswered);
        out.push('\n');
    }
    out
}

fn answer_lines(question: &Question, answer: &Answer) -> String {
    let mut out = String::new();
    let is_recommended = answer.is_answered()
        && ((!answer.selected.is_empty() && {
            let mut selected = answer.selected.clone();
            let mut recommended = question.recommended.clone();
            selected.sort();
            recommended.sort();
            selected == recommended
        }) || question
            .suggested
            .as_deref()
            .is_some_and(|suggested| normalize(suggested) == normalize(&answer.text)));
    let rec = if is_recommended { " (your recommendation)" } else { "" };
    if !answer.text.trim().is_empty() {
        out.push_str(&format!("Answer{rec}:\n> {}\n", answer.text.trim().replace('\n', "\n> ")));
    } else if answer.selected.len() > 1 {
        out.push_str(&format!("Answer{rec}:\n"));
        for label in &answer.selected {
            out.push_str(&format!("- {label}\n"));
        }
    } else if let Some(label) = answer.selected.first() {
        out.push_str(&format!("Answer: {label}{rec}\n"));
    } else if answer.skipped {
        out.push_str("Skipped\n");
    } else if question.is_settled() {
        let settled: Vec<&str> = question
            .choices
            .iter()
            .filter(|choice| choice.settled)
            .map(|choice| choice.label.as_str())
            .collect();
        out.push_str(&format!("Answer: {} (already settled in the document)\n", settled.join(", ")));
    }
    if !answer.note.trim().is_empty() {
        out.push_str(&format!("Note: {}\n", answer.note.trim().replace('\n', "\n> ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = "# Title\n\nFirst paragraph.\nStill first.\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\n:::question\nid: storage\nWhere do we keep drafts?\n\nWe need them after a restart.\n\n- [ ] Local only — cheap\n- [ ] Server — survives reinstall\n\nRecommended: Local only\n:::\n\n:::question-text\nAnything else?\n:::\n";

    #[test]
    fn splits_blocks_and_parses_questions() {
        let blocks = parse(SPEC);
        let texts: Vec<_> = blocks.iter().map(|block| (block.start_line, block.end_line)).collect();
        assert_eq!(texts, vec![(1, 1), (3, 4), (6, 10), (12, 22), (24, 26)]);

        let question = blocks[3].question.as_ref().unwrap();
        assert_eq!(question.kind, QuestionKind::Single);
        assert_eq!(question.prompt, "Where do we keep drafts?");
        assert_eq!(question.context, vec!["id: storage", "We need them after a restart."]);
        assert_eq!(question.choices[0].label, "Local only");
        assert_eq!(question.choices[0].description, "cheap");
        assert_eq!(question.recommended, vec!["Local only"]);
        assert!(question.key.starts_with("q-") && question.key.len() == 10);

        let text = blocks[4].question.as_ref().unwrap();
        assert_eq!(text.kind, QuestionKind::Text);
        assert!(blocks[0].question.is_none());
    }

    #[test]
    fn exports_answers_then_comments() {
        let blocks = parse(SPEC);
        let key = blocks[3].question.as_ref().unwrap().key.clone();
        let answers = HashMap::from_iter([(
            key,
            Answer {
                selected: vec!["Local only".into()],
                note: "Purge after 30 days.".into(),
                ..Answer::default()
            },
        )]);
        let comments = vec![SpecComment {
            block: 1,
            original_text: blocks[1].text.clone(),
            text: "Say why.".into(),
            created_at: 1,
        }];
        assert_eq!(
            export_feedback(&blocks, &comments, &answers),
            "# File Feedback\n\n## Answers to your questions\n\n1 of 2 questions answered.\n\n\
             ### Q1. Where do we keep drafts? (line 12)\nAnswer: Local only (your recommendation)\nNote: Purge after 30 days.\n\n\
             ### Unanswered\n- Q2. Anything else? (line 24)\n\n\
             I've reviewed this file and have 1 piece of feedback:\n\n\
             ## 1. (lines 3–4) Feedback on: \"First paragraph.\"\n> Say why.\n\n---\n"
        );
        assert_eq!(export_feedback(&blocks, &[], &HashMap::default()), "No changes detected.");
    }

    #[test]
    fn finds_changed_blocks() {
        let blocks = parse("# Title\n\nNew text.\n");
        assert_eq!(changed_blocks(&blocks, Some("# Title\n\nOld text.\n")), HashSet::from([1]));
        assert!(changed_blocks(&blocks, None).is_empty());
    }
}
