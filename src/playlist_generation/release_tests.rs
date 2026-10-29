use super::jobs::{self, Job, Stage, State};
use super::releases::{self, Outcome, Release};
use super::{Period, default_name};
use crate::api::models::{Album, Artist, ArtistRef, CursorPage, Cursors, Page, Track};
use std::collections::BTreeSet;

fn job() -> Job {
    Job::new(
        "listener".into(),
        "unliked-followed-releases".into(),
        Period {
            start: "03/2025".into(),
            end: String::new(),
        },
        "MNCAS - 03-2025".into(),
        false,
    )
    .unwrap()
}
fn crash(job: &Job) -> Job {
    serde_json::from_slice(&serde_json::to_vec(job).unwrap()).unwrap()
}
fn artist(id: &str) -> Artist {
    Artist {
        id: id.into(),
        ..Default::default()
    }
}
fn album(id: &str, date: &str, tracks: u32) -> Album {
    Album {
        id: id.into(),
        release_date: Some(date.into()),
        total_tracks: Some(tracks),
        ..Default::default()
    }
}
fn track(id: &str, artist: &str) -> Track {
    Track {
        id: Some(id.into()),
        uri: format!("spotify:track:{id}"),
        artists: vec![ArtistRef {
            id: Some(artist.into()),
            ..Default::default()
        }],
        ..Default::default()
    }
}
fn page<T>(items: Vec<T>, offset: u32, total: u32, next: bool) -> Page<T> {
    Page {
        limit: items.len() as u32,
        items,
        offset,
        total,
        next: next.then(|| "next".into()),
    }
}
fn tracks_job(count: u32) -> Job {
    let mut job = job();
    job.releases.artists.push("followed".into());
    job.releases.artist_ids.insert("followed".into());
    job.releases.albums.push_back(Release {
        id: "release".into(),
        date: "2025-03-15".into(),
        tracks: Some(count),
    });
    job.stage = Stage::ReleaseTracks;
    job
}

#[test]
fn release_calendar_precision_and_inclusive_intersections() {
    let period = Period {
        start: "15/03/2025".into(),
        end: "20/03/2025".into(),
    };
    for date in ["2025-03-15", "2025-03-20", "2025-03", "2025"] {
        assert!(period.overlaps_release(date), "{date}");
    }
    for date in [
        "2025-03-14",
        "2025-03-21",
        "2025-04",
        "2024",
        "2025-02-29",
        "2025-13",
        "bad",
        "2025-3-15",
    ] {
        assert!(!period.overlaps_release(date), "{date}");
    }
    assert!(Period::default().overlaps_release("2024-02-29"));
    assert!(!Period::default().overlaps_release("unknown"));
    assert_eq!(
        default_name("unliked-followed-releases", &Period::default()).unwrap(),
        "MNCAS - Todas"
    );
    assert_eq!(
        default_name("unliked-followed-releases", &period).unwrap(),
        "MNCAS - 15-03-2025 a 20-03-2025"
    );
}

#[test]
fn all_followed_artists_are_paginated_and_checkpointed_without_a_fifty_artist_cap() {
    let mut job = job();
    releases::apply(
        &mut job,
        Outcome::Artists(CursorPage {
            items: (0..50).map(|n| artist(&format!("artist{n}"))).collect(),
            total: Some(61),
            next: Some("next".into()),
            cursors: Some(Cursors {
                after: Some("artist49".into()),
                before: None,
            }),
        }),
    )
    .unwrap();
    let mut restored = crash(&job);
    assert_eq!(restored.releases.after.as_deref(), Some("artist49"));
    releases::apply(
        &mut restored,
        Outcome::Artists(CursorPage {
            items: (50..61).map(|n| artist(&format!("artist{n}"))).collect(),
            total: Some(61),
            ..Default::default()
        }),
    )
    .unwrap();
    assert_eq!(restored.stage, Stage::ArtistReleases);
    assert_eq!(restored.releases.artists.len(), 61);
    for _ in 0..61 {
        releases::apply(&mut restored, Outcome::Albums(Page::whole(Vec::new()))).unwrap();
    }
    assert_eq!(restored.releases.artist_index, 61);
    assert_eq!(restored.stage, Stage::Create);
    assert!(matches!(jobs::prepare(&mut restored), jobs::Step::Finish));
}

#[test]
fn missing_repeated_and_incomplete_artist_pages_fail_without_advancing() {
    let mut job = job();
    let page = CursorPage {
        items: vec![artist("a")],
        total: Some(2),
        next: Some("next".into()),
        ..Default::default()
    };
    assert!(releases::apply(&mut job, Outcome::Artists(page.clone())).is_err());
    assert!(job.releases.artists.is_empty());
    let mut valid = page;
    valid.cursors = Some(Cursors {
        after: Some("a".into()),
        before: None,
    });
    releases::apply(&mut job, Outcome::Artists(valid.clone())).unwrap();
    assert!(releases::apply(&mut job, Outcome::Artists(valid)).is_err());
    assert!(
        releases::apply(
            &mut job,
            Outcome::Artists(CursorPage {
                items: vec![],
                total: Some(2),
                ..Default::default()
            })
        )
        .is_err()
    );
    assert_eq!(job.releases.artists.len(), 1);
}

