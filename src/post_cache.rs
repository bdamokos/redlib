use crate::user::collapse_duplicate_submissions;
use crate::utils::Post;
use log::{info, warn};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

const CACHE_TTL_SECS: u64 = 10_800;
const FETCH_CONCURRENCY: usize = 5;
const PER_SOURCE_LIMIT: usize = 100;
const INITIAL_SOURCE_WINDOW: usize = 32;
const PAGE_FILL_BATCH: usize = 4;
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
	posts: Vec<Post>,
	after: String,
	head_created_ts: Option<u64>,
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
	background_started: bool,
	background_complete: bool,
	last_updated: Instant,
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

async fn fetch_source_posts(source: FeedSource, quarantine: bool) -> Result<CachedSourcePosts, String> {
	if let Ok(cache) = SOURCE_CACHE.lock() {
		if let Some(entry) = cache.get(&source.cache_key()) {
			if !source_cache_is_expired(entry) {
				return Ok(entry.clone());
			}
		}
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
			posts: Vec::new(),
			after: String::new(),
			head_created_ts: None,
			last_error: None,
		})
		.collect();

	FeedSession {
		id,
		request,
		sources,
		background_started: false,
		background_complete: false,
		last_updated: Instant::now(),
	}
}

fn merged_posts_for_session(session: &FeedSession) -> Vec<Post> {
	let mut queues = Vec::new();

	for entry in &session.sources {
		if entry.state != SessionSourceState::Ready || entry.posts.is_empty() {
			continue;
		}

		let filtered = entry
			.posts
			.iter()
			.filter(|post| {
				!session.request.filters.contains(&post.community)
					&& !session.request.filters.contains(&format!("u_{}", post.author.name))
			})
			.cloned()
			.collect::<VecDeque<_>>();

		if !filtered.is_empty() {
			queues.push((entry.source.clone(), filtered));
		}
	}

	let active_keys = active_source_keys(&queues);
	let mut all_posts = Vec::new();
	let mut seen_ids = HashSet::new();

	loop {
		let mut made_progress = false;

		for (source, queue) in &mut queues {
			if !active_keys.contains(&source.cache_key()) {
				continue;
			}

			while let Some(post) = queue.pop_front() {
				if seen_ids.insert(post.id.clone()) {
					all_posts.push(post);
					made_progress = true;
					break;
				}
			}
		}

		if !made_progress {
			break;
		}
	}

	collapse_duplicate_submissions(all_posts)
}

fn active_source_keys(queues: &[(FeedSource, VecDeque<Post>)]) -> HashSet<String> {
	let now = chrono::Utc::now().timestamp() as u64;
	let mut heads = queues
		.iter()
		.filter_map(|(source, queue)| queue.front().map(|post| (source.cache_key(), post.created_ts)))
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
	let warming = !session.background_complete;
	let status_message = if warming {
		if ready_user_sources < total_user_sources {
			format!(
				"Loading followed feeds in the background. Warmed {} of {} feeds.",
				ready_sources,
				session.sources.len()
			)
		} else if !current_page_ready {
			format!(
				"Warmed {} of {} feeds. Filling this page with newer posts.",
				ready_sources,
				session.sources.len()
			)
		} else if next_page_ready {
			format!("Warmed {} of {} feeds. Next page is ready.", ready_sources, session.sources.len())
		} else {
			format!(
				"Warmed {} of {} feeds. Preparing the next page in the background.",
				ready_sources,
				session.sources.len()
			)
		}
	} else {
		format!("Warmed all {} feeds.", session.sources.len())
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
		refresh_token: session.last_updated.elapsed().as_millis() as u64 ^ (posts.len() as u64) ^ (ready_sources as u64),
	}
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
		session.last_updated = Instant::now();
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
			handles.push(tokio::spawn(async move { (index, source.clone(), fetch_source_posts(source, quarantine).await) }));
		}

		for handle in handles {
			let Ok((index, source, result)) = handle.await else {
				continue;
			};
			let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get_mut(session_id) else {
				continue;
			};
			let Some(entry) = session.sources.get_mut(index) else {
				continue;
			};

			match result {
				Ok(cached) => {
					entry.state = SessionSourceState::Ready;
					entry.posts = cached.posts;
					entry.after = cached.after;
					entry.head_created_ts = entry.posts.first().map(|post| post.created_ts);
					entry.last_error = None;
					log::trace!("post_cache: warmed {}", source.display_name());
				}
				Err(error) => {
					entry.state = SessionSourceState::Failed;
					entry.last_error = Some(error.clone());
					warn!("post_cache: failed to warm {}: {}", source.display_name(), error);
				}
			}
			session.last_updated = Instant::now();
		}
	}
}

async fn warm_session_to_fill(session_id: &str, page: usize, per_page: usize) {
	loop {
		let (page_ready, remaining) = {
			let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get(session_id) else {
				return;
			};
			let merged = merged_posts_for_session(session);
			let target = (page + 1) * per_page;
			(merged.len() >= target, warm_targets(session, PAGE_FILL_BATCH))
		};

		if page_ready || remaining.is_empty() {
			break;
		}

		warm_session_indices(session_id, remaining).await;
	}

	let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
	if let Some(session) = sessions.get_mut(session_id) {
		session.last_updated = Instant::now();
	}
}

