---
name: zed8-update
description: Update Zed8, Roman's personal build of Zed, from upstream Zed. Merges zed-industries/zed main into the zed8 branch, fixes conflicts and build errors, runs the tests, builds Zed8.app and pushes to rmakowiak/zed8. Use when Roman says "update zed8", "pull upstream zed" or types /zed8-update.
---

# Update Zed8 from upstream

Zed8 is Zed plus a review panel for Plannotator, worktree groups in the threads sidebar, and a few hooks in upstream files. The repo is `~/workspace/zed`, branch `zed8`.

- `origin` is `https://github.com/rmakowiak/zed8.git`, Roman's public fork.
- `upstream` is `https://github.com/zed-industries/zed.git`.
- `zed8/UPSTREAM.md` lists every change to an upstream file. Read it before you start.

## Rules

- Merge upstream into `zed8`. Do not rebase, and do not force-push.
- Never push to `upstream`.
- Do not quit or restart Zed8 without Roman's yes. Quitting it ends the Claude threads that run in it, possibly this one.
- If you change an upstream file, add a row to `zed8/UPSTREAM.md` in the same commit.

## Steps

1. Go to `~/workspace/zed`. Make sure that the branch is `zed8` and that `git status` is clean. If it is not clean, stop and tell Roman.
2. Run `git fetch upstream main`. Write down the merge base: `git merge-base HEAD upstream/main`.
3. Show Roman the number of new upstream commits: `git log --oneline <merge base>..upstream/main | wc -l`. Continue without waiting.
4. Run `git merge upstream/main --no-edit`.
5. If there are conflicts, resolve each file:
   - In a file from `zed8/UPSTREAM.md`, take upstream's version, then apply the Zed8 change again as the table describes.
   - In `Cargo.lock`, take upstream's version (`git checkout --theirs Cargo.lock`). The build in step 6 adds the Zed8 entries again.
   - A conflict in a file that is not in the table is a mistake. Take upstream's version and tell Roman.
   - Commit the merge.
6. Build. Put `~/.cargo/bin` on `PATH` first.

   ```sh
   cargo build --profile release-fast -p zed --features gpui_platform/runtime_shaders
   ```

   Upstream can rename or change the APIs that `crates/plannotator_review` uses. Fix the crate, not upstream code. If a hook in an upstream file breaks, fix the hook and update its row in `zed8/UPSTREAM.md`. Commit the fixes on top of the merge.
7. Test:

   ```sh
   cargo test -p plannotator_review
   cargo test -p sidebar -p gpui_platform --features gpui_platform/runtime_shaders
   ```

   The sidebar tests need `-p gpui_platform`. Without it, the Metal shaders do not compile on this Mac. If an upstream sidebar test fails because threads now group by worktree, fix the expected rows in the test.
8. Build the app: `zed8/bundle.sh --no-install`. The first build in `target/zed8` takes about 20 minutes. Later builds are faster.
9. Push: `git push origin zed8`. If SSH has no identity, the HTTPS URL above works through the `gh` credential helper.
10. Install. Ask Roman first. When he says yes, he quits Zed8 (or you quit it with his yes). Then run `zed8/bundle.sh`, which copies the app to `/Applications/Zed8.app`, and open it with `open -a Zed8`.
11. Report: the number of upstream commits, the conflicts and how you resolved them, the fixes in the crate, the test results, and whether Zed8 is installed.

## When the merge goes wrong

The merge is one commit. To undo it before the push, run `git reset --hard ORIG_HEAD` with Roman's yes. After the push, revert it with `git revert -m 1 <merge commit>`.
