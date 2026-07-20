use crate::prefs_store;
use crate::utils::{Post, Preferences};
use hyper::{header, Body, Request, Response, StatusCode};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{mpsc, LazyLock, Mutex};
use std::time::Instant;

const DETECTOR_VERSION: &str = "snap-promo-v3";
const MAX_ANALYSIS_BYTES: usize = 8 * 1024 * 1024;
const MAX_MEDIA_MAPPINGS: usize = 4096;
const MAX_PERSISTED_MARKERS: usize = 100_000;
const MAX_PERSISTED_OBSERVATIONS: usize = 100_000;
pub const PROMOTION_AUTO_FILTER_THRESHOLD: usize = 5;
pub const REDGIFS_AUTO_FILTER_THRESHOLD: usize = 10;

static SOCIAL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(?:\bsnap(?:chat)?\b|\bsnp\b|\btelegram\b|\btg\b|\bt\.me\b)").unwrap());
static VOTE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bupv(?:ote|otes|oted|oting|te|tes|et|ets)\b").unwrap());
static URGENCY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bdelet\w*\s+(?:in|within)\s+\[?\d{1,3}\]?\s*(?:h|hr|hrs|hours?|days?)\b").unwrap());
static ENGAGEMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
	Regex::new(
		r"(?ix)
		(?:\b(?:first|1st)\s*\d{1,4}\b.{0,80}\b(?:get|gets|receive|receives|sending|send)\b)
		|(?:\b(?:sending|send|giving|give|get|gets)\b.{0,80}\b(?:first|1st)\s*\d{1,4}\b)
		|(?:\bsay\s+(?:yes|no|yo)\b.{0,80}\b(?:nudes?|pics?|pictures?|vids?|videos?|receive|received)\b)
		|(?:\b(?:nudes?|pics?|pictures?|vids?|videos?)\b.{0,80}\b(?:first|1st)\s*\d{1,4}\s*(?:people|guys|who)\b)
		|(?:\b(?:nudes?|pics?|pictures?|vids?|videos?)\b.{0,80}\b(?:say|comment|drop)\s+(?:yes|no|yo|me)\b)",
	)
	.unwrap()
});

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedPromoMarker {
	pub post_id: String,
	pub signal: String,
	pub detector_version: String,
	pub detected_at_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedContentFilterObservation {
	pub post_id: String,
	pub author: String,
	pub community: String,
	pub redgifs_profile: String,
	pub seen_at_unix: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct SubredditPromotionStats {
	pub seen_posts: usize,
	pub filtered_posts: usize,
	pub filtered_percent: u32,
}

#[derive(Default)]
struct DetectionState {
	markers: HashMap<String, PersistedPromoMarker>,
	observations: HashMap<String, PersistedContentFilterObservation>,
	seen_by_subreddit: HashMap<String, HashSet<String>>,
	promo_by_subreddit: HashMap<String, HashSet<String>>,
	promo_by_author: HashMap<String, HashSet<String>>,
	redgifs_profiles_by_author: HashMap<String, HashMap<String, usize>>,
}

#[derive(Clone)]
struct MediaMapping {
	post_ids: HashSet<String>,
	last_seen: Instant,
}

#[derive(Clone, Copy)]
enum MediaState {
	Pending,
	Negative,
	Positive,
}

struct ImageJob {
	url: String,
	content_type: String,
	body: hyper::body::Bytes,
}

#[derive(Deserialize)]
struct WorkerReply {
	flagged: bool,
	signal: String,
	#[serde(default)]
	fast_ms: f64,
	#[serde(default)]
	total_ms: f64,
}

static STATE: LazyLock<Mutex<DetectionState>> = LazyLock::new(|| Mutex::new(DetectionState::default()));
static MEDIA_POSTS: LazyLock<Mutex<HashMap<String, MediaMapping>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static MEDIA_STATES: LazyLock<Mutex<HashMap<String, MediaState>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WORKER: LazyLock<mpsc::SyncSender<ImageJob>> = LazyLock::new(|| {
	let (sender, receiver) = mpsc::sync_channel(16);
	std::thread::Builder::new()
		.name("snap-promo-opencv".to_string())
		.spawn(move || worker_loop(receiver))
		.expect("spawn snap promo worker");
	sender
});

pub const fn max_analysis_bytes() -> usize {
	MAX_ANALYSIS_BYTES
}

fn title_signal(title: &str) -> Option<&'static str> {
	if SOCIAL_RE.is_match(title) {
		Some("social_promo")
	} else if URGENCY_RE.is_match(title) {
		Some("urgency_bait")
	} else if VOTE_RE.is_match(title) {
		Some("vote_bait")
	} else if ENGAGEMENT_RE.is_match(title) {
		Some("engagement_bait")
	} else {
		None
	}
}

fn normalize_author(value: &str) -> String {
	value.trim().trim_start_matches("u/").trim_start_matches("u_").to_ascii_lowercase()
}

fn normalize_community(value: &str) -> String {
	value.trim().trim_start_matches("r/").to_ascii_lowercase()
}

fn rebuild_indexes(state: &mut DetectionState) {
	state.seen_by_subreddit.clear();
	state.promo_by_subreddit.clear();
	state.promo_by_author.clear();
	state.redgifs_profiles_by_author.clear();
	for observation in state.observations.values() {
		if !observation.community.is_empty() {
			state
				.seen_by_subreddit
				.entry(observation.community.clone())
				.or_default()
				.insert(observation.post_id.clone());
		}
		if !observation.author.is_empty() && !observation.redgifs_profile.is_empty() {
			*state
				.redgifs_profiles_by_author
				.entry(observation.author.clone())
				.or_default()
				.entry(observation.redgifs_profile.clone())
				.or_default() += 1;
		}
	}
	for marker in state.markers.values() {
		let observation = state.observations.get(&marker.post_id);
		let author = observation.map(|value| value.author.as_str()).unwrap_or_default();
		let community = observation.map(|value| value.community.as_str()).unwrap_or_default();
		if !author.is_empty() {
			state.promo_by_author.entry(author.to_string()).or_default().insert(marker.post_id.clone());
		}
		if !community.is_empty() {
			state.promo_by_subreddit.entry(community.to_string()).or_default().insert(marker.post_id.clone());
		}
	}
}

fn add_observation_indexes(state: &mut DetectionState, observation: &PersistedContentFilterObservation) {
	if !observation.community.is_empty() {
		state
			.seen_by_subreddit
			.entry(observation.community.clone())
			.or_default()
			.insert(observation.post_id.clone());
	}
	if !observation.author.is_empty() && !observation.redgifs_profile.is_empty() {
		*state
			.redgifs_profiles_by_author
			.entry(observation.author.clone())
			.or_default()
			.entry(observation.redgifs_profile.clone())
			.or_default() += 1;
	}
	if state.markers.contains_key(&observation.post_id) {
		if !observation.author.is_empty() {
			state.promo_by_author.entry(observation.author.clone()).or_default().insert(observation.post_id.clone());
		}
		if !observation.community.is_empty() {
			state
				.promo_by_subreddit
				.entry(observation.community.clone())
				.or_default()
				.insert(observation.post_id.clone());
		}
	}
}

fn remove_observation_indexes(state: &mut DetectionState, observation: &PersistedContentFilterObservation) {
	if let Some(posts) = state.seen_by_subreddit.get_mut(&observation.community) {
		posts.remove(&observation.post_id);
		if posts.is_empty() {
			state.seen_by_subreddit.remove(&observation.community);
		}
	}
	if let Some(profiles) = state.redgifs_profiles_by_author.get_mut(&observation.author) {
		if let Some(count) = profiles.get_mut(&observation.redgifs_profile) {
			*count = count.saturating_sub(1);
			if *count == 0 {
				profiles.remove(&observation.redgifs_profile);
			}
		}
		if profiles.is_empty() {
			state.redgifs_profiles_by_author.remove(&observation.author);
		}
	}
	if state.markers.contains_key(&observation.post_id) {
		if let Some(posts) = state.promo_by_author.get_mut(&observation.author) {
			posts.remove(&observation.post_id);
			if posts.is_empty() {
				state.promo_by_author.remove(&observation.author);
			}
		}
		if let Some(posts) = state.promo_by_subreddit.get_mut(&observation.community) {
			posts.remove(&observation.post_id);
			if posts.is_empty() {
				state.promo_by_subreddit.remove(&observation.community);
			}
		}
	}
}

pub fn observe_post(post: &Post) {
	if post.kind != "t3" {
		return;
	}
	let Some(post_id) = crate::post_components::PostId::parse(&post.id) else {
		return;
	};
	let post_id = post_id.to_string();
	let author = normalize_author(&post.author.name);
	let community = normalize_community(&post.community);
	let redgifs_profile = crate::utils::normalize_creator_identity_slug(&post.media.redgifs_username);
	let seen_at_unix = crate::cache_persistence::unix_now();
	let mut changed = false;
	if let Ok(mut state) = STATE.lock() {
		let existing = state.observations.get(&post_id).cloned();
		let observation = PersistedContentFilterObservation {
			post_id: post_id.clone(),
			author: if author.is_empty() {
				existing.as_ref().map(|value| value.author.clone()).unwrap_or_default()
			} else {
				author
			},
			community: if community.is_empty() {
				existing.as_ref().map(|value| value.community.clone()).unwrap_or_default()
			} else {
				community
			},
			redgifs_profile: if redgifs_profile.is_empty() {
				existing.as_ref().map(|value| value.redgifs_profile.clone()).unwrap_or_default()
			} else {
				redgifs_profile
			},
			seen_at_unix,
		};
		let should_replace = existing
			.as_ref()
			.is_none_or(|existing| existing.author != observation.author || existing.community != observation.community || existing.redgifs_profile != observation.redgifs_profile);
		if should_replace {
			if let Some(existing) = existing.as_ref() {
				remove_observation_indexes(&mut state, existing);
			}
			state.observations.insert(post_id.clone(), observation.clone());
			add_observation_indexes(&mut state, &observation);
			if state.observations.len() > MAX_PERSISTED_OBSERVATIONS {
				if let Some(oldest) = state.observations.values().min_by_key(|value| value.seen_at_unix).map(|value| value.post_id.clone()) {
					if let Some(removed) = state.observations.remove(&oldest) {
						remove_observation_indexes(&mut state, &removed);
					}
				}
			}
			changed = true;
		}
	}
	if changed {
		crate::cache_persistence::mark_dirty();
	}
	if let Some(signal) = title_signal(&post.title) {
		mark(&post_id, signal);
	}
}

fn mark(post_id: &str, signal: &str) {
	if crate::post_components::PostId::parse(post_id).is_none() {
		return;
	}
	let Ok(mut state) = STATE.lock() else { return };
	if state.markers.contains_key(post_id) {
		return;
	}
	state.markers.insert(
		post_id.to_string(),
		PersistedPromoMarker {
			post_id: post_id.to_string(),
			signal: signal.to_string(),
			detector_version: DETECTOR_VERSION.to_string(),
			detected_at_unix: crate::cache_persistence::unix_now(),
		},
	);
	if let Some(observation) = state.observations.get(post_id).cloned() {
		if !observation.author.is_empty() {
			state.promo_by_author.entry(observation.author).or_default().insert(post_id.to_string());
		}
		if !observation.community.is_empty() {
			state.promo_by_subreddit.entry(observation.community).or_default().insert(post_id.to_string());
		}
	}
	drop(state);
	crate::cache_persistence::mark_dirty();
}

pub fn register_media_post(url: &str, post_id: &str) {
	if crate::post_components::PostId::parse(post_id).is_none() {
		return;
	}
	let Ok(mut mappings) = MEDIA_POSTS.lock() else { return };
	if mappings.len() >= MAX_MEDIA_MAPPINGS && !mappings.contains_key(url) {
		if let Some(oldest) = mappings.iter().min_by_key(|(_, value)| value.last_seen).map(|(key, _)| key.clone()) {
			mappings.remove(&oldest);
			if let Ok(mut states) = MEDIA_STATES.lock() {
				states.remove(&oldest);
			}
		}
	}
	let mapping = mappings.entry(url.to_string()).or_insert_with(|| MediaMapping {
		post_ids: HashSet::new(),
		last_seen: Instant::now(),
	});
	mapping.last_seen = Instant::now();
	mapping.post_ids.insert(post_id.to_string());
	if matches!(MEDIA_STATES.lock().ok().and_then(|states| states.get(url).copied()), Some(MediaState::Positive)) {
		mark(post_id, "snapcode");
	}
}

pub fn enqueue_image(url: &str, content_type: &str, body: hyper::body::Bytes) {
	if body.is_empty() || body.len() > MAX_ANALYSIS_BYTES || !content_type.starts_with("image/") {
		return;
	}
	if !MEDIA_POSTS.lock().ok().is_some_and(|mappings| mappings.contains_key(url)) {
		return;
	}
	if let Ok(mut states) = MEDIA_STATES.lock() {
		if states.contains_key(url) {
			return;
		}
		states.insert(url.to_string(), MediaState::Pending);
	}
	let job = ImageJob {
		url: url.to_string(),
		content_type: content_type.to_string(),
		body,
	};
	if WORKER.try_send(job).is_err() {
		if let Ok(mut states) = MEDIA_STATES.lock() {
			states.remove(url);
		}
	}
}

fn worker_loop(receiver: mpsc::Receiver<ImageJob>) {
	for job in receiver {
		match run_worker_job(&job) {
			Ok(reply) => {
				let state = if reply.flagged && reply.signal == "snapcode" {
					MediaState::Positive
				} else {
					MediaState::Negative
				};
				if let Ok(mut states) = MEDIA_STATES.lock() {
					states.insert(job.url.clone(), state);
				}
				if matches!(state, MediaState::Positive) {
					let ids = MEDIA_POSTS
						.lock()
						.ok()
						.and_then(|mappings| mappings.get(&job.url).map(|mapping| mapping.post_ids.iter().cloned().collect::<Vec<_>>()))
						.unwrap_or_default();
					for id in ids {
						mark(&id, "snapcode");
					}
				}
				log::debug!(
					"promo detector {} {:.2}ms total ({:.2}ms features) for {}",
					reply.signal,
					reply.total_ms,
					reply.fast_ms,
					job.url
				);
			}
			Err(error) => {
				log::warn!("promo detector unavailable: {error}");
				if let Ok(mut states) = MEDIA_STATES.lock() {
					states.remove(&job.url);
				}
			}
		}
	}
}

fn run_worker_job(job: &ImageJob) -> Result<WorkerReply, String> {
	// Starting once per image is deliberately avoided: the helper stays alive
	// and uses OpenCV's own reduced JPEG decode. It is restarted after a crash.
	static PROCESS: LazyLock<Mutex<Option<WorkerProcess>>> = LazyLock::new(|| Mutex::new(None));
	let mut guard = PROCESS.lock().map_err(|_| "worker process lock poisoned".to_string())?;
	if guard.is_none() {
		*guard = Some(WorkerProcess::start()?);
	}
	let result = guard.as_mut().unwrap().request(job);
	if result.is_err() {
		*guard = None;
	}
	result
}

struct WorkerProcess {
	_child: std::process::Child,
	stdin: std::process::ChildStdin,
	stdout: BufReader<std::process::ChildStdout>,
}

impl WorkerProcess {
	fn start() -> Result<Self, String> {
		let python = std::env::var("REDLIB_PROMO_CLASSIFIER_PYTHON").unwrap_or_else(|_| "python3".to_string());
		let script = std::env::var("REDLIB_PROMO_CLASSIFIER_SCRIPT").unwrap_or_else(|_| "scripts/snap_promo_detector.py".to_string());
		let mut child = Command::new(python)
			.arg(script)
			.arg("--worker")
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::inherit())
			.spawn()
			.map_err(|error| error.to_string())?;
		let stdin = child.stdin.take().ok_or("missing worker stdin")?;
		let stdout = BufReader::new(child.stdout.take().ok_or("missing worker stdout")?);
		Ok(Self { _child: child, stdin, stdout })
	}

	fn request(&mut self, job: &ImageJob) -> Result<WorkerReply, String> {
		let metadata = serde_json::to_vec(&json!({ "content_type": job.content_type })).map_err(|error| error.to_string())?;
		self.stdin.write_all(&(metadata.len() as u32).to_be_bytes()).map_err(|error| error.to_string())?;
		self.stdin.write_all(&metadata).map_err(|error| error.to_string())?;
		self.stdin.write_all(&(job.body.len() as u64).to_be_bytes()).map_err(|error| error.to_string())?;
		self.stdin.write_all(&job.body).map_err(|error| error.to_string())?;
		self.stdin.flush().map_err(|error| error.to_string())?;
		let mut line = String::new();
		self.stdout.read_line(&mut line).map_err(|error| error.to_string())?;
		if line.is_empty() {
			return Err("worker exited without a reply".to_string());
		}
		serde_json::from_str(&line).map_err(|error| error.to_string())
	}
}

