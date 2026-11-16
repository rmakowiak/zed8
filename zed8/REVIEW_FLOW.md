# Review flow: draft, send, verify

One review draft per PR collects items from the agent review, the manual test and the guided review. The draft is the only part that sends. Each send starts a round. When the fixes come back, Verify finds out which items they answer. Until then, it waits in standby.

Mockups: https://claude.ai/artifact/9NQc11fQDEL3gL12vnghES

## Terms

- Item: one piece of feedback in the draft. It has a source (agent, manual, guided), a body, and an anchor (file, line, commit) or no anchor.
- Round: one draft and its send. Round 1 is the first review. After Verify finishes, round 2 starts.
- Destination: where a round goes. A GitHub review for someone else's PR, a coding agent thread for your own PR.
- Marker: a hidden HTML comment in every review body, `<!-- zed8 pr-43539 round-1 -->`. Zed8 finds its own reviews on GitHub with it.
- Reconcile: Zed8 asks GitHub (or the thread) what really happened, and sets the round state from that answer.

## Files

All state is in `~/workspace/n8n-reviews/pr-<n>/`, so it survives a restart, a closed worktree and `prune --merged`.

- `rounds/round-<n>.json`: the items, the summary, the decision and the destination. It also holds the commit that the sources ran at, the state, and the send result (review ID, comment IDs, URL, or thread ID).
- `rounds/round-<n>/`: copies of the screenshots that the round attaches. A new manual test run cannot delete them.
- `rounds/round-<n>.answers.json`: the fix thread's answer to each item (own PR only).
- `rounds/round-<n>.verify.json`: the Verify result for each item, with evidence.
- `review.lock`: one Zed8 window at a time writes the review.

## Round states

| State | Means | Next |
|---|---|---|
| Draft | You collect items. Nothing left Zed8. | Sending, Pending |
| Pending | Saved on GitHub as a pending review. Only you see it. | Sent (you submit it here or on GitHub), Draft (you delete it on GitHub) |
| Sending | Zed8 started the send. | Sent, Failed, Unknown |
| Sent | GitHub returned the review ID, or the fix thread started. | Verify can arm |
| Failed | Nothing reached GitHub or the agent. The draft stays as it was. | Sending (Retry), Draft (Edit) |
| Partial | Some of the send reached GitHub, the rest did not. | Sending (Retry sends only the rest) |
| Unknown | Zed8 cannot tell (quit or timeout during Sending). | Reconcile, then Sent or Failed |

## Verify states

| State | Means |
|---|---|
| None | No card. The latest round is not Sent, or it was a plain Approve. |
| Waiting | The round is Sent, and the fixes are not ready. |
| Ready | New commits exist, but nobody said the fixes are ready. "Verify now" shows. Nothing runs. |
| Running | Read and Check run. They only read. |
| Done | Each item has a result. The open items are in the next draft. |
| Rerun pending | Done, and the manual test cases wait for your Start. |
| Stale | New commits arrived after Verify. The results stay, and "Run again" shows. |
| Interrupted | Zed8 quit during Running. Retry shows. Nothing restarts by itself. |
| Failed | The run failed. The error and Retry show. |
| Off | The PR is merged or closed. |

## Rules

These rules stop Verify from showing up without a reason, and stop double posts.

1. Only a Sent round arms Verify. Draft, Pending, Sending, Failed, Partial and Unknown never arm it.
2. A plain Approve does not arm Verify. New commits after an Approve show "N commits since your approval" with "Review the new commits".
3. Verify starts by itself only on a clear signal: the author asks you to review again, or the fix thread writes its answers file. New commits alone only make the card Ready.
4. One Verify runs per round. A second trigger during Running does nothing.
5. Verify never starts the manual test rerun by itself, because it needs a build, a running n8n and the browser.
6. Before a retry, Zed8 reconciles. If GitHub already has a review with the round's marker, Zed8 does not post again.
7. Rounds are never deleted. A Sent round stays readable in the Draft tab.
8. The draft keeps its own copy of each item. A rerun of a source never changes or deletes a ticked item.

## Scenarios

### S1. Someone else's PR, happy path

1. You tick 6 agent findings, 2 manual test cases and 1 guided note. The composer shows 9 items.
2. You choose Request changes and press Submit. The round goes Draft, Sending, Sent. The Verify card shows Waiting.
3. Arvin pushes 3 commits. The card shows Ready, and nothing runs.
4. Arvin asks you to review again. Verify runs Read and Check, then shows Rerun pending for MT4 and MT10.
5. You press Start. MT4 passes, MT10 partly passes. Verify is Done: 6 fixed, 1 partly, 1 not addressed, 1 will not fix.
6. The round 2 draft holds the partly and the not addressed items. You send them. They go out as replies on the original threads.
7. Arvin fixes them. Verify finds nothing open. Approve becomes the default, and you approve. Verify goes to None.

