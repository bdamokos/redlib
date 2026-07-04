use crate::utils::Subreddit;
use htmlescape::decode_html;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

pub const DEFAULT_SUBREDDIT_SEMANTIC_MODEL: &str = "nomic-embed-text";
const MAX_SUBREDDIT_SEMANTIC_TEXT_CHARS: usize = 1_200;

static HTML_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").unwrap());
static WHITESPACE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static SUBREDDIT_SEMANTIC_CACHE: LazyLock<Mutex<HashMap<String, PersistedSubredditSemanticEmbedding>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PersistedSubredditSemanticEmbedding {
	pub subreddit: String,
	pub model: String,
	pub text_hash: String,
	pub embedding: Vec<f32>,
	pub embedded_at_unix: u64,
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
	crate::cache_persistence::mark_dirty();
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

pub(crate) fn snapshot_persisted_embeddings(_now_unix: u64) -> Vec<PersistedSubredditSemanticEmbedding> {
	let Ok(cache) = SUBREDDIT_SEMANTIC_CACHE.lock() else {
		return Vec::new();
	};
	let mut entries = cache.values().cloned().collect::<Vec<_>>();
	entries.sort_by(|left, right| left.subreddit.cmp(&right.subreddit).then_with(|| left.model.cmp(&right.model)));
	entries
}

pub(crate) fn restore_persisted_embeddings(entries: Vec<PersistedSubredditSemanticEmbedding>, _saved_at_unix: u64, _now_unix: u64) {
	let restored = entries
		.into_iter()
		.filter_map(|mut entry| {
			entry.subreddit = normalize_subreddit_name(&entry.subreddit);
			entry.model = entry.model.trim().to_string();
			entry.text_hash = entry.text_hash.trim().to_string();
			entry.embedding = normalized_embedding(entry.embedding)?;
			(!entry.subreddit.is_empty() && !entry.model.is_empty() && !entry.text_hash.is_empty()).then_some((entry.subreddit.clone(), entry))
		})
		.collect::<HashMap<_, _>>();

	if let Ok(mut cache) = SUBREDDIT_SEMANTIC_CACHE.lock() {
		cache.extend(restored);
	}
}

#[cfg(test)]
pub(crate) fn clear_test_cache() {
	if let Ok(mut cache) = SUBREDDIT_SEMANTIC_CACHE.lock() {
		cache.clear();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

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
}
