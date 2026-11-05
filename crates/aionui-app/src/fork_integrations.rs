//! Stable composition seam for integrations maintained by the personal fork.
//!
//! Upstream-facing composition files should depend only on the bundles and
//! builders in this module. Add future fork-only services, states, and routes
//! here so recurring upstream rebases do not expand the shared-file diff.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aionui_ai_agent::claude_accounts::{
    ClaudeAccountRouterState, ClaudeAccountService, ClaudeAccountStore, IClaudeAccountUsage, claude_account_routes,
    install_store,
};
use aionui_ai_agent::error::AgentError;
use aionui_api_types::SessionSendMessageResponse;
use aionui_auth::{AuthState, auth_middleware};
use aionui_db::fork_extensions::claude_account_usage::count_conversations_using_claude_account;
use aionui_db::{IConversationRepository, IMcpServerRepository, SqlitePool};
use aionui_session_message::{
    RemoteSessionRelay, RemoteSessionRelayRequest, SessionMessageError, SessionMessageService,
    session_message_remote_routes,
};
use aionui_system::external_launch::{
    MindNProgressNavigationRouterState, MindNProgressNavigationService, SessionMessageRelayOutcome,
    mindnprogress_navigation_routes,
};
use async_trait::async_trait;
use axum::Router;
use axum::middleware::from_fn_with_state;

pub(crate) struct ForkIntegrationServices {
    mindnprogress_navigation: Arc<MindNProgressNavigationService>,
    claude_accounts: Arc<ClaudeAccountService>,
    /// Set by [`ForkIntegrationServices::attach_session_messages`] once the upstream service exists.
    session_messages: OnceLock<Arc<SessionMessageService>>,
}

impl ForkIntegrationServices {
    /// Relay session messages for conversations on other machines through
    /// MindNProgress, and receive the ones relayed to this machine.
    pub(crate) fn attach_session_messages(&self, service: &Arc<SessionMessageService>) {
        service.set_remote_relay(Arc::new(MindNProgressSessionRelay {
            navigation: self.mindnprogress_navigation.clone(),
        }));
        let _ = self.session_messages.set(service.clone());
    }
}

/// Builds fork-owned services from repositories constructed by [`crate::services::AppServices`].
pub(crate) fn build_fork_integration_services(
    conversation_repo: Arc<dyn IConversationRepository>,
    mcp_server_repo: Arc<dyn IMcpServerRepository>,
    pool: SqlitePool,
    data_dir: &Path,
) -> anyhow::Result<ForkIntegrationServices> {
    let http_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;

    let claude_account_store = Arc::new(ClaudeAccountStore::in_data_dir(data_dir));
    install_store(claude_account_store.clone());

    Ok(ForkIntegrationServices {
        mindnprogress_navigation: Arc::new(MindNProgressNavigationService::new(
            conversation_repo,
            mcp_server_repo,
            http_client,
        )),
        claude_accounts: Arc::new(ClaudeAccountService::new(
            claude_account_store,
            Arc::new(ConversationClaudeAccountUsage { pool }),
        )),
        session_messages: OnceLock::new(),
    })
}

#[derive(Clone)]
pub(crate) struct ForkIntegrationStates {
    mindnprogress_navigation: MindNProgressNavigationRouterState,
    claude_accounts: ClaudeAccountRouterState,
    session_messages: Option<Arc<SessionMessageService>>,
}

/// Builds all fork-owned router state inside the application composition layer.
pub(crate) fn build_fork_integration_states(services: &ForkIntegrationServices) -> ForkIntegrationStates {
    ForkIntegrationStates {
        mindnprogress_navigation: MindNProgressNavigationRouterState {
            service: services.mindnprogress_navigation.clone(),
        },
        claude_accounts: ClaudeAccountRouterState {
            service: services.claude_accounts.clone(),
        },
        session_messages: services.session_messages.get().cloned(),
    }
}

/// Returns the authenticated route tree maintained by the personal fork.
pub(crate) fn fork_integration_authenticated_routes(states: ForkIntegrationStates, auth_state: AuthState) -> Router {
    let mut routes = mindnprogress_navigation_routes(states.mindnprogress_navigation)
        .merge(claude_account_routes(states.claude_accounts));
    if let Some(service) = states.session_messages {
        routes = routes.merge(session_message_remote_routes(service));
    }
    routes.route_layer(from_fn_with_state(auth_state, auth_middleware))
}

/// Hands session messages for conversations on other machines to MindNProgress.
struct MindNProgressSessionRelay {
    navigation: Arc<MindNProgressNavigationService>,
}

