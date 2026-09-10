ALTER TABLE caldav_sources ADD COLUMN sync_event_error TEXT;

-- Re-evaluate custom definitions previously accepted through X-LIC-LOCATION.
-- Keep the last complete cache, but neither ctags nor an old worker may certify it.
UPDATE caldav_sources SET sync_verified_at = NULL, sync_revision = sync_revision + 1;

-- A source transferred or reconfigured must not expose a previous account's event.
CREATE TRIGGER clear_private_sync_event_error
AFTER UPDATE OF url, username, password_enc, auth_type, provider_type, account_id
ON caldav_sources
BEGIN
    UPDATE caldav_sources SET sync_event_error = NULL WHERE id = NEW.id;
END;

CREATE TRIGGER clear_private_sync_event_error_on_owner_change
AFTER UPDATE OF user_id ON accounts WHEN OLD.user_id IS NOT NEW.user_id
BEGIN
    UPDATE caldav_sources SET sync_event_error = NULL, sync_verified_at = NULL,
        sync_revision = sync_revision + 1 WHERE account_id = NEW.id;
END;
