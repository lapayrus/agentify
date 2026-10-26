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
const SCHEMA_VERSION: i64 = 1;

/// Ordered migrations. Each entry is applied exactly once, inside a
/// transaction, and recorded in `schema_migrations`.
const MIGRATIONS: &[(i64, &str)] = &[(
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
)];

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
}
