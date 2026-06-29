use crate::client::{proxy, CLIENT};
use cached::proc_macro::cached;
use hyper::{Body, Method, Request, Response};
use log::{info, warn};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{LazyLock, Mutex};

static REDGIFS_TOKEN: LazyLock<Mutex<(String, i64)>> = LazyLock::new(|| Mutex::new((String::new(), 0)));
static REDGIFS_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]+$").unwrap());
static REDGIFS_USERNAME_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{1,64}$").unwrap());
static REDGIFS_MEDIA_PATH_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]+(?:-[A-Za-z0-9]+)*\.(?:mp4|jpg|jpeg|png|webp)$").unwrap());
static REDGIFS_META_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<meta\s+(?:property|name)="(?P<name>[^"]+)"\s+content="(?P<content>[^"]+)""#).unwrap());
static REDGIFS_LINK_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<link\s+[^>]*rel="(?P<rel>[^"]+)"[^>]*href="(?P<href>[^"]+)""#).unwrap());
static REDGIFS_USER_HTML_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)redgifs\.com/users/([a-z0-9_-]+)").unwrap());

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedgifsMediaPaths {
	video_path: String,
	mobile_video_path: String,
	poster_path: String,
	hd_poster_path: String,
	duration_seconds: i64,
	created_at_unix: u64,
	has_audio: bool,
	username: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RedgifsUserGif {
	#[serde(default)]
	pub id: String,
	#[serde(default)]
	pub description: Option<String>,
	#[serde(default, rename = "createDate")]
	pub create_date: i64,
	#[serde(default)]
	pub duration: f64,
	#[serde(default, rename = "userName")]
	pub user_name: String,
	#[serde(default)]
	pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct RedgifsUserSearchResponse {
	#[serde(default)]
	gifs: Vec<RedgifsUserGif>,
}

impl RedgifsMediaPaths {
	pub(crate) fn video_path(&self) -> &str {
		&self.video_path
	}

	pub(crate) fn poster_path(&self) -> &str {
		&self.poster_path
	}

	pub(crate) fn hd_poster_path(&self) -> &str {
		if self.hd_poster_path.is_empty() {
			&self.poster_path
		} else {
			&self.hd_poster_path
		}
	}

	pub(crate) fn mobile_video_path(&self) -> &str {
		&self.mobile_video_path
	}

	pub(crate) fn duration_seconds(&self) -> i64 {
		self.duration_seconds
	}

	pub(crate) fn created_at_unix(&self) -> u64 {
		self.created_at_unix
	}

	pub(crate) fn username(&self) -> &str {
		&self.username
	}
}

pub fn is_redgifs_domain(domain: &str) -> bool {
	matches!(domain, "redgifs.com" | "www.redgifs.com") || (domain.starts_with('v') && domain.ends_with(".redgifs.com"))
}

pub fn normalize_username(value: &str) -> Option<String> {
	let username = value.trim().trim_start_matches('@');
	if REDGIFS_USERNAME_REGEX.is_match(username) {
		Some(username.to_ascii_lowercase())
	} else {
		None
	}
}

pub async fn fetch_user_recent_gifs(username: String, count: usize) -> Result<Vec<RedgifsUserGif>, String> {
	let username = normalize_username(&username).ok_or_else(|| "Invalid RedGifs username".to_string())?;
	let count = count.clamp(1, 20);
	fetch_user_recent_gifs_cached(username, count).await
}

#[cached(size = 2048, time = 86400, result = true)]
async fn fetch_user_recent_gifs_cached(username: String, count: usize) -> Result<Vec<RedgifsUserGif>, String> {
	let token = get_token().await?;
	let api_url = format!("https://api.redgifs.com/v2/users/{username}/search?order=latest&count={count}");
	let req = create_request(&api_url, Some(&token))?;
	let res = CLIENT.request(req).await.map_err(|err| err.to_string())?;
	let status = res.status();
	let content_type = res.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_owned);
	let content_length = res
		.headers()
		.get("content-length")
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<u64>().ok());
	let body_bytes = hyper::body::to_bytes(res.into_body()).await.map_err(|err| err.to_string())?;
	log_redgifs_download("redgifs_user_search", &api_url, status, content_type.as_deref(), content_length, body_bytes.len());
	if !status.is_success() {
		let preview = String::from_utf8_lossy(&body_bytes).chars().take(200).collect::<String>();
		return Err(format!("RedGifs user search returned HTTP {status}: {preview}"));
	}

	let response: RedgifsUserSearchResponse = serde_json::from_slice(&body_bytes).map_err(|err| err.to_string())?;
	Ok(response.gifs)
}

pub fn watch_path_from_url(url: &str) -> Option<String> {
	let parsed = url.parse::<hyper::Uri>().ok()?;
	let authority = parsed.authority()?.host();
	if !is_redgifs_domain(authority) {
		return None;
	}

	let mut segments = parsed.path().trim_matches('/').split('/');
	let kind = segments.next()?;
	let value = segments.next()?;
	if segments.next().is_some() || !matches!(kind, "watch" | "ifr") || !REDGIFS_ID_REGEX.is_match(value) {
		return None;
	}

	Some(value.to_ascii_lowercase())
}

pub fn watch_path_from_embed_code(embed_code: &str) -> Option<String> {
	for delimiter in ['"', '\''] {
		let needle = format!("src={delimiter}");
		let Some(start) = embed_code.find(&needle) else {
			continue;
		};

		let remainder = &embed_code[start + needle.len()..];
		let Some((src, _)) = remainder.split_once(delimiter) else {
			continue;
		};

		if let Some(path) = watch_path_from_url(src.trim()) {
			return Some(path);
		}
	}

	None
}

fn is_valid_media_path(path: &str) -> bool {
	!path.contains('/') && REDGIFS_MEDIA_PATH_REGEX.is_match(path)
}

fn media_path_from_url(url: &str) -> Option<String> {
	let filename = url.strip_prefix("https://media.redgifs.com/")?;
	if is_valid_media_path(filename) {
		Some(format!("/redgifs/{filename}"))
	} else {
		None
	}
}

fn derive_mobile_video_path_from_media_path(path: &str) -> Option<String> {
	let filename = path.strip_prefix("/redgifs/")?;
	let stem = filename.strip_suffix(".mp4")?;
	let stem = stem.strip_suffix("-silent").unwrap_or(stem);
	let stem = stem.strip_suffix("-mobile").unwrap_or(stem);
	let candidate = format!("{stem}-mobile.mp4");
	if is_valid_media_path(&candidate) {
		Some(format!("/redgifs/{candidate}"))
	} else {
		None
	}
}

fn derive_hd_poster_path_from_media_path(path: &str) -> Option<String> {
	let filename = path.strip_prefix("/redgifs/")?;
	let stem = filename.strip_suffix(".mp4")?;
	let stem = stem.strip_suffix("-silent").unwrap_or(stem);
	let stem = stem.strip_suffix("-mobile").unwrap_or(stem);
	let candidate = format!("{stem}-poster.jpg");
	if is_valid_media_path(&candidate) {
		Some(format!("/redgifs/{candidate}"))
	} else {
		None
	}
}

pub fn direct_media_url_from_path(path: &str) -> Option<String> {
	let filename = path.strip_prefix("/redgifs/")?;
	if is_valid_media_path(filename) {
		Some(format!("https://media.redgifs.com/{filename}"))
	} else {
		None
	}
}

pub async fn handler(req: Request<Body>) -> Result<Response<Body>, String> {
	let path = req.uri().path().trim_start_matches("/redgifs/").trim_matches('/').to_string();

	if is_valid_media_path(&path) {
		return proxy(req, &format!("https://media.redgifs.com/{path}")).await;
	}

	let Some(video_id) = validate_video_id(&path) else {
		return Ok(Response::builder().status(404).body("RedGifs video not found".into()).unwrap_or_default());
	};

	match fetch_media_paths(video_id.clone()).await {
		Ok(media_paths) => Ok(
			Response::builder()
				.status(302)
				.header("Location", media_paths.video_path)
				.body(Body::empty())
				.unwrap_or_default(),
		),
		Err(err) => {
			warn!("Failed to resolve RedGifs media for video id {video_id}: {err}");
			Ok(Response::builder().status(404).body("RedGifs video not found".into()).unwrap_or_default())
		}
	}
}

fn download_diagnostics_enabled() -> bool {
	std::env::var("REDLIB_DOWNLOAD_DIAGNOSTICS")
		.map(|value| value.eq_ignore_ascii_case("on") || value.eq_ignore_ascii_case("true"))
		.unwrap_or(false)
}

fn log_redgifs_download(category: &str, target: &str, status: hyper::StatusCode, content_type: Option<&str>, content_length: Option<u64>, actual_bytes: usize) {
	if !download_diagnostics_enabled() {
		return;
	}

	info!(
		"download_diagnostics: category={} target={} status={} content_type={} content_length={:?} actual_bytes={}",
		category,
		target,
		status,
		content_type.unwrap_or(""),
		content_length,
		actual_bytes
	);
}

pub async fn fetch_poster_path(video_id: &str) -> Result<String, String> {
	fetch_media_paths(video_id.to_string()).await.map(|paths| paths.poster_path)
}

pub async fn fetch_duration_seconds(video_id: &str) -> Result<i64, String> {
	fetch_media_paths(video_id.to_string()).await.map(|paths| paths.duration_seconds)
}

pub async fn fetch_video_path(video_id: &str) -> Result<String, String> {
	fetch_media_paths(video_id.to_string()).await.map(|paths| paths.video_path)
}

#[cached(size = 512, time = 10800, result = true)]
pub(crate) async fn fetch_media_paths(video_id: String) -> Result<RedgifsMediaPaths, String> {
	match fetch_media_paths_from_api(&video_id).await {
		Ok(paths) => Ok(paths),
		Err(api_err) => {
			warn!("Failed RedGifs API lookup for {video_id}: {api_err}; falling back to watch page");
			fetch_media_paths_from_watch_page(&video_id)
				.await
				.map_err(|watch_err| format!("{api_err}; fallback failed: {watch_err}"))
		}
	}
}

async fn fetch_media_paths_from_api(video_id: &str) -> Result<RedgifsMediaPaths, String> {
	let token = get_token().await?;
	let api_url = format!("https://api.redgifs.com/v2/gifs/{video_id}?views=yes");
	let req = create_request(&api_url, Some(&token))?;
	let res = CLIENT.request(req).await.map_err(|err| err.to_string())?;
	let status = res.status();
	let content_type = res.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_owned);
	let content_length = res
		.headers()
		.get("content-length")
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<u64>().ok());
	let body_bytes = hyper::body::to_bytes(res.into_body()).await.map_err(|err| err.to_string())?;
	log_redgifs_download("redgifs_api", &api_url, status, content_type.as_deref(), content_length, body_bytes.len());
	if !status.is_success() {
		let preview = String::from_utf8_lossy(&body_bytes).chars().take(200).collect::<String>();
		return Err(format!("RedGifs API returned HTTP {status}: {preview}"));
	}
	let json: Value = serde_json::from_slice(&body_bytes).map_err(|err| err.to_string())?;
	parse_api_media_paths(video_id, &json)
}

