use crate::server::RequestExt;
use crate::utils::{Flair, Media, Post, Preferences};
use askama::Template;
use hyper::{Body, Request, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex};

const REVISION_SCHEMA: &[u8] = b"stable-post-v3";
const CARD_ETAG_SCHEMA: &str = "card-v2";
const MAX_POST_ID_LEN: usize = 24;
const MAX_CANONICAL_POSTS: usize = 10_000;
const MAX_REVISION_BYTES: usize = 192 * 1024 * 1024;
const REVISIONS_PER_POST: usize = 2;
const MAX_REVISION_AGE_SECS: u64 = 7 * 24 * 60 * 60;
const MAX_LIVE_MEDIA_BYTES_PER_POST: usize = 256 * 1024;
const MAX_LIVE_MEDIA_BYTES: usize = 64 * 1024 * 1024;

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
	Listing,
	Peer,
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

/// Preference-sensitive, short-lived media metadata kept outside immutable
/// stable-card bytes. This projection is deliberately bounded and rendered
/// only by a private/no-store endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PostLiveMediaV1 {
	pub id: PostId,
	pub permalink: String,
	pub post_type: String,
	pub media: Media,
	pub uploader_css_classes: String,
	pub observed_at: u64,
}

impl PostLiveMediaV1 {
	fn from_post(post: &Post, stable: &StablePostContentV1, observed_at: u64) -> Option<Self> {
		if !matches!(post.post_type.as_str(), "video" | "gif") {
			return None;
		}
		if post.media.url.is_empty() && post.media.embed_code.is_empty() && post.media.hls_url.is_empty() && post.media.dash_url.is_empty() {
			return None;
		}

		let uploader_css_classes = post.redgifs_uploader_css_classes().to_string();
		let projection = Self {
			id: stable.id.clone(),
			permalink: post.permalink.clone(),
			post_type: post.post_type.clone(),
			media: post.media.clone(),
			uploader_css_classes,
			observed_at,
		};
		(projection.canonical_bytes().len() <= MAX_LIVE_MEDIA_BYTES_PER_POST).then_some(projection)
	}

	fn canonical_bytes(&self) -> Vec<u8> {
		serde_json::to_vec(self).unwrap_or_default()
	}

	fn deterministic_tie_break(&self) -> [u8; 32] {
		Sha256::digest(self.canonical_bytes()).into()
	}

	fn estimated_bytes(&self) -> usize {
		self.canonical_bytes().len()
	}

	pub fn redgifs_uploader_css_classes(&self) -> &str {
		&self.uploader_css_classes
	}
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
	pub live_media: Option<PostLiveMediaV1>,
	pub source: ObservationSource,
	pub source_key: String,
	pub observed_at: u64,
}

#[derive(Clone, Debug)]
pub struct CanonicalPostSnapshot {
	pub stable: Arc<StablePostContentV1>,
	pub revision: ContentRevision,
	pub live: PostLiveV1,
	pub live_media: Option<Arc<PostLiveMediaV1>>,
	pub source: ObservationSource,
	pub source_key: String,
	pub stable_observed_at: u64,
	pub live_observed_at: u64,
	pub live_media_source: ObservationSource,
	pub live_media_source_key: String,
	pub live_media_observed_at: u64,
}

#[derive(Default)]
struct CanonicalPostStore {
	latest: HashMap<PostId, CanonicalPostSnapshot>,
	revisions: HashMap<(PostId, ContentRevision), Arc<StablePostContentV1>>,
	revision_order: VecDeque<(PostId, ContentRevision)>,
	revision_seen_at: HashMap<(PostId, ContentRevision), u64>,
	revision_bytes: usize,
	live_media_bytes: usize,
}

static CANONICAL_POSTS: LazyLock<Mutex<CanonicalPostStore>> = LazyLock::new(|| Mutex::new(CanonicalPostStore::default()));

#[derive(Template)]
#[template(path = "post_content_card.html")]
struct StablePostCardTemplate<'a> {
	post: &'a StablePostContentV1,
	revision: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedStableCard {
	pub post_id: String,
	pub revision: String,
	pub path: String,
	pub html: String,
}

