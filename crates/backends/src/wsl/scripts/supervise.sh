#!/bin/sh
# agentify-supervise — the foreground owner of one Project's runtime.
#
# The daemon starts this as `wsl.exe -d <distro> -- /usr/local/bin/agentify-supervise <port>...`
# and keeps that launcher alive for as long as the Environment runs, because a
# background process started by a one-shot `wsl.exe` is reaped when the launcher
# exits. Everything this Project needs is owned here, in the foreground:
#
#   * the container engine, if the golden template carries one;
#   * one listener on 127.0.0.1 for every leased port, so the daemon's health
#     probe over the Windows loopback reaches this Project and nothing else.
#
# Its stdout is a protocol read by the daemon (and nothing else):
#   agentify-supervise: ready <port>     a leased port is bound
#   agentify-supervise: fail <port> <why> a leased port could not be bound
#   agentify-supervise: ready            every leased port is bound
#   agentify-supervise: engine ready|unavailable
set -u

mkdir -p /run/agentify

# The engine is started here rather than by an init unit, so the distro needs no
# particular init system for workloads to come up. It is skipped entirely when
# the golden template has no engine, which is a supported configuration.
if [ -x /usr/bin/dockerd ]; then
  if /usr/bin/docker info >/dev/null 2>&1; then
    echo "agentify-supervise: engine ready"
  else
    if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
      systemctl start docker >/dev/null 2>&1 || true
    else
      /usr/bin/dockerd >/run/agentify/dockerd.log 2>&1 &
    fi
    i=0
    while [ "$i" -lt 80 ]; do
      if /usr/bin/docker info >/dev/null 2>&1; then
        break
      fi
      sleep 0.25
      i=$((i + 1))
    done
    if /usr/bin/docker info >/dev/null 2>&1; then
      echo "agentify-supervise: engine ready"
    else
      echo "agentify-supervise: engine unavailable"
    fi
  fi
fi

exec /usr/bin/python3 -u - "$@" <<'PY'
import socket
import sys
import time

ports = sorted({int(raw) for raw in sys.argv[1:] if raw.strip()})
listeners = []
for port in ports:
    handle = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    handle.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        handle.bind(("127.0.0.1", port))
        handle.listen(16)
        handle.setblocking(False)
    except OSError as error:
        reason = error.strerror or "bind failed"
        print("agentify-supervise: fail %d %s" % (port, reason), flush=True)
        sys.exit(3)
    listeners.append(handle)
    print("agentify-supervise: ready %d" % port, flush=True)

print("agentify-supervise: ready", flush=True)

# Whether a probe gets a greeting is the Resource's business, not the
# supervisor's: accept and close, and keep every lease bound until this process
# is stopped.
while True:
    for handle in listeners:
        try:
            connection, _ = handle.accept()
            connection.close()
        except BlockingIOError:
            pass
        except OSError:
            pass
    time.sleep(0.02)
PY
