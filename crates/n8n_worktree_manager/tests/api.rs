use n8n_worktree_manager::linear::{FakeTransport, Linear};
use n8n_worktree_manager::model::{SetupStatus, Step};
use n8n_worktree_manager::settings::{Secrets, Settings};
use n8n_worktree_manager::system::{FakeSystem, Output};
use n8n_worktree_manager::{CreateOptions, Manager, SetOptions, TeardownOptions};
use pretty_assertions::assert_eq;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

struct Setup {
    _home: tempfile::TempDir,
    wt_home: PathBuf,
    source: PathBuf,
    system: Arc<FakeSystem>,
    manager: Manager,
}

fn setup() -> Setup {
    let home = tempfile::tempdir().expect("temp dir");
    let wt_home = home.path().join("wt");
    let source = home.path().join("n8n");
    fs::create_dir_all(source.join("packages/cli")).expect("source repo");
    fs::write(source.join("pnpm-workspace.yaml"), "").expect("source repo");
    let mut settings = Settings::new(wt_home.clone(), Secrets::default());
    settings.default_source_repo = source.clone();
    let system = Arc::new(FakeSystem::default());
    let manager = Manager::new(settings, system.clone());
    Setup {
        _home: home,
        wt_home,
        source,
        system,
        manager,
    }
}

#[test]
fn create_adds_a_git_worktree_on_a_new_branch_and_writes_a_pending_meta() {
    let setup = setup();

    let meta = setup
        .manager
        .create(CreateOptions::new("Feature One"))
        .expect("create");

    let checkout = setup.wt_home.join("n8n/feature-one/n8n");
    let source = setup.source.display();
    assert_eq!(
        setup.system.commands(),
        [
            format!("git -C {source} remote get-url origin"),
            format!(
                "git -C {source} fetch --quiet origin +refs/heads/master:refs/remotes/origin/master"
            ),
            format!(
                "git -C {source} worktree add {} -b feature-one origin/master --quiet",
                checkout.display()
            ),
        ]
    );
    assert_eq!(meta.name, "feature-one");
    assert_eq!(meta.branch, "feature-one");
    assert_eq!(meta.slot, 0);
    let on_disk = setup.manager.show("feature-one").expect("show");
    let state = on_disk.setup.expect("setup state");
    assert_eq!(state.status, SetupStatus::Pending);
    assert_eq!(state.done, Vec::<Step>::new());
}

#[test]
fn create_for_an_existing_name_returns_it_and_runs_nothing() {
    let setup = setup();
    let first = setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("first create");
    setup.system.clear_commands();

    let second = setup
        .manager
        .create(CreateOptions::new("Feature One"))
        .expect("second create");

    assert_eq!(second, first);
    assert_eq!(setup.system.commands(), Vec::<String>::new());
    assert_eq!(setup.manager.list().expect("list").len(), 1);
}

#[test]
fn create_with_a_branch_uses_the_local_or_the_remote_branch() {
    let setup = setup();
    let source = setup.source.display().to_string();
    setup.system.fail(&format!(
        "git -C {source} show-ref --verify --quiet refs/heads/remote-only"
    ));
    setup.system.fail(&format!(
        "git -C {source} show-ref --verify --quiet refs/heads/new-branch"
    ));
    setup.system.fail(&format!(
        "git -C {source} show-ref --verify --quiet refs/remotes/origin/new-branch"
    ));

    for (name, branch) in [
        ("one", "local-branch"),
        ("two", "remote-only"),
        ("three", "new-branch"),
    ] {
        let mut options = CreateOptions::new(name);
        options.branch = Some(branch.to_string());
        let meta = setup.manager.create(options).expect("create");
        assert_eq!(meta.branch, branch);
    }

    let adds: Vec<String> = setup
        .system
        .commands()
        .into_iter()
        .filter(|line| line.contains("worktree add"))
        .collect();
    let checkout = |name: &str| {
        setup
            .wt_home
            .join(format!("n8n/{name}/n8n"))
            .display()
            .to_string()
    };
    assert_eq!(
        adds,
        [
            format!(
                "git -C {source} worktree add {} local-branch --quiet",
                checkout("one")
            ),
            format!(
                "git -C {source} worktree add {} -b remote-only origin/remote-only --quiet",
                checkout("two")
            ),
            format!(
                "git -C {source} worktree add {} -b new-branch origin/master --quiet",
                checkout("three")
            ),
        ]
    );
}

