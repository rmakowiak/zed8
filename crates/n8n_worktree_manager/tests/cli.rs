use pretty_assertions::assert_eq;
use std::fs;
use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_n8n-worktree-manager");

const META: &str = r#"{
  "name": "context-424",
  "slot": 2,
  "branch": "rmakowiak/context-424-skill-sync",
  "base": "master",
  "queue": false,
  "sandbox": true,
  "telemetry": false,
  "source_repo": "/src/n8n",
  "created_at": "2026-11-15T10:00:00Z",
  "issue": "CONTEXT-424",
  "setup": { "status": "failed", "done": ["files", "compose_up", "wait_postgres"], "step": "install", "error": "pnpm install failed" },
  "ports": {
    "postgres": 25434, "redis": 26381, "mailpit_smtp": 21027, "mailpit_http": 28027,
    "pgweb": 27434, "n8n": 25680, "editor": 28082, "broker": 29681,
    "sandbox": 19080, "telemetry": 19081
  }
}"#;

fn home_with_one_worktree() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("temp dir");
    let dir = home.path().join("n8n/context-424");
    fs::create_dir_all(&dir).expect("folder");
    fs::write(dir.join("meta.json"), META).expect("meta");
    home
}

#[allow(clippy::disallowed_methods, reason = "the test runs the binary as a process")]
fn run(home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    Command::new(BINARY)
        .args(args)
        .env("N8N_WT_HOME", home.path())
        .output()
        .expect("binary runs")
}

#[test]
fn list_json_prints_the_agreed_shape() {
    let home = home_with_one_worktree();

    let output = run(&home, &["list", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON on stdout");
    let dir = home.path().join("n8n/context-424");
    let expected = serde_json::json!({
        "schema_version": 1,
        "worktrees": [{
            "name": "context-424",
            "slot": 2,
            "branch": "rmakowiak/context-424-skill-sync",
            "base": "master",
            "issue": "CONTEXT-424",
            "queue": false,
            "sandbox": true,
            "telemetry": false,
            "created_at": "2026-11-15T10:00:00Z",
            "dir": dir.display().to_string(),
            "checkout": dir.join("n8n").display().to_string(),
            "setup": {
                "status": "failed",
                "step": "install",
                "summary": "failed at install",
                "error": "pnpm install failed"
            },
            "ports": {
                "postgres": 25434, "redis": 26381, "mailpit_smtp": 21027, "mailpit_http": 28027,
                "pgweb": 27434, "n8n": 25680, "editor": 28082, "broker": 29681,
                "sandbox": 19080, "telemetry": 19081
            },
            "urls": {
                "backend": "http://localhost:25680",
                "editor": "http://localhost:28082",
                "mailpit": "http://localhost:28027",
                "pgweb": "http://localhost:27434"
            }
        }]
    });
    assert_eq!(actual, expected);
}

#[test]
fn an_unknown_name_exits_1_with_the_error_on_stderr() {
    let home = home_with_one_worktree();

    let output = run(&home, &["show", "nope", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No worktree named 'nope'"), "{stderr}");
}
