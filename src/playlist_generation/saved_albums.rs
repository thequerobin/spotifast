//! Saved-album discovery and unliked tracks, checkpointed with the parent job.
use super::jobs::{Candidate, Job, Stage, checked_next};
use super::{DateBasis, Source, playable, valid_id};
use crate::api::models::{Page, SavedAlbum, Track};
use crate::api::{ApiClient, ApiError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub saved_at: String,
    pub tracks: Option<u32>,
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

pub(super) fn collect(job: &mut Job, albums: &[SavedAlbum]) {
    let mut seen = job.albums.clone();
    let date_basis = DateBasis::for_source(Source::SavedAlbumTracks, &job.parameters);
    for saved in albums {
        if seen.insert(saved.album.id.clone())
            && date_basis.matches(
                &job.period,
                saved.added_at.as_deref(),
                saved.album.release_date.as_deref(),
            )
        {
            job.saved_album_tracks.albums.push_back(Album {
                id: saved.album.id.clone(),
                saved_at: saved
                    .added_at
                    .as_deref()
                    .and_then(|value| value.parse::<jiff::Timestamp>().ok())
                    .map(|stamp| stamp.to_string())
                    .unwrap_or_default(),
                tracks: saved.album.total_tracks,
            });
        }
    }
}

pub(super) fn finish_discovery(job: &mut Job) {
    job.tracks_total = job
        .saved_album_tracks
        .albums
        .iter()
        .try_fold(0_u32, |sum, album| sum.checked_add(album.tracks?));
    job.stage = if job.saved_album_tracks.albums.is_empty() {
        Stage::Create
    } else {
        Stage::SavedAlbumTracks
    };
}

pub(super) async fn execute(client: &ApiClient, job: &Job) -> Result<Outcome, ApiError> {
    let scan = &job.saved_album_tracks;
    let album = scan
        .albums
        .front()
        .filter(|album| valid_id(&album.id))
        .ok_or_else(|| invalid("Missing saved-album checkpoint"))?;
    // Commit a page only together with its complete liked-membership answer.
    let page = client
        .album_tracks(&album.id, scan.track_offset, 40)
        .await?;
    checked_page(scan, &page)?;
    let uris: Vec<_> = page
        .items
        .iter()
        .filter(|track| playable(track))
        .map(|track| track.uri.clone())
        .collect();
    let mut liked = BTreeSet::new();
    for chunk in uris.chunks(40) {
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
            job.candidates.sort_by_cached_key(|c| {
                std::cmp::Reverse(c.added_at.parse::<jiff::Timestamp>().ok())
            });
            job.tracks_total = Some(job.analyzed);
            job.stage = Stage::Create;
        }
    }
    Ok(())
}
