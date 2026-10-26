# Decide the Windows isolation boundary

Type: grilling
Status: resolved
Blocked by: none

## Question

Windows WSL distros share one utility VM, kernel, and network namespace (ticket 08). What do we promise on Windows?

Option A — namespace-level isolation: one distro per Project for filesystem and toolchain isolation, one Docker network per Project, cgroup limits per distro, and a daemon-owned machine-wide port registry with leased loopback ports. Fast (~450 ms), low RAM, works on Home, but a kernel exploit or a host-network mistake crosses Projects.

Option B — VM-grade isolation: a Hyper-V VM per Project on Pro/Enterprise, with A as the Home fallback. A true boundary, but two Windows backends, heavy cold start and RAM, and a different promise on Home.

Option C — defer: ship A, keep B as an opt-in Backend behind the same contract, revisit when a user need forces it.

Recommendation: A for v0, and say the weaker boundary plainly in the product language. Only pay for B if a concrete requirement appears.

## Answer

Option A. Windows v0 ships namespace-level isolation: one WSL distro per Project for filesystem and toolchain isolation, one Docker network per Project, cgroup limits per distro, and a daemon-owned machine-wide port registry with leased loopback ports.

The product language must say that Windows Projects share the kernel and network namespace with the host and each other, and that the boundary is filesystem, process, and resource isolation rather than a kernel boundary. macOS keeps its VM boundary.

VM-grade Hyper-V isolation is out of scope for this effort and returns only as a fresh effort if a concrete requirement appears.
