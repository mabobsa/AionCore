//! Fork: internal route that receives session messages relayed from another
//! machine by MindNProgress. Failures carry the session tool error code so the
//! relay can hand the same code back to the sender's CLI.

use std::sync::Arc;

use aionui_auth::CurrentUser;
use axum::Router;
use axum::extract::{Extension, Json, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::json;

use crate::service::{RemoteSessionDelivery, SessionMessageService};

pub const REMOTE_SESSION_MESSAGE_PATH: &str = "/api/internal/session-messages/remote";

pub fn session_message_remote_routes(service: Arc<SessionMessageService>) -> Router {
    Router::new()
        .route(REMOTE_SESSION_MESSAGE_PATH, post(deliver))
        .with_state(service)
}

async fn deliver(
    State(service): State<Arc<SessionMessageService>>,
    Extension(user): Extension<CurrentUser>,
    Json(delivery): Json<RemoteSessionDelivery>,
) -> Response {
    match service.deliver_remote(&user.id, delivery).await {
        Ok(response) => (StatusCode::OK, Json(json!({ "success": true, "data": response }))).into_response(),
        Err(error) => {
            let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::BAD_REQUEST);
            let body = json!({ "success": false, "code": error.code(), "msg": error.to_string() });
            (status, Json(body)).into_response()
        }
    }
}
