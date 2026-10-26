# One VM or distro per Project is the isolation boundary

Coolify isolates resources within a shared Docker network on a host. A shared kernel and network is not an isolation boundary between projects, and the product promise is that one Project cannot see or break another. We decided each Project gets its own runtime: an Apple container microVM on macOS 26+/Apple silicon, a Lima VM on other macOS, and a WSL distro on Windows.

On macOS this is a real VM boundary. On Windows, measured WSL2 behaviour is namespace-level only: separate rootfs, PID/mount/user namespaces, and a cgroup subtree over a shared kernel and network namespace. The daemon therefore owns a machine-wide port registry with leased loopback ports, and the product language says plainly what Windows isolates and what it does not. Shared-runtime mode remains an explicit opt-out. The cost is cold-start time and RAM per running Project; the benefit is independent teardown and, on macOS, a kernel boundary.

Consequences: the Backend contract must express lifecycle, not just container operations; idle Projects need autosuspend; the UI must show per-Project resource cost; Windows port publishing must be leased from the registry.
