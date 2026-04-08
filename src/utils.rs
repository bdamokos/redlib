#![allow(dead_code)]
#![allow(clippy::cmp_owned)]

use crate::config::{self, get_setting};
//
// CRATES
//
use crate::{
	client::{json, latest_ratelimit_snapshot, CLIENT, OAUTH_CLIENT},
	redgifs,
	server::{RequestExt, ResponseExt},
};
use askama::Template;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use cached::proc_macro::cached;
use cookie::Cookie;
use htmlescape::decode_html;
use hyper::body::to_bytes;
use hyper::header::LOCATION;
use hyper::{Body, Request, Response, Uri};
use libflate::deflate::{Decoder, Encoder};
use log::error;
use percent_encoding::percent_decode_str;
use regex::Regex;
use revision::{revisioned, Revisioned};
use rust_embed::RustEmbed;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use serde_json_path::{JsonPath, JsonPathExt};
use std::collections::{HashMap, HashSet};
use std::env;
use std::future::Future;
use std::io::{Read, Write};
use std::str::FromStr;
use std::string::ToString;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration as StdDuration, Instant};
use time::{macros::format_description, Duration, OffsetDateTime};
use tokio::task::JoinSet;
use url::Url;

/// Write a message to stderr on debug mode. This function is a no-op on
/// release code.
#[macro_export]
macro_rules! dbg_msg {
	($x:expr) => {
		#[cfg(debug_assertions)]
		eprintln!("{}:{}: {}", file!(), line!(), $x.to_string())
	};

	($($x:expr),+) => {
		#[cfg(debug_assertions)]
		dbg_msg!(format!($($x),+))
	};
}

/// Identifies whether or not the page is a subreddit, a user page, or a post.
/// This is used by the NSFW landing template to determine the mesage to convey
/// to the user.
#[derive(PartialEq, Eq)]
pub enum ResourceType {
	Subreddit,
	User,
	Post,
}

impl ResourceType {
	pub fn as_str(&self) -> &str {
		match self {
			ResourceType::Subreddit => "Subreddit",
			ResourceType::User => "User",
			ResourceType::Post => "Post",
		}
	}
}

/// Post flair with content, background color and foreground color
#[derive(Clone, Serialize, PartialEq)]
pub struct Flair {
	pub flair_parts: Vec<FlairPart>,
	pub text: String,
	pub background_color: String,
	pub foreground_color: String,
}

/// Part of flair, either emoji or text
#[derive(Clone, Serialize, PartialEq)]
pub struct FlairPart {
	pub flair_part_type: String,
	pub value: String,
}

#[derive(Default, Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[revisioned(revision = 1)]
pub struct FlairSubscription {
	pub subreddit: String,
	pub flair: String,
}

impl FlairPart {
	pub fn parse(flair_type: &str, rich_flair: Option<&Vec<Value>>, text_flair: Option<&str>) -> Vec<Self> {
		// Parse type of flair
		match flair_type {
			// If flair contains emojis and text
			"richtext" => match rich_flair {
				Some(rich) => rich
					.iter()
					// For each part of the flair, extract text and emojis
					.map(|part| {
						let value = |name: &str| part[name].as_str().unwrap_or_default();
						Self {
							flair_part_type: value("e").to_string(),
							value: match value("e") {
								"text" => value("t").to_string(),
								"emoji" => format_url(value("u")),
								_ => String::new(),
							},
						}
					})
					.collect::<Vec<Self>>(),
				None => Vec::new(),
			},
			// If flair contains only text
			"text" => match text_flair {
				Some(text) => vec![Self {
					flair_part_type: "text".to_string(),
					value: text.to_string(),
				}],
				None => Vec::new(),
			},
			_ => Vec::new(),
		}
	}
}

#[derive(Clone, Serialize, PartialEq)]
pub struct Author {
	pub name: String,
	pub flair: Flair,
	pub distinguished: String,
	pub external_links: Vec<ExternalLink>,
	pub avg_video_length: Option<(String, String)>,
}

#[derive(Default, Clone, Serialize, PartialEq, Eq)]
pub struct ExternalLink {
	pub label: String,
	pub url: String,
	pub host: String,
	pub icon_path: String,
	#[serde(default)]
	pub has_coomer: bool,
}

#[derive(Default, Clone, PartialEq, Eq)]
pub struct UserProfileDetails {
	pub blurb: String,
	pub bio_html: String,
	pub external_links: Vec<ExternalLink>,
}

#[derive(Deserialize)]
struct RedditSocialLinkContext {
	social_link: RedditSocialLink,
}

#[derive(Deserialize)]
struct RedditSocialLink {
	url: String,
	name: Option<String>,
	position: Option<usize>,
}

#[derive(Clone)]
struct CachedAuthorLinks {
	links: Vec<ExternalLink>,
	cached_at: Instant,
}

#[derive(Clone)]
struct CachedSubredditFlairs {
	flairs: HashMap<String, Flair>,
	cached_at: Instant,
}

#[derive(Clone)]
struct CachedUserSubreddits {
	subreddits: HashMap<String, u32>,
	cached_at: Instant,
}

#[derive(Clone)]
struct CachedSubredditUsers {
	users: HashMap<String, u32>,
	cached_at: Instant,
}

#[derive(Clone, Serialize, PartialEq)]
pub struct Poll {
	pub poll_options: Vec<PollOption>,
	pub voting_end_timestamp: (String, String),
	pub total_vote_count: u64,
}

impl Poll {
	pub fn parse(poll_data: &Value) -> Option<Self> {
		poll_data.as_object()?;

		let total_vote_count = poll_data["total_vote_count"].as_u64()?;
		// voting_end_timestamp is in the format of milliseconds
		let voting_end_timestamp = time(poll_data["voting_end_timestamp"].as_f64()? / 1000.0);
		let poll_options = PollOption::parse(&poll_data["options"])?;

		Some(Self {
			poll_options,
			voting_end_timestamp,
			total_vote_count,
		})
	}

	pub fn most_votes(&self) -> u64 {
		self.poll_options.iter().filter_map(|o| o.vote_count).max().unwrap_or(0)
	}
}

#[derive(Clone, Serialize, PartialEq)]
pub struct PollOption {
	pub id: u64,
	pub text: String,
	pub vote_count: Option<u64>,
}

impl PollOption {
	pub fn parse(options: &Value) -> Option<Vec<Self>> {
		Some(
			options
				.as_array()?
				.iter()
				.filter_map(|option| {
					// For each poll option

					// we can't just use as_u64() because "id": String("...") and serde would parse it as None
					let id = option["id"].as_str()?.parse::<u64>().ok()?;
					let text = option["text"].as_str()?.to_owned();
					let vote_count = option["vote_count"].as_u64();

					// Construct PollOption items
					Some(Self { id, text, vote_count })
				})
				.collect::<Vec<Self>>(),
		)
	}
}

/// Post flags with NSFW and stickied
#[derive(Clone, Serialize, PartialEq)]
pub struct Flags {
	pub spoiler: bool,
	pub nsfw: bool,
	pub stickied: bool,
	#[serde(skip)]
	pub nsfw_str: &'static str,
	#[serde(skip)]
	pub spoiler_str: &'static str,
	#[serde(skip)]
	pub stickied_str: &'static str,
}

impl Flags {
	pub fn new(spoiler: bool, nsfw: bool, stickied: bool) -> Self {
		Self {
			spoiler,
			nsfw,
			stickied,
			nsfw_str: if nsfw { "true" } else { "false" },
			spoiler_str: if spoiler { "true" } else { "false" },
			stickied_str: if stickied { "true" } else { "false" },
		}
	}
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Media {
	pub url: String,
	pub preview_url: String,
	pub preview_srcset: String,
	pub hls_url: String,
	pub dash_url: String,
	pub width: i64,
	pub height: i64,
	pub poster: String,
	pub duration_seconds: i64,
	pub duration: String,
	pub download_name: String,
	pub embed_code: String,
	pub video_preview_enabled: bool,
	pub video_preview_job_hint: String,
}

impl Media {
	pub fn empty() -> Self {
		Self {
			url: String::new(),
			preview_url: String::new(),
			preview_srcset: String::new(),
			hls_url: String::new(),
			dash_url: String::new(),
			width: 0,
			height: 0,
			poster: String::new(),
			duration_seconds: 0,
			duration: String::new(),
			download_name: String::new(),
			embed_code: String::new(),
			video_preview_enabled: false,
			video_preview_job_hint: String::new(),
		}
	}

