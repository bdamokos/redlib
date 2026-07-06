use arc_swap::ArcSwap;
use cached::proc_macro::cached;
use futures_lite::{future::Boxed, FutureExt};
use hyper::client::HttpConnector;
use hyper::header::HeaderValue;
use hyper::{body, body::Buf, header, Body, Client, Method, Request, Response, StatusCode, Uri};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use libflate::gzip;
use log::{error, info, trace, warn};
use percent_encoding::{percent_encode, CONTROLS};
use serde_json::Value;
use wreq::redirect::Policy;
use wreq::{Client as WreqClient, EmulationFactory, Method as WreqMethod, Response as WreqResponse};
use wreq_util::{Emulation, EmulationOS, EmulationOption};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{io, result::Result};

use crate::config::get_setting;
use crate::dbg_msg;
use crate::oauth::{force_refresh_token, oauth_is_available, Oauth, OauthBackendImpl};
use crate::server::RequestExt;
use crate::utils::{format_url, Post};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";
const REDDIT_URL_BASE_HOST: &str = "oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";

const IMAGE_PRELOAD_TTL: Duration = Duration::from_secs(15 * 60);
const IMAGE_PRELOAD_MAX_ENTRY_BYTES: usize = 6 * 1024 * 1024;
const IMAGE_PRELOAD_MAX_TOTAL_BYTES: usize = 96 * 1024 * 1024;
const IMAGE_PRELOAD_MAX_POSTS: usize = 8;
const IMAGE_PRELOAD_MAX_URLS: usize = 12;
const IMAGE_PRELOAD_CONCURRENCY: usize = 3;
const IMAGE_PRELOAD_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(8);

pub static HTTPS_CONNECTOR: LazyLock<HttpsConnector<HttpConnector>> = LazyLock::new(|| {
	hyper_rustls::HttpsConnectorBuilder::new()
		.with_tls_config(
			rustls::ClientConfig::builder()
				// These are the Firefox 145.0 cipher suite,
				// minus the suites missing forward-secrecy support,
				// in the same order.
				// https://github.com/redlib-org/redlib/issues/446#issuecomment-3609306592
				.with_cipher_suites(&[
					rustls::cipher_suite::TLS13_AES_256_GCM_SHA384,
					rustls::cipher_suite::TLS13_AES_128_GCM_SHA256,
					rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
					rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
					rustls::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
					rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
					rustls::cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
					rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
					rustls::cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
				])
				// .with_safe_default_cipher_suites()
				.with_safe_default_kx_groups()
				.with_safe_default_protocol_versions()
				.unwrap()
				.with_native_roots()
				.with_no_client_auth(),
		)
		.https_only()
		.enable_http2()
		.build()
});

pub static HYPER_CLIENT: LazyLock<Client<HttpsConnector<HttpConnector>>> = LazyLock::new(|| {
	let mut builder = Client::builder();
	if cfg!(test) {
		// Async tests create and tear down multiple Tokio runtimes. Disabling the
		// idle pool avoids reusing connections whose dispatch task belonged to a
		// runtime that has already been dropped.
		builder.pool_max_idle_per_host(0);
	}
	builder.build::<_, Body>(HTTPS_CONNECTOR.clone())
});

pub use HYPER_CLIENT as CLIENT;

pub static WREQ_CLIENT: LazyLock<WreqClient> = LazyLock::new(build_wreq_client);

pub static OAUTH_CLIENT: LazyLock<ArcSwap<Oauth>> = LazyLock::new(|| ArcSwap::new(Oauth::unavailable("OAuth initialization pending").into()));

pub static OAUTH_RATELIMIT_REMAINING: AtomicU16 = AtomicU16::new(99);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);
pub static OAUTH_BLOCKED_UNTIL: AtomicU64 = AtomicU64::new(0);
pub static LAST_REDDIT_RATELIMIT: LazyLock<std::sync::Mutex<Option<RedditRateLimitSnapshot>>> = LazyLock::new(|| std::sync::Mutex::new(None));
static IMAGE_PRELOAD_CACHE: LazyLock<Mutex<ImagePreloadCache>> = LazyLock::new(|| Mutex::new(ImagePreloadCache::default()));
static IMAGE_PRELOAD_SEMAPHORE: LazyLock<Arc<tokio::sync::Semaphore>> = LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(IMAGE_PRELOAD_CONCURRENCY)));

const BREAKER_FLOOR: u16 = 5;
const BLOCK_COOLDOWN_SECS: u64 = 900;

#[derive(Clone)]
struct CachedImageResponse {
	status: hyper::StatusCode,
	content_type: Option<HeaderValue>,
	cache_control: Option<HeaderValue>,
	body: body::Bytes,
	inserted_at: Instant,
}

#[derive(Default)]
struct ImagePreloadCache {
	entries: HashMap<String, CachedImageResponse>,
	order: VecDeque<String>,
	inflight: HashSet<String>,
	total_bytes: usize,
}

impl ImagePreloadCache {
	fn prune_expired(&mut self) {
		let now = Instant::now();
		let expired = self
			.entries
			.iter()
			.filter_map(|(url, entry)| (now.duration_since(entry.inserted_at) > IMAGE_PRELOAD_TTL).then(|| url.clone()))
			.collect::<Vec<_>>();

		for url in expired {
			self.remove(&url);
		}
	}

	fn remove(&mut self, url: &str) {
		if let Some(entry) = self.entries.remove(url) {
			self.total_bytes = self.total_bytes.saturating_sub(entry.body.len());
		}
		self.order.retain(|key| key != url);
		self.inflight.remove(url);
	}

	fn get(&mut self, url: &str) -> Option<CachedImageResponse> {
		self.prune_expired();
		self.entries.get(url).cloned()
	}

	fn is_inflight(&mut self, url: &str) -> bool {
		self.prune_expired();
		self.inflight.contains(url)
	}

	fn mark_inflight(&mut self, url: &str) -> bool {
		self.prune_expired();
		if self.entries.contains_key(url) || self.inflight.contains(url) {
			return false;
		}
		self.inflight.insert(url.to_string());
		true
	}

	fn insert(&mut self, url: String, entry: CachedImageResponse) {
		self.inflight.remove(&url);
		if entry.body.len() > IMAGE_PRELOAD_MAX_ENTRY_BYTES {
			return;
		}
		if let Some(old) = self.entries.remove(&url) {
			self.total_bytes = self.total_bytes.saturating_sub(old.body.len());
			self.order.retain(|key| key != &url);
		}
		self.total_bytes += entry.body.len();
		self.order.push_back(url.clone());
		self.entries.insert(url, entry);
		while self.total_bytes > IMAGE_PRELOAD_MAX_TOTAL_BYTES {
			let Some(oldest) = self.order.pop_front() else {
				break;
			};
			if let Some(old) = self.entries.remove(&oldest) {
				self.total_bytes = self.total_bytes.saturating_sub(old.body.len());
			}
		}
	}

