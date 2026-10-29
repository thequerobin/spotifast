//! Local, conservative consumption budget. Spotify does not publish remaining quota.
//! One manager spans every Web API grant and survives reauthorization and restarts.
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::ApiError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    High,
    Medium,
    Low,
}

impl Priority {
    fn index(self) -> usize {
        match self {
            Self::High => 0,
            Self::Medium => 1,
            Self::Low => 2,
        }
    }
}

/// These are internal defaults, never Spotify limits. A developer's other apps
/// and processes are outside this manager's visibility.
#[derive(Clone, Debug)]
pub struct Config {
    pub requests_per_window: usize,
    pub window: Duration,
    pub concurrency: usize,
    pub max_queued: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            requests_per_window: configured("SPOTIFAST_API_REQUESTS_PER_30S", 30, 1, 120),
            window: Duration::from_secs(30),
            concurrency: configured("SPOTIFAST_API_CONCURRENCY", 2, 1, 4),
            max_queued: 256,
        }
    }
}

fn configured(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| (min..=max).contains(value))
        .unwrap_or(default)
}

#[derive(Clone, Debug, Default)]
pub struct EndpointMetrics {
    pub requests: u64,
    pub rate_limits: u64,
    pub quota_limits: u64,
    pub unauthorized: u64,
    pub forbidden: u64,
    pub server_errors: u64,
    pub network_errors: u64,
    pub latency_ms: u64,
}
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub recent_requests: usize,
    pub internal_budget: usize,
    pub queued: [usize; 3],
    pub in_flight: usize,
    pub cooldown_seconds: u64,
    pub quota_blocked: bool,
    pub storage_error: Option<String>,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub endpoints: BTreeMap<String, EndpointMetrics>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Durable {
    version: u8,
    limit: usize,
    sent: VecDeque<u64>,
    cooldown_until: u64,
    quota_blocked: bool,
}
struct State {
    durable: Durable,
    queue: VecDeque<(u64, Priority)>,
    next_ticket: u64,
    turn: usize,
    in_flight: usize,
    next_send: u64,
    successes: usize,
    metrics: Snapshot,
    path: Option<PathBuf>,
}

pub struct CostManager {
    config: Config,
    state: Mutex<State>,
    disk: Arc<tokio::sync::Mutex<()>>,
    changed: tokio::sync::Notify,
    epoch: u64,
    started: Instant,
}

