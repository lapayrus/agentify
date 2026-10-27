//! Approvals and the activity feed.
//!
//! A destructive operation does not run when it is requested: it becomes an
//! [`ApprovalView`] a human decides on, after an interactive OS gesture. The
//! caller receives `202` with the approval and the operation id, and waits on
//! the operation instead of retrying.
//!
//! Every mutation also lands in the audit chain; [`ActivityPage`] is that chain
//! as the API serves it. Values are never recorded, only identities, targets,
//! parameter hashes, and outcomes.

use serde::{Deserialize, Serialize};

/// An Approval nobody has decided yet.
pub const APPROVAL_PENDING: &str = "pending";
/// A human authorised the operation; it has run, or is running.
pub const APPROVAL_APPROVED: &str = "approved";
/// A human refused. The target is unchanged.
pub const APPROVAL_DENIED: &str = "denied";
/// Nobody decided before the approval's deadline.
pub const APPROVAL_EXPIRED: &str = "expired";

/// Operation state while it waits for a human: not terminal, so a waiter keeps
/// waiting.
pub const OPERATION_WAITING_FOR_APPROVAL: &str = "waiting_for_approval";

/// Who did something: an Agent token, or a human at this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorIdentity {
    /// `agent` or `human`.
    pub kind: String,
    /// Stable id: the token id for an Agent, the OS account name for a human.
    pub id: String,
    /// The token the request came in on, when there was one.
    #[serde(rename = "tokenId", skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
}

impl ActorIdentity {
    /// The Agent behind a token id.
    pub fn agent(token_id: &str) -> Self {
        Self {
            kind: "agent".to_string(),
            id: token_id.to_string(),
            token_id: Some(token_id.to_string()),
        }
    }

    /// The human at the console who passed the OS gesture.
    pub fn human(account: &str, token_id: Option<&str>) -> Self {
        Self {
            kind: "human".to_string(),
            id: account.to_string(),
            token_id: token_id.map(str::to_string),
        }
    }
}

/// One named parameter of the operation an Approval covers. Never a value of a
/// secret: only what the operation was asked to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterView {
    pub name: String,
    pub value: String,
}

/// An Approval, as the UI and the API show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalView {
    pub id: String,
    /// `delete_project` or `reset_environment`.
    pub operation: String,
    /// What the operation would change: a Project name, or `<project>/<env>`.
    pub target: String,
    /// SHA-256 over the operation and its parameters. The Approval authorises
    /// exactly this.
    #[serde(rename = "paramsHash")]
    pub params_hash: String,
    /// The parameters in a form a human can judge.
    pub parameters: Vec<ParameterView>,
    /// The Agent that asked.
    pub requester: ActorIdentity,
    #[serde(rename = "operationId")]
    pub operation_id: String,
    #[serde(rename = "projectId", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// `pending`, `approved`, `denied`, or `expired`.
    pub state: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// After this instant the Approval cannot be used.
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
    #[serde(rename = "decidedAt", skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(rename = "decidedBy", skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<ActorIdentity>,
}

impl ApprovalView {
    pub fn is_pending(&self) -> bool {
        self.state == APPROVAL_PENDING
    }
}

/// `GET /v1/approvals`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalList {
    pub approvals: Vec<ApprovalView>,
}

/// The `202` body a destructive call returns: the Approval to decide on, and
/// the operation to wait on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalAccepted {
    #[serde(rename = "approvalId")]
    pub approval_id: String,
    #[serde(rename = "operationId")]
    pub operation_id: String,
    /// `delete_project` or `reset_environment`.
    pub operation: String,
    pub target: String,
    #[serde(rename = "paramsHash")]
    pub params_hash: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
}

/// Body of `POST /v1/approvals/{id}/approve`. Every field is optional; when a
/// caller supplies one it must agree with the Approval, or the decision is
/// refused: a yes covers exactly what the human was shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecisionRequest {
    /// The operation the caller believes it is approving.
    #[serde(default, rename = "operationId", skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// The parameter hash the caller was shown.
    #[serde(default, rename = "paramsHash", skip_serializing_if = "Option::is_none")]
    pub params_hash: Option<String>,
    /// Why a human refused. Recorded in the audit, never a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What a decision produced: the Approval's new state and its operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub approval: ApprovalView,
    pub operation: crate::environment::OperationView,
}

/// One row of the append-only, hash-chained audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// Chain position, 1-based and gapless.
    pub seq: i64,
    pub at: String,
    pub actor: ActorIdentity,
    #[serde(rename = "projectId", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// The operation that ran, was requested, or was decided.
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(rename = "paramsHash", skip_serializing_if = "Option::is_none")]
    pub params_hash: Option<String>,
    #[serde(rename = "approvalId", skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// `requested`, `approved`, `denied`, `expired`, `succeeded`, or `failed`.
    pub result: String,
    #[serde(rename = "errorCode", skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(rename = "requestId")]
    pub request_id: String,
    #[serde(rename = "prevHash")]
    pub prev_hash: String,
    /// SHA-256 over this entry and `prev_hash`.
    pub hash: String,
}

/// The result of walking the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainIntegrity {
    /// True when every entry hashes to its own `hash` and links to its
    /// predecessor.
    pub verified: bool,
    /// How many entries the walk covered.
    pub entries: i64,
    #[serde(rename = "checkedAt")]
    pub checked_at: String,
    /// The first entry that did not verify, when one did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `GET /v1/activity`: the chain, newest first, plus the verdict of a full walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityPage {
    pub entries: Vec<ActivityEntry>,
    pub integrity: ChainIntegrity,
}
