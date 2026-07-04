use clap::Parser;
use hyper::{body::to_bytes, Body, Client, Method, Request, Uri};
use redlib::{
	cache_persistence,
	subreddit_content_semantics::{self, SubredditContentCorpusOptions, SubredditContentSemanticBackfillCounters, SubredditContentSemanticBackfillStatus},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cmp::Ordering;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Backfill bounded subreddit vibe samples into the Redlib content semantic sidecar using local Ollama.")]
struct Args {
	#[arg(long, env = "REDLIB_CACHE_DIR")]
	cache_dir: Option<String>,

	#[arg(long, env = "REDLIB_SUBREDDIT_SEMANTIC_MODEL", default_value = redlib::subreddit_semantics::DEFAULT_SUBREDDIT_SEMANTIC_MODEL)]
	model: String,

	#[arg(long, env = "OLLAMA_BASE_URL", default_value = "http://127.0.0.1:11434")]
	ollama_url: String,

	#[arg(long)]
	limit: Option<usize>,

	#[arg(long = "subreddit")]
	subreddits: Vec<String>,

	#[arg(long)]
	max_posts_per_subreddit: Option<usize>,

	#[arg(long)]
	max_text_chars_per_post: Option<usize>,

	#[arg(long)]
	max_corpus_text_chars: Option<usize>,

	#[arg(long)]
	min_posts_per_subreddit: Option<usize>,

	#[arg(long)]
	min_corpus_text_chars: Option<usize>,

	#[arg(long)]
	force: bool,

	#[arg(long)]
	dry_run: bool,

	#[arg(long)]
	quiet: bool,

	#[arg(long)]
	report_json: Option<PathBuf>,

	#[arg(long, default_value_t = 12)]
	report_neighbor_samples: usize,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbeddingsResponse {
	embedding: Option<Vec<f32>>,
	embeddings: Option<Vec<Vec<f32>>>,
}

#[derive(Clone, Debug, Serialize)]
struct SimilarityStats {
	count: usize,
	average_top5_mean_similarity: Option<f32>,
	p50_top5_mean_similarity: Option<f32>,
	p90_top5_mean_similarity: Option<f32>,
	p95_top5_mean_similarity: Option<f32>,
	p99_top5_mean_similarity: Option<f32>,
	max_top5_mean_similarity: Option<f32>,
	top5_mean_similarity_at_least_90: usize,
	top5_mean_similarity_at_least_95: usize,
	top5_mean_similarity_at_least_97: usize,
	top5_mean_similarity_at_least_99: usize,
}

#[derive(Clone, Debug, Serialize)]
struct NeighborScore {
	subreddit: String,
	similarity: f32,
	post_count: usize,
	text_chars: usize,
}

#[derive(Clone, Debug, Serialize)]
struct NeighborSample {
	subreddit: String,
	post_count: usize,
	source_count: usize,
	text_chars: usize,
	top_neighbor_similarity: Option<f32>,
	top5_mean_similarity: Option<f32>,
	neighbors: Vec<NeighborScore>,
}

#[derive(Clone, Debug, Serialize)]
struct ContentBackfillReport {
	model: String,
	cache_dir: String,
	sidecar_path: String,
	options: SubredditContentCorpusOptions,
	counters: SubredditContentSemanticBackfillCounters,
	similarity: SimilarityStats,
	most_generic: Vec<NeighborSample>,
	samples: Vec<NeighborSample>,
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

#[tokio::main]
async fn main() -> Result<(), String> {
	let args = Args::parse();
	if let Some(cache_dir) = &args.cache_dir {
		std::env::set_var("REDLIB_CACHE_DIR", cache_dir);
	}
	std::env::set_var("REDLIB_SUBREDDIT_SEMANTIC_MODEL", &args.model);
	std::env::set_var("REDLIB_CACHE_PERSISTENCE_READ_ONLY", "on");

	let started_at_unix = cache_persistence::unix_now();
	let options = corpus_options_from_args(&args);
	let mut counters = SubredditContentSemanticBackfillCounters::default();
	record_backfill_status(status_for_run(&args, &options, &counters, started_at_unix, "running", None, None));

	match run_backfill(&args, &options, &mut counters).await {
		Ok(report) => {
			let state = if counters.failed > 0 { "completed_with_errors" } else { "completed" };
			record_backfill_status(status_for_run(
				&args,
				&options,
				&counters,
				started_at_unix,
				state,
				Some(cache_persistence::unix_now()),
				None,
			));
			print_backfill_summary(&counters, &report);
			if let Some(path) = &args.report_json {
				let bytes = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
				std::fs::write(path, bytes).map_err(|error| error.to_string())?;
				if !args.quiet {
					println!("wrote report {}", path.display());
				}
			}
			Ok(())
		}
		Err(error) => {
			record_backfill_status(status_for_run(
				&args,
				&options,
				&counters,
				started_at_unix,
				"failed",
				Some(cache_persistence::unix_now()),
				Some(error.clone()),
			));
			Err(error)
		}
	}
}

fn corpus_options_from_args(args: &Args) -> SubredditContentCorpusOptions {
	let mut options = subreddit_content_semantics::corpus_options_from_env();
	if let Some(value) = args.max_posts_per_subreddit {
		options.max_posts_per_subreddit = value.max(1);
	}
	if let Some(value) = args.max_text_chars_per_post {
		options.max_text_chars_per_post = value.max(1);
	}
	if let Some(value) = args.max_corpus_text_chars {
		options.max_corpus_text_chars = value.max(1);
	}
	if let Some(value) = args.min_posts_per_subreddit {
		options.min_posts_per_subreddit = value.max(1);
	}
	if let Some(value) = args.min_corpus_text_chars {
		options.min_corpus_text_chars = value.max(1);
	}
	options
}

fn status_for_run(
	args: &Args,
	options: &SubredditContentCorpusOptions,
	counters: &SubredditContentSemanticBackfillCounters,
	started_at_unix: u64,
	state: &str,
	finished_at_unix: Option<u64>,
	error: Option<String>,
) -> SubredditContentSemanticBackfillStatus {
	SubredditContentSemanticBackfillStatus {
		state: state.to_string(),
		model: args.model.clone(),
		started_at_unix,
		finished_at_unix,
		counters: counters.clone(),
		dry_run: args.dry_run,
		force: args.force,
		limit: args.limit,
		max_posts_per_subreddit: options.max_posts_per_subreddit,
		max_text_chars_per_post: options.max_text_chars_per_post,
		max_corpus_text_chars: options.max_corpus_text_chars,
		min_posts_per_subreddit: options.min_posts_per_subreddit,
		min_corpus_text_chars: options.min_corpus_text_chars,
		error,
	}
}

fn record_backfill_status(status: SubredditContentSemanticBackfillStatus) {
	if let Err(error) = subreddit_content_semantics::write_backfill_status(&status) {
		eprintln!("content backfill status write failed: {error}");
	}
}

fn print_backfill_summary(counters: &SubredditContentSemanticBackfillCounters, report: &ContentBackfillReport) {
	println!(
		"subreddit_content_semantic_backfill: source_files={} source_posts={} unique_posts={} total_subreddits={} candidates={} too_small={} embedded={} already_current={} failed={} active_vectors={} usable_vectors={} sidecar={}",
		counters.source_files,
		counters.source_posts,
		counters.unique_posts,
		counters.total_subreddits,
		counters.candidate_subreddits,
		counters.too_small_subreddits,
		counters.embedded,
		counters.already_current,
		counters.failed,
		report.similarity.count,
		subreddit_content_semantics::active_usable_embedding_count(),
		subreddit_content_semantics::persisted_embeddings_sidecar_path().display()
	);
	if let Some(p95) = report.similarity.p95_top5_mean_similarity {
		println!(
			"content_similarity: avg_top5={:?} p95_top5={:.4} >=0.95={} >=0.97={} >=0.99={}",
			report.similarity.average_top5_mean_similarity,
			p95,
			report.similarity.top5_mean_similarity_at_least_95,
			report.similarity.top5_mean_similarity_at_least_97,
			report.similarity.top5_mean_similarity_at_least_99
		);
	}
}

async fn run_backfill(args: &Args, options: &SubredditContentCorpusOptions, counters: &mut SubredditContentSemanticBackfillCounters) -> Result<ContentBackfillReport, String> {
	cache_persistence::init();
	let collection = subreddit_content_semantics::build_subreddit_content_post_embedding_candidates(options)?;
	counters.source_files = collection.stats.source_files;
	counters.source_posts = collection.stats.source_posts;
	counters.unique_posts = collection.stats.unique_posts;
	counters.total_subreddits = collection.stats.total_subreddits;
	counters.candidate_subreddits = collection.stats.candidate_subreddits;
	counters.too_small_subreddits = collection.stats.too_small_subreddits;

	let mut candidates = collection.candidates;
	if !args.subreddits.is_empty() {
		let selected = args
			.subreddits
			.iter()
			.map(|name| redlib::subreddit_semantics::normalize_subreddit_name(name))
			.filter(|name| !name.is_empty())
			.collect::<std::collections::HashSet<_>>();
		candidates.retain(|candidate| selected.contains(&candidate.subreddit));
		counters.candidate_subreddits = candidates
			.iter()
			.map(|candidate| candidate.subreddit.clone())
			.collect::<std::collections::HashSet<_>>()
			.len();
	}
	let candidate_counts_by_subreddit = candidates.iter().fold(std::collections::HashMap::<String, usize>::new(), |mut counts, candidate| {
		*counts.entry(candidate.subreddit.clone()).or_default() += 1;
		counts
	});
	candidates.retain(|candidate| {
		candidate_counts_by_subreddit
			.get(&candidate.subreddit)
			.is_some_and(|count| *count >= options.min_posts_per_subreddit)
	});

	let client = Client::new();
	for candidate in candidates.into_iter().take(args.limit.unwrap_or(usize::MAX)) {
		if !args.force && !subreddit_content_semantics::post_candidate_needs_embedding(&candidate, &args.model) {
			counters.already_current += 1;
			continue;
		}
		if args.dry_run {
			if !args.quiet {
				println!(
					"would embed sampled post r/{} chars={} post_hash={} text_hash={}",
					candidate.subreddit, candidate.text_chars, candidate.post_key_hash, candidate.text_hash
				);
			}
			continue;
		}

		let prompt = format!("r/{}\nCached post title and text:\n{}", candidate.subreddit, candidate.text);
		match ollama_embedding(&client, &args.ollama_url, &args.model, &prompt).await {
			Ok(embedding) => {
				let stored = subreddit_content_semantics::store_post_sample_embedding(&candidate, &args.model, embedding, cache_persistence::unix_now())?;
				if stored {
					counters.embedded += 1;
				} else {
					counters.already_current += 1;
				}
				if !args.quiet {
					println!("embedded sampled post r/{}", candidate.subreddit);
				}
			}
			Err(error) => {
				counters.failed += 1;
				eprintln!("embedding failed sampled post r/{}: {error}", candidate.subreddit);
			}
		}
	}

	if !args.dry_run {
		subreddit_content_semantics::refresh_similarity_quality_scores_for_model(&args.model)?;
		subreddit_content_semantics::write_persisted_embeddings_sidecar_now()?;
	}

	Ok(build_report(args, options, counters))
}

fn embedding_dot(left: &[f32], right: &[f32]) -> f32 {
	if left.len() != right.len() || left.is_empty() {
		return 0.0;
	}
	left.iter().zip(right).map(|(left, right)| left * right).sum()
}

fn percentile(values: &[f32], percentile: f64) -> Option<f32> {
	if values.is_empty() {
		return None;
	}
	let index = ((values.len().saturating_sub(1) as f64) * percentile).round() as usize;
	values.get(index.min(values.len() - 1)).copied()
}

fn similarity_stats(entries: &[subreddit_content_semantics::PersistedSubredditContentSemanticEmbedding]) -> SimilarityStats {
	let mut values = entries
		.iter()
		.filter_map(|entry| entry.top5_mean_similarity)
		.filter(|value| value.is_finite())
		.collect::<Vec<_>>();
	values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
	SimilarityStats {
		count: entries.len(),
		average_top5_mean_similarity: (!values.is_empty()).then(|| values.iter().sum::<f32>() / values.len() as f32),
		p50_top5_mean_similarity: percentile(&values, 0.50),
		p90_top5_mean_similarity: percentile(&values, 0.90),
		p95_top5_mean_similarity: percentile(&values, 0.95),
		p99_top5_mean_similarity: percentile(&values, 0.99),
		max_top5_mean_similarity: values.last().copied(),
		top5_mean_similarity_at_least_90: values.iter().filter(|value| **value >= 0.90).count(),
		top5_mean_similarity_at_least_95: values.iter().filter(|value| **value >= 0.95).count(),
		top5_mean_similarity_at_least_97: values.iter().filter(|value| **value >= 0.97).count(),
		top5_mean_similarity_at_least_99: values.iter().filter(|value| **value >= 0.99).count(),
	}
}

fn neighbor_sample(
	entry: &subreddit_content_semantics::PersistedSubredditContentSemanticEmbedding,
	entries: &[subreddit_content_semantics::PersistedSubredditContentSemanticEmbedding],
) -> NeighborSample {
	let mut neighbors = entries
		.iter()
		.filter(|candidate| candidate.subreddit != entry.subreddit)
		.filter(|candidate| candidate.embedding.len() == entry.embedding.len())
		.map(|candidate| NeighborScore {
			subreddit: candidate.subreddit.clone(),
			similarity: embedding_dot(&entry.embedding, &candidate.embedding),
			post_count: candidate.post_count,
			text_chars: candidate.text_chars,
		})
		.collect::<Vec<_>>();
	neighbors.sort_by(|left, right| right.similarity.partial_cmp(&left.similarity).unwrap_or(Ordering::Equal));
	neighbors.truncate(5);
	NeighborSample {
		subreddit: entry.subreddit.clone(),
		post_count: entry.post_count,
		source_count: entry.source_count,
		text_chars: entry.text_chars,
		top_neighbor_similarity: entry.top_neighbor_similarity,
		top5_mean_similarity: entry.top5_mean_similarity,
		neighbors,
	}
}

fn build_report(args: &Args, options: &SubredditContentCorpusOptions, counters: &SubredditContentSemanticBackfillCounters) -> ContentBackfillReport {
	let entries = subreddit_content_semantics::active_entries_for_model(&args.model);
	let similarity = similarity_stats(&entries);
	let mut generic_entries = entries.clone();
	generic_entries.sort_by(|left, right| {
		right
			.top5_mean_similarity
			.partial_cmp(&left.top5_mean_similarity)
			.unwrap_or(Ordering::Equal)
			.then_with(|| right.post_count.cmp(&left.post_count))
			.then_with(|| left.subreddit.cmp(&right.subreddit))
	});
	let most_generic = generic_entries
		.iter()
		.take(args.report_neighbor_samples)
		.map(|entry| neighbor_sample(entry, &entries))
		.collect::<Vec<_>>();

	let mut sampled_entries = entries.clone();
	sampled_entries.sort_by(|left, right| {
		right
			.post_count
			.cmp(&left.post_count)
			.then_with(|| right.text_chars.cmp(&left.text_chars))
			.then_with(|| left.subreddit.cmp(&right.subreddit))
	});
	let samples = sampled_entries
		.iter()
		.take(args.report_neighbor_samples)
		.map(|entry| neighbor_sample(entry, &entries))
		.collect::<Vec<_>>();

	ContentBackfillReport {
		model: args.model.clone(),
		cache_dir: cache_persistence::cache_dir(),
		sidecar_path: subreddit_content_semantics::persisted_embeddings_sidecar_path().display().to_string(),
		options: options.clone(),
		counters: counters.clone(),
		similarity,
		most_generic,
		samples,
	}
}
