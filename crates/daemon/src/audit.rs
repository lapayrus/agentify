//! The append-only, hash-chained audit.
//!
//! Every mutation appends one entry. Each entry hashes its own content together
//! with the previous entry's hash, so rewriting or removing history breaks the
//! chain and a read says so. It is detectable, not tamper-proof: someone who can
//! write the database file can also drop the append-only triggers, and the chain
//! is what catches them.
//!
//! Entries carry identities, targets, parameter *hashes*, and outcomes. Values
//! never reach the audit: not a secret, not a command's output, not a log line.

use agentify_proto::{ActivityEntry, ActivityPage, ActorIdentity, ChainIntegrity};

use crate::auth;
use crate::clock;
use crate::state::{ActorRecord, AuditRow, NewAuditEntry, State};

/// The `prev_hash` of the first entry: 32 zero bytes, hex.
pub const GENESIS: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// Separates hash material fields, so no field can be shifted into another.
const FIELD_SEPARATOR: char = '\u{1f}';

/// One mutation to record.
#[derive(Debug, Clone)]
pub struct Append<'a> {
    pub actor: ActorRecord,
    /// The Project the mutation touched, when there is one.
    pub project_id: Option<&'a str>,
    pub operation: &'a str,
    pub target: Option<&'a str>,
    pub params_hash: Option<&'a str>,
    pub approval_id: Option<&'a str>,
    /// `requested`, `approved`, `denied`, `expired`, `succeeded`, or `failed`.
    pub result: &'a str,
    pub error_code: Option<&'a str>,
    pub duration_ms: Option<u64>,
    pub request_id: &'a str,
}

impl Append<'_> {
    /// A succeeded mutation with no error and no duration yet.
    pub fn done<'a>(actor: ActorRecord, operation: &'a str, request_id: &'a str) -> Append<'a> {
        Append {
            actor,
            project_id: None,
            operation,
            target: None,
            params_hash: None,
            approval_id: None,
            result: "succeeded",
            error_code: None,
            duration_ms: None,
            request_id,
        }
    }
}

/// SHA-256 over one entry and the hash before it.
pub fn hash_entry(prev_hash: &str, entry: &NewAuditEntry) -> String {
    let token_id = entry.actor.token_id.clone().unwrap_or_default();
    let duration = entry.duration_ms.map(|ms| ms.to_string()).unwrap_or_default();
    let fields = [
        prev_hash.to_string(),
        entry.at.clone(),
        entry.actor.kind.clone(),
        entry.actor.id.clone(),
        token_id,
        entry.project_id.clone().unwrap_or_default(),
        entry.operation.clone(),
        entry.target.clone().unwrap_or_default(),
        entry.params_hash.clone().unwrap_or_default(),
        entry.approval_id.clone().unwrap_or_default(),
        entry.result.clone(),
        entry.error_code.clone().unwrap_or_default(),
        duration,
        entry.request_id.clone(),
    ];
    auth::sha256_hex(&fields.join(&FIELD_SEPARATOR.to_string()))
}

/// Append one entry to the chain.
pub fn append(state: &State, entry: Append<'_>) -> anyhow::Result<ActivityEntry> {
    let prev_hash = state.last_audit_hash()?;
    let mut row = NewAuditEntry {
        at: clock::now_rfc3339(),
        actor: ActorRecord {
            kind: entry.actor.kind,
            id: entry.actor.id,
            token_id: entry.actor.token_id,
        },
        project_id: entry.project_id.map(str::to_string),
        operation: entry.operation.to_string(),
        target: entry.target.map(str::to_string),
        params_hash: entry.params_hash.map(str::to_string),
        approval_id: entry.approval_id.map(str::to_string),
        result: entry.result.to_string(),
        error_code: entry.error_code.map(str::to_string),
        duration_ms: entry.duration_ms,
        request_id: entry.request_id.to_string(),
        prev_hash: prev_hash.clone(),
        hash: String::new(),
    };
    row.hash = hash_entry(&prev_hash, &row);
    let seq = state.append_audit(&row)?;
    Ok(view(AuditRow { seq, entry: row }))
}

/// `GET /v1/activity`: the newest entries, plus the verdict of a full walk.
pub fn page(
    state: &State,
    limit: usize,
    project_id: Option<&str>,
) -> anyhow::Result<ActivityPage> {
    let entries = state
        .audit_rows(limit, project_id)?
        .into_iter()
        .map(view)
        .collect();
    Ok(ActivityPage {
        entries,
        integrity: verify(state)?,
    })
}

/// Walk the whole chain and say whether it still holds together.
pub fn verify(state: &State) -> anyhow::Result<ChainIntegrity> {
    let chain = state.audit_chain()?;
    let mut expected_prev = GENESIS.to_string();
    for row in &chain {
        if row.entry.prev_hash != expected_prev {
            return Ok(ChainIntegrity {
                verified: false,
                entries: chain.len() as i64,
                checked_at: clock::now_rfc3339(),
                detail: Some(format!(
                    "entry {} does not link to the entry before it",
                    row.seq
                )),
            });
        }
        let recomputed = hash_entry(&row.entry.prev_hash, &row.entry);
        if recomputed != row.entry.hash {
            return Ok(ChainIntegrity {
                verified: false,
                entries: chain.len() as i64,
                checked_at: clock::now_rfc3339(),
                detail: Some(format!("entry {} was altered after it was written", row.seq)),
            });
        }
        expected_prev = row.entry.hash.clone();
    }
    Ok(ChainIntegrity {
        verified: true,
        entries: chain.len() as i64,
        checked_at: clock::now_rfc3339(),
        detail: None,
    })
}

