use crate::cache_persistence::{
	instant_to_unix, option_instant_to_unix, option_unix_to_instant, unix_to_instant, PersistedRefreshOrigin, PersistedRememberedSubredditEntry, PersistedSourceCacheEntry,
	PersistedSourceUnavailableState,
};
use crate::materialized_catalog;
use crate::oauth::oauth_is_available;
use crate::user::{collapse_duplicate_submissions, post_feed_activity_ts, update_user_video_stats_from_posts};
use crate::utils::{Post, Preferences};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hyper::body::to_bytes;
use hyper::{Body, Request, Response, StatusCode};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Instant;
use tokio::time::{Duration, MissedTickBehavior};

pub(crate) const CACHE_TTL_SECS: u64 = 10_800;
const MAX_FEED_SESSIONS: usize = 64;
const MAX_FROZEN_FEED_PAGES: usize = 16;
pub(crate) const REMEMBERED_SUBREDDIT_TTL_SECS: u64 = 3 * 24 * 60 * 60;
pub(crate) const REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS: u64 = 5 * 60;
const REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK: usize = 3;
const REMEMBERED_SUBREDDIT_DAEMON_MAX_ATTEMPTS_PER_TICK: usize = 20;
const REMEMBERED_SUBREDDIT_DAEMON_MAX_UNPROVEN_PERMANENT_FAILURES_PER_TICK: usize = 3;
/// Visit-interest boost decays with this half-life so manual browsing lifts refresh priority temporarily.
const VISIT_PRIORITY_HALFLIFE_SECS: u64 = 72 * 60 * 60;
/// Minimum time between visit signals for the same feed (pagination / rapid reload spam guard).
const VISIT_SIGNAL_COOLDOWN_SECS: u64 = 45 * 60;
const VISIT_PRIORITY_BUMP: f64 = 1.0;
const VISIT_PRIORITY_MAX_SCORE: f64 = 16.0;
/// Maps decaying visit score into `effective_freshness_target` reduction: target *= 1/(1 + k*score).
const VISIT_PRIORITY_SENSITIVITY: f64 = 0.35;
/// Rating feedback has a smaller, separate cache-priority effect to reduce feedback-loop risk.
const FEEDBACK_PRIORITY_HALFLIFE_SECS: u64 = 7 * 24 * 60 * 60;
const FEEDBACK_PRIORITY_BUMP: f64 = 0.30;
const FEEDBACK_PRIORITY_MAX_SCORE: f64 = 3.0;
const FEEDBACK_PRIORITY_MIN_SCORE: f64 = -3.0;
const FEEDBACK_PRIORITY_SENSITIVITY: f64 = 0.08;
const RECENT_REQUEST_PRIORITY_SECS: u64 = 6 * 60 * 60;
const DURABLE_UNAVAILABLE_BACKOFF_SECS: u64 = 30 * 24 * 60 * 60;
/// One in every four daemon selections is the least-recently-served eligible
/// source. The other three retain cadence, overdue, visit and feedback priority.
const FAIRNESS_SELECTION_STRIDE: u64 = 4;
const UNFOLLOWED_DECAY_GRACE_SECS: u64 = 6 * 60 * 60;
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
const BACKGROUND_LOOKAHEAD_PAGES: usize = 1;
const BACKGROUND_DIVERSITY_SOURCE_CAP: usize = 12;
pub const POSTS_PER_PAGE: usize = 25;
const DEFAULT_SOURCE_RESIDENT_TTL_SECS: u64 = 15 * 60;
const DEFAULT_SOURCE_CACHE_MAX_ENTRIES: usize = 128;
const DEFAULT_SOURCE_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Max time the server waits for a full contextual page (or background complete) before
/// sealing whatever merged posts exist so the HTML/fragment response does not keep changing.
const CONTEXTUAL_FEED_PAINT_WAIT: Duration = Duration::from_secs(12);