	pub async fn parse(data: &Value) -> (String, Self, Vec<GalleryMedia>) {
		let mut gallery = Vec::new();

		// Define the various known places that Reddit might put video URLs.
		let data_preview = &data["preview"]["reddit_video_preview"];
		let secure_media = &data["secure_media"]["reddit_video"];
		let crosspost_parent_media = &data["crosspost_parent_list"][0]["secure_media"]["reddit_video"];
		let raw_embed_code = data["secure_media_embed"]["content"].as_str().unwrap_or_default().to_string();
		let redgifs_id = redgifs::watch_path_from_url(data["url"].as_str().unwrap_or_default()).or_else(|| redgifs::watch_path_from_embed_code(&raw_embed_code));
		let redgifs_poster = if let Some(video_id) = redgifs_id.as_ref() {
			redgifs::fetch_poster_path(video_id).await.unwrap_or_default()
		} else {
			String::new()
		};
		let redgifs_duration_seconds = if let Some(video_id) = redgifs_id.as_ref() {
			redgifs::fetch_duration_seconds(video_id).await.unwrap_or_default()
		} else {
			0
		};
		let redgifs_preview_media_url = if let Some(video_id) = redgifs_id.as_ref() {
			redgifs::fetch_video_path(video_id)
				.await
				.ok()
				.and_then(|path| redgifs::direct_media_url_from_path(&path))
				.unwrap_or_default()
		} else {
			String::new()
		};
		let redgifs_url = redgifs_id.as_ref().map(|id| Value::String(format!("https://www.redgifs.com/watch/{id}")));

		let embed_code = if redgifs_url.is_some() { String::new() } else { raw_embed_code };

		// RedGifs previews often point at muted Reddit transcodes; use the direct RedGifs MP4 instead.
		let (post_type, url_val, hls_url_val, dash_url_val) = if let Some(redgifs_url) = redgifs_url.as_ref() {
			("video", redgifs_url, None, None)
		} else if data_preview["fallback_url"].is_string() {
			(
				if data_preview["is_gif"].as_bool().unwrap_or(false) { "gif" } else { "video" },
				&data_preview["fallback_url"],
				Some(&data_preview["hls_url"]),
				Some(&data_preview["dash_url"]),
			)
		} else if secure_media["fallback_url"].is_string() {
			(
				if secure_media["is_gif"].as_bool().unwrap_or(false) { "gif" } else { "video" },
				&secure_media["fallback_url"],
				Some(&secure_media["hls_url"]),
				Some(&secure_media["dash_url"]),
			)
		} else if crosspost_parent_media["fallback_url"].is_string() {
			(
				if crosspost_parent_media["is_gif"].as_bool().unwrap_or(false) { "gif" } else { "video" },
				&crosspost_parent_media["fallback_url"],
				Some(&crosspost_parent_media["hls_url"]),
				Some(&crosspost_parent_media["dash_url"]),
			)
		} else if data["post_hint"].as_str().unwrap_or("") == "image" {
			// Handle images, whether GIFs or pics
			let preview = &data["preview"]["images"][0];
			let mp4 = &preview["variants"]["mp4"];

			if mp4.is_object() {
				// Return the mp4 if the media is a gif
				("gif", &mp4["source"]["url"], None, None)
			} else {
				// Return the picture if the media is an image
				if data["domain"] == "i.redd.it" {
					("image", &data["url"], None, None)
				} else {
					("image", &preview["source"]["url"], None, None)
				}
			}
		} else if data["is_self"].as_bool().unwrap_or_default() {
			// If type is self, return permalink
			("self", &data["permalink"], None, None)
		} else if data["is_gallery"].as_bool().unwrap_or_default() {
			// If this post contains a gallery of images
			gallery = GalleryMedia::parse(&data["gallery_data"]["items"], &data["media_metadata"]);

			("gallery", &data["url"], None, None)
		} else if data["crosspost_parent_list"][0]["is_gallery"].as_bool().unwrap_or_default() {
			// If this post contains a gallery of images
			gallery = GalleryMedia::parse(
				&data["crosspost_parent_list"][0]["gallery_data"]["items"],
				&data["crosspost_parent_list"][0]["media_metadata"],
			);

			("gallery", &data["url"], None, None)
		} else if data["is_reddit_media_domain"].as_bool().unwrap_or_default() && data["domain"] == "i.redd.it" {
			// If this post contains a reddit media (image) URL.
			("image", &data["url"], None, None)
		} else {
			// If type can't be determined, return url
			("link", &data["url"], None, None)
		};

		let source = &data["preview"]["images"][0]["source"];

		let hls_url = hls_url_val.map_or(String::new(), |val| format_url(val.as_str().unwrap_or_default()));
		let dash_url = dash_url_val.map_or(String::new(), |val| format_url(val.as_str().unwrap_or_default()));
		let (preview_url, preview_srcset) = image_preview_variants(&data["preview"]["images"][0]);
		let poster = if redgifs_url.is_some() {
			redgifs_poster
		} else {
			format_url(source["url"].as_str().unwrap_or_default())
		};
		let duration_seconds = if redgifs_url.is_some() {
			redgifs_duration_seconds
		} else {
			data_preview["duration"]
				.as_f64()
				.or_else(|| secure_media["duration"].as_f64())
				.or_else(|| crosspost_parent_media["duration"].as_f64())
				.map(|value| value.round() as i64)
				.unwrap_or_default()
		};
		let duration = format_video_duration(duration_seconds);
		let video_preview_enabled =
			crate::video_preview::feature_enabled() && (post_type == "video" || post_type == "gif") && embed_code.is_empty() && !url_val.as_str().unwrap_or_default().is_empty();
		let video_preview_job_hint = if redgifs_url.is_some() {
			redgifs_preview_media_url
		} else if post_type == "video" || post_type == "gif" {
			url_val.as_str().unwrap_or_default().to_string()
		} else {
			String::new()
		};

		let download_name = if post_type == "image" || post_type == "gif" || post_type == "video" {
			let permalink_base = url_path_basename(data["permalink"].as_str().unwrap_or_default());
			let media_url_base = url_path_basename(url_val.as_str().unwrap_or_default());

			format!("redlib_{permalink_base}_{media_url_base}")
		} else {
			String::new()
		};

		(
			post_type.to_string(),
			Self {
				url: format_url(url_val.as_str().unwrap_or_default()),
				preview_url,
				preview_srcset,
				hls_url,
				dash_url,
				width: source["width"].as_i64().unwrap_or_default(),
				height: source["height"].as_i64().unwrap_or_default(),
				poster,
				duration_seconds,
				duration,
				download_name,
				embed_code,
				video_preview_enabled,
				video_preview_job_hint,
			},
			gallery,
		)
	}
}

pub fn format_video_duration(seconds: i64) -> String {
	if seconds <= 0 {
		return String::new();
	}

	let hours = seconds / 3600;
	let minutes = (seconds % 3600) / 60;
	let remaining_seconds = seconds % 60;

	if hours > 0 {
		format!("{hours}:{minutes:02}:{remaining_seconds:02}")
	} else {
		format!("{minutes}:{remaining_seconds:02}")
	}
}

#[derive(Clone, Serialize, PartialEq)]
pub struct GalleryMedia {
	pub url: String,
	pub preview_url: String,
	pub preview_srcset: String,
	pub width: i64,
	pub height: i64,
	pub caption: String,
	pub outbound_url: String,
}

impl GalleryMedia {
	fn parse(items: &Value, metadata: &Value) -> Vec<Self> {
		items
			.as_array()
			.unwrap_or(&Vec::new())
			.iter()
			.map(|item| {
				// For each image in gallery
				let media_id = item["media_id"].as_str().unwrap_or_default();
				let image = &metadata[media_id]["s"];
				let image_type = &metadata[media_id]["m"];

				let url = if image_type == "image/gif" {
					image["gif"].as_str().unwrap_or_default()
				} else {
					image["u"].as_str().unwrap_or_default()
				};

				// Construct gallery items
				Self {
					url: format_url(url),
					preview_url: largest_preview_url(&metadata[media_id]["p"]),
					preview_srcset: preview_srcset(&metadata[media_id]["p"]),
					width: image["x"].as_i64().unwrap_or_default(),
					height: image["y"].as_i64().unwrap_or_default(),
					caption: item["caption"].as_str().unwrap_or_default().to_string(),
					outbound_url: item["outbound_url"].as_str().unwrap_or_default().to_string(),
				}
			})
			.collect::<Vec<Self>>()
	}
}

/// Post containing content, metadata and media
#[derive(Serialize, PartialEq)]
pub struct Post {
	pub id: String,
	pub kind: String,
	pub title: String,
	pub community: String,
	pub body: String,
	pub author: Author,
	pub permalink: String,
	pub parent_kind: String,
	pub link_title: String,
	pub link_permalink: String,
	pub poll: Option<Poll>,
	pub score: (String, String),
	pub upvote_ratio: i64,
	pub post_type: String,
	pub flair: Flair,
	pub flags: Flags,
	pub thumbnail: Media,
	pub media: Media,
	pub domain: String,
	pub rel_time: String,
	pub created: String,
	pub created_ts: u64,
	pub num_duplicates: u64,
	pub comments: (String, String),
	pub gallery: Vec<GalleryMedia>,
	pub awards: Awards,
	pub nsfw: bool,
	pub out_url: Option<String>,
	pub ws_url: String,
	pub preview_body: String,
	pub collapsed_duplicates: Vec<CollapsedDuplicate>,
}

#[derive(Clone, Serialize, PartialEq)]
pub struct CollapsedDuplicate {
	pub title: String,
	pub permalink: String,
	pub community: String,
	pub display_community: String,
	pub rel_time: String,
	pub comments: (String, String),
	pub flair: Flair,
}

impl Clone for Post {
	fn clone(&self) -> Self {
		Self {
			id: self.id.clone(),
			kind: self.kind.clone(),
			title: self.title.clone(),
			community: self.community.clone(),
			body: self.body.clone(),
			author: self.author.clone(),
			permalink: self.permalink.clone(),
			parent_kind: self.parent_kind.clone(),
			link_title: self.link_title.clone(),
			link_permalink: self.link_permalink.clone(),
			poll: self.poll.clone(),
			score: self.score.clone(),
			upvote_ratio: self.upvote_ratio,
			post_type: self.post_type.clone(),
			flair: self.flair.clone(),
			flags: self.flags.clone(),
			thumbnail: self.thumbnail.clone(),
			media: self.media.clone(),
			domain: self.domain.clone(),
			rel_time: self.rel_time.clone(),
			created: self.created.clone(),
			created_ts: self.created_ts,
			num_duplicates: self.num_duplicates,
			comments: self.comments.clone(),
			gallery: self.gallery.clone(),
			awards: Awards(self.awards.0.clone()),
			nsfw: self.nsfw,
			out_url: self.out_url.clone(),
			ws_url: self.ws_url.clone(),
			preview_body: self.preview_body.clone(),
			collapsed_duplicates: self.collapsed_duplicates.clone(),
		}
	}
}

impl Post {
	/// Fetch posts of a user or subreddit and return a vector of posts and the "after" value
	pub async fn fetch(path: &str, quarantine: bool) -> Result<(Vec<Self>, String), String> {
		// Send a request to the url
		let res = match json(path.to_string(), quarantine).await {
			// If success, receive JSON in response
			Ok(response) => response,
			// If the Reddit API returns an error, exit this function
			Err(msg) => return Err(msg),
		};

		// Fetch the list of posts from the JSON response
		let Some(post_list) = res["data"]["children"].as_array() else {
			return Err("No posts found".to_string());
		};

		let mut posts: Vec<Self> = Vec::new();

		// For each post from posts list
		for post in post_list {
			let data = &post["data"];
			let kind = post["kind"].as_str().unwrap_or_default().to_string();
			let permalink = val(post, "permalink");
			let parent_kind = parent_kind(&val(post, "parent_id"));
			let link_permalink = listing_link_permalink(data["link_permalink"].as_str().unwrap_or_default(), &permalink);

			let (rel_time, created) = time(data["created_utc"].as_f64().unwrap_or_default());
			let created_ts = data["created_utc"].as_f64().unwrap_or_default().round() as u64;
			let score = data["score"].as_i64().unwrap_or_default();
			let ratio: f64 = data["upvote_ratio"].as_f64().unwrap_or(1.0) * 100.0;
			let title = val(post, "title");

			// User overview/comments listings can contain comment children (`t1`) as well as posts.
			let (post_type, media, gallery) = if kind == "t1" {
				("comment".to_string(), Media::empty(), Vec::new())
			} else {
				Media::parse(data).await
			};
			let awards = Awards::parse(&data["all_awardings"]);

			// selftext_html is set for text posts when browsing.
			let mut body = rewrite_urls(&val(post, "selftext_html"));
			if body.is_empty() {
				body = rewrite_urls(&val(post, "body_html"));
			}

			posts.push(Self {
				id: val(post, "id"),
				kind,
				title,
				community: val(post, "subreddit"),
				body,
				author: Author {
					name: val(post, "author"),
					flair: Flair {
						flair_parts: FlairPart::parse(
							data["author_flair_type"].as_str().unwrap_or_default(),
							data["author_flair_richtext"].as_array(),
							data["author_flair_text"].as_str(),
						),
						text: val(post, "link_flair_text"),
						background_color: val(post, "author_flair_background_color"),
						foreground_color: val(post, "author_flair_text_color"),
					},
					distinguished: val(post, "distinguished"),
					external_links: Vec::new(),
					avg_video_length: crate::user::cached_avg_video_length(&val(post, "author")),
				},
				permalink,
				parent_kind,
				link_title: val(post, "link_title"),
				link_permalink,
				poll: Poll::parse(&data["poll_data"]),
				score: if data["hide_score"].as_bool().unwrap_or_default() {
					("\u{2022}".to_string(), "Hidden".to_string())
				} else {
					format_num(score)
				},
				upvote_ratio: ratio as i64,
				post_type,
				flair: Flair {
					flair_parts: FlairPart::parse(
						data["link_flair_type"].as_str().unwrap_or_default(),
						data["link_flair_richtext"].as_array(),
						data["link_flair_text"].as_str(),
					),
					text: val(post, "link_flair_text"),
					background_color: val(post, "link_flair_background_color"),
					foreground_color: if val(post, "link_flair_text_color") == "dark" {
						"black".to_string()
					} else {
						"white".to_string()
					},
				},
				flags: Flags {
					spoiler: data["spoiler"].as_bool().unwrap_or_default(),
					nsfw: data["over_18"].as_bool().unwrap_or_default(),
					stickied: data["stickied"].as_bool().unwrap_or_default() || data["pinned"].as_bool().unwrap_or(false),
					nsfw_str: if data["over_18"].as_bool().unwrap_or_default() { "true" } else { "false" },
					spoiler_str: if data["spoiler"].as_bool().unwrap_or_default() { "true" } else { "false" },
					stickied_str: if data["stickied"].as_bool().unwrap_or_default() || data["pinned"].as_bool().unwrap_or(false) {
						"true"
					} else {
						"false"
					},
				},
				thumbnail: Media {
					url: format_url(val(post, "thumbnail").as_str()),
					preview_url: String::new(),
					preview_srcset: String::new(),
					hls_url: String::new(),
					dash_url: String::new(),
					width: data["thumbnail_width"].as_i64().unwrap_or_default(),
					height: data["thumbnail_height"].as_i64().unwrap_or_default(),
					poster: String::new(),
					duration_seconds: 0,
					duration: String::new(),
					download_name: String::new(),
					embed_code: String::new(),
					video_preview_enabled: false,
					video_preview_job_hint: String::new(),
				},
				media,
				domain: val(post, "domain"),
				rel_time,
				created,
				created_ts,
				num_duplicates: post["data"]["num_duplicates"].as_u64().unwrap_or(0),
				comments: format_num(data["num_comments"].as_i64().unwrap_or_default()),
				gallery,
				awards,
				nsfw: post["data"]["over_18"].as_bool().unwrap_or_default(),
				ws_url: val(post, "websocket_url"),
				out_url: post["data"]["url_overridden_by_dest"].as_str().map(|a| a.to_string()),
				preview_body: String::new(),
				collapsed_duplicates: Vec::new(),
			});
		}
		Ok((posts, res["data"]["after"].as_str().unwrap_or_default().to_string()))
	}
}

#[derive(Template)]
#[template(path = "comment.html")]
/// Comment with content, post, score and data/time that it was posted
pub struct Comment {
	pub id: String,
	pub kind: String,
	pub parent_id: String,
	pub parent_kind: String,
	pub post_link: String,
	pub post_author: String,
	pub body: String,
	pub author: Author,
	pub score: (String, String),
	pub rel_time: String,
	pub created: String,
	pub edited: (String, String),
	pub replies: Vec<Comment>,
	pub highlighted: bool,
	pub awards: Awards,
	pub collapsed: bool,
	pub is_filtered: bool,
	pub more_count: i64,
	pub prefs: Preferences,
}

#[derive(Default, Clone, Serialize, PartialEq)]
pub struct Award {
	pub name: String,
	pub icon_url: String,
	pub description: String,
	pub count: i64,
}

impl std::fmt::Display for Award {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{} {} {}", self.name, self.icon_url, self.description)
	}
}

#[derive(Serialize, PartialEq)]
pub struct Awards(pub Vec<Award>);

impl std::ops::Deref for Awards {
	type Target = Vec<Award>;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl std::fmt::Display for Awards {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		self.iter().try_fold((), |_, award| writeln!(f, "{award}"))
	}
}

impl Awards {
	/// Convert Reddit awards JSON to Awards struct
	pub fn parse(items: &Value) -> Self {
		let parsed = items.as_array().unwrap_or(&Vec::new()).iter().fold(Vec::new(), |mut awards, item| {
			let name = item["name"].as_str().unwrap_or_default().to_string();
			let icon_url = format_url(item["resized_icons"][0]["url"].as_str().unwrap_or_default());
			let description = item["description"].as_str().unwrap_or_default().to_string();
			let count: i64 = i64::from_str(&item["count"].to_string()).unwrap_or(1);

			awards.push(Award {
				name,
				icon_url,
				description,
				count,
			});

			awards
		});

		Self(parsed)
	}
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
	pub msg: String,
	pub prefs: Preferences,
	pub ratelimit: RateLimitDisplay,
	pub url: String,
}

#[derive(Default)]
pub struct RateLimitDisplay {
	pub visible: bool,
	pub status: String,
	pub remaining: String,
	pub reset: String,
	pub used: String,
	pub retry_after: String,
	pub request_rate: String,
}

impl RateLimitDisplay {
	fn from_error_message(msg: &str) -> Self {
		let message = msg.to_ascii_lowercase();
		let mentions_rate_limit = message.contains("too many requests") || message.contains("rate limit") || message.contains("ratelimit");
		let Some(snapshot) = latest_ratelimit_snapshot() else {
			return Self::default();
		};

		if !mentions_rate_limit && !snapshot.is_rate_limited() {
			return Self::default();
		}

		let request_rate = snapshot
			.remaining_requests_per_second()
			.map(|value| format!("{value:.2} req/s"))
			.unwrap_or_else(|| "Unavailable".to_string());
		let reset = snapshot
			.reset_seconds
			.map(|value| {
				if value >= 60.0 {
					format!("{value:.0} seconds ({:.1} minutes)", value / 60.0)
				} else {
					format!("{value:.0} seconds")
				}
			})
			.unwrap_or_else(|| "Unavailable".to_string());

		Self {
			visible: true,
			status: snapshot.status.to_string(),
			remaining: snapshot.remaining.map(|value| format!("{value:.0}")).unwrap_or_else(|| "Unavailable".to_string()),
			reset,
			used: snapshot.used.map(|value| format!("{value:.0}")).unwrap_or_else(|| "Unavailable".to_string()),
			retry_after: snapshot
				.retry_after_seconds
				.map(|value| format!("{value:.0} seconds"))
				.unwrap_or_else(|| "Unavailable".to_string()),
			request_rate,
		}
	}
}

#[derive(Template)]
#[template(path = "info.html")]
pub struct InfoTemplate {
	pub msg: String,
	pub prefs: Preferences,
	pub url: String,
}

/// Template for NSFW landing page. The landing page is displayed when a page's
/// content is wholly NSFW, but a user has not enabled the option to view NSFW
/// posts.
#[derive(Template)]
#[template(path = "nsfwlanding.html")]
pub struct NSFWLandingTemplate {
	/// Identifier for the resource. This is either a subreddit name or a
	/// username. (In the case of the latter, set is_user to true.)
	pub res: String,

	/// Identifies whether or not the resource is a subreddit, a user page,
	/// or a post.
	pub res_type: ResourceType,

	/// User preferences.
	pub prefs: Preferences,

	/// Request URL.
	pub url: String,
}

#[derive(Default, Clone, PartialEq)]
/// User struct containing metadata about user
pub struct User {
	pub name: String,
	pub title: String,
	pub icon: String,
	pub post_karma: (String, String),
	pub comment_karma: (String, String),
	pub created: String,
	pub banner: String,
	pub description: String,
	pub bio_html: String,
	pub external_links: Vec<ExternalLink>,
	pub nsfw: bool,
}

const AUTHOR_LINKS_CACHE_SIZE: usize = 512;
const AUTHOR_LINKS_CACHE_TTL: StdDuration = StdDuration::from_secs(10_800);
const AUTHOR_LINK_PREFETCH_CONCURRENCY: usize = 8;
const SUBREDDIT_FLAIR_CACHE_SIZE: usize = 1000;
const USER_SUBREDDIT_CACHE_SIZE: usize = 1000;
const SUBREDDIT_USER_CACHE_SIZE: usize = 1000;

static AUTHOR_LINKS_CACHE: LazyLock<Mutex<HashMap<String, CachedAuthorLinks>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SUBREDDIT_FLAIR_CACHE: LazyLock<Mutex<HashMap<String, CachedSubredditFlairs>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static USER_SUBREDDIT_CACHE: LazyLock<Mutex<HashMap<String, CachedUserSubreddits>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SUBREDDIT_USER_CACHE: LazyLock<Mutex<HashMap<String, CachedSubredditUsers>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn user_profile_details(subreddit: &Value) -> UserProfileDetails {
	let blurb = subreddit["public_description"].as_str().unwrap_or_default().trim().to_string();
	let bio_html = rewrite_urls(subreddit["description_html"].as_str().unwrap_or_default()).trim().to_string();
	let description = subreddit["description"].as_str().unwrap_or_default();

	UserProfileDetails {
		blurb: blurb.clone(),
		bio_html: bio_html.clone(),
		external_links: extract_external_links(&[&blurb, &bio_html, description, subreddit["public_description_html"].as_str().unwrap_or_default()]),
	}
}

pub async fn user_profile_details_with_social_links(name: &str, subreddit: &Value) -> UserProfileDetails {
	let mut profile = user_profile_details(subreddit);

	if let Ok(html) = fetch_user_profile_page_html(name).await {
		append_external_links(&mut profile.external_links, extract_profile_social_links(&html));
	}

	store_author_links(name, dedupe_external_links_by_host(profile.external_links.clone()));
	profile
}

#[cached(size = 512, time = 10800, result = true)]
pub async fn fetch_user_profile_details(name: String) -> Result<UserProfileDetails, String> {
	let path = format!("/user/{name}/about.json?raw_json=1");
	let response = json(path, false).await?;
	Ok(user_profile_details_with_social_links(&name, &response["data"]["subreddit"]).await)
}

pub async fn author_external_links(name: &str) -> Vec<ExternalLink> {
	let author = name.trim();
	if author.is_empty() || author == "[deleted]" {
		return Vec::new();
	}

	if let Some(links) = cached_author_links(author) {
		return links;
	}

	let links = fetch_user_profile_details(author.to_string())
		.await
		.map(|profile| dedupe_external_links_by_host(profile.external_links))
		.unwrap_or_default();

	store_author_links(author, links.clone());
	links
}

pub fn cached_author_external_links(name: &str) -> Vec<ExternalLink> {
	cached_author_links(name).unwrap_or_default()
}

