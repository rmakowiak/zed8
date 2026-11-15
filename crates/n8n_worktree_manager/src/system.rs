use anyhow::{Context as _, Result};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// One process to start. `log` sends stdout and stderr to a file instead of
/// capturing them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub log: Option<PathBuf>,
}

impl Command {
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_string(),
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.push((key.to_string(), value.into()));
        self
    }

    pub fn log(mut self, path: impl Into<PathBuf>) -> Self {
        self.log = Some(path.into());
        self
    }

    /// The command as one line, for logs, errors and the fake.
    pub fn line(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(stdout: &str) -> Self {
        Self {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    pub fn failed(stderr: &str) -> Self {
        Self {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }
}

/// Everything that leaves the process: git, docker compose, pnpm, curl.
/// Only this module starts processes, so tests swap it for `FakeSystem`.
pub trait System: Send + Sync {
    fn run(&self, command: &Command) -> Result<Output>;
    fn sleep(&self, duration: Duration);
}

pub struct RealSystem;

#[allow(
    clippy::disallowed_methods,
    reason = "blocking on purpose: the CLI is synchronous, and Zed8 calls the library on a background thread"
)]
impl System for RealSystem {
    fn run(&self, command: &Command) -> Result<Output> {
        let mut process = std::process::Command::new(&command.program);
        process.args(&command.args);
        if let Some(cwd) = &command.cwd {
            process.current_dir(cwd);
        }
        for (key, value) in &command.env {
            process.env(key, value);
        }
        process.stdin(std::process::Stdio::null());
        if let Some(log) = &command.log {
            let file = File::create(log).with_context(|| format!("creating {}", log.display()))?;
            process.stdout(file.try_clone()?);
            process.stderr(file);
            let status = process
                .status()
                .with_context(|| format!("starting {}", command.line()))?;
            return Ok(Output {
                success: status.success(),
                ..Output::default()
            });
        }
        let output = process
            .output()
            .with_context(|| format!("starting {}", command.line()))?;
        Ok(Output {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Records every command and answers with the first rule whose prefix
/// matches the command line. Without a rule, a command succeeds with no
/// output.
#[derive(Default)]
pub struct FakeSystem {
    commands: Mutex<Vec<Command>>,
    rules: Mutex<Vec<(String, Output)>>,
}

impl FakeSystem {
    pub fn respond(&self, prefix: &str, output: Output) {
        if let Ok(mut rules) = self.rules.lock() {
            rules.push((prefix.to_string(), output));
        }
    }

    pub fn fail(&self, prefix: &str) {
        self.respond(prefix, Output::failed("failed in the fake"));
    }

    pub fn clear_rules(&self) {
        if let Ok(mut rules) = self.rules.lock() {
            rules.clear();
        }
    }

    pub fn clear_commands(&self) {
        if let Ok(mut commands) = self.commands.lock() {
            commands.clear();
        }
    }

    pub fn commands(&self) -> Vec<String> {
        self.recorded().iter().map(Command::line).collect()
    }

    pub fn recorded(&self) -> Vec<Command> {
        self.commands
            .lock()
            .map(|commands| commands.clone())
            .unwrap_or_default()
    }
}

impl System for FakeSystem {
    fn run(&self, command: &Command) -> Result<Output> {
        if let Ok(mut commands) = self.commands.lock() {
            commands.push(command.clone());
        }
        let line = command.line();
        let rules = self
            .rules
            .lock()
            .map_err(|_| anyhow::anyhow!("fake rules lock poisoned"))?;
        Ok(rules
            .iter()
            .find(|(prefix, _)| line.starts_with(prefix.as_str()))
            .map(|(_, output)| output.clone())
            .unwrap_or_else(|| Output::ok("")))
    }

    fn sleep(&self, _duration: Duration) {}
}
