use std::time::Duration;

use aionui_db::{init_database, init_database_memory, maybe_copy_legacy_database};
use sqlx::Row;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};

async fn open_file_pool(path: &std::path::Path) -> sqlx::SqlitePool {
    PoolOptions::<sqlx::Sqlite>::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(false)
                .foreign_keys(true)
                .busy_timeout(Duration::from_millis(5000))
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await
        .unwrap()
}

// -- T1.1 Initialization --

#[tokio::test]
async fn init_creates_users_table() {
    let db = init_database_memory().await.unwrap();

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert!(
        count.0 >= 1,
        "users table should exist and have at least the system user"
    );
}

// -- T1.2 Pragma configuration --

#[tokio::test]
async fn pragma_foreign_keys_enabled() {
    let db = init_database_memory().await.unwrap();

    let row: (i64,) = sqlx::query_as("PRAGMA foreign_keys")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.0, 1, "foreign_keys should be ON");
}

#[tokio::test]
async fn pragma_busy_timeout() {
    let db = init_database_memory().await.unwrap();

    let row: (i64,) = sqlx::query_as("PRAGMA busy_timeout")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.0, 5000, "busy_timeout should be 5000ms");
}

#[tokio::test]
async fn pragma_journal_mode_wal_on_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = init_database(&dir.path().join("test.db")).await.unwrap();

    let row: (String,) = sqlx::query_as("PRAGMA journal_mode")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(
        row.0.to_lowercase(),
        "wal",
        "journal_mode should be WAL for file-backed DB"
    );
    db.close().await;
}

// -- T1.3 Idempotent re-initialization --

#[tokio::test]
async fn idempotent_reinit_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // First init + insert test data
    let db = init_database(&path).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u1', 'alice', 'hash123', 1000, 1000)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;

    // Second init — data should persist
    let db = init_database(&path).await.unwrap();
    let row = sqlx::query("SELECT username FROM users WHERE id = 'u1'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("username"), "alice");
    db.close().await;
}

#[tokio::test]
async fn idempotent_reinit_recovers_assistant_unification_checksum_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let db = init_database(&path).await.unwrap();
    db.close().await;

    let pool = PoolOptions::<sqlx::Sqlite>::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(false)
                .foreign_keys(true)
                .busy_timeout(Duration::from_millis(5000))
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = 12")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let db = init_database(&path).await.unwrap();
    let checksum: Vec<u8> = sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 12")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_ne!(checksum, vec![0]);
    db.close().await;
}

#[tokio::test]
async fn idempotent_reinit_repairs_legacy_assistant_unification_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let db = init_database(&path).await.unwrap();
    db.close().await;

    let pool = open_file_pool(&path).await;
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
    sqlx::query("DROP TABLE assistant_preferences")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE assistant_overlays")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE assistant_definitions")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
