//! Saved-album discovery and unliked tracks, checkpointed with the parent job.
use super::jobs::{Candidate, Job, Stage, checked_next, now};
use super::{DateBasis, Source, playable, valid_id};
use crate::api::models::{Page, SavedAlbum, Track};
use crate::api::{ApiClient, ApiError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};

const TRACK_PAGE_SIZE: u32 = 40;
const MAX_PREFETCHED_ALBUMS: usize = 50;
const MAX_PREFETCH_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub saved_at: String,
    pub tracks: Option<u32>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetched: Option<Page<Track>>,
    #[serde(default)]
    pub prefetched_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scan {
    pub albums: VecDeque<Album>,
    pub track_offset: u32,
    pub track_total: Option<u32>,
}

pub struct Outcome {
    pub page: Page<Track>,
    pub liked: BTreeSet<String>,
}

fn invalid(message: &str) -> ApiError {
    ApiError::Decode(message.into())
}

fn checked_page(scan: &Scan, page: &Page<Track>) -> Result<Option<u32>, ApiError> {
    let next = checked_next(page, scan.track_offset)?;
    let album = scan
        .albums
        .front()
        .ok_or_else(|| invalid("Missing saved-album checkpoint"))?;
    if scan.track_total.is_some_and(|total| total != page.total)
        || album.tracks.is_some_and(|total| total != page.total)
    {
        return Err(invalid(
            "The saved album's tracks changed during generation. Cancel this job and start a new one.",
        ));
    }
    if page.limit > 0
        && page.items.len() as u32 != page.total.saturating_sub(page.offset).min(page.limit)
    {
        return Err(invalid(
            "Spotify returned an incomplete saved-album track page; resume to retry it",
        ));
    }
    Ok(next)
}

/// Reuse a full first processing batch without adding membership requests.
/// A shorter embedded page falls back to the normal 40-track read.
fn embedded_tracks(saved: &SavedAlbum) -> Option<Page<Track>> {
    let page = saved.album.tracks.as_ref()?;
    checked_next(page, 0).ok()?;
    let count = page.items.len() as u32;
    if count != page.total.min(page.limit)
        || page.next.is_some() != (count < page.total)
        || saved
            .album
            .total_tracks
            .is_some_and(|total| total != page.total)
        || count < page.total.min(TRACK_PAGE_SIZE)
    {
        return None;
    }
    let page = Page {
        items: page.items.iter().take(TRACK_PAGE_SIZE as usize).cloned().collect(),
        total: page.total,
        limit: TRACK_PAGE_SIZE,
        offset: 0,
        next: (page.total > TRACK_PAGE_SIZE).then(|| format!(
            "https://api.spotify.com/v1/albums/{}/tracks?limit={TRACK_PAGE_SIZE}&offset={TRACK_PAGE_SIZE}",
            saved.album.id
        )),
    };
    (serde_json::to_vec(&page).ok()?.len() <= MAX_PREFETCH_BYTES).then_some(page)
}

pub(super) fn collect(job: &mut Job, albums: &[SavedAlbum]) {
    let mut seen = job.albums.clone();
    let date_basis = DateBasis::for_source(Source::SavedAlbumTracks, &job.parameters);
    let mut prefetched_count = job
        .saved_album_tracks
        .albums
        .iter()
        .filter(|album| album.prefetched.is_some())
        .count();
    for saved in albums {
        if seen.insert(saved.album.id.clone())
            && date_basis.matches(
                &job.period,
                saved.added_at.as_deref(),
                saved.album.release_date.as_deref(),
            )
        {
            let prefetched = (prefetched_count < MAX_PREFETCHED_ALBUMS)
                .then(|| embedded_tracks(saved))
                .flatten();
            prefetched_count += usize::from(prefetched.is_some());
            job.saved_album_tracks.albums.push_back(Album {
                id: saved.album.id.clone(),
                saved_at: saved
                    .added_at
                    .as_deref()
                    .and_then(|value| value.parse::<jiff::Timestamp>().ok())
                    .map(|stamp| stamp.to_string())
                    .unwrap_or_default(),
                tracks: saved.album.total_tracks,
                release_date: saved.album.release_date.clone(),
                prefetched,
                prefetched_at: now(),
            });
        }
    }
}

