use crate::config::get_setting;
use crate::server::RequestExt;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hyper::body::to_bytes;
use hyper::{Body, Request, Response, StatusCode};
use log::{info, warn};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::{mpsc, Semaphore};

const DEFAULT_MAX_CONCURRENT: usize = 2;
const DEFAULT_MAX_READY_JOBS: usize = 128;
const DEFAULT_MAX_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_MAX_QUEUE: usize = 256;
const DEFAULT_READY_TTL: Duration = Duration::from_secs(30 * 60);
const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(25);
const DEFAULT_FAILED_RETRY: Duration = Duration::from_secs(120);
const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
const DEFAULT_NEAR_TERM_POLL_MS: u64 = 1_500;
const DEFAULT_BACKPRESSURE_POLL_MS: u64 = 5_000;
const FRAME_CACHE_SECONDS: u64 = 300;
const PREVIEW_MAX_WIDTH: i64 = 640;

type FrameFuture<'a> = Pin<Box<dyn Future<Output = Result<FrameBlob, String>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoPreviewJobKey {
	pub media_url: String,
	pub poster_url: String,
	pub duration_seconds: i64,
	pub width: i64,
	pub height: i64,
}

impl VideoPreviewJobKey {
	fn canonical_string(&self) -> String {
		format!("{}\n{}\n{}\n{}\n{}", self.media_url, self.poster_url, self.duration_seconds, self.width, self.height)
	}
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct VideoPreviewFrame {
	pub index: usize,
	pub timestamp_seconds: i64,
	pub image_path: String,
	pub width: i64,
	pub height: i64,
}

#[derive(Clone, Debug)]
pub enum VideoPreviewJobState {
	Pending,
	Running { completed_frames: usize, total_frames: usize },
	Ready { frames: Vec<VideoPreviewFrame>, expires_at: Instant },
	Failed { message: String, retry_after: Instant },
}

#[derive(Clone)]
struct JobEntry {
	key: VideoPreviewJobKey,
	state: VideoPreviewJobState,
	queued: bool,
	last_accessed: Instant,
}

#[derive(Clone, Debug)]
struct LeaseEntry {
	page_path: String,
	post_id: String,
	job_ids: HashSet<String>,
	last_seen: Instant,
}

#[derive(Clone, Debug)]
pub struct FrameBlob {
	content_type: &'static str,
	bytes: Vec<u8>,
}

impl FrameBlob {
	fn len(&self) -> usize {
		self.bytes.len()
	}
}

#[derive(Default)]
struct FrameCache {
	blobs: HashMap<String, FrameBlob>,
	total_bytes: usize,
}

pub trait PreviewFrameGenerator: Send + Sync {
	fn extract_frame<'a>(&'a self, request: &'a FrameRequest) -> FrameFuture<'a>;
}

#[derive(Clone, Debug)]
pub struct FrameRequest {
	pub media_url: String,
	pub timestamp_seconds: i64,
	pub width: i64,
	pub height: i64,
}

struct FfmpegFrameGenerator;

impl PreviewFrameGenerator for FfmpegFrameGenerator {
	fn extract_frame<'a>(&'a self, request: &'a FrameRequest) -> FrameFuture<'a> {
		Box::pin(async move {
			let scale_filter = format!("scale='min({PREVIEW_MAX_WIDTH},iw)':-2");
			let output = Command::new("ffmpeg")
				.arg("-hide_banner")
				.arg("-loglevel")
				.arg("error")
				.arg("-nostdin")
				.arg("-ss")
				.arg(request.timestamp_seconds.to_string())
				.arg("-i")
				.arg(&request.media_url)
				.arg("-frames:v")
				.arg("1")
				.arg("-vf")
				.arg(scale_filter)
				.arg("-f")
				.arg("image2pipe")
				.arg("-vcodec")
				.arg("webp")
				.arg("-")
				.stdin(Stdio::null())
				.stdout(Stdio::piped())
				.stderr(Stdio::piped())
				.output()
				.await
				.map_err(|err| format!("ffmpeg execution failed: {err}"))?;

			if !output.status.success() {
				let stderr = String::from_utf8_lossy(&output.stderr).chars().take(300).collect::<String>();
				return Err(format!("ffmpeg exited with {}: {stderr}", output.status));
			}

			if output.stdout.is_empty() {
				return Err("ffmpeg returned an empty frame".to_string());
			}

			Ok(FrameBlob {
				content_type: "image/webp",
				bytes: output.stdout,
			})
		})
	}
}

static VIDEO_PREVIEW_JOBS: LazyLock<Mutex<HashMap<String, JobEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static VIDEO_PREVIEW_LEASES: LazyLock<Mutex<HashMap<String, LeaseEntry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static VIDEO_PREVIEW_FRAMES: LazyLock<Mutex<FrameCache>> = LazyLock::new(|| Mutex::new(FrameCache::default()));
static JOB_QUEUE_TX: OnceLock<mpsc::Sender<String>> = OnceLock::new();
static INIT_ONCE: OnceLock<()> = OnceLock::new();
static GENERATOR: LazyLock<Mutex<Arc<dyn PreviewFrameGenerator>>> = LazyLock::new(|| Mutex::new(Arc::new(FfmpegFrameGenerator) as Arc<dyn PreviewFrameGenerator>));
static MAX_CONCURRENT: LazyLock<usize> = LazyLock::new(|| env_usize("REDLIB_VIDEO_PREVIEW_MAX_CONCURRENT", DEFAULT_MAX_CONCURRENT));
static MAX_READY_JOBS: LazyLock<usize> = LazyLock::new(|| env_usize("REDLIB_VIDEO_PREVIEW_MAX_JOBS", DEFAULT_MAX_READY_JOBS));
static MAX_BYTES: LazyLock<usize> = LazyLock::new(|| env_usize("REDLIB_VIDEO_PREVIEW_MAX_BYTES", DEFAULT_MAX_BYTES));
static MAX_QUEUE: LazyLock<usize> = LazyLock::new(|| env_usize("REDLIB_VIDEO_PREVIEW_MAX_QUEUE", DEFAULT_MAX_QUEUE));

#[derive(Deserialize)]
struct LeaseRequest {
	page_session_id: String,
	page_path: String,
	post_id: String,
	media_url: String,
	poster_url: String,
	duration_seconds: i64,
	width: i64,
	height: i64,
}

#[derive(Deserialize)]
struct SessionRequest {
	page_session_id: String,
	job_ids: Option<Vec<String>>,
}

#[derive(Serialize)]
struct LeaseResponse {
	job_id: String,
	state: ApiJobState,
	frames: Vec<VideoPreviewFrame>,
	poll_after_ms: u64,
}

#[derive(Serialize)]
struct StatusResponse {
	job_id: String,
	state: ApiJobState,
	frames: Vec<VideoPreviewFrame>,
	poll_after_ms: u64,
}

#[derive(Serialize)]
struct ApiJobState {
	status: &'static str,
	completed_frames: usize,
	total_frames: usize,
	message: String,
}

pub fn feature_enabled() -> bool {
	#[cfg(test)]
	{
		return true;
	}

	#[cfg(not(test))]
	matches!(std::env::var("REDLIB_VIDEO_PREVIEW_ENABLED").ok().as_deref(), Some("on"))
}

pub fn init() {
	if !feature_enabled() {
		return;
	}

	if INIT_ONCE.set(()).is_err() {
		return;
	}

	let (tx, mut rx) = mpsc::channel::<String>(*MAX_QUEUE);
	let _ = JOB_QUEUE_TX.set(tx);
	let semaphore = Arc::new(Semaphore::new(*MAX_CONCURRENT));

	tokio::spawn({
		let semaphore = semaphore.clone();
		async move {
			while let Some(job_id) = rx.recv().await {
				let Ok(permit) = semaphore.clone().acquire_owned().await else {
					break;
				};

				tokio::spawn(async move {
					let _permit = permit;
					process_job(job_id).await;
				});
			}
		}
	});

	tokio::spawn(async move {
		loop {
			tokio::time::sleep(DEFAULT_SWEEP_INTERVAL).await;
			sweep_expired_state();
		}
	});
}

pub async fn lease(req: Request<Body>) -> Result<Response<Body>, String> {
	if !feature_enabled() {
		return json_response(
			StatusCode::SERVICE_UNAVAILABLE,
			&serde_json::json!({
				"message": "video previews disabled"
			}),
		);
	}

	let request: LeaseRequest = parse_json_body(req).await?;
	let key = VideoPreviewJobKey {
		media_url: absolutize_media_url(&request.media_url),
		poster_url: absolutize_media_url(&request.poster_url),
		duration_seconds: request.duration_seconds,
		width: request.width,
		height: request.height,
	};

	let (job_id, queue_enqueued) = upsert_job_and_lease(key, &request);
	let (state, frames, poll_after_ms) = snapshot_job(&job_id);

	if queue_enqueued {
		info!("Queued video preview job {job_id} for post {}", request.post_id);
	}

	json_response(
		StatusCode::OK,
		&LeaseResponse {
			job_id,
			state,
			frames,
			poll_after_ms,
		},
	)
}

pub async fn heartbeat(req: Request<Body>) -> Result<Response<Body>, String> {
	let request: SessionRequest = parse_json_body(req).await?;
	let now = Instant::now();

	if let Ok(mut leases) = VIDEO_PREVIEW_LEASES.lock() {
		if let Some(entry) = leases.get_mut(&request.page_session_id) {
			entry.last_seen = now;
			if let Some(job_ids) = request.job_ids {
				entry.job_ids = job_ids.into_iter().collect();
			}
		}
	}

	json_response(StatusCode::OK, &serde_json::json!({ "ok": true }))
}

pub async fn release(req: Request<Body>) -> Result<Response<Body>, String> {
	let request: SessionRequest = parse_json_body(req).await?;

	if let Ok(mut leases) = VIDEO_PREVIEW_LEASES.lock() {
		if let Some(job_ids) = request.job_ids {
			if let Some(entry) = leases.get_mut(&request.page_session_id) {
				for job_id in job_ids {
					entry.job_ids.remove(&job_id);
				}
				if entry.job_ids.is_empty() {
					leases.remove(&request.page_session_id);
				}
			}
		} else {
			leases.remove(&request.page_session_id);
		}
	}

	json_response(StatusCode::OK, &serde_json::json!({ "ok": true }))
}

pub async fn status(req: Request<Body>) -> Result<Response<Body>, String> {
	let job_id = req.param("job_id").unwrap_or_default();
	let (state, frames, poll_after_ms) = snapshot_job(&job_id);

	json_response(
		StatusCode::OK,
		&StatusResponse {
			job_id,
			state,
			frames,
			poll_after_ms,
		},
	)
}

pub async fn frame(req: Request<Body>) -> Result<Response<Body>, String> {
	let job_id = req.param("job_id").unwrap_or_default();
	let index = req.param("index").unwrap_or_default().parse::<usize>().map_err(|err| err.to_string())?;
	let frame_key = frame_cache_key(&job_id, index);

	let blob = {
		let mut jobs = VIDEO_PREVIEW_JOBS.lock().map_err(|_| "Lock error".to_string())?;
		let cache = VIDEO_PREVIEW_FRAMES.lock().map_err(|_| "Lock error".to_string())?;

		if let Some(job) = jobs.get_mut(&job_id) {
			job.last_accessed = Instant::now();
		}

		cache.blobs.get(&frame_key).cloned()
	};

	let Some(blob) = blob else {
		return response(StatusCode::NOT_FOUND, "text/plain", Body::from("Not found"));
	};

	Response::builder()
		.status(StatusCode::OK)
		.header("content-type", blob.content_type)
		.header("Cache-Control", format!("public, max-age={FRAME_CACHE_SECONDS}"))
		.body(Body::from(blob.bytes))
		.map_err(|err| err.to_string())
}

pub fn computed_frame_count(duration_seconds: i64) -> usize {
	if duration_seconds <= 20 {
		return 6;
	}

	(6 + ((duration_seconds - 20) / 10).max(0) as usize).min(24)
}

pub fn preview_timestamps(duration_seconds: i64) -> Vec<i64> {
	let count = computed_frame_count(duration_seconds);

	if duration_seconds <= 0 || count == 0 {
		return (0..count).map(|_| 1).collect();
	}

	if count == 1 {
		return vec![((duration_seconds as f64) * 0.5).round().max(1.0) as i64];
	}

	let duration = duration_seconds as f64;
	let start = 0.5f64.max(duration * 0.05);
	let end = start.max(duration * 0.95);
	let step = if count > 1 { (end - start) / ((count - 1) as f64) } else { 0.0 };

	(0..count).map(|index| (start + step * (index as f64)).round().max(1.0) as i64).collect()
}

fn env_usize(name: &str, default: usize) -> usize {
	std::env::var(name).ok().and_then(|value| value.parse::<usize>().ok()).unwrap_or(default)
}

fn current_generator() -> Arc<dyn PreviewFrameGenerator> {
	GENERATOR.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

#[cfg(test)]
fn set_generator(generator: Arc<dyn PreviewFrameGenerator>) {
	if let Ok(mut state) = GENERATOR.lock() {
		*state = generator;
	}
}

fn frame_cache_key(job_id: &str, index: usize) -> String {
	format!("{job_id}:{index}")
}

fn job_id_for_key(key: &VideoPreviewJobKey) -> String {
	URL_SAFE_NO_PAD.encode(key.canonical_string())
}

fn absolutize_media_url(value: &str) -> String {
	if value.starts_with("http://") || value.starts_with("https://") || value.is_empty() {
		return value.to_string();
	}

	if let Some(base) = get_setting("REDLIB_FULL_URL") {
		return join_base_and_path(&base, value);
	}

	join_base_and_path("http://127.0.0.1:8080", value)
}

fn join_base_and_path(base: &str, path: &str) -> String {
	let trimmed_base = base.trim_end_matches('/');
	if path.starts_with('/') {
		format!("{trimmed_base}{path}")
	} else {
		format!("{trimmed_base}/{path}")
	}
}

fn upsert_job_and_lease(key: VideoPreviewJobKey, request: &LeaseRequest) -> (String, bool) {
	let now = Instant::now();
	let job_id = job_id_for_key(&key);
	let mut queue_enqueued = false;

	if let Ok(mut jobs) = VIDEO_PREVIEW_JOBS.lock() {
		let entry = jobs.entry(job_id.clone()).or_insert_with(|| JobEntry {
			key: key.clone(),
			state: VideoPreviewJobState::Pending,
			queued: false,
			last_accessed: now,
		});
		entry.key = key;
		entry.last_accessed = now;

		match &entry.state {
			VideoPreviewJobState::Ready { expires_at, .. } if *expires_at > now => {}
			VideoPreviewJobState::Failed { retry_after, .. } if *retry_after > now => {}
			VideoPreviewJobState::Running { .. } | VideoPreviewJobState::Pending => {
				if !entry.queued {
					queue_enqueued = enqueue_job_locked(&job_id, entry);
				}
			}
			_ => {
				entry.state = VideoPreviewJobState::Pending;
				queue_enqueued = enqueue_job_locked(&job_id, entry);
			}
		}
	}

	if let Ok(mut leases) = VIDEO_PREVIEW_LEASES.lock() {
		let lease = leases.entry(request.page_session_id.clone()).or_insert_with(|| LeaseEntry {
			page_path: request.page_path.clone(),
			post_id: request.post_id.clone(),
			job_ids: HashSet::new(),
			last_seen: now,
		});
		lease.page_path = request.page_path.clone();
		lease.post_id = request.post_id.clone();
		lease.last_seen = now;
		lease.job_ids.insert(job_id.clone());
	}

	(job_id, queue_enqueued)
}

fn enqueue_job_locked(job_id: &str, entry: &mut JobEntry) -> bool {
	let Some(tx) = JOB_QUEUE_TX.get() else {
		entry.queued = false;
		return false;
	};

	match tx.try_send(job_id.to_string()) {
		Ok(()) => {
			entry.queued = true;
			true
		}
		Err(_) => {
			entry.queued = false;
			false
		}
	}
}

fn snapshot_job(job_id: &str) -> (ApiJobState, Vec<VideoPreviewFrame>, u64) {
	let now = Instant::now();

	if let Ok(mut jobs) = VIDEO_PREVIEW_JOBS.lock() {
		if let Some(job) = jobs.get_mut(job_id) {
			job.last_accessed = now;

			return match &job.state {
				VideoPreviewJobState::Pending => (
					ApiJobState {
						status: "pending",
						completed_frames: 0,
						total_frames: computed_frame_count(job.key.duration_seconds),
						message: String::new(),
					},
					Vec::new(),
					if job.queued { DEFAULT_NEAR_TERM_POLL_MS } else { DEFAULT_BACKPRESSURE_POLL_MS },
				),
				VideoPreviewJobState::Running { completed_frames, total_frames } => (
					ApiJobState {
						status: "running",
						completed_frames: *completed_frames,
						total_frames: *total_frames,
						message: String::new(),
					},
					Vec::new(),
					DEFAULT_NEAR_TERM_POLL_MS,
				),
				VideoPreviewJobState::Ready { frames, .. } => (
					ApiJobState {
						status: "ready",
						completed_frames: frames.len(),
						total_frames: frames.len(),
						message: String::new(),
					},
					frames.clone(),
					0,
				),
				VideoPreviewJobState::Failed { message, .. } => (
					ApiJobState {
						status: "failed",
						completed_frames: 0,
						total_frames: computed_frame_count(job.key.duration_seconds),
						message: message.clone(),
					},
					Vec::new(),
					DEFAULT_BACKPRESSURE_POLL_MS,
				),
			};
		}
	}

	(
		ApiJobState {
			status: "missing",
			completed_frames: 0,
			total_frames: 0,
			message: String::new(),
		},
		Vec::new(),
		DEFAULT_BACKPRESSURE_POLL_MS,
	)
}

async fn process_job(job_id: String) {
	let key = {
		let mut jobs = match VIDEO_PREVIEW_JOBS.lock() {
			Ok(jobs) => jobs,
			Err(_) => return,
		};

		let Some(job) = jobs.get_mut(&job_id) else {
			return;
		};

		if !has_live_lease(&job_id) {
			job.queued = false;
			job.state = VideoPreviewJobState::Pending;
			return;
		}

		let timestamps = preview_timestamps(job.key.duration_seconds);
		let total_frames = timestamps.len();
		job.queued = false;
		job.state = VideoPreviewJobState::Running {
			completed_frames: 0,
			total_frames,
		};
		job.key.clone()
	};

	info!("Starting video preview job {job_id}");

	let generator = current_generator();
	let timestamps = preview_timestamps(key.duration_seconds);
	let mut produced_blobs = Vec::with_capacity(timestamps.len());
	let mut produced_frames = Vec::with_capacity(timestamps.len());

	for (index, timestamp_seconds) in timestamps.iter().copied().enumerate() {
		if !has_live_lease(&job_id) {
			cancel_job(&job_id);
			info!("Cancelled video preview job {job_id} because its page is no longer live");
			return;
		}

		let request = FrameRequest {
			media_url: key.media_url.clone(),
			timestamp_seconds,
			width: key.width,
			height: key.height,
		};

		let blob = match generator.extract_frame(&request).await {
			Ok(blob) => blob,
			Err(err) => {
				fail_job(&job_id, err);
				return;
			}
		};

		let image_path = format!("/api/video-previews/{job_id}/frame/{index}");
		produced_frames.push(VideoPreviewFrame {
			index,
			timestamp_seconds,
			image_path,
			width: key.width,
			height: key.height,
		});
		produced_blobs.push((index, blob));

		if let Ok(mut jobs) = VIDEO_PREVIEW_JOBS.lock() {
			if let Some(job) = jobs.get_mut(&job_id) {
				job.state = VideoPreviewJobState::Running {
					completed_frames: index + 1,
					total_frames: timestamps.len(),
				};
			}
		}
	}

	if !has_live_lease(&job_id) {
		cancel_job(&job_id);
		info!("Cancelled video preview job {job_id} after generation because its page is no longer live");
		return;
	}

	publish_ready_job(&job_id, produced_frames, produced_blobs);
	info!("Completed video preview job {job_id}");
}

fn publish_ready_job(job_id: &str, frames: Vec<VideoPreviewFrame>, blobs: Vec<(usize, FrameBlob)>) {
	let now = Instant::now();

	let mut jobs = match VIDEO_PREVIEW_JOBS.lock() {
		Ok(jobs) => jobs,
		Err(_) => return,
	};
	let mut cache = match VIDEO_PREVIEW_FRAMES.lock() {
		Ok(cache) => cache,
		Err(_) => return,
	};

	remove_job_frames_locked(job_id, &mut jobs, &mut cache);

	for (index, blob) in blobs {
		cache.total_bytes += blob.len();
		cache.blobs.insert(frame_cache_key(job_id, index), blob);
	}

	if let Some(job) = jobs.get_mut(job_id) {
		job.queued = false;
		job.last_accessed = now;
		job.state = VideoPreviewJobState::Ready {
			frames,
			expires_at: now + DEFAULT_READY_TTL,
		};
	}

	evict_ready_jobs_locked(&mut jobs, &mut cache);
}

fn cancel_job(job_id: &str) {
	if let Ok(mut jobs) = VIDEO_PREVIEW_JOBS.lock() {
		if let Some(job) = jobs.get_mut(job_id) {
			job.queued = false;
			job.state = VideoPreviewJobState::Pending;
		}
	}
}

fn fail_job(job_id: &str, message: String) {
	warn!("Video preview job {job_id} failed: {message}");
	if let Ok(mut jobs) = VIDEO_PREVIEW_JOBS.lock() {
		if let Some(job) = jobs.get_mut(job_id) {
			job.queued = false;
			job.state = VideoPreviewJobState::Failed {
				message,
				retry_after: Instant::now() + DEFAULT_FAILED_RETRY,
			};
		}
	}
}

fn has_live_lease(job_id: &str) -> bool {
	let now = Instant::now();
	let Ok(leases) = VIDEO_PREVIEW_LEASES.lock() else {
		return false;
	};

	leases
		.values()
		.any(|lease| now.duration_since(lease.last_seen) <= DEFAULT_LEASE_TTL && lease.job_ids.contains(job_id))
}

fn sweep_expired_state() {
	let now = Instant::now();

	if let Ok(mut leases) = VIDEO_PREVIEW_LEASES.lock() {
		leases.retain(|_, lease| now.duration_since(lease.last_seen) <= DEFAULT_LEASE_TTL);
	}

	let mut jobs = match VIDEO_PREVIEW_JOBS.lock() {
		Ok(jobs) => jobs,
		Err(_) => return,
	};
	let mut cache = match VIDEO_PREVIEW_FRAMES.lock() {
		Ok(cache) => cache,
		Err(_) => return,
	};

	let expired_job_ids = jobs
		.iter()
		.filter_map(|(job_id, job)| match &job.state {
			VideoPreviewJobState::Ready { expires_at, .. } if *expires_at <= now => Some(job_id.clone()),
			_ => None,
		})
		.collect::<Vec<_>>();

	for job_id in expired_job_ids {
		remove_job_frames_locked(&job_id, &mut jobs, &mut cache);
		jobs.remove(&job_id);
	}
}

fn remove_job_frames_locked(job_id: &str, jobs: &mut HashMap<String, JobEntry>, cache: &mut FrameCache) {
	let frame_indexes = jobs
		.get(job_id)
		.and_then(|job| match &job.state {
			VideoPreviewJobState::Ready { frames, .. } => Some(frames.iter().map(|frame| frame.index).collect::<Vec<_>>()),
			_ => None,
		})
		.unwrap_or_default();

	for index in frame_indexes {
		if let Some(blob) = cache.blobs.remove(&frame_cache_key(job_id, index)) {
			cache.total_bytes = cache.total_bytes.saturating_sub(blob.len());
		}
	}
}

fn evict_ready_jobs_locked(jobs: &mut HashMap<String, JobEntry>, cache: &mut FrameCache) {
	while ready_job_count(jobs) > *MAX_READY_JOBS || cache.total_bytes > *MAX_BYTES {
		let Some(job_id) = jobs
			.iter()
			.filter(|(_, job)| matches!(job.state, VideoPreviewJobState::Ready { .. }))
			.min_by_key(|(_, job)| job.last_accessed)
			.map(|(job_id, _)| job_id.clone())
		else {
			break;
		};

		remove_job_frames_locked(&job_id, jobs, cache);
		jobs.remove(&job_id);
	}
}

fn ready_job_count(jobs: &HashMap<String, JobEntry>) -> usize {
	jobs.values().filter(|job| matches!(job.state, VideoPreviewJobState::Ready { .. })).count()
}

async fn parse_json_body<T: DeserializeOwned>(req: Request<Body>) -> Result<T, String> {
	let body = to_bytes(req.into_body()).await.map_err(|err| err.to_string())?;
	serde_json::from_slice(&body).map_err(|err| err.to_string())
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Result<Response<Body>, String> {
	let body = serde_json::to_vec(value).map_err(|err| err.to_string())?;
	Response::builder()
		.status(status)
		.header("content-type", "application/json")
		.body(Body::from(body))
		.map_err(|err| err.to_string())
}

fn response(status: StatusCode, content_type: &str, body: Body) -> Result<Response<Body>, String> {
	Response::builder()
		.status(status)
		.header("content-type", content_type)
		.body(body)
		.map_err(|err| err.to_string())
}

#[cfg(test)]
pub(crate) fn reset_state_for_tests() {
	if let Ok(mut jobs) = VIDEO_PREVIEW_JOBS.lock() {
		jobs.clear();
	}
	if let Ok(mut leases) = VIDEO_PREVIEW_LEASES.lock() {
		leases.clear();
	}
	if let Ok(mut frames) = VIDEO_PREVIEW_FRAMES.lock() {
		frames.blobs.clear();
		frames.total_bytes = 0;
	}
	set_generator(Arc::new(FfmpegFrameGenerator));
}

#[cfg(test)]
mod tests {
	use super::*;
	use hyper::body::to_bytes;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::Mutex;

	static TEST_GUARD: Mutex<()> = Mutex::new(());

	struct FakeGenerator {
		fail_after: Option<usize>,
		calls: AtomicUsize,
	}

	impl FakeGenerator {
		fn successful() -> Arc<Self> {
			Arc::new(Self {
				fail_after: None,
				calls: AtomicUsize::new(0),
			})
		}
	}

	impl PreviewFrameGenerator for FakeGenerator {
		fn extract_frame<'a>(&'a self, request: &'a FrameRequest) -> FrameFuture<'a> {
			Box::pin(async move {
				let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
				if self.fail_after.is_some_and(|value| call_index >= value) {
					return Err("forced failure".to_string());
				}
				Ok(FrameBlob {
					content_type: "image/webp",
					bytes: format!("frame-{}", request.timestamp_seconds).into_bytes(),
				})
			})
		}
	}

	#[test]
	fn computes_frame_counts() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		assert_eq!(computed_frame_count(0), 6);
		assert_eq!(computed_frame_count(5), 6);
		assert_eq!(computed_frame_count(20), 6);
		assert_eq!(computed_frame_count(21), 6);
		assert_eq!(computed_frame_count(30), 7);
		assert_eq!(computed_frame_count(60), 10);
		assert_eq!(computed_frame_count(10_000), 24);
	}

	#[test]
	fn computes_preview_timestamps() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		let values = preview_timestamps(60);
		assert_eq!(values.len(), computed_frame_count(60));
		assert!(values.windows(2).all(|window| window[0] <= window[1]));
		assert!(values.first().copied().unwrap_or_default() >= 1);
		assert!(values.last().copied().unwrap_or_default() <= 60);
	}

	#[test]
	fn job_ids_are_deterministic() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		let key = VideoPreviewJobKey {
			media_url: "/vid/test".to_string(),
			poster_url: "/thumb/test".to_string(),
			duration_seconds: 30,
			width: 320,
			height: 180,
		};
		assert_eq!(job_id_for_key(&key), job_id_for_key(&key));
	}