#[derive(Template)]
#[template(path = "post_live_media.html")]
struct PostLiveMediaTemplate<'a> {
	post: &'a PostLiveMediaV1,
	revision: &'a str,
	prefs: Preferences,
	url: &'a str,
}

crate::impl_template_str_eq!(PostLiveMediaTemplate<'_>);

pub async fn card(req: Request<Body>) -> Result<Response<Body>, String> {
	card_response(req, false).await
}

/// Cache-friendly representation of the stable card. Cloudflare's standard
/// cache recognizes the `.bin` suffix without a custom Cache Rule, while the
/// octet-stream media type and `nosniff` keep the HTML fragment non-executable.
pub async fn card_bin(req: Request<Body>) -> Result<Response<Body>, String> {
	card_response(req, true).await
}

async fn card_response(req: Request<Body>, binary: bool) -> Result<Response<Body>, String> {
	if req.uri().query().is_some() {
		return not_found();
	}
	let Some(raw_post_id) = req.param("id") else {
		return not_found();
	};
	let Some(post_id) = PostId::parse(&raw_post_id) else {
		return not_found();
	};
	if post_id.as_str() != raw_post_id {
		return not_found();
	}
	let Some(raw_revision) = req.param("revision") else {
		return not_found();
	};
	let Some(revision) = ContentRevision::parse(&raw_revision) else {
		return not_found();
	};
	if revision.as_str() != raw_revision {
		return not_found();
	}
	let Some(post) = exact(post_id.as_str(), revision.as_str()) else {
		return not_found();
	};
	let body = StablePostCardTemplate {
		post: &post,
		revision: revision.as_str(),
	}
	.render()
	.map_err(|error| error.to_string())?;

	let mut response = Response::builder()
		.status(200)
		.header("content-type", if binary { "application/octet-stream" } else { "text/html; charset=utf-8" })
		.header("cache-control", "public, max-age=31536000, s-maxage=86400, immutable")
		.header("etag", card_etag(revision.as_str()))
		.header("x-redlib-cache-policy", "immutable-post-content");
	if binary {
		response = response.header("x-content-type-options", "nosniff");
	}
	response.body(Body::from(body)).map_err(|error| error.to_string())
}

/// Render the exact bytes and identity of the latest stable card without
/// involving HTTP or an upstream fetch. Private shells can use this to reuse a
/// locally known card immediately while comments continue loading separately.
pub fn render_latest_stable_card(post_id: &str) -> Result<Option<RenderedStableCard>, String> {
	let Some(snapshot) = latest(post_id) else {
		return Ok(None);
	};
	let post_id = snapshot.stable.id.to_string();
	let revision = snapshot.revision.to_string();
	let html = StablePostCardTemplate {
		post: &snapshot.stable,
		revision: &revision,
	}
	.render()
	.map_err(|error| error.to_string())?;
	Ok(Some(RenderedStableCard {
		path: format!("/post-content/{post_id}/{revision}/card.bin"),
		post_id,
		revision,
		html,
	}))
}

pub async fn live_media(req: Request<Body>) -> Result<Response<Body>, String> {
	if req.uri().query().is_some() {
		return live_media_not_found();
	}
	let Some(raw_post_id) = req.param("id") else {
		return live_media_not_found();
	};
	let Some(post_id) = PostId::parse(&raw_post_id) else {
		return live_media_not_found();
	};
	if post_id.as_str() != raw_post_id {
		return live_media_not_found();
	}
	let Some(raw_revision) = req.param("revision") else {
		return live_media_not_found();
	};
	let Some(revision) = ContentRevision::parse(&raw_revision) else {
		return live_media_not_found();
	};
	if revision.as_str() != raw_revision || exact(post_id.as_str(), revision.as_str()).is_none() {
		return live_media_not_found();
	}

	let Some(snapshot) = latest(post_id.as_str()) else {
		return live_media_not_found();
	};
	if !snapshot.stable.expects_live_media() {
		return live_media_not_found();
	}
	let Some(media) = snapshot.live_media else {
		return live_media_not_found();
	};
	let prefs = Preferences::new(&req);
	let body = PostLiveMediaTemplate {
		post: &media,
		revision: revision.as_str(),
		prefs,
		url: &media.permalink,
	}
	.render()
	.map_err(|error| error.to_string())?;

	Response::builder()
		.status(200)
		.header("content-type", "text/html; charset=utf-8")
		.header("cache-control", "private, no-store")
		.header("cloudflare-cdn-cache-control", "no-store")
		.header("vary", "Cookie")
		.header("x-redlib-cache-policy", "private-post-live-media")
		.body(Body::from(body))
		.map_err(|error| error.to_string())
}

fn card_etag(revision: &str) -> String {
	format!("\"{revision}-{CARD_ETAG_SCHEMA}\"")
}

fn not_found() -> Result<Response<Body>, String> {
	Response::builder()
		.status(404)
		.header("content-type", "text/plain; charset=utf-8")
		.header("cache-control", "no-store")
		.body(Body::from("Post content revision not found"))
		.map_err(|error| error.to_string())
}

fn live_media_not_found() -> Result<Response<Body>, String> {
	Response::builder()
		.status(404)
		.header("content-type", "text/plain; charset=utf-8")
		.header("cache-control", "private, no-store")
		.header("cloudflare-cdn-cache-control", "no-store")
		.header("vary", "Cookie")
		.body(Body::from("Live post media not found"))
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
			body: SanitizedHtml::from_reddit_html(&crate::utils::link_reddit_user_mentions_in_html(&post.body)),
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

	fn expects_live_media(&self) -> bool {
		matches!(self.volatile_media_kind(), Some("video" | "gif"))
	}

	pub fn revision(&self) -> ContentRevision {
		self.revision_with_schema(REVISION_SCHEMA)
	}

	fn revision_with_schema(&self, schema: &[u8]) -> ContentRevision {
		let mut canonical = Vec::new();
		push_bytes(&mut canonical, schema);
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
		let stable = StablePostContentV1::from_post(post)?;
		Some(Self {
			live_media: PostLiveMediaV1::from_post(post, &stable, observed_at),
			stable,
			live: PostLiveV1::from_post(post, observed_at),
			source,
			source_key: source_key.into(),
			observed_at,
		})
	}
}

pub fn observe_post(post: &Post, source: ObservationSource, source_key: impl Into<String>, observed_at: u64) -> Option<CanonicalPostSnapshot> {
	crate::client::register_promo_post_media(post);
	crate::promo_detection::observe_post(post);
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
	insert_revision(&mut store, id.clone(), revision.clone(), stable.clone(), observation.observed_at);

	let (live, live_observed_at) = match store.latest.get(&id) {
		Some(existing) if existing.live_observed_at > observation.observed_at => (existing.live.clone(), existing.live_observed_at),
		_ => (observation.live.clone(), observation.observed_at),
	};
	let (live_media, live_media_source, live_media_source_key, live_media_observed_at) = match store.latest.get(&id) {
		Some(existing) if !live_media_observation_outranks(existing, &observation) => (
			existing.live_media.clone(),
			existing.live_media_source,
			existing.live_media_source_key.clone(),
			existing.live_media_observed_at,
		),
		_ => (
			observation.live_media.clone().map(Arc::new),
			observation.source,
			observation.source_key.clone(),
			observation.observed_at,
		),
	};
	let snapshot = CanonicalPostSnapshot {
		stable,
		revision,
		live,
		live_media,
		source: stable_source,
		source_key: stable_source_key,
		stable_observed_at,
		live_observed_at,
		live_media_source,
		live_media_source_key,
		live_media_observed_at,
	};
	if let Some(existing) = store.latest.get(&id).and_then(|snapshot| snapshot.live_media.as_ref()) {
		store.live_media_bytes = store.live_media_bytes.saturating_sub(existing.estimated_bytes());
	}
	if let Some(media) = snapshot.live_media.as_ref() {
		store.live_media_bytes = store.live_media_bytes.saturating_add(media.estimated_bytes());
	}
	store.latest.insert(id, snapshot.clone());
	prune_store(&mut store, observation.observed_at);
	snapshot
}

fn live_media_observation_outranks(existing: &CanonicalPostSnapshot, incoming: &PostObservation) -> bool {
	match (&existing.live_media, &incoming.live_media) {
		(Some(_), None) => return incoming.source >= ObservationSource::Direct && !incoming.stable.expects_live_media(),
		(None, Some(_)) => return true,
		(None, None) => return false,
		(Some(_), Some(_)) => {}
	}
	if incoming.observed_at != existing.live_media_observed_at {
		return incoming.observed_at > existing.live_media_observed_at;
	}
	if incoming.source != existing.live_media_source {
		return incoming.source > existing.live_media_source;
	}
	if incoming.source_key != existing.live_media_source_key {
		return incoming.source_key < existing.live_media_source_key;
	}

	let incoming_key = incoming.live_media.as_ref().map(PostLiveMediaV1::deterministic_tie_break).unwrap_or([0; 32]);
	let existing_key = existing.live_media.as_ref().map(|media| media.deterministic_tie_break()).unwrap_or([0; 32]);
	incoming_key > existing_key
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
	crate::client::register_promo_post_media(post);
	if post.kind != "t3" {
		return String::new();
	}
	if let Some(snapshot) = latest(&post.id) {
		return snapshot.revision.to_string();
	}
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

fn insert_revision(store: &mut CanonicalPostStore, id: PostId, revision: ContentRevision, stable: Arc<StablePostContentV1>, observed_at: u64) {
	let key = (id.clone(), revision.clone());
	if store.revisions.contains_key(&key) {
		store.revision_seen_at.insert(key, observed_at);
		return;
	}
	store.revision_bytes = store.revision_bytes.saturating_add(stable.estimated_bytes());
	store.revisions.insert(key.clone(), stable);
	store.revision_seen_at.insert(key.clone(), observed_at);
	store.revision_order.push_back(key);

	let same_post = store.revision_order.iter().filter(|(post_id, _)| post_id == &id).cloned().collect::<Vec<_>>();
	let excess = same_post.len().saturating_sub(REVISIONS_PER_POST);
	for stale in same_post.into_iter().take(excess) {
		remove_revision(store, &stale);
	}
}

fn prune_store(store: &mut CanonicalPostStore, now: u64) {
	let stale = store
		.revision_seen_at
		.iter()
		.filter(|(key, seen_at)| now.saturating_sub(**seen_at) > MAX_REVISION_AGE_SECS && store.latest.get(&key.0).map(|latest| latest.revision != key.1).unwrap_or(true))
		.map(|(key, _)| key.clone())
		.collect::<Vec<_>>();
	for key in stale {
		remove_revision(store, &key);
	}
	while store.latest.len() > MAX_CANONICAL_POSTS || store.revision_bytes > MAX_REVISION_BYTES || store.live_media_bytes > MAX_LIVE_MEDIA_BYTES {
		let Some(oldest) = store.revision_order.front().cloned() else {
			break;
		};
		remove_revision(store, &oldest);
		if !store.revisions.keys().any(|(post_id, _)| post_id == &oldest.0) {
			if let Some(removed) = store.latest.remove(&oldest.0).and_then(|snapshot| snapshot.live_media) {
				store.live_media_bytes = store.live_media_bytes.saturating_sub(removed.estimated_bytes());
			}
		}
	}
}

fn remove_revision(store: &mut CanonicalPostStore, key: &(PostId, ContentRevision)) {
	if let Some(stable) = store.revisions.remove(key) {
		store.revision_bytes = store.revision_bytes.saturating_sub(stable.estimated_bytes());
	}
	store.revision_seen_at.remove(key);
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
	use crate::server::RequestExt;
	use crate::utils::{Flags, GalleryMedia, Media};
	use hyper::body::to_bytes;
	use hyper::StatusCode;

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

	fn sample_video_post(id: &str, media_url: &str) -> Post {
		let mut post = sample_post();
		post.id = id.to_string();
		post.post_type = "video".to_string();
		post.media = Media {
			url: media_url.to_string(),
			hls_url: format!("/hls/{id}/HLSPlaylist.m3u8"),
			dash_url: format!("/dash/{id}/DASHPlaylist.mpd"),
			poster: format!("/preview/{id}.jpg"),
			width: 720,
			height: 1280,
			video_preview_enabled: true,
			video_preview_job_hint: media_url.to_string(),
			..Media::empty()
		};
		post
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
	fn volatile_media_urls_are_live_and_do_not_change_stable_revision() {
		let first = sample_video_post("volatile1", "/vid/volatile1/DASH_720.mp4?token=first");
		let second = sample_video_post("volatile1", "/vid/volatile1/DASH_720.mp4?token=second");
		let first_observation = PostObservation::from_post(&first, ObservationSource::Listing, "listing", 100).unwrap();
		let second_observation = PostObservation::from_post(&second, ObservationSource::Listing, "listing", 101).unwrap();

		assert_eq!(first_observation.stable.revision(), second_observation.stable.revision());
		assert!(matches!(first_observation.stable.media, StableMediaV1::Volatile { ref kind } if kind == "video"));
		assert_eq!(first_observation.live_media.as_ref().unwrap().media.url, "/vid/volatile1/DASH_720.mp4?token=first");
		assert_eq!(second_observation.live_media.as_ref().unwrap().media.url, "/vid/volatile1/DASH_720.mp4?token=second");
	}

	#[test]
	fn live_media_projection_rejects_unbounded_payloads() {
		let mut post = sample_video_post("bounded1", "/vid/bounded1/DASH_720.mp4");
		post.media.embed_code = "x".repeat(MAX_LIVE_MEDIA_BYTES_PER_POST + 1);
		let observation = PostObservation::from_post(&post, ObservationSource::Direct, "direct", 100).unwrap();
		assert!(observation.live_media.is_none());
	}

	#[test]
	fn revision_schema_v3_invalidates_earlier_card_urls() {
		let stable = StablePostContentV1::from_post(&sample_post()).unwrap();
		assert_eq!(REVISION_SCHEMA, b"stable-post-v3");
		assert_ne!(stable.revision(), stable.revision_with_schema(b"stable-post-v1"));
		assert_ne!(stable.revision(), stable.revision_with_schema(b"stable-post-v2"));
	}

	#[test]
	fn stable_content_repairs_persisted_split_user_mentions() {
		let mut post = sample_post();
		post.body = r#"<p><a href="/u/skye" rel="noopener noreferrer">u/skye</a>_yasmin2</p>"#.to_string();

		let stable = StablePostContentV1::from_post(&post).unwrap();

		assert_eq!(stable.body_html(), r#"<p><a href="/user/skye_yasmin2" rel="noopener noreferrer">u/skye_yasmin2</a></p>"#);
	}

	#[test]
	fn card_etag_uses_v2_identity() {
		assert_eq!(card_etag("abc123"), "\"abc123-card-v2\"");
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
			live_media: observation.live_media.clone().map(Arc::new),
			source: observation.source,
			source_key: observation.source_key.clone(),
			stable_observed_at: observation.observed_at,
			live_observed_at: observation.observed_at,
			live_media_source: observation.source,
			live_media_source_key: observation.source_key.clone(),
			live_media_observed_at: observation.observed_at,
		}
	}

	#[test]
	fn live_media_freshness_and_ties_are_deterministic() {
		let existing_post = sample_video_post("mediaorder1", "/vid/mediaorder1/old.mp4");
		let incoming_post = sample_video_post("mediaorder1", "/vid/mediaorder1/new.mp4");
		let existing = PostObservation::from_post(&existing_post, ObservationSource::Direct, "source-b", 100).unwrap();

		let newer = PostObservation::from_post(&incoming_post, ObservationSource::Listing, "source-z", 101).unwrap();
		assert!(live_media_observation_outranks(&snapshot_from(&existing), &newer));

		let equal_time_direct = PostObservation::from_post(&incoming_post, ObservationSource::Direct, "source-z", 100).unwrap();
		let equal_time_listing = PostObservation::from_post(&incoming_post, ObservationSource::Listing, "source-a", 100).unwrap();
		assert!(live_media_observation_outranks(&snapshot_from(&equal_time_listing), &equal_time_direct));

		let source_a = PostObservation::from_post(&incoming_post, ObservationSource::Listing, "source-a", 100).unwrap();
		let source_b = PostObservation::from_post(&existing_post, ObservationSource::Listing, "source-b", 100).unwrap();
		assert!(live_media_observation_outranks(&snapshot_from(&source_b), &source_a));
		assert!(!live_media_observation_outranks(&snapshot_from(&source_a), &source_b));
	}

	#[test]
	fn incomplete_observation_cannot_erase_playable_live_media() {
		let playable_post = sample_video_post("mediaincomplete1", "/vid/mediaincomplete1/video.mp4");
		let playable = PostObservation::from_post(&playable_post, ObservationSource::Direct, "direct", 100).unwrap();
		let mut incomplete_post = playable_post.clone();
		incomplete_post.media = Media::empty();
		let incomplete = PostObservation::from_post(&incomplete_post, ObservationSource::Listing, "listing", 200).unwrap();

		assert!(incomplete.live_media.is_none());
		assert!(!live_media_observation_outranks(&snapshot_from(&playable), &incomplete));

		let direct_incomplete = PostObservation::from_post(&incomplete_post, ObservationSource::Direct, "direct", 201).unwrap();
		assert!(!live_media_observation_outranks(&snapshot_from(&playable), &direct_incomplete));
	}

	#[tokio::test]
	async fn authoritative_type_change_clears_and_stops_serving_old_media() {
		let id = "mediacleared1";
		let playable_post = sample_video_post(id, "/vid/mediacleared1/video.mp4");
		let playable = observe_post(&playable_post, ObservationSource::Direct, "direct-video", 100).unwrap();
		let old_revision = playable.revision.to_string();
		let mut changed_post = playable_post.clone();
		changed_post.post_type = "self".to_string();
		changed_post.media = Media::empty();
		let changed = observe_post(&changed_post, ObservationSource::Direct, "direct-self", 200).unwrap();

		assert!(changed.live_media.is_none());
		assert!(!changed.stable.expects_live_media());
		let uri = format!("/post-live/{id}/{old_revision}/media");
		let response = live_media(request_with_live_media_params(&uri, id, &old_revision)).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
		assert_eq!(response.headers()["cache-control"], "private, no-store");
	}

	#[test]
	fn render_time_revision_lookup_does_not_advance_live_media() {
		let id = "renderlookup1";
		let fresh = sample_video_post(id, "/vid/renderlookup1/fresh.mp4");
		let snapshot = observe_post(&fresh, ObservationSource::Direct, "direct", 100).unwrap();
		let stale = sample_video_post(id, "/vid/renderlookup1/stale.mp4");

		assert_eq!(revision_for_post(&stale), snapshot.revision.to_string());
		let after = latest(id).unwrap();
		assert_eq!(after.live_media_observed_at, 100);
		assert_eq!(after.live_media.unwrap().media.url, "/vid/renderlookup1/fresh.mp4");
	}

	fn request_with_live_media_params(uri: &str, id: &str, revision: &str) -> Request<Body> {
		let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
		let mut params = route_recognizer::Params::new();
		params.insert("id".to_string(), id.to_string());
		params.insert("revision".to_string(), revision.to_string());
		req.set_params(params);
		req
	}

	#[tokio::test]
	async fn live_media_endpoint_is_revision_bound_private_and_uses_latest_media() {
		let id = "liveendpoint1";
		let first = sample_video_post(id, "/vid/liveendpoint1/old.mp4");
		let first_snapshot = observe_post(&first, ObservationSource::Listing, "listing", 100).unwrap();
		let revision = first_snapshot.revision.to_string();
		let second = sample_video_post(id, "/vid/liveendpoint1/fresh.mp4");
		observe_post(&second, ObservationSource::Direct, "direct", 101).unwrap();

		let uri = format!("/post-live/{id}/{revision}/media");
		let response = live_media(request_with_live_media_params(&uri, id, &revision)).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.headers()["cache-control"], "private, no-store");
		assert_eq!(response.headers()["cloudflare-cdn-cache-control"], "no-store");
		assert_eq!(response.headers()["vary"], "Cookie");
		assert!(!response.headers().contains_key("etag"));
		let body = String::from_utf8(to_bytes(response.into_body()).await.unwrap().to_vec()).unwrap();
		assert!(body.contains(&format!("data-post-live-media-id=\"{id}\"")));
		assert!(body.contains(&format!("data-post-live-media-revision=\"{revision}\"")));
		assert!(body.contains("data-post-live-media-observed-at=\"101\""));
		assert!(body.contains("/vid/liveendpoint1/fresh.mp4"));
		assert!(!body.contains("/vid/liveendpoint1/old.mp4"));
	}

	#[tokio::test]
	async fn live_media_endpoint_rejects_queries_and_unknown_revisions_no_store() {
		let id = "liveendpoint2";
		let post = sample_video_post(id, "/vid/liveendpoint2/video.mp4");
		let snapshot = observe_post(&post, ObservationSource::Direct, "direct", 200).unwrap();
		let revision = snapshot.revision.to_string();

		let query_uri = format!("/post-live/{id}/{revision}/media?variant=1");
		let query_response = live_media(request_with_live_media_params(&query_uri, id, &revision)).await.unwrap();
		assert_eq!(query_response.status(), StatusCode::NOT_FOUND);
		assert_eq!(query_response.headers()["cache-control"], "private, no-store");

		let unknown_revision = "0".repeat(64);
		let unknown_uri = format!("/post-live/{id}/{unknown_revision}/media");
		let unknown_response = live_media(request_with_live_media_params(&unknown_uri, id, &unknown_revision)).await.unwrap();
		assert_eq!(unknown_response.status(), StatusCode::NOT_FOUND);
		assert_eq!(unknown_response.headers()["cloudflare-cdn-cache-control"], "no-store");
	}

	#[tokio::test]
	async fn immutable_card_rejects_noncanonical_cache_aliases() {
		let id = "cardalias1";
		let mut post = sample_post();
		post.id = id.to_string();
		let snapshot = observe_post(&post, ObservationSource::Direct, "direct", 250).unwrap();
		let revision = snapshot.revision.to_string();

		let uppercase_id = id.to_ascii_uppercase();
		let uppercase_id_uri = format!("/post-content/{uppercase_id}/{revision}/card.bin");
		let id_response = card_bin(request_with_live_media_params(&uppercase_id_uri, &uppercase_id, &revision)).await.unwrap();
		assert_eq!(id_response.status(), StatusCode::NOT_FOUND);
		assert_eq!(id_response.headers()["cache-control"], "no-store");

		let uppercase_revision = revision.to_ascii_uppercase();
		let uppercase_revision_uri = format!("/post-content/{id}/{uppercase_revision}/card.bin");
		let revision_response = card_bin(request_with_live_media_params(&uppercase_revision_uri, id, &uppercase_revision)).await.unwrap();
		assert_eq!(revision_response.status(), StatusCode::NOT_FOUND);
		assert_eq!(revision_response.headers()["cache-control"], "no-store");
	}

	#[tokio::test]
	async fn binary_card_is_nosniff_and_matches_legacy_card_identity() {
		let id = "cardbinary1";
		let mut post = sample_post();
		post.id = id.to_string();
		let snapshot = observe_post(&post, ObservationSource::Direct, "direct", 275).unwrap();
		let revision = snapshot.revision.to_string();

		let binary_uri = format!("/post-content/{id}/{revision}/card.bin");
		let binary = card_bin(request_with_live_media_params(&binary_uri, id, &revision)).await.unwrap();
		assert_eq!(binary.status(), StatusCode::OK);
		assert_eq!(binary.headers()["content-type"], "application/octet-stream");
		assert_eq!(binary.headers()["x-content-type-options"], "nosniff");
		assert_eq!(binary.headers()["cache-control"], "public, max-age=31536000, s-maxage=86400, immutable");
		assert_eq!(binary.headers()["etag"], card_etag(&revision));
		assert_eq!(binary.headers()["x-redlib-cache-policy"], "immutable-post-content");
		assert!(!binary.headers().contains_key("set-cookie"));
		let binary_body = to_bytes(binary.into_body()).await.unwrap();

		let legacy_uri = format!("/post-content/{id}/{revision}/card");
		let legacy = card(request_with_live_media_params(&legacy_uri, id, &revision)).await.unwrap();
		assert_eq!(legacy.status(), StatusCode::OK);
		assert_eq!(legacy.headers()["content-type"], "text/html; charset=utf-8");
		assert_eq!(legacy.headers()["etag"], card_etag(&revision));
		let legacy_body = to_bytes(legacy.into_body()).await.unwrap();
		assert_eq!(binary_body, legacy_body);
	}

	#[tokio::test]
	async fn binary_card_query_variant_is_no_store() {
		let id = "cardbinary2";
		let mut post = sample_post();
		post.id = id.to_string();
		let snapshot = observe_post(&post, ObservationSource::Direct, "direct", 276).unwrap();
		let revision = snapshot.revision.to_string();
		let uri = format!("/post-content/{id}/{revision}/card.bin?variant=1");

		let response = card_bin(request_with_live_media_params(&uri, id, &revision)).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
		assert_eq!(response.headers()["cache-control"], "no-store");
	}

	#[test]
	fn latest_stable_card_helper_matches_exact_card_identity() {
		let id = "cardhelper1";
		let post = sample_video_post(id, "/vid/cardhelper1/video.mp4");
		let snapshot = observe_post(&post, ObservationSource::Listing, "listing", 300).unwrap();
		let rendered = render_latest_stable_card(id).unwrap().unwrap();

		assert_eq!(rendered.post_id, id);
		assert_eq!(rendered.revision, snapshot.revision.to_string());
		assert_eq!(rendered.path, format!("/post-content/{id}/{}/card.bin", snapshot.revision));
		assert!(rendered.html.contains(&format!("data-post-content-id=\"{id}\"")));
		assert!(rendered.html.contains(&format!("data-post-content-revision=\"{}\"", snapshot.revision)));
		assert!(!rendered.html.contains("/vid/cardhelper1/video.mp4"));
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

	#[test]
	fn age_pruning_removes_old_prior_revision_but_keeps_current() {
		let first = PostObservation::from_post(&sample_post(), ObservationSource::Listing, "listing", 1).unwrap();
		let mut edited_post = sample_post();
		edited_post.title = "Edited".to_string();
		let edited = PostObservation::from_post(&edited_post, ObservationSource::Direct, "direct", 2).unwrap();
		let first_revision = first.stable.revision();
		let edited_revision = edited.stable.revision();
		let id = first.stable.id.clone();
		let mut store = CanonicalPostStore::default();
		insert_revision(&mut store, id.clone(), first_revision.clone(), Arc::new(first.stable), 1);
		insert_revision(&mut store, id.clone(), edited_revision.clone(), Arc::new(edited.stable.clone()), 2);
		store.latest.insert(id.clone(), snapshot_from(&edited));
		prune_store(&mut store, MAX_REVISION_AGE_SECS + 10);
		assert!(!store.revisions.contains_key(&(id.clone(), first_revision)));
		assert!(store.revisions.contains_key(&(id, edited_revision)));
	}
}
