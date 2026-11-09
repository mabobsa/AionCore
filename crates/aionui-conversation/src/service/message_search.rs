use aionui_api_types::SearchMessagesMatchQuery;
use aionui_db::{MessagePageCursor, MessageSearchResult};

use super::ConversationService;
use crate::error::ConversationError;

/// Each term adds one LIKE per inspected message; keep the SQL bounded.
const MAX_MATCH_ALL_TERMS: usize = 32;

impl ConversationService {
    /// Search one keyword, or every whitespace-separated term in the same
    /// message when `match_all` is requested.
    pub(super) async fn search_message_rows(
        &self,
        user_id: &str,
        keyword: &str,
        match_query: SearchMessagesMatchQuery,
        cursor: Option<&MessagePageCursor>,
        page_size: u32,
    ) -> Result<MessageSearchResult, ConversationError> {
        let terms = if match_query.match_all {
            split_search_terms(keyword)
        } else {
            Vec::new()
        };
        if terms.len() <= 1 {
            // Repeated terms collapse to one, which is a plain keyword search.
            let keyword = terms.first().map_or(keyword, String::as_str);
            return Ok(self
                .conversation_repo
                .search_messages(user_id, keyword, cursor, page_size)
                .await?);
        }
        if terms.len() > MAX_MATCH_ALL_TERMS {
            return Err(ConversationError::BadRequest {
                reason: format!("keyword must not contain more than {MAX_MATCH_ALL_TERMS} terms"),
            });
        }
        Ok(self
            .conversation_repo
            .search_messages_all_terms(user_id, &terms, cursor, page_size)
            .await?)
    }
}

/// Split on whitespace and drop repeated terms, ignoring ASCII case like
/// SQLite LIKE does.
fn split_search_terms(keyword: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for term in keyword.split_whitespace() {
        if !terms.iter().any(|existing| existing.eq_ignore_ascii_case(term)) {
            terms.push(term.to_owned());
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_search_terms_drops_blank_and_repeated_terms() {
        assert_eq!(
            split_search_terms("  AAAA\tccc  aaaa CCC\nBBBB "),
            vec!["AAAA".to_owned(), "ccc".to_owned(), "BBBB".to_owned()]
        );
        assert_eq!(
            split_search_terms("한글  검색 한글"),
            vec!["한글".to_owned(), "검색".to_owned()]
        );
        assert!(split_search_terms("   ").is_empty());
    }
}
