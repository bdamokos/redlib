use crate::utils::Subreddit;
use htmlescape::decode_html;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const DEFAULT_SUBREDDIT_SEMANTIC_MODEL: &str = "nomic-embed-text";
const SUBREDDIT_SEMANTIC_SIDECAR_VERSION: u8 = 1;
const SUBREDDIT_SEMANTIC_SIDECAR_FILE_NAME: &str = "subreddit-semantic-embeddings.v1.bin";
const SUBREDDIT_SEMANTIC_SIDECAR_LOCK_FILE_NAME: &str = "subreddit-semantic-embeddings.v1.lock";
const SUBREDDIT_SEMANTIC_BACKFILL_STATUS_FILE_NAME: &str = "subreddit-semantic-backfill-status.v1.json";
const MAX_SUBREDDIT_SEMANTIC_TEXT_CHARS: usize = 1_200;
const SIDECAR_LOCK_TIMEOUT_SECS: u64 = 30;
const SIDECAR_STALE_LOCK_SECS: u64 = 30 * 60;

static HTML_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").unwrap());
static WHITESPACE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static SUBREDDIT_SEMANTIC_CACHE: LazyLock<Mutex<HashMap<String, PersistedSubredditSemanticEmbedding>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SUBREDDIT_SEMANTIC_SIDECAR_MTIME: LazyLock<Mutex<Option<SystemTime>>> = LazyLock::new(|| Mutex::new(None));

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedSubredditSemanticEmbedding {
	pub subreddit: String,
	pub model: String,
	pub text_hash: String,
	pub embedding: Vec<f32>,
	pub embedded_at_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSubredditSemanticEmbeddingFile {
	version: u8,
	saved_at_unix: u64,
	entries: Vec<PersistedSubredditSemanticEmbedding>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubredditSemanticBackfillCounters {
	pub total_candidates: usize,
	pub embedded: usize,
	pub already_current: usize,
	#[serde(default)]
	pub nsfwdog_seed_count: usize,
	#[serde(default)]
	pub nsfwdog_queries: usize,
	#[serde(default)]
	pub nsfwdog_results: usize,
	#[serde(default)]
	pub nsfwdog_resolved: usize,
	#[serde(default)]
	pub nsfwdog_added: usize,
	#[serde(default)]
	pub nsfwdog_failed: usize,
	pub metadata_refresh_attempted: usize,
	pub metadata_refresh_skipped: usize,
	pub refreshed_metadata: usize,
	pub public_fallback_metadata: usize,
	pub name_only_metadata: usize,
	pub missing_metadata: usize,
	pub failed: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubredditSemanticBackfillStatus {
	pub state: String,
	pub model: String,
	pub started_at_unix: u64,
	pub finished_at_unix: Option<u64>,
	pub counters: SubredditSemanticBackfillCounters,
	pub dry_run: bool,
	pub force: bool,
	pub limit: Option<usize>,
	pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubredditSemanticDatabaseStatus {
	pub model: String,
	pub sidecar_path: String,
	pub sidecar_exists: bool,
	pub sidecar_size_bytes: Option<u64>,
	pub sidecar_modified_unix: Option<u64>,
	pub sidecar_modified_age_secs: Option<u64>,
	pub sidecar_saved_at_unix: Option<u64>,
	pub sidecar_entry_count: usize,
	pub loaded_entry_count: usize,
	pub active_model_entry_count: usize,
	pub other_model_entry_count: usize,
	pub embedding_dimensions: Option<usize>,
	pub oldest_embedding_at_unix: Option<u64>,
	pub newest_embedding_at_unix: Option<u64>,
	pub refresh_error: Option<String>,
	pub sidecar_read_error: Option<String>,
	pub last_run: Option<SubredditSemanticBackfillStatus>,
	pub last_run_error: Option<String>,
}

pub fn embedding_model() -> String {
	std::env::var("REDLIB_SUBREDDIT_SEMANTIC_MODEL")
		.ok()
		.map(|model| model.trim().to_string())
		.filter(|model| !model.is_empty())
		.unwrap_or_else(|| DEFAULT_SUBREDDIT_SEMANTIC_MODEL.to_string())
}

pub fn normalize_subreddit_name(name: &str) -> String {
	let trimmed = name.trim();
	let without_path_prefix = trimmed.strip_prefix("/r/").unwrap_or(trimmed);
	let without_prefix = without_path_prefix.strip_prefix("r/").unwrap_or(without_path_prefix);
	without_prefix.trim().to_lowercase()
}

fn compact_text(value: &str) -> String {
	let without_tags = HTML_TAG_REGEX.replace_all(value, " ");
	let decoded = decode_html(&without_tags).unwrap_or_else(|_| without_tags.into_owned());
	WHITESPACE_REGEX.replace_all(decoded.trim(), " ").trim().to_string()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
	if value.chars().count() <= max_chars {
		return value.to_string();
	}
	value.chars().take(max_chars).collect::<String>()
}

pub fn semantic_text_for_subreddit(subreddit: &Subreddit) -> String {
	let name = normalize_subreddit_name(&subreddit.name);
	let mut parts = Vec::new();
	if !name.is_empty() {
		parts.push(format!("r/{name}"));
	}
	let title = compact_text(&subreddit.title);
	if !title.is_empty() && title.to_lowercase() != name {
		parts.push(title);
	}
	let description = compact_text(&subreddit.description);
	if !description.is_empty() {
		parts.push(description);
	}
	let info = compact_text(&subreddit.info);
	if !info.is_empty() {
		parts.push(info);
	}
	truncate_chars(&parts.join("\n"), MAX_SUBREDDIT_SEMANTIC_TEXT_CHARS)
}

pub fn semantic_text_hash(text: &str) -> String {
	let digest = Sha256::digest(text.as_bytes());
	digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn normalize_embedding(embedding: &mut [f32]) -> bool {
	let norm = embedding.iter().map(|value| (*value as f64) * (*value as f64)).sum::<f64>().sqrt();
	if norm <= 0.0 {
		return false;
	}
	for value in embedding {
		*value = (*value as f64 / norm) as f32;
	}
	true
}

fn normalized_embedding(mut embedding: Vec<f32>) -> Option<Vec<f32>> {
	if embedding.is_empty() || !embedding.iter().all(|value| value.is_finite()) {
		return None;
	}
	normalize_embedding(&mut embedding).then_some(embedding)
}

pub fn store_embedding(subreddit: &str, model: &str, text_hash: &str, embedding: Vec<f32>, embedded_at_unix: u64) -> Result<(), String> {
	let subreddit = normalize_subreddit_name(subreddit);
	let model = model.trim().to_string();
	let text_hash = text_hash.trim().to_string();
	if subreddit.is_empty() {
		return Err("subreddit name is empty".to_string());
	}
	if model.is_empty() {
		return Err("embedding model is empty".to_string());
	}
	if text_hash.is_empty() {
		return Err("semantic text hash is empty".to_string());
	}
	let Some(embedding) = normalized_embedding(embedding) else {
		return Err("embedding is empty, non-finite, or zero-length".to_string());
	};

	let Ok(mut cache) = SUBREDDIT_SEMANTIC_CACHE.lock() else {
		return Err("semantic embedding cache lock is poisoned".to_string());
	};
	cache.insert(
		subreddit.clone(),
		PersistedSubredditSemanticEmbedding {
			subreddit,
			model,
			text_hash,
			embedding,
			embedded_at_unix,
		},
	);
	Ok(())
}

pub fn embedding_for_subreddit(subreddit: &str) -> Option<Vec<f32>> {
	embedding_for_subreddit_model(subreddit, &embedding_model())
}

pub fn embedding_for_subreddit_model(subreddit: &str, model: &str) -> Option<Vec<f32>> {
	let subreddit = normalize_subreddit_name(subreddit);
	let model = model.trim();
	if subreddit.is_empty() || model.is_empty() {
		return None;
	}
	let cache = SUBREDDIT_SEMANTIC_CACHE.lock().ok()?;
	let entry = cache.get(&subreddit)?;
	(entry.model == model).then(|| entry.embedding.clone())
}

pub fn has_current_embedding(subreddit: &Subreddit, model: &str) -> bool {
	let subreddit_name = normalize_subreddit_name(&subreddit.name);
	if subreddit_name.is_empty() || model.trim().is_empty() {
		return false;
	}
	let text = semantic_text_for_subreddit(subreddit);
	if text.is_empty() {
		return false;
	}
	let text_hash = semantic_text_hash(&text);
	let Ok(cache) = SUBREDDIT_SEMANTIC_CACHE.lock() else {
		return false;
	};
	cache
		.get(&subreddit_name)
		.is_some_and(|entry| entry.model == model.trim() && entry.text_hash == text_hash && !entry.embedding.is_empty())
}

pub fn embedding_count_for_model(model: &str) -> usize {
	let model = model.trim();
	let Ok(cache) = SUBREDDIT_SEMANTIC_CACHE.lock() else {
		return 0;
	};
	cache.values().filter(|entry| entry.model == model && !entry.embedding.is_empty()).count()
}

pub fn active_embedding_count() -> usize {
	embedding_count_for_model(&embedding_model())
}

pub fn persisted_embeddings_sidecar_path() -> PathBuf {
	Path::new(&crate::cache_persistence::cache_dir()).join(SUBREDDIT_SEMANTIC_SIDECAR_FILE_NAME)
}

fn persisted_embeddings_lock_path() -> PathBuf {
	Path::new(&crate::cache_persistence::cache_dir()).join(SUBREDDIT_SEMANTIC_SIDECAR_LOCK_FILE_NAME)
}

pub fn backfill_status_path() -> PathBuf {
	Path::new(&crate::cache_persistence::cache_dir()).join(SUBREDDIT_SEMANTIC_BACKFILL_STATUS_FILE_NAME)
}

fn system_time_to_unix(time: SystemTime) -> Option<u64> {
	time.duration_since(UNIX_EPOCH).ok().map(|duration| duration.as_secs())
}

pub(crate) fn snapshot_persisted_embeddings(_now_unix: u64) -> Vec<PersistedSubredditSemanticEmbedding> {
	let Ok(cache) = SUBREDDIT_SEMANTIC_CACHE.lock() else {
		return Vec::new();
	};
	let mut entries = cache.values().cloned().collect::<Vec<_>>();
	entries.sort_by(|left, right| left.subreddit.cmp(&right.subreddit).then_with(|| left.model.cmp(&right.model)));
	entries
}

fn normalized_entry(mut entry: PersistedSubredditSemanticEmbedding) -> Option<(String, PersistedSubredditSemanticEmbedding)> {
	entry.subreddit = normalize_subreddit_name(&entry.subreddit);
	entry.model = entry.model.trim().to_string();
	entry.text_hash = entry.text_hash.trim().to_string();
	entry.embedding = normalized_embedding(entry.embedding)?;
	(!entry.subreddit.is_empty() && !entry.model.is_empty() && !entry.text_hash.is_empty()).then_some((entry.subreddit.clone(), entry))
}

fn normalized_entries_map(entries: Vec<PersistedSubredditSemanticEmbedding>) -> HashMap<String, PersistedSubredditSemanticEmbedding> {
	entries.into_iter().filter_map(normalized_entry).collect::<HashMap<_, _>>()
}

pub(crate) fn restore_persisted_embeddings(entries: Vec<PersistedSubredditSemanticEmbedding>, _saved_at_unix: u64, _now_unix: u64) {
	let restored = normalized_entries_map(entries);

	if let Ok(mut cache) = SUBREDDIT_SEMANTIC_CACHE.lock() {
		cache.extend(restored);
	}
}

fn read_persisted_embeddings_file(path: &Path) -> Result<Option<PersistedSubredditSemanticEmbeddingFile>, String> {
	let bytes = match fs::read(path) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.to_string()),
	};
	let file: PersistedSubredditSemanticEmbeddingFile = bincode::deserialize(&bytes).map_err(|error| error.to_string())?;
	if file.version != SUBREDDIT_SEMANTIC_SIDECAR_VERSION {
		return Err(format!("unsupported semantic embeddings sidecar version {}", file.version));
	}
	Ok(Some(file))
}

fn read_backfill_status_file(path: &Path) -> Result<Option<SubredditSemanticBackfillStatus>, String> {
	let bytes = match fs::read(path) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.to_string()),
	};
	serde_json::from_slice(&bytes).map(Some).map_err(|error| error.to_string())
}

pub fn read_backfill_status() -> Result<Option<SubredditSemanticBackfillStatus>, String> {
	read_backfill_status_file(&backfill_status_path())
}

pub fn write_backfill_status(status: &SubredditSemanticBackfillStatus) -> Result<(), String> {
	let final_path = backfill_status_path();
	if let Some(parent) = final_path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let bytes = serde_json::to_vec_pretty(status).map_err(|error| error.to_string())?;
	let temp_path = final_path.with_extension(format!("json.tmp.{}", std::process::id()));
	let mut temp_file = File::create(&temp_path).map_err(|error| error.to_string())?;
	temp_file.write_all(&bytes).map_err(|error| error.to_string())?;
	temp_file.sync_all().map_err(|error| error.to_string())?;
	drop(temp_file);
	fs::rename(&temp_path, &final_path).map_err(|error| error.to_string())
}

pub fn embedding_database_status() -> SubredditSemanticDatabaseStatus {
	let model = embedding_model();
	let refresh_error = refresh_persisted_embeddings_sidecar().err();
	let now_unix = crate::cache_persistence::unix_now();
	let sidecar_path = persisted_embeddings_sidecar_path();
	let sidecar_metadata = fs::metadata(&sidecar_path).ok();
	let sidecar_modified_unix = sidecar_metadata.as_ref().and_then(|metadata| metadata.modified().ok()).and_then(system_time_to_unix);
	let sidecar_modified_age_secs = sidecar_modified_unix.map(|modified_at| now_unix.saturating_sub(modified_at));
	let (sidecar_saved_at_unix, sidecar_entry_count, sidecar_read_error) = match read_persisted_embeddings_file(&sidecar_path) {
		Ok(Some(file)) => (Some(file.saved_at_unix), file.entries.len(), None),
		Ok(None) => (None, 0, None),
		Err(error) => (None, 0, Some(error)),
	};

	let entries = snapshot_persisted_embeddings(now_unix);
	let mut active_model_entry_count = 0usize;
	let mut other_model_entry_count = 0usize;
	let mut embedding_dimensions = None;
	let mut oldest_embedding_at_unix = None;
	let mut newest_embedding_at_unix = None;
	for entry in &entries {
		if entry.embedding.is_empty() {
			continue;
		}
		if entry.model == model {
			active_model_entry_count += 1;
			if embedding_dimensions.is_none() {
				embedding_dimensions = Some(entry.embedding.len());
			}
			oldest_embedding_at_unix = Some(
				oldest_embedding_at_unix
					.map(|oldest: u64| oldest.min(entry.embedded_at_unix))
					.unwrap_or(entry.embedded_at_unix),
			);
			newest_embedding_at_unix = Some(
				newest_embedding_at_unix
					.map(|newest: u64| newest.max(entry.embedded_at_unix))
					.unwrap_or(entry.embedded_at_unix),
			);
		} else {
			other_model_entry_count += 1;
		}
	}
	let (last_run, last_run_error) = match read_backfill_status() {
		Ok(status) => (status, None),
		Err(error) => (None, Some(error)),
	};

	SubredditSemanticDatabaseStatus {
		model,
		sidecar_path: sidecar_path.display().to_string(),
		sidecar_exists: sidecar_metadata.is_some(),
		sidecar_size_bytes: sidecar_metadata.as_ref().map(|metadata| metadata.len()),
		sidecar_modified_unix,
		sidecar_modified_age_secs,
		sidecar_saved_at_unix,
		sidecar_entry_count,
		loaded_entry_count: entries.len(),
		active_model_entry_count,
		other_model_entry_count,
		embedding_dimensions,
		oldest_embedding_at_unix,
		newest_embedding_at_unix,
		refresh_error,
		sidecar_read_error,
		last_run,
		last_run_error,
	}
}

pub(crate) fn load_persisted_embeddings_sidecar() -> Result<(), String> {
	let path = persisted_embeddings_sidecar_path();
	let modified = fs::metadata(&path).and_then(|metadata| metadata.modified()).ok();
	let Some(file) = read_persisted_embeddings_file(&path)? else {
		if let Ok(mut mtime) = SUBREDDIT_SEMANTIC_SIDECAR_MTIME.lock() {
			*mtime = None;
		}
		return Ok(());
	};
	restore_persisted_embeddings(file.entries, file.saved_at_unix, crate::cache_persistence::unix_now());
	if let Ok(mut mtime) = SUBREDDIT_SEMANTIC_SIDECAR_MTIME.lock() {
		*mtime = modified;
	}
	Ok(())
}

pub fn refresh_persisted_embeddings_sidecar() -> Result<bool, String> {
	let path = persisted_embeddings_sidecar_path();
	let modified = match fs::metadata(&path).and_then(|metadata| metadata.modified()) {
		Ok(modified) => modified,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			if let Ok(mut mtime) = SUBREDDIT_SEMANTIC_SIDECAR_MTIME.lock() {
				*mtime = None;
			}
			return Ok(false);
		}
		Err(error) => return Err(error.to_string()),
	};

	if SUBREDDIT_SEMANTIC_SIDECAR_MTIME.lock().ok().and_then(|mtime| *mtime) == Some(modified) {
		return Ok(false);
	}

	load_persisted_embeddings_sidecar()?;
	Ok(true)
}

