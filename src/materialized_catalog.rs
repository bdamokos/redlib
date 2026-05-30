#[cfg(test)]
use crate::cache_persistence::PersistedPost;
use crate::post_cache::{FeedSession, FeedSort, FeedSource, SessionSourceState};
use crate::utils::Post;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CatalogSourceKey {
	Subreddit(String),
	Flair { subreddit: String, flair: String },
	UserComments(String),
}

#[derive(Clone)]
pub struct CatalogRecord {
	pub post: Post,
	pub created_ts: u64,
	pub activity_ts: u64,
	pub community_lc: String,
	pub author_lc: String,
	pub source_keys: HashSet<CatalogSourceKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
	pub post_id: String,
	pub created_ts: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogUserSidebarStats {
	pub subreddit_counts: Vec<(String, u32)>,
}

#[cfg(test)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedCatalogSnapshot {
	pub records: Vec<PersistedCatalogRecord>,
}

#[cfg(test)]
impl Default for PersistedCatalogSnapshot {
	fn default() -> Self {
		Self { records: Vec::new() }
	}
}

#[cfg(test)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedCatalogRecord {
	pub post: PersistedPost,
	pub created_ts: u64,
	pub activity_ts: u64,
	pub community_lc: String,
	pub author_lc: String,
	pub source_keys: Vec<PersistedCatalogSourceKey>,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum PersistedCatalogSourceKey {
	Subreddit(String),
	Flair { subreddit: String, flair: String },
	UserComments(String),
}

#[derive(Default)]
struct MaterializedCatalog {
	posts_by_id: HashMap<String, CatalogRecord>,
	timeline_new: Vec<TimelineEntry>,
	source_to_post_ids: HashMap<CatalogSourceKey, HashSet<String>>,
	author_to_post_ids: HashMap<String, HashSet<String>>,
	author_community_counts: HashMap<String, HashMap<String, usize>>,
}

static MATERIALIZED_CATALOG: LazyLock<Mutex<MaterializedCatalog>> = LazyLock::new(|| Mutex::new(MaterializedCatalog::default()));

fn normalize_catalog_source(source: &FeedSource) -> Option<CatalogSourceKey> {
	match source {
		FeedSource::Subreddit { name, sort } if *sort == FeedSort::New => {
			let name = name.trim().to_lowercase();
			(!name.is_empty()).then_some(CatalogSourceKey::Subreddit(name))
		}
		FeedSource::Flair { subreddit, flair, sort } if *sort == FeedSort::New => {
			let subreddit = subreddit.trim().to_lowercase();
			let flair = flair.trim().to_lowercase();
			(!subreddit.is_empty() && !flair.is_empty()).then_some(CatalogSourceKey::Flair { subreddit, flair })
		}
		FeedSource::UserComments { name, sort } if *sort == FeedSort::New => {
			let name = name.trim().to_lowercase();
			(!name.is_empty()).then_some(CatalogSourceKey::UserComments(name))
		}
		_ => None,
	}
}

fn catalog_record(post: &Post) -> CatalogRecord {
	CatalogRecord {
		post: post.clone(),
		created_ts: post.created_ts,
		activity_ts: crate::user::post_feed_activity_ts(post),
		community_lc: post.community.trim().to_lowercase(),
		author_lc: post.author.name.trim().to_lowercase(),
		source_keys: HashSet::new(),
	}
}

fn rebuild_indexes(catalog: &mut MaterializedCatalog) {
	catalog.timeline_new = catalog
		.posts_by_id
		.values()
		.map(|record| TimelineEntry {
			post_id: record.post.id.clone(),
			created_ts: record.created_ts,
		})
		.collect();
	catalog
		.timeline_new
		.sort_by(|left, right| right.created_ts.cmp(&left.created_ts).then_with(|| right.post_id.cmp(&left.post_id)));

	catalog.source_to_post_ids.clear();
	catalog.author_to_post_ids.clear();
	catalog.author_community_counts.clear();

	for (post_id, record) in &catalog.posts_by_id {
		for source_key in &record.source_keys {
			catalog.source_to_post_ids.entry(source_key.clone()).or_default().insert(post_id.clone());
		}

		if !record.author_lc.is_empty() {
			catalog.author_to_post_ids.entry(record.author_lc.clone()).or_default().insert(post_id.clone());
		}

		if !record.author_lc.is_empty() && !record.community_lc.is_empty() && !record.community_lc.starts_with("u_") {
			*catalog
				.author_community_counts
				.entry(record.author_lc.clone())
				.or_default()
				.entry(record.community_lc.clone())
				.or_default() += 1;
		}
	}
}

