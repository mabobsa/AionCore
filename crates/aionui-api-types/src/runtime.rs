use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatusScopeKind {
    Conversation,
    Mcp,
    CustomAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeStatusScope {
    pub kind: RuntimeStatusScopeKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeResourceKind {
    Node,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatusPhase {
    WaitingForLock,
    Downloading,
    Extracting,
    Validating,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailureKind {
    Timeout,
    DownloadFailed,
    HttpStatus,
    ValidationFailed,
    UnsupportedPlatform,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeStatusPayload {
    pub resource: RuntimeResourceKind,
    pub scope: RuntimeStatusScope,
    pub phase: RuntimeStatusPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<RuntimeFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_status_payload_serializes() {
        let payload = RuntimeStatusPayload {
            resource: RuntimeResourceKind::Node,
            scope: RuntimeStatusScope {
                kind: RuntimeStatusScopeKind::Conversation,
                id: "conv-1".into(),
            },
            phase: RuntimeStatusPhase::Downloading,
            failure_kind: None,
            message: Some("downloading".into()),
            status_code: None,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["resource"], "node");
        assert_eq!(json["scope"]["kind"], "conversation");
        assert_eq!(json["phase"], "downloading");
        assert_eq!(json["message"], "downloading");
    }
}
