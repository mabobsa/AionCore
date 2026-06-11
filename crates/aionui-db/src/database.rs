use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use fs2::FileExt;
use sqlx::migrate::Migrator;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Row, Sqlite, SqlitePool};
use tracing::{info, warn};

use crate::error::DbError;

/// Maximum number of connections in the pool.
const MAX_CONNECTIONS: u32 = 5;

/// SQLite busy timeout in milliseconds.
const BUSY_TIMEOUT_MS: u64 = 5000;
const STARTUP_FILE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

static DB_MIGRATOR: Migrator = sqlx::migrate!();
// Historical special-case for the MCP schema reconciliation fallback.
// Keep this pinned to migration version 7 even as newer migrations land.
const MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION: i64 = 7;
const ASSISTANT_SCHEMA_RECONCILIATION_MIGRATION_VERSION: i64 = 12;

/// Wraps a SQLite connection pool with lifecycle management.
#[derive(Clone, Debug)]
pub struct Database {
    pool: SqlitePool,
}

#[derive(Debug)]
pub struct DatabaseInitError {
    stage: &'static str,
    source: DbError,
}

impl DatabaseInitError {
    pub fn new(stage: &'static str, source: DbError) -> Self {
        Self { stage, source }
    }

    pub fn stage(&self) -> &'static str {
        self.stage
    }

    pub fn into_source(self) -> DbError {
        self.source
    }

    fn source(&self) -> &DbError {
        &self.source
    }
}

impl std::fmt::Display for DatabaseInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage, self.source)
    }
}

impl std::error::Error for DatabaseInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl Database {
    /// Returns a reference to the underlying connection pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Closes all connections in the pool.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// Initialize a file-backed SQLite database.
///
/// Creates the database file and parent directories if they don't exist,
/// configures pragmas (foreign_keys, busy_timeout, journal_mode=WAL),
/// runs migrations, and ensures the system default user exists.
///
/// If initialization fails on an existing file, only explicit corruption-like
/// failures attempt recovery by backing up the corrupted file and creating a
/// fresh database. Migration mismatches and lock contention fail fast.
pub async fn init_database(path: &Path) -> Result<Database, DbError> {
    init_database_staged(path).await.map_err(DatabaseInitError::into_source)
}

pub async fn init_database_staged(path: &Path) -> Result<Database, DatabaseInitError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            DatabaseInitError::new(
                "database.open",
                DbError::Init(format!("Failed to create database directory: {e}")),
            )
        })?;
    }

    match try_init_file_staged(path).await {
        Ok(db) => Ok(db),
        Err(e) if path.exists() && should_attempt_recovery(e.source()) => {
            warn!("Database initialization failed, attempting recovery: {e}");
            recover_and_retry(path, e.into_source()).await
        }
        Err(e) => Err(e),
    }
}

/// Initialize an in-memory SQLite database (for testing).
///
/// Uses a single connection to ensure all queries share the same in-memory database.
/// Note: WAL journal mode is not available for in-memory databases.
pub async fn init_database_memory() -> Result<Database, DbError> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .map_err(|e| DbError::Init(format!("Invalid memory connection string: {e}")))?
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));

    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(DbError::Query)?;

    // In-memory DBs are not shared across processes, so no advisory lock is
    // needed (and there is no on-disk path we could create one against).
    run_migrations(&pool).await?;
    ensure_system_user(&pool).await?;

    info!("In-memory database initialized");
    Ok(Database { pool })
}

/// Copy the legacy `aionui.db` to the new target path if the target does not exist.
///
/// This enables safe upgrades: the old database remains untouched and the backend
/// operates exclusively on the copy. The copy is atomic (write to `.tmp`, then rename)
/// so a crash mid-copy leaves no half-written target file.
pub fn maybe_copy_legacy_database(target: &Path) -> Result<(), DbError> {
    if target.exists() {
        return Ok(());
    }

    let legacy = target.with_file_name("aionui.db");
    if !legacy.exists() {
        return Ok(());
    }

    let lock_path = migrate_lock_path(target);
    let _guard = match MigrateLockGuard::acquire(&lock_path) {
        Ok(guard) => Some(guard),
        Err(e) => {
            warn!(
                lock = %lock_path.display(),
                error = %e,
                "Could not acquire legacy database copy lock; continuing without it"
            );
            None
        }
    };
    if target.exists() {
        return Ok(());
    }

    let tmp = target.with_extension("db.tmp");
    retry_startup_file_op("copy legacy database", &legacy, || std::fs::copy(&legacy, &tmp))
        .map_err(|e| DbError::Init(format!("Failed to copy legacy database: {e}")))?;
    if target.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(());
    }
    match retry_startup_file_op("rename temp database", &tmp, || std::fs::rename(&tmp, target)) {
        Ok(()) => {}
        Err(e) if target.exists() => {
            warn!(
                target = %target.display(),
                tmp = %tmp.display(),
                error = %e,
                "Legacy database target appeared after rename failed; using existing target"
            );
            let _ = std::fs::remove_file(&tmp);
        }
        Err(e) => return Err(DbError::Init(format!("Failed to rename temp database: {e}"))),
    }

    let _ = std::fs::remove_file(target.with_extension("db-wal"));
    let _ = std::fs::remove_file(target.with_extension("db-shm"));

    info!("Copied legacy database {} -> {}", legacy.display(), target.display());
    Ok(())
}