fn parse_api_media_paths(video_id: &str, json: &Value) -> Result<RedgifsMediaPaths, String> {
	let gif = &json["gif"];
	let has_audio = gif["hasAudio"].as_bool().unwrap_or(false);
	let urls = &gif["urls"];
	let selected_video_url = if has_audio {
		urls["hd"].as_str().or_else(|| urls["sd"].as_str()).or_else(|| urls["silent"].as_str())
	} else {
		urls["silent"].as_str().or_else(|| urls["hd"].as_str()).or_else(|| urls["sd"].as_str())
	};
	let video_path = selected_video_url
		.and_then(media_path_from_url)
		.ok_or_else(|| "No video URL in RedGifs response".to_string())?;
	let mobile_video_path = urls["sd"]
		.as_str()
		.and_then(media_path_from_url)
		.or_else(|| derive_mobile_video_path_from_media_path(&video_path))
		.unwrap_or_default();
	let poster_path = json["gif"]["urls"]["poster"]
		.as_str()
		.or_else(|| json["gif"]["urls"]["thumbnail"].as_str())
		.and_then(media_path_from_url)
		.unwrap_or_default();
	let hd_poster_path = derive_hd_poster_path_from_media_path(&video_path).unwrap_or_else(|| poster_path.clone());
	let duration_seconds = gif["duration"].as_f64().map(|value| value.round() as i64).unwrap_or_default();
	let created_at_unix = gif["createDate"].as_i64().filter(|value| *value > 0).map(|value| value as u64).unwrap_or_default();
	let username = gif["userName"].as_str().or_else(|| gif["username"].as_str()).unwrap_or_default().trim().to_string();
	info!("Selected RedGifs API media for {}: has_audio={} path={}", video_id, has_audio, video_path);

	Ok(RedgifsMediaPaths {
		video_path,
		mobile_video_path,
		poster_path,
		hd_poster_path,
		duration_seconds,
		created_at_unix,
		has_audio,
		username,
	})
}

