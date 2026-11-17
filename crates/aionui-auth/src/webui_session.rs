//! Session verification for the WebUI proxy, including an embedded Local backend.
#![allow(clippy::disallowed_types)] // HTTP route boundary.

use aionui_common::ApiError;
use axum::{Extension, Router, http::StatusCode, middleware::from_fn_with_state, routing::get};

use crate::{AuthState, CurrentUser, auth_middleware};

/// The proxy accepts only 204, so an older Core fails closed. This probe has no
/// side effects and no per-request action limiter (every WebUI API calls it).
pub(crate) fn routes(auth: AuthState, local: bool) -> Router {
    Router::new()
        .route(
            "/api/auth/webui-session",
            get(move |Extension(user): Extension<CurrentUser>| async move {
                // Local domain routes use this fixed identity; never let a different
                // user's valid session authorize access to the desktop's data.
                if local && user.id != "system_default_user" {
                    return Err(ApiError::Forbidden("Desktop session required".into()));
                }
                Ok(StatusCode::NO_CONTENT)
            }),
        )
        .route_layer(from_fn_with_state(auth, auth_middleware))
}
