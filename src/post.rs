#![allow(clippy::cmp_owned)]

// CRATES
use crate::bookmarks;
use crate::client::json;
use crate::config::get_setting;
use crate::instance_scrape;
use crate::server::RequestExt;
use crate::subreddit::{can_access_quarantine, quarantine};
use crate::utils::{
	error, format_num, get_filters, nsfw_landing, param, parse_post, rewrite_emotes, setting, template, time, val, Author, Awards, Comment, Flair, FlairPart, Post, Preferences,
};
use hyper::{header, Body, Request, Response};
use log::{info, warn};

use askama::Template;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

// STRUCTS
#[derive(Template)]
#[template(path = "post.html")]
pub struct PostTemplate {
	comments: Vec<Comment>,
	post: Post,
	cached_post_shell: Option<CachedPostShell>,
	post_shell_id: String,
	post_shell_subreddit: String,
	fallback_notice: String,
	sort: String,
	prefs: Preferences,
	single_thread: bool,
	single_thread_str: &'static str,
	url: String,
	url_without_query: String,
	comment_query: String,
	shell_mode: bool,
	fragment_url: String,
	loading_message: String,
}

crate::impl_template_str_eq!(PostTemplate);

#[derive(Clone, Debug)]
struct CachedPostShell {
	post_id: String,
	revision: String,
	component_path: String,
	request_path: String,
	card_html: String,
	title: String,
	community: String,
	blurred: bool,
	allow_stable_media: bool,
}

#[derive(Template)]
#[template(path = "post_fragment.html")]
struct PostFragmentTemplate {
	comments: Vec<Comment>,
	post: Post,
	fallback_notice: String,
	sort: String,
	prefs: Preferences,
	single_thread_str: &'static str,
	url: String,
	url_without_query: String,
	comment_query: String,
}

crate::impl_template_str_eq!(PostFragmentTemplate);

static COMMENT_SEARCH_CAPTURE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\?q=(.*)&type=comment").unwrap());

