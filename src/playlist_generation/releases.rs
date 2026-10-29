//! Followed-artist discovery and unliked release selection. All cursors and
//! offsets belong to the durable job, not to an in-memory feed cache.
use super::jobs::{Candidate, Job, Stage, checked_next};
use super::{Period, playable, valid_id};
use crate::api::models::{Album, Artist, CursorPage, Page, Track};
use crate::api::{ApiClient, ApiError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub id: String,
    pub date: String,
    pub tracks: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scan {
    // Legacy checkpoints keep their original catalog traversal. New jobs use
    // indexed discovery only for a bounded calendar interval.
    #[serde(default)]
    pub optimized: bool,
    #[serde(default)]
    pub catalog_cacheable: bool,
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    #[serde(default)]
    pub years: Vec<u16>,
    #[serde(default)]
    pub year_index: usize,
    #[serde(default)]
    pub search_offset: u32,
    #[serde(default)]
    pub search_total: Option<u32>,
    #[serde(default)]
    pub fallback: bool,
    #[serde(default)]
    pub buffer: Vec<Release>,
    #[serde(default)]
    pub matched: u32,
    #[serde(default)]
    pub search_tracks: u32,
    #[serde(default)]
    pub cache_hits: u32,
    #[serde(default)]
    pub fallbacks: u32,
    #[serde(default)]
    pub pending_cache: Option<super::release_cache::Write>,
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
    Search,
    Albums,
    Tracks,
}

pub enum Outcome {
    Artists(CursorPage<Artist>),
    Search(Page<Track>),
    Fallback,
    Cached(Vec<Release>),
    Albums(Page<Album>),
    Tracks {
        page: Page<Track>,
        liked: BTreeSet<String>,
    },
}

impl Scan {
    pub fn for_period(period: &Period) -> Self {
        let years = period.search_years();
        Self {
            optimized: !years.is_empty(),
            years,
            ..Self::default()
        }
    }

    pub fn search_query(&self) -> Option<String> {
        if !self.optimized || self.fallback {
            return None;
        }
        let id = self.artists.get(self.artist_index)?;
        let name = self.names.get(id)?.replace(['"', '\\'], " ");
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            return None;
        }
        let year = self.years.get(self.year_index)?;
        Some(format!("artist:\"{name}\" year:{year}"))
    }

    pub fn step(&self) -> Step {
        if self.search_query().is_some() {
            Step::Search
        } else {
            Step::Albums
        }
    }

    pub fn cache_key(&self) -> Option<(String, Option<u16>)> {
        let id = self.artists.get(self.artist_index)?.clone();
        if self.search_query().is_some() {
            (self.search_offset == 0).then(|| (id, self.years.get(self.year_index).copied()))
        } else {
            (self.album_offset == 0).then_some((id, None))
        }
    }
}

fn fallback(job: &mut Job) {
    if !job.releases.fallback {
        job.releases.fallbacks += 1;
    }
    job.releases.fallback = true;
    job.releases.search_offset = 0;
    job.releases.search_total = None;
    job.releases.buffer.clear();
}

fn accept(job: &mut Job, releases: Vec<Release>) {
    for release in releases {
        if !job.releases.seen_albums.insert(release.id.clone()) {
            continue;
        }
        job.albums_checked += 1;
        if job.period.overlaps_release(&release.date) {
            job.releases.matched += 1;
            job.releases.albums.push_back(release);
        }
    }
}

fn next_artist(job: &mut Job) {
    let scan = &mut job.releases;
    scan.artist_index += 1;
    scan.album_offset = 0;
    scan.artist_album_total = None;
    scan.year_index = 0;
    scan.search_offset = 0;
    scan.search_total = None;
    scan.fallback = false;
    scan.buffer.clear();
    scan.catalog_cacheable = false;
    if scan.artist_index == scan.artists.len() {
        if scan.albums.is_empty() {
            finish_selection(job);
        } else {
            job.tracks_total = scan
                .albums
                .iter()
                .try_fold(0_u32, |sum, album| sum.checked_add(album.tracks?));
            job.stage = Stage::ReleaseTracks;
        }
    }
}