pub fn hydrate_post_author_links_from_cache(posts: &mut [Post]) {
	for post in posts.iter_mut() {
		post.author.external_links = cached_author_external_links(&post.author.name);
	}
}

pub async fn prefetch_post_author_links(posts: &[Post]) {
	let mut authors = posts
		.iter()
		.map(|post| post.author.name.trim())
		.filter(|name| !name.is_empty() && *name != "[deleted]" && cached_author_links(name).is_none())
		.map(ToString::to_string)
		.collect::<HashSet<_>>()
		.into_iter()
		.collect::<Vec<_>>();
	authors.sort_unstable();

	let mut pending = JoinSet::new();
	for author in authors {
		pending.spawn(async move {
			let _ = author_external_links(&author).await;
		});

		if pending.len() >= AUTHOR_LINK_PREFETCH_CONCURRENCY {
			let _ = pending.join_next().await;
		}
	}

	while pending.join_next().await.is_some() {}
}

pub fn store_subreddit_flairs(subreddit: &str, posts: &[Post]) {
	let subreddit = subreddit.trim();
	if subreddit.is_empty() {
		return;
	}

	let mut seen_flairs = HashMap::new();
	for post in posts {
		let flair_text = post.flair.text.trim();
		if flair_text.is_empty() {
			continue;
		}
		seen_flairs.entry(flair_text.to_string()).or_insert_with(|| post.flair.clone());
	}

	if seen_flairs.is_empty() {
		return;
	}

	let Ok(mut cache) = SUBREDDIT_FLAIR_CACHE.lock() else {
		return;
	};
	let now = Instant::now();
	let entry = cache.entry(subreddit.to_string()).or_insert_with(|| CachedSubredditFlairs {
		flairs: HashMap::new(),
		cached_at: now,
	});
	entry.cached_at = now;
	for (name, flair) in seen_flairs {
		entry.flairs.insert(name, flair);
	}

	while cache.len() > SUBREDDIT_FLAIR_CACHE_SIZE {
		let Some(oldest_key) = cache.iter().min_by_key(|(_, entry)| entry.cached_at).map(|(key, _)| key.clone()) else {
			break;
		};
		cache.remove(&oldest_key);
	}
}

pub fn store_post_flairs(posts: &[Post]) {
	let Ok(mut cache) = SUBREDDIT_FLAIR_CACHE.lock() else {
		return;
	};
	let now = Instant::now();

	for post in posts {
		let subreddit = post.community.trim();
		let flair_text = post.flair.text.trim();
		if subreddit.is_empty() || flair_text.is_empty() {
			continue;
		}

		let entry = cache.entry(subreddit.to_string()).or_insert_with(|| CachedSubredditFlairs {
			flairs: HashMap::new(),
			cached_at: now,
		});
		entry.cached_at = now;
		entry.flairs.entry(flair_text.to_string()).or_insert_with(|| post.flair.clone());

		while cache.len() > SUBREDDIT_FLAIR_CACHE_SIZE {
			let Some(oldest_key) = cache.iter().min_by_key(|(_, entry)| entry.cached_at).map(|(key, _)| key.clone()) else {
				break;
			};
			cache.remove(&oldest_key);
		}
	}
}

pub fn cached_subreddit_flairs(subreddit: &str) -> Vec<Flair> {
	let subreddit = subreddit.trim();
	if subreddit.is_empty() {
		return Vec::new();
	}

	let Ok(mut cache) = SUBREDDIT_FLAIR_CACHE.lock() else {
		return Vec::new();
	};
	let Some(entry) = cache.get_mut(subreddit) else {
		return Vec::new();
	};
	entry.cached_at = Instant::now();

	let mut flairs = entry.flairs.values().cloned().collect::<Vec<_>>();
	flairs.sort_by(|a, b| a.text.to_lowercase().cmp(&b.text.to_lowercase()).then_with(|| a.text.cmp(&b.text)));
	flairs
}

pub fn store_user_subreddits(username: &str, posts: &[Post]) {
	let username = username.trim();
	if username.is_empty() {
		return;
	}

	let Ok(mut cache) = USER_SUBREDDIT_CACHE.lock() else {
		return;
	};
	let now = Instant::now();
	let entry = cache.entry(username.to_string()).or_insert_with(|| CachedUserSubreddits {
		subreddits: HashMap::new(),
		cached_at: now,
	});
	entry.cached_at = now;

	for post in posts {
		let subreddit = post.community.trim();
		if subreddit.is_empty() || subreddit.starts_with("u_") {
		} else {
			*entry.subreddits.entry(subreddit.to_string()).or_insert(0) += 1;
		}

		for duplicate in &post.collapsed_duplicates {
			let duplicate_subreddit = duplicate.community.trim();
			if duplicate_subreddit.is_empty() || duplicate_subreddit.starts_with("u_") {
				continue;
			}

			*entry.subreddits.entry(duplicate_subreddit.to_string()).or_insert(0) += 1;
		}
	}

	while cache.len() > USER_SUBREDDIT_CACHE_SIZE {
		let Some(oldest_key) = cache.iter().min_by_key(|(_, entry)| entry.cached_at).map(|(key, _)| key.clone()) else {
			break;
		};
		cache.remove(&oldest_key);
	}
}

pub fn cached_user_subreddits(username: &str) -> Vec<(String, u32)> {
	let username = username.trim();
	if username.is_empty() {
		return Vec::new();
	}

	let Ok(mut cache) = USER_SUBREDDIT_CACHE.lock() else {
		return Vec::new();
	};
	let Some(entry) = cache.get_mut(username) else {
		return Vec::new();
	};
	entry.cached_at = Instant::now();

	let mut subreddits = entry.subreddits.iter().map(|(name, count)| (name.clone(), *count)).collect::<Vec<_>>();
	subreddits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())).then_with(|| a.0.cmp(&b.0)));
	subreddits
}

pub fn store_subreddit_users(subreddit: &str, posts: &[Post]) {
	let subreddit = subreddit.trim();
	if subreddit.is_empty() {
		return;
	}

	let Ok(mut cache) = SUBREDDIT_USER_CACHE.lock() else {
		return;
	};
	let now = Instant::now();
	let entry = cache.entry(subreddit.to_string()).or_insert_with(|| CachedSubredditUsers {
		users: HashMap::new(),
		cached_at: now,
	});
	entry.cached_at = now;

	for post in posts {
		let author = post.author.name.trim();
		if author.is_empty() || author == "[deleted]" {
			continue;
		}

		*entry.users.entry(author.to_string()).or_insert(0) += 1;
	}

	while cache.len() > SUBREDDIT_USER_CACHE_SIZE {
		let Some(oldest_key) = cache.iter().min_by_key(|(_, entry)| entry.cached_at).map(|(key, _)| key.clone()) else {
			break;
		};
		cache.remove(&oldest_key);
	}
}

pub fn cached_subreddit_users(subreddit: &str) -> Vec<(String, u32)> {
	let subreddit = subreddit.trim();
	if subreddit.is_empty() {
		return Vec::new();
	}

	let Ok(mut cache) = SUBREDDIT_USER_CACHE.lock() else {
		return Vec::new();
	};
	let Some(entry) = cache.get_mut(subreddit) else {
		return Vec::new();
	};
	entry.cached_at = Instant::now();

	let mut users = entry.users.iter().map(|(name, count)| (name.clone(), *count)).collect::<Vec<_>>();
	users.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())).then_with(|| a.0.cmp(&b.0)));
	users
}

#[derive(Clone, Default, PartialEq)]
/// Subreddit struct containing metadata about community
pub struct Subreddit {
	pub name: String,
	pub title: String,
	pub description: String,
	pub info: String,
	pub has_sidebar: bool,
	// pub moderators: Vec<String>,
	pub icon: String,
	pub members: (String, String),
	pub active: (String, String),
	pub wiki: bool,
	pub nsfw: bool,
}

/// Parser for query params, used in sorting (eg. /r/rust/?sort=hot)
#[derive(serde::Deserialize, PartialEq)]
pub struct Params {
	pub t: Option<String>,
	pub q: Option<String>,
	pub sort: Option<String>,
	pub after: Option<String>,
	pub before: Option<String>,
}

#[derive(Default, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[revisioned(revision = 4)]
pub struct Preferences {
	#[revision(start = 1)]
	#[serde(skip_serializing, skip_deserializing)]
	pub available_themes: Vec<String>,
	#[revision(start = 1)]
	pub theme: String,
	#[revision(start = 1)]
	pub front_page: String,
	#[revision(start = 1)]
	pub layout: String,
	#[revision(start = 1)]
	pub wide: String,
	#[revision(start = 1)]
	pub blur_spoiler: String,
	#[revision(start = 1)]
	pub show_nsfw: String,
	#[revision(start = 1)]
	pub blur_nsfw: String,
	#[revision(start = 1)]
	pub hide_hls_notification: String,
	#[revision(start = 2)]
	pub image_quality: String,
	#[revision(start = 1)]
	pub video_quality: String,
	#[revision(start = 1)]
	pub hide_sidebar_and_summary: String,
	#[revision(start = 1)]
	pub use_dash: String,
	pub use_hls: String,
	#[revision(start = 1)]
	pub autoplay_videos: String,
	#[revision(start = 4)]
	pub video_previews: String,
	#[revision(start = 1)]
	pub fixed_navbar: String,
	#[revision(start = 1)]
	pub disable_visit_reddit_confirmation: String,
	#[revision(start = 1)]
	pub comment_sort: String,
	#[revision(start = 1)]
	pub post_sort: String,
	#[revision(start = 1)]
	#[serde(serialize_with = "serialize_vec_with_plus", deserialize_with = "deserialize_vec_with_plus")]
	pub subscriptions: Vec<String>,
	#[revision(start = 1)]
	#[serde(serialize_with = "serialize_vec_with_plus", deserialize_with = "deserialize_vec_with_plus")]
	pub filters: Vec<String>,
	#[revision(start = 3)]
	#[serde(default, serialize_with = "serialize_flair_subscriptions", deserialize_with = "deserialize_flair_subscriptions")]
	pub flair_subscriptions: Vec<FlairSubscription>,
	#[revision(start = 1)]
	pub hide_awards: String,
	#[revision(start = 1)]
	pub hide_score: String,
	#[revision(start = 1)]
	pub remove_default_feeds: String,
}

#[derive(Deserialize)]
struct LegacyPreferencesV1 {
	pub theme: String,
	pub front_page: String,
	pub layout: String,
	pub wide: String,
	pub blur_spoiler: String,
	pub show_nsfw: String,
	pub blur_nsfw: String,
	pub hide_hls_notification: String,
	pub video_quality: String,
	pub hide_sidebar_and_summary: String,
	pub use_dash: String,
	pub use_hls: String,
	pub autoplay_videos: String,
	pub fixed_navbar: String,
	pub disable_visit_reddit_confirmation: String,
	pub comment_sort: String,
	pub post_sort: String,
	#[serde(deserialize_with = "deserialize_vec_with_plus")]
	pub subscriptions: Vec<String>,
	#[serde(deserialize_with = "deserialize_vec_with_plus")]
	pub filters: Vec<String>,
	pub hide_awards: String,
	pub hide_score: String,
	pub remove_default_feeds: String,
}

impl From<LegacyPreferencesV1> for Preferences {
	fn from(value: LegacyPreferencesV1) -> Self {
		Self {
			available_themes: Vec::new(),
			theme: value.theme,
			front_page: value.front_page,
			layout: value.layout,
			wide: value.wide,
			blur_spoiler: value.blur_spoiler,
			show_nsfw: value.show_nsfw,
			blur_nsfw: value.blur_nsfw,
			hide_hls_notification: value.hide_hls_notification,
			image_quality: String::new(),
			video_quality: value.video_quality,
			hide_sidebar_and_summary: value.hide_sidebar_and_summary,
			use_dash: value.use_dash,
			use_hls: value.use_hls,
			autoplay_videos: value.autoplay_videos,
			video_previews: "on".to_string(),
			fixed_navbar: value.fixed_navbar,
			disable_visit_reddit_confirmation: value.disable_visit_reddit_confirmation,
			comment_sort: value.comment_sort,
			post_sort: value.post_sort,
			subscriptions: value.subscriptions,
			filters: value.filters,
			flair_subscriptions: Vec::new(),
			hide_awards: value.hide_awards,
			hide_score: value.hide_score,
			remove_default_feeds: value.remove_default_feeds,
		}
	}
}

#[derive(Deserialize)]
struct LegacyPreferencesV0 {
	pub theme: String,
	pub front_page: String,
	pub layout: String,
	pub wide: String,
	pub blur_spoiler: String,
	pub show_nsfw: String,
	pub blur_nsfw: String,
	pub hide_hls_notification: String,
	pub video_quality: String,
	pub hide_sidebar_and_summary: String,
	pub use_dash: String,
	pub use_hls: String,
	pub autoplay_videos: String,
	pub fixed_navbar: String,
	pub comment_sort: String,
	pub post_sort: String,
	#[serde(deserialize_with = "deserialize_vec_with_plus")]
	pub subscriptions: Vec<String>,
	#[serde(deserialize_with = "deserialize_vec_with_plus")]
	pub filters: Vec<String>,
	pub hide_awards: String,
	pub hide_score: String,
	pub remove_default_feeds: String,
}

impl From<LegacyPreferencesV0> for Preferences {
	fn from(value: LegacyPreferencesV0) -> Self {
		Self {
			available_themes: Vec::new(),
			theme: value.theme,
			front_page: value.front_page,
			layout: value.layout,
			wide: value.wide,
			blur_spoiler: value.blur_spoiler,
			show_nsfw: value.show_nsfw,
			blur_nsfw: value.blur_nsfw,
			hide_hls_notification: value.hide_hls_notification,
			image_quality: String::new(),
			video_quality: value.video_quality,
			hide_sidebar_and_summary: value.hide_sidebar_and_summary,
			use_dash: value.use_dash,
			use_hls: value.use_hls,
			autoplay_videos: value.autoplay_videos,
			video_previews: "on".to_string(),
			fixed_navbar: value.fixed_navbar,
			disable_visit_reddit_confirmation: String::new(),
			comment_sort: value.comment_sort,
			post_sort: value.post_sort,
			subscriptions: value.subscriptions,
			filters: value.filters,
			flair_subscriptions: Vec::new(),
			hide_awards: value.hide_awards,
			hide_score: value.hide_score,
			remove_default_feeds: value.remove_default_feeds,
		}
	}
}

fn serialize_vec_with_plus<S>(vec: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	serializer.serialize_str(&vec.join("+"))
}

fn deserialize_vec_with_plus<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
	D: Deserializer<'de>,
{
	let string = String::deserialize(deserializer)?;
	if string.is_empty() {
		return Ok(Vec::new());
	}
	Ok(string.split('+').map(|s| s.to_string()).collect())
}

pub fn encode_flair_subscriptions(subscriptions: &[FlairSubscription]) -> Result<String, String> {
	if subscriptions.is_empty() {
		return Ok(String::new());
	}

	let json = serde_json::to_vec(subscriptions).map_err(|e| e.to_string())?;
	Ok(URL_SAFE_NO_PAD.encode(json))
}

pub fn decode_flair_subscriptions(encoded: &str) -> Result<Vec<FlairSubscription>, String> {
	if encoded.is_empty() {
		return Ok(Vec::new());
	}

	let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|e| e.to_string())?;
	serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn serialize_flair_subscriptions<S>(subscriptions: &[FlairSubscription], serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	serializer.serialize_str(&encode_flair_subscriptions(subscriptions).map_err(serde::ser::Error::custom)?)
}

fn deserialize_flair_subscriptions<'de, D>(deserializer: D) -> Result<Vec<FlairSubscription>, D::Error>
where
	D: Deserializer<'de>,
{
	let encoded = String::deserialize(deserializer)?;
	decode_flair_subscriptions(&encoded).map_err(serde::de::Error::custom)
}

fn chunk_cookie_value(value: &str, max_len: usize) -> Vec<String> {
	if value.is_empty() {
		return Vec::new();
	}

	let mut chunks = Vec::new();
	let mut current = String::new();

	for ch in value.chars() {
		if !current.is_empty() && current.len() + ch.len_utf8() > max_len {
			chunks.push(current);
			current = String::new();
		}
		current.push(ch);
	}

	if !current.is_empty() {
		chunks.push(current);
	}

	chunks
}

