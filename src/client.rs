use arc_swap::ArcSwap;
use cached::proc_macro::cached;
use futures_lite::future::block_on;
use futures_lite::{future::Boxed, FutureExt};
use hyper::client::HttpConnector;
use hyper::header::HeaderValue;
use hyper::{body, body::Buf, header, Body, Client, Method, Request, Response, Uri};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use libflate::gzip;
use log::{error, info, trace, warn};
use percent_encoding::{percent_encode, CONTROLS};
use serde_json::Value;

use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU16};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use std::{io, result::Result};

use crate::dbg_msg;
use crate::oauth::{force_refresh_token, token_daemon, Oauth, OauthBackendImpl};
use crate::server::RequestExt;
use crate::utils::{format_url, Post};

const REDDIT_URL_BASE: &str = "https://oauth.reddit.com";
const REDDIT_URL_BASE_HOST: &str = "oauth.reddit.com";

const REDDIT_SHORT_URL_BASE: &str = "https://redd.it";
const REDDIT_SHORT_URL_BASE_HOST: &str = "redd.it";

const ALTERNATIVE_REDDIT_URL_BASE: &str = "https://www.reddit.com";
const ALTERNATIVE_REDDIT_URL_BASE_HOST: &str = "www.reddit.com";

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

pub static CLIENT: LazyLock<Client<HttpsConnector<HttpConnector>>> = LazyLock::new(|| {
	let mut builder = Client::builder();
	if cfg!(test) {
		// Async tests create and tear down multiple Tokio runtimes. Disabling the
		// idle pool avoids reusing connections whose dispatch task belonged to a
		// runtime that has already been dropped.
		builder.pool_max_idle_per_host(0);
	}
	builder.build::<_, Body>(HTTPS_CONNECTOR.clone())
});

pub static OAUTH_CLIENT: LazyLock<ArcSwap<Oauth>> = LazyLock::new(|| {
	let client = block_on(Oauth::new());
	tokio::spawn(token_daemon());
	ArcSwap::new(client.into())
});

pub static OAUTH_RATELIMIT_REMAINING: AtomicU16 = AtomicU16::new(99);

pub static OAUTH_IS_ROLLING_OVER: AtomicBool = AtomicBool::new(false);
pub static LAST_REDDIT_RATELIMIT: LazyLock<std::sync::Mutex<Option<RedditRateLimitSnapshot>>> = LazyLock::new(|| std::sync::Mutex::new(None));

const URL_PAIRS: [(&str, &str); 2] = [
	(ALTERNATIVE_REDDIT_URL_BASE, ALTERNATIVE_REDDIT_URL_BASE_HOST),
	(REDDIT_SHORT_URL_BASE, REDDIT_SHORT_URL_BASE_HOST),
];

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

pub async fn proxy(req: Request<Body>, format: &str) -> Result<Response<Body>, String> {
	let mut url = format!("{format}?{}", req.uri().query().unwrap_or_default());

	// For each parameter in request
	for (name, value) in &req.params() {
		// Fill the parameter value in the url
		url = url.replace(&format!("{{{name}}}"), value);
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

	client
		.request(stream_request)
		.await
		.map(|mut res| {
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

			res
		})
		.map_err(|e| e.to_string())
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

	// Construct the hyper client from the HTTPS connector.
	let client: &LazyLock<Client<_, Body>> = &CLIENT;

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

	let mut builder = Request::builder().method(method).uri(&url);

	for (key, value) in headers {
		builder = builder.header(key, value);
	}

	let builder = builder.body(Body::empty());

	async move {
		pace_reddit_request_if_needed().await;

		match builder {
			Ok(req) => {
				match client.request(req).await {
					Ok(mut response) => {
						if let Some(snapshot) = capture_ratelimit_snapshot(&response) {
							store_ratelimit_snapshot(snapshot);
						}

						// Reddit may respond with a 3xx. Decide whether or not to
						// redirect based on caller params.
						if response.status().is_redirection() {
							if !redirect {
								return Ok(response);
							};
							let location_header = response.headers().get(header::LOCATION);
							if location_header == Some(&HeaderValue::from_static(ALTERNATIVE_REDDIT_URL_BASE)) {
								return Err("Reddit response was invalid".to_string());
							}
							return request(
								method,
								location_header
									.map(|val| {
										// We need to make adjustments to the URI
										// we get back from Reddit. Namely, we
										// must:
										//
										//     1. Remove the authority (e.g.
										//     https://www.reddit.com) that may be
										//     present, so that we recurse on the
										//     path (and query parameters) as
										//     required.
										//
										//     2. Percent-encode the path.
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
						};

						match response.headers().get(header::CONTENT_ENCODING) {
							// Content not compressed.
							None => Ok(response),

							// Content encoded (hopefully with gzip).
							Some(hdr) => {
								match hdr.to_str() {
									Ok(val) => match val {
										"gzip" => {}
										"identity" => return Ok(response),
										_ => return Err("Reddit response was encoded with an unsupported compressor".to_string()),
									},
									Err(_) => return Err("Reddit response was invalid".to_string()),
								}

								// We get here if the body is gzip-compressed.

								// The body must be something that implements
								// std::io::Read, hence the conversion to
								// bytes::buf::Buf and then transformation into a
								// Reader.
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
									};
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

						Err(e.to_string())
					}
				}
			}
			Err(_) => Err("Post url contains non-ASCII characters".to_string()),
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
	let current_rate_limit = OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst);
	let is_rolling_over = OAUTH_IS_ROLLING_OVER.load(Ordering::SeqCst);
	if current_rate_limit < 10 && !is_rolling_over {
		warn!("Rate limit {current_rate_limit} is low. Spawning force_refresh_token()");
		tokio::spawn(force_refresh_token());
	}
	OAUTH_RATELIMIT_REMAINING.fetch_sub(1, Ordering::SeqCst);

	// Fetch the url...
	match reddit_get(path.clone(), quarantine).await {
		Ok(response) => {
			let status = response.status();
			if let Some(snapshot) = capture_ratelimit_snapshot(&response) {
				store_ratelimit_snapshot(snapshot);
			}
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
									error!("Forcing a token refresh");
									let () = force_refresh_token().await;
									return Err("OAuth token has expired. Please refresh the page!".to_string());
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
	// First, test the Oauth client: we can perform a rate limit check if the OAuth backend is MobileSpoof; if GenericWeb, we skip the check.
	if matches!(OAUTH_CLIENT.load().backend, OauthBackendImpl::GenericWeb(_)) {
		warn!("[⚠️] Cannot perform rate limit check, running as GenericWeb. Skipping check.");
		return Ok(());
	}

	// First, check a subreddit.
	self_check("reddit").await?;
	// This will reduce the rate limit to 99. Assert this check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 1 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}
	// Now, we switch out the OAuth client.
	// This checks for the IP rate limit association.
	force_refresh_token().await;
	// Now, check a new sub to break cache.
	self_check("rust").await?;
	// Again, assert the rate limit check.
	if OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst) != 99 {
		return Err(format!("Rate limit check 2 failed: expected 99, got {}", OAUTH_RATELIMIT_REMAINING.load(Ordering::SeqCst)));
	}

	Ok(())
}

#[cfg(test)]
use {crate::config::get_setting, sealed_test::prelude::*};

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
