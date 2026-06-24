use crate::materialized_catalog::{SuggestedProfileScoringMode, SuggestedProfilesReport};
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
		page_path_for_limit(self.next_suggestion_limit, self.scoring_mode)
	}

	fn overlap_mode_path(&self) -> String {
		page_path_for_limit(SUGGESTED_PROFILE_DEFAULT_LIMIT, SuggestedProfileScoringMode::Overlap)
	}

	fn vector_mode_path(&self) -> String {
		page_path_for_limit(SUGGESTED_PROFILE_DEFAULT_LIMIT, SuggestedProfileScoringMode::Vector)
	}

	fn is_overlap_mode(&self) -> bool {
		self.scoring_mode == SuggestedProfileScoringMode::Overlap
	}

	fn is_vector_mode(&self) -> bool {
		self.scoring_mode == SuggestedProfileScoringMode::Vector
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
		_ => SuggestedProfileScoringMode::Overlap,
	}
}

fn page_path_for_limit(limit: usize, mode: SuggestedProfileScoringMode) -> String {
	let mut params = Vec::new();
	if mode == SuggestedProfileScoringMode::Vector {
		params.push("mode=vector".to_string());
	}
	if limit > SUGGESTED_PROFILE_DEFAULT_LIMIT {
		params.push(format!("limit={limit}"));
	}
	if params.is_empty() {
		"/suggested-profiles".to_string()
	} else {
		format!("/suggested-profiles?{}", params.join("&"))
	}
}

fn redirect_url_for_limit(limit: usize, mode: SuggestedProfileScoringMode) -> String {
	let path = page_path_for_limit(limit, mode);
	path.trim_start_matches('/').replace('?', "%3F").replace('=', "%3D").replace('&', "%26")
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let filters = get_filters(&req);
	let followed = followed_profiles(&prefs);
	let followed_subreddits = followed_profile_subreddit_cache(&followed);
	let mut excluded = filtered_profiles(&filters);
	excluded.extend(ignored_profiles(&req, crate::cache_persistence::unix_now()));
	let suggestion_limit = requested_suggestion_limit(&req);
	let scoring_mode = requested_scoring_mode(&req);
	let fetch_limit = if suggestion_limit < SUGGESTED_PROFILE_MAX_LIMIT {
		suggestion_limit + 1
	} else {
		suggestion_limit
	};
	let mut report = crate::materialized_catalog::suggested_profiles_with_mode(&followed, &followed_subreddits, &excluded, fetch_limit, scoring_mode);
	let has_more_suggestions = report.suggestions.len() > suggestion_limit && suggestion_limit < SUGGESTED_PROFILE_MAX_LIMIT;
	report.suggestions.truncate(suggestion_limit);

	Ok(template(&SuggestedProfilesTemplate {
		prefs,
		url: "/suggested-profiles".to_string(),
		redirect_url: redirect_url_for_limit(suggestion_limit, scoring_mode),
		report,
		scoring_mode,
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
}
