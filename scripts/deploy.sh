#!/bin/sh
# Build qsh on a server from this source tree and make it the current build
# (~/.qsh/current), as the automatic install of the client does. A running
# daemon changes to it, with its sessions, when the next client connects.
# Usage: scripts/deploy.sh user@host [ssh options...]
# Example: scripts/deploy.sh root@server.example.com -i ~/.ssh/key
set -eu
host=$1; shift
dir=$(cd "$(dirname "$0")/.." && pwd)
rsync -az --delete --exclude target --exclude .git -e "ssh $*" "$dir/" "$host:src/qsh/"
ssh "$@" "$host" 'set -e; cd src/qsh
  if ! cargo build --release >build.log 2>&1; then tail -n 30 build.log; exit 1; fi
  grep -E "^(warning|error)" -A5 build.log || true
  id=$(target/release/qsh version | awk "{print \$5}" | cut -d: -f1)
  v="$HOME/.qsh/versions/$id"
  mkdir -p "$v"
  install -m 755 target/release/qsh "$v/qsh.new" && mv -f "$v/qsh.new" "$v/qsh"
  ln -sfn "versions/$id/qsh" "$HOME/.qsh/current.new" && mv -f "$HOME/.qsh/current.new" "$HOME/.qsh/current"
  echo "installed $v/qsh"'
