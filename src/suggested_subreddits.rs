use crate::materialized_catalog::{SuggestedProfileEntry, SuggestedProfileScoringMode, SuggestedProfilesReport};
use crate::utils::{
	get_filters, param, post_feedback_score_adjustment, post_feedback_summary_affinity, read_sharded_cookie, template, PostFeedbackSummary, Preferences,
	POST_FEEDBACK_MODEL_MANUAL,
};
use askama::Template;
use hyper::{Body, Request, Response};
use std::collections::{HashMap, HashSet};

const SUGGESTED_SUBREDDIT_DEFAULT_LIMIT: usize = 40;
const SUGGESTED_SUBREDDIT_INCREMENT: usize = 40;
const SUGGESTED_SUBREDDIT_MAX_LIMIT: usize = 200;
const SUGGESTED_SUBREDDIT_PROFILE_SAMPLE_LIMIT: usize = 120;
const SUGGESTED_SUBREDDIT_PROFILE_SUBREDDIT_LIMIT: usize = 32;
const SUGGESTED_SUBREDDIT_SOURCE_LIMIT: usize = 8;
const SUGGESTED_SUBREDDIT_FEEDBACK_MAX_SCORE_ADJUSTMENT: f64 = 18.0;
const SUGGESTED_SUBREDDIT_IGNORE_COOKIE: &str = "suggested_subreddit_ignores";
const SUGGESTED_PROFILE_IGNORE_COOKIE: &str = "suggested_profile_ignores";
const SUGGESTED_IGNORE_TTL_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq)]
struct SuggestedSubredditSourceProfile {
	name: String,
	post_count: usize,
	from_suggested_profile: bool,
	match_score_percent: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
struct SuggestedSubredditEntry {
	name: String,
	score: u32,
	followed_post_count: usize,
	followed_profile_count: usize,
	suggested_post_count: usize,
	suggested_profile_count: usize,
	total_post_count: usize,
	source_profile_count: usize,
	source_profiles: Vec<SuggestedSubredditSourceProfile>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct SuggestedSubredditsReport {
	followed_profile_count: usize,
	followed_profiles_with_cache: usize,
	suggested_profile_count: usize,
	suggested_profiles_with_cache: usize,
	candidate_subreddit_count: usize,
	suggestions: Vec<SuggestedSubredditEntry>,
}

#[derive(Default)]
struct SuggestedSubredditAccumulator {
	name: String,
	followed_posts: usize,
	suggested_posts: usize,
	followed_profiles: HashSet<String>,
	suggested_profiles: HashSet<String>,
	source_profiles: HashMap<String, SuggestedSubredditSourceProfile>,
}

#[derive(Template)]
#[template(path = "suggested_subreddits.html")]
struct SuggestedSubredditsTemplate {
	prefs: Preferences,
	url: String,
	redirect_url: String,
	report: SuggestedSubredditsReport,
	scoring_mode: SuggestedProfileScoringMode,
	suggestion_limit: usize,
	next_suggestion_limit: usize,
	has_more_suggestions: bool,
}

crate::impl_template_str_eq!(SuggestedSubredditsTemplate);

impl SuggestedSubredditsTemplate {
	fn subreddit_path(&self, name: &str) -> String {
		format!("/r/{name}")
	}

	fn subscribe_path(&self, name: &str) -> String {
		format!("/r/{name}/subscribe?redirect={}&canonical={name}", self.redirect_url)
	}

	fn unsubscribe_path(&self, name: &str) -> String {
		format!("/r/{name}/unsubscribe?redirect={}&canonical={name}", self.redirect_url)
	}

	fn load_more_path(&self) -> String {
		page_path_for_limit(self.next_suggestion_limit, self.scoring_mode)
	}

	fn overlap_mode_path(&self) -> String {
		page_path_for_limit(SUGGESTED_SUBREDDIT_DEFAULT_LIMIT, SuggestedProfileScoringMode::Overlap)
	}

