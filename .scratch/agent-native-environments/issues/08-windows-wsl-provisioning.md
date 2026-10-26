# Research: Windows per-Project WSL provisioning

Type: research
Status: resolved
Blocked by: none

## Question

Can the daemon provision a per-Project WSL2 distro without administrator rights after the one-time WSL enable?

Verify `wsl --import` behaviour, rootfs sourcing and redistribution rights, systemd and container engine choice, port publishing across multiple distros, host folder sharing and inotify behaviour, disk growth and cleanup, exec and streaming, cold-start cost, and failure modes on Windows Home.

Findings go to [`research/windows-wsl-per-project.md`](../../../research/windows-wsl-per-project.md).

## Answer

`wsl --import <name> <dir> <rootfs.tar> --version 2` works without admin once WSL is enabled; measured 0.71 s for a 3.5 MB rootfs, 76 MiB VHD floor, ~450 ms warm start, 122 ms exec. First enablement needs elevation and a reboot. Ubuntu base rootfs is 28.6 MiB, Alpine 3.3 MiB; bundle neither, download and verify.

The load-bearing negative: **all distros share one utility VM, kernel, and network namespace.** Measured cross-distro listener visibility and port bind conflicts. That breaks ADR-0001's kernel-boundary claim on Windows and makes port collisions structural, so the daemon needs a machine-wide port registry and leased loopback ports.

Other constraints: rootful dockerd plus systemd is the least fragile engine; rootless podman breaks privileged, sysctl, and bind-mount chown. Keep project files and database data on ext4, not `/mnt/c`; inotify cannot see Windows-side changes. `wsl -t` per distro, never `wsl --shutdown`. No PTY off-console, and background processes die when the one-shot launcher exits, so each Project needs a foreground supervisor. Sparse VHD compaction is disabled without an unsafe flag.

Full detail: [research/windows-wsl-per-project.md](../../../research/windows-wsl-per-project.md).
