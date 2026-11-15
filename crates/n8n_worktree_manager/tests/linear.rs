use n8n_worktree_manager::linear::{FakeTransport, IssueSummary, Linear, join};
use n8n_worktree_manager::model::{Meta, Setup};
use pretty_assertions::assert_eq;

const MY_ISSUES_ANSWER: &str = r#"{
  "data": {
    "viewer": {
      "assignedIssues": {
        "nodes": [
          {
            "identifier": "CONTEXT-424",
            "title": "Skill Sync dashboard: add the P3 events",
            "url": "https://linear.app/n8n/issue/CONTEXT-424/skill-sync-dashboard",
            "branchName": "rmakowiak/context-424-skill-sync-dashboard",
            "priority": 2,
            "state": { "name": "In Progress", "type": "started" },
            "project": { "name": "Skill Sync" },
            "assignee": { "name": "Roman" }
          },
          {
            "identifier": "CONTEXT-406",
            "title": "Skills module: remove unused barrel export",
            "url": "https://linear.app/n8n/issue/CONTEXT-406",
            "branchName": "rmakowiak/context-406-skills-module",
            "priority": 0,
            "state": { "name": "Ready for pick up", "type": "unstarted" },
            "project": null,
            "assignee": null
          }
        ]
      }
    }
  }
}"#;

#[test]
fn my_issues_parses_the_answer_and_asks_only_for_open_issues() {
    let transport = FakeTransport::with_answers([MY_ISSUES_ANSWER]);
    let linear = Linear::new(Box::new(transport.clone()));

    let issues = linear.my_issues().expect("issues");

    assert_eq!(
        issues,
        [
            IssueSummary {
                identifier: "CONTEXT-424".to_string(),
                title: "Skill Sync dashboard: add the P3 events".to_string(),
                status: "In Progress".to_string(),
                project: Some("Skill Sync".to_string()),
                url: "https://linear.app/n8n/issue/CONTEXT-424/skill-sync-dashboard".to_string(),
                branch_name: Some("rmakowiak/context-424-skill-sync-dashboard".to_string()),
                assignee: Some("Roman".to_string()),
                priority: 2,
            },
            IssueSummary {
                identifier: "CONTEXT-406".to_string(),
                title: "Skills module: remove unused barrel export".to_string(),
                status: "Ready for pick up".to_string(),
                project: None,
                url: "https://linear.app/n8n/issue/CONTEXT-406".to_string(),
                branch_name: Some("rmakowiak/context-406-skills-module".to_string()),
                assignee: None,
                priority: 0,
            },
        ]
    );
    let request: serde_json::Value =
        serde_json::from_str(&transport.requests()[0]).expect("request is JSON");
    let query = request["query"].as_str().expect("query");
    assert!(query.contains("assignedIssues"), "{query}");
    assert!(
        query.contains(r#"nin: ["completed", "canceled"]"#),
        "{query}"
    );
}

#[test]
fn a_graphql_error_becomes_an_error() {
    let transport = FakeTransport::with_answers([
        r#"{"errors":[{"message":"Authentication required, not authenticated"}]}"#,
    ]);
    let linear = Linear::new(Box::new(transport));

    let error = linear.my_issues().expect_err("error answer");

    assert!(
        error.to_string().contains("Authentication required"),
        "{error}"
    );
}

fn issue(identifier: &str, branch: &str) -> IssueSummary {
    IssueSummary {
        identifier: identifier.to_string(),
        title: format!("Title of {identifier}"),
        status: "In Progress".to_string(),
        project: None,
        url: String::new(),
        branch_name: Some(branch.to_string()),
        assignee: None,
        priority: 0,
    }
}

#[test]
fn join_links_issues_to_worktrees_by_issue_field_then_branch() {
    let mut by_field = Meta::new("work-a", 0, "some-branch", "master", "/src/n8n");
    by_field.issue = Some("CONTEXT-1".to_string());
    let by_branch_key = Meta::new("work-b", 1, "context-2-other-title", "master", "/src/n8n");
    let mut by_linear_branch = Meta::new("work-c", 2, "rmakowiak/ctx-3-x", "master", "/src/n8n");
    by_linear_branch.setup = Some(Setup::pending(true));
    let issues = vec![
        issue("CONTEXT-1", "rmakowiak/context-1-a"),
        issue("CONTEXT-2", "rmakowiak/context-2-b"),
        issue("OTHER-3", "rmakowiak/ctx-3-x"),
        issue("CONTEXT-4", "rmakowiak/context-4-d"),
    ];

    let rows = join(issues, &[by_field, by_branch_key, by_linear_branch]);

    let links: Vec<(&str, Option<&str>, Option<&str>)> = rows
        .iter()
        .map(|row| {
            (
                row.issue.identifier.as_str(),
                row.worktree.as_deref(),
                row.setup.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        links,
        [
            ("CONTEXT-1", Some("work-a"), Some("done")),
            ("CONTEXT-2", Some("work-b"), Some("done")),
            ("OTHER-3", Some("work-c"), Some("pending")),
            ("CONTEXT-4", None, None),
        ]
    );
}

fn issue_for_start(state_type: &str) -> String {
    format!(
        r#"{{"data":{{"issue":{{
            "id":"uuid-406","identifier":"CONTEXT-406","title":"Skills module",
            "branchName":"rmakowiak/context-406-skills-module",
            "state":{{"id":"s-ready","name":"Ready for pick up","type":"{state_type}"}},
            "team":{{"states":{{"nodes":[
                {{"id":"s-review","name":"Testing & Review","type":"started","position":3}},
                {{"id":"s-ready","name":"Ready for pick up","type":"unstarted","position":1}},
                {{"id":"s-progress","name":"In Progress","type":"started","position":2}}
            ]}}}}
        }}}}}}"#
    )
}

