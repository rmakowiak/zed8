use n8n_worktree_manager::model::Meta;
use n8n_worktree_manager::store::Store;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;

const OLD_META: &str = r#"{
  "name": "zulu",
  "slot": 3,
  "branch": "cat-bot/run-1",
  "base": "",
  "queue": false,
  "sandbox": true,
  "signoz": true,
  "source_repo": "/src/n8n",
  "created_at": "2026-11-11T14:02:10Z",
  "ports": {
    "postgres": 25435, "redis": 26382, "mailpit_smtp": 21028, "mailpit_http": 28028,
    "pgweb": 27435, "n8n": 25681, "editor": 28083, "broker": 29682,
    "sandbox": 19080, "telemetry": 19081, "signoz": 8080
  }
}"#;

const NEW_META: &str = r#"{
  "name": "alpha",
  "slot": 0,
  "branch": "context-424-skill-sync",
  "base": "master",
  "queue": false,
  "sandbox": false,
  "telemetry": true,
  "source_repo": "/src/n8n",
  "created_at": "2026-11-15T10:00:00Z",
  "issue": "CONTEXT-424",
  "ports": {
    "postgres": 25432, "redis": 26379, "mailpit_smtp": 21025, "mailpit_http": 28025,
    "pgweb": 27432, "n8n": 25678, "editor": 28080, "broker": 29679,
    "sandbox": 19080, "telemetry": 19081
  }
}"#;

fn write_meta(envs: &Path, name: &str, text: &str) {
    fs::create_dir_all(envs.join(name)).expect("create folder");
    fs::write(envs.join(name).join("meta.json"), text).expect("write meta");
}

#[test]
fn list_reads_old_and_new_meta_files_sorted_by_name() {
    let home = tempfile::tempdir().expect("temp dir");
    let envs = home.path().join("n8n");
    write_meta(&envs, "zulu", OLD_META);
    write_meta(&envs, "alpha", NEW_META);
    // A folder without meta.json is not a worktree of this tool.
    fs::create_dir_all(envs.join("polished-beam")).expect("create folder");

    let worktrees = Store::new(home.path().to_path_buf()).list().expect("list");

    let names: Vec<&str> = worktrees.iter().map(|meta| meta.name.as_str()).collect();
    assert_eq!(names, ["alpha", "zulu"]);
    assert_eq!(worktrees[0].issue.as_deref(), Some("CONTEXT-424"));
    assert_eq!(worktrees[1].issue, None);
    assert_eq!(worktrees[1].ports.postgres, 25435);
}

fn meta_for(name: &str, slot: u16) -> Meta {
    Meta::new(name, slot, name, "master", "/src/n8n")
}

fn store_with_slots(slots: &[u16]) -> (tempfile::TempDir, Store) {
    let home = tempfile::tempdir().expect("temp dir");
    let store = Store::new(home.path().to_path_buf());
    for slot in slots {
        let name = format!("taken-{slot}");
        let text = serde_json::to_string(&meta_for(&name, *slot)).expect("serialize");
        write_meta(&store.envs_dir(), &name, &text);
    }
    (home, store)
}

#[test]
fn a_claim_takes_the_lowest_free_slot() {
    let (_home, store) = store_with_slots(&[0, 1, 3]);

    let meta = store
        .claim(|_| true, |slot| meta_for("new-one", slot))
        .expect("claim");

    assert_eq!(meta.slot, 2);
    assert_eq!(meta.ports.postgres, 25434);
    let names: Vec<String> = store
        .list()
        .expect("list")
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(names.contains(&"new-one".to_string()));
}

#[test]
fn a_claim_fails_when_all_fifty_slots_are_taken() {
    let all: Vec<u16> = (0..50).collect();
    let (_home, store) = store_with_slots(&all);

    let error = store
        .claim(|_| true, |slot| meta_for("one-too-many", slot))
        .expect_err("no slot is free");

    assert!(
        error.to_string().contains("No free slot"),
        "unexpected error: {error}"
    );
}

#[test]
fn claims_at_the_same_time_get_different_slots() {
    let (home, _store) = store_with_slots(&[]);
    let threads: Vec<_> = (0..8)
        .map(|index| {
            let wt_home = home.path().to_path_buf();
            std::thread::spawn(move || {
                let name = format!("parallel-{index}");
                Store::new(wt_home)
                    .claim(|_| true, |slot| meta_for(&name, slot))
                    .map(|meta| meta.slot)
            })
        })
        .collect();

    let mut slots: Vec<u16> = threads
        .into_iter()
        .map(|thread| thread.join().expect("thread").expect("claim"))
        .collect();
    slots.sort();
    assert_eq!(slots, (0..8).collect::<Vec<u16>>());
}
