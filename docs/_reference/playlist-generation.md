---
title: Playlist Generation
description: MCANS and unliked followed-artist releases with resumable desktop jobs.
nav_order: 12
---

## Unliked releases from followed artists (MNCAS)

Select **Músicas Não Curtidas de Artistas Seguidos (MNCAS)** in the existing
playlist creation form. The default name is `MNCAS - {period}` and remains
editable. Leaving both years as **Any** includes all available releases.

This mode pages through **all** followed artists (not the Home shelf's
50-artist fallback). New jobs with a closed period spanning at most five
calendar years search tracks by quoted artist name and each requested year.
The artist's Spotify **ID**, not its name, must occur in the track credits.
Each returned album/single is deduplicated and filtered against the requested
month/day interval before its tracks are inspected. Searching tracks also
finds credited collaborations and appearances on compilations. Saved-album
membership is irrelevant to this mode.

Search is an indexed discovery source, not a transactional enumeration of
Spotify's catalog. Indexing or edition metadata may differ from the complete
artist catalog. At 1,000 reported search hits, a changing total, malformed
pagination or missing required release metadata, the current artist falls back
to its complete albums, singles, appearances and compilations. An unsupported
search endpoint disables further searches for that job. Rate limits and
transport failures retain the search checkpoint and use the normal retries.
Open or broader periods, missing artist names and legacy checkpoints retain
the complete-catalog traversal. No early stop relies on an undocumented date
ordering. A track must credit at least one followed artist to be selected.

Complete artist/year query results and complete artist catalogs are cached
under `playlist-generations/release-catalog-cache/`. Cache keys include the
Spotify account, artist ID and discovery year (or catalog mode). They store
only release IDs, dates and track counts, never credentials or liked-song
membership. The requested month/day filter is reapplied when reusing them.
The current/future year and full catalogs expire after 15 minutes; previous
years expire after 24 hours. Only fully paginated results are cached, including
empty results. Mid-query progress remains in the durable job checkpoint.
Corrupt, oversized or expired cache files are ignored. Startup removes expired
files and retains at most 4,096 existing cache entries; individual entries are
limited to 8 MiB. Cache-write failures do not fail a job. Checkpoint-write
failures still stop processing. Removing this disposable cache does not remove
jobs or playlists.

The history separates **unique releases checked**, **releases within the
period**, and **songs analyzed/selected/added**. A compilation shared by several
followed artists is counted once. Already-running jobs resume with their saved
catalog cursor; start a new generation to use indexed discovery.

Spotify supplies a release date on the album/single, not an independently
verified original-release date for every recording. Reissues therefore use
the date of that edition. A date with only year or month precision denotes
the whole corresponding calendar interval; it is included when that interval
overlaps the requested range. Missing or invalid dates are skipped. Sorting
uses the first day of the known release period, newest first, without
claiming that this is an exact release day.

Before selecting tracks, the job checks Spotify library membership in groups
of at most 40 URIs and excludes liked tracks. Membership reflects the time
of the check, rather than an atomic snapshot of all likes for the run. URI
and available recording metadata prevent duplicates; different editions
with different IDs and no shared recording metadata cannot always be
recognized as one recording. Local/unavailable tracks and missing IDs are
skipped. Keep follows and likes stable during a run for consistent results.

The followed-artist cursor, artist index, release-page offset, unique album
queue, track offset and selected songs are checkpointed after each page.
HTTP 429, network retries, pause/resume, playlist reconciliation and library
synchronization use the same worker as MCANS. The history identifies each
discovery stage and shows artists processed, releases scanned and tracks
analyzed. Discovery has indeterminate progress until the actual workload is
known. Large catalogues can take substantial time and API requests, even
when a narrow date range excludes most tracks.

## Creating an MCANS playlist

Open **Create a playlist** using the plus in Your Library, select **Músicas
Curtidas de Álbuns Não Salvos (MCANS)**, enter an optional period, edit the
suggested name and choose **Start generation**. The manual playlist form is
still available in the same selector. Generation history appears below the
form, including after restarting Spotifast. Pause, resume and cancel operate
on individual jobs. Cancelling keeps any playlist and songs already created
in Spotify; it does not delete remote content. Cancelled jobs remain in history.

MCANS reads all saved albums and liked songs through the existing verified
Web API grant. It excludes a liked song only when that song's album ID is
in the saved-album library. Liking another song never saves its album for
this comparison. Different editions have different Spotify album IDs and
are treated separately. Unavailable songs, local files, missing track/album
identifiers and invalid track URIs are skipped. Selection uses `added_at`,
not release dates, and orders results newest first. URI and recording-key
deduplication use the existing track model, including linked releases and ISRC.
No matching songs completes the job without creating an empty playlist.

