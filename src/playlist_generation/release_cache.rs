//! Complete discovery results only. Cache failures never fail a playlist job.
use super::jobs::{Job, now};
use super::releases::{Outcome, Release};
use super::{Period, cache_policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 8 * 1024 * 1024;

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
    include_compilations: bool,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    key: Key,
    stored_at: u64,
    releases: Vec<Release>,
}

fn key(account: &str, artist: &str, year: Option<u16>, include_compilations: bool) -> Key {
    Key {
        version: 2,
        account: account.into(),
        artist: artist.into(),
        year,
        include_compilations,
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

// A cache entry covers the complete artist catalogue or indexed year. A
// historical month can reuse that complete answer even when newer months expire.
fn latest_month(period: &Period, year: Option<u16>) -> Option<i32> {
    let (_, high) = period.bounds().ok()?;
    let high = high.as_deref().and_then(cache_policy::month);
    match (high, year) {
        (Some(high), Some(year)) => Some(high.min(i32::from(year) * 12 + 11)),
        (None, Some(year)) => Some(i32::from(year) * 12 + 11),
        (high, None) => high,
    }
}

fn valid(entry: &Entry, key: &Key) -> bool {
    entry.key == *key
        && entry.releases.iter().all(|release| {
            super::valid_id(&release.id)
                && ((key.year.is_none() && release.date.is_empty())
                    || super::Period::release_period(&release.date).is_some())
        })
}

fn fresh(entry: &Entry, key: &Key, period: &Period, time: u64) -> bool {
    let month = latest_month(period, key.year);
    // Do not freeze a discovery made before the historical month had ended.
    // It may have missed releases published later in that same month.
    let sealed = match (month, cache_policy::current_month(time)) {
        (Some(month), Some(current)) if month < current => {
            cache_policy::current_month(entry.stored_at).is_some_and(|stored| stored > month)
        }
        _ => true,
    };
    valid(entry, key) && sealed && cache_policy::fresh(entry.stored_at, month, time)
}

pub async fn read(root: &Path, job: &Job) -> Option<Outcome> {
    let (artist, year) = job.releases.cache_key()?;
    let key = key(&job.account, &artist, year, job.include_compilations());
    let path = path(root, &key)?;
    if tokio::fs::metadata(&path).await.ok()?.len() > MAX_BYTES {
        return None;
    }
    let bytes = tokio::fs::read(path).await.ok()?;
    let entry: Entry = serde_json::from_slice(&bytes).ok()?;
    fresh(&entry, &key, &job.period, now()).then_some(Outcome::Cached(entry.releases))
}

pub async fn flush(root: &Path, job: &mut Job) {
    let Some(write) = job.releases.pending_cache.take() else {
        return;
    };
    let key = key(
        &job.account,
        &write.artist,
        write.year,
        job.include_compilations(),
    );
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

/// Retain historical discoveries indefinitely. Only corrupt/unsupported cache
/// files are expendable; age or an entry-count cap must not delete archives.
pub async fn prune(root: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(root.join("release-catalog-cache")).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let usable = if meta.len() <= MAX_BYTES {
            tokio::fs::read(entry.path())
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Entry>(&bytes).ok())
                .is_some_and(|cached| cached.key.version == 2 && valid(&cached, &cached.key))
        } else {
            false
        };
        if !usable {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_future_timestamps_and_cache_key_tampering_are_rejected() {
        let expected_key = key("account", "artist", None, false);
        let mut entry = Entry {
            key: key("account", "artist", None, false),
            stored_at: 1000,
            releases: Vec::new(),
        };
        assert!(fresh(&entry, &expected_key, &Period::default(), 1001));
        assert!(!fresh(
            &entry,
            &expected_key,
            &Period::default(),
            1000 + cache_policy::DAY
        ));
        assert!(!fresh(&entry, &expected_key, &Period::default(), 999));
        entry.key.account = "another".into();
        assert!(!fresh(&entry, &expected_key, &Period::default(), 1001));
    }
}

#[cfg(test)]
mod compilation_cache_tests {
    use super::*;

    #[test]
    fn compilation_choices_and_untyped_legacy_entries_cannot_share_cache_paths() {
        let with = key("listener", "artist", Some(2025), true);
        let without = key("listener", "artist", Some(2025), false);
        assert_ne!(
            path(Path::new("cache"), &with),
            path(Path::new("cache"), &without)
        );
        let legacy =
            serde_json::json!({"version":1,"account":"listener","artist":"artist","year":2025});
        assert!(serde_json::from_value::<Key>(legacy).is_err());
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    fn at(value: &str) -> u64 {
        value.parse::<jiff::Timestamp>().unwrap().as_second() as u64
    }

    #[test]
    fn historical_months_reuse_complete_catalogues_without_freezing_incomplete_months() {
        let expected = key("listener", "artist", None, false);
        let mut entry = Entry {
            key: key("listener", "artist", None, false),
            stored_at: at("2026-09-15T00:00:00Z"),
            releases: vec![],
        };
        let september = Period {
            start: "09/2026".into(),
            end: String::new(),
        };
        let now = at("2026-11-20T00:00:00Z");
        assert!(!fresh(&entry, &expected, &september, now));
        entry.stored_at = at("2026-10-01T00:00:00Z");
        assert!(fresh(&entry, &expected, &september, now));
        assert!(!fresh(&entry, &expected, &Period::default(), now));
        let october = Period {
            start: "10/2026".into(),
            end: String::new(),
        };
        entry.stored_at = now - 7 * cache_policy::DAY + 1;
        assert!(fresh(&entry, &expected, &october, now));
        entry.stored_at -= 1;
        assert!(!fresh(&entry, &expected, &october, now));
        let november = Period {
            start: "11/2026".into(),
            end: String::new(),
        };
        entry.stored_at = now - cache_policy::DAY + 1;
        assert!(fresh(&entry, &expected, &november, now));
        entry.stored_at -= 1;
        assert!(!fresh(&entry, &expected, &november, now));
    }

    #[tokio::test]
    async fn startup_retains_old_archives_and_only_removes_invalid_files() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("release-retention-{:x}", rand::random::<u64>()));
        let key = key("listener", "artist", Some(2020), false);
        let file = path(&root, &key).unwrap();
        tokio::fs::create_dir_all(file.parent().unwrap())
            .await
            .unwrap();
        let entry = Entry {
            key,
            stored_at: at("2021-01-01T00:00:00Z"),
            releases: vec![],
        };
        tokio::fs::write(&file, serde_json::to_vec(&entry).unwrap())
            .await
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(entry.stored_at),
            ))
            .unwrap();
        let corrupt = file.with_file_name("broken.json");
        tokio::fs::write(&corrupt, b"broken").await.unwrap();
        prune(&root).await;
        assert!(file.exists());
        assert!(!corrupt.exists());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
