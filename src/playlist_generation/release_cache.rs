//! Complete discovery results only. Cache failures never fail a playlist job.
use super::jobs::{Job, now};
use super::releases::{Outcome, Release};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_AGE: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Write {
    pub artist: String,
    pub year: Option<u16>,
    pub releases: Vec<Release>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Key {
    version: u8,
    account: String,
    artist: String,
    year: Option<u16>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    key: Key,
    stored_at: u64,
    releases: Vec<Release>,
}

fn key(account: &str, artist: &str, year: Option<u16>) -> Key {
    Key {
        version: 1,
        account: account.into(),
        artist: artist.into(),
        year,
    }
}

fn path(root: &Path, key: &Key) -> Option<PathBuf> {
    let bytes = serde_json::to_vec(key).ok()?;
    let digest = Sha256::digest(bytes);
    Some(
        root.join("release-catalog-cache")
            .join(format!("{digest:x}.json")),
    )
}

fn fresh(entry: &Entry, key: &Key, time: u64) -> bool {
    let age_limit = if key
        .year
        .is_some_and(|year| year < jiff::Zoned::now().year() as u16)
    {
        MAX_AGE
    } else {
        15 * 60
    };
    entry.key == *key
        && time
            .checked_sub(entry.stored_at)
            .is_some_and(|age| age < age_limit)
        && entry.releases.iter().all(|release| {
            super::valid_id(&release.id)
                && ((key.year.is_none() && release.date.is_empty())
                    || super::Period::release_period(&release.date).is_some())
        })
}

pub async fn read(root: &Path, job: &Job) -> Option<Outcome> {
    let (artist, year) = job.releases.cache_key()?;
    let key = key(&job.account, &artist, year);
    let path = path(root, &key)?;
    if tokio::fs::metadata(&path).await.ok()?.len() > MAX_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    let entry: Entry = serde_json::from_slice(&bytes).ok()?;
    fresh(&entry, &key, now()).then_some(Outcome::Cached(entry.releases))
}

pub async fn flush(root: &Path, job: &mut Job) {
    let Some(write) = job.releases.pending_cache.take() else {
        return;
    };
    let key = key(&job.account, &write.artist, write.year);
    let Some(path) = path(root, &key) else {
        return;
    };
    let entry = Entry {
        key,
        stored_at: now(),
        releases: write.releases,
    };
    let Ok(bytes) = serde_json::to_vec(&entry) else {
        return;
    };
    if bytes.len() as u64 > MAX_BYTES {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return;
    }
    let result = tokio::task::spawn_blocking(move || fastframe_file::write(&path, bytes)).await;
    if !matches!(result, Ok(Ok(()))) {
        log::debug!("Could not persist release discovery cache");
    }
}

/// The cache is expendable and separate from the durable job checkpoints.
pub async fn prune(root: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(root.join("release-catalog-cache")).await else {
        return;
    };
    let mut retained = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let expired = meta
            .modified()
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_none_or(|age| age.as_secs() > MAX_AGE);
        if expired || meta.len() > MAX_BYTES {
            let _ = tokio::fs::remove_file(entry.path()).await;
        } else {
            retained.push((meta.modified().ok(), entry.path()));
        }
    }
    retained.sort_by_key(|(modified, _)| *modified);
    let excess = retained.len().saturating_sub(4096);
    for (_, path) in retained.into_iter().take(excess) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_future_timestamps_and_cache_key_tampering_are_rejected() {
        let expected_key = key("account", "artist", None);
        let mut entry = Entry {
            key: key("account", "artist", None),
            stored_at: 1000,
            releases: Vec::new(),
        };
        assert!(fresh(&entry, &expected_key, 1001));
        assert!(!fresh(&entry, &expected_key, 1000 + 15 * 60));
        assert!(!fresh(&entry, &expected_key, 999));
        entry.key.account = "another".into();
        assert!(!fresh(&entry, &expected_key, 1001));
    }
}