#[test]
fn release_catalog_offsets_and_global_album_dedup_survive_restart() {
    let mut job = job();
    releases::apply(
        &mut job,
        Outcome::Artists(CursorPage {
            items: vec![artist("a"), artist("b")],
            total: Some(2),
            ..Default::default()
        }),
    )
    .unwrap();
    releases::apply(
        &mut job,
        Outcome::Albums(page(vec![album("shared", "2025-03", 2)], 0, 2, true)),
    )
    .unwrap();
    let mut restored = crash(&job);
    assert_eq!(restored.releases.album_offset, 1);
    releases::apply(
        &mut restored,
        Outcome::Albums(page(vec![album("old", "2024-03-01", 5)], 1, 2, false)),
    )
    .unwrap();
    assert_eq!(restored.releases.artist_index, 1);
    releases::apply(
        &mut restored,
        Outcome::Albums(Page::whole(vec![
            album("shared", "2025-03", 2),
            album("new", "2025-03-31", 3),
        ])),
    )
    .unwrap();
    assert_eq!(restored.stage, Stage::ReleaseTracks);
    assert_eq!(restored.releases.albums.len(), 2);
    assert_eq!(restored.tracks_total, Some(5));
    assert_eq!(restored.albums_checked, 3);
}

#[test]
fn excludes_liked_unavailable_and_unrelated_compilation_tracks() {
    let mut job = tracks_job(5);
    let mut unavailable = track("unavailable", "followed");
    unavailable.is_playable = Some(false);
    let mut collaboration = track("collab", "other");
    collaboration.artists.push(ArtistRef {
        id: Some("followed".into()),
        ..Default::default()
    });
    releases::apply(
        &mut job,
        Outcome::Tracks {
            page: Page::whole(vec![
                track("liked", "followed"),
                track("unliked", "followed"),
                track("unrelated", "other"),
                unavailable,
                collaboration,
            ]),
            liked: BTreeSet::from(["spotify:track:liked".into()]),
        },
    )
    .unwrap();
    assert_eq!(job.candidates.len(), 2);
    assert_eq!(job.analyzed, 5);
    assert_eq!(job.skipped, 3);
    assert!(
        job.candidates
            .iter()
            .all(|candidate| candidate.added_at == "2025-03-15T00:00:00Z")
    );
    assert_eq!(job.stage, Stage::Create);
}

#[test]
fn album_track_offsets_and_selection_survive_restart() {
    let mut job = tracks_job(41);
    let first = (0..40)
        .map(|n| track(&format!("t{n}"), "followed"))
        .collect();
    releases::apply(
        &mut job,
        Outcome::Tracks {
            page: page(first, 0, 41, true),
            liked: BTreeSet::new(),
        },
    )
    .unwrap();
    let mut restored = crash(&job);
    assert_eq!(restored.releases.track_offset, 40);
    assert_eq!(restored.candidates.len(), 40);
    releases::apply(
        &mut restored,
        Outcome::Tracks {
            page: page(vec![track("last", "followed")], 40, 41, false),
            liked: BTreeSet::new(),
        },
    )
    .unwrap();
    assert_eq!(restored.candidates.len(), 41);
    assert_eq!(restored.analyzed, 41);
    assert!(restored.releases.albums.is_empty());
    assert_eq!(restored.stage, Stage::Create);
}

#[test]
fn duplicate_tracks_across_releases_are_not_selected_twice() {
    let mut job = tracks_job(1);
    job.releases.albums.push_back(Release {
        id: "second".into(),
        date: "2025-03-20".into(),
        tracks: Some(1),
    });
    for _ in 0..2 {
        releases::apply(
            &mut job,
            Outcome::Tracks {
                page: Page::whole(vec![track("same", "followed")]),
                liked: BTreeSet::new(),
            },
        )
        .unwrap();
    }
    assert_eq!(job.candidates.len(), 1);
    assert_eq!(job.skipped, 1);
}

#[test]
fn empty_following_and_legacy_mcans_checkpoints_remain_valid() {
    let mut job = job();
    releases::apply(
        &mut job,
        Outcome::Artists(CursorPage {
            total: Some(0),
            ..Default::default()
        }),
    )
    .unwrap();
    assert!(matches!(jobs::prepare(&mut job), jobs::Step::Finish));
    let mcans = Job::new(
        "listener".into(),
        "mcans".into(),
        Default::default(),
        "MCANS".into(),
        false,
    )
    .unwrap();
    let mut old = serde_json::to_value(mcans).unwrap();
    old.as_object_mut().unwrap().remove("releases");
    let restored: Job = serde_json::from_value(old).unwrap();
    assert_eq!(restored.stage, Stage::Albums);
    assert!(restored.releases.artists.is_empty());
}

