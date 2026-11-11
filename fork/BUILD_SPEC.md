# Plannotator review in Zed: build spec

This fork adds Plannotator's guided code review and spec review to Zed. The review shows as a Zed tab and a dock panel, bound to the worktree of the window.

Mockup (approved 2026-11-11, version 3): https://claude.ai/artifact/8E1RRDc2xRn584Tj6LBUg7

## Decisions

### Code review

- A code review is always for a GitHub PR. A branch without a PR shows the review entry as disabled, with the reason.
- You can review any PR: your own or another person's.
- The diff comes from local git: the merge-base of the PR base branch and the local HEAD. If the local branch and the PR head are different, Zed shows a warning.
- The guided review extends Zed's branch diff view ("git: diff branch"). It does not add a new view.
- The tab shows one chapter at a time. The dock lists all chapters with a reviewed tick.
- Claude's notes from the guide are inline blocks under their lines. They are foldable and have a ✦ mark.
- Mermaid diagrams render to an image with a local tool (`mmdc`).
- Comments are plain comments. There are no comment types (changed on 2026-11-11 after the first test).
- The decision buttons are Approve and Send feedback.
- You start a review from a "Review PR" button in the git panel or the branch diff toolbar, or from the command palette.

### Spec review

- An agent always opens a spec review. There is no manual entry.
- The spec uses the Plannotator format: text you comment on, plus question blocks with choices.
- Zed's markdown preview shows the spec, with comment blocks and question cards inside the preview.
- A new round highlights the changes since the last round. Old comments show as resolved or still open.
- Approve sends "approved" to Claude and records the status with a date. Nothing else happens.

### Both

- Zed gets a native UI on the Plannotator protocol. If Zed is not open, the browser stays the fallback.
- Zed always goes through a Plannotator session. If you start a review from Zed, Zed types the review command into the bound Claude terminal. Claude starts the session, and Zed joins it with no browser.
- Claude Code in the bound terminal thread writes the guide.
- Notes go to the thread that started the review. The tab header and the dock show the thread name, and a click opens a picker.
- Send goes to the Plannotator session that Claude waits on, never to the Zed Agent panel. Claude decides whether to post on GitHub.
- The reviewed state and unsent drafts live in Plannotator storage (`~/.plannotator/`).
- The UI uses Zed's own muted palette. Color appears only in diff bars, syntax and focus.

## Starting points in the Zed source

- Inline review comments and "Send review to agent" already exist behind the feature flag `diff-review` (`crates/editor/src/git.rs`, `crates/git_ui/src/branch_diff.rs`, `crates/feature_flags/src/flags.rs`).
- The shipped "Review Diff" button is a different feature. It opens a new Agent panel thread with a fixed review prompt plus the whole branch diff, and sends it (`ReviewBranchDiff` in `crates/agent_ui/src/agent_panel.rs`). There, the AI reviews the code. In guided review, the AI explains the code and you review it. Its `external_thread(… AgentInitialContent …)` call shows how Zed starts an agent thread with a prompt.
- A debug build honors `"feature_flags": {"diff-review": "on"}` in settings. A release build ignores overrides for users who are not staff.
- Plannotator protocol notes: `docs/agent-guides/server-and-storage.md` and `guides-and-shared-ui.md` in the Plannotator repo (github.com/backnotprop/plannotator).

## Fork rules

- Keep new code in its own crate. Change upstream files only through thin hooks: register the pane, add the dock panel, add commands.
- Rebase on upstream about once a month.

## Plan

1. Build stock Zed in debug mode and turn on `diff-review`. Try the inline comments on a real PR. Done 2026-11-11: the debug build takes about 7 minutes. The comment box works in the branch diff. It is a single-line input with an avatar, a cancel button and a submit button, and it has no comment types yet.
2. Send feedback to the Plannotator session instead of the Agent panel (was step 4, moved up on 2026-11-11 to get one full loop first). In progress on branch `plannotator-review`:
   - The crate `crates/plannotator_review` reads `~/.plannotator/sessions/` and posts to `/api/feedback`.
   - It uses the newest session with mode `review`, a target inside the worktree, and a running pid.
   - Upstream has no handler for `SendReviewToAgent`. The button dispatches it and nothing happens. Our crate is the only handler.
   - If the send fails, the comments go to `~/.plannotator/zed-unsent/<ms>.md`.
   - Tested 2026-11-11: "Send review" in the branch diff sent 5 comments to a waiting `plannotator review --json` session, with file paths and line numbers.
3. Extend the Plannotator client: read guides, drafts and the reviewed state.
4. Add the Review dock panel and the chapter mode in the branch diff. In progress: the dock panel and the chapter tabs work. Claude's notes show as foldable blocks in the chapter tab (2026-11-11). A new-side note goes below its last line. An old-side note goes above its diff hunk.
5. Add spec review in the markdown preview.

## Fork changes to upstream files

Keep this list short. Each item is a thin hook.

- `crates/feature_flags/src/store.rs`: `overrides_enabled` honors `settings.json` overrides in release builds too.
- `crates/editor/src/git.rs` and `editor.rs`: `take_review_comments_for_export` and `ExportedReviewComment`.
- `crates/git_ui/src/branch_diff.rs`: `BranchDiff::editor` is public, not test-only.
- `Cargo.toml`, `crates/zed/Cargo.toml`, `crates/zed/src/main.rs`: register the crate and call `plannotator_review::init`.

## Build setup

- Rust comes from rustup in the home folder (`~/.cargo/bin`). The toolchain version is in `rust-toolchain.toml`.
- cmake is in a private venv, linked to `~/.local/bin/cmake`.
- Without Xcode, build with runtime shaders: `cargo build -p zed --features gpui_platform/runtime_shaders`.
- The debug build is slow to use. For daily use, build `--profile release-fast` (optimized, no link-time optimization). The binary is `target/release-fast/zed`.
- Run the fork with its own data folder: `zed --user-data-dir ~/workspace/zed-dev-data`. Its `config` folder is a copy of `~/.config/zed`, with `"feature_flags": {"diff-review": "on"}` added. The extensions and the Claude Code agent come from `~/Library/Application Support/Zed`.

## Todo

- [ ] Update macOS, then install Xcode from the App Store. The App Store Xcode needs a newer macOS than 26.4. After the install, run:

  ```sh
  sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
  sudo xcodebuild -license accept
  xcodebuild -runFirstLaunch
  xcodebuild -downloadComponent MetalToolchain
  ```

  Then check that `xcode-select -p` prints the Xcode path and that `xcrun metal --version` works. A release bundle (`script/bundle-mac`) needs this. A debug build does not.
