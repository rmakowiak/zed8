use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Sends one GraphQL request body and returns the answer body.
pub trait Transport: Send + Sync {
    fn post(&self, body: &str) -> Result<String>;
}

/// The Linear GraphQL API, through curl. The key goes to curl on stdin, so
/// it never shows in the process list.
pub struct CurlTransport {
    pub api_key: String,
}

#[allow(
    clippy::disallowed_methods,
    reason = "blocking on purpose: the CLI is synchronous, and Zed8 calls the library on a background thread"
)]
impl Transport for CurlTransport {
    fn post(&self, body: &str) -> Result<String> {
        use std::io::Write as _;
        if self.api_key.is_empty() {
            bail!(
                "No Linear API key. Add LINEAR_API_KEY to secrets.env (Linear > Settings > Security & access > Personal API keys)."
            );
        }
        let config = format!(
            "url = \"https://api.linear.app/graphql\"\nheader = \"Content-Type: application/json\"\nheader = \"Authorization: {}\"\ndata-binary = \"{}\"\n",
            curl_escape(&self.api_key),
            curl_escape(body)
        );
        let mut child = std::process::Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--max-time",
                "20",
                "--config",
                "-",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("starting curl")?;
        child
            .stdin
            .take()
            .context("curl stdin")?
            .write_all(config.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!(
                "curl to Linear failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn curl_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Answers with the given bodies in order and records every request.
#[derive(Clone, Default)]
pub struct FakeTransport {
    answers: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeTransport {
    pub fn with_answers<'a>(answers: impl IntoIterator<Item = &'a str>) -> Self {
        let transport = Self::default();
        if let Ok(mut queue) = transport.answers.lock() {
            queue.extend(answers.into_iter().map(str::to_string));
        }
        transport
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }
}

impl Transport for FakeTransport {
    fn post(&self, body: &str) -> Result<String> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(body.to_string());
        }
        self.answers
            .lock()
            .map_err(|_| anyhow!("fake answers lock poisoned"))?
            .pop_front()
            .context("the fake has no answer left")
    }
}

pub struct Linear {
    transport: Box<dyn Transport>,
}

const ISSUE_FIELDS: &str = "identifier title url branchName priority state { name type } project { name } assignee { name }";

impl Linear {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self { transport }
    }

    /// Sends a query and returns its `data`, or the GraphQL errors as an error.
    pub fn query(&self, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables }).to_string();
        let answer: Value = serde_json::from_str(&self.transport.post(&body)?)
            .context("parsing the Linear answer")?;
        if let Some(errors) = answer.get("errors").and_then(Value::as_array) {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|error| error.get("message").and_then(Value::as_str))
                .collect();
            bail!("Linear: {}", messages.join("; "));
        }
        answer
            .get("data")
            .cloned()
            .context("the Linear answer has no data")
    }

    /// The open issues assigned to me.
    pub fn my_issues(&self) -> Result<Vec<IssueSummary>> {
        let query = format!(
            r#"query {{ viewer {{ assignedIssues(first: 100, filter: {{ state: {{ type: {{ nin: ["completed", "canceled"] }} }} }}) {{ nodes {{ {ISSUE_FIELDS} }} }} }} }}"#
        );
        let data = self.query(&query, json!({}))?;
        Ok(nodes(&data["viewer"]["assignedIssues"])
            .iter()
            .map(issue_summary)
            .collect())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub url: String,
    pub state: String,
    pub progress: f64,
    /// True for a project that I am a member of.
    pub mine: bool,
    #[serde(default)]
    pub lead_is_me: bool,
    /// The key of the project's first team, such as CONTEXT.
    #[serde(default)]
    pub team: Option<String>,
}

const PROJECT_FIELDS: &str = "id name url state progress lead { isMe } teams(first: 1) { nodes { key } }";