pub const fn per_source_limit() -> usize {
	PER_SOURCE_LIMIT
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FeedSort {
	New,
	Hot,
	Random,
}

impl FeedSort {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::New => "new",
			Self::Hot => "hot",
			Self::Random => "random",
		}
	}

	pub fn from_listing(sort: &str) -> Self {
		match sort {
			"hot" => Self::Hot,
			"random" => Self::Random,
			_ => Self::New,
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FeedSource {
	Subreddit {
		name: String,
		sort: FeedSort,
	},
	/// `/user/{name}/comments.json` for the same `u_{name}` subscription pseudo-feed.
	UserComments {
		name: String,
		sort: FeedSort,
	},
	Flair {
		subreddit: String,
		flair: String,
		sort: FeedSort,
	},
}

impl FeedSource {
	pub fn subreddit(name: impl Into<String>, sort: FeedSort) -> Self {
		Self::Subreddit { name: name.into(), sort }
	}

	pub fn user_comments(name: impl Into<String>, sort: FeedSort) -> Self {
		Self::UserComments { name: name.into(), sort }
	}

	pub fn flair(subreddit: impl Into<String>, flair: impl Into<String>, sort: FeedSort) -> Self {
		Self::Flair {
			subreddit: subreddit.into(),
			flair: flair.into(),
			sort,
		}
	}

	pub fn sort(&self) -> FeedSort {
		match self {
			Self::Subreddit { sort, .. } | Self::UserComments { sort, .. } | Self::Flair { sort, .. } => *sort,
		}
	}

	pub fn display_name(&self) -> String {
		match self {
			Self::Subreddit { name, .. } => name.clone(),
			Self::UserComments { name, .. } => format!("{name} (comments)"),
			Self::Flair { subreddit, flair, .. } => format!("{subreddit}#{flair}"),
		}
	}

	pub(crate) fn cache_key(&self) -> String {
		match self {
			Self::Subreddit { name, sort } => format!("subreddit:{}:{}", name.to_lowercase(), sort.as_str()),
			Self::UserComments { name, sort } => format!("user_comments:{}:{}", name.to_lowercase(), sort.as_str()),
			Self::Flair { subreddit, flair, sort } => {
				format!("flair:{}:{}:{}", subreddit.to_lowercase(), flair.to_lowercase(), sort.as_str())
			}
		}
	}

	fn build_path(&self) -> String {
		match self {
			Self::Subreddit { name, sort } => {
				let lower = name.to_lowercase();
				if let Some(username) = lower.strip_prefix("u_").filter(|rest| !rest.is_empty()) {
					format!("/user/{username}/submitted.json?sort={}&limit={PER_SOURCE_LIMIT}&raw_json=1", sort.as_str())
				} else {
					format!("/r/{}/{}.json?limit={PER_SOURCE_LIMIT}&raw_json=1", name.replace('+', "%2B"), sort.as_str())
				}
			}
			Self::UserComments { name, sort } => {
				let lower = name.to_lowercase();
				let username = lower.strip_prefix("u_").filter(|rest| !rest.is_empty()).unwrap_or(lower.as_str());
				format!("/user/{username}/comments.json?sort={}&limit={PER_SOURCE_LIMIT}&raw_json=1", sort.as_str())
			}
			Self::Flair { subreddit, sort, .. } => {
				format!("/r/{}/{}.json?limit={PER_SOURCE_LIMIT}&raw_json=1", subreddit.replace('+', "%2B"), sort.as_str())
			}
		}
	}

	fn public_path(&self) -> String {
		match self {
			Self::Subreddit { name, sort } => {
				let lower = name.to_lowercase();
				if let Some(username) = lower.strip_prefix("u_").filter(|rest| !rest.is_empty()) {
					format!("/user/{username}/{}", sort.as_str())
				} else {
					format!("/r/{}/{}", name.replace('+', "%2B"), sort.as_str())
				}
			}
			Self::UserComments { name, sort } => {
				let lower = name.to_lowercase();
				let username = lower.strip_prefix("u_").filter(|rest| !rest.is_empty()).unwrap_or(lower.as_str());
				format!("/user/{username}/comments/{}", sort.as_str())
			}
			Self::Flair { subreddit, flair, sort } => format!(
				"/r/{}/{}?flair={}",
				subreddit.replace('+', "%2B"),
				sort.as_str(),
				crate::subreddit::encode_query_value(flair)
			),
		}
	}

	fn filter_posts(&self, mut posts: Vec<Post>) -> Vec<Post> {
		match self {
			Self::Subreddit { .. } | Self::UserComments { .. } => posts,
			Self::Flair { flair, .. } => {
				let flair = flair.trim().to_string();
				posts.retain(|post| post.flair.text.trim() == flair);
				posts
			}
		}
	}
}

fn source_from_cache_key(key: &str) -> Option<FeedSource> {
	if let Some(name) = key.strip_prefix("subreddit:") {
		let (name, sort) = name.rsplit_once(':').map_or((name, FeedSort::New), |(name, sort)| (name, FeedSort::from_listing(sort)));
		Some(FeedSource::subreddit(name.to_string(), sort))
	} else if let Some(name) = key.strip_prefix("user_comments:") {
		let (name, sort) = name.rsplit_once(':').map_or((name, FeedSort::New), |(name, sort)| (name, FeedSort::from_listing(sort)));
		Some(FeedSource::user_comments(name.to_string(), sort))
	} else if let Some(rest) = key.strip_prefix("flair:") {
		let (subreddit, flair_and_sort) = rest.split_once(':')?;
		let (flair, sort) = flair_and_sort
			.rsplit_once(':')
			.map_or((flair_and_sort, FeedSort::New), |(flair, sort)| (flair, FeedSort::from_listing(sort)));
		Some(FeedSource::flair(subreddit.to_string(), flair.to_string(), sort))
	} else {
		None
	}
}

#[derive(Clone)]
struct CachedSourcePosts {
	source: FeedSource,
	posts: Vec<Post>,
	after: String,
	fetched_at: Instant,
	last_accessed: Instant,
	refresh_origin: Option<RefreshOrigin>,
	estimated_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct SourceRefreshToken {
	cache_key: String,
	generation: u64,
	lifecycle: Arc<SourceRefreshLifecycle>,
}

struct SourceRefreshLifecycle {
	cache_key: String,
	generation: u64,
	state: Arc<Mutex<u64>>,
	remembered_key: Option<String>,
}

impl Drop for SourceRefreshLifecycle {
	fn drop(&mut self) {
		let mut remembered_changed = false;
		let current_generation = self.state.lock().unwrap_or_else(|error| error.into_inner());
		if *current_generation == self.generation {
			if let Some(remembered_key) = self.remembered_key.as_ref() {
				let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner());
				if let Some(entry) = remembered.get_mut(remembered_key) {
					remembered_changed = entry.refresh_in_flight;
					entry.refresh_in_flight = false;
				}
			}
		}
		drop(current_generation);
		if remembered_changed {
			crate::cache_persistence::mark_dirty();
		}
		release_source_refresh_state(&self.cache_key, &self.state);
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceRefreshOutcome {
	Committed,
	Stale,
	Failed,
}

impl SourceRefreshOutcome {
	pub(crate) fn committed(self) -> bool {
		self == Self::Committed
	}
}

#[derive(Clone, Debug)]
pub struct RememberedSubredditEntry {
	pub name: String,
	pub last_requested_at: Instant,
	pub last_refresh_started_at: Option<Instant>,
	pub last_refresh_succeeded_at: Option<Instant>,
	pub last_refresh_failed_at: Option<Instant>,
	pub banned_until: Option<Instant>,
	pub unavailable_reason: Option<String>,
	pub consecutive_failures: u32,
	pub estimated_post_interval_secs: u64,
	pub refresh_in_flight: bool,
	/// Accumulated visit-interest score (decays with [`VISIT_PRIORITY_HALFLIFE_SECS`] wall-clock).
	pub visit_priority_score: f64,
	pub visit_priority_last_update_at: Option<Instant>,
	pub visit_signal_last_applied_at: Option<Instant>,
	/// Accumulated rating-feedback score. Positive values mildly promote, negative values mildly defer.
	pub feedback_priority_score: f64,
	pub feedback_priority_last_update_at: Option<Instant>,
	pub profile_followers: usize,
	pub unfollowed_decay: bool,
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
	DurableUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RememberedRefreshOutcome {
	Succeeded,
	Failed(RememberedFailureDisposition),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionSourceState {
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
	pub cached_posts: usize,
	pub estimated_cached_post_pages: usize,
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
	pub feed_url: String,
	pub daemon_managed: bool,
	pub cached: bool,
	pub cached_posts: usize,
	pub cached_age_secs: Option<u64>,
	/// Unix time of the most recent cached post activity (`post_feed_activity_ts`), not the first row.
	pub head_post_created_ts: Option<u64>,
	pub head_post_age_secs: Option<u64>,
	pub cache_expires_in_secs: Option<u64>,
	pub last_refresh_origin: Option<String>,
	pub last_refresh_succeeded_secs_ago: Option<u64>,
	pub last_refresh_failed_secs_ago: Option<u64>,
	pub refresh_frequency_secs: Option<u64>,
	pub schedule_state: String,
	pub queue_position: Option<usize>,
	pub predicted_next_fetch_in_secs: Option<u64>,
	pub blocked_until_secs: Option<u64>,
	pub retry_at_unix: Option<u64>,
	pub unavailable_reason: Option<String>,
	pub scheduling_reason: String,
	pub fairness_slot: bool,
	pub lifecycle_state: String,
	pub profile_followers: usize,
	pub consecutive_failures: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct CacheStatusResponse {
	pub generated_at_unix: u64,
	pub summary: CacheStatusSummary,
	pub sources: Vec<CacheStatusSource>,
}

#[derive(Clone, Copy)]
struct ResidentCacheStatusSnapshot {
	cached_posts: usize,
	cached_age_secs: u64,
	head_post_created_ts: Option<u64>,
	refresh_origin: Option<RefreshOrigin>,
}

#[derive(Clone)]
pub(crate) struct SessionSourceEntry {
	pub(crate) source: FeedSource,
	pub(crate) state: SessionSourceState,
	pub(crate) last_error: Option<String>,
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
	primary_metric: i128,
	secondary_metric: u64,
	fetched_at: Instant,
	source_priority: (u8, String, String),
}

impl Ord for ReadyQueueHead {
	fn cmp(&self, other: &Self) -> Ordering {
		self
			.primary_metric
			.cmp(&other.primary_metric)
			.then_with(|| self.secondary_metric.cmp(&other.secondary_metric))
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
	pub profile_id: String,
	pub cache_revision: u64,
	pub bundle_followed_user_comments: bool,
	pub daemon_discovery_order: bool,
	pub random_rotation_seed: u64,
	/// When set, merge / finalize use this sort while [`FeedSource::sort`] may differ (e.g. `/following/hot`
	/// loads `New` listings to match daemon-cached `u_` keys).
	pub merge_sort: Option<FeedSort>,
}

impl Default for FeedSessionRequest {
	fn default() -> Self {
		Self {
			sources: Vec::new(),
			filters: HashSet::new(),
			quarantine: false,
			query_fingerprint: String::new(),
			profile_id: String::new(),
			cache_revision: 0,
			bundle_followed_user_comments: false,
			daemon_discovery_order: false,
			random_rotation_seed: 0,
			merge_sort: None,
		}
	}
}

#[derive(Clone)]
pub(crate) struct FeedSession {
	pub(crate) id: String,
	pub(crate) request: FeedSessionRequest,
	pub(crate) sources: Vec<SessionSourceEntry>,
	pub(crate) frozen_pages: HashMap<usize, Vec<Post>>,
	pub(crate) background_active: bool,
	pub(crate) background_complete: bool,
	pub(crate) requested_page: usize,
	pub(crate) last_updated: Instant,
	pub(crate) revision: u64,
}

#[derive(Clone)]
pub struct FeedPage {
	pub posts: Vec<Post>,
	pub has_prev: bool,
	pub has_next: bool,
	pub ready: bool,
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

#[derive(Clone, Debug)]
pub struct FeedSessionPageMetric {
	pub page: usize,
	pub full_page: bool,
	pub ready: bool,
	pub representative_created_ts: Option<u64>,
	pub representative_score: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionWarmMode {
	InitialLoad,
	FollowupCachePreferred { max_sources: usize },
}

static SOURCE_CACHE: LazyLock<Mutex<HashMap<String, CachedSourcePosts>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SOURCE_REFRESH_STATES: LazyLock<Mutex<HashMap<String, Weak<Mutex<u64>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static FEED_SESSIONS: LazyLock<Mutex<HashMap<String, FeedSession>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
type SqliteProfileSyncTarget = (Vec<(String, i64)>, Vec<String>);
static SQLITE_PROFILE_SYNC_DESIRED: LazyLock<Mutex<HashMap<String, SqliteProfileSyncTarget>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
struct SqliteSessionCursorState {
	last_accessed: Instant,
	after_page: HashMap<usize, crate::cache_sqlite::TimelineCursor>,
}

static SQLITE_SESSION_CURSORS: LazyLock<Mutex<HashMap<String, SqliteSessionCursorState>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
pub static REMEMBERED_SUBREDDITS: LazyLock<Mutex<HashMap<String, RememberedSubredditEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static ACTIVE_BROWSER_SESSIONS: LazyLock<Mutex<HashMap<String, ActiveBrowserSession>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static REMEMBERED_SUBREDDIT_DAEMON_NEXT_TICK_AT: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));
static REMEMBERED_DAEMON_SELECTION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PROFILE_SOURCE_MEMBERSHIPS: LazyLock<Mutex<HashMap<String, HashSet<String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SOURCE_CACHE_REVISION: AtomicU64 = AtomicU64::new(0);
static DELETED_MEDIA_TOMBSTONES: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static DELETED_MEDIA_PERSIST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static DELETED_MEDIA_SAFETY_HEALTHY: AtomicBool = AtomicBool::new(true);
#[cfg(test)]
static TEST_STATE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
#[cfg(test)]
static TEST_DURABLE_WRITE_DELAY_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_DURABLE_WRITE_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_DURABLE_WRITE_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_DURABLE_WRITE_FAIL: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_TOMBSTONE_WRITE_FAIL: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_TOMBSTONE_MARKER_WRITE_FAIL: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static TEST_PRUNE_REMOVE_DELAY_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static TEST_PRUNE_REMOVE_ACTIVE: AtomicBool = AtomicBool::new(false);

fn source_cache_is_expired(entry: &CachedSourcePosts) -> bool {
	entry.fetched_at.elapsed().as_secs() > CACHE_TTL_SECS
}

fn source_resident_ttl_secs() -> u64 {
	std::env::var("REDLIB_SOURCE_CACHE_RESIDENT_TTL_SECS")
		.ok()
		.and_then(|value| value.parse::<u64>().ok())
		.unwrap_or(DEFAULT_SOURCE_RESIDENT_TTL_SECS)
}

fn source_cache_max_entries() -> usize {
	std::env::var("REDLIB_SOURCE_CACHE_MAX_ENTRIES")
		.ok()
		.and_then(|value| value.parse::<usize>().ok())
		.filter(|value| *value > 0)
		.unwrap_or(DEFAULT_SOURCE_CACHE_MAX_ENTRIES)
}

fn source_cache_max_bytes() -> usize {
	std::env::var("REDLIB_SOURCE_CACHE_MAX_BYTES")
		.ok()
		.and_then(|value| value.parse::<usize>().ok())
		.filter(|value| *value > 0)
		.unwrap_or(DEFAULT_SOURCE_CACHE_MAX_BYTES)
}

fn estimated_source_bytes(posts: &[Post], after: &str) -> usize {
	let serialized = bincode::serialized_size(&(posts, after))
		.ok()
		.and_then(|value| usize::try_from(value).ok())
		.unwrap_or_else(|| posts.len().saturating_mul(4 * 1024).saturating_add(after.len()));
	serialized
		.saturating_add(std::mem::size_of::<CachedSourcePosts>())
		.saturating_add(std::mem::size_of_val(posts))
}

fn apply_legacy_mixed_feed_source_refresh(source: &FeedSource, posts: &[Post]) {
	if !crate::cache_sqlite::reads_enabled() {
		materialized_catalog::apply_mixed_feed_source_refresh(source, posts);
	}
}

fn remembered_entry_is_active(entry: &RememberedSubredditEntry) -> bool {
	entry.last_requested_at.elapsed().as_secs() <= REMEMBERED_SUBREDDIT_TTL_SECS
}

fn valid_plain_subreddit_name(name: &str) -> bool {
	(3..=21).contains(&name.len()) && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_reddit_username(name: &str) -> bool {
	(3..=20).contains(&name.len()) && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn normalize_plain_subreddit_name(subreddit: &str) -> Option<String> {
	let normalized = subreddit.trim().to_lowercase();
	(valid_plain_subreddit_name(&normalized)
		&& normalized != "popular"
		&& normalized != "all"
		&& normalized != "random"
		&& normalized != "randnsfw"
		&& !normalized.starts_with("u_"))
	.then_some(normalized)
}

fn normalize_user_subscription_name(name: &str) -> Option<String> {
	let normalized = name.trim().to_lowercase();
	let username = normalized.strip_prefix("u_").unwrap_or(normalized.as_str());
	valid_reddit_username(username).then(|| format!("u_{username}"))
}

/// Normalized key for remembered feeds: plain subreddits and `u_username` pseudo-feeds (subscriptions / user listing cache).
fn normalize_remembered_feed_name(name: &str) -> Option<String> {
	let normalized = name.trim().to_lowercase();
	if normalized.is_empty() || normalized == "popular" || normalized == "all" || normalized == "random" || normalized == "randnsfw" || normalized.contains('+') {
		return None;
	}
	if normalized.starts_with("u_") {
		normalize_user_subscription_name(&normalized)
	} else {
		normalize_plain_subreddit_name(&normalized)
	}
}

fn normalize_remembered_source(source: &FeedSource) -> Option<FeedSource> {
	match source {
		FeedSource::Subreddit { name, sort } if *sort == FeedSort::New => Some(FeedSource::subreddit(normalize_remembered_feed_name(name)?, FeedSort::New)),
		FeedSource::UserComments { name, sort } if *sort == FeedSort::New => Some(FeedSource::user_comments(normalize_user_subscription_name(name)?, FeedSort::New)),
		FeedSource::Flair { subreddit, flair, sort } if *sort == FeedSort::New => {
			let subreddit = normalize_plain_subreddit_name(subreddit)?;
			let flair = flair.trim();
			(!flair.is_empty()).then_some(FeedSource::flair(subreddit, flair.to_string(), FeedSort::New))
		}
		_ => None,
	}
}

fn remembered_feed_key_for_source(source: &FeedSource) -> Option<String> {
	match normalize_remembered_source(source)? {
		FeedSource::Subreddit { name, .. } => Some(name),
		FeedSource::UserComments { name, .. } => Some(format!("user_comments:{name}")),
		FeedSource::Flair { subreddit, flair, .. } => Some(format!("flair:{subreddit}:{}", URL_SAFE_NO_PAD.encode(flair.as_bytes()))),
	}
}

fn remembered_feed_source_from_key(key: &str) -> Option<FeedSource> {
	if let Some(name) = key.strip_prefix("user_comments:") {
		return normalize_remembered_source(&FeedSource::user_comments(name.to_string(), FeedSort::New));
	}

	if let Some(rest) = key.strip_prefix("flair:") {
		let (subreddit, encoded_flair) = rest.split_once(':')?;
		let flair = String::from_utf8(URL_SAFE_NO_PAD.decode(encoded_flair).ok()?).ok()?;
		return normalize_remembered_source(&FeedSource::flair(subreddit.to_string(), flair, FeedSort::New));
	}

	normalize_remembered_source(&FeedSource::subreddit(key.to_string(), FeedSort::New))
}

fn is_persisted_subreddit_feed_source(source: &FeedSource) -> bool {
	remembered_feed_key_for_source(source).is_some()
}

fn new_remembered_subreddit_entry(name: String) -> RememberedSubredditEntry {
	RememberedSubredditEntry {
		name,
		last_requested_at: Instant::now(),
		last_refresh_started_at: None,
		last_refresh_succeeded_at: None,
		last_refresh_failed_at: None,
		banned_until: None,
		unavailable_reason: None,
		consecutive_failures: 0,
		estimated_post_interval_secs: DEFAULT_ESTIMATED_POST_INTERVAL_SECS,
		refresh_in_flight: false,
		visit_priority_score: 0.0,
		visit_priority_last_update_at: None,
		visit_signal_last_applied_at: None,
		feedback_priority_score: 0.0,
		feedback_priority_last_update_at: None,
		profile_followers: 0,
		unfollowed_decay: false,
	}
}

fn elapsed_secs_at(now: Instant, then: Instant) -> u64 {
	now.saturating_duration_since(then).as_secs()
}

fn decayed_priority_score_at(score: f64, anchor: Option<Instant>, half_life_secs: u64, now: Instant) -> f64 {
	let Some(anchor) = anchor else {
		return 0.0;
	};
	if !score.is_finite() {
		return 0.0;
	}
	let dt = now.saturating_duration_since(anchor).as_secs_f64();
	let lambda = std::f64::consts::LN_2 / half_life_secs as f64;
	score * (-lambda * dt).exp()
}

fn decayed_priority_score(score: f64, anchor: Option<Instant>, half_life_secs: u64) -> f64 {
	decayed_priority_score_at(score, anchor, half_life_secs, Instant::now())
}

fn visit_priority_score_at(entry: &RememberedSubredditEntry, now: Instant) -> f64 {
	decayed_priority_score_at(entry.visit_priority_score, entry.visit_priority_last_update_at, VISIT_PRIORITY_HALFLIFE_SECS, now)
}

fn visit_priority_score_now(entry: &RememberedSubredditEntry) -> f64 {
	decayed_priority_score(entry.visit_priority_score, entry.visit_priority_last_update_at, VISIT_PRIORITY_HALFLIFE_SECS)
}

fn feedback_priority_score_at(entry: &RememberedSubredditEntry, now: Instant) -> f64 {
	decayed_priority_score_at(entry.feedback_priority_score, entry.feedback_priority_last_update_at, FEEDBACK_PRIORITY_HALFLIFE_SECS, now)
}

fn feedback_priority_score_now(entry: &RememberedSubredditEntry) -> f64 {
	decayed_priority_score(entry.feedback_priority_score, entry.feedback_priority_last_update_at, FEEDBACK_PRIORITY_HALFLIFE_SECS)
}

/// Browsing the canonical first page of a remembered `new` feed (subreddit, flair, or `u_` listing)
/// temporarily increases daemon refresh priority. Throttled by [`VISIT_SIGNAL_COOLDOWN_SECS`]; score
/// decays with a 72h half-life so repeat visits stack while interest is sustained.
pub fn record_remembered_feed_visit_signal(source: &FeedSource) {
	let Some(normalized) = normalize_remembered_source(source) else {
		return;
	};
	let Some(key) = remembered_feed_key_for_source(&normalized) else {
		return;
	};

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let entry = match remembered.entry(key) {
		Entry::Occupied(slot) => slot.into_mut(),
		Entry::Vacant(slot) => {
			let name = slot.key().clone();
			slot.insert(new_remembered_subreddit_entry(name))
		}
	};

	if entry.visit_signal_last_applied_at.is_some_and(|t| t.elapsed().as_secs() < VISIT_SIGNAL_COOLDOWN_SECS) {
		return;
	}

	let mut score = visit_priority_score_now(entry);
	score = (score + VISIT_PRIORITY_BUMP).min(VISIT_PRIORITY_MAX_SCORE);
	entry.visit_priority_score = score;
	entry.visit_priority_last_update_at = Some(Instant::now());
	entry.visit_signal_last_applied_at = Some(Instant::now());
	crate::cache_persistence::mark_dirty();
}

pub fn record_remembered_feed_feedback_signal_delta(source: &FeedSource, signal_delta: i8) {
	if signal_delta == 0 {
		return;
	}
	let Some(normalized) = normalize_remembered_source(source) else {
		return;
	};
	let Some(key) = remembered_feed_key_for_source(&normalized) else {
		return;
	};

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let entry = match remembered.entry(key) {
		Entry::Occupied(slot) => slot.into_mut(),
		Entry::Vacant(slot) if signal_delta > 0 => {
			let name = slot.key().clone();
			slot.insert(new_remembered_subreddit_entry(name))
		}
		Entry::Vacant(_) => return,
	};

	let score = feedback_priority_score_now(entry) + signal_delta as f64 * FEEDBACK_PRIORITY_BUMP;
	entry.feedback_priority_score = score.clamp(FEEDBACK_PRIORITY_MIN_SCORE, FEEDBACK_PRIORITY_MAX_SCORE);
	entry.feedback_priority_last_update_at = Some(Instant::now());
	crate::cache_persistence::mark_dirty();
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
	let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) else {
		return;
	};
	remember_source_request(&source);
}

pub fn remember_flair_request(subreddit: &str, flair: &str) {
	let Some(source) = normalize_remembered_source(&FeedSource::flair(subreddit.to_string(), flair.to_string(), FeedSort::New)) else {
		return;
	};
	remember_source_request(&source);
}

fn remember_source_request(source: &FeedSource) {
	let Some(key) = remembered_feed_key_for_source(source) else {
		return;
	};

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let entry = remembered.entry(key.clone()).or_insert_with(|| new_remembered_subreddit_entry(key));
	entry.last_requested_at = Instant::now();
	crate::cache_persistence::mark_dirty();
}

fn remember_followed_user_comment_source(name: &str) {
	let Some(source) = normalize_remembered_source(&FeedSource::user_comments(name.to_string(), FeedSort::New)) else {
		return;
	};
	remember_source_request(&source);
}

pub fn remember_frontpage_subreddits(preferences: &Preferences, include_followed_user_comments: bool) {
	for subreddit in &preferences.subscriptions {
		if let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) {
			remember_source_request(&source);
			record_remembered_feed_visit_signal(&source);
		}
		if include_followed_user_comments && subreddit.starts_with("u_") {
			if let Some(source) = normalize_remembered_source(&FeedSource::user_comments(subreddit.to_string(), FeedSort::New)) {
				remember_followed_user_comment_source(subreddit);
				record_remembered_feed_visit_signal(&source);
			}
		}
	}

	for subscription in &preferences.flair_subscriptions {
		if let Some(source) = normalize_remembered_source(&FeedSource::flair(subscription.subreddit.clone(), subscription.flair.clone(), FeedSort::New)) {
			remember_source_request(&source);
			record_remembered_feed_visit_signal(&source);
		}
	}
}

fn membership_source_keys(preferences: &Preferences) -> HashSet<String> {
	let mut keys = HashSet::new();
	for subscription in &preferences.subscriptions {
		if let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subscription.clone(), FeedSort::New)) {
			if let Some(key) = remembered_feed_key_for_source(&source) {
				keys.insert(key);
			}
		}
	}
	for subscription in &preferences.flair_subscriptions {
		if let Some(source) = normalize_remembered_source(&FeedSource::flair(subscription.subreddit.clone(), subscription.flair.clone(), FeedSort::New)) {
			if let Some(key) = remembered_feed_key_for_source(&source) {
				keys.insert(key);
			}
		}
	}
	keys
}

pub(crate) fn reconcile_profile_memberships(profiles: Vec<(String, Preferences)>) {
	let next = profiles
		.into_iter()
		.map(|(profile, preferences)| (profile, membership_source_keys(&preferences)))
		.collect::<HashMap<_, _>>();
	let previous_keys = {
		let mut memberships = PROFILE_SOURCE_MEMBERSHIPS.lock().unwrap_or_else(|error| error.into_inner());
		let previous = memberships.values().flat_map(|keys| keys.iter().cloned()).collect::<HashSet<_>>();
		*memberships = next.clone();
		previous
	};
	let counts = next.values().flat_map(|keys| keys.iter()).fold(HashMap::<String, usize>::new(), |mut counts, key| {
		*counts.entry(key.clone()).or_default() += 1;
		counts
	});
	let now = Instant::now();
	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner());
	for (key, count) in &counts {
		let entry = remembered.entry(key.clone()).or_insert_with(|| new_remembered_subreddit_entry(key.clone()));
		entry.profile_followers = *count;
		entry.unfollowed_decay = false;
		entry.last_requested_at = now;
	}
	for key in previous_keys {
		if counts.contains_key(&key) {
			continue;
		}
		if let Some(entry) = remembered.get_mut(&key) {
			entry.profile_followers = 0;
			entry.unfollowed_decay = true;
			entry.visit_priority_score = 0.0;
			entry.visit_priority_last_update_at = None;
			entry.visit_signal_last_applied_at = None;
			entry.feedback_priority_score = entry.feedback_priority_score.min(0.0);
			entry.last_requested_at = now - Duration::from_secs(REMEMBERED_SUBREDDIT_TTL_SECS.saturating_sub(UNFOLLOWED_DECAY_GRACE_SECS));
		}
	}
	drop(remembered);
	crate::cache_persistence::mark_dirty();
}

fn sqlite_active_profile_sources(preferences: &Preferences) -> Vec<(String, i64)> {
	let mut roles_by_source = HashMap::<String, i64>::new();
	for subscription in &preferences.subscriptions {
		let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subscription.clone(), FeedSort::New)) else {
			continue;
		};
		let role = if subscription.trim().to_ascii_lowercase().starts_with("u_") { 1 } else { 0 };
		roles_by_source.insert(source.cache_key(), role);
		if role == 1 {
			if let Some(comments) = normalize_remembered_source(&FeedSource::user_comments(subscription.clone(), FeedSort::New)) {
				roles_by_source.insert(comments.cache_key(), 2);
			}
		}
	}
	for subscription in &preferences.flair_subscriptions {
		if let Some(source) = normalize_remembered_source(&FeedSource::flair(subscription.subreddit.clone(), subscription.flair.clone(), FeedSort::New)) {
			roles_by_source.insert(source.cache_key(), 0);
		}
	}
	let mut sources = roles_by_source.into_iter().collect::<Vec<_>>();
	sources.sort_by(|left, right| left.0.cmp(&right.0));
	sources
}

pub(crate) fn schedule_sqlite_active_profile_sync(preferences: &Preferences, profile_id: &str) {
	if !crate::cache_sqlite::shadow_enabled() || profile_id.is_empty() {
		return;
	}
	let sources = sqlite_active_profile_sources(preferences);
	let mut filters = preferences
		.filters
		.iter()
		.map(|value| value.trim().to_ascii_lowercase())
		.filter(|value| !value.is_empty())
		.collect::<Vec<_>>();
	filters.sort();
	filters.dedup();
	let profile_id = sqlite_active_profile_id(preferences, profile_id);
	let should_spawn = {
		let Ok(mut desired) = SQLITE_PROFILE_SYNC_DESIRED.lock() else {
			warn!("cache_sqlite: active profile scheduler lock poisoned");
			return;
		};
		let should_spawn = !desired.contains_key(&profile_id);
		desired.insert(profile_id.clone(), (sources, filters));
		should_spawn
	};
	if !should_spawn {
		return;
	}
	tokio::task::spawn_blocking(move || loop {
		let target = {
			let Ok(desired) = SQLITE_PROFILE_SYNC_DESIRED.lock() else {
				warn!("cache_sqlite: active profile scheduler lock poisoned");
				return;
			};
			let Some(target) = desired.get(&profile_id) else {
				return;
			};
			target.clone()
		};
		let result = crate::cache_sqlite::sync_active_profile_with_filters(profile_id.as_bytes(), &target.0, &target.1);
		let Ok(mut desired) = SQLITE_PROFILE_SYNC_DESIRED.lock() else {
			warn!("cache_sqlite: active profile scheduler lock poisoned");
			return;
		};
		if result.is_err() || desired.get(&profile_id) == Some(&target) {
			desired.remove(&profile_id);
			if let Err(error) = result {
				warn!("cache_sqlite: failed to synchronize active profile: {error}");
			}
			return;
		}
	});
}

pub(crate) fn sqlite_active_profile_id(preferences: &Preferences, profile_id: &str) -> String {
	if profile_id.is_empty() {
		return String::new();
	}
	let sources = sqlite_active_profile_sources(preferences);
	let mut filters = preferences
		.filters
		.iter()
		.map(|value| value.trim().to_ascii_lowercase())
		.filter(|value| !value.is_empty())
		.collect::<Vec<_>>();
	filters.sort();
	filters.dedup();
	let mut hasher = DefaultHasher::new();
	profile_id.hash(&mut hasher);
	sources.hash(&mut hasher);
	filters.hash(&mut hasher);
	format!("{profile_id}:{:016x}", hasher.finish())
}

pub(crate) fn seed_remembered_feeds_from_source_cache_entries<I, S>(entries: I, now_unix: u64) -> usize
where
	I: IntoIterator<Item = (S, u64)>,
	S: AsRef<str>,
{
	let mut inserted = 0;
	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());

	for (cache_key, fetched_at_unix) in entries {
		let Some(source) = source_from_cache_key(cache_key.as_ref()).and_then(|source| normalize_remembered_source(&source)) else {
			continue;
		};
		let Some(key) = remembered_feed_key_for_source(&source) else {
			continue;
		};
		if let Entry::Vacant(slot) = remembered.entry(key.clone()) {
			let fetched_at = unix_to_instant(fetched_at_unix, now_unix);
			let mut entry = new_remembered_subreddit_entry(key);
			entry.last_requested_at = fetched_at;
			entry.last_refresh_started_at = Some(fetched_at);
			entry.last_refresh_succeeded_at = Some(fetched_at);
			slot.insert(entry);
			inserted += 1;
		}
	}
	drop(remembered);

	if inserted > 0 {
		crate::cache_persistence::mark_dirty();
	}

	inserted
}

#[cfg(test)]
pub(crate) fn seed_remembered_feeds_from_source_cache_keys<I, S>(keys: I) -> usize
where
	I: IntoIterator<Item = S>,
	S: AsRef<str>,
{
	seed_remembered_feeds_from_source_cache_entries(
		keys.into_iter().map(|key| (key, crate::cache_persistence::unix_now())),
		crate::cache_persistence::unix_now(),
	)
}

pub fn is_subreddit_remembered(subreddit: &str) -> bool {
	let Some(normalized) = normalize_remembered_feed_name(subreddit) else {
		return false;
	};

	REMEMBERED_SUBREDDITS
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.get(&normalized)
		.is_some_and(remembered_entry_is_active)
}

fn is_source_remembered(source: &FeedSource) -> bool {
	let key = remembered_feed_key_for_source(source);
	let Some(key) = key else {
		return false;
	};

	REMEMBERED_SUBREDDITS
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.get(&key)
		.is_some_and(remembered_entry_is_active)
}

fn source_cache_is_servable_with_mode(source: &FeedSource, entry: &CachedSourcePosts, mode: CacheServeMode) -> bool {
	if matches!(mode, CacheServeMode::AllowStaleOnOauthOutage) && (!oauth_is_available() || crate::instance_scrape::reddit_rate_limited_now()) {
		return true;
	}

	is_source_remembered(source) || !source_cache_is_expired(entry)
}

fn source_cache_is_servable(source: &FeedSource, entry: &CachedSourcePosts) -> bool {
	source_cache_is_servable_with_mode(source, entry, CacheServeMode::Normal)
}

fn persisted_source_entry_from_cache(entry: &CachedSourcePosts, now_unix: u64) -> Option<PersistedSourceCacheEntry> {
	let source_key = remembered_feed_key_for_source(&entry.source)?;
	Some(PersistedSourceCacheEntry {
		subreddit: source_key,
		posts: entry.posts.iter().map(crate::cache_persistence::PersistedPost::from_runtime).collect(),
		after: entry.after.clone(),
		fetched_at_unix: instant_to_unix(entry.fetched_at, now_unix),
		refresh_origin: entry.refresh_origin.map(PersistedRefreshOrigin::from_runtime),
	})
}

fn write_source_entry_to_disk(source_cache_key: &str, entry: &CachedSourcePosts) -> bool {
	#[cfg(test)]
	{
		let _ = (source_cache_key, entry);
		TEST_DURABLE_WRITE_COUNT.fetch_add(1, AtomicOrdering::SeqCst);
		if TEST_DURABLE_WRITE_FAIL.load(AtomicOrdering::SeqCst) {
			return false;
		}
		let delay_ms = TEST_DURABLE_WRITE_DELAY_MS.load(AtomicOrdering::SeqCst);
		if delay_ms > 0 {
			TEST_DURABLE_WRITE_ACTIVE.store(true, AtomicOrdering::SeqCst);
			std::thread::sleep(Duration::from_millis(delay_ms));
			TEST_DURABLE_WRITE_ACTIVE.store(false, AtomicOrdering::SeqCst);
		}
		if TEST_DURABLE_WRITE_FAIL.load(AtomicOrdering::SeqCst) {
			return false;
		}
		return true;
	}
	#[cfg(not(test))]
	{
		let Some(persisted) = persisted_source_entry_from_cache(entry, crate::cache_persistence::unix_now()) else {
			return true;
		};
		if let Err(error) = crate::cache_persistence::write_source_cache_entry(source_cache_key, &persisted) {
			warn!("post_cache: failed to persist source cache entry {}: {}", entry.source.display_name(), error);
			return false;
		}
		true
	}
}

fn write_persisted_source_entry_to_disk(source: &FeedSource, entry: &PersistedSourceCacheEntry) {
	#[cfg(test)]
	{
		let _ = (source, entry);
		return;
	}
	#[cfg(not(test))]
	{
		if let Err(error) = crate::cache_persistence::write_source_cache_entry(&source.cache_key(), entry) {
			warn!("post_cache: failed to persist source cache entry {}: {}", source.display_name(), error);
		}
	}
}

fn load_source_entry_from_disk(source: &FeedSource, mode: CacheServeMode) -> Option<CachedSourcePosts> {
	#[cfg(test)]
	{
		let _ = (source, mode);
		return None;
	}
	#[cfg(not(test))]
	{
		if !deleted_media_safety_healthy() {
			return None;
		}
		let cache_key = source.cache_key();
		let refresh_state = source_refresh_state(&cache_key);
		let refresh_guard = refresh_state.lock().unwrap_or_else(|error| error.into_inner());
		let mut persisted = match crate::cache_persistence::read_source_cache_entry(&cache_key) {
			Ok(Some(entry)) => entry,
			Ok(None) => return None,
			Err(error) => {
				warn!("post_cache: failed to read source cache entry {}: {}", source.display_name(), error);
				// A tombstone failure can close the safety latch after the
				// initial check above but before the persistence layer reads.
				// Preserve the legacy blob and SQLite shadow in that case:
				// safety unavailability is not evidence of corruption.
				if deleted_media_safety_healthy() {
					crate::cache_persistence::remove_source_cache_entry(&cache_key);
				}
				return None;
			}
		};
		let expected_key = remembered_feed_key_for_source(source)?;
		if persisted.subreddit != expected_key {
			warn!(
				"post_cache: ignoring source cache entry {} with mismatched key {}",
				source.display_name(),
				persisted.subreddit
			);
			return None;
		}

		let now_unix = crate::cache_persistence::unix_now();
		let mut posts = persisted
			.posts
			.iter()
			.cloned()
			.map(crate::cache_persistence::PersistedPost::into_runtime)
			.collect::<Vec<_>>();
		let deleted_media_changed = prune_known_deleted_media_from_posts(&mut posts);
		if deleted_media_changed {
			persisted.posts = posts.iter().map(crate::cache_persistence::PersistedPost::from_runtime).collect();
			if let Err(error) = crate::cache_persistence::write_source_cache_entry(&cache_key, &persisted) {
				mark_deleted_media_safety_unhealthy();
				warn!(
					"post_cache: refusing to publish source {} after deleted-media pruning could not be persisted: {}",
					source.display_name(),
					error
				);
				return None;
			}
		}
		let estimated_bytes = estimated_source_bytes(&posts, &persisted.after);
		let entry = CachedSourcePosts {
			source: source.clone(),
			posts,
			after: persisted.after,
			fetched_at: unix_to_instant(persisted.fetched_at_unix, now_unix),
			last_accessed: Instant::now(),
			refresh_origin: persisted.refresh_origin.map(PersistedRefreshOrigin::into_runtime),
			estimated_bytes,
		};
		if !source_cache_is_servable_with_mode(source, &entry, mode) {
			return None;
		}

		update_user_video_stats_from_posts(&entry.posts);
		if materialized_catalog::source_is_supported(source) {
			materialized_catalog::apply_source_refresh_with_observation(source, &entry.posts, crate::post_components::ObservationSource::Persisted);
			apply_legacy_mixed_feed_source_refresh(source, &entry.posts);
		}
		if let Ok(mut cache) = SOURCE_CACHE.lock() {
			cache.insert(cache_key, entry.clone());
		}
		drop(refresh_guard);
		release_source_refresh_state(&source.cache_key(), &refresh_state);
		prune_source_cache();
		Some(entry)
	}
}

fn estimate_post_interval_secs(posts: &[Post], prior: u64) -> u64 {
	let mut timestamps = posts.iter().map(|post| post.created_ts).collect::<Vec<_>>();
	timestamps.sort_unstable_by(|left, right| right.cmp(left));

	let deltas = timestamps
		.windows(2)
		.filter_map(|window| {
			let delta = window[0].saturating_sub(window[1]);
			(delta > 0).then_some(delta)
		})
		.collect::<Vec<_>>();

	if deltas.len() < 2 {
		return prior.clamp(MIN_ESTIMATED_POST_INTERVAL_SECS, MAX_ESTIMATED_POST_INTERVAL_SECS);
	}

	let average = deltas.iter().map(|value| *value as u128).sum::<u128>() / deltas.len() as u128;
	(average.min(u64::MAX as u128) as u64).clamp(MIN_ESTIMATED_POST_INTERVAL_SECS, MAX_ESTIMATED_POST_INTERVAL_SECS)
}

fn freshness_target_secs(estimated_post_interval_secs: u64) -> u64 {
	estimated_post_interval_secs.saturating_mul(20).clamp(MIN_FRESHNESS_TARGET_SECS, MAX_FRESHNESS_TARGET_SECS)
}

fn effective_freshness_target_secs(entry: &RememberedSubredditEntry) -> u64 {
	effective_freshness_target_secs_at(entry, Instant::now())
}

fn effective_freshness_target_secs_at(entry: &RememberedSubredditEntry, now: Instant) -> u64 {
	let mut base = freshness_target_secs(entry.estimated_post_interval_secs);
	if entry.consecutive_failures >= 2 {
		base = base.saturating_mul(entry.consecutive_failures.max(1).min(8) as u64);
	}
	let score = visit_priority_score_at(entry, now);
	let mut target = base as f64 / (1.0 + VISIT_PRIORITY_SENSITIVITY * score);
	let feedback_score = feedback_priority_score_at(entry, now);
	if feedback_score >= 0.0 {
		target /= 1.0 + FEEDBACK_PRIORITY_SENSITIVITY * feedback_score;
	} else {
		target *= 1.0 + FEEDBACK_PRIORITY_SENSITIVITY * -feedback_score;
	}
	target.clamp(MIN_FRESHNESS_TARGET_SECS as f64, MAX_FRESHNESS_TARGET_SECS as f64) as u64
}

fn remembered_entry_refresh_blocked(entry: &RememberedSubredditEntry) -> bool {
	remembered_entry_refresh_blocked_at(entry, Instant::now())
}

fn remembered_entry_refresh_blocked_at(entry: &RememberedSubredditEntry, now: Instant) -> bool {
	entry.banned_until.is_some_and(|until| until > now)
}

fn refresh_ratio_at(entry: &RememberedSubredditEntry, now: Instant) -> (u128, u128) {
	let elapsed = entry.last_refresh_succeeded_at.map(|instant| elapsed_secs_at(now, instant) as u128).unwrap_or(u128::MAX);
	(elapsed, effective_freshness_target_secs_at(entry, now) as u128)
}

fn compare_refresh_ratio_at(left: &RememberedSubredditEntry, right: &RememberedSubredditEntry, now: Instant) -> Ordering {
	let (left_num, left_den) = refresh_ratio_at(left, now);
	let (right_num, right_den) = refresh_ratio_at(right, now);
	right_num.saturating_mul(left_den).cmp(&left_num.saturating_mul(right_den))
}

fn recent_request_priority_at(entry: &RememberedSubredditEntry, now: Instant) -> bool {
	elapsed_secs_at(now, entry.last_requested_at) <= RECENT_REQUEST_PRIORITY_SECS
}

fn remembered_refresh_priority_at(left: &RememberedSubredditEntry, right: &RememberedSubredditEntry, now: Instant) -> Ordering {
	let left_recent = recent_request_priority_at(left, now);
	let right_recent = recent_request_priority_at(right, now);
	match right_recent.cmp(&left_recent) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	let left_never = left.last_refresh_succeeded_at.is_none();
	let right_never = right.last_refresh_succeeded_at.is_none();
	match right_never.cmp(&left_never) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	let left_overdue = left
		.last_refresh_succeeded_at
		.is_none_or(|instant| elapsed_secs_at(now, instant) >= effective_freshness_target_secs_at(left, now));
	let right_overdue = right
		.last_refresh_succeeded_at
		.is_none_or(|instant| elapsed_secs_at(now, instant) >= effective_freshness_target_secs_at(right, now));
	match right_overdue.cmp(&left_overdue) {
		Ordering::Equal => {}
		ordering => return ordering,
	}

	match compare_refresh_ratio_at(left, right, now) {
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

fn fairness_service_age_secs_at(entry: &RememberedSubredditEntry, now: Instant) -> u64 {
	entry
		.last_refresh_started_at
		.or(entry.last_refresh_succeeded_at)
		.map(|instant| elapsed_secs_at(now, instant))
		.unwrap_or_else(|| elapsed_secs_at(now, entry.last_requested_at))
}

fn fairness_priority_at(left: &RememberedSubredditEntry, right: &RememberedSubredditEntry, now: Instant) -> Ordering {
	fairness_service_age_secs_at(right, now)
		.cmp(&fairness_service_age_secs_at(left, now))
		.then_with(|| left.name.cmp(&right.name))
}

fn fairness_slot_for_sequence(sequence: u64) -> bool {
	sequence % FAIRNESS_SELECTION_STRIDE == FAIRNESS_SELECTION_STRIDE - 1
}

fn ordered_refresh_entries_with_fairness(mut entries: Vec<RememberedSubredditEntry>, now: Instant, start_sequence: u64) -> Vec<(RememberedSubredditEntry, bool)> {
	let mut ordered = Vec::with_capacity(entries.len());
	let mut sequence = start_sequence;
	while !entries.is_empty() {
		let fairness_slot = fairness_slot_for_sequence(sequence);
		let selected = entries
			.iter()
			.enumerate()
			.min_by(|(_, left), (_, right)| {
				if fairness_slot {
					fairness_priority_at(left, right, now)
				} else {
					remembered_refresh_priority_at(left, right, now)
				}
			})
			.map(|(index, _)| index)
			.unwrap_or_default();
		ordered.push((entries.remove(selected), fairness_slot));
		sequence = sequence.saturating_add(1);
	}
	ordered
}

pub(crate) fn mark_plain_subreddit_refresh_started(subreddit: &str) -> Option<SourceRefreshToken> {
	let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) else {
		return None;
	};
	Some(begin_remembered_source_refresh(&source))
}

fn mark_remembered_source_refresh_started(source: &FeedSource) -> Option<String> {
	let Some(key) = remembered_feed_key_for_source(source) else {
		return None;
	};

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let Some(entry) = remembered.get_mut(&key) else {
		return None;
	};

	entry.last_refresh_started_at = Some(Instant::now());
	entry.refresh_in_flight = true;
	crate::cache_persistence::mark_dirty();
	Some(key)
}

fn source_refresh_state(cache_key: &str) -> Arc<Mutex<u64>> {
	let mut states = SOURCE_REFRESH_STATES.lock().unwrap_or_else(|error| error.into_inner());
	// Only active refresh/decay operations retain a strong state reference.
	// Pruning dead weak entries here keeps the registry proportional to
	// concurrent work instead of every source ever observed.
	states.retain(|_, state| state.strong_count() > 0);
	if let Some(state) = states.get(cache_key).and_then(Weak::upgrade) {
		return state;
	}
	let state = Arc::new(Mutex::new(0));
	states.insert(cache_key.to_string(), Arc::downgrade(&state));
	state
}

fn release_source_refresh_state(cache_key: &str, state: &Arc<Mutex<u64>>) {
	let mut states = SOURCE_REFRESH_STATES.lock().unwrap_or_else(|error| error.into_inner());
	if Arc::strong_count(state) == 1 && states.get(cache_key).and_then(Weak::upgrade).is_some_and(|current| Arc::ptr_eq(&current, state)) {
		states.remove(cache_key);
	}
}

fn invalidate_active_source_refreshes() {
	let states = {
		let mut registry = SOURCE_REFRESH_STATES.lock().unwrap_or_else(|error| error.into_inner());
		registry.retain(|_, state| state.strong_count() > 0);
		registry
			.iter()
			.filter_map(|(cache_key, state)| Weak::upgrade(state).map(|state| (cache_key.clone(), state)))
			.collect::<Vec<_>>()
	};
	let mut remembered_changed = false;
	for (cache_key, state) in states {
		let mut generation = state.lock().unwrap_or_else(|error| error.into_inner());
		*generation = (*generation).saturating_add(1);
		let remembered_key = source_from_cache_key(&cache_key).as_ref().and_then(remembered_feed_key_for_source);
		if let Some(remembered_key) = remembered_key {
			let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner());
			if let Some(entry) = remembered.get_mut(&remembered_key) {
				// The invalidated completion will return Stale and therefore
				// cannot clear its own bookkeeping. Make the source eligible
				// for a replacement refresh while holding the same generation
				// guard that invalidated the token.
				remembered_changed |= entry.refresh_in_flight;
				entry.refresh_in_flight = false;
			}
		}
	}
	if remembered_changed {
		crate::cache_persistence::mark_dirty();
	}
}

pub(crate) fn begin_source_refresh(source: &FeedSource) -> SourceRefreshToken {
	begin_source_refresh_with_remembered_state(source)
}

fn begin_remembered_source_refresh(source: &FeedSource) -> SourceRefreshToken {
	begin_source_refresh_with_remembered_state(source)
}

fn begin_source_refresh_with_remembered_state(source: &FeedSource) -> SourceRefreshToken {
	let cache_key = source.cache_key();
	let state = source_refresh_state(&cache_key);
	let (generation, remembered_key) = {
		let mut current = state.lock().unwrap_or_else(|error| error.into_inner());
		*current = current.saturating_add(1);
		// Keep refresh-start bookkeeping in the same generation critical
		// section as token allocation. Otherwise an older completion can clear
		// refresh_in_flight after a newer request has announced itself but
		// before that request acquires the generation mutex.
		let remembered_key = mark_remembered_source_refresh_started(source);
		(*current, remembered_key)
	};
	let lifecycle = Arc::new(SourceRefreshLifecycle {
		cache_key: cache_key.clone(),
		generation,
		state: Arc::clone(&state),
		remembered_key,
	});
	SourceRefreshToken { cache_key, generation, lifecycle }
}

fn should_evict_resident_source_entry(entry: &CachedSourcePosts) -> bool {
	is_persisted_subreddit_feed_source(&entry.source) && entry.last_accessed.elapsed().as_secs() > source_resident_ttl_secs()
}

fn prune_source_cache_locked(cache: &mut HashMap<String, CachedSourcePosts>) -> Vec<(FeedSource, bool)> {
	let mut pruned_sources = Vec::new();
	cache.retain(|_, entry| {
		let servable = source_cache_is_servable(&entry.source, entry);
		let evict_resident = servable && should_evict_resident_source_entry(entry);
		let keep = servable && !evict_resident;
		if !keep {
			// Automatic pruning owns only request-visible RAM/catalog state. A
			// source refresh can already be in flight after this lock is
			// released, so deleting its legacy blob here could race a newer
			// durable commit. Explicit remembered-source decay owns disk GC.
			pruned_sources.push((entry.source.clone(), false));
		}
		keep
	});
	enforce_source_cache_limits_locked(cache, source_cache_max_entries(), source_cache_max_bytes(), &mut pruned_sources);
	pruned_sources
}

fn enforce_source_cache_limits_locked(cache: &mut HashMap<String, CachedSourcePosts>, max_entries: usize, max_bytes: usize, pruned_sources: &mut Vec<(FeedSource, bool)>) {
	let mut resident_bytes = cache.values().map(|entry| entry.estimated_bytes).sum::<usize>();
	if cache.len() > max_entries || resident_bytes > max_bytes {
		let mut oldest = cache.iter().map(|(key, entry)| (key.clone(), entry.last_accessed)).collect::<Vec<_>>();
		oldest.sort_by_key(|(_, accessed)| *accessed);
		for (key, _) in oldest {
			if cache.len() <= max_entries && resident_bytes <= max_bytes {
				break;
			}
			let Some(entry) = cache.remove(&key) else {
				continue;
			};
			resident_bytes = resident_bytes.saturating_sub(entry.estimated_bytes);
			pruned_sources.push((entry.source, false));
		}
	}
}

fn semantic_backfill_priority(refresh_origin: Option<RefreshOrigin>) -> crate::observed_semantic_backfill::ObservedBackfillPriority {
	match refresh_origin {
		Some(RefreshOrigin::FrontPageSession) => crate::observed_semantic_backfill::ObservedBackfillPriority::Priority,
		Some(RefreshOrigin::Daemon) => crate::observed_semantic_backfill::ObservedBackfillPriority::Background,
		Some(RefreshOrigin::DirectLiveFetch) | None => crate::observed_semantic_backfill::ObservedBackfillPriority::Opportunistic,
	}
}

pub fn source_cache_revision() -> u64 {
	SOURCE_CACHE_REVISION.load(AtomicOrdering::Relaxed)
}

fn bump_source_cache_revision() {
	SOURCE_CACHE_REVISION.fetch_add(1, AtomicOrdering::Relaxed);
}

fn source_window_signature(posts: &[Post], after: &str) -> u64 {
	let mut hasher = DefaultHasher::new();
	after.hash(&mut hasher);
	posts.len().hash(&mut hasher);
	for post in posts {
		post.id.hash(&mut hasher);
		post.kind.hash(&mut hasher);
		post.community.hash(&mut hasher);
		post.author.name.hash(&mut hasher);
		post.created_ts.hash(&mut hasher);
		post.score.1.hash(&mut hasher);
		post.post_type.hash(&mut hasher);
	}
	hasher.finish()
}

pub(crate) fn prune_source_cache() {
	let pruned_sources = {
		let Ok(mut cache) = SOURCE_CACHE.lock() else {
			return;
		};
		prune_source_cache_locked(&mut cache)
	};

	if pruned_sources.is_empty() {
		return;
	}

	remove_pruned_sources_if_still_absent(&pruned_sources);
	bump_source_cache_revision();
	crate::cache_persistence::mark_dirty();
}

fn remove_pruned_sources_if_still_absent(pruned_sources: &[(FeedSource, bool)]) {
	for (source, remove_disk_entry) in pruned_sources {
		#[cfg(test)]
		{
			let delay_ms = TEST_PRUNE_REMOVE_DELAY_MS.load(AtomicOrdering::SeqCst);
			if delay_ms > 0 {
				TEST_PRUNE_REMOVE_ACTIVE.store(true, AtomicOrdering::SeqCst);
				std::thread::sleep(Duration::from_millis(delay_ms));
				TEST_PRUNE_REMOVE_ACTIVE.store(false, AtomicOrdering::SeqCst);
			}
		}
		let cache_key = source.cache_key();
		let state = source_refresh_state(&cache_key);
		let generation = state.lock().unwrap_or_else(|error| error.into_inner());
		let still_absent = SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&cache_key).is_none();
		if still_absent {
			materialized_catalog::remove_sources(std::slice::from_ref(source));
			if *remove_disk_entry {
				crate::cache_persistence::remove_source_cache_entry(&cache_key);
				materialized_catalog::remove_mixed_feed_sources(std::slice::from_ref(source));
			}
		}
		drop(generation);
		release_source_refresh_state(&cache_key, &state);
	}
}

fn store_source_posts_with_token_and_effects<Committed, Failed>(
	source: &FeedSource,
	posts: Vec<Post>,
	after: String,
	refresh_origin: Option<RefreshOrigin>,
	observation_source: crate::post_components::ObservationSource,
	token: SourceRefreshToken,
	on_committed: Committed,
	on_failed: Failed,
) -> SourceRefreshOutcome
where
	Committed: FnOnce(&[Post]),
	Failed: FnOnce(),
{
	let cache_key = source.cache_key();
	if token.cache_key != cache_key {
		warn!(
			"post_cache: rejected source refresh token for {} because it belongs to {}",
			source.display_name(),
			token.cache_key
		);
		return SourceRefreshOutcome::Stale;
	}
	let current_generation = token.lifecycle.state.lock().unwrap_or_else(|error| error.into_inner());
	if *current_generation != token.generation {
		info!(
			"post_cache: discarded stale source refresh for {} (generation {} < {})",
			source.display_name(),
			token.generation,
			*current_generation
		);
		return SourceRefreshOutcome::Stale;
	}
	if !deleted_media_safety_healthy() {
		warn!("post_cache: rejected source refresh for {} while deleted-media durability is unsafe", source.display_name());
		on_failed();
		return SourceRefreshOutcome::Failed;
	}

	let mut posts = posts;
	prune_known_deleted_media_from_posts(&mut posts);
	let new_signature = source_window_signature(&posts, &after);
	let estimated_bytes = estimated_source_bytes(&posts, &after);
	let new_entry = CachedSourcePosts {
		source: source.clone(),
		posts,
		after,
		fetched_at: Instant::now(),
		last_accessed: Instant::now(),
		refresh_origin,
		estimated_bytes,
	};
	// Persist legacy-first and complete the SQLite shadow transaction before
	// publishing the new request-visible window. This work is intentionally
	// outside SOURCE_CACHE; the per-source generation guard prevents an
	// overlapping completion from reordering the durable commit.
	if !write_source_entry_to_disk(&cache_key, &new_entry) {
		on_failed();
		return SourceRefreshOutcome::Failed;
	}

	crate::utils::store_post_flairs(&new_entry.posts);
	match source {
		FeedSource::Subreddit { name, .. } | FeedSource::Flair { subreddit: name, .. } => {
			crate::utils::store_subreddit_flairs(name, &new_entry.posts);
			if !name.to_ascii_lowercase().starts_with("u_") {
				crate::utils::store_subreddit_users(name, &new_entry.posts);
			}
		}
		FeedSource::UserComments { .. } => {}
	}
	update_user_video_stats_from_posts(&new_entry.posts);
	if materialized_catalog::source_is_supported(source) {
		if refresh_origin == Some(RefreshOrigin::Daemon) {
			materialized_catalog::record_daemon_discoveries(source, &new_entry.posts);
		} else {
			// A later daemon refresh must not promote posts already seen through an
			// interactive fetch; only genuinely new daemon discoveries rise to the top.
			materialized_catalog::mark_posts_seen_before_daemon(source, &new_entry.posts);
		}
		materialized_catalog::apply_source_refresh_with_observation(source, &new_entry.posts, observation_source);
		apply_legacy_mixed_feed_source_refresh(source, &new_entry.posts);
	}
	on_committed(&new_entry.posts);
	let mut source_window_changed = true;
	let mut pruned_sources = Vec::new();
	if let Ok(mut cache) = SOURCE_CACHE.lock() {
		source_window_changed = cache
			.get(&cache_key)
			.is_none_or(|entry| source_window_signature(&entry.posts, &entry.after) != new_signature);
		cache.insert(cache_key.clone(), new_entry.clone());
		pruned_sources.extend(prune_source_cache_locked(&mut cache));
	}
	let priority = semantic_backfill_priority(refresh_origin);
	crate::observed_semantic_backfill::enqueue_observed_posts(&cache_key, &new_entry.posts, priority);
	drop(current_generation);
	remove_pruned_sources_if_still_absent(&pruned_sources);
	if source_window_changed || !pruned_sources.is_empty() {
		bump_source_cache_revision();
	}
	if is_persisted_subreddit_feed_source(source) || materialized_catalog::source_is_supported(source) {
		crate::cache_persistence::mark_dirty();
	}
	SourceRefreshOutcome::Committed
}

fn store_source_posts_with_token(
	source: &FeedSource,
	posts: Vec<Post>,
	after: String,
	refresh_origin: Option<RefreshOrigin>,
	token: SourceRefreshToken,
) -> SourceRefreshOutcome {
	store_source_posts_with_token_and_effects(
		source,
		posts,
		after,
		refresh_origin,
		crate::post_components::ObservationSource::Listing,
		token,
		|_| {},
		|| {},
	)
}

#[cfg(test)]
fn store_source_posts_with_origin(source: &FeedSource, posts: Vec<Post>, after: String, refresh_origin: Option<RefreshOrigin>) -> SourceRefreshOutcome {
	let token = begin_source_refresh(source);
	store_source_posts_with_token(source, posts, after, refresh_origin, token)
}

fn publish_remembered_source_refresh_with_token_and_effects<Extra>(
	source: &FeedSource,
	posts: Vec<Post>,
	after: String,
	refresh_origin: RefreshOrigin,
	observation_source: crate::post_components::ObservationSource,
	token: SourceRefreshToken,
	extra_committed: Extra,
) -> SourceRefreshOutcome
where
	Extra: FnOnce(&[Post]),
{
	let Some(source) = normalize_remembered_source(source) else {
		return SourceRefreshOutcome::Stale;
	};
	let success_source = source.clone();
	let success_after = after.clone();
	let failed_source = source.clone();
	store_source_posts_with_token_and_effects(
		&source,
		posts,
		after,
		Some(refresh_origin),
		observation_source,
		token,
		move |committed_posts| {
			extra_committed(committed_posts);
			if matches!(refresh_origin, RefreshOrigin::Daemon | RefreshOrigin::FrontPageSession) {
				crate::video_preview::prewarm_listing_video_previews(committed_posts);
			}
			let Some(key) = remembered_feed_key_for_source(&success_source) else {
				return;
			};
			let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(entry) = remembered.get_mut(&key) else {
				return;
			};
			entry.last_refresh_succeeded_at = Some(Instant::now());
			entry.last_refresh_failed_at = None;
			entry.banned_until = None;
			entry.unavailable_reason = None;
			entry.consecutive_failures = 0;
			entry.estimated_post_interval_secs = estimate_post_interval_secs(committed_posts, entry.estimated_post_interval_secs);
			entry.refresh_in_flight = false;
			info!(
				"post_cache: remembered feed refresh succeeded for {} via {:?}: {} posts, after_present={}",
				success_source.display_name(),
				refresh_origin,
				committed_posts.len(),
				!success_after.is_empty()
			);
			crate::cache_persistence::mark_dirty();
		},
		move || {
			let _ = record_remembered_source_refresh_failure(&failed_source, "durable source cache publication failed", refresh_origin);
		},
	)
}

fn publish_remembered_source_refresh_with_token(
	source: &FeedSource,
	posts: Vec<Post>,
	after: String,
	refresh_origin: RefreshOrigin,
	token: SourceRefreshToken,
) -> SourceRefreshOutcome {
	publish_remembered_source_refresh_with_token_and_effects(source, posts, after, refresh_origin, crate::post_components::ObservationSource::Listing, token, |_| {})
}

fn publish_remembered_source_refresh(source: &FeedSource, posts: Vec<Post>, after: String, refresh_origin: RefreshOrigin) -> SourceRefreshOutcome {
	let Some(normalized) = normalize_remembered_source(source) else {
		return SourceRefreshOutcome::Stale;
	};
	let token = begin_source_refresh(&normalized);
	publish_remembered_source_refresh_with_token(&normalized, posts, after, refresh_origin, token)
}

pub fn publish_plain_subreddit_refresh(subreddit: &str, posts: Vec<Post>, after: String, refresh_origin: RefreshOrigin) {
	let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) else {
		return;
	};

	publish_remembered_source_refresh(&source, posts, after, refresh_origin);
}

pub(crate) fn publish_plain_subreddit_refresh_with_token(
	subreddit: &str,
	posts: Vec<Post>,
	after: String,
	refresh_origin: RefreshOrigin,
	token: SourceRefreshToken,
) -> SourceRefreshOutcome {
	let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) else {
		return SourceRefreshOutcome::Stale;
	};
	publish_remembered_source_refresh_with_token(&source, posts, after, refresh_origin, token)
}

