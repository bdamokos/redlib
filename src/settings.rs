#![allow(clippy::cmp_owned)]

use std::collections::HashMap;

// CRATES
use crate::prefs_store;
use crate::utils::{decode_flair_subscriptions, deflate_decompress, param, preferences_from_bincode, redirect, template, Preferences};
use askama::Template;
use futures_lite::StreamExt;
use hyper::{Body, Request, Response};
use tokio::time::timeout;
use url::form_urlencoded;

// STRUCTS
#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTemplate {
	pub prefs: Preferences,
	pub status: String,
	pub url: String,
}

crate::impl_template_str_eq!(SettingsTemplate);

// CONSTANTS

const PREFS: [&str; 25] = [
	"theme",
	"front_page",
	"layout",
	"wide",
	"comment_sort",
	"post_sort",
	"blur_spoiler",
	"show_nsfw",
	"blur_nsfw",
	"use_dash",
	"use_hls",
	"hide_hls_notification",
	"autoplay_videos",
	"video_previews",
	"direct_redgifs_media",
	"redgifs_poster_quality",
	"redgifs_filter_exempt_subreddits",
	"hide_sidebar_and_summary",
	"fixed_navbar",
	"hide_awards",
	"hide_score",
	"disable_visit_reddit_confirmation",
	"image_quality",
	"video_quality",
	"remove_default_feeds",
];

// FUNCTIONS

/// Retrieve cookies from request "Cookie" header
pub async fn get(req: Request<Body>) -> Result<Response<Body>, String> {
	let url = req.uri().to_string();
	Ok(template(&SettingsTemplate {
		prefs: Preferences::new(&req),
		status: settings_status(&req).unwrap_or_default(),
		url,
	}))
}

/// Set cookies using response "Set-Cookie" header
pub async fn set(req: Request<Body>) -> Result<Response<Body>, String> {
	let mut prefs = Preferences::new(&req);

	// Split the body into parts
	let (parts, mut body) = req.into_parts();

	// Aggregate the body...
	// let whole_body = hyper::body::aggregate(req).await.map_err(|e| e.to_string())?;
	let body_bytes = body
		.try_fold(Vec::new(), |mut data, chunk| {
			data.extend_from_slice(&chunk);
			Ok(data)
		})
		.await
		.map_err(|e| e.to_string())?;

	let form = url::form_urlencoded::parse(&body_bytes).into_owned().collect::<HashMap<_, _>>();
	apply_standard_preferences(&mut prefs, &form, true);

	let mut response = redirect(&settings_redirect_path("saved"));
	let req = Request::from_parts(parts, Body::empty());
	prefs_store::save_preferences(&mut response, &req, &prefs)?;

	Ok(response)
}

fn set_cookies_method(req: Request<Body>, remove_cookies: bool) -> Result<Response<Body>, String> {
	// Split the body into parts
	let (parts, _) = req.into_parts();

	let query = parts.uri.query().unwrap_or_default().to_string();

	let form = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect::<HashMap<_, _>>();

	let path = match form.get("redirect") {
		Some(value) => {
			let value = value.replace("%26", "&").replace("%23", "#");
			if value.starts_with('/') {
				value
			} else {
				format!("/{value}")
			}
		}
		None => "/".to_string(),
	};

	let mut response = redirect(&path);
	let req = Request::from_parts(parts, Body::empty());
	let mut prefs = if remove_cookies { default_preferences() } else { Preferences::new(&req) };

	apply_standard_preferences(&mut prefs, &form, remove_cookies);
	apply_feed_preferences(&mut prefs, &form, remove_cookies)?;
	prefs_store::save_preferences(&mut response, &req, &prefs)?;

	Ok(response)
}

fn settings_redirect_path(status: &str) -> String {
	format!("/settings?status={status}")
}

fn settings_status(req: &Request<Body>) -> Option<String> {
	param(&format!("?{}", req.uri().query().unwrap_or_default()), "status")
}

/// Set cookies using response "Set-Cookie" header
pub async fn restore(req: Request<Body>) -> Result<Response<Body>, String> {
	set_cookies_method(req, true)
}

pub async fn update(req: Request<Body>) -> Result<Response<Body>, String> {
	set_cookies_method(req, false)
}

pub async fn encoded_restore(req: Request<Body>) -> Result<Response<Body>, String> {
	let (parts, body_stream) = req.into_parts();
	let body = hyper::body::to_bytes(body_stream)
		.await
		.map_err(|e| format!("Failed to get bytes from request body: {e}"))?;

	if body.len() > 1024 * 1024 {
		return Err("Request body too large".to_string());
	}

	let encoded_prefs = form_urlencoded::parse(&body)
		.find(|(key, _)| key == "encoded_prefs")
		.map(|(_, value)| value)
		.ok_or_else(|| "encoded_prefs parameter not found in request body".to_string())?;

	let bytes = base2048::decode(&encoded_prefs).ok_or_else(|| "Failed to decode base2048 encoded preferences".to_string())?;

	let out = timeout(std::time::Duration::from_secs(1), async { deflate_decompress(bytes) })
		.await
		.map_err(|e| format!("Failed to decompress bytes: {e}"))??;

	let mut prefs: Preferences = timeout(std::time::Duration::from_secs(1), async { preferences_from_bincode(&out) })
		.await
		.map_err(|e| format!("Failed to deserialize preferences: {e}"))?
		.map_err(|e| format!("Failed to deserialize bytes into Preferences struct: {e}"))?;

	prefs.prepare_for_runtime();

	let restore_req = Request::from_parts(parts, Body::empty());
	let mut response = redirect(&settings_redirect_path("imported"));
	prefs_store::save_preferences(&mut response, &restore_req, &prefs)?;
	Ok(response)
}

