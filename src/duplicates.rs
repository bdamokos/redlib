//! Handler for post duplicates.

use crate::client::json;
use crate::server::RequestExt;
use crate::subreddit::{can_access_quarantine, quarantine};
use crate::utils::{self, error, filter_posts, nsfw_landing, parse_post, template, Author, Awards, Flair, Post, Preferences};

use askama::Template;
use hyper::{Body, Request, Response};
use serde_json::Value;
use std::borrow::ToOwned;
use std::collections::HashSet;
use std::vec::Vec;

/// `DuplicatesParams` contains the parameters in the URL.
#[derive(PartialEq)]
struct DuplicatesParams {
	before: String,
	after: String,
	sort: String,
}

/// `DuplicatesTemplate` defines an Askama template for rendering duplicate
/// posts.
#[derive(Template)]
#[template(path = "duplicates.html")]
pub struct DuplicatesTemplate {
	shell_mode: bool,
	fragment_url: String,
	loading_message: String,
	/// params contains the relevant request parameters.
	params: DuplicatesParams,

	/// post is the post whose ID is specified in the reqeust URL. Note that
	/// this is not necessarily the "original" post.
	post: Post,

	/// duplicates is the list of posts that, per Reddit, are duplicates of
	/// Post above.
	duplicates: Vec<Post>,

	/// prefs are the user preferences.
	prefs: Preferences,

	/// url is the request URL.
	url: String,

	/// num_posts_filtered counts how many posts were filtered from the
	/// duplicates list.
	num_posts_filtered: u64,

	/// all_posts_filtered is true if every duplicate was filtered. This is an
	/// edge case but can still happen.
	all_posts_filtered: bool,
}

crate::impl_template_str_eq!(DuplicatesTemplate);

#[derive(Template)]
#[template(path = "duplicates_fragment.html")]
struct DuplicatesFragmentTemplate {
	params: DuplicatesParams,
	post: Post,
	duplicates: Vec<Post>,
	prefs: Preferences,
	url: String,
	num_posts_filtered: u64,
	all_posts_filtered: bool,
}

crate::impl_template_str_eq!(DuplicatesFragmentTemplate);

