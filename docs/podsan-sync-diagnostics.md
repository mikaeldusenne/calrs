# PodSaN sync diagnostics

A dashboard HTTP 504 after about ten seconds does not identify which component
timed out. Observe one sync, including what happens after the browser fails.

After merging into `podsan`, mirror to GitLab, rebuild the fork image, then
redeploy `calrs-podsan`. Existing `calrs=info` logging includes the diagnostics.
Verify the running image rather than the current target of the moving tag:

```bash
calrs_image_id=$(podman container inspect --format '{{.Image}}' podsan-p132-calrs-1)
podman image inspect "$calrs_image_id" \
  --format '{{index .Labels "org.opencontainers.image.revision"}}'
podman exec podsan-p132-calrs-1 printenv CALRS_SYNC_LOOKBACK_DAYS
```

Start this command, click **Sync** once, and note the browser error time in UTC.
Keep following the logs after a 504; stop with Ctrl-C after the attempt returns
or after recording the last unfinished stage:

```bash
podman logs --since 1m --timestamps -f podsan-p132-calrs-1 2>&1 |
  rg --line-buffered 'calrs::sync_diagnostics:'
```

`sync_id`, `source_id` and `trigger` correlate the attempt; `request_id` identifies
each HTTP request. Completion records contain `elapsed_ms`.

| Observation | Interpretation |
| --- | --- |
| `discover_principal`, `discover_calendar_home`, `list_calendars` | CalDAV discovery; each PROPFIND currently has the upstream client's 10-second timeout. |
| `response_headers` unfinished | Waiting for HTTP headers. |
| `response_body` unfinished | Headers arrived; the body is still downloading. A 207 does not mean the download finished. |
| `request_kind="time_range"` | Normal full fetch; `window_start` is its lower bound, with no future upper bound. |
| `request_kind="unfiltered"` | The filtered REPORT was rejected; inspect the preceding HTTP status. |
| `request_kind="initial_token"` | A separate REPORT may retrieve the entire calendar to obtain a token. |
| `fetch_events` returned, but `sync` still active | The fetch/parsing finished; local processing, reconciliation or a subsequent token request remains. |
| `stage="sync"` returns after the browser's 504 | Compare the attempt's timestamps with the reverse proxy's timeout and access logs. |
| `outcome="error"` / `"abandoned"` | An operation returned an error / its future was dropped or panicked. Abrupt process termination cannot emit a final log. |

`response_bytes` is the completed decoded UTF-8 text length, not wire traffic;
it is unavailable when body reading fails. New logs contain metadata only:
no URLs, credentials, calendar names, ICS bodies or arbitrary error messages.
Other existing logs can contain personal data; share only the filtered lines.

## Maintenance boundary and limits

The adapter in `src/sync_diagnostics.rs` wraps the existing request builder and
response without constructing requests, parsing calendars or updating the DB.
Its tests live separately. The upstream hooks are one module declaration,
four `send_observed` calls, and wrappers around sync/discovery/fetch futures.
When rebasing, preserve upstream behavior and reattach these hooks as needed;
do not copy the upstream sync implementation into a downstream module.

This diagnostic change preserves timeout values, the unfiltered fallback,
initial-token fetching, SQL writes and existing error handling. An `ok` record
means that operation returned `Ok`, not that every calendar was synchronized:
upstream currently swallows some fetch/write failures. Inspect inner stages and
HTTP statuses. Credential setup, lock waits and individual DB writes are not timed.

Separate follow-up fixes should address false success/freshness after failed
calendar fetches, ctag saved before a first successful fetch, and unfiltered
REPORT errors being parsed as empty calendars. They are deliberately excluded
from this instrumentation PR and can be proposed upstream independently.
