//! Correlate Codex config writes with both the RPC acknowledgement and the
//! thread/settings/updated notification. Verified against codex-cli 0.167.1
//! generated ThreadSettingsUpdateParams / ThreadSettingsUpdatedNotification.
use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::{Mutex, oneshot};

use super::BackendError;

struct Pending {
    label: String,
    expected: Option<(String, String, String)>,
    acknowledged: bool,
    observed: bool,
    completion: Option<oneshot::Sender<Result<(), String>>>,
}

#[derive(Default)]
struct ConfigState {
    pending: HashMap<u64, Pending>,
    // Only values read from actual Codex responses/notifications, never requested values.
    actual: HashMap<(String, String), String>,
}

#[derive(Default)]
pub(super) struct CodexConfigWrites(Mutex<ConfigState>);

impl CodexConfigWrites {
    pub async fn track(&self, id: u64, label: String) {
        self.0.lock().await.pending.insert(
            id,
            Pending {
                label,
                expected: None,
                acknowledged: false,
                observed: false,
                completion: None,
            },
        );
    }

    pub async fn register(
        &self,
        id: u64,
        thread: &str,
        field: &str,
        value: &str,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        self.0.lock().await.pending.insert(
            id,
            Pending {
                label: format!("{field} -> {value}"),
                expected: Some((thread.into(), field.into(), value.into())),
                acknowledged: false,
                observed: false,
                completion: Some(tx),
            },
        );
        rx
    }

    pub async fn forget(&self, id: u64) {
        self.0.lock().await.pending.remove(&id);
    }

