# Zed8

Zed8 is Roman's personal build of [Zed](https://github.com/zed-industries/zed). It adds these features to Zed:

- A Review panel for Plannotator: guided code review of a PR, and spec review, inside Zed. See `BUILD_SPEC.md`.
- Threads in the threads sidebar grouped by worktree.

Later, it will also hold plugins for n8n work.

## Install

```sh
zed8/bundle.sh
```

The script builds `Zed8.app` and starts `zed8/install.sh`. The install script waits until you quit Zed8, puts the new app in `/Applications`, opens it, and links the `zed8` command in `~/.local/bin`. The last build stays at `/Applications/Zed8.app.previous`. To go back to it, quit Zed8 and move it back in place. Zed8 uses your real Zed settings and projects. Quit official Zed before you open Zed8. Both apps cannot use the same data at the same time.

On the first start, macOS asks again for the permissions that you gave to Zed, such as Screen Recording and Accessibility.

## Develop

`zed8/run.sh <worktree>` starts a development build with its own data in `~/workspace/zed-dev-data`. It can run beside Zed8.

## Feedback

Open an issue in [rmakowiak/zed8](https://github.com/rmakowiak/zed8/issues) with the `feedback` label. Claude reads the open issues and works through them.

## Update from upstream

Ask Claude Code to run the `zed8-update` skill (`zed8/skills/zed8-update/SKILL.md`). It merges upstream Zed, fixes conflicts with the help of `UPSTREAM.md`, builds, tests and pushes.

## License

Zed is GPL-3. Zed8 keeps the licenses of Zed. The new crate `crates/plannotator_review` is GPL-3 too.
