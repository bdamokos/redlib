use crate::materialized_catalog::{SuggestedProfileScoringMode, SuggestedProfileVectorCenterRequest, SuggestedProfilesReport, SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT};
use crate::utils::{get_filters, param, read_sharded_cookie, template, Preferences};
use askama::Template;
use hyper::{Body, Request, Response};
use std::collections::{HashMap, HashSet};

const SUGGESTED_PROFILE_DEFAULT_LIMIT: usize = 40;
const SUGGESTED_PROFILE_INCREMENT: usize = 40;
const SUGGESTED_PROFILE_MAX_LIMIT: usize = 200;
const SUGGESTED_PROFILE_IGNORE_COOKIE: &str = "suggested_profile_ignores";
const SUGGESTED_PROFILE_IGNORE_TTL_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Template)]
#[template(path = "suggested_profiles.html")]
struct SuggestedProfilesTemplate {
	prefs: Preferences,
	url: String,
	redirect_url: String,
	report: SuggestedProfilesReport,
	scoring_mode: SuggestedProfileScoringMode,
	vector_center: Option<SuggestedProfileVectorCenterRequest>,
	semantic_mix_percent: u8,
	suggestion_limit: usize,
	next_suggestion_limit: usize,
	has_more_suggestions: bool,
}

crate::impl_template_str_eq!(SuggestedProfilesTemplate);

impl SuggestedProfilesTemplate {
	fn profile_path(&self, name: &str) -> String {
		format!("/user/{name}")
	}

	fn subscribe_path(&self, name: &str) -> String {
		format!("/r/u_{name}/subscribe?redirect={}&canonical=u_{name}", self.redirect_url)
	}

	fn unsubscribe_path(&self, name: &str) -> String {
		format!("/r/u_{name}/unsubscribe?redirect={}&canonical=u_{name}", self.redirect_url)
	}

	fn load_more_path(&self) -> String {
		let center_id = self.vector_center.as_ref().map(SuggestedProfileVectorCenterRequest::id);
		page_path_for_limit_center_id(self.next_suggestion_limit, self.scoring_mode, center_id.as_deref(), self.semantic_mix_percent)
	}

	fn overlap_mode_path(&self) -> String {
		page_path_for_limit_center_id(SUGGESTED_PROFILE_DEFAULT_LIMIT, SuggestedProfileScoringMode::Overlap, None, self.semantic_mix_percent)
	}

	fn vector_mode_path(&self) -> String {
		let center_id = self.vector_center.as_ref().map(SuggestedProfileVectorCenterRequest::id);
		page_path_for_limit_center_id(
			SUGGESTED_PROFILE_DEFAULT_LIMIT,
			SuggestedProfileScoringMode::Vector,
			center_id.as_deref(),
			self.semantic_mix_percent,
		)
	}

	fn semantic_mode_path(&self) -> String {
		let center_id = self.vector_center.as_ref().map(SuggestedProfileVectorCenterRequest::id);
		page_path_for_limit_center_id(
			SUGGESTED_PROFILE_DEFAULT_LIMIT,
			SuggestedProfileScoringMode::Semantic,
			center_id.as_deref(),
			self.semantic_mix_percent,
		)
	}

	fn vector_center_path(&self, center_id: &str) -> String {
		let mode = if self.is_semantic_mode() {
			SuggestedProfileScoringMode::Semantic
		} else {
			SuggestedProfileScoringMode::Vector
		};
		page_path_for_limit_center_id(SUGGESTED_PROFILE_DEFAULT_LIMIT, mode, Some(center_id), self.semantic_mix_percent)
	}

	fn clear_vector_center_path(&self) -> String {
		page_path_for_limit_center_id(SUGGESTED_PROFILE_DEFAULT_LIMIT, self.scoring_mode, None, self.semantic_mix_percent)
	}

	fn semantic_mix_path(&self, semantic_mix_percent: u8) -> String {
		let center_id = self.vector_center.as_ref().map(SuggestedProfileVectorCenterRequest::id);
		page_path_for_limit_center_id(
			SUGGESTED_PROFILE_DEFAULT_LIMIT,
			SuggestedProfileScoringMode::Semantic,
			center_id.as_deref(),
			semantic_mix_percent,
		)
	}

	fn current_vector_center_id(&self) -> String {
		self.vector_center.as_ref().map(SuggestedProfileVectorCenterRequest::id).unwrap_or_default()
	}

