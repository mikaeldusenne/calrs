# PodSaN downstream branch

The `podsan` branch carries the small set of changes required by the PodSaN deployment while `main` stays aligned with upstream Cal.rs.

## Downstream changes

- trust the CA bundle referenced by `SSL_CERT_FILE` for OIDC;
- allow SMTP configuration without AUTH and use native CA roots;
- authenticate resource ICS feeds with the resource's encrypted CalDAV credentials;
- use the PodSaN calendar icon;
- default newly created OIDC users and accounts to `Europe/Paris`;
- default full-sync lookback to 7 days (`CALRS_SYNC_LOOKBACK_DAYS` overrides it);
- trace CalDAV synchronization phases without logging response bodies or credentials;
- build with the PodSaN certificate image.

## Image build

When this branch is mirrored to GitLab, `.gitlab-ci.yml` builds two image tags in the Podman store shared with the trusted `podman-deploy` runner:

- `localhost/${EDS_IMAGE_PREFIX}-calrs:podsan-<short commit SHA>`: traceable build;
- `localhost/${EDS_IMAGE_PREFIX}-calrs:podsan`: moving deployment tag.

No container registry is required. The deployment project consumes the moving local tag directly and should verify that it exists before starting.

This design assumes that both pipelines use runners connected to the same Podman socket. Build the fork image again after storage cleanup or before deploying from another host.

## Diagnosing a slow sync or dashboard HTTP 504

After merging into `podsan`, mirror the branch to GitLab, build the fork image,
then redeploy `calrs-podsan` so its container uses that image. No additional
Compose setting is required for diagnostics: the existing `calrs=info` logging
level includes them. A PR branch does not trigger the GitLab image build.

First identify the running image, rather than the current target of the moving tag:

```bash
calrs_image_id=$(podman container inspect --format '{{.Image}}' podsan-p132-calrs-1)
podman image inspect "$calrs_image_id" \
  --format '{{index .Labels "org.opencontainers.image.revision"}}'
podman exec podsan-p132-calrs-1 printenv CALRS_SYNC_LOOKBACK_DAYS
```

An unset lookback uses 7 days; `0` starts at the current time. The value is a
lower bound for the normal full-fetch request, not a guarantee that every
request is filtered. The HTTP-status fallback remains unfiltered, and initial
sync-token discovery can also retrieve the whole collection. Neither has been
removed or given a longer timeout by this diagnostic change.

Start this command, click **Sync** once on the affected source, and note the
UTC time of any browser error. Keep following the logs after a 504; stop with
Ctrl-C after a terminal result or after recording the last unfinished stage:

```bash
podman logs --since 1m --timestamps -f podsan-p132-calrs-1 2>&1 |
  rg --line-buffered 'calrs::sync_diagnostics:'
```

Each `calendar_sync` span includes a random `sync_id` and the internal
`source_id`. The caller supplies `trigger=dashboard`, `background`, `on_demand`
or `cli`; new-source onboarding uses `source_setup`. Client construction is
traced before the sync starts and has `source_id` but no `sync_id` yet.
Calendar phases add `calendar_id`. Requests have their own `request_id`.

| Last active phase / metadata | What it tells you |
| --- | --- |
| `build_client` | Credential decryption or OAuth refresh, before discovery. |
| `discover_principal`, `discover_calendar_home`, `list_calendars` | Discovery uses a 10-second timeout per HTTP request. The last unfinished subphase locates the delay. |
| `response_headers` | Still waiting for response headers; `timeout_ms` records the applicable deadline. |
| `response_body` | Headers arrived, but downloading the body is still in progress. A 207 alone is not a completed download. |
| `request_kind=time_range` | Normal full fetch. `window_start` and `lookback_days` show the actual filter; there is no future upper bound. |
| `request_kind=unfiltered` | A rejected filtered request triggered the fallback. The preceding warning contains its HTTP status. |
| `request_kind=initial_token` | The filtered fetch already succeeded; Cal.rs is trying to obtain a token with a separate, unfiltered REPORT. Failure is logged but remains optional. |
| `parse_events`, `parse_delta`, `save_events`, `reconcile_events`, `reconcile_delta`, `reconcile_bookings` | Time is spent in local processing or reconciliation (which can itself verify events and send cancellation notifications). |
| `stage=sync outcome=ok` after the browser's 504 | Cal.rs completed after the browser failed; investigate the reverse proxy's request timeout and access logs. |
| `outcome=error` | Use the innermost failed stage, `error_kind`, HTTP status and duration. A failed calendar fetch/cache write no longer advances source freshness. |
| `outcome=abandoned` | The future was dropped or panicked; this is not proof of which component caused cancellation. Abrupt process termination cannot emit this final log. |

Phase completion records include `elapsed_ms`; HTTP records separate headers
from body reception and include the status. `response_bytes` measures the
successfully decoded UTF-8 text length, not wire traffic; it is unavailable
when body reading fails. Counts describe parsed or stored events, not their
contents. A transport phase can finish successfully with an HTTP error status;
the enclosing operation determines whether that error is fatal or recoverable.

The dedicated `calrs::sync_diagnostics` log target classifies errors without formatting arbitrary error
messages, URLs, tokens, HTTP headers, calendar names or ICS bodies. Do not turn
on global HTTP/DavMail wire logging or export a browser HAR for this first pass.
Existing booking/notification logs elsewhere in the application may still
contain personal data: share only the relevant diagnostic lines.

A dashboard 504 does not establish that CalDAV timed out. The dashboard waits
for synchronization before responding, and a proxy can time out sooner than
the sync completes. Envoy's route timeout defaults to 15 seconds unless
overridden ([Envoy timeout documentation](https://www.envoyproxy.io/docs/envoy/latest/faq/configuration/timeouts)).
Compare the same attempt's UTC timestamps with the proxy's status, duration
and response flags. If needed, repeat once with a small working calendar.
Change the failing phase only after this comparison identifies it.

This change does not make reconciliation transactional or redesign existing
best-effort booking notifications. Successful calendars can already be cached
when another calendar fails; the source is then reported incomplete and its
freshness timestamps are preserved for a retry.

## Updating from upstream

1. Fast-forward `main` from `olivierlambert/calrs`.
2. Rebase `podsan` onto the updated `main`.
3. Review every downstream commit and run the GitLab image build.
4. Validate OIDC, SMTP, user calendars, shared resources, booking, cancellation and rescheduling before deployment.
