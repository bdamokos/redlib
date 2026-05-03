use crate::client::json;
use crate::utils::{clear_sharded_cookie, format_num, parse_post, read_sharded_cookie, redirect, template, write_sharded_cookie, Post, Preferences};
use askama::Template;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hyper::body::to_bytes;
use hyper::header::CONTENT_TYPE;
use hyper::StatusCode;
use hyper::{Body, Request, Response};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use time::OffsetDateTime;
use url::form_urlencoded;

const BOOKMARK_COOKIE: &str = "bookmarks";
const BOOKMARK_BATCH_SIZE: usize = 100;
const BOOKMARKS_PER_PAGE: usize = 25;
const MAX_BOOKMARKS: usize = 200;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BookmarkEntry {
	#[serde(rename = "i")]
	pub id: String,
	#[serde(rename = "t")]
	pub title: String,
	#[serde(rename = "c")]
	pub community: String,
	#[serde(rename = "p")]
	pub permalink: String,
	#[serde(rename = "k")]
	pub last_known_comments: u64,
	#[serde(rename = "v")]
	pub last_visited_comments: u64,
	#[serde(rename = "b")]
	pub bookmarked_at: u64,
	#[serde(rename = "l")]
	pub last_visited_at: u64,
}

#[derive(Template)]
#[template(path = "bookmarks.html")]
pub struct BookmarksTemplate {
	pub posts: Vec<Post>,
	pub prefs: Preferences,
	pub url: String,
	pub prev_page_url: String,
	pub next_page_url: String,
	pub total_bookmarks: usize,
	pub unread_posts: usize,
	pub total_new_comments: u64,
	pub page: usize,
}

crate::impl_template_str_eq!(BookmarksTemplate);

fn now_unix() -> u64 {
	OffsetDateTime::now_utc().unix_timestamp().max(0) as u64
}

fn comment_count_from_post(post: &Post) -> u64 {
	post.comments.1.parse::<u64>().unwrap_or_default()
}

fn encode_bookmarks(entries: &[BookmarkEntry]) -> Result<String, String> {
	if entries.is_empty() {
		return Ok(String::new());
	}
	let json = serde_json::to_vec(entries).map_err(|e| e.to_string())?;
	Ok(URL_SAFE_NO_PAD.encode(json))
}

fn decode_bookmarks(encoded: &str) -> Result<Vec<BookmarkEntry>, String> {
	if encoded.is_empty() {
		return Ok(Vec::new());
	}
	let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|e| e.to_string())?;
	serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn normalize_entries(entries: &mut Vec<BookmarkEntry>) {
	entries.retain(|entry| !entry.id.trim().is_empty() && !entry.permalink.trim().is_empty());
	let mut by_id: HashMap<String, BookmarkEntry> = HashMap::new();
	for entry in entries.drain(..) {
		match by_id.get(&entry.id) {
			Some(existing) if existing.bookmarked_at >= entry.bookmarked_at => {}
			_ => {
				by_id.insert(entry.id.clone(), entry);
			}
		}
	}
	entries.extend(by_id.into_values());
	entries.sort_by(|left, right| right.bookmarked_at.cmp(&left.bookmarked_at).then_with(|| right.id.cmp(&left.id)));
	if entries.len() > MAX_BOOKMARKS {
		entries.truncate(MAX_BOOKMARKS);
	}
}

pub fn read_bookmarks(req: &Request<Body>) -> Vec<BookmarkEntry> {
	let encoded = read_sharded_cookie(req, BOOKMARK_COOKIE);
	let mut entries = decode_bookmarks(&encoded).unwrap_or_default();
	normalize_entries(&mut entries);
	entries
}

fn write_bookmarks(response: &mut Response<Body>, req: &Request<Body>, entries: &[BookmarkEntry]) -> Result<(), String> {
	let mut normalized = entries.to_vec();
	normalize_entries(&mut normalized);
	if normalized.is_empty() {
		clear_sharded_cookie(response, req, BOOKMARK_COOKIE);
		return Ok(());
	}
	write_sharded_cookie(response, req, BOOKMARK_COOKIE, &encode_bookmarks(&normalized)?);
	Ok(())
}

fn bookmark_lookup(req: &Request<Body>) -> HashMap<String, BookmarkEntry> {
	read_bookmarks(req).into_iter().map(|entry| (entry.id.clone(), entry)).collect()
}