fn decode_cookie_chunk(value: &str) -> String {
	percent_decode_str(value).decode_utf8_lossy().into_owned()
}

pub fn read_sharded_cookie(req: &Request<Body>, base_name: &str) -> String {
	let mut value = String::new();
	let mut index = 0usize;

	loop {
		let cookie_name = if index == 0 { base_name.to_string() } else { format!("{base_name}{index}") };

		let Some(cookie) = req.cookie(&cookie_name) else {
			break;
		};

		value.push_str(&decode_cookie_chunk(cookie.value()));
		index += 1;
	}

	value
}

pub fn write_sharded_cookie(response: &mut Response<Body>, req: &Request<Body>, base_name: &str, value: &str) {
	let chunks = chunk_cookie_value(value, 3800);

	if chunks.is_empty() {
		clear_sharded_cookie(response, req, base_name);
		return;
	}

	for (index, chunk) in chunks.iter().enumerate() {
		let cookie_name = if index == 0 { base_name.to_string() } else { format!("{base_name}{index}") };

		response.insert_cookie(
			Cookie::build((cookie_name, chunk.clone()))
				.path("/")
				.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
				.into(),
		);
	}

	let mut stale_index = chunks.len();
	loop {
		let cookie_name = if stale_index == 0 { base_name.to_string() } else { format!("{base_name}{stale_index}") };

		if req.cookie(&cookie_name).is_none() {
			break;
		}

		response.remove_cookie(cookie_name);
		stale_index += 1;
	}
}

pub fn clear_sharded_cookie(response: &mut Response<Body>, req: &Request<Body>, base_name: &str) {
	let mut index = 0usize;
	loop {
		let cookie_name = if index == 0 { base_name.to_string() } else { format!("{base_name}{index}") };

		if req.cookie(&cookie_name).is_none() {
			break;
		}

		response.remove_cookie(cookie_name);
		index += 1;
	}
}

#[derive(RustEmbed)]
#[folder = "static/themes/"]
#[include = "*.css"]
pub struct ThemeAssets;

impl Preferences {
	/// Build preferences from cookies
	pub fn new(req: &Request<Body>) -> Self {
		// Read available theme names from embedded css files.
		// Always make the default "system" theme available.
		let mut themes = vec!["system".to_string()];
		for file in ThemeAssets::iter() {
			let chunks: Vec<&str> = file.as_ref().split(".css").collect();
			themes.push(chunks[0].to_owned());
		}
		Self {
			available_themes: themes,
			theme: setting(req, "theme"),
			front_page: setting(req, "front_page"),
			layout: setting(req, "layout"),
			wide: setting(req, "wide"),
			blur_spoiler: setting(req, "blur_spoiler"),
			show_nsfw: setting(req, "show_nsfw"),
			hide_sidebar_and_summary: setting(req, "hide_sidebar_and_summary"),
			blur_nsfw: setting(req, "blur_nsfw"),
			use_dash: setting(req, "use_dash"),
			use_hls: setting(req, "use_hls"),
			hide_hls_notification: setting(req, "hide_hls_notification"),
			image_quality: setting(req, "image_quality"),
			video_quality: setting(req, "video_quality"),
			autoplay_videos: setting(req, "autoplay_videos"),
			video_previews: setting_or_default(req, "video_previews", "on".to_string()),
			fixed_navbar: setting_or_default(req, "fixed_navbar", "on".to_string()),
			disable_visit_reddit_confirmation: setting(req, "disable_visit_reddit_confirmation"),
			comment_sort: setting(req, "comment_sort"),
			post_sort: setting(req, "post_sort"),
			subscriptions: setting(req, "subscriptions").split('+').map(String::from).filter(|s| !s.is_empty()).collect(),
			filters: setting(req, "filters").split('+').map(String::from).filter(|s| !s.is_empty()).collect(),
			flair_subscriptions: decode_flair_subscriptions(&setting(req, "flair_subscriptions")).unwrap_or_default(),
			hide_awards: setting(req, "hide_awards"),
			hide_score: setting(req, "hide_score"),
			remove_default_feeds: setting(req, "remove_default_feeds"),
		}
	}

	pub fn to_urlencoded(&self) -> Result<String, String> {
		serde_urlencoded::to_string(self).map_err(|e| e.to_string())
	}

	pub fn has_subscription(&self, value: &str) -> bool {
		self.subscriptions.iter().any(|subscription| subscription.eq_ignore_ascii_case(value))
	}

	pub fn has_filter(&self, value: &str) -> bool {
		self.filters.iter().any(|filter| filter.eq_ignore_ascii_case(value))
	}

	pub fn to_bincode(&self) -> Result<Vec<u8>, String> {
		let mut bytes = Vec::new();
		self.serialize_revisioned(&mut bytes).map_err(|e| e.to_string())?;
		Ok(bytes)
	}
	pub fn to_compressed_bincode(&self) -> Result<Vec<u8>, String> {
		deflate_compress(self.to_bincode()?)
	}
	pub fn to_bincode_str(&self) -> Result<String, String> {
		Ok(base2048::encode(&self.to_compressed_bincode()?))
	}
}

pub fn preferences_from_bincode(bytes: &[u8]) -> Result<Preferences, String> {
	let mut reader = bytes;
	match Preferences::deserialize_revisioned(&mut reader) {
		Ok(preferences) => Ok(preferences),
		Err(_) => bincode::deserialize::<Preferences>(bytes)
			.or_else(|_| bincode::deserialize::<LegacyPreferencesV1>(bytes).map(Preferences::from))
			.or_else(|_| bincode::deserialize::<LegacyPreferencesV0>(bytes).map(Preferences::from))
			.map_err(|e| e.to_string()),
	}
}

pub fn deflate_compress(i: Vec<u8>) -> Result<Vec<u8>, String> {
	let mut e = Encoder::new(Vec::new());
	e.write_all(&i).map_err(|e| e.to_string())?;
	e.finish().into_result().map_err(|e| e.to_string())
}

pub fn deflate_decompress(i: Vec<u8>) -> Result<Vec<u8>, String> {
	let mut decoder = Decoder::new(&i[..]);
	let mut out = Vec::new();
	decoder.read_to_end(&mut out).map_err(|e| format!("Failed to read from gzip decoder: {e}"))?;
	Ok(out)
}

/// Gets a `HashSet` of filters from the cookie in the given `Request`.
pub fn get_filters(req: &Request<Body>) -> HashSet<String> {
	setting(req, "filters").split('+').map(String::from).filter(|s| !s.is_empty()).collect::<HashSet<String>>()
}

/// Filters a `Vec<Post>` by the given `HashSet` of filters (each filter being
/// a subreddit name or a user name). If a `Post`'s subreddit or author is
/// found in the filters, it is removed.
///
/// The first value of the return tuple is the number of posts filtered. The
/// second return value is `true` if all posts were filtered.
pub fn filter_posts(posts: &mut Vec<Post>, filters: &HashSet<String>) -> (u64, bool) {
	// This is the length of the Vec<Post> prior to applying the filter.
	let lb: u64 = posts.len().try_into().unwrap_or(0);

	if posts.is_empty() {
		(0, false)
	} else {
		posts.retain(|p| !(filters.contains(&p.community) || filters.contains(&["u_", &p.author.name].concat())));

		// Get the length of the Vec<Post> after applying the filter.
		// If lb > la, then at least one post was removed.
		let la: u64 = posts.len().try_into().unwrap_or(0);

		(lb - la, posts.is_empty())
	}
}

/// Creates a [`Post`] from a provided JSON.
pub async fn parse_post(post: &Value) -> Post {
	// Grab UTC time as unix timestamp
	let (rel_time, created) = time(post["data"]["created_utc"].as_f64().unwrap_or_default());
	// Parse post score and upvote ratio
	let score = post["data"]["score"].as_i64().unwrap_or_default();
	let ratio: f64 = post["data"]["upvote_ratio"].as_f64().unwrap_or(1.0) * 100.0;

	// Determine the type of media along with the media URL
	let (post_type, media, gallery) = Media::parse(&post["data"]).await;

	let created_ts = post["data"]["created_utc"].as_f64().unwrap_or_default().round() as u64;

	let awards: Awards = Awards::parse(&post["data"]["all_awardings"]);

	let permalink = val(post, "permalink");

	let poll = Poll::parse(&post["data"]["poll_data"]);

	let body = if val(post, "removed_by_category") == "moderator" {
		format!(
			"<div class=\"md\"><p>[removed] — <a href=\"https://{}{permalink}\">view removed post</a></p></div>",
			get_setting("REDLIB_PUSHSHIFT_FRONTEND").unwrap_or_else(|| String::from(crate::config::DEFAULT_PUSHSHIFT_FRONTEND)),
		)
	} else {
		let selftext = val(post, "selftext");
		if selftext.contains("```") {
			let mut html_output = String::new();
			let parser = pulldown_cmark::Parser::new(&selftext);
			pulldown_cmark::html::push_html(&mut html_output, parser);
			rewrite_urls(&html_output)
		} else {
			rewrite_urls(&val(post, "selftext_html"))
		}
	};

	// Build a post using data parsed from Reddit post API
	Post {
		id: val(post, "id"),
		kind: post["kind"].as_str().unwrap_or("t3").to_string(),
		title: val(post, "title"),
		community: val(post, "subreddit"),
		body,
		author: Author {
			name: val(post, "author"),
			flair: Flair {
				flair_parts: FlairPart::parse(
					post["data"]["author_flair_type"].as_str().unwrap_or_default(),
					post["data"]["author_flair_richtext"].as_array(),
					post["data"]["author_flair_text"].as_str(),
				),
				text: val(post, "link_flair_text"),
				background_color: val(post, "author_flair_background_color"),
				foreground_color: val(post, "author_flair_text_color"),
			},
			distinguished: val(post, "distinguished"),
			external_links: author_external_links(&val(post, "author")).await,
			avg_video_length: crate::user::cached_avg_video_length(&val(post, "author")),
		},
		permalink,
		parent_kind: parent_kind(&val(post, "parent_id")),
		link_title: val(post, "link_title"),
		link_permalink: listing_link_permalink(post["data"]["link_permalink"].as_str().unwrap_or_default(), &val(post, "permalink")),
		poll,
		score: format_num(score),
		upvote_ratio: ratio as i64,
		post_type,
		media,
		thumbnail: Media {
			url: format_url(val(post, "thumbnail").as_str()),
			preview_url: String::new(),
			preview_srcset: String::new(),
			hls_url: String::new(),
			dash_url: String::new(),
			width: post["data"]["thumbnail_width"].as_i64().unwrap_or_default(),
			height: post["data"]["thumbnail_height"].as_i64().unwrap_or_default(),
			poster: String::new(),
			duration_seconds: 0,
			duration: String::new(),
			download_name: String::new(),
			embed_code: String::new(),
			video_preview_enabled: false,
			video_preview_job_hint: String::new(),
		},
		flair: Flair {
			flair_parts: FlairPart::parse(
				post["data"]["link_flair_type"].as_str().unwrap_or_default(),
				post["data"]["link_flair_richtext"].as_array(),
				post["data"]["link_flair_text"].as_str(),
			),
			text: val(post, "link_flair_text"),
			background_color: val(post, "link_flair_background_color"),
			foreground_color: if val(post, "link_flair_text_color") == "dark" {
				"black".to_string()
			} else {
				"white".to_string()
			},
		},
		flags: Flags {
			spoiler: post["data"]["spoiler"].as_bool().unwrap_or_default(),
			nsfw: post["data"]["over_18"].as_bool().unwrap_or_default(),
			stickied: post["data"]["stickied"].as_bool().unwrap_or_default() || post["data"]["pinned"].as_bool().unwrap_or(false),
			nsfw_str: if post["data"]["over_18"].as_bool().unwrap_or_default() { "true" } else { "false" },
			spoiler_str: if post["data"]["spoiler"].as_bool().unwrap_or_default() { "true" } else { "false" },
			stickied_str: if post["data"]["stickied"].as_bool().unwrap_or_default() || post["data"]["pinned"].as_bool().unwrap_or(false) {
				"true"
			} else {
				"false"
			},
		},
		domain: val(post, "domain"),
		rel_time,
		created,
		created_ts,
		num_duplicates: post["data"]["num_duplicates"].as_u64().unwrap_or(0),
		comments: format_num(post["data"]["num_comments"].as_i64().unwrap_or_default()),
		gallery,
		awards,
		nsfw: post["data"]["over_18"].as_bool().unwrap_or_default(),
		ws_url: val(post, "websocket_url"),
		out_url: post["data"]["url_overridden_by_dest"].as_str().map(|a| a.to_string()),
		preview_body: String::new(),
		collapsed_duplicates: Vec::new(),
	}
}

fn parent_kind(parent_id: &str) -> String {
	parent_id.split_once('_').map_or(String::new(), |(kind, _)| kind.to_string())
}

fn listing_link_permalink(link_permalink: &str, permalink: &str) -> String {
	if link_permalink.starts_with('/') {
		return link_permalink.to_string();
	}

	if let Ok(parsed) = Url::parse(link_permalink) {
		let path = parsed.path().to_string();
		if !path.is_empty() {
			return path;
		}
	}

	let trimmed = permalink.trim_end_matches('/');
	if let Some((thread_path, _)) = trimmed.rsplit_once('/') {
		return format!("{thread_path}/");
	}

	permalink.to_string()
}

//
// FORMATTING
//

/// Grab a query parameter from a url
pub fn param(path: &str, value: &str) -> Option<String> {
	Some(
		Url::parse(format!("https://libredd.it/{path}").as_str())
			.ok()?
			.query_pairs()
			.into_owned()
			.collect::<HashMap<_, _>>()
			.get(value)?
			.clone(),
	)
}

pub fn flair_feed_url(subreddit: &str, flair: &str) -> String {
	let query = url::form_urlencoded::Serializer::new(String::new()).append_pair("flair", flair).finish();
	format!("/r/{subreddit}?{query}")
}

pub fn flair_subscription_target(subreddit: &str, flair: &str) -> String {
	let encoded = URL_SAFE_NO_PAD.encode(flair.as_bytes());
	format!("flair:{subreddit}:{encoded}")
}

pub fn flair_subscription_action_url(subreddit: &str, action: &str, flair: &str, redirect: Option<&str>) -> String {
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	serializer.append_pair("name", flair);
	serializer.append_pair("canonical", subreddit);
	if let Some(redirect) = redirect.filter(|value| !value.is_empty()) {
		serializer.append_pair("redirect", redirect);
	}

	format!("/r/{subreddit}/flair/{action}?{}", serializer.finish())
}

/// Retrieve the value of a setting by name
pub fn setting(req: &Request<Body>, name: &str) -> String {
	if matches!(name, "subscriptions" | "filters" | "flair_subscriptions") {
		let value = read_sharded_cookie(req, name);
		if !value.is_empty() {
			return value;
		}
	}

	req
		.cookie(name)
		.unwrap_or_else(|| {
			// If there is no cookie for this setting, try receiving a default from the config
			if let Some(default) = get_setting(&format!("REDLIB_DEFAULT_{}", name.to_uppercase())) {
				Cookie::new(name, default)
			} else {
				Cookie::from(name)
			}
		})
		.value()
		.to_string()
}

/// Retrieve the value of a setting by name or the default value
pub fn setting_or_default(req: &Request<Body>, name: &str, default: String) -> String {
	let value = setting(req, name);
	if value.is_empty() {
		default
	} else {
		value
	}
}

/// Detect and redirect in the event of a random subreddit
pub async fn catch_random(sub: &str, additional: &str) -> Result<Response<Body>, String> {
	if sub == "random" || sub == "randnsfw" {
		Ok(redirect(&format!(
			"/r/{}{additional}",
			json(format!("/r/{sub}/about.json?raw_json=1"), false).await?["data"]["display_name"]
				.as_str()
				.unwrap_or_default()
		)))
	} else {
		Err("No redirect needed".to_string())
	}
}