fn marker_for(post_id: &str) -> Option<PersistedPromoMarker> {
	STATE.lock().ok()?.markers.get(post_id).cloned()
}

pub fn post_is_promotional(post_id: &str) -> bool {
	STATE.lock().ok().is_some_and(|state| state.markers.contains_key(post_id))
}

pub fn subreddit_promotion_stats(community: &str) -> SubredditPromotionStats {
	let community = normalize_community(community);
	let Ok(state) = STATE.lock() else {
		return SubredditPromotionStats::default();
	};
	let seen_posts = state.seen_by_subreddit.get(&community).map(HashSet::len).unwrap_or_default();
	let filtered_posts = state.promo_by_subreddit.get(&community).map(HashSet::len).unwrap_or_default();
	let filtered_percent = if seen_posts == 0 {
		0
	} else {
		((filtered_posts.min(seen_posts) as f64 / seen_posts as f64) * 100.0).round() as u32
	};
	SubredditPromotionStats {
		seen_posts,
		filtered_posts,
		filtered_percent,
	}
}

/// Recommendation evidence from a subreddit is reduced monotonically as its
/// observed promotion-spam rate grows. RedGIF-reposter statistics are kept
/// separate and deliberately do not affect this factor.
pub fn promotion_quality_factor(community: &str) -> f64 {
	let stats = subreddit_promotion_stats(community);
	promotion_quality_factor_from_counts(stats.seen_posts, stats.filtered_posts)
}

