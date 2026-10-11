use aionui_api_types::{ExternalConversationDispatchWorkspaceLease, ExternalWorkspaceOwnership};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    workspace_id: String,
    job_id: String,
    lease_id: String,
    project_root: String,
    conversation_id: String,
    ownership: ExternalWorkspaceOwnership,
}

// Remote leases require a durable, local Runner reservation before any runtime rebind.
// Old local leases remain compatible; a remote ownership claim can never bypass this check.
pub(crate) async fn validate_session(
    lease: &ExternalConversationDispatchWorkspaceLease,
    conversation_id: Option<&str>,
    allow_unbound: bool,
) -> Result<(), ()> {
    let Some(ownership) = &lease.ownership else {
        return Ok(());
    };
    let root = std::path::Path::new(&lease.project_root);
    if !root.is_absolute() || ownership.generation == 0 {
        return Err(());
    }
    let canonical = tokio::fs::canonicalize(root).await.map_err(|_| ())?;
    let bytes = tokio::fs::read(root.join(".ai-session.json")).await.map_err(|_| ())?;
    let session: Session = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let session_root = tokio::fs::canonicalize(&session.project_root).await.map_err(|_| ())?;
    if canonical != session_root
        || session.workspace_id != lease.workspace_id
        || session.job_id != lease.job_id
        || session.lease_id != lease.lease_id
        || session.ownership != *ownership
        || !(allow_unbound && session.conversation_id.is_empty()
            || conversation_id == Some(session.conversation_id.as_str()))
    {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(root: &str) -> ExternalConversationDispatchWorkspaceLease {
        ExternalConversationDispatchWorkspaceLease {
            workspace_id: "fork1".to_owned(),
            job_id: "job-1".to_owned(),
            lease_id: "lease-1".to_owned(),
            project_root: root.to_owned(),
            ownership: Some(ExternalWorkspaceOwnership {
                coordinator_id: "mnp-main".to_owned(),
                machine_id: "macbook".to_owned(),
                pool_id: "unity".to_owned(),
                generation: 1,
            }),
        }
    }

    #[tokio::test]
    async fn rejects_missing_foreign_conversation_and_stale_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_str().unwrap();
        let expected = lease(root);
        assert!(validate_session(&expected, Some("conversation-1"), false).await.is_err());
        let session = serde_json::json!({
            "workspaceId": "fork1", "jobId": "job-1", "leaseId": "lease-1",
            "projectRoot": root, "conversationId": "conversation-1", "ownership": expected.ownership.clone(),
        });
        tokio::fs::write(directory.path().join(".ai-session.json"), session.to_string()).await.unwrap();
        assert!(validate_session(&expected, Some("conversation-1"), false).await.is_ok());
        assert!(validate_session(&expected, Some("other"), false).await.is_err());
        let mut stale = expected.clone();
        stale.ownership.as_mut().unwrap().generation = 2;
        assert!(validate_session(&stale, Some("conversation-1"), false).await.is_err());
    }

    #[tokio::test]
    async fn allows_unbound_new_dispatch_and_preserves_legacy_local_contract() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_str().unwrap();
        let mut expected = lease(root);
        let session = serde_json::json!({
            "workspaceId": "fork1", "jobId": "job-1", "leaseId": "lease-1",
            "projectRoot": root, "conversationId": "", "ownership": expected.ownership.clone(),
        });
        tokio::fs::write(directory.path().join(".ai-session.json"), session.to_string()).await.unwrap();
        assert!(validate_session(&expected, None, true).await.is_ok());
        assert!(validate_session(&expected, Some("conversation-1"), false).await.is_err());
        expected.ownership = None;
        assert!(validate_session(&expected, None, false).await.is_ok());
    }
}