fn allowed_source_keys_for_session(session: &FeedSession) -> HashSet<CatalogSourceKey> {
	session
		.sources
		.iter()
		.filter(|entry| entry.state == SessionSourceState::Ready)
		.filter_map(|entry| normalize_catalog_source(&entry.source))
		.collect()
}

fn query_posts_for_keys_limited(catalog: &MaterializedCatalog, allowed_keys: &HashSet<CatalogSourceKey>, filters: &HashSet<String>, limit: usize) -> Vec<Post> {
	if allowed_keys.is_empty() {
		return Vec::new();
	}
	if limit == 0 {
		return Vec::new();
	}

	let mut posts = Vec::new();
	for entry in &catalog.timeline_new {
		let Some(record) = catalog.posts_by_id.get(&entry.post_id) else {
			continue;
		};
		if record.source_keys.is_disjoint(allowed_keys) {
			continue;
		}
		if filters.contains(&record.post.community) || filters.contains(&format!("u_{}", record.post.author.name)) {
			continue;
		}
		if crate::utils::is_deleted_reddit_placeholder_post(&record.post) {
			continue;
		}
		posts.push(record.post.clone());
		if posts.len() >= limit {
			break;
		}
	}
	posts
}

pub fn source_is_supported(source: &FeedSource) -> bool {
	normalize_catalog_source(source).is_some()
}

pub(crate) fn apply_source_refresh(source: &FeedSource, posts: &[Post]) {
	let Some(source_key) = normalize_catalog_source(source) else {
		return;
	};

	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let prior_post_ids = catalog.source_to_post_ids.get(&source_key).cloned().unwrap_or_default();
	let mut current_post_ids = HashSet::new();

	for post in posts {
		current_post_ids.insert(post.id.clone());
		let record = catalog.posts_by_id.entry(post.id.clone()).or_insert_with(|| catalog_record(post));
		record.post = post.clone();
		record.created_ts = post.created_ts;
		record.activity_ts = crate::user::post_feed_activity_ts(post);
		record.community_lc = post.community.trim().to_lowercase();
		record.author_lc = post.author.name.trim().to_lowercase();
		record.source_keys.insert(source_key.clone());
	}

	for post_id in prior_post_ids.difference(&current_post_ids) {
		let should_remove = if let Some(record) = catalog.posts_by_id.get_mut(post_id) {
			record.source_keys.remove(&source_key);
			record.source_keys.is_empty()
		} else {
			false
		};
		if should_remove {
			catalog.posts_by_id.remove(post_id);
		}
	}

	catalog.source_to_post_ids.insert(source_key, current_post_ids);
	rebuild_indexes(&mut catalog);
}

#[cfg(test)]
pub(crate) fn restore_catalog(snapshot: PersistedCatalogSnapshot) {
	let mut catalog = MaterializedCatalog::default();
	for record in snapshot.records {
		let source_keys = record.source_keys.into_iter().map(PersistedCatalogSourceKey::into_runtime).collect::<HashSet<_>>();
		if source_keys.is_empty() {
			continue;
		}

		let post = record.post.into_runtime();
		catalog.posts_by_id.insert(
			post.id.clone(),
			CatalogRecord {
				post,
				created_ts: record.created_ts,
				activity_ts: record.activity_ts,
				community_lc: record.community_lc,
				author_lc: record.author_lc,
				source_keys,
			},
		);
	}
	rebuild_indexes(&mut catalog);

	let mut state = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	*state = catalog;
}

