//! Isolated cross-repository WebUI proxy test server. No desktop data or agents.
use aionui_auth::{
    AuthIdentityMode, AuthRouterState, CookieConfig, JwtService, QrTokenStore, RefreshCoalescer, auth_routes,
    hash_password,
};
use aionui_db::{IUserRepository, SqliteUserRepository, init_database_memory};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::main]
async fn main() {
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone()));
    user_repo
        .set_system_user_credentials("admin", &hash_password("TestPassword123!").unwrap())
        .await
        .unwrap();
    let app = auth_routes(AuthRouterState {
        jwt_service: Arc::new(JwtService::new("isolated-test-secret".into())),
        refresh_coalescer: RefreshCoalescer::new(),
        user_repo,
        fs_adopter: None,
        cookie_config: Arc::new(CookieConfig {
            secure: false,
            same_site: "Lax",
        }),
        qr_token_store: Arc::new(QrTokenStore::new()),
        identity_mode: AuthIdentityMode::Local,
        bootstrap_secret: None,
        session_revoked_hook: None,
        local: true,
        aionpro_mode: false,
    });
    let app = app
        .route("/ws", axum::routing::get(echo_socket))
        .route("/api/stt/stream", axum::routing::get(echo_socket))
        .merge(wildcard_probe_routes());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    println!("PORT={}", listener.local_addr().unwrap().port());
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)
            })
            .await;
        })
        .await
        .unwrap();
}

async fn echo_socket(ws: axum::extract::ws::WebSocketUpgrade) -> axum::response::Response {
    ws.on_upgrade(async |mut socket| {
        if let Some(Ok(message)) = socket.recv().await {
            let _ = socket.send(message).await;
        }
    })
}

// Mirror the production wildcard route shapes, without office processes or files.
// These deliberately public fixture routes expose Axum's raw-target behavior;
// the WebUI gate must reject ambiguous requests before any handler is reached.
fn wildcard_probe_routes() -> axum::Router {
    axum::Router::new()
        .route("/api/ppt-proxy/{port}/{*path}", axum::routing::get(wildcard_probe))
        .route(
            "/api/office-watch-proxy/{port}/{*path}",
            axum::routing::get(wildcard_probe),
        )
        .route("/api/assets/logos/{*asset_path}", axum::routing::get(wildcard_probe))
        .route(
            "/api/extensions/{extension_name}/assets/{*asset_path}",
            axum::routing::get(wildcard_probe),
        )
        .route("/fixture/wildcard-hits", axum::routing::get(wildcard_hits))
        .layer(axum::Extension(Arc::new(AtomicUsize::new(0))))
}

async fn wildcard_probe(
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    axum::Extension(hits): axum::Extension<Arc<AtomicUsize>>,
) -> String {
    hits.fetch_add(1, Ordering::SeqCst);
    uri.to_string()
}

async fn wildcard_hits(axum::Extension(hits): axum::Extension<Arc<AtomicUsize>>) -> String {
    hits.load(Ordering::SeqCst).to_string()
}
