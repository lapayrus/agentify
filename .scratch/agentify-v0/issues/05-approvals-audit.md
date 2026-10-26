# 05: Approvals, audit, and activity

**What to build:** Destructive operations become requests that a human authorises with an OS gesture. An Agent receives 202 with an Approval and operation id, waits on the operation, and cannot approve its own request. Every mutation lands in a hash-chained audit and the same activity feed the UI shows.

**Blocked by:** 03 Environment lifecycle on the fake Backend.

**Status:** ready-for-agent

- [ ] `reset_environment` and `delete_project` return 202 with `approval_id` and `operation_id`; the operation proceeds only after approval.
- [ ] Approvals are single-use, bound to a hash of the operation and parameters, and expire after 15 minutes; replay and mismatched parameters are rejected.
- [ ] Only a human session can approve, after an interactive OS gesture; an Agent token gets `ApprovalRequired`.
- [ ] `wait_for_operation` returns the final result without polling loops.
- [ ] The audit is append-only and hash-chained; entries include actor, token, operation, target, parameters hash, and result; reads verify the chain.
- [ ] The UI shows the Approval card with target, requester, parameters, and expiry, plus Approve and Deny; the activity feed shows Agent and human actions with actor identity.
- [ ] Denying an Approval leaves the Project unchanged and is audited.