	fn clear_inflight(&mut self, url: &str) {
		self.inflight.remove(url);
	}
}

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedditTransport {
	Wreq,
	Hyper,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedditTransportMode {
	Auto,
	WreqOnly,
	HyperOnly,
}

fn reddit_transport_mode() -> RedditTransportMode {
	match get_setting("REDLIB_REDDIT_TRANSPORT").unwrap_or_else(|| "auto".to_string()).to_ascii_lowercase().as_str() {
		"wreq" => RedditTransportMode::WreqOnly,
		"hyper" => RedditTransportMode::HyperOnly,
		_ => RedditTransportMode::Auto,
	}
}

fn build_wreq_client() -> WreqClient {
	let emulations = [Emulation::Chrome145, Emulation::Firefox147];
	let emulation_os = [EmulationOS::Android, EmulationOS::Windows];
	let rand = fastrand::usize(..);
	let emulation = EmulationOption::builder()
		.emulation(emulations[rand % emulations.len()])
		.emulation_os(emulation_os[rand % emulation_os.len()])
		.build()
		.emulation();

	info!("Building Reddit Wreq client with emulation {:?}", emulation);
	WreqClient::builder()
		.emulation(emulation)
		.redirect(Policy::none())
		.build()
		.expect("wreq client should build")
}

#[derive(Clone, Debug)]
pub struct RedditRateLimitSnapshot {
	pub remaining: Option<f32>,
	pub reset_seconds: Option<f32>,
	pub used: Option<f32>,
	pub retry_after_seconds: Option<f32>,
	pub status: u16,
	pub observed_at: Instant,
}

impl RedditRateLimitSnapshot {
	pub fn is_rate_limited(&self) -> bool {
		self.status == 429 || self.retry_after_seconds.is_some()
	}

	fn effective_seconds_remaining(seconds: f32, observed_at: Instant) -> f32 {
		(seconds - observed_at.elapsed().as_secs_f32()).max(0.0)
	}

	fn effective_reset_seconds(&self) -> Option<f32> {
		self.reset_seconds.map(|seconds| Self::effective_seconds_remaining(seconds, self.observed_at))
	}

	fn effective_retry_after_seconds(&self) -> Option<f32> {
		self.retry_after_seconds.map(|seconds| Self::effective_seconds_remaining(seconds, self.observed_at))
	}

	pub fn remaining_requests_per_second(&self) -> Option<f32> {
		match (self.remaining, self.effective_reset_seconds()) {
			(Some(remaining), Some(reset_seconds)) if remaining > 0.0 && reset_seconds > 0.0 => Some(remaining / reset_seconds),
			_ => None,
		}
	}

	pub fn suggested_wait_duration(&self) -> Option<Duration> {
		if let Some(retry_after) = self.effective_retry_after_seconds() {
			if retry_after > 0.0 {
				return Some(Duration::from_secs_f32(retry_after));
			}
		}

		match (self.remaining, self.effective_reset_seconds()) {
			(Some(remaining), Some(reset_seconds)) if remaining <= 0.0 && reset_seconds > 0.0 => Some(Duration::from_secs_f32(reset_seconds)),
			(Some(remaining), Some(reset_seconds)) if remaining > 0.0 && reset_seconds > 0.0 => {
				let spacing = reset_seconds / remaining.max(1.0);
				if spacing > 1.0 {
					Some(Duration::from_secs_f32(spacing.min(5.0)))
				} else {
					None
				}
			}
			_ => None,
		}
	}
}

fn parse_header_f32(headers: &hyper::HeaderMap<HeaderValue>, key: &str) -> Option<f32> {
	headers.get(key).and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<f32>().ok())
}

fn capture_ratelimit_snapshot(response: &Response<Body>) -> Option<RedditRateLimitSnapshot> {
	let headers = response.headers();
	let remaining = parse_header_f32(headers, "x-ratelimit-remaining");
	let reset_seconds = parse_header_f32(headers, "x-ratelimit-reset");
	let used = parse_header_f32(headers, "x-ratelimit-used");
	let retry_after_seconds = parse_header_f32(headers, header::RETRY_AFTER.as_str());

	if remaining.is_none() && reset_seconds.is_none() && used.is_none() && retry_after_seconds.is_none() {
		return None;
	}

	Some(RedditRateLimitSnapshot {
		remaining,
		reset_seconds,
		used,
		retry_after_seconds,
		status: response.status().as_u16(),
		observed_at: Instant::now(),
	})
}

fn store_ratelimit_snapshot(snapshot: RedditRateLimitSnapshot) {
	if let Some(remaining) = snapshot.remaining {
		OAUTH_RATELIMIT_REMAINING.store(remaining.round().max(0.0) as u16, Ordering::SeqCst);
	}

	if let Ok(mut state) = LAST_REDDIT_RATELIMIT.lock() {
		*state = Some(snapshot);
	}
}

pub fn latest_ratelimit_snapshot() -> Option<RedditRateLimitSnapshot> {
	LAST_REDDIT_RATELIMIT.lock().ok().and_then(|state| state.clone())
}

async fn pace_reddit_request_if_needed() {
	let snapshot = latest_ratelimit_snapshot();
	let Some(snapshot) = snapshot else {
		return;
	};

	let Some(wait_duration) = snapshot.suggested_wait_duration() else {
		return;
	};

	if wait_duration.is_zero() {
		return;
	}

	if snapshot.is_rate_limited() {
		warn!(
			"Reddit rate limit hit. Waiting {:?} before next API request. Remaining: {:?}, reset: {:?}, retry-after: {:?}",
			wait_duration, snapshot.remaining, snapshot.reset_seconds, snapshot.retry_after_seconds
		);
		tokio::time::sleep(wait_duration).await;
		return;
	}

	if let Some(requests_per_second) = snapshot.remaining_requests_per_second() {
		if requests_per_second < 1.0 {
			trace!(
				"Pacing Reddit API request for {:?} to respect budget of {:.3} requests/sec",
				wait_duration,
				requests_per_second
			);
			tokio::time::sleep(wait_duration).await;
		}
	}
}

trait IntoHyperResponse {
	fn into_hyper_response(self) -> Response<Body>;
}

impl IntoHyperResponse for WreqResponse {
	fn into_hyper_response(self) -> Response<Body> {
		let status = self.status();
		let version = self.version();
		let mut builder = Response::builder().status(status.as_u16()).version(match version {
			wreq::Version::HTTP_09 => hyper::Version::HTTP_09,
			wreq::Version::HTTP_10 => hyper::Version::HTTP_10,
			wreq::Version::HTTP_11 => hyper::Version::HTTP_11,
			wreq::Version::HTTP_2 => hyper::Version::HTTP_2,
			wreq::Version::HTTP_3 => hyper::Version::HTTP_3,
			_ => hyper::Version::HTTP_11,
		});

		for (name, value) in self.headers() {
			if let (Ok(name), Ok(value)) = (header::HeaderName::from_bytes(name.as_str().as_bytes()), header::HeaderValue::from_bytes(value.as_bytes())) {
				builder = builder.header(name, value);
			}
		}

		builder.body(Body::wrap_stream(self.bytes_stream())).unwrap_or_default()
	}
}

fn wreq_builder_for_method(method: &Method, url: &str) -> Result<wreq::RequestBuilder, String> {
	let request_method = WreqMethod::from_bytes(method.as_str().as_bytes()).map_err(|error| error.to_string())?;
	Ok(WREQ_CLIENT.request(request_method, url))
}