fn promotion_quality_factor_from_counts(seen_posts: usize, filtered_posts: usize) -> f64 {
	if seen_posts == 0 {
		return 1.0;
	}
	let rate = filtered_posts.min(seen_posts) as f64 / seen_posts as f64;
	1.0 / (1.0 + 4.0 * rate)
}

fn qualifying_filter_reasons(state: &DetectionState, author: &str) -> Vec<&'static str> {
	let mut reasons = Vec::new();
	if state.promo_by_author.get(author).map(HashSet::len).unwrap_or_default() >= PROMOTION_AUTO_FILTER_THRESHOLD {
		reasons.push("promotion_spam");
	}
	if state.redgifs_profiles_by_author.get(author).map(|profiles| profiles.len()).unwrap_or_default() >= REDGIFS_AUTO_FILTER_THRESHOLD {
		reasons.push("redgifs_reposter");
	}
	reasons
}

pub fn qualifying_author_counts() -> (usize, usize) {
	let Ok(state) = STATE.lock() else { return (0, 0) };
	let promotion = state.promo_by_author.values().filter(|post_ids| post_ids.len() >= PROMOTION_AUTO_FILTER_THRESHOLD).count();
	let redgifs = state
		.redgifs_profiles_by_author
		.values()
		.filter(|profiles| profiles.len() >= REDGIFS_AUTO_FILTER_THRESHOLD)
		.count();
	(promotion, redgifs)
}