CREATE TABLE assistant_definitions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    owner_type TEXT NOT NULL,
    source_ref TEXT,
    source_version TEXT,
    source_hash TEXT,
    name TEXT NOT NULL,
    name_i18n TEXT NOT NULL DEFAULT '{}',
    description TEXT,
    description_i18n TEXT NOT NULL DEFAULT '{}',
    avatar TEXT,
    agent_backend TEXT NOT NULL,
    rule_resource_type TEXT NOT NULL,
    rule_resource_ref TEXT,
    rule_inline_content TEXT,
    recommended_prompts TEXT NOT NULL DEFAULT '[]',
    recommended_prompts_i18n TEXT NOT NULL DEFAULT '{}',
    default_model_mode TEXT NOT NULL,
    default_model_value TEXT,
    default_permission_mode TEXT NOT NULL,
    default_permission_value TEXT,
    default_skills_mode TEXT NOT NULL,
    default_skill_ids TEXT NOT NULL DEFAULT '[]',
    custom_skill_names TEXT NOT NULL DEFAULT '[]',
    default_disabled_builtin_skill_ids TEXT NOT NULL DEFAULT '[]',
    default_mcps_mode TEXT NOT NULL,
    default_mcp_ids TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER
)
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE UNIQUE INDEX idx_assistant_definitions_source_ref
         ON assistant_definitions(source, source_ref)
         WHERE source_ref IS NOT NULL",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_assistant_definitions_source ON assistant_definitions(source)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE INDEX idx_assistant_definitions_agent_backend ON assistant_definitions(agent_backend)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE assistant_states (
            assistant_id TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL DEFAULT 1,
            sort_order INTEGER NOT NULL DEFAULT 0,
            agent_backend_override TEXT,
            last_used_at INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE assistant_preferences (
            assistant_id TEXT PRIMARY KEY,
            last_model_id TEXT,
            last_permission_value TEXT,
            last_skill_ids TEXT NOT NULL DEFAULT '[]',
            last_disabled_builtin_skill_ids TEXT NOT NULL DEFAULT '[]',
            last_mcp_ids TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_definitions (
            id, source, owner_type, source_ref, source_version, source_hash,
            name, name_i18n, description, description_i18n, avatar,
            agent_backend, rule_resource_type, rule_resource_ref, rule_inline_content,
            recommended_prompts, recommended_prompts_i18n,
            default_model_mode, default_model_value,
            default_permission_mode, default_permission_value,
            default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
            default_mcps_mode, default_mcp_ids, created_at, updated_at, deleted_at
        ) VALUES (
            'u1', 'user', 'user', 'u1', NULL, NULL,
            'Mine', '{}', 'desc', '{}', '🤖',
            'aionrs', 'user_file', 'u1', NULL,
            '[\"hello\"]', '{}',
            'auto', NULL,
            'auto', NULL,
            'fixed', '[\"pdf\"]', '[]', '[]',
            'auto', '[]', 1, 2, NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_states (assistant_id, enabled, sort_order, agent_backend_override, last_used_at, created_at, updated_at)
         VALUES ('u1', 0, 7, 'codex', 1234, 1, 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_preferences (assistant_id, last_model_id, last_permission_value, last_skill_ids,
            last_disabled_builtin_skill_ids, last_mcp_ids, created_at, updated_at)
         VALUES ('u1', 'gpt-4.1', 'workspace-write', '[\"pdf\"]', '[]', '[\"mcp-1\"]', 1, 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await.unwrap();
    pool.close().await;

    let db = init_database(&path).await.unwrap();
    let row = sqlx::query(
        "SELECT definition_id, assistant_key, avatar_type, avatar_value
         FROM assistant_definitions WHERE assistant_key = 'u1'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let definition_id = row.get::<String, _>("definition_id");
    assert_eq!(row.get::<String, _>("assistant_key"), "u1");
    assert_eq!(row.get::<String, _>("avatar_type"), "emoji");
    assert_eq!(row.get::<Option<String>, _>("avatar_value"), Some("🤖".to_string()));

    let state_definition_id: String = sqlx::query_scalar("SELECT definition_id FROM assistant_overlays LIMIT 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(state_definition_id, definition_id);

    let pref_definition_id: String = sqlx::query_scalar("SELECT definition_id FROM assistant_preferences LIMIT 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(pref_definition_id, definition_id);
    db.close().await;
}

#[tokio::test]
async fn idempotent_reinit_drops_legacy_extension_assistant_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let db = init_database(&path).await.unwrap();
    db.close().await;

    let pool = open_file_pool(&path).await;
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
    sqlx::query("DROP TABLE assistant_preferences")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE assistant_overlays")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE assistant_definitions")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
CREATE TABLE assistant_definitions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    owner_type TEXT NOT NULL,
    source_ref TEXT,
    source_version TEXT,
    source_hash TEXT,
    name TEXT NOT NULL,
    name_i18n TEXT NOT NULL DEFAULT '{}',
    description TEXT,
    description_i18n TEXT NOT NULL DEFAULT '{}',
    avatar TEXT,
    agent_backend TEXT NOT NULL,
    rule_resource_type TEXT NOT NULL,
    rule_resource_ref TEXT,
    rule_inline_content TEXT,
    recommended_prompts TEXT NOT NULL DEFAULT '[]',
    recommended_prompts_i18n TEXT NOT NULL DEFAULT '{}',
    default_model_mode TEXT NOT NULL,
    default_model_value TEXT,
    default_permission_mode TEXT NOT NULL,
    default_permission_value TEXT,
    default_skills_mode TEXT NOT NULL,
    default_skill_ids TEXT NOT NULL DEFAULT '[]',
    custom_skill_names TEXT NOT NULL DEFAULT '[]',
    default_disabled_builtin_skill_ids TEXT NOT NULL DEFAULT '[]',
    default_mcps_mode TEXT NOT NULL,
    default_mcp_ids TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER
)
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE UNIQUE INDEX idx_assistant_definitions_source_ref
         ON assistant_definitions(source, source_ref)
         WHERE source_ref IS NOT NULL",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_assistant_definitions_source ON assistant_definitions(source)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE INDEX idx_assistant_definitions_agent_backend ON assistant_definitions(agent_backend)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE assistant_states (
            assistant_id TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL DEFAULT 1,
            sort_order INTEGER NOT NULL DEFAULT 0,
            agent_backend_override TEXT,
            last_used_at INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE assistant_preferences (
            assistant_id TEXT PRIMARY KEY,
            last_model_id TEXT,
            last_permission_value TEXT,
            last_skill_ids TEXT NOT NULL DEFAULT '[]',
            last_disabled_builtin_skill_ids TEXT NOT NULL DEFAULT '[]',
            last_mcp_ids TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_definitions (
            id, source, owner_type, source_ref, source_version, source_hash,
            name, name_i18n, description, description_i18n, avatar,
            agent_backend, rule_resource_type, rule_resource_ref, rule_inline_content,
            recommended_prompts, recommended_prompts_i18n,
            default_model_mode, default_model_value,
            default_permission_mode, default_permission_value,
            default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
            default_mcps_mode, default_mcp_ids, created_at, updated_at, deleted_at
        ) VALUES (
            'ext-helper', 'extension', 'extension', 'ext-helper', NULL, NULL,
            'Helper', '{}', 'desc', '{}', NULL,
            'aionrs', 'inline', NULL, 'ctx',
            '[]', '{}',
            'unset', NULL,
            'unset', NULL,
            'fixed', '[]', '[]', '[]',
            'unset', '[]', 1, 2, NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_states (assistant_id, enabled, sort_order, agent_backend_override, last_used_at, created_at, updated_at)
         VALUES ('ext-helper', 1, 3, NULL, NULL, 1, 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_preferences (assistant_id, last_model_id, last_permission_value, last_skill_ids,
            last_disabled_builtin_skill_ids, last_mcp_ids, created_at, updated_at)
         VALUES ('ext-helper', 'gpt-4.1', NULL, '[]', '[]', '[]', 1, 2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await.unwrap();
    pool.close().await;

    let db = init_database(&path).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM assistant_definitions")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    let overlay_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM assistant_overlays")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(overlay_count, 0);
    let preference_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM assistant_preferences")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(preference_count, 0);
    db.close().await;
}

#[tokio::test]
async fn idempotent_reinit_repairs_stale_assistant_definition_default_mode_constraints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    let db = init_database(&path).await.unwrap();
    db.close().await;

    let pool = open_file_pool(&path).await;
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
    sqlx::query("DROP TABLE assistant_preferences")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE assistant_overlays")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DROP TABLE assistant_definitions")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r#"
CREATE TABLE assistant_definitions (
    definition_id TEXT PRIMARY KEY,
    assistant_key TEXT NOT NULL,
    source TEXT NOT NULL,
    owner_type TEXT NOT NULL,
    source_ref TEXT,
    source_version TEXT,
    source_hash TEXT,
    name TEXT NOT NULL,
    name_i18n TEXT NOT NULL DEFAULT '{}',
    description TEXT,
    description_i18n TEXT NOT NULL DEFAULT '{}',
    avatar_type TEXT NOT NULL DEFAULT 'none',
    avatar_value TEXT,
    agent_backend TEXT NOT NULL,
    rule_resource_type TEXT NOT NULL,
    rule_resource_ref TEXT,
    rule_inline_content TEXT,
    recommended_prompts TEXT NOT NULL DEFAULT '[]',
    recommended_prompts_i18n TEXT NOT NULL DEFAULT '{}',
    default_model_mode TEXT NOT NULL CHECK (default_model_mode IN ('auto', 'fixed')),
    default_model_value TEXT,
    default_permission_mode TEXT NOT NULL CHECK (default_permission_mode IN ('auto', 'fixed')),
    default_permission_value TEXT,
    default_skills_mode TEXT NOT NULL CHECK (default_skills_mode IN ('auto', 'fixed')),
    default_skill_ids TEXT NOT NULL DEFAULT '[]',
    custom_skill_names TEXT NOT NULL DEFAULT '[]',
    default_disabled_builtin_skill_ids TEXT NOT NULL DEFAULT '[]',
    default_mcps_mode TEXT NOT NULL CHECK (default_mcps_mode IN ('auto', 'fixed')),
    default_mcp_ids TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted_at INTEGER
)
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE UNIQUE INDEX idx_assistant_definitions_source_ref
         ON assistant_definitions(source, source_ref)
         WHERE source_ref IS NOT NULL",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE UNIQUE INDEX idx_assistant_definitions_assistant_key
         ON assistant_definitions(assistant_key)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_assistant_definitions_source ON assistant_definitions(source)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE INDEX idx_assistant_definitions_agent_backend ON assistant_definitions(agent_backend)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE assistant_overlays (
            definition_id TEXT PRIMARY KEY,
            enabled INTEGER NOT NULL DEFAULT 1,
            sort_order INTEGER NOT NULL DEFAULT 0,
            agent_backend_override TEXT,
            last_used_at INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("CREATE INDEX idx_assistant_overlays_enabled ON assistant_overlays(enabled)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("CREATE INDEX idx_assistant_overlays_sort_order ON assistant_overlays(sort_order)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE assistant_preferences (
            definition_id TEXT PRIMARY KEY,
            last_model_id TEXT,
            last_permission_value TEXT,
            last_skill_ids TEXT NOT NULL DEFAULT '[]',
            last_disabled_builtin_skill_ids TEXT NOT NULL DEFAULT '[]',
            last_mcp_ids TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_definitions (
            definition_id, assistant_key, source, owner_type, source_ref, source_version, source_hash,
            name, name_i18n, description, description_i18n, avatar_type, avatar_value,
            agent_backend, rule_resource_type, rule_resource_ref, rule_inline_content,
            recommended_prompts, recommended_prompts_i18n,
            default_model_mode, default_model_value,
            default_permission_mode, default_permission_value,
            default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
            default_mcps_mode, default_mcp_ids, created_at, updated_at, deleted_at
         ) VALUES (
            'def-1', 'assistant-1', 'builtin', 'system', 'assistant-1', NULL, NULL,
            'Builtin Assistant', '{}', 'desc', '{}', 'emoji', '🤖',
            'codex', 'builtin_asset', 'assistant-1', NULL,
            '[]', '{}',
            'auto', NULL,
            'auto', NULL,
            'fixed', '[]', '[]', '[]',
            'auto', '[]', 1000, 1000, NULL
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await.unwrap();
    pool.close().await;

    let db = init_database(&path).await.unwrap();
    let table_sql: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'assistant_definitions'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(table_sql.contains("default_model_mode IN ('unset', 'auto', 'fixed')"));
    assert!(table_sql.contains("default_permission_mode IN ('unset', 'auto', 'fixed')"));
    assert!(table_sql.contains("default_mcps_mode IN ('unset', 'auto', 'fixed')"));

    sqlx::query(
        "UPDATE assistant_definitions
         SET default_model_mode = 'unset',
             default_permission_mode = 'unset',
             default_mcps_mode = 'unset'
         WHERE definition_id = 'def-1'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    db.close().await;
}

// -- T1.4 Migrations --

#[tokio::test]
async fn migrations_applied() {
    let db = init_database_memory().await.unwrap();

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert!(count.0 >= 1, "at least one migration should be applied");
}

// -- T1.5 System default user --

#[tokio::test]
async fn system_default_user_exists() {
    let db = init_database_memory().await.unwrap();

    let row = sqlx::query("SELECT id, username, password_hash FROM users WHERE id = 'system_default_user'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("id"), "system_default_user");
    assert_eq!(row.get::<String, _>("username"), "admin");
    assert_eq!(
        row.get::<String, _>("password_hash"),
        "",
        "system user should have empty password hash"
    );
}

#[tokio::test]
async fn system_user_has_valid_timestamps() {
    let before = aionui_common::now_ms();
    let db = init_database_memory().await.unwrap();
    let after = aionui_common::now_ms();

    let row = sqlx::query("SELECT created_at, updated_at FROM users WHERE id = 'system_default_user'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    let created = row.get::<i64, _>("created_at");
    let updated = row.get::<i64, _>("updated_at");
    assert!(
        created >= before && created <= after,
        "created_at should be within test window"
    );
    assert!(
        updated >= before && updated <= after,
        "updated_at should be within test window"
    );
}

// -- Schema validation --

#[tokio::test]
async fn users_table_accepts_all_columns() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users \
         (id, username, email, password_hash, avatar_path, jwt_secret, created_at, updated_at, last_login) \
         VALUES ('u1', 'testuser', 'test@example.com', 'hash', '/avatar.png', 'secret', 1000, 2000, 3000)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let row = sqlx::query("SELECT * FROM users WHERE id = 'u1'")
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(row.get::<String, _>("email"), "test@example.com");
    assert_eq!(
        row.get::<Option<String>, _>("avatar_path"),
        Some("/avatar.png".to_string())
    );
    assert_eq!(row.get::<Option<String>, _>("jwt_secret"), Some("secret".to_string()));
    assert_eq!(row.get::<Option<i64>, _>("last_login"), Some(3000));
}

#[tokio::test]
async fn username_unique_constraint() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u1', 'duplicate', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let result = sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('u2', 'duplicate', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "duplicate username should violate unique constraint");
}

#[tokio::test]
async fn email_unique_constraint() {
    let db = init_database_memory().await.unwrap();

    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, created_at, updated_at) \
         VALUES ('u1', 'user1', 'same@example.com', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let result = sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, created_at, updated_at) \
         VALUES ('u2', 'user2', 'same@example.com', 'h', 1, 1)",
    )
    .execute(db.pool())
    .await;

    assert!(result.is_err(), "duplicate email should violate unique constraint");
}

// -- Corruption recovery --

#[tokio::test]
async fn corruption_recovery_creates_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");

    // Write invalid content to simulate corruption
    std::fs::write(&path, b"not a valid sqlite database").unwrap();

    let db = init_database(&path).await.unwrap();

    // Recovered database should work
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(count.0 >= 1, "recovered DB should have system user");

    // Backup file should exist
    let has_backup = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains("backup"));
    assert!(has_backup, "backup of corrupted file should exist");

    db.close().await;
}

// -- Directory creation --

#[tokio::test]
async fn creates_parent_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("nested").join("test.db");

    let db = init_database(&path).await.unwrap();
    assert!(path.exists(), "database file should be created in nested directory");
    db.close().await;
}

// -- Legacy database copy --

#[test]
fn copy_legacy_noop_when_no_legacy_db() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");

    maybe_copy_legacy_database(&target).unwrap();
    assert!(!target.exists(), "target should not be created when no legacy db");
}

#[test]
fn copy_legacy_noop_when_target_exists() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    std::fs::write(&legacy, b"legacy data").unwrap();
    std::fs::write(&target, b"existing target").unwrap();

    maybe_copy_legacy_database(&target).unwrap();

    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"existing target", "existing target must not be overwritten");
}

#[test]
fn copy_legacy_copies_when_target_missing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    std::fs::write(&legacy, b"legacy database content").unwrap();

    maybe_copy_legacy_database(&target).unwrap();

    assert!(target.exists(), "target should be created");
    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"legacy database content", "content should match legacy");

    let legacy_content = std::fs::read(&legacy).unwrap();
    assert_eq!(
        legacy_content, b"legacy database content",
        "legacy must not be modified"
    );
}

