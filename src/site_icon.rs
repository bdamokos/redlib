use crate::client::{CLIENT, OAUTH_CLIENT};
use crate::server::RequestExt;
use hyper::body::to_bytes;
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE, LOCATION};
use hyper::{Body, Request, Response, Uri};
use regex::Regex;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use url::Url;

const MAX_CACHE_BYTES: usize = 5 * 1024 * 1024;
const MAX_ICON_BYTES: usize = 256 * 1024;
const CACHE_TTL_DAYS: u64 = 3;
const CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 24 * CACHE_TTL_DAYS);
const MAX_REDIRECTS: usize = 4;
const MAX_HTML_BYTES: usize = 128 * 1024;
const CACHE_CONTROL_VALUE: &str = "public, max-age=259200, s-maxage=259200";
const DEFAULT_ICON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="8" fill="#e6e8eb"/><path d="M16 6a10 10 0 1 0 0 20a10 10 0 0 0 0-20Zm0 2c1.7 0 3.2.5 4.5 1.4c-1 .5-2.2.9-3.5 1.1c-.2-1-.5-1.8-1-2.5Zm-2.2.4c-.4.7-.8 1.6-1 2.5c-1.3-.2-2.5-.6-3.5-1.1A7.9 7.9 0 0 1 13.8 8.4ZM8.1 11.3c1.3.7 2.9 1.2 4.6 1.5A25 25 0 0 0 12.5 16c0 1.1.1 2.2.3 3.2c-1.7.3-3.3.8-4.6 1.5A8 8 0 0 1 8 16c0-1.7.5-3.3 1.1-4.7Zm6.6 1.8c.4 0 .9.1 1.3.1s.9 0 1.3-.1c.2.9.2 1.9.2 2.9s-.1 2-.2 2.9c-.4 0-.9.1-1.3.1s-.9 0-1.3-.1a19.6 19.6 0 0 1-.2-2.9c0-1 .1-2 .2-2.9Zm6.5-1.8A8 8 0 0 1 24 16c0 1.7-.5 3.3-1.1 4.7c-1.3-.7-2.9-1.2-4.6-1.5c.2-1 .3-2.1.3-3.2s-.1-2.2-.3-3.2c1.7-.3 3.3-.8 4.6-1.5Zm-11.9 11c1 .5 2.2.9 3.5 1.1c.2 1 .5 1.8 1 2.5a8 8 0 0 1-4.5-3.6Zm7.6 1.1c1.3-.2 2.5-.6 3.5-1.1a8 8 0 0 1-4.5 3.6c.4-.7.8-1.6 1-2.5Z" fill="#7f8894"/></svg>"##;

static LINK_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<link\b[^>]*>").unwrap());
static REL_ATTR_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\brel\s*=\s*["']([^"']+)["']"#).unwrap());
static HREF_ATTR_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\bhref\s*=\s*["']([^"']+)["']"#).unwrap());

static ICON_CACHE: LazyLock<Mutex<IconCache>> = LazyLock::new(|| Mutex::new(IconCache::default()));

#[derive(Default)]
struct IconCache {
	entries: HashMap<String, CachedIcon>,
	total_bytes: usize,
	tick: u64,
}

#[derive(Clone)]
struct CachedIcon {
	body: Vec<u8>,
	content_type: String,
	bytes: usize,
	last_accessed: u64,
	expires_at: Instant,
}

pub async fn icon(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(host) = req.param("host") else {
		return Ok(response_from_icon(default_icon()));
	};

	let Some(normalized_host) = normalize_host(&host) else {
		return Ok(response_from_icon(default_icon()));
	};

	if let Some(icon) = cached_icon(&normalized_host) {
		return Ok(response_from_icon(icon));
	}

	let fetched = fetch_icon(&normalized_host).await.unwrap_or_else(|_| default_icon());
	store_icon(normalized_host, fetched.clone());
	Ok(response_from_icon(fetched))
}

fn normalize_host(host: &str) -> Option<String> {
	let normalized = host.trim().trim_matches('.').to_lowercase();
	let parsed = Url::parse(&format!("https://{normalized}/")).ok()?;
	let parsed_host = parsed.host_str()?.to_lowercase();
	if parsed_host.is_empty() || !parsed_host.contains('.') {
		return None;
	}
	Some(parsed_host)
}

fn default_icon() -> CachedIcon {
	CachedIcon {
		body: DEFAULT_ICON.to_vec(),
		content_type: "image/svg+xml".to_string(),
		bytes: DEFAULT_ICON.len(),
		last_accessed: 0,
		expires_at: Instant::now() + CACHE_TTL,
	}
}

fn cached_icon(host: &str) -> Option<CachedIcon> {
	let mut cache = ICON_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
	cache.tick = cache.tick.saturating_add(1);
	let now = Instant::now();
	let tick = cache.tick;

	let expired = cache.entries.get(host).map(|entry| entry.expires_at <= now).unwrap_or(false);
	if expired {
		if let Some(entry) = cache.entries.remove(host) {
			cache.total_bytes = cache.total_bytes.saturating_sub(entry.bytes);
		}
		return None;
	}

	cache.entries.get_mut(host).map(|entry| {
		entry.last_accessed = tick;
		entry.clone()
	})
}

