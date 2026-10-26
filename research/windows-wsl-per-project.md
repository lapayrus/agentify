# Windows / WSL2 per-project provisioning and operations

Research ticket: concrete provisioning + operations detail for "one WSL2 distro per Project" on Windows 11.
Primary sources only (Microsoft Learn, microsoft/WSL repo, distro vendor artifacts). Numbers marked **[measured]** were
verified on the reference machine described below during this investigation; everything else is cited.

**Reference machine [measured]:** Windows 11 Home Single Language, build 10.0.26200.9711; WSL `2.6.3.0`; kernel
`6.6.87.2-1`; AMD Ryzen 7 5800H, 13.9 GB RAM; user `.wslconfig` = `memory=6GB, processors=4, swap=4GB` (so the WSL VM
is capped at 6 GB, not the 50 % default); no `networkingMode` set (NAT); no `sparseVhd` set. Standard (non-admin) shell.

---

## 0. Architectural finding that changes the design (flag this against ADR-0001)

**Per-distro isolation is not a kernel boundary, and it is not a network boundary either.**

* All WSL2 distros run inside **one** utility VM with **one** shared kernel: `.wslconfig` `[wsl2]` settings are documented
  as applying to "the VM that powers **any** WSL 2 distribution"; `uname -r` is identical in every distro.
  <https://learn.microsoft.com/en-us/windows/wsl/wsl-config>
* All distros in that VM share **one network namespace and one IP** [measured]: a second, freshly imported distro
  reported the same `eth0` address (`192.168.188.230`) as the pre-existing Ubuntu distro, **saw Ubuntu's listener in
  `netstat -ltn`**, and got `nc: bind: Address in use` when binding the same port Ubuntu already held on `0.0.0.0`.

