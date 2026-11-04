//! Account-scoped album pages shared by playlist jobs and application restarts.
use super::{cache_policy, jobs::checked_next, jobs::now, valid_id};
use crate::api::models::{Page, Track};
use crate::api::{ApiClient, ApiError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const PAGE_SIZE: u32 = 40;
const MAX_BYTES: u64 = 256 * 1024;

pub struct Request<'a> {
    pub account: &'a str,
    pub album: &'a str,
    pub release_date: Option<&'a str>,
    pub offset: u32,
    pub total: Option<u32>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Key {
    version: u8,
    account: String,
    album: String,
    offset: u32,
}

impl Request<'_> {
    fn key(&self) -> Key {
        Key {
            version: 1,
            account: self.account.into(),
            album: self.album.into(),
            offset: self.offset,
        }
    }

    fn valid(&self, page: &Page<Track>) -> bool {
        valid_id(self.album)
            && page.limit == PAGE_SIZE
            && self.offset.checked_add(PAGE_SIZE).is_some()
            && self.total.is_none_or(|total| total == page.total)
            && page.items.len() as u32 == page.total.saturating_sub(self.offset).min(PAGE_SIZE)
            && checked_next(page, self.offset).is_ok()
            && page.next.is_some() == (self.offset + (page.items.len() as u32) < page.total)
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    key: Key,
    stored_at: u64,
    release_month: Option<i32>,
    page: Page<Track>,
}

fn path(root: &Path, key: &Key) -> Option<PathBuf> {
    let digest = Sha256::digest(serde_json::to_vec(key).ok()?);
    Some(
        root.join("album-track-cache")
            .join(format!("{digest:x}.json")),
    )
}

pub async fn read(root: &Path, request: &Request<'_>) -> Option<Page<Track>> {
    let key = request.key();
    let path = path(root, &key)?;
    if tokio::fs::metadata(&path).await.ok()?.len() > MAX_BYTES {
        return None;
    }
    let entry: Entry = serde_json::from_slice(&tokio::fs::read(path).await.ok()?).ok()?;
    let month = request.release_date.and_then(cache_policy::month);
    // Old checkpoints lack release dates. A fresh library/catalogue response
    // can supply that date without downloading the same fresh page again.
    let dated = entry.release_month.is_none() && month.is_some();
    let retention_month = if dated { None } else { month };
    if entry.key != key
        || (entry.release_month != month && !dated)
        || !cache_policy::fresh(entry.stored_at, retention_month, now())
        || !request.valid(&entry.page)
    {
        return None;
    }
    if dated {
        write_at(root, request, &entry.page, entry.stored_at).await;
    }
    Some(entry.page)
}

pub async fn write(root: &Path, request: &Request<'_>, page: &Page<Track>) {
    write_at(root, request, page, now()).await;
}

async fn write_at(root: &Path, request: &Request<'_>, page: &Page<Track>, stored_at: u64) {
    if !request.valid(page) {
        return;
    }
    let entry = Entry {
        key: request.key(),
        stored_at,
        release_month: request.release_date.and_then(cache_policy::month),
        page: page.clone(),
    };
    let Some(path) = path(root, &entry.key) else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(&entry) else {
        return;
    };
    if bytes.len() as u64 > MAX_BYTES {
        return;
    }
    if let Some(parent) = path.parent()
        && tokio::fs::create_dir_all(parent).await.is_err()
    {
        return;
    }
    // An expendable cache must not turn a successful Spotify read into a job failure.
    let _ = tokio::task::spawn_blocking(move || fastframe_file::write(&path, bytes)).await;
}

pub async fn load(
    client: &ApiClient,
    root: Option<&Path>,
    request: &Request<'_>,
    prefetched: Option<(&Page<Track>, u64)>,
) -> Result<Page<Track>, ApiError> {
    // Prefer metadata Spotify already returned for free with the live library.
    if let Some((page, fetched_at)) = prefetched {
        if let Some(root) = root {
            write_at(root, request, page, fetched_at).await;
        }
        return Ok(page.clone());
    }
    if let Some(root) = root
        && let Some(page) = read(root, request).await
    {
        client.cost_manager().cache_access(true);
        return Ok(page);
    }
    let page = client
        .album_tracks(request.album, request.offset, PAGE_SIZE)
        .await?;
    if let Some(root) = root {
        write(root, request, &page).await;
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disk_pages_are_scoped_validated_and_historical_pages_do_not_expire() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("catalog-cache-{:x}", rand::random::<u64>()));
        let request = Request {
            account: "listener",
            album: "album",
            release_date: Some("2020-01-01"),
            offset: 0,
            total: Some(1),
        };
        let page = Page {
            items: vec![Track::default()],
            total: 1,
            limit: 40,
            offset: 0,
            next: None,
        };
        write(&root, &request, &page).await;
        let file = path(&root, &request.key()).unwrap();
        let mut entry: Entry =
            serde_json::from_slice(&tokio::fs::read(&file).await.unwrap()).unwrap();
        entry.stored_at = 1;
        tokio::fs::write(&file, serde_json::to_vec(&entry).unwrap())
            .await
            .unwrap();
        assert!(read(&root, &request).await.is_some());
        assert!(
            read(
                &root,
                &Request {
                    account: "other",
                    ..request
                }
            )
            .await
            .is_none()
        );
        assert!(
            read(
                &root,
                &Request {
                    total: Some(2),
                    ..request
                }
            )
            .await
            .is_none()
        );
        assert!(
            read(
                &root,
                &Request {
                    offset: 40,
                    ..request
                }
            )
            .await
            .is_none()
        );
        assert!(
            read(
                &root,
                &Request {
                    release_date: None,
                    ..request
                }
            )
            .await
            .is_none()
        );
        let undated = Request {
            release_date: None,
            ..request
        };
        let fetched_at = now() - cache_policy::DAY + 60;
        let client = ApiClient::test_transport("http://127.0.0.1:1".into());
        load(&client, Some(&root), &undated, Some((&page, fetched_at)))
            .await
            .unwrap();
        assert!(read(&root, &undated).await.is_some());
        let saved: Entry = serde_json::from_slice(&tokio::fs::read(&file).await.unwrap()).unwrap();
        assert_eq!(saved.stored_at, fetched_at);
        assert!(read(&root, &request).await.is_some());
        let dated: Entry = serde_json::from_slice(&tokio::fs::read(&file).await.unwrap()).unwrap();
        assert_eq!(dated.stored_at, fetched_at);
        assert_eq!(
            dated.release_month,
            request.release_date.and_then(cache_policy::month)
        );
        write_at(&root, &undated, &page, now() - cache_policy::DAY).await;
        assert!(read(&root, &undated).await.is_none());
        assert!(read(&root, &request).await.is_none());
        entry.stored_at = now() + 100;
        tokio::fs::write(&file, serde_json::to_vec(&entry).unwrap())
            .await
            .unwrap();
        assert!(read(&root, &request).await.is_none());
        tokio::fs::write(&file, b"partial cache write")
            .await
            .unwrap();
        assert!(read(&root, &request).await.is_none());
        write(
            &root,
            &request,
            &Page {
                next: Some("next".into()),
                ..page
            },
        )
        .await;
        assert!(read(&root, &request).await.is_none());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