static REGEX_URL_WWW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://www\.reddit\.com/(.*)").unwrap());
static REGEX_URL_OLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://old\.reddit\.com/(.*)").unwrap());
static REGEX_URL_NP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://np\.reddit\.com/(.*)").unwrap());
static REGEX_URL_PLAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://reddit\.com/(.*)").unwrap());
static REGEX_URL_VIDEOS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://v\.redd\.it/(.*)/DASH_([0-9]{2,4}(\.mp4|$|\?source=fallback))").unwrap());
static REGEX_URL_VIDEOS_HLS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://v\.redd\.it/(.+)/(HLSPlaylist\.m3u8.*)$").unwrap());
static REGEX_URL_VIDEOS_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://v\.redd\.it/(.+)/(DASHPlaylist\.mpd.*)$").unwrap());
static REGEX_URL_IMAGES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://i\.redd\.it/(.*)").unwrap());
static REGEX_URL_THUMBS_A: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://a\.thumbs\.redditmedia\.com/(.*)").unwrap());
static REGEX_URL_THUMBS_B: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://b\.thumbs\.redditmedia\.com/(.*)").unwrap());
static REGEX_URL_EMOJI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://emoji\.redditmedia\.com/(.*)/(.*)").unwrap());
static REGEX_URL_PREVIEW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://preview\.redd\.it/(.*)").unwrap());
static REGEX_URL_EXTERNAL_PREVIEW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://external\-preview\.redd\.it/(.*)").unwrap());
static REGEX_URL_STYLES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://styles\.redditmedia\.com/(.*)").unwrap());
static REGEX_URL_STATIC_MEDIA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://www\.redditstatic\.com/(.*)").unwrap());

fn image_preview_variants(image: &Value) -> (String, String) {
	let resolutions = &image["resolutions"];
	let preview_url = largest_preview_url(resolutions);
	let preview_srcset = preview_srcset(resolutions);

	if preview_url.is_empty() {
		(format_url(image["source"]["url"].as_str().unwrap_or_default()), preview_srcset)
	} else {
		(preview_url, preview_srcset)
	}
}

fn largest_preview_url(resolutions: &Value) -> String {
	resolutions
		.as_array()
		.and_then(|items| items.last())
		.and_then(|item| item["url"].as_str())
		.map(format_url)
		.unwrap_or_default()
}

fn preview_srcset(resolutions: &Value) -> String {
	resolutions
		.as_array()
		.map(|items| {
			items
				.iter()
				.filter_map(|item| {
					let url = item["url"].as_str()?;
					let width = item["width"].as_i64()?;
					Some(format!("{} {}w", format_url(url), width))
				})
				.collect::<Vec<_>>()
				.join(", ")
		})
		.unwrap_or_default()
}

/// Direct urls to proxy if proxy is enabled
pub fn format_url(url: &str) -> String {
	if url.is_empty() || url == "self" || url == "default" || url == "nsfw" || url == "spoiler" {
		String::new()
	} else {
		Url::parse(url).map_or(url.to_string(), |parsed| {
			let domain = parsed.domain().unwrap_or_default();

			let capture = |regex: &Regex, format: &str, segments: i16| {
				regex.captures(url).map_or(String::new(), |caps| match segments {
					1 => [format, &caps[1]].join(""),
					2 => [format, &caps[1], "/", &caps[2]].join(""),
					_ => String::new(),
				})
			};

			macro_rules! chain {
				() => {
					{
						String::new()
					}
				};

				( $first_fn:expr $(, $other_fns:expr)* $(,)?) => {
					{
						let result = $first_fn;
						if result.is_empty() {
							chain!($($other_fns,)*)
						}
						else
						{
							result
						}
					}
				};
			}

			match domain {
				"www.reddit.com" => capture(&REGEX_URL_WWW, "/", 1),
				"old.reddit.com" => capture(&REGEX_URL_OLD, "/", 1),
				"np.reddit.com" => capture(&REGEX_URL_NP, "/", 1),
				"reddit.com" => capture(&REGEX_URL_PLAIN, "/", 1),
				"v.redd.it" => chain!(
					capture(&REGEX_URL_VIDEOS, "/vid/", 2),
					capture(&REGEX_URL_VIDEOS_HLS, "/hls/", 2),
					capture(&REGEX_URL_VIDEOS_DASH, "/dash/", 2)
				),
				"i.redd.it" => capture(&REGEX_URL_IMAGES, "/img/", 1),
				"a.thumbs.redditmedia.com" => capture(&REGEX_URL_THUMBS_A, "/thumb/a/", 1),
				"b.thumbs.redditmedia.com" => capture(&REGEX_URL_THUMBS_B, "/thumb/b/", 1),
				"emoji.redditmedia.com" => capture(&REGEX_URL_EMOJI, "/emoji/", 2),
				"preview.redd.it" => capture(&REGEX_URL_PREVIEW, "/preview/pre/", 1),
				"external-preview.redd.it" => capture(&REGEX_URL_EXTERNAL_PREVIEW, "/preview/external-pre/", 1),
				"styles.redditmedia.com" => capture(&REGEX_URL_STYLES, "/style/", 1),
				"www.redditstatic.com" => capture(&REGEX_URL_STATIC_MEDIA, "/static/", 1),
				"www.redgifs.com" | "redgifs.com" => redgifs::watch_path_from_url(url).map_or_else(|| url.to_string(), |path| format!("/redgifs/{path}")),
				d if d.starts_with("v") && d.ends_with(".redgifs.com") => redgifs::watch_path_from_url(url).map_or_else(|| url.to_string(), |path| format!("/redgifs/{path}")),
				_ => url.to_string(),
			}
		})
	}
}

static REGEX_BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^- (.*)$").unwrap());
static REGEX_BULLET_CONSECUTIVE_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"</ul>\n<ul>").unwrap());

pub fn render_bullet_lists(input_text: &str) -> String {
	// ref: https://stackoverflow.com/a/4902622
	// First enclose each bullet with <ul> <li> tags
	let text1 = REGEX_BULLET.replace_all(input_text, "<ul><li>$1</li></ul>").to_string();
	// Then remove any consecutive </ul> <ul> tags
	REGEX_BULLET_CONSECUTIVE_LINES.replace_all(&text1, "").to_string()
}

// These are links we want to replace in-body
static REDDIT_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"href="(https|http|)://(www\.|old\.|np\.|amp\.|new\.|)(reddit\.com|redd\.it)/"#).unwrap());
static REDDIT_PREVIEW_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://(external-preview|preview|i)\.redd\.it(.*)").unwrap());
static REDDIT_EMOJI_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://(www|).redditstatic\.com/(.*)").unwrap());
static REDLIB_PREVIEW_LINK_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"/(img|preview/)(pre|external-pre)?/(.*?)>"#).unwrap());
static REDLIB_PREVIEW_TEXT_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r">(.*?)</a>").unwrap());
static HTML_HREF_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"href="([^"]+)""#).unwrap());
static PLAIN_URL_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https?://[^\s<>"']+"#).unwrap());
static BARE_DOMAIN_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\b(?:www\.)?(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,}(?:/[^\s<>"']*)?"#).unwrap());
static PROFILE_SOCIAL_LINK_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"data-faceplate-tracking-context="([^"]*social_link[^"]*)""#).unwrap());
static LINKTREE_NEXT_DATA_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)<script[^>]*id="__NEXT_DATA__"[^>]*>(.*?)</script>"#).unwrap());
static ALLMYLINKS_DATA_X_URL_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\bdata-x-url="([^"]+)""#).unwrap());
static ALLMYLINKS_TITLE_URL_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)\btitle="(https?://[^"]+)""#).unwrap());

