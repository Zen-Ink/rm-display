#!/usr/bin/env bash
set -euo pipefail

HERE=$(cd "$(dirname "$0")/.." && pwd)
SCRIPT_PATH="$HERE/scripts/takeover.sh"
export TMPDIR="$HERE/.cache/tmp"
mkdir -p "$TMPDIR"

if [[ "${1:-}" != "--inside-systemd-unit" ]]; then
    exec systemd-run --wait --collect --unit=rm-display-takeover \
        --property="ExecStopPost=-/bin/systemctl start xochitl" \
        /bin/bash "$SCRIPT_PATH" --inside-systemd-unit "$@"
fi
shift

restore_xochitl() {
    systemctl start xochitl
}

child=
terminate_child() {
    if [[ -n "$child" ]]; then
        kill -TERM "$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
    fi
}

trap 'terminate_child; restore_xochitl' EXIT INT TERM
systemctl stop xochitl

LD_LIBRARY_PATH="$HERE:/usr/lib/plugins/scenegraph${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    "$HERE/rm-display-receiver" "$@" &
child=$!
wait "$child"
status=$?
child=
exit "$status"
