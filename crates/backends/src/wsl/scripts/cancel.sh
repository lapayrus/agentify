#!/bin/sh
# agentify-cancel — stop every process one exec started, and nothing else.
#
# The daemon runs this in the background when an exec is cancelled or times out:
#
#   wsl.exe -d <distro> -- sh -c '<this script>'
#
# Two independent handles on the same command, because either can be the one that
# is available:
#
#   1. the process group the wrapper created, recorded in
#      /run/agentify/exec/<marker>.pgid — one `kill -<group>` reaches every child
#      that stayed in it;
#   2. the marker `AGENTIFY_EXEC_MARKER=<marker>` in the environment of every
#      process the command started — a sweep of /proc finds the ones that left
#      the group, and finds all of them if the pid file was never written.
#
# `pkill` is used for the group when it is installed, and the plain shell builtin
# is the fallback: the golden template does not guarantee procps.
set -u

marker='__MARKER__'
pidfile="/run/agentify/exec/$marker.pgid"

signal_group() {
  [ -f "$pidfile" ] || return 0
  pgid=$(cat "$pidfile" 2>/dev/null)
  [ -n "$pgid" ] || return 0
  if command -v pkill >/dev/null 2>&1; then
    pkill -"$1" -g "$pgid" 2>/dev/null
  fi
  kill -"$1" -"$pgid" 2>/dev/null
  return 0
}

signal_marked() {
  for dir in /proc/[0-9]*; do
    [ -r "$dir/environ" ] || continue
    if tr '\0' '\n' <"$dir/environ" 2>/dev/null | grep -qx "AGENTIFY_EXEC_MARKER=$marker"; then
      kill -"$1" "${dir#/proc/}" 2>/dev/null
    fi
  done
  return 0
}

signal_group TERM
signal_marked TERM
sleep 0.1
signal_group KILL
signal_marked KILL
rm -f "$pidfile"
