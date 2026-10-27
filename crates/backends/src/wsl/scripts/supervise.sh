#!/bin/sh
# agentify-supervise — the foreground owner of one Project's runtime.
#
# The daemon starts this as
#
#     wsl.exe -d <distro> -- /usr/local/bin/agentify-supervise [--compose <file>] [--map <svc:guest:host>]... [<port>]...
#
# and keeps that launcher alive for as long as the Environment runs, because a
# background process started by a one-shot `wsl.exe` is reaped when the launcher
# exits. Everything this Project needs is owned here, in the foreground:
#
#   * the container engine, if the golden template carries one;
#   * the Project's workloads, started with `docker compose up -d --build` from
#     the Compose file the daemon generated (`--compose`);
#   * one listener on 127.0.0.1 for every leased port, so the daemon's health
#     probe over the Windows loopback reaches this Project and nothing else. In
#     workload mode that listener is a relay to the container, and it exists only
#     while the container answers — a lease that is bound while nothing is behind
#     it would make the daemon's probe lie.
#
# Two modes, one script, because both must hold a foreground process:
#
#   listener-only  `<port>...` with no `--compose`: bind and hold the leases. This
#                  is the mode a Backend conformance run uses, where there are no
#                  workloads to build.
#   workloads      `--compose <file> --map svc:guest:host ...`: start the
#                  workloads, then bind and relay each lease.
#
# Its stdout is a protocol read by the daemon (and nothing else):
#   agentify-supervise: ready <port>     a leased port is bound (and relaying)
#   agentify-supervise: fail <port> <why> a leased port could not be bound
#   agentify-supervise: ready            every leased port is bound
#   agentify-supervise: engine ready|unavailable
#   agentify-supervise: lost <port>      the workload behind a bound lease stopped
#   agentify-supervise: recovered <port> it came back
set -u

COMPOSE=""
MAPS=""
PORTS=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --compose)
      COMPOSE="$2"
      shift 2
      ;;
    --map)
      MAPS="$MAPS $2"
      shift 2
      ;;
    *)
      PORTS="$PORTS $1"
      shift
      ;;
  esac
done

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
    while [ "$i" -lt 120 ]; do
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

# Compose comes in two shapes on Linux, and a distro may carry either. One
# helper, decided once, so nothing else in this script has to care.
COMPOSE_CMD=""
if [ -n "$COMPOSE" ]; then
  if /usr/bin/docker compose version >/dev/null 2>&1; then
    COMPOSE_CMD="docker compose"
  elif command -v docker-compose >/dev/null 2>&1; then
    COMPOSE_CMD="docker-compose"
  else
    echo "agentify-supervise: compose unavailable (neither \`docker compose\` nor \`docker-compose\` is installed)"
    exit 4
  fi

  if [ ! -f "$COMPOSE" ]; then
    echo "agentify-supervise: no Compose file at $COMPOSE"
    exit 4
  fi

  # The output of the build and the up is the Environment's log: an Agent that
  # asks why the start was slow should be able to read it. It goes to stderr so
  # it can never be mistaken for a protocol line.
  echo "agentify-supervise: compose up ($COMPOSE_CMD -f $COMPOSE up -d --build)"
  if ! $COMPOSE_CMD -f "$COMPOSE" up -d --build 1>&2; then
    echo "agentify-supervise: compose up failed"
    $COMPOSE_CMD -f "$COMPOSE" ps 1>&2 || true
    exit 4
  fi
  echo "agentify-supervise: compose up finished"
  # Compose publishes every lease on 127.0.0.1 inside this distro; there is no
  # relay to run. Hold the foreground launcher so the workloads stay owned by
  # this Project, and let the daemon probe the published ports.
  echo "agentify-supervise: ready"
  while true; do sleep 3600; done
fi

COMPOSE_CMD="$COMPOSE_CMD" \
  AGENTIFY_COMPOSE="$COMPOSE" \
  AGENTIFY_MAPS="$MAPS" \
  AGENTIFY_PORTS="$PORTS" \
  exec /usr/bin/python3 -u - <<'PY'
"""Hold a Project's leases, relaying each to its workload.

One thread per connection, one socket per lease. A lease is bound only while its
target answers: binding it unconditionally would make the daemon's TCP probe
succeed against nothing, which is exactly the kind of lie this adapter refuses.
"""

import os
import select
import socket
import subprocess
import sys
import threading
import time

MARKER = "agentify-supervise:"
# How long the supervisor waits for a workload's port before it gives up and
# says so. A cold boot builds an image and initialises a database; a warm start
# is seconds.
READY_BUDGET = float(os.environ.get("AGENTIFY_SUPERVISE_READY_SECONDS") or 600)
# How often a bound lease is checked against the workload behind it.
CHECK_EVERY = 2.0


def say(line):
    print("%s %s" % (MARKER, line), flush=True)


def compose_command():
    raw = (os.environ.get("AGENTIFY_COMPOSE_CMD") or "").strip()
    return raw.split() if raw else []


def run(argv, timeout=60):
    try:
        finished = subprocess.run(
            argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=timeout
        )
    except Exception:
        return None
    if finished.returncode != 0:
        return None
    return finished.stdout.decode("utf-8", "replace")


