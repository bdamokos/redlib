//! Server-side "seen post" memory keyed by browser session cookie (`rl_session`).
//! TTL matches [`crate::post_cache::CACHE_TTL_SECS`] so this data drops with feed-session scale.

use crate::post_cache::CACHE_TTL_SECS;
use crate::server::{RequestExt, ResponseExt};
use crate::utils::{param, Post};
use cookie::Cookie;
use hyper::{Body, Request, Response};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use time::{Duration, OffsetDateTime};

pub const RL_SESSION_COOKIE: &str = "rl_session";
pub const RL_HIDE_SEEN_COOKIE: &str = "rl_hide_seen";

#[derive(Clone)]
pub struct FeedSeenCookieCtx {
	pub session_id: String,
	pub hide_seen_on: bool,
	pub applies: bool,
	pub needs_session_cookie: bool,
	pub hide_seen_cookie_override: Option<String>,
}

struct SeenState {
	ids: HashSet<String>,
	last_touch: Instant,
}

static SEEN_BY_SESSION: LazyLock<Mutex<HashMap<String, SeenState>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn ttl_duration() -> Duration {
	Duration::seconds(CACHE_TTL_SECS as i64)
}

fn prune_seen_locked(map: &mut HashMap<String, SeenState>) {
	let ttl = CACHE_TTL_SECS;
	map.retain(|_, state| state.last_touch.elapsed().as_secs() <= ttl);
}

/// When `hide_seen` is true, drop posts already in the session set, then record returned IDs.
/// When false, show everything but still record what we served (so turning hide back on stays consistent).
pub fn filter_and_record_seen(session_id: &str, hide_seen: bool, posts: Vec<Post>) -> Vec<Post> {
	if session_id.trim().is_empty() {
		return posts;
	}
	let mut map = SEEN_BY_SESSION.lock().unwrap_or_else(|e| e.into_inner());
	prune_seen_locked(&mut map);

	if !hide_seen {
		let ids: Vec<String> = posts.iter().map(|p| p.id.clone()).collect();
		let entry = map.entry(session_id.to_string()).or_insert(SeenState {
			ids: HashSet::new(),
			last_touch: Instant::now(),
		});
		entry.ids.extend(ids);
		entry.last_touch = Instant::now();
		return posts;
	}

	let mut result = Vec::new();
	let mut new_ids: Vec<String> = Vec::new();
	{
		let seen = map.get(session_id).map(|s| &s.ids);
		for post in posts {
			if seen.map_or(true, |ids| !ids.contains(&post.id)) {
				new_ids.push(post.id.clone());
				result.push(post);
			}
		}
	}
	let entry = map.entry(session_id.to_string()).or_insert(SeenState {
		ids: HashSet::new(),
		last_touch: Instant::now(),
	});
	entry.ids.extend(new_ids);
	entry.last_touch = Instant::now();
	result
}

pub fn filter_unseen(session_id: &str, posts: &[Post]) -> Vec<Post> {
	if session_id.trim().is_empty() {
		return posts.to_vec();
	}
	let mut map = SEEN_BY_SESSION.lock().unwrap_or_else(|e| e.into_inner());
	prune_seen_locked(&mut map);
	let seen = map.get(session_id).map(|s| &s.ids);
	posts.iter().filter(|post| seen.map_or(true, |ids| !ids.contains(&post.id))).cloned().collect()
}

pub fn record_seen(session_id: &str, posts: &[Post]) {
	if session_id.trim().is_empty() {
		return;
	}
	let mut map = SEEN_BY_SESSION.lock().unwrap_or_else(|e| e.into_inner());
	prune_seen_locked(&mut map);
	let entry = map.entry(session_id.to_string()).or_insert(SeenState {
		ids: HashSet::new(),
		last_touch: Instant::now(),
	});
	entry.ids.extend(posts.iter().map(|p| p.id.clone()));
	entry.last_touch = Instant::now();
}