impl Default for CostManager {
    fn default() -> Self {
        Self::new(Config::default())
    }
}
impl CostManager {
    pub fn new(mut config: Config) -> Self {
        config.requests_per_window = config.requests_per_window.max(1);
        config.concurrency = config.concurrency.max(1);
        config.window = config.window.max(Duration::from_millis(1));
        Self {
            state: Mutex::new(State {
                durable: Durable {
                    version: 1,
                    limit: config.requests_per_window,
                    sent: VecDeque::new(),
                    cooldown_until: 0,
                    quota_blocked: false,
                },
                queue: VecDeque::new(),
                next_ticket: 0,
                turn: 0,
                in_flight: 0,
                next_send: 0,
                successes: 0,
                metrics: Snapshot::default(),
                path: None,
            }),
            config,
            disk: Arc::new(tokio::sync::Mutex::new(())),
            changed: tokio::sync::Notify::new(),
            epoch: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            started: Instant::now(),
        }
    }
    fn now(&self) -> u64 {
        self.epoch
            .saturating_add(self.started.elapsed().as_millis() as u64)
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn prune(&self, state: &mut State, now: u64) {
        let window = self.config.window.as_millis() as u64;
        state
            .durable
            .sent
            .retain(|time| time.saturating_add(window) > now);
    }
    /// Called on the backend before restoring grants or starting jobs.
    pub async fn restore(&self, path: PathBuf) -> Result<(), ApiError> {
        let result = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<Durable>(&bytes)
                .map(Some)
                .map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        };
        let mut state = self.lock();
        state.path = Some(path);
        match result {
            Ok(saved) if saved.as_ref().is_none_or(|s| s.version == 1 && s.limit > 0) => {
                if let Some(mut saved) = saved {
                    saved.limit = saved.limit.min(self.config.requests_per_window);
                    state.durable = saved;
                    self.prune(&mut state, self.now());
                    // A recent reservation may have been dispatched just before exit.
                    state.next_send = state
                        .durable
                        .sent
                        .back()
                        .copied()
                        .unwrap_or(0)
                        .saturating_add(
                            self.config.window.as_millis() as u64 / state.durable.limit as u64,
                        );
                }
                Ok(())
            }
            other => {
                state.metrics.storage_error = Some(format!(
                    "Cannot restore API budget: {}",
                    other
                        .err()
                        .unwrap_or_else(|| "unsupported state version".into())
                ));
                Err(ApiError::Network(
                    state.metrics.storage_error.clone().unwrap_or_default(),
                ))
            }
        }
    }
    async fn persist(self: &Arc<Self>) -> Result<(), ApiError> {
        let manager = self.clone();
        // Let a durability write finish even when its caller is cancelled.
        tokio::spawn(async move { manager.persist_inner().await })
            .await
            .map_err(|e| ApiError::Network(format!("API budget writer stopped: {e}")))?
    }
    async fn persist_inner(&self) -> Result<(), ApiError> {
        // Serialize writes and take the newest snapshot AFTER acquiring this lock.
        let disk = self.disk.clone().lock_owned().await;
        let (path, bytes) = {
            let state = self.lock();
            let Some(path) = &state.path else {
                return Ok(());
            };
            (
                path.clone(),
                serde_json::to_vec(&state.durable).map_err(|e| ApiError::Decode(e.to_string()))?,
            )
        };
        let result = tokio::task::spawn_blocking(move || {
            let _disk = disk;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            fastframe_file::write(&path, bytes)
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            let message =
                "Cannot save API budget; requests paused to preserve cooldowns".to_string();
            self.lock().metrics.storage_error = Some(message.clone());
            return Err(ApiError::Network(message));
        }
        Ok(())
    }
    fn selected(state: &State) -> Option<(usize, usize)> {
        // Weighted round robin, FIFO within each priority. Low always gets a turn.
        const TURNS: [Priority; 7] = [
            Priority::High,
            Priority::High,
            Priority::Medium,
            Priority::High,
            Priority::Medium,
            Priority::High,
            Priority::Low,
        ];
        (0..TURNS.len()).find_map(|offset| {
            let turn = (state.turn + offset) % TURNS.len();
            state
                .queue
                .iter()
                .position(|(_, priority)| *priority == TURNS[turn])
                .map(|index| (index, (turn + 1) % TURNS.len()))
        })
    }
    pub async fn acquire(self: &Arc<Self>, priority: Priority) -> Result<Permit, ApiError> {
        let ticket = {
            let mut state = self.lock();
            let capacity = match priority {
                Priority::High => self.config.max_queued,
                Priority::Medium => (self.config.max_queued * 7 / 8).max(1),
                Priority::Low => (self.config.max_queued * 3 / 4).max(1),
            };
            if state.queue.len() >= capacity {
                return Err(ApiError::Network(
                    "API request queue is full; try again later".into(),
                ));
            }
            state.next_ticket = state.next_ticket.wrapping_add(1);
            let ticket = state.next_ticket;
            state.queue.push_back((ticket, priority));
            ticket
        };
        let mut waiting = Waiting {
            manager: self.clone(),
            ticket,
            reserved: false,
        };
        self.changed.notify_waiters();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (dispatched, delay) = {
                let mut state = self.lock();
                if let Some(error) = &state.metrics.storage_error {
                    return Err(ApiError::Network(error.clone()));
                }
                if state.durable.quota_blocked {
                    return Err(ApiError::QuotaExhausted);
                }
                let now = self.now();
                self.prune(&mut state, now);
                if state.in_flight < self.config.concurrency
                    && let Some((index, turn)) = Self::selected(&state)
                    && state.queue[index].0 == ticket
                {
                    let window_ready = if state.durable.sent.len() >= state.durable.limit {
                        // If adaptation lowered the budget, wait until enough sends
                        // have expired, not just until the first timestamp expires.
                        state.durable.sent[state.durable.sent.len() - state.durable.limit]
                            .saturating_add(self.config.window.as_millis() as u64)
                    } else {
                        0
                    };
                    let ready = state
                        .next_send
                        .max(state.durable.cooldown_until)
                        .max(window_ready);
                    if now >= ready {
                        state.queue.remove(index);
                        state.turn = turn;
                        state.durable.sent.push_back(now);
                        state.next_send = now.saturating_add(
                            self.config.window.as_millis() as u64 / state.durable.limit as u64,
                        );
                        state.in_flight += 1;
                        waiting.reserved = true;
                        (true, None)
                    } else {
                        (false, Some(Duration::from_millis(ready - now)))
                    }
                } else {
                    (false, None)
                }
            };
            if dispatched {
                self.changed.notify_waiters();
                // Cancellation releases the slot but retains the reservation.
                self.persist().await?;
                let permit = Permit(self.clone());
                waiting.reserved = false;
                return Ok(permit);
            }
            if let Some(delay) = delay {
                tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = &mut changed => {} }
            } else {
                changed.await;
            }
        }
    }
    pub async fn throttle(self: &Arc<Self>, quota: bool, wait: Duration) -> Result<(), ApiError> {
        {
            let mut state = self.lock();
            state.durable.quota_blocked |= quota;
            state.durable.cooldown_until = state.durable.cooldown_until.max(
                self.now()
                    .saturating_add(wait.as_millis().min(u64::MAX as u128) as u64),
            );
            state.durable.limit = (state.durable.limit / 2).max(1);
            state.successes = 0;
        }
        self.changed.notify_waiters();
        self.persist().await
    }
    /// Explicit user action only. No invented reset time and no probe traffic.
    /// Retains rate-limit cooldowns and the reduced internal dispatch budget.
    pub async fn recheck_quota(self: &Arc<Self>) -> Result<(), ApiError> {
        self.lock().durable.quota_blocked = false;
        self.changed.notify_waiters();
        self.persist().await
    }
    pub fn record(&self, endpoint: String, status: Option<u16>, latency: Duration, quota: bool) {
        let mut state = self.lock();
        let metrics = state.metrics.endpoints.entry(endpoint).or_default();
        metrics.requests += 1;
        metrics.latency_ms = metrics
            .latency_ms
            .saturating_add(latency.as_millis() as u64);
        match status {
            Some(429) if quota => metrics.quota_limits += 1,
            Some(429) => metrics.rate_limits += 1,
            Some(401) => metrics.unauthorized += 1,
            Some(403) => metrics.forbidden += 1,
            Some(500..=599) => metrics.server_errors += 1,
            None => metrics.network_errors += 1,
            _ => {}
        }
        // Cautious recovery after sustained successful traffic, never above the
        // configured local ceiling. No bursts to discover Spotify's real limit.
        if status.is_some_and(|s| (200..300).contains(&s)) {
            state.successes += 1;
            if state.successes >= 120 {
                state.durable.limit =
                    (state.durable.limit + 1).min(self.config.requests_per_window);
                state.successes = 0;
            }
        }
    }
    pub fn cache_access(&self, hit: bool) {
        let mut state = self.lock();
        if hit {
            state.metrics.cache_hits += 1;
        } else {
            state.metrics.cache_misses += 1;
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut state = self.lock();
        self.prune(&mut state, self.now());
        let mut snapshot = state.metrics.clone();
        snapshot.recent_requests = state.durable.sent.len();
        snapshot.internal_budget = state.durable.limit;
        snapshot.in_flight = state.in_flight;
        snapshot.cooldown_seconds = state
            .durable
            .cooldown_until
            .saturating_sub(self.now())
            .div_ceil(1000);
        snapshot.quota_blocked = state.durable.quota_blocked;
        for (_, priority) in &state.queue {
            snapshot.queued[priority.index()] += 1;
        }
        snapshot
    }
}
struct Waiting {
    manager: Arc<CostManager>,
    ticket: u64,
    reserved: bool,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        let mut state = self.manager.lock();
        state.queue.retain(|(ticket, _)| *ticket != self.ticket);
        if self.reserved {
            state.in_flight -= 1;
        }
        drop(state);
        self.manager.changed.notify_waiters();
    }
}
pub struct Permit(Arc<CostManager>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.lock().in_flight -= 1;
        self.0.changed.notify_waiters();
    }
}

