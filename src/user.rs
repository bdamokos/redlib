#![allow(clippy::cmp_owned)]

// CRATES
use crate::client::json;
use crate::redgifs;
use crate::server::RequestExt;
use crate::utils::{error, filter_posts, format_num, format_url, get_filters, nsfw_landing, param, setting, template, Post, Preferences, User};
use crate::{config, utils};
use askama::Template;
use chrono::DateTime;
use htmlescape::decode_html;
use hyper::{Body, Request, Response};
use std::collections::{HashMap, HashSet};
use time::{macros::format_description, OffsetDateTime};
use tokio::task::JoinSet;
use url::form_urlencoded;
use url::Url;

const TARGET_POST_COUNT: usize = 25;
const MAX_DUPLICATE_FILL_REQUESTS: usize = 4;
const USER_SORT_OPTIONS: [&str; 4] = ["hot", "new", "top", "controversial"];

// STRUCTS
#[derive(Template)]
#[template(path = "user.html")]
pub struct UserTemplate {
	user: User,
	posts: Vec<Post>,
	user_subreddits: Vec<UserSubredditEntry>,
	sort: (String, String),
	ends: (String, String),
	/// "overview", "comments", or "submitted"
	listing: String,
	prefs: Preferences,
	url: String,
	redirect_url: String,
	/// Whether the user themself is filtered.
	is_filtered: bool,
	/// Whether all fetched posts are filtered (to differentiate between no posts fetched in the first place,
	/// and all fetched posts being filtered).
	all_posts_filtered: bool,
	/// Whether all posts were hidden because they are NSFW (and user has disabled show NSFW)
	all_posts_hidden_nsfw: bool,
	no_posts: bool,
}

crate::impl_template_str_eq!(UserTemplate);

#[derive(PartialEq)]
struct UserSubredditEntry {
	name: String,
	count: u32,
}

fn collapse_duplicate_submissions(posts: Vec<Post>) -> Vec<Post> {
	let mut collapsed: Vec<Post> = Vec::new();
	let mut seen = HashMap::new();

	for post in posts {
		let Some(key) = duplicate_submission_key(&post) else {
			collapsed.push(post);
			continue;
		};

		if let Some(index) = seen.get(&key).copied() {
			let primary: &mut Post = &mut collapsed[index];
			if primary.body.is_empty() && primary.preview_body.is_empty() && !post.body.is_empty() {
				primary.preview_body = post.body.clone();
			}
			primary.collapsed_duplicates.push(collapsed_duplicate(post));
			continue;
		}

		seen.insert(key, collapsed.len());
		collapsed.push(post);
	}

	collapsed
}

fn duplicate_submission_key(post: &Post) -> Option<String> {
	match post.post_type.as_str() {
		"gallery" if !post.gallery.is_empty() => Some(format!(
			"gallery:{}",
			post.gallery.iter().map(|image| normalize_duplicate_url(&image.url)).collect::<Vec<_>>().join("|")
		)),
		"image" if !post.media.url.is_empty() => Some(format!("image:{}", normalize_duplicate_url(&post.media.url))),
		"gif" => duplicate_gif_key(post),
		"video" if redgifs::is_redgifs_domain(&post.domain) => duplicate_gif_key(post),
		_ => None,
	}
}

fn duplicate_gif_key(post: &Post) -> Option<String> {
	if let Some(embed_src) = extract_embed_src(&post.media.embed_code) {
		return Some(format!("gif:{}", normalize_duplicate_url(&embed_src)));
	}

	if !post.media.url.is_empty() {
		return Some(format!("gif:{}", normalize_duplicate_url(&post.media.url)));
	}

	None
}

fn extract_embed_src(embed_code: &str) -> Option<String> {
	for delimiter in ['"', '\''] {
		let needle = format!("src={delimiter}");
		let Some(start) = embed_code.find(&needle) else {
			continue;
		};

		let remainder = &embed_code[start + needle.len()..];
		let Some((src, _)) = remainder.split_once(delimiter) else {
			continue;
		};

		if !src.trim().is_empty() {
			return Some(src.trim().to_string());
		}
	}

	None
}

