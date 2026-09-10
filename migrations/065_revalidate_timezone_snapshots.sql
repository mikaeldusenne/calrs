-- Earlier snapshot parsers could accept a named timezone as floating time.
-- Keep cached data, but require one complete validation with the new resolver.
-- Increment the revision so an already-running old worker cannot publish it.
UPDATE caldav_sources
SET sync_verified_at = NULL, sync_revision = sync_revision + 1;