#[test]
fn copy_legacy_removes_wal_sidecars() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    std::fs::write(&legacy, b"legacy data").unwrap();
    std::fs::write(target.with_extension("db-wal"), b"wal").unwrap();
    std::fs::write(target.with_extension("db-shm"), b"shm").unwrap();

    maybe_copy_legacy_database(&target).unwrap();

    assert!(
        !target.with_extension("db-wal").exists(),
        "WAL sidecar should be removed"
    );
    assert!(
        !target.with_extension("db-shm").exists(),
        "SHM sidecar should be removed"
    );
}

#[test]
fn copy_legacy_overwrites_leftover_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");
    let tmp = target.with_extension("db.tmp");

    std::fs::write(&legacy, b"real data").unwrap();
    std::fs::write(&tmp, b"leftover from crash").unwrap();

    maybe_copy_legacy_database(&target).unwrap();

    assert!(target.exists(), "target should be created");
    assert!(!tmp.exists(), "tmp file should be cleaned up via rename");
    let content = std::fs::read(&target).unwrap();
    assert_eq!(content, b"real data");
}

#[tokio::test]
async fn copy_legacy_then_init_database_works() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("aionui-backend.db");
    let legacy = dir.path().join("aionui.db");

    let legacy_db = init_database(&legacy).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES ('test_user', 'alice', 'hash', 1000, 1000)",
    )
    .execute(legacy_db.pool())
    .await
    .unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(legacy_db.pool())
        .await
        .unwrap();
    legacy_db.close().await;

    maybe_copy_legacy_database(&target).unwrap();

    let db = init_database(&target).await.unwrap();

    let row = sqlx::query("SELECT username FROM users WHERE id = 'test_user'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("username"), "alice");

    let legacy_db2 = init_database(&legacy).await.unwrap();
    let row2 = sqlx::query("SELECT username FROM users WHERE id = 'test_user'")
        .fetch_one(legacy_db2.pool())
        .await
        .unwrap();
    assert_eq!(row2.get::<String, _>("username"), "alice");

    db.close().await;
    legacy_db2.close().await;
}

