# Thread board in the threads sidebar

Each Claude Code terminal thread in the threads sidebar shows its state, the time in that state, and its tokens. The code is in `crates/thread_board/`. The sidebar calls it through the hooks that `UPSTREAM.md` lists.

Mockups (approved 2026-11-14): https://claude.ai/artifact/QDcFd3YAGF3Jd89LVurAUK

## Decisions

- A thread that waits on you stays in its worktree group. Nothing moves to the top of the sidebar.
- A worktree heading shows a yellow count of threads that wait, a red count of stuck threads, and the tokens of the group. Without any thread state, it shows the thread count as before.
- The state line under the title has one color per state:
  - Waits on you (yellow): a Plannotator gate of the Review panel, an `AskUserQuestion` call, or a tool call while the terminal bell rang (a permission prompt).
  - Running (blue): the Claude process file says `busy`. The line names the open tool call, or says "Thinking".
  - Stuck (red): busy, and the transcript did not change for 5 minutes, or the last 3 tool results were errors.
  - Done (green, dim title): the process is idle or stopped.
- The layout is mockup A. Each row has the title, the state line, the time in that state and the tokens. A selected row shows only the highlight. Nothing opens under it.
- The footer counts running, waiting and stuck threads, and a "Today" button opens the token breakdown: by worktree, by kind of token, and the top 3 threads, for today or the last 7 days.
- The price is an estimate at API prices. A subscription does not pay per token.
- Subagent tokens count toward their thread.

## Data

All data is local:

- `~/.claude/sessions/<pid>.json`: the session ID and `busy` or `idle`, for the Claude process in the terminal.
- `~/.plannotator/zed-claude-terminal-sessions.json`: the last session of a thread whose Claude process stopped. The Review crate's session recorder writes it.
- `~/.claude/projects/<cwd>/<session>.jsonl` and its `subagents/` folder: tokens per message, the model, tool calls and your messages. The board reads only the lines added since its last read.

Each workspace reads its terminal threads every 2 seconds.

## Where the build differs from the mockups

- The board view (B) is not built, because it groups threads by state and not by worktree.
- The details under the selected row (C) and the stuck details with "Not stuck" (D) are not built. The stuck row gives its reason in the state line.
- The token breakdown has Today and 7 days. "This PR" is not built.
