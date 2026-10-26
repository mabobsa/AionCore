use std::{future::Future, time::Duration};

use tokio::time::{Instant, timeout_at};
use tracing::{info, warn};

use crate::ConversationError;

/// A single deadline shared by every deletion stage, including lock waits.
pub(super) struct DeleteProgress<'a> {
    conversation_id: &'a str,
    deadline: Instant,
}

impl<'a> DeleteProgress<'a> {
    pub(super) fn new(conversation_id: &'a str) -> Self {
        Self {
            conversation_id,
            deadline: Instant::now() + Duration::from_secs(180),
        }
    }

    pub(super) async fn run<T>(
        &self,
        phase: &'static str,
        seconds: u64,
        operation: impl Future<Output = Result<T, ConversationError>>,
    ) -> Result<T, ConversationError> {
        let started = Instant::now();
        let deadline = self.deadline.min(started + Duration::from_secs(seconds));
        info!(
            conversation_id = self.conversation_id,
            phase, "Conversation deletion stage started"
        );
        let result = if started >= deadline {
            Err(ConversationError::Timeout {
                reason: format!("Conversation deletion timed out at {phase}"),
            })
        } else {
            timeout_at(deadline, operation).await.unwrap_or_else(|_| {
                Err(ConversationError::Timeout {
                    reason: format!("Conversation deletion timed out at {phase}"),
                })
            })
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => info!(
                conversation_id = self.conversation_id,
                phase, elapsed_ms, "Conversation deletion stage completed"
            ),
            Err(error) => warn!(
                conversation_id = self.conversation_id,
                phase,
                elapsed_ms,
                code = error.error_code(),
                "Conversation deletion stage failed"
            ),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn timeout_identifies_the_blocked_stage() {
        let progress = DeleteProgress::new("c");
        let err = progress
            .run::<()>("stop_agent", 30, std::future::pending())
            .await
            .unwrap_err();
        assert!(matches!(err, ConversationError::Timeout { reason } if reason.contains("stop_agent")));
    }

    #[tokio::test(start_paused = true)]
    async fn subsequent_stages_share_the_overall_deadline() {
        let progress = DeleteProgress::new("c");
        progress
            .run("first", 180, async {
                tokio::time::sleep(Duration::from_secs(170)).await;
                Ok(())
            })
            .await
            .unwrap();
        let started = Instant::now();
        assert!(matches!(
            progress.run::<()>("second", 30, std::future::pending()).await,
            Err(ConversationError::Timeout { .. })
        ));
        assert_eq!(started.elapsed(), Duration::from_secs(10));
    }
}
