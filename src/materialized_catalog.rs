#[cfg(test)]
use crate::cache_persistence::PersistedPost;
use crate::post_cache::{FeedSession, FeedSort, FeedSource, SessionSourceState};
use crate::utils::Post;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

const SUGGESTED_PROFILE_TOP_SUBREDDITS: usize = 16;
const SUGGESTED_PROFILE_VECTOR_SUBREDDITS: usize = 32;
const SUGGESTED_PROFILE_REASON_LIMIT: usize = 5;
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
const AUTHOR_HISTORY_MAX_POST_AGE_SECS: u64 = 180 * 24 * 60 * 60;
const AUTHOR_HISTORY_MAX_POSTS_PER_AUTHOR: usize = 160;
const AUTHOR_HISTORY_GLOBAL_MAX_POSTS: usize = 60_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SuggestedProfileScoringMode {
	#[default]
	Overlap,
	Vector,
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

pub(crate) fn suggested_profiles_with_mode(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
	limit: usize,
	mode: SuggestedProfileScoringMode,
) -> SuggestedProfilesReport {
	match mode {
		SuggestedProfileScoringMode::Overlap => suggested_profiles_overlap(followed_profiles, followed_profile_subreddits, excluded_profiles, limit),
		SuggestedProfileScoringMode::Vector => suggested_profiles_vector_mode(followed_profiles, followed_profile_subreddits, excluded_profiles, limit),
	}
}

fn suggested_profiles_overlap(
	followed_profiles: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	excluded_profiles: &HashSet<String>,
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

			if dot <= 0.0 || candidate_norm_sq <= 0.0 || matched_posts == 0 {
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
			let score = similarity * confidence * suggested_profile_overlap_factor(overlap_count);
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

			let mut candidate_vector = suggested_profile_vector(&counts, &subreddit_weights, SUGGESTED_PROFILE_VECTOR_SUBREDDITS);
			if normalize_suggested_profile_vector(&mut candidate_vector) <= 0.0 {
				continue;
			}
			compared_profile_count += 1;

			let (similarity, best_centroid) = centroids
				.iter()
				.map(|centroid| (suggested_profile_dot(centroid, &candidate_vector), centroid))
				.max_by(|(left, _), (right, _)| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
				.unwrap_or((0.0, &centroids[0]));
			if similarity <= 0.0 {
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

			if matched_posts == 0 {
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
			let score = similarity * confidence * suggested_profile_vector_breadth_factor(overlap_count);
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

pub(crate) fn snapshot_persisted_author_history(now_unix: u64) -> Vec<PersistedAuthorHistoryEntry> {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	prune_author_history_locked(&mut catalog, now_unix);
	reject_author_history_with_many_redgifs_profiles_locked(&mut catalog, now_unix);
	rebuild_indexes(&mut catalog);

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

pub(crate) fn snapshot_persisted_rejected_author_redgifs_profiles(now_unix: u64) -> Vec<PersistedRejectedAuthorRedgifsProfileEntry> {
	let mut catalog = MATERIALIZED_CATALOG.lock().unwrap_or_else(|err| err.into_inner());
	prune_author_history_locked(&mut catalog, now_unix);
	reject_author_history_with_many_redgifs_profiles_locked(&mut catalog, now_unix);
	rebuild_indexes(&mut catalog);

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

#[cfg(test)]
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
	use super::{
		apply_source_refresh, catalog_user_sidebar_stats, mark_deleted_reddit_media_asset, query_source_candidates, record_author_history_from_posts,
		rejected_author_redgifs_profile_count, reset_catalog, restore_catalog, restore_persisted_author_history, restore_persisted_rejected_author_redgifs_profiles,
		snapshot_catalog, snapshot_persisted_author_history, snapshot_persisted_rejected_author_redgifs_profiles, suggested_profiles, suggested_profiles_with_mode,
		SuggestedProfileScoringMode,
	};
	use crate::post_cache::{FeedSort, FeedSource};
	use crate::utils::{Author, Awards, Flags, Flair, Media, ParentComment, Post};
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