class Workload:
    """One Resource's container, found through Compose rather than published."""

    def __init__(self, service, guest_port):
        self.service = service
        self.guest_port = guest_port

    def address(self):
        """The container's IP on the Compose network, or None if it is gone."""
        argv = compose_command()
        if not argv:
            return None
        identifiers = run(argv + ["ps", "-q", self.service])
        if not identifiers or not identifiers.strip():
            return None
        container = identifiers.split()[0]
        address = run(
            [
                "docker",
                "inspect",
                "-f",
                "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
                container,
            ]
        )
        if not address:
            return None
        address = address.strip().splitlines()[0].strip() if address.strip() else ""
        return address or None

    def reachable(self):
        address = self.address()
        if not address:
            return False
        return probe(address, self.guest_port)


def probe(host, port, timeout=1.0):
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def pump(source, sink):
    try:
        while True:
            data = source.recv(65536)
            if not data:
                break
            sink.sendall(data)
    except OSError:
        pass
    finally:
        for handle in (source, sink):
            try:
                handle.close()
            except OSError:
                pass


class Lease:
    """One leased host port, bound while its workload answers."""

    def __init__(self, port, workload):
        self.port = port
        self.workload = workload
        self.listener = None
        self.announced_lost = False

    def bind(self):
        handle = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        handle.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            handle.bind(("127.0.0.1", self.port))
            handle.listen(64)
            handle.setblocking(False)
        except OSError as error:
            handle.close()
            say("fail %d %s" % (self.port, error.strerror or "bind failed"))
            sys.exit(3)
        self.listener = handle

    def close(self):
        if self.listener is not None:
            try:
                self.listener.close()
            except OSError:
                pass
            self.listener = None

    def accept_all(self):
        if self.listener is None:
            return
        while True:
            try:
                client, _ = self.listener.accept()
            except BlockingIOError:
                return
            except OSError:
                return
            threading.Thread(
                target=self.serve, args=(client,), daemon=True
            ).start()

    def serve(self, client):
        address = self.workload.address()
        if not address:
            try:
                client.close()
            except OSError:
                pass
            return
        try:
            target = socket.create_connection(
                (address, self.workload.guest_port), timeout=5.0
            )
            target.settimeout(None)
            client.settimeout(None)
        except OSError:
            try:
                client.close()
            except OSError:
                pass
            return
        threading.Thread(target=pump, args=(client, target), daemon=True).start()
        threading.Thread(target=pump, args=(target, client), daemon=True).start()


def listener_only(port):
    """Bind `port` and answer every probe with nothing: no workload exists."""
    handle = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    handle.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        handle.bind(("127.0.0.1", port))
        handle.listen(16)
        handle.setblocking(False)
    except OSError as error:
        say("fail %d %s" % (port, error.strerror or "bind failed"))
        sys.exit(3)
    return handle


def main():
    maps = []
    for token in (os.environ.get("AGENTIFY_MAPS") or "").split():
        parts = token.split(":")
        if len(parts) != 3:
            say("fail 0 malformed --map %r" % token)
            sys.exit(3)
        service, guest, host = parts
        maps.append((service, int(guest), int(host)))
    bare = [int(port) for port in (os.environ.get("AGENTIFY_PORTS") or "").split()]

    if not maps:
        # Listener-only: accept and close, and keep every lease bound until this
        # process is stopped. Whether a probe gets a greeting is the Resource's
        # business, not the supervisor's.
        listeners = []
        for port in sorted(set(bare)):
            listeners.append(listener_only(port))
            say("ready %d" % port)
        say("ready")
        while True:
            select.select(listeners, [], [], 1.0)
            for handle in listeners:
                while True:
                    try:
                        connection, _ = handle.accept()
                        connection.close()
                    except BlockingIOError:
                        break
                    except OSError:
                        break
        return

    leases = [Lease(host, Workload(service, guest)) for service, guest, host in maps]

    deadline = time.monotonic() + READY_BUDGET
    pending = list(leases)
    while pending:
        for lease in list(pending):
            if lease.workload.reachable():
                lease.bind()
                pending.remove(lease)
                say("ready %d" % lease.port)
        if pending:
            if time.monotonic() > deadline:
                for lease in pending:
                    say(
                        "fail %d the workload %s never answered on port %d"
                        % (lease.port, lease.workload.service, lease.workload.guest_port)
                    )
                sys.exit(3)
            time.sleep(0.5)

    # Every workload answers and every lease is bound: this is the line the
    # daemon treats as "the Environment is up".
    say("ready")

    checked_at = time.monotonic()
    while True:
        for lease in leases:
            lease.accept_all()
        if time.monotonic() - checked_at >= CHECK_EVERY:
            checked_at = time.monotonic()
            for lease in leases:
                alive = lease.workload.reachable()
                if lease.listener is not None and not alive:
                    # The workload behind this lease is gone: stop answering so
                    # the daemon's probe reports the truth.
                    lease.close()
                    lease.announced_lost = True
                    say("lost %d" % lease.port)
                elif lease.listener is None and alive:
                    lease.bind()
                    lease.announced_lost = False
                    say("recovered %d" % lease.port)
        time.sleep(0.02)


main()
PY