/// No search strings, account IDs, playlist IDs, or tokens in diagnostics.
pub fn endpoint(method: &reqwest::Method, path: &str) -> String {
    let path = path
        .strip_prefix("https://api.spotify.com/v1")
        .unwrap_or(path);
    let mut previous = "";
    let parts = path
        .split('?')
        .next()
        .unwrap_or_default()
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|part| {
            let id = matches!(
                previous,
                "artists" | "albums" | "tracks" | "playlists" | "shows" | "episodes" | "users"
            );
            previous = part;
            if id { "{id}" } else { part }
        })
        .collect::<Vec<_>>();
    format!("{method} /{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manager() -> Arc<CostManager> {
        Arc::new(CostManager::new(Config {
            requests_per_window: 3,
            window: Duration::from_secs(30),
            concurrency: 1,
            max_queued: 16,
        }))
    }
    async fn tick() {
        tokio::task::yield_now().await;
    }

    #[test]
    fn weighted_selection_reserves_a_turn_for_every_priority() {
        let manager = manager();
        let mut state = manager.lock();
        for ticket in 0..30 {
            state.queue.push_back((
                ticket,
                [Priority::High, Priority::Medium, Priority::Low][ticket as usize % 3],
            ));
        }
        let mut served = Vec::new();
        for _ in 0..7 {
            let (index, next) = CostManager::selected(&state).unwrap();
            served.push(state.queue.remove(index).unwrap().1);
            state.turn = next;
        }
        assert_eq!(
            served,
            [
                Priority::High,
                Priority::High,
                Priority::Medium,
                Priority::High,
                Priority::Medium,
                Priority::High,
                Priority::Low
            ]
        );
        assert_eq!(
            endpoint(
                &reqwest::Method::GET,
                "https://api.spotify.com/v1/users/private-name/playlists?token=hidden"
            ),
            "GET /users/{id}/playlists"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sliding_window_and_pacing_prevent_bursts() {
        let manager = manager();
        drop(manager.acquire(Priority::High).await.unwrap());
        let task = tokio::spawn({
            let m = manager.clone();
            async move { m.acquire(Priority::High).await }
        });
        tick().await;
        tokio::time::advance(Duration::from_secs(9)).await;
        tick().await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        tick().await;
        drop(task.await.unwrap().unwrap());
        drop(manager.acquire(Priority::High).await.unwrap());
        assert_eq!(manager.snapshot().recent_requests, 3);
        drop(manager.acquire(Priority::High).await.unwrap());
        assert!(manager.snapshot().recent_requests <= 3);
    }
    #[tokio::test(start_paused = true)]
    async fn priority_is_fair_and_cancellation_releases_capacity() {
        let manager = manager();
        let first = manager.acquire(Priority::High).await.unwrap();
        let mut tasks = Vec::new();
        // Enqueue low first; later interactive requests should overtake it,
        // but weighted turns must still serve low under sustained high load.
        for priority in [
            Priority::Low,
            Priority::High,
            Priority::Medium,
            Priority::High,
            Priority::High,
            Priority::High,
            Priority::High,
            Priority::High,
        ] {
            let m = manager.clone();
            tasks.push(tokio::spawn(async move {
                let permit = m.acquire(priority).await.unwrap();
                drop(permit);
                priority
            }));
            tick().await;
        }
        let cancelled = tasks.pop().unwrap();
        cancelled.abort();
        let _ = cancelled.await;
        assert_eq!(manager.snapshot().queued, [5, 1, 1]);
        drop(first);
        // The first high turn was spent before enqueuing, so the next turn
        // is high, then medium, with low guaranteed by the seventh slot.
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(manager.snapshot().queued, [0, 0, 0]);
        assert_eq!(manager.snapshot().in_flight, 0);
    }
    #[tokio::test(start_paused = true)]
    async fn server_wait_is_never_shortened_and_adaptation_has_a_ceiling() {
        let manager = manager();
        manager
            .throttle(false, Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(manager.snapshot().cooldown_seconds, 3600);
        assert_eq!(manager.snapshot().internal_budget, 1);
        manager
            .throttle(false, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(manager.snapshot().cooldown_seconds, 3600);
        let task = tokio::spawn({
            let m = manager.clone();
            async move { m.acquire(Priority::Low).await }
        });
        tick().await;
        tokio::time::advance(Duration::from_secs(3599)).await;
        tick().await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        tick().await;
        drop(task.await.unwrap().unwrap());
        for _ in 0..1000 {
            manager.record("GET /me".into(), Some(200), Duration::from_millis(5), false);
        }
        assert_eq!(manager.snapshot().internal_budget, 3);
    }
    #[tokio::test]
    async fn quota_and_confirmed_reservations_survive_restart() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("cost-test-{:x}", rand::random::<u64>()));
        let path = root.join("budget.json");
        let manager = manager();
        manager.restore(path.clone()).await.unwrap();
        drop(manager.acquire(Priority::Low).await.unwrap());
        manager
            .throttle(true, Duration::from_secs(3600))
            .await
            .unwrap();
        let restored = Arc::new(CostManager::default());
        restored.restore(path.clone()).await.unwrap();
        assert!(restored.snapshot().quota_blocked);
        assert_eq!(restored.snapshot().recent_requests, 1);
        assert!(restored.snapshot().cooldown_seconds >= 3599);
        assert!(matches!(
            restored.acquire(Priority::High).await,
            Err(ApiError::QuotaExhausted)
        ));
        restored.recheck_quota().await.unwrap();
        assert!(!restored.snapshot().quota_blocked);
        assert!(restored.snapshot().cooldown_seconds >= 3599);
        let bytes = tokio::fs::read(&path).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("token"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
    #[tokio::test]
    async fn corrupted_state_and_disk_failure_stop_dispatch() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("cost-error-{:x}", rand::random::<u64>()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let path = root.join("budget.json");
        tokio::fs::write(&path, b"invalid").await.unwrap();
        let corrupt = manager();
        assert!(corrupt.restore(path.clone()).await.is_err());
        assert!(corrupt.acquire(Priority::High).await.is_err());
        let unwritable = manager();
        let blocked = root.join("blocked.json");
        unwritable.restore(blocked.clone()).await.unwrap();
        tokio::fs::create_dir(&blocked).await.unwrap();
        assert!(unwritable.acquire(Priority::High).await.is_err());
        assert_eq!(unwritable.snapshot().in_flight, 0);
        assert!(unwritable.snapshot().storage_error.is_some());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn backpressure_and_quota_do_not_leak_tickets() {
        let manager = Arc::new(CostManager::new(Config {
            max_queued: 1,
            ..Config::default()
        }));
        let permit = manager.acquire(Priority::High).await.unwrap();
        let waiting = tokio::spawn({
            let m = manager.clone();
            async move { m.acquire(Priority::Low).await }
        });
        tick().await;
        assert!(manager.acquire(Priority::High).await.is_err());
        manager.throttle(true, Duration::ZERO).await.unwrap();
        assert!(matches!(
            waiting.await.unwrap(),
            Err(ApiError::QuotaExhausted)
        ));
        drop(permit);
        assert_eq!(manager.snapshot().queued, [0, 0, 0]);
    }
    #[tokio::test(start_paused = true)]
    async fn background_backpressure_reserves_interactive_queue_space() {
        let manager = Arc::new(CostManager::new(Config {
            concurrency: 1,
            max_queued: 4,
            ..Default::default()
        }));
        let permit = manager.acquire(Priority::High).await.unwrap();
        let mut queued = Vec::new();
        for _ in 0..3 {
            let m = manager.clone();
            queued.push(tokio::spawn(async move { m.acquire(Priority::Low).await }));
            tick().await;
        }
        assert!(manager.acquire(Priority::Low).await.is_err());
        let interactive = tokio::spawn({
            let m = manager.clone();
            async move { m.acquire(Priority::High).await }
        });
        tick().await;
        assert_eq!(manager.snapshot().queued, [1, 0, 3]);
        interactive.abort();
        let _ = interactive.await;
        for task in queued {
            task.abort();
            let _ = task.await;
        }
        drop(permit);
        assert_eq!(manager.snapshot().queued, [0, 0, 0]);
    }
}
