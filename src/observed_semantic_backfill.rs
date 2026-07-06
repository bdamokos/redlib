use crate::subreddit_content_semantics::{self, SubredditContentPostEmbeddingCandidate};
use crate::utils::Post;
use hyper::{body::to_bytes, Body, Client, Method, Request, Uri};
use log::{info, warn};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const DEFAULT_OPPORTUNISTIC_JOB_BUDGET_SECS: u64 = 5 * 60;
const DEFAULT_OPPORTUNISTIC_DELAY_MS: u64 = 150;
const DEFAULT_PRIORITY_DELAY_MS: u64 = 25;
const MAX_QUEUE_SUBREDDITS: usize = 512;
const MAX_PENDING_CANDIDATES_PER_SUBREDDIT: usize = 512;

static OBSERVED_BACKFILL_QUEUE: LazyLock<Mutex<ObservedBackfillQueue>> = LazyLock::new(|| Mutex::new(ObservedBackfillQueue::default()));
static OBSERVED_BACKFILL_NOTIFY: LazyLock<Notify> = LazyLock::new(Notify::new);
static OBSERVED_BACKFILL_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObservedBackfillPriority {
	Opportunistic,
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
	embedded: usize,
	already_current: usize,
	failed: usize,
	timed_out: bool,
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

fn enqueue_candidates(candidates: Vec<SubredditContentPostEmbeddingCandidate>, priority: ObservedBackfillPriority) -> usize {
	let mut queued = 0usize;
	let grouped = group_candidates_by_subreddit(candidates);
	if grouped.is_empty() {
		return 0;
	}

	let Ok(mut queue) = OBSERVED_BACKFILL_QUEUE.lock() else {
		return 0;
	};
	for (subreddit, candidates) in grouped {
		if candidates.is_empty() {
			continue;
		}
		if !queue.jobs.contains_key(&subreddit) {
			if queue.jobs.len() >= MAX_QUEUE_SUBREDDITS {
				if let Some(oldest) = queue.order.pop_front() {
					queue.jobs.remove(&oldest);
				}
			}
			queue.order.push_back(subreddit.clone());
		}
		let job = queue.jobs.entry(subreddit.clone()).or_insert_with(|| ObservedBackfillJob {
			subreddit,
			candidates: Vec::new(),
			priority,
			updated_at: Instant::now(),
		});
		job.priority = job.priority.max(priority);
		job.updated_at = Instant::now();
		merge_candidates(&mut job.candidates, candidates);
		queued += 1;
	}
	queued
}

pub fn enqueue_observed_posts(source_key: &str, posts: &[Post], priority: ObservedBackfillPriority) {
	if !observed_backfill_enabled() || posts.is_empty() {
		return;
	}

	let options = subreddit_content_semantics::corpus_options_from_env();
	let candidates = subreddit_content_semantics::runtime_post_embedding_candidates(source_key, posts, options.max_text_chars_per_post)
		.into_iter()
		.filter(|candidate| subreddit_content_semantics::post_candidate_needs_embedding(candidate, &crate::subreddit_semantics::embedding_model()))
		.collect::<Vec<_>>();
	let candidate_count = candidates.len();
	let queued = enqueue_candidates(candidates, priority);
	if queued > 0 {
		info!(
			"observed_semantic_backfill: queued {} post candidates across {} subreddits from {} ({:?})",
			candidate_count, queued, source_key, priority
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

async fn ollama_embedding(client: &Client<hyper::client::HttpConnector>, base_url: &str, model: &str, prompt: &str) -> Result<Vec<f32>, String> {
	let uri = format!("{}/api/embeddings", normalize_ollama_url(base_url))
		.parse::<Uri>()
		.map_err(|error| error.to_string())?;
	let body = json!({
		"model": model,
		"prompt": prompt,
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

async fn process_job(job: ObservedBackfillJob) -> Result<ObservedBackfillOutcome, String> {
	let model = crate::subreddit_semantics::embedding_model();
	let base_url = ollama_url();
	let delay_ms = match job.priority {
		ObservedBackfillPriority::Priority => env_u64("REDLIB_CONTENT_SEMANTIC_PRIORITY_DELAY_MS", DEFAULT_PRIORITY_DELAY_MS),
		ObservedBackfillPriority::Opportunistic => env_u64("REDLIB_CONTENT_SEMANTIC_OPPORTUNISTIC_DELAY_MS", DEFAULT_OPPORTUNISTIC_DELAY_MS),
	};
	let budget = (job.priority == ObservedBackfillPriority::Opportunistic)
		.then(|| Duration::from_secs(env_u64("REDLIB_CONTENT_SEMANTIC_OPPORTUNISTIC_JOB_BUDGET_SECS", DEFAULT_OPPORTUNISTIC_JOB_BUDGET_SECS)));
	let started = Instant::now();
	let client = Client::new();
	let mut outcome = ObservedBackfillOutcome::default();

	for candidate in job.candidates {
		if budget.is_some_and(|budget| started.elapsed() >= budget) {
			outcome.timed_out = true;
			break;
		}
		if !subreddit_content_semantics::post_candidate_needs_embedding(&candidate, &model) {
			outcome.already_current += 1;
			continue;
		}

		let prompt = format!("r/{}\nCached post title and text:\n{}", candidate.subreddit, candidate.text);
		match ollama_embedding(&client, &base_url, &model, &prompt).await {
			Ok(embedding) => match subreddit_content_semantics::store_post_aggregate_embedding(&candidate, &model, embedding, crate::cache_persistence::unix_now()) {
				Ok(true) => outcome.embedded += 1,
				Ok(false) => outcome.already_current += 1,
				Err(error) => {
					outcome.failed += 1;
					warn!("observed_semantic_backfill: failed to store r/{} embedding: {}", candidate.subreddit, error);
				}
			},
			Err(error) => {
				return Err(format!("embedding failed for r/{}: {error}", candidate.subreddit));
			}
		}

		if delay_ms > 0 {
			tokio::time::sleep(Duration::from_millis(delay_ms)).await;
		}
	}

	if outcome.embedded > 0 {
		subreddit_content_semantics::write_persisted_embeddings_sidecar_now()?;
	}

	Ok(outcome)
}

async fn worker_loop() {
	info!("observed_semantic_backfill: worker started");
	loop {
		let Some(job) = pop_next_job() else {
			OBSERVED_BACKFILL_NOTIFY.notified().await;
			continue;
		};
		let subreddit = job.subreddit.clone();
		let priority = job.priority;
		let candidate_count = job.candidates.len();
		info!(
			"observed_semantic_backfill: processing r/{} with {} candidates ({:?})",
			subreddit, candidate_count, priority
		);
		match process_job(job).await {
			Ok(outcome) => {
				info!(
					"observed_semantic_backfill: finished r/{} ({:?}): embedded={}, already_current={}, failed={}, timed_out={}",
					subreddit, priority, outcome.embedded, outcome.already_current, outcome.failed, outcome.timed_out
				);
			}
			Err(error) => {
				warn!("observed_semantic_backfill: failed r/{} ({:?}): {}", subreddit, priority, error);
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
}
