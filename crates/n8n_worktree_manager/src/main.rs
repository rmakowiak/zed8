use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};
use n8n_worktree_manager::render::{self, SCHEMA_VERSION};
use n8n_worktree_manager::services::SERVICES;
use n8n_worktree_manager::settings::Settings;
use n8n_worktree_manager::system::{Command, RealSystem, System as _};
use n8n_worktree_manager::{CreateOptions, Manager, SetOptions, TeardownOptions};
use serde_json::json;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

/// Isolated n8n worktrees: one git worktree, one Postgres, one set of ports
/// each. Text for people, --json for Raycast and scripts.
#[derive(Parser)]
#[command(name = "n8n-worktree-manager", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Show a macOS notification when the command ends, for runs without a
    /// terminal, for example from Raycast.
    #[arg(long, global = true)]
    notify: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// List the worktrees.
    #[command(alias = "ls")]
    List(JsonFlag),
    /// Show one worktree.
    Show {
        name: String,
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Print the issue, ports and URLs of the worktree that holds a folder.
    /// The Claude Code SessionStart hook runs it. Prints nothing outside a worktree.
    Context { dir: Option<PathBuf> },
    /// Create a worktree, then set it up (unless --no-provision).
    Create(CreateArgs),
    /// Run the setup steps that are not done yet. Resumes after a failure.
    Provision { name: String },
    /// Change a worktree and rewrite its generated files.
    Set(SetArgs),
    /// Rewrite the generated files: env.sh, compose files, Zed files, badge.
    Sync {
        name: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Focus the Zed window of the worktree, or open a new window.
    Open { name: String },
    /// Tear down the worktrees whose PR is merged. Skips a worktree with
    /// uncommitted changes or with commits that are not in the merged PR.
    Prune {
        /// Required, so a bare `prune` from the old tool does nothing by surprise.
        #[arg(long, required = true)]
        merged: bool,
        /// Also delete the local branches.
        #[arg(long)]
        delete_branch: bool,
        /// Skip the question.
        #[arg(long)]
        yes: bool,
        /// Only list, as JSON. Tears nothing down.
        #[command(flatten)]
        json: JsonFlag,
    },
    /// Stop the containers, delete the database, remove the git worktree and the folder.
    #[command(aliases = ["rm", "down"])]
    Teardown {
        name: String,
        /// Tear down even with uncommitted changes.
        #[arg(long)]
        force: bool,
        /// Also delete the local branch.
        #[arg(long)]
        delete_branch: bool,
        /// Do not ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// The shared services: sandbox and telemetry.
    Services {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Read Linear. Prints JSON.
    Linear {
        #[command(subcommand)]
        query: LinearQuery,
    },
    /// Move a Linear issue to In Progress, create its worktree and set it up.
    Start {
        key: String,
        #[arg(long)]
        no_provision: bool,
        /// Open the worktree in Zed after create.
        #[arg(long)]
        open: bool,
    },
    /// The old `n8n-wt telemetry` commands.
    #[command(hide = true)]
    Telemetry {
        action: String,
        name: Option<String>,
    },
    /// The old `n8n-wt sandbox` commands.
    #[command(hide = true)]
    Sandbox { action: String },
}

#[derive(Args)]
struct JsonFlag {
    /// Print JSON on stdout.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct CreateArgs {
    name: String,
    /// An existing branch, or a new branch name. Default: the worktree name.
    #[arg(long)]
    branch: Option<String>,
    /// The branch on origin to start a new branch from.
    #[arg(long, default_value = "master")]
    base: String,
    /// The Linear issue key, for example CONTEXT-424.
    #[arg(long)]
    issue: Option<String>,
    /// Queue mode: Redis and EXECUTIONS_MODE=queue.
    #[arg(long)]
    queue: bool,
    #[arg(long)]
    no_sandbox: bool,
    #[arg(long)]
    no_telemetry: bool,
    /// Skip pnpm build in the setup.
    #[arg(long, alias = "skip-build")]
    no_build: bool,
    /// Only create. Run `provision` later.
    #[arg(long)]
    no_provision: bool,
    /// Open the worktree in Zed after create.
    #[arg(long)]
    open: bool,
    /// The old `n8n-wt create --no-open`. Not opening is the default now.
    #[arg(long, hide = true, conflicts_with = "open")]
    no_open: bool,
    #[command(flatten)]
    json: JsonFlag,
}

#[derive(Args)]
struct SetArgs {
    name: String,
    #[arg(long)]
    issue: Option<String>,
    #[arg(long, conflicts_with = "issue")]
    no_issue: bool,
    /// on or off
    #[arg(long)]
    queue: Option<String>,
    /// <service>=on|off, for example sandbox=off. Can repeat.
    #[arg(long)]
    service: Vec<String>,
    /// <flag>=<value> for this worktree only, for example 136_skill_sync=true.
    /// The value is JSON when it parses (true, 3), otherwise a string. Can repeat.
    #[arg(long)]
    flag: Vec<String>,
    /// Removes a worktree flag set with --flag. Can repeat.
    #[arg(long)]
    no_flag: Vec<String>,
}

#[derive(Subcommand)]
enum ServiceAction {
    Up {
        service: Option<String>,
    },
    Down {
        service: Option<String>,
    },
    Status {
        service: Option<String>,
        #[command(flatten)]
        json: JsonFlag,
    },
}

#[derive(Subcommand)]
enum LinearQuery {
    /// My open issues, joined with the worktrees.
    Issues,
    /// My projects. With --search, all matching projects, mine first.
    Projects {
        #[arg(long)]
        search: Option<String>,
    },
    /// One project with milestones, issues, documents and the last update.
    Project { id: String },
    /// One issue with description, sub-issues, comments and links.
    Issue { key: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let manager = Manager::new(Settings::load(), Arc::new(RealSystem));
    let command_line = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let result = run(&manager, cli.command);
    if cli.notify {
        let text = match &result {
            Ok(()) => format!("Done: {command_line}"),
            Err(error) => format!("Failed: {error}"),
        };
        notify(&text);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn notify(text: &str) {
    let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"n8n-worktree-manager\"",
        escape(text)
    );
    match RealSystem.run(&Command::new("osascript").args(["-e", &script])) {
        Ok(output) if output.success => {}
        Ok(output) => eprintln!("warning: no notification: {}", output.stderr.trim()),
        Err(error) => eprintln!("warning: no notification: {error:#}"),
    }
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn on_off(value: &str) -> Result<bool> {
    match value {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        other => bail!("Expected on or off, got '{other}'"),
    }
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn service_names(service: Option<String>) -> Vec<String> {
    match service {
        Some(service) => vec![service],
        None => SERVICES
            .iter()
            .map(|service| service.name.to_string())
            .collect(),
    }
}

fn run(manager: &Manager, command: Commands) -> Result<()> {
    let settings = manager.settings();
    match command {
        Commands::List(flag) => {
            let worktrees = manager.list()?;
            if flag.json {
                let items: Vec<_> = worktrees
                    .iter()
                    .map(|meta| render::worktree_json(settings, meta))
                    .collect();
                return print_json(
                    &json!({ "schema_version": SCHEMA_VERSION, "worktrees": items }),
                );
            }
            if worktrees.is_empty() {
                println!("No worktrees yet. Create one with: n8n-worktree-manager create <name>");
                return Ok(());
            }
            println!(
                "{:<24} {:<13} {:<20} {:<24} {:<24} POSTGRES",
                "NAME", "ISSUE", "SETUP", "EDITOR", "BACKEND"
            );
            for meta in &worktrees {
                let setup = meta
                    .setup
                    .as_ref()
                    .map(|setup| setup.summary())
                    .unwrap_or_else(|| "done".to_string());
                println!(
                    "{:<24} {:<13} {:<20} {:<24} {:<24} localhost:{}",
                    meta.name,
                    meta.issue.as_deref().unwrap_or("-"),
                    setup,
                    format!("http://localhost:{}", meta.ports.editor),
                    format!("http://localhost:{}", meta.ports.n8n),
                    meta.ports.postgres
                );
            }
            println!(
                "\nLogin: {} / {} (the same dev account on every worktree)",
                settings.owner_email, settings.owner_password
            );
            Ok(())
        }
        Commands::Show { name, json } => {
            let meta = manager.show(&name)?;
            if json.json {
                return print_json(&render::worktree_json(settings, &meta));
            }
            print!("{}", render::context(settings, &meta, None));
            Ok(())
        }
        Commands::Context { dir } => {
            let dir = match dir {
                Some(dir) => dir,
                None => std::env::current_dir()?,
            };
            print!("{}", manager.context(&dir)?);
            Ok(())
        }
        Commands::Create(args) => {
            let mut options = CreateOptions::new(&args.name);
            options.branch = args.branch;
            options.base = args.base;
            options.issue = args.issue.map(|key| key.to_uppercase());
            options.queue = args.queue;
            options.sandbox = !args.no_sandbox;
            options.telemetry = !args.no_telemetry;
            options.build = !args.no_build;
            let meta = manager.create(options)?;
            eprintln!(
                "Created '{}' in slot {} on branch {}",
                meta.name, meta.slot, meta.branch
            );
            if args.open {
                open(manager, &meta.name)?;
            }
            let meta = if args.no_provision {
                meta
            } else {
                provision(manager, &meta.name)?
            };
            if args.json.json {
                return print_json(&render::worktree_json(settings, &meta));
            }
            print!("{}", render::context(settings, &meta, None));
            Ok(())
        }
        Commands::Provision { name } => {
            let meta = provision(manager, &name)?;
            print!("{}", render::context(settings, &meta, None));
            Ok(())
        }
        Commands::Set(args) => {
            let mut options = SetOptions::default();
            if let Some(issue) = args.issue {
                options.issue = Some(Some(issue));
            } else if args.no_issue {
                options.issue = Some(None);
            }
            if let Some(queue) = args.queue {
                options.queue = Some(on_off(&queue)?);
            }
            for service in args.service {
                let (name, value) = service
                    .split_once('=')
                    .context("--service needs <service>=on|off")?;
                options.services.push((name.to_string(), on_off(value)?));
            }
            for flag in args.flag {
                let (key, value) = flag
                    .split_once('=')
                    .context("--flag needs <flag>=<value>")?;
                let value = serde_json::from_str(value)
                    .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
                options.flags.push((key.to_string(), Some(value)));
            }
            for key in args.no_flag {
                options.flags.push((key, None));
            }
            let meta = manager.set(&args.name, options)?;
            eprintln!(
                "Updated '{}'. Restart dev:be to load a new env.sh.",
                meta.name
            );
            Ok(())
        }
        Commands::Sync { name, all } => {
            let names = match (name, all) {
                (Some(name), false) => vec![name],
                (None, true) => manager.list()?.into_iter().map(|meta| meta.name).collect(),
                _ => bail!("Give a name or --all"),
            };
            for name in names {
                if manager.checkout_dir(&name).is_dir() {
                    manager.sync(&name)?;
                    eprintln!("Synced '{name}'");
                } else {
                    eprintln!("Skipped '{name}': no checkout");
                }
            }
            Ok(())
        }
        Commands::Prune {
            merged: _,
            delete_branch,
            yes,
            json,
        } => {
            let merged = manager.merged()?;
            if json.json {
                return print_json(
                    &json!({ "schema_version": SCHEMA_VERSION, "merged": merged }),
                );
            }
            if merged.is_empty() {
                println!("No worktree has a merged PR.");
                return Ok(());
            }
            for worktree in &merged {
                let note = match &worktree.skip {
                    Some(reason) => format!("skip: {reason}"),
                    None => "tear down".to_string(),
                };
                println!(
                    "{:<24} #{:<6} {:<40} {note}",
                    worktree.name, worktree.pr, worktree.branch
                );
            }
            let safe: Vec<_> = merged.iter().filter(|worktree| worktree.skip.is_none()).collect();
            if safe.is_empty() {
                println!("Nothing to tear down.");
                return Ok(());
            }
            if !yes
                && !confirm(&format!(
                    "Tear down {} worktree(s)? This deletes their databases and folders.",
                    safe.len()
                ))?
            {
                bail!("Aborted. Pass --yes to skip the question.");
            }
            let options = TeardownOptions {
                force: false,
                delete_branch,
            };
            let mut failed = 0;
            for worktree in safe {
                match manager.teardown(&worktree.name, options) {
                    Ok(()) => eprintln!("Tore down '{}'", worktree.name),
                    Err(error) => {
                        failed += 1;
                        eprintln!("error: '{}': {error:#}", worktree.name);
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} teardown(s) failed");
            }
            Ok(())
        }
        Commands::Open { name } => open(manager, &name),
        Commands::Teardown {
            name,
            force,
            delete_branch,
            yes,
        } => {
            let meta = manager.show(&name)?;
            if !yes
                && !confirm(&format!(
                    "Tear down '{}'? This deletes its database and {}.",
                    meta.name,
                    manager.worktree_dir(&meta.name).display()
                ))?
            {
                bail!("Aborted. Pass --yes to skip the question.");
            }
            let options = TeardownOptions {
                force,
                delete_branch,
            };
            match manager.teardown(&meta.name, options) {
                Err(error)
                    if !force
                        && error.to_string().contains("uncommitted changes")
                        && confirm(&format!("{error} Lose them?"))? =>
                {
                    manager.teardown(
                        &meta.name,
                        TeardownOptions {
                            force: true,
                            delete_branch,
                        },
                    )?;
                }
                result => result?,
            }
            eprintln!("Tore down '{}'", meta.name);
            Ok(())
        }
        Commands::Services { action } => match action {
            ServiceAction::Up { service } => {
                for name in service_names(service) {
                    eprintln!("Starting {name}");
                    manager.service_up(&name)?;
                }
                Ok(())
            }
            ServiceAction::Down { service } => {
                for name in service_names(service) {
                    manager.service_down(&name)?;
                    eprintln!("Stopped {name}");
                }
                Ok(())
            }
            ServiceAction::Status { service, json } => {
                let statuses = service_names(service)
                    .iter()
                    .map(|name| manager.service_status(name))
                    .collect::<Result<Vec<_>>>()?;
                if json.json {
                    return print_json(
                        &json!({ "schema_version": SCHEMA_VERSION, "services": statuses }),
                    );
                }
                for status in statuses {
                    let state = match (status.running, status.ready) {
                        (true, true) => "ready",
                        (true, false) => "running, not ready",
                        _ => "stopped",
                    };
                    println!("{:<10} localhost:{:<6} {state}", status.name, status.port);
                }
                Ok(())
            }
        },
        Commands::Linear { query } => {
            let linear = manager.linear();
            let value = match query {
                LinearQuery::Issues => json!({
                    "schema_version": SCHEMA_VERSION,
                    "issues": manager.issues(&linear)?,
                }),
                LinearQuery::Projects { search } => json!({
                    "schema_version": SCHEMA_VERSION,
                    "projects": linear.projects(search.as_deref())?,
                }),
                LinearQuery::Project { id } => linear.project(&id)?,
                LinearQuery::Issue { key } => linear.issue(&key)?,
            };
            print_json(&value)
        }
        Commands::Start {
            key,
            no_provision,
            open: open_after,
        } => {
            let meta = manager.start(&manager.linear(), &key)?;
            eprintln!(
                "{} is In Progress. Worktree '{}' on branch {}",
                key.to_uppercase(),
                meta.name,
                meta.branch
            );
            if open_after {
                open(manager, &meta.name)?;
            }
            if !no_provision {
                provision(manager, &meta.name)?;
            }
            print!("{}", manager.context(&manager.worktree_dir(&meta.name))?);
            Ok(())
        }
        Commands::Telemetry { action, name } => match action.as_str() {
            "up" | "down" | "status" => legacy_service(manager, "telemetry", &action),
            "tail" => telemetry_tail(manager, name),
            other => bail!("Unknown telemetry command '{other}'. Use: up, down, status, tail"),
        },
        Commands::Sandbox { action } => legacy_service(manager, "sandbox", &action),
    }
}

fn legacy_service(manager: &Manager, name: &str, action: &str) -> Result<()> {
    let action = match action {
        "up" => ServiceAction::Up {
            service: Some(name.to_string()),
        },
        "down" => ServiceAction::Down {
            service: Some(name.to_string()),
        },
        "status" => ServiceAction::Status {
            service: Some(name.to_string()),
            json: JsonFlag { json: false },
        },
        other => bail!("Unknown {name} command '{other}'. Use: up, down, status"),
    };
    run(manager, Commands::Services { action })
}

fn provision(manager: &Manager, name: &str) -> Result<n8n_worktree_manager::model::Meta> {
    eprintln!(
        "Setting up '{name}': files, compose up, postgres, pnpm install, pnpm build. This takes several minutes."
    );
    let meta = manager.provision(name)?;
    eprintln!("'{name}' is set up");
    Ok(meta)
}

#[allow(
    clippy::disallowed_methods,
    reason = "blocking on purpose: the CLI is synchronous, and Zed8 calls the library on a background thread"
)]
fn open(manager: &Manager, name: &str) -> Result<()> {
    let checkout = manager.checkout_dir(&manager.show(name)?.name);
    let zed_cli = &manager.settings().zed_cli;
    // No -e: it would replace the project of the front window, and an agent
    // in a terminal of that window would stop.
    let status = std::process::Command::new(zed_cli)
        .arg(&checkout)
        .status()
        .with_context(|| format!("starting {zed_cli}"))?;
    if !status.success() {
        bail!("{zed_cli} could not open {}", checkout.display());
    }
    Ok(())
}

/// Opens the telemetry TUI on the running capture. With a name, only the
/// events of that worktree's n8n, by its instance id.
#[allow(
    clippy::disallowed_methods,
    reason = "blocking on purpose: the CLI is synchronous, and Zed8 calls the library on a background thread"
)]
fn telemetry_tail(manager: &Manager, name: Option<String>) -> Result<()> {
    let settings = manager.settings();
    let local_cli = settings.telemetry_inspector_dir.join("dist/cli.js");
    let mut command = if local_cli.is_file() {
        let mut command = std::process::Command::new("node");
        command.arg(&local_cli);
        command
    } else {
        let mut command = std::process::Command::new("npx");
        command.args(["-y", "github:rmakowiak/telemetry-inspector"]);
        command
    };
    command.args([
        "--port",
        &n8n_worktree_manager::model::TELEMETRY_PORT.to_string(),
    ]);
    if let Some(name) = name {
        let meta = manager.show(&name)?;
        let compose = &manager.settings().compose_command;
        let (program, compose_args) = compose
            .split_first()
            .context("no compose command in the settings")?;
        let output = std::process::Command::new(program)
            .args(compose_args)
            .args([
                "exec",
                "-T",
                "postgres",
                "psql",
                "-U",
                "postgres",
                "-d",
                "n8n",
                "-tAc",
                "select value from deployment_key where type = 'instance.id' and status = 'active'",
            ])
            .current_dir(manager.worktree_dir(&meta.name))
            .output()?;
        let from_db = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let instance = if from_db.is_empty() {
            meta.extra
                .get("instance_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .context("The instance id is not known yet. Start dev:be once, then try again.")?
        } else {
            from_db
        };
        command.args(["--instance", &instance]);
    }
    let status = command.status().context("starting telemetry-inspector")?;
    if !status.success() {
        bail!("telemetry-inspector exited with {status}");
    }
    Ok(())
}