impl Linear {
    /// My projects. With a search text: my matching projects first, then
    /// every other matching project of the workspace.
    pub fn projects(&self, search: Option<&str>) -> Result<Vec<ProjectSummary>> {
        let data = match search {
            Some(search) => self.query(
                &format!(
                    "query($search: String!) {{ mine: projects(first: 50, filter: {{ members: {{ isMe: {{ eq: true }} }}, name: {{ containsIgnoreCase: $search }} }}) {{ nodes {{ {PROJECT_FIELDS} }} }} all: projects(first: 50, filter: {{ name: {{ containsIgnoreCase: $search }} }}) {{ nodes {{ {PROJECT_FIELDS} }} }} }}"
                ),
                json!({ "search": search }),
            )?,
            None => self.query(
                &format!(
                    "query {{ mine: projects(first: 50, filter: {{ members: {{ isMe: {{ eq: true }} }} }}) {{ nodes {{ {PROJECT_FIELDS} }} }} }}"
                ),
                json!({}),
            )?,
        };
        let mut projects: Vec<ProjectSummary> = nodes(&data["mine"])
            .iter()
            .map(|node| project_summary(node, true))
            .collect();
        for node in nodes(&data["all"]) {
            let project = project_summary(&node, false);
            if !projects.iter().any(|known| known.id == project.id) {
                projects.push(project);
            }
        }
        Ok(projects)
    }

    /// One project for the preview: milestones, issues, documents and the
    /// latest update.
    pub fn project(&self, id: &str) -> Result<Value> {
        let data = self.query(
            &format!(
                "query($id: String!) {{ project(id: $id) {{ {PROJECT_FIELDS} description targetDate lead {{ name }} projectMilestones {{ nodes {{ name targetDate progress sortOrder }} }} issues(first: 100) {{ nodes {{ {ISSUE_FIELDS} projectMilestone {{ name }} }} }} documents {{ nodes {{ title url }} }} projectUpdates(first: 1) {{ nodes {{ body createdAt user {{ name }} }} }} }} }}"
            ),
            json!({ "id": id }),
        )?;
        not_null(&data["project"], &format!("No Linear project {id}"))
    }

    /// The project of `project`, typed for a panel. Issues keep the order
    /// of Linear.
    pub fn project_view(&self, id: &str) -> Result<ProjectView> {
        let project = self.project(id)?;
        // Linear returns the milestones in no useful order.
        let mut milestone_nodes = nodes(&project["projectMilestones"]);
        milestone_nodes.sort_by(|a, b| {
            let order = |node: &Value| node["sortOrder"].as_f64().unwrap_or_default();
            order(a).total_cmp(&order(b))
        });
        let milestones = milestone_nodes
            .iter()
            .map(|node| Milestone {
                name: text(&node["name"]).unwrap_or_default(),
                percent: node["progress"].as_f64().unwrap_or_default(),
                target_date: text(&node["targetDate"]),
            })
            .collect();
        let issues = nodes(&project["issues"])
            .iter()
            .map(|node| ProjectIssue {
                issue: issue_summary(node),
                milestone: text(&node["projectMilestone"]["name"]),
                finished: matches!(
                    node["state"]["type"].as_str(),
                    Some("completed" | "canceled" | "duplicate")
                ),
            })
            .collect();
        let documents = nodes(&project["documents"])
            .iter()
            .map(|node| Link {
                title: text(&node["title"]).unwrap_or_default(),
                url: text(&node["url"]).unwrap_or_default(),
            })
            .collect();
        let latest_update = nodes(&project["projectUpdates"])
            .first()
            .map(|node| ProjectUpdate {
                author: text(&node["user"]["name"]).unwrap_or_default(),
                created_at: text(&node["createdAt"]).unwrap_or_default(),
                body: text(&node["body"]).unwrap_or_default(),
            });
        Ok(ProjectView {
            summary: project_summary(&project, false),
            name: text(&project["name"]).unwrap_or_default(),
            description: text(&project["description"]).filter(|text| !text.trim().is_empty()),
            target_date: text(&project["targetDate"]),
            lead: text(&project["lead"]["name"]),
            milestones,
            issues,
            documents,
            latest_update,
        })
    }

    /// One issue for the preview: description, sub-issues, comments, links.
    pub fn issue(&self, key: &str) -> Result<Value> {
        let key = key.trim().to_uppercase();
        let data = self.query(
            &format!(
                "query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} assignee {{ isMe }} projectMilestone {{ name progress }} project {{ targetDate documents {{ nodes {{ title url }} }} }} description createdAt updatedAt parent {{ identifier title }} children {{ nodes {{ identifier title state {{ name }} }} }} comments {{ nodes {{ body createdAt user {{ name }} }} }} attachments {{ nodes {{ title url }} }} }} }}"
            ),
            json!({ "id": key }),
        )?;
        not_null(&data["issue"], &format!("No Linear issue {key}"))
    }
}

