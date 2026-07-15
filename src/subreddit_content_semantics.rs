use crate::cache_persistence;
use htmlescape::decode_html;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SUBREDDIT_CONTENT_SEMANTIC_SIDECAR_VERSION: u8 = 3;
const SUBREDDIT_CONTENT_SEMANTIC_SIDECAR_FILE_NAME: &str = "subreddit-content-semantic-embeddings.v3.bin";
const SUBREDDIT_CONTENT_SEMANTIC_SIDECAR_LOCK_FILE_NAME: &str = "subreddit-content-semantic-embeddings.v3.lock";
const SUBREDDIT_CONTENT_SEMANTIC_MUTATION_LOCK_FILE_NAME: &str = "subreddit-content-semantic-mutation.lock";
const SUBREDDIT_CONTENT_SEMANTIC_BACKFILL_STATUS_FILE_NAME: &str = "subreddit-content-semantic-backfill-status.v1.json";
const MATERIALIZED_CATALOG_FILE_NAME: &str = "materialized-catalog.v1.bin";
const DEFAULT_CONTENT_SEMANTIC_MIX_PERCENT: u8 = 35;
const DEFAULT_MAX_POSTS_PER_SUBREDDIT: usize = 16;
const DEFAULT_MAX_TEXT_CHARS_PER_POST: usize = 360;
const DEFAULT_MAX_CORPUS_TEXT_CHARS: usize = 3_000;
const DEFAULT_MIN_POSTS_PER_SUBREDDIT: usize = 6;
const DEFAULT_MIN_CORPUS_TEXT_CHARS: usize = 180;
const DEFAULT_MAX_TOP5_MEAN_SIMILARITY: f32 = 1.0;
const SIDECAR_LOCK_TIMEOUT_SECS: u64 = 30;
const SIDECAR_STALE_LOCK_SECS: u64 = 30 * 60;

