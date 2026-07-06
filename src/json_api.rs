use crate::client;
use crate::client::{PeerRateLimitStatus, RedditRateLimitStatus};
use crate::peer_api_fallback;
use crate::post as post_page;
use crate::server::RequestExt;
use crate::subreddit;
use crate::utils::{self, filter_posts, get_filters, parse_post, setting, sfw_only, should_be_nsfw_gated, Comment, Post, Subreddit, User};
use hyper::{Body, Request, Response};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::form_urlencoded;

#[derive(Serialize, Deserialize)]
struct ApiEnvelope<T> {
	data: Option<T>,
	error: Option<String>,
	meta: ApiMetadata,
}

#[derive(Serialize, Deserialize)]
struct ApiMetadata {
	rate_limit: RedditRateLimitStatus,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	peer_rate_limits: Vec<PeerRateLimitStatus>,
}

fn api_metadata() -> ApiMetadata {
	ApiMetadata {
		rate_limit: client::current_ratelimit_status(),
		peer_rate_limits: client::peer_ratelimit_statuses(),
	}
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ListingResponse {
	pub(crate) subreddit: Option<Subreddit>,
	pub(crate) posts: Vec<Post>,
	pub(crate) after: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub(crate) peer_origin: Option<String>,
	pub(crate) all_posts_filtered: bool,
	pub(crate) all_posts_hidden_nsfw: bool,
}

#[derive(Serialize)]
struct UserResponse {
	user: User,
	posts: Vec<Post>,
	after: Option<String>,
	all_posts_filtered: bool,
	all_posts_hidden_nsfw: bool,
}

#[derive(Serialize)]
struct SearchResponse {
	posts: Vec<Post>,
	after: Option<String>,
	all_posts_filtered: bool,
	all_posts_hidden_nsfw: bool,
}

#[derive(Serialize)]
struct PostResponse {
	post: Post,
	comments: Vec<Comment>,
}

fn json_response<T: Serialize>(data: T) -> Result<Response<Body>, String> {
	let body = serde_json::to_string(&ApiEnvelope {
		data: Some(data),
		error: None,
		meta: api_metadata(),
	})
	.map_err(|error| error.to_string())?;
	Response::builder()
		.status(200)
		.header("content-type", "application/json; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

fn json_error(msg: impl Into<String>, status: u16) -> Result<Response<Body>, String> {
	let body = serde_json::to_string(&ApiEnvelope::<()> {
		data: None,
		error: Some(msg.into()),
		meta: api_metadata(),
	})
	.map_err(|error| error.to_string())?;
	Response::builder()
		.status(status)
		.header("content-type", "application/json; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

fn upstream_error(msg: String) -> Result<Response<Body>, String> {
	let lower = msg.to_ascii_lowercase();
	let status = if lower.contains("too many requests") || lower.contains("rate limit") || lower.contains("ratelimit") {
		429
	} else if lower.contains("oauth_unavailable") {
		503
	} else if matches!(msg.as_str(), "quarantined" | "gated") {
		403
	} else if matches!(msg.as_str(), "private" | "banned" | "suspended") {
		404
	} else {
		502
	};
	json_error(msg, status)
}

fn clean_route_param(value: Option<String>) -> String {
	value.unwrap_or_default().trim_end_matches(".js").to_string()
}

fn query_pairs(req: &Request<Body>) -> Vec<(String, String)> {
	form_urlencoded::parse(req.uri().query().unwrap_or_default().as_bytes()).into_owned().collect()
}

fn has_query_param(req: &Request<Body>, name: &str) -> bool {
	query_pairs(req).iter().any(|(key, _)| key == name)
}

fn query_param(req: &Request<Body>, name: &str) -> Option<String> {
	query_pairs(req).into_iter().find_map(|(key, value)| (key == name).then_some(value))
}

fn query_with_raw_json(req: &Request<Body>, defaults: &[(&str, String)]) -> String {
	let mut pairs = query_pairs(req).into_iter().filter(|(key, _)| key != "raw_json").collect::<Vec<_>>();

	for (key, value) in defaults {
		if !pairs.iter().any(|(existing, _)| existing == key) {
			pairs.push(((*key).to_string(), value.clone()));
		}
	}

	pairs.push(("raw_json".to_string(), "1".to_string()));
	form_urlencoded::Serializer::new(String::new())
		.extend_pairs(pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish()
}

fn filter_by_flair(posts: &mut Vec<Post>, req: &Request<Body>) {
	let Some(flair) = query_param(req, "flair") else {
		return;
	};
	let flair = flair.trim();
	if !flair.is_empty() {
		posts.retain(|post| post.flair.text.trim() == flair);
	}
}

fn finalize_posts(req: &Request<Body>, posts: &mut Vec<Post>, filter_flair: bool) -> (bool, bool) {
	let fetched_posts = !posts.is_empty();
	let filters = get_filters(req);
	let (_, local_filters_removed_all) = filter_posts(posts, &filters);
	if filter_flair {
		filter_by_flair(posts, req);
	}
	utils::hydrate_post_author_links_from_cache(posts);
	utils::refresh_post_times(posts);
	let all_posts_filtered = fetched_posts && (local_filters_removed_all || posts.is_empty());
	let all_posts_hidden_nsfw = !posts.is_empty() && posts.iter().all(|post| post.flags.nsfw) && setting(req, "show_nsfw") != "on";
	(all_posts_filtered, all_posts_hidden_nsfw)
}

fn after_value(after: String) -> Option<String> {
	(!after.is_empty()).then_some(after)
}

fn api_source_path(req: &Request<Body>) -> String {
	let path = req.uri().path().strip_prefix("/api").unwrap_or_else(|| req.uri().path());
	let path = path.trim_end_matches(".js").trim_end_matches('/');
	if path.is_empty() {
		"/".to_string()
	} else {
		path.to_string()
	}
}

fn post_child(response: &Value) -> Option<&Value> {
	response.get(0)?.get("data")?.get("children")?.as_array()?.first()
}

fn default_user_sort(req: &Request<Body>) -> String {
	let sort = setting(req, "post_sort");
	if matches!(sort.as_str(), "hot" | "new" | "top" | "controversial") {
		sort
	} else {
		"hot".to_string()
	}
}

fn subreddit_sort(req: &Request<Body>) -> String {
	let sort = clean_route_param(req.param("sort"));
	if sort.is_empty() {
		"hot".to_string()
	} else if sort == "random" {
		"new".to_string()
	} else {
		sort
	}
}

async fn subreddit_metadata(name: &str, quarantined: bool) -> Option<Subreddit> {
	if name.is_empty() {
		return None;
	}
	if name == "all" || name == "popular" || name.contains('+') {
		return Some(Subreddit {
			name: name.to_string(),
			title: name.to_string(),
			..Subreddit::default()
		});
	}
	match subreddit::refresh_subreddit_metadata(name, quarantined).await {
		Ok(subreddit) => Some(subreddit),
		Err(_) => subreddit::cached_subreddit_metadata_for_name(name).or_else(|| {
			Some(Subreddit {
				name: name.to_string(),
				title: name.to_string(),
				..Subreddit::default()
			})
		}),
	}
}

pub async fn subreddit_listing(req: Request<Body>) -> Result<Response<Body>, String> {
	let sub = clean_route_param(req.param("sub"));
	if sub.is_empty() {
		return json_error("Missing subreddit", 400);
	}

	let sort = subreddit_sort(&req);
	let quarantined = subreddit::can_access_quarantine(&req, &sub);
	let query = query_with_raw_json(&req, &[]);
	let path = format!("/r/{}/{}.json?{}", sub.replace('+', "%2B"), sort, query);
	let (mut posts, after) = match Post::fetch(&path, quarantined).await {
		Ok(result) => result,
		Err(msg) => {
			if !peer_api_fallback::request_is_peer_api_fallback(&req) && crate::instance_scrape::should_attempt_scrape_fallback(&msg) {
				info!("json_api: retrying subreddit listing r/{sub}/{sort} through configured Redlib peer API after upstream error: {msg}");
				match peer_api_fallback::fetch_subreddit_listing(&sub, &sort, req.uri().query()).await {
					Ok(peer) => {
						let mut posts = peer.posts;
						let (all_posts_filtered, all_posts_hidden_nsfw) = finalize_posts(&req, &mut posts, true);
						return json_response(ListingResponse {
							subreddit: peer.subreddit,
							posts,
							after: after_value(peer.after),
							peer_origin: Some(peer.origin),
							all_posts_filtered,
							all_posts_hidden_nsfw,
						});
					}
					Err(error) => warn!("json_api: Redlib peer API fallback failed for r/{sub}/{sort}: {error}"),
				}
			}
			return upstream_error(msg);
		}
	};
	let (all_posts_filtered, all_posts_hidden_nsfw) = finalize_posts(&req, &mut posts, true);
	let subreddit = subreddit_metadata(&sub, quarantined).await;

	json_response(ListingResponse {
		subreddit,
		posts,
		after: after_value(after),
		peer_origin: None,
		all_posts_filtered,
		all_posts_hidden_nsfw,
	})
}

pub async fn user_listing(req: Request<Body>) -> Result<Response<Body>, String> {
	let username = clean_route_param(req.param("name"));
	if username.is_empty() {
		return json_error("Missing username", 400);
	}
	let listing = clean_route_param(req.param("listing"));
	let listing = if listing.is_empty() { "overview".to_string() } else { listing };
	if !matches!(listing.as_str(), "overview" | "submitted" | "comments") {
		return json_error("Unsupported user listing", 400);
	}

	let defaults = if has_query_param(&req, "sort") {
		Vec::new()
	} else {
		vec![("sort", default_user_sort(&req))]
	};
	let query = query_with_raw_json(&req, &defaults);
	let path = format!("/user/{username}/{listing}.json?{query}");
	let (mut posts, after) = match Post::fetch(&path, false).await {
		Ok(result) => result,
		Err(msg) => return upstream_error(msg),
	};
	let (all_posts_filtered, all_posts_hidden_nsfw) = finalize_posts(&req, &mut posts, false);
	let user = crate::user::fetch_user_metadata_status(&username).await.unwrap_or_else(|_| User {
		name: username,
		..User::default()
	});

	json_response(UserResponse {
		user,
		posts,
		after: after_value(after),
		all_posts_filtered,
		all_posts_hidden_nsfw,
	})
}

pub async fn search(req: Request<Body>) -> Result<Response<Body>, String> {
	if query_param(&req, "q").unwrap_or_default().trim().is_empty() {
		return json_error("Missing search query", 400);
	}

	let sub = clean_route_param(req.param("sub"));
	let mut defaults = Vec::new();
	if setting(&req, "show_nsfw") == "on" && !sfw_only() {
		defaults.push(("include_over_18", "on".to_string()));
	}
	let query = query_with_raw_json(&req, &defaults);
	let path = if sub.is_empty() {
		format!("/search.json?{query}")
	} else {
		format!("/r/{}/search.json?{query}", sub.replace('+', "%2B"))
	};
	let quarantined = sub.is_empty() || subreddit::can_access_quarantine(&req, &sub);
	let (mut posts, after) = match Post::fetch(&path, quarantined).await {
		Ok(result) => result,
		Err(msg) => return upstream_error(msg),
	};
	let (all_posts_filtered, all_posts_hidden_nsfw) = finalize_posts(&req, &mut posts, false);

	json_response(SearchResponse {
		posts,
		after: after_value(after),
		all_posts_filtered,
		all_posts_hidden_nsfw,
	})
}

pub async fn post(req: Request<Body>) -> Result<Response<Body>, String> {
	let source_path = api_source_path(&req);
	let reddit_path = if source_path.ends_with('/') { source_path } else { format!("{source_path}/") };

	let defaults = if has_query_param(&req, "sort") {
		Vec::new()
	} else {
		let default_sort = setting(&req, "comment_sort");
		if default_sort.is_empty() {
			Vec::new()
		} else {
			vec![("sort", default_sort)]
		}
	};
	let path = format!("{}.json?{}", reddit_path, query_with_raw_json(&req, &defaults));
	let sub = clean_route_param(req.param("sub"));
	let quarantined = sub.is_empty() || subreddit::can_access_quarantine(&req, &sub);
	let response = match client::json(path, quarantined).await {
		Ok(response) => response,
		Err(msg) => return upstream_error(msg),
	};
	let Some(child) = post_child(&response) else {
		return json_error("Post not found", 404);
	};
	let mut post = parse_post(child).await;
	post.author.external_links = utils::cached_author_external_links(&post.author.name);

	let req_url = req.uri().to_string();
	if post.nsfw && should_be_nsfw_gated(&req, &req_url) {
		return json_error("NSFW content is hidden by this instance or your preferences", 403);
	}

	let empty_comments = Value::Null;
	let comment_listing = response.get(1).unwrap_or(&empty_comments);
	let highlighted_comment = clean_route_param(req.param("comment_id"));
	let comment_query = query_param(&req, "q").unwrap_or_default();
	let filters = get_filters(&req);
	let mut comments = post_page::parse_comments_for_api(comment_listing, &post.permalink, &post.author.name, &highlighted_comment, &filters, &comment_query, &req);
	let prefs = utils::Preferences::new(&req);
	crate::instance_scrape::apply_comment_prefs(&mut comments, &prefs);

	json_response(PostResponse { post, comments })
}

#[cfg(test)]
mod tests {
	use super::{api_metadata, ApiEnvelope, ListingResponse};

	#[test]
	fn api_envelope_serializes_rate_limit_metadata() {
		let envelope = ApiEnvelope {
			data: Some("ok"),
			error: None,
			meta: api_metadata(),
		};

		let value = serde_json::to_value(envelope).expect("envelope should serialize");

		assert_eq!(value["data"], "ok");
		assert!(value["meta"]["rate_limit"]["state"].is_string());
		assert!(value["meta"]["rate_limit"]["stale"].is_boolean());
	}

	#[test]
	fn listing_response_serializes_peer_origin_only_when_present() {
		let normal = serde_json::to_value(ListingResponse {
			subreddit: None,
			posts: Vec::new(),
			after: None,
			peer_origin: None,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
		})
		.expect("listing response should serialize");
		assert!(normal.get("peer_origin").is_none());

		let peer = serde_json::to_value(ListingResponse {
			subreddit: None,
			posts: Vec::new(),
			after: None,
			peer_origin: Some("http://192.168.50.251:18081".to_string()),
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
		})
		.expect("listing response should serialize");
		assert_eq!(peer["peer_origin"], "http://192.168.50.251:18081");
	}
}
