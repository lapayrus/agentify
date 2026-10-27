//! SQLite state: embedded, bundled, WAL mode.
//!
//! The daemon is the only writer. Calls are short and local, so a single
//! connection behind a mutex is enough; there is no connection pool to tune and
//! no external database to install.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::auth;
use crate::clock;

/// Schema version the running binary expects.
const SCHEMA_VERSION: i64 = 7;

/// Ordered migrations. Each entry is applied exactly once, inside a
/// transaction, and recorded in `schema_migrations`.
const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        r#"
    CREATE TABLE instance (
        id         TEXT PRIMARY KEY,
        created_at TEXT NOT NULL
    );

    CREATE TABLE agent_tokens (
        id         TEXT PRIMARY KEY,
        token_hash TEXT NOT NULL UNIQUE,
        created_at TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        revoked_at TEXT,
        scopes     TEXT NOT NULL
    );
    "#,
    ),
    (
        2,
        r#"
    -- A Project is a Manifest plus at least one Environment. The daemon mints
    -- the id and owns the workspace path; the name is the Project's identity.
    CREATE TABLE projects (
        id             TEXT PRIMARY KEY,
        name           TEXT NOT NULL UNIQUE,
        source_kind    TEXT NOT NULL,
        source_name    TEXT,
        spec_json      TEXT NOT NULL,
        workspace_path TEXT NOT NULL,
        created_at     TEXT NOT NULL
    );

    -- v0 recognises only `dev`, and creating a Project creates it.
    CREATE TABLE environments (
        id         TEXT PRIMARY KEY,
        project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        name       TEXT NOT NULL,
        created_at TEXT NOT NULL,
        UNIQUE (project_id, name)
    );

    -- Resources are stored per Environment, keyed by the name the Manifest uses.
    CREATE TABLE resources (
        id             TEXT PRIMARY KEY,
        project_id     TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
        name           TEXT NOT NULL,
        kind           TEXT NOT NULL,
        created_at     TEXT NOT NULL,
        UNIQUE (environment_id, name)
    );

    CREATE INDEX resources_by_project ON resources (project_id);
    "#,
    ),
    (
        3,
        r#"
    -- Desired state is what the user or an Agent asked for; observed state is
    -- what the daemon last saw. They diverge after a daemon restart, which is
    -- why both are stored rather than one state column.
    ALTER TABLE environments ADD COLUMN desired_state  TEXT NOT NULL DEFAULT 'stopped';
    ALTER TABLE environments ADD COLUMN observed_state TEXT NOT NULL DEFAULT 'stopped';
    ALTER TABLE environments ADD COLUMN health         TEXT NOT NULL DEFAULT 'unknown';
    ALTER TABLE environments ADD COLUMN updated_at     TEXT NOT NULL DEFAULT '';

    -- A port lease is machine-wide: one row per (Environment, Resource, guest
    -- port), and one host port may belong to exactly one row.
    CREATE TABLE port_leases (
        id             TEXT PRIMARY KEY,
        environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
        resource       TEXT NOT NULL,
        guest_port     INTEGER NOT NULL,
        host_port      INTEGER NOT NULL,
        created_at     TEXT NOT NULL,
        UNIQUE (environment_id, resource, guest_port)
    );

    CREATE UNIQUE INDEX port_leases_host_port ON port_leases (host_port);
    CREATE INDEX port_leases_by_environment ON port_leases (environment_id);

    -- A mutation that outlives its request timeout still has to be finishable.
    CREATE TABLE operations (
        id             TEXT PRIMARY KEY,
        kind           TEXT NOT NULL,
        project_id     TEXT NOT NULL,
        environment_id TEXT NOT NULL,
        state          TEXT NOT NULL,
        created_at     TEXT NOT NULL,
        updated_at     TEXT NOT NULL,
        finished_at    TEXT,
        error_code     TEXT,
        error_detail   TEXT,
        result_json    TEXT
    );

    CREATE INDEX operations_by_environment ON operations (environment_id);

    -- `Idempotency-Key` on a mutation replays the original response for 24h.
    CREATE TABLE idempotency_records (
        key          TEXT PRIMARY KEY,
        method       TEXT NOT NULL,
        path         TEXT NOT NULL,
        request_hash TEXT NOT NULL,
        status       INTEGER NOT NULL,
        body         TEXT NOT NULL,
        created_at   TEXT NOT NULL
    );
    CREATE INDEX idempotency_by_created ON idempotency_records (created_at);
    "#,
    ),
    (
        6,
        r#"
    -- An Approval is a human decision about exactly one destructive operation.
    -- It is single use, bound to the operation's parameter hash, and expires.
    -- Nothing about it is a secret: identities, targets, and hashes only.
    CREATE TABLE approvals (
        id                TEXT PRIMARY KEY,
        operation_id      TEXT NOT NULL,
        operation         TEXT NOT NULL,
        project_id        TEXT,
        project_name      TEXT NOT NULL,
        environment_id    TEXT,
        target            TEXT NOT NULL,
        params_json       TEXT NOT NULL,
        params_hash       TEXT NOT NULL,
        requester_kind    TEXT NOT NULL,
        requester_id      TEXT NOT NULL,
        requester_token   TEXT,
        state             TEXT NOT NULL,
        created_at        TEXT NOT NULL,
        expires_at        TEXT NOT NULL,
        decided_at        TEXT,
        decided_kind      TEXT,
        decided_id        TEXT,
        decided_token     TEXT
    );

    CREATE INDEX approvals_by_state ON approvals (state, created_at);
    CREATE INDEX approvals_by_project ON approvals (project_id);

    -- The audit is append-only: every mutation appends one row, and each row
    -- hashes the row before it. The triggers stop the daemon from rewriting
    -- history; the hash chain is what detects someone who disables them.
    CREATE TABLE audit (
        seq         INTEGER PRIMARY KEY AUTOINCREMENT,
        at          TEXT NOT NULL,
        actor_kind  TEXT NOT NULL,
        actor_id    TEXT NOT NULL,
        token_id    TEXT,
        project_id  TEXT,
        operation   TEXT NOT NULL,
        target      TEXT,
        params_hash TEXT,
        approval_id TEXT,
        result      TEXT NOT NULL,
        error_code  TEXT,
        duration_ms INTEGER,
        request_id  TEXT NOT NULL,
        prev_hash   TEXT NOT NULL,
        hash        TEXT NOT NULL
    );

    CREATE INDEX audit_by_project ON audit (project_id, seq);

    CREATE TRIGGER audit_is_append_only_on_update
    BEFORE UPDATE ON audit
    BEGIN
        SELECT RAISE(ABORT, 'the audit log is append-only');
    END;

    CREATE TRIGGER audit_is_append_only_on_delete
    BEFORE DELETE ON audit
    BEGIN
        SELECT RAISE(ABORT, 'the audit log is append-only');
    END;
    "#,
    ),
    (
        7,
        r#"
    -- A Project's secret *values* live in the OS credential manager, never here.
    -- This table is metadata only: which Project has which secret, where the value
    -- came from (`generate`, `fromEnv`, or `literal`), and when it was written.
    -- There is no column a value could be stored in, so the state database is
    -- safe to copy and there is nothing to leak.
    CREATE TABLE secrets_metadata (
        project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        name        TEXT NOT NULL,
        source_kind TEXT NOT NULL,
        source_ref  TEXT,
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL,
        PRIMARY KEY (project_id, name)
    );
    "#,
    ),
];

