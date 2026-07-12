use crate::server::RequestExt;
use crate::utils::{Flair, Post};
use askama::Template;
use hyper::{Body, Request, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex};

const REVISION_SCHEMA: &[u8] = b"stable-post-v1";
const MAX_POST_ID_LEN: usize = 24;
const MAX_CANONICAL_POSTS: usize = 10_000;
const MAX_REVISION_BYTES: usize = 192 * 1024 * 1024;
const REVISIONS_PER_POST: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PostId(String);

impl PostId {
	pub fn parse(value: &str) -> Option<Self> {
		let value = value.trim().to_ascii_lowercase();
		(!value.is_empty() && value.len() <= MAX_POST_ID_LEN && value.bytes().all(|byte| byte.is_ascii_alphanumeric())).then_some(Self(value))
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl fmt::Display for PostId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.0)
	}
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentRevision(String);

impl ContentRevision {
	pub fn parse(value: &str) -> Option<Self> {
		let value = value.trim().to_ascii_lowercase();
		(value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(Self(value))
	}

	fn from_bytes(bytes: &[u8]) -> Self {
		let digest = Sha256::digest(bytes);
		Self(digest.iter().map(|byte| format!("{byte:02x}")).collect())
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

impl fmt::Display for ContentRevision {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.0)
	}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObservationSource {
	#[default]
	Persisted,
	Scrape,
	Peer,
	Listing,
	Direct,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostAvailability {
	#[default]
	Available,
	RemovedByModerator,
	DeletedByAuthor,
	TemporarilyUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SanitizedHtml(String);

impl SanitizedHtml {
	fn from_reddit_html(value: &str) -> Self {
		Self(ammonia::Builder::default().clean(value).to_string())
	}

	pub fn as_str(&self) -> &str {
		&self.0
	}
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StablePollOptionV1 {
	pub id: u64,
	pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableGalleryItemV1 {
	pub url: String,
	pub width: i64,
	pub height: i64,
	pub caption: String,
	pub outbound_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StableMediaV1 {
	None,
	Link { url: String, domain: String },
	Image { url: String, width: i64, height: i64 },
	Gallery { items: Vec<StableGalleryItemV1> },
	Volatile { kind: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StablePostContentV1 {
	pub id: PostId,
	pub title: String,
	pub body: SanitizedHtml,
	pub author_name: String,
	pub author_flair: Flair,
	pub author_distinguished: String,
	pub community: String,
	pub permalink: String,
	pub created_ts: u64,
	pub flair: Flair,
	pub nsfw: bool,
	pub spoiler: bool,
	pub availability: PostAvailability,
	pub media: StableMediaV1,
	pub poll_options: Vec<StablePollOptionV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostLiveV1 {
	pub score_display: String,
	pub score_raw: String,
	pub upvote_ratio: i64,
	pub comment_count_display: String,
	pub comment_count_raw: String,
	pub duplicate_count: u64,
	pub stickied: bool,
	pub observed_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerPostStateV1 {
	pub bookmarked: bool,
	pub bookmark_new_comments: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct PostObservation {
	pub stable: StablePostContentV1,
	pub live: PostLiveV1,
	pub source: ObservationSource,
	pub source_key: String,
	pub observed_at: u64,
}

#[derive(Clone, Debug)]
pub struct CanonicalPostSnapshot {
	pub stable: Arc<StablePostContentV1>,
	pub revision: ContentRevision,
	pub live: PostLiveV1,
	pub source: ObservationSource,
	pub source_key: String,
	pub stable_observed_at: u64,
	pub live_observed_at: u64,
}

#[derive(Default)]
struct CanonicalPostStore {
	latest: HashMap<PostId, CanonicalPostSnapshot>,
	revisions: HashMap<(PostId, ContentRevision), Arc<StablePostContentV1>>,
	revision_order: VecDeque<(PostId, ContentRevision)>,
	revision_bytes: usize,
}

static CANONICAL_POSTS: LazyLock<Mutex<CanonicalPostStore>> = LazyLock::new(|| Mutex::new(CanonicalPostStore::default()));

#[derive(Template)]
#[template(path = "post_content_card.html")]
struct StablePostCardTemplate<'a> {
	post: &'a StablePostContentV1,
	revision: &'a str,
}

pub async fn card(req: Request<Body>) -> Result<Response<Body>, String> {
	if req.uri().query().is_some() {
		return not_found();
	}
	let Some(post_id) = req.param("id") else {
		return not_found();
	};
	let Some(revision) = req.param("revision") else {
		return not_found();
	};
	let Some(post) = exact(&post_id, &revision) else {
		return not_found();
	};
	let body = StablePostCardTemplate { post: &post, revision: &revision }.render().map_err(|error| error.to_string())?;

	Response::builder()
		.status(200)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "public, max-age=31536000, s-maxage=86400, immutable")
		.header("etag", format!("\"{revision}-card-v1\""))
		.header("x-redlib-cache-policy", "immutable-post-content")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

fn not_found() -> Result<Response<Body>, String> {
	Response::builder()
		.status(404)
		.header("content-type", "text/plain; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from("Post content revision not found"))
		.map_err(|error| error.to_string())
}

impl StablePostContentV1 {
	pub fn from_post(post: &Post) -> Option<Self> {
		if post.kind != "t3" {
			return None;
		}
		let id = PostId::parse(&post.id)?;
		let media = match post.post_type.as_str() {
			"image" if is_stable_local_media_url(&post.media.url) => StableMediaV1::Image {
				url: post.media.url.clone(),
				width: post.media.width,
				height: post.media.height,
			},
			"gallery" => StableMediaV1::Gallery {
				items: post
					.gallery
					.iter()
					.filter(|item| is_stable_local_media_url(&item.url))
					.map(|item| StableGalleryItemV1 {
						url: item.url.clone(),
						width: item.width,
						height: item.height,
						caption: item.caption.clone(),
						outbound_url: item.outbound_url.clone(),
					})
					.collect(),
			},
			"link" => StableMediaV1::Link {
				url: post.out_url.clone().filter(|url| is_safe_link_url(url)).unwrap_or_else(|| post.media.url.clone()),
				domain: post.domain.clone(),
			},
			"self" => StableMediaV1::None,
			kind => StableMediaV1::Volatile { kind: kind.to_string() },
		};
		let poll_options = post
			.poll
			.as_ref()
			.map(|poll| {
				poll
					.poll_options
					.iter()
					.map(|option| StablePollOptionV1 {
						id: option.id,
						text: option.text.clone(),
					})
					.collect()
			})
			.unwrap_or_default();

		Some(Self {
			id,
			title: post.title.clone(),
			body: SanitizedHtml::from_reddit_html(&post.body),
			author_name: post.author.name.clone(),
			author_flair: post.author.flair.clone(),
			author_distinguished: post.author.distinguished.clone(),
			community: post.community.clone(),
			permalink: post.permalink.clone(),
			created_ts: post.created_ts,
			flair: post.flair.clone(),
			nsfw: post.flags.nsfw,
			spoiler: post.flags.spoiler,
			availability: PostAvailability::Available,
			media,
			poll_options,
		})
	}

	pub fn body_html(&self) -> &str {
		self.body.as_str()
	}

	pub fn is_image(&self) -> bool {
		matches!(self.media, StableMediaV1::Image { .. })
	}

	pub fn image_url(&self) -> &str {
		match &self.media {
			StableMediaV1::Image { url, .. } => url,
			_ => "",
		}
	}

	pub fn image_width(&self) -> i64 {
		match &self.media {
			StableMediaV1::Image { width, .. } => *width,
			_ => 0,
		}
	}

	pub fn image_height(&self) -> i64 {
		match &self.media {
			StableMediaV1::Image { height, .. } => *height,
			_ => 0,
		}
	}

	pub fn gallery(&self) -> &[StableGalleryItemV1] {
		match &self.media {
			StableMediaV1::Gallery { items } => items,
			_ => &[],
		}
	}

	pub fn link(&self) -> Option<(&str, &str)> {
		match &self.media {
			StableMediaV1::Link { url, domain } => Some((url, domain)),
			_ => None,
		}
	}

	pub fn volatile_media_kind(&self) -> Option<&str> {
		match &self.media {
			StableMediaV1::Volatile { kind } => Some(kind),
			_ => None,
		}
	}

	pub fn revision(&self) -> ContentRevision {
		let mut canonical = Vec::new();
		push_bytes(&mut canonical, REVISION_SCHEMA);
		push_str(&mut canonical, self.id.as_str());
		push_str(&mut canonical, &self.title);
		push_str(&mut canonical, self.body.as_str());
		push_str(&mut canonical, &self.author_name);
		push_flair(&mut canonical, &self.author_flair);
		push_str(&mut canonical, &self.author_distinguished);
		push_str(&mut canonical, &self.community);
		push_str(&mut canonical, &self.permalink);
		canonical.extend_from_slice(&self.created_ts.to_be_bytes());
		push_flair(&mut canonical, &self.flair);
		canonical.push(u8::from(self.nsfw));
		canonical.push(u8::from(self.spoiler));
		canonical.push(self.availability as u8);
		push_media(&mut canonical, &self.media);
		canonical.extend_from_slice(&(self.poll_options.len() as u64).to_be_bytes());
		for option in &self.poll_options {
			canonical.extend_from_slice(&option.id.to_be_bytes());
			push_str(&mut canonical, &option.text);
		}
		ContentRevision::from_bytes(&canonical)
	}

	fn completeness_score(&self) -> usize {
		usize::from(!self.title.is_empty())
			+ usize::from(!self.body.as_str().is_empty()) * 2
			+ usize::from(!self.author_name.is_empty())
			+ usize::from(!self.community.is_empty())
			+ usize::from(!self.permalink.is_empty())
			+ usize::from(!matches!(self.media, StableMediaV1::None | StableMediaV1::Volatile { .. })) * 2
	}

	fn estimated_bytes(&self) -> usize {
		self.title.len()
			+ self.body.as_str().len()
			+ self.author_name.len()
			+ self.community.len()
			+ self.permalink.len()
			+ estimated_flair_bytes(&self.author_flair)
			+ estimated_flair_bytes(&self.flair)
			+ match &self.media {
				StableMediaV1::None => 0,
				StableMediaV1::Link { url, domain } => url.len() + domain.len(),
				StableMediaV1::Image { url, .. } => url.len(),
				StableMediaV1::Gallery { items } => items.iter().map(|item| item.url.len() + item.caption.len() + item.outbound_url.len()).sum(),
				StableMediaV1::Volatile { kind } => kind.len(),
			}
	}
}

impl PostLiveV1 {
	pub fn from_post(post: &Post, observed_at: u64) -> Self {
		Self {
			score_display: post.score.0.clone(),
			score_raw: post.score.1.clone(),
			upvote_ratio: post.upvote_ratio,
			comment_count_display: post.comments.0.clone(),
			comment_count_raw: post.comments.1.clone(),
			duplicate_count: post.num_duplicates,
			stickied: post.flags.stickied,
			observed_at,
		}
	}
}

impl ViewerPostStateV1 {
	pub fn from_post(post: &Post) -> Self {
		Self {
			bookmarked: post.bookmarked,
			bookmark_new_comments: post.bookmark_new_comments,
		}
	}
}

impl PostObservation {
	pub fn from_post(post: &Post, source: ObservationSource, source_key: impl Into<String>, observed_at: u64) -> Option<Self> {
		Some(Self {
			stable: StablePostContentV1::from_post(post)?,
			live: PostLiveV1::from_post(post, observed_at),
			source,
			source_key: source_key.into(),
			observed_at,
		})
	}
}

pub fn observe_post(post: &Post, source: ObservationSource, source_key: impl Into<String>, observed_at: u64) -> Option<CanonicalPostSnapshot> {
	let observation = PostObservation::from_post(post, source, source_key, observed_at)?;
	Some(observe(observation))
}

pub fn observe_posts(posts: &[Post], source: ObservationSource, source_key: impl Into<String>, observed_at: u64) {
	let source_key = source_key.into();
	for post in posts {
		let _ = observe_post(post, source, source_key.clone(), observed_at);
	}
}

pub fn observe(observation: PostObservation) -> CanonicalPostSnapshot {
	let mut store = CANONICAL_POSTS.lock().unwrap_or_else(|error| error.into_inner());
	let id = observation.stable.id.clone();
	let (merged_stable, stable_source, stable_source_key, stable_observed_at) = match store.latest.get(&id) {
		Some(existing) => {
			let (stable, incoming_wins) = merge_stable(existing, &observation);
			if incoming_wins {
				(stable, observation.source, observation.source_key.clone(), observation.observed_at)
			} else {
				(stable, existing.source, existing.source_key.clone(), existing.stable_observed_at)
			}
		}
		None => (observation.stable.clone(), observation.source, observation.source_key.clone(), observation.observed_at),
	};
	let revision = merged_stable.revision();
	let stable = Arc::new(merged_stable);
	insert_revision(&mut store, id.clone(), revision.clone(), stable.clone());

	let (live, live_observed_at) = match store.latest.get(&id) {
		Some(existing) if existing.live_observed_at > observation.observed_at => (existing.live.clone(), existing.live_observed_at),
		_ => (observation.live.clone(), observation.observed_at),
	};
	let snapshot = CanonicalPostSnapshot {
		stable,
		revision,
		live,
		source: stable_source,
		source_key: stable_source_key,
		stable_observed_at,
		live_observed_at,
	};
	store.latest.insert(id, snapshot.clone());
	prune_store(&mut store);
	snapshot
}

pub fn latest(post_id: &str) -> Option<CanonicalPostSnapshot> {
	let id = PostId::parse(post_id)?;
	CANONICAL_POSTS.lock().ok()?.latest.get(&id).cloned()
}

pub fn exact(post_id: &str, revision: &str) -> Option<Arc<StablePostContentV1>> {
	let id = PostId::parse(post_id)?;
	let revision = ContentRevision::parse(revision)?;
	CANONICAL_POSTS.lock().ok()?.revisions.get(&(id, revision)).cloned()
}

/// Ensure a rendered submission has an exact local stable revision and return
/// the revision used by its private context wrapper.
pub fn revision_for_post(post: &Post) -> String {
	observe_post(post, ObservationSource::Listing, "render", crate::cache_persistence::unix_now())
		.map(|snapshot| snapshot.revision.to_string())
		.unwrap_or_default()
}

fn merge_stable(existing: &CanonicalPostSnapshot, observation: &PostObservation) -> (StablePostContentV1, bool) {
	let mut merged = existing.stable.as_ref().clone();
	let incoming = &observation.stable;
	let incoming_is_authoritative = observation.source >= ObservationSource::Direct;
	let incoming_is_preferred = observation_outranks(existing, observation);

	if !incoming_is_preferred {
		fill_missing_stable_fields(&mut merged, incoming);
		return (merged, false);
	}
	merge_string(&mut merged.title, &incoming.title, incoming_is_authoritative);
	if !incoming.body.as_str().is_empty() || incoming_is_authoritative {
		merged.body = incoming.body.clone();
	}
	merge_string(&mut merged.author_name, &incoming.author_name, incoming_is_authoritative);
	if !incoming.author_flair.flair_parts.is_empty() || incoming_is_authoritative {
		merged.author_flair = incoming.author_flair.clone();
	}
	merge_string(&mut merged.author_distinguished, &incoming.author_distinguished, incoming_is_authoritative);
	merge_string(&mut merged.community, &incoming.community, incoming_is_authoritative);
	merge_string(&mut merged.permalink, &incoming.permalink, incoming_is_authoritative);
	if incoming.created_ts > 0 || incoming_is_authoritative {
		merged.created_ts = incoming.created_ts;
	}
	if !incoming.flair.flair_parts.is_empty() || incoming_is_authoritative {
		merged.flair = incoming.flair.clone();
	}
	merged.nsfw = incoming.nsfw;
	merged.spoiler = incoming.spoiler;
	merged.availability = incoming.availability;
	if !matches!(incoming.media, StableMediaV1::None | StableMediaV1::Volatile { .. }) || incoming_is_authoritative {
		merged.media = incoming.media.clone();
	}
	if !incoming.poll_options.is_empty() || incoming_is_authoritative {
		merged.poll_options = incoming.poll_options.clone();
	}
	(merged, true)
}

fn observation_outranks(existing: &CanonicalPostSnapshot, incoming: &PostObservation) -> bool {
	if incoming.source != existing.source {
		return incoming.source > existing.source;
	}
	if incoming.observed_at != existing.stable_observed_at {
		return incoming.observed_at > existing.stable_observed_at;
	}
	if incoming.stable.completeness_score() != existing.stable.completeness_score() {
		return incoming.stable.completeness_score() > existing.stable.completeness_score();
	}
	if incoming.source_key != existing.source_key {
		return incoming.source_key < existing.source_key;
	}
	incoming.stable.revision().as_str() > existing.revision.as_str()
}

fn fill_missing_stable_fields(current: &mut StablePostContentV1, incoming: &StablePostContentV1) {
	fill_missing_string(&mut current.title, &incoming.title);
	if current.body.as_str().is_empty() && !incoming.body.as_str().is_empty() {
		current.body = incoming.body.clone();
	}
	fill_missing_string(&mut current.author_name, &incoming.author_name);
	if current.author_flair.flair_parts.is_empty() && !incoming.author_flair.flair_parts.is_empty() {
		current.author_flair = incoming.author_flair.clone();
	}
	fill_missing_string(&mut current.author_distinguished, &incoming.author_distinguished);
	fill_missing_string(&mut current.community, &incoming.community);
	fill_missing_string(&mut current.permalink, &incoming.permalink);
	if current.created_ts == 0 {
		current.created_ts = incoming.created_ts;
	}
	if current.flair.flair_parts.is_empty() && !incoming.flair.flair_parts.is_empty() {
		current.flair = incoming.flair.clone();
	}
	if matches!(current.media, StableMediaV1::None | StableMediaV1::Volatile { .. }) && !matches!(incoming.media, StableMediaV1::None | StableMediaV1::Volatile { .. }) {
		current.media = incoming.media.clone();
	}
	if current.poll_options.is_empty() && !incoming.poll_options.is_empty() {
		current.poll_options = incoming.poll_options.clone();
	}
}

fn fill_missing_string(current: &mut String, incoming: &str) {
	if current.is_empty() && !incoming.is_empty() {
		*current = incoming.to_string();
	}
}

fn merge_string(current: &mut String, incoming: &str, authoritative: bool) {
	if !incoming.is_empty() || authoritative {
		*current = incoming.to_string();
	}
}

fn insert_revision(store: &mut CanonicalPostStore, id: PostId, revision: ContentRevision, stable: Arc<StablePostContentV1>) {
	let key = (id.clone(), revision.clone());
	if store.revisions.contains_key(&key) {
		return;
	}
	store.revision_bytes = store.revision_bytes.saturating_add(stable.estimated_bytes());
	store.revisions.insert(key.clone(), stable);
	store.revision_order.push_back(key);

	let same_post = store.revision_order.iter().filter(|(post_id, _)| post_id == &id).cloned().collect::<Vec<_>>();
	let excess = same_post.len().saturating_sub(REVISIONS_PER_POST);
	for stale in same_post.into_iter().take(excess) {
		remove_revision(store, &stale);
	}
}

fn prune_store(store: &mut CanonicalPostStore) {
	while store.latest.len() > MAX_CANONICAL_POSTS || store.revision_bytes > MAX_REVISION_BYTES {
		let Some(oldest) = store.revision_order.front().cloned() else {
			break;
		};
		remove_revision(store, &oldest);
		if !store.revisions.keys().any(|(post_id, _)| post_id == &oldest.0) {
			store.latest.remove(&oldest.0);
		}
	}
}

fn remove_revision(store: &mut CanonicalPostStore, key: &(PostId, ContentRevision)) {
	if let Some(stable) = store.revisions.remove(key) {
		store.revision_bytes = store.revision_bytes.saturating_sub(stable.estimated_bytes());
	}
	store.revision_order.retain(|candidate| candidate != key);
}

fn is_stable_local_media_url(url: &str) -> bool {
	url.starts_with('/') && !url.starts_with("//")
}

fn is_safe_link_url(url: &str) -> bool {
	url.starts_with("https://") || url.starts_with("http://") || is_stable_local_media_url(url)
}

fn push_bytes(target: &mut Vec<u8>, value: &[u8]) {
	target.extend_from_slice(&(value.len() as u64).to_be_bytes());
	target.extend_from_slice(value);
}

fn push_str(target: &mut Vec<u8>, value: &str) {
	push_bytes(target, value.as_bytes());
}

fn push_flair(target: &mut Vec<u8>, flair: &Flair) {
	push_str(target, &flair.text);
	push_str(target, &flair.background_color);
	push_str(target, &flair.foreground_color);
	target.extend_from_slice(&(flair.flair_parts.len() as u64).to_be_bytes());
	for part in &flair.flair_parts {
		push_str(target, &part.flair_part_type);
		push_str(target, &part.value);
	}
}

fn push_media(target: &mut Vec<u8>, media: &StableMediaV1) {
	match media {
		StableMediaV1::None => target.push(0),
		StableMediaV1::Link { url, domain } => {
			target.push(1);
			push_str(target, url);
			push_str(target, domain);
		}
		StableMediaV1::Image { url, width, height } => {
			target.push(2);
			push_str(target, url);
			target.extend_from_slice(&width.to_be_bytes());
			target.extend_from_slice(&height.to_be_bytes());
		}
		StableMediaV1::Gallery { items } => {
			target.push(3);
			target.extend_from_slice(&(items.len() as u64).to_be_bytes());
			for item in items {
				push_str(target, &item.url);
				target.extend_from_slice(&item.width.to_be_bytes());
				target.extend_from_slice(&item.height.to_be_bytes());
				push_str(target, &item.caption);
				push_str(target, &item.outbound_url);
			}
		}
		StableMediaV1::Volatile { kind } => {
			target.push(4);
			push_str(target, kind);
		}
	}
}

fn estimated_flair_bytes(flair: &Flair) -> usize {
	flair.text.len()
		+ flair.background_color.len()
		+ flair.foreground_color.len()
		+ flair.flair_parts.iter().map(|part| part.flair_part_type.len() + part.value.len()).sum::<usize>()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::utils::{Flags, GalleryMedia, Media};

	fn sample_post() -> Post {
		Post {
			id: "abc123".to_string(),
			title: "Stable title".to_string(),
			body: "<p>Hello <strong>world</strong></p>".to_string(),
			community: "rust".to_string(),
			permalink: "/r/rust/comments/abc123/stable_title/".to_string(),
			created_ts: 1_700_000_000,
			flags: Flags::new(false, false, false),
			media: Media {
				url: "/img/example.png".to_string(),
				width: 640,
				height: 480,
				..Media::empty()
			},
			post_type: "image".to_string(),
			..Post::default()
		}
	}

	#[test]
	fn live_and_viewer_changes_do_not_change_revision() {
		let first = sample_post();
		let mut second = first.clone();
		second.score = ("9.9k".to_string(), "9999".to_string());
		second.comments = ("42".to_string(), "42".to_string());
		second.bookmarked = true;
		second.bookmark_new_comments = Some(3);
		assert_eq!(
			StablePostContentV1::from_post(&first).unwrap().revision(),
			StablePostContentV1::from_post(&second).unwrap().revision()
		);
	}

	#[test]
	fn stable_changes_create_a_new_revision() {
		let first = sample_post();
		let mut second = first.clone();
		second.body = "<p>Edited body</p>".to_string();
		assert_ne!(
			StablePostContentV1::from_post(&first).unwrap().revision(),
			StablePostContentV1::from_post(&second).unwrap().revision()
		);
	}

	#[test]
	fn gallery_order_is_part_of_revision() {
		let mut first = sample_post();
		first.post_type = "gallery".to_string();
		first.gallery = vec![
			GalleryMedia {
				url: "/img/a.png".to_string(),
				preview_url: String::new(),
				preview_srcset: String::new(),
				width: 1,
				height: 1,
				caption: "a".to_string(),
				outbound_url: String::new(),
			},
			GalleryMedia {
				url: "/img/b.png".to_string(),
				preview_url: String::new(),
				preview_srcset: String::new(),
				width: 1,
				height: 1,
				caption: "b".to_string(),
				outbound_url: String::new(),
			},
		];
		let mut second = first.clone();
		second.gallery.reverse();
		assert_ne!(
			StablePostContentV1::from_post(&first).unwrap().revision(),
			StablePostContentV1::from_post(&second).unwrap().revision()
		);
	}

	#[test]
	fn invalid_ids_are_rejected() {
		assert!(PostId::parse("abc123").is_some());
		assert!(PostId::parse("../abc").is_none());
		assert!(PostId::parse("").is_none());
	}

	fn snapshot_from(observation: &PostObservation) -> CanonicalPostSnapshot {
		let stable = Arc::new(observation.stable.clone());
		CanonicalPostSnapshot {
			revision: stable.revision(),
			stable,
			live: observation.live.clone(),
			source: observation.source,
			source_key: observation.source_key.clone(),
			stable_observed_at: observation.observed_at,
			live_observed_at: observation.observed_at,
		}
	}

	#[test]
	fn equal_time_merge_is_independent_of_arrival_order() {
		let mut first = sample_post();
		first.title = "Alpha".to_string();
		let mut second = sample_post();
		second.title = "Beta".to_string();
		let first = PostObservation::from_post(&first, ObservationSource::Listing, "source-b", 100).unwrap();
		let second = PostObservation::from_post(&second, ObservationSource::Listing, "source-a", 100).unwrap();
		let (forward, _) = merge_stable(&snapshot_from(&first), &second);
		let (reverse, _) = merge_stable(&snapshot_from(&second), &first);
		assert_eq!(forward.revision(), reverse.revision());
		assert_eq!(forward.title, "Beta");
	}

	#[test]
	fn lower_quality_observation_cannot_erase_richer_direct_content() {
		let mut direct = sample_post();
		direct.title = "Authoritative title".to_string();
		direct.body = "<p>Authoritative body</p>".to_string();
		let mut listing = sample_post();
		listing.title = "Listing title".to_string();
		listing.body.clear();
		let direct = PostObservation::from_post(&direct, ObservationSource::Direct, "direct", 10).unwrap();
		let listing = PostObservation::from_post(&listing, ObservationSource::Listing, "listing", 20).unwrap();
		let (merged, incoming_won) = merge_stable(&snapshot_from(&direct), &listing);
		assert!(!incoming_won);
		assert_eq!(merged.title, "Authoritative title");
		assert_eq!(merged.body_html(), "<p>Authoritative body</p>");
	}
}