### S2. Your own PR, happy path

1. The composer shows "Send to agent". You pick "New thread", mark 4 items as must fix, and send. After the thread starts, the round is Sent.
2. The fix thread pushes and writes `round-1.answers.json`. Verify starts at once and checks against the local worktree.
3. Verify is Done. You send round 2 to the same thread or a new one, or you comment on the PR.

### S3. The send fails

1. You press Submit while `gh` is logged out. GitHub gets nothing.
2. The round is Failed. The composer shows the `gh` error and Retry. The draft and the decision stay.
3. Arvin pushes commits. The round line shows "2 new commits since you started drafting". No Verify card shows, because the round is not Sent.

### S4. Zed8 quits during the send

1. You press Submit and quit Zed8 at once. The round stays Sending in its file.
2. On start, Zed8 sees Sending and sets Unknown. It reconciles: it looks for your review with the round's marker.
3. If GitHub has it, the round is Sent with that review ID, and Verify arms. If not, the round is Failed and shows Retry.

## Test cases

Each case names the seam that tests it. See "Seams under test".

### Collect

| ID | Given | When | Then | Seam |
|---|---|---|---|---|
| C1 | An agent finding | You tick it in the findings tab | The draft has the item with its file, line, severity and source commit | draft |
| C2 | A ticked finding | You untick it before the send | The item leaves the draft | draft |
| C3 | A ticked finding | The agent review runs again and the finding comes back with the same file, line and text | The tab shows it as ticked. The draft has one item, not two. | draft (fingerprint) |
| C4 | A ticked finding | The agent review runs again and the finding is gone | The item stays in the draft with "source no longer has it" | draft |
| C5 | A ticked manual test case with a screenshot | The manual test runs again and replaces `evidence/` | The draft still shows the screenshot from `rounds/round-1/` | draft |
| C6 | A guided note you wrote | Nothing | It is ticked by default. Claude's replies to it are not ticked. | draft |
| C7 | A guided note on a line that is not in the PR diff | You send | The item goes in the review body as "file:line", not as an inline comment | send plan |
| C8 | Agent review ran at a3ede3e, manual test at an older commit | You open the composer | It warns "sources ran on 2 commits" and names them | draft |
| C9 | A ticked item | You edit its text in the Draft tab | The draft keeps your text. The source tab shows "edited in draft". | draft |
| C10 | Zed8 restarts | You open the review | The draft has the same items, summary and decision | files |
| C11 | A Sent round 1 | You tick a new finding | It goes into the round 2 draft, not into round 1 | draft |

### Decide

| ID | Given | When | Then | Seam |
|---|---|---|---|---|
| D1 | Someone else's PR | The composer opens | Destination is GitHub, and the split button offers Comment, Approve, Request changes and Save as pending | destination |
| D2 | Your own PR | The composer opens | Destination is the coding agent. Approve and Request changes are hidden. | destination |
| D3 | A PR whose author is a bot or another account of yours | You switch the destination by hand | The switch holds, and the decision resets | destination |
| D4 | An empty draft and no summary | You choose Approve | Send is allowed | send guard |
| D5 | An empty draft and no summary | You choose Request changes or Comment | Send is blocked with "Add an item or a summary" | send guard |
| D6 | Items, and the summary is empty | You send | Claude writes the summary first. If that fails, the send stops and the summary box shows the error. | send guard |
| D7 | Text goes out under your name | You open the preview | The text is after the unslop pass. You see the result before the send. | send plan |
| D8 | The PR is merged or closed | You open the composer | Send is off, with "PR merged". The draft stays readable. | send guard |

### Send to GitHub

