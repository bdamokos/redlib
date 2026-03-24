#![allow(clippy::cmp_owned)]

use std::collections::HashMap;

// CRATES
use crate::server::ResponseExt;
use crate::utils::{clear_sharded_cookie, deflate_decompress, param, preferences_from_bincode, redirect, template, write_sharded_cookie, Preferences};
use askama::Template;
use cookie::Cookie;
use futures_lite::StreamExt;
use hyper::{Body, Request, Response};
use time::{Duration, OffsetDateTime};
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

const PREFS: [&str; 21] = [
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
	// Split the body into parts
	let (parts, mut body) = req.into_parts();

	// Grab existing cookies
	let _cookies: Vec<Cookie<'_>> = parts
		.headers
		.get_all("Cookie")
		.iter()
		.filter_map(|header| Cookie::parse(header.to_str().unwrap_or_default()).ok())
		.collect();

	// Aggregate the body...
	// let whole_body = hyper::body::aggregate(req).await.map_err(|e| e.to_string())?;
	let body_bytes = body
		.try_fold(Vec::new(), |mut data, chunk| {
			data.extend_from_slice(&chunk);
			Ok(data)
		})
		.await
		.map_err(|e| e.to_string())?;

	let form = url::form_urlencoded::parse(&body_bytes).collect::<HashMap<_, _>>();

	let mut response = redirect(&settings_redirect_path("saved"));

	for &name in &PREFS {
		match form.get(name) {
			Some(value) => response.insert_cookie(
				Cookie::build((name.to_owned(), value.clone()))
					.path("/")
					.http_only(true)
					.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
					.into(),
			),
			None => response.remove_cookie(name.to_string()),
		};
	}

	Ok(response)
}

fn set_cookies_method(req: Request<Body>, remove_cookies: bool) -> Response<Body> {
	// Split the body into parts
	let (parts, _) = req.into_parts();

	// Grab existing cookies
	let _cookies: Vec<Cookie<'_>> = parts
		.headers
		.get_all("Cookie")
		.iter()
		.filter_map(|header| Cookie::parse(header.to_str().unwrap_or_default()).ok())
		.collect();

	let query = parts.uri.query().unwrap_or_default().to_string();

	let form = url::form_urlencoded::parse(query.as_bytes()).collect::<HashMap<_, _>>();

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

	for name in PREFS {
		match form.get(name) {
			Some(value) => response.insert_cookie(
				Cookie::build((name.to_owned(), value.clone()))
					.path("/")
					.http_only(true)
					.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
					.into(),
			),
			None => {
				if remove_cookies {
					response.remove_cookie(name.to_string());
				}
			}
		};
	}

	// Get subscriptions/filters to restore from query string
	let subscriptions = form.get("subscriptions");
	let filters = form.get("filters");
	let flair_subscriptions = form.get("flair_subscriptions");
	let req = Request::from_parts(parts, Body::empty());

	// If there are subscriptions to restore set them and delete any old subscriptions cookies, otherwise delete them all
	if let Some(subscriptions) = subscriptions {
		write_sharded_cookie(&mut response, &req, "subscriptions", subscriptions);
	} else {
		clear_sharded_cookie(&mut response, &req, "subscriptions");
	}

	// If there are filters to restore set them and delete any old filters cookies, otherwise delete them all
	if let Some(filters) = filters {
		write_sharded_cookie(&mut response, &req, "filters", filters);
	} else {
		clear_sharded_cookie(&mut response, &req, "filters");
	}

	if let Some(flair_subscriptions) = flair_subscriptions {
		write_sharded_cookie(&mut response, &req, "flair_subscriptions", flair_subscriptions);
	} else {
		clear_sharded_cookie(&mut response, &req, "flair_subscriptions");
	}

	response
}

fn settings_redirect_path(status: &str) -> String {
	format!("/settings?status={status}")
}

fn settings_status(req: &Request<Body>) -> Option<String> {
	param(&format!("?{}", req.uri().query().unwrap_or_default()), "status")
}

/// Set cookies using response "Set-Cookie" header
pub async fn restore(req: Request<Body>) -> Result<Response<Body>, String> {
	Ok(set_cookies_method(req, true))
}

pub async fn update(req: Request<Body>) -> Result<Response<Body>, String> {
	Ok(set_cookies_method(req, false))
}

pub async fn encoded_restore(req: Request<Body>) -> Result<Response<Body>, String> {
	let body = hyper::body::to_bytes(req.into_body())
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

	prefs.available_themes = vec![];

	let url = format!("/settings/restore/?redirect=settings%3Fstatus%3Dimported&{}", prefs.to_urlencoded()?);

	Ok(redirect(&url))
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
}