fn normalize_duplicate_url(url: &str) -> String {
	let trimmed = url.trim();
	if trimmed.is_empty() {
		return String::new();
	}

	if trimmed.starts_with('/') {
		return trimmed.split('?').next().unwrap_or(trimmed).trim_end_matches('/').to_string();
	}

	let Ok(mut parsed) = Url::parse(trimmed) else {
		return trimmed.trim_end_matches('/').to_string();
	};

	parsed.set_fragment(None);
	if matches!(parsed.domain(), Some("i.redd.it" | "preview.redd.it" | "external-preview.redd.it" | "v.redd.it")) {
		parsed.set_query(None);
	}

	let path = parsed.path().trim_end_matches('/').to_string();
	if !path.is_empty() {
		parsed.set_path(&path);
	}

	parsed.into()
}

fn collapsed_duplicate(post: Post) -> crate::utils::CollapsedDuplicate {
	crate::utils::CollapsedDuplicate {
		title: post.title,
		permalink: post.permalink,
		community: post.community.clone(),
		display_community: display_community(&post.community),
		rel_time: post.rel_time,
		comments: post.comments,
		flair: post.flair,
	}
}

fn display_community(community: &str) -> String {
	if community.starts_with("u_") {
		format!("u/{}", &community[2..])
	} else {
		format!("r/{community}")
	}
}

fn path_with_after(path: &str, after: &str) -> Result<String, String> {
	let mut url = Url::parse(&format!("https://redlib.local{path}")).map_err(|err| err.to_string())?;
	let mut query_pairs: Vec<(String, String)> = url
		.query_pairs()
		.filter(|(key, _)| key != "after")
		.map(|(key, value)| (key.into_owned(), value.into_owned()))
		.collect();

	if !after.is_empty() {
		query_pairs.push(("after".to_string(), after.to_string()));
	}

	url.set_query(None);
	{
		let mut serializer = url.query_pairs_mut();
		for (key, value) in query_pairs {
			serializer.append_pair(&key, &value);
		}
	}

	Ok(url[url::Position::BeforePath..].to_string())
}

fn default_user_sort(req: &Request<Body>) -> String {
	let default_sort = setting(req, "post_sort");
	if USER_SORT_OPTIONS.contains(&default_sort.as_str()) {
		default_sort
	} else {
		"hot".to_string()
	}
}

fn user_listing_path(username: &str, listing: &str, query: &str, default_sort: &str) -> String {
	let mut query_pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();

	if !query_pairs.iter().any(|(key, _)| key == "sort") {
		query_pairs.push(("sort".to_string(), default_sort.to_string()));
	}

	query_pairs.push(("raw_json".to_string(), "1".to_string()));

	let query = form_urlencoded::Serializer::new(String::new())
		.extend_pairs(query_pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())))
		.finish();

	format!("/user/{username}/{listing}.json?{query}")
}

fn user_reddit_path(username: &str, listing: &str, query: &str) -> String {
	let base = if listing == "overview" {
		format!("/user/{username}/")
	} else {
		format!("/user/{username}/{listing}")
	};

	if query.is_empty() {
		base
	} else {
		format!("{base}?{query}")
	}
}