/// Select a page of posts, optionally skipping posts already seen in this session, and record
/// only the returned posts as seen.
///
/// Returns `(page_posts, has_next)` where `has_next` is based on unseen availability when
/// hide-seen is enabled; otherwise it mirrors plain pagination.
pub fn page_and_record_seen(session_id: &str, hide_seen: bool, posts: &[Post], page: usize, per_page: usize) -> (Vec<Post>, bool) {
	if session_id.trim().is_empty() || per_page == 0 {
		return (Vec::new(), false);
	}

	let mut map = SEEN_BY_SESSION.lock().unwrap_or_else(|e| e.into_inner());
	prune_seen_locked(&mut map);

	// Non-hide mode: plain paging but still mark served IDs.
	if !hide_seen {
		let start = page.saturating_mul(per_page);
		let end = (start + per_page).min(posts.len());
		let page_posts = if start < posts.len() { posts[start..end].to_vec() } else { Vec::new() };
		let has_next = end < posts.len();

		let entry = map.entry(session_id.to_string()).or_insert(SeenState {
			ids: HashSet::new(),
			last_touch: Instant::now(),
		});
		entry.ids.extend(page_posts.iter().map(|p| p.id.clone()));
		entry.last_touch = Instant::now();

		return (page_posts, has_next);
	}

	let seen_ids = map.get(session_id).map(|s| &s.ids);
	let wanted_start = page.saturating_mul(per_page);
	let wanted_end = wanted_start.saturating_add(per_page);

	let mut unseen_index = 0usize;
	let mut page_posts = Vec::new();
	let mut page_ids = Vec::new();
	let mut has_more_unseen = false;

	for post in posts {
		if seen_ids.is_some_and(|ids| ids.contains(&post.id)) {
			continue;
		}
		if unseen_index >= wanted_end {
			has_more_unseen = true;
			break;
		}
		if unseen_index >= wanted_start && page_posts.len() < per_page {
			page_ids.push(post.id.clone());
			page_posts.push(post.clone());
		}
		unseen_index += 1;
	}

	let entry = map.entry(session_id.to_string()).or_insert(SeenState {
		ids: HashSet::new(),
		last_touch: Instant::now(),
	});
	entry.ids.extend(page_ids);
	entry.last_touch = Instant::now();

	(page_posts, has_more_unseen)
}

/// Hide-seen mode for contextual aggregate **page 0** after the listing is sealed: prefer unseen
/// posts in `sealed_posts` order (stable first paint), then fill remaining slots from
/// `merged_posts` in merge order. Records only returned posts as seen.
pub fn page_and_record_seen_contextual_first_page(
	session_id: &str,
	sealed_posts: &[Post],
	merged_posts: &[Post],
	per_page: usize,
) -> (Vec<Post>, bool) {
	if session_id.trim().is_empty() || per_page == 0 {
		return (Vec::new(), false);
	}

	let mut map = SEEN_BY_SESSION.lock().unwrap_or_else(|e| e.into_inner());
	prune_seen_locked(&mut map);
	let seen_before: HashSet<String> = map.get(session_id).map(|s| s.ids.clone()).unwrap_or_default();

	let mut picked_ids: HashSet<String> = HashSet::new();
	let mut page_posts: Vec<Post> = Vec::new();

	for post in sealed_posts {
		if page_posts.len() >= per_page {
			break;
		}
		if seen_before.contains(&post.id) || picked_ids.contains(&post.id) {
			continue;
		}
		picked_ids.insert(post.id.clone());
		page_posts.push(post.clone());
	}

	for post in merged_posts {
		if page_posts.len() >= per_page {
			break;
		}
		if seen_before.contains(&post.id) || picked_ids.contains(&post.id) {
			continue;
		}
		picked_ids.insert(post.id.clone());
		page_posts.push(post.clone());
	}

	let page_ids: Vec<String> = page_posts.iter().map(|p| p.id.clone()).collect();
	let entry = map.entry(session_id.to_string()).or_insert(SeenState {
		ids: HashSet::new(),
		last_touch: Instant::now(),
	});
	entry.ids.extend(page_ids);
	entry.last_touch = Instant::now();
	let seen_after = &entry.ids;
	let has_next = merged_posts.iter().any(|p| !seen_after.contains(&p.id));

	(page_posts, has_next)
}

pub fn ensure_rl_session_id(req: &Request<Body>) -> (String, bool) {
	if let Some(c) = req.cookie(RL_SESSION_COOKIE) {
		let v = c.value().trim();
		if !v.is_empty() && v.len() <= 128 && v.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-') {
			return (v.to_string(), false);
		}
	}
	(uuid::Uuid::new_v4().to_string(), true)
}

/// Default hide-seen is **on** when `listing_scope` is true. Query `hide_seen=on|off` sets cookie override.
pub fn hide_seen_effective_for_request(req: &Request<Body>, listing_scope: bool) -> (bool, Option<String>) {
	if !listing_scope {
		return (false, None);
	}
	let q = req.uri().query().unwrap_or("");
	let q_val = param(&format!("?{q}"), "hide_seen");
	if let Some(ref v) = q_val {
		let lower = v.to_lowercase();
		let effective = !matches!(lower.as_str(), "off" | "false" | "0" | "no");
		let cookie_val = if effective { "on".to_string() } else { "off".to_string() };
		return (effective, Some(cookie_val));
	}
	if let Some(c) = req.cookie(RL_HIDE_SEEN_COOKIE) {
		if c.value() == "off" {
			return (false, None);
		}
	}
	(true, None)
}

