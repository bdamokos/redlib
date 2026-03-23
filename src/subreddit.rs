#![allow(clippy::cmp_owned)]

use crate::{config, utils};
// CRATES
use crate::utils::{
	catch_random, clear_sharded_cookie, deflate_compress, deflate_decompress, error, filter_posts, format_num, format_url, get_filters, info,
	nsfw_landing, param, redirect, rewrite_urls, setting, template, val, write_sharded_cookie, FlairSubscription, Post, Preferences, Subreddit,
};
use crate::{client::json, server::RequestExt, server::ResponseExt};
use askama::Template;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use htmlescape::decode_html;
use hyper::{Body, Request, Response};
use log::warn;

use chrono::DateTime;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::LazyLock;
use url::form_urlencoded;

const FRONT_CURSOR_PARAM: &str = "front_cursor";
const REDDIT_LISTING_PATH_SOFT_LIMIT: usize = 7_000;
const TARGET_POST_COUNT: usize = 25;
const MAX_AGGREGATE_FILL_ROUNDS: usize = 3;
const AFTER_PATH_HEADROOM: usize = 128;

// STRUCTS
#[derive(Template)]
#[template(path = "subreddit.html")]
pub struct SubredditTemplate {
	sub: Subreddit,
	posts: Vec<Post>,
	top_users: Vec<SubredditUserEntry>,
	sort: (String, String),
	sort_options: Vec<SortOption>,
	flair_filters: Vec<FlairFilterOption>,
	active_flair_filter: String,
	clear_flair_filter_url: String,
	prev_page_url: String,
	next_page_url: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	enable_duplicate_collapse: bool,
	/// Whether the subreddit itself is filtered.
	is_filtered: bool,
	/// Whether all fetched posts are filtered (to differentiate between no posts fetched in the first place,
	/// and all fetched posts being filtered).
	all_posts_filtered: bool,
	/// Whether all posts were hidden because they are NSFW (and user has disabled show NSFW)
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
	is_flair_subscribed: bool,
}

crate::impl_template_str_eq!(SubredditTemplate);

#[derive(PartialEq)]
struct SubredditUserEntry {
	name: String,
	count: u32,
}

#[derive(PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum AggregateFeedSource {
	SubredditChunk { chunk: String },
	Flair { subreddit: String, flair: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AggregateFrontCursor {
	v: u8,
	sources: Vec<AggregateFeedSource>,
	source_afters: Vec<String>,
	prev_stack: Vec<Vec<String>>,
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
}

crate::impl_template_str_eq!(WikiTemplate);

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

static GEO_FILTER_MATCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"geo_filter=(?<region>\w+)").unwrap());

fn should_enable_duplicate_collapse(sub_name: &str) -> bool {
	sub_name.is_empty() || sub_name.contains('+') || sub_name == "all" || sub_name == "popular"
}