struct SidecarLock {
	path: PathBuf,
}

impl Drop for SidecarLock {
	fn drop(&mut self) {
		let _ = fs::remove_file(&self.path);
	}
}

fn lock_file_is_stale(path: &Path) -> bool {
	fs::metadata(path)
		.and_then(|metadata| metadata.modified())
		.ok()
		.and_then(|modified| modified.elapsed().ok())
		.is_some_and(|age| age > Duration::from_secs(SIDECAR_STALE_LOCK_SECS))
}

fn acquire_sidecar_lock() -> Result<SidecarLock, String> {
	let lock_path = persisted_embeddings_lock_path();
	if let Some(parent) = lock_path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let started = Instant::now();
	loop {
		match OpenOptions::new().write(true).create_new(true).open(&lock_path) {
			Ok(mut file) => {
				file.write_all(std::process::id().to_string().as_bytes()).map_err(|error| error.to_string())?;
				file.sync_all().map_err(|error| error.to_string())?;
				return Ok(SidecarLock { path: lock_path });
			}
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
				if lock_file_is_stale(&lock_path) {
					let _ = fs::remove_file(&lock_path);
					continue;
				}
				if started.elapsed() > Duration::from_secs(SIDECAR_LOCK_TIMEOUT_SECS) {
					return Err(format!("timed out waiting for semantic embeddings sidecar lock {}", lock_path.display()));
				}
				thread::sleep(Duration::from_millis(100));
			}
			Err(error) => return Err(error.to_string()),
		}
	}
}

