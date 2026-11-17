//! Isolated cross-repository WebUI proxy test server. No desktop data or agents.
use aionui_auth::{
    AuthIdentityMode, AuthRouterState, CookieConfig, JwtService, QrTokenStore, RefreshCoalescer, auth_routes,
    hash_password,
};
use aionui_db::{IUserRepository, SqliteUserRepository, init_database_memory};
use std::sync::Arc;

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
        .route("/api/stt/stream", axum::routing::get(echo_socket));
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