#[test]
fn provision_runs_the_steps_in_order_and_marks_each_done() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    setup.system.clear_commands();

    let meta = setup.manager.provision("feature-one").expect("provision");

    let dir = setup.wt_home.join("n8n/feature-one");
    let checkout = dir.join("n8n");
    assert_eq!(
        setup.system.commands(),
        [
            format!(
                "git -C {} rev-parse --path-format=absolute --git-common-dir",
                checkout.display()
            ),
            "docker compose up -d".to_string(),
            "docker compose exec -T postgres pg_isready -U postgres".to_string(),
            "pnpm install".to_string(),
            "pnpm build".to_string(),
            "pnpm --filter n8n-editor-ui build".to_string(),
        ]
    );
    let recorded = setup.system.recorded();
    assert_eq!(recorded[1].cwd.as_deref(), Some(dir.as_path()));
    assert_eq!(recorded[3].cwd.as_deref(), Some(checkout.as_path()));
    assert_eq!(
        recorded[3].log.as_deref(),
        Some(dir.join("install.log").as_path())
    );
    assert!(
        recorded[5]
            .env
            .contains(&("N8N_WT_LABEL".to_string(), "feature-one".to_string()))
    );
    for file in [
        "env.sh",
        ".env",
        "docker-compose.yml",
        "pgweb-dark.css",
        "n8n/.zed/tasks.json",
        "n8n/.zed/settings.json",
    ] {
        assert!(dir.join(file).is_file(), "{file} is missing");
    }
    assert!(dir.join("n8n-home").is_dir());
    let state = meta.setup.expect("setup state");
    assert_eq!(state.status, SetupStatus::Done);
    assert_eq!(state.done, Step::ALL.to_vec());
    assert_eq!(
        setup.manager.show("feature-one").expect("show").setup,
        Some(state)
    );
}

#[test]
fn a_failed_install_is_recorded_and_the_next_provision_resumes_there() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    setup.system.fail("pnpm install");

    let error = setup
        .manager
        .provision("feature-one")
        .expect_err("install fails");
    assert!(
        format!("{error:#}").contains("failed at install"),
        "{error:#}"
    );
    let state = setup
        .manager
        .show("feature-one")
        .expect("show")
        .setup
        .expect("setup");
    assert_eq!(state.status, SetupStatus::Failed);
    assert_eq!(state.summary(), "failed at install");
    assert_eq!(
        state.done,
        [Step::Files, Step::ComposeUp, Step::WaitPostgres]
    );

    setup.system.clear_rules();
    setup.system.clear_commands();
    setup
        .manager
        .provision("feature-one")
        .expect("second provision");

    assert_eq!(
        setup.system.commands(),
        [
            "pnpm install",
            "pnpm build",
            "pnpm --filter n8n-editor-ui build"
        ]
    );
}

#[test]
fn teardown_removes_containers_worktree_and_folder_and_frees_the_slot() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    let dir = setup.wt_home.join("n8n/feature-one");
    let checkout = dir.join("n8n");
    fs::create_dir_all(&checkout).expect("checkout");
    setup.system.clear_commands();

    setup
        .manager
        .teardown("feature-one", TeardownOptions::default())
        .expect("teardown");

    let source = setup.source.display();
    assert_eq!(
        setup.system.commands(),
        [
            format!("git -C {} status --porcelain", checkout.display()),
            "docker compose -p n8n-wt-feature-one down -v --remove-orphans".to_string(),
            format!(
                "git -C {} rev-parse --path-format=absolute --git-common-dir",
                checkout.display()
            ),
            format!(
                "git -C {source} worktree remove {} --force",
                checkout.display()
            ),
        ]
    );
    assert!(!dir.exists());
    assert!(setup.manager.list().expect("list").is_empty());
    let next = setup
        .manager
        .create(CreateOptions::new("feature-two"))
        .expect("create again");
    assert_eq!(next.slot, 0);
}

#[test]
fn teardown_refuses_uncommitted_changes_without_force() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    let checkout = setup.wt_home.join("n8n/feature-one/n8n");
    fs::create_dir_all(&checkout).expect("checkout");
    setup.system.respond(
        &format!("git -C {} status --porcelain", checkout.display()),
        Output::ok(" M packages/cli/src/index.ts\n"),
    );

    let error = setup
        .manager
        .teardown("feature-one", TeardownOptions::default())
        .expect_err("dirty checkout");
    assert!(error.to_string().contains("uncommitted changes"), "{error}");
    assert!(checkout.exists());

    let force = TeardownOptions {
        force: true,
        ..TeardownOptions::default()
    };
    setup
        .manager
        .teardown("feature-one", force)
        .expect("forced teardown");
    assert!(!checkout.exists());
}