/// Rewrite Reddit links to Redlib in body of text
pub fn rewrite_urls(input_text: &str) -> String {
	let mut text1 =
		// Rewrite Reddit links to Redlib
		REDDIT_REGEX.replace_all(input_text, r#"href="/"#).to_string();

	loop {
		if REDDIT_EMOJI_REGEX.find(&text1).is_none() {
			break;
		} else {
			text1 = REDDIT_EMOJI_REGEX
				.replace_all(&text1, format_url(REDDIT_EMOJI_REGEX.find(&text1).map(|x| x.as_str()).unwrap_or_default()))
				.to_string()
		}
	}

	// Remove (html-encoded) "\" from URLs.
	text1 = text1.replace("%5C", "").replace("\\_", "_");

	// Rewrite external media previews to Redlib
	loop {
		if REDDIT_PREVIEW_REGEX.find(&text1).is_none() {
			return text1;
		} else {
			let formatted_url = format_url(REDDIT_PREVIEW_REGEX.find(&text1).map(|x| x.as_str()).unwrap_or_default());

			let image_url = REDLIB_PREVIEW_LINK_REGEX.find(&formatted_url).map_or("", |m| m.as_str());
			let mut image_caption = REDLIB_PREVIEW_TEXT_REGEX.find(&formatted_url).map_or("", |m| m.as_str());

			/* As long as image_caption isn't empty remove first and last four characters of image_text to leave us with just the text in the caption without any HTML.
			This makes it possible to enclose it in a <figcaption> later on without having stray HTML breaking it */
			if !image_caption.is_empty() {
				image_caption = &image_caption[1..image_caption.len() - 4];
			}

			// image_url contains > at the end of it, and right above this we remove image_text's front >, leaving us with just a single > between them
			let image_to_replace = format!("<p><a href=\"{image_url}{image_caption}</a></p>");

			/* We don't want to show a caption that's just the image's link, so we check if we find a Reddit preview link within the image's caption.
			If we don't find one we must have actual text, so we include a <figcaption> block that contains it.
			Otherwise we don't include the <figcaption> block as we don't need it. */
			let _image_replacement = if REDDIT_PREVIEW_REGEX.find(image_caption).is_none() {
				// Without this " would show as \" instead. "\&quot;" is how the quotes are formatted within image_text beforehand
				format!(
					"<figure><a href=\"{image_url}<img loading=\"lazy\" src=\"{image_url}</a><figcaption>{}</figcaption></figure>",
					image_caption.replace("\\&quot;", "\"")
				)
			} else {
				format!("<figure><a href=\"{image_url}<img loading=\"lazy\" src=\"{image_url}</a></figure>")
			};

			/* In order to know if we're dealing with a normal or external preview we need to take a look at the first capture group of REDDIT_PREVIEW_REGEX
			if it's preview we're dealing with something that needs /preview/pre, external-preview is /preview/external-pre, and i is /img */
			let reddit_preview_regex_capture = REDDIT_PREVIEW_REGEX.captures(&text1).unwrap().get(1).map_or("", |m| m.as_str());

			let _preview_type = match reddit_preview_regex_capture {
				"preview" => "/preview/pre",
				"external-preview" => "/preview/external-pre",
				_ => "/img",
			};

			text1 = REDDIT_PREVIEW_REGEX
				.replace(&text1, format!("{_preview_type}$2"))
				.replace(&image_to_replace, &_image_replacement)
		}
	}
}

fn extract_external_links(sources: &[&str]) -> Vec<ExternalLink> {
	let mut by_key = HashMap::new();

	for source in sources {
		if source.is_empty() {
			continue;
		}

		for capture in HTML_HREF_REGEX.captures_iter(source) {
			if let Some(link) = build_external_link(capture.get(1).map_or("", |m| m.as_str())) {
				by_key.entry(external_link_key(&link)).or_insert(link);
			}
		}

		for matched in PLAIN_URL_REGEX.find_iter(source) {
			if let Some(link) = build_external_link(matched.as_str()) {
				by_key.entry(external_link_key(&link)).or_insert(link);
			}
		}

		for matched in BARE_DOMAIN_REGEX.find_iter(source) {
			if let Some(link) = build_external_link(matched.as_str()) {
				by_key.entry(external_link_key(&link)).or_insert(link);
			}
		}
	}

	let mut links = by_key.into_values().collect::<Vec<_>>();
	links.sort_by(|a, b| a.label.cmp(&b.label));
	links
}

fn build_external_link(raw: &str) -> Option<ExternalLink> {
	build_external_link_with_label(raw, None)
}

fn build_external_link_with_label(raw: &str, label_override: Option<&str>) -> Option<ExternalLink> {
	let decoded = raw
		.trim()
		.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'))
		.replace("&amp;", "&");
	if decoded.is_empty() || decoded.starts_with('/') {
		return None;
	}

	let parsed = match Url::parse(&decoded) {
		Ok(parsed) => match parsed.scheme() {
			"http" | "https" => parsed,
			_ => return None,
		},
		Err(_) => Url::parse(&format!("https://{decoded}")).ok()?,
	};

	let host = parsed.host_str()?.trim().trim_matches('.').to_lowercase();
	if host.is_empty()
		|| matches!(
			host.as_str(),
			"reddit.com" | "www.reddit.com" | "old.reddit.com" | "new.reddit.com" | "np.reddit.com" | "redd.it"
		) {
		return None;
	}

	let label = label_override
		.map(str::trim)
		.filter(|label| !label.is_empty())
		.unwrap_or_else(|| host.trim_start_matches("www."))
		.to_string();

	Some(ExternalLink {
		label,
		url: parsed.into(),
		host: host.clone(),
		icon_path: format!("/site_icon/{host}"),
		has_coomer: false,
	})
}

fn append_external_links(target: &mut Vec<ExternalLink>, links: Vec<ExternalLink>) {
	let mut seen: HashSet<String> = target.iter().map(external_link_key).collect();

	for link in links {
		if seen.insert(external_link_key(&link)) {
			target.push(link);
		}
	}
}

pub async fn external_links_with_coomer_lookups(links: Vec<ExternalLink>) -> Vec<ExternalLink> {
	let mut expanded_links = links.clone();
	let mut coomer_links = Vec::new();

	for link in links {
		append_external_links(&mut coomer_links, coomer_lookup_links(&link).await);
	}

	append_external_links(&mut expanded_links, coomer_links);
	expanded_links
}

pub async fn external_links_with_coomer_flags(links: Vec<ExternalLink>) -> Vec<ExternalLink> {
	let mut pending = JoinSet::new();

	for (index, link) in links.iter().enumerate() {
		let link = link.clone();
		pending.spawn(async move {
			let has_coomer = !coomer_lookup_links(&link).await.is_empty();
			(index, has_coomer)
		});
	}

	let mut flagged_links = links;
	while let Some(result) = pending.join_next().await {
		let Ok((index, has_coomer)) = result else {
			continue;
		};
		if let Some(link) = flagged_links.get_mut(index) {
			link.has_coomer = has_coomer;
		}
	}

	flagged_links
}

fn dedupe_external_links_by_host(links: Vec<ExternalLink>) -> Vec<ExternalLink> {
	let mut seen = HashSet::new();
	let mut deduped = Vec::new();

	for link in links {
		if seen.insert(link.host.clone()) {
			deduped.push(link);
		}
	}

	deduped
}

fn external_link_key(link: &ExternalLink) -> String {
	format!("{}|{}", link.url.to_lowercase(), link.label.to_lowercase())
}

async fn coomer_lookup_links(link: &ExternalLink) -> Vec<ExternalLink> {
	coomer_lookup_links_with_checker(link, coomer_lookup_exists).await
}

async fn coomer_lookup_links_with_checker<F, Fut>(link: &ExternalLink, exists: F) -> Vec<ExternalLink>
where
	F: Fn(String) -> Fut + Copy,
	Fut: Future<Output = bool>,
{
	let mut matches = Vec::new();

	for candidate in coomer_lookup_candidates(link).await {
		if exists(candidate.url.clone()).await {
			matches.push(candidate);
		}
	}

	matches
}

async fn coomer_lookup_candidates(link: &ExternalLink) -> Vec<ExternalLink> {
	if let Some(direct_link) = direct_coomer_lookup_link(link).await {
		return vec![direct_link];
	}

	coomer_lookup_candidates_from_links(nested_creator_links(link).await).await
}

async fn coomer_lookup_candidates_from_links(links: Vec<ExternalLink>) -> Vec<ExternalLink> {
	let mut candidates = Vec::new();

	for link in links {
		let Some(coomer_link) = direct_coomer_lookup_link(&link).await else {
			continue;
		};
		append_external_links(&mut candidates, vec![coomer_link]);
	}

	candidates
}

async fn nested_creator_links(link: &ExternalLink) -> Vec<ExternalLink> {
	let Some(host) = normalize_external_page_host(&link.url) else {
		return Vec::new();
	};
	if !matches!(host.as_str(), "linktr.ee" | "www.linktr.ee" | "allmylinks.com" | "www.allmylinks.com") {
		return Vec::new();
	}
	let Ok(html) = fetch_external_page_html(link.url.clone(), 4).await else {
		return Vec::new();
	};

	match host.as_str() {
		"linktr.ee" | "www.linktr.ee" => extract_linktree_creator_links(&html),
		"allmylinks.com" | "www.allmylinks.com" => extract_allmylinks_creator_links(&html),
		_ => Vec::new(),
	}
}

fn normalize_external_page_host(url: &str) -> Option<String> {
	let parsed = Url::parse(url).ok()?;
	let host = parsed.host_str()?.trim().trim_matches('.').to_lowercase();
	if host.is_empty() {
		return None;
	}
	Some(host)
}

#[cached(size = 1024, time = 10800, result = true)]
async fn fetch_external_page_html(url: String, redirects_left: usize) -> Result<String, String> {
	let mut current_url = url;
	let mut redirects_remaining = redirects_left;

	loop {
		let uri = current_url.parse::<Uri>().map_err(|err| err.to_string())?;
		let request = Request::builder()
			.method("GET")
			.uri(uri)
			.header("Accept", "text/html,*/*;q=0.8")
			.header("Accept-Encoding", "identity")
			.header("User-Agent", OAUTH_CLIENT.load_full().user_agent())
			.body(Body::empty())
			.map_err(|err| err.to_string())?;
		let response = CLIENT.request(request).await.map_err(|err| err.to_string())?;

		if response.status().is_redirection() && redirects_remaining > 0 {
			let Some(location) = response.headers().get(LOCATION).and_then(|value| value.to_str().ok()) else {
				return Err(format!("redirect response missing location for {current_url}"));
			};

			current_url = Url::parse(&current_url)
				.ok()
				.and_then(|base| base.join(location).ok())
				.map(|joined| joined.to_string())
				.unwrap_or_else(|| location.to_string());
			redirects_remaining -= 1;
			continue;
		}

		if !response.status().is_success() {
			return Err(format!("external page request failed with status {}", response.status()));
		}

		let body = to_bytes(response.into_body()).await.map_err(|err| err.to_string())?;
		return String::from_utf8(body.to_vec()).map_err(|err| err.to_string());
	}
}

fn extract_linktree_creator_links(html: &str) -> Vec<ExternalLink> {
	let Some(next_data) = LINKTREE_NEXT_DATA_REGEX.captures(html).and_then(|captures| captures.get(1)).map(|value| value.as_str()) else {
		return Vec::new();
	};
	let Ok(parsed) = serde_json::from_str::<Value>(next_data) else {
		return Vec::new();
	};
	let Ok(url_path) = JsonPath::parse("$..url") else {
		return Vec::new();
	};

	let mut links = Vec::new();
	for value in url_path.query(&parsed).all() {
		let Some(raw_url) = value.as_str() else {
			continue;
		};
		let Some(link) = build_external_link(raw_url) else {
			continue;
		};
		if is_creator_link_host(&link.host) {
			append_external_links(&mut links, vec![link]);
		}
	}

	links
}

fn extract_allmylinks_creator_links(html: &str) -> Vec<ExternalLink> {
	let mut links = Vec::new();

	for regex in [&ALLMYLINKS_DATA_X_URL_REGEX, &ALLMYLINKS_TITLE_URL_REGEX] {
		for capture in regex.captures_iter(html) {
			let Some(raw_url) = capture.get(1).map(|value| value.as_str()) else {
				continue;
			};
			let Some(link) = build_external_link(raw_url) else {
				continue;
			};
			if is_creator_link_host(&link.host) {
				append_external_links(&mut links, vec![link]);
			}
		}
	}

	links
}

fn is_creator_link_host(host: &str) -> bool {
	matches!(host, "onlyfans.com" | "www.onlyfans.com" | "fansly.com" | "www.fansly.com")
}

async fn direct_coomer_lookup_link(link: &ExternalLink) -> Option<ExternalLink> {
	let parsed = Url::parse(&link.url).ok()?;
	let host = parsed.host_str()?.trim().trim_matches('.').to_lowercase();
	let (service, profile_id) = match host.as_str() {
		"onlyfans.com" | "www.onlyfans.com" => ("onlyfans", coomer_profile_id(&parsed, false).await?),
		"fansly.com" | "www.fansly.com" => ("fansly", coomer_profile_id(&parsed, true).await?),
		_ => return None,
	};
	let label = match service {
		"onlyfans" => "Coomer (OnlyFans)",
		"fansly" => "Coomer (Fansly)",
		_ => return None,
	};

	Some(ExternalLink {
		label: label.to_string(),
		url: format!("https://coomer.st/{service}/user/{profile_id}"),
		host: "coomer.st".to_string(),
		icon_path: "/site_icon/coomer.st".to_string(),
		has_coomer: false,
	})
}

async fn coomer_profile_id(parsed: &Url, is_fansly: bool) -> Option<String> {
	let segments = parsed.path_segments()?.into_iter().filter(|segment| !segment.is_empty()).collect::<Vec<_>>();

	let profile_segment = match segments.as_slice() {
		["u" | "user" | "users" | "profile", profile, ..] => *profile,
		[profile, ..] if !is_fansly => *profile,
		[profile, ..] if is_fansly => {
			if is_numeric_identifier(profile) {
				*profile
			} else {
				return resolve_fansly_account_id(profile.to_string()).await;
			}
		}
		[] => return None,
		_ => return None,
	};

	let profile_id = profile_segment.trim();
	if profile_id.is_empty() {
		return None;
	}

	Some(profile_id.to_string())
}

fn is_numeric_identifier(value: &str) -> bool {
	!value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
}

#[cached(size = 2048, time = 10800)]
async fn resolve_fansly_account_id(username: String) -> Option<String> {
	let username = username.trim();
	if username.is_empty() {
		return None;
	}

	let mut url = Url::parse("https://apiv3.fansly.com/api/v1/account").ok()?;
	url.query_pairs_mut().append_pair("usernames", username);
	let user_agent = OAUTH_CLIENT.load_full().user_agent().to_string();
	let request = Request::builder()
		.method("GET")
		.uri(url.as_str())
		.header("Accept-Encoding", "identity")
		.header("User-Agent", user_agent)
		.body(Body::empty())
		.ok()?;
	let response = CLIENT.request(request).await.ok()?;
	if !response.status().is_success() {
		return None;
	}

	let body = hyper::body::to_bytes(response.into_body()).await.ok()?;
	let parsed: Value = serde_json::from_slice(&body).ok()?;
	parsed["response"]
		.as_array()
		.and_then(|accounts| accounts.first())
		.and_then(|account| account["id"].as_str())
		.map(|id| id.to_string())
}

#[cached(size = 2048, time = 10800)]
async fn coomer_lookup_exists(url: String) -> bool {
	let Some(api_url) = coomer_profile_api_url(&url) else {
		return false;
	};
	let user_agent = OAUTH_CLIENT.load_full().user_agent().to_string();
	let request = match Request::builder()
		.method("GET")
		.uri(&api_url)
		.header("Accept", "text/css")
		.header("Accept-Encoding", "identity")
		.header("User-Agent", &user_agent)
		.body(Body::empty())
	{
		Ok(request) => request,
		Err(_) => return false,
	};

	let Ok(response) = CLIENT.request(request).await else {
		return false;
	};
	response.status().is_success()
}

fn coomer_profile_api_url(url: &str) -> Option<String> {
	let parsed = Url::parse(url).ok()?;
	let host = parsed.host_str()?.trim().trim_matches('.').to_lowercase();
	if host != "coomer.st" && host != "www.coomer.st" {
		return None;
	}

	let segments = parsed.path_segments()?.filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
	let [service, "user", profile_id, ..] = segments.as_slice() else {
		return None;
	};

	Some(format!("https://coomer.st/api/v1/{service}/user/{profile_id}/profile"))
}

fn cached_author_links(name: &str) -> Option<Vec<ExternalLink>> {
	let author = name.trim();
	if author.is_empty() || author == "[deleted]" {
		return None;
	}

	let mut cache = AUTHOR_LINKS_CACHE.lock().ok()?;
	prune_author_links_cache(&mut cache);
	cache.get(author).map(|entry| entry.links.clone())
}

fn store_author_links(name: &str, links: Vec<ExternalLink>) {
	let author = name.trim();
	if author.is_empty() || author == "[deleted]" {
		return;
	}

	let Ok(mut cache) = AUTHOR_LINKS_CACHE.lock() else {
		return;
	};
	prune_author_links_cache(&mut cache);
	cache.insert(author.to_string(), CachedAuthorLinks { links, cached_at: Instant::now() });

	while cache.len() > AUTHOR_LINKS_CACHE_SIZE {
		let Some(oldest_key) = cache.iter().min_by_key(|(_, entry)| entry.cached_at).map(|(key, _)| key.clone()) else {
			break;
		};
		cache.remove(&oldest_key);
	}
}

fn prune_author_links_cache(cache: &mut HashMap<String, CachedAuthorLinks>) {
	let now = Instant::now();
	cache.retain(|_, entry| now.duration_since(entry.cached_at) <= AUTHOR_LINKS_CACHE_TTL);
}

async fn fetch_user_profile_page_html(name: &str) -> Result<String, String> {
	let url = format!("https://www.reddit.com/user/{name}/");
	let user_agent = OAUTH_CLIENT.load_full().user_agent().to_string();
	let request = Request::builder()
		.method("GET")
		.uri(&url)
		.header("Host", "www.reddit.com")
		.header("Accept-Encoding", "identity")
		.header("User-Agent", user_agent)
		.body(Body::empty())
		.map_err(|err| err.to_string())?;
	let response = CLIENT.request(request).await.map_err(|err| err.to_string())?;

	if !response.status().is_success() {
		return Err(format!("profile html request failed with status {}", response.status()));
	}

	let body = hyper::body::to_bytes(response.into_body()).await.map_err(|err| err.to_string())?;
	String::from_utf8(body.to_vec()).map_err(|err| err.to_string())
}

fn extract_profile_social_links(source: &str) -> Vec<ExternalLink> {
	let mut ordered_links = Vec::new();
	let mut seen = HashSet::new();

	for capture in PROFILE_SOCIAL_LINK_REGEX.captures_iter(source) {
		let Some(raw_context) = capture.get(1).map(|m| m.as_str()) else {
			continue;
		};
		let decoded = decode_html(raw_context).unwrap_or_else(|_| raw_context.to_string());
		let Ok(context) = serde_json::from_str::<RedditSocialLinkContext>(&decoded) else {
			continue;
		};
		let Some(link) = build_external_link_with_label(&context.social_link.url, context.social_link.name.as_deref()) else {
			continue;
		};
		if seen.insert(external_link_key(&link)) {
			ordered_links.push((context.social_link.position.unwrap_or(usize::MAX), link));
		}
	}

	ordered_links.sort_by_key(|(position, _)| *position);
	ordered_links.into_iter().map(|(_, link)| link).collect()
}

// These links all follow a pattern of "https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/SUBREDDIT_ID/RANDOM_FILENAME.png"
static REDDIT_EMOTE_LINK_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/(.*)"#).unwrap());

// These all follow a pattern of '"emote|SUBREDDIT_IT|NUMBER"', we want the number
static REDDIT_EMOTE_ID_NUMBER_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""emote\|.*\|(.*)""#).unwrap());

pub fn rewrite_emotes(media_metadata: &Value, comment: String) -> String {
	/* Create the paths we'll use to look for our data inside the json.
	Because we don't know the name of any given emote we use a wildcard to parse them. */
	let link_path = JsonPath::parse("$[*].s.u").expect("valid JSON Path");
	let id_path = JsonPath::parse("$[*].id").expect("valid JSON Path");
	let size_path = JsonPath::parse("$[*].s.y").expect("valid JSON Path");

	// Extract all of the results from those json paths
	let link_nodes = media_metadata.json_path(&link_path);
	let id_nodes = media_metadata.json_path(&id_path);

	// Initialize our vectors
	let mut id_vec = Vec::new();
	let mut link_vec = Vec::new();

	// Add the relevant data to each of our vectors so we can access it by number later
	for current_id in id_nodes {
		id_vec.push(current_id)
	}
	for current_link in link_nodes {
		link_vec.push(current_link)
	}

	/* Set index to the length of link_vec.
	This is one larger than we'll actually be looking at, but we correct that later */
	let mut index = link_vec.len();

	// Comment needs to be in scope for when we call rewrite_urls()
	let mut comment = comment;

	/* Loop until index hits zero.
	This also prevents us from trying to do anything on an empty vector */
	while index != 0 {
		/* Subtract 1 from index to get the real index we should be looking at.
		Then continue on each subsequent loop to continue until we hit the last entry in the vector.
		This is how we get this to deal with multiple emotes in a single message and properly replace each ID with it's link */
		index -= 1;

		// Convert our current index in id_vec into a string so we can search through it with regex
		let current_id = id_vec[index].to_string();

		/* The ID number can be multiple lengths, so we capture it with regex.
		We also want to only attempt anything when we get matches to avoid panicking */
		if let Some(id_capture) = REDDIT_EMOTE_ID_NUMBER_REGEX.captures(&current_id) {
			// Format the ID to include the colons it has in the comment text
			let id = format!(":{}:", &id_capture[1]);

			// Convert current link to string to search through it with the regex
			let link = link_vec[index].to_string();

			// Make sure we only do operations when we get matches, otherwise we panic when trying to access the first match
			if let Some(link_capture) = REDDIT_EMOTE_LINK_REGEX.captures(&link) {
				/* Reddit sends a size for the image based on whether it's alone or accompanied by text.
				It's a good idea and makes everything look nicer, so we'll do the same. */
				let size = media_metadata.json_path(&size_path).first().unwrap().to_string();

				// Replace the ID we found earlier in the comment with the respective image and it's link from the regex capture
				let to_replace_with = format!(
					"<img loading=\"lazy\" src=\"/emote/{} width=\"{size}\" height=\"{size}\" style=\"vertical-align:text-bottom\">",
					&link_capture[1]
				);

				// Inside the comment replace the ID we found with the string that will embed the image
				comment = comment.replace(&id, &to_replace_with);
			}
		}
	}

	// render bullet (unordered) lists
	comment = render_bullet_lists(&comment);

	// Call rewrite_urls() to transform any other Reddit links
	rewrite_urls(&comment)
}

/// Format vote count to a string that will be displayed.
/// Append `m` and `k` for millions and thousands respectively, and
/// round to the nearest tenth.
pub fn format_num(num: i64) -> (String, String) {
	let truncated = if num >= 1_000_000 || num <= -1_000_000 {
		format!("{:.1}m", num as f64 / 1_000_000.0)
	} else if num >= 1000 || num <= -1000 {
		format!("{:.1}k", num as f64 / 1_000.0)
	} else {
		num.to_string()
	};

	(truncated, num.to_string())
}

/// Parse a relative and absolute time from a UNIX timestamp
pub fn time(created: f64) -> (String, String) {
	let time = OffsetDateTime::from_unix_timestamp(created.round() as i64).unwrap_or(OffsetDateTime::UNIX_EPOCH);
	let now = OffsetDateTime::now_utc();
	let min = time.min(now);
	let max = time.max(now);
	let time_delta = max - min;

	// If the time difference is more than a month, show full date
	let mut rel_time = if time_delta > Duration::days(30) {
		time.format(format_description!("[month repr:short] [day] '[year repr:last_two]")).unwrap_or_default()
	// Otherwise, show relative date/time
	} else if time_delta.whole_days() > 0 {
		format!("{}d", time_delta.whole_days())
	} else if time_delta.whole_hours() > 0 {
		format!("{}h", time_delta.whole_hours())
	} else {
		format!("{}m", time_delta.whole_minutes())
	};

	if time_delta <= Duration::days(30) {
		if now < time {
			rel_time += " left";
		} else {
			rel_time += " ago";
		}
	}

	(
		rel_time,
		time
			.format(format_description!("[month repr:short] [day] [year], [hour]:[minute]:[second] UTC"))
			.unwrap_or_default(),
	)
}

/// val() function used to parse JSON from Reddit APIs
pub fn val(j: &Value, k: &str) -> String {
	j["data"][k].as_str().unwrap_or_default().to_string()
}

//
// NETWORKING
//

pub fn template(t: &impl Template) -> Response<Body> {
	Response::builder()
		.status(200)
		.header("content-type", "text/html")
		.body(t.render().unwrap_or_default().into())
		.unwrap_or_default()
}

pub fn redirect(path: &str) -> Response<Body> {
	Response::builder()
		.status(302)
		.header("content-type", "text/html")
		.header("Location", path)
		.body(format!("Redirecting to <a href=\"{path}\">{path}</a>...").into())
		.unwrap_or_default()
}

/// Renders a generic error landing page.
pub async fn error(req: Request<Body>, msg: &str) -> Result<Response<Body>, String> {
	error!("Error page rendered: {}", msg.split('|').next().unwrap_or_default());
	let url = req.uri().to_string();
	let body = ErrorTemplate {
		msg: msg.to_string(),
		prefs: Preferences::new(&req),
		ratelimit: RateLimitDisplay::from_error_message(msg),
		url,
	}
	.render()
	.unwrap_or_default();

	Ok(Response::builder().status(404).header("content-type", "text/html").body(body.into()).unwrap_or_default())
}

/// Renders a generic info landing page.
pub async fn info(req: Request<Body>, msg: &str) -> Result<Response<Body>, String> {
	let url = req.uri().to_string();
	let body = InfoTemplate {
		msg: msg.to_string(),
		prefs: Preferences::new(&req),
		url,
	}
	.render()
	.unwrap_or_default();

	Ok(Response::builder().status(200).header("content-type", "text/html").body(body.into()).unwrap_or_default())
}

/// Returns true if the config/env variable `REDLIB_SFW_ONLY` carries the
/// value `on`.
///
/// If this variable is set as such, the instance will operate in SFW-only
/// mode; all NSFW content will be filtered. Attempts to access NSFW
/// subreddits or posts or userpages for users Reddit has deemed NSFW will
/// be denied.
pub fn sfw_only() -> bool {
	match get_setting("REDLIB_SFW_ONLY") {
		Some(val) => val == "on",
		None => false,
	}
}

/// Returns true if the config/env variable REDLIB_ENABLE_RSS is set to "on".
/// If this variable is set as such, the instance will enable RSS feeds.
/// Otherwise, the instance will not provide RSS feeds.
pub fn enable_rss() -> bool {
	match get_setting("REDLIB_ENABLE_RSS") {
		Some(val) => val == "on",
		None => false,
	}
}

/// Returns true if the config/env variable `REDLIB_ROBOTS_DISABLE_INDEXING` carries the
/// value `on`.
///
/// If this variable is set as such, the instance will block all robots in robots.txt and
/// insert the noindex, nofollow meta tag on every page.
pub fn disable_indexing() -> bool {
	match get_setting("REDLIB_ROBOTS_DISABLE_INDEXING") {
		Some(val) => val == "on",
		None => false,
	}
}

/// Determines if a request should redirect to a NSFW landing gate.
pub fn should_be_nsfw_gated(req: &Request<Body>, _req_url: &str) -> bool {
	(setting(req, "show_nsfw") != "on") || sfw_only()
}

/// Renders the landing page for NSFW content when the user has not enabled
/// "show NSFW posts" in settings.
pub async fn nsfw_landing(req: Request<Body>, req_url: String) -> Result<Response<Body>, String> {
	let res_type: ResourceType;

	// Determine from the request URL if the resource is a subreddit, a user
	// page, or a post.
	let resource: String = if !req.param("name").unwrap_or_default().is_empty() {
		res_type = ResourceType::User;
		req.param("name").unwrap_or_default()
	} else if !req.param("id").unwrap_or_default().is_empty() {
		res_type = ResourceType::Post;
		req.param("id").unwrap_or_default()
	} else {
		res_type = ResourceType::Subreddit;
		req.param("sub").unwrap_or_default()
	};

	let body = NSFWLandingTemplate {
		res: resource,
		res_type,
		prefs: Preferences::new(&req),
		url: req_url,
	}
	.render()
	.unwrap_or_default();

	Ok(Response::builder().status(403).header("content-type", "text/html").body(body.into()).unwrap_or_default())
}

/// Returns the last (non-empty) segment of a path string
pub fn url_path_basename(path: &str) -> String {
	let url_result = Url::parse(format!("https://libredd.it/{path}").as_str());

	match url_result {
		Ok(mut url) => {
			url.path_segments_mut().unwrap().pop_if_empty();

			url.path_segments().unwrap().next_back().unwrap().to_string()
		}
		Err(_) => path.to_string(),
	}
}

/// Returns the URL of a post, as needed by RSS feeds
pub fn get_post_url(post: &Post) -> String {
	if let Some(out_url) = &post.out_url {
		// Handle cross post
		if out_url.starts_with("/r/") {
			format!("{}{}", config::get_setting("REDLIB_FULL_URL").unwrap_or_default(), out_url)
		} else {
			out_url.to_string()
		}
	} else {
		format!("{}{}", config::get_setting("REDLIB_FULL_URL").unwrap_or_default(), post.permalink)
	}
}

/// Helper function for comparing strings in templates
#[macro_export]
macro_rules! impl_template_str_eq {
	($type:ty) => {
		impl $type {
			pub fn str_eq(&self, a: &str, b: &str) -> bool {
				a == b
			}

			pub fn bool_to_str(&self, val: bool) -> &'static str {
				if val {
					"true"
				} else {
					"false"
				}
			}
		}
	};
}

