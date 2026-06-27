use crate::prefs_store;
use crate::utils::{redirect, PostFeedbackEntry, Preferences};
use hyper::body::to_bytes;
use hyper::header::CONTENT_TYPE;
use hyper::{Body, Request, Response, StatusCode};
use serde_json::json;
use std::collections::HashMap;
use url::form_urlencoded;

fn request_is_ajax(parts: &hyper::http::request::Parts) -> bool {
	parts.headers.get("X-Redlib-Ajax").and_then(|value| value.to_str().ok()).is_some_and(|value| value == "1")
}

fn redirect_target(form: &HashMap<String, String>) -> String {
	form
		.get("redirect")
		.map(|value| if value.starts_with('/') { value.clone() } else { format!("/{value}") })
		.unwrap_or_else(|| "/".to_string())
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("Cache-Control", "no-store")
		.header(CONTENT_TYPE, "text/plain; charset=utf-8")
		.body(Body::from(message.to_string()))
		.unwrap_or_default()
}

fn ajax_ok_response(signal: &str) -> Response<Body> {
	Response::builder()
		.status(200)
		.header("Cache-Control", "no-store")
		.header(CONTENT_TYPE, "application/json; charset=utf-8")
		.body(Body::from(json!({ "signal": signal }).to_string()))
		.unwrap_or_default()
}

pub async fn update(req: Request<Body>) -> Result<Response<Body>, String> {
	let mut prefs = Preferences::new(&req);
	let (parts, body) = req.into_parts();
	let body_bytes = to_bytes(body).await.map_err(|e| e.to_string())?;
	let form = form_urlencoded::parse(&body_bytes).into_owned().collect::<HashMap<_, _>>();
	let is_ajax = request_is_ajax(&parts);
	let redirect_to = redirect_target(&form);

	let id = form.get("id").map(|value| value.trim()).unwrap_or_default();
	let community = form.get("community").map(|value| value.trim()).unwrap_or_default();
	let model = form.get("model").map(|value| value.trim()).filter(|value| !value.is_empty()).unwrap_or("manual");
	let signal = form.get("signal").map(|value| value.trim()).unwrap_or_default();
	if id.is_empty() || community.is_empty() || signal.is_empty() {
		return Ok(error_response(StatusCode::BAD_REQUEST, "Missing feedback fields"));
	}

	let entry = PostFeedbackEntry {
		post_id: id.to_string(),
		model: model.to_string(),
		subreddit: community.to_string(),
		author: form.get("author").cloned().unwrap_or_default(),
		signal: signal.to_string(),
		created_at_unix: crate::cache_persistence::unix_now(),
	};
	let next_signal = match prefs.update_post_feedback(entry) {
		Ok(signal) => signal,
		Err(message) => return Ok(error_response(StatusCode::BAD_REQUEST, &message)),
	};

	prefs.prepare_for_runtime();
	let request = Request::from_parts(parts, Body::empty());
	let mut response = if is_ajax { ajax_ok_response(&next_signal) } else { redirect(&redirect_to) };
	prefs_store::save_preferences(&mut response, &request, &prefs)?;
	Ok(response)
}