fn default_preferences() -> Preferences {
	let req = Request::builder().uri("/").body(Body::empty()).expect("empty request should build");
	Preferences::from_cookies(&req)
}

fn apply_standard_preferences(prefs: &mut Preferences, form: &HashMap<String, String>, reset_missing: bool) {
	let defaults = reset_missing.then(default_preferences);
	for name in PREFS {
		if let Some(value) = form.get(name) {
			apply_standard_preference(prefs, name, value);
		} else if let Some(defaults) = &defaults {
			if let Some(value) = defaults.setting_value(name) {
				apply_standard_preference(prefs, name, &value);
			}
		}
	}
	prefs.prepare_for_runtime();
}

fn apply_standard_preference(prefs: &mut Preferences, name: &str, value: &str) {
	match name {
		"theme" => prefs.theme = value.to_string(),
		"front_page" => prefs.front_page = value.to_string(),
		"layout" => prefs.layout = value.to_string(),
		"wide" => prefs.wide = value.to_string(),
		"comment_sort" => prefs.comment_sort = value.to_string(),
		"post_sort" => prefs.post_sort = value.to_string(),
		"blur_spoiler" => prefs.blur_spoiler = value.to_string(),
		"show_nsfw" => prefs.show_nsfw = value.to_string(),
		"blur_nsfw" => prefs.blur_nsfw = value.to_string(),
		"use_dash" => prefs.use_dash = value.to_string(),
		"use_hls" => prefs.use_hls = value.to_string(),
		"hide_hls_notification" => prefs.hide_hls_notification = value.to_string(),
		"autoplay_videos" => prefs.autoplay_videos = value.to_string(),
		"video_previews" => prefs.video_previews = value.to_string(),
		"direct_redgifs_media" => prefs.direct_redgifs_media = value.to_string(),
		"redgifs_poster_quality" => prefs.redgifs_poster_quality = value.to_string(),
		"redgifs_filter_exempt_subreddits" => {
			prefs.redgifs_filter_exempt_subreddits = plus_list(&value.replace(',', "+"));
		}
		"hide_sidebar_and_summary" => prefs.hide_sidebar_and_summary = value.to_string(),
		"fixed_navbar" => prefs.fixed_navbar = value.to_string(),
		"hide_awards" => prefs.hide_awards = value.to_string(),
		"hide_score" => prefs.hide_score = value.to_string(),
		"disable_visit_reddit_confirmation" => prefs.disable_visit_reddit_confirmation = value.to_string(),
		"image_quality" => prefs.image_quality = value.to_string(),
		"video_quality" => prefs.video_quality = value.to_string(),
		"remove_default_feeds" => prefs.remove_default_feeds = value.to_string(),
		_ => {}
	}
}

fn apply_feed_preferences(prefs: &mut Preferences, form: &HashMap<String, String>, reset_missing: bool) -> Result<(), String> {
	if let Some(subscriptions) = form.get("subscriptions") {
		prefs.subscriptions = plus_list(subscriptions);
	} else if reset_missing {
		prefs.subscriptions.clear();
	}

	if let Some(filters) = form.get("filters") {
		prefs.filters = plus_list(filters);
	} else if reset_missing {
		prefs.filters.clear();
	}

	if let Some(flair_subscriptions) = form.get("flair_subscriptions") {
		prefs.flair_subscriptions = decode_flair_subscriptions(flair_subscriptions)?;
	} else if reset_missing {
		prefs.flair_subscriptions.clear();
	}

	prefs.prepare_for_runtime();
	Ok(())
}

fn plus_list(value: &str) -> Vec<String> {
	value.split('+').map(str::trim).filter(|item| !item.is_empty()).map(ToString::to_string).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn settings_redirect_path_uses_settings_page() {
		assert_eq!(settings_redirect_path("saved"), "/settings?status=saved");
	}

	#[test]
	fn settings_status_reads_query_string() {
		let req = Request::builder().uri("/settings?status=imported").body(Body::empty()).unwrap();

		assert_eq!(settings_status(&req), Some("imported".to_string()));
	}

	#[test]
	fn settings_save_normalizes_redgifs_filter_exemptions() {
		let mut prefs = Preferences::default();
		let form = HashMap::from([("redgifs_filter_exempt_subreddits".to_string(), "r/FanCommunity, anotherCommunity+fancommunity".to_string())]);
		apply_standard_preferences(&mut prefs, &form, false);
		assert_eq!(prefs.redgifs_filter_exempt_subreddits, ["anothercommunity".to_string(), "fancommunity".to_string()]);
	}
}