fn view(row: AuditRow) -> ActivityEntry {
    ActivityEntry {
        seq: row.seq,
        at: row.entry.at,
        actor: ActorIdentity {
            kind: row.entry.actor.kind,
            id: row.entry.actor.id,
            token_id: row.entry.actor.token_id,
        },
        project_id: row.entry.project_id,
        operation: row.entry.operation,
        target: row.entry.target,
        params_hash: row.entry.params_hash,
        approval_id: row.entry.approval_id,
        result: row.entry.result,
        error_code: row.entry.error_code,
        duration_ms: row.entry.duration_ms,
        request_id: row.entry.request_id,
        prev_hash: row.entry.prev_hash,
        hash: row.entry.hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> ActorRecord {
        ActorRecord {
            kind: "agent".to_string(),
            id: "token-1".to_string(),
            token_id: Some("token-1".to_string()),
        }
    }

    fn append_one(state: &State, operation: &str, result: &str) -> ActivityEntry {
        let mut entry = Append::done(agent(), operation, "req-1");
        entry.result = result;
        super::append(state, entry).expect("append")
    }

    #[test]
    fn entries_chain_and_verify() {
        let state = State::open_in_memory().expect("state");
        let first = append_one(&state, "create_project", "succeeded");
        let second = append_one(&state, "start_environment", "succeeded");
        assert_eq!(first.prev_hash, GENESIS);
        assert_eq!(second.prev_hash, first.hash);
        assert_ne!(first.hash, second.hash);

        let integrity = verify(&state).expect("verify");
        assert!(integrity.verified, "{integrity:?}");
        assert_eq!(integrity.entries, 2);
    }

    #[test]
    fn the_hash_covers_every_field() {
        let entry = NewAuditEntry {
            at: "2026-01-01T00:00:00Z".to_string(),
            actor: agent(),
            project_id: Some("proj-1".to_string()),
            operation: "delete_project".to_string(),
            target: Some("hello-pg".to_string()),
            params_hash: Some("a".repeat(64)),
            approval_id: Some("approval-1".to_string()),
            result: "succeeded".to_string(),
            error_code: None,
            duration_ms: Some(3),
            request_id: "req-1".to_string(),
            prev_hash: GENESIS.to_string(),
            hash: String::new(),
        };
        let base = hash_entry(GENESIS, &entry);

        let mut changed = entry.clone();
        changed.result = "failed".to_string();
        assert_ne!(base, hash_entry(GENESIS, &changed));

        let mut changed = entry.clone();
        changed.target = Some("other".to_string());
        assert_ne!(base, hash_entry(GENESIS, &changed));

        let mut changed = entry.clone();
        changed.duration_ms = Some(4);
        assert_ne!(base, hash_entry(GENESIS, &changed));

        let mut changed = entry;
        changed.actor.id = "token-2".to_string();
        assert_ne!(
            base,
            hash_entry(GENESIS, &changed),
            "the actor is part of what is signed"
        );
    }

    #[test]
    fn an_altered_entry_is_detected() {
        let temp = tempfile::tempdir().expect("temp dir");
        let state = State::open(temp.path()).expect("state");
        append_one(&state, "create_project", "succeeded");
        append_one(&state, "delete_project", "denied");
        assert!(verify(&state).expect("verify").verified);

        // A writer with access to the file can drop the trigger; the chain is
        // what catches the edit that follows.
        let path = agentify_proto::state_db_path(temp.path());
        let conn = rusqlite::Connection::open(&path).expect("open the state database");
        conn.execute_batch(
            "DROP TRIGGER audit_is_append_only_on_update;
             UPDATE audit SET result = 'succeeded' WHERE seq = 2;",
        )
        .expect("tamper");
        drop(conn);

        let integrity = verify(&state).expect("verify");
        assert!(
            !integrity.verified,
            "an altered entry is detected: {integrity:?}"
        );
        assert!(
            integrity.detail.unwrap_or_default().contains('2'),
            "the verdict names the entry"
        );
    }

    #[test]
    fn the_appended_entries_cannot_be_rewritten_through_sql() {
        let temp = tempfile::tempdir().expect("temp dir");
        let state = State::open(temp.path()).expect("state");
        append_one(&state, "create_project", "succeeded");

        let path = agentify_proto::state_db_path(temp.path());
        let conn = rusqlite::Connection::open(&path).expect("open the state database");
        for statement in [
            "UPDATE audit SET result = 'failed' WHERE seq = 1",
            "DELETE FROM audit WHERE seq = 1",
        ] {
            let result = conn.execute(statement, []);
            assert!(
                result.is_err(),
                "the append-only trigger refuses: {statement}"
            );
        }
        assert!(verify(&state).expect("verify").verified);
    }
}