async fn fetch_user_listing_posts(username: &str, path: &str, listing: &str, filters: &HashSet<String>) -> Result<(Vec<Post>, String, bool), String> {
	let should_refill_duplicates = listing == "submitted" || listing == "overview";
	let mut request_path = path.to_string();
	let mut all_posts = Vec::new();
	let mut fetched_raw_posts = false;
	let mut requests = 0;

	let after = loop {
		let (mut posts, next_after) = Post::fetch(&request_path, false).await?;
		fetched_raw_posts |= !posts.is_empty();
		utils::store_post_flairs(&posts);
		utils::store_user_subreddits(username, &posts);

		filter_posts(&mut posts, filters);
		all_posts.extend(posts);

		if should_refill_duplicates {
			all_posts = collapse_duplicate_submissions(all_posts);
		}

		let should_continue = should_refill_duplicates && all_posts.len() < TARGET_POST_COUNT && !next_after.is_empty() && requests < MAX_DUPLICATE_FILL_REQUESTS;
		if !should_continue {
			break next_after;
		}

		request_path = path_with_after(path, &next_after)?;
		requests += 1;
	};

	utils::hydrate_post_author_links_from_cache(&mut all_posts);
	let all_posts_filtered = fetched_raw_posts && all_posts.is_empty();
	Ok((all_posts, after, all_posts_filtered))
}

#[cfg(test)]
mod tests {
	use super::collapse_duplicate_submissions;
	use askama::Template;

	use crate::utils::{Author, Awards, CollapsedDuplicate, Flags, Flair, FlairPart, Media, Post, Preferences};

	fn test_post(id: &str, title: &str, community: &str, body: &str, post_type: &str, media_url: &str, gallery_urls: &[&str]) -> Post {
		Post {
			id: id.to_string(),
			kind: "t3".to_string(),
			title: title.to_string(),
			community: community.to_string(),
			body: body.to_string(),
			author: Author {
				name: "SilkySavannah".to_string(),
				flair: Flair {
					flair_parts: Vec::new(),
					text: String::new(),
					background_color: String::new(),
					foreground_color: String::new(),
				},
				distinguished: String::new(),
				external_links: Vec::new(),
			},
			permalink: format!("/r/{community}/comments/{id}/"),
			parent_kind: String::new(),
			link_title: String::new(),
			link_permalink: String::new(),
			poll: None,
			score: ("1".to_string(), "1".to_string()),
			upvote_ratio: 100,
			post_type: post_type.to_string(),
			flair: Flair {
				flair_parts: Vec::new(),
				text: String::new(),
				background_color: String::new(),
				foreground_color: String::new(),
			},
			flags: Flags::new(false, false, false),
			thumbnail: Media {
				url: String::new(),
				..Media::empty()
			},
			media: Media {
				url: media_url.to_string(),
				..Media::empty()
			},
			domain: "i.redd.it".to_string(),
			rel_time: "1h ago".to_string(),
			created: String::new(),
			created_ts: 0,
			num_duplicates: 0,
			comments: ("0".to_string(), "0".to_string()),
			gallery: gallery_urls
				.iter()
				.map(|url| crate::utils::GalleryMedia {
					url: (*url).to_string(),
					preview_url: String::new(),
					preview_srcset: String::new(),
					width: 0,
					height: 0,
					caption: String::new(),
					outbound_url: String::new(),
				})
				.collect(),
			awards: Awards(Vec::new()),
			nsfw: false,
			out_url: None,
			ws_url: String::new(),
			preview_body: String::new(),
			collapsed_duplicates: Vec::new(),
		}
	}

	fn test_prefs() -> Preferences {
		Preferences {
			available_themes: vec!["system".to_string()],
			layout: "card".to_string(),
			show_nsfw: "on".to_string(),
			fixed_navbar: "on".to_string(),
			disable_visit_reddit_confirmation: "on".to_string(),
			..Preferences::default()
		}
	}

	fn test_embed_gif_post(id: &str, title: &str, community: &str, embed_src: &str) -> Post {
		let mut post = test_post(id, title, community, "", "gif", "", &[]);
		post.media.embed_code = format!("<iframe src=\"{embed_src}\" frameborder=\"0\"></iframe>");
		post
	}

	fn test_comment(id: &str, community: &str, body: &str, parent_kind: &str, link_title: &str) -> Post {
		let mut post = test_post(id, String::new().as_str(), community, body, "comment", "", &[]);
		post.kind = "t1".to_string();
		post.parent_kind = parent_kind.to_string();
		post.link_title = link_title.to_string();
		post.link_permalink = format!("/r/{community}/comments/thread/");
		post.permalink = format!("/r/{community}/comments/thread/{id}/");
		post.comments = ("32".to_string(), "32".to_string());
		post
	}

