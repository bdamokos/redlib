use crate::user::collapse_duplicate_submissions;
use crate::utils::{Post, Preferences};
use log::{info, warn};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use tokio::time::{Duration, MissedTickBehavior};

const CACHE_TTL_SECS: u64 = 10_800;
const REMEMBERED_SUBREDDIT_TTL_SECS: u64 = 3 * 24 * 60 * 60;
const REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS: u64 = 5 * 60;
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
const FRESH_RECENT_SECONDS: i64 = 6 * 60 * 60;
const FRESH_CUTOFF_SECONDS: i64 = 24 * 60 * 60;
const MID_CUTOFF_SECONDS: i64 = 48 * 60 * 60;
pub const POSTS_PER_PAGE: usize = 25;

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
}

#[derive(Clone, Debug)]
pub struct RememberedSubredditEntry {
	pub name: String,
	pub last_requested_at: Instant,
	pub last_refresh_started_at: Option<Instant>,
	pub last_refresh_succeeded_at: Option<Instant>,
	pub last_refresh_failed_at: Option<Instant>,
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
enum SessionSourceState {
	Unstarted,
	Fetching,
	Ready,
	Failed,
}

#[derive(Clone)]
struct SessionSourceEntry {
	source: FeedSource,
	state: SessionSourceState,
	last_error: Option<String>,
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

fn new_remembered_subreddit_entry(name: String) -> RememberedSubredditEntry {
	RememberedSubredditEntry {
		name,
		last_requested_at: Instant::now(),
		last_refresh_started_at: None,
		last_refresh_succeeded_at: None,
		last_refresh_failed_at: None,
		consecutive_failures: 0,
		estimated_post_interval_secs: DEFAULT_ESTIMATED_POST_INTERVAL_SECS,
		refresh_in_flight: false,
	}
}

pub fn remember_subreddit_request(subreddit: &str) {
	let Some(normalized) = normalize_plain_subreddit_name(subreddit) else {
		return;
	};

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let entry = remembered.entry(normalized.clone()).or_insert_with(|| new_remembered_subreddit_entry(normalized));
	entry.last_requested_at = Instant::now();
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

fn source_cache_is_servable(source: &FeedSource, entry: &CachedSourcePosts) -> bool {
	match source {
		FeedSource::Subreddit { name } if !name.starts_with("u_") => is_subreddit_remembered(name) || !source_cache_is_expired(entry),
		_ => !source_cache_is_expired(entry),
	}
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
}

pub fn publish_plain_subreddit_refresh(subreddit: &str, posts: Vec<Post>, after: String, refresh_origin: RefreshOrigin) {
	let Some(normalized) = normalize_plain_subreddit_name(subreddit) else {
		return;
	};

	crate::utils::store_post_flairs(&posts);
	store_source_posts(&FeedSource::Subreddit { name: normalized.clone() }, posts.clone(), after);
	if matches!(refresh_origin, RefreshOrigin::Daemon | RefreshOrigin::FrontPageSession) {
		crate::video_preview::prewarm_listing_video_previews(&posts);
	}

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let Some(entry) = remembered.get_mut(&normalized) else {
		return;
	};

	entry.last_refresh_succeeded_at = Some(Instant::now());
	entry.last_refresh_failed_at = None;
	entry.consecutive_failures = 0;
	entry.estimated_post_interval_secs = estimate_post_interval_secs(&posts, entry.estimated_post_interval_secs);
	entry.refresh_in_flight = false;
}

pub fn record_plain_subreddit_refresh_failure(subreddit: &str, error: &str, refresh_origin: RefreshOrigin) {
	let Some(normalized) = normalize_plain_subreddit_name(subreddit) else {
		return;
	};

	let mut remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let Some(entry) = remembered.get_mut(&normalized) else {
		return;
	};

	entry.last_refresh_failed_at = Some(Instant::now());
	entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
	entry.refresh_in_flight = false;
	warn!("post_cache: plain subreddit refresh failed for {} via {:?}: {}", entry.name, refresh_origin, error);
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
}

pub fn pick_next_remembered_subreddit_to_refresh() -> Option<String> {
	let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
	let mut entries = remembered.values().filter(|entry| remembered_entry_is_active(entry)).cloned().collect::<Vec<_>>();
	entries.sort_by(remembered_refresh_priority);
	entries.into_iter().next().map(|entry| entry.name)
}

async fn refresh_remembered_subreddit(subreddit: &str) {
	mark_plain_subreddit_refresh_started(subreddit);
	let listing_path = format!("/r/{}/new.json?limit={PER_SOURCE_LIMIT}&raw_json=1", subreddit.replace('+', "%2B"));
	let listing_result = Post::fetch(&listing_path, false).await;
	let metadata_result = crate::subreddit::refresh_subreddit_metadata(subreddit, false).await;

	match listing_result {
		Ok((posts, after)) => {
			publish_plain_subreddit_refresh(subreddit, posts, after, RefreshOrigin::Daemon);
		}
		Err(error) => {
			record_plain_subreddit_refresh_failure(subreddit, &error, RefreshOrigin::Daemon);
		}
	}

	if let Err(error) = metadata_result {
		warn!("post_cache: failed to refresh metadata for {} via {:?}: {}", subreddit, RefreshOrigin::Daemon, error);
	}
}

pub fn spawn_remembered_subreddit_daemon() {
	tokio::spawn(async {
		let mut interval = tokio::time::interval(Duration::from_secs(REMEMBERED_SUBREDDIT_DAEMON_INTERVAL_SECS));
		interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
		interval.tick().await;

		loop {
			interval.tick().await;
			cleanup_decayed_remembered_subreddits();

			let Some(subreddit) = pick_next_remembered_subreddit_to_refresh() else {
				continue;
			};

			let should_skip = {
				let remembered = REMEMBERED_SUBREDDITS.lock().unwrap_or_else(|e| e.into_inner());
				remembered.get(&subreddit).is_none_or(|entry| !remembered_entry_is_active(entry) || entry.refresh_in_flight)
			};
			if should_skip {
				continue;
			}

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

pub fn get_cached_source_page(source: &FeedSource, page: usize, per_page: usize) -> Option<(Vec<Post>, String, bool, bool)> {
	let cache = SOURCE_CACHE.lock().ok()?;
	let entry = cache.get(&source.cache_key())?;
	if !source_cache_is_servable(source, entry) {
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

pub fn get_cached_subreddit_page(subreddit: &str, page: usize, per_page: usize) -> Option<(Vec<Post>, String, bool, bool)> {
	get_cached_source_page(&FeedSource::Subreddit { name: subreddit.to_string() }, page, per_page)
}

fn store_source_posts(source: &FeedSource, posts: Vec<Post>, after: String) {
	if let Ok(mut cache) = SOURCE_CACHE.lock() {
		cache.insert(
			source.cache_key(),
			CachedSourcePosts {
				posts,
				after,
				fetched_at: Instant::now(),
			},
		);
	}
}

fn cached_source_posts(source: &FeedSource) -> Option<CachedSourcePosts> {
	let cache = SOURCE_CACHE.lock().ok()?;
	let entry = cache.get(&source.cache_key())?;
	if source_cache_is_expired(entry) {
		return None;
	}

	Some(entry.clone())
}

async fn fetch_source_posts(source: FeedSource, quarantine: bool, refresh_origin: RefreshOrigin) -> Result<CachedSourcePosts, String> {
	if let Some(entry) = cached_source_posts(&source) {
		return Ok(entry);
	}

	if let FeedSource::Subreddit { name } = &source {
		mark_plain_subreddit_refresh_started(name);
	}

	let (posts, after) = match Post::fetch(&source.build_path(), quarantine).await {
		Ok(result) => result,
		Err(error) => {
			if let FeedSource::Subreddit { name } = &source {
				record_plain_subreddit_refresh_failure(name, &error, refresh_origin);
			}
			return Err(error);
		}
	};
	let posts = source.filter_posts(posts);

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

fn filtered_source_queues(session: &FeedSession) -> Vec<(FeedSource, VecDeque<Post>)> {
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
			.filter(|post| !session.request.filters.contains(&post.community) && !session.request.filters.contains(&format!("u_{}", post.author.name)))
			.cloned()
			.collect::<VecDeque<_>>();

		if !filtered.is_empty() {
			queues.push((entry.source.clone(), filtered));
		}
	}

	queues
}

fn active_ready_source_count(session: &FeedSession) -> usize {
	let queues = filtered_source_queues(session);
	active_source_keys(&queues, &HashSet::new()).len()
}

fn merged_posts_for_session(session: &FeedSession) -> Vec<Post> {
	let mut queues = filtered_source_queues(session);

	let mut all_posts = Vec::new();
	let mut seen_ids = HashSet::new();
	let mut exhausted_sources = HashSet::new();
	let mut consecutive_stalled_rounds = 0usize;

	while exhausted_sources.len() < queues.len() {
		let active_keys = active_source_keys(&queues, &exhausted_sources);
		if active_keys.is_empty() {
			break;
		}

		let mut made_progress = false;

		for (source, queue) in &mut queues {
			let cache_key = source.cache_key();
			if exhausted_sources.contains(&cache_key) || !active_keys.contains(&cache_key) {
				continue;
			}

			let mut emitted = false;
			while let Some(post) = queue.pop_front() {
				if seen_ids.insert(post.id.clone()) {
					all_posts.push(post);
					emitted = true;
					made_progress = true;
					break;
				}
			}

			if !emitted && queue.is_empty() {
				exhausted_sources.insert(cache_key);
			}
		}

		if !made_progress {
			consecutive_stalled_rounds += 1;
			if consecutive_stalled_rounds >= 2 {
				break;
			}

			for (source, queue) in &queues {
				if queue.is_empty() {
					exhausted_sources.insert(source.cache_key());
				}
			}
		} else {
			consecutive_stalled_rounds = 0;
		}
	}

	collapse_duplicate_submissions(all_posts)
}

fn active_source_keys(queues: &[(FeedSource, VecDeque<Post>)], exhausted_sources: &HashSet<String>) -> HashSet<String> {
	let now = chrono::Utc::now().timestamp() as u64;
	let mut heads = queues
		.iter()
		.filter_map(|(source, queue)| {
			let key = source.cache_key();
			if exhausted_sources.contains(&key) {
				return None;
			}
			queue.front().map(|post| (key, post.created_ts))
		})
		.collect::<Vec<_>>();

	if heads.is_empty() {
		return HashSet::new();
	}

	heads.sort_by(|left, right| right.1.cmp(&left.1));
	let freshest_ts = heads[0].1;
	let freshest_age = now.saturating_sub(freshest_ts);
	let cutoff = if freshest_age < FRESH_RECENT_SECONDS as u64 {
		FRESH_CUTOFF_SECONDS as u64
	} else if freshest_age < FRESH_CUTOFF_SECONDS as u64 {
		MID_CUTOFF_SECONDS as u64
	} else {
		u64::MAX
	};

	let active = heads
		.iter()
		.filter(|(_, ts)| freshest_ts.saturating_sub(*ts) <= cutoff)
		.map(|(key, _)| key.clone())
		.collect::<HashSet<_>>();

	if active.is_empty() {
		heads.into_iter().map(|(key, _)| key).collect()
	} else {
		active
	}
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
	let status_message = if warming {
		if ready_user_sources < total_user_sources {
			format!("Loading followed feeds in the background. Warmed {} of {} feeds.", ready_sources, session.sources.len())
		} else if !current_page_ready {
			format!("Warmed {} of {} feeds. Filling this page with newer posts.", ready_sources, session.sources.len())
		} else if next_page_ready {
			format!("Warmed {} of {} feeds. Next page is ready.", ready_sources, session.sources.len())
		} else {
			format!("Warmed {} of {} feeds. Preparing the next page in the background.", ready_sources, session.sources.len())
		}
	} else if session.background_complete {
		format!("Warmed all {} feeds.", session.sources.len())
	} else if next_page_ready {
		format!("Warmed {} of {} feeds. More feeds will load as you paginate.", ready_sources, session.sources.len())
	} else {
		format!("Warmed {} of {} feeds for now. More feeds will load as needed.", ready_sources, session.sources.len())
	};

	FeedSessionStatus {
		session_id: session.id.clone(),
		total_sources: session.sources.len(),
		ready_sources,
		failed_sources,
		warming,
		current_page_ready,
		next_page_ready,
		posts_merged: posts.len(),
		status_message,
		refresh_token: session.revision ^ ((posts.len() as u64) << 16) ^ (ready_sources as u64),
	}
}

fn touch_session(session: &mut FeedSession) {
	session.last_updated = Instant::now();
	session.revision = session.revision.wrapping_add(1);
}

fn background_target_posts(session: &FeedSession) -> usize {
	(session.requested_page + 1 + BACKGROUND_LOOKAHEAD_PAGES) * POSTS_PER_PAGE
}

fn round_robin_source_target(page: usize, per_page: usize) -> usize {
	((page + 1) * per_page).max(INITIAL_SOURCE_WINDOW)
}

fn initial_sync_source_target(page: usize) -> usize {
	round_robin_source_target(page, POSTS_PER_PAGE)
}

fn max_sync_source_budget(page: usize) -> usize {
	initial_sync_source_target(page) + (MAX_SYNC_FILL_BATCHES * PAGE_FILL_BATCH)
}

fn background_target_sources(session: &FeedSession) -> usize {
	round_robin_source_target(session.requested_page + BACKGROUND_LOOKAHEAD_PAGES, POSTS_PER_PAGE).min(session.sources.len())
}

fn attempted_source_count(session: &FeedSession) -> usize {
	session.sources.iter().filter(|entry| entry.state != SessionSourceState::Unstarted).count()
}

fn sync_sources_to_request(session: &FeedSession, page: usize) -> usize {
	initial_sync_source_target(page).saturating_sub(attempted_source_count(session))
}

fn followup_sync_source_budget(session: &FeedSession) -> usize {
	attempted_source_count(session).saturating_add(PAGE_FILL_BATCH).min(session.sources.len())
}

fn background_target_reached(session: &FeedSession) -> bool {
	merged_posts_for_session(session).len() >= background_target_posts(session) && attempted_source_count(session) >= background_target_sources(session)
}

fn session_has_remaining_sources(session: &FeedSession) -> bool {
	session.sources.iter().any(|entry| entry.state == SessionSourceState::Unstarted)
}

fn warm_targets(session: &FeedSession, target_count: usize) -> Vec<usize> {
	session
		.sources
		.iter()
		.enumerate()
		.filter(|(_, entry)| entry.state == SessionSourceState::Unstarted)
		.take(target_count)
		.map(|(index, _)| index)
		.collect()
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

async fn warm_session_for_round_robin(session_id: &str, page: usize, per_page: usize) {
	let target_sources = round_robin_source_target(page, per_page);

	loop {
		let remaining = {
			let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get(session_id) else {
				return;
			};

			if active_ready_source_count(session) >= target_sources {
				return;
			}

			warm_targets(session, PAGE_FILL_BATCH)
		};

		if remaining.is_empty() {
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
		max_sources: followup_sync_source_budget(session),
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
		warm_session_for_round_robin(&session_id, page, per_page).await;
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
					warm_session_for_round_robin(&background_id, page, per_page).await;
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

#[cfg(test)]
mod tests {
	use super::{
		active_ready_source_count, active_source_keys, attempted_source_count, background_target_reached, background_target_sources, count_sources_in_state, feed_page,
		feed_status, followup_sync_source_budget, get_cached_source_page, initial_sync_source_target, mark_fetching_sources_failed, max_sync_source_budget,
		merged_posts_for_session, pick_next_remembered_subreddit_to_refresh, plan_session_warm_mode, publish_plain_subreddit_refresh, reconcile_session_with_source_cache,
		record_plain_subreddit_refresh_failure, remember_frontpage_subreddits, remember_subreddit_request, remembered_refresh_priority, round_robin_source_target,
		session_status_from_posts, sort_sources, source_cache_is_servable, store_source_posts, sync_sources_to_request, FeedSession, FeedSessionRequest, FeedSource,
		RefreshOrigin, RememberedSubredditEntry, SessionSourceEntry, SessionSourceState, SessionWarmMode, CACHE_TTL_SECS, FEED_SESSIONS, POSTS_PER_PAGE, REMEMBERED_SUBREDDITS,
		SOURCE_CACHE,
	};
	use crate::utils::{Author, Awards, Flags, Flair, FlairSubscription, Media, Post, Preferences};
	use std::cmp::Ordering;
	use std::collections::{HashMap, HashSet, VecDeque};
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
	fn suppresses_clearly_stale_sources_when_fresh_cluster_exists() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let queues = vec![
			(
				FeedSource::Subreddit { name: "fresh".to_string() },
				VecDeque::from(vec![test_post("one", "fresh", now - 60)]),
			),
			(
				FeedSource::Subreddit { name: "recent".to_string() },
				VecDeque::from(vec![test_post("two", "recent", now - 7_200)]),
			),
			(
				FeedSource::Subreddit { name: "stale".to_string() },
				VecDeque::from(vec![test_post("three", "stale", now - 172_800)]),
			),
		];

		let active = active_source_keys(&queues, &HashSet::new());

		assert!(active.contains("subreddit:fresh"));
		assert!(active.contains("subreddit:recent"));
		assert!(!active.contains("subreddit:stale"));
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
		assert_eq!(initial_sync_source_target(0), 32);
		assert_eq!(max_sync_source_budget(0), 40);
		assert_eq!(initial_sync_source_target(2), 75);
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

		assert_eq!(followup_sync_source_budget(&session), 16);
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
		assert_eq!(round_robin_source_target(0, POSTS_PER_PAGE), 32);
		assert_eq!(round_robin_source_target(1, POSTS_PER_PAGE), 50);
		assert_eq!(round_robin_source_target(2, POSTS_PER_PAGE), 75);
		assert_eq!(initial_sync_source_target(2), 75);
	}

	#[test]
	fn active_ready_source_count_ignores_stale_heads() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		let fresh_source = FeedSource::Subreddit { name: "fresh".to_string() };
		let recent_source = FeedSource::Subreddit { name: "recent".to_string() };
		let stale_source = FeedSource::Subreddit { name: "stale".to_string() };
		seed_source(&fresh_source, vec![test_post("one", "fresh", now - 60)]);
		seed_source(&recent_source, vec![test_post("two", "recent", now - 7_200)]);
		seed_source(&stale_source, vec![test_post("three", "stale", now - 172_800)]);
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
				session_entry(recent_source, SessionSourceState::Ready),
				session_entry(stale_source, SessionSourceState::Ready),
			],
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 0,
			last_updated: Instant::now(),
			revision: 0,
		};

		assert_eq!(active_ready_source_count(&session), 2);
	}

	#[test]
	fn merged_posts_complete_a_full_first_pass_before_repeating_ready_sources() {
		let _guard = test_lock();
		reset_test_caches();
		let now = chrono::Utc::now().timestamp() as u64;
		for index in 0..100 {
			let source = FeedSource::Subreddit {
				name: format!("subreddit{index}"),
			};
			seed_source(
				&source,
				vec![
					test_post(&format!("post-{index}-a"), &format!("subreddit{index}"), now - index as u64),
					test_post(&format!("post-{index}-b"), &format!("subreddit{index}"), now - index as u64 - 1),
				],
			);
		}
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: (0..100)
				.map(|index| {
					session_entry(
						FeedSource::Subreddit {
							name: format!("subreddit{index}"),
						},
						SessionSourceState::Ready,
					)
				})
				.collect(),
			frozen_pages: HashMap::new(),
			background_active: false,
			background_complete: false,
			requested_page: 3,
			last_updated: Instant::now(),
			revision: 0,
		};

		let merged = merged_posts_for_session(&session);
		let first_hundred_sources = merged.iter().take(100).map(|post| post.community.clone()).collect::<HashSet<_>>();

		assert_eq!(first_hundred_sources.len(), 100);

		let repeated_sources = merged.iter().skip(100).take(25).map(|post| post.community.clone()).collect::<HashSet<_>>();

		assert_eq!(repeated_sources.len(), 25);
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
		assert_eq!(status.total_sources, 2);
	}
}