So `docs/adr/0001-per-project-isolation-boundary.md` ("a real kernel boundary") is **not** satisfied by one distro per
project. What you actually get per distro: own root filesystem (`ext4.vhdx`), own PID/mount/IPC/user namespaces, own
cgroup subtree (`isolateDistroCgroup=true`, cgroup v2), own lifecycle. You do **not** get: kernel isolation, memory
capping (memory/processors are VM-global), or port isolation. Cross-project port collisions are structural (see §4).
Two consequences for the product: (a) resource limits must be enforced with cgroups inside the distro
(`memory.max`, `cpu.max` on the distro's cgroup), not `.wslconfig`; (b) the UI must show ports as a **machine-wide**
resource, allocated centrally by the daemon, never "whatever the compose file says".

---

## 1. Creating a per-project distro

### Command

```powershell
wsl --import <DistroName> <InstallDir> <rootfs.tar> --version 2
# e.g.
wsl --import agentify-myproj "$env:LOCALAPPDATA\Agentify\distros\myproj" .\rootfs.tar --version 2
```

Options that matter: `--version <1|2>` (always pass 2), `--vhd` (import a `.vhdx` instead of a tar),
`wsl --import-in-place <Name> <ext4.vhdx>` (vhd must already be ext4), `wsl --export <Name> <file> --format tar|tar.gz|tar.xz|vhd`.
<https://learn.microsoft.com/en-us/windows/wsl/basic-commands>, `wsl --help` on 2.6.3.0.

Behaviour and constraints:

* Import always lands you as **root**; the distro has **no launcher exe**, so `ubuntu config --default-user` does not
  work. Set the default user via `/etc/wsl.conf` `[user] default=<name>`, then `wsl -t <Name>` and restart.
  <https://learn.microsoft.com/en-us/windows/wsl/use-custom-distro>
* `--import` creates the tar's contents into a fresh VHD; the VHD **floor size is ~76 MiB** regardless of tar size
  [measured: `ext4.vhdx` = 79,691,776 bytes for a 3.5 MB Alpine rootfs].
* The `<InstallDir>` is yours to choose; the path is recorded in `HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`
  (`BasePath`). Never let Windows tools (backup, antivirus "compress/encrypt contents", Explorer) touch the VHD: the docs
  explicitly warn that this corrupts the distro, and compressed/encrypted/sparse VHDs are rejected outright.
  <https://learn.microsoft.com/en-us/windows/wsl/disk-space>, <https://learn.microsoft.com/en-us/windows/wsl/troubleshooting>
* `\\wsl.localhost\<DistroName>\...` works for imported distros [measured: `Test-Path '\\wsl.localhost\agentify-t1\etc\os-release'` → True].

### Admin requirements

* **Import does not require elevation once WSL is enabled** [measured: non-elevated user imported successfully and the
  command returned `The operation completed successfully.`]. Same for `--terminate`, `--unregister`, `--export`,
  `--manage`, and running commands.
* **First-time enablement does require elevation.** `wsl --install` must be run from an elevated PowerShell
  ("Open PowerShell in **administrator** mode"); it enables the two optional components
  (`Microsoft-Windows-Subsystem-Linux`, `VirtualMachinePlatform`) and needs a **reboot**.
  <https://learn.microsoft.com/en-us/windows/wsl/install>
  Offline/scripted equivalent, elevated:
  `dism.exe /online /enable-feature /featurename:VirtualMachinePlatform /all /norestart`
  then install the WSL MSI from <https://github.com/microsoft/WSL/releases>.
  <https://learn.microsoft.com/en-us/windows/wsl/install> (Offline install section)
* Things that also need admin and that the app should therefore avoid or gate: `wsl --shutdown`, `wsl --mount`,
  `netsh interface portproxy`, `Set-NetFirewallHyperVVMSetting`/`New-NetFirewallHyperVRule`, `diskpart`, `e2fsck` repair
  via `wsl --mount`, editing `.wslconfig` in `%UserProfile%` (user-writable, but **machine-global** — see §4).

### Detecting WSL state without elevation [measured, all work as the standard user]

```powershell
# 1. Is WSL installed and how new?  (exit 0 + version block == Store/MSI WSL present)
wsl.exe --version      # 2.6.3.0
wsl.exe --status       # default distro / default version

# 2. Optional features (no admin needed): InstallState 1=Enabled, 2=Disabled, 3=Absent
Get-CimInstance Win32_OptionalFeature -Filter "Name='VirtualMachinePlatform' OR Name='Microsoft-Windows-Subsystem-Linux'" |
  Select-Object Name, InstallState

# 3. Virtualization
(Get-CimInstance Win32_ComputerSystem).HypervisorPresent                                   # True
(Get-CimInstance Win32_Processor).VirtualizationFirmwareEnabled                            # True
```

**Caveat, verified:** on this machine `Win32_Processor.SecondLevelAddressTranslationExtensions` and
`VMMonitorModeExtensions` both report **False while `HypervisorPresent` is True** (the hypervisor owns AMD-V). Do **not**
gate the UX on SLAT/VMMonitorModeExtensions; treat `HypervisorPresent=True` + `InstallState=1` + a working
`wsl --version` as "ready", and only diagnose BIOS virtualization when the *error* says so (§8).

Enablement UX that avoids a surprise UAC: check the three probes; if any fail, show a single "Enable WSL" action that
launches one elevated `powershell Start-Process -Verb RunAs` running `wsl --install --no-distribution` (or the two
`dism` commands), then require a reboot and re-probe on next launch. Never call `wsl --install` from the non-elevated app
process — it fails/will not prompt reliably.

### Windows Home vs Pro

WSL2 works identically on Home and Pro: it needs **only** "Virtual Machine Platform", which Microsoft documents as
"a *subset* of Hyper-V ... available on all Desktop SKUs", and FAQ: "WSL 2 is available on all Desktop SKUs where WSL is
available, including Windows 10 Home and Windows 11 Home". <https://learn.microsoft.com/en-us/windows/wsl/faq>
Differences that matter to an app:

* No Hyper-V Manager / `Optimize-VHD` PowerShell module on Home — VHD compaction must use `diskpart` or the
  export/import-in-place dance (§6). [measured: `HypervisorPlatform` InstallState = 2 (disabled) on this Home box]
* No Group Policy editor; policy failures on Home appear only as runtime errors (0x800704ec), and Home has no domain/MDM
  firewall lockdown.
* Store/MSIX delivery, optional features, firewall and reboot UX are identical.

---

## 2. Rootfs sourcing, size, licensing, systemd

| Source | Artifact | Size | systemd | Notes |
|---|---|---|---|---|
| Ubuntu base | `ubuntu-base-24.04.5-base-amd64.tar.gz` | **30,028,293 B ≈ 28.6 MiB** [measured via HEAD] | ships `systemd` package | <https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/> |
| Ubuntu WSL image | `ubuntu-noble-wsl-amd64-wsl.rootfs.tar.gz` | **340 MB** (gz) | yes, WSL-tuned | <https://cloud-images.ubuntu.com/wsl/releases/noble/current/> |
| Alpine | `alpine-minirootfs-3.21.2-x86_64.tar.gz` | **3,507,490 B ≈ 3.3 MiB** [measured] | **no** (OpenRC; systemd not available) | <https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64/> |
| Debian | no vendor minimal-rootfs tarball on cdimage | `debian:bookworm-slim` exported ≈ 30 MB compressed / ≈ 75 MB unpacked (approx.) | yes | `docker export`, or `debootstrap`; <https://learn.microsoft.com/en-us/windows/wsl/use-custom-distro> shows the docker-export path |

**Recommendation:** Ubuntu base for project distros (Debian-family, systemd, `apt`, smallest credible systemd rootfs at
~29 MiB); Alpine only as an optional "tiny" engine where you accept no systemd.

**Redistribution.** Safest posture: **do not bundle rootfs tarballs in the installer**; download at provision time, verify
vendor SHA256/`.zsync`/`.sha512` (Alpine publishes `.sha256`/`.sha512`/`.asc`; Ubuntu publishes `SHA256SUMS`), and cache
under `%LOCALAPPDATA%`. Reasons:

* Alpine is MIT-licensed with published checksums → bundling is trivially fine (<https://alpinelinux.org/>).
* Debian is DFSG-free and redistributable, but the *name/logo* is trademarked; redistribute unmodified, keep copyright
  notices (<https://www.debian.org/legal/licenses/>).
* `ubuntu-base` is redistributable, but Canonical's trademark/IP policy governs modified images and naming, and shipping
  a *modified* Ubuntu inside a commercial app is exactly the case that policy addresses
  (<https://ubuntu.com/legal/intellectual-property-policy>). Unmodified vendor tarball downloaded by the user's machine =
  lowest risk.
* Microsoft Store distro payloads (`.wsl` files listed in
  <https://github.com/microsoft/WSL/blob/master/distributions/DistributionInfo.json>) are **not** yours to redistribute.

**systemd.** Available on Store/MSI WSL on Windows 11:

```ini
# /etc/wsl.conf  (inside the distro)
[boot]
systemd=true
initTimeout=10000     # default 10000 ms; raise if first boot is slow
```
`[boot]` is "only available on Windows 11 and Server 2022"; `systemd` "only supported on WSL 2"; an inbox (non-Store)
WSL where `wsl --version` fails does not support it. <https://learn.microsoft.com/en-us/windows/wsl/wsl-config>
Alpine has no systemd — its container engine must be started by the `[boot] command=` hook or by an OpenRC service.

---

## 3. Container engine inside the distro

Kernel capabilities present on 6.6.87.2-microsoft-standard-WSL2 [measured from `/proc/config.gz`]:
`CONFIG_USER_NS=y`, `CONFIG_OVERLAY_FS=y`, `CONFIG_BRIDGE=m`, `CONFIG_VETH=y`, `CONFIG_NF_NAT=y`,
`CONFIG_IP_NF_NAT=m`, `CONFIG_IP_NF_FILTER=m`, `CONFIG_NF_TABLES=y`, `CONFIG_CGROUPS=y`, `CONFIG_MEMCG=y`,
`CONFIG_CGROUP_BPF=y`, `CONFIG_SECCOMP=y`, `CONFIG_IO_URING=y`, `CONFIG_HUGETLBFS=y`; cgroup2 mounted at `/sys/fs/cgroup`
(cgroup v2 default since WSL sets `cgroups=v2`). <https://github.com/microsoft/WSL2-Linux-Kernel>

Everything a database needs is therefore available **to root**: overlayfs, netfilter NAT for published ports, veth/bridge,
cgroup v2 for `--memory`/`--cpus`, hugetlbfs, io_uring (some DBs/tools probe it), seccomp.

**Least fragile choice: rootful `dockerd` inside the project distro** (or rootful Podman; see below).

* `dockerd` (rootful)
  * Works because the distro *is* the blast radius — root in distro ≠ root on Windows, and each project has its own root.
  * Fragile points: (a) **iptables backend** — modern Docker (≥ 20.10) is fine with `nft`, but old installs need
    `update-alternatives --set iptables /usr/sbin/iptables-legacy`; (b) **no init by default** — without
    `systemd=true` you must supervise `dockerd` yourself; (c) **state loss on `wsl -t`** — iptables/NAT rules, the
    `docker0` bridge and running containers die with the distro, so `dockerd` must be restarted and containers
    re-created from the manifest (i.e. treat containers as ephemeral, state in volumes); (d) `br_netfilter`/`bridge`
    modules load lazily — first container start can be slow.
* **rootless Podman** — viable but strictly weaker for "arbitrary databases".
  * Needs `/etc/subuid` + `/etc/subgid` (≥ 65,536 subordinate IDs), `newuidmap`/`newgidmap` (`uidmap` package), and
    cgroup v2 for resource limits (<https://github.com/containers/podman/blob/main/docs/tutorials/rootless_tutorial.md>).
  * Breaks: `--privileged`, host-PID/host-network style escapes, sysctl tuning (`--sysctl net.core.somaxconn=…` is
    restricted), `net.ipv4.ip_unprivileged_port_start` needed for ports < 1024, rootless networking via
    `slirp4netns`/`pasta` (no bridge; published ports go through `rootlessport`), and file ownership on bind-mounted data
    (many DB images `chown` their data dir and fail). Rootless overlayfs works on this 6.6 kernel via user namespaces, but
    `fuse-overlayfs` is still the fallback and is measurably slower per I/O.
  * Docker's own rootless mode has the same shape of prerequisites (`uidmap`, subuid/subgid, systemd user session,
    `loginctl enable-linger`) and is documented as a security/compat trade-off, not a free win.
    <https://docs.docker.com/engine/security/rootless/>
* **containerd + nerdctl** — fewest iptables moving parts for *pulling/running* but: rootful daemon (systemd unit),
  needs **CNI plugins** (`bridge`, `portmap`, `firewall`) in `/opt/cni/bin` for container networking and port publishing,
  no native compose semantics (needs `nerdctl compose`), and builds need BuildKit. More assembly, more failure surfaces,
  no benefit for the DB use case.

**Recommendation:** rootful `dockerd` + `systemd=true`, images pinned by digest, all mutable state in named volumes inside
`/var/lib/…` (ext4, in the VHD). Fall back to rootful Podman only if a Docker-free distribution requirement appears.
Do not attempt rootless for database workloads: the privileged/sysctl/chown failures are exactly the "breaks for arbitrary
databases" class the ticket asks about.

---

## 4. Port publishing: how a port inside a distro/container becomes a Windows port

Chain: container → published by the engine onto the distro's namespace → the distro's listener on `0.0.0.0`/`127.0.0.1`
→ WSL's port tracker → Windows `localhost:<port>`.

### NAT mode (default) — measured behaviour

* `localhostForwarding=true` is the default (`.wslconfig [wsl2]`). <https://learn.microsoft.com/en-us/windows/wsl/wsl-config>
* **It works** [measured]: `python3 -m http.server 18099 --bind 0.0.0.0` inside a distro was reachable as
  `http://localhost:18099` from Windows.
* **IPv6-only binds are a trap** [measured]: a listener on `:::18099` (busybox `nc`) was *not* reachable via
  `localhost:18099` from Windows, while `ss -ltn` inside showed it. Bind `0.0.0.0` (or `127.0.0.1`), not `::`.
* **All distros share one namespace** [measured]: same IP, mutual visibility of listeners, `EADDRINUSE` across distros.
  ⇒ **Two projects cannot both publish 5432.** Port collisions are a first-class product problem.
* Windows→distro-IP connections are also blocked by the **Hyper-V firewall**, on by default since WSL 2.0.9; the docs'
  remedy is an elevated `New-NetFirewallHyperVRule`/`Set-NetFirewallHyperVVMSetting`.
  <https://learn.microsoft.com/en-us/windows/wsl/networking>
* Fallbacks that exist but need admin: `netsh interface portproxy add v4tov4 …connectaddress=(wsl hostname -I)`, and
  mirrored mode.

### `networkingMode=mirrored` (Windows 11 22H2+, `.wslconfig [wsl2]`)

Benefits: LAN-reachable, IPv6, multicast, `127.0.0.1` works in both directions, better VPN behaviour.
Costs/limits, all documented: Windows Firewall/Hyper-V firewall now filters WSL traffic (inbound default-block ⇒ needs an
elevated rule); a Linux bind of a port Windows already listens on fails unless listed in `[experimental] ignoredPorts`;
a fixed list of inbound ports is never steered to Linux (UDP 68, TCP 135/1900/2869/5004/3702/5357/5358); WSL overwrites
several sysctls; and **Docker published ports are a known breakage** with the documented workarounds `--network host` or
`ignoredPorts`. <https://learn.microsoft.com/en-us/windows/wsl/networking>,
<https://learn.microsoft.com/en-us/windows/wsl/troubleshooting>

Also: `networkingMode` lives in `.wslconfig` = **machine-global**, affecting the user's other distros and Docker Desktop.
Mirrored mode also does **not** give per-project port isolation (still one netns per VM).

### Is localhost forwarding reliable enough to be the only mechanism?

No. It is a user-mode port tracker; the failure classes are documented and real: Hyper-V firewall defaults, VPNs
(Bitdefender 26.0.2.1, OpenVPN 2.6.501, McAfee Safe Connect 2.16.1.124 listed as incompatible), corporate firewall policy,
`[experimental] ignoredPorts` interactions, and IPv6-only binds. Design assumption: **"usually works, must be verified"**.

Recommended design:

1. Daemon allocates a **globally unique Windows host port per service instance** (no reuse across projects) and records it
   in the manifest/project state.
2. Inside the distro, ask the engine to publish to **`127.0.0.1:<hostPort>`** (container → distro loopback) rather than
   `0.0.0.0`, so the port is never exposed on the LAN and collisions surface at docker-run time, not silently later.
3. After start, the daemon **probes `127.0.0.1:<hostPort>` from Windows** with a TCP connect + protocol handshake; if the
   probe fails, retry once after re-reading `wsl -d <distro> hostname -I` and dialling `<vmIP>:<hostPort>`, and only then
   surface a precise error ("port forwarding unavailable — mirrored mode / firewall / port taken by <owner>").
4. Treat `netsh portproxy` and mirrored mode as **documented, user-consented** remediation, not silent app behaviour.
5. Because the port is machine-wide, the daemon (single control plane, ADR-0003) should own a port registry with
   lease/expiry so a crashed project doesn't leak a port.

---

## 5. Host folder access

* `\\wsl.localhost\<Distro>` (formerly `\\wsl$`) is a 9P server served from the distro; it is how Windows apps reach Linux
  files. `/mnt/c` is DrvFs over 9P. <https://learn.microsoft.com/en-us/windows/wsl/filesystems>
* Microsoft's guidance is unambiguous: keep files in the Linux filesystem when the toolchain is Linux; cross-OS access is
  a compatibility affordance, not a performance path. The long-standing tracker issue for `/mnt` being far slower than
  ext4 is microsoft/WSL#4197 (845 reactions, open).
* node_modules: 9P/DrvFs metadata operations (stat/open/rename storms) dominate `npm install`; a Windows-hosted
  `node_modules` consumed from Linux is typically several times slower and can break on case-sensitivity and symlinks.
  Keep `node_modules` inside ext4.
* Postgres data: **must** be inside ext4. DrvFs cannot honour Linux ownership/mode/`fsync` semantics without
  `metadata` mounted options, is case-insensitive by default, and Docker refuses/behaves badly on such volumes.
* inotify: file watchers **do not see Windows-side changes** across 9P/DrvFs (microsoft/WSL#5424, #2507; watch-count
  limits are also low by default, microsoft/WSL#1705, #4293). Any dev server watching files that a Windows editor writes
  must poll, or the watcher must live inside the distro watching ext4.
* Recommendation: **no bind-mount of the Windows project tree into the runtime.** Canonical location = ext4 inside the
  project distro (`/home/agentify/<project>`), surfaced to Windows IDEs via `\\wsl.localhost\<distro>\home\agentify\<project>`.
  Provide explicit *sync* (rsync/unison or `wsl --export`) only for import/export and backup, with a diff/summary step.
  If a Windows path must be consumed, mount it read-only (`/mnt/c/...`, `ro` in `/etc/fstab`) and say so in the UI.

---

## 6. Lifecycle: stop, delete, disks, memory, autostart, cleanup

| Operation | Command | Scope / effect |
|---|---|---|
| Stop one project | `wsl -t <Name>` / `wsl.exe --terminate <Name>` | Kills that distro's processes. Kernel, VM, other distros unaffected. All containers inside die. |
| Stop everything | `wsl --shutdown` | Kills the whole utility VM (all distros + Docker Desktop's distros). **Never** call this from the app. |
| Delete a project | `wsl --unregister <Name>` | Removes registration **and** the root filesystem/VHD. Irreversible. |
| Move / back up | `wsl --export <Name> <f> --format vhd` then `wsl --import-in-place` | Documented move flow (FAQ). |
| Grow disk | `wsl --manage <Name> --resize 200GB` (WSL ≥ 2.5), else `diskpart` + `resize2fs` | Default VHD max is **1 TB** (`defaultVhdSize`; 512 GB before WSL 0.58.0, 256 GB before that). |
| Compaction | see below | VHDs grow and never shrink by themselves. |

VHD location: `HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss` → `BasePath`; default install root
`%LocalAppData%\wsl` (`[general] distributionInstallPath`). <https://learn.microsoft.com/en-us/windows/wsl/disk-space>

**Sparse VHD / compaction — important, verified:** on WSL 2.6.3.0,
`wsl --manage <Name> --set-sparse true` **refuses**:

```
Sparse VHD support is currently disabled due to potential data corruption.
To force a distribution to use a sparse vhd, please run:
wsl.exe --manage <DistributionName> --set-sparse true --allow-unsafe
```

`[experimental] sparseVhd=true` in `.wslconfig` only affects *newly created* VHDs and is experimental/off by default.
Tracked by microsoft/WSL#4699 (1422 reactions) and #4103. So the safe compaction path for the app is
**export → unregister → import-in-place** (or `diskpart` `select vdisk file=… / compact vdisk` with the distro
terminated; `Optimize-VHD` needs the Hyper-V module, absent on Home). Do **not** offer `--allow-unsafe` in the UI without
a corruption warning and a backup.

**RAM.** `[wsl2] memory` defaults to **50 % of host RAM**; `.wslconfig` is machine-global, so per-project limits are
impossible there. Idle behaviour: `[general] instanceIdleTimeout` = **15000 ms**, `[wsl2] vmIdleTimeout` = **60000 ms**;
`[experimental] autoMemoryReclaim` defaults to `dropCache` (options `disabled|gradual|dropCache`). Memory is not returned
promptly in practice (microsoft/WSL#4166, 452 reactions). Per-project accounting should read the distro's cgroup
(`/sys/fs/cgroup/<distro cgroup>/memory.current`, `cpu.stat`) with `isolateDistroCgroup=true`, and enforce with
`memory.max`/`cpu.max` set at distro start.

**Auto-start at login.** WSL has **no per-distro autostart**. Options, in order of preference:
1. The app's own daemon (Tauri/Rust) starts on login (Run key / scheduled task) and, for projects marked "running",
   launches a long-lived supervisor `wsl -d <Name> -- /usr/local/bin/agentify-supervise` (§7).
2. `systemd=true` + a unit inside the distro — but note a distro's systemd only runs while the distro is running; a
   distro starts when *something* launches it (i.e. still needs 1).
3. `/etc/wsl.conf [boot] command="…"` runs as root at every distro start (Windows 11+) — good for `mkfifo`/socket prep,
   not for autostart at login.

**Cleanup guarantees.** `wsl --unregister` removes registration + VHD, but leaves: the install directory (delete it
yourself), `/mnt/wsl` shared mounts, the WSL VM (still running for other distros), port-tracker entries, and any exported
temporary VHDs. A "delete project" must therefore be: terminate → verify no listeners → unregister → delete dir → verify
`wsl -l -v` → release port leases → delete volumes/tokens. If a distro is mid-crash, the VHD can be left **read-only**
("An error occurred mounting the distribution disk, it was mounted read-only as a fallback") and requires the documented
`wsl --mount <vhdx> --vhd --bare` + `wsl sudo e2fsck -f /dev/sdX` + `wsl --unmount` repair.

---

## 7. Exec and streaming

Launch shape (2.6.x):

```powershell
wsl.exe -d <Name> -u root --cd /home/agentify/proj -- /usr/bin/docker compose up
```

* **Exit codes are propagated** [measured: `sh -c "exit 42"` → `$LASTEXITCODE` 42]. Caveat: `wsl.exe` itself returns WSL
  error codes for infrastructure failures, so distinguish "child exited N" from "Wsl/Service/… error" by parsing stderr.
* **stdout and stderr are separate handles** [measured: redirecting `1>` and `2>` to different files captured `O` and `E`
  separately; merging into one pipe merges them]. Wire two pipes.
* **No PTY unless attached to a console.** `wsl.exe` allocates a ConPTY only when its own stdio is a console, so an app
  spawning it with pipes gets plain pipes: no TTY, no window-size, no `isatty()` (TUIs like `psql`, `vim`, `top` will
  misbehave). For real PTY semantics, run a tiny in-distro PTY server (a `pty`-based helper over a Unix socket or TCP on
  `127.0.0.1`) and speak to it directly; `wsl.exe` is not a terminal multiplexer.
* **Signals.** With a console, Ctrl-C reaches the Linux foreground process group. From a pipe-based child, `wsl.exe` is
  the forwarder and signal propagation is best-effort; the robust path is to send signals *inside*:
  `wsl -d <Name> -- kill -TERM <pid>` (record PIDs yourself), or have the supervisor relay.
* **Long-lived processes — the sharpest operational edge [measured].** A process started via
  `wsl -d X -- sh -c "nohup … &"` or `setsid` **does not survive** the launching `wsl.exe` exiting: the listener was gone
  seconds after the launcher returned (verified three ways: `nohup`, `setsid`, and subshell `( … & )`). Pattern that works:
  keep a **foreground** `wsl.exe` child per project whose payload is a supervisor/PID-1-ish process, e.g.
  `wsl -d <Name> -- /usr/local/bin/agentify-supervise` which itself starts dockerd/systemd units and streams logs; the
  Windows daemon holds that child's handle for the project's lifetime (and can then `wsl -t` to hard-stop). Optionally
  `systemd=true` + `systemctl start` and keep a keeper (`wsl -d X -- tail -f /dev/null`) so the distro isn't reaped.
* Startup latency budget [measured]: warm exec `122 ms`; exec after `wsl -t` `453 ms`; import of a 3.5 MB rootfs
  `0.71 s` producing a 76 MiB VHD. Cold VM boot (first distro after `wsl --shutdown`/reboot) adds seconds (see §8).

---

## 8. Failure modes, detection, remediation

| Failure | Symptom | Detect (non-elevated) | Fix |
|---|---|---|---|
| Virtualization off in BIOS | `0x80370102` "The virtual machine could not be started because a required feature is not installed" / `0x80070003` | `HypervisorPresent` False, `InstallState(VirtualMachinePlatform)` may be 1 | enable SVM/VT-x in firmware; enable "Virtual Machine Platform"; reboot; check `bcdedit /enum \| findstr hypervisorlaunchtype` (elevated). Docs: <https://learn.microsoft.com/en-us/windows/wsl/troubleshooting> |
| WSL feature missing | `WslRegisterDistribution failed with error: 0x8007019e`, `wsl` not recognized | `(Get-Command wsl.exe)` fails; `Win32_OptionalFeature … Microsoft-Windows-Subsystem-Linux` = 2/3 | elevated `wsl --install --no-distribution` (or `dism … VirtualMachinePlatform`), reboot |
| Corporate policy blocks WSL | `Error: 0x800704ec This program is blocked by group policy` | none locally; only at first launch | `wsl --update` (Store build is not policy-blocked in the same way); otherwise app must declare unsupported. FAQ: <https://learn.microsoft.com/en-us/windows/wsl/faq> |
| Corporate firewall policy | no DNS in WSL; NAT can't reach shared access | `Get-NetFirewallProfile -PolicyStore ActiveStore` → `AllowLocalFirewallRules`/`AllowInboundRules` = False | set `dnsTunneling=true` (default on 22H2+); docs also require `generateResolvConf` not disabled |
| ICS/SharedAccess disabled | WSL2 creation fails, "no more endpoints available from the endpoint mapper" | `Get-Service SharedAccess` | must stay Manual (Trigger Start); policy must not disable ICS |
| Antivirus | no network in WSL (Kaspersky, AVG, Avast, Symantec EP, F-Secure listed) and/or slow VHD I/O | none programmatic | process exclusions for `wslservice.exe`, `wslrelay.exe`, `vmmemWSL`, and the distro directories |
| Windows update | WSL optional feature disabled → `0x80040154` after update; HNS firewall rule stale → DNS breaks | probe again on every app start; `wsl --version` drift | re-enable feature; `Restart-Service hns` / re-create HNS rules; `wsl --shutdown` after network stack changes |
| Distro corruption | read-only fallback mount; `ext4.vhdx` inconsistent after power loss; corruption reports in microsoft/WSL#5895 | mount error text at start | terminate all, `wsl --mount <vhdx> --vhd --bare`, `wsl sudo e2fsck -f /dev/sdX`, `wsl --unmount`; keep per-project `--export` backups |
| Port forwarding silently dead | `localhost:<p>` refuses though the listener is up | daemon TCP probe after start (see §4) | mirrored mode / Hyper-V firewall rule / `ignoredPorts`; or dial `<vm IP>:<port>` |
| Cold start & RAM | first launch after reboot is slow; `vmmemWSL` holds memory | `Get-Process vmmemWSL`; measure first-exec latency | keep the VM warm (one keeper), `autoMemoryReclaim=gradual`, warn on per-project RAM in UI |

Measured cost model to seed the UI [measured on 13.9 GB / 5800H, VM capped at 6 GB]: VHD floor **76 MiB** per project;
Ubuntu-base rootfs **29 MiB** download; warm command **~120 ms**; distro (re)start **~450 ms**; import **<1 s** for a
minimal rootfs. Add image pulls and DB first-boot on top; those dominate the perceived cold start, not WSL.

---

## Recommended decisions (summary)

1. Ubuntu base 24.04 rootfs, downloaded and SHA-verified at provision time; `systemd=true` in `wsl.conf`; per-project
   dir under `%LOCALAPPDATA%\Agentify\distros\<slug>` (or a user-chosen drive), VHD never touched by Windows tools.
2. Rootful `dockerd` supervised by an in-distro supervisor process; containers ephemeral, state in volumes; images by digest.
3. Central port registry in the daemon; publish to `127.0.0.1:<leasedPort>` inside the distro; verify from Windows with a
   probe; mirrored mode / portproxy as consented remediation only.
4. Project files live on ext4; expose via `\\wsl.localhost\<distro>`; sync only for import/export; document the poll-based
   watcher requirement.
5. Enforce per-project resources with cgroups; never edit `.wslconfig` silently; never call `wsl --shutdown`.
6. Lifecycle: keeper child per running project, `wsl -t` to stop, `--unregister` + dir delete to destroy, export-then-
   import-in-place for compaction (avoid `--allow-unsafe`).
7. Re-probe the environment (features, virtualization, WSL version, port forwarding) on every app start; WSL updates itself
   automatically, so version drift is normal, not exceptional.

## ADR conflict to raise

`docs/adr/0001-per-project-isolation-boundary.md` claims "a real kernel boundary". On Windows/WSL2 you get a shared kernel,
a shared utility VM, and a **shared network namespace** across all project distros (verified). If a kernel- or
network-level boundary is a product promise, WSL2-per-distro does not deliver it on Windows and the ADR needs a documented
Windows-specific weakening (or a per-project VM, which Windows Home cannot host outside WSL/Hyper-V).