pub fn decorate_posts(req: &Request<Body>, posts: &mut [Post]) {
	let bookmarks = bookmark_lookup(req);
	for post in posts {
		if let Some(bookmark) = bookmarks.get(&post.id) {
			post.bookmarked = true;
			let delta = comment_count_from_post(post).saturating_sub(bookmark.last_visited_comments);
			post.bookmark_new_comments = (delta > 0).then_some(delta);
		}
	}
}

pub fn decorate_bookmarked_posts(posts: &mut [Post], bookmarks: &HashMap<String, BookmarkEntry>) {
	for post in posts {
		if let Some(bookmark) = bookmarks.get(&post.id) {
			post.bookmarked = true;
			post.bookmark_new_comments = Some(comment_count_from_post(post).saturating_sub(bookmark.last_visited_comments));
		}
	}
}

pub fn decorate_post(req: &Request<Body>, post: &mut Post) {
	decorate_posts(req, std::slice::from_mut(post));
}

pub fn update_visit_baseline(response: &mut Response<Body>, req: &Request<Body>, post: &Post) -> Result<(), String> {
	let mut entries = read_bookmarks(req);
	let current_comments = comment_count_from_post(post);
	let now = now_unix();
	let mut changed = false;

	for entry in &mut entries {
		if entry.id != post.id {
			continue;
		}
		let next_last_known = entry.last_known_comments.max(current_comments);
		if entry.last_visited_comments != current_comments || entry.last_known_comments != next_last_known || entry.last_visited_at != now {
			entry.last_known_comments = next_last_known;
			entry.last_visited_comments = current_comments;
			entry.last_visited_at = now;
			changed = true;
		}
	}

	if changed {
		write_bookmarks(response, req, &entries)?;
	}

	Ok(())
}

async fn parse_form(req: Request<Body>) -> Result<(hyper::http::request::Parts, HashMap<String, String>), String> {
	let (parts, body) = req.into_parts();
	let body_bytes = to_bytes(body).await.map_err(|e| e.to_string())?;
	let form = form_urlencoded::parse(&body_bytes).into_owned().collect::<HashMap<_, _>>();
	Ok((parts, form))
}

fn request_is_ajax(parts: &hyper::http::request::Parts) -> bool {
	parts.headers.get("X-Redlib-Ajax").and_then(|value| value.to_str().ok()).is_some_and(|value| value == "1")
}

fn ajax_bookmark_ok_response() -> Response<Body> {
	Response::builder()
		.status(200)
		.header("Cache-Control", "no-store")
		.header(CONTENT_TYPE, "text/plain; charset=utf-8")
		.body(Body::from("OK"))
		.unwrap_or_default()
}

fn redirect_target(_parts: &hyper::http::request::Parts, form: &HashMap<String, String>) -> String {
	form
		.get("redirect")
		.map(|value| if value.starts_with('/') { value.clone() } else { format!("/{value}") })
		.unwrap_or_else(|| "/bookmarks".to_string())
}

fn add_entry(entries: &mut Vec<BookmarkEntry>, entry: BookmarkEntry) {
	entries.retain(|existing| existing.id != entry.id);
	entries.push(entry);
	normalize_entries(entries);
}

async fn fetch_bookmark_entry(id: &str) -> Result<BookmarkEntry, String> {
	let response = json(format!("/api/info.json?raw_json=1&id=t3_{id}"), false).await?;
	let child = response["data"]["children"]
		.as_array()
		.and_then(|children| children.first())
		.ok_or_else(|| "Bookmark post not found".to_string())?;
	let post = parse_post(child).await;
	let comment_count = comment_count_from_post(&post);
	let now = now_unix();
	Ok(BookmarkEntry {
		id: post.id,
		title: post.title,
		community: post.community,
		permalink: post.permalink,
		last_known_comments: comment_count,
		last_visited_comments: comment_count,
		bookmarked_at: now,
		last_visited_at: now,
	})
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
	Response::builder()
		.status(status)
		.header("Cache-Control", "no-store")
		.body(Body::from(message.to_string()))
		.unwrap_or_default()
}

fn entry_from_form(id: &str, form: &HashMap<String, String>) -> Option<BookmarkEntry> {
	let title = form.get("title")?.trim();
	let community = form.get("community")?.trim();
	let permalink = form.get("permalink")?.trim();
	let comments = form.get("comments").map(|v| v.trim()).unwrap_or_default();
	if title.is_empty() || community.is_empty() || permalink.is_empty() {
		return None;
	}

	let comment_count = comments.parse::<u64>().unwrap_or_default();
	let now = now_unix();
	Some(BookmarkEntry {
		id: id.to_string(),
		title: title.to_string(),
		community: community.to_string(),
		permalink: permalink.to_string(),
		last_known_comments: comment_count,
		last_visited_comments: comment_count,
		bookmarked_at: now,
		last_visited_at: now,
	})
}