Generated playlists are added to Spotifast's library after confirming their
Spotify header and saved-library membership, both after creation and after
the final batch. Completed jobs are checked again after startup, repairing
older jobs missing from the local list without creating another playlist.
Reads of the library that lag behind the confirmed header cannot hide the
new entry. Playlists subsequently removed from Spotify's library are not
restored merely because their generation remains in history.

## Periods and names

MCANS uses **UTC** for like timestamps. MNCAS uses release calendar dates.
Use the **Year**, **Month**, **Day** selectors, in that order, for each
endpoint. **Any** year removes that endpoint; selecting a year enables the
optional month, and selecting a month enables the optional day. Clearing a
component clears the more precise components. When a year or month changes,
the day is clamped to the last valid day, including leap years. The two
endpoints occupy separate rows with equal-width columns.

Saved jobs retain their existing `YYYY`, `MM/YYYY` or `DD/MM/YYYY` format.
One start field denotes the whole year, month or day.
Two fields include every instant from the start period's first day through
the end period's final day. Precisions can differ: `2024` through `06/2025`
includes January 1, 2024 through June 30, 2025, inclusive. An end alone
has no lower bound. Both years set to **Any** means no filter. Invalid dates,
non-leap February 29 and reversed intervals are refused before starting.

The suggested name is `MCANS - {period}` or `MNCAS - {period}`, with slashes replaced by hyphens,
` a ` between endpoints and `Todas` for an unfiltered job. A custom name
is retained when the period changes. Names must contain 1 to 100 characters.

## Execution and recovery

Spotifast has no hosted backend. Jobs run on its existing Tokio runtime,
outside the interface thread, and continue while it runs in the tray.
Fully exiting Spotifast or shutting down the computer stops execution.
The next launch automatically resumes pending, running and retrying jobs
once their original Spotify account has been verified. Paused and failed
jobs require Resume. Other accounts cannot run or control these jobs.

Durable versioned JSON checkpoints live in the state directory under
`playlist-generations/{job-id}.json`. They contain account ID, parameters,
timestamps, counters, page offsets, saved album IDs, selected track URIs,
remote playlist ID and mutation intents. They contain no access or refresh
tokens. Writes use `fastframe_file`'s synced atomic replacement from a
blocking worker. A disk failure stops generation before another request;
an unreadable or unsupported job file is preserved and reported instead
of being silently overwritten. History is retained until its files are
removed while Spotifast is closed.
Signing out stops execution but retains these jobs and their history for
the original account; they are separate from disposable library caches.

Every library page checkpoints its results and next offset. Playlist
creation and each insertion batch checkpoint an intent **before** sending.
The description contains a unique job marker. An interrupted creation
searches all pages of the account's playlists for that marker, verifying
ownership. An interrupted insertion reads all remote playlist pages and
marks an intended batch complete only after all its URIs are present.
Completed batches are retained and are never sent again.

Spotify's creation and append APIs have no documented idempotency key.
If an uncertain creation cannot be found, or an uncertain batch is only
partly visible, Spotifast marks the job Failed with an explanation and
does not repeat the mutation. Check Spotify and Resume when the original
result becomes visible. If it never appears, cancel and explicitly start
a new job after inspecting the existing playlists. This conservative case
needs intervention because absence alone does not prove the first request
failed. Do not remove the job marker or edit generated content while a job
is running or waiting for reconciliation.

Spotify's offset pagination is not a transactional library snapshot. A
changed library total or incomplete page stops the job with an explanation;
cancel and start a fresh job after finishing library edits. Changes that
preserve the total cannot be reliably detected. Albums reflect membership
when scanned, rather than an atomic snapshot of the entire run.

## Progress and request policy

The history shows state, current stage, analyzed/total songs, album
count, followed artists processed where applicable, selected and added songs,
execution time and an estimate when there
are completed insertions to extrapolate from. Initial discovery has an
indeterminate indicator. Once scanning finishes, overall progress is
completed artists + album rows + analyzed songs + inserted songs, divided by those
same scanned rows + all selected insertions. No fixed stage percentages are
invented. Elapsed time accumulates time in the running process across
restarts, including queued work and retry waits, excluding time with the
process closed, another account signed in, or the task paused.
Estimates are approximate, particularly after throttling.

Jobs rotate after each step through one generation worker, so several jobs
can coexist without flooding Spotify. The existing shared transport's
request semaphore, queue and per-grant cooldown also cover generation.
All Web API traffic honors the **full** `Retry-After` duration. Generation
checkpoints a RETRYING state and retry deadline. Repeated throttling increases generation
pacing up to 30 seconds between steps; successful steps gradually recover.
The pace is adaptive, not an assumed Spotify requests-per-second quota.

Network errors and server failures use exponential backoff plus jitter,
up to eight consecutive transient failures (`MAX_ATTEMPTS` in the job
module). Resume resets the counter. HTTP 429 uses the same bounded retry
count, full server wait and adaptive pacing. A quota refusal pauses the job
until an explicit quota recheck and Resume. Refused authorization needs intervention. Existing OAuth refresh handles aging tokens and one 401 retry.
Permissions errors remain visible and may require signing in again.

