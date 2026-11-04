//! Resumable playlist selection, independent of the native interface.
mod cache_policy;
mod catalog_cache;
pub mod jobs;
#[cfg(test)]
mod optimization_tests;
pub mod period;
pub mod release_cache;
#[cfg(test)]
mod release_tests;
pub mod releases;
#[cfg(test)]
mod saved_album_tests;
pub mod saved_albums;
#[cfg(test)]
mod tests;

use crate::api::models::{SavedTrack, Track};
pub use period::Period;
use std::collections::{BTreeMap, BTreeSet};

pub type Parameters = BTreeMap<String, String>;

pub const INCLUDE_COMPILATIONS: &str = "include_compilations";
pub const DATE_BASIS: &str = "date_basis";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateBasis {
    Added,
    Release,
}

impl DateBasis {
    /// Missing parameters retain the meaning of existing saved jobs.
    pub fn for_source(source: Source, parameters: &Parameters) -> Self {
        match parameters.get(DATE_BASIS).map(String::as_str) {
            Some("release") => Self::Release,
            Some("added") => Self::Added,
            _ if source == Source::FollowedReleases => Self::Release,
            _ => Self::Added,
        }
    }

    pub fn value(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Release => "release",
        }
    }

    pub fn matches(self, period: &Period, added_at: Option<&str>, release: Option<&str>) -> bool {
        match self {
            Self::Added => period.contains(added_at),
            Self::Release if period.bounds() == Ok((None, None)) => true,
            Self::Release => release.is_some_and(|date| period.overlaps_release(date)),
        }
    }
}

pub fn validate_date_basis(source: Source, parameters: &Parameters) -> Result<(), String> {
    if parameters
        .get(DATE_BASIS)
        .is_some_and(|value| !matches!(value.as_str(), "added" | "release"))
    {
        return Err("Invalid date basis".into());
    }
    if source == Source::FollowedReleases
        && DateBasis::for_source(source, parameters) == DateBasis::Added
    {
        return Err("Unliked songs from followed artists have no library addition date".into());
    }
    Ok(())
}

pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    LikedTracks,
    FollowedReleases,
    SavedAlbumTracks,
}

pub struct Kind {
    pub id: &'static str,
    pub label: &'static str,
    pub prefix: &'static str,
    pub source: Source,
    pub needs_albums: bool,
    pub fields: &'static [Field],
    pub validate: fn(&Parameters) -> Result<(), String>,
    pub select: fn(&Track, &BTreeSet<String>, &Parameters) -> bool,
}

pub static KINDS: &[Kind] = &[
    Kind {
        id: "mcans",
        label: "Liked Songs from Unsaved Albums (LSUA)",
        prefix: "LSUA",
        source: Source::LikedTracks,
        needs_albums: true,
        fields: &[],
        validate: |_| Ok(()),
        select: |track, albums, _parameters| {
            track
                .album
                .as_ref()
                .map(|album| &album.id)
                .is_some_and(|id| valid_id(id) && !albums.contains(id))
        },
    },
    Kind {
        id: "unliked-followed-releases",
        label: "Unliked Songs from Followed Artists (USFA)",
        prefix: "USFA",
        source: Source::FollowedReleases,
        needs_albums: false,
        fields: &[],
        validate: |parameters| {
            if parameters
                .get(INCLUDE_COMPILATIONS)
                .is_some_and(|value| !matches!(value.as_str(), "true" | "false"))
            {
                return Err("Invalid compilation option".into());
            }
            Ok(())
        },
        select: |_, _, _| true,
    },
    Kind {
        id: "unliked-saved-albums",
        label: "Unliked Songs from Saved Albums (USSA)",
        prefix: "USSA",
        source: Source::SavedAlbumTracks,
        needs_albums: true,
        fields: &[],
        validate: |_| Ok(()),
        select: |_, _, _| true,
    },
];

pub fn kind(id: &str) -> Result<&'static Kind, String> {
    KINDS
        .iter()
        .find(|kind| kind.id == id)
        .ok_or_else(|| "Unknown playlist type".into())
}

pub fn default_name(id: &str, period: &Period) -> Result<String, String> {
    period.bounds()?;
    Ok(format!("{} - {}", kind(id)?.prefix, period.label()))
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

pub fn eligible(
    saved: &SavedTrack,
    kind: &Kind,
    period: &Period,
    albums: &BTreeSet<String>,
    parameters: &Parameters,
) -> bool {
    playable(&saved.track)
        && DateBasis::for_source(kind.source, parameters).matches(
            period,
            saved.added_at.as_deref(),
            saved
                .track
                .album
                .as_ref()
                .and_then(|album| album.release_date.as_deref()),
        )
        && (kind.select)(&saved.track, albums, parameters)
}

/// Shared validity checks for every selection source.
pub fn playable(track: &crate::api::models::Track) -> bool {
    !track.is_local
        && track.is_playable != Some(false)
        && track.id.as_ref().is_some_and(|id| valid_id(id))
        && track
            .uri
            .strip_prefix("spotify:track:")
            .is_some_and(|id| valid_id(id) && track.id.as_deref() == Some(id))
}