#[cfg(test)]
pub(crate) fn snapshot_catalog() -> PersistedCatalogSnapshot {
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let mut records = catalog
		.posts_by_id
		.values()
		.map(|record| PersistedCatalogRecord {
			post: PersistedPost::from_runtime(&record.post),
			created_ts: record.created_ts,
			activity_ts: record.activity_ts,
			community_lc: record.community_lc.clone(),
			author_lc: record.author_lc.clone(),
			source_keys: record.source_keys.iter().cloned().map(PersistedCatalogSourceKey::from_runtime).collect(),
		})
		.collect::<Vec<_>>();
	records.sort_by(|left, right| left.post.id.cmp(&right.post.id));
	PersistedCatalogSnapshot { records }
}

#[cfg(test)]
pub(crate) fn query_source_candidates(sources: &[FeedSource], filters: &HashSet<String>) -> Vec<Post> {
	let allowed_keys = sources.iter().filter_map(normalize_catalog_source).collect::<HashSet<_>>();
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	query_posts_for_keys_limited(&catalog, &allowed_keys, filters, usize::MAX)
}

pub(crate) fn query_session_stream_limited(session: &FeedSession, limit: usize) -> Vec<Post> {
	let allowed_keys = allowed_source_keys_for_session(session);
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	query_posts_for_keys_limited(&catalog, &allowed_keys, &session.request.filters, limit)
}

pub(crate) fn query_source_candidates_limited(sources: &[FeedSource], filters: &HashSet<String>, limit: usize) -> Vec<Post> {
	let allowed_keys = sources.iter().filter_map(normalize_catalog_source).collect::<HashSet<_>>();
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	query_posts_for_keys_limited(&catalog, &allowed_keys, filters, limit)
}

pub(crate) fn catalog_user_sidebar_stats(username: &str) -> Option<CatalogUserSidebarStats> {
	let username = username.trim().to_lowercase();
	if username.is_empty() {
		return None;
	}

	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let counts = catalog.author_community_counts.get(&username)?;
	let mut subreddit_counts = counts
		.iter()
		.filter(|(subreddit, _)| !subreddit.is_empty() && !subreddit.starts_with("u_"))
		.map(|(subreddit, count)| (subreddit.clone(), (*count).min(u32::MAX as usize) as u32))
		.collect::<Vec<_>>();
	subreddit_counts.sort_by(|left, right| {
		right
			.1
			.cmp(&left.1)
			.then_with(|| left.0.to_lowercase().cmp(&right.0.to_lowercase()))
			.then_with(|| left.0.cmp(&right.0))
	});
	(!subreddit_counts.is_empty()).then_some(CatalogUserSidebarStats { subreddit_counts })
}

pub(crate) fn catalog_avg_video_length(username: &str) -> Option<(String, String)> {
	let username = username.trim().to_lowercase();
	if username.is_empty() {
		return None;
	}

	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let post_ids = catalog.author_to_post_ids.get(&username)?;

	let mut seen_keys: HashSet<String> = HashSet::new();
	let mut seen_redgifs_users: HashSet<String> = HashSet::new();
	let mut total_duration_seconds: i64 = 0;
	let mut video_count: usize = 0;

	for post_id in post_ids {
		let Some(record) = catalog.posts_by_id.get(post_id) else {
			continue;
		};
		let post = &record.post;

		if matches!(post.post_type.as_str(), "video" | "gif") && !post.media.redgifs_username.is_empty() {
			let slug = crate::utils::normalize_creator_identity_slug(&post.media.redgifs_username);
			if !slug.is_empty() {
				seen_redgifs_users.insert(slug);
			}
		}

		let Some(video_key) = crate::user::video_aggregate_key(post) else {
			continue;
		};
		if seen_keys.insert(video_key) {
			video_count += 1;
			total_duration_seconds += post.media.duration_seconds;
		}
	}

	if video_count == 0 {
		return None;
	}

	let average = total_duration_seconds / video_count as i64;
	let count_label = if video_count == 1 { "video" } else { "videos" };
	let mut meta = format!("from {} seen {count_label}", video_count);
	let rg_profiles = seen_redgifs_users.len();
	if rg_profiles > 1 {
		meta.push_str(&format!(" · {rg_profiles} rg profiles"));
	}

	Some((crate::utils::format_video_duration(average), meta))
}

pub(crate) fn reset_catalog() {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	*catalog = MaterializedCatalog::default();
}

