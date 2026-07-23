#![allow(clippy::cmp_owned)]

// CRATES
use crate::client::json;
use crate::instance_scrape;
use crate::redgifs;
use crate::server::RequestExt;
use crate::utils::{error, filter_posts, format_num, format_url, get_filters, param, setting, template, FollowingComment, ParentComment, Post, Preferences, User};
use crate::{config, peer_api_fallback, utils, video_mode};
use askama::Template;
use chrono::DateTime;
use htmlescape::decode_html;
use hyper::{Body, Request, Response};
use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration as StdDuration, Instant};
use time::{macros::format_description, OffsetDateTime};
use tokio::task::JoinSet;
use url::form_urlencoded;
use url::Url;

const TARGET_POST_COUNT: usize = 25;
const MAX_DUPLICATE_FILL_REQUESTS: usize = 4;
const MAX_FILTERED_PAGE_ADVANCE: usize = 3;
const USER_SORT_OPTIONS: [&str; 4] = ["hot", "new", "top", "controversial"];
const USER_METADATA_CACHE_SIZE: usize = 1000;
const USER_METADATA_CACHE_TTL: StdDuration = StdDuration::from_secs(10_800);
const USER_VIDEO_STATS_CACHE_SIZE: usize = 1000;
const USER_VIDEO_STATS_CACHE_TTL: StdDuration = StdDuration::from_secs(10_800);
const USER_CACHE_MATERIALIZED_POST_LIMIT: usize = 160;
const USER_CACHE_SUBREDDIT_SOURCE_LIMIT: usize = 64;
const FOLLOWING_PARENT_COMMENT_BATCH_SIZE: usize = 100;
const FOLLOWING_PARENT_COMMENT_EXPAND_THRESHOLD: usize = 320;

#[derive(Clone)]
struct CachedUserMetadata {
	user: User,
	cached_at: Instant,
}

