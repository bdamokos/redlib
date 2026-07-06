use clap::Parser;
use hyper::{body::to_bytes, header, Body, Client, Method, Request, Uri};
use redlib::{
	cache_persistence, materialized_catalog, subreddit,
	subreddit_semantics::{self, SubredditSemanticBackfillCounters, SubredditSemanticBackfillStatus},
	utils,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};

const DEFAULT_NSFWDOG_API_BASE: &str = "https://api2.nsfwdog.com";
const DEFAULT_NSFWDOG_SIMILAR_LIMIT: usize = 50;
const DEFAULT_NSFWDOG_SEED_LIMIT: usize = 8;
const DEFAULT_NSFWDOG_PER_SEED_LIMIT: usize = 16;

#[derive(Debug, Parser)]
#[command(about = "Backfill subreddit semantic embeddings into the Redlib semantic sidecar using local Ollama.")]
struct Args {
	#[arg(long, env = "REDLIB_CACHE_DIR")]
	cache_dir: Option<String>,

	#[arg(long, env = "REDLIB_SUBREDDIT_SEMANTIC_MODEL", default_value = subreddit_semantics::DEFAULT_SUBREDDIT_SEMANTIC_MODEL)]
	model: String,

	#[arg(long, env = "OLLAMA_BASE_URL", default_value = "http://127.0.0.1:11434")]
	ollama_url: String,

	#[arg(long)]
	limit: Option<usize>,

	#[arg(long = "subreddit")]
	subreddits: Vec<String>,

	#[arg(long)]
	refresh_missing_metadata: bool,

	#[arg(long)]
	metadata_refresh_limit: Option<usize>,

	#[arg(long)]
	name_only_if_missing_metadata: bool,

	#[arg(long, env = "REDLIB_NSFWDOG_SIMILAR_LIMIT", default_value_t = DEFAULT_NSFWDOG_SIMILAR_LIMIT)]
	nsfwdog_similar_limit: usize,

	#[arg(long, env = "REDLIB_NSFWDOG_SEED_LIMIT", default_value_t = DEFAULT_NSFWDOG_SEED_LIMIT)]
	nsfwdog_seed_limit: usize,

	#[arg(long, env = "REDLIB_NSFWDOG_PER_SEED_LIMIT", default_value_t = DEFAULT_NSFWDOG_PER_SEED_LIMIT)]
	nsfwdog_per_seed_limit: usize,

	#[arg(long, env = "REDLIB_NSFWDOG_API_BASE", default_value = DEFAULT_NSFWDOG_API_BASE)]
	nsfwdog_api_base: String,

	#[arg(long)]
	force: bool,

	#[arg(long)]
	dry_run: bool,

