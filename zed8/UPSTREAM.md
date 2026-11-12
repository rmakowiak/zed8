# Zed8 changes to upstream files

Zed8 is Zed (`zed-industries/zed`, branch `main`) plus the changes below.
Most of Zed8 lives in new places: `crates/plannotator_review/` and `zed8/`.
Upstream never touches them, so they never conflict.

The files in this list belong to upstream. A merge from upstream can
conflict in them. Keep each change small. If you add a change to an
upstream file, add a row here in the same commit.

| File | Change | Why |
|---|---|---|
| `Cargo.toml` | `crates/plannotator_review` in `members` and in `[workspace.dependencies]` | Registers the review crate. |
| `Cargo.lock` | Entries for `plannotator_review` | Follows from `Cargo.toml`. On a conflict, take upstream's file and run `cargo build` to add the entries again. |
| `crates/zed/Cargo.toml` | `plannotator_review.workspace = true` | The app depends on the crate. |
| `crates/zed/src/main.rs` | `plannotator_review::init(cx);` after `git_ui::init(cx);` | Starts the review panel and its actions. |
| `crates/feature_flags/src/flags.rs` | `DiffReviewFeatureFlag::enabled_for_all` returns `true` | Inline review comments in diffs are on without a settings line. |
| `crates/feature_flags/src/store.rs` | `overrides_enabled` ignores the staff and debug check | Settings can turn other flags on in a release build. |
| `crates/editor/src/editor.rs` | `ExportedReviewComment` in the `pub use git::{...}` list | The crate reads review comments. |
| `crates/editor/src/git.rs` | `ExportedReviewComment` and `take_review_comments_for_export` | The crate takes the comments of a diff to send them. |
| `crates/git_ui/src/branch_diff.rs` | `BranchDiff::editor` is public (no `cfg(test)`) | The crate reads comments in the branch diff. |
| `crates/agent_ui/src/agent_panel.rs` | `AgentPanel::terminal_view(terminal_id)` | The crate finds Claude terminal threads, also in the background. |
| `crates/sidebar/src/sidebar.rs` | Worktree groups. See the list under the table. | The threads sidebar groups threads by worktree. This is the largest change to upstream code. |
| `crates/sidebar/src/sidebar_tests.rs` | `test_threads_are_grouped_by_worktree` | Tests the grouping. |

The sidebar change has these parts:

- the `WorktreeHeading` struct and the `worktree_headings` and `grouped_rows` fields of `SidebarContents`
- `push_entries_by_display_time`, which groups the rows
- `render_worktree_heading`, and the heading and guide line in `render_list_entry`
- no worktree chip on grouped rows, in `render_thread` and `render_terminal`
- the heading flag in `EntryShape`

## Build settings that are not in the code

The script `zed8/bundle.sh` sets these values for the app build:

- `ZED_RELEASE_CHANNEL=stable`: Zed8 uses your real Zed data.
- `ZED_UPDATE_EXPLANATION`: turns off auto-update. An update from zed.dev replaces Zed8 with official Zed.
- `--features gpui_platform/runtime_shaders`: Metal shaders compile at run time, because this Mac has no full Xcode.
- `CARGO_TARGET_DIR=target/zed8`: the settings above change how most crates compile, so the app has its own build folder.
