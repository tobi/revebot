#!/usr/bin/env bash
# Run a Makefile spec target on another machine, inside a named herdr
# workspace there so it can be watched with `herdr --remote <host>`.
#
#   scripts/spec-remote.sh gb300:~/src/tries/revebot spec-full [label]
#
# rsyncs this tree (minus target/ and .git/) to host:dir, ensures tla-checker is
# installed there, creates (or reuses) a herdr workspace labelled `label`
# (default: revebot-spec) on the remote server, and runs `make <target>` in its
# root pane. Without a running remote herdr server it falls back to plain ssh.
set -euo pipefail

remote=${1:?host:dir}
target=${2:-spec}
label=${3:-revebot-spec}
host=${remote%%:*}
dir=${remote#*:}

here=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck disable=SC2029  # $dir may contain ~, which the remote shell expands
ssh "$host" "mkdir -p $dir"
rsync -az --delete --exclude target --exclude .git "$here/" "$remote/"

# Everything after this runs on the remote.
ssh "$host" bash -s -- "$dir" "$target" "$label" <<'REMOTE'
set -euo pipefail
dir=$1; target=$2; label=$3
export PATH="$HOME/.local/share/mise/shims:$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
command -v tla >/dev/null || cargo install tla-checker@0.6.11 --bin tla

if ! command -v herdr >/dev/null || ! herdr status server 2>/dev/null | grep -q '^status: running'; then
  echo "no herdr server on $(hostname); running inline" >&2
  cd "$dir" && exec make "$target" TLA=tla
fi

# Reuse the labelled workspace when it exists, otherwise create it.
ws=$(herdr workspace list | jq -r --arg l "$label" '.result.workspaces[] | select(.label == $l) | .workspace_id' | head -1)
if [ -z "$ws" ]; then
  created=$(herdr workspace create --cwd "$dir" --label "$label" --no-focus)
  ws=$(jq -er '.result.workspace.workspace_id' <<<"$created")
  pane=$(jq -er '.result.root_pane.pane_id' <<<"$created")
  sleep 1 # the interactive shell needs a moment
else
  pane=$(herdr pane list --workspace "$ws" | jq -er '.result.panes[0].pane_id')
fi

log="$dir/target/spec-$target.log"
mkdir -p "$dir/target"
herdr pane run "$pane" "cd $dir && export PATH=\"\$HOME/.cargo/bin:\$PATH\" && make $target TLA=tla 2>&1 | tee $log; rc=\${PIPESTATUS[0]}; printf 'HERDR_DONE_%s\n' \"\$rc\""
echo "running make $target in herdr workspace '$label' ($ws), pane $pane on $(hostname); log: $log" >&2
herdr pane wait-output "$pane" --regex 'HERDR_DONE_[0-9]+' --source recent-unwrapped --lines 40 --timeout 86400000 >/dev/null
rc=$(herdr pane read "$pane" --source recent-unwrapped --lines 40 | grep -o 'HERDR_DONE_[0-9]*' | tail -1 | sed 's/HERDR_DONE_//')
grep -v 'states explored |' "$log" || true
exit "${rc:-1}"
REMOTE