	#[test]
	fn collapses_matching_image_posts() {
		let posts = vec![
			test_post("one", "First title", "sub_one", "", "image", "https://i.redd.it/example.jpg?width=2048", &[]),
			test_post("two", "Second title", "sub_two", "", "image", "https://i.redd.it/example.jpg?width=1024", &[]),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].title, "First title");
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "Second title");
		assert_eq!(collapsed[0].collapsed_duplicates[0].display_community, "r/sub_two");
	}

	#[test]
	fn uses_duplicate_text_as_preview_when_primary_has_none() {
		let posts = vec![
			test_post("one", "Primary", "sub_one", "", "image", "https://i.redd.it/example.jpg", &[]),
			test_post(
				"two",
				"Secondary",
				"sub_two",
				"<div class=\"md\"><p>caption</p></div>",
				"image",
				"https://i.redd.it/example.jpg",
				&[],
			),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].preview_body, "<div class=\"md\"><p>caption</p></div>");
	}

	#[test]
	fn collapses_matching_galleries_using_normalized_urls() {
		let posts = vec![
			test_post(
				"one",
				"Gallery one",
				"sub_one",
				"",
				"gallery",
				"",
				&["/preview/pre/a.jpg?width=300", "/preview/pre/b.jpg?width=300"],
			),
			test_post(
				"two",
				"Gallery two",
				"sub_two",
				"",
				"gallery",
				"",
				&["/preview/pre/a.jpg?width=1200", "/preview/pre/b.jpg?width=1200"],
			),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
	}

	#[test]
	fn collapses_matching_gif_posts() {
		let posts = vec![
			test_post("one", "GIF one", "sub_one", "", "gif", "https://v.redd.it/example/DASH_720.mp4?source=fallback", &[]),
			test_post("two", "GIF two", "sub_two", "", "gif", "https://v.redd.it/example/DASH_720.mp4?foo=bar", &[]),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "GIF two");
	}

	#[test]
	fn collapses_matching_embedded_gif_posts() {
		let posts = vec![
			test_embed_gif_post("one", "Embed one", "sub_one", "https://www.redgifs.com/ifr/mediumpurplesoggyconure/"),
			test_embed_gif_post("two", "Embed two", "sub_two", "https://www.redgifs.com/ifr/mediumpurplesoggyconure"),
		];

		let collapsed = collapse_duplicate_submissions(posts);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "Embed two");
	}

	#[test]
	fn collapses_matching_redgifs_video_posts() {
		let mut first = test_post("one", "Video one", "sub_one", "", "video", "/redgifs/zanyjudiciousrook", &[]);
		first.domain = "www.redgifs.com".to_string();

		let mut second = test_post("two", "Video two", "sub_two", "", "video", "/redgifs/zanyjudiciousrook/", &[]);
		second.domain = "redgifs.com".to_string();

		let collapsed = collapse_duplicate_submissions(vec![first, second]);

		assert_eq!(collapsed.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates.len(), 1);
		assert_eq!(collapsed[0].collapsed_duplicates[0].title, "Video two");
	}

	#[test]
	fn renders_collapsed_duplicates_in_user_template() {
		let mut post = test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[]);
		post.preview_body = "<div class=\"md\"><p>Preview text from a duplicate post.</p></div>".to_string();
		post.collapsed_duplicates = vec![
			CollapsedDuplicate {
				title: "Other title one".to_string(),
				permalink: "/r/other_one/comments/one/".to_string(),
				community: "other_one".to_string(),
				display_community: "r/other_one".to_string(),
				rel_time: "2h ago".to_string(),
				comments: ("12".to_string(), "12".to_string()),
				flair: Flair {
					flair_parts: vec![FlairPart {
						flair_part_type: "text".to_string(),
						value: "Question".to_string(),
					}],
					text: "Question".to_string(),
					background_color: "#eeeeee".to_string(),
					foreground_color: "#111111".to_string(),
				},
			},
			CollapsedDuplicate {
				title: "Other title two".to_string(),
				permalink: "/r/other_two/comments/two/".to_string(),
				community: "other_two".to_string(),
				display_community: "r/other_two".to_string(),
				rel_time: "3h ago".to_string(),
				comments: ("1".to_string(), "1".to_string()),
				flair: Flair {
					flair_parts: Vec::new(),
					text: String::new(),
					background_color: String::new(),
					foreground_color: String::new(),
				},
			},
		];

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			posts: vec![post],
			user_subreddits: Vec::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "submitted".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user/submitted".to_string(),
			redirect_url: "user/demo_user/submitted".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}
		.render()
		.unwrap();

		assert!(html.contains("submitted_duplicates.js"));
		assert!(html.contains("Also posted to 2 other subreddits"));
		assert!(html.contains("Other title one"));
		assert!(html.contains("Other title two"));
		assert!(html.contains("12 comments"));
		assert!(html.contains("1 comment"));
		assert!(html.contains("Preview text from a duplicate post."));

		assert!(html.contains("/r/other_one/search?q=flair_name%3A%22Question%22&restrict_sr=on"));
		assert!(html.contains("/r/other_one/comments/one/\">Other title one</a>"));
		assert!(html.contains("/r/other_one\">r/other_one</a>"));
		assert!(html.contains("dir=\"ltr\"><span>Question</span></a>"));

		println!("{html}");
	}

	#[test]
	fn renders_duplicate_script_in_overview_listing() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			posts: vec![test_post("demo", "Primary post title", "demo_sub", "", "image", "https://i.redd.it/example.jpg", &[])],
			user_subreddits: Vec::new(),
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}
		.render()
		.unwrap();

		assert!(html.contains("submitted_duplicates.js"));
	}

	#[test]
	fn user_listing_path_uses_default_sort_when_missing() {
		assert_eq!(
			super::user_listing_path("demo_user", "overview", "t=week", "top"),
			"/user/demo_user/overview.json?t=week&sort=top&raw_json=1"
		);
	}

	#[test]
	fn user_listing_path_preserves_explicit_sort() {
		assert_eq!(
			super::user_listing_path("demo_user", "submitted", "sort=new&t=day", "top"),
			"/user/demo_user/submitted.json?sort=new&t=day&raw_json=1"
		);
	}

	#[test]
	fn user_reddit_path_uses_profile_root_for_overview() {
		assert_eq!(super::user_reddit_path("demo_user", "overview", ""), "/user/demo_user/");
		assert_eq!(super::user_reddit_path("demo_user", "overview", "sort=new"), "/user/demo_user/?sort=new");
	}

	#[test]
	fn user_reddit_path_keeps_non_overview_listings() {
		assert_eq!(super::user_reddit_path("demo_user", "comments", ""), "/user/demo_user/comments");
		assert_eq!(
			super::user_reddit_path("demo_user", "submitted", "sort=top&t=week"),
			"/user/demo_user/submitted?sort=top&t=week"
		);
	}

	#[test]
	fn renders_comment_entries_without_link_thumbnail() {
		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			posts: vec![test_comment(
				"comment_one",
				"demo_sub",
				"<div class=\"md\"><p>This is a reply.</p></div>",
				"t1",
				"Thread title",
			)],
			user_subreddits: vec![
				super::UserSubredditEntry {
					name: "demo_sub".to_string(),
					count: 2,
				},
				super::UserSubredditEntry {
					name: "other_sub".to_string(),
					count: 1,
				},
			],
			sort: (String::new(), String::new()),
			ends: (String::new(), String::new()),
			listing: "overview".to_string(),
			prefs: test_prefs(),
			url: "/user/demo_user".to_string(),
			redirect_url: "user/demo_user".to_string(),
			is_filtered: false,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}
		.render()
		.unwrap();

		assert!(html.contains("Reply to comment in"));
		assert!(html.contains("Thread title"));
		assert!(html.contains("View thread"));
		assert!(!html.contains("post_thumbnail"));
		assert!(html.contains("<a class=\"user_subreddit_link\" href=\"/r/demo_sub\">r/demo_sub</a>"));
		assert!(html.contains("<span class=\"user_subreddit_meta\">2</span>"));
		assert!(html.contains("<a class=\"user_subreddit_link\" href=\"/r/other_sub\">r/other_sub</a>"));
		assert!(html.contains("<span class=\"user_subreddit_meta\">1</span>"));
	}
}