pub(super) fn finish_discovery(job: &mut Job) {
    let more_albums = job
        .albums_total
        .is_some_and(|total| job.albums_checked < total);
    job.tracks_total = if more_albums {
        None
    } else {
        job.saved_album_tracks
            .albums
            .iter()
            .try_fold(job.analyzed, |sum, album| sum.checked_add(album.tracks?))
    };
    job.stage = if !job.saved_album_tracks.albums.is_empty() {
        Stage::SavedAlbumTracks
    } else if more_albums {
        Stage::Albums
    } else {
        job.candidates
            .sort_by_cached_key(|c| std::cmp::Reverse(c.added_at.parse::<jiff::Timestamp>().ok()));
        Stage::Create
    };
}

pub(super) async fn execute_cached(
    client: &ApiClient,
    job: &Job,
    cache_root: Option<&std::path::Path>,
) -> Result<Outcome, ApiError> {
    let scan = &job.saved_album_tracks;
    let album = scan
        .albums
        .front()
        .filter(|album| valid_id(&album.id))
        .ok_or_else(|| invalid("Missing saved-album checkpoint"))?;
    // Commit a page only together with its complete liked-membership answer.
    let prefetched = album.prefetched.as_ref().filter(|page| {
        page.offset == scan.track_offset
            && super::cache_policy::fresh(
                album.prefetched_at,
                album
                    .release_date
                    .as_deref()
                    .and_then(super::cache_policy::month),
                now(),
            )
            && checked_page(scan, page).is_ok()
    });
    let page = super::catalog_cache::load(
        client,
        cache_root,
        &super::catalog_cache::Request {
            account: &job.account,
            album: &album.id,
            release_date: album.release_date.as_deref(),
            offset: scan.track_offset,
            total: scan.track_total.or(album.tracks),
        },
        prefetched.map(|page| (page, album.prefetched_at)),
    )
    .await?;
    checked_page(scan, &page)?;
    let uris: Vec<_> = page
        .items
        .iter()
        .filter(|track| playable(track))
        .map(|track| track.uri.clone())
        .collect();
    let mut liked = BTreeSet::new();
    for chunk in uris.chunks(TRACK_PAGE_SIZE as usize) {
        let flags = client.contains(chunk).await?;
        if flags.len() != chunk.len() {
            return Err(invalid(
                "Spotify returned an incomplete liked-song membership response",
            ));
        }
        liked.extend(
            chunk
                .iter()
                .zip(flags)
                .filter(|(_, liked)| *liked)
                .map(|(uri, _)| uri.clone()),
        );
    }
    Ok(Outcome { page, liked })
}

pub(super) fn apply(job: &mut Job, outcome: Outcome) -> Result<(), ApiError> {
    let Outcome { page, liked } = outcome;
    let scan = &job.saved_album_tracks;
    let next = checked_page(scan, &page)?;
    let album = scan
        .albums
        .front()
        .ok_or_else(|| invalid("Missing saved-album checkpoint"))?;
    let saved_at = album.saved_at.clone();
    let strategy = super::kind(&job.kind).map_err(|message| invalid(&message))?;
    if let Some(album) = job.saved_album_tracks.albums.front_mut() {
        album.prefetched = None;
        album.prefetched_at = 0;
    }
    let mut keys: BTreeSet<_> = job.candidates.iter().map(|c| c.key.clone()).collect();
    let mut uris: BTreeSet<_> = job.candidates.iter().map(|c| c.uri.clone()).collect();
    job.saved_album_tracks.track_total = Some(page.total);
    job.analyzed += page.items.len() as u32;
    for track in page.items {
        let key = track.recording_key().unwrap_or_else(|| track.uri.clone());
        if playable(&track)
            && (strategy.select)(&track, &job.albums, &job.parameters)
            && !liked.contains(&track.uri)
            && !keys.contains(&key)
            && !uris.contains(&track.uri)
        {
            keys.insert(key.clone());
            uris.insert(track.uri.clone());
            job.candidates.push(Candidate {
                uri: track.uri,
                key,
                added_at: saved_at.clone(),
            });
        } else {
            job.skipped += 1;
        }
    }
    if let Some(next) = next {
        job.saved_album_tracks.track_offset = next;
    } else {
        job.saved_album_tracks.albums.pop_front();
        job.saved_album_tracks.track_offset = 0;
        job.saved_album_tracks.track_total = None;
        if job.saved_album_tracks.albums.is_empty() {
            finish_discovery(job);
        }
    }
    Ok(())
}
