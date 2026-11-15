use std::collections::BTreeMap;
use std::path::PathBuf;

/// The values of `secrets.env`. The file is a shell file with `KEY=value`
/// lines. Values can be in single or double quotes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Secrets(pub(crate) BTreeMap<String, String>);

impl Secrets {
    pub fn parse(text: &str) -> Self {
        let mut values = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            values.insert(key.trim().to_string(), unquote(value.trim()).to_string());
        }
        Self(values)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

/// Zed8 when it is installed, otherwise official Zed.
fn default_zed_cli(home: &str) -> String {
    let zed8 = format!("{home}/Applications/Zed8.app/Contents/MacOS/cli");
    if std::path::Path::new(&zed8).is_file() {
        zed8
    } else {
        "/Applications/Zed.app/Contents/MacOS/cli".to_string()
    }
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Everything the tool needs to know besides one worktree: the state folder
/// and the values that go into every env.sh. The defaults are the values of
/// the Bash version. `secrets.env` overrides any of them by its key.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub wt_home: PathBuf,
    pub default_source_repo: PathBuf,
    pub owner_email: String,
    pub owner_first_name: String,
    pub owner_last_name: String,
    pub owner_password: String,
    pub owner_password_hash: String,
    pub license_tenant_id: String,
    pub license_activation_key: String,
    pub ai_enabled_modules: String,
    pub ai_instance_model: String,
    pub anthropic_api_key: String,
    pub brave_search_api_key: String,
    pub feature_flag_overrides: String,
    pub sandbox_api_key: String,
    pub sandbox_runner_api_key: String,
    pub sandbox_registration_token: String,
    pub sandbox_api_image: String,
    pub sandbox_runner_image: String,
    pub sandbox_sandbox_image: String,
    pub telemetry_inspector_dir: PathBuf,
    pub linear_api_key: String,
    /// The Zed command line tool that `open` calls.
    pub zed_cli: String,
    /// The workspace part of Linear URLs: linear.app/<workspace>/issue/KEY.
    pub linear_workspace: String,
    /// Folder with the `node` and `pnpm` binaries for provision. Empty means
    /// the folder of nvm's default alias, or the PATH.
    pub node_bin_dir: String,
}

impl Settings {
    pub fn new(wt_home: PathBuf, secrets: Secrets) -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let value = |key: &str, default: &str| secrets.get(key).unwrap_or(default).to_string();
        Self {
            default_source_repo: PathBuf::from(value(
                "N8N_WT_SOURCE_REPO",
                &format!("{home}/workspace/n8n"),
            )),
            owner_email: value("DEV_OWNER_EMAIL", "n8n.local@rmakowiak.com"),
            owner_first_name: value("DEV_OWNER_FIRST_NAME", "Dev"),
            owner_last_name: value("DEV_OWNER_LAST_NAME", "Local"),
            owner_password: value("DEV_OWNER_PASSWORD", ""),
            owner_password_hash: value("DEV_OWNER_PASSWORD_HASH", ""),
            license_tenant_id: value("LICENSE_TENANT_ID", "1001"),
            license_activation_key: value("LICENSE_ACTIVATION_KEY", ""),
            ai_enabled_modules: value("AI_ENABLED_MODULES", "agents"),
            ai_instance_model: value("AI_INSTANCE_MODEL", "anthropic/claude-opus-4-8"),
            anthropic_api_key: value("ANTHROPIC_API_KEY", ""),
            brave_search_api_key: value("BRAVE_SEARCH_API_KEY", ""),
            feature_flag_overrides: value(
                "FEATURE_FLAG_OVERRIDES",
                r#"{"111_context_preferences":"variant"}"#,
            ),
            sandbox_api_key: value("SANDBOX_API_KEY", "n8n-sandbox-ci-key"),
            sandbox_runner_api_key: value("SANDBOX_RUNNER_API_KEY", "ci-runner-key"),
            sandbox_registration_token: value("SANDBOX_REGISTRATION_TOKEN", "ci-reg-token"),
            sandbox_api_image: value("SANDBOX_API_IMAGE", "n8nio/n8n-sandbox-service-api:latest"),
            sandbox_runner_image: value(
                "SANDBOX_RUNNER_IMAGE",
                "n8nio/n8n-sandbox-service-runner-dind:latest",
            ),
            sandbox_sandbox_image: value(
                "SANDBOX_SANDBOX_IMAGE",
                "n8nio/n8n-sandbox-service-sandbox:latest",
            ),
            telemetry_inspector_dir: PathBuf::from(value(
                "N8N_WT_TELEMETRY_INSPECTOR",
                &format!("{home}/workspace/telemetry-inspector"),
            )),
            linear_api_key: value("LINEAR_API_KEY", ""),
            zed_cli: value("N8N_WT_ZED_CLI", &default_zed_cli(&home)),
            linear_workspace: value("LINEAR_WORKSPACE", "n8n"),
            node_bin_dir: value("NODE_BIN_DIR", ""),
            wt_home,
        }
    }

    /// The settings of this machine: the state folder from N8N_WT_HOME
    /// (default ~/workspace/n8n-worktrees), then `secrets.env` in it. An
    /// environment variable with the same key wins over both.
    pub fn load() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        let wt_home = std::env::var("N8N_WT_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(home).join("workspace").join("n8n-worktrees"));
        let text = std::fs::read_to_string(wt_home.join("secrets.env")).unwrap_or_default();
        let mut secrets = Secrets::parse(&text);
        for key in [
            "N8N_WT_SOURCE_REPO",
            "N8N_WT_ZED_CLI",
            "N8N_WT_TELEMETRY_INSPECTOR",
            "LINEAR_API_KEY",
            "NODE_BIN_DIR",
        ] {
            if let Ok(value) = std::env::var(key) {
                secrets.0.insert(key.to_string(), value);
            }
        }
        Self::new(wt_home, secrets)
    }

    /// The folder that holds one folder per worktree.
    pub fn envs_dir(&self) -> PathBuf {
        self.wt_home.join("n8n")
    }

    pub fn worktree_dir(&self, name: &str) -> PathBuf {
        self.envs_dir().join(name)
    }

    pub fn turbo_cache_dir(&self) -> PathBuf {
        self.wt_home.join("shared").join("turbo-cache")
    }

    pub fn shared_dir(&self, service: &str) -> PathBuf {
        self.wt_home.join("shared").join(service)
    }
}
