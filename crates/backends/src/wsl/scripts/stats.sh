#!/bin/sh
# agentify-stats — what the machine can prove about this Environment, right now.
#
# Run inside the Project's distro (`sh -s`), so every figure is measured from the
# substrate rather than guessed at by the daemon. Printed as `key=value` lines;
# the adapter parses them. A figure the machine cannot produce is **omitted**:
# nothing here invents a number.
#
#   cpu_percent=<f>     busy share of every online CPU, so 100 means "all of one
#                       CPU", the same convention `docker stats` uses
#   memory_bytes=<n>    MemTotal - MemAvailable: what this distro is using
#   disk_bytes=<n>      bytes used on the distro's own filesystem
#   container=<name>|<cpu>%|<used> / <limit>   one per running container, only
#                       when the engine is present and has any
set -u

cpus=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)
case "$cpus" in
  '' | *[!0-9]*) cpus=1 ;;
esac
[ "$cpus" -ge 1 ] 2>/dev/null || cpus=1

cpu_sample() {
  awk '/^cpu /{print ($2 + $3 + $4), ($5 + $6 + $7 + $8)}' /proc/stat 2>/dev/null
}

# Two samples 200 ms apart: an instantaneous figure would be a lie, and a stored
# one would be an older Environment's. A WSL distro has its own VM, so its
# /proc/stat is its own CPU time.
set -- $(cpu_sample)
busy0=${1:-0}
idle0=${2:-0}
sleep 0.2
set -- $(cpu_sample)
busy1=${1:-0}
idle1=${2:-0}
dbusy=$((busy1 - busy0))
didle=$((idle1 - idle0))
dtotal=$((dbusy + didle))
if [ "$dtotal" -gt 0 ]; then
  awk -v busy="$dbusy" -v total="$dtotal" -v cpus="$cpus" \
    'BEGIN { printf "cpu_percent=%.2f\n", (busy / total) * 100 * cpus }'
else
  echo "cpu_percent=0.00"
fi

awk '/^MemTotal:/ { total = $2 }
     /^MemAvailable:/ { available = $2 }
     /^MemFree:/ { free = $2 }
     /^Buffers:/ { buffers = $2 }
     /^Cached:/ { cached = $2 }
     END {
       if (total == 0) exit;
       if (available == 0) { available = free + buffers + cached }
       used = total - available
       if (used < 0) { used = 0 }
       printf "memory_bytes=%d\n", used * 1024
     }' /proc/meminfo 2>/dev/null

if command -v df >/dev/null 2>&1; then
  df -B1 / 2>/dev/null | awk 'NR > 1 { printf "disk_bytes=%d\n", $3; exit }'
fi

# Per-Resource figures need a substrate that knows about Resources: the container
# engine. Without it, the environment-level numbers above are all there is, and
# none of them is extrapolated into a per-Resource one.
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  if [ -n "$(docker ps -q 2>/dev/null)" ]; then
    docker stats --no-stream --format 'container={{.Name}}|{{.CPUPerc}}|{{.MemUsage}}' 2>/dev/null
  fi
fi
