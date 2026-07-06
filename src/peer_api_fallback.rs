use crate::client;
use crate::config::get_setting;
use crate::utils::{Post, Subreddit};
use hyper::body::to_bytes;
use hyper::client::HttpConnector;
use hyper::header::{HeaderValue, ACCEPT, USER_AGENT};
use hyper::{Body, Client as HyperClient, Method, Request, StatusCode};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use log::{info, warn};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use url::{form_urlencoded, Url};

const DEFAULT_TIMEOUT_SECS: u64 = 5;
const PEER_FAILURE_BACKOFF_SECS: u64 = 60;
const PEER_API_NOTICE: &str = "Showing content fetched from a configured Redlib API peer because Reddit rate limited this request.";
const PEER_API_UA: &str = "Redlib peer API fallback";
pub(crate) const PEER_API_FALLBACK_HEADER: &str = "x-redlib-peer-api-fallback";
pub(crate) const PEER_API_FORCE_HEADER: &str = "x-redlib-force-peer";
const PEER_API_FORCE_QUERY_PARAM: &str = "force_peer";

static PEER_FAILURE_BACKOFF: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static LAST_SUCCESSFUL_PEER: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));
static PEER_API_CLIENT: LazyLock<HyperClient<HttpsConnector<HttpConnector>, Body>> = LazyLock::new(|| {
	let connector = hyper_rustls::HttpsConnectorBuilder::new()
		.with_tls_config(rustls::ClientConfig::builder().with_safe_defaults().with_native_roots().with_no_client_auth())
		.https_or_http()
		.enable_http2()
		.build();
	let mut builder = HyperClient::builder();
	if cfg!(test) {
		builder.pool_max_idle_per_host(0);
	}
	builder.build::<_, Body>(connector)
});

#[derive(Clone)]
pub(crate) struct PeerSubredditListing {
	pub(crate) origin: String,
	pub(crate) subreddit: Option<Subreddit>,
	pub(crate) posts: Vec<Post>,
	pub(crate) after: String,
}

#[derive(Deserialize)]
struct ApiEnvelope<T> {
	data: Option<T>,
	error: Option<String>,
}

#[derive(Deserialize)]
struct PeerListingResponse {
	subreddit: Option<Subreddit>,
	posts: Vec<Post>,
	after: Option<String>,
}

pub(crate) fn peer_api_notice() -> &'static str {
	PEER_API_NOTICE
}

pub(crate) fn request_is_peer_api_fallback(req: &Request<Body>) -> bool {
	req.headers().contains_key(PEER_API_FALLBACK_HEADER)
}

pub(crate) fn peer_api_fallback_enabled() -> bool {
	!configured_peer_origins().is_empty()
}

pub(crate) fn request_should_force_peer_api_fallback(req: &Request<Body>) -> bool {
	request_should_force_peer_api_fallback_with_setting(req, get_setting("REDLIB_PEER_API_FALLBACK_FORCE_TESTING").as_deref())
}

fn request_should_force_peer_api_fallback_with_setting(req: &Request<Body>, force_testing: Option<&str>) -> bool {
	config_value_enabled(force_testing) && !request_is_peer_api_fallback(req) && request_requests_force_peer(req)
}

fn request_requests_force_peer(req: &Request<Body>) -> bool {
	req
		.uri()
		.query()
		.is_some_and(|query| form_urlencoded::parse(query.as_bytes()).any(|(key, value)| key == PEER_API_FORCE_QUERY_PARAM && request_value_enabled(&value)))
		|| req.headers().get(PEER_API_FORCE_HEADER).is_some_and(header_value_enabled)
}

fn config_value_enabled(value: Option<&str>) -> bool {
	value.is_some_and(|value| request_value_enabled(value.trim()))
}

fn request_value_enabled(value: &str) -> bool {
	matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn header_value_enabled(value: &HeaderValue) -> bool {
	value.to_str().ok().is_some_and(request_value_enabled)
}

pub(crate) async fn fetch_subreddit_listing(subreddit: &str, sort: &str, query: Option<&str>) -> Result<PeerSubredditListing, String> {
	let origins = prioritize_last_successful_peer(configured_peer_origins());
	if origins.is_empty() {
		return Err("no configured Redlib peer API fallback sources".to_string());
	}

	let path = build_subreddit_api_path(subreddit, sort, query);
	info!(
		"peer_api_fallback: attempting subreddit listing fallback for {} using {} configured peer(s)",
		path,
		origins.len()
	);

	let mut last_error = "no peer attempted".to_string();
	for origin in origins {
		if peer_is_backed_off(&origin) {
			info!("peer_api_fallback: skipping backed off peer {}", origin);
			continue;
		}

		let url = format!("{origin}{path}");
		match fetch_peer_json(&url).await {
			Ok((status, body)) => match parse_subreddit_listing_response(status, &body) {
				Ok(mut listing) => {
					normalize_peer_posts(&mut listing.posts);
					if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
						client::record_peer_ratelimit_from_api_value(&origin, &value);
					}
					note_peer_success(&origin);
					info!("peer_api_fallback: fetched {} from {}", path, origin);
					return Ok(PeerSubredditListing {
						origin,
						subreddit: listing.subreddit,
						posts: listing.posts,
						after: listing.after.unwrap_or_default(),
					});
				}
				Err(error) => {
					note_peer_failure(&origin);
					warn!("peer_api_fallback: peer {} returned unusable listing for {}: {}", origin, path, error);
					last_error = error;
				}
			},
			Err(error) => {
				note_peer_failure(&origin);
				warn!("peer_api_fallback: peer {} failed for {}: {}", origin, path, error);
				last_error = error;
			}
		}
	}

	Err(last_error)
}

