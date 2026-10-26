use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

use aionui_ai_agent::{AgentError, IWorkerTaskManager};
use aionui_api_types::{CreateConversationRequest, WebSocketMessage};
use aionui_common::{AgentKillReason, TimestampMs};
use aionui_conversation::service::native_history::{NativeHistoryCleaner, NativeHistoryOutcome};
use aionui_conversation::skill_resolver::SkillResolver;
use aionui_conversation::{ConversationError, ConversationService};
use aionui_db::models::AcpSessionRow;
use aionui_db::{
    IAcpSessionRepository, SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteConversationRepository,
    init_database_memory,
};
use aionui_realtime::EventBroadcaster;
use serde_json::json;

const OWNER: &str = "system_default_user";
const THREAD: &str = "11111111-1111-4111-8111-111111111111";

struct NoTasks {
    pool: sqlx::SqlitePool,
    hang_stop: AtomicBool,
    stopped: Notify,
    builds: AtomicUsize,
    blocked_build: AtomicBool,
    build_entered: Notify,
    build_proceed: Notify,
}
#[async_trait::async_trait]
impl IWorkerTaskManager for NoTasks {
    fn get_task(&self, _: &str) -> Option<aionui_ai_agent::AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: aionui_ai_agent::types::BuildTaskOptions,
    ) -> Result<aionui_ai_agent::AgentInstance, AgentError> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        self.build_entered.notify_one();
        if self.blocked_build.load(Ordering::SeqCst) {
            self.build_proceed.notified().await;
        }
        Err(AgentError::internal("unused"))
    }
    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        id: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        self.stopped.notify_one();
        if self.hang_stop.load(Ordering::SeqCst) {
            return Box::pin(std::future::pending());
        }
        let pool = self.pool.clone();
        let id = id.to_owned();
        Box::pin(async move {
            // Model the last binding emitted by a first turn while termination
            // is in progress. Cleanup must read this anchor after awaiting stop.
            sqlx::query("UPDATE acp_session SET session_id=? WHERE conversation_id=? AND session_id IS NULL")
                .bind(THREAD)
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        })
    }
    async fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct NoSkills;
#[async_trait::async_trait]
impl SkillResolver for NoSkills {
    async fn auto_inject_names(&self) -> Vec<String> {
        vec![]
    }
    async fn resolve_skills(&self, _: &[String]) -> Vec<aionui_extension::ResolvedAgentSkill> {
        vec![]
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<WebSocketMessage<serde_json::Value>>>);
impl EventBroadcaster for Events {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.0.lock().unwrap().push(event);
    }
}

struct Cleaner {
    pool: sqlx::SqlitePool,
    fail: bool,
    calls: Mutex<Vec<Vec<String>>>,
    blocked: AtomicBool,
    entered: Notify,
    proceed: Notify,
}
#[async_trait::async_trait]
impl NativeHistoryCleaner for Cleaner {
    async fn delete_codex_history(
        &self,
        user: &str,
        session: &AcpSessionRow,
        protected: &[String],
    ) -> Result<NativeHistoryOutcome, ConversationError> {
        assert_eq!(user, OWNER);
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM conversations c JOIN acp_session s ON s.conversation_id=c.id WHERE c.id=?",
        )
        .bind(&session.conversation_id)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        assert_eq!(
            rows, 1,
            "both local rows must survive until native cleanup has succeeded"
        );
        self.calls.lock().unwrap().push(protected.to_vec());
        self.entered.notify_one();
        if self.blocked.load(Ordering::SeqCst) {
            self.proceed.notified().await;
        }
        if self.fail {
            return Err(ConversationError::Internal {
                reason: "fixture native failure".into(),
            });
        }
        Ok(if protected.iter().any(|id| Some(id) == session.session_id.as_ref()) {
            NativeHistoryOutcome::RetainedReference
        } else {
            NativeHistoryOutcome::Deleted
        })
    }
}

struct Fixture {
    db: aionui_db::Database,
    service: ConversationService,
    sessions: Arc<SqliteAcpSessionRepository>,
    cleaner: Arc<Cleaner>,
    events: Arc<Events>,
    directory: tempfile::TempDir,
    tasks: Arc<NoTasks>,
}