#[derive(Clone)]
struct CachedUserVideoStats {
	stats: UserVideoStatsAccumulator,
	cached_at: Instant,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct UserVideoStats {
	average_duration_seconds: i64,
	video_count: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserVideoSignal {
	pub avg_video_length: Option<(String, String)>,
	pub video_count: usize,
	pub redgifs_profile_count: usize,
	pub redgifs_usernames: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct UserVideoStatsAccumulator {
	video_durations: HashMap<String, i64>,
	seen_redgifs_users: HashSet<String>,
}

impl UserVideoStatsAccumulator {
	pub(crate) fn observe_post(&mut self, post: &Post) {
		let redgifs_username = if matches!(post.post_type.as_str(), "video" | "gif") {
			post.media.redgifs_username.as_str()
		} else {
			""
		};
		self.observe_video_signal(video_aggregate_key(post), post.media.duration_seconds, redgifs_username);
	}

	pub(crate) fn observe_video_signal(&mut self, video_key: Option<String>, duration_seconds: i64, redgifs_username: &str) {
		if !redgifs_username.is_empty() {
			let slug = utils::normalize_creator_identity_slug(redgifs_username);
			if !slug.is_empty() {
				self.seen_redgifs_users.insert(slug);
			}
		}

		let Some(video_key) = video_key else {
			return;
		};
		self.video_durations.entry(video_key).or_insert(duration_seconds);
	}

	pub(crate) fn merge(&mut self, other: Self) {
		for (video_key, duration_seconds) in other.video_durations {
			self.video_durations.entry(video_key).or_insert(duration_seconds);
		}
		self.seen_redgifs_users.extend(other.seen_redgifs_users);
	}

	fn video_count(&self) -> usize {
		self.video_durations.len()
	}

	fn redgifs_profile_count(&self) -> usize {
		self.seen_redgifs_users.len()
	}

	pub(crate) fn redgifs_usernames(&self) -> Vec<String> {
		let mut usernames = self.seen_redgifs_users.iter().cloned().collect::<Vec<_>>();
		usernames.sort();
		usernames
	}

	fn total_duration_seconds(&self) -> i64 {
		self.video_durations.values().sum()
	}

	fn average_duration_seconds(&self) -> Option<i64> {
		let video_count = self.video_count();
		if video_count == 0 {
			return None;
		}
		Some(self.total_duration_seconds() / video_count as i64)
	}

	pub(crate) fn avg_video_length(&self) -> Option<(String, String)> {
		let average = self.average_duration_seconds()?;
		let video_count = self.video_count();
		let count_label = if video_count == 1 { "video" } else { "videos" };
		let mut meta = format!("from {} seen {count_label}", video_count);
		let rg_profiles = self.seen_redgifs_users.len();
		if rg_profiles > 1 {
			meta.push_str(&format!(" · {rg_profiles} rg profiles"));
		}
		Some((utils::format_video_duration(average), meta))
	}
}

static USER_VIDEO_STATS_CACHE: LazyLock<Mutex<HashMap<String, CachedUserVideoStats>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static USER_METADATA_CACHE: LazyLock<Mutex<HashMap<String, CachedUserMetadata>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

// STRUCTS
#[derive(Template)]
#[template(path = "user.html")]
pub struct UserTemplate {
	user: User,
	username: String,
	posts: Vec<Post>,
	cached_post_ids: Vec<String>,
	user_subreddits: Vec<UserSubredditEntry>,
	coomer_recent_posts: Option<utils::CoomerRecentPostsPanel>,
	fallback_notice: String,
	sort: (String, String),
	ends: (String, String),
	/// "overview", "comments", or "submitted"
	listing: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	/// Whether the user themself is filtered.
	is_filtered: bool,
	/// Whether all fetched posts are filtered (to differentiate between no posts fetched in the first place,
	/// and all fetched posts being filtered).
	all_posts_filtered: bool,
	/// Whether all posts were hidden because they are NSFW (and user has disabled show NSFW)
	all_posts_hidden_nsfw: bool,
	avg_video_length: Option<(String, String)>,
	no_posts: bool,
	shell_mode: bool,
	fragment_url: String,
	replace_selector: String,
	prev_page_url: String,
	next_page_url: String,
	loading_message: String,
	video_mode_enabled: bool,
	video_mode_sidebar: video_mode::VideoModeSidebarData,
}

crate::impl_template_str_eq!(UserTemplate);

#[derive(Template)]
#[template(path = "user_fragment.html")]
struct UserFragmentTemplate {
	user: User,
	posts: Vec<Post>,
	cached_post_ids: Vec<String>,
	user_subreddits: Vec<UserSubredditEntry>,
	coomer_recent_posts: Option<utils::CoomerRecentPostsPanel>,
	fallback_notice: String,
	sort: (String, String),
	ends: (String, String),
	listing: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	is_filtered: bool,
	all_posts_filtered: bool,
	all_posts_hidden_nsfw: bool,
	avg_video_length: Option<(String, String)>,
	no_posts: bool,
	prev_page_url: String,
	next_page_url: String,
	video_mode_enabled: bool,
	video_mode_sidebar: video_mode::VideoModeSidebarData,
}

#[derive(Template)]
#[template(path = "user_sidebar_fragment.html")]
struct UserSidebarTemplate {
	user: User,
	user_subreddits: Vec<UserSubredditEntry>,
	coomer_recent_posts: Option<utils::CoomerRecentPostsPanel>,
	prefs: Preferences,
	redirect_url: String,
	is_filtered: bool,
	avg_video_length: Option<(String, String)>,
	video_mode_enabled: bool,
	video_mode_sidebar: video_mode::VideoModeSidebarData,
}

impl UserFragmentTemplate {
	pub fn str_eq(&self, x: &str, y: &str) -> bool {
		x == y
	}

	pub fn post_from_cache(&self, post: &Post) -> bool {
		cached_post_ids_contains(&self.cached_post_ids, post)
	}

	pub fn show_successful_empty_state(&self) -> bool {
		show_successful_empty_state(self.no_posts, self.all_posts_filtered, &self.fallback_notice)
	}
}

impl UserTemplate {
	pub fn post_from_cache(&self, post: &Post) -> bool {
		cached_post_ids_contains(&self.cached_post_ids, post)
	}

	pub fn show_successful_empty_state(&self) -> bool {
		show_successful_empty_state(self.no_posts, self.all_posts_filtered, &self.fallback_notice)
	}
}

fn show_successful_empty_state(no_posts: bool, all_posts_filtered: bool, fallback_notice: &str) -> bool {
	no_posts && !all_posts_filtered && fallback_notice.is_empty()
}

#[derive(PartialEq)]
struct UserSubredditEntry {
	name: String,
	count: u32,
}

fn user_sidebar_subreddits(username: &str) -> Vec<UserSubredditEntry> {
	let mut counts: HashMap<String, (String, u32)> = HashMap::new();

	let mut merge_count = |name: String, count: u32| {
		let trimmed = name.trim();
		if trimmed.is_empty() || trimmed.starts_with("u_") || count == 0 {
			return;
		}

		let normalized = trimmed.to_lowercase();
		let entry = counts.entry(normalized).or_insert_with(|| (trimmed.to_string(), 0));
		if entry.0 == entry.0.to_lowercase() && trimmed != trimmed.to_lowercase() {
			entry.0 = trimmed.to_string();
		}
		entry.1 = entry.1.max(count);
	};

	if let Some(stats) = crate::materialized_catalog::catalog_user_sidebar_stats(username) {
		for (name, count) in stats.subreddit_counts {
			merge_count(name, count);
		}
	}

	for (name, count) in utils::cached_user_subreddits(username) {
		merge_count(name, count);
	}

	let mut subreddits = counts.into_values().map(|(name, count)| UserSubredditEntry { name, count }).collect::<Vec<_>>();
	subreddits.sort_by(|left, right| {
		right
			.count
			.cmp(&left.count)
			.then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
			.then_with(|| left.name.cmp(&right.name))
	});
	subreddits
}

fn link_post_id_from_listing(post: &Post) -> Option<String> {
	let lid = post.link_id.trim();
	lid.strip_prefix("t3_").map(|id| id.to_string())
}

pub(crate) fn following_comment_from_listing_post(post: Post) -> FollowingComment {
	FollowingComment {
		id: post.id,
		author: post.author.name,
		body: post.body,
		permalink: post.permalink,
		created_ts: post.created_ts,
		rel_time: post.rel_time,
		community: post.community,
		score: post.score,
		parent_id: post.parent_id,
		parent_kind: post.parent_kind,
		parent_comment: post.parent_comment,
		link_title: post.link_title,
		link_permalink: post.link_permalink,
		link_author: post.link_author,
		link_created_ts: post.link_created_ts,
		thread_comments: post.comments,
		thread_nsfw: post.flags.nsfw,
	}
}

fn removed_parent_comment_html() -> String {
	"<div class=\"md\"><p>[removed]</p></div>".to_string()
}

fn parent_comment_preview(parent: &serde_json::Value) -> Option<ParentComment> {
	if parent["kind"].as_str().unwrap_or_default() != "t1" {
		return None;
	}

	let data = &parent["data"];
	let body_text = crate::utils::val(parent, "body");
	let body = if (crate::utils::val(parent, "author") == "[deleted]" && body_text == "[removed]") || body_text == "[ Removed by Reddit ]" {
		removed_parent_comment_html()
	} else {
		crate::utils::rewrite_emotes(&data["media_metadata"], crate::utils::val(parent, "body_html"))
	};

	Some(ParentComment {
		id: crate::utils::val(parent, "id"),
		body: if body.is_empty() { removed_parent_comment_html() } else { body },
		permalink: crate::utils::val(parent, "permalink"),
		author: crate::utils::val(parent, "author"),
		rel_time: crate::utils::time(data["created_utc"].as_f64().unwrap_or_default()).0,
		expand: false,
	})
}

async fn fetch_parent_comment_lookup(parent_ids: &[String]) -> HashMap<String, ParentComment> {
	let mut parents = HashMap::new();

	for chunk in parent_ids.chunks(FOLLOWING_PARENT_COMMENT_BATCH_SIZE) {
		let ids = chunk.iter().map(|id| format!("t1_{id}")).collect::<Vec<_>>().join(",");
		let path = format!("/api/info.json?raw_json=1&id={ids}");
		let Ok(response) = json(path, false).await else {
			continue;
		};
		let Some(children) = response["data"]["children"].as_array() else {
			continue;
		};
		for parent in children {
			let Some(mut preview) = parent_comment_preview(parent) else {
				continue;
			};
			preview.expand = preview.body.len() > FOLLOWING_PARENT_COMMENT_EXPAND_THRESHOLD;
			parents.insert(preview.id.clone(), preview);
		}
	}

	parents
}

async fn fetch_following_link_post_lookup(post_ids: &[String]) -> HashMap<String, Post> {
	let mut linked_posts = HashMap::new();

	for chunk in post_ids.chunks(FOLLOWING_PARENT_COMMENT_BATCH_SIZE) {
		let ids = chunk.iter().map(|id| format!("t3_{id}")).collect::<Vec<_>>().join(",");
		let path = format!("/api/info.json?raw_json=1&id={ids}");
		let Ok(response) = json(path, false).await else {
			continue;
		};
		let Some(children) = response["data"]["children"].as_array() else {
			continue;
		};
		for child in children {
			if child["kind"].as_str().unwrap_or_default() != "t3" {
				continue;
			}
			let post = crate::utils::parse_post(child).await;
			linked_posts.insert(post.id.clone(), post);
		}
	}

	linked_posts
}

fn append_missing_following_link_posts(mut posts: Vec<Post>, mut linked_posts: HashMap<String, Post>) -> Vec<Post> {
	let mut existing_post_ids = posts.iter().filter(|post| post.kind == "t3").map(|post| post.id.clone()).collect::<HashSet<_>>();
	let linked_post_ids = posts
		.iter()
		.filter(|post| post.kind == "t1" && post.post_type == "comment")
		.filter_map(link_post_id_from_listing)
		.collect::<Vec<_>>();

	for post_id in linked_post_ids {
		if existing_post_ids.contains(&post_id) {
			continue;
		}
		let Some(post) = linked_posts.remove(&post_id) else {
			continue;
		};
		existing_post_ids.insert(post_id);
		posts.push(post);
	}

	posts
}

/// Bundled `/following` row with only comment metadata (no submission body/media loaded yet).
pub(crate) fn following_submission_needs_content_fetch(post: &Post) -> bool {
	post.kind == "t3"
		&& !post.following_comments.is_empty()
		&& post.preview_body.is_empty()
		&& post.body.is_empty()
		&& post.media.url.is_empty()
		&& post.gallery.is_empty()
		&& post.thumbnail.url.is_empty()
}

fn merge_full_submission_into_following_bundle(shell: &mut Post, full: Post) {
	let following_comments = std::mem::take(&mut shell.following_comments);
	let bookmarked = shell.bookmarked;
	let bookmark_new_comments = shell.bookmark_new_comments;
	*shell = full;
	shell.following_comments = following_comments;
	shell.bookmarked = bookmarked;
	shell.bookmark_new_comments = bookmark_new_comments;
}

/// Loads full thread submissions for synthetic bundled posts (and any shell missing media/body).
pub(crate) async fn hydrate_following_submission_posts(mut posts: Vec<Post>) -> Vec<Post> {
	let ids: Vec<String> = posts
		.iter()
		.filter(|p| following_submission_needs_content_fetch(p))
		.map(|p| p.id.clone())
		.collect::<HashSet<_>>()
		.into_iter()
		.collect();
	if ids.is_empty() {
		return posts;
	}
	let lookup = fetch_following_link_post_lookup(&ids).await;
	if lookup.is_empty() {
		return posts;
	}
	for post in &mut posts {
		if !following_submission_needs_content_fetch(post) {
			continue;
		}
		let Some(full) = lookup.get(&post.id).cloned() else {
			continue;
		};
		merge_full_submission_into_following_bundle(post, full);
	}
	posts
}

pub(crate) async fn hydrate_following_comment_context(mut posts: Vec<Post>) -> Vec<Post> {
	let parent_ids = posts
		.iter()
		.filter(|post| post.kind == "t1" && post.parent_kind == "t1" && !post.parent_id.is_empty())
		.map(|post| post.parent_id.clone())
		.collect::<HashSet<_>>()
		.into_iter()
		.collect::<Vec<_>>();

	if parent_ids.is_empty() {
		return posts;
	}

	let parent_lookup = fetch_parent_comment_lookup(&parent_ids).await;
	if parent_lookup.is_empty() {
		return posts;
	}

	for post in &mut posts {
		if post.kind != "t1" || post.parent_kind != "t1" {
			continue;
		}
		let Some(parent) = parent_lookup.get(&post.parent_id) else {
			continue;
		};
		post.parent_comment = parent.clone();
	}

	let linked_post_ids = posts
		.iter()
		.filter(|post| post.kind == "t1" && post.post_type == "comment")
		.filter_map(link_post_id_from_listing)
		.collect::<HashSet<_>>()
		.into_iter()
		.collect::<Vec<_>>();
	if linked_post_ids.is_empty() {
		return posts;
	}

	let linked_posts = fetch_following_link_post_lookup(&linked_post_ids).await;
	if linked_posts.is_empty() {
		return posts;
	}

	append_missing_following_link_posts(posts, linked_posts)
}

pub(crate) fn dedupe_and_sort_following_comments(comments: &mut Vec<FollowingComment>) {
	let mut seen = HashSet::new();
	comments.retain(|c| seen.insert(c.id.clone()));
	comments.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
}

pub(crate) fn merge_following_comments_into(primary: &mut Post, mut extra: Vec<FollowingComment>) {
	if extra.is_empty() {
		return;
	}
	primary.following_comments.append(&mut extra);
	dedupe_and_sort_following_comments(&mut primary.following_comments);
}

pub fn post_feed_activity_ts(post: &Post) -> u64 {
	let mut ts = post.created_ts;
	for c in &post.following_comments {
		ts = ts.max(c.created_ts);
	}
	ts
}

fn synthetic_post_from_orphan_bundle(post_id: String, mut comments: Vec<FollowingComment>) -> Option<Post> {
	if comments.is_empty() {
		return None;
	}
	dedupe_and_sort_following_comments(&mut comments);
	let first = comments.first()?;
	let nsfw = first.thread_nsfw;
	let mut post = Post::default();
	post.id = post_id;
	post.kind = "t3".into();
	post.title = first.link_title.clone();
	post.community = first.community.clone();
	post.author.name = first.link_author.clone();
	post.link_author = first.link_author.clone();
	post.permalink = crate::utils::listing_link_permalink(&first.link_permalink, &first.permalink);
	post.link_permalink = first.link_permalink.clone();
	post.link_title = first.link_title.clone();
	post.comments = first.thread_comments.clone();
	post.nsfw = nsfw;
	post.flags.nsfw = nsfw;
	post.flags.nsfw_str = if nsfw { "true" } else { "false" }.into();
	post.following_comments = comments;
	let link_ts = post.following_comments.iter().map(|c| c.link_created_ts).find(|&ts| ts > 0).unwrap_or(0);
	if link_ts > 0 {
		let (rel_time, created) = crate::utils::time(link_ts as f64);
		post.rel_time = rel_time;
		post.created = created;
		post.created_ts = link_ts;
		post.link_created_ts = link_ts;
	}
	Some(post)
}

pub(crate) fn bundle_followed_user_comments(posts: Vec<Post>) -> Vec<Post> {
	let mut comment_rows: Vec<Post> = Vec::new();
	let mut t3_rows: Vec<Post> = Vec::new();
	for p in posts {
		if p.kind == "t1" && p.post_type == "comment" {
			comment_rows.push(p);
		} else {
			t3_rows.push(p);
		}
	}
	let mut by_post: HashMap<String, Vec<FollowingComment>> = HashMap::new();
	for c in comment_rows {
		let Some(pid) = link_post_id_from_listing(&c) else {
			continue;
		};
		by_post.entry(pid).or_default().push(following_comment_from_listing_post(c));
	}
	for v in by_post.values_mut() {
		dedupe_and_sort_following_comments(v);
	}
	for post in &mut t3_rows {
		if let Some(list) = by_post.remove(&post.id) {
			post.following_comments = list;
		}
	}
	let mut out = t3_rows;
	for (pid, comments) in by_post {
		if let Some(synth) = synthetic_post_from_orphan_bundle(pid, comments) {
			out.push(synth);
		}
	}
	out
}

pub(crate) fn collapse_duplicate_submissions(posts: Vec<Post>) -> Vec<Post> {
	let mut collapsed: Vec<Post> = Vec::new();
	let mut seen = HashMap::new();

	for post in posts {
		let keys = duplicate_submission_keys(&post);
		if keys.is_empty() {
			collapsed.push(post);
			continue;
		}

		if let Some(index) = keys.iter().find_map(|key| seen.get(key).copied()) {
			let primary: &mut Post = &mut collapsed[index];
			if primary.body.is_empty() && primary.preview_body.is_empty() && !post.body.is_empty() {
				primary.preview_body = post.body.clone();
			}
			let mut merged = post;
			merge_following_comments_into(primary, std::mem::take(&mut merged.following_comments));
			primary.collapsed_duplicates.push(collapsed_duplicate(merged));
			for key in keys {
				seen.entry(key).or_insert(index);
			}
			continue;
		}

		let index = collapsed.len();
		for key in keys {
			seen.insert(key, index);
		}
		collapsed.push(post);
	}

	collapsed
}

fn push_normalized_duplicate_key(keys: &mut Vec<String>, prefix: &str, url: &str) {
	let normalized = normalize_duplicate_url(url);
	if !normalized.is_empty() {
		keys.push(format!("{prefix}:{normalized}"));
	}
}

fn duplicate_submission_keys(post: &Post) -> Vec<String> {
	let mut keys = Vec::new();
	match post.post_type.as_str() {
		"gallery" if !post.gallery.is_empty() => {
			let url_parts = post.gallery.iter().map(|image| normalize_duplicate_url(&image.url)).collect::<Vec<_>>();
			let url_key = url_parts.join("|");
			if url_parts.iter().all(|part| !part.is_empty()) {
				keys.push(format!("gallery:{url_key}"));
			}
			let preview_parts = post.gallery.iter().map(|image| normalize_duplicate_url(&image.preview_url)).collect::<Vec<_>>();
			let preview_key = preview_parts.join("|");
			if preview_parts.iter().all(|part| !part.is_empty()) && preview_key != url_key {
				keys.push(format!("gallery-preview:{preview_key}"));
			}
		}
		"image" => {
			push_normalized_duplicate_key(&mut keys, "image", &post.media.url);
			push_normalized_duplicate_key(&mut keys, "image-preview", &post.media.preview_url);
			push_normalized_duplicate_key(&mut keys, "image-thumbnail", &post.thumbnail.url);
			if !post.media.content_fingerprint.is_empty() {
				keys.push(format!("image-content:{}", post.media.content_fingerprint));
			}
		}
		"gif" => {
			if let Some(key) = duplicate_gif_key(post) {
				keys.push(key);
			}
			if !post.media.content_fingerprint.is_empty() {
				keys.push(format!("media-content:{}", post.media.content_fingerprint));
			}
		}
		"video" => {
			if redgifs::is_redgifs_domain(&post.domain) {
				if let Some(key) = duplicate_gif_key(post) {
					keys.push(key);
				}
			} else {
				push_normalized_duplicate_key(&mut keys, "video", &post.media.url);
				if let Some(embed_src) = extract_embed_src(&post.media.embed_code) {
					push_normalized_duplicate_key(&mut keys, "video", &embed_src);
				}
			}
			if !post.media.content_fingerprint.is_empty() {
				keys.push(format!("media-content:{}", post.media.content_fingerprint));
			}
		}
		_ => {}
	}
	keys
}

pub(crate) fn post_merge_keys_attr(post: &Post) -> String {
	let mut keys = Vec::new();
	let identity = cached_post_identity(post);
	if !identity.is_empty() {
		keys.push(identity);
	}
	keys.extend(duplicate_submission_keys(post));
	keys.join("\t")
}

fn duplicate_gif_key(post: &Post) -> Option<String> {
	if let Some(embed_src) = extract_embed_src(&post.media.embed_code) {
		return Some(format!("gif:{}", normalize_duplicate_url(&embed_src)));
	}

	if !post.media.url.is_empty() {
		return Some(format!("gif:{}", normalize_duplicate_url(&post.media.url)));
	}

	None
}

pub(crate) fn video_aggregate_key(post: &Post) -> Option<String> {
	if post.media.duration_seconds <= 0 {
		return None;
	}

	if matches!(post.post_type.as_str(), "gif" | "video") && !post.media.content_fingerprint.is_empty() {
		return Some(format!("media-content:{}", post.media.content_fingerprint));
	}

	let redgifs_video_id = post
		.media
		.url
		.trim()
		.strip_prefix("/redgifs/")
		.map(|value| value.trim_matches('/'))
		.filter(|value| !value.is_empty())
		.map(str::to_ascii_lowercase)
		.or_else(|| redgifs::watch_path_from_url(&post.media.url))
		.or_else(|| redgifs::watch_path_from_embed_code(&post.media.embed_code));

	match post.post_type.as_str() {
		"gif" => redgifs_video_id.map(|video_id| format!("redgifs:{video_id}")).or_else(|| duplicate_gif_key(post)),
		"video" => {
			if redgifs::is_redgifs_domain(&post.domain) || post.media.url.starts_with("/redgifs/") || redgifs_video_id.is_some() {
				redgifs_video_id.map(|video_id| format!("redgifs:{video_id}")).or_else(|| duplicate_gif_key(post))
			} else if !post.media.url.is_empty() {
				Some(format!("video:{}", normalize_duplicate_url(&post.media.url)))
			} else {
				extract_embed_src(&post.media.embed_code).map(|src| format!("video:{}", normalize_duplicate_url(&src)))
			}
		}
		_ => None,
	}
}

fn extract_embed_src(embed_code: &str) -> Option<String> {
	for delimiter in ['"', '\''] {
		let needle = format!("src={delimiter}");
		let Some(start) = embed_code.find(&needle) else {
			continue;
		};

		let remainder = &embed_code[start + needle.len()..];
		let Some((src, _)) = remainder.split_once(delimiter) else {
			continue;
		};

		if !src.trim().is_empty() {
			return Some(src.trim().to_string());
		}
	}

	None
}

fn normalize_duplicate_url(url: &str) -> String {
	let trimmed = url.trim();
	if trimmed.is_empty() {
		return String::new();
	}

	if trimmed.starts_with('/') {
		return trimmed.split('?').next().unwrap_or(trimmed).trim_end_matches('/').to_string();
	}

	let Ok(mut parsed) = Url::parse(trimmed) else {
		return trimmed.trim_end_matches('/').to_string();
	};

	parsed.set_fragment(None);
	if matches!(parsed.domain(), Some("i.redd.it" | "preview.redd.it" | "external-preview.redd.it" | "v.redd.it")) {
		parsed.set_query(None);
	}

	let path = parsed.path().trim_end_matches('/').to_string();
	if !path.is_empty() {
		parsed.set_path(&path);
	}

	parsed.into()
}

fn normalize_user_video_stats_cache_key(username: &str) -> String {
	username.trim().to_lowercase()
}

fn normalize_user_metadata_cache_key(username: &str) -> String {
	username.trim().to_lowercase()
}

fn prune_user_metadata_cache(cache: &mut HashMap<String, CachedUserMetadata>) {
	let now = Instant::now();
	cache.retain(|_, entry| now.duration_since(entry.cached_at) <= USER_METADATA_CACHE_TTL);
}

fn cached_user_metadata(username: &str) -> Option<User> {
	let cache_key = normalize_user_metadata_cache_key(username);
	if cache_key.is_empty() {
		return None;
	}

	let Ok(mut cache) = USER_METADATA_CACHE.lock() else {
		return None;
	};
	prune_user_metadata_cache(&mut cache);
	cache.get(&cache_key).map(|entry| entry.user.clone())
}

pub(crate) fn store_user_metadata(username: &str, user: &User) {
	let cache_key = normalize_user_metadata_cache_key(username);
	if cache_key.is_empty() {
		return;
	}

	let Ok(mut cache) = USER_METADATA_CACHE.lock() else {
		return;
	};
	prune_user_metadata_cache(&mut cache);
	cache.insert(
		cache_key,
		CachedUserMetadata {
			user: user.clone(),
			cached_at: Instant::now(),
		},
	);
	while cache.len() > USER_METADATA_CACHE_SIZE {
		let Some(oldest_key) = cache.iter().min_by_key(|(_, entry)| entry.cached_at).map(|(key, _)| key.clone()) else {
			break;
		};
		cache.remove(&oldest_key);
	}
}

fn prune_user_video_stats_cache(cache: &mut HashMap<String, CachedUserVideoStats>) {
	let now = Instant::now();
	cache.retain(|_, entry| now.duration_since(entry.cached_at) <= USER_VIDEO_STATS_CACHE_TTL);
}

fn observe_user_video_post(entry: &mut CachedUserVideoStats, post: &Post) {
	entry.stats.observe_post(post);
}

fn update_user_video_stats(username: &str, posts: &[Post]) {
	let cache_key = normalize_user_video_stats_cache_key(username);
	if cache_key.is_empty() {
		return;
	}

	let Ok(mut cache) = USER_VIDEO_STATS_CACHE.lock() else {
		return;
	};
	prune_user_video_stats_cache(&mut cache);

	let now = Instant::now();
	let entry = cache.entry(cache_key.clone()).or_insert_with(|| CachedUserVideoStats {
		stats: UserVideoStatsAccumulator::default(),
		cached_at: now,
	});
	entry.cached_at = now;

	for post in posts {
		observe_user_video_post(entry, post);
	}

	while cache.len() > USER_VIDEO_STATS_CACHE_SIZE {
		let Some(oldest_key) = cache.iter().min_by_key(|(_, entry)| entry.cached_at).map(|(key, _)| key.clone()) else {
			break;
		};
		cache.remove(&oldest_key);
	}
}

pub(crate) fn update_user_video_stats_from_posts(posts: &[Post]) {
	let Ok(mut cache) = USER_VIDEO_STATS_CACHE.lock() else {
		return;
	};
	prune_user_video_stats_cache(&mut cache);

	let now = Instant::now();
	for post in posts {
		let cache_key = normalize_user_video_stats_cache_key(&post.author.name);
		if cache_key.is_empty() {
			continue;
		}

		let entry = cache.entry(cache_key).or_insert_with(|| CachedUserVideoStats {
			stats: UserVideoStatsAccumulator::default(),
			cached_at: now,
		});
		entry.cached_at = now;
		observe_user_video_post(entry, post);
	}

	while cache.len() > USER_VIDEO_STATS_CACHE_SIZE {
		let Some(oldest_key) = cache.iter().min_by_key(|(_, entry)| entry.cached_at).map(|(key, _)| key.clone()) else {
			break;
		};
		cache.remove(&oldest_key);
	}
}

#[cfg(test)]
fn cached_user_video_stats(username: &str) -> Option<UserVideoStats> {
	let cache_key = normalize_user_video_stats_cache_key(username);
	if cache_key.is_empty() {
		return None;
	}

	let Ok(mut cache) = USER_VIDEO_STATS_CACHE.lock() else {
		return None;
	};
	prune_user_video_stats_cache(&mut cache);

	let entry = cache.get_mut(&cache_key)?;
	entry.cached_at = Instant::now();

	Some(UserVideoStats {
		average_duration_seconds: entry.stats.average_duration_seconds()?,
		video_count: entry.stats.video_count(),
	})
}

pub fn cached_avg_video_length(username: &str) -> Option<(String, String)> {
	cached_video_signal(username).avg_video_length
}

pub fn cached_video_signal(username: &str) -> UserVideoSignal {
	let cache_key = normalize_user_video_stats_cache_key(username);
	if cache_key.is_empty() {
		return UserVideoSignal::default();
	}

	let mut stats = if let Ok(mut cache) = USER_VIDEO_STATS_CACHE.lock() {
		prune_user_video_stats_cache(&mut cache);
		if let Some(entry) = cache.get_mut(&cache_key) {
			entry.cached_at = Instant::now();
			entry.stats.clone()
		} else {
			UserVideoStatsAccumulator::default()
		}
	} else {
		UserVideoStatsAccumulator::default()
	};

	if let Some(catalog_stats) = crate::materialized_catalog::catalog_video_stats(&cache_key) {
		stats.merge(catalog_stats);
	}

	UserVideoSignal {
		avg_video_length: stats.avg_video_length(),
		video_count: stats.video_count(),
		redgifs_profile_count: stats.redgifs_profile_count(),
		redgifs_usernames: stats.redgifs_usernames(),
	}
}

pub fn hydrate_posts_avg_video_length_from_cache(posts: &mut [Post]) {
	for post in posts.iter_mut() {
		post.author.avg_video_length = cached_avg_video_length(&post.author.name);
	}
}

fn avg_video_length_redgifs_profile_count(meta: &str) -> Option<usize> {
	let profile_segment = meta.split('·').find(|segment| segment.trim().ends_with("rg profiles"))?;
	profile_segment.trim().split_whitespace().next()?.parse().ok()
}

pub fn avg_video_length_rg_profile_count(value: &(String, String)) -> usize {
	avg_video_length_redgifs_profile_count(&value.1).unwrap_or_default()
}

pub fn avg_video_length_rg_profile_class(value: &Option<(String, String)>) -> &'static str {
	match value.as_ref().map(avg_video_length_rg_profile_count).unwrap_or_default() {
		2 => " post_rg_profiles_2",
		count if count >= 3 => " post_rg_profiles_3plus",
		_ => "",
	}
}

pub fn avg_video_length_author_label(value: &(String, String)) -> String {
	let mut label = format!("avg {}", value.0);
	let rg_profiles = avg_video_length_rg_profile_count(value);
	if rg_profiles > 1 {
		label.push_str(&format!(" · rg {rg_profiles}"));
	}
	label
}

#[cfg(test)]
pub(crate) fn clear_test_caches() {
	if let Ok(mut cache) = USER_VIDEO_STATS_CACHE.lock() {
		cache.clear();
	}
	if let Ok(mut cache) = USER_METADATA_CACHE.lock() {
		cache.clear();
	}
}

pub(crate) fn collapsed_duplicate(post: Post) -> crate::utils::CollapsedDuplicate {
	crate::utils::CollapsedDuplicate {
		title: post.title,
		permalink: post.permalink,
		community: post.community.clone(),
		display_community: display_community(&post.community),
		created_ts: post.created_ts,
		rel_time: post.rel_time,
		comments: post.comments,
		flair: post.flair,
	}
}

pub(crate) fn display_community(community: &str) -> String {
	if community.starts_with("u_") {
		format!("u/{}", &community[2..])
	} else {
		format!("r/{community}")
	}
}

fn default_user_sort(req: &Request<Body>) -> String {
	let default_sort = setting(req, "post_sort");
	if USER_SORT_OPTIONS.contains(&default_sort.as_str()) {
		default_sort
	} else {
		"hot".to_string()
	}
}

fn is_video_listing(listing: &str) -> bool {
	listing == "videos"
}

fn current_url_parts(url: &str) -> (&str, &str) {
	url.split_once('?').unwrap_or((url, ""))
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

fn now_unix() -> u64 {
	OffsetDateTime::now_utc().unix_timestamp().max(0) as u64
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
	hidden_query_pairs.retain(|(key, _)| key != "cache_page" && key != "before" && key != "after");

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

fn video_mode_page_url(current_url: &str, page: usize, query: &video_mode::VideoModeQuery, embed_video_params_in_url: bool) -> String {
	let (path, raw_query) = current_url_parts(current_url);
	let mut query_pairs = video_mode::query_pairs(raw_query);
	query_pairs.retain(|(key, _)| key != "cache_page" && key != "before" && key != "after");
	video_mode::strip_video_query_pairs(&mut query_pairs);
	if embed_video_params_in_url {
		video_mode::append_query_pairs(&mut query_pairs, query);
	}
	if page > 0 {
		query_pairs.push(("cache_page".to_string(), page.to_string()));
	}
	build_url_from_query_pairs(path, &query_pairs)
}

fn user_listing_path(username: &str, listing: &str, query: &str, default_sort: &str) -> String {
	let mut query_pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();

	if !is_video_listing(listing) && !query_pairs.iter().any(|(key, _)| key == "sort") {
		query_pairs.push(("sort".to_string(), default_sort.to_string()));
	}

	query_pairs.push(("raw_json".to_string(), "1".to_string()));

	let query = form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish();

	format!("/user/{username}/{listing}.json?{query}")
}

fn user_reddit_path(username: &str, listing: &str, query: &str) -> String {
	let base = if listing == "overview" {
		format!("/user/{username}/")
	} else {
		format!("/user/{username}/{listing}")
	};

	if query.is_empty() {
		base
	} else {
		format!("{base}?{query}")
	}
}

fn user_fragment_path(username: &str, listing: &str, query: &str) -> String {
	let base = if listing == "overview" {
		format!("/user-fragment/{username}")
	} else {
		format!("/user-fragment/{username}/{listing}")
	};

	if query.is_empty() {
		base
	} else {
		format!("{base}?{query}")
	}
}

fn placeholder_user(username: &str) -> User {
	User {
		name: username.to_string(),
		..Default::default()
	}
}

fn cached_or_placeholder_user(username: &str) -> User {
	cached_user_metadata(username).unwrap_or_else(|| placeholder_user(username))
}

fn user_upstream_unavailable_notice(username: &str, msg: &str) -> Option<String> {
	match crate::client::source_unavailable_from_error(msg)? {
		crate::client::SourceUnavailable::Suspended => Some(format!("u/{username} is suspended on Reddit. Showing cached posts when available.")),
		crate::client::SourceUnavailable::Banned => Some(format!("u/{username} is banned upstream. Showing cached posts when available.")),
		crate::client::SourceUnavailable::Private => Some(format!("u/{username} is private on Reddit. Showing cached posts when available.")),
		crate::client::SourceUnavailable::NotFound => Some(format!("u/{username} was not found on Reddit. Showing cached posts when available.")),
		crate::client::SourceUnavailable::Forbidden => Some(format!("u/{username} is unavailable from Reddit. Showing cached posts when available.")),
	}
}

fn cached_post_identity(post: &Post) -> String {
	if !post.id.trim().is_empty() {
		format!("{}:{}", post.kind, post.id)
	} else {
		post.permalink.trim().to_string()
	}
}

fn cached_post_ids_contains(cached_post_ids: &[String], post: &Post) -> bool {
	let identity = cached_post_identity(post);
	!identity.is_empty() && cached_post_ids.iter().any(|value| value == &identity)
}

#[derive(Default)]
struct UserPostMerge {
	posts: Vec<Post>,
	cached_post_ids: Vec<String>,
	all_cached_posts_filtered: bool,
}

fn cached_user_sources_for_listing(username: &str, listing: &str) -> Vec<crate::post_cache::FeedSource> {
	let submitted = crate::post_cache::FeedSource::subreddit(format!("u_{username}"), crate::post_cache::FeedSort::New);
	let comments = crate::post_cache::FeedSource::user_comments(format!("u_{username}"), crate::post_cache::FeedSort::New);

	match listing {
		"submitted" | "videos" => vec![submitted],
		"comments" => vec![comments],
		"overview" => vec![submitted, comments],
		_ => Vec::new(),
	}
}

fn cached_post_matches_user_listing(post: &Post, listing: &str) -> bool {
	match listing {
		"submitted" | "videos" => post.kind != "t1",
		"comments" => post.kind == "t1",
		"overview" => true,
		_ => false,
	}
}

fn cached_subreddit_source_posts_for_user(username: &str, listing: &str, filters: &HashSet<String>) -> Vec<Post> {
	if listing == "comments" {
		return Vec::new();
	}

	let username_lc = username.trim().to_lowercase();
	if username_lc.is_empty() {
		return Vec::new();
	}

	let mut seen_sources = HashSet::new();
	let mut posts = Vec::new();
	for entry in user_sidebar_subreddits(username).into_iter().take(USER_CACHE_SUBREDDIT_SOURCE_LIMIT) {
		let subreddit = entry.name.trim();
		if subreddit.is_empty() || filters.contains(subreddit) || filters.contains(&subreddit.to_lowercase()) {
			continue;
		}
		if !seen_sources.insert(subreddit.to_lowercase()) {
			continue;
		}

		let source = crate::post_cache::FeedSource::subreddit(subreddit.to_string(), crate::post_cache::FeedSort::New);
		let Some(cached_posts) = crate::post_cache::servable_cached_source_posts(&source) else {
			continue;
		};
		posts.extend(
			cached_posts
				.into_iter()
				.filter(|post| post.author.name.trim().eq_ignore_ascii_case(&username_lc))
				.filter(|post| cached_post_matches_user_listing(post, listing)),
		);
	}
	posts
}

fn cached_user_posts_for_listing(username: &str, listing: &str, filters: &HashSet<String>) -> Vec<Post> {
	let mut posts = cached_user_sources_for_listing(username, listing)
		.into_iter()
		.filter_map(|source| crate::post_cache::servable_cached_source_posts(&source))
		.flatten()
		.collect::<Vec<_>>();

	posts.extend(cached_subreddit_source_posts_for_user(username, listing, filters));
	match crate::cache_sqlite::query_submitted(username, USER_CACHE_MATERIALIZED_POST_LIMIT, filters) {
		Ok(Some(cached)) => posts.extend(cached.into_iter().map(crate::cache_persistence::PersistedPost::into_runtime)),
		Ok(None) => posts.extend(crate::materialized_catalog::query_author_posts_limited(
			username,
			filters,
			USER_CACHE_MATERIALIZED_POST_LIMIT,
		)),
		Err(error) => {
			warn!("user cache: SQLite Submitted query failed; using legacy materialized catalog: {error}");
			posts.extend(crate::materialized_catalog::query_author_posts_limited(
				username,
				filters,
				USER_CACHE_MATERIALIZED_POST_LIMIT,
			));
		}
	}
	posts.retain(|post| cached_post_matches_user_listing(post, listing));
	posts.sort_by(|left, right| {
		post_feed_activity_ts(right)
			.cmp(&post_feed_activity_ts(left))
			.then_with(|| right.created_ts.cmp(&left.created_ts))
			.then_with(|| right.id.cmp(&left.id))
	});
	posts
}

fn request_is_first_user_page(query: &str) -> bool {
	let request_after = param(&format!("?{query}"), "after").unwrap_or_default();
	let request_before = param(&format!("?{query}"), "before").unwrap_or_default();
	let cache_page = param(&format!("?{query}"), "cache_page").and_then(|page| page.parse::<usize>().ok()).unwrap_or(0);
	request_after.is_empty() && request_before.is_empty() && cache_page == 0
}

fn merge_user_posts_with_cached_posts(live_posts: Vec<Post>, mut cached_posts: Vec<Post>, filters: &HashSet<String>) -> UserPostMerge {
	let cached_raw_count = cached_posts.len();
	let (_, all_cached_posts_filtered) = filter_posts(&mut cached_posts, filters);

	let mut cached_identities = HashSet::new();
	let mut seen = HashSet::new();
	for post in &live_posts {
		let identity = cached_post_identity(post);
		if !identity.is_empty() {
			seen.insert(identity);
		}
	}

	let mut posts = live_posts;
	let mut cached_post_ids = Vec::new();
	for post in cached_posts {
		let identity = cached_post_identity(&post);
		if identity.is_empty() || !seen.insert(identity.clone()) {
			continue;
		}
		cached_identities.insert(identity);
		posts.push(post);
	}
	posts = collapse_duplicate_submissions(posts);
	for post in &posts {
		let identity = cached_post_identity(post);
		if cached_identities.contains(&identity) {
			cached_post_ids.push(identity);
		}
	}

	UserPostMerge {
		posts,
		cached_post_ids,
		all_cached_posts_filtered: cached_raw_count > 0 && all_cached_posts_filtered,
	}
}

fn cached_video_mode_sidebar(req: &Request<Body>, username: &str, url: &str, query: &str, filters: &HashSet<String>) -> video_mode::VideoModeSidebarData {
	let cached_posts = cached_user_posts_for_listing(username, "videos", filters);
	let cookie_value = crate::utils::read_sharded_cookie(req, video_mode::VIDEO_MODE_QUERY_COOKIE);
	let parsed_query = video_mode::parse_query_or_cookie(query, &cookie_value);
	let listing_data = video_mode::build_listing(&cached_posts, filters, &parsed_query, now_unix());
	let normalized_query = normalized_video_mode_query(&parsed_query, &listing_data);
	let cache_page = param(&format!("?{query}"), "cache_page").and_then(|page| page.parse::<usize>().ok()).unwrap_or(0);
	let start = cache_page.saturating_mul(crate::post_cache::POSTS_PER_PAGE);
	let end = (start + crate::post_cache::POSTS_PER_PAGE).min(listing_data.posts.len());
	let preview_posts = if start < listing_data.posts.len() {
		listing_data.posts[start..end].to_vec()
	} else {
		Vec::new()
	};
	build_video_mode_sidebar(url, &listing_data, &preview_posts, &normalized_query)
}

async fn fetch_user_listing_posts(username: &str, path: &str, listing: &str, filters: &HashSet<String>) -> Result<(Vec<Post>, String, bool), String> {
	let should_refill_duplicates = listing == "submitted" || listing == "overview";
	let mut request_path = path.to_string();
	let mut all_posts = Vec::new();
	let mut fetched_raw_posts = false;
	let mut requests = 0;
	let mut filtered_page_advances = 0;

	let after = loop {
		let (mut posts, next_after) = Post::fetch(&request_path, false).await?;
		fetched_raw_posts |= !posts.is_empty();
		utils::store_post_flairs(&posts);

		let (_, page_all_posts_filtered) = filter_posts(&mut posts, filters);
		if should_refill_duplicates {
			utils::hydrate_media_content_fingerprints(&mut posts).await;
		}
		all_posts.extend(posts);

		if should_refill_duplicates {
			all_posts = collapse_duplicate_submissions(all_posts);
		}

		let should_continue_for_duplicates = should_refill_duplicates && all_posts.len() < TARGET_POST_COUNT && requests < MAX_DUPLICATE_FILL_REQUESTS;
		// A page can be non-empty on Reddit but empty after local filters; advance a few cursors
		// so one filtered page does not look like the end of the listing.
		let should_continue_for_filtered_page = all_posts.is_empty() && page_all_posts_filtered && filtered_page_advances < MAX_FILTERED_PAGE_ADVANCE;
		let should_continue = !next_after.is_empty() && (should_continue_for_duplicates || should_continue_for_filtered_page);
		if !should_continue {
			break next_after;
		}

		if should_continue_for_filtered_page {
			filtered_page_advances += 1;
		}
		request_path = utils::listing_path_with_after(path, &next_after)?;
		requests += 1;
	};

	if should_refill_duplicates {
		all_posts.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
	}
	utils::store_user_subreddits(username, &all_posts);
	crate::materialized_catalog::record_author_history_from_posts(&all_posts);
	utils::hydrate_post_author_links_from_cache(&mut all_posts);
	update_user_video_stats(username, &all_posts);
	let all_posts_filtered = fetched_raw_posts && all_posts.is_empty();
	Ok((all_posts, after, all_posts_filtered))
}

async fn cached_or_live_user_video_posts(username: &str) -> Result<(Vec<Post>, bool), String> {
	let source = crate::post_cache::FeedSource::subreddit(format!("u_{username}"), crate::post_cache::FeedSort::New);
	if let Some(posts) = crate::post_cache::servable_cached_source_posts(&source) {
		return Ok((posts, true));
	}

	let path = format!("/user/{username}/submitted.json?sort=new&limit={}&raw_json=1", crate::post_cache::per_source_limit());
	let (posts, after) = Post::fetch(&path, false).await?;
	utils::store_post_flairs(&posts);
	utils::store_user_subreddits(username, &posts);
	crate::materialized_catalog::record_author_history_from_posts(&posts);
	update_user_video_stats(username, &posts);
	crate::post_cache::store_source_posts_for_cache(&source, posts.clone(), after, Some(crate::post_cache::RefreshOrigin::DirectLiveFetch));
	Ok((posts, false))
}

#[cfg(test)]
mod tests {
	use super::{append_missing_following_link_posts, collapse_duplicate_submissions, show_successful_empty_state};
	use askama::Template;
	use hyper::{Body, Request};
	use std::collections::HashSet;
	use url::form_urlencoded;

	use crate::utils::{Author, Awards, CollapsedDuplicate, Flags, Flair, FlairPart, FollowingComment, Media, ParentComment, Post, Preferences};

	#[test]
	fn successful_zero_post_listing_has_a_distinct_empty_state() {
		assert!(show_successful_empty_state(true, false, ""));
	}

	#[test]
	fn filtered_and_fallback_listings_do_not_use_the_successful_empty_state() {
		assert!(!show_successful_empty_state(true, true, ""));
		assert!(!show_successful_empty_state(true, false, "Reddit is rate-limited; showing cached posts."));
		assert!(!show_successful_empty_state(false, false, ""));
	}

	fn test_post(id: &str, title: &str, community: &str, body: &str, post_type: &str, media_url: &str, gallery_urls: &[&str]) -> Post {
		Post {
			id: id.to_string(),
			kind: "t3".to_string(),
			title: title.to_string(),
			community: community.to_string(),
			body: body.to_string(),
			author: Author {
				name: "SilkySavannah".to_string(),
				flair: Flair {
					flair_parts: Vec::new(),
					text: String::new(),
					background_color: String::new(),
					foreground_color: String::new(),
				},
				distinguished: String::new(),
				external_links: Vec::new(),
				avg_video_length: None,
			},
			permalink: format!("/r/{community}/comments/{id}/"),
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
			post_type: post_type.to_string(),
			flair: Flair {
				flair_parts: Vec::new(),
				text: String::new(),
				background_color: String::new(),
				foreground_color: String::new(),
			},
			flags: Flags::new(false, false, false),
			thumbnail: Media {
				url: String::new(),
				..Media::empty()
			},
			media: Media {
				url: media_url.to_string(),
				..Media::empty()
			},
			domain: "i.redd.it".to_string(),
			rel_time: "1h ago".to_string(),
			created: String::new(),
			created_ts: 0,
			num_duplicates: 0,
			comments: ("0".to_string(), "0".to_string()),
			gallery: gallery_urls
				.iter()
				.map(|url| crate::utils::GalleryMedia {
					url: (*url).to_string(),
					preview_url: String::new(),
					preview_srcset: String::new(),
					width: 0,
					height: 0,
					caption: String::new(),
					outbound_url: String::new(),
				})
				.collect(),
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

	fn test_prefs() -> Preferences {
		Preferences {
			available_themes: vec!["system".to_string()],
			layout: "card".to_string(),
			show_nsfw: "on".to_string(),
			fixed_navbar: "on".to_string(),
			disable_visit_reddit_confirmation: "on".to_string(),
			..Preferences::default()
		}
	}

	fn test_embed_gif_post(id: &str, title: &str, community: &str, embed_src: &str) -> Post {
		let mut post = test_post(id, title, community, "", "gif", "", &[]);
		post.media.embed_code = format!("<iframe src=\"{embed_src}\" frameborder=\"0\"></iframe>");
		post
	}

	fn test_video_post(id: &str, title: &str, media_url: &str, duration_seconds: i64) -> Post {
		let mut post = test_post(id, title, "demo_sub", "", "video", media_url, &[]);
		post.media.duration_seconds = duration_seconds;
		post.media.duration = crate::utils::format_video_duration(duration_seconds);
		post
	}

	fn test_comment(id: &str, community: &str, body: &str, parent_kind: &str, link_title: &str) -> Post {
		let mut post = test_post(id, String::new().as_str(), community, body, "comment", "", &[]);
		post.kind = "t1".to_string();
		post.parent_kind = parent_kind.to_string();
		post.link_title = link_title.to_string();
		post.link_permalink = format!("/r/{community}/comments/thread/");
		post.permalink = format!("/r/{community}/comments/thread/{id}/");
		post.comments = ("32".to_string(), "32".to_string());
		post
	}

	#[test]
	fn collapses_matching_image_posts() {
		let posts = vec![
			test_post("one", "First title", "sub_one", "", "image", "https://i.redd.it/example.jpg?width=2048", &[]),
			test_post("two", "Second title", "sub_two", "", "image", "https://i.redd.it/example.jpg?width=1024", &[]),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].title, "First title");
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "Second title");
		assert_eq!(collapsed[0].collapsed_duplicates[0].display_community, "r/sub_two");
	}

	#[test]
	fn collapses_matching_image_content_fingerprints() {
		let mut first = test_post("one", "First title", "sub_one", "", "image", "https://i.redd.it/first.jpg", &[]);
		first.media.preview_url = "/preview/pre/first.jpg?width=640&format=pjpg&auto=webp&s=one".to_string();
		first.thumbnail.url = "/thumb/a/first.jpg".to_string();
		first.media.content_fingerprint = "same-preview-content".to_string();
		let mut second = test_post("two", "Second title", "sub_two", "", "image", "https://i.redd.it/second.jpg", &[]);
		second.media.preview_url = "/preview/pre/second.jpg?width=640&format=pjpg&auto=webp&s=two".to_string();
		second.thumbnail.url = "/thumb/b/second.jpg".to_string();
		second.media.content_fingerprint = "same-preview-content".to_string();

		let collapsed = collapse_duplicate_submissions(vec![first, second]);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "Second title");
	}

	#[test]
	fn collapses_matching_image_thumbnail_urls() {
		let mut first = test_post("one", "First title", "sub_one", "", "image", "https://i.redd.it/first.jpg", &[]);
		first.thumbnail.url = "/thumb/a/shared.jpg".to_string();
		let mut second = test_post("two", "Second title", "sub_two", "", "image", "https://i.redd.it/second.jpg", &[]);
		second.thumbnail.url = "/thumb/a/shared.jpg?ignored=1".to_string();

		let collapsed = collapse_duplicate_submissions(vec![first, second]);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
	}

	#[test]
	fn append_missing_following_link_posts_adds_missing_original_threads() {
		let mut comment = test_comment("c1", "demo_sub", "<div class=\"md\"><p>hi</p></div>", "t3", "Thread title");
		comment.link_id = "t3_thread1".to_string();
		let original = test_post("thread1", "Thread title", "demo_sub", "<div class=\"md\"><p>post body</p></div>", "self", "", &[]);

		let hydrated = append_missing_following_link_posts(vec![comment], std::iter::once(("thread1".to_string(), original)).collect());

		assert_eq!(hydrated.len(), 2);
		assert!(hydrated.iter().any(|post| post.kind == "t3" && post.id == "thread1"));
	}

	#[test]
	fn merge_full_submission_keeps_following_comments_and_bookmark_state() {
		let mut shell = Post::default();
		shell.id = "abc".to_string();
		shell.kind = "t3".into();
		shell.following_comments.push(FollowingComment {
			id: "c1".to_string(),
			author: "u1".to_string(),
			body: String::new(),
			permalink: String::new(),
			created_ts: 0,
			rel_time: String::new(),
			community: "s".to_string(),
			score: ("0".to_string(), "0".to_string()),
			parent_id: String::new(),
			parent_kind: String::new(),
			parent_comment: ParentComment::default(),
			link_title: String::new(),
			link_permalink: String::new(),
			link_author: String::new(),
			link_created_ts: 0,
			thread_comments: ("0".to_string(), "0".to_string()),
			thread_nsfw: false,
		});
		shell.bookmarked = true;
		let full = test_post("abc", "T", "s", "<div class=\"md\"><p>inside</p></div>", "self", "", &[]);
		super::merge_full_submission_into_following_bundle(&mut shell, full);
		assert_eq!(shell.following_comments.len(), 1);
		assert_eq!(shell.following_comments[0].id, "c1");
		assert!(shell.body.contains("inside"));
		assert!(shell.bookmarked);
	}

	#[test]
	fn collapse_merges_following_comments_for_duplicate_media() {
		let mut a = test_post("one", "A", "s1", "", "image", "https://i.redd.it/x.jpg", &[]);
		a.following_comments.push(FollowingComment {
			id: "c1".to_string(),
			author: "demo_author".to_string(),
			body: String::new(),
			permalink: String::new(),
			created_ts: 1,
			rel_time: String::new(),
			community: "s1".to_string(),
			score: ("1".to_string(), "1".to_string()),
			parent_id: String::new(),
			parent_kind: String::new(),
			parent_comment: ParentComment::default(),
			link_title: String::new(),
			link_permalink: String::new(),
			link_author: String::new(),
			link_created_ts: 0,
			thread_comments: ("0".to_string(), "0".to_string()),
			thread_nsfw: false,
		});
		let mut b = test_post("two", "B", "s2", "", "image", "https://i.redd.it/x.jpg?w=1", &[]);
		b.following_comments.push(FollowingComment {
			id: "c2".to_string(),
			author: "demo_author".to_string(),
			body: String::new(),
			permalink: String::new(),
			created_ts: 2,
			rel_time: String::new(),
			community: "s2".to_string(),
			score: ("1".to_string(), "1".to_string()),
			parent_id: String::new(),
			parent_kind: String::new(),
			parent_comment: ParentComment::default(),
			link_title: String::new(),
			link_permalink: String::new(),
			link_author: String::new(),
			link_created_ts: 0,
			thread_comments: ("0".to_string(), "0".to_string()),
			thread_nsfw: false,
		});
		let collapsed = collapse_duplicate_submissions(vec![a, b]);
		assert_eq!(collapsed.len(), 1);
		let ids: Vec<_> = collapsed[0].following_comments.iter().map(|c| c.id.as_str()).collect();
		assert!(ids.contains(&"c1") && ids.contains(&"c2"));
	}

	#[test]
	fn uses_duplicate_text_as_preview_when_primary_has_none() {
		let posts = vec![
			test_post("one", "Primary", "sub_one", "", "image", "https://i.redd.it/example.jpg", &[]),
			test_post(
				"two",
				"Secondary",
				"sub_two",
				"<div class=\"md\"><p>caption</p></div>",
				"image",
				"https://i.redd.it/example.jpg",
				&[],
			),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].preview_body, "<div class=\"md\"><p>caption</p></div>");
	}

	#[test]
	fn collapses_matching_galleries_using_normalized_urls() {
		let posts = vec![
			test_post(
				"one",
				"Gallery one",
				"sub_one",
				"",
				"gallery",
				"",
				&["/preview/pre/a.jpg?width=300", "/preview/pre/b.jpg?width=300"],
			),
			test_post(
				"two",
				"Gallery two",
				"sub_two",
				"",
				"gallery",
				"",
				&["/preview/pre/a.jpg?width=1200", "/preview/pre/b.jpg?width=1200"],
			),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
	}

	#[test]
	fn collapses_matching_gif_posts() {
		let posts = vec![
			test_post("one", "GIF one", "sub_one", "", "gif", "https://v.redd.it/example/DASH_720.mp4?source=fallback", &[]),
			test_post("two", "GIF two", "sub_two", "", "gif", "https://v.redd.it/example/DASH_720.mp4?foo=bar", &[]),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "GIF two");
	}

	#[test]
	fn collapses_matching_gif_content_fingerprints() {
		let mut first = test_post("one", "GIF one", "sub_one", "", "gif", "/preview/pre/first.gif?format=mp4&s=one", &[]);
		first.media.content_fingerprint = "same-video-preview-content".to_string();
		let mut second = test_post("two", "GIF two", "sub_two", "", "gif", "/preview/pre/second.gif?format=mp4&s=two", &[]);
		second.media.content_fingerprint = "same-video-preview-content".to_string();

		let collapsed = collapse_duplicate_submissions(vec![first, second]);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "GIF two");
	}

	#[test]
	fn merge_user_posts_with_cached_posts_marks_only_cache_additions() {
		let live = test_post("live", "Live title", "demo_sub", "", "image", "https://i.redd.it/live.jpg", &[]);
		let cached_duplicate = test_post("live", "Cached duplicate", "demo_sub", "", "image", "https://i.redd.it/live.jpg", &[]);
		let cached_extra = test_post("cached", "Cached title", "demo_sub", "", "image", "https://i.redd.it/cached.jpg", &[]);
		let cached_filtered = test_post("filtered", "Filtered title", "blocked_sub", "", "image", "https://i.redd.it/filtered.jpg", &[]);
		let filters = HashSet::from(["blocked_sub".to_string()]);

		let merged = super::merge_user_posts_with_cached_posts(vec![live], vec![cached_duplicate, cached_extra.clone(), cached_filtered], &filters);

		assert_eq!(merged.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["live", "cached"]);
		assert_eq!(merged.cached_post_ids, vec![super::cached_post_identity(&cached_extra)]);
		assert!(!merged.all_cached_posts_filtered);
	}

	#[test]
	fn merge_user_posts_with_cached_posts_collapses_cached_duplicate_under_live_post() {
		let live = test_post("live", "Live title", "sub_one", "", "image", "https://i.redd.it/shared.jpg?width=2048", &[]);
		let cached_duplicate = test_post(
			"cached-duplicate",
			"Cached duplicate",
			"sub_two",
			"",
			"image",
			"https://i.redd.it/shared.jpg?width=1024",
			&[],
		);
		let filters = HashSet::new();

		let merged = super::merge_user_posts_with_cached_posts(vec![live], vec![cached_duplicate], &filters);

		assert_eq!(merged.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["live"]);
		assert_eq!(merged.posts[0].collapsed_duplicates.len(), 1);
		assert_eq!(merged.posts[0].collapsed_duplicates[0].title, "Cached duplicate");
		assert!(merged.cached_post_ids.is_empty());
	}

	#[test]
	fn merge_user_posts_with_cached_posts_collapses_cache_only_duplicates() {
		let cached_primary = test_post("cached-primary", "Cached primary", "sub_one", "", "image", "https://i.redd.it/shared.jpg?width=2048", &[]);
		let cached_duplicate = test_post(
			"cached-duplicate",
			"Cached duplicate",
			"sub_two",
			"",
			"image",
			"https://i.redd.it/shared.jpg?width=1024",
			&[],
		);
		let filters = HashSet::new();

		let merged = super::merge_user_posts_with_cached_posts(Vec::new(), vec![cached_primary.clone(), cached_duplicate], &filters);

		assert_eq!(merged.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["cached-primary"]);
		assert_eq!(merged.posts[0].collapsed_duplicates.len(), 1);
		assert_eq!(merged.posts[0].collapsed_duplicates[0].title, "Cached duplicate");
		assert_eq!(merged.cached_post_ids, vec![super::cached_post_identity(&cached_primary)]);
	}

	#[test]
	fn cached_user_posts_for_listing_uses_materialized_posts_from_subreddit_cache() {
		let _guard = crate::materialized_catalog::MATERIALIZED_CATALOG_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
		crate::materialized_catalog::reset_catalog();
		let mut cached = test_post("cached-sub-post", "Cached subreddit post", "loaded_sub", "", "image", "https://i.redd.it/cached.jpg", &[]);
		cached.author.name = "OpenedUser".to_string();
		cached.created_ts = 300;
		let mut filtered = test_post("cached-filtered", "Cached filtered post", "blocked_sub", "", "image", "https://i.redd.it/filtered.jpg", &[]);
		filtered.author.name = "OpenedUser".to_string();
		filtered.created_ts = 290;
		let mut comment = test_comment("cached-comment", "loaded_sub", "cached comment", "t3", "Cached link");
		comment.author.name = "OpenedUser".to_string();
		comment.created_ts = 280;

		crate::materialized_catalog::apply_source_refresh(
			&crate::post_cache::FeedSource::subreddit("loaded_sub", crate::post_cache::FeedSort::New),
			&[cached.clone(), filtered, comment],
		);

		let filters = HashSet::from(["blocked_sub".to_string()]);
		let posts = super::cached_user_posts_for_listing("openeduser", "submitted", &filters);

		assert_eq!(posts.into_iter().map(|post| post.id).collect::<Vec<_>>(), vec!["cached-sub-post".to_string()]);
		crate::materialized_catalog::reset_catalog();
	}

	#[test]
	fn cached_user_posts_for_listing_loads_sidebar_subreddit_source_cache_without_catalog() {
		let _guard = crate::materialized_catalog::MATERIALIZED_CATALOG_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
		crate::materialized_catalog::reset_catalog();
		crate::utils::clear_test_caches();
		let mut cached = test_post(
			"cached-sidebar-source-post",
			"Cached sidebar source post",
			"sidebar_loaded_sub",
			"",
			"image",
			"https://i.redd.it/sidebar-cached.jpg",
			&[],
		);
		cached.author.name = "OpenedUser".to_string();
		cached.created_ts = 300;

		crate::post_cache::store_source_posts_for_cache(
			&crate::post_cache::FeedSource::subreddit("sidebar_loaded_sub", crate::post_cache::FeedSort::New),
			vec![cached.clone()],
			String::new(),
			Some(crate::post_cache::RefreshOrigin::DirectLiveFetch),
		);
		crate::materialized_catalog::reset_catalog();
		crate::utils::store_user_subreddits("openeduser", &[cached.clone()]);

		let posts = super::cached_user_posts_for_listing("openeduser", "submitted", &HashSet::new());

		assert_eq!(posts.into_iter().map(|post| post.id).collect::<Vec<_>>(), vec!["cached-sidebar-source-post".to_string()]);
		crate::materialized_catalog::reset_catalog();
		crate::utils::clear_test_caches();
	}

	#[test]
	fn renders_from_cache_marker_for_marked_user_posts() {
		let live = test_post("live", "Live title", "demo_sub", "", "image", "https://i.redd.it/live.jpg", &[]);
		let cached = test_post("cached", "Cached title", "demo_sub", "", "image", "https://i.redd.it/cached.jpg", &[]);
		let cached_post_ids = vec![super::cached_post_identity(&cached)];
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![live, cached],
			cached_post_ids,
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: "u/demo_user is suspended on Reddit. Showing cached posts when available.".to_string(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert_eq!(html.matches("user_cached_post_marker").count(), 1);
		assert!(html.contains("from cache"));
		assert!(html.contains("u/demo_user is suspended on Reddit"));
		assert!(html.find("Live title").unwrap() < html.find("from cache").unwrap());
	}

	#[test]
	fn renders_deferred_cached_user_feed_as_user_link() {
		let mut posts = (0..6)
			.map(|index| {
				test_post(
					&format!("live-{index}"),
					&format!("Live title {index}"),
					"demo_sub",
					"",
					"image",
					&format!("https://i.redd.it/live-{index}.jpg"),
					&[],
				)
			})
			.collect::<Vec<_>>();
		let cached = test_post(
			"cached-user-feed",
			"Cached user-feed title",
			"u_demo_user",
			"",
			"image",
			"https://i.redd.it/cached-user-feed.jpg",
			&[],
		);
		let cached_post_ids = vec![super::cached_post_identity(&cached)];
		posts.push(cached);

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts,
			cached_post_ids,
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("user_cached_post_marker"));
		assert!(html.contains("class=\"post post_component_wrapper"));
		assert!(html.contains("<a class=\"post_subreddit\" href=\"/u/demo_user\">u/demo_user</a>"));
		assert!(!html.contains(">r/u_demo_user</a>"));
	}

	#[test]
	fn user_unavailable_notice_covers_private_not_found_and_rate_limit_separately() {
		assert_eq!(
			super::user_upstream_unavailable_notice("demo_user", "private").as_deref(),
			Some("u/demo_user is private on Reddit. Showing cached posts when available.")
		);
		assert_eq!(
			super::user_upstream_unavailable_notice("demo_user", r#"Reddit error 404 "null": Not Found | /user/demo_user/about.json"#).as_deref(),
			Some("u/demo_user was not found on Reddit. Showing cached posts when available.")
		);
		assert_eq!(super::user_upstream_unavailable_notice("demo_user", "Too many requests."), None);
	}

	#[test]
	fn collapses_matching_embedded_gif_posts() {
		let posts = vec![
			test_embed_gif_post("one", "Embed one", "sub_one", "https://www.redgifs.com/ifr/mediumpurplesoggyconure/"),
			test_embed_gif_post("two", "Embed two", "sub_two", "https://www.redgifs.com/ifr/mediumpurplesoggyconure"),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "Embed two");
	}

	#[test]
	fn collapses_matching_redgifs_video_posts() {
		let mut first = test_post("one", "Video one", "sub_one", "", "video", "/redgifs/zanyjudiciousrook", &[]);
		first.domain = "www.redgifs.com".to_string();

		let mut second = test_post("two", "Video two", "sub_two", "", "video", "/redgifs/zanyjudiciousrook/", &[]);
		second.domain = "redgifs.com".to_string();

		let collapsed = collapse_duplicate_submissions(vec![first, second]);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "Video two");
	}

	#[test]
	fn collapses_matching_video_content_fingerprints() {
		let mut first = test_post("one", "Video one", "sub_one", "", "video", "/vid/first/720.mp4", &[]);
		first.media.content_fingerprint = "same-video-content".to_string();
		let mut second = test_post("two", "Video two", "sub_two", "", "video", "/vid/second/720.mp4", &[]);
		second.media.content_fingerprint = "same-video-content".to_string();

		let collapsed = collapse_duplicate_submissions(vec![first, second]);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "Video two");
	}

	#[test]
	fn renders_collapsed_duplicates_in_user_template() {
		let mut post = test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[]);
		post.preview_body = "<div class=\"md\"><p>Preview text from a duplicate post.</p></div>".to_string();
		post.collapsed_duplicates = vec![
			CollapsedDuplicate {
				title: "Other title one".to_string(),
				permalink: "/r/other_one/comments/one/".to_string(),
				community: "other_one".to_string(),
				display_community: "r/other_one".to_string(),
				created_ts: 0,
				rel_time: "2h ago".to_string(),
				comments: ("12".to_string(), "12".to_string()),
				flair: Flair {
					flair_parts: vec![FlairPart {
						flair_part_type: "text".to_string(),
						value: "Question".to_string(),
					}],
					text: "Question".to_string(),
					background_color: "#eeeeee".to_string(),
					foreground_color: "#111111".to_string(),
				},
			},
			CollapsedDuplicate {
				title: "Other title two".to_string(),
				permalink: "/r/other_two/comments/two/".to_string(),
				community: "other_two".to_string(),
				display_community: "r/other_two".to_string(),
				created_ts: 0,
				rel_time: "3h ago".to_string(),
				comments: ("1".to_string(), "1".to_string()),
				flair: Flair {
					flair_parts: Vec::new(),
					text: String::new(),
					background_color: String::new(),
					foreground_color: String::new(),
				},
			},
		];

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![post],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "submitted".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user/submitted".to_string(),
			redirect_url: "user/demo_user/submitted".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(!html.contains("submitted_duplicates.js"));
		assert!(html.contains("Also posted to 2 other subreddits"));
		assert!(html.contains("Other title one"));
		assert!(html.contains("Other title two"));
		assert!(html.contains("12 comments"));
		assert!(html.contains("1 comment"));
		assert!(html.contains("Preview text from a duplicate post."));

		assert!(html.contains("/r/other_one/search?q=flair_name%3A%22Question%22&restrict_sr=on"));
		assert!(html.contains("/r/other_one/comments/one/\">Other title one</a>"));
		assert!(html.contains("/r/other_one\">r/other_one</a>"));
		assert!(html.contains("dir=\"ltr\"><span>Question</span></a>"));

		println!("{html}");
	}

