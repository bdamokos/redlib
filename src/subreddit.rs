#![allow(clippy::cmp_owned)]

use crate::{config, utils};
// CRATES
use crate::utils::{
	catch_random, error, filter_posts, format_num, format_url, get_filters, info, nsfw_landing, param, redirect, rewrite_urls, setting, template, val, Post, Preferences,
	Subreddit,
};
use crate::{client::json, server::RequestExt, server::ResponseExt};
use askama::Template;
use cookie::Cookie;
use htmlescape::decode_html;
use hyper::{Body, Request, Response};

use chrono::DateTime;
use regex::Regex;
use std::sync::LazyLock;
use time::{Duration, OffsetDateTime};
use url::form_urlencoded;

// STRUCTS
#[derive(Template)]
#[template(path = "subreddit.html")]
pub struct SubredditTemplate {
	sub: Subreddit,
	posts: Vec<Post>,
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
}

crate::impl_template_str_eq!(SubredditTemplate);

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
			url: update_url_query(current_url, &[("flair", Some(flair.text.as_str())), ("before", None), ("after", None)]),
			active: flair.text == active_flair_filter,
			flair,
		})
		.collect()
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

	// Request subreddit metadata
	let sub = if !sub_name.contains('+') && sub_name != subscribed && sub_name != "popular" && sub_name != "all" {
		// Regular subreddit
		subreddit(&sub_name, quarantined).await.unwrap_or_default()
	} else if sub_name == subscribed {
		// Subscription feed
		if req.uri().path().starts_with("/r/") {
			subreddit(&sub_name, quarantined).await.unwrap_or_default()
		} else {
			Subreddit::default()
		}
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

	let mut extra_query_pairs = vec![("raw_json".to_string(), "1".to_string())];
	if sub_name == "popular" {
		let geo_filter = match GEO_FILTER_MATCH.captures(&query) {
			Some(geo_filter) => geo_filter["region"].to_string(),
			None => "GLOBAL".to_owned(),
		};
		extra_query_pairs.push(("geo_filter".to_string(), geo_filter));
	}

	let url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26").replace('+', "%2B");
	let active_flair_filter = param(&format!("?{}", req.uri().query().unwrap_or_default()), "flair").unwrap_or_default();
	let reddit_query = update_url_query(&format!("?{}", req.uri().query().unwrap_or_default()), &[("flair", None)]);
	let path = {
		let mut query_pairs: Vec<(String, String)> = form_urlencoded::parse(reddit_query.trim_start_matches('?').as_bytes()).into_owned().collect();
		query_pairs.extend(extra_query_pairs);
		let query = form_urlencoded::Serializer::new(String::new())
			.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
			.finish();
		format!("/r/{}/{sort}.json?{query}", sub_name.replace('+', "%2B"))
	};
	let flair_filters = if !sub.name.is_empty() && sub.name != "all" && sub.name != "popular" && !sub.name.contains('+') {
		build_flair_filters(&url, &active_flair_filter, utils::cached_subreddit_flairs(&sub.name))
	} else {
		Vec::new()
	};
	let sort_options = if sub_name.is_empty() {
		build_sort_options("", &url, &sort, "")
	} else {
		build_sort_options(&format!("/r/{sub_name}"), &url, &sort, &active_flair_filter)
	};
	let filters = get_filters(&req);

	// If all requested subs are filtered, we don't need to fetch posts.
	if sub_name.split('+').all(|s| filters.contains(s)) {
		Ok(template(&SubredditTemplate {
			sub,
			posts: Vec::new(),
			sort: (sort, param(&path, "t").unwrap_or_default()),
			sort_options,
			flair_filters,
			active_flair_filter,
			clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None)]),
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
	} else {
		match Post::fetch(&path, quarantined).await {
			Ok((mut posts, after)) => {
				utils::store_post_flairs(&posts);
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
					sort: (sort.clone(), timeframe.clone()),
					sort_options,
					flair_filters: if !sub_name.is_empty() && sub_name != "all" && sub_name != "popular" && !sub_name.contains('+') {
						build_flair_filters(&url, &active_flair_filter, utils::cached_subreddit_flairs(&sub_name))
					} else {
						Vec::new()
					},
					active_flair_filter: active_flair_filter.clone(),
					clear_flair_filter_url: update_url_query(&url, &[("flair", None), ("before", None), ("after", None)]),
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
	use super::{filter_posts_by_flair, should_enable_duplicate_collapse, update_url_query, SubredditTemplate};
	use askama::Template;

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
