#![allow(clippy::cmp_owned)]

// CRATES
use crate::client::json;
use crate::server::RequestExt;
use crate::utils::{error, filter_posts, format_num, format_url, get_filters, nsfw_landing, param, setting, template, Post, Preferences, User};
use crate::{config, utils};
use askama::Template;
use chrono::DateTime;
use htmlescape::decode_html;
use hyper::{Body, Request, Response};
use std::collections::HashMap;
use time::{macros::format_description, OffsetDateTime};
use url::Url;

// STRUCTS
#[derive(Template)]
#[template(path = "user.html")]
pub struct UserTemplate {
	user: User,
	posts: Vec<Post>,
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
	}
}

fn display_community(community: &str) -> String {
	if community.starts_with("u_") {
		format!("u/{}", &community[2..])
	} else {
		format!("r/{community}")
	}
}

#[cfg(test)]
mod tests {
	use super::collapse_duplicate_submissions;
	use askama::Template;

	use crate::utils::{Author, Awards, CollapsedDuplicate, Flags, Flair, Media, Post, Preferences};

	fn test_post(id: &str, title: &str, community: &str, body: &str, post_type: &str, media_url: &str, gallery_urls: &[&str]) -> Post {
		Post {
			id: id.to_string(),
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
			},
			permalink: format!("/r/{community}/comments/{id}/"),
			link_title: String::new(),
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
				hls_url: String::new(),
				dash_url: String::new(),
				width: 0,
				height: 0,
				poster: String::new(),
				download_name: String::new(),
				embed_code: String::new(),
			},
			media: Media {
				url: media_url.to_string(),
				hls_url: String::new(),
				dash_url: String::new(),
				width: 0,
				height: 0,
				poster: String::new(),
				download_name: String::new(),
				embed_code: String::new(),
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
			},
			CollapsedDuplicate {
				title: "Other title two".to_string(),
				permalink: "/r/other_two/comments/two/".to_string(),
				community: "other_two".to_string(),
				display_community: "r/other_two".to_string(),
				rel_time: "3h ago".to_string(),
			},
		];

		let html = super::UserTemplate {
			user: crate::utils::User {
				name: "demo_user".to_string(),
				..Default::default()
			},
			posts: vec![post],
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
		assert!(html.contains("Preview text from a duplicate post."));

		println!("{html}");
	}
}

// FUNCTIONS
pub async fn profile(req: Request<Body>) -> Result<Response<Body>, String> {
	let listing = req.param("listing").unwrap_or_else(|| "overview".to_string());

	// Build the Reddit JSON API path
	let path = format!(
		"/user/{}/{listing}.json?{}&raw_json=1",
		req.param("name").unwrap_or_else(|| "reddit".to_string()),
		req.uri().query().unwrap_or_default(),
	);
	let url = String::from(req.uri().path_and_query().map_or("", |val| val.as_str()));
	let redirect_url = url[1..].replace('?', "%3F").replace('&', "%26");

	// Retrieve other variables from Redlib request
	let sort = param(&path, "sort").unwrap_or_default();
	let username = req.param("name").unwrap_or_default();

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
		match Post::fetch(&path, false).await {
			Ok((mut posts, after)) => {
				let (_, all_posts_filtered) = filter_posts(&mut posts, &filters);
				if listing == "submitted" {
					posts = collapse_duplicate_submissions(posts);
				}
				let no_posts = posts.is_empty();
				let all_posts_hidden_nsfw = !no_posts && (posts.iter().all(|p| p.flags.nsfw) && setting(&req, "show_nsfw") != "on");
				Ok(template(&UserTemplate {
					user,
					posts,
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
	json(path, false).await.map(|res| {
		// Grab creation date as unix timestamp
		let created_unix = res["data"]["created"].as_f64().unwrap_or(0.0).round() as i64;
		let created = OffsetDateTime::from_unix_timestamp(created_unix).unwrap_or(OffsetDateTime::UNIX_EPOCH);

		// Closure used to parse JSON from Reddit APIs
		let about = |item| res["data"]["subreddit"][item].as_str().unwrap_or_default().to_string();

		// Parse the JSON output into a User struct
		User {
			name: res["data"]["name"].as_str().unwrap_or(name).to_owned(),
			title: about("title"),
			icon: format_url(&about("icon_img")),
			post_karma: format_num(res["data"]["link_karma"].as_i64().unwrap_or(0)),
			comment_karma: format_num(res["data"]["comment_karma"].as_i64().unwrap_or(0)),
			created: created.format(format_description!("[month repr:short] [day] '[year repr:last_two]")).unwrap_or_default(),
			banner: about("banner_img"),
			description: about("public_description"),
			nsfw: res["data"]["subreddit"]["over_18"].as_bool().unwrap_or_default(),
		}
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