pub(crate) async fn send_reddit_transport_request(
	method: &'static Method,
	url: &str,
	headers: &[(String, String)],
	body_bytes: Vec<u8>,
) -> Result<(Response<Body>, RedditTransport), String> {
	match reddit_transport_mode() {
		RedditTransportMode::HyperOnly => send_hyper_reddit_request(method, url, headers, body_bytes)
			.await
			.map(|response| (response, RedditTransport::Hyper)),
		RedditTransportMode::WreqOnly => send_wreq_reddit_request(method, url, headers, body_bytes)
			.await
			.map(|response| (response, RedditTransport::Wreq)),
		RedditTransportMode::Auto => match send_wreq_reddit_request(method, url, headers, body_bytes.clone()).await {
			Ok(response) => Ok((response, RedditTransport::Wreq)),
			Err(error) if should_retry_with_hyper(&error) => {
				warn!("Reddit Wreq transport failed for {} {}: {}. Retrying with hyper.", method, url, error);
				send_hyper_reddit_request(method, url, headers, body_bytes)
					.await
					.map(|response| (response, RedditTransport::Hyper))
			}
			Err(error) => Err(error),
		},
	}
}

async fn send_wreq_reddit_request(method: &'static Method, url: &str, headers: &[(String, String)], body_bytes: Vec<u8>) -> Result<Response<Body>, String> {
	let mut builder = wreq_builder_for_method(method, url)?;
	for (key, value) in headers {
		builder = builder.header(key, value);
	}
	if !body_bytes.is_empty() {
		builder = builder.body(body_bytes);
	}
	let response = builder.send().await.map_err(|error| error.to_string())?;
	Ok(response.into_hyper_response())
}

async fn send_hyper_reddit_request(method: &'static Method, url: &str, headers: &[(String, String)], body_bytes: Vec<u8>) -> Result<Response<Body>, String> {
	let client: &LazyLock<Client<_, Body>> = &HYPER_CLIENT;
	let mut builder = Request::builder().method(method).uri(url);
	for (key, value) in headers {
		builder = builder.header(key, value);
	}
	let request_body = if body_bytes.is_empty() { Body::empty() } else { Body::from(body_bytes) };
	let request = builder.body(request_body).map_err(|_| "Post url contains non-ASCII characters".to_string())?;
	client.request(request).await.map_err(|error| error.to_string())
}

fn should_retry_with_hyper(error: &str) -> bool {
	let lower = error.to_ascii_lowercase();
	lower.contains("tls")
		|| lower.contains("ssl")
		|| lower.contains("connect")
		|| lower.contains("protocol")
		|| lower.contains("http2")
		|| lower.contains("h2")
		|| lower.contains("eof")
		|| lower.contains("dns")
}

/// Gets the canonical path for a resource on Reddit. This is accomplished by
/// making a `HEAD` request to Reddit at the path given in `path`.
///
/// This function returns `Ok(Some(path))`, where `path`'s value is identical
/// to that of the value of the argument `path`, if Reddit responds to our
/// `HEAD` request with a 2xx-family HTTP code. It will also return an
/// `Ok(Some(String))` if Reddit responds to our `HEAD` request with a
/// `Location` header in the response, and the HTTP code is in the 3xx-family;
/// the `String` will contain the path as reported in `Location`. The return
/// value is `Ok(None)` if Reddit responded with a 3xx, but did not provide a
/// `Location` header. An `Err(String)` is returned if Reddit responds with a
/// 429, or if we were unable to decode the value in the `Location` header.
#[cached(size = 1024, time = 600, result = true)]
#[async_recursion::async_recursion]
pub async fn canonical_path(path: String, tries: i8) -> Result<Option<String>, String> {
	if tries == 0 {
		return Ok(None);
	}

	// for each URL pair, try the HEAD request
	let res = {
		// for url base and host in URL_PAIRS, try reddit_short_head(path.clone(), true, url_base, url_base_host) and if it succeeds, set res. else, res = None
		let mut res = None;
		for (url_base, url_base_host) in URL_PAIRS {
			res = reddit_short_head(path.clone(), true, url_base, url_base_host).await.ok();
			if let Some(res) = &res {
				if !res.status().is_client_error() {
					break;
				}
			}
		}
		res
	};

	let res = res.ok_or_else(|| "Unable to make HEAD request to Reddit.".to_string())?;
	let status = res.status().as_u16();
	let policy_error = res.headers().get(header::RETRY_AFTER).is_some();

	match status {
		// If Reddit responds with a 2xx, then the path is already canonical.
		200..=299 => Ok(Some(path)),

		// If Reddit responds with a 301, then the path is redirected.
		301 => match res.headers().get(header::LOCATION) {
			Some(val) => {
				let Ok(original) = val.to_str() else {
					return Err("Unable to decode Location header.".to_string());
				};

				// We need to strip the .json suffix from the original path.
				// In addition, we want to remove share parameters.
				// Cut it off here instead of letting it propagate all the way
				// to main.rs
				let stripped_uri = original.strip_suffix(".json").unwrap_or(original).split('?').next().unwrap_or_default();

				// The reason why we now have to format_url, is because the new OAuth
				// endpoints seem to return full paths, instead of relative paths.
				// So we need to strip the .json suffix from the original path, and
				// also remove all Reddit domain parts with format_url.
				// Otherwise, it will literally redirect to Reddit.com.
				let uri = format_url(stripped_uri);

				// Decrement tries and try again
				canonical_path(uri, tries - 1).await
			}
			None => Ok(None),
		},

		// If Reddit responds with anything other than 3xx (except for the 2xx and 301
		// as above), return a None.
		300..=399 => Ok(None),

		// Rate limiting
		429 => Err("Too many requests.".to_string()),

		// Special condition rate limiting - https://github.com/redlib-org/redlib/issues/229
		403 if policy_error => Err("Too many requests.".to_string()),

		_ => Ok(
			res
				.headers()
				.get(header::LOCATION)
				.map(|val| percent_encode(val.as_bytes(), CONTROLS).to_string().trim_start_matches(REDDIT_URL_BASE).to_string()),
		),
	}
}

fn cacheable_image_url(url: &str) -> bool {
	url.starts_with("https://i.redd.it/")
		|| url.starts_with("https://preview.redd.it/")
		|| url.starts_with("https://external-preview.redd.it/")
		|| url.starts_with("https://a.thumbs.redditmedia.com/")
		|| url.starts_with("https://b.thumbs.redditmedia.com/")
}

fn local_proxy_image_to_upstream(local_url: &str) -> Option<String> {
	let (path, query) = local_url.split_once('?').unwrap_or((local_url, ""));
	let suffix = if query.is_empty() { String::new() } else { format!("?{query}") };

	if let Some(path) = path.strip_prefix("/img/") {
		return Some(format!("https://i.redd.it/{path}{suffix}"));
	}
	if let Some(path) = path.strip_prefix("/preview/pre/") {
		return Some(format!("https://preview.redd.it/{path}{suffix}"));
	}
	if let Some(path) = path.strip_prefix("/preview/external-pre/") {
		return Some(format!("https://external-preview.redd.it/{path}{suffix}"));
	}
	if let Some(id) = path.strip_prefix("/thumb/a/") {
		return Some(format!("https://a.thumbs.redditmedia.com/{id}{suffix}"));
	}
	if let Some(id) = path.strip_prefix("/thumb/b/") {
		return Some(format!("https://b.thumbs.redditmedia.com/{id}{suffix}"));
	}

	None
}

