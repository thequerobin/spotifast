//! One durable actor owns the job files. A checkpoint is synced before every
//! mutation; an interrupted mutation is reconciled before another is issued.
use super::{Parameters, Period, eligible, kind};
use crate::api::models::{Page, Playlist, PlaylistItem, SavedAlbum, SavedTrack};
use crate::api::{ApiClient, ApiError, ApiGateway, Operation};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum State {
    Pending,
    Running,
    Paused,
    Retrying,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Stage {
    Albums,
    Tracks,
    Create,
    Add,
    Done,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub uri: String,
    pub key: String,
    pub added_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    version: u32,
    pub id: String,
    pub account: String,
    pub kind: String,
    pub period: Period,
    #[serde(default)]
    pub parameters: Parameters,
    pub name: String,
    pub public: bool,
    pub created_at: u64,
    pub state: State,
    pub stage: Stage,
    pub analyzed: u32,
    pub tracks_total: Option<u32>,
    pub albums_checked: u32,
    pub albums_total: Option<u32>,
    pub skipped: u32,
    pub playlist_id: Option<String>,
    pub error: Option<String>,
    pub elapsed: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub insertion_ms: u64,
    pub retries: u32,
    pub retry_at: u64,
    pub albums_offset: u32,
    pub tracks_offset: u32,
    pub albums: BTreeSet<String>,
    pub candidates: Vec<Candidate>,
    pub added: BTreeSet<String>,
    /// Persisted before sending. Never infer failure from a missing response.
    pub create_intent: bool,
    pub pending_batch: Vec<String>,
    pub batches_completed: u32,
}

#[derive(Clone, Debug)]
pub struct View {
    pub id: String,
    pub account: String,
    pub name: String,
    pub state: State,
    pub stage: Stage,
    pub analyzed: u32,
    pub tracks_total: Option<u32>,
    pub albums_checked: u32,
    pub selected: usize,
    pub added: usize,
    pub skipped: u32,
    pub playlist_id: Option<String>,
    pub error: Option<String>,
    pub elapsed: u64,
    pub progress: Option<f32>,
    pub eta: Option<u64>,
}

impl Job {
    pub fn new(
        account: String,
        kind_id: String,
        period: Period,
        name: String,
        public: bool,
    ) -> Result<Self, String> {
        let strategy = kind(&kind_id)?;
        period.bounds()?;
        if name.trim().is_empty() || name.chars().count() > 100 {
            return Err("Use a playlist name between 1 and 100 characters".into());
        }
        Ok(Self {
            version: 1,
            id: format!("{:032x}", rand::random::<u128>()),
            account,
            kind: kind_id,
            period,
            parameters: Parameters::new(),
            name,
            public,
            created_at: now(),
            state: State::Pending,
            stage: if strategy.needs_albums {
                Stage::Albums
            } else {
                Stage::Tracks
            },
            analyzed: 0,
            tracks_total: None,
            albums_checked: 0,
            albums_total: None,
            skipped: 0,
            playlist_id: None,
            error: None,
            elapsed: 0,
            elapsed_ms: 0,
            insertion_ms: 0,
            retries: 0,
            retry_at: 0,
            albums_offset: 0,
            tracks_offset: 0,
            albums: BTreeSet::new(),
            candidates: Vec::new(),
            added: BTreeSet::new(),
            create_intent: false,
            pending_batch: Vec::new(),
            batches_completed: 0,
        })
    }

    pub fn view(&self) -> View {
        // Overall work is measurable only after both libraries have been read.
        // Count real rows and insertions, without arbitrary stage weights.
        let progress = if self.state == State::Completed {
            Some(1.0)
        } else if matches!(self.stage, Stage::Create | Stage::Add) {
            let done = self.albums_checked as usize + self.analyzed as usize + self.added.len();
            let total =
                self.albums_checked as usize + self.analyzed as usize + self.candidates.len();
            (total > 0).then(|| done as f32 / total as f32)
        } else {
            None
        };
        let eta = if self.stage == Stage::Add && !self.added.is_empty() && self.insertion_ms >= 1000
        {
            Some(
                self.insertion_ms
                    .saturating_mul(self.candidates.len().saturating_sub(self.added.len()) as u64)
                    / self.added.len() as u64
                    / 1000,
            )
        } else {
            None
        };
        View {
            id: self.id.clone(),
            account: self.account.clone(),
            name: self.name.clone(),
            state: self.state,
            stage: self.stage,
            analyzed: self.analyzed,
            tracks_total: self.tracks_total,
            albums_checked: self.albums_checked,
            selected: self.candidates.len(),
            added: self.added.len(),
            skipped: self.skipped,
            playlist_id: self.playlist_id.clone(),
            error: self.error.clone(),
            elapsed: self.elapsed,
            progress,
            eta,
        }
    }

    fn marker(&self) -> String {
        format!("Spotifast generation {}", self.id)
    }
    pub fn runnable(&self) -> bool {
        matches!(
            self.state,
            State::Pending | State::Running | State::Retrying
        )
    }
}

#[derive(Clone, Debug)]
pub enum Request {
    Start {
        account: String,
        kind: String,
        period: Period,
        parameters: Parameters,
        name: String,
        public: bool,
    },
    Control {
        id: String,
        state: State,
    },
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub jobs: Vec<View>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Handle {
    sender: mpsc::UnboundedSender<Request>,
    snapshot: Arc<Mutex<Snapshot>>,
}

impl Handle {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<Request>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                sender,
                snapshot: Arc::default(),
            },
            receiver,
        )
    }
    pub fn send(&self, request: Request) {
        let _ = self.sender.send(request);
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    fn publish(&self, jobs: &[Job], error: Option<String>, waker: &crate::backend::Waker) {
        *self.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = Snapshot {
            jobs: jobs.iter().map(Job::view).collect(),
            error,
        };
        waker.wake();
    }
}

