#![allow(clippy::cmp_owned)]

// CRATES
use crate::utils::{self, catch_random, error, filter_posts, format_num, format_url, get_filters, param, redirect, setting, template, val, Post, Preferences};
use crate::{
	client::json,
	instance_scrape,
	server::RequestExt,
	subreddit::{can_access_quarantine, quarantine},
};
use askama::Template;
use hyper::{Body, Request, Response};
use regex::Regex;
use std::sync::LazyLock;

// STRUCTS
#[derive(Clone, PartialEq)]
struct SearchParams {
	q: String,
	sort: String,
	t: String,
	before: String,
	after: String,
	restrict_sr: String,
	typed: String,
}

// STRUCTS
#[derive(Clone, PartialEq)]
pub(crate) struct Subreddit {
	name: String,
	url: String,
	icon: String,
	description: String,
	subscribers: (String, String),
	is_subscribed: bool,
}

#[derive(Template)]
#[template(path = "search.html")]
pub struct SearchTemplate {
	shell_mode: bool,
	fragment_url: String,
	loading_message: String,
	posts: Vec<Post>,
	subreddits: Vec<Subreddit>,
	fallback_notice: String,
	sub: String,
	params: SearchParams,
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
}

crate::impl_template_str_eq!(SearchTemplate);

#[derive(Template)]
#[template(path = "search_fragment.html")]
struct SearchFragmentTemplate {
	posts: Vec<Post>,
	subreddits: Vec<Subreddit>,
	fallback_notice: String,
	sub: String,
	params: SearchParams,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	is_filtered: bool,
	all_posts_filtered: bool,
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
}

crate::impl_template_str_eq!(SearchFragmentTemplate);

/// Regex matched against search queries to determine if they are reddit urls.
static REDDIT_URL_MATCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://([^\./]+\.)*reddit.com/").unwrap());

fn search_fragment_path(req: &Request<Body>) -> String {
	let path = req.uri().path();
	let query = req.uri().query().unwrap_or_default();
	let fragment_path = if path == "/search" {
		"/search-fragment".to_string()
	} else {
		format!("/search-fragment{}", path)
	};

	if query.is_empty() {
		fragment_path
	} else {
		format!("{fragment_path}?{query}")
	}
}

fn search_source_path(req: &Request<Body>) -> String {
	let path = req.uri().path();
	if path == "/search-fragment" {
		"/search".to_string()
	} else {
		path.replacen("/search-fragment", "", 1)
	}
}

fn search_current_url(req: &Request<Body>) -> String {
	let path = search_source_path(req);
	let query = req.uri().query().unwrap_or_default();
	if query.is_empty() {
		path
	} else {
		format!("{path}?{query}")
	}
}

