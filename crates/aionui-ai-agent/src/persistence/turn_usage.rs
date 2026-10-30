//! Drain late CLI usage before releasing a completed conversation turn.
//! Display completion remains immediate; only resource release waits.
//!
//! `_meta["aionui/contextUsage"]` is an optional ACP extension. `pending` means
//! the current turn has not provided a persisted reading; `unavailable` means
//! the drain ended without one. Both retain the previous numbers, which must
//! not be interpreted as this turn's confirmed occupancy. `current` is written
//! atomically with an agent-reported usage snapshot. `turn_id` identifies its
//! Aion turn; `updated_at` is the last successful reading's Unix milliseconds
//! (null for legacy/no reading), while `status_changed_at` orders transitions.
//! Missing metadata remains legacy/unknown, never an inferred fresh reading.

use std::time::Duration;

use aionui_session::{SessionEnvelope, SessionEvent};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageDrainOutcome {
    NotRequired,
    Persisted,
    NotReported,
    PersistenceFailed,
    ProcessClosed,
    TimedOut,
}

pub(crate) const USAGE_META_KEY: &str = "aionui/contextUsage";
pub(crate) const USAGE_WARNING_AFTER: Duration = Duration::from_secs(10);
pub(crate) const USAGE_DEADLINE: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Default)]
struct DrainState {
    generation: u64,
    minimum_generation: u64,
    turn_id: Option<String>,
    pending: bool,
    saved: Option<bool>,
    closed: bool,
    frozen: bool,
}

pub(crate) struct TurnUsageDrain(watch::Sender<DrainState>);

impl Default for TurnUsageDrain {
    fn default() -> Self {
        Self(watch::channel(DrainState::default()).0)
    }
}

impl TurnUsageDrain {
    /// Called before dispatch, while the conversation's turn claim is held.
    pub(crate) fn begin(&self, turn_id: &str) {
        self.0.send_modify(|state| {
            *state = DrainState {
                generation: state.generation,
                minimum_generation: state.generation + 1,
                turn_id: Some(turn_id.to_owned()),
                ..Default::default()
            };
        });
    }

    pub(crate) fn turn_id(&self) -> Option<String> {
        self.0.borrow().turn_id.clone()
    }

    pub(crate) fn accepts_usage(&self, generation: u64) -> bool {
        let state = self.0.borrow();
        !state.frozen && generation >= state.minimum_generation && generation >= state.generation
    }

    /// Stop accepting queued results before force-terminating a timed-out CLI.
    pub(crate) fn freeze(&self) {
        self.0.send_modify(|state| state.frozen = true);
    }

    pub(crate) fn observe(&self, env: &SessionEnvelope) {
        if env.turn_gen < self.0.borrow().minimum_generation || self.0.borrow().frozen {
            return;
        }
        match &env.event {
            SessionEvent::AdapterSpecific { tag, .. } if tag == "_claude/usage_pending" => {
                self.0.send_modify(|state| {
                    if env.turn_gen >= state.generation && !(state.pending && env.turn_gen == state.generation) {
                        if env.turn_gen > state.generation {
                            state.saved = None;
                        }
                        state.generation = env.turn_gen;
                        state.pending = true;
                    }
                });
            }
            SessionEvent::AdapterSpecific { tag, .. } if tag == "_claude/usage_complete" => {
                self.0.send_modify(|state| {
                    if env.turn_gen == state.generation {
                        state.pending = false;
                    }
                });
            }
            SessionEvent::Detached { .. } => self.close(),
            _ => {}
        }
        self.0
            .send_modify(|state| state.generation = state.generation.max(env.turn_gen));
    }

    pub(crate) fn record_write(&self, generation: u64, saved: bool) {
        self.0.send_modify(|state| {
            if !state.frozen && generation >= state.minimum_generation && generation == state.generation {
                state.saved = Some(saved);
            }
        });
    }

    pub(crate) fn close(&self) {
        self.0.send_modify(|state| state.closed = true);
    }

    pub(crate) async fn wait(&self, budget: Duration) -> UsageDrainOutcome {
        let mut rx = self.0.subscribe();
        let ready = tokio::time::timeout(budget, rx.wait_for(|state| !state.pending || state.closed)).await;
        let Ok(Ok(state)) = ready else {
            return UsageDrainOutcome::TimedOut;
        };
        if state.pending && state.closed {
            return UsageDrainOutcome::ProcessClosed;
        }
        match state.saved {
            Some(true) => UsageDrainOutcome::Persisted,
            Some(false) => UsageDrainOutcome::PersistenceFailed,
            None if state.generation > 0 || state.turn_id.is_some() => UsageDrainOutcome::NotReported,
            None => UsageDrainOutcome::NotRequired,
        }
    }
}