	#[arg(long)]
	quiet: bool,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbeddingsResponse {
	embedding: Option<Vec<f32>>,
	embeddings: Option<Vec<Vec<f32>>>,
}

#[derive(Debug, Deserialize)]
struct NsfwDogSimilarResponse {
	#[serde(default)]
	results: Vec<NsfwDogSimilarResult>,
}

#[derive(Debug, Deserialize)]
struct NsfwDogSimilarResult {
	slug: String,
}

#[derive(Debug, Deserialize)]
struct NsfwDogNameResponse {
	subreddit_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NsfwDogSeed {
	query_name: String,
	normalized_name: String,
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

fn normalize_nsfwdog_base_url(value: &str) -> String {
	value.trim().trim_end_matches('/').to_string()
}

fn nsfwdog_api_uri(base_url: &str, path: &str, params: &[(&str, &str)]) -> Result<Uri, String> {
	let mut url = url::Url::parse(&normalize_nsfwdog_base_url(base_url)).map_err(|error| error.to_string())?;
	url.set_path(path);
	{
		let mut query = url.query_pairs_mut();
		for (key, value) in params {
			query.append_pair(key, value);
		}
	}
	url.as_str().parse::<Uri>().map_err(|error| error.to_string())
}

fn safe_nsfwdog_slug(slug: &str) -> Option<String> {
	let slug = slug.trim();
	(!slug.is_empty() && slug.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')).then(|| slug.to_string())
}

fn nsfwdog_query_subreddit_name(name: &str) -> Option<String> {
	let trimmed = name.trim();
	let without_path_prefix = trimmed.strip_prefix("/r/").unwrap_or(trimmed);
	let without_prefix = without_path_prefix.strip_prefix("r/").unwrap_or(without_path_prefix);
	let name = without_prefix.trim();
	(!name.is_empty() && name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')).then(|| name.to_string())
}

fn normalize_nsfwdog_subreddit_name(name: &str) -> Option<String> {
	let name = subreddit_semantics::normalize_subreddit_name(name);
	(!name.is_empty() && !name.starts_with("u_") && name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')).then_some(name)
}

async fn nsfwdog_get_json<T>(uri: Uri) -> Result<T, String>
where
	T: for<'de> Deserialize<'de>,
{
	let request = Request::builder()
		.method(Method::GET)
		.uri(uri)
		.header(header::ACCEPT, "application/json")
		.header(header::USER_AGENT, "redlib-semantic-backfill/0.1")
		.body(Body::empty())
		.map_err(|error| error.to_string())?;
	let response = redlib::client::CLIENT.request(request).await.map_err(|error| error.to_string())?;
	let status = response.status();
	let bytes = to_bytes(response.into_body()).await.map_err(|error| error.to_string())?;
	if !status.is_success() {
		return Err(format!("NSFWDOG returned HTTP {status}: {}", body_snippet(&bytes)));
	}
	serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

async fn nsfwdog_similar_results(args: &Args, seed: &str) -> Result<Vec<NsfwDogSimilarResult>, String> {
	let subreddit = format!("r/{seed}");
	let offset = "0".to_string();
	let limit = args.nsfwdog_per_seed_limit.min(args.nsfwdog_similar_limit).to_string();
	let uri = nsfwdog_api_uri(
		&args.nsfwdog_api_base,
		"/api/v1/similar",
		&[("subreddit", &subreddit), ("offset", &offset), ("limit", &limit)],
	)?;
	let response: NsfwDogSimilarResponse = nsfwdog_get_json(uri).await?;
	Ok(response.results)
}

async fn nsfwdog_resolve_subreddit_name(args: &Args, slug: &str) -> Result<String, String> {
	let slug = safe_nsfwdog_slug(slug).ok_or_else(|| format!("unsafe NSFWDOG slug {slug:?}"))?;
	let path = format!("/v1/subreddits/{slug}/name/");
	let uri = nsfwdog_api_uri(&args.nsfwdog_api_base, &path, &[])?;
	let response: NsfwDogNameResponse = nsfwdog_get_json(uri).await?;
	normalize_nsfwdog_subreddit_name(&response.subreddit_name).ok_or_else(|| format!("invalid resolved subreddit name {:?}", response.subreddit_name))
}

fn nsfwdog_seed_names(args: &Args, known_names: &BTreeSet<String>, cached_metadata: &[utils::Subreddit]) -> Vec<NsfwDogSeed> {
	if args.nsfwdog_similar_limit == 0 || args.nsfwdog_seed_limit == 0 || args.nsfwdog_per_seed_limit == 0 {
		return Vec::new();
	}

	let metadata_display_names = cached_metadata
		.iter()
		.filter_map(|metadata| {
			let normalized = normalize_nsfwdog_subreddit_name(&metadata.name)?;
			let query_name = nsfwdog_query_subreddit_name(&metadata.name)?;
			Some((normalized, query_name))
		})
		.collect::<HashMap<_, _>>();
	let mut seen = HashSet::new();
	let mut seeds = Vec::new();
	let mut push_seed = |value: &str| {
		let Some(query_name) = nsfwdog_query_subreddit_name(value) else {
			return;
		};
		let Some(normalized_name) = normalize_nsfwdog_subreddit_name(&query_name) else {
			return;
		};
		if !known_names.contains(&normalized_name) || !seen.insert(normalized_name.clone()) {
			return;
		}
		seeds.push(NsfwDogSeed { query_name, normalized_name });
	};

	for subreddit in &args.subreddits {
		push_seed(subreddit);
	}
	for subreddit in materialized_catalog::cached_catalog_subreddit_names_by_activity() {
		if let Some(display_name) = metadata_display_names.get(&subreddit) {
			push_seed(display_name);
		} else {
			push_seed(&subreddit);
		}
	}
	for metadata in cached_metadata {
		push_seed(&metadata.name);
	}
	for subreddit in known_names {
		push_seed(subreddit);
	}
	seeds.truncate(args.nsfwdog_seed_limit);
	seeds
}

async fn discover_nsfwdog_similar_candidates(
	args: &Args,
	known_names: &BTreeSet<String>,
	cached_metadata: &[utils::Subreddit],
	counters: &mut SubredditSemanticBackfillCounters,
) -> Vec<String> {
	let seeds = nsfwdog_seed_names(args, known_names, cached_metadata);
	counters.nsfwdog_seed_count = seeds.len();
	if seeds.is_empty() {
		return Vec::new();
	}

	let mut seen = known_names.iter().cloned().collect::<HashSet<_>>();
	let mut discovered = Vec::new();
	for seed in seeds {
		if discovered.len() >= args.nsfwdog_similar_limit {
			break;
		}
		counters.nsfwdog_queries += 1;
		let results = match nsfwdog_similar_results(args, &seed.query_name).await {
			Ok(results) => results,
			Err(error) => {
				counters.nsfwdog_failed += 1;
				if !args.quiet {
					eprintln!("nsfwdog similar failed r/{}: {error}", seed.query_name);
				}
				continue;
			}
		};
		counters.nsfwdog_results += results.len();
		for result in results {
			if discovered.len() >= args.nsfwdog_similar_limit {
				break;
			}
			match nsfwdog_resolve_subreddit_name(args, &result.slug).await {
				Ok(name) => {
					counters.nsfwdog_resolved += 1;
					if !seen.insert(name.clone()) {
						continue;
					}
					if !args.quiet {
						println!("nsfwdog discovered r/{name} from r/{}", seed.query_name);
					}
					discovered.push(name);
					counters.nsfwdog_added += 1;
				}
				Err(error) => {
					counters.nsfwdog_failed += 1;
					if !args.quiet {
						eprintln!("nsfwdog resolve failed {} from r/{}: {error}", result.slug, seed.query_name);
					}
				}
			}
		}
	}
	discovered
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

async fn public_subreddit_metadata_from_base(name: &str, base_url: &str) -> Result<utils::Subreddit, String> {
	let uri = format!("{}/r/{name}/about.json?raw_json=1", base_url.trim_end_matches('/'))
		.parse::<Uri>()
		.map_err(|error| error.to_string())?;
	let request = Request::builder()
		.method(Method::GET)
		.uri(uri)
		.header(header::ACCEPT, "application/json")
		.header(header::USER_AGENT, "redlib-semantic-backfill/0.1")
		.body(Body::empty())
		.map_err(|error| error.to_string())?;
	let response = redlib::client::CLIENT.request(request).await.map_err(|error| error.to_string())?;
	let status = response.status();
	let bytes = to_bytes(response.into_body()).await.map_err(|error| error.to_string())?;
	if !status.is_success() {
		return Err(format!("public Reddit metadata returned HTTP {status}: {}", body_snippet(&bytes)));
	}
	let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
	if value["data"].is_null() {
		return Err("public Reddit metadata response did not contain data".to_string());
	}

	let members = value["data"]["subscribers"].as_u64().unwrap_or_default() as i64;
	let active = value["data"]["accounts_active"].as_u64().unwrap_or_default() as i64;
	let community_icon = value["data"]["community_icon"].as_str().unwrap_or_default();
	let icon = if community_icon.is_empty() {
		utils::val(&value, "icon_img")
	} else {
		community_icon.to_string()
	};
	let info = utils::rewrite_urls(&utils::val(&value, "description_html"));
	let mut subreddit = utils::Subreddit {
		name: utils::val(&value, "display_name"),
		title: utils::val(&value, "title"),
		description: utils::val(&value, "public_description"),
		has_sidebar: !info.trim().is_empty(),
		info,
		icon: utils::format_url(&icon),
		members: utils::format_num(members),
		active: utils::format_num(active),
		wiki: value["data"]["wiki_enabled"].as_bool().unwrap_or_default(),
		nsfw: value["data"]["over18"].as_bool().unwrap_or_default(),
	};
	if subreddit.name.trim().is_empty() {
		subreddit.name = name.to_string();
	}
	subreddit::cache_subreddit_metadata(&subreddit);
	Ok(subreddit)
}

async fn public_subreddit_metadata(name: &str) -> Result<utils::Subreddit, String> {
	let mut errors = Vec::new();
	for base_url in ["https://api.reddit.com", "https://www.reddit.com", "https://old.reddit.com"] {
		match public_subreddit_metadata_from_base(name, base_url).await {
			Ok(metadata) => return Ok(metadata),
			Err(error) => errors.push(format!("{base_url}: {error}")),
		}
	}
	Err(errors.join("; "))
}

async fn refresh_subreddit_metadata_for_backfill(name: &str) -> Result<(utils::Subreddit, bool), String> {
	match subreddit::refresh_subreddit_metadata(name, false).await {
		Ok(metadata) => Ok((metadata, false)),
		Err(primary_error) => match public_subreddit_metadata(name).await {
			Ok(metadata) => Ok((metadata, true)),
			Err(fallback_error) => Err(format!("{primary_error}; public fallback failed: {fallback_error}")),
		},
	}
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
	let mut counters = SubredditSemanticBackfillCounters::default();
	record_backfill_status(SubredditSemanticBackfillStatus {
		state: "running".to_string(),
		model: args.model.clone(),
		started_at_unix,
		finished_at_unix: None,
		counters: counters.clone(),
		dry_run: args.dry_run,
		force: args.force,
		limit: args.limit,
		error: None,
	});

	match run_backfill(&args, &mut counters).await {
		Ok(()) => {
			let state = if counters.failed > 0 { "completed_with_errors" } else { "completed" };
			record_backfill_status(SubredditSemanticBackfillStatus {
				state: state.to_string(),
				model: args.model.clone(),
				started_at_unix,
				finished_at_unix: Some(cache_persistence::unix_now()),
				counters: counters.clone(),
				dry_run: args.dry_run,
				force: args.force,
				limit: args.limit,
				error: None,
			});
			print_backfill_summary(&counters);
			Ok(())
		}
		Err(error) => {
			record_backfill_status(SubredditSemanticBackfillStatus {
				state: "failed".to_string(),
				model: args.model.clone(),
				started_at_unix,
				finished_at_unix: Some(cache_persistence::unix_now()),
				counters,
				dry_run: args.dry_run,
				force: args.force,
				limit: args.limit,
				error: Some(error.clone()),
			});
			Err(error)
		}
	}
}

fn record_backfill_status(status: SubredditSemanticBackfillStatus) {
	if let Err(error) = subreddit_semantics::write_backfill_status(&status) {
		eprintln!("backfill status write failed: {error}");
	}
}

fn print_backfill_summary(counters: &SubredditSemanticBackfillCounters) {
	println!(
		"subreddit_semantic_backfill: candidates={} embedded={} already_current={} nsfwdog_seeds={} nsfwdog_queries={} nsfwdog_results={} nsfwdog_resolved={} nsfwdog_added={} nsfwdog_failed={} metadata_refresh_attempted={} metadata_refresh_skipped={} refreshed_metadata={} public_fallback_metadata={} name_only_metadata={} missing_metadata={} failed={} cache={}",
		counters.total_candidates,
		counters.embedded,
		counters.already_current,
		counters.nsfwdog_seed_count,
		counters.nsfwdog_queries,
		counters.nsfwdog_results,
		counters.nsfwdog_resolved,
		counters.nsfwdog_added,
		counters.nsfwdog_failed,
		counters.metadata_refresh_attempted,
		counters.metadata_refresh_skipped,
		counters.refreshed_metadata,
		counters.public_fallback_metadata,
		counters.name_only_metadata,
		counters.missing_metadata,
		counters.failed,
		subreddit_semantics::persisted_embeddings_sidecar_path().display()
	);
}

async fn run_backfill(args: &Args, counters: &mut SubredditSemanticBackfillCounters) -> Result<(), String> {
	cache_persistence::init();
	let metadata_refresh_enabled = args.refresh_missing_metadata && args.metadata_refresh_limit.unwrap_or(usize::MAX) > 0;
	if metadata_refresh_enabled {
		std::sync::LazyLock::force(&redlib::client::OAUTH_CLIENT);
		redlib::oauth::force_refresh_token().await;
		let status = redlib::oauth::oauth_status_snapshot();
		if !status.available {
			eprintln!(
				"oauth unavailable for metadata refresh{}",
				status.last_error.as_ref().map(|error| format!(": {error}")).unwrap_or_default()
			);
		}
	}

	let mut known_names = materialized_catalog::cached_catalog_subreddit_names().into_iter().collect::<BTreeSet<_>>();
	let cached_metadata = subreddit::cached_subreddit_metadata_entries();
	for metadata in &cached_metadata {
		let name = subreddit_semantics::normalize_subreddit_name(&metadata.name);
		if !name.is_empty() {
			known_names.insert(name);
		}
	}
	for subreddit in &args.subreddits {
		let name = subreddit_semantics::normalize_subreddit_name(subreddit);
		if !name.is_empty() {
			known_names.insert(name);
		}
	}
	let discovered_names = discover_nsfwdog_similar_candidates(args, &known_names, &cached_metadata, counters).await;
	let mut names = known_names.into_iter().collect::<Vec<_>>();
	names.extend(discovered_names);
	let metadata_by_name = cached_metadata
		.into_iter()
		.map(|metadata| (subreddit_semantics::normalize_subreddit_name(&metadata.name), metadata))
		.collect::<HashMap<_, _>>();

	counters.total_candidates = names.len();
	let client = Client::new();

	for name in names.into_iter().take(args.limit.unwrap_or(usize::MAX)) {
		let metadata = match metadata_by_name.get(&name).cloned().or_else(|| subreddit::cached_subreddit_metadata_for_name(&name)) {
			Some(metadata) => metadata,
			None if args.refresh_missing_metadata && args.metadata_refresh_limit.map(|limit| counters.metadata_refresh_attempted < limit).unwrap_or(true) => {
				counters.metadata_refresh_attempted += 1;
				match refresh_subreddit_metadata_for_backfill(&name).await {
					Ok((metadata, used_public_fallback)) => {
						counters.refreshed_metadata += 1;
						if used_public_fallback {
							counters.public_fallback_metadata += 1;
						}
						metadata
					}
					Err(error) => {
						counters.missing_metadata += 1;
						eprintln!("metadata failed r/{name}: {error}");
						if args.name_only_if_missing_metadata {
							counters.name_only_metadata += 1;
							utils::Subreddit {
								name: name.clone(),
								..utils::Subreddit::default()
							}
						} else {
							continue;
						}
					}
				}
			}
			None if args.refresh_missing_metadata => {
				counters.metadata_refresh_skipped += 1;
				if args.name_only_if_missing_metadata {
					counters.name_only_metadata += 1;
					utils::Subreddit {
						name: name.clone(),
						..utils::Subreddit::default()
					}
				} else {
					counters.missing_metadata += 1;
					continue;
				}
			}
			None => {
				if args.name_only_if_missing_metadata {
					counters.name_only_metadata += 1;
					utils::Subreddit {
						name: name.clone(),
						..utils::Subreddit::default()
					}
				} else {
					counters.missing_metadata += 1;
					continue;
				}
			}
		};

		if !args.force && subreddit_semantics::has_current_embedding(&metadata, &args.model) {
			counters.already_current += 1;
			continue;
		}

		let text = subreddit_semantics::semantic_text_for_subreddit(&metadata);
		if text.is_empty() {
			counters.missing_metadata += 1;
			continue;
		}
		let text_hash = subreddit_semantics::semantic_text_hash(&text);
		if args.dry_run {
			if !args.quiet {
				println!("would embed r/{name} chars={} hash={}", text.chars().count(), text_hash);
			}
			continue;
		}

		match ollama_embedding(&client, &args.ollama_url, &args.model, &text).await {
			Ok(embedding) => {
				subreddit_semantics::store_embedding(&name, &args.model, &text_hash, embedding, cache_persistence::unix_now())?;
				counters.embedded += 1;
				if !args.quiet {
					println!("embedded r/{name}");
				}
			}
			Err(error) => {
				counters.failed += 1;
				eprintln!("embedding failed r/{name}: {error}");
			}
		}
	}

	if !args.dry_run {
		subreddit_semantics::write_persisted_embeddings_sidecar_now()?;
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn nsfwdog_subreddit_names_are_normalized_and_validated() {
		assert_eq!(normalize_nsfwdog_subreddit_name("r/IndianCelebScenes"), Some("indiancelebscenes".to_string()));
		assert_eq!(normalize_nsfwdog_subreddit_name("/r/Indian_Actress_Hub"), Some("indian_actress_hub".to_string()));
		assert_eq!(normalize_nsfwdog_subreddit_name("u_followeduser"), None);
		assert_eq!(normalize_nsfwdog_subreddit_name("r/not/a/subreddit"), None);
	}

	#[test]
	fn nsfwdog_slugs_are_restricted_to_safe_path_segments() {
		assert_eq!(safe_nsfwdog_slug("indian-actress-hub"), Some("indian-actress-hub".to_string()));
		assert_eq!(safe_nsfwdog_slug("../escape"), None);
		assert_eq!(safe_nsfwdog_slug(""), None);
	}

	#[test]
	fn nsfwdog_api_uri_encodes_query_parameters() {
		let uri = nsfwdog_api_uri(
			"https://api2.nsfwdog.com/",
			"/api/v1/similar",
			&[("subreddit", "r/IndianCelebScenes"), ("offset", "0"), ("limit", "10")],
		)
		.unwrap();
		assert_eq!(uri.to_string(), "https://api2.nsfwdog.com/api/v1/similar?subreddit=r%2FIndianCelebScenes&offset=0&limit=10");
	}
}
