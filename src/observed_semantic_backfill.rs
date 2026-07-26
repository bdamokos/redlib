use crate::server::RequestExt;
use crate::subreddit_content_semantics::{self, SubredditContentPostEmbeddingCandidate};
use crate::utils::Post;
use hyper::header::CONTENT_TYPE;
use hyper::{body::to_bytes, Body, Client, Method, Request, Response, Uri};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const DEFAULT_OPPORTUNISTIC_JOB_BUDGET_SECS: u64 = 5 * 60;
const DEFAULT_OPPORTUNISTIC_DELAY_MS: u64 = 150;
const DEFAULT_BACKGROUND_DELAY_MS: u64 = 150;
const DEFAULT_PRIORITY_DELAY_MS: u64 = 25;
const DEFAULT_SIDECAR_FLUSH_MAX_DELAY_SECS: u64 = 60;
const DEFAULT_WORK_SLICE_CANDIDATES: usize = 32;
const SIDECAR_FLUSH_RETRY_SECS: u64 = 5;
const MAX_QUEUE_SUBREDDITS: usize = 512;
const MAX_PENDING_CANDIDATES_PER_SUBREDDIT: usize = 512;
const VECTOR_PREVIEW_VALUES: usize = 30;

static OBSERVED_BACKFILL_QUEUE: LazyLock<Mutex<ObservedBackfillQueue>> = LazyLock::new(|| Mutex::new(ObservedBackfillQueue::default()));
static OBSERVED_BACKFILL_ACTIVITY: LazyLock<Mutex<HashMap<String, ObservedBackfillActivity>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static OBSERVED_BACKFILL_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);
static OBSERVED_BACKFILL_STARTED: AtomicBool = AtomicBool::new(false);
static OBSERVED_CONTENT_SIDECAR_DIRTY: AtomicBool = AtomicBool::new(false);
static OBSERVED_METADATA_SIDECAR_DIRTY: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObservedBackfillPriority {
	Opportunistic,
	Background,
	Priority,
}

#[derive(Clone, Debug)]
struct ObservedBackfillJob {
	subreddit: String,
	candidates: Vec<SubredditContentPostEmbeddingCandidate>,
	priority: ObservedBackfillPriority,
	updated_at: Instant,
}

#[derive(Default)]
struct ObservedBackfillQueue {
	order: VecDeque<String>,
	jobs: HashMap<String, ObservedBackfillJob>,
}

#[derive(Debug, Default)]
struct ObservedBackfillOutcome {
	metadata_embedded: usize,
	metadata_failed: usize,
	embedded: usize,
	already_current: usize,
	failed: usize,
	timed_out: bool,
}