pub async fn save(root: &Path, job: &Job) -> Result<(), String> {
    let path = root.join(format!("{}.json", job.id));
    let bytes = serde_json::to_vec(job).map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || fastframe_file::write(&path, bytes))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

pub async fn load(root: &Path) -> Result<Vec<Job>, String> {
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|e| e.to_string())?;
    let mut entries = tokio::fs::read_dir(root).await.map_err(|e| e.to_string())?;
    let mut jobs = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        if entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        let bytes = tokio::fs::read(entry.path())
            .await
            .map_err(|e| e.to_string())?;
        let job: Job = serde_json::from_slice(&bytes)
            .map_err(|e| format!("Cannot read generation {}: {e}", entry.path().display()))?;
        if job.version != 1 || job.id.len() != 32 || !job.id.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("Unsupported playlist job file; preserve it before repairing".into());
        }
        jobs.push(job);
    }
    jobs.sort_by_key(|job| job.created_at);
    Ok(jobs)
}

#[derive(Clone, Debug)]
pub enum Step {
    Albums,
    Tracks,
    Create,
    RecoverCreate,
    Send(Vec<String>),
    Reconcile,
    Finish,
}

/// Changes here are saved before the returned operation can reach Spotify.
pub fn prepare(job: &mut Job) -> Step {
    match job.stage {
        Stage::Albums => Step::Albums,
        Stage::Tracks => Step::Tracks,
        Stage::Create if job.candidates.is_empty() => Step::Finish,
        Stage::Create if job.create_intent => Step::RecoverCreate,
        Stage::Create => {
            job.create_intent = true;
            Step::Create
        }
        Stage::Add if !job.pending_batch.is_empty() => Step::Reconcile,
        Stage::Add => {
            let batch: Vec<_> = job
                .candidates
                .iter()
                .filter(|c| !job.added.contains(&c.uri))
                .take(100)
                .map(|c| c.uri.clone())
                .collect();
            if batch.is_empty() {
                Step::Finish
            } else {
                job.pending_batch = batch.clone();
                Step::Send(batch)
            }
        }
        Stage::Done => Step::Finish,
    }
}

pub enum Outcome {
    Albums(Page<SavedAlbum>),
    Tracks(Page<SavedTrack>),
    Playlist(String),
    Added,
    Present(BTreeSet<String>),
    Finished,
}

fn invalid(message: &str) -> ApiError {
    ApiError::Decode(message.into())
}

