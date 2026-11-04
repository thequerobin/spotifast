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
