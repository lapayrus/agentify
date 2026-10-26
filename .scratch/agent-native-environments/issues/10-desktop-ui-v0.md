# Define the desktop UI v0

Type: prototype
Status: resolved
Blocked by: 07

## Question

What does the human see in v0?

Prototype the screens and decide which ones ship: Project list with per-Project resource cost, Environment detail with Resources and health, live logs, terminal and exec, Manifest editor, secret management, Approval inbox, and audit log.

The UI is a client of the same API as an Agent, so every action on these screens must correspond to an API call. Show how an Agent's activity appears to the human, and how an Approval request interrupts the flow.

## Answer

The chosen structure is a hybrid: **B's header and light visual language over A's three-zone shell**.

Layout:

- Header (from B): brand on the left, a one-line summary on the right (`N Projects · daemon connected · api v1`). No toolbar.
- Left rail (from A): Projects, each row showing state dot, name, and `cpu · memory`. The per-Project cost lives here, always visible.
- Main pane (from A): Project title with Environment badge, URL, a status bar (state, health, resource count, cpu/mem/disk, last actor), Start and Stop actions, `Overview` and `Logs` tabs, and the Resource table (name, kind, state, health, cpu, memory, detail).
- Right side (from A): the Approval card at the top, then the Activity feed. The card names the operation, Project, requester, parameter summary, and expiry, with Approve and Deny.

Screens that ship in v0: Project rail, Environment detail, live logs, Approval inbox, and activity feed. Not shipped: Manifest editor, interactive terminal, secret-management UI, and a separate audit screen.

Behaviour:

- Selecting a Project is `GET /v1/projects/{project}`; Start and Stop are the Environment lifecycle calls; Approve and Deny are `POST /v1/approvals/{id}/approve|deny`; logs use the SSE endpoint with `follow=true`; the rail's `+` is `POST /v1/projects`.
- Agent and human actions appear in the same activity feed, distinguished only by actor; Agent actions are not a separate mode.
- An Approval interrupts without blocking the rest of the UI: the card is persistent, parameter-bound, shows its expiry, and disappears on approve or deny.
- Every action maps to an API call the human could make through the CLI; the UI has no privileged path.
- Empty states: no Projects ("create one from a fixture or Manifest") and no pending Approvals ("Agents continue until an Approval is required").

Rejected: B's three-column layout, because giving Approvals a permanent full column overweights a rare event; C's keyboard-first list, because the human's main job is monitoring and unblocking, not command entry.

Prototype: [`prototype/ui-v0/ui-v0.html`](../../../prototype/ui-v0/ui-v0.html). Variant A is the shell, variant B supplies the header and palette; the hybrid is specified above rather than rendered as a fourth variant.