#[derive(Clone, Debug, Default)]
struct ObservedBackfillActivity {
	active_candidates: usize,
	active_remaining: usize,
	embedded: usize,
	already_current: usize,
	failed: usize,
	timed_out: bool,
	activity_seq: u64,
	last_update_unix: u64,
	latest_embedding: Option<ObservedEmbeddingVectorPreview>,
	aggregate_embedding: Option<ObservedAggregateVectorPreview>,
	state: ObservedBackfillState,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
enum ObservedBackfillState {
	#[default]
	Idle,
	Queued,
	Active,
	Finished,
	Errored,
}

#[derive(Clone, Debug, Serialize)]
struct ObservedEmbeddingVectorPreview {
	embedded_at_unix: u64,
	dimensions: usize,
	displayed_dimensions: usize,
	post_key_hash: String,
	values: Vec<f32>,
}

#[derive(Clone, Debug, Serialize)]
struct ObservedAggregateVectorPreview {
	embedded_at_unix: u64,
	dimensions: usize,
	displayed_dimensions: usize,
	post_count: usize,
	values: Vec<f32>,
}

#[derive(Debug, Serialize)]
pub struct ObservedBackfillActivitySnapshot {
	subreddit: String,
	enabled: bool,
	state: ObservedBackfillState,
	queued_candidates: usize,
	queued_jobs: usize,
	active_candidates: usize,
	active_remaining: usize,
	embedded: usize,
	already_current: usize,
	failed: usize,
	timed_out: bool,
	activity_seq: u64,
	last_update_unix: u64,
	latest_embedding: Option<ObservedEmbeddingVectorPreview>,
	aggregate_embedding: Option<ObservedAggregateVectorPreview>,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbeddingsResponse {
	embedding: Option<Vec<f32>>,
	embeddings: Option<Vec<Vec<f32>>>,
}

fn env_flag(name: &str, default: bool) -> bool {
	match std::env::var(name) {
		Ok(value) => matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
		Err(_) => default,
	}
}

fn env_u64(name: &str, default: u64) -> u64 {
	std::env::var(name).ok().and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(default)
}

fn observed_backfill_enabled() -> bool {
	env_flag("REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL", false)
}

fn ollama_url() -> String {
	std::env::var("OLLAMA_BASE_URL")
		.ok()
		.filter(|value| !value.trim().is_empty())
		.unwrap_or_else(|| "http://127.0.0.1:11434".to_string())
}

fn normalize_ollama_url(value: &str) -> String {
	value.trim().trim_end_matches('/').to_string()
}

fn body_snippet(bytes: &[u8]) -> String {
	const MAX_ERROR_BODY_CHARS: usize = 180;
	let text = String::from_utf8_lossy(bytes);
	let mut snippet = text.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>();
	if text.chars().nth(MAX_ERROR_BODY_CHARS).is_some() {
		snippet.push_str("...");
	}
	snippet
}

fn merge_candidates(existing: &mut Vec<SubredditContentPostEmbeddingCandidate>, incoming: Vec<SubredditContentPostEmbeddingCandidate>) {
	let mut by_key = existing.drain(..).map(|candidate| (candidate.post_key_hash.clone(), candidate)).collect::<HashMap<_, _>>();
	for candidate in incoming {
		by_key
			.entry(candidate.post_key_hash.clone())
			.and_modify(|existing| {
				if candidate.created_ts > existing.created_ts {
					*existing = candidate.clone();
				}
			})
			.or_insert(candidate);
	}

	let mut merged = by_key.into_values().collect::<Vec<_>>();
	merged.sort_by(|left, right| left.created_ts.cmp(&right.created_ts).then_with(|| left.post_key_hash.cmp(&right.post_key_hash)));
	if merged.len() > MAX_PENDING_CANDIDATES_PER_SUBREDDIT {
		let drop_count = merged.len() - MAX_PENDING_CANDIDATES_PER_SUBREDDIT;
		merged.drain(0..drop_count);
	}
	*existing = merged;
}

fn group_candidates_by_subreddit(candidates: Vec<SubredditContentPostEmbeddingCandidate>) -> HashMap<String, Vec<SubredditContentPostEmbeddingCandidate>> {
	let mut grouped = HashMap::<String, Vec<SubredditContentPostEmbeddingCandidate>>::new();
	for candidate in candidates {
		grouped.entry(candidate.subreddit.clone()).or_default().push(candidate);
	}
	for candidates in grouped.values_mut() {
		candidates.sort_by(|left, right| left.created_ts.cmp(&right.created_ts).then_with(|| left.post_key_hash.cmp(&right.post_key_hash)));
	}
	grouped
}

fn now_unix() -> u64 {
	crate::cache_persistence::unix_now()
}

fn semantic_target_label(target: &str) -> String {
	target.strip_prefix("u_").map_or_else(|| format!("r/{target}"), |username| format!("u/{username}"))
}

fn update_activity<F>(subreddit: &str, update: F)
where
	F: FnOnce(&mut ObservedBackfillActivity),
{
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(subreddit);
	if subreddit.is_empty() {
		return;
	}
	let Ok(mut activity) = OBSERVED_BACKFILL_ACTIVITY.lock() else {
		return;
	};
	let entry = activity.entry(subreddit).or_default();
	update(entry);
	entry.activity_seq = entry.activity_seq.saturating_add(1);
	entry.last_update_unix = now_unix();
}

fn mark_queued_activity(subreddit: &str) {
	update_activity(subreddit, |activity| {
		if !matches!(activity.state, ObservedBackfillState::Active) {
			activity.state = ObservedBackfillState::Queued;
		}
		activity.timed_out = false;
	});
}

fn mark_active_activity(job: &ObservedBackfillJob) {
	update_activity(&job.subreddit, |activity| {
		activity.active_candidates = job.candidates.len();
		activity.active_remaining = job.candidates.len();
		activity.embedded = 0;
		activity.already_current = 0;
		activity.failed = 0;
		activity.timed_out = false;
		activity.state = ObservedBackfillState::Active;
	});
}

fn mark_skipped_current_activity(subreddit: &str) {
	update_activity(subreddit, |activity| {
		activity.already_current = activity.already_current.saturating_add(1);
		activity.active_remaining = activity.active_remaining.saturating_sub(1);
	});
}

fn mark_failed_activity(subreddit: &str) {
	update_activity(subreddit, |activity| {
		activity.failed = activity.failed.saturating_add(1);
		activity.active_remaining = activity.active_remaining.saturating_sub(1);
	});
}

fn vector_preview(candidate: &SubredditContentPostEmbeddingCandidate, embedding: &[f32], embedded_at_unix: u64) -> ObservedEmbeddingVectorPreview {
	ObservedEmbeddingVectorPreview {
		embedded_at_unix,
		dimensions: embedding.len(),
		displayed_dimensions: embedding.len().min(VECTOR_PREVIEW_VALUES),
		post_key_hash: candidate.post_key_hash.clone(),
		values: embedding.iter().take(VECTOR_PREVIEW_VALUES).copied().collect(),
	}
}

fn aggregate_preview(subreddit: &str, model: &str) -> Option<ObservedAggregateVectorPreview> {
	let preview = subreddit_content_semantics::aggregate_preview_for_subreddit_model(subreddit, model, VECTOR_PREVIEW_VALUES)?;
	Some(ObservedAggregateVectorPreview {
		embedded_at_unix: preview.embedded_at_unix,
		dimensions: preview.dimensions,
		displayed_dimensions: preview.displayed_dimensions,
		post_count: preview.post_count,
		values: preview.values,
	})
}

fn mark_embedded_activity(candidate: &SubredditContentPostEmbeddingCandidate, model: &str, latest_embedding: ObservedEmbeddingVectorPreview) {
	let aggregate_embedding = aggregate_preview(&candidate.subreddit, model);
	update_activity(&candidate.subreddit, |activity| {
		activity.embedded = activity.embedded.saturating_add(1);
		activity.active_remaining = activity.active_remaining.saturating_sub(1);
		activity.latest_embedding = Some(latest_embedding);
		activity.aggregate_embedding = aggregate_embedding;
	});
}

fn mark_finished_activity(subreddit: &str, outcome: &ObservedBackfillOutcome) {
	update_activity(subreddit, |activity| {
		activity.active_remaining = 0;
		activity.timed_out = outcome.timed_out;
		activity.state = if outcome.failed > 0 {
			ObservedBackfillState::Errored
		} else {
			ObservedBackfillState::Finished
		};
	});
}

fn queued_counts_for_subreddit(subreddit: &str) -> (usize, usize) {
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(subreddit);
	let Ok(queue) = OBSERVED_BACKFILL_QUEUE.lock() else {
		return (0, 0);
	};
	queue.jobs.get(&subreddit).map(|job| (job.candidates.len(), 1)).unwrap_or((0, 0))
}

pub fn activity_snapshot(subreddit: &str) -> ObservedBackfillActivitySnapshot {
	let subreddit = crate::subreddit_semantics::normalize_subreddit_name(subreddit);
	let (queued_candidates, queued_jobs) = queued_counts_for_subreddit(&subreddit);
	let mut activity = OBSERVED_BACKFILL_ACTIVITY
		.lock()
		.ok()
		.and_then(|activity| activity.get(&subreddit).cloned())
		.unwrap_or_default();
	if activity.aggregate_embedding.is_none() {
		activity.aggregate_embedding = aggregate_preview(&subreddit, &crate::subreddit_semantics::embedding_model());
	}
	let state = if matches!(activity.state, ObservedBackfillState::Idle) && queued_jobs > 0 {
		ObservedBackfillState::Queued
	} else {
		activity.state.clone()
	};
	ObservedBackfillActivitySnapshot {
		subreddit,
		enabled: observed_backfill_enabled(),
		state,
		queued_candidates,
		queued_jobs,
		active_candidates: activity.active_candidates,
		active_remaining: activity.active_remaining,
		embedded: activity.embedded,
		already_current: activity.already_current,
		failed: activity.failed,
		timed_out: activity.timed_out,
		activity_seq: activity.activity_seq,
		last_update_unix: activity.last_update_unix,
		latest_embedding: activity.latest_embedding,
		aggregate_embedding: activity.aggregate_embedding,
	}
}

pub async fn activity_api(req: Request<Body>) -> Result<Response<Body>, String> {
	let subreddit = req.param("sub").unwrap_or_default();
	let body = serde_json::to_string(&activity_snapshot(&subreddit)).map_err(|error| error.to_string())?;
	Ok(
		Response::builder()
			.status(200)
			.header(CONTENT_TYPE, "application/json; charset=utf-8")
			.header("Cache-Control", "no-store")
			.body(body.into())
			.unwrap_or_default(),
	)
}

fn metadata_embedding_needed(subreddit: &str) -> bool {
	if subreddit.is_empty() || subreddit.starts_with("u_") {
		return false;
	}
	let model = crate::subreddit_semantics::embedding_model();
	crate::subreddit::cached_subreddit_metadata_for_name(subreddit).is_some_and(|metadata| !crate::subreddit_semantics::has_current_embedding(&metadata, &model))
}

fn enqueue_candidates_with_metadata_targets(
	candidates: Vec<SubredditContentPostEmbeddingCandidate>,
	metadata_targets: impl IntoIterator<Item = String>,
	priority: ObservedBackfillPriority,
) -> usize {
	let mut queued = 0usize;
	let mut grouped = group_candidates_by_subreddit(candidates);
	for subreddit in metadata_targets {
		let subreddit = crate::subreddit_semantics::normalize_subreddit_name(&subreddit);
		if metadata_embedding_needed(&subreddit) {
			grouped.entry(subreddit).or_default();
		}
	}
	if grouped.is_empty() {
		return 0;
	}

	let Ok(mut queue) = OBSERVED_BACKFILL_QUEUE.lock() else {
		return 0;
	};
	for (subreddit, candidates) in grouped {
		if !queue.jobs.contains_key(&subreddit) {
			if queue.jobs.len() >= MAX_QUEUE_SUBREDDITS {
				if let Some(oldest) = queue.order.pop_front() {
					queue.jobs.remove(&oldest);
				}
			}
			queue.order.push_back(subreddit.clone());
		}
		let job = queue.jobs.entry(subreddit.clone()).or_insert_with(|| ObservedBackfillJob {
			subreddit: subreddit.clone(),
			candidates: Vec::new(),
			priority,
			updated_at: Instant::now(),
		});
		job.priority = job.priority.max(priority);
		job.updated_at = Instant::now();
		merge_candidates(&mut job.candidates, candidates);
		mark_queued_activity(&subreddit);
		queued += 1;
	}
	queued
}

fn enqueue_candidates(candidates: Vec<SubredditContentPostEmbeddingCandidate>, priority: ObservedBackfillPriority) -> usize {
	enqueue_candidates_with_metadata_targets(candidates, Vec::new(), priority)
}

pub fn enqueue_observed_posts(source_key: &str, posts: &[Post], priority: ObservedBackfillPriority) {
	if !observed_backfill_enabled() || posts.is_empty() {
		return;
	}

	let metadata_targets = posts.iter().map(|post| post.community.clone()).collect::<Vec<_>>();
	let options = subreddit_content_semantics::corpus_options_from_env();
	let candidates = subreddit_content_semantics::runtime_post_embedding_candidates(source_key, posts, options.max_text_chars_per_post)
		.into_iter()
		.filter(|candidate| subreddit_content_semantics::post_candidate_needs_embedding(candidate, &crate::subreddit_semantics::embedding_model()))
		.collect::<Vec<_>>();
	let candidate_count = candidates.len();
	let queued = enqueue_candidates_with_metadata_targets(candidates, metadata_targets, priority);
	if queued > 0 {
		info!(
			"observed_semantic_backfill: queued {} post candidates across {} subreddits from {} ({:?})",
			candidate_count, queued, source_key, priority
		);
		OBSERVED_BACKFILL_NOTIFY.notify_one();
	}
}

pub fn enqueue_observed_user_posts(username: &str, source_key: &str, posts: &[Post], priority: ObservedBackfillPriority) {
	if !observed_backfill_enabled() || posts.is_empty() {
		return;
	}

	let options = subreddit_content_semantics::corpus_options_from_env();
	let candidates = subreddit_content_semantics::runtime_user_post_embedding_candidates(username, source_key, posts, options.max_text_chars_per_post)
		.into_iter()
		.filter(|candidate| subreddit_content_semantics::post_candidate_needs_embedding(candidate, &crate::subreddit_semantics::embedding_model()))
		.collect::<Vec<_>>();
	let candidate_count = candidates.len();
	let queued = enqueue_candidates(candidates, priority);
	if queued > 0 {
		info!(
			"observed_semantic_backfill: queued {} post candidates for user {} from {} ({:?})",
			candidate_count, username, source_key, priority
		);
		OBSERVED_BACKFILL_NOTIFY.notify_one();
	}
}

fn pop_next_job() -> Option<ObservedBackfillJob> {
	let Ok(mut queue) = OBSERVED_BACKFILL_QUEUE.lock() else {
		return None;
	};

	let best = queue
		.order
		.iter()
		.enumerate()
		.filter_map(|(index, subreddit)| queue.jobs.get(subreddit).map(|job| (index, job.priority, job.updated_at)))
		.max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.2.cmp(&left.2)).then_with(|| right.0.cmp(&left.0)))?;
	let subreddit = queue.order.remove(best.0)?;
	queue.jobs.remove(&subreddit)
}