fn not_null(value: &Value, message: &str) -> Result<Value> {
    if value.is_null() {
        bail!("{message}");
    }
    Ok(value.clone())
}

fn project_summary(node: &Value, mine: bool) -> ProjectSummary {
    ProjectSummary {
        id: text(&node["id"]).unwrap_or_default(),
        name: text(&node["name"]).unwrap_or_default(),
        url: text(&node["url"]).unwrap_or_default(),
        state: text(&node["state"]).unwrap_or_default(),
        progress: node["progress"].as_f64().unwrap_or_default(),
        mine,
        lead_is_me: node["lead"]["isMe"].as_bool().unwrap_or_default(),
        team: nodes(&node["teams"])
            .first()
            .and_then(|team| text(&team["key"])),
    }
}

/// A project for the Work panel and its preview tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectView {
    pub summary: ProjectSummary,
    pub name: String,
    pub description: Option<String>,
    pub target_date: Option<String>,
    pub lead: Option<String>,
    pub milestones: Vec<Milestone>,
    pub issues: Vec<ProjectIssue>,
    pub documents: Vec<Link>,
    pub latest_update: Option<ProjectUpdate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Milestone {
    pub name: String,
    /// From 0 to 100. A project's progress is from 0 to 1.
    pub percent: f64,
    pub target_date: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectIssue {
    pub issue: IssueSummary,
    pub milestone: Option<String>,
    /// Completed, canceled or a duplicate.
    pub finished: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub title: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectUpdate {
    pub author: String,
    pub created_at: String,
    pub body: String,
}

/// The result of `start_issue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartedIssue {
    pub identifier: String,
    pub title: String,
    pub branch_name: String,
    /// False when the issue was started already, so nothing was written.
    pub moved: bool,
}

impl Linear {
    /// Moves an issue to the "In Progress" state of its team. A team without
    /// that name gets its first "started" state.
    pub fn start_issue(&self, key: &str) -> Result<StartedIssue> {
        self.start_issue_with(key, true)
    }

    /// The issue's key, title and branch, with no write: for a worktree on
    /// someone else's issue, for example to review it.
    pub fn issue_for_start(&self, key: &str) -> Result<StartedIssue> {
        self.start_issue_with(key, false)
    }

    fn start_issue_with(&self, key: &str, move_issue: bool) -> Result<StartedIssue> {
        let key = key.trim().to_uppercase();
        let data = self.query(
            "query($id: String!) { issue(id: $id) { id identifier title branchName state { id name type } team { states { nodes { id name type position } } } } }",
            json!({ "id": key }),
        )?;
        let issue = &data["issue"];
        if issue.is_null() {
            bail!("No Linear issue {key}");
        }
        let mut started = StartedIssue {
            identifier: text(&issue["identifier"]).unwrap_or(key),
            title: text(&issue["title"]).unwrap_or_default(),
            branch_name: text(&issue["branchName"]).unwrap_or_default(),
            moved: false,
        };
        let state_type = issue["state"]["type"].as_str().unwrap_or_default();
        if !move_issue || matches!(state_type, "started" | "completed" | "canceled") {
            return Ok(started);
        }
        let mut states = nodes(&issue["team"]["states"]);
        states.sort_by(|left, right| {
            let position = |state: &Value| state["position"].as_f64().unwrap_or_default();
            position(left).total_cmp(&position(right))
        });
        let target = states
            .iter()
            .find(|state| state["name"].as_str() == Some("In Progress"))
            .or_else(|| {
                states
                    .iter()
                    .find(|state| state["type"].as_str() == Some("started"))
            })
            .and_then(|state| text(&state["id"]))
            .context("the team of the issue has no started state")?;
        let id = text(&issue["id"]).context("the issue has no id")?;
        let data = self.query(
            "mutation($id: String!, $stateId: String!) { issueUpdate(id: $id, input: { stateId: $stateId }) { success } }",
            json!({ "id": id, "stateId": target }),
        )?;
        if data["issueUpdate"]["success"].as_bool() != Some(true) {
            bail!("Linear did not move {} to In Progress", started.identifier);
        }
        started.moved = true;
        Ok(started)
    }
}

fn nodes(connection: &Value) -> Vec<Value> {
    connection
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn issue_summary(node: &Value) -> IssueSummary {
    IssueSummary {
        identifier: text(&node["identifier"]).unwrap_or_default(),
        title: text(&node["title"]).unwrap_or_default(),
        status: text(&node["state"]["name"]).unwrap_or_default(),
        status_type: text(&node["state"]["type"]).unwrap_or_default(),
        project: text(&node["project"]["name"]),
        url: text(&node["url"]).unwrap_or_default(),
        branch_name: text(&node["branchName"]),
        assignee: text(&node["assignee"]["name"]),
        priority: node["priority"]
            .as_u64()
            .and_then(|priority| u8::try_from(priority).ok())
            .unwrap_or_default(),
    }
}

/// The fields of a Linear issue that lists, the cache and `context` use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSummary {
    pub identifier: String,
    pub title: String,
    pub status: String,
    /// The Linear state type: triage, backlog, unstarted, started,
    /// completed or canceled. Caches from before it have none.
    #[serde(default)]
    pub status_type: String,
    pub project: Option<String>,
    pub url: String,
    pub branch_name: Option<String>,
    pub assignee: Option<String>,
    pub priority: u8,
}

/// The issue key at the start of the last part of a branch name:
/// `rmakowiak/context-424-title` gives `CONTEXT-424`. The number has at most
/// six digits, so a bot branch like `run-43026071447-1` gives nothing.
pub fn issue_key_from_branch(branch: &str) -> Option<String> {
    let last = branch.rsplit('/').next()?;
    let (team, rest) = last.split_once('-')?;
    if team.is_empty()
        || !team
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        return None;
    }
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest[digits.len()..].chars().next();
    if digits.is_empty() || digits.len() > 6 || after.is_some_and(|character| character != '-') {
        return None;
    }
    Some(format!("{}-{digits}", team.to_uppercase()))
}