fn collect_post_image_urls(posts: &[Post], prefs: &crate::utils::Preferences) -> Vec<String> {
	let mut urls = Vec::new();
	let mut seen = HashSet::new();
	let prefer_full = prefs.image_quality == "best";

	fn push_image_url(urls: &mut Vec<String>, seen: &mut HashSet<String>, url: &str) {
		if url.is_empty() {
			return;
		}
		let upstream = local_proxy_image_to_upstream(url).unwrap_or_else(|| url.to_string());
		if !cacheable_image_url(&upstream) || !seen.insert(upstream.clone()) {
			return;
		}
		urls.push(upstream);
	}

	for post in posts.iter().take(IMAGE_PRELOAD_MAX_POSTS) {
		if post.flags.nsfw && prefs.show_nsfw != "on" {
			continue;
		}
		if post.post_type == "image" {
			if prefer_full {
				push_image_url(&mut urls, &mut seen, &post.media.url);
			} else {
				push_image_url(
					&mut urls,
					&mut seen,
					if post.media.preview_url.is_empty() { &post.media.url } else { &post.media.preview_url },
				);
			}
		} else if post.post_type == "gallery" {
			for image in &post.gallery {
				if prefer_full {
					push_image_url(&mut urls, &mut seen, &image.url);
				} else {
					push_image_url(&mut urls, &mut seen, if image.preview_url.is_empty() { &image.url } else { &image.preview_url });
				}
				if urls.len() >= IMAGE_PRELOAD_MAX_URLS {
					return urls;
				}
			}
		} else if !post.thumbnail.url.is_empty() {
			push_image_url(&mut urls, &mut seen, &post.thumbnail.url);
		}

		if urls.len() >= IMAGE_PRELOAD_MAX_URLS {
			return urls;
		}
	}

	urls
}

pub fn preload_post_images(posts: &[Post], prefs: &crate::utils::Preferences) {
	if cfg!(test) {
		return;
	}

	let Ok(handle) = tokio::runtime::Handle::try_current() else {
		return;
	};

	for url in collect_post_image_urls(posts, prefs) {
		let should_spawn = IMAGE_PRELOAD_CACHE.lock().map(|mut cache| cache.mark_inflight(&url)).unwrap_or(false);
		if !should_spawn {
			continue;
		}

		handle.spawn(async move {
			let Ok(_permit) = IMAGE_PRELOAD_SEMAPHORE.clone().acquire_owned().await else {
				if let Ok(mut cache) = IMAGE_PRELOAD_CACHE.lock() {
					cache.clear_inflight(&url);
				}
				return;
			};

			if let Err(err) = preload_image_url(url.clone()).await {
				trace!("image preload skipped for {url}: {err}");
				if let Ok(mut cache) = IMAGE_PRELOAD_CACHE.lock() {
					cache.clear_inflight(&url);
				}
			}
		});
	}
}

async fn preload_image_url(url: String) -> Result<(), String> {
	let parsed_uri = url.parse::<Uri>().map_err(|_| "Couldn't parse preload URL".to_string())?;
	let mut builder = Request::get(parsed_uri);
	{
		let client = OAUTH_CLIENT.load_full();
		builder = builder.header("User-Agent", client.user_agent());
	}
	let request = builder.body(Body::empty()).map_err(|_| "Couldn't build preload request".to_string())?;
	let response = tokio::time::timeout(IMAGE_PRELOAD_UPSTREAM_TIMEOUT, CLIENT.request(request))
		.await
		.map_err(|_| "preload request timed out".to_string())?
		.map_err(|e| e.to_string())?;
	let status = response.status();
	if !status.is_success() {
		if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) && cacheable_image_url(&url) {
			crate::post_cache::mark_deleted_reddit_media_url(&url);
		}
		return Err(format!("upstream returned {status}"));
	}

	let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
	if !content_type.as_ref().and_then(|value| value.to_str().ok()).is_some_and(|value| value.starts_with("image/")) {
		return Err("upstream response was not an image".to_string());
	}
	if response
		.headers()
		.get(header::CONTENT_LENGTH)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<usize>().ok())
		.is_some_and(|length| length > IMAGE_PRELOAD_MAX_ENTRY_BYTES)
	{
		return Err("upstream image is too large to preload".to_string());
	}
	let cache_control = response.headers().get(header::CACHE_CONTROL).cloned();
	let body = tokio::time::timeout(IMAGE_PRELOAD_UPSTREAM_TIMEOUT, body::to_bytes(response.into_body()))
		.await
		.map_err(|_| "preload body timed out".to_string())?
		.map_err(|e| e.to_string())?;
	if crate::utils::image_body_matches_deleted_reddit_placeholder(&body) {
		crate::post_cache::mark_deleted_reddit_media_url(&url);
		return Err("upstream image was Reddit's deleted placeholder".to_string());
	}
	let entry = CachedImageResponse {
		status,
		content_type,
		cache_control,
		body,
		inserted_at: Instant::now(),
	};

	if let Ok(mut cache) = IMAGE_PRELOAD_CACHE.lock() {
		cache.insert(url, entry);
	}
	Ok(())
}

fn image_cache_get(url: &str) -> Option<CachedImageResponse> {
	IMAGE_PRELOAD_CACHE.lock().ok().and_then(|mut cache| cache.get(url))
}

fn image_cache_inflight(url: &str) -> bool {
	IMAGE_PRELOAD_CACHE.lock().map(|mut cache| cache.is_inflight(url)).unwrap_or(false)
}

async fn image_cache_get_or_wait(url: &str) -> Option<CachedImageResponse> {
	if let Some(entry) = image_cache_get(url) {
		return Some(entry);
	}

	if !image_cache_inflight(url) {
		return None;
	}

	for _ in 0..30 {
		tokio::time::sleep(Duration::from_millis(25)).await;
		if let Some(entry) = image_cache_get(url) {
			return Some(entry);
		}
		if !image_cache_inflight(url) {
			return None;
		}
	}

	None
}

fn image_content_type_is_image(content_type: Option<&HeaderValue>) -> bool {
	content_type.and_then(|value| value.to_str().ok()).is_some_and(|value| value.starts_with("image/"))
}

fn cached_image_is_deleted_placeholder(entry: &CachedImageResponse) -> bool {
	image_content_type_is_image(entry.content_type.as_ref()) && crate::utils::image_body_matches_deleted_reddit_placeholder(&entry.body)
}

fn deleted_media_response() -> Result<Response<Body>, String> {
	Response::builder()
		.status(StatusCode::GONE)
		.header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
		.header(header::CACHE_CONTROL, "no-store")
		.header("x-redlib-deleted-media", "1")
		.body(Body::from("Deleted Reddit image"))
		.map_err(|error| error.to_string())
}

