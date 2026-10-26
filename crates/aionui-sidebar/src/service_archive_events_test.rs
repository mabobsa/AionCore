//! Archive notifications must follow the committed state without waiting for teardown.

use std::time::Duration;

use tokio::sync::Notify;

use super::*;

struct BlockingTeardown {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl ArchiveTeardownPorts for BlockingTeardown {
    async fn stop_conversation(&self, _user_id: &str, _id: &str) -> Result<(), String> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }

    async fn stop_team(&self, _user_id: &str, _id: &str) -> Result<(), String> {
        panic!("conversation archive must not stop a team")
    }
}

#[tokio::test]
async fn archive_notifies_after_commit_before_slow_teardown_finishes() {
    let fx = fixture().await;
    insert_conv(fx.pool(), USER, "slow", None, "{}", 100).await;
    let teardown = Arc::new(BlockingTeardown {
        entered: Notify::new(),
        release: Notify::new(),
    });
    fx.service.set_archive_teardown_ports(teardown.clone());
    let mut events = fx.event_bus.subscribe();
    let operation = fx.service.archive_conversation(USER, "slow");
    tokio::pin!(operation);

    // Poll the archive until teardown blocks; no wall-clock sleep is needed.
    tokio::select! {
        result = &mut operation => panic!("archive completed before teardown release: {result:?}"),
        () = teardown.entered.notified() => {},
    }
    let event = events
        .try_recv()
        .expect("notification must precede teardown completion");
    assert_eq!(event.name, "conversation.listChanged");
    assert_eq!(
        event.data,
        serde_json::json!({
            "user_id": USER, "conversation_id": "slow", "action": "updated", "source": null,
        })
    );
    assert!(conv_archived_at(fx.pool(), "slow").await.is_some());
    let active = fx
        .service
        .first_screen(USER, Some(10), &[], ArchiveScope::Active)
        .await
        .unwrap();
    assert!(
        all_conv_ids(&active).is_empty(),
        "a notified reader sees the committed archive"
    );

    teardown.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), operation)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    fx.service.unarchive_conversation(USER, "slow").await.unwrap();
    let restored = events.try_recv().expect("restore must also invalidate the list");
    assert_eq!(restored.name, "conversation.listChanged");
    assert_eq!(restored.data, event.data);
    assert_eq!(conv_archived_at(fx.pool(), "slow").await, None);
    let active = fx
        .service
        .first_screen(USER, Some(10), &[], ArchiveScope::Active)
        .await
        .unwrap();
    assert_eq!(all_conv_ids(&active), vec!["slow"]);
}

#[tokio::test]
async fn missing_and_foreign_conversations_do_not_broadcast_archive_changes() {
    let fx = fixture().await;
    insert_conv(fx.pool(), OTHER, "foreign", None, "{}", 100).await;
    let mut events = fx.event_bus.subscribe();
    for id in ["missing", "foreign"] {
        let err = fx.service.archive_conversation(USER, id).await.unwrap_err();
        assert!(matches!(err, SidebarError::ScopeGone));
        let err = fx.service.unarchive_conversation(USER, id).await.unwrap_err();
        assert!(matches!(err, SidebarError::ScopeGone));
    }
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_eq!(conv_archived_at(fx.pool(), "foreign").await, None);
}