fn split_job_for_work_slice(mut job: ObservedBackfillJob, max_candidates: usize) -> (ObservedBackfillJob, Option<ObservedBackfillJob>) {
	let max_candidates = max_candidates.max(1);
	if job.candidates.len() <= max_candidates {
		return (job, None);
	}

	let remaining_candidates = job.candidates.split_off(max_candidates);
	let continuation = ObservedBackfillJob {
		subreddit: job.subreddit.clone(),
		candidates: remaining_candidates,
		priority: job.priority,
		updated_at: job.updated_at,
	};
	(job, Some(continuation))
}

async fn ollama_embedding(client: &Client<hyper::client::HttpConnector>, base_url: &str, model: &str, prompt: &str) -> Result<Vec<f32>, String> {
	let uri = format!("{}/api/embeddings", normalize_ollama_url(base_url))
		.parse::<Uri>()
		.map_err(|error| error.to_string())?;
	let body = json!({
		"model": model,
		"prompt": prompt,
		"keep_alive": crate::ollama::keep_alive(),
	})
	.to_string();
	let request = Request::builder()
		.method(Method::POST)
		.uri(uri)
		.header("content-type", "application/json")
		.body(Body::from(body))
		.map_err(|error| error.to_string())?;
	let response = client.request(request).await.map_err(|error| error.to_string())?;
	let status = response.status();
	let bytes = to_bytes(response.into_body()).await.map_err(|error| error.to_string())?;
	if !status.is_success() {
		return Err(format!("Ollama returned HTTP {status}: {}", body_snippet(&bytes)));
	}
	let parsed: OllamaEmbeddingsResponse = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
	parsed
		.embedding
		.or_else(|| parsed.embeddings.and_then(|mut embeddings| embeddings.drain(..).next()))
		.filter(|embedding| !embedding.is_empty())
		.ok_or_else(|| "Ollama response did not contain an embedding".to_string())
}