pub(super) async fn execute(
    client: &ApiClient,
    job: &Job,
    step: &Step,
) -> Result<Outcome, ApiError> {
    match step {
        Step::Albums => Ok(Outcome::Albums(
            client.saved_albums(job.albums_offset, 50).await?,
        )),
        Step::Tracks => Ok(Outcome::Tracks(
            client.saved_tracks(job.tracks_offset, 50).await?,
        )),
        Step::Create => {
            let playlist = client
                .create_playlist(&job.name, job.public, &job.marker())
                .await?;
            if !super::valid_id(&playlist.id) {
                return Err(invalid("Playlist creation returned no identifier"));
            }
            Ok(Outcome::Playlist(playlist.id))
        }
        Step::RecoverCreate => {
            let mut offset = 0;
            let mut found = None;
            loop {
                let page = client.my_playlists(offset, 50).await?;
                let next = checked_next(&page, offset)?;
                for playlist in page.items {
                    if playlist.owner.id.as_deref() == Some(job.account.as_str())
                        && playlist.description.as_deref() == Some(job.marker().as_str())
                    {
                        if !super::valid_id(&playlist.id) {
                            return Err(invalid("Recovered playlist has no valid identifier"));
                        }
                        if found.is_some() {
                            return Err(invalid(
                                "More than one playlist has this job marker; inspect them before continuing",
                            ));
                        }
                        found = Some(playlist);
                    }
                }
                match next {
                    Some(next) => offset = next,
                    None => break,
                }
            }
            found.map(|playlist: Playlist| Outcome::Playlist(playlist.id)).ok_or_else(|| invalid("Creation result is unknown and its playlist is not visible yet. Check Spotify, then resume. No second playlist was created."))
        }
        Step::Send(batch) => {
            client
                .add_playlist_items(
                    job.playlist_id
                        .as_deref()
                        .ok_or_else(|| invalid("Missing playlist identifier"))?,
                    batch,
                    None,
                )
                .await?;
            Ok(Outcome::Added)
        }
        Step::Reconcile => {
            let id = job
                .playlist_id
                .as_deref()
                .ok_or_else(|| invalid("Missing playlist identifier"))?;
            let mut offset = 0;
            let mut present = BTreeSet::new();
            loop {
                let page: Page<PlaylistItem> = client.playlist_items(id, offset, 50).await?;
                let next = checked_next(&page, offset)?;
                present.extend(
                    page.items
                        .iter()
                        .filter_map(|item| item.playable())
                        .map(|item| item.uri().to_owned()),
                );
                match next {
                    Some(next) => offset = next,
                    None => break,
                }
            }
            Ok(Outcome::Present(present))
        }
        Step::Finish => Ok(Outcome::Finished),
    }
}

pub fn checked_next<T>(page: &Page<T>, offset: u32) -> Result<Option<u32>, ApiError> {
    if page.offset != offset || page.total < offset.saturating_add(page.items.len() as u32) {
        return Err(invalid("Spotify returned an unexpected page offset"));
    }
    let next = page.next_offset();
    if next.is_some_and(|next| next <= offset)
        || (next.is_some() && page.items.is_empty())
        || (next.is_none()
            && offset.saturating_add(page.limit.max(page.items.len() as u32)) < page.total)
    {
        return Err(invalid(
            "Spotify returned an incomplete library page; resume to retry this page",
        ));
    }
    Ok(next)
}

