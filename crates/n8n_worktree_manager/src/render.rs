use crate::linear::{IssueSummary, issue_key_from_branch};
use crate::model::Meta;
use crate::services::service;
use crate::settings::Settings;

/// One `export KEY=value` line of env.sh. `quoted` puts the value in single
/// quotes, for values with characters that the shell would change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
    pub quoted: bool,
}

impl EnvVar {
    pub fn plain(key: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.to_string(),
            value: value.into(),
            quoted: false,
        }
    }

    pub fn quoted(key: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.to_string(),
            value: value.into(),
            quoted: true,
        }
    }
}

pub fn env_vars(settings: &Settings, meta: &Meta) -> Vec<EnvVar> {
    let ports = &meta.ports;
    let dir = settings.worktree_dir(&meta.name);
    let mut vars = vec![
        EnvVar::plain("DB_TYPE", "postgresdb"),
        EnvVar::plain("DB_POSTGRESDB_HOST", "localhost"),
        EnvVar::plain("DB_POSTGRESDB_PORT", ports.postgres.to_string()),
        EnvVar::plain("DB_POSTGRESDB_DATABASE", "n8n"),
        EnvVar::plain("DB_POSTGRESDB_USER", "postgres"),
        EnvVar::plain("DB_POSTGRESDB_PASSWORD", "password"),
        EnvVar::plain("NODEJS_PREFER_IPV4", "true"),
        EnvVar::plain("N8N_PORT", ports.n8n.to_string()),
        EnvVar::plain("N8N_EDITOR_PORT", ports.editor.to_string()),
        EnvVar::plain("N8N_SMTP_HOST", "localhost"),
        EnvVar::plain("N8N_SMTP_PORT", ports.mailpit_smtp.to_string()),
        // The task runner broker binds this port in every mode, so two
        // worktrees on the n8n default port collide.
        EnvVar::plain("N8N_RUNNERS_BROKER_PORT", ports.broker.to_string()),
    ];
    if meta.queue {
        vars.push(EnvVar::plain("EXECUTIONS_MODE", "queue"));
        vars.push(EnvVar::plain("QUEUE_BULL_REDIS_HOST", "localhost"));
        vars.push(EnvVar::plain(
            "QUEUE_BULL_REDIS_PORT",
            ports.redis.to_string(),
        ));
    }
    vars.extend([
        EnvVar::plain(
            "N8N_USER_FOLDER",
            dir.join("n8n-home").to_string_lossy().into_owned(),
        ),
        EnvVar::plain("N8N_WT_LABEL", &meta.name),
        EnvVar::plain("N8N_INSTANCE_OWNER_MANAGED_BY_ENV", "true"),
        EnvVar::plain("N8N_INSTANCE_OWNER_EMAIL", &settings.owner_email),
        EnvVar::plain("N8N_INSTANCE_OWNER_FIRST_NAME", &settings.owner_first_name),
        EnvVar::plain("N8N_INSTANCE_OWNER_LAST_NAME", &settings.owner_last_name),
        EnvVar::quoted(
            "N8N_INSTANCE_OWNER_PASSWORD_HASH",
            &settings.owner_password_hash,
        ),
        EnvVar::plain("N8N_LICENSE_TENANT_ID", &settings.license_tenant_id),
        EnvVar::plain(
            "N8N_LICENSE_ACTIVATION_KEY",
            &settings.license_activation_key,
        ),
        EnvVar::plain("N8N_ENABLED_MODULES", &settings.ai_enabled_modules),
        EnvVar::plain("N8N_INSTANCE_AI_MODEL", &settings.ai_instance_model),
        EnvVar::plain("N8N_INSTANCE_AI_RUN_DEBUG_ENABLED", "true"),
    ]);
    // An n8n-only name, so the key does not reach other tools in the shell.
    // Claude Code reads ANTHROPIC_API_KEY.
    if !settings.anthropic_api_key.is_empty() {
        vars.push(EnvVar::plain(
            "N8N_INSTANCE_AI_MODEL_API_KEY",
            &settings.anthropic_api_key,
        ));
    }
    vars.push(EnvVar::plain(
        "INSTANCE_AI_BRAVE_SEARCH_API_KEY",
        &settings.brave_search_api_key,
    ));
    let flag_overrides = flag_overrides(settings, meta);
    if !flag_overrides.is_empty() {
        vars.push(EnvVar::quoted("N8N_FEATURE_FLAG_OVERRIDES", flag_overrides));
    }
    vars.push(EnvVar::plain(
        "TURBO_CACHE_DIR",
        settings.turbo_cache_dir().to_string_lossy().into_owned(),
    ));
    for (name, enabled) in [("sandbox", meta.sandbox), ("telemetry", meta.telemetry)] {
        if let Some(service) = service(name).filter(|_| enabled) {
            vars.extend((service.env)(settings));
        }
    }
    vars
}