impl_template_str_eq!(Comment);
impl_template_str_eq!(ErrorTemplate);
impl_template_str_eq!(InfoTemplate);
impl_template_str_eq!(NSFWLandingTemplate);
impl_template_str_eq!(User);

#[cfg(test)]
mod tests {
	use super::{
		coomer_lookup_candidates_from_links, coomer_lookup_links_with_checker, decode_flair_subscriptions, dedupe_external_links_by_host, encode_flair_subscriptions,
		extract_allmylinks_creator_links, extract_linktree_creator_links, extract_profile_social_links, format_num, format_url, format_video_duration, read_sharded_cookie,
		rewrite_urls, user_profile_details, write_sharded_cookie, ExternalLink, FlairSubscription, Preferences,
	};
	use futures_lite::future::block_on;
	use hyper::{Body, Request, Response};
	use serde_json::json;

	async fn always_exists(_: String) -> bool {
		true
	}

	#[test]
	fn format_num_works() {
		assert_eq!(format_num(567), ("567".to_string(), "567".to_string()));
		assert_eq!(format_num(1234), ("1.2k".to_string(), "1234".to_string()));
		assert_eq!(format_num(1999), ("2.0k".to_string(), "1999".to_string()));
		assert_eq!(format_num(1001), ("1.0k".to_string(), "1001".to_string()));
		assert_eq!(format_num(1_999_999), ("2.0m".to_string(), "1999999".to_string()));
	}

	#[test]
	fn rewrite_urls_removes_backslashes_and_rewrites_url() {
		assert_eq!(
			rewrite_urls(
				"<a href=\"https://new.reddit.com/r/linux%5C_gaming/comments/x/just%5C_a%5C_test%5C/\">https://new.reddit.com/r/linux\\_gaming/comments/x/just\\_a\\_test/</a>"
			),
			"<a href=\"/r/linux_gaming/comments/x/just_a_test/\">https://new.reddit.com/r/linux_gaming/comments/x/just_a_test/</a>"
		);
		assert_eq!(
			rewrite_urls(
				"e.g. &lt;a href=\"https://www.reddit.com/r/linux%5C_gaming/comments/ql9j15/anyone%5C_else%5C_confused%5C_with%5C_linus%5C_linux%5C_issues/\"&gt;https://www.reddit.com/r/linux\\_gaming/comments/ql9j15/anyone\\_else\\_confused\\_with\\_linus\\_linux\\_issues/&lt;/a&gt;"
			),
			"e.g. &lt;a href=\"/r/linux_gaming/comments/ql9j15/anyone_else_confused_with_linus_linux_issues/\"&gt;https://www.reddit.com/r/linux_gaming/comments/ql9j15/anyone_else_confused_with_linus_linux_issues/&lt;/a&gt;"
		);
	}

	#[test]
	fn rewrite_urls_keeps_intentional_backslashes() {
		assert_eq!(
			rewrite_urls("printf \"\\npolkit.addRule(function(action, subject)"),
			"printf \"\\npolkit.addRule(function(action, subject)"
		);
	}

	#[test]
	fn extracts_external_links_from_user_profile_details() {
		let profile = user_profile_details(&json!({
			"public_description": "Find me elsewhere https://paa.ge/example",
			"public_description_html": "<div class=\"md\"><p>Also on https://reddit.com/u/example</p></div>",
			"description": "https://fansly.com/example",
			"description_html": "<div class=\"md\"><p><a href=\"https://fansly.com/example\">Fansly</a> <a href=\"https://www.reddit.com/u/example\">Reddit</a></p></div>"
		}));

		assert_eq!(profile.blurb, "Find me elsewhere https://paa.ge/example");
		assert_eq!(profile.external_links.len(), 2);
		assert_eq!(profile.external_links[0].host, "fansly.com");
		assert_eq!(profile.external_links[0].icon_path, "/site_icon/fansly.com");
		assert_eq!(profile.external_links[1].host, "paa.ge");
		assert_eq!(profile.external_links[1].icon_path, "/site_icon/paa.ge");
	}

	#[test]
	fn extracts_bare_domain_links_from_blurb() {
		let profile = user_profile_details(&json!({
			"public_description": "Supporters get more at realankri.com 😏",
			"public_description_html": null,
			"description": "",
			"description_html": null
		}));

		assert_eq!(profile.external_links.len(), 1);
		assert_eq!(profile.external_links[0].host, "realankri.com");
		assert_eq!(profile.external_links[0].url, "https://realankri.com/");
	}

	#[test]
	fn extracts_structured_social_links_from_profile_html() {
		let profile_html = r#"
			<faceplate-tracker source="profile" action="click" noun="social_link" data-faceplate-tracking-context="{&quot;social_link&quot;:{&quot;type&quot;:&quot;CUSTOM&quot;,&quot;url&quot;:&quot;https://onlyfans.com/lady-katarina&quot;,&quot;name&quot;:&quot;DM ME HERE&quot;,&quot;position&quot;:0}}">
			<faceplate-tracker source="profile" action="click" noun="social_link" data-faceplate-tracking-context="{&quot;social_link&quot;:{&quot;type&quot;:&quot;ONLYFANS&quot;,&quot;url&quot;:&quot;https://onlyfans.com/lady-katarina&quot;,&quot;name&quot;:&quot;lady-katarina&quot;,&quot;position&quot;:1}}">
			<faceplate-tracker source="profile" action="click" noun="social_link" data-faceplate-tracking-context="{&quot;social_link&quot;:{&quot;type&quot;:&quot;CUSTOM&quot;,&quot;url&quot;:&quot;https://fansly.com/LadyKatarina&quot;,&quot;name&quot;:&quot;Fansly - DM ME&quot;,&quot;position&quot;:2}}">
			<faceplate-tracker source="profile" action="click" noun="social_link" data-faceplate-tracking-context="{&quot;social_link&quot;:{&quot;type&quot;:&quot;TWITTER&quot;,&quot;url&quot;:&quot;https://twitter.com/Lady_Kaatarina&quot;,&quot;name&quot;:&quot;Lady_Kaatarina&quot;,&quot;position&quot;:3}}">
		"#;

		let links = extract_profile_social_links(profile_html);

		assert_eq!(links.len(), 4);
		assert_eq!(links[0].label, "DM ME HERE");
		assert_eq!(links[1].label, "lady-katarina");
		assert_eq!(links[2].host, "fansly.com");
		assert_eq!(links[3].host, "twitter.com");
	}

	#[test]
	fn deduplicates_author_links_by_host() {
		let deduped = dedupe_external_links_by_host(vec![
			ExternalLink {
				label: "DM ME HERE".to_string(),
				url: "https://onlyfans.com/lady-katarina".to_string(),
				host: "onlyfans.com".to_string(),
				icon_path: "/site_icon/onlyfans.com".to_string(),
				has_coomer: false,
			},
			ExternalLink {
				label: "lady-katarina".to_string(),
				url: "https://onlyfans.com/lady-katarina".to_string(),
				host: "onlyfans.com".to_string(),
				icon_path: "/site_icon/onlyfans.com".to_string(),
				has_coomer: false,
			},
			ExternalLink {
				label: "Fansly - DM ME".to_string(),
				url: "https://fansly.com/LadyKatarina".to_string(),
				host: "fansly.com".to_string(),
				icon_path: "/site_icon/fansly.com".to_string(),
				has_coomer: false,
			},
		]);

		assert_eq!(deduped.len(), 2);
		assert_eq!(deduped[0].host, "onlyfans.com");
		assert_eq!(deduped[1].host, "fansly.com");
	}

	#[test]
	fn maps_onlyfans_and_fansly_profiles_to_coomer_lookup_urls() {
		let onlyfans = block_on(coomer_lookup_links_with_checker(
			&ExternalLink {
				label: "lady-katarina".to_string(),
				url: "https://onlyfans.com/lady-katarina".to_string(),
				host: "onlyfans.com".to_string(),
				icon_path: "/site_icon/onlyfans.com".to_string(),
				has_coomer: false,
			},
			always_exists,
		))
		.into_iter()
		.next()
		.expect("onlyfans profile should map to coomer");
		let fansly = block_on(coomer_lookup_links_with_checker(
			&ExternalLink {
				label: "Fansly".to_string(),
				url: "https://fansly.com/user/640681284019953664".to_string(),
				host: "fansly.com".to_string(),
				icon_path: "/site_icon/fansly.com".to_string(),
				has_coomer: false,
			},
			always_exists,
		))
		.into_iter()
		.next()
		.expect("fansly profile should map to coomer");

		assert_eq!(onlyfans.url, "https://coomer.st/onlyfans/user/lady-katarina");
		assert_eq!(fansly.url, "https://coomer.st/fansly/user/640681284019953664");
	}

	#[test]
	fn ignores_non_profile_links_when_building_coomer_lookups() {
		let expanded = block_on(coomer_lookup_links_with_checker(
			&ExternalLink {
				label: "Twitter".to_string(),
				url: "https://twitter.com/example".to_string(),
				host: "twitter.com".to_string(),
				icon_path: "/site_icon/twitter.com".to_string(),
				has_coomer: false,
			},
			always_exists,
		));

		assert!(expanded.is_empty());
	}

	#[test]
	fn extracts_onlyfans_and_fansly_links_from_linktree_next_data() {
		let html = r#"
			<script id="__NEXT_DATA__" type="application/json">
				{"props":{"pageProps":{"contentWarning":"generic","activeGates":["age"],"links":[
					{"url":"https://onlyfans.com/example"},
					{"url":"https://fansly.com/user/640681284019953664"},
					{"url":"https://twitter.com/example"},
					{"url":"https://onlyfans.com/example"}
				]}}}
			</script>
		"#;

		let links = extract_linktree_creator_links(html);

		assert_eq!(links.len(), 2);
		assert_eq!(links[0].host, "onlyfans.com");
		assert_eq!(links[1].host, "fansly.com");
	}

	#[test]
	fn extracts_hidden_adult_links_from_allmylinks_markup() {
		let html = r#"
			<div class="sensitive-links-wrapper">
				<div class="adult-hidden">
					<a data-x-url="https://fansly.com/ms_dira" title="https://fansly.com/ms_dira"></a>
					<a title="https://onlyfans.com/example"></a>
					<a data-x-url="https://twitter.com/example"></a>
				</div>
			</div>
		"#;

		let links = extract_allmylinks_creator_links(html);

		assert_eq!(links.len(), 2);
		assert_eq!(links[0].host, "fansly.com");
		assert_eq!(links[1].host, "onlyfans.com");
	}

