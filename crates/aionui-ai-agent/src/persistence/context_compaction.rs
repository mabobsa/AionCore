//! Durable compaction ledger, separate from occupancy and usage freshness.
//!
//! Native history sources verified on 2026-11-01 (metadata only): Claude
//! `.claude-max/projects/C--Git-Holdem-hdtf-client/5ac768c9-9a20-42b5-9720-784ba361feaf.jsonl`
//! system/compact_boundary uuid; Codex 0.164.0
//! `sessions/2026/10/23/rollout-2026-10-23T17-46-00-01a167f0-3a5a-7533-a16a-afebdb06cf00.jsonl`
//! event_msg/item_completed/ContextCompaction id. The latter matches the live
//! ItemCompletedNotification ID, NOT compaction_response_id in `compacted`.

use std::collections::BTreeMap;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use aionui_common::now_ms;
use aionui_db::{IAcpSessionRepository, SaveRuntimeStateParams};
use aionui_session::{SessionConfig, SessionSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub(crate) const META_KEY: &str = "aionui/contextCompaction";

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    count: usize,
    last_compacted_at: Option<i64>,
    history_complete: bool,
    updated_at: i64,
    // Persist stable IDs too, so resume/replay never increments an existing event.
    #[serde(default)]
    events: BTreeMap<String, Option<i64>>,
}

#[derive(Default)]
pub(crate) struct History {
    events: BTreeMap<String, Option<i64>>,
    complete: bool,
}

fn timestamp(value: &Value) -> Option<i64> {
    value
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
}

/// Extract compaction metadata from the owning transcript, excluding child records.
fn read_history(path: &Path, backend: &str) -> io::Result<History> {
    let mut history = History {
        complete: true,
        ..Default::default()
    };
    let file = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut compacted_records = 0;
    let mut completed_records = 0;
    for line in file.lines() {
        let line = line?;
        // Large transcripts contain tool output and summaries; parse only lines
        // that can carry a boundary. Strings mentioning these words still have
        // to pass the structural gates below.
        if !line.contains("compact_boundary") && !line.contains("ContextCompaction") && !line.contains("\"compacted\"")
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            history.complete = false;
            continue;
        };
        let (id, at) = match backend {
            "claude"
                if value["type"] == "system"
                    && value["subtype"] == "compact_boundary"
                    && value["isSidechain"] != true
                    && value.get("parent_tool_use_id").is_none_or(Value::is_null) =>
            {
                (value["uuid"].as_str(), timestamp(&value["timestamp"]))
            }
            "codex" if value["type"] == "compacted" => {
                compacted_records += 1;
                continue;
            }
            "codex"
                if value["type"] == "event_msg"
                    && value["payload"]["type"] == "item_completed"
                    && value["payload"]["item"]["type"] == "ContextCompaction" =>
            {
                completed_records += 1;
                (
                    value["payload"]["item"]["id"].as_str(),
                    value["payload"]["completed_at_ms"]
                        .as_i64()
                        .or_else(|| timestamp(&value["timestamp"])),
                )
            }
            _ => continue,
        };
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            history.events.insert(format!("{backend}:{id}"), at);
        } else {
            history.complete = false;
        }
    }
    // Older/truncated Codex logs may lack completed item IDs. Do not invent
    // aliases for their response IDs: those cannot deduplicate the live stream.
    if backend == "codex" && compacted_records > completed_records {
        history.complete = false;
    }
    Ok(history)
}

fn native_root(backend: &str, config: &SessionConfig) -> Option<PathBuf> {
    let (env, default_dir) = match backend {
        "claude" => ("CLAUDE_CONFIG_DIR", ".claude"),
        "codex" => ("CODEX_HOME", ".codex"),
        _ => return None,
    };
    config
        .spawn_env
        .iter()
        .rev()
        .find(|entry| entry.name.eq_ignore_ascii_case(env))
        .map(|entry| PathBuf::from(&entry.value))
        .or_else(|| std::env::var_os(env).filter(|v| !v.is_empty()).map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|home| home.join(default_dir)))
}