	#[test]
	fn does_not_render_duplicate_script_in_overview_listing_by_default() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[])],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(!html.contains("submitted_duplicates.js"));
	}

	#[test]
	fn renders_shell_mode_user_placeholder_with_fragment_url() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: Vec::new(),
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: true,
			fragment_url: "/user-fragment/demo_user".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: "Loading user page...".to_string(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("data-fragment-url=\"/user-fragment/demo_user\""));
		assert!(html.contains("data-error-message=\"Loading the user page is taking longer than expected.\""));
		assert!(html.contains("data-timeout-ms=\"20000\""));
		assert!(!html.contains("data-append-fragment-content=\"true\""));
		assert!(html.contains("Loading user page..."));
		assert!(html.contains("id=\"page_fragment\""));
		assert!(html.contains("<main>"));
		assert!(html.contains("<aside>"));
	}

	#[test]
	fn renders_shell_mode_cached_posts_before_fragment_loads() {
		let cached = test_post("cached", "Cached shell title", "demo_sub", "", "image", "https://i.redd.it/cached-shell.jpg", &[]);
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![cached.clone()],
			cached_post_ids: vec![super::cached_post_identity(&cached)],
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: true,
			fragment_url: "/user-fragment/demo_user".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: "Loading user page...".to_string(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("data-fragment-url=\"/user-fragment/demo_user\""));
		assert!(html.contains("data-append-fragment-content=\"true\""));
		assert!(html.contains("Cached shell title"));
		assert!(html.contains("data-post-merge-keys=\"t3:cached"));
		assert!(html.contains("image:https://i.redd.it/cached-shell.jpg"));
		assert!(html.contains("user_cached_post_marker"));
		assert!(html.contains("from cache"));
		assert!(!html.contains("<center>Loading posts...</center>"));
	}

	#[test]
	fn renders_videos_listing_without_sort_controls_and_hides_single_option_filter_groups() {
		let clear_redirect = format!(
			"/api/video-mode/clear-redirect?{}",
			form_urlencoded::Serializer::new(String::new())
				.append_pair("redirect", "/user/demo_user/videos?sort=new")
				.finish()
		);
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![test_post("demo", "Primary post title", "demo_sub", "", "video", "https://v.redd.it/example", &[])],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "videos".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user/videos".to_string(),
			redirect_url: "user/demo_user/videos".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "main".to_string(),
			video_mode_enabled: true,
			video_mode_sidebar: crate::video_mode::VideoModeSidebarData {
				action_url: "/user/demo_user/videos".to_string(),
				clear_url: "/user/demo_user/videos?sort=new".to_string(),
				clear_cookie_redirect_href: clear_redirect.clone(),
				hidden_query_pairs: vec![("sort".to_string(), "new".to_string())],
				subreddit_options: vec![crate::video_mode::VideoModeFilterOption {
					name: "demo_sub".to_string(),
					count: 1,
					page_count: 1,
					active: false,
				}],
				user_options: vec![crate::video_mode::VideoModeFilterOption {
					name: "demo_user".to_string(),
					count: 1,
					page_count: 1,
					active: false,
				}],
				clear_subreddits: false,
				clear_users: false,
				video_sub_mode_value: String::new(),
				video_user_mode_value: String::new(),
				age_min_hours: 0,
				age_max_hours: 24,
				age_bound_hours: 24,
			},
		}
		.render()
		.unwrap();

		assert!(html.contains("class=\"selected\" href=\"/user/demo_user/videos\""));
		assert!(html.contains("Video Filters"));
		assert!(html.contains("href=\"https://www.reddit.com/user/demo_user/submitted?sort=new\""));
		assert!(html.contains("name=\"video_age_min_hours\""));
		assert!(html.contains(&format!("href=\"{clear_redirect}\"")));
		assert!(!html.contains("id=\"sort_select\""));
		assert!(!html.contains("id=\"sort_submit\""));
		assert!(!html.contains(">Subreddits<"));
		assert!(!html.contains(">Users<"));
	}

	#[test]
	fn video_sidebar_defaults_to_checked_options_and_renders_bulk_toggle_buttons() {
		let clear_redirect = format!(
			"/api/video-mode/clear-redirect?{}",
			form_urlencoded::Serializer::new(String::new()).append_pair("redirect", "/user/demo_user/videos").finish()
		);
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: Vec::new(),
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "videos".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user/videos".to_string(),
			redirect_url: "user/demo_user/videos".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: true,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "main".to_string(),
			video_mode_enabled: true,
			video_mode_sidebar: crate::video_mode::VideoModeSidebarData {
				action_url: "/user/demo_user/videos".to_string(),
				clear_url: "/user/demo_user/videos".to_string(),
				clear_cookie_redirect_href: clear_redirect,
				hidden_query_pairs: Vec::new(),
				subreddit_options: vec![
					crate::video_mode::VideoModeFilterOption {
						name: "demo_sub".to_string(),
						count: 3,
						page_count: 2,
						active: true,
					},
					crate::video_mode::VideoModeFilterOption {
						name: "other_sub".to_string(),
						count: 1,
						page_count: 0,
						active: true,
					},
				],
				user_options: vec![
					crate::video_mode::VideoModeFilterOption {
						name: "demo_user".to_string(),
						count: 2,
						page_count: 2,
						active: true,
					},
					crate::video_mode::VideoModeFilterOption {
						name: "other_user".to_string(),
						count: 1,
						page_count: 0,
						active: true,
					},
				],
				clear_subreddits: false,
				clear_users: false,
				video_sub_mode_value: String::new(),
				video_user_mode_value: String::new(),
				age_min_hours: 0,
				age_max_hours: 24,
				age_bound_hours: 24,
			},
		}
		.render()
		.unwrap();

		assert!(html.contains("data-video-filter-toggle=\"clear\""));
		assert!(html.contains("data-video-filter-toggle=\"check\""));
		assert!(html.contains("name=\"video_sub\" value=\"demo_sub\" checked"));
		assert!(html.contains("name=\"video_user\" value=\"demo_user\" checked"));
		assert!(html.contains("role=\"status\" aria-live=\"polite\""));
		assert!(html.contains("Reddit returned no posts for this user."));
	}

	#[test]
	fn user_video_mode_uses_submitted_new_as_external_fallback() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: Vec::new(),
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "videos".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user/videos".to_string(),
			redirect_url: "user/demo_user/videos".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: true,
			fragment_url: "/user-fragment/demo_user/videos".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: "Loading videos...".to_string(),
			replace_selector: "main".to_string(),
			video_mode_enabled: true,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("https://www.reddit.com/user/demo_user/submitted?sort=new"));
		assert!(!html.contains("https://www.reddit.com/user/demo_user/videos"));
	}

	#[test]
	fn user_listing_path_uses_default_sort_when_missing() {
		assert_eq!(
			super::user_listing_path("demo_user", "overview", "t=week", "top"),
			"/user/demo_user/overview.json?t=week&sort=top&raw_json=1"
		);
	}

	#[test]
	fn user_listing_path_preserves_explicit_sort() {
		assert_eq!(
			super::user_listing_path("demo_user", "submitted", "sort=new&t=day", "top"),
			"/user/demo_user/submitted.json?sort=new&t=day&raw_json=1"
		);
	}

	#[test]
	fn user_listing_path_omits_sort_for_videos() {
		assert_eq!(
			super::user_listing_path("demo_user", "videos", "video_sub=rust&video_user=alice", "top"),
			"/user/demo_user/videos.json?video_sub=rust&video_user=alice&raw_json=1"
		);
	}

	#[test]
	fn user_reddit_path_uses_profile_root_for_overview() {
		assert_eq!(super::user_reddit_path("demo_user", "overview", ""), "/user/demo_user/");
		assert_eq!(super::user_reddit_path("demo_user", "overview", "sort=new"), "/user/demo_user/?sort=new");
	}

	#[test]
	fn user_reddit_path_keeps_non_overview_listings() {
		assert_eq!(super::user_reddit_path("demo_user", "comments", ""), "/user/demo_user/comments");
		assert_eq!(
			super::user_reddit_path("demo_user", "submitted", "sort=top&t=week"),
			"/user/demo_user/submitted?sort=top&t=week"
		);
	}

	#[test]
	fn renders_comment_entries_without_link_thumbnail() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![test_comment(
				"comment_one",
				"demo_sub",
				"<div class=\"md\"><p>This is a reply.</p></div>",
				"t1",
				"Thread title",
			)],
			cached_post_ids: Vec::new(),
			user_subreddits: vec![
				super::UserSubredditEntry {
					name: "demo_sub".to_string(),
					count: 2,
				},
				super::UserSubredditEntry {
					name: "other_sub".to_string(),
					count: 1,
				},
			],
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("Reply to comment in"));
		assert!(html.contains("Thread title"));
		assert!(html.contains("View thread"));
		assert!(!html.contains("post_thumbnail"));
		assert!(html.contains("<a class=\"user_subreddit_link\" href=\"/r/demo_sub\">r/demo_sub</a>"));
		assert!(html.contains("<span class=\"user_subreddit_meta\">2</span>"));
		assert!(html.contains("<a class=\"user_subreddit_link\" href=\"/r/other_sub\">r/other_sub</a>"));
		assert!(html.contains("<span class=\"user_subreddit_meta\">1</span>"));
	}

	#[test]
	fn renders_following_comment_original_and_parent_links() {
		let mut post = test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[]);
		post.following_comments.push(FollowingComment {
			id: "comment_one".to_string(),
			author: "demo_author".to_string(),
			body: "<p>This is the child comment.</p>".to_string(),
			permalink: "/r/demo_sub/comments/thread/comment_one/".to_string(),
			created_ts: 1,
			rel_time: "1h ago".to_string(),
			community: "demo_sub".to_string(),
			score: ("1".to_string(), "1".to_string()),
			parent_id: "parent_one".to_string(),
			parent_kind: "t1".to_string(),
			parent_comment: ParentComment {
				id: "parent_one".to_string(),
				body: "<p>This is the parent comment.</p>".to_string(),
				permalink: "/r/demo_sub/comments/thread/parent_one/".to_string(),
				author: "demo_parent".to_string(),
				rel_time: "2h ago".to_string(),
				expand: false,
			},
			link_title: "Thread title".to_string(),
			link_permalink: "/r/demo_sub/comments/thread/".to_string(),
			link_author: String::new(),
			link_created_ts: 0,
			thread_comments: ("32".to_string(), "32".to_string()),
			thread_nsfw: false,
		});

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![post],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("original comment"));
		assert!(html.contains("view parent"));
		assert!(html.contains("This is the parent comment."));
	}

	#[test]
	fn user_subreddit_tallies_include_collapsed_duplicate_communities() {
		let username = "demo_user_tallies";
		let mut primary = test_post("one", "Primary", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[]);
		primary.collapsed_duplicates = vec![CollapsedDuplicate {
			title: "Duplicate".to_string(),
			permalink: "/r/other_sub/comments/two/".to_string(),
			community: "other_sub".to_string(),
			display_community: "r/other_sub".to_string(),
			created_ts: 0,
			rel_time: "1h ago".to_string(),
			comments: ("1".to_string(), "1".to_string()),
			flair: Flair {
				flair_parts: Vec::new(),
				text: String::new(),
				background_color: String::new(),
				foreground_color: String::new(),
			},
		}];

		crate::utils::store_user_subreddits(username, &[primary]);
		let tallies = crate::utils::cached_user_subreddits(username);

		assert!(tallies.contains(&("demo_sub".to_string(), 1)));
		assert!(tallies.contains(&("other_sub".to_string(), 1)));
	}

	#[test]
	fn user_sidebar_subreddits_prefer_materialized_catalog_stats() {
		let _guard = crate::materialized_catalog::MATERIALIZED_CATALOG_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
		super::clear_test_caches();
		crate::utils::clear_test_caches();
		crate::materialized_catalog::reset_catalog();

		let username = "catalog_sidebar_user";
		let mut first = test_post("one", "First", "rust", "", "self", "", &[]);
		first.author.name = username.to_string();
		let mut second = test_post("two", "Second", "programming", "", "self", "", &[]);
		second.author.name = username.to_string();
		let mut third = test_post("three", "Third", "rust", "", "self", "", &[]);
		third.author.name = username.to_string();

		crate::materialized_catalog::apply_source_refresh(
			&crate::post_cache::FeedSource::subreddit(format!("u_{username}"), crate::post_cache::FeedSort::New),
			&[first, second, third],
		);
		crate::utils::store_user_subreddits(username, &[]);

		let subreddits = super::user_sidebar_subreddits(username);
		assert_eq!(
			subreddits.into_iter().map(|entry| (entry.name, entry.count)).collect::<Vec<_>>(),
			vec![("rust".to_string(), 2), ("programming".to_string(), 1)]
		);
	}

	#[test]
	fn user_sidebar_subreddits_merge_page_tallies_with_partial_catalog_stats() {
		let _guard = crate::materialized_catalog::MATERIALIZED_CATALOG_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
		super::clear_test_caches();
		crate::utils::clear_test_caches();
		crate::materialized_catalog::reset_catalog();

		let username = "partial_catalog_sidebar_user";
		let mut first = test_post("catalog-one", "Catalog one", "kissing_porn", "", "self", "", &[]);
		first.author.name = username.to_string();
		let mut second = test_post("catalog-two", "Catalog two", "kissing_porn", "", "self", "", &[]);
		second.author.name = username.to_string();
		crate::materialized_catalog::apply_source_refresh(
			&crate::post_cache::FeedSource::subreddit(format!("u_{username}"), crate::post_cache::FeedSort::New),
			&[first, second],
		);

		let visible = test_post("visible-one", "Visible one", "NSFWverifiedamateurs", "", "self", "", &[]);
		let overlapping = test_post("visible-two", "Visible two", "kissing_porn", "", "self", "", &[]);
		crate::utils::store_user_subreddits(username, &[visible, overlapping]);

		let subreddits = super::user_sidebar_subreddits(username);
		assert_eq!(
			subreddits.into_iter().map(|entry| (entry.name, entry.count)).collect::<Vec<_>>(),
			vec![("kissing_porn".to_string(), 2), ("NSFWverifiedamateurs".to_string(), 1)]
		);
	}

	#[test]
	fn video_stats_deduplicate_matching_video_urls() {
		let username = "demo_user_video_dedupe";
		let first = test_video_post("one", "Video one", "https://v.redd.it/example/DASH_720.mp4?source=fallback", 31);
		let second = test_video_post("two", "Video two", "https://v.redd.it/example/DASH_720.mp4?foo=bar", 31);

		super::update_user_video_stats(username, &[first, second]);

		assert_eq!(
			super::cached_user_video_stats(username),
			Some(super::UserVideoStats {
				average_duration_seconds: 31,
				video_count: 1,
			})
		);
	}

	#[test]
	fn video_stats_deduplicate_matching_content_fingerprints() {
		let username = "demo_user_video_content_dedupe";
		let mut first = test_video_post("one", "Video one", "https://v.redd.it/first/DASH_720.mp4?source=fallback", 31);
		first.media.content_fingerprint = "same-video-content".to_string();
		let mut second = test_video_post("two", "Video two", "https://v.redd.it/second/DASH_720.mp4?source=fallback", 90);
		second.media.content_fingerprint = "same-video-content".to_string();

		super::update_user_video_stats(username, &[first, second]);

		assert_eq!(
			super::cached_user_video_stats(username),
			Some(super::UserVideoStats {
				average_duration_seconds: 31,
				video_count: 1,
			})
		);
	}

	#[test]
	fn video_stats_include_distinct_videos_in_average() {
		let username = "demo_user_video_average";
		let first = test_video_post("one", "Video one", "https://v.redd.it/first/DASH_720.mp4", 30);
		let second = test_video_post("two", "Video two", "https://v.redd.it/second/DASH_720.mp4", 90);

		super::update_user_video_stats(username, &[first, second]);

		assert_eq!(
			super::cached_user_video_stats(username),
			Some(super::UserVideoStats {
				average_duration_seconds: 60,
				video_count: 2,
			})
		);
		assert_eq!(super::cached_avg_video_length(username), Some(("1:00".to_string(), "from 2 seen videos".to_string())));
	}

	#[test]
	fn avg_video_length_flags_multiple_redgifs_uploaders() {
		let _guard = crate::materialized_catalog::MATERIALIZED_CATALOG_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
		super::clear_test_caches();
		let username = "demo_user_multi_redgifs";
		let mut first = test_video_post("one", "One", "/redgifs/oneclip", 60);
		first.media.redgifs_username = "alice".to_string();
		let mut second = test_video_post("two", "Two", "/redgifs/twoclip", 60);
		second.media.redgifs_username = "bob".to_string();

		super::update_user_video_stats(username, &[first, second]);

		let meta = super::cached_avg_video_length(username).unwrap().1;
		assert!(meta.contains("2 rg profiles"), "expected short rg hint in {meta:?}");
		assert_eq!(super::cached_video_signal(username).redgifs_usernames, vec!["alice".to_string(), "bob".to_string()]);
	}

	#[test]
	fn avg_video_length_merges_materialized_catalog_redgifs_uploaders() {
		let _guard = crate::materialized_catalog::MATERIALIZED_CATALOG_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
		super::clear_test_caches();
		crate::materialized_catalog::reset_catalog();

		let username = "demo_user_catalog_multi_redgifs";
		let mut first = test_video_post("one", "One", "/redgifs/oneclip", 60);
		first.author.name = username.to_string();
		first.media.redgifs_username = "alice".to_string();
		let mut second = test_video_post("two", "Two", "/redgifs/twoclip", 60);
		second.author.name = username.to_string();
		second.media.redgifs_username = "bob".to_string();

		super::update_user_video_stats(username, std::slice::from_ref(&first));
		crate::materialized_catalog::apply_source_refresh(
			&crate::post_cache::FeedSource::subreddit(format!("u_{username}"), crate::post_cache::FeedSort::New),
			&[first, second],
		);

		let stats = super::cached_avg_video_length(username).unwrap();
		assert_eq!(stats.0, "1:00");
		assert!(stats.1.contains("2 rg profiles"), "expected catalog rg hint in {:?}", stats.1);

		crate::materialized_catalog::reset_catalog();
	}

	#[test]
	fn avg_video_length_omits_rg_profiles_when_single_redgifs_uploader() {
		let _guard = crate::materialized_catalog::MATERIALIZED_CATALOG_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
		super::clear_test_caches();
		let username = "demo_user_one_rg";
		let mut post = test_video_post("one", "One", "/redgifs/oneclip", 60);
		post.media.redgifs_username = "samehandle".to_string();

		super::update_user_video_stats(username, &[post]);

		let meta = super::cached_avg_video_length(username).unwrap().1;
		assert!(!meta.contains("rg profiles"), "unexpected rg suffix in {meta:?}");
	}

	#[test]
	fn avg_video_length_rg_profile_classes_have_two_penalty_levels() {
		assert_eq!(
			super::avg_video_length_rg_profile_class(&Some(("1:00".to_string(), "from 2 seen videos · 2 rg profiles".to_string()))),
			" post_rg_profiles_2"
		);
		assert_eq!(
			super::avg_video_length_rg_profile_class(&Some(("1:00".to_string(), "from 3 seen videos · 3 rg profiles".to_string()))),
			" post_rg_profiles_3plus"
		);
		assert_eq!(super::avg_video_length_rg_profile_class(&Some(("1:00".to_string(), "from 2 seen videos".to_string()))), "");
	}

	#[test]
	fn video_stats_ignore_zero_duration_posts() {
		let username = "demo_user_zero_duration";
		let post = test_video_post("one", "Video one", "https://v.redd.it/example/DASH_720.mp4", 0);

		super::update_user_video_stats(username, &[post]);

		assert_eq!(super::cached_user_video_stats(username), None);
	}

	#[test]
	fn video_stats_ignore_non_video_posts() {
		let username = "demo_user_non_video";
		let image = test_post("one", "Image", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[]);
		let comment = test_comment("comment_one", "demo_sub", "body", "t1", "Thread title");

		super::update_user_video_stats(username, &[image, comment]);

		assert_eq!(super::cached_user_video_stats(username), None);
	}

	#[test]
	fn video_stats_deduplicate_matching_redgifs_sources() {
		let username = "demo_user_redgifs_dedupe";
		let mut first = test_video_post("one", "Video one", "/redgifs/zanyjudiciousrook", 31);
		first.domain = "www.redgifs.com".to_string();

		let mut second = test_video_post("two", "Video two", "", 31);
		second.post_type = "gif".to_string();
		second.media.embed_code = "<iframe src=\"https://www.redgifs.com/ifr/zanyjudiciousrook/\" frameborder=\"0\"></iframe>".to_string();

		super::update_user_video_stats(username, &[first, second]);

		assert_eq!(
			super::cached_user_video_stats(username),
			Some(super::UserVideoStats {
				average_duration_seconds: 31,
				video_count: 1,
			})
		);
	}

	#[test]
	fn renders_avg_video_length_in_user_template() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[])],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: Some(("1:01".to_string(), "from 2 seen videos".to_string())),
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("Avg video length"));
		assert!(html.contains("1:01"));
		assert!(html.contains("from 2 seen videos"));
	}

	#[test]
	fn omits_avg_video_length_when_absent() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[])],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(!html.contains("Avg video length"));
	}

	#[test]
	fn renders_recent_coomer_posts_in_user_sidebar() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[])],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: Some(crate::utils::CoomerRecentPostsPanel {
				filtered_to_video: true,
				items: vec![crate::utils::CoomerRecentPostItem {
					title: "A much longer Coomer title".to_string(),
					truncated_title: "A much longer Coomer title".to_string(),
					url: "https://coomer.st/onlyfans/user/demo/post/1".to_string(),
					published: "2025-09-10".to_string(),
					has_video: true,
				}],
			}),
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("Recent creator videos"));
		assert!(html.contains("A much longer Coomer title"));
		assert!(html.contains("2025-09-10"));
		assert!(html.contains("user_coomer_post_badge"));
	}

	#[test]
	fn renders_recent_coomer_posts_even_when_reddit_posts_are_hidden_for_nsfw() {
		let mut prefs = test_prefs();
		prefs.show_nsfw = "off".to_string();

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[])],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: Some(crate::utils::CoomerRecentPostsPanel {
				filtered_to_video: true,
				items: vec![crate::utils::CoomerRecentPostItem {
					title: "Visible from coomer".to_string(),
					truncated_title: "Visible from coomer".to_string(),
					url: "https://coomer.st/onlyfans/user/demo/post/1".to_string(),
					published: "2025-09-10".to_string(),
					has_video: true,
				}],
			}),
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs,
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: true,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("All posts are hidden because they are NSFW"));
		assert!(html.contains("Recent creator videos"));
		assert!(html.contains("Visible from coomer"));
	}

	#[test]
	fn renders_cached_author_avg_video_length_next_to_external_links() {
		let mut post = test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[]);
		post.author.external_links = vec![crate::utils::ExternalLink {
			label: "Example".to_string(),
			url: "https://example.com/demo".to_string(),
			host: "example.com".to_string(),
			icon_path: "/icon/example.com".to_string(),
			has_coomer: false,
		}];
		post.author.avg_video_length = Some(("1:01".to_string(), "from 2 seen videos · 2 rg profiles".to_string()));

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![post],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("class=\"author_avg_video_length\""));
		assert!(html.contains("data-state=\"loaded\""));
		assert!(html.contains("post_rg_profiles_2"));
		assert!(html.contains("avg 1:01 · rg 2"));
		assert!(html.contains("from 2 seen videos · 2 rg profiles"));
	}

	#[test]
	fn cached_author_links_without_avg_video_length_remain_hydratable() {
		let mut post = test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[]);
		post.author.external_links = vec![crate::utils::ExternalLink {
			label: "Example".to_string(),
			url: "https://example.com/demo".to_string(),
			host: "example.com".to_string(),
			icon_path: "/icon/example.com".to_string(),
			has_coomer: false,
		}];
		post.author.avg_video_length = None;

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			username: "demo_user".to_string(),
			posts: vec![post],
			cached_post_ids: Vec::new(),
			user_subreddits: Vec::new(),
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("class=\"external_site_icons\""));
		assert!(html.contains("data-state=\"pending\""));
		assert!(!html.contains("class=\"author_avg_video_length\""));
	}

	#[test]
	fn renders_user_follow_and_filter_state_case_insensitively() {
		let mut prefs = test_prefs();
		prefs.subscriptions = vec!["u_demouser".to_string(), "demosub".to_string()];
		prefs.filters = vec!["u_demouser".to_string()];
		prefs.subscriptions_lc = prefs.subscriptions.iter().map(|s| s.to_ascii_lowercase()).collect();
		prefs.filters_lc = prefs.filters.iter().map(|s| s.to_ascii_lowercase()).collect();

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "DemoUser".to_string(),
				..Default::default()
			},
			username: "DemoUser".to_string(),
			posts: vec![test_comment(
				"comment_one",
				"DemoSub",
				"<div class=\"md\"><p>This is a reply.</p></div>",
				"t1",
				"Thread title",
			)],
			cached_post_ids: Vec::new(),
			user_subreddits: vec![super::UserSubredditEntry {
				name: "DemoSub".to_string(),
				count: 1,
			}],
			coomer_recent_posts: None,
			fallback_notice: String::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs,
			url: "/user/DemoUser".to_string(),
			redirect_url: "user/DemoUser".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			shell_mode: false,
			fragment_url: String::new(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			loading_message: String::new(),
			replace_selector: "#column_one".to_string(),
			video_mode_enabled: false,
			video_mode_sidebar: super::empty_video_mode_sidebar(),
		}
		.render()
		.unwrap();

		assert!(html.contains("action=\"/r/u_DemoUser/unsubscribe"));
		assert!(html.contains("<button class=\"unsubscribe\">Unfollow</button>"));
		assert!(html.contains("action=\"/r/u_DemoUser/unfilter"));
		assert!(html.contains("<button class=\"unfilter\">Unfilter</button>"));
		assert!(!html.contains("action=\"/r/DemoSub/subscribe"));
	}

	#[tokio::test]
	async fn author_link_cache_miss_returns_without_profile_enrichment() {
		let req = Request::builder().uri("/author_links?names=definitely_missing_cached_author").body(Body::empty()).unwrap();
		let response = super::author_links(req).await.unwrap();
		let body = hyper::body::to_bytes(response.into_body()).await.unwrap();
		let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();

		assert_eq!(payload["definitely_missing_cached_author"]["links"], serde_json::json!([]));
		assert!(payload["definitely_missing_cached_author"]["avg_video_length"].is_null());
	}
}