pub fn viewer_auto_filter_counts(prefs: &Preferences) -> (usize, usize) {
	let Ok(state) = STATE.lock() else { return (0, 0) };
	let is_filtered = |author: &str| !prefs.has_followed_user(author) && prefs.has_filter(&format!("u_{author}"));
	let promotion = state
		.promo_by_author
		.iter()
		.filter(|(author, post_ids)| post_ids.len() >= PROMOTION_AUTO_FILTER_THRESHOLD && is_filtered(author))
		.count();
	let redgifs = state
		.redgifs_profiles_by_author
		.iter()
		.filter(|(author, profiles)| profiles.len() >= REDGIFS_AUTO_FILTER_THRESHOLD && is_filtered(author))
		.count();
	(promotion, redgifs)
}

pub async fn status(req: Request<Body>) -> Result<Response<Body>, String> {
	let mut prefs = Preferences::new(&req);
	let ids = req
		.uri()
		.query()
		.and_then(|query| {
			url::form_urlencoded::parse(query.as_bytes())
				.find(|(key, _)| key == "ids")
				.map(|(_, value)| value.into_owned())
		})
		.unwrap_or_default();
	let mut signals = serde_json::Map::new();
	let parsed_ids = ids
		.split(',')
		.take(100)
		.filter_map(crate::post_components::PostId::parse)
		.map(|id| id.to_string())
		.collect::<Vec<_>>();
	for id in &parsed_ids {
		if marker_for(id).is_none() {
			if let Some(snapshot) = crate::post_components::latest(id) {
				if let Some(signal) = title_signal(&snapshot.stable.title) {
					mark(id, signal);
				}
			}
		}
		if let Some(marker) = marker_for(id) {
			signals.insert(id.clone(), json!({ "signal": marker.signal }));
		}
	}

	let mut auto_filters = serde_json::Map::new();
	let mut preferences_changed = false;
	if let Ok(state) = STATE.lock() {
		let authors = state
			.promo_by_author
			.keys()
			.chain(state.redgifs_profiles_by_author.keys())
			.filter(|author| !author.is_empty() && author.as_str() != "[deleted]")
			.cloned()
			.collect::<HashSet<_>>();
		for author in authors {
			let reasons = qualifying_filter_reasons(&state, &author);
			if reasons.is_empty() || prefs.has_followed_user(&author) {
				continue;
			}
			let filter = format!("u_{author}");
			if !prefs.has_filter(&filter) {
				prefs.filters.push(filter);
				preferences_changed = true;
			}
			auto_filters.insert(author, json!({ "reasons": reasons }));
		}
	}
	if preferences_changed {
		prefs.prepare_for_runtime();
	}
	let (promotion_filtered_users, redgifs_filtered_users) = viewer_auto_filter_counts(&prefs);
	let mut response = Response::builder()
		.status(StatusCode::OK)
		.header(header::CONTENT_TYPE, "application/json; charset=utf-8")
		.header(header::CACHE_CONTROL, "private, no-store")
		.body(Body::from(
			json!({
				"signals": signals,
				"auto_filters": auto_filters,
				"stats": {
					"promotion_filtered_users": promotion_filtered_users,
					"redgifs_filtered_users": redgifs_filtered_users,
				}
			})
			.to_string(),
		))
		.map_err(|error| error.to_string())?;
	if preferences_changed {
		prefs_store::save_preferences(&mut response, &req, &prefs)?;
	}
	Ok(response)
}