#[test]
fn set_writes_the_issue_and_rewrites_env_sh_for_a_service_change() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    setup.manager.provision("feature-one").expect("provision");
    let env_sh = setup.wt_home.join("n8n/feature-one/env.sh");
    assert!(
        fs::read_to_string(&env_sh)
            .expect("env.sh")
            .contains("N8N_SANDBOX_SERVICE_URL")
    );

    let options = SetOptions {
        issue: Some(Some("CONTEXT-424".to_string())),
        services: vec![("sandbox".to_string(), false)],
        ..SetOptions::default()
    };
    let meta = setup.manager.set("feature-one", options).expect("set");

    assert_eq!(meta.issue.as_deref(), Some("CONTEXT-424"));
    assert!(!meta.sandbox);
    let on_disk = setup.manager.show("feature-one").expect("show");
    assert_eq!(on_disk.issue.as_deref(), Some("CONTEXT-424"));
    assert!(
        !fs::read_to_string(&env_sh)
            .expect("env.sh")
            .contains("SANDBOX")
    );
}

#[test]
fn provision_wires_in_the_editor_badge_like_the_bash_version() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    let checkout = setup.wt_home.join("n8n/feature-one/n8n");
    let editor_ui = checkout.join("packages/frontend/editor-ui");
    fs::create_dir_all(editor_ui.join("vite")).expect("editor-ui");
    let manifest = env!("CARGO_MANIFEST_DIR");
    fs::copy(
        format!("{manifest}/tests/fixtures/vite.config.mts"),
        editor_ui.join("vite.config.mts"),
    )
    .expect("fixture");

    setup.manager.provision("feature-one").expect("provision");

    let patched = fs::read_to_string(editor_ui.join("vite.config.mts")).expect("patched");
    let golden = fs::read_to_string(format!("{manifest}/tests/golden/badge/vite.config.mts"))
        .expect("golden");
    assert_eq!(patched, golden);
    assert!(
        editor_ui
            .join("vite/vite-plugin-worktree-badge.mts")
            .is_file()
    );
    assert!(setup.system.commands().contains(&format!(
        "git -C {} update-index --skip-worktree packages/frontend/editor-ui/vite.config.mts",
        checkout.display()
    )));

    setup.system.clear_commands();
    setup.manager.sync("feature-one").expect("sync");
    let synced = fs::read_to_string(editor_ui.join("vite.config.mts")).expect("synced");
    assert_eq!(synced, golden, "a second run must not patch twice");
}

#[test]
fn context_finds_the_worktree_from_a_folder_inside_it_and_reads_the_cache() {
    let setup = setup();
    let mut options = CreateOptions::new("context-424");
    options.issue = Some("CONTEXT-424".to_string());
    setup.manager.create(options).expect("create");
    let inside = setup.wt_home.join("n8n/context-424/n8n/packages/cli");
    fs::create_dir_all(&inside).expect("folder");
    fs::create_dir_all(setup.wt_home.join("cache")).expect("cache folder");
    fs::write(
        setup.wt_home.join("cache/linear-issues.json"),
        r#"[{"identifier":"CONTEXT-424","title":"Skill Sync dashboard","status":"In Progress",
            "project":null,"url":"https://linear.app/n8n/issue/CONTEXT-424","branch_name":null,
            "assignee":null,"priority":2}]"#,
    )
    .expect("cache");

    let text = setup.manager.context(&inside).expect("context");

    assert!(text.contains("Title     Skill Sync dashboard\n"), "{text}");
    assert!(text.contains("Worktree  context-424 (slot 0)"), "{text}");

    let outside = setup
        .manager
        .context(&setup.source)
        .expect("context outside");
    assert_eq!(outside, "");
}

#[test]
fn start_moves_the_issue_and_creates_a_worktree_on_the_linear_branch() {
    let setup = setup();
    let transport = FakeTransport::with_answers([
        r#"{"data":{"issue":{"id":"uuid-406","identifier":"CONTEXT-406","title":"Skills module",
            "branchName":"rmakowiak/context-406-skills-module",
            "state":{"id":"s-ready","name":"Ready for pick up","type":"unstarted"},
            "team":{"states":{"nodes":[{"id":"s-progress","name":"In Progress","type":"started","position":2}]}}}}}"#,
        r#"{"data":{"issueUpdate":{"success":true}}}"#,
    ]);
    let linear = Linear::new(Box::new(transport.clone()));
    // A new Linear branch exists neither locally nor on origin.
    setup
        .system
        .fail(&format!("git -C {} show-ref", setup.source.display()));

    let meta = setup.manager.start(&linear, "CONTEXT-406").expect("start");

    assert_eq!(meta.name, "context-406");
    assert_eq!(meta.branch, "rmakowiak/context-406-skills-module");
    assert_eq!(meta.issue.as_deref(), Some("CONTEXT-406"));
    assert_eq!(transport.requests().len(), 2);
    assert!(
        setup
            .system
            .commands()
            .iter()
            .any(|line| line.contains("worktree add")
                && line.contains("-b rmakowiak/context-406-skills-module"))
    );
}

