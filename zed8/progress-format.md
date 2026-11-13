# Progress file of a review action

The Review panel of Zed8 starts each review action (Agent review, Manual test, Guided review) in its own Claude thread. The first prompt names the skill and a progress file:

```
/pr-review --state ~/.plannotator/zed-actions/<worktree>/agent-review.json
```

The skill writes that file each time its step changes. Zed reads it every 2 seconds and shows it on the card of the action. Without `--state`, the skill writes no progress file.

## Format

One JSON object. Every field is optional. Write the whole object each time, with a plain overwrite.

```json
{
  "steps": ["Plan", "Lenses", "Findings", "Post"],
  "step": 1,
  "detail": "Lenses 2/4",
  "summary": "Plan approved 14:06 with 3 answers",
  "waiting": false,
  "done": false
}
```

- `steps`: the step names of the card, in order. Without it, Zed shows the default steps of the action.
- `step`: the index of the current step in `steps`. The steps before it show as done.
- `detail`: a short text that replaces the name of the current step, such as `Run 3/8`. Keep it under 16 characters.
- `summary`: one line under the card name. Say what happens now or what the result is, such as `7 passed, 1 failed`.
- `waiting`: `true` while Claude waits for an answer in chat. A Plannotator session already counts as waiting, so set it only for a question in chat.
- `done`: at the end of the action, `true`.

## Steps of each action

| Action | Skill | Steps |
|---|---|---|
| Agent review | `pr-review` | Plan, Lenses, Findings, Post |
| Manual test | `manual-test` | Charter, Instance, Run, Results |
| Guided review | `zed-guided-review` | Guide, Chapters, Feedback |

Write the file with one shell command, for example:

```bash
cat > "$STATE" <<'EOF'
{"steps":["Plan","Lenses","Findings","Post"],"step":1,"detail":"Lenses 0/4","summary":"Plan approved"}
EOF
```
