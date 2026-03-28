use crate::user::collapse_duplicate_submissions;
use crate::utils::Post;
use log::{info, warn};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

const CACHE_TTL_SECS: u64 = 10_800;
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

static SOURCE_CACHE: LazyLock<Mutex<HashMap<String, CachedSourcePosts>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static FEED_SESSIONS: LazyLock<Mutex<HashMap<String, FeedSession>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn source_cache_is_expired(entry: &CachedSourcePosts) -> bool {
	entry.fetched_at.elapsed().as_secs() > CACHE_TTL_SECS
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

pub fn get_cached_subreddit_page(subreddit: &str, page: usize, per_page: usize) -> Option<(Vec<Post>, String, bool, bool)> {
	let cache = SOURCE_CACHE.lock().ok()?;
	let entry = cache.get(&FeedSource::Subreddit { name: subreddit.to_string() }.cache_key())?;
	if source_cache_is_expired(entry) {
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

async fn fetch_source_posts(source: FeedSource, quarantine: bool) -> Result<CachedSourcePosts, String> {
	if let Some(entry) = cached_source_posts(&source) {
		return Ok(entry);
	}

	let (posts, after) = Post::fetch(&source.build_path(), quarantine).await?;
	let posts = source.filter_posts(posts);
	crate::utils::store_post_flairs(&posts);
	store_source_posts(&source, posts.clone(), after.clone());

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
		background_active: false,
		background_complete: false,
		requested_page: 0,
		last_updated: Instant::now(),
		revision: 0,
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

fn session_page_from_posts(posts: &[Post], page: usize, per_page: usize, warming: bool) -> FeedPage {
	let start = page * per_page;
	let end = (start + per_page).min(posts.len());
	let page_posts = if start < posts.len() { posts[start..end].to_vec() } else { Vec::new() };
	let has_prev = page > 0;
	let has_next = end < posts.len() || warming;

	FeedPage {
		posts: page_posts,
		has_prev,
		has_next,
	}
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
	let current_page_ready = posts.len() >= ((page + 1) * per_page) || session.background_complete;
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
			handles.push((index, source, tokio::spawn(async move { fetch_source_posts(task_source, quarantine).await })));
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

pub async fn ensure_feed_session(request: FeedSessionRequest, page: usize, per_page: usize) -> String {
	let session_id = build_session_id(&request);
	reconcile_stuck_fetching_sources(&session_id).await;

	{
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		sessions.retain(|_, session| !session_is_expired(session));

		let session = sessions.entry(session_id.clone()).or_insert_with(|| make_session(request.clone()));
		session.requested_page = session.requested_page.max(page);
		touch_session(session);
	}

	let sync_target = {
		let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get(&session_id) else {
			return session_id;
		};
		warm_targets(session, sync_sources_to_request(session, page))
	};

	warm_session_indices(&session_id, sync_target).await;
	warm_session_to_fill(&session_id, page, per_page, max_sync_source_budget(page)).await;
	warm_session_for_round_robin(&session_id, page, per_page).await;

	let spawn_background = {
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get_mut(&session_id) else {
			return session_id;
		};

		if !session_has_remaining_sources(session) {
			session.background_complete = true;
			session.background_active = false;
			false
		} else if session.background_active {
			false
		} else if background_target_reached(session) {
			false
		} else {
			session.background_active = true;
			true
		}
	};

	if spawn_background {
		let background_id = session_id.clone();
		tokio::spawn(async move {
			warm_session_background(background_id).await;
		});
	}

	session_id
}

pub fn feed_page(session_id: &str, page: usize, per_page: usize) -> Option<FeedPage> {
	let sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get(session_id)?;
	if session_is_expired(session) {
		return None;
	}

	let posts = merged_posts_for_session(session);
	let has_more_available = session.background_active || page < session.requested_page || session_has_remaining_sources(session);
	Some(session_page_from_posts(&posts, page, per_page, has_more_available))
}

pub fn feed_status(session_id: &str, page: usize, per_page: usize) -> Option<FeedSessionStatus> {
	let sessions = FEED_SESSIONS.lock().ok()?;
	let session = sessions.get(session_id)?;
	if session_is_expired(session) {
		return None;
	}

	let posts = merged_posts_for_session(session);
	Some(session_status_from_posts(session, &posts, page, per_page))
}

pub fn cache_stats() -> (usize, usize) {
	let cache = SOURCE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
	let sources = cache.values().filter(|entry| !source_cache_is_expired(entry)).count();
	let posts: usize = cache.values().filter(|entry| !source_cache_is_expired(entry)).map(|entry| entry.posts.len()).sum();
	(sources, posts)
}

#[cfg(test)]
mod tests {
	use super::{
		active_ready_source_count, active_source_keys, attempted_source_count, background_target_reached, background_target_sources, count_sources_in_state,
		initial_sync_source_target, mark_fetching_sources_failed, max_sync_source_budget, merged_posts_for_session, round_robin_source_target, session_status_from_posts,
		sort_sources, store_source_posts, sync_sources_to_request, FeedSession, FeedSessionRequest, FeedSource, SessionSourceEntry, SessionSourceState, FEED_SESSIONS,
		POSTS_PER_PAGE, SOURCE_CACHE,
	};
	use crate::utils::{Author, Awards, Flags, Flair, Media, Post};
	use std::collections::{HashSet, VecDeque};
	use std::sync::{LazyLock, Mutex, MutexGuard};
	use std::time::Instant;

	static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

	fn test_lock() -> MutexGuard<'static, ()> {
		TEST_LOCK.lock().unwrap()
	}

	fn reset_test_caches() {
		SOURCE_CACHE.lock().unwrap().clear();
		FEED_SESSIONS.lock().unwrap().clear();
	}

	fn session_entry(source: FeedSource, state: SessionSourceState) -> SessionSourceEntry {
		SessionSourceEntry {
			source,
			state,
			last_error: None,
		}
	}

	fn seed_source(source: &FeedSource, posts: Vec<Post>) {
		store_source_posts(source, posts, String::new());
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
				.map(|index| session_entry(FeedSource::Subreddit { name: format!("subreddit{index}") }, SessionSourceState::Unstarted))
				.collect(),
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
	fn enough_posts_from_too_few_sources_does_not_stop_background_warming() {
		let _guard = test_lock();
		reset_test_caches();
		let posts = (0..100)
			.map(|index| test_post(&format!("post-{index}"), "rust", chrono::Utc::now().timestamp() as u64 - index as u64))
			.collect::<Vec<_>>();
		let hot_source = FeedSource::Subreddit {
			name: "subreddit0".to_string(),
		};
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
			seed_source(
				&source,
				vec![test_post(&format!("post-{index}"), &format!("subreddit{index}"), now - index as u64)],
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
			background_active: false,
			background_complete: false,
			requested_page: 3,
			last_updated: Instant::now(),
			revision: 0,
		};

		let merged = merged_posts_for_session(&session);
		let first_hundred_sources = merged
			.iter()
			.take(100)
			.map(|post| post.community.clone())
			.collect::<HashSet<_>>();

		assert_eq!(first_hundred_sources.len(), 100);

		let repeated_sources = merged
			.iter()
			.skip(100)
			.take(25)
			.map(|post| post.community.clone())
			.collect::<HashSet<_>>();

		assert_eq!(repeated_sources.len(), 25);
	}
}