fn flag_overrides(settings: &Settings, meta: &Meta) -> String {
    if meta.flags.is_empty() {
        // The text from secrets.env as written, so the output equals the Bash version.
        return settings.feature_flag_overrides.clone();
    }
    // A global value that is not a JSON object cannot be merged, so the
    // worktree flags replace it.
    let mut merged = serde_json::from_str::<serde_json::Value>(&settings.feature_flag_overrides)
        .ok()
        .and_then(|value| match value {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();
    for (key, value) in &meta.flags {
        merged.insert(key.clone(), value.clone());
    }
    serde_json::Value::Object(merged).to_string()
}

pub fn env_sh(settings: &Settings, meta: &Meta) -> String {
    let mut text = String::from(
        "# source this before running pnpm dev:be / pnpm dev:fe:editor in the checkout (n8n/)\n",
    );
    for var in env_vars(settings, meta) {
        if var.quoted {
            text.push_str(&format!("export {}='{}'\n", var.key, var.value));
        } else {
            text.push_str(&format!("export {}={}\n", var.key, var.value));
        }
    }
    text
}

fn postgres_url(meta: &Meta) -> String {
    format!(
        "postgres://postgres:password@localhost:{}/n8n?sslmode=disable",
        meta.ports.postgres
    )
}

/// `.zed/tasks.json`: the dev servers, the tests, lazysql and lazydocker.
/// The tasks source env.sh themselves, so they work while terminal.env is
/// stale.
pub fn zed_tasks(settings: &Settings, meta: &Meta) -> String {
    let dir = settings.worktree_dir(&meta.name);
    let source = format!("source \"{}\" &&", dir.join("env.sh").display());
    let task = |label: &str, command: String| {
        serde_json::json!({
            "label": format!("n8n-wt: {label} ({})", meta.name),
            "command": command,
            "use_new_terminal": false,
            "allow_concurrent_runs": false,
            "reveal": "always",
        })
    };
    let pnpm = |label: &str, script: &str| task(label, format!("{source} pnpm {script}"));
    let mut lazydocker = task("lazydocker", "lazydocker".to_string());
    lazydocker["cwd"] = serde_json::Value::String(dir.to_string_lossy().into_owned());
    let tasks = serde_json::Value::Array(vec![
        pnpm("dev:be", "dev:be"),
        pnpm("dev:fe:editor", "dev:fe:editor"),
        pnpm("test:backend:unit", "test:ci:backend:unit"),
        pnpm("test:backend:integration", "test:ci:backend:integration"),
        pnpm("test:frontend", "test:ci:frontend"),
        pnpm("build", "build"),
        task("lazysql", format!("lazysql \"{}\"", postgres_url(meta))),
        lazydocker,
    ]);
    pretty_json(&tasks)
}

/// `.zed/settings.json`: the env.sh values as terminal.env, so every Zed
/// terminal in the worktree has the ports and the database of the worktree.
pub fn zed_settings(settings: &Settings, meta: &Meta) -> String {
    let env: serde_json::Map<String, serde_json::Value> = env_vars(settings, meta)
        .into_iter()
        .map(|var| (var.key, serde_json::Value::String(var.value)))
        .collect();
    pretty_json(&serde_json::json!({ "terminal": { "env": env } }))
}

/// The `.env` next to docker-compose.yml.
pub fn compose_env(meta: &Meta) -> String {
    let ports = &meta.ports;
    format!(
        "COMPOSE_PROJECT_NAME={}\nPOSTGRES_PORT={}\nREDIS_PORT={}\nMAILPIT_SMTP_PORT={}\nMAILPIT_HTTP_PORT={}\nPGWEB_PORT={}\n",
        compose_project(&meta.name),
        ports.postgres,
        ports.redis,
        ports.mailpit_smtp,
        ports.mailpit_http,
        ports.pgweb,
    )
}

pub fn compose_project(name: &str) -> String {
    format!("n8n-wt-{name}")
}

pub fn pretty_json(value: &serde_json::Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

/// Wires the badge plugin into the editor-ui vite config: one import after
/// the dev-ports import, one plugin call after `devServerPlugin`. None when
/// an anchor is missing, because half a patch breaks `vite build`.
pub fn patch_vite_config(text: &str) -> Option<String> {
    let import_anchor = text.find("from './vite/dev-ports.mjs';\n")?;
    let import_end = import_anchor + "from './vite/dev-ports.mjs';\n".len();
    let plugin_anchor = "devServerPlugin(process.env),\n";
    let plugin_end = text.find(plugin_anchor)? + plugin_anchor.len();
    if plugin_end < import_end {
        return None;
    }
    let mut patched = String::with_capacity(text.len() + 128);
    patched.push_str(&text[..import_end]);
    patched
        .push_str("import { worktreeBadgePlugin } from './vite/vite-plugin-worktree-badge.mjs';\n");
    patched.push_str(&text[import_end..plugin_end]);
    patched.push_str("\tworktreeBadgePlugin(process.env),\n");
    patched.push_str(&text[plugin_end..]);
    Some(patched)
}

/// What a Claude session in the worktree must know: the issue, the ports,
/// the URLs and the login. The SessionStart hook prints it. Local files only.
pub fn context(settings: &Settings, meta: &Meta, issue: Option<&IssueSummary>) -> String {
    let mut text = String::new();
    let mut line = |label: &str, value: &str| text.push_str(&format!("{label:<9} {value}\n"));
    let key_from_branch = issue_key_from_branch(&meta.branch);
    let key = meta.issue.clone().or_else(|| key_from_branch.clone());
    let mut header = String::new();
    if let Some(key) = &key {
        let source = if meta.issue.is_some() {
            ""
        } else {
            " (key from the branch name)"
        };
        header = format!("This worktree works on Linear issue {key}{source}.\n");
    }
    let mut issue_lines = String::new();
    if let Some(key) = &key {
        let mut issue_line =
            |label: &str, value: &str| issue_lines.push_str(&format!("{label:<9} {value}\n"));
        let url = format!(
            "https://linear.app/{}/issue/{key}",
            settings.linear_workspace
        );
        match issue.filter(|issue| &issue.identifier == key) {
            Some(issue) => {
                issue_line("Title", &issue.title);
                issue_line("Status", &issue.status);
                if let Some(project) = &issue.project {
                    issue_line("Project", project);
                }
                issue_line("URL", &issue.url);
                issue_lines.push_str("Read the full issue and its comments with the Linear MCP tools get_issue and list_comments.\n");
            }
            None => issue_line("URL", &url),
        }
        issue_lines.push('\n');
    }
    let ports = &meta.ports;
    line(
        "Worktree",
        &format!("{} (slot {}), branch {}", meta.name, meta.slot, meta.branch),
    );
    line(
        "Backend",
        &format!("http://localhost:{} (pnpm dev:be)", ports.n8n),
    );
    line(
        "Editor",
        &format!("http://localhost:{} (pnpm dev:fe:editor)", ports.editor),
    );
    line(
        "Postgres",
        &format!(
            "localhost:{} (database n8n, user postgres, password password)",
            ports.postgres
        ),
    );
    line(
        "Mailpit",
        &format!("http://localhost:{}", ports.mailpit_http),
    );
    line("pgweb", &format!("http://localhost:{}", ports.pgweb));
    if meta.queue {
        line("Redis", &format!("localhost:{} (queue mode)", ports.redis));
    }
    line(
        "Login",
        &format!("{} / {}", settings.owner_email, settings.owner_password),
    );
    if meta.sandbox {
        line(
            "Sandbox",
            &format!(
                "http://localhost:{} (shared by all worktrees)",
                ports.sandbox
            ),
        );
    }
    if meta.telemetry {
        line(
            "Telemetry",
            &format!(
                "http://localhost:{} (shared by all worktrees)",
                ports.telemetry
            ),
        );
    }
    line(
        "Env",
        "source ../env.sh before you run pnpm in the checkout. Zed terminals and tasks have it already.",
    );
    format!("{header}{issue_lines}{text}")
}

/// Version of the `--json` shapes that Workbench and other scripts read.
/// Adding a field keeps it. Renaming or removing one raises it.
pub const SCHEMA_VERSION: u32 = 1;

/// One worktree in the `--json` output of `list` and `show`.
pub fn worktree_json(settings: &Settings, meta: &Meta) -> serde_json::Value {
    let dir = settings.worktree_dir(&meta.name);
    let ports = &meta.ports;
    let setup = match &meta.setup {
        Some(setup) => serde_json::json!({
            "status": setup.status,
            "step": setup.step,
            "summary": setup.summary(),
            "error": setup.error,
        }),
        None => serde_json::json!({
            "status": "done",
            "step": null,
            "summary": "done",
            "error": null,
        }),
    };
    serde_json::json!({
        "name": meta.name,
        "slot": meta.slot,
        "branch": meta.branch,
        "base": meta.base,
        "issue": meta.issue,
        "queue": meta.queue,
        "sandbox": meta.sandbox,
        "telemetry": meta.telemetry,
        "created_at": meta.created_at,
        "dir": dir.to_string_lossy(),
        "checkout": dir.join("n8n").to_string_lossy(),
        "setup": setup,
        "ports": ports,
        "urls": {
            "backend": format!("http://localhost:{}", ports.n8n),
            "editor": format!("http://localhost:{}", ports.editor),
            "mailpit": format!("http://localhost:{}", ports.mailpit_http),
            "pgweb": format!("http://localhost:{}", ports.pgweb),
        },
    })
}
