use hyper::{header, Body, Request, Response, StatusCode};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{mpsc, LazyLock, Mutex};
use std::time::Instant;

const DETECTOR_VERSION: &str = "snap-promo-v1";
const MAX_ANALYSIS_BYTES: usize = 8 * 1024 * 1024;
const MAX_MEDIA_MAPPINGS: usize = 4096;
const MAX_PERSISTED_MARKERS: usize = 100_000;

static SOCIAL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(?:\bsnap(?:chat)?\b|\bsnp\b|\btelegram\b|\btg\b|\bt\.me\b)").unwrap());
static VOTE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bupv(?:ote|otes|oted|oting|te|tes|et|ets)\b").unwrap());
static URGENCY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bdelet\w*\s+(?:in|within)\s+\[?\d{1,3}\]?\s*(?:h|hr|hrs|hours?|days?)\b").unwrap());

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedPromoMarker {
	pub post_id: String,
	pub signal: String,
	pub detector_version: String,
	pub detected_at_unix: u64,
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

static MARKERS: LazyLock<Mutex<HashMap<String, PersistedPromoMarker>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
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
	} else {
		None
	}
}

fn mark(post_id: &str, signal: &str) {
	if crate::post_components::PostId::parse(post_id).is_none() {
		return;
	}
	let Ok(mut markers) = MARKERS.lock() else { return };
	if markers.contains_key(post_id) {
		return;
	}
	markers.insert(
		post_id.to_string(),
		PersistedPromoMarker {
			post_id: post_id.to_string(),
			signal: signal.to_string(),
			detector_version: DETECTOR_VERSION.to_string(),
			detected_at_unix: crate::cache_persistence::unix_now(),
		},
	);
	drop(markers);
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
	MARKERS.lock().ok()?.get(post_id).cloned()
}

pub async fn status(req: Request<Body>) -> Result<Response<Body>, String> {
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
	for id in ids.split(',').take(100) {
		let Some(id) = crate::post_components::PostId::parse(id) else { continue };
		let id = id.to_string();
		if marker_for(&id).is_none() {
			if let Some(snapshot) = crate::post_components::latest(&id) {
				if let Some(signal) = title_signal(&snapshot.stable.title) {
					mark(&id, signal);
				}
			}
		}
		if let Some(marker) = marker_for(&id) {
			signals.insert(id, json!({ "signal": marker.signal }));
		}
	}
	Response::builder()
		.status(StatusCode::OK)
		.header(header::CONTENT_TYPE, "application/json; charset=utf-8")
		.header(header::CACHE_CONTROL, "private, no-store")
		.body(Body::from(json!({ "signals": signals }).to_string()))
		.map_err(|error| error.to_string())
}

pub fn snapshot_markers() -> Vec<PersistedPromoMarker> {
	let mut values = MARKERS.lock().map(|markers| markers.values().cloned().collect::<Vec<_>>()).unwrap_or_default();
	values.sort_by_key(|marker| std::cmp::Reverse(marker.detected_at_unix));
	values.truncate(MAX_PERSISTED_MARKERS);
	values
}

pub fn restore_markers(markers: Vec<PersistedPromoMarker>) {
	let Ok(mut state) = MARKERS.lock() else { return };
	state.clear();
	for marker in markers.into_iter().take(MAX_PERSISTED_MARKERS) {
		if crate::post_components::PostId::parse(&marker.post_id).is_some() {
			state.insert(marker.post_id.clone(), marker);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::title_signal;

	#[test]
	fn title_rules_match_promotional_terms_and_bait() {
		assert_eq!(title_signal("add me on Snapchat"), Some("social_promo"));
		assert_eq!(title_signal("TG: example"), Some("social_promo"));
		assert_eq!(title_signal("whoever upvtes gets it"), Some("vote_bait"));
		assert_eq!(title_signal("deleting in 24 hours"), Some("urgency_bait"));
	}

	#[test]
	fn username_alone_is_neutral() {
		assert_eq!(title_signal("echoonayxis"), None);
		assert_eq!(title_signal("@yadiraskde"), None);
	}
}