fn response_should_probe_deleted_placeholder(url: &str, req: &Request<Body>, res: &Response<Body>) -> bool {
	if !cacheable_image_url(url) || req.headers().get("Range").is_some() {
		return false;
	}
	if !image_content_type_is_image(res.headers().get(header::CONTENT_TYPE)) {
		return false;
	}

	res
		.headers()
		.get(header::CONTENT_LENGTH)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<usize>().ok())
		.is_some_and(|length| length <= crate::utils::deleted_reddit_placeholder_max_probe_bytes())
}

fn response_marks_missing_reddit_image(url: &str, req: &Request<Body>, res: &Response<Body>) -> bool {
	if !cacheable_image_url(url) || req.headers().get("Range").is_some() {
		return false;
	}

	matches!(res.status(), StatusCode::NOT_FOUND | StatusCode::GONE)
}

fn cached_image_response(entry: CachedImageResponse) -> Result<Response<Body>, String> {
	let mut builder = Response::builder().status(entry.status).header("x-redlib-image-preload-cache", "hit");
	if let Some(content_type) = entry.content_type {
		builder = builder.header(header::CONTENT_TYPE, content_type);
	}
	if let Some(cache_control) = entry.cache_control {
		builder = builder.header(header::CACHE_CONTROL, cache_control);
	}
	builder.body(Body::from(entry.body)).map_err(|error| error.to_string())
}

pub async fn proxy(req: Request<Body>, format: &str) -> Result<Response<Body>, String> {
	let mut url = match req.uri().query() {
		Some(query) => format!("{format}?{query}"),
		None => format.to_string(),
	};

	// For each parameter in request
	for (name, value) in &req.params() {
		// Fill the parameter value in the url
		url = url.replace(&format!("{{{name}}}"), value);
	}

	if cacheable_image_url(&url) && req.headers().get("Range").is_none() {
		if let Some(entry) = image_cache_get_or_wait(&url).await {
			if cached_image_is_deleted_placeholder(&entry) {
				crate::post_cache::mark_deleted_reddit_media_url(&url);
				return deleted_media_response();
			}
			return cached_image_response(entry);
		}
	}

	stream(&url, &req).await
}

async fn stream(url: &str, req: &Request<Body>) -> Result<Response<Body>, String> {
	// First parameter is target URL (mandatory).
	let parsed_uri = url.parse::<Uri>().map_err(|_| "Couldn't parse URL".to_string())?;

	// Build the hyper client from the HTTPS connector.
	let client: &LazyLock<Client<_, Body>> = &CLIENT;

	let mut builder = Request::get(parsed_uri);

	// Copy useful headers from original request
	for &key in &["Range", "If-Modified-Since", "Cache-Control"] {
		if let Some(value) = req.headers().get(key) {
			builder = builder.header(key, value);
		}
	}

	// Add User-Agent header of the currently spoofed device
	{
		let client = OAUTH_CLIENT.load_full();
		builder = builder.header("User-Agent", client.user_agent());
	}

	let stream_request = builder.body(Body::empty()).map_err(|_| "Couldn't build empty body in stream".to_string())?;

	let mut res = client.request(stream_request).await.map_err(|e| e.to_string())?;
	let content_type = res.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).map(str::to_owned);
	let content_length = res
		.headers()
		.get(header::CONTENT_LENGTH)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<u64>().ok());
	let range = req.headers().get("Range").and_then(|value| value.to_str().ok()).map(str::to_owned);
	let mut rm = |key: &str| res.headers_mut().remove(key);

	rm("access-control-expose-headers");
	rm("server");
	rm("vary");
	rm("etag");
	rm("x-cdn");
	rm("x-cdn-client-region");
	rm("x-cdn-name");
	rm("x-cdn-server-region");
	rm("x-reddit-cdn");
	rm("x-reddit-video-features");
	rm("Nel");
	rm("Report-To");

	log_download_diagnostics("proxy_stream", url, res.status(), content_type.as_deref(), content_length, None, range.as_deref());

	if response_marks_missing_reddit_image(url, req, &res) {
		crate::post_cache::mark_deleted_reddit_media_url(url);
		return deleted_media_response();
	}

	if response_should_probe_deleted_placeholder(url, req, &res) {
		let (parts, body) = res.into_parts();
		let body = tokio::time::timeout(IMAGE_PRELOAD_UPSTREAM_TIMEOUT, body::to_bytes(body))
			.await
			.map_err(|_| "proxy image probe timed out".to_string())?
			.map_err(|e| e.to_string())?;
		if crate::utils::image_body_matches_deleted_reddit_placeholder(&body) {
			crate::post_cache::mark_deleted_reddit_media_url(url);
			return deleted_media_response();
		}
		return Ok(Response::from_parts(parts, Body::from(body)));
	}

	Ok(res)
}

/// Makes a GET request to Reddit at `path`. By default, this will honor HTTP
/// 3xx codes Reddit returns and will automatically redirect.
fn reddit_get(path: String, quarantine: bool) -> Boxed<Result<Response<Body>, String>> {
	request(&Method::GET, path, true, quarantine, REDDIT_URL_BASE, REDDIT_URL_BASE_HOST)
}