/// Agent tokens expire after seven days by default (ADR-0003).
const TOKEN_LIFETIME_DAYS: i64 = 7;

/// A token row, minus the hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRecord {
    pub id: String,
    pub scopes: Vec<String>,
    pub expires_at: String,
}

/// A freshly minted token. The plaintext leaves the daemon exactly once: into
/// the discovery file.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub id: String,
    pub token: String,
    pub scopes: Vec<String>,
    pub expires_at: String,
}

/// Stable identity of a data root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub id: String,
    pub created_at: String,
}

/// An Environment row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentRecord {
    pub id: String,
    pub name: String,
    /// What the last mutation asked for: `running` or `stopped`.
    pub desired_state: String,
    /// What the daemon last observed.
    pub observed_state: String,
    /// `healthy`, `unhealthy`, or `unknown`, decided by the daemon's probes.
    pub health: String,
    pub updated_at: String,
}

/// A stored state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateUpdate {
    pub desired_state: String,
    pub observed_state: String,
    pub health: String,
}

/// One row of the machine-wide port registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRecord {
    pub environment_id: String,
    pub resource: String,
    pub guest_port: u16,
    pub host_port: u16,
    pub created_at: String,
}

/// A stored operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRecord {
    pub id: String,
    pub kind: String,
    pub project_id: String,
    pub environment_id: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub error_code: Option<String>,
    pub error_detail: Option<String>,
    pub result_json: Option<String>,
}

/// A replayed response for an `Idempotency-Key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyRecord {
    pub key: String,
    pub method: String,
    pub path: String,
    pub request_hash: String,
    pub status: u16,
    pub body: String,
    pub created_at: String,
}

/// A stored Approval: the human decision a destructive operation waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub id: String,
    pub operation_id: String,
    /// `delete_project` or `reset_environment`.
    pub operation: String,
    pub project_id: Option<String>,
    pub project_name: String,
    pub environment_id: Option<String>,
    /// What the operation acts on, as a human reads it.
    pub target: String,
    /// The parameters as JSON: the operation and its target, never a value.
    pub params_json: String,
    /// SHA-256 over the operation and its parameters.
    pub params_hash: String,
    pub requester: ActorRecord,
    /// `pending`, `approved`, `denied`, or `expired`.
    pub state: String,
    pub created_at: String,
    pub expires_at: String,
    pub decided_at: Option<String>,
    pub decided_by: Option<ActorRecord>,
}

/// A human or Agent identity, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorRecord {
    pub kind: String,
    pub id: String,
    pub token_id: Option<String>,
}

/// The decision a human made, and who made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub state: String,
    pub decided_at: String,
    pub decided_by: ActorRecord,
}

/// One audit row on its way in. The hash is computed by [`crate::audit`], so the
/// store never invents one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAuditEntry {
    pub at: String,
    pub actor: ActorRecord,
    pub project_id: Option<String>,
    pub operation: String,
    pub target: Option<String>,
    pub params_hash: Option<String>,
    pub approval_id: Option<String>,
    pub result: String,
    pub error_code: Option<String>,
    pub duration_ms: Option<u64>,
    pub request_id: String,
    pub prev_hash: String,
    pub hash: String,
}