| ID | Given | When | Then | Seam |
|---|---|---|---|---|
| G1 | `gh` is older than 2.99 and the draft has screenshots | You press Submit | The check stops the send before anything posts, and names the update | preflight |
| G2 | `gh` is logged out or the network is down | You press Submit | Failed. Nothing posted. Draft and decision stay. Retry shows. No Verify card. | round state |
| G3 | A screenshot upload fails | You press Submit | Nothing else posts, because uploads go first. Failed, with the file name. | send plan |
| G4 | The PR head moved since you drafted | You press Submit | Zed8 asks: send at the drafted commit, or check the anchors on the new head | send guard |
| G5 | You send at the drafted commit, and it still exists | GitHub accepts | Inline comments sit on the drafted commit and show as outdated where the code changed | send plan |
| G6 | A force-push removed the drafted commit | You press Submit | Items whose lines are gone move to the body as "file:line". The preview shows which. | send plan |
| G7 | GitHub rejects one inline comment (422, line not in the diff) | The review call fails | Failed, nothing posted (a review call is all or nothing). The item is marked, and Retry offers "move to body". | round state |
| G8 | The manual test cases post as their own comment before the review | The comment posts and the review fails | Partial. Retry sends only the review and links the comment. | round state |
| G9 | Sending | You press Submit again | Nothing happens. The button is off during Sending. | round state |
| G10 | Zed8 quits during Sending | Zed8 starts again | Unknown, then reconcile by marker: Sent or Failed | reconcile |
| G11 | Failed, but the review did reach GitHub (a timeout after the post) | You press Retry | Reconcile finds the marker. The round becomes Sent. Nothing posts twice. | reconcile |
| G12 | Save as pending | GitHub accepts | Pending, with a link. No Verify card. | round state |
| G13 | Pending | You submit it on GitHub in the browser | The next poll finds it submitted. Sent, with the decision you picked there. | reconcile |
| G14 | Pending | You delete it on GitHub | The next poll does not find it. The round goes back to Draft with its items. | reconcile |
| G15 | Sent with Request changes | A maintainer dismisses your review | The round line says "dismissed". Verify stays armed. | reconcile |

### Send to a coding agent

| ID | Given | When | Then | Seam |
|---|---|---|---|---|
| A1 | Your own PR | You send to a new thread | The thread starts with the round file path. After the thread runs, the round is Sent. | round state |
| A2 | `claude` cannot start | You send | Failed, with the error. No Verify card. | round state |
| A3 | The thread ends with no answers file | The thread is Done | The card says "Fix thread ended without answers" with Open thread, Verify anyway, Send again | verify trigger |
| A4 | The answers file skips 2 items | Verify runs | Those 2 items are "no answer" | verify check |
| A5 | The answers name a commit that is not in the worktree | Verify runs | Those items are "answer points to a missing commit" | verify check |
| A6 | You stop the fix thread by hand | The thread is Done | Same as A3 | verify trigger |
| A7 | You send to an existing thread | The thread is busy | Zed8 asks to queue the message or start a new thread | destination |

### Verify trigger

| ID | Given | When | Then | Seam |
|---|---|---|---|---|
| V1 | The round is Draft, Failed, Partial, Unknown or Pending | New commits arrive | No Verify card. The round line counts the commits. | verify trigger |
| V2 | The round is Sent | New commits arrive and nobody asks again | Ready. Nothing runs. | verify trigger |
| V3 | The round is Sent | The author asks you to review again | Running | verify trigger |
| V4 | The round is Sent | The author asks again, with no new commits | Running. Check uses the replies only. | verify trigger |
| V5 | The round is Draft (never sent) | The author asks you to review again | No Verify card. The Work panel row says "asked again". | verify trigger |
| V6 | The last round was a plain Approve | New commits arrive | No Verify card. "N commits since your approval" with "Review the new commits". | verify trigger |
| V7 | The last round was Approve with comments | New commits arrive | No Verify card, same as V6 | verify trigger |
| V8 | Running | The author asks again, or you press Verify now | Nothing new starts | verify trigger |
| V9 | Done | More commits arrive | Stale. The results stay, and "Run again" shows. | verify trigger |
| V10 | Running | Zed8 quits and starts again | Interrupted, with Retry. Nothing starts by itself. | verify trigger |
| V11 | The PR is merged or closed | Any trigger | Off | verify trigger |
| V12 | Two Zed8 windows on the same worktree | Both see the trigger | One runs Verify. The other shows the same card from the files. | files (lock) |
| V13 | Own PR | The answers file appears | Running at once, against the local worktree, even with no push | verify trigger |

### Verify check

| ID | Given | When | Then | Seam |
|---|---|---|---|---|
| K1 | An item's line changed in the new commits and its thread has "fixed in sha" | Check runs | Fixed, with the commit as evidence | verify check |
| K2 | No change near the item and no reply | Check runs | Not addressed | verify check |
| K3 | The author replied with a reason and changed nothing | Check runs | Will not fix, with the quote | verify check |
| K4 | The author resolved the thread and changed nothing | Check runs | "Resolved without change". It is not counted as fixed. | verify check |
| K5 | The new commits only merge master | Check runs | The PR's own changes did not change. The card says "only a base merge". Items are not addressed. | verify check |
| K6 | A force-push removed the round commit | Check runs | Check compares files, not the commit range, and the card says so | verify check |
| K7 | The item's file was deleted or renamed | Check runs | The item follows the rename. A deleted file gives "file removed", for you to judge. | verify check |
| K8 | Files changed that no item covers | Check is done | The card offers "Review the N files" (one agent lens on them) | verify check |
| K9 | The Claude run fails or hits a limit | Check runs | Failed, with the error and Retry. The round files are unchanged. | verify trigger |
| K10 | A manual test case reran | The case fails again | Partly or not addressed, with both screenshots side by side | verify check |
| K11 | Local n8n runs an older commit | You press Start on the rerun | Zed8 restarts it on the new commit first, or asks | verify check |