#[test]
fn services_up_telemetry_starts_the_capture_and_saves_its_pid() {
    let setup = setup();
    // No pid file yet, so the capture is not running.
    setup.system.respond("sh -c nohup", Output::ok("4242\n"));

    setup.manager.service_up("telemetry").expect("up");

    let telemetry = setup.wt_home.join("shared/telemetry");
    let commands = setup.system.commands();
    let start = commands
        .iter()
        .find(|line| line.starts_with("sh -c nohup"))
        .expect("start command");
    assert!(start.contains("--headless --port 19081"), "{start}");
    assert!(
        start.contains(&telemetry.join("events.jsonl").display().to_string()),
        "{start}"
    );
    assert_eq!(
        commands.last().map(String::as_str),
        Some("curl -fsS http://localhost:19081/health")
    );
    assert_eq!(
        fs::read_to_string(telemetry.join("telemetry.pid")).expect("pid file"),
        "4242\n"
    );
}

#[test]
fn set_flag_is_kept_in_meta_and_survives_sync() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    setup.manager.provision("feature-one").expect("provision");
    let env_sh = setup.wt_home.join("n8n/feature-one/env.sh");

    let options = SetOptions {
        flags: vec![("136_skill_sync".to_string(), Some(serde_json::json!(true)))],
        ..SetOptions::default()
    };
    setup.manager.set("feature-one", options).expect("set");
    setup.manager.sync("feature-one").expect("sync");

    let on_disk = setup.manager.show("feature-one").expect("show");
    assert_eq!(
        on_disk.flags.get("136_skill_sync"),
        Some(&serde_json::json!(true))
    );
    assert!(
        fs::read_to_string(&env_sh)
            .expect("env.sh")
            .contains("\"136_skill_sync\":true")
    );

    let options = SetOptions {
        flags: vec![("136_skill_sync".to_string(), None)],
        ..SetOptions::default()
    };
    let meta = setup.manager.set("feature-one", options).expect("set");
    assert!(meta.flags.is_empty());
    assert!(
        !fs::read_to_string(&env_sh)
            .expect("env.sh")
            .contains("136_skill_sync")
    );
}

#[test]
fn create_with_a_branch_returns_the_worktree_that_has_it_checked_out() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    let checkout = setup.wt_home.join("n8n/feature-one/n8n");
    // The branch was switched after create, so meta.json still says feature-one.
    setup.system.respond(
        &format!("git -C {} branch --show-current", checkout.display()),
        Output::ok("context-424-skills\n"),
    );
    setup.system.clear_commands();

    let mut options = CreateOptions::new("context-424");
    options.branch = Some("context-424-skills".to_string());
    let meta = setup.manager.create(options).expect("create");

    assert_eq!(meta.name, "feature-one");
    assert!(
        !setup
            .system
            .commands()
            .iter()
            .any(|line| line.contains("worktree add") || line.contains("fetch"))
    );
    assert!(!setup.wt_home.join("n8n/context-424").exists());
}

#[test]
fn create_fetches_over_https_because_a_raycast_shell_has_no_ssh_key() {
    let setup = setup();
    let source = setup.source.display().to_string();
    setup.system.respond(
        &format!("git -C {source} remote get-url origin"),
        Output::ok("git@github.com:n8n-io/n8n.git\n"),
    );
    setup.system.fail(&format!(
        "git -C {source} show-ref --verify --quiet refs/heads/new-branch"
    ));
    setup.system.fail(&format!(
        "git -C {source} show-ref --verify --quiet refs/remotes/origin/new-branch"
    ));

    let mut options = CreateOptions::new("new-branch");
    options.branch = Some("new-branch".to_string());
    setup.manager.create(options).expect("create");

    let fetches: Vec<String> = setup
        .system
        .commands()
        .into_iter()
        .filter(|line| line.contains(" fetch "))
        .collect();
    assert_eq!(
        fetches,
        [
            format!(
                "git -C {source} fetch --quiet https://github.com/n8n-io/n8n.git +refs/heads/new-branch:refs/remotes/origin/new-branch"
            ),
            format!(
                "git -C {source} fetch --quiet https://github.com/n8n-io/n8n.git +refs/heads/master:refs/remotes/origin/master"
            ),
        ]
    );
}

