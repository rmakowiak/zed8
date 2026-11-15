# Linear in Zed8, and n8n-worktree-manager

This plan adds Linear to Zed8 and replaces the Bash tool `n8n-wt` with a Rust crate, `n8n_worktree_manager`. Zed8 uses the crate as a library. The terminal and Raycast Workbench use its binary, `n8n-worktree-manager`.

Status (2026-11-15): phases 1, 2 and 4 are built, and phase 3 (cutover) is done up to the app install. The crate `crates/n8n_worktree_manager` has 36 tests, all green, and clippy is clean. Phase 5 (Zed8 Work panel) is next.

Backlog of ideas from the review: the Apple Notes note "Zed8 backlog".

Mockups (canvas, option A chosen): https://claude.ai/artifact/61YnhfcBgey39cLH1W3iFV
First refactoring plan (partly replaced by this file): https://claude.ai/artifact/G819Rv5iB8w4wzy3SDh5Rg

## Use cases

1. See upcoming issues: status "Ready for pick up" and assigned to me.
2. See the issues of one project.
3. Preview an issue or a project without leaving Zed8.
4. Start a worktree for an issue, on the Linear branch name, and keep the link from the worktree to the issue.
5. Never tell Claude which issue a worktree is for. Every Claude session in the worktree knows it.

## Decisions

- UI is mockup A: a Work panel in the right dock, with two tabs, Up next and Project.
- A preview of an issue or a project opens as a tab in the center pane.
- Start work asks for confirmation. Then it moves the issue to In Progress, creates the worktree and opens it.
- A worktree opens in the current Zed8 window and gets the focus. It never opens a new window.
- The Project tab shows my projects first. A search box searches all projects of the workspace.
- Zed8 and Workbench are two interfaces with the same functionality. The crate owns the Linear queries, the join of issue, branch, worktree and PR, and every write.
- Protocol: the CLI with `--json` (JSON on stdout, errors on stderr, exit code), plus a cache file that both clients read. No daemon, no MCP.
- Linear auth: a personal API key in `secrets.env`, so it works outside Raycast.
- The link from worktree to issue is an `issue` field in `meta.json`. The issue key in the branch name is the fallback.
- A Claude Code SessionStart hook runs `n8n-worktree-manager context`. It reads local files only and prints the issue, the ports and the URLs of the worktree.

## What the tool keeps and what it drops

Removed from `n8n-wt` on 2026-11-14: SigNoz and MCP Inspector, with their Docker data.

The Rust version keeps these jobs: create, read, change and tear down a worktree, point it at the shared services (AI sandbox and telemetry capture, two rows in the services table), and the editor badge.

The editor badge stays. `provision` copies the vite plugin template, patches `vite.config.mts`, marks it skip-worktree, and builds editor-ui a second time with `N8N_WT_LABEL`. The port to Rust is a small step: one template file, one text patch, one git command.

The Rust version drops these parts of `n8n-wt`:

| Part | Why it goes |
| --- | --- |
| Chrome tab groups, tab bridge, `open` in Chrome | No browser features. |
| `adopt` and the `create_worktree` hook in `~/.config/zed/tasks.json` | The Work panel creates worktrees, not the Zed worktree picker. |
| `registry.json` and `prune` | `meta.json` already holds the slot and the ports. The list of worktrees is the folders that contain a `meta.json`. |
| The CLAUDE.md block (skip-worktree patch of a tracked file) | `context` gives the same facts through the SessionStart hook. |
| `go`, `zed`, `badge` as commands | `create` is safe to run twice. `sync` rewrites the generated files and the badge. `open` focuses. |
| Rollback of a failed create | Provisioning resumes from the failed step instead. |

## Commands

```
n8n-worktree-manager list [--json]
n8n-worktree-manager show <name> [--json]
n8n-worktree-manager context [<dir>]
n8n-worktree-manager create <name> [--branch <b>] [--base <b>] [--issue <KEY>] [--queue] [--no-sandbox] [--no-telemetry] [--no-build] [--no-provision] [--open]
n8n-worktree-manager provision <name>
n8n-worktree-manager set <name> [--issue <KEY>] [--queue on|off] [--service <service>=on|off]
n8n-worktree-manager sync <name>|--all
n8n-worktree-manager open <name>
n8n-worktree-manager teardown <name> [--force] [--delete-branch] [--yes]
n8n-worktree-manager services up|down|status [<service>]
n8n-worktree-manager linear issues|projects [--search <text>]|project <id>|issue <KEY> --json
n8n-worktree-manager start <KEY> [--no-provision] [--open]
```

## Crate