// FUNCTIONS
pub async fn profile(req: Request<Body>) -> Result<Response<Body>, String> {
	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());
	let username = req.param("name").unwrap_or_else(|| "reddit".to_string());
	let default_sort = default_user_sort(&req);
	let query = req.uri().query().unwrap_or_default();

	// Build the Reddit JSON API path
	let path = user_listing_path(&username, &listing, query, &default_sort);
	let url = user_reddit_path(&username, &listing, query);
	let current_url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let redirect_url = current_url[1..].replace('?', "%3F").replace('&', "%26");

	// Retrieve other variables from Redlib request
	let sort = param(&path, "sort").unwrap_or_default();

	// Retrieve info from user about page.
	let user = user(&username).await.unwrap_or_default();

	let req_url = req.uri().to_string();
	// Return landing page if this post if this Reddit deems this user NSFW,
	// but we have also disabled the display of NSFW content or if the instance
	// is SFW-only.
	if user.nsfw && crate::utils::should_be_nsfw_gated(&req, &req_url) {
		return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
	}

	let filters = get_filters(&req);
	if filters.contains(&["u_", &username].concat()) {
		Ok(template(&UserTemplate {
			user,
			posts: Vec::new(),
			user_subreddits: utils::cached_user_subreddits(&username)
				.into_iter()
				.map(|(name, count)| UserSubredditEntry { name, count })
				.collect(),
			sort: (sort, param(&path, "t").unwrap_or_default()),
			ends: (param(&path, "after").unwrap_or_default(), String::new()),
			listing,
			prefs: Preferences::new(&req),
			url,
			redirect_url,
			is_filtered: true,
			all_posts_filtered: false,
			all_posts_hidden_nsfw: false,
			no_posts: false,
		}))
	} else {
		// Request user posts/comments from Reddit
		match fetch_user_listing_posts(&username, &path, &listing, &filters).await {
			Ok((posts, after, all_posts_filtered)) => {
				let no_posts = posts.is_empty();
				let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
				Ok(template(&UserTemplate {
					user,
					posts,
					user_subreddits: utils::cached_user_subreddits(&username)
						.into_iter()
						.map(|(name, count)| UserSubredditEntry { name, count })
						.collect(),
					sort: (sort, param(&path, "t").unwrap_or_default()),
					ends: (param(&path, "after").unwrap_or_default(), after),
					listing,
					prefs: Preferences::new(&req),
					url,
					redirect_url,
					is_filtered: false,
					all_posts_filtered,
					all_posts_hidden_nsfw,
					no_posts,
				}))
			}
			// If there is an error show error page
			Err(msg) => error(req, &msg).await,
		}
	}
}

