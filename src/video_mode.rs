use crate::utils::Post;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use url::form_urlencoded;

pub const VIDEO_SUB_PARAM: &str = "video_sub";
pub const VIDEO_USER_PARAM: &str = "video_user";
pub const VIDEO_SUB_MODE_PARAM: &str = "video_sub_mode";
pub const VIDEO_USER_MODE_PARAM: &str = "video_user_mode";
pub const VIDEO_AGE_MIN_HOURS_PARAM: &str = "video_age_min_hours";
pub const VIDEO_AGE_MAX_HOURS_PARAM: &str = "video_age_max_hours";
pub const VIDEO_MODE_QUERY_COOKIE: &str = "video_mode_query";
const MIN_AGE_BOUND_HOURS: u32 = 24;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacetMode {
	#[default]
	Include,
	Exclude,
	None,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoModeQuery {
	pub selected_subreddits: Vec<String>,
	pub selected_users: Vec<String>,
	pub subreddit_mode: FacetMode,
	pub user_mode: FacetMode,
	pub age_min_hours: u32,
	pub age_max_hours: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoModeFilterOption {
	pub name: String,
	pub count: usize,
	pub page_count: usize,
	pub active: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoModeSidebarData {
	pub action_url: String,
	pub clear_url: String,
	/// `GET /api/video-mode/clear-redirect?redirect=...` — clears the compact cookie and returns to a URL without `video_*` params.
	pub clear_cookie_redirect_href: String,
	pub hidden_query_pairs: Vec<(String, String)>,
	pub subreddit_options: Vec<VideoModeFilterOption>,
	pub user_options: Vec<VideoModeFilterOption>,
	pub clear_subreddits: bool,
	pub clear_users: bool,
	/// Hidden `video_sub_mode`: `""`, `none`, or `exclude` (for legacy GET submits).
	pub video_sub_mode_value: String,
	pub video_user_mode_value: String,
	pub age_min_hours: u32,
	pub age_max_hours: u32,
	pub age_bound_hours: u32,
}

#[derive(Clone, Default, PartialEq)]
pub struct VideoModeListing {
	pub posts: Vec<Post>,
	pub subreddit_options: Vec<VideoModeFilterOption>,
	pub user_options: Vec<VideoModeFilterOption>,
	pub age_min_hours: u32,
	pub age_max_hours: u32,
	pub age_bound_hours: u32,
}

fn normalize_values(values: Vec<String>) -> Vec<String> {
	let mut seen = HashSet::new();
	let mut normalized = Vec::new();
	for value in values {
		let trimmed = value.trim();
		if trimmed.is_empty() {
			continue;
		}

		let folded = trimmed.to_lowercase();
		if seen.insert(folded) {
			normalized.push(trimmed.to_string());
		}
	}
	normalized
}

pub fn parse_query(query: &str) -> VideoModeQuery {
	let mut parsed = VideoModeQuery::default();
	for (key, value) in form_urlencoded::parse(query.as_bytes()).into_owned() {
		match key.as_str() {
			VIDEO_SUB_PARAM => parsed.selected_subreddits.push(value),
			VIDEO_USER_PARAM => parsed.selected_users.push(value),
			VIDEO_SUB_MODE_PARAM => match value.as_str() {
				"none" => parsed.subreddit_mode = FacetMode::None,
				"exclude" => parsed.subreddit_mode = FacetMode::Exclude,
				_ => {}
			},
			VIDEO_USER_MODE_PARAM => match value.as_str() {
				"none" => parsed.user_mode = FacetMode::None,
				"exclude" => parsed.user_mode = FacetMode::Exclude,
				_ => {}
			},
			VIDEO_AGE_MIN_HOURS_PARAM => {
				parsed.age_min_hours = value.parse::<u32>().unwrap_or_default();
			}
			VIDEO_AGE_MAX_HOURS_PARAM => {
				parsed.age_max_hours = value.parse::<u32>().ok();
			}
			_ => {}
		}
	}
	parsed.selected_subreddits = normalize_values(parsed.selected_subreddits);
	parsed.selected_users = normalize_values(parsed.selected_users);
	parsed
}

pub fn query_has_video_params(query: &str) -> bool {
	for (key, _) in form_urlencoded::parse(query.as_bytes()).into_owned() {
		match key.as_str() {
			VIDEO_SUB_PARAM | VIDEO_USER_PARAM | VIDEO_SUB_MODE_PARAM | VIDEO_USER_MODE_PARAM | VIDEO_AGE_MIN_HOURS_PARAM | VIDEO_AGE_MAX_HOURS_PARAM => return true,
			_ => {}
		}
	}
	false
}

fn decode_query_cookie(encoded: &str) -> Result<VideoModeQuery, String> {
	if encoded.trim().is_empty() {
		return Ok(VideoModeQuery::default());
	}
	let bytes = URL_SAFE_NO_PAD.decode(encoded.trim()).map_err(|e| e.to_string())?;
	let json = crate::utils::deflate_decompress(bytes)?;
	serde_json::from_slice(&json).map_err(|e| e.to_string())
}

pub fn parse_query_or_cookie(query: &str, cookie_encoded: &str) -> VideoModeQuery {
	if query_has_video_params(query) {
		return parse_query(query);
	}
	decode_query_cookie(cookie_encoded).unwrap_or_default()
}

pub fn query_pairs(query: &str) -> Vec<(String, String)> {
	form_urlencoded::parse(query.as_bytes()).into_owned().collect()
}

pub fn strip_video_query_pairs(query_pairs: &mut Vec<(String, String)>) {
	query_pairs.retain(|(key, _)| {
		key != VIDEO_SUB_PARAM
			&& key != VIDEO_USER_PARAM
			&& key != VIDEO_SUB_MODE_PARAM
			&& key != VIDEO_USER_MODE_PARAM
			&& key != VIDEO_AGE_MIN_HOURS_PARAM
			&& key != VIDEO_AGE_MAX_HOURS_PARAM
	});
}

pub fn append_query_pairs(query_pairs: &mut Vec<(String, String)>, query: &VideoModeQuery) {
	match query.subreddit_mode {
		FacetMode::None => {
			query_pairs.push((VIDEO_SUB_MODE_PARAM.to_string(), "none".to_string()));
		}
		FacetMode::Exclude => {
			query_pairs.push((VIDEO_SUB_MODE_PARAM.to_string(), "exclude".to_string()));
			for subreddit in &query.selected_subreddits {
				query_pairs.push((VIDEO_SUB_PARAM.to_string(), subreddit.clone()));
			}
		}
		FacetMode::Include => {
			for subreddit in &query.selected_subreddits {
				query_pairs.push((VIDEO_SUB_PARAM.to_string(), subreddit.clone()));
			}
		}
	}
	match query.user_mode {
		FacetMode::None => {
			query_pairs.push((VIDEO_USER_MODE_PARAM.to_string(), "none".to_string()));
		}
		FacetMode::Exclude => {
			query_pairs.push((VIDEO_USER_MODE_PARAM.to_string(), "exclude".to_string()));
			for user in &query.selected_users {
				query_pairs.push((VIDEO_USER_PARAM.to_string(), user.clone()));
			}
		}
		FacetMode::Include => {
			for user in &query.selected_users {
				query_pairs.push((VIDEO_USER_PARAM.to_string(), user.clone()));
			}
		}
	}
	query_pairs.push((VIDEO_AGE_MIN_HOURS_PARAM.to_string(), query.age_min_hours.to_string()));
	if let Some(max_hours) = query.age_max_hours {
		query_pairs.push((VIDEO_AGE_MAX_HOURS_PARAM.to_string(), max_hours.to_string()));
	}
}

pub fn serialize_query_pairs(query_pairs: &[(String, String)]) -> String {
	form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish()
}

pub fn normalize_selected_values(selected: &[String], options: &[VideoModeFilterOption]) -> Vec<String> {
	if selected.is_empty() || selected.len() != options.len() {
		return selected.to_vec();
	}

	let selected_values = selected.iter().map(|value| value.to_lowercase()).collect::<HashSet<_>>();
	let option_values = options.iter().map(|option| option.name.to_lowercase()).collect::<HashSet<_>>();
	if selected_values == option_values {
		Vec::new()
	} else {
		selected.to_vec()
	}
}

pub fn is_video_post(post: &Post) -> bool {
	matches!(post.post_type.as_str(), "video" | "gif") && post.media.duration_seconds > 0
}

pub fn build_listing(posts: &[Post], filters: &HashSet<String>, query: &VideoModeQuery, now_unix: u64) -> VideoModeListing {
	let eligible_posts = posts
		.iter()
		.filter(|post| !filters.contains(&post.community) && !filters.contains(&format!("u_{}", post.author.name)))
		.filter(|post| !crate::utils::is_deleted_reddit_placeholder_post(post))
		.filter(|post| is_video_post(post))
		.cloned()
		.collect::<Vec<_>>();

	let age_bound_hours = derive_age_bound_hours(&eligible_posts, now_unix);
	let (age_min_hours, age_max_hours) = clamp_age_band(query, age_bound_hours);

	let age_filtered = eligible_posts
		.into_iter()
		.filter(|post| {
			let age_hours = post_age_hours(post, now_unix);
			age_hours >= age_min_hours && age_hours <= age_max_hours
		})
		.collect::<Vec<_>>();

	let selected_subreddits = query.selected_subreddits.iter().map(|value| value.to_lowercase()).collect::<HashSet<_>>();
	let selected_users = query.selected_users.iter().map(|value| value.to_lowercase()).collect::<HashSet<_>>();

	let subreddit_options = facet_options(&age_filtered, |post| post.community.clone(), &selected_subreddits, query.subreddit_mode);
	let user_options = facet_options(&age_filtered, |post| post.author.name.clone(), &selected_users, query.user_mode);

	let mut filtered_posts = age_filtered
		.into_iter()
		.filter(|post| {
			let community = post.community.to_lowercase();
			let author = post.author.name.to_lowercase();

			let allow_subreddit = match query.subreddit_mode {
				FacetMode::None => false,
				FacetMode::Include => selected_subreddits.is_empty() || selected_subreddits.contains(&community),
				FacetMode::Exclude => !selected_subreddits.contains(&community),
			};

			let allow_user = match query.user_mode {
				FacetMode::None => false,
				FacetMode::Include => selected_users.is_empty() || selected_users.contains(&author),
				FacetMode::Exclude => !selected_users.contains(&author),
			};

			allow_subreddit && allow_user
		})
		.collect::<Vec<_>>();

	filtered_posts.sort_by(compare_posts);

	let mut seen_ids = HashSet::new();
	filtered_posts.retain(|post| seen_ids.insert(post.id.clone()));

	VideoModeListing {
		posts: crate::user::collapse_duplicate_submissions(filtered_posts),
		subreddit_options,
		user_options,
		age_min_hours,
		age_max_hours,
		age_bound_hours,
	}
}

pub fn compare_posts(left: &Post, right: &Post) -> Ordering {
	right
		.media
		.duration_seconds
		.cmp(&left.media.duration_seconds)
		.then_with(|| right.created_ts.cmp(&left.created_ts))
		.then_with(|| left.community.to_lowercase().cmp(&right.community.to_lowercase()))
		.then_with(|| left.author.name.to_lowercase().cmp(&right.author.name.to_lowercase()))
		.then_with(|| left.id.cmp(&right.id))
}

fn derive_age_bound_hours(posts: &[Post], now_unix: u64) -> u32 {
	let oldest_age_secs = posts
		.iter()
		.filter(|post| post.created_ts > 0)
		.map(|post| now_unix.saturating_sub(post.created_ts))
		.max()
		.unwrap_or_default();

	let age_hours = oldest_age_secs.div_ceil(3600) as u32;
	age_hours.max(MIN_AGE_BOUND_HOURS)
}

fn clamp_age_band(query: &VideoModeQuery, age_bound_hours: u32) -> (u32, u32) {
	let min_hours = query.age_min_hours.min(age_bound_hours);
	let max_hours = query.age_max_hours.unwrap_or(age_bound_hours).min(age_bound_hours);
	if min_hours <= max_hours {
		(min_hours, max_hours)
	} else {
		(max_hours, min_hours)
	}
}

fn post_age_hours(post: &Post, now_unix: u64) -> u32 {
	now_unix.saturating_sub(post.created_ts).div_ceil(3600) as u32
}

fn facet_options<F>(posts: &[Post], value_fn: F, values: &HashSet<String>, mode: FacetMode) -> Vec<VideoModeFilterOption>
where
	F: Fn(&Post) -> String,
{
	let mut counts = HashMap::new();
	for post in posts {
		let value = value_fn(post);
		if value.trim().is_empty() {
			continue;
		}
		*counts.entry(value).or_insert(0usize) += 1;
	}

	let mut options = counts
		.into_iter()
		.map(|(name, count)| VideoModeFilterOption {
			active: {
				let key = name.to_lowercase();
				match mode {
					FacetMode::None => false,
					FacetMode::Include => values.is_empty() || values.contains(&key),
					FacetMode::Exclude => !values.contains(&key),
				}
			},
			name,
			count,
			page_count: 0,
		})
		.collect::<Vec<_>>();

	options.sort_by(|left, right| {
		right
			.count
			.cmp(&left.count)
			.then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
			.then_with(|| left.name.cmp(&right.name))
	});
	options
}

pub fn sort_options_by_page_count<F>(options: &[VideoModeFilterOption], page_posts: &[Post], value_fn: F) -> Vec<VideoModeFilterOption>
where
	F: Fn(&Post) -> String,
{
	let mut page_counts = HashMap::new();
	for post in page_posts {
		let value = value_fn(post);
		if value.trim().is_empty() {
			continue;
		}
		*page_counts.entry(value.to_lowercase()).or_insert(0usize) += 1;
	}

	let mut sorted = options.to_vec();
	for option in &mut sorted {
		option.page_count = page_counts.get(&option.name.to_lowercase()).copied().unwrap_or_default();
	}

	sorted.sort_by(|left, right| {
		right
			.page_count
			.cmp(&left.page_count)
			.then_with(|| right.count.cmp(&left.count))
			.then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
			.then_with(|| left.name.cmp(&right.name))
	});
	sorted
}

#[cfg(test)]
mod tests {
	use super::{
		build_listing, compare_posts, normalize_selected_values, parse_query, sort_options_by_page_count, FacetMode, VideoModeFilterOption, VideoModeQuery,
		VIDEO_AGE_MAX_HOURS_PARAM, VIDEO_AGE_MIN_HOURS_PARAM, VIDEO_SUB_PARAM, VIDEO_USER_PARAM,
	};
	use crate::utils::{Author, Awards, Flags, Flair, Media, Poll, Post};
	use std::cmp::Ordering;
	use std::collections::HashSet;

	fn test_post(id: &str, subreddit: &str, user: &str, created_ts: u64, duration_seconds: i64) -> Post {
		Post {
			id: id.to_string(),
			kind: "t3".to_string(),
			title: id.to_string(),
			community: subreddit.to_string(),
			body: String::new(),
			author: Author {
				name: user.to_string(),
				flair: empty_flair(),
				distinguished: String::new(),
				external_links: Vec::new(),
				avg_video_length: None,
			},
			permalink: format!("/r/{subreddit}/comments/{id}/"),
			parent_kind: String::new(),
			link_title: String::new(),
			link_permalink: String::new(),
			poll: None::<Poll>,
			score: ("1".to_string(), "1".to_string()),
			upvote_ratio: 100,
			post_type: "video".to_string(),
			flair: empty_flair(),
			flags: Flags::new(false, false, false),
			thumbnail: Media::empty(),
			media: Media {
				url: format!("https://v.redd.it/{id}"),
				duration_seconds,
				duration: crate::utils::format_video_duration(duration_seconds),
				..Media::empty()
			},
			domain: "v.redd.it".to_string(),
			rel_time: String::new(),
			created: String::new(),
			created_ts,
			num_duplicates: 0,
			comments: ("0".to_string(), "0".to_string()),
			gallery: Vec::new(),
			awards: Awards(Vec::new()),
			nsfw: false,
			ws_url: String::new(),
			out_url: None,
			preview_body: String::new(),
			collapsed_duplicates: Vec::new(),
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

	#[test]
	fn comparator_orders_by_duration_then_recency_then_source_then_user_then_id() {
		let longest = test_post("b", "rust", "alice", 100, 60);
		let newer = test_post("c", "rust", "alice", 101, 30);
		let older = test_post("d", "rust", "alice", 100, 30);
		let later_source = test_post("e", "zig", "alice", 100, 30);
		let later_user = test_post("f", "rust", "zoe", 100, 30);
		let later_id = test_post("z", "rust", "alice", 100, 30);

		assert_eq!(compare_posts(&longest, &newer), Ordering::Less);
		assert_eq!(compare_posts(&newer, &older), Ordering::Less);
		assert_eq!(compare_posts(&older, &later_source), Ordering::Less);
		assert_eq!(compare_posts(&older, &later_user), Ordering::Less);
		assert_eq!(compare_posts(&older, &later_id), Ordering::Less);
	}

	#[test]
	fn parse_query_collects_repeated_video_filters() {
		let query = format!("{VIDEO_SUB_PARAM}=rust&{VIDEO_SUB_PARAM}=programming&{VIDEO_USER_PARAM}=alice&{VIDEO_AGE_MIN_HOURS_PARAM}=4&{VIDEO_AGE_MAX_HOURS_PARAM}=72");
		assert_eq!(
			parse_query(&query),
			VideoModeQuery {
				selected_subreddits: vec!["rust".to_string(), "programming".to_string()],
				selected_users: vec!["alice".to_string()],
				subreddit_mode: FacetMode::Include,
				user_mode: FacetMode::Include,
				age_min_hours: 4,
				age_max_hours: Some(72),
			}
		);
	}

	#[test]
	fn build_listing_clamps_age_band_and_sorts_facets() {
		let now = 1_000_000;
		let posts = vec![
			test_post("one", "rust", "alice", now - 3600, 45),
			test_post("two", "rust", "alice", now - 7200, 30),
			test_post("three", "programming", "bob", now - 10_800, 60),
			test_post("four", "programming", "carol", now - 20_000, 15),
		];

		let listing = build_listing(
			&posts,
			&HashSet::new(),
			&VideoModeQuery {
				selected_subreddits: vec![],
				selected_users: vec![],
				subreddit_mode: FacetMode::Include,
				user_mode: FacetMode::Include,
				age_min_hours: 100,
				age_max_hours: Some(1),
			},
			now,
		);

		assert_eq!(listing.age_bound_hours, 24);
		assert_eq!(listing.age_min_hours, 1);
		assert_eq!(listing.age_max_hours, 24);
		assert_eq!(listing.subreddit_options[0].name, "programming");
		assert_eq!(listing.subreddit_options[0].count, 2);
		assert!(listing.subreddit_options.iter().all(|option| option.active));
		assert_eq!(listing.user_options[0].name, "alice");
		assert_eq!(listing.user_options[0].count, 2);
		assert!(listing.user_options.iter().all(|option| option.active));
		assert_eq!(listing.posts[0].id, "three");
	}

	#[test]
	fn normalize_selected_values_clears_full_selection() {
		let options = vec![
			VideoModeFilterOption {
				name: "rust".to_string(),
				count: 2,
				page_count: 0,
				active: true,
			},
			VideoModeFilterOption {
				name: "programming".to_string(),
				count: 1,
				page_count: 0,
				active: true,
			},
		];

		assert!(normalize_selected_values(&["rust".to_string(), "programming".to_string()], &options).is_empty());
		assert_eq!(normalize_selected_values(&["rust".to_string()], &options), vec!["rust".to_string()]);
	}

	#[test]
	fn sorts_options_by_current_page_counts_then_total_counts() {
		let options = vec![
			VideoModeFilterOption {
				name: "rust".to_string(),
				count: 5,
				page_count: 0,
				active: true,
			},
			VideoModeFilterOption {
				name: "programming".to_string(),
				count: 8,
				page_count: 0,
				active: true,
			},
			VideoModeFilterOption {
				name: "zig".to_string(),
				count: 3,
				page_count: 0,
				active: true,
			},
		];
		let page_posts = vec![
			test_post("one", "rust", "alice", 100, 10),
			test_post("two", "rust", "bob", 100, 10),
			test_post("three", "zig", "carol", 100, 10),
		];

		let sorted = sort_options_by_page_count(&options, &page_posts, |post| post.community.clone());

		assert_eq!(sorted[0].name, "rust");
		assert_eq!(sorted[0].page_count, 2);
		assert_eq!(sorted[1].name, "zig");
		assert_eq!(sorted[1].page_count, 1);
		assert_eq!(sorted[2].name, "programming");
		assert_eq!(sorted[2].page_count, 0);
	}
}