/// An audit row as it was stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub seq: i64,
    pub entry: NewAuditEntry,
}


/// A Resource row: identity and kind. The details live in the Project's spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRecord {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// A Project as stored: rows plus the normalized spec JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub source_kind: String,
    pub source_name: Option<String>,
    pub spec_json: String,
    pub workspace_path: String,
    pub created_at: String,
    pub environment: EnvironmentRecord,
    pub resources: Vec<ResourceRecord>,
}

/// What a caller supplies to create a Project. The daemon mints every id and
/// every daemon-owned name before it gets here.
#[derive(Debug, Clone)]
pub struct NewProject {
    pub id: String,
    pub name: String,
    pub source_kind: String,
    pub source_name: Option<String>,
    pub spec_json: String,
    pub workspace_path: String,
    pub created_at: String,
    pub environment_id: String,
    pub environment_name: String,
    /// `(id, name, kind)` per Resource.
    pub resources: Vec<(String, String, String)>,
    /// The secrets the Manifest declared. Metadata only: the values go to the
    /// credential manager.
    pub secrets: Vec<NewSecret>,
}

/// A secret's metadata on its way in. There is no value field on purpose: the
/// only place a value is stored is the OS credential manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSecret {
    pub name: String,
    /// `generate`, `fromEnv`, or `literal`.
    pub source_kind: String,
    /// The host variable a `fromEnv` import came from.
    pub source_ref: Option<String>,
}

/// A secret's metadata as it was stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRecord {
    pub project_id: String,
    pub name: String,
    pub source_kind: String,
    pub source_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct State {
    conn: Mutex<Connection>,
}

impl State {
    /// Open (creating and migrating if needed) the state database under a data
    /// root.
    pub fn open(data_root: &Path) -> Result<Self> {
        let path = agentify_proto::state_db_path(data_root);
        let conn = Connection::open(&path)
            .with_context(|| format!("opening the state database {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// Open an in-memory database. Used by unit tests only.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")
            .or_else(|_| conn.pragma_update(None, "journal_mode", "MEMORY"))
            .context("enabling WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;

        let state = Self {
            conn: Mutex::new(conn),
        };
        state.migrate()?;
        Ok(state)
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow::anyhow!("the state database lock is poisoned"))?;
        f(&conn)
    }

    fn migrate(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                     version    INTEGER PRIMARY KEY,
                     applied_at TEXT NOT NULL
                 );",
            )?;

            for (version, sql) in MIGRATIONS {
                let applied: bool = conn
                    .query_row(
                        "SELECT 1 FROM schema_migrations WHERE version = ?1",
                        [version],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if applied {
                    continue;
                }
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(sql)
                    .with_context(|| format!("applying migration {version}"))?;
                tx.execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    rusqlite::params![version, clock::now_rfc3339()],
                )?;
                tx.commit()?;
            }

