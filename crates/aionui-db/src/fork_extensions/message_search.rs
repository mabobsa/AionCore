use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::{DbError, MessagePageCursor, MessageSearchResult, MessageSearchRow};

// Bound the work even when a keyword is rare or absent. Clients publish each
// batch immediately and continue from its cursor while the search stays open.
pub(crate) const SCAN_LIMIT: usize = 256;

#[derive(sqlx::FromRow)]
struct Candidate {
    id: String,
    created_at: i64,
    matches: bool,
}

pub(crate) async fn search(
    pool: &SqlitePool,
    user_id: &str,
    keyword: &str,
    cursor: Option<&MessagePageCursor>,
    page_size: u32,
) -> Result<MessageSearchResult, DbError> {
    let page_size = if page_size == 0 { 20 } else { page_size.min(100) } as usize;
    let pattern = format!(
        "%{}%",
        keyword.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
    );
    // Keep messages as the outer loop. Letting SQLite start with conversations
    // forces it to inspect and sort the entire history before applying LIMIT.
    let mut scan = QueryBuilder::<Sqlite>::new("SELECT m.id, m.created_at, m.content LIKE ");
    scan.push_bind(&pattern);
    scan.push(" ESCAPE '\\' AS matches FROM messages m INDEXED BY idx_messages_search_recent ");
    scan.push("CROSS JOIN conversations c ON c.id = m.conversation_id WHERE c.user_id = ");
    scan.push_bind(user_id);
    if let Some(cursor) = cursor {
        scan.push(" AND (m.created_at, m.id) < (");
        scan.push_bind(cursor.created_at)
            .push(", ")
            .push_bind(&cursor.id)
            .push(")");
    }
    scan.push(" ORDER BY m.created_at DESC, m.id DESC LIMIT ")
        .push_bind(SCAN_LIMIT as i64);
    let candidates = scan.build_query_as::<Candidate>().fetch_all(pool).await?;

    let mut matched_ids = Vec::new();
    let mut inspected = 0;
    for candidate in &candidates {
        inspected += 1;
        if candidate.matches {
            matched_ids.push(candidate.id.as_str());
        }
        if matched_ids.len() == page_size {
            break;
        }
    }
    let has_more = inspected < candidates.len() || candidates.len() == SCAN_LIMIT;
    let next_cursor = candidates
        .get(inspected.saturating_sub(1))
        .filter(|_| has_more)
        .map(|row| MessagePageCursor {
            created_at: row.created_at,
            id: row.id.clone(),
        });

    // Fetch full content and conversation fields only for the small hit set.
    // Recheck ownership and the match in case a streaming row changed since
    // the scan. A supplied cursor never changes the authorization scope.
    let items = if matched_ids.is_empty() {
        Vec::new()
    } else {
        let mut hits = QueryBuilder::<Sqlite>::new(
            "SELECT m.id AS message_id, m.type, m.content, m.created_at, \
             c.id AS conversation_id, c.name AS conversation_name, c.type AS conversation_type, \
             c.extra AS conversation_extra, c.model AS conversation_model, \
             c.status AS conversation_status, c.source AS conversation_source, \
             c.channel_chat_id AS conversation_channel_chat_id, c.pinned AS conversation_pinned, \
             c.pinned_at AS conversation_pinned_at, c.created_at AS conversation_created_at, \
             c.updated_at AS conversation_updated_at \
             FROM messages m CROSS JOIN conversations c ON c.id = m.conversation_id WHERE c.user_id = ",
        );
        hits.push_bind(user_id).push(" AND m.id IN (");
        let mut ids = hits.separated(", ");
        for id in matched_ids {
            ids.push_bind(id);
        }
        ids.push_unseparated(")");
        hits.push(" AND m.content LIKE ")
            .push_bind(&pattern)
            .push(" ESCAPE '\\'");
        hits.push(" ORDER BY m.created_at DESC, m.id DESC");
        hits.build_query_as::<MessageSearchRow>().fetch_all(pool).await?
    };
    tracing::debug!(
        inspected,
        matches = items.len(),
        has_more,
        "message search batch completed"
    );
    Ok(MessageSearchResult { items, next_cursor })
}
