use super::jobs::{self, Job, Stage, State, Step};
use super::saved_albums::{self, Outcome};
use super::{Period, default_name};
use crate::api::models::{Album, Page, SavedAlbum, Track};

fn job() -> Job {
    let period = Period::default();
    Job::new(
        "listener".into(),
        "unliked-saved-albums".into(),
        period.clone(),
        default_name("unliked-saved-albums", &period).unwrap(),
        false,
    )
    .unwrap()
}

fn crash(job: &Job) -> Job {
    serde_json::from_slice(&serde_json::to_vec(job).unwrap()).unwrap()
}

fn album(id: &str, at: Option<&str>, tracks: u32) -> SavedAlbum {
    SavedAlbum {
        added_at: at.map(str::to_owned),
        album: Album {
            id: id.into(),
            total_tracks: Some(tracks),
            ..Default::default()
        },
    }
}

fn track(id: &str) -> Track {
    Track {
        id: Some(id.into()),
        uri: format!("spotify:track:{id}"),
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

fn embedded_album(id: &str, saved_at: &str, tracks: Vec<Track>) -> SavedAlbum {
    let mut saved = album(id, Some(saved_at), tracks.len() as u32);
    saved.album.tracks = Some(Page::whole(tracks));
    saved
}

#[tokio::test]
async fn embedded_tracks_stream_between_album_pages_and_survive_restart_without_catalog_calls() {
    let first = embedded_album(
        "first",
        "2025-03-02T00:00:00Z",
        vec![track("liked"), track("one")],
    );
    let second = embedded_album(
        "second",
        "2025-03-01T00:00:00Z",
        vec![track("one"), track("two")],
    );
    let (client, server) = super::tests::server(vec![
        (
            "GET /me/albums?limit=50&offset=0",
            "200 OK",
            serde_json::json!({"items":[{"added_at":first.added_at,"album":first.album}],"offset":0,"limit":1,"total":2,"next":"next"}).to_string(),
        ),
        ("GET /me/library/contains?", "200 OK", "[true,false]".into()),
        (
            "GET /me/albums?limit=50&offset=1",
            "200 OK",
            serde_json::json!({"items":[{"added_at":second.added_at,"album":second.album}],"offset":1,"limit":1,"total":2,"next":null}).to_string(),
        ),
        (
            "GET /me/library/contains?",
            "200 OK",
            "[false,false]".into(),
        ),
        (
            "POST /me/playlists",
            "201 Created",
            r#"{"id":"generated"}"#.into(),
        ),
        (
            "POST /playlists/generated/items",
            "201 Created",
            r#"{"snapshot_id":"saved"}"#.into(),
        ),
    ])
    .await;
    let mut job = job();
    for expected in [
        Stage::Albums,
        Stage::SavedAlbumTracks,
        Stage::Albums,
        Stage::SavedAlbumTracks,
        Stage::Create,
        Stage::Add,
        Stage::Add,
    ] {
        assert_eq!(job.stage, expected);
        let step = jobs::prepare(&mut job);
        job = crash(&job);
        let outcome = jobs::execute(&client, &job, &step).await.unwrap();
        jobs::apply(&mut job, outcome).unwrap();
        job = crash(&job);
    }
    server.await.unwrap();
    assert_eq!(job.state, State::Completed);
    assert_eq!(job.albums_checked, 2);
    assert_eq!(job.analyzed, 4);
    assert_eq!(job.tracks_total, Some(4));
    assert_eq!(job.added.len(), 2);
    assert_eq!(
        job.candidates
            .iter()
            .map(|c| c.uri.as_str())
            .collect::<Vec<_>>(),
        vec!["spotify:track:one", "spotify:track:two"]
    );
    let metrics = client.cost_manager().snapshot();
    assert!(!metrics.endpoints.contains_key("GET /albums/{id}/tracks"));
    assert_eq!(
        metrics.endpoints.values().map(|m| m.requests).sum::<u64>(),
        6
    );
}

#[tokio::test]
async fn embedded_first_page_continues_through_all_55_tracks_after_restart() {
    let mut saved = album("long", None, 55);
    saved.album.tracks = Some(page(
        (0..50).map(|n| track(&format!("t{n}"))).collect(),
        0,
        55,
        true,
    ));
    let mut job = job();
    jobs::apply(&mut job, jobs::Outcome::Albums(Page::whole(vec![saved]))).unwrap();
    assert_eq!(
        job.saved_album_tracks.albums[0]
            .prefetched
            .as_ref()
            .unwrap()
            .items
            .len(),
        40
    );
    let (client, server) = super::tests::server(vec![
        (
            "GET /me/library/contains?",
            "200 OK",
            serde_json::to_string(&vec![false; 40]).unwrap(),
        ),
        (
            "GET /albums/long/tracks?limit=40&offset=40",
            "200 OK",
            serde_json::to_string(&Page {
                limit: 40,
                ..page(
                    (40..55).map(|n| track(&format!("t{n}"))).collect(),
                    40,
                    55,
                    false,
                )
            })
            .unwrap(),
        ),
        (
            "GET /me/library/contains?",
            "200 OK",
            serde_json::to_string(&vec![false; 15]).unwrap(),
        ),
    ])
    .await;
    for offset in [0, 40] {
        job = crash(&job);
        assert_eq!(job.saved_album_tracks.track_offset, offset);
        let step = jobs::prepare(&mut job);
        let outcome = jobs::execute(&client, &job, &step).await.unwrap();
        jobs::apply(&mut job, outcome).unwrap();
        if offset == 0 {
            assert!(job.saved_album_tracks.albums[0].prefetched.is_none());
        }
    }
    server.await.unwrap();
    assert_eq!(job.stage, Stage::Create);
    assert_eq!(job.candidates.len(), 55);
    assert_eq!(job.analyzed, 55);
    assert_eq!(job.skipped, 0);
    assert_eq!(
        client.cost_manager().snapshot().endpoints["GET /albums/{id}/tracks"].requests,
        1
    );
}

#[test]
fn incomplete_embedded_pages_fall_back_and_prefetch_payloads_are_bounded() {
    let base = embedded_album(
        "test",
        "2025-01-01T00:00:00Z",
        vec![track("one"), track("two")],
    );
    for mutate in [
        |saved: &mut SavedAlbum| saved.album.tracks = None,
        |saved: &mut SavedAlbum| saved.album.tracks.as_mut().unwrap().offset = 1,
        |saved: &mut SavedAlbum| {
            saved.album.tracks.as_mut().unwrap().items.pop().unwrap();
        },
        |saved: &mut SavedAlbum| saved.album.tracks.as_mut().unwrap().next = Some("next".into()),
        |saved: &mut SavedAlbum| saved.album.total_tracks = Some(3),
        |saved: &mut SavedAlbum| {
            saved.album.tracks.as_mut().unwrap().items[0].name = "x".repeat(128 * 1024)
        },
        |saved: &mut SavedAlbum| {
            saved.album.total_tracks = Some(80);
            saved.album.tracks = Some(page(
                (0..20).map(|n| track(&format!("t{n}"))).collect(),
                0,
                80,
                true,
            ));
        },
    ] {
        let mut saved = base.clone();
        mutate(&mut saved);
        let mut job = job();
        jobs::apply(&mut job, jobs::Outcome::Albums(Page::whole(vec![saved]))).unwrap();
        assert!(job.saved_album_tracks.albums[0].prefetched.is_none());
    }
    let mut job = job();
    for index in 0..2 {
        let saved: Vec<_> = (0..50)
            .map(|n| {
                embedded_album(
                    &format!("a{index}b{n}"),
                    "2025-01-01T00:00:00Z",
                    vec![track(&format!("t{n}"))],
                )
            })
            .collect();
        jobs::apply(
            &mut job,
            jobs::Outcome::Albums(page(saved, index * 50, 100, index == 0)),
        )
        .unwrap();
    }
    let job = crash(&job);
    assert_eq!(
        job.saved_album_tracks
            .albums
            .iter()
            .filter(|a| a.prefetched.is_some())
            .count(),
        50
    );
}

#[tokio::test]
async fn embedded_20_tracks_do_not_truncate_an_80_track_album() {
    let mut saved = album("partial", None, 80);
    saved.album.tracks = Some(page(
        (0..20).map(|n| track(&format!("t{n}"))).collect(),
        0,
        80,
        true,
    ));
    let mut job = job();
    jobs::apply(&mut job, jobs::Outcome::Albums(Page::whole(vec![saved]))).unwrap();
    assert!(job.saved_album_tracks.albums[0].prefetched.is_none());
    let mut replies = Vec::new();
    for offset in [0, 40] {
        replies.push((
            if offset == 0 {
                "GET /albums/partial/tracks?limit=40&offset=0"
            } else {
                "GET /albums/partial/tracks?limit=40&offset=40"
            },
            "200 OK",
            serde_json::to_string(&page(
                (offset..offset + 40)
                    .map(|n| track(&format!("t{n}")))
                    .collect(),
                offset,
                80,
                offset == 0,
            ))
            .unwrap(),
        ));
        replies.push((
            "GET /me/library/contains?",
            "200 OK",
            serde_json::to_string(&vec![false; 40]).unwrap(),
        ));
    }
    let (client, server) = super::tests::server(replies).await;
    for offset in [0, 40] {
        job = crash(&job);
        assert_eq!(job.saved_album_tracks.track_offset, offset);
        let step = jobs::prepare(&mut job);
        let outcome = jobs::execute(&client, &job, &step).await.unwrap();
        jobs::apply(&mut job, outcome).unwrap();
    }
    server.await.unwrap();
    assert_eq!(job.stage, Stage::Create);
    assert_eq!(job.candidates.len(), 80);
    assert_eq!(job.analyzed, 80);
    assert_eq!(job.skipped, 0);
    assert_eq!(
        client.cost_manager().snapshot().endpoints["GET /albums/{id}/tracks"].requests,
        2
    );
}

#[tokio::test]
async fn expired_and_legacy_embedded_tracks_use_normal_pagination() {
    for timestamp in [
        None,
        Some(jobs::now() - 24 * 60 * 60),
        Some(jobs::now() + 60),
    ] {
        let mut job = job();
        let saved = embedded_album("saved", "2025-01-01T00:00:00Z", vec![track("one")]);
        jobs::apply(&mut job, jobs::Outcome::Albums(Page::whole(vec![saved]))).unwrap();
        match timestamp {
            Some(at) => job.saved_album_tracks.albums[0].prefetched_at = at,
            None => {
                let mut legacy = serde_json::to_value(&job).unwrap();
                let album = legacy["saved_album_tracks"]["albums"][0]
                    .as_object_mut()
                    .unwrap();
                album.remove("prefetched");
                album.remove("prefetched_at");
                job = serde_json::from_value(legacy).unwrap();
            }
        }
        let (client, server) = super::tests::server(vec![
            (
                "GET /albums/saved/tracks?limit=40&offset=0",
                "200 OK",
                serde_json::to_string(&Page::whole(vec![track("one")])).unwrap(),
            ),
            ("GET /me/library/contains?", "200 OK", "[false]".into()),
        ])
        .await;
        let step = jobs::prepare(&mut job);
        let outcome = jobs::execute(&client, &crash(&job), &step).await.unwrap();
        jobs::apply(&mut job, outcome).unwrap();
        server.await.unwrap();
        assert_eq!(job.candidates.len(), 1);
        assert_eq!(job.stage, Stage::Create);
    }
}

#[tokio::test]
async fn embedded_tracks_survive_a_quota_pause_without_repeating_the_catalog_read() {
    let mut job = job();
    let saved = embedded_album("saved", "2025-01-01T00:00:00Z", vec![track("one")]);
    jobs::apply(&mut job, jobs::Outcome::Albums(Page::whole(vec![saved]))).unwrap();
    let (client, server) = super::tests::server(vec![(
        "GET /me/library/contains?",
        "429 Too Many Requests\r\nRetry-After: 60",
        r#"{"error":{"status":429,"reason":"QUOTA_EXCEEDED"}}"#.into(),
    )])
    .await;
    let step = jobs::prepare(&mut job);
    let error = crate::api::client::generation_request(jobs::execute(&client, &job, &step))
        .await
        .err()
        .unwrap();
    jobs::failed_attempt(&mut job, &step, error);
    server.await.unwrap();
    let mut job = crash(&job);
    assert_eq!(job.state, State::PausedQuota);
    assert_eq!(job.saved_album_tracks.track_offset, 0);
    assert!(job.saved_album_tracks.albums[0].prefetched.is_some());
    assert!(job.candidates.is_empty());
    // A separate mock transport represents a later retry after quota recovery.
    let (client, server) = super::tests::server(vec![(
        "GET /me/library/contains?",
        "200 OK",
        "[false]".into(),
    )])
    .await;
    let outcome = jobs::execute(&client, &job, &step).await.unwrap();
    jobs::apply(&mut job, outcome).unwrap();
    server.await.unwrap();
    assert_eq!(job.candidates.len(), 1);
    assert!(job.saved_album_tracks.albums.is_empty());
}

#[test]
fn saved_album_period_uses_saved_time_in_utc_and_survives_paginated_restart() {
    let mut job = job();
    job.period.start = "03/2025".into();
    assert_eq!(
        default_name("unliked-saved-albums", &Period::default()).unwrap(),
        "USSA - All"
    );
    assert_eq!(job.stage, Stage::Albums);
    assert!(matches!(jobs::prepare(&mut job), Step::Albums));
    jobs::apply(
        &mut job,
        jobs::Outcome::Albums(page(
            vec![
                album("inside", Some("2025-04-01T01:00:00+02:00"), 2),
                album("outside", Some("2025-02-28T23:59:59Z"), 200),
            ],
            0,
            4,
            true,
        )),
    )
    .unwrap();
    let mut job = crash(&job);
    assert_eq!(job.albums_offset, 2);
    jobs::apply(
        &mut job,
        jobs::Outcome::Albums(page(
            vec![
                album("missingdate", None, 100),
                album("inside", Some("2025-03-31T23:00:00Z"), 2),
            ],
            2,
            4,
            false,
        )),
    )
    .unwrap();
    assert_eq!(job.albums_checked, 4);
    assert_eq!(job.saved_album_tracks.albums.len(), 1);
    assert_eq!(job.saved_album_tracks.albums[0].id, "inside");
    assert_eq!(job.tracks_total, Some(2));
    assert!(matches!(jobs::prepare(&mut job), Step::SavedAlbumTracks));
    let old_job = Job::new(
        "listener".into(),
        "mcans".into(),
        Period::default(),
        "MCANS".into(),
        false,
    )
    .unwrap();
    let mut legacy = serde_json::to_value(old_job).unwrap();
    legacy.as_object_mut().unwrap().remove("saved_album_tracks");
    let restored = serde_json::from_value::<Job>(legacy).unwrap();
    assert_eq!(restored.stage, Stage::Albums);
    assert!(restored.saved_album_tracks.albums.is_empty());
}

#[test]
fn release_filter_excludes_saved_albums_before_tracks_and_resumes_selection() {
    let mut job = job();
    job.period.start = "03/2025".into();
    job.set_start_parameters([(super::DATE_BASIS.into(), "release".into())].into())
        .unwrap();
    let mut released = album("released", Some("2026-01-01T00:00:00Z"), 2);
    released.album.release_date = Some("2025-03".into());
    let mut saved_recently = album("savedrecently", Some("2025-03-15T00:00:00Z"), 100);
    saved_recently.album.release_date = Some("1990-01-01".into());
    jobs::apply(
        &mut job,
        jobs::Outcome::Albums(page(vec![released, saved_recently], 0, 3, true)),
    )
    .unwrap();
    let mut job = crash(&job);
    jobs::apply(
        &mut job,
        jobs::Outcome::Albums(page(
            vec![album("unknown", Some("2025-03-01T00:00:00Z"), 100)],
            2,
            3,
            false,
        )),
    )
    .unwrap();
    assert_eq!(job.parameters[super::DATE_BASIS], "release");
    assert_eq!(job.saved_album_tracks.albums.len(), 1);
    assert_eq!(job.saved_album_tracks.albums[0].id, "released");
    assert_eq!(job.tracks_total, Some(2));
    saved_albums::apply(
        &mut job,
        Outcome {
            page: page(vec![track("liked")], 0, 2, true),
            liked: ["spotify:track:liked".into()].into(),
        },
    )
    .unwrap();
    let mut job = crash(&job);
    assert_eq!(job.saved_album_tracks.track_offset, 1);
    saved_albums::apply(
        &mut job,
        Outcome {
            page: page(vec![track("unliked")], 1, 2, false),
            liked: Default::default(),
        },
    )
    .unwrap();
    assert_eq!(job.stage, Stage::Create);
    assert_eq!(job.candidates.len(), 1);
    assert_eq!(job.candidates[0].uri, "spotify:track:unliked");
}

#[test]
fn selects_only_unliked_playable_tracks_deduplicates_and_resumes_each_album_page() {
    let mut job = job();
    jobs::apply(
        &mut job,
        jobs::Outcome::Albums(Page::whole(vec![
            album("first", None, 5),
            album("second", None, 2),
        ])),
    )
    .unwrap();
    let mut duplicate = track("new");
    duplicate.external_ids.isrc = Some("SAME".into());
    let mut local = track("local");
    local.is_local = true;
    saved_albums::apply(
        &mut job,
        Outcome {
            page: page(vec![track("liked"), duplicate.clone(), local], 0, 5, true),
            liked: ["spotify:track:liked".into()].into(),
        },
    )
    .unwrap();
    let mut job = crash(&job);
    assert_eq!(job.saved_album_tracks.track_offset, 3);
    assert_eq!(job.candidates.len(), 1);
    let mut unavailable = track("unavailable");
    unavailable.is_playable = Some(false);
    saved_albums::apply(
        &mut job,
        Outcome {
            page: page(vec![unavailable, Track::default()], 3, 5, false),
            liked: Default::default(),
        },
    )
    .unwrap();
    let mut job = crash(&job);
    assert_eq!(job.saved_album_tracks.albums[0].id, "second");
    assert_eq!(job.saved_album_tracks.track_offset, 0);
    duplicate.id = Some("edition".into());
    duplicate.uri = "spotify:track:edition".into();
    saved_albums::apply(
        &mut job,
        Outcome {
            page: Page::whole(vec![duplicate, track("another")]),
            liked: Default::default(),
        },
    )
    .unwrap();
    assert_eq!(job.stage, Stage::Create);
    assert_eq!(job.analyzed, 7);
    assert_eq!(job.skipped, 5);
    assert_eq!(
        job.candidates
            .iter()
            .map(|c| c.uri.as_str())
            .collect::<Vec<_>>(),
        vec!["spotify:track:new", "spotify:track:another"]
    );
    assert_eq!(job.tracks_total, Some(7));
}

#[test]
fn changed_album_or_broken_page_keeps_the_checkpoint_and_empty_selection_creates_nothing() {
    let mut job = job();
    jobs::apply(
        &mut job,
        jobs::Outcome::Albums(Page::whole(vec![album("one", None, 1)])),
    )
    .unwrap();
    let before = serde_json::to_value(&job).unwrap();
    for response in [
        page(vec![track("new")], 1, 2, false),
        Page::whole(vec![track("new"), track("other")]),
    ] {
        assert!(
            saved_albums::apply(
                &mut job,
                Outcome {
                    page: response,
                    liked: Default::default()
                }
            )
            .is_err()
        );
        assert_eq!(serde_json::to_value(&job).unwrap(), before);
    }
    saved_albums::apply(
        &mut job,
        Outcome {
            page: Page::whole(vec![track("liked")]),
            liked: ["spotify:track:liked".into()].into(),
        },
    )
    .unwrap();
    assert!(matches!(jobs::prepare(&mut job), Step::Finish));
    jobs::apply(&mut job, jobs::Outcome::Finished).unwrap();
    assert_eq!(job.state, State::Completed);
    assert!(job.playlist_id.is_none());
    let mut empty = self::job();
    jobs::apply(&mut empty, jobs::Outcome::Albums(Page::whole(vec![]))).unwrap();
    assert!(matches!(jobs::prepare(&mut empty), Step::Finish));
}

#[tokio::test]
async fn incomplete_likes_or_rate_limit_never_advances_the_track_page() {
    for (status, body) in [
        ("200 OK", "[]"),
        ("429 Too Many Requests\r\nRetry-After: 60", "{}"),
        ("500 Internal Server Error", "{}"),
    ] {
        let mut job = job();
        jobs::apply(
            &mut job,
            jobs::Outcome::Albums(Page::whole(vec![album("one", None, 1)])),
        )
        .unwrap();
        let (client, server) = super::tests::server(vec![
            (
                "GET /albums/one/tracks?limit=40&offset=0",
                "200 OK",
                serde_json::to_string(&Page::whole(vec![track("new")])).unwrap(),
            ),
            ("GET /me/library/contains?", status, body.into()),
        ])
        .await;
        let step = jobs::prepare(&mut job);
        let error = crate::api::client::generation_request(jobs::execute(&client, &job, &step))
            .await
            .err()
            .unwrap();
        jobs::failed_attempt(&mut job, &step, error);
        let restored = crash(&job);
        assert_eq!(restored.saved_album_tracks.track_offset, 0);
        assert_eq!(restored.saved_album_tracks.albums[0].id, "one");
        assert!(restored.candidates.is_empty());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn http_flow_reads_saved_albums_and_likes_then_creates_and_fills_playlist() {
    let mut job = job();
    let albums = serde_json::json!({"items":[{"added_at":"2025-03-01T00:00:00Z","album":{"id":"saved","total_tracks":2}}],"offset":0,"limit":50,"total":1,"next":null}).to_string();
    let (client, server) = super::tests::server(vec![
        ("GET /me/albums?limit=50&offset=0", "200 OK", albums),
        (
            "GET /albums/saved/tracks?limit=40&offset=0",
            "200 OK",
            serde_json::to_string(&Page::whole(vec![track("liked"), track("new")])).unwrap(),
        ),
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
    for _ in 0..5 {
        let step = jobs::prepare(&mut job);
        if let Step::Send(ref uris) = step {
            assert_eq!(uris, &["spotify:track:new"]);
        }
        job = crash(&job);
        let outcome = jobs::execute(&client, &job, &step).await.unwrap();
        jobs::apply(&mut job, outcome).unwrap();
        job = crash(&job);
    }
    assert_eq!(job.state, State::Completed);
    assert_eq!(job.added.len(), 1);
    assert_eq!(job.candidates[0].uri, "spotify:track:new");
    assert_eq!(job.candidates[0].added_at, "2025-03-01T00:00:00Z");
    server.await.unwrap();
}