## Extending selection

`src/playlist_generation/mod.rs` contains the small strategy registry.
`Source` selects a durable discovery pipeline: liked tracks or followed
releases. Selection predicates receive a `Track`, shared parameters and
relevant library state independently of the date filter.
For another liked-song selection, implement its predicate and add a `Kind`:

```rust
Kind {
    id: "liked-from-saved-albums",
    label: "Liked songs from saved albums",
    prefix: "Saved albums",
    source: Source::LikedTracks,
    needs_albums: true,
    fields: &[],
    validate: |_| Ok(()),
    select: |track, albums, _parameters| track.album.as_ref()
        .is_some_and(|album| albums.contains(&album.id)),
}
```

The form discovers this entry without another conditional. Calendar
validation, editable naming, account isolation, paging, deduplication,
retries, persistence, batching and history are shared. The shared `Period` is accompanied by a persisted string parameter map.
Declare additional text fields through `fields` and their validation through
`validate`; the form renders them automatically and passes their values to
`select`. Add their labels to the translation catalogs. A strategy needing
another data source or non-text controls should extend the typed
checkpoint model and native form deliberately;
the registry is not a general workflow framework.

## Implementation files

| Files | Responsibility |
| --- | --- |
| `src/playlist_generation/mod.rs` | Strategy registry, parameter declarations, eligibility and naming |
| `src/playlist_generation/period.rs` | Calendar validation, normalized boundaries and UTC filtering |
| `src/playlist_generation/jobs.rs` | Durable jobs, state transitions, scheduling, retries, checkpoints, reconciliation and timing |
| `src/playlist_generation/releases.rs`, `src/playlist_generation/release_tests.rs` | Followed release discovery, membership checks and recovery regression tests |
| `src/playlist_generation/tests.rs` | 26 selection, date, pagination, HTTP, interruption and recovery tests |
| `src/ui/playlist_generation.rs` | Native generation form, progress, task controls and history |
| `src/backend.rs` | Starts the job actor on the existing runtime and exposes its handle |
| `src/app.rs`, `src/model.rs`, `src/lib.rs`, `src/ui/mod.rs`, `src/ui/dialogs.rs` | Action dispatch, draft state and integration with the existing creation modal |
| `src/api/client.rs`, `src/api/gateway.rs` | Durable generation request policy and isolated test transports |
| `src/demo.rs`, `src/entrypoint.rs` | `--demo-show generation` for native visual verification |
| `assets/i18n/`, `README.md`, `docs/_reference/settings-and-files.md` | Updated catalogs, discovery and persistence documentation |

## Spotify references

Checked against the current official documentation:

- [Saved tracks](https://developer.spotify.com/documentation/web-api/reference/get-users-saved-tracks)
  and [saved albums](https://developer.spotify.com/documentation/web-api/reference/get-users-saved-albums)
  use `user-library-read` and pages of at most 50.
- [Create playlist](https://developer.spotify.com/documentation/web-api/reference/create-playlist)
  uses `/me/playlists`; [add items](https://developer.spotify.com/documentation/web-api/reference/add-items-to-playlist)
  uses `/playlists/{id}/items` and at most 100 URIs per request.
- [Rate limits](https://developer.spotify.com/documentation/web-api/concepts/rate-limits)
  vary by application and quota mode; Spotify does not publish one universal
  numerical allowance. Respect its response and `Retry-After`.
- [Development Mode changes](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide)
  require Premium for the app owner and limit newly added accounts. These
  are Spotify account/app restrictions, not limits imposed by MCANS.

The existing OAuth scopes already include `user-library-read`,
`playlist-read-private`, `playlist-modify-private` and `playlist-modify-public`.
No new Spotify app, service, audio source or credential store is introduced.

The followed-release pipeline reuses the documented
[followed artist cursor](https://developer.spotify.com/documentation/web-api/reference/get-followed),
[artist release catalogue](https://developer.spotify.com/documentation/web-api/reference/get-an-artists-albums),
[album tracks](https://developer.spotify.com/documentation/web-api/reference/get-an-albums-tracks)
and [saved-item membership](https://developer.spotify.com/documentation/web-api/reference/check-library-contains)
endpoints. Existing `user-follow-read` and library/playlist scopes suffice.

## Shared API budget

Generation is low priority in the [API cost manager](api-cost-manager.md).
Interactive reads and short mutations have higher priority, with weighted
turns guaranteeing progress for background work. Quota exhaustion persists
`PAUSED_QUOTA`; confirmed results stay intact and no timer invents a reset.
Check Spotify's developer dashboard, choose **Allow quota recheck** in the
diagnostic panel, then **Resume** the affected jobs. This does not clear any
remaining `Retry-After` interval. Repeated rate-limit failures stop after eight
retries and require intervention rather than retrying forever. Existing job
files remain readable; existing state names retain their meaning.