#[test]
fn merged_lists_worktrees_with_a_merged_pr_and_skips_unsafe_ones() {
    let setup = setup();
    for name in ["clean", "dirty", "ahead", "open-pr", "orphan"] {
        setup
            .manager
            .create(CreateOptions::new(name))
            .expect("create");
    }
    let checkout = |name: &str| setup.wt_home.join(format!("n8n/{name}/n8n")).display().to_string();
    // The orphan lost its checkout: its folder holds only generated files.
    for name in ["clean", "dirty", "ahead", "open-pr"] {
        fs::create_dir_all(checkout(name)).expect("checkout");
    }
    let merged_pr = |number: u32, head: &str| {
        Output::ok(&format!(
            r#"[{{"number":{number},"headRefOid":"{head}","url":"https://github.com/n8n-io/n8n/pull/{number}"}}]"#
        ))
    };
    for (branch, number, head) in [
        ("clean", 1, "aaa"),
        ("dirty", 2, "bbb"),
        ("ahead", 3, "ccc"),
        ("orphan", 4, "ddd"),
    ] {
        setup.system.respond(
            &format!("gh pr list --repo n8n-io/n8n --head {branch} --state merged"),
            merged_pr(number, head),
        );
    }
    setup.system.respond(
        &format!("git -C {} status --porcelain", checkout("dirty")),
        Output::ok(" M packages/cli/src/index.ts\n?? dev-be.log\n"),
    );
    setup.system.fail(&format!(
        "git -C {} merge-base --is-ancestor HEAD ccc",
        checkout("ahead")
    ));

    let merged = setup.manager.merged().expect("merged");

    let summary: Vec<(String, u32, Option<String>)> = merged
        .into_iter()
        .map(|worktree| (worktree.name, worktree.pr, worktree.skip))
        .collect();
    assert_eq!(
        summary,
        [
            ("ahead".to_string(), 3, Some("commits that are not in the merged PR".to_string())),
            ("clean".to_string(), 1, None),
            (
                "dirty".to_string(),
                2,
                Some("uncommitted changes: packages/cli/src/index.ts, dev-be.log".to_string())
            ),
            ("orphan".to_string(), 4, None),
        ]
    );
}

#[test]
fn teardown_without_a_checkout_skips_git_and_removes_the_folder() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("orphan"))
        .expect("create");
    let dir = setup.wt_home.join("n8n/orphan");
    setup.system.clear_commands();

    setup
        .manager
        .teardown("orphan", TeardownOptions::default())
        .expect("teardown");

    assert_eq!(
        setup.system.commands(),
        ["docker compose -p n8n-wt-orphan down -v --remove-orphans"]
    );
    assert!(!dir.exists());
}

#[test]
fn teardown_finds_the_source_repo_from_the_checkout_not_from_a_stale_meta() {
    let setup = setup();
    setup
        .manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    let checkout = setup.wt_home.join("n8n/feature-one/n8n");
    fs::create_dir_all(&checkout).expect("checkout");
    setup.system.respond(
        &format!(
            "git -C {} rev-parse --path-format=absolute --git-common-dir",
            checkout.display()
        ),
        Output::ok("/real/n8n/.git\n"),
    );
    setup.system.clear_commands();

    setup
        .manager
        .teardown("feature-one", TeardownOptions::default())
        .expect("teardown");

    assert!(setup.system.commands().contains(&format!(
        "git -C /real/n8n worktree remove {} --force",
        checkout.display()
    )));
}

#[test]
fn a_mac_with_only_the_standalone_docker_compose_uses_it() {
    let home = tempfile::tempdir().expect("temp dir");
    let mut settings = Settings::new(home.path().join("wt"), Secrets::default());
    settings.default_source_repo = home.path().join("n8n");
    fs::create_dir_all(home.path().join("n8n/packages/cli")).expect("source repo");
    fs::write(home.path().join("n8n/pnpm-workspace.yaml"), "").expect("source repo");
    settings.compose_command = vec!["docker-compose".to_string()];
    let system = Arc::new(FakeSystem::default());
    let manager = Manager::new(settings, system.clone());
    manager
        .create(CreateOptions::new("feature-one"))
        .expect("create");
    manager.provision("feature-one").expect("provision");

    manager
        .teardown("feature-one", TeardownOptions::default())
        .expect("teardown");

    let compose: Vec<String> = system
        .commands()
        .into_iter()
        .filter(|line| line.contains("compose") && !line.starts_with("git"))
        .collect();
    assert!(!compose.is_empty());
    assert!(
        compose.iter().all(|line| line.starts_with("docker-compose ")),
        "{compose:#?}"
    );
}