// USER
async fn user(name: &str) -> Result<User, String> {
	// Build the Reddit JSON API path
	let path: String = format!("/user/{name}/about.json?raw_json=1");

	// Send a request to the url
	let res = json(path, false).await?;

	// Grab creation date as unix timestamp
	let created_unix = res["data"]["created"].as_f64().unwrap_or(0.0).round() as i64;
	let created = OffsetDateTime::from_unix_timestamp(created_unix).unwrap_or(OffsetDateTime::UNIX_EPOCH);
	let profile = utils::user_profile_details_with_social_links(name, &res["data"]["subreddit"]).await;

	// Closure used to parse JSON from Reddit APIs
	let about = |item| res["data"]["subreddit"][item].as_str().unwrap_or_default().to_string();

	// Parse the JSON output into a User struct
	Ok(User {
		name: res["data"]["name"].as_str().unwrap_or(name).to_owned(),
		title: about("title"),
		icon: format_url(&about("icon_img")),
		post_karma: format_num(res["data"]["link_karma"].as_i64().unwrap_or(0)),
		comment_karma: format_num(res["data"]["comment_karma"].as_i64().unwrap_or(0)),
		created: created.format(format_description!("[month repr:short] [day] '[year repr:last_two]")).unwrap_or_default(),
		banner: about("banner_img"),
		description: profile.blurb,
		bio_html: profile.bio_html,
		external_links: utils::external_links_with_coomer_lookups(profile.external_links).await,
		nsfw: res["data"]["subreddit"]["over_18"].as_bool().unwrap_or_default(),
	})
}