async fn process_job(client: &Client<hyper::client::HttpConnector>, job: ObservedBackfillJob) -> Result<ObservedBackfillOutcome, String> {
	let model = crate::subreddit_semantics::embedding_model();
	let base_url = ollama_url();
	let delay_ms = match job.priority {
		ObservedBackfillPriority::Priority => env_u64("REDLIB_CONTENT_SEMANTIC_PRIORITY_DELAY_MS", DEFAULT_PRIORITY_DELAY_MS),
		ObservedBackfillPriority::Background => env_u64("REDLIB_CONTENT_SEMANTIC_BACKGROUND_DELAY_MS", DEFAULT_BACKGROUND_DELAY_MS),
		ObservedBackfillPriority::Opportunistic => env_u64("REDLIB_CONTENT_SEMANTIC_OPPORTUNISTIC_DELAY_MS", DEFAULT_OPPORTUNISTIC_DELAY_MS),
	};
	let budget = (job.priority == ObservedBackfillPriority::Opportunistic)
		.then(|| Duration::from_secs(env_u64("REDLIB_CONTENT_SEMANTIC_OPPORTUNISTIC_JOB_BUDGET_SECS", DEFAULT_OPPORTUNISTIC_JOB_BUDGET_SECS)));
	let started = Instant::now();
	let mut outcome = ObservedBackfillOutcome::default();
	mark_active_activity(&job);
	if !job.subreddit.starts_with("u_") {
		if let Some(metadata) = crate::subreddit::cached_subreddit_metadata_for_name(&job.subreddit) {
			if !crate::subreddit_semantics::has_current_embedding(&metadata, &model) {
				let text = crate::subreddit_semantics::semantic_text_for_subreddit(&metadata);
				if !text.is_empty() {
					let text_hash = crate::subreddit_semantics::semantic_text_hash(&text);
					let prompt = format!("r/{}\nSubreddit title and description:\n{text}", job.subreddit);
					match ollama_embedding(client, &base_url, &model, &prompt).await {
						Ok(embedding) => {
							crate::subreddit_semantics::store_embedding(&job.subreddit, &model, &text_hash, embedding, now_unix())?;
							OBSERVED_METADATA_SIDECAR_DIRTY.store(true, AtomicOrdering::Release);
							outcome.metadata_embedded = 1;
						}
						Err(error) => {
							outcome.metadata_failed = 1;
							warn!("observed_semantic_backfill: metadata embedding failed for r/{}: {}", job.subreddit, error);
						}
					}
				}
			}
		}
	}

	for candidate in job.candidates {
		if budget.is_some_and(|budget| started.elapsed() >= budget) {
			outcome.timed_out = true;
			break;
		}
		if !subreddit_content_semantics::post_candidate_needs_embedding(&candidate, &model) {
			outcome.already_current += 1;
			mark_skipped_current_activity(&candidate.subreddit);
			continue;
		}

		let semantic_target = semantic_target_label(&candidate.subreddit);
		let prompt = format!("{semantic_target}\nCached post title and text:\n{}", candidate.text);
		match ollama_embedding(client, &base_url, &model, &prompt).await {
			Ok(embedding) => {
				let embedded_at_unix = now_unix();
				let latest_embedding = vector_preview(&candidate, &embedding, embedded_at_unix);
				match subreddit_content_semantics::store_post_aggregate_embedding(&candidate, &model, embedding, embedded_at_unix) {
					Ok(true) => {
						outcome.embedded += 1;
						OBSERVED_CONTENT_SIDECAR_DIRTY.store(true, AtomicOrdering::Release);
						mark_embedded_activity(&candidate, &model, latest_embedding);
					}
					Ok(false) => {
						outcome.already_current += 1;
						mark_skipped_current_activity(&candidate.subreddit);
					}
					Err(error) => {
						mark_failed_activity(&candidate.subreddit);
						return Err(format!("failed to store {semantic_target} embedding: {error}"));
					}
				}
			}
			Err(error) => {
				mark_failed_activity(&candidate.subreddit);
				return Err(format!("embedding failed for {semantic_target}: {error}"));
			}
		}

		if delay_ms > 0 {
			tokio::time::sleep(Duration::from_millis(delay_ms)).await;
		}
	}

	Ok(outcome)
}