pub async fn add(req: Request<Body>) -> Result<Response<Body>, String> {
	let (parts, form) = parse_form(req).await?;
	let id = form.get("id").cloned().unwrap_or_default();
	let redirect_to = redirect_target(&parts, &form);

	if id.is_empty() {
		return Ok(error_response(StatusCode::BAD_REQUEST, "Missing bookmark id"));
	}

	let is_ajax = request_is_ajax(&parts);
	let new_entry = if let Some(entry) = entry_from_form(&id, &form) {
		entry
	} else {
		match fetch_bookmark_entry(&id).await {
			Ok(entry) => entry,
			Err(message) => {
				let status = if message.to_lowercase().contains("not found") {
					StatusCode::NOT_FOUND
				} else {
					StatusCode::BAD_GATEWAY
				};
				return Ok(error_response(status, &message));
			}
		}
	};

	let req = Request::from_parts(parts, Body::empty());
	let mut entries = read_bookmarks(&req);
	add_entry(&mut entries, new_entry);

	let mut response = if is_ajax {
		ajax_bookmark_ok_response()
	} else {
		redirect(&redirect_to)
	};
	write_bookmarks(&mut response, &req, &entries)?;
	Ok(response)
}

pub async fn remove(req: Request<Body>) -> Result<Response<Body>, String> {
	let (parts, form) = parse_form(req).await?;
	let id = form.get("id").cloned().unwrap_or_default();
	let redirect_to = redirect_target(&parts, &form);
	let is_ajax = request_is_ajax(&parts);
	if id.is_empty() {
		return Ok(error_response(StatusCode::BAD_REQUEST, "Missing bookmark id"));
	}
	let req = Request::from_parts(parts, Body::empty());
	let mut entries = read_bookmarks(&req);
	entries.retain(|entry| entry.id != id);

	let mut response = if is_ajax {
		ajax_bookmark_ok_response()
	} else {
		redirect(&redirect_to)
	};
	write_bookmarks(&mut response, &req, &entries)?;
	Ok(response)
}

async fn fetch_live_posts(entries: &[BookmarkEntry]) -> HashMap<String, Post> {
	let mut posts = HashMap::new();
	for chunk in entries.chunks(BOOKMARK_BATCH_SIZE) {
		let ids = chunk.iter().map(|entry| format!("t3_{}", entry.id)).collect::<Vec<_>>().join(",");
		let path = format!("/api/info.json?raw_json=1&id={ids}");
		let Ok(response) = json(path, false).await else {
			continue;
		};
		let Some(children) = response["data"]["children"].as_array() else {
			continue;
		};
		for child in children {
			let post = parse_post(child).await;
			posts.insert(post.id.clone(), post);
		}
	}
	posts
}

fn fallback_post(entry: &BookmarkEntry) -> Post {
	let mut post = Post::default();
	post.id = entry.id.clone();
	post.title = entry.title.clone();
	post.community = entry.community.clone();
	post.permalink = entry.permalink.clone();
	post.comments = format_num(entry.last_known_comments as i64);
	post.bookmarked = true;
	post
}