pub fn snapshot_markers() -> Vec<PersistedPromoMarker> {
	let mut values = STATE.lock().map(|state| state.markers.values().cloned().collect::<Vec<_>>()).unwrap_or_default();
	values.sort_by_key(|marker| std::cmp::Reverse(marker.detected_at_unix));
	values.truncate(MAX_PERSISTED_MARKERS);
	values
}

fn marker_is_restorable(marker: &PersistedPromoMarker) -> bool {
	let stale_snapcode = marker.signal == "snapcode" && marker.detector_version != DETECTOR_VERSION;
	!stale_snapcode && crate::post_components::PostId::parse(&marker.post_id).is_some()
}

pub fn restore_markers(markers: Vec<PersistedPromoMarker>) {
	let Ok(mut state) = STATE.lock() else { return };
	state.markers.clear();
	for marker in markers.into_iter().take(MAX_PERSISTED_MARKERS) {
		if marker_is_restorable(&marker) {
			state.markers.insert(marker.post_id.clone(), marker);
		}
	}
	rebuild_indexes(&mut state);
}

pub fn snapshot_observations() -> Vec<PersistedContentFilterObservation> {
	let mut values = STATE.lock().map(|state| state.observations.values().cloned().collect::<Vec<_>>()).unwrap_or_default();
	values.sort_by_key(|observation| std::cmp::Reverse(observation.seen_at_unix));
	values.truncate(MAX_PERSISTED_OBSERVATIONS);
	values
}