pub fn cache_path(wt_home: &std::path::Path) -> std::path::PathBuf {
    wt_home.join("cache").join("linear-issues.json")
}

/// The issues of the last `linear issues` call. Empty when there is no
/// cache yet, so `context` works offline and without a key.
pub fn read_cache(wt_home: &std::path::Path) -> Vec<IssueSummary> {
    std::fs::read_to_string(cache_path(wt_home))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// An issue with the worktree that works on it, if any.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRow {
    #[serde(flatten)]
    pub issue: IssueSummary,
    pub worktree: Option<String>,
    /// The setup summary of the worktree: "pending", "failed at install", "done".
    pub setup: Option<String>,
    /// The setup steps done and the steps it runs. A worktree from before
    /// the setup record counts as done.
    pub setup_progress: Option<(usize, usize)>,
}

/// Links each issue to its worktree: by the `issue` field of `meta.json`,
/// then by the issue key in the branch name, then by the Linear branch name.
pub fn join(issues: Vec<IssueSummary>, worktrees: &[crate::model::Meta]) -> Vec<IssueRow> {
    issues
        .into_iter()
        .map(|issue| {
            let worktree = worktrees
                .iter()
                .find(|meta| meta.issue.as_deref() == Some(issue.identifier.as_str()))
                .or_else(|| {
                    worktrees.iter().find(|meta| {
                        issue_key_from_branch(&meta.branch).as_deref()
                            == Some(issue.identifier.as_str())
                    })
                })
                .or_else(|| {
                    worktrees
                        .iter()
                        .find(|meta| issue.branch_name.as_deref() == Some(meta.branch.as_str()))
                });
            IssueRow {
                worktree: worktree.map(|meta| meta.name.clone()),
                setup: worktree.map(|meta| {
                    meta.setup
                        .as_ref()
                        .map(|setup| setup.summary())
                        .unwrap_or_else(|| "done".to_string())
                }),
                setup_progress: worktree.map(|meta| match &meta.setup {
                    Some(setup) => setup.progress(),
                    None => (crate::model::Step::ALL.len(), crate::model::Step::ALL.len()),
                }),
                issue,
            }
        })
        .collect()
}