async fn try_init_file_staged(path: &Path) -> Result<Database, DatabaseInitError> {
    // Serialize the whole file-backed startup path, not only the sqlx
    // migrator. Opening a fresh SQLite file also runs connection-level PRAGMAs
    // such as WAL setup, which can race before migrations start.
    let lock_path = migrate_lock_path(path);
    let _guard = match MigrateLockGuard::acquire(&lock_path) {
        Ok(guard) => Some(guard),
        Err(e) => {
            // Don't fail startup if flock isn't available (e.g. on some
            // network filesystems) - fall back to SQLite busy-timeout and
            // retry-on-conflict behavior below.
            warn!("Could not acquire database startup lock {}: {e}", lock_path.display());
            None
        }
    };

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .journal_mode(SqliteJournalMode::Wal);

    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(opts)
        .await
        .map_err(|e| DatabaseInitError::new("database.open", DbError::Query(e)))?;

    run_migrations_staged(&pool).await?;
    ensure_system_user(&pool)
        .await
        .map_err(|e| DatabaseInitError::new("database.seed", e))?;

    info!("Database initialized at {}", path.display());
    Ok(Database { pool })
}

/// Path of the cross-process advisory lock file used to serialize concurrent
/// migrators on the same database.
///
/// We put it next to the DB file so it lives on the same filesystem (avoids
/// odd flock semantics across mount points) and gets cleaned up alongside the
/// DB if a user resets their data directory.
fn migrate_lock_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    let new_name = match p.file_name().and_then(|s| s.to_str()) {
        Some(name) => format!("{name}.migrate.lock"),
        None => "aionui.migrate.lock".to_string(),
    };
    p.set_file_name(new_name);
    p
}

fn retry_startup_file_op<T, F>(operation: &str, path: &Path, mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    for (attempt, delay) in STARTUP_FILE_RETRY_DELAYS.iter().enumerate() {
        match op() {
            Ok(value) => return Ok(value),
            Err(e) if is_retryable_startup_file_error(&e) => {
                warn!(
                    operation,
                    path = %path.display(),
                    attempt = attempt + 1,
                    retry_after_ms = delay.as_millis(),
                    raw_os_error = ?e.raw_os_error(),
                    error = %e,
                    "Startup file operation failed; retrying"
                );
                std::thread::sleep(*delay);
            }
            Err(e) => return Err(e),
        }
    }
    op()
}

fn is_retryable_startup_file_error(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::Interrupted
        | std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::WouldBlock => true,
        _ => matches!(error.raw_os_error(), Some(5 | 32 | 33)),
    }
}

async fn run_migrations(pool: &SqlitePool) -> Result<(), DbError> {
    run_migrations_staged(pool)
        .await
        .map_err(DatabaseInitError::into_source)
}

async fn run_migrations_staged(pool: &SqlitePool) -> Result<(), DatabaseInitError> {
    // File-backed callers hold a cross-process startup lock before opening the
    // SQLite pool. sqlx-sqlite's Migrate impl has no-op
    // lock()/unlock() and the migrator does list_applied → apply without an
    // outer transaction, so two processes opening the same DB simultaneously
    // (e.g. Electron auto-update spawning v2.1.1 while v2.0.x is still
    // shutting down, or `aioncore doctor` racing the server) can both decide
    // to apply the same version and the slower one's INSERT into
    // `_sqlx_migrations` blows up with `UNIQUE constraint failed:
    // _sqlx_migrations.version`. The outer startup lock also covers
    // schema-repair and connection PRAGMAs before migration execution.
    ensure_schema_columns(pool)
        .await
        .map_err(|e| DatabaseInitError::new("database.schema_repair", e))?;
    // Migration 002 rebuilds tables via RENAME+DROP. Two pragmas are needed:
    // - foreign_keys=OFF: prevents DROP TABLE from triggering ON DELETE CASCADE
    // - legacy_alter_table=ON: prevents ALTER TABLE RENAME from rewriting FK
    //   references in other tables (SQLite 3.26+ rewrites them by default)
    // Both must be set outside a transaction (sqlx wraps each migration in one).
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| DatabaseInitError::new("database.migration", DbError::Query(e)))?;
    sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
        .execute(&mut *conn)
        .await
        .map_err(|e| DatabaseInitError::new("database.migration", DbError::Query(e)))?;

    let result = run_migrations_with_retry(&mut conn)
        .await
        .map_err(|e| DatabaseInitError::new("database.migration", e));

    sqlx::query("PRAGMA foreign_keys = ON; PRAGMA legacy_alter_table = OFF")
        .execute(&mut *conn)
        .await
        .map_err(|e| DatabaseInitError::new("database.migration", DbError::Query(e)))?;
    result
}

/// Run sqlx migrations with one retry on `_sqlx_migrations` UNIQUE conflict.
///
/// The advisory file lock above already serialises well-behaved processes,
/// but a UNIQUE conflict can still leak through when:
/// - flock() failed (network FS, sandbox restrictions) and we proceeded.
/// - Two processes that both bypassed the lock raced.
///
/// In every UNIQUE-conflict scenario the failing migration's transaction was
/// rolled back, so re-running `sqlx::migrate!().run` is safe: the second
/// pass sees the row that the winner committed, checksum matches (same
/// shipped binary), and the migration is treated as already applied.
async fn run_migrations_with_retry(conn: &mut sqlx::SqliteConnection) -> Result<(), DbError> {
    match DB_MIGRATOR.run(&mut *conn).await {
        Ok(()) => Ok(()),
        Err(e) if is_migrations_table_unique_conflict(&e) => {
            warn!("Concurrent migrator detected (UNIQUE conflict on _sqlx_migrations); retrying");
            DB_MIGRATOR.run(&mut *conn).await.map_err(DbError::Migration)
        }
        Err(sqlx::migrate::MigrateError::VersionMismatch(version))
            if version == MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION =>
        {
            if align_reconciled_mcp_migration_checksum(&mut *conn).await? {
                warn!(
                    "Aligned checksum for reconciled MCP migration {}; retrying",
                    MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION
                );
                DB_MIGRATOR.run(&mut *conn).await.map_err(DbError::Migration)
            } else {
                Err(DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(
                    version,
                )))
            }
        }
        Err(sqlx::migrate::MigrateError::VersionMismatch(version))
            if version == ASSISTANT_SCHEMA_RECONCILIATION_MIGRATION_VERSION =>
        {
            if align_reconciled_assistant_migration_checksum(&mut *conn).await? {
                warn!(
                    "Aligned checksum for reconciled assistant migration {}; retrying",
                    ASSISTANT_SCHEMA_RECONCILIATION_MIGRATION_VERSION
                );
                DB_MIGRATOR.run(&mut *conn).await.map_err(DbError::Migration)
            } else {
                Err(DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(
                    version,
                )))
            }
        }
        Err(e) => Err(DbError::Migration(e)),
    }
}

