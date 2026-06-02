mod managed;
mod system;
mod types;

pub use managed::{install_and_validate as install_managed_runtime, probe_support as probe_node_runtime_supported};
pub use system::{derive_runtime_root, detect_system_runtime, probe_system_runtime, tool_command, validate_same_root};
pub use types::{
    DoctorRow, NodeRuntimeError, NodeRuntimeSupport, NodeTool, ResolvedCommand, ResolvedNodeRuntime,
    ResolvedNodeSource, RuntimeCommandProbe,
};

pub fn probe_runtime_command(command: &str) -> RuntimeCommandProbe {
    let trimmed = command.trim();
    let path = std::path::Path::new(trimmed);

    if path.is_absolute() || trimmed.contains('/') || trimmed.contains('\\') {
        return RuntimeCommandProbe::ExplicitPath {
            path: path.to_path_buf(),
        };
    }

    match trimmed {
        "node" => RuntimeCommandProbe::NodeTool {
            tool: NodeTool::Node,
            command: trimmed.to_owned(),
        },
        "npm" => RuntimeCommandProbe::NodeTool {
            tool: NodeTool::Npm,
            command: trimmed.to_owned(),
        },
        "npx" => RuntimeCommandProbe::NodeTool {
            tool: NodeTool::Npx,
            command: trimmed.to_owned(),
        },
        _ => RuntimeCommandProbe::PathLookup {
            command: trimmed.to_owned(),
        },
    }
}

pub async fn ensure_node_runtime() -> Result<ResolvedNodeRuntime, NodeRuntimeError> {
    match detect_system_runtime().await {
        Ok(runtime) => Ok(runtime),
        Err(_) => install_managed_runtime().await,
    }
}

pub async fn ensure_runtime_command(command: &str) -> Result<ResolvedCommand, NodeRuntimeError> {
    match probe_runtime_command(command) {
        RuntimeCommandProbe::ExplicitPath { path } => Ok(ResolvedCommand::plain(path)),
        RuntimeCommandProbe::PathLookup { command } => crate::resolve_command_path(&command)
            .map(ResolvedCommand::plain)
            .ok_or_else(|| NodeRuntimeError::system_invalid(format!("command '{command}' not found in PATH"))),
        RuntimeCommandProbe::NodeTool { tool, .. } => {
            let runtime = ensure_node_runtime().await?;
            Ok(tool_command(tool, &runtime))
        }
    }
}

pub fn doctor_snapshot() -> Vec<DoctorRow> {
    if let Ok(runtime) = probe_system_runtime() {
        let source = match runtime.source {
            ResolvedNodeSource::System => "system",
            ResolvedNodeSource::Managed => "managed",
        };
        return vec![
            DoctorRow {
                tool: "node".into(),
                source: source.into(),
                detail: runtime.node_path.display().to_string(),
            },
            DoctorRow {
                tool: "npm".into(),
                source: source.into(),
                detail: runtime.npm_path.display().to_string(),
            },
            DoctorRow {
                tool: "npx".into(),
                source: source.into(),
                detail: runtime.npx_path.display().to_string(),
            },
        ];
    }

    let support = probe_node_runtime_supported();
    let source = if support.supported { "managed" } else { "unavailable" };
    vec![
        DoctorRow {
            tool: "node".into(),
            source: source.into(),
            detail: support.detail.clone(),
        },
        DoctorRow {
            tool: "npm".into(),
            source: source.into(),
            detail: support.detail.clone(),
        },
        DoctorRow {
            tool: "npx".into(),
            source: source.into(),
            detail: support.detail,
        },
    ]
}

pub fn doctor_snapshot_for_test(rows: Vec<(&str, &str, &str)>) -> Vec<DoctorRow> {
    rows.into_iter()
        .map(|(tool, source, detail)| DoctorRow {
            tool: tool.into(),
            source: source.into(),
            detail: detail.into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_non_node_command_is_path_only() {
        let probe = probe_runtime_command("sh");
        assert!(matches!(probe, RuntimeCommandProbe::PathLookup { .. }));
    }

    #[test]
    fn probe_bare_node_uses_runtime_probe() {
        let probe = probe_runtime_command("node");
        assert!(matches!(
            probe,
            RuntimeCommandProbe::NodeTool {
                tool: NodeTool::Node,
                ..
            }
        ));
    }

    #[test]
    fn probe_explicit_path_is_passthrough() {
        let probe = probe_runtime_command("/tmp/custom-node");
        assert!(matches!(probe, RuntimeCommandProbe::ExplicitPath { .. }));
    }
}