async fn fetch_media_paths_from_watch_page(video_id: &str) -> Result<RedgifsMediaPaths, String> {
	let watch_url = format!("https://www.redgifs.com/watch/{video_id}");
	let req = create_request(&watch_url, None)?;
	let res = CLIENT.request(req).await.map_err(|err| err.to_string())?;
	let status = res.status();
	let content_type = res.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_owned);
	let content_length = res
		.headers()
		.get("content-length")
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<u64>().ok());
	let body_bytes = hyper::body::to_bytes(res.into_body()).await.map_err(|err| err.to_string())?;
	log_redgifs_download("redgifs_watch_page", &watch_url, status, content_type.as_deref(), content_length, body_bytes.len());
	if !status.is_success() {
		let preview = String::from_utf8_lossy(&body_bytes).chars().take(200).collect::<String>();
		return Err(format!("RedGifs watch page returned HTTP {status}: {preview}"));
	}

	let html = String::from_utf8(body_bytes.to_vec()).map_err(|err| err.to_string())?;
	parse_watch_page_media_paths(video_id, &html).await
}

async fn media_url_exists(url: &str) -> bool {
	if tokio::runtime::Handle::try_current().is_err() {
		return false;
	}

	let Ok(req) = create_request_with_method(Method::HEAD, url, None) else {
		return false;
	};
	let Ok(res) = CLIENT.request(req).await else {
		return false;
	};
	res.status().is_success()
}