// FUNCTIONS
pub async fn profile(req: Request<Body>) -> Result<Response<Body>, String> {
	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());
	let username = req.param("name").unwrap_or_else(|| "reddit".to_string());
	let default_sort = default_user_sort(&req);
	let query = req.uri().query().unwrap_or_default();
	let video_listing = is_video_listing(&listing);

	// Build the Reddit JSON API path
	let path = user_listing_path(&username, &listing, query, &default_sort);
	let url = user_reddit_path(&username, &listing, query);
	let current_url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let redirect_url = current_url[1..].replace('?', "%3F").replace('&', "%26");

	// Retrieve other variables from Redlib request
	let sort = param(&path, "sort").unwrap_or_default();
	let prefs = Preferences::new(&req);
	let filters = get_filters(&req);
	let user_filter = ["u_", &username].concat();
	let is_filtered = filters.contains(&user_filter);
	let user_subreddits = user_sidebar_subreddits(&username);
	let avg_video_length = cached_avg_video_length(&username);
	let shell_user = cached_or_placeholder_user(&username);
	let cached_shell_merge = if !video_listing && request_is_first_user_page(query) {
		merge_user_posts_with_cached_posts(Vec::new(), cached_user_posts_for_listing(&username, &listing, &filters), &filters)
	} else {
		UserPostMerge::default()
	};
	let shell_all_posts_hidden_nsfw = !cached_shell_merge.posts.is_empty() && cached_shell_merge.posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on";
	let video_mode_sidebar = if video_listing {
		cached_video_mode_sidebar(&req, &username, &url, query, &filters)
	} else {
		empty_video_mode_sidebar()
	};

	Ok(template(&UserTemplate {
		user: shell_user,
		username: username.clone(),
		posts: cached_shell_merge.posts,
		cached_post_ids: cached_shell_merge.cached_post_ids,
		user_subreddits,
		coomer_recent_posts: None,
		fallback_notice: String::new(),
		sort: (sort, param(&path, "t").unwrap_or_default()),
		ends: (param(&path, "after").unwrap_or_default(), String::new()),
		listing: listing.clone(),
		prefs,
		url: url.clone(),
		redirect_url,
		is_filtered,
		all_posts_filtered: cached_shell_merge.all_cached_posts_filtered,
		all_posts_hidden_nsfw: shell_all_posts_hidden_nsfw,
		avg_video_length,
		no_posts: false,
		shell_mode: true,
		fragment_url: user_fragment_path(&username, &listing, query),
		replace_selector: "#column_one".to_string(),
		prev_page_url: String::new(),
		next_page_url: String::new(),
		loading_message: if video_listing {
			"Loading videos...".to_string()
		} else {
			"Loading user page...".to_string()
		},
		video_mode_enabled: video_listing,
		video_mode_sidebar,
	}))
}

