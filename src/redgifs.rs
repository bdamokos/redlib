use crate::client::{proxy, CLIENT};
use cached::proc_macro::cached;
use hyper::{Body, Request, Response};
use regex::Regex;
use serde_json::Value;
use std::sync::{LazyLock, Mutex};

static REDGIFS_TOKEN: LazyLock<Mutex<(String, i64)>> = LazyLock::new(|| Mutex::new((String::new(), 0)));
static REDGIFS_ID_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]+$").unwrap());
static REDGIFS_MEDIA_PATH_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]+(?:-[A-Za-z0-9]+)*\.(?:mp4|jpg|jpeg|png|webp)$").unwrap());

#[derive(Clone, Debug, PartialEq, Eq)]
struct RedgifsMediaPaths {
	video_path: String,
	poster_path: String,
}

pub fn is_redgifs_domain(domain: &str) -> bool {
	matches!(domain, "redgifs.com" | "www.redgifs.com") || (domain.starts_with('v') && domain.ends_with(".redgifs.com"))
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
		Err(_) => Ok(Response::builder().status(404).body("RedGifs video not found".into()).unwrap_or_default()),
	}
}

pub async fn fetch_poster_path(video_id: &str) -> Result<String, String> {
	fetch_media_paths(video_id.to_string()).await.map(|paths| paths.poster_path)
}

#[cached(size = 512, time = 3600, result = true)]
async fn fetch_media_paths(video_id: String) -> Result<RedgifsMediaPaths, String> {
	let token = get_token().await?;
	let api_url = format!("https://api.redgifs.com/v2/gifs/{video_id}?views=yes");
	let req = create_request(&api_url, Some(&token))?;
	let res = CLIENT.request(req).await.map_err(|err| err.to_string())?;
	let body_bytes = hyper::body::to_bytes(res.into_body()).await.map_err(|err| err.to_string())?;
	let json: Value = serde_json::from_slice(&body_bytes).map_err(|err| err.to_string())?;

	let video_path = json["gif"]["urls"]["hd"]
		.as_str()
		.or_else(|| json["gif"]["urls"]["sd"].as_str())
		.and_then(media_path_from_url)
		.ok_or_else(|| "No video URL in RedGifs response".to_string())?;
	let poster_path = json["gif"]["urls"]["poster"]
		.as_str()
		.or_else(|| json["gif"]["urls"]["thumbnail"].as_str())
		.and_then(media_path_from_url)
		.unwrap_or_default();

	Ok(RedgifsMediaPaths { video_path, poster_path })
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

	let req = create_request("https://api.redgifs.com/v2/auth/temporary", None)?;
	let res = CLIENT.request(req).await.map_err(|err| err.to_string())?;
	let body_bytes = hyper::body::to_bytes(res.into_body()).await.map_err(|err| err.to_string())?;
	let json: Value = serde_json::from_slice(&body_bytes).map_err(|err| err.to_string())?;
	let token = json["token"].as_str().ok_or_else(|| "No token in RedGifs response".to_string())?.to_string();

	let mut cache = REDGIFS_TOKEN.lock().map_err(|_| "Lock error".to_string())?;
	cache.0 = token.clone();
	cache.1 = now + 86_000;
	Ok(token)
}

fn create_request(url: &str, token: Option<&str>) -> Result<Request<Body>, String> {
	let mut builder = Request::get(url)
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
	use super::{is_valid_media_path, media_path_from_url, watch_path_from_embed_code, watch_path_from_url};

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
}