async fn parse_watch_page_media_paths(video_id: &str, html: &str) -> Result<RedgifsMediaPaths, String> {
	let mut og_video = None;
	let mut twitter_image = None;
	let mut og_duration = None;

	for captures in REDGIFS_META_TAG_REGEX.captures_iter(html) {
		let Some(name) = captures.name("name").map(|value| value.as_str()) else {
			continue;
		};
		let Some(content) = captures.name("content").map(|value| value.as_str()) else {
			continue;
		};

		match name {
			"og:video" => og_video = Some(content.to_string()),
			"twitter:image" | "og:image" => {
				if twitter_image.is_none() {
					twitter_image = Some(content.to_string());
				}
			}
			"og:video:duration" => og_duration = content.parse::<f64>().ok(),
			_ => {}
		}
	}

	if twitter_image.is_none() {
		for captures in REDGIFS_LINK_TAG_REGEX.captures_iter(html) {
			let Some(rel) = captures.name("rel").map(|value| value.as_str()) else {
				continue;
			};
			if rel != "preload" {
				continue;
			}
			let Some(href) = captures.name("href").map(|value| value.as_str()) else {
				continue;
			};
			if href.starts_with("https://media.redgifs.com/") && href.ends_with(".jpg") {
				twitter_image = Some(href.to_string());
				break;
			}
		}
	}

	let og_video = og_video.ok_or_else(|| "No video URL in RedGifs watch page".to_string())?;
	let mut selected_video_url = og_video.clone();
	let mut has_audio = false;
	if let Some(non_silent) = og_video.strip_suffix("-silent.mp4").map(|prefix| format!("{prefix}.mp4")) {
		if media_url_exists(&non_silent).await {
			selected_video_url = non_silent;
			has_audio = true;
		}
	}
	let video_path = media_path_from_url(&selected_video_url).ok_or_else(|| "No video URL in RedGifs watch page".to_string())?;
	let mobile_video_path = derive_mobile_video_path_from_media_path(&video_path).unwrap_or_default();
	let poster_path = twitter_image.as_deref().and_then(media_path_from_url).unwrap_or_default();
	let hd_poster_path = derive_hd_poster_path_from_media_path(&video_path).unwrap_or_else(|| poster_path.clone());
	let duration_seconds = og_duration.map(|value| value.round() as i64).unwrap_or_default();
	let username = REDGIFS_USER_HTML_REGEX
		.captures(html)
		.and_then(|captures| captures.get(1))
		.map(|value| value.as_str().to_string())
		.unwrap_or_default();
	info!("Selected RedGifs watch-page media for {}: has_audio={} path={}", video_id, has_audio, video_path);

	Ok(RedgifsMediaPaths {
		video_path,
		mobile_video_path,
		poster_path,
		hd_poster_path,
		duration_seconds,
		created_at_unix: 0,
		has_audio,
		username,
	})
}