pub async fn profile_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());
	let username = req.param("name").unwrap_or_else(|| "reddit".to_string());
	let default_sort = default_user_sort(&req);
	let query = req.uri().query().unwrap_or_default();
	let video_listing = is_video_listing(&listing);

	fn join_notices(primary: &str, secondary: &str) -> String {
		match (primary.trim().is_empty(), secondary.trim().is_empty()) {
			(true, true) => String::new(),
			(false, true) => primary.to_string(),
			(true, false) => secondary.to_string(),
			(false, false) => format!("{primary} {secondary}"),
		}
	}

	let path = user_listing_path(&username, &listing, query, &default_sort);
	let url = user_reddit_path(&username, &listing, query);
	let current_url = if query.is_empty() { url.clone() } else { format!("{url}") };
	let redirect_url = current_url.trim_start_matches('/').replace('?', "%3F").replace('&', "%26");
	let sort = param(&path, "sort").unwrap_or_default();
	let request_after = param(&format!("?{query}"), "after").unwrap_or_default();
	let request_before = param(&format!("?{query}"), "before").unwrap_or_default();
	let mut pre_peer = if !video_listing
		&& !peer_api_fallback::request_is_peer_api_fallback(&req)
		&& (peer_api_fallback::request_should_force_peer_api_fallback(&req) || instance_scrape::reddit_rate_limited_now())
	{
		info!("user: using configured Redlib peer API for {} before a local Reddit request", current_url);
		match peer_api_fallback::fetch_user_listing(&username, &listing, &sort, Some(query)).await {
			Ok(peer) => Some(peer),
			Err(peer_error) => {
				warn!("user: preemptive Redlib peer API fallback failed for {}: {}", current_url, peer_error);
				None
			}
		}
	} else {
		None
	};
	let pre_scraped = if pre_peer.is_none() && instance_scrape::instance_scrape_enabled() && instance_scrape::reddit_rate_limited_now() {
		info!("user: using scrape fallback for {} because Reddit is currently rate limited", current_url);
		match instance_scrape::scrape_user_page(&current_url).await {
			Ok(scraped) => Some(scraped),
			Err(scrape_error) => {
				warn!("user: preemptive scrape fallback failed for {}: {}", current_url, scrape_error);
				None
			}
		}
	} else {
		None
	};
	let user = pre_peer
		.as_ref()
		.map(|peer| peer.user.clone())
		.or_else(|| pre_scraped.as_ref().map(|scraped| scraped.user.clone()))
		.unwrap_or_else(|| cached_or_placeholder_user(&username));
	let mut status_notice = String::new();
	if !video_listing && pre_peer.is_none() && pre_scraped.is_none() && !peer_api_fallback::request_is_peer_api_fallback(&req) && instance_scrape::reddit_rate_limited_now() {
		info!("user: quota state requires configured Redlib peer API before loading listing {}", current_url);
		match peer_api_fallback::fetch_user_listing(&username, &listing, &sort, Some(query)).await {
			Ok(peer) => {
				pre_peer = Some(peer);
			}
			Err(peer_error) => warn!("user: preemptive Redlib peer API fallback failed for {}: {}", current_url, peer_error),
		}
	}
	let coomer_recent_posts: Option<utils::CoomerRecentPostsPanel> = None;

	if video_listing || (listing == "submitted" && sort == "new" && request_after.is_empty() && request_before.is_empty()) {
		crate::post_cache::remember_subreddit_request(&format!("u_{username}"));
	}

	let user_visit_signal_page = param(&format!("?{query}"), "cache_page").and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
	if (video_listing || (listing == "submitted" && sort == "new" && request_after.is_empty() && request_before.is_empty())) && user_visit_signal_page == 0 {
		crate::post_cache::record_remembered_feed_visit_signal(&crate::post_cache::FeedSource::subreddit(format!("u_{username}"), crate::post_cache::FeedSort::New));
	}

	let filters = get_filters(&req);
	let fragment = if filters.contains(&["u_", &username].concat()) {
		UserFragmentTemplate {
			user,
			posts: Vec::new(),
			cached_post_ids: Vec::new(),
			user_subreddits: user_sidebar_subreddits(&username),
			coomer_recent_posts,
			fallback_notice: status_notice.clone(),
			sort: (sort, param(&path, "t").unwrap_or_default()),
			ends: (param(&path, "after").unwrap_or_default(), String::new()),
			listing,
			prefs: Preferences::new(&req),
			url,
			redirect_url,
			is_filtered: true,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			avg_video_length: None,
			no_posts: false,
			prev_page_url: String::new(),
			next_page_url: String::new(),
			video_mode_enabled: false,
			video_mode_sidebar: empty_video_mode_sidebar(),
		}
	} else if video_listing {
		let (source_posts, source_posts_from_cache) = match cached_or_live_user_video_posts(&username).await {
			Ok(posts) => posts,
			Err(msg) => {
				if let Some(notice) = user_upstream_unavailable_notice(&username, &msg) {
					status_notice = join_notices(&status_notice, &notice);
					(cached_user_posts_for_listing(&username, "videos", &filters), true)
				} else {
					return Err(msg);
				}
			}
		};
		let cookie_value = crate::utils::read_sharded_cookie(&req, video_mode::VIDEO_MODE_QUERY_COOKIE);
		let parsed_query = video_mode::parse_query_or_cookie(query, &cookie_value);
		let listing_data = video_mode::build_listing(&source_posts, &filters, &parsed_query, now_unix());
		let normalized_query = normalized_video_mode_query(&parsed_query, &listing_data);
		let request_has_video_params = video_mode::query_has_video_params(query);
		let cache_page = param(&format!("?{query}"), "cache_page").and_then(|page| page.parse::<usize>().ok()).unwrap_or(0);
		let start = cache_page.saturating_mul(crate::post_cache::POSTS_PER_PAGE);
		let end = (start + crate::post_cache::POSTS_PER_PAGE).min(listing_data.posts.len());
		let posts = if start < listing_data.posts.len() {
			listing_data.posts[start..end].to_vec()
		} else {
			Vec::new()
		};
		let no_posts = listing_data.posts.is_empty();
		let all_posts_hidden_nsfw = !posts.is_empty() && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
		let eligible_video_posts = source_posts
			.iter()
			.filter(|post| !filters.contains(&post.community) && !filters.contains(&format!("u_{}", post.author.name)))
			.filter(|post| !crate::utils::is_deleted_reddit_placeholder_post(post))
			.filter(|post| video_mode::is_video_post(post))
			.count();
		let avg_video_length = cached_avg_video_length(&username);
		let mut posts = posts;
		utils::hydrate_post_author_links_from_cache(&mut posts);
		let cached_post_ids = if source_posts_from_cache {
			posts.iter().map(cached_post_identity).filter(|identity| !identity.is_empty()).collect()
		} else {
			Vec::new()
		};
		let video_mode_sidebar = build_video_mode_sidebar(&url, &listing_data, &posts, &normalized_query);

		UserFragmentTemplate {
			user: user.clone(),
			posts,
			cached_post_ids,
			user_subreddits: user_sidebar_subreddits(&username),
			coomer_recent_posts,
			fallback_notice: status_notice.clone(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing,
			prefs: Preferences::new(&req),
			url: url.clone(),
			redirect_url,
			is_filtered: false,
			all_posts_filtered: eligible_video_posts > 0 && listing_data.posts.is_empty(),
			all_posts_hidden_nsfw,
			avg_video_length,
			no_posts,
			prev_page_url: if cache_page > 0 {
				video_mode_page_url(&url, cache_page.saturating_sub(1), &normalized_query, request_has_video_params)
			} else {
				String::new()
			},
			next_page_url: if end < listing_data.posts.len() {
				video_mode_page_url(&url, cache_page + 1, &normalized_query, request_has_video_params)
			} else {
				String::new()
			},
			video_mode_enabled: true,
			video_mode_sidebar,
		}
	} else {
		let first_user_page = request_is_first_user_page(query);
		let cacheable_submitted_first_page = listing == "submitted" && sort == "new" && first_user_page;
		let cacheable_comments_first_page = listing == "comments" && sort == "new" && first_user_page;
		let merge_cached_posts = |posts: Vec<Post>, all_live_posts_filtered: bool| -> (Vec<Post>, Vec<String>, bool) {
			if !first_user_page {
				return (posts, Vec::new(), all_live_posts_filtered);
			}

			let merged = merge_user_posts_with_cached_posts(posts, cached_user_posts_for_listing(&username, &listing, &filters), &filters);
			let all_posts_filtered = if merged.posts.is_empty() {
				all_live_posts_filtered || merged.all_cached_posts_filtered
			} else {
				false
			};
			(merged.posts, merged.cached_post_ids, all_posts_filtered)
		};
		let store_live_listing_cache = |posts: &[Post], after: &str| {
			if cacheable_submitted_first_page {
				crate::post_cache::store_source_posts_for_cache(
					&crate::post_cache::FeedSource::subreddit(format!("u_{username}"), crate::post_cache::FeedSort::New),
					posts.to_vec(),
					after.to_string(),
					Some(crate::post_cache::RefreshOrigin::DirectLiveFetch),
				);
			}
			if cacheable_comments_first_page {
				crate::post_cache::store_source_posts_for_cache(
					&crate::post_cache::FeedSource::user_comments(format!("u_{username}"), crate::post_cache::FeedSort::New),
					posts.to_vec(),
					after.to_string(),
					Some(crate::post_cache::RefreshOrigin::DirectLiveFetch),
				);
			}
		};
		let mut fallback_notice = String::new();
		let maybe_scraped = pre_scraped;
		let (fragment_user, posts, cached_post_ids, ends, prev_page_url, next_page_url, all_posts_filtered) = if let Some(peer) = pre_peer {
			store_user_metadata(&username, &peer.user);
			utils::store_post_flairs(&peer.posts);
			utils::store_user_subreddits(&username, &peer.posts);
			crate::materialized_catalog::record_author_history_from_posts(&peer.posts);
			store_live_listing_cache(&peer.posts, &peer.after);
			let mut peer_posts = peer.posts;
			let (_, all_peer_posts_filtered) = filter_posts(&mut peer_posts, &filters);
			let (posts, cached_post_ids, all_posts_filtered) = merge_cached_posts(peer_posts, all_peer_posts_filtered);
			fallback_notice = join_notices(&status_notice, peer_api_fallback::peer_api_notice());
			(
				peer.user,
				posts,
				cached_post_ids,
				(String::new(), peer.after),
				String::new(),
				String::new(),
				all_posts_filtered,
			)
		} else if let Some(scraped) = maybe_scraped {
			store_user_metadata(&username, &scraped.user);
			utils::store_post_flairs(&scraped.posts);
			utils::store_user_subreddits(&username, &scraped.posts);
			crate::materialized_catalog::record_author_history_from_posts(&scraped.posts);
			store_live_listing_cache(&scraped.posts, &scraped.after);
			let mut scraped_posts = scraped.posts.clone();
			let (_, all_scraped_posts_filtered) = filter_posts(&mut scraped_posts, &filters);
			let (posts, cached_post_ids, all_posts_filtered) = merge_cached_posts(scraped_posts, all_scraped_posts_filtered);
			fallback_notice = join_notices(&status_notice, instance_scrape::scrape_notice());
			(
				scraped.user,
				posts,
				cached_post_ids,
				(String::new(), scraped.after),
				String::new(),
				String::new(),
				all_posts_filtered,
			)
		} else {
			match fetch_user_listing_posts(&username, &path, &listing, &filters).await {
				Ok((posts, after, all_posts_filtered)) => {
					store_live_listing_cache(&posts, &after);
					let (posts, cached_post_ids, all_posts_filtered) = merge_cached_posts(posts, all_posts_filtered);
					(
						user.clone(),
						posts,
						cached_post_ids,
						(param(&path, "after").unwrap_or_default(), after),
						String::new(),
						String::new(),
						all_posts_filtered,
					)
				}
				Err(msg) if user_upstream_unavailable_notice(&username, &msg).is_some() => {
					let notice = user_upstream_unavailable_notice(&username, &msg).unwrap_or_default();
					fallback_notice = join_notices(&fallback_notice, &notice);
					let merged = merge_user_posts_with_cached_posts(Vec::new(), cached_user_posts_for_listing(&username, &listing, &filters), &filters);
					(
						user.clone(),
						merged.posts,
						merged.cached_post_ids,
						(String::new(), String::new()),
						String::new(),
						String::new(),
						merged.all_cached_posts_filtered,
					)
				}
				Err(msg) if !peer_api_fallback::request_is_peer_api_fallback(&req) && instance_scrape::should_attempt_scrape_fallback(&msg) => {
					info!("user: retrying {} via configured Redlib peer API after listing error: {}", current_url, msg);
					match peer_api_fallback::fetch_user_listing(&username, &listing, &sort, Some(query)).await {
						Ok(peer) => {
							store_user_metadata(&username, &peer.user);
							utils::store_post_flairs(&peer.posts);
							utils::store_user_subreddits(&username, &peer.posts);
							crate::materialized_catalog::record_author_history_from_posts(&peer.posts);
							store_live_listing_cache(&peer.posts, &peer.after);
							let mut peer_posts = peer.posts;
							let (_, all_peer_posts_filtered) = filter_posts(&mut peer_posts, &filters);
							let (posts, cached_post_ids, all_posts_filtered) = merge_cached_posts(peer_posts, all_peer_posts_filtered);
							fallback_notice = join_notices(&status_notice, peer_api_fallback::peer_api_notice());
							(
								peer.user,
								posts,
								cached_post_ids,
								(String::new(), peer.after),
								String::new(),
								String::new(),
								all_posts_filtered,
							)
						}
						Err(peer_error) if instance_scrape::instance_scrape_enabled() => {
							warn!("user: Redlib peer API fallback failed for {}: {}; trying scrape fallback", current_url, peer_error);
							let scraped = instance_scrape::scrape_user_page(&current_url).await.map_err(|_| msg.clone())?;
							store_user_metadata(&username, &scraped.user);
							utils::store_post_flairs(&scraped.posts);
							utils::store_user_subreddits(&username, &scraped.posts);
							crate::materialized_catalog::record_author_history_from_posts(&scraped.posts);
							store_live_listing_cache(&scraped.posts, &scraped.after);
							let mut scraped_posts = scraped.posts.clone();
							let (_, all_scraped_posts_filtered) = filter_posts(&mut scraped_posts, &filters);
							let (posts, cached_post_ids, all_posts_filtered) = merge_cached_posts(scraped_posts, all_scraped_posts_filtered);
							fallback_notice = join_notices(&status_notice, instance_scrape::scrape_notice());
							(
								scraped.user,
								posts,
								cached_post_ids,
								(String::new(), scraped.after),
								String::new(),
								String::new(),
								all_posts_filtered,
							)
						}
						Err(peer_error) => {
							warn!("user: Redlib peer API fallback failed for {}: {}", current_url, peer_error);
							return Err(msg);
						}
					}
				}
				Err(msg) => {
					warn!("user: direct listing request failed for {} without usable scrape fallback: {}", current_url, msg);
					return Err(msg);
				}
			}
		};
		fallback_notice = join_notices(&status_notice, &fallback_notice);
		let no_posts = posts.is_empty();
		let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
		let avg_video_length = cached_avg_video_length(&username);
		UserFragmentTemplate {
			user: fragment_user,
			posts,
			cached_post_ids,
			user_subreddits: user_sidebar_subreddits(&username),
			coomer_recent_posts,
			fallback_notice,
			sort: (sort, param(&path, "t").unwrap_or_default()),
			ends,
			listing,
			prefs: Preferences::new(&req),
			url,
			redirect_url,
			is_filtered: false,
			all_posts_filtered,
			all_posts_hidden_nsfw,
			avg_video_length,
			no_posts,
			prev_page_url,
			next_page_url,
			video_mode_enabled: false,
			video_mode_sidebar: empty_video_mode_sidebar(),
		}
	};

	let mut fragment = fragment;
	utils::hydrate_post_author_links_from_cache(&mut fragment.posts);
	utils::refresh_post_times(&mut fragment.posts);
	crate::observed_semantic_backfill::enqueue_observed_user_posts(
		&username,
		&format!("user:{username}:{}:direct-page", fragment.listing),
		&fragment.posts,
		crate::observed_semantic_backfill::ObservedBackfillPriority::Opportunistic,
	);
	let body = fragment.render().map_err(|error| error.to_string())?;
	Response::builder()
		.status(200)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

pub async fn sidebar_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());
	let username = req.param("name").unwrap_or_else(|| "reddit".to_string());
	let query = req.uri().query().unwrap_or_default();
	let url = user_reddit_path(&username, &listing, query);
	let redirect_url = url.trim_start_matches('/').replace('?', "%3F").replace('&', "%26");
	let prefs = Preferences::new(&req);
	let filters = get_filters(&req);
	let is_filtered = filters.contains(&["u_", &username].concat());
	let video_listing = is_video_listing(&listing);

	let mut sidebar_user = match tokio::time::timeout(StdDuration::from_secs(12), user(&username)).await {
		Ok(Ok(user)) => user,
		Ok(Err(msg)) => {
			info!("user sidebar: profile metadata unavailable for u/{username}: {msg}");
			cached_or_placeholder_user(&username)
		}
		Err(_) => {
			warn!("user sidebar: profile metadata timed out for u/{username}");
			cached_or_placeholder_user(&username)
		}
	};
	let _ = tokio::time::timeout(StdDuration::from_secs(3), utils::hydrate_user_external_links_from_author_cache(&mut sidebar_user)).await;
	let coomer_recent_posts = tokio::time::timeout(StdDuration::from_secs(6), utils::creator_recent_posts_panel(&username, &sidebar_user.external_links))
		.await
		.unwrap_or(None);
	let video_mode_sidebar = if video_listing {
		cached_video_mode_sidebar(&req, &username, &url, query, &filters)
	} else {
		empty_video_mode_sidebar()
	};

	let body = UserSidebarTemplate {
		user: sidebar_user,
		user_subreddits: user_sidebar_subreddits(&username),
		coomer_recent_posts,
		prefs,
		redirect_url,
		is_filtered,
		avg_video_length: cached_avg_video_length(&username),
		video_mode_enabled: video_listing,
		video_mode_sidebar,
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

async fn parse_user_metadata(name: &str, res: &serde_json::Value, enrich_external_links: bool) -> User {
	// Grab creation date as unix timestamp
	let created_unix = res["data"]["created"].as_f64().unwrap_or(0.0).round() as i64;
	let created = OffsetDateTime::from_unix_timestamp(created_unix).unwrap_or(OffsetDateTime::UNIX_EPOCH);
	let profile = if enrich_external_links {
		utils::user_profile_details_with_social_links(name, &res["data"]["subreddit"]).await
	} else {
		utils::user_profile_details(&res["data"]["subreddit"])
	};
	let external_links = if enrich_external_links {
		tokio::time::timeout(StdDuration::from_secs(6), utils::external_links_with_coomer_lookups(profile.external_links.clone()))
			.await
			.unwrap_or_else(|_| profile.external_links.clone())
	} else {
		profile.external_links
	};

	// Closure used to parse JSON from Reddit APIs
	let about = |item| res["data"]["subreddit"][item].as_str().unwrap_or_default().to_string();

	// Parse the JSON output into a User struct
	User {
		name: res["data"]["name"].as_str().unwrap_or(name).to_owned(),
		title: about("title"),
		icon: format_url(&about("icon_img")),
		post_karma: format_num(res["data"]["link_karma"].as_i64().unwrap_or(0)),
		comment_karma: format_num(res["data"]["comment_karma"].as_i64().unwrap_or(0)),
		created: created.format(format_description!("[month repr:short] [day] '[year repr:last_two]")).unwrap_or_default(),
		banner: about("banner_img"),
		description: profile.blurb,
		bio_html: profile.bio_html,
		external_links,
		nsfw: res["data"]["subreddit"]["over_18"].as_bool().unwrap_or_default(),
	}
}

async fn fetch_user_metadata_with_mode(name: &str, enrich_external_links: bool) -> Result<User, String> {
	// Build the Reddit JSON API path
	let path: String = format!("/user/{name}/about.json?raw_json=1");

	// Send a request to the url
	let res = json(path, false).await?;
	let user = parse_user_metadata(name, &res, enrich_external_links).await;
	store_user_metadata(name, &user);
	Ok(user)
}

async fn fetch_user_metadata_fresh(name: &str) -> Result<User, String> {
	fetch_user_metadata_with_mode(name, true).await
}

pub(crate) async fn fetch_user_metadata_status(name: &str) -> Result<User, String> {
	fetch_user_metadata_with_mode(name, false).await
}

// USER
async fn user(name: &str) -> Result<User, String> {
	if let Some(cached) = cached_user_metadata(name) {
		return Ok(cached);
	}

	fetch_user_metadata_fresh(name).await
}

pub async fn rss(req: Request<Body>) -> Result<Response<Body>, String> {
	if config::get_setting("REDLIB_ENABLE_RSS").is_none() {
		return Ok(error(req, "RSS is disabled on this instance.").await.unwrap_or_default());
	}
	use crate::utils::rewrite_urls;
	use hyper::header::CONTENT_TYPE;
	use rss::{ChannelBuilder, Item};

	// Get user
	let user_str = req.param("name").unwrap_or_default();

	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());

	// Get path
	let path = format!("/user/{user_str}/{listing}.json?{}&raw_json=1", req.uri().query().unwrap_or_default(),);

	// Get user
	let user_obj = user(&user_str).await.unwrap_or_default();

	// Get posts
	let (posts, _) = Post::fetch(&path, false).await?;

	// Build the RSS feed
	let channel = ChannelBuilder::default()
		.title(user_str)
		.description(user_obj.description)
		.items(
			posts
				.into_iter()
				.map(|post| Item {
					title: Some(post.title.to_string()),
					link: Some(format_url(&utils::get_post_url(&post))),
					author: Some(post.author.name),
					pub_date: Some(DateTime::from_timestamp(post.created_ts as i64, 0).unwrap_or_default().to_rfc2822()),
					content: Some(rewrite_urls(&decode_html(&post.body).unwrap_or_else(|_| post.body.clone()))),
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

pub async fn author_links(req: Request<Body>) -> Result<Response<Body>, String> {
	let names = param(&format!("?{}", req.uri().query().unwrap_or_default()), "names").unwrap_or_default();
	let authors = names
		.split(',')
		.map(str::trim)
		.filter(|name| !name.is_empty() && *name != "[deleted]")
		.take(64)
		.collect::<Vec<_>>();

	let mut pending = JoinSet::new();
	let mut seen = HashSet::new();

	for author in authors {
		if !seen.insert(author.to_string()) {
			continue;
		}

		let author_name = author.to_string();
		pending.spawn(async move {
			// Cached pages must never turn a decorative author-link lookup into
			// one Reddit profile API request per visible post.
			let links = utils::external_links_with_coomer_flags(utils::cached_author_external_links(&author_name)).await;
			let avg_video_length = cached_avg_video_length(&author_name);
			(author_name, links, avg_video_length)
		});
	}

	let mut links_by_author = HashMap::new();
	while let Some(result) = pending.join_next().await {
		if let Ok((author, links, avg_video_length)) = result {
			let rg_profile_count = avg_video_length.as_ref().map(avg_video_length_rg_profile_count).unwrap_or_default();
			links_by_author.insert(
				author,
				serde_json::json!({
					"links": links,
					"avg_video_length": avg_video_length.as_ref().map(|value| value.0.clone()),
					"avg_video_length_label": avg_video_length.as_ref().map(avg_video_length_author_label),
					"avg_video_length_rg_profiles": rg_profile_count,
					"avg_video_length_title": avg_video_length.map(|value| value.1),
				}),
			);
		}
	}

	let body = serde_json::to_string(&links_by_author).map_err(|err| err.to_string())?;
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/json")
			.header("Cache-Control", "no-store")
			.body(body.into())
			.unwrap_or_default(),
	)
}