fn complete_query(job: &mut Job, cached: Option<Vec<Release>>) {
    let search = job.releases.search_query().is_some();
    let releases = if let Some(releases) = cached {
        job.releases.cache_hits += 1;
        releases
    } else {
        let releases = std::mem::take(&mut job.releases.buffer);
        if search || job.releases.catalog_cacheable {
            job.releases.pending_cache = Some(super::release_cache::Write {
                artist: job.releases.artists[job.releases.artist_index].clone(),
                year: if search {
                    job.releases.years.get(job.releases.year_index).copied()
                } else {
                    None
                },
                releases: releases.clone(),
            });
        }
        releases
    };
    accept(job, releases);
    if search && job.releases.year_index + 1 < job.releases.years.len() {
        job.releases.year_index += 1;
        job.releases.search_offset = 0;
        job.releases.search_total = None;
    } else {
        next_artist(job);
    }
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
        Step::Search => {
            let query = scan
                .search_query()
                .ok_or_else(|| invalid("Missing release search checkpoint"))?;
            match client
                .search_release_tracks(&query, scan.search_offset)
                .await
            {
                Ok(page) => Ok(Outcome::Search(page)),
                Err(
                    ApiError::Status {
                        status: 400 | 403 | 404 | 422,
                        ..
                    }
                    | ApiError::Decode(_),
                ) => Ok(Outcome::Fallback),
                Err(error) => Err(error),
            }
        }
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
                job.releases
                    .names
                    .insert(artist.id.clone(), artist.name.clone());
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
        Outcome::Fallback => {
            job.releases.optimized = false;
            fallback(job);
        }
        Outcome::Cached(releases) => complete_query(job, Some(releases)),
        Outcome::Search(page) => {
            let next = checked_next(&page, job.releases.search_offset);
            if page.total >= 1000
                || next.is_err()
                || job
                    .releases
                    .search_total
                    .is_some_and(|total| total != page.total)
            {
                fallback(job);
                return Ok(());
            }
            let artist = job
                .releases
                .artists
                .get(job.releases.artist_index)
                .ok_or_else(|| invalid("Missing followed artist checkpoint"))?;
            let mut releases = Vec::new();
            for track in &page.items {
                if !track
                    .artists
                    .iter()
                    .any(|credit| credit.id.as_ref() == Some(artist))
                {
                    continue;
                }
                let Some(album) = track.album.as_ref().filter(|album| valid_id(&album.id)) else {
                    fallback(job);
                    return Ok(());
                };
                let Some(date) = album
                    .release_date
                    .as_ref()
                    .filter(|date| Period::release_period(date).is_some())
                else {
                    fallback(job);
                    return Ok(());
                };
                releases.push(Release {
                    id: album.id.clone(),
                    date: date.clone(),
                    tracks: album.total_tracks,
                });
            }
            job.releases.search_total = Some(page.total);
            job.releases.search_tracks += page.items.len() as u32;
            for release in releases {
                if let Some(previous) = job.releases.buffer.iter().find(|old| old.id == release.id)
                {
                    if previous.date != release.date {
                        fallback(job);
                        return Ok(());
                    }
                } else {
                    job.releases.buffer.push(release);
                }
            }
            if let Some(next) = next? {
                if next >= 1000 {
                    fallback(job);
                } else {
                    job.releases.search_offset = next;
                }
            } else {
                complete_query(job, None);
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
            if job.releases.album_offset == 0 {
                job.releases.catalog_cacheable = true;
            }
            job.releases.artist_album_total = Some(page.total);
            let releases: Vec<_> = page
                .items
                .into_iter()
                .map(|album| {
                    let date = album
                        .release_date
                        .filter(|date| Period::release_period(date).is_some())
                        .unwrap_or_default();
                    Release {
                        id: album.id,
                        date,
                        tracks: album.total_tracks,
                    }
                })
                .collect();
            // Apply each catalog page immediately so old checkpoints keep their
            // cursors and selected releases. Cache only a complete traversal.
            job.releases.buffer.extend(releases.iter().cloned());
            accept(job, releases);
            if let Some(next) = next {
                job.releases.album_offset = next;
            } else {
                complete_query(job, None);
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