fn render_search_fragment(mut template_data: SearchFragmentTemplate) -> Result<Response<Body>, String> {
	utils::refresh_post_times(&mut template_data.posts);
	let body = template_data.render().map_err(|error| error.to_string())?;
	Response::builder()
		.status(200)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

fn search_params_from_request(path: &str, query: &str, after: String, typed: String) -> SearchParams {
	SearchParams {
		q: query.replace('"', "&quot;"),
		sort: param(path, "sort").unwrap_or_else(|| "relevance".to_string()),
		t: param(path, "t").unwrap_or_default(),
		before: param(path, "after").unwrap_or_default(),
		after,
		restrict_sr: param(path, "restrict_sr").unwrap_or_default(),
		typed,
	}
}

// SERVICES
pub async fn find(req: Request<Body>) -> Result<Response<Body>, String> {
	let uri_path = req.uri().path().replace("+", "%2B");
	let path = format!("{}.json?{}&raw_json=1", uri_path, req.uri().query().unwrap_or_default());
	let mut query = param(&path, "q").unwrap_or_default();
	query = REDDIT_URL_MATCH.replace(&query, "").to_string();

	if query.is_empty() {
		return Ok(redirect("/"));
	}

	if query.starts_with("r/") || query.starts_with("user/") {
		return Ok(redirect(&format!("/{query}")));
	}

	if query.starts_with("R/") {
		return Ok(redirect(&format!("/r{}", &query[1..])));
	}

	if query.starts_with("u/") || query.starts_with("U/") {
		return Ok(redirect(&format!("/user{}", &query[1..])));
	}

	let sub = req.param("sub").unwrap_or_default();
	let url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26").replace('+', "%2B");
	let typed = param(&path, "type").unwrap_or_default();

	Ok(template(&SearchTemplate {
		shell_mode: true,
		fragment_url: search_fragment_path(&req),
		loading_message: "Loading search results...".to_string(),
		posts: Vec::new(),
		subreddits: Vec::new(),
		fallback_notice: String::new(),
		sub,
		params: search_params_from_request(&path, &query, String::new(), typed),
		prefs: Preferences::new(&req),
		url,
		redirect_url,
		is_filtered: false,
		all_posts_filtered: false,
		all_posts_hidden_nsfw: false,
		no_posts: false,
	}))
}

pub async fn find_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	// This ensures that during a search, no NSFW posts are fetched at all
	let nsfw_results = if setting(&req, "show_nsfw") == "on" && !utils::sfw_only() {
		"&include_over_18=on"
	} else {
		""
	};
	let uri_path = search_source_path(&req).replace("+", "%2B");
	let path = format!("{}.json?{}{}&raw_json=1", uri_path, req.uri().query().unwrap_or_default(), nsfw_results);
	let mut query = param(&path, "q").unwrap_or_default();
	query = REDDIT_URL_MATCH.replace(&query, "").to_string();

	if query.is_empty() {
		return Ok(redirect("/"));
	}

	if query.starts_with("r/") || query.starts_with("user/") {
		return Ok(redirect(&format!("/{query}")));
	}

	if query.starts_with("R/") {
		return Ok(redirect(&format!("/r{}", &query[1..])));
	}

	if query.starts_with("u/") || query.starts_with("U/") {
		return Ok(redirect(&format!("/user{}", &query[1..])));
	}

	let sub = req.param("sub").unwrap_or_default();
	let quarantined = can_access_quarantine(&req, &sub);
	// Handle random subreddits
	if let Ok(random) = catch_random(&sub, "/find").await {
		return Ok(random);
	}

	let typed = param(&path, "type").unwrap_or_default();
	let filters = get_filters(&req);
	let url = search_current_url(&req);
	let prefs = Preferences::new(&req);
	let should_scrape_direct = instance_scrape::instance_scrape_enabled() && instance_scrape::reddit_rate_limited_now();

	if should_scrape_direct {
		return match instance_scrape::scrape_search_page(&url).await {
			Ok(mut scraped) => {
				let (_, all_posts_filtered) = filter_posts(&mut scraped.posts, &filters);
				let no_posts = scraped.posts.is_empty();
				let all_posts_hidden_nsfw = !no_posts && (scraped.posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
				utils::hydrate_post_author_links_from_cache(&mut scraped.posts);
				render_search_fragment(SearchFragmentTemplate {
					posts: scraped.posts,
					subreddits: mark_subreddit_subscriptions(
						scraped
							.subreddits
							.into_iter()
							.map(|subreddit| Subreddit {
								name: subreddit.name,
								url: subreddit.url,
								icon: subreddit.icon,
								description: subreddit.description,
								subscribers: subreddit.subscribers,
								is_subscribed: false,
							})
							.collect(),
						&prefs.subscriptions,
					),
					fallback_notice: instance_scrape::scrape_notice().to_string(),
					sub,
					params: search_params_from_request(&path, &query, scraped.after, typed),
					prefs,
					url,
					redirect_url: search_current_url(&req)[1..].replace('?', "%3F").replace('&', "%26").replace('+', "%2B"),
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
				})
			}
			Err(_) => error(req, "Too many requests.").await,
		};
	}

	// If search is not restricted to this subreddit, show other subreddits in search results
	let subreddits = if param(&path, "restrict_sr").is_none() {
		let mut subreddits = search_subreddits(&query, &typed).await;
		subreddits.retain(|s| !filters.contains(s.name.as_str()));
		subreddits
	} else {
		Vec::new()
	};

	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26").replace('+', "%2B");

	// If all requested subs are filtered, we don't need to fetch posts.
	if sub.split('+').all(|s| filters.contains(s)) {
		render_search_fragment(SearchFragmentTemplate {
			posts: Vec::new(),
			subreddits: mark_subreddit_subscriptions(subreddits, &prefs.subscriptions),
			fallback_notice: String::new(),
			sub,
			params: search_params_from_request(&path, &query, String::new(), typed),
			prefs,
			url,
			redirect_url,
			is_filtered: true,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		})
	} else {
		match Post::fetch(&path, quarantined).await {
			Ok((mut posts, after)) => {
				utils::store_post_flairs(&posts);
				let (_, all_posts_filtered) = filter_posts(&mut posts, &filters);
				let no_posts = posts.is_empty();
				let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
				utils::hydrate_post_author_links_from_cache(&mut posts);
				render_search_fragment(SearchFragmentTemplate {
					posts,
					subreddits: mark_subreddit_subscriptions(subreddits, &prefs.subscriptions),
					fallback_notice: String::new(),
					sub,
					params: search_params_from_request(&path, &query, after, typed),
					prefs,
					url,
					redirect_url,
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
				})
			}
			Err(msg) if instance_scrape::instance_scrape_enabled() && instance_scrape::should_attempt_scrape_fallback(&msg) => {
				match instance_scrape::scrape_search_page(&url).await {
					Ok(mut scraped) => {
						let (_, all_posts_filtered) = filter_posts(&mut scraped.posts, &filters);
						let no_posts = scraped.posts.is_empty();
						let all_posts_hidden_nsfw = !no_posts && (scraped.posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
						utils::hydrate_post_author_links_from_cache(&mut scraped.posts);
						render_search_fragment(SearchFragmentTemplate {
							posts: scraped.posts,
							subreddits: mark_subreddit_subscriptions(
								scraped
									.subreddits
									.into_iter()
									.map(|subreddit| Subreddit {
										name: subreddit.name,
										url: subreddit.url,
										icon: subreddit.icon,
										description: subreddit.description,
										subscribers: subreddit.subscribers,
										is_subscribed: false,
									})
									.collect(),
								&prefs.subscriptions,
							),
							fallback_notice: instance_scrape::scrape_notice().to_string(),
							sub,
							params: search_params_from_request(&path, &query, scraped.after, typed),
							prefs,
							url,
							redirect_url,
							is_filtered: false,
							all_posts_filtered,
							all_posts_hidden_nsfw,
							no_posts,
						})
					}
					Err(_) => error(req, &msg).await,
				}
			}
			Err(msg) => {
				if msg == "quarantined" || msg == "gated" {
					let sub = req.param("sub").unwrap_or_default();
					Ok(quarantine(&req, sub, &msg))
				} else {
					error(req, &msg).await
				}
			}
		}
	}
}

async fn search_subreddits(q: &str, typed: &str) -> Vec<Subreddit> {
	let limit = if typed == "sr_user" { "50" } else { "3" };
	let subreddit_search_path = format!("/subreddits/search.json?q={}&limit={limit}", q.replace(' ', "+"));

	// Send a request to the url
	json(subreddit_search_path, false).await.unwrap_or_default()["data"]["children"]
		.as_array()
		.map(ToOwned::to_owned)
		.unwrap_or_default()
		.iter()
		.map(|subreddit| {
			// For each subreddit from subreddit list
			// Fetch subreddit icon either from the community_icon or icon_img value
			let icon = subreddit["data"]["community_icon"].as_str().map_or_else(|| val(subreddit, "icon_img"), ToString::to_string);

			Subreddit {
				name: val(subreddit, "display_name"),
				url: val(subreddit, "url"),
				icon: format_url(&icon),
				description: val(subreddit, "public_description"),
				subscribers: format_num(subreddit["data"]["subscribers"].as_f64().unwrap_or_default() as i64),
				is_subscribed: false,
			}
		})
		.collect::<Vec<Subreddit>>()
}

fn mark_subreddit_subscriptions(subreddits: Vec<Subreddit>, subscriptions: &[String]) -> Vec<Subreddit> {
	subreddits
		.into_iter()
		.map(|mut subreddit| {
			subreddit.is_subscribed = subscriptions.iter().any(|sub| sub.eq_ignore_ascii_case(&subreddit.name));
			subreddit
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::{search_current_url, search_fragment_path, search_source_path, SearchFragmentTemplate, SearchParams, SearchTemplate};
	use crate::utils::Preferences;
	use askama::Template;
	use hyper::{Body, Request};

	fn test_prefs() -> Preferences {
		Preferences {
			available_themes: vec!["system".to_string()],
			layout: "card".to_string(),
			fixed_navbar: "on".to_string(),
			disable_visit_reddit_confirmation: "on".to_string(),
			..Preferences::default()
		}
	}

	fn test_params() -> SearchParams {
		SearchParams {
			q: "rust".to_string(),
			sort: "relevance".to_string(),
			t: String::new(),
			before: String::new(),
			after: String::new(),
			restrict_sr: String::new(),
			typed: String::new(),
		}
	}

	#[test]
	fn builds_search_fragment_path_for_global_search() {
		let req = Request::builder().uri("/search?q=rust").body(Body::empty()).unwrap();
		assert_eq!(search_fragment_path(&req), "/search-fragment?q=rust");
	}

	#[test]
	fn current_url_strips_search_fragment_prefix() {
		let req = Request::builder().uri("/search-fragment/r/rust/search?q=borrow").body(Body::empty()).unwrap();
		assert_eq!(search_current_url(&req), "/r/rust/search?q=borrow");
	}

	#[test]
	fn search_source_path_restores_global_search_route() {
		let req = Request::builder().uri("/search-fragment?q=rust").body(Body::empty()).unwrap();
		assert_eq!(search_source_path(&req), "/search");
	}

	#[test]
	fn renders_shell_mode_search_placeholder_with_fragment_url() {
		let html = SearchTemplate {
			shell_mode: true,
			fragment_url: "/search-fragment?q=rust".to_string(),
			loading_message: "Loading search results...".to_string(),
			posts: Vec::new(),
			subreddits: Vec::new(),
			fallback_notice: String::new(),
			sub: String::new(),
			params: test_params(),
			prefs: test_prefs(),
			url: "/search?q=rust".to_string(),
			redirect_url: "search%3Fq=rust".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}
		.render()
		.unwrap();

		assert!(html.contains("data-fragment-url=\"/search-fragment?q=rust\""));
		assert!(html.contains("Loading search results..."));
		assert!(html.contains("Loading search results is taking longer than expected."));
	}

	#[test]
	fn renders_search_fragment_next_link_without_whitespace() {
		let html = SearchFragmentTemplate {
			posts: Vec::new(),
			subreddits: Vec::new(),
			fallback_notice: String::new(),
			sub: String::new(),
			params: SearchParams {
				after: "t3_test".to_string(),
				..test_params()
			},
			prefs: test_prefs(),
			url: "/search?q=rust".to_string(),
			redirect_url: "search%3Fq=rust".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}
		.render()
		.unwrap();

		assert!(html.contains("href=\"?q=rust&restrict_sr=&sort=relevance&t=&after=t3_test\" accesskey=\"N\""));
	}
}