fn find_transcript(root: &Path, sid: &str, depth: usize, backend: &str) -> io::Result<Option<PathBuf>> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let matches = if backend == "claude" {
                name == format!("{sid}.jsonl")
            } else {
                name.ends_with(&format!("-{sid}.jsonl"))
            };
            if matches {
                return Ok(Some(entry.path()));
            }
        } else if kind.is_dir()
            && depth > 0
            && let Some(path) = find_transcript(&entry.path(), sid, depth - 1, backend)?
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Called once before opening a direct backend; no history scans on GET /usage,
/// no extra agent request, and no concurrent pump write during restoration.
pub(crate) async fn restore(
    repo: &dyn IAcpSessionRepository,
    user_id: &str,
    conversation_id: &str,
    backend: &str,
    spec: &SessionSpec,
    config: &SessionConfig,
) {
    let sid = match spec {
        SessionSpec::Fresh { .. } => {
            persist(
                repo,
                user_id,
                conversation_id,
                History {
                    complete: true,
                    ..Default::default()
                },
            )
            .await;
            return;
        }
        SessionSpec::Resume {
            backend_session_id: Some(id),
            ..
        } => id,
        // A fork can target an earlier turn; the parent's full history includes
        // compactions AFTER that point. Restore only once the fork has its own
        // native resume anchor, rather than overstating the inherited count.
        SessionSpec::Fork { .. } => return,
        _ => return,
    };
    // Never interpolate an arbitrary persisted value into a filesystem path.
    if uuid::Uuid::parse_str(sid).is_err() {
        return;
    }
    let Some(root) = native_root(backend, config) else {
        return;
    };
    let backend = backend.to_owned();
    let sid = sid.clone();
    let history = tokio::task::spawn_blocking(move || {
        let directories = if backend == "claude" {
            vec![(root.join("projects"), 1)]
        } else {
            vec![(root.join("sessions"), 3), (root.join("archived_sessions"), 0)]
        };
        for (directory, depth) in directories {
            if !directory.exists() {
                continue;
            }
            if let Some(path) = find_transcript(&directory, &sid, depth, &backend)? {
                return read_history(&path, &backend).map(Some);
            }
        }
        Ok::<_, io::Error>(None)
    })
    .await;
    match history {
        Ok(Ok(Some(history))) => {
            persist(repo, user_id, conversation_id, history).await;
        }
        Ok(Ok(None)) => {
            tracing::debug!(
                conversation_id,
                "compaction history unavailable; retaining known ledger"
            );
        }
        _ => {
            tracing::warn!(
                conversation_id,
                "compaction history could not be read; retaining known ledger"
            );
        }
    }
}

pub(crate) async fn record(
    repo: &dyn IAcpSessionRepository,
    user_id: &str,
    conversation_id: &str,
    event_id: &str,
    completed_at_ms: Option<i64>,
) -> Option<Value> {
    if event_id.is_empty() {
        return None;
    }
    let mut history = History::default();
    history
        .events
        .insert(event_id.to_owned(), completed_at_ms.or_else(|| Some(now_ms())));
    persist(repo, user_id, conversation_id, history).await
}

fn merge(usage: &mut Map<String, Value>, history: History) -> bool {
    let meta = usage.entry("_meta").or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    let old = meta.get(META_KEY).cloned();
    let mut ledger: Ledger = old
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let before = ledger.events.len();
    let complete = ledger.history_complete;
    for (id, time) in history.events {
        // Replays cannot update either the count or the latest timestamp.
        ledger.events.entry(id).or_insert(time);
    }
    ledger.history_complete |= history.complete;
    if old.is_some() && before == ledger.events.len() && complete == ledger.history_complete {
        return false;
    }
    ledger.count = ledger.events.len();
    ledger.last_compacted_at = ledger.events.values().filter_map(|at| *at).max();
    ledger.updated_at = now_ms().max(ledger.updated_at.saturating_add(1));
    meta[META_KEY] = json!(ledger);
    true
}

