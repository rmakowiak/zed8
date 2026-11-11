#!/bin/sh
# Starts the fork Zed on a worktree, without the environment of a Claude Code session.
cd "$(dirname "$0")/.." || exit 1
for var in $(env | grep -Eo "^(CLAUDE[A-Z_]*|AI_AGENT)="  | tr -d =); do unset "$var"; done
exec ./target/release-fast/zed --user-data-dir "$HOME/workspace/zed-dev-data" "${1:-$HOME/workspace/n8n-worktrees/n8n/context-433/n8n}"
