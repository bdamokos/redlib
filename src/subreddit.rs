#![allow(clippy::cmp_owned)]

use crate::{config, instance_scrape, peer_api_fallback, prefs_store, seen_posts, utils, video_mode};
// CRATES
use crate::cache_persistence::{instant_to_unix, unix_to_instant, PersistedSubredditMetadataEntry, PersistedSubredditNsfwEntry};
use crate::post_cache::{FeedSessionRequest, FeedSort, FeedSource};
use crate::utils::{
	catch_random, deflate_compress, deflate_decompress, error, filter_posts, format_num, format_url, get_filters, info, nsfw_landing, param, redirect, rewrite_urls, setting,
	template, val, FlairSubscription, Post, Preferences, Subreddit,
};
use crate::{client::json, server::RequestExt, server::ResponseExt};
use askama::Template;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use cookie::Cookie;
use htmlescape::decode_html;
use hyper::header::CONTENT_TYPE;
use hyper::{Body, Request, Response};
use log::{info, warn};

use chrono::DateTime;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinSet;
use url::form_urlencoded;

const FRONT_CURSOR_PARAM: &str = "front_cursor";
const FEED_SESSION_PARAM: &str = "feed_session";
const FOLLOWING_FRONT_PAGE: &str = "following";
const FOLLOWING_ROUTE: &str = "/following";
const REDDIT_LISTING_PATH_SOFT_LIMIT: usize = 7_000;
const TARGET_POST_COUNT: usize = 25;
const MAX_FILTERED_PAGE_ADVANCE: usize = 3;
const MAX_AGGREGATE_FILL_ROUNDS: usize = 3;
const AFTER_PATH_HEADROOM: usize = 128;
const MAX_SUBREDDITS_PER_LISTING_CHUNK: usize = 100;
const AGGREGATE_FETCH_CONCURRENCY: usize = 4;
const SUBREDDIT_METADATA_CACHE_TTL_SECS: u64 = 10_800;
const OAUTH_CACHE_NOTICE: &str = "Showing cached posts because live Reddit requests are temporarily unavailable. Content may be stale.";
const CONTEXTUAL_PAGINATION_MAX_LINKS: usize = 6;
static HTML_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").unwrap());
static SUBREDDIT_METADATA_CACHE: LazyLock<Mutex<HashMap<String, CachedSubredditMetadata>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SUBREDDIT_NSFW_CACHE: LazyLock<Mutex<HashMap<String, CachedSubredditNsfwStatus>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
struct CachedSubredditMetadata {
	subreddit: Subreddit,
	fetched_at: Instant,
}

#[derive(Clone, Copy)]
struct CachedSubredditNsfwStatus {
	is_nsfw: bool,
	fetched_at: Instant,
}

struct FilteredListingFetch {
	posts: Vec<Post>,
	raw_posts: Vec<Post>,
	after: String,
	all_posts_filtered: bool,
}

// STRUCTS
#[derive(Template)]
#[template(path = "subreddit.html")]
pub struct SubredditTemplate {
	sub: Subreddit,
	posts: Vec<Post>,
	top_users: Vec<SubredditUserEntry>,
	related_subreddits: crate::materialized_catalog::RelatedSubredditReport,
	sort: (String, String),
	sort_options: Vec<SortOption>,
	flair_filters: Vec<FlairFilterOption>,
	active_flair_filter: String,
	active_flair_filter_encoded: String,
	clear_flair_filter_url: String,
	prev_page_url: String,
	next_page_url: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	/// Whether the subreddit itself is filtered.
	is_filtered: bool,
	/// Whether all fetched posts are filtered (to differentiate between no posts fetched in the first place,
	/// and all fetched posts being filtered).
	all_posts_filtered: bool,
	/// Whether all posts were hidden because they are NSFW (and user has disabled show NSFW)
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
	cache_notice: String,
	peer_origin: String,
	is_flair_subscribed: bool,
	feed_session_id: String,
	feed_status_url: String,
	feed_fragment_url: String,
	feed_status_message: String,
	feed_status_warming: bool,
	show_contextual_pagination: bool,
	contextual_pagination: Vec<ContextualPageLink>,
	video_mode_enabled: bool,
	video_mode_sidebar: video_mode::VideoModeSidebarData,
	show_hide_seen_toggle: bool,
	hide_seen_on: bool,
	hide_seen_toggle_url: String,
}

crate::impl_template_str_eq!(SubredditTemplate);

#[derive(Template)]
#[template(path = "subreddit_shell.html")]
pub struct SubredditShellTemplate {
	sub: Subreddit,
	top_users: Vec<SubredditUserEntry>,
	related_subreddits: crate::materialized_catalog::RelatedSubredditReport,
	flair_filters: Vec<FlairFilterOption>,
	active_flair_filter: String,
	active_flair_filter_encoded: String,
	clear_flair_filter_url: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	is_filtered: bool,
	is_flair_subscribed: bool,
	fragment_url: String,
	loading_message: String,
	placeholder_message: String,
	replace_selector: String,
	video_mode_enabled: bool,
	video_mode_sidebar: video_mode::VideoModeSidebarData,
}

crate::impl_template_str_eq!(SubredditShellTemplate);

#[derive(Template)]
#[template(path = "subreddit_fragment.html")]
struct SubredditFragmentTemplate {
	sub: Subreddit,
	posts: Vec<Post>,
	top_users: Vec<SubredditUserEntry>,
	related_subreddits: crate::materialized_catalog::RelatedSubredditReport,
	sort: (String, String),
	sort_options: Vec<SortOption>,
	flair_filters: Vec<FlairFilterOption>,
	active_flair_filter: String,
	active_flair_filter_encoded: String,
	clear_flair_filter_url: String,
	prev_page_url: String,
	next_page_url: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	is_filtered: bool,
	all_posts_filtered: bool,
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
	cache_notice: String,
	peer_origin: String,
	is_flair_subscribed: bool,
	feed_session_id: String,
	feed_status_url: String,
	feed_fragment_url: String,
	show_contextual_pagination: bool,
	contextual_pagination: Vec<ContextualPageLink>,
	video_mode_enabled: bool,
	video_mode_sidebar: video_mode::VideoModeSidebarData,
	show_hide_seen_toggle: bool,
	hide_seen_on: bool,
	hide_seen_toggle_url: String,
}

crate::impl_template_str_eq!(SubredditFragmentTemplate);

const CONTEXTUAL_FRONT_PAGE_SYNC_SOURCE_BUDGET: usize = 24;

fn contextual_front_page_sync_source_budget(cache_page: usize) -> usize {
	CONTEXTUAL_FRONT_PAGE_SYNC_SOURCE_BUDGET.max((cache_page + 1).saturating_mul(crate::post_cache::POSTS_PER_PAGE))
}

#[derive(Template)]
#[template(path = "feed_fragment.html")]
struct FeedFragmentTemplate {
	posts: Vec<Post>,
	prev_page_url: String,
	next_page_url: String,
	prefs: Preferences,
	url: String,
	all_posts_filtered: bool,
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
	show_contextual_pagination: bool,
	contextual_pagination: Vec<ContextualPageLink>,
}

impl FeedFragmentTemplate {
	pub fn str_eq(&self, x: &str, y: &str) -> bool {
		x == y
	}
}

#[derive(PartialEq)]
struct SubredditUserEntry {
	name: String,
	count: u32,
}

fn related_subreddit_sidebar(sub: &Subreddit, prefs: &Preferences, filters: &HashSet<String>) -> crate::materialized_catalog::RelatedSubredditReport {
	if sub.name.is_empty() || sub.name == "all" || sub.name == "popular" || sub.name.contains('+') {
		return crate::materialized_catalog::RelatedSubredditReport::default();
	}
	crate::materialized_catalog::related_subreddit_report(&sub.name, &prefs.subscriptions, filters, 5)
}

fn enqueue_direct_subreddit_embedding_activity(sub_name: &str, posts: &[Post]) {
	if sub_name.is_empty() || sub_name == "all" || sub_name == "popular" || sub_name.contains('+') || posts.is_empty() {
		return;
	}
	let source_key = format!("subreddit:{sub_name}:direct-page");
	crate::observed_semantic_backfill::enqueue_observed_posts(&source_key, posts, crate::observed_semantic_backfill::ObservedBackfillPriority::Priority);
}

#[derive(Clone, PartialEq)]
struct SortOption {
	label: String,
	url: String,
	selected: bool,
}

#[derive(PartialEq)]
struct FlairFilterOption {
	name: String,
	url: String,
	active: bool,
	flair: crate::utils::Flair,
}

#[derive(Clone, PartialEq)]
struct ContextualPageLink {
	label: String,
	url: String,
	current: bool,
	title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum AggregateFeedSource {
	SubredditChunk { chunk: String },
	Flair { subreddit: String, flair: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AggregateFrontPageState {
	source_afters: Vec<String>,
	#[serde(default)]
	next_source_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AggregateFrontCursor {
	v: u8,
	sources: Vec<AggregateFeedSource>,
	source_afters: Vec<String>,
	#[serde(default)]
	next_source_index: usize,
	prev_stack: Vec<AggregateFrontPageState>,
	#[serde(default)]
	recent_post_ids: Vec<String>,
	sort: String,
	timeframe: String,
	query_fingerprint: String,
}

struct ChunkListingPage {
	posts: Vec<Post>,
	after: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SavedFeed {
	Subreddit { name: String },
	Flair { subreddit: String, flair: String },
}

struct AggregatedFeed {
	posts: Vec<Post>,
	next_cursor: Option<String>,
	prev_cursor: Option<String>,
	all_posts_filtered: bool,
	partial_failure: bool,
}

#[derive(Template)]
#[template(path = "wiki.html")]
pub struct WikiTemplate {
	sub: String,
	wiki: String,
	page: String,
	prefs: Preferences,
	url: String,
	shell_mode: bool,
	fragment_url: String,
	loading_message: String,
}

crate::impl_template_str_eq!(WikiTemplate);

#[derive(Template)]
#[template(path = "wiki_fragment.html")]
struct WikiFragmentTemplate {
	sub: String,
	wiki: String,
}

#[derive(Template)]
#[template(path = "wall.html")]
pub struct WallTemplate {
	title: String,
	sub: String,
	msg: String,
	prefs: Preferences,
	url: String,
}

crate::impl_template_str_eq!(WallTemplate);

#[derive(Template)]
#[template(path = "cache_status.html")]
pub struct CacheStatusTemplate {
	prefs: Preferences,
	url: String,
	generated_at_unix: u64,
	summary: crate::post_cache::CacheStatusSummary,
	sources: Vec<crate::post_cache::CacheStatusSource>,
	seconds_until_daemon_tick: u64,
	upcoming_sources: Vec<crate::post_cache::CacheStatusSource>,
	recent_sources: Vec<crate::post_cache::CacheStatusSource>,
	semantic_database: crate::subreddit_semantics::SubredditSemanticDatabaseStatus,
	content_semantic_database: crate::subreddit_content_semantics::SubredditContentSemanticDatabaseStatus,
	promotion_filtered_users: usize,
	redgifs_filtered_users: usize,
}

crate::impl_template_str_eq!(CacheStatusTemplate);

impl CacheStatusTemplate {
	fn human_duration(&self, seconds: &u64) -> String {
		let seconds = *seconds;
		let days = seconds / 86_400;
		let hours = (seconds % 86_400) / 3_600;
		let minutes = (seconds % 3_600) / 60;
		let secs = seconds % 60;

		if days > 0 {
			format!("{days}d {hours}h")
		} else if hours > 0 {
			format!("{hours}h {minutes}m")
		} else if minutes > 0 {
			format!("{minutes}m {secs}s")
		} else {
			format!("{secs}s")
		}
	}

	fn schedule_state_label(&self, state: &str) -> &'static str {
		match state {
			"in_flight" => "refreshing now",
			"scheduled" => "queued for daemon refresh",
			"blocked" => "temporarily backed off after failures",
			"idle" => "remembered, but not currently queued",
			"not_cached" => "not cached yet",
			"session_only" => "only fetched while you browse",
			_ => "unknown",
		}
	}

	fn human_bytes(&self, bytes: &u64) -> String {
		let bytes = *bytes as f64;
		if bytes >= 1024.0 * 1024.0 {
			format!("{:.1} MiB", bytes / 1024.0 / 1024.0)
		} else if bytes >= 1024.0 {
			format!("{:.1} KiB", bytes / 1024.0)
		} else {
			format!("{bytes:.0} B")
		}
	}

	fn backfill_state_label(&self, state: &str) -> &'static str {
		match state {
			"running" => "running",
			"completed" => "completed",
			"completed_with_errors" => "completed with errors",
			"failed" => "failed",
			_ => "unknown",
		}
	}
}

static GEO_FILTER_MATCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"geo_filter=(?<region>\w+)").unwrap());

fn placeholder_subreddit(name: &str) -> Subreddit {
	Subreddit {
		name: name.to_string(),
		title: name.to_string(),
		..Subreddit::default()
	}
}

fn cached_subreddit_metadata(sub: &str) -> Option<Subreddit> {
	let mut cache = SUBREDDIT_METADATA_CACHE.lock().ok()?;
	let key = sub.to_lowercase();
	let entry = cache.get(&key)?;
	if !crate::post_cache::is_subreddit_remembered(sub) && entry.fetched_at.elapsed().as_secs() > SUBREDDIT_METADATA_CACHE_TTL_SECS {
		cache.remove(&key);
		return None;
	}

	Some(entry.subreddit.clone())
}

pub fn cached_subreddit_metadata_for_name(sub: &str) -> Option<Subreddit> {
	cached_subreddit_metadata(sub)
}

pub fn cached_subreddit_metadata_entries() -> Vec<Subreddit> {
	let Ok(cache) = SUBREDDIT_METADATA_CACHE.lock() else {
		return Vec::new();
	};
	let mut entries = cache
		.values()
		.map(|entry| entry.subreddit.clone())
		.filter(|subreddit| !subreddit.name.is_empty())
		.collect::<Vec<_>>();
	entries.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()).then_with(|| left.name.cmp(&right.name)));
	entries
}

fn cached_subreddit_nsfw_status(sub: &str) -> Option<bool> {
	let mut cache = SUBREDDIT_NSFW_CACHE.lock().ok()?;
	let key = sub.to_lowercase();
	let entry = cache.get(&key)?;
	if !crate::post_cache::is_subreddit_remembered(sub) && entry.fetched_at.elapsed().as_secs() > SUBREDDIT_METADATA_CACHE_TTL_SECS {
		cache.remove(&key);
		return None;
	}

	Some(entry.is_nsfw)
}

fn store_subreddit_metadata(subreddit: &Subreddit) {
	if subreddit.name.is_empty() {
		return;
	}

	let Ok(mut cache) = SUBREDDIT_METADATA_CACHE.lock() else {
		return;
	};
	cache.insert(
		subreddit.name.to_lowercase(),
		CachedSubredditMetadata {
			subreddit: subreddit.clone(),
			fetched_at: Instant::now(),
		},
	);
	if let Ok(mut nsfw_cache) = SUBREDDIT_NSFW_CACHE.lock() {
		nsfw_cache.insert(
			subreddit.name.to_lowercase(),
			CachedSubredditNsfwStatus {
				is_nsfw: subreddit.nsfw,
				fetched_at: Instant::now(),
			},
		);
	}
	crate::cache_persistence::mark_dirty();
}

pub(crate) fn remove_subreddit_metadata_cache(sub: &str) {
	let key = sub.to_lowercase();
	if let Ok(mut cache) = SUBREDDIT_METADATA_CACHE.lock() {
		cache.remove(&key);
	}
	if let Ok(mut cache) = SUBREDDIT_NSFW_CACHE.lock() {
		cache.remove(&key);
	}
	crate::cache_persistence::mark_dirty();
}

pub fn cache_subreddit_metadata(subreddit: &Subreddit) {
	store_subreddit_metadata(subreddit);
}

pub(crate) fn snapshot_persisted_metadata(now_unix: u64) -> Vec<PersistedSubredditMetadataEntry> {
	let Ok(cache) = SUBREDDIT_METADATA_CACHE.lock() else {
		return Vec::new();
	};

	cache
		.values()
		.filter(|entry| !entry.subreddit.name.is_empty())
		.map(|entry| PersistedSubredditMetadataEntry {
			subreddit: crate::cache_persistence::PersistedSubreddit::from_runtime(&entry.subreddit),
			fetched_at_unix: instant_to_unix(entry.fetched_at, now_unix),
		})
		.collect()
}

pub(crate) fn snapshot_persisted_nsfw(now_unix: u64) -> Vec<PersistedSubredditNsfwEntry> {
	let Ok(cache) = SUBREDDIT_NSFW_CACHE.lock() else {
		return Vec::new();
	};

	cache
		.iter()
		.map(|(subreddit, entry)| PersistedSubredditNsfwEntry {
			subreddit: subreddit.clone(),
			is_nsfw: entry.is_nsfw,
			fetched_at_unix: instant_to_unix(entry.fetched_at, now_unix),
		})
		.collect()
}

pub(crate) fn restore_persisted_state(
	metadata_entries: Vec<PersistedSubredditMetadataEntry>,
	nsfw_entries: Vec<PersistedSubredditNsfwEntry>,
	_saved_at_unix: u64,
	now_unix: u64,
) {
	let restored_metadata = metadata_entries
		.into_iter()
		.filter_map(|entry| {
			let subreddit = entry.subreddit.into_runtime();
			if subreddit.name.is_empty() {
				return None;
			}
			let age_secs = now_unix.saturating_sub(entry.fetched_at_unix);
			if !crate::post_cache::is_subreddit_remembered(&subreddit.name) && age_secs > SUBREDDIT_METADATA_CACHE_TTL_SECS {
				return None;
			}
			Some((
				subreddit.name.to_lowercase(),
				CachedSubredditMetadata {
					subreddit,
					fetched_at: unix_to_instant(entry.fetched_at_unix, now_unix),
				},
			))
		})
		.collect::<HashMap<_, _>>();

	let restored_nsfw = nsfw_entries
		.into_iter()
		.filter_map(|entry| {
			let subreddit = entry.subreddit.trim().to_lowercase();
			if subreddit.is_empty() {
				return None;
			}
			let age_secs = now_unix.saturating_sub(entry.fetched_at_unix);
			if !crate::post_cache::is_subreddit_remembered(&subreddit) && age_secs > SUBREDDIT_METADATA_CACHE_TTL_SECS {
				return None;
			}
			Some((
				subreddit,
				CachedSubredditNsfwStatus {
					is_nsfw: entry.is_nsfw,
					fetched_at: unix_to_instant(entry.fetched_at_unix, now_unix),
				},
			))
		})
		.collect::<HashMap<_, _>>();

	if let Ok(mut cache) = SUBREDDIT_METADATA_CACHE.lock() {
		cache.extend(restored_metadata);
	}
	if let Ok(mut cache) = SUBREDDIT_NSFW_CACHE.lock() {
		cache.extend(restored_nsfw);
	}
}

fn warm_subreddit_metadata(sub: &str, quarantined: bool) {
	if sub.is_empty() || cached_subreddit_metadata(sub).is_some() {
		return;
	}

	let subreddit_name = sub.to_string();
	tokio::spawn(async move {
		if let Ok(subreddit) = subreddit(&subreddit_name, quarantined).await {
			store_subreddit_metadata(&subreddit);
		}
	});
}

fn wiki_fragment_path(req: &Request<Body>) -> String {
	let query = req.uri().query().unwrap_or_default();
	if query.is_empty() {
		format!("/wiki-fragment{}", req.uri().path())
	} else {
		format!("/wiki-fragment{}?{query}", req.uri().path())
	}
}

fn wiki_source_path(req: &Request<Body>) -> String {
	req.uri().path().replacen("/wiki-fragment", "", 1)
}

fn is_async_subscription_request(req: &Request<Body>) -> bool {
	req.headers().get("X-Redlib-Ajax").and_then(|value| value.to_str().ok()).is_some_and(|value| value == "1")
}

/// 200 + body so intermediaries reliably forward `Set-Cookie` (some strip or mishandle 204).
fn async_subscription_ok_response() -> Response<Body> {
	Response::builder()
		.status(200)
		.header("Cache-Control", "no-store")
		.header(CONTENT_TYPE, "text/plain; charset=utf-8")
		.body(Body::from("OK"))
		.unwrap_or_default()
}

fn update_url_query(url: &str, updates: &[(&str, Option<&str>)]) -> String {
	let (path, query) = url.split_once('?').unwrap_or((url, ""));
	let mut query_pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
		.into_owned()
		.filter(|(key, _)| !updates.iter().any(|(updated_key, _)| key == updated_key))
		.collect();

	for (key, value) in updates {
		if let Some(value) = value.filter(|value| !value.is_empty()) {
			query_pairs.push((key.to_string(), value.to_string()));
		}
	}

	let query = form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish();

	if query.is_empty() {
		path.to_string()
	} else {
		format!("{path}?{query}")
	}
}

fn build_url_from_query_pairs(path: &str, query_pairs: &[(String, String)]) -> String {
	let query = form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish();
	if query.is_empty() {
		path.to_string()
	} else {
		format!("{path}?{query}")
	}
}

fn current_url_parts(url: &str) -> (&str, &str) {
	url.split_once('?').unwrap_or((url, ""))
}

fn is_video_length_sort(sort: &str) -> bool {
	sort == "video-length"
}

fn is_random_sort(sort: &str) -> bool {
	sort == "random"
}

fn is_daemon_discovery_sort(sort: &str) -> bool {
	sort == "discovered"
}

fn canonical_new_sort_url(path: &str, query: Option<&str>) -> String {
	let path = path.strip_suffix("/random").map(|base| format!("{base}/new")).unwrap_or_else(|| path.to_string());
	match query {
		Some(query) if !query.is_empty() => format!("{path}?{query}"),
		_ => path,
	}
}

fn default_post_sort_setting(prefs: &Preferences) -> String {
	if prefs.post_sort.is_empty() {
		"hot".to_string()
	} else {
		prefs.post_sort.clone()
	}
}

fn sort_option_label(method: &str) -> String {
	match method {
		"video-length" => "Video Length".to_string(),
		"random" => "Random".to_string(),
		"discovered" => "Discovered".to_string(),
		_ => format!("{}{}", method.get(0..1).unwrap_or_default().to_uppercase(), method.get(1..).unwrap_or_default()),
	}
}

fn listing_sort_path(base_path: &str, method: &str) -> String {
	if base_path.is_empty() || base_path == "/" {
		format!("/{method}")
	} else {
		format!("{base_path}/{method}")
	}
}

fn empty_video_mode_sidebar() -> video_mode::VideoModeSidebarData {
	video_mode::VideoModeSidebarData::default()
}

fn video_mode_facet_hidden_value(mode: video_mode::FacetMode) -> &'static str {
	match mode {
		video_mode::FacetMode::None => "none",
		video_mode::FacetMode::Exclude => "exclude",
		video_mode::FacetMode::Include => "",
	}
}

fn build_video_mode_sidebar(url: &str, listing: &video_mode::VideoModeListing, page_posts: &[Post], query: &video_mode::VideoModeQuery) -> video_mode::VideoModeSidebarData {
	let (path, query_str) = current_url_parts(url);
	let mut hidden_query_pairs = video_mode::query_pairs(query_str);
	video_mode::strip_video_query_pairs(&mut hidden_query_pairs);
	hidden_query_pairs.retain(|(key, _)| key != "cache_page" && key != "before" && key != "after" && key != FRONT_CURSOR_PARAM && key != FEED_SESSION_PARAM);

	let clear_url = build_url_from_query_pairs(path, &hidden_query_pairs);
	let clear_cookie_redirect_href = format!(
		"/api/video-mode/clear-redirect?{}",
		form_urlencoded::Serializer::new(String::new()).append_pair("redirect", &clear_url).finish()
	);

	let video_sub_mode_value = if !listing.subreddit_options.is_empty() && listing.subreddit_options.iter().all(|option| !option.active) {
		"none".to_string()
	} else {
		video_mode_facet_hidden_value(query.subreddit_mode).to_string()
	};
	let video_user_mode_value = if !listing.user_options.is_empty() && listing.user_options.iter().all(|option| !option.active) {
		"none".to_string()
	} else {
		video_mode_facet_hidden_value(query.user_mode).to_string()
	};

	video_mode::VideoModeSidebarData {
		action_url: path.to_string(),
		clear_url,
		clear_cookie_redirect_href,
		hidden_query_pairs,
		subreddit_options: video_mode::sort_options_by_page_count(&listing.subreddit_options, page_posts, |post| post.community.clone()),
		user_options: video_mode::sort_options_by_page_count(&listing.user_options, page_posts, |post| post.author.name.clone()),
		clear_subreddits: !listing.subreddit_options.is_empty() && listing.subreddit_options.iter().all(|option| !option.active),
		clear_users: !listing.user_options.is_empty() && listing.user_options.iter().all(|option| !option.active),
		video_sub_mode_value,
		video_user_mode_value,
		age_min_hours: listing.age_min_hours,
		age_max_hours: listing.age_max_hours,
		age_bound_hours: listing.age_bound_hours,
	}
}

fn video_mode_page_url(current_url: &str, page: usize, query: &video_mode::VideoModeQuery, embed_video_params_in_url: bool) -> String {
	let (path, raw_query) = current_url_parts(current_url);
	let mut query_pairs = video_mode::query_pairs(raw_query);
	query_pairs.retain(|(key, _)| key != "cache_page" && key != "before" && key != "after" && key != FRONT_CURSOR_PARAM && key != FEED_SESSION_PARAM);
	video_mode::strip_video_query_pairs(&mut query_pairs);
	if embed_video_params_in_url {
		video_mode::append_query_pairs(&mut query_pairs, query);
	}
	if page > 0 {
		query_pairs.push(("cache_page".to_string(), page.to_string()));
	}
	build_url_from_query_pairs(path, &query_pairs)
}

fn filter_posts_by_flair(posts: &mut Vec<Post>, flair: &str) {
	let flair = flair.trim();
	if flair.is_empty() {
		return;
	}

	posts.retain(|post| post.flair.text.trim() == flair);
}

fn feed_status_url(current_url: &str, session_id: &str, page: usize) -> String {
	update_url_query(
		"/feed-status",
		&[
			(FEED_SESSION_PARAM, Some(session_id)),
			("cache_page", Some(&page.to_string())),
			("current_url", Some(current_url)),
		],
	)
}

fn feed_fragment_url(current_url: &str, session_id: &str, page: usize) -> String {
	update_url_query(
		"/feed-fragment",
		&[
			(FEED_SESSION_PARAM, Some(session_id)),
			("cache_page", Some(&page.to_string())),
			("current_url", Some(current_url)),
		],
	)
}

fn subreddit_fragment_path(req: &Request<Body>) -> String {
	let path = req.uri().path();
	let query = req.uri().query().unwrap_or_default();
	let fragment_path = if path == "/" {
		"/subreddit-fragment".to_string()
	} else {
		format!("/subreddit-fragment{path}")
	};
	if query.is_empty() {
		fragment_path
	} else {
		format!("{fragment_path}?{query}")
	}
}