/// Detect the specific "another process inserted this version first" error.
///
/// sqlx wraps the SQLite error inside `MigrateError::Execute(sqlx::Error)`.
/// We match on the textual message rather than the SQLite extended error code
/// because sqlx loses the structured code by the time it bubbles up here.
fn is_migrations_table_unique_conflict(err: &sqlx::migrate::MigrateError) -> bool {
    let msg = err.to_string();
    msg.contains("UNIQUE constraint failed: _sqlx_migrations.version")
}

/// RAII guard that holds an exclusive file lock for the lifetime of the
/// migration run. Drop unlocks and best-effort closes the file handle.
struct MigrateLockGuard {
    file: std::fs::File,
}

impl MigrateLockGuard {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // Blocking lock — fs2 has no async variant. We're inside an async
        // context but startup blocks anyway and the critical section is
        // bounded (single-process migration run), so this is acceptable.
        FileExt::lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for MigrateLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Ensure columns expected by Rust models exist in the database.
///
/// `CREATE TABLE IF NOT EXISTS` does not modify existing tables, so columns
/// added after a table was first created may be missing. This function
/// safely adds any missing columns via `ALTER TABLE ADD COLUMN`.
async fn ensure_schema_columns(pool: &SqlitePool) -> Result<(), DbError> {
    reconcile_mcp_server_schema(pool).await?;
    reconcile_assistant_unification_schema(pool).await?;

    let expected: &[(&str, &str, &str)] = &[
        ("cron_jobs", "skill_content", "TEXT"),
        ("cron_jobs", "description", "TEXT"),
        ("conversations", "pinned", "INTEGER NOT NULL DEFAULT 0"),
        ("conversations", "pinned_at", "INTEGER"),
        ("teams", "agents_version", "TEXT NOT NULL DEFAULT '1.0.0'"),
    ];

    for &(table, column, col_def) in expected {
        let table_exists: bool =
            sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?")
                .bind(table)
                .fetch_one(pool)
                .await
                .map_err(DbError::Query)?;

        if !table_exists {
            continue;
        }

        let col_exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info(?) WHERE name = ?")
            .bind(table)
            .bind(column)
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;

        if !col_exists {
            let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {col_def}");
            sqlx::query(&sql).execute(pool).await.map_err(DbError::Query)?;
            info!("Added missing column {table}.{column}");
        }
    }
    Ok(())
}

async fn reconcile_assistant_unification_schema(pool: &SqlitePool) -> Result<(), DbError> {
    let table_exists: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='assistant_definitions'",
    )
    .fetch_one(pool)
    .await
    .map_err(DbError::Query)?;
    if !table_exists {
        return Ok(());
    }

    let final_shape = assistant_unification_schema_is_final(pool).await?;
    if final_shape {
        return Ok(());
    }

    rebuild_legacy_assistant_unification_schema(pool).await?;
    info!("Rebuilt assistant unification tables into final identity schema");

    Ok(())
}

async fn reconcile_mcp_server_schema(pool: &SqlitePool) -> Result<(), DbError> {
    let table_exists: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='mcp_servers'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
    if !table_exists {
        return Ok(());
    }

    let has_status: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'status'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
    let has_last_test_status: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'last_test_status'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;

    let has_deleted_at: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'deleted_at'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;

    let clean_pre_migration = has_status && !has_last_test_status && !has_deleted_at;
    if clean_pre_migration {
        return Ok(());
    }

    if has_status && !has_last_test_status {
        sqlx::query("ALTER TABLE mcp_servers RENAME COLUMN status TO last_test_status")
            .execute(pool)
            .await
            .map_err(DbError::Query)?;
        info!("Renamed mcp_servers.status to last_test_status");
    } else if !has_status && !has_last_test_status {
        sqlx::query("ALTER TABLE mcp_servers ADD COLUMN last_test_status TEXT NOT NULL DEFAULT 'disconnected'")
            .execute(pool)
            .await
            .map_err(DbError::Query)?;
        info!("Added missing column mcp_servers.last_test_status");
    }

    if !has_deleted_at {
        sqlx::query("ALTER TABLE mcp_servers ADD COLUMN deleted_at INTEGER")
            .execute(pool)
            .await
            .map_err(DbError::Query)?;
        info!("Added missing column mcp_servers.deleted_at");
    }

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_mcp_servers_deleted_at ON mcp_servers(deleted_at)")
        .execute(pool)
        .await
        .map_err(DbError::Query)?;

    record_reconciled_mcp_migration(pool).await?;

    Ok(())
}