	fn is_overlap_mode(&self) -> bool {
		self.scoring_mode == SuggestedProfileScoringMode::Overlap
	}

	fn is_vector_mode(&self) -> bool {
		self.scoring_mode == SuggestedProfileScoringMode::Vector
	}

	fn is_semantic_mode(&self) -> bool {
		self.scoring_mode == SuggestedProfileScoringMode::Semantic
	}

	fn next_batch_size(&self) -> usize {
		self.next_suggestion_limit.saturating_sub(self.suggestion_limit)
	}
}

fn followed_profiles(prefs: &Preferences) -> Vec<String> {
	prefs
		.subscriptions
		.iter()
		.filter_map(|subscription| subscription.strip_prefix("u_").map(str::to_string))
		.collect()
}

fn filtered_profiles(filters: &HashSet<String>) -> HashSet<String> {
	filters.iter().filter_map(|filter| filter.strip_prefix("u_").map(str::to_string)).collect()
}

fn ignored_profiles(req: &Request<Body>, now_unix: u64) -> HashSet<String> {
	read_sharded_cookie(req, SUGGESTED_PROFILE_IGNORE_COOKIE)
		.split('+')
		.filter_map(|entry| {
			let (name, ignored_at) = entry.split_once(':')?;
			let ignored_at = ignored_at.parse::<u64>().ok()?;
			let name = name.trim().to_lowercase();
			(!name.is_empty() && now_unix.saturating_sub(ignored_at) <= SUGGESTED_PROFILE_IGNORE_TTL_SECS).then_some(name)
		})
		.collect()
}

fn followed_profile_subreddit_cache(followed: &[String]) -> HashMap<String, Vec<(String, u32)>> {
	followed
		.iter()
		.filter_map(|profile| {
			let subreddits = crate::utils::cached_user_subreddits(profile);
			(!subreddits.is_empty()).then_some((profile.trim().to_lowercase(), subreddits))
		})
		.collect()
}

fn requested_suggestion_limit(req: &Request<Body>) -> usize {
	let query = format!("?{}", req.uri().query().unwrap_or_default());
	param(&query, "limit")
		.and_then(|limit| limit.parse::<usize>().ok())
		.filter(|limit| *limit > 0)
		.map(|limit| limit.clamp(SUGGESTED_PROFILE_DEFAULT_LIMIT, SUGGESTED_PROFILE_MAX_LIMIT))
		.unwrap_or(SUGGESTED_PROFILE_DEFAULT_LIMIT)
}

fn requested_scoring_mode(req: &Request<Body>) -> SuggestedProfileScoringMode {
	let query = format!("?{}", req.uri().query().unwrap_or_default());
	match param(&query, "mode").as_deref() {
		Some("vector") => SuggestedProfileScoringMode::Vector,
		Some("semantic") => SuggestedProfileScoringMode::Semantic,
		_ => SuggestedProfileScoringMode::Overlap,
	}
}

fn requested_semantic_mix_percent(req: &Request<Body>) -> u8 {
	let query = format!("?{}", req.uri().query().unwrap_or_default());
	param(&query, "mix")
		.and_then(|mix| mix.parse::<u8>().ok())
		.map(|mix| mix.min(100))
		.unwrap_or(SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT)
}

fn requested_vector_center(req: &Request<Body>) -> Option<SuggestedProfileVectorCenterRequest> {
	let query = format!("?{}", req.uri().query().unwrap_or_default());
	param(&query, "center").and_then(|center| SuggestedProfileVectorCenterRequest::parse(&center))
}

#[cfg(test)]
fn page_path_for_limit(limit: usize, mode: SuggestedProfileScoringMode) -> String {
	page_path_for_limit_center_id(limit, mode, None, SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT)
}