async fn get_token() -> Result<String, String> {
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_err(|_| "Time error".to_string())?
		.as_secs() as i64;

	{
		let cache = REDGIFS_TOKEN.lock().map_err(|_| "Lock error".to_string())?;
		if !cache.0.is_empty() && now < cache.1 {
			return Ok(cache.0.clone());
		}
	}

	let auth_url = "https://api.redgifs.com/v2/auth/temporary";
	let req = create_request(auth_url, None)?;
	let res = CLIENT.request(req).await.map_err(|err| err.to_string())?;
	let status = res.status();
	let content_type = res.headers().get("content-type").and_then(|value| value.to_str().ok()).map(str::to_owned);
	let content_length = res
		.headers()
		.get("content-length")
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<u64>().ok());
	let body_bytes = hyper::body::to_bytes(res.into_body()).await.map_err(|err| err.to_string())?;
	log_redgifs_download("redgifs_auth", auth_url, status, content_type.as_deref(), content_length, body_bytes.len());
	let json: Value = serde_json::from_slice(&body_bytes).map_err(|err| err.to_string())?;
	let token = json["token"].as_str().ok_or_else(|| "No token in RedGifs response".to_string())?.to_string();

	let mut cache = REDGIFS_TOKEN.lock().map_err(|_| "Lock error".to_string())?;
	cache.0 = token.clone();
	cache.1 = now + 258_000;
	Ok(token)
}

fn create_request(url: &str, token: Option<&str>) -> Result<Request<Body>, String> {
	create_request_with_method(Method::GET, url, token)
}

