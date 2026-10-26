//! Archive ordering uses the archive event, independently of creation/activity.

use super::*;

fn ordered_ids(items: &[SidebarItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item {
            SidebarItem::Conversation { conversation } => conversation.id.clone(),
            SidebarItem::Team(team) => team.team_id.clone(),
        })
        .collect()
}

#[tokio::test]
async fn archiving_an_old_conversation_moves_it_to_top_without_changing_activity() {
    let fx = fixture().await;
    let pool = fx.pool();
    for (id, activity) in [("old", 10), ("recent", 100), ("already-archived", 1000)] {
        insert_conv(pool, USER, id, None, "{}", activity).await;
    }
    archive_conv(pool, "already-archived", 5000).await;

    fx.service.archive_conversation(USER, "old").await.unwrap();
    let archived = fx
        .service
        .first_screen(USER, Some(1), &[], ArchiveScope::Archived)
        .await
        .unwrap();
    let chats = find_chats(&archived);
    assert_eq!(ordered_ids(&chats.items), vec!["old"]);
    assert!(chats.has_more, "the newly archived item is in the first window");
    let SidebarItem::Conversation { conversation } = &chats.items[0] else {
        panic!("expected conversation");
    };
    assert_eq!(conversation.created_at, 10);
    assert_eq!(conversation.modified_at, 10, "archive time must not replace activity");

    fx.service.unarchive_conversation(USER, "old").await.unwrap();
    let active = fx
        .service
        .first_screen(USER, Some(10), &[], ArchiveScope::Active)
        .await
        .unwrap();
    assert_eq!(ordered_ids(&find_chats(&active).items), vec!["recent", "old"]);

    // Restoring and archiving again must use the new archive event.
    fx.service.archive_conversation(USER, "old").await.unwrap();
    let rearchived = fx
        .service
        .first_screen(USER, Some(10), &[], ArchiveScope::Archived)
        .await
        .unwrap();
    assert_eq!(
        ordered_ids(&find_chats(&rearchived).items),
        vec!["old", "already-archived"]
    );
}

#[tokio::test]
async fn archive_paging_orders_mixed_items_by_archive_time_with_stable_ties() {
    let fx = fixture().await;
    let pool = fx.pool();
    for (id, activity, archived_at) in [
        ("newest", 10, 400),
        ("a", 30, 300),
        ("b", 50, 300),
        ("oldest", 1000, 100),
    ] {
        insert_conv(pool, USER, id, None, "{}", activity).await;
        archive_conv(pool, id, archived_at).await;
    }
    insert_team(pool, USER, "team", "", None, 10000).await;
    insert_member(pool, USER, "member", "team", 20000).await;
    SqliteSidebarStore::new(pool.clone())
        .set_team_archived(USER, "team", Some(300))
        .await
        .unwrap();
    insert_conv(pool, USER, "active", None, "{}", 30000).await;
    insert_conv(pool, OTHER, "foreign", None, "{}", 40000).await;
    archive_conv(pool, "foreign", 50000).await;

    // Page boundaries cut through equal timestamps and across item types.
    for limit in [1, 2, 3] {
        let first = fx
            .service
            .first_screen(USER, Some(limit), &[], ArchiveScope::Archived)
            .await
            .unwrap();
        let group = find_chats(&first);
        let mut seen = group.items.clone();
        let mut cursor = group.next_cursor.clone();
        let mut pages = 1;
        while let Some(raw) = cursor {
            assert!(pages < 6, "paging must terminate");
            let page = fx
                .service
                .items(USER, "chats", Some(&raw), Some(limit), ArchiveScope::Archived)
                .await
                .unwrap();
            assert_eq!(page.has_more, page.next_cursor.is_some());
            seen.extend(page.items);
            cursor = page.next_cursor;
            pages += 1;
        }
        assert_eq!(ordered_ids(&seen), vec!["newest", "a", "b", "team", "oldest"]);
        let team = seen
            .iter()
            .find_map(|item| match item {
                SidebarItem::Team(team) => Some(team),
                _ => None,
            })
            .unwrap();
        assert_eq!(team.member_conversation_ids, vec!["member"]);
        assert_eq!(
            team.updated_at, 20000,
            "team activity stays distinct from archive ordering"
        );
    }

    let first = fx
        .service
        .first_screen(USER, Some(1), &[], ArchiveScope::Archived)
        .await
        .unwrap();
    let archived_cursor = find_chats(&first).next_cursor.as_deref().unwrap();
    let err = fx
        .service
        .items(USER, "chats", Some(archived_cursor), Some(1), ArchiveScope::Active)
        .await
        .unwrap_err();
    assert!(matches!(err, SidebarError::BadRequest(message) if message == "cursor version mismatch"));
}

#[tokio::test]
async fn archive_groups_follow_latest_archive_including_projects_directories_and_chats() {
    let fx = fixture().await;
    let pool = fx.pool();
    for (project_id, activity, archived_at) in [("earlier", 10000, 100), ("later", 10, 300)] {
        let path = fx._tmp.path().join(project_id).display().to_string();
        insert_std_project(pool, USER, project_id, project_id, &canon_of(&path), &file_uri(&path)).await;
        let id = format!("conv-{project_id}");
        insert_conv(pool, USER, &id, Some(project_id), "{}", activity).await;
        archive_conv(pool, &id, archived_at).await;
    }
    let dir = fx._tmp.path().join("unbound").display().to_string();
    insert_conv_ws(pool, USER, "dir-conv", &dir, 50).await;
    archive_conv(pool, "dir-conv", 200).await;
    insert_conv(pool, USER, "chat", None, "{}", 1).await;
    archive_conv(pool, "chat", 400).await;

    let response = fx
        .service
        .first_screen(USER, Some(10), &[], ArchiveScope::Archived)
        .await
        .unwrap();
    let groups: Vec<_> = response
        .groups
        .iter()
        .map(|g| match &g.scope {
            SidebarScope::Project { project_id, .. } => project_id.as_str(),
            SidebarScope::Dir { .. } => "directory",
            SidebarScope::Chats => "chats",
            SidebarScope::Pinned => panic!("archive must not contain pinned items"),
        })
        .collect();
    assert_eq!(groups, vec!["chats", "later", "directory", "earlier"]);

    let page = fx
        .service
        .items(USER, "project:later", None, Some(1), ArchiveScope::Archived)
        .await
        .unwrap();
    assert_eq!(ordered_ids(&page.items), vec!["conv-later"]);
    assert!(!page.has_more);
}