fn store_icon(host: String, mut icon: CachedIcon) {
	if icon.bytes > MAX_ICON_BYTES {
		return;
	}

	let mut cache = ICON_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
	cache.tick = cache.tick.saturating_add(1);
	icon.last_accessed = cache.tick;
	icon.expires_at = Instant::now() + CACHE_TTL;

	if let Some(existing) = cache.entries.remove(&host) {
		cache.total_bytes = cache.total_bytes.saturating_sub(existing.bytes);
	}

	cache.total_bytes += icon.bytes;
	cache.entries.insert(host, icon);

	while cache.total_bytes > MAX_CACHE_BYTES {
		let Some((evict_key, evict_bytes)) = cache
			.entries
			.iter()
			.min_by_key(|(_, entry)| entry.last_accessed)
			.map(|(key, entry)| (key.clone(), entry.bytes))
		else {
			break;
		};

		cache.entries.remove(&evict_key);
		cache.total_bytes = cache.total_bytes.saturating_sub(evict_bytes);
	}
}

fn response_from_icon(icon: CachedIcon) -> Response<Body> {
	Response::builder()
		.status(200)
		.header(CONTENT_TYPE, icon.content_type)
		.header(CACHE_CONTROL, CACHE_CONTROL_VALUE)
		.body(icon.body.into())
		.unwrap_or_default()
}

async fn fetch_icon(host: &str) -> Result<CachedIcon, String> {
	let favicon_url = format!("https://{host}/favicon.ico");
	if let Some(icon) = fetch_image(&favicon_url).await? {
		return Ok(icon);
	}

	let html_url = format!("https://{host}/");
	if let Some(icon_url) = discover_icon_url(&html_url).await? {
		if let Some(icon) = fetch_image(&icon_url).await? {
			return Ok(icon);
		}
	}

	Ok(default_icon())
}

async fn discover_icon_url(page_url: &str) -> Result<Option<String>, String> {
	let response = fetch_url(page_url, "text/html,*/*;q=0.8", MAX_REDIRECTS).await?;
	if !response.status().is_success() {
		return Ok(None);
	}

	let body = to_bytes(response.into_body()).await.map_err(|err| err.to_string())?;
	if body.len() > MAX_HTML_BYTES {
		return Ok(None);
	}

	let html = String::from_utf8_lossy(&body);
	let Some(base_url) = Url::parse(page_url).ok() else {
		return Ok(None);
	};

	for tag in LINK_TAG_REGEX.find_iter(&html) {
		let tag = tag.as_str();
		let rel = REL_ATTR_REGEX
			.captures(tag)
			.and_then(|captures| captures.get(1))
			.map(|value| value.as_str().to_ascii_lowercase())
			.unwrap_or_default();

		if !rel.split_ascii_whitespace().any(|part| part.contains("icon")) {
			continue;
		}

		let Some(href) = HREF_ATTR_REGEX.captures(tag).and_then(|captures| captures.get(1)).map(|value| value.as_str()) else {
			continue;
		};

		if let Ok(icon_url) = base_url.join(&href.replace("&amp;", "&")) {
			return Ok(Some(icon_url.into()));
		}
	}

	Ok(None)
}

async fn fetch_image(url: &str) -> Result<Option<CachedIcon>, String> {
	let response = fetch_url(url, "image/*,*/*;q=0.8", MAX_REDIRECTS).await?;
	if !response.status().is_success() {
		return Ok(None);
	}

	let content_type = response
		.headers()
		.get(CONTENT_TYPE)
		.and_then(|value| value.to_str().ok())
		.unwrap_or("image/x-icon")
		.to_string();

	if !content_type.starts_with("image/") {
		return Ok(None);
	}

	let body = to_bytes(response.into_body()).await.map_err(|err| err.to_string())?;
	if body.is_empty() || body.len() > MAX_ICON_BYTES {
		return Ok(None);
	}

	Ok(Some(CachedIcon {
		bytes: body.len(),
		body: body.to_vec(),
		content_type,
		last_accessed: 0,
		expires_at: Instant::now() + CACHE_TTL,
	}))
}

async fn fetch_url(url: &str, accept: &str, redirects_left: usize) -> Result<Response<Body>, String> {
	let mut current_url = url.to_string();
	let mut redirects_remaining = redirects_left;

	loop {
		let uri = current_url.parse::<Uri>().map_err(|err| err.to_string())?;
		let request = Request::builder()
			.method("GET")
			.uri(uri)
			.header("User-Agent", OAUTH_CLIENT.load_full().user_agent())
			.header("Accept", accept)
			.body(Body::empty())
			.map_err(|err| err.to_string())?;

		let response = CLIENT.request(request).await.map_err(|err| err.to_string())?;
		if !response.status().is_redirection() || redirects_remaining == 0 {
			return Ok(response);
		}

		let Some(location) = response.headers().get(LOCATION).and_then(|value| value.to_str().ok()) else {
			return Ok(response);
		};

		current_url = Url::parse(&current_url)
			.ok()
			.and_then(|base| base.join(location).ok())
			.map(|joined| joined.to_string())
			.unwrap_or_else(|| location.to_string());
		redirects_remaining -= 1;
	}
}

#[cfg(test)]
mod tests {
	use super::{CACHE_CONTROL_VALUE, CACHE_TTL, CACHE_TTL_DAYS};
	use std::time::Duration;

	#[test]
	fn site_icon_cache_ttl_is_tripled() {
		assert_eq!(CACHE_TTL_DAYS, 3);
		assert_eq!(CACHE_TTL, Duration::from_secs(60 * 60 * 24 * 3));
		assert_eq!(CACHE_CONTROL_VALUE, "public, max-age=259200, s-maxage=259200");
	}
}