fn page_url(base: &str, page: usize) -> String {
	if page == 0 {
		base.to_string()
	} else {
		format!("{base}?page={page}")
	}
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let url = req.uri().to_string();
	let query = req.uri().query().unwrap_or_default();
	let page = form_urlencoded::parse(query.as_bytes())
		.find(|(key, _)| key == "page")
		.and_then(|(_, value)| value.parse::<usize>().ok())
		.unwrap_or(0);

	let entries = read_bookmarks(&req);
	let total_bookmarks = entries.len();
	if entries.is_empty() {
		return Ok(template(&BookmarksTemplate {
			posts: Vec::new(),
			prefs,
			url,
			prev_page_url: String::new(),
			next_page_url: String::new(),
			total_bookmarks: 0,
			unread_posts: 0,
			total_new_comments: 0,
			page,
		}));
	}

	let live_posts = fetch_live_posts(&entries).await;
	let bookmark_lookup = entries.iter().cloned().map(|entry| (entry.id.clone(), entry)).collect::<HashMap<_, _>>();

	let mut ordered = entries
		.iter()
		.cloned()
		.map(|entry| {
			let current_comments = live_posts.get(&entry.id).map(comment_count_from_post).unwrap_or(entry.last_known_comments);
			let new_comments = current_comments.saturating_sub(entry.last_visited_comments);
			(entry, new_comments)
		})
		.collect::<Vec<_>>();

	ordered.sort_by(|(left, left_new), (right, right_new)| {
		right_new
			.cmp(left_new)
			.then_with(|| right.last_visited_at.cmp(&left.last_visited_at))
			.then_with(|| right.bookmarked_at.cmp(&left.bookmarked_at))
	});

	let unread_posts = ordered.iter().filter(|(_, new_comments)| *new_comments > 0).count();
	let total_new_comments = ordered.iter().map(|(_, new_comments)| *new_comments).sum::<u64>();
	let start = page.saturating_mul(BOOKMARKS_PER_PAGE);
	let end = (start + BOOKMARKS_PER_PAGE).min(ordered.len());
	let page_slice = if start < ordered.len() { &ordered[start..end] } else { &[] };

	let mut posts = page_slice
		.iter()
		.map(|(entry, _)| live_posts.get(&entry.id).cloned().unwrap_or_else(|| fallback_post(entry)))
		.collect::<Vec<_>>();
	decorate_bookmarked_posts(&mut posts, &bookmark_lookup);

	let prev_page_url = if page > 0 { page_url("/bookmarks", page - 1) } else { String::new() };
	let next_page_url = if end < ordered.len() { page_url("/bookmarks", page + 1) } else { String::new() };

	Ok(template(&BookmarksTemplate {
		posts,
		prefs,
		url,
		prev_page_url,
		next_page_url,
		total_bookmarks,
		unread_posts,
		total_new_comments,
		page,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;
	use hyper::{Body, Request, Response};

	fn entry(id: &str, bookmarked_at: u64) -> BookmarkEntry {
		BookmarkEntry {
			id: id.to_string(),
			title: format!("Post {id}"),
			community: "rust".to_string(),
			permalink: format!("/r/rust/comments/{id}/post_{id}"),
			last_known_comments: 4,
			last_visited_comments: 2,
			bookmarked_at,
			last_visited_at: bookmarked_at,
		}
	}

	fn cookie_request_from_response(response: &Response<Body>) -> Request<Body> {
		let cookie_header = response
			.headers()
			.get_all("set-cookie")
			.iter()
			.filter_map(|value| value.to_str().ok())
			.filter_map(|value| value.split(';').next())
			.collect::<Vec<_>>()
			.join("; ");
		Request::builder().uri("/").header("cookie", cookie_header).body(Body::empty()).unwrap()
	}

	#[test]
	fn normalize_entries_keeps_newest_copy_per_post() {
		let mut entries = vec![entry("abc", 10), entry("abc", 20), entry("def", 15)];
		normalize_entries(&mut entries);

		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].id, "abc");
		assert_eq!(entries[0].bookmarked_at, 20);
		assert_eq!(entries[1].id, "def");
	}

	#[test]
	fn write_then_read_bookmarks_round_trips() {
		let req = Request::builder().uri("/").body(Body::empty()).unwrap();
		let mut response = Response::new(Body::empty());
		write_bookmarks(&mut response, &req, &[entry("abc", 10), entry("def", 20)]).unwrap();

		let cookie_req = cookie_request_from_response(&response);
		let decoded = read_bookmarks(&cookie_req);

		assert_eq!(decoded.len(), 2);
		assert_eq!(decoded[0].id, "def");
		assert_eq!(decoded[1].id, "abc");
	}

	#[test]
	fn update_visit_baseline_refreshes_counts_for_matching_post() {
		let req = Request::builder()
			.uri("/")
			.header("cookie", {
				let mut response = Response::new(Body::empty());
				write_bookmarks(&mut response, &Request::builder().uri("/").body(Body::empty()).unwrap(), &[entry("abc", 10)]).unwrap();
				response
					.headers()
					.get_all("set-cookie")
					.iter()
					.filter_map(|value| value.to_str().ok())
					.filter_map(|value| value.split(';').next())
					.collect::<Vec<_>>()
					.join("; ")
			})
			.body(Body::empty())
			.unwrap();
		let mut response = Response::new(Body::empty());
		let mut post = Post::default();
		post.id = "abc".to_string();
		post.comments = ("12".to_string(), "12".to_string());

		update_visit_baseline(&mut response, &req, &post).unwrap();
		let updated_req = cookie_request_from_response(&response);
		let updated = read_bookmarks(&updated_req);

		assert_eq!(updated[0].last_known_comments, 12);
		assert_eq!(updated[0].last_visited_comments, 12);
	}
}