pub fn write_persisted_embeddings_sidecar_now() -> Result<(), String> {
	let final_path = persisted_embeddings_sidecar_path();
	if let Some(parent) = final_path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let _lock = acquire_sidecar_lock()?;
	let mut entries_by_subreddit = read_persisted_embeddings_file(&final_path)?
		.map(|file| normalized_entries_map(file.entries))
		.unwrap_or_default();
	for entry in snapshot_persisted_embeddings(crate::cache_persistence::unix_now()) {
		entries_by_subreddit.insert(entry.subreddit.clone(), entry);
	}
	let mut entries = entries_by_subreddit.into_values().collect::<Vec<_>>();
	entries.sort_by(|left, right| left.subreddit.cmp(&right.subreddit).then_with(|| left.model.cmp(&right.model)));

	let file = PersistedSubredditSemanticEmbeddingFile {
		version: SUBREDDIT_SEMANTIC_SIDECAR_VERSION,
		saved_at_unix: crate::cache_persistence::unix_now(),
		entries,
	};
	let bytes = bincode::serialize(&file).map_err(|error| error.to_string())?;
	let temp_path = final_path.with_extension(format!("tmp.{}", std::process::id()));
	let mut temp_file = File::create(&temp_path).map_err(|error| error.to_string())?;
	temp_file.write_all(&bytes).map_err(|error| error.to_string())?;
	temp_file.sync_all().map_err(|error| error.to_string())?;
	drop(temp_file);
	fs::rename(&temp_path, &final_path).map_err(|error| error.to_string())?;

	if let Ok(modified) = fs::metadata(&final_path).and_then(|metadata| metadata.modified()) {
		if let Ok(mut mtime) = SUBREDDIT_SEMANTIC_SIDECAR_MTIME.lock() {
			*mtime = Some(modified);
		}
	}
	Ok(())
}

