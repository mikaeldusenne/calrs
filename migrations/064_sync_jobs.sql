-- Persistent diagnostics; last_synced remains the last COMPLETE snapshot.
ALTER TABLE caldav_sources ADD COLUMN sync_status TEXT NOT NULL DEFAULT 'idle';
ALTER TABLE caldav_sources ADD COLUMN sync_id TEXT;
ALTER TABLE caldav_sources ADD COLUMN sync_stage TEXT;
ALTER TABLE caldav_sources ADD COLUMN sync_started_at TEXT;
ALTER TABLE caldav_sources ADD COLUMN sync_finished_at TEXT;
ALTER TABLE caldav_sources ADD COLUMN sync_error TEXT;
ALTER TABLE caldav_sources ADD COLUMN sync_http_status INTEGER;
-- Old versions could report success after a failed fetch: revalidate on upgrade.
ALTER TABLE caldav_sources ADD COLUMN sync_verified_at TEXT;
ALTER TABLE caldav_sources ADD COLUMN sync_window_start TEXT;
ALTER TABLE caldav_sources ADD COLUMN sync_window_end TEXT;
ALTER TABLE caldav_sources ADD COLUMN sync_revision INTEGER NOT NULL DEFAULT 0;
-- Changing credentials/location invalidates verification, including edits by CLI.
CREATE TRIGGER invalidate_source_sync AFTER UPDATE OF url, username, password_enc, auth_type, provider_type, account_id ON caldav_sources
WHEN OLD.url IS NOT NEW.url OR OLD.username IS NOT NEW.username OR OLD.password_enc IS NOT NEW.password_enc
  OR OLD.auth_type IS NOT NEW.auth_type OR OLD.provider_type IS NOT NEW.provider_type OR OLD.account_id IS NOT NEW.account_id
BEGIN
    UPDATE caldav_sources SET sync_verified_at = NULL, sync_revision = sync_revision + 1 WHERE id = NEW.id;
END;
-- Complete resources retain recurrence masters and all their exceptions.
CREATE TABLE caldav_objects (
    calendar_id TEXT NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    href TEXT NOT NULL,
    etag TEXT NOT NULL,
    ical TEXT NOT NULL,
    PRIMARY KEY (calendar_id, href)
);