```
crates/n8n_worktree_manager/
  Cargo.toml          [lib] path = "src/n8n_worktree_manager.rs", [[bin]] n8n-worktree-manager = "src/main.rs"
  src/n8n_worktree_manager.rs   public API: list, show, create, provision, set, teardown, context
  src/model.rs        Worktree, Ports, Services, SetupState, the meta.json schema
  src/render.rs       pure functions: ports from slot, env.sh, compose .env, .zed files, context text
  src/store.rs        scan worktrees, claim a slot under a lock, atomic writes
  src/system.rs       the System trait for git, docker compose and pnpm, and a fake for tests
  src/services.rs     one table: name, port, env lines, start command, health URL
  src/linear.rs       GraphQL client, cache file, join of issue and worktree
  src/main.rs         clap CLI: text for people, --json for Raycast
  templates/          docker-compose.yml, compiled in with include_str!
```

Rules:

- The crate does not depend on gpui. `cargo build -p n8n_worktree_manager` compiles in seconds, and the Work panel crate depends on this crate, never the reverse.
- All file content comes from pure functions in `render.rs`.
- Only `system.rs` starts processes.
- Every write to `meta.json` goes to a temp file first, then a rename.
- The state stays where it is today, in `~/workspace/n8n-worktrees`. Existing worktrees keep working with no migration.

### Create in two phases

1. `create` claims the slot, makes the git worktree and writes `meta.json` with `setup.status = pending`. It takes a few seconds.
2. `provision` runs the steps: compose up, wait for Postgres, write env.sh and the Zed files, pnpm install, pnpm build. It records each step and its result in `meta.json` under `setup`.

If a step fails, the worktree stays, and every list shows "failed at <step>". `provision` again resumes from that step. `teardown` removes it.

From the Work panel, Zed8 calls `create` as a library function and opens the folder in the current window. Then it runs `n8n-worktree-manager provision <name>` as a terminal task in that worktree group. The panel row reads the progress from `meta.json`. Raycast and the terminal run the same two steps through the CLI.

### Packaging

`zed8/bundle.sh` builds the binary and copies it to `Zed8.app/Contents/MacOS/n8n-worktree-manager`. `zed8/install.sh` links it to `~/.local/bin/n8n-worktree-manager` and to `~/.local/bin/n8n-wt`, so the short name keeps working. Workbench calls the absolute path, with no `zsh -ilc`.

`provision` runs pnpm, and pnpm needs the nvm Node version. The binary finds it from nvm's default alias, or from `node_bin_dir` in its configuration file.

`open` sends `zed://n8n-worktree-manager/open?name=<name>` to Zed8. Zed8 switches the current window to that worktree.

## Phases

Each phase is red, green, refactor: one failing test, the least code to pass it, then the next test. Refactoring happens at review time, after a slice works.

1. Golden files. Run the Bash functions with a fake `secrets.env` in a temp `N8N_WT_HOME` and save the output for a few configurations: `env.sh`, the compose `.env`, `.zed/tasks.json`, `.zed/settings.json`, `meta.json`. These files are the independent source of truth for the Rust tests. No real secret goes into a golden file.
2. The crate, slice by slice, in the order of the test backlog below.
3. Cutover. The new binary replaces `n8n-wt`. Workbench switches to `list --json`. The SessionStart hook goes in. The `create_worktree` hook and the CLAUDE.md blocks go away.
4. Linear in the crate: client, cache, `start`.
5. Zed8 Work panel, preview tabs, Start work dialog, `zed://` open.

## Seams under test

A seam is the public boundary that a test uses. Tests use only the seams that are confirmed below.

| Seam | What it catches | What it misses |
| --- | --- | --- |
| S1. `render` functions with plain values in, text out | Wrong ports, wrong env.sh lines, a difference from the Bash golden files | Whether the files reach the disk, process calls |
| S2. The store on a temp folder | Slot collisions, the lock, reading old `meta.json` files (with `signoz` keys, without `issue`) | Git and Docker |
| S3. The public API (`create`, `provision`, `set`, `teardown`, `list`) on a temp folder with a fake `System` | The order of the steps, resume after a failure, a teardown that leaves nothing, files on disk, the commands that were run | How real git, Docker and pnpm behave |
| S4. The binary, run as a process | The `--json` contract that Workbench reads, exit codes, errors on stderr | Internal logic, which S1 to S3 cover |
| S5. The Linear client with a fake HTTP client | Parsing of the GraphQL answers, the join of issue and worktree, the cache | Changes to the real Linear API |

No automated tests for the Work panel UI in this plan. It gets a manual test.

## Test backlog for phase 2

One test at a time, in this order. Each test drives the next piece of code.

