use super::types::{NodeRuntimeError, NodeRuntimeSupport, ResolvedNodeRuntime};

pub fn probe_support() -> NodeRuntimeSupport {
    NodeRuntimeSupport {
        supported: cfg!(target_os = "macos") || cfg!(target_os = "linux") || cfg!(windows),
        detail: "managed node runtime install not implemented yet".into(),
    }
}

pub async fn install_and_validate() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    Err(NodeRuntimeError::system_invalid(
        "managed node runtime install not implemented yet",
    ))
}