async fn persist(
    repo: &dyn IAcpSessionRepository,
    user_id: &str,
    conversation_id: &str,
    history: History,
) -> Option<Value> {
    let state = match repo.load_runtime_state_for_user(user_id, conversation_id).await {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(conversation_id, %error, "compaction ledger load failed");
            return None;
        }
    };
    let mut usage = state
        .and_then(|s| s.context_usage_json)
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if !merge(&mut usage, history) {
        return None;
    }
    let value = json!(usage);
    let json = value.to_string();
    let params = SaveRuntimeStateParams {
        context_usage_json: Some(Some(&json)),
        ..Default::default()
    };
    match repo
        .save_runtime_state_for_user(user_id, conversation_id, &params)
        .await
    {
        Ok(true) => {
            tracing::info!(
                conversation_id,
                count = value["_meta"][META_KEY]["count"].as_u64(),
                history_complete = value["_meta"][META_KEY]["history_complete"].as_bool(),
                "context compaction ledger persisted"
            );
            Some(value)
        }
        Ok(false) => {
            tracing::warn!(conversation_id, "compaction ledger row missing; nothing persisted");
            None
        }
        Err(error) => {
            tracing::warn!(conversation_id, %error, "compaction ledger persist failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Metadata-only excerpts of the native records cited above. Bodies omitted.
    const CLAUDE: &str = r#"{"type":"system","subtype":"compact_boundary","uuid":"0a7cda3f-1d16-4a8b-a3d6-1d27ab21b3aa","timestamp":"2026-10-30T00:02:34.090Z","compactMetadata":{"trigger":"auto","preTokens":27349}}"#;
    const CODEX: &str = r#"{"timestamp":"2026-10-23T11:15:59.861Z","type":"event_msg","payload":{"type":"item_completed","thread_id":"01a167f0-3a5a-7533-a16a-afebdb06cf00","turn_id":"01a1686d-f62a-7303-8dc5-c6ae2c64806a","completed_at_ms":1792754159861,"item":{"type":"ContextCompaction","id":"01a16877-86ab-77f3-87f0-cc7b799a6354"}}}"#;

    fn history(backend: &str, lines: &[&str]) -> History {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        read_history(file.path(), backend).unwrap()
    }

    #[test]
    fn compaction_history_replays_deduplicate_without_touching_usage() {
        let mut usage = json!({"used":26420,"size":1000000,"cost":{"amount":0.117,"currency":"USD"},"_meta":{"aionui/contextUsage":{"status":"pending"},"_claude/rateLimit":{"status":"allowed"}}}).as_object().unwrap().clone();
        let original = usage.clone();
        assert!(merge(&mut usage, history("claude", &[CLAUDE, CLAUDE])));
        let saved = usage.clone();
        assert!(!merge(&mut usage, history("claude", &[CLAUDE])));
        assert_eq!(saved, usage);
        assert_eq!(usage["_meta"][META_KEY]["count"], 1);
        assert!(usage["_meta"][META_KEY]["last_compacted_at"].as_i64().unwrap() > 0);
        usage
            .get_mut("_meta")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(META_KEY);
        assert_eq!(usage, original);
    }

    #[test]
    fn compaction_history_counts_codex_completion_not_both_native_records() {
        let h = history(
            "codex",
            &[
                r#"{"type":"compacted","payload":{"compaction_response_id":"different-id"}}"#,
                CODEX,
                CODEX,
            ],
        );
        assert_eq!(h.events.len(), 1);
        assert!(h.complete);
        assert_eq!(
            h.events["codex:01a16877-86ab-77f3-87f0-cc7b799a6354"],
            Some(1792754159861)
        );
    }

    #[test]
    fn compaction_history_without_completion_identity_is_incomplete_not_known_zero() {
        for (backend, lines) in [
            ("codex", vec![r#"{"type":"compacted","payload":{}}"#]),
            ("claude", vec![r#"{"type":"system","subtype":"compact_boundary"}"#]),
            ("claude", vec![r#"{"subtype":"compact_boundary""#]),
        ] {
            let h = history(backend, &lines);
            assert!(!h.complete);
            assert!(h.events.is_empty());
        }
    }

    #[test]
    fn compaction_history_ignores_text_mentions_and_started_items() {
        let h = history(
            "codex",
            &[
                r#"{"type":"event_msg","payload":{"type":"item_started","item":{"type":"ContextCompaction","id":"not-finished"}}}"#,
                r#"{"type":"response_item","payload":{"type":"message","text":"ContextCompaction"}}"#,
            ],
        );
        assert!(h.events.is_empty());
        let child = history(
            "claude",
            &[r#"{"type":"system","subtype":"compact_boundary","uuid":"child","isSidechain":true}"#],
        );
        assert!(child.events.is_empty());
    }

    #[test]
    fn compaction_live_then_restore_uses_same_codex_identity() {
        let mut usage = Map::new();
        let mut live = History::default();
        live.events
            .insert("codex:01a16877-86ab-77f3-87f0-cc7b799a6354".into(), Some(1792754159861));
        merge(&mut usage, live);
        assert_eq!(usage["_meta"][META_KEY]["history_complete"], false);
        merge(&mut usage, history("codex", &[CODEX]));
        assert_eq!(usage["_meta"][META_KEY]["count"], 1);
        assert_eq!(usage["_meta"][META_KEY]["history_complete"], true);
    }

    #[test]
    fn compaction_lookup_respects_account_root_and_does_not_read_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("workspace");
        let children = project.join("subagents");
        std::fs::create_dir_all(&children).unwrap();
        std::fs::write(children.join("session.jsonl"), CLAUDE).unwrap();
        assert!(find_transcript(&projects, "session", 1, "claude").unwrap().is_none());
        std::fs::write(project.join("session.jsonl"), CLAUDE).unwrap();
        assert_eq!(
            find_transcript(&projects, "session", 1, "claude").unwrap(),
            Some(project.join("session.jsonl"))
        );
        let config = SessionConfig {
            spawn_env: vec![aionui_common::EnvVar {
                name: "CLAUDE_CONFIG_DIR".into(),
                value: dir.path().to_string_lossy().into_owned(),
            }],
            ..Default::default()
        };
        assert_eq!(native_root("claude", &config), Some(dir.path().to_path_buf()));
    }

    /// Opt-in metadata-only audit; never changes a native transcript or the DB.
    #[test]
    #[ignore = "requires explicit AIONUI_COMPACTION_HISTORY_FILE and AIONUI_COMPACTION_BACKEND"]
    fn compaction_native_history_readonly_audit() {
        let path = std::env::var_os("AIONUI_COMPACTION_HISTORY_FILE").expect("explicit transcript path");
        let backend = std::env::var("AIONUI_COMPACTION_BACKEND").expect("explicit backend");
        let started = std::time::Instant::now();
        let history = read_history(Path::new(&path), &backend).unwrap();
        println!(
            "backend={backend} count={} complete={} elapsed_ms={}",
            history.events.len(),
            history.complete,
            started.elapsed().as_millis()
        );
        assert!(
            !history.events.is_empty(),
            "audit input must contain completed compactions"
        );
        assert!(history.complete);
    }
}