fn render_feed_fragment(mut template_data: FeedFragmentTemplate) -> Result<Response<Body>, String> {
	utils::refresh_post_times(&mut template_data.posts);
	crate::client::preload_post_images(&template_data.posts, &template_data.prefs);
	let body = template_data.render().map_err(|error| error.to_string())?;
	Response::builder()
		.status(200)
		.header("content-type", "text/html; charset=utf-8")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

fn render_subreddit_fragment(
	_req: &Request<Body>,
	mut template_data: SubredditFragmentTemplate,
	feed_seen: Option<seen_posts::FeedSeenCookieCtx>,
) -> Result<Response<Body>, String> {
	if let Some(ref ctx) = feed_seen {
		if ctx.applies {
			template_data.show_hide_seen_toggle = true;
			template_data.hide_seen_on = ctx.hide_seen_on;
			template_data.hide_seen_toggle_url = update_url_query(&template_data.url, &[("hide_seen", Some(if ctx.hide_seen_on { "off" } else { "on" }))]);
		}
	} else {
		template_data.show_hide_seen_toggle = false;
		template_data.hide_seen_on = false;
		template_data.hide_seen_toggle_url.clear();
	}

	utils::refresh_post_times(&mut template_data.posts);
	crate::client::preload_post_images(&template_data.posts, &template_data.prefs);
	let body = template_data.render().map_err(|error| error.to_string())?;
	let mut response = Response::builder()
		.status(200)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.map_err(|error| error.to_string())?;

	if let Some(ref ctx) = feed_seen {
		if ctx.applies {
			seen_posts::attach_rl_feed_cookies(&mut response, &ctx.session_id, ctx.needs_session_cookie, ctx.hide_seen_cookie_override.as_deref());
		}
	}
	Ok(response)
}

fn subreddit_source_unavailable_notice(sub_name: &str, reason: crate::client::SourceUnavailable) -> String {
	match reason {
		crate::client::SourceUnavailable::Banned => format!("r/{sub_name} has been banned from Reddit."),
		crate::client::SourceUnavailable::Private => format!("r/{sub_name} is a private community on Reddit."),
		crate::client::SourceUnavailable::Suspended => format!("r/{sub_name} is unavailable from Reddit."),
		crate::client::SourceUnavailable::NotFound => format!("r/{sub_name} was not found on Reddit."),
		crate::client::SourceUnavailable::Forbidden => format!("r/{sub_name} is unavailable from Reddit."),
	}
}

#[allow(clippy::too_many_arguments)]
fn render_subreddit_source_unavailable_fragment(
	req: &Request<Body>,
	sub: Subreddit,
	sub_name: &str,
	reason: crate::client::SourceUnavailable,
	sort: (String, String),
	sort_options: Vec<SortOption>,
	flair_filters: Vec<FlairFilterOption>,
	active_flair_filter: &str,
	url: String,
	redirect_url: String,
	is_flair_subscribed: bool,
	filters: &HashSet<String>,
	top_users: Vec<SubredditUserEntry>,
	feed_seen: Option<seen_posts::FeedSeenCookieCtx>,
) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(req);
	let related_subreddits = related_subreddit_sidebar(&sub, &prefs, filters);
	render_subreddit_fragment(
		req,
		SubredditFragmentTemplate {
			sub,
			posts: Vec::new(),
			top_users,
			related_subreddits,
			sort,
			sort_options,
			flair_filters,
			active_flair_filter: active_flair_filter.to_string(),
			active_flair_filter_encoded: encode_query_value(active_flair_filter),
			clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)]),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs,
			url,
			redirect_url,
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			cache_notice: subreddit_source_unavailable_notice(sub_name, reason),
			peer_origin: String::new(),
			is_flair_subscribed,
			feed_session_id: String::new(),
			feed_status_url: String::new(),
			feed_fragment_url: String::new(),
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
			video_mode_enabled: false,
			video_mode_sidebar: empty_video_mode_sidebar(),
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		},
		feed_seen,
	)
}

fn render_scraped_subreddit_fragment(
	req: &Request<Body>,
	scraped: instance_scrape::ScrapedSubredditPage,
	sub_name: &str,
	sort: &str,
	timeframe: &str,
	sort_options: Vec<SortOption>,
	active_flair_filter: &str,
	url: &str,
	redirect_url: &str,
	filters: &HashSet<String>,
	request_before: &str,
	is_flair_subscribed: bool,
	cacheable_first_page: bool,
	cache_notice: &str,
	peer_origin: &str,
	source_refresh_token: Option<crate::post_cache::SourceRefreshToken>,
) -> Result<Response<Body>, String> {
	if cacheable_first_page {
		if let Some(token) = source_refresh_token {
			let committed_subreddit = scraped.subreddit.clone();
			crate::post_cache::publish_plain_subreddit_refresh_with_token_and_effects(
				sub_name,
				scraped.posts.clone(),
				scraped.after.clone(),
				crate::post_cache::RefreshOrigin::DirectLiveFetch,
				if peer_origin.is_empty() {
					crate::post_components::ObservationSource::Scrape
				} else {
					crate::post_components::ObservationSource::Peer
				},
				token,
				move |_| cache_subreddit_metadata(&committed_subreddit),
			);
		}
	} else {
		cache_subreddit_metadata(&scraped.subreddit);
		crate::post_components::observe_posts(
			&scraped.posts,
			if peer_origin.is_empty() {
				crate::post_components::ObservationSource::Scrape
			} else {
				crate::post_components::ObservationSource::Peer
			},
			if peer_origin.is_empty() { "instance-scrape" } else { peer_origin },
			crate::cache_persistence::unix_now(),
		);
		utils::store_post_flairs(&scraped.posts);
		enqueue_direct_subreddit_embedding_activity(sub_name, &scraped.posts);
		utils::store_subreddit_users(sub_name, &scraped.posts);
	}

	let mut posts = scraped.posts;
	let (_, all_posts_filtered) = filter_posts(&mut posts, filters);
	filter_posts_by_flair(&mut posts, active_flair_filter);
	if sort == "new" {
		posts.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
		posts.sort_by(|a, b| b.flags.stickied.cmp(&a.flags.stickied));
	} else if is_random_sort(sort) {
		crate::post_cache::sort_posts_by_deterministic_random(&mut posts);
	}
	let no_posts = posts.is_empty();
	let prefs = Preferences::new(req);
	let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(req, "show_nsfw") != "on");
	utils::hydrate_post_author_links_from_cache(&mut posts);
	let related_subreddits = related_subreddit_sidebar(&scraped.subreddit, &prefs, filters);

	render_subreddit_fragment(
		req,
		SubredditFragmentTemplate {
			sub: scraped.subreddit,
			posts,
			top_users: utils::cached_subreddit_users(sub_name)
				.into_iter()
				.map(|(name, count)| SubredditUserEntry { name, count })
				.collect(),
			related_subreddits,
			sort: (sort.to_string(), timeframe.to_string()),
			sort_options,
			flair_filters: build_flair_filters(url, active_flair_filter, utils::cached_subreddit_flairs(sub_name)),
			active_flair_filter: active_flair_filter.to_string(),
			active_flair_filter_encoded: encode_query_value(active_flair_filter),
			clear_flair_filter_url: update_url_query(url, &[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)]),
			prev_page_url: if !request_before.is_empty() {
				update_url_query(url, &[("sort", Some(sort)), ("t", Some(timeframe)), ("before", Some(request_before)), ("after", None)])
			} else {
				String::new()
			},
			next_page_url: if !scraped.after.is_empty() {
				update_url_query(
					url,
					&[("sort", Some(sort)), ("t", Some(timeframe)), ("before", None), ("after", Some(scraped.after.as_str()))],
				)
			} else {
				String::new()
			},
			prefs,
			url: url.to_string(),
			redirect_url: redirect_url.to_string(),
			is_filtered: false,
			all_posts_filtered,
			all_posts_hidden_nsfw,
			no_posts,
			cache_notice: cache_notice.to_string(),
			peer_origin: peer_origin.to_string(),
			is_flair_subscribed,
			feed_session_id: String::new(),
			feed_status_url: String::new(),
			feed_fragment_url: String::new(),
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
			video_mode_enabled: false,
			video_mode_sidebar: empty_video_mode_sidebar(),
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		},
		None,
	)
}

async fn render_peer_api_subreddit_fragment(
	req: &Request<Body>,
	sub_name: &str,
	sort: &str,
	sort_options: Vec<SortOption>,
	active_flair_filter: &str,
	url: &str,
	redirect_url: &str,
	filters: &HashSet<String>,
	request_before: &str,
	is_flair_subscribed: bool,
	cacheable_first_page: bool,
	query: Option<&str>,
	timeframe: &str,
) -> Option<Result<Response<Body>, String>> {
	if !peer_api_fallback::peer_api_fallback_enabled() {
		return None;
	}
	let refresh_token = cacheable_first_page.then(|| crate::post_cache::begin_source_refresh(&FeedSource::subreddit(sub_name.to_string(), FeedSort::New)));
	match peer_api_fallback::fetch_subreddit_listing(sub_name, sort, query).await {
		Ok(peer) => {
			info!("subreddit: rendering {} via configured Redlib peer API {}", url, peer.origin);
			let scraped = instance_scrape::ScrapedSubredditPage {
				subreddit: peer.subreddit.unwrap_or_else(|| placeholder_subreddit(sub_name)),
				posts: peer.posts,
				after: peer.after,
			};
			Some(render_scraped_subreddit_fragment(
				req,
				scraped,
				sub_name,
				sort,
				timeframe,
				sort_options,
				active_flair_filter,
				url,
				redirect_url,
				filters,
				request_before,
				is_flair_subscribed,
				cacheable_first_page,
				peer_api_fallback::peer_api_notice(),
				&peer.origin,
				refresh_token,
			))
		}
		Err(error) => {
			warn!("subreddit: Redlib peer API fallback failed for {}: {}", url, error);
			None
		}
	}
}

fn build_sort_options(base_path: &str, current_query_url: &str, selected_sort: &str, flair_filter: &str) -> Vec<SortOption> {
	let (_, query) = current_url_parts(current_query_url);
	let methods = if base_path == FOLLOWING_ROUTE {
		vec!["new", "discovered", "hot", "random", "top", "rising", "controversial", "video-length"]
	} else if base_path.is_empty() {
		vec!["hot", "new", "discovered", "random", "top", "rising", "controversial", "video-length"]
	} else {
		vec!["hot", "new", "top", "rising", "controversial", "video-length"]
	};
	methods
		.into_iter()
		.map(|method| SortOption {
			label: if base_path == FOLLOWING_ROUTE && method == "new" {
				"Chronological".to_string()
			} else {
				sort_option_label(method)
			},
			url: {
				let mut query_pairs = video_mode::query_pairs(query);
				query_pairs.retain(|(key, _)| key != "before" && key != "after" && key != FRONT_CURSOR_PARAM && key != "cache_page" && key != FEED_SESSION_PARAM);
				if method != "video-length" || !is_video_length_sort(selected_sort) {
					video_mode::strip_video_query_pairs(&mut query_pairs);
				}
				if flair_filter.is_empty() {
					query_pairs.retain(|(key, _)| key != "flair");
				}
				build_url_from_query_pairs(&listing_sort_path(base_path, method), &query_pairs)
			},
			selected: method == selected_sort,
		})
		.collect()
}

fn build_flair_filters(current_url: &str, active_flair_filter: &str, flairs: Vec<crate::utils::Flair>) -> Vec<FlairFilterOption> {
	flairs
		.into_iter()
		.filter(|flair| !flair.text.trim().is_empty())
		.map(|flair| FlairFilterOption {
			name: flair.text.clone(),
			url: update_url_query(
				current_url,
				&[("flair", Some(flair.text.as_str())), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)],
			),
			active: flair.text == active_flair_filter,
			flair,
		})
		.collect()
}

pub(crate) fn encode_query_value(value: &str) -> String {
	form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn normalized_video_mode_query(query: &video_mode::VideoModeQuery, listing: &video_mode::VideoModeListing) -> video_mode::VideoModeQuery {
	video_mode::VideoModeQuery {
		selected_subreddits: match query.subreddit_mode {
			video_mode::FacetMode::Include => video_mode::normalize_selected_values(&query.selected_subreddits, &listing.subreddit_options),
			_ => query.selected_subreddits.clone(),
		},
		selected_users: match query.user_mode {
			video_mode::FacetMode::Include => video_mode::normalize_selected_values(&query.selected_users, &listing.user_options),
			_ => query.selected_users.clone(),
		},
		subreddit_mode: query.subreddit_mode,
		user_mode: query.user_mode,
		age_min_hours: listing.age_min_hours,
		age_max_hours: Some(listing.age_max_hours),
	}
}

fn paginate_posts(posts: &[Post], page: usize, per_page: usize) -> (Vec<Post>, bool, bool) {
	let start = page.saturating_mul(per_page);
	if start >= posts.len() {
		return (Vec::new(), page > 0, false);
	}
	let end = (start + per_page).min(posts.len());
	(posts[start..end].to_vec(), page > 0, end < posts.len())
}

fn video_mode_visibility_counts(posts: &[Post], filters: &HashSet<String>, now_unix: u64) -> (usize, usize) {
	let eligible = posts
		.iter()
		.filter(|post| !crate::utils::post_matches_filter_policy(post, filters))
		.filter(|post| !crate::utils::is_deleted_reddit_placeholder_post(post))
		.filter(|post| video_mode::is_video_post(post))
		.count();
	let eligible_with_timestamp = posts
		.iter()
		.filter(|post| !crate::utils::post_matches_filter_policy(post, filters))
		.filter(|post| !crate::utils::is_deleted_reddit_placeholder_post(post))
		.filter(|post| video_mode::is_video_post(post))
		.filter(|post| now_unix >= post.created_ts)
		.count();
	(eligible, eligible_with_timestamp)
}

fn collect_front_page_video_candidates(profile_id: &str, view_kind: i64, sources: &[FeedSource], filters: &HashSet<String>) -> Vec<Post> {
	crate::post_cache::catalog_profile_video_candidates(profile_id, view_kind, sources, filters)
}

async fn cached_or_live_new_subreddit_posts(subreddit: &str, quarantined: bool) -> Result<Vec<Post>, String> {
	let source = FeedSource::subreddit(subreddit.to_string(), FeedSort::New);
	if let Some(posts) = crate::post_cache::servable_cached_source_posts(&source) {
		return Ok(posts);
	}

	let path = format!("/r/{}/new.json?limit={}&raw_json=1", subreddit.replace('+', "%2B"), crate::post_cache::per_source_limit());
	let refresh_token = crate::post_cache::begin_source_refresh(&source);
	let (posts, after) = Post::fetch(&path, quarantined).await?;
	crate::post_cache::publish_plain_subreddit_refresh_with_token(subreddit, posts.clone(), after, crate::post_cache::RefreshOrigin::DirectLiveFetch, refresh_token);
	Ok(posts)
}

fn render_video_mode_subreddit_fragment(
	req: &Request<Body>,
	sub: Subreddit,
	source_posts: Vec<Post>,
	top_users: Vec<SubredditUserEntry>,
	sort: String,
	sort_options: Vec<SortOption>,
	flair_filters: Vec<FlairFilterOption>,
	active_flair_filter: String,
	url: String,
	redirect_url: String,
	is_flair_subscribed: bool,
	filters: &HashSet<String>,
	cache_page: usize,
	cache_notice: String,
	feed_seen: Option<seen_posts::FeedSeenCookieCtx>,
) -> Result<Response<Body>, String> {
	let query = req.uri().query().unwrap_or_default();
	let cookie_value = crate::utils::read_sharded_cookie(req, video_mode::VIDEO_MODE_QUERY_COOKIE);
	let parsed_query = video_mode::parse_query_or_cookie(query, &cookie_value);
	let listing = video_mode::build_listing(&source_posts, filters, &parsed_query, now_unix());
	let normalized_query = normalized_video_mode_query(&parsed_query, &listing);
	let request_has_video_params = video_mode::query_has_video_params(query);
	let (page_posts, has_prev, has_next) = paginate_posts(&listing.posts, cache_page, crate::post_cache::POSTS_PER_PAGE);
	let prefs = Preferences::new(req);
	let no_posts = listing.posts.is_empty();
	let all_posts_hidden_nsfw = !page_posts.is_empty() && (page_posts.iter().all(|p| p.flags.nsfw) && setting(req, "show_nsfw") != "on");
	let (eligible_videos, _) = video_mode_visibility_counts(&source_posts, filters, now_unix());
	let all_posts_filtered = eligible_videos > 0 && listing.posts.is_empty();
	let mut page_posts = page_posts;
	utils::hydrate_post_author_links_from_cache(&mut page_posts);
	let video_mode_sidebar = build_video_mode_sidebar(&url, &listing, &page_posts, &normalized_query);
	let related_subreddits = related_subreddit_sidebar(&sub, &prefs, filters);

	render_subreddit_fragment(
		req,
		SubredditFragmentTemplate {
			sub,
			posts: page_posts,
			top_users,
			related_subreddits,
			sort: (sort, String::new()),
			sort_options,
			flair_filters,
			active_flair_filter: active_flair_filter.clone(),
			active_flair_filter_encoded: encode_query_value(&active_flair_filter),
			clear_flair_filter_url: update_url_query(
				&url,
				&[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None), ("cache_page", None)],
			),
			prev_page_url: if has_prev {
				video_mode_page_url(&url, cache_page.saturating_sub(1), &normalized_query, request_has_video_params)
			} else {
				String::new()
			},
			next_page_url: if has_next {
				video_mode_page_url(&url, cache_page + 1, &normalized_query, request_has_video_params)
			} else {
				String::new()
			},
			prefs,
			url: url.clone(),
			redirect_url,
			is_filtered: false,
			all_posts_filtered,
			all_posts_hidden_nsfw,
			no_posts,
			cache_notice,
			peer_origin: String::new(),
			is_flair_subscribed,
			feed_session_id: String::new(),
			feed_status_url: String::new(),
			feed_fragment_url: String::new(),
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
			video_mode_enabled: true,
			video_mode_sidebar,
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		},
		feed_seen,
	)
}

fn subreddit_listing_query(query: &str, include_geo_filter: bool) -> String {
	let mut query_pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
		.into_owned()
		.filter(|(key, _)| key != "flair" && key != "before" && key != "after" && key != FRONT_CURSOR_PARAM)
		.collect();

	if include_geo_filter && !query_pairs.iter().any(|(key, _)| key == "geo_filter") {
		let geo_filter = match GEO_FILTER_MATCH.captures(query) {
			Some(geo_filter) => geo_filter["region"].to_string(),
			None => "GLOBAL".to_owned(),
		};
		query_pairs.push(("geo_filter".to_string(), geo_filter));
	}

	query_pairs.push(("raw_json".to_string(), "1".to_string()));

	form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish()
}

fn listing_query_fingerprint(query: &str) -> String {
	let mut query_pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
		.into_owned()
		.filter(|(key, _)| key != FRONT_CURSOR_PARAM && key != "before" && key != "after" && key != "cache_page" && key != FEED_SESSION_PARAM)
		.collect();
	query_pairs.sort();

	form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish()
}

fn split_subscriptions_for_listing(subs: &[String], sort: &str, query: &str) -> Vec<String> {
	let mut chunks = Vec::new();
	let mut current = Vec::new();

	for sub in subs {
		let mut next = current.clone();
		next.push(sub.clone());
		let chunk = next.join("+");
		let projected_len = format!("/r/{chunk}/{sort}.json?{query}").len() + AFTER_PATH_HEADROOM;
		let projected_count = next.len();

		if !current.is_empty() && (projected_len > REDDIT_LISTING_PATH_SOFT_LIMIT || projected_count > MAX_SUBREDDITS_PER_LISTING_CHUNK) {
			chunks.push(current.join("+"));
			current = vec![sub.clone()];
		} else {
			current = next;
		}
	}

	if !current.is_empty() {
		chunks.push(current.join("+"));
	}

	chunks
}

fn saved_feeds(preferences: &Preferences) -> Vec<SavedFeed> {
	let mut feeds = preferences.subscriptions.iter().cloned().map(|name| SavedFeed::Subreddit { name }).collect::<Vec<_>>();

	feeds.extend(preferences.flair_subscriptions.iter().cloned().map(|subscription| SavedFeed::Flair {
		subreddit: subscription.subreddit,
		flair: subscription.flair,
	}));

	feeds
}

fn is_followed_user_subscription(name: &str) -> bool {
	name.starts_with("u_")
}

fn followed_user_subscriptions(preferences: &Preferences) -> Vec<String> {
	preferences.subscriptions.iter().filter(|name| is_followed_user_subscription(name)).cloned().collect()
}

fn ordered_front_page_subscriptions(subscriptions: &[String]) -> Vec<String> {
	let mut followed_users = Vec::new();
	let mut followed_subreddits = Vec::new();

	for subscription in subscriptions {
		if is_followed_user_subscription(subscription) {
			followed_users.push(subscription.clone());
		} else {
			followed_subreddits.push(subscription.clone());
		}
	}

	followed_users.sort_by_key(|name| name.to_lowercase());
	followed_subreddits.sort_by_key(|name| name.to_lowercase());

	if followed_users.is_empty() {
		return followed_subreddits;
	}
	if followed_subreddits.is_empty() {
		return followed_users;
	}

	let user_count = followed_users.len();
	let subreddit_count = followed_subreddits.len();
	let total_count = subscriptions.len();
	let mut ordered = Vec::with_capacity(total_count);
	let mut user_index = 0;
	let mut subreddit_index = 0;
	let mut user_credit = 0;
	let mut subreddit_credit = 0;
	while user_index < followed_users.len() || subreddit_index < followed_subreddits.len() {
		user_credit += user_count;
		subreddit_credit += subreddit_count;
		let choose_user = if user_index >= followed_users.len() {
			false
		} else if subreddit_index >= followed_subreddits.len() {
			true
		} else if user_credit == subreddit_credit {
			followed_users[user_index].to_lowercase() < followed_subreddits[subreddit_index].to_lowercase()
		} else {
			user_credit > subreddit_credit
		};

		if choose_user {
			ordered.push(followed_users[user_index].clone());
			user_index += 1;
			user_credit = user_credit.saturating_sub(total_count);
		} else {
			ordered.push(followed_subreddits[subreddit_index].clone());
			subreddit_index += 1;
			subreddit_credit = subreddit_credit.saturating_sub(total_count);
		}
	}

	ordered
}

fn is_following_route(path: &str) -> bool {
	let path = path.strip_prefix("/subreddit-fragment").unwrap_or(path);
	path == FOLLOWING_ROUTE || path.starts_with("/following/")
}

fn front_page_override(path: &str) -> Option<&'static str> {
	if is_following_route(path) {
		Some(FOLLOWING_FRONT_PAGE)
	} else {
		None
	}
}

fn is_top_level_random_route(path: &str) -> bool {
	let path = path.strip_prefix("/subreddit-fragment").unwrap_or(path);
	path == "/random"
}

fn front_page_for_path(path: &str, configured_front_page: &str) -> String {
	if let Some(front_page) = front_page_override(path) {
		front_page.to_string()
	} else if is_top_level_random_route(path) {
		"default".to_string()
	} else {
		configured_front_page.to_string()
	}
}

fn is_following_front_page(front_page: &str) -> bool {
	front_page == FOLLOWING_FRONT_PAGE
}

/// Listing sort for merge/finalize when source fetch sort intentionally differs from display sort.
fn contextual_feed_merge_sort(front_page: &str, sort: &str) -> Option<FeedSort> {
	(is_following_front_page(front_page) || is_random_sort(sort) || is_daemon_discovery_sort(sort)).then(|| FeedSort::from_listing(sort))
}

fn build_subscription_sources(subscriptions: &[String], sort: FeedSort) -> Vec<FeedSource> {
	let mut sources = subscriptions.iter().cloned().map(|name| FeedSource::subreddit(name, sort)).collect::<Vec<_>>();
	crate::post_cache::sort_sources(&mut sources);
	sources
}

fn build_feed_sources(preferences: &Preferences, sort: FeedSort) -> Vec<FeedSource> {
	let mut sources = ordered_front_page_subscriptions(&preferences.subscriptions)
		.into_iter()
		.map(|name| FeedSource::subreddit(name, sort))
		.collect::<Vec<_>>();

	let mut flair_sources = preferences
		.flair_subscriptions
		.iter()
		.cloned()
		.map(|subscription| FeedSource::flair(subscription.subreddit, subscription.flair, sort))
		.collect::<Vec<_>>();
	crate::post_cache::sort_sources(&mut flair_sources);
	sources.extend(flair_sources);
	sources
}

fn build_video_mode_front_page_sources(preferences: &Preferences, front_page: &str) -> Vec<FeedSource> {
	let sort = FeedSort::New;
	if is_following_front_page(front_page) {
		build_subscription_sources(&followed_user_subscriptions(preferences), sort)
	} else {
		build_feed_sources(preferences, sort)
	}
}

fn front_page_uses_contextual_session(is_subreddit_route: bool, sort: &str) -> bool {
	!is_subreddit_route && matches!(sort, "new" | "hot" | "random" | "discovered")
}

fn build_front_page_session_sources(
	preferences: &Preferences,
	front_page: &str,
	front_page_is_default: bool,
	has_saved_feeds: bool,
	sub_name: &str,
	sort: &str,
) -> Vec<FeedSource> {
	let listing_sort = if is_random_sort(sort) { FeedSort::New } else { FeedSort::from_listing(sort) };
	let mut sources = if front_page_is_default && has_saved_feeds {
		build_feed_sources(preferences, listing_sort)
	} else if is_following_front_page(front_page) && has_saved_feeds {
		let mut sources = Vec::new();
		// Remembered-feed daemon only refreshes `New` user listings; match that cache key so
		// `/following` reuses daemon-filled `u_` pages instead of warming hot-specific keys.
		let fetch_sort = FeedSort::New;
		for name in followed_user_subscriptions(preferences) {
			sources.push(FeedSource::subreddit(name.clone(), fetch_sort));
			sources.push(FeedSource::user_comments(name, fetch_sort));
		}
		sources
	} else if sub_name.is_empty() {
		Vec::new()
	} else {
		vec![FeedSource::subreddit(sub_name.to_string(), listing_sort)]
	};
	if !(front_page_is_default && has_saved_feeds) {
		crate::post_cache::sort_sources(&mut sources);
	}
	sources
}

/// Uses [`post_cache::feed_page`] output for plain paging so frozen/sealed pages stay stable while
/// hide-seen mode still walks the full merged feed.
fn contextual_listing_posts_from_feed_page(
	feed_seen: Option<&seen_posts::FeedSeenCookieCtx>,
	cache_page: usize,
	page: &mut crate::post_cache::FeedPage,
	merged_posts: Option<&[Post]>,
) -> (Vec<Post>, bool) {
	let per_page = crate::post_cache::POSTS_PER_PAGE;
	match feed_seen {
		Some(ctx) if ctx.hide_seen_on && cache_page == 0 => {
			let merged_posts = merged_posts.unwrap_or(&page.posts);
			seen_posts::page_and_record_seen_contextual_first_page(&ctx.session_id, &page.posts, merged_posts, per_page)
		}
		Some(ctx) if ctx.hide_seen_on => {
			let merged_posts = merged_posts.unwrap_or(&page.posts);
			seen_posts::page_and_record_seen(&ctx.session_id, true, merged_posts, cache_page, per_page)
		}
		Some(ctx) => {
			let posts = std::mem::take(&mut page.posts);
			seen_posts::record_seen(&ctx.session_id, &posts);
			(posts, false)
		}
		None => {
			let posts = std::mem::take(&mut page.posts);
			seen_posts::record_seen("anon", &posts);
			(posts, false)
		}
	}
}