async fn record_reconciled_mcp_migration(pool: &SqlitePool) -> Result<(), DbError> {
    let Some(migration) = DB_MIGRATOR
        .iter()
        .find(|migration| migration.version == MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
    else {
        return Ok(());
    };

    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS _sqlx_migrations (
    version BIGINT PRIMARY KEY,
    description TEXT NOT NULL,
    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    success BOOLEAN NOT NULL,
    checksum BLOB NOT NULL,
    execution_time BIGINT NOT NULL
)
        "#,
    )
    .execute(pool)
    .await
    .map_err(DbError::Query)?;

    let already_applied: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM _sqlx_migrations WHERE version = ? AND success = 1")
            .bind(MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
    if already_applied {
        return Ok(());
    }

    sqlx::query(
        r#"
INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
VALUES (?, ?, TRUE, ?, 0)
        "#,
    )
    .bind(migration.version)
    .bind(&*migration.description)
    .bind(&*migration.checksum)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    info!("Recorded reconciled MCP schema migration {}", migration.version);
    Ok(())
}

async fn align_reconciled_mcp_migration_checksum(conn: &mut sqlx::SqliteConnection) -> Result<bool, DbError> {
    let has_status: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'status'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    let has_last_test_status: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'last_test_status'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    let has_deleted_at: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'deleted_at'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;

    if has_status || !has_last_test_status || !has_deleted_at {
        return Ok(false);
    }

    let Some(migration) = DB_MIGRATOR
        .iter()
        .find(|migration| migration.version == MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
    else {
        return Ok(false);
    };

    let updated = sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
        .bind(&*migration.checksum)
        .bind(MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
        .execute(&mut *conn)
        .await
        .map_err(DbError::Query)?;

    Ok(updated.rows_affected() > 0)
}

async fn align_reconciled_assistant_migration_checksum(conn: &mut sqlx::SqliteConnection) -> Result<bool, DbError> {
    if !assistant_unification_schema_is_final_conn(conn).await? {
        return Ok(false);
    }

    let Some(migration) = DB_MIGRATOR
        .iter()
        .find(|migration| migration.version == ASSISTANT_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
    else {
        return Ok(false);
    };

    let updated = sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
        .bind(&*migration.checksum)
        .bind(ASSISTANT_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
        .execute(&mut *conn)
        .await
        .map_err(DbError::Query)?;

    Ok(updated.rows_affected() > 0)
}

async fn assistant_unification_schema_is_final(pool: &SqlitePool) -> Result<bool, DbError> {
    let has_definition_id = table_has_column(pool, "assistant_definitions", "definition_id").await?;
    let has_assistant_key = table_has_column(pool, "assistant_definitions", "assistant_key").await?;
    let has_avatar_type = table_has_column(pool, "assistant_definitions", "avatar_type").await?;
    let has_avatar_value = table_has_column(pool, "assistant_definitions", "avatar_value").await?;
    let state_uses_definition_id = table_has_column(pool, "assistant_overlays", "definition_id").await?;
    let preference_uses_definition_id = table_has_column(pool, "assistant_preferences", "definition_id").await?;
    let scalar_default_modes_support_unset = assistant_definition_scalar_default_modes_support_unset(pool).await?;

    Ok(has_definition_id
        && has_assistant_key
        && has_avatar_type
        && has_avatar_value
        && state_uses_definition_id
        && preference_uses_definition_id
        && scalar_default_modes_support_unset)
}

async fn table_exists(pool: &SqlitePool, table: &str) -> Result<bool, DbError> {
    sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?")
        .bind(table)
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)
}

async fn table_has_column(pool: &SqlitePool, table: &str, column: &str) -> Result<bool, DbError> {
    sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)
}

async fn assistant_unification_schema_is_final_conn(conn: &mut sqlx::SqliteConnection) -> Result<bool, DbError> {
    let has_definition_id = table_has_column_conn(conn, "assistant_definitions", "definition_id").await?;
    let has_assistant_key = table_has_column_conn(conn, "assistant_definitions", "assistant_key").await?;
    let has_avatar_type = table_has_column_conn(conn, "assistant_definitions", "avatar_type").await?;
    let has_avatar_value = table_has_column_conn(conn, "assistant_definitions", "avatar_value").await?;
    let state_uses_definition_id = table_has_column_conn(conn, "assistant_overlays", "definition_id").await?;
    let preference_uses_definition_id = table_has_column_conn(conn, "assistant_preferences", "definition_id").await?;
    let scalar_default_modes_support_unset = assistant_definition_scalar_default_modes_support_unset_conn(conn).await?;

    Ok(has_definition_id
        && has_assistant_key
        && has_avatar_type
        && has_avatar_value
        && state_uses_definition_id
        && preference_uses_definition_id
        && scalar_default_modes_support_unset)
}

async fn table_has_column_conn(conn: &mut sqlx::SqliteConnection, table: &str, column: &str) -> Result<bool, DbError> {
    sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(column)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::Query)
}

async fn assistant_definition_scalar_default_modes_support_unset(pool: &SqlitePool) -> Result<bool, DbError> {
    let table_sql: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'assistant_definitions'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
    Ok(table_sql
        .as_deref()
        .map(assistant_definition_sql_supports_unset_defaults)
        .unwrap_or(false))
}

async fn assistant_definition_scalar_default_modes_support_unset_conn(
    conn: &mut sqlx::SqliteConnection,
) -> Result<bool, DbError> {
    let table_sql: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'assistant_definitions'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    Ok(table_sql
        .as_deref()
        .map(assistant_definition_sql_supports_unset_defaults)
        .unwrap_or(false))
}

fn assistant_definition_sql_supports_unset_defaults(table_sql: &str) -> bool {
    let normalized = table_sql.to_ascii_lowercase().replace(char::is_whitespace, "");
    normalized.contains("default_model_mode")
        && normalized.contains("default_permission_mode")
        && normalized.contains("default_mcps_mode")
        && normalized.contains("('unset','auto','fixed')")
}

async fn rebuild_legacy_assistant_unification_schema(pool: &SqlitePool) -> Result<(), DbError> {
    let overlay_table = if table_exists(pool, "assistant_overlays").await? {
        "assistant_overlays"
    } else if table_exists(pool, "assistant_states").await? {
        "assistant_states"
    } else {
        return Err(DbError::Init(
            "assistant schema repair failed: neither assistant_overlays nor assistant_states exists".into(),
        ));
    };
    let definitions_use_internal_identity = table_has_column(pool, "assistant_definitions", "definition_id").await?;
    let overlays_use_definition_id = table_has_column(pool, overlay_table, "definition_id").await?;
    let preferences_use_definition_id = table_has_column(pool, "assistant_preferences", "definition_id").await?;

    let definition_rows = if definitions_use_internal_identity {
        sqlx::query(
            "SELECT definition_id, assistant_key, source, owner_type, source_ref, source_version, source_hash,
                name, name_i18n, description, description_i18n, avatar_type, avatar_value,
                agent_backend, rule_resource_type, rule_resource_ref, rule_inline_content,
                recommended_prompts, recommended_prompts_i18n,
                default_model_mode, default_model_value,
                default_permission_mode, default_permission_value,
                default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
                default_mcps_mode, default_mcp_ids, created_at, updated_at, deleted_at
         FROM assistant_definitions",
        )
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?
    } else {
        sqlx::query(
            "SELECT id, source, owner_type, source_ref, source_version, source_hash,
                name, name_i18n, description, description_i18n, avatar,
                agent_backend, rule_resource_type, rule_resource_ref, rule_inline_content,
                recommended_prompts, recommended_prompts_i18n,
                default_model_mode, default_model_value,
                default_permission_mode, default_permission_value,
                default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
                default_mcps_mode, default_mcp_ids, created_at, updated_at, deleted_at
         FROM assistant_definitions",
        )
        .fetch_all(pool)
        .await
        .map_err(DbError::Query)?
    };

    let state_rows = sqlx::query(&format!(
        "SELECT {} AS overlay_key, enabled, sort_order, agent_backend_override, last_used_at, created_at, updated_at
         FROM {overlay_table}",
        if overlays_use_definition_id {
            "definition_id"
        } else {
            "assistant_id"
        }
    ))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    let preference_rows = sqlx::query(&format!(
        "SELECT {} AS preference_key, last_model_id, last_permission_value, last_skill_ids,
                last_disabled_builtin_skill_ids, last_mcp_ids, created_at, updated_at
         FROM assistant_preferences",
        if preferences_use_definition_id {
            "definition_id"
        } else {
            "assistant_id"
        }
    ))
    .fetch_all(pool)
    .await
    .map_err(DbError::Query)?;

    let mut definition_id_map = std::collections::HashMap::new();
    let mut definitions = Vec::with_capacity(definition_rows.len());
    for row in definition_rows {
        if definitions_use_internal_identity {
            let source: String = row.get("source");
            let owner_type: String = row.get("owner_type");
            if source == "extension" || owner_type == "extension" {
                continue;
            }
            let definition_id: String = row.get("definition_id");
            let assistant_key: String = row.get("assistant_key");
            definition_id_map.insert(assistant_key.clone(), definition_id.clone());
            definitions.push((
                definition_id,
                assistant_key,
                source,
                owner_type,
                row.get::<Option<String>, _>("source_ref"),
                row.get::<Option<String>, _>("source_version"),
                row.get::<Option<String>, _>("source_hash"),
                row.get::<String, _>("name"),
                row.get::<String, _>("name_i18n"),
                row.get::<Option<String>, _>("description"),
                row.get::<String, _>("description_i18n"),
                row.get::<String, _>("avatar_type"),
                row.get::<Option<String>, _>("avatar_value"),
                row.get::<String, _>("agent_backend"),
                row.get::<String, _>("rule_resource_type"),
                row.get::<Option<String>, _>("rule_resource_ref"),
                row.get::<Option<String>, _>("rule_inline_content"),
                row.get::<String, _>("recommended_prompts"),
                row.get::<String, _>("recommended_prompts_i18n"),
                row.get::<String, _>("default_model_mode"),
                row.get::<Option<String>, _>("default_model_value"),
                row.get::<String, _>("default_permission_mode"),
                row.get::<Option<String>, _>("default_permission_value"),
                row.get::<String, _>("default_skills_mode"),
                row.get::<String, _>("default_skill_ids"),
                row.get::<String, _>("custom_skill_names"),
                row.get::<String, _>("default_disabled_builtin_skill_ids"),
                row.get::<String, _>("default_mcps_mode"),
                row.get::<String, _>("default_mcp_ids"),
                row.get::<i64, _>("created_at"),
                row.get::<i64, _>("updated_at"),
                row.get::<Option<i64>, _>("deleted_at"),
            ));
        } else {
            let source: String = row.get("source");
            let owner_type: String = row.get("owner_type");
            if source == "extension" || owner_type == "extension" {
                continue;
            }
            let assistant_key: String = row.get("id");
            let avatar: Option<String> = row.get("avatar");
            let definition_id = aionui_common::generate_prefixed_id("asstdef");
            let (avatar_type, avatar_value) = infer_avatar_storage(&source, avatar.as_deref());
            definition_id_map.insert(assistant_key.clone(), definition_id.clone());
            definitions.push((
                definition_id,
                assistant_key,
                source,
                owner_type,
                row.get::<Option<String>, _>("source_ref"),
                row.get::<Option<String>, _>("source_version"),
                row.get::<Option<String>, _>("source_hash"),
                row.get::<String, _>("name"),
                row.get::<String, _>("name_i18n"),
                row.get::<Option<String>, _>("description"),
                row.get::<String, _>("description_i18n"),
                avatar_type,
                avatar_value,
                row.get::<String, _>("agent_backend"),
                row.get::<String, _>("rule_resource_type"),
                row.get::<Option<String>, _>("rule_resource_ref"),
                row.get::<Option<String>, _>("rule_inline_content"),
                row.get::<String, _>("recommended_prompts"),
                row.get::<String, _>("recommended_prompts_i18n"),
                row.get::<String, _>("default_model_mode"),
                row.get::<Option<String>, _>("default_model_value"),
                row.get::<String, _>("default_permission_mode"),
                row.get::<Option<String>, _>("default_permission_value"),
                row.get::<String, _>("default_skills_mode"),
                row.get::<String, _>("default_skill_ids"),
                row.get::<String, _>("custom_skill_names"),
                row.get::<String, _>("default_disabled_builtin_skill_ids"),
                row.get::<String, _>("default_mcps_mode"),
                row.get::<String, _>("default_mcp_ids"),
                row.get::<i64, _>("created_at"),
                row.get::<i64, _>("updated_at"),
                row.get::<Option<i64>, _>("deleted_at"),
            ));
        }
    }

    let mut tx = pool.begin().await.map_err(DbError::Query)?;
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    for table in [
        "_assistant_definitions_legacy_v12",
        "_assistant_overlays_legacy_v12",
        "_assistant_preferences_legacy_v12",
    ] {
        sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    }

    sqlx::query("ALTER TABLE assistant_definitions RENAME TO _assistant_definitions_legacy_v12")
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    sqlx::query(&format!(
        "ALTER TABLE {overlay_table} RENAME TO _assistant_overlays_legacy_v12"
    ))
    .execute(&mut *tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query("ALTER TABLE assistant_preferences RENAME TO _assistant_preferences_legacy_v12")
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    for index in [
        "idx_assistant_definitions_source_ref",
        "idx_assistant_definitions_assistant_key",
        "idx_assistant_definitions_source",
        "idx_assistant_definitions_agent_backend",
        "idx_assistant_states_enabled",
        "idx_assistant_states_sort_order",
        "idx_assistant_overlays_enabled",
        "idx_assistant_overlays_sort_order",
    ] {
        sqlx::query(&format!("DROP INDEX IF EXISTS {index}"))
            .execute(&mut *tx)
            .await
            .map_err(DbError::Query)?;
    }

    create_final_assistant_unification_tables(&mut tx).await?;

    for row in &definitions {
        sqlx::query(
            "INSERT INTO assistant_definitions (
                definition_id, assistant_key, source, owner_type, source_ref, source_version, source_hash,
                name, name_i18n, description, description_i18n, avatar_type, avatar_value,
                agent_backend, rule_resource_type, rule_resource_ref, rule_inline_content,
                recommended_prompts, recommended_prompts_i18n,
                default_model_mode, default_model_value, default_permission_mode, default_permission_value,
                default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
                default_mcps_mode, default_mcp_ids, created_at, updated_at, deleted_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.0)
        .bind(&row.1)
        .bind(&row.2)
        .bind(&row.3)
        .bind(&row.4)
        .bind(&row.5)
        .bind(&row.6)
        .bind(&row.7)
        .bind(&row.8)
        .bind(&row.9)
        .bind(&row.10)
        .bind(&row.11)
        .bind(&row.12)
        .bind(&row.13)
        .bind(&row.14)
        .bind(&row.15)
        .bind(&row.16)
        .bind(&row.17)
        .bind(&row.18)
        .bind(&row.19)
        .bind(&row.20)
        .bind(&row.21)
        .bind(&row.22)
        .bind(&row.23)
        .bind(&row.24)
        .bind(&row.25)
        .bind(&row.26)
        .bind(&row.27)
        .bind(&row.28)
        .bind(row.29)
        .bind(row.30)
        .bind(row.31)
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }

    for row in state_rows {
        let overlay_key: String = row.get("overlay_key");
        let definition_id = if overlays_use_definition_id {
            overlay_key
        } else {
            let Some(definition_id) = definition_id_map.get(&overlay_key) else {
                continue;
            };
            definition_id.clone()
        };
        sqlx::query(
            "INSERT INTO assistant_overlays (
                definition_id, enabled, sort_order, agent_backend_override, last_used_at, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(definition_id)
        .bind(row.get::<bool, _>("enabled"))
        .bind(row.get::<i32, _>("sort_order"))
        .bind(row.get::<Option<String>, _>("agent_backend_override"))
        .bind(row.get::<Option<i64>, _>("last_used_at"))
        .bind(row.get::<i64, _>("created_at"))
        .bind(row.get::<i64, _>("updated_at"))
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }

    for row in preference_rows {
        let preference_key: String = row.get("preference_key");
        let definition_id = if preferences_use_definition_id {
            preference_key
        } else {
            let Some(definition_id) = definition_id_map.get(&preference_key) else {
                continue;
            };
            definition_id.clone()
        };
        sqlx::query(
            "INSERT INTO assistant_preferences (
                definition_id, last_model_id, last_permission_value, last_skill_ids,
                last_disabled_builtin_skill_ids, last_mcp_ids, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(definition_id)
        .bind(row.get::<Option<String>, _>("last_model_id"))
        .bind(row.get::<Option<String>, _>("last_permission_value"))
        .bind(row.get::<String, _>("last_skill_ids"))
        .bind(row.get::<String, _>("last_disabled_builtin_skill_ids"))
        .bind(row.get::<String, _>("last_mcp_ids"))
        .bind(row.get::<i64, _>("created_at"))
        .bind(row.get::<i64, _>("updated_at"))
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    }

    sqlx::query("DROP TABLE _assistant_definitions_legacy_v12")
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    sqlx::query("DROP TABLE _assistant_overlays_legacy_v12")
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    sqlx::query("DROP TABLE _assistant_preferences_legacy_v12")
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;

    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *tx)
        .await
        .map_err(DbError::Query)?;
    tx.commit().await.map_err(DbError::Query)?;
    Ok(())
}

async fn create_final_assistant_unification_tables(tx: &mut sqlx::Transaction<'_, Sqlite>) -> Result<(), DbError> {
    sqlx::query(
        "CREATE TABLE assistant_definitions (
            definition_id                      TEXT PRIMARY KEY,
            assistant_key                      TEXT    NOT NULL,
            source                             TEXT    NOT NULL
                                                       CHECK (source IN ('builtin', 'user', 'generated')),
            owner_type                         TEXT    NOT NULL
                                                       CHECK (owner_type IN ('system', 'user')),
            source_ref                         TEXT,
            source_version                     TEXT,
            source_hash                        TEXT,
            name                               TEXT    NOT NULL,
            name_i18n                          TEXT    NOT NULL DEFAULT '{}',
            description                        TEXT,
            description_i18n                   TEXT    NOT NULL DEFAULT '{}',
            avatar_type                        TEXT    NOT NULL DEFAULT 'none'
                                                       CHECK (avatar_type IN ('none', 'emoji', 'builtin_asset', 'user_asset')),
            avatar_value                       TEXT,
            agent_backend                      TEXT    NOT NULL,
            rule_resource_type                 TEXT    NOT NULL
                                                       CHECK (rule_resource_type IN ('none', 'builtin_asset', 'user_file', 'inline', 'extension')),
            rule_resource_ref                  TEXT,
            rule_inline_content                TEXT,
            recommended_prompts                TEXT    NOT NULL DEFAULT '[]',
            recommended_prompts_i18n           TEXT    NOT NULL DEFAULT '{}',
            default_model_mode                 TEXT    NOT NULL CHECK (default_model_mode IN ('unset', 'auto', 'fixed')),
            default_model_value                TEXT,
            default_permission_mode            TEXT    NOT NULL CHECK (default_permission_mode IN ('unset', 'auto', 'fixed')),
            default_permission_value           TEXT,
            default_skills_mode                TEXT    NOT NULL CHECK (default_skills_mode IN ('auto', 'fixed')),
            default_skill_ids                  TEXT    NOT NULL DEFAULT '[]',
            custom_skill_names                 TEXT    NOT NULL DEFAULT '[]',
            default_disabled_builtin_skill_ids TEXT    NOT NULL DEFAULT '[]',
            default_mcps_mode                  TEXT    NOT NULL CHECK (default_mcps_mode IN ('unset', 'auto', 'fixed')),
            default_mcp_ids                    TEXT    NOT NULL DEFAULT '[]',
            created_at                         INTEGER NOT NULL,
            updated_at                         INTEGER NOT NULL,
            deleted_at                         INTEGER
        )",
    )
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "CREATE UNIQUE INDEX idx_assistant_definitions_source_ref
         ON assistant_definitions(source, source_ref)
         WHERE source_ref IS NOT NULL",
    )
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query(
        "CREATE UNIQUE INDEX idx_assistant_definitions_assistant_key
         ON assistant_definitions(assistant_key)",
    )
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query("CREATE INDEX idx_assistant_definitions_source ON assistant_definitions(source)")
        .execute(&mut **tx)
        .await
        .map_err(DbError::Query)?;
    sqlx::query("CREATE INDEX idx_assistant_definitions_agent_backend ON assistant_definitions(agent_backend)")
        .execute(&mut **tx)
        .await
        .map_err(DbError::Query)?;

    sqlx::query(
        "CREATE TABLE assistant_overlays (
            definition_id         TEXT PRIMARY KEY,
            enabled               INTEGER NOT NULL DEFAULT 1,
            sort_order            INTEGER NOT NULL DEFAULT 0,
            agent_backend_override TEXT,
            last_used_at          INTEGER,
            created_at            INTEGER NOT NULL,
            updated_at            INTEGER NOT NULL,
            FOREIGN KEY (definition_id) REFERENCES assistant_definitions(definition_id) ON DELETE CASCADE
        )",
    )
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;
    sqlx::query("CREATE INDEX idx_assistant_overlays_enabled ON assistant_overlays(enabled)")
        .execute(&mut **tx)
        .await
        .map_err(DbError::Query)?;
    sqlx::query("CREATE INDEX idx_assistant_overlays_sort_order ON assistant_overlays(sort_order)")
        .execute(&mut **tx)
        .await
        .map_err(DbError::Query)?;

    sqlx::query(
        "CREATE TABLE assistant_preferences (
            definition_id                    TEXT PRIMARY KEY,
            last_model_id                    TEXT,
            last_permission_value            TEXT,
            last_skill_ids                   TEXT    NOT NULL DEFAULT '[]',
            last_disabled_builtin_skill_ids  TEXT    NOT NULL DEFAULT '[]',
            last_mcp_ids                     TEXT    NOT NULL DEFAULT '[]',
            created_at                       INTEGER NOT NULL,
            updated_at                       INTEGER NOT NULL,
            FOREIGN KEY (definition_id) REFERENCES assistant_definitions(definition_id) ON DELETE CASCADE
        )",
    )
    .execute(&mut **tx)
    .await
    .map_err(DbError::Query)?;

    Ok(())
}

