#!/bin/sh
# Start or stop the network lab on a Linux machine (as root): the emulator
# (netem.py), and an sshd on the server side of it (10.99.1.1:2222). Logins
# to it get QSH_HOME=DIR/home/.qsh, so the lab's qsh daemon, sessions and
# devices are separate from those of the real user. Use `qsh --server
# DIR/qsh` (a copy of the binary), so that nothing is installed in ~/.qsh.
# Nothing outside DIR changes, except /dev/net/tun (made if it is missing)
# and the two TUN devices, which go away with the emulator.
#
# Usage: tests/netlab/lab.sh start|stop [DIR]   (default DIR: /tmp/qshlab)
# Clients connect with:
#   ssh -p 2222 -i DIR/key -o StrictHostKeyChecking=no \
#       -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@10.99.0.2
set -eu
cmd=$1
L=${2:-/tmp/qshlab}
here=$(cd "$(dirname "$0")" && pwd)

stop() {
    [ -f "$L/sshd.pid" ] && kill "$(cat "$L/sshd.pid")" 2>/dev/null || true
    [ -f "$L/netem.pid" ] && kill "$(cat "$L/netem.pid")" 2>/dev/null || true
    # The lab's qsh daemon.
    pkill -f "^$L/qsh daemon" 2>/dev/null || true
    rm -f "$L/sshd.pid" "$L/netem.pid"
}

case $cmd in
start)
    stop
    mkdir -p "$L/home" "$L/chome"
    [ -f "$L/hostkey" ] || ssh-keygen -q -t ed25519 -N '' -f "$L/hostkey"
    [ -f "$L/key" ] || ssh-keygen -q -t ed25519 -N '' -f "$L/key"
    echo "environment=\"QSH_HOME=$L/home/.qsh\" $(cat "$L/key.pub")" >"$L/authorized_keys"
    cat >"$L/sshd_config" <<EOF
ListenAddress 10.99.1.1:2222
HostKey $L/hostkey
AuthorizedKeysFile $L/authorized_keys
PermitRootLogin prohibit-password
PermitUserEnvironment yes
PasswordAuthentication no
KbdInteractiveAuthentication no
StrictModes no
PidFile $L/sshd.pid
EOF
    [ -f "$L/profile.json" ] || echo '{}' >"$L/profile.json"
    nohup python3 "$here/netem.py" "$L/profile.json" "$L/stats.json" >"$L/netem.log" 2>&1 &
    echo $! >"$L/netem.pid"
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        grep -q ready "$L/netem.log" 2>/dev/null && break
        sleep 0.2
    done
    grep -q ready "$L/netem.log" || { cat "$L/netem.log"; exit 1; }
    mkdir -p /run/sshd
    /usr/sbin/sshd -f "$L/sshd_config"
    echo "lab ready: $L"
    ;;
stop)
    stop
    echo "lab stopped"
    ;;
*)
    echo "usage: $0 start|stop [DIR]" >&2
    exit 2
    ;;
esac