#[cfg(test)]
pub(crate) fn clear_test_cache() {
	if let Ok(mut cache) = SUBREDDIT_SEMANTIC_CACHE.lock() {
		cache.clear();
	}
	if let Ok(mut mtime) = SUBREDDIT_SEMANTIC_SIDECAR_MTIME.lock() {
		*mtime = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::ffi::OsString;
	use std::path::PathBuf;
	use std::sync::{LazyLock, Mutex};
	use std::time::{SystemTime, UNIX_EPOCH};

	static TEST_ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

	struct CacheDirGuard {
		previous: Option<OsString>,
		path: PathBuf,
	}

	struct EnvVarGuard {
		key: &'static str,
		previous: Option<OsString>,
	}

	impl CacheDirGuard {
		fn new(label: &str) -> Self {
			let previous = std::env::var_os("REDLIB_CACHE_DIR");
			let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
			let path = std::env::temp_dir().join(format!("redlib-semantic-{label}-{}-{unique}", std::process::id()));
			std::fs::create_dir_all(&path).unwrap();
			std::env::set_var("REDLIB_CACHE_DIR", &path);
			Self { previous, path }
		}
	}

	impl EnvVarGuard {
		fn set(key: &'static str, value: &str) -> Self {
			let previous = std::env::var_os(key);
			std::env::set_var(key, value);
			Self { key, previous }
		}
	}

	impl Drop for CacheDirGuard {
		fn drop(&mut self) {
			if let Some(previous) = &self.previous {
				std::env::set_var("REDLIB_CACHE_DIR", previous);
			} else {
				std::env::remove_var("REDLIB_CACHE_DIR");
			}
			let _ = std::fs::remove_dir_all(&self.path);
			clear_test_cache();
		}
	}

	impl Drop for EnvVarGuard {
		fn drop(&mut self) {
			if let Some(previous) = &self.previous {
				std::env::set_var(self.key, previous);
			} else {
				std::env::remove_var(self.key);
			}
		}
	}

	#[test]
	fn semantic_text_combines_name_title_and_description() {
		let text = semantic_text_for_subreddit(&Subreddit {
			name: "Rust".to_string(),
			title: "Rust language".to_string(),
			description: "A place for Rustaceans".to_string(),
			info: "<p>systems programming</p>".to_string(),
			..Subreddit::default()
		});

		assert!(text.contains("r/rust"));
		assert!(text.contains("Rust language"));
		assert!(text.contains("A place for Rustaceans"));
		assert!(text.contains("systems programming"));
	}

	#[test]
	fn stored_embeddings_are_normalized_and_model_scoped() {
		clear_test_cache();
		store_embedding("Rust", "test-model", "hash", vec![3.0, 4.0], 1).unwrap();

		let embedding = embedding_for_subreddit_model("rust", "test-model").unwrap();
		assert!((embedding[0] - 0.6).abs() < 0.0001);
		assert!((embedding[1] - 0.8).abs() < 0.0001);
		assert_eq!(embedding_for_subreddit_model("rust", "other-model"), None);
	}

	#[test]
	fn sidecar_round_trips_embeddings() {
		let _lock = TEST_ENV_LOCK.lock().unwrap();
		let _guard = CacheDirGuard::new("round-trip");
		clear_test_cache();

		store_embedding("Rust", "test-model", "hash", vec![3.0, 4.0], 1).unwrap();
		write_persisted_embeddings_sidecar_now().unwrap();
		assert!(persisted_embeddings_sidecar_path().is_file());

		clear_test_cache();
		assert_eq!(embedding_for_subreddit_model("rust", "test-model"), None);
		assert!(refresh_persisted_embeddings_sidecar().unwrap());

		let embedding = embedding_for_subreddit_model("rust", "test-model").unwrap();
		assert!((embedding[0] - 0.6).abs() < 0.0001);
		assert!((embedding[1] - 0.8).abs() < 0.0001);
	}

	#[test]
	fn sidecar_write_merges_existing_entries() {
		let _lock = TEST_ENV_LOCK.lock().unwrap();
		let _guard = CacheDirGuard::new("merge");
		clear_test_cache();

		store_embedding("Rust", "test-model", "rust-hash", vec![1.0, 0.0], 1).unwrap();
		write_persisted_embeddings_sidecar_now().unwrap();
		clear_test_cache();

		store_embedding("Baking", "test-model", "baking-hash", vec![0.0, 2.0], 2).unwrap();
		write_persisted_embeddings_sidecar_now().unwrap();
		clear_test_cache();

		assert!(refresh_persisted_embeddings_sidecar().unwrap());
		assert!(embedding_for_subreddit_model("rust", "test-model").is_some());
		assert!(embedding_for_subreddit_model("baking", "test-model").is_some());
	}

	#[test]
	fn database_status_reports_sidecar_and_backfill_status() {
		let _lock = TEST_ENV_LOCK.lock().unwrap();
		let _guard = CacheDirGuard::new("status");
		let _model_guard = EnvVarGuard::set("REDLIB_SUBREDDIT_SEMANTIC_MODEL", "test-model");
		clear_test_cache();

		store_embedding("Rust", "test-model", "rust-hash", vec![1.0, 0.0], 10).unwrap();
		store_embedding("Baking", "other-model", "baking-hash", vec![0.0, 1.0], 20).unwrap();
		write_persisted_embeddings_sidecar_now().unwrap();
		write_backfill_status(&SubredditSemanticBackfillStatus {
			state: "completed".to_string(),
			model: "test-model".to_string(),
			started_at_unix: 100,
			finished_at_unix: Some(120),
			counters: SubredditSemanticBackfillCounters {
				total_candidates: 3,
				embedded: 1,
				already_current: 2,
				..SubredditSemanticBackfillCounters::default()
			},
			dry_run: false,
			force: false,
			limit: Some(3),
			error: None,
		})
		.unwrap();
		clear_test_cache();

		let status = embedding_database_status();
		assert!(status.sidecar_exists);
		assert_eq!(status.model, "test-model");
		assert_eq!(status.sidecar_entry_count, 2);
		assert_eq!(status.loaded_entry_count, 2);
		assert_eq!(status.active_model_entry_count, 1);
		assert_eq!(status.other_model_entry_count, 1);
		assert_eq!(status.embedding_dimensions, Some(2));
		assert_eq!(status.oldest_embedding_at_unix, Some(10));
		assert_eq!(status.newest_embedding_at_unix, Some(10));
		assert_eq!(status.last_run.unwrap().counters.embedded, 1);
	}
}