fn empty_post_for_shell(community: &str, permalink: &str) -> Post {
	Post {
		id: String::new(),
		kind: "t3".to_string(),
		title: "Loading duplicates...".to_string(),
		community: community.to_string(),
		body: String::new(),
		author: Author {
			name: String::new(),
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
		permalink: permalink.to_string(),
		parent_kind: String::new(),
		link_id: String::new(),
		link_title: String::new(),
		link_permalink: String::new(),
		poll: None,
		score: ("0".to_string(), "0".to_string()),
		upvote_ratio: 0,
		post_type: "self".to_string(),
		flair: Flair {
			flair_parts: Vec::new(),
			text: String::new(),
			background_color: String::new(),
			foreground_color: String::new(),
		},
		flags: crate::utils::Flags::new(false, false, false),
		thumbnail: crate::utils::Media::empty(),
		media: crate::utils::Media::empty(),
		domain: String::new(),
		rel_time: String::new(),
		created: String::new(),
		created_ts: 0,
		num_duplicates: 0,
		comments: ("0".to_string(), "0".to_string()),
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

fn duplicates_fragment_path(req: &Request<Body>) -> String {
	let query = req.uri().query().unwrap_or_default();
	if query.is_empty() {
		format!("/duplicates-fragment{}", req.uri().path())
	} else {
		format!("/duplicates-fragment{}?{query}", req.uri().path())
	}
}

fn duplicates_source_path(req: &Request<Body>) -> String {
	req.uri().path().replacen("/duplicates-fragment", "", 1)
}

/// Make the GET request to Reddit. It assumes `req` is the appropriate Reddit
/// REST endpoint for enumerating post duplicates.
pub async fn item(req: Request<Body>) -> Result<Response<Body>, String> {
	let community = req.param("sub").unwrap_or_default();
	let url = req.uri().to_string();
	Ok(template(&DuplicatesTemplate {
		shell_mode: true,
		fragment_url: duplicates_fragment_path(&req),
		loading_message: "Loading duplicates...".to_string(),
		params: DuplicatesParams {
			before: String::new(),
			after: String::new(),
			sort: String::new(),
		},
		post: empty_post_for_shell(&community, &url),
		duplicates: Vec::new(),
		prefs: Preferences::new(&req),
		url,
		num_posts_filtered: 0,
		all_posts_filtered: false,
	}))
}

pub async fn item_fragment(req: Request<Body>) -> Result<Response<Body>, String> {
	let source_path = duplicates_source_path(&req);
	let path: String = format!("{}.json?{}&raw_json=1", source_path, req.uri().query().unwrap_or_default());
	let sub = req.param("sub").unwrap_or_default();
	let quarantined = can_access_quarantine(&req, &sub);

	// Log the request in debugging mode
	#[cfg(debug_assertions)]
	req.param("id").unwrap_or_default();

	// Send the GET, and await JSON.
	match json(path, quarantined).await {
		// Process response JSON.
		Ok(response) => {
			let mut post = parse_post(&response[0]["data"]["children"][0]).await;
			post.author.external_links = utils::cached_author_external_links(&post.author.name);

			let req_url = req.uri().to_string();
			// Return landing page if this post if this Reddit deems this post
			// NSFW, but we have also disabled the display of NSFW content
			// or if the instance is SFW-only
			if post.nsfw && crate::utils::should_be_nsfw_gated(&req, &req_url) {
				return Ok(nsfw_landing(req, req_url).await.unwrap_or_default());
			}

			let prefs = Preferences::new(&req);
			let filters: HashSet<String> = prefs.filters.iter().cloned().collect();
			let (duplicates, num_posts_filtered, all_posts_filtered) = parse_duplicates(&response[1], &filters).await;

			// These are the values for the "before=", "after=", and "sort="
			// query params, respectively.
			let mut before: String = String::new();
			let mut after: String = String::new();
			let mut sort: String = String::new();

			// FIXME: We have to perform a kludge to work around a Reddit API
			// bug.
			//
			// The JSON object in "data" will never contain a "before" value so
			// it is impossible to use it to determine our position in a
			// listing. We'll make do by getting the ID of the first post in
			// the listing, setting that as our "before" value, and ask Reddit
			// to give us a batch of duplicate posts up to that post.
			//
			// Likewise, if we provide a "before" request in the GET, the
			// result won't have an "after" in the JSON, in addition to missing
			// the "before." So we will have to use the final post in the list
			// of duplicates.
			//
			// That being said, we'll also need to capture the value of the
			// "sort=" parameter as well, so we will need to inspect the
			// query key-value pairs anyway.
			let l = duplicates.len();
			if l > 0 {
				// This gets set to true if "before=" is one of the GET params.
				let mut have_before: bool = false;

				// This gets set to true if "after=" is one of the GET params.
				let mut have_after: bool = false;

				// Inspect the query key-value pairs. We will need to record
				// the value of "sort=", along with checking to see if either
				// one of "before=" or "after=" are given.
				//
				// If we're in the middle of the batch (evidenced by the
				// presence of a "before=" or "after=" parameter in the GET),
				// then use the first post as the "before" reference.
				//
				// We'll do this iteratively. Better than with .map_or()
				// since a closure will continue to operate on remaining
				// elements even after we've determined one of "before=" or
				// "after=" (or both) are in the GET request.
				//
				// In practice, here should only ever be one of "before=" or
				// "after=" and never both.
				let query_str = req.uri().query().unwrap_or_default().to_string();

				if !query_str.is_empty() {
					for param in query_str.split('&') {
						let kv: Vec<&str> = param.split('=').collect();
						if kv.len() < 2 {
							// Reject invalid query parameter.
							continue;
						}

						let key: &str = kv[0];
						match key {
							"before" => have_before = true,
							"after" => have_after = true,
							"sort" => {
								let val: &str = kv[1];
								match val {
									"new" | "num_comments" => sort = val.to_string(),
									_ => {}
								}
							}
							_ => {}
						}
					}
				}

				if have_after {
					"t3_".clone_into(&mut before);
					before.push_str(&duplicates[0].id);
				}

				// Address potentially missing "after". If "before=" is in the
				// GET, then "after" will be null in the JSON (see FIXME
				// above).
				if have_before {
					// The next batch will need to start from one after the
					// last post in the current batch.
					"t3_".clone_into(&mut after);
					after.push_str(&duplicates[l - 1].id);

					// Here is where things get terrible. Notice that we
					// haven't set `before`. In order to do so, we will
					// need to know if there is a batch that exists before
					// this one, and doing so requires actually fetching the
					// previous batch. In other words, we have to do yet one
					// more GET to Reddit. There is no other way to determine
					// whether or not to define `before`.
					//
					// We'll mitigate that by requesting at most one duplicate.
					let new_path: String = format!(
						"{}.json?before=t3_{}&sort={}&limit=1&raw_json=1",
						req.uri().path(),
						&duplicates[0].id,
						if sort.is_empty() { "num_comments".to_string() } else { sort.clone() }
					);
					match json(new_path, true).await {
						Ok(response) => {
							if !response[1]["data"]["children"].as_array().unwrap_or(&Vec::new()).is_empty() {
								"t3_".clone_into(&mut before);
								before.push_str(&duplicates[0].id);
							}
						}
						Err(msg) => {
							// Abort entirely if we couldn't get the previous
							// batch.
							return error(req, &msg).await;
						}
					}
				} else {
					after = response[1]["data"]["after"].as_str().unwrap_or_default().to_string();
				}
			}

			let body = DuplicatesFragmentTemplate {
				params: DuplicatesParams { before, after, sort },
				post,
				duplicates,
				prefs,
				url: req_url,
				num_posts_filtered,
				all_posts_filtered,
			}
			.render()
			.map_err(|error| error.to_string())?;
			Response::builder()
				.status(200)
				.header("content-type", "text/html; charset=utf-8")
				.header("cache-control", "no-store")
				.body(Body::from(body))
				.map_err(|error| error.to_string())
		}

		// Process error.
		Err(msg) => {
			if msg == "quarantined" || msg == "gated" {
				let sub = req.param("sub").unwrap_or_default();
				Ok(quarantine(&req, sub, &msg))
			} else {
				error(req, &msg).await
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{duplicates_fragment_path, duplicates_source_path, DuplicatesParams, DuplicatesTemplate};
	use crate::utils::{Author, Awards, Flair, Post, Preferences};
	use askama::Template;
	use hyper::{Body, Request};

	fn test_post() -> Post {
		Post {
			id: String::new(),
			kind: "t3".to_string(),
			title: "Loading duplicates...".to_string(),
			community: "rust".to_string(),
			body: String::new(),
			author: Author {
				name: String::new(),
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
			permalink: "/r/rust/duplicates/abc/example".to_string(),
			parent_kind: String::new(),
			link_id: String::new(),
			link_title: String::new(),
			link_permalink: String::new(),
			poll: None,
			score: ("0".to_string(), "0".to_string()),
			upvote_ratio: 0,
			post_type: "self".to_string(),
			flair: Flair {
				flair_parts: Vec::new(),
				text: String::new(),
				background_color: String::new(),
				foreground_color: String::new(),
			},
			flags: crate::utils::Flags::new(false, false, false),
			thumbnail: crate::utils::Media::empty(),
			media: crate::utils::Media::empty(),
			domain: String::new(),
			rel_time: String::new(),
			created: String::new(),
			created_ts: 0,
			num_duplicates: 0,
			comments: ("0".to_string(), "0".to_string()),
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

	fn test_prefs() -> Preferences {
		Preferences {
			available_themes: vec!["system".to_string()],
			layout: "card".to_string(),
			fixed_navbar: "on".to_string(),
			..Preferences::default()
		}
	}

	#[test]
	fn builds_duplicates_fragment_path() {
		let req = Request::builder().uri("/r/rust/duplicates/abc/example?sort=new").body(Body::empty()).unwrap();
		assert_eq!(duplicates_fragment_path(&req), "/duplicates-fragment/r/rust/duplicates/abc/example?sort=new");
	}

	#[test]
	fn duplicates_source_path_removes_fragment_prefix() {
		let req = Request::builder()
			.uri("/duplicates-fragment/r/rust/duplicates/abc/example?sort=new")
			.body(Body::empty())
			.unwrap();
		assert_eq!(duplicates_source_path(&req), "/r/rust/duplicates/abc/example");
	}

	#[test]
	fn renders_shell_mode_duplicates_placeholder_with_fragment_url() {
		let html = DuplicatesTemplate {
			shell_mode: true,
			fragment_url: "/duplicates-fragment/r/rust/duplicates/abc/example".to_string(),
			loading_message: "Loading duplicates...".to_string(),
			params: DuplicatesParams {
				before: String::new(),
				after: String::new(),
				sort: String::new(),
			},
			post: test_post(),
			duplicates: Vec::new(),
			prefs: test_prefs(),
			url: "/r/rust/duplicates/abc/example".to_string(),
			num_posts_filtered: 0,
			all_posts_filtered: false,
		}
		.render()
		.unwrap();

		assert!(html.contains("data-fragment-url=\"/duplicates-fragment/r/rust/duplicates/abc/example\""));
		assert!(html.contains("Loading duplicates..."));
	}
}

// DUPLICATES
async fn parse_duplicates(json: &Value, filters: &HashSet<String>) -> (Vec<Post>, u64, bool) {
	let post_duplicates: &Vec<Value> = &json["data"]["children"].as_array().map_or(Vec::new(), ToOwned::to_owned);
	let mut duplicates: Vec<Post> = Vec::new();

	// Process each post and place them in the Vec<Post>.
	for val in post_duplicates {
		let mut post: Post = parse_post(val).await;
		post.author.external_links = utils::cached_author_external_links(&post.author.name);
		duplicates.push(post);
	}

	let (num_posts_filtered, all_posts_filtered) = filter_posts(&mut duplicates, filters);
	utils::hydrate_post_author_links_from_cache(&mut duplicates);
	(duplicates, num_posts_filtered, all_posts_filtered)
}