pub fn apply(job: &mut Job, outcome: Outcome) -> Result<(), ApiError> {
    match outcome {
        Outcome::Albums(page) => {
            let next = checked_next(&page, job.albums_offset)?;
            if job.albums_total.is_some_and(|total| total != page.total) {
                return Err(invalid(
                    "The album library changed during generation. Cancel this job and start a new one.",
                ));
            }
            if page.limit > 0
                && page.items.len() as u32 != page.total.saturating_sub(page.offset).min(page.limit)
            {
                return Err(invalid(
                    "Spotify returned an incomplete saved-album page; resume to retry it",
                ));
            }
            if page
                .items
                .iter()
                .any(|item| !super::valid_id(&item.album.id))
            {
                return Err(invalid(
                    "Spotify returned a saved album without an identifier",
                ));
            }
            job.albums_total = Some(page.total);
            job.albums_checked += page.items.len() as u32;
            job.albums
                .extend(page.items.into_iter().map(|item| item.album.id));
            if let Some(next) = next {
                job.albums_offset = next;
            } else {
                job.stage = Stage::Tracks;
            }
        }
        Outcome::Tracks(page) => {
            let next = checked_next(&page, job.tracks_offset)?;
            if job.tracks_total.is_some_and(|total| total != page.total) {
                return Err(invalid(
                    "Liked songs changed during generation. Cancel this job and start a new one.",
                ));
            }
            let strategy = kind(&job.kind).map_err(|e| invalid(&e))?;
            let mut seen: BTreeSet<_> = job.candidates.iter().map(|c| c.key.clone()).collect();
            let mut uris: BTreeSet<_> = job.candidates.iter().map(|c| c.uri.clone()).collect();
            job.tracks_total = Some(page.total);
            job.analyzed += page.items.len() as u32;
            for saved in page.items {
                let key = saved
                    .track
                    .recording_key()
                    .unwrap_or_else(|| saved.track.uri.clone());
                if eligible(&saved, strategy, &job.period, &job.albums, &job.parameters)
                    && !seen.contains(&key)
                    && !uris.contains(&saved.track.uri)
                {
                    seen.insert(key.clone());
                    uris.insert(saved.track.uri.clone());
                    job.candidates.push(Candidate {
                        uri: saved.track.uri,
                        key,
                        added_at: saved
                            .added_at
                            .and_then(|value| value.parse::<jiff::Timestamp>().ok())
                            .map(|stamp| stamp.to_string())
                            .unwrap_or_default(),
                    });
                } else {
                    job.skipped += 1;
                }
            }
            if let Some(next) = next {
                job.tracks_offset = next;
            } else {
                job.candidates.sort_by_cached_key(|candidate| {
                    std::cmp::Reverse(candidate.added_at.parse::<jiff::Timestamp>().ok())
                });
                job.stage = Stage::Create;
            }
        }
        Outcome::Playlist(id) => {
            job.playlist_id = Some(id);
            job.stage = Stage::Add;
        }
        Outcome::Added => {
            job.added.extend(std::mem::take(&mut job.pending_batch));
            job.batches_completed += 1;
        }
        Outcome::Present(present) => {
            // Absence is not proof the write failed: Spotify can be eventually
            // consistent. Quarantine uncertain writes rather than replay them.
            if !job.pending_batch.iter().all(|uri| present.contains(uri)) {
                return Err(invalid(
                    "An interrupted batch is not fully visible in Spotify. Check the playlist and resume after it becomes visible. The batch was not repeated.",
                ));
            }
            job.added.extend(std::mem::take(&mut job.pending_batch));
            job.batches_completed += 1;
        }
        Outcome::Finished => {
            job.stage = Stage::Done;
            job.state = State::Completed;
            if job.candidates.is_empty() {
                job.error = Some("No matching songs; no playlist was created".into());
            }
        }
    }
    Ok(())
}

pub fn failed_attempt(job: &mut Job, step: &Step, error: ApiError) {
    job.retries = job.retries.saturating_add(1);
    let throttled = matches!(
        &error,
        ApiError::Status { status: 429, .. } | ApiError::RateLimited
    );
    let temporary = throttled || error.is_network() || error.status().is_some_and(|s| s >= 500);
    if throttled {
        // A 429 explicitly rejected the mutation, so retrying is safe.
        if matches!(step, Step::Create) {
            job.create_intent = false;
        }
        if matches!(step, Step::Send(_)) {
            job.pending_batch.clear();
        }
    }
    let delay = if let ApiError::Status {
        status: 429,
        message,
    } = &error
    {
        message.parse::<u64>().unwrap_or(1)
    } else {
        (1_u64 << job.retries.min(8)) + rand::random_range(0..=3)
    };
    job.retry_at = now().saturating_add(delay);
    job.state = if temporary && (throttled || job.retries <= MAX_ATTEMPTS) {
        State::Retrying
    } else {
        State::Failed
    };
    job.error = Some(if throttled {
        format!("Spotify rate limit; retry in {delay}s")
    } else {
        error.to_string()
    });
    log::warn!(
        "Playlist job {} attempt={} state={:?} wait={}s: {}",
        job.id,
        job.retries,
        job.state,
        delay,
        job.error.as_deref().unwrap_or_default()
    );
}