/// ACP extension only: preserve every agent-reported number and vendor metadata.
/// `updated_at` refers to the last successful usage write, not a status change.
pub(crate) fn merge_freshness(
    usage: &mut serde_json::Map<String, serde_json::Value>,
    turn_id: &str,
    status: &str,
    reason: Option<&str>,
) {
    let meta = usage.entry("_meta").or_insert_with(|| serde_json::json!({}));
    if !meta.is_object() {
        *meta = serde_json::json!({});
    }
    let previous = &meta[USAGE_META_KEY];
    let changed_at = aionui_common::now_ms().max(previous["status_changed_at"].as_i64().unwrap_or(0).saturating_add(1));
    let updated_at = if status == "current" {
        Some(changed_at)
    } else {
        previous["updated_at"].as_i64()
    };
    meta[USAGE_META_KEY] = serde_json::json!({
        "status": status,
        "turn_id": turn_id,
        "updated_at": updated_at,
        "status_changed_at": changed_at,
        "reason": reason,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(generation: u64, tag: &str) -> SessionEnvelope {
        SessionEnvelope {
            session_id: "conv".into(),
            turn_gen: generation,
            event: SessionEvent::AdapterSpecific {
                tag: tag.into(),
                payload: serde_json::Value::Null,
            },
        }
    }

    #[tokio::test]
    async fn late_result_waits_for_write_and_rejects_previous_turn_completion() {
        let drain = TurnUsageDrain::default();
        drain.observe(&marker(2, "_claude/usage_pending"));
        drain.observe(&marker(1, "_claude/usage_complete"));
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::TimedOut);
        drain.record_write(2, true);
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::TimedOut);
        drain.observe(&marker(2, "_claude/usage_complete"));
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::Persisted);
    }

    #[tokio::test]
    async fn missing_usage_failed_write_and_process_exit_do_not_hang() {
        let drain = TurnUsageDrain::default();
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::NotRequired);
        drain.observe(&marker(1, "_claude/usage_pending"));
        drain.observe(&marker(1, "_claude/usage_complete"));
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::NotReported);
        drain.observe(&marker(2, "_claude/usage_pending"));
        drain.record_write(2, false);
        drain.observe(&marker(2, "_claude/usage_complete"));
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::PersistenceFailed);
        drain.observe(&marker(3, "_claude/usage_pending"));
        drain.close();
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::ProcessClosed);
    }

    #[tokio::test]
    async fn new_turn_rejects_old_usage_and_timeout_freezes_queued_results() {
        let drain = TurnUsageDrain::default();
        drain.begin("first");
        drain.observe(&marker(1, "_claude/usage_pending"));
        drain.freeze();
        assert!(!drain.accepts_usage(1));
        drain.begin("second");
        assert!(!drain.accepts_usage(1));
        drain.observe(&marker(2, "_claude/usage_pending"));
        drain.record_write(1, true);
        drain.observe(&marker(1, "_claude/usage_complete"));
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::TimedOut);
        drain.record_write(2, true);
        drain.observe(&marker(2, "_claude/usage_complete"));
        assert_eq!(drain.wait(Duration::ZERO).await, UsageDrainOutcome::Persisted);
    }

    #[test]
    fn pending_and_unavailable_preserve_last_reading_and_its_timestamp() {
        let mut usage = serde_json::json!({"used":42,"size":100,"_meta":{"vendor":true}})
            .as_object()
            .unwrap()
            .clone();
        merge_freshness(&mut usage, "first", "current", None);
        let reported_at = usage["_meta"][USAGE_META_KEY]["updated_at"].clone();
        merge_freshness(&mut usage, "second", "pending", None);
        merge_freshness(&mut usage, "second", "unavailable", Some("timed_out"));
        assert_eq!(usage["used"], 42);
        assert_eq!(usage["size"], 100);
        assert_eq!(usage["_meta"]["vendor"], true);
        assert_eq!(usage["_meta"][USAGE_META_KEY]["updated_at"], reported_at);
        assert_eq!(usage["_meta"][USAGE_META_KEY]["status"], "unavailable");
    }
}