fn build_aggregate_sources(preferences: &Preferences, front_page: &str, sort: &str, reddit_query: &str) -> Vec<AggregateFeedSource> {
	let subscriptions = if is_following_front_page(front_page) {
		followed_user_subscriptions(preferences)
	} else {
		ordered_front_page_subscriptions(&preferences.subscriptions)
	};

	let mut sources = split_subscriptions_for_listing(&subscriptions, sort, reddit_query)
		.into_iter()
		.map(|chunk| AggregateFeedSource::SubredditChunk { chunk })
		.collect::<Vec<_>>();

	if !is_following_front_page(front_page) {
		sources.extend(preferences.flair_subscriptions.iter().cloned().map(|subscription| AggregateFeedSource::Flair {
			subreddit: subscription.subreddit,
			flair: subscription.flair,
		}));
	}

	sources
}

fn build_listing_path(chunk: &str, sort: &str, base_query: &str, after: &str, before: &str) -> String {
	let mut query_pairs: Vec<(String, String)> = form_urlencoded::parse(base_query.as_bytes()).into_owned().collect();
	if !after.is_empty() {
		query_pairs.push(("after".to_string(), after.to_string()));
	}
	if !before.is_empty() {
		query_pairs.push(("before".to_string(), before.to_string()));
	}

	let query = form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish();

	let reddit_sort = if is_random_sort(sort) || is_daemon_discovery_sort(sort) { "new" } else { sort };
	format!("/r/{}/{reddit_sort}.json?{query}", chunk.replace('+', "%2B"))
}

async fn fetch_listing_skipping_filtered_pages(path: &str, quarantined: bool, filters: &HashSet<String>) -> Result<FilteredListingFetch, String> {
	let mut request_path = path.to_string();
	let mut raw_posts = Vec::new();
	let mut fetched_any_raw_posts = false;
	let mut last_after = String::new();

	// A page can be non-empty on Reddit but empty after local filters; advance a few cursors
	// so one filtered page does not look like the end of the listing.
	for advance_count in 0..=MAX_FILTERED_PAGE_ADVANCE {
		let (page_raw_posts, next_after) = Post::fetch(&request_path, quarantined).await?;
		fetched_any_raw_posts |= !page_raw_posts.is_empty();
		raw_posts.extend(page_raw_posts.clone());

		let mut posts = page_raw_posts;
		let (_, page_all_posts_filtered) = filter_posts(&mut posts, filters);
		last_after = next_after.clone();

		if !page_all_posts_filtered || next_after.is_empty() || advance_count == MAX_FILTERED_PAGE_ADVANCE {
			let all_posts_filtered = fetched_any_raw_posts && posts.is_empty();
			return Ok(FilteredListingFetch {
				posts,
				raw_posts,
				after: next_after,
				all_posts_filtered,
			});
		}

		request_path = utils::listing_path_with_after(path, &next_after)?;
	}

	Ok(FilteredListingFetch {
		posts: Vec::new(),
		raw_posts,
		after: last_after,
		all_posts_filtered: fetched_any_raw_posts,
	})
}

fn feed_page_url(current_url: &str, session_id: &str, page: usize) -> String {
	update_url_query(
		current_url,
		&[
			("cache_page", Some(&page.to_string())),
			(FEED_SESSION_PARAM, Some(session_id)),
			("before", None),
			("after", None),
			(FRONT_CURSOR_PARAM, None),
		],
	)
}

async fn ensure_contextual_feed_session(requested_session_id: String, request: FeedSessionRequest, cache_page: usize) -> String {
	let expected_session_id = crate::post_cache::feed_session_id_for_request(&request);
	if requested_session_id == expected_session_id
		&& crate::post_cache::continue_feed_session_with_budget(
			&requested_session_id,
			cache_page,
			crate::post_cache::POSTS_PER_PAGE,
			contextual_front_page_sync_source_budget(cache_page),
		)
		.await
	{
		return requested_session_id;
	}

	crate::post_cache::ensure_feed_session_with_budget(request, cache_page, crate::post_cache::POSTS_PER_PAGE, contextual_front_page_sync_source_budget(cache_page)).await
}

fn now_unix() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_secs()).unwrap_or_default()
}

fn random_rotation_seed_for_sort(sort: &str) -> u64 {
	if is_random_sort(sort) {
		crate::post_cache::random_rotation_seed_for_time(now_unix())
	} else {
		0
	}
}

fn approximate_age_label(age_secs: u64) -> String {
	const BUCKETS: &[(u64, &str)] = &[
		(60 * 60, "~1h"),
		(3 * 60 * 60, "~3h"),
		(8 * 60 * 60, "~8h"),
		(24 * 60 * 60, "~1d"),
		(3 * 24 * 60 * 60, "~3d"),
		(7 * 24 * 60 * 60, "~7d"),
		(14 * 24 * 60 * 60, "~14d"),
		(30 * 24 * 60 * 60, "~30d"),
	];
	let (.., label) = BUCKETS.iter().min_by_key(|(bucket, _)| age_secs.abs_diff(*bucket)).copied().unwrap_or((60 * 60, "~1h"));
	label.to_string()
}

fn round_hot_score_anchor(score: i64) -> i64 {
	if score == 0 {
		return 0;
	}
	let magnitude = 10_i64.pow(score.abs().to_string().len().saturating_sub(1) as u32);
	((score as f64 / magnitude as f64).round() as i64) * magnitude
}

fn approximate_hot_score_label(score: i64) -> String {
	format!("~{}", format_num(round_hot_score_anchor(score)).0)
}

fn limit_contextual_links(mut links: Vec<ContextualPageLink>) -> Vec<ContextualPageLink> {
	if links.len() <= CONTEXTUAL_PAGINATION_MAX_LINKS {
		return links;
	}

	let current_index = links.iter().position(|link| link.current).unwrap_or(0);
	let half_window = CONTEXTUAL_PAGINATION_MAX_LINKS / 2;
	let mut start = current_index.saturating_sub(half_window);
	let end = (start + CONTEXTUAL_PAGINATION_MAX_LINKS).min(links.len());
	start = end.saturating_sub(CONTEXTUAL_PAGINATION_MAX_LINKS);
	links.drain(..start);
	links.truncate(end - start);
	links
}

fn should_replace_contextual_label(current_page: usize, existing_page: usize, candidate_page: usize) -> bool {
	let existing_distance = existing_page.abs_diff(current_page);
	let candidate_distance = candidate_page.abs_diff(current_page);
	candidate_distance < existing_distance || (candidate_distance == existing_distance && candidate_page == current_page)
}

fn build_contextual_pagination(current_url: &str, session_id: &str, current_page: usize, sort: &str) -> Vec<ContextualPageLink> {
	if !matches!(sort, "new" | "hot") {
		return Vec::new();
	}

	let Some(page_metrics) = crate::post_cache::feed_session_page_metrics(session_id, crate::post_cache::POSTS_PER_PAGE) else {
		return Vec::new();
	};
	if page_metrics.len() < 3 {
		return Vec::new();
	}

	let now = now_unix();
	let mut links_by_label = HashMap::new();
	for metric in page_metrics {
		if !metric.full_page && !metric.ready && metric.page != current_page {
			continue;
		}

		let (label, title) = match sort {
			"new" => {
				let Some(created_ts) = metric.representative_created_ts else {
					continue;
				};
				let age_secs = now.saturating_sub(created_ts);
				(
					approximate_age_label(age_secs),
					format!("Jump to posts around {}", approximate_age_label(age_secs).trim_start_matches('~')),
				)
			}
			"hot" => {
				let Some(score) = metric.representative_score else {
					continue;
				};
				let label = approximate_hot_score_label(score);
				(label.clone(), format!("Jump to posts around {} score", label.trim_start_matches('~')))
			}
			_ => continue,
		};

		let link = ContextualPageLink {
			label: label.clone(),
			url: feed_page_url(current_url, session_id, metric.page),
			current: metric.page == current_page,
			title,
		};
		match links_by_label.get(&label) {
			Some((existing_page, _)) if !should_replace_contextual_label(current_page, *existing_page, metric.page) => {}
			_ => {
				links_by_label.insert(label, (metric.page, link));
			}
		}
	}

	let mut links = links_by_label.into_values().collect::<Vec<_>>();
	links.sort_by_key(|(page, _)| *page);
	let links = links.into_iter().map(|(_, link)| link).collect::<Vec<_>>();
	let non_current = links.iter().filter(|link| !link.current).count();
	if non_current < 2 {
		return Vec::new();
	}

	limit_contextual_links(links)
}

fn encode_front_cursor(cursor: &AggregateFrontCursor) -> Option<String> {
	let serialized = serde_json::to_vec(cursor).ok()?;
	let compressed = deflate_compress(serialized).ok()?;
	Some(URL_SAFE_NO_PAD.encode(compressed))
}

fn decode_front_cursor(encoded: &str) -> Option<AggregateFrontCursor> {
	let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
	let decompressed = deflate_decompress(decoded).ok()?;
	let cursor: AggregateFrontCursor = serde_json::from_slice(&decompressed).ok()?;
	(cursor.v == 1).then_some(cursor)
}

fn build_front_cursor_url(current_url: &str, encoded_cursor: &str) -> String {
	update_url_query(current_url, &[(FRONT_CURSOR_PARAM, Some(encoded_cursor)), ("before", None), ("after", None)])
}

fn cursor_matches_request(cursor: &AggregateFrontCursor, sources: &[AggregateFeedSource], sort: &str, timeframe: &str, query_fingerprint: &str) -> bool {
	cursor.sources == sources
		&& cursor.source_afters.len() == sources.len()
		&& cursor.sort == sort
		&& cursor.timeframe == timeframe
		&& cursor.query_fingerprint == query_fingerprint
}

async fn fetch_source_listing_page(source: &AggregateFeedSource, sort: &str, reddit_query: &str, after: &str, quarantine: bool) -> Result<ChunkListingPage, String> {
	let (posts, next_after) = match source {
		AggregateFeedSource::SubredditChunk { chunk } => Post::fetch(&build_listing_path(chunk, sort, reddit_query, after, ""), quarantine).await?,
		AggregateFeedSource::Flair { subreddit, flair } => {
			let (mut posts, next_after) = Post::fetch(&build_listing_path(subreddit, sort, reddit_query, after, ""), quarantine).await?;
			filter_posts_by_flair(&mut posts, flair);
			(posts, next_after)
		}
	};
	utils::store_post_flairs(&posts);
	Ok(ChunkListingPage { posts, after: next_after })
}

fn merge_chunk_posts(
	chunk_buffers: &mut [VecDeque<Post>],
	merged_posts: &mut Vec<Post>,
	filters: &std::collections::HashSet<String>,
	active_flair_filter: &str,
	recent_post_ids: &HashSet<String>,
	seen_post_ids: &mut HashSet<String>,
	any_post_after_filter: &mut bool,
	start_source_index: usize,
) -> usize {
	if chunk_buffers.is_empty() {
		return 0;
	}

	let mut next_source_index = start_source_index % chunk_buffers.len();
	let mut cursor = next_source_index;
	let mut empty_streak = 0usize;

	while empty_streak < chunk_buffers.len() {
		let Some(post) = chunk_buffers[cursor].pop_front() else {
			cursor = (cursor + 1) % chunk_buffers.len();
			empty_streak += 1;
			continue;
		};

		empty_streak = 0;
		next_source_index = (cursor + 1) % chunk_buffers.len();

		let is_filtered = crate::utils::post_matches_filter_policy(&post, filters);
		if !is_filtered {
			*any_post_after_filter = true;
			if crate::utils::is_deleted_reddit_placeholder_post(&post) {
				cursor = next_source_index;
				continue;
			}
			if (active_flair_filter.is_empty() || post.flair.text.trim() == active_flair_filter) && !recent_post_ids.contains(&post.id) && seen_post_ids.insert(post.id.clone()) {
				merged_posts.push(post);
				if merged_posts.len() >= TARGET_POST_COUNT {
					return next_source_index;
				}
			}
		}

		cursor = next_source_index;
	}

	next_source_index
}

async fn fetch_aggregated_subscription_feed(
	sources: &[AggregateFeedSource],
	cursor: Option<AggregateFrontCursor>,
	sort: &str,
	timeframe: &str,
	reddit_query: &str,
	query_fingerprint: &str,
	filters: &std::collections::HashSet<String>,
	active_flair_filter: &str,
	quarantine: bool,
) -> Result<AggregatedFeed, String> {
	let current_state = cursor
		.as_ref()
		.map(|value| value.source_afters.clone())
		.unwrap_or_else(|| vec![String::new(); sources.len()]);
	let current_source_index = cursor.as_ref().map(|value| value.next_source_index).unwrap_or(0);
	let mut next_state = current_state.clone();
	let mut prev_stack = cursor.as_ref().map(|value| value.prev_stack.clone()).unwrap_or_default();
	let page_has_cursor = cursor.is_some();
	let mut chunk_buffers: Vec<VecDeque<Post>> = (0..sources.len()).map(|_| VecDeque::new()).collect();
	let mut merged_posts = Vec::new();
	let recent_post_ids = cursor
		.as_ref()
		.map(|value| value.recent_post_ids.iter().cloned().collect::<HashSet<_>>())
		.unwrap_or_default();
	let mut seen_post_ids = HashSet::new();
	let mut fetched_raw_posts = false;
	let mut any_post_after_filter = false;
	let mut successful_chunks = 0usize;
	let mut chunk_errors = Vec::new();
	let mut next_source_index = current_source_index;

	for round in 0..MAX_AGGREGATE_FILL_ROUNDS {
		let mut fetched_any_chunk = false;

		let eligible = sources
			.iter()
			.enumerate()
			.filter_map(|(index, source)| {
				if round > 0 && next_state[index].is_empty() {
					return None;
				}
				if round == 0 && page_has_cursor && current_state[index].is_empty() {
					return None;
				}

				Some((index, source.clone(), next_state[index].clone()))
			})
			.collect::<Vec<_>>();

		for batch in eligible.chunks(AGGREGATE_FETCH_CONCURRENCY) {
			let mut join_set = JoinSet::new();

			for (index, source, request_after) in batch.iter().cloned() {
				let sort = sort.to_string();
				let reddit_query = reddit_query.to_string();
				join_set.spawn(async move {
					(
						index,
						source.clone(),
						fetch_source_listing_page(&source, &sort, &reddit_query, &request_after, quarantine).await,
					)
				});
			}

			while let Some(join_result) = join_set.join_next().await {
				let (index, source, result) = join_result.map_err(|error| error.to_string())?;
				match result {
					Ok(page) => {
						fetched_any_chunk = true;
						successful_chunks += 1;
						fetched_raw_posts |= !page.posts.is_empty();
						chunk_buffers[index].extend(page.posts);
						next_state[index] = page.after;
					}
					Err(err) => {
						chunk_errors.push(format!("source {source:?}: {err}"));
					}
				}
			}
		}

		next_source_index = merge_chunk_posts(
			&mut chunk_buffers,
			&mut merged_posts,
			filters,
			active_flair_filter,
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
			next_source_index,
		);

		if merged_posts.len() >= TARGET_POST_COUNT {
			break;
		}

		if !fetched_any_chunk || next_state.iter().all(String::is_empty) {
			break;
		}
	}

	if successful_chunks == 0 {
		let err = chunk_errors.into_iter().next().unwrap_or_else(|| "No aggregated chunk requests succeeded".to_string());
		return Err(err);
	}

	for err in &chunk_errors {
		warn!("Partial aggregate front page failure: {err}");
	}

	let page_post_ids = merged_posts.iter().map(|post| post.id.clone()).collect::<Vec<_>>();

	let next_cursor = if next_state.iter().any(|after| !after.is_empty()) {
		let mut next_prev_stack = prev_stack.clone();
		next_prev_stack.push(AggregateFrontPageState {
			source_afters: current_state.clone(),
			next_source_index: current_source_index,
		});
		encode_front_cursor(&AggregateFrontCursor {
			v: 1,
			sources: sources.to_vec(),
			source_afters: next_state,
			next_source_index,
			prev_stack: next_prev_stack,
			recent_post_ids: page_post_ids.clone(),
			sort: sort.to_string(),
			timeframe: timeframe.to_string(),
			query_fingerprint: query_fingerprint.to_string(),
		})
	} else {
		None
	};

	let prev_cursor = prev_stack.pop().and_then(|previous_state| {
		encode_front_cursor(&AggregateFrontCursor {
			v: 1,
			sources: sources.to_vec(),
			source_afters: previous_state.source_afters,
			next_source_index: previous_state.next_source_index,
			prev_stack,
			recent_post_ids: Vec::new(),
			sort: sort.to_string(),
			timeframe: timeframe.to_string(),
			query_fingerprint: query_fingerprint.to_string(),
		})
	});

	Ok(AggregatedFeed {
		posts: merged_posts,
		next_cursor,
		prev_cursor,
		all_posts_filtered: fetched_raw_posts && !any_post_after_filter,
		partial_failure: !chunk_errors.is_empty(),
	})
}

// SERVICES
pub async fn community(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let subscribed = prefs.subscriptions.join("+");
	let path = req.uri().path().to_string();
	let has_default_saved_feeds = !saved_feeds(&prefs).is_empty();
	let front_page = front_page_for_path(&path, &setting(&req, "front_page"));
	if path == "/" && front_page == "bookmarks" {
		return Ok(redirect("/bookmarks"));
	}
	let remove_default_feeds = setting(&req, "remove_default_feeds") == "on";
	let front_page_is_default = front_page == "default" || front_page.is_empty();
	let front_page_is_following = is_following_front_page(&front_page);
	let followed_users = followed_user_subscriptions(&prefs);
	let has_followed_users = !followed_users.is_empty();
	let post_sort = if front_page_is_following {
		"new".to_string()
	} else {
		default_post_sort_setting(&prefs)
	};
	let requested_sort = req.param("sort").unwrap_or_else(|| req.param("id").unwrap_or(post_sort));
	if req.param("sub").is_some() && is_random_sort(&requested_sort) {
		return Ok(redirect(&canonical_new_sort_url(&path, req.uri().query())));
	}
	let sort = requested_sort;
	let aggregate_front_page = req.param("sub").is_none() && ((front_page_is_default && has_default_saved_feeds) || (front_page_is_following && has_followed_users));
	let video_mode_front_page = req.param("sub").is_none() && is_video_length_sort(&sort);
	if aggregate_front_page {
		crate::post_cache::remember_frontpage_subreddits(&prefs, front_page_is_following);
		if let Some(profile_id) = prefs_store::request_session_id(&req) {
			crate::post_cache::schedule_sqlite_active_profile_sync(&prefs, &profile_id);
		}
	}

	let sub_name = req.param("sub").unwrap_or(if video_mode_front_page {
		String::new()
	} else if front_page_is_default {
		if has_default_saved_feeds {
			subscribed.clone()
		} else {
			"popular".to_string()
		}
	} else if front_page_is_following {
		followed_users.join("+")
	} else {
		front_page.clone()
	});

	if !video_mode_front_page && front_page_is_following && !has_followed_users && req.param("sub").is_none() {
		return info(req, "Follow some users to populate the followed users view.").await;
	}

	if !video_mode_front_page && (sub_name == "popular" || sub_name == "all") && remove_default_feeds {
		if !has_default_saved_feeds {
			return info(req, "Subscribe to some subreddits! (Default feeds disabled in settings)").await;
		} else {
			return info(
				req,
				"You have subscribed to some subreddits, but your front page is not set to default. Visit settings and change front page to default.",
			)
			.await;
		}
	}

	if let Ok(random) = catch_random(&sub_name, "").await {
		return Ok(random);
	}

	if req.param("sub").is_some() && sub_name.starts_with("u_") {
		return Ok(redirect(&["/user/", &sub_name[2..]].concat()));
	}

	let url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let active_flair_filter = param(&format!("?{}", req.uri().query().unwrap_or_default()), "flair").unwrap_or_default();
	let contextual_front_page = front_page_uses_contextual_session(req.param("sub").is_some(), &sort);
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26").replace('+', "%2B");
	let loading_message = if is_video_length_sort(&sort) {
		"Loading videos...".to_string()
	} else if contextual_front_page {
		// The contextual front page is rendered server-side below. Starting a shell-only
		// feed session here makes that renderer join an already-backgrounded session
		// and can paint an unstable partial page after a fresh deploy.
		if front_page_is_following {
			"Loading followed users in the background.".to_string()
		} else {
			"Loading followed feeds in the background.".to_string()
		}
	} else {
		"Loading posts...".to_string()
	};
	let placeholder_message = "Loading posts...".to_string();
	let cached_sub = if aggregate_front_page {
		Subreddit::default()
	} else if !sub_name.is_empty() && sub_name != "all" && sub_name != "popular" && !sub_name.contains('+') {
		cached_subreddit_metadata(&sub_name).unwrap_or_else(|| placeholder_subreddit(&sub_name))
	} else {
		Subreddit {
			name: sub_name.clone(),
			..Subreddit::default()
		}
	};
	let flair_filters = if !cached_sub.name.is_empty() && cached_sub.name != "all" && cached_sub.name != "popular" && !cached_sub.name.contains('+') {
		build_flair_filters(&url, &active_flair_filter, utils::cached_subreddit_flairs(&cached_sub.name))
	} else {
		Vec::new()
	};
	let top_users = if !cached_sub.name.is_empty() && cached_sub.name != "all" && cached_sub.name != "popular" && !cached_sub.name.contains('+') {
		utils::cached_subreddit_users(&cached_sub.name)
			.into_iter()
			.map(|(name, count)| SubredditUserEntry { name, count })
			.collect()
	} else {
		Vec::new()
	};
	let is_flair_subscribed = !active_flair_filter.is_empty()
		&& !cached_sub.name.is_empty()
		&& cached_sub.name != "all"
		&& cached_sub.name != "popular"
		&& !cached_sub.name.contains('+')
		&& prefs
			.flair_subscriptions
			.iter()
			.any(|subscription| subscription.subreddit == cached_sub.name && subscription.flair == active_flair_filter);
	let video_mode_sidebar = if is_video_length_sort(&sort) {
		let source_posts = if req.param("sub").is_some() {
			let mut cached_posts = crate::post_cache::servable_cached_source_posts(&FeedSource::subreddit(sub_name.clone(), FeedSort::New)).unwrap_or_default();
			filter_posts_by_flair(&mut cached_posts, &active_flair_filter);
			cached_posts
		} else {
			collect_front_page_video_candidates(
				&crate::post_cache::sqlite_active_profile_id(&prefs, &prefs_store::request_session_id(&req).unwrap_or_default()),
				if front_page_is_following { 2 } else { 1 },
				&build_video_mode_front_page_sources(&prefs, &front_page),
				&get_filters(&req),
			)
		};
		let parsed_vm = video_mode::parse_query_or_cookie(
			req.uri().query().unwrap_or_default(),
			&crate::utils::read_sharded_cookie(&req, video_mode::VIDEO_MODE_QUERY_COOKIE),
		);
		let listing = video_mode::build_listing(&source_posts, &get_filters(&req), &parsed_vm, now_unix());
		let normalized_vm = normalized_video_mode_query(&parsed_vm, &listing);
		let preview_posts = listing.posts.iter().take(crate::post_cache::POSTS_PER_PAGE).cloned().collect::<Vec<_>>();
		build_video_mode_sidebar(&url, &listing, &preview_posts, &normalized_vm)
	} else {
		empty_video_mode_sidebar()
	};

	let feed_seen_scope_shell = req.param("sub").is_none() && seen_posts::public_page_is_feed_seen_scope(&url);
	let feed_seen_shell = if feed_seen_scope_shell {
		let (session_id, needs_session_cookie) = seen_posts::ensure_rl_session_id(&req);
		let (hide_seen_on, hide_seen_cookie_override) = seen_posts::hide_seen_effective_for_request(&req, true);
		Some(seen_posts::FeedSeenCookieCtx {
			session_id,
			hide_seen_on,
			applies: true,
			needs_session_cookie,
			hide_seen_cookie_override,
		})
	} else {
		None
	};
	let user_agent = req.headers().get("user-agent").and_then(|value| value.to_str().ok()).unwrap_or_default();
	// Avoid post-load DOM fragment replacement for contextual front pages by rendering the
	// first visible listing server-side. The shell->fragment swap causes visible content
	// changes after first paint, and the feed is intentionally sealed before paint so later
	// background warmups do not reshuffle the displayed posts.
	if contextual_front_page {
		let query = req.uri().query().unwrap_or_default().to_string();
		let cache_page = param(&format!("?{}", query), "cache_page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
		let timeframe = param(&format!("?{}", query), "t").unwrap_or_default();
		let query_fingerprint = listing_query_fingerprint(&query);
		let front_page_session_sources = build_front_page_session_sources(&prefs, &front_page, front_page_is_default, aggregate_front_page, &sub_name, &sort);
		let requested_feed_session_id = param(&format!("?{}", query), FEED_SESSION_PARAM).unwrap_or_default();

		let feed_session_id = ensure_contextual_feed_session(
			requested_feed_session_id,
			FeedSessionRequest {
				sources: front_page_session_sources,
				filters: get_filters(&req),
				quarantine: true,
				query_fingerprint,
				profile_id: crate::post_cache::sqlite_active_profile_id(&prefs, &prefs_store::request_session_id(&req).unwrap_or_default()),
				// Keep plain homepage reloads on the same contextual session while
				// background source refreshes advance the global cache revision.
				cache_revision: 0,
				bundle_followed_user_comments: is_following_front_page(&front_page),
				daemon_discovery_order: is_daemon_discovery_sort(&sort),
				random_rotation_seed: random_rotation_seed_for_sort(&sort),
				merge_sort: contextual_feed_merge_sort(&front_page, &sort),
				..Default::default()
			},
			cache_page,
		)
		.await;

		crate::post_cache::wait_and_seal_contextual_feed_paint(&feed_session_id, cache_page, crate::post_cache::POSTS_PER_PAGE).await;

		let page = crate::post_cache::feed_page(&feed_session_id, cache_page, crate::post_cache::POSTS_PER_PAGE);
		if let Some(mut page) = page {
			let feed_status = crate::post_cache::feed_status(&feed_session_id, cache_page, crate::post_cache::POSTS_PER_PAGE);
			let feed_status_message = feed_status.as_ref().map(|status| status.status_message.clone()).unwrap_or_default();
			let feed_status_warming = feed_status.as_ref().map(|status| status.warming).unwrap_or(false);
			let hide_seen_on = feed_seen_shell.as_ref().is_some_and(|ctx| ctx.hide_seen_on);
			let merged_posts = if hide_seen_on { crate::post_cache::feed_merged_posts(&feed_session_id) } else { None };
			let (page_posts, has_next_unseen) = contextual_listing_posts_from_feed_page(feed_seen_shell.as_ref(), cache_page, &mut page, merged_posts.as_deref());
			let mut page_posts = page_posts;
			page.has_next = page.has_next || has_next_unseen;

			let no_posts = page.ready && page_posts.is_empty();
			let all_posts_hidden_nsfw = !no_posts && (page_posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
			if is_following_route(&url) {
				page_posts = crate::user::hydrate_following_submission_posts(page_posts).await;
			}
			utils::hydrate_post_author_links_from_cache(&mut page_posts);
			info!(
				"post_cache: community-full-page url={} cache_page={} session={} rendered_posts={} no_posts={} has_next={} ua={}",
				url,
				cache_page,
				feed_session_id,
				page_posts.len(),
				no_posts,
				page.has_next,
				user_agent
			);

			let prev_page_url = if cache_page > 0 {
				feed_page_url(&url, &feed_session_id, cache_page - 1)
			} else {
				String::new()
			};
			let next_page_url = if page.has_next {
				feed_page_url(&url, &feed_session_id, cache_page + 1)
			} else {
				String::new()
			};
			let contextual_pagination = if cache_page == 0 {
				Vec::new()
			} else {
				build_contextual_pagination(&url, &feed_session_id, cache_page, &sort)
			};
			let sort_base_path = if is_following_front_page(&front_page) {
				FOLLOWING_ROUTE.to_string()
			} else {
				String::new()
			};

			let related_subreddits = related_subreddit_sidebar(&cached_sub, &prefs, &get_filters(&req));
			let mut page_template = SubredditTemplate {
				sub: cached_sub,
				posts: page_posts,
				top_users,
				related_subreddits,
				sort: (sort.clone(), timeframe),
				sort_options: build_sort_options(&sort_base_path, &url, &sort, &active_flair_filter),
				flair_filters,
				active_flair_filter: active_flair_filter.clone(),
				active_flair_filter_encoded: encode_query_value(&active_flair_filter),
				clear_flair_filter_url: update_url_query(
					&url,
					&[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None), ("cache_page", None)],
				),
				prev_page_url,
				next_page_url,
				prefs,
				url: url.clone(),
				redirect_url,
				is_filtered: false,
				all_posts_filtered: false,
				all_posts_hidden_nsfw,
				no_posts,
				cache_notice: String::new(),
				peer_origin: String::new(),
				is_flair_subscribed,
				feed_session_id: feed_session_id.clone(),
				feed_status_url: feed_status_url(&url, &feed_session_id, cache_page),
				feed_fragment_url: feed_fragment_url(&url, &feed_session_id, cache_page),
				feed_status_message,
				feed_status_warming,
				show_contextual_pagination: !contextual_pagination.is_empty(),
				contextual_pagination,
				video_mode_enabled: false,
				video_mode_sidebar: empty_video_mode_sidebar(),
				show_hide_seen_toggle: false,
				hide_seen_on: false,
				hide_seen_toggle_url: String::new(),
			};

			if let Some(ref ctx) = feed_seen_shell {
				if ctx.applies {
					page_template.show_hide_seen_toggle = true;
					page_template.hide_seen_on = ctx.hide_seen_on;
					page_template.hide_seen_toggle_url = update_url_query(&page_template.url, &[("hide_seen", Some(if ctx.hide_seen_on { "off" } else { "on" }))]);
				}
			}

			crate::client::preload_post_images(&page_template.posts, &page_template.prefs);
			let mut response = template(&page_template);
			if let Some(ref ctx) = feed_seen_shell {
				if ctx.applies {
					seen_posts::attach_rl_feed_cookies(&mut response, &ctx.session_id, ctx.needs_session_cookie, ctx.hide_seen_cookie_override.as_deref());
				}
			}
			return Ok(response);
		}
	}

	let related_subreddits = related_subreddit_sidebar(&cached_sub, &prefs, &get_filters(&req));
	let mut response = template(&SubredditShellTemplate {
		sub: cached_sub,
		top_users,
		related_subreddits,
		flair_filters,
		active_flair_filter: active_flair_filter.clone(),
		active_flair_filter_encoded: encode_query_value(&active_flair_filter),
		clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)]),
		prefs,
		url,
		redirect_url,
		is_filtered: false,
		is_flair_subscribed,
		fragment_url: subreddit_fragment_path(&req),
		loading_message,
		placeholder_message,
		replace_selector: if is_video_length_sort(&sort) { "main".to_string() } else { "#column_one".to_string() },
		video_mode_enabled: is_video_length_sort(&sort),
		video_mode_sidebar,
	});
	if let Some(ref ctx) = feed_seen_shell {
		if ctx.applies {
			seen_posts::attach_rl_feed_cookies(&mut response, &ctx.session_id, ctx.needs_session_cookie, ctx.hide_seen_cookie_override.as_deref());
		}
	}
	Ok(response)
}