fn should_flush_sidecar(dirty: bool, queue_empty: bool, elapsed: Duration, max_delay: Duration) -> bool {
	dirty && (queue_empty || elapsed >= max_delay)
}

#[derive(Default)]
struct SidecarFlushOutcome {
	metadata_flushed: bool,
	content_flushed: bool,
	allocator_trimmed: bool,
}

async fn flush_sidecars_and_trim_allocator(flush_metadata: bool, flush_content: bool) -> Result<SidecarFlushOutcome, String> {
	tokio::task::spawn_blocking(move || -> Result<SidecarFlushOutcome, String> {
		let mut outcome = SidecarFlushOutcome::default();
		if flush_metadata {
			crate::subreddit_semantics::write_persisted_embeddings_sidecar_now()?;
			outcome.metadata_flushed = true;
		}
		if flush_content {
			subreddit_content_semantics::write_persisted_embeddings_sidecar_now()?;
			outcome.content_flushed = true;
		}
		outcome.allocator_trimmed = crate::memory::trim_allocator();
		Ok(outcome)
	})
	.await
	.map_err(|error| format!("embedding sidecar writer stopped: {error}"))?
}

async fn acquire_mutation_guard() -> Result<subreddit_content_semantics::ContentSemanticMutationGuard, String> {
	tokio::task::spawn_blocking(subreddit_content_semantics::acquire_content_semantic_mutation_guard)
		.await
		.map_err(|error| format!("embedding mutation lock task stopped: {error}"))?
}

