use crate::cache_persistence::{
	instant_to_unix, option_instant_to_unix, option_unix_to_instant, unix_to_instant, PersistedRefreshOrigin, PersistedRememberedSubredditEntry, PersistedSourceCacheEntry,
};
use crate::oauth::oauth_is_available;
use crate::user::collapse_duplicate_submissions;
use crate::utils::{Post, Preferences};
use hyper::body::to_bytes;
use hyper::{Body, Request, Response, StatusCode};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use tokio::time::{Duration, MissedTickBehavior};

pub(crate) const CACHE_TTL_SECS: u64 = 10_800;
pub(crate) const REMEMBERED_SUBREDDIT_TTL_SECS: u64 = 3 * 24 * 60 * 60;
pub(crate) const REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS: u64 = 5 * 60;
const BANNED_REFRESH_BACKOFF_SECS: u64 = 30 * 24 * 60 * 60;
const PERMANENT_FAILURE_BACKOFF_SECS: u64 = 14 * 24 * 60 * 60;
const ACTIVE_BROWSER_SESSION_TTL_SECS: u64 = 20;
const DEFAULT_ESTIMATED_POST_INTERVAL_SECS: u64 = 21_600;
const MIN_ESTIMATED_POST_INTERVAL_SECS: u64 = 60;
const MAX_ESTIMATED_POST_INTERVAL_SECS: u64 = 86_400;
const MIN_FRESHNESS_TARGET_SECS: u64 = 1_800;
const MAX_FRESHNESS_TARGET_SECS: u64 = 172_800;
const FETCH_CONCURRENCY: usize = 8;
const PER_SOURCE_LIMIT: usize = 40;
const INITIAL_SOURCE_WINDOW: usize = 32;
const PAGE_FILL_BATCH: usize = 4;
const MAX_SYNC_FILL_BATCHES: usize = 2;
const BACKGROUND_LOOKAHEAD_PAGES: usize = 2;
pub const POSTS_PER_PAGE: usize = 25;

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserSessionHeartbeatRequest {
	pub session_id: String,
	pub path: String,
	pub visible: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BrowserSessionReleaseRequest {
	pub session_id: String,
}

#[derive(Clone, Debug)]
struct ActiveBrowserSession {
	path: String,
	visible: bool,
	last_seen: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FeedSource {
	Subreddit { name: String },
	Flair { subreddit: String, flair: String },
}

impl FeedSource {
	pub fn display_name(&self) -> String {
		match self {
			Self::Subreddit { name } => name.clone(),
			Self::Flair { subreddit, flair } => format!("{subreddit}#{flair}"),
		}
	}

	fn cache_key(&self) -> String {
		match self {
			Self::Subreddit { name } => format!("subreddit:{}", name.to_lowercase()),
			Self::Flair { subreddit, flair } => format!("flair:{}:{}", subreddit.to_lowercase(), flair.to_lowercase()),
		}
	}

	fn build_path(&self) -> String {
		match self {
			Self::Subreddit { name } => {
				format!("/r/{}/new.json?limit={PER_SOURCE_LIMIT}&raw_json=1", name.replace('+', "%2B"))
			}
			Self::Flair { subreddit, .. } => {
				format!("/r/{}/new.json?limit={PER_SOURCE_LIMIT}&raw_json=1", subreddit.replace('+', "%2B"))
			}
		}
	}

	fn filter_posts(&self, mut posts: Vec<Post>) -> Vec<Post> {
		match self {
			Self::Subreddit { .. } => posts,
			Self::Flair { flair, .. } => {
				let flair = flair.trim().to_string();
				posts.retain(|post| post.flair.text.trim() == flair);
				posts
			}
		}
	}
}

#[derive(Clone)]
struct CachedSourcePosts {
	posts: Vec<Post>,
	after: String,
	fetched_at: Instant,
	refresh_origin: Option<RefreshOrigin>,
}

#[derive(Clone, Debug)]
pub struct RememberedSubredditEntry {
	pub name: String,
	pub last_requested_at: Instant,
	pub last_refresh_started_at: Option<Instant>,
	pub last_refresh_succeeded_at: Option<Instant>,
	pub last_refresh_failed_at: Option<Instant>,
	pub banned_until: Option<Instant>,
	pub consecutive_failures: u32,
	pub estimated_post_interval_secs: u64,
	pub refresh_in_flight: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshOrigin {
	Daemon,
	FrontPageSession,
	DirectLiveFetch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RememberedFailureDisposition {
	Transient,
	Permanent,
	Banned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionSourceState {
	Unstarted,
	Fetching,
	Ready,
	Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheServeMode {
	Normal,
	AllowStaleOnOauthOutage,
}

#[derive(Clone, Debug, Serialize)]
pub struct CacheStatusSummary {
	pub sources_total: usize,
	pub cached_sources: usize,
	pub daemon_managed_sources: usize,
	pub scheduled_sources: usize,
	pub in_flight_sources: usize,
	pub blocked_sources: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CacheStatusSource {
	pub source_key: String,
	pub display_name: String,
	pub source_type: String,
	pub daemon_managed: bool,
	pub cached: bool,
	pub cached_posts: usize,
	pub cached_age_secs: Option<u64>,
	pub head_post_created_ts: Option<u64>,
	pub head_post_age_secs: Option<u64>,
	pub cache_expires_in_secs: Option<u64>,
	pub last_refresh_origin: Option<String>,
	pub last_refresh_succeeded_secs_ago: Option<u64>,
	pub last_refresh_failed_secs_ago: Option<u64>,
	pub refresh_frequency_secs: Option<u64>,
	pub schedule_state: String,
	pub predicted_next_fetch_in_secs: Option<u64>,
	pub blocked_until_secs: Option<u64>,
	pub consecutive_failures: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct CacheStatusResponse {
	pub generated_at_unix: u64,
	pub summary: CacheStatusSummary,
	pub sources: Vec<CacheStatusSource>,
}

#[derive(Clone)]
struct SessionSourceEntry {
	source: FeedSource,
	state: SessionSourceState,
	last_error: Option<String>,
}

#[derive(Clone)]
struct ReadySourceQueue {
	source: FeedSource,
	posts: VecDeque<Post>,
	fetched_at: Instant,
}

#[derive(Clone, Eq, PartialEq)]
struct ReadyQueueHead {
	queue_index: usize,
	head_created_ts: u64,
	fetched_at: Instant,
	source_priority: (u8, String, String),
}

impl Ord for ReadyQueueHead {
	fn cmp(&self, other: &Self) -> Ordering {
		self
			.head_created_ts
			.cmp(&other.head_created_ts)
			.then_with(|| self.fetched_at.cmp(&other.fetched_at))
			.then_with(|| other.source_priority.cmp(&self.source_priority))
			.then_with(|| other.queue_index.cmp(&self.queue_index))
	}
}

impl PartialOrd for ReadyQueueHead {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

#[derive(Clone)]
pub struct FeedSessionRequest {
	pub sources: Vec<FeedSource>,
	pub filters: HashSet<String>,
	pub quarantine: bool,
	pub query_fingerprint: String,
}

#[derive(Clone)]
struct FeedSession {
	id: String,
	request: FeedSessionRequest,
	sources: Vec<SessionSourceEntry>,
	frozen_pages: HashMap<usize, Vec<Post>>,
	background_active: bool,
	background_complete: bool,
	requested_page: usize,
	last_updated: Instant,
	revision: u64,
}

#[derive(Clone)]
pub struct FeedPage {
	pub posts: Vec<Post>,
	pub has_prev: bool,
	pub has_next: bool,
}

#[derive(Clone, Serialize)]
pub struct FeedSessionStatus {
	pub session_id: String,
	pub total_sources: usize,
	pub ready_sources: usize,
	pub daemon_cached_sources: usize,
	pub failed_sources: usize,
	pub warming: bool,
	pub current_page_ready: bool,
	pub next_page_ready: bool,
	pub posts_merged: usize,
	pub status_message: String,
	pub refresh_token: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionWarmMode {
	InitialLoad,
	FollowupCachePreferred { max_sources: usize },
}

static SOURCE_CACHE: LazyLock<Mutex<HashMap<String, CachedSourcePosts>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static FEED_SESSIONS: LazyLock<Mutex<HashMap<String, FeedSession>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
pub static REMEMBERED_SUBREDDITS: LazyLock<Mutex<HashMap<String, RememberedSubredditEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static ACTIVE_BROWSER_SESSIONS: LazyLock<Mutex<HashMap<String, ActiveBrowserSession>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static REMEMBERED_SUBREDDIT_DAEMON_NEXT_TICK_AT: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

fn source_cache_is_expired(entry: &CachedSourcePosts) -> bool {
	entry.fetched_at.elapsed().as_secs() > CACHE_TTL_SECS
}

fn remembered_entry_is_active(entry: &RememberedSubredditEntry) -> bool {
	entry.last_requested_at.elapsed().as_secs() <= REMEMBERED_SUBREDDIT_TTL_SECS
}

fn normalize_plain_subreddit_name(subreddit: &str) -> Option<String> {
	let normalized = subreddit.trim().to_lowercase();
	(!normalized.is_empty()
		&& normalized != "popular"
		&& normalized != "all"
		&& normalized != "random"
		&& normalized != "randnsfw"
		&& !normalized.contains('+')
		&& !normalized.starts_with("u_"))
	.then_some(normalized)
}

fn is_plain_subreddit_source(source: &FeedSource) -> bool {
	matches!(source, FeedSource::Subreddit { name } if normalize_plain_subreddit_name(name).is_some())
}

fn new_remembered_subreddit_entry(name: String) -> RememberedSubredditEntry {
	RememberedSubredditEntry {
		name,
		last_requested_at: Instant::now(),
		last_refresh_started_at: None,
		last_refresh_succeeded_at: None,
		last_refresh_failed_at: None,
		banned_until: None,
		consecutive_failures: 0,
		estimated_post_interval_secs: DEFAULT_ESTIMATED_POST_INTERVAL_SECS,
		refresh_in_flight: false,
	}
}

fn prune_active_browser_sessions_locked(sessions: &mut HashMap<String, ActiveBrowserSession>) {
	let now = Instant::now();
	sessions.retain(|_, session| session.visible && now.duration_since(session.last_seen).as_secs() <= ACTIVE_BROWSER_SESSION_TTL_SECS);
}

fn active_browser_session_paths() -> Vec<String> {
	let Ok(mut sessions) = ACTIVE_BROWSER_SESSIONS.lock() else {
		return Vec::new();
	};
	prune_active_browser_sessions_locked(&mut sessions);
	sessions.values().map(|session| session.path.clone()).collect()
}

pub fn has_active_browser_sessions() -> bool {
	!active_browser_session_paths().is_empty()
}

pub fn remember_subreddit_request(subreddit: &str) {
	let Some(normalized) = normalize_plain_subreddit_name(subreddit) else {
		return;
	};

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let entry = remembered.entry(normalized.clone()).or_insert_with(|| new_remembered_subreddit_entry(normalized));
	entry.last_requested_at = Instant::now();
	crate::cache_persistence::mark_dirty();
}

pub fn remember_frontpage_subreddits(preferences: &Preferences) {
	for subreddit in &preferences.subscriptions {
		remember_subreddit_request(subreddit);
	}

	for subscription in &preferences.flair_subscriptions {
		remember_subreddit_request(&subscription.subreddit);
	}
}

pub fn is_subreddit_remembered(subreddit: &str) -> bool {
	let Some(normalized) = normalize_plain_subreddit_name(subreddit) else {
		return false;
	};

	REMEMBERED_SUBREDDITS
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.get(&normalized)
		.is_some_and(remembered_entry_is_active)
}

fn source_cache_is_servable_with_mode(source: &FeedSource, entry: &CachedSourcePosts, mode: CacheServeMode) -> bool {
	if matches!(mode, CacheServeMode::AllowStaleOnOauthOutage) && !oauth_is_available() {
		return true;
	}

	match source {
		FeedSource::Subreddit { name } if !name.starts_with("u_") => is_subreddit_remembered(name) || !source_cache_is_expired(entry),
		_ => !source_cache_is_expired(entry),
	}
}

fn source_cache_is_servable(source: &FeedSource, entry: &CachedSourcePosts) -> bool {
	source_cache_is_servable_with_mode(source, entry, CacheServeMode::Normal)
}

fn median_u64(values: &mut [u64]) -> u64 {
	values.sort_unstable();
	let mid = values.len() / 2;
	if values.len() % 2 == 0 {
		(values[mid - 1] + values[mid]) / 2
	} else {
		values[mid]
	}
}

fn estimate_post_interval_secs(posts: &[Post], prior: u64) -> u64 {
	let mut timestamps = posts.iter().map(|post| post.created_ts).collect::<Vec<_>>();
	timestamps.sort_unstable_by(|left, right| right.cmp(left));

	let mut deltas = timestamps
		.windows(2)
		.filter_map(|window| {
			let delta = window[0].saturating_sub(window[1]);
			(delta > 0).then_some(delta)
		})
		.collect::<Vec<_>>();

	if deltas.len() < 2 {
		return prior.clamp(MIN_ESTIMATED_POST_INTERVAL_SECS, MAX_ESTIMATED_POST_INTERVAL_SECS);
	}

	median_u64(&mut deltas).clamp(MIN_ESTIMATED_POST_INTERVAL_SECS, MAX_ESTIMATED_POST_INTERVAL_SECS)
}

fn freshness_target_secs(estimated_post_interval_secs: u64) -> u64 {
	estimated_post_interval_secs.saturating_mul(20).clamp(MIN_FRESHNESS_TARGET_SECS, MAX_FRESHNESS_TARGET_SECS)
}

fn effective_freshness_target_secs(entry: &RememberedSubredditEntry) -> u64 {
	let base = freshness_target_secs(entry.estimated_post_interval_secs);
	if entry.consecutive_failures >= 2 {
		base.saturating_mul(entry.consecutive_failures.max(1).min(8) as u64)
	} else {
		base
	}
}

fn remembered_entry_refresh_blocked(entry: &RememberedSubredditEntry) -> bool {
	entry.banned_until.is_some_and(|until| until > Instant::now())
}

fn refresh_ratio(entry: &RememberedSubredditEntry) -> (u128, u128) {
	let elapsed = entry.last_refresh_succeeded_at.map(|instant| instant.elapsed().as_secs() as u128).unwrap_or(u128::MAX);
	(elapsed, effective_freshness_target_secs(entry) as u128)
}

fn compare_refresh_ratio(left: &RememberedSubredditEntry, right: &RememberedSubredditEntry) -> Ordering {
	let (left_num, left_den) = refresh_ratio(left);
	let (right_num, right_den) = refresh_ratio(right);
	right_num.saturating_mul(left_den).cmp(&left_num.saturating_mul(right_den))
}

fn remembered_refresh_priority(left: &RememberedSubredditEntry, right: &RememberedSubredditEntry) -> Ordering {
	let left_never = left.last_refresh_succeeded_at.is_none();
	let right_never = right.last_refresh_succeeded_at.is_none();
	match right_never.cmp(&left_never) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	let left_overdue = left
		.last_refresh_succeeded_at
		.is_none_or(|instant| instant.elapsed().as_secs() >= effective_freshness_target_secs(left));
	let right_overdue = right
		.last_refresh_succeeded_at
		.is_none_or(|instant| instant.elapsed().as_secs() >= effective_freshness_target_secs(right));
	match right_overdue.cmp(&left_overdue) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	match compare_refresh_ratio(left, right) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	match right.last_requested_at.cmp(&left.last_requested_at) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	match (left.last_refresh_succeeded_at, right.last_refresh_succeeded_at) {
		(Some(left_success), Some(right_success)) => match left_success.cmp(&right_success) {
			Ordering::Equal => {}
			ordering => return ordering,
		},
		_ => {}
	}

	left.name.cmp(&right.name)
}

pub(crate) fn mark_plain_subreddit_refresh_started(subreddit: &str) {
	let Some(normalized) = normalize_plain_subreddit_name(subreddit) else {
		return;
	};

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let Some(entry) = remembered.get_mut(&normalized) else {
		return;
	};
	entry.last_refresh_started_at = Some(Instant::now());
	entry.refresh_in_flight = true;
	crate::cache_persistence::mark_dirty();
}

fn store_source_posts_with_origin(source: &FeedSource, posts: Vec<Post>, after: String, refresh_origin: Option<RefreshOrigin>) {
	if let Ok(mut cache) = SOURCE_CACHE.lock() {
		cache.insert(
			source.cache_key(),
			CachedSourcePosts {
				posts,
				after,
				fetched_at: Instant::now(),
				refresh_origin,
			},
		);
	}
	if is_plain_subreddit_source(source) {
		crate::cache_persistence::mark_dirty();
	}
}

pub fn publish_plain_subreddit_refresh(subreddit: &str, posts: Vec<Post>, after: String, refresh_origin: RefreshOrigin) {
	let Some(normalized) = normalize_plain_subreddit_name(subreddit) else {
		return;
	};

	crate::utils::store_post_flairs(&posts);
	store_source_posts_with_origin(&FeedSource::Subreddit { name: normalized.clone() }, posts.clone(), after.clone(), Some(refresh_origin));
	if matches!(refresh_origin, RefreshOrigin::Daemon | RefreshOrigin::FrontPageSession) {
		crate::video_preview::prewarm_listing_video_previews(&posts);
		spawn_author_link_prefetch(posts.clone());
	}

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let Some(entry) = remembered.get_mut(&normalized) else {
		return;
	};

	entry.last_refresh_succeeded_at = Some(Instant::now());
	entry.last_refresh_failed_at = None;
	entry.banned_until = None;
	entry.consecutive_failures = 0;
	entry.estimated_post_interval_secs = estimate_post_interval_secs(&posts, entry.estimated_post_interval_secs);
	entry.refresh_in_flight = false;
	info!(
		"post_cache: plain subreddit refresh succeeded for {} via {:?}: {} posts, after_present={}",
		entry.name,
		refresh_origin,
		posts.len(),
		!after.is_empty()
	);
	crate::cache_persistence::mark_dirty();
}

fn refresh_failure_disposition(error: &str) -> RememberedFailureDisposition {
	let normalized = error.trim().to_ascii_lowercase();
	if normalized == "banned" || normalized.contains("banned") {
		RememberedFailureDisposition::Banned
	} else if normalized == "private"
		|| normalized.contains("private")
		|| normalized == "gated"
		|| normalized.contains("gated")
		|| normalized.contains("404")
		|| normalized.contains("not found")
		|| normalized.contains("no posts found")
	{
		RememberedFailureDisposition::Permanent
	} else {
		RememberedFailureDisposition::Transient
	}
}

pub fn record_plain_subreddit_refresh_failure(subreddit: &str, error: &str, refresh_origin: RefreshOrigin) {
	let Some(normalized) = normalize_plain_subreddit_name(subreddit) else {
		return;
	};

	let now = Instant::now();
	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let Some(entry) = remembered.get_mut(&normalized) else {
		return;
	};

	entry.last_refresh_failed_at = Some(now);
	match refresh_failure_disposition(error) {
		RememberedFailureDisposition::Banned => {
			entry.banned_until = Some(now + Duration::from_secs(BANNED_REFRESH_BACKOFF_SECS));
		}
		RememberedFailureDisposition::Permanent => {
			entry.banned_until = Some(now + Duration::from_secs(PERMANENT_FAILURE_BACKOFF_SECS));
		}
		RememberedFailureDisposition::Transient => {}
	}
	entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
	entry.refresh_in_flight = false;
	warn!("post_cache: plain subreddit refresh failed for {} via {:?}: {}", entry.name, refresh_origin, error);
	crate::cache_persistence::mark_dirty();
}

pub fn cleanup_decayed_remembered_subreddits() {
	let decayed = {
		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		let decayed = remembered
			.iter()
			.filter(|(_, entry)| !remembered_entry_is_active(entry))
			.map(|(name, _)| name.clone())
			.collect::<Vec<_>>();
		for subreddit in &decayed {
			remembered.remove(subreddit);
		}
		decayed
	};

	if decayed.is_empty() {
		return;
	}

	let mut cache = SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
	for subreddit in &decayed {
		cache.remove(&FeedSource::Subreddit { name: subreddit.clone() }.cache_key());
		crate::subreddit::remove_subreddit_metadata_cache(subreddit);
	}
	crate::cache_persistence::mark_dirty();
}

pub(crate) fn store_source_posts_for_cache(source: &FeedSource, posts: Vec<Post>, after: String, refresh_origin: Option<RefreshOrigin>) {
	store_source_posts_with_origin(source, posts, after, refresh_origin);
}

pub(crate) fn snapshot_persisted_source_entries(now_unix: u64) -> Vec<PersistedSourceCacheEntry> {
	let Ok(cache) = SOURCE_CACHE.lock() else {
		return Vec::new();
	};

	cache
		.iter()
		.filter_map(|(key, entry)| {
			let subreddit = key.strip_prefix("subreddit:")?.to_string();
			normalize_plain_subreddit_name(&subreddit)?;
			Some(PersistedSourceCacheEntry {
				subreddit,
				posts: entry.posts.iter().map(crate::cache_persistence::PersistedPost::from_runtime).collect(),
				after: entry.after.clone(),
				fetched_at_unix: instant_to_unix(entry.fetched_at, now_unix),
				refresh_origin: entry.refresh_origin.map(PersistedRefreshOrigin::from_runtime),
			})
		})
		.collect()
}

pub(crate) fn snapshot_persisted_remembered_subreddits(now_unix: u64) -> Vec<PersistedRememberedSubredditEntry> {
	let Ok(remembered) = REMEMBERED_SUBREDDITS.lock() else {
		return Vec::new();
	};

	remembered
		.values()
		.filter(|entry| normalize_plain_subreddit_name(&entry.name).is_some())
		.map(|entry| PersistedRememberedSubredditEntry {
			name: entry.name.clone(),
			last_requested_at_unix: instant_to_unix(entry.last_requested_at, now_unix),
			last_refresh_started_at_unix: option_instant_to_unix(entry.last_refresh_started_at, now_unix),
			last_refresh_succeeded_at_unix: option_instant_to_unix(entry.last_refresh_succeeded_at, now_unix),
			last_refresh_failed_at_unix: option_instant_to_unix(entry.last_refresh_failed_at, now_unix),
			banned_until_unix: option_instant_to_unix(entry.banned_until, now_unix),
			consecutive_failures: entry.consecutive_failures,
			estimated_post_interval_secs: entry.estimated_post_interval_secs,
			refresh_in_flight: entry.refresh_in_flight,
		})
		.collect()
}

pub(crate) fn restore_persisted_state(
	source_entries: Vec<PersistedSourceCacheEntry>,
	remembered_entries: Vec<PersistedRememberedSubredditEntry>,
	_saved_at_unix: u64,
	now_unix: u64,
) {
	let restored_remembered = remembered_entries
		.into_iter()
		.filter_map(|entry| {
			let normalized = normalize_plain_subreddit_name(&entry.name)?;
			let age_secs = now_unix.saturating_sub(entry.last_requested_at_unix);
			(age_secs <= REMEMBERED_SUBREDDIT_TTL_SECS).then_some((
				normalized,
				RememberedSubredditEntry {
					name: entry.name,
					last_requested_at: unix_to_instant(entry.last_requested_at_unix, now_unix),
					last_refresh_started_at: option_unix_to_instant(entry.last_refresh_started_at_unix, now_unix),
					last_refresh_succeeded_at: option_unix_to_instant(entry.last_refresh_succeeded_at_unix, now_unix),
					last_refresh_failed_at: option_unix_to_instant(entry.last_refresh_failed_at_unix, now_unix),
					banned_until: option_unix_to_instant(entry.banned_until_unix, now_unix),
					consecutive_failures: entry.consecutive_failures,
					estimated_post_interval_secs: entry.estimated_post_interval_secs,
					refresh_in_flight: false,
				},
			))
		})
		.collect::<HashMap<_, _>>();

	if let Ok(mut remembered) = REMEMBERED_SUBREDDITS.lock() {
		remembered.extend(restored_remembered.clone());
	}

	let restored_sources = source_entries
		.into_iter()
		.filter_map(|entry| {
			let subreddit = normalize_plain_subreddit_name(&entry.subreddit)?;
			let age_secs = now_unix.saturating_sub(entry.fetched_at_unix);
			let remember_kept = restored_remembered.get(&subreddit).is_some();
			if age_secs > CACHE_TTL_SECS && !remember_kept {
				return None;
			}

			Some((
				FeedSource::Subreddit { name: subreddit }.cache_key(),
				CachedSourcePosts {
					posts: entry.posts.into_iter().map(crate::cache_persistence::PersistedPost::into_runtime).collect(),
					after: entry.after,
					fetched_at: unix_to_instant(entry.fetched_at_unix, now_unix),
					refresh_origin: entry.refresh_origin.map(PersistedRefreshOrigin::into_runtime),
				},
			))
		})
		.collect::<HashMap<_, _>>();

	if let Ok(mut cache) = SOURCE_CACHE.lock() {
		cache.extend(restored_sources);
	}
}

fn remembered_subreddit_daemon_counts() -> (usize, usize, usize, usize) {
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let total = remembered.len();
	let mut active = 0;
	let mut eligible = 0;
	let mut blocked_or_in_flight = 0;

	for entry in remembered.values() {
		if !remembered_entry_is_active(entry) {
			continue;
		}

		active += 1;
		if entry.refresh_in_flight || remembered_entry_refresh_blocked(entry) {
			blocked_or_in_flight += 1;
		} else {
			eligible += 1;
		}
	}

	(total, active, eligible, blocked_or_in_flight)
}

async fn parse_json_body<T: serde::de::DeserializeOwned>(req: Request<Body>) -> Result<T, String> {
	let body = to_bytes(req.into_body()).await.map_err(|err| err.to_string())?;
	serde_json::from_slice(&body).map_err(|err| err.to_string())
}

fn json_response<T: Serialize>(status: StatusCode, payload: &T) -> Result<Response<Body>, String> {
	let body = serde_json::to_vec(payload).map_err(|err| err.to_string())?;
	Response::builder()
		.status(status)
		.header("content-type", "application/json")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.map_err(|err| err.to_string())
}

pub async fn browser_session_heartbeat(req: Request<Body>) -> Result<Response<Body>, String> {
	let request: BrowserSessionHeartbeatRequest = parse_json_body(req).await?;
	if request.session_id.trim().is_empty() {
		return json_response(StatusCode::BAD_REQUEST, &serde_json::json!({ "ok": false, "message": "missing session_id" }));
	}

	if let Ok(mut sessions) = ACTIVE_BROWSER_SESSIONS.lock() {
		prune_active_browser_sessions_locked(&mut sessions);
		if request.visible {
			sessions.insert(
				request.session_id,
				ActiveBrowserSession {
					path: request.path,
					visible: true,
					last_seen: Instant::now(),
				},
			);
		} else {
			sessions.remove(request.session_id.trim());
		}
	}

	json_response(StatusCode::OK, &serde_json::json!({ "ok": true }))
}

pub async fn browser_session_release(req: Request<Body>) -> Result<Response<Body>, String> {
	let request: BrowserSessionReleaseRequest = parse_json_body(req).await?;
	if let Ok(mut sessions) = ACTIVE_BROWSER_SESSIONS.lock() {
		sessions.remove(request.session_id.trim());
	}
	json_response(StatusCode::OK, &serde_json::json!({ "ok": true }))
}

fn spawn_author_link_prefetch(posts: Vec<Post>) {
	if tokio::runtime::Handle::try_current().is_err() {
		return;
	}

	tokio::spawn(async move {
		crate::utils::prefetch_post_author_links(&posts).await;
	});
}

pub fn pick_next_remembered_subreddit_to_refresh() -> Option<String> {
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let mut entries = remembered
		.values()
		.filter(|entry| remembered_entry_is_active(entry) && !remembered_entry_refresh_blocked(entry))
		.cloned()
		.collect::<Vec<_>>();
	entries.sort_by(remembered_refresh_priority);
	entries.into_iter().next().map(|entry| entry.name)
}

async fn refresh_remembered_subreddit(subreddit: &str) {
	mark_plain_subreddit_refresh_started(subreddit);
	let listing_path = format!("/r/{}/new.json?limit={PER_SOURCE_LIMIT}&raw_json=1", subreddit.replace('+', "%2B"));
	let listing_result = Post::fetch(&listing_path, false).await;

	match listing_result {
		Ok((posts, after)) => {
			publish_plain_subreddit_refresh(subreddit, posts, after, RefreshOrigin::Daemon);
		}
		Err(error) => {
			record_plain_subreddit_refresh_failure(subreddit, &error, RefreshOrigin::Daemon);
		}
	}

	match crate::subreddit::refresh_subreddit_metadata(subreddit, false).await {
		Ok(metadata) => {
			info!(
				"post_cache: refreshed metadata for {} via {:?}: title={}, nsfw={}",
				subreddit,
				RefreshOrigin::Daemon,
				metadata.title,
				metadata.nsfw
			);
		}
		Err(error) => {
			warn!("post_cache: failed to refresh metadata for {} via {:?}: {}", subreddit, RefreshOrigin::Daemon, error);
		}
	}
}

pub fn spawn_remembered_subreddit_daemon() {
	tokio::spawn(async {
		let mut interval = tokio::time::interval(Duration::from_secs(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS));
		interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
		info!("post_cache: remembered subreddit daemon started (interval={}s)", REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS);
		if let Ok(mut next_tick) = REMEMBERED_SUBREDDIT_DAEMON_NEXT_TICK_AT.lock() {
			*next_tick = Some(Instant::now() + Duration::from_secs(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS));
		}
		interval.tick().await;

		loop {
			interval.tick().await;
			if let Ok(mut next_tick) = REMEMBERED_SUBREDDIT_DAEMON_NEXT_TICK_AT.lock() {
				*next_tick = Some(Instant::now() + Duration::from_secs(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS));
			}
			cleanup_decayed_remembered_subreddits();
			let (total, active, eligible, blocked_or_in_flight) = remembered_subreddit_daemon_counts();
			info!(
				"post_cache: remembered subreddit daemon tick (total={}, active={}, eligible={}, blocked_or_in_flight={})",
				total, active, eligible, blocked_or_in_flight
			);

			let Some(subreddit) = pick_next_remembered_subreddit_to_refresh() else {
				info!("post_cache: remembered subreddit daemon idle: no eligible subreddit to refresh");
				continue;
			};

			let should_skip = {
				let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
				remembered.get(&subreddit).is_none_or(|entry| !remembered_entry_is_active(entry) || entry.refresh_in_flight)
			};
			if should_skip {
				info!("post_cache: remembered subreddit daemon skipped refresh for {} because it is no longer eligible", subreddit);
				continue;
			}

			info!("post_cache: remembered subreddit daemon refreshing {}", subreddit);
			refresh_remembered_subreddit(&subreddit).await;
		}
	});
}

fn session_is_expired(entry: &FeedSession) -> bool {
	entry.last_updated.elapsed().as_secs() > CACHE_TTL_SECS
}

fn source_priority(source: &FeedSource) -> (u8, String, String) {
	match source {
		FeedSource::Subreddit { name } if name.starts_with("u_") => (0, name.to_lowercase(), String::new()),
		FeedSource::Subreddit { name } => (1, name.to_lowercase(), String::new()),
		FeedSource::Flair { subreddit, flair } => (2, subreddit.to_lowercase(), flair.to_lowercase()),
	}
}

pub fn sort_sources(sources: &mut [FeedSource]) {
	sources.sort_by_key(source_priority);
}

pub const fn initial_source_window() -> usize {
	INITIAL_SOURCE_WINDOW
}

pub fn get_cached_source_page_with_mode(source: &FeedSource, page: usize, per_page: usize, mode: CacheServeMode) -> Option<(Vec<Post>, String, bool, bool)> {
	let cache = SOURCE_CACHE.lock().ok()?;
	let entry = cache.get(&source.cache_key())?;
	if !source_cache_is_servable_with_mode(source, entry, mode) {
		return None;
	}

	let start = page * per_page;
	if start >= entry.posts.len() {
		return None;
	}
	let end = (start + per_page).min(entry.posts.len());
	let has_next = end < entry.posts.len() || !entry.after.is_empty();
	let has_prev = page > 0;
	let after = if end >= entry.posts.len() { entry.after.clone() } else { String::new() };
	Some((entry.posts[start..end].to_vec(), after, has_prev, has_next))
}

pub fn get_cached_source_page(source: &FeedSource, page: usize, per_page: usize) -> Option<(Vec<Post>, String, bool, bool)> {
	get_cached_source_page_with_mode(source, page, per_page, CacheServeMode::Normal)
}

pub fn get_cached_subreddit_page_with_mode(subreddit: &str, page: usize, per_page: usize, mode: CacheServeMode) -> Option<(Vec<Post>, String, bool, bool)> {
	get_cached_source_page_with_mode(&FeedSource::Subreddit { name: subreddit.to_string() }, page, per_page, mode)
}

pub fn get_cached_subreddit_page(subreddit: &str, page: usize, per_page: usize) -> Option<(Vec<Post>, String, bool, bool)> {
	get_cached_subreddit_page_with_mode(subreddit, page, per_page, CacheServeMode::Normal)
}

fn store_source_posts(source: &FeedSource, posts: Vec<Post>, after: String) {
	store_source_posts_with_origin(source, posts, after, None);
}

fn cached_source_origin_with_mode(source: &FeedSource, mode: CacheServeMode) -> Option<RefreshOrigin> {
	let cache = SOURCE_CACHE.lock().ok()?;
	let entry = cache.get(&source.cache_key())?;
	if !source_cache_is_servable_with_mode(source, entry, mode) {
		return None;
	}

	entry.refresh_origin
}

fn cached_source_origin(source: &FeedSource) -> Option<RefreshOrigin> {
	cached_source_origin_with_mode(source, CacheServeMode::Normal)
}

fn cached_source_posts_with_mode(source: &FeedSource, mode: CacheServeMode) -> Option<CachedSourcePosts> {
	let cache = SOURCE_CACHE.lock().ok()?;
	let entry = cache.get(&source.cache_key())?;
	if !source_cache_is_servable_with_mode(source, entry, mode) {
		return None;
	}

	Some(entry.clone())
}

fn cached_source_posts(source: &FeedSource) -> Option<CachedSourcePosts> {
	cached_source_posts_with_mode(source, CacheServeMode::Normal)
}

async fn fetch_source_posts(source: FeedSource, quarantine: bool, refresh_origin: RefreshOrigin) -> Result<CachedSourcePosts, String> {
	if let Some(entry) = cached_source_posts(&source) {
		return Ok(entry);
	}

	if let FeedSource::Subreddit { name } = &source {
		if !matches!(refresh_origin, RefreshOrigin::DirectLiveFetch) {
			let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
			if remembered
				.get(&name.to_lowercase())
				.and_then(|entry| entry.banned_until)
				.is_some_and(|until| until > Instant::now())
			{
				return Err("banned".to_string());
			}
		}
		mark_plain_subreddit_refresh_started(name);
	}

	let (posts, after) = match Post::fetch(&source.build_path(), quarantine).await {
		Ok(result) => result,
		Err(error) => {
			if error.contains("oauth_unavailable") {
				if let Some(entry) = cached_source_posts_with_mode(&source, CacheServeMode::AllowStaleOnOauthOutage) {
					warn!("post_cache: serving stale cached source {} because OAuth is unavailable", source.display_name());
					return Ok(entry);
				}
			}
			if let FeedSource::Subreddit { name } = &source {
				record_plain_subreddit_refresh_failure(name, &error, refresh_origin);
			}
			return Err(error);
		}
	};
	let posts = source.filter_posts(posts);
	if matches!(refresh_origin, RefreshOrigin::Daemon | RefreshOrigin::FrontPageSession) && !matches!(&source, FeedSource::Subreddit { name } if !name.starts_with("u_")) {
		spawn_author_link_prefetch(posts.clone());
	}

	match &source {
		FeedSource::Subreddit { name } if !name.starts_with("u_") => {
			publish_plain_subreddit_refresh(name, posts.clone(), after.clone(), refresh_origin);
		}
		_ => {
			crate::utils::store_post_flairs(&posts);
			store_source_posts(&source, posts.clone(), after.clone());
		}
	}

	Ok(CachedSourcePosts {
		posts,
		after,
		fetched_at: Instant::now(),
		refresh_origin: match source {
			FeedSource::Subreddit { ref name } if !name.starts_with("u_") => Some(refresh_origin),
			_ => None,
		},
	})
}

fn build_session_id(request: &FeedSessionRequest) -> String {
	let mut hasher = DefaultHasher::new();
	request.quarantine.hash(&mut hasher);
	request.query_fingerprint.hash(&mut hasher);

	let mut filter_values = request.filters.iter().cloned().collect::<Vec<_>>();
	filter_values.sort();
	filter_values.hash(&mut hasher);

	for source in &request.sources {
		source.hash(&mut hasher);
	}

	format!("{:x}", hasher.finish())
}

fn make_session(request: FeedSessionRequest) -> FeedSession {
	let id = build_session_id(&request);
	let sources = request
		.sources
		.iter()
		.cloned()
		.map(|source| SessionSourceEntry {
			source,
			state: SessionSourceState::Unstarted,
			last_error: None,
		})
		.collect();

	FeedSession {
		id,
		request,
		sources,
		frozen_pages: HashMap::new(),
		background_active: false,
		background_complete: false,
		requested_page: 0,
		last_updated: Instant::now(),
		revision: 0,
	}
}

fn reconcile_session_with_source_cache(session: &mut FeedSession) {
	let mut changed = false;
	for entry in &mut session.sources {
		if cached_source_posts(&entry.source).is_some() && entry.state != SessionSourceState::Ready {
			entry.state = SessionSourceState::Ready;
			entry.last_error = None;
			changed = true;
		}
	}

	if changed {
		touch_session(session);
	}
}

fn filtered_source_queues(session: &FeedSession) -> Vec<ReadySourceQueue> {
	let mut queues = Vec::new();
	for entry in &session.sources {
		if entry.state != SessionSourceState::Ready {
			continue;
		}

		let Some(cached) = cached_source_posts(&entry.source) else {
			continue;
		};

		if cached.posts.is_empty() {
			continue;
		}

		let filtered = cached
			.posts
			.iter()
			.filter(|post| {
				!session.request.filters.contains(&post.community)
					&& !session.request.filters.contains(&format!("u_{}", post.author.name))
					&& !crate::utils::is_deleted_reddit_placeholder_post(post)
			})
			.cloned()
			.collect::<VecDeque<_>>();

		if !filtered.is_empty() {
			queues.push(ReadySourceQueue {
				source: entry.source.clone(),
				posts: filtered,
				fetched_at: cached.fetched_at,
			});
		}
	}

	queues
}

fn ready_queue_head(queue_index: usize, queue: &ReadySourceQueue) -> Option<ReadyQueueHead> {
	Some(ReadyQueueHead {
		queue_index,
		head_created_ts: queue.posts.front()?.created_ts,
		fetched_at: queue.fetched_at,
		source_priority: source_priority(&queue.source),
	})
}

fn merged_posts_for_session(session: &FeedSession) -> Vec<Post> {
	let mut queues = filtered_source_queues(session);

	let mut all_posts = Vec::new();
	let mut seen_ids = HashSet::new();
	let mut queue_heads = BinaryHeap::new();

	for (queue_index, queue) in queues.iter().enumerate() {
		if let Some(head) = ready_queue_head(queue_index, queue) {
			queue_heads.push(head);
		}
	}

	while let Some(head) = queue_heads.pop() {
		let queue = &mut queues[head.queue_index];
		let Some(post) = queue.posts.pop_front() else {
			continue;
		};

		if seen_ids.insert(post.id.clone()) {
			all_posts.push(post);
		}

		if let Some(next_head) = ready_queue_head(head.queue_index, queue) {
			queue_heads.push(next_head);
		}
	}

	collapse_duplicate_submissions(all_posts)
}

fn current_page_is_ready(session: &FeedSession, posts: &[Post], page: usize, per_page: usize) -> bool {
	posts.len() >= ((page + 1) * per_page) || session.background_complete || session.frozen_pages.contains_key(&page)
}

fn sort_page_posts_by_date_desc(mut posts: Vec<Post>) -> Vec<Post> {
	posts.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
	posts
}

fn session_status_from_posts(session: &FeedSession, posts: &[Post], page: usize, per_page: usize) -> FeedSessionStatus {
	let ready_sources = session.sources.iter().filter(|entry| entry.state == SessionSourceState::Ready).count();
	let daemon_cached_sources = session
		.sources
		.iter()
		.filter(|entry| cached_source_origin(&entry.source) == Some(RefreshOrigin::Daemon))
		.count();
	let failed_sources = session.sources.iter().filter(|entry| entry.state == SessionSourceState::Failed).count();
	let total_user_sources = session
		.sources
		.iter()
		.filter(|entry| matches!(&entry.source, FeedSource::Subreddit { name } if name.starts_with("u_")))
		.count();
	let ready_user_sources = session
		.sources
		.iter()
		.filter(|entry| entry.state == SessionSourceState::Ready)
		.filter(|entry| matches!(&entry.source, FeedSource::Subreddit { name } if name.starts_with("u_")))
		.count();
	let current_page_ready = current_page_is_ready(session, posts, page, per_page);
	let next_page_ready = posts.len() >= ((page + 2) * per_page);
	let warming = session.background_active;
	let daemon_cache_suffix = if daemon_cached_sources > 0 {
		format!(" ({daemon_cached_sources} from daemon cache)")
	} else {
		String::new()
	};
	let status_message = if warming {
		if ready_user_sources < total_user_sources {
			format!(
				"Loading followed feeds in the background. Warmed {} of {} feeds{}.",
				ready_sources,
				session.sources.len(),
				daemon_cache_suffix
			)
		} else if !current_page_ready {
			format!(
				"Warmed {} of {} feeds{}. Filling this page with newer posts.",
				ready_sources,
				session.sources.len(),
				daemon_cache_suffix
			)
		} else if next_page_ready {
			format!("Warmed {} of {} feeds{}. Next page is ready.", ready_sources, session.sources.len(), daemon_cache_suffix)
		} else {
			format!(
				"Warmed {} of {} feeds{}. Preparing the next page in the background.",
				ready_sources,
				session.sources.len(),
				daemon_cache_suffix
			)
		}
	} else if session.background_complete {
		format!("Warmed all {} feeds{}.", session.sources.len(), daemon_cache_suffix)
	} else if next_page_ready {
		format!(
			"Warmed {} of {} feeds{}. More feeds will load as you paginate.",
			ready_sources,
			session.sources.len(),
			daemon_cache_suffix
		)
	} else {
		format!(
			"Warmed {} of {} feeds{} for now. More feeds will load as needed.",
			ready_sources,
			session.sources.len(),
			daemon_cache_suffix
		)
	};

	FeedSessionStatus {
		session_id: session.id.clone(),
		total_sources: session.sources.len(),
		ready_sources,
		daemon_cached_sources,
		failed_sources,
		warming,
		current_page_ready,
		next_page_ready,
		posts_merged: posts.len(),
		status_message,
		refresh_token: session.revision ^ ((posts.len() as u64) << 16) ^ (ready_sources as u64) ^ ((daemon_cached_sources as u64) << 8),
	}
}

fn touch_session(session: &mut FeedSession) {
	session.last_updated = Instant::now();
	session.revision = session.revision.wrapping_add(1);
}

fn background_target_posts(session: &FeedSession) -> usize {
	(session.requested_page + 1 + BACKGROUND_LOOKAHEAD_PAGES) * POSTS_PER_PAGE
}

fn source_budget_target(page: usize, per_page: usize) -> usize {
	((page + 1) * per_page).max(INITIAL_SOURCE_WINDOW)
}

fn initial_source_budget_target(page: usize) -> usize {
	source_budget_target(page, POSTS_PER_PAGE)
}

fn max_sync_source_budget(page: usize) -> usize {
	initial_source_budget_target(page) + (MAX_SYNC_FILL_BATCHES * PAGE_FILL_BATCH)
}

fn background_target_sources(session: &FeedSession) -> usize {
	source_budget_target(session.requested_page + BACKGROUND_LOOKAHEAD_PAGES, POSTS_PER_PAGE).min(session.sources.len())
}

fn attempted_source_count(session: &FeedSession) -> usize {
	session.sources.iter().filter(|entry| entry.state != SessionSourceState::Unstarted).count()
}

fn sync_sources_to_request(session: &FeedSession, page: usize) -> usize {
	initial_source_budget_target(page).saturating_sub(attempted_source_count(session))
}

fn followup_source_budget(session: &FeedSession) -> usize {
	attempted_source_count(session).saturating_add(PAGE_FILL_BATCH).min(session.sources.len())
}

fn background_target_reached(session: &FeedSession) -> bool {
	merged_posts_for_session(session).len() >= background_target_posts(session) && attempted_source_count(session) >= background_target_sources(session)
}

fn session_has_remaining_sources(session: &FeedSession) -> bool {
	session.sources.iter().any(|entry| entry.state == SessionSourceState::Unstarted)
}

fn remembered_entry_for_source<'a>(remembered: &'a HashMap<String, RememberedSubredditEntry>, source: &FeedSource) -> Option<&'a RememberedSubredditEntry> {
	match source {
		FeedSource::Subreddit { name } if !name.starts_with("u_") => remembered.get(&name.to_lowercase()),
		_ => None,
	}
}

fn compare_optional_instants_desc(left: Option<Instant>, right: Option<Instant>) -> Ordering {
	match (left, right) {
		(Some(left), Some(right)) => right.cmp(&left),
		(None, Some(_)) => Ordering::Greater,
		(Some(_), None) => Ordering::Less,
		(None, None) => Ordering::Equal,
	}
}

fn compare_warm_target_priority(
	left_index: usize,
	left_source: &FeedSource,
	left_entry: Option<&RememberedSubredditEntry>,
	right_index: usize,
	right_source: &FeedSource,
	right_entry: Option<&RememberedSubredditEntry>,
) -> Ordering {
	let left_never_refreshed = left_entry.and_then(|entry| entry.last_refresh_succeeded_at).is_none();
	let right_never_refreshed = right_entry.and_then(|entry| entry.last_refresh_succeeded_at).is_none();
	match right_never_refreshed.cmp(&left_never_refreshed) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	match right_entry
		.and_then(|entry| Some(entry.last_requested_at))
		.cmp(&left_entry.and_then(|entry| Some(entry.last_requested_at)))
	{
		Ordering::Equal => {}
		ordering => return ordering,
	}

	match compare_optional_instants_desc(
		left_entry.and_then(|entry| entry.last_refresh_succeeded_at),
		right_entry.and_then(|entry| entry.last_refresh_succeeded_at),
	) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	match source_priority(left_source).cmp(&source_priority(right_source)) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	left_index.cmp(&right_index)
}

fn warm_targets(session: &FeedSession, target_count: usize) -> Vec<usize> {
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let mut targets = session
		.sources
		.iter()
		.enumerate()
		.filter(|(_, entry)| entry.state == SessionSourceState::Unstarted)
		.map(|(index, _)| index)
		.collect::<Vec<_>>();

	targets.sort_by(|left_index, right_index| {
		let left_source = &session.sources[*left_index].source;
		let right_source = &session.sources[*right_index].source;
		let left_entry = remembered_entry_for_source(&remembered, left_source);
		let right_entry = remembered_entry_for_source(&remembered, right_source);
		compare_warm_target_priority(*left_index, left_source, left_entry, *right_index, right_source, right_entry)
	});
	targets.truncate(target_count);
	targets
}

async fn warm_session_indices(session_id: &str, indices: Vec<usize>) {
	if indices.is_empty() {
		return;
	}

	let jobs = {
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get_mut(session_id) else {
			return;
		};
		touch_session(session);
		indices
			.iter()
			.filter_map(|index| {
				let entry = session.sources.get_mut(*index)?;
				if entry.state != SessionSourceState::Unstarted {
					return None;
				}
				entry.state = SessionSourceState::Fetching;
				Some((*index, entry.source.clone(), session.request.quarantine))
			})
			.collect::<Vec<_>>()
	};

	for chunk in jobs.chunks(FETCH_CONCURRENCY) {
		let mut handles = Vec::new();
		for (index, source, quarantine) in chunk.iter().cloned() {
			let task_source = source.clone();
			handles.push((
				index,
				source,
				tokio::spawn(async move { fetch_source_posts(task_source, quarantine, RefreshOrigin::FrontPageSession).await }),
			));
		}

		for (index, source, handle) in handles {
			let result = handle.await;
			let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get_mut(session_id) else {
				continue;
			};
			let Some(entry) = session.sources.get_mut(index) else {
				continue;
			};

			match result {
				Ok(Ok(_cached)) => {
					entry.state = SessionSourceState::Ready;
					entry.last_error = None;
					log::trace!("post_cache: warmed {}", source.display_name());
				}
				Ok(Err(error)) => {
					entry.state = SessionSourceState::Failed;
					entry.last_error = Some(error.clone());
					warn!("post_cache: failed to warm {}: {}", source.display_name(), error);
				}
				Err(join_error) => {
					let error = format!("task join error: {join_error}");
					entry.state = SessionSourceState::Failed;
					entry.last_error = Some(error.clone());
					warn!("post_cache: failed to warm {}: {}", source.display_name(), error);
				}
			}
			touch_session(session);
		}
	}
}

fn count_sources_in_state(session: &FeedSession, state: SessionSourceState) -> usize {
	session.sources.iter().filter(|entry| entry.state == state).count()
}

fn mark_fetching_sources_failed(session: &mut FeedSession) {
	for entry in &mut session.sources {
		if entry.state == SessionSourceState::Fetching {
			entry.state = SessionSourceState::Failed;
			entry.last_error = Some("task join error".to_string());
		}
	}
}

async fn reconcile_stuck_fetching_sources(session_id: &str) {
	let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
	let Some(session) = sessions.get_mut(session_id) else {
		return;
	};
	if count_sources_in_state(session, SessionSourceState::Fetching) == 0 {
		return;
	}
	mark_fetching_sources_failed(session);
	touch_session(session);
}

async fn warm_session_to_fill(session_id: &str, page: usize, per_page: usize, max_sources: usize) {
	loop {
		let (page_ready, attempted_sources, remaining) = {
			let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get(session_id) else {
				return;
			};
			let merged = merged_posts_for_session(session);
			let target = (page + 1) * per_page;
			let attempted_sources = attempted_source_count(session);
			let remaining_budget = max_sources.saturating_sub(attempted_sources).min(PAGE_FILL_BATCH);
			(merged.len() >= target, attempted_sources, warm_targets(session, remaining_budget))
		};

		if page_ready || attempted_sources >= max_sources || remaining.is_empty() {
			break;
		}

		warm_session_indices(session_id, remaining).await;
	}

	let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
	if let Some(session) = sessions.get_mut(session_id) {
		touch_session(session);
	}
}

async fn warm_session_background(session_id: String) {
	loop {
		reconcile_stuck_fetching_sources(&session_id).await;
		let (remaining, target_reached) = {
			let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get(&session_id) else {
				return;
			};
			let target_reached = background_target_reached(session);
			(warm_targets(session, PAGE_FILL_BATCH), target_reached)
		};

		if remaining.is_empty() {
			let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			if let Some(session) = sessions.get_mut(&session_id) {
				session.background_active = false;
				session.background_complete = true;
				touch_session(session);
			}
			info!("post_cache: feed session {session_id} warm complete");
			return;
		}

		if target_reached {
			let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			if let Some(session) = sessions.get_mut(&session_id) {
				session.background_active = false;
				touch_session(session);
			}
			info!("post_cache: feed session {session_id} warm paused");
			return;
		}

		warm_session_indices(&session_id, remaining).await;
	}
}

fn session_requested_page_ready(session: &FeedSession, page: usize, per_page: usize) -> bool {
	let merged_posts = merged_posts_for_session(session);
	current_page_is_ready(session, &merged_posts, page, per_page)
}

fn plan_session_warm_mode(session: &mut FeedSession, page: usize, per_page: usize, session_is_new: bool) -> Option<SessionWarmMode> {
	if !session_has_remaining_sources(session) {
		session.background_complete = true;
		session.background_active = false;
		return None;
	}

	if session.background_active {
		return None;
	}

	if session_is_new {
		if background_target_reached(session) {
			return None;
		}
		session.background_complete = false;
		session.background_active = true;
		return Some(SessionWarmMode::InitialLoad);
	}

	if session_requested_page_ready(session, page, per_page) {
		session.background_active = false;
		return None;
	}

	session.background_complete = false;
	session.background_active = true;
	Some(SessionWarmMode::FollowupCachePreferred {
		max_sources: followup_source_budget(session),
	})
}

async fn warm_session_followup_cached(session_id: String, page: usize, per_page: usize, max_sources: usize) {
	warm_session_to_fill(&session_id, page, per_page, max_sources).await;

	let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
	if let Some(session) = sessions.get_mut(&session_id) {
		session.background_active = false;
		if !session_has_remaining_sources(session) {
			session.background_complete = true;
		}
		touch_session(session);
	}
}

pub async fn ensure_feed_session(request: FeedSessionRequest, page: usize, per_page: usize) -> String {
	let session_id = build_session_id(&request);
	reconcile_stuck_fetching_sources(&session_id).await;

	let session_is_new = {
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		sessions.retain(|_, session| !session_is_expired(session));

		let session_is_new = !sessions.contains_key(&session_id);
		let session = sessions.entry(session_id.clone()).or_insert_with(|| make_session(request.clone()));
		session.requested_page = session.requested_page.max(page);
		reconcile_session_with_source_cache(session);
		touch_session(session);
		session_is_new
	};

	let sync_target = if session_is_new {
		let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get(&session_id) else {
			return session_id;
		};
		warm_targets(session, sync_sources_to_request(session, page))
	} else {
		Vec::new()
	};

	warm_session_indices(&session_id, sync_target).await;
	if session_is_new {
		warm_session_to_fill(&session_id, page, per_page, max_sync_source_budget(page)).await;
	}

	let warm_mode = {
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get_mut(&session_id) else {
			return session_id;
		};
		plan_session_warm_mode(session, page, per_page, session_is_new)
	};

	if let Some(warm_mode) = warm_mode {
		let background_id = session_id.clone();
		tokio::spawn(async move {
			match warm_mode {
				SessionWarmMode::InitialLoad => warm_session_background(background_id).await,
				SessionWarmMode::FollowupCachePreferred { max_sources } => {
					warm_session_followup_cached(background_id, page, per_page, max_sources).await;
				}
			}
		});
	}

	session_id
}

pub async fn begin_feed_session(request: FeedSessionRequest, page: usize, per_page: usize) -> String {
	let session_id = build_session_id(&request);
	reconcile_stuck_fetching_sources(&session_id).await;

	let session_is_new = {
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		sessions.retain(|_, session| !session_is_expired(session));

		let session_is_new = !sessions.contains_key(&session_id);
		let session = sessions.entry(session_id.clone()).or_insert_with(|| make_session(request.clone()));
		session.requested_page = session.requested_page.max(page);
		reconcile_session_with_source_cache(session);
		touch_session(session);
		session_is_new
	};

	let warm_mode = {
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get_mut(&session_id) else {
			return session_id;
		};
		plan_session_warm_mode(session, page, per_page, session_is_new)
	};

	if let Some(warm_mode) = warm_mode {
		let background_id = session_id.clone();
		tokio::spawn(async move {
			match warm_mode {
				SessionWarmMode::InitialLoad => {
					warm_session_to_fill(&background_id, page, per_page, max_sync_source_budget(page)).await;
					warm_session_background(background_id).await;
				}
				SessionWarmMode::FollowupCachePreferred { max_sources } => {
					warm_session_followup_cached(background_id, page, per_page, max_sources).await;
				}
			}
		});
	}

	session_id
}

pub fn feed_page(session_id: &str, page: usize, per_page: usize) -> Option<FeedPage> {
	let mut sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get_mut(session_id)?;
	if session_is_expired(session) {
		return None;
	}
	reconcile_session_with_source_cache(session);

	let merged_posts = merged_posts_for_session(session);
	let merged_len = merged_posts.len();
	let posts = if let Some(frozen_posts) = session.frozen_pages.get(&page) {
		frozen_posts.clone()
	} else {
		if current_page_is_ready(session, &merged_posts, page, per_page) {
			let start = page * per_page;
			let end = (start + per_page).min(merged_posts.len());
			let frozen = if start < merged_posts.len() {
				sort_page_posts_by_date_desc(merged_posts[start..end].to_vec())
			} else {
				Vec::new()
			};
			session.frozen_pages.insert(page, frozen.clone());
			frozen
		} else {
			let start = page * per_page;
			let end = (start + per_page).min(merged_posts.len());
			if start < merged_posts.len() {
				sort_page_posts_by_date_desc(merged_posts[start..end].to_vec())
			} else {
				Vec::new()
			}
		}
	};
	let has_more_available = session.background_active || page < session.requested_page || session_has_remaining_sources(session);
	Some(FeedPage {
		posts,
		has_prev: page > 0,
		has_next: ((page + 1) * per_page) < merged_len || has_more_available,
	})
}

pub fn feed_status(session_id: &str, page: usize, per_page: usize) -> Option<FeedSessionStatus> {
	let mut sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get_mut(session_id)?;
	if session_is_expired(session) {
		return None;
	}
	reconcile_session_with_source_cache(session);

	let posts = merged_posts_for_session(session);
	Some(session_status_from_posts(session, &posts, page, per_page))
}

pub fn cache_stats() -> (usize, usize) {
	fn source_from_cache_key(key: &str) -> Option<FeedSource> {
		if let Some(name) = key.strip_prefix("subreddit:") {
			Some(FeedSource::Subreddit { name: name.to_string() })
		} else if let Some(rest) = key.strip_prefix("flair:") {
			let mut parts = rest.splitn(2, ':');
			let subreddit = parts.next().unwrap_or_default().to_string();
			let flair = parts.next().unwrap_or_default().to_string();
			Some(FeedSource::Flair { subreddit, flair })
		} else {
			None
		}
	}

	let cache = SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
	let sources = cache
		.iter()
		.filter(|(key, entry)| source_from_cache_key(key).is_some_and(|source| source_cache_is_servable(&source, entry)))
		.count();
	let posts: usize = cache
		.iter()
		.filter(|(key, entry)| source_from_cache_key(key).is_some_and(|source| source_cache_is_servable(&source, entry)))
		.map(|(_, entry)| entry.posts.len())
		.sum();
	(sources, posts)
}

fn visible_sources_from_preferences(preferences: &Preferences) -> Vec<FeedSource> {
	let mut seen = HashSet::new();
	let mut sources = Vec::new();

	for subreddit in &preferences.subscriptions {
		let normalized = subreddit.trim();
		if normalized.is_empty() {
			continue;
		}
		let source = FeedSource::Subreddit { name: normalized.to_string() };
		if seen.insert(source.cache_key()) {
			sources.push(source);
		}
	}

	for subscription in &preferences.flair_subscriptions {
		if subscription.subreddit.trim().is_empty() || subscription.flair.trim().is_empty() {
			continue;
		}
		let source = FeedSource::Flair {
			subreddit: subscription.subreddit.clone(),
			flair: subscription.flair.clone(),
		};
		if seen.insert(source.cache_key()) {
			sources.push(source);
		}
	}

	sort_sources(&mut sources);
	sources
}

fn daemon_next_fetch_map() -> (u64, HashMap<String, usize>) {
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let mut entries = remembered
		.values()
		.filter(|entry| remembered_entry_is_active(entry) && !remembered_entry_refresh_blocked(entry) && !entry.refresh_in_flight)
		.cloned()
		.collect::<Vec<_>>();
	entries.sort_by(remembered_refresh_priority);

	let seconds_until_tick = REMEMBERED_SUBREDDIT_DAEMON_NEXT_TICK_AT
		.lock()
		.ok()
		.and_then(|next_tick| *next_tick)
		.map(|next_tick| next_tick.saturating_duration_since(Instant::now()).as_secs())
		.unwrap_or(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS);

	let positions = entries.into_iter().enumerate().map(|(index, entry)| (entry.name, index)).collect::<HashMap<_, _>>();

	(seconds_until_tick, positions)
}

pub fn cache_status_for_preferences(preferences: &Preferences) -> CacheStatusResponse {
	let now = Instant::now();
	let generated_at_unix = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_secs())
		.unwrap_or_default();
	let visible_sources = visible_sources_from_preferences(preferences);
	let (seconds_until_tick, queue_positions) = daemon_next_fetch_map();
	let cache = SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());

	let mut sources = Vec::with_capacity(visible_sources.len());
	for source in visible_sources {
		let cache_key = source.cache_key();
		let cache_entry = cache.get(&cache_key);
		let remembered_entry = match &source {
			FeedSource::Subreddit { name } if !name.starts_with("u_") => remembered.get(&name.to_lowercase()),
			_ => None,
		};
		let daemon_managed = matches!(&source, FeedSource::Subreddit { name } if !name.starts_with("u_"));
		let cached = cache_entry.is_some();
		let cached_posts = cache_entry.map(|entry| entry.posts.len()).unwrap_or_default();
		let cached_age_secs = cache_entry.map(|entry| entry.fetched_at.elapsed().as_secs());
		let head_post_created_ts = cache_entry.and_then(|entry| entry.posts.first().map(|post| post.created_ts));
		let head_post_age_secs = head_post_created_ts.map(|created_ts| generated_at_unix.saturating_sub(created_ts));
		let cache_expires_in_secs = if let Some(entry) = remembered_entry.filter(|entry| remembered_entry_is_active(entry)) {
			Some(REMEMBERED_SUBREDDIT_TTL_SECS.saturating_sub(entry.last_requested_at.elapsed().as_secs()))
		} else {
			cache_entry.map(|entry| CACHE_TTL_SECS.saturating_sub(entry.fetched_at.elapsed().as_secs()))
		};
		let blocked_until_secs = remembered_entry
			.and_then(|entry| entry.banned_until)
			.map(|until| until.saturating_duration_since(now).as_secs())
			.filter(|seconds| *seconds > 0);
		let predicted_next_fetch_in_secs = if let Some(entry) = remembered_entry {
			if entry.refresh_in_flight {
				Some(0)
			} else if let Some(blocked) = blocked_until_secs {
				Some(blocked)
			} else {
				queue_positions
					.get(&entry.name)
					.map(|position| seconds_until_tick + (*position as u64 * REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS))
			}
		} else {
			None
		};
		let schedule_state = if let Some(entry) = remembered_entry {
			if entry.refresh_in_flight {
				"in_flight"
			} else if blocked_until_secs.is_some() {
				"blocked"
			} else if queue_positions.contains_key(&entry.name) {
				"scheduled"
			} else {
				"idle"
			}
		} else if daemon_managed {
			"not_cached"
		} else {
			"session_only"
		};

		sources.push(CacheStatusSource {
			source_key: cache_key,
			display_name: source.display_name(),
			source_type: match source {
				FeedSource::Subreddit { .. } => "subreddit".to_string(),
				FeedSource::Flair { .. } => "flair".to_string(),
			},
			daemon_managed,
			cached,
			cached_posts,
			cached_age_secs,
			head_post_created_ts,
			head_post_age_secs,
			cache_expires_in_secs,
			last_refresh_origin: cache_entry.and_then(|entry| entry.refresh_origin).map(|origin| format!("{origin:?}").to_lowercase()),
			last_refresh_succeeded_secs_ago: remembered_entry
				.and_then(|entry| entry.last_refresh_succeeded_at)
				.map(|instant| instant.elapsed().as_secs()),
			last_refresh_failed_secs_ago: remembered_entry.and_then(|entry| entry.last_refresh_failed_at).map(|instant| instant.elapsed().as_secs()),
			refresh_frequency_secs: remembered_entry.map(effective_freshness_target_secs),
			schedule_state: schedule_state.to_string(),
			predicted_next_fetch_in_secs,
			blocked_until_secs,
			consecutive_failures: remembered_entry.map(|entry| entry.consecutive_failures).unwrap_or_default(),
		});
	}

	let summary = CacheStatusSummary {
		sources_total: sources.len(),
		cached_sources: sources.iter().filter(|source| source.cached).count(),
		daemon_managed_sources: sources.iter().filter(|source| source.daemon_managed).count(),
		scheduled_sources: sources.iter().filter(|source| source.schedule_state == "scheduled").count(),
		in_flight_sources: sources.iter().filter(|source| source.schedule_state == "in_flight").count(),
		blocked_sources: sources.iter().filter(|source| source.schedule_state == "blocked").count(),
	};

	CacheStatusResponse {
		generated_at_unix,
		summary,
		sources,
	}
}

#[cfg(test)]
mod tests {
	use super::{
		active_browser_session_paths, attempted_source_count, background_target_reached, background_target_sources, cache_status_for_preferences, count_sources_in_state,
		feed_page, feed_status, followup_source_budget, get_cached_source_page, has_active_browser_sessions, initial_source_budget_target, mark_fetching_sources_failed,
		max_sync_source_budget, merged_posts_for_session, pick_next_remembered_subreddit_to_refresh, plan_session_warm_mode, publish_plain_subreddit_refresh,
		reconcile_session_with_source_cache, record_plain_subreddit_refresh_failure, remember_frontpage_subreddits, remember_subreddit_request, remembered_refresh_priority,
		restore_persisted_state, session_status_from_posts, snapshot_persisted_remembered_subreddits, snapshot_persisted_source_entries, sort_sources, source_budget_target,
		source_cache_is_servable, store_source_posts, store_source_posts_with_origin, sync_sources_to_request, warm_targets, ActiveBrowserSession, FeedSession,
		FeedSessionRequest, FeedSource, RefreshOrigin, RememberedSubredditEntry, SessionSourceEntry, SessionSourceState, SessionWarmMode, ACTIVE_BROWSER_SESSIONS,
		ACTIVE_BROWSER_SESSION_TTL_SECS, BANNED_REFRESH_BACKOFF_SECS, CACHE_TTL_SECS, FEED_SESSIONS, PERMANENT_FAILURE_BACKOFF_SECS, POSTS_PER_PAGE, REMEMBERED_SUBREDDITS,
		SOURCE_CACHE,
	};
	use crate::utils::{Author, Awards, Flags, Flair, FlairSubscription, Media, Post, Preferences};
	use std::cmp::Ordering;
	use std::collections::{HashMap, HashSet};
	use std::sync::{LazyLock, Mutex, MutexGuard};
	use std::time::{Duration, Instant};

	static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

	fn test_lock() -> MutexGuard<'static, ()> {
		TEST_LOCK.lock().unwrap()
	}

	fn reset_test_caches() {
		SOURCE_CACHE.lock().unwrap().clear();
		FEED_SESSIONS.lock().unwrap().clear();
		REMEMBERED_SUBREDDITS.lock().unwrap().clear();
		ACTIVE_BROWSER_SESSIONS.lock().unwrap().clear();
	}

	fn session_entry(source: FeedSource, state: SessionSourceState) -> SessionSourceEntry {
		SessionSourceEntry { source, state, last_error: None }
	}

	fn seed_source(source: &FeedSource, posts: Vec<Post>) {
		store_source_posts(source, posts, String::new());
	}

	fn remembered_entry(name: &str) -> RememberedSubredditEntry {
		RememberedSubredditEntry {
			name: name.to_string(),
			last_requested_at: Instant::now(),
			last_refresh_started_at: None,
			last_refresh_succeeded_at: None,
			last_refresh_failed_at: None,
			banned_until: None,
			consecutive_failures: 0,
			estimated_post_interval_secs: 21_600,
			refresh_in_flight: false,
		}
	}

	fn test_prefs_with_saved_feeds() -> Preferences {
		Preferences {
			subscriptions: vec![
				"rust".to_string(),
				"popular".to_string(),
				"rust+programming".to_string(),
				"u_demo".to_string(),
				String::new(),
			],
			flair_subscriptions: vec![
				FlairSubscription {
					subreddit: "programming".to_string(),
					flair: "News".to_string(),
				},
				FlairSubscription {
					subreddit: "all".to_string(),
					flair: "Ignored".to_string(),
				},
			],
			..Preferences::default()
		}
	}

	fn empty_flair() -> Flair {
		Flair {
			flair_parts: Vec::new(),
			text: String::new(),
			background_color: String::new(),
			foreground_color: String::new(),
		}
	}

	fn test_post(id: &str, community: &str, created_ts: u64) -> Post {
		Post {
			id: id.to_string(),
			kind: "t3".to_string(),
			title: id.to_string(),
			community: community.to_string(),
			body: String::new(),
			author: Author {
				name: "demo".to_string(),
				flair: empty_flair(),
				distinguished: String::new(),
				external_links: Vec::new(),
				avg_video_length: None,
			},
			permalink: format!("/r/{community}/comments/{id}/"),
			parent_kind: String::new(),
			link_title: String::new(),
			link_permalink: String::new(),
			poll: None,
			score: ("1".to_string(), "1".to_string()),
			upvote_ratio: 100,
			post_type: "self".to_string(),
			flair: empty_flair(),
			flags: Flags::new(false, false, false),
			thumbnail: Media::empty(),
			media: Media::empty(),
			domain: String::new(),
			rel_time: String::new(),
			created: String::new(),
			created_ts,
			num_duplicates: 0,
			comments: ("0".to_string(), "0".to_string()),
			gallery: Vec::new(),
			awards: Awards(Vec::new()),
			nsfw: false,
			out_url: None,
			ws_url: String::new(),
			preview_body: String::new(),
			collapsed_duplicates: Vec::new(),
		}
	}

	#[test]
	fn sorts_user_sources_before_subreddits_and_flairs() {
		let _guard = test_lock();
		reset_test_caches();
		let mut sources = vec![
			FeedSource::Flair {
				subreddit: "zeta".to_string(),
				flair: "Tag".to_string(),
			},
			FeedSource::Subreddit { name: "rust".to_string() },
			FeedSource::Subreddit { name: "u_demo".to_string() },
		];

		sort_sources(&mut sources);

		assert!(matches!(&sources[0], FeedSource::Subreddit { name } if name == "u_demo"));
		assert!(matches!(&sources[1], FeedSource::Subreddit { name } if name == "rust"));
		assert!(matches!(&sources[2], FeedSource::Flair { subreddit, .. } if subreddit == "zeta"));
	}

	#[test]
	fn remember_subreddit_request_uses_normalized_plain_names() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("Rust");
		remember_subreddit_request("popular");
		remember_subreddit_request("rust+programming");
		remember_subreddit_request("u_demo");
		remember_subreddit_request("");

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		assert_eq!(remembered.len(), 1);
		assert!(remembered.contains_key("rust"));
	}

	#[test]
	fn remember_frontpage_subreddits_tracks_subscriptions_and_flair_parents() {
		let _guard = test_lock();
		reset_test_caches();

		remember_frontpage_subreddits(&test_prefs_with_saved_feeds());

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		assert!(remembered.contains_key("rust"));
		assert!(remembered.contains_key("programming"));
		assert_eq!(remembered.len(), 2);
	}

	#[test]
	fn remembered_plain_subreddit_cache_remains_servable_beyond_short_ttl() {
		let _guard = test_lock();
		reset_test_caches();

		let source = FeedSource::Subreddit { name: "rust".to_string() };
		remember_subreddit_request("rust");
		seed_source(&source, vec![test_post("one", "rust", 100)]);
		SOURCE_CACHE.lock().unwrap().get_mut(&source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let cache = SOURCE_CACHE.lock().unwrap();
		let entry = cache.get(&source.cache_key()).unwrap();
		assert!(source_cache_is_servable(&source, entry));
		drop(cache);
		assert!(get_cached_source_page(&source, 0, 25).is_some());
	}

	#[test]
	fn non_remembered_plain_subreddit_cache_expires_on_short_ttl() {
		let _guard = test_lock();
		reset_test_caches();

		let source = FeedSource::Subreddit { name: "rust".to_string() };
		seed_source(&source, vec![test_post("one", "rust", 100)]);
		SOURCE_CACHE.lock().unwrap().get_mut(&source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let cache = SOURCE_CACHE.lock().unwrap();
		let entry = cache.get(&source.cache_key()).unwrap();
		assert!(!source_cache_is_servable(&source, entry));
		drop(cache);
		assert!(get_cached_source_page(&source, 0, 25).is_none());
	}

	#[test]
	fn flair_and_user_sources_keep_short_ttl_rules() {
		let _guard = test_lock();
		reset_test_caches();

		let flair_source = FeedSource::Flair {
			subreddit: "rust".to_string(),
			flair: "News".to_string(),
		};
		let user_source = FeedSource::Subreddit { name: "u_demo".to_string() };
		seed_source(&flair_source, vec![test_post("flair", "rust", 100)]);
		seed_source(&user_source, vec![test_post("user", "u_demo", 99)]);
		SOURCE_CACHE.lock().unwrap().get_mut(&flair_source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);
		SOURCE_CACHE.lock().unwrap().get_mut(&user_source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		assert!(get_cached_source_page(&flair_source, 0, 25).is_none());
		assert!(get_cached_source_page(&user_source, 0, 25).is_none());
	}

	#[test]
	fn pick_next_remembered_subreddit_prefers_never_refreshed_then_overdue() {
		let _guard = test_lock();
		reset_test_caches();

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let mut first = remembered_entry("alpha");
		first.last_requested_at = Instant::now();
		let mut second = remembered_entry("beta");
		second.last_requested_at = Instant::now() - Duration::from_secs(60);
		second.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(10));
		let mut third = remembered_entry("gamma");
		third.last_requested_at = Instant::now() - Duration::from_secs(120);
		third.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(60_000));
		remembered.insert("alpha".to_string(), first);
		remembered.insert("beta".to_string(), second);
		remembered.insert("gamma".to_string(), third);
		drop(remembered);

		assert_eq!(pick_next_remembered_subreddit_to_refresh().as_deref(), Some("alpha"));

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		remembered.get_mut("alpha").unwrap().last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(300));
		drop(remembered);

		assert_eq!(pick_next_remembered_subreddit_to_refresh().as_deref(), Some("gamma"));
	}

	#[test]
	fn remembered_refresh_priority_applies_failure_backoff() {
		let _guard = test_lock();
		reset_test_caches();

		let mut steady = remembered_entry("steady");
		steady.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(50_000));
		steady.estimated_post_interval_secs = 1_000;

		let mut failing = remembered_entry("failing");
		failing.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(50_000));
		failing.estimated_post_interval_secs = 1_000;
		failing.consecutive_failures = 4;

		assert_eq!(remembered_refresh_priority(&steady, &failing), Ordering::Less);
	}

	#[test]
	fn publish_plain_subreddit_refresh_updates_shared_cache_and_stats() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		{
			let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
			let entry = remembered.get_mut("rust").unwrap();
			entry.refresh_in_flight = true;
			entry.consecutive_failures = 3;
		}

		publish_plain_subreddit_refresh(
			"rust",
			vec![
				test_post("one", "rust", 600),
				test_post("two", "rust", 480),
				test_post("three", "rust", 360),
				test_post("four", "rust", 240),
			],
			"t3_after".to_string(),
			RefreshOrigin::FrontPageSession,
		);

		assert!(get_cached_source_page(&FeedSource::Subreddit { name: "rust".to_string() }, 0, 25).is_some());
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("rust").unwrap();
		assert!(entry.last_refresh_succeeded_at.is_some());
		assert_eq!(entry.consecutive_failures, 0);
		assert!(!entry.refresh_in_flight);
		assert_eq!(entry.estimated_post_interval_secs, 120);
	}

	#[test]
	fn failure_recording_increments_backoff_and_clears_in_flight() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		{
			let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
			let entry = remembered.get_mut("rust").unwrap();
			entry.refresh_in_flight = true;
		}

		record_plain_subreddit_refresh_failure("rust", "boom", RefreshOrigin::Daemon);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("rust").unwrap();
		assert_eq!(entry.consecutive_failures, 1);
		assert!(entry.last_refresh_failed_at.is_some());
		assert!(!entry.refresh_in_flight);
	}

	#[test]
	fn failure_recording_uses_long_backoff_for_permanent_failures() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		record_plain_subreddit_refresh_failure("rust", "404 not found", RefreshOrigin::Daemon);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("rust").unwrap();
		let banned_until = entry.banned_until.expect("permanent backoff");
		let remaining = banned_until.saturating_duration_since(Instant::now());
		assert!(remaining <= Duration::from_secs(PERMANENT_FAILURE_BACKOFF_SECS));
		assert!(remaining > Duration::from_secs(PERMANENT_FAILURE_BACKOFF_SECS.saturating_sub(5)));
	}

	#[test]
	fn failure_recording_uses_banned_backoff_for_banned_subreddits() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		record_plain_subreddit_refresh_failure("rust", "banned", RefreshOrigin::Daemon);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("rust").unwrap();
		let banned_until = entry.banned_until.expect("banned backoff");
		let remaining = banned_until.saturating_duration_since(Instant::now());
		assert!(remaining <= Duration::from_secs(BANNED_REFRESH_BACKOFF_SECS));
		assert!(remaining > Duration::from_secs(BANNED_REFRESH_BACKOFF_SECS.saturating_sub(5)));
	}

	#[test]
	fn active_browser_sessions_expire_after_ttl() {
		let _guard = test_lock();
		reset_test_caches();

		ACTIVE_BROWSER_SESSIONS.lock().unwrap().insert(
			"visible".to_string(),
			ActiveBrowserSession {
				path: "/r/rust".to_string(),
				visible: true,
				last_seen: Instant::now() - Duration::from_secs(ACTIVE_BROWSER_SESSION_TTL_SECS + 1),
			},
		);

		assert!(!has_active_browser_sessions());
		assert!(ACTIVE_BROWSER_SESSIONS.lock().unwrap().is_empty());
	}

	#[test]
	fn hidden_browser_sessions_do_not_pause_daemon() {
		let _guard = test_lock();
		reset_test_caches();

		ACTIVE_BROWSER_SESSIONS.lock().unwrap().insert(
			"hidden".to_string(),
			ActiveBrowserSession {
				path: "/r/rust".to_string(),
				visible: false,
				last_seen: Instant::now(),
			},
		);

		assert!(!has_active_browser_sessions());
		assert!(ACTIVE_BROWSER_SESSIONS.lock().unwrap().is_empty());
	}

	#[test]
	fn visible_browser_sessions_pause_daemon_until_they_expire() {
		let _guard = test_lock();
		reset_test_caches();

		ACTIVE_BROWSER_SESSIONS.lock().unwrap().insert(
			"visible".to_string(),
			ActiveBrowserSession {
				path: "/r/rust".to_string(),
				visible: true,
				last_seen: Instant::now(),
			},
		);

		assert!(has_active_browser_sessions());
		assert_eq!(active_browser_session_paths(), vec!["/r/rust".to_string()]);

		ACTIVE_BROWSER_SESSIONS.lock().unwrap().get_mut("visible").unwrap().last_seen = Instant::now() - Duration::from_secs(ACTIVE_BROWSER_SESSION_TTL_SECS + 1);

		assert!(!has_active_browser_sessions());
		assert!(active_browser_session_paths().is_empty());
	}

	#[test]
	fn gets_cached_pages_for_user_sources() {
		let _guard = test_lock();
		reset_test_caches();
		let user_source = FeedSource::Subreddit { name: "u_demo".to_string() };
		let posts = (0..30)
			.map(|index| test_post(&index.to_string(), "u_demo", chrono::Utc::now().timestamp() as u64 - index as u64))
			.collect::<Vec<_>>();
		seed_source(&user_source, posts);

		let cached = get_cached_source_page(&user_source, 0, 25).expect("cached first page");

		assert_eq!(cached.0.len(), 25);
		assert!(!cached.2);
		assert!(cached.3);
	}

	#[test]
	fn merged_posts_prioritize_freshest_ready_heads_even_when_sources_repeat() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let fresh_source = FeedSource::Subreddit { name: "fresh".to_string() };
		let older_source = FeedSource::Subreddit { name: "older".to_string() };
		seed_source(&fresh_source, vec![test_post("fresh-1", "fresh", now), test_post("fresh-2", "fresh", now - 1)]);
		seed_source(&older_source, vec![test_post("older-1", "older", now - 2), test_post("older-2", "older", now - 3)]);

		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![
				session_entry(fresh_source, SessionSourceState::Ready),
				session_entry(older_source, SessionSourceState::Ready),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let ids = merged_posts_for_session(&session).into_iter().take(4).map(|post| post.id).collect::<Vec<_>>();

		assert_eq!(ids, vec!["fresh-1", "fresh-2", "older-1", "older-2"]);
	}

	#[test]
	fn status_message_mentions_followed_feeds_while_users_are_still_warming() {
		let _guard = test_lock();
		reset_test_caches();
		let rust_source = FeedSource::Subreddit { name: "rust".to_string() };
		seed_source(&rust_source, vec![test_post("one", "rust", chrono::Utc::now().timestamp() as u64)]);
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![
				session_entry(FeedSource::Subreddit { name: "u_demo".to_string() }, SessionSourceState::Unstarted),
				session_entry(rust_source, SessionSourceState::Ready),
			],
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let status = session_status_from_posts(&session, &[], 0, 25);

		assert!(status.status_message.contains("Loading followed feeds in the background."));
	}

	#[test]
	fn status_message_mentions_next_page_when_ready() {
		let _guard = test_lock();
		reset_test_caches();
		let post_count = 60;
		let posts = (0..post_count)
			.map(|index| test_post(&index.to_string(), "rust", chrono::Utc::now().timestamp() as u64 - index as u64))
			.collect::<Vec<_>>();
		let rust_source = FeedSource::Subreddit { name: "rust".to_string() };
		seed_source(&rust_source, posts.clone());
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![session_entry(rust_source, SessionSourceState::Ready)],
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let status = session_status_from_posts(&session, &posts, 0, 25);

		assert!(status.next_page_ready);
		assert!(status.status_message.contains("Next page is ready."));
	}

	#[test]
	fn caps_sync_fill_budget_close_to_initial_window() {
		let _guard = test_lock();
		reset_test_caches();
		assert_eq!(initial_source_budget_target(0), 32);
		assert_eq!(max_sync_source_budget(0), 40);
		assert_eq!(initial_source_budget_target(2), 75);
		assert_eq!(max_sync_source_budget(2), 83);
	}

	#[test]
	fn background_target_prefers_loading_more_sources_for_future_pages() {
		let _guard = test_lock();
		reset_test_caches();
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: (0..120)
				.map(|index| {
					session_entry(
						FeedSource::Subreddit {
							name: format!("subreddit{index}"),
						},
						SessionSourceState::Unstarted,
					)
				})
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(background_target_sources(&session), 75);
	}

	#[test]
	fn existing_sessions_do_not_request_a_fresh_initial_window_again() {
		let _guard = test_lock();
		reset_test_caches();
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: (0..40)
				.map(|index| {
					session_entry(
						FeedSource::Subreddit {
							name: format!("subreddit{index}"),
						},
						if index < 32 { SessionSourceState::Ready } else { SessionSourceState::Unstarted },
					)
				})
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(sync_sources_to_request(&session, 0), 0);
		assert_eq!(sync_sources_to_request(&session, 1), 18);
	}

	#[test]
	fn followup_cache_preferred_budget_only_adds_one_small_batch() {
		let _guard = test_lock();
		reset_test_caches();
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: (0..32)
				.map(|index| {
					session_entry(
						FeedSource::Subreddit {
							name: format!("subreddit{index}"),
						},
						if index < 12 { SessionSourceState::Ready } else { SessionSourceState::Unstarted },
					)
				})
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(followup_source_budget(&session), 16);
	}

	#[test]
	fn existing_session_with_ready_requested_page_skips_new_warmup() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		for index in 0..25 {
			let source = FeedSource::Subreddit {
				name: format!("subreddit{index}"),
			};
			seed_source(&source, vec![test_post(&format!("post-{index}"), &format!("subreddit{index}"), now - index as u64)]);
		}

		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: (0..30)
				.map(|index| {
					session_entry(
						FeedSource::Subreddit {
							name: format!("subreddit{index}"),
						},
						if index < 25 { SessionSourceState::Ready } else { SessionSourceState::Unstarted },
					)
				})
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let warm_mode = plan_session_warm_mode(&mut session, 0, POSTS_PER_PAGE, false);

		assert_eq!(warm_mode, None);
		assert!(!session.background_active);
	}

	#[test]
	fn existing_session_with_unready_page_uses_cache_preferred_followup_mode() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		for index in 0..8 {
			let source = FeedSource::Subreddit {
				name: format!("subreddit{index}"),
			};
			seed_source(&source, vec![test_post(&format!("post-{index}"), &format!("subreddit{index}"), now - index as u64)]);
		}

		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: (0..20)
				.map(|index| {
					session_entry(
						FeedSource::Subreddit {
							name: format!("subreddit{index}"),
						},
						if index < 8 { SessionSourceState::Ready } else { SessionSourceState::Unstarted },
					)
				})
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let warm_mode = plan_session_warm_mode(&mut session, 0, POSTS_PER_PAGE, false);

		assert_eq!(warm_mode, Some(SessionWarmMode::FollowupCachePreferred { max_sources: 12 }));
		assert!(session.background_active);
	}

	#[test]
	fn enough_posts_from_too_few_sources_does_not_stop_background_warming() {
		let _guard = test_lock();
		reset_test_caches();
		let posts = (0..100)
			.map(|index| test_post(&format!("post-{index}"), "rust", chrono::Utc::now().timestamp() as u64 - index as u64))
			.collect::<Vec<_>>();
		let hot_source = FeedSource::Subreddit { name: "subreddit0".to_string() };
		seed_source(&hot_source, posts);
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: (0..200)
				.map(|index| {
					session_entry(
						FeedSource::Subreddit {
							name: format!("subreddit{index}"),
						},
						if index < 32 { SessionSourceState::Ready } else { SessionSourceState::Unstarted },
					)
				})
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert!(!background_target_reached(&session));
	}

	#[test]
	fn older_sources_reenter_after_fresher_sources_are_exhausted() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let fresh_source = FeedSource::Subreddit { name: "fresh".to_string() };
		let stale_source = FeedSource::Subreddit { name: "stale".to_string() };
		seed_source(&fresh_source, vec![test_post("fresh-post", "fresh", now - 60)]);
		seed_source(&stale_source, vec![test_post("stale-post", "stale", now - 172_800)]);
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![
				session_entry(fresh_source, SessionSourceState::Ready),
				session_entry(stale_source, SessionSourceState::Ready),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let merged = merged_posts_for_session(&session);
		let ids = merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>();
		assert_eq!(ids, vec!["fresh-post", "stale-post"]);
	}

	#[test]
	fn refresh_token_is_stable_without_session_mutation() {
		let _guard = test_lock();
		reset_test_caches();
		let posts = vec![test_post("one", "rust", chrono::Utc::now().timestamp() as u64)];
		let rust_source = FeedSource::Subreddit { name: "rust".to_string() };
		seed_source(&rust_source, posts.clone());
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![session_entry(rust_source, SessionSourceState::Ready)],
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 42,
		};

		let first = session_status_from_posts(&session, &posts, 0, 25).refresh_token;
		let second = session_status_from_posts(&session, &posts, 0, 25).refresh_token;

		assert_eq!(first, second);
	}

	#[test]
	fn mark_fetching_sources_failed_recovers_stuck_session_entries() {
		let _guard = test_lock();
		reset_test_caches();
		let ready_source = FeedSource::Subreddit { name: "ready".to_string() };
		seed_source(&ready_source, vec![test_post("one", "ready", chrono::Utc::now().timestamp() as u64)]);
		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![
				session_entry(ready_source, SessionSourceState::Ready),
				session_entry(FeedSource::Subreddit { name: "stuck".to_string() }, SessionSourceState::Fetching),
			],
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(count_sources_in_state(&session, SessionSourceState::Fetching), 1);
		mark_fetching_sources_failed(&mut session);

		assert_eq!(count_sources_in_state(&session, SessionSourceState::Fetching), 0);
		assert_eq!(count_sources_in_state(&session, SessionSourceState::Failed), 1);
		assert_eq!(session.sources[1].last_error.as_deref(), Some("task join error"));
	}

	#[test]
	fn background_target_waits_for_more_attempted_sources_than_first_page_uses() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		for index in 0..32 {
			let source = FeedSource::Subreddit {
				name: format!("subreddit{index}"),
			};
			seed_source(&source, vec![test_post(&format!("post-{index}"), &format!("subreddit{index}"), now - index as u64)]);
		}
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: (0..40)
				.map(|index| {
					session_entry(
						FeedSource::Subreddit {
							name: format!("subreddit{index}"),
						},
						if index < 32 { SessionSourceState::Ready } else { SessionSourceState::Unstarted },
					)
				})
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let first_page_sources = merged_posts_for_session(&session)
			.into_iter()
			.take(POSTS_PER_PAGE)
			.map(|post| post.community)
			.collect::<HashSet<_>>();

		assert_eq!(first_page_sources.len(), POSTS_PER_PAGE);
		assert!(attempted_source_count(&session) > first_page_sources.len());
		assert!(!background_target_reached(&session));
	}

	#[test]
	fn round_robin_target_scales_with_requested_page_count() {
		let _guard = test_lock();
		reset_test_caches();
		assert_eq!(source_budget_target(0, POSTS_PER_PAGE), 32);
		assert_eq!(source_budget_target(1, POSTS_PER_PAGE), 50);
		assert_eq!(source_budget_target(2, POSTS_PER_PAGE), 75);
		assert_eq!(initial_source_budget_target(2), 75);
	}

	#[test]
	fn warm_targets_prioritize_never_refreshed_then_recent_requests() {
		let _guard = test_lock();
		reset_test_caches();

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let mut older_refreshed = remembered_entry("older_refreshed");
		older_refreshed.last_requested_at = Instant::now() - Duration::from_secs(30);
		older_refreshed.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(300));
		let mut never_recent = remembered_entry("never_recent");
		never_recent.last_requested_at = Instant::now() - Duration::from_secs(5);
		let mut never_older = remembered_entry("never_older");
		never_older.last_requested_at = Instant::now() - Duration::from_secs(60);
		remembered.insert(older_refreshed.name.clone(), older_refreshed);
		remembered.insert(never_recent.name.clone(), never_recent);
		remembered.insert(never_older.name.clone(), never_older);
		drop(remembered);

		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![
				session_entry(
					FeedSource::Subreddit {
						name: "older_refreshed".to_string(),
					},
					SessionSourceState::Unstarted,
				),
				session_entry(FeedSource::Subreddit { name: "never_older".to_string() }, SessionSourceState::Unstarted),
				session_entry(FeedSource::Subreddit { name: "never_recent".to_string() }, SessionSourceState::Unstarted),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(warm_targets(&session, 3), vec![2, 1, 0]);
	}

	#[test]
	fn warm_targets_break_ties_with_source_priority() {
		let _guard = test_lock();
		reset_test_caches();

		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![
				session_entry(
					FeedSource::Flair {
						subreddit: "rust".to_string(),
						flair: "News".to_string(),
					},
					SessionSourceState::Unstarted,
				),
				session_entry(FeedSource::Subreddit { name: "rust".to_string() }, SessionSourceState::Unstarted),
				session_entry(FeedSource::Subreddit { name: "u_demo".to_string() }, SessionSourceState::Unstarted),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(warm_targets(&session, 3), vec![2, 1, 0]);
	}

	#[test]
	fn merged_posts_skip_duplicate_ids_without_stalling_fresher_sources() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let freshest = FeedSource::Subreddit { name: "freshest".to_string() };
		let duplicate = FeedSource::Subreddit { name: "duplicate".to_string() };
		seed_source(&freshest, vec![test_post("shared", "freshest", now), test_post("fresh-next", "freshest", now - 2)]);
		seed_source(&duplicate, vec![test_post("shared", "duplicate", now - 1), test_post("dup-next", "duplicate", now - 3)]);

		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![session_entry(freshest, SessionSourceState::Ready), session_entry(duplicate, SessionSourceState::Ready)],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let ids = merged_posts_for_session(&session).into_iter().map(|post| post.id).collect::<Vec<_>>();

		assert_eq!(ids, vec!["shared", "fresh-next", "dup-next"]);
	}

	#[test]
	fn ready_pages_remain_stable_after_more_sources_finish_warming() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let older_source = FeedSource::Subreddit { name: "older".to_string() };
		let newer_source = FeedSource::Subreddit { name: "newer".to_string() };
		let older_posts = (0..30)
			.map(|index| test_post(&format!("older-{index}"), "older", now - 1_000 - index as u64))
			.collect::<Vec<_>>();
		let newer_posts = (0..30).map(|index| test_post(&format!("newer-{index}"), "newer", now - index as u64)).collect::<Vec<_>>();
		seed_source(&older_source, older_posts);
		seed_source(&newer_source, newer_posts);

		FEED_SESSIONS.lock().unwrap().insert(
			"demo".to_string(),
			FeedSession {
				id: "demo".to_string(),
				request: FeedSessionRequest {
					sources: vec![],
					filters: HashSet::new(),
					quarantine: false,
					query_fingerprint: "demo".to_string(),
				},
				sources: vec![
					session_entry(older_source.clone(), SessionSourceState::Ready),
					session_entry(newer_source.clone(), SessionSourceState::Unstarted),
				],
				frozen_pages: HashMap::new(),
				background_active: true,
				background_complete: false,
				requested_page: 0,
				last_updated: Instant::now(),
				revision: 0,
			},
		);

		let first_page = feed_page("demo", 0, POSTS_PER_PAGE).expect("initial page");
		let initial_ids = first_page.posts.iter().map(|post| post.id.clone()).collect::<Vec<_>>();
		assert_eq!(initial_ids[0], "newer-0");

		{
			let mut sessions = FEED_SESSIONS.lock().unwrap();
			let session = sessions.get_mut("demo").expect("session");
			session.sources[1].state = SessionSourceState::Ready;
		}

		let second_page = feed_page("demo", 0, POSTS_PER_PAGE).expect("stable page");
		let refreshed_ids = second_page.posts.iter().map(|post| post.id.clone()).collect::<Vec<_>>();

		assert_eq!(refreshed_ids, initial_ids);
	}

	#[test]
	fn feed_page_sorts_selected_posts_newest_first() {
		let _guard = test_lock();
		reset_test_caches();
		let source_a = FeedSource::Subreddit { name: "alpha".to_string() };
		let source_b = FeedSource::Subreddit { name: "beta".to_string() };
		seed_source(&source_a, vec![test_post("alpha-1", "alpha", 100), test_post("alpha-2", "alpha", 97)]);
		seed_source(&source_b, vec![test_post("beta-1", "beta", 99), test_post("beta-2", "beta", 98)]);

		FEED_SESSIONS.lock().unwrap().insert(
			"demo".to_string(),
			FeedSession {
				id: "demo".to_string(),
				request: FeedSessionRequest {
					sources: vec![],
					filters: HashSet::new(),
					quarantine: false,
					query_fingerprint: "demo".to_string(),
				},
				sources: vec![session_entry(source_a, SessionSourceState::Ready), session_entry(source_b, SessionSourceState::Ready)],
				frozen_pages: HashMap::new(),
				background_active: false,
				background_complete: true,
				requested_page: 0,
				last_updated: Instant::now(),
				revision: 0,
			},
		);

		let page = feed_page("demo", 0, 4).expect("page");
		let ids = page.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>();

		assert_eq!(ids, vec!["alpha-1", "beta-1", "beta-2", "alpha-2"]);
	}

	#[test]
	fn reconciles_new_session_sources_from_existing_shared_cache() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let rust_source = FeedSource::Subreddit { name: "rust".to_string() };
		let golang_source = FeedSource::Subreddit { name: "golang".to_string() };
		seed_source(&rust_source, vec![test_post("rust-1", "rust", now)]);
		seed_source(&golang_source, vec![test_post("go-1", "golang", now - 1)]);

		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![rust_source.clone(), golang_source.clone()],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![
				session_entry(rust_source, SessionSourceState::Unstarted),
				session_entry(golang_source, SessionSourceState::Failed),
			],
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		reconcile_session_with_source_cache(&mut session);

		assert_eq!(count_sources_in_state(&session, SessionSourceState::Ready), 2);
		assert_eq!(count_sources_in_state(&session, SessionSourceState::Unstarted), 0);
		assert_eq!(count_sources_in_state(&session, SessionSourceState::Failed), 0);
	}

	#[test]
	fn feed_status_counts_cached_sources_for_new_session() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let rust_source = FeedSource::Subreddit { name: "rust".to_string() };
		let programming_source = FeedSource::Subreddit { name: "programming".to_string() };
		seed_source(&rust_source, vec![test_post("rust-1", "rust", now)]);
		seed_source(&programming_source, vec![test_post("prog-1", "programming", now - 1)]);

		FEED_SESSIONS.lock().unwrap().insert(
			"demo".to_string(),
			FeedSession {
				id: "demo".to_string(),
				request: FeedSessionRequest {
					sources: vec![rust_source.clone(), programming_source.clone()],
					filters: HashSet::new(),
					quarantine: false,
					query_fingerprint: "demo".to_string(),
				},
				sources: vec![
					session_entry(rust_source, SessionSourceState::Unstarted),
					session_entry(programming_source, SessionSourceState::Unstarted),
				],
				frozen_pages: HashMap::new(),
				background_active: true,
				background_complete: false,
				requested_page: 0,
				last_updated: Instant::now(),
				revision: 0,
			},
		);

		let status = feed_status("demo", 0, POSTS_PER_PAGE).expect("session status");

		assert_eq!(status.ready_sources, 2);
		assert_eq!(status.daemon_cached_sources, 0);
		assert_eq!(status.total_sources, 2);
	}

	#[test]
	fn feed_status_counts_daemon_cached_sources_and_servable_remembered_entries() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		remember_subreddit_request("rust");
		let rust_source = FeedSource::Subreddit { name: "rust".to_string() };
		store_source_posts_with_origin(&rust_source, vec![test_post("rust-1", "rust", now)], String::new(), Some(RefreshOrigin::Daemon));
		SOURCE_CACHE.lock().unwrap().get_mut(&rust_source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		FEED_SESSIONS.lock().unwrap().insert(
			"demo".to_string(),
			FeedSession {
				id: "demo".to_string(),
				request: FeedSessionRequest {
					sources: vec![rust_source.clone()],
					filters: HashSet::new(),
					quarantine: false,
					query_fingerprint: "demo".to_string(),
				},
				sources: vec![session_entry(rust_source, SessionSourceState::Unstarted)],
				frozen_pages: HashMap::new(),
				background_active: false,
				background_complete: false,
				requested_page: 0,
				last_updated: Instant::now(),
				revision: 0,
			},
		);

		let status = feed_status("demo", 0, POSTS_PER_PAGE).expect("session status");
		assert_eq!(status.ready_sources, 1);
		assert_eq!(status.daemon_cached_sources, 1);
		assert!(status.status_message.contains("1 from daemon cache"));
		assert_eq!(feed_page("demo", 0, POSTS_PER_PAGE).expect("page").posts.len(), 1);
	}

	#[test]
	fn cache_status_reports_head_post_freshness_fields() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let rust_source = FeedSource::Subreddit { name: "rust".to_string() };
		seed_source(&rust_source, vec![test_post("rust-1", "rust", now - 90), test_post("rust-2", "rust", now - 120)]);

		let prefs = Preferences {
			subscriptions: vec!["rust".to_string()],
			..Preferences::default()
		};

		let status = cache_status_for_preferences(&prefs);
		let source = status.sources.iter().find(|source| source.display_name == "rust").expect("rust cache status");

		assert_eq!(source.head_post_created_ts, Some(now - 90));
		assert!(source.head_post_age_secs.is_some_and(|age| age >= 90));
	}

	#[test]
	fn banned_refreshes_are_deprioritized_until_backoff_expires() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("vlogsex");
		remember_subreddit_request("rust");
		record_plain_subreddit_refresh_failure("vlogsex", "banned", RefreshOrigin::Daemon);

		assert_eq!(pick_next_remembered_subreddit_to_refresh().as_deref(), Some("rust"));

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("vlogsex").unwrap();
		assert!(entry.banned_until.is_some_and(|until| until > Instant::now() + Duration::from_secs(24 * 60 * 60)));
	}

	#[test]
	fn persisted_restore_respects_remembered_retention_rules() {
		let _guard = test_lock();
		reset_test_caches();

		let stale_source = FeedSource::Subreddit { name: "rust".to_string() };
		store_source_posts_with_origin(&stale_source, vec![test_post("one", "rust", 100)], "after".to_string(), None);
		SOURCE_CACHE.lock().unwrap().get_mut(&stale_source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let stale_snapshot = snapshot_persisted_source_entries(1_000_000);
		SOURCE_CACHE.lock().unwrap().clear();
		restore_persisted_state(stale_snapshot, Vec::new(), 1_000_000, 1_000_000);
		assert!(SOURCE_CACHE.lock().unwrap().is_empty());

		remember_subreddit_request("remembered");
		let remembered_source = FeedSource::Subreddit { name: "remembered".to_string() };
		store_source_posts_with_origin(&remembered_source, vec![test_post("two", "remembered", 200)], "after".to_string(), None);
		SOURCE_CACHE.lock().unwrap().get_mut(&remembered_source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let remembered_snapshot = snapshot_persisted_source_entries(1_000_100);
		let remembered_state = snapshot_persisted_remembered_subreddits(1_000_100);
		SOURCE_CACHE.lock().unwrap().clear();
		REMEMBERED_SUBREDDITS.lock().unwrap().clear();
		restore_persisted_state(remembered_snapshot, remembered_state, 1_000_100, 1_000_100);
		assert!(SOURCE_CACHE.lock().unwrap().contains_key(&remembered_source.cache_key()));
		assert!(REMEMBERED_SUBREDDITS.lock().unwrap().contains_key("remembered"));
	}
}