#[tokio::test]
async fn http_membership_is_checked_in_batches_and_incomplete_replies_do_not_advance() {
    let mut job = tracks_job(2);
    let tracks = serde_json::to_string(&Page::whole(vec![
        track("liked", "followed"),
        track("new", "followed"),
    ]))
    .unwrap();
    let (client, server) = super::tests::server(vec![
        (
            "GET /albums/release/tracks?limit=40&offset=0",
            "200 OK",
            tracks,
        ),
        ("GET /me/library/contains?", "200 OK", "[true]".into()),
    ])
    .await;
    let result = releases::execute(&client, &job, &releases::Step::Tracks).await;
    assert!(result.is_err());
    assert_eq!(job.releases.track_offset, 0);
    assert!(job.candidates.is_empty());
    job = crash(&job);
    assert_eq!(job.stage, Stage::ReleaseTracks);
    server.await.unwrap();
}

#[tokio::test]
async fn http_rate_limit_keeps_the_discovery_checkpoint() {
    let mut job = job();
    job.stage = Stage::ArtistReleases;
    job.releases.artists.push("followed".into());
    job.releases.artist_ids.insert("followed".into());
    job.releases.album_offset = 10;
    let (client, server) = super::tests::server(vec![(
        "GET /artists/followed/albums?",
        "429 Too Many Requests\r\nRetry-After: 120",
        "{}".into(),
    )])
    .await;
    let step = jobs::prepare(&mut job);
    let error = crate::api::client::generation_request(jobs::execute(&client, &job, &step))
        .await
        .err()
        .unwrap();
    jobs::failed_attempt(&mut job, &step, error);
    assert_eq!(job.state, State::Retrying);
    assert!(job.retry_at >= jobs::now() + 119);
    assert_eq!(crash(&job).releases.album_offset, 10);
    server.await.unwrap();
}

#[tokio::test]
async fn runtime_discovers_followed_releases_checks_likes_and_creates_the_playlist() {
    let root = std::env::temp_dir().join(format!(
        "spotifast-release-runtime-{:032x}",
        rand::random::<u128>()
    ));
    jobs::load(&root).await.unwrap();
    let job = job();
    jobs::save(&root, &job).await.unwrap();
    let artists =
        serde_json::json!({"artists":{"items":[{"id":"followed"}],"total":1,"next":null}})
            .to_string();
    let albums = serde_json::to_string(&Page::whole(vec![
        album("old", "2024-03-01", 50),
        album("new", "2025-03-15", 3),
    ]))
    .unwrap();
    let tracks = serde_json::to_string(&Page::whole(vec![
        track("liked", "followed"),
        track("new", "followed"),
        track("other", "unfollowed"),
    ]))
    .unwrap();
    let (client, server) = super::tests::server(vec![
        ("GET /me/following?type=artist&limit=50", "200 OK", artists),
        ("GET /artists/followed/albums?", "200 OK", albums),
        ("GET /albums/new/tracks?limit=40&offset=0", "200 OK", tracks),
        ("GET /me/library/contains?", "200 OK", "[true,false]".into()),
        (
            "POST /me/playlists",
            "201 Created",
            "{\"id\":\"generated\"}".into(),
        ),
        (
            "POST /playlists/generated/items",
            "201 Created",
            "{\"snapshot_id\":\"saved\"}".into(),
        ),
    ])
    .await;
    let api = std::sync::Arc::new(crate::api::ApiGateway::generation_test(client, "listener"));
    let (handle, receiver) = jobs::Handle::channel();
    let actor = tokio::spawn(jobs::run(
        api,
        root.clone(),
        handle.clone(),
        receiver,
        crate::backend::Waker::default(),
    ));
    // This verifies durable completion, not a five-second throughput budget.
    // Allow the HTTP timeout when parallel Windows tests contend for resources.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if handle
                .snapshot()
                .jobs
                .iter()
                .any(|view| view.state == State::Completed)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "{error}: jobs={:?}, storage_error={:?}",
            handle.snapshot().jobs,
            handle.snapshot().error
        )
    });
    actor.abort();
    let _ = actor.await;
    server.await.unwrap();
    let restored = jobs::load(&root).await.unwrap();
    assert_eq!(restored[0].candidates.len(), 1);
    assert_eq!(restored[0].candidates[0].uri, "spotify:track:new");
    assert_eq!(restored[0].added.len(), 1);
    assert_eq!(restored[0].state, State::Completed);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
