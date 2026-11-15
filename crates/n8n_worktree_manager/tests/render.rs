use n8n_worktree_manager::linear::IssueSummary;
use n8n_worktree_manager::model::{Meta, Ports};
use n8n_worktree_manager::render;
use n8n_worktree_manager::settings::{Secrets, Settings};
use pretty_assertions::assert_eq;

#[test]
fn slot_zero_gives_the_base_ports_and_slot_three_adds_three() {
    let expected_slot_zero = Ports {
        postgres: 25432,
        redis: 26379,
        mailpit_smtp: 21025,
        mailpit_http: 28025,
        pgweb: 27432,
        n8n: 25678,
        editor: 28080,
        broker: 29679,
        sandbox: 19080,
        telemetry: 19081,
    };
    assert_eq!(Ports::for_slot(0), expected_slot_zero);

    let expected_slot_three = Ports {
        postgres: 25435,
        redis: 26382,
        mailpit_smtp: 21028,
        mailpit_http: 28028,
        pgweb: 27435,
        n8n: 25681,
        editor: 28083,
        broker: 29682,
        sandbox: 19080,
        telemetry: 19081,
    };
    assert_eq!(Ports::for_slot(3), expected_slot_three);
}

fn golden(path: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/golden/{path}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("golden file exists")
}

fn fixture(path: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{path}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture exists")
}

fn settings_with_fake_secrets() -> Settings {
    Settings::new("/wt".into(), Secrets::parse(&fixture("secrets.env")))
}

fn golden_meta(name: &str) -> Meta {
    serde_json::from_str(&golden(&format!("{name}/meta.json"))).expect("golden meta parses")
}

#[test]
fn env_sh_with_sandbox_and_telemetry_equals_the_bash_output() {
    let env_sh = render::env_sh(&settings_with_fake_secrets(), &golden_meta("alpha"));
    assert_eq!(env_sh, golden("alpha/env.sh"));
}

#[test]
fn env_sh_without_secrets_and_services_equals_the_bash_output() {
    let settings = Settings::new("/wt".into(), Secrets::default());
    let env_sh = render::env_sh(&settings, &golden_meta("beta"));
    assert!(!env_sh.contains("SANDBOX"));
    assert!(!env_sh.contains("DIAGNOSTICS"));
    assert_eq!(env_sh, golden("beta/env.sh"));
}

#[test]
fn queue_mode_adds_the_queue_and_redis_lines() {
    let env_sh = render::env_sh(&settings_with_fake_secrets(), &golden_meta("gamma"));
    assert!(env_sh.contains("export EXECUTIONS_MODE=queue\n"));
    assert!(env_sh.contains("export QUEUE_BULL_REDIS_PORT=26386\n"));
    assert_eq!(env_sh, golden("gamma/env.sh"));
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).expect("valid JSON")
}

#[test]
fn zed_files_and_compose_env_equal_the_bash_output() {
    let settings = settings_with_fake_secrets();
    for name in ["alpha", "gamma"] {
        let meta = golden_meta(name);
        assert_eq!(
            json(&render::zed_tasks(&settings, &meta)),
            json(&golden(&format!("{name}/tasks.json"))),
            "tasks.json of {name}"
        );
        assert_eq!(
            json(&render::zed_settings(&settings, &meta)),
            json(&golden(&format!("{name}/settings.json"))),
            "settings.json of {name}"
        );
        assert_eq!(
            render::compose_env(&meta),
            golden(&format!("{name}/compose.env")),
            "compose .env of {name}"
        );
    }
}

#[test]
fn context_prints_the_issue_the_ports_and_the_urls() {
    let mut settings = settings_with_fake_secrets();
    settings.owner_password = "fake-password".to_string();
    let mut meta = golden_meta("alpha");
    meta.branch = "rmakowiak/context-424-skill-sync-dashboard".to_string();
    meta.issue = Some("CONTEXT-424".to_string());
    let issue = IssueSummary {
        identifier: "CONTEXT-424".to_string(),
        title: "Skill Sync dashboard: add the P3 events".to_string(),
        status: "In Progress".to_string(),
        project: Some("Skill Sync".to_string()),
        url: "https://linear.app/n8n/issue/CONTEXT-424".to_string(),
        branch_name: None,
        assignee: None,
        priority: 0,
    };

    let text = render::context(&settings, &meta, Some(&issue));

    assert_eq!(
        text,
        "\
This worktree works on Linear issue CONTEXT-424.
Title     Skill Sync dashboard: add the P3 events
Status    In Progress
Project   Skill Sync
URL       https://linear.app/n8n/issue/CONTEXT-424
Read the full issue and its comments with the Linear MCP tools get_issue and list_comments.

Worktree  alpha (slot 3), branch rmakowiak/context-424-skill-sync-dashboard
Backend   http://localhost:25681 (pnpm dev:be)
Editor    http://localhost:28083 (pnpm dev:fe:editor)
Postgres  localhost:25435 (database n8n, user postgres, password password)
Mailpit   http://localhost:28028
pgweb     http://localhost:27435
Login     n8n.local@rmakowiak.com / fake-password
Sandbox   http://localhost:19080 (shared by all worktrees)
Telemetry http://localhost:19081 (shared by all worktrees)
Env       source ../env.sh before you run pnpm in the checkout. Zed terminals and tasks have it already.
"
    );
}

#[test]
fn context_without_an_issue_field_takes_the_key_from_the_branch_name() {
    let mut meta = golden_meta("beta");
    meta.branch = "context-488-mcp-publish".to_string();
    let settings = Settings::new("/wt".into(), Secrets::default());

    let text = render::context(&settings, &meta, None);

    assert!(
        text.starts_with(
            "This worktree works on Linear issue CONTEXT-488 (key from the branch name).\n\
             URL       https://linear.app/n8n/issue/CONTEXT-488\n"
        ),
        "{text}"
    );
    assert!(!text.contains("Sandbox"));

    meta.branch = "cat-bot/run-43026071447-1".to_string();
    let text = render::context(&settings, &meta, None);
    assert!(text.starts_with("Worktree  beta (slot 0)"), "{text}");
}