/// Makes a HEAD request to Reddit at `path, using the short URL base. This will not follow redirects.
fn reddit_short_head(path: String, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<Response<Body>, String>> {
	request(&Method::HEAD, path, false, quarantine, base_path, host)
}

// /// Makes a HEAD request to Reddit at `path`. This will not follow redirects.
// fn reddit_head(path: String, quarantine: bool) -> Boxed<Result<Response<Body>, String>> {
// 	request(&Method::HEAD, path, false, quarantine, false)
// }
// Unused - reddit_head is only ever called in the context of a short URL

/// Makes a request to Reddit. If `redirect` is `true`, `request_with_redirect`
/// will recurse on the URL that Reddit provides in the Location HTTP header
/// in its response.
fn request(method: &'static Method, path: String, redirect: bool, quarantine: bool, base_path: &'static str, host: &'static str) -> Boxed<Result<Response<Body>, String>> {
	// Build Reddit URL from path.
	let url = format!("{base_path}{path}");

	// Build request to Reddit. When making a GET, request gzip compression.
	// (Reddit doesn't do brotli yet.)
	let mut headers: Vec<(String, String)> = vec![
		("Host".into(), host.into()),
		("Accept-Encoding".into(), if method == Method::GET { "gzip".into() } else { "identity".into() }),
		(
			"Cookie".into(),
			if quarantine {
				"_options=%7B%22pref_quarantine_optin%22%3A%20true%2C%20%22pref_gated_sr_optin%22%3A%20true%7D".into()
			} else {
				"".into()
			},
		),
	];

	{
		let client = OAUTH_CLIENT.load_full();
		for (key, value) in client.headers_map.clone() {
			headers.push((key, value));
		}
	}

	// shuffle headers: https://github.com/redlib-org/redlib/issues/324
	fastrand::shuffle(&mut headers);

	async move {
		pace_reddit_request_if_needed().await;

		match send_reddit_transport_request(method, &url, &headers, Vec::new()).await {
			Ok((mut response, transport)) => {
				trace!("Reddit {} {} served over {:?}", method, url, transport);
				if let Some(snapshot) = capture_ratelimit_snapshot(&response) {
					store_ratelimit_snapshot(snapshot);
				}

				if response.status().is_redirection() {
					if !redirect {
						return Ok(response);
					}
					let location_header = response.headers().get(header::LOCATION);
					if location_header == Some(&HeaderValue::from_static(ALTERNATIVE_REDDIT_URL_BASE)) {
						return Err("Reddit response was invalid".to_string());
					}
					return request(
						method,
						location_header
							.map(|val| {
								let new_path = percent_encode(val.as_bytes(), CONTROLS)
									.to_string()
									.trim_start_matches(REDDIT_URL_BASE)
									.trim_start_matches(ALTERNATIVE_REDDIT_URL_BASE)
									.to_string();
								format!("{new_path}{}raw_json=1", if new_path.contains('?') { "&" } else { "?" })
							})
							.unwrap_or_default()
							.to_string(),
						true,
						quarantine,
						base_path,
						host,
					)
					.await;
				}

				match response.headers().get(header::CONTENT_ENCODING) {
					None => Ok(response),
					Some(hdr) => {
						match hdr.to_str() {
							Ok(val) => match val {
								"gzip" => {}
								"identity" => return Ok(response),
								_ => return Err("Reddit response was encoded with an unsupported compressor".to_string()),
							},
							Err(_) => return Err("Reddit response was invalid".to_string()),
						}

						let mut decompressed: Vec<u8>;
						{
							let mut aggregated_body = match body::aggregate(response.body_mut()).await {
								Ok(b) => b.reader(),
								Err(e) => return Err(e.to_string()),
							};

							let mut decoder = match gzip::Decoder::new(&mut aggregated_body) {
								Ok(decoder) => decoder,
								Err(e) => return Err(e.to_string()),
							};

							decompressed = Vec::<u8>::new();
							if let Err(e) = io::copy(&mut decoder, &mut decompressed) {
								return Err(e.to_string());
							}
						}

						response.headers_mut().remove(header::CONTENT_ENCODING);
						response.headers_mut().insert(header::CONTENT_LENGTH, decompressed.len().into());
						*(response.body_mut()) = Body::from(decompressed);

						Ok(response)
					}
				}
			}
			Err(e) => {
				dbg_msg!("{method} {REDDIT_URL_BASE}{path}: {}", e);
				Err(e)
			}
		}
	}
	.boxed()
}

fn debug_logging_enabled() -> bool {
	std::env::var("REDLIB_DEBUG_LOGGING").map(|value| value.eq_ignore_ascii_case("true")).unwrap_or(false)
}

fn download_diagnostics_enabled() -> bool {
	std::env::var("REDLIB_DOWNLOAD_DIAGNOSTICS")
		.map(|value| value.eq_ignore_ascii_case("on") || value.eq_ignore_ascii_case("true"))
		.unwrap_or(false)
}

fn log_download_diagnostics(
	category: &str,
	url_or_path: &str,
	status: hyper::StatusCode,
	content_type: Option<&str>,
	content_length: Option<u64>,
	actual_bytes: Option<usize>,
	range: Option<&str>,
) {
	if !download_diagnostics_enabled() {
		return;
	}

	info!(
		"download_diagnostics: category={} target={} status={} content_type={} content_length={:?} actual_bytes={:?} range={}",
		category,
		url_or_path,
		status,
		content_type.unwrap_or(""),
		content_length,
		actual_bytes,
		range.unwrap_or("")
	);
}

fn response_preview(text: &str, max_chars: usize) -> String {
	let mut preview: String = text.chars().take(max_chars).collect();
	if text.chars().count() > max_chars {
		preview.push_str("...");
	}
	preview.replace('\n', "\\n").replace('\r', "\\r")
}

fn is_uri_too_long_response(status: hyper::StatusCode, text: &str) -> bool {
	status == hyper::StatusCode::URI_TOO_LONG || text.trim().eq_ignore_ascii_case("Error: URI Too Long")
}

fn is_network_security_block(status: hyper::StatusCode, content_type: Option<&str>, text: &str) -> bool {
	status == hyper::StatusCode::FORBIDDEN && content_type.is_some_and(|value| value.starts_with("text/html")) && text.contains("You've been blocked by network security")
}

fn now_unix() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn check_breaker_gate(blocked_until: u64, rate_remaining: u16, is_rolling_over: bool, now: u64) -> Option<String> {
	if blocked_until > now {
		return Some(format!("Reddit unavailable; cooling down until unix {blocked_until}"));
	}
	if rate_remaining < BREAKER_FLOOR && !is_rolling_over {
		return Some("Rate budget conservation; refusing request".to_string());
	}
	None
}

fn should_record_403_cooldown(status: hyper::StatusCode, has_retry_after: bool) -> bool {
	status == hyper::StatusCode::FORBIDDEN && has_retry_after
}

fn record_403_cooldown(now: u64) {
	let until = now + BLOCK_COOLDOWN_SECS;
	OAUTH_BLOCKED_UNTIL.store(until, Ordering::SeqCst);
	warn!("Reddit returned 403; cooling down json() calls until unix {until}");
}

fn persist_debug_response(path: &str, status: hyper::StatusCode, headers: &[(String, String)], text: &str) {
	use chrono::Utc;
	use std::fs::{create_dir_all, OpenOptions};
	use std::io::Write;

	if create_dir_all("logs/reddit_api").is_err() {
		return;
	}

	let timestamp = Utc::now().format("%Y%m%d_%H%M%S%.3f");
	let filename = format!("logs/reddit_api/response_{}.log", timestamp);
	let Ok(mut file) = OpenOptions::new().write(true).create(true).append(false).open(&filename) else {
		return;
	};

	let _ = writeln!(file, "// Path: {path}");
	let _ = writeln!(file, "// Status: {status}");
	for (key, value) in headers {
		let _ = writeln!(file, "// Header: {key}: {value}");
	}
	let _ = writeln!(file);
	let _ = writeln!(file, "{text}");
}

/// Make a request to a Reddit API and parse the JSON response
#[cached(size = 100, time = 30, result = true)]
pub async fn json(path: String, quarantine: bool) -> Result<Value, String> {
	// Closure to quickly build errors
	let err = |msg: &str, e: String, path: String| -> Result<Value, String> {
		// eprintln!("{} - {}: {}", url, msg, e);
		Err(format!("{msg}: {e} | {path}"))
	};

	// First, handle rolling over the OAUTH_CLIENT if need be.
	let now = now_unix();
	let current_rate_limit = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	let is_rolling_over = OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst);
	if let Some(msg) = check_breaker_gate(OAUTH_BLOCKED_UNTIL.load(Ordering::SeqCst), current_rate_limit, is_rolling_over, now) {
		return Err(format!("{msg} | {path}"));
	}
	if current_rate_limit < 10 && !is_rolling_over {
		warn!("Rate limit {current_rate_limit} is low. Spawning force_refresh_token()");
		tokio::spawn(force_refresh_token());
	}
	OAUTH_RATELIMIT_REMAINING.fetch_sub(1, Ordering::SeqCst);

	if !oauth_is_available() {
		return Err(format!("oauth_unavailable | {path}"));
	}

	// Fetch the url...
	match reddit_get(path.clone(), quarantine).await {
		Ok(response) => {
			let status = response.status();
			if let Some(snapshot) = capture_ratelimit_snapshot(&response) {
				store_ratelimit_snapshot(snapshot);
			}
			let response_had_retry_after = response.headers().get(header::RETRY_AFTER).is_some();
			let content_type = response.headers().get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()).map(str::to_owned);
			let content_length = response
				.headers()
				.get(header::CONTENT_LENGTH)
				.and_then(|value| value.to_str().ok())
				.and_then(|value| value.parse::<u64>().ok());
			let debug_headers = if debug_logging_enabled() {
				Some(
					response
						.headers()
						.iter()
						.map(|(key, value)| (key.as_str().to_owned(), value.to_str().unwrap_or("<non-utf8>").to_owned()))
						.collect::<Vec<_>>(),
				)
			} else {
				None
			};

			let _reset: Option<String> = if let (Some(remaining), Some(reset), Some(used)) = (
				response.headers().get("x-ratelimit-remaining").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
				response.headers().get("x-ratelimit-reset").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
				response.headers().get("x-ratelimit-used").and_then(|val| val.to_str().ok().map(|s| s.to_string())),
			) {
				trace!(
					"Ratelimit remaining: Header says {remaining}, we have {current_rate_limit}. Resets in {reset}. Rollover: {}. Ratelimit used: {used}",
					if is_rolling_over { "yes" } else { "no" },
				);

				Some(reset)
			} else {
				None
			};

			// Get the response body
			match hyper::body::to_bytes(response.into_body()).await {
				Ok(bytes) => {
					let text = String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())?;
					let actual_bytes = bytes.len();

					// Save response to debug log if debug logging is enabled
					if let Some(headers) = debug_headers.as_ref() {
						persist_debug_response(&path, status, headers, &text);
					}

					log_download_diagnostics("reddit_json", &path, status, content_type.as_deref(), content_length, Some(actual_bytes), None);

					if should_record_403_cooldown(status, response_had_retry_after) {
						record_403_cooldown(now);
						return Err(format!(
							"Too many requests. Reddit returned 403 with Retry-After; cooling down for {BLOCK_COOLDOWN_SECS}s | {path}"
						));
					}

					if status == hyper::StatusCode::TOO_MANY_REQUESTS {
						return Err("Too many requests.".to_string());
					}

					// Parse the response as JSON
					match serde_json::from_str(&text) {
						Ok(json) => {
							let json: Value = json;

							// If user is suspended
							if let Some(data) = json.get("data") {
								if let Some(is_suspended) = data.get("is_suspended").and_then(Value::as_bool) {
									if is_suspended {
										return Err("suspended".into());
									}
								}
							}

							// If Reddit returned an error
							if json["error"].is_i64() {
								// OAuth token has expired; http status 401
								if json["message"] == "Unauthorized" {
									error!("OAuth token expired, triggering background refresh");
									tokio::spawn(force_refresh_token());
									return Err(format!("oauth_unavailable | {path}"));
								}

								// Handle quarantined
								if json["reason"] == "quarantined" {
									return Err("quarantined".into());
								}
								// Handle gated
								if json["reason"] == "gated" {
									return Err("gated".into());
								}
								// Handle private subs
								if json["reason"] == "private" {
									return Err("private".into());
								}
								// Handle banned subs
								if json["reason"] == "banned" {
									return Err("banned".into());
								}

								Err(format!("Reddit error {} \"{}\": {} | {path}", json["error"], json["reason"], json["message"]))
							} else {
								Ok(json)
							}
						}
						Err(e) => {
							let content_type = content_type.as_deref().unwrap_or("unknown");
							let preview = response_preview(&text, 240);
							error!("Got an invalid response from reddit {e}. Status code: {status}. Content-Type: {content_type}. Body preview: {preview}");
							if status.is_server_error() {
								Err("Reddit is having issues, check if there's an outage".to_string())
							} else if is_uri_too_long_response(status, &text) {
								err(
									"Reddit rejected this request because the multireddit URL is too long",
									format!("Status code: {status}, Content-Type: {content_type}"),
									path,
								)
							} else if text.trim().is_empty() {
								err(
									"Reddit returned an empty response body",
									format!("Status code: {status}, Content-Type: {content_type}"),
									path,
								)
							} else if is_network_security_block(status, Some(content_type), &text) {
								err(
									"Reddit blocked this request with a network security page",
									format!("Status code: {status}, Content-Type: {content_type}"),
									path,
								)
							} else if content_type.starts_with("text/html") {
								err(
									"Reddit returned HTML instead of JSON",
									format!("Status code: {status}, Content-Type: {content_type}, Body preview: {preview}"),
									path,
								)
							} else {
								err("Failed to parse page JSON data", e.to_string(), path)
							}
						}
					}
				}
				Err(e) => err("Failed receiving body from Reddit", e.to_string(), path),
			}
		}
		Err(e) => err("Couldn't send request to Reddit", e, path),
	}
}

