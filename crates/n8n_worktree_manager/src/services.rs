use crate::model::{SANDBOX_PORT, TELEMETRY_PORT};
use crate::render::EnvVar;
use crate::settings::Settings;
use crate::system::Command;

/// A service that every worktree can share: one instance on a fixed port.
pub struct Service {
    pub name: &'static str,
    pub port: u16,
    /// The env.sh lines that point a worktree at the service.
    pub env: fn(&Settings) -> Vec<EnvVar>,
}

pub const SERVICES: &[Service] = &[
    Service {
        name: "sandbox",
        port: SANDBOX_PORT,
        env: sandbox_env,
    },
    Service {
        name: "telemetry",
        port: TELEMETRY_PORT,
        env: telemetry_env,
    },
];

pub fn service(name: &str) -> Option<&'static Service> {
    SERVICES.iter().find(|service| service.name == name)
}

fn sandbox_env(settings: &Settings) -> Vec<EnvVar> {
    vec![
        EnvVar::plain("N8N_INSTANCE_AI_SANDBOX_ENABLED", "true"),
        EnvVar::plain("N8N_INSTANCE_AI_SANDBOX_PROVIDER", "n8n-sandbox"),
        EnvVar::plain(
            "N8N_SANDBOX_SERVICE_URL",
            format!("http://localhost:{SANDBOX_PORT}"),
        ),
        EnvVar::plain("N8N_SANDBOX_SERVICE_API_KEY", &settings.sandbox_api_key),
    ]
}

fn telemetry_env(_settings: &Settings) -> Vec<EnvVar> {
    let url = format!("http://localhost:{TELEMETRY_PORT}");
    vec![
        EnvVar::plain("N8N_DIAGNOSTICS_ENABLED", "true"),
        EnvVar::quoted("N8N_DIAGNOSTICS_CONFIG_BACKEND", format!("stub;{url}")),
        EnvVar::quoted("N8N_DIAGNOSTICS_CONFIG_FRONTEND", format!("stub;{url}")),
        EnvVar::plain("N8N_DIAGNOSTICS_POSTHOG_API_KEY", "stub"),
        EnvVar::plain("N8N_DIAGNOSTICS_POSTHOG_API_HOST", url),
    ]
}

const SANDBOX_COMPOSE: &str = include_str!("../templates/sandbox-compose.yml");
const SANDBOX_PROJECT: &str = "n8n-wt-shared-sandbox";
const TELEMETRY_REPO: &str = "github:rmakowiak/telemetry-inspector";

/// The state of one shared service.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub port: u16,
    pub running: bool,
    pub ready: bool,
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

impl crate::Manager {
    pub fn service_up(&self, name: &str) -> anyhow::Result<()> {
        match name {
            "sandbox" => self.sandbox_up(),
            "telemetry" => self.telemetry_up(),
            other => anyhow::bail!("Unknown service '{other}'. Known: sandbox, telemetry."),
        }
    }

    pub fn service_down(&self, name: &str) -> anyhow::Result<()> {
        match name {
            "sandbox" => {
                let dir = self.settings.shared_dir("sandbox");
                if dir.join("docker-compose.yml").is_file() {
                    self.run(&Command::new("docker").args(["compose", "stop"]).cwd(dir))?;
                }
                Ok(())
            }
            "telemetry" => {
                let pid_file = self.settings.shared_dir("telemetry").join("telemetry.pid");
                if let Some(pid) = self.live_pid(&pid_file)? {
                    self.run(&Command::new("kill").arg(pid))?;
                }
                std::fs::remove_file(&pid_file).ok();
                Ok(())
            }
            other => anyhow::bail!("Unknown service '{other}'. Known: sandbox, telemetry."),
        }
    }