#[test]
fn start_moves_the_issue_to_in_progress() {
    let transport = FakeTransport::with_answers([
        issue_for_start("unstarted").as_str(),
        r#"{"data":{"issueUpdate":{"success":true}}}"#,
    ]);
    let linear = Linear::new(Box::new(transport.clone()));

    let started = linear.start_issue("context-406").expect("start");

    assert_eq!(started.identifier, "CONTEXT-406");
    assert_eq!(started.branch_name, "rmakowiak/context-406-skills-module");
    assert!(started.moved);
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    let update: serde_json::Value = serde_json::from_str(&requests[1]).expect("JSON");
    assert!(
        update["query"]
            .as_str()
            .expect("query")
            .contains("issueUpdate")
    );
    assert_eq!(update["variables"]["id"], "uuid-406");
    assert_eq!(update["variables"]["stateId"], "s-progress");
}

#[test]
fn start_does_not_write_when_the_issue_is_started_already() {
    let transport = FakeTransport::with_answers([issue_for_start("started").as_str()]);
    let linear = Linear::new(Box::new(transport.clone()));

    let started = linear.start_issue("CONTEXT-406").expect("start");

    assert!(!started.moved);
    assert_eq!(transport.requests().len(), 1);
}

#[test]
fn projects_with_a_search_put_mine_first_without_duplicates() {
    let transport = FakeTransport::with_answers([r#"{"data":{
        "mine":{"nodes":[{"id":"p-skill","name":"Skill Sync","url":"u1","state":"started","progress":0.4}]},
        "all":{"nodes":[
            {"id":"p-other","name":"Skill Store","url":"u2","state":"planned","progress":0},
            {"id":"p-skill","name":"Skill Sync","url":"u1","state":"started","progress":0.4}
        ]}
    }}"#]);
    let linear = Linear::new(Box::new(transport.clone()));

    let projects = linear.projects(Some("skill")).expect("projects");

    let names: Vec<(&str, bool)> = projects
        .iter()
        .map(|project| (project.name.as_str(), project.mine))
        .collect();
    assert_eq!(names, [("Skill Sync", true), ("Skill Store", false)]);
    let request: serde_json::Value = serde_json::from_str(&transport.requests()[0]).expect("JSON");
    assert_eq!(request["variables"]["search"], "skill");
}

#[test]
fn the_issue_preview_returns_the_issue_and_fails_for_an_unknown_key() {
    let transport = FakeTransport::with_answers([
        r#"{"data":{"issue":{"identifier":"CONTEXT-424","title":"T","comments":{"nodes":[]}}}}"#,
        r#"{"data":{"issue":null}}"#,
    ]);
    let linear = Linear::new(Box::new(transport.clone()));

    let preview = linear.issue("CONTEXT-424").expect("preview");
    assert_eq!(preview["identifier"], "CONTEXT-424");
    let query = transport.requests()[0].clone();
    assert!(query.contains("comments"), "{query}");

    let error = linear.issue("CONTEXT-999").expect_err("unknown key");
    assert!(error.to_string().contains("CONTEXT-999"), "{error}");
}
