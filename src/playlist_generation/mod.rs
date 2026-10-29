//! Resumable playlist selection, independent of the native interface.
pub mod jobs;
#[cfg(test)]
mod optimization_tests;
pub mod period;
pub mod release_cache;
#[cfg(test)]
mod release_tests;
pub mod releases;
#[cfg(test)]
mod tests;

use crate::api::models::{SavedTrack, Track};
pub use period::Period;
use std::collections::{BTreeMap, BTreeSet};

pub type Parameters = BTreeMap<String, String>;

pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    LikedTracks,
    FollowedReleases,
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
        label: "Músicas Curtidas de Álbuns Não Salvos (MCANS)",
        prefix: "MCANS",
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
        label: "Músicas Não Curtidas de Artistas Seguidos (MNCAS)",
        prefix: "MNCAS",
        source: Source::FollowedReleases,
        needs_albums: false,
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
        && period.contains(saved.added_at.as_deref())
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