1. S1: slot 0 gives Postgres 25432, Redis 26379, Mailpit SMTP 21025, Mailpit HTTP 28025, pgweb 27432, n8n 25678, editor 28080, broker 29679. Slot 3 adds 3 to each.
2. S1: env.sh for a worktree with sandbox and telemetry on equals its golden file.
3. S1: env.sh with sandbox and telemetry off has no sandbox and no telemetry lines, and equals its golden file.
4. S1: the queue mode adds `EXECUTIONS_MODE=queue` and the Redis lines.
5. S1: `.zed/tasks.json` and `.zed/settings.json` equal their golden files.
6. S2: `list` on a folder with two real `meta.json` files (one from before the `issue` field) returns both, sorted by name.
7. S2: a claim gives the lowest free slot. If all 50 slots are taken, the claim fails with a clear error.
8. S2: two claims at the same time get different slots.
9. S3: `create` runs `git worktree add` on the right branch and writes `meta.json` with `setup.status = pending`.
10. S3: `create` for a name that exists returns the existing worktree and runs nothing.
11. S3: `provision` runs the steps in order and marks each as done.
12. S3: when `pnpm install` fails, `meta.json` says "failed at install". A second `provision` starts at install, not at compose up.
13. S3: `teardown` runs `docker compose down -v`, removes the git worktree and the folder, and frees the slot.
14. S3: `set --issue CONTEXT-424` writes the key to `meta.json`.
15. S1: `context` prints the issue key, the ports and the URLs. Without an `issue` field, it takes the key from the branch name.
16. S4: `list --json` prints the agreed JSON shape. An unknown name exits 1 with the error on stderr.

## Answers (2026-11-15)

- Seams: all five, S1 to S5.
- Expected values: golden files from the Bash version.
- Command names: `n8n-worktree-manager`, plus an `n8n-wt` link to the same binary.
- Editor badge: keep. CLAUDE.md block: drop.
- AI sandbox and telemetry capture: keep, as two rows in the services table.

Test 17 for the backlog: S3, `provision` writes the badge plugin, patches `vite.config.mts`, and runs the second editor-ui build with `N8N_WT_LABEL`. The patched file equals its golden file.

Ideas from the review that are not in this plan (prune of merged worktrees, setup progress in the Review panel, a worktree health view) are in the note "Zed8 backlog".

## Build notes (2026-11-15)

What changed from the plan while building:

- `teardown` keeps the branch by default, like the Bash version. `--delete-branch` deletes it. The plan had `--keep-branch`, which would delete unpushed work by default.
- `create` in the terminal also runs `provision`, like `n8n-wt create`. `--no-provision` stops after the fast part. Zed8 calls the library and runs `provision` as a task.
- `create --branch` takes a local branch, then a branch on origin, then makes a new branch from `--base`. `start` uses this for the Linear branch name.
- The provision steps are: files, compose up, wait for postgres, install, build, badge build. Files come first, so the Zed tasks exist while the build runs. A failed badge build is a warning, like in Bash.
- The slot lock is the same folder as in Bash (`registry.lock`), so both tools can run side by side until the cutover. `registry.json` is not read.
- `list --json`, `services status --json` and `linear issues|projects` carry `schema_version: 1`.
- Linear goes through curl. The key goes to curl on stdin, so it is not in the process list. The key is `LINEAR_API_KEY` in `secrets.env`.
- `context` takes the issue title from `cache/linear-issues.json`, which `linear issues` writes. It needs no network.
- The hidden commands `telemetry up|down|status|tail` and `sandbox up|down|status` keep the old calls working, for example the one in `crates/plannotator_review/src/tools.rs`.

Worktree flags: `set <name> --flag 136_skill_sync=true` writes the flag to `"flags"` in `meta.json`, and `--no-flag <key>` removes it. The flags are merged into `FEATURE_FLAG_OVERRIDES` from `secrets.env`, and a worktree flag wins on the same key. `set` and `sync` still rewrite the whole `env.sh`, so a hand edit is lost. Put flags in `meta.json` instead. The hand-added flag of `pr-43011` is moved there.

Parity check: for all 15 real worktrees, the Rust `env.sh`, `.env`, `.zed/tasks.json` and `.zed/settings.json` equal the Bash files, except for the hand edit of `pr-43011`, which is now a worktree flag.

Tests by seam: S1 render 7, S2 store 4, S3 API 12, S4 binary 2, S5 Linear 7, plus the Bash golden files in `tests/golden` (`generate.sh` makes them again).

Cutover (phase 3), 2026-11-15:

- Done: the SessionStart hook in `~/.claude/settings.json`, the empty `~/.config/zed/tasks.json` (no `adopt` hook), the CLAUDE.md blocks removed from the 7 checkouts (each file equals HEAD again, skip-worktree cleared), and `install.sh` links `~/.local/bin/n8n-wt` to the new binary.
- Raycast `go.ts` runs `n8n-worktree-manager create <name> --branch <branch> --open --notify`, and `worktrees.ts` reads `list --json`. The change takes effect after `npm run dev` in `raycast/`.
- `create --branch` now covers `go`. It returns the worktree whose checkout is on the branch (read from git, not meta.json), and it fetches over the HTTPS URL of origin, because a Raycast shell has no SSH key. `--notify` (any command) shows a macOS notification at the end.
- Old calls keep working: `ls`, `rm`, `down`, `create --skip-build`, `create --no-open`. `open` has no `-e`, so it focuses the window of the worktree and never replaces the project of the front window.
- Left: the app install (quit Zed8), `npm run dev` for Raycast, and `LINEAR_API_KEY` in `secrets.env`.
