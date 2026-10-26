# Research: macOS container backend

Type: research
Status: resolved
Blocked by: none

## Question

Which macOS Backend do we ship, and how does the daemon drive it?

Compare embedding Apple's `containerization` framework against shipping the `container` CLI, cover Lima as the fallback for older macOS and Intel, and report per-Project VM lifecycle, port publishing, file sharing, licensing and notarization.

State whether Intel support is worth the cost for v0. Findings go to [`research/macos-container-backend.md`](../../../research/macos-container-backend.md).

## Answer

Matrix: Apple `container` on macOS 26+/Apple silicon; Lima with `vmType: vz` on macOS 14–15 Apple silicon; Intel is out of v0. Apple container gives one VM per container over Virtualization.framework, with a gRPC/vsock API and a supported CLI JSON surface; the Swift package is pre-1.0, so drive the CLI with pinned majors.

Capacity is the constraint: default 1 GiB and 4 vCPU per container, and freed guest memory is never returned to the host, so N running Projects means N resident VMs. Volumes are sparse ext4 images. Per-container host-reachable IPs need the restricted `com.apple.vm.networking` entitlement, so file for it early and rely on loopback publishing until granted. DNS requires a `sudo` resolver entry.

Lima: per-Project VM, ~30 s cold start, 38 MB binary, Apache-2.0, virtiofs mounts, snapshots too thin to build restore on. Drive both backends through CLI+JSON adapters returning a capability descriptor; order teardown containers → machines → volumes/networks. Auto-start via SMAppService; uninstall must stop the agent, delete containers, machines, volumes, networks, and config.

Full detail: [research/macos-container-backend.md](../../../research/macos-container-backend.md).