pub fn restore_observations(observations: Vec<PersistedContentFilterObservation>) {
	let Ok(mut state) = STATE.lock() else { return };
	state.observations.clear();
	for observation in observations.into_iter().take(MAX_PERSISTED_OBSERVATIONS) {
		if crate::post_components::PostId::parse(&observation.post_id).is_some() {
			state.observations.insert(observation.post_id.clone(), observation);
		}
	}
	rebuild_indexes(&mut state);
}

#[cfg(test)]
mod tests {
	use super::{
		marker_is_restorable, observe_post, promotion_quality_factor_from_counts, qualifying_filter_reasons, title_signal, DetectionState, PersistedPromoMarker,
		PROMOTION_AUTO_FILTER_THRESHOLD, REDGIFS_AUTO_FILTER_THRESHOLD, STATE,
	};
	use crate::utils::Post;
	use std::collections::{HashMap, HashSet};

	#[test]
	fn title_rules_match_promotional_terms_and_bait() {
		assert_eq!(title_signal("add me on Snapchat"), Some("social_promo"));
		assert_eq!(title_signal("TG: example"), Some("social_promo"));
		assert_eq!(title_signal("whoever upvtes gets it"), Some("vote_bait"));
		assert_eq!(title_signal("deleting in 24 hours"), Some("urgency_bait"));
	}