async fn warm_session_background(session_id: String) {
	loop {
		let remaining = {
			let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			let Some(session) = sessions.get(&session_id) else {
				return;
			};
			warm_targets(session, PAGE_FILL_BATCH)
		};

		if remaining.is_empty() {
			let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
			if let Some(session) = sessions.get_mut(&session_id) {
				session.background_complete = true;
				session.last_updated = Instant::now();
			}
			info!("post_cache: feed session {session_id} warm complete");
			return;
		}

		warm_session_indices(&session_id, remaining).await;
	}
}

pub async fn ensure_feed_session(request: FeedSessionRequest, page: usize, per_page: usize) -> String {
	let session_id = build_session_id(&request);
	let mut spawn_background = false;

	{
		let mut sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		sessions.retain(|_, session| !session_is_expired(session));

		let session = sessions.entry(session_id.clone()).or_insert_with(|| make_session(request.clone()));
		session.last_updated = Instant::now();

		if !session.background_started {
			session.background_started = true;
			spawn_background = true;
		}
	}

	let sync_target = {
		let sessions = FEED_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
		let Some(session) = sessions.get(&session_id) else {
			return session_id;
		};
		let target_sources = INITIAL_SOURCE_WINDOW.max((page + 1) * PAGE_FILL_BATCH);
		warm_targets(session, target_sources)
	};

	warm_session_indices(&session_id, sync_target).await;
	warm_session_to_fill(&session_id, page, per_page).await;

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
	Some(session_page_from_posts(&posts, page, per_page, !session.background_complete))
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
		active_source_keys, session_status_from_posts, sort_sources, FeedSession, FeedSessionRequest, FeedSource, SessionSourceEntry, SessionSourceState,
	};
	use crate::utils::{Author, Awards, Flags, Flair, Media, Post};
	use std::collections::{HashSet, VecDeque};
	use std::time::Instant;

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
		let mut sources = vec![
			FeedSource::Flair {
				subreddit: "zeta".to_string(),
				flair: "Tag".to_string(),
			},
			FeedSource::Subreddit {
				name: "rust".to_string(),
			},
			FeedSource::Subreddit {
				name: "u_demo".to_string(),
			},
		];

		sort_sources(&mut sources);

		assert!(matches!(&sources[0], FeedSource::Subreddit { name } if name == "u_demo"));
		assert!(matches!(&sources[1], FeedSource::Subreddit { name } if name == "rust"));
		assert!(matches!(&sources[2], FeedSource::Flair { subreddit, .. } if subreddit == "zeta"));
	}

	#[test]
	fn suppresses_clearly_stale_sources_when_fresh_cluster_exists() {
		let now = chrono::Utc::now().timestamp() as u64;
		let queues = vec![
			(
				FeedSource::Subreddit {
					name: "fresh".to_string(),
				},
				VecDeque::from(vec![test_post("one", "fresh", now - 60)]),
			),
			(
				FeedSource::Subreddit {
					name: "recent".to_string(),
				},
				VecDeque::from(vec![test_post("two", "recent", now - 7_200)]),
			),
			(
				FeedSource::Subreddit {
					name: "stale".to_string(),
				},
				VecDeque::from(vec![test_post("three", "stale", now - 172_800)]),
			),
		];

		let active = active_source_keys(&queues);

		assert!(active.contains("subreddit:fresh"));
		assert!(active.contains("subreddit:recent"));
		assert!(!active.contains("subreddit:stale"));
	}

	#[test]
	fn status_message_mentions_followed_feeds_while_users_are_still_warming() {
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![
				SessionSourceEntry {
					source: FeedSource::Subreddit {
						name: "u_demo".to_string(),
					},
					state: SessionSourceState::Unstarted,
					posts: Vec::new(),
					after: String::new(),
					head_created_ts: None,
					last_error: None,
				},
				SessionSourceEntry {
					source: FeedSource::Subreddit {
						name: "rust".to_string(),
					},
					state: SessionSourceState::Ready,
					posts: vec![test_post("one", "rust", chrono::Utc::now().timestamp() as u64)],
					after: String::new(),
					head_created_ts: None,
					last_error: None,
				},
			],
			background_started: true,
			background_complete: false,
			last_updated: Instant::now(),
		};

		let status = session_status_from_posts(&session, &[], 0, 25);

		assert!(status.status_message.contains("Loading followed feeds in the background."));
	}

	#[test]
	fn status_message_mentions_next_page_when_ready() {
		let post_count = 60;
		let posts = (0..post_count)
			.map(|index| test_post(&index.to_string(), "rust", chrono::Utc::now().timestamp() as u64 - index as u64))
			.collect::<Vec<_>>();
		let session = FeedSession {
			id: "demo".to_string(),
			request: FeedSessionRequest {
				sources: vec![],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: "demo".to_string(),
			},
			sources: vec![SessionSourceEntry {
				source: FeedSource::Subreddit {
					name: "rust".to_string(),
				},
				state: SessionSourceState::Ready,
				posts: posts.clone(),
				after: String::new(),
				head_created_ts: None,
				last_error: None,
			}],
			background_started: true,
			background_complete: false,
			last_updated: Instant::now(),
		};

		let status = session_status_from_posts(&session, &posts, 0, 25);

		assert!(status.next_page_ready);
		assert!(status.status_message.contains("Next page is ready."));
	}
}
