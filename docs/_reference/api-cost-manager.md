---
title: API Cost Manager
description: Local request budgets, quota protection, diagnostics and safe job recovery.
nav_order: 13
---

## Scope and request inventory

`ApiGateway` routes grants and `ApiClient::send_body` is the only Web API
transport. Profile, playback state/control/queue, playlists and playlist items,
images uploaded to playlists, library membership, saved tracks/albums/shows,
followed artists, search, artists, albums, tracks, podcasts and pagination all
pass through `api::cost::CostManager`. Token refresh stays in `auth.rs` and is
serialized by `WebTokens`; it targets the OAuth service rather than the Web
API. Playback-session, Pathfinder, artwork CDN, lyrics, updates, Cast and
Sonos requests target different services and retain their existing budgets.
No additional grants or quota-bypassing identities are introduced.

One manager is owned by `NetActivity`, shared across both Web API grants and
all reauthorizations. This deliberately limits more conservatively than an
app-specific limiter: Spotify's quota bucket membership is not published and
multiple Development Mode apps on one developer account share quota. Calls
from other processes, applications and machines are not visible here.

## Dispatch and configuration

The default local ceiling is **30 sends per rolling 30 seconds**, paced one
second apart, with at most **two in flight** and **256 queued**. These values
are Spotifast policy, not Spotify's published limits or a quota estimate.
Backpressure returns an actionable error when the queue is full. Background
requests can occupy at most three quarters of the queue, reserving space for
interactive work. Medium-priority writes can occupy at most seven eighths. Cancelled
futures remove their queue tickets and release their slots.

Interactive GETs have high priority, short writes medium priority and
generation/unseen background work low priority. FIFO holds within each
priority. Weighted turns allocate four high, two medium and one low slot per
cycle when all three queues have work. Empty priorities are skipped. Existing
per-source background and transport semaphores remain additional bounds.

For local tuning, set these environment variables before launching the app:

| Variable | Default | Accepted range |
| --- | --- | --- |
| `SPOTIFAST_API_REQUESTS_PER_30S` | 30 | 1 to 120 |
| `SPOTIFAST_API_CONCURRENCY` | 2 | 1 to 4 |

Invalid values use defaults. Larger values are not a claim of additional
Spotify quota. A restored reduced budget never exceeds the configured ceiling.

A 429 halves the local dispatch ceiling, down to one send per window. After
120 successful responses it recovers by one, never above the configured
ceiling. The rolling window still counts every attempt, including rejected
calls. There are no deliberate bursts to discover Spotify's threshold.

## Rate limits and quota circuits

Ordinary 429 responses retain the full `Retry-After` in seconds. Exponential
backoff and positive jitter may extend that wait; no 30-second truncation is
applied. Retries are bounded. A corrected 401 refreshes once; permanent 400/403
responses are not retried. GET network errors and 5xx responses get at most
two transport retries. Generation owns its own durable retry counter instead
of nesting transient retries inside each step. Requests have a 30-second
transport timeout.

`error.reason == QUOTA_EXCEEDED` opens the global Web API circuit. Because the
API does not identify its quota buckets, stopping all routes avoids guessing
which related endpoints are safe. Requests waiting in the queue fail with the
same diagnosis and generation jobs enter `PAUSED_QUOTA`. No daily reset time,
remaining quota percentage or automatic probe is assumed. The diagnostic
panel's **Allow quota recheck** clears only that circuit, then paused jobs can
be resumed. The same recovery control is shown on the sign-in screen after a restart.
It sends no probe itself. A further quota rejection opens the
circuit again. Existing rate-limit waits are preserved.

## Persistence and writes

`api-cost.json` in the state directory stores version, adaptive budget, recent
send reservations, cooldown deadline and quota circuit. It stores no tokens,
search terms, endpoint payloads or account identifiers. Startup reads this
before restoring grants or starting jobs. Every reservation is atomically
synced on a blocking worker before the transport can dispatch. Cancellation
lets the write finish and retains the reservation, so crashes may overcount
rather than undercount a request. Unknown/corrupt state or a disk write failure
stops dispatch and is shown in diagnostics; preserve the file and repair the
storage problem before restarting. Do not delete it to skip a cooldown.

Ordinary mutations never automatically replay network failures, timeouts or
5xx responses whose outcome could be uncertain. Explicit 401/429 rejections
are safe to retry within the bounded policy. The production HTTP client also
disables reqwest's implicit retries. Playlist generation keeps its existing
intent-before-write and confirmed-result checkpoints. Interrupted creation
finds the original job marker; interrupted insertion checks remote contents.
An absent, not-yet-visible result is quarantined for user review rather than
repeated. The job actor continues while Spotifast is in the tray; a fully
closed app or powered-off computer does no processing.

## Request savings and diagnostics

Concurrent identical catalog reads share one per-key lock and reuse successful
JSON for 15 seconds, within a per-client cache of at most 64 entries of at most
256 KiB each. This in-memory cache is discarded on authorization replacement
and cannot be read after sign-out. Player, library and mutable playlist reads
remain fresh. An aborted first caller releases the lock so another caller can
complete; failed requests are never cached. Existing complete artist/year
caches and incremental generation checkpoints remain in use. Discovery still
filters artists/years at the source where supported; this change adds no
unavailable batch endpoint. Existing ID deduplication stays in the job layer.

Expand **API cost manager** in playlist creation for recent sends, effective
rate, current local ceiling, queue by priority, active work, global cooldown,
quota status, catalog cache hits/reads and per-endpoint 429 rate/quota,
401/403/5xx, network failure and mean response-header latency counters. Counters
are process-local and endpoint IDs and query parameters are removed. No remote
telemetry is sent. Official remaining quota is shown as unavailable. The
creation form labels preflight cost as an estimate because catalog sizes and
pagination totals may not be known before the first request.

## Validation

The cost module's virtual-clock tests cover pacing, the rolling window,
priorities, bounded queues, cancellation, long server waits and adaptive
recovery. Disk tests reopen state and verify reservations, cooldowns and quota
circuits; corrupt storage blocks requests. Client mock HTTP tests cover global
quota blocking, full Retry-After, catalog single-flight/expiry, sign-out and
uncertain mutations. Generation regression tests preserve confirmed batches
and quota pauses across JSON round trips and bound repeated throttling.

Run `cargo test --locked --no-default-features --lib api::` and
`cargo test --locked --no-default-features --lib playlist_generation::`, then
the full checks in `CONTRIBUTING.md`. Mocks never contact Spotify. For a manual
check, generate a playlist, close and reopen the app, inspect its counters and
verify that confirmed batches do not repeat. Multi-platform build checks and
native visual review remain necessary before release.

Official references checked on October 29, 2026:
[Rate Limits](https://developer.spotify.com/documentation/web-api/concepts/rate-limits),
[Quota Modes](https://developer.spotify.com/documentation/web-api/concepts/quota-modes),
[quota updates](https://developer.spotify.com/blog/2026-07-23-web-api-quota-updates),
[Developer Terms](https://developer.spotify.com/terms) and
[Developer Policy](https://developer.spotify.com/policy).