fn infer_avatar_storage(source: &str, avatar: Option<&str>) -> (String, Option<String>) {
    let Some(value) = avatar.map(str::trim).filter(|value| !value.is_empty()) else {
        return ("none".to_string(), None);
    };

    let avatar_type = if looks_like_avatar_asset(value) {
        match source {
            "builtin" => "builtin_asset",
            _ => "user_asset",
        }
    } else {
        "emoji"
    };

    (avatar_type.to_string(), Some(value.to_string()))
}

fn looks_like_avatar_asset(value: &str) -> bool {
    value.contains('/') || (Path::new(value).extension().is_some() && !value.starts_with('.'))
}

/// Ensure the system default user exists.
///
/// Uses INSERT OR IGNORE so it is safe to call on every startup.
/// The system user has an empty password hash, which signals "needs setup".
/// Username defaults to `admin` — matches the legacy web-host login flow so
/// users upgrading from pre-M6 builds keep the same login username.
async fn ensure_system_user(pool: &SqlitePool) -> Result<(), DbError> {
    let now = aionui_common::now_ms();
    sqlx::query(
        "INSERT OR IGNORE INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind("system_default_user")
    .bind("admin")
    .bind("")
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

async fn recover_and_retry(path: &Path, original_error: DbError) -> Result<Database, DatabaseInitError> {
    let backup_path = format!("{}.backup.{}", path.display(), aionui_common::now_ms());
    warn!("Backing up corrupted database to: {backup_path}");

    std::fs::rename(path, &backup_path).map_err(|e| {
        DatabaseInitError::new(
            "database.recovery",
            DbError::Init(format!(
                "Recovery failed: could not backup corrupted database: {e}. \
                 Original error: {original_error}"
            )),
        )
    })?;

    match try_init_file_staged(path).await {
        Ok(db) => {
            warn!(
                code = "BOOTSTRAP_RECOVERED_DATABASE_CORRUPTION",
                stage = "database.recovery",
                backup_path = %backup_path,
                "Database recovered after corruption-like startup failure"
            );
            Ok(db)
        }
        Err(retry_err) => Err(DatabaseInitError::new(
            "database.recovery",
            DbError::Init(format!(
                "Recovery failed after backup: {retry_err}. Original error: {original_error}"
            )),
        )),
    }
}

fn should_attempt_recovery(err: &DbError) -> bool {
    match err {
        DbError::Migration(_) => false,
        DbError::NotFound(_) | DbError::Conflict(_) => false,
        DbError::Query(_) | DbError::Init(_) => is_corruption_like_error(err),
    }
}

fn is_corruption_like_error(err: &DbError) -> bool {
    let message = err.to_string().to_ascii_lowercase();

    [
        "sqlite_corrupt",
        "database disk image is malformed",
        "file is not a database",
        "sqlite_notadb",
        "malformed database schema",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_skips_migration_version_mismatch() {
        let err = DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(6));

        assert!(
            !should_attempt_recovery(&err),
            "migration checksum mismatch must not trigger recovery"
        );
    }

    #[test]
    fn recovery_skips_lock_contention_errors() {
        let err = DbError::Init("database is locked".into());

        assert!(
            !should_attempt_recovery(&err),
            "lock contention must not trigger recovery"
        );
    }

    #[test]
    fn recovery_allows_corruption_like_errors() {
        let err = DbError::Init("database disk image is malformed".into());

        assert!(
            should_attempt_recovery(&err),
            "corruption-like failures should trigger recovery"
        );
    }

    #[tokio::test]
    async fn migration_preserves_fk_references() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool();

        let fk_table: String = sqlx::query_scalar(
            "SELECT \"table\" FROM pragma_foreign_key_list('messages') WHERE \"from\"='conversation_id'",
        )
        .fetch_one(pool)
        .await
        .unwrap();

        assert_eq!(fk_table, "conversations");
    }

    #[test]
    fn migrations_table_unique_conflict_detected_from_message() {
        // Build the same Execute(sqlx::Error) shape that surfaces when two
        // processes race on `INSERT INTO _sqlx_migrations`. The detector has
        // to match on the textual message because the SQLite extended code
        // is not preserved on the path through MigrateError.
        let inner = sqlx::Error::Protocol("UNIQUE constraint failed: _sqlx_migrations.version".to_string());
        let err = sqlx::migrate::MigrateError::Execute(inner);
        assert!(is_migrations_table_unique_conflict(&err));
    }

    #[test]
    fn migrations_table_unique_conflict_ignores_other_errors() {
        let other = sqlx::migrate::MigrateError::VersionMismatch(3);
        assert!(!is_migrations_table_unique_conflict(&other));

        let unrelated = sqlx::migrate::MigrateError::Execute(sqlx::Error::Protocol(
            "UNIQUE constraint failed: users.username".to_string(),
        ));
        assert!(!is_migrations_table_unique_conflict(&unrelated));
    }

    #[test]
    fn migrate_lock_path_sits_next_to_db() {
        let db = Path::new("/var/lib/aionui/aionui-backend.db");
        let lock = migrate_lock_path(db);
        assert_eq!(lock.parent(), db.parent());
        assert_eq!(lock.file_name().unwrap(), "aionui-backend.db.migrate.lock");
    }

    #[test]
    fn startup_file_retry_handles_windows_transient_lock_errors() {
        for code in [5, 32, 33] {
            let err = std::io::Error::from_raw_os_error(code);
            assert!(
                is_retryable_startup_file_error(&err),
                "Windows startup file error {code} should be retryable"
            );
        }
    }

    #[test]
    fn startup_file_retry_rejects_non_transient_errors() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        assert!(!is_retryable_startup_file_error(&err));
    }
}
