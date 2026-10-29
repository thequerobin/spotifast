//! Followed-artist discovery and unliked release selection. All cursors and
//! offsets belong to the durable job, not to an in-memory feed cache.
use super::jobs::{Candidate, Job, Stage, checked_next};
use super::{Period, playable, valid_id};
use crate::api::models::{Album, Artist, CursorPage, Page, Track};
use crate::api::{ApiClient, ApiError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub id: String,
    pub date: String,
    pub tracks: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scan {
    pub artists: Vec<String>,
    pub followed_total: Option<u32>,
    pub artist_ids: BTreeSet<String>,
    pub after: Option<String>,
    pub seen_cursors: BTreeSet<String>,
    pub artist_index: usize,
    pub album_offset: u32,
    pub artist_album_total: Option<u32>,
    pub seen_albums: BTreeSet<String>,
    pub albums: VecDeque<Release>,
    pub track_offset: u32,
    pub album_track_total: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum Step {
    Artists,
    Albums,
    Tracks,
}

pub enum Outcome {
    Artists(CursorPage<Artist>),
    Albums(Page<Album>),
    Tracks {
        page: Page<Track>,
        liked: BTreeSet<String>,
    },
}

fn invalid(message: &str) -> ApiError {
    ApiError::Decode(message.into())
}

pub async fn execute(client: &ApiClient, job: &Job, step: &Step) -> Result<Outcome, ApiError> {
    let scan = &job.releases;
    match step {
        Step::Artists => Ok(Outcome::Artists(
            client.followed_artists(scan.after.as_deref(), 50).await?,
        )),
        Step::Albums => {
            let artist = scan
                .artists
                .get(scan.artist_index)
                .ok_or_else(|| invalid("Missing followed artist checkpoint"))?;
            Ok(Outcome::Albums(
                client
                    .artist_albums(
                        artist,
                        "album,single,appears_on,compilation",
                        scan.album_offset,
                        50,
                    )
                    .await?,
            ))
        }
        Step::Tracks => {
            let album = scan
                .albums
                .front()
                .ok_or_else(|| invalid("Missing release checkpoint"))?;
            // The membership endpoint takes at most 40 URIs. Each page and its
            // membership answer are committed together, including after retry.
            let page = client
                .album_tracks(&album.id, scan.track_offset, 40)
                .await?;
            checked_next(&page, scan.track_offset)?;
            let uris: Vec<_> = page
                .items
                .iter()
                .filter(|track| relevant(track, scan))
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
            Ok(Outcome::Tracks { page, liked })
        }
    }
}

pub fn relevant(track: &Track, scan: &Scan) -> bool {
    playable(track)
        && track
            .artists
            .iter()
            .filter_map(|artist| artist.id.as_ref())
            .any(|id| scan.artist_ids.contains(id))
}

fn finish_selection(job: &mut Job) {
    job.candidates.sort_by_cached_key(|candidate| {
        std::cmp::Reverse(candidate.added_at.parse::<jiff::Timestamp>().ok())
    });
    job.tracks_total = Some(job.analyzed);
    job.stage = Stage::Create;
}

pub fn apply(job: &mut Job, outcome: Outcome) -> Result<(), ApiError> {
    match outcome {
        Outcome::Artists(page) => {
            if page.items.iter().any(|artist| !valid_id(&artist.id)) {
                return Err(invalid(
                    "Spotify returned a followed artist without an identifier",
                ));
            }
            let next = if page.next.is_some() {
                let after = page
                    .cursors
                    .as_ref()
                    .and_then(|cursor| cursor.after.as_ref())
                    .filter(|after| valid_id(after) && !job.releases.seen_cursors.contains(*after));
                if page.items.is_empty() || after.is_none() {
                    return Err(invalid(
                        "Spotify returned an incomplete or repeated artist cursor",
                    ));
                }
                after.cloned()
            } else {
                None
            };
            if page.next.is_none()
                && page.total.is_some_and(|total| {
                    let mut all = job.releases.artist_ids.clone();
                    all.extend(page.items.iter().map(|artist| artist.id.clone()));
                    all.len() != total as usize
                })
            {
                return Err(invalid(
                    "Spotify returned an incomplete followed-artist library",
                ));
            }
            for artist in page.items {
                if job.releases.artist_ids.insert(artist.id.clone()) {
                    job.releases.artists.push(artist.id);
                }
            }
            job.releases.followed_total = page.total;
            job.releases.after = next.clone();
            if let Some(after) = next {
                job.releases.seen_cursors.insert(after);
            } else if job.releases.artists.is_empty() {
                finish_selection(job);
            } else {
                job.stage = Stage::ArtistReleases;
            }
        }
        Outcome::Albums(page) => {
            let next = checked_next(&page, job.releases.album_offset)?;
            if job
                .releases
                .artist_album_total
                .is_some_and(|total| total != page.total)
            {
                return Err(invalid(
                    "The artist's releases changed during scanning; cancel and start again",
                ));
            }
            if page.items.iter().any(|album| !valid_id(&album.id)) {
                return Err(invalid("Spotify returned a release without an identifier"));
            }
            job.releases.artist_album_total = Some(page.total);
            job.albums_checked += page.items.len() as u32;
            for album in page.items {
                if !job.releases.seen_albums.insert(album.id.clone()) {
                    continue;
                }
                let Some(date) = album
                    .release_date
                    .filter(|date| job.period.overlaps_release(date))
                else {
                    continue;
                };
                job.releases.albums.push_back(Release {
                    id: album.id,
                    date,
                    tracks: album.total_tracks,
                });
            }
            if let Some(next) = next {
                job.releases.album_offset = next;
            } else {
                job.releases.artist_index += 1;
                job.releases.album_offset = 0;
                job.releases.artist_album_total = None;
                if job.releases.artist_index == job.releases.artists.len() {
                    if job.releases.albums.is_empty() {
                        finish_selection(job);
                    } else {
                        job.tracks_total = job
                            .releases
                            .albums
                            .iter()
                            .try_fold(0_u32, |sum, album| sum.checked_add(album.tracks?));
                        job.stage = Stage::ReleaseTracks;
                    }
                }
            }
        }
        Outcome::Tracks { page, liked } => {
            let next = checked_next(&page, job.releases.track_offset)?;
            if job
                .releases
                .album_track_total
                .is_some_and(|total| total != page.total)
            {
                return Err(invalid(
                    "The album's tracks changed during scanning; cancel and start again",
                ));
            }
            let album = job
                .releases
                .albums
                .front()
                .ok_or_else(|| invalid("Missing release checkpoint"))?;
            let date = Period::release_period(&album.date)
                .and_then(|period| period.bounds().ok())
                .and_then(|(start, _)| start)
                .ok_or_else(|| invalid("Invalid release date checkpoint"))?;
            let mut seen: BTreeSet<_> = job
                .candidates
                .iter()
                .map(|candidate| candidate.key.clone())
                .collect();
            let mut uris: BTreeSet<_> = job
                .candidates
                .iter()
                .map(|candidate| candidate.uri.clone())
                .collect();
            if album.tracks.is_some_and(|total| total != page.total) {
                return Err(invalid(
                    "The release track total changed after discovery; cancel and start again",
                ));
            }
            let strategy = super::kind(&job.kind).map_err(|message| invalid(&message))?;
            job.releases.album_track_total = Some(page.total);
            job.analyzed += page.items.len() as u32;
            for track in page.items {
                let key = track.recording_key().unwrap_or_else(|| track.uri.clone());
                if relevant(&track, &job.releases)
                    && (strategy.select)(&track, &job.albums, &job.parameters)
                    && !liked.contains(&track.uri)
                    && seen.insert(key.clone())
                    && uris.insert(track.uri.clone())
                {
                    job.candidates.push(Candidate {
                        uri: track.uri,
                        key,
                        added_at: format!("{date}T00:00:00Z"),
                    });
                } else {
                    job.skipped += 1;
                }
            }
            if let Some(next) = next {
                job.releases.track_offset = next;
            } else {
                job.releases.albums.pop_front();
                job.releases.track_offset = 0;
                job.releases.album_track_total = None;
                if job.releases.albums.is_empty() {
                    finish_selection(job);
                }
            }
        }
    }
    Ok(())
}