pub async fn rss(req: Request<Body>) -> Result<Response<Body>, String> {
	if config::get_setting("REDLIB_ENABLE_RSS").is_none() {
		return Ok(error(req, "RSS is disabled on this instance.").await.unwrap_or_default());
	}
	use crate::utils::rewrite_urls;
	use hyper::header::CONTENT_TYPE;
	use rss::{ChannelBuilder, Item};

	// Get user
	let user_str = req.param("name").unwrap_or_default();

	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());

	// Get path
	let path = format!("/user/{user_str}/{listing}.json?{}&raw_json=1", req.uri().query().unwrap_or_default(),);

	// Get user
	let user_obj = user(&user_str).await.unwrap_or_default();

	// Get posts
	let (posts, _) = Post::fetch(&path, false).await?;

	// Build the RSS feed
	let channel = ChannelBuilder::default()
		.title(user_str)
		.description(user_obj.description)
		.items(
			posts
				.into_iter()
				.map(|post| Item {
					title: Some(post.title.to_string()),
					link: Some(format_url(&utils::get_post_url(&post))),
					author: Some(post.author.name),
					pub_date: Some(DateTime::from_timestamp(post.created_ts as i64, 0).unwrap_or_default().to_rfc2822()),
					content: Some(rewrite_urls(&decode_html(&post.body).unwrap_or_else(|_| post.body.clone()))),
					..Default::default()
				})
				.collect::<Vec<_>>(),
		)
		.build();

	// Serialize the feed to RSS
	let body = channel.to_string().into_bytes();

	// Create the HTTP response
	let mut res = Response::new(Body::from(body));
	res.headers_mut().insert(CONTENT_TYPE, hyper::header::HeaderValue::from_static("application/rss+xml"));

	Ok(res)
}

pub async fn author_links(req: Request<Body>) -> Result<Response<Body>, String> {
	let names = param(&format!("?{}", req.uri().query().unwrap_or_default()), "names").unwrap_or_default();
	let authors = names
		.split(',')
		.map(str::trim)
		.filter(|name| !name.is_empty() && *name != "[deleted]")
		.take(64)
		.collect::<Vec<_>>();

	let mut pending = JoinSet::new();
	let mut seen = HashSet::new();

	for author in authors {
		if !seen.insert(author.to_string()) {
			continue;
		}

		let author_name = author.to_string();
		pending.spawn(async move {
			let links = utils::external_links_with_coomer_flags(utils::author_external_links(&author_name).await).await;
			(author_name, links)
		});
	}

	let mut links_by_author = HashMap::new();
	while let Some(result) = pending.join_next().await {
		if let Ok((author, links)) = result {
			links_by_author.insert(author, links);
		}
	}

	let body = serde_json::to_string(&links_by_author).map_err(|err| err.to_string())?;
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/json")
			.header("Cache-Control", "no-store")
			.body(body.into())
			.unwrap_or_default(),
	)
}
