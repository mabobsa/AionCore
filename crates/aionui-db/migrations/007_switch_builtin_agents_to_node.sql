-- Migration 007: Switch builtin ACP agents from `bun x --bun <pkg>` to
-- `npm exec --prefix=<stable-dir>` driven by the bundled node runtime.
--
-- Root cause: `bun x` is not in-place — every spawn copies the cached
-- package into a fresh tmp dir, which collides on Windows with file-read
-- handles held by concurrent sends (Sentry ELECTRON-1FB). `npm exec` with
-- a fixed --prefix populates `node_modules/` once and reuses it on
-- subsequent launches: no copy on the hot path, no EBUSY surface.
--
-- Rows are matched by `agent_source_info.binary_name` rather than primary
-- key so the migration is robust to local re-installs that may have
-- shifted IDs. Idempotent via the `command = 'bun'` guard.
--
-- Versions reflect those pinned in migration 004; bump to current at merge
-- time if upstream has cut a newer release.

UPDATE agent_metadata
SET command = 'npm',
    args = '["exec","--prefix=${AGENT_PREFIX}","--cache=${AGENT_NPM_CACHE}","--yes","--","@agentclientprotocol/claude-agent-acp@0.33.1"]',
    agent_source_info = json_set(COALESCE(agent_source_info, '{}'), '$.bridge_binary', 'npm'),
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE agent_source = 'builtin'
  AND command = 'bun'
  AND json_extract(agent_source_info, '$.binary_name') = 'claude';

UPDATE agent_metadata
SET command = 'npm',
    args = '["exec","--prefix=${AGENT_PREFIX}","--cache=${AGENT_NPM_CACHE}","--yes","--","@zed-industries/codex-acp@0.14.0"]',
    agent_source_info = json_set(COALESCE(agent_source_info, '{}'), '$.bridge_binary', 'npm'),
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE agent_source = 'builtin'
  AND command = 'bun'
  AND json_extract(agent_source_info, '$.binary_name') = 'codex';

UPDATE agent_metadata
SET command = 'npm',
    args = '["exec","--prefix=${AGENT_PREFIX}","--cache=${AGENT_NPM_CACHE}","--yes","--","@tencent-ai/codebuddy-code@2.97.0","--acp"]',
    agent_source_info = json_set(COALESCE(agent_source_info, '{}'), '$.bridge_binary', 'npm'),
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE agent_source = 'builtin'
  AND command = 'bun'
  AND json_extract(agent_source_info, '$.binary_name') = 'codebuddy';