### Next round

| ID | Given | When | Then | Seam |
|---|---|---|---|---|
| N1 | Verify is Done | The round 2 draft opens | It holds the partly and not addressed items, ticked. Will not fix items are there, not ticked. | next round |
| N2 | A carried item | You send round 2 | It goes out as a reply on the original GitHub thread, not as a new comment | send plan |
| N3 | Items Verify marked fixed | You send round 2 | "Resolve 6 fixed threads" is on by default. You can turn it off. | send plan |
| N4 | Nothing is open after Verify | The composer opens | Approve is the default | next round |
| N5 | Round 1 is Sent | You open the Draft tab | Round 1 is readable, with its review link, and cannot be edited | files |
| N6 | The worktree was pruned | You open the review from the Work panel | The review files are still there. "Open review" makes the worktree again. | files |

### Work panel

| ID | Given | When | Then |
|---|---|---|---|
| W1 | A review request with no worktree | The Reviews tab loads | Waits on you, with "Start review" |
| W2 | Verify is Done, or Verify is Ready on your own PR | The tab loads | Waits on you, with the round state |
| W3 | Your round is Sent and Verify is Waiting | The tab loads | Waits on the author |
| W4 | A send failed | The tab loads | Waits on you, with "Send failed" |
| W5 | The PR merged | The tab loads | The row leaves the tab |

## Seams under test

These are pure functions over data, so the tests need no GitHub and no Zed window.

- `round::next(state, event) -> state`: every round state change, including Failed, Partial and Unknown.
- `verify::trigger(round, pr, answers, verify) -> VerifyState`: rules 1 to 4, and V1 to V13.
- `draft::add / remove / fingerprint`: C1 to C11.
- `destination::pick(pr, me) -> Destination`: D1 to D3, A7.
- `send::guard(draft, pr) -> Result<(), Block>`: D4 to D8, G1, G4.
- `send::plan(draft, pr) -> Vec<Step>`: the calls in their order, what goes inline and what goes in the body, replies on carried items, resolves. G3, G5 to G8, C7, N2, N3.
- `reconcile(round, reviews_on_github) -> state`: G10, G11, G13 to G15.
- `verify::check` is a Claude run, so its tests are fixtures: a round file, a diff and replies in, the expected result per item out (K1 to K7).

## Open questions

- O1. Does `gh pr review` take `--attach` in gh 2.99 or newer? If yes, the manual test screenshots go in the review body, and G8 cannot happen. If no, the cases post as their own comment first (G8).
- O2. Re-request: GitHub shows it as a `review_requested` event after your review. Zed8 needs to poll the PR timeline for it, or read it from `gh pr view --json reviewRequests`.
- O3. Resolving threads needs the GraphQL `resolveReviewThread` call. `gh api graphql` can do it.

## Build 1 (2026-11-16)

Code: `crates/plannotator_review/src/review_round.rs` (the rules, with tests), `review_send.rs` (files and GitHub calls), `panel/draft.rs` (composer, Verify card, header tools), `draft_view.rs` (the Draft tab), and `crates/work_panel/src/reviews.rs` (the Reviews tab). The skills are `review-verify` and `review-fix`. `pr-review` stops after gate 2 when Done says `ZED8-DRAFT:`.

These parts of the spec are not built yet:

- D6: Claude does not write an empty summary. A round with items sends without one.
- D7: the unslop pass does not run before a send.
- Your comments in a guide chapter join the draft with "Add N to draft" on the tab bar, which moves them out of the editor. There is no checkbox on each comment. Replies to Claude's notes stay with "Ask Claude".
- G4: Zed8 does not ask. The composer warns that the PR moved and sends at the drafted commit.
- The fix thread is always a new thread (A7 and the thread picker are not built).
- N3: fixed threads are not resolved.
- K10 and K11: Verify does not rerun manual test cases. It marks them "needs a rerun".
- The Draft tab shows the items but cannot edit their text, and has no GitHub preview.