    pub fn service_status(&self, name: &str) -> anyhow::Result<ServiceStatus> {
        let service = service(name).ok_or_else(|| {
            anyhow::anyhow!("Unknown service '{name}'. Known: sandbox, telemetry.")
        })?;
        let (running, ready) = match name {
            "sandbox" => {
                let running = self.sandbox_running()?;
                (running, running && self.sandbox_ready()?)
            }
            _ => {
                let pid_file = self.settings.shared_dir("telemetry").join("telemetry.pid");
                let running = self.live_pid(&pid_file)?.is_some();
                (running, running && self.telemetry_healthy()?)
            }
        };
        Ok(ServiceStatus {
            name: service.name.to_string(),
            port: service.port,
            running,
            ready,
        })
    }

    /// The pid in the file, if that process still lives. A stale file after
    /// a crash or a reboot does not count.
    fn live_pid(&self, pid_file: &std::path::Path) -> anyhow::Result<Option<String>> {
        let Ok(pid) = std::fs::read_to_string(pid_file) else {
            return Ok(None);
        };
        let pid = pid.trim().to_string();
        if pid.is_empty() {
            return Ok(None);
        }
        let alive = self
            .system
            .run(&Command::new("kill").args(["-0", &pid]))?
            .success;
        Ok(alive.then_some(pid))
    }

    fn telemetry_healthy(&self) -> anyhow::Result<bool> {
        let url = format!("http://localhost:{TELEMETRY_PORT}/health");
        Ok(self
            .system
            .run(&Command::new("curl").args(["-fsS", &url]))?
            .success)
    }

    fn telemetry_up(&self) -> anyhow::Result<()> {
        let dir = self.settings.shared_dir("telemetry");
        std::fs::create_dir_all(&dir)?;
        let pid_file = dir.join("telemetry.pid");
        if self.live_pid(&pid_file)?.is_some() {
            return Ok(());
        }
        let local_cli = self.settings.telemetry_inspector_dir.join("dist/cli.js");
        let program = if local_cli.is_file() {
            format!("node {}", shell_quote(&local_cli.to_string_lossy()))
        } else {
            format!("npx -y {TELEMETRY_REPO}")
        };
        // --headless is the capture. The TUI attaches later and can quit
        // without stopping the capture.
        let script = format!(
            "nohup {program} --headless --port {TELEMETRY_PORT} --log {} > {} 2>&1 & echo $!",
            shell_quote(&dir.join("events.jsonl").to_string_lossy()),
            shell_quote(&dir.join("telemetry.log").to_string_lossy()),
        );
        let mut command = Command::new("sh").args(["-c", &script]);
        if let Some(path) = self.node_path_for_services() {
            command = command.env("PATH", path);
        }
        let pid = self.run(&command)?;
        crate::store::write_atomic(&pid_file, &format!("{}\n", pid.trim()))?;
        for _ in 0..100 {
            if self.telemetry_healthy()? {
                return Ok(());
            }
            self.system.sleep(std::time::Duration::from_millis(200));
        }
        anyhow::bail!(
            "telemetry-inspector did not come up. See {}",
            dir.join("telemetry.log").display()
        )
    }

    fn sandbox_running(&self) -> anyhow::Result<bool> {
        let output = self.system.run(&Command::new("docker").args([
            "ps",
            "--filter",
            &format!("name={SANDBOX_PROJECT}-sandbox-api"),
            "--filter",
            "status=running",
            "--format",
            "{{.Names}}",
        ]))?;
        Ok(output.success && !output.stdout.trim().is_empty())
    }

    /// Ready means a sandbox can be created and deleted. Only a real round
    /// trip shows that the runner is registered and the image is loaded.
    fn sandbox_ready(&self) -> anyhow::Result<bool> {
        let url = format!("http://localhost:{SANDBOX_PORT}/sandboxes");
        let header = format!("X-Api-Key: {}", self.settings.sandbox_api_key);
        let created = self
            .system
            .run(&Command::new("curl").args(["-sf", "-X", "POST", "-H", &header, &url]))?;
        if !created.success {
            return Ok(false);
        }
        let id = serde_json::from_str::<serde_json::Value>(&created.stdout)
            .ok()
            .and_then(|value| value["id"].as_str().map(str::to_string));
        if let Some(id) = id {
            let delete_url = format!("{url}/{id}");
            self.system.run(&Command::new("curl").args([
                "-sf",
                "-X",
                "DELETE",
                "-H",
                &header,
                &delete_url,
            ]))?;
        }
        Ok(true)
    }

