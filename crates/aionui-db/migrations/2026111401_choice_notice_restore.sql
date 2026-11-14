-- A user may turn a dismissed choice notification back on. Clients show a
-- restored row again but must not send another mobile push for it.
ALTER TABLE choice_notifications ADD COLUMN restored_at INTEGER;
