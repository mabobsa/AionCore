use super::*;

/// Owns the deletion flag across awaits, including cancellation and timeouts.
pub(crate) struct DeletionGuard {
    state: Arc<ConversationRuntimeStateService>,
    conversation_id: String,
}

/// Keeps deletion from racing an already-started runtime build.
pub(crate) struct RuntimePreparation {
    state: Arc<ConversationRuntimeStateService>,
    conversation_id: String,
}

impl ConversationRuntimeStateService {
    pub(crate) fn begin_delete(self: &Arc<Self>, id: &str) -> Result<DeletionGuard, ConversationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConversationError::internal("runtime state lock poisoned"))?;
        if state.deleting_conversations.contains(id) || state.restarting_conversations.contains(id) {
            return Err(ConversationError::Busy {
                reason: format!("conversation {id} is being deleted or restarted"),
            });
        }
        state.deleting_conversations.insert(id.to_owned());
        state.revision += 1;
        info!(conversation_id = id, "conversation marked deleting");
        Ok(DeletionGuard {
            state: self.clone(),
            conversation_id: id.to_owned(),
        })
    }

    pub(crate) fn begin_runtime_preparation(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<RuntimePreparation, ConversationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConversationError::internal("runtime state lock poisoned"))?;
        if state.deleting_conversations.contains(id) || state.shutting_down || state.database_maintenance_active() {
            return Err(ConversationError::Busy {
                reason: format!("conversation {id} cannot start a runtime during deletion or maintenance"),
            });
        }
        *state.runtime_preparations.entry(id.to_owned()).or_default() += 1;
        Ok(RuntimePreparation {
            state: self.clone(),
            conversation_id: id.to_owned(),
        })
    }

    pub(crate) async fn wait_for_runtime_preparations(&self, id: &str) {
        loop {
            let notified = self.release_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .state
                .lock()
                .is_ok_and(|state| !state.runtime_preparations.contains_key(id))
            {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for DeletionGuard {
    fn drop(&mut self) {
        self.state.clear_deleting(&self.conversation_id);
    }
}

impl Drop for RuntimePreparation {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.state.lock()
            && let Some(count) = state.runtime_preparations.get_mut(&self.conversation_id)
        {
            *count -= 1;
            if *count == 0 {
                state.runtime_preparations.remove(&self.conversation_id);
            }
        }
        self.state.release_notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deletion_waits_for_existing_builds_and_rejects_new_ones() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let first = state.begin_runtime_preparation("c").unwrap();
        let second = state.begin_runtime_preparation("c").unwrap();
        let deleting = state.begin_delete("c").unwrap();
        assert!(matches!(
            state.begin_runtime_preparation("c"),
            Err(ConversationError::Busy { .. })
        ));
        assert!(matches!(state.begin_delete("c"), Err(ConversationError::Busy { .. })));
        drop(first);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                state.wait_for_runtime_preparations("c")
            )
            .await
            .is_err()
        );
        drop(second);
        state.wait_for_runtime_preparations("c").await;
        drop(deleting);
        assert!(state.begin_runtime_preparation("c").is_ok());
    }

    #[test]
    fn deleting_idle_conversation_cannot_accept_messages() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let guard = state.begin_delete("c").unwrap();
        assert!(!state.summary_from_parts("c", None, false, 0, false).can_send_message);
        drop(guard);
        assert!(state.summary_from_parts("c", None, false, 0, false).can_send_message);
    }
}