pub const MAX_ATTEMPTS: u32 = 8;

pub fn control(job: &mut Job, state: State) {
    if matches!(job.state, State::Completed | State::Cancelled) {
        return;
    }
    if matches!(state, State::Paused | State::Pending | State::Cancelled) {
        job.state = state;
        job.retries = 0;
        job.retry_at = 0;
    }
}

/// Count wall time only while this process is running for the job's account.
/// Includes queueing and retry waits, but excludes paused/offline process time.
pub fn advance_time(jobs: &mut [Job], account: Option<&str>, milliseconds: u64) {
    for job in jobs {
        if job.runnable() && account == Some(job.account.as_str()) {
            job.elapsed_ms = job.elapsed_ms.saturating_add(milliseconds);
            job.elapsed = job.elapsed_ms / 1000;
        }
    }
}

pub fn next_index(jobs: &[Job], account: &str, cursor: usize) -> Option<usize> {
    (0..jobs.len())
        .map(|n| (cursor + n) % jobs.len())
        .find(|index| {
            let job = &jobs[*index];
            job.runnable() && job.retry_at <= now() && job.account == account
        })
}

/// Serialized steps bound generation concurrency to one, sharing the API's
/// request semaphore and cooldown with every other feature. Jobs rotate fairly.
pub async fn run(
    api: Arc<ApiGateway>,
    root: PathBuf,
    handle: Handle,
    mut requests: mpsc::UnboundedReceiver<Request>,
    waker: crate::backend::Waker,
) {
    let mut jobs = match load(&root).await {
        Ok(jobs) => jobs,
        Err(error) => {
            handle.publish(&[], Some(error), &waker);
            return;
        }
    };
    let mut last_tick = std::time::Instant::now();
    let mut pending = VecDeque::new();
    let mut cursor = 0;
    let mut cadence_ms = 0_u64;
    let mut next_request = std::time::Instant::now();
    handle.publish(&jobs, None, &waker);
    loop {
        let milliseconds = last_tick.elapsed().as_millis().min(u64::MAX as u128) as u64;
        last_tick = std::time::Instant::now();
        let account = api.account();
        advance_time(
            &mut jobs,
            account.as_ref().map(|account| account.as_str()),
            milliseconds,
        );
        if milliseconds >= 1000
            && jobs.iter().any(|job| {
                job.runnable()
                    && account
                        .as_ref()
                        .is_some_and(|account| account.as_str() == job.account)
            })
        {
            handle.publish(&jobs, None, &waker);
        }
        while let Some(request) = pending.pop_front().or_else(|| requests.try_recv().ok()) {
            let changed = match request {
                Request::Start {
                    account,
                    kind,
                    period,
                    parameters,
                    name,
                    public,
                } => {
                    if api.account().as_ref().map(|id| id.as_str()) != Some(account.as_str()) {
                        continue;
                    }
                    match Job::new(account, kind, period, name, public) {
                        Ok(mut job) => {
                            if let Err(error) =
                                super::kind(&job.kind).and_then(|kind| (kind.validate)(&parameters))
                            {
                                handle.publish(&jobs, Some(error), &waker);
                                continue;
                            }
                            job.parameters = parameters;
                            jobs.push(job);
                            Some(jobs.len() - 1)
                        }
                        Err(error) => {
                            handle.publish(&jobs, Some(error), &waker);
                            continue;
                        }
                    }
                }
                Request::Control { id, state } => {
                    let index = jobs.iter().position(|job| {
                        job.id == id
                            && api
                                .account()
                                .as_ref()
                                .is_some_and(|account| account.as_str() == job.account)
                    });
                    if let Some(index) = index {
                        control(&mut jobs[index], state);
                    }
                    index
                }
            };
            if let Some(index) = changed
                && let Err(error) = save(&root, &jobs[index]).await
            {
                handle.publish(
                    &jobs,
                    Some(format!("Cannot save generation: {error}")),
                    &waker,
                );
                return;
            }
            handle.publish(&jobs, None, &waker);
        }
        let account = api.account();
        let count = jobs.len();
        let index = account
            .as_ref()
            .and_then(|account| next_index(&jobs, account.as_str(), cursor));
        let Some(index) = index else {
            let wait = if jobs.iter().any(Job::runnable) {
                Duration::from_secs(1)
            } else {
                Duration::from_secs(60)
            };
            tokio::select! {
                request = requests.recv() => match request {
                    Some(request) => pending.push_back(request),
                    None => return,
                },
                _ = tokio::time::sleep(wait) => {},
            }
            continue;
        };
        if next_request > std::time::Instant::now() {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        cursor = (index + 1) % count;
        let job = &mut jobs[index];
        job.state = State::Running;
        job.error = None;
        let step = prepare(job);
        if let Err(error) = save(&root, job).await {
            handle.publish(
                &jobs,
                Some(format!("Cannot checkpoint generation: {error}")),
                &waker,
            );
            return;
        }
        handle.publish(&jobs, None, &waker);
        let started = std::time::Instant::now();
        let dispatched = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = {
            let job = &jobs[index];
            let work = async {
                let client = api.client_for(Operation::PlaylistCreation).await?;
                crate::api::client::generation_job_request(
                    execute(&client, job, &step),
                    dispatched.clone(),
                )
                .await
            };
            tokio::pin!(work);
            loop {
                tokio::select! {
                    result = &mut work => break Some(result),
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        if api.account() != account { break None; }
                        if let Ok(request) = requests.try_recv() {
                            let interrupt = matches!(&request, Request::Control { id, .. } if id == &job.id);
                            pending.push_back(request);
                            if interrupt { break None; }
                        }
                        let mut snapshot = handle.snapshot.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(view) = snapshot.jobs.iter_mut().find(|view| view.id == job.id) {
                            view.elapsed = job.elapsed + last_tick.elapsed().as_secs();
                        }
                        drop(snapshot);
                        waker.wake();
                    }
                }
            }
        };
        if !dispatched.load(std::sync::atomic::Ordering::SeqCst) && !matches!(&result, Some(Ok(_)))
        {
            clear_unsent_intent(&mut jobs[index], &step);
        }
        let throttled = matches!(
            &result,
            Some(Err(
                ApiError::Status { status: 429, .. } | ApiError::RateLimited
            ))
        );
        cadence_ms = if throttled {
            cadence_ms.max(250).saturating_mul(2).min(30_000)
        } else {
            cadence_ms * 9 / 10
        };
        next_request = std::time::Instant::now() + Duration::from_millis(cadence_ms);
        let elapsed = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let milliseconds = last_tick.elapsed().as_millis().min(u64::MAX as u128) as u64;
        last_tick = std::time::Instant::now();
        advance_time(
            &mut jobs,
            account.as_ref().map(|account| account.as_str()),
            milliseconds,
        );
        finish_step(&mut jobs[index], &step, result, elapsed);
        if let Err(error) = save(&root, &jobs[index]).await {
            handle.publish(
                &jobs,
                Some(format!("Cannot save generation: {error}")),
                &waker,
            );
            return;
        }
        handle.publish(&jobs, None, &waker);
    }
}

pub fn clear_unsent_intent(job: &mut Job, step: &Step) {
    if matches!(step, Step::Create) {
        job.create_intent = false;
    }
    if matches!(step, Step::Send(_)) {
        job.pending_batch.clear();
    }
}

fn finish_step(
    job: &mut Job,
    step: &Step,
    result: Option<Result<Outcome, ApiError>>,
    elapsed: u64,
) {
    if matches!(step, Step::Send(_) | Step::Reconcile) {
        job.insertion_ms = job.insertion_ms.saturating_add(elapsed);
    }
    if let Some(result) = result {
        match result.and_then(|outcome| apply(job, outcome)) {
            Ok(()) => {
                job.retries = 0;
                job.retry_at = 0;
            }
            Err(error) => failed_attempt(job, step, error),
        }
    }
}