fn normalize_peer_posts(posts: &mut [Post]) {
	for post in posts {
		post.flags.normalize_template_strings();
	}
}

fn configured_peer_origins() -> Vec<String> {
	configured_peer_origins_from(get_setting("REDLIB_PEER_API_FALLBACK_SOURCES").as_deref().unwrap_or_default())
}

fn configured_peer_origins_from(configured: &str) -> Vec<String> {
	configured
		.split(',')
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.filter_map(|value| normalize_origin(value).ok())
		.collect()
}

fn normalize_origin(value: &str) -> Result<String, String> {
	let parsed = Url::parse(value).map_err(|error| error.to_string())?;
	if !matches!(parsed.scheme(), "http" | "https") {
		return Err("peer origin must use http or https".to_string());
	}
	let mut origin = parsed.origin().ascii_serialization();
	if let Some(path) = parsed.path().strip_suffix('/') {
		if !path.is_empty() {
			origin.push_str(path);
		}
	}
	Ok(origin.trim_end_matches('/').to_string())
}

fn build_subreddit_api_path(subreddit: &str, sort: &str, query: Option<&str>) -> String {
	let encoded_subreddit = subreddit.replace('+', "%2B");
	let normalized_sort = sort.trim_end_matches(".js").trim_matches('/');
	let path = if normalized_sort.is_empty() || normalized_sort == "hot" {
		format!("/api/r/{encoded_subreddit}")
	} else {
		format!("/api/r/{encoded_subreddit}/{normalized_sort}")
	};
	let filtered_query = filtered_peer_query(query.unwrap_or_default());
	if filtered_query.is_empty() {
		path
	} else {
		format!("{path}?{filtered_query}")
	}
}

fn filtered_peer_query(query: &str) -> String {
	form_urlencoded::Serializer::new(String::new())
		.extend_pairs(
			form_urlencoded::parse(query.as_bytes())
				.into_owned()
				.filter(|(key, _)| !matches!(key.as_str(), "cache_page" | "feed_session" | "front_cursor" | "raw_json" | PEER_API_FORCE_QUERY_PARAM)),
		)
		.finish()
}

fn request_timeout() -> Duration {
	let secs = get_setting("REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS")
		.and_then(|value| value.parse::<u64>().ok())
		.filter(|value| *value > 0)
		.unwrap_or(DEFAULT_TIMEOUT_SECS);
	Duration::from_secs(secs)
}

async fn fetch_peer_json(url: &str) -> Result<(StatusCode, String), String> {
	let request = Request::builder()
		.method(Method::GET)
		.uri(url)
		.header(ACCEPT, "application/json")
		.header(USER_AGENT, PEER_API_UA)
		.header(PEER_API_FALLBACK_HEADER, "1")
		.body(Body::empty())
		.map_err(|error| error.to_string())?;
	let response = tokio::time::timeout(request_timeout(), PEER_API_CLIENT.request(request))
		.await
		.map_err(|_| "peer API request timed out".to_string())?
		.map_err(|error| error.to_string())?;
	let status = response.status();
	let body = to_bytes(response.into_body()).await.map_err(|error| error.to_string())?;
	let body = String::from_utf8_lossy(&body).to_string();
	Ok((status, body))
}

fn parse_subreddit_listing_response(status: StatusCode, body: &str) -> Result<PeerListingResponse, String> {
	if status != StatusCode::OK {
		return Err(format!("peer returned HTTP {}", status.as_u16()));
	}
	let envelope: ApiEnvelope<PeerListingResponse> = serde_json::from_str(body).map_err(|error| error.to_string())?;
	if let Some(error) = envelope.error.filter(|error| !error.trim().is_empty()) {
		return Err(format!("peer API error: {error}"));
	}
	envelope.data.ok_or_else(|| "peer API response did not include data".to_string())
}

fn peer_is_backed_off(origin: &str) -> bool {
	PEER_FAILURE_BACKOFF
		.lock()
		.ok()
		.and_then(|backoff| backoff.get(origin).copied())
		.is_some_and(|failed_at| failed_at.elapsed().as_secs() < PEER_FAILURE_BACKOFF_SECS)
}

fn note_peer_failure(origin: &str) {
	if let Ok(mut backoff) = PEER_FAILURE_BACKOFF.lock() {
		backoff.insert(origin.to_string(), Instant::now());
	}
}