async fn acquire_mutation_guard_and_refresh_sidecar() -> Result<subreddit_content_semantics::ContentSemanticMutationGuard, String> {
	let guard = acquire_mutation_guard().await?;
	tokio::task::spawn_blocking(subreddit_content_semantics::refresh_persisted_embeddings_sidecar)
		.await
		.map_err(|error| format!("embedding sidecar refresh task stopped: {error}"))??;
	Ok(guard)
}

async fn worker_loop() {
	info!("observed_semantic_backfill: worker started");
	let client = Client::new();
	let flush_max_delay = Duration::from_secs(env_u64("REDLIB_CONTENT_SEMANTIC_SIDECAR_FLUSH_MAX_DELAY_SECS", DEFAULT_SIDECAR_FLUSH_MAX_DELAY_SECS));
	let work_slice_candidates =
		env_u64("REDLIB_CONTENT_SEMANTIC_WORK_SLICE_CANDIDATES", DEFAULT_WORK_SLICE_CANDIDATES as u64).clamp(1, MAX_PENDING_CANDIDATES_PER_SUBREDDIT as u64) as usize;
	let mut last_flush_attempt = Instant::now();
	let mut mutation_guard = None;
	loop {
		let Some(job) = pop_next_job() else {
			let metadata_dirty = OBSERVED_METADATA_SIDECAR_DIRTY.load(AtomicOrdering::Acquire);
			let content_dirty = OBSERVED_CONTENT_SIDECAR_DIRTY.load(AtomicOrdering::Acquire);
			let dirty = metadata_dirty || content_dirty;
			if should_flush_sidecar(dirty, true, last_flush_attempt.elapsed(), flush_max_delay) {
				if mutation_guard.is_none() {
					// Dirty memory must never be replaced by a disk refresh. This
					// branch is defensive—the normal worker retains its guard until a
					// successful flush—but still preserves local progress if that
					// invariant is ever broken.
					match acquire_mutation_guard().await {
						Ok(guard) => mutation_guard = Some(guard),
						Err(error) => {
							warn!("observed_semantic_backfill: failed to acquire mutation guard for flush: {error}");
							tokio::time::sleep(Duration::from_secs(SIDECAR_FLUSH_RETRY_SECS)).await;
							continue;
						}
					}
				}
				last_flush_attempt = Instant::now();
				match flush_sidecars_and_trim_allocator(metadata_dirty, content_dirty).await {
					Ok(flushed) => {
						if flushed.metadata_flushed {
							OBSERVED_METADATA_SIDECAR_DIRTY.store(false, AtomicOrdering::Release);
						}
						if flushed.content_flushed {
							OBSERVED_CONTENT_SIDECAR_DIRTY.store(false, AtomicOrdering::Release);
						}
						info!(
							"observed_semantic_backfill: persisted drained embedding burst (metadata={}, content={}, allocator_trimmed={})",
							flushed.metadata_flushed, flushed.content_flushed, flushed.allocator_trimmed
						);
						mutation_guard.take();
					}
					Err(error) => {
						warn!("observed_semantic_backfill: failed to persist drained embedding burst: {error}");
						tokio::select! {
							_ = tokio::time::sleep(Duration::from_secs(SIDECAR_FLUSH_RETRY_SECS)) => {},
							_ = OBSERVED_BACKFILL_NOTIFY.notified() => {},
						}
					}
				}
				continue;
			}
			mutation_guard.take();
			OBSERVED_BACKFILL_NOTIFY.notified().await;
			continue;
		};
		if mutation_guard.is_none() {
			match acquire_mutation_guard_and_refresh_sidecar().await {
				Ok(guard) => mutation_guard = Some(guard),
				Err(error) => {
					let priority = job.priority;
					enqueue_candidates(job.candidates, priority);
					warn!("observed_semantic_backfill: failed to acquire mutation guard for work: {error}");
					tokio::time::sleep(Duration::from_secs(SIDECAR_FLUSH_RETRY_SECS)).await;
					continue;
				}
			}
		}
		// Opportunistic jobs retain their existing whole-job time budget. Daemon
		// and visible-session jobs are sliced so queued foreground work can run
		// between slices without resetting that budget.
		let slice_limit = if job.priority == ObservedBackfillPriority::Opportunistic {
			usize::MAX
		} else {
			work_slice_candidates
		};
		let (job, continuation) = split_job_for_work_slice(job, slice_limit);
		let subreddit = job.subreddit.clone();
		let priority = job.priority;
		let candidate_count = job.candidates.len();
		info!(
			"observed_semantic_backfill: processing r/{} with {} candidates ({:?})",
			subreddit, candidate_count, priority
		);
		let should_resume_continuation = match process_job(&client, job).await {
			Ok(outcome) => {
				mark_finished_activity(&subreddit, &outcome);
				info!(
					"observed_semantic_backfill: finished r/{} ({:?}): metadata_embedded={}, metadata_failed={}, content_embedded={}, already_current={}, failed={}, timed_out={}",
					subreddit, priority, outcome.metadata_embedded, outcome.metadata_failed, outcome.embedded, outcome.already_current, outcome.failed, outcome.timed_out
				);
				!outcome.timed_out
			}
			Err(error) => {
				update_activity(&subreddit, |activity| {
					activity.active_remaining = 0;
					activity.state = ObservedBackfillState::Errored;
				});
				warn!("observed_semantic_backfill: failed r/{} ({:?}): {}", subreddit, priority, error);
				false
			}
		};
		// On an error or time-budget stop, leave the cursor before the
		// unprocessed tail. The normal next source observation will retry that
		// tail in order; running a later slice now could advance past it forever.
		if should_resume_continuation {
			if let Some(continuation) = continuation {
				let remaining = continuation.candidates.len();
				let priority = continuation.priority;
				if enqueue_candidates(continuation.candidates, priority) > 0 {
					info!(
						"observed_semantic_backfill: yielded r/{} with {} candidates remaining ({:?})",
						subreddit, remaining, priority
					);
				}
			}
		}

		let metadata_dirty = OBSERVED_METADATA_SIDECAR_DIRTY.load(AtomicOrdering::Acquire);
		let content_dirty = OBSERVED_CONTENT_SIDECAR_DIRTY.load(AtomicOrdering::Acquire);
		let dirty = metadata_dirty || content_dirty;
		if should_flush_sidecar(dirty, false, last_flush_attempt.elapsed(), flush_max_delay) {
			last_flush_attempt = Instant::now();
			match flush_sidecars_and_trim_allocator(metadata_dirty, content_dirty).await {
				Ok(flushed) => {
					if flushed.metadata_flushed {
						OBSERVED_METADATA_SIDECAR_DIRTY.store(false, AtomicOrdering::Release);
					}
					if flushed.content_flushed {
						OBSERVED_CONTENT_SIDECAR_DIRTY.store(false, AtomicOrdering::Release);
					}
					info!(
						"observed_semantic_backfill: persisted active embedding burst (metadata={}, content={}, allocator_trimmed={})",
						flushed.metadata_flushed, flushed.content_flushed, flushed.allocator_trimmed
					);
					mutation_guard.take();
				}
				Err(error) => warn!("observed_semantic_backfill: failed to persist active embedding burst: {error}"),
			}
		}
	}
}

