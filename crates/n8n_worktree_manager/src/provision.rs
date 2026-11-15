use anyhow::{Context as _, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::{Meta, Setup, SetupStatus, Step};
use crate::store::write_atomic;
use crate::system::Command;
use crate::{CHECKOUT, Manager, render};

const COMPOSE_TEMPLATE: &str = include_str!("../templates/docker-compose.yml");
const PGWEB_CSS: &str = include_str!("../templates/pgweb-dark.css");
const BADGE_PLUGIN: &str = include_str!("../templates/vite-plugin-worktree-badge.mts");
const VITE_CONFIG: &str = "packages/frontend/editor-ui/vite.config.mts";
const BADGE_PLUGIN_PATH: &str = "packages/frontend/editor-ui/vite/vite-plugin-worktree-badge.mts";

impl Manager {
    /// Runs the setup steps that are not done yet, in order, and records each
    /// one in `meta.json`. After a failure, the next call starts at the
    /// failed step.
    pub fn provision(&self, name: &str) -> Result<Meta> {
        let mut meta = self.show(name)?;
        let Some(mut setup) = meta.setup.clone() else {
            return Ok(meta);
        };
        for step in Step::ALL {
            if setup.done.contains(&step) {
                continue;
            }
            if !setup.build && matches!(step, Step::Build | Step::BadgeBuild) {
                continue;
            }
            setup.status = SetupStatus::Running;
            setup.step = Some(step);
            setup.error = None;
            self.save_setup(&mut meta, &setup)?;
            if let Err(error) = self.run_step(&meta, step) {
                setup.status = SetupStatus::Failed;
                setup.error = Some(format!("{error:#}"));
                self.save_setup(&mut meta, &setup)?;
                return Err(error.context(format!("'{}' failed at {}", meta.name, step.label())));
            }
            setup.done.push(step);
        }
        setup.status = SetupStatus::Done;
        setup.step = None;
        self.save_setup(&mut meta, &setup)?;
        Ok(meta)
    }

    fn save_setup(&self, meta: &mut Meta, setup: &Setup) -> Result<()> {
        meta.setup = Some(setup.clone());
        self.store.write(meta)
    }

    fn run_step(&self, meta: &Meta, step: Step) -> Result<()> {
        let dir = self.worktree_dir(&meta.name);
        let checkout = dir.join(CHECKOUT);
        match step {
            Step::Files => self.write_files(meta),
            Step::ComposeUp => self.compose_up(meta),
            Step::WaitPostgres => self.wait_for_postgres(&dir),
            Step::Install => self.run(
                &self
                    .pnpm(&checkout, &["install"])
                    .log(dir.join("install.log")),
            ),
            Step::Build => self.run(
                &self
                    .pnpm(&checkout, &["build"])
                    .log(dir.join("build.log")),
            ),
            // turbo drops env vars that turbo.json does not list, so the badge
            // label reaches `vite build` only through a direct pnpm run.
            Step::BadgeBuild => {
                let command = self
                    .pnpm(&checkout, &["--filter", "n8n-editor-ui", "build"])
                    .env("N8N_WT_LABEL", &meta.name)
                    .log(dir.join("build-badge.log"));
                if let Err(error) = self.run(&command) {
                    eprintln!(
                        "warning: the badge build failed, so only dev:fe:editor shows the badge. See {}: {error:#}",
                        dir.join("build-badge.log").display()
                    );
                }
                Ok(String::new())
            }
        }
        .map(|_| ())
    }

    fn pnpm(&self, checkout: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("pnpm")
            .args(args.iter().copied())
            .cwd(checkout)
            .env(
                "TURBO_CACHE_DIR",
                self.settings.turbo_cache_dir().to_string_lossy(),
            );
        if let Some(path) = self.node_path() {
            command = command.env("PATH", path);
        }
        command
    }

    pub(crate) fn node_path_for_services(&self) -> Option<String> {
        self.node_path()
    }

    /// PATH with the Node of nvm first. Zed and Raycast start the binary
    /// without a login shell, so the PATH has no Node.
    fn node_path(&self) -> Option<String> {
        let bin_dir = if self.settings.node_bin_dir.is_empty() {
            nvm_default_bin_dir()?
        } else {
            PathBuf::from(&self.settings.node_bin_dir)
        };
        let path = std::env::var("PATH").unwrap_or_default();
        Some(format!("{}:{path}", bin_dir.display()))
    }

    pub(crate) fn compose_up(&self, meta: &Meta) -> Result<String> {
        let mut command = self.compose();
        if meta.queue {
            command = command.args(["--profile", "queue"]);
        }
        self.run(
            &command
                .args(["up", "-d"])
                .cwd(self.worktree_dir(&meta.name)),
        )
    }

    fn wait_for_postgres(&self, dir: &Path) -> Result<String> {
        let command = self
            .compose()
            .args([
                "exec",
                "-T",
                "postgres",
                "pg_isready",
                "-U",
                "postgres",
            ])
            .cwd(dir);
        for _ in 0..60 {
            if self.system.run(&command)?.success {
                return Ok(String::new());
            }
            self.system.sleep(Duration::from_secs(1));
        }
        bail!("Postgres did not get ready in 60 seconds. See: docker compose logs postgres")
    }

    /// Writes every generated file of a worktree. Safe to run again: `sync`
    /// and `set` call it after a change.
    pub(crate) fn write_files(&self, meta: &Meta) -> Result<String> {
        let dir = self.worktree_dir(&meta.name);
        let checkout = dir.join(CHECKOUT);
        self.apply_badge(&checkout)?;

        write_atomic(&dir.join("docker-compose.yml"), COMPOSE_TEMPLATE)?;
        write_atomic(&dir.join("pgweb-dark.css"), PGWEB_CSS)?;
        write_atomic(&dir.join(".env"), &render::compose_env(meta))?;
        write_atomic(&dir.join("env.sh"), &render::env_sh(&self.settings, meta))?;
        fs::create_dir_all(dir.join("n8n-home"))?;

        let zed_dir = checkout.join(".zed");
        self.exclude_zed_dir(&checkout)?;
        write_atomic(
            &zed_dir.join("tasks.json"),
            &render::zed_tasks(&self.settings, meta),
        )?;
        write_atomic(
            &zed_dir.join("settings.json"),
            &render::zed_settings(&self.settings, meta),
        )?;

        self.init_turbo_cache(&meta.source_repo)?;
        Ok(String::new())
    }

    /// Adds `.zed/` to `.git/info/exclude`, which all worktrees of a repo
    /// share, so git never shows the generated Zed files.
    fn exclude_zed_dir(&self, checkout: &Path) -> Result<()> {
        let common_dir = self.run(
            &Command::new("git")
                .arg("-C")
                .arg(checkout.to_string_lossy())
                .args(["rev-parse", "--path-format=absolute", "--git-common-dir"]),
        )?;
        let common_dir = common_dir.trim();
        if common_dir.is_empty() {
            return Ok(());
        }
        let exclude = Path::new(common_dir).join("info").join("exclude");
        let current = fs::read_to_string(&exclude).unwrap_or_default();
        if current.lines().any(|line| line == ".zed/") {
            return Ok(());
        }
        let mut text = current;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(".zed/\n");
        write_atomic(&exclude, &text)
    }

    /// The editor badge: a colored favicon and a banner with the worktree
    /// name in the editor tab. A local patch of `vite.config.mts` that git
    /// never shows (skip-worktree).
    fn apply_badge(&self, checkout: &Path) -> Result<()> {
        let vite_config = checkout.join(VITE_CONFIG);
        let Ok(text) = fs::read_to_string(&vite_config) else {
            return Ok(());
        };
        write_atomic(&checkout.join(BADGE_PLUGIN_PATH), BADGE_PLUGIN)?;
        if text.contains("worktreeBadgePlugin") {
            return Ok(());
        }
        let Some(patched) = render::patch_vite_config(&text) else {
            eprintln!(
                "warning: could not patch {} (the anchors changed upstream?). No badge.",
                vite_config.display()
            );
            fs::remove_file(checkout.join(BADGE_PLUGIN_PATH)).ok();
            return Ok(());
        };
        fs::write(&vite_config, patched)
            .with_context(|| format!("writing {}", vite_config.display()))?;
        self.run(
            &Command::new("git")
                .arg("-C")
                .arg(checkout.to_string_lossy())
                .args(["update-index", "--skip-worktree", VITE_CONFIG]),
        )?;
        Ok(())
    }

    /// One Turbo cache for all worktrees, seeded once from the source
    /// checkout, so the first build of a new worktree is mostly cache hits.
    fn init_turbo_cache(&self, source_repo: &str) -> Result<()> {
        let cache = self.settings.turbo_cache_dir();
        if cache.exists() {
            return Ok(());
        }
        let source_cache = Path::new(source_repo).join(".turbo").join("cache");
        if source_cache.is_dir() {
            if let Some(parent) = cache.parent() {
                fs::create_dir_all(parent)?;
            }
            self.run(
                &Command::new("cp")
                    .arg("-R")
                    .arg(source_cache.to_string_lossy())
                    .arg(cache.to_string_lossy()),
            )?;
        } else {
            fs::create_dir_all(&cache)?;
        }
        Ok(())
    }
}

/// The bin folder of the Node version that nvm's default alias names.
fn nvm_default_bin_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").ok()?);
    let nvm = home.join(".nvm");
    let alias = fs::read_to_string(nvm.join("alias").join("default")).ok()?;
    let wanted = alias.trim().trim_start_matches('v').to_string();
    let mut matches: Vec<(Vec<u64>, PathBuf)> = fs::read_dir(nvm.join("versions").join("node"))
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version = name.trim_start_matches('v');
            let matches_alias = version == wanted || version.starts_with(&format!("{wanted}."));
            matches_alias.then(|| {
                let numbers = version
                    .split('.')
                    .filter_map(|part| part.parse().ok())
                    .collect();
                (numbers, entry.path().join("bin"))
            })
        })
        .collect();
    matches.sort();
    matches.pop().map(|(_, bin)| bin)
}