            let applied: i64 =
                conn.query_row("SELECT COALESCE(MAX(version), 0) FROM schema_migrations", [], |row| {
                    row.get(0)
                })?;
            anyhow::ensure!(
                applied == SCHEMA_VERSION,
                "the state database is at schema version {applied} but this daemon expects {SCHEMA_VERSION}"
            );
            Ok(())
        })
    }

    /// The instance id of this data root, minting one on first start.
    pub fn instance(&self) -> Result<Instance> {
        self.with_conn(|conn| {
            if let Some(instance) = conn
                .query_row("SELECT id, created_at FROM instance LIMIT 1", [], |row| {
                    Ok(Instance {
                        id: row.get(0)?,
                        created_at: row.get(1)?,
                    })
                })
                .optional()?
            {
                return Ok(instance);
            }
            let instance = Instance {
                id: uuid::Uuid::new_v4().to_string(),
                created_at: clock::now_rfc3339(),
            };
            conn.execute(
                "INSERT INTO instance (id, created_at) VALUES (?1, ?2)",
                rusqlite::params![instance.id, instance.created_at],
            )?;
            Ok(instance)
        })
    }

    /// Mint an Agent-scoped token and revoke every token issued before it. Each
    /// daemon run therefore hands out exactly one live token.
    pub fn issue_agent_token(&self) -> Result<IssuedToken> {
        let token = auth::random_token()?;
        let issued = IssuedToken {
            id: uuid::Uuid::new_v4().to_string(),
            scopes: vec![
                agentify_proto::SCOPE_READ.to_string(),
                agentify_proto::SCOPE_OPERATE.to_string(),
            ],
            expires_at: clock::rfc3339_in_days(TOKEN_LIFETIME_DAYS),
            token: token.clone(),
        };
        let hash = auth::sha256_hex(&token);
        let scopes = issued.scopes.join(",");
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE agent_tokens SET revoked_at = ?1 WHERE revoked_at IS NULL",
                rusqlite::params![clock::now_rfc3339()],
            )?;
            tx.execute(
                "INSERT INTO agent_tokens (id, token_hash, created_at, expires_at, revoked_at, scopes)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                rusqlite::params![
                    issued.id,
                    hash,
                    clock::now_rfc3339(),
                    issued.expires_at,
                    scopes
                ],
            )?;
            tx.commit()?;
            Ok(())
        })?;
        Ok(issued)
    }

    /// Look up a live token by the SHA-256 of its plaintext.
    pub fn find_active_token(&self, token_hash: &str) -> Result<Option<TokenRecord>> {
        self.with_conn(|conn| {
            let record = conn
                .query_row(
                    "SELECT id, scopes, expires_at FROM agent_tokens
                     WHERE token_hash = ?1 AND revoked_at IS NULL",
                    [token_hash],
                    |row| {
                        Ok(TokenRecord {
                            id: row.get(0)?,
                            scopes: split_scopes(row.get::<_, String>(1)?),
                            expires_at: row.get(2)?,
                        })
                    },
                )
                .optional()?;
            Ok(record)
        })
    }

    /// Revoke the token with this id, if it is still live. Returns `true` when a
    /// row changed and `false` when there was nothing left to revoke.
    pub fn revoke_token(&self, token_id: &str) -> Result<bool> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE agent_tokens SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                rusqlite::params![clock::now_rfc3339(), token_id],
            )?;
            Ok(changed > 0)
        })
    }

    /// Insert a Project with its Environment and Resources in one transaction.
    ///
    /// Returns `Ok(false)` when the name is already taken, so the caller can
    /// report `AlreadyExists` without racing on a unique-constraint message.
    pub fn create_project(&self, project: NewProject) -> Result<bool> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;

            let taken: bool = tx
                .query_row(
                    "SELECT 1 FROM projects WHERE name = ?1",
                    [&project.name],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if taken {
                return Ok(false);
            }

            tx.execute(
                "INSERT INTO projects (id, name, source_kind, source_name, spec_json, workspace_path, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    project.id,
                    project.name,
                    project.source_kind,
                    project.source_name,
                    project.spec_json,
                    project.workspace_path,
                    project.created_at,
                ],
            )?;
            tx.execute(
                "INSERT INTO environments (id, project_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    project.environment_id,
                    project.id,
                    project.environment_name,
                    project.created_at,
                ],
            )?;
            for (id, name, kind) in &project.resources {
                tx.execute(
                    "INSERT INTO resources (id, project_id, environment_id, name, kind, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        id,
                        project.id,
                        project.environment_id,
                        name,
                        kind,
                        project.created_at,
                    ],
                )?;
            }
            // The Manifest's secrets are metadata here; their values were
            // resolved and stored in the credential manager before this call.
            for secret in &project.secrets {
                tx.execute(
                    "INSERT INTO secrets_metadata
                         (project_id, name, source_kind, source_ref, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    rusqlite::params![
                        project.id,
                        secret.name,
                        secret.source_kind,
                        secret.source_ref,
                        project.created_at,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(true)
        })
    }

    /// Every Project, oldest first.
    pub fn list_projects(&self) -> Result<Vec<ProjectRecord>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT p.id, p.name, p.source_kind, p.source_name, p.spec_json,
                        p.workspace_path, p.created_at, e.id, e.name,
                        e.desired_state, e.observed_state, e.health, e.updated_at
                 FROM projects p
                 JOIN environments e ON e.project_id = p.id AND e.name = ?1
                 ORDER BY p.created_at, p.name",
            )?;
            let rows = statement
                .query_map([agentify_proto::DEV_ENVIRONMENT], project_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .map(|record| hydrate_resources(conn, record))
                .collect()
        })
    }

    /// How many Projects exist, for the abuse cap.
    pub fn count_projects(&self) -> Result<i64> {
        self.with_conn(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))?;
            Ok(count)
        })
    }

    /// A Project by name or by id.
    pub fn find_project(&self, reference: &str) -> Result<Option<ProjectRecord>> {
        self.with_conn(|conn| {
            let record = conn
                .query_row(
                    "SELECT p.id, p.name, p.source_kind, p.source_name, p.spec_json,
                            p.workspace_path, p.created_at, e.id, e.name,
                            e.desired_state, e.observed_state, e.health, e.updated_at
                     FROM projects p
                     JOIN environments e ON e.project_id = p.id AND e.name = ?2
                     WHERE p.name = ?1 OR p.id = ?1
                     LIMIT 1",
                    rusqlite::params![reference, agentify_proto::DEV_ENVIRONMENT],
                    project_row,
                )
                .optional()?;
            match record {
                Some(record) => Ok(Some(hydrate_resources(conn, record)?)),
                None => Ok(None),
            }
        })
    }

    /// Record a state transition. Short and local: no await happens while the
    /// connection is held.
    pub fn set_environment_state(&self, environment_id: &str, update: &StateUpdate) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE environments
                 SET desired_state = ?2, observed_state = ?3, health = ?4, updated_at = ?5
                 WHERE id = ?1",
                rusqlite::params![
                    environment_id,
                    update.desired_state,
                    update.observed_state,
                    update.health,
                    clock::now_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    /// Every Environment with its stored state, for startup reconciliation.
    pub fn list_environments(&self) -> Result<Vec<EnvironmentRecord>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, name, desired_state, observed_state, health, updated_at
                 FROM environments ORDER BY created_at, id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(EnvironmentRecord {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        desired_state: row.get(2)?,
                        observed_state: row.get(3)?,
                        health: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// One Environment's stored state.
    pub fn environment(&self, id: &str) -> Result<Option<EnvironmentRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, name, desired_state, observed_state, health, updated_at
                     FROM environments WHERE id = ?1",
                    [id],
                    |row| {
                        Ok(EnvironmentRecord {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            desired_state: row.get(2)?,
                            observed_state: row.get(3)?,
                            health: row.get(4)?,
                            updated_at: row.get(5)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    // ---- port leases -------------------------------------------------------

    /// Every lease for one Environment, ordered by Resource then guest port.
    pub fn leases(&self, environment_id: &str) -> Result<Vec<LeaseRecord>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT environment_id, resource, guest_port, host_port, created_at
                 FROM port_leases WHERE environment_id = ?1
                 ORDER BY resource, guest_port",
            )?;
            let rows = statement
                .query_map([environment_id], lease_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Every lease on the machine, so a caller can name the holder of a port.
    pub fn all_leases(&self) -> Result<Vec<LeaseRecord>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT environment_id, resource, guest_port, host_port, created_at
                 FROM port_leases ORDER BY host_port",
            )?;
            let rows = statement
                .query_map([], lease_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Who holds a host port, if anyone.
    pub fn lease_holder(&self, host_port: u16) -> Result<Option<LeaseRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT environment_id, resource, guest_port, host_port, created_at
                     FROM port_leases WHERE host_port = ?1",
                    [host_port],
                    lease_row,
                )
                .optional()?)
        })
    }

    /// Put an Environment's lease for one guest port in place, replacing any
    /// previous row for it. Returns `false` when the host port belongs to
    /// another Environment, or to another Resource of this one: one host port is
    /// exactly one lease.
    pub fn put_lease(&self, lease: &LeaseRecord) -> Result<bool> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let holder: Option<(String, String, u16)> = tx
                .query_row(
                    "SELECT environment_id, resource, guest_port FROM port_leases WHERE host_port = ?1",
                    [lease.host_port],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((environment_id, resource, guest_port)) = holder {
                let same_lease = environment_id == lease.environment_id
                    && resource == lease.resource
                    && guest_port == lease.guest_port;
                if !same_lease {
                    return Ok(false);
                }
            }
            tx.execute(
                "DELETE FROM port_leases
                 WHERE environment_id = ?1 AND resource = ?2 AND guest_port = ?3",
                rusqlite::params![lease.environment_id, lease.resource, lease.guest_port],
            )?;
            tx.execute(
                "INSERT INTO port_leases (id, environment_id, resource, guest_port, host_port, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    lease.environment_id,
                    lease.resource,
                    lease.guest_port,
                    lease.host_port,
                    lease.created_at,
                ],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// Release every lease an Environment holds.
    pub fn release_leases(&self, environment_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM port_leases WHERE environment_id = ?1",
                [environment_id],
            )?;
            Ok(())
        })
    }

    // ---- operations --------------------------------------------------------

    pub fn create_operation(&self, operation: &OperationRecord) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO operations (id, kind, project_id, environment_id, state,
                                         created_at, updated_at, finished_at, error_code,
                                         error_detail, result_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL, NULL)",
                rusqlite::params![
                    operation.id,
                    operation.kind,
                    operation.project_id,
                    operation.environment_id,
                    operation.state,
                    operation.created_at,
                    operation.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    /// Finish an operation. Idempotent: a second finish leaves the first result.
    /// An operation waiting for an Approval is finishable too, so a denial or an
    /// expiry releases whoever is waiting on it.
    pub fn finish_operation(
        &self,
        id: &str,
        state: &str,
        error_code: Option<&str>,
        error_detail: Option<&str>,
        result_json: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let now = clock::now_rfc3339();
            conn.execute(
                "UPDATE operations
                 SET state = ?2, updated_at = ?3, finished_at = ?3,
                     error_code = ?4, error_detail = ?5, result_json = ?6
                 WHERE id = ?1 AND state IN ('running', 'waiting_for_approval')",
                rusqlite::params![id, state, now, error_code, error_detail, result_json],
            )?;
            Ok(())
        })
    }

    /// Move an operation that was waiting for an Approval back to running, so a
    /// reader never sees a decided Approval beside a waiting operation.
    pub fn start_operation(&self, id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE operations SET state = 'running', updated_at = ?2
                 WHERE id = ?1 AND state = 'waiting_for_approval'",
                rusqlite::params![id, clock::now_rfc3339()],
            )?;
            Ok(())
        })
    }

    pub fn operation(&self, id: &str) -> Result<Option<OperationRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, kind, project_id, environment_id, state, created_at,
                            updated_at, finished_at, error_code, error_detail, result_json
                     FROM operations WHERE id = ?1",
                    [id],
                    |row| {
                        Ok(OperationRecord {
                            id: row.get(0)?,
                            kind: row.get(1)?,
                            project_id: row.get(2)?,
                            environment_id: row.get(3)?,
                            state: row.get(4)?,
                            created_at: row.get(5)?,
                            updated_at: row.get(6)?,
                            finished_at: row.get(7)?,
                            error_code: row.get(8)?,
                            error_detail: row.get(9)?,
                            result_json: row.get(10)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    /// Is any mutation still in flight? A running operation and an operation
    /// waiting for a human Approval both count: neither has finished, so an
    /// update must not apply while either is open.
    pub fn has_running_operations(&self) -> Result<bool> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM operations WHERE state IN ('running', 'waiting_for_approval')
                 )",
                [],
                |row| row.get(0),
            )?)
        })
    }

    // ---- idempotency -------------------------------------------------------

    pub fn idempotency(&self, key: &str) -> Result<Option<IdempotencyRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT key, method, path, request_hash, status, body, created_at
                     FROM idempotency_records WHERE key = ?1",
                    [key],
                    |row| {
                        Ok(IdempotencyRecord {
                            key: row.get(0)?,
                            method: row.get(1)?,
                            path: row.get(2)?,
                            request_hash: row.get(3)?,
                            status: row.get(4)?,
                            body: row.get(5)?,
                            created_at: row.get(6)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn put_idempotency(&self, record: &IdempotencyRecord) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO idempotency_records
                     (key, method, path, request_hash, status, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    record.key,
                    record.method,
                    record.path,
                    record.request_hash,
                    record.status,
                    record.body,
                    record.created_at,
                ],
            )?;
            Ok(())
        })
    }

    /// Drop replayed responses older than the cutoff (`now - 24h`).
    pub fn purge_idempotency_before(&self, cutoff: &str) -> Result<usize> {
        self.with_conn(|conn| {
            let removed =
                conn.execute("DELETE FROM idempotency_records WHERE created_at < ?1", [cutoff])?;
            Ok(removed)
        })
    }

    // ---- delete ------------------------------------------------------------

    /// Delete a Project and everything the database hangs off it. Foreign keys
    /// cascade to its Environments, Resources, and port leases, and the explicit
    /// deletes keep that guarantee visible even if a caller turns foreign keys
    /// off. Operations and Approvals survive on purpose: a caller may still be
    /// waiting on one, and the audit trail is not a thing a delete erases.
    ///
    /// Returns whether a Project row was actually removed.
    pub fn delete_project(&self, project_id: &str) -> Result<bool> {
        self.with_conn(|conn| {
            let transaction = conn.unchecked_transaction()?;
            transaction.execute(
                "DELETE FROM port_leases WHERE environment_id IN
                     (SELECT id FROM environments WHERE project_id = ?1)",
                [project_id],
            )?;
            transaction.execute("DELETE FROM resources WHERE project_id = ?1", [project_id])?;
            transaction.execute("DELETE FROM environments WHERE project_id = ?1", [project_id])?;
            let removed = transaction.execute("DELETE FROM projects WHERE id = ?1", [project_id])?;
            transaction.commit()?;
            Ok(removed > 0)
        })
    }

    // ---- approvals ---------------------------------------------------------

    pub fn create_approval(&self, approval: &ApprovalRecord) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO approvals (id, operation_id, operation, project_id, project_name,
                                        environment_id, target, params_json, params_hash,
                                        requester_kind, requester_id, requester_token, state,
                                        created_at, expires_at, decided_at, decided_kind,
                                        decided_id, decided_token)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                         NULL, NULL, NULL, NULL)",
                rusqlite::params![
                    approval.id,
                    approval.operation_id,
                    approval.operation,
                    approval.project_id,
                    approval.project_name,
                    approval.environment_id,
                    approval.target,
                    approval.params_json,
                    approval.params_hash,
                    approval.requester.kind,
                    approval.requester.id,
                    approval.requester.token_id,
                    approval.state,
                    approval.created_at,
                    approval.expires_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn approval(&self, id: &str) -> Result<Option<ApprovalRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!("{APPROVAL_SELECT} WHERE id = ?1"),
                    [id],
                    approval_row,
                )
                .optional()?)
        })
    }

    /// Approvals in a state, newest first. `None` means every state.
    pub fn approvals(&self, state: Option<&str>, limit: usize) -> Result<Vec<ApprovalRecord>> {
        self.with_conn(|conn| {
            let mut rows = Vec::new();
            match state {
                Some(state) => {
                    let mut statement = conn.prepare(&format!(
                        "{APPROVAL_SELECT} WHERE state = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2"
                    ))?;
                    for row in statement.query_map(rusqlite::params![state, limit as i64], approval_row)? {
                        rows.push(row?);
                    }
                }
                None => {
                    let mut statement = conn.prepare(&format!(
                        "{APPROVAL_SELECT} ORDER BY created_at DESC, id DESC LIMIT ?1"
                    ))?;
                    for row in statement.query_map([limit as i64], approval_row)? {
                        rows.push(row?);
                    }
                }
            }
            Ok(rows)
        })
    }

    /// Decide a pending Approval. Returns `false` when it was already decided:
    /// the `state = 'pending'` guard is what makes an Approval single use.
    pub fn decide_approval(&self, id: &str, decision: &ApprovalDecision) -> Result<bool> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE approvals
                 SET state = ?2, decided_at = ?3, decided_kind = ?4, decided_id = ?5,
                     decided_token = ?6
                 WHERE id = ?1 AND state = 'pending'",
                rusqlite::params![
                    id,
                    decision.state,
                    decision.decided_at,
                    decision.decided_by.kind,
                    decision.decided_by.id,
                    decision.decided_by.token_id,
                ],
            )?;
            Ok(changed > 0)
        })
    }

    /// Expire every Approval whose deadline has passed, and say which ones.
    pub fn expire_due_approvals(&self, now: &str) -> Result<Vec<ApprovalRecord>> {
        self.with_conn(|conn| {
            let due = {
                let mut statement = conn.prepare(&format!(
                    "{APPROVAL_SELECT} WHERE state = 'pending' AND expires_at <= ?1"
                ))?;
                let rows = statement
                    .query_map([now], approval_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            for approval in &due {
                conn.execute(
                    "UPDATE approvals SET state = 'expired', decided_at = ?2
                     WHERE id = ?1 AND state = 'pending'",
                    rusqlite::params![approval.id, now],
                )?;
            }
            Ok(due)
        })
    }

    // ---- secrets -----------------------------------------------------------

    /// A Project's secrets, by name. Metadata only: the values are elsewhere.
    pub fn secrets(&self, project_id: &str) -> Result<Vec<SecretRecord>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(&format!(
                "{SECRET_SELECT} WHERE project_id = ?1 ORDER BY name"
            ))?;
            let rows = statement
                .query_map([project_id], secret_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// One secret's metadata, by Project and name.
    pub fn find_secret(&self, project_id: &str, name: &str) -> Result<Option<SecretRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!("{SECRET_SELECT} WHERE project_id = ?1 AND name = ?2"),
                    rusqlite::params![project_id, name],
                    secret_row,
                )
                .optional()?)
        })
    }

    /// Record a secret's metadata. Returns `false` when the name is already
    /// taken, so the caller can report `AlreadyExists` without racing on a
    /// unique-constraint message.
    pub fn insert_secret(&self, secret: &SecretRecord) -> Result<bool> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "INSERT OR IGNORE INTO secrets_metadata
                     (project_id, name, source_kind, source_ref, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    secret.project_id,
                    secret.name,
                    secret.source_kind,
                    secret.source_ref,
                    secret.created_at,
                    secret.updated_at,
                ],
            )?;
            Ok(changed > 0)
        })
    }

    /// Replace a secret's metadata after its value changed. The creation time
    /// stays put: a Project's history is not rewritten by a value update.
    pub fn update_secret(&self, secret: &SecretRecord) -> Result<bool> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE secrets_metadata
                 SET source_kind = ?3, source_ref = ?4, updated_at = ?5
                 WHERE project_id = ?1 AND name = ?2",
                rusqlite::params![
                    secret.project_id,
                    secret.name,
                    secret.source_kind,
                    secret.source_ref,
                    secret.updated_at,
                ],
            )?;
            Ok(changed > 0)
        })
    }

    /// Forget a secret's metadata. The value is deleted from the store by the
    /// caller, which is what makes the order matter: a row without a value is
    /// recoverable, a value without a row is not.
    pub fn delete_secret(&self, project_id: &str, name: &str) -> Result<bool> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "DELETE FROM secrets_metadata WHERE project_id = ?1 AND name = ?2",
                rusqlite::params![project_id, name],
            )?;
            Ok(changed > 0)
        })
    }

    // ---- audit -------------------------------------------------------------

    /// The hash of the newest audit entry, or the genesis hash when there is
    /// none.
    pub fn last_audit_hash(&self) -> Result<String> {
        self.with_conn(|conn| {
            let hash: Option<String> = conn
                .query_row("SELECT hash FROM audit ORDER BY seq DESC LIMIT 1", [], |row| {
                    row.get(0)
                })
                .optional()?;
            Ok(hash.unwrap_or_else(|| crate::audit::GENESIS.to_string()))
        })
    }

    /// Append one audit row and return its chain position.
    pub fn append_audit(&self, entry: &NewAuditEntry) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO audit (at, actor_kind, actor_id, token_id, project_id, operation,
                                    target, params_hash, approval_id, result, error_code,
                                    duration_ms, request_id, prev_hash, hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    entry.at,
                    entry.actor.kind,
                    entry.actor.id,
                    entry.actor.token_id,
                    entry.project_id,
                    entry.operation,
                    entry.target,
                    entry.params_hash,
                    entry.approval_id,
                    entry.result,
                    entry.error_code,
                    entry.duration_ms,
                    entry.request_id,
                    entry.prev_hash,
                    entry.hash,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Audit rows, newest first, optionally narrowed to one Project.
    pub fn audit_rows(&self, limit: usize, project_id: Option<&str>) -> Result<Vec<AuditRow>> {
        self.with_conn(|conn| {
            let mut rows = Vec::new();
            match project_id {
                Some(project_id) => {
                    let mut statement = conn.prepare(&format!(
                        "{AUDIT_SELECT} WHERE project_id = ?1 ORDER BY seq DESC LIMIT ?2"
                    ))?;
                    for row in
                        statement.query_map(rusqlite::params![project_id, limit as i64], audit_row)?
                    {
                        rows.push(row?);
                    }
                }
                None => {
                    let mut statement = conn.prepare(&format!(
                        "{AUDIT_SELECT} ORDER BY seq DESC LIMIT ?1"
                    ))?;
                    for row in statement.query_map([limit as i64], audit_row)? {
                        rows.push(row?);
                    }
                }
            }
            Ok(rows)
        })
    }

    /// Every audit row, oldest first: the chain, for verification.
    pub fn audit_chain(&self) -> Result<Vec<AuditRow>> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare(&format!("{AUDIT_SELECT} ORDER BY seq ASC"))?;
            let rows = statement
                .query_map([], audit_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

const APPROVAL_SELECT: &str = "SELECT id, operation_id, operation, project_id, project_name,
        environment_id, target, params_json, params_hash, requester_kind, requester_id,
        requester_token, state, created_at, expires_at, decided_at, decided_kind, decided_id,
        decided_token FROM approvals";

const AUDIT_SELECT: &str = "SELECT seq, at, actor_kind, actor_id, token_id, project_id, operation,
        target, params_hash, approval_id, result, error_code, duration_ms, request_id, prev_hash,
        hash FROM audit";

const SECRET_SELECT: &str = "SELECT project_id, name, source_kind, source_ref, created_at,
        updated_at FROM secrets_metadata";

fn secret_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SecretRecord> {
    Ok(SecretRecord {
        project_id: row.get(0)?,
        name: row.get(1)?,
        source_kind: row.get(2)?,
        source_ref: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn approval_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
    let decided_kind: Option<String> = row.get(16)?;
    let decided_id: Option<String> = row.get(17)?;
    let decided_token: Option<String> = row.get(18)?;
    Ok(ApprovalRecord {
        id: row.get(0)?,
        operation_id: row.get(1)?,
        operation: row.get(2)?,
        project_id: row.get(3)?,
        project_name: row.get(4)?,
        environment_id: row.get(5)?,
        target: row.get(6)?,
        params_json: row.get(7)?,
        params_hash: row.get(8)?,
        requester: ActorRecord {
            kind: row.get(9)?,
            id: row.get(10)?,
            token_id: row.get(11)?,
        },
        state: row.get(12)?,
        created_at: row.get(13)?,
        expires_at: row.get(14)?,
        decided_at: row.get(15)?,
        decided_by: match (decided_kind, decided_id) {
            (Some(kind), Some(id)) => Some(ActorRecord {
                kind,
                id,
                token_id: decided_token,
            }),
            _ => None,
        },
    })
}

fn audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRow> {
    Ok(AuditRow {
        seq: row.get(0)?,
        entry: NewAuditEntry {
            at: row.get(1)?,
            actor: ActorRecord {
                kind: row.get(2)?,
                id: row.get(3)?,
                token_id: row.get(4)?,
            },
            project_id: row.get(5)?,
            operation: row.get(6)?,
            target: row.get(7)?,
            params_hash: row.get(8)?,
            approval_id: row.get(9)?,
            result: row.get(10)?,
            error_code: row.get(11)?,
            duration_ms: row.get(12)?,
            request_id: row.get(13)?,
            prev_hash: row.get(14)?,
            hash: row.get(15)?,
        },
    })
}

fn lease_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseRecord> {
    Ok(LeaseRecord {
        environment_id: row.get(0)?,
        resource: row.get(1)?,
        guest_port: row.get(2)?,
        host_port: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRecord> {
    Ok(ProjectRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        source_kind: row.get(2)?,
        source_name: row.get(3)?,
        spec_json: row.get(4)?,
        workspace_path: row.get(5)?,
        created_at: row.get(6)?,
        environment: EnvironmentRecord {
            id: row.get(7)?,
            name: row.get(8)?,
            desired_state: row.get(9)?,
            observed_state: row.get(10)?,
            health: row.get(11)?,
            updated_at: row.get(12)?,
        },
        resources: Vec::new(),
    })
}

fn hydrate_resources(
    conn: &Connection,
    mut record: ProjectRecord,
) -> Result<ProjectRecord> {
    let mut statement =
        conn.prepare("SELECT id, name, kind FROM resources WHERE project_id = ?1 ORDER BY name")?;
    record.resources = statement
        .query_map([&record.id], |row| {
            Ok(ResourceRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                kind: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(record)
}

fn split_scopes(raw: String) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_applied_once_and_the_instance_id_is_stable() {
        let first = State::open_in_memory().expect("state");
        let instance = first.instance().expect("instance");
        let again = first.instance().expect("instance");
        assert_eq!(instance, again);

        let second = State::open_in_memory().expect("state");
        assert_ne!(second.instance().expect("instance").id, instance.id);
    }

    #[test]
    fn issuing_a_token_revokes_the_previous_one() {
        let state = State::open_in_memory().expect("state");
        let first = state.issue_agent_token().expect("token");
        let first_hash = auth::sha256_hex(&first.token);
        assert!(state.find_active_token(&first_hash).expect("lookup").is_some());

        let second = state.issue_agent_token().expect("token");
        assert!(
            state.find_active_token(&first_hash).expect("lookup").is_none(),
            "the previous run's token must not keep working"
        );
        let record = state
            .find_active_token(&auth::sha256_hex(&second.token))
            .expect("lookup")
            .expect("the new token is live");
        assert_eq!(record.scopes, vec!["read", "operate"]);
    }

    #[test]
    fn revoking_a_token_removes_it() {
        let state = State::open_in_memory().expect("state");
        let issued = state.issue_agent_token().expect("token");
        let hash = auth::sha256_hex(&issued.token);
        assert!(state.find_active_token(&hash).expect("lookup").is_some());

        assert!(state.revoke_token(&issued.id).expect("revoke"));
        assert!(
            state.find_active_token(&hash).expect("lookup").is_none(),
            "a revoked token must not be live"
        );
        assert!(
            !state.revoke_token(&issued.id).expect("revoke"),
            "revoking the same token twice is a no-op"
        );
    }
}
