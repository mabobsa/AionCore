//! Bundled runtime (bun) resolver for aioncore.
//!
//! Embeds the bun runtime at build time (zstd-compressed) and extracts it
//! to the user's OS cache directory on first call. Callers use
//! [`resolve_bun`] to obtain a usable executable path and [`bun_bin_dir`]
//! to prepend the runtime directory to child-process `PATH`.

mod cache;
mod embed;
mod extract;
pub mod node_runtime;
mod resolver;
mod shell_env;

pub use cache::init;
pub use node_runtime::{
    DoctorRow, NodeRuntimeError, NodeRuntimeFailureKind, NodeRuntimeProgress, NodeRuntimeProgressPhase,
    NodeRuntimeProgressReporter, NodeRuntimeSupport, NodeTool, ResolvedCommand, ResolvedNodeRuntime,
    ResolvedNodeSource, RuntimeCommandProbe, SharedNodeRuntimeProgressReporter, doctor_snapshot,
    doctor_snapshot_for_test, ensure_node_runtime, ensure_node_runtime_with_reporter, ensure_runtime_command,
    ensure_runtime_command_with_reporter, probe_node_runtime_supported, probe_runtime_command,
};
pub use resolver::{ResolveError, bun_bin_dir, resolve_bun, resolve_command_in, resolve_command_path};
pub use shell_env::enhance_process_path;
mod spawn;
pub use spawn::{Builder, kill_process_tree};

#[cfg(test)]
#[path = "../build_support.rs"]
mod build_support_tests;