pub fn attach_rl_feed_cookies(response: &mut Response<Body>, session_id: &str, set_session_cookie: bool, hide_seen_cookie_val: Option<&str>) {
	let ttl = ttl_duration();
	let exp = OffsetDateTime::now_utc() + ttl;
	if set_session_cookie {
		let cookie = Cookie::build((RL_SESSION_COOKIE.to_string(), session_id.to_string()))
			.path("/")
			.http_only(true)
			.same_site(cookie::SameSite::Lax)
			.expires(exp)
			.into();
		response.insert_cookie(cookie);
	}
	if let Some(val) = hide_seen_cookie_val.filter(|v| *v == "on" || *v == "off") {
		let cookie = Cookie::build((RL_HIDE_SEEN_COOKIE.to_string(), val.to_string()))
			.path("/")
			.http_only(true)
			.same_site(cookie::SameSite::Lax)
			.expires(exp)
			.into();
		response.insert_cookie(cookie);
	}
}

/// Paths for `/`, `/hot`, `/following`, etc. (no `/r/...`).
pub fn public_page_is_feed_seen_scope(path_and_query: &str) -> bool {
	let path = path_and_query.split('?').next().unwrap_or(path_and_query);
	if path == "/" {
		return true;
	}
	if path == "/following" || path.starts_with("/following/") {
		return true;
	}
	if path.starts_with('/') && path.matches('/').count() == 1 {
		let seg = path.trim_start_matches('/');
		return matches!(seg, "best" | "hot" | "new" | "top" | "rising" | "controversial" | "video-length");
	}
	false
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::utils::{Author, Awards, Flags, Flair, Media};

	fn dummy_post(id: &str) -> Post {
		Post {
			id: id.to_string(),
			kind: "t3".to_string(),
			title: String::new(),
			community: "x".to_string(),
			body: String::new(),
			author: Author {
				name: "u".to_string(),
				flair: Flair {
					flair_parts: Vec::new(),
					text: String::new(),
					background_color: String::new(),
					foreground_color: String::new(),
				},
				distinguished: String::new(),
				external_links: Vec::new(),
				avg_video_length: None,
			},
			permalink: String::new(),
			parent_kind: String::new(),
			link_id: String::new(),
			link_title: String::new(),
			link_permalink: String::new(),
			poll: None,
			score: (String::new(), String::new()),
			upvote_ratio: 0,
			post_type: String::new(),
			flair: Flair {
				flair_parts: Vec::new(),
				text: String::new(),
				background_color: String::new(),
				foreground_color: String::new(),
			},
			flags: Flags::new(false, false, false),
			thumbnail: Media::empty(),
			media: Media::empty(),
			domain: String::new(),
			rel_time: String::new(),
			created: String::new(),
			created_ts: 0,
			num_duplicates: 0,
			comments: (String::new(), String::new()),
			gallery: Vec::new(),
			awards: Awards(Vec::new()),
			nsfw: false,
			out_url: None,
			ws_url: String::new(),
			preview_body: String::new(),
			collapsed_duplicates: Vec::new(),
			following_comments: Vec::new(),
		}
	}

	#[test]
	fn filter_hide_seen_skips_then_records() {
		let sid = "test-session";
		let a = vec![dummy_post("a"), dummy_post("b")];
		let out = filter_and_record_seen(sid, true, a);
		assert_eq!(out.len(), 2);
		let out2 = filter_and_record_seen(sid, true, vec![dummy_post("a"), dummy_post("b"), dummy_post("c")]);
		assert_eq!(out2.len(), 1);
		assert_eq!(out2[0].id, "c");
	}

	#[test]
	fn public_paths_match_front_and_following() {
		assert!(public_page_is_feed_seen_scope("/"));
		assert!(public_page_is_feed_seen_scope("/hot"));
		assert!(public_page_is_feed_seen_scope("/following"));
		assert!(public_page_is_feed_seen_scope("/following/new"));
		assert!(!public_page_is_feed_seen_scope("/r/rust"));
	}

	#[test]
	fn contextual_first_page_hide_seen_prefers_sealed_order_then_merge() {
		let sid = "ctx-first-page-sealed-order";
		let sealed = vec![dummy_post("b"), dummy_post("a")];
		let merged = vec![dummy_post("a"), dummy_post("b"), dummy_post("c")];
		let (page, has_next) = page_and_record_seen_contextual_first_page(sid, &sealed, &merged, 2);
		assert_eq!(page.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["b", "a"]);
		assert!(has_next);
	}

	#[test]
	fn contextual_first_page_hide_seen_skips_seen_in_sealed_then_fills_from_merge() {
		let sid = "ctx-first-page-skip-seen";
		record_seen(sid, &[dummy_post("b")]);
		let sealed = vec![dummy_post("b"), dummy_post("a")];
		let merged = vec![dummy_post("a"), dummy_post("b"), dummy_post("c")];
		let (page, _) = page_and_record_seen_contextual_first_page(sid, &sealed, &merged, 2);
		assert_eq!(page.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["a", "c"]);
	}
}