fn create_request_with_method(method: Method, url: &str, token: Option<&str>) -> Result<Request<Body>, String> {
	let mut builder = Request::builder()
		.method(method)
		.uri(url)
		.header("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
		.header("referer", "https://www.redgifs.com/")
		.header("origin", "https://www.redgifs.com")
		.header("content-type", "application/json");

	if let Some(token) = token {
		builder = builder.header("authorization", format!("Bearer {token}"));
	}

	builder.body(Body::empty()).map_err(|err| err.to_string())
}

fn validate_video_id(value: &str) -> Option<String> {
	let trimmed = value.trim();
	if REDGIFS_ID_REGEX.is_match(trimmed) {
		Some(trimmed.to_ascii_lowercase())
	} else {
		None
	}
}

#[cfg(test)]
mod tests {
	use super::{
		derive_hd_poster_path_from_media_path, is_valid_media_path, media_path_from_url, parse_api_media_paths, parse_watch_page_media_paths, watch_path_from_embed_code,
		watch_path_from_url,
	};
	use futures_lite::future::block_on;
	use serde_json::json;

	#[test]
	fn extracts_watch_ids_from_supported_urls() {
		assert_eq!(
			watch_path_from_url("https://www.redgifs.com/watch/zanyjudiciousrook"),
			Some("zanyjudiciousrook".to_string())
		);
		assert_eq!(watch_path_from_url("https://www.redgifs.com/ifr/zanyjudiciousrook"), Some("zanyjudiciousrook".to_string()));
		assert_eq!(
			watch_path_from_url("https://v3.redgifs.com/watch/ZanyJudiciousRook?foo=bar"),
			Some("zanyjudiciousrook".to_string())
		);
		assert_eq!(watch_path_from_url("https://example.com/watch/zanyjudiciousrook"), None);
		assert_eq!(watch_path_from_url("https://www.redgifs.com/users/example"), None);
	}

	#[test]
	fn extracts_watch_ids_from_embed_code() {
		assert_eq!(
			watch_path_from_embed_code("<iframe src=\"https://www.redgifs.com/ifr/zanyjudiciousrook\" frameborder=\"0\"></iframe>"),
			Some("zanyjudiciousrook".to_string())
		);
	}

	#[test]
	fn accepts_media_filenames_used_for_video_and_poster_assets() {
		assert!(is_valid_media_path("FamiliarFrizzyChevrotain.mp4"));
		assert!(is_valid_media_path("FamiliarFrizzyChevrotain-mobile.mp4"));
		assert!(is_valid_media_path("FamiliarFrizzyChevrotain-poster.jpg"));
		assert!(!is_valid_media_path("../FamiliarFrizzyChevrotain.mp4"));
	}

	#[test]
	fn converts_redgifs_media_urls_to_local_proxy_paths() {
		assert_eq!(
			media_path_from_url("https://media.redgifs.com/FamiliarFrizzyChevrotain.mp4"),
			Some("/redgifs/FamiliarFrizzyChevrotain.mp4".to_string())
		);
		assert_eq!(
			media_path_from_url("https://media.redgifs.com/FamiliarFrizzyChevrotain-poster.jpg"),
			Some("/redgifs/FamiliarFrizzyChevrotain-poster.jpg".to_string())
		);
		assert_eq!(media_path_from_url("https://example.com/FamiliarFrizzyChevrotain.mp4"), None);
		assert_eq!(media_path_from_url("https://media.redgifs.com/../bad.mp4"), None);
	}

	#[test]
	fn derives_mobile_preview_path_from_full_or_silent_video_path() {
		assert_eq!(
			super::derive_mobile_video_path_from_media_path("/redgifs/FamiliarFrizzyChevrotain.mp4"),
			Some("/redgifs/FamiliarFrizzyChevrotain-mobile.mp4".to_string())
		);
		assert_eq!(
			super::derive_mobile_video_path_from_media_path("/redgifs/FamiliarFrizzyChevrotain-silent.mp4"),
			Some("/redgifs/FamiliarFrizzyChevrotain-mobile.mp4".to_string())
		);
	}

	#[test]
	fn derives_hd_poster_path_from_full_silent_or_mobile_video_path() {
		assert_eq!(
			derive_hd_poster_path_from_media_path("/redgifs/FamiliarFrizzyChevrotain.mp4"),
			Some("/redgifs/FamiliarFrizzyChevrotain-poster.jpg".to_string())
		);
		assert_eq!(
			derive_hd_poster_path_from_media_path("/redgifs/FamiliarFrizzyChevrotain-silent.mp4"),
			Some("/redgifs/FamiliarFrizzyChevrotain-poster.jpg".to_string())
		);
		assert_eq!(
			derive_hd_poster_path_from_media_path("/redgifs/FamiliarFrizzyChevrotain-mobile.mp4"),
			Some("/redgifs/FamiliarFrizzyChevrotain-poster.jpg".to_string())
		);
	}

	#[test]
	fn parses_redgifs_api_metadata_with_create_date() {
		let json = json!({
			"gif": {
				"hasAudio": true,
				"duration": 58.367,
				"createDate": 1699143549,
				"userName": "honeynut",
				"urls": {
					"hd": "https://media.redgifs.com/ZanyJudiciousRook.mp4",
					"sd": "https://media.redgifs.com/ZanyJudiciousRook-mobile.mp4",
					"silent": "https://media.redgifs.com/ZanyJudiciousRook-silent.mp4",
					"poster": "https://media.redgifs.com/ZanyJudiciousRook-poster.jpg"
				}
			}
		});

		assert_eq!(
			parse_api_media_paths("zanyjudiciousrook", &json).unwrap(),
			super::RedgifsMediaPaths {
				video_path: "/redgifs/ZanyJudiciousRook.mp4".to_string(),
				mobile_video_path: "/redgifs/ZanyJudiciousRook-mobile.mp4".to_string(),
				poster_path: "/redgifs/ZanyJudiciousRook-poster.jpg".to_string(),
				hd_poster_path: "/redgifs/ZanyJudiciousRook-poster.jpg".to_string(),
				duration_seconds: 58,
				created_at_unix: 1699143549,
				has_audio: true,
				username: "honeynut".to_string(),
			}
		);
	}

	#[test]
	fn parses_redgifs_watch_page_metadata_as_fallback() {
		let html = r#"
			<html>
				<head>
					<meta property="og:video" content="https://media.redgifs.com/ImperturbableLumpyShihtzu-silent.mp4">
					<meta name="twitter:image" content="https://media.redgifs.com/ImperturbableLumpyShihtzu-mobile.jpg">
					<meta property="og:video:duration" content="31">
					<a href="https://www.redgifs.com/users/imperturbablecreator">uploader</a>
				</head>
			</html>
		"#;

		assert_eq!(
			block_on(parse_watch_page_media_paths("imperturbablelumpyshihtzu", html)).unwrap(),
			super::RedgifsMediaPaths {
				video_path: "/redgifs/ImperturbableLumpyShihtzu-silent.mp4".to_string(),
				mobile_video_path: "/redgifs/ImperturbableLumpyShihtzu-mobile.mp4".to_string(),
				poster_path: "/redgifs/ImperturbableLumpyShihtzu-mobile.jpg".to_string(),
				hd_poster_path: "/redgifs/ImperturbableLumpyShihtzu-poster.jpg".to_string(),
				duration_seconds: 31,
				created_at_unix: 0,
				has_audio: false,
				username: "imperturbablecreator".to_string(),
			}
		);
	}

	#[test]
	fn falls_back_to_preloaded_image_when_twitter_image_is_missing() {
		let html = r#"
			<html>
				<head>
					<link rel="preload" as="image" href="https://media.redgifs.com/ImperturbableLumpyShihtzu-mobile.jpg">
					<meta property="og:video" content="https://media.redgifs.com/ImperturbableLumpyShihtzu.mp4">
				</head>
			</html>
		"#;

		assert_eq!(
			block_on(parse_watch_page_media_paths("imperturbablelumpyshihtzu", html)).unwrap(),
			super::RedgifsMediaPaths {
				video_path: "/redgifs/ImperturbableLumpyShihtzu.mp4".to_string(),
				mobile_video_path: "/redgifs/ImperturbableLumpyShihtzu-mobile.mp4".to_string(),
				poster_path: "/redgifs/ImperturbableLumpyShihtzu-mobile.jpg".to_string(),
				hd_poster_path: "/redgifs/ImperturbableLumpyShihtzu-poster.jpg".to_string(),
				duration_seconds: 0,
				created_at_unix: 0,
				has_audio: false,
				username: String::new(),
			}
		);
	}
}