fn is_async_subscription_request(req: &Request<Body>) -> bool {
	req.headers().get("X-Redlib-Ajax").and_then(|value| value.to_str().ok()).is_some_and(|value| value == "1")
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

fn filter_posts_by_flair(posts: &mut Vec<Post>, flair: &str) {
	let flair = flair.trim();
	if flair.is_empty() {
		return;
	}

	posts.retain(|post| post.flair.text.trim() == flair);
}

fn build_sort_options(base_path: &str, current_query_url: &str, selected_sort: &str, flair_filter: &str) -> Vec<SortOption> {
	["hot", "new", "top", "rising", "controversial"]
		.into_iter()
		.map(|method| SortOption {
			label: format!("{}{}", method.get(0..1).unwrap_or_default().to_uppercase(), method.get(1..).unwrap_or_default()),
			url: update_url_query(
				&format!("{base_path}/{method}"),
				&[
					("flair", (!flair_filter.is_empty()).then_some(flair_filter)),
					("t", param(current_query_url, "t").as_deref()),
					(FRONT_CURSOR_PARAM, None),
					("before", None),
					("after", None),
				],
			),
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
		.filter(|(key, _)| key != FRONT_CURSOR_PARAM && key != "before" && key != "after")
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

		if !current.is_empty() && projected_len > REDDIT_LISTING_PATH_SOFT_LIMIT {
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
	let mut feeds = preferences
		.subscriptions
		.iter()
		cloned()
		.map(|name| SavedFeed::Subreddit { name })
		.collect::<Vec<_>>();

	feeds.extend(preferences.flair_subscriptions.iter().cloned().map(|subscription| SavedFeed::Flair {
		subreddit: subscription.subreddit,
		flair: subscription.flair,
	}));

	feeds
}

fn build_aggregate_sources(preferences: &Preferences, sort: &str, reddit_query: &str) -> Vec<AggregateFeedSource> {
	let mut sources = split_subscriptions_for_listing(&preferences.subscriptions, sort, reddit_query)
		into_iter()
		.map(|chunk| AggregateFeedSource::SubredditChunk { chunk })
		.collect::<Vec<_>>();

	sources.extend(preferences.flair_subscriptions.iter().cloned().map(|subscription| AggregateFeedSource::Flair {
		subreddit: subscription.subreddit,
		flair: subscription.flair,
	}));

	sources
}

fn build_listing_path(chunk: &str, sort: &str, base_query: &str, after: &str) -> String {
	let mut query_pairs: Vec<(String, String)> = form_urlencoded::parse(base_query.as_bytes()).into_owned().collect();
	if !after.is_empty() {
		query_pairs.push(("after".to_string(), after.to_string()));
	}

	let query = form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish();

	format!("/r/{}/{sort}.json?{query}", chunk.replace('+', "%2B"))
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
	let (mut posts, next_after) = match source {
		AggregateFeedSource::SubredditChunk { chunk } => Post::fetch(&build_listing_path(chunk, sort, reddit_query, after), quarantine).await?,
		AggregateFeedSource::Flair { subreddit, flair } => {
			let (mut posts, next_after) = Post::fetch(&build_listing_path(subreddit, sort, reddit_query, after), quarantine).await?;
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
) {
	loop {
		let mut made_progress = false;

		for buffer in chunk_buffers.iter_mut() {
			let Some(post) = buffer.pop_front() else {
				continue;
			};
			made_progress = true;

			let is_filtered = filters.contains(&post.community) || filters.contains(&format!("u_{}", post.author.name));
			if is_filtered {
				continue;
			}

			*any_post_after_filter = true;
			if !active_flair_filter.is_empty() && post.flair.text.trim() != active_flair_filter {
				continue;
			}
			if recent_post_ids.contains(&post.id) || !seen_post_ids.insert(post.id.clone()) {
				continue;
			}

			merged_posts.push(post);
			if merged_posts.len() >= TARGET_POST_COUNT {
				return;
			}
		}

		if !made_progress {
			return;
		}
	}
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
	let current_state = cursor.as_ref().map(|value| value.source_afters.clone()).unwrap_or_else(|| vec![String::new(); sources.len()]);
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

	for round in 0..MAX_AGGREGATE_FILL_ROUNDS {
		let mut fetched_any_chunk = false;

		for (index, source) in sources.iter().enumerate() {
			if round > 0 && next_state[index].is_empty() {
				continue;
			}
			if round == 0 && page_has_cursor && current_state[index].is_empty() {
				continue;
			}

			let request_after = next_state[index].clone();
			match fetch_source_listing_page(source, sort, reddit_query, &request_after, quarantine).await {
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

		merge_chunk_posts(
			&mut chunk_buffers,
			&mut merged_posts,
			filters,
			active_flair_filter,
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
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
		next_prev_stack.push(current_state.clone());
		encode_front_cursor(&AggregateFrontCursor {
			v: 1,
			sources: sources.to_vec(),
			source_afters: next_state,
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
			source_afters: previous_state,
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
	// Build Reddit API path
	let root = req.uri().path() == "/";
	let query = req.uri().query().unwrap_or_default().to_string();
	let subscribed = setting(&req, "subscriptions");
	let front_page = setting(&req, "front_page");
	let remove_default_feeds = setting(&req, "remove_default_feeds") == "on";
	let post_sort = req.cookie("post_sort").map_or_else(|| "hot".to_string(), |c| c.value().to_string());
	let sort = req.param("sort").unwrap_or_else(|| req.param("id").unwrap_or(post_sort));

	let sub_name = req.param("sub").unwrap_or(if front_page == "default" || front_page.is_empty() {
		if subscribed.is_empty() {
			"popular".to_string()
		} else {
			subscribed.clone()
		}
	} else {
		front_page.clone()
	});

	if (sub_name == "popular" || sub_name == "all") && remove_default_feeds {
		if subscribed.is_empty() {
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

	let quarantined = can_access_quarantine(&req, &sub_name) || root;

	// Handle random subreddits
	if let Ok(random) = catch_random(&sub_name, "").await {
		return Ok(random);
	}

	if req.param("sub").is_some() && sub_name.starts_with("u_") {
		return Ok(redirect(&["/user/", &sub_name[2..]].concat()));
	}

	let reddit_query = subreddit_listing_query(&query, sub_name == "popular");
	let aggregate_subscriptions = sub_name == subscribed && path_needs_frontpage_aggregation(&subscribed, &sort, &reddit_query);

	// Request subreddit metadata
	let sub = if !sub_name.contains('+') && sub_name != subscribed && sub_name != "popular" && sub_name != "all" {
		// Regular subreddit
		subreddit(&sub_name, quarantined).await.unwrap_or_default()
	} else if sub_name == subscribed && !aggregate_subscriptions {
		// Subscription feed
		if req.uri().path().starts_with("/r/") {
			subreddit(&sub_name, quarantined).await.unwrap_or_default()
		} else {
			Subreddit::default()
		}
	} else if aggregate_subscriptions && !req.uri().path().starts_with("/r/") {
		Subreddit::default()
	} else {
		// Multireddit, all, popular
		Subreddit {
			name: sub_name.clone(),
			..Subreddit::default()
		}
	};

	let req_url = req.uri().to_string();
	// Return landing page if this post if this is NSFW community but the user
	// has disabled the display of NSFW content or if the instance is SFW-only.
	if sub.nsfw && crate::utils::should_be_nsfw_gated(&req, &req_url) {
		return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
	}

	let url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26").replace('+', "%2B");
	let active_flair_filter = param(&format!("?{}", req.uri().query().unwrap_or_default()), "flair").unwrap_or_default();
	let path = build_listing_path(&sub_name, &sort, &reddit_query, "");
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
	let sort_base_path = if aggregate_subscriptions {
		if req.uri().path().starts_with("/r/") {
			format!("/r/{sub_name}")
		} else {
			String::new()
		}
	} else if sub_name.is_empty() {
		String::new()
	} else {
		format!("/r/{sub_name}")
	};
	let sort_options = build_sort_options(&sort_base_path, &url, &sort, &active_flair_filter);
	let filters = get_filters(&req);
	let timeframe = param(&path, "t").unwrap_or_default();

	// If all requested subs are filtered, we don't need to fetch posts.
	if sub_name.split('+').all(|s| filters.contains(s)) {
		Ok(template(&SubredditTemplate {
			sub,
			posts: Vec::new(),
			top_users,
			sort: (sort, param(&path, "t").unwrap_or_default()),
			sort_options,
			flair_filters,
			active_flair_filter,
			clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)]),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: Preferences::new(&req),
			url,
			redirect_url,
			enable_duplicate_collapse: should_enable_duplicate_collapse(&sub_name),
			is_filtered: true,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}))
	} else if aggregate_subscriptions {
		let chunks = split_subscriptions_for_listing(&Preferences::new(&req).subscriptions, &sort, &reddit_query);
		let query_fingerprint = listing_query_fingerprint(&query);
		let cursor = param(&format!("?{}", req.uri().query().unwrap_or_default()), FRONT_CURSOR_PARAM)
			.and_then(|encoded| decode_front_cursor(&encoded))
			.filter(|cursor| cursor_matches_request(cursor, &chunks, &sort, &timeframe, &query_fingerprint));

		match fetch_aggregated_subscription_feed(
			&chunks,
			cursor,
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
			Ok(mut feed) => {
				if sort == "new" {
					feed.posts.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
					feed.posts.sort_by(|a, b| b.flags.stickied.cmp(&a.flags.stickied));
				}
				let no_posts = feed.posts.is_empty();
				let all_posts_hidden_nsfw = !no_posts && (feed.posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
				utils::hydrate_post_author_links_from_cache(&mut feed.posts);
				if feed.partial_failure && feed.posts.is_empty() {
					warn!("Aggregate front page rendered without posts after partial failures");
				}

				Ok(template(&SubredditTemplate {
					sub,
					posts: feed.posts,
					top_users,
					sort: (sort.clone(), timeframe.clone()),
					sort_options,
					flair_filters,
					active_flair_filter: active_flair_filter.clone(),
					clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None), (FRONT_CURSOR_PARAM, None)]),
					prev_page_url: feed.prev_cursor.as_deref().map(|cursor| build_front_cursor_url(&url, cursor)).unwrap_or_default(),
					next_page_url: feed.next_cursor.as_deref().map(|cursor| build_front_cursor_url(&url, cursor)).unwrap_or_default(),
					prefs: Preferences::new(&req),
					url,
					redirect_url,
					enable_duplicate_collapse: should_enable_duplicate_collapse(&sub_name),
					is_filtered: false,
					all_posts_filtered: feed.all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
				}))
			}
			Err(msg) => error(req, &msg).await,
		}
	} else {
		match Post::fetch(&path, quarantined).await {
			Ok((mut posts, after)) => {
				utils::store_post_flairs(&posts);
				if !sub_name.is_empty() && sub_name != "all" && sub_name != "popular" && !sub_name.contains('+') {
					utils::store_subreddit_users(&sub_name, &posts);
				}
				let (_, all_posts_filtered) = filter_posts(&mut posts, &filters);
				filter_posts_by_flair(&mut posts, &active_flair_filter);
				let no_posts = posts.is_empty();
				let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
				if sort == "new" {
					posts.sort_by(|a, b| b.created_ts.cmp(&a.created_ts));
					posts.sort_by(|a, b| b.flags.stickied.cmp(&a.flags.stickied));
				}
				utils::hydrate_post_author_links_from_cache(&mut posts);
				let timeframe = param(&path, "t").unwrap_or_default();
				let before = param(&path, "after").unwrap_or_default();
				Ok(template(&SubredditTemplate {
					sub,
					posts,
					top_users: if !sub_name.is_empty() && sub_name != "all" && sub_name != "popular" && !sub_name.contains('+') {
						utils::cached_subreddit_users(&sub_name)
							.into_iter()
							.map(|(name, count)| SubredditUserEntry { name, count })
							.collect()
					} else {
						Vec::new()
					},
					sort: (sort.clone(), timeframe.clone()),
					sort_options,
					flair_filters: if !sub_name.is_empty() && sub_name != "all" && sub_name != "popular" && !sub_name.contains('+') {
						build_flair_filters(&url, &active_flair_filter, utils::cached_subreddit_flairs(&sub_name))
					} else {
						Vec::new()
					},
					active_flair_filter: active_flair_filter.clone(),
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
					prefs: Preferences::new(&req),
					url,
					redirect_url,
					enable_duplicate_collapse: should_enable_duplicate_collapse(&sub_name),
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
				}))
			}
			Err(msg) => match msg.as_str() {
				"quarantined" | "gated" => Ok(quarantine(&req, sub_name, &msg)),
				"private" => error(req, &format!("r/{sub_name} is a private community")).await,
				"banned" => error(req, &format!("r/{sub_name} has been banned from Reddit")).await,
				_ => error(req, &msg).await,
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{
		decode_front_cursor, encode_front_cursor, filter_posts_by_flair, listing_query_fingerprint, merge_chunk_posts, should_enable_duplicate_collapse,
		split_subscriptions_for_listing, update_url_query, AggregateFrontCursor, SubredditTemplate, FRONT_CURSOR_PARAM,
	};
	use askama::Template;
	use std::collections::{HashSet, VecDeque};

	use crate::utils::{Author, Awards, Flags, Flair, Media, Post, Preferences, Subreddit};

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
			parent_kind: String::new(),
			link_title: String::new(),
			link_permalink: String::new(),
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
		}
	}

	#[test]
	fn enables_duplicate_collapse_for_aggregate_feeds() {
		assert!(should_enable_duplicate_collapse(""));
		assert!(should_enable_duplicate_collapse("popular"));
		assert!(should_enable_duplicate_collapse("all"));
		assert!(should_enable_duplicate_collapse("rust+programming"));
		assert!(!should_enable_duplicate_collapse("rust"));
	}

	#[test]
	fn renders_duplicate_script_for_aggregate_feed_pages() {
		let html = SubredditTemplate {
			sub: Subreddit {
				name: "rust+programming".to_string(),
				..Default::default()
			},
			posts: vec![test_post()],
			top_users: Vec::new(),
			sort: ("hot".to_string(), String::new()),
			sort_options: Vec::new(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			clear_flair_filter_url: "/r/rust+programming".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/r/rust+programming".to_string(),
			redirect_url: "r/rust%2Bprogramming".to_string(),
			enable_duplicate_collapse: true,
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}
		.render()
		.unwrap();

		assert!(html.contains("submitted_duplicates.js"));
	}

	#[test]
	fn does_not_render_duplicate_script_for_single_subreddit_pages() {
		let html = SubredditTemplate {
			sub: Subreddit {
				name: "rust".to_string(),
				..Default::default()
			},
			posts: vec![test_post()],
			top_users: vec![super::SubredditUserEntry {
				name: "demo_user".to_string(),
				count: 3,
			}],
			sort: ("hot".to_string(), String::new()),
			sort_options: Vec::new(),
			flair_filters: Vec::new(),
			active_flair_filter: String::new(),
			clear_flair_filter_url: "/r/rust".to_string(),
			prev_page_url: String::new(),
			next_page_url: String::new(),
			prefs: test_prefs(),
			url: "/r/rust".to_string(),
			redirect_url: "r/rust".to_string(),
			enable_duplicate_collapse: false,
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}
		.render()
		.unwrap();

		assert!(!html.contains("submitted_duplicates.js"));
		assert!(html.contains("/user/demo_user\">u/demo_user</a>"));
		assert!(html.contains(">3</span>"));
	}

	#[test]
	fn updates_urls_without_losing_existing_query_pairs() {
		assert_eq!(
			update_url_query("/r/rust?sort=hot&after=t3_demo", &[("flair", Some("Discussion")), ("after", None)]),
			"/r/rust?sort=hot&flair=Discussion"
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
		let top = options.iter().find(|option| option.label == "Top").unwrap();
		let rising = options.iter().find(|option| option.label == "Rising").unwrap();
		let controversial = options.iter().find(|option| option.label == "Controversial").unwrap();

		assert_eq!(top.url, "/top?t=day");
		assert_eq!(rising.url, "/rising?t=day");
		assert_eq!(controversial.url, "/controversial?t=day");
	}

	#[test]
	fn splits_large_subscription_lists_into_multiple_listing_chunks() {
		let subscriptions = (0..1_200).map(|idx| format!("subreddit{idx}")).collect::<Vec<_>>();
		let chunks = split_subscriptions_for_listing(&subscriptions, "hot", "raw_json=1");

		assert!(chunks.len() > 1);
		assert_eq!(chunks.join("+"), subscriptions.join("+"));
		assert!(chunks
			.iter()
			.all(|chunk| { format!("/r/{chunk}/hot.json?raw_json=1").len() + super::AFTER_PATH_HEADROOM <= super::REDDIT_LISTING_PATH_SOFT_LIMIT }));
	}

	#[test]
	fn aggregate_cursor_round_trips() {
		let cursor = AggregateFrontCursor {
			v: 1,
			chunks: vec!["a+b".to_string(), "c+d".to_string()],
			chunk_afters: vec!["t3_a".to_string(), String::new()],
			prev_stack: vec![vec![String::new(), String::new()]],
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
	fn query_fingerprint_ignores_front_cursor_and_page_tokens() {
		assert_eq!(
			listing_query_fingerprint("sort=hot&front_cursor=abc&after=t3_a&flair=News"),
			listing_query_fingerprint("sort=hot&before=t3_b&flair=News")
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

		merge_chunk_posts(
			&mut chunk_buffers,
			&mut merged,
			&HashSet::new(),
			"",
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
		);

		assert_eq!(merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["1", "3", "2"]);
		assert!(any_post_after_filter);

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

		merge_chunk_posts(
			&mut filtered_chunk_buffers,
			&mut filtered_merged,
			&filters,
			"",
			&filtered_recent_post_ids,
			&mut filtered_seen_post_ids,
			&mut filtered_flag,
		);

		assert_eq!(filtered_merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["1", "2"]);
		assert!(filtered_flag);
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

		merge_chunk_posts(
			&mut chunk_buffers,
			&mut merged,
			&HashSet::new(),
			"",
			&recent_post_ids,
			&mut seen_post_ids,
			&mut any_post_after_filter,
		);

		assert_eq!(merged.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["1", "3"]);
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

	let preferences = Preferences::new(&req);
	let mut sub_list = preferences.subscriptions;
	let mut filters = preferences.filters;

	// Retrieve list of posts for these subreddits to extract display names

	let posts = json(format!("/r/{sub}/hot.json?raw_json=1"), true).await;
	let display_lookup: Vec<(String, &str)> = match &posts {
		Ok(posts) => posts["data"]["children"]
			.as_array()
			.map(|list| {
				list
					.iter()
					.map(|post| {
						let display_name = post["data"]["subreddit"].as_str().unwrap_or_default();
						(display_name.to_lowercase(), display_name)
					})
					.collect::<Vec<_>>()
			})
			.unwrap_or_default(),
		Err(_) => vec![],
	};

	// Find each subreddit name (separated by '+') in sub parameter
	for part in sub.split('+').filter(|x| x != &"") {
		// Retrieve display name for the subreddit
		let display;
		let part = if part.starts_with("u_") {
			part
		} else if let Some(&(_, display)) = display_lookup.iter().find(|x| x.0 == part.to_lowercase()) {
			// This is already known, doesn't require separate request
			display
		} else {
			// This subreddit display name isn't known, retrieve it
			let path: String = format!("/r/{part}/about.json?raw_json=1");
			display = json(path, true).await;
			match &display {
				Ok(display) => display["data"]["display_name"].as_str(),
				Err(_) => None,
			}
			.unwrap_or(part)
		};

		// Modify sub list based on action
		if action.contains(&"subscribe".to_string()) && !sub_list.contains(&part.to_owned()) {
			// Add each sub name to the subscribed list
			sub_list.push(part.to_owned());
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
		Response::builder().status(204).header("Cache-Control", "no-store").body(Body::empty()).unwrap_or_default()
	} else {
		redirect(&path)
	};

	// If sub_list is empty remove all subscriptions cookies, otherwise update them and remove old ones
	if sub_list.is_empty() {
		// Remove subscriptions cookie
		response.remove_cookie("subscriptions".to_string());

		// Start with first numbered subscriptions cookie
		let mut subscriptions_number = 1;

		// While whatever subscriptionsNUMBER cookie we're looking at has a value
		while req.cookie(&format!("subscriptions{subscriptions_number}")).is_some() {
			// Remove that subscriptions cookie
			response.remove_cookie(format!("subscriptions{subscriptions_number}"));

			// Increment subscriptions cookie number
			subscriptions_number += 1;
		}
	} else {
		// Start at 0 to keep track of what number we need to start deleting old subscription cookies from
		let mut subscriptions_number_to_delete_from = 0;

		// Starting at 0 so we handle the subscription cookie without a number first
		for (subscriptions_number, list) in join_until_size_limit(&sub_list).into_iter().enumerate() {
			let subscriptions_cookie = if subscriptions_number == 0 {
				"subscriptions".to_string()
			} else {
				format!("subscriptions{subscriptions_number}")
			};

			response.insert_cookie(
				Cookie::build((subscriptions_cookie, list))
					.path("/")
					.http_only(true)
					.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
					.into(),
			);

			subscriptions_number_to_delete_from += 1;
		}

		// While whatever subscriptionsNUMBER cookie we're looking at has a value
		while req.cookie(&format!("subscriptions{subscriptions_number_to_delete_from}")).is_some() {
			// Remove that subscriptions cookie
			response.remove_cookie(format!("subscriptions{subscriptions_number_to_delete_from}"));

			// Increment subscriptions cookie number
			subscriptions_number_to_delete_from += 1;
		}
	}

	// If filters is empty remove all filters cookies, otherwise update them and remove old ones
	if filters.is_empty() {
		// Remove filters cookie
		response.remove_cookie("filters".to_string());

		// Start with first numbered filters cookie
		let mut filters_number = 1;

		// While whatever filtersNUMBER cookie we're looking at has a value
		while req.cookie(&format!("filters{filters_number}")).is_some() {
			// Remove that filters cookie
			response.remove_cookie(format!("filters{filters_number}"));

			// Increment filters cookie number
			filters_number += 1;
		}
	} else {
		// Start at 0 to keep track of what number we need to start deleting old filters cookies from
		let mut filters_number_to_delete_from = 0;

		for (filters_number, list) in join_until_size_limit(&filters).into_iter().enumerate() {
			let filters_cookie = if filters_number == 0 {
				"filters".to_string()
			} else {
				format!("filters{filters_number}")
			};

			response.insert_cookie(
				Cookie::build((filters_cookie, list))
					.path("/")
					.http_only(true)
					.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
					.into(),
			);

			filters_number_to_delete_from += 1;
		}

		// While whatever filtersNUMBER cookie we're looking at has a value
		while req.cookie(&format!("filters{filters_number_to_delete_from}")).is_some() {
			// Remove that filters cookie
			response.remove_cookie(format!("filters{filters_number_to_delete_from}"));

			// Increment filters cookie number
			filters_number_to_delete_from += 1;
		}
	}

	Ok(response)
}

pub async fn wiki(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = req.param("sub").unwrap_or_else(|| "reddit.com".to_string());
	let quarantined = can_access_quarantine(&req, &sub);
	// Handle random subreddits
	if let Ok(random) = catch_random(&sub, "/wiki").await {
		return Ok(random);
	}

	let page = req.param("page").unwrap_or_else(|| "index".to_string());
	let path: String = format!("/r/{sub}/wiki/{page}.json?raw_json=1");
	let url = req.uri().to_string();

	match json(path, quarantined).await {
		Ok(response) => Ok(template(&WikiTemplate {
			sub,
			wiki: rewrite_urls(response["data"]["content_html"].as_str().unwrap_or("<h3>Wiki not found</h3>")),
			page,
			prefs: Preferences::new(&req),
			url,
		})),
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
	let quarantined = can_access_quarantine(&req, &sub);

	// Handle random subreddits
	if let Ok(random) = catch_random(&sub, "/about/sidebar").await {
		return Ok(random);
	}

	// Build the Reddit JSON API url
	let path: String = format!("/r/{sub}/about.json?raw_json=1");
	let url = req.uri().to_string();

	// Send a request to the url
	match json(path, quarantined).await {
		// If success, receive JSON in response
		Ok(response) => Ok(template(&WikiTemplate {
			wiki: rewrite_urls(&val(&response, "description_html")),
			// wiki: format!(
			// 	"{}<hr><h1>Moderators</h1><br><ul>{}</ul>",
			// 	rewrite_urls(&val(&response, "description_html"),
			// 	moderators(&sub, quarantined).await.unwrap_or(vec!["Could not fetch moderators".to_string()]).join(""),
			// ),
			sub,
			page: "Sidebar".to_string(),
			prefs: Preferences::new(&req),
			url,
		})),
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
async fn subreddit(sub: &str, quarantined: bool) -> Result<Subreddit, String> {
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

	Ok(Subreddit {
		name: val(&res, "display_name"),
		title: val(&res, "title"),
		description: val(&res, "public_description"),
		info: rewrite_urls(&val(&res, "description_html")),
		// moderators: moderators_list(sub, quarantined).await.unwrap_or_default(),
		icon: format_url(&icon),
		members: format_num(members),
		active: format_num(active),
		wiki: res["data"]["wiki_enabled"].as_bool().unwrap_or_default(),
		nsfw: res["data"]["over18"].as_bool().unwrap_or_default(),
	})
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