    /// Return the label for existing error logging, including unawaited mode writes.
    pub async fn response(&self, id: u64, frame: &Value) -> Option<String> {
        let mut state = self.0.lock().await;
        let ConfigState { pending, actual } = &mut *state;
        let item = pending.get_mut(&id)?;
        if let Some((thread, field, value)) = &item.expected {
            item.observed = actual.get(&(thread.clone(), field.clone())) == Some(value);
        }
        let label = item.label.clone();
        if let Some(error) = frame.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("config rejected")
                .to_owned();
            if let Some(tx) = item.completion.take() {
                let _ = tx.send(Err(message));
            }
            pending.remove(&id);
        } else if frame.get("result").is_some_and(Value::is_object) {
            item.acknowledged = true;
            if item.expected.is_none() || item.observed {
                if let Some(tx) = item.completion.take() {
                    let _ = tx.send(Ok(()));
                }
                pending.remove(&id);
            }
        }
        Some(label)
    }

    pub async fn observe(&self, params: &Value) {
        let mut state = self.0.lock().await;
        let ConfigState { pending, actual } = &mut *state;
        if let Some(thread) = params.get("threadId").and_then(Value::as_str) {
            for field in ["model", "effort"] {
                if let Some(value) = params.get("threadSettings").and_then(|s| s.get(field)) {
                    let key = (thread.into(), field.into());
                    if let Some(value) = value.as_str() {
                        actual.insert(key, value.into());
                    } else {
                        actual.remove(&key);
                    }
                }
            }
        }
        pending.retain(|_, item| {
            if item.completion.as_ref().is_some_and(oneshot::Sender::is_closed) {
                return false;
            }
            let Some((thread, field, value)) = &item.expected else {
                return true;
            };
            item.observed = actual.get(&(thread.clone(), field.clone())) == Some(value);
            if item.acknowledged && item.observed {
                if let Some(tx) = item.completion.take() {
                    let _ = tx.send(Ok(()));
                }
                return false;
            }
            true
        });
    }

    // thread/start and thread/resume report the effective model and reasoningEffort.
    // An idempotent settings/update can ACK without emitting settings/updated, so
    // retain this authoritative baseline for confirmation of unchanged settings.
    pub async fn observe_opened(&self, result: &Value) {
        if let Some(thread) = result.get("thread").and_then(|t| t.get("id")).and_then(Value::as_str)
            && result.get("model").and_then(Value::as_str).is_some()
        {
            self.observe(&serde_json::json!({"threadId": thread, "threadSettings": {
                "model": result.get("model"), "effort": result.get("reasoningEffort")
            }}))
            .await;
        }
    }

    pub async fn wait(&self, id: u64, rx: oneshot::Receiver<Result<(), String>>) -> Result<(), BackendError> {
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), rx).await;
        self.forget(id).await;
        match result {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(message))) => Err(BackendError::Transport(message)),
            _ => Err(BackendError::DeliveryUnconfirmed(
                "Codex config acknowledgement or settings confirmation missing".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn config_requires_ack_and_matching_thread_settings_in_either_order() {
        for reverse in [false, true] {
            let pending = CodexConfigWrites::default();
            let mut rx = pending.register(1, "thread", "model", "astra").await;
            pending
                .observe(&json!({"threadId":"other","threadSettings":{"model":"astra"}}))
                .await;
            assert!(rx.try_recv().is_err());
            let settings = json!({"threadId":"thread","threadSettings":{"model":"astra"}});
            let ack = json!({"result":{}});
            if reverse {
                pending.observe(&settings).await;
            } else {
                pending.response(1, &ack).await;
            }
            assert!(rx.try_recv().is_err(), "one signal alone must not confirm");
            if reverse {
                pending.response(1, &ack).await;
            } else {
                pending.observe(&settings).await;
            }
            pending.wait(1, rx).await.unwrap();
            assert!(pending.0.lock().await.pending.is_empty());
        }
    }

    #[tokio::test]
    async fn unchanged_setting_requires_ack_but_not_a_redundant_notification() {
        let pending = CodexConfigWrites::default();
        pending
            .observe_opened(&json!({"thread":{"id":"thread"},"model":"astra","reasoningEffort":"xhigh"}))
            .await;
        for (id, field, value) in [(1, "model", "astra"), (2, "effort", "xhigh")] {
            let mut rx = pending.register(id, "thread", field, value).await;
            assert!(rx.try_recv().is_err());
            pending.response(id, &json!({"result":{}})).await;
            pending.wait(id, rx).await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cleared_effort_does_not_reuse_a_stale_confirmed_value() {
        let pending = CodexConfigWrites::default();
        pending
            .observe_opened(&json!({"thread":{"id":"thread"},"model":"astra","reasoningEffort":"xhigh"}))
            .await;
        pending
            .observe(&json!({"threadId":"thread","threadSettings":{"effort":null}}))
            .await;
        let rx = pending.register(1, "thread", "effort", "xhigh").await;
        pending.response(1, &json!({"result":{}})).await;
        assert!(matches!(
            pending.wait(1, rx).await,
            Err(BackendError::DeliveryUnconfirmed(_))
        ));
    }

    #[tokio::test]
    async fn rejection_overrides_optimistic_or_unrelated_observations() {
        let pending = CodexConfigWrites::default();
        let rx = pending.register(1, "thread", "effort", "xhigh").await;
        pending
            .observe(&json!({"threadId":"thread","threadSettings":{"effort":"high"}}))
            .await;
        pending
            .response(1, &json!({"error":{"message":"unsupported effort"}}))
            .await;
        assert!(
            matches!(pending.wait(1, rx).await, Err(BackendError::Transport(message)) if message == "unsupported effort")
        );
        assert!(pending.0.lock().await.pending.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn ack_without_observation_times_out_and_releases_pending_write() {
        let pending = CodexConfigWrites::default();
        let rx = pending.register(1, "thread", "model", "astra").await;
        pending.response(1, &json!({"result":{}})).await;
        assert!(matches!(
            pending.wait(1, rx).await,
            Err(BackendError::DeliveryUnconfirmed(_))
        ));
        assert!(pending.0.lock().await.pending.is_empty());
    }
}
