#!/bin/bash
# Writes the golden files for the Rust tests from the Bash tool n8n-wt.
# The golden files are the independent source of truth: the Rust code must
# write the same files for the same inputs. Uses fake secrets only.
#
#   /bin/bash crates/n8n_worktree_manager/tests/golden/generate.sh [path to n8n-worktrees]
set -euo pipefail

OUT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
FIXTURES="$OUT/../fixtures"
TOOL="${1:-$HOME/workspace/n8n-worktrees}"
TMP="$(mktemp -d)"
TMP="$(cd "$TMP" && pwd -P)"
trap 'rm -rf "$TMP"' EXIT

# One configuration: name slot queue sandbox telemetry secrets(yes|no) base branch
generate() {
	local name="$1" slot="$2" queue="$3" sandbox="$4" telemetry="$5" secrets="$6" base="$7" branch="$8"
	local home="$TMP/$name"
	mkdir -p "$home/n8n/$name/n8n" "$OUT/$name"
	if [ "$secrets" = yes ]; then
		cp "$FIXTURES/secrets.env" "$home/secrets.env"
	fi
	git -C "$home/n8n/$name/n8n" init --quiet
	(
		export N8N_WT_HOME="$home" N8N_WT_NO_MAIN=1
		# shellcheck source=/dev/null
		source "$TOOL/bin/n8n-wt"
		local dir="$home/n8n/$name"
		compute_ports "$slot"
		write_env_sh "$dir" "$queue" "$sandbox" "$name" "$telemetry"
		write_meta "$dir" "$name" "$slot" "$base" "$queue" "$sandbox" "/src/n8n" "$branch" "$telemetry"
		write_zed_files "$dir" "$name"
		{
			echo "COMPOSE_PROJECT_NAME=n8n-wt-$name"
			echo "POSTGRES_PORT=$POSTGRES_PORT"
			echo "REDIS_PORT=$REDIS_PORT"
			echo "MAILPIT_SMTP_PORT=$MAILPIT_SMTP_PORT"
			echo "MAILPIT_HTTP_PORT=$MAILPIT_HTTP_PORT"
			echo "PGWEB_PORT=$PGWEB_PORT"
		} > "$dir/.env"
	)
	local dir="$home/n8n/$name"
	# The state folder is a temp folder. The golden files use /wt instead.
	sed "s|$home|/wt|g" "$dir/env.sh" > "$OUT/$name/env.sh"
	sed "s|$home|/wt|g" "$dir/.env" > "$OUT/$name/compose.env"
	sed "s|$home|/wt|g" "$dir/n8n/.zed/tasks.json" > "$OUT/$name/tasks.json"
	sed "s|$home|/wt|g" "$dir/n8n/.zed/settings.json" > "$OUT/$name/settings.json"
	jq '.created_at = "2026-01-01T00:00:00Z"' "$dir/meta.json" > "$OUT/$name/meta.json"
}

generate alpha 3 false true true yes master alpha
generate beta 0 false false false no master beta
generate gamma 7 true true false yes "" feature/gamma-existing

# The editor badge: the patched vite config and the plugin file.
badge="$TMP/badge"
mkdir -p "$badge/n8n/badge/n8n/packages/frontend/editor-ui/vite"
cp "$FIXTURES/vite.config.mts" "$badge/n8n/badge/n8n/packages/frontend/editor-ui/vite.config.mts"
(
	cd "$badge/n8n/badge/n8n" && git init --quiet && git add -A && git -c user.email=t@t -c user.name=t commit --quiet -m init
	export N8N_WT_HOME="$badge" N8N_WT_NO_MAIN=1
	# shellcheck source=/dev/null
	source "$TOOL/bin/n8n-wt"
	apply_worktree_badge_hack "$badge/n8n/badge" badge >/dev/null
)
mkdir -p "$OUT/badge"
cp "$badge/n8n/badge/n8n/packages/frontend/editor-ui/vite.config.mts" "$OUT/badge/vite.config.mts"
echo "Golden files written to $OUT"