	fn vector_mode_path(&self) -> String {
		page_path_for_limit(SUGGESTED_SUBREDDIT_DEFAULT_LIMIT, SuggestedProfileScoringMode::Vector)
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

fn normalize_profile_name(name: &str) -> String {
	let trimmed = name.trim();
	let without_prefix = trimmed.strip_prefix("u_").or_else(|| trimmed.strip_prefix("u/")).unwrap_or(trimmed);
	without_prefix.trim().to_lowercase()
}

fn normalize_subreddit_name(name: &str) -> String {
	let trimmed = name.trim();
	let without_prefix = trimmed.strip_prefix("r/").unwrap_or(trimmed);
	without_prefix.trim().to_lowercase()
}

fn followed_profiles(prefs: &Preferences) -> Vec<String> {
	let mut seen = HashSet::new();
	let mut profiles = Vec::new();
	for profile in prefs.subscriptions.iter().filter_map(|subscription| subscription.strip_prefix("u_")) {
		let normalized = normalize_profile_name(profile);
		if normalized.is_empty() || !seen.insert(normalized) {
			continue;
		}
		profiles.push(profile.trim().to_string());
	}
	profiles.sort_by_key(|profile| normalize_profile_name(profile));
	profiles
}

fn followed_subreddits(prefs: &Preferences) -> HashSet<String> {
	prefs
		.subscriptions
		.iter()
		.filter(|subscription| !subscription.starts_with("u_"))
		.map(|subscription| normalize_subreddit_name(subscription))
		.filter(|subreddit| !subreddit.is_empty())
		.collect()
}

fn filtered_subreddits(filters: &HashSet<String>) -> HashSet<String> {
	filters
		.iter()
		.filter(|filter| !filter.starts_with("u_"))
		.map(|filter| normalize_subreddit_name(filter))
		.filter(|subreddit| !subreddit.is_empty())
		.collect()
}

fn filtered_profiles(filters: &HashSet<String>) -> HashSet<String> {
	filters.iter().filter_map(|filter| filter.strip_prefix("u_").map(normalize_profile_name)).collect()
}

fn expiring_name_cookie(req: &Request<Body>, cookie_name: &str, now_unix: u64) -> HashSet<String> {
	read_sharded_cookie(req, cookie_name)
		.split('+')
		.filter_map(|entry| {
			let (name, ignored_at) = entry.split_once(':')?;
			let ignored_at = ignored_at.parse::<u64>().ok()?;
			let name = name.trim().to_lowercase();
			(!name.is_empty() && now_unix.saturating_sub(ignored_at) <= SUGGESTED_IGNORE_TTL_SECS).then_some(name)
		})
		.collect()
}

fn requested_suggestion_limit(req: &Request<Body>) -> usize {
	let query = format!("?{}", req.uri().query().unwrap_or_default());
	param(&query, "limit")
		.and_then(|limit| limit.parse::<usize>().ok())
		.filter(|limit| *limit > 0)
		.map(|limit| limit.clamp(SUGGESTED_SUBREDDIT_DEFAULT_LIMIT, SUGGESTED_SUBREDDIT_MAX_LIMIT))
		.unwrap_or(SUGGESTED_SUBREDDIT_DEFAULT_LIMIT)
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
	if limit > SUGGESTED_SUBREDDIT_DEFAULT_LIMIT {
		params.push(format!("limit={limit}"));
	}
	if params.is_empty() {
		"/suggested-subreddits".to_string()
	} else {
		format!("/suggested-subreddits?{}", params.join("&"))
	}
}

fn redirect_url_for_limit(limit: usize, mode: SuggestedProfileScoringMode) -> String {
	let path = page_path_for_limit(limit, mode);
	path.trim_start_matches('/').replace('?', "%3F").replace('=', "%3D").replace('&', "%26")
}

fn insert_count(counts: &mut HashMap<String, u32>, subreddit: String, count: u32) {
	if subreddit.is_empty() || subreddit.starts_with("u_") || count == 0 {
		return;
	}
	let entry = counts.entry(subreddit).or_default();
	*entry = (*entry).max(count);
}

fn sorted_profile_subreddit_counts(mut counts: HashMap<String, u32>) -> Vec<(String, u32)> {
	let mut subreddits = counts.drain().collect::<Vec<_>>();
	subreddits.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
	subreddits.truncate(SUGGESTED_SUBREDDIT_PROFILE_SUBREDDIT_LIMIT);
	subreddits
}

fn manual_post_feedback_summary<'a>(feedback: &'a HashMap<(String, String), PostFeedbackSummary>, name: &str) -> Option<&'a PostFeedbackSummary> {
	feedback.get(&(POST_FEEDBACK_MODEL_MANUAL.to_string(), normalize_subreddit_name(name)))
}

fn has_positive_post_feedback(summary: &PostFeedbackSummary) -> bool {
	post_feedback_summary_affinity(Some(summary)) > 0.0
}

fn profile_subreddit_counts(profile: &str) -> Vec<(String, u32)> {
	let normalized = normalize_profile_name(profile);
	if normalized.is_empty() {
		return Vec::new();
	}

	let mut counts = HashMap::new();
	if let Some(stats) = crate::materialized_catalog::catalog_user_sidebar_stats(&normalized) {
		for (subreddit, count) in stats.subreddit_counts {
			insert_count(&mut counts, normalize_subreddit_name(&subreddit), count);
		}
	}

	for lookup in [profile.trim(), normalized.as_str()] {
		for (subreddit, count) in crate::utils::cached_user_subreddits(lookup) {
			insert_count(&mut counts, normalize_subreddit_name(&subreddit), count);
		}
	}

	sorted_profile_subreddit_counts(counts)
}

fn profile_subreddit_counts_for_followed(followed: &[String]) -> HashMap<String, Vec<(String, u32)>> {
	followed
		.iter()
		.map(|profile| (normalize_profile_name(profile), profile_subreddit_counts(profile)))
		.collect()
}

fn profile_subreddit_counts_for_suggestions(suggestions: &[SuggestedProfileEntry]) -> HashMap<String, Vec<(String, u32)>> {
	let mut counts_by_profile = HashMap::new();
	for suggestion in suggestions {
		let mut counts = profile_subreddit_counts(&suggestion.name).into_iter().collect::<HashMap<_, _>>();
		for subreddit in &suggestion.overlap_subreddits {
			insert_count(
				&mut counts,
				normalize_subreddit_name(&subreddit.name),
				subreddit.profile_count.min(u32::MAX as usize) as u32,
			);
		}
		counts_by_profile.insert(normalize_profile_name(&suggestion.name), sorted_profile_subreddit_counts(counts));
	}
	counts_by_profile
}

fn add_profile_signal(
	accumulators: &mut HashMap<String, SuggestedSubredditAccumulator>,
	profile: &str,
	counts: &[(String, u32)],
	from_suggested_profile: bool,
	match_score_percent: Option<u32>,
	excluded_subreddits: &HashSet<String>,
) -> bool {
	let profile = normalize_profile_name(profile);
	if profile.is_empty() {
		return false;
	}

	let mut added_any = false;
	for (subreddit, count) in counts {
		let subreddit = normalize_subreddit_name(subreddit);
		if subreddit.is_empty() || subreddit.starts_with("u_") || excluded_subreddits.contains(&subreddit) || *count == 0 {
			continue;
		}

		added_any = true;
		let count = *count as usize;
		let accumulator = accumulators.entry(subreddit.clone()).or_insert_with(|| SuggestedSubredditAccumulator {
			name: subreddit.clone(),
			..SuggestedSubredditAccumulator::default()
		});
		let source = accumulator.source_profiles.entry(profile.clone()).or_insert_with(|| SuggestedSubredditSourceProfile {
			name: profile.clone(),
			post_count: 0,
			from_suggested_profile,
			match_score_percent,
		});
		source.post_count = source.post_count.max(count);
		source.from_suggested_profile = source.from_suggested_profile && from_suggested_profile;
		source.match_score_percent = source.match_score_percent.or(match_score_percent);

		if from_suggested_profile {
			accumulator.suggested_profiles.insert(profile.clone());
			accumulator.suggested_posts += count;
		} else {
			accumulator.followed_profiles.insert(profile.clone());
			accumulator.followed_posts += count;
		}
	}

	added_any
}

fn entry_score(accumulator: &SuggestedSubredditAccumulator, subreddit_feedback: Option<&PostFeedbackSummary>) -> u32 {
	let followed_breadth = accumulator.followed_profiles.len() as f64;
	let suggested_breadth = accumulator.suggested_profiles.len() as f64;
	let followed_volume = (accumulator.followed_posts as f64).sqrt();
	let suggested_volume = (accumulator.suggested_posts as f64).sqrt();

	(followed_breadth * 28.0
		+ followed_volume * 9.0
		+ suggested_breadth * 10.0
		+ suggested_volume * 4.0
		+ post_feedback_score_adjustment(subreddit_feedback, SUGGESTED_SUBREDDIT_FEEDBACK_MAX_SCORE_ADJUSTMENT))
	.round()
	.clamp(1.0, u32::MAX as f64) as u32
}

fn report_from_profile_counts(
	followed: &[String],
	followed_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	profile_suggestions: &[SuggestedProfileEntry],
	suggested_profile_subreddits: &HashMap<String, Vec<(String, u32)>>,
	followed_subreddits: &HashSet<String>,
	filtered_subreddits: &HashSet<String>,
	ignored_subreddits: &HashSet<String>,
	subreddit_feedback: &HashMap<(String, String), PostFeedbackSummary>,
	limit: usize,
) -> SuggestedSubredditsReport {
	let mut excluded_subreddits = followed_subreddits.clone();
	excluded_subreddits.extend(filtered_subreddits.iter().cloned());
	excluded_subreddits.extend(ignored_subreddits.iter().cloned());

	let mut accumulators = HashMap::new();
	let mut followed_profiles_with_cache = 0usize;
	for profile in followed {
		let normalized_profile = normalize_profile_name(profile);
		let empty = Vec::new();
		let counts = followed_profile_subreddits.get(&normalized_profile).unwrap_or(&empty);
		if add_profile_signal(&mut accumulators, profile, counts, false, None, &excluded_subreddits) {
			followed_profiles_with_cache += 1;
		}
	}

	let mut suggested_profiles_with_cache = 0usize;
	for suggestion in profile_suggestions {
		let profile = normalize_profile_name(&suggestion.name);
		let empty = Vec::new();
		let counts = suggested_profile_subreddits.get(&profile).unwrap_or(&empty);
		if add_profile_signal(&mut accumulators, &profile, counts, true, Some(suggestion.score_percent), &excluded_subreddits) {
			suggested_profiles_with_cache += 1;
		}
	}
	for ((model, subreddit), summary) in subreddit_feedback {
		if model != POST_FEEDBACK_MODEL_MANUAL || !has_positive_post_feedback(summary) {
			continue;
		}
		let subreddit = normalize_subreddit_name(subreddit);
		if subreddit.is_empty() || subreddit.starts_with("u_") || excluded_subreddits.contains(&subreddit) {
			continue;
		}
		accumulators.entry(subreddit.clone()).or_insert_with(|| SuggestedSubredditAccumulator {
			name: subreddit,
			..SuggestedSubredditAccumulator::default()
		});
	}

	let candidate_subreddit_count = accumulators.len();
	let mut suggestions = accumulators
		.into_values()
		.map(|accumulator| {
			let followed_profile_count = accumulator.followed_profiles.len();
			let suggested_profile_count = accumulator.suggested_profiles.len();
			let score = entry_score(&accumulator, manual_post_feedback_summary(subreddit_feedback, &accumulator.name));
			let total_post_count = accumulator.followed_posts + accumulator.suggested_posts;
			let mut source_profiles = accumulator.source_profiles.into_values().collect::<Vec<_>>();
			source_profiles.sort_by(|left, right| {
				left
					.from_suggested_profile
					.cmp(&right.from_suggested_profile)
					.then_with(|| right.post_count.cmp(&left.post_count))
					.then_with(|| right.match_score_percent.unwrap_or_default().cmp(&left.match_score_percent.unwrap_or_default()))
					.then_with(|| left.name.cmp(&right.name))
			});
			source_profiles.truncate(SUGGESTED_SUBREDDIT_SOURCE_LIMIT);

			SuggestedSubredditEntry {
				name: accumulator.name,
				score,
				followed_post_count: accumulator.followed_posts,
				followed_profile_count,
				suggested_post_count: accumulator.suggested_posts,
				suggested_profile_count,
				total_post_count,
				source_profile_count: followed_profile_count + suggested_profile_count,
				source_profiles,
			}
		})
		.collect::<Vec<_>>();

	suggestions.sort_by(|left, right| {
		right
			.score
			.cmp(&left.score)
			.then_with(|| right.followed_profile_count.cmp(&left.followed_profile_count))
			.then_with(|| right.followed_post_count.cmp(&left.followed_post_count))
			.then_with(|| right.suggested_profile_count.cmp(&left.suggested_profile_count))
			.then_with(|| right.suggested_post_count.cmp(&left.suggested_post_count))
			.then_with(|| left.name.cmp(&right.name))
	});
	suggestions.truncate(limit);

	SuggestedSubredditsReport {
		followed_profile_count: followed.len(),
		followed_profiles_with_cache,
		suggested_profile_count: profile_suggestions.len(),
		suggested_profiles_with_cache,
		candidate_subreddit_count,
		suggestions,
	}
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let filters = get_filters(&req);
	let followed = followed_profiles(&prefs);
	let followed_profile_subreddits = profile_subreddit_counts_for_followed(&followed);
	let now_unix = crate::cache_persistence::unix_now();
	let ignored_profile_names = expiring_name_cookie(&req, SUGGESTED_PROFILE_IGNORE_COOKIE, now_unix);
	let ignored_subreddit_names = expiring_name_cookie(&req, SUGGESTED_SUBREDDIT_IGNORE_COOKIE, now_unix);
	let mut excluded_profiles = filtered_profiles(&filters);
	excluded_profiles.extend(ignored_profile_names);
	let suggestion_limit = requested_suggestion_limit(&req);
	let scoring_mode = requested_scoring_mode(&req);
	let fetch_limit = if suggestion_limit < SUGGESTED_SUBREDDIT_MAX_LIMIT {
		suggestion_limit + 1
	} else {
		suggestion_limit
	};
	let author_feedback = prefs.post_feedback_by_model_and_author();
	let subreddit_feedback = prefs.post_feedback_by_model_and_subreddit();
	let profile_report: SuggestedProfilesReport = crate::materialized_catalog::suggested_profiles_with_mode_and_feedback(
		&followed,
		&followed_profile_subreddits,
		&excluded_profiles,
		&author_feedback,
		SUGGESTED_SUBREDDIT_PROFILE_SAMPLE_LIMIT,
		scoring_mode,
	);
	let suggested_profile_subreddits = profile_subreddit_counts_for_suggestions(&profile_report.suggestions);
	let mut report = report_from_profile_counts(
		&followed,
		&followed_profile_subreddits,
		&profile_report.suggestions,
		&suggested_profile_subreddits,
		&followed_subreddits(&prefs),
		&filtered_subreddits(&filters),
		&ignored_subreddit_names,
		&subreddit_feedback,
		fetch_limit,
	);
	let has_more_suggestions = report.suggestions.len() > suggestion_limit && suggestion_limit < SUGGESTED_SUBREDDIT_MAX_LIMIT;
	report.suggestions.truncate(suggestion_limit);

	Ok(template(&SuggestedSubredditsTemplate {
		prefs,
		url: "/suggested-subreddits".to_string(),
		redirect_url: redirect_url_for_limit(suggestion_limit, scoring_mode),
		report,
		scoring_mode,
		suggestion_limit,
		next_suggestion_limit: (suggestion_limit + SUGGESTED_SUBREDDIT_INCREMENT).min(SUGGESTED_SUBREDDIT_MAX_LIMIT),
		has_more_suggestions,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn request_for(uri: &str) -> Request<Body> {
		Request::builder().uri(uri).body(Body::empty()).expect("valid test request")
	}

	fn suggestion(name: &str, score_percent: u32) -> SuggestedProfileEntry {
		SuggestedProfileEntry {
			name: name.to_string(),
			score_percent,
			cached_posts: 0,
			matched_posts: 0,
			avg_video_length: None,
			video_count: 0,
			redgifs_profile_count: 0,
			media_posts: Vec::new(),
			overlap_subreddits: Vec::new(),
		}
	}

	#[test]
	fn requested_suggestion_limit_defaults_and_clamps() {
		assert_eq!(requested_suggestion_limit(&request_for("/suggested-subreddits")), SUGGESTED_SUBREDDIT_DEFAULT_LIMIT);
		assert_eq!(requested_suggestion_limit(&request_for("/suggested-subreddits?limit=80")), 80);
		assert_eq!(requested_suggestion_limit(&request_for("/suggested-subreddits?limit=3")), SUGGESTED_SUBREDDIT_DEFAULT_LIMIT);
		assert_eq!(requested_suggestion_limit(&request_for("/suggested-subreddits?limit=999")), SUGGESTED_SUBREDDIT_MAX_LIMIT);
	}

	#[test]
	fn requested_scoring_mode_accepts_vector_only() {
		assert_eq!(requested_scoring_mode(&request_for("/suggested-subreddits")), SuggestedProfileScoringMode::Overlap);
		assert_eq!(
			requested_scoring_mode(&request_for("/suggested-subreddits?mode=unknown")),
			SuggestedProfileScoringMode::Overlap
		);
		assert_eq!(
			requested_scoring_mode(&request_for("/suggested-subreddits?mode=vector")),
			SuggestedProfileScoringMode::Vector
		);
	}

	#[test]
	fn ignored_subreddits_keep_entries_for_thirty_days() {
		let now = 1_700_000_000;
		let recent = now - SUGGESTED_IGNORE_TTL_SECS;
		let expired = recent - 1;
		let req = Request::builder()
			.uri("/suggested-subreddits")
			.header("cookie", format!("suggested_subreddit_ignores=Rust:{recent}+Old:{expired}+bad+Nope:not-a-time"))
			.body(Body::empty())
			.expect("valid test request");

		let ignored = expiring_name_cookie(&req, SUGGESTED_SUBREDDIT_IGNORE_COOKIE, now);

		assert!(ignored.contains("rust"));
		assert!(!ignored.contains("old"));
		assert!(!ignored.contains("nope"));
	}

	#[test]
	fn page_paths_preserve_vector_mode_for_load_more() {
		assert_eq!(
			page_path_for_limit(SUGGESTED_SUBREDDIT_DEFAULT_LIMIT, SuggestedProfileScoringMode::Overlap),
			"/suggested-subreddits"
		);
		assert_eq!(page_path_for_limit(80, SuggestedProfileScoringMode::Overlap), "/suggested-subreddits?limit=80");
		assert_eq!(
			page_path_for_limit(SUGGESTED_SUBREDDIT_DEFAULT_LIMIT, SuggestedProfileScoringMode::Vector),
			"/suggested-subreddits?mode=vector"
		);
		assert_eq!(page_path_for_limit(80, SuggestedProfileScoringMode::Vector), "/suggested-subreddits?mode=vector&limit=80");
	}

	#[test]
	fn report_excludes_followed_filtered_and_ignored_subreddits() {
		let followed = vec!["alice".to_string()];
		let followed_counts = HashMap::from([(
			"alice".to_string(),
			vec![("rust".to_string(), 5), ("already".to_string(), 4), ("filtered".to_string(), 3), ("ignored".to_string(), 2)],
		)]);
		let report = report_from_profile_counts(
			&followed,
			&followed_counts,
			&[],
			&HashMap::new(),
			&HashSet::from(["already".to_string()]),
			&HashSet::from(["filtered".to_string()]),
			&HashSet::from(["ignored".to_string()]),
			&HashMap::new(),
			10,
		);

		assert_eq!(report.suggestions.len(), 1);
		assert_eq!(report.suggestions[0].name, "rust");
	}

	#[test]
	fn report_ranks_followed_profile_activity_above_suggested_only_activity() {
		let followed = vec!["alice".to_string(), "carol".to_string()];
		let followed_counts = HashMap::from([("alice".to_string(), vec![("rust".to_string(), 3)]), ("carol".to_string(), vec![("rust".to_string(), 2)])]);
		let profile_suggestions = vec![suggestion("bob", 90), suggestion("dave", 80)];
		let suggested_counts = HashMap::from([("bob".to_string(), vec![("noisy".to_string(), 30)]), ("dave".to_string(), vec![("noisy".to_string(), 20)])]);

		let report = report_from_profile_counts(
			&followed,
			&followed_counts,
			&profile_suggestions,
			&suggested_counts,
			&HashSet::new(),
			&HashSet::new(),
			&HashSet::new(),
			&HashMap::new(),
			10,
		);

		assert_eq!(report.suggestions[0].name, "rust");
		assert_eq!(report.suggestions[0].followed_profile_count, 2);
		assert_eq!(report.suggestions[1].name, "noisy");
		assert_eq!(report.suggestions[1].suggested_profile_count, 2);
	}

	#[test]
	fn report_uses_post_feedback_to_nudge_subreddit_rank() {
		let followed = vec!["alice".to_string()];
		let followed_counts = HashMap::from([("alice".to_string(), vec![("alpha".to_string(), 3), ("beta".to_string(), 3)])]);

		let without_feedback = report_from_profile_counts(
			&followed,
			&followed_counts,
			&[],
			&HashMap::new(),
			&HashSet::new(),
			&HashSet::new(),
			&HashSet::new(),
			&HashMap::new(),
			10,
		);
		assert_eq!(without_feedback.suggestions[0].name, "alpha");

		let feedback = HashMap::from([((POST_FEEDBACK_MODEL_MANUAL.to_string(), "beta".to_string()), PostFeedbackSummary { likes: 4, dislikes: 0 })]);
		let with_feedback = report_from_profile_counts(
			&followed,
			&followed_counts,
			&[],
			&HashMap::new(),
			&HashSet::new(),
			&HashSet::new(),
			&HashSet::new(),
			&feedback,
			10,
		);

		assert_eq!(with_feedback.suggestions[0].name, "beta");
		assert!(with_feedback.suggestions[0].score > without_feedback.suggestions[0].score);
	}

	#[test]
	fn report_includes_positive_feedback_subreddit_candidates() {
		let followed = vec!["alice".to_string()];
		let followed_counts = HashMap::from([("alice".to_string(), vec![("rust".to_string(), 3)])]);
		let feedback = HashMap::from([(
			(POST_FEEDBACK_MODEL_MANUAL.to_string(), "likedonly".to_string()),
			PostFeedbackSummary { likes: 4, dislikes: 0 },
		)]);

		let report = report_from_profile_counts(
			&followed,
			&followed_counts,
			&[],
			&HashMap::new(),
			&HashSet::new(),
			&HashSet::new(),
			&HashSet::new(),
			&feedback,
			10,
		);

		let liked = report
			.suggestions
			.iter()
			.find(|suggestion| suggestion.name == "likedonly")
			.expect("liked subreddit suggestion");
		assert_eq!(liked.total_post_count, 0);
		assert_eq!(liked.score, SUGGESTED_SUBREDDIT_FEEDBACK_MAX_SCORE_ADJUSTMENT as u32);
	}
}