impl Fixture {
    async fn new(fail: bool, install_cleaner: bool) -> Self {
        let db = init_database_memory().await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let sessions = Arc::new(SqliteAcpSessionRepository::new(db.pool().clone()));
        let events = Arc::new(Events::default());
        let cleaner = Arc::new(Cleaner {
            pool: db.pool().clone(),
            fail,
            calls: Mutex::new(vec![]),
            blocked: AtomicBool::new(false),
            entered: Notify::new(),
            proceed: Notify::new(),
        });
        let tasks = Arc::new(NoTasks {
            pool: db.pool().clone(),
            hang_stop: AtomicBool::new(false),
            stopped: Notify::new(),
            builds: AtomicUsize::new(0),
            blocked_build: AtomicBool::new(false),
            build_entered: Notify::new(),
            build_proceed: Notify::new(),
        });
        let mut service = ConversationService::new(
            directory.path().into(),
            events.clone(),
            Arc::new(NoSkills),
            tasks.clone(),
            Arc::new(SqliteConversationRepository::new(db.pool().clone())),
            Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone())),
            sessions.clone(),
        );
        if install_cleaner {
            service = service.with_native_history_cleaner(cleaner.clone());
        }
        Self {
            db,
            service,
            sessions,
            cleaner,
            events,
            directory,
            tasks,
        }
    }

    async fn conversation(&self, backend: &str) -> String {
        let req: CreateConversationRequest = serde_json::from_value(json!({
            "type":"acp", "extra":{"backend":backend, "workspace":self.directory.path(), "custom_workspace":true}
        }))
        .unwrap();
        let conv = self.service.create(OWNER, req).await.unwrap();
        self.sessions
            .update_session_id_for_user(OWNER, &conv.id, THREAD)
            .await
            .unwrap();
        conv.id
    }

    async fn physical_session_count(&self, id: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM acp_session WHERE conversation_id=?")
            .bind(id)
            .fetch_one(self.db.pool())
            .await
            .unwrap()
    }
}

fn start_delete(f: &Fixture, id: &str) -> tokio::task::JoinHandle<Result<(), ConversationError>> {
    let service = f.service.clone();
    let id = id.to_owned();
    tokio::spawn(async move { service.delete(OWNER, &id).await })
}

#[tokio::test]
async fn deletion_drains_a_warmup_already_building_before_stopping_the_agent() {
    let f = Fixture::new(false, true).await;
    let id = f.conversation("codex").await;
    f.tasks.blocked_build.store(true, Ordering::SeqCst);
    let service = f.service.clone();
    let tasks: Arc<dyn IWorkerTaskManager> = f.tasks.clone();
    let warmup_id = id.clone();
    let warmup = tokio::spawn(async move { service.warmup(OWNER, &warmup_id, &tasks).await });
    f.tasks.build_entered.notified().await;
    let deletion = start_delete(&f, &id);
    while !f.service.runtime_state().is_deleting(&id) {
        tokio::task::yield_now().await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), f.tasks.stopped.notified())
            .await
            .is_err()
    );
    f.tasks.build_proceed.notify_one();
    assert!(matches!(warmup.await.unwrap(), Err(ConversationError::Internal { .. })));
    deletion.await.unwrap().unwrap();
    assert!(f.service.conversation_repo().get(OWNER, &id).await.unwrap().is_none());
}

#[tokio::test]
async fn native_timeout_preserves_rows_and_allows_retry() {
    let f = Fixture::new(false, true).await;
    let id = f.conversation("codex").await;
    f.cleaner.blocked.store(true, Ordering::SeqCst);
    let deletion = start_delete(&f, &id);
    f.cleaner.entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(141)).await;
    let error = deletion.await.unwrap().unwrap_err();
    tokio::time::resume();
    assert!(matches!(error, ConversationError::Timeout { reason } if reason.contains("native_history")));
    assert_eq!(f.physical_session_count(&id).await, 1);
    assert!(!f.service.runtime_state().is_deleting(&id));
    f.cleaner.blocked.store(false, Ordering::SeqCst);
    f.service.delete(OWNER, &id).await.unwrap();
    assert!(f.service.conversation_repo().get(OWNER, &id).await.unwrap().is_none());
}

#[tokio::test]
async fn stop_timeout_never_starts_history_cleanup() {
    let f = Fixture::new(false, true).await;
    let id = f.conversation("codex").await;
    f.tasks.hang_stop.store(true, Ordering::SeqCst);
    let deletion = start_delete(&f, &id);
    f.tasks.stopped.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = deletion.await.unwrap().unwrap_err();
    tokio::time::resume();
    assert!(matches!(error, ConversationError::Timeout { reason } if reason.contains("stop_agent")));
    assert!(f.cleaner.calls.lock().unwrap().is_empty());
    assert!(!f.service.runtime_state().is_deleting(&id));
}

#[tokio::test]
async fn deleting_rejects_warmup_ensure_restart_and_duplicate_delete_after_turn_finishes() {
    let f = Fixture::new(false, true).await;
    let id = f.conversation("codex").await;
    let state = f.service.runtime_state();
    let mut turn = state.try_claim_turn(&id, "turn").unwrap();
    f.cleaner.blocked.store(true, Ordering::SeqCst);
    let deletion = start_delete(&f, &id);
    f.cleaner.entered.notified().await;
    assert!(turn.release());
    let tasks: Arc<dyn IWorkerTaskManager> = f.tasks.clone();
    assert!(matches!(
        f.service.warmup(OWNER, &id, &tasks).await,
        Err(ConversationError::Busy { .. })
    ));
    assert!(matches!(
        f.service.ensure_runtime(OWNER, &id, &tasks).await,
        Err(ConversationError::Busy { .. })
    ));
    assert!(matches!(
        f.service.restart_runtime(OWNER, &id, &tasks).await,
        Err(ConversationError::Busy { .. })
    ));
    assert!(matches!(
        f.service.delete(OWNER, &id).await,
        Err(ConversationError::Busy { .. })
    ));
    assert_eq!(f.tasks.builds.load(Ordering::SeqCst), 0);
    f.cleaner.proceed.notify_one();
    deletion.await.unwrap().unwrap();
    assert!(!state.is_deleting(&id));
}