    fn sandbox_up(&self) -> anyhow::Result<()> {
        let settings = &self.settings;
        let dir = settings.shared_dir("sandbox");
        let tls = settings.shared_dir("sandbox-tls");
        crate::store::write_atomic(&dir.join("docker-compose.yml"), SANDBOX_COMPOSE)?;
        let env = format!(
            "COMPOSE_PROJECT_NAME={SANDBOX_PROJECT}\nSHARED_SANDBOX_PORT={SANDBOX_PORT}\nSANDBOX_TLS_DIR={}\nSANDBOX_API_IMAGE={}\nSANDBOX_RUNNER_IMAGE={}\nSANDBOX_SANDBOX_IMAGE={}\nSANDBOX_API_KEY={}\nSANDBOX_RUNNER_API_KEY={}\nSANDBOX_REGISTRATION_TOKEN={}\n",
            tls.display(),
            settings.sandbox_api_image,
            settings.sandbox_runner_image,
            settings.sandbox_sandbox_image,
            settings.sandbox_api_key,
            settings.sandbox_runner_api_key,
            settings.sandbox_registration_token,
        );
        crate::store::write_atomic(&dir.join(".env"), &env)?;
        // The mTLS certs belong to fixed host names, so they are made once.
        if !tls.join("ca.crt").is_file() {
            std::fs::create_dir_all(&tls)?;
            self.run(&Command::new("docker").args([
                "run",
                "--rm",
                "--user",
                "0:0",
                "--entrypoint",
                "sh",
                "-e",
                "NUM_RUNNERS=1",
                "-v",
                &format!("{}:/tls", tls.display()),
                &settings.sandbox_api_image,
                "-c",
                "bootstrap-mtls.sh --out-dir /tls --api-san sandbox-api --control-san-prefix sandbox-runner --world-readable && chown -R sandbox-api:sandbox-api /tls/api && chmod -R a+rX /tls",
            ]))?;
        }
        self.run(
            &Command::new("docker")
                .args(["compose", "up", "-d"])
                .cwd(&dir),
        )?;
        if self.sandbox_ready()? {
            return Ok(());
        }
        let runner = self.run(
            &Command::new("docker")
                .args(["compose", "ps", "-q", "sandbox-runner-1"])
                .cwd(&dir),
        )?;
        let runner = runner.trim().to_string();
        if runner.is_empty() {
            anyhow::bail!(
                "No sandbox-runner-1 container. See: docker compose -p {SANDBOX_PROJECT} logs"
            );
        }
        let inner_docker = Command::new("docker").args(["exec", &runner, "docker", "info"]);
        let mut inner_ready = false;
        for _ in 0..30 {
            if self.system.run(&inner_docker)?.success {
                inner_ready = true;
                break;
            }
            self.system.sleep(std::time::Duration::from_secs(2));
        }
        if !inner_ready {
            anyhow::bail!(
                "The Docker daemon in the sandbox runner did not start. See: docker logs {runner}"
            );
        }
        // A preload is faster than a pull inside the runner (1.35 GB).
        self.run(&Command::new("sh").args([
            "-c",
            &format!(
                "docker save {} | docker exec -i {} docker load",
                shell_quote(&settings.sandbox_sandbox_image),
                shell_quote(&runner)
            ),
        ]))?;
        for _ in 0..90 {
            if self.sandbox_ready()? {
                return Ok(());
            }
            self.system.sleep(std::time::Duration::from_secs(2));
        }
        anyhow::bail!(
            "The sandbox API did not get ready. See: docker compose -p {SANDBOX_PROJECT} logs sandbox-api"
        )
    }
}
