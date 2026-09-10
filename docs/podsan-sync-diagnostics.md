# PodSaN calendar synchronization

## User workflow

After deploying this change, **Sync** immediately redirects to an authenticated
status page. It refreshes every two seconds while queued/running. Closing the
browser does not cancel the job. Repeated clicks join the same active attempt.

The page shows the last stage, elapsed time, last complete verification, a
diagnostic reference and a safe error code/HTTP status. It links to write-calendar
setup after a successful first sync. Sharing these fields is sufficient; do not
copy calendar content or raw DavMail logs.

When validation fails on an event, its owner also sees a private event summary:
title, start/end, timezone identifiers, UID and recurrence instance, when present.
Descriptions, attendees and email bodies are excluded. This HTML is escaped,
owner-only and served with `Cache-Control: no-store`; it is not an operator log.
The summary is cleared on retry or source reconfiguration.

## Fetch and publication

CalDAV synchronization follows [RFC 4791 section 8.2.1](https://www.rfc-editor.org/rfc/rfc4791.html#section-8.2.1):

1. Discover calendars and compare their ctags.
2. Query only href/ETag for events overlapping the useful window.
3. Download changed resources with calendar-multiget, in batches of 50.
4. Recheck collection ctags, validate calendar data and publish the entire source
   in one SQLite transaction.

The default lower bound is midnight UTC seven days ago
(`CALRS_SYNC_LOOKBACK_DAYS`, valid range 0–36500). There is no arbitrary future
cutoff: event types may have no booking horizon. This filters occurrences, NOT
the DTSTART of a recurrence master. Complete masters and exceptions are retained,
including old series. Recurrences are expanded only for the displayed/requested
period with the RFC recurrence library; the former 2000-iteration truncation is
removed. EXDATE, moved instances and UNTIL are interpreted with their timezones.

There is no initial sync-token probe, speculative sync-collection, or unfiltered
fallback in source synchronization. A server that rejects the bounded inventory,
returns incomplete XML/properties, changes an object during download, or provides
calendar data we cannot safely interpret causes an explicit failure. A valid empty
inventory does clear the cache. Force Sync ignores cached ctags/ETags, but keeps
the same date filter and safety checks.

Failed or cancelled work does not publish a partial snapshot, save premature
ctags, or advance freshness. The previous complete cache stays intact. Credentials
or URL/account changes invalidate verification and prevent an old worker from
publishing. CalDAV orphan-booking reconciliation runs only after publication,
outside the job result, still with remote confirm-before-cancel.

Native EWS uses the same publication boundary, records its existing two-year
coverage and rejects truncated CalendarView responses; it does not use the
CalDAV ETag path. Unverified EWS deletions no longer auto-cancel bookings.

## Limits and booking safety

No additional service is required: SQLite holds status; Tokio runs at most two
workers per process. One atomic DB claim per source deduplicates manual, automatic,
guest-triggered and CLI synchronization. Automatic failures back off for one minute;
manual retry is immediate.

The total deadline is 180 seconds, including queue time. Existing upstream
timeouts remain 10 seconds for discovery and 60 seconds for REPORTs. CalDAV
responses are capped at 16 MiB and staged calendar content at 64 MiB. Interrupted
jobs become explicit failures after their 190-second recovery lease expires.

Booking checks withhold availability if a contributing source has never been
verified, last failed (including while its retry runs), is over five minutes old,
or does not cover the requested window. A fresh complete snapshot remains usable
during a normal refresh. A source with no discovered calendars is not treated as
an empty calendar. Users with no calendar source keep their existing behavior.
The reminder loop proactively queues stale sources; guest pages also enqueue
without waiting for Exchange. SQL read failures block availability.

DavMail/Exchange TZIDs such as `Romance Standard Time` resolve through IANA,
Windows/CLDR or recognized Microsoft/libical URI aliases. Unknown names work too
when their VTIMEZONE provides valid STANDARD/DAYLIGHT observances: offsets,
DTSTART, RRULE and RDATE are evaluated directly, without guessing a similar zone.
X-LIC-LOCATION is a fallback when no observances are supplied. Only timezone rules
are serialized alongside cached dates; original calendar resources are preserved.
The same resolver converts EXDATE/RECURRENCE-ID exclusions and booking intervals.
Custom recurrences retain ancient masters, apply UNTIL in UTC, choose the first
occurrence of an ambiguous local time and skip nonexistent generated times without
counting them toward COUNT. Historical occurrences are not stored individually.

Unknown zones with missing/invalid definitions still fail as `unsupported_timezone`.
Unsupported event-level sub-daily rules and RDATE/RANGE=THISANDFUTURE also fail
explicitly; they must not silently become free time. Expansion safety limits block
the requested period and emit a fixed error code. Observance expansion is capped
at 8192 transitions; custom event expansion at one million candidates and 10000
in-window occurrences.
The new status UI is provided in French and English; other shipped locales
currently contain explicitly marked English copy.

This is not a distributed reservation lock: Outlook can change immediately
after verification. Nor does it guarantee that every network or proxy is always
available. The change removes the long Exchange operation from the source-sync
HTTP request and makes failures observable and conservative. Shared-resource
(room) synchronization remains a separate workflow.

## Deployment and diagnosis

Merge through human review, rebuild the Calrs fork, mirror/deploy it through the
normal PodSaN workflow, and verify the running image revision. No proxy timeout
increase or new infrastructure is needed. Deploy the DavMail log hardening too.

Back up SQLite before upgrading (migrations 064–066). Existing sources deliberately
start unverified because older versions could record false successes: their
booking availability is withheld until the first successful background sync.
Migrations 065/066 retain cached events but invalidate previous timezone
verification, so an unchanged ctag cannot preserve a snapshot accepted by an old
parser. Migration 066 also adds the private event-error field.
Test one busy account before general rollout, including an old recurring meeting,
a moved/deleted occurrence and an Outlook conflict. To roll back, stop the new
workers and restore the pre-upgrade DB backup together with the previous image.

If a 504 still occurs on the short Sync POST or status GET, record its timestamp,
request path and running image revision, then inspect the actual reverse proxy.
Do not attribute it automatically to Exchange.

For `unsupported_timezone`, an updated image handles both recognized aliases and
valid custom observances. If the error persists, the owner can locate the exact
event using the private report and correct its timezone in the calendar source.
Keep availability blocked and use the attempt ID and error code for operator
diagnosis; do not paste the private report, ICS or email contents into a public ticket.
Do not close the incident on a documentation merge: first verify the deployed image,
a successful complete sync, and known busy times including a moved/cancelled
occurrence and a daylight-saving transition.

For operator-only metadata, run `./scripts/diagnose-sync.sh` in `calrs-podsan`.
It remains compatible with older instrumentation and prints only selected stages,
duration and allowlisted error codes. The full attempt ID shown in the page is the
same `sync_id` in Calrs diagnostic spans; `request_id` identifies each upstream
request. `response_headers` versus `response_body` pinpoints where an HTTP wait
occurred. A final `background sync finished outcome="ok"` means a complete
snapshot was verified, unlike an old intermediate “step finished” record.

No diagnostic log contains credentials, URLs, email/calendar contents or
arbitrary upstream error text. The private owner-facing summary is stored separately
from log messages; its error Display/Debug representations are redacted too.
The SMTP test-message/body dumps have been removed.
ICS content still exists in the private application cache because recurrence
masters and exceptions are needed. DavMail must run at WARN or above: its DEBUG
wire dumps are independent of Calrs and are not disabled by dumpICS=0 alone.
Old log files are not automatically erased.
