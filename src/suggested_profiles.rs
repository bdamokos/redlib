use crate::materialized_catalog::SuggestedProfilesReport;
use crate::utils::{get_filters, template, Preferences};
use askama::Template;
use hyper::{Body, Request, Response};
use std::collections::{HashMap, HashSet};

const SUGGESTED_PROFILE_LIMIT: usize = 40;

#[derive(Template)]
#[template(path = "suggested_profiles.html")]
struct SuggestedProfilesTemplate {
	prefs: Preferences,
	url: String,
	redirect_url: String,
	report: SuggestedProfilesReport,
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

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let filters = get_filters(&req);
	let followed = followed_profiles(&prefs);
	let followed_subreddits = followed_profile_subreddit_cache(&followed);
	let excluded = filtered_profiles(&filters);
	let report = crate::materialized_catalog::suggested_profiles(&followed, &followed_subreddits, &excluded, SUGGESTED_PROFILE_LIMIT);

	Ok(template(&SuggestedProfilesTemplate {
		prefs,
		url: "/suggested-profiles".to_string(),
		redirect_url: "suggested-profiles".to_string(),
		report,
	}))
}
