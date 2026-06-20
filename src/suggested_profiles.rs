use crate::materialized_catalog::SuggestedProfilesReport;
use crate::utils::{get_filters, param, template, Preferences};
use askama::Template;
use hyper::{Body, Request, Response};
use std::collections::{HashMap, HashSet};

const SUGGESTED_PROFILE_DEFAULT_LIMIT: usize = 40;
const SUGGESTED_PROFILE_INCREMENT: usize = 40;
const SUGGESTED_PROFILE_MAX_LIMIT: usize = 200;

#[derive(Template)]
#[template(path = "suggested_profiles.html")]
struct SuggestedProfilesTemplate {
	prefs: Preferences,
	url: String,
	redirect_url: String,
	report: SuggestedProfilesReport,
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
		format!("/suggested-profiles?limit={}", self.next_suggestion_limit)
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

fn redirect_url_for_limit(limit: usize) -> String {
	if limit <= SUGGESTED_PROFILE_DEFAULT_LIMIT {
		"suggested-profiles".to_string()
	} else {
		format!("suggested-profiles%3Flimit%3D{limit}")
	}
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let filters = get_filters(&req);
	let followed = followed_profiles(&prefs);
	let followed_subreddits = followed_profile_subreddit_cache(&followed);
	let excluded = filtered_profiles(&filters);
	let suggestion_limit = requested_suggestion_limit(&req);
	let fetch_limit = if suggestion_limit < SUGGESTED_PROFILE_MAX_LIMIT {
		suggestion_limit + 1
	} else {
		suggestion_limit
	};
	let mut report = crate::materialized_catalog::suggested_profiles(&followed, &followed_subreddits, &excluded, fetch_limit);
	let has_more_suggestions = report.suggestions.len() > suggestion_limit && suggestion_limit < SUGGESTED_PROFILE_MAX_LIMIT;
	report.suggestions.truncate(suggestion_limit);

	Ok(template(&SuggestedProfilesTemplate {
		prefs,
		url: "/suggested-profiles".to_string(),
		redirect_url: redirect_url_for_limit(suggestion_limit),
		report,
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
}