	#[test]
	fn expands_multiple_nested_creator_links_to_multiple_coomer_profiles() {
		let coomer_links = block_on(coomer_lookup_candidates_from_links(vec![
			ExternalLink {
				label: "lady-katarina".to_string(),
				url: "https://onlyfans.com/lady-katarina".to_string(),
				host: "onlyfans.com".to_string(),
				icon_path: "/site_icon/onlyfans.com".to_string(),
				has_coomer: false,
			},
			ExternalLink {
				label: "Fansly".to_string(),
				url: "https://fansly.com/user/640681284019953664".to_string(),
				host: "fansly.com".to_string(),
				icon_path: "/site_icon/fansly.com".to_string(),
				has_coomer: false,
			},
		]));

		assert_eq!(coomer_links.len(), 2);
		assert_eq!(coomer_links[0].url, "https://coomer.st/onlyfans/user/lady-katarina");
		assert_eq!(coomer_links[1].url, "https://coomer.st/fansly/user/640681284019953664");
	}

	#[test]
	fn test_format_url() {
		assert_eq!(format_url("https://a.thumbs.redditmedia.com/XYZ.jpg"), "/thumb/a/XYZ.jpg");
		assert_eq!(format_url("https://emoji.redditmedia.com/a/b"), "/emoji/a/b");

		assert_eq!(
			format_url("https://external-preview.redd.it/foo.jpg?auto=webp&s=bar"),
			"/preview/external-pre/foo.jpg?auto=webp&s=bar"
		);

		assert_eq!(format_url("https://i.redd.it/foobar.jpg"), "/img/foobar.jpg");
		assert_eq!(
			format_url("https://preview.redd.it/qwerty.jpg?auto=webp&s=asdf"),
			"/preview/pre/qwerty.jpg?auto=webp&s=asdf"
		);
		assert_eq!(format_url("https://v.redd.it/foo/DASH_360.mp4?source=fallback"), "/vid/foo/360.mp4");
		assert_eq!(
			format_url("https://v.redd.it/foo/HLSPlaylist.m3u8?a=bar&v=1&f=sd"),
			"/hls/foo/HLSPlaylist.m3u8?a=bar&v=1&f=sd"
		);
		assert_eq!(format_url("https://www.redditstatic.com/gold/awards/icon/icon.png"), "/static/gold/awards/icon/icon.png");
		assert_eq!(
			format_url("https://www.redditstatic.com/marketplace-assets/v1/core/emotes/snoomoji_emotes/free_emotes_pack/shrug.gif"),
			"/static/marketplace-assets/v1/core/emotes/snoomoji_emotes/free_emotes_pack/shrug.gif"
		);
		assert_eq!(format_url("https://www.redgifs.com/watch/zanyjudiciousrook"), "/redgifs/zanyjudiciousrook");
		assert_eq!(format_url("https://www.redgifs.com/ifr/zanyjudiciousrook"), "/redgifs/zanyjudiciousrook");
		assert_eq!(format_url("https://v3.redgifs.com/watch/zanyjudiciousrook?foo=bar"), "/redgifs/zanyjudiciousrook");

		assert_eq!(format_url(""), "");
		assert_eq!(format_url("self"), "");
		assert_eq!(format_url("default"), "");
		assert_eq!(format_url("nsfw"), "");
		assert_eq!(format_url("spoiler"), "");
	}

	#[test]
	fn formats_video_durations() {
		assert_eq!(format_video_duration(0), "");
		assert_eq!(format_video_duration(9), "0:09");
		assert_eq!(format_video_duration(59), "0:59");
		assert_eq!(format_video_duration(60), "1:00");
		assert_eq!(format_video_duration(3671), "1:01:11");
	}

	#[test]
	fn serialize_prefs() {
		let prefs = Preferences {
			available_themes: vec![],
			theme: "laserwave".to_owned(),
			front_page: "default".to_owned(),
			layout: "compact".to_owned(),
			wide: "on".to_owned(),
			blur_spoiler: "on".to_owned(),
			show_nsfw: "off".to_owned(),
			blur_nsfw: "on".to_owned(),
			hide_hls_notification: "off".to_owned(),
			image_quality: "auto".to_owned(),
			video_quality: "best".to_owned(),
			hide_sidebar_and_summary: "off".to_owned(),
			use_dash: "on".to_owned(),
			use_hls: "on".to_owned(),
			autoplay_videos: "on".to_owned(),
			video_previews: "on".to_owned(),
			fixed_navbar: "on".to_owned(),
			disable_visit_reddit_confirmation: "on".to_owned(),
			comment_sort: "confidence".to_owned(),
			post_sort: "top".to_owned(),
			subscriptions: vec!["memes".to_owned(), "mildlyinteresting".to_owned()],
			filters: vec![],
			flair_subscriptions: vec![],
			hide_awards: "off".to_owned(),
			hide_score: "off".to_owned(),
			remove_default_feeds: "off".to_owned(),
		};
		let urlencoded = serde_urlencoded::to_string(prefs).expect("Failed to serialize Prefs");

		assert_eq!(urlencoded, "theme=laserwave&front_page=default&layout=compact&wide=on&blur_spoiler=on&show_nsfw=off&blur_nsfw=on&hide_hls_notification=off&image_quality=auto&video_quality=best&hide_sidebar_and_summary=off&use_dash=on&use_hls=on&autoplay_videos=on&video_previews=on&fixed_navbar=on&disable_visit_reddit_confirmation=on&comment_sort=confidence&post_sort=top&subscriptions=memes%2Bmildlyinteresting&filters=&flair_subscriptions=&hide_awards=off&hide_score=off&remove_default_feeds=off");
	}

	#[test]
	fn flair_subscriptions_round_trip_through_encoded_string() {
		let subscriptions = vec![
			FlairSubscription {
				subreddit: "rust".to_string(),
				flair: "News".to_string(),
			},
			FlairSubscription {
				subreddit: "programming".to_string(),
				flair: "Question / Help".to_string(),
			},
		];

		let encoded = encode_flair_subscriptions(&subscriptions).unwrap();
		let decoded = decode_flair_subscriptions(&encoded).unwrap();

		assert_eq!(decoded, subscriptions);
	}

	#[test]
	fn sharded_subscription_cookies_are_readable_by_client_scripts() {
		let req = Request::builder().body(Body::empty()).unwrap();
		let mut response = Response::builder().body(Body::empty()).unwrap();

		write_sharded_cookie(&mut response, &req, "subscriptions", "rust+u_spez");

		let set_cookie_headers = response
			.headers()
			.get_all("Set-Cookie")
			.iter()
			.map(|value| value.to_str().unwrap_or_default().to_string())
			.collect::<Vec<_>>();

		assert!(!set_cookie_headers.is_empty());
		assert!(set_cookie_headers.iter().all(|value| !value.contains("HttpOnly")));
		assert!(set_cookie_headers.iter().any(|value| value.starts_with("subscriptions=rust+u_spez")));
	}

	#[test]
	fn read_sharded_cookie_decodes_percent_encoded_chunks() {
		let req = Request::builder()
			.header("Cookie", "subscriptions=rust%2Bu_spez; subscriptions1=%2Bprogramming")
			.body(Body::empty())
			.unwrap();

		assert_eq!(read_sharded_cookie(&req, "subscriptions"), "rust+u_spez+programming");
	}
}

#[test]
fn test_rewriting_emoji() {
	let input = r#"<div class="md"><p>How can you have such hard feelings towards a license? <img src="https://www.redditstatic.com/marketplace-assets/v1/core/emotes/snoomoji_emotes/free_emotes_pack/shrug.gif" width="20" height="20" style="vertical-align:middle"> Let people use what license they want, and BSD is one of the least restrictive ones AFAIK.</p>"#;
	let output = r#"<div class="md"><p>How can you have such hard feelings towards a license? <img src="/static/marketplace-assets/v1/core/emotes/snoomoji_emotes/free_emotes_pack/shrug.gif" width="20" height="20" style="vertical-align:middle"> Let people use what license they want, and BSD is one of the least restrictive ones AFAIK.</p>"#;
	assert_eq!(rewrite_urls(input), output);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fetching_subreddit_quarantined() {
	let subreddit = Post::fetch("/r/drugs", true).await;
	assert!(subreddit.is_ok());
	assert!(!subreddit.unwrap().0.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fetching_nsfw_subreddit() {
	// Gonwild is a place for closed, Euclidean Geometric shapes to exchange their nth terms for karma; showing off their edges in a comfortable environment without pressure.
	// Find a good sub that is tagged NSFW but that actually isn't in case my future employers are watching (they probably are)
	// switched from randnsfw as it is no longer functional.
	let subreddit = Post::fetch("/r/gonwild", false).await;
	assert!(subreddit.is_ok());
	assert!(!subreddit.unwrap().0.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_fetching_ws() {
	let subreddit = Post::fetch("/r/popular", false).await;
	assert!(subreddit.is_ok());
	for post in subreddit.unwrap().0 {
		assert!(post.ws_url.is_empty() || post.ws_url.starts_with("wss://"));
	}
}

#[test]
fn test_rewriting_image_links() {
	let input =
		r#"<p><a href="https://preview.redd.it/6awags382xo31.png?width=2560&amp;format=png&amp;auto=webp&amp;s=9c563aed4f07a91bdd249b5a3cea43a79710dcfc">caption 1</a></p>"#;
	let output = r#"<figure><a href="/preview/pre/6awags382xo31.png?width=2560&amp;format=png&amp;auto=webp&amp;s=9c563aed4f07a91bdd249b5a3cea43a79710dcfc"><img loading="lazy" src="/preview/pre/6awags382xo31.png?width=2560&amp;format=png&amp;auto=webp&amp;s=9c563aed4f07a91bdd249b5a3cea43a79710dcfc"></a><figcaption>caption 1</figcaption></figure>"#;
	assert_eq!(rewrite_urls(input), output);
}

#[test]
fn test_url_path_basename() {
	// without trailing slash
	assert_eq!(url_path_basename("/first/last"), "last");
	// with trailing slash
	assert_eq!(url_path_basename("/first/last/"), "last");
	// with query parameters
	assert_eq!(url_path_basename("/first/last/?some=query"), "last");
	// file path
	assert_eq!(url_path_basename("/cdn/image.jpg"), "image.jpg");
	// when a full url is passed instead of just a path
	assert_eq!(url_path_basename("https://doma.in/first/last"), "last");
	// empty path
	assert_eq!(url_path_basename("/"), "");
}

#[test]
fn test_rewriting_emotes() {
	let json_input = serde_json::from_str(r#"{"emote|t5_31hpy|2028":{"e":"Image","id":"emote|t5_31hpy|2028","m":"image/png","s":{"u":"https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/t5_31hpy/PW6WsOaLcd.png","x":60,"y":60},"status":"valid","t":"sticker"}}"#).expect("Valid JSON");
	let comment_input = r#"<div class="comment_body "><div class="md"><p>:2028:</p></div></div>"#;
	let output = r#"<div class="comment_body "><div class="md"><p><img loading="lazy" src="/emote/t5_31hpy/PW6WsOaLcd.png" width="60" height="60" style="vertical-align:text-bottom"></p></div></div>"#;
	assert_eq!(rewrite_emotes(&json_input, comment_input.to_string()), output);
}

#[test]
fn test_rewriting_bullet_list() {
	let input = r#"<div class="md"><p>Hi, I&#39;ve bought this very same monitor and found no calibration whatsoever. I have an ICC profile that has been set up since I&#39;ve installed its driver from the LG website and it works ok. I also used <a href="http://www.lagom.nl/lcd-test/">http://www.lagom.nl/lcd-test/</a> to calibrate it. After some good tinkering I&#39;ve found the following settings + the color profile from the driver gets me past all the tests perfectly:
- Brightness 50 (still have to settle on this one, it&#39;s personal preference, it controls the backlight, not the colors)
- Contrast 70 (which for me was the default one)
- Picture mode Custom
- Super resolution + Off (it looks horrible anyway)
- Sharpness 50 (default one I think)
- Black level High (low messes up gray colors)
- DFC Off
- Response Time Middle (personal preference, <a href="https://www.blurbusters.com/">https://www.blurbusters.com/</a> show horrible overdrive with it on high)
- Freesync doesn&#39;t matter
- Black stabilizer 50
- Gamma setting on 0
- Color Temp Medium
How`s your monitor by the way? Any IPS bleed whatsoever? I either got lucky or the panel is pretty good, 0 bleed for me, just the usual IPS glow. How about the pixels? I see the pixels even at one meter away, especially on Microsoft Edge&#39;s icon for example, the blue background is just blocky, don&#39;t know why.</p>
</div>"#;
	let output = r#"<div class="md"><p>Hi, I&#39;ve bought this very same monitor and found no calibration whatsoever. I have an ICC profile that has been set up since I&#39;ve installed its driver from the LG website and it works ok. I also used <a href="http://www.lagom.nl/lcd-test/">http://www.lagom.nl/lcd-test/</a> to calibrate it. After some good tinkering I&#39;ve found the following settings + the color profile from the driver gets me past all the tests perfectly:
<ul><li>Brightness 50 (still have to settle on this one, it&#39;s personal preference, it controls the backlight, not the colors)</li><li>Contrast 70 (which for me was the default one)</li><li>Picture mode Custom</li><li>Super resolution + Off (it looks horrible anyway)</li><li>Sharpness 50 (default one I think)</li><li>Black level High (low messes up gray colors)</li><li>DFC Off</li><li>Response Time Middle (personal preference, <a href="https://www.blurbusters.com/">https://www.blurbusters.com/</a> show horrible overdrive with it on high)</li><li>Freesync doesn&#39;t matter</li><li>Black stabilizer 50</li><li>Gamma setting on 0</li><li>Color Temp Medium</li></ul>
How`s your monitor by the way? Any IPS bleed whatsoever? I either got lucky or the panel is pretty good, 0 bleed for me, just the usual IPS glow. How about the pixels? I see the pixels even at one meter away, especially on Microsoft Edge&#39;s icon for example, the blue background is just blocky, don&#39;t know why.</p>
</div>"#;

	assert_eq!(render_bullet_lists(input), output);
}

#[test]
fn test_default_prefs_serialization_loop_json() {
	let prefs = Preferences::default();
	let serialized = serde_json::to_string(&prefs).unwrap();
	let deserialized: Preferences = serde_json::from_str(&serialized).unwrap();
	assert_eq!(prefs, deserialized);
}

#[test]
fn test_default_prefs_serialization_loop_bincode() {
	let prefs = Preferences::default();
	test_round_trip(&prefs, false);
	test_round_trip(&prefs, true);
}

static KNOWN_GOOD_CONFIGS: &[&str] = &[
	"ఴӅβØØҞÉဏႢձĬ༧ȒʯऌԔӵ୮༏",
	"ਧՊΥÀÃǎƱГ۸ඣമĖฤ႙ʟาúໜϾௐɥঀĜໃહཞઠѫҲɂఙ࿔ǲઉƲӟӻĻฅΜδ໖ԜǗဖငƦơ৶Ą௩ԹʛใЛʃශаΏ",
	"ਧԩΥÀÃÎŠ౭൩ඔႠϼҭöҪƸռઇԾॐნɔາǒՍҰच௨ಖມŃЉŐདƦ๙ϩএఠȝഽйʮჯඒϰळՋ௮ສ৵ऎΦѧਹಧଟƙŃ३î༦ŌပղयƟแҜ།",
];

#[test]
fn test_known_good_configs_deserialization() {
	for config in KNOWN_GOOD_CONFIGS {
		let bytes = base2048::decode(config).unwrap();
		let decompressed = deflate_decompress(bytes).unwrap();
		assert!(preferences_from_bincode(&decompressed).is_ok());
	}
}

#[test]
fn test_known_good_configs_full_round_trip() {
	for config in KNOWN_GOOD_CONFIGS {
		let bytes = base2048::decode(config).unwrap();
		let decompressed = deflate_decompress(bytes).unwrap();
		let prefs = preferences_from_bincode(&decompressed).unwrap();
		test_round_trip(&prefs, false);
		test_round_trip(&prefs, true);
	}
}

fn test_round_trip(input: &Preferences, compression: bool) {
	let serialized = bincode::serialize(input).unwrap();
	let compressed = if compression { deflate_compress(serialized).unwrap() } else { serialized };
	let decompressed = if compression { deflate_decompress(compressed).unwrap() } else { compressed };
	let deserialized = preferences_from_bincode(&decompressed).unwrap();
	assert_eq!(*input, deserialized);
}