fn empty_post_for_shell(community: &str, permalink: &str) -> Post {
	Post {
		id: String::new(),
		kind: "t3".to_string(),
		title: "Loading post...".to_string(),
		community: community.to_string(),
		body: String::new(),
		author: Author {
			name: String::new(),
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
		permalink: permalink.to_string(),
		parent_id: String::new(),
		parent_kind: String::new(),
		parent_comment: crate::utils::ParentComment::default(),
		link_id: String::new(),
		link_title: String::new(),
		link_permalink: String::new(),
		link_author: String::new(),
		link_created_ts: 0,
		poll: None,
		score: ("0".to_string(), "0".to_string()),
		upvote_ratio: 0,
		post_type: "self".to_string(),
		flair: Flair {
			flair_parts: Vec::new(),
			text: String::new(),
			background_color: String::new(),
			foreground_color: String::new(),
		},
		flags: crate::utils::Flags::new(false, false, false),
		thumbnail: crate::utils::Media::empty(),
		media: crate::utils::Media::empty(),
		domain: String::new(),
		rel_time: String::new(),
		created: String::new(),
		created_ts: 0,
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

fn post_fragment_path(req: &Request<Body>) -> String {
	let path = req.uri().path().trim_end_matches('/');
	let path = if path.is_empty() { "/" } else { path };
	let query = req.uri().query().unwrap_or_default();
	if query.is_empty() {
		format!("/post-fragment{path}")
	} else {
		format!("/post-fragment{path}?{query}")
	}
}

fn post_source_path(req: &Request<Body>) -> String {
	let path = req.uri().path().trim_end_matches('/');
	let path = if path.is_empty() { "/" } else { path };
	path.replacen("/post-fragment", "", 1)
}

pub fn full_render_requested(url: &str) -> bool {
	url
		.split_once('?')
		.map(|(_, query)| url::form_urlencoded::parse(query.as_bytes()).any(|(key, value)| key == "render" && value == "full"))
		.unwrap_or(false)
}

pub async fn item(req: Request<Body>) -> Result<Response<Body>, String> {
	let requested_post_id = req.param("id").unwrap_or_default();
	let url = req.uri().to_string();
	let request_path = req.uri().path().to_string();
	let prefs = Preferences::new(&req);
	let cached_snapshot = crate::post_components::latest(&requested_post_id);
	if cached_snapshot.as_ref().is_some_and(|snapshot| snapshot.stable.nsfw) && crate::utils::should_be_nsfw_gated(&req, &url) {
		return nsfw_landing(req, url).await;
	}

	let cached_post_shell = match (cached_snapshot.as_ref(), crate::post_components::render_latest_stable_card(&requested_post_id)) {
		(Some(snapshot), Ok(Some(rendered)))
			if rendered.post_id == requested_post_id && rendered.revision == snapshot.revision.as_str() && rendered.post_id == snapshot.stable.id.as_str() =>
		{
			let blurred = (snapshot.stable.nsfw && prefs.blur_nsfw == "on") || (snapshot.stable.spoiler && prefs.blur_spoiler == "on");
			Some(CachedPostShell {
				post_id: rendered.post_id,
				revision: rendered.revision,
				component_path: rendered.path,
				request_path,
				card_html: rendered.html,
				title: snapshot.stable.title.clone(),
				community: snapshot.stable.community.clone(),
				blurred,
				allow_stable_media: !blurred,
			})
		}
		(_, Err(error)) => {
			warn!("post: failed to render cached stable shell for {}: {}", requested_post_id, error);
			None
		}
		_ => None,
	};
	let post_shell_subreddit = cached_post_shell
		.as_ref()
		.map(|cached| cached.community.clone())
		.unwrap_or_else(|| req.param("sub").unwrap_or_default());
	let loading_message = if cached_post_shell.is_some() {
		"Loading fresh post metadata and comments..."
	} else {
		"Loading post and comments..."
	};
	let mut response = template(&PostTemplate {
		comments: Vec::new(),
		post: empty_post_for_shell(&post_shell_subreddit, &url),
		cached_post_shell,
		post_shell_id: requested_post_id,
		post_shell_subreddit,
		fallback_notice: String::new(),
		sort: String::new(),
		prefs: Preferences::new(&req),
		single_thread: req.param("comment_id").is_some(),
		single_thread_str: if req.param("comment_id").is_some() { "true" } else { "false" },
		url: url.clone(),
		url_without_query: url.clone(),
		comment_query: String::new(),
		shell_mode: true,
		fragment_url: post_fragment_path(&req),
		loading_message: loading_message.to_string(),
	});
	response.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("private, no-store"));
	response.headers_mut().insert("cloudflare-cdn-cache-control", header::HeaderValue::from_static("no-store"));
	Ok(response)
}

pub async fn item_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	// Build Reddit API path
	let request_path = post_source_path(&req);
	let reddit_path = if request_path.ends_with('/') {
		request_path.clone()
	} else {
		format!("{request_path}/")
	};
	let mut path: String = format!("{}.json?{}&raw_json=1", reddit_path, req.uri().query().unwrap_or_default());
	let sub = req.param("sub").unwrap_or_default();
	let quarantined = can_access_quarantine(&req, &sub);
	let url = req.uri().to_string();

	// Set sort to sort query parameter
	let sort = param(&path, "sort").unwrap_or_else(|| {
		// Grab default comment sort method from Cookies
		let default_sort = setting(&req, "comment_sort");

		// If there's no sort query but there's a default sort, set sort to default_sort
		if default_sort.is_empty() {
			String::new()
		} else {
			path = format!("{}.json?{}&sort={}&raw_json=1", reddit_path, req.uri().query().unwrap_or_default(), default_sort);
			default_sort
		}
	});

	// Log the post ID being fetched in debug mode
	#[cfg(debug_assertions)]
	req.param("id").unwrap_or_default();

	let single_thread = req.param("comment_id").is_some();
	let highlighted_comment = &req.param("comment_id").unwrap_or_default();
	let query_body = match COMMENT_SEARCH_CAPTURE.captures(&url) {
		Some(captures) => captures.get(1).unwrap().as_str().replace("%20", " ").replace('+', " "),
		None => String::new(),
	};
	let query_string = format!("q={query_body}&type=comment");
	let form = url::form_urlencoded::parse(query_string.as_bytes()).collect::<HashMap<_, _>>();
	let comment_query = form.get("q").unwrap().clone().to_string();

	if instance_scrape::instance_scrape_enabled() && instance_scrape::reddit_rate_limited_now() {
		info!("post: using scrape fallback for {} because Reddit is currently rate limited", url);
		if let Ok(mut scraped) = instance_scrape::scrape_post_page(&url, highlighted_comment).await {
			crate::post_components::observe_post(
				&scraped.post,
				crate::post_components::ObservationSource::Scrape,
				url.clone(),
				crate::cache_persistence::unix_now(),
			);
			let prefs = Preferences::new(&req);
			bookmarks::decorate_post(&req, &mut scraped.post);
			if scraped.post.bookmarked {
				scraped.post.bookmark_new_comments = Some(0);
			}
			let rendered_post = scraped.post.clone();
			instance_scrape::apply_comment_prefs(&mut scraped.comments, &prefs);
			let body = PostFragmentTemplate {
				comments: scraped.comments,
				post: rendered_post.clone(),
				fallback_notice: instance_scrape::scrape_notice().to_string(),
				sort,
				prefs,
				single_thread_str: if single_thread { "true" } else { "false" },
				url: req.uri().to_string(),
				url_without_query: url.clone().trim_end_matches(&format!("?q={comment_query}&type=comment")).to_string(),
				comment_query,
			}
			.render()
			.map_err(|error| error.to_string())?;
			let mut response = Response::builder()
				.status(200)
				.header("content-type", "text/html; charset=utf-8")
				.header("cache-control", "no-store")
				.body(Body::from(body))
				.map_err(|error| error.to_string())?;
			bookmarks::update_visit_baseline(&mut response, &req, &rendered_post)?;
			return Ok(response);
		}
		warn!("post: scrape fallback failed for {} while Reddit was already rate limited", url);
	}

	// Send a request to the url, receive JSON in response
	match json(path, quarantined).await {
		// Otherwise, grab the JSON output from the request
		Ok(response) => {
			// Parse the JSON into Post and Comment structs
			let mut post = parse_post(&response[0]["data"]["children"][0]).await;
			crate::post_components::observe_post(
				&post,
				crate::post_components::ObservationSource::Direct,
				request_path.clone(),
				crate::cache_persistence::unix_now(),
			);
			post.author.external_links = crate::utils::cached_author_external_links(&post.author.name);
			bookmarks::decorate_post(&req, &mut post);
			if post.bookmarked {
				post.bookmark_new_comments = Some(0);
			}

			let req_url = req.uri().to_string();
			// Return landing page if this post if this Reddit deems this post
			// NSFW, but we have also disabled the display of NSFW content
			// or if the instance is SFW-only.
			if post.nsfw && crate::utils::should_be_nsfw_gated(&req, &req_url) {
				return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
			}

			let comments = match comment_query.as_str() {
				"" => parse_comments(&response[1], &post.permalink, &post.author.name, highlighted_comment, &get_filters(&req), &req),
				_ => query_comments(
					&response[1],
					&post.permalink,
					&post.author.name,
					highlighted_comment,
					&get_filters(&req),
					&comment_query,
					&req,
				),
			};

			// Use the Post and Comment structs to generate a website to show users
			let body = PostFragmentTemplate {
				comments,
				post: post.clone(),
				fallback_notice: String::new(),
				sort,
				prefs: Preferences::new(&req),
				single_thread_str: if single_thread { "true" } else { "false" },
				url: req_url,
				url_without_query: url.clone().trim_end_matches(&format!("?q={comment_query}&type=comment")).to_string(),
				comment_query,
			}
			.render()
			.map_err(|error| error.to_string())?;
			let mut response = Response::builder()
				.status(200)
				.header("content-type", "text/html; charset=utf-8")
				.header("cache-control", "no-store")
				.body(Body::from(body))
				.map_err(|error| error.to_string())?;
			bookmarks::update_visit_baseline(&mut response, &req, &post)?;
			Ok(response)
		}
		// If the Reddit API returns an error, exit and send error page to user
		Err(msg) => {
			if instance_scrape::instance_scrape_enabled() && instance_scrape::should_attempt_scrape_fallback(&msg) {
				info!("post: retrying {} via scrape fallback after Reddit error: {}", url, msg);
				match instance_scrape::scrape_post_page(&url, highlighted_comment).await {
					Ok(mut scraped) => {
						crate::post_components::observe_post(
							&scraped.post,
							crate::post_components::ObservationSource::Scrape,
							url.clone(),
							crate::cache_persistence::unix_now(),
						);
						let prefs = Preferences::new(&req);
						bookmarks::decorate_post(&req, &mut scraped.post);
						if scraped.post.bookmarked {
							scraped.post.bookmark_new_comments = Some(0);
						}
						let rendered_post = scraped.post.clone();
						instance_scrape::apply_comment_prefs(&mut scraped.comments, &prefs);
						let body = PostFragmentTemplate {
							comments: scraped.comments,
							post: rendered_post.clone(),
							fallback_notice: instance_scrape::scrape_notice().to_string(),
							sort,
							prefs,
							single_thread_str: if single_thread { "true" } else { "false" },
							url: req.uri().to_string(),
							url_without_query: url.clone().trim_end_matches(&format!("?q={comment_query}&type=comment")).to_string(),
							comment_query,
						}
						.render()
						.map_err(|error| error.to_string())?;
						let mut response = Response::builder()
							.status(200)
							.header("content-type", "text/html; charset=utf-8")
							.header("cache-control", "no-store")
							.body(Body::from(body))
							.map_err(|error| error.to_string())?;
						bookmarks::update_visit_baseline(&mut response, &req, &rendered_post)?;
						return Ok(response);
					}
					Err(scrape_error) => {
						warn!("post: scrape fallback failed for {} after Reddit error {}: {}", url, msg, scrape_error);
					}
				}
			}
			if msg == "quarantined" || msg == "gated" {
				let sub = req.param("sub").unwrap_or_default();
				Ok(quarantine(&req, sub, &msg))
			} else {
				error(req, &msg).await
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{full_render_requested, post_fragment_path, post_source_path, PostFragmentTemplate, PostTemplate};
	use crate::server::RequestExt;
	use crate::utils::Preferences;
	use askama::Template;
	use hyper::{Body, Request};

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

	#[test]
	fn renders_shell_mode_post_placeholder_with_fragment_url() {
		let html = PostTemplate {
			comments: Vec::new(),
			post: super::empty_post_for_shell("rust", "/r/rust/comments/demo/title"),
			cached_post_shell: None,
			post_shell_id: "demo".to_string(),
			post_shell_subreddit: "rust".to_string(),
			fallback_notice: String::new(),
			sort: String::new(),
			prefs: test_prefs(),
			single_thread: false,
			single_thread_str: "false",
			url: "/r/rust/comments/demo/title".to_string(),
			url_without_query: "/r/rust/comments/demo/title".to_string(),
			comment_query: String::new(),
			shell_mode: true,
			fragment_url: "/post-fragment/r/rust/comments/demo/title".to_string(),
			loading_message: "Loading post and comments...".to_string(),
		}
		.render()
		.unwrap();

		assert!(html.contains("data-fragment-url=\"/post-fragment/r/rust/comments/demo/title\""));
		assert!(html.contains("data-error-message=\"Loading the post is taking longer than expected.\""));
		assert!(html.contains("data-timeout-ms=\"20000\""));
		assert!(html.contains("Loading post and comments..."));
		assert!(html.contains("data-post-shell-id=\"demo\""));
	}

	#[tokio::test]
	async fn item_paints_latest_stable_card_before_private_fragment() {
		let mut post = super::empty_post_for_shell("rust", "/r/rust/comments/shellcache01/stable_title");
		post.id = "shellcache01".to_string();
		post.title = "Stable cached title".to_string();
		post.community = "rust".to_string();
		post.permalink = "/r/rust/comments/shellcache01/stable_title/".to_string();
		post.body = "<div class=\"md\"><p>Stable cached body</p></div>".to_string();
		let snapshot =
			crate::post_components::observe_post(&post, crate::post_components::ObservationSource::Listing, "post-shell-test", 1_700_000_000).expect("stable observation");
		let expected_card = crate::post_components::render_latest_stable_card(&post.id)
			.expect("stable card render")
			.expect("latest stable card");

		let mut req = Request::builder().uri("/r/rust/comments/shellcache01/stable_title?sort=new").body(Body::empty()).unwrap();
		let mut params = route_recognizer::Params::new();
		params.insert("sub".to_string(), "rust".to_string());
		params.insert("id".to_string(), post.id.clone());
		params.insert("title".to_string(), "stable_title".to_string());
		req.set_params(params);

		let response = super::item(req).await.expect("cached shell response");
		assert_eq!(response.headers()[hyper::header::CACHE_CONTROL], "private, no-store");
		assert_eq!(response.headers()["cloudflare-cdn-cache-control"], "no-store");
		assert!(!response.headers().contains_key(hyper::header::SET_COOKIE));
		let body = hyper::body::to_bytes(response.into_body()).await.expect("cached shell body");
		let html = String::from_utf8(body.to_vec()).expect("utf8 shell");

		assert_eq!(expected_card.revision, snapshot.revision.as_str());
		assert!(html.contains(&format!("data-post-shell-id=\"{}\"", post.id)));
		assert!(html.contains(&format!("data-post-shell-revision=\"{}\"", snapshot.revision)));
		assert!(html.contains(&format!("data-post-component-url=\"{}\"", expected_card.path)));
		assert!(html.contains("data-post-shell-request-path=\"/r/rust/comments/shellcache01/stable_title\""));
		assert!(html.contains(&expected_card.html), "shell must inline the exact stable-card bytes");
		assert!(html.contains("data-fragment-url=\"/post-fragment/r/rust/comments/shellcache01/stable_title?sort=new\""));
		let cached_content = html.find("Stable cached body").expect("cached content");
		let loading_comments = html.rfind("Loading fresh post metadata and comments...").expect("private fragment loading state");
		assert!(cached_content < loading_comments, "stable content should paint before the private loading state");
	}

	#[test]
	fn post_fragment_path_trims_trailing_slash() {
		let request = Request::builder().uri("/r/rust/comments/demo/title/?sort=new").body(Body::empty()).unwrap();

		assert_eq!(post_fragment_path(&request), "/post-fragment/r/rust/comments/demo/title?sort=new");
	}

	#[test]
	fn post_source_path_removes_fragment_prefix() {
		let request = Request::builder().uri("/post-fragment/r/rust/comments/demo/title").body(Body::empty()).unwrap();

		assert_eq!(post_source_path(&request), "/r/rust/comments/demo/title");
	}

	#[test]
	fn full_render_flag_requires_exact_query_value() {
		assert!(full_render_requested("/post-fragment/r/rust/comments/demo/title?sort=new&render=full"));
		assert!(!full_render_requested("/post-fragment/r/rust/comments/demo/title?render=card"));
	}

	#[test]
	fn post_fragment_separates_revisioned_content_from_private_comments() {
		let mut post = super::empty_post_for_shell("rust", "/r/rust/comments/demo/title");
		post.kind = "t3".to_string();
		post.id = "demo".to_string();
		post.title = "Demo title".to_string();
		post.community = "rust".to_string();
		post.permalink = "/r/rust/comments/demo/title".to_string();
		let html = PostFragmentTemplate {
			comments: Vec::new(),
			post,
			fallback_notice: String::new(),
			sort: "confidence".to_string(),
			prefs: test_prefs(),
			single_thread_str: "false",
			url: "/post-fragment/r/rust/comments/demo/title".to_string(),
			url_without_query: "/r/rust/comments/demo/title".to_string(),
			comment_query: String::new(),
		}
		.render()
		.unwrap();

		assert!(html.contains("data-post-component-url=\"/post-content/demo/"));
		assert!(html.contains("data-post-live-media-url=\"/post-live/demo/"));
		assert!(html.contains("data-post-type=\"self\""));
		assert!(html.contains("data-post-component-slot"));
		assert!(html.contains("id=\"commentQueryForms\""));
	}
}

// COMMENTS

fn parse_comments(json: &serde_json::Value, post_link: &str, post_author: &str, highlighted_comment: &str, filters: &HashSet<String>, req: &Request<Body>) -> Vec<Comment> {
	// Parse the comment JSON into a Vector of Comments
	let comments = json["data"]["children"].as_array().map_or(Vec::new(), std::borrow::ToOwned::to_owned);

	// For each comment, retrieve the values to build a Comment object
	comments
		.into_iter()
		.map(|comment| {
			let data = &comment["data"];
			let replies: Vec<Comment> = if data["replies"].is_object() {
				parse_comments(&data["replies"], post_link, post_author, highlighted_comment, filters, req)
			} else {
				Vec::new()
			};
			build_comment(&comment, data, replies, post_link, post_author, highlighted_comment, filters, req)
		})
		.collect()
}

fn query_comments(
	json: &serde_json::Value,
	post_link: &str,
	post_author: &str,
	highlighted_comment: &str,
	filters: &HashSet<String>,
	query: &str,
	req: &Request<Body>,
) -> Vec<Comment> {
	let comments = json["data"]["children"].as_array().map_or(Vec::new(), std::borrow::ToOwned::to_owned);
	let mut results = Vec::new();

	for comment in comments {
		let data = &comment["data"];

		// If this comment contains replies, handle those too
		if data["replies"].is_object() {
			results.append(&mut query_comments(&data["replies"], post_link, post_author, highlighted_comment, filters, query, req));
		}

		let c = build_comment(&comment, data, Vec::new(), post_link, post_author, highlighted_comment, filters, req);
		if c.body.to_lowercase().contains(&query.to_lowercase()) {
			results.push(c);
		}
	}

	results
}

pub(crate) fn parse_comments_for_api(
	json: &serde_json::Value,
	post_link: &str,
	post_author: &str,
	highlighted_comment: &str,
	filters: &HashSet<String>,
	query: &str,
	req: &Request<Body>,
) -> Vec<Comment> {
	if query.is_empty() {
		parse_comments(json, post_link, post_author, highlighted_comment, filters, req)
	} else {
		query_comments(json, post_link, post_author, highlighted_comment, filters, query, req)
	}
}

#[allow(clippy::too_many_arguments)]
fn build_comment(
	comment: &serde_json::Value,
	data: &serde_json::Value,
	replies: Vec<Comment>,
	post_link: &str,
	post_author: &str,
	highlighted_comment: &str,
	filters: &HashSet<String>,
	req: &Request<Body>,
) -> Comment {
	let id = val(comment, "id");

	let body = if (val(comment, "author") == "[deleted]" && val(comment, "body") == "[removed]") || val(comment, "body") == "[ Removed by Reddit ]" {
		format!(
			"<div class=\"md\"><p>[removed] — <a href=\"https://{}{post_link}{id}\">view removed comment</a></p></div>",
			get_setting("REDLIB_PUSHSHIFT_FRONTEND").unwrap_or_else(|| String::from(crate::config::DEFAULT_PUSHSHIFT_FRONTEND)),
		)
	} else {
		rewrite_emotes(&data["media_metadata"], val(comment, "body_html"))
	};
	let kind = comment["kind"].as_str().unwrap_or_default().to_string();

	let unix_time = data["created_utc"].as_f64().unwrap_or_default();
	let (rel_time, created) = time(unix_time);

	let edited = data["edited"].as_f64().map_or((String::new(), String::new()), time);

	let score = data["score"].as_i64().unwrap_or(0);

	// The JSON API only provides comments up to some threshold.
	// Further comments have to be loaded by subsequent requests.
	// The "kind" value will be "more" and the "count"
	// shows how many more (sub-)comments exist in the respective nesting level.
	// Note that in certain (seemingly random) cases, the count is simply wrong.
	let more_count = data["count"].as_i64().unwrap_or_default();

	let awards: Awards = Awards::parse(&data["all_awardings"]);

	let parent_kind_and_id = val(comment, "parent_id");
	let parent_info = parent_kind_and_id.split('_').collect::<Vec<&str>>();

	let highlighted = id == highlighted_comment;

	let author = Author {
		name: val(comment, "author"),
		flair: Flair {
			flair_parts: FlairPart::parse(
				data["author_flair_type"].as_str().unwrap_or_default(),
				data["author_flair_richtext"].as_array(),
				data["author_flair_text"].as_str(),
			),
			text: val(comment, "link_flair_text"),
			background_color: val(comment, "author_flair_background_color"),
			foreground_color: val(comment, "author_flair_text_color"),
		},
		distinguished: val(comment, "distinguished"),
		external_links: Vec::new(),
		avg_video_length: crate::user::cached_avg_video_length(&val(comment, "author")),
	};
	let is_filtered = filters.contains(&["u_", author.name.as_str()].concat());

	// Many subreddits have a default comment posted about the sub's rules etc.
	// Many Redlib users do not wish to see this kind of comment by default.
	// Reddit does not tell us which users are "bots", so a good heuristic is to
	// collapse stickied moderator comments.
	let is_moderator_comment = data["distinguished"].as_str().unwrap_or_default() == "moderator";
	let is_stickied = data["stickied"].as_bool().unwrap_or_default();
	let collapsed = (is_moderator_comment && is_stickied) || is_filtered;

	Comment {
		id,
		kind,
		parent_id: parent_info[1].to_string(),
		parent_kind: parent_info[0].to_string(),
		post_link: post_link.to_string(),
		post_author: post_author.to_string(),
		body,
		author,
		score: if data["score_hidden"].as_bool().unwrap_or_default() {
			("\u{2022}".to_string(), "Hidden".to_string())
		} else {
			format_num(score)
		},
		rel_time,
		created,
		edited,
		replies,
		highlighted,
		awards,
		collapsed,
		is_filtered,
		more_count,
		prefs: Preferences::new(req),
	}
}