pub(crate) fn publish_plain_subreddit_refresh_with_token_and_effects<Committed>(
	subreddit: &str,
	posts: Vec<Post>,
	after: String,
	refresh_origin: RefreshOrigin,
	observation_source: crate::post_components::ObservationSource,
	token: SourceRefreshToken,
	on_committed: Committed,
) -> SourceRefreshOutcome
where
	Committed: FnOnce(&[Post]),
{
	let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) else {
		return SourceRefreshOutcome::Stale;
	};
	publish_remembered_source_refresh_with_token_and_effects(&source, posts, after, refresh_origin, observation_source, token, on_committed)
}

fn durable_unavailable_reason(error: &str) -> Option<&'static str> {
	match crate::client::source_unavailable_from_error(error)? {
		crate::client::SourceUnavailable::Banned => Some("banned"),
		crate::client::SourceUnavailable::Private => Some("private"),
		crate::client::SourceUnavailable::Suspended => Some("suspended"),
		crate::client::SourceUnavailable::NotFound => Some("not_found"),
		// A generic 403 can reflect temporary OAuth or access trouble.
		crate::client::SourceUnavailable::Forbidden => None,
	}
}

fn refresh_failure_disposition(error: &str) -> RememberedFailureDisposition {
	if durable_unavailable_reason(error).is_some() {
		RememberedFailureDisposition::DurableUnavailable
	} else {
		RememberedFailureDisposition::Transient
	}
}

pub fn record_plain_subreddit_refresh_failure(subreddit: &str, error: &str, refresh_origin: RefreshOrigin) {
	let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) else {
		return;
	};
	let _ = record_remembered_source_refresh_failure(&source, error, refresh_origin);
}

pub(crate) fn record_plain_subreddit_refresh_failure_with_token(
	subreddit: &str,
	error: &str,
	refresh_origin: RefreshOrigin,
	token: SourceRefreshToken,
) -> SourceRefreshOutcome {
	let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) else {
		return SourceRefreshOutcome::Stale;
	};
	record_remembered_source_refresh_failure_with_token(&source, error, refresh_origin, token)
}

fn record_remembered_source_refresh_failure_with_token(source: &FeedSource, error: &str, refresh_origin: RefreshOrigin, token: SourceRefreshToken) -> SourceRefreshOutcome {
	if token.cache_key != source.cache_key() {
		return SourceRefreshOutcome::Stale;
	}
	let current_generation = token.lifecycle.state.lock().unwrap_or_else(|lock_error| lock_error.into_inner());
	if *current_generation != token.generation {
		return SourceRefreshOutcome::Stale;
	}
	let _ = record_remembered_source_refresh_failure(source, error, refresh_origin);
	SourceRefreshOutcome::Failed
}

fn record_remembered_source_refresh_failure(source: &FeedSource, error: &str, refresh_origin: RefreshOrigin) -> Option<RememberedFailureDisposition> {
	let Some(key) = remembered_feed_key_for_source(source) else {
		return None;
	};
	let disposition = refresh_failure_disposition(error);
	let now = Instant::now();
	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let Some(entry) = remembered.get_mut(&key) else {
		return None;
	};

	entry.last_refresh_failed_at = Some(now);
	if let Some(reason) = durable_unavailable_reason(error) {
		entry.banned_until = Some(now + Duration::from_secs(DURABLE_UNAVAILABLE_BACKOFF_SECS));
		entry.unavailable_reason = Some(reason.to_string());
	}
	entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
	entry.refresh_in_flight = false;
	warn!(
		"post_cache: remembered feed refresh failed for {} via {:?}: {}",
		source.display_name(),
		refresh_origin,
		error
	);
	crate::cache_persistence::mark_dirty();
	Some(disposition)
}

pub fn cleanup_decayed_remembered_subreddits() {
	let candidates = {
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		remembered
			.iter()
			.filter(|(_, entry)| !remembered_entry_is_active(entry) || remembered_feed_source_from_key(&entry.name).is_none())
			.map(|(name, _)| name.clone())
			.collect::<Vec<_>>()
	};

	if candidates.is_empty() {
		return;
	}

	let mut removed_any = false;
	for key in candidates {
		let Some(source) = remembered_feed_source_from_key(&key) else {
			let removed = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner()).remove(&key).is_some();
			removed_any |= removed;
			continue;
		};
		let cache_key = source.cache_key();
		let state = source_refresh_state(&cache_key);
		let mut generation = state.lock().unwrap_or_else(|error| error.into_inner());
		// Hold the remembered map through the eligibility recheck and deletion.
		// A concurrent request can re-remember the source only after cleanup is
		// complete, while the generation bump invalidates any older in-flight
		// completion before it can resurrect the removed window.
		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner());
		let still_decayed = remembered
			.get(&key)
			.is_some_and(|entry| !remembered_entry_is_active(entry) || remembered_feed_source_from_key(&entry.name).is_none());
		if still_decayed {
			*generation = (*generation).saturating_add(1);
			remembered.remove(&key);
			SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).remove(&cache_key);
			crate::cache_persistence::remove_source_cache_entry(&cache_key);
			if let FeedSource::Subreddit { name, .. } = &source {
				crate::subreddit::remove_subreddit_metadata_cache(name);
			}
			materialized_catalog::remove_sources(std::slice::from_ref(&source));
			materialized_catalog::remove_mixed_feed_sources(std::slice::from_ref(&source));
			removed_any = true;
		}
		drop(remembered);
		drop(generation);
		release_source_refresh_state(&cache_key, &state);
	}
	if removed_any {
		crate::cache_persistence::mark_dirty();
	}
}

#[cfg(test)]
pub(crate) fn store_source_posts_for_cache(source: &FeedSource, posts: Vec<Post>, after: String, refresh_origin: Option<RefreshOrigin>) {
	store_source_posts_with_origin(source, posts, after, refresh_origin);
}

pub(crate) fn store_source_posts_for_cache_with_token_and_effects<Committed>(
	source: &FeedSource,
	posts: Vec<Post>,
	after: String,
	refresh_origin: Option<RefreshOrigin>,
	observation_source: crate::post_components::ObservationSource,
	token: SourceRefreshToken,
	on_committed: Committed,
) -> SourceRefreshOutcome
where
	Committed: FnOnce(&[Post]),
{
	store_source_posts_with_token_and_effects(source, posts, after, refresh_origin, observation_source, token, on_committed, || {})
}

#[cfg(test)]
pub(crate) fn age_source_cache_for_test(source: &FeedSource, age: Duration) {
	let mut cache = SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
	let entry = cache.get_mut(&source.cache_key()).expect("source cache entry must exist");
	entry.fetched_at = Instant::now() - age;
}

pub(crate) fn snapshot_persisted_source_entries(now_unix: u64) -> Vec<PersistedSourceCacheEntry> {
	prune_source_cache();
	let Ok(cache) = SOURCE_CACHE.lock() else {
		return Vec::new();
	};

	cache.values().filter_map(|entry| persisted_source_entry_from_cache(entry, now_unix)).collect()
}

pub(crate) fn snapshot_persisted_remembered_subreddits(now_unix: u64) -> Vec<PersistedRememberedSubredditEntry> {
	let Ok(remembered) = REMEMBERED_SUBREDDITS.lock() else {
		return Vec::new();
	};

	remembered
		.values()
		.filter(|entry| remembered_feed_source_from_key(&entry.name).is_some())
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
			visit_priority_score: entry.visit_priority_score,
			visit_priority_last_update_unix: option_instant_to_unix(entry.visit_priority_last_update_at, now_unix),
			visit_signal_last_applied_unix: option_instant_to_unix(entry.visit_signal_last_applied_at, now_unix),
			feedback_priority_score: entry.feedback_priority_score,
			feedback_priority_last_update_unix: option_instant_to_unix(entry.feedback_priority_last_update_at, now_unix),
		})
		.collect()
}

pub(crate) fn snapshot_persisted_source_unavailability() -> Vec<PersistedSourceUnavailableState> {
	let Ok(remembered) = REMEMBERED_SUBREDDITS.lock() else {
		return Vec::new();
	};
	let mut states = remembered
		.values()
		.filter_map(|entry| {
			entry.unavailable_reason.as_ref().map(|reason| PersistedSourceUnavailableState {
				name: entry.name.clone(),
				reason: reason.clone(),
			})
		})
		.collect::<Vec<_>>();
	states.sort_by(|left, right| left.name.cmp(&right.name));
	states
}

#[cfg(not(test))]
pub(crate) fn reset_persisted_mixed_feed_index() {
	materialized_catalog::reset_mixed_feed_catalog();
}

#[cfg(not(test))]
pub(crate) fn index_persisted_source_for_mixed_feed(entry: &PersistedSourceCacheEntry) {
	let Some(source) = remembered_feed_source_from_key(&entry.subreddit) else {
		return;
	};
	materialized_catalog::seed_following_discovery_baseline(&source, &entry.posts, entry.fetched_at_unix);
	materialized_catalog::apply_mixed_feed_persisted_source_refresh(&source, &entry.posts);
}

pub(crate) fn restore_persisted_state(
	source_entries: Vec<PersistedSourceCacheEntry>,
	remembered_entries: Vec<PersistedRememberedSubredditEntry>,
	unavailability: Vec<PersistedSourceUnavailableState>,
	_saved_at_unix: u64,
	now_unix: u64,
) {
	let unavailable_by_name = unavailability.into_iter().map(|state| (state.name, state.reason)).collect::<HashMap<_, _>>();
	let restored_remembered = remembered_entries
		.into_iter()
		.filter_map(|entry| {
			let source = remembered_feed_source_from_key(&entry.name)?;
			let normalized = remembered_feed_key_for_source(&source)?;
			let age_secs = now_unix.saturating_sub(entry.last_requested_at_unix);
			(age_secs <= REMEMBERED_SUBREDDIT_TTL_SECS).then_some((
				normalized.clone(),
				RememberedSubredditEntry {
					name: normalized.clone(),
					last_requested_at: unix_to_instant(entry.last_requested_at_unix, now_unix),
					last_refresh_started_at: option_unix_to_instant(entry.last_refresh_started_at_unix, now_unix),
					last_refresh_succeeded_at: option_unix_to_instant(entry.last_refresh_succeeded_at_unix, now_unix),
					last_refresh_failed_at: option_unix_to_instant(entry.last_refresh_failed_at_unix, now_unix),
					banned_until: option_unix_to_instant(entry.banned_until_unix, now_unix),
					unavailable_reason: unavailable_by_name.get(&normalized).cloned(),
					consecutive_failures: entry.consecutive_failures,
					estimated_post_interval_secs: entry.estimated_post_interval_secs,
					refresh_in_flight: false,
					visit_priority_score: entry.visit_priority_score,
					visit_priority_last_update_at: option_unix_to_instant(entry.visit_priority_last_update_unix, now_unix),
					visit_signal_last_applied_at: option_unix_to_instant(entry.visit_signal_last_applied_unix, now_unix),
					feedback_priority_score: entry.feedback_priority_score,
					feedback_priority_last_update_at: option_unix_to_instant(entry.feedback_priority_last_update_unix, now_unix),
					profile_followers: 0,
					unfollowed_decay: false,
				},
			))
		})
		.collect::<HashMap<_, _>>();

	if let Ok(mut remembered) = REMEMBERED_SUBREDDITS.lock() {
		remembered.extend(restored_remembered.clone());
	}

	for entry in source_entries {
		let Some(source) = remembered_feed_source_from_key(&entry.subreddit) else {
			continue;
		};
		let Some(remembered_key) = remembered_feed_key_for_source(&source) else {
			continue;
		};
		let age_secs = now_unix.saturating_sub(entry.fetched_at_unix);
		let remember_kept = restored_remembered.get(&remembered_key).is_some();
		if age_secs > CACHE_TTL_SECS && !remember_kept {
			crate::cache_persistence::remove_source_cache_entry(&source.cache_key());
			materialized_catalog::remove_mixed_feed_sources(std::slice::from_ref(&source));
			continue;
		}

		materialized_catalog::apply_mixed_feed_persisted_source_refresh(&source, &entry.posts);
		materialized_catalog::seed_following_discovery_baseline(&source, &entry.posts, entry.fetched_at_unix);
		write_persisted_source_entry_to_disk(&source, &entry);
	}
	rebuild_materialized_catalog_from_source_cache();
}