static HTML_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").unwrap());
static WHITESPACE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static CONTENT_SEMANTIC_CACHE: LazyLock<Mutex<HashMap<String, PersistedSubredditContentSemanticEmbedding>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static CONTENT_SEMANTIC_SIDECAR_MTIME: LazyLock<Mutex<Option<SystemTime>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedSubredditContentSemanticEmbedding {
	pub subreddit: String,
	pub model: String,
	pub corpus_hash: String,
	pub embedding: Vec<f32>,
	pub embedded_at_unix: u64,
	pub post_count: usize,
	pub source_count: usize,
	pub text_chars: usize,
	pub vector_sum: Vec<f32>,
	pub last_processed_created_ts: u64,
	pub last_processed_post_key_hash: String,
	pub top_neighbor_similarity: Option<f32>,
	pub top5_mean_similarity: Option<f32>,
	pub samples: Vec<PersistedSubredditContentSemanticSample>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedSubredditContentSemanticSample {
	pub post_key_hash: String,
	pub text_hash: String,
	pub sort_key: u64,
	pub embedding: Vec<f32>,
	pub text_chars: usize,
	pub source_key_hash: String,
	pub observed_at_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSubredditContentSemanticEmbeddingFile {
	version: u8,
	saved_at_unix: u64,
	entries: Vec<PersistedSubredditContentSemanticEmbedding>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubredditContentSemanticBackfillCounters {
	pub source_files: usize,
	pub source_posts: usize,
	pub unique_posts: usize,
	pub total_subreddits: usize,
	pub candidate_subreddits: usize,
	pub too_small_subreddits: usize,
	pub embedded: usize,
	pub already_current: usize,
	pub failed: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SubredditContentSemanticBackfillStatus {
	pub state: String,
	pub model: String,
	pub started_at_unix: u64,
	pub finished_at_unix: Option<u64>,
	pub counters: SubredditContentSemanticBackfillCounters,
	pub dry_run: bool,
	pub force: bool,
	pub limit: Option<usize>,
	pub max_posts_per_subreddit: usize,
	pub max_text_chars_per_post: usize,
	pub max_corpus_text_chars: usize,
	pub min_posts_per_subreddit: usize,
	pub min_corpus_text_chars: usize,
	pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubredditContentSemanticDatabaseStatus {
	pub model: String,
	pub content_mix_percent: u8,
	pub sidecar_path: String,
	pub sidecar_exists: bool,
	pub sidecar_size_bytes: Option<u64>,
	pub sidecar_modified_unix: Option<u64>,
	pub sidecar_modified_age_secs: Option<u64>,
	pub sidecar_saved_at_unix: Option<u64>,
	pub sidecar_entry_count: usize,
	pub loaded_entry_count: usize,
	pub active_model_entry_count: usize,
	pub usable_model_entry_count: usize,
	pub other_model_entry_count: usize,
	pub embedding_dimensions: Option<usize>,
	pub min_posts_per_subreddit: usize,
	pub min_corpus_text_chars: usize,
	pub max_top5_mean_similarity: f32,
	pub top5_mean_similarity_average: Option<f32>,
	pub top5_mean_similarity_p95: Option<f32>,
	pub oldest_embedding_at_unix: Option<u64>,
	pub newest_embedding_at_unix: Option<u64>,
	pub refresh_error: Option<String>,
	pub sidecar_read_error: Option<String>,
	pub last_run: Option<SubredditContentSemanticBackfillStatus>,
	pub last_run_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubredditContentCorpusOptions {
	pub max_posts_per_subreddit: usize,
	pub max_text_chars_per_post: usize,
	pub max_corpus_text_chars: usize,
	pub min_posts_per_subreddit: usize,
	pub min_corpus_text_chars: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubredditContentCorpus {
	pub subreddit: String,
	pub text: String,
	pub corpus_hash: String,
	pub post_count: usize,
	pub source_count: usize,
	pub text_chars: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubredditContentCorpusStats {
	pub source_files: usize,
	pub source_posts: usize,
	pub unique_posts: usize,
	pub total_subreddits: usize,
	pub candidate_subreddits: usize,
	pub too_small_subreddits: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubredditContentCorpusCollection {
	pub corpora: Vec<SubredditContentCorpus>,
	pub stats: SubredditContentCorpusStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubredditContentPostCandidateCollection {
	pub candidates: Vec<SubredditContentPostEmbeddingCandidate>,
	pub stats: SubredditContentCorpusStats,
}

#[derive(Clone, Debug)]
struct CandidatePostText {
	text: String,
	created_ts: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubredditContentPostEmbeddingCandidate {
	pub subreddit: String,
	pub text: String,
	pub post_key_hash: String,
	pub text_hash: String,
	pub sort_key: u64,
	pub source_key_hash: String,
	pub created_ts: u64,
	pub text_chars: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct PersistedContentCatalogSnapshotFile {
	version: u8,
	saved_at_unix: u64,
	snapshot: PersistedContentCatalogSnapshot,
}

#[derive(Clone, Debug, Deserialize)]
struct PersistedContentCatalogSnapshot {
	records: Vec<PersistedContentCatalogRecord>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct PersistedContentCatalogRecord {
	post: LegacyPersistedPost,
	created_ts: u64,
	activity_ts: u64,
	community_lc: String,
	author_lc: String,
	source_keys: Vec<PersistedContentCatalogSourceKey>,
}

#[derive(Clone, Debug, Deserialize)]
enum PersistedContentCatalogSourceKey {
	Subreddit(String),
	Flair { subreddit: String, flair: String },
	UserComments(String),
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedFlairPart {
	flair_part_type: String,
	value: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedFlair {
	flair_parts: Vec<LegacyPersistedFlairPart>,
	text: String,
	background_color: String,
	foreground_color: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedExternalLink {
	label: String,
	url: String,
	host: String,
	icon_path: String,
	has_coomer: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedAuthor {
	name: String,
	flair: LegacyPersistedFlair,
	distinguished: String,
	external_links: Vec<LegacyPersistedExternalLink>,
	avg_video_length: Option<(String, String)>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedPollOption {
	id: u64,
	text: String,
	vote_count: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedPoll {
	poll_options: Vec<LegacyPersistedPollOption>,
	voting_end_timestamp: (String, String),
	total_vote_count: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedFlags {
	spoiler: bool,
	nsfw: bool,
	stickied: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedMedia {
	url: String,
	preview_url: String,
	preview_srcset: String,
	hls_url: String,
	dash_url: String,
	width: i64,
	height: i64,
	poster: String,
	duration_seconds: i64,
	duration: String,
	download_name: String,
	embed_code: String,
	video_preview_enabled: bool,
	video_preview_job_hint: String,
	video_preview_fallback_url: String,
	redgifs_username: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedGalleryMedia {
	url: String,
	preview_url: String,
	preview_srcset: String,
	width: i64,
	height: i64,
	caption: String,
	outbound_url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedAward {
	name: String,
	icon_url: String,
	description: String,
	count: i64,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedCollapsedDuplicate {
	title: String,
	permalink: String,
	community: String,
	display_community: String,
	created_ts: u64,
	rel_time: String,
	comments: (String, String),
	flair: LegacyPersistedFlair,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedParentComment {
	id: String,
	body: String,
	permalink: String,
	author: String,
	rel_time: String,
	expand: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedFollowingComment {
	id: String,
	author: String,
	body: String,
	permalink: String,
	created_ts: u64,
	rel_time: String,
	community: String,
	score: (String, String),
	parent_id: String,
	parent_kind: String,
	parent_comment: LegacyPersistedParentComment,
	link_title: String,
	link_permalink: String,
	link_author: String,
	link_created_ts: u64,
	thread_comments: (String, String),
	thread_nsfw: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct LegacyPersistedPost {
	id: String,
	kind: String,
	title: String,
	community: String,
	body: String,
	author: LegacyPersistedAuthor,
	permalink: String,
	parent_id: String,
	parent_kind: String,
	parent_comment: LegacyPersistedParentComment,
	link_id: String,
	link_title: String,
	link_permalink: String,
	link_author: String,
	link_created_ts: u64,
	poll: Option<LegacyPersistedPoll>,
	score: (String, String),
	upvote_ratio: i64,
	post_type: String,
	flair: LegacyPersistedFlair,
	flags: LegacyPersistedFlags,
	thumbnail: LegacyPersistedMedia,
	media: LegacyPersistedMedia,
	domain: String,
	rel_time: String,
	created: String,
	created_ts: u64,
	num_duplicates: u64,
	comments: (String, String),
	gallery: Vec<LegacyPersistedGalleryMedia>,
	awards: Vec<LegacyPersistedAward>,
	nsfw: bool,
	out_url: Option<String>,
	ws_url: String,
	preview_body: String,
	collapsed_duplicates: Vec<LegacyPersistedCollapsedDuplicate>,
	following_comments: Vec<LegacyPersistedFollowingComment>,
}

fn env_usize(key: &str, default: usize) -> usize {
	std::env::var(key)
		.ok()
		.and_then(|value| value.trim().parse::<usize>().ok())
		.filter(|value| *value > 0)
		.unwrap_or(default)
}

fn env_u8_percent(key: &str, default: u8) -> u8 {
	std::env::var(key)
		.ok()
		.and_then(|value| value.trim().parse::<u8>().ok())
		.map(|value| value.min(100))
		.unwrap_or(default)
}

fn env_f32(key: &str, default: f32) -> f32 {
	std::env::var(key)
		.ok()
		.and_then(|value| value.trim().parse::<f32>().ok())
		.filter(|value| value.is_finite())
		.unwrap_or(default)
}

pub fn content_mix_percent() -> u8 {
	env_u8_percent("REDLIB_SUBREDDIT_CONTENT_SEMANTIC_MIX", DEFAULT_CONTENT_SEMANTIC_MIX_PERCENT)
}

pub fn max_top5_mean_similarity() -> f32 {
	env_f32("REDLIB_SUBREDDIT_CONTENT_MAX_TOP5_MEAN_SIMILARITY", DEFAULT_MAX_TOP5_MEAN_SIMILARITY)
}

pub fn corpus_options_from_env() -> SubredditContentCorpusOptions {
	SubredditContentCorpusOptions {
		max_posts_per_subreddit: env_usize("REDLIB_SUBREDDIT_CONTENT_MAX_POSTS", DEFAULT_MAX_POSTS_PER_SUBREDDIT),
		max_text_chars_per_post: env_usize("REDLIB_SUBREDDIT_CONTENT_MAX_POST_CHARS", DEFAULT_MAX_TEXT_CHARS_PER_POST),
		max_corpus_text_chars: env_usize("REDLIB_SUBREDDIT_CONTENT_MAX_CORPUS_CHARS", DEFAULT_MAX_CORPUS_TEXT_CHARS),
		min_posts_per_subreddit: env_usize("REDLIB_SUBREDDIT_CONTENT_MIN_POSTS", DEFAULT_MIN_POSTS_PER_SUBREDDIT),
		min_corpus_text_chars: env_usize("REDLIB_SUBREDDIT_CONTENT_MIN_CHARS", DEFAULT_MIN_CORPUS_TEXT_CHARS),
	}
}

fn compact_text(value: &str) -> String {
	let without_tags = HTML_TAG_REGEX.replace_all(value, " ");
	let decoded = decode_html(&without_tags).unwrap_or_else(|_| without_tags.into_owned());
	WHITESPACE_REGEX.replace_all(decoded.trim(), " ").trim().to_string()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
	if value.chars().count() <= max_chars {
		return value.to_string();
	}
	value.chars().take(max_chars).collect()
}

fn semantic_hash(text: &str) -> String {
	let digest = Sha256::digest(text.as_bytes());
	digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sort_key_from_hash(hash: &str) -> u64 {
	hash.get(..16).and_then(|prefix| u64::from_str_radix(prefix, 16).ok()).unwrap_or(u64::MAX)
}

fn post_sample_key_hash(subreddit: &str, identity: &str) -> String {
	semantic_hash(&format!("{}:{}", crate::subreddit_semantics::normalize_subreddit_name(subreddit), identity.trim()))
}

fn normalize_embedding(embedding: &mut [f32]) -> bool {
	let norm = embedding.iter().map(|value| (*value as f64) * (*value as f64)).sum::<f64>().sqrt();
	if norm <= 0.0 {
		return false;
	}
	for value in embedding {
		*value = (*value as f64 / norm) as f32;
	}
	true
}

fn normalized_embedding(mut embedding: Vec<f32>) -> Option<Vec<f32>> {
	if embedding.is_empty() || !embedding.iter().all(|value| value.is_finite()) {
		return None;
	}
	normalize_embedding(&mut embedding).then_some(embedding)
}

fn embedding_dot(left: &[f32], right: &[f32]) -> f32 {
	if left.len() != right.len() || left.is_empty() {
		return 0.0;
	}
	left.iter().zip(right).map(|(left, right)| left * right).sum::<f32>()
}

fn is_empty_content_text(value: &str) -> bool {
	let value = value.trim();
	value.is_empty() || matches!(value.to_lowercase().as_str(), "[deleted]" | "[removed]" | "deleted" | "removed")
}

fn content_text_from_parts(title: &str, link_title: &str, body: &str, preview_body: &str, max_text_chars_per_post: usize) -> Option<String> {
	let title = compact_text(title);
	let link_title = compact_text(link_title);
	let mut body = compact_text(body);
	if body.is_empty() {
		body = compact_text(preview_body);
	}

	let mut parts = Vec::new();
	if !is_empty_content_text(&title) {
		parts.push(title.clone());
	}
	if !is_empty_content_text(&link_title) && link_title.to_lowercase() != title.to_lowercase() {
		parts.push(link_title);
	}
	if !is_empty_content_text(&body) && body.to_lowercase() != title.to_lowercase() {
		parts.push(body);
	}

	let text = compact_text(&parts.join(" | "));
	if text.is_empty() {
		None
	} else {
		Some(truncate_chars(&text, max_text_chars_per_post))
	}
}

fn content_text_for_post(post: &cache_persistence::PersistedPost, max_text_chars_per_post: usize) -> Option<String> {
	content_text_from_parts(&post.title, &post.link_title, &post.body, &post.preview_body, max_text_chars_per_post)
}

fn content_text_for_runtime_post(post: &crate::utils::Post, max_text_chars_per_post: usize) -> Option<String> {
	content_text_from_parts(&post.title, &post.link_title, &post.body, &post.preview_body, max_text_chars_per_post)
}

fn content_text_for_legacy_post(post: &LegacyPersistedPost, max_text_chars_per_post: usize) -> Option<String> {
	content_text_from_parts(&post.title, &post.link_title, &post.body, &post.preview_body, max_text_chars_per_post)
}

fn post_identity(post: &cache_persistence::PersistedPost) -> String {
	if !post.id.trim().is_empty() {
		return format!("{}:{}", post.kind.trim(), post.id.trim());
	}
	if !post.permalink.trim().is_empty() {
		return post.permalink.trim().to_string();
	}
	semantic_hash(&format!("{}:{}:{}", post.community, post.title, post.created_ts))
}

fn runtime_post_identity(post: &crate::utils::Post) -> String {
	if !post.id.trim().is_empty() {
		return format!("{}:{}", post.kind.trim(), post.id.trim());
	}
	if !post.permalink.trim().is_empty() {
		return post.permalink.trim().to_string();
	}
	semantic_hash(&format!("{}:{}:{}", post.community, post.title, post.created_ts))
}

fn legacy_post_identity(post: &LegacyPersistedPost) -> String {
	if !post.id.trim().is_empty() {
		return format!("{}:{}", post.kind.trim(), post.id.trim());
	}
	if !post.permalink.trim().is_empty() {
		return post.permalink.trim().to_string();
	}
	semantic_hash(&format!("{}:{}:{}", post.community, post.title, post.created_ts))
}

fn materialized_catalog_path() -> PathBuf {
	Path::new(&crate::cache_persistence::cache_dir()).join(MATERIALIZED_CATALOG_FILE_NAME)
}

fn read_materialized_catalog_snapshot() -> Result<Option<PersistedContentCatalogSnapshotFile>, String> {
	let path = materialized_catalog_path();
	let file = match File::open(&path) {
		Ok(file) => file,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.to_string()),
	};
	bincode::deserialize_from(BufReader::new(file))
		.map(Some)
		.map_err(|error| format!("failed to read {}: {error}", path.display()))
}

fn source_key_label(source_key: &PersistedContentCatalogSourceKey) -> String {
	match source_key {
		PersistedContentCatalogSourceKey::Subreddit(subreddit) => format!("subreddit:{}", subreddit.trim().to_lowercase()),
		PersistedContentCatalogSourceKey::Flair { subreddit, flair } => {
			format!("flair:{}:{}", subreddit.trim().to_lowercase(), flair.trim().to_lowercase())
		}
		PersistedContentCatalogSourceKey::UserComments(user) => format!("user_comments:{}", user.trim().to_lowercase()),
	}
}

fn post_embedding_candidate(subreddit: &str, identity: &str, text: String, source_key: &str, created_ts: u64) -> Option<SubredditContentPostEmbeddingCandidate> {
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(subreddit);
	if subreddit.is_empty() {
		return None;
	}
	let text = compact_text(&text);
	if text.is_empty() {
		return None;
	}
	let post_key_hash = post_sample_key_hash(&subreddit, identity);
	let text_hash = semantic_hash(&text.to_lowercase());
	let sort_key = sort_key_from_hash(&post_key_hash);
	let text_chars = text.chars().count();
	Some(SubredditContentPostEmbeddingCandidate {
		subreddit,
		text,
		post_key_hash,
		text_hash,
		sort_key,
		source_key_hash: semantic_hash(source_key),
		created_ts,
		text_chars,
	})
}

/// Build embedding candidates for a user's own aggregate vibe vector.
///
/// Ordinary runtime candidates remain grouped by the post's subreddit. User
/// pages call this explicitly so the same bounded post sample also contributes
/// to a separate `u_<username>` aggregate.
pub fn runtime_user_post_embedding_candidates(
	username: &str,
	source_key: &str,
	posts: &[crate::utils::Post],
	max_text_chars_per_post: usize,
) -> Vec<SubredditContentPostEmbeddingCandidate> {
	let username = username.trim().trim_start_matches("/u/").trim_start_matches("u/").trim();
	if username.is_empty() {
		return Vec::new();
	}
	let user_target = format!("u_{}", username.to_lowercase());
	let mut candidates_by_post = HashMap::<String, SubredditContentPostEmbeddingCandidate>::new();
	for post in posts {
		let Some(text) = content_text_for_runtime_post(post, max_text_chars_per_post) else {
			continue;
		};
		let identity = runtime_post_identity(post);
		if let Some(candidate) = post_embedding_candidate(&user_target, &identity, text, source_key, post.created_ts) {
			candidates_by_post
				.entry(candidate.post_key_hash.clone())
				.and_modify(|existing| {
					if candidate.created_ts > existing.created_ts {
						*existing = candidate.clone();
					}
				})
				.or_insert(candidate);
		}
	}

	let mut candidates = candidates_by_post.into_values().collect::<Vec<_>>();
	candidates.sort_by(|left, right| left.created_ts.cmp(&right.created_ts).then_with(|| left.post_key_hash.cmp(&right.post_key_hash)));
	candidates
}

pub fn runtime_post_embedding_candidates(source_key: &str, posts: &[crate::utils::Post], max_text_chars_per_post: usize) -> Vec<SubredditContentPostEmbeddingCandidate> {
	let mut candidates_by_post = HashMap::<String, SubredditContentPostEmbeddingCandidate>::new();
	for post in posts {
		let subreddit = crate::subreddit_semantics::normalize_subreddit_name(&post.community);
		if subreddit.is_empty() || subreddit.starts_with("u_") {
			continue;
		}
		let Some(text) = content_text_for_runtime_post(post, max_text_chars_per_post) else {
			continue;
		};
		let identity = runtime_post_identity(post);
		if let Some(candidate) = post_embedding_candidate(&subreddit, &identity, text, source_key, post.created_ts) {
			candidates_by_post
				.entry(candidate.post_key_hash.clone())
				.and_modify(|existing| {
					if candidate.created_ts > existing.created_ts {
						*existing = candidate.clone();
					}
				})
				.or_insert(candidate);
		}
	}

	let mut candidates = candidates_by_post.into_values().collect::<Vec<_>>();
	candidates.sort_by(|left, right| {
		left
			.subreddit
			.cmp(&right.subreddit)
			.then_with(|| left.created_ts.cmp(&right.created_ts))
			.then_with(|| left.post_key_hash.cmp(&right.post_key_hash))
	});
	candidates
}

pub fn build_subreddit_content_post_embedding_candidates(options: &SubredditContentCorpusOptions) -> Result<SubredditContentPostCandidateCollection, String> {
	let mut stats = SubredditContentCorpusStats::default();
	let mut candidates_by_post = HashMap::<String, SubredditContentPostEmbeddingCandidate>::new();

	cache_persistence::for_each_persisted_source_cache_file(|file| {
		stats.source_files += 1;
		for post in &file.entry.posts {
			stats.source_posts += 1;
			let subreddit = crate::subreddit_semantics::normalize_subreddit_name(&post.community);
			if subreddit.is_empty() || subreddit.starts_with("u_") {
				continue;
			}
			let Some(text) = content_text_for_post(post, options.max_text_chars_per_post) else {
				continue;
			};
			let identity = post_identity(post);
			if let Some(candidate) = post_embedding_candidate(&subreddit, &identity, text, &file.key, post.created_ts) {
				candidates_by_post
					.entry(candidate.post_key_hash.clone())
					.and_modify(|existing| {
						if candidate.created_ts > existing.created_ts {
							*existing = candidate.clone();
						}
					})
					.or_insert(candidate);
			}
		}
	})?;

	if let Some(catalog_file) = read_materialized_catalog_snapshot()? {
		let _catalog_version = catalog_file.version;
		let _catalog_saved_at_unix = catalog_file.saved_at_unix;
		stats.source_files += 1;
		for record in &catalog_file.snapshot.records {
			stats.source_posts += 1;
			let subreddit = crate::subreddit_semantics::normalize_subreddit_name(if record.community_lc.trim().is_empty() {
				&record.post.community
			} else {
				&record.community_lc
			});
			if subreddit.is_empty() || subreddit.starts_with("u_") {
				continue;
			}
			let Some(text) = content_text_for_legacy_post(&record.post, options.max_text_chars_per_post) else {
				continue;
			};
			let identity = legacy_post_identity(&record.post);
			let source_key = record.source_keys.first().map(source_key_label).unwrap_or_else(|| "materialized-catalog".to_string());
			if let Some(candidate) = post_embedding_candidate(&subreddit, &identity, text, &source_key, record.created_ts) {
				candidates_by_post
					.entry(candidate.post_key_hash.clone())
					.and_modify(|existing| {
						if candidate.created_ts > existing.created_ts {
							*existing = candidate.clone();
						}
					})
					.or_insert(candidate);
			}
		}
	}

	stats.unique_posts = candidates_by_post.len();
	let mut candidates = candidates_by_post.into_values().collect::<Vec<_>>();
	candidates.sort_by(|left, right| {
		left
			.subreddit
			.cmp(&right.subreddit)
			.then_with(|| left.created_ts.cmp(&right.created_ts))
			.then_with(|| left.post_key_hash.cmp(&right.post_key_hash))
	});
	let mut candidates_per_subreddit = HashMap::<String, usize>::new();
	for candidate in &candidates {
		*candidates_per_subreddit.entry(candidate.subreddit.clone()).or_default() += 1;
	}
	stats.total_subreddits = candidates_per_subreddit.len();
	stats.too_small_subreddits = candidates_per_subreddit.values().filter(|count| **count < options.min_posts_per_subreddit).count();
	stats.candidate_subreddits = candidates_per_subreddit.values().filter(|count| **count >= options.min_posts_per_subreddit).count();

	Ok(SubredditContentPostCandidateCollection { candidates, stats })
}

pub fn build_subreddit_content_corpora(options: &SubredditContentCorpusOptions) -> Result<SubredditContentCorpusCollection, String> {
	let mut stats = SubredditContentCorpusStats::default();
	let mut seen_posts = HashSet::new();
	let mut posts_by_subreddit = HashMap::<String, Vec<CandidatePostText>>::new();
	let mut source_keys_by_subreddit = HashMap::<String, HashSet<String>>::new();

	cache_persistence::for_each_persisted_source_cache_file(|file| {
		stats.source_files += 1;
		for post in &file.entry.posts {
			stats.source_posts += 1;
			let subreddit = crate::subreddit_semantics::normalize_subreddit_name(&post.community);
			if subreddit.is_empty() || subreddit.starts_with("u_") {
				continue;
			}
			let identity = format!("{subreddit}:{}", post_identity(post));
			if !seen_posts.insert(identity) {
				continue;
			}
			let Some(text) = content_text_for_post(post, options.max_text_chars_per_post) else {
				continue;
			};
			posts_by_subreddit.entry(subreddit.clone()).or_default().push(CandidatePostText {
				text,
				created_ts: post.created_ts,
			});
			source_keys_by_subreddit.entry(subreddit).or_default().insert(file.key.clone());
		}
	})?;

	if let Some(catalog_file) = read_materialized_catalog_snapshot()? {
		let _catalog_version = catalog_file.version;
		let _catalog_saved_at_unix = catalog_file.saved_at_unix;
		stats.source_files += 1;
		for record in &catalog_file.snapshot.records {
			stats.source_posts += 1;
			let subreddit = crate::subreddit_semantics::normalize_subreddit_name(if record.community_lc.trim().is_empty() {
				&record.post.community
			} else {
				&record.community_lc
			});
			if subreddit.is_empty() || subreddit.starts_with("u_") {
				continue;
			}
			let identity = format!("{subreddit}:{}", legacy_post_identity(&record.post));
			if !seen_posts.insert(identity) {
				continue;
			}
			let Some(text) = content_text_for_legacy_post(&record.post, options.max_text_chars_per_post) else {
				continue;
			};
			posts_by_subreddit.entry(subreddit.clone()).or_default().push(CandidatePostText {
				text,
				created_ts: record.created_ts,
			});
			let source_keys = source_keys_by_subreddit.entry(subreddit).or_default();
			if record.source_keys.is_empty() {
				source_keys.insert("materialized-catalog".to_string());
			} else {
				source_keys.extend(record.source_keys.iter().map(source_key_label));
			}
		}
	}
	stats.unique_posts = seen_posts.len();
	stats.total_subreddits = posts_by_subreddit.len();

	let corpora = build_subreddit_content_corpora_from_candidates(posts_by_subreddit, &source_keys_by_subreddit, options, &mut stats);
	Ok(SubredditContentCorpusCollection { corpora, stats })
}

fn build_subreddit_content_corpora_from_candidates(
	posts_by_subreddit: HashMap<String, Vec<CandidatePostText>>,
	source_keys_by_subreddit: &HashMap<String, HashSet<String>>,
	options: &SubredditContentCorpusOptions,
	stats: &mut SubredditContentCorpusStats,
) -> Vec<SubredditContentCorpus> {
	let mut corpora = Vec::new();
	for (subreddit, mut posts) in posts_by_subreddit {
		posts.sort_by(|left, right| right.created_ts.cmp(&left.created_ts));
		let mut seen_texts = HashSet::new();
		let mut selected = Vec::new();
		let mut text_chars = 0usize;
		for post in posts {
			if selected.len() >= options.max_posts_per_subreddit {
				break;
			}
			let text_key = post.text.to_lowercase();
			if !seen_texts.insert(text_key) {
				continue;
			}
			let line = format!("Post: {}", post.text);
			let line_chars = line.chars().count();
			if text_chars > 0 && text_chars.saturating_add(line_chars) > options.max_corpus_text_chars {
				break;
			}
			text_chars += line_chars;
			selected.push(line);
		}

		if selected.len() < options.min_posts_per_subreddit || text_chars < options.min_corpus_text_chars {
			stats.too_small_subreddits += 1;
			continue;
		}

		let text = format!("r/{subreddit}\nRecent cached post titles and text:\n{}", selected.join("\n"));
		let text_chars = text.chars().count();
		corpora.push(SubredditContentCorpus {
			source_count: source_keys_by_subreddit.get(&subreddit).map(|keys| keys.len()).unwrap_or_default(),
			corpus_hash: semantic_hash(&text),
			post_count: selected.len(),
			subreddit,
			text,
			text_chars,
		});
	}
	corpora.sort_by(|left, right| left.subreddit.cmp(&right.subreddit));
	stats.candidate_subreddits = corpora.len();
	corpora
}

pub fn store_embedding(
	subreddit: &str,
	model: &str,
	corpus_hash: &str,
	embedding: Vec<f32>,
	embedded_at_unix: u64,
	post_count: usize,
	source_count: usize,
	text_chars: usize,
) -> Result<(), String> {
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(subreddit);
	let model = model.trim().to_string();
	let corpus_hash = corpus_hash.trim().to_string();
	if subreddit.is_empty() {
		return Err("subreddit name is empty".to_string());
	}
	if model.is_empty() {
		return Err("embedding model is empty".to_string());
	}
	if corpus_hash.is_empty() {
		return Err("content corpus hash is empty".to_string());
	}
	let Some(embedding) = normalized_embedding(embedding) else {
		return Err("embedding is empty, non-finite, or zero-length".to_string());
	};

	let Ok(mut cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return Err("content semantic embedding cache lock is poisoned".to_string());
	};
	cache.insert(
		subreddit.clone(),
		PersistedSubredditContentSemanticEmbedding {
			subreddit,
			model,
			corpus_hash,
			vector_sum: embedding.iter().map(|value| *value * post_count.max(1) as f32).collect(),
			embedding,
			embedded_at_unix,
			post_count,
			source_count,
			text_chars,
			last_processed_created_ts: 0,
			last_processed_post_key_hash: String::new(),
			top_neighbor_similarity: None,
			top5_mean_similarity: None,
			samples: Vec::new(),
		},
	);
	Ok(())
}

fn empty_aggregate_entry(subreddit: String, model: String) -> PersistedSubredditContentSemanticEmbedding {
	PersistedSubredditContentSemanticEmbedding {
		subreddit,
		model,
		corpus_hash: String::new(),
		embedding: Vec::new(),
		embedded_at_unix: 0,
		post_count: 0,
		source_count: 0,
		text_chars: 0,
		vector_sum: Vec::new(),
		last_processed_created_ts: 0,
		last_processed_post_key_hash: String::new(),
		top_neighbor_similarity: None,
		top5_mean_similarity: None,
		samples: Vec::new(),
	}
}

fn candidate_is_after_cursor(candidate: &SubredditContentPostEmbeddingCandidate, entry: &PersistedSubredditContentSemanticEmbedding) -> bool {
	candidate.created_ts > entry.last_processed_created_ts
		|| (candidate.created_ts == entry.last_processed_created_ts && candidate.post_key_hash > entry.last_processed_post_key_hash)
}

fn aggregate_corpus_hash(entry: &PersistedSubredditContentSemanticEmbedding) -> String {
	semantic_hash(&format!(
		"cursor:{}:{}:{}:{}",
		entry.last_processed_created_ts, entry.last_processed_post_key_hash, entry.post_count, entry.text_chars
	))
}

fn recompute_aggregate_entry(entry: &mut PersistedSubredditContentSemanticEmbedding, embedded_at_unix: u64) -> Result<(), String> {
	entry.embedding.clone_from(&entry.vector_sum);
	if entry.embedding.is_empty() || !entry.embedding.iter().all(|value| value.is_finite()) || !normalize_embedding(&mut entry.embedding) {
		return Err("content semantic aggregate vector is empty".to_string());
	}
	entry.corpus_hash = aggregate_corpus_hash(entry);
	entry.embedded_at_unix = embedded_at_unix;
	entry.top_neighbor_similarity = None;
	entry.top5_mean_similarity = None;
	entry.samples.clear();
	Ok(())
}

pub fn post_candidate_needs_embedding(candidate: &SubredditContentPostEmbeddingCandidate, model: &str) -> bool {
	let model = model.trim();
	if candidate.subreddit.is_empty() || model.is_empty() {
		return false;
	}
	let Ok(cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return true;
	};
	let Some(entry) = cache.get(&candidate.subreddit) else {
		return true;
	};
	if entry.model != model {
		return true;
	}
	candidate_is_after_cursor(candidate, entry)
}

pub fn store_post_aggregate_embedding(candidate: &SubredditContentPostEmbeddingCandidate, model: &str, embedding: Vec<f32>, embedded_at_unix: u64) -> Result<bool, String> {
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(&candidate.subreddit);
	let model = model.trim().to_string();
	if subreddit.is_empty() {
		return Err("subreddit name is empty".to_string());
	}
	if model.is_empty() {
		return Err("embedding model is empty".to_string());
	}
	let Some(embedding) = normalized_embedding(embedding) else {
		return Err("embedding is empty, non-finite, or zero-length".to_string());
	};

	let Ok(mut cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return Err("content semantic embedding cache lock is poisoned".to_string());
	};
	let entry = cache.entry(subreddit.clone()).or_insert_with(|| empty_aggregate_entry(subreddit.clone(), model.clone()));
	if entry.model == model && !candidate_is_after_cursor(candidate, entry) {
		return Ok(false);
	}

	// Build the next aggregate off to the side. A zero/non-finite recompute or
	// any future validation failure must not leave the live entry's cursor and
	// counters advanced past a candidate that was not successfully stored.
	let mut updated = if entry.model == model {
		entry.clone()
	} else {
		empty_aggregate_entry(subreddit.clone(), model.clone())
	};
	if updated.vector_sum.is_empty() {
		updated.vector_sum = vec![0.0; embedding.len()];
	}
	if updated.vector_sum.len() != embedding.len() {
		updated = empty_aggregate_entry(subreddit, model);
		updated.vector_sum = vec![0.0; embedding.len()];
	}
	for (sum_value, embedding_value) in updated.vector_sum.iter_mut().zip(&embedding) {
		*sum_value += *embedding_value;
	}
	updated.post_count = updated.post_count.saturating_add(1);
	updated.source_count = updated.source_count.saturating_add(1);
	updated.text_chars = updated.text_chars.saturating_add(candidate.text_chars);
	updated.last_processed_created_ts = candidate.created_ts;
	updated.last_processed_post_key_hash = candidate.post_key_hash.clone();
	recompute_aggregate_entry(&mut updated, embedded_at_unix)?;
	*entry = updated;
	Ok(true)
}

pub fn has_current_embedding(corpus: &SubredditContentCorpus, model: &str) -> bool {
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(&corpus.subreddit);
	if subreddit.is_empty() || model.trim().is_empty() {
		return false;
	}
	let Ok(cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return false;
	};
	cache
		.get(&subreddit)
		.is_some_and(|entry| entry.model == model.trim() && entry.corpus_hash == corpus.corpus_hash && !entry.embedding.is_empty())
}

fn entry_is_usable(entry: &PersistedSubredditContentSemanticEmbedding, model: &str) -> bool {
	if entry.model != model || entry.embedding.is_empty() {
		return false;
	}
	let options = corpus_options_from_env();
	if entry.post_count < options.min_posts_per_subreddit || entry.text_chars < options.min_corpus_text_chars {
		return false;
	}
	match entry.top5_mean_similarity {
		Some(score) => score <= max_top5_mean_similarity(),
		None => true,
	}
}

pub fn embedding_for_subreddit_model(subreddit: &str, model: &str) -> Option<Vec<f32>> {
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(subreddit);
	let model = model.trim();
	if subreddit.is_empty() || model.is_empty() {
		return None;
	}
	let cache = CONTENT_SEMANTIC_CACHE.lock().ok()?;
	let entry = cache.get(&subreddit)?;
	entry_is_usable(entry, model).then(|| entry.embedding.clone())
}

pub fn embedding_count_for_model(model: &str) -> usize {
	let model = model.trim();
	let Ok(cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return 0;
	};
	cache.values().filter(|entry| entry.model == model && !entry.embedding.is_empty()).count()
}

pub fn usable_embedding_count_for_model(model: &str) -> usize {
	let model = model.trim();
	let Ok(cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return 0;
	};
	cache.values().filter(|entry| entry_is_usable(entry, model)).count()
}

pub fn active_embedding_count() -> usize {
	embedding_count_for_model(&crate::subreddit_semantics::embedding_model())
}

pub fn active_usable_embedding_count() -> usize {
	usable_embedding_count_for_model(&crate::subreddit_semantics::embedding_model())
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubredditContentAggregatePreview {
	pub embedded_at_unix: u64,
	pub dimensions: usize,
	pub displayed_dimensions: usize,
	pub post_count: usize,
	pub values: Vec<f32>,
}

pub fn aggregate_preview_for_subreddit_model(subreddit: &str, model: &str, displayed_dimensions: usize) -> Option<SubredditContentAggregatePreview> {
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(subreddit);
	let model = model.trim();
	if subreddit.is_empty() || model.is_empty() {
		return None;
	}
	let cache = CONTENT_SEMANTIC_CACHE.lock().ok()?;
	let entry = cache.get(&subreddit)?;
	if entry.model != model || entry.vector_sum.is_empty() || entry.post_count == 0 || !entry.vector_sum.iter().all(|value| value.is_finite()) {
		return None;
	}
	let displayed_dimensions = entry.vector_sum.len().min(displayed_dimensions);
	let divisor = entry.post_count as f32;
	Some(SubredditContentAggregatePreview {
		embedded_at_unix: entry.embedded_at_unix,
		dimensions: entry.vector_sum.len(),
		displayed_dimensions,
		post_count: entry.post_count,
		values: entry.vector_sum.iter().take(displayed_dimensions).map(|value| *value / divisor).collect(),
	})
}

pub fn active_entries_for_model(model: &str) -> Vec<PersistedSubredditContentSemanticEmbedding> {
	let model = model.trim();
	let Ok(cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return Vec::new();
	};
	let mut entries = cache
		.values()
		.filter(|entry| entry.model == model && !entry.embedding.is_empty())
		.cloned()
		.collect::<Vec<_>>();
	entries.sort_by(|left, right| left.subreddit.cmp(&right.subreddit));
	entries
}

pub fn persisted_embeddings_sidecar_path() -> PathBuf {
	Path::new(&crate::cache_persistence::cache_dir()).join(SUBREDDIT_CONTENT_SEMANTIC_SIDECAR_FILE_NAME)
}

fn persisted_embeddings_lock_path() -> PathBuf {
	Path::new(&crate::cache_persistence::cache_dir()).join(SUBREDDIT_CONTENT_SEMANTIC_SIDECAR_LOCK_FILE_NAME)
}

fn persisted_embeddings_mutation_lock_path() -> PathBuf {
	Path::new(&crate::cache_persistence::cache_dir()).join(SUBREDDIT_CONTENT_SEMANTIC_MUTATION_LOCK_FILE_NAME)
}

pub fn backfill_status_path() -> PathBuf {
	Path::new(&crate::cache_persistence::cache_dir()).join(SUBREDDIT_CONTENT_SEMANTIC_BACKFILL_STATUS_FILE_NAME)
}

fn system_time_to_unix(time: SystemTime) -> Option<u64> {
	time.duration_since(UNIX_EPOCH).ok().map(|duration| duration.as_secs())
}

fn snapshot_persisted_embeddings(_now_unix: u64) -> Vec<PersistedSubredditContentSemanticEmbedding> {
	let cache = CONTENT_SEMANTIC_CACHE.lock().unwrap_or_else(|error| error.into_inner());
	let mut entries = cache.values().cloned().collect::<Vec<_>>();
	entries.sort_by(|left, right| left.subreddit.cmp(&right.subreddit).then_with(|| left.model.cmp(&right.model)));
	entries
}

fn normalized_entry(mut entry: PersistedSubredditContentSemanticEmbedding) -> Option<(String, PersistedSubredditContentSemanticEmbedding)> {
	entry.subreddit = crate::subreddit_semantics::normalize_subreddit_name(&entry.subreddit);
	entry.model = entry.model.trim().to_string();
	entry.corpus_hash = entry.corpus_hash.trim().to_string();
	entry.last_processed_post_key_hash = entry.last_processed_post_key_hash.trim().to_string();
	entry.embedding = normalized_embedding(entry.embedding)?;
	if entry.vector_sum.is_empty() {
		entry.vector_sum = entry.embedding.clone();
	}
	if entry.vector_sum.len() != entry.embedding.len() || !entry.vector_sum.iter().all(|value| value.is_finite()) {
		return None;
	}
	entry.embedding.clone_from(&entry.vector_sum);
	if !normalize_embedding(&mut entry.embedding) {
		return None;
	}
	if entry.corpus_hash.is_empty() {
		entry.corpus_hash = aggregate_corpus_hash(&entry);
	}
	entry.samples.clear();
	(!entry.subreddit.is_empty() && !entry.model.is_empty() && !entry.corpus_hash.is_empty()).then_some((entry.subreddit.clone(), entry))
}

fn normalized_entries_map(entries: Vec<PersistedSubredditContentSemanticEmbedding>) -> HashMap<String, PersistedSubredditContentSemanticEmbedding> {
	entries.into_iter().filter_map(normalized_entry).collect::<HashMap<_, _>>()
}

fn restore_persisted_embeddings(entries: Vec<PersistedSubredditContentSemanticEmbedding>) {
	let restored = normalized_entries_map(entries);
	if let Ok(mut cache) = CONTENT_SEMANTIC_CACHE.lock() {
		cache.extend(restored);
	}
}

fn read_persisted_embeddings_file(path: &Path) -> Result<Option<PersistedSubredditContentSemanticEmbeddingFile>, String> {
	let file = match File::open(path) {
		Ok(file) => file,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.to_string()),
	};
	let file: PersistedSubredditContentSemanticEmbeddingFile = bincode::deserialize_from(BufReader::new(file)).map_err(|error| error.to_string())?;
	if file.version != SUBREDDIT_CONTENT_SEMANTIC_SIDECAR_VERSION {
		return Err(format!("unsupported content semantic embeddings sidecar version {}", file.version));
	}
	Ok(Some(file))
}

fn read_backfill_status_file(path: &Path) -> Result<Option<SubredditContentSemanticBackfillStatus>, String> {
	let bytes = match fs::read(path) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.to_string()),
	};
	serde_json::from_slice(&bytes).map(Some).map_err(|error| error.to_string())
}

pub fn read_backfill_status() -> Result<Option<SubredditContentSemanticBackfillStatus>, String> {
	read_backfill_status_file(&backfill_status_path())
}

pub fn write_backfill_status(status: &SubredditContentSemanticBackfillStatus) -> Result<(), String> {
	let final_path = backfill_status_path();
	if let Some(parent) = final_path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let bytes = serde_json::to_vec_pretty(status).map_err(|error| error.to_string())?;
	let temp_path = final_path.with_extension(format!("json.tmp.{}", std::process::id()));
	let mut temp_file = File::create(&temp_path).map_err(|error| error.to_string())?;
	temp_file.write_all(&bytes).map_err(|error| error.to_string())?;
	temp_file.sync_all().map_err(|error| error.to_string())?;
	drop(temp_file);
	fs::rename(&temp_path, &final_path).map_err(|error| error.to_string())
}

fn percentile(values: &[f32], percentile: f64) -> Option<f32> {
	if values.is_empty() {
		return None;
	}
	let index = ((values.len().saturating_sub(1) as f64) * percentile).round() as usize;
	values.get(index.min(values.len() - 1)).copied()
}

pub fn embedding_database_status() -> SubredditContentSemanticDatabaseStatus {
	let model = crate::subreddit_semantics::embedding_model();
	let refresh_error = refresh_persisted_embeddings_sidecar().err();
	let now_unix = crate::cache_persistence::unix_now();
	let sidecar_path = persisted_embeddings_sidecar_path();
	let sidecar_metadata = fs::metadata(&sidecar_path).ok();
	let sidecar_modified_unix = sidecar_metadata.as_ref().and_then(|metadata| metadata.modified().ok()).and_then(system_time_to_unix);
	let sidecar_modified_age_secs = sidecar_modified_unix.map(|modified_at| now_unix.saturating_sub(modified_at));
	let (sidecar_saved_at_unix, sidecar_entry_count, sidecar_read_error) = match read_persisted_embeddings_file(&sidecar_path) {
		Ok(Some(file)) => (Some(file.saved_at_unix), file.entries.len(), None),
		Ok(None) => (None, 0, None),
		Err(error) => (None, 0, Some(error)),
	};

	let entries = snapshot_persisted_embeddings(now_unix);
	let mut active_model_entry_count = 0usize;
	let mut usable_model_entry_count = 0usize;
	let mut other_model_entry_count = 0usize;
	let mut embedding_dimensions = None;
	let mut oldest_embedding_at_unix = None;
	let mut newest_embedding_at_unix = None;
	let mut top5_values = Vec::new();
	for entry in &entries {
		if entry.embedding.is_empty() {
			continue;
		}
		if entry.model == model {
			active_model_entry_count += 1;
			if entry_is_usable(entry, &model) {
				usable_model_entry_count += 1;
			}
			if embedding_dimensions.is_none() {
				embedding_dimensions = Some(entry.embedding.len());
			}
			if let Some(score) = entry.top5_mean_similarity {
				if score.is_finite() {
					top5_values.push(score);
				}
			}
			oldest_embedding_at_unix = Some(
				oldest_embedding_at_unix
					.map(|oldest: u64| oldest.min(entry.embedded_at_unix))
					.unwrap_or(entry.embedded_at_unix),
			);
			newest_embedding_at_unix = Some(
				newest_embedding_at_unix
					.map(|newest: u64| newest.max(entry.embedded_at_unix))
					.unwrap_or(entry.embedded_at_unix),
			);
		} else {
			other_model_entry_count += 1;
		}
	}
	top5_values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
	let top5_mean_similarity_average = (!top5_values.is_empty()).then(|| top5_values.iter().sum::<f32>() / top5_values.len() as f32);
	let top5_mean_similarity_p95 = percentile(&top5_values, 0.95);
	let (last_run, last_run_error) = match read_backfill_status() {
		Ok(status) => (status, None),
		Err(error) => (None, Some(error)),
	};
	let options = corpus_options_from_env();

	SubredditContentSemanticDatabaseStatus {
		model,
		content_mix_percent: content_mix_percent(),
		sidecar_path: sidecar_path.display().to_string(),
		sidecar_exists: sidecar_metadata.is_some(),
		sidecar_size_bytes: sidecar_metadata.as_ref().map(|metadata| metadata.len()),
		sidecar_modified_unix,
		sidecar_modified_age_secs,
		sidecar_saved_at_unix,
		sidecar_entry_count,
		loaded_entry_count: entries.len(),
		active_model_entry_count,
		usable_model_entry_count,
		other_model_entry_count,
		embedding_dimensions,
		min_posts_per_subreddit: options.min_posts_per_subreddit,
		min_corpus_text_chars: options.min_corpus_text_chars,
		max_top5_mean_similarity: max_top5_mean_similarity(),
		top5_mean_similarity_average,
		top5_mean_similarity_p95,
		oldest_embedding_at_unix,
		newest_embedding_at_unix,
		refresh_error,
		sidecar_read_error,
		last_run,
		last_run_error,
	}
}

pub(crate) fn load_persisted_embeddings_sidecar() -> Result<(), String> {
	let path = persisted_embeddings_sidecar_path();
	let modified = fs::metadata(&path).and_then(|metadata| metadata.modified()).ok();
	let Some(file) = read_persisted_embeddings_file(&path)? else {
		if let Ok(mut mtime) = CONTENT_SEMANTIC_SIDECAR_MTIME.lock() {
			*mtime = None;
		}
		return Ok(());
	};
	restore_persisted_embeddings(file.entries);
	if let Ok(mut mtime) = CONTENT_SEMANTIC_SIDECAR_MTIME.lock() {
		*mtime = modified;
	}
	Ok(())
}

pub fn refresh_persisted_embeddings_sidecar() -> Result<bool, String> {
	let path = persisted_embeddings_sidecar_path();
	let modified = match fs::metadata(&path).and_then(|metadata| metadata.modified()) {
		Ok(modified) => modified,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			if let Ok(mut mtime) = CONTENT_SEMANTIC_SIDECAR_MTIME.lock() {
				*mtime = None;
			}
			return Ok(false);
		}
		Err(error) => return Err(error.to_string()),
	};

	if CONTENT_SEMANTIC_SIDECAR_MTIME.lock().ok().and_then(|mtime| *mtime) == Some(modified) {
		return Ok(false);
	}

	load_persisted_embeddings_sidecar()?;
	Ok(true)
}

/// Cross-process guard for content-semantic mutations. Every mutator holds an
/// exclusive guard from sidecar refresh/load through its fully persisted
/// burst, so overlapping Redlib processes and the standalone backfill cannot
/// fork the same aggregate and overwrite each other's vector/cursor progress.
pub struct ContentSemanticMutationGuard {
	_file: File,
}

pub fn acquire_content_semantic_mutation_guard() -> Result<ContentSemanticMutationGuard, String> {
	let path = persisted_embeddings_mutation_lock_path();
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let file = OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(false)
		.open(&path)
		.map_err(|error| error.to_string())?;
	file.lock().map_err(|error| format!("failed to lock {}: {error}", path.display()))?;
	Ok(ContentSemanticMutationGuard { _file: file })
}

struct SidecarLock {
	path: PathBuf,
}

impl Drop for SidecarLock {
	fn drop(&mut self) {
		let _ = fs::remove_file(&self.path);
	}
}

fn lock_file_is_stale(path: &Path) -> bool {
	fs::metadata(path)
		.and_then(|metadata| metadata.modified())
		.ok()
		.and_then(|modified| modified.elapsed().ok())
		.is_some_and(|age| age > Duration::from_secs(SIDECAR_STALE_LOCK_SECS))
}

fn acquire_sidecar_lock() -> Result<SidecarLock, String> {
	let lock_path = persisted_embeddings_lock_path();
	if let Some(parent) = lock_path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let started = Instant::now();
	loop {
		match OpenOptions::new().write(true).create_new(true).open(&lock_path) {
			Ok(mut file) => {
				file.write_all(std::process::id().to_string().as_bytes()).map_err(|error| error.to_string())?;
				file.sync_all().map_err(|error| error.to_string())?;
				return Ok(SidecarLock { path: lock_path });
			}
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
				if lock_file_is_stale(&lock_path) {
					let _ = fs::remove_file(&lock_path);
					continue;
				}
				if started.elapsed() > Duration::from_secs(SIDECAR_LOCK_TIMEOUT_SECS) {
					return Err(format!("timed out waiting for content semantic embeddings sidecar lock {}", lock_path.display()));
				}
				thread::sleep(Duration::from_millis(100));
			}
			Err(error) => return Err(error.to_string()),
		}
	}
}

pub fn refresh_similarity_quality_scores_for_model(model: &str) -> Result<(), String> {
	let model = model.trim();
	let Ok(mut cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return Err("content semantic embedding cache lock is poisoned".to_string());
	};
	let entries = cache
		.values()
		.filter(|entry| entry.model == model && !entry.embedding.is_empty())
		.map(|entry| (entry.subreddit.clone(), entry.embedding.clone()))
		.collect::<Vec<_>>();

	for index in 0..entries.len() {
		let (subreddit, embedding) = &entries[index];
		let mut similarities = Vec::new();
		for (other_index, (_, other_embedding)) in entries.iter().enumerate() {
			if index == other_index || embedding.len() != other_embedding.len() {
				continue;
			}
			similarities.push(embedding_dot(embedding, other_embedding));
		}
		similarities.sort_by(|left, right| right.partial_cmp(left).unwrap_or(Ordering::Equal));
		let top_neighbor_similarity = similarities.first().copied();
		let top5_mean_similarity = if similarities.is_empty() {
			None
		} else {
			let limit = similarities.len().min(5);
			Some(similarities.iter().take(limit).sum::<f32>() / limit as f32)
		};
		if let Some(entry) = cache.get_mut(subreddit) {
			entry.top_neighbor_similarity = top_neighbor_similarity;
			entry.top5_mean_similarity = top5_mean_similarity;
		}
	}
	Ok(())
}

/// Return whether embeddings for this model have changed since the last full
/// similarity-quality pass. Storing or updating an aggregate clears both
/// scores on that entry; because one changed vector can affect every other
/// entry's nearest-neighbor scores, any missing score makes the model stale.
/// A lone embedding correctly has no neighbor and therefore needs no refresh.
pub fn similarity_quality_scores_need_refresh_for_model(model: &str) -> bool {
	let model = model.trim();
	let Ok(cache) = CONTENT_SEMANTIC_CACHE.lock() else {
		return true;
	};
	let mut entries = cache.values().filter(|entry| entry.model == model && !entry.embedding.is_empty());
	let Some(first) = entries.next() else {
		return false;
	};
	let first_missing = first.top_neighbor_similarity.is_none() || first.top5_mean_similarity.is_none();
	let mut has_multiple = false;
	let mut has_missing = first_missing;
	for entry in entries {
		has_multiple = true;
		has_missing |= entry.top_neighbor_similarity.is_none() || entry.top5_mean_similarity.is_none();
	}
	has_multiple && has_missing
}

pub fn write_persisted_embeddings_sidecar_now() -> Result<(), String> {
	let final_path = persisted_embeddings_sidecar_path();
	if let Some(parent) = final_path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let _lock = acquire_sidecar_lock()?;
	let disk_modified = fs::metadata(&final_path).and_then(|metadata| metadata.modified()).ok();
	let known_modified = CONTENT_SEMANTIC_SIDECAR_MTIME.lock().ok().and_then(|mtime| *mtime);
	let live_entries = snapshot_persisted_embeddings(crate::cache_persistence::unix_now());
	let mut entries = if disk_modified == known_modified {
		live_entries
	} else {
		let mut entries_by_subreddit = read_persisted_embeddings_file(&final_path)?
			.map(|file| normalized_entries_map(file.entries))
			.unwrap_or_default();
		for entry in live_entries {
			entries_by_subreddit.insert(entry.subreddit.clone(), entry);
		}
		entries_by_subreddit.into_values().collect::<Vec<_>>()
	};
	entries.sort_by(|left, right| left.subreddit.cmp(&right.subreddit).then_with(|| left.model.cmp(&right.model)));

	let file = PersistedSubredditContentSemanticEmbeddingFile {
		version: SUBREDDIT_CONTENT_SEMANTIC_SIDECAR_VERSION,
		saved_at_unix: crate::cache_persistence::unix_now(),
		entries,
	};
	let temp_path = final_path.with_extension(format!("tmp.{}", std::process::id()));
	let temp_file = File::create(&temp_path).map_err(|error| error.to_string())?;
	let mut writer = BufWriter::new(temp_file);
	bincode::serialize_into(&mut writer, &file).map_err(|error| error.to_string())?;
	writer.flush().map_err(|error| error.to_string())?;
	let temp_file = writer.into_inner().map_err(|error| error.into_error().to_string())?;
	temp_file.sync_all().map_err(|error| error.to_string())?;
	drop(temp_file);
	fs::rename(&temp_path, &final_path).map_err(|error| error.to_string())?;

	if let Ok(modified) = fs::metadata(&final_path).and_then(|metadata| metadata.modified()) {
		if let Ok(mut mtime) = CONTENT_SEMANTIC_SIDECAR_MTIME.lock() {
			*mtime = Some(modified);
		}
	}
	Ok(())
}

#[cfg(test)]
pub(crate) fn clear_test_cache() {
	if let Ok(mut cache) = CONTENT_SEMANTIC_CACHE.lock() {
		cache.clear();
	}
	if let Ok(mut mtime) = CONTENT_SEMANTIC_SIDECAR_MTIME.lock() {
		*mtime = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::ffi::OsString;
	use std::path::PathBuf;

	static TEST_CACHE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

	struct CacheDirGuard {
		previous: Option<OsString>,
		path: PathBuf,
	}

	impl CacheDirGuard {
		fn new(label: &str) -> Self {
			let previous = std::env::var_os("REDLIB_CACHE_DIR");
			let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
			let path = std::env::temp_dir().join(format!("redlib-content-semantic-{label}-{}-{unique}", std::process::id()));
			std::fs::create_dir_all(&path).unwrap();
			std::env::set_var("REDLIB_CACHE_DIR", &path);
			Self { previous, path }
		}
	}

	impl Drop for CacheDirGuard {
		fn drop(&mut self) {
			if let Some(previous) = &self.previous {
				std::env::set_var("REDLIB_CACHE_DIR", previous);
			} else {
				std::env::remove_var("REDLIB_CACHE_DIR");
			}
			let _ = std::fs::remove_dir_all(&self.path);
			clear_test_cache();
		}
	}

	#[test]
	fn normalized_embedding_rejects_zero_vector() {
		assert!(normalized_embedding(vec![0.0, 0.0]).is_none());
		assert!(normalized_embedding(vec![1.0, 0.0]).is_some());
	}

	#[test]
	fn user_runtime_candidates_target_the_user_instead_of_the_post_subreddit() {
		let post = crate::utils::Post {
			id: "post-1".to_string(),
			title: "A useful submission".to_string(),
			community: "rust".to_string(),
			body: "Async runtimes and task scheduling".to_string(),
			created_ts: 123,
			..crate::utils::Post::default()
		};

		let candidates = runtime_user_post_embedding_candidates("ExampleUser", "user:ExampleUser:overview", &[post], 360);

		assert_eq!(candidates.len(), 1);
		assert_eq!(candidates[0].subreddit, "u_exampleuser");
		assert_eq!(candidates[0].created_ts, 123);
		assert!(candidates[0].text.contains("A useful submission"));
	}

	#[test]
	fn user_candidate_is_stored_as_its_own_aggregate_vector() {
		let _guard = TEST_CACHE_LOCK.lock().unwrap();
		clear_test_cache();
		let post = crate::utils::Post {
			id: "post-1".to_string(),
			title: "A useful submission".to_string(),
			community: "rust".to_string(),
			body: "Async runtimes and task scheduling".to_string(),
			created_ts: 123,
			..crate::utils::Post::default()
		};
		let candidate = runtime_user_post_embedding_candidates("ExampleUser", "user:ExampleUser:overview", &[post], 360)
			.into_iter()
			.next()
			.unwrap();

		assert!(store_post_aggregate_embedding(&candidate, "test-model", vec![3.0, 4.0], 200).unwrap());
		let stored = active_entries_for_model("test-model").into_iter().find(|entry| entry.subreddit == "u_exampleuser").unwrap();

		assert_eq!(stored.post_count, 1);
		assert_eq!(stored.vector_sum, vec![0.6, 0.8]);
		assert_eq!(stored.embedding, vec![0.6, 0.8]);
	}

	#[test]
	fn ordinary_runtime_candidates_still_reject_user_pseudo_communities() {
		let post = crate::utils::Post {
			id: "post-1".to_string(),
			title: "A useful submission".to_string(),
			community: "u_exampleuser".to_string(),
			created_ts: 123,
			..crate::utils::Post::default()
		};

		assert!(runtime_post_embedding_candidates("subreddit:u_exampleuser:new", &[post], 360).is_empty());
	}

	#[test]
	fn stored_embedding_round_trips_from_memory() {
		let _guard = TEST_CACHE_LOCK.lock().unwrap();
		clear_test_cache();
		store_embedding("Rust", "test-model", "hash", vec![3.0, 4.0], 123, 8, 2, 400).unwrap();
		let embedding = embedding_for_subreddit_model("rust", "test-model").unwrap();
		assert_eq!(embedding.len(), 2);
		assert!((embedding[0] - 0.6).abs() < 0.001);
		assert!((embedding[1] - 0.8).abs() < 0.001);
	}

	#[test]
	fn similarity_quality_refresh_is_needed_only_after_multi_entry_changes() {
		let _guard = TEST_CACHE_LOCK.lock().unwrap();
		clear_test_cache();

		store_embedding("Rust", "test-model", "rust-hash", vec![1.0, 0.0], 100, 1, 1, 100).unwrap();
		assert!(!similarity_quality_scores_need_refresh_for_model("test-model"));

		store_embedding("Baking", "test-model", "baking-hash", vec![0.0, 1.0], 100, 1, 1, 100).unwrap();
		assert!(similarity_quality_scores_need_refresh_for_model("test-model"));
		refresh_similarity_quality_scores_for_model("test-model").unwrap();
		assert!(!similarity_quality_scores_need_refresh_for_model("test-model"));

		store_embedding("Rust", "test-model", "rust-new", vec![1.0, 1.0], 200, 2, 1, 200).unwrap();
		assert!(similarity_quality_scores_need_refresh_for_model("test-model"));
	}

	#[test]
	fn sidecar_fast_path_round_trips_and_changed_mtime_merges() {
		let _lock = TEST_CACHE_LOCK.lock().unwrap();
		let _cache_dir = CacheDirGuard::new("merge");
		clear_test_cache();

		store_embedding("Rust", "test-model", "rust-hash", vec![1.0, 0.0], 100, 1, 10, 100).unwrap();
		write_persisted_embeddings_sidecar_now().unwrap();
		clear_test_cache();

		store_embedding("Baking", "test-model", "baking-hash", vec![0.0, 1.0], 200, 1, 10, 200).unwrap();
		write_persisted_embeddings_sidecar_now().unwrap();
		clear_test_cache();
		assert!(refresh_persisted_embeddings_sidecar().unwrap());

		let names = active_entries_for_model("test-model").into_iter().map(|entry| entry.subreddit).collect::<Vec<_>>();
		assert_eq!(names, vec!["baking".to_string(), "rust".to_string()]);
	}

	#[test]
	fn content_semantic_mutation_guard_is_exclusive() {
		let _lock = TEST_CACHE_LOCK.lock().unwrap();
		let _cache_dir = CacheDirGuard::new("mutation-lock");
		let first = acquire_content_semantic_mutation_guard().unwrap();
		let (sender, receiver) = std::sync::mpsc::channel();
		let waiter = std::thread::spawn(move || {
			let _second = acquire_content_semantic_mutation_guard().unwrap();
			sender.send(()).unwrap();
		});

		assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
		drop(first);
		assert!(receiver.recv_timeout(Duration::from_secs(2)).is_ok());
		waiter.join().unwrap();
	}

	#[test]
	fn duplicate_candidate_text_does_not_change_corpus_hash() {
		let options = SubredditContentCorpusOptions {
			max_posts_per_subreddit: 10,
			max_text_chars_per_post: 360,
			max_corpus_text_chars: 3_000,
			min_posts_per_subreddit: 3,
			min_corpus_text_chars: 10,
		};
		let source_keys_by_subreddit = HashMap::from([("rust".to_string(), HashSet::from(["subreddit:rust".to_string()]))]);
		let base_posts = vec![
			CandidatePostText {
				text: "Async runtimes and task scheduling".to_string(),
				created_ts: 60,
			},
			CandidatePostText {
				text: "Borrow checker diagnostics for beginners".to_string(),
				created_ts: 50,
			},
			CandidatePostText {
				text: "Crate release notes and ecosystem updates".to_string(),
				created_ts: 40,
			},
		];
		let duplicated_posts = base_posts
			.iter()
			.cloned()
			.chain(std::iter::once(CandidatePostText {
				text: "Async runtimes and task scheduling".to_string(),
				created_ts: 70,
			}))
			.collect::<Vec<_>>();
		let mut base_stats = SubredditContentCorpusStats::default();
		let mut duplicate_stats = SubredditContentCorpusStats::default();
		let base = build_subreddit_content_corpora_from_candidates(HashMap::from([("rust".to_string(), base_posts)]), &source_keys_by_subreddit, &options, &mut base_stats);
		let with_duplicate = build_subreddit_content_corpora_from_candidates(
			HashMap::from([("rust".to_string(), duplicated_posts)]),
			&source_keys_by_subreddit,
			&options,
			&mut duplicate_stats,
		);

		assert_eq!(base.len(), 1);
		assert_eq!(with_duplicate.len(), 1);
		assert_eq!(base[0].post_count, 3);
		assert_eq!(with_duplicate[0].post_count, 3);
		assert_eq!(base[0].corpus_hash, with_duplicate[0].corpus_hash);
		assert_eq!(base[0].text, with_duplicate[0].text);
	}

	#[test]
	fn duplicate_or_older_post_does_not_move_running_vector() {
		let _guard = TEST_CACHE_LOCK.lock().unwrap();
		clear_test_cache();
		let candidate = SubredditContentPostEmbeddingCandidate {
			subreddit: "rust".to_string(),
			text: "Async runtimes and task scheduling".to_string(),
			post_key_hash: "post-a".to_string(),
			text_hash: "text-a".to_string(),
			sort_key: 10,
			source_key_hash: "source-a".to_string(),
			created_ts: 100,
			text_chars: 34,
		};

		assert!(store_post_aggregate_embedding(&candidate, "test-model", vec![1.0, 0.0], 100).unwrap());
		let first = active_entries_for_model("test-model").into_iter().next().unwrap();
		assert_eq!(first.post_count, 1);
		assert_eq!(first.last_processed_created_ts, 100);
		assert!(!store_post_aggregate_embedding(&candidate, "test-model", vec![0.0, 1.0], 200).unwrap());
		let second = active_entries_for_model("test-model").into_iter().next().unwrap();

		assert_eq!(second.post_count, 1);
		assert_eq!(first.corpus_hash, second.corpus_hash);
		assert_eq!(first.embedding, second.embedding);
		assert_eq!(first.embedded_at_unix, second.embedded_at_unix);
	}

	#[test]
	fn failed_aggregate_recompute_does_not_advance_cursor_or_vector() {
		let _guard = TEST_CACHE_LOCK.lock().unwrap();
		clear_test_cache();
		let first = SubredditContentPostEmbeddingCandidate {
			subreddit: "rust".to_string(),
			text: "First post".to_string(),
			post_key_hash: "post-a".to_string(),
			text_hash: "text-a".to_string(),
			sort_key: 10,
			source_key_hash: "source-a".to_string(),
			created_ts: 100,
			text_chars: 10,
		};
		let mut cancelling = first.clone();
		cancelling.post_key_hash = "post-b".to_string();
		cancelling.text_hash = "text-b".to_string();
		cancelling.created_ts = 101;

		assert!(store_post_aggregate_embedding(&first, "test-model", vec![1.0, 0.0], 100).unwrap());
		let before = active_entries_for_model("test-model").into_iter().next().unwrap();
		assert!(store_post_aggregate_embedding(&cancelling, "test-model", vec![-1.0, 0.0], 200).is_err());
		let after = active_entries_for_model("test-model").into_iter().next().unwrap();

		assert_eq!(after, before);
	}

	#[test]
	fn newer_post_advances_running_vector() {
		let _guard = TEST_CACHE_LOCK.lock().unwrap();
		clear_test_cache();
		let first = SubredditContentPostEmbeddingCandidate {
			subreddit: "rust".to_string(),
			text: "Async runtimes and task scheduling".to_string(),
			post_key_hash: "post-a".to_string(),
			text_hash: "text-a".to_string(),
			sort_key: 10,
			source_key_hash: "source-a".to_string(),
			created_ts: 100,
			text_chars: 34,
		};
		let mut second = first.clone();
		second.post_key_hash = "post-b".to_string();
		second.text_hash = "text-b".to_string();
		second.created_ts = 101;
		second.text_chars = 28;

		assert!(store_post_aggregate_embedding(&first, "test-model", vec![1.0, 0.0], 100).unwrap());
		assert!(store_post_aggregate_embedding(&second, "test-model", vec![0.0, 1.0], 200).unwrap());
		let entry = active_entries_for_model("test-model").into_iter().next().unwrap();

		assert_eq!(entry.post_count, 2);
		assert_eq!(entry.text_chars, 62);
		assert_eq!(entry.last_processed_created_ts, 101);
		assert_eq!(entry.last_processed_post_key_hash, "post-b");
		assert!((entry.embedding[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.001);
		assert!((entry.embedding[1] - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.001);
		assert_eq!(entry.vector_sum, vec![1.0, 1.0]);
	}

	#[test]
	fn aggregate_preview_exposes_running_average() {
		let _guard = TEST_CACHE_LOCK.lock().unwrap();
		clear_test_cache();
		let first = SubredditContentPostEmbeddingCandidate {
			subreddit: "rust".to_string(),
			text: "Async runtimes and task scheduling".to_string(),
			post_key_hash: "post-a".to_string(),
			text_hash: "text-a".to_string(),
			sort_key: 10,
			source_key_hash: "source-a".to_string(),
			created_ts: 100,
			text_chars: 34,
		};
		let mut second = first.clone();
		second.post_key_hash = "post-b".to_string();
		second.text_hash = "text-b".to_string();
		second.created_ts = 101;

		assert!(store_post_aggregate_embedding(&first, "test-model", vec![1.0, 0.0], 100).unwrap());
		assert!(store_post_aggregate_embedding(&second, "test-model", vec![0.0, 1.0], 200).unwrap());

		let preview = aggregate_preview_for_subreddit_model("rust", "test-model", 1).unwrap();
		assert_eq!(preview.post_count, 2);
		assert_eq!(preview.dimensions, 2);
		assert_eq!(preview.displayed_dimensions, 1);
		assert_eq!(preview.values, vec![0.5]);
	}
}