pub async fn community_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	// Build Reddit API path
	let query = req.uri().query().unwrap_or_default().to_string();
	let prefs = Preferences::new(&req);
	let subscribed = prefs.subscriptions.join("+");
	let request_path = req.uri().path().to_string();
	let has_default_saved_feeds = !saved_feeds(&prefs).is_empty();
	let front_page = front_page_for_path(&request_path, &setting(&req, "front_page"));
	let remove_default_feeds = setting(&req, "remove_default_feeds") == "on";
	let is_subreddit_route = req.uri().path().starts_with("/subreddit-fragment/r/");
	let front_page_is_default = front_page == "default" || front_page.is_empty();
	let front_page_is_following = is_following_front_page(&front_page);
	let followed_users = followed_user_subscriptions(&prefs);
	let has_followed_users = !followed_users.is_empty();
	let post_sort = if front_page_is_following {
		"new".to_string()
	} else {
		default_post_sort_setting(&prefs)
	};
	let requested_sort = req.param("sort").unwrap_or_else(|| req.param("id").unwrap_or(post_sort));
	if is_subreddit_route && is_random_sort(&requested_sort) {
		return Ok(redirect(&canonical_new_sort_url(&request_path, req.uri().query())));
	}
	let sort = requested_sort;
	let aggregate_front_page = !is_subreddit_route && ((front_page_is_default && has_default_saved_feeds) || (front_page_is_following && has_followed_users));
	let video_mode_front_page = !is_subreddit_route && is_video_length_sort(&sort);

	let sub_name = req.param("sub").unwrap_or(if video_mode_front_page {
		String::new()
	} else if front_page_is_default {
		if has_default_saved_feeds {
			subscribed.clone()
		} else {
			"popular".to_string()
		}
	} else if front_page_is_following {
		followed_users.join("+")
	} else {
		front_page.clone()
	});

	if !video_mode_front_page && front_page_is_following && !has_followed_users && !is_subreddit_route {
		return info(req, "Follow some users to populate the followed users view.").await;
	}

	if !video_mode_front_page && (sub_name == "popular" || sub_name == "all") && remove_default_feeds {
		if !has_default_saved_feeds {
			return info(req, "Subscribe to some subreddits! (Default feeds disabled in settings)").await;
		} else {
			// If there are subscribed subs, but we get here, then the problem is that front_page pref is set to something besides default.
			// Tell user to go to settings and change front page to default.
			return info(
				req,
				"You have subscribed to some subreddits, but your front page is not set to default. Visit settings and change front page to default.",
			)
			.await;
		}
	}

	let quarantined = can_access_quarantine(&req, &sub_name) || !is_subreddit_route;

	// Handle random subreddits
	if let Ok(random) = catch_random(&sub_name, "").await {
		return Ok(random);
	}

	if req.param("sub").is_some() && sub_name.starts_with("u_") {
		return Ok(redirect(&["/user/", &sub_name[2..]].concat()));
	}

	let reddit_query = subreddit_listing_query(&query, sub_name == "popular");
	let aggregate_sources = build_aggregate_sources(&prefs, &front_page, &sort, &reddit_query);
	let aggregate_saved_feeds = aggregate_front_page && !aggregate_sources.is_empty();
	let contextual_front_page = front_page_uses_contextual_session(is_subreddit_route, &sort);
	let front_page_session_sources = if contextual_front_page {
		build_front_page_session_sources(&prefs, &front_page, front_page_is_default, aggregate_front_page, &sub_name, &sort)
	} else {
		Vec::new()
	};

	let fragment_request_url = req.uri().to_string();
	let req_url = fragment_request_url.replacen("/subreddit-fragment", "", 1);

	let url = {
		let fragment_url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
		let original_url = fragment_url.replacen("/subreddit-fragment", "", 1);
		if original_url.is_empty() {
			"/".to_string()
		} else {
			original_url
		}
	};
	let feed_seen_scope = !is_subreddit_route && seen_posts::public_page_is_feed_seen_scope(&url);
	let feed_seen_ctx = if feed_seen_scope {
		let (session_id, needs_session_cookie) = seen_posts::ensure_rl_session_id(&req);
		let (hide_seen_on, hide_seen_cookie_override) = seen_posts::hide_seen_effective_for_request(&req, true);
		Some(seen_posts::FeedSeenCookieCtx {
			session_id,
			hide_seen_on,
			applies: true,
			needs_session_cookie,
			hide_seen_cookie_override,
		})
	} else {
		None
	};
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26").replace('+', "%2B");
	let active_flair_filter = param(&format!("?{}", req.uri().query().unwrap_or_default()), "flair").unwrap_or_default();
	let request_after = param(&format!("?{}", req.uri().query().unwrap_or_default()), "after").unwrap_or_default();
	let request_before = param(&format!("?{}", req.uri().query().unwrap_or_default()), "before").unwrap_or_default();
	let is_single_sub = !sub_name.is_empty() && sub_name != "all" && sub_name != "popular" && !sub_name.contains('+');
	let uses_remembered_subreddit_cache = is_single_sub && sort == "new";
	let cache_page = param(&format!("?{}", req.uri().query().unwrap_or_default()), "cache_page")
		.and_then(|p| p.parse::<usize>().ok())
		.unwrap_or(0);
	if uses_remembered_subreddit_cache {
		crate::post_cache::remember_subreddit_request(&sub_name);
		if request_after.is_empty() && request_before.is_empty() && cache_page == 0 {
			if !active_flair_filter.is_empty() {
				crate::post_cache::record_remembered_feed_visit_signal(&FeedSource::flair(sub_name.clone(), active_flair_filter.clone(), FeedSort::New));
			} else {
				crate::post_cache::record_remembered_feed_visit_signal(&FeedSource::subreddit(sub_name.clone(), FeedSort::New));
			}
		}
	}
	if aggregate_saved_feeds {
		crate::post_cache::remember_frontpage_subreddits(&prefs, front_page_is_following);
		if let Some(profile_id) = prefs_store::request_session_id(&req) {
			crate::post_cache::schedule_sqlite_active_profile_sync(&prefs, &profile_id);
		}
	}
	let cached_subreddit_page = if uses_remembered_subreddit_cache && request_after.is_empty() && request_before.is_empty() {
		crate::post_cache::get_cached_subreddit_page(&sub_name, cache_page, crate::post_cache::POSTS_PER_PAGE)
	} else {
		None
	};
	let prefers_cached_subreddit_page = cached_subreddit_page.is_some();
	if prefers_cached_subreddit_page && crate::utils::should_be_nsfw_gated(&req, &req_url) && cached_subreddit_nsfw_status(&sub_name).is_some_and(|is_nsfw| is_nsfw) {
		return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
	}
	let path = if aggregate_saved_feeds || sub_name.is_empty() {
		String::new()
	} else {
		build_listing_path(&sub_name, &sort, &reddit_query, &request_after, &request_before)
	};
	let force_peer_api_fallback = is_subreddit_route && is_single_sub && peer_api_fallback::request_should_force_peer_api_fallback(&req);
	if force_peer_api_fallback {
		info!("subreddit: forcing configured Redlib peer API fallback for {}", req_url);
		let sort_base_path = if is_following_route(&url) {
			FOLLOWING_ROUTE.to_string()
		} else if aggregate_saved_feeds {
			String::new()
		} else if sub_name.is_empty() {
			String::new()
		} else {
			format!("/r/{sub_name}")
		};
		let sort_options = build_sort_options(&sort_base_path, &url, &sort, &active_flair_filter);
		let filters = get_filters(&req);
		let timeframe = param(&path, "t").unwrap_or_default();
		let is_flair_subscribed = !active_flair_filter.is_empty()
			&& sub_name != "all"
			&& sub_name != "popular"
			&& prefs
				.flair_subscriptions
				.iter()
				.any(|subscription| subscription.subreddit == sub_name && subscription.flair == active_flair_filter);
		let scrape_cacheable_first_page = uses_remembered_subreddit_cache && request_after.is_empty() && request_before.is_empty();
		if let Some(response) = render_peer_api_subreddit_fragment(
			&req,
			&sub_name,
			&sort,
			sort_options,
			&active_flair_filter,
			&url,
			&redirect_url,
			&filters,
			&request_before,
			is_flair_subscribed,
			scrape_cacheable_first_page,
			req.uri().query(),
			&timeframe,
		)
		.await
		{
			return response;
		}
	}

	if is_subreddit_route
		&& is_single_sub
		&& (peer_api_fallback::peer_api_fallback_enabled() || instance_scrape::instance_scrape_enabled())
		&& instance_scrape::reddit_rate_limited_now()
	{
		info!("subreddit: using configured fallback for {} because Reddit is currently rate limited", req_url);
		let sort_base_path = if is_following_route(&url) {
			FOLLOWING_ROUTE.to_string()
		} else if aggregate_saved_feeds {
			String::new()
		} else if sub_name.is_empty() {
			String::new()
		} else {
			format!("/r/{sub_name}")
		};
		let sort_options = build_sort_options(&sort_base_path, &url, &sort, &active_flair_filter);
		let filters = get_filters(&req);
		let timeframe = param(&path, "t").unwrap_or_default();
		let is_flair_subscribed = !active_flair_filter.is_empty()
			&& sub_name != "all"
			&& sub_name != "popular"
			&& prefs
				.flair_subscriptions
				.iter()
				.any(|subscription| subscription.subreddit == sub_name && subscription.flair == active_flair_filter);
		let scrape_cacheable_first_page = uses_remembered_subreddit_cache && request_after.is_empty() && request_before.is_empty();
		if let Some(response) = render_peer_api_subreddit_fragment(
			&req,
			&sub_name,
			&sort,
			sort_options.clone(),
			&active_flair_filter,
			&url,
			&redirect_url,
			&filters,
			&request_before,
			is_flair_subscribed,
			scrape_cacheable_first_page,
			req.uri().query(),
			&timeframe,
		)
		.await
		{
			return response;
		}
		if instance_scrape::instance_scrape_enabled() {
			let refresh_token = scrape_cacheable_first_page.then(|| crate::post_cache::begin_source_refresh(&FeedSource::subreddit(sub_name.clone(), FeedSort::New)));
			if let Ok(scraped) = instance_scrape::scrape_subreddit_page(&req_url).await {
				return render_scraped_subreddit_fragment(
					&req,
					scraped,
					&sub_name,
					&sort,
					&timeframe,
					sort_options,
					&active_flair_filter,
					&url,
					&redirect_url,
					&filters,
					&request_before,
					is_flair_subscribed,
					scrape_cacheable_first_page,
					instance_scrape::scrape_notice(),
					"",
					refresh_token,
				);
			}
		}
		warn!("subreddit: preemptive scrape fallback failed for {}", req_url);
	}
	let mut metadata_error: Option<String> = None;
	let sub = if is_subreddit_route && is_single_sub {
		if prefers_cached_subreddit_page {
			if let Some(cached) = cached_subreddit_metadata(&sub_name) {
				cached
			} else if cached_subreddit_nsfw_status(&sub_name) == Some(false) {
				warm_subreddit_metadata(&sub_name, quarantined);
				placeholder_subreddit(&sub_name)
			} else if crate::utils::should_be_nsfw_gated(&req, &req_url) {
				match subreddit(&sub_name, quarantined).await {
					Ok(subreddit) => subreddit,
					Err(error) => {
						metadata_error = Some(error);
						placeholder_subreddit(&sub_name)
					}
				}
			} else {
				warm_subreddit_metadata(&sub_name, quarantined);
				placeholder_subreddit(&sub_name)
			}
		} else if uses_remembered_subreddit_cache && request_after.is_empty() && request_before.is_empty() {
			if let Some(cached) = cached_subreddit_metadata(&sub_name) {
				cached
			} else if cached_subreddit_nsfw_status(&sub_name) == Some(false) {
				warm_subreddit_metadata(&sub_name, quarantined);
				placeholder_subreddit(&sub_name)
			} else if crate::utils::should_be_nsfw_gated(&req, &req_url) {
				match subreddit(&sub_name, quarantined).await {
					Ok(subreddit) => subreddit,
					Err(error) => {
						metadata_error = Some(error);
						placeholder_subreddit(&sub_name)
					}
				}
			} else {
				warm_subreddit_metadata(&sub_name, quarantined);
				placeholder_subreddit(&sub_name)
			}
		} else {
			match subreddit(&sub_name, quarantined).await {
				Ok(subreddit) => subreddit,
				Err(error) => {
					metadata_error = Some(error);
					placeholder_subreddit(&sub_name)
				}
			}
		}
	} else if aggregate_saved_feeds && !is_subreddit_route {
		Subreddit::default()
	} else {
		placeholder_subreddit(&sub_name)
	};
	// Return landing page if this post if this is NSFW community but the user
	// has disabled the display of NSFW content or if the instance is SFW-only.
	if sub.nsfw && crate::utils::should_be_nsfw_gated(&req, &req_url) {
		return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
	}
	let flair_filters = if !sub.name.is_empty() && sub.name != "all" && sub.name != "popular" && !sub.name.contains('+') {
		build_flair_filters(&url, &active_flair_filter, utils::cached_subreddit_flairs(&sub.name))
	} else {
		Vec::new()
	};
	let top_users = if !sub.name.is_empty() && sub.name != "all" && sub.name != "popular" && !sub.name.contains('+') {
		utils::cached_subreddit_users(&sub.name)
			.into_iter()
			.map(|(name, count)| SubredditUserEntry { name, count })
			.collect()
	} else {
		Vec::new()
	};
	let sort_base_path = if is_following_route(&url) {
		FOLLOWING_ROUTE.to_string()
	} else if aggregate_saved_feeds {
		String::new()
	} else if sub_name.is_empty() {
		String::new()
	} else {
		format!("/r/{sub_name}")
	};
	let sort_options = build_sort_options(&sort_base_path, &url, &sort, &active_flair_filter);
	let filters = get_filters(&req);
	let timeframe = param(&path, "t").unwrap_or_default();
	let is_flair_subscribed = !active_flair_filter.is_empty()
		&& !sub.name.is_empty()
		&& sub.name != "all"
		&& sub.name != "popular"
		&& !sub.name.contains('+')
		&& prefs
			.flair_subscriptions
			.iter()
			.any(|subscription| subscription.subreddit == sub.name && subscription.flair == active_flair_filter);
	let scrape_cacheable_first_page = is_subreddit_route && is_single_sub && uses_remembered_subreddit_cache && request_after.is_empty() && request_before.is_empty();

	if let Some(reason) = (is_subreddit_route && is_single_sub)
		.then(|| metadata_error.as_deref().and_then(crate::client::source_unavailable_from_error))
		.flatten()
	{
		return render_subreddit_source_unavailable_fragment(
			&req,
			sub,
			&sub_name,
			reason,
			(sort.clone(), timeframe.clone()),
			sort_options,
			flair_filters,
			&active_flair_filter,
			url,
			redirect_url,
			is_flair_subscribed,
			&filters,
			top_users,
			feed_seen_ctx.clone(),
		);
	}

	if is_subreddit_route
		&& is_single_sub
		&& (peer_api_fallback::peer_api_fallback_enabled() || instance_scrape::instance_scrape_enabled())
		&& metadata_error.as_deref().is_some_and(instance_scrape::should_attempt_scrape_fallback)
	{
		info!(
			"subreddit: retrying {} via scrape fallback after metadata error: {}",
			req_url,
			metadata_error.as_deref().unwrap_or_default()
		);
		if let Some(response) = render_peer_api_subreddit_fragment(
			&req,
			&sub_name,
			&sort,
			sort_options.clone(),
			&active_flair_filter,
			&url,
			&redirect_url,
			&filters,
			&request_before,
			is_flair_subscribed,
			scrape_cacheable_first_page,
			req.uri().query(),
			&timeframe,
		)
		.await
		{
			return response;
		}
		if instance_scrape::instance_scrape_enabled() {
			let refresh_token = scrape_cacheable_first_page.then(|| crate::post_cache::begin_source_refresh(&FeedSource::subreddit(sub_name.clone(), FeedSort::New)));
			if let Ok(scraped) = instance_scrape::scrape_subreddit_page(&req_url).await {
				return render_scraped_subreddit_fragment(
					&req,
					scraped,
					&sub_name,
					&sort,
					&timeframe,
					sort_options,
					&active_flair_filter,
					&url,
					&redirect_url,
					&filters,
					&request_before,
					is_flair_subscribed,
					scrape_cacheable_first_page,
					instance_scrape::scrape_notice(),
					"",
					refresh_token,
				);
			}
		}
		warn!("subreddit: metadata scrape fallback failed for {}", req_url);
	}

	// If all requested subs are filtered, we don't need to fetch posts.
	if !aggregate_saved_feeds && !sub_name.is_empty() && sub_name.split('+').all(|s| filters.contains(s)) {
		let prefs = Preferences::new(&req);
		let related_subreddits = related_subreddit_sidebar(&sub, &prefs, &filters);
		render_subreddit_fragment(
			&req,
			SubredditFragmentTemplate {
				sub,
				posts: Vec::new(),
				top_users,
				related_subreddits,
				sort: (sort, param(&path, "t").unwrap_or_default()),
				sort_options,
				flair_filters,
				active_flair_filter: active_flair_filter.clone(),
				active_flair_filter_encoded: encode_query_value(&active_flair_filter),
				clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)]),
				prev_page_url: String::new(),
				next_page_url: String::new(),
				prefs,
				url,
				redirect_url,
				is_filtered: true,
				all_posts_filtered: false,
				all_posts_hidden_nsfw: false,
				no_posts: false,
				cache_notice: String::new(),
				peer_origin: String::new(),
				is_flair_subscribed,
				feed_session_id: String::new(),
				feed_status_url: String::new(),
				feed_fragment_url: String::new(),
				show_contextual_pagination: false,
				contextual_pagination: Vec::new(),
				video_mode_enabled: false,
				video_mode_sidebar: empty_video_mode_sidebar(),
				show_hide_seen_toggle: false,
				hide_seen_on: false,
				hide_seen_toggle_url: String::new(),
			},
			None,
		)
	} else if is_video_length_sort(&sort) {
		let source_posts = if is_subreddit_route && is_single_sub {
			let mut posts = match cached_or_live_new_subreddit_posts(&sub_name, quarantined).await {
				Ok(posts) => posts,
				Err(msg) => {
					if let Some(reason) = crate::client::source_unavailable_from_error(&msg) {
						return render_subreddit_source_unavailable_fragment(
							&req,
							sub,
							&sub_name,
							reason,
							(sort.clone(), timeframe.clone()),
							sort_options,
							flair_filters,
							&active_flair_filter,
							url,
							redirect_url,
							is_flair_subscribed,
							&filters,
							top_users,
							feed_seen_ctx.clone(),
						);
					}
					return Err(msg);
				}
			};
			filter_posts_by_flair(&mut posts, &active_flair_filter);
			posts
		} else if !is_subreddit_route {
			collect_front_page_video_candidates(
				&crate::post_cache::sqlite_active_profile_id(&prefs, &prefs_store::request_session_id(&req).unwrap_or_default()),
				if front_page_is_following { 2 } else { 1 },
				&build_video_mode_front_page_sources(&prefs, &front_page),
				&filters,
			)
		} else {
			Vec::new()
		};

		render_video_mode_subreddit_fragment(
			&req,
			sub,
			source_posts,
			top_users,
			sort,
			sort_options,
			flair_filters,
			active_flair_filter,
			url,
			redirect_url,
			is_flair_subscribed,
			&filters,
			cache_page,
			String::new(),
			feed_seen_ctx.clone(),
		)
	} else if contextual_front_page {
		let cache_page = param(&format!("?{}", req.uri().query().unwrap_or_default()), "cache_page")
			.and_then(|p| p.parse::<usize>().ok())
			.unwrap_or(0);
		let query_fingerprint = listing_query_fingerprint(&query);
		let requested_feed_session_id = param(&format!("?{}", req.uri().query().unwrap_or_default()), FEED_SESSION_PARAM).unwrap_or_default();
		let feed_session_id = ensure_contextual_feed_session(
			requested_feed_session_id,
			FeedSessionRequest {
				sources: front_page_session_sources,
				filters: filters.clone(),
				quarantine: quarantined,
				query_fingerprint,
				profile_id: crate::post_cache::sqlite_active_profile_id(&prefs, &prefs_store::request_session_id(&req).unwrap_or_default()),
				// Keep plain homepage reloads on the same contextual session while
				// background source refreshes advance the global cache revision.
				cache_revision: 0,
				bundle_followed_user_comments: is_following_front_page(&front_page),
				daemon_discovery_order: is_daemon_discovery_sort(&sort),
				random_rotation_seed: random_rotation_seed_for_sort(&sort),
				merge_sort: contextual_feed_merge_sort(&front_page, &sort),
				..Default::default()
			},
			cache_page,
		)
		.await;

		crate::post_cache::wait_and_seal_contextual_feed_paint(&feed_session_id, cache_page, crate::post_cache::POSTS_PER_PAGE).await;

		if let Some(mut page) = crate::post_cache::feed_page(&feed_session_id, cache_page, crate::post_cache::POSTS_PER_PAGE) {
			let feed_status = crate::post_cache::feed_status(&feed_session_id, cache_page, crate::post_cache::POSTS_PER_PAGE);
			let page_ready = feed_status.as_ref().map(|status| status.current_page_ready).unwrap_or(false);
			let ready_sources = feed_status.as_ref().map(|status| status.ready_sources).unwrap_or_default();
			let daemon_cached_sources = feed_status.as_ref().map(|status| status.daemon_cached_sources).unwrap_or_default();
			let merged_posts = feed_status.as_ref().map(|status| status.posts_merged).unwrap_or_default();
			info!(
				"post_cache: serving page {} from session {} ({} session sources ready, {} from daemon cache, {} total posts)",
				cache_page, feed_session_id, ready_sources, daemon_cached_sources, merged_posts
			);
			// Instead of serving a short page when many posts are seen, page over the full merged feed
			// and pick a full window of unseen posts.
			let hide_seen_on = feed_seen_ctx.as_ref().is_some_and(|ctx| ctx.hide_seen_on);
			let merged_posts = if hide_seen_on { crate::post_cache::feed_merged_posts(&feed_session_id) } else { None };
			let (page_posts, has_next_unseen) = contextual_listing_posts_from_feed_page(feed_seen_ctx.as_ref(), cache_page, &mut page, merged_posts.as_deref());
			let mut page_posts = page_posts;
			page.has_next = page.has_next || has_next_unseen;

			let no_posts = page_ready && page_posts.is_empty();
			let all_posts_hidden_nsfw = !no_posts && (page_posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
			if is_following_route(&url) {
				page_posts = crate::user::hydrate_following_submission_posts(page_posts).await;
			}
			utils::hydrate_post_author_links_from_cache(&mut page_posts);
			let user_agent = req.headers().get("user-agent").and_then(|value| value.to_str().ok()).unwrap_or_default();
			info!(
				"post_cache: subreddit-fragment url={} cache_page={} session={} rendered_posts={} page_ready={} no_posts={} has_next={} ua={}",
				url,
				cache_page,
				feed_session_id,
				page_posts.len(),
				page_ready,
				no_posts,
				page.has_next,
				user_agent
			);

			let prev_page_url = if cache_page > 0 {
				feed_page_url(&url, &feed_session_id, cache_page - 1)
			} else {
				String::new()
			};
			let next_page_url = if page.has_next {
				feed_page_url(&url, &feed_session_id, cache_page + 1)
			} else {
				String::new()
			};
			let contextual_pagination = if cache_page == 0 {
				Vec::new()
			} else {
				build_contextual_pagination(&url, &feed_session_id, cache_page, &sort)
			};

			let prefs = Preferences::new(&req);
			let related_subreddits = related_subreddit_sidebar(&sub, &prefs, &filters);
			return render_subreddit_fragment(
				&req,
				SubredditFragmentTemplate {
					sub,
					posts: page_posts,
					top_users,
					related_subreddits,
					sort: (sort.clone(), timeframe.clone()),
					sort_options,
					flair_filters,
					active_flair_filter: active_flair_filter.clone(),
					active_flair_filter_encoded: encode_query_value(&active_flair_filter),
					clear_flair_filter_url: update_url_query(
						&url,
						&[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None), ("cache_page", None)],
					),
					prev_page_url,
					next_page_url,
					prefs,
					url: url.clone(),
					redirect_url,
					is_filtered: false,
					all_posts_filtered: false,
					all_posts_hidden_nsfw,
					no_posts,
					cache_notice: String::new(),
					peer_origin: String::new(),
					is_flair_subscribed,
					feed_session_id: feed_session_id.clone(),
					feed_status_url: feed_status_url(&url, &feed_session_id, cache_page),
					feed_fragment_url: feed_fragment_url(&url, &feed_session_id, cache_page),
					show_contextual_pagination: !contextual_pagination.is_empty(),
					contextual_pagination,
					video_mode_enabled: false,
					video_mode_sidebar: empty_video_mode_sidebar(),
					show_hide_seen_toggle: false,
					hide_seen_on: false,
					hide_seen_toggle_url: String::new(),
				},
				feed_seen_ctx.clone(),
			);
		}
		return error(req, "Feed session not found or expired").await;
	} else if aggregate_saved_feeds {
		let query_fingerprint = listing_query_fingerprint(&query);
		let mut cursor = param(&format!("?{}", req.uri().query().unwrap_or_default()), FRONT_CURSOR_PARAM)
			.and_then(|encoded| decode_front_cursor(&encoded))
			.filter(|cursor| cursor_matches_request(cursor, &aggregate_sources, &sort, &timeframe, &query_fingerprint));

		// If hide-seen is on, keep fetching additional cursor pages until we have enough unseen posts
		// to display a full page (or we run out of cursor / hit a safety cap).
		let wants_unseen_fill = feed_seen_ctx.as_ref().is_some_and(|ctx| ctx.applies && ctx.hide_seen_on);
		let mut collected_posts: Vec<Post> = Vec::new();
		let mut next_cursor: Option<String> = None;
		let mut prev_cursor: Option<String> = None;
		let mut all_posts_filtered = false;
		let mut partial_failure = false;

		for _round in 0..MAX_AGGREGATE_FILL_ROUNDS {
			let feed = match fetch_aggregated_subscription_feed(
				&aggregate_sources,
				cursor.clone(),
				&sort,
				&timeframe,
				&reddit_query,
				&query_fingerprint,
				&filters,
				&active_flair_filter,
				quarantined,
			)
			.await
			{
				Ok(feed) => feed,
				Err(msg) => return error(req, &msg).await,
			};

			all_posts_filtered |= feed.all_posts_filtered;
			partial_failure |= feed.partial_failure;
			if prev_cursor.is_none() {
				prev_cursor = feed.prev_cursor.clone();
			}

			collected_posts.extend(feed.posts);
			next_cursor = feed.next_cursor.clone();

			if !wants_unseen_fill {
				prev_cursor = feed.prev_cursor;
				break;
			}

			let ctx = feed_seen_ctx.as_ref().expect("wants_unseen_fill implies ctx");
			let unseen = seen_posts::filter_unseen(&ctx.session_id, &collected_posts);
			if unseen.len() >= crate::post_cache::POSTS_PER_PAGE {
				prev_cursor = feed.prev_cursor;
				break;
			}

			// No more cursor: can't fill further.
			let Some(encoded_next) = next_cursor.as_deref() else {
				prev_cursor = feed.prev_cursor;
				break;
			};
			cursor = decode_front_cursor(encoded_next);
			if cursor.is_none() {
				prev_cursor = feed.prev_cursor;
				break;
			}
		}

		let mut posts = if wants_unseen_fill {
			let ctx = feed_seen_ctx.as_ref().expect("wants_unseen_fill implies ctx");
			let mut unseen = seen_posts::filter_unseen(&ctx.session_id, &collected_posts);
			// Avoid discarding unseen posts we've already fetched: show all unseen up to a safe cap.
			if unseen.len() > crate::post_cache::POSTS_PER_PAGE * 4 {
				unseen.truncate(crate::post_cache::POSTS_PER_PAGE * 4);
			}
			seen_posts::record_seen(&ctx.session_id, &unseen);
			unseen
		} else {
			collected_posts
		};

		if sort == "new" {
			posts.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
			posts.sort_by(|a, b| b.flags.stickied.cmp(&a.flags.stickied));
		} else if is_random_sort(&sort) {
			crate::post_cache::sort_posts_by_deterministic_random(&mut posts);
		}

		let no_posts = posts.is_empty();
		let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
		utils::hydrate_post_author_links_from_cache(&mut posts);
		if partial_failure && posts.is_empty() {
			warn!("Aggregate front page rendered without posts after partial failures");
		}

		let next_page_url = next_cursor.as_deref().map(|cursor| build_front_cursor_url(&url, cursor)).unwrap_or_default();
		let prev_page_url = prev_cursor
			.as_deref()
			.filter(|c| !c.is_empty())
			.map(|cursor| build_front_cursor_url(&url, cursor))
			.unwrap_or_default();

		let prefs = Preferences::new(&req);
		let related_subreddits = related_subreddit_sidebar(&sub, &prefs, &filters);
		render_subreddit_fragment(
			&req,
			SubredditFragmentTemplate {
				sub,
				posts,
				top_users,
				related_subreddits,
				sort: (sort.clone(), timeframe.clone()),
				sort_options,
				flair_filters,
				active_flair_filter: active_flair_filter.clone(),
				active_flair_filter_encoded: encode_query_value(&active_flair_filter),
				clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)]),
				prev_page_url,
				next_page_url,
				prefs,
				url,
				redirect_url,
				is_filtered: false,
				all_posts_filtered,
				all_posts_hidden_nsfw,
				no_posts,
				cache_notice: String::new(),
				peer_origin: String::new(),
				is_flair_subscribed,
				feed_session_id: String::new(),
				feed_status_url: String::new(),
				feed_fragment_url: String::new(),
				show_contextual_pagination: false,
				contextual_pagination: Vec::new(),
				video_mode_enabled: false,
				video_mode_sidebar: empty_video_mode_sidebar(),
				show_hide_seen_toggle: false,
				hide_seen_on: false,
				hide_seen_toggle_url: String::new(),
			},
			feed_seen_ctx.clone(),
		)
	} else {
		// For single subreddits with sort=new, serve from the per-sub cache before any live Reddit listing fetch.
		if let Some((mut posts, mut after, mut has_prev, mut has_next)) = cached_subreddit_page {
			info!("post_cache: serving r/{} page {} from cache", sub_name, cache_page);
			let mut rendered_cache_page = cache_page;
			let mut all_posts_filtered;
			let mut filtered_page_advances = 0;
			// Mirror live cursor skipping for remembered cache pages.
			loop {
				let (_, page_all_posts_filtered) = filter_posts(&mut posts, &filters);
				all_posts_filtered = page_all_posts_filtered;
				if !page_all_posts_filtered || !has_next || !after.is_empty() || filtered_page_advances >= MAX_FILTERED_PAGE_ADVANCE {
					break;
				}

				let next_cache_page = rendered_cache_page + 1;
				let Some((next_posts, next_after, next_has_prev, next_has_next)) =
					crate::post_cache::get_cached_subreddit_page(&sub_name, next_cache_page, crate::post_cache::POSTS_PER_PAGE)
				else {
					break;
				};
				posts = next_posts;
				after = next_after;
				has_prev = next_has_prev;
				has_next = next_has_next;
				rendered_cache_page = next_cache_page;
				filtered_page_advances += 1;
			}
			filter_posts_by_flair(&mut posts, &active_flair_filter);
			let no_posts = posts.is_empty();
			let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
			utils::hydrate_post_author_links_from_cache(&mut posts);

			let prev_page_url = if has_prev {
				update_url_query(
					&url,
					&[("cache_page", Some(&rendered_cache_page.saturating_sub(1).to_string())), ("before", None), ("after", None)],
				)
			} else {
				String::new()
			};
			let next_page_url = if has_next && !after.is_empty() {
				// Last cached page: link to Reddit's after-based pagination.
				update_url_query(&url, &[("sort", Some(sort.as_str())), ("after", Some(&after)), ("cache_page", None)])
			} else if has_next {
				update_url_query(&url, &[("cache_page", Some(&(rendered_cache_page + 1).to_string())), ("before", None), ("after", None)])
			} else {
				String::new()
			};

			let prefs = Preferences::new(&req);
			let related_subreddits = related_subreddit_sidebar(&sub, &prefs, &filters);
			return render_subreddit_fragment(
				&req,
				SubredditFragmentTemplate {
					sub,
					posts,
					top_users,
					related_subreddits,
					sort: (sort.clone(), timeframe.clone()),
					sort_options,
					flair_filters,
					active_flair_filter: active_flair_filter.clone(),
					active_flair_filter_encoded: encode_query_value(&active_flair_filter),
					clear_flair_filter_url: update_url_query(
						&url,
						&[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None), ("cache_page", None)],
					),
					prev_page_url,
					next_page_url,
					prefs,
					url,
					redirect_url,
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
					cache_notice: String::new(),
					peer_origin: String::new(),
					is_flair_subscribed,
					feed_session_id: String::new(),
					feed_status_url: String::new(),
					feed_fragment_url: String::new(),
					show_contextual_pagination: false,
					contextual_pagination: Vec::new(),
					video_mode_enabled: false,
					video_mode_sidebar: empty_video_mode_sidebar(),
					show_hide_seen_toggle: false,
					hide_seen_on: false,
					hide_seen_toggle_url: String::new(),
				},
				None,
			);

			// Fall through to normal fetch if not cached.
		}

		let source_refresh_token = if uses_remembered_subreddit_cache && request_after.is_empty() && request_before.is_empty() {
			crate::post_cache::mark_plain_subreddit_refresh_started(&sub_name)
		} else {
			None
		};

		match fetch_listing_skipping_filtered_pages(&path, quarantined, &filters).await {
			Ok(FilteredListingFetch {
				mut posts,
				raw_posts,
				after,
				all_posts_filtered,
			}) => {
				if let Some(token) = source_refresh_token.clone() {
					crate::post_cache::publish_plain_subreddit_refresh_with_token(&sub_name, raw_posts.clone(), after.clone(), crate::post_cache::RefreshOrigin::DirectLiveFetch, token);
				} else {
					utils::store_post_flairs(&raw_posts);
					if is_single_sub {
						enqueue_direct_subreddit_embedding_activity(&sub_name, &raw_posts);
						utils::store_subreddit_users(&sub_name, &raw_posts);
					}
				}
				filter_posts_by_flair(&mut posts, &active_flair_filter);
				let no_posts = posts.is_empty();
				let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
				if sort == "new" {
					posts.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
					posts.sort_by(|a, b| b.flags.stickied.cmp(&a.flags.stickied));
				}
				utils::hydrate_post_author_links_from_cache(&mut posts);
				let timeframe = param(&path, "t").unwrap_or_default();
				let before = param(&path, "before").unwrap_or_default();
				let prefs = Preferences::new(&req);
				let related_subreddits = related_subreddit_sidebar(&sub, &prefs, &filters);
				render_subreddit_fragment(
					&req,
					SubredditFragmentTemplate {
						sub,
						posts,
						top_users: if is_single_sub {
							utils::cached_subreddit_users(&sub_name)
								.into_iter()
								.map(|(name, count)| SubredditUserEntry { name, count })
								.collect()
						} else {
							Vec::new()
						},
						related_subreddits,
						sort: (sort.clone(), timeframe.clone()),
						sort_options,
						flair_filters: if is_single_sub {
							build_flair_filters(&url, &active_flair_filter, utils::cached_subreddit_flairs(&sub_name))
						} else {
							Vec::new()
						},
						active_flair_filter: active_flair_filter.clone(),
						active_flair_filter_encoded: encode_query_value(&active_flair_filter),
						clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)]),
						prev_page_url: if !before.is_empty() {
							update_url_query(
								&url,
								&[
									("sort", Some(sort.as_str())),
									("t", Some(timeframe.as_str())),
									("before", Some(before.as_str())),
									("after", None),
								],
							)
						} else {
							String::new()
						},
						next_page_url: if !after.is_empty() {
							update_url_query(
								&url,
								&[
									("sort", Some(sort.as_str())),
									("t", Some(timeframe.as_str())),
									("before", None),
									("after", Some(after.as_str())),
								],
							)
						} else {
							String::new()
						},
						prefs,
						url,
						redirect_url,
						is_filtered: false,
						all_posts_filtered,
						all_posts_hidden_nsfw,
						no_posts,
						cache_notice: String::new(),
						peer_origin: String::new(),
						is_flair_subscribed,
						feed_session_id: String::new(),
						feed_status_url: String::new(),
						feed_fragment_url: String::new(),
						show_contextual_pagination: false,
						contextual_pagination: Vec::new(),
						video_mode_enabled: false,
						video_mode_sidebar: empty_video_mode_sidebar(),
						show_hide_seen_toggle: false,
						hide_seen_on: false,
						hide_seen_toggle_url: String::new(),
					},
					None,
				)
			}
			Err(msg) => {
				if let Some(reason) = (is_subreddit_route && is_single_sub).then(|| crate::client::source_unavailable_from_error(&msg)).flatten() {
					if let Some(token) = source_refresh_token.clone() {
						crate::post_cache::record_plain_subreddit_refresh_failure_with_token(&sub_name, &msg, crate::post_cache::RefreshOrigin::DirectLiveFetch, token);
					}
					return render_subreddit_source_unavailable_fragment(
						&req,
						sub,
						&sub_name,
						reason,
						(sort.clone(), timeframe.clone()),
						sort_options,
						flair_filters,
						&active_flair_filter,
						url,
						redirect_url,
						is_flair_subscribed,
						&filters,
						top_users,
						None,
					);
				}
				let can_serve_stale_single_subreddit_cache = is_single_sub && request_after.is_empty() && request_before.is_empty();
				if can_serve_stale_single_subreddit_cache {
					if let Some(token) = source_refresh_token {
						crate::post_cache::record_plain_subreddit_refresh_failure_with_token(&sub_name, &msg, crate::post_cache::RefreshOrigin::DirectLiveFetch, token);
					}
					if msg.contains("oauth_unavailable") || instance_scrape::should_attempt_scrape_fallback(&msg) {
						if let Some((mut posts, mut after, mut has_prev, mut has_next)) = crate::post_cache::get_cached_subreddit_page_with_mode(
							&sub_name,
							cache_page,
							crate::post_cache::POSTS_PER_PAGE,
							crate::post_cache::CacheServeMode::AllowStaleOnOauthOutage,
						) {
							let mut rendered_cache_page = cache_page;
							let mut all_posts_filtered;
							let mut filtered_page_advances = 0;
							// Mirror live cursor skipping when serving stale cache during OAuth outages.
							loop {
								let (_, page_all_posts_filtered) = filter_posts(&mut posts, &filters);
								all_posts_filtered = page_all_posts_filtered;
								if !page_all_posts_filtered || !has_next || !after.is_empty() || filtered_page_advances >= MAX_FILTERED_PAGE_ADVANCE {
									break;
								}

								let next_cache_page = rendered_cache_page + 1;
								let Some((next_posts, next_after, next_has_prev, next_has_next)) = crate::post_cache::get_cached_subreddit_page_with_mode(
									&sub_name,
									next_cache_page,
									crate::post_cache::POSTS_PER_PAGE,
									crate::post_cache::CacheServeMode::AllowStaleOnOauthOutage,
								) else {
									break;
								};
								posts = next_posts;
								after = next_after;
								has_prev = next_has_prev;
								has_next = next_has_next;
								rendered_cache_page = next_cache_page;
								filtered_page_advances += 1;
							}
							filter_posts_by_flair(&mut posts, &active_flair_filter);
							let no_posts = posts.is_empty();
							let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
							utils::hydrate_post_author_links_from_cache(&mut posts);

							let prev_page_url = if has_prev {
								update_url_query(
									&url,
									&[("cache_page", Some(&rendered_cache_page.saturating_sub(1).to_string())), ("before", None), ("after", None)],
								)
							} else {
								String::new()
							};
							let next_page_url = if has_next && !after.is_empty() {
								update_url_query(&url, &[("sort", Some(sort.as_str())), ("after", Some(&after)), ("cache_page", None)])
							} else if has_next {
								update_url_query(&url, &[("cache_page", Some(&(rendered_cache_page + 1).to_string())), ("before", None), ("after", None)])
							} else {
								String::new()
							};

							let prefs = Preferences::new(&req);
							let related_subreddits = related_subreddit_sidebar(&sub, &prefs, &filters);
							return render_subreddit_fragment(
								&req,
								SubredditFragmentTemplate {
									sub,
									posts,
									top_users,
									related_subreddits,
									sort: (sort.clone(), timeframe.clone()),
									sort_options,
									flair_filters,
									active_flair_filter: active_flair_filter.clone(),
									active_flair_filter_encoded: encode_query_value(&active_flair_filter),
									clear_flair_filter_url: update_url_query(
										&url,
										&[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None), ("cache_page", None)],
									),
									prev_page_url,
									next_page_url,
									prefs,
									url,
									redirect_url,
									is_filtered: false,
									all_posts_filtered,
									all_posts_hidden_nsfw,
									no_posts,
									cache_notice: OAUTH_CACHE_NOTICE.to_string(),
									peer_origin: String::new(),
									is_flair_subscribed,
									feed_session_id: String::new(),
									feed_status_url: String::new(),
									feed_fragment_url: String::new(),
									show_contextual_pagination: false,
									contextual_pagination: Vec::new(),
									video_mode_enabled: false,
									video_mode_sidebar: empty_video_mode_sidebar(),
									show_hide_seen_toggle: false,
									hide_seen_on: false,
									hide_seen_toggle_url: String::new(),
								},
								None,
							);
						}
					}
				}
				if is_subreddit_route
					&& is_single_sub
					&& (peer_api_fallback::peer_api_fallback_enabled() || instance_scrape::instance_scrape_enabled())
					&& instance_scrape::should_attempt_scrape_fallback(&msg)
				{
					info!("subreddit: retrying {} via configured fallback after listing error: {}", req_url, msg);
					if let Some(response) = render_peer_api_subreddit_fragment(
						&req,
						&sub_name,
						&sort,
						sort_options.clone(),
						&active_flair_filter,
						&url,
						&redirect_url,
						&filters,
						&request_before,
						is_flair_subscribed,
						scrape_cacheable_first_page,
						req.uri().query(),
						&timeframe,
					)
					.await
					{
						return response;
					}
					if instance_scrape::instance_scrape_enabled() {
						let refresh_token = scrape_cacheable_first_page.then(|| crate::post_cache::begin_source_refresh(&FeedSource::subreddit(sub_name.clone(), FeedSort::New)));
						if let Ok(scraped) = instance_scrape::scrape_subreddit_page(&req_url).await {
							return render_scraped_subreddit_fragment(
								&req,
								scraped,
								&sub_name,
								&sort,
								&timeframe,
								sort_options,
								&active_flair_filter,
								&url,
								&redirect_url,
								&filters,
								&request_before,
								is_flair_subscribed,
								scrape_cacheable_first_page,
								instance_scrape::scrape_notice(),
								"",
								refresh_token,
							);
						}
					}
					warn!("subreddit: listing scrape fallback failed for {}", req_url);
				}
				match msg.as_str() {
					"quarantined" | "gated" => Ok(quarantine(&req, sub_name, &msg)),
					"private" => error(req, &format!("r/{sub_name} is a private community")).await,
					"banned" => error(req, &format!("r/{sub_name} has been banned from Reddit")).await,
					_ => error(req, &msg).await,
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{
		canonical_new_sort_url, decode_front_cursor, encode_front_cursor, filter_posts_by_flair, listing_query_fingerprint, merge_chunk_posts, split_subscriptions_for_listing,
		update_url_query, AggregateFeedSource, AggregateFrontCursor, AggregateFrontPageState, FeedFragmentTemplate, SubredditTemplate, FRONT_CURSOR_PARAM,
	};
	use askama::Template;
	use hyper::{body, Body, Request};
	use std::collections::{HashSet, VecDeque};
	use std::time::{Duration, Instant};

	use crate::post_cache::{self, FeedSessionRequest, FeedSort, FeedSource, RefreshOrigin};
	use crate::utils::{Author, Awards, Flags, Flair, FollowingComment, Media, ParentComment, Post, Preferences, Subreddit};

	fn empty_flair() -> Flair {
		Flair {
			flair_parts: Vec::new(),
			text: String::new(),
			background_color: String::new(),
			foreground_color: String::new(),
		}
	}

	fn test_prefs() -> Preferences {
		Preferences {
			layout: "card".to_string(),
			show_nsfw: "on".to_string(),
			use_hls: "off".to_string(),
			..Default::default()
		}
	}

	fn test_post() -> Post {
		Post {
			id: "demo".to_string(),
			kind: "t3".to_string(),
			title: "Primary post title".to_string(),
			community: "demo_sub".to_string(),
			body: String::new(),
			author: Author {
				name: "demo_user".to_string(),
				flair: empty_flair(),
				distinguished: String::new(),
				external_links: Vec::new(),
				avg_video_length: None,
			},
			permalink: "/r/demo_sub/comments/demo/".to_string(),
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
			post_type: "image".to_string(),
			flair: empty_flair(),
			flags: Flags::new(false, false, false),
			thumbnail: Media::empty(),
			media: Media {
				url: "https://i.redd.it/example.jpg".to_string(),
				..Media::empty()
			},
			domain: "i.redd.it".to_string(),
			rel_time: "1h ago".to_string(),
			created: "2026-03-19 00:00:00 UTC".to_string(),
			created_ts: 1,
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
	fn feed_fragment_marks_followed_authors_case_insensitively() {
		let mut prefs = test_prefs();
		prefs.subscriptions = vec!["u_Demo_User".to_string()];
		prefs.subscriptions_lc = prefs.subscriptions.iter().map(|value| value.to_ascii_lowercase()).collect();

		let html = FeedFragmentTemplate {
			posts: vec![test_post()],
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs,
			url: "/".to_string(),
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
		}
		.render()
		.unwrap();

		assert!(html.contains("class=\"followed_author_marker\">followed</span>"));
	}

	#[test]
	fn listing_warms_one_device_neutral_card_identity_before_hybrid_rows() {
		let posts = (1..=7)
			.map(|index| {
				let mut post = test_post();
				post.id = format!("warm{index}");
				post.permalink = format!("/r/demo_sub/comments/warm{index}/");
				post
			})
			.collect();
		let html = FeedFragmentTemplate {
			posts,
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/r/demo_sub".to_string(),
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
		}
		.render()
		.unwrap();

		assert_eq!(html.matches("data-post-component-warm-url=").count(), 6);
		assert_eq!(html.matches("data-post-component-url=").count(), 1);
		assert_eq!(html.matches("data-post-live-media-url=").count(), 1);
		assert!(html.contains("data-post-component-warm-url=\"/post-content/warm1/"));
		assert!(html.contains("/card.bin\""));
		assert!(html.contains("data-post-component-url=\"/post-content/warm7/"));
		assert!(html.contains("data-post-live-media-url=\"/post-live/warm7/"));
		assert_eq!(html.matches("data-post-component-loading").count(), 1);
		assert!(html.contains("Loading post content…"));
		assert!(!html.contains("post-content-mobile"));
		assert!(!html.contains("post-content-desktop"));
	}

	#[test]
	fn deferred_feed_rows_mark_followed_authors() {
		let posts = (1..=7)
			.map(|index| {
				let mut post = test_post();
				post.id = format!("followed{index}");
				post.permalink = format!("/r/demo_sub/comments/followed{index}/");
				post
			})
			.collect();
		let mut prefs = test_prefs();
		prefs.subscriptions = vec!["u_Demo_User".to_string()];
		prefs.subscriptions_lc = prefs.subscriptions.iter().map(|value| value.to_ascii_lowercase()).collect();

		let html = FeedFragmentTemplate {
			posts,
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs,
			url: "/".to_string(),
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
		}
		.render()
		.unwrap();

		assert_eq!(html.matches("class=\"followed_author_marker\">followed</span>").count(), 7);
		assert!(html.contains("data-post-component-id=\"followed7\""));
		assert!(html.contains("data-post-component-url=\"/post-content/followed7/"));
		assert_eq!(html.matches("data-auto-expand-preview=\"true\"").count(), 1);
	}

	#[test]
	fn deferred_unfollowed_rows_do_not_request_preview_expansion() {
		let posts = (1..=7)
			.map(|index| {
				let mut post = test_post();
				post.id = format!("unfollowed{index}");
				post.permalink = format!("/r/demo_sub/comments/unfollowed{index}/");
				post
			})
			.collect();

		let html = FeedFragmentTemplate {
			posts,
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/".to_string(),
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
		}
		.render()
		.unwrap();

		assert!(html.contains("data-post-component-id=\"unfollowed7\""));
		assert!(!html.contains("data-auto-expand-preview=\"true\""));
	}

	fn metric_post(id: &str, community: &str, created_ts: u64, score: i64, stickied: bool) -> Post {
		let mut post = test_post();
		post.id = id.to_string();
		post.title = format!("Post {id}");
		post.community = community.to_string();
		post.created_ts = created_ts;
		post.score = crate::utils::format_num(score);
		post.flags = Flags::new(false, false, stickied);
		post.media.url = format!("https://i.redd.it/{id}.jpg");
		post.permalink = format!("/r/{community}/comments/{id}/");
		post
	}

	fn test_following_comment() -> FollowingComment {
		FollowingComment {
			id: "comment_demo".to_string(),
			author: "followed_user".to_string(),
			body: "Followed user comment".to_string(),
			permalink: "/r/demo_sub/comments/demo/comment_demo/".to_string(),
			created_ts: 1,
			rel_time: "30m ago".to_string(),
			community: "demo_sub".to_string(),
			score: ("1".to_string(), "1".to_string()),
			parent_id: String::new(),
			parent_kind: "t3".to_string(),
			parent_comment: ParentComment::default(),
			link_title: "Primary post title".to_string(),
			link_permalink: "/r/demo_sub/comments/demo/".to_string(),
			link_author: "demo_user".to_string(),
			link_created_ts: 1,
			thread_comments: ("0".to_string(), "0".to_string()),
			thread_nsfw: false,
		}
	}

	async fn seed_contextual_session(source: FeedSource, posts: Vec<Post>, fingerprint: &str) -> String {
		post_cache::store_source_posts_for_cache(&source, posts, String::new(), Some(RefreshOrigin::FrontPageSession));
		post_cache::ensure_feed_session(
			FeedSessionRequest {
				sources: vec![source],
				filters: HashSet::new(),
				quarantine: false,
				query_fingerprint: fingerprint.to_string(),
				..Default::default()
			},
			0,
			post_cache::POSTS_PER_PAGE,
		)
		.await
	}

	#[test]
	fn aggregate_feed_pages_do_not_render_client_duplicate_script_by_default() {
		let posts = (0..7)
			.map(|index| {
				let mut post = test_post();
				post.id = format!("demo{index}");
				post.permalink = format!("/r/demo_sub/comments/demo{index}/");
				post
			})
			.collect();
		let html = SubredditTemplate {
			sub: Subreddit {
				name: "rust+programming".to_string(),
				..Default::default()
			},
			posts,
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			sort: ("hot".to_string(), String::new()),
			sort_options: Vec::new(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: "/r/rust+programming".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/r/rust+programming".to_string(),
			redirect_url: "r/rust%2Bprogramming".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			cache_notice: String::new(),
			peer_origin: String::new(),
			is_flair_subscribed: false,
			feed_session_id: String::new(),
			feed_status_url: String::new(),
			feed_fragment_url: String::new(),
			feed_status_message: String::new(),
			feed_status_warming: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		}
		.render()
		.unwrap();

		assert!(!html.contains("submitted_duplicates.js"));
		assert_eq!(html.matches("data-post-component-url=").count(), 1);
		assert!(html.contains("data-post-component-id=\"demo6\""));
	}

	#[test]
	fn does_not_render_duplicate_script_for_single_subreddit_pages() {
		let html = SubredditTemplate {
			sub: Subreddit {
				name: "rust".to_string(),
				has_sidebar: true,
				..Default::default()
			},
			posts: vec![test_post()],
			top_users: vec![super::SubredditUserEntry {
				name: "demo_user".to_string(),
				count: 3,
			}],
			related_subreddits: crate::materialized_catalog::RelatedSubredditReport {
				semantic_embedding_count: 42,
				vibe_embedding_count: 17,
				followed_subreddits: vec![crate::materialized_catalog::RelatedSubredditEntry {
					name: "programming".to_string(),
					score_percent: 84,
					cached_profile_count: 2,
					cached_post_count: 8,
				}],
				cached_subreddits: vec![crate::materialized_catalog::RelatedSubredditEntry {
					name: "rustlang".to_string(),
					score_percent: 91,
					cached_profile_count: 4,
					cached_post_count: 12,
				}],
				vibe_cached_subreddits: vec![crate::materialized_catalog::RelatedSubredditEntry {
					name: "systems".to_string(),
					score_percent: 87,
					cached_profile_count: 3,
					cached_post_count: 10,
				}],
				followed_profiles: vec![crate::materialized_catalog::RelatedProfileEntry {
					name: "followed_dev".to_string(),
					score_percent: 79,
					cached_posts: 20,
					current_subreddit_posts: 3,
					signal_subreddits: vec!["rust".to_string()],
				}],
				cached_profiles: vec![crate::materialized_catalog::RelatedProfileEntry {
					name: "new_dev".to_string(),
					score_percent: 83,
					cached_posts: 18,
					current_subreddit_posts: 2,
					signal_subreddits: vec!["rustlang".to_string()],
				}],
				..Default::default()
			},
			sort: ("hot".to_string(), String::new()),
			sort_options: Vec::new(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: "/r/rust".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/r/rust".to_string(),
			redirect_url: "r/rust".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			cache_notice: String::new(),
			peer_origin: String::new(),
			is_flair_subscribed: false,
			feed_session_id: String::new(),
			feed_status_url: String::new(),
			feed_fragment_url: String::new(),
			feed_status_message: String::new(),
			feed_status_warming: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		}
		.render()
		.unwrap();

		assert!(!html.contains("submitted_duplicates.js"));
		let users_index = html.find("id=\"sub_top_users\"").expect("sidebar should keep user stats");
		let related_index = html.find("id=\"sub_related\"").expect("sidebar should render related recommendations");
		assert!(users_index < related_index, "user stats should remain above related recommendations");
		assert!(html.contains("/user/demo_user\">u/demo_user</a>"));
		assert!(html.contains(">3</span>"));
		assert!(html.contains("Related communities"));
		assert!(html.contains("Related people"));
		assert_eq!(html.matches(">Discover</p>").count(), 2);
		assert!(html.contains("id=\"sub_related_topic\" checked"));
		assert!(html.contains("id=\"sub_related_vibe\""));
		assert!(html.contains("/r/rustlang"));
		assert!(html.contains("91% topic match"));
		assert!(html.contains("/r/systems"));
		assert!(html.contains("87% vibe match"));
		assert!(html.contains("/user/new_dev"));
		assert!(html.contains("83% match"));
		assert_eq!(html.matches("<summary>Already following</summary>").count(), 2);
		assert!(html.contains("/r/programming"));
		assert!(html.contains("/user/followed_dev"));
		assert!(!html.contains("description vectors"));
		assert!(!html.contains("embedded posts"));
		assert!(!html.contains("Known by description"));
		assert!(!html.contains("Profiles from cache"));
	}

	#[test]
	fn renders_shell_mode_subreddit_placeholder_with_fragment_url() {
		let html = super::SubredditShellTemplate {
			sub: Subreddit {
				name: "rust".to_string(),
				..Default::default()
			},
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: String::new(),
			prefs: test_prefs(),
			url: "/r/rust".to_string(),
			redirect_url: "r/rust".to_string(),
			is_filtered: false,
			is_flair_subscribed: false,
			fragment_url: "/subreddit-fragment/r/rust".to_string(),
			loading_message: "Loading posts...".to_string(),
			placeholder_message: "Loading posts...".to_string(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("data-fragment-url=\"/subreddit-fragment/r/rust\""));
		assert!(html.contains("data-error-message=\"Loading posts is taking longer than expected.\""));
		assert!(html.contains("data-timeout-ms=\"20000\""));
		assert!(html.contains("Loading posts..."));
	}

	#[test]
	fn subreddit_fragment_path_preserves_force_peer_query() {
		let request = Request::builder().uri("/r/rust/new?force_peer=1&limit=1").body(Body::empty()).unwrap();

		assert_eq!(super::subreddit_fragment_path(&request), "/subreddit-fragment/r/rust/new?force_peer=1&limit=1");
	}

	#[test]
	fn subreddit_video_length_shell_uses_canonical_reddit_target() {
		let html = super::SubredditShellTemplate {
			sub: Subreddit {
				name: "videos".to_string(),
				..Default::default()
			},
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: String::new(),
			prefs: test_prefs(),
			url: "/r/videos/video-length".to_string(),
			redirect_url: "r/videos/video-length".to_string(),
			is_filtered: false,
			is_flair_subscribed: false,
			fragment_url: "/subreddit-fragment/r/videos/video-length".to_string(),
			loading_message: "Loading videos...".to_string(),
			placeholder_message: "Loading videos...".to_string(),
			replace_selector: "main".to_string(),
			video_mode_enabled: true,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("https://www.reddit.com/r/videos/new"));
		assert!(!html.contains("https://www.reddit.com/r/videos/video-length"));
	}

	#[test]
	fn subreddit_video_mode_uses_reddit_new_as_external_fallback() {
		let html = super::SubredditShellTemplate {
			sub: Subreddit {
				name: "videos".to_string(),
				..Default::default()
			},
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: String::new(),
			prefs: test_prefs(),
			url: "/r/videos/video-length".to_string(),
			redirect_url: "r/videos/video-length".to_string(),
			is_filtered: false,
			is_flair_subscribed: false,
			fragment_url: "/subreddit-fragment/r/videos/video-length".to_string(),
			loading_message: "Loading videos...".to_string(),
			placeholder_message: "Loading videos...".to_string(),
			replace_selector: "main".to_string(),
			video_mode_enabled: true,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("https://www.reddit.com/r/videos/new"));
		assert!(!html.contains("https://www.reddit.com/r/videos/video-length"));
	}

	#[test]
	fn renders_home_shell_search_as_global_search() {
		let html = super::SubredditShellTemplate {
			sub: Subreddit {
				name: "rust+programming".to_string(),
				..Default::default()
			},
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: String::new(),
			prefs: test_prefs(),
			url: "/".to_string(),
			redirect_url: String::new(),
			is_filtered: false,
			is_flair_subscribed: false,
			fragment_url: "/subreddit-fragment".to_string(),
			loading_message: "Loading posts...".to_string(),
			placeholder_message: "Loading posts...".to_string(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("action=\"/search\""));
		assert!(!html.contains("action=\"/r/rust+programming/search\""));
	}

	#[test]
	fn renders_home_new_shell_with_feed_warmup_message() {
		let loading_message = "Loading followed feeds in the background. Warmed 12 of 48 feeds.".to_string();
		let html = super::SubredditShellTemplate {
			sub: Subreddit {
				name: "rust+programming".to_string(),
				..Default::default()
			},
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: String::new(),
			prefs: test_prefs(),
			url: "/new".to_string(),
			redirect_url: String::new(),
			is_filtered: false,
			is_flair_subscribed: false,
			fragment_url: "/subreddit-fragment/new".to_string(),
			loading_message: loading_message.clone(),
			placeholder_message: "Loading posts...".to_string(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains(&format!("<div class=\"feed_status_banner warming\">{loading_message}</div>")));
		assert!(html.contains("<center>Loading posts...</center>"));
		assert!(!html.contains("<center>Loading followed feeds in the background."));
		assert!(html.contains("feed_status_banner"));
	}

	#[tokio::test]
	async fn following_full_page_uses_existing_daemon_cache() {
		let username = "u_following_daemon_cache";
		let submitted_source = FeedSource::subreddit(username, FeedSort::New);
		let comments_source = FeedSource::user_comments(username, FeedSort::New);
		let mut cached_post = metric_post("following-daemon-cache-submission", username, 1_000, 42, false);
		cached_post.author.name = username.trim_start_matches("u_").to_string();

		post_cache::store_source_posts_for_cache(&submitted_source, vec![cached_post], String::new(), Some(RefreshOrigin::Daemon));
		post_cache::store_source_posts_for_cache(&comments_source, Vec::new(), String::new(), Some(RefreshOrigin::Daemon));
		post_cache::age_source_cache_for_test(&submitted_source, Duration::from_secs(post_cache::CACHE_TTL_SECS + 60));
		post_cache::age_source_cache_for_test(&comments_source, Duration::from_secs(post_cache::CACHE_TTL_SECS + 60));

		let req = Request::builder()
			.uri("/following/new")
			.header("cookie", format!("subscriptions={username}; show_nsfw=on; post_sort=new"))
			.body(Body::empty())
			.unwrap();
		let response = super::community(req).await.expect("following response");
		let body = hyper::body::to_bytes(response.into_body()).await.expect("following body");
		let html = String::from_utf8(body.to_vec()).expect("utf-8 html");

		assert!(html.contains("Post following-daemon-cache-submission"), "{html}");
		assert!(html.contains("from daemon cache"), "{html}");
	}

	#[test]
	fn subreddit_shell_keeps_subscription_controls_available() {
		let mut prefs = test_prefs();
		prefs.subscriptions = vec!["rust".to_string()];

		let html = super::SubredditShellTemplate {
			sub: Subreddit {
				name: "rust".to_string(),
				..Default::default()
			},
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: String::new(),
			prefs,
			url: "/r/rust".to_string(),
			redirect_url: "r/rust".to_string(),
			is_filtered: false,
			is_flair_subscribed: false,
			fragment_url: "/subreddit-fragment/r/rust".to_string(),
			loading_message: "Loading posts...".to_string(),
			placeholder_message: "Loading posts...".to_string(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("data-subscription-target=\"subreddit:rust\""));
		assert!(html.contains("action=\"/r/rust/redgifs-exempt?redirect=r/rust\""));
		assert!(html.contains(">Allow multi-profile RedGIF posters</button>"));
	}

	#[test]
	fn subreddit_sidebar_renders_active_redgifs_exemption_and_toggle() {
		let mut prefs = test_prefs();
		prefs.redgifs_filter_exempt_subreddits = vec!["rust".to_string()];
		prefs.prepare_for_runtime();
		let html = super::SubredditShellTemplate {
			sub: Subreddit {
				name: "rust".to_string(),
				..Default::default()
			},
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: String::new(),
			prefs,
			url: "/r/rust".to_string(),
			redirect_url: "r/rust".to_string(),
			is_filtered: false,
			is_flair_subscribed: false,
			fragment_url: "/subreddit-fragment/r/rust".to_string(),
			loading_message: "Loading posts...".to_string(),
			placeholder_message: "Loading posts...".to_string(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("action=\"/r/rust/redgifs-unexempt?redirect=r/rust\""));
		assert!(html.contains(">Use multi-profile RedGIF filter</button>"));
		assert!(html.contains("allowed in this community only"));
	}

	#[test]
	fn subreddit_redgifs_exemption_toggle_preserves_unrelated_preferences() {
		let mut prefs = test_prefs();
		prefs.filters = vec!["u_reposter".to_string(), "anothercommunity".to_string()];
		prefs.subscriptions = vec!["rust".to_string()];
		prefs.prepare_for_runtime();
		let filters = prefs.filters.clone();
		let subscriptions = prefs.subscriptions.clone();

		super::update_redgifs_filter_exemption(&mut prefs, "R/Rust", true);
		assert!(prefs.has_redgifs_filter_exemption("rust"));
		assert_eq!(prefs.filters, filters);
		assert_eq!(prefs.subscriptions, subscriptions);

		super::update_redgifs_filter_exemption(&mut prefs, "rust", false);
		assert!(!prefs.has_redgifs_filter_exemption("rust"));
		assert_eq!(prefs.filters, filters);
		assert_eq!(prefs.subscriptions, subscriptions);
	}

	#[test]
	fn subreddit_redgifs_exemption_routes_map_to_explicit_actions() {
		assert_eq!(super::redgifs_filter_exemption_action("/r/rust/redgifs-exempt"), Some(true));
		assert_eq!(super::redgifs_filter_exemption_action("/r/rust/redgifs-unexempt"), Some(false));
		assert_eq!(super::redgifs_filter_exemption_action("/r/rust"), None);
	}

	#[test]
	fn subreddit_shell_does_not_emit_fragment_cache_attrs() {
		let html = super::SubredditShellTemplate {
			sub: Subreddit::default(),
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: String::new(),
			prefs: test_prefs(),
			url: "/".to_string(),
			redirect_url: String::new(),
			is_filtered: false,
			is_flair_subscribed: false,
			fragment_url: "/subreddit-fragment".to_string(),
			loading_message: "Loading posts...".to_string(),
			placeholder_message: "Loading posts...".to_string(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(!html.contains("data-fragment-cache-enabled="));
		assert!(!html.contains("data-fragment-cache-version="));
	}

	#[test]
	fn builds_wiki_fragment_path() {
		let req = Request::builder().uri("/r/rust/wiki/install?view=compact").body(Body::empty()).unwrap();
		assert_eq!(super::wiki_fragment_path(&req), "/wiki-fragment/r/rust/wiki/install?view=compact");
	}

	#[test]
	fn renders_shell_mode_wiki_placeholder_with_fragment_url() {
		let html = super::WikiTemplate {
			sub: "rust".to_string(),
			wiki: String::new(),
			page: "install".to_string(),
			prefs: test_prefs(),
			url: "/r/rust/wiki/install".to_string(),
			shell_mode: true,
			fragment_url: "/wiki-fragment/r/rust/wiki/install".to_string(),
			loading_message: "Loading wiki page...".to_string(),
		}
		.render()
		.unwrap();

		assert!(html.contains("data-fragment-url=\"/wiki-fragment/r/rust/wiki/install\""));
		assert!(html.contains("Loading wiki page..."));
	}

	#[test]
	fn subreddit_fragment_does_not_render_duplicate_feed_status_banner() {
		let html = super::SubredditFragmentTemplate {
			sub: Subreddit {
				name: "rust+programming".to_string(),
				..Default::default()
			},
			posts: vec![test_post()],
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			sort: ("new".to_string(), String::new()),
			sort_options: Vec::new(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: "/".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/".to_string(),
			redirect_url: String::new(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			cache_notice: String::new(),
			peer_origin: String::new(),
			is_flair_subscribed: false,
			feed_session_id: "demo".to_string(),
			feed_status_url: "/feed-status?feed_session=demo".to_string(),
			feed_fragment_url: "/feed-fragment?feed_session=demo".to_string(),
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		}
		.render()
		.unwrap();

		assert!(!html.contains("feed_status_banner"));
		assert!(html.contains("id=\"feed_content\""));
	}

	#[test]
	fn following_feed_posts_use_standard_listing_header() {
		let mut post = test_post();
		post.following_comments.push(test_following_comment());

		let html = super::FeedFragmentTemplate {
			posts: vec![post],
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/following/new".to_string(),
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
		}
		.render()
		.unwrap();

		assert!(html.contains("class=\"post_header\""));
		assert!(html.contains("<a class=\"post_subreddit\" href=\"/r/demo_sub\">r/demo_sub</a>"));
		assert!(html.contains("<a class=\"post_author \" href=\"/u/demo_user\">u/demo_user</a>"));
		assert!(html.contains("post_following_comments"));
		assert!(!html.contains("post_header_following_thread"));
		assert!(!html.contains("post_following_thread_meta"));
	}

	#[test]
	fn feed_preview_from_followed_author_renders_expanded() {
		let mut post = test_post();
		post.preview_body = "<div class=\"md\"><p>Long followed-author preview.</p></div>".to_string();

		let mut prefs = test_prefs();
		prefs.subscriptions = vec!["u_demo_user".to_string()];
		prefs.prepare_for_runtime();

		let html = super::FeedFragmentTemplate {
			posts: vec![post],
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs,
			url: "/following/new".to_string(),
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
		}
		.render()
		.unwrap();

		assert!(html.contains("class=\"post_body post_preview expanded\""));
	}

	#[test]
	fn subreddit_fragment_renders_peer_origin_marker() {
		let html = super::SubredditFragmentTemplate {
			sub: Subreddit::default(),
			posts: vec![test_post()],
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			sort: ("hot".to_string(), String::new()),
			sort_options: Vec::new(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: "/r/rust".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/r/rust".to_string(),
			redirect_url: "r/rust".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			cache_notice: String::new(),
			peer_origin: "http://192.168.50.251:18081".to_string(),
			is_flair_subscribed: false,
			feed_session_id: String::new(),
			feed_status_url: String::new(),
			feed_fragment_url: String::new(),
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		}
		.render()
		.unwrap();

		assert!(html.contains("post_origin_marker peer_origin_marker"));
		assert!(html.contains("from spare"));
		assert!(html.contains("Fetched from Redlib peer http://192.168.50.251:18081"));
		assert!(html.find("from spare").unwrap() < html.find("Primary post title").unwrap());
	}

	#[test]
	fn does_not_render_sidebar_panel_when_subreddit_sidebar_is_empty() {
		let html = SubredditTemplate {
			sub: Subreddit {
				name: "rust".to_string(),
				info: "<div class=\"md\"></div>".to_string(),
				has_sidebar: false,
				..Default::default()
			},
			posts: vec![test_post()],
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			sort: ("hot".to_string(), String::new()),
			sort_options: Vec::new(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: "/r/rust".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/r/rust".to_string(),
			redirect_url: "r/rust".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			cache_notice: String::new(),
			peer_origin: String::new(),
			is_flair_subscribed: false,
			feed_session_id: String::new(),
			feed_status_url: String::new(),
			feed_fragment_url: String::new(),
			feed_status_message: String::new(),
			feed_status_warming: false,
			show_contextual_pagination: false,
			contextual_pagination: Vec::new(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		}
		.render()
		.unwrap();

		assert!(!html.contains("id=\"sidebar\""));
	}

	#[tokio::test]
	async fn source_unavailable_subreddit_fragment_renders_deterministic_notice() {
		let req = Request::builder()
			.uri("/subreddit-fragment/r/missing_sub/new")
			.body(Body::empty())
			.expect("request should build");
		let response = super::render_subreddit_source_unavailable_fragment(
			&req,
			Subreddit {
				name: "missing_sub".to_string(),
				..Default::default()
			},
			"missing_sub",
			crate::client::SourceUnavailable::Banned,
			("new".to_string(), String::new()),
			Vec::new(),
			Vec::new(),
			"",
			"/r/missing_sub/new".to_string(),
			"r/missing_sub/new".to_string(),
			false,
			&HashSet::new(),
			Vec::new(),
			None,
		)
		.expect("fragment should render");

		assert_eq!(response.status(), 200);
		let body = body::to_bytes(response.into_body()).await.expect("body should read");
		let html = String::from_utf8(body.to_vec()).expect("fragment should be utf8");
		assert!(html.contains("r/missing_sub has been banned from Reddit."));
		assert!(!html.contains("Loading posts"));
	}

	#[test]
	fn subreddit_fragment_renders_contextual_pagination_links() {
		let html = super::SubredditFragmentTemplate {
			sub: Subreddit::default(),
			posts: vec![test_post()],
			top_users: Vec::new(),
			related_subreddits: Default::default(),
			sort: ("hot".to_string(), String::new()),
			sort_options: Vec::new(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			active_flair_filter_encoded: String::new(),
			clear_flair_filter_url: "/".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/hot".to_string(),
			redirect_url: String::new(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
			cache_notice: String::new(),
			peer_origin: String::new(),
			is_flair_subscribed: false,
			feed_session_id: "demo".to_string(),
			feed_status_url: String::new(),
			feed_fragment_url: String::new(),
			show_contextual_pagination: true,
			contextual_pagination: vec![
				super::ContextualPageLink {
					label: "~5k".to_string(),
					url: "/hot?cache_page=0&feed_session=demo".to_string(),
					current: true,
					title: "Jump to posts around 5k score".to_string(),
				},
				super::ContextualPageLink {
					label: "~500".to_string(),
					url: "/hot?cache_page=1&feed_session=demo".to_string(),
					current: false,
					title: "Jump to posts around 500 score".to_string(),
				},
			],
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
			show_hide_seen_toggle: false,
			hide_seen_on: false,
			hide_seen_toggle_url: String::new(),
		}
		.render()
		.unwrap();

		assert!(html.contains("contextual_pagination"));
		assert!(html.contains("contextual_page_link selected"));
		assert!(html.contains("~5k"));
		assert!(html.contains("~500"));
	}

	#[tokio::test]
	async fn contextual_pagination_for_new_uses_ready_page_age_bands() {
		let now = super::now_unix();
		let source = FeedSource::subreddit("contextual-new-test", FeedSort::New);
		let mut posts = Vec::new();
		for index in 0..25 {
			posts.push(metric_post(&format!("new-a-{index}"), "contextual-new-test", now - 1_800 - index * 60, 100, false));
		}
		for index in 0..25 {
			posts.push(metric_post(&format!("new-b-{index}"), "contextual-new-test", now - 4 * 3_600 - index * 120, 95, false));
		}
		for index in 0..25 {
			posts.push(metric_post(&format!("new-c-{index}"), "contextual-new-test", now - 36 * 3_600 - index * 600, 90, false));
		}
		for index in 0..25 {
			posts.push(metric_post(
				&format!("new-d-{index}"),
				"contextual-new-test",
				now - 7 * 24 * 3_600 - index * 3_600,
				85,
				false,
			));
		}

		let session_id = seed_contextual_session(source, posts, "contextual-new-fingerprint").await;
		let links = super::build_contextual_pagination("/new", &session_id, 0, "new");

		assert!(links.len() >= 3);
		assert!(links.iter().any(|link| link.current));
		assert!(links.iter().all(|link| link.label.starts_with('~')));
		let unique = links.iter().map(|link| link.label.clone()).collect::<HashSet<_>>();
		assert_eq!(unique.len(), links.len());
	}

	#[tokio::test]
	async fn contextual_session_rejects_stale_sources_after_unfollow() {
		let stale_source = FeedSource::subreddit("u_contextual-unfollowed", FeedSort::New);
		let current_source = FeedSource::subreddit("u_contextual-followed", FeedSort::New);
		post_cache::store_source_posts_for_cache(
			&stale_source,
			vec![metric_post("stale-unfollowed-post", "contextual-unfollowed", 1_000, 10, false)],
			String::new(),
			Some(RefreshOrigin::FrontPageSession),
		);
		post_cache::store_source_posts_for_cache(
			&current_source,
			vec![metric_post("current-followed-post", "contextual-followed", 2_000, 20, false)],
			String::new(),
			Some(RefreshOrigin::FrontPageSession),
		);

		let stale_session_id = post_cache::ensure_feed_session(
			FeedSessionRequest {
				sources: vec![stale_source],
				query_fingerprint: "following-new".to_string(),
				bundle_followed_user_comments: true,
				merge_sort: Some(FeedSort::New),
				..Default::default()
			},
			0,
			post_cache::POSTS_PER_PAGE,
		)
		.await;
		let current_request = FeedSessionRequest {
			sources: vec![current_source],
			query_fingerprint: "following-new".to_string(),
			bundle_followed_user_comments: true,
			merge_sort: Some(FeedSort::New),
			..Default::default()
		};

		let current_session_id = super::ensure_contextual_feed_session(stale_session_id.clone(), current_request, 0).await;
		assert_ne!(current_session_id, stale_session_id);
		let page = post_cache::feed_page(&current_session_id, 0, post_cache::POSTS_PER_PAGE).expect("current session page");
		let ids = page.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>();
		assert_eq!(ids, vec!["current-followed-post"]);
	}

	#[tokio::test]
	async fn contextual_pagination_for_hot_uses_score_bands_and_suppresses_hidden_scores() {
		let source = FeedSource::subreddit("contextual-hot-test", FeedSort::Hot);
		let mut posts = Vec::new();
		for index in 0..25 {
			posts.push(metric_post(
				&format!("hot-a-{index}"),
				"contextual-hot-test",
				1_000 - index as u64,
				5_000 - index as i64 * 10,
				false,
			));
		}
		for index in 0..25 {
			posts.push(metric_post(
				&format!("hot-b-{index}"),
				"contextual-hot-test",
				900 - index as u64,
				700 - index as i64 * 5,
				false,
			));
		}
		for index in 0..25 {
			posts.push(metric_post(&format!("hot-c-{index}"), "contextual-hot-test", 800 - index as u64, 80 - index as i64, false));
		}
		let session_id = seed_contextual_session(source, posts, "contextual-hot-fingerprint").await;
		let links = super::build_contextual_pagination("/hot", &session_id, 0, "hot");

		assert!(links.len() >= 3);
		assert!(links.iter().any(|link| link.current));
		assert!(links.iter().all(|link| link.label.starts_with('~')));

		let hidden_source = FeedSource::subreddit("contextual-hot-hidden", FeedSort::Hot);
		let hidden_posts = (0..75)
			.map(|index| {
				let mut post = metric_post(&format!("hidden-{index}"), "contextual-hot-hidden", 700 - index as u64, 0, false);
				post.score = ("\u{2022}".to_string(), "Hidden".to_string());
				post
			})
			.collect::<Vec<_>>();
		let hidden_session = seed_contextual_session(hidden_source, hidden_posts, "contextual-hot-hidden-fingerprint").await;
		assert!(super::build_contextual_pagination("/hot", &hidden_session, 0, "hot").is_empty());
	}

	#[tokio::test]
	async fn contextual_pagination_for_later_pages_uses_surrounding_ready_pages() {
		let now = super::now_unix();
		let source = FeedSource::subreddit("contextual-window-test", FeedSort::New);
		let mut posts = Vec::new();
		let page_ages = [60 * 60, 3 * 60 * 60, 8 * 60 * 60, 24 * 60 * 60, 3 * 24 * 60 * 60, 7 * 24 * 60 * 60];

		for (page_index, age_secs) in page_ages.into_iter().enumerate() {
			for post_index in 0..25 {
				posts.push(metric_post(
					&format!("window-{page_index}-{post_index}"),
					"contextual-window-test",
					now - age_secs - post_index * 60,
					100 - page_index as i64,
					false,
				));
			}
		}

		let session_id = seed_contextual_session(source, posts, "contextual-window-fingerprint").await;
		let links = super::build_contextual_pagination("/new", &session_id, 3, "new");

		assert!(links.len() >= 5);
		assert!(links.iter().any(|link| link.current));
		assert!(links.iter().any(|link| link.url.contains("cache_page=1")));
		assert!(links.iter().any(|link| link.url.contains("cache_page=4")));
	}

	#[tokio::test]
	async fn contextual_pagination_for_front_page_uses_all_cached_pages() {
		let now = super::now_unix();
		let source = FeedSource::subreddit("contextual-front-window-test", FeedSort::New);
		let mut posts = Vec::new();
		let page_ages = [60 * 60, 3 * 60 * 60, 8 * 60 * 60, 24 * 60 * 60, 3 * 24 * 60 * 60, 7 * 24 * 60 * 60];

		for (page_index, age_secs) in page_ages.into_iter().enumerate() {
			for post_index in 0..25 {
				posts.push(metric_post(
					&format!("front-window-{page_index}-{post_index}"),
					"contextual-front-window-test",
					now - age_secs - post_index * 60,
					100 - page_index as i64,
					false,
				));
			}
		}

		let session_id = seed_contextual_session(source, posts, "contextual-front-window-fingerprint").await;
		let links = super::build_contextual_pagination("/new", &session_id, 0, "new");

		assert!(links.len() >= 5);
		assert!(links.iter().any(|link| link.current));
		assert!(links.iter().any(|link| link.url.contains("cache_page=4")));
		assert!(links.iter().any(|link| link.url.contains("cache_page=5")));
	}

	#[tokio::test]
	async fn feed_fragment_keeps_seen_posts_visible() {
		let source = FeedSource::subreddit("feed-fragment-seen-fill-test", FeedSort::New);
		let posts = (0..30)
			.map(|index| {
				metric_post(
					&format!("seen-fill-{index}"),
					"feed-fragment-seen-fill-test",
					1_000 - index as u64,
					100 - index as i64,
					false,
				)
			})
			.collect::<Vec<_>>();

		let session_id = seed_contextual_session(source, posts, "feed-fragment-seen-fill-fingerprint").await;
		let merged_posts = post_cache::feed_merged_posts(&session_id).expect("merged session posts");
		assert_eq!(merged_posts.len(), 30);
		let browser_session_id = format!("seen-fill-browser-{session_id}");
		let (first_page, _) = crate::seen_posts::page_and_record_seen(&browser_session_id, true, &merged_posts, 0, post_cache::POSTS_PER_PAGE);
		assert_eq!(first_page.len(), post_cache::POSTS_PER_PAGE);

		let req = Request::builder()
			.uri(format!("/feed-fragment?feed_session={session_id}&cache_page=0&current_url=/new"))
			.header("cookie", format!("rl_session={browser_session_id}"))
			.body(Body::empty())
			.unwrap();
		let response = super::feed_fragment(req).await.expect("feed fragment response");
		let body = hyper::body::to_bytes(response.into_body()).await.expect("feed fragment body");
		let html = String::from_utf8(body.to_vec()).expect("utf-8 html");

		assert!(html.contains("Post seen-fill-0"), "{html}");
		assert!(html.contains("Post seen-fill-24"), "{html}");
		assert!(!html.contains("Post seen-fill-25"), "{html}");
		assert!(!html.contains("No posts were found."), "{html}");
	}

	#[tokio::test]
	async fn feed_fragment_keeps_seen_posts_visible_for_following_routes() {
		let source = FeedSource::subreddit("following-seen-fill-test", FeedSort::New);
		let posts = (0..30)
			.map(|index| {
				metric_post(
					&format!("following-seen-fill-{index}"),
					"following-seen-fill-test",
					1_000 - index as u64,
					100 - index as i64,
					false,
				)
			})
			.collect::<Vec<_>>();

		let session_id = seed_contextual_session(source, posts, "following-seen-fill-fingerprint").await;
		let merged_posts = post_cache::feed_merged_posts(&session_id).expect("merged session posts");
		assert_eq!(merged_posts.len(), 30);
		let browser_session_id = format!("following-seen-fill-browser-{session_id}");
		let (first_page, _) = crate::seen_posts::page_and_record_seen(&browser_session_id, true, &merged_posts, 0, post_cache::POSTS_PER_PAGE);
		assert_eq!(first_page.len(), post_cache::POSTS_PER_PAGE);

		let req = Request::builder()
			.uri(format!("/feed-fragment?feed_session={session_id}&cache_page=0&current_url=/following/new"))
			.header("cookie", format!("rl_session={browser_session_id}"))
			.body(Body::empty())
			.unwrap();
		let response = super::feed_fragment(req).await.expect("feed fragment response");
		let body = hyper::body::to_bytes(response.into_body()).await.expect("feed fragment body");
		let html = String::from_utf8(body.to_vec()).expect("utf-8 html");

		assert!(html.contains("Post following-seen-fill-0"), "{html}");
		assert!(html.contains("Post following-seen-fill-24"), "{html}");
		assert!(!html.contains("Post following-seen-fill-25"), "{html}");
		assert!(!html.contains("No posts were found."), "{html}");
	}

	#[test]
	fn detects_empty_sidebar_markup_as_empty() {
		assert!(!super::subreddit_sidebar_has_content(""));
		assert!(!super::subreddit_sidebar_has_content("   "));
		assert!(!super::subreddit_sidebar_has_content("<div class=\"md\"></div>"));
		assert!(!super::subreddit_sidebar_has_content("<div class=\"md\"><p> </p></div>"));
		assert!(super::subreddit_sidebar_has_content("<div class=\"md\"><p>Rules</p></div>"));
	}

	#[test]
	fn updates_urls_without_losing_existing_query_pairs() {
		assert_eq!(
			update_url_query("/r/rust?sort=hot&after=t3_demo", &[("flair", Some("Discussion")), ("after", None)]),
			"/r/rust?sort=hot&flair=Discussion"
		);
	}

	#[test]
	fn listing_path_includes_requested_after_cursor() {
		assert_eq!(
			super::build_listing_path("rust", "hot", "raw_json=1", "t3_after", ""),
			"/r/rust/hot.json?raw_json=1&after=t3_after"
		);
	}

	#[test]
	fn listing_path_includes_requested_before_cursor() {
		assert_eq!(
			super::build_listing_path("rust", "hot", "raw_json=1", "", "t3_before"),
			"/r/rust/hot.json?raw_json=1&before=t3_before"
		);
	}

	#[test]
	fn filters_posts_by_exact_flair_name() {
		let mut posts = vec![
			Post {
				flair: Flair {
					text: "Discussion".to_string(),
					..empty_flair()
				},
				..test_post()
			},
			Post {
				flair: Flair {
					text: "News".to_string(),
					..empty_flair()
				},
				..test_post()
			},
		];

		filter_posts_by_flair(&mut posts, "Discussion");

		assert_eq!(posts.len(), 1);
		assert_eq!(posts[0].flair.text, "Discussion");
	}

	#[test]
	fn sort_links_reset_front_cursor_and_pagination() {
		let options = super::build_sort_options("", "/?sort=hot&front_cursor=abc&after=t3_demo&t=day", "hot", "");
		assert!(options.iter().all(|option| !option.url.contains(FRONT_CURSOR_PARAM)));
		assert!(options.iter().all(|option| !option.url.contains("after=")));
	}

	#[test]
	fn aggregate_frontpage_sort_links_use_frontpage_routes() {
		let options = super::build_sort_options("", "/hot?front_cursor=abc&after=t3_demo&t=day", "hot", "");
		let discovered = options.iter().find(|option| option.label == "Discovered").unwrap();
		let random = options.iter().find(|option| option.label == "Random").unwrap();
		let top = options.iter().find(|option| option.label == "Top").unwrap();
		let rising = options.iter().find(|option| option.label == "Rising").unwrap();
		let controversial = options.iter().find(|option| option.label == "Controversial").unwrap();

		assert_eq!(discovered.url, "/discovered?t=day");
		assert_eq!(random.url, "/random?t=day");
		assert_eq!(top.url, "/top?t=day");
		assert_eq!(rising.url, "/rising?t=day");
		assert_eq!(controversial.url, "/controversial?t=day");
	}

	#[test]
	fn home_discovered_uses_new_source_windows_and_discovery_merge() {
		let mut prefs = test_prefs();
		prefs.subscriptions = vec!["rust".to_string(), "programming".to_string()];
		let sources = super::build_front_page_session_sources(&prefs, "default", true, true, "unused", "discovered");

		assert!(super::front_page_uses_contextual_session(false, "discovered"));
		assert_eq!(super::contextual_feed_merge_sort("default", "discovered"), Some(FeedSort::New));
		assert!(!sources.is_empty());
		assert!(sources
			.iter()
			.all(|source| matches!(source, FeedSource::Subreddit { sort: FeedSort::New, .. } | FeedSource::Flair { sort: FeedSort::New, .. })));
	}

	#[test]
	fn following_sort_links_use_following_routes() {
		let options = super::build_sort_options("/following", "/following/hot?front_cursor=abc&after=t3_demo&t=day", "hot", "");
		let chronological = options.iter().find(|option| option.label == "Chronological").unwrap();
		let discovered = options.iter().find(|option| option.label == "Discovered").unwrap();
		let random = options.iter().find(|option| option.label == "Random").unwrap();
		let top = options.iter().find(|option| option.label == "Top").unwrap();
		let video_length = options.iter().find(|option| option.label == "Video Length").unwrap();

		assert_eq!(chronological.url, "/following/new?t=day");
		assert_eq!(discovered.url, "/following/discovered?t=day");
		assert_eq!(random.url, "/following/random?t=day");
		assert_eq!(top.url, "/following/top?t=day");
		assert_eq!(video_length.url, "/following/video-length?t=day");
	}

	#[test]
	fn subreddit_sort_links_omit_random() {
		let options = super::build_sort_options("/r/rust", "/r/rust/hot?front_cursor=abc&after=t3_demo&t=day", "hot", "");

		assert!(options.iter().all(|option| option.label != "Random"));
		assert!(options.iter().all(|option| !option.url.contains("/random")));
	}

	#[test]
	fn subreddit_random_sort_canonicalizes_to_new() {
		assert_eq!(canonical_new_sort_url("/r/rust/random", Some("flair=Questions")), "/r/rust/new?flair=Questions");
		assert_eq!(canonical_new_sort_url("/subreddit-fragment/r/rust/random", None), "/subreddit-fragment/r/rust/new");
	}

	#[test]
	fn top_level_random_ignores_following_front_page_preference() {
		assert_eq!(super::front_page_for_path("/random", "following"), "default");
		assert_eq!(super::front_page_for_path("/subreddit-fragment/random", "following"), "default");
		assert_eq!(super::front_page_for_path("/following/random", "default"), "following");
		assert_eq!(super::front_page_for_path("/", "following"), "following");
	}

	#[test]
	fn random_front_page_uses_new_sources_with_random_merge_sort() {
		let mut prefs = test_prefs();
		prefs.subscriptions = vec!["rust".to_string(), "programming".to_string()];

		let sources = super::build_front_page_session_sources(&prefs, "default", true, true, "rust+programming", "random");

		assert!(super::front_page_uses_contextual_session(false, "random"));
		assert_eq!(super::contextual_feed_merge_sort("default", "random"), Some(FeedSort::Random));
		assert_eq!(super::build_listing_path("rust", "random", "raw_json=1", "", ""), "/r/rust/new.json?raw_json=1");
		assert!(sources
			.iter()
			.all(|source| matches!(source, FeedSource::Subreddit { sort: FeedSort::New, .. } | FeedSource::Flair { sort: FeedSort::New, .. })));
	}

	#[test]
	fn top_level_random_uses_main_sources_not_following_comment_sources() {
		let mut prefs = test_prefs();
		prefs.subscriptions = vec!["rust".to_string(), "u_demo".to_string()];
		let front_page = super::front_page_for_path("/random", "following");
		let front_page_is_default = front_page == "default" || front_page.is_empty();

		let sources = super::build_front_page_session_sources(&prefs, &front_page, front_page_is_default, true, "rust+u_demo", "random");

		assert!(sources
			.iter()
			.any(|source| matches!(source, FeedSource::Subreddit { name, sort: FeedSort::New } if name == "rust")));
		assert!(sources.iter().all(|source| !matches!(source, FeedSource::UserComments { .. })));
	}

	#[test]
	fn default_front_page_uses_the_actual_follow_ratio() {
		let subscriptions = vec![
			"u_charlie".to_string(),
			"rust".to_string(),
			"u_alpha".to_string(),
			"programming".to_string(),
			"webdev".to_string(),
			"u_bravo".to_string(),
			"science".to_string(),
			"askreddit".to_string(),
		];

		assert_eq!(
			super::ordered_front_page_subscriptions(&subscriptions),
			vec![
				"askreddit".to_string(),
				"u_alpha".to_string(),
				"programming".to_string(),
				"rust".to_string(),
				"u_bravo".to_string(),
				"science".to_string(),
				"u_charlie".to_string(),
				"webdev".to_string(),
			]
		);
	}

	#[test]
	fn default_front_page_sources_do_not_give_followed_users_precedence() {
		let mut prefs = test_prefs();
		prefs.subscriptions = vec![
			"u_charlie".to_string(),
			"rust".to_string(),
			"u_alpha".to_string(),
			"programming".to_string(),
			"webdev".to_string(),
			"u_bravo".to_string(),
			"science".to_string(),
			"askreddit".to_string(),
		];

		let sources = super::build_front_page_session_sources(&prefs, "default", true, true, "unused", "new");
		let source_names = sources
			.iter()
			.map(|source| match source {
				FeedSource::Subreddit { name, .. } => name.clone(),
				other => panic!("unexpected source: {other:?}"),
			})
			.collect::<Vec<_>>();

		assert_eq!(
			source_names,
			vec!["askreddit", "u_alpha", "programming", "rust", "u_bravo", "science", "u_charlie", "webdev"]
		);
	}

	#[test]
	fn default_front_page_prefix_matches_a_twenty_five_percent_user_follow_ratio() {
		let mut subscriptions = (0..75).map(|index| format!("subreddit{index:02}")).collect::<Vec<_>>();
		subscriptions.extend((0..25).map(|index| format!("u_profile{index:02}")));

		let ordered = super::ordered_front_page_subscriptions(&subscriptions);
		let first_window = &ordered[..32];

		assert!(!super::is_followed_user_subscription(&first_window[0]));
		assert_eq!(first_window.iter().filter(|name| super::is_followed_user_subscription(name)).count(), 8);
	}

	#[test]
	fn video_length_sort_option_uses_human_label_and_clears_stale_filters_when_entering_mode() {
		let options = super::build_sort_options(
			"/r/rust",
			"/r/rust/hot?video_sub=rust&video_user=alice&video_age_min_hours=4&video_age_max_hours=48&cache_page=2&t=day",
			"hot",
			"",
		);
		let video_length = options.iter().find(|option| option.label == "Video Length").unwrap();

		assert_eq!(video_length.url, "/r/rust/video-length?t=day");
	}

	#[test]
	fn video_length_sort_links_preserve_filters_within_video_mode_only() {
		let options = super::build_sort_options(
			"/r/rust",
			"/r/rust/video-length?video_sub=rust&video_user=alice&video_age_min_hours=4&video_age_max_hours=48&cache_page=2&t=day",
			"video-length",
			"",
		);
		let video_length = options.iter().find(|option| option.label == "Video Length").unwrap();
		let hot = options.iter().find(|option| option.label == "Hot").unwrap();

		assert_eq!(
			video_length.url,
			"/r/rust/video-length?video_sub=rust&video_user=alice&video_age_min_hours=4&video_age_max_hours=48&t=day"
		);
		assert_eq!(hot.url, "/r/rust/hot?t=day");
	}

	#[test]
	fn video_mode_sidebar_clear_url_strips_video_filters_and_pagination() {
		let sidebar = super::build_video_mode_sidebar(
			"/video-length?video_sub=rust&video_user=alice&video_age_min_hours=4&video_age_max_hours=48&cache_page=2&t=day&flair=News",
			&crate::video_mode::VideoModeListing {
				posts: Vec::new(),
				subreddit_options: Vec::new(),
				user_options: Vec::new(),
				age_min_hours: 4,
				age_max_hours: 48,
				age_bound_hours: 72,
			},
			&[],
			&crate::video_mode::VideoModeQuery::default(),
		);

		assert_eq!(sidebar.action_url, "/video-length");
		assert_eq!(sidebar.clear_url, "/video-length?t=day&flair=News");
		assert_eq!(
			sidebar.clear_cookie_redirect_href,
			format!(
				"/api/video-mode/clear-redirect?{}",
				url::form_urlencoded::Serializer::new(String::new())
					.append_pair("redirect", "/video-length?t=day&flair=News")
					.finish()
			)
		);
		assert_eq!(
			sidebar.hidden_query_pairs,
			vec![("t".to_string(), "day".to_string()), ("flair".to_string(), "News".to_string())]
		);
	}

	#[test]
	fn splits_large_subscription_lists_into_multiple_listing_chunks() {
		let subscriptions = (0..1_200).map(|idx| format!("subreddit{idx}")).collect::<Vec<_>>();
		let chunks = split_subscriptions_for_listing(&subscriptions, "hot", "raw_json=1");

		assert!(chunks.len() > 1);
		assert_eq!(chunks.join("+"), subscriptions.join("+"));
		assert!(chunks.iter().all(|chunk| chunk.split('+').count() <= super::MAX_SUBREDDITS_PER_LISTING_CHUNK));
		assert!(chunks
			.iter()
			.all(|chunk| { format!("/r/{chunk}/hot.json?raw_json=1").len() + super::AFTER_PATH_HEADROOM <= super::REDDIT_LISTING_PATH_SOFT_LIMIT }));
	}

	#[test]
	fn aggregate_cursor_round_trips() {
		let cursor = AggregateFrontCursor {
			v: 1,
			sources: vec![
				AggregateFeedSource::SubredditChunk { chunk: "a+b".to_string() },
				AggregateFeedSource::Flair {
					subreddit: "rust".to_string(),
					flair: "News".to_string(),
				},
			],
			source_afters: vec!["t3_a".to_string(), String::new()],
			next_source_index: 1,
			prev_stack: vec![AggregateFrontPageState {
				source_afters: vec![String::new(), String::new()],
				next_source_index: 0,
			}],
			recent_post_ids: vec!["t3_demo".to_string()],
			sort: "hot".to_string(),
			timeframe: String::new(),
			query_fingerprint: "sort=hot&flair=News".to_string(),
		};

		let encoded = encode_front_cursor(&cursor).unwrap();
		let decoded = decode_front_cursor(&encoded).unwrap();

		assert_eq!(decoded, cursor);
	}

	#[test]
	fn caches_subreddit_nsfw_status_with_metadata() {
		super::SUBREDDIT_METADATA_CACHE.lock().unwrap().clear();
		super::SUBREDDIT_NSFW_CACHE.lock().unwrap().clear();

		super::store_subreddit_metadata(&Subreddit {
			name: "demo".to_string(),
			nsfw: true,
			..Subreddit::default()
		});

		assert_eq!(super::cached_subreddit_nsfw_status("demo"), Some(true));
		assert!(super::cached_subreddit_metadata("demo").is_some());
	}

	#[test]
	fn remembered_subreddit_metadata_stays_servable_beyond_short_ttl() {
		super::SUBREDDIT_METADATA_CACHE.lock().unwrap().clear();
		super::SUBREDDIT_NSFW_CACHE.lock().unwrap().clear();
		crate::post_cache::REMEMBERED_SUBREDDITS.lock().unwrap().clear();

		crate::post_cache::remember_subreddit_request("demo");
		super::store_subreddit_metadata(&Subreddit {
			name: "demo".to_string(),
			nsfw: true,
			..Subreddit::default()
		});
		super::SUBREDDIT_METADATA_CACHE.lock().unwrap().get_mut("demo").unwrap().fetched_at =
			Instant::now() - std::time::Duration::from_secs(super::SUBREDDIT_METADATA_CACHE_TTL_SECS + 60);
		super::SUBREDDIT_NSFW_CACHE.lock().unwrap().get_mut("demo").unwrap().fetched_at =
			Instant::now() - std::time::Duration::from_secs(super::SUBREDDIT_METADATA_CACHE_TTL_SECS + 60);

		assert!(super::cached_subreddit_metadata("demo").is_some());
		assert_eq!(super::cached_subreddit_nsfw_status("demo"), Some(true));
	}

	#[test]
	fn cleanup_removes_decayed_metadata_and_plain_cache() {
		super::SUBREDDIT_METADATA_CACHE.lock().unwrap().clear();
		super::SUBREDDIT_NSFW_CACHE.lock().unwrap().clear();
		crate::post_cache::REMEMBERED_SUBREDDITS.lock().unwrap().clear();

		crate::post_cache::remember_subreddit_request("demo");
		crate::post_cache::publish_plain_subreddit_refresh("demo", vec![test_post()], String::new(), crate::post_cache::RefreshOrigin::DirectLiveFetch);
		super::store_subreddit_metadata(&Subreddit {
			name: "demo".to_string(),
			nsfw: false,
			..Subreddit::default()
		});
		crate::post_cache::REMEMBERED_SUBREDDITS.lock().unwrap().get_mut("demo").unwrap().last_requested_at =
			Instant::now() - std::time::Duration::from_secs(3 * 24 * 60 * 60 + 60);

		crate::post_cache::cleanup_decayed_remembered_subreddits();

		assert!(super::cached_subreddit_metadata("demo").is_none());
		assert_eq!(super::cached_subreddit_nsfw_status("demo"), None);
		assert!(crate::post_cache::get_cached_subreddit_page("demo", 0, crate::post_cache::POSTS_PER_PAGE).is_none());
	}

	#[test]
	fn query_fingerprint_ignores_front_cursor_and_page_tokens() {
		assert_eq!(
			listing_query_fingerprint("sort=hot&front_cursor=abc&after=t3_a&cache_page=1&feed_session=demo&flair=News"),
			listing_query_fingerprint("sort=hot&before=t3_b&cache_page=7&feed_session=other&flair=News")
		);
	}

	#[test]
	fn merge_round_robins_chunks_and_applies_filters() {
		let first = VecDeque::from(vec![
			Post {
				id: "1".to_string(),
				community: "rust".to_string(),
				..test_post()
			},
			Post {
				id: "2".to_string(),
				community: "rust".to_string(),
				..test_post()
			},
		]);
		let second = VecDeque::from(vec![Post {
			id: "3".to_string(),
			community: "programming".to_string(),
			..test_post()
		}]);
		let mut merged = Vec::new();
		let recent_post_ids = HashSet::new();
		let mut seen_post_ids = HashSet::new();
		let mut any_post_after_filter = false;
		let mut chunk_buffers = [first, second];

		let next_source_index = merge_chunk_posts(
			&mut chunk_buffers,
			&mut merged,
			&HashSet::new(),
			"",
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
			0,
		);

		assert_eq!(merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["1", "3", "2"]);
		assert!(any_post_after_filter);
		assert_eq!(next_source_index, 1);

		let mut filtered_merged = Vec::new();
		let filtered_recent_post_ids = HashSet::new();
		let mut filtered_seen_post_ids = HashSet::new();
		let mut filtered_flag = false;
		let filters = HashSet::from(["programming".to_string()]);
		let mut filtered_chunk_buffers = [
			VecDeque::from(vec![
				Post {
					id: "1".to_string(),
					community: "rust".to_string(),
					..test_post()
				},
				Post {
					id: "2".to_string(),
					community: "rust".to_string(),
					..test_post()
				},
			]),
			VecDeque::from(vec![Post {
				id: "3".to_string(),
				community: "programming".to_string(),
				..test_post()
			}]),
		];

		let filtered_next_source_index = merge_chunk_posts(
			&mut filtered_chunk_buffers,
			&mut filtered_merged,
			&filters,
			"",
			&filtered_recent_post_ids,
			&mut filtered_seen_post_ids,
			&mut filtered_flag,
			0,
		);

		assert_eq!(filtered_merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["1", "2"]);
		assert!(filtered_flag);
		assert_eq!(filtered_next_source_index, 1);
	}

	#[test]
	fn merge_skips_duplicate_and_recent_posts() {
		let mut chunk_buffers = [
			VecDeque::from(vec![
				Post {
					id: "1".to_string(),
					community: "rust".to_string(),
					..test_post()
				},
				Post {
					id: "2".to_string(),
					community: "rust".to_string(),
					..test_post()
				},
			]),
			VecDeque::from(vec![
				Post {
					id: "1".to_string(),
					community: "programming".to_string(),
					..test_post()
				},
				Post {
					id: "3".to_string(),
					community: "programming".to_string(),
					..test_post()
				},
			]),
		];
		let recent_post_ids = HashSet::from(["2".to_string()]);
		let mut seen_post_ids = HashSet::new();
		let mut merged = Vec::new();
		let mut any_post_after_filter = false;

		let next_source_index = merge_chunk_posts(
			&mut chunk_buffers,
			&mut merged,
			&HashSet::new(),
			"",
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
			0,
		);

		assert_eq!(merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["1", "3"]);
		assert_eq!(next_source_index, 0);
	}

	#[test]
	fn merge_skips_deleted_placeholder_image_posts_on_front_page() {
		let mut deleted = test_post();
		deleted.id = "1".to_string();
		deleted.media.url = "https://redlib1.bdamokos.org/preview/pre/p1povyvu85vg1.jpg?width=1075&format=pjpg&auto=webp&s=a3314a246d276e09c69b745aff5cb5e9168d2a53".to_string();

		let mut normal = test_post();
		normal.id = "2".to_string();
		normal.media.url = "https://i.redd.it/example.jpg".to_string();

		let mut chunk_buffers = [VecDeque::from(vec![deleted, normal])];
		let recent_post_ids = HashSet::new();
		let mut seen_post_ids = HashSet::new();
		let mut merged = Vec::new();
		let mut any_post_after_filter = false;

		let next_source_index = merge_chunk_posts(
			&mut chunk_buffers,
			&mut merged,
			&HashSet::new(),
			"",
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
			0,
		);

		assert!(any_post_after_filter);
		assert_eq!(merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["2"]);
		assert_eq!(next_source_index, 0);
	}

	#[test]
	fn merge_preserves_round_robin_position_across_refills() {
		let recent_post_ids = HashSet::new();
		let mut seen_post_ids = HashSet::new();
		let mut any_post_after_filter = false;
		let mut first_round_buffers = [
			VecDeque::from(vec![Post {
				id: "1".to_string(),
				community: "user_a".to_string(),
				..test_post()
			}]),
			VecDeque::new(),
		];
		let mut merged = Vec::new();

		let next_source_index = merge_chunk_posts(
			&mut first_round_buffers,
			&mut merged,
			&HashSet::new(),
			"",
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
			0,
		);

		assert_eq!(merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["1"]);
		assert_eq!(next_source_index, 1);

		let mut second_round_buffers = [
			VecDeque::from(vec![Post {
				id: "2".to_string(),
				community: "user_a".to_string(),
				..test_post()
			}]),
			VecDeque::from(vec![Post {
				id: "3".to_string(),
				community: "user_b".to_string(),
				..test_post()
			}]),
		];

		let next_source_index = merge_chunk_posts(
			&mut second_round_buffers,
			&mut merged,
			&HashSet::new(),
			"",
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
			next_source_index,
		);

		assert_eq!(merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["1", "3", "2"]);
		assert_eq!(next_source_index, 1);
	}
}

pub fn quarantine(req: &Request<Body>, sub: String, restriction: &str) -> Response<Body> {
	let wall = WallTemplate {
		title: format!("r/{sub} is {restriction}"),
		msg: "Please click the button below to continue to this subreddit.".to_string(),
		url: req.uri().to_string(),
		sub,
		prefs: Preferences::new(req),
	};

	Response::builder()
		.status(403)
		.header("content-type", "text/html")
		.body(wall.render().unwrap_or_default().into())
		.unwrap_or_default()
}

pub async fn add_quarantine_exception(req: Request<Body>) -> Result<Response<Body>, String> {
	let subreddit = req.param("sub").ok_or("Invalid URL")?;
	let redir = param(&format!("?{}", req.uri().query().unwrap_or_default()), "redir").ok_or("Invalid URL")?;
	let mut response = redirect(&redir);
	response.insert_cookie(
		Cookie::build((&format!("allow_quaran_{}", subreddit.to_lowercase()), "true"))
			.path("/")
			.http_only(true)
			.expires(cookie::Expiration::Session)
			.into(),
	);
	Ok(response)
}

pub fn can_access_quarantine(req: &Request<Body>, sub: &str) -> bool {
	// Determine if the subreddit can be accessed
	setting(req, &format!("allow_quaran_{}", sub.to_lowercase())).parse().unwrap_or_default()
}

// Join items in chunks of 4000 bytes in length for cookies
pub fn join_until_size_limit<T: std::fmt::Display>(vec: &[T]) -> Vec<std::string::String> {
	let mut result = Vec::new();
	let mut list = String::new();
	let mut current_size = 0;

	for item in vec {
		// Size in bytes
		let item_size = item.to_string().len();
		// Use 4000 bytes to leave us some headroom because the name and options of the cookie count towards the 4096 byte cap
		if current_size + item_size > 4000 {
			// If last item add a seperator on the end of the list so it's interpreted properly in tanden with the next cookie
			list.push('+');

			// Push current list to result vector
			result.push(list);

			// Reset the list variable so we can continue with only new items
			list = String::new();
		}
		// Add separator if not the first item
		if !list.is_empty() {
			list.push('+');
		}
		// Add current item to list
		list.push_str(&item.to_string());
		current_size = list.len() + item_size;
	}
	// Make sure to push whatever the remaining subreddits are there into the result vector
	result.push(list);

	// Return resulting vector
	result
}

async fn resolve_subreddit_display_name(part: &str, display_lookup: &[(String, String)]) -> String {
	if part.starts_with("u_") {
		return part.to_string();
	}

	if let Some((_, display)) = display_lookup.iter().find(|entry| entry.0 == part.to_lowercase()) {
		return display.clone();
	}

	match json(format!("/r/{part}/about.json?raw_json=1"), true).await {
		Ok(display) => display["data"]["display_name"].as_str().unwrap_or(part).to_string(),
		Err(_) => part.to_string(),
	}
}

fn canonical_subreddit_parts(query: &str) -> Option<Vec<String>> {
	let canonical = param(&format!("?{query}"), "canonical")?.replace(' ', "+");
	let values = canonical
		.split('+')
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.map(ToString::to_string)
		.collect::<Vec<_>>();
	(!values.is_empty()).then_some(values)
}

pub async fn feed_status_api(req: Request<Body>) -> Result<Response<Body>, String> {
	let session_id = param(&format!("?{}", req.uri().query().unwrap_or_default()), FEED_SESSION_PARAM).unwrap_or_default();
	let page = param(&format!("?{}", req.uri().query().unwrap_or_default()), "cache_page")
		.and_then(|value| value.parse::<usize>().ok())
		.unwrap_or(0);

	let Some(status) = crate::post_cache::feed_status(&session_id, page, crate::post_cache::POSTS_PER_PAGE) else {
		return error(req, "Feed session not found or expired").await;
	};

	let body = serde_json::to_vec(&status).map_err(|error| error.to_string())?;
	Response::builder()
		.status(200)
		.header("content-type", "application/json")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

pub async fn cache_status_page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let status = crate::post_cache::cache_status_for_preferences(&prefs);
	let semantic_database = crate::subreddit_semantics::embedding_database_status();
	let content_semantic_database = crate::subreddit_content_semantics::embedding_database_status();
	let seconds_until_daemon_tick = crate::post_cache::cache_status_seconds_until_daemon_tick();
	let (promotion_filtered_users, redgifs_filtered_users) = crate::promo_detection::viewer_auto_filter_counts(&prefs);
	let mut upcoming_sources = status
		.sources
		.iter()
		.filter(|source| source.schedule_state == "scheduled" || source.schedule_state == "in_flight")
		.cloned()
		.collect::<Vec<_>>();
	upcoming_sources.sort_by(|left, right| {
		left
			.predicted_next_fetch_in_secs
			.unwrap_or(u64::MAX)
			.cmp(&right.predicted_next_fetch_in_secs.unwrap_or(u64::MAX))
			.then_with(|| left.queue_position.unwrap_or(usize::MAX).cmp(&right.queue_position.unwrap_or(usize::MAX)))
			.then_with(|| left.display_name.to_lowercase().cmp(&right.display_name.to_lowercase()))
	});
	upcoming_sources.truncate(5);
	let recent_sources = recent_cached_sources(&status.sources);
	Ok(template(&CacheStatusTemplate {
		prefs,
		url: req.uri().path().to_string(),
		generated_at_unix: status.generated_at_unix,
		summary: status.summary,
		sources: status.sources,
		seconds_until_daemon_tick,
		upcoming_sources,
		recent_sources,
		semantic_database,
		content_semantic_database,
		promotion_filtered_users,
		redgifs_filtered_users,
	}))
}

fn recent_cached_sources(sources: &[crate::post_cache::CacheStatusSource]) -> Vec<crate::post_cache::CacheStatusSource> {
	let mut recent_sources = sources.iter().filter(|source| source.cached_age_secs.is_some()).cloned().collect::<Vec<_>>();
	recent_sources.sort_by(|left, right| {
		left
			.cached_age_secs
			.unwrap_or(u64::MAX)
			.cmp(&right.cached_age_secs.unwrap_or(u64::MAX))
			.then_with(|| left.display_name.to_lowercase().cmp(&right.display_name.to_lowercase()))
	});
	recent_sources.truncate(5);
	recent_sources
}

pub async fn cache_status_api(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let status = crate::post_cache::cache_status_for_preferences(&prefs);
	let body = serde_json::to_vec(&status).map_err(|error| error.to_string())?;
	Response::builder()
		.status(200)
		.header("content-type", "application/json")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

pub async fn feed_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	let session_id = param(&format!("?{}", req.uri().query().unwrap_or_default()), FEED_SESSION_PARAM).unwrap_or_default();
	let page = param(&format!("?{}", req.uri().query().unwrap_or_default()), "cache_page")
		.and_then(|value| value.parse::<usize>().ok())
		.unwrap_or(0);
	crate::post_cache::wait_and_seal_contextual_feed_paint(&session_id, page, crate::post_cache::POSTS_PER_PAGE).await;
	let Some(mut feed_page) = crate::post_cache::feed_page(&session_id, page, crate::post_cache::POSTS_PER_PAGE) else {
		return error(req, "Feed session not found or expired").await;
	};
	let page_ready = feed_page.ready;
	let current_url = param(&format!("?{}", req.uri().query().unwrap_or_default()), "current_url").unwrap_or_else(|| "/".to_string());

	let feed_seen_scope = seen_posts::public_page_is_feed_seen_scope(&current_url);
	let (rl_session_id, needs_rl_session_cookie) = if feed_seen_scope {
		seen_posts::ensure_rl_session_id(&req)
	} else {
		(String::new(), false)
	};
	let (hide_seen_on, hide_seen_cookie_override) = seen_posts::hide_seen_effective_for_request(&req, feed_seen_scope);
	let feed_seen_for_posts = if feed_seen_scope {
		Some(seen_posts::FeedSeenCookieCtx {
			session_id: rl_session_id.clone(),
			hide_seen_on,
			applies: true,
			needs_session_cookie: needs_rl_session_cookie,
			hide_seen_cookie_override: hide_seen_cookie_override.clone(),
		})
	} else {
		None
	};
	let merged_posts = if hide_seen_on { crate::post_cache::feed_merged_posts(&session_id) } else { None };
	let (mut posts, has_next_unseen) = contextual_listing_posts_from_feed_page(feed_seen_for_posts.as_ref(), page, &mut feed_page, merged_posts.as_deref());
	if is_following_route(&current_url) {
		posts = crate::user::hydrate_following_submission_posts(posts).await;
	}
	utils::hydrate_post_author_links_from_cache(&mut posts);

	let prev_page_url = if page > 0 {
		update_url_query(
			&current_url,
			&[
				("cache_page", Some(&(page - 1).to_string())),
				(FEED_SESSION_PARAM, Some(&session_id)),
				("before", None),
				("after", None),
				(FRONT_CURSOR_PARAM, None),
			],
		)
	} else {
		String::new()
	};
	let next_page_url = if feed_page.has_next {
		update_url_query(
			&current_url,
			&[
				("cache_page", Some(&(page + 1).to_string())),
				(FEED_SESSION_PARAM, Some(&session_id)),
				("before", None),
				("after", None),
				(FRONT_CURSOR_PARAM, None),
			],
		)
	} else {
		String::new()
	};
	let next_page_url = if has_next_unseen && next_page_url.is_empty() {
		update_url_query(
			&current_url,
			&[
				("cache_page", Some(&(page + 1).to_string())),
				(FEED_SESSION_PARAM, Some(&session_id)),
				("before", None),
				("after", None),
				(FRONT_CURSOR_PARAM, None),
			],
		)
	} else {
		next_page_url
	};
	let no_posts = page_ready && posts.is_empty();
	let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|post| post.flags.nsfw) && setting(&req, "show_nsfw") != "on");
	let session_sort = crate::post_cache::feed_session_sort(&session_id).map(|sort| sort.as_str().to_string()).unwrap_or_default();
	let contextual_pagination = if page == 0 {
		Vec::new()
	} else {
		build_contextual_pagination(&current_url, &session_id, page, &session_sort)
	};
	let user_agent = req.headers().get("user-agent").and_then(|value| value.to_str().ok()).unwrap_or_default();
	info!(
		"post_cache: feed-fragment current_url={} page={} session={} rendered_posts={} page_ready={} no_posts={} has_next={} ua={}",
		current_url,
		page,
		session_id,
		posts.len(),
		page_ready,
		no_posts,
		feed_page.has_next || has_next_unseen,
		user_agent
	);

	let mut response = render_feed_fragment(FeedFragmentTemplate {
		posts,
		prev_page_url,
		next_page_url,
		prefs: Preferences::new(&req),
		url: current_url,
		all_posts_filtered: false,
		all_posts_hidden_nsfw,
		no_posts,
		show_contextual_pagination: !contextual_pagination.is_empty(),
		contextual_pagination,
	})?;
	if feed_seen_scope {
		seen_posts::attach_rl_feed_cookies(&mut response, &rl_session_id, needs_rl_session_cookie, hide_seen_cookie_override.as_deref());
	}
	Ok(response)
}

// Sub, filter, unfilter, or unsub by setting subscription cookie using response "Set-Cookie" header
pub async fn subscriptions_filters(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_default();
	let action: Vec<String> = req.uri().path().split('/').map(String::from).collect();

	// Handle random subreddits
	if sub == "random" || sub == "randnsfw" {
		if action.contains(&"filter".to_string()) || action.contains(&"unfilter".to_string()) {
			return Err("Can't filter random subreddit!".to_string());
		}
		return Err("Can't subscribe to random subreddit!".to_string());
	}

	let query = req.uri().query().unwrap_or_default().to_string();

	let mut preferences = Preferences::new(&req);
	let mut sub_list = preferences.subscriptions.clone();
	let mut filters = preferences.filters.clone();
	let canonical_parts = canonical_subreddit_parts(&query);

	// Retrieve list of posts for these subreddits to extract display names
	let display_lookup: Vec<(String, String)> = if canonical_parts.is_some() {
		Vec::new()
	} else {
		let posts = json(format!("/r/{sub}/hot.json?raw_json=1"), true).await;
		match &posts {
			Ok(posts) => posts["data"]["children"]
				.as_array()
				.map(|list| {
					list
						.iter()
						.map(|post| {
							let display_name = post["data"]["subreddit"].as_str().unwrap_or_default();
							(display_name.to_lowercase(), display_name.to_string())
						})
						.collect::<Vec<_>>()
				})
				.unwrap_or_default(),
			Err(_) => vec![],
		}
	};

	// Find each subreddit name (separated by '+') in sub parameter
	let parts = canonical_parts.unwrap_or_else(|| sub.split('+').filter(|x| x != &"").map(ToString::to_string).collect());
	for part in parts {
		let part = if display_lookup.is_empty() {
			part
		} else {
			resolve_subreddit_display_name(&part, &display_lookup).await
		};

		// Modify sub list based on action
		if action.contains(&"subscribe".to_string()) && !sub_list.iter().any(|sub| sub.eq_ignore_ascii_case(&part)) {
			// Add each sub name to the subscribed list
			sub_list.push(part.clone());
			filters.retain(|s| s.to_lowercase() != part.to_lowercase());
			// Reorder sub names alphabetically
			sub_list.sort_by_key(|a| a.to_lowercase());
			filters.sort_by_key(|a| a.to_lowercase());
		} else if action.contains(&"unsubscribe".to_string()) {
			// Remove sub name from subscribed list
			sub_list.retain(|s| s.to_lowercase() != part.to_lowercase());
		} else if action.contains(&"filter".to_string()) && !filters.contains(&part.to_owned()) {
			// Add each sub name to the filtered list
			filters.push(part.to_owned());
			sub_list.retain(|s| s.to_lowercase() != part.to_lowercase());
			// Reorder sub names alphabetically
			filters.sort_by_key(|a| a.to_lowercase());
			sub_list.sort_by_key(|a| a.to_lowercase());
		} else if action.contains(&"unfilter".to_string()) {
			// Remove sub name from filtered list
			filters.retain(|s| s.to_lowercase() != part.to_lowercase());
		}
	}

	// Redirect back to subreddit
	// check for redirect parameter if unsubscribing/unfiltering from outside sidebar
	let path = if let Some(redirect_path) = param(&format!("?{query}"), "redirect") {
		format!("/{redirect_path}")
	} else {
		format!("/r/{sub}")
	};

	let mut response = if is_async_subscription_request(&req) {
		async_subscription_ok_response()
	} else {
		redirect(&path)
	};

	preferences.subscriptions = sub_list;
	preferences.filters = filters;
	preferences.prepare_for_runtime();
	prefs_store::save_preferences(&mut response, &req, &preferences)?;

	Ok(response)
}

fn update_redgifs_filter_exemption(preferences: &mut Preferences, subreddit: &str, exempt: bool) {
	let subreddit = subreddit.trim().trim_start_matches("r/").trim_start_matches("/r/").trim_matches('/');
	if exempt {
		if !preferences.has_redgifs_filter_exemption(subreddit) {
			preferences.redgifs_filter_exempt_subreddits.push(subreddit.to_string());
		}
	} else {
		preferences.redgifs_filter_exempt_subreddits.retain(|value| {
			!value
				.trim()
				.trim_start_matches("r/")
				.trim_start_matches("/r/")
				.trim_matches('/')
				.eq_ignore_ascii_case(subreddit)
		});
	}
	preferences.prepare_for_runtime();
}

fn redgifs_filter_exemption_action(path: &str) -> Option<bool> {
	if path.ends_with("/redgifs-exempt") {
		Some(true)
	} else if path.ends_with("/redgifs-unexempt") {
		Some(false)
	} else {
		None
	}
}

/// Toggle the multiple-RedGIF-profile filter exemption directly from a
/// subreddit sidebar. This changes only the scoped exemption list; the stored
/// author filters and all other preferences remain untouched.
pub async fn redgifs_filter_exemption(req: Request<Body>) -> Result<Response<Body>, String> {
	let subreddit = req.param("sub").unwrap_or_default();
	if subreddit.is_empty() || subreddit.contains('+') || matches!(subreddit.to_ascii_lowercase().as_str(), "all" | "popular" | "random" | "randnsfw") {
		return Err("A single subreddit is required for this exemption.".to_string());
	}
	let exempt = redgifs_filter_exemption_action(req.uri().path()).ok_or_else(|| "Invalid RedGIF exemption action.".to_string())?;
	let query = req.uri().query().unwrap_or_default();
	let path = param(&format!("?{query}"), "redirect").map_or_else(|| format!("/r/{subreddit}"), |redirect_path| format!("/{redirect_path}"));
	let mut preferences = Preferences::new(&req);
	update_redgifs_filter_exemption(&mut preferences, &subreddit, exempt);
	let mut response = redirect(&path);
	prefs_store::save_preferences(&mut response, &req, &preferences)?;
	Ok(response)
}

pub async fn flair_subscriptions(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_default();
	let action: Vec<String> = req.uri().path().split('/').map(String::from).collect();
	let query = req.uri().query().unwrap_or_default().to_string();
	let flair = param(&format!("?{query}"), "name").unwrap_or_default().trim().to_string();
	let canonical_subreddit = param(&format!("?{query}"), "canonical").unwrap_or_default().replace(' ', "+");

	if sub == "random" || sub == "randnsfw" {
		return Err("Can't subscribe to random subreddit flair!".to_string());
	}
	if flair.is_empty() {
		return Err("Flair name is required.".to_string());
	}

	let subreddit = if canonical_subreddit.is_empty() {
		resolve_subreddit_display_name(&sub, &[]).await
	} else {
		canonical_subreddit
	};

	let mut preferences = Preferences::new(&req);
	let mut flair_subscriptions = preferences.flair_subscriptions.clone();
	let target = FlairSubscription { subreddit, flair: flair.clone() };

	if action.contains(&"subscribe".to_string()) {
		if !flair_subscriptions.iter().any(|subscription| subscription == &target) {
			flair_subscriptions.push(target.clone());
			flair_subscriptions.sort_by(|left, right| {
				left
					.subreddit
					.to_lowercase()
					.cmp(&right.subreddit.to_lowercase())
					.then_with(|| left.subreddit.cmp(&right.subreddit))
					.then_with(|| left.flair.to_lowercase().cmp(&right.flair.to_lowercase()))
					.then_with(|| left.flair.cmp(&right.flair))
			});
		}
	} else if action.contains(&"unsubscribe".to_string()) {
		flair_subscriptions.retain(|subscription| subscription != &target);
	}

	let path = if let Some(redirect_path) = param(&format!("?{query}"), "redirect") {
		format!("/{redirect_path}")
	} else {
		utils::flair_feed_url(&target.subreddit, &flair)
	};

	let mut response = if is_async_subscription_request(&req) {
		async_subscription_ok_response()
	} else {
		redirect(&path)
	};

	preferences.flair_subscriptions = flair_subscriptions;
	preferences.prepare_for_runtime();
	prefs_store::save_preferences(&mut response, &req, &preferences)?;

	Ok(response)
}

pub async fn wiki(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_else(|| "reddit.com".to_string());
	let url = req.uri().to_string();
	Ok(template(&WikiTemplate {
		sub,
		wiki: String::new(),
		page: req.param("page").unwrap_or_else(|| "index".to_string()),
		prefs: Preferences::new(&req),
		url,
		shell_mode: true,
		fragment_url: wiki_fragment_path(&req),
		loading_message: "Loading wiki page...".to_string(),
	}))
}

pub async fn wiki_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_else(|| "reddit.com".to_string());
	let quarantined = can_access_quarantine(&req, &sub);
	// Handle random subreddits
	if let Ok(random) = catch_random(&sub, "/wiki").await {
		return Ok(random);
	}

	let source_path = wiki_source_path(&req);
	let path: String = format!("{source_path}.json?raw_json=1");

	match json(path, quarantined).await {
		Ok(response) => {
			let body = WikiFragmentTemplate {
				sub,
				wiki: rewrite_urls(response["data"]["content_html"].as_str().unwrap_or("<h3>Wiki not found</h3>")),
			}
			.render()
			.map_err(|error| error.to_string())?;
			Response::builder()
				.status(200)
				.header("content-type", "text/html; charset=utf-8")
				.header("cache-control", "no-store")
				.body(Body::from(body))
				.map_err(|error| error.to_string())
		}
		Err(msg) => {
			if msg == "quarantined" || msg == "gated" {
				Ok(quarantine(&req, sub, &msg))
			} else {
				error(req, &msg).await
			}
		}
	}
}

pub async fn sidebar(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_else(|| "reddit.com".to_string());
	let url = req.uri().to_string();
	Ok(template(&WikiTemplate {
		sub,
		wiki: String::new(),
		page: "Sidebar".to_string(),
		prefs: Preferences::new(&req),
		url,
		shell_mode: true,
		fragment_url: wiki_fragment_path(&req),
		loading_message: "Loading sidebar...".to_string(),
	}))
}

pub async fn sidebar_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_else(|| "reddit.com".to_string());
	let quarantined = can_access_quarantine(&req, &sub);

	// Handle random subreddits
	if let Ok(random) = catch_random(&sub, "/about/sidebar").await {
		return Ok(random);
	}

	// Build the Reddit JSON API url
	let path: String = format!("/r/{sub}/about.json?raw_json=1");
	// Send a request to the url
	match json(path, quarantined).await {
		// If success, receive JSON in response
		Ok(response) => {
			let body = WikiFragmentTemplate {
				wiki: rewrite_urls(&val(&response, "description_html")),
				// wiki: format!(
				// 	"{}<hr><h1>Moderators</h1><br><ul>{}</ul>",
				// 	rewrite_urls(&val(&response, "description_html"),
				// 	moderators(&sub, quarantined).await.unwrap_or(vec!["Could not fetch moderators".to_string()]).join(""),
				// ),
				sub,
			}
			.render()
			.map_err(|error| error.to_string())?;
			Response::builder()
				.status(200)
				.header("content-type", "text/html; charset=utf-8")
				.header("cache-control", "no-store")
				.body(Body::from(body))
				.map_err(|error| error.to_string())
		}
		Err(msg) => {
			if msg == "quarantined" || msg == "gated" {
				Ok(quarantine(&req, sub, &msg))
			} else {
				error(req, &msg).await
			}
		}
	}
}

// pub async fn moderators(sub: &str, quarantined: bool) -> Result<Vec<String>, String> {
// 	// Retrieve and format the html for the moderators list
// 	Ok(
// 		moderators_list(sub, quarantined)
// 			.await?
// 			.iter()
// 			.map(|m| format!("<li><a style=\"color: var(--accent)\" href=\"/u/{name}\">{name}</a></li>", name = m))
// 			.collect(),
// 	)
// }

// async fn moderators_list(sub: &str, quarantined: bool) -> Result<Vec<String>, String> {
// 	// Build the moderator list URL
// 	let path: String = format!("/r/{}/about/moderators.json?raw_json=1", sub);

// 	// Retrieve response
// 	json(path, quarantined).await.map(|response| {
// 		// Traverse json tree and format into list of strings
// 		response["data"]["children"]
// 			.as_array()
// 			.unwrap_or(&Vec::new())
// 			.iter()
// 			.filter_map(|moderator| {
// 				let name = moderator["name"].as_str().unwrap_or_default();
// 				if name.is_empty() {
// 					None
// 				} else {
// 					Some(name.to_string())
// 				}
// 			})
// 			.collect::<Vec<_>>()
// 	})
// }

// SUBREDDIT
pub async fn refresh_subreddit_metadata(sub: &str, quarantined: bool) -> Result<Subreddit, String> {
	// Build the Reddit JSON API url
	let path: String = format!("/r/{sub}/about.json?raw_json=1");

	// Send a request to the url
	let res = json(path, quarantined).await?;

	// Metadata regarding the subreddit
	let members: i64 = res["data"]["subscribers"].as_u64().unwrap_or_default() as i64;
	let active: i64 = res["data"]["accounts_active"].as_u64().unwrap_or_default() as i64;

	// Fetch subreddit icon either from the community_icon or icon_img value
	let community_icon: &str = res["data"]["community_icon"].as_str().unwrap_or_default();
	let icon = if community_icon.is_empty() { val(&res, "icon_img") } else { community_icon.to_string() };
	let info = rewrite_urls(&val(&res, "description_html"));

	let subreddit = Subreddit {
		name: val(&res, "display_name"),
		title: val(&res, "title"),
		description: val(&res, "public_description"),
		has_sidebar: subreddit_sidebar_has_content(&info),
		info,
		icon: format_url(&icon),
		members: format_num(members),
		active: format_num(active),
		wiki: res["data"]["wiki_enabled"].as_bool().unwrap_or_default(),
		nsfw: res["data"]["over18"].as_bool().unwrap_or_default(),
	};
	store_subreddit_metadata(&subreddit);
	Ok(subreddit)
}

async fn subreddit(sub: &str, quarantined: bool) -> Result<Subreddit, String> {
	if let Some(cached) = cached_subreddit_metadata(sub) {
		return Ok(cached);
	}

	refresh_subreddit_metadata(sub, quarantined).await
}

fn subreddit_sidebar_has_content(info: &str) -> bool {
	let text = HTML_TAG_REGEX.replace_all(info, "");
	!decode_html(&text).unwrap_or_else(|_| text.into_owned()).trim().is_empty()
}

pub async fn rss(req: Request<Body>) -> Result<Response<Body>, String> {
	if config::get_setting("REDLIB_ENABLE_RSS").is_none() {
		return Ok(error(req, "RSS is disabled on this instance.").await.unwrap_or_default());
	}

	use hyper::header::CONTENT_TYPE;
	use rss::{ChannelBuilder, Item};

	// Get subreddit
	let sub = req.param("sub").unwrap_or_default();
	let post_sort = req.cookie("post_sort").map_or_else(|| "hot".to_string(), |c| c.value().to_string());
	let sort = req.param("sort").unwrap_or_else(|| req.param("id").unwrap_or(post_sort));

	// Get path
	let path = format!("/r/{sub}/{sort}.json?{}", req.uri().query().unwrap_or_default());

	// Get subreddit data
	let subreddit = subreddit(&sub, false).await?;

	// Get posts
	let (posts, _) = Post::fetch(&path, false).await?;

	// Build the RSS feed
	let channel = ChannelBuilder::default()
		.title(&subreddit.title)
		.description(&subreddit.description)
		.items(
			posts
				.into_iter()
				.map(|post| Item {
					title: Some(post.title.to_string()),
					link: Some(format_url(&utils::get_post_url(&post))),
					author: Some(post.author.name),
					content: Some(rewrite_urls(&decode_html(&post.body).unwrap())),
					pub_date: Some(DateTime::from_timestamp(post.created_ts as i64, 0).unwrap_or_default().to_rfc2822()),
					description: Some(format!(
						"<a href='{}{}'>Comments</a>",
						config::get_setting("REDLIB_FULL_URL").unwrap_or_default(),
						post.permalink
					)),
					..Default::default()
				})
				.collect::<Vec<_>>(),
		)
		.build();

	// Serialize the feed to RSS
	let body = channel.to_string().into_bytes();

	// Create the HTTP response
	let mut res = Response::new(Body::from(body));
	res.headers_mut().insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/rss+xml"));

	Ok(res)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fetching_subreddit() {
	let subreddit = subreddit("rust", false).await;
	assert!(subreddit.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gated_and_quarantined() {
	let quarantined = subreddit("edgy", true).await;
	assert!(quarantined.is_ok());
	let gated = subreddit("drugs", true).await;
	assert!(gated.is_ok());
}
