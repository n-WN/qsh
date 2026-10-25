#!/bin/sh
# Build qsh on a Linux server from this source tree and install it.
# Usage: scripts/deploy.sh user@host [ssh options...]
# Example: scripts/deploy.sh root@server.example.com -i ~/.ssh/key
set -eu
host=$1; shift
dir=$(cd "$(dirname "$0")/.." && pwd)
rsync -az --delete --exclude target --exclude .git -e "ssh $*" "$dir/" "$host:src/qsh/"
ssh "$@" "$host" 'set -e; cd src/qsh && cargo build --release 2>&1 | grep -E "^(warning|error)" -A5 || true
  if [ -w /usr/local/bin ]; then dst=/usr/local/bin/qsh; else mkdir -p ~/.local/bin; dst=~/.local/bin/qsh; fi
  install -m 755 target/release/qsh "$dst.new" && mv -f "$dst.new" "$dst" && echo "installed $dst"'