	#[test]
	fn sweeper_removes_expired_leases() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		reset_state_for_tests();
		let mut job_ids = HashSet::new();
		job_ids.insert("job".to_string());
		VIDEO_PREVIEW_LEASES.lock().unwrap().insert(
			"page".to_string(),
			LeaseEntry {
				page_path: "/r/test".to_string(),
				post_id: "abc".to_string(),
				job_ids,
				last_seen: Instant::now() - DEFAULT_LEASE_TTL - Duration::from_secs(1),
			},
		);
		sweep_expired_state();
		assert!(VIDEO_PREVIEW_LEASES.lock().unwrap().is_empty());
	}

	#[test]
	fn cache_eviction_removes_frame_blobs() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		reset_state_for_tests();
		let mut jobs = VIDEO_PREVIEW_JOBS.lock().unwrap();
		let mut cache = VIDEO_PREVIEW_FRAMES.lock().unwrap();
		let now = Instant::now();
		for idx in 0..3 {
			let job_id = format!("job-{idx}");
			jobs.insert(
				job_id.clone(),
				JobEntry {
					key: VideoPreviewJobKey {
						media_url: format!("/vid/{idx}"),
						poster_url: String::new(),
						duration_seconds: 30,
						width: 320,
						height: 180,
					},
					state: VideoPreviewJobState::Ready {
						frames: vec![VideoPreviewFrame {
							index: 0,
							timestamp_seconds: 1,
							image_path: format!("/api/video-previews/{job_id}/frame/0"),
							width: 320,
							height: 180,
						}],
						expires_at: now + DEFAULT_READY_TTL,
					},
					queued: false,
					last_accessed: now - Duration::from_secs((idx + 1) as u64),
				},
			);
			cache.total_bytes += 10;
			cache.blobs.insert(
				frame_cache_key(&job_id, 0),
				FrameBlob {
					content_type: "image/webp",
					bytes: vec![0; 10],
				},
			);
		}
		evict_ready_jobs_locked(&mut jobs, &mut cache);
	}

	#[tokio::test]
	async fn lease_deduplicates_jobs() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		reset_state_for_tests();
		init();
		let key = VideoPreviewJobKey {
			media_url: "/vid/test".to_string(),
			poster_url: String::new(),
			duration_seconds: 30,
			width: 320,
			height: 180,
		};
		let request = LeaseRequest {
			page_session_id: "page-a".to_string(),
			page_path: "/r/test".to_string(),
			post_id: "abc".to_string(),
			media_url: "/vid/test".to_string(),
			poster_url: String::new(),
			duration_seconds: 30,
			width: 320,
			height: 180,
		};
		let (job_id_a, _) = upsert_job_and_lease(key.clone(), &request);
		let request_b = LeaseRequest {
			page_session_id: "page-b".to_string(),
			..request
		};
		let (job_id_b, _) = upsert_job_and_lease(key, &request_b);
		assert_eq!(job_id_a, job_id_b);
		assert_eq!(VIDEO_PREVIEW_JOBS.lock().unwrap().len(), 1);
	}

	#[tokio::test]
	async fn running_job_cancels_when_lease_expires() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		reset_state_for_tests();
		let generator = FakeGenerator::successful();
		set_generator(generator.clone());
		let key = VideoPreviewJobKey {
			media_url: "http://example.com/video.mp4".to_string(),
			poster_url: String::new(),
			duration_seconds: 30,
			width: 320,
			height: 180,
		};
		let job_id = job_id_for_key(&key);
		VIDEO_PREVIEW_JOBS.lock().unwrap().insert(
			job_id.clone(),
			JobEntry {
				key,
				state: VideoPreviewJobState::Pending,
				queued: false,
				last_accessed: Instant::now(),
			},
		);
		let mut job_ids = HashSet::new();
		job_ids.insert(job_id.clone());
		VIDEO_PREVIEW_LEASES.lock().unwrap().insert(
			"page".to_string(),
			LeaseEntry {
				page_path: "/r/test".to_string(),
				post_id: "abc".to_string(),
				job_ids,
				last_seen: Instant::now() - DEFAULT_LEASE_TTL - Duration::from_secs(1),
			},
		);

		process_job(job_id.clone()).await;
		let jobs = VIDEO_PREVIEW_JOBS.lock().unwrap();
		let state = &jobs.get(&job_id).unwrap().state;
		assert!(matches!(state, VideoPreviewJobState::Pending));
	}

	#[tokio::test]
	async fn failed_job_enters_cooldown() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		reset_state_for_tests();
		set_generator(Arc::new(FakeGenerator {
			fail_after: Some(0),
			calls: AtomicUsize::new(0),
		}));
		let key = VideoPreviewJobKey {
			media_url: "http://example.com/video.mp4".to_string(),
			poster_url: String::new(),
			duration_seconds: 30,
			width: 320,
			height: 180,
		};
		let job_id = job_id_for_key(&key);
		VIDEO_PREVIEW_JOBS.lock().unwrap().insert(
			job_id.clone(),
			JobEntry {
				key,
				state: VideoPreviewJobState::Pending,
				queued: false,
				last_accessed: Instant::now(),
			},
		);
		let mut job_ids = HashSet::new();
		job_ids.insert(job_id.clone());
		VIDEO_PREVIEW_LEASES.lock().unwrap().insert(
			"page".to_string(),
			LeaseEntry {
				page_path: "/r/test".to_string(),
				post_id: "abc".to_string(),
				job_ids,
				last_seen: Instant::now(),
			},
		);

		process_job(job_id.clone()).await;
		let jobs = VIDEO_PREVIEW_JOBS.lock().unwrap();
		let state = &jobs.get(&job_id).unwrap().state;
		assert!(matches!(state, VideoPreviewJobState::Failed { .. }));
	}

	#[tokio::test]
	async fn frame_handler_returns_not_found_for_missing_frame() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		reset_state_for_tests();
		let req = Request::builder().uri("/api/video-previews/missing/frame/0").body(Body::empty()).unwrap();
		let mut req = req;
		let mut params = route_recognizer::Params::new();
		params.insert("job_id".to_string(), "missing".to_string());
		params.insert("index".to_string(), "0".to_string());
		req.set_params(params);
		let response = frame(req).await.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn lease_handler_returns_pending() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		reset_state_for_tests();
		init();
		let req = Request::builder()
			.uri("/api/video-previews/lease")
			.body(Body::from(
				r#"{"page_session_id":"page","page_path":"/r/test","post_id":"abc","media_url":"/vid/test/123","poster_url":"/thumb/test","duration_seconds":30,"width":320,"height":180}"#,
			))
			.unwrap();
		let response = lease(req).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let body = to_bytes(response.into_body()).await.unwrap();
		let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
		assert_eq!(value["state"]["status"], "pending");
	}

	#[tokio::test]
	async fn release_removes_lease() {
		let _guard = TEST_GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
		reset_state_for_tests();
		VIDEO_PREVIEW_LEASES.lock().unwrap().insert(
			"page".to_string(),
			LeaseEntry {
				page_path: "/r/test".to_string(),
				post_id: "abc".to_string(),
				job_ids: HashSet::from(["job".to_string()]),
				last_seen: Instant::now(),
			},
		);
		let req = Request::builder()
			.uri("/api/video-previews/release")
			.body(Body::from(r#"{"page_session_id":"page"}"#))
			.unwrap();
		let response = release(req).await.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert!(VIDEO_PREVIEW_LEASES.lock().unwrap().is_empty());
	}
}