async fn self_check(sub: &str) -> Result<(), String> {
	let query = format!("/r/{sub}/hot.json?&raw_json=1");

	match Post::fetch(&query, true).await {
		Ok(_) => Ok(()),
		Err(e) => Err(e),
	}
}

pub async fn rate_limit_check() -> Result<(), String> {
	if !oauth_is_available() {
		warn!("[⚠️] Cannot perform rate limit check while OAuth is unavailable. Skipping check.");
		return Ok(());
	}

	// First, test the Oauth client: we can perform a rate limit check if the OAuth backend is MobileSpoof; if GenericWeb, we skip the check.
	if matches!(OAUTH_CLIENT.load().backend, OauthBackendImpl::GenericWeb(_)) {
		warn!("[⚠️] Cannot perform rate limit check, running as GenericWeb. Skipping check.");
		return Ok(());
	}

	// First, check a subreddit.
	OAUTH_RATELIMIT_REMAINING.store(100, Ordering::SeqCst);
	self_check("reddit").await?;
	// This will reduce the rate limit to 99. Assert this check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 1 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}
	// Now, we switch out the OAuth client.
	// This checks for the IP rate limit association.
	force_refresh_token().await;
	// Now, check a new sub to break cache.
	OAUTH_RATELIMIT_REMAINING.store(100, Ordering::SeqCst);
	self_check("rust").await?;
	// Again, assert the rate limit check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 2 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}

	Ok(())
}

#[cfg(test)]
use sealed_test::prelude::*;

#[tokio::test(flavor = "multi_thread")]
async fn test_rate_limit_check() {
	rate_limit_check().await.unwrap();
}

#[test]
fn test_detects_network_security_block_page() {
	assert!(is_network_security_block(
		hyper::StatusCode::FORBIDDEN,
		Some("text/html"),
		"<body><div>You've been blocked by network security.</div></body>",
	));
	assert!(!is_network_security_block(hyper::StatusCode::FORBIDDEN, Some("application/json"), "{\"error\":403}",));
}

#[test]
fn test_response_preview_truncates_and_escapes_newlines() {
	assert_eq!(response_preview("line1\nline2", 8), "line1\\nli...");
}

#[test]
fn test_breaker_gate_allows_normal_budget() {
	assert_eq!(check_breaker_gate(0, BREAKER_FLOOR, false, 1_000), None);
}

