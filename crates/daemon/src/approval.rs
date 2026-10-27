//! Approvals: the human decision a destructive operation waits on.
//!
//! An Approval is bound to a SHA-256 over the operation and its parameters, so a
//! yes covers exactly what the human was shown. It is single use (the store's
//! `state = 'pending'` guard is the whole of that mechanism), it expires, and it
//! is decided only after an interactive OS gesture.

use std::collections::BTreeMap;

use agentify_proto::{ActorIdentity, ApprovalView, ParameterView};

use crate::auth;
use crate::state::{ActorRecord, ApprovalDecision, ApprovalRecord};

/// Remove a Project, its runtime, its volumes, and its rows.
pub const DELETE_PROJECT: &str = "delete_project";
/// Clear an Environment's data and restart it.
pub const RESET_ENVIRONMENT: &str = "reset_environment";

/// What a destructive call is asking for.
#[derive(Debug, Clone)]
pub struct Destructive {
    /// `delete_project` or `reset_environment`.
    pub operation: &'static str,
    /// What a human reads on the card.
    pub target: String,
    /// The parameters the Approval is bound to. Never a value.
    pub parameters: BTreeMap<String, String>,
}

impl Destructive {
    /// The hash an Approval is bound to: the operation and its parameters,
    /// canonically serialized (a `BTreeMap` sorts, so the bytes are stable).
    pub fn params_hash(&self) -> String {
        #[derive(serde::Serialize)]
        struct Material<'a> {
            operation: &'a str,
            target: &'a str,
            parameters: &'a BTreeMap<String, String>,
        }
        let material = serde_json::to_string(&Material {
            operation: self.operation,
            target: &self.target,
            parameters: &self.parameters,
        })
        .unwrap_or_default();
        auth::sha256_hex(&material)
    }

    /// The parameters as JSON, for the stored Approval.
    pub fn params_json(&self) -> String {
        serde_json::to_string(&self.parameters).unwrap_or_else(|_| "{}".to_string())
    }
}

/// The decision to record, from the human who made it.
pub fn decision(state: &str, by: ActorRecord) -> ApprovalDecision {
    ApprovalDecision {
        state: state.to_string(),
        decided_at: crate::clock::now_rfc3339(),
        decided_by: by,
    }
}

/// The wire view of a stored Approval.
pub fn view(record: &ApprovalRecord) -> ApprovalView {
    ApprovalView {
        id: record.id.clone(),
        operation: record.operation.clone(),
        target: record.target.clone(),
        params_hash: record.params_hash.clone(),
        parameters: parameters_of(record),
        requester: actor(&record.requester),
        operation_id: record.operation_id.clone(),
        project_id: record.project_id.clone(),
        state: record.state.clone(),
        created_at: record.created_at.clone(),
        expires_at: record.expires_at.clone(),
        decided_at: record.decided_at.clone(),
        decided_by: record.decided_by.as_ref().map(actor),
    }
}

fn actor(record: &ActorRecord) -> ActorIdentity {
    ActorIdentity {
        kind: record.kind.clone(),
        id: record.id.clone(),
        token_id: record.token_id.clone(),
    }
}

fn parameters_of(record: &ApprovalRecord) -> Vec<ParameterView> {
    serde_json::from_str::<BTreeMap<String, String>>(&record.params_json)
        .map(|parameters| {
            parameters
                .into_iter()
                .map(|(name, value)| ParameterView { name, value })
                .collect()
        })
        .unwrap_or_default()
}

/// The sentence the OS prompt shows. It names the operation and its target, so a
/// human approves something specific rather than "a request".
pub fn prompt(record: &ApprovalRecord) -> String {
    match record.operation.as_str() {
        DELETE_PROJECT => format!(
            "Agentify: delete the Project \"{}\" and its data?",
            record.project_name
        ),
        RESET_ENVIRONMENT => format!(
            "Agentify: reset the Environment \"{}\" and clear its data?",
            record.target
        ),
        other => format!("Agentify: allow {} on {}?", other, record.target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destructive(target: &str, pairs: &[(&str, &str)]) -> Destructive {
        Destructive {
            operation: DELETE_PROJECT,
            target: target.to_string(),
            parameters: pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn the_hash_is_stable_and_parameter_bound() {
        let first = destructive("hello-pg", &[("project", "hello-pg")]);
        let again = destructive("hello-pg", &[("project", "hello-pg")]);
        assert_eq!(first.params_hash(), again.params_hash());
        assert_eq!(first.params_hash().len(), 64);

        assert_ne!(
            first.params_hash(),
            destructive("other", &[("project", "other")]).params_hash(),
            "another target is another Approval"
        );
        assert_ne!(
            first.params_hash(),
            destructive("hello-pg", &[("project", "hello-pg"), ("extra", "1")]).params_hash(),
            "another parameter is another Approval"
        );
    }

    #[test]
    fn the_prompt_names_what_would_be_destroyed() {
        let record = ApprovalRecord {
            id: "approval-1".to_string(),
            operation_id: "op-1".to_string(),
            operation: DELETE_PROJECT.to_string(),
            project_id: Some("proj-1".to_string()),
            project_name: "hello-pg".to_string(),
            environment_id: Some("env-1".to_string()),
            target: "hello-pg".to_string(),
            params_json: "{}".to_string(),
            params_hash: "a".repeat(64),
            requester: ActorRecord {
                kind: "agent".to_string(),
                id: "token-1".to_string(),
                token_id: Some("token-1".to_string()),
            },
            state: agentify_proto::APPROVAL_PENDING.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: "2026-01-01T00:15:00Z".to_string(),
            decided_at: None,
            decided_by: None,
        };
        let prompt = prompt(&record);
        assert!(prompt.contains("delete"), "{prompt}");
        assert!(prompt.contains("hello-pg"), "{prompt}");

        let view = view(&record);
        assert_eq!(view.state, "pending");
        assert!(view.is_pending());
        assert_eq!(view.requester.kind, "agent");
        assert_eq!(view.target, "hello-pg");
    }
}
