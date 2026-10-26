# Define data lifecycle

Type: grilling
Status: resolved
Blocked by: 03

## Question

How is Environment data stored, seeded, reset, and destroyed?

Decide volume ownership per Resource, the seed and template strategy for databases, Checkpoint and restore semantics, and whether forking an Environment is in v0.

Cover disk accounting and pruning, what `reset` means for each Resource kind, and exactly what teardown removes. A user must be able to trust that teardown is complete, so state the verification, not just the intent.

State where volume data physically lives on each Backend. On Windows that is ext4 inside the distro, never `/mnt/c`.

## Answer

Data lives inside the Backend, except for Checkpoints, which live on the host.

- Volumes are daemon-named and owned by the Environment. In v0 only database Resources have a volume; app volumes are out of scope.
- `remove` is destructive in v0. `keepVolumes` is an `Unsupported` capability until a later ticket; `stop` is the preservation path. `project.delete` (Approval-gated) removes runtime, volumes, routes, leases, and Checkpoints together.
- Checkpoints are host-side database-native archives: `pg_dump` runs in the runtime and the archive is copied out with `copyOut`. They are backend-neutral. One Checkpoint covers every database Resource in the Environment as a set; cross-engine consistency is best-effort.
- `checkpoint.create` and `checkpoint.delete` are explicit. There are no automatic checkpoints. Sizes are visible in `stats`, and retention is manual.
- `checkpoint.restore` stops dependent Resources, restores, then restarts them. It is Approval-gated because it overwrites current data; ticket 01 is amended.
- `reset` drops and recreates each database, then restarts dependents so entrypoint migrations re-run. It is Approval-gated.
- There is no seed field in the Manifest. Initial data comes from migrations or `exec`; a Checkpoint captures a seeded state.
- Data physically lives on ext4 inside the Windows distro, in Apple container's sparse volume images, and inside the Lima VM. The daemon never assumes a path; it passes volume names only.
- At startup the daemon reconciles: runtimes, volumes, routes, and leases absent from its database are reported, never auto-deleted. Cleanup requires an Approval.
- Teardown verification is part of `remove`: the runtime is unregistered, volumes are gone, the Caddy route is deleted, and the lease is released. Failures are reported rather than swallowed.
