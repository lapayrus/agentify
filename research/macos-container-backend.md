# macOS container backend — research findings

Ticket: choose and specify the macOS backend for per-project, VM-grade isolated dev environments
(second platform after Windows). Constraints taken as given: containers are the substrate; per-project
VM-grade isolation is the goal; Docker Desktop and OrbStack are not redistributable; only Apache-2.0/MIT
components may ship.

All claims below are from primary sources (Apple docs, `apple/container` + `apple/containerization`
repository files and release metadata, `lima-vm/lima` docs and release metadata). Facts are as of
`apple/container` 1.6.1 (2026-10-23), `apple/containerization` 0.52.x, Lima 2.2.1 (2026-10-03).

---

## 1. Apple `container` / Containerization: what you actually get

### 1.1 Isolation unit and what is inside it

`container` runs **one lightweight VM per container** ("Using the open source Containerization package,
it runs a lightweight VM for each container that you create"), not a shared VM hosting many containers.
Properties claimed in the technical overview: full-VM isolation properties per container, a minimal
root filesystem and init to cut attack surface, and mount-only-what-you-need privacy (a shared VM must
mount everything up front). See `docs/technical-overview.md`.

Concretely per container:

* **VMM**: Virtualization.framework (`VZVirtualMachineManager`), Apple silicon only. The package
  abstracts the VMM behind `VirtualMachineManager` / `VirtualMachineInstance` protocols and now also
  ships a Linux/KVM backend (`CHVirtualMachineManager`, cloud-hypervisor + virtiofsd) — irrelevant for us
  on macOS but it proves the protocol boundary is stable and swappable.
* **Guest agent**: `vminitd` is PID 1 in the guest and serves a **gRPC API over vsock**; it configures the
  runtime environment, launches containerized processes, and returns I/O, signals and events. `vmexec`
  handles exec. Both are static Linux binaries built inside a Linux container at build time.
* **Kernel**: Containerization ships an "optimized Linux kernel configuration" (`kernel/`) for fast boot;
  kernels are **user-selectable per container** (`container run -k/--kernel`, `--kernel-arg`,
  `container system kernel set --recommended|--binary|--tar|--digest`). Upstream tests functionality from
  Linux 6.14.9; Kata's `vmlinux.container` is called out as a viable pre-built alternative. The default
  kernel is downloaded/installed by `container system start` (or `system kernel set`).
* **Rootfs**: OCI image unpacked into an **ext4 sparse block image** per container; `container commit`
  and `container export` snapshot that block device.

### 1.2 Boot time and memory overhead

Apple's claims, verbatim, are qualitative: containers "achieve sub-second start times using an optimized
Linux kernel configuration and a minimal root filesystem with a lightweight init system"
(`containerization/README.md`), and "boot times that are comparable to containers running in a shared VM"
(`container/docs/technical-overview.md`). There is no published ms-level benchmark from Apple; treat
"sub-second" as the design target for a small image, and measure with your own harness before committing
to a UX promise.

Memory:
* Default per-container VM budget is **1 GiB RAM / 4 vCPUs** (`--memory`, `--cpus` to override); the
  builder VM defaults to 2 GiB / 2 vCPUs.
* Overcommit is allowed; macOS schedules guest VM processes against physical resources like any other
  processes.
* **Known limitation**: Virtualization.framework implements only *partial* memory ballooning. Pages freed
  by guest processes are **not returned to the host**; Apple's own advice is to restart memory-heavy
  containers occasionally. For a "one VM per project" product this is the single most important capacity
  fact: N idle projects ≈ N VMs' worth of resident memory (bounded by what each actually touched), and
  stopping a project's containers is the only way to give memory back.

### 1.3 Volumes and persistence

* **Bind mounts** (`-v host:guest`, `--mount type=bind|virtiofs,...`): virtiofs-backed shared directory.
* **Named volumes**: default `local` driver, each volume is its own **sparse ext4 image** at
  `~/Library/Application Support/com.apple.container/volumes/<name>/volume.img`, default apparent size
  **512 GiB** (`sizeInBytes` is capacity, not usage). Creation-time options `size=` (fixed) and
  `journal=ordered|writeback|journal[:size]`. Lifecycle: `volume create|list|inspect|delete|prune`.
  Named volumes are explicitly recommended over bind mounts for I/O performance.
* **Anonymous volumes** (`-v /path`) are auto-created, labelled `com.apple.container.resource.anonymous`,
  and — unlike Docker — **not** removed with `--rm`; you must clean them up.
* **tmpfs** mounts are guest-memory-only, not shareable between containers.
* No automatic shrinking: freed blocks stay allocated in the sparse image (use `container clean`,
  `prune`, `system df`).

### 1.4 Ports, DNS, hostnames, networks

* **Publishing**: `-p/--publish [host-ip:]host-port:container-port[/tcp|udp]` forwards loopback→container;
  `--publish-socket host_path:container_path` publishes a Unix socket. Every container also gets a real
  IP on a vmnet network, reachable from the host and from peers on the same network, so port publishing
  is optional for host-side tools (find it with `container inspect`).
* **Embedded DNS**: set `[dns] domain = "test"` in `~/.config/container/config.toml` → every container is
  registered as `<name>.test`, and containers resolve each other by that name. Aliases:
  `--network default,alias=api.test,alias=.gateway.test` (exact aliases and dot-prefixed subtree aliases;
  ≤32 per network; fixed at create time). For the **Mac itself** to resolve those names you need
  `sudo container system dns create test`, which writes `/etc/resolver/<domain>` pointing at
  `127.0.0.1:2053`. That is one of the few admin-requiring operations.
* **Documented gap**: bare-hostname lookup of a container on a *custom* network does not work
  (`apple/container#1809`); use `<name>.<domain>` on `default`, or the container IP.
* **Custom networks (macOS 26+ only)**: `container network create foo [--subnet 192.168.100.0/24]
  [--subnet-v6 fd00:1234::/64] [--internal]`; networks are mutually isolated. On **macOS 15** all
  containers share the default vmnet network, `container network *` is unavailable, `--network` errors,
  and container-to-container traffic is impossible (vmnet limitation).
* Default subnet is `192.168.64.0/24`, overridable via `[network] subnet`/`subnetv6`.

### 1.5 `container machine`

A `machine` is a **persistent Linux environment** built from an OCI image: the image's `/sbin/init`
runs as PID 1, your macOS `$HOME` is mounted **read-write at the same path**, `SSH_AUTH_SOCK` is
forwarded, *all* Linux capabilities are added, masked/read-only paths are disabled, and the guest account
gets `NOPASSWD:ALL` sudo mapped to your host UID/GID. Apple documents this trade-off explicitly: it
"trades away much of the isolation that makes a regular Apple container safer".

Commands: `machine create|run|list|inspect|set|set-default|logs|stop|delete` (alias `m`). `--cpus`,
`--memory` (**default: half of host memory**), `--home-mount ro|rw|none`, `--user/--uid/--gid/--home`,
`--virtualization` (nested virt, needs M3+ and a `CONFIG_KVM=y` kernel), `--kernel`. Filesystem survives
stop/start; `machine set` changes take effect after restart.

Product read: `machine` is the closest native analogue to "one dev box per project" and is great for
trusted, single-project use — but it is the *opposite* of the VM-grade isolation goal if projects are
semi-trusted. For our isolation story, model a project as a set of `container run` services on a
project-scoped network, and treat `machine` as an optional "shell project" mode.

### 1.6 Snapshot / commit

* `container commit <container> <ref>` — new image from a container's filesystem; **for a running
  container it takes a runtime snapshot automatically** for consistency (added in 1.6.0, #1399/#1762).
* `container export -o file.tar <container>` — filesystem tar, also snapshot-consistent when running.
* `container image save/load|tag|prune`, `container system df`, `container clean` complete the
  image-storage surface.
* There is no "VM snapshot / restore whole project" primitive in the CLI. Snapshotting is
  image-level (commit) not machine-state-level. Containerization exposes the pieces (EXT4 block
  device, `ContainerManager`, image store) rather than a one-call snapshot API — so if we want
  "checkpoint a project", we build it from commit + volume images.

### 1.7 The actual API surface (important)

Three distinct layers, in decreasing order of supported-ness:

1. **CLI, with structured output** — the supported integration surface. `container list|image list|
   stats|system status|system version|system df|system property list|network list|volume list` all accept
   `--format json|yaml|toml` (JSON documented with `jq` examples in `docs/container-inspection.md`).
   `container inspect` / `image inspect` / `volume inspect` / `network inspect` / `machine inspect` emit
   JSON only. The README states CLI compatibility "generally preserves backward compatibility within a
   major release" and that releases are product versions, not semver.
2. **`container-apiserver` XPC API** — a launch agent started by `container system start`. The README
   says **"The `container-apiserver` XPC API compatibility preserves forward and backward compatibility
   within a major version"**, while "other non-public XPC helpers do not guarantee CLI or API
   compatibility". So apiserver's XPC is the semi-public daemon API. Client/server code lives in
   `Sources/ContainerXPC`, `Sources/APIServer`, `Sources/Services`. It is Swift-only, in-process-adjacent,
   and requires you to speak the same XPC protocol version as the installed service.
3. **`containerization` Swift package** — the library API you embed: `LinuxContainer`,
   `LinuxProcess`/`LinuxProcessConfiguration`, `ContainerManager`, `VMConfiguration`/`VMResources`,
   `Mount`/`AttachedFilesystem`, `Network`/`VmnetNetwork`/`NATNetworkInterface`, `DNSConfiguration`,
   `Vminitd`, `Kernel`, `UnixSocketRelay`, `ContainerStatistics`, `ExitStatus`. This is the layer that
   gives you per-VM control without a daemon, and it is also the layer with the weakest stability
   promise: "source stability is only guaranteed within minor versions (0.1.1 → 0.1.2)" and they
   recommend `.upToNextMinorVersion`. It is pre-1.0 (0.52.x).

There is also a **CLI plugin mechanism** (`Sources/Plugins`, plugins in the install root; `container build`
and `container builder` were extracted into a plugin in 1.6.0), which is a legitimate way to add
`container <our-verb>` subcommands.

---

## 2. Embedding vs shipping

### 2.1 Can we link `containerization` and skip a separate installer?

**Yes, technically.** `containerization` is a normal SwiftPM package (Apache-2.0) that links
Virtualization.framework directly — "no extra binaries required" for the macOS VMM path. A third-party app
can add it as a package dependency and drive `LinuxContainer` in-process. The catch is that a container
needs three artifacts that are *not* in the Swift package's compiled output:

1. a **Linux kernel** (`vmlinux`) — download or build, then bundle or fetch;
2. the **`vminitd`/`vmexec` guest init** — a static Linux binary, delivered in `container` as an *init
   image* (a container image, referenced by `[vminit] image = ...` in config.toml);
3. an **ext4 image builder / OCI store** path for rootfs (in-package: `ContainerizationEXT4`,
   `ContainerizationOCI`).

So "no separate installer" is achievable, at the cost of shipping (or downloading) kernel + init image
yourself and owning the upgrade story. Practical middle ground: **bundle the `container` CLI and helpers
inside our app bundle** (it is Apache-2.0), invoke it with `--app-root` / `--install-root` / `--log-root`
pointed at our own directories. Those flags exist precisely so `container` can run out of a non-system
location — `BUILDING.md` uses them for isolated test installs (`container system start --app-root
.test-data`).

### 2.2 Entitlements and notarization

* `com.apple.security.virtualization` — required to use Virtualization.framework; a Boolean entitlement
  added through the Xcode "Virtualization" capability / provisioning profile. Self-serve.
* `com.apple.vm.networking` — **required to use the vmnet APIs and it is restricted**: "This entitlement
  is restricted to developers of virtualization software. To request this entitlement, contact your Apple
  representative." Containerization's `VmnetNetwork.swift` is exactly a vmnet client, so Apple's model of
  "every container gets a host-reachable IP on a vmnet network" is gated behind a non-self-serve
  entitlement. This is the single biggest embedding risk: budget for an Apple entitlement request, and
  design a fallback (userspace NAT/port-forward only, i.e. host-loopback publishing, no per-container IP)
  if it is not granted.
* Note also a documented macOS 26 vmnet bug: network creation fails if the `container` helper
  applications live under `Documents` or `Desktop`. Keep helpers in `/Applications/...` or
  `~/Library/Application Support/...`, not in a synced Documents tree.
* Distribution: Developer ID signature + hardened runtime, then notarize with `notarytool submit --wait`
  and `stapler staple` (Apple: "Notarizing macOS software before distribution").

### 2.3 What `container`'s installer needs admin for, and how to avoid it

The signed `.pkg` (pkg id `com.apple.container-installer`) installs into **`/usr/local`** — `bin/` and
`libexec/` — which is why it prompts for an administrator password. Runtime state is per-user:
`~/Library/Application Support/com.apple.container`, `~/.config/container/config.toml`, and launchd
**agents** (labels `com.apple.container.apiserver`, `com.apple.container.container-core-images`,
`com.apple.container.container-network-vmnet`, `com.apple.container.container-runtime-linux.<name>` — from
`BUILDING.md`'s `launchctl list` example). No privileged daemon, no kernel extension, no `sudo` needed to
run.

Admin is required for exactly two things:
1. writing into `/usr/local` (avoidable via `--install-root`/`--app-root`/`--log-root`);
2. `container system dns create <domain>`, which writes `/etc/resolver/<domain>` (only needed for the
   *Mac* to resolve `<name>.<domain>`; containers resolve each other without it).

Uninstall (`scripts/uninstall-container.sh`) works off `pkgutil --only-files --files
com.apple.container-installer`, `pkgutil --forget`, and optionally removes
`~/Library/Application Support/com.apple.container` + `defaults delete com.apple.container.defaults`.

---

## 3. Version gates

| Fact | Source |
|---|---|
| "You need a Mac with **Apple silicon** to run `container`." | `container/README.md` |
| "`container` is supported on **macOS 26**… We do not support older versions" | `container/README.md` |
| "You can run `container` on **macOS 15**" but with limitations: no container-to-container networking, `container network` unavailable, `--network` errors, IP/subnet mismatch risk on first container | `container/docs/technical-overview.md` |
| Building Containerization requires **Apple silicon + macOS 26 + Xcode 26**; "Older versions of macOS are not supported" | `containerization/README.md` |
| Building `container` needs macOS 15 minimum, macOS 26 recommended, Xcode 26 | `container/BUILDING.md` |

So: **runtime floor = macOS 26 on Apple silicon** for the supported configuration; macOS 15 runs with
documented networking degradation; **macOS 14 and earlier: not supported at all**. **Intel Macs: not
supported** — the project is Apple-silicon-only, the guest kernels/`vminitd` are arm64, and there is no
Intel build path in either repo (Rosetta 2 is used in the *opposite* direction, to run linux/amd64 images
on Apple silicon: `--rosetta`, `Vminitd+Rosetta.swift`). `container system kernel set --arch amd64`
exists, but nothing in the docs promises Intel-host support.

Consequence for us: on macOS 14/15 (and any Intel box) Apple's stack is simply unavailable, so a second
backend is mandatory, not optional.

---

## 4. Lima

### 4.1 Per-project VM

```bash
limactl create --name proj-a --tty=false --vm-type=vz --mount-type=virtiofs \
  --cpus 4 --memory 8 --disk 60 template:docker
limactl start proj-a
limactl shell proj-a -- <cmd>          # exec; default instance name can be shortened to `lima <cmd>`
limactl stop proj-a / limactl delete --force proj-a
```

Each instance is a real VM under `$LIMA_HOME` (default `~/.lima/<name>`) with its own disk, cloud-init,
agent and (optionally) container engine. `limactl` also supports `--set key=value` overrides,
`$LIMA_HOME/_config/default.yaml` and `override.yaml` for global defaults, and per-instance
`default.yaml` layering — a clean hook if we want to inject our own defaults.

### 4.2 VM type

`vmType` is fixed at creation. **`vz` is the default on macOS ≥ 13.5** (Lima ≥ 1.0) and is Virtualization.framework;
`qemu` is the fallback and the only option where VZ can't do the job (e.g. non-native arch, legacy BIOS
guests). VZ caveats: no `legacyBIOS`, no kernel boot logs in `serial.log`, and no cross-arch guests
(Virtualization.framework cannot run an Intel guest on ARM or vice versa).

### 4.3 Container engine inside

* **containerd + nerdctl** is the historical default: `containerd.user` (rootless) defaults **true** on
  Linux x86_64/aarch64 guests; `containerd.system` (rootful, plus BuildKit and stargz snapshotter)
  defaults false.
* **dockerd** via `template:docker`, with the socket surfaced to the host:
  `export DOCKER_HOST=$(limactl list docker --format 'unix://{{.Dir}}/sock/docker.sock')`.
* `template:k8s` for Kubernetes, `template:podman`, etc.

For a uniform contract, the Docker-compatible socket from `template:docker` is the cheapest common
denominator with the rest of the ecosystem; nerdctl gives containerd-native semantics without a Docker
daemon.

### 4.4 File sharing and performance

Defaults by version: `reverse-sshfs` (< 0.17), virtiofs for VZ from 0.17, and **since 1.0: 9p for QEMU on
non-Windows, virtiofs for VZ**. virtiofs on macOS = VZ shared directory (`mountType: virtiofs`, needs
`vmType: vz`, macOS ≥ 13). 9p is the QEMU path (`9p2000.L`, `msize` 128 KiB, `fscache`/`mmap` caching).
There is an experimental `mountInotify` (Lima ≥ 0.21) because **inotify does not propagate across 9p /
virtiofs shares** by default — that matters for file-watching dev servers, and is a real functional gap
vs. Apple containers' virtiofs mounts (which have their own quirks but are the same class of problem).

Lima publishes port-forwarding benchmarks (M4 Max, macOS 26.0.1, Lima 2.0.0-alpha.2):

| Path | TCP | TCP reverse |
|---|---|---|
| localhost, SSH (no vsock) | 4.06 Gbit/s | 3.84 Gbit/s |
| localhost, gRPC forwarder | 5.37 Gbit/s | 7.11 Gbit/s |
| localhost, SSH over AF_VSOCK | 6.32 Gbit/s | 7.47 Gbit/s |
| by IP, `lima:shared` | 3.46 Gbit/s | 2.35 Gbit/s |
| by IP, `vzNAT` | **59.2 Gbit/s** | **130 Gbit/s** |

`vzNAT` (`--network vzNAT`) gives the guest a host-reachable IP and is ~10–50× faster than slirp/socket_vmnet
paths. Note: `socket_vmnet`-based `lima:shared` requires installing a root-owned helper — avoid if
"no admin" is a hard requirement; `vzNAT` needs no helper.

### 4.5 Port forwarding

Automatic localhost forwarding, SSH or gRPC backend (default: **gRPC from v1.1**; SSH was the default in
v0.1–0.9 and again briefly in v1.0.1). SSH forwarder: no UDP, spawns an SSH master process; gRPC: TCP+UDP,
no extra process. Lima ≥ 2.0 uses **AF_VSOCK** for SSH transport with VZ + systemd ≥ v256. Static rules,
ranges, per-port `hostIP`/`guestIP`, `proto`, `ignore`, and Unix-socket forwarding are all YAML-configurable
(`portForwards:`).

### 4.6 Snapshots

`limactl snapshot create|list|apply|delete <instance> [--tag <t>|--id <n>]`. Command exists in 2.2.1.
Treat as best-effort: Lima's snapshot support is thin (id/tag-keyed, no documented atomicity guarantees,
QEMU-oriented in practice). Do not build the product's "restore project" on it; prefer rebuilding VMs from
declarative config + named volumes.

### 4.7 Cold start and RAM

* Builtin defaults: `cpus: min(4, host cores)`, `memory: min(4 GiB, half of host RAM)`, `disk: 100 GiB`.
* Cold start: Lima's own quickstart output shows `INFO[0029] READY` — i.e. **~30 s** for a first start
  that includes image download/validation and cloud-init; subsequent starts are faster but still seconds,
  not sub-second. `limactl start` also has first-boot provisioning (package installs for containerd etc.),
  which can take minutes once.
* Compare: Apple containers are per-container and sub-second; Lima is per-project and ~10s of seconds.
  Both are acceptable if the UX starts VM bring-up in the background and streams progress.

### 4.8 Can Lima be bundled? Windows?

* **License**: `lima-vm/lima` is **Apache-2.0**, CNCF incubating. Compatible with our constraint.
* **Size**: `lima-2.2.1-Darwin-arm64.tar.gz` = **38.3 MB** (x86_64: 25.0 MB). That's the whole
  `limactl`/`lima` toolchain incl. guest agents; guest OS images (Ubuntu cloud image, several hundred MB)
  are fetched at first `start` — do **not** bundle images.
* **Windows**: Lima does publish `lima-2.2.1-Windows-AMD64.zip` / `-ARM64.zip` (24.9 / 22.3 MB) and has a
  `vmType: wsl2` driver, but Lima's own docs label `wsl2` vmType/mount type **experimental**, with WSL2
  file-permission caveats. **Not worth supporting as our Windows backend** — on Windows we already own WSL
  directly, and inserting Lima means shipping a second experimental WSL driver on top of WSL.

---

## 5. Fallback matrix (recommendation)

| Host | Backend | Rationale |
|---|---|---|
| **(a) macOS 26+, Apple silicon** | **Apple `container` / Containerization** | One microVM per container = the isolation product promise; per-container IP + DNS names; `machine` for shell projects; `commit` for snapshots; `~150 MB` footprint; no third-party VM tool. Escape hatch: if `com.apple.vm.networking` is refused, run containers with loopback publishing only. |
| **(b) macOS 14–15, Apple silicon** | **Lima (`vmType: vz`)**, `template:docker` or nerdctl | Apple's stack is unsupported below macOS 26 (15 = degraded networking, 14 = no). Lima's VZ path is the same Virtualization.framework underneath, so per-project VM isolation is preserved; virtiofs mounts; `vzNAT` for host-reachable IPs without a root helper; LaunchAgent autostart built in. Cost: ~0–5 s slower start, no per-container VM (per-project VM instead), 38 MB binary. |
| **(c) Intel Macs** | **Do not support for v0** | Apple's stack is Apple-silicon-only. Lima would work (`vmType: vz` needs macOS ≥ 13, `qemu` otherwise) but: no Rosetta-equivalent for arm64 images (must run amd64 images or emulated VMs), VZ on Intel can't run arm64 guests, tiny share of the target audience, and it doubles our QE matrix and image-variants story. Ship a clear "Apple silicon required" check at install time. Revisit only if telemetry shows real demand — and then only as `qemu`-based best-effort. |

Note the practical consequence of (a)+(b): the *same* project spec must be executable either as N
per-container microVMs (Apple) or as one VM running N containers (Lima). That shapes the daemon contract
below: model projects as "a set of services + volumes + published ports + a private network", and let the
backend decide how many VMs that maps to.

---

## 6. Uniform daemon contract (Apple / Lima / WSL)

**Recommendation: process-CLI-first, with a thin per-backend adapter.** Not libraries, not XPC.

| Capability | Apple `container` | Lima | WSL (for contrast) |
|---|---|---|---|
| Start control plane | `container system start|stop|status` | n/a (per-instance host agent) | `wsl --status` |
| Create project env | `container network create`, `create/run`, `volume create` | `limactl create/start --name` | `wsl --import <distro> <dir> <tar>` |
| Exec | `container exec [-it] <c> <cmd>` | `limactl shell <i> -- <cmd>` | `wsl -d <d> -- <cmd>` |
| Logs | `container logs [-f] [--boot] <c>` | `limactl shell <i> journalctl …` / engine logs | `wsl -d <d> -- journalctl` |
| Streaming I/O | `run -i -t`, `start --attach`, `exec -i -t`; stdio over vsock | SSH/vsock into guest, `--tty=false` for automation | pipes over `wsl.exe` |
| Ports | `-p host:ctr[/udp]`, `--publish-socket` | YAML `portForwards:` + dynamic localhost fwd | `localhostForwarding` |
| Health/metrics | `system status --format json`, `stats --format json --no-stream`, `system df` | `limactl list --format json`, `limactl info` | `wsl --list --verbose` |
| Snapshot | `commit` / `export` | `limactl snapshot` (weak) | export tar |
| Structured output | `--format json|yaml|toml` on list/inspect/stats/system/volume/network | `--format json`/Go templates on `list`; `limactl info` JSON | none (parse text) |
| Auth to daemon | XPC to `container-apiserver` (Swift only) | files/sockets under `$LIMA_HOME`, SSH keys | `wsl.exe` LSA |

Design rules:

1. **One adapter per backend, one internal interface.** Interface = `EnsureHostReady`, `CreateProject`,
   `Start/Stop/DeleteProject`, `Exec(streaming)`, `Logs(follow)`, `List/Inspect`, `PublishPorts`,
   `Snapshot`, `Capabilities`. Return a **capability descriptor** per backend (has per-container IPs? has
   `machine`? has snapshots? mount type? max vCPU?) so the UI can degrade honestly instead of the daemon
   guessing.
2. **JSON over CLI as the wire format**, because that is the only surface Apple promises compatibility
   for within a major version, and Lima has `--format json`/`nl` templates. Pin the *major* version of
   each backend at startup (`container system version --format json`, `limactl --version`) and refuse to
   drive an unknown major.
3. **Never depend on `container inspect` field names without a version gate.** Apple explicitly reserves
   the right to break CLI shape across majors, and `container`'s releases are product versions, not semver.
   Normalize into our own schema in the adapter.
4. **Error surfacing is the weak point.** Neither `container` nor `limactl` emits structured errors;
   you get a non-zero exit plus human text on stderr (and for Apple, an XPC error string underneath). So:
   run every mutating command with `--debug` captured to a ring buffer, classify stderr with a small
   pattern table (image pull auth, port already bound, no such container, network not available on this
   macOS, service not running), and always attach raw output to the surfaced error. `container system
   status --format json` should be the liveness probe; treat "apiserver unresponsive" as a distinct
   recoverable state (`system stop` + `system start`).
5. **Streaming**: for Apple, `container run -i -t` / `exec -i -t` with piped stdio is enough (no pty
   allocation needed on our side unless we want TTY semantics); for Lima, `limactl shell --tty=false`
   plus an explicit pty layer inside the guest if a project wants a terminal. `container logs -f` and
   `--boot` (VM boot log) are the debugging escape hatch; expose both.
6. **Apple-specific XPC**: do **not** build on `container-apiserver`'s XPC despite the compatibility
   promise — it ties us to Swift, to the same audit-token/entitlement model as Apple's own CLI, to the
   installed service version, and to a module surface that is not documented as a public SDK. If we ever
   want in-process control, use the `containerization` package with `.upToNextMinorVersion` pinning and a
   hard "this is pre-1.0" risk note.
7. **Ordering/idempotency**: `container volume delete` refuses volumes in use by stopped containers;
   `network delete` refuses networks with attached containers; `limactl` refuses to start an instance with
   an orphaned driver but auto-recovers stale PID/socket files (documented in Lima ≥ 2.2 autostart docs).
   The adapter must implement dependency-ordered teardown (containers → machines → volumes/networks) and
   treat Lima's self-recovery as the happy path.

---

## 7. Lifecycle at login, without admin

* **Apple container**: services are launchd **agents** owned by the logged-in user
  (`com.apple.container.apiserver` etc.), started by `container system start` and deregistered by
  `container system stop [--prefix ...]`. There is no privileged component, so nothing here needs admin.
  Our app should *not* try to register Apple's labels itself; call `container system start` (idempotent)
  from our own agent, and `container system stop` on quit only if the user asks (the service is shared).
* **Our own background item**: `SMAppService` (macOS 13+) is the supported API — "use SMAppService to
  register and control LoginItems, LaunchAgents, and LaunchDaemons as helper executables for your app".
  For a per-user helper: place the plist in `Contents/Library/LaunchAgents/`, ship the helper in
  `Contents/MacOS/`, call `SMAppService.agent(plistName:).register()`. The user sees and can disable it in
  System Settings → General → Login Items & Extensions, and `status` returns
  `.requiresApproval`/`.enabled` — surface that in UI rather than failing silently. Plain
  `~/Library/LaunchAgents/*.plist` also works without admin (Lima does exactly this) but is less
  discoverable/uninstallable; prefer SMAppService, fall back to a plain plist only if needed.
  Use `KeepAlive` for the supervisor; do not use `RunAtLoad` on the VM itself — start the VM lazily on
  first project use.
* **Lima**: `limactl autostart enable <instance>` (Lima ≥ 2.2) installs a **LaunchAgent in
  `~/Library/LaunchAgents/`** with `--keep-alive=true` by default; `--condition=boot` instead installs
  `/Library/LaunchDaemons/io.lima-vm.daemon.<instance>.plist` (needs sudo once) — we want the
  login condition only. `limactl autostart disable <instance>` removes it. This is exactly the "no admin"
  shape we need, and it means we can either use Lima's mechanism or our own agent calling
  `limactl start <instance>`; doing the latter keeps lifecycle ownership in one place.
* **Clean uninstall** must run, in order:
  1. our agent: `SMAppService.agent(...).unregister()`;
  2. Apple: `container system stop`; `container delete --all --force`; `container machine rm <each>`;
     `container volume delete --all`; `container network delete --all`; `container builder stop|delete`;
     then remove `~/Library/Application Support/com.apple.container` and `~/.config/container` (only the
     keys we own — `[dns] domain` is shared state) and `defaults delete com.apple.container.defaults`
     only if the user never had their own install;
  3. host DNS: `sudo container system dns delete <domain>` removes the `/etc/resolver/<domain>` file —
     this is the one admin step in uninstall; make it an explicit, explained prompt (and note the file is
     just a text file under `/etc/resolver`, so "leftover" is fully reversible);
  4. Lima: `limactl stop <i>`, `limactl delete --force <i>`, `limactl autostart disable <i>`, then remove
     `~/.lima` (only instances we created — never `rm -rf ~/.lima` if the user has their own Lima).
  5. Leave no privileged helper, no LaunchDaemon, no KEXT.
  Also: `container system dns create` and `/etc/resolver` are the only two things that can look like
  "the app changed my system"; document them.

---

## 8. Packaging constraints

* **Signing/notarization**: Developer ID Application signature, **hardened runtime** enabled, notarize via
  `notarytool submit ... --wait`, then `stapler staple` the `.app`/`.dmg`/`.pkg`. Any embedded helper
  executables (our supervisor, and — if bundled — `container`, `container-apiserver`,
  `container-runtime-linux`, `container-core-images`, `container-network-vmnet`, `limactl`, `lima`) must be
  signed with the same team ID and have the right entitlements: `com.apple.security.virtualization` on
  anything that instantiates a VM, `com.apple.vm.networking` on anything that touches vmnet. Notarization
  will reject unsigned nested Mach-Os, and helper binaries named `container-*` inside a bundle need to be
  in `Contents/MacOS/` or `Contents/Helpers/` (not `Resources`) to be signed and launched cleanly.
* **Licensing (Apache-2.0 obligations)**: ship `LICENSE` and `NOTICE` for `apple/container`,
  `apple/containerization`, and `lima-vm/lima` verbatim, plus an in-app "Open Source Licenses" screen.
  Mark any files we modify. Do not use Apple's or the Lima project's marks to imply endorsement (the
  Apache-2.0 trademark clause); name them factually ("uses Apple's open-source container runtime").
  Note the nuance: Apple's *signed installer pkg* is Apache-2.0 code, but shipping Apple's signed binary
  inside our product is a different act from shipping our own build of the same source. Safest path:
  **build from source ourselves**, sign with our identity, and never redistribute anyone's prebuilt
  binaries (Docker Desktop and OrbStack are excluded by policy anyway).
* **App size impact**:
  * Apple path: `container-1.6.1-installer-signed.pkg` = **156.0 MB** (unsigned equivalent 152.7 MB).
    That is CLI + all XPC helpers + dSYM-less release binaries, before any kernel or images. Budget
    ~+150 MB in the bundle if we embed it, or ~0 if we require/optionally install the upstream pkg
    (worse UX, and it needs admin).
  * Kernel: not bundled by the installer — installed by `container system start`/`system kernel set`
    (download). Keep that behaviour: fetch on first run, verify digest (`--digest sha256:...` is supported),
    cache in Application Support. This keeps the download small and makes kernel upgrades independent of
    app releases.
  * Lima path: `+38 MB` (arm64 tarball) for `limactl`/`lima` + guest agents, **plus whatever guest image**
    the template points at (hundreds of MB). Never bundle guest images; use a pinned digest URL and stream
    progress on first project creation.
  * Net: an app that embeds both is ~200 MB installed and can grow to several GB once images land —
    so the installer should present "runtime" vs "images" disk usage separately and give the user a
    "reclaim" action (`container prune`, `volume prune`, `image prune`, `builder delete`, `limactl delete`).

---

## 9. Risks / open questions to close before build

1. **`com.apple.vm.networking` entitlement** — request now; it gates per-container IPs on the Apple path.
   Fallback design: loopback publishing + userspace forwarding only.
2. **No published boot-time numbers** — benchmark `container run --rm alpine true` and a real project
   service on M-series before promising "instant".
3. **Memory ballooning is partial** — N projects ≈ N VMs' resident memory with no reclamation; the product
   needs an explicit "stop project" affordance and probably an idle-reaper.
4. **Containerization is 0.x with minor-version source stability** — if we embed the Swift package, pin
   `.upToNextMinorVersion` and expect breaking changes between minors.
5. **inotify across virtiofs/9p** — file-watching dev servers (Vite, webpack) may not see host-side changes
   in the Lima path (documented `mountInotify` is experimental). Validate against our project templates.
6. **`container machine` is not an isolation boundary** — if any UI surface exposes it as "the project",
   it silently trades away the product's core promise.
7. **macOS 15 is a gray zone** — Apple says "you can run on 15, we won't fix 15-only issues". Decide
   explicitly: treat 15 as Lima-only (recommended) rather than shipping a degraded Apple path.

---

## Sources

- apple/container README (requirements, macOS 26 support, install/uninstall, apiserver XPC compatibility): https://github.com/apple/container/blob/main/README.md
- apple/container technical overview (VM-per-container, launch agents/XPC helpers, macOS 15 limits, memory ballooning): https://github.com/apple/container/blob/main/docs/technical-overview.md
- apple/container command reference (run/create/exec/logs/commit/export/volume/network/machine/system/`--format`): https://github.com/apple/container/blob/main/docs/command-reference.md
- apple/container networking (DNS domain, `/etc/resolver`, aliases, `-p`, custom networks, macOS 26 gate): https://github.com/apple/container/blob/main/docs/networking.md
- apple/container volumes (named ext4 sparse images, journaling, anonymous volumes, tmpfs, mount options): https://github.com/apple/container/blob/main/docs/volumes.md
- apple/container resource usage (1 GiB/4 vCPU defaults, overcommit, stats JSON, sparse images): https://github.com/apple/container/blob/main/docs/resource-usage.md
- apple/container container machine (persistent env, home mount, caps, sudo, memory default): https://github.com/apple/container/blob/main/docs/container-machine.md
- apple/container inspection/JSON output: https://github.com/apple/container/blob/main/docs/container-inspection.md
- apple/container BUILDING.md (macOS 15 min / 26 recommended, `--app-root` isolation, launchd labels, vmnet Documents/Desktop bug): https://github.com/apple/container/blob/main/BUILDING.md
- apple/container 1.6.1 release (installer pkg sizes, `container commit` in 1.6.0, DNS aliases): https://github.com/apple/container/releases/tag/1.6.1
- apple/containerization README (sub-second boot, vminitd gRPC/vsock, kernel, backends, 0.x source stability, macOS 26/Xcode 26): https://github.com/apple/containerization/blob/main/README.md
- Containerization sources (`VmnetNetwork`, `Network`, `NATNetworkInterface`, `VZVirtualMachineManager`, `Vminitd`, `Kernel`, `VMResources`): https://github.com/apple/containerization/tree/main/Sources/Containerization
- apple/container sources layout (APIServer, ContainerXPC, Services, Plugins, CLI): https://github.com/apple/container/tree/main/Sources
- Apple: `com.apple.vm.networking` entitlement (restricted; request from Apple representative): https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.vm.networking
- Apple: `com.apple.security.virtualization` entitlement: https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.virtualization
- Apple: Notarizing macOS software before distribution (`notarytool`/`stapler`): https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution
- Apple: SMAppService (register LoginItems/LaunchAgents/LaunchDaemons, macOS 13+): https://developer.apple.com/documentation/servicemanagement/smappservice
- Lima: VM types (vz default ≥ 13.5, qemu fallback, per-OS matrix): https://lima-vm.io/docs/config/vmtype/
- Lima: VZ caveats (no cross-arch, no legacyBIOS, no boot logs): https://lima-vm.io/docs/config/vmtype/vz/
- Lima: filesystem mounts (reverse-sshfs / 9p / virtiofs defaults by version, mountInotify experimental): https://lima-vm.io/docs/config/mount/
- Lima: port forwarding (SSH vs gRPC, AF_VSOCK, benchmark table): https://lima-vm.io/docs/config/port/
- Lima: automatic startup (`limactl autostart`, LaunchAgent vs LaunchDaemon, keep-alive, recovery): https://lima-vm.io/docs/usage/autostart/
- Lima: usage (`template:docker`, `DOCKER_HOST` socket, `--tty=false`, `INFO[0029] READY`): https://lima-vm.io/docs/usage/
- Lima: `limactl snapshot` reference: https://lima-vm.io/docs/reference/limactl_snapshot/
- Lima default template (cpus/memory/disk defaults, containerd user/system, network options): https://github.com/lima-vm/lima/blob/master/templates/default.yaml
- Lima README (containerd/nerdctl origin, Docker/Podman/k8s, adopters): https://github.com/lima-vm/lima/blob/master/README.md
- Lima 2.2.1 release (Darwin arm64 38.3 MB, Windows AMD64/ARM64 assets): https://github.com/lima-vm/lima/releases/tag/v2.2.1
- Lima license metadata (Apache-2.0): https://github.com/lima-vm/lima
- WSL CLI reference (for the cross-platform contract): https://learn.microsoft.com/en-us/windows/wsl/basic-commands