#[cfg(test)]
pub(crate) static MATERIALIZED_CATALOG_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[cfg(test)]
impl PersistedCatalogSourceKey {
	fn from_runtime(value: CatalogSourceKey) -> Self {
		match value {
			CatalogSourceKey::Subreddit(name) => Self::Subreddit(name),
			CatalogSourceKey::Flair { subreddit, flair } => Self::Flair { subreddit, flair },
			CatalogSourceKey::UserComments(name) => Self::UserComments(name),
		}
	}

	fn into_runtime(self) -> CatalogSourceKey {
		match self {
			Self::Subreddit(name) => CatalogSourceKey::Subreddit(name),
			Self::Flair { subreddit, flair } => CatalogSourceKey::Flair { subreddit, flair },
			Self::UserComments(name) => CatalogSourceKey::UserComments(name),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{apply_source_refresh, catalog_user_sidebar_stats, query_source_candidates, reset_catalog, restore_catalog, snapshot_catalog};
	use crate::post_cache::{FeedSort, FeedSource};
	use crate::utils::{Author, Awards, Flags, Flair, Media, ParentComment, Post};
	use std::collections::HashSet;
	use std::sync::MutexGuard;

	fn test_lock() -> MutexGuard<'static, ()> {
		super::MATERIALIZED_CATALOG_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
	}

	fn empty_flair() -> Flair {
		Flair {
			flair_parts: Vec::new(),
			text: String::new(),
			background_color: String::new(),
			foreground_color: String::new(),
		}
	}

	fn test_post(id: &str, subreddit: &str, author: &str, created_ts: u64) -> Post {
		Post {
			id: id.to_string(),
			kind: "t3".to_string(),
			title: format!("Post {id}"),
			community: subreddit.to_string(),
			body: String::new(),
			author: Author {
				name: author.to_string(),
				flair: empty_flair(),
				distinguished: String::new(),
				external_links: Vec::new(),
				avg_video_length: None,
			},
			permalink: format!("/r/{subreddit}/comments/{id}/"),
			parent_id: String::new(),
			parent_kind: String::new(),
			parent_comment: ParentComment::default(),
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
			domain: "self.test".to_string(),
			rel_time: "1h".to_string(),
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

	#[test]
	fn source_membership_retention_requires_last_source_removal() {
		let _guard = test_lock();
		reset_catalog();
		let post = test_post("one", "rust", "alice", 100);
		let subreddit_source = FeedSource::subreddit("rust", FeedSort::New);
		let flair_source = FeedSource::flair("rust", "News", FeedSort::New);

		apply_source_refresh(&subreddit_source, std::slice::from_ref(&post));
		apply_source_refresh(&flair_source, std::slice::from_ref(&post));
		apply_source_refresh(&flair_source, &[]);

		let posts = query_source_candidates(&[subreddit_source], &HashSet::new());
		assert_eq!(posts.into_iter().map(|post| post.id).collect::<Vec<_>>(), vec!["one".to_string()]);
	}

	#[test]
	fn user_comment_membership_is_independent() {
		let _guard = test_lock();
		reset_catalog();
		let post = test_post("comment-post", "rust", "alice", 100);
		let submitted = FeedSource::subreddit("u_alice", FeedSort::New);
		let comments = FeedSource::user_comments("u_alice", FeedSort::New);

		apply_source_refresh(&submitted, std::slice::from_ref(&post));
		apply_source_refresh(&comments, std::slice::from_ref(&post));
		apply_source_refresh(&submitted, &[]);

		let posts = query_source_candidates(&[comments], &HashSet::new());
		assert_eq!(posts.into_iter().map(|post| post.id).collect::<Vec<_>>(), vec!["comment-post".to_string()]);
	}

	#[test]
	fn snapshot_round_trip_preserves_sidebar_stats() {
		let _guard = test_lock();
		reset_catalog();
		let posts = vec![test_post("one", "rust", "alice", 100), test_post("two", "programming", "alice", 90)];
		apply_source_refresh(&FeedSource::subreddit("u_alice", FeedSort::New), &posts);

		let snapshot = snapshot_catalog();
		reset_catalog();
		restore_catalog(snapshot);

		let stats = catalog_user_sidebar_stats("alice").expect("sidebar stats");
		assert_eq!(stats.subreddit_counts, vec![("programming".to_string(), 1), ("rust".to_string(), 1)]);
	}
}