pub fn spawn_observed_semantic_backfill_worker() {
	if !observed_backfill_enabled() {
		info!("observed_semantic_backfill: disabled");
		return;
	}
	if OBSERVED_BACKFILL_STARTED.swap(true, AtomicOrdering::SeqCst) {
		return;
	}
	tokio::spawn(worker_loop());
}

#[cfg(test)]
mod tests {
	use super::*;

	fn candidate(subreddit: &str, key: &str, created_ts: u64) -> SubredditContentPostEmbeddingCandidate {
		SubredditContentPostEmbeddingCandidate {
			subreddit: subreddit.to_string(),
			text: format!("{subreddit} {key}"),
			post_key_hash: key.to_string(),
			text_hash: format!("text-{key}"),
			sort_key: created_ts,
			source_key_hash: "source".to_string(),
			created_ts,
			text_chars: 12,
		}
	}

	#[test]
	fn merge_candidates_keeps_newest_duplicate_and_caps_oldest() {
		let mut existing = vec![candidate("rust", "a", 1), candidate("rust", "b", 2)];
		merge_candidates(&mut existing, vec![candidate("rust", "a", 3)]);
		assert_eq!(existing.len(), 2);
		assert_eq!(existing[0].post_key_hash, "b");
		assert_eq!(existing[1].post_key_hash, "a");
		assert_eq!(existing[1].created_ts, 3);
	}

	#[test]
	fn group_candidates_sorts_per_subreddit_oldest_first() {
		let grouped = group_candidates_by_subreddit(vec![candidate("rust", "b", 2), candidate("go", "a", 1), candidate("rust", "a", 1)]);
		assert_eq!(grouped["rust"].iter().map(|candidate| candidate.post_key_hash.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
		assert_eq!(grouped["go"].len(), 1);
	}

	#[test]
	fn metadata_only_work_uses_the_same_observed_worker_queue() {
		let subreddit = "observed_metadata_only_queue_test";
		crate::subreddit::cache_subreddit_metadata(&crate::utils::Subreddit {
			name: subreddit.to_string(),
			title: "Observed metadata queue".to_string(),
			description: "Metadata should not depend on a missing content candidate.".to_string(),
			..crate::utils::Subreddit::default()
		});

		assert_eq!(
			enqueue_candidates_with_metadata_targets(Vec::new(), vec![subreddit.to_string()], ObservedBackfillPriority::Background),
			1
		);
		let mut queue = OBSERVED_BACKFILL_QUEUE.lock().unwrap();
		let job = queue.jobs.remove(subreddit).expect("metadata-only job");
		queue.order.retain(|name| name != subreddit);
		assert!(job.candidates.is_empty());
		assert_eq!(job.priority, ObservedBackfillPriority::Background);
	}

	#[test]
	fn semantic_target_label_distinguishes_users_from_subreddits() {
		assert_eq!(semantic_target_label("rust"), "r/rust");
		assert_eq!(semantic_target_label("u_exampleuser"), "u/exampleuser");
	}

	#[test]
	fn sidecar_flushes_when_queue_drains_or_max_delay_elapses() {
		let max_delay = Duration::from_secs(60);
		assert!(!should_flush_sidecar(false, true, Duration::from_secs(60), max_delay));
		assert!(should_flush_sidecar(true, true, Duration::ZERO, max_delay));
		assert!(!should_flush_sidecar(true, false, Duration::from_secs(59), max_delay));
		assert!(should_flush_sidecar(true, false, Duration::from_secs(60), max_delay));
	}

	#[test]
	fn idle_activity_uses_persisted_average_vibe_and_total_post_count() {
		let subreddit = "observed_activity_idle_average_test";
		let model = crate::subreddit_semantics::embedding_model();
		let first = candidate(subreddit, "first", 1);
		let second = candidate(subreddit, "second", 2);
		subreddit_content_semantics::store_post_aggregate_embedding(&first, &model, vec![1.0, 0.0], 100).unwrap();
		subreddit_content_semantics::store_post_aggregate_embedding(&second, &model, vec![0.0, 1.0], 200).unwrap();

		let snapshot = activity_snapshot(subreddit);
		let average = snapshot.aggregate_embedding.expect("idle average should be available");

		assert!(matches!(snapshot.state, ObservedBackfillState::Idle));
		assert_eq!(average.post_count, 2);
		assert_eq!(snapshot.embedded, 0, "run-local count must not masquerade as the persisted total");
	}

	#[test]
	fn work_slices_preserve_every_candidate_for_resumption() {
		let job = ObservedBackfillJob {
			subreddit: "rust".to_string(),
			candidates: (0..40).map(|index| candidate("rust", &format!("post-{index}"), index)).collect(),
			priority: ObservedBackfillPriority::Background,
			updated_at: Instant::now(),
		};
		let (active, continuation) = split_job_for_work_slice(job, 32);
		let continuation = continuation.expect("remaining candidates");

		assert_eq!(active.candidates.len(), 32);
		assert_eq!(continuation.candidates.len(), 8);
		assert_eq!(active.candidates[31].post_key_hash, "post-31");
		assert_eq!(continuation.candidates[0].post_key_hash, "post-32");
		assert_eq!(continuation.priority, ObservedBackfillPriority::Background);
	}
}
