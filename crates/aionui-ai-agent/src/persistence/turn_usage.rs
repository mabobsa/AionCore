//! Drain late CLI usage before releasing a completed conversation turn.
//! Display completion remains immediate; only resource release waits.

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

#[derive(Clone, Copy, Debug, Default)]
struct DrainState {
    generation: u64,
    pending: bool,
    saved: Option<bool>,
    closed: bool,
}

pub(crate) struct TurnUsageDrain(watch::Sender<DrainState>);

impl Default for TurnUsageDrain {
    fn default() -> Self {
        Self(watch::channel(DrainState::default()).0)
    }
}

impl TurnUsageDrain {
    pub(crate) fn observe(&self, env: &SessionEnvelope) {
        match &env.event {
            SessionEvent::AdapterSpecific { tag, .. } if tag == "_claude/usage_pending" => {
                self.0.send_modify(|state| {
                    if env.turn_gen >= state.generation && !(state.pending && env.turn_gen == state.generation) {
                        *state = DrainState {
                            generation: env.turn_gen,
                            pending: true,
                            ..Default::default()
                        };
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
    }

    pub(crate) fn record_write(&self, generation: u64, saved: bool) {
        self.0.send_modify(|state| {
            if state.pending && generation == state.generation {
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
            None if state.generation > 0 => UsageDrainOutcome::NotReported,
            None => UsageDrainOutcome::NotRequired,
        }
    }
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
}
