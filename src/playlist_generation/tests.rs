use super::jobs::*;
use super::*;
use crate::api::{ApiError, models::*};
use std::collections::BTreeSet;

fn period(start: &str, end: &str) -> Period {
    Period {
        start: start.into(),
        end: end.into(),
    }
}
fn job() -> Job {
    Job::new(
        "listener".into(),
        "mcans".into(),
        Period::default(),
        "MCANS - Todas".into(),
        false,
    )
    .unwrap()
}
fn song(id: &str, album: &str, at: &str) -> SavedTrack {
    SavedTrack {
        added_at: Some(at.into()),
        track: Track {
            id: Some(id.into()),
            uri: format!("spotify:track:{id}"),
            album: Some(Album {
                id: album.into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}
fn candidate(n: usize) -> Candidate {
    Candidate {
        uri: format!("spotify:track:{n}"),
        key: n.to_string(),
        added_at: "2025-03-15T10:00:00Z".into(),
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
fn crash(job: &Job) -> Job {
    serde_json::from_slice(&serde_json::to_vec(job).unwrap()).unwrap()
}

#[test]
fn calendar_precision_and_inclusive_bounds() {
    for (input, low, high) in [
        ("2025", "2025-01-01", "2025-12-31"),
        ("03/2025", "2025-03-01", "2025-03-31"),
        ("15/03/2025", "2025-03-15", "2025-03-15"),
        ("02/2024", "2024-02-01", "2024-02-29"),
        ("02/2000", "2000-02-01", "2000-02-29"),
        ("02/1900", "1900-02-01", "1900-02-28"),
    ] {
        let p = period(input, "");
        assert_eq!(p.bounds().unwrap(), (Some(low.into()), Some(high.into())));
        assert!(p.contains(Some(&format!("{low}T00:00:00Z"))));
        assert!(p.contains(Some(&format!("{high}T23:59:59.999999999Z"))));
    }
}

#[test]
fn mixed_ranges_and_utc_offsets() {
    for p in [
        period("2024", "06/2025"),
        period("01/2024", "30/06/2025"),
        period("01/01/2024", "06/2025"),
    ] {
        assert!(p.contains(Some("2024-01-01T00:00:00Z")));
        assert!(p.contains(Some("2025-06-30T23:59:59Z")));
        assert!(!p.contains(Some("2023-12-31T23:59:59Z")));
        assert!(!p.contains(Some("2025-07-01T00:00:00Z")));
        assert!(p.contains(Some("2025-07-01T01:00:00+02:00")));
    }
    assert!(Period::default().contains(None));
    assert!(!period("2025", "").contains(None));
    assert!(!period("2025", "").contains(Some("2025-99-00")));
}

#[test]
fn invalid_dates_and_reversed_ranges() {
    for input in [
        "0000",
        "25",
        "20250",
        "3/2025",
        "13/2025",
        "00/2025",
        "31/04/2025",
        "29/02/2025",
        "29/02/1900",
        "01/00/2025",
        "00/01/2025",
        "2025/03",
        "x",
        "1/1/2025",
        "01/01/2025/5",
    ] {
        assert!(period(input, "").bounds().is_err(), "{input}");
    }
    assert!(period("2025", "2024").bounds().is_err());
    assert!(period("07/2025", "06/2025").bounds().is_err());
    assert!(period("2024", "06/2025").bounds().is_ok());
}

#[test]
fn default_names_preserve_precision() {
    for (start, end, expected) in [
        ("", "", "Todas"),
        ("2025", "", "2025"),
        ("03/2025", "", "03-2025"),
        ("15/03/2025", "", "15-03-2025"),
        ("2023", "2025", "2023 a 2025"),
        ("03/2025", "08/2025", "03-2025 a 08-2025"),
        ("15/03/2025", "20/08/2025", "15-03-2025 a 20-08-2025"),
    ] {
        assert_eq!(
            default_name("mcans", &period(start, end)).unwrap(),
            format!("MCANS - {expected}")
        );
    }
    assert!(default_name("unknown", &Period::default()).is_err());
}

#[test]
fn excludes_only_actually_saved_album_ids() {
    let albums = BTreeSet::from(["saved".to_owned()]);
    let strategy = kind("mcans").unwrap();
    assert!(!eligible(
        &song("one", "saved", "2025-01-01T00:00:00Z"),
        strategy,
        &Period::default(),
        &albums,
        &Parameters::new()
    ));
    assert!(eligible(
        &song("two", "unsaved", "2025-01-01T00:00:00Z"),
        strategy,
        &Period::default(),
        &albums,
        &Parameters::new()
    ));
    assert!(eligible(
        &song("three", "unsaved", "2025-01-02T00:00:00Z"),
        strategy,
        &Period::default(),
        &albums,
        &Parameters::new()
    ));
    assert!(!eligible(
        &song("two", "unsaved", "2024-01-01T00:00:00Z"),
        strategy,
        &period("2025", ""),
        &albums,
        &Parameters::new()
    ));
}

#[test]
fn unavailable_local_and_missing_identifiers_are_skipped() {
    let original = song("one", "album", "2025-01-01T00:00:00Z");
    for mutate in [
        |s: &mut SavedTrack| s.track.id = None,
        |s: &mut SavedTrack| s.track.uri = String::new(),
        |s: &mut SavedTrack| s.track.album = None,
        |s: &mut SavedTrack| {
            s.track.id = Some("bad/id".into());
            s.track.uri = "spotify:track:bad/id".into();
        },
        |s: &mut SavedTrack| s.track.album.as_mut().unwrap().id = "bad/id".into(),
        |s: &mut SavedTrack| s.track.is_local = true,
        |s: &mut SavedTrack| s.track.is_playable = Some(false),
    ] {
        let mut saved = original.clone();
        mutate(&mut saved);
        assert!(!eligible(
            &saved,
            kind("mcans").unwrap(),
            &Period::default(),
            &BTreeSet::new(),
            &Parameters::new()
        ));
    }
}

#[test]
fn interrupted_album_pagination_retains_membership_and_offset() {
    let mut job = job();
    apply(
        &mut job,
        Outcome::Albums(page(
            vec![SavedAlbum {
                album: Album {
                    id: "saved".into(),
                    ..Default::default()
                },
                ..Default::default()
            }],
            0,
            2,
            true,
        )),
    )
    .unwrap();
    let mut restored = crash(&job);
    assert_eq!(restored.albums_offset, 1);
    assert!(restored.albums.contains("saved"));
    assert!(matches!(prepare(&mut restored), Step::Albums));
    apply(
        &mut restored,
        Outcome::Albums(page(
            vec![SavedAlbum {
                album: Album {
                    id: "second".into(),
                    ..Default::default()
                },
                ..Default::default()
            }],
            1,
            2,
            false,
        )),
    )
    .unwrap();
    assert_eq!(restored.stage, Stage::Tracks);
    assert_eq!(restored.albums_checked, 2);
}

#[test]
fn interrupted_track_pagination_deduplicates_and_sorts_added_at() {
    let mut job = job();
    job.stage = Stage::Tracks;
    let old = song("old", "unsaved", "2025-01-01T00:00:00Z");
    apply(
        &mut job,
        Outcome::Tracks(page(vec![old.clone()], 0, 3, true)),
    )
    .unwrap();
    let mut restored = crash(&job);
    assert_eq!(restored.tracks_offset, 1);
    apply(
        &mut restored,
        Outcome::Tracks(page(
            vec![old, song("new", "unsaved", "2025-12-31T23:59:59Z")],
            1,
            3,
            false,
        )),
    )
    .unwrap();
    assert_eq!(restored.analyzed, 3);
    assert_eq!(restored.candidates.len(), 2);
    assert_eq!(restored.candidates[0].uri, "spotify:track:new");
    assert_eq!(restored.stage, Stage::Create);
}

#[test]
fn empty_libraries_do_not_create_a_remote_playlist() {
    let mut job = job();
    apply(&mut job, Outcome::Albums(Page::whole(vec![]))).unwrap();
    apply(&mut job, Outcome::Tracks(Page::whole(vec![]))).unwrap();
    assert!(matches!(prepare(&mut job), Step::Finish));
    apply(&mut job, Outcome::Finished).unwrap();
    assert_eq!(job.state, State::Completed);
    assert!(job.playlist_id.is_none());
    assert!(job.error.is_some());
}

#[test]
fn creation_intent_survives_unknown_result() {
    let mut job = job();
    job.stage = Stage::Create;
    job.candidates.push(candidate(1));
    assert!(matches!(prepare(&mut job), Step::Create));
    let mut restored = crash(&job);
    assert!(matches!(prepare(&mut restored), Step::RecoverCreate));
    apply(&mut restored, Outcome::Playlist("remote".into())).unwrap();
    assert_eq!(crash(&restored).playlist_id.as_deref(), Some("remote"));
    assert_eq!(restored.stage, Stage::Add);
}

#[test]
fn interrupted_insertion_reconciles_without_repeating_a_batch() {
    let mut job = job();
    job.stage = Stage::Add;
    job.playlist_id = Some("remote".into());
    job.candidates = (0..250).map(candidate).collect();
    let Step::Send(first) = prepare(&mut job) else {
        panic!("expected batch")
    };
    assert_eq!(first.len(), 100);
    let mut restored = crash(&job);
    assert!(matches!(prepare(&mut restored), Step::Reconcile));
    apply(&mut restored, Outcome::Present(first.into_iter().collect())).unwrap();
    let Step::Send(second) = prepare(&mut restored) else {
        panic!("expected next batch")
    };
    assert_eq!(second.len(), 100);
    assert!(second.iter().all(|uri| !restored.added.contains(uri)));
    apply(&mut restored, Outcome::Added).unwrap();
    let Step::Send(last) = prepare(&mut restored) else {
        panic!("expected last batch")
    };
    assert_eq!(last.len(), 50);
    apply(&mut restored, Outcome::Added).unwrap();
    assert!(matches!(prepare(&mut restored), Step::Finish));
    assert_eq!(restored.added.len(), 250);
}

#[test]
fn an_ambiguous_absent_batch_is_quarantined() {
    let mut job = job();
    job.stage = Stage::Add;
    job.candidates.push(candidate(1));
    let step = prepare(&mut job);
    let error = apply(&mut job, Outcome::Present(BTreeSet::new())).unwrap_err();
    failed_attempt(&mut job, &step, error);
    assert_eq!(job.state, State::Failed);
    assert_eq!(job.pending_batch.len(), 1);
    assert!(matches!(prepare(&mut crash(&job)), Step::Reconcile));
}

#[test]
fn rate_limit_persists_full_server_wait_and_safe_retry_intent() {
    let mut job = job();
    job.stage = Stage::Create;
    job.candidates.push(candidate(1));
    let step = prepare(&mut job);
    let before = now();
    failed_attempt(
        &mut job,
        &step,
        ApiError::Status {
            status: 429,
            message: "3600".into(),
        },
    );
    let mut restored = crash(&job);
    assert_eq!(restored.state, State::Retrying);
    assert!(restored.retry_at >= before + 3600);
    assert!(matches!(prepare(&mut restored), Step::Create));
    restored.stage = Stage::Add;
    let step = prepare(&mut restored);
    failed_attempt(
        &mut restored,
        &step,
        ApiError::Status {
            status: 429,
            message: "2".into(),
        },
    );
    assert!(restored.pending_batch.is_empty());
    assert!(matches!(prepare(&mut restored), Step::Send(_)));
}

#[test]
fn temporary_network_errors_back_off_then_require_intervention() {
    let mut job = job();
    for attempt in 1..=MAX_ATTEMPTS {
        failed_attempt(&mut job, &Step::Albums, ApiError::Network("offline".into()));
        assert_eq!(job.retries, attempt);
        assert_eq!(job.state, State::Retrying);
        assert!(job.retry_at > now());
    }
    failed_attempt(&mut job, &Step::Albums, ApiError::Network("offline".into()));
    assert_eq!(job.state, State::Failed);
}

#[test]
fn pagination_rejects_incomplete_or_repeated_pages() {
    let mut page = page(vec![1], 0, 3, true);
    assert_eq!(checked_next(&page, 0).unwrap(), Some(1));
    assert!(checked_next(&page, 1).is_err());
    page.next = None;
    assert!(checked_next(&page, 0).is_err());
    page.next = Some("next".into());
    page.items.clear();
    page.limit = 0;
    assert!(checked_next(&page, 0).is_err());
}

#[test]
fn large_library_checkpoints_remain_bounded_to_selected_rows() {
    let mut job = job();
    job.stage = Stage::Tracks;
    for offset in (0..10_000).step_by(50) {
        let songs = (offset..offset + 50)
            .map(|n| song(&n.to_string(), "unsaved", "2025-01-01T00:00:00Z"))
            .collect();
        apply(
            &mut job,
            Outcome::Tracks(page(songs, offset, 10_000, offset < 9950)),
        )
        .unwrap();
    }
    assert_eq!(job.analyzed, 10_000);
    assert_eq!(job.candidates.len(), 10_000);
    assert_eq!(crash(&job).candidates.len(), 10_000);
}

#[test]
fn progress_is_indeterminate_until_total_work_is_known() {
    let mut job = job();
    assert!(job.view().progress.is_none());
    job.stage = Stage::Tracks;
    job.analyzed = 100;
    job.tracks_total = Some(1000);
    assert!(job.view().progress.is_none());
    job.stage = Stage::Add;
    job.candidates = (0..100).map(candidate).collect();
    assert_eq!(job.view().progress, Some(0.5));
    job.added.extend((0..50).map(|n| candidate(n).uri));
    job.elapsed = 3600;
    job.insertion_ms = 1000;
    assert_eq!(
        job.view().eta,
        Some(1),
        "ETA uses insertion throughput, not scanning time"
    );
    job.state = State::Completed;
    assert_eq!(job.view().progress, Some(1.0));
}

#[tokio::test]
async fn disk_checkpoints_survive_restart_for_multiple_independent_jobs() {
    let root = std::env::temp_dir().join(format!(
        "spotifast-generation-test-{:032x}",
        rand::random::<u128>()
    ));
    assert!(load(&root).await.unwrap().is_empty());
    let mut first = job();
    first.albums_offset = 50;
    first.albums.insert("saved".into());
    let mut second = job();
    second.state = State::Paused;
    second.tracks_offset = 500;
    save(&root, &first).await.unwrap();
    save(&root, &second).await.unwrap();
    let restored = load(&root).await.unwrap();
    assert_eq!(restored.len(), 2);
    let a = restored.iter().find(|job| job.id == first.id).unwrap();
    assert_eq!(a.albums_offset, 50);
    assert!(a.albums.contains("saved"));
    let b = restored.iter().find(|job| job.id == second.id).unwrap();
    assert_eq!(b.state, State::Paused);
    assert_eq!(b.tracks_offset, 500);
    tokio::fs::write(root.join("broken.json"), b"interrupted")
        .await
        .unwrap();
    assert!(load(&root).await.is_err());
    tokio::fs::remove_dir_all(root).await.unwrap();
}

/// Real HTTP transport against a deterministic local Spotify stand-in.
async fn server(
    replies: Vec<(&'static str, &'static str, String)>,
) -> (crate::api::ApiClient, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client =
        crate::api::ApiClient::test_transport(format!("http://{}", listener.local_addr().unwrap()));
    let task = tokio::spawn(async move {
        for (expected, status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|n| n.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            assert!(
                String::from_utf8_lossy(&bytes).starts_with(expected),
                "{}",
                String::from_utf8_lossy(&bytes)
            );
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (client, task)
}

#[tokio::test]
async fn http_paginated_playlist_recovery_uses_the_task_marker() {
    let mut job = job();
    job.stage = Stage::Create;
    job.candidates.push(candidate(1));
    prepare(&mut job);
    let job = crash(&job);
    let first = serde_json::json!({"items": [{"id":"other", "description":"unrelated"}], "offset":0,"limit":1,"total":2,"next":"next"}).to_string();
    let second = serde_json::json!({"items": [{"id":"original", "description":format!("Spotifast generation {}", job.id),"owner":{"id":"listener"}}], "offset":1,"limit":1,"total":2,"next":null}).to_string();
    let (client, server) = server(vec![
        ("GET /me/playlists?limit=50&offset=0", "200 OK", first),
        ("GET /me/playlists?limit=50&offset=1", "200 OK", second),
    ])
    .await;
    let outcome = super::jobs::execute(&client, &job, &Step::RecoverCreate)
        .await
        .unwrap();
    let mut restored = job;
    apply(&mut restored, outcome).unwrap();
    assert_eq!(restored.playlist_id.as_deref(), Some("original"));
    server.await.unwrap();
}

#[tokio::test]
async fn http_interrupted_batch_is_reconciled_across_all_remote_pages() {
    let mut job = job();
    job.stage = Stage::Add;
    job.playlist_id = Some("remote".into());
    job.candidates = (0..2).map(candidate).collect();
    prepare(&mut job);
    let mut restored = crash(&job);
    let first = serde_json::json!({"items":[{"item":{"type":"track","id":"0","uri":"spotify:track:0"}}],"offset":0,"limit":1,"total":2,"next":"next"}).to_string();
    let second = serde_json::json!({"items":[{"track":{"type":"track","id":"1","uri":"spotify:track:1"}}],"offset":1,"limit":1,"total":2,"next":null}).to_string();
    let (client, server) = server(vec![
        (
            "GET /playlists/remote/items?limit=50&offset=0",
            "200 OK",
            first,
        ),
        (
            "GET /playlists/remote/items?limit=50&offset=1",
            "200 OK",
            second,
        ),
    ])
    .await;
    let outcome = super::jobs::execute(&client, &restored, &Step::Reconcile)
        .await
        .unwrap();
    apply(&mut restored, outcome).unwrap();
    assert_eq!(restored.added.len(), 2);
    assert!(matches!(prepare(&mut restored), Step::Finish));
    server.await.unwrap();
}

#[tokio::test]
async fn http_rate_limit_returns_immediately_for_durable_scheduling() {
    let (client, server) = server(vec![(
        "POST /me/playlists",
        "429 Too Many Requests\r\nRetry-After: 3600",
        "{}".into(),
    )])
    .await;
    let mut job = job();
    job.stage = Stage::Create;
    job.candidates.push(candidate(1));
    let step = prepare(&mut job);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        crate::api::client::generation_request(super::jobs::execute(&client, &job, &step)),
    )
    .await
    .unwrap();
    failed_attempt(&mut job, &step, result.err().unwrap());
    assert_eq!(job.state, State::Retrying);
    assert!(job.retry_at >= now() + 3599);
    assert!(client.cooling_down().await);
    server.await.unwrap();
}

#[tokio::test]
async fn http_network_disconnect_keeps_the_mutation_intent() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client =
        crate::api::ApiClient::test_transport(format!("http://{}", listener.local_addr().unwrap()));
    let task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        assert!(socket.read(&mut buffer).await.unwrap() > 0);
        // The remote side might have committed before the connection vanished.
    });
    let mut job = job();
    job.stage = Stage::Create;
    job.candidates.push(candidate(1));
    let step = prepare(&mut job);
    let error = super::jobs::execute(&client, &job, &step)
        .await
        .err()
        .unwrap();
    failed_attempt(&mut job, &step, error);
    assert_eq!(job.state, State::Retrying);
    assert!(matches!(prepare(&mut crash(&job)), Step::RecoverCreate));
    task.await.unwrap();
}

#[test]
fn pause_resume_cancel_and_account_isolated_round_robin() {
    let mut jobs = vec![job(), job(), job()];
    jobs[2].account = "another-account".into();
    advance_time(&mut jobs, Some("listener"), 1500);
    assert_eq!(jobs[0].elapsed, 1);
    assert_eq!(jobs[1].elapsed_ms, 1500);
    assert_eq!(jobs[2].elapsed_ms, 0);
    advance_time(&mut jobs, None, 10_000);
    assert_eq!(jobs[0].elapsed_ms, 1500);
    assert_eq!(next_index(&jobs, "listener", 0), Some(0));
    assert_eq!(next_index(&jobs, "listener", 1), Some(1));
    assert_eq!(next_index(&jobs, "listener", 2), Some(0));
    jobs[0].albums_offset = 50;
    jobs[0].albums.insert("saved".into());
    control(&mut jobs[0], State::Paused);
    advance_time(&mut jobs, Some("listener"), 2000);
    assert_eq!(jobs[0].elapsed_ms, 1500);
    assert_eq!(jobs[1].elapsed_ms, 3500);
    assert_eq!(next_index(&jobs, "listener", 0), Some(1));
    let mut restored = crash(&jobs[0]);
    assert_eq!(restored.state, State::Paused);
    control(&mut restored, State::Pending);
    assert_eq!(restored.albums_offset, 50);
    assert!(restored.albums.contains("saved"));
    control(&mut restored, State::Cancelled);
    control(&mut restored, State::Pending);
    assert_eq!(restored.state, State::Cancelled);
    assert!(!restored.runnable());
    jobs[1].state = State::Retrying;
    jobs[1].retry_at = now() + 3600;
    assert_eq!(next_index(&jobs, "listener", 0), None);
    assert_eq!(next_index(&jobs, "another-account", 0), Some(2));
    assert_eq!(next_index(&[], "listener", 0), None);
}

#[test]
fn strategy_declares_and_persists_its_own_parameters() {
    let strategy = Kind {
        id: "custom",
        label: "Custom",
        prefix: "Custom",
        needs_albums: false,
        fields: &[Field {
            key: "track",
            label: "Track ID",
            hint: "Spotify ID",
        }],
        validate: |parameters| {
            parameters
                .get("track")
                .filter(|value| !value.is_empty())
                .map(|_| ())
                .ok_or_else(|| "A track ID is required".into())
        },
        select: |saved, _, parameters| saved.track.id.as_ref() == parameters.get("track"),
    };
    let mut params = Parameters::new();
    assert!((strategy.validate)(&params).is_err());
    params.insert("track".into(), "selected".into());
    assert!((strategy.validate)(&params).is_ok());
    assert!(eligible(
        &song("selected", "album", "2025-01-01T00:00:00Z"),
        &strategy,
        &Period::default(),
        &BTreeSet::new(),
        &params
    ));
    assert!(!eligible(
        &song("other", "album", "2025-01-01T00:00:00Z"),
        &strategy,
        &Period::default(),
        &BTreeSet::new(),
        &params
    ));
    let mut job = job();
    job.parameters = params;
    assert_eq!(
        crash(&job).parameters.get("track").map(String::as_str),
        Some("selected")
    );
}

#[test]
fn pausing_before_network_dispatch_does_not_leave_an_ambiguous_mutation() {
    let mut job = job();
    job.stage = Stage::Create;
    job.candidates.push(candidate(1));
    let step = prepare(&mut job);
    clear_unsent_intent(&mut job, &step);
    control(&mut job, State::Paused);
    control(&mut job, State::Pending);
    assert!(matches!(prepare(&mut job), Step::Create));
    job.stage = Stage::Add;
    let step = prepare(&mut job);
    clear_unsent_intent(&mut job, &step);
    assert!(matches!(prepare(&mut job), Step::Send(_)));
}

#[tokio::test]
async fn runtime_automatically_resumes_a_persisted_job_and_finishes_it() {
    let root = std::env::temp_dir().join(format!(
        "spotifast-generation-runtime-{:032x}",
        rand::random::<u128>()
    ));
    load(&root).await.unwrap();
    let mut interrupted = job();
    interrupted.state = State::Running;
    interrupted.stage = Stage::Tracks;
    interrupted.tracks_offset = 50;
    interrupted.tracks_total = Some(51);
    interrupted.analyzed = 50;
    interrupted.candidates.push(candidate(0));
    save(&root, &interrupted).await.unwrap();
    let tracks = serde_json::json!({"items":[song("one", "unsaved", "2025-02-01T00:00:00Z")],"offset":50,"limit":50,"total":51,"next":null}).to_string();
    let (client, server) = server(vec![
        ("GET /me/tracks?limit=50&offset=50", "200 OK", tracks),
        (
            "POST /me/playlists",
            "201 Created",
            "{\"id\":\"resumed\"}".into(),
        ),
        (
            "POST /playlists/resumed/items",
            "201 Created",
            "{\"snapshot_id\":\"saved\"}".into(),
        ),
    ])
    .await;
    let api = std::sync::Arc::new(crate::api::ApiGateway::generation_test(client, "listener"));
    let (handle, receiver) = Handle::channel();
    let actor = tokio::spawn(run(
        api,
        root.clone(),
        handle.clone(),
        receiver,
        crate::backend::Waker::default(),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
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
    .unwrap();
    actor.abort();
    let _ = actor.await;
    server.await.unwrap();
    let restored = load(&root).await.unwrap();
    assert_eq!(restored[0].id, interrupted.id);
    assert_eq!(restored[0].analyzed, 51);
    assert_eq!(restored[0].added.len(), 2);
    assert_eq!(restored[0].playlist_id.as_deref(), Some("resumed"));
    assert_eq!(restored[0].state, State::Completed);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