	#[test]
	fn title_rules_match_first_responder_nude_engagement_bait() {
		let examples = [
			("sending nudes 1st 79 guys upvtes and coment yes", "vote_bait"),
			("first50 to say yes gets my free nudes no joke", "engagement_bait"),
			("say no if u havent received any nudes from reddit", "engagement_bait"),
			("first 100 guys to drop me get my nude vids try me", "engagement_bait"),
			("F22 hey im sending 16 pics to the first 134", "engagement_bait"),
			("F21 can you guess what im sending to the first 80", "engagement_bait"),
			("F20 I have 8 pics for the first 87 people just say yes", "engagement_bait"),
			("F20 I have 8 pics for the first 87 people. Just say HEY", "engagement_bait"),
			("first 42 horny guys who say yo get 5 wet pussy pics", "engagement_bait"),
			("First50,,,,to say yes gets myfree,,,,,nu'des no joke", "engagement_bait"),
			("Say no if u haven't received any nudes from Redditors who promised, only the first 69", "engagement_bait"),
		];
		for (title, expected) in examples {
			assert_eq!(title_signal(title), Some(expected), "{title}");
		}
	}

	#[test]
	fn username_alone_is_neutral() {
		assert_eq!(title_signal("echoonayxis"), None);
		assert_eq!(title_signal("@yadiraskde"), None);
		assert_eq!(title_signal("my first 10 pictures from vacation"), None);
	}

