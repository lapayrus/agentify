#!/bin/sh
# agentify-prepare — the recipe for the golden template.
#
# Run once, as root, inside a freshly imported Ubuntu base distro, immediately
# before that distro is exported and cached. Every Project's distro is imported
# from the resulting tar, so the slow work (apt) happens once per machine rather
# than once per Project.
#
# Two knobs, both set by the daemon from its configuration:
#   AGENTIFY_ENGINE=1   install the container engine (docker.io) — best effort,
#                       because a distro without an engine is still usable
#   AGENTIFY_SYSTEMD=1  boot with systemd (`[boot] systemd=true`)
set -eu

export DEBIAN_FRONTEND=noninteractive

apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  ca-certificates \
  systemd \
  systemd-sysv \
  dbus \
  python3-minimal \
  iptables \
  iproute2

mkdir -p /etc/agentify
: > /etc/agentify/engine

if [ "${AGENTIFY_ENGINE:-0}" = "1" ]; then
  if apt-get install -y -qq --no-install-recommends docker.io; then
    printf 'docker.io\n' > /etc/agentify/engine
    # The unit is enabled but not started here: the supervisor starts it, so a
    # distro without systemd behaves identically.
    systemctl enable docker.service >/dev/null 2>&1 || true
  else
    echo "agentify-prepare: the container engine could not be installed; this distro runs without one" >&2
  fi

  # Compose is what runs the Project's Resources. Two package names ship it, and
  # a distro with either is fine: the supervisor asks `docker compose` first and
  # falls back to the standalone binary.
  if ! apt-get install -y -qq --no-install-recommends docker-compose-v2; then
    echo "agentify-prepare: docker-compose-v2 is not in this archive; trying the standalone binary" >&2
    apt-get install -y -qq --no-install-recommends docker-compose || \
      echo "agentify-prepare: no Compose is installed; this distro can hold leases but not run workloads" >&2
  fi
fi

# A WSL distro imported from a tar has no launcher executable, so the only way
# to choose its init is /etc/wsl.conf.
if [ "${AGENTIFY_SYSTEMD:-0}" = "1" ]; then
  printf '[boot]\nsystemd=true\n\n[user]\ndefault=root\n' > /etc/wsl.conf
else
  printf '[boot]\nsystemd=false\n\n[user]\ndefault=root\n' > /etc/wsl.conf
fi

apt-get clean
rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb
