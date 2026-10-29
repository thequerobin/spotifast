use super::{Period, jobs, release_cache, releases};
use crate::api::models::{Album, Artist, ArtistRef, CursorPage, Page, Track};

fn job() -> jobs::Job {
    let mut job = jobs::Job::new(
        "listener".into(),
        "unliked-followed-releases".into(),
        Period {
            start: "15/03/2025".into(),
            end: "20/03/2025".into(),
        },
        "MNCAS".into(),
        false,
    )
    .unwrap();
    releases::apply(
        &mut job,
        releases::Outcome::Artists(CursorPage {
            items: vec![Artist {
                id: "followed".into(),
                name: "Followed Artist".into(),
                ..Default::default()
            }],
            total: Some(1),
            ..Default::default()
        }),
    )
    .unwrap();
    job
}

fn track(id: &str, artist: &str, album: &str, date: &str) -> Track {
    Track {
        id: Some(id.into()),
        uri: format!("spotify:track:{id}"),
        artists: vec![ArtistRef {
            id: Some(artist.into()),
            ..Default::default()
        }],
        album: Some(Album {
            id: album.into(),
            release_date: Some(date.into()),
            total_tracks: Some(2),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn crash(job: &jobs::Job) -> jobs::Job {
    serde_json::from_slice(&serde_json::to_vec(job).unwrap()).unwrap()
}

#[test]
fn bounded_year_queries_and_open_or_broad_catalog_fallback() {
    assert_eq!(
        job().releases.search_query().as_deref(),
        Some("artist:\"Followed Artist\" year:2025")
    );
    assert_eq!(
        Period {
            start: "12/2024".into(),
            end: "01/2026".into()
        }
        .search_years(),
        [2024, 2025, 2026]
    );
    for period in [
        Period::default(),
        Period {
            start: "2020".into(),
            end: "2025".into(),
        },
        Period {
            start: String::new(),
            end: "2025".into(),
        },
    ] {
        assert!(period.search_years().is_empty());
    }
    let mut job = job();
    job.releases
        .names
        .insert("followed".into(), "A \" OR year:1900 \\\nB".into());
    assert_eq!(
        job.releases.search_query().unwrap(),
        "artist:\"A OR year:1900 B\" year:2025"
    );
}

#[test]
fn search_checks_artist_ids_deduplicates_albums_and_filters_the_actual_period() {
    let mut job = job();
    releases::apply(
        &mut job,
        releases::Outcome::Search(Page::whole(vec![
            track("first", "followed", "new", "2025-03-15"),
            track("second", "followed", "new", "2025-03-15"),
            track("oldtrack", "followed", "old", "2025-01-01"),
            track("homonym", "someoneelse", "wrong", "2025-03-16"),
        ])),
    )
    .unwrap();
    assert_eq!(job.stage, jobs::Stage::ReleaseTracks);
    assert_eq!(job.releases.albums.len(), 1);
    assert_eq!(job.releases.albums[0].id, "new");
    assert_eq!(job.view().albums_checked, 2);
    assert_eq!(job.view().matched_releases, 1);
    assert_eq!(job.releases.search_tracks, 4);
    let cache = job.releases.pending_cache.as_ref().unwrap();
    assert_eq!(cache.releases.len(), 2); // All year results, not only this week.
}

#[test]
fn paginated_search_resumes_with_its_exact_year_offset_and_partial_results() {
    let mut job = job();
    releases::apply(
        &mut job,
        releases::Outcome::Search(Page {
            items: vec![track("first", "followed", "new", "2025-03-15")],
            limit: 1,
            offset: 0,
            total: 2,
            next: Some("next".into()),
        }),
    )
    .unwrap();
    let mut job = crash(&job);
    assert_eq!(job.releases.search_offset, 1);
    assert_eq!(job.releases.buffer.len(), 1);
    assert!(job.releases.pending_cache.is_none());
    assert!(matches!(
        jobs::prepare(&mut job),
        jobs::Step::Release(releases::Step::Search)
    ));
    releases::apply(
        &mut job,
        releases::Outcome::Search(Page {
            items: vec![track("second", "followed", "new", "2025-03-15")],
            limit: 1,
            offset: 1,
            total: 2,
            next: None,
        }),
    )
    .unwrap();
    assert_eq!(job.releases.albums.len(), 1);
    assert_eq!(
        job.releases.pending_cache.as_ref().unwrap().releases.len(),
        1
    );
}

#[test]
fn truncated_changed_and_malformed_searches_fall_back_instead_of_claiming_completeness() {
    for (offset, total, next) in [
        (0, 1000, true),
        (0, 1001, true),
        (5, 1, false),
        (0, 2, false),
    ] {
        let mut job = job();
        releases::apply(
            &mut job,
            releases::Outcome::Search(Page {
                items: vec![track("first", "followed", "new", "2025-03-15")],
                limit: 1,
                offset,
                total,
                next: next.then(|| "next".into()),
            }),
        )
        .unwrap();
        assert_eq!(job.releases.artist_index, 0);
        assert!(job.releases.fallback);
        assert!(job.releases.buffer.is_empty());
        assert!(matches!(
            jobs::prepare(&mut job),
            jobs::Step::Release(releases::Step::Albums)
        ));
        assert!(job.releases.pending_cache.is_none());
    }
}

#[test]
fn each_year_and_artist_has_an_independent_cursor() {
    let mut job = job();
    job.releases.years = vec![2024, 2025];
    for year in [2024, 2025] {
        assert!(
            job.releases
                .search_query()
                .unwrap()
                .ends_with(&format!("year:{year}"))
        );
        releases::apply(&mut job, releases::Outcome::Search(Page::whole(Vec::new()))).unwrap();
    }
    assert_eq!(job.releases.artist_index, 1);
    assert_eq!(job.stage, jobs::Stage::Create);
}

#[test]
fn legacy_catalog_resume_does_not_cache_an_incomplete_catalog() {
    let mut value = serde_json::to_value(job()).unwrap();
    for key in ["optimized", "years", "names", "catalog_cacheable"] {
        value["releases"].as_object_mut().unwrap().remove(key);
    }
    let mut job: jobs::Job = serde_json::from_value(value).unwrap();
    job.releases.album_offset = 50;
    releases::apply(
        &mut job,
        releases::Outcome::Albums(Page {
            items: vec![Album {
                id: "new".into(),
                release_date: Some("2025-03-15".into()),
                ..Default::default()
            }],
            offset: 50,
            limit: 1,
            total: 51,
            next: None,
        }),
    )
    .unwrap();
    assert!(job.releases.pending_cache.is_none());
    assert_eq!(job.releases.albums.len(), 1);
}

#[tokio::test]
async fn cache_survives_restart_is_account_scoped_and_reapplies_the_requested_week() {
    let root = std::env::temp_dir().join(format!(
        "spotifast-search-cache-{:032x}",
        rand::random::<u128>()
    ));
    let mut original = job();
    releases::apply(
        &mut original,
        releases::Outcome::Search(Page::whole(vec![
            track("one", "followed", "march", "2025-03-15"),
            track("two", "followed", "april", "2025-04-05"),
        ])),
    )
    .unwrap();
    release_cache::flush(&root, &mut original).await;
    let mut later = job();
    later.period = Period {
        start: "04/2025".into(),
        end: String::new(),
    };
    let outcome = release_cache::read(&root, &later).await.unwrap();
    releases::apply(&mut later, outcome).unwrap();
    assert_eq!(later.releases.cache_hits, 1);
    assert_eq!(later.releases.albums.len(), 1);
    assert_eq!(later.releases.albums[0].id, "april");
    let mut other = job();
    other.account = "other".into();
    assert!(release_cache::read(&root, &other).await.is_none());
    let mut resumed = crash(&job());
    resumed.releases.search_offset = 10;
    assert!(release_cache::read(&root, &resumed).await.is_none());
    assert!(jobs::load(&root).await.unwrap().is_empty()); // Cache isn't a checkpoint.
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn corrupt_cache_is_ignored_without_failing_a_generation() {
    let root = std::env::temp_dir().join(format!(
        "spotifast-bad-cache-{:032x}",
        rand::random::<u128>()
    ));
    let mut original = job();
    releases::apply(
        &mut original,
        releases::Outcome::Search(Page::whole(Vec::new())),
    )
    .unwrap();
    release_cache::flush(&root, &mut original).await;
    let mut files = tokio::fs::read_dir(root.join("release-catalog-cache"))
        .await
        .unwrap();
    let path = files.next_entry().await.unwrap().unwrap().path();
    tokio::fs::write(path, b"interrupted cache").await.unwrap();
    assert!(release_cache::read(&root, &job()).await.is_none());
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn http_search_is_year_filtered_and_never_requests_the_whole_catalog_on_success() {
    let mut job = job();
    let body =
        serde_json::json!({"tracks":Page::whole(vec![track("one","followed","new","2025-03-15")])})
            .to_string();
    let (client, server) = super::tests::server(vec![(
        "GET /search?q=artist%3A%22Followed+Artist%22+year%3A2025&type=track&",
        "200 OK",
        body,
    )])
    .await;
    let outcome = releases::execute(&client, &job, &releases::Step::Search)
        .await
        .unwrap();
    releases::apply(&mut job, outcome).unwrap();
    assert_eq!(job.releases.albums.len(), 1);
    assert_eq!(job.releases.artist_index, 1);
    server.await.unwrap();
}

#[tokio::test]
async fn unsupported_search_disables_further_searches_but_rate_limits_do_not() {
    for status in ["403 Forbidden", "429 Too Many Requests\r\nRetry-After: 30"] {
        let mut job = job();
        let (client, server) =
            super::tests::server(vec![("GET /search?", status, "{}".into())]).await;
        let outcome = releases::execute(&client, &job, &releases::Step::Search).await;
        if status.starts_with("403") {
            releases::apply(&mut job, outcome.unwrap()).unwrap();
            assert!(!job.releases.optimized);
            assert!(job.releases.fallback);
        } else {
            assert!(outcome.is_err());
            assert!(job.releases.optimized);
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn full_catalog_cache_is_complete_deduplicated_and_restart_safe() {
    let root = std::env::temp_dir().join(format!(
        "spotifast-catalog-cache-{:032x}",
        rand::random::<u128>()
    ));
    let mut original = job();
    releases::apply(&mut original, releases::Outcome::Fallback).unwrap();
    let shared = Album {
        id: "shared".into(),
        release_date: Some("2025-03-15".into()),
        ..Default::default()
    };
    releases::apply(
        &mut original,
        releases::Outcome::Albums(Page {
            items: vec![shared.clone()],
            limit: 1,
            offset: 0,
            total: 3,
            next: Some("next".into()),
        }),
    )
    .unwrap();
    assert!(original.releases.pending_cache.is_none());
    let mut restored = crash(&original);
    releases::apply(
        &mut restored,
        releases::Outcome::Albums(Page {
            items: vec![
                shared,
                Album {
                    id: "undated".into(),
                    ..Default::default()
                },
            ],
            limit: 2,
            offset: 1,
            total: 3,
            next: None,
        }),
    )
    .unwrap();
    assert_eq!(restored.view().albums_checked, 2);
    assert_eq!(restored.view().matched_releases, 1);
    release_cache::flush(&root, &mut restored).await;
    let mut next = job();
    releases::apply(&mut next, releases::Outcome::Fallback).unwrap();
    let cached = release_cache::read(&root, &next).await.unwrap();
    releases::apply(&mut next, cached).unwrap();
    assert_eq!(next.view().albums_checked, 2);
    assert_eq!(next.releases.albums.len(), 1);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
