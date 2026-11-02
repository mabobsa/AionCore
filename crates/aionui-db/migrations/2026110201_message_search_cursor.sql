-- Stable newest-first traversal, including messages sharing a timestamp.
CREATE INDEX IF NOT EXISTS idx_messages_search_recent ON messages(created_at DESC, id DESC);