#[tokio::test]
async fn waiting_for_another_delete_times_out_without_clearing_its_flag() {
    let f = Fixture::new(false, true).await;
    let first = f.conversation("codex").await;
    let second = f.conversation("codex").await;
    f.cleaner.blocked.store(true, Ordering::SeqCst);
    let deletion = start_delete(&f, &first);
    f.cleaner.entered.notified().await;
    let waiting = start_delete(&f, &second);
    while !f.service.runtime_state().is_deleting(&second) {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    let error = waiting.await.unwrap().unwrap_err();
    tokio::time::resume();
    assert!(matches!(error, ConversationError::Timeout { reason } if reason.contains("history_lock")));
    assert!(!f.service.runtime_state().is_deleting(&second));
    assert!(f.service.runtime_state().is_deleting(&first));
    deletion.abort();
    assert!(deletion.await.unwrap_err().is_cancelled());
    assert!(!f.service.runtime_state().is_deleting(&first));
}

#[tokio::test]
async fn native_cleanup_precedes_atomic_local_delete() {
    let f = Fixture::new(false, true).await;
    let id = f.conversation("codex").await;
    f.events.0.lock().unwrap().clear();
    f.service.delete(OWNER, &id).await.unwrap();
    assert_eq!(f.cleaner.calls.lock().unwrap().len(), 1);
    assert!(f.service.conversation_repo().get(OWNER, &id).await.unwrap().is_none());
    assert_eq!(f.physical_session_count(&id).await, 0);
    assert_eq!(
        f.events
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.data["action"] == "deleted")
            .count(),
        1
    );
}

#[tokio::test]
async fn first_turn_binding_is_read_after_process_termination() {
    let f = Fixture::new(false, true).await;
    let req = serde_json::from_value(json!({
        "type":"acp", "extra":{"backend":"codex", "workspace":f.directory.path(), "custom_workspace":true}
    }))
    .unwrap();
    let id = f.service.create(OWNER, req).await.unwrap().id;
    assert!(
        f.sessions
            .get_for_user(OWNER, &id)
            .await
            .unwrap()
            .unwrap()
            .session_id
            .is_none()
    );
    f.service.delete(OWNER, &id).await.unwrap();
    assert_eq!(f.cleaner.calls.lock().unwrap().len(), 1);
    assert_eq!(f.physical_session_count(&id).await, 0);
}

#[tokio::test]
async fn native_failure_preserves_conversation_anchor_and_retry_state() {
    let f = Fixture::new(true, true).await;
    let id = f.conversation("codex").await;
    f.events.0.lock().unwrap().clear();
    let err = f.service.delete(OWNER, &id).await.unwrap_err();
    assert!(matches!(err, ConversationError::Internal { ref reason } if reason == "fixture native failure"));
    assert!(f.service.conversation_repo().get(OWNER, &id).await.unwrap().is_some());
    assert_eq!(f.physical_session_count(&id).await, 1);
    assert!(!f.service.runtime_state().is_deleting(&id));
    assert!(f.events.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cross_user_delete_never_invokes_native_cleanup() {
    let f = Fixture::new(false, true).await;
    let id = f.conversation("codex").await;
    assert!(matches!(
        f.service.delete("another-user", &id).await,
        Err(ConversationError::NotFound { .. })
    ));
    assert!(f.cleaner.calls.lock().unwrap().is_empty());
    assert_eq!(f.physical_session_count(&id).await, 1);
}

#[tokio::test]
async fn deleting_alias_preserves_the_other_conversations_anchor() {
    let f = Fixture::new(false, true).await;
    let deleting = f.conversation("codex").await;
    let retained = f.conversation("codex").await;
    f.service.delete(OWNER, &deleting).await.unwrap();
    assert!(f.cleaner.calls.lock().unwrap()[0].contains(&THREAD.to_owned()));
    assert_eq!(f.physical_session_count(&deleting).await, 0);
    assert_eq!(f.physical_session_count(&retained).await, 1);
}

#[tokio::test]
async fn missing_native_cleaner_keeps_codex_history_but_does_not_block_other_backends() {
    let f = Fixture::new(false, false).await;
    let codex = f.conversation("codex").await;
    assert!(matches!(
        f.service.delete(OWNER, &codex).await,
        Err(ConversationError::Internal { .. })
    ));
    assert_eq!(f.physical_session_count(&codex).await, 1);
    let claude = f.conversation("claude").await;
    f.service.delete(OWNER, &claude).await.unwrap();
    assert_eq!(f.physical_session_count(&claude).await, 0);
}
