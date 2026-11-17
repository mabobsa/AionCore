use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn test_local_mode_skips_auth() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();

    let router = aionui_app::create_router(&services).await.expect("build router");

    // Health check should work
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // An authenticated endpoint should work WITHOUT a token in local mode
    let response = router
        .oneshot(Request::builder().uri("/api/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    services.database.close().await;
}

#[tokio::test]
async fn test_local_mode_rejects_browser_requests_from_outside_the_app() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");
    let get = |headers: &[(&str, &str)]| {
        let mut request = Request::builder().uri("/api/settings");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        router.clone().oneshot(request.body(Body::empty()).unwrap())
    };
    let allow_origin = |response: &axum::response::Response| {
        response
            .headers()
            .get("access-control-allow-origin")
            .map(|value| value.to_str().unwrap().to_owned())
    };

    // The desktop app, Node/Rust clients and the WebUI proxy keep working.
    for headers in [
        vec![],
        vec![("host", "127.0.0.1:60601")],
        vec![("host", "127.0.0.1:60601"), ("origin", "http://127.0.0.1:5300")],
        vec![("host", "localhost:60601"), ("origin", "file://")],
    ] {
        let response = get(&headers).await.unwrap();
        assert_ne!(response.status(), StatusCode::FORBIDDEN, "{headers:?}");
        let origin = headers
            .iter()
            .find(|(name, _)| *name == "origin")
            .map(|(_, value)| *value);
        assert_eq!(allow_origin(&response).as_deref(), origin, "{headers:?}");
    }

    // A web page or a DNS-rebound host name is refused and gets no CORS grant.
    for headers in [
        vec![("host", "127.0.0.1:60601"), ("origin", "https://evil.example")],
        vec![("host", "127.0.0.1:60601"), ("origin", "null")],
        vec![("host", "attacker.example:60601")],
    ] {
        let response = get(&headers).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{headers:?}");
        assert_eq!(allow_origin(&response), None, "{headers:?}");
    }

    let preflight = router
        .clone()
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/conversations")
                .header("host", "127.0.0.1:60601")
                .header("origin", "https://evil.example")
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allow_origin(&preflight), None);

    let websocket = router
        .oneshot(
            Request::builder()
                .uri("/ws")
                .header("host", "127.0.0.1:60601")
                .header("origin", "https://evil.example")
                .header("connection", "upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(websocket.status(), StatusCode::FORBIDDEN);

    services.database.close().await;
}

#[tokio::test]
async fn test_non_local_mode_requires_auth() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();

    let router = aionui_app::create_router(&services).await.expect("build router");

    let response = router
        .oneshot(Request::builder().uri("/api/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "UNAUTHORIZED");

    services.database.close().await;
}

#[tokio::test]
async fn test_local_mode_aionpro_requires_session_and_allows_bootstrap_provision_without_csrf() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some("bootstrap-secret".to_string()),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");

    let business_response = router
        .clone()
        .oneshot(Request::builder().uri("/api/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(business_response.status(), StatusCode::UNAUTHORIZED);

    let provision_response = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/auth/internal/external-users/pro-app")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Pro App User"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(provision_response.status(), StatusCode::OK);
    let body = to_bytes(provision_response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["user_type"], "aionpro");
    assert_ne!(json["data"]["user_id"], "system_default_user");

    services.database.close().await;
}