#[async_trait]
impl RemoteSessionRelay for MindNProgressSessionRelay {
    async fn relay(
        &self,
        user_id: &str,
        request: RemoteSessionRelayRequest,
    ) -> Result<SessionSendMessageResponse, SessionMessageError> {
        let body = serde_json::to_value(&request).map_err(|error| SessionMessageError::TransportUnavailable {
            reason: format!("session relay payload: {error}"),
        })?;
        let outcome = self
            .navigation
            .relay_session_message(user_id, &request.from_conversation_id, &body)
            .await;
        relay_outcome_to_result(&request.to, outcome)
    }
}

/// Without MindNProgress the target is simply not on this machine, exactly as before.
fn relay_outcome_to_result(
    to: &str,
    outcome: SessionMessageRelayOutcome,
) -> Result<SessionSendMessageResponse, SessionMessageError> {
    let (status, body) = match outcome {
        SessionMessageRelayOutcome::NotConfigured => {
            return Err(SessionMessageError::TargetNotFound { id: to.to_owned() });
        }
        SessionMessageRelayOutcome::Unavailable => {
            return Err(SessionMessageError::TransportUnavailable {
                reason: "MindNProgress relay is unavailable".to_owned(),
            });
        }
        SessionMessageRelayOutcome::Answered { status, body } => (status, body),
    };
    if (200..300).contains(&status) {
        let data = body.get("data").cloned().unwrap_or(body);
        return serde_json::from_value(data).map_err(|_| SessionMessageError::TransportUnavailable {
            reason: "MindNProgress relay returned an invalid response".to_owned(),
        });
    }
    let code = body.get("code").and_then(serde_json::Value::as_str).unwrap_or_default();
    let message = body
        .get("error")
        .or_else(|| body.get("msg"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("MindNProgress relay failed")
        .to_owned();
    let id = to.to_owned();
    Err(match code {
        "target_not_found" => SessionMessageError::TargetNotFound { id },
        "target_archived" => SessionMessageError::TargetArchived { id },
        "target_is_team" => SessionMessageError::TargetIsTeam { id },
        "target_is_self" => SessionMessageError::TargetIsSelf { id },
        "rate_limited" => SessionMessageError::RateLimited,
        "queue_full" => SessionMessageError::QueueFull,
        "feature_disabled" => SessionMessageError::FeatureDisabled,
        "schema_validation_failed" => SessionMessageError::SchemaValidation { reason: message },
        _ => SessionMessageError::TransportUnavailable {
            reason: format!("{message} (HTTP {status})"),
        },
    })
}

struct ConversationClaudeAccountUsage {
    pool: SqlitePool,
}

#[async_trait]
impl IClaudeAccountUsage for ConversationClaudeAccountUsage {
    async fn conversations_using(&self, account_id: &str) -> Result<i64, AgentError> {
        count_conversations_using_claude_account(&self.pool, account_id)
            .await
            .map_err(|e| AgentError::internal(format!("count claude account conversations: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use aionui_api_types::SessionDeliveryStatus;
    use aionui_session_message::SessionMessageError;
    use aionui_system::external_launch::SessionMessageRelayOutcome;
    use serde_json::json;

    use super::relay_outcome_to_result;

    #[test]
    fn missing_integration_keeps_the_target_not_found_answer() {
        let result = relay_outcome_to_result("c1", SessionMessageRelayOutcome::NotConfigured);
        assert!(matches!(result, Err(SessionMessageError::TargetNotFound { id }) if id == "c1"));
    }

    #[test]
    fn relayed_delivery_status_comes_back_to_the_sender() {
        let outcome = SessionMessageRelayOutcome::Answered {
            status: 200,
            body: json!({ "data": { "status": "queued", "to": "c1" }, "machine": { "label": "MacBook" } }),
        };
        let response = relay_outcome_to_result("c1", outcome).unwrap();
        assert_eq!(response.status, SessionDeliveryStatus::Queued);
        assert_eq!(response.to, "c1");
    }

    #[test]
    fn recipient_error_codes_map_back_to_session_errors() {
        let answered = |status, code: &str| SessionMessageRelayOutcome::Answered {
            status,
            body: json!({ "code": code, "error": "refused" }),
        };
        assert!(matches!(
            relay_outcome_to_result("c1", answered(403, "target_is_team")),
            Err(SessionMessageError::TargetIsTeam { .. })
        ));
        assert!(matches!(
            relay_outcome_to_result("c1", answered(404, "target_not_found")),
            Err(SessionMessageError::TargetNotFound { .. })
        ));
        assert!(matches!(
            relay_outcome_to_result("c1", answered(429, "rate_limited")),
            Err(SessionMessageError::RateLimited)
        ));
        assert!(matches!(
            relay_outcome_to_result("c1", answered(503, "MACHINE_OFFLINE")),
            Err(SessionMessageError::TransportUnavailable { .. })
        ));
        assert!(matches!(
            relay_outcome_to_result("c1", SessionMessageRelayOutcome::Unavailable),
            Err(SessionMessageError::TransportUnavailable { .. })
        ));
    }
}
