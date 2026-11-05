//! Fork: session messages between machines of one MindNProgress setup.
//!
//! A target id that is not in this machine's database may belong to a
//! conversation on another machine. The sender side hands the message to a
//! [`RemoteSessionRelay`] (MindNProgress), which finds the machine holding the
//! target and calls that machine's [`SessionMessageService::deliver_remote`].
//! The recipient applies the usual checks and the same recipient block, then
//! delivers through the human send path. A relayed message never takes a
//! second hop: `deliver_remote` only delivers locally.

use std::sync::Arc;

use aionui_api_types::{SessionDeliveryStatus, SessionSendMessageResponse};
use aionui_conversation::session_mentions::{team_id_from_extra_str, workspace_from_extra};
use aionui_db::models::ConversationRow;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{DeliverAttemptError, SessionMessageService, build_session_message_block, compose_delivery_content};
use crate::error::SessionMessageError;
use crate::queue::{PendingDelivery, TTL_MS};
use crate::rate_limit::RateVerdict;

/// Sender-side payload handed to the relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionRelayRequest {
    pub from_conversation_id: String,
    pub from_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_workspace: Option<String>,
    pub to: String,
    pub message: String,
}

/// Recipient-side payload: the relayed request plus the sender's machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionDelivery {
    #[serde(flatten)]
    pub request: RemoteSessionRelayRequest,
    /// Display label of the sender's machine.
    pub from_machine: String,
}

#[async_trait]
pub trait RemoteSessionRelay: Send + Sync {
    /// Deliver to a conversation that is not on this machine. Returns
    /// `TargetNotFound` when no other machine holds it either.
    async fn relay(
        &self,
        user_id: &str,
        request: RemoteSessionRelayRequest,
    ) -> Result<SessionSendMessageResponse, SessionMessageError>;
}

/// `workspace:` value for a message from another machine. Paths there are
/// never readable here, whatever their spelling.
fn remote_workspace_field(workspace: Option<&str>, machine: &str) -> String {
    let location = workspace.unwrap_or("unknown folder");
    format!("{location} on another machine ({machine}); don't use relative paths, don't assume readable")
}

impl SessionMessageService {
    /// Connect the relay used for targets that are not on this machine. Set
    /// once at startup; without it a missing target stays `target_not_found`.
    pub fn set_remote_relay(&self, relay: Arc<dyn RemoteSessionRelay>) {
        let _ = self.remote_relay.set(relay);
    }

    /// `send` step 5 when the target is not on this machine.
    pub(super) async fn relay_missing_target(
        &self,
        user_id: &str,
        sender: &ConversationRow,
        from_conversation_id: &str,
        to: &str,
        message: &str,
    ) -> Result<SessionSendMessageResponse, SessionMessageError> {
        let Some(relay) = self.remote_relay.get() else {
            return Err(SessionMessageError::TargetNotFound { id: to.to_owned() });
        };
        let request = RemoteSessionRelayRequest {
            from_conversation_id: from_conversation_id.to_owned(),
            from_name: sender.name.clone(),
            from_workspace: workspace_from_extra(&sender.extra),
            to: to.to_owned(),
            message: message.to_owned(),
        };
        let result = relay.relay(user_id, request).await;
        match &result {
            Ok(response) => info!(
                from_conversation_id,
                to_conversation_id = to,
                outcome = "relayed",
                status = ?response.status,
                "cross-session delivery relayed to another machine"
            ),
            Err(error) => warn!(
                from_conversation_id,
                to_conversation_id = to,
                outcome = "rejected",
                error_code = ?error.code(),
                "cross-machine session relay refused"
            ),
        }
        result
    }

    /// Deliver a message relayed from another machine of the same setup.
    pub async fn deliver_remote(
        &self,
        user_id: &str,
        delivery: RemoteSessionDelivery,
    ) -> Result<SessionSendMessageResponse, SessionMessageError> {
        if !self.is_enabled_for(user_id).await {
            return Err(SessionMessageError::FeatureDisabled);
        }
        let RemoteSessionDelivery { request, from_machine } = delivery;
        let from = request.from_conversation_id.trim();
        let to = request.to.trim();
        if from.is_empty() || to.is_empty() || to == "*" || request.message.trim().is_empty() {
            return Err(SessionMessageError::SchemaValidation {
                reason: "a relayed message needs a sender, a target conversation id and a body".to_owned(),
            });
        }
        if let RateVerdict::Tripped { gate, window_count } = self.deps.rate_limiter.check_and_record(from, to) {
            warn!(
                from_conversation_id = from,
                to_conversation_id = to,
                window_count,
                gate = ?gate,
                outcome = "rejected",
                error_code = "rate_limited",
                "relayed cross-session rate gate tripped"
            );
            return Err(SessionMessageError::RateLimited);
        }
        let target = self
            .deps
            .conversation_repo
            .get(user_id, to)
            .await
            .map_err(|error| SessionMessageError::TransportUnavailable {
                reason: error.to_string(),
            })?
            .ok_or_else(|| SessionMessageError::TargetNotFound { id: to.to_owned() })?;
        if team_id_from_extra_str(&target.extra).is_some() {
            return Err(SessionMessageError::TargetIsTeam { id: to.to_owned() });
        }

        let block = build_session_message_block(
            &request.from_name,
            from,
            &remote_workspace_field(request.from_workspace.as_deref(), &from_machine),
            from,
        );
        let content = compose_delivery_content(&block, &request.message);
        match self.deliver_now(user_id, to, content.clone()).await {
            Ok(sent) => {
                info!(
                    from_conversation_id = from,
                    to_conversation_id = to,
                    from_machine = %from_machine,
                    outcome = "delivered",
                    delivered_midturn = sent.delivered_midturn,
                    "relayed cross-session delivery accepted"
                );
                Ok(SessionSendMessageResponse {
                    status: SessionDeliveryStatus::Delivered,
                    to: to.to_owned(),
                })
            }
            Err(DeliverAttemptError::Transient(reason)) => {
                self.deps.queue.push(PendingDelivery {
                    to: to.to_owned(),
                    user_id: user_id.to_owned(),
                    from_conversation_id: from.to_owned(),
                    message: content,
                    expires_at_ms: self.deps.queue.clock().now_ms() + TTL_MS,
                })?;
                self.deps.notify.notify_one();
                info!(
                    from_conversation_id = from,
                    to_conversation_id = to,
                    from_machine = %from_machine,
                    outcome = "queued",
                    reason = %reason,
                    "relayed cross-session delivery queued; target not ready"
                );
                Ok(SessionSendMessageResponse {
                    status: SessionDeliveryStatus::Queued,
                    to: to.to_owned(),
                })
            }
            Err(DeliverAttemptError::Archived(_)) => Err(SessionMessageError::TargetArchived { id: to.to_owned() }),
            Err(DeliverAttemptError::Hard(reason)) => Err(SessionMessageError::TransportUnavailable { reason }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::remote_workspace_field;

    #[test]
    fn remote_workspace_names_the_sender_machine_and_forbids_relative_paths() {
        assert_eq!(
            remote_workspace_field(Some("D:/work"), "Main PC"),
            "D:/work on another machine (Main PC); don't use relative paths, don't assume readable"
        );
        assert!(remote_workspace_field(None, "MacBook").starts_with("unknown folder on another machine (MacBook)"));
    }
}
