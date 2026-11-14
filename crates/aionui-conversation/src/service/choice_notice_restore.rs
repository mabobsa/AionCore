use aionui_api_types::{ChoiceNoticeListResponse, RestoreChoiceNoticeRequest};
use aionui_common::now_ms;
use tracing::info;

use super::{ConversationService, MAX_CHOICE_NOTICE_ID_LENGTH};
use crate::error::ConversationError;

impl ConversationService {
    /// Turn a user-dismissed choice notice back on for every client. The
    /// restored row carries `restored_at` so clients do not push it to mobile
    /// again. Unknown, pending or superseded keys leave the snapshot unchanged.
    pub async fn restore_choice_notice(
        &self,
        user_id: &str,
        req: RestoreChoiceNoticeRequest,
    ) -> Result<ChoiceNoticeListResponse, ConversationError> {
        if req.conversation_id.is_empty()
            || req.conversation_id.len() > MAX_CHOICE_NOTICE_ID_LENGTH
            || req.message_id.is_empty()
            || req.message_id.len() > MAX_CHOICE_NOTICE_ID_LENGTH
        {
            return Err(ConversationError::BadRequest {
                reason: "choice notification contains an invalid conversation or message id".into(),
            });
        }
        self.conversation_repo
            .get(user_id, &req.conversation_id)
            .await?
            .ok_or_else(|| ConversationError::NotFound {
                id: req.conversation_id.clone(),
            })?;

        let key =
            serde_json::to_string(&(&req.conversation_id, &req.message_id, req.marker_index)).map_err(|error| {
                ConversationError::internal(format!("Failed to build choice notification key: {error}"))
            })?;
        let restored = self
            .conversation_repo
            .restore_choice_notice(user_id, &key, now_ms())
            .await?;
        let items = self.list_choice_notices(user_id).await?;
        if restored {
            info!(
                user_id,
                conversation_id = %req.conversation_id,
                message_id = %req.message_id,
                marker_index = req.marker_index,
                pending_count = items.len(),
                "Choice notification restored"
            );
            self.broadcast_choice_notices(user_id, &items)?;
        }
        Ok(items)
    }
}
