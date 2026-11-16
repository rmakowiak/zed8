//! The Tools of the Review panel: the PR preview environment, its feature
//! flags and the local n8n of the worktree. They work as the "My Work"
//! Raycast extension in `~/workspace/n8n-worktrees/raycast`:
//!
//! - Each flag of a preview is a `flag:<key>` label on the PR (`=false` and
//!   `=<variant>` also work). The `cloud-preview` label starts a preview.
//! - The n8n-assistant comment that starts with `<!-- pr-preview -->` has the
//!   URL of the preview and the flags that it runs with.
//! - The flag picker lists the flags of the PostHog Staging project, when
//!   `POSTHOG_PERSONAL_API_KEY` is set, and the `flag:` labels of the repo.
//!
//! Zed only changes PR labels. It never changes PostHog.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

pub const PREVIEW_LABEL: &str = "cloud-preview";
const FLAG_PREFIX: &str = "flag:";
const PREVIEW_MARKER: &str = "<!-- pr-preview -->";
const POSTHOG_HOST: &str = "https://eu.posthog.com";
/// PR previews run on staging Cloud, so their flags come from PostHog Staging.
const POSTHOG_STAGING_PROJECT: u32 = 142235;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrTools {
    pub labels: Vec<String>,
    pub preview: Option<Preview>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub url: String,
    /// The flags that the running preview has on.
    pub flags: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlagState {
    /// The label is on the PR and the preview runs with the flag.
    InPreview,
    /// The label is on the PR, but the preview does not have it yet.
    Pending,
    /// The label is on the PR and no preview runs.
    NoPreview,
}

/// The flag key of a `flag:<key>`, `flag:<key>=false` or `flag:<key>=<variant>` label.
pub fn flag_of(label: &str) -> Option<&str> {
    let rest = label.strip_prefix(FLAG_PREFIX)?;
    rest.split('=').next().filter(|key| !key.is_empty())
}

impl PrTools {
    pub fn has_preview(&self) -> bool {
        self.preview.is_some() || self.labels.iter().any(|label| label == PREVIEW_LABEL)
    }

    pub fn flag_labels(&self) -> Vec<(String, FlagState)> {
        self.labels
            .iter()
            .filter_map(|label| {
                let key = flag_of(label)?;
                let state = match &self.preview {
                    Some(preview) if preview.flags.iter().any(|flag| flag == key) => FlagState::InPreview,
                    Some(_) => FlagState::Pending,
                    None => FlagState::NoPreview,
                };
                Some((label.clone(), state))
            })
            .collect()
    }

    /// The labels to add and remove so that the PR has a preview with
    /// exactly `picked` flags. A running preview picks up label changes by
    /// itself, so it needs no new `cloud-preview`.
    pub fn label_change(&self, picked: &[String]) -> LabelChange {
        let current: BTreeSet<&str> = self.labels.iter().filter_map(|label| flag_of(label)).collect();
        let mut add: Vec<String> = Vec::new();
        if !self.has_preview() {
            add.push(PREVIEW_LABEL.to_string());
        }
        add.extend(
            picked
                .iter()
                .filter(|flag| !current.contains(flag_of(&format!("{FLAG_PREFIX}{flag}")).unwrap_or(flag)))
                .map(|flag| format!("{FLAG_PREFIX}{flag}")),
        );
        let picked_keys: BTreeSet<&str> = picked.iter().map(|flag| flag.split('=').next().unwrap_or(flag)).collect();
        let remove = self
            .labels
            .iter()
            .filter(|label| flag_of(label).is_some_and(|key| !picked_keys.contains(key)))
            .cloned()
            .collect();
        LabelChange { add, remove }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LabelChange {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

impl LabelChange {
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

/// Reads the preview comment: the URL row and the JSON of the Flags row.
pub fn parse_preview(body: &str) -> Option<Preview> {
    if !body.starts_with(PREVIEW_MARKER) {
        return None;
    }
    let row = |name: &str| {
        body.lines().find_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            (cells.len() >= 3 && cells[1] == name).then(|| cells[2].to_string())
        })
    };
    let url = row("URL").filter(|url| url.starts_with("https://"))?;
    let flags = row("Flags")
        .map(|cell| cell.trim_matches('`').to_string())
        .and_then(|json| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json).ok())
        .map(|map| {
            map.into_iter()
                .filter(|(_, value)| value != &serde_json::Value::Bool(false))
                .map(|(key, _)| key)
                .collect()
        })
        .unwrap_or_default();
    Some(Preview { url, flags })
}

#[derive(Deserialize)]
struct GhPr {
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    comments: Vec<GhComment>,
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Deserialize)]
struct GhComment {
    #[serde(default)]
    author: Option<GhAuthor>,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct GhAuthor {
    login: String,
}

fn gh(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("gh").current_dir(root).args(args).output().context("run gh")?;
    anyhow::ensure!(
        output.status.success(),
        "gh {}: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Posts `body_file` as a comment on the PR, with `attachments` uploaded
/// by `gh pr comment --attach` (gh 2.99 or newer). Returns the comment URL.
pub fn post_pr_comment(root: &Path, pr: u64, body_file: &Path, attachments: &[std::path::PathBuf]) -> Result<String> {
    let mut args = vec!["pr".to_string(), "comment".to_string(), pr.to_string(), "--body-file".to_string()];
    args.push(body_file.display().to_string());
    for attachment in attachments {
        args.push("--attach".to_string());
        args.push(attachment.display().to_string());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match gh(root, &args) {
        Ok(output) => Ok(output.trim().to_string()),
        Err(error) if error.to_string().contains("unknown flag: --attach") => Err(anyhow::anyhow!(
            "This gh is too old to attach screenshots. Update gh to 2.99 or newer."
        )),
        Err(error) => Err(error),
    }
}

/// The labels and the preview of the PR, from `gh`.
pub fn fetch_pr_tools(root: &Path, pr: u64) -> Result<PrTools> {
    let json = gh(root, &["pr", "view", &pr.to_string(), "--json", "labels,comments"])?;
    let pr: GhPr = serde_json::from_str(&json)?;
    let preview = pr
        .comments
        .iter()
        .filter(|comment| comment.author.as_ref().is_some_and(|author| author.login.starts_with("n8n-assistant")))
        .filter_map(|comment| parse_preview(&comment.body))
        .next_back();
    Ok(PrTools {
        labels: pr.labels.into_iter().map(|label| label.name).collect(),
        preview,
    })
}

/// Adds and removes PR labels. A `flag:` label that the repo does not have
/// yet is created first, with a description.
pub fn apply_labels(root: &Path, pr: u64, change: &LabelChange) -> Result<()> {
    for label in change.add.iter().filter(|label| label.starts_with(FLAG_PREFIX)) {
        let key = flag_of(label).unwrap_or(label);
        let description = format!("PR preview: turn on the {key} feature flag");
        if let Err(error) = gh(root, &["label", "create", label, "--color", "5319e7", "--description", &description]) {
            // The label exists already.
            log::info!("plannotator: label {label} not created: {error}");
        }
    }
    let number = pr.to_string();
    let mut args = vec!["pr", "edit", number.as_str()];
    let add = change.add.join(",");
    let remove = change.remove.join(",");
    if !change.add.is_empty() {
        args.extend(["--add-label", add.as_str()]);
    }
    if !change.remove.is_empty() {
        args.extend(["--remove-label", remove.as_str()]);
    }
    gh(root, &args)?;
    Ok(())
}

/// The flags to pick from: PostHog Staging (newest first), then the `flag:`
/// labels of the repo.
pub fn flag_options(root: &Path) -> Vec<String> {
    let mut options = posthog_staging_flags().unwrap_or_else(|error| {
        log::info!("plannotator: no PostHog flags: {error:#}");
        Vec::new()
    });
    if let Ok(json) = gh(root, &["label", "list", "--search", FLAG_PREFIX, "--limit", "200", "--json", "name"]) {
        let labels: Vec<GhLabel> = serde_json::from_str(&json).unwrap_or_default();
        for label in labels {
            if let Some(key) = flag_of(&label.name)
                && !options.iter().any(|option| option == key)
            {
                options.push(key.to_string());
            }
        }
    }
    options
}

fn posthog_staging_flags() -> Result<Vec<String>> {
    let key = std::env::var("POSTHOG_PERSONAL_API_KEY").context("POSTHOG_PERSONAL_API_KEY is not set")?;
    #[derive(Deserialize)]
    struct Page {
        next: Option<String>,
        results: Vec<Flag>,
    }
    #[derive(Deserialize)]
    struct Flag {
        key: String,
        #[serde(default)]
        deleted: bool,
    }
    let mut flags = Vec::new();
    let mut url = Some(format!("{POSTHOG_HOST}/api/projects/{POSTHOG_STAGING_PROJECT}/feature_flags/?limit=200"));
    while let Some(next) = url.take() {
        // The key goes through stdin, so it is not in the process list.
        let mut child = Command::new("curl")
            .args(["-sf", "-K", "-", &next])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write as _;
            writeln!(stdin, "header = \"Authorization: Bearer {key}\"")?;
        }
        let output = child.wait_with_output()?;
        anyhow::ensure!(output.status.success(), "PostHog did not answer");
        let page: Page = serde_json::from_slice(&output.stdout)?;
        flags.extend(page.results.into_iter().filter(|flag| !flag.deleted).map(|flag| flag.key));
        url = page.next;
    }
    flags.sort_by_key(|flag| std::cmp::Reverse(natural_key(flag)));
    Ok(flags)
}

/// Sorts `136_skill_sync` after `99_x`: the leading number counts as a number.
fn natural_key(key: &str) -> (u64, String) {
    let digits: String = key.chars().take_while(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(0), key.to_string())
}

/// The local n8n of an n8n-wt worktree: its port from `../meta.json`, and
/// whether something listens on it.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalN8n {
    /// The n8n-wt worktree name, for `n8n-wt <command> <name>`.
    pub name: String,
    /// The worktree folder, which holds `env.sh` and `meta.json`.
    pub dir: PathBuf,
    pub port: u16,
    pub running: bool,
    /// The shared telemetry capture, when the worktree sends to it.
    pub telemetry: Option<TelemetryCapture>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TelemetryCapture {
    pub port: u16,
    pub running: bool,
}

impl LocalN8n {
    /// The same label as the `dev:be` task that n8n-wt writes to
    /// `.zed/tasks.json`, so Zed treats both as one task.
    pub fn backend_task_label(&self) -> String {
        format!("n8n-wt: dev:be ({})", self.name)
    }

    pub fn backend_command(&self) -> String {
        let env = self.dir.join("env.sh");
        let env = env.to_string_lossy();
        format!("source '{}' && pnpm dev:be", env.replace('\'', "'\\''"))
    }

    /// Starts the shared capture if it is down, then opens its TUI with only
    /// the events of this worktree's n8n.
    pub fn telemetry_command(&self) -> String {
        let name = format!("'{}'", self.name.replace('\'', "'\\''"));
        format!("n8n-wt telemetry up && exec n8n-wt telemetry tail {name}")
    }
}

/// The Linear workspace of the issue links.
const LINEAR_WORKSPACE: &str = "n8n";

pub fn linear_issue_url(key: &str) -> String {
    format!("https://linear.app/{LINEAR_WORKSPACE}/issue/{key}")
}

/// The Linear issue of the worktree: the `issue` field of its `meta.json`,
/// else the key in the branch name, else a key in the PR title. The same
/// order as `n8n-worktree-manager context`, plus the title for a PR whose
/// branch has no key.
pub fn linear_issue(root: Option<&Path>, branch: Option<&str>, pr_title: Option<&str>) -> Option<String> {
    let from_meta = root
        .and_then(|root| root.parent())
        .and_then(|dir| std::fs::read(dir.join("meta.json")).ok())
        .and_then(|text| serde_json::from_slice::<serde_json::Value>(&text).ok())
        .and_then(|meta| meta.get("issue")?.as_str().map(str::to_string));
    from_meta
        .or_else(|| branch.and_then(n8n_worktree_manager::linear::issue_key_from_branch))
        .or_else(|| {
            pr_title?
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
                .find_map(n8n_worktree_manager::linear::issue_key_from_branch)
        })
}

fn port_is_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(200),
    )
    .is_ok()
}

pub fn local_n8n(root: &Path) -> Option<LocalN8n> {
    let dir = root.parent()?.to_path_buf();
    let meta = dir.join("meta.json");
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
    let ports = meta.get("ports")?;
    let port = u16::try_from(ports.get("n8n")?.as_u64()?).ok()?;
    let name = meta.get("name")?.as_str()?.to_string();
    let telemetry = meta
        .get("telemetry")
        .and_then(|telemetry| telemetry.as_bool())
        .unwrap_or(false)
        .then(|| ports.get("telemetry")?.as_u64())
        .flatten()
        .and_then(|port| u16::try_from(port).ok())
        .map(|port| TelemetryCapture { port, running: port_is_open(port) });
    Some(LocalN8n { name, dir, port, running: port_is_open(port), telemetry })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linear_issue_comes_from_meta_then_the_branch_then_the_pr_title() {
        let home = tempfile::tempdir().expect("temp dir");
        let root = home.path().join("context-429").join("n8n");
        std::fs::create_dir_all(&root).expect("checkout");
        let title = Some("feat(core): Review updates (CONTEXT-431)");

        assert_eq!(
            linear_issue(Some(&root), Some("cat-bot/run-43401128653-1"), title).as_deref(),
            Some("CONTEXT-431")
        );
        assert_eq!(
            linear_issue(Some(&root), Some("context-428-show-each-source"), title).as_deref(),
            Some("CONTEXT-428")
        );
        std::fs::write(root.parent().expect("dir").join("meta.json"), r#"{"issue":"CONTEXT-429"}"#)
            .expect("meta");
        assert_eq!(
            linear_issue(Some(&root), Some("context-428-show-each-source"), title).as_deref(),
            Some("CONTEXT-429")
        );
        assert_eq!(linear_issue(None, Some("main"), Some("chore: Bump deps")), None);
    }

    #[test]
    fn reads_the_preview_comment() {
        let body = "<!-- pr-preview -->\n| Key | Value |\n|---|---|\n| URL | https://pr-1.app.n8n.cloud |\n| Flags | `{\"136_skill_sync\":true,\"old\":false,\"ab\":\"test\"}` |\n";
        let preview = parse_preview(body).unwrap();
        assert_eq!(preview.url, "https://pr-1.app.n8n.cloud");
        assert_eq!(preview.flags, vec!["136_skill_sync".to_string(), "ab".to_string()]);
        assert!(parse_preview("no marker").is_none());
    }

    #[test]
    fn flag_states_and_label_change() {
        let tools = PrTools {
            labels: vec!["flag:a".into(), "flag:b=test".into(), "bug".into()],
            preview: Some(Preview { url: "https://x".into(), flags: vec!["a".into()] }),
        };
        assert_eq!(
            tools.flag_labels(),
            vec![("flag:a".into(), FlagState::InPreview), ("flag:b=test".into(), FlagState::Pending)]
        );
        let change = tools.label_change(&["a".into(), "c".into()]);
        assert_eq!(change.add, vec!["flag:c".to_string()]);
        assert_eq!(change.remove, vec!["flag:b=test".to_string()]);

        let no_preview = PrTools { labels: vec![], preview: None };
        assert_eq!(no_preview.label_change(&["x".into()]).add, vec!["cloud-preview".to_string(), "flag:x".into()]);
    }

    #[test]
    fn newest_flag_numbers_come_first() {
        let mut keys = vec!["99_x", "136_skill_sync", "mcp"];
        keys.sort_by_key(|key| std::cmp::Reverse(natural_key(key)));
        assert_eq!(keys, vec!["136_skill_sync", "99_x", "mcp"]);
    }

    #[test]
    fn reads_the_worktree_and_its_telemetry_capture() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let write_meta = |telemetry: bool| {
            let meta = serde_json::json!({
                "name": "context-433",
                "telemetry": telemetry,
                "ports": { "n8n": 1, "telemetry": 2 },
            });
            std::fs::write(dir.path().join("meta.json"), meta.to_string())
        };
        let root = dir.path().join("n8n");

        write_meta(true)?;
        let local = local_n8n(&root).context("no local n8n")?;
        assert_eq!(local.name, "context-433");
        assert_eq!(local.telemetry.map(|capture| capture.port), Some(2));

        write_meta(false)?;
        assert_eq!(local_n8n(&root).context("no local n8n")?.telemetry, None);

        let local = LocalN8n {
            name: "it's".to_string(),
            dir: PathBuf::from("/wt/it's"),
            port: 1,
            running: false,
            telemetry: None,
        };
        assert_eq!(
            local.telemetry_command(),
            "n8n-wt telemetry up && exec n8n-wt telemetry tail 'it'\\''s'"
        );
        assert_eq!(local.backend_task_label(), "n8n-wt: dev:be (it's)");
        assert_eq!(local.backend_command(), "source '/wt/it'\\''s/env.sh' && pnpm dev:be");
        Ok(())
    }
}