// -- Concurrent migrator regression (ELECTRON-1KK) --
//
// Repro for the Sentry secondary symptom: two processes opening the same
// SQLite DB on first start (e.g. Electron auto-update spawning the new
// version while the old one is still finalising shutdown, or
// `aioncore doctor` racing the server) both decide to apply the same
// migration version. sqlx-sqlite's lock()/unlock() are no-ops, so without
// the advisory file lock and retry-on-UNIQUE the slower process used to
// blow up with `UNIQUE constraint failed: _sqlx_migrations.version`.
//
// We use OS threads (not tokio::spawn) so each migrator runs on its own
// runtime — this matches the real "two processes" topology more closely
// than cooperative tasks would, and avoids the `&SqlitePool: Send` lifetime
// gymnastics that block tokio::spawn on this future.
#[test]
fn concurrent_init_database_does_not_panic_on_unique_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let p = path.clone();
        handles.push(std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move { init_database(&p).await })
        }));
    }

    // Every thread must succeed — none should bubble up the UNIQUE-constraint
    // error from `_sqlx_migrations`.
    let mut errors = Vec::new();
    for h in handles {
        match h.join().expect("thread panicked") {
            Ok(_db) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }
    assert!(
        errors.is_empty(),
        "all parallel migrators should succeed, got errors: {errors:?}"
    );

    // All migrators converged on the same baseline schema with no duplicate
    // `_sqlx_migrations` rows.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let db = init_database(&path).await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!(count.0 >= 1, "at least one migration should be recorded");

        let dup: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM (SELECT version FROM _sqlx_migrations GROUP BY version HAVING COUNT(*) > 1)",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(dup.0, 0, "no duplicate versions should ever exist in _sqlx_migrations");
        db.close().await;
    });

    // Lock file is created next to the DB and is harmless to leave behind.
    let lock = path.with_file_name("aionui-backend.db.migrate.lock");
    assert!(lock.exists(), "advisory lock file should be present after migrate");
}
