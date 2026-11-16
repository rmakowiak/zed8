use serde::{Deserialize, Serialize};

pub const MAX_SLOTS: u16 = 50;
pub const SANDBOX_PORT: u16 = 19080;
pub const TELEMETRY_PORT: u16 = 19081;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ports {
    pub postgres: u16,
    pub redis: u16,
    pub mailpit_smtp: u16,
    pub mailpit_http: u16,
    pub pgweb: u16,
    pub n8n: u16,
    pub editor: u16,
    pub broker: u16,
    pub sandbox: u16,
    pub telemetry: u16,
}

impl Ports {
    /// The ports of this slot only. The sandbox and telemetry ports are
    /// shared by all worktrees.
    pub fn own(&self) -> [u16; 8] {
        [
            self.postgres,
            self.redis,
            self.mailpit_smtp,
            self.mailpit_http,
            self.pgweb,
            self.n8n,
            self.editor,
            self.broker,
        ]
    }

    pub fn for_slot(slot: u16) -> Self {
        Self {
            postgres: 25432 + slot,
            redis: 26379 + slot,
            mailpit_smtp: 21025 + slot,
            mailpit_http: 28025 + slot,
            pgweb: 27432 + slot,
            n8n: 25678 + slot,
            editor: 28080 + slot,
            // Far from the n8n base: at n8n + 1, the broker of one slot took
            // the n8n port of the next slot.
            broker: 29679 + slot,
            sandbox: SANDBOX_PORT,
            telemetry: TELEMETRY_PORT,
        }
    }
}

/// The `meta.json` of one worktree. Old files have keys that this version
/// no longer knows (for example `signoz`), so unknown keys go to `extra` and
/// are written back unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    pub slot: u16,
    pub branch: String,
    #[serde(default)]
    pub base: String,
    pub queue: bool,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub telemetry: bool,
    #[serde(default)]
    pub source_repo: String,
    #[serde(default)]
    pub created_at: String,
    pub ports: Ports,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    /// Feature flag overrides of this worktree only. They are merged into
    /// FEATURE_FLAG_OVERRIDES from secrets.env, and win on the same key.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub flags: serde_json::Map<String, serde_json::Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Meta {
    /// A new worktree with the defaults of `create`: no queue, sandbox and
    /// telemetry on.
    pub fn new(name: &str, slot: u16, branch: &str, base: &str, source_repo: &str) -> Self {
        Self {
            name: name.to_string(),
            slot,
            branch: branch.to_string(),
            base: base.to_string(),
            queue: false,
            sandbox: true,
            telemetry: true,
            source_repo: source_repo.to_string(),
            created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            ports: Ports::for_slot(slot),
            issue: None,
            setup: None,
            flags: serde_json::Map::new(),
            extra: serde_json::Map::new(),
        }
    }
}

/// The steps of `provision`, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Files,
    ComposeUp,
    WaitPostgres,
    Install,
    Build,
    BadgeBuild,
}

impl Step {
    pub const ALL: [Step; 6] = [
        Step::Files,
        Step::ComposeUp,
        Step::WaitPostgres,
        Step::Install,
        Step::Build,
        Step::BadgeBuild,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Step::Files => "files",
            Step::ComposeUp => "compose up",
            Step::WaitPostgres => "wait for postgres",
            Step::Install => "install",
            Step::Build => "build",
            Step::BadgeBuild => "badge build",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStatus {
    Pending,
    Running,
    Failed,
    Done,
}

/// The progress of `provision`. A `meta.json` without it comes from the Bash
/// version, which set up a worktree in one go, so it counts as done.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub status: SetupStatus,
    #[serde(default)]
    pub done: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default = "default_true")]
    pub build: bool,
}

fn default_true() -> bool {
    true
}

impl Setup {
    pub fn pending(build: bool) -> Self {
        Self {
            status: SetupStatus::Pending,
            done: Vec::new(),
            step: None,
            error: None,
            build,
        }
    }

    /// One line for lists: "pending", "running: install", "failed at install", "done".
    /// The steps done and the steps this setup runs, for "setting up 3/6".
    pub fn progress(&self) -> (usize, usize) {
        let runs = |step: &Step| self.build || !matches!(step, Step::Build | Step::BadgeBuild);
        let total = Step::ALL.iter().filter(|step| runs(step)).count();
        let done = self.done.iter().filter(|step| runs(step)).count();
        (done, total)
    }

    pub fn summary(&self) -> String {
        match (self.status, self.step) {
            (SetupStatus::Running, Some(step)) => format!("running: {}", step.label()),
            (SetupStatus::Failed, Some(step)) => format!("failed at {}", step.label()),
            (SetupStatus::Pending, _) => "pending".to_string(),
            (SetupStatus::Running, None) => "running".to_string(),
            (SetupStatus::Failed, None) => "failed".to_string(),
            (SetupStatus::Done, _) => "done".to_string(),
        }
    }
}
