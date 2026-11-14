use sqlx::SqlitePool;

use crate::DbError;
use crate::models::ChoiceNoticeRow;

/// Turn a user-dismissed choice notice back on.
///
/// Registration keeps one pending row per identical non-empty question in a
/// conversation, newest first. Restoring follows the same rule: a newer
/// identical pending row already stands for this one, and older identical
/// pending rows are superseded by it.
pub(crate) async fn restore(pool: &SqlitePool, user_id: &str, key: &str, restored_at: i64) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query_as::<_, ChoiceNoticeRow>(
        "SELECT * FROM choice_notifications WHERE user_id = ? AND key = ? AND status = 'dismissed'",
    )
    .bind(user_id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(false);
    };

    if !row.preview.is_empty() {
        let newer_identical = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM choice_notifications \
             WHERE user_id = ? AND conversation_id = ? AND status = 'pending' AND preview = ? \
               AND message_id != ? AND key != ? \
               AND (created_at, message_id, marker_index) > (?, ?, ?)",
        )
        .bind(user_id)
        .bind(&row.conversation_id)
        .bind(&row.preview)
        .bind(&row.message_id)
        .bind(key)
        .bind(row.created_at)
        .bind(&row.message_id)
        .bind(row.marker_index)
        .fetch_one(&mut *tx)
        .await?;
        if newer_identical > 0 {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE choice_notifications SET status = 'superseded', updated_at = ? \
             WHERE user_id = ? AND conversation_id = ? AND status = 'pending' AND preview = ? \
               AND message_id != ? AND key != ?",
        )
        .bind(restored_at)
        .bind(user_id)
        .bind(&row.conversation_id)
        .bind(&row.preview)
        .bind(&row.message_id)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    }

    let result = sqlx::query(
        "UPDATE choice_notifications SET status = 'pending', restored_at = ?, updated_at = ? \
         WHERE user_id = ? AND key = ? AND status = 'dismissed'",
    )
    .bind(restored_at)
    .bind(restored_at)
    .bind(user_id)
    .bind(key)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}