fn note_peer_success(origin: &str) {
	if let Ok(mut backoff) = PEER_FAILURE_BACKOFF.lock() {
		backoff.remove(origin);
	}
	if let Ok(mut last) = LAST_SUCCESSFUL_PEER.lock() {
		*last = Some(origin.to_string());
	}
}

fn prioritize_last_successful_peer(mut origins: Vec<String>) -> Vec<String> {
	let last_successful = LAST_SUCCESSFUL_PEER.lock().ok().and_then(|value| value.clone());
	let Some(last_successful) = last_successful else {
		return origins;
	};
	let Some(index) = origins.iter().position(|origin| *origin == last_successful) else {
		return origins;
	};
	if index > 0 {
		let origin = origins.remove(index);
		origins.insert(0, origin);
	}
	origins
}

#[cfg(test)]
mod tests {
	use super::*;
	use hyper::service::{make_service_fn, service_fn};
	use hyper::Response;
	use std::convert::Infallible;

	#[test]
	fn parses_configured_peer_origins() {
		assert_eq!(
			configured_peer_origins_from(" http://127.0.0.1:18081/, https://example.com/redlib/ "),
			vec!["http://127.0.0.1:18081".to_string(), "https://example.com/redlib".to_string()]
		);
	}

	#[test]
	fn builds_subreddit_api_path_without_internal_cursor_params() {
		assert_eq!(
			build_subreddit_api_path("rust+programming", "new", Some("after=t3_next&cache_page=2&flair=Help&raw_json=1&force_peer=1")),
			"/api/r/rust%2Bprogramming/new?after=t3_next&flair=Help"
		);
	}

	#[test]
	fn force_peer_override_requires_enabled_gate() {
		let request = Request::builder().uri("/api/r/rust?force_peer=1").body(Body::empty()).unwrap();
		assert!(request_should_force_peer_api_fallback_with_setting(&request, Some("on")));
		assert!(!request_should_force_peer_api_fallback_with_setting(&request, Some("off")));
		assert!(!request_should_force_peer_api_fallback_with_setting(&request, None));
	}

	#[test]
	fn force_peer_override_accepts_header_when_enabled() {
		let request = Request::builder().uri("/api/r/rust").header(PEER_API_FORCE_HEADER, "true").body(Body::empty()).unwrap();

		assert!(request_should_force_peer_api_fallback_with_setting(&request, Some("yes")));
	}

	#[test]
	fn force_peer_override_ignores_peer_fallback_requests() {
		let request = Request::builder()
			.uri("/api/r/rust?force_peer=1")
			.header(PEER_API_FALLBACK_HEADER, "1")
			.body(Body::empty())
			.unwrap();

		assert!(!request_should_force_peer_api_fallback_with_setting(&request, Some("on")));
	}

	#[tokio::test]
	async fn fetch_peer_json_supports_http_origin() {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let addr = listener.local_addr().unwrap();
		let server = hyper::Server::from_tcp(listener).unwrap().serve(make_service_fn(|_| async {
			Ok::<_, Infallible>(service_fn(|req: Request<Body>| async move {
				assert_eq!(req.uri().path(), "/api/r/rust");
				assert_eq!(req.headers().get(PEER_API_FALLBACK_HEADER).and_then(|value| value.to_str().ok()), Some("1"));
				Ok::<_, Infallible>(
					Response::builder()
						.status(StatusCode::OK)
						.header("content-type", "application/json")
						.body(Body::from(r#"{"data":{"subreddit":null,"posts":[],"after":null},"error":null}"#))
						.unwrap(),
				)
			}))
		}));
		let server_handle = tokio::spawn(server);

		let (status, body) = fetch_peer_json(&format!("http://{addr}/api/r/rust")).await.unwrap();
		assert_eq!(status, StatusCode::OK);
		assert!(body.contains(r#""posts":[]"#));

		server_handle.abort();
	}

	#[test]
	fn parses_peer_listing_envelope() {
		let body = r#"{"data":{"subreddit":{"name":"rust","title":"Rust","description":"","info":"","has_sidebar":false,"icon":"","members":["0","0"],"active":["0","0"],"wiki":false,"nsfw":false},"posts":[],"after":null},"error":null}"#;
		let listing = parse_subreddit_listing_response(StatusCode::OK, body).unwrap();
		assert_eq!(listing.subreddit.unwrap().name, "rust");
		assert_eq!(listing.posts.len(), 0);
		assert_eq!(listing.after, None);
	}

	#[test]
	fn rejects_peer_api_errors() {
		let body = r#"{"data":null,"error":"rate limited"}"#;
		let error = match parse_subreddit_listing_response(StatusCode::OK, body) {
			Ok(_) => panic!("expected peer API error"),
			Err(error) => error,
		};
		assert!(error.contains("rate limited"));
	}

	#[test]
	fn rejects_non_success_status() {
		let error = match parse_subreddit_listing_response(StatusCode::BAD_GATEWAY, "{}") {
			Ok(_) => panic!("expected HTTP status error"),
			Err(error) => error,
		};
		assert!(error.contains("502"));
	}
}