pub(crate) fn rebuild_materialized_catalog_from_source_cache() {
	if !deleted_media_safety_healthy() {
		materialized_catalog::reset_source_catalog_preserving_author_history();
		return;
	}
	prune_source_cache();
	let sources = SOURCE_CACHE
		.lock()
		.unwrap_or_else(|err| err.into_inner())
		.values()
		.filter(|entry| materialized_catalog::source_is_supported(&entry.source))
		.map(|entry| (entry.source.clone(), entry.posts.clone()))
		.collect::<Vec<_>>();

	materialized_catalog::reset_source_catalog_preserving_author_history();
	for (source, posts) in sources {
		materialized_catalog::apply_source_refresh_with_observation(&source, &posts, crate::post_components::ObservationSource::Persisted);
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
		if remembered_feed_source_from_key(&entry.name).is_none() {
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

pub fn pick_next_remembered_feed_to_refresh() -> Option<FeedSource> {
	pick_next_remembered_feed_to_refresh_with_mode(RememberedFeedPickMode::AnyEligible)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RememberedFeedPickMode {
	AnyEligible,
	ProvenOnly,
}

fn pick_next_remembered_feed_to_refresh_with_mode(mode: RememberedFeedPickMode) -> Option<FeedSource> {
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let now = Instant::now();
	let entries = remembered
		.values()
		.filter(|entry| remembered_entry_is_active(entry) && !remembered_entry_refresh_blocked_at(entry, now) && !entry.refresh_in_flight)
		.filter(|entry| mode == RememberedFeedPickMode::AnyEligible || entry.last_refresh_succeeded_at.is_some())
		.cloned()
		.collect::<Vec<_>>();
	let selection_sequence = REMEMBERED_DAEMON_SELECTION_SEQUENCE.load(AtomicOrdering::Relaxed);
	ordered_refresh_entries_with_fairness(entries, now, selection_sequence)
		.into_iter()
		.find_map(|(entry, _)| remembered_feed_source_from_key(&entry.name))
}

pub fn pick_next_remembered_subreddit_to_refresh() -> Option<FeedSource> {
	pick_next_remembered_feed_to_refresh()
}

fn source_should_hydrate_media_fingerprints(source: &FeedSource) -> bool {
	matches!(source, FeedSource::Subreddit { name, .. } if name.to_lowercase().starts_with("u_"))
}

fn remembered_source_had_success(source: &FeedSource) -> bool {
	let Some(key) = remembered_feed_key_for_source(source) else {
		return false;
	};
	REMEMBERED_SUBREDDITS
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.get(&key)
		.is_some_and(|entry| entry.last_refresh_succeeded_at.is_some())
}

async fn refresh_remembered_feed(source: &FeedSource) -> RememberedRefreshOutcome {
	let refresh_token = begin_remembered_source_refresh(source);
	let listing_result = match Post::fetch(&source.build_path(), false).await {
		Ok((posts, after)) => {
			let mut posts = source.filter_posts(posts);
			if source_should_hydrate_media_fingerprints(source) {
				crate::utils::hydrate_media_content_fingerprints(&mut posts).await;
			}
			Ok((posts, after))
		}
		Err(error) => Err(error),
	};

	let outcome = match listing_result {
		Ok((posts, after)) => match publish_remembered_source_refresh_with_token(source, posts, after, RefreshOrigin::Daemon, refresh_token) {
			SourceRefreshOutcome::Committed | SourceRefreshOutcome::Stale => RememberedRefreshOutcome::Succeeded,
			SourceRefreshOutcome::Failed => RememberedRefreshOutcome::Failed(RememberedFailureDisposition::Transient),
		},
		Err(error) => match record_remembered_source_refresh_failure_with_token(source, &error, RefreshOrigin::Daemon, refresh_token) {
			SourceRefreshOutcome::Stale => RememberedRefreshOutcome::Succeeded,
			SourceRefreshOutcome::Failed | SourceRefreshOutcome::Committed => RememberedRefreshOutcome::Failed(refresh_failure_disposition(&error)),
		},
	};

	let metadata_subreddit = match source {
		FeedSource::Subreddit { name, .. } if !name.to_lowercase().starts_with("u_") => Some(name.as_str()),
		FeedSource::Flair { subreddit, .. } => Some(subreddit.as_str()),
		FeedSource::Subreddit { .. } | FeedSource::UserComments { .. } => None,
	};
	let Some(subreddit) = metadata_subreddit else {
		return outcome;
	};

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

	outcome
}

fn daemon_should_continue_after_refresh(outcome: RememberedRefreshOutcome, attempts: usize, successes: usize) -> bool {
	if attempts >= REMEMBERED_SUBREDDIT_DAEMON_MAX_ATTEMPTS_PER_TICK {
		return false;
	}

	match outcome {
		RememberedRefreshOutcome::Succeeded => successes < REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK,
		RememberedRefreshOutcome::Failed(RememberedFailureDisposition::DurableUnavailable) => true,
		RememberedRefreshOutcome::Failed(RememberedFailureDisposition::Transient) => false,
	}
}

pub fn spawn_remembered_feed_daemon() {
	tokio::spawn(async {
		let mut interval = tokio::time::interval(Duration::from_secs(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS));
		interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
		info!("post_cache: remembered feed daemon started (interval={}s)", REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS);
		if let Ok(mut next_tick) = REMEMBERED_SUBREDDIT_DAEMON_NEXT_TICK_AT.lock() {
			*next_tick = Some(Instant::now() + Duration::from_secs(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS));
		}
		interval.tick().await;

		loop {
			interval.tick().await;
			if let Ok(mut next_tick) = REMEMBERED_SUBREDDIT_DAEMON_NEXT_TICK_AT.lock() {
				*next_tick = Some(Instant::now() + Duration::from_secs(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS));
			}
			prune_source_cache();
			cleanup_decayed_remembered_subreddits();
			let (total, active, eligible, blocked_or_in_flight) = remembered_subreddit_daemon_counts();
			info!(
				"post_cache: remembered feed daemon tick (total={}, active={}, eligible={}, blocked_or_in_flight={})",
				total, active, eligible, blocked_or_in_flight
			);

			let mut attempts = 0;
			let mut successes = 0;
			let mut unproven_permanent_failures = 0;
			loop {
				let pick_mode = if unproven_permanent_failures >= REMEMBERED_SUBREDDIT_DAEMON_MAX_UNPROVEN_PERMANENT_FAILURES_PER_TICK {
					RememberedFeedPickMode::ProvenOnly
				} else {
					RememberedFeedPickMode::AnyEligible
				};
				let source = pick_next_remembered_feed_to_refresh_with_mode(pick_mode)
					.or_else(|| (pick_mode == RememberedFeedPickMode::ProvenOnly).then(pick_next_remembered_feed_to_refresh).flatten());
				let Some(source) = source else {
					if attempts == 0 {
						info!("post_cache: remembered feed daemon idle: no eligible source to refresh");
					}
					break;
				};

				let should_skip = {
					let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
					remembered_feed_key_for_source(&source).map_or(true, |key| {
						remembered.get(&key).is_none_or(|entry| !remembered_entry_is_active(entry) || entry.refresh_in_flight)
					})
				};
				if should_skip {
					info!(
						"post_cache: remembered feed daemon skipped refresh for {} because it is no longer eligible",
						source.display_name()
					);
					break;
				}

				attempts += 1;
				REMEMBERED_DAEMON_SELECTION_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
				let source_had_success = remembered_source_had_success(&source);
				info!("post_cache: remembered feed daemon refreshing {}", source.display_name());
				let outcome = refresh_remembered_feed(&source).await;
				if matches!(outcome, RememberedRefreshOutcome::Succeeded) {
					successes += 1;
				}
				if matches!(outcome, RememberedRefreshOutcome::Failed(RememberedFailureDisposition::DurableUnavailable)) && !source_had_success {
					unproven_permanent_failures += 1;
				}
				let should_continue = daemon_should_continue_after_refresh(outcome, attempts, successes);
				match outcome {
					RememberedRefreshOutcome::Succeeded => {
						if should_continue {
							info!(
								"post_cache: remembered feed daemon refreshed {} feeds after {} attempts; continuing batch",
								successes, attempts
							);
							continue;
						}
						if attempts > 1 || successes > 1 {
							info!("post_cache: remembered feed daemon refreshed {} feeds after {} attempts", successes, attempts);
						}
						break;
					}
					RememberedRefreshOutcome::Failed(RememberedFailureDisposition::Transient) => {
						break;
					}
					RememberedRefreshOutcome::Failed(disposition) => {
						if !should_continue {
							warn!(
								"post_cache: remembered feed daemon reached attempt limit after {} attempts; last failure was {:?} for {}",
								attempts,
								disposition,
								source.display_name()
							);
							break;
						}
						if !source_had_success && unproven_permanent_failures == REMEMBERED_SUBREDDIT_DAEMON_MAX_UNPROVEN_PERMANENT_FAILURES_PER_TICK {
							info!(
								"post_cache: remembered feed daemon switching to previously successful feeds after {} unproven {:?} failures",
								unproven_permanent_failures, disposition
							);
						}
						info!(
							"post_cache: remembered feed daemon continuing after {:?} failure for {}",
							disposition,
							source.display_name()
						);
					}
				}
			}
		}
	});
}

pub fn remembered_feed_daemon_enabled() -> bool {
	remembered_feed_daemon_enabled_from(crate::config::get_setting("REDLIB_REMEMBERED_FEED_DAEMON_ENABLED").as_deref())
}

fn remembered_feed_daemon_enabled_from(value: Option<&str>) -> bool {
	value
		.map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
		.unwrap_or(true)
}

pub fn spawn_remembered_subreddit_daemon() {
	spawn_remembered_feed_daemon();
}

fn session_is_expired(entry: &FeedSession) -> bool {
	entry.last_updated.elapsed().as_secs() > CACHE_TTL_SECS
}

fn prune_feed_sessions_locked(sessions: &mut HashMap<String, FeedSession>, incoming_id: &str) {
	sessions.retain(|_, session| !session_is_expired(session));
	if sessions.contains_key(incoming_id) {
		return;
	}
	while sessions.len() >= MAX_FEED_SESSIONS {
		let Some(oldest) = sessions.iter().max_by_key(|(_, session)| session.last_updated.elapsed()).map(|(id, _)| id.clone()) else {
			break;
		};
		sessions.remove(&oldest);
	}
}

fn source_priority(source: &FeedSource) -> (u8, String, String) {
	match source {
		FeedSource::Subreddit { name, sort } => (0, format!("{}:{}", sort.as_str(), name.to_lowercase()), "0".to_string()),
		FeedSource::UserComments { name, sort } => (0, format!("{}:{}", sort.as_str(), name.to_lowercase()), "1".to_string()),
		FeedSource::Flair { subreddit, flair, sort } => (1, format!("{}:{}", sort.as_str(), subreddit.to_lowercase()), flair.to_lowercase()),
	}
}

pub fn sort_sources(sources: &mut [FeedSource]) {
	sources.sort_by_key(source_priority);
}

pub const fn initial_source_window() -> usize {
	INITIAL_SOURCE_WINDOW
}

pub(crate) const RANDOM_ROTATION_BUCKET_SECS: u64 = 15 * 60;

pub(crate) fn random_rotation_seed_for_time(unix_secs: u64) -> u64 {
	unix_secs / RANDOM_ROTATION_BUCKET_SECS
}

pub fn get_cached_source_page_with_mode(source: &FeedSource, page: usize, per_page: usize, mode: CacheServeMode) -> Option<(Vec<Post>, String, bool, bool)> {
	let entry = cached_source_posts_with_mode(source, mode)?;
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

pub fn servable_cached_source_posts(source: &FeedSource) -> Option<Vec<Post>> {
	cached_source_posts_with_mode(source, CacheServeMode::Normal).map(|entry| entry.posts)
}

pub fn get_cached_subreddit_page_with_mode(subreddit: &str, page: usize, per_page: usize, mode: CacheServeMode) -> Option<(Vec<Post>, String, bool, bool)> {
	get_cached_source_page_with_mode(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New), page, per_page, mode)
}

pub fn get_cached_subreddit_page(subreddit: &str, page: usize, per_page: usize) -> Option<(Vec<Post>, String, bool, bool)> {
	get_cached_subreddit_page_with_mode(subreddit, page, per_page, CacheServeMode::Normal)
}

#[cfg(test)]
fn store_source_posts(source: &FeedSource, posts: Vec<Post>, after: String) {
	store_source_posts_with_origin(source, posts, after, None);
}

fn resident_cached_source_origin(source: &FeedSource) -> Option<RefreshOrigin> {
	let Ok(cache) = SOURCE_CACHE.lock() else {
		return None;
	};
	// Catalog-backed feeds can intentionally serve a persisted daemon window after its
	// live-fetch TTL has elapsed, so retain the provenance even when that resident entry
	// would no longer satisfy a direct source request.
	cache.get(&source.cache_key()).and_then(|entry| entry.refresh_origin)
}

fn cached_source_posts_with_mode(source: &FeedSource, mode: CacheServeMode) -> Option<CachedSourcePosts> {
	if !deleted_media_safety_healthy() {
		return None;
	}
	let cache_key = source.cache_key();
	if let Ok(mut cache) = SOURCE_CACHE.lock() {
		if let Some(entry) = cache.get_mut(&cache_key) {
			if source_cache_is_servable_with_mode(source, entry, mode) {
				entry.last_accessed = Instant::now();
				return Some(entry.clone());
			}
		}
	}

	load_source_entry_from_disk(source, mode)
}

fn cached_source_posts(source: &FeedSource) -> Option<CachedSourcePosts> {
	cached_source_posts_with_mode(source, CacheServeMode::Normal)
}

fn source_has_resident_servable_cache(source: &FeedSource) -> bool {
	let Ok(cache) = SOURCE_CACHE.lock() else {
		return false;
	};
	cache.get(&source.cache_key()).is_some_and(|entry| source_cache_is_servable(source, entry))
}

fn source_has_persisted_cache_entry(source: &FeedSource) -> bool {
	#[cfg(test)]
	{
		let _ = source;
		return false;
	}
	#[cfg(not(test))]
	{
		crate::cache_persistence::source_cache_entry_exists(&source.cache_key())
	}
}

fn resolve_source_refresh_result(source: &FeedSource, published: SourceRefreshOutcome, candidate: CachedSourcePosts) -> Result<CachedSourcePosts, String> {
	match published {
		SourceRefreshOutcome::Committed => Ok(candidate),
		SourceRefreshOutcome::Stale => cached_source_posts_with_mode(source, CacheServeMode::AllowStaleOnOauthOutage).ok_or_else(|| {
			format!(
				"source refresh for {} was superseded before an authoritative window became available",
				source.display_name()
			)
		}),
		SourceRefreshOutcome::Failed => cached_source_posts_with_mode(source, CacheServeMode::AllowStaleOnOauthOutage)
			.ok_or_else(|| format!("durable source cache publication failed for {}", source.display_name())),
	}
}

async fn fetch_source_posts(source: FeedSource, quarantine: bool, refresh_origin: RefreshOrigin) -> Result<CachedSourcePosts, String> {
	if let Some(entry) = cached_source_posts(&source) {
		return Ok(entry);
	}

	if matches!(refresh_origin, RefreshOrigin::FrontPageSession) && crate::instance_scrape::reddit_rate_limited_now() {
		if let Some(entry) = cached_source_posts_with_mode(&source, CacheServeMode::AllowStaleOnOauthOutage) {
			warn!("post_cache: serving stale cached source {} because Reddit is rate limited", source.display_name());
			return Ok(entry);
		}
		return Err("Reddit rate limited; skipping front-page warm".to_string());
	}

	let remembered_source = remembered_feed_key_for_source(&source).is_some();
	if let Some(key) = remembered_feed_key_for_source(&source) {
		if !matches!(refresh_origin, RefreshOrigin::DirectLiveFetch) {
			let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
			if remembered.get(&key).and_then(|entry| entry.banned_until).is_some_and(|until| until > Instant::now()) {
				return Err("banned".to_string());
			}
		}
	}
	let refresh_token = if remembered_source {
		begin_remembered_source_refresh(&source)
	} else {
		begin_source_refresh(&source)
	};

	let (posts, after) = match Post::fetch(&source.build_path(), quarantine).await {
		Ok(result) => result,
		Err(error) => {
			if error.contains("oauth_unavailable") {
				if let Some(entry) = cached_source_posts_with_mode(&source, CacheServeMode::AllowStaleOnOauthOutage) {
					warn!("post_cache: serving stale cached source {} because OAuth is unavailable", source.display_name());
					if remembered_feed_key_for_source(&source).is_some() {
						let _ = record_remembered_source_refresh_failure_with_token(&source, &error, refresh_origin, refresh_token);
					}
					return Ok(entry);
				}
			}
			if remembered_feed_key_for_source(&source).is_some() {
				let _ = record_remembered_source_refresh_failure_with_token(&source, &error, refresh_origin, refresh_token);
			}
			return Err(error);
		}
	};
	let mut posts = source.filter_posts(posts);
	if source_should_hydrate_media_fingerprints(&source) {
		crate::utils::hydrate_media_content_fingerprints(&mut posts).await;
	}
	if matches!(&source, FeedSource::UserComments { .. }) {
		posts = crate::user::hydrate_following_comment_context(posts).await;
	}
	let published = if remembered_feed_key_for_source(&source).is_some() {
		publish_remembered_source_refresh_with_token(&source, posts.clone(), after.clone(), refresh_origin, refresh_token)
	} else {
		store_source_posts_with_token(&source, posts.clone(), after.clone(), None, refresh_token)
	};

	let estimated_bytes = estimated_source_bytes(&posts, &after);
	let candidate = CachedSourcePosts {
		source: source.clone(),
		posts,
		after,
		fetched_at: Instant::now(),
		last_accessed: Instant::now(),
		refresh_origin: remembered_feed_key_for_source(&source).map(|_| refresh_origin),
		estimated_bytes,
	};
	resolve_source_refresh_result(&source, published, candidate)
}

fn build_session_id(request: &FeedSessionRequest) -> String {
	let mut hasher = DefaultHasher::new();
	request.quarantine.hash(&mut hasher);
	request.query_fingerprint.hash(&mut hasher);
	request.profile_id.hash(&mut hasher);
	request.cache_revision.hash(&mut hasher);
	request.bundle_followed_user_comments.hash(&mut hasher);
	request.daemon_discovery_order.hash(&mut hasher);
	request.random_rotation_seed.hash(&mut hasher);
	request.merge_sort.hash(&mut hasher);

	let mut filter_values = request.filters.iter().cloned().collect::<Vec<_>>();
	filter_values.sort();
	filter_values.hash(&mut hasher);

	for source in &request.sources {
		source.hash(&mut hasher);
	}

	format!("{:x}", hasher.finish())
}

pub(crate) fn feed_session_id_for_request(request: &FeedSessionRequest) -> String {
	build_session_id(request)
}

fn session_merge_sort(session: &FeedSession) -> FeedSort {
	session
		.request
		.merge_sort
		.or_else(|| session.sources.first().map(|entry| entry.source.sort()))
		.unwrap_or(FeedSort::New)
}

pub(crate) fn session_random_rotation_seed(session: &FeedSession) -> u64 {
	session.request.random_rotation_seed
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
	let use_mixed_feed_index = session_uses_mixed_feed_catalog(session);
	let hot = session_merge_sort(session) == FeedSort::Hot;
	for (index, entry) in session.sources.iter_mut().enumerate() {
		let sqlite_cached = hot && index < INITIAL_SOURCE_WINDOW && crate::cache_sqlite::source_exists(&entry.source.cache_key());
		let cached_for_session =
			source_has_resident_servable_cache(&entry.source) || (use_mixed_feed_index && materialized_catalog::mixed_feed_source_is_cached(&entry.source)) || sqlite_cached;
		if cached_for_session && entry.state != SessionSourceState::Ready {
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
				!crate::promo_detection::post_is_promotional(&post.id)
					&& !crate::utils::post_matches_filter_policy(post, &session.request.filters)
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

fn session_uses_materialized_catalog(session: &FeedSession) -> bool {
	matches!(session_merge_sort(session), FeedSort::New | FeedSort::Random) && session.sources.iter().all(|entry| materialized_catalog::source_is_supported(&entry.source))
}

fn session_uses_mixed_feed_catalog(session: &FeedSession) -> bool {
	session.sources.len() > 1 && session_uses_materialized_catalog(session)
}

fn parse_numeric_score(post: &Post) -> Option<i64> {
	post.score.1.parse::<i64>().ok()
}

fn fnv1a64_update(mut hash: u64, value: &str) -> u64 {
	for byte in value.as_bytes() {
		hash ^= u64::from(*byte);
		hash = hash.wrapping_mul(1_099_511_628_211);
	}
	hash
}

pub(crate) fn deterministic_random_post_sort_key(post: &Post) -> u64 {
	let mut hash = 14_695_981_039_346_656_037u64;
	hash = fnv1a64_update(hash, "redlib-random-feed-v1");
	hash = fnv1a64_update(hash, "\0");
	hash = fnv1a64_update(hash, &post.kind);
	hash = fnv1a64_update(hash, "\0");
	hash = fnv1a64_update(hash, &post.id);
	hash = fnv1a64_update(hash, "\0");
	hash = fnv1a64_update(hash, &post.permalink);
	hash = fnv1a64_update(hash, "\0");
	fnv1a64_update(hash, &post.community)
}

fn mix_random_rotation_seed(mut value: u64) -> u64 {
	value ^= 0x6d2b_79f5_aa65_4727;
	value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
	value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
	value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
	value ^ (value >> 31)
}

pub(crate) fn deterministic_random_post_sort_key_with_seed(post: &Post, rotation_seed: u64) -> u64 {
	let key = deterministic_random_post_sort_key(post);
	if rotation_seed == 0 {
		key
	} else {
		key.wrapping_sub(mix_random_rotation_seed(rotation_seed))
	}
}

pub(crate) fn sort_posts_by_deterministic_random_with_seed(posts: &mut [Post], rotation_seed: u64) {
	posts.sort_by(|left, right| {
		deterministic_random_post_sort_key_with_seed(right, rotation_seed)
			.cmp(&deterministic_random_post_sort_key_with_seed(left, rotation_seed))
			.then_with(|| left.id.cmp(&right.id))
			.then_with(|| left.permalink.cmp(&right.permalink))
	});
}

pub(crate) fn sort_posts_by_deterministic_random(posts: &mut [Post]) {
	sort_posts_by_deterministic_random_with_seed(posts, 0);
}

fn representative_created_ts(posts: &[Post]) -> Option<u64> {
	representative_created_ts_from_values(posts.iter().map(|post| post.created_ts))
}

fn representative_created_ts_from_values(created_ts: impl IntoIterator<Item = u64>) -> Option<u64> {
	let mut created = created_ts.into_iter().filter(|ts| *ts > 0).collect::<Vec<_>>();
	if created.is_empty() {
		return None;
	}
	created.sort_unstable();
	Some(created[created.len() / 2])
}

fn representative_score(posts: &[Post]) -> Option<i64> {
	representative_score_from_values(posts.iter().filter(|post| !post.flags.stickied).filter_map(parse_numeric_score))
}

fn representative_score_from_values(scores: impl IntoIterator<Item = i64>) -> Option<i64> {
	let mut scores = scores.into_iter().collect::<Vec<_>>();
	if scores.len() < 3 {
		return None;
	}
	scores.sort_unstable();
	Some(scores[scores.len() / 2])
}

fn ready_queue_head(queue_index: usize, queue: &ReadySourceQueue, merge_sort: FeedSort) -> Option<ReadyQueueHead> {
	let post = queue.posts.front()?;
	let (primary_metric, secondary_metric) = match merge_sort {
		FeedSort::New => (post.created_ts as i128, post.created_ts),
		FeedSort::Hot => (parse_numeric_score(post).unwrap_or(i64::MIN) as i128, post.created_ts),
		FeedSort::Random => (deterministic_random_post_sort_key(post) as i128, post.created_ts),
	};

	Some(ReadyQueueHead {
		queue_index,
		primary_metric,
		secondary_metric,
		fetched_at: queue.fetched_at,
		source_priority: source_priority(&queue.source),
	})
}

fn merged_posts_for_session_live(session: &FeedSession) -> Vec<Post> {
	let merge_sort = session_merge_sort(session);
	let mut queues = filtered_source_queues(session);

	let mut all_posts = Vec::new();
	let mut seen_ids = HashSet::new();
	let mut queue_heads = BinaryHeap::new();

	for (queue_index, queue) in queues.iter().enumerate() {
		if let Some(head) = ready_queue_head(queue_index, queue, merge_sort) {
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

		if let Some(next_head) = ready_queue_head(head.queue_index, queue, merge_sort) {
			queue_heads.push(next_head);
		}
	}

	finalize_session_posts(session, all_posts)
}

fn retain_following_membership(session: &FeedSession, posts: &mut Vec<Post>) {
	if session.request.bundle_followed_user_comments {
		let followed_authors = session
			.request
			.sources
			.iter()
			.filter_map(|source| match source {
				FeedSource::Subreddit { name, .. } => name.strip_prefix("u_").map(str::to_ascii_lowercase),
				_ => None,
			})
			.collect::<HashSet<_>>();
		posts.retain(|post| !post.following_comments.is_empty() || followed_authors.contains(&post.author.name.to_ascii_lowercase()));
	}
}

fn finalize_session_posts_with_order(session: &FeedSession, all_posts: Vec<Post>, preserve_random_order: bool) -> Vec<Post> {
	let merge_sort = session_merge_sort(session);
	let mut merged = if session.request.bundle_followed_user_comments {
		crate::user::bundle_followed_user_comments(all_posts)
	} else {
		all_posts
	};
	merged = collapse_duplicate_submissions(merged);
	retain_following_membership(session, &mut merged);
	if merge_sort == FeedSort::Random && !preserve_random_order {
		sort_posts_by_deterministic_random_with_seed(&mut merged, session.request.random_rotation_seed);
		return merged;
	}
	if session.request.daemon_discovery_order {
		materialized_catalog::sort_posts_by_daemon_discovery(&mut merged);
		return merged;
	}
	if session.request.bundle_followed_user_comments && merge_sort == FeedSort::New {
		merged.sort_by(|left, right| {
			crate::user::post_feed_activity_ts(right)
				.cmp(&crate::user::post_feed_activity_ts(left))
				.then_with(|| right.id.cmp(&left.id))
		});
		return merged;
	}
	merged
}

fn finalize_session_posts(session: &FeedSession, all_posts: Vec<Post>) -> Vec<Post> {
	finalize_session_posts_with_order(session, all_posts, false)
}

fn sqlite_timeline_selection(session: &FeedSession) -> Option<(i64, crate::cache_sqlite::TimelineSort)> {
	if session.request.profile_id.is_empty() || session_merge_sort(session) == FeedSort::Hot {
		return None;
	}
	let sort = if session.request.daemon_discovery_order {
		crate::cache_sqlite::TimelineSort::Discovered
	} else {
		match session_merge_sort(session) {
			FeedSort::New => crate::cache_sqlite::TimelineSort::New,
			FeedSort::Random => crate::cache_sqlite::TimelineSort::Random,
			FeedSort::Hot => return None,
		}
	};
	let view_kind = if session.request.bundle_followed_user_comments { 2 } else { 1 };
	Some((view_kind, sort))
}

fn sqlite_timeline_page_for_session(session: &FeedSession, page: usize, per_page: usize) -> Result<Option<(Vec<Post>, bool)>, String> {
	let Some((view_kind, sort)) = sqlite_timeline_selection(session) else {
		return Ok(None);
	};
	if sort == crate::cache_sqlite::TimelineSort::Random {
		let random_epoch = i64::try_from(session.request.random_rotation_seed).unwrap_or(i64::MAX);
		crate::cache_sqlite::ensure_profile_random_epoch(session.request.profile_id.as_bytes(), random_epoch)?;
	}
	sqlite_timeline_page_for_session_with_query(session, page, per_page, |cursor, limit| {
		crate::cache_sqlite::query_profile_timeline(session.request.profile_id.as_bytes(), view_kind, sort, cursor, limit, &session.request.filters)
	})
}

fn sqlite_timeline_page_for_session_with_query<F>(session: &FeedSession, page: usize, per_page: usize, mut query: F) -> Result<Option<(Vec<Post>, bool)>, String>
where
	F: FnMut(Option<&crate::cache_sqlite::TimelineCursor>, usize) -> Result<Option<crate::cache_sqlite::TimelinePage>, String>,
{
	const MAX_CURSOR_SESSIONS: usize = 64;
	const MAX_CURSOR_PAGES_PER_SESSION: usize = 2_048;
	const MAX_FILTER_FILL_SCANS: usize = 4;

	if page >= MAX_CURSOR_PAGES_PER_SESSION {
		return Err(format!("SQLite timeline page {page} exceeds the bounded cursor window"));
	}
	let excluded_prior_ids = frozen_post_ids_before_page(session, page);
	let exclusion_fill_scans = excluded_prior_ids.len().saturating_add(per_page.saturating_sub(1)) / per_page.max(1);
	let max_filter_fill_scans = MAX_FILTER_FILL_SCANS.saturating_add(exclusion_fill_scans);
	let mut cursors = SQLITE_SESSION_CURSORS.lock().map_err(|_| "SQLite session cursor lock poisoned".to_string())?;
	cursors.retain(|_, state| state.last_accessed.elapsed().as_secs() <= CACHE_TTL_SECS);
	if !cursors.contains_key(&session.id) && cursors.len() >= MAX_CURSOR_SESSIONS {
		if let Some(oldest) = cursors.iter().max_by_key(|(_, state)| state.last_accessed.elapsed()).map(|(id, _)| id.clone()) {
			cursors.remove(&oldest);
		}
	}
	let state = cursors.entry(session.id.clone()).or_insert_with(|| SqliteSessionCursorState {
		last_accessed: Instant::now(),
		after_page: HashMap::new(),
	});
	state.last_accessed = Instant::now();

	let (mut logical_page, mut cursor) = if page == 0 {
		(0, None)
	} else if let Some(cursor) = state.after_page.get(&(page - 1)).cloned() {
		(page, Some(cursor))
	} else if !excluded_prior_ids.is_empty() {
		// A missing exact continuation cursor can follow cursor eviction, a prior
		// legacy fallback, or a process discontinuity. Restart at the current head
		// for the requested page and explicitly remove every already-painted ID;
		// replaying logical pages here would skip an extra page after exclusions.
		(page, None)
	} else if let Some((prior_page, cursor)) = state
		.after_page
		.iter()
		.filter(|(completed_page, _)| **completed_page < page)
		.max_by_key(|(completed_page, _)| **completed_page)
		.map(|(completed_page, cursor)| (*completed_page, cursor.clone()))
	{
		(prior_page + 1, Some(cursor))
	} else {
		(0, None)
	};

	loop {
		let mut posts = Vec::with_capacity(per_page);
		let mut after = cursor.clone();
		for _ in 0..max_filter_fill_scans {
			let remaining = per_page.saturating_sub(posts.len());
			if remaining == 0 {
				break;
			}
			let Some(batch) = query(after.as_ref(), remaining)? else {
				return Ok(None);
			};
			if let Some(cursor) = batch.next.as_ref() {
				crate::cache_sqlite::pin_profile_generation(session.request.profile_id.as_bytes(), &session.id, cursor.generation);
			}
			let mut batch_posts = batch.posts.into_iter().map(crate::cache_persistence::PersistedPost::into_runtime).collect::<Vec<_>>();
			batch_posts.retain(|post| !excluded_prior_ids.contains(&post.id));
			retain_following_membership(session, &mut batch_posts);
			posts.extend(batch_posts);
			after = batch.next;
			if after.is_none() {
				break;
			}
		}
		if let Some(after_cursor) = after.clone() {
			state.after_page.insert(logical_page, after_cursor);
		} else {
			state.after_page.remove(&logical_page);
		}
		if logical_page == page {
			return Ok(Some((posts, after.is_some())));
		}
		if after.is_none() {
			return Ok(Some((Vec::new(), false)));
		}
		cursor = after;
		logical_page += 1;
	}
}

fn merged_posts_for_session(session: &FeedSession) -> Vec<Post> {
	if !deleted_media_safety_healthy() {
		return Vec::new();
	}
	if let Some((view_kind, sort)) = sqlite_timeline_selection(session) {
		let target = background_target_posts(session).clamp(POSTS_PER_PAGE, 250);
		match crate::cache_sqlite::query_profile_timeline(session.request.profile_id.as_bytes(), view_kind, sort, None, target, &session.request.filters) {
			Ok(Some(page)) => return page.posts.into_iter().map(crate::cache_persistence::PersistedPost::into_runtime).collect(),
			Ok(None) => {}
			Err(error) => warn!("post_cache: SQLite timeline read failed; using legacy merge: {error}"),
		}
	}
	if session_uses_materialized_catalog(session) {
		// The materialized catalog is ordered, but it still requires scanning candidates.
		// Bound the scan by the session's existing background target so request-time work
		// scales with page depth rather than total catalog size.
		const CATALOG_OVERFETCH_FACTOR: usize = 3;
		const CATALOG_MAX_CANDIDATES: usize = 5_000;
		let target = background_target_posts(session);
		let max_candidates = if session.request.bundle_followed_user_comments {
			100_000
		} else {
			CATALOG_MAX_CANDIDATES
		};
		let limit = target.saturating_mul(CATALOG_OVERFETCH_FACTOR).min(max_candidates);
		if session_uses_mixed_feed_catalog(session) {
			if let Some(mut posts) = materialized_catalog::query_mixed_feed_session_stream_limited(session, limit) {
				let mut seen_ids = posts.iter().map(|post| post.id.clone()).collect::<HashSet<_>>();
				for post in materialized_catalog::query_session_stream_limited(session, limit) {
					if posts.len() >= limit {
						break;
					}
					if seen_ids.insert(post.id.clone()) {
						posts.push(post);
					}
				}
				if session_merge_sort(session) == FeedSort::New {
					posts.sort_by(|left, right| right.created_ts.cmp(&left.created_ts).then_with(|| right.id.cmp(&left.id)));
				}
				return finalize_session_posts_with_order(session, posts, true);
			}
		}
		return finalize_session_posts(session, materialized_catalog::query_session_stream_limited(session, limit));
	}

	merged_posts_for_session_live(session)
}

fn frozen_post_ids_before_page(session: &FeedSession, page: usize) -> HashSet<String> {
	session
		.frozen_pages
		.iter()
		.filter(|(p, _)| **p < page)
		.flat_map(|(_, posts)| posts.iter().map(|post| post.id.clone()))
		.collect()
}

fn merged_tail_excluding_frozen_prior_pages(session: &FeedSession, page: usize, merged: &[Post]) -> Vec<Post> {
	let exclude = frozen_post_ids_before_page(session, page);
	if exclude.is_empty() {
		return merged.to_vec();
	}
	merged.iter().filter(|post| !exclude.contains(&post.id)).cloned().collect()
}

fn current_page_is_ready(session: &FeedSession, posts: &[Post], page: usize, per_page: usize) -> bool {
	if session.frozen_pages.contains_key(&page) {
		return true;
	}
	let tail = merged_tail_excluding_frozen_prior_pages(session, page, posts);
	tail.len() >= per_page || session.background_complete
}

fn finalize_page_posts(sort: FeedSort, bundle_activity_sort: bool, daemon_discovery_order: bool, mut posts: Vec<Post>) -> Vec<Post> {
	if daemon_discovery_order {
		materialized_catalog::sort_posts_by_daemon_discovery(&mut posts);
		return posts;
	}
	match sort {
		FeedSort::New => {
			if bundle_activity_sort {
				posts.sort_by(|a, b| crate::user::post_feed_activity_ts(b).cmp(&crate::user::post_feed_activity_ts(a)));
			} else {
				posts.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
			}
			posts
		}
		FeedSort::Hot => posts,
		FeedSort::Random => posts,
	}
}

/// `/following` uses two session sources per person (submitted + comments). For status text and
/// [`FeedSessionStatus`] tallies, count each followed user once; `ready` requires both arms Ready.
fn bundled_followed_account_progress(session: &FeedSession) -> Option<(usize, usize)> {
	if !session.request.bundle_followed_user_comments {
		return None;
	}
	#[derive(Default)]
	struct Arms {
		submitted: Option<SessionSourceState>,
		comments: Option<SessionSourceState>,
	}
	let mut by_user: HashMap<String, Arms> = HashMap::new();
	for entry in &session.sources {
		match &entry.source {
			FeedSource::Subreddit { name, .. } if name.starts_with("u_") => {
				by_user.entry(name.to_lowercase()).or_default().submitted = Some(entry.state);
			}
			FeedSource::UserComments { name, .. } if name.starts_with("u_") => {
				by_user.entry(name.to_lowercase()).or_default().comments = Some(entry.state);
			}
			_ => {}
		}
	}
	if by_user.is_empty() {
		return None;
	}
	let total = by_user.len();
	let ready = by_user
		.values()
		.filter(|arms| matches!(arms.submitted, Some(SessionSourceState::Ready)) && matches!(arms.comments, Some(SessionSourceState::Ready)))
		.count();
	Some((total, ready))
}

fn session_status_from_posts(session: &FeedSession, posts: &[Post], page: usize, per_page: usize) -> FeedSessionStatus {
	let raw_ready_sources = session.sources.iter().filter(|entry| entry.state == SessionSourceState::Ready).count();
	let daemon_cached_sources = session
		.sources
		.iter()
		.filter(|entry| entry.state == SessionSourceState::Ready && resident_cached_source_origin(&entry.source) == Some(RefreshOrigin::Daemon))
		.count();
	let failed_sources = session.sources.iter().filter(|entry| entry.state == SessionSourceState::Failed).count();
	let total_user_sources = session
		.sources
		.iter()
		.filter(|entry| {
			matches!(&entry.source, FeedSource::Subreddit { name, .. } if name.starts_with("u_"))
				|| matches!(&entry.source, FeedSource::UserComments { name, .. } if name.starts_with("u_"))
		})
		.count();
	let ready_user_sources = session
		.sources
		.iter()
		.filter(|entry| entry.state == SessionSourceState::Ready)
		.filter(|entry| {
			matches!(&entry.source, FeedSource::Subreddit { name, .. } if name.starts_with("u_"))
				|| matches!(&entry.source, FeedSource::UserComments { name, .. } if name.starts_with("u_"))
		})
		.count();
	let bundle_accounts = bundled_followed_account_progress(session);
	let (status_total, status_ready) = if let Some((total, ready)) = bundle_accounts {
		(total, ready)
	} else {
		(session.sources.len(), raw_ready_sources)
	};
	let feed_word = if bundle_accounts.is_some() { "followed accounts" } else { "feeds" };
	let user_feeds_still_loading = if let Some((total, ready)) = bundle_accounts {
		ready < total
	} else {
		ready_user_sources < total_user_sources
	};
	let current_page_ready = current_page_is_ready(session, posts, page, per_page);
	let tail_next = merged_tail_excluding_frozen_prior_pages(session, page.saturating_add(1), posts);
	let next_page_ready = tail_next.len() >= per_page || session.background_complete;
	let warming = session.background_active;
	let daemon_cache_suffix = if daemon_cached_sources > 0 {
		format!(" ({daemon_cached_sources} from daemon cache)")
	} else {
		String::new()
	};
	let status_message = if warming {
		if user_feeds_still_loading {
			format!(
				"Loading followed feeds in the background. Warmed {} of {} {}{}.",
				status_ready, status_total, feed_word, daemon_cache_suffix
			)
		} else if !current_page_ready {
			format!(
				"Warmed {} of {} {}{}. Filling this page with newer posts.",
				status_ready, status_total, feed_word, daemon_cache_suffix
			)
		} else if next_page_ready {
			format!("Warmed {} of {} {}{}. Next page is ready.", status_ready, status_total, feed_word, daemon_cache_suffix)
		} else {
			format!(
				"Warmed {} of {} {}{}. Preparing the next page in the background.",
				status_ready, status_total, feed_word, daemon_cache_suffix
			)
		}
	} else if session.background_complete {
		format!("Warmed all {} {}{}.", status_total, feed_word, daemon_cache_suffix)
	} else if next_page_ready {
		format!(
			"Warmed {} of {} {}{}. More feeds will load as you paginate.",
			status_ready, status_total, feed_word, daemon_cache_suffix
		)
	} else {
		format!(
			"Warmed {} of {} {}{} for now. More feeds will load as needed.",
			status_ready, status_total, feed_word, daemon_cache_suffix
		)
	};

	FeedSessionStatus {
		session_id: session.id.clone(),
		total_sources: status_total,
		ready_sources: status_ready,
		daemon_cached_sources,
		failed_sources,
		warming,
		current_page_ready,
		next_page_ready,
		posts_merged: posts.len(),
		status_message,
		refresh_token: session.revision ^ ((posts.len() as u64) << 16) ^ (status_ready as u64) ^ ((daemon_cached_sources as u64) << 8),
	}
}

fn touch_session(session: &mut FeedSession) {
	session.last_updated = Instant::now();
	session.revision = session.revision.wrapping_add(1);
}

#[derive(Default)]
struct DeletedMediaPruneResult {
	primary_media_removed: usize,
	posts_removed: usize,
	changed: bool,
}

fn post_has_non_media_content(post: &Post) -> bool {
	!post.body.trim().is_empty() || !post.preview_body.trim().is_empty() || post.poll.is_some() || post.kind == "t1"
}

fn post_has_primary_media(post: &Post) -> bool {
	match post.post_type.as_str() {
		"image" => !post.media.url.is_empty() || !post.media.preview_url.is_empty(),
		"gallery" => !post.gallery.is_empty(),
		"video" | "gif" => !post.media.url.is_empty() || !post.media.hls_url.is_empty() || !post.media.dash_url.is_empty() || !post.media.embed_code.is_empty(),
		"link" => !post.media.url.is_empty(),
		_ => false,
	}
}

fn prune_deleted_media_from_post(post: &mut Post, deleted_asset_key: &str) -> (bool, bool) {
	let mut changed = false;
	let mut primary_media_removed = false;

	if crate::utils::media_references_reddit_image_asset(&post.thumbnail, deleted_asset_key) {
		post.thumbnail = crate::utils::Media::empty();
		changed = true;
	}

	match post.post_type.as_str() {
		"image" => {
			if crate::utils::media_references_reddit_image_asset(&post.media, deleted_asset_key) {
				post.media = crate::utils::Media::empty();
				post.post_type = "self".to_string();
				changed = true;
				primary_media_removed = true;
			}
		}
		"gallery" => {
			let before = post.gallery.len();
			post
				.gallery
				.retain(|image| !crate::utils::gallery_media_references_reddit_image_asset(image, deleted_asset_key));
			if post.gallery.len() != before {
				changed = true;
				primary_media_removed = true;
				if post.gallery.is_empty() {
					post.post_type = "self".to_string();
				}
			}
		}
		_ => {}
	}

	(changed, primary_media_removed)
}

fn prune_deleted_media_from_posts(posts: &mut Vec<Post>, deleted_asset_key: &str) -> DeletedMediaPruneResult {
	let mut result = DeletedMediaPruneResult::default();

	posts.retain_mut(|post| {
		let (changed, primary_media_removed) = prune_deleted_media_from_post(post, deleted_asset_key);
		if !changed {
			return true;
		}

		result.changed = true;
		if primary_media_removed {
			result.primary_media_removed += 1;
			if !post_has_non_media_content(post) && !post_has_primary_media(post) {
				result.posts_removed += 1;
				return false;
			}
		}

		true
	});

	result
}

fn record_deleted_media_tombstone(deleted_asset_key: &str) -> bool {
	// Serialize snapshot/write/insert ordering without holding the tombstone
	// read mutex across fsync. Otherwise two concurrent deletions could write an
	// older, smaller snapshot after a newer one.
	let _persist_guard = DELETED_MEDIA_PERSIST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
	let mut snapshot = {
		let tombstones = DELETED_MEDIA_TOMBSTONES.lock().unwrap_or_else(|error| error.into_inner());
		tombstones.iter().cloned().collect::<Vec<_>>()
	};
	if !snapshot.iter().any(|key| key == deleted_asset_key) {
		snapshot.push(deleted_asset_key.to_string());
	}
	let restart_guarded = if deleted_media_safety_healthy() {
		match persist_deleted_media_tombstone_snapshot(&snapshot) {
			Ok(()) => {
				DELETED_MEDIA_TOMBSTONES
					.lock()
					.unwrap_or_else(|error| error.into_inner())
					.insert(deleted_asset_key.to_string());
				return true;
			}
			Err((error, restart_guarded)) => {
				mark_deleted_media_safety_unhealthy();
				warn!("post_cache: failed to persist deleted-media tombstone; durable cache reads are disabled: {error}");
				restart_guarded
			}
		}
	} else {
		false
	};
	restart_guarded
}

fn persist_deleted_media_tombstone_snapshot(snapshot: &[String]) -> Result<(), (String, bool)> {
	#[cfg(not(test))]
	{
		crate::cache_persistence::write_deleted_media_tombstones(snapshot)
	}
	#[cfg(test)]
	{
		let _ = snapshot;
		if TEST_TOMBSTONE_MARKER_WRITE_FAIL.load(AtomicOrdering::SeqCst) {
			Err(("injected deleted-media safety-marker write failure".to_string(), false))
		} else if TEST_TOMBSTONE_WRITE_FAIL.load(AtomicOrdering::SeqCst) {
			Err(("injected deleted-media tombstone write failure".to_string(), true))
		} else {
			Ok(())
		}
	}
}

pub(crate) fn restore_deleted_media_tombstones(asset_keys: Vec<String>) {
	let mut tombstones = DELETED_MEDIA_TOMBSTONES.lock().unwrap_or_else(|error| error.into_inner());
	*tombstones = asset_keys.into_iter().filter(|key| !key.trim().is_empty()).collect();
	DELETED_MEDIA_SAFETY_HEALTHY.store(true, AtomicOrdering::Release);
}

pub(crate) fn mark_deleted_media_safety_unhealthy() {
	DELETED_MEDIA_SAFETY_HEALTHY.store(false, AtomicOrdering::Release);
	crate::cache_sqlite::mark_reads_unhealthy();
}

pub(crate) fn deleted_media_safety_healthy() -> bool {
	DELETED_MEDIA_SAFETY_HEALTHY.load(AtomicOrdering::Acquire)
}

pub(crate) fn deleted_media_tombstone_asset_keys() -> Vec<String> {
	let mut keys = DELETED_MEDIA_TOMBSTONES
		.lock()
		.unwrap_or_else(|error| error.into_inner())
		.iter()
		.cloned()
		.collect::<Vec<_>>();
	keys.sort();
	keys
}

#[cfg(test)]
pub(crate) fn test_state_lock() -> std::sync::MutexGuard<'static, ()> {
	TEST_STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

fn prune_known_deleted_media_from_posts(posts: &mut Vec<Post>) -> bool {
	let deleted_asset_keys = {
		let tombstones = DELETED_MEDIA_TOMBSTONES.lock().unwrap_or_else(|error| error.into_inner());
		tombstones.iter().cloned().collect::<Vec<_>>()
	};
	let mut changed = false;
	for deleted_asset_key in deleted_asset_keys {
		changed |= prune_deleted_media_from_posts(posts, &deleted_asset_key).changed;
	}
	changed
}

pub(crate) fn prune_known_deleted_media_from_persisted_posts(posts: &mut Vec<crate::cache_persistence::PersistedPost>) -> bool {
	if DELETED_MEDIA_TOMBSTONES.lock().unwrap_or_else(|error| error.into_inner()).is_empty() {
		return false;
	}
	let mut runtime_posts = std::mem::take(posts)
		.into_iter()
		.map(crate::cache_persistence::PersistedPost::into_runtime)
		.collect::<Vec<_>>();
	let changed = prune_known_deleted_media_from_posts(&mut runtime_posts);
	*posts = runtime_posts.iter().map(crate::cache_persistence::PersistedPost::from_runtime).collect();
	changed
}

pub(crate) fn mark_deleted_reddit_media_url(media_url: &str) -> usize {
	let Some(deleted_asset_key) = crate::utils::reddit_image_asset_key(media_url) else {
		return 0;
	};

	if !record_deleted_media_tombstone(&deleted_asset_key) {
		return 0;
	}
	// Existing SQLite payload/materialized rows may predate this tombstone.
	// Close the optional read cutover immediately; restart revalidates the
	// migration against the durable tombstone revision before reopening reads.
	crate::cache_sqlite::mark_reads_unhealthy();
	// Include first fetches that have not published a resident source yet. A
	// commit already in progress finishes before its generation is invalidated
	// and is then included in the resident snapshot below; a network fetch still
	// in flight becomes stale and cannot reintroduce the deleted asset.
	invalidate_active_source_refreshes();
	let sources = SOURCE_CACHE
		.lock()
		.unwrap_or_else(|error| error.into_inner())
		.values()
		.map(|entry| entry.source.clone())
		.collect::<Vec<_>>();
	let mut updated_sources = 0usize;
	let mut removed_primary_media = 0;
	let mut removed_posts = 0;
	for source in sources {
		let cache_key = source.cache_key();
		let state = source_refresh_state(&cache_key);
		let mut generation = state.lock().unwrap_or_else(|error| error.into_inner());
		let Some(mut updated_entry) = SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&cache_key).cloned() else {
			drop(generation);
			release_source_refresh_state(&cache_key, &state);
			continue;
		};
		let result = prune_deleted_media_from_posts(&mut updated_entry.posts, &deleted_asset_key);
		if result.changed {
			*generation = (*generation).saturating_add(1);
		}
		if result.changed {
			if !write_source_entry_to_disk(&cache_key, &updated_entry) {
				mark_deleted_media_safety_unhealthy();
			}
			removed_primary_media += result.primary_media_removed;
			removed_posts += result.posts_removed;
			if materialized_catalog::source_is_supported(&updated_entry.source) {
				materialized_catalog::apply_source_refresh(&updated_entry.source, &updated_entry.posts);
				apply_legacy_mixed_feed_source_refresh(&updated_entry.source, &updated_entry.posts);
			}
			SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).insert(cache_key.clone(), updated_entry);
			updated_sources += 1;
		}
		drop(generation);
		release_source_refresh_state(&cache_key, &state);
	}
	let removed_history_media = materialized_catalog::mark_deleted_reddit_media_asset(&deleted_asset_key);

	let mut updated_sessions = 0;
	if let Ok(mut sessions) = FEED_SESSIONS.lock() {
		for session in sessions.values_mut() {
			let mut session_changed = false;
			for posts in session.frozen_pages.values_mut() {
				let result = prune_deleted_media_from_posts(posts, &deleted_asset_key);
				if result.changed {
					removed_primary_media += result.primary_media_removed;
					removed_posts += result.posts_removed;
					session_changed = true;
				}
			}
			if session_changed {
				updated_sessions += 1;
				touch_session(session);
			}
		}
	}

	if updated_sources > 0 {
		bump_source_cache_revision();
		crate::cache_persistence::mark_dirty();
	}

	if updated_sources > 0 || updated_sessions > 0 || removed_history_media > 0 {
		info!(
			"post_cache: marked deleted Reddit media asset {} (sources={}, sessions={}, history_media_removed={}, primary_media_removed={}, posts_removed={})",
			deleted_asset_key, updated_sources, updated_sessions, removed_history_media, removed_primary_media, removed_posts
		);
	}

	removed_primary_media
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
	source_budget_target(session.requested_page + BACKGROUND_LOOKAHEAD_PAGES, POSTS_PER_PAGE)
		.min(BACKGROUND_DIVERSITY_SOURCE_CAP)
		.min(session.sources.len())
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
	merged_posts_for_session(session).len() >= background_target_posts(session) || attempted_source_count(session) >= background_target_sources(session)
}

fn session_has_remaining_sources(session: &FeedSession) -> bool {
	session.sources.iter().any(|entry| entry.state == SessionSourceState::Unstarted)
}

fn session_has_remaining_persisted_cache_sources(session: &FeedSession) -> bool {
	session
		.sources
		.iter()
		.any(|entry| entry.state == SessionSourceState::Unstarted && source_has_persisted_cache_entry(&entry.source))
}

fn remembered_entry_for_source<'a>(remembered: &'a HashMap<String, RememberedSubredditEntry>, source: &FeedSource) -> Option<&'a RememberedSubredditEntry> {
	remembered_feed_key_for_source(source).and_then(|key| remembered.get(&key))
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
		.map(|(index, entry)| (index, remembered_entry_for_source(&remembered, &entry.source).cloned()))
		.collect::<Vec<_>>();
	drop(remembered);
	let mut targets = targets
		.drain(..)
		.map(|(index, remembered_entry)| (index, source_has_persisted_cache_entry(&session.sources[index].source), remembered_entry))
		.collect::<Vec<_>>();

	targets.sort_by(|(left_index, left_persisted, left_entry), (right_index, right_persisted, right_entry)| {
		let left_source = &session.sources[*left_index].source;
		let right_source = &session.sources[*right_index].source;
		match right_persisted.cmp(left_persisted) {
			Ordering::Equal => {}
			ordering => return ordering,
		}
		compare_warm_target_priority(*left_index, left_source, left_entry.as_ref(), *right_index, right_source, right_entry.as_ref())
	});
	targets.truncate(target_count);
	targets.into_iter().map(|(index, _, _)| index).collect()
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
			let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get_mut(session_id) else {
				return;
			};
			let merged = merged_posts_for_session(session);
			let target = (page + 1) * per_page;
			let attempted_sources = attempted_source_count(session);
			let remaining_budget = max_sources.saturating_sub(attempted_sources).min(PAGE_FILL_BATCH);
			let page_ready = merged.len() >= target;
			if page_ready {
				freeze_feed_page_from_merged_if_available(session, page, per_page, &merged);
			}
			let remaining = if page_ready { Vec::new() } else { warm_targets(session, remaining_budget) };
			(page_ready, attempted_sources, remaining)
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
		let session = {
			let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get(&session_id) else {
				return;
			};
			session.clone()
		};
		let target_reached = background_target_reached(&session);
		let remaining = warm_targets(&session, PAGE_FILL_BATCH);
		let remaining_persisted_cache_sources = session_has_remaining_persisted_cache_sources(&session);

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

		if target_reached && !remaining_persisted_cache_sources {
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
	if session.frozen_pages.contains_key(&page) {
		return true;
	}
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

async fn ensure_feed_session_inner(request: FeedSessionRequest, page: usize, per_page: usize, max_sync_sources_override: Option<usize>) -> String {
	let session_id = build_session_id(&request);
	reconcile_stuck_fetching_sources(&session_id).await;

	let session_is_new = {
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		prune_feed_sessions_locked(&mut sessions, &session_id);

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
		let sync_sources = sync_sources_to_request(session, page);
		warm_targets(session, max_sync_sources_override.unwrap_or(sync_sources).min(sync_sources).min(PAGE_FILL_BATCH))
	} else {
		Vec::new()
	};

	warm_session_indices(&session_id, sync_target).await;
	if session_is_new {
		warm_session_to_fill(&session_id, page, per_page, max_sync_sources_override.unwrap_or_else(|| max_sync_source_budget(page))).await;
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

pub async fn ensure_feed_session(request: FeedSessionRequest, page: usize, per_page: usize) -> String {
	ensure_feed_session_inner(request, page, per_page, None).await
}

pub async fn ensure_feed_session_with_budget(request: FeedSessionRequest, page: usize, per_page: usize, max_sync_sources: usize) -> String {
	ensure_feed_session_inner(request, page, per_page, Some(max_sync_sources)).await
}

pub async fn continue_feed_session_with_budget(session_id: &str, page: usize, per_page: usize, max_sync_sources: usize) -> bool {
	if session_id.trim().is_empty() {
		return false;
	}

	reconcile_stuck_fetching_sources(session_id).await;
	{
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		sessions.retain(|_, session| !session_is_expired(session));
		let Some(session) = sessions.get_mut(session_id) else {
			return false;
		};
		session.requested_page = session.requested_page.max(page);
		reconcile_session_with_source_cache(session);
		touch_session(session);
	}

	let needs_sync_fill = {
		let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get(session_id) else {
			return false;
		};
		!session_requested_page_ready(session, page, per_page)
	};
	if needs_sync_fill {
		warm_session_to_fill(session_id, page, per_page, max_sync_sources).await;
	}

	let warm_mode = {
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get_mut(session_id) else {
			return false;
		};
		plan_session_warm_mode(session, page, per_page, false)
	};

	if let Some(warm_mode) = warm_mode {
		let background_id = session_id.to_string();
		tokio::spawn(async move {
			match warm_mode {
				SessionWarmMode::InitialLoad => warm_session_background(background_id).await,
				SessionWarmMode::FollowupCachePreferred { max_sources } => {
					warm_session_followup_cached(background_id, page, per_page, max_sources).await;
				}
			}
		});
	}

	true
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
	if !deleted_media_safety_healthy() {
		return None;
	}
	let mut sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get_mut(session_id)?;
	if session_is_expired(session) {
		return None;
	}
	reconcile_session_with_source_cache(session);
	let has_more_available = session.background_active || page < session.requested_page || session_has_remaining_sources(session);
	if let Some(posts) = session.frozen_pages.get(&page).cloned() {
		let has_next = has_more_available || posts.len() >= per_page;
		return Some(FeedPage {
			posts,
			has_prev: page > 0,
			has_next,
			ready: true,
		});
	}
	match sqlite_timeline_page_for_session(session, page, per_page) {
		Ok(Some((posts, has_next))) => {
			freeze_feed_page(session, page, posts.clone());
			return Some(FeedPage {
				posts,
				has_prev: page > 0,
				has_next,
				ready: true,
			});
		}
		Ok(None) => {}
		Err(error) => warn!("post_cache: SQLite timeline page failed; using legacy merge: {error}"),
	}

	let merged_posts = merged_posts_for_session(session);
	let page_ready = current_page_is_ready(session, &merged_posts, page, per_page);
	let sort = session_merge_sort(session);
	let bundle_activity = session.request.bundle_followed_user_comments;
	let tail_for_page = merged_tail_excluding_frozen_prior_pages(session, page, &merged_posts);
	let posts = if let Some(frozen_posts) = session.frozen_pages.get(&page) {
		frozen_posts.clone()
	} else {
		let chunk: Vec<Post> = tail_for_page.iter().take(per_page).cloned().collect();
		let finalized = finalize_page_posts(sort, bundle_activity, session.request.daemon_discovery_order, chunk);
		if page_ready {
			freeze_feed_page(session, page, finalized.clone());
		}
		finalized
	};
	let shown_ids: HashSet<String> = posts.iter().map(|post| post.id.clone()).collect();
	let has_next_from_feed = tail_for_page.iter().any(|post| !shown_ids.contains(&post.id));
	Some(FeedPage {
		posts,
		has_prev: page > 0,
		has_next: has_next_from_feed || has_more_available,
		ready: page_ready,
	})
}

fn freeze_feed_page(session: &mut FeedSession, page: usize, posts: Vec<Post>) {
	session.frozen_pages.insert(page, posts);
	while session.frozen_pages.len() > MAX_FROZEN_FEED_PAGES {
		let Some(evict_page) = session
			.frozen_pages
			.keys()
			.filter(|candidate| **candidate != page)
			.max_by_key(|candidate| candidate.abs_diff(page))
			.copied()
		else {
			break;
		};
		session.frozen_pages.remove(&evict_page);
	}
}

fn freeze_feed_page_from_merged_if_available(session: &mut FeedSession, page: usize, per_page: usize, merged_posts: &[Post]) {
	if session.frozen_pages.contains_key(&page) {
		return;
	}
	let sort = session_merge_sort(session);
	let bundle_activity = session.request.bundle_followed_user_comments;
	let tail_for_page = merged_tail_excluding_frozen_prior_pages(session, page, merged_posts);
	let chunk: Vec<Post> = tail_for_page.into_iter().take(per_page).collect();
	if chunk.is_empty() {
		return;
	}
	let finalized = finalize_page_posts(sort, bundle_activity, session.request.daemon_discovery_order, chunk);
	freeze_feed_page(session, page, finalized);
	touch_session(session);
}

fn seal_feed_page_for_paint_if_needed(session_id: &str, page: usize, per_page: usize) {
	let Ok(mut sessions) = FEED_SESSIONS.lock() else {
		return;
	};
	let Some(session) = sessions.get_mut(session_id) else {
		return;
	};
	if session_is_expired(session) || session.frozen_pages.contains_key(&page) {
		return;
	}
	reconcile_session_with_source_cache(session);
	let merged_posts = merged_posts_for_session(session);
	freeze_feed_page_from_merged_if_available(session, page, per_page, &merged_posts);
}

/// Blocks the response until the contextual feed page is ready (full window or background
/// finished) or [`CONTEXTUAL_FEED_PAINT_WAIT`] elapses, then seals page `page` if it is still
/// unfrozen so later merges do not reshuffle that page.
pub async fn wait_and_seal_contextual_feed_paint(session_id: &str, page: usize, per_page: usize) {
	let deadline = tokio::time::Instant::now() + CONTEXTUAL_FEED_PAINT_WAIT;
	loop {
		let ready = {
			let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get_mut(session_id) else {
				return;
			};
			if session_is_expired(session) {
				return;
			}
			if session.frozen_pages.contains_key(&page) {
				true
			} else {
				reconcile_session_with_source_cache(session);
				let merged_posts = merged_posts_for_session(session);
				current_page_is_ready(session, &merged_posts, page, per_page)
			}
		};
		if ready || tokio::time::Instant::now() >= deadline {
			break;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	seal_feed_page_for_paint_if_needed(session_id, page, per_page);
}

pub fn feed_status(session_id: &str, page: usize, per_page: usize) -> Option<FeedSessionStatus> {
	let mut sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get_mut(session_id)?;
	if session_is_expired(session) {
		return None;
	}
	reconcile_session_with_source_cache(session);
	if let Some(posts) = session.frozen_pages.get(&page).cloned() {
		return Some(session_status_from_posts(session, &posts, page, per_page));
	}

	let posts = merged_posts_for_session(session);
	Some(session_status_from_posts(session, &posts, page, per_page))
}

/// Returns the fully merged feed posts for a session (pre-pagination).
///
/// This is intended for higher-level pagination strategies (e.g. skipping seen posts)
/// that need to scan beyond a fixed page slice.
pub fn feed_merged_posts(session_id: &str) -> Option<Vec<Post>> {
	let mut sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get_mut(session_id)?;
	if session_is_expired(session) {
		return None;
	}
	reconcile_session_with_source_cache(session);
	Some(merged_posts_for_session(session))
}

pub fn feed_session_page_metrics(session_id: &str, per_page: usize) -> Option<Vec<FeedSessionPageMetric>> {
	let mut sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get_mut(session_id)?;
	if session_is_expired(session) {
		return None;
	}
	reconcile_session_with_source_cache(session);

	let posts = if session_uses_materialized_catalog(session) {
		// Contextual pagination needs enough pages to build age/score buckets, but does not require the full stream.
		const METRICS_TARGET_PAGES: usize = 40;
		const METRICS_OVERFETCH_FACTOR: usize = 3;
		const METRICS_MAX_CANDIDATES: usize = 25_000;
		let target_posts = METRICS_TARGET_PAGES.saturating_mul(per_page).max(background_target_posts(session));
		let limit = target_posts.saturating_mul(METRICS_OVERFETCH_FACTOR).min(METRICS_MAX_CANDIDATES);
		let candidates = if session_uses_mixed_feed_catalog(session) {
			materialized_catalog::query_mixed_feed_session_page_metric_candidates_limited(session, limit)
				.unwrap_or_else(|| materialized_catalog::query_session_page_metric_candidates_limited(session, limit))
		} else {
			materialized_catalog::query_session_page_metric_candidates_limited(session, limit)
		};
		let mut metrics = Vec::new();
		for (page, chunk) in candidates.chunks(per_page).enumerate() {
			let full_page = chunk.len() == per_page;
			metrics.push(FeedSessionPageMetric {
				page,
				full_page,
				ready: full_page || session.background_complete,
				representative_created_ts: representative_created_ts_from_values(chunk.iter().map(|candidate| candidate.created_ts)),
				representative_score: representative_score_from_values(chunk.iter().filter(|candidate| !candidate.stickied).filter_map(|candidate| candidate.score)),
			});
		}
		return Some(metrics);
	} else {
		merged_posts_for_session(session)
	};
	let mut metrics = Vec::new();
	for (page, chunk) in posts.chunks(per_page).enumerate() {
		metrics.push(FeedSessionPageMetric {
			page,
			full_page: chunk.len() == per_page,
			ready: current_page_is_ready(session, &posts, page, per_page),
			representative_created_ts: representative_created_ts(chunk),
			representative_score: representative_score(chunk),
		});
	}

	Some(metrics)
}

pub fn catalog_source_candidates(sources: &[FeedSource], filters: &HashSet<String>) -> Option<Vec<Post>> {
	if !deleted_media_safety_healthy() {
		return None;
	}
	if sources.iter().all(materialized_catalog::source_is_supported) {
		// Video-mode and other aggregate listings only need a bounded candidate pool.
		// Avoid scanning the full catalog for pages with many subscriptions.
		const CATALOG_MAX_CANDIDATES: usize = 8_000;
		if sources.len() > 1 {
			if let Some(posts) = materialized_catalog::query_mixed_feed_source_candidates_limited(sources, filters, CATALOG_MAX_CANDIDATES) {
				return Some(posts);
			}
		}
		Some(materialized_catalog::query_source_candidates_limited(sources, filters, CATALOG_MAX_CANDIDATES))
	} else {
		None
	}
}

pub fn catalog_profile_video_candidates(profile_id: &str, view_kind: i64, sources: &[FeedSource], filters: &HashSet<String>) -> Vec<Post> {
	const CATALOG_MAX_CANDIDATES: usize = 8_000;
	if !profile_id.is_empty() {
		match crate::cache_sqlite::query_profile_video_candidates(profile_id.as_bytes(), view_kind, CATALOG_MAX_CANDIDATES, filters) {
			Ok(Some(posts)) => return posts.into_iter().map(crate::cache_persistence::PersistedPost::into_runtime).collect(),
			Ok(None) => {}
			Err(error) => warn!("post_cache: SQLite video candidate query failed; using legacy catalog: {error}"),
		}
	}
	catalog_source_candidates(sources, filters).unwrap_or_else(|| {
		let mut posts = Vec::new();
		for source in sources {
			if let Some(mut cached) = servable_cached_source_posts(source) {
				posts.append(&mut cached);
			}
		}
		posts
	})
}

pub fn feed_session_sort(session_id: &str) -> Option<FeedSort> {
	let sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get(session_id)?;
	if session_is_expired(session) {
		return None;
	}
	Some(session_merge_sort(session))
}

pub fn cache_stats() -> (usize, usize) {
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
		let Some(source) = normalize_remembered_source(&FeedSource::subreddit(subreddit.to_string(), FeedSort::New)) else {
			continue;
		};
		if seen.insert(source.cache_key()) {
			sources.push(source);
		}
	}

	for subscription in &preferences.flair_subscriptions {
		let Some(source) = normalize_remembered_source(&FeedSource::flair(subscription.subreddit.clone(), subscription.flair.clone(), FeedSort::New)) else {
			continue;
		};
		if seen.insert(source.cache_key()) {
			sources.push(source);
		}
	}

	sort_sources(&mut sources);
	sources
}

pub fn cache_status_seconds_until_daemon_tick() -> u64 {
	if !remembered_feed_daemon_enabled() {
		return 0;
	}
	REMEMBERED_SUBREDDIT_DAEMON_NEXT_TICK_AT
		.lock()
		.ok()
		.and_then(|next_tick| *next_tick)
		.map(|next_tick| next_tick.saturating_duration_since(Instant::now()).as_secs())
		.unwrap_or(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS)
}

fn freshest_cached_post_activity_ts(entry: &CachedSourcePosts) -> Option<u64> {
	let mut best: Option<u64> = None;
	for post in &entry.posts {
		let ts = post_feed_activity_ts(post);
		if ts > 0 {
			best = Some(best.map_or(ts, |b| b.max(ts)));
		}
		for duplicate in &post.collapsed_duplicates {
			if duplicate.created_ts > 0 {
				best = Some(best.map_or(duplicate.created_ts, |b| b.max(duplicate.created_ts)));
			}
		}
	}
	best
}

fn freshest_persisted_cached_post_activity_ts(entry: &PersistedSourceCacheEntry) -> Option<u64> {
	let mut best: Option<u64> = None;
	for post in &entry.posts {
		if post.created_ts > 0 {
			best = Some(best.map_or(post.created_ts, |b| b.max(post.created_ts)));
		}
		for duplicate in &post.collapsed_duplicates {
			if duplicate.created_ts > 0 {
				best = Some(best.map_or(duplicate.created_ts, |b| b.max(duplicate.created_ts)));
			}
		}
	}
	best
}

fn read_persisted_source_entry_for_status(source: &FeedSource) -> Option<PersistedSourceCacheEntry> {
	#[cfg(test)]
	{
		let _ = source;
		return None;
	}
	#[cfg(not(test))]
	{
		let cache_key = source.cache_key();
		let entry = match crate::cache_persistence::read_source_cache_entry(&cache_key) {
			Ok(Some(entry)) => entry,
			Ok(None) => return None,
			Err(error) => {
				warn!("post_cache: failed to read source cache status for {}: {}", source.display_name(), error);
				return None;
			}
		};
		let expected_key = remembered_feed_key_for_source(source)?;
		(entry.subreddit == expected_key).then_some(entry)
	}
}

fn persisted_source_entry_is_servable(entry: &PersistedSourceCacheEntry, remembered_entry: Option<&RememberedSubredditEntry>, now_unix: u64) -> bool {
	remembered_entry.is_some_and(remembered_entry_is_active) || now_unix.saturating_sub(entry.fetched_at_unix) <= CACHE_TTL_SECS
}

fn daemon_next_fetch_map() -> (u64, HashMap<String, usize>, HashSet<String>) {
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let now = Instant::now();
	let entries = remembered
		.values()
		.filter(|entry| remembered_entry_is_active(entry) && !remembered_entry_refresh_blocked_at(entry, now) && !entry.refresh_in_flight)
		.filter(|entry| remembered_feed_source_from_key(&entry.name).is_some())
		.cloned()
		.collect::<Vec<_>>();
	let selection_sequence = REMEMBERED_DAEMON_SELECTION_SEQUENCE.load(AtomicOrdering::Relaxed);
	let entries = ordered_refresh_entries_with_fairness(entries, now, selection_sequence);

	let seconds_until_tick = cache_status_seconds_until_daemon_tick();
	let fairness_sources = entries
		.iter()
		.filter(|(_, fairness)| *fairness)
		.map(|(entry, _)| entry.name.clone())
		.collect::<HashSet<_>>();
	let positions = entries.into_iter().enumerate().map(|(index, (entry, _))| (entry.name, index)).collect::<HashMap<_, _>>();

	(seconds_until_tick, positions, fairness_sources)
}

fn predicted_next_fetch_secs_for_position(seconds_until_tick: u64, position: usize) -> u64 {
	let batch_index = position / REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK.max(1);
	seconds_until_tick.saturating_add((batch_index as u64).saturating_mul(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS))
}

pub fn cache_status_for_preferences(preferences: &Preferences) -> CacheStatusResponse {
	remember_frontpage_subreddits(preferences, false);
	let now = Instant::now();
	let generated_at_unix = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|duration| duration.as_secs())
		.unwrap_or_default();
	let visible_sources = visible_sources_from_preferences(preferences);
	let (seconds_until_tick, queue_positions, fairness_sources) = daemon_next_fetch_map();
	// Snapshot only the small fields used below, then release SOURCE_CACHE
	// before acquiring REMEMBERED_SUBREDDITS. Decay cleanup deliberately uses
	// the reverse data-flow (remembered eligibility before cache deletion), so
	// nesting these mutexes would create a deadlock cycle.
	let resident_cache = {
		let cache = SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
		visible_sources
			.iter()
			.filter_map(|source| {
				let cache_key = source.cache_key();
				cache.get(&cache_key).map(|entry| {
					(
						cache_key,
						ResidentCacheStatusSnapshot {
							cached_posts: entry.posts.len(),
							cached_age_secs: entry.fetched_at.elapsed().as_secs(),
							head_post_created_ts: freshest_cached_post_activity_ts(entry),
							refresh_origin: entry.refresh_origin,
						},
					)
				})
			})
			.collect::<HashMap<_, _>>()
	};
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());

	let mut sources = Vec::with_capacity(visible_sources.len());
	for source in visible_sources {
		let cache_key = source.cache_key();
		let cache_entry = resident_cache.get(&cache_key);
		let remembered_entry = remembered_feed_key_for_source(&source).and_then(|key| remembered.get(&key));
		let daemon_managed = remembered_feed_key_for_source(&source).is_some();
		let disk_entry = cache_entry
			.is_none()
			.then(|| read_persisted_source_entry_for_status(&source))
			.flatten()
			.filter(|entry| persisted_source_entry_is_servable(entry, remembered_entry, generated_at_unix));
		let cached = cache_entry.is_some() || disk_entry.is_some();
		let cached_posts = cache_entry
			.map(|entry| entry.cached_posts)
			.or_else(|| disk_entry.as_ref().map(|entry| entry.posts.len()))
			.unwrap_or_default();
		let cached_age_secs = cache_entry
			.map(|entry| entry.cached_age_secs)
			.or_else(|| disk_entry.as_ref().map(|entry| generated_at_unix.saturating_sub(entry.fetched_at_unix)));
		let head_post_created_ts = cache_entry
			.and_then(|entry| entry.head_post_created_ts)
			.or_else(|| disk_entry.as_ref().and_then(freshest_persisted_cached_post_activity_ts));
		let head_post_age_secs = head_post_created_ts.map(|created_ts| generated_at_unix.saturating_sub(created_ts));
		let cache_expires_in_secs = if let Some(entry) = remembered_entry.filter(|entry| remembered_entry_is_active(entry)) {
			Some(REMEMBERED_SUBREDDIT_TTL_SECS.saturating_sub(entry.last_requested_at.elapsed().as_secs()))
		} else {
			cached_age_secs.map(|age_secs| CACHE_TTL_SECS.saturating_sub(age_secs))
		};
		let blocked_until_secs = remembered_entry
			.and_then(|entry| entry.banned_until)
			.map(|until| until.saturating_duration_since(now).as_secs())
			.filter(|seconds| *seconds > 0);
		let retry_at_unix = blocked_until_secs.map(|seconds| generated_at_unix.saturating_add(seconds));
		let queue_position = remembered_entry.and_then(|entry| queue_positions.get(&entry.name).copied());
		let predicted_next_fetch_in_secs = if let Some(entry) = remembered_entry {
			if entry.refresh_in_flight {
				Some(0)
			} else if let Some(blocked) = blocked_until_secs {
				Some(blocked)
			} else {
				queue_position.map(|position| predicted_next_fetch_secs_for_position(seconds_until_tick, position))
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
		let fairness_slot = remembered_entry.is_some_and(|entry| fairness_sources.contains(&entry.name));
		let scheduling_reason = if let (Some(reason), Some(_)) = (remembered_entry.and_then(|entry| entry.unavailable_reason.as_deref()), blocked_until_secs) {
			format!("30-day {reason} suspension")
		} else if let Some(reason) = remembered_entry.and_then(|entry| entry.unavailable_reason.as_deref()) {
			format!("previously {reason}; retry is eligible")
		} else if fairness_slot {
			"fairness slot (least recently served)".to_string()
		} else if remembered_entry.is_some_and(|entry| recent_request_priority_at(entry, now)) {
			"recent visit boost; decays over time".to_string()
		} else if remembered_entry.is_some_and(|entry| entry.last_refresh_succeeded_at.is_none()) {
			"not refreshed successfully yet".to_string()
		} else {
			"posting cadence and overdue age".to_string()
		};
		let lifecycle_state = match remembered_entry {
			Some(entry) if entry.unfollowed_decay => "unfollowed; cache interest is decaying".to_string(),
			Some(entry) if entry.profile_followers > 1 => format!("followed by {} profiles", entry.profile_followers),
			Some(entry) if entry.profile_followers == 1 => "followed by one profile".to_string(),
			Some(_) => "remembered from recent browsing".to_string(),
			None => "not remembered".to_string(),
		};

		sources.push(CacheStatusSource {
			source_key: cache_key,
			display_name: source.display_name(),
			source_type: match &source {
				FeedSource::Subreddit { name, .. } if name.to_lowercase().starts_with("u_") => "user".to_string(),
				FeedSource::UserComments { .. } => "user_comments".to_string(),
				FeedSource::Subreddit { .. } => "subreddit".to_string(),
				FeedSource::Flair { .. } => "flair".to_string(),
			},
			feed_url: source.public_path(),
			daemon_managed,
			cached,
			cached_posts,
			cached_age_secs,
			head_post_created_ts,
			head_post_age_secs,
			cache_expires_in_secs,
			last_refresh_origin: cache_entry
				.and_then(|entry| entry.refresh_origin)
				.map(|origin| format!("{origin:?}").to_lowercase())
				.or_else(|| {
					disk_entry
						.as_ref()
						.and_then(|entry| entry.refresh_origin)
						.map(|origin| format!("{origin:?}").to_lowercase())
				}),
			last_refresh_succeeded_secs_ago: remembered_entry
				.and_then(|entry| entry.last_refresh_succeeded_at)
				.map(|instant| instant.elapsed().as_secs()),
			last_refresh_failed_secs_ago: remembered_entry.and_then(|entry| entry.last_refresh_failed_at).map(|instant| instant.elapsed().as_secs()),
			refresh_frequency_secs: remembered_entry.map(effective_freshness_target_secs),
			schedule_state: schedule_state.to_string(),
			queue_position,
			predicted_next_fetch_in_secs,
			blocked_until_secs,
			retry_at_unix,
			unavailable_reason: remembered_entry.and_then(|entry| entry.unavailable_reason.clone()),
			scheduling_reason,
			fairness_slot,
			lifecycle_state,
			profile_followers: remembered_entry.map(|entry| entry.profile_followers).unwrap_or_default(),
			consecutive_failures: remembered_entry.map(|entry| entry.consecutive_failures).unwrap_or_default(),
		});
	}

	let cached_posts_total = sources.iter().map(|source| source.cached_posts).sum::<usize>();
	let summary = CacheStatusSummary {
		sources_total: sources.len(),
		cached_sources: sources.iter().filter(|source| source.cached).count(),
		cached_posts: cached_posts_total,
		estimated_cached_post_pages: cached_posts_total.saturating_add(POSTS_PER_PAGE - 1) / POSTS_PER_PAGE,
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
		active_browser_session_paths, age_source_cache_for_test, attempted_source_count, background_target_reached, background_target_sources, begin_remembered_source_refresh,
		begin_source_refresh, build_session_id, cache_status_for_preferences, catalog_source_candidates, cleanup_decayed_remembered_subreddits, continue_feed_session_with_budget,
		count_sources_in_state, daemon_should_continue_after_refresh, effective_freshness_target_secs, enforce_source_cache_limits_locked, ensure_feed_session_with_budget,
		estimate_post_interval_secs, feed_page, feed_status, finalize_session_posts, followup_source_budget, freeze_feed_page, get_cached_source_page,
		has_active_browser_sessions, initial_source_budget_target, mark_deleted_reddit_media_url, mark_fetching_sources_failed, max_sync_source_budget, merged_posts_for_session,
		merged_posts_for_session_live, ordered_refresh_entries_with_fairness, pick_next_remembered_feed_to_refresh, pick_next_remembered_feed_to_refresh_with_mode,
		pick_next_remembered_subreddit_to_refresh, plan_session_warm_mode, predicted_next_fetch_secs_for_position, prune_feed_sessions_locked, prune_source_cache,
		publish_plain_subreddit_refresh, publish_remembered_source_refresh_with_token, random_rotation_seed_for_time, reconcile_profile_memberships,
		reconcile_session_with_source_cache, record_plain_subreddit_refresh_failure, record_remembered_feed_feedback_signal_delta, record_remembered_feed_visit_signal,
		record_remembered_source_refresh_failure_with_token, remember_flair_request, remember_frontpage_subreddits, remember_subreddit_request, remembered_refresh_priority_at,
		resolve_source_refresh_result, restore_persisted_state, retain_following_membership, seed_remembered_feeds_from_source_cache_entries,
		seed_remembered_feeds_from_source_cache_keys, servable_cached_source_posts, session_status_from_posts, snapshot_persisted_remembered_subreddits,
		snapshot_persisted_source_entries, snapshot_persisted_source_unavailability, sort_posts_by_deterministic_random, sort_posts_by_deterministic_random_with_seed,
		sort_sources, source_budget_target, source_cache_is_servable, source_cache_revision, sqlite_active_profile_id, sqlite_timeline_page_for_session_with_query,
		sqlite_timeline_selection, store_source_posts, store_source_posts_with_origin, store_source_posts_with_token, store_source_posts_with_token_and_effects,
		sync_sources_to_request, warm_targets, ActiveBrowserSession, CachedSourcePosts, FeedSession, FeedSessionRequest, FeedSort, FeedSource, RefreshOrigin,
		RememberedFailureDisposition, RememberedFeedPickMode, RememberedRefreshOutcome, RememberedSubredditEntry, SessionSourceEntry, SessionSourceState, SessionWarmMode,
		SourceRefreshOutcome, SqliteSessionCursorState, ACTIVE_BROWSER_SESSIONS, ACTIVE_BROWSER_SESSION_TTL_SECS, BACKGROUND_LOOKAHEAD_PAGES, CACHE_TTL_SECS,
		DELETED_MEDIA_SAFETY_HEALTHY, DELETED_MEDIA_TOMBSTONES, DURABLE_UNAVAILABLE_BACKOFF_SECS, FEED_SESSIONS, MAX_FEED_SESSIONS, MAX_FROZEN_FEED_PAGES,
		MIN_ESTIMATED_POST_INTERVAL_SECS, MIN_FRESHNESS_TARGET_SECS, POSTS_PER_PAGE, PROFILE_SOURCE_MEMBERSHIPS, RECENT_REQUEST_PRIORITY_SECS,
		REMEMBERED_DAEMON_SELECTION_SEQUENCE, REMEMBERED_SUBREDDITS, REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS, REMEMBERED_SUBREDDIT_DAEMON_MAX_ATTEMPTS_PER_TICK,
		REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK, REMEMBERED_SUBREDDIT_TTL_SECS, SOURCE_CACHE, SOURCE_CACHE_REVISION, SOURCE_REFRESH_STATES, SQLITE_SESSION_CURSORS,
		TEST_DURABLE_WRITE_ACTIVE, TEST_DURABLE_WRITE_COUNT, TEST_DURABLE_WRITE_DELAY_MS, TEST_DURABLE_WRITE_FAIL, TEST_PRUNE_REMOVE_ACTIVE, TEST_PRUNE_REMOVE_DELAY_MS,
		TEST_TOMBSTONE_MARKER_WRITE_FAIL, TEST_TOMBSTONE_WRITE_FAIL,
	};
	use crate::utils::{Author, Awards, Flags, Flair, FlairSubscription, Media, Post, Preferences};
	use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
	use std::cmp::Ordering;
	use std::collections::{HashMap, HashSet};
	use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
	use std::sync::{Arc, Barrier, MutexGuard};
	use std::time::{Duration, Instant};

	fn test_lock() -> MutexGuard<'static, ()> {
		super::test_state_lock()
	}

	fn reset_test_caches() {
		SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
		SOURCE_REFRESH_STATES.lock().unwrap_or_else(|e| e.into_inner()).clear();
		FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).clear();
		SQLITE_SESSION_CURSORS.lock().unwrap_or_else(|e| e.into_inner()).clear();
		REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner()).clear();
		PROFILE_SOURCE_MEMBERSHIPS.lock().unwrap_or_else(|e| e.into_inner()).clear();
		REMEMBERED_DAEMON_SELECTION_SEQUENCE.store(0, AtomicOrdering::Relaxed);
		ACTIVE_BROWSER_SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).clear();
		DELETED_MEDIA_TOMBSTONES.lock().unwrap_or_else(|e| e.into_inner()).clear();
		DELETED_MEDIA_SAFETY_HEALTHY.store(true, AtomicOrdering::Release);
		SOURCE_CACHE_REVISION.store(0, AtomicOrdering::Relaxed);
		TEST_DURABLE_WRITE_DELAY_MS.store(0, AtomicOrdering::SeqCst);
		TEST_DURABLE_WRITE_ACTIVE.store(false, AtomicOrdering::SeqCst);
		TEST_DURABLE_WRITE_COUNT.store(0, AtomicOrdering::SeqCst);
		TEST_DURABLE_WRITE_FAIL.store(false, AtomicOrdering::SeqCst);
		TEST_TOMBSTONE_MARKER_WRITE_FAIL.store(false, AtomicOrdering::SeqCst);
		TEST_TOMBSTONE_WRITE_FAIL.store(false, AtomicOrdering::SeqCst);
		TEST_PRUNE_REMOVE_DELAY_MS.store(0, AtomicOrdering::SeqCst);
		TEST_PRUNE_REMOVE_ACTIVE.store(false, AtomicOrdering::SeqCst);
		crate::materialized_catalog::reset_catalog();
		crate::utils::clear_test_caches();
		crate::user::clear_test_caches();
	}

	#[test]
	fn remembered_feed_daemon_enabled_defaults_on() {
		assert!(super::remembered_feed_daemon_enabled_from(None));
		assert!(super::remembered_feed_daemon_enabled_from(Some("on")));
		assert!(super::remembered_feed_daemon_enabled_from(Some("TRUE")));
		assert!(super::remembered_feed_daemon_enabled_from(Some("1")));
	}

	#[test]
	fn remembered_feed_daemon_enabled_rejects_off_values() {
		assert!(!super::remembered_feed_daemon_enabled_from(Some("off")));
		assert!(!super::remembered_feed_daemon_enabled_from(Some("false")));
		assert!(!super::remembered_feed_daemon_enabled_from(Some("0")));
	}

	#[test]
	fn daemon_embeddings_are_background_while_visible_sessions_stay_priority() {
		use crate::observed_semantic_backfill::ObservedBackfillPriority;

		assert_eq!(super::semantic_backfill_priority(Some(RefreshOrigin::Daemon)), ObservedBackfillPriority::Background);
		assert_eq!(super::semantic_backfill_priority(Some(RefreshOrigin::FrontPageSession)), ObservedBackfillPriority::Priority);
		assert_eq!(
			super::semantic_backfill_priority(Some(RefreshOrigin::DirectLiveFetch)),
			ObservedBackfillPriority::Opportunistic
		);
	}

	fn session_entry(source: FeedSource, state: SessionSourceState) -> SessionSourceEntry {
		SessionSourceEntry { source, state, last_error: None }
	}

	fn seed_source(source: &FeedSource, posts: Vec<Post>) {
		store_source_posts(source, posts, String::new());
	}

	fn test_comment_post(id: &str, username: &str, community: &str, link_post_id: &str, created_ts: u64) -> Post {
		let mut post = test_post(id, community, created_ts);
		post.kind = "t1".to_string();
		post.post_type = "comment".to_string();
		post.author.name = username.to_string();
		post.link_id = format!("t3_{link_post_id}");
		post.link_title = format!("link-{link_post_id}");
		post.link_permalink = format!("/r/{community}/comments/{link_post_id}/");
		post.permalink = format!("/r/{community}/comments/{link_post_id}/comment/{id}/");
		post
	}

	fn remembered_entry(name: &str) -> RememberedSubredditEntry {
		RememberedSubredditEntry {
			name: name.to_string(),
			last_requested_at: Instant::now(),
			last_refresh_started_at: None,
			last_refresh_succeeded_at: None,
			last_refresh_failed_at: None,
			banned_until: None,
			unavailable_reason: None,
			consecutive_failures: 0,
			estimated_post_interval_secs: 21_600,
			refresh_in_flight: false,
			visit_priority_score: 0.0,
			visit_priority_last_update_at: None,
			visit_signal_last_applied_at: None,
			feedback_priority_score: 0.0,
			feedback_priority_last_update_at: None,
			profile_followers: 0,
			unfollowed_decay: false,
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
			parent_id: String::new(),
			parent_kind: String::new(),
			parent_comment: crate::utils::ParentComment::default(),
			link_id: String::new(),
			link_title: String::new(),
			link_permalink: String::new(),
			link_author: String::new(),
			link_created_ts: 0,
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
			following_comments: Vec::new(),
			bookmarked: false,
			bookmark_new_comments: None,
		}
	}

	fn test_video_post(id: &str, community: &str, author: &str, created_ts: u64, media_url: &str, duration_seconds: i64) -> Post {
		let mut post = test_post(id, community, created_ts);
		post.author.name = author.to_string();
		post.post_type = "video".to_string();
		post.media.url = media_url.to_string();
		post.media.duration_seconds = duration_seconds;
		post.media.duration = crate::utils::format_video_duration(duration_seconds);
		post
	}

	fn new_source(name: &str) -> FeedSource {
		FeedSource::subreddit(name.to_string(), FeedSort::New)
	}

	fn hot_source(name: &str) -> FeedSource {
		FeedSource::subreddit(name.to_string(), FeedSort::Hot)
	}

	fn new_flair_source(subreddit: &str, flair: &str) -> FeedSource {
		FeedSource::flair(subreddit.to_string(), flair.to_string(), FeedSort::New)
	}

	#[test]
	fn deleted_media_invalidation_removes_stale_image_posts_from_cache_and_sessions() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		let mut deleted = test_post("deleted", "rust", 200);
		deleted.post_type = "image".to_string();
		deleted.media.url = "/img/deleted.jpg".to_string();
		deleted.media.preview_url = "/preview/pre/deleted.jpg?width=640&format=pjpg".to_string();
		deleted.media.preview_srcset = "/preview/pre/deleted.jpg?width=320&format=pjpg 320w, /preview/pre/deleted.jpg?width=960&format=pjpg 960w".to_string();
		let mut kept = test_post("kept", "rust", 100);
		kept.post_type = "image".to_string();
		kept.media.url = "/img/kept.jpg".to_string();

		seed_source(&source, vec![deleted.clone(), kept.clone()]);
		FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).insert(
			"session".to_string(),
			FeedSession {
				id: "session".to_string(),
				request: FeedSessionRequest {
					sources: vec![source.clone()],
					..FeedSessionRequest::default()
				},
				sources: vec![session_entry(source.clone(), SessionSourceState::Ready)],
				frozen_pages: HashMap::from([(0, vec![deleted, kept])]),
				background_active: false,
				background_complete: true,
				requested_page: 0,
				last_updated: Instant::now(),
				revision: 0,
			},
		);

		assert!(mark_deleted_reddit_media_url("https://preview.redd.it/deleted.jpg?width=640&format=pjpg") > 0);

		let cached = servable_cached_source_posts(&source).expect("source cache should remain servable");
		assert_eq!(cached.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["kept"]);
		let frozen = feed_page("session", 0, POSTS_PER_PAGE).expect("session page should remain servable");
		assert_eq!(frozen.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["kept"]);
		let catalog_posts = crate::materialized_catalog::query_source_candidates(&[source], &HashSet::new());
		assert_eq!(catalog_posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["kept"]);
	}

	#[test]
	fn deleted_media_invalidation_prunes_gallery_until_empty() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		let mut gallery = test_post("gallery", "rust", 200);
		gallery.post_type = "gallery".to_string();
		gallery.gallery = vec![
			crate::utils::GalleryMedia {
				url: "/preview/pre/deleted.jpg?width=640&format=pjpg".to_string(),
				preview_url: String::new(),
				preview_srcset: String::new(),
				width: 640,
				height: 800,
				caption: String::new(),
				outbound_url: String::new(),
			},
			crate::utils::GalleryMedia {
				url: "/preview/pre/kept.jpg?width=640&format=pjpg".to_string(),
				preview_url: String::new(),
				preview_srcset: String::new(),
				width: 640,
				height: 800,
				caption: "kept".to_string(),
				outbound_url: String::new(),
			},
		];

		seed_source(&source, vec![gallery]);
		assert_eq!(mark_deleted_reddit_media_url("https://preview.redd.it/deleted.jpg?width=960&format=pjpg"), 1);
		let cached = servable_cached_source_posts(&source).expect("source cache should remain servable");
		assert_eq!(cached.len(), 1);
		assert_eq!(cached[0].post_type, "gallery");
		assert_eq!(cached[0].gallery.len(), 1);
		assert_eq!(cached[0].gallery[0].caption, "kept");

		assert_eq!(mark_deleted_reddit_media_url("https://preview.redd.it/kept.jpg?width=960&format=pjpg"), 1);
		let cached = servable_cached_source_posts(&source).expect("source cache should remain servable");
		assert!(cached.is_empty());
	}

	#[test]
	fn sorts_subreddit_sources_together_before_flairs() {
		let _guard = test_lock();
		reset_test_caches();
		let mut sources = vec![new_flair_source("zeta", "Tag"), new_source("rust"), new_source("u_demo")];

		sort_sources(&mut sources);

		assert!(matches!(&sources[0], FeedSource::Subreddit { name, .. } if name == "rust"));
		assert!(matches!(&sources[1], FeedSource::Subreddit { name, .. } if name == "u_demo"));
		assert!(matches!(&sources[2], FeedSource::Flair { subreddit, .. } if subreddit == "zeta"));
	}

	#[test]
	fn catalog_backed_home_new_matches_live_merge() {
		let _guard = test_lock();
		reset_test_caches();

		let rust = new_source("rust");
		let news = new_flair_source("programming", "News");
		let mut flair_post = test_post("p2", "programming", 180);
		flair_post.flair.text = "News".to_string();
		seed_source(&rust, vec![test_post("p1", "rust", 200), test_post("shared", "rust", 150)]);
		seed_source(&news, vec![flair_post, test_post("shared", "rust", 150)]);

		let session = FeedSession {
			id: "catalog-home".to_string(),
			request: FeedSessionRequest {
				sources: vec![rust.clone(), news.clone()],
				..Default::default()
			},
			sources: vec![session_entry(rust, SessionSourceState::Ready), session_entry(news, SessionSourceState::Ready)],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let live_ids = merged_posts_for_session_live(&session).into_iter().map(|post| post.id).collect::<Vec<_>>();
		let catalog_ids = merged_posts_for_session(&session).into_iter().map(|post| post.id).collect::<Vec<_>>();
		assert_eq!(catalog_ids, live_ids);
	}

	#[test]
	fn catalog_backed_following_new_matches_live_merge() {
		let _guard = test_lock();
		reset_test_caches();

		let submitted_source = FeedSource::subreddit("u_demo".to_string(), FeedSort::New);
		let comments_source = FeedSource::user_comments("u_demo".to_string(), FeedSort::New);
		let submitted = test_post("link-1", "rust", 100);
		let comment = test_comment_post("comment-1", "demo", "rust", "link-1", 140);

		seed_source(&submitted_source, vec![submitted]);
		seed_source(&comments_source, vec![comment]);

		let session = FeedSession {
			id: "catalog-following".to_string(),
			request: FeedSessionRequest {
				sources: vec![submitted_source.clone(), comments_source.clone()],
				bundle_followed_user_comments: true,
				..Default::default()
			},
			sources: vec![
				session_entry(submitted_source, SessionSourceState::Ready),
				session_entry(comments_source, SessionSourceState::Ready),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let live = merged_posts_for_session_live(&session);
		let catalog = merged_posts_for_session(&session);
		assert_eq!(
			catalog.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(),
			live.iter().map(|post| post.id.as_str()).collect::<Vec<_>>()
		);
		assert_eq!(catalog.first().map(|post| post.following_comments.len()), Some(1));
	}

	#[test]
	fn following_feed_rejects_plain_posts_from_non_followed_authors() {
		let submitted_source = FeedSource::subreddit("u_Alice".to_string(), FeedSort::New);
		let comments_source = FeedSource::user_comments("u_Alice".to_string(), FeedSort::New);
		let session = FeedSession {
			id: "following-author-invariant".to_string(),
			request: FeedSessionRequest {
				sources: vec![submitted_source.clone(), comments_source.clone()],
				bundle_followed_user_comments: true,
				..Default::default()
			},
			sources: vec![
				session_entry(submitted_source, SessionSourceState::Ready),
				session_entry(comments_source, SessionSourceState::Ready),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let mut followed_submission = test_post("followed-submission", "community", 300);
		followed_submission.author.name = "alice".to_string();
		let mut stray_submission = test_post("stray-submission", "community", 250);
		stray_submission.author.name = "not-followed".to_string();
		let mut commented_thread = test_post("commented-thread", "community", 200);
		commented_thread.author.name = "thread-author".to_string();
		let followed_comment = test_comment_post("followed-comment", "Alice", "community", "commented-thread", 350);

		let merged = finalize_session_posts(&session, vec![followed_submission, stray_submission, commented_thread, followed_comment]);
		assert_eq!(
			merged.iter().map(|post| post.id.as_str()).collect::<HashSet<_>>(),
			HashSet::from(["followed-submission", "commented-thread"])
		);
		let bundle = merged.iter().find(|post| post.id == "commented-thread").expect("followed comment bundle");
		assert_eq!(bundle.following_comments.len(), 1);
	}

	#[test]
	fn following_membership_filter_rejects_plain_sqlite_rows() {
		let session = FeedSession {
			id: "following-sqlite-author-invariant".to_string(),
			request: FeedSessionRequest {
				sources: vec![
					FeedSource::subreddit("u_Alice".to_string(), FeedSort::New),
					FeedSource::user_comments("u_Alice".to_string(), FeedSort::New),
				],
				profile_id: "sqlite-profile".to_string(),
				bundle_followed_user_comments: true,
				..Default::default()
			},
			sources: Vec::new(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};
		let mut followed = test_post("followed-sqlite", "community", 300);
		followed.author.name = "ALICE".to_string();
		let mut stray = test_post("stray-sqlite", "community", 200);
		stray.author.name = "not-followed".to_string();
		let mut bundle = test_post("bundle-sqlite", "community", 100);
		bundle.author.name = "thread-author".to_string();
		bundle.following_comments.push(crate::user::following_comment_from_listing_post(test_comment_post(
			"comment-sqlite",
			"Alice",
			"community",
			"bundle-sqlite",
			350,
		)));
		let mut posts = vec![followed, stray, bundle];

		retain_following_membership(&session, &mut posts);

		assert_eq!(
			posts.iter().map(|post| post.id.as_str()).collect::<HashSet<_>>(),
			HashSet::from(["followed-sqlite", "bundle-sqlite"])
		);
	}

	#[test]
	fn following_new_is_strictly_chronological_even_for_prolific_accounts() {
		let _guard = test_lock();
		reset_test_caches();

		let mut sources = Vec::new();
		let now = 10_000;
		for user_index in 0..10 {
			let username = format!("followed_{user_index}");
			let source = FeedSource::subreddit(format!("u_{username}"), FeedSort::New);
			let post_count = if user_index < 2 { 30 } else { 1 };
			let mut posts = Vec::new();
			for post_index in 0..post_count {
				let mut post = test_post(
					&format!("{username}-{post_index}"),
					&format!("community_{user_index}"),
					now - ((user_index as u64) * 10) - post_index as u64,
				);
				post.author.name = username.clone();
				posts.push(post);
			}
			seed_source(&source, posts);
			sources.push(source);
		}

		let session = FeedSession {
			id: "following-interleave".to_string(),
			request: FeedSessionRequest {
				sources: sources.clone(),
				bundle_followed_user_comments: true,
				..Default::default()
			},
			sources: sources.into_iter().map(|source| session_entry(source, SessionSourceState::Ready)).collect(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let merged = merged_posts_for_session(&session);
		assert_eq!(merged.len(), 68, "the full followed-profile cache should remain reachable");
		assert!(merged
			.windows(2)
			.all(|pair| crate::user::post_feed_activity_ts(&pair[0]) >= crate::user::post_feed_activity_ts(&pair[1])));
		assert_eq!(merged.first().map(|post| post.author.name.as_str()), Some("followed_0"));
		assert_eq!(merged.get(1).map(|post| post.author.name.as_str()), Some("followed_0"));
	}

	#[test]
	fn following_new_uses_cached_posts_from_all_requested_profiles_before_warmup() {
		let _guard = test_lock();
		reset_test_caches();

		let mut sources = Vec::new();
		for index in 0..40 {
			let username = format!("profile_{index:02}");
			let source = FeedSource::subreddit(format!("u_{username}"), FeedSort::New);
			let mut post = test_post(&format!("post-{index:02}"), "community", 10_000 - index);
			post.author.name = username;
			seed_source(&source, vec![post]);
			sources.push(source);
		}

		let session = FeedSession {
			id: "following-all-profile-catalog".to_string(),
			request: FeedSessionRequest {
				sources: sources.clone(),
				bundle_followed_user_comments: true,
				..Default::default()
			},
			sources: sources.into_iter().map(|source| session_entry(source, SessionSourceState::Unstarted)).collect(),
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let authors = merged_posts_for_session(&session)
			.into_iter()
			.take(POSTS_PER_PAGE)
			.map(|post| post.author.name)
			.collect::<HashSet<_>>();
		assert_eq!(authors.len(), POSTS_PER_PAGE);
	}

	#[test]
	fn daemon_discovery_order_only_promotes_newly_seen_posts() {
		let _guard = test_lock();
		reset_test_caches();

		let source = FeedSource::subreddit("u_demo", FeedSort::New);
		let old = test_post("old", "community", 200);
		let newly_found = test_post("newly-found", "community", 100);
		crate::materialized_catalog::mark_posts_seen_before_daemon(&source, std::slice::from_ref(&old));
		crate::materialized_catalog::record_daemon_discoveries(&source, &[old.clone(), newly_found.clone()]);

		let mut posts = vec![old.clone(), newly_found.clone()];
		crate::materialized_catalog::sort_posts_by_daemon_discovery(&mut posts);
		assert_eq!(posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["newly-found", "old"]);

		crate::materialized_catalog::record_daemon_discoveries(&source, &[old, newly_found]);
		crate::materialized_catalog::sort_posts_by_daemon_discovery(&mut posts);
		assert_eq!(posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["newly-found", "old"]);
	}

	#[test]
	fn daemon_discovery_order_tracks_regular_home_sources() {
		let _guard = test_lock();
		reset_test_caches();
		let source = FeedSource::subreddit("rust", FeedSort::New);
		let old = test_post("old", "rust", 300);
		let newly_found = test_post("newly-found", "rust", 100);

		crate::materialized_catalog::mark_posts_seen_before_daemon(&source, std::slice::from_ref(&old));
		crate::materialized_catalog::record_daemon_discoveries(&source, &[old.clone(), newly_found.clone()]);

		let mut posts = vec![old, newly_found];
		crate::materialized_catalog::sort_posts_by_daemon_discovery(&mut posts);
		assert_eq!(posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["newly-found", "old"]);
	}

	#[test]
	fn daemon_discovery_migration_uses_one_deployment_baseline_without_dropping_posts() {
		let _guard = test_lock();
		reset_test_caches();

		let first_source = FeedSource::subreddit("rust", FeedSort::New);
		let second_source = FeedSource::subreddit("u_second", FeedSort::New);
		let first = crate::cache_persistence::PersistedPost::from_runtime(&test_post("first", "community", 100));
		let second = crate::cache_persistence::PersistedPost::from_runtime(&test_post("second", "community", 200));
		crate::materialized_catalog::seed_following_discovery_baseline(&first_source, &[first], 111);
		crate::materialized_catalog::seed_following_discovery_baseline(&second_source, &[second], 222);

		let entries = crate::materialized_catalog::snapshot_following_discoveries();
		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].discovered_at_unix, entries[1].discovered_at_unix);
		assert_ne!(entries[0].discovered_at_unix, 111);
		assert_ne!(entries[0].discovered_at_unix, 222);
	}

	#[test]
	fn catalog_video_candidates_match_listing_output_from_cached_union() {
		let _guard = test_lock();
		reset_test_caches();

		let rust = new_source("rust");
		let programming = new_source("programming");
		seed_source(
			&rust,
			vec![
				test_video_post("v1", "rust", "alice", 200, "https://v.redd.it/one/DASH_720.mp4", 40),
				test_video_post("shared-video", "rust", "alice", 180, "https://v.redd.it/shared/DASH_720.mp4", 15),
			],
		);
		seed_source(
			&programming,
			vec![
				test_video_post("v2", "programming", "bob", 220, "https://v.redd.it/two/DASH_720.mp4", 50),
				test_video_post("shared-video", "rust", "alice", 180, "https://v.redd.it/shared/DASH_720.mp4", 15),
			],
		);

		let filters = HashSet::new();
		let query = crate::video_mode::VideoModeQuery::default();
		let listing_from_cache = crate::video_mode::build_listing(
			&[servable_cached_source_posts(&rust).unwrap(), servable_cached_source_posts(&programming).unwrap()].concat(),
			&filters,
			&query,
			1_000,
		);
		let listing_from_catalog = crate::video_mode::build_listing(
			&catalog_source_candidates(&[rust, programming], &filters).expect("catalog candidates"),
			&filters,
			&query,
			1_000,
		);

		assert_eq!(
			listing_from_catalog.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(),
			listing_from_cache.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>()
		);
	}

	#[test]
	fn restore_persisted_state_keeps_source_payloads_out_of_resident_cache() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		remember_subreddit_request("rust");
		store_source_posts_with_origin(&source, vec![test_post("persisted", "rust", 100)], String::new(), Some(RefreshOrigin::Daemon));
		let source_entries = snapshot_persisted_source_entries(1_000_100);
		let remembered_entries = snapshot_persisted_remembered_subreddits(1_000_100);

		reset_test_caches();
		restore_persisted_state(source_entries, remembered_entries, Vec::new(), 1_000_100, 1_000_100);

		assert!(SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
		assert!(catalog_source_candidates(&[source], &HashSet::new()).unwrap().is_empty());
		assert!(REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner()).contains_key("rust"));
	}

	#[test]
	fn source_cache_keys_and_paths_are_sort_aware() {
		let source_new = new_source("rust");
		let source_hot = hot_source("rust");
		let user_new = new_source("u_someone");
		let user_mixed = new_source("u_SomeOne");

		assert_eq!(source_new.cache_key(), "subreddit:rust:new");
		assert_eq!(source_hot.cache_key(), "subreddit:rust:hot");
		assert_eq!(user_new.cache_key(), "subreddit:u_someone:new");
		assert_eq!(source_new.build_path(), "/r/rust/new.json?limit=40&raw_json=1");
		assert_eq!(source_hot.build_path(), "/r/rust/hot.json?limit=40&raw_json=1");
		assert_eq!(user_new.build_path(), "/user/someone/submitted.json?sort=new&limit=40&raw_json=1");
		assert_eq!(user_mixed.build_path(), "/user/someone/submitted.json?sort=new&limit=40&raw_json=1");
		let uc = FeedSource::user_comments("u_someone", FeedSort::New);
		assert_eq!(uc.cache_key(), "user_comments:u_someone:new");
		assert_eq!(uc.build_path(), "/user/someone/comments.json?sort=new&limit=40&raw_json=1");
	}

	#[test]
	fn session_identity_differs_across_listing_sorts() {
		let request_new = FeedSessionRequest {
			sources: vec![new_source("rust")],
			filters: HashSet::new(),
			quarantine: false,
			query_fingerprint: "sort=new".to_string(),
			..Default::default()
		};
		let request_hot = FeedSessionRequest {
			sources: vec![hot_source("rust")],
			filters: HashSet::new(),
			quarantine: false,
			query_fingerprint: "sort=hot".to_string(),
			..Default::default()
		};

		assert_ne!(build_session_id(&request_new), build_session_id(&request_hot));
	}

	#[test]
	fn session_identity_differs_when_merge_sort_differs_for_same_sources() {
		let sources = vec![
			FeedSource::subreddit("u_demo".to_string(), FeedSort::New),
			FeedSource::user_comments("u_demo".to_string(), FeedSort::New),
		];
		let new_merge = FeedSessionRequest {
			sources: sources.clone(),
			filters: HashSet::new(),
			quarantine: false,
			query_fingerprint: "fp".to_string(),
			bundle_followed_user_comments: true,
			merge_sort: Some(FeedSort::New),
			..Default::default()
		};
		let hot_merge = FeedSessionRequest {
			merge_sort: Some(FeedSort::Hot),
			..new_merge.clone()
		};
		assert_ne!(build_session_id(&new_merge), build_session_id(&hot_merge));
	}

	#[test]
	fn session_identity_differs_when_cache_revision_changes() {
		let base = FeedSessionRequest {
			sources: vec![new_source("rust")],
			filters: HashSet::new(),
			quarantine: false,
			query_fingerprint: "sort=new".to_string(),
			cache_revision: 1,
			..Default::default()
		};
		let changed = FeedSessionRequest {
			cache_revision: 2,
			..base.clone()
		};

		assert_ne!(build_session_id(&base), build_session_id(&changed));
	}

	#[test]
	fn session_identity_differs_when_random_rotation_seed_changes() {
		let base = FeedSessionRequest {
			sources: vec![new_source("rust")],
			filters: HashSet::new(),
			quarantine: false,
			query_fingerprint: "sort=random".to_string(),
			random_rotation_seed: 1,
			merge_sort: Some(FeedSort::Random),
			..Default::default()
		};
		let changed = FeedSessionRequest {
			random_rotation_seed: 2,
			..base.clone()
		};

		assert_ne!(build_session_id(&base), build_session_id(&changed));
	}

	#[test]
	fn source_cache_revision_advances_only_when_source_window_changes() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		let posts = vec![test_post("one", "rust", 100), test_post("two", "rust", 90)];
		assert_eq!(source_cache_revision(), 0);

		seed_source(&source, posts.clone());
		let first_revision = source_cache_revision();
		assert!(first_revision > 0);

		seed_source(&source, posts);
		assert_eq!(source_cache_revision(), first_revision);

		seed_source(&source, vec![test_post("new", "rust", 110), test_post("one", "rust", 100)]);
		assert!(source_cache_revision() > first_revision);
	}

	#[test]
	fn fresh_session_after_cache_revision_change_does_not_reuse_frozen_page() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		let old_posts = (0..POSTS_PER_PAGE)
			.map(|index| test_post(&format!("old-{index}"), "rust", 1_000 - index as u64))
			.collect::<Vec<_>>();
		seed_source(&source, old_posts);

		let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
		let old_session_id = runtime.block_on(ensure_feed_session_with_budget(
			FeedSessionRequest {
				sources: vec![source.clone()],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "front-page".to_string(),
				cache_revision: source_cache_revision(),
				..Default::default()
			},
			0,
			POSTS_PER_PAGE,
			1,
		));
		assert!(
			FEED_SESSIONS
				.lock()
				.unwrap()
				.get(&old_session_id)
				.is_some_and(|session| session.frozen_pages.contains_key(&0)),
			"synchronous fill should freeze the ready first page"
		);
		let old_page = feed_page(&old_session_id, 0, POSTS_PER_PAGE).expect("old page");
		assert_eq!(old_page.posts.first().map(|post| post.id.as_str()), Some("old-0"));

		let fresh_posts = std::iter::once(test_post("fresh-0", "rust", 2_000))
			.chain((0..POSTS_PER_PAGE - 1).map(|index| test_post(&format!("new-{index}"), "rust", 1_900 - index as u64)))
			.collect::<Vec<_>>();
		seed_source(&source, fresh_posts);

		assert!(runtime.block_on(continue_feed_session_with_budget(&old_session_id, 0, POSTS_PER_PAGE, 1)));
		assert_eq!(
			feed_page(&old_session_id, 0, POSTS_PER_PAGE)
				.expect("continued old frozen page")
				.posts
				.first()
				.map(|post| post.id.as_str()),
			Some("old-0")
		);

		let fresh_session_id = runtime.block_on(ensure_feed_session_with_budget(
			FeedSessionRequest {
				sources: vec![source],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "front-page".to_string(),
				cache_revision: source_cache_revision(),
				..Default::default()
			},
			0,
			POSTS_PER_PAGE,
			1,
		));

		assert_ne!(fresh_session_id, old_session_id);
		assert_eq!(
			feed_page(&old_session_id, 0, POSTS_PER_PAGE)
				.expect("old frozen page")
				.posts
				.first()
				.map(|post| post.id.as_str()),
			Some("old-0")
		);
		assert_eq!(
			feed_page(&fresh_session_id, 0, POSTS_PER_PAGE)
				.expect("fresh page")
				.posts
				.first()
				.map(|post| post.id.as_str()),
			Some("fresh-0")
		);
	}

	#[test]
	fn hot_sessions_merge_posts_by_score_desc() {
		let _guard = test_lock();
		reset_test_caches();

		let mut top = test_post("top", "rust", 100);
		top.score = ("5.0k".to_string(), "5000".to_string());
		let mut mid = test_post("mid", "programming", 200);
		mid.score = ("900".to_string(), "900".to_string());
		let mut low = test_post("low", "science", 300);
		low.score = ("75".to_string(), "75".to_string());

		let left = hot_source("rust");
		let right = hot_source("programming");
		let third = hot_source("science");
		seed_source(&left, vec![mid.clone()]);
		seed_source(&right, vec![top.clone()]);
		seed_source(&third, vec![low.clone()]);

		let session = FeedSession {
			id: "hot-demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![left.clone(), right.clone(), third.clone()],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "sort=hot".to_string(),
				..Default::default()
			},
			sources: vec![
				session_entry(left, SessionSourceState::Ready),
				session_entry(right, SessionSourceState::Ready),
				session_entry(third, SessionSourceState::Ready),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let ids = merged_posts_for_session(&session).into_iter().map(|post| post.id).collect::<Vec<_>>();
		assert_eq!(ids, vec!["top", "mid", "low"]);
	}

	#[test]
	fn catalog_backed_random_session_samples_beyond_newest_prefix() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		let posts = (0..300)
			.map(|index| test_post(&format!("catalog-random-{index:03}"), "rust", 10_000 - index as u64))
			.collect::<Vec<_>>();
		let candidate_limit = POSTS_PER_PAGE * (1 + BACKGROUND_LOOKAHEAD_PAGES) * 3;
		let newest_prefix_ids = posts.iter().take(candidate_limit).map(|post| post.id.clone()).collect::<HashSet<_>>();
		let mut expected = posts.clone();
		sort_posts_by_deterministic_random(&mut expected);
		expected.truncate(candidate_limit);
		assert!(expected.iter().any(|post| !newest_prefix_ids.contains(&post.id)));
		let expected_ids = expected.iter().map(|post| post.id.clone()).collect::<Vec<_>>();
		seed_source(&source, posts);

		let session = FeedSession {
			id: "random-catalog".to_string(),
			request: FeedSessionRequest {
				sources: vec![source.clone()],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "sort=random".to_string(),
				merge_sort: Some(FeedSort::Random),
				..Default::default()
			},
			sources: vec![session_entry(source, SessionSourceState::Ready)],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let ids = merged_posts_for_session(&session).into_iter().map(|post| post.id).collect::<Vec<_>>();
		assert_eq!(ids, expected_ids);
	}

	#[test]
	fn compact_mixed_feed_index_admits_all_cached_sources_without_full_payloads() {
		let _guard = test_lock();
		reset_test_caches();

		let sources = (0..30)
			.map(|source_index| {
				let community = format!("mixed-{source_index:02}");
				let source = new_source(&community);
				let posts = (0..12)
					.map(|post_index| test_post(&format!("mixed-{source_index:02}-{post_index:02}"), &community, 10_000 - post_index as u64))
					.collect::<Vec<_>>();
				crate::materialized_catalog::apply_mixed_feed_source_refresh(&source, &posts);
				source
			})
			.collect::<Vec<_>>();

		assert!(SOURCE_CACHE.lock().unwrap_or_else(|err| err.into_inner()).is_empty());
		let mut session = FeedSession {
			id: "compact-mixed-random".to_string(),
			request: FeedSessionRequest {
				sources: sources.clone(),
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "sort=random".to_string(),
				merge_sort: Some(FeedSort::Random),
				..Default::default()
			},
			sources: sources.into_iter().map(|source| session_entry(source, SessionSourceState::Unstarted)).collect(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		reconcile_session_with_source_cache(&mut session);
		assert_eq!(count_sources_in_state(&session, SessionSourceState::Ready), 30);
		let first_page = merged_posts_for_session(&session).into_iter().take(POSTS_PER_PAGE).collect::<Vec<_>>();
		assert_eq!(first_page.len(), POSTS_PER_PAGE);
		assert_eq!(first_page.iter().map(|post| post.community.as_str()).collect::<HashSet<_>>().len(), POSTS_PER_PAGE);
	}

	#[test]
	fn random_rotation_seed_uses_fifteen_minute_buckets() {
		assert_eq!(random_rotation_seed_for_time(0), 0);
		assert_eq!(random_rotation_seed_for_time(899), 0);
		assert_eq!(random_rotation_seed_for_time(900), 1);
		assert_eq!(random_rotation_seed_for_time(1_799), 1);
		assert_eq!(random_rotation_seed_for_time(1_800), 2);
	}

	#[test]
	fn catalog_backed_random_session_rotates_sample_with_seed() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust-rotation");
		let posts = (0..300)
			.map(|index| test_post(&format!("catalog-rotation-{index:03}"), "rust-rotation", 10_000 - index as u64))
			.collect::<Vec<_>>();

		let mut expected = posts.clone();
		sort_posts_by_deterministic_random_with_seed(&mut expected, 42);
		let candidate_limit = POSTS_PER_PAGE * (1 + BACKGROUND_LOOKAHEAD_PAGES) * 3;
		expected.truncate(candidate_limit);

		let mut unrotated = posts.clone();
		sort_posts_by_deterministic_random(&mut unrotated);
		unrotated.truncate(candidate_limit);
		assert_ne!(
			expected.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(),
			unrotated.iter().map(|post| post.id.as_str()).collect::<Vec<_>>()
		);

		let expected_ids = expected.iter().map(|post| post.id.clone()).collect::<Vec<_>>();
		seed_source(&source, posts);

		let session = FeedSession {
			id: "random-catalog-rotated".to_string(),
			request: FeedSessionRequest {
				sources: vec![source.clone()],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "sort=random".to_string(),
				random_rotation_seed: 42,
				merge_sort: Some(FeedSort::Random),
				..Default::default()
			},
			sources: vec![session_entry(source, SessionSourceState::Ready)],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let ids = merged_posts_for_session(&session).into_iter().map(|post| post.id).collect::<Vec<_>>();
		assert_eq!(ids, expected_ids);
	}

	#[test]
	fn merged_following_feed_bundles_t1_under_t3() {
		let _guard = test_lock();
		reset_test_caches();

		let mut submission = test_post("abc", "rust", 200);
		submission.media.url = "https://i.redd.it/x.jpg".to_string();
		submission.post_type = "image".to_string();

		let mut comment = test_post("c1", "rust", 100);
		comment.kind = "t1".to_string();
		comment.post_type = "comment".to_string();
		comment.link_id = "t3_abc".to_string();
		comment.body = "<div class=\"md\"><p>hi</p></div>".to_string();
		comment.id = "c1".to_string();

		let submitted = FeedSource::subreddit("u_followed".to_string(), FeedSort::New);
		let comments = FeedSource::user_comments("u_followed".to_string(), FeedSort::New);
		seed_source(&submitted, vec![submission]);
		seed_source(&comments, vec![comment]);

		let session = FeedSession {
			id: "bundle-merged".to_string(),
			request: FeedSessionRequest {
				sources: vec![submitted.clone(), comments.clone()],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "bundle".to_string(),
				bundle_followed_user_comments: true,
				..Default::default()
			},
			sources: vec![session_entry(submitted, SessionSourceState::Ready), session_entry(comments, SessionSourceState::Ready)],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let merged = merged_posts_for_session(&session);
		assert!(merged.iter().all(|post| post.kind != "t1"));
		let primary = merged.iter().find(|post| post.id == "abc").expect("t3 row");
		assert_eq!(primary.following_comments.len(), 1);
		assert_eq!(primary.following_comments[0].id, "c1");
	}

	#[test]
	fn store_source_posts_updates_author_video_stats_from_feed_posts() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		store_source_posts(
			&source,
			vec![
				test_video_post("one", "rust", "author_alpha", 300, "https://v.redd.it/alpha/DASH_720.mp4", 30),
				test_video_post("two", "rust", "author_alpha", 200, "https://v.redd.it/beta/DASH_720.mp4", 90),
				test_video_post("three", "rust", "author_bravo", 100, "https://v.redd.it/gamma/DASH_720.mp4", 45),
			],
			String::new(),
		);

		assert_eq!(
			crate::user::cached_avg_video_length("author_alpha"),
			Some(("1:00".to_string(), "from 2 seen videos".to_string()))
		);
		assert_eq!(
			crate::user::cached_avg_video_length("author_bravo"),
			Some(("0:45".to_string(), "from 1 seen video".to_string()))
		);
	}

	#[test]
	fn restored_source_cache_defers_author_video_stats_until_source_load() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		store_source_posts_with_origin(
			&source,
			vec![test_video_post("one", "rust", "author_restore", 100, "https://v.redd.it/restore/DASH_720.mp4", 75)],
			"after".to_string(),
			Some(RefreshOrigin::Daemon),
		);

		let now_unix = 1_700_000_000;
		let source_entries = snapshot_persisted_source_entries(now_unix);
		let remembered_entries = snapshot_persisted_remembered_subreddits(now_unix);

		reset_test_caches();
		restore_persisted_state(source_entries, remembered_entries, Vec::new(), now_unix, now_unix);

		assert_eq!(crate::user::cached_avg_video_length("author_restore"), None);
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
		assert_eq!(remembered.len(), 2);
		assert!(remembered.contains_key("rust"));
		assert!(remembered.contains_key("u_demo"));
	}

	#[test]
	fn visit_signal_shortens_effective_freshness_target() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		let before = REMEMBERED_SUBREDDITS.lock().unwrap()["rust"].clone();
		let target_before = effective_freshness_target_secs(&before);
		record_remembered_feed_visit_signal(&new_source("rust"));
		let after = REMEMBERED_SUBREDDITS.lock().unwrap()["rust"].clone();
		let target_after = effective_freshness_target_secs(&after);
		assert!(target_after < target_before, "visit signal should increase refresh priority (lower freshness target secs)");
	}

	#[test]
	fn visit_priority_boost_decays_by_half_after_one_half_life() {
		let now = Instant::now();
		let mut entry = remembered_entry("rust");
		entry.visit_priority_score = 2.0;
		entry.visit_priority_last_update_at = Some(now);

		let decayed = super::visit_priority_score_at(&entry, now + Duration::from_secs(super::VISIT_PRIORITY_HALFLIFE_SECS));

		assert!((decayed - 1.0).abs() < 0.000_001);
	}

	#[test]
	fn average_post_gap_drives_explainable_cadence() {
		let posts = vec![
			test_post("one", "rust", 600),
			test_post("two", "rust", 480),
			test_post("three", "rust", 240),
			test_post("four", "rust", 120),
		];

		assert_eq!(estimate_post_interval_secs(&posts, 999), 160);
	}

	#[test]
	fn fairness_slot_selects_least_recently_served_source() {
		let now = Instant::now();
		let mut popular = remembered_entry("popular");
		popular.last_requested_at = now;
		popular.last_refresh_started_at = Some(now - Duration::from_secs(60));
		popular.visit_priority_score = 10.0;
		popular.visit_priority_last_update_at = Some(now);
		let mut rarely_viewed = remembered_entry("rarely_viewed");
		rarely_viewed.last_requested_at = now - Duration::from_secs(7 * 24 * 60 * 60);
		rarely_viewed.last_refresh_started_at = Some(now - Duration::from_secs(6 * 24 * 60 * 60));

		let ordered = ordered_refresh_entries_with_fairness(vec![popular, rarely_viewed], now, super::FAIRNESS_SELECTION_STRIDE - 1);

		assert_eq!(ordered[0].0.name, "rarely_viewed");
		assert!(ordered[0].1);
	}

	#[test]
	fn bounded_exploration_occurs_every_fourth_selection() {
		let slots = (0..12).filter(|sequence| super::fairness_slot_for_sequence(*sequence)).collect::<Vec<_>>();

		assert_eq!(slots, vec![3, 7, 11]);
	}

	#[test]
	fn visit_signal_cooldown_prevents_immediate_second_bump() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		record_remembered_feed_visit_signal(&new_source("rust"));
		let once = REMEMBERED_SUBREDDITS.lock().unwrap()["rust"].visit_priority_score;
		record_remembered_feed_visit_signal(&new_source("rust"));
		let twice = REMEMBERED_SUBREDDITS.lock().unwrap()["rust"].visit_priority_score;
		assert_eq!(once, twice);
	}

	#[test]
	fn feedback_signal_has_smaller_cache_priority_effect_than_visit_signal() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		remember_subreddit_request("programming");
		let before = REMEMBERED_SUBREDDITS.lock().unwrap()["rust"].clone();
		let target_before = effective_freshness_target_secs(&before);

		record_remembered_feed_feedback_signal_delta(&new_source("rust"), 1);
		record_remembered_feed_visit_signal(&new_source("programming"));

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let target_after_feedback = effective_freshness_target_secs(&remembered["rust"]);
		let target_after_visit = effective_freshness_target_secs(&remembered["programming"]);

		assert!(target_after_feedback < target_before, "positive feedback should mildly increase refresh priority");
		assert!(target_after_feedback > target_after_visit, "feedback priority should be weaker than a direct visit signal");
	}

	#[test]
	fn negative_feedback_does_not_create_remembered_feed() {
		let _guard = test_lock();
		reset_test_caches();

		record_remembered_feed_feedback_signal_delta(&new_source("rust"), -1);

		assert!(REMEMBERED_SUBREDDITS.lock().unwrap().is_empty());
	}

	#[test]
	fn remember_frontpage_subreddits_tracks_subscriptions_and_flair_parents() {
		let _guard = test_lock();
		reset_test_caches();

		remember_frontpage_subreddits(&test_prefs_with_saved_feeds(), false);

		let programming_flair_key = format!("flair:programming:{}", URL_SAFE_NO_PAD.encode("News".as_bytes()));
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		assert!(remembered.contains_key("rust"));
		assert!(remembered.contains_key(&programming_flair_key));
		assert!(remembered.contains_key("u_demo"));
		assert!(!remembered.contains_key("user_comments:u_demo"));
		assert_eq!(remembered.len(), 3);
	}

	#[test]
	fn following_frontpage_tracks_followed_user_comment_sources() {
		let _guard = test_lock();
		reset_test_caches();

		remember_frontpage_subreddits(&test_prefs_with_saved_feeds(), true);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		assert!(remembered.contains_key("user_comments:u_demo"));
	}

	#[test]
	fn remembered_plain_subreddit_cache_remains_servable_beyond_short_ttl() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
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
	fn remembered_flair_cache_remains_servable_beyond_short_ttl() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_flair_source("rust", "News");
		remember_flair_request("rust", "News");
		store_source_posts_with_origin(&source, vec![test_post("one", "rust", 100)], String::new(), Some(RefreshOrigin::Daemon));
		SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let cache = SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
		let entry = cache.get(&source.cache_key()).unwrap();
		assert!(source_cache_is_servable(&source, entry));
		drop(cache);
		assert!(get_cached_source_page(&source, 0, 25).is_some());
	}

	#[test]
	fn non_remembered_plain_subreddit_cache_expires_on_short_ttl() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		seed_source(&source, vec![test_post("one", "rust", 100)]);
		SOURCE_CACHE.lock().unwrap().get_mut(&source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let cache = SOURCE_CACHE.lock().unwrap();
		let entry = cache.get(&source.cache_key()).unwrap();
		assert!(!source_cache_is_servable(&source, entry));
		drop(cache);
		assert!(get_cached_source_page(&source, 0, 25).is_none());
	}

	#[test]
	fn pruning_expired_source_cache_removes_catalog_membership() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		seed_source(&source, vec![test_post("one", "rust", 100)]);
		assert_eq!(catalog_source_candidates(std::slice::from_ref(&source), &HashSet::new()).unwrap().len(), 1);

		age_source_cache_for_test(&source, Duration::from_secs(CACHE_TTL_SECS + 60));
		prune_source_cache();

		assert!(SOURCE_CACHE.lock().unwrap().get(&source.cache_key()).is_none());
		assert!(catalog_source_candidates(&[source], &HashSet::new()).unwrap().is_empty());
	}

	#[test]
	fn pruning_keeps_remembered_source_catalog_membership() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		remember_subreddit_request("rust");
		seed_source(&source, vec![test_post("one", "rust", 100)]);
		age_source_cache_for_test(&source, Duration::from_secs(CACHE_TTL_SECS + 60));
		prune_source_cache();

		assert!(SOURCE_CACHE.lock().unwrap().get(&source.cache_key()).is_some());
		assert_eq!(catalog_source_candidates(&[source], &HashSet::new()).unwrap().len(), 1);
	}

	#[test]
	fn pruning_evicts_idle_remembered_source_from_resident_memory() {
		let _guard = test_lock();
		reset_test_caches();

		let source = new_source("rust");
		remember_subreddit_request("rust");
		store_source_posts_with_origin(&source, vec![test_post("one", "rust", 100)], String::new(), Some(RefreshOrigin::Daemon));
		SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&source.cache_key()).unwrap().last_accessed =
			Instant::now() - Duration::from_secs(super::source_resident_ttl_secs() + 60);
		prune_source_cache();

		assert!(SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&source.cache_key()).is_none());
		assert!(catalog_source_candidates(&[source], &HashSet::new()).unwrap().is_empty());
		assert!(REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner()).contains_key("rust"));
	}

	#[test]
	fn flair_and_user_sources_keep_short_ttl_rules() {
		let _guard = test_lock();
		reset_test_caches();

		let flair_source = new_flair_source("rust", "News");
		let user_source = new_source("u_demo");
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

		assert_eq!(pick_next_remembered_subreddit_to_refresh().map(|s| s.display_name()), Some("alpha".to_string()));

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		remembered.get_mut("alpha").unwrap().last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(300));
		drop(remembered);

		assert_eq!(pick_next_remembered_subreddit_to_refresh().map(|s| s.display_name()), Some("gamma".to_string()));
	}

	#[test]
	fn pick_next_remembered_feed_can_return_flair_subscription() {
		let _guard = test_lock();
		reset_test_caches();

		remember_flair_request("programming", "News");

		assert_eq!(
			pick_next_remembered_feed_to_refresh().map(|source| source.display_name()),
			Some("programming#News".to_string())
		);
	}

	#[test]
	fn pick_next_remembered_feed_skips_in_flight_source() {
		let _guard = test_lock();
		reset_test_caches();

		let mut in_flight = remembered_entry("alpha");
		in_flight.refresh_in_flight = true;
		in_flight.last_requested_at = Instant::now();
		let mut ready = remembered_entry("beta");
		ready.last_requested_at = Instant::now() - Duration::from_secs(60);

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		remembered.insert("alpha".to_string(), in_flight);
		remembered.insert("beta".to_string(), ready);
		drop(remembered);

		assert_eq!(pick_next_remembered_feed_to_refresh().map(|source| source.display_name()), Some("beta".to_string()));
	}

	#[test]
	fn remembered_feed_validation_rejects_malformed_source_names() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("u");
		remember_subreddit_request("ab");
		remember_subreddit_request("rust-dev");
		remember_subreddit_request("rust");
		remember_subreddit_request("u_demo-user");

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		assert!(!remembered.contains_key("u"));
		assert!(!remembered.contains_key("ab"));
		assert!(!remembered.contains_key("rust-dev"));
		assert!(remembered.contains_key("rust"));
		assert!(remembered.contains_key("u_demo-user"));
	}

	#[test]
	fn cleanup_removes_invalid_remembered_feed_keys() {
		let _guard = test_lock();
		reset_test_caches();

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		remembered.insert("u".to_string(), remembered_entry("u"));
		remembered.insert("rust".to_string(), remembered_entry("rust"));
		drop(remembered);

		cleanup_decayed_remembered_subreddits();

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		assert!(!remembered.contains_key("u"));
		assert!(remembered.contains_key("rust"));
	}

	#[test]
	fn proven_only_picker_skips_never_successful_rows() {
		let _guard = test_lock();
		reset_test_caches();

		let mut unproven = remembered_entry("unproven");
		unproven.last_requested_at = Instant::now();
		let mut proven = remembered_entry("proven");
		proven.last_requested_at = Instant::now() - Duration::from_secs(120);
		proven.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(60 * 60));

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		remembered.insert("unproven".to_string(), unproven);
		remembered.insert("proven".to_string(), proven);
		drop(remembered);

		assert_eq!(pick_next_remembered_feed_to_refresh().map(|source| source.display_name()), Some("unproven".to_string()));
		assert_eq!(
			pick_next_remembered_feed_to_refresh_with_mode(RememberedFeedPickMode::ProvenOnly).map(|source| source.display_name()),
			Some("proven".to_string())
		);
	}

	#[test]
	fn cache_status_ignores_invalid_visible_subscriptions() {
		let _guard = test_lock();
		reset_test_caches();

		let prefs = Preferences {
			subscriptions: vec!["u".to_string(), "u_ab".to_string(), "rust-dev".to_string(), "rust".to_string(), "u_demo-user".to_string()],
			..Preferences::default()
		};

		let status = cache_status_for_preferences(&prefs);
		let names = status.sources.iter().map(|source| source.display_name.as_str()).collect::<Vec<_>>();

		assert_eq!(names, vec!["rust", "u_demo-user"]);
	}

	#[test]
	fn cache_status_queue_positions_ignore_invalid_remembered_rows() {
		let _guard = test_lock();
		reset_test_caches();

		let mut invalid = remembered_entry("u");
		invalid.last_refresh_succeeded_at = None;
		let mut rust = remembered_entry("rust");
		rust.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(60 * 60));

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		remembered.insert("u".to_string(), invalid);
		remembered.insert("rust".to_string(), rust);
		drop(remembered);

		let prefs = Preferences {
			subscriptions: vec!["rust".to_string()],
			..Preferences::default()
		};

		let status = cache_status_for_preferences(&prefs);
		let rust_row = status.sources.iter().find(|source| source.display_name == "rust").expect("rust row");

		assert_eq!(rust_row.queue_position, Some(0));
		assert_eq!(rust_row.schedule_state, "scheduled");
	}

	#[test]
	fn pick_next_remembered_feed_prefers_recent_visible_request_over_stale_global_never_refreshed_row() {
		let _guard = test_lock();
		reset_test_caches();

		let mut stale_global = remembered_entry("staleglobal");
		stale_global.last_requested_at = Instant::now() - Duration::from_secs(24 * 60 * 60);
		stale_global.last_refresh_succeeded_at = None;

		let mut visible = remembered_entry("visible");
		visible.last_requested_at = Instant::now();
		visible.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(4 * 60 * 60));
		visible.estimated_post_interval_secs = 60;

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		remembered.insert("staleglobal".to_string(), stale_global);
		remembered.insert("visible".to_string(), visible);
		drop(remembered);

		assert_eq!(pick_next_remembered_feed_to_refresh().map(|source| source.display_name()), Some("visible".to_string()));
	}

	#[test]
	fn cache_status_marks_visible_preferences_as_recent_requests() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		{
			let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
			let entry = remembered.get_mut("rust").unwrap();
			entry.last_requested_at = Instant::now() - Duration::from_secs(24 * 60 * 60);
			entry.last_refresh_succeeded_at = Some(Instant::now() - Duration::from_secs(4 * 60 * 60));
			entry.estimated_post_interval_secs = 60;
		}

		let prefs = Preferences {
			subscriptions: vec!["rust".to_string()],
			..Preferences::default()
		};
		let status = cache_status_for_preferences(&prefs);
		let rust_row = status.sources.iter().find(|source| source.display_name == "rust").expect("rust row");

		assert_eq!(rust_row.schedule_state, "scheduled");
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		assert!(remembered["rust"].last_requested_at.elapsed().as_secs() < 5);
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

		assert_eq!(remembered_refresh_priority_at(&steady, &failing, Instant::now()), Ordering::Less);
	}

	#[test]
	fn remembered_feed_picker_handles_many_boundary_priority_rows() {
		let _guard = test_lock();
		reset_test_caches();
		let now = Instant::now();

		let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		for index in 0..1_500 {
			let mut entry = remembered_entry(&format!("feed_{index:04}"));
			entry.last_requested_at = now - Duration::from_secs(RECENT_REQUEST_PRIORITY_SECS);
			entry.last_refresh_succeeded_at = Some(now - Duration::from_secs(MIN_FRESHNESS_TARGET_SECS));
			entry.estimated_post_interval_secs = MIN_ESTIMATED_POST_INTERVAL_SECS;
			remembered.insert(entry.name.clone(), entry);
		}
		drop(remembered);

		let picked = pick_next_remembered_feed_to_refresh().expect("boundary rows should remain sortable");
		assert!(picked.display_name().starts_with("feed_"));
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

		assert!(get_cached_source_page(&new_source("rust"), 0, 25).is_some());
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("rust").unwrap();
		assert!(entry.last_refresh_succeeded_at.is_some());
		assert_eq!(entry.consecutive_failures, 0);
		assert!(!entry.refresh_in_flight);
		assert_eq!(entry.estimated_post_interval_secs, 120);
	}

	#[test]
	fn publish_plain_subreddit_refresh_seeds_subreddit_flair_cache() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");

		let mut news = test_post("one", "rust", 600);
		news.flair = Flair {
			flair_parts: Vec::new(),
			text: "News".to_string(),
			background_color: "#ffffff".to_string(),
			foreground_color: "#000000".to_string(),
		};
		let mut discussion = test_post("two", "rust", 480);
		discussion.flair = Flair {
			flair_parts: Vec::new(),
			text: "Discussion".to_string(),
			background_color: "#000000".to_string(),
			foreground_color: "#ffffff".to_string(),
		};

		publish_plain_subreddit_refresh("rust", vec![news, discussion], String::new(), RefreshOrigin::Daemon);

		let flair_names = crate::utils::cached_subreddit_flairs("rust").into_iter().map(|flair| flair.text).collect::<Vec<_>>();
		assert_eq!(flair_names, vec!["Discussion".to_string(), "News".to_string()]);
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
	fn failure_recording_uses_durable_backoff_for_not_found() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		record_plain_subreddit_refresh_failure("rust", "404 not found", RefreshOrigin::Daemon);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("rust").unwrap();
		let banned_until = entry.banned_until.expect("durable backoff");
		let remaining = banned_until.saturating_duration_since(Instant::now());
		assert!(remaining <= Duration::from_secs(DURABLE_UNAVAILABLE_BACKOFF_SECS));
		assert!(remaining > Duration::from_secs(DURABLE_UNAVAILABLE_BACKOFF_SECS.saturating_sub(5)));
		assert_eq!(entry.unavailable_reason.as_deref(), Some("not_found"));
	}

	#[test]
	fn failure_recording_treats_underscore_not_found_as_permanent() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("couplescumshots");
		record_plain_subreddit_refresh_failure("couplescumshots", "not_found", RefreshOrigin::Daemon);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("couplescumshots").unwrap();
		let banned_until = entry.banned_until.expect("not_found should get durable backoff");
		let remaining = banned_until.saturating_duration_since(Instant::now());
		assert!(remaining <= Duration::from_secs(DURABLE_UNAVAILABLE_BACKOFF_SECS));
		assert!(remaining > Duration::from_secs(DURABLE_UNAVAILABLE_BACKOFF_SECS.saturating_sub(5)));
		assert_eq!(entry.unavailable_reason.as_deref(), Some("not_found"));
	}

	#[test]
	fn picker_skips_underscore_not_found_feed_after_failure() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("couplescumshots");
		remember_subreddit_request("rust");
		record_plain_subreddit_refresh_failure("couplescumshots", "not_found", RefreshOrigin::Daemon);

		assert_eq!(pick_next_remembered_feed_to_refresh().map(|source| source.display_name()), Some("rust".to_string()));
	}

	#[test]
	fn failure_recording_keeps_unclassified_forbidden_transient() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("u_forbidden");
		record_plain_subreddit_refresh_failure("u_forbidden", r#"Reddit error 403 "null": "Forbidden""#, RefreshOrigin::Daemon);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("u_forbidden").unwrap();
		assert!(entry.banned_until.is_none());
		assert!(entry.unavailable_reason.is_none());
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
		assert!(remaining <= Duration::from_secs(DURABLE_UNAVAILABLE_BACKOFF_SECS));
		assert!(remaining > Duration::from_secs(DURABLE_UNAVAILABLE_BACKOFF_SECS.saturating_sub(5)));
		assert_eq!(entry.unavailable_reason.as_deref(), Some("banned"));
	}

	#[test]
	fn failure_recording_uses_durable_backoff_for_explicit_private_state() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("closed");
		record_plain_subreddit_refresh_failure("closed", "private", RefreshOrigin::Daemon);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("closed").unwrap();
		assert_eq!(entry.unavailable_reason.as_deref(), Some("private"));
		assert!(entry.banned_until.is_some_and(|until| until > Instant::now()));
	}

	#[test]
	fn durable_backoff_expiry_makes_source_eligible_again() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		{
			let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
			let entry = remembered.get_mut("rust").unwrap();
			entry.banned_until = Some(Instant::now() - Duration::from_secs(1));
			entry.unavailable_reason = Some("banned".to_string());
		}

		assert_eq!(pick_next_remembered_feed_to_refresh().map(|source| source.display_name()), Some("rust".to_string()));
	}

	#[test]
	fn durable_backoff_and_reason_survive_snapshot_restore() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("rust");
		record_plain_subreddit_refresh_failure("rust", "banned", RefreshOrigin::Daemon);
		let remembered = snapshot_persisted_remembered_subreddits(1_000_000);
		let unavailable = snapshot_persisted_source_unavailability();

		reset_test_caches();
		restore_persisted_state(Vec::new(), remembered, unavailable, 1_000_000, 1_000_100);

		let restored = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = restored.get("rust").expect("restored remembered source");
		assert_eq!(entry.unavailable_reason.as_deref(), Some("banned"));
		assert!(entry.banned_until.is_some_and(|until| until > Instant::now()));
		assert!(!entry.refresh_in_flight);
	}

	#[test]
	fn unsubscribe_preserves_shared_source_when_another_profile_follows() {
		let _guard = test_lock();
		reset_test_caches();
		let followed = Preferences {
			subscriptions: vec!["rust".to_string()],
			..Preferences::default()
		};
		reconcile_profile_memberships(vec![("one".to_string(), followed.clone()), ("two".to_string(), followed)]);
		record_remembered_feed_visit_signal(&new_source("rust"));

		reconcile_profile_memberships(vec![
			("one".to_string(), Preferences::default()),
			(
				"two".to_string(),
				Preferences {
					subscriptions: vec!["rust".to_string()],
					..Preferences::default()
				},
			),
		]);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("rust").unwrap();
		assert_eq!(entry.profile_followers, 1);
		assert!(!entry.unfollowed_decay);
		assert!(entry.visit_priority_score > 0.0);
	}

	#[test]
	fn last_unsubscribe_removes_interest_and_enters_fast_decay() {
		let _guard = test_lock();
		reset_test_caches();
		reconcile_profile_memberships(vec![(
			"one".to_string(),
			Preferences {
				subscriptions: vec!["rust".to_string()],
				..Preferences::default()
			},
		)]);
		record_remembered_feed_visit_signal(&new_source("rust"));

		reconcile_profile_memberships(vec![("one".to_string(), Preferences::default())]);

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("rust").unwrap();
		assert_eq!(entry.profile_followers, 0);
		assert!(entry.unfollowed_decay);
		assert_eq!(entry.visit_priority_score, 0.0);
		assert!(entry.last_requested_at.elapsed() >= Duration::from_secs(REMEMBERED_SUBREDDIT_TTL_SECS - super::UNFOLLOWED_DECAY_GRACE_SECS));
		assert!(entry.banned_until.is_none());
	}

	#[test]
	fn daemon_continues_after_permanent_failures_and_successes_until_budget() {
		assert!(daemon_should_continue_after_refresh(
			RememberedRefreshOutcome::Failed(RememberedFailureDisposition::DurableUnavailable),
			1,
			0
		));
		assert!(daemon_should_continue_after_refresh(
			RememberedRefreshOutcome::Failed(RememberedFailureDisposition::DurableUnavailable),
			REMEMBERED_SUBREDDIT_DAEMON_MAX_ATTEMPTS_PER_TICK - 1,
			0
		));
		assert!(!daemon_should_continue_after_refresh(
			RememberedRefreshOutcome::Failed(RememberedFailureDisposition::DurableUnavailable),
			REMEMBERED_SUBREDDIT_DAEMON_MAX_ATTEMPTS_PER_TICK,
			0
		));
		assert!(!daemon_should_continue_after_refresh(
			RememberedRefreshOutcome::Failed(RememberedFailureDisposition::Transient),
			1,
			0
		));
		assert!(daemon_should_continue_after_refresh(
			RememberedRefreshOutcome::Succeeded,
			1,
			REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK - 1
		));
		assert!(!daemon_should_continue_after_refresh(
			RememberedRefreshOutcome::Succeeded,
			REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK,
			REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK
		));
	}

	#[test]
	fn cache_status_prediction_uses_daemon_success_batch_size() {
		assert_eq!(predicted_next_fetch_secs_for_position(10, 0), 10);
		assert_eq!(predicted_next_fetch_secs_for_position(10, REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK - 1), 10);
		assert_eq!(
			predicted_next_fetch_secs_for_position(10, REMEMBERED_SUBREDDIT_DAEMON_MAX_SUCCESSES_PER_TICK),
			10 + REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS
		);
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
		let user_source = new_source("u_demo");
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
		let fresh_source = new_source("fresh");
		let older_source = new_source("older");
		seed_source(&fresh_source, vec![test_post("fresh-1", "fresh", now), test_post("fresh-2", "fresh", now - 1)]);
		seed_source(&older_source, vec![test_post("older-1", "older", now - 2), test_post("older-2", "older", now - 3)]);

		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
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
		let rust_source = new_source("rust");
		seed_source(&rust_source, vec![test_post("one", "rust", chrono::Utc::now().timestamp() as u64)]);
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: vec![
				session_entry(new_source("u_demo"), SessionSourceState::Unstarted),
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
		let rust_source = new_source("rust");
		seed_source(&rust_source, posts.clone());
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
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
	fn background_target_caps_speculative_source_diversity() {
		let _guard = test_lock();
		reset_test_caches();
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: (0..120)
				.map(|index| session_entry(new_source(&format!("subreddit{index}")), SessionSourceState::Unstarted))
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: true,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(background_target_sources(&session), 12);
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
				..Default::default()
			},
			sources: (0..40)
				.map(|index| {
					session_entry(
						new_source(&format!("subreddit{index}")),
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
				..Default::default()
			},
			sources: (0..32)
				.map(|index| {
					session_entry(
						new_source(&format!("subreddit{index}")),
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
			let source = new_source(&format!("subreddit{index}"));
			seed_source(&source, vec![test_post(&format!("post-{index}"), &format!("subreddit{index}"), now - index as u64)]);
		}

		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: (0..30)
				.map(|index| {
					session_entry(
						new_source(&format!("subreddit{index}")),
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
	fn frozen_requested_page_skips_remerging_large_session() {
		let _guard = test_lock();
		reset_test_caches();
		let frozen_post = test_post("frozen", "rust", chrono::Utc::now().timestamp() as u64);
		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: (0..1_200)
				.map(|index| session_entry(new_source(&format!("subreddit{index}")), SessionSourceState::Unstarted))
				.collect(),
			frozen_pages: HashMap::from([(0, vec![frozen_post])]),
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
	fn new_session_defers_background_target_check_to_background_worker() {
		let _guard = test_lock();
		reset_test_caches();
		let hot_source = new_source("rust");
		seed_source(
			&hot_source,
			(0..100)
				.map(|index| test_post(&format!("post-{index}"), "rust", chrono::Utc::now().timestamp() as u64 - index as u64))
				.collect(),
		);
		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: std::iter::once(session_entry(hot_source, SessionSourceState::Ready))
				.chain((1..1_200).map(|index| session_entry(new_source(&format!("subreddit{index}")), SessionSourceState::Unstarted)))
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let warm_mode = plan_session_warm_mode(&mut session, 0, POSTS_PER_PAGE, true);

		assert_eq!(warm_mode, Some(SessionWarmMode::InitialLoad));
		assert!(session.background_active);
	}

	#[test]
	fn existing_session_with_unready_page_uses_cache_preferred_followup_mode() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		for index in 0..8 {
			let source = new_source(&format!("subreddit{index}"));
			seed_source(&source, vec![test_post(&format!("post-{index}"), &format!("subreddit{index}"), now - index as u64)]);
		}

		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: (0..20)
				.map(|index| {
					session_entry(
						new_source(&format!("subreddit{index}")),
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
	fn enough_posts_stop_background_warming_without_more_speculative_sources() {
		let _guard = test_lock();
		reset_test_caches();
		let posts = (0..100)
			.map(|index| test_post(&format!("post-{index}"), "rust", chrono::Utc::now().timestamp() as u64 - index as u64))
			.collect::<Vec<_>>();
		let hot_source = new_source("subreddit0");
		seed_source(&hot_source, posts);
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: (0..200)
				.map(|index| {
					session_entry(
						new_source(&format!("subreddit{index}")),
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

		assert!(background_target_reached(&session));
	}

	#[test]
	fn sparse_session_keeps_warming_until_diversity_cap() {
		let _guard = test_lock();
		reset_test_caches();
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: (0..40)
				.map(|index| {
					session_entry(
						new_source(&format!("subreddit{index}")),
						if index < 8 { SessionSourceState::Ready } else { SessionSourceState::Unstarted },
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

		assert!(!background_target_reached(&session));
	}

	#[test]
	fn older_sources_reenter_after_fresher_sources_are_exhausted() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let fresh_source = new_source("fresh");
		let stale_source = new_source("stale");
		seed_source(&fresh_source, vec![test_post("fresh-post", "fresh", now - 60)]);
		seed_source(&stale_source, vec![test_post("stale-post", "stale", now - 172_800)]);
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
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
		let rust_source = new_source("rust");
		seed_source(&rust_source, posts.clone());
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
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
		let ready_source = new_source("ready");
		seed_source(&ready_source, vec![test_post("one", "ready", chrono::Utc::now().timestamp() as u64)]);
		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: vec![
				session_entry(ready_source, SessionSourceState::Ready),
				session_entry(new_source("stuck"), SessionSourceState::Fetching),
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
			let source = new_source(&format!("subreddit{index}"));
			seed_source(&source, vec![test_post(&format!("post-{index}"), &format!("subreddit{index}"), now - index as u64)]);
		}
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
			},
			sources: (0..40)
				.map(|index| {
					session_entry(
						new_source(&format!("subreddit{index}")),
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
		assert!(background_target_reached(&session));
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
	fn sqlite_timeline_keeps_home_hot_on_legacy_and_selects_chronological_modes() {
		let base_session = FeedSession {
			id: "sqlite-routing".to_string(),
			request: FeedSessionRequest {
				profile_id: "profile".to_string(),
				sources: vec![new_source("rust")],
				..Default::default()
			},
			sources: vec![session_entry(new_source("rust"), SessionSourceState::Ready)],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		let mut hot = base_session.clone();
		hot.request.merge_sort = Some(FeedSort::Hot);
		assert!(sqlite_timeline_selection(&hot).is_none());

		let mut chronological = base_session.clone();
		chronological.request.merge_sort = Some(FeedSort::New);
		assert_eq!(sqlite_timeline_selection(&chronological), Some((1, crate::cache_sqlite::TimelineSort::New)));

		let mut discovered = base_session;
		discovered.request.merge_sort = Some(FeedSort::New);
		discovered.request.daemon_discovery_order = true;
		assert_eq!(sqlite_timeline_selection(&discovered), Some((1, crate::cache_sqlite::TimelineSort::Discovered)));

		discovered.request.bundle_followed_user_comments = true;
		assert_eq!(sqlite_timeline_selection(&discovered), Some((2, crate::cache_sqlite::TimelineSort::Discovered)));
	}

	#[test]
	fn sqlite_profile_identity_tracks_sources_and_normalized_filters() {
		let mut preferences = Preferences::default();
		preferences.subscriptions = vec!["rust".to_string(), "u_alice".to_string()];
		preferences.filters = vec!["Programming".to_string(), " programming ".to_string()];
		let first = sqlite_active_profile_id(&preferences, "prefs-cookie");
		assert_eq!(first, sqlite_active_profile_id(&preferences, "prefs-cookie"));

		preferences.filters = vec!["programming".to_string()];
		assert_eq!(first, sqlite_active_profile_id(&preferences, "prefs-cookie"));
		preferences.filters.push("u_blocked".to_string());
		assert_ne!(first, sqlite_active_profile_id(&preferences, "prefs-cookie"));
		assert_ne!(first, sqlite_active_profile_id(&preferences, "other-cookie"));
	}

	#[test]
	fn frozen_feed_pages_have_a_hard_nearest_page_cap() {
		let mut session = FeedSession {
			id: "frozen-cap".to_string(),
			request: FeedSessionRequest::default(),
			sources: Vec::new(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: true,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};
		for page in 0..(MAX_FROZEN_FEED_PAGES + 8) {
			freeze_feed_page(&mut session, page, vec![test_post(&format!("post-{page}"), "rust", page as u64)]);
		}
		assert_eq!(session.frozen_pages.len(), MAX_FROZEN_FEED_PAGES);
		assert!(session.frozen_pages.contains_key(&(MAX_FROZEN_FEED_PAGES + 7)));
		assert!(!session.frozen_pages.contains_key(&0));
	}

	#[test]
	fn feed_sessions_have_a_hard_lru_cap() {
		let mut sessions = HashMap::new();
		for index in 0..MAX_FEED_SESSIONS {
			sessions.insert(
				format!("session-{index}"),
				FeedSession {
					id: format!("session-{index}"),
					request: FeedSessionRequest::default(),
					sources: Vec::new(),
					frozen_pages: HashMap::new(),
					background_active: false,
					background_complete: true,
					requested_page: 0,
					last_updated: Instant::now() - Duration::from_secs((MAX_FEED_SESSIONS - index) as u64),
					revision: 0,
				},
			);
		}
		prune_feed_sessions_locked(&mut sessions, "incoming");
		assert_eq!(sessions.len(), MAX_FEED_SESSIONS - 1);
		assert!(!sessions.contains_key("session-0"));
		assert!(sessions.contains_key(&format!("session-{}", MAX_FEED_SESSIONS - 1)));
	}

	#[test]
	fn resident_source_cache_has_hard_count_and_byte_caps() {
		let mut cache = HashMap::new();
		for index in 0..4 {
			let source = FeedSource::subreddit(format!("source{index}"), FeedSort::New);
			cache.insert(
				source.cache_key(),
				CachedSourcePosts {
					source,
					posts: Vec::new(),
					after: String::new(),
					fetched_at: Instant::now(),
					last_accessed: Instant::now() - Duration::from_secs((4 - index) as u64),
					refresh_origin: None,
					estimated_bytes: 10,
				},
			);
		}
		let mut pruned = Vec::new();
		enforce_source_cache_limits_locked(&mut cache, 3, 25, &mut pruned);
		assert_eq!(cache.len(), 2);
		assert_eq!(cache.values().map(|entry| entry.estimated_bytes).sum::<usize>(), 20);
		assert_eq!(pruned.len(), 2);
		assert!(cache.contains_key("subreddit:source2:new"));
		assert!(cache.contains_key("subreddit:source3:new"));
	}

	#[test]
	fn stale_overlapping_source_refresh_cannot_replace_newer_generation() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		let older = begin_source_refresh(&source);
		let newer = begin_source_refresh(&source);

		assert_eq!(
			store_source_posts_with_token(&source, vec![test_post("newer", "rust", 200)], "new-after".to_string(), Some(RefreshOrigin::Daemon), newer,),
			SourceRefreshOutcome::Committed
		);
		assert_eq!(
			store_source_posts_with_token(&source, vec![test_post("older", "rust", 100)], "old-after".to_string(), Some(RefreshOrigin::Daemon), older,),
			SourceRefreshOutcome::Stale
		);

		let cached = SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&source.cache_key()).cloned().unwrap();
		assert_eq!(cached.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["newer"]);
		assert_eq!(cached.after, "new-after");
		assert_eq!(TEST_DURABLE_WRITE_COUNT.load(AtomicOrdering::SeqCst), 1);
	}

	#[test]
	fn stale_source_refresh_runs_no_shared_commit_effects() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		let older = begin_source_refresh(&source);
		let newer = begin_source_refresh(&source);
		assert_eq!(
			store_source_posts_with_token(&source, vec![test_post("newer", "rust", 200)], String::new(), Some(RefreshOrigin::DirectLiveFetch), newer,),
			SourceRefreshOutcome::Committed
		);
		let shared_effects = Arc::new(AtomicU64::new(0));
		let observed_effects = Arc::clone(&shared_effects);
		assert_eq!(
			store_source_posts_with_token_and_effects(
				&source,
				vec![test_post("older", "rust", 100)],
				String::new(),
				Some(RefreshOrigin::DirectLiveFetch),
				crate::post_components::ObservationSource::Peer,
				older,
				move |_| {
					observed_effects.fetch_add(1, AtomicOrdering::SeqCst);
				},
				|| {},
			),
			SourceRefreshOutcome::Stale
		);
		assert_eq!(shared_effects.load(AtomicOrdering::SeqCst), 0);
		assert!(!SOURCE_REFRESH_STATES.lock().unwrap_or_else(|error| error.into_inner()).contains_key(&source.cache_key()));
	}

	#[test]
	fn stale_user_live_peer_scrape_and_video_completions_run_no_shared_effects() {
		let _guard = test_lock();
		reset_test_caches();
		let cases = [
			(FeedSource::subreddit("u_video", FeedSort::New), crate::post_components::ObservationSource::Listing),
			(FeedSource::subreddit("u_submitted", FeedSort::New), crate::post_components::ObservationSource::Peer),
			(FeedSource::user_comments("u_comments", FeedSort::New), crate::post_components::ObservationSource::Scrape),
		];
		for (source, observation_source) in cases {
			let older = begin_source_refresh(&source);
			let newer = begin_source_refresh(&source);
			assert_eq!(
				store_source_posts_with_token(&source, vec![test_post("newer", "rust", 200)], String::new(), Some(RefreshOrigin::DirectLiveFetch), newer,),
				SourceRefreshOutcome::Committed
			);
			let shared_effects = Arc::new(AtomicU64::new(0));
			let observed_effects = Arc::clone(&shared_effects);
			assert_eq!(
				store_source_posts_with_token_and_effects(
					&source,
					vec![test_post("older", "rust", 100)],
					String::new(),
					Some(RefreshOrigin::DirectLiveFetch),
					observation_source,
					older,
					move |_| {
						observed_effects.fetch_add(1, AtomicOrdering::SeqCst);
					},
					|| {},
				),
				SourceRefreshOutcome::Stale
			);
			assert_eq!(shared_effects.load(AtomicOrdering::SeqCst), 0);
		}
	}

	#[test]
	fn stale_success_cannot_clear_newer_refresh_in_flight() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		remember_subreddit_request("rust");
		let older = begin_remembered_source_refresh(&source);
		let newer = begin_remembered_source_refresh(&source);
		assert_eq!(
			publish_remembered_source_refresh_with_token(&source, vec![test_post("older", "rust", 100)], String::new(), RefreshOrigin::DirectLiveFetch, older,),
			SourceRefreshOutcome::Stale
		);
		assert!(
			REMEMBERED_SUBREDDITS
				.lock()
				.unwrap_or_else(|error| error.into_inner())
				.get("rust")
				.unwrap()
				.refresh_in_flight
		);
		drop(newer);
	}

	#[test]
	fn generic_refresh_supersession_cannot_strand_remembered_bookkeeping() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		remember_subreddit_request("rust");

		let daemon = begin_remembered_source_refresh(&source);
		let abandoned_generic = begin_source_refresh(&source);
		drop(abandoned_generic);
		assert!(!REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner())["rust"].refresh_in_flight);
		assert_eq!(
			publish_remembered_source_refresh_with_token(&source, vec![test_post("stale-daemon", "rust", 100)], String::new(), RefreshOrigin::Daemon, daemon,),
			SourceRefreshOutcome::Stale
		);
		assert_eq!(pick_next_remembered_feed_to_refresh().map(|source| source.display_name()), Some("rust".to_string()));

		let daemon = begin_remembered_source_refresh(&source);
		let committed_generic = begin_source_refresh(&source);
		assert_eq!(
			store_source_posts_with_token(
				&source,
				vec![test_post("generic", "rust", 300)],
				String::new(),
				Some(RefreshOrigin::DirectLiveFetch),
				committed_generic,
			),
			SourceRefreshOutcome::Committed
		);
		assert!(!REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner())["rust"].refresh_in_flight);
		assert_eq!(
			publish_remembered_source_refresh_with_token(&source, vec![test_post("stale-daemon-2", "rust", 200)], String::new(), RefreshOrigin::Daemon, daemon,),
			SourceRefreshOutcome::Stale
		);
		assert_eq!(pick_next_remembered_feed_to_refresh().map(|source| source.display_name()), Some("rust".to_string()));
	}

	#[test]
	fn concurrent_last_token_clone_drops_clear_remembered_bookkeeping_once() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		remember_subreddit_request("rust");
		let first = begin_remembered_source_refresh(&source);
		let second = first.clone();
		let barrier = Arc::new(Barrier::new(3));
		let drop_clone = |token, barrier: Arc<Barrier>| {
			std::thread::spawn(move || {
				barrier.wait();
				drop(token);
			})
		};
		let first_drop = drop_clone(first, Arc::clone(&barrier));
		let second_drop = drop_clone(second, Arc::clone(&barrier));
		barrier.wait();
		first_drop.join().unwrap();
		second_drop.join().unwrap();
		assert!(!REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner())["rust"].refresh_in_flight);
		assert_eq!(pick_next_remembered_feed_to_refresh().map(|source| source.display_name()), Some("rust".to_string()));
	}

	#[test]
	fn stale_failure_cannot_overwrite_newer_success_bookkeeping() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		remember_subreddit_request("rust");
		let older = begin_remembered_source_refresh(&source);
		let newer = begin_remembered_source_refresh(&source);
		assert_eq!(
			publish_remembered_source_refresh_with_token(&source, vec![test_post("newer", "rust", 200)], String::new(), RefreshOrigin::DirectLiveFetch, newer,),
			SourceRefreshOutcome::Committed
		);
		assert_eq!(
			record_remembered_source_refresh_failure_with_token(&source, "network failed", RefreshOrigin::DirectLiveFetch, older),
			SourceRefreshOutcome::Stale
		);
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner());
		let entry = remembered.get("rust").unwrap();
		assert_eq!(entry.consecutive_failures, 0);
		assert!(entry.last_refresh_failed_at.is_none());
		assert!(!entry.refresh_in_flight);
	}

	#[test]
	fn durable_publication_failure_clears_in_flight_and_schedules_retry() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		remember_subreddit_request("rust");
		let token = begin_remembered_source_refresh(&source);
		TEST_DURABLE_WRITE_FAIL.store(true, AtomicOrdering::SeqCst);
		assert_eq!(
			publish_remembered_source_refresh_with_token(&source, vec![test_post("failed", "rust", 100)], String::new(), RefreshOrigin::Daemon, token,),
			SourceRefreshOutcome::Failed
		);
		TEST_DURABLE_WRITE_FAIL.store(false, AtomicOrdering::SeqCst);
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner());
		let entry = remembered.get("rust").unwrap();
		assert_eq!(entry.consecutive_failures, 1);
		assert!(entry.last_refresh_failed_at.is_some());
		assert!(!entry.refresh_in_flight);
		assert!(SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&source.cache_key()).is_none());
	}

	#[test]
	fn stale_or_failed_publication_returns_only_an_authoritative_window() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		seed_source(&source, vec![test_post("authoritative", "rust", 100)]);
		let mut candidate = SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&source.cache_key()).cloned().unwrap();
		candidate.posts = vec![test_post("unpublished", "rust", 200)];

		let stale = resolve_source_refresh_result(&source, SourceRefreshOutcome::Stale, candidate.clone()).unwrap();
		assert_eq!(stale.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["authoritative"]);
		let failed = resolve_source_refresh_result(&source, SourceRefreshOutcome::Failed, candidate.clone()).unwrap();
		assert_eq!(failed.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["authoritative"]);
		let committed = resolve_source_refresh_result(&source, SourceRefreshOutcome::Committed, candidate.clone()).unwrap();
		assert_eq!(committed.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["unpublished"]);

		SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).clear();
		assert!(resolve_source_refresh_result(&source, SourceRefreshOutcome::Stale, candidate.clone()).is_err());
		assert!(resolve_source_refresh_result(&source, SourceRefreshOutcome::Failed, candidate).is_err());
	}

	#[test]
	fn prune_cannot_remove_catalog_published_by_concurrent_refresh() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		seed_source(&source, vec![test_post("old", "rust", 100)]);
		age_source_cache_for_test(&source, Duration::from_secs(CACHE_TTL_SECS + 60));
		let token = begin_source_refresh(&source);
		TEST_PRUNE_REMOVE_DELAY_MS.store(200, AtomicOrdering::SeqCst);
		let pruning = std::thread::spawn(prune_source_cache);
		let wait_started = Instant::now();
		while !TEST_PRUNE_REMOVE_ACTIVE.load(AtomicOrdering::SeqCst) {
			assert!(wait_started.elapsed() < Duration::from_secs(2), "prune removal hook did not start");
			std::thread::yield_now();
		}
		assert_eq!(
			store_source_posts_with_token(&source, vec![test_post("new", "rust", 200)], String::new(), Some(RefreshOrigin::DirectLiveFetch), token,),
			SourceRefreshOutcome::Committed
		);
		pruning.join().unwrap();
		TEST_PRUNE_REMOVE_DELAY_MS.store(0, AtomicOrdering::SeqCst);
		let candidates = catalog_source_candidates(std::slice::from_ref(&source), &HashSet::new()).unwrap();
		assert_eq!(candidates.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["new"]);
	}

	#[test]
	fn decay_gc_invalidates_in_flight_refresh_before_removal() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		remember_subreddit_request("rust");
		seed_source(&source, vec![test_post("old", "rust", 100)]);
		let token = begin_source_refresh(&source);
		REMEMBERED_SUBREDDITS
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.get_mut("rust")
			.unwrap()
			.last_requested_at = Instant::now() - Duration::from_secs(REMEMBERED_SUBREDDIT_TTL_SECS + 60);
		cleanup_decayed_remembered_subreddits();
		assert_eq!(
			store_source_posts_with_token(&source, vec![test_post("late", "rust", 200)], String::new(), Some(RefreshOrigin::Daemon), token,),
			SourceRefreshOutcome::Stale
		);
		assert!(!REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner()).contains_key("rust"));
		assert!(SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&source.cache_key()).is_none());
		assert!(catalog_source_candidates(std::slice::from_ref(&source), &HashSet::new()).unwrap().is_empty());
	}

	#[test]
	fn deleted_media_invalidation_rejects_first_fetch_without_resident_source() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		remember_subreddit_request("rust");
		let token = begin_remembered_source_refresh(&source);
		assert!(REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner())["rust"].refresh_in_flight);
		assert_eq!(mark_deleted_reddit_media_url("https://preview.redd.it/deleted.jpg?width=640&format=pjpg"), 0);
		assert!(!REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner())["rust"].refresh_in_flight);
		assert_eq!(
			publish_remembered_source_refresh_with_token(&source, vec![test_post("late", "rust", 200)], String::new(), RefreshOrigin::DirectLiveFetch, token,),
			SourceRefreshOutcome::Stale
		);
		assert!(SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&source.cache_key()).is_none());
		let token = begin_remembered_source_refresh(&source);
		let mut deleted = test_post("new-fetch", "rust", 300);
		deleted.post_type = "image".to_string();
		deleted.media.url = "https://preview.redd.it/deleted.jpg?width=640&format=pjpg".to_string();
		assert_eq!(
			publish_remembered_source_refresh_with_token(&source, vec![deleted], String::new(), RefreshOrigin::DirectLiveFetch, token,),
			SourceRefreshOutcome::Committed
		);
		assert!(!REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|error| error.into_inner())["rust"].refresh_in_flight);
		assert!(SOURCE_CACHE
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.get(&source.cache_key())
			.unwrap()
			.posts
			.is_empty());
	}

	#[test]
	fn restored_deleted_media_tombstone_prunes_persisted_source_before_import() {
		let _guard = test_lock();
		reset_test_caches();
		super::restore_deleted_media_tombstones(vec!["preview.redd.it/deleted.jpg".to_string()]);
		let mut deleted = test_post("persisted", "rust", 300);
		deleted.post_type = "image".to_string();
		deleted.media.url = "https://preview.redd.it/deleted.jpg?width=640&format=pjpg".to_string();
		let mut persisted = vec![crate::cache_persistence::PersistedPost::from_runtime(&deleted)];
		assert!(super::prune_known_deleted_media_from_persisted_posts(&mut persisted));
		assert!(persisted.is_empty());
	}

	#[test]
	fn deleted_media_rewrite_failure_keeps_pruned_ram_and_latches_durable_reads() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		let mut deleted = test_post("resident", "rust", 300);
		deleted.post_type = "image".to_string();
		deleted.media.url = "https://preview.redd.it/deleted-write-failure.jpg?width=640&format=pjpg".to_string();
		seed_source(&source, vec![deleted]);
		TEST_DURABLE_WRITE_FAIL.store(true, AtomicOrdering::SeqCst);
		assert_eq!(mark_deleted_reddit_media_url("https://preview.redd.it/deleted-write-failure.jpg?width=640&format=pjpg"), 1);
		assert!(SOURCE_CACHE
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.get(&source.cache_key())
			.unwrap()
			.posts
			.is_empty());
		assert!(!super::deleted_media_safety_healthy());
		TEST_DURABLE_WRITE_FAIL.store(false, AtomicOrdering::SeqCst);
	}

	#[test]
	fn deleted_media_tombstone_write_failure_latches_reads_and_keeps_pruned_ram() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		let mut deleted = test_post("resident-sidecar", "rust", 300);
		deleted.post_type = "image".to_string();
		deleted.media.url = "https://preview.redd.it/deleted-sidecar-failure.jpg?width=640&format=pjpg".to_string();
		seed_source(&source, vec![deleted]);
		TEST_TOMBSTONE_WRITE_FAIL.store(true, AtomicOrdering::SeqCst);
		assert_eq!(
			mark_deleted_reddit_media_url("https://preview.redd.it/deleted-sidecar-failure.jpg?width=640&format=pjpg"),
			1
		);
		assert!(SOURCE_CACHE
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.get(&source.cache_key())
			.unwrap()
			.posts
			.is_empty());
		assert!(!super::deleted_media_safety_healthy());
		assert!(servable_cached_source_posts(&source).is_none());
		assert!(catalog_source_candidates(std::slice::from_ref(&source), &HashSet::new()).is_none());
		TEST_TOMBSTONE_WRITE_FAIL.store(false, AtomicOrdering::SeqCst);
		let token = begin_source_refresh(&source);
		assert_eq!(
			store_source_posts_with_token(
				&source,
				vec![test_post("post-failure-refresh", "rust", 400)],
				String::new(),
				Some(RefreshOrigin::Daemon),
				token
			),
			SourceRefreshOutcome::Failed
		);
		assert!(SOURCE_CACHE
			.lock()
			.unwrap_or_else(|error| error.into_inner())
			.get(&source.cache_key())
			.unwrap()
			.posts
			.is_empty());
	}

	#[test]
	fn deleted_media_marker_write_failure_does_not_apply_an_unrecoverable_deletion() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		let mut deleted = test_post("resident-marker", "rust", 300);
		deleted.post_type = "image".to_string();
		deleted.media.url = "https://preview.redd.it/deleted-marker-failure.jpg?width=640&format=pjpg".to_string();
		seed_source(&source, vec![deleted]);

		TEST_TOMBSTONE_MARKER_WRITE_FAIL.store(true, AtomicOrdering::SeqCst);
		assert_eq!(mark_deleted_reddit_media_url("https://preview.redd.it/deleted-marker-failure.jpg?width=640&format=pjpg"), 0);
		assert!(!super::deleted_media_safety_healthy());
		assert!(servable_cached_source_posts(&source).is_none());
		assert!(catalog_source_candidates(std::slice::from_ref(&source), &HashSet::new()).is_none());
		assert_eq!(
			SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&source.cache_key()).unwrap().posts.len(),
			1
		);

		TEST_TOMBSTONE_MARKER_WRITE_FAIL.store(false, AtomicOrdering::SeqCst);
		let token = begin_source_refresh(&source);
		assert_eq!(
			store_source_posts_with_token(
				&source,
				vec![test_post("post-marker-failure-refresh", "rust", 400)],
				String::new(),
				Some(RefreshOrigin::Daemon),
				token
			),
			SourceRefreshOutcome::Failed
		);
		assert_eq!(
			SOURCE_CACHE.lock().unwrap_or_else(|error| error.into_inner()).get(&source.cache_key()).unwrap().posts[0].id,
			"resident-marker"
		);
	}

	#[test]
	fn hot_cache_read_is_not_blocked_by_durable_shadow_write() {
		let _guard = test_lock();
		reset_test_caches();
		let source = new_source("rust");
		remember_subreddit_request("rust");
		store_source_posts_with_origin(&source, vec![test_post("initial", "rust", 100)], String::new(), Some(RefreshOrigin::Daemon));
		let token = begin_source_refresh(&source);
		TEST_DURABLE_WRITE_DELAY_MS.store(200, AtomicOrdering::SeqCst);
		let writer_source = source.clone();
		let writer =
			std::thread::spawn(move || store_source_posts_with_token(&writer_source, vec![test_post("refreshed", "rust", 200)], String::new(), Some(RefreshOrigin::Daemon), token));
		let wait_started = Instant::now();
		while !TEST_DURABLE_WRITE_ACTIVE.load(AtomicOrdering::SeqCst) {
			assert!(wait_started.elapsed() < Duration::from_secs(2), "durable write hook did not start");
			std::thread::yield_now();
		}

		let read_started = Instant::now();
		let cached = servable_cached_source_posts(&source).expect("Hot source remains readable");
		assert!(read_started.elapsed() < Duration::from_millis(50));
		assert_eq!(cached[0].id, "initial");
		assert_eq!(writer.join().unwrap(), SourceRefreshOutcome::Committed);
		assert_eq!(servable_cached_source_posts(&source).unwrap()[0].id, "refreshed");
		TEST_DURABLE_WRITE_DELAY_MS.store(0, AtomicOrdering::SeqCst);
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
				..Default::default()
			},
			sources: vec![
				session_entry(new_source("older_refreshed"), SessionSourceState::Unstarted),
				session_entry(new_source("never_older"), SessionSourceState::Unstarted),
				session_entry(new_source("never_recent"), SessionSourceState::Unstarted),
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
				..Default::default()
			},
			sources: vec![
				session_entry(new_flair_source("rust", "News"), SessionSourceState::Unstarted),
				session_entry(new_source("rust"), SessionSourceState::Unstarted),
				session_entry(new_source("u_demo"), SessionSourceState::Unstarted),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(warm_targets(&session, 3), vec![1, 2, 0]);
	}

	#[test]
	fn merged_posts_skip_duplicate_ids_without_stalling_fresher_sources() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let freshest = new_source("freshest");
		let duplicate = new_source("duplicate");
		seed_source(&freshest, vec![test_post("shared", "freshest", now), test_post("fresh-next", "freshest", now - 2)]);
		seed_source(&duplicate, vec![test_post("shared", "duplicate", now - 1), test_post("dup-next", "duplicate", now - 3)]);

		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
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

	fn assert_frozen_chronological_pages_do_not_repeat_after_newer_posts_arrive(following: bool) {
		let now = chrono::Utc::now().timestamp() as u64;
		let older_source = new_source(if following { "u_older" } else { "older" });
		let newer_source = new_source(if following { "u_newer" } else { "newer" });
		let older_posts = (0..50)
			.map(|index| {
				let mut post = test_post(&format!("older-{index}"), "older", now - 1_000 - index as u64);
				post.author.name = "older".to_string();
				post
			})
			.collect::<Vec<_>>();
		seed_source(&older_source, older_posts);

		FEED_SESSIONS.lock().unwrap().insert(
			"demo".to_string(),
			FeedSession {
				id: "demo".to_string(),
				request: FeedSessionRequest {
					sources: vec![older_source.clone(), newer_source.clone()],
					filters: HashSet::new(),
					quarantine: false,
					query_fingerprint: "demo".to_string(),
					bundle_followed_user_comments: following,
					..Default::default()
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
		assert_eq!(initial_ids[0], "older-0");
		assert_eq!(initial_ids.len(), POSTS_PER_PAGE);

		let newer_posts = (0..30)
			.map(|index| {
				let mut post = test_post(&format!("newer-{index}"), "newer", now - index as u64);
				post.author.name = "newer".to_string();
				post
			})
			.collect::<Vec<_>>();
		seed_source(&newer_source, newer_posts);

		let refreshed_first_page = feed_page("demo", 0, POSTS_PER_PAGE).expect("stable first page");
		let refreshed_ids = refreshed_first_page.posts.iter().map(|post| post.id.clone()).collect::<Vec<_>>();
		let next_page = feed_page("demo", 1, POSTS_PER_PAGE).expect("next page");
		let next_ids = next_page.posts.iter().map(|post| post.id.clone()).collect::<Vec<_>>();
		let frozen_ids = initial_ids.iter().collect::<HashSet<_>>();

		assert_eq!(refreshed_ids, initial_ids);
		assert_eq!(next_ids[0], "newer-0");
		assert_eq!(next_ids.len(), POSTS_PER_PAGE);
		assert!(next_ids.iter().all(|id| !frozen_ids.contains(id)), "next page repeated a frozen page-zero post");
	}

	#[test]
	fn home_next_page_excludes_frozen_ids_after_newer_posts_arrive() {
		let _guard = test_lock();
		reset_test_caches();
		assert_frozen_chronological_pages_do_not_repeat_after_newer_posts_arrive(false);
	}

	#[test]
	fn following_next_page_excludes_frozen_ids_after_newer_posts_arrive() {
		let _guard = test_lock();
		reset_test_caches();
		assert_frozen_chronological_pages_do_not_repeat_after_newer_posts_arrive(true);
	}

	fn assert_sqlite_lost_cursor_excludes_frozen_ids_and_fills_page(following: bool) {
		let now = chrono::Utc::now().timestamp() as u64;
		let older_source = new_source("u_older");
		let newer_source = new_source("u_newer");
		let mut older_posts = (0..50)
			.map(|index| {
				let mut post = test_post(&format!("older-{index}"), "older", now - 1_000 - index as u64);
				post.author.name = "older".to_string();
				post
			})
			.collect::<Vec<_>>();
		let mut newer_posts = (0..10)
			.map(|index| {
				let mut post = test_post(&format!("newer-{index}"), "newer", now - index as u64);
				post.author.name = "newer".to_string();
				post
			})
			.collect::<Vec<_>>();
		let frozen_page = older_posts[..POSTS_PER_PAGE].to_vec();
		let frozen_ids = frozen_page.iter().map(|post| post.id.clone()).collect::<HashSet<_>>();
		newer_posts.append(&mut older_posts);
		let current_generation = newer_posts;
		let session_id = if following { "sqlite-following-lost-cursor" } else { "sqlite-home-lost-cursor" };
		let session = FeedSession {
			id: session_id.to_string(),
			request: FeedSessionRequest {
				sources: vec![older_source, newer_source],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "sqlite-lost-cursor".to_string(),
				profile_id: session_id.to_string(),
				bundle_followed_user_comments: following,
				merge_sort: Some(FeedSort::New),
				..Default::default()
			},
			sources: Vec::new(),
			frozen_pages: HashMap::from([(0, frozen_page)]),
			background_active: false,
			background_complete: true,
			requested_page: 1,
			last_updated: Instant::now(),
			revision: 0,
		};
		let expected_view_kind = if following { 2 } else { 1 };
		assert_eq!(sqlite_timeline_selection(&session), Some((expected_view_kind, crate::cache_sqlite::TimelineSort::New)));

		// Model a page-zero cursor pinned to the old generation, followed by the
		// in-memory cursor registry being lost before querying generation two.
		SQLITE_SESSION_CURSORS.lock().unwrap().insert(
			session.id.clone(),
			SqliteSessionCursorState {
				last_accessed: Instant::now(),
				after_page: HashMap::from([(
					0,
					crate::cache_sqlite::TimelineCursor {
						generation: 1,
						random_epoch: 0,
						sort_primary: 0,
						sort_secondary: 0,
						card_key: "older-24".to_string(),
					},
				)]),
			},
		);
		SQLITE_SESSION_CURSORS.lock().unwrap().clear();

		let mut query_count = 0usize;
		let result = sqlite_timeline_page_for_session_with_query(&session, 1, POSTS_PER_PAGE, |cursor, limit| {
			query_count += 1;
			let start = cursor
				.and_then(|cursor| current_generation.iter().position(|post| post.id == cursor.card_key))
				.map_or(0, |index| index + 1);
			let end = start.saturating_add(limit).min(current_generation.len());
			let batch = &current_generation[start..end];
			let next = batch.last().map(|post| crate::cache_sqlite::TimelineCursor {
				generation: 2,
				random_epoch: 0,
				sort_primary: i64::try_from(post.created_ts).unwrap_or(i64::MAX),
				sort_secondary: 0,
				card_key: post.id.clone(),
			});
			Ok(Some(crate::cache_sqlite::TimelinePage {
				posts: batch.iter().map(crate::cache_persistence::PersistedPost::from_runtime).collect(),
				next,
			}))
		})
		.expect("SQLite page query")
		.expect("SQLite page");
		let ids = result.0.iter().map(|post| post.id.clone()).collect::<Vec<_>>();

		assert_eq!(ids.len(), POSTS_PER_PAGE);
		assert_eq!(ids.first().map(String::as_str), Some("newer-0"));
		assert!(ids.iter().all(|id| !frozen_ids.contains(id)), "SQLite continuation repeated a frozen page-zero post");
		assert!(query_count > 1, "excluded SQLite rows should be scanned past to fill the page");
	}

	#[test]
	fn sqlite_home_lost_cursor_excludes_frozen_ids_after_generation_advance() {
		let _guard = test_lock();
		reset_test_caches();
		assert_sqlite_lost_cursor_excludes_frozen_ids_and_fills_page(false);
	}

	#[test]
	fn sqlite_following_lost_cursor_excludes_frozen_ids_after_generation_advance() {
		let _guard = test_lock();
		reset_test_caches();
		assert_sqlite_lost_cursor_excludes_frozen_ids_and_fills_page(true);
	}

	#[test]
	fn feed_page_sorts_selected_posts_newest_first() {
		let _guard = test_lock();
		reset_test_caches();
		let source_a = new_source("alpha");
		let source_b = new_source("beta");
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
					..Default::default()
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
		let rust_source = new_source("rust");
		let golang_source = new_source("golang");
		seed_source(&rust_source, vec![test_post("rust-1", "rust", now)]);
		seed_source(&golang_source, vec![test_post("go-1", "golang", now - 1)]);

		let mut session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![rust_source.clone(), golang_source.clone()],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
				..Default::default()
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
		let rust_source = new_source("rust");
		let programming_source = new_source("programming");
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
					..Default::default()
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
	fn feed_status_counts_followed_accounts_when_bundling_user_comments() {
		let _guard = test_lock();
		reset_test_caches();
		let s1a = FeedSource::subreddit("u_alpha".to_string(), FeedSort::New);
		let s1b = FeedSource::user_comments("u_alpha".to_string(), FeedSort::New);
		let s2a = FeedSource::subreddit("u_beta".to_string(), FeedSort::New);
		let s2b = FeedSource::user_comments("u_beta".to_string(), FeedSort::New);

		FEED_SESSIONS.lock().unwrap().insert(
			"demo".to_string(),
			FeedSession {
				id: "demo".to_string(),
				request: FeedSessionRequest {
					sources: vec![s1a.clone(), s1b.clone(), s2a.clone(), s2b.clone()],
					filters: HashSet::new(),
					quarantine: false,
					query_fingerprint: "demo".to_string(),
					profile_id: String::new(),
					cache_revision: 0,
					bundle_followed_user_comments: true,
					daemon_discovery_order: false,
					random_rotation_seed: 0,
					merge_sort: None,
				},
				sources: vec![
					session_entry(s1a, SessionSourceState::Ready),
					session_entry(s1b, SessionSourceState::Ready),
					session_entry(s2a, SessionSourceState::Ready),
					session_entry(s2b, SessionSourceState::Unstarted),
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
		assert_eq!(status.ready_sources, 1);
		assert_eq!(status.total_sources, 2);
		assert!(status.status_message.contains("1 of 2 followed accounts"));
	}

	#[test]
	fn feed_status_counts_daemon_cached_sources_and_servable_remembered_entries() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		remember_subreddit_request("rust");
		let rust_source = new_source("rust");
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
					..Default::default()
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
		let rust_source = new_source("rust");
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
	fn cache_status_head_post_age_uses_freshest_cached_post_not_first_row() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let rust_source = new_source("rust");
		seed_source(&rust_source, vec![test_post("rust-old", "rust", now - 120), test_post("rust-new", "rust", now - 90)]);

		let prefs = Preferences {
			subscriptions: vec!["rust".to_string()],
			..Preferences::default()
		};

		let status = cache_status_for_preferences(&prefs);
		let source = status.sources.iter().find(|source| source.display_name == "rust").expect("rust cache status");

		assert_eq!(source.head_post_created_ts, Some(now - 90));
		assert!(source.head_post_age_secs.is_some_and(|age| age >= 90 && age < 95));
	}

	#[test]
	fn cache_status_reports_only_sources_visible_to_preferences() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let visible_source = new_source("rust");
		let hidden_source = new_source("private");
		seed_source(&visible_source, vec![test_post("rust-1", "rust", now)]);
		seed_source(&hidden_source, vec![test_post("private-1", "private", now)]);

		let prefs = Preferences {
			subscriptions: vec!["rust".to_string()],
			..Preferences::default()
		};

		let status = cache_status_for_preferences(&prefs);
		let names = status.sources.iter().map(|source| source.display_name.as_str()).collect::<Vec<_>>();

		assert_eq!(names, vec!["rust"]);
		assert!(status.sources[0].cached);
	}

	#[test]
	fn cache_status_seeds_remembered_rows_without_prior_feed_hit() {
		let _guard = test_lock();
		reset_test_caches();

		let prefs = Preferences {
			subscriptions: vec!["rust".to_string(), "u_follows".to_string()],
			..Preferences::default()
		};

		assert!(REMEMBERED_SUBREDDITS.lock().unwrap().is_empty());
		let status = cache_status_for_preferences(&prefs);
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		assert!(remembered.contains_key("rust"));
		assert!(remembered.contains_key("u_follows"));
		drop(remembered);

		let rust_row = status.sources.iter().find(|s| s.display_name == "rust").expect("rust row");
		assert!(rust_row.daemon_managed);
		assert_eq!(rust_row.schedule_state, "scheduled");
		let user_row = status.sources.iter().find(|s| s.display_name == "u_follows").expect("user row");
		assert_eq!(user_row.source_type, "user");
		assert_eq!(user_row.schedule_state, "scheduled");
	}

	#[test]
	fn cache_status_marks_flair_only_subscription_as_daemon_managed() {
		let _guard = test_lock();
		reset_test_caches();

		let prefs = Preferences {
			flair_subscriptions: vec![FlairSubscription {
				subreddit: "programming".to_string(),
				flair: "News".to_string(),
			}],
			..Preferences::default()
		};

		assert!(REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
		let status = cache_status_for_preferences(&prefs);
		let flair_key = format!("flair:programming:{}", URL_SAFE_NO_PAD.encode("News".as_bytes()));
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		assert!(remembered.contains_key(&flair_key));
		drop(remembered);

		let flair_row = status.sources.iter().find(|s| s.display_name == "programming#News").expect("flair row");
		assert_eq!(flair_row.source_type, "flair");
		assert!(flair_row.daemon_managed);
		assert_eq!(flair_row.schedule_state, "scheduled");
	}

	#[test]
	fn seed_remembered_feeds_from_source_cache_keys_recovers_daemon_queue() {
		let _guard = test_lock();
		reset_test_caches();

		let inserted = seed_remembered_feeds_from_source_cache_keys([
			"subreddit:rust:new",
			"user_comments:u_demo:new",
			"flair:programming:news:new",
			"subreddit:all:new",
			"subreddit:rust:hot",
			"not-a-source-key",
		]);

		assert_eq!(inserted, 3);
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		assert!(remembered.contains_key("rust"));
		assert!(remembered.contains_key("user_comments:u_demo"));
		assert!(remembered.contains_key(&format!("flair:programming:{}", URL_SAFE_NO_PAD.encode("news".as_bytes()))));
		assert!(!remembered.contains_key("all"));
		assert_eq!(remembered.len(), 3);
	}

	#[test]
	fn seed_remembered_feeds_from_source_cache_entries_restores_refresh_time() {
		let _guard = test_lock();
		reset_test_caches();

		let inserted = seed_remembered_feeds_from_source_cache_entries([("subreddit:rust:new", 1_000)], 10_000);

		assert_eq!(inserted, 1);
		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
		let entry = remembered.get("rust").expect("seeded rust");
		assert!(entry.last_refresh_succeeded_at.is_some());
		assert!(entry.last_requested_at.elapsed().as_secs() >= 8_900);
	}

	#[test]
	fn banned_refreshes_are_deprioritized_until_backoff_expires() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("vlogsex");
		remember_subreddit_request("rust");
		record_plain_subreddit_refresh_failure("vlogsex", "banned", RefreshOrigin::Daemon);

		assert_eq!(pick_next_remembered_subreddit_to_refresh().map(|s| s.display_name()), Some("rust".to_string()));

		let remembered = REMEMBERED_SUBREDDITS.lock().unwrap();
		let entry = remembered.get("vlogsex").unwrap();
		assert!(entry.banned_until.is_some_and(|until| until > Instant::now() + Duration::from_secs(24 * 60 * 60)));
	}

	#[test]
	fn persisted_restore_respects_remembered_retention_rules() {
		let _guard = test_lock();
		reset_test_caches();

		let stale_source = new_source("rust");
		store_source_posts_with_origin(&stale_source, vec![test_post("one", "rust", 100)], "after".to_string(), None);
		SOURCE_CACHE.lock().unwrap().get_mut(&stale_source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let stale_snapshot = snapshot_persisted_source_entries(1_000_000);
		SOURCE_CACHE.lock().unwrap().clear();
		restore_persisted_state(stale_snapshot, Vec::new(), Vec::new(), 1_000_000, 1_000_000);
		assert!(SOURCE_CACHE.lock().unwrap().is_empty());

		remember_subreddit_request("remembered");
		let remembered_source = new_source("remembered");
		store_source_posts_with_origin(&remembered_source, vec![test_post("two", "remembered", 200)], "after".to_string(), None);
		SOURCE_CACHE.lock().unwrap().get_mut(&remembered_source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let remembered_snapshot = snapshot_persisted_source_entries(1_000_100);
		let remembered_state = snapshot_persisted_remembered_subreddits(1_000_100);
		SOURCE_CACHE.lock().unwrap().clear();
		REMEMBERED_SUBREDDITS.lock().unwrap().clear();
		restore_persisted_state(remembered_snapshot, remembered_state, Vec::new(), 1_000_100, 1_000_100);
		assert!(SOURCE_CACHE.lock().unwrap().get(&remembered_source.cache_key()).is_none());
		assert!(REMEMBERED_SUBREDDITS.lock().unwrap().contains_key("remembered"));
	}

	#[test]
	fn persisted_restore_keeps_stale_followed_user_feed_when_remembered() {
		let _guard = test_lock();
		reset_test_caches();

		remember_subreddit_request("u_followeduser");
		let user_source = new_source("u_followeduser");
		store_source_posts_with_origin(&user_source, vec![test_post("uf", "somewhere", 400)], String::new(), None);
		SOURCE_CACHE.lock().unwrap().get_mut(&user_source.cache_key()).unwrap().fetched_at = Instant::now() - Duration::from_secs(CACHE_TTL_SECS + 60);

		let posts_snapshot = snapshot_persisted_source_entries(3_000_000);
		let remembered_snapshot = snapshot_persisted_remembered_subreddits(3_000_000);
		SOURCE_CACHE.lock().unwrap().clear();
		REMEMBERED_SUBREDDITS.lock().unwrap().clear();
		restore_persisted_state(posts_snapshot, remembered_snapshot, Vec::new(), 3_000_000, 3_000_000);

		assert!(SOURCE_CACHE.lock().unwrap().get(&user_source.cache_key()).is_none());
		assert!(REMEMBERED_SUBREDDITS.lock().unwrap().contains_key("u_followeduser"));
	}
}
