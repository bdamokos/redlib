use crate::utils::{clear_sharded_cookie, deflate_compress, write_sharded_cookie};
use crate::video_mode::{FacetMode, VideoModeQuery, VIDEO_MODE_QUERY_COOKIE};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hyper::body::to_bytes;
use hyper::{Body, Request, Response, StatusCode};
use serde::Deserialize;

/// Max characters storable across all sharded `video_mode_query` cookies (`16 × 3800`).
const MAX_VIDEO_MODE_COOKIE_CHARS: usize = 16 * 3800;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ApplyFacetPayload {
	mode: FacetMode,
	values: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ApplyPayload {
	subreddits: ApplyFacetPayload,
	users: ApplyFacetPayload,
	age_min_hours: u32,
	age_max_hours: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct WrapperPayload {
	video_mode: ApplyPayload,
}

fn encode_query_cookie(query: &VideoModeQuery) -> Result<String, String> {
	let json = serde_json::to_vec(query).map_err(|e| e.to_string())?;
	let compressed = deflate_compress(json)?;
	Ok(URL_SAFE_NO_PAD.encode(compressed))
}

fn sanitize_values(values: Vec<String>, max_items: usize) -> Vec<String> {
	let mut out = Vec::new();
	for value in values.into_iter().map(|v| v.trim().to_string()) {
		if value.is_empty() {
			continue;
		}
		out.push(value);
		if out.len() >= max_items {
			break;
		}
	}
	out
}

pub async fn apply(mut req: Request<Body>) -> Result<Response<Body>, String> {
	let bytes = to_bytes(req.body_mut()).await.map_err(|e| e.to_string())?;
	let payload: WrapperPayload = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;

	let max_items = 2_000usize;
	let query = VideoModeQuery {
		subreddit_mode: payload.video_mode.subreddits.mode,
		user_mode: payload.video_mode.users.mode,
		selected_subreddits: sanitize_values(payload.video_mode.subreddits.values, max_items),
		selected_users: sanitize_values(payload.video_mode.users.values, max_items),
		age_min_hours: payload.video_mode.age_min_hours,
		age_max_hours: payload.video_mode.age_max_hours,
	};

	let encoded = encode_query_cookie(&query)?;
	if encoded.len() > MAX_VIDEO_MODE_COOKIE_CHARS {
		return Ok(Response::builder()
			.status(StatusCode::PAYLOAD_TOO_LARGE)
			.header("content-type", "application/json; charset=utf-8")
			.header("cache-control", "no-store")
			.body(Body::from(r#"{"error":"video_mode_cookie_too_large"}"#))
			.map_err(|e| e.to_string())?);
	}

	let mut response = Response::builder()
		.status(StatusCode::NO_CONTENT)
		.header("cache-control", "no-store")
		.body(Body::empty())
		.map_err(|e| e.to_string())?;
	write_sharded_cookie(&mut response, &req, VIDEO_MODE_QUERY_COOKIE, &encoded);
	Ok(response)
}

pub async fn clear(req: Request<Body>) -> Result<Response<Body>, String> {
	let mut response = Response::builder()
		.status(StatusCode::NO_CONTENT)
		.header("cache-control", "no-store")
		.body(Body::empty())
		.map_err(|e| e.to_string())?;
	clear_sharded_cookie(&mut response, &req, VIDEO_MODE_QUERY_COOKIE);
	Ok(response)
}

/// `GET /api/video-mode/clear-redirect?redirect=%2Fvideo-length%3F...` — same-site path + query only.
pub async fn clear_redirect(req: Request<Body>) -> Result<Response<Body>, String> {
	let raw = req.uri().query().unwrap_or_default();
	let redirect_target = url::form_urlencoded::parse(raw.as_bytes())
		.into_owned()
		.find(|(key, _)| key == "redirect")
		.map(|(_, value)| value)
		.unwrap_or_default();

	if redirect_target.is_empty() || !redirect_target.starts_with('/') || redirect_target.starts_with("//") {
		return Err("Invalid redirect target".to_string());
	}

	let mut response = Response::builder()
		.status(StatusCode::FOUND)
		.header("location", redirect_target)
		.header("cache-control", "no-store")
		.body(Body::empty())
		.map_err(|e| e.to_string())?;
	clear_sharded_cookie(&mut response, &req, VIDEO_MODE_QUERY_COOKIE);
	Ok(response)
}