	#[test]
	fn restore_invalidates_only_snapcodes_from_older_detector_versions() {
		let stale_snapcode = PersistedPromoMarker {
			post_id: "stalesnap1".to_string(),
			signal: "snapcode".to_string(),
			detector_version: "snap-promo-v2".to_string(),
			detected_at_unix: 1,
		};
		let old_title_marker = PersistedPromoMarker {
			post_id: "titlebait1".to_string(),
			signal: "engagement_bait".to_string(),
			detector_version: "snap-promo-v2".to_string(),
			detected_at_unix: 1,
		};
		assert!(!marker_is_restorable(&stale_snapcode));
		assert!(marker_is_restorable(&old_title_marker));
	}

	#[test]
	fn auto_filter_reasons_keep_promotion_and_redgifs_thresholds_separate() {
		let mut state = DetectionState::default();
		state.promo_by_author.insert(
			"promoter".to_string(),
			(0..PROMOTION_AUTO_FILTER_THRESHOLD).map(|index| format!("p{index}")).collect::<HashSet<_>>(),
		);
		state.redgifs_profiles_by_author.insert(
			"reposter".to_string(),
			(0..REDGIFS_AUTO_FILTER_THRESHOLD).map(|index| (format!("rg{index}"), 1)).collect::<HashMap<_, _>>(),
		);
		assert_eq!(qualifying_filter_reasons(&state, "promoter"), ["promotion_spam"]);
		assert_eq!(qualifying_filter_reasons(&state, "reposter"), ["redgifs_reposter"]);
	}

	#[test]
	fn promotion_penalty_grows_monotonically_with_spam_rate() {
		let clean = promotion_quality_factor_from_counts(100, 0);
		let some_spam = promotion_quality_factor_from_counts(100, 10);
		let heavy_spam = promotion_quality_factor_from_counts(100, 50);
		assert_eq!(clean, 1.0);
		assert!(clean > some_spam);
		assert!(some_spam > heavy_spam);
		assert_eq!(promotion_quality_factor_from_counts(0, 10), 1.0);
	}

	#[test]
	fn observed_thresholds_count_unique_posts_and_distinct_redgifs_profiles() {
		for index in 0..PROMOTION_AUTO_FILTER_THRESHOLD {
			let mut post = Post::default();
			post.id = format!("phase2p{index}");
			post.title = "first 50 to say yes gets my free nudes".to_string();
			post.community = "phase2stats".to_string();
			post.author.name = "phase2promoter".to_string();
			observe_post(&post);
			observe_post(&post);
		}
		for index in 0..REDGIFS_AUTO_FILTER_THRESHOLD {
			let mut post = Post::default();
			post.id = format!("phase2r{index}");
			post.title = "ordinary title".to_string();
			post.community = "phase2stats".to_string();
			post.author.name = "phase2reposter".to_string();
			post.media.redgifs_username = format!("borrowedcreator{index}");
			observe_post(&post);
		}

		let state = STATE.lock().expect("detector state");
		assert_eq!(state.promo_by_author["phase2promoter"].len(), PROMOTION_AUTO_FILTER_THRESHOLD);
		assert_eq!(state.redgifs_profiles_by_author["phase2reposter"].len(), REDGIFS_AUTO_FILTER_THRESHOLD);
		assert_eq!(qualifying_filter_reasons(&state, "phase2promoter"), ["promotion_spam"]);
		assert_eq!(qualifying_filter_reasons(&state, "phase2reposter"), ["redgifs_reposter"]);
	}
}
