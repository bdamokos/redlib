use crate::cache_persistence::PersistedPost;
use crate::post_cache::{deterministic_random_post_sort_key_with_seed, session_random_rotation_seed, FeedSession, FeedSort, FeedSource, SessionSourceState};
use crate::subreddit_content_semantics;
use crate::subreddit_semantics;
use crate::utils::{post_feedback_score_adjustment, post_feedback_summary_affinity, Post, PostFeedbackSummary, POST_FEEDBACK_MODEL_MANUAL};
use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

const SUGGESTED_PROFILE_TOP_SUBREDDITS: usize = 16;
const SUGGESTED_PROFILE_VECTOR_SUBREDDITS: usize = 32;
const SUGGESTED_PROFILE_REASON_LIMIT: usize = 5;
const SUGGESTED_PROFILE_VECTOR_CENTER_SUBREDDIT_LIMIT: usize = 12;
const SUGGESTED_PROFILE_MIN_POSTS: usize = 2;
const SUGGESTED_PROFILE_SUBREDDIT_USER_LIMIT: usize = 120;
const SUGGESTED_PROFILE_MEDIA_LIMIT: usize = 3;
const SUGGESTED_PROFILE_SELF_SUBREDDIT_FACTOR: f64 = 0.30;
const SUGGESTED_PROFILE_SINGLE_SOURCE_FACTOR: f64 = 0.55;
const SUGGESTED_PROFILE_TWO_SOURCE_FACTOR: f64 = 0.75;
const SUGGESTED_PROFILE_SINGLE_OVERLAP_FACTOR: f64 = 0.60;
const SUGGESTED_PROFILE_TWO_OVERLAP_FACTOR: f64 = 0.85;
const SUGGESTED_PROFILE_VECTOR_SINGLE_OVERLAP_FACTOR: f64 = 0.45;
const SUGGESTED_PROFILE_VECTOR_TWO_OVERLAP_FACTOR: f64 = 0.80;
const SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD: usize = 4;
const SUGGESTED_PROFILE_FEEDBACK_MAX_SCORE_ADJUSTMENT: f64 = 0.25;
const RELATED_SUBREDDIT_MIN_SIMILARITY: f64 = 0.15;
const RELATED_PROFILE_MIN_SIMILARITY: f64 = 0.15;
const AUTHOR_HISTORY_MAX_POST_AGE_SECS: u64 = 180 * 24 * 60 * 60;
const AUTHOR_HISTORY_MAX_POSTS_PER_AUTHOR: usize = 160;
const AUTHOR_HISTORY_GLOBAL_MAX_POSTS: usize = 60_000;
/// Keep a small, fair slice of every persisted feed in memory for aggregate front pages.
/// This is intentionally much smaller than the full per-source cache window.
const MIXED_FEED_POSTS_PER_SOURCE: usize = 8;
/// Soft global bound. The newest post for each source is protected so even very large
/// subscription sets retain broad source coverage.
const MIXED_FEED_MAX_POSTS: usize = 4_096;
// Keep the complete practical followed-profile history independently of the compact
// homepage catalog. This is intentionally aligned with the discovery index bound so
// deep `/following` pages can reach every cached post up to the durable safety cap.
const FOLLOWING_FEED_MAX_POSTS: usize = 100_000;
const DAEMON_DISCOVERY_MAX_POSTS: usize = 100_000;
pub const SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT: u8 = 50;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SuggestedProfileScoringMode {
	#[default]
	Overlap,
	Vector,
	Semantic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuggestedProfileVectorCenterRequest {
	Cluster(usize),
	Subreddit(String),
}

impl SuggestedProfileVectorCenterRequest {
	pub fn parse(value: &str) -> Option<Self> {
		let value = value.trim();
		if value.is_empty() {
			return None;
		}

		if let Some(index) = value.strip_prefix("cluster:").and_then(|index| index.trim().parse::<usize>().ok()) {
			return if index > 0 { Some(Self::Cluster(index - 1)) } else { None };
		}

		let subreddit = value.strip_prefix("subreddit:").or_else(|| value.strip_prefix("r:")).unwrap_or(value);
		let subreddit = normalize_vector_center_subreddit_name(subreddit);
		(!subreddit.is_empty()).then_some(Self::Subreddit(subreddit))
	}

	pub fn id(&self) -> String {
		match self {
			Self::Cluster(index) => format!("cluster:{}", index + 1),
			Self::Subreddit(subreddit) => format!("subreddit:{subreddit}"),
		}
	}
}

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
/// Kept under its original serialized name for snapshot compatibility. Entries now cover
/// every daemon-managed aggregate-feed source, not only followed profiles.
pub(crate) struct PersistedFollowingDiscoveryEntry {
	pub post_id: String,
	pub discovered_at_unix: u64,
	pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FollowingDiscoveryStamp {
	discovered_at_unix: u64,
	sequence: u64,
}

struct FollowingDiscoveryState {
	posts: HashMap<String, FollowingDiscoveryStamp>,
	next_sequence: u64,
	bootstrap_at_unix: Option<u64>,
}

impl Default for FollowingDiscoveryState {
	fn default() -> Self {
		Self {
			posts: HashMap::new(),
			next_sequence: 0,
			bootstrap_at_unix: Some(crate::cache_persistence::unix_now()),
		}
	}
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

#[derive(Clone, Debug, PartialEq)]
pub struct SuggestedProfileSubreddit {
	pub name: String,
	pub followed_count: usize,
	pub followed_profile_count: usize,
	pub profile_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SuggestedProfileEntry {
	pub name: String,
	pub score_percent: u32,
	pub cached_posts: usize,
	pub matched_posts: usize,
	pub avg_video_length: Option<(String, String)>,
	pub video_count: usize,
	pub redgifs_profile_count: usize,
	pub media_posts: Vec<SuggestedProfileMediaPost>,
	pub overlap_subreddits: Vec<SuggestedProfileSubreddit>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuggestedProfileVectorCenter {
	pub id: String,
	pub label: String,
	pub detail: String,
	pub selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedProfileMediaPost {
	pub title: String,
	pub permalink: String,
	pub subreddit: String,
	pub image_url: String,
	pub media_type: String,
	pub duration: String,
	pub nsfw: bool,
	pub spoiler: bool,
	pub created_ts: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RelatedSubredditReport {
	pub semantic_embedding_count: usize,
	pub vibe_embedding_count: usize,
	pub followed_subreddits: Vec<RelatedSubredditEntry>,
	pub cached_subreddits: Vec<RelatedSubredditEntry>,
	pub vibe_followed_subreddits: Vec<RelatedSubredditEntry>,
	pub vibe_cached_subreddits: Vec<RelatedSubredditEntry>,
	pub followed_profiles: Vec<RelatedProfileEntry>,
	pub cached_profiles: Vec<RelatedProfileEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RelatedSubredditEntry {
	pub name: String,
	pub score_percent: u32,
	pub cached_profile_count: usize,
	pub cached_post_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RelatedProfileEntry {
	pub name: String,
	pub score_percent: u32,
	pub cached_posts: usize,
	pub current_subreddit_posts: usize,
	pub signal_subreddits: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedAuthorHistoryPost {
	pub post_id: String,
	pub subreddit: String,
	pub created_ts: u64,
	pub seen_at_unix: u64,
	pub media_key: String,
	pub media_post: Option<SuggestedProfileMediaPost>,
	pub video_key: Option<String>,
	pub video_duration_seconds: i64,
	pub redgifs_username: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedAuthorHistoryEntry {
	pub author: String,
	pub posts: Vec<PersistedAuthorHistoryPost>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedRejectedAuthorRedgifsProfileEntry {
	pub author: String,
	pub redgifs_profile_count: usize,
	pub seen_at_unix: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SuggestedProfilesReport {
	pub followed_profile_count: usize,
	pub followed_profiles_with_cache: usize,
	pub cached_profile_count: usize,
	pub compared_profile_count: usize,
	pub taste_centroid_count: usize,
	pub active_vector_center_label: Option<String>,
	pub semantic_embedding_count: usize,
	pub semantic_mix_percent: u8,
	pub vector_cluster_centers: Vec<SuggestedProfileVectorCenter>,
	pub vector_subreddit_centers: Vec<SuggestedProfileVectorCenter>,
	pub followed_subreddits: Vec<SuggestedProfileSubreddit>,
	pub suggestions: Vec<SuggestedProfileEntry>,
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

#[derive(Clone, Debug)]
pub(crate) struct CatalogPageMetricCandidate {
	pub created_ts: u64,
	pub score: Option<i64>,
	pub stickied: bool,
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
	author_video_stats: HashMap<String, crate::user::UserVideoStatsAccumulator>,
	author_media_posts: HashMap<String, Vec<SuggestedProfileMediaPost>>,
	author_history: HashMap<String, HashMap<String, PersistedAuthorHistoryPost>>,
	rejected_author_redgifs_profiles: HashMap<String, PersistedRejectedAuthorRedgifsProfileEntry>,
}

static MATERIALIZED_CATALOG: LazyLock<Mutex<MaterializedCatalog>> = LazyLock::new(|| Mutex::new(MaterializedCatalog::default()));

#[derive(Clone)]
struct MixedFeedRecord {
	post: Post,
	source_keys: HashSet<CatalogSourceKey>,
}

#[derive(Default)]
struct MixedFeedCatalog {
	posts_by_id: HashMap<String, MixedFeedRecord>,
	source_to_post_ids: HashMap<CatalogSourceKey, HashSet<String>>,
}

static MIXED_FEED_CATALOG: LazyLock<Mutex<MixedFeedCatalog>> = LazyLock::new(|| Mutex::new(MixedFeedCatalog::default()));
static FOLLOWING_FEED_CATALOG: LazyLock<Mutex<MixedFeedCatalog>> = LazyLock::new(|| Mutex::new(MixedFeedCatalog::default()));
static DAEMON_DISCOVERY: LazyLock<Mutex<FollowingDiscoveryState>> = LazyLock::new(|| Mutex::new(FollowingDiscoveryState::default()));

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

fn is_followed_user_source(source: &FeedSource) -> bool {
	matches!(source, FeedSource::Subreddit { name, sort: FeedSort::New } if name.to_lowercase().starts_with("u_"))
		|| matches!(source, FeedSource::UserComments { name, sort: FeedSort::New } if name.to_lowercase().starts_with("u_"))
}

fn insert_following_discovery_if_missing(state: &mut FollowingDiscoveryState, post_id: &str, discovered_at_unix: u64) {
	if post_id.is_empty() || state.posts.contains_key(post_id) {
		return;
	}
	state.next_sequence = state.next_sequence.saturating_add(1);
	state.posts.insert(
		post_id.to_string(),
		FollowingDiscoveryStamp {
			discovered_at_unix,
			sequence: state.next_sequence,
		},
	);
}

pub(crate) fn record_daemon_discoveries(source: &FeedSource, posts: &[Post]) {
	if normalize_catalog_source(source).is_none() || posts.is_empty() {
		return;
	}
	let now_unix = crate::cache_persistence::unix_now();
	let mut state = DAEMON_DISCOVERY.lock().unwrap_or_else(|err| err.into_inner());
	for post in posts.iter().rev() {
		insert_following_discovery_if_missing(&mut state, &post.id, now_unix);
	}
}

pub(crate) fn mark_posts_seen_before_daemon(source: &FeedSource, posts: &[Post]) {
	if normalize_catalog_source(source).is_none() || posts.is_empty() {
		return;
	}
	let mut state = DAEMON_DISCOVERY.lock().unwrap_or_else(|err| err.into_inner());
	for post in posts.iter().rev() {
		insert_following_discovery_if_missing(&mut state, &post.id, 0);
	}
}

pub(crate) fn seed_following_discovery_baseline(source: &FeedSource, posts: &[PersistedPost], fetched_at_unix: u64) {
	if normalize_catalog_source(source).is_none() || posts.is_empty() {
		return;
	}
	let mut ordered = posts.iter().collect::<Vec<_>>();
	ordered.sort_by(|left, right| left.created_ts.cmp(&right.created_ts).then_with(|| left.id.cmp(&right.id)));
	let mut state = DAEMON_DISCOVERY.lock().unwrap_or_else(|err| err.into_inner());
	let baseline_unix = state.bootstrap_at_unix.unwrap_or(fetched_at_unix);
	for post in ordered {
		insert_following_discovery_if_missing(&mut state, &post.id, baseline_unix);
	}
}

pub(crate) fn restore_following_discoveries(entries: Vec<PersistedFollowingDiscoveryEntry>) {
	let mut state = FollowingDiscoveryState::default();
	for entry in entries {
		state.next_sequence = state.next_sequence.max(entry.sequence);
		state.posts.insert(
			entry.post_id,
			FollowingDiscoveryStamp {
				discovered_at_unix: entry.discovered_at_unix,
				sequence: entry.sequence,
			},
		);
	}
	// A release that broadens discovery tracking must give all legacy source windows one
	// deployment baseline instead of inventing an order from their unrelated fetch times.
	// Already-restored entries are insert-only and retain their original stamps.
	*DAEMON_DISCOVERY.lock().unwrap_or_else(|err| err.into_inner()) = state;
}

pub(crate) fn snapshot_following_discoveries() -> Vec<PersistedFollowingDiscoveryEntry> {
	let state = DAEMON_DISCOVERY.lock().unwrap_or_else(|err| err.into_inner());
	let mut entries = state
		.posts
		.iter()
		.map(|(post_id, stamp)| PersistedFollowingDiscoveryEntry {
			post_id: post_id.clone(),
			discovered_at_unix: stamp.discovered_at_unix,
			sequence: stamp.sequence,
		})
		.collect::<Vec<_>>();
	entries.sort_by(|left, right| right.discovered_at_unix.cmp(&left.discovered_at_unix).then_with(|| right.sequence.cmp(&left.sequence)));
	entries.truncate(DAEMON_DISCOVERY_MAX_POSTS);
	entries
}

pub(crate) fn sort_posts_by_daemon_discovery(posts: &mut [Post]) {
	let state = DAEMON_DISCOVERY.lock().unwrap_or_else(|err| err.into_inner());
	let rank = |post: &Post| {
		std::iter::once(post.id.as_str())
			.chain(post.following_comments.iter().map(|comment| comment.id.as_str()))
			.filter_map(|post_id| state.posts.get(post_id))
			.map(|stamp| (stamp.discovered_at_unix, stamp.sequence))
			.max()
			.unwrap_or_default()
	};
	posts.sort_by(|left, right| {
		rank(right)
			.cmp(&rank(left))
			.then_with(|| crate::user::post_feed_activity_ts(right).cmp(&crate::user::post_feed_activity_ts(left)))
			.then_with(|| right.id.cmp(&left.id))
	});
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

fn mixed_feed_selected_posts(posts: &[Post]) -> Vec<Post> {
	let mut selected = posts.iter().collect::<Vec<_>>();
	selected.sort_by(|left, right| right.created_ts.cmp(&left.created_ts).then_with(|| right.id.cmp(&left.id)));
	selected.truncate(MIXED_FEED_POSTS_PER_SOURCE);
	selected.into_iter().cloned().collect()
}

fn remove_mixed_feed_post(catalog: &mut MixedFeedCatalog, post_id: &str) {
	let Some(record) = catalog.posts_by_id.remove(post_id) else {
		return;
	};
	for source_key in record.source_keys {
		if let Some(post_ids) = catalog.source_to_post_ids.get_mut(&source_key) {
			post_ids.remove(post_id);
		}
	}
}

fn prune_mixed_feed_catalog(catalog: &mut MixedFeedCatalog) {
	prune_feed_catalog(catalog, MIXED_FEED_MAX_POSTS);
}

fn prune_following_feed_catalog(catalog: &mut MixedFeedCatalog) {
	prune_feed_catalog(catalog, FOLLOWING_FEED_MAX_POSTS);
}

fn prune_feed_catalog(catalog: &mut MixedFeedCatalog, max_posts: usize) {
	if catalog.posts_by_id.len() <= max_posts {
		return;
	}

	let protected = catalog
		.source_to_post_ids
		.values()
		.filter_map(|post_ids| {
			post_ids
				.iter()
				.filter_map(|post_id| catalog.posts_by_id.get(post_id).map(|record| (record.post.created_ts, post_id.clone())))
				.max()
				.map(|(_, post_id)| post_id)
		})
		.collect::<HashSet<_>>();
	let mut removable = catalog
		.posts_by_id
		.iter()
		.filter(|(post_id, _)| !protected.contains(*post_id))
		.map(|(post_id, record)| (record.post.created_ts, post_id.clone()))
		.collect::<Vec<_>>();
	removable.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

	let remove_count = catalog.posts_by_id.len().saturating_sub(max_posts).min(removable.len());
	for (_, post_id) in removable.into_iter().take(remove_count) {
		remove_mixed_feed_post(catalog, &post_id);
	}
	catalog.source_to_post_ids.retain(|_, post_ids| !post_ids.is_empty());
}

/// Update the compact aggregate-feed index for one source. Unlike the full materialized
/// catalog, this index survives resident source eviction because the source payload remains
/// available on disk.
pub(crate) fn apply_mixed_feed_source_refresh(source: &FeedSource, posts: &[Post]) {
	apply_following_feed_source_refresh(source, posts);
	apply_compact_mixed_feed_source_refresh(source, posts);
}

fn apply_compact_mixed_feed_source_refresh(source: &FeedSource, posts: &[Post]) {
	let Some(source_key) = normalize_catalog_source(source) else {
		return;
	};
	let selected = mixed_feed_selected_posts(posts);
	let current_post_ids = selected.iter().map(|post| post.id.clone()).collect::<HashSet<_>>();

	let mut catalog = MIXED_FEED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let prior_post_ids = catalog.source_to_post_ids.get(&source_key).cloned().unwrap_or_default();
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

	for post in selected {
		let record = catalog.posts_by_id.entry(post.id.clone()).or_insert_with(|| MixedFeedRecord {
			post: post.clone(),
			source_keys: HashSet::new(),
		});
		record.post = post;
		record.source_keys.insert(source_key.clone());
	}
	catalog.source_to_post_ids.insert(source_key, current_post_ids);
	prune_mixed_feed_catalog(&mut catalog);
}

fn apply_following_feed_source_refresh(source: &FeedSource, posts: &[Post]) {
	if !is_followed_user_source(source) {
		return;
	}
	let Some(source_key) = normalize_catalog_source(source) else {
		return;
	};
	let current_post_ids = posts.iter().map(|post| post.id.clone()).collect::<HashSet<_>>();
	let mut catalog = FOLLOWING_FEED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let prior_post_ids = catalog.source_to_post_ids.get(&source_key).cloned().unwrap_or_default();
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
	for post in posts {
		let record = catalog.posts_by_id.entry(post.id.clone()).or_insert_with(|| MixedFeedRecord {
			post: post.clone(),
			source_keys: HashSet::new(),
		});
		record.post = post.clone();
		record.source_keys.insert(source_key.clone());
	}
	catalog.source_to_post_ids.insert(source_key, current_post_ids);
	prune_following_feed_catalog(&mut catalog);
}

pub(crate) fn apply_mixed_feed_persisted_source_refresh(source: &FeedSource, posts: &[PersistedPost]) {
	if is_followed_user_source(source) {
		let following_posts = posts.iter().cloned().map(PersistedPost::into_runtime).collect::<Vec<_>>();
		apply_following_feed_source_refresh(source, &following_posts);
	}
	let mut selected = posts.iter().collect::<Vec<_>>();
	selected.sort_by(|left, right| right.created_ts.cmp(&left.created_ts).then_with(|| right.id.cmp(&left.id)));
	let selected = selected
		.into_iter()
		.take(MIXED_FEED_POSTS_PER_SOURCE)
		.cloned()
		.map(PersistedPost::into_runtime)
		.collect::<Vec<_>>();
	apply_compact_mixed_feed_source_refresh(source, &selected);
}

pub(crate) fn mixed_feed_source_is_cached(source: &FeedSource) -> bool {
	let Some(source_key) = normalize_catalog_source(source) else {
		return false;
	};
	MIXED_FEED_CATALOG
		.lock()
		.unwrap_or_else(|err| err.into_inner())
		.source_to_post_ids
		.get(&source_key)
		.is_some_and(|post_ids| !post_ids.is_empty())
}

fn sorted_subreddit_counts(counts: &HashMap<String, usize>, limit: usize) -> Vec<(String, usize)> {
	let mut entries = counts
		.iter()
		.filter(|(subreddit, count)| !subreddit.is_empty() && !subreddit.starts_with("u_") && **count > 0)
		.map(|(subreddit, count)| (subreddit.clone(), *count))
		.collect::<Vec<_>>();
	entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
	entries.truncate(limit);
	entries
}

fn suggested_profile_weight(count: usize) -> f64 {
	(count as f64).sqrt()
}

fn suggested_profile_source_factor(subreddit: &str, followed_profiles: &HashSet<String>, followed_profile_identities: &HashSet<String>, followed_profile_count: usize) -> f64 {
	let subreddit_identity = crate::utils::creator_identity_fingerprint(subreddit);
	let is_followed_self_subreddit = followed_profiles.contains(subreddit) || (!subreddit_identity.is_empty() && followed_profile_identities.contains(&subreddit_identity));
	if is_followed_self_subreddit && followed_profile_count <= 1 {
		SUGGESTED_PROFILE_SELF_SUBREDDIT_FACTOR
	} else if followed_profile_count <= 1 {
		SUGGESTED_PROFILE_SINGLE_SOURCE_FACTOR
	} else if followed_profile_count == 2 {
		SUGGESTED_PROFILE_TWO_SOURCE_FACTOR
	} else {
		1.0
	}
}

fn suggested_profile_overlap_factor(overlap_count: usize) -> f64 {
	match overlap_count {
		0 => 0.0,
		1 => SUGGESTED_PROFILE_SINGLE_OVERLAP_FACTOR,
		2 => SUGGESTED_PROFILE_TWO_OVERLAP_FACTOR,
		_ => 1.0,
	}
}

fn suggested_profile_vector_breadth_factor(overlap_count: usize) -> f64 {
	match overlap_count {
		0 => 0.0,
		1 => SUGGESTED_PROFILE_VECTOR_SINGLE_OVERLAP_FACTOR,
		2 => SUGGESTED_PROFILE_VECTOR_TWO_OVERLAP_FACTOR,
		_ => 1.0,
	}
}

fn suggested_profile_vector_count_weight(count: usize) -> f64 {
	((count as f64) + 1.0).ln()
}

fn suggested_profile_vector_idf(total_profiles: usize, subreddit_profile_count: usize) -> f64 {
	let total = total_profiles.max(1) as f64;
	let subreddit_profiles = subreddit_profile_count.max(1) as f64;
	(((total + 1.0) / (subreddit_profiles + 1.0)).ln() + 0.35).clamp(0.20, 2.75)
}

fn manual_post_feedback_summary<'a>(feedback: &'a HashMap<(String, String), PostFeedbackSummary>, name: &str) -> Option<&'a PostFeedbackSummary> {
	feedback.get(&(POST_FEEDBACK_MODEL_MANUAL.to_string(), name.trim().to_lowercase()))
}

fn suggested_profile_feedback_adjusted_score(score: f64, summary: Option<&PostFeedbackSummary>) -> f64 {
	(score + post_feedback_score_adjustment(summary, SUGGESTED_PROFILE_FEEDBACK_MAX_SCORE_ADJUSTMENT)).clamp(0.01, 1.0)
}

fn suggested_profile_promotion_quality_factor(counts: &HashMap<String, usize>) -> f64 {
	let mut weighted_quality = 0.0;
	let mut total = 0usize;
	for (subreddit, count) in counts {
		if *count == 0 {
			continue;
		}
		weighted_quality += crate::promo_detection::promotion_quality_factor(subreddit) * *count as f64;
		total = total.saturating_add(*count);
	}
	if total == 0 {
		1.0
	} else {
		weighted_quality / total as f64
	}
}

fn suggested_profile_has_positive_feedback(summary: Option<&PostFeedbackSummary>) -> bool {
	post_feedback_summary_affinity(summary) > 0.0
}

fn normalize_suggested_profile_vector(vector: &mut HashMap<String, f64>) -> f64 {
	let norm = vector.values().map(|weight| weight * weight).sum::<f64>().sqrt();
	if norm > 0.0 {
		for weight in vector.values_mut() {
			*weight /= norm;
		}
	}
	norm
}

fn suggested_profile_vector(counts: &HashMap<String, usize>, subreddit_weights: &HashMap<String, f64>, limit: usize) -> HashMap<String, f64> {
	let mut vector = HashMap::new();
	for (subreddit, count) in sorted_subreddit_counts(counts, limit) {
		let Some(subreddit_weight) = subreddit_weights.get(&subreddit) else {
			continue;
		};
		let weight = suggested_profile_vector_count_weight(count) * subreddit_weight;
		if weight > 0.0 {
			vector.insert(subreddit, weight);
		}
	}
	vector
}

fn suggested_profile_dot(left: &HashMap<String, f64>, right: &HashMap<String, f64>) -> f64 {
	let (small, large) = if left.len() <= right.len() { (left, right) } else { (right, left) };
	small
		.iter()
		.filter_map(|(key, left_weight)| large.get(key).map(|right_weight| left_weight * right_weight))
		.sum()
}

fn suggested_profile_dense_dot(left: &[f64], right: &[f64]) -> f64 {
	if left.len() != right.len() {
		return 0.0;
	}
	left.iter().zip(right).map(|(left, right)| left * right).sum()
}

fn normalize_f64_vector(vector: &mut [f64]) -> f64 {
	let norm = vector.iter().map(|weight| weight * weight).sum::<f64>().sqrt();
	if norm > 0.0 {
		for weight in vector {
			*weight /= norm;
		}
	}
	norm
}

fn normalized_f32_embedding(embedding: &[f32]) -> Option<Vec<f64>> {
	let mut vector = embedding.iter().map(|value| *value as f64).collect::<Vec<_>>();
	(normalize_f64_vector(&mut vector) > 0.0).then_some(vector)
}

fn embedding_cosine(left: &[f32], right: &[f32]) -> f64 {
	let Some(left) = normalized_f32_embedding(left) else {
		return 0.0;
	};
	let Some(right) = normalized_f32_embedding(right) else {
		return 0.0;
	};
	suggested_profile_dense_dot(&left, &right)
}

fn normalize_f32_vector(vector: &mut [f32]) -> bool {
	let norm = vector.iter().map(|value| (*value as f64) * (*value as f64)).sum::<f64>().sqrt();
	if norm <= 0.0 {
		return false;
	}
	for value in vector {
		*value = (*value as f64 / norm) as f32;
	}
	true
}

fn blended_subreddit_semantic_embedding(subreddit: &str, model: &str) -> Option<Vec<f32>> {
	let metadata = subreddit_semantics::embedding_for_subreddit_model(subreddit, model);
	let content = subreddit_content_semantics::embedding_for_subreddit_model(subreddit, model);
	let content_mix = subreddit_content_semantics::content_mix_percent();

	match (metadata, content) {
		(Some(_metadata), Some(content)) if content_mix >= 100 => Some(content),
		(Some(metadata), Some(content)) if metadata.len() == content.len() && content_mix > 0 => {
			let mix = (content_mix as f64) / 100.0;
			let mut blended = metadata
				.iter()
				.zip(content.iter())
				.map(|(metadata, content)| ((*metadata as f64 * (1.0 - mix)) + (*content as f64 * mix)) as f32)
				.collect::<Vec<_>>();
			normalize_f32_vector(&mut blended).then_some(blended)
		}
		(Some(metadata), _) => Some(metadata),
		(None, Some(content)) if content_mix > 0 => Some(content),
		_ => None,
	}
}

fn active_semantic_embedding_count() -> usize {
	subreddit_semantics::active_embedding_count().max(subreddit_content_semantics::active_usable_embedding_count())
}

fn normalize_suggested_profile_dense_vector(vector: &mut [f64]) -> f64 {
	let norm = vector.iter().map(|weight| weight * weight).sum::<f64>().sqrt();
	if norm > 0.0 {
		for weight in vector {
			*weight /= norm;
		}
	}
	norm
}

fn suggested_profile_semantic_vector(
	counts: &HashMap<String, usize>,
	subreddit_weights: &HashMap<String, f64>,
	embeddings: &HashMap<String, Vec<f32>>,
	limit: usize,
) -> Option<Vec<f64>> {
	let mut vector = Vec::<f64>::new();
	for (subreddit, count) in sorted_subreddit_counts(counts, limit) {
		let Some(subreddit_weight) = subreddit_weights.get(&subreddit) else {
			continue;
		};
		let Some(embedding) = embeddings.get(&subreddit) else {
			continue;
		};
		if vector.is_empty() {
			vector.resize(embedding.len(), 0.0);
		}
		if vector.len() != embedding.len() {
			continue;
		}
		let weight = suggested_profile_vector_count_weight(count) * subreddit_weight;
		if weight <= 0.0 {
			continue;
		}
		for (index, value) in embedding.iter().enumerate() {
			vector[index] += *value as f64 * weight;
		}
	}
	(normalize_suggested_profile_dense_vector(&mut vector) > 0.0).then_some(vector)
}

fn suggested_profile_dense_centroid(vectors: &[Option<Vec<f64>>], indexes: &[usize]) -> Option<Vec<f64>> {
	let mut centroid = Vec::<f64>::new();
	let mut count = 0usize;
	for index in indexes {
		let Some(vector) = vectors.get(*index).and_then(|vector| vector.as_ref()) else {
			continue;
		};
		if centroid.is_empty() {
			centroid.resize(vector.len(), 0.0);
		}
		if centroid.len() != vector.len() {
			continue;
		}
		for (centroid_value, vector_value) in centroid.iter_mut().zip(vector) {
			*centroid_value += vector_value;
		}
		count += 1;
	}
	if count == 0 {
		return None;
	}
	for value in &mut centroid {
		*value /= count as f64;
	}
	(normalize_suggested_profile_dense_vector(&mut centroid) > 0.0).then_some(centroid)
}

fn suggested_profile_dense_centroids_for_assignments(vectors: &[Option<Vec<f64>>], assignments: &[usize], centroid_count: usize) -> Vec<Option<Vec<f64>>> {
	(0..centroid_count)
		.map(|cluster| {
			let indexes = assignments
				.iter()
				.enumerate()
				.filter_map(|(index, assignment)| (*assignment == cluster).then_some(index))
				.collect::<Vec<_>>();
			suggested_profile_dense_centroid(vectors, &indexes)
		})
		.collect()
}

fn suggested_profile_blended_similarity(vector_similarity: f64, semantic_similarity: f64, semantic_mix_percent: u8) -> f64 {
	let mix = (semantic_mix_percent.min(100) as f64) / 100.0;
	(vector_similarity.max(0.0) * (1.0 - mix)) + (semantic_similarity.max(0.0) * mix)
}

fn suggested_profile_centroid(vectors: &[HashMap<String, f64>], indexes: &[usize]) -> HashMap<String, f64> {
	let mut centroid = HashMap::new();
	for index in indexes {
		for (subreddit, weight) in &vectors[*index] {
			*centroid.entry(subreddit.clone()).or_default() += weight;
		}
	}
	if !indexes.is_empty() {
		for weight in centroid.values_mut() {
			*weight /= indexes.len() as f64;
		}
	}
	normalize_suggested_profile_vector(&mut centroid);
	centroid
}

fn suggested_profile_vector_assignments(vectors: &[HashMap<String, f64>], centroids: &[HashMap<String, f64>]) -> Vec<usize> {
	vectors
		.iter()
		.map(|vector| {
			centroids
				.iter()
				.enumerate()
				.max_by(|(_, left), (_, right)| {
					suggested_profile_dot(vector, left)
						.partial_cmp(&suggested_profile_dot(vector, right))
						.unwrap_or(std::cmp::Ordering::Equal)
				})
				.map(|(index, _)| index)
				.unwrap_or(0)
		})
		.collect()
}

fn suggested_profile_vector_mean_distance(vectors: &[HashMap<String, f64>], assignments: &[usize], centroids: &[HashMap<String, f64>]) -> f64 {
	if vectors.is_empty() {
		return 0.0;
	}
	vectors
		.iter()
		.enumerate()
		.map(|(index, vector)| 1.0 - suggested_profile_dot(vector, &centroids[assignments[index]]))
		.sum::<f64>()
		/ vectors.len() as f64
}

fn suggested_profile_kmeans_centroids(vectors: &[HashMap<String, f64>], k: usize) -> Vec<HashMap<String, f64>> {
	if vectors.is_empty() || k <= 1 {
		return vec![suggested_profile_centroid(vectors, &(0..vectors.len()).collect::<Vec<_>>())];
	}

	let mut centroids = vec![vectors[0].clone()];
	while centroids.len() < k {
		let mut best_index = 0usize;
		let mut best_distance = -1.0f64;
		for (index, vector) in vectors.iter().enumerate() {
			let nearest_similarity = centroids.iter().map(|centroid| suggested_profile_dot(vector, centroid)).fold(0.0, f64::max);
			let distance = 1.0 - nearest_similarity;
			if distance > best_distance {
				best_distance = distance;
				best_index = index;
			}
		}
		centroids.push(vectors[best_index].clone());
	}

	for _ in 0..20 {
		let assignments = suggested_profile_vector_assignments(vectors, &centroids);
		let mut next_centroids = Vec::with_capacity(k);
		for cluster in 0..k {
			let indexes = assignments
				.iter()
				.enumerate()
				.filter_map(|(index, assignment)| (*assignment == cluster).then_some(index))
				.collect::<Vec<_>>();
			next_centroids.push(if indexes.is_empty() {
				centroids[cluster].clone()
			} else {
				suggested_profile_centroid(vectors, &indexes)
			});
		}
		if next_centroids == centroids {
			break;
		}
		centroids = next_centroids;
	}

	centroids
}

fn suggested_profile_taste_centroids(vectors: &[HashMap<String, f64>]) -> Vec<HashMap<String, f64>> {
	if vectors.is_empty() {
		return Vec::new();
	}
	let max_k = if vectors.len() >= 24 {
		4
	} else if vectors.len() >= 12 {
		3
	} else if vectors.len() >= 6 {
		2
	} else {
		1
	}
	.min(vectors.len());

	let mut best_centroids = Vec::new();
	let mut best_score = f64::INFINITY;
	for k in 1..=max_k {
		let centroids = if k == 1 {
			vec![suggested_profile_centroid(vectors, &(0..vectors.len()).collect::<Vec<_>>())]
		} else {
			suggested_profile_kmeans_centroids(vectors, k)
		};
		let assignments = suggested_profile_vector_assignments(vectors, &centroids);
		let mean_distance = suggested_profile_vector_mean_distance(vectors, &assignments, &centroids);
		let penalized_score = mean_distance + (k.saturating_sub(1) as f64 * 0.025);
		if penalized_score < best_score {
			best_score = penalized_score;
			best_centroids = centroids;
		}
	}
	best_centroids
}

fn normalize_vector_center_subreddit_name(name: &str) -> String {
	let trimmed = name.trim();
	let without_path_prefix = trimmed.strip_prefix("/r/").unwrap_or(trimmed);
	let without_prefix = without_path_prefix.strip_prefix("r/").unwrap_or(without_path_prefix);
	without_prefix.trim().to_lowercase()
}

fn suggested_profile_vector_center_top_subreddits(vector: &HashMap<String, f64>, limit: usize) -> Vec<String> {
	let mut entries = vector
		.iter()
		.filter(|(subreddit, weight)| !subreddit.is_empty() && !subreddit.starts_with("u_") && **weight > 0.0)
		.map(|(subreddit, weight)| (subreddit.clone(), *weight))
		.collect::<Vec<_>>();
	entries.sort_by(|left, right| right.1.partial_cmp(&left.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| left.0.cmp(&right.0)));
	entries.truncate(limit);
	entries.into_iter().map(|(subreddit, _)| subreddit).collect()
}

fn suggested_profile_vector_center_detail(vector: &HashMap<String, f64>) -> String {
	suggested_profile_vector_center_top_subreddits(vector, 3)
		.into_iter()
		.map(|subreddit| format!("r/{subreddit}"))
		.collect::<Vec<_>>()
		.join(", ")
}

fn suggested_profile_cluster_center_label(index: usize, total: usize) -> String {
	if total <= 1 {
		"Overall taste".to_string()
	} else {
		format!("Cluster {}", index + 1)
	}
}

fn suggested_profile_vector_center_options(
	centroids: &[HashMap<String, f64>],
	followed_top: &[(String, usize)],
	selected_id: Option<&str>,
) -> (Vec<SuggestedProfileVectorCenter>, Vec<SuggestedProfileVectorCenter>) {
	let cluster_centers = centroids
		.iter()
		.enumerate()
		.map(|(index, centroid)| {
			let request = SuggestedProfileVectorCenterRequest::Cluster(index);
			let id = request.id();
			SuggestedProfileVectorCenter {
				id: id.clone(),
				label: suggested_profile_cluster_center_label(index, centroids.len()),
				detail: suggested_profile_vector_center_detail(centroid),
				selected: selected_id == Some(id.as_str()),
			}
		})
		.collect();

	let subreddit_centers = followed_top
		.iter()
		.take(SUGGESTED_PROFILE_VECTOR_CENTER_SUBREDDIT_LIMIT)
		.map(|(subreddit, count)| {
			let request = SuggestedProfileVectorCenterRequest::Subreddit(subreddit.clone());
			let id = request.id();
			SuggestedProfileVectorCenter {
				id: id.clone(),
				label: format!("r/{subreddit}"),
				detail: format!("{count} posts"),
				selected: selected_id == Some(id.as_str()),
			}
		})
		.collect();

	(cluster_centers, subreddit_centers)
}

fn suggested_profile_semantic_reason_subreddits(
	profile_counts: &[(String, usize)],
	embeddings: &HashMap<String, Vec<f32>>,
	followed_subreddit_counts: &HashMap<String, usize>,
	followed_subreddit_profile_counts: &HashMap<String, usize>,
) -> Vec<SuggestedProfileSubreddit> {
	let mut reasons = profile_counts
		.iter()
		.filter(|(subreddit, count)| *count > 0 && embeddings.contains_key(subreddit))
		.map(|(subreddit, count)| SuggestedProfileSubreddit {
			name: subreddit.clone(),
			followed_count: *followed_subreddit_counts.get(subreddit).unwrap_or(&0),
			followed_profile_count: *followed_subreddit_profile_counts.get(subreddit).unwrap_or(&0),
			profile_count: *count,
		})
		.collect::<Vec<_>>();
	reasons.sort_by(|left, right| {
		right
			.profile_count
			.cmp(&left.profile_count)
			.then_with(|| right.followed_count.cmp(&left.followed_count))
			.then_with(|| left.name.cmp(&right.name))
	});
	reasons.truncate(SUGGESTED_PROFILE_REASON_LIMIT);
	reasons
}

fn normalize_profile_name(name: &str) -> String {
	let trimmed = name.trim();
	let without_prefix = trimmed.strip_prefix("u_").or_else(|| trimmed.strip_prefix("u/")).unwrap_or(trimmed);
	without_prefix.trim().to_lowercase()
}

fn first_non_empty(values: &[&str]) -> Option<String> {
	values.iter().map(|value| value.trim()).find(|value| !value.is_empty()).map(ToString::to_string)
}

fn suggested_profile_media_source(post: &Post) -> Option<(String, &'static str)> {
	match post.post_type.as_str() {
		"image" => first_non_empty(&[&post.media.preview_url, &post.media.url, &post.thumbnail.url]).map(|url| (url, "image")),
		"gif" => first_non_empty(&[&post.media.poster, &post.thumbnail.url, &post.media.preview_url, &post.media.url]).map(|url| (url, "gif")),
		"video" => first_non_empty(&[&post.media.poster, &post.thumbnail.url, &post.media.preview_url]).map(|url| (url, "video")),
		"gallery" => post
			.gallery
			.first()
			.and_then(|image| first_non_empty(&[&image.preview_url, &image.url, &post.thumbnail.url]))
			.map(|url| (url, "gallery")),
		_ => None,
	}
}

fn suggested_profile_media_key_url(url: &str) -> Option<String> {
	let trimmed = url.trim();
	if trimmed.is_empty() {
		return None;
	}
	let without_fragment = trimmed.split_once('#').map_or(trimmed, |(value, _)| value);
	let without_query = without_fragment.split_once('?').map_or(without_fragment, |(value, _)| value);
	Some(without_query.trim_end_matches('/').to_ascii_lowercase())
}

fn suggested_profile_media_key(post: &Post, fallback_url: &str) -> String {
	let mut candidates = Vec::new();
	candidates.push(post.media.content_fingerprint.as_str());
	candidates.push(post.media.url.as_str());
	if post.post_type == "gallery" {
		if let Some(image) = post.gallery.first() {
			candidates.push(image.url.as_str());
			candidates.push(image.preview_url.as_str());
		}
	}
	candidates.push(post.media.poster.as_str());
	candidates.push(post.media.preview_url.as_str());
	candidates.push(post.thumbnail.url.as_str());
	if let Some(out_url) = &post.out_url {
		candidates.push(out_url.as_str());
	}
	candidates.push(fallback_url);

	candidates
		.into_iter()
		.find_map(suggested_profile_media_key_url)
		.unwrap_or_else(|| fallback_url.trim().to_ascii_lowercase())
}

fn suggested_profile_media_candidate_from_post(post: &Post, created_ts: u64) -> Option<(String, SuggestedProfileMediaPost)> {
	if post.kind == "t1" {
		return None;
	}
	let (image_url, media_type) = suggested_profile_media_source(post)?;
	let media_key = suggested_profile_media_key(post, &image_url);
	Some((
		media_key,
		SuggestedProfileMediaPost {
			title: post.title.clone(),
			permalink: post.permalink.clone(),
			subreddit: post.community.clone(),
			image_url,
			media_type: media_type.to_string(),
			duration: post.media.duration.clone(),
			nsfw: post.flags.nsfw,
			spoiler: post.flags.spoiler,
			created_ts,
		},
	))
}

fn suggested_profile_media_candidate(record: &CatalogRecord) -> Option<(String, SuggestedProfileMediaPost)> {
	suggested_profile_media_candidate_from_post(&record.post, record.created_ts)
}

fn suggested_profile_media_post_references_reddit_asset(post: &SuggestedProfileMediaPost, asset_key: &str) -> bool {
	crate::utils::reddit_image_asset_key(&post.image_url).is_some_and(|key| key == asset_key)
}

fn author_history_activity_ts(post: &PersistedAuthorHistoryPost) -> u64 {
	post.created_ts.max(post.seen_at_unix)
}

fn author_history_post_from_post(post: &Post, now_unix: u64) -> Option<PersistedAuthorHistoryPost> {
	if post.id.trim().is_empty() || post.author.name.trim().is_empty() || post.author.name == "[deleted]" {
		return None;
	}
	let subreddit = post.community.trim().to_lowercase();
	if subreddit.is_empty() || subreddit.starts_with("u_") || crate::utils::is_deleted_reddit_placeholder_post(post) {
		return None;
	}

	let (media_key, media_post) = suggested_profile_media_candidate_from_post(post, post.created_ts)
		.map(|(key, post)| (key, Some(post)))
		.unwrap_or_else(|| (String::new(), None));
	let redgifs_username = if matches!(post.post_type.as_str(), "video" | "gif") {
		post.media.redgifs_username.clone()
	} else {
		String::new()
	};

	Some(PersistedAuthorHistoryPost {
		post_id: post.id.clone(),
		subreddit,
		created_ts: post.created_ts,
		seen_at_unix: now_unix,
		media_key,
		media_post,
		video_key: crate::user::video_aggregate_key(post),
		video_duration_seconds: post.media.duration_seconds,
		redgifs_username,
	})
}

fn suggested_profile_unique_media_posts(mut posts: Vec<(String, SuggestedProfileMediaPost)>, limit: usize) -> Vec<SuggestedProfileMediaPost> {
	posts.sort_by(|(_, left), (_, right)| right.created_ts.cmp(&left.created_ts).then_with(|| right.permalink.cmp(&left.permalink)));

	let mut seen_media = HashSet::new();
	let mut unique_posts = Vec::new();
	for (media_key, post) in posts {
		if !seen_media.insert(media_key) {
			continue;
		}
		unique_posts.push(post);
		if unique_posts.len() >= limit {
			break;
		}
	}
	unique_posts
}

fn suggested_profile_media_posts(username: &str, limit: usize) -> Vec<SuggestedProfileMediaPost> {
	let username = normalize_profile_name(username);
	if username.is_empty() || limit == 0 {
		return Vec::new();
	}

	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let Some(posts) = catalog.author_media_posts.get(&username) else {
		return Vec::new();
	};

	let mut posts = posts.clone();
	posts.truncate(limit);
	posts
}

fn decorate_suggested_profiles(suggestions: &mut [SuggestedProfileEntry]) {
	for suggestion in suggestions {
		let video_signal = crate::user::cached_video_signal(&suggestion.name);
		suggestion.avg_video_length = video_signal.avg_video_length;
		suggestion.video_count = video_signal.video_count;
		suggestion.redgifs_profile_count = video_signal.redgifs_profile_count;
		suggestion.media_posts = suggested_profile_media_posts(&suggestion.name, SUGGESTED_PROFILE_MEDIA_LIMIT);
	}
}

fn suggested_profile_is_rejected_for_redgifs_profile_count(username: &str) -> bool {
	if rejected_author_redgifs_profile_count(username) >= SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD {
		return true;
	}
	let redgifs_profile_count = crate::user::cached_video_signal(username).redgifs_profile_count;
	if redgifs_profile_count < SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD {
		return false;
	}
	remember_rejected_author_redgifs_profile_count(username, redgifs_profile_count);
	true
}

fn rejected_author_redgifs_profile_count_locked(catalog: &MaterializedCatalog, username: &str) -> usize {
	let username = normalize_profile_name(username);
	if username.is_empty() {
		return 0;
	}
	catalog
		.rejected_author_redgifs_profiles
		.get(&username)
		.map(|entry| entry.redgifs_profile_count)
		.unwrap_or_default()
}

pub(crate) fn rejected_author_redgifs_profile_count(username: &str) -> usize {
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	rejected_author_redgifs_profile_count_locked(&catalog, username)
}

fn remember_rejected_author_redgifs_profile_count(username: &str, redgifs_profile_count: usize) {
	if redgifs_profile_count < SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD {
		return;
	}
	let username = normalize_profile_name(username);
	if username.is_empty() || username == "[deleted]" {
		return;
	}

	let now_unix = crate::cache_persistence::unix_now();
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let had_indexed_data = catalog.author_history.remove(&username).is_some()
		|| catalog.author_to_post_ids.contains_key(&username)
		|| catalog.author_community_counts.contains_key(&username)
		|| catalog.author_video_stats.contains_key(&username)
		|| catalog.author_media_posts.contains_key(&username);
	catalog
		.rejected_author_redgifs_profiles
		.entry(username.clone())
		.and_modify(|entry| {
			entry.redgifs_profile_count = entry.redgifs_profile_count.max(redgifs_profile_count);
			entry.seen_at_unix = now_unix;
		})
		.or_insert_with(|| PersistedRejectedAuthorRedgifsProfileEntry {
			author: username,
			redgifs_profile_count,
			seen_at_unix: now_unix,
		});
	if had_indexed_data {
		rebuild_indexes(&mut catalog);
	}
	crate::cache_persistence::mark_dirty();
}

fn redgifs_profile_count_for_history_posts(posts: &HashMap<String, PersistedAuthorHistoryPost>) -> usize {
	posts
		.values()
		.filter_map(|post| {
			let slug = crate::utils::normalize_creator_identity_slug(&post.redgifs_username);
			(!slug.is_empty()).then_some(slug)
		})
		.collect::<HashSet<_>>()
		.len()
}

fn reject_author_history_with_many_redgifs_profiles_locked(catalog: &mut MaterializedCatalog, now_unix: u64) {
	let rejected_authors = catalog
		.author_history
		.iter()
		.filter_map(|(author, posts)| {
			let redgifs_profile_count = redgifs_profile_count_for_history_posts(posts);
			(redgifs_profile_count >= SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD).then_some((author.clone(), redgifs_profile_count))
		})
		.collect::<Vec<_>>();

	for (author, redgifs_profile_count) in rejected_authors {
		catalog.author_history.remove(&author);
		catalog.rejected_author_redgifs_profiles.insert(
			author.clone(),
			PersistedRejectedAuthorRedgifsProfileEntry {
				author,
				redgifs_profile_count,
				seen_at_unix: now_unix,
			},
		);
	}
}

fn prune_author_history_locked(catalog: &mut MaterializedCatalog, now_unix: u64) {
	let cutoff_unix = now_unix.saturating_sub(AUTHOR_HISTORY_MAX_POST_AGE_SECS);

	catalog.rejected_author_redgifs_profiles.retain(|_, entry| entry.seen_at_unix >= cutoff_unix);

	for posts in catalog.author_history.values_mut() {
		posts.retain(|_, post| author_history_activity_ts(post) >= cutoff_unix);
		if posts.len() <= AUTHOR_HISTORY_MAX_POSTS_PER_AUTHOR {
			continue;
		}

		let mut keep_ids = posts.keys().cloned().collect::<Vec<_>>();
		keep_ids.sort_by(|left, right| {
			let left_post = posts.get(left).expect("left post exists");
			let right_post = posts.get(right).expect("right post exists");
			author_history_activity_ts(right_post)
				.cmp(&author_history_activity_ts(left_post))
				.then_with(|| right.cmp(left))
		});
		keep_ids.truncate(AUTHOR_HISTORY_MAX_POSTS_PER_AUTHOR);
		let keep_ids = keep_ids.into_iter().collect::<HashSet<_>>();
		posts.retain(|post_id, _| keep_ids.contains(post_id));
	}

	catalog.author_history.retain(|_, posts| !posts.is_empty());

	let total_posts = catalog.author_history.values().map(HashMap::len).sum::<usize>();
	if total_posts <= AUTHOR_HISTORY_GLOBAL_MAX_POSTS {
		return;
	}

	let mut posts_by_age = Vec::with_capacity(total_posts);
	for (author, posts) in &catalog.author_history {
		for (post_id, post) in posts {
			posts_by_age.push((author_history_activity_ts(post), author.clone(), post_id.clone()));
		}
	}
	posts_by_age.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.2.cmp(&right.2)));

	let remove_count = total_posts.saturating_sub(AUTHOR_HISTORY_GLOBAL_MAX_POSTS);
	for (_, author, post_id) in posts_by_age.into_iter().take(remove_count) {
		if let Some(posts) = catalog.author_history.get_mut(&author) {
			posts.remove(&post_id);
		}
	}
	catalog.author_history.retain(|_, posts| !posts.is_empty());
}

fn record_author_history_from_posts_locked(catalog: &mut MaterializedCatalog, posts: &[Post], now_unix: u64) {
	for post in posts {
		let author = post.author.name.trim().to_lowercase();
		if author.is_empty() || author == "[deleted]" {
			continue;
		}
		if let Some(rejected) = catalog.rejected_author_redgifs_profiles.get_mut(&author) {
			rejected.seen_at_unix = now_unix;
			continue;
		}
		let Some(history_post) = author_history_post_from_post(post, now_unix) else {
			continue;
		};
		catalog.author_history.entry(author).or_default().insert(history_post.post_id.clone(), history_post);
	}
	prune_author_history_locked(catalog, now_unix);
	reject_author_history_with_many_redgifs_profiles_locked(catalog, now_unix);
}

pub(crate) fn record_author_history_from_posts(posts: &[Post]) {
	if posts.is_empty() {
		return;
	}
	let now_unix = crate::cache_persistence::unix_now();
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	record_author_history_from_posts_locked(&mut catalog, posts, now_unix);
	rebuild_indexes(&mut catalog);
	crate::cache_persistence::mark_dirty();
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
	catalog.author_video_stats.clear();
	catalog.author_media_posts.clear();

	let mut author_media_candidates: HashMap<String, Vec<(String, SuggestedProfileMediaPost)>> = HashMap::new();

	for (post_id, record) in &catalog.posts_by_id {
		for source_key in &record.source_keys {
			catalog.source_to_post_ids.entry(source_key.clone()).or_default().insert(post_id.clone());
		}

		let author_is_rejected = rejected_author_redgifs_profile_count_locked(catalog, &record.author_lc) >= SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD;

		if !record.author_lc.is_empty() && !author_is_rejected {
			catalog.author_to_post_ids.entry(record.author_lc.clone()).or_default().insert(post_id.clone());
			catalog.author_video_stats.entry(record.author_lc.clone()).or_default().observe_post(&record.post);
			if let Some(candidate) = suggested_profile_media_candidate(record) {
				author_media_candidates.entry(record.author_lc.clone()).or_default().push(candidate);
			}
		}

		if !record.author_lc.is_empty() && !author_is_rejected && !record.community_lc.is_empty() && !record.community_lc.starts_with("u_") {
			*catalog
				.author_community_counts
				.entry(record.author_lc.clone())
				.or_default()
				.entry(record.community_lc.clone())
				.or_default() += 1;
		}
	}

	for (author, posts) in &catalog.author_history {
		if author.is_empty() {
			continue;
		}
		for (post_id, post) in posts {
			if catalog.posts_by_id.contains_key(post_id) {
				continue;
			}

			catalog.author_to_post_ids.entry(author.clone()).or_default().insert(post_id.clone());
			if !post.subreddit.is_empty() && !post.subreddit.starts_with("u_") {
				*catalog
					.author_community_counts
					.entry(author.clone())
					.or_default()
					.entry(post.subreddit.clone())
					.or_default() += 1;
			}

			if post.video_key.is_some() || !post.redgifs_username.is_empty() {
				catalog
					.author_video_stats
					.entry(author.clone())
					.or_default()
					.observe_video_signal(post.video_key.clone(), post.video_duration_seconds, &post.redgifs_username);
			}

			if let Some(media_post) = &post.media_post {
				author_media_candidates
					.entry(author.clone())
					.or_default()
					.push((post.media_key.clone(), media_post.clone()));
			}
		}
	}

	for (author, posts) in author_media_candidates {
		let posts = suggested_profile_unique_media_posts(posts, SUGGESTED_PROFILE_MEDIA_LIMIT);
		if !posts.is_empty() {
			catalog.author_media_posts.insert(author, posts);
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

fn requested_source_keys_for_session(session: &FeedSession) -> HashSet<CatalogSourceKey> {
	session.sources.iter().filter_map(|entry| normalize_catalog_source(&entry.source)).collect()
}

fn catalog_record_is_visible(record: &CatalogRecord, allowed_keys: &HashSet<CatalogSourceKey>, filters: &HashSet<String>) -> bool {
	if record.source_keys.is_disjoint(allowed_keys) {
		return false;
	}
	if crate::promo_detection::post_is_promotional(&record.post.id) {
		return false;
	}
	if crate::utils::post_matches_filter_policy(&record.post, filters) {
		return false;
	}
	if crate::utils::is_deleted_reddit_placeholder_post(&record.post) {
		return false;
	}
	true
}

fn session_merge_sort(session: &FeedSession) -> FeedSort {
	session
		.request
		.merge_sort
		.or_else(|| session.sources.first().map(|entry| entry.source.sort()))
		.unwrap_or(FeedSort::New)
}

fn query_posts_for_keys_limited(
	catalog: &MaterializedCatalog,
	allowed_keys: &HashSet<CatalogSourceKey>,
	filters: &HashSet<String>,
	limit: usize,
	sort: FeedSort,
	random_rotation_seed: u64,
) -> Vec<Post> {
	if allowed_keys.is_empty() {
		return Vec::new();
	}
	if limit == 0 {
		return Vec::new();
	}

	if sort == FeedSort::Random {
		let mut ranked = catalog
			.posts_by_id
			.values()
			.filter(|record| catalog_record_is_visible(record, allowed_keys, filters))
			.map(|record| (deterministic_random_post_sort_key_with_seed(&record.post, random_rotation_seed), record.post.id.clone()))
			.collect::<Vec<_>>();
		ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
		ranked.truncate(limit);
		return ranked
			.into_iter()
			.filter_map(|(_, post_id)| catalog.posts_by_id.get(&post_id).map(|record| record.post.clone()))
			.collect();
	}

	let mut posts = Vec::new();
	for entry in &catalog.timeline_new {
		let Some(record) = catalog.posts_by_id.get(&entry.post_id) else {
			continue;
		};
		if !catalog_record_is_visible(record, allowed_keys, filters) {
			continue;
		}
		posts.push(record.post.clone());
		if posts.len() >= limit {
			break;
		}
	}
	posts
}

fn mixed_feed_record_is_visible(record: &MixedFeedRecord, allowed_keys: &HashSet<CatalogSourceKey>, filters: &HashSet<String>) -> bool {
	if record.source_keys.is_disjoint(allowed_keys) {
		return false;
	}
	if crate::utils::post_matches_filter_policy(&record.post, filters) {
		return false;
	}
	!crate::utils::is_deleted_reddit_placeholder_post(&record.post)
}

fn query_mixed_feed_for_keys_limited(
	catalog: &MixedFeedCatalog,
	allowed_keys: &HashSet<CatalogSourceKey>,
	filters: &HashSet<String>,
	limit: usize,
	sort: FeedSort,
	random_rotation_seed: u64,
	daemon_discovery_order: bool,
	following_activity_order: bool,
) -> Option<Vec<Post>> {
	if allowed_keys.is_empty() || limit == 0 {
		return Some(Vec::new());
	}
	let has_cached_source = allowed_keys
		.iter()
		.any(|source_key| catalog.source_to_post_ids.get(source_key).is_some_and(|post_ids| !post_ids.is_empty()));
	if !has_cached_source {
		return None;
	}

	if sort == FeedSort::Random {
		// Randomize within source-sized rounds. This preserves random-looking output while
		// ensuring one post from every cached feed is considered before a second post from
		// any feed, instead of letting a few prolific sources dominate the random window.
		let mut best_source_rank = HashMap::<String, usize>::new();
		for source_key in allowed_keys {
			let Some(post_ids) = catalog.source_to_post_ids.get(source_key) else {
				continue;
			};
			let mut ranked = post_ids
				.iter()
				.filter_map(|post_id| {
					let record = catalog.posts_by_id.get(post_id)?;
					mixed_feed_record_is_visible(record, allowed_keys, filters)
						.then_some((deterministic_random_post_sort_key_with_seed(&record.post, random_rotation_seed), post_id.clone()))
				})
				.collect::<Vec<_>>();
			ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
			for (rank, (_, post_id)) in ranked.into_iter().enumerate() {
				best_source_rank.entry(post_id).and_modify(|current| *current = (*current).min(rank)).or_insert(rank);
			}
		}
		let mut posts = best_source_rank
			.into_iter()
			.filter_map(|(post_id, rank)| {
				let record = catalog.posts_by_id.get(&post_id)?;
				Some((rank, deterministic_random_post_sort_key_with_seed(&record.post, random_rotation_seed), record.post.clone()))
			})
			.collect::<Vec<_>>();
		posts.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)).then_with(|| left.2.id.cmp(&right.2.id)));
		posts.truncate(limit);
		return Some(posts.into_iter().map(|(_, _, post)| post).collect());
	}
	if daemon_discovery_order {
		let discovery = DAEMON_DISCOVERY.lock().unwrap_or_else(|err| err.into_inner());
		let mut posts = catalog
			.posts_by_id
			.values()
			.filter(|record| mixed_feed_record_is_visible(record, allowed_keys, filters))
			.map(|record| {
				let stamp = std::iter::once(record.post.id.as_str())
					.chain(record.post.following_comments.iter().map(|comment| comment.id.as_str()))
					.filter_map(|post_id| discovery.posts.get(post_id).copied())
					.max_by_key(|stamp| (stamp.discovered_at_unix, stamp.sequence))
					.unwrap_or(FollowingDiscoveryStamp {
						discovered_at_unix: 0,
						sequence: 0,
					});
				(stamp, record.post.clone())
			})
			.collect::<Vec<_>>();
		posts.sort_by(|left, right| {
			right
				.0
				.discovered_at_unix
				.cmp(&left.0.discovered_at_unix)
				.then_with(|| right.0.sequence.cmp(&left.0.sequence))
				.then_with(|| right.1.created_ts.cmp(&left.1.created_ts))
				.then_with(|| right.1.id.cmp(&left.1.id))
		});
		posts.truncate(limit);
		return Some(posts.into_iter().map(|(_, post)| post).collect());
	}

	let mut posts = catalog
		.posts_by_id
		.values()
		.filter(|record| mixed_feed_record_is_visible(record, allowed_keys, filters))
		.map(|record| record.post.clone())
		.collect::<Vec<_>>();
	posts.sort_by(|left, right| {
		let left_ts = if following_activity_order {
			crate::user::post_feed_activity_ts(left)
		} else {
			left.created_ts
		};
		let right_ts = if following_activity_order {
			crate::user::post_feed_activity_ts(right)
		} else {
			right.created_ts
		};
		right_ts.cmp(&left_ts).then_with(|| right.id.cmp(&left.id))
	});
	posts.truncate(limit);
	Some(posts)
}

fn query_page_metric_candidates_for_keys_limited(
	catalog: &MaterializedCatalog,
	allowed_keys: &HashSet<CatalogSourceKey>,
	filters: &HashSet<String>,
	limit: usize,
	sort: FeedSort,
	random_rotation_seed: u64,
) -> Vec<CatalogPageMetricCandidate> {
	if allowed_keys.is_empty() || limit == 0 {
		return Vec::new();
	}

	if sort == FeedSort::Random {
		let mut ranked = catalog
			.posts_by_id
			.values()
			.filter(|record| catalog_record_is_visible(record, allowed_keys, filters))
			.map(|record| (deterministic_random_post_sort_key_with_seed(&record.post, random_rotation_seed), record.post.id.clone()))
			.collect::<Vec<_>>();
		ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
		ranked.truncate(limit);
		return ranked
			.into_iter()
			.filter_map(|(_, post_id)| {
				let record = catalog.posts_by_id.get(&post_id)?;
				Some(CatalogPageMetricCandidate {
					created_ts: record.created_ts,
					score: record.post.score.1.parse::<i64>().ok(),
					stickied: record.post.flags.stickied,
				})
			})
			.collect();
	}

	let mut candidates = Vec::new();
	for entry in &catalog.timeline_new {
		let Some(record) = catalog.posts_by_id.get(&entry.post_id) else {
			continue;
		};
		if !catalog_record_is_visible(record, allowed_keys, filters) {
			continue;
		}
		candidates.push(CatalogPageMetricCandidate {
			created_ts: record.created_ts,
			score: record.post.score.1.parse::<i64>().ok(),
			stickied: record.post.flags.stickied,
		});
		if candidates.len() >= limit {
			break;
		}
	}
	candidates
}

pub(crate) fn query_author_posts_limited(username: &str, filters: &HashSet<String>, limit: usize) -> Vec<Post> {
	let username = normalize_profile_name(username);
	if username.is_empty() || limit == 0 {
		return Vec::new();
	}

	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let Some(author_post_ids) = catalog.author_to_post_ids.get(&username) else {
		return Vec::new();
	};

	let mut posts = Vec::new();
	for entry in &catalog.timeline_new {
		if !author_post_ids.contains(&entry.post_id) {
			continue;
		}
		let Some(record) = catalog.posts_by_id.get(&entry.post_id) else {
			continue;
		};
		if crate::utils::post_matches_filter_policy(&record.post, filters) {
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
	apply_source_refresh_with_observation(source, posts, crate::post_components::ObservationSource::Listing);
}

pub(crate) fn apply_source_refresh_with_observation(source: &FeedSource, posts: &[Post], observation_source: crate::post_components::ObservationSource) {
	let Some(source_key) = normalize_catalog_source(source) else {
		return;
	};

	crate::post_components::observe_posts(posts, observation_source, format!("{source_key:?}"), crate::cache_persistence::unix_now());

	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	if !posts.is_empty() {
		record_author_history_from_posts_locked(&mut catalog, posts, crate::cache_persistence::unix_now());
	}
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

/// Remove several expired source memberships with one catalog rebuild.
///
/// Source-cache pruning often evicts a small batch at once. Rebuilding every
/// derived author/timeline index after each individual removal creates large,
/// short-lived allocations while producing the same final catalog.
pub(crate) fn remove_sources(sources: &[FeedSource]) {
	let source_keys = sources.iter().filter_map(normalize_catalog_source).collect::<HashSet<_>>();
	if source_keys.is_empty() {
		return;
	}

	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	for record in catalog.posts_by_id.values_mut() {
		record.source_keys.retain(|source_key| !source_keys.contains(source_key));
	}
	catalog.posts_by_id.retain(|_, record| !record.source_keys.is_empty());
	rebuild_indexes(&mut catalog);
}

/// Remove sources from the compact persisted-feed mix only when their on-disk payload is
/// actually deleted. Resident-cache eviction must leave this index intact.
pub(crate) fn remove_mixed_feed_sources(sources: &[FeedSource]) {
	remove_sources_from_feed_catalog(&FOLLOWING_FEED_CATALOG, sources);
	remove_sources_from_feed_catalog(&MIXED_FEED_CATALOG, sources);
}

fn remove_sources_from_feed_catalog(catalog: &Mutex<MixedFeedCatalog>, sources: &[FeedSource]) {
	let source_keys = sources.iter().filter_map(normalize_catalog_source).collect::<HashSet<_>>();
	if source_keys.is_empty() {
		return;
	}

	let mut catalog = catalog.lock().unwrap_or_else(|err| err.into_inner());
	let mut post_ids = HashSet::new();
	for source_key in &source_keys {
		if let Some(ids) = catalog.source_to_post_ids.remove(source_key) {
			post_ids.extend(ids);
		}
	}
	for post_id in post_ids {
		let should_remove = if let Some(record) = catalog.posts_by_id.get_mut(&post_id) {
			record.source_keys.retain(|source_key| !source_keys.contains(source_key));
			record.source_keys.is_empty()
		} else {
			false
		};
		if should_remove {
			catalog.posts_by_id.remove(&post_id);
		}
	}
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
	query_posts_for_keys_limited(&catalog, &allowed_keys, filters, usize::MAX, FeedSort::New, 0)
}

pub(crate) fn query_session_stream_limited(session: &FeedSession, limit: usize) -> Vec<Post> {
	let allowed_keys = allowed_source_keys_for_session(session);
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	query_posts_for_keys_limited(
		&catalog,
		&allowed_keys,
		&session.request.filters,
		limit,
		session_merge_sort(session),
		session_random_rotation_seed(session),
	)
}

/// Query the compact persisted-feed mix using every requested cached source, not only the
/// handful of full source payloads admitted to the current browser session.
pub(crate) fn query_mixed_feed_session_stream_limited(session: &FeedSession, limit: usize) -> Option<Vec<Post>> {
	let allowed_keys = requested_source_keys_for_session(session);
	let catalog = if session.request.bundle_followed_user_comments {
		FOLLOWING_FEED_CATALOG.lock().unwrap_or_else(|err| err.into_inner())
	} else {
		MIXED_FEED_CATALOG.lock().unwrap_or_else(|err| err.into_inner())
	};
	query_mixed_feed_for_keys_limited(
		&catalog,
		&allowed_keys,
		&session.request.filters,
		limit,
		session_merge_sort(session),
		session_random_rotation_seed(session),
		session.request.daemon_discovery_order,
		session.request.bundle_followed_user_comments,
	)
}

pub(crate) fn query_mixed_feed_source_candidates_limited(sources: &[FeedSource], filters: &HashSet<String>, limit: usize) -> Option<Vec<Post>> {
	let allowed_keys = sources.iter().filter_map(normalize_catalog_source).collect::<HashSet<_>>();
	let catalog = MIXED_FEED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	query_mixed_feed_for_keys_limited(&catalog, &allowed_keys, filters, limit, FeedSort::New, 0, false, false)
}

pub(crate) fn query_mixed_feed_session_page_metric_candidates_limited(session: &FeedSession, limit: usize) -> Option<Vec<CatalogPageMetricCandidate>> {
	query_mixed_feed_session_stream_limited(session, limit).map(|posts| {
		posts
			.into_iter()
			.map(|post| CatalogPageMetricCandidate {
				created_ts: post.created_ts,
				score: post.score.1.parse::<i64>().ok(),
				stickied: post.flags.stickied,
			})
			.collect()
	})
}

pub(crate) fn query_session_page_metric_candidates_limited(session: &FeedSession, limit: usize) -> Vec<CatalogPageMetricCandidate> {
	let allowed_keys = allowed_source_keys_for_session(session);
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	query_page_metric_candidates_for_keys_limited(
		&catalog,
		&allowed_keys,
		&session.request.filters,
		limit,
		session_merge_sort(session),
		session_random_rotation_seed(session),
	)
}

pub(crate) fn query_source_candidates_limited(sources: &[FeedSource], filters: &HashSet<String>, limit: usize) -> Vec<Post> {
	let allowed_keys = sources.iter().filter_map(normalize_catalog_source).collect::<HashSet<_>>();
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	query_posts_for_keys_limited(&catalog, &allowed_keys, filters, limit, FeedSort::New, 0)
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

pub fn cached_catalog_subreddit_names() -> Vec<String> {
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let mut names = HashSet::new();
	for counts in catalog.author_community_counts.values() {
		for subreddit in counts.keys() {
			let subreddit = subreddit_semantics::normalize_subreddit_name(subreddit);
			if !subreddit.is_empty() && !subreddit.starts_with("u_") {
				names.insert(subreddit);
			}
		}
	}
	for source_key in catalog.source_to_post_ids.keys() {
		match source_key {
			CatalogSourceKey::Subreddit(subreddit) => {
				let subreddit = subreddit_semantics::normalize_subreddit_name(subreddit);
				if !subreddit.is_empty() && !subreddit.starts_with("u_") {
					names.insert(subreddit);
				}
			}
			CatalogSourceKey::Flair { subreddit, .. } => {
				let subreddit = subreddit_semantics::normalize_subreddit_name(subreddit);
				if !subreddit.is_empty() && !subreddit.starts_with("u_") {
					names.insert(subreddit);
				}
			}
			CatalogSourceKey::UserComments(_) => {}
		}
	}
	let mut names = names.into_iter().collect::<Vec<_>>();
	names.sort();
	names
}

pub fn cached_catalog_subreddit_names_by_activity() -> Vec<String> {
	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let mut stats = HashMap::<String, (usize, usize)>::new();
	for counts in catalog.author_community_counts.values() {
		for (subreddit, count) in counts {
			let subreddit = subreddit_semantics::normalize_subreddit_name(subreddit);
			if subreddit.is_empty() || subreddit.starts_with("u_") || *count == 0 {
				continue;
			}
			let entry = stats.entry(subreddit).or_default();
			entry.0 += 1;
			entry.1 += *count;
		}
	}
	for (source_key, post_ids) in &catalog.source_to_post_ids {
		let subreddit = match source_key {
			CatalogSourceKey::Subreddit(subreddit) => subreddit,
			CatalogSourceKey::Flair { subreddit, .. } => subreddit,
			CatalogSourceKey::UserComments(_) => continue,
		};
		let subreddit = subreddit_semantics::normalize_subreddit_name(subreddit);
		if subreddit.is_empty() || subreddit.starts_with("u_") || post_ids.is_empty() {
			continue;
		}
		let entry = stats.entry(subreddit).or_default();
		entry.1 += post_ids.len();
	}
	let mut names = stats.into_iter().collect::<Vec<_>>();
	names.sort_by(|left, right| {
		let (left_profile_count, left_post_count) = left.1;
		let (right_profile_count, right_post_count) = right.1;
		right_profile_count
			.cmp(&left_profile_count)
			.then_with(|| right_post_count.cmp(&left_post_count))
			.then_with(|| left.0.cmp(&right.0))
	});
	names.into_iter().map(|(name, _)| name).collect()
}

fn related_score_percent(score: f64) -> u32 {
	(score * 100.0).round().clamp(1.0, 100.0) as u32
}

fn normalized_followed_sets(subscriptions: &[String]) -> (HashSet<String>, HashSet<String>) {
	let mut subreddits = HashSet::new();
	let mut profiles = HashSet::new();
	for subscription in subscriptions {
		let normalized = subscription.trim().to_lowercase();
		if normalized.is_empty() {
			continue;
		}
		if let Some(profile) = normalized.strip_prefix("u_") {
			if !profile.is_empty() {
				profiles.insert(profile.to_string());
			}
		} else {
			let subreddit = subreddit_semantics::normalize_subreddit_name(&normalized);
			if !subreddit.is_empty() && !subreddit.starts_with("u_") {
				subreddits.insert(subreddit);
			}
		}
	}
	(subreddits, profiles)
}

fn related_profile_signals(counts: &HashMap<String, usize>, current_embedding: &[f32], embeddings: &HashMap<String, Vec<f32>>, limit: usize) -> Vec<String> {
	let mut signals = sorted_subreddit_counts(counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS)
		.into_iter()
		.filter_map(|(subreddit, count)| {
			let embedding = embeddings.get(&subreddit)?;
			let similarity = embedding_cosine(current_embedding, embedding);
			(similarity > 0.0).then_some((
				subreddit.clone(),
				similarity * suggested_profile_vector_count_weight(count) * crate::promo_detection::promotion_quality_factor(&subreddit),
				count,
			))
		})
		.collect::<Vec<_>>();
	signals.sort_by(|left, right| {
		right
			.1
			.partial_cmp(&left.1)
			.unwrap_or(std::cmp::Ordering::Equal)
			.then_with(|| right.2.cmp(&left.2))
			.then_with(|| left.0.cmp(&right.0))
	});
	signals.truncate(limit);
	signals.into_iter().map(|(subreddit, _, _)| subreddit).collect()
}

fn rank_related_subreddits(
	candidate_subreddits: &HashSet<String>,
	current_embedding: Option<&[f32]>,
	followed_subreddits: &HashSet<String>,
	normalized_filters: &HashSet<String>,
	subreddit_stats: &HashMap<String, (usize, usize)>,
	content_post_counts: &HashMap<String, usize>,
	embedding_for_candidate: impl Fn(&str) -> Option<Vec<f32>>,
	use_content_post_count: bool,
	limit: usize,
) -> (Vec<RelatedSubredditEntry>, Vec<RelatedSubredditEntry>) {
	let Some(current_embedding) = current_embedding else {
		return (Vec::new(), Vec::new());
	};
	let mut related_subreddits = candidate_subreddits
		.iter()
		.filter(|candidate| !normalized_filters.contains(*candidate))
		.filter_map(|candidate| {
			let embedding = embedding_for_candidate(candidate)?;
			let similarity = embedding_cosine(current_embedding, &embedding);
			if similarity < RELATED_SUBREDDIT_MIN_SIMILARITY {
				return None;
			}
			let (cached_profile_count, cached_post_count) = subreddit_stats
				.get(candidate)
				.copied()
				.unwrap_or_else(|| (0, content_post_counts.get(candidate).copied().unwrap_or_default()));
			let cached_post_count = if use_content_post_count {
				content_post_counts.get(candidate).copied().unwrap_or(cached_post_count)
			} else {
				cached_post_count
			};
			Some((
				followed_subreddits.contains(candidate),
				RelatedSubredditEntry {
					name: candidate.clone(),
					score_percent: related_score_percent(similarity * crate::promo_detection::promotion_quality_factor(candidate)),
					cached_profile_count,
					cached_post_count,
				},
			))
		})
		.collect::<Vec<_>>();
	related_subreddits.sort_by(|left, right| {
		right
			.1
			.score_percent
			.cmp(&left.1.score_percent)
			.then_with(|| right.1.cached_profile_count.cmp(&left.1.cached_profile_count))
			.then_with(|| right.1.cached_post_count.cmp(&left.1.cached_post_count))
			.then_with(|| left.1.name.cmp(&right.1.name))
	});
	let mut followed = Vec::new();
	let mut cached = Vec::new();
	for (is_followed, entry) in related_subreddits {
		if is_followed {
			if followed.len() < limit {
				followed.push(entry);
			}
		} else if cached.len() < limit {
			cached.push(entry);
		}
		if followed.len() >= limit && cached.len() >= limit {
			break;
		}
	}
	(followed, cached)
}

pub(crate) fn related_subreddit_report(subreddit: &str, subscriptions: &[String], filters: &HashSet<String>, limit: usize) -> RelatedSubredditReport {
	let subreddit = subreddit_semantics::normalize_subreddit_name(subreddit);
	if subreddit.is_empty() || subreddit == "all" || subreddit == "popular" || subreddit.contains('+') || limit == 0 {
		return RelatedSubredditReport::default();
	}
	if let Err(error) = subreddit_semantics::refresh_persisted_embeddings_sidecar() {
		warn!("related subreddits: failed to refresh subreddit semantic embeddings sidecar: {error}");
	}
	if let Err(error) = subreddit_content_semantics::refresh_persisted_embeddings_sidecar() {
		warn!("related subreddits: failed to refresh subreddit content semantic embeddings sidecar: {error}");
	}
	let semantic_model = subreddit_semantics::embedding_model();
	let metadata_embedding = subreddit_semantics::embedding_for_subreddit_model(&subreddit, &semantic_model);
	let vibe_embedding = subreddit_content_semantics::embedding_for_subreddit_model(&subreddit, &semantic_model);
	let profile_embedding = blended_subreddit_semantic_embedding(&subreddit, &semantic_model);
	if metadata_embedding.is_none() && vibe_embedding.is_none() && profile_embedding.is_none() {
		return RelatedSubredditReport {
			semantic_embedding_count: subreddit_semantics::active_embedding_count(),
			vibe_embedding_count: subreddit_content_semantics::active_usable_embedding_count(),
			..RelatedSubredditReport::default()
		};
	}
	let current_vector = profile_embedding.as_deref().and_then(normalized_f32_embedding);

	let (followed_subreddits, followed_profiles) = normalized_followed_sets(subscriptions);
	let normalized_filters = filters.iter().map(|filter| filter.trim().to_lowercase()).collect::<HashSet<_>>();
	let (subreddit_stats, profile_counts_by_profile) = {
		let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
		let mut subreddit_stats: HashMap<String, (usize, usize)> = HashMap::new();
		for counts in catalog.author_community_counts.values() {
			for (candidate, count) in counts {
				let candidate = subreddit_semantics::normalize_subreddit_name(candidate);
				if candidate.is_empty() || candidate.starts_with("u_") || *count == 0 {
					continue;
				}
				let stats = subreddit_stats.entry(candidate).or_default();
				stats.0 += 1;
				stats.1 += *count;
			}
		}
		let profile_counts_by_profile = catalog
			.author_community_counts
			.iter()
			.map(|(profile, counts)| (profile.clone(), counts.clone()))
			.collect::<HashMap<_, _>>();
		(subreddit_stats, profile_counts_by_profile)
	};

	let content_entries = subreddit_content_semantics::active_entries_for_model(&semantic_model);
	let content_post_counts = content_entries.iter().map(|entry| (entry.subreddit.clone(), entry.post_count)).collect::<HashMap<_, _>>();
	let mut candidate_subreddits = subreddit_stats.keys().cloned().collect::<HashSet<_>>();
	candidate_subreddits.extend(followed_subreddits.iter().cloned());
	candidate_subreddits.extend(content_entries.into_iter().map(|entry| entry.subreddit));
	candidate_subreddits.retain(|candidate| candidate != &subreddit && !candidate.starts_with("u_"));
	let (followed_related_subreddits, cached_related_subreddits) = rank_related_subreddits(
		&candidate_subreddits,
		metadata_embedding.as_deref(),
		&followed_subreddits,
		&normalized_filters,
		&subreddit_stats,
		&content_post_counts,
		|candidate| subreddit_semantics::embedding_for_subreddit_model(candidate, &semantic_model),
		false,
		limit,
	);
	let (vibe_followed_subreddits, vibe_cached_subreddits) = rank_related_subreddits(
		&candidate_subreddits,
		vibe_embedding.as_deref(),
		&followed_subreddits,
		&normalized_filters,
		&subreddit_stats,
		&content_post_counts,
		|candidate| subreddit_content_semantics::embedding_for_subreddit_model(candidate, &semantic_model),
		true,
		limit,
	);

	let mut profile_signal_subreddits = HashSet::from([subreddit.clone()]);
	profile_signal_subreddits.extend(followed_related_subreddits.iter().map(|entry| entry.name.clone()));
	profile_signal_subreddits.extend(cached_related_subreddits.iter().map(|entry| entry.name.clone()));
	let profile_counts_by_profile = profile_counts_by_profile
		.into_iter()
		.filter(|(_, counts)| {
			counts
				.keys()
				.map(|candidate| subreddit_semantics::normalize_subreddit_name(candidate))
				.any(|candidate| profile_signal_subreddits.contains(&candidate))
		})
		.collect::<HashMap<_, _>>();

	let mut profile_embeddings = HashMap::new();
	for counts in profile_counts_by_profile.values() {
		for (candidate, _) in sorted_subreddit_counts(counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS) {
			if profile_embeddings.contains_key(&candidate) {
				continue;
			}
			if let Some(embedding) = blended_subreddit_semantic_embedding(&candidate, &semantic_model) {
				profile_embeddings.insert(candidate, embedding);
			}
		}
	}
	let subreddit_weights = profile_embeddings.keys().map(|name| (name.clone(), 1.0)).collect::<HashMap<_, _>>();
	let mut related_profiles = profile_counts_by_profile
		.into_iter()
		.filter(|(profile, _)| !profile.is_empty() && profile != "[deleted]" && !normalized_filters.contains(&format!("u_{profile}")))
		.filter_map(|(profile, counts)| {
			let current_vector = current_vector.as_deref()?;
			let current_embedding = profile_embedding.as_deref()?;
			let profile_counts = sorted_subreddit_counts(&counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
			let cached_posts = profile_counts.iter().map(|(_, count)| *count).sum::<usize>();
			if cached_posts < SUGGESTED_PROFILE_MIN_POSTS {
				return None;
			}
			let vector = suggested_profile_semantic_vector(&counts, &subreddit_weights, &profile_embeddings, SUGGESTED_PROFILE_VECTOR_SUBREDDITS)?;
			let similarity = suggested_profile_dense_dot(&current_vector, &vector);
			if similarity < RELATED_PROFILE_MIN_SIMILARITY {
				return None;
			}
			let confidence = (cached_posts as f64 / 12.0).sqrt().min(1.0);
			Some((
				followed_profiles.contains(&profile),
				RelatedProfileEntry {
					name: profile,
					score_percent: related_score_percent(similarity * confidence * suggested_profile_promotion_quality_factor(&counts)),
					cached_posts,
					current_subreddit_posts: counts.get(&subreddit).copied().unwrap_or_default(),
					signal_subreddits: related_profile_signals(&counts, &current_embedding, &profile_embeddings, 3),
				},
			))
		})
		.collect::<Vec<_>>();
	related_profiles.sort_by(|left, right| {
		right
			.1
			.score_percent
			.cmp(&left.1.score_percent)
			.then_with(|| right.1.current_subreddit_posts.cmp(&left.1.current_subreddit_posts))
			.then_with(|| right.1.cached_posts.cmp(&left.1.cached_posts))
			.then_with(|| left.1.name.cmp(&right.1.name))
	});
	let mut followed_related_profiles = Vec::new();
	let mut cached_related_profiles = Vec::new();
	for (followed, entry) in related_profiles {
		if followed {
			if followed_related_profiles.len() < limit {
				followed_related_profiles.push(entry);
			}
		} else if cached_related_profiles.len() < limit {
			cached_related_profiles.push(entry);
		}
		if followed_related_profiles.len() >= limit && cached_related_profiles.len() >= limit {
			break;
		}
	}

	RelatedSubredditReport {
		semantic_embedding_count: subreddit_semantics::active_embedding_count(),
		vibe_embedding_count: subreddit_content_semantics::active_usable_embedding_count(),
		followed_subreddits: followed_related_subreddits,
		cached_subreddits: cached_related_subreddits,
		vibe_followed_subreddits,
		vibe_cached_subreddits,
		followed_profiles: followed_related_profiles,
		cached_profiles: cached_related_profiles,
	}
}

#[cfg(test)]
pub(crate) fn suggested_profiles(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	limit: usize,
) -> SuggestedProfilesReport {
	suggested_profiles_with_mode(
		followed_profiles,
		followed_profile_subreddits,
		excluded_profiles,
		limit,
		SuggestedProfileScoringMode::Overlap,
	)
}

#[cfg(test)]
pub(crate) fn suggested_profiles_with_mode(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	limit: usize,
	mode: SuggestedProfileScoringMode,
) -> SuggestedProfilesReport {
	let feedback = HashMap::new();
	suggested_profiles_with_mode_and_feedback(followed_profiles, followed_profile_subreddits, excluded_profiles, &feedback, limit, mode)
}

#[cfg(test)]
pub(crate) fn suggested_profiles_with_mode_and_feedback(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	author_feedback: &HashMap<(String, String), PostFeedbackSummary>,
	limit: usize,
	mode: SuggestedProfileScoringMode,
) -> SuggestedProfilesReport {
	suggested_profiles_with_mode_center_and_feedback(followed_profiles, followed_profile_subreddits, excluded_profiles, author_feedback, limit, mode, None)
}

#[cfg(test)]
pub(crate) fn suggested_profiles_with_mode_center_and_feedback(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	author_feedback: &HashMap<(String, String), PostFeedbackSummary>,
	limit: usize,
	mode: SuggestedProfileScoringMode,
	vector_center: Option<&SuggestedProfileVectorCenterRequest>,
) -> SuggestedProfilesReport {
	suggested_profiles_with_mode_center_mix_and_feedback(
		followed_profiles,
		followed_profile_subreddits,
		excluded_profiles,
		author_feedback,
		limit,
		mode,
		vector_center,
		SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT,
	)
}

pub(crate) fn suggested_profiles_with_mode_center_mix_and_feedback(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	author_feedback: &HashMap<(String, String), PostFeedbackSummary>,
	limit: usize,
	mode: SuggestedProfileScoringMode,
	vector_center: Option<&SuggestedProfileVectorCenterRequest>,
	semantic_mix_percent: u8,
) -> SuggestedProfilesReport {
	match mode {
		SuggestedProfileScoringMode::Overlap => suggested_profiles_overlap(followed_profiles, followed_profile_subreddits, excluded_profiles, author_feedback, limit),
		SuggestedProfileScoringMode::Vector => {
			suggested_profiles_vector_mode(followed_profiles, followed_profile_subreddits, excluded_profiles, author_feedback, limit, vector_center)
		}
		SuggestedProfileScoringMode::Semantic => suggested_profiles_semantic_mode(
			followed_profiles,
			followed_profile_subreddits,
			excluded_profiles,
			author_feedback,
			limit,
			vector_center,
			semantic_mix_percent,
		),
	}
}

fn suggested_profiles_overlap(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	author_feedback: &HashMap<(String, String), PostFeedbackSummary>,
	limit: usize,
) -> SuggestedProfilesReport {
	let followed_profiles = followed_profiles
		.iter()
		.map(|name| normalize_profile_name(name))
		.filter(|name| !name.is_empty())
		.collect::<HashSet<_>>();
	let excluded_profiles = excluded_profiles
		.iter()
		.map(|name| normalize_profile_name(name))
		.filter(|name| !name.is_empty())
		.collect::<HashSet<_>>();
	let followed_profile_identities = followed_profiles
		.iter()
		.map(|profile| crate::utils::creator_identity_fingerprint(profile))
		.filter(|profile| !profile.is_empty())
		.collect::<HashSet<_>>();
	if followed_profiles.is_empty() || limit == 0 {
		let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
		return SuggestedProfilesReport {
			followed_profile_count: followed_profiles.len(),
			cached_profile_count: catalog.author_community_counts.len(),
			..SuggestedProfilesReport::default()
		};
	}

	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let mut followed_counts: HashMap<String, usize> = HashMap::new();
	let mut followed_profile_counts: HashMap<String, usize> = HashMap::new();
	let mut followed_profiles_with_cache = 0usize;

	for profile in &followed_profiles {
		let mut counts = catalog.author_community_counts.get(profile).cloned().unwrap_or_default();
		if let Some(cached_subreddits) = followed_profile_subreddits.get(profile) {
			for (subreddit, count) in cached_subreddits {
				let subreddit = subreddit.trim().to_lowercase();
				if subreddit.is_empty() || subreddit.starts_with("u_") || *count == 0 {
					continue;
				}
				let count = (*count) as usize;
				let entry = counts.entry(subreddit).or_default();
				*entry = (*entry).max(count);
			}
		}
		let profile_counts = sorted_subreddit_counts(&counts, SUGGESTED_PROFILE_TOP_SUBREDDITS);
		if profile_counts.is_empty() {
			continue;
		}
		followed_profiles_with_cache += 1;
		for (subreddit, count) in profile_counts {
			*followed_profile_counts.entry(subreddit.clone()).or_default() += 1;
			*followed_counts.entry(subreddit).or_default() += count;
		}
	}

	let followed_top = sorted_subreddit_counts(&followed_counts, SUGGESTED_PROFILE_TOP_SUBREDDITS);
	let followed_weights = followed_top
		.iter()
		.map(|(subreddit, count)| {
			let source_count = *followed_profile_counts.get(subreddit).unwrap_or(&0);
			let source_factor = suggested_profile_source_factor(subreddit, &followed_profiles, &followed_profile_identities, source_count);
			(subreddit.clone(), suggested_profile_weight(*count) * source_factor)
		})
		.collect::<HashMap<_, _>>();
	let followed_norm = followed_weights.values().map(|weight| weight * weight).sum::<f64>().sqrt();
	let followed_subreddit_counts = followed_counts.clone();
	let followed_subreddit_profile_counts = followed_profile_counts.clone();

	let mut candidate_counts_by_profile: HashMap<String, HashMap<String, usize>> = HashMap::new();
	let mut cached_profile_names = catalog.author_community_counts.keys().cloned().collect::<HashSet<_>>();

	for (profile, counts) in &catalog.author_community_counts {
		if followed_profiles.contains(profile) || excluded_profiles.contains(profile) {
			continue;
		}

		let profile_counts = sorted_subreddit_counts(counts, SUGGESTED_PROFILE_TOP_SUBREDDITS);
		if profile_counts.iter().all(|(subreddit, _)| !followed_weights.contains_key(subreddit)) {
			continue;
		}

		let candidate_counts = candidate_counts_by_profile.entry(profile.clone()).or_default();
		for (subreddit, count) in profile_counts {
			let entry = candidate_counts.entry(subreddit).or_default();
			*entry = (*entry).max(count);
		}
	}
	for ((model, profile), summary) in author_feedback {
		if model != POST_FEEDBACK_MODEL_MANUAL || !suggested_profile_has_positive_feedback(Some(summary)) {
			continue;
		}
		let profile = normalize_profile_name(profile);
		if profile.is_empty() || followed_profiles.contains(&profile) || excluded_profiles.contains(&profile) {
			continue;
		}
		let Some(counts) = catalog.author_community_counts.get(&profile) else {
			continue;
		};
		let candidate_counts = candidate_counts_by_profile.entry(profile).or_default();
		for (subreddit, count) in sorted_subreddit_counts(counts, SUGGESTED_PROFILE_TOP_SUBREDDITS) {
			let entry = candidate_counts.entry(subreddit).or_default();
			*entry = (*entry).max(count);
		}
	}
	drop(catalog);

	for (subreddit, _) in &followed_top {
		for (profile, count) in crate::utils::cached_subreddit_users(subreddit).into_iter().take(SUGGESTED_PROFILE_SUBREDDIT_USER_LIMIT) {
			let profile = normalize_profile_name(&profile);
			if profile.is_empty() || profile == "[deleted]" {
				continue;
			}
			cached_profile_names.insert(profile.clone());
			if followed_profiles.contains(&profile) || excluded_profiles.contains(&profile) || count == 0 {
				continue;
			}

			let subreddit_counts = candidate_counts_by_profile.entry(profile).or_default();
			let entry = subreddit_counts.entry(subreddit.clone()).or_default();
			*entry = (*entry).max(count as usize);
		}
	}

	let mut suggestions = Vec::new();
	let mut compared_profile_count = 0usize;

	if followed_norm > 0.0 {
		for (profile, counts) in candidate_counts_by_profile {
			if suggested_profile_is_rejected_for_redgifs_profile_count(&profile) {
				continue;
			}
			let profile_counts = sorted_subreddit_counts(&counts, SUGGESTED_PROFILE_TOP_SUBREDDITS);
			let cached_posts = profile_counts.iter().map(|(_, count)| *count).sum::<usize>();
			if cached_posts < SUGGESTED_PROFILE_MIN_POSTS {
				continue;
			}
			let feedback_summary = manual_post_feedback_summary(author_feedback, &profile);
			let has_positive_feedback = suggested_profile_has_positive_feedback(feedback_summary);

			compared_profile_count += 1;

			let mut dot = 0.0;
			let mut candidate_norm_sq = 0.0;
			let mut matched_posts = 0usize;
			let mut overlap_subreddits = Vec::new();

			for (subreddit, count) in &profile_counts {
				let candidate_weight = suggested_profile_weight(*count);
				candidate_norm_sq += candidate_weight * candidate_weight;
				let Some(followed_weight) = followed_weights.get(subreddit) else {
					continue;
				};
				dot += followed_weight * candidate_weight;
				matched_posts += *count;
				overlap_subreddits.push(SuggestedProfileSubreddit {
					name: subreddit.clone(),
					followed_count: *followed_subreddit_counts.get(subreddit).unwrap_or(&0),
					followed_profile_count: *followed_subreddit_profile_counts.get(subreddit).unwrap_or(&0),
					profile_count: *count,
				});
			}

			if candidate_norm_sq <= 0.0 || ((dot <= 0.0 || matched_posts == 0) && !has_positive_feedback) {
				continue;
			}

			let overlap_count = overlap_subreddits.len();
			overlap_subreddits.sort_by(|left, right| {
				let left_contribution = left.followed_count.saturating_mul(left.profile_count).saturating_mul(left.followed_profile_count.max(1));
				let right_contribution = right.followed_count.saturating_mul(right.profile_count).saturating_mul(right.followed_profile_count.max(1));
				right_contribution.cmp(&left_contribution).then_with(|| left.name.cmp(&right.name))
			});
			overlap_subreddits.truncate(SUGGESTED_PROFILE_REASON_LIMIT);

			let similarity = dot / (followed_norm * candidate_norm_sq.sqrt());
			let confidence = (cached_posts as f64 / 8.0).min(1.0);
			let score = suggested_profile_feedback_adjusted_score(
				similarity * confidence * suggested_profile_overlap_factor(overlap_count) * suggested_profile_promotion_quality_factor(&counts),
				feedback_summary,
			);
			let score_percent = (score * 100.0).round().clamp(1.0, 100.0) as u32;

			suggestions.push(SuggestedProfileEntry {
				name: profile.clone(),
				score_percent,
				cached_posts,
				matched_posts,
				avg_video_length: None,
				video_count: 0,
				redgifs_profile_count: 0,
				media_posts: Vec::new(),
				overlap_subreddits,
			});
		}
	}

	suggestions.sort_by(|left, right| {
		right
			.score_percent
			.cmp(&left.score_percent)
			.then_with(|| right.matched_posts.cmp(&left.matched_posts))
			.then_with(|| right.cached_posts.cmp(&left.cached_posts))
			.then_with(|| left.name.cmp(&right.name))
	});
	suggestions.truncate(limit);

	decorate_suggested_profiles(&mut suggestions);

	SuggestedProfilesReport {
		followed_profile_count: followed_profiles.len(),
		followed_profiles_with_cache,
		cached_profile_count: cached_profile_names.len(),
		compared_profile_count,
		taste_centroid_count: usize::from(followed_profiles_with_cache > 0),
		active_vector_center_label: None,
		semantic_embedding_count: 0,
		semantic_mix_percent: 0,
		vector_cluster_centers: Vec::new(),
		vector_subreddit_centers: Vec::new(),
		followed_subreddits: followed_top
			.into_iter()
			.take(SUGGESTED_PROFILE_REASON_LIMIT)
			.map(|(name, followed_count)| SuggestedProfileSubreddit {
				followed_profile_count: *followed_subreddit_profile_counts.get(&name).unwrap_or(&0),
				name,
				followed_count,
				profile_count: 0,
			})
			.collect(),
		suggestions,
	}
}

fn suggested_profiles_vector_mode(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	author_feedback: &HashMap<(String, String), PostFeedbackSummary>,
	limit: usize,
	vector_center: Option<&SuggestedProfileVectorCenterRequest>,
) -> SuggestedProfilesReport {
	let followed_profiles = followed_profiles
		.iter()
		.map(|name| normalize_profile_name(name))
		.filter(|name| !name.is_empty())
		.collect::<HashSet<_>>();
	let excluded_profiles = excluded_profiles
		.iter()
		.map(|name| normalize_profile_name(name))
		.filter(|name| !name.is_empty())
		.collect::<HashSet<_>>();
	let followed_profile_identities = followed_profiles
		.iter()
		.map(|profile| crate::utils::creator_identity_fingerprint(profile))
		.filter(|profile| !profile.is_empty())
		.collect::<HashSet<_>>();
	let mut followed_profile_names = followed_profiles.iter().cloned().collect::<Vec<_>>();
	followed_profile_names.sort();

	if followed_profiles.is_empty() || limit == 0 {
		let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
		return SuggestedProfilesReport {
			followed_profile_count: followed_profiles.len(),
			cached_profile_count: catalog.author_community_counts.len(),
			..SuggestedProfilesReport::default()
		};
	}

	let (mut profile_counts_by_profile, mut cached_profile_names) = {
		let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
		let mut profile_counts_by_profile = HashMap::new();
		for (profile, counts) in &catalog.author_community_counts {
			let counts = sorted_subreddit_counts(counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS).into_iter().collect::<HashMap<_, _>>();
			if !counts.is_empty() {
				profile_counts_by_profile.insert(profile.clone(), counts);
			}
		}
		(profile_counts_by_profile, catalog.author_community_counts.keys().cloned().collect::<HashSet<_>>())
	};

	for profile in &followed_profile_names {
		let Some(cached_subreddits) = followed_profile_subreddits.get(profile) else {
			continue;
		};
		let counts = profile_counts_by_profile.entry(profile.clone()).or_default();
		for (subreddit, count) in cached_subreddits {
			let subreddit = subreddit.trim().to_lowercase();
			if subreddit.is_empty() || subreddit.starts_with("u_") || *count == 0 {
				continue;
			}
			let entry = counts.entry(subreddit).or_default();
			*entry = (*entry).max(*count as usize);
		}
	}

	let mut followed_counts: HashMap<String, usize> = HashMap::new();
	let mut followed_profile_counts: HashMap<String, usize> = HashMap::new();
	let mut followed_profiles_with_cache = 0usize;

	for profile in &followed_profile_names {
		let Some(counts) = profile_counts_by_profile.get(profile) else {
			continue;
		};
		let profile_counts = sorted_subreddit_counts(counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
		if profile_counts.is_empty() {
			continue;
		}
		followed_profiles_with_cache += 1;
		for (subreddit, count) in profile_counts {
			*followed_profile_counts.entry(subreddit.clone()).or_default() += 1;
			*followed_counts.entry(subreddit).or_default() += count;
		}
	}

	let followed_top = sorted_subreddit_counts(&followed_counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);

	for (subreddit, _) in &followed_top {
		for (profile, count) in crate::utils::cached_subreddit_users(subreddit).into_iter().take(SUGGESTED_PROFILE_SUBREDDIT_USER_LIMIT) {
			let profile = normalize_profile_name(&profile);
			if profile.is_empty() || profile == "[deleted]" {
				continue;
			}
			cached_profile_names.insert(profile.clone());
			if followed_profiles.contains(&profile) || excluded_profiles.contains(&profile) || count == 0 {
				continue;
			}

			let subreddit_counts = profile_counts_by_profile.entry(profile).or_default();
			let entry = subreddit_counts.entry(subreddit.clone()).or_default();
			*entry = (*entry).max(count as usize);
		}
	}

	let mut subreddit_profile_counts: HashMap<String, usize> = HashMap::new();
	for counts in profile_counts_by_profile.values() {
		for (subreddit, _) in sorted_subreddit_counts(counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS) {
			*subreddit_profile_counts.entry(subreddit).or_default() += 1;
		}
	}

	let total_profiles = profile_counts_by_profile.len().max(1);
	let subreddit_weights = subreddit_profile_counts
		.iter()
		.map(|(subreddit, subreddit_profile_count)| {
			let source_count = *followed_profile_counts.get(subreddit).unwrap_or(&0);
			let source_factor = if source_count > 0 {
				suggested_profile_source_factor(subreddit, &followed_profiles, &followed_profile_identities, source_count)
			} else {
				1.0
			};
			(subreddit.clone(), suggested_profile_vector_idf(total_profiles, *subreddit_profile_count) * source_factor)
		})
		.collect::<HashMap<_, _>>();

	let mut followed_vectors = Vec::new();
	for profile in &followed_profile_names {
		let Some(counts) = profile_counts_by_profile.get(profile) else {
			continue;
		};
		let mut vector = suggested_profile_vector(counts, &subreddit_weights, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
		if normalize_suggested_profile_vector(&mut vector) <= 0.0 {
			continue;
		}
		followed_vectors.push(vector);
	}
	let centroids = suggested_profile_taste_centroids(&followed_vectors);
	let mut active_vector_center_label = None;
	let mut active_vector_center_id = None;
	let active_vector_center = match vector_center {
		Some(SuggestedProfileVectorCenterRequest::Cluster(index)) => centroids.get(*index).map(|centroid| {
			let request = SuggestedProfileVectorCenterRequest::Cluster(*index);
			active_vector_center_id = Some(request.id());
			active_vector_center_label = Some(suggested_profile_cluster_center_label(*index, centroids.len()));
			centroid.clone()
		}),
		Some(SuggestedProfileVectorCenterRequest::Subreddit(subreddit)) => {
			let subreddit = normalize_vector_center_subreddit_name(subreddit);
			if subreddit.is_empty() {
				None
			} else {
				let request = SuggestedProfileVectorCenterRequest::Subreddit(subreddit.clone());
				active_vector_center_id = Some(request.id());
				active_vector_center_label = Some(format!("r/{subreddit}"));
				Some(HashMap::from([(subreddit, 1.0)]))
			}
		}
		None => None,
	};
	let (vector_cluster_centers, vector_subreddit_centers) = suggested_profile_vector_center_options(&centroids, &followed_top, active_vector_center_id.as_deref());

	let followed_subreddit_counts = followed_counts.clone();
	let followed_subreddit_profile_counts = followed_profile_counts.clone();
	let mut suggestions = Vec::new();
	let mut compared_profile_count = 0usize;

	if !centroids.is_empty() {
		for (profile, counts) in profile_counts_by_profile {
			if followed_profiles.contains(&profile) || excluded_profiles.contains(&profile) || profile == "[deleted]" {
				continue;
			}
			if suggested_profile_is_rejected_for_redgifs_profile_count(&profile) {
				continue;
			}

			let profile_counts = sorted_subreddit_counts(&counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
			let cached_posts = profile_counts.iter().map(|(_, count)| *count).sum::<usize>();
			if cached_posts < SUGGESTED_PROFILE_MIN_POSTS {
				continue;
			}
			let feedback_summary = manual_post_feedback_summary(author_feedback, &profile);
			let has_positive_feedback = suggested_profile_has_positive_feedback(feedback_summary);

			let mut candidate_vector = suggested_profile_vector(&counts, &subreddit_weights, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
			if normalize_suggested_profile_vector(&mut candidate_vector) <= 0.0 {
				continue;
			}
			compared_profile_count += 1;

			let (similarity, best_centroid) = if let Some(center) = active_vector_center.as_ref() {
				(suggested_profile_dot(center, &candidate_vector), center)
			} else {
				centroids
					.iter()
					.map(|centroid| (suggested_profile_dot(centroid, &candidate_vector), centroid))
					.max_by(|(left, _), (right, _)| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
					.unwrap_or((0.0, &centroids[0]))
			};
			if similarity <= 0.0 && !has_positive_feedback {
				continue;
			}

			let mut matched_posts = 0usize;
			let mut overlap_subreddits = Vec::new();
			for (subreddit, count) in &profile_counts {
				if !best_centroid.contains_key(subreddit) {
					continue;
				}
				matched_posts += *count;
				overlap_subreddits.push(SuggestedProfileSubreddit {
					name: subreddit.clone(),
					followed_count: *followed_subreddit_counts.get(subreddit).unwrap_or(&0),
					followed_profile_count: *followed_subreddit_profile_counts.get(subreddit).unwrap_or(&0),
					profile_count: *count,
				});
			}

			if matched_posts == 0 && !has_positive_feedback {
				continue;
			}

			let overlap_count = overlap_subreddits.len();
			overlap_subreddits.sort_by(|left, right| {
				let left_contribution = left.followed_count.saturating_mul(left.profile_count).saturating_mul(left.followed_profile_count.max(1));
				let right_contribution = right.followed_count.saturating_mul(right.profile_count).saturating_mul(right.followed_profile_count.max(1));
				right_contribution.cmp(&left_contribution).then_with(|| left.name.cmp(&right.name))
			});
			overlap_subreddits.truncate(SUGGESTED_PROFILE_REASON_LIMIT);

			let confidence = (cached_posts as f64 / 12.0).sqrt().min(1.0);
			let score = suggested_profile_feedback_adjusted_score(
				similarity * confidence * suggested_profile_vector_breadth_factor(overlap_count) * suggested_profile_promotion_quality_factor(&counts),
				feedback_summary,
			);
			let score_percent = (score * 100.0).round().clamp(1.0, 100.0) as u32;

			suggestions.push(SuggestedProfileEntry {
				name: profile.clone(),
				score_percent,
				cached_posts,
				matched_posts,
				avg_video_length: None,
				video_count: 0,
				redgifs_profile_count: 0,
				media_posts: Vec::new(),
				overlap_subreddits,
			});
		}
	}

	suggestions.sort_by(|left, right| {
		right
			.score_percent
			.cmp(&left.score_percent)
			.then_with(|| right.matched_posts.cmp(&left.matched_posts))
			.then_with(|| right.cached_posts.cmp(&left.cached_posts))
			.then_with(|| left.name.cmp(&right.name))
	});
	suggestions.truncate(limit);
	decorate_suggested_profiles(&mut suggestions);

	SuggestedProfilesReport {
		followed_profile_count: followed_profiles.len(),
		followed_profiles_with_cache,
		cached_profile_count: cached_profile_names.len(),
		compared_profile_count,
		taste_centroid_count: centroids.len(),
		active_vector_center_label,
		semantic_embedding_count: 0,
		semantic_mix_percent: 0,
		vector_cluster_centers,
		vector_subreddit_centers,
		followed_subreddits: followed_top
			.into_iter()
			.take(SUGGESTED_PROFILE_REASON_LIMIT)
			.map(|(name, followed_count)| SuggestedProfileSubreddit {
				followed_profile_count: *followed_subreddit_profile_counts.get(&name).unwrap_or(&0),
				name,
				followed_count,
				profile_count: 0,
			})
			.collect(),
		suggestions,
	}
}

fn suggested_profiles_semantic_mode(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	author_feedback: &HashMap<(String, String), PostFeedbackSummary>,
	limit: usize,
	vector_center: Option<&SuggestedProfileVectorCenterRequest>,
	semantic_mix_percent: u8,
) -> SuggestedProfilesReport {
	let semantic_mix_percent = semantic_mix_percent.min(100);
	if let Err(error) = subreddit_semantics::refresh_persisted_embeddings_sidecar() {
		warn!("suggested profiles: failed to refresh subreddit semantic embeddings sidecar: {error}");
	}
	if let Err(error) = subreddit_content_semantics::refresh_persisted_embeddings_sidecar() {
		warn!("suggested profiles: failed to refresh subreddit content semantic embeddings sidecar: {error}");
	}
	let followed_profiles = followed_profiles
		.iter()
		.map(|name| normalize_profile_name(name))
		.filter(|name| !name.is_empty())
		.collect::<HashSet<_>>();
	let excluded_profiles = excluded_profiles
		.iter()
		.map(|name| normalize_profile_name(name))
		.filter(|name| !name.is_empty())
		.collect::<HashSet<_>>();
	let followed_profile_identities = followed_profiles
		.iter()
		.map(|profile| crate::utils::creator_identity_fingerprint(profile))
		.filter(|profile| !profile.is_empty())
		.collect::<HashSet<_>>();
	let mut followed_profile_names = followed_profiles.iter().cloned().collect::<Vec<_>>();
	followed_profile_names.sort();

	if followed_profiles.is_empty() || limit == 0 {
		let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
		return SuggestedProfilesReport {
			followed_profile_count: followed_profiles.len(),
			cached_profile_count: catalog.author_community_counts.len(),
			semantic_embedding_count: active_semantic_embedding_count(),
			semantic_mix_percent,
			..SuggestedProfilesReport::default()
		};
	}

	let (mut profile_counts_by_profile, mut cached_profile_names) = {
		let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
		let mut profile_counts_by_profile = HashMap::new();
		for (profile, counts) in &catalog.author_community_counts {
			let counts = sorted_subreddit_counts(counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS).into_iter().collect::<HashMap<_, _>>();
			if !counts.is_empty() {
				profile_counts_by_profile.insert(profile.clone(), counts);
			}
		}
		(profile_counts_by_profile, catalog.author_community_counts.keys().cloned().collect::<HashSet<_>>())
	};

	for profile in &followed_profile_names {
		let Some(cached_subreddits) = followed_profile_subreddits.get(profile) else {
			continue;
		};
		let counts = profile_counts_by_profile.entry(profile.clone()).or_default();
		for (subreddit, count) in cached_subreddits {
			let subreddit = subreddit.trim().to_lowercase();
			if subreddit.is_empty() || subreddit.starts_with("u_") || *count == 0 {
				continue;
			}
			let entry = counts.entry(subreddit).or_default();
			*entry = (*entry).max(*count as usize);
		}
	}

	let mut followed_counts: HashMap<String, usize> = HashMap::new();
	let mut followed_profile_counts: HashMap<String, usize> = HashMap::new();
	let mut followed_profiles_with_cache = 0usize;

	for profile in &followed_profile_names {
		let Some(counts) = profile_counts_by_profile.get(profile) else {
			continue;
		};
		let profile_counts = sorted_subreddit_counts(counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
		if profile_counts.is_empty() {
			continue;
		}
		followed_profiles_with_cache += 1;
		for (subreddit, count) in profile_counts {
			*followed_profile_counts.entry(subreddit.clone()).or_default() += 1;
			*followed_counts.entry(subreddit).or_default() += count;
		}
	}

	let followed_top = sorted_subreddit_counts(&followed_counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);

	for (subreddit, _) in &followed_top {
		for (profile, count) in crate::utils::cached_subreddit_users(subreddit).into_iter().take(SUGGESTED_PROFILE_SUBREDDIT_USER_LIMIT) {
			let profile = normalize_profile_name(&profile);
			if profile.is_empty() || profile == "[deleted]" {
				continue;
			}
			cached_profile_names.insert(profile.clone());
			if followed_profiles.contains(&profile) || excluded_profiles.contains(&profile) || count == 0 {
				continue;
			}

			let subreddit_counts = profile_counts_by_profile.entry(profile).or_default();
			let entry = subreddit_counts.entry(subreddit.clone()).or_default();
			*entry = (*entry).max(count as usize);
		}
	}

	let mut subreddit_profile_counts: HashMap<String, usize> = HashMap::new();
	for counts in profile_counts_by_profile.values() {
		for (subreddit, _) in sorted_subreddit_counts(counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS) {
			*subreddit_profile_counts.entry(subreddit).or_default() += 1;
		}
	}

	let total_profiles = profile_counts_by_profile.len().max(1);
	let subreddit_weights = subreddit_profile_counts
		.iter()
		.map(|(subreddit, subreddit_profile_count)| {
			let source_count = *followed_profile_counts.get(subreddit).unwrap_or(&0);
			let source_factor = if source_count > 0 {
				suggested_profile_source_factor(subreddit, &followed_profiles, &followed_profile_identities, source_count)
			} else {
				1.0
			};
			(subreddit.clone(), suggested_profile_vector_idf(total_profiles, *subreddit_profile_count) * source_factor)
		})
		.collect::<HashMap<_, _>>();

	let semantic_model = subreddit_semantics::embedding_model();
	let mut semantic_embeddings = HashMap::new();
	for subreddit in subreddit_profile_counts.keys() {
		if let Some(embedding) = blended_subreddit_semantic_embedding(subreddit, &semantic_model) {
			semantic_embeddings.insert(subreddit.clone(), embedding);
		}
	}
	if let Some(SuggestedProfileVectorCenterRequest::Subreddit(subreddit)) = vector_center {
		let subreddit = normalize_vector_center_subreddit_name(subreddit);
		if !semantic_embeddings.contains_key(&subreddit) {
			if let Some(embedding) = blended_subreddit_semantic_embedding(&subreddit, &semantic_model) {
				semantic_embeddings.insert(subreddit, embedding);
			}
		}
	}
	let semantic_embedding_count = semantic_embeddings.len();

	let mut followed_vectors = Vec::new();
	let mut followed_semantic_vectors = Vec::new();
	for profile in &followed_profile_names {
		let Some(counts) = profile_counts_by_profile.get(profile) else {
			continue;
		};
		let mut vector = suggested_profile_vector(counts, &subreddit_weights, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
		if normalize_suggested_profile_vector(&mut vector) <= 0.0 {
			continue;
		}
		let semantic_vector = suggested_profile_semantic_vector(counts, &subreddit_weights, &semantic_embeddings, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
		followed_vectors.push(vector);
		followed_semantic_vectors.push(semantic_vector);
	}
	let centroids = suggested_profile_taste_centroids(&followed_vectors);
	let assignments = suggested_profile_vector_assignments(&followed_vectors, &centroids);
	let semantic_centroids = suggested_profile_dense_centroids_for_assignments(&followed_semantic_vectors, &assignments, centroids.len());

	let mut active_vector_center_label = None;
	let mut active_vector_center_id = None;
	let active_vector_center = match vector_center {
		Some(SuggestedProfileVectorCenterRequest::Cluster(index)) => centroids.get(*index).map(|centroid| {
			let request = SuggestedProfileVectorCenterRequest::Cluster(*index);
			active_vector_center_id = Some(request.id());
			active_vector_center_label = Some(suggested_profile_cluster_center_label(*index, centroids.len()));
			(*index, centroid.clone(), semantic_centroids.get(*index).cloned().flatten())
		}),
		Some(SuggestedProfileVectorCenterRequest::Subreddit(subreddit)) => {
			let subreddit = normalize_vector_center_subreddit_name(subreddit);
			if subreddit.is_empty() {
				None
			} else {
				let request = SuggestedProfileVectorCenterRequest::Subreddit(subreddit.clone());
				active_vector_center_id = Some(request.id());
				active_vector_center_label = Some(format!("r/{subreddit}"));
				let semantic_center = semantic_embeddings
					.get(&subreddit)
					.map(|embedding| embedding.iter().map(|value| *value as f64).collect::<Vec<_>>());
				Some((usize::MAX, HashMap::from([(subreddit, 1.0)]), semantic_center))
			}
		}
		None => None,
	};
	let (vector_cluster_centers, vector_subreddit_centers) = suggested_profile_vector_center_options(&centroids, &followed_top, active_vector_center_id.as_deref());

	let followed_subreddit_counts = followed_counts.clone();
	let followed_subreddit_profile_counts = followed_profile_counts.clone();
	let mut suggestions = Vec::new();
	let mut compared_profile_count = 0usize;

	if !centroids.is_empty() {
		for (profile, counts) in profile_counts_by_profile {
			if followed_profiles.contains(&profile) || excluded_profiles.contains(&profile) || profile == "[deleted]" {
				continue;
			}
			if suggested_profile_is_rejected_for_redgifs_profile_count(&profile) {
				continue;
			}

			let profile_counts = sorted_subreddit_counts(&counts, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
			let cached_posts = profile_counts.iter().map(|(_, count)| *count).sum::<usize>();
			if cached_posts < SUGGESTED_PROFILE_MIN_POSTS {
				continue;
			}
			let feedback_summary = manual_post_feedback_summary(author_feedback, &profile);
			let has_positive_feedback = suggested_profile_has_positive_feedback(feedback_summary);

			let mut candidate_vector = suggested_profile_vector(&counts, &subreddit_weights, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
			if normalize_suggested_profile_vector(&mut candidate_vector) <= 0.0 {
				continue;
			}
			let candidate_semantic_vector = suggested_profile_semantic_vector(&counts, &subreddit_weights, &semantic_embeddings, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
			compared_profile_count += 1;

			let (similarity, best_centroid) = if let Some((_, center, semantic_center)) = active_vector_center.as_ref() {
				let vector_similarity = suggested_profile_dot(center, &candidate_vector);
				let semantic_similarity = semantic_center
					.as_ref()
					.zip(candidate_semantic_vector.as_ref())
					.map(|(center, candidate)| suggested_profile_dense_dot(center, candidate))
					.unwrap_or(0.0);
				(suggested_profile_blended_similarity(vector_similarity, semantic_similarity, semantic_mix_percent), center)
			} else {
				centroids
					.iter()
					.enumerate()
					.map(|(index, centroid)| {
						let vector_similarity = suggested_profile_dot(centroid, &candidate_vector);
						let semantic_similarity = semantic_centroids
							.get(index)
							.and_then(|center| center.as_ref())
							.zip(candidate_semantic_vector.as_ref())
							.map(|(center, candidate)| suggested_profile_dense_dot(center, candidate))
							.unwrap_or(0.0);
						(suggested_profile_blended_similarity(vector_similarity, semantic_similarity, semantic_mix_percent), centroid)
					})
					.max_by(|(left, _), (right, _)| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
					.unwrap_or((0.0, &centroids[0]))
			};
			if similarity <= 0.0 && !has_positive_feedback {
				continue;
			}

			let mut matched_posts = 0usize;
			let mut overlap_subreddits = Vec::new();
			for (subreddit, count) in &profile_counts {
				if !best_centroid.contains_key(subreddit) {
					continue;
				}
				matched_posts += *count;
				overlap_subreddits.push(SuggestedProfileSubreddit {
					name: subreddit.clone(),
					followed_count: *followed_subreddit_counts.get(subreddit).unwrap_or(&0),
					followed_profile_count: *followed_subreddit_profile_counts.get(subreddit).unwrap_or(&0),
					profile_count: *count,
				});
			}

			overlap_subreddits.sort_by(|left, right| {
				let left_contribution = left.followed_count.saturating_mul(left.profile_count).saturating_mul(left.followed_profile_count.max(1));
				let right_contribution = right.followed_count.saturating_mul(right.profile_count).saturating_mul(right.followed_profile_count.max(1));
				right_contribution.cmp(&left_contribution).then_with(|| left.name.cmp(&right.name))
			});
			overlap_subreddits.truncate(SUGGESTED_PROFILE_REASON_LIMIT);

			if overlap_subreddits.is_empty() {
				overlap_subreddits =
					suggested_profile_semantic_reason_subreddits(&profile_counts, &semantic_embeddings, &followed_subreddit_counts, &followed_subreddit_profile_counts);
				matched_posts = overlap_subreddits.iter().map(|subreddit| subreddit.profile_count).sum();
			}

			if matched_posts == 0 && !has_positive_feedback {
				continue;
			}

			let overlap_count = overlap_subreddits.len();
			let confidence = (cached_posts as f64 / 12.0).sqrt().min(1.0);
			let score = suggested_profile_feedback_adjusted_score(
				similarity * confidence * suggested_profile_vector_breadth_factor(overlap_count) * suggested_profile_promotion_quality_factor(&counts),
				feedback_summary,
			);
			let score_percent = (score * 100.0).round().clamp(1.0, 100.0) as u32;

			suggestions.push(SuggestedProfileEntry {
				name: profile.clone(),
				score_percent,
				cached_posts,
				matched_posts,
				avg_video_length: None,
				video_count: 0,
				redgifs_profile_count: 0,
				media_posts: Vec::new(),
				overlap_subreddits,
			});
		}
	}

	suggestions.sort_by(|left, right| {
		right
			.score_percent
			.cmp(&left.score_percent)
			.then_with(|| right.matched_posts.cmp(&left.matched_posts))
			.then_with(|| right.cached_posts.cmp(&left.cached_posts))
			.then_with(|| left.name.cmp(&right.name))
	});
	suggestions.truncate(limit);
	decorate_suggested_profiles(&mut suggestions);

	SuggestedProfilesReport {
		followed_profile_count: followed_profiles.len(),
		followed_profiles_with_cache,
		cached_profile_count: cached_profile_names.len(),
		compared_profile_count,
		taste_centroid_count: centroids.len(),
		active_vector_center_label,
		semantic_embedding_count,
		semantic_mix_percent,
		vector_cluster_centers,
		vector_subreddit_centers,
		followed_subreddits: followed_top
			.into_iter()
			.take(SUGGESTED_PROFILE_REASON_LIMIT)
			.map(|(name, followed_count)| SuggestedProfileSubreddit {
				followed_profile_count: *followed_subreddit_profile_counts.get(&name).unwrap_or(&0),
				name,
				followed_count,
				profile_count: 0,
			})
			.collect(),
		suggestions,
	}
}

pub(crate) fn catalog_video_stats(username: &str) -> Option<crate::user::UserVideoStatsAccumulator> {
	let username = username.trim().to_lowercase();
	if username.is_empty() {
		return None;
	}

	let catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	let stats = catalog.author_video_stats.get(&username)?.clone();

	if stats.avg_video_length().is_some() {
		Some(stats)
	} else {
		None
	}
}

fn snapshot_persisted_author_history_locked(catalog: &MaterializedCatalog) -> Vec<PersistedAuthorHistoryEntry> {
	let mut entries = catalog
		.author_history
		.iter()
		.filter(|(author, posts)| !author.trim().is_empty() && !posts.is_empty())
		.map(|(author, posts)| {
			let mut posts = posts.values().cloned().collect::<Vec<_>>();
			posts.sort_by(|left, right| {
				author_history_activity_ts(right)
					.cmp(&author_history_activity_ts(left))
					.then_with(|| right.post_id.cmp(&left.post_id))
			});
			PersistedAuthorHistoryEntry { author: author.clone(), posts }
		})
		.collect::<Vec<_>>();
	entries.sort_by(|left, right| left.author.cmp(&right.author));
	entries
}

fn snapshot_persisted_rejected_author_redgifs_profiles_locked(catalog: &MaterializedCatalog, now_unix: u64) -> Vec<PersistedRejectedAuthorRedgifsProfileEntry> {
	let mut entries = catalog
		.rejected_author_redgifs_profiles
		.values()
		.filter(|entry| {
			!entry.author.trim().is_empty()
				&& entry.redgifs_profile_count >= SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD
				&& now_unix.saturating_sub(entry.seen_at_unix) <= AUTHOR_HISTORY_MAX_POST_AGE_SECS
		})
		.cloned()
		.collect::<Vec<_>>();
	entries.sort_by(|left, right| left.author.cmp(&right.author));
	entries
}

fn prepare_persisted_author_state(catalog: &mut MaterializedCatalog, now_unix: u64) {
	prune_author_history_locked(catalog, now_unix);
	reject_author_history_with_many_redgifs_profiles_locked(catalog, now_unix);
	rebuild_indexes(catalog);
}

pub(crate) fn snapshot_persisted_author_state(now_unix: u64) -> (Vec<PersistedAuthorHistoryEntry>, Vec<PersistedRejectedAuthorRedgifsProfileEntry>) {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	prepare_persisted_author_state(&mut catalog, now_unix);
	(
		snapshot_persisted_author_history_locked(&catalog),
		snapshot_persisted_rejected_author_redgifs_profiles_locked(&catalog, now_unix),
	)
}

#[cfg(test)]
pub(crate) fn snapshot_persisted_author_history(now_unix: u64) -> Vec<PersistedAuthorHistoryEntry> {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	prepare_persisted_author_state(&mut catalog, now_unix);
	snapshot_persisted_author_history_locked(&catalog)
}

pub(crate) fn restore_persisted_author_history(entries: Vec<PersistedAuthorHistoryEntry>, _saved_at_unix: u64, now_unix: u64) {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	catalog.author_history.clear();

	for entry in entries {
		let author = normalize_profile_name(&entry.author);
		if author.is_empty() || author == "[deleted]" {
			continue;
		}
		if rejected_author_redgifs_profile_count_locked(&catalog, &author) >= SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD {
			continue;
		}
		let posts = catalog.author_history.entry(author).or_default();
		for post in entry.posts {
			if post.post_id.trim().is_empty() || post.subreddit.trim().is_empty() || post.subreddit.starts_with("u_") {
				continue;
			}
			posts.insert(post.post_id.clone(), post);
		}
	}

	prune_author_history_locked(&mut catalog, now_unix);
	reject_author_history_with_many_redgifs_profiles_locked(&mut catalog, now_unix);
	rebuild_indexes(&mut catalog);
}

#[cfg(test)]
pub(crate) fn snapshot_persisted_rejected_author_redgifs_profiles(now_unix: u64) -> Vec<PersistedRejectedAuthorRedgifsProfileEntry> {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	prepare_persisted_author_state(&mut catalog, now_unix);
	snapshot_persisted_rejected_author_redgifs_profiles_locked(&catalog, now_unix)
}

pub(crate) fn restore_persisted_rejected_author_redgifs_profiles(entries: Vec<PersistedRejectedAuthorRedgifsProfileEntry>, _saved_at_unix: u64, now_unix: u64) {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	catalog.rejected_author_redgifs_profiles.clear();

	for entry in entries {
		let author = normalize_profile_name(&entry.author);
		if author.is_empty() || author == "[deleted]" {
			continue;
		}
		if entry.redgifs_profile_count < SUGGESTED_PROFILE_REDGIFS_PROFILE_REJECT_THRESHOLD {
			continue;
		}
		if now_unix.saturating_sub(entry.seen_at_unix) > AUTHOR_HISTORY_MAX_POST_AGE_SECS {
			continue;
		}

		catalog.author_history.remove(&author);
		catalog.rejected_author_redgifs_profiles.insert(
			author.clone(),
			PersistedRejectedAuthorRedgifsProfileEntry {
				author,
				redgifs_profile_count: entry.redgifs_profile_count,
				seen_at_unix: entry.seen_at_unix,
			},
		);
	}

	prune_author_history_locked(&mut catalog, now_unix);
	rebuild_indexes(&mut catalog);
}

pub(crate) fn mark_deleted_reddit_media_asset(deleted_asset_key: &str) -> usize {
	let mut removed_media_posts = 0usize;
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());

	for posts in catalog.author_history.values_mut() {
		for post in posts.values_mut() {
			let references_deleted_asset = post
				.media_post
				.as_ref()
				.is_some_and(|media_post| suggested_profile_media_post_references_reddit_asset(media_post, deleted_asset_key));
			if references_deleted_asset {
				post.media_key.clear();
				post.media_post = None;
				removed_media_posts += 1;
			}
		}
	}

	if removed_media_posts > 0 {
		rebuild_indexes(&mut catalog);
		crate::cache_persistence::mark_dirty();
	}

	removed_media_posts
}

pub(crate) fn reset_source_catalog_preserving_author_history() {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	catalog.posts_by_id.clear();
	catalog.timeline_new.clear();
	catalog.source_to_post_ids.clear();
	rebuild_indexes(&mut catalog);
}

pub(crate) fn reset_mixed_feed_catalog() {
	let mut catalog = MIXED_FEED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	*catalog = MixedFeedCatalog::default();
	drop(catalog);
	let mut following = FOLLOWING_FEED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	*following = MixedFeedCatalog::default();
}

#[cfg(test)]
pub(crate) fn reset_catalog() {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	*catalog = MaterializedCatalog::default();
	drop(catalog);
	reset_mixed_feed_catalog();
	*DAEMON_DISCOVERY.lock().unwrap_or_else(|err| err.into_inner()) = FollowingDiscoveryState::default();
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
	use super::{
		apply_source_refresh, catalog_user_sidebar_stats, mark_deleted_reddit_media_asset, query_author_posts_limited, query_source_candidates, record_author_history_from_posts,
		rejected_author_redgifs_profile_count, related_subreddit_report, remove_sources, reset_catalog, restore_catalog, restore_persisted_author_history,
		restore_persisted_rejected_author_redgifs_profiles, snapshot_catalog, snapshot_persisted_author_history, snapshot_persisted_rejected_author_redgifs_profiles,
		suggested_profiles, suggested_profiles_with_mode, suggested_profiles_with_mode_and_feedback, suggested_profiles_with_mode_center_and_feedback,
		suggested_profiles_with_mode_center_mix_and_feedback, SuggestedProfileScoringMode, SuggestedProfileVectorCenterRequest,
	};
	use crate::post_cache::{FeedSort, FeedSource};
	use crate::utils::{Author, Awards, Flags, Flair, Media, ParentComment, Post, PostFeedbackSummary, POST_FEEDBACK_MODEL_MANUAL};
	use std::collections::{HashMap, HashSet};
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

	fn test_image_post(id: &str, subreddit: &str, author: &str, created_ts: u64, image_url: &str) -> Post {
		let mut post = test_post(id, subreddit, author, created_ts);
		post.post_type = "image".to_string();
		post.media.url = image_url.to_string();
		post.media.preview_url = image_url.to_string();
		post.media.width = 640;
		post.media.height = 480;
		post
	}

	fn test_video_post(id: &str, subreddit: &str, author: &str, created_ts: u64, redgifs_user: &str, duration_seconds: i64) -> Post {
		let mut post = test_post(id, subreddit, author, created_ts);
		post.post_type = "video".to_string();
		post.domain = "redgifs.com".to_string();
		post.media.url = format!("/redgifs/{id}");
		post.media.poster = format!("/redgifs/{id}-poster.jpg");
		post.media.duration_seconds = duration_seconds;
		post.media.duration = crate::utils::format_video_duration(duration_seconds);
		post.media.redgifs_username = redgifs_user.to_string();
		post
	}

	fn test_posts(prefix: &str, subreddit: &str, author: &str, count: usize, newest_ts: u64) -> Vec<Post> {
		(0..count)
			.map(|idx| test_post(&format!("{prefix}-{idx}"), subreddit, author, newest_ts.saturating_sub(idx as u64)))
			.collect()
	}

	#[test]
	fn suggested_profile_source_factor_matches_punctuated_creator_subreddit() {
		let followed_profiles = HashSet::from(["julesari__".to_string()]);
		let followed_profile_identities = followed_profiles
			.iter()
			.map(|profile| crate::utils::creator_identity_fingerprint(profile))
			.collect::<HashSet<_>>();

		assert_eq!(
			super::suggested_profile_source_factor("julesari", &followed_profiles, &followed_profile_identities, 1),
			super::SUGGESTED_PROFILE_SELF_SUBREDDIT_FACTOR
		);
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
		remove_sources(std::slice::from_ref(&flair_source));

		let posts = query_source_candidates(std::slice::from_ref(&subreddit_source), &HashSet::new());
		assert_eq!(posts.into_iter().map(|post| post.id).collect::<Vec<_>>(), vec!["one".to_string()]);

		remove_sources(std::slice::from_ref(&subreddit_source));
		assert!(query_source_candidates(&[subreddit_source], &HashSet::new()).is_empty());
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
	fn query_author_posts_limited_returns_full_posts_from_any_source() {
		let _guard = test_lock();
		reset_catalog();
		let alice_newer = test_post("alice-rust", "rust", "Alice", 300);
		let alice_older = test_post("alice-programming", "programming", "alice", 290);
		let bob = test_post("bob-rust", "rust", "bob", 280);

		record_author_history_from_posts(&[test_post("alice-history-only", "history", "alice", 400)]);
		apply_source_refresh(&FeedSource::subreddit("rust", FeedSort::New), &[alice_newer, bob]);
		apply_source_refresh(&FeedSource::subreddit("programming", FeedSort::New), &[alice_older]);

		let posts = query_author_posts_limited("u/Alice", &HashSet::new(), 10);

		assert_eq!(
			posts.into_iter().map(|post| post.id).collect::<Vec<_>>(),
			vec!["alice-rust".to_string(), "alice-programming".to_string()]
		);
		reset_catalog();
	}

	#[test]
	fn retained_author_history_survives_source_eviction_for_suggestions() {
		let _guard = test_lock();
		reset_catalog();

		let followed = vec![test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)];
		let candidate = vec![test_post("bob-rust", "rust", "bob", 280), test_post("bob-programming", "programming", "bob", 270)];
		let candidate_source = FeedSource::subreddit("rust", FeedSort::New);

		apply_source_refresh(&FeedSource::subreddit("u_alice", FeedSort::New), &followed);
		apply_source_refresh(&candidate_source, &candidate);
		apply_source_refresh(&candidate_source, &[]);

		assert!(query_source_candidates(&[candidate_source], &HashSet::new()).is_empty());
		let report = suggested_profiles(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10);

		let suggestion = report.suggestions.first().expect("retained suggestion");
		assert_eq!(suggestion.name, "bob");
		assert_eq!(suggestion.cached_posts, 2);
		assert_eq!(suggestion.matched_posts, 2);
		assert_eq!(catalog_user_sidebar_stats("bob").expect("retained sidebar stats").subreddit_counts.len(), 2);
	}

	#[test]
	fn retained_author_history_media_is_pruned_when_reddit_asset_is_deleted() {
		let _guard = test_lock();
		reset_catalog();

		apply_source_refresh(
			&FeedSource::subreddit("u_alice", FeedSort::New),
			&[test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)],
		);
		let candidate_source = FeedSource::subreddit("rust", FeedSort::New);
		apply_source_refresh(
			&candidate_source,
			&[
				test_image_post("bob-image", "rust", "bob", 280, "https://preview.redd.it/deleted.jpg?width=640&format=pjpg"),
				test_post("bob-programming", "programming", "bob", 270),
			],
		);
		apply_source_refresh(&candidate_source, &[]);

		let before = suggested_profiles(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10);
		assert_eq!(before.suggestions[0].media_posts.len(), 1);

		assert_eq!(mark_deleted_reddit_media_asset("preview.redd.it/deleted.jpg"), 1);

		let after = suggested_profiles(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10);
		assert_eq!(after.suggestions[0].name, "bob");
		assert!(after.suggestions[0].media_posts.is_empty());
		assert_eq!(after.suggestions[0].cached_posts, 2);
	}

	#[test]
	fn retained_author_history_snapshot_round_trips() {
		let _guard = test_lock();
		reset_catalog();
		record_author_history_from_posts(&[test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)]);

		let now_unix = crate::cache_persistence::unix_now();
		let snapshot = snapshot_persisted_author_history(now_unix);
		reset_catalog();
		restore_persisted_author_history(snapshot, now_unix, now_unix);

		let stats = catalog_user_sidebar_stats("alice").expect("restored retained sidebar stats");
		assert_eq!(stats.subreddit_counts, vec![("programming".to_string(), 1), ("rust".to_string(), 1)]);
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

	#[test]
	fn suggested_profiles_rank_cached_authors_by_followed_overlap() {
		let _guard = test_lock();
		reset_catalog();
		let followed = vec![
			test_post("alice-rust-1", "rust", "alice", 300),
			test_post("alice-rust-2", "rust", "alice", 290),
			test_post("alice-programming", "programming", "alice", 280),
		];
		let close_match = vec![
			test_post("bob-rust", "rust", "bob", 270),
			test_post("bob-programming", "programming", "bob", 260),
			test_post("bob-cargo", "cargo", "bob", 250),
		];
		let loose_match = vec![test_post("charlie-rust", "rust", "charlie", 240), test_post("charlie-music", "music", "charlie", 230)];

		apply_source_refresh(&FeedSource::subreddit("u_alice", FeedSort::New), &followed);
		apply_source_refresh(&FeedSource::subreddit("rust", FeedSort::New), &close_match);
		apply_source_refresh(&FeedSource::subreddit("programming", FeedSort::New), &loose_match);

		let report = suggested_profiles(&["u_alice".to_string()], &HashMap::new(), &HashSet::new(), 10);

		assert_eq!(report.followed_profile_count, 1);
		assert_eq!(report.followed_profiles_with_cache, 1);
		assert_eq!(report.suggestions[0].name, "bob");
		assert!(!report.suggestions.iter().any(|entry| entry.name == "alice"));
		assert!(report.suggestions[0].overlap_subreddits.iter().any(|entry| entry.name == "rust"));
		assert!(report.suggestions[0].overlap_subreddits.iter().any(|entry| entry.name == "programming"));
	}

	#[test]
	fn suggested_profiles_use_post_feedback_to_nudge_rank() {
		let _guard = test_lock();
		reset_catalog();

		apply_source_refresh(
			&FeedSource::subreddit("u_alice", FeedSort::New),
			&[test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)],
		);
		apply_source_refresh(
			&FeedSource::subreddit("rust", FeedSort::New),
			&[test_post("bob-rust", "rust", "bob", 280), test_post("carol-rust", "rust", "carol", 270)],
		);
		apply_source_refresh(
			&FeedSource::subreddit("programming", FeedSort::New),
			&[
				test_post("bob-programming", "programming", "bob", 260),
				test_post("carol-programming", "programming", "carol", 250),
			],
		);

		let without_feedback = suggested_profiles(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10);
		assert_eq!(without_feedback.suggestions[0].name, "bob");

		let feedback = HashMap::from([((POST_FEEDBACK_MODEL_MANUAL.to_string(), "carol".to_string()), PostFeedbackSummary { likes: 4, dislikes: 0 })]);
		let with_feedback = suggested_profiles_with_mode_and_feedback(
			&["alice".to_string()],
			&HashMap::new(),
			&HashSet::new(),
			&feedback,
			10,
			SuggestedProfileScoringMode::Overlap,
		);

		assert_eq!(with_feedback.suggestions[0].name, "carol");
		assert!(with_feedback.suggestions[0].score_percent > without_feedback.suggestions[0].score_percent);
	}

	#[test]
	fn suggested_profiles_include_positive_feedback_cached_authors() {
		let _guard = test_lock();
		reset_catalog();

		apply_source_refresh(
			&FeedSource::subreddit("u_alice", FeedSort::New),
			&[test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)],
		);
		apply_source_refresh(
			&FeedSource::subreddit("cooking", FeedSort::New),
			&[test_post("carol-cooking", "cooking", "carol", 280), test_post("carol-baking", "baking", "carol", 270)],
		);
		assert!(catalog_user_sidebar_stats("carol").is_some());

		let without_feedback = suggested_profiles(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10);
		assert!(!without_feedback.suggestions.iter().any(|suggestion| suggestion.name == "carol"));

		let feedback = HashMap::from([((POST_FEEDBACK_MODEL_MANUAL.to_string(), "carol".to_string()), PostFeedbackSummary { likes: 4, dislikes: 0 })]);
		let with_feedback = suggested_profiles_with_mode_and_feedback(
			&["alice".to_string()],
			&HashMap::new(),
			&HashSet::new(),
			&feedback,
			10,
			SuggestedProfileScoringMode::Overlap,
		);

		let carol = with_feedback.suggestions.iter().find(|suggestion| suggestion.name == "carol").expect("liked cached author");
		assert_eq!(carol.matched_posts, 0);
		assert_eq!(carol.score_percent, 25);
	}

	#[test]
	fn suggested_profiles_downrank_single_creator_subreddit_matches() {
		let _guard = test_lock();
		reset_catalog();

		apply_source_refresh(
			&FeedSource::subreddit("u_julesari__", FeedSort::New),
			&test_posts("julesari-self", "julesari", "julesari__", 39, 1_000),
		);
		for (idx, profile) in ["alice", "carol", "dave"].iter().enumerate() {
			let mut posts = test_posts(&format!("{profile}-chastity"), "chastitycouples", profile, 4, 900 - idx as u64 * 20);
			posts.extend(test_posts(&format!("{profile}-modern"), "newmoderncouples", profile, 4, 880 - idx as u64 * 20));
			apply_source_refresh(&FeedSource::subreddit(&format!("u_{profile}"), FeedSort::New), &posts);
		}
		apply_source_refresh(
			&FeedSource::subreddit("julesari", FeedSort::New),
			&test_posts("zealous", "julesari", "zealousgamblno", 14, 700),
		);
		let mut broad_match = test_posts("broad-chastity", "chastitycouples", "ensemble_match", 4, 650);
		broad_match.extend(test_posts("broad-modern", "newmoderncouples", "ensemble_match", 4, 640));
		apply_source_refresh(&FeedSource::subreddit("chastitycouples", FeedSort::New), &broad_match);

		let report = suggested_profiles(
			&["u_julesari__".to_string(), "alice".to_string(), "carol".to_string(), "dave".to_string()],
			&HashMap::new(),
			&HashSet::new(),
			10,
		);

		let broad_pos = report.suggestions.iter().position(|entry| entry.name == "ensemble_match").expect("broad match");
		let narrow_pos = report.suggestions.iter().position(|entry| entry.name == "zealousgamblno").expect("narrow match");
		let broad = &report.suggestions[broad_pos];
		let narrow = &report.suggestions[narrow_pos];

		assert!(broad_pos < narrow_pos, "{broad:?} should rank ahead of {narrow:?}");
		assert!(broad.score_percent > narrow.score_percent, "{broad:?} should score ahead of {narrow:?}");
		assert_eq!(narrow.overlap_subreddits.len(), 1);
		assert_eq!(narrow.overlap_subreddits[0].name, "julesari");
		assert_eq!(narrow.overlap_subreddits[0].followed_profile_count, 1);
	}

	#[test]
	fn suggested_profiles_vector_downweights_common_one_subreddit_matches() {
		let _guard = test_lock();
		reset_catalog();

		let mut followed = test_posts("alice-common", "common_feed", "alice", 6, 1_000);
		followed.extend(test_posts("alice-rare-one", "rare_one", "alice", 4, 980));
		followed.extend(test_posts("alice-rare-two", "rare_two", "alice", 4, 960));
		apply_source_refresh(&FeedSource::subreddit("u_alice", FeedSort::New), &followed);

		for idx in 0..8 {
			apply_source_refresh(
				&FeedSource::subreddit(&format!("u_filler_{idx}"), FeedSort::New),
				&test_posts(&format!("filler-{idx}"), "common_feed", &format!("filler_{idx}"), 4, 900 - idx),
			);
		}

		apply_source_refresh(
			&FeedSource::subreddit("common_feed", FeedSort::New),
			&test_posts("common-only", "common_feed", "common_only", 12, 700),
		);
		let mut specialist = test_posts("specialist-one", "rare_one", "rare_specialist", 2, 680);
		specialist.extend(test_posts("specialist-two", "rare_two", "rare_specialist", 2, 670));
		apply_source_refresh(&FeedSource::subreddit("rare_one", FeedSort::New), &specialist);

		let report = suggested_profiles_with_mode(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10, SuggestedProfileScoringMode::Vector);

		let specialist_pos = report.suggestions.iter().position(|entry| entry.name == "rare_specialist").expect("rare specialist");
		let common_pos = report.suggestions.iter().position(|entry| entry.name == "common_only").expect("common only");
		let specialist = &report.suggestions[specialist_pos];
		let common = &report.suggestions[common_pos];

		assert!(specialist_pos < common_pos, "{specialist:?} should rank ahead of {common:?}");
		assert!(specialist.score_percent > common.score_percent, "{specialist:?} should score ahead of {common:?}");
		assert_eq!(common.overlap_subreddits.len(), 1);
		assert_eq!(common.overlap_subreddits[0].name, "common_feed");
	}

	#[test]
	fn suggested_profiles_vector_uses_multiple_centroids_for_split_tastes() {
		let _guard = test_lock();
		reset_catalog();

		let mut followed = Vec::new();
		let mut side_cache = HashMap::new();
		for index in 0..3 {
			let profile = format!("codefollowed{index}");
			side_cache.insert(profile.clone(), vec![("rust".to_string(), 5), ("programming".to_string(), 5)]);
			followed.push(profile);
		}
		for index in 0..3 {
			let profile = format!("foodfollowed{index}");
			side_cache.insert(profile.clone(), vec![("baking".to_string(), 5), ("cooking".to_string(), 5)]);
			followed.push(profile);
		}

		let report = suggested_profiles_with_mode(&followed, &side_cache, &HashSet::new(), 10, SuggestedProfileScoringMode::Vector);

		assert_eq!(report.followed_profiles_with_cache, 6);
		assert!(report.taste_centroid_count > 1, "{report:?}");
	}

	#[test]
	fn suggested_profiles_vector_can_center_on_a_subreddit() {
		let _guard = test_lock();
		reset_catalog();

		let mut followed = Vec::new();
		let mut side_cache = HashMap::new();
		for index in 0..3 {
			let profile = format!("codefollowed{index}");
			side_cache.insert(profile.clone(), vec![("rust".to_string(), 5), ("programming".to_string(), 5)]);
			followed.push(profile);
		}
		for index in 0..3 {
			let profile = format!("foodfollowed{index}");
			side_cache.insert(profile.clone(), vec![("baking".to_string(), 5), ("cooking".to_string(), 5)]);
			followed.push(profile);
		}

		let mut rust_candidate = test_posts("rust-match-rust", "rust", "rust_match", 4, 800);
		rust_candidate.extend(test_posts("rust-match-programming", "programming", "rust_match", 4, 790));
		apply_source_refresh(&FeedSource::subreddit("rust", FeedSort::New), &rust_candidate);

		let mut baking_candidate = test_posts("baking-match-baking", "baking", "baking_match", 4, 780);
		baking_candidate.extend(test_posts("baking-match-cooking", "cooking", "baking_match", 4, 770));
		apply_source_refresh(&FeedSource::subreddit("baking", FeedSort::New), &baking_candidate);

		let report = suggested_profiles_with_mode_center_and_feedback(
			&followed,
			&side_cache,
			&HashSet::new(),
			&HashMap::new(),
			10,
			SuggestedProfileScoringMode::Vector,
			Some(&SuggestedProfileVectorCenterRequest::Subreddit("baking".to_string())),
		);

		assert_eq!(report.active_vector_center_label.as_deref(), Some("r/baking"));
		assert!(report.vector_subreddit_centers.iter().any(|center| center.id == "subreddit:baking" && center.selected));
		assert_eq!(report.suggestions[0].name, "baking_match");
		assert_eq!(
			report.suggestions[0].overlap_subreddits,
			vec![super::SuggestedProfileSubreddit {
				name: "baking".to_string(),
				followed_count: 15,
				followed_profile_count: 3,
				profile_count: 4,
			}]
		);
	}

	#[test]
	fn suggested_profiles_semantic_center_can_match_related_subreddits() {
		let _guard = test_lock();
		reset_catalog();
		crate::subreddit_semantics::clear_test_cache();
		crate::subreddit_semantics::store_embedding("baking", "nomic-embed-text", "baking-hash", vec![1.0, 0.0], 1).unwrap();
		crate::subreddit_semantics::store_embedding("sourdough", "nomic-embed-text", "sourdough-hash", vec![0.98, 0.02], 1).unwrap();
		crate::subreddit_semantics::store_embedding("rust", "nomic-embed-text", "rust-hash", vec![0.0, 1.0], 1).unwrap();

		let followed = vec!["foodfollowed".to_string()];
		let side_cache = HashMap::from([("foodfollowed".to_string(), vec![("baking".to_string(), 6)])]);

		apply_source_refresh(
			&FeedSource::subreddit("sourdough", FeedSort::New),
			&test_posts("sourdough-match", "sourdough", "sourdough_match", 6, 800),
		);
		apply_source_refresh(&FeedSource::subreddit("rust", FeedSort::New), &test_posts("rust-match", "rust", "rust_match", 6, 790));

		let report = suggested_profiles_with_mode_center_mix_and_feedback(
			&followed,
			&side_cache,
			&HashSet::new(),
			&HashMap::new(),
			10,
			SuggestedProfileScoringMode::Semantic,
			Some(&SuggestedProfileVectorCenterRequest::Subreddit("baking".to_string())),
			100,
		);

		assert_eq!(report.active_vector_center_label.as_deref(), Some("r/baking"));
		assert_eq!(report.semantic_embedding_count, 3);
		assert_eq!(report.semantic_mix_percent, 100);
		assert_eq!(report.suggestions[0].name, "sourdough_match");
		assert_eq!(report.suggestions[0].overlap_subreddits[0].name, "sourdough");
		crate::subreddit_semantics::clear_test_cache();
	}

	#[test]
	fn related_subreddit_report_splits_followed_and_cached_semantic_matches() {
		let _guard = test_lock();
		reset_catalog();
		crate::subreddit_semantics::clear_test_cache();
		crate::subreddit_semantics::store_embedding("baking", "nomic-embed-text", "baking-hash", vec![1.0, 0.0], 1).unwrap();
		crate::subreddit_semantics::store_embedding("sourdough", "nomic-embed-text", "sourdough-hash", vec![0.98, 0.02], 1).unwrap();
		crate::subreddit_semantics::store_embedding("cake", "nomic-embed-text", "cake-hash", vec![0.95, 0.05], 1).unwrap();
		crate::subreddit_semantics::store_embedding("rust", "nomic-embed-text", "rust-hash", vec![0.0, 1.0], 1).unwrap();
		crate::subreddit_content_semantics::clear_test_cache();
		crate::subreddit_content_semantics::store_embedding("baking", "nomic-embed-text", "baking-vibe-hash", vec![0.0, 1.0], 1, 12, 1, 400).unwrap();
		crate::subreddit_content_semantics::store_embedding("sourdough", "nomic-embed-text", "sourdough-vibe-hash", vec![0.9, 0.1], 1, 12, 1, 400).unwrap();
		crate::subreddit_content_semantics::store_embedding("cake", "nomic-embed-text", "cake-vibe-hash", vec![1.0, 0.0], 1, 12, 1, 400).unwrap();
		crate::subreddit_content_semantics::store_embedding("rust", "nomic-embed-text", "rust-vibe-hash", vec![0.02, 0.98], 1, 12, 1, 400).unwrap();
		crate::subreddit_content_semantics::store_embedding("u_cached_profile", "nomic-embed-text", "profile-hash", vec![1.0, 0.0], 1, 25, 1, 400).unwrap();

		apply_source_refresh(
			&FeedSource::subreddit("u_baker", FeedSort::New),
			&test_posts("baker-sourdough", "sourdough", "baker", 3, 900),
		);
		apply_source_refresh(&FeedSource::subreddit("cake", FeedSort::New), &test_posts("cake-match", "cake", "cake_author", 4, 800));
		apply_source_refresh(&FeedSource::subreddit("rust", FeedSort::New), &test_posts("rust-match", "rust", "rust_author", 4, 700));

		let report = related_subreddit_report("baking", &["sourdough".to_string(), "u_baker".to_string()], &HashSet::new(), 3);

		assert_eq!(report.followed_subreddits[0].name, "sourdough");
		assert!(report.cached_subreddits.iter().any(|entry| entry.name == "cake"));
		assert!(!report.cached_subreddits.iter().any(|entry| entry.name == "rust"));
		assert!(!report.cached_subreddits.iter().any(|entry| entry.name == "u_cached_profile"));
		assert!(report.vibe_cached_subreddits.iter().any(|entry| entry.name == "rust" && entry.cached_post_count == 12));
		assert!(!report.vibe_cached_subreddits.iter().any(|entry| entry.name == "cake"));
		assert!(report.vibe_followed_subreddits.is_empty());
		assert_eq!(report.semantic_embedding_count, 4);
		assert_eq!(report.vibe_embedding_count, 5);
		assert_eq!(report.followed_profiles[0].name, "baker");
		assert!(report.cached_profiles.iter().any(|entry| entry.name == "cake_author"));
		crate::subreddit_semantics::clear_test_cache();
		crate::subreddit_content_semantics::clear_test_cache();
	}

	#[test]
	fn suggested_profiles_respect_filtered_profiles() {
		let _guard = test_lock();
		reset_catalog();
		apply_source_refresh(
			&FeedSource::subreddit("u_alice", FeedSort::New),
			&[test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)],
		);
		apply_source_refresh(
			&FeedSource::subreddit("rust", FeedSort::New),
			&[test_post("bob-rust", "rust", "bob", 280), test_post("bob-programming", "programming", "bob", 270)],
		);

		let excluded = HashSet::from(["u_bob".to_string()]);
		let report = suggested_profiles(&["alice".to_string()], &HashMap::new(), &excluded, 10);

		assert!(report.suggestions.is_empty());
	}

	#[test]
	fn suggested_profiles_use_followed_profile_side_cache_for_seed() {
		let _guard = test_lock();
		reset_catalog();
		apply_source_refresh(
			&FeedSource::subreddit("rust", FeedSort::New),
			&[test_post("bob-rust", "rust", "bob", 280), test_post("bob-programming", "programming", "bob", 270)],
		);

		let side_cache = HashMap::from([("alice".to_string(), vec![("rust".to_string(), 4), ("programming".to_string(), 2)])]);
		let report = suggested_profiles(&["alice".to_string()], &side_cache, &HashSet::new(), 10);

		assert_eq!(report.followed_profiles_with_cache, 1);
		assert_eq!(report.suggestions[0].name, "bob");
	}

	#[test]
	fn suggested_profiles_use_subreddit_author_cache_for_candidates() {
		let _guard = test_lock();
		reset_catalog();
		crate::utils::clear_test_caches();

		let side_cache = HashMap::from([(
			"alice".to_string(),
			vec![("rare_alpha_for_suggestions".to_string(), 4), ("rare_beta_for_suggestions".to_string(), 2)],
		)]);
		crate::utils::store_subreddit_users(
			"rare_alpha_for_suggestions",
			&[
				test_post("bob-alpha-1", "rare_alpha_for_suggestions", "bob", 300),
				test_post("bob-alpha-2", "rare_alpha_for_suggestions", "bob", 290),
				test_post("alice-alpha", "rare_alpha_for_suggestions", "alice", 280),
			],
		);
		crate::utils::store_subreddit_users(
			"rare_beta_for_suggestions",
			&[
				test_post("bob-beta", "rare_beta_for_suggestions", "bob", 270),
				test_post("carol-beta", "rare_beta_for_suggestions", "carol", 260),
			],
		);

		let report = suggested_profiles(&["alice".to_string()], &side_cache, &HashSet::new(), 10);

		assert_eq!(report.followed_profiles_with_cache, 1);
		assert_eq!(report.compared_profile_count, 1);
		assert_eq!(report.suggestions[0].name, "bob");
		assert_eq!(report.suggestions[0].matched_posts, 3);
		assert!(!report.suggestions.iter().any(|entry| entry.name == "alice"));

		crate::utils::clear_test_caches();
	}

	#[test]
	fn suggested_profiles_include_cached_media_and_video_signals() {
		let _guard = test_lock();
		reset_catalog();
		crate::user::clear_test_caches();

		apply_source_refresh(
			&FeedSource::subreddit("u_alice", FeedSort::New),
			&[test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)],
		);
		apply_source_refresh(
			&FeedSource::subreddit("rust", FeedSort::New),
			&[
				test_image_post("bob-image-new", "rust", "bob", 280, "/img/bob-new.jpg"),
				test_video_post("bob-video-one", "rust", "bob", 270, "creator_one", 30),
				test_video_post("bob-video-two", "rust", "bob", 260, "creator_two", 90),
				test_image_post("bob-image-old", "rust", "bob", 250, "/img/bob-old.jpg"),
			],
		);

		let report = suggested_profiles(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10);
		let suggestion = report.suggestions.first().expect("suggestion");

		assert_eq!(suggestion.name, "bob");
		assert_eq!(suggestion.media_posts.len(), 3);
		assert_eq!(suggestion.media_posts[0].title, "Post bob-image-new");
		assert_eq!(suggestion.media_posts[0].image_url, "/img/bob-new.jpg");
		assert_eq!(suggestion.media_posts[1].media_type, "video");
		assert_eq!(suggestion.avg_video_length, Some(("1:00".to_string(), "from 2 seen videos · 2 rg profiles".to_string())));
		assert_eq!(suggestion.video_count, 2);
		assert_eq!(suggestion.redgifs_profile_count, 2);

		crate::user::clear_test_caches();
		reset_catalog();
	}

	#[test]
	fn suggested_profiles_filter_candidates_with_four_redgifs_profiles() {
		let _guard = test_lock();
		reset_catalog();
		crate::user::clear_test_caches();

		apply_source_refresh(
			&FeedSource::subreddit("u_alice", FeedSort::New),
			&[test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)],
		);
		apply_source_refresh(
			&FeedSource::subreddit("rust", FeedSort::New),
			&[
				test_video_post("bad-rust-one", "rust", "bad_rg", 280, "rg_one", 30),
				test_video_post("bad-rust-two", "rust", "bad_rg", 270, "rg_two", 35),
				test_video_post("bad-programming-three", "programming", "bad_rg", 260, "rg_three", 40),
				test_video_post("bad-programming-four", "programming", "bad_rg", 250, "rg_four", 45),
				test_post("good-rust", "rust", "good_match", 240),
				test_post("good-programming", "programming", "good_match", 230),
			],
		);

		assert_eq!(rejected_author_redgifs_profile_count("bad_rg"), 4);
		assert!(catalog_user_sidebar_stats("bad_rg").is_none());

		for mode in [SuggestedProfileScoringMode::Overlap, SuggestedProfileScoringMode::Vector] {
			let report = suggested_profiles_with_mode(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10, mode);
			assert!(report.suggestions.iter().any(|entry| entry.name == "good_match"), "{mode:?}: {report:?}");
			assert!(!report.suggestions.iter().any(|entry| entry.name == "bad_rg"), "{mode:?}: {report:?}");
		}

		apply_source_refresh(&FeedSource::subreddit("rust", FeedSort::New), &[]);
		assert!(query_source_candidates(&[FeedSource::subreddit("rust", FeedSort::New)], &HashSet::new()).is_empty());
		assert_eq!(rejected_author_redgifs_profile_count("bad_rg"), 4);
		assert!(catalog_user_sidebar_stats("bad_rg").is_none());
		assert_eq!(catalog_user_sidebar_stats("good_match").expect("good retained stats").subreddit_counts.len(), 2);

		crate::user::clear_test_caches();
		reset_catalog();
	}

	#[test]
	fn rejected_redgifs_profile_marker_snapshot_round_trips() {
		let _guard = test_lock();
		reset_catalog();

		record_author_history_from_posts(&[
			test_video_post("bad-one", "rust", "bad_rg", 280, "rg_one", 30),
			test_video_post("bad-two", "rust", "bad_rg", 270, "rg_two", 35),
			test_video_post("bad-three", "programming", "bad_rg", 260, "rg_three", 40),
			test_video_post("bad-four", "programming", "bad_rg", 250, "rg_four", 45),
		]);

		let now_unix = crate::cache_persistence::unix_now();
		let rejected_snapshot = snapshot_persisted_rejected_author_redgifs_profiles(now_unix);
		let history_snapshot = snapshot_persisted_author_history(now_unix);
		assert!(history_snapshot.iter().all(|entry| entry.author != "bad_rg"));
		assert_eq!(rejected_snapshot.len(), 1);
		assert_eq!(rejected_snapshot[0].author, "bad_rg");
		assert_eq!(rejected_snapshot[0].redgifs_profile_count, 4);

		reset_catalog();
		restore_persisted_rejected_author_redgifs_profiles(rejected_snapshot, now_unix, now_unix);
		restore_persisted_author_history(history_snapshot, now_unix, now_unix);

		assert_eq!(rejected_author_redgifs_profile_count("bad_rg"), 4);
		assert!(catalog_user_sidebar_stats("bad_rg").is_none());
		reset_catalog();
	}

	#[test]
	fn suggested_profiles_media_previews_use_distinct_media_sources() {
		let _guard = test_lock();
		reset_catalog();
		crate::user::clear_test_caches();

		apply_source_refresh(
			&FeedSource::subreddit("u_alice", FeedSort::New),
			&[test_post("alice-rust", "rust", "alice", 300), test_post("alice-programming", "programming", "alice", 290)],
		);
		apply_source_refresh(
			&FeedSource::subreddit("rust", FeedSort::New),
			&[
				test_image_post("bob-image-new", "rust", "bob", 280, "/img/shared.jpg?width=640"),
				test_image_post("bob-image-duplicate", "rust", "bob", 270, "/img/shared.jpg?width=320"),
				test_image_post("bob-image-second", "rust", "bob", 260, "/img/second.jpg"),
				test_image_post("bob-image-third", "rust", "bob", 250, "/img/third.jpg"),
			],
		);

		let report = suggested_profiles(&["alice".to_string()], &HashMap::new(), &HashSet::new(), 10);
		let suggestion = report.suggestions.first().expect("suggestion");
		let urls = suggestion.media_posts.iter().map(|post| post.image_url.as_str()).collect::<Vec<_>>();

		assert_eq!(suggestion.name, "bob");
		assert_eq!(urls, vec!["/img/shared.jpg?width=640", "/img/second.jpg", "/img/third.jpg"]);

		crate::user::clear_test_caches();
		reset_catalog();
	}
}