#[test]
fn test_breaker_gate_blocks_active_cooldown() {
	let msg = check_breaker_gate(1_900, 99, false, 1_000).expect("cooldown should block");
	assert!(msg.contains("cooling down until unix 1900"));
}

#[test]
fn test_breaker_gate_blocks_low_budget_unless_rolling_over() {
	assert_eq!(
		check_breaker_gate(0, BREAKER_FLOOR - 1, false, 1_000),
		Some("Rate budget conservation; refusing request".to_string())
	);
	assert_eq!(check_breaker_gate(0, BREAKER_FLOOR - 1, true, 1_000), None);
}

#[test]
fn test_403_cooldown_requires_retry_after() {
	assert!(should_record_403_cooldown(hyper::StatusCode::FORBIDDEN, true));
	assert!(!should_record_403_cooldown(hyper::StatusCode::FORBIDDEN, false));
	assert!(!should_record_403_cooldown(hyper::StatusCode::TOO_MANY_REQUESTS, true));
}

#[test]
#[sealed_test]
fn test_record_403_cooldown_sets_blocked_until() {
	OAUTH_BLOCKED_UNTIL.store(0, Ordering::SeqCst);
	record_403_cooldown(1_000);
	assert_eq!(OAUTH_BLOCKED_UNTIL.load(Ordering::SeqCst), 1_000 + BLOCK_COOLDOWN_SECS);
	OAUTH_BLOCKED_UNTIL.store(0, Ordering::SeqCst);
}

#[test]
fn test_local_proxy_image_to_upstream_preserves_query() {
	assert_eq!(
		local_proxy_image_to_upstream("/img/example.jpg?width=640&format=pjpg"),
		Some("https://i.redd.it/example.jpg?width=640&format=pjpg".to_string())
	);
	assert_eq!(
		local_proxy_image_to_upstream("/preview/pre/example.png?width=1080"),
		Some("https://preview.redd.it/example.png?width=1080".to_string())
	);
	assert_eq!(
		local_proxy_image_to_upstream("/preview/external-pre/example.jpg"),
		Some("https://external-preview.redd.it/example.jpg".to_string())
	);
	assert_eq!(local_proxy_image_to_upstream("/vid/demo/DASH_720.mp4"), None);
}

#[test]
fn test_missing_reddit_image_responses_mark_for_invalidation() {
	let req = Request::get("/preview/pre/example.jpg").body(Body::empty()).expect("request should build");
	let missing = Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).expect("response should build");
	let gone = Response::builder().status(StatusCode::GONE).body(Body::empty()).expect("response should build");
	let forbidden = Response::builder().status(StatusCode::FORBIDDEN).body(Body::empty()).expect("response should build");
	let ranged_req = Request::builder().header("Range", "bytes=0-1").body(Body::empty()).expect("request should build");

	assert!(response_marks_missing_reddit_image("https://preview.redd.it/example.jpg?width=640", &req, &missing));
	assert!(response_marks_missing_reddit_image("https://i.redd.it/example.jpg", &req, &gone));
	assert!(!response_marks_missing_reddit_image("https://preview.redd.it/example.jpg?width=640", &req, &forbidden));
	assert!(!response_marks_missing_reddit_image("https://example.com/example.jpg", &req, &missing));
	assert!(!response_marks_missing_reddit_image("https://preview.redd.it/example.jpg?width=640", &ranged_req, &missing));
}

#[test]
fn test_ratelimit_snapshot_wait_counts_down_with_elapsed_time() {
	let snapshot = RedditRateLimitSnapshot {
		remaining: Some(0.0),
		reset_seconds: Some(203.0),
		used: None,
		retry_after_seconds: None,
		status: 429,
		observed_at: Instant::now() - Duration::from_secs(200),
	};

	let wait = snapshot.suggested_wait_duration().expect("expected remaining wait");
	assert!(wait > Duration::from_secs_f32(2.0));
	assert!(wait <= Duration::from_secs(3));
}

#[test]
fn test_ratelimit_snapshot_stops_waiting_after_reset_elapsed() {
	let snapshot = RedditRateLimitSnapshot {
		remaining: Some(0.0),
		reset_seconds: Some(10.0),
		used: None,
		retry_after_seconds: None,
		status: 429,
		observed_at: Instant::now() - Duration::from_secs(11),
	};

	assert_eq!(snapshot.suggested_wait_duration(), None);
}

#[test]
fn test_retry_after_counts_down_with_elapsed_time() {
	let snapshot = RedditRateLimitSnapshot {
		remaining: Some(1.0),
		reset_seconds: Some(60.0),
		used: None,
		retry_after_seconds: Some(10.0),
		status: 429,
		observed_at: Instant::now() - Duration::from_secs(7),
	};

	let wait = snapshot.suggested_wait_duration().expect("expected retry-after wait");
	assert!(wait > Duration::from_secs_f32(2.0));
	assert!(wait <= Duration::from_secs(3));
}

#[test]
fn test_detects_uri_too_long_response() {
	assert!(is_uri_too_long_response(hyper::StatusCode::URI_TOO_LONG, "Error: URI Too Long"));
	assert!(is_uri_too_long_response(hyper::StatusCode::OK, "Error: URI Too Long"));
	assert!(!is_uri_too_long_response(hyper::StatusCode::OK, "{\"kind\":\"Listing\"}"));
}

#[test]
#[sealed_test(env = [("REDLIB_DEFAULT_SUBSCRIPTIONS", "rust")])]
fn test_default_subscriptions() {
	tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
		let subscriptions = get_setting("REDLIB_DEFAULT_SUBSCRIPTIONS");
		assert!(subscriptions.is_some());

		// check rate limit
		rate_limit_check().await.unwrap();
	});
}

#[cfg(test)]
const POPULAR_URL: &str = "/r/popular/hot.json?&raw_json=1&geo_filter=GLOBAL";

#[tokio::test(flavor = "multi_thread")]
async fn test_localization_popular() {
	let val = json(POPULAR_URL.to_string(), false).await.unwrap();
	assert_eq!("GLOBAL", val["data"]["geo_filter"].as_str().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_obfuscated_share_link() {
	let share_link = "/r/rust/s/kPgq8WNHRK".into();
	// Correct link without share parameters
	let canonical_link = "/r/rust/comments/18t5968/why_use_tuple_struct_over_standard_struct/kfbqlbc/".into();
	assert_eq!(canonical_path(share_link, 3).await, Ok(Some(canonical_link)));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_private_sub() {
	let link = json("/r/suicide/about.json?raw_json=1".into(), true).await;
	assert!(link.is_err());
	assert_eq!(link, Err("private".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_banned_sub() {
	let link = json("/r/aaa/about.json?raw_json=1".into(), true).await;
	assert!(link.is_err());
	assert_eq!(link, Err("banned".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_gated_sub() {
	match json("/r/drugs/about.json?raw_json=1".into(), false).await {
		Err(err) => assert_eq!(err, "gated"),
		Ok(value) => {
			assert_eq!(value["kind"].as_str(), Some("t5"));
			assert_eq!(value["data"]["display_name"].as_str(), Some("Drugs"));
		}
	}
}
