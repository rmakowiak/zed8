pub mod github;
pub mod linear;
pub mod model;
mod provision;
pub mod prune;
pub mod render;
pub mod services;
pub mod settings;
pub mod store;
pub mod system;

use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use model::{Meta, Ports, Setup};
use settings::Settings;
use store::{Store, read_meta};
use system::{Command, System};

/// The name of the folder of the checkout inside a worktree folder.
pub const CHECKOUT: &str = "n8n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateOptions {
    pub name: String,
    /// An existing branch to check out, or a new branch to make from `base`.
    /// Without it, the branch is the worktree name.
    pub branch: Option<String>,
    pub base: String,
    pub issue: Option<String>,
    pub queue: bool,
    pub sandbox: bool,
    pub telemetry: bool,
    pub build: bool,
    pub source_repo: Option<PathBuf>,
}

impl CreateOptions {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            branch: None,
            base: "master".to_string(),
            issue: None,
            queue: false,
            sandbox: true,
            telemetry: true,
            build: true,
            source_repo: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetOptions {
    /// `Some(None)` removes the issue.
    pub issue: Option<Option<String>>,
    pub queue: Option<bool>,
    /// Shared services to switch on or off, by name: `sandbox`, `telemetry`.
    pub services: Vec<(String, bool)>,
    /// Feature flags of this worktree to set, by key. `None` removes the key.
    pub flags: Vec<(String, Option<serde_json::Value>)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TeardownOptions {
    /// Tear down even with uncommitted changes in the checkout.
    pub force: bool,
    /// Also delete the local branch in the source repo.
    pub delete_branch: bool,
}

pub struct Manager {
    settings: Settings,
    store: Store,
    system: Arc<dyn System>,
}

impl Manager {
    pub fn new(settings: Settings, system: Arc<dyn System>) -> Self {
        let store = Store::new(settings.wt_home.clone());
        Self {
            settings,
            store,
            system,
        }
    }

    /// `docker compose` or `docker-compose`, from the settings.
    pub fn compose(&self) -> Command {
        let (program, args) = self
            .settings
            .compose_command
            .split_first()
            .map(|(program, args)| (program.as_str(), args))
            .unwrap_or(("docker", &[]));
        Command::new(program).args(args.iter().cloned())
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn list(&self) -> Result<Vec<Meta>> {
        self.store.list()
    }

    pub fn show(&self, name: &str) -> Result<Meta> {
        let path = self.store.meta_path(&slugify(name));
        if !path.is_file() {
            bail!("No worktree named '{name}'. See: n8n-worktree-manager list");
        }
        read_meta(&path)
    }

    pub fn worktree_dir(&self, name: &str) -> PathBuf {
        self.settings.worktree_dir(name)
    }

    pub fn checkout_dir(&self, name: &str) -> PathBuf {
        self.worktree_dir(name).join(CHECKOUT)
    }

    /// Claims a slot, adds the git worktree and writes `meta.json` with the
    /// setup pending. Fast: the slow part is `provision`.
    pub fn create(&self, options: CreateOptions) -> Result<Meta> {
        let name = slugify(&options.name);
        if name.is_empty() {
            bail!("The name '{}' is empty after slugify", options.name);
        }
        if self.store.meta_path(&name).is_file() {
            return self.show(&name);
        }
        if let Some(branch) = &options.branch
            && let Some(meta) = self.find_by_branch(branch)?
        {
            return Ok(meta);
        }
        if self.worktree_dir(&name).exists() {
            bail!(
                "{} exists but has no meta.json. Move it away or pick another name.",
                self.worktree_dir(&name).display()
            );
        }
        let source_repo = options
            .source_repo
            .clone()
            .unwrap_or_else(|| self.settings.default_source_repo.clone());
        if !is_n8n_repo(&source_repo) {
            bail!(
                "No n8n checkout at {}. Set N8N_WT_SOURCE_REPO in secrets.env.",
                source_repo.display()
            );
        }
        let checkout = self.checkout_dir(&name);
        let branch = options.branch.clone().unwrap_or_else(|| name.clone());
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(source_repo.to_string_lossy())
                .args(args.iter().copied())
        };

        let origin = self.fetch_url(&source_repo)?;
        let fetch = |branch: &str| {
            git(&[
                "fetch",
                "--quiet",
                &origin,
                &format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
            ])
        };

        let usable = |slot: u16| {
            Ports::for_slot(slot)
                .own()
                .iter()
                .all(|port| self.system.port_free(*port))
        };
        let meta = self.store.claim(usable, |slot| {
            let mut meta = Meta::new(
                &name,
                slot,
                &branch,
                &options.base,
                &source_repo.to_string_lossy(),
            );
            meta.queue = options.queue;
            meta.sandbox = options.sandbox;
            meta.telemetry = options.telemetry;
            meta.issue = options.issue.clone();
            meta.setup = Some(Setup::pending(options.build));
            meta
        })?;

        let checkout_text = checkout.to_string_lossy().into_owned();
        let added = (|| -> Result<()> {
            if options.branch.is_some() {
                // A missing remote branch is normal for a new Linear branch.
                self.system.run(&fetch(&branch))?;
                let local_ref = format!("refs/heads/{branch}");
                if self
                    .system
                    .run(&git(&["show-ref", "--verify", "--quiet", &local_ref]))?
                    .success
                {
                    self.run(&git(&[
                        "worktree",
                        "add",
                        &checkout_text,
                        &branch,
                        "--quiet",
                    ]))?;
                    return Ok(());
                }
                let remote_ref = format!("refs/remotes/origin/{branch}");
                if self
                    .system
                    .run(&git(&["show-ref", "--verify", "--quiet", &remote_ref]))?
                    .success
                {
                    let remote = format!("origin/{branch}");
                    self.run(&git(&[
                        "worktree",
                        "add",
                        &checkout_text,
                        "-b",
                        &branch,
                        &remote,
                        "--quiet",
                    ]))?;
                    return Ok(());
                }
            }
            self.run(&fetch(&options.base))?;
            let new_from = format!("origin/{}", options.base);
            self.run(&git(&[
                "worktree",
                "add",
                &checkout_text,
                "-b",
                &branch,
                &new_from,
                "--quiet",
            ]))?;
            Ok(())
        })();
        if let Err(error) = added {
            // Nothing else exists yet, so the claim goes away with the folder.
            std::fs::remove_dir_all(self.worktree_dir(&name)).ok();
            return Err(error);
        }
        Ok(meta)
    }

    /// The Linear client with the key from `secrets.env`.
    pub fn linear(&self) -> linear::Linear {
        linear::Linear::new(Box::new(linear::CurlTransport {
            api_key: self.settings.linear_api_key.clone(),
        }))
    }

    /// My open issues joined with the worktrees. Also refreshes the cache
    /// that `context` reads.
    pub fn issues(&self, linear: &linear::Linear) -> Result<Vec<linear::IssueRow>> {
        let issues = linear.my_issues()?;
        let cache = linear::cache_path(&self.settings.wt_home);
        store::write_atomic(&cache, &(serde_json::to_string_pretty(&issues)? + "\n"))?;
        Ok(linear::join(issues, &self.list()?))
    }

    /// A project, and its issues joined with the worktrees, in the same
    /// order as `project.issues`.
    pub fn project(
        &self,
        linear: &linear::Linear,
        id: &str,
    ) -> Result<(linear::ProjectView, Vec<linear::IssueRow>)> {
        let project = linear.project_view(id)?;
        let issues = project.issues.iter().map(|issue| issue.issue.clone()).collect();
        let rows = linear::join(issues, &self.list()?);
        Ok((project, rows))
    }

    /// The issues of the last `issues` call, from the cache file, joined with
    /// the worktrees of now. No network, so a panel can show them at once.
    pub fn cached_issues(&self) -> Result<Vec<linear::IssueRow>> {
        let issues = linear::read_cache(&self.settings.wt_home);
        Ok(linear::join(issues, &self.list()?))
    }

    /// Moves the issue to In Progress, then creates its worktree on the
    /// Linear branch name, with the issue key in `meta.json`. Provision is
    /// the next step.
    /// Creates the worktree of an issue on its Linear branch. With
    /// `move_issue`, an issue that is not started moves to In Progress first.
    /// Without it, nothing is written to Linear.
    pub fn start(&self, linear: &linear::Linear, key: &str, move_issue: bool) -> Result<Meta> {
        let started = if move_issue {
            linear.start_issue(key)?
        } else {
            linear.issue_for_start(key)?
        };
        let mut options = CreateOptions::new(&started.identifier.to_lowercase());
        // A remote agent such as catbot opens its PR on its own branch. The
        // worktree goes on that branch, not on a new Linear branch.
        let pr_branch = self.open_pr_branch(&started.identifier, &started.branch_name)?;
        match pr_branch {
            Some(branch) => options.branch = Some(branch),
            None if !started.branch_name.is_empty() => options.branch = Some(started.branch_name.clone()),
            None => {}
        }
        options.issue = Some(started.identifier.clone());
        let meta = self.create(options)?;
        if meta.issue.as_deref() != Some(started.identifier.as_str()) {
            return self.set(
                &meta.name,
                SetOptions {
                    issue: Some(Some(started.identifier)),
                    ..SetOptions::default()
                },
            );
        }
        Ok(meta)
    }

    /// The branch of an open PR that names the issue, from this repository.
    /// A PR on the Linear branch wins; else the newest PR.
    fn open_pr_branch(&self, key: &str, linear_branch: &str) -> Result<Option<String>> {
        let output = self.system.run(&Command::new("gh").args([
            "pr",
            "list",
            "--repo",
            "n8n-io/n8n",
            "--state",
            "open",
            "--search",
            key,
            "--json",
            "number,headRefName,isCrossRepository,title,body",
            "--limit",
            "10",
        ]))?;
        if !output.success {
            // Without GitHub, the Linear branch still works.
            eprintln!("warning: gh pr list for {key} failed: {}", output.stderr.trim());
            return Ok(None);
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct OpenPr {
            head_ref_name: String,
            #[serde(default)]
            is_cross_repository: bool,
            #[serde(default)]
            title: String,
            #[serde(default)]
            body: String,
        }
        let prs: Vec<OpenPr> = match output.stdout.trim() {
            "" => Vec::new(),
            text => serde_json::from_str(text).with_context(|| format!("reading the open PRs of {key}"))?,
        };
        let names_key = |text: &str| {
            let upper = text.to_uppercase();
            upper.match_indices(key).any(|(at, _)| {
                let after = upper[at + key.len()..].chars().next();
                let before = upper[..at].chars().next_back();
                !after.is_some_and(|c| c.is_ascii_alphanumeric())
                    && !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-')
            })
        };
        let candidates: Vec<OpenPr> = prs
            .into_iter()
            .filter(|pr| {
                !pr.is_cross_repository
                    && (names_key(&pr.title) || names_key(&pr.body) || names_key(&pr.head_ref_name))
            })
            .collect();
        if candidates.iter().any(|pr| !linear_branch.is_empty() && pr.head_ref_name == linear_branch) {
            return Ok(Some(linear_branch.to_string()));
        }
        Ok(candidates.into_iter().next().map(|pr| pr.head_ref_name))
    }

    /// The context text for the worktree that contains `dir`, or an empty
    /// text when `dir` is not in a worktree. The SessionStart hook prints it.
    pub fn context(&self, dir: &Path) -> Result<String> {
        let Some(meta) = self.find_by_dir(dir)? else {
            return Ok(String::new());
        };
        let cache = linear::read_cache(&self.settings.wt_home);
        let key = meta
            .issue
            .clone()
            .or_else(|| linear::issue_key_from_branch(&meta.branch));
        let issue = key.and_then(|key| cache.into_iter().find(|issue| issue.identifier == key));
        Ok(render::context(&self.settings, &meta, issue.as_ref()))
    }

    /// The worktree whose folder contains `dir`.
    pub fn find_by_dir(&self, dir: &Path) -> Result<Option<Meta>> {
        let envs = self.store.envs_dir();
        let Ok(envs) = envs.canonicalize() else {
            return Ok(None);
        };
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        for ancestor in dir.ancestors() {
            if ancestor.parent() == Some(envs.as_path()) && ancestor.join("meta.json").is_file() {
                return read_meta(&ancestor.join("meta.json")).map(Some);
            }
        }
        Ok(None)
    }

    /// Changes a worktree and rewrites its generated files. A running dev:be
    /// must restart to see a new env.sh.
    pub fn set(&self, name: &str, options: SetOptions) -> Result<Meta> {
        let mut meta = self.show(name)?;
        if let Some(issue) = options.issue {
            meta.issue = issue.map(|key| key.trim().to_uppercase());
        }
        let queue_changed = options.queue.is_some_and(|queue| queue != meta.queue);
        if let Some(queue) = options.queue {
            meta.queue = queue;
        }
        for (service, enabled) in &options.services {
            match service.as_str() {
                "sandbox" => meta.sandbox = *enabled,
                "telemetry" => meta.telemetry = *enabled,
                other => bail!("Unknown service '{other}'. Known: sandbox, telemetry."),
            }
        }
        for (key, value) in options.flags {
            match value {
                Some(value) => meta.flags.insert(key, value),
                None => meta.flags.remove(&key),
            };
        }
        self.store.write(&meta)?;
        if self.worktree_dir(&meta.name).join("env.sh").is_file() {
            self.write_files(&meta)?;
            if queue_changed && meta.queue {
                self.compose_up(&meta)?;
            }
        }
        Ok(meta)
    }

    /// The repo that holds the git data of a checkout. It asks git first,
    /// because `source_repo` in meta.json can name a repo that is gone.
    fn source_repo_of(&self, checkout: &Path, meta: &Meta) -> PathBuf {
        if checkout.is_dir() {
            let output = self.system.run(
                &Command::new("git")
                    .arg("-C")
                    .arg(checkout.to_string_lossy())
                    .args(["rev-parse", "--path-format=absolute", "--git-common-dir"]),
            );
            match output {
                Ok(output) if output.success => {
                    let common_dir = output.stdout.trim();
                    if let Some(repo) = common_dir.strip_suffix("/.git") {
                        return PathBuf::from(repo);
                    }
                }
                Ok(_) => {}
                Err(error) => eprintln!("warning: {error:#}"),
            }
        }
        PathBuf::from(&meta.source_repo)
    }

    /// The worktree whose checkout is on `branch`. It reads git, not
    /// meta.json, because the branch can change after create.
    fn find_by_branch(&self, branch: &str) -> Result<Option<Meta>> {
        for meta in self.list()? {
            let checkout = self.checkout_dir(&meta.name);
            let output = self.system.run(
                &Command::new("git")
                    .arg("-C")
                    .arg(checkout.to_string_lossy())
                    .args(["branch", "--show-current"]),
            )?;
            if output.success && output.stdout.trim() == branch {
                return Ok(Some(meta));
            }
        }
        Ok(None)
    }

    /// The HTTPS URL of a GitHub origin. A shell that Raycast starts has no
    /// unlocked SSH key, and n8n-io/n8n is public, so HTTPS needs no key.
    fn fetch_url(&self, source_repo: &Path) -> Result<String> {
        let output = self.system.run(
            &Command::new("git")
                .arg("-C")
                .arg(source_repo.to_string_lossy())
                .args(["remote", "get-url", "origin"]),
        )?;
        let url = output.stdout.trim();
        if !output.success || url.is_empty() {
            return Ok("origin".to_string());
        }
        let https = if let Some(path) = url.strip_prefix("git@github.com:") {
            format!("https://github.com/{path}")
        } else if let Some(path) = url.strip_prefix("ssh://git@github.com/") {
            format!("https://github.com/{path}")
        } else {
            url.to_string()
        };
        Ok(https)
    }

    /// Rewrites the generated files of a worktree: env.sh, the compose files,
    /// the Zed files and the badge.
    pub fn sync(&self, name: &str) -> Result<Meta> {
        let meta = self.show(name)?;
        self.write_files(&meta)?;
        Ok(meta)
    }

    /// Stops the containers and deletes their database, removes the git
    /// worktree and deletes the folder. The slot is free once the folder is
    /// gone.
    pub fn teardown(&self, name: &str, options: TeardownOptions) -> Result<()> {
        let meta = self.show(name)?;
        let dir = self.worktree_dir(&meta.name);
        let checkout = dir.join(CHECKOUT);
        let git_in = |repo: &Path, args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(repo.to_string_lossy())
                .args(args.iter().copied())
        };
        if checkout.is_dir() && !options.force {
            let status = self
                .system
                .run(&git_in(&checkout, &["status", "--porcelain"]))?;
            if !status.stdout.trim().is_empty() {
                bail!(
                    "'{}' has uncommitted changes in {}. Commit them, or tear down with --force to lose them.",
                    meta.name,
                    checkout.display()
                );
            }
        }
        // By project name, so it also works when docker-compose.yml is gone.
        let down = self.compose().args([
            "-p",
            &render::compose_project(&meta.name),
            "down",
            "-v",
            "--remove-orphans",
        ]);
        if let Err(error) = self.run(&down) {
            eprintln!("warning: {error:#}");
        }
        let source_repo = self.source_repo_of(&checkout, &meta);
        if checkout.is_dir() && !source_repo.as_os_str().is_empty() {
            let checkout_text = checkout.to_string_lossy().into_owned();
            if let Err(error) = self.run(&git_in(
                &source_repo,
                &["worktree", "remove", &checkout_text, "--force"],
            )) {
                eprintln!("warning: {error:#}");
            }
        }
        if options.delete_branch && !source_repo.as_os_str().is_empty() {
            if let Err(error) = self.run(&git_in(&source_repo, &["branch", "-D", &meta.branch])) {
                eprintln!("warning: {error:#}");
            }
        }
        std::fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))
    }

    /// Runs a command and fails with its stderr when it fails.
    fn run(&self, command: &Command) -> Result<String> {
        let output = self.system.run(command)?;
        if !output.success {
            bail!("{} failed: {}", command.line(), output.stderr.trim());
        }
        Ok(output.stdout)
    }
}

pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for character in name.to_lowercase().chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

pub fn is_n8n_repo(path: &Path) -> bool {
    path.join("pnpm-workspace.yaml").is_file() && path.join("packages/cli").is_dir()
}