fn page_path_for_limit_center_id(limit: usize, mode: SuggestedProfileScoringMode, center_id: Option<&str>, semantic_mix_percent: u8) -> String {
	let effective_mode = if center_id.is_some() && mode == SuggestedProfileScoringMode::Overlap {
		SuggestedProfileScoringMode::Vector
	} else {
		mode
	};
	let mut serializer = url::form_urlencoded::Serializer::new(String::new());
	match effective_mode {
		SuggestedProfileScoringMode::Overlap => {}
		SuggestedProfileScoringMode::Vector => {
			serializer.append_pair("mode", "vector");
		}
		SuggestedProfileScoringMode::Semantic => {
			serializer.append_pair("mode", "semantic");
			if semantic_mix_percent != SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT {
				serializer.append_pair("mix", &semantic_mix_percent.min(100).to_string());
			}
		}
	}
	if let Some(center_id) = center_id {
		serializer.append_pair("center", center_id);
	}
	if limit > SUGGESTED_PROFILE_DEFAULT_LIMIT {
		serializer.append_pair("limit", &limit.to_string());
	}
	let query = serializer.finish();
	if query.is_empty() {
		"/suggested-profiles".to_string()
	} else {
		format!("/suggested-profiles?{query}")
	}
}

fn redirect_url_for_limit(limit: usize, mode: SuggestedProfileScoringMode, center_id: Option<&str>, semantic_mix_percent: u8) -> String {
	let path = page_path_for_limit_center_id(limit, mode, center_id, semantic_mix_percent);
	url::form_urlencoded::byte_serialize(path.trim_start_matches('/').as_bytes()).collect()
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let filters = get_filters(&req);
	let followed = followed_profiles(&prefs);
	let followed_subreddits = followed_profile_subreddit_cache(&followed);
	let mut excluded = filtered_profiles(&filters);
	excluded.extend(ignored_profiles(&req, crate::cache_persistence::unix_now()));
	let suggestion_limit = requested_suggestion_limit(&req);
	let requested_center = requested_vector_center(&req);
	let requested_mode = requested_scoring_mode(&req);
	let scoring_mode = if requested_center.is_some() && requested_mode == SuggestedProfileScoringMode::Overlap {
		SuggestedProfileScoringMode::Vector
	} else {
		requested_mode
	};
	let semantic_mix_percent = requested_semantic_mix_percent(&req);
	let vector_center = matches!(scoring_mode, SuggestedProfileScoringMode::Vector | SuggestedProfileScoringMode::Semantic)
		.then_some(requested_center)
		.flatten();
	let vector_center_id = vector_center.as_ref().map(SuggestedProfileVectorCenterRequest::id);
	let fetch_limit = if suggestion_limit < SUGGESTED_PROFILE_MAX_LIMIT {
		suggestion_limit + 1
	} else {
		suggestion_limit
	};
	let author_feedback = prefs.post_feedback_by_model_and_author();
	let mut report = crate::materialized_catalog::suggested_profiles_with_mode_center_mix_and_feedback(
		&followed,
		&followed_subreddits,
		&excluded,
		&author_feedback,
		fetch_limit,
		scoring_mode,
		vector_center.as_ref(),
		semantic_mix_percent,
	);
	let has_more_suggestions = report.suggestions.len() > suggestion_limit && suggestion_limit < SUGGESTED_PROFILE_MAX_LIMIT;
	report.suggestions.truncate(suggestion_limit);

	Ok(template(&SuggestedProfilesTemplate {
		prefs,
		url: "/suggested-profiles".to_string(),
		redirect_url: redirect_url_for_limit(suggestion_limit, scoring_mode, vector_center_id.as_deref(), semantic_mix_percent),
		report,
		scoring_mode,
		vector_center,
		semantic_mix_percent,
		suggestion_limit,
		next_suggestion_limit: (suggestion_limit + SUGGESTED_PROFILE_INCREMENT).min(SUGGESTED_PROFILE_MAX_LIMIT),
		has_more_suggestions,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn request_for(uri: &str) -> Request<Body> {
		Request::builder().uri(uri).body(Body::empty()).expect("valid test request")
	}

	#[test]
	fn requested_suggestion_limit_defaults_without_query() {
		assert_eq!(requested_suggestion_limit(&request_for("/suggested-profiles")), SUGGESTED_PROFILE_DEFAULT_LIMIT);
	}

	#[test]
	fn requested_suggestion_limit_uses_query_value() {
		assert_eq!(requested_suggestion_limit(&request_for("/suggested-profiles?limit=80")), 80);
	}

	#[test]
	fn requested_suggestion_limit_clamps_low_values_to_default() {
		assert_eq!(requested_suggestion_limit(&request_for("/suggested-profiles?limit=3")), SUGGESTED_PROFILE_DEFAULT_LIMIT);
	}

	#[test]
	fn requested_suggestion_limit_clamps_high_values_to_maximum() {
		assert_eq!(requested_suggestion_limit(&request_for("/suggested-profiles?limit=999")), SUGGESTED_PROFILE_MAX_LIMIT);
	}

	#[test]
	fn requested_scoring_mode_defaults_to_overlap() {
		assert_eq!(requested_scoring_mode(&request_for("/suggested-profiles")), SuggestedProfileScoringMode::Overlap);
		assert_eq!(
			requested_scoring_mode(&request_for("/suggested-profiles?mode=unknown")),
			SuggestedProfileScoringMode::Overlap
		);
	}

	#[test]
	fn requested_scoring_mode_accepts_vector() {
		assert_eq!(requested_scoring_mode(&request_for("/suggested-profiles?mode=vector")), SuggestedProfileScoringMode::Vector);
	}

	#[test]
	fn requested_vector_center_accepts_cluster_and_subreddit() {
		assert_eq!(
			requested_vector_center(&request_for("/suggested-profiles?center=cluster%3A2")),
			Some(SuggestedProfileVectorCenterRequest::Cluster(1))
		);
		assert_eq!(
			requested_vector_center(&request_for("/suggested-profiles?center=subreddit%3ARust")),
			Some(SuggestedProfileVectorCenterRequest::Subreddit("rust".to_string()))
		);
		assert_eq!(requested_vector_center(&request_for("/suggested-profiles?center=cluster%3A0")), None);
	}

	#[test]
	fn ignored_profiles_keeps_entries_for_thirty_days() {
		let now = 1_700_000_000;
		let recent = now - SUGGESTED_PROFILE_IGNORE_TTL_SECS;
		let expired = recent - 1;
		let req = Request::builder()
			.uri("/suggested-profiles")
			.header("cookie", format!("suggested_profile_ignores=Demo:{recent}+Old:{expired}+bad+Nope:not-a-time"))
			.body(Body::empty())
			.expect("valid test request");

		let ignored = ignored_profiles(&req, now);

		assert!(ignored.contains("demo"));
		assert!(!ignored.contains("old"));
		assert!(!ignored.contains("nope"));
	}

	#[test]
	fn page_paths_preserve_vector_mode_for_load_more() {
		assert_eq!(
			page_path_for_limit(SUGGESTED_PROFILE_DEFAULT_LIMIT, SuggestedProfileScoringMode::Overlap),
			"/suggested-profiles"
		);
		assert_eq!(page_path_for_limit(80, SuggestedProfileScoringMode::Overlap), "/suggested-profiles?limit=80");
		assert_eq!(
			page_path_for_limit(SUGGESTED_PROFILE_DEFAULT_LIMIT, SuggestedProfileScoringMode::Vector),
			"/suggested-profiles?mode=vector"
		);
		assert_eq!(page_path_for_limit(80, SuggestedProfileScoringMode::Vector), "/suggested-profiles?mode=vector&limit=80");
	}

	#[test]
	fn page_paths_preserve_vector_center_for_load_more() {
		assert_eq!(
			page_path_for_limit_center_id(
				SUGGESTED_PROFILE_DEFAULT_LIMIT,
				SuggestedProfileScoringMode::Vector,
				Some("subreddit:rust"),
				SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT
			),
			"/suggested-profiles?mode=vector&center=subreddit%3Arust"
		);
		assert_eq!(
			page_path_for_limit_center_id(80, SuggestedProfileScoringMode::Vector, Some("cluster:2"), SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT),
			"/suggested-profiles?mode=vector&center=cluster%3A2&limit=80"
		);
		assert_eq!(
			page_path_for_limit_center_id(
				80,
				SuggestedProfileScoringMode::Overlap,
				Some("subreddit:rust"),
				SUGGESTED_PROFILE_SEMANTIC_DEFAULT_MIX_PERCENT
			),
			"/suggested-profiles?mode=vector&center=subreddit%3Arust&limit=80"
		);
	}

	#[test]
	fn page_paths_preserve_semantic_mix() {
		assert_eq!(
			page_path_for_limit_center_id(SUGGESTED_PROFILE_DEFAULT_LIMIT, SuggestedProfileScoringMode::Semantic, None, 75),
			"/suggested-profiles?mode=semantic&mix=75"
		);
		assert_eq!(
			page_path_for_limit_center_id(80, SuggestedProfileScoringMode::Semantic, Some("cluster:2"), 25),
			"/suggested-profiles?mode=semantic&mix=25&center=cluster%3A2&limit=80"
		);
	}
}
