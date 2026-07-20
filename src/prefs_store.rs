use crate::config;
use crate::server::{RequestExt, ResponseExt};
use crate::utils::Preferences;
use cookie::{Cookie, SameSite};
use hyper::{Body, Request, Response};
use log::warn;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const PREFS_COOKIE: &str = "rl_prefs_id";
const RECORD_VERSION: u8 = 1;
const DEFAULT_PREFS_DB: &str = "/var/lib/redlib/state/preferences.sqlite3";
const DEFAULT_MAX_AGE_DAYS: u64 = 365;
const DEFAULT_TOUCH_INTERVAL_SECS: u64 = 3600;
const EVICTION_INTERVAL_SECS: u64 = 3600;
const MAX_COOKIE_SHARDS: usize = 16;
const LEGACY_ADOPTION_SCAN_LIMIT: usize = 200;
const LEGACY_ADOPTION_LOOKBACK_SECS: u64 = 30 * 24 * 60 * 60;
const LEGACY_ADOPTION_MAX_LEGACY_FEED_ITEMS: usize = 64;
const LEGACY_ADOPTION_MIN_CANDIDATE_FEED_ITEMS: usize = 64;
const LEGACY_ADOPTION_MIN_EXTRA_FEED_ITEMS: usize = 25;

const LEGACY_SIMPLE_COOKIES: &[&str] = &[
	"theme",
	"front_page",
	"layout",
	"wide",
	"comment_sort",
	"post_sort",
	"blur_spoiler",
	"show_nsfw",
	"blur_nsfw",
	"use_dash",
	"use_hls",
	"hide_hls_notification",
	"autoplay_videos",
	"video_previews",
	"direct_redgifs_media",
	"redgifs_poster_quality",
	"hide_sidebar_and_summary",
	"fixed_navbar",
	"hide_awards",
	"hide_score",
	"disable_visit_reddit_confirmation",
	"image_quality",
	"video_quality",
	"remove_default_feeds",
];

const LEGACY_SHARDED_COOKIES: &[&str] = &["subscriptions", "filters", "flair_subscriptions"];

#[derive(Clone)]
pub(crate) struct RequestPreferences {
	pub id: String,
	pub prefs: Preferences,
}

#[derive(Clone, Default)]
pub(crate) struct PreferenceCookieActions {
	set_session_id: Option<String>,
	clear_session_id: bool,
	legacy_cookie_names: Vec<String>,
}

#[derive(Clone, Debug)]
struct PreferenceRecord {
	last_visit_unix: u64,
	prefs: Preferences,
}

#[derive(Default)]
struct StoreRuntime {
	last_eviction_unix: u64,
}

static STORE: LazyLock<Mutex<StoreRuntime>> = LazyLock::new(|| Mutex::new(StoreRuntime::default()));

pub(crate) fn request_preferences(req: &Request<Body>) -> Option<Preferences> {
	req.extensions().get::<RequestPreferences>().map(|state| state.prefs.clone())
}

pub(crate) fn stored_setting(req: &Request<Body>, name: &str) -> Option<String> {
	req.extensions().get::<RequestPreferences>().and_then(|state| state.prefs.setting_value(name))
}

pub(crate) fn prepare_request(req: &mut Request<Body>) -> PreferenceCookieActions {
	let mut actions = PreferenceCookieActions::default();
	let cookie_id = session_cookie_id(req);
	let legacy_present = has_legacy_preferences(req);

	if let Some(cookie_id) = cookie_id {
		let id = trusted_profile_canonical_id(&cookie_id);
		if id != cookie_id {
			actions.set_session_id = Some(id.clone());
		}
		match load_record(&id) {
			Ok(Some(mut record)) => {
				touch_record(&id, &mut record);
				let mut prefs = record.prefs;
				prefs.prepare_for_runtime();
				req.extensions_mut().insert(RequestPreferences { id, prefs });
				if legacy_present {
					actions.legacy_cookie_names = legacy_preference_cookie_names(req);
				}
				return actions;
			}
			Ok(None) if legacy_present => {
				if let Some((id, prefs)) = prepare_legacy_preferences_record(req, Some(id)) {
					req.extensions_mut().insert(RequestPreferences { id: id.clone(), prefs });
					actions.set_session_id = Some(id);
					actions.legacy_cookie_names = legacy_preference_cookie_names(req);
				}
				return actions;
			}
			Ok(None) => {
				actions.clear_session_id = true;
				return actions;
			}
			Err(error) => {
				warn!("preference store: failed to load record {id}: {error}");
				if legacy_present {
					if let Some((id, prefs)) = prepare_legacy_preferences_record(req, Some(id)) {
						req.extensions_mut().insert(RequestPreferences { id: id.clone(), prefs });
						actions.set_session_id = Some(id);
						actions.legacy_cookie_names = legacy_preference_cookie_names(req);
					}
				} else {
					actions.clear_session_id = true;
				}
				return actions;
			}
		}
	}

	if legacy_present {
		if let Some((id, prefs)) = prepare_legacy_preferences_record(req, None) {
			req.extensions_mut().insert(RequestPreferences { id: id.clone(), prefs });
			actions.set_session_id = Some(id);
			actions.legacy_cookie_names = legacy_preference_cookie_names(req);
		}
	}

	actions
}

pub(crate) fn save_preferences(response: &mut Response<Body>, req: &Request<Body>, prefs: &Preferences) -> Result<(), String> {
	let id = request_session_id(req).unwrap_or_else(new_session_id);
	save_record(&id, prefs)?;
	response.insert_cookie(session_cookie(&id));
	let legacy_cookie_names = legacy_preference_cookie_names(req);
	clear_legacy_preference_cookies(response, &legacy_cookie_names);
	Ok(())
}

impl PreferenceCookieActions {
	pub(crate) fn apply(&self, response: &mut Response<Body>) {
		if let Some(id) = &self.set_session_id {
			response.insert_cookie(session_cookie(id));
		}
		if self.clear_session_id {
			response.remove_cookie(PREFS_COOKIE.to_string());
		}
		clear_legacy_preference_cookies(response, &self.legacy_cookie_names);
	}
}

fn session_cookie(id: &str) -> Cookie<'static> {
	Cookie::build((PREFS_COOKIE.to_string(), id.to_string()))
		.path("/")
		.http_only(true)
		.same_site(SameSite::Lax)
		.expires(OffsetDateTime::now_utc() + Duration::weeks(52))
		.into()
}

fn session_cookie_id(req: &Request<Body>) -> Option<String> {
	req.cookie(PREFS_COOKIE).map(|cookie| cookie.value().trim().to_string()).filter(|id| valid_session_id(id))
}

fn request_session_id(req: &Request<Body>) -> Option<String> {
	req
		.extensions()
		.get::<RequestPreferences>()
		.map(|state| state.id.clone())
		.or_else(|| session_cookie_id(req))
}

fn valid_session_id(id: &str) -> bool {
	Uuid::parse_str(id).is_ok()
}

fn new_session_id() -> String {
	Uuid::new_v4().to_string()
}

/// Resolve explicitly allowlisted browser profiles to one canonical row. The
/// first UUID in the private server setting is canonical; unrelated profile
/// IDs retain the normal per-browser isolation behavior.
fn trusted_profile_canonical_id(id: &str) -> String {
	let configured = config::get_setting("REDLIB_TRUSTED_PREFS_PROFILE_IDS").unwrap_or_default();
	trusted_profile_canonical_id_from(id, &configured).unwrap_or_else(|| id.to_string())
}

fn trusted_profile_canonical_id_from(id: &str, configured: &str) -> Option<String> {
	let mut ids = configured.split([',', '+']).map(str::trim).filter(|candidate| valid_session_id(candidate));
	let canonical = ids.next()?;
	if canonical == id || ids.any(|candidate| candidate == id) {
		Some(canonical.to_string())
	} else {
		None
	}
}

fn now_unix() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn config_u64(name: &str, default: u64) -> u64 {
	config::get_setting(name)
		.and_then(|value| value.trim().parse::<u64>().ok())
		.filter(|value| *value > 0)
		.unwrap_or(default)
}

fn prefs_db_path() -> PathBuf {
	config::get_setting("REDLIB_PREFS_DB")
		.filter(|value| !value.trim().is_empty())
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from(DEFAULT_PREFS_DB))
}

fn open_db() -> Result<Connection, String> {
	let path = prefs_db_path();
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let conn = Connection::open(path).map_err(|error| error.to_string())?;
	conn.busy_timeout(StdDuration::from_secs(5)).map_err(|error| error.to_string())?;
	conn.pragma_update(None, "journal_mode", "WAL").map_err(|error| error.to_string())?;
	conn.pragma_update(None, "synchronous", "NORMAL").map_err(|error| error.to_string())?;
	conn
		.execute_batch(
			"
		CREATE TABLE IF NOT EXISTS preferences (
			id TEXT PRIMARY KEY NOT NULL,
			version INTEGER NOT NULL,
			created_at_unix INTEGER NOT NULL,
			updated_at_unix INTEGER NOT NULL,
			last_visit_unix INTEGER NOT NULL,
			revision INTEGER NOT NULL,
			prefs_json TEXT NOT NULL
		);
		CREATE INDEX IF NOT EXISTS preferences_last_visit_idx
			ON preferences(last_visit_unix);
		",
		)
		.map_err(|error| error.to_string())?;
	Ok(conn)
}

fn load_record(id: &str) -> Result<Option<PreferenceRecord>, String> {
	if !valid_session_id(id) {
		return Ok(None);
	}

	let conn = open_db()?;
	let record = conn
		.query_row(
			"
			SELECT version, created_at_unix, updated_at_unix, last_visit_unix, revision, prefs_json
			FROM preferences
			WHERE id = ?1
			",
			params![id],
			|row| {
				let version: i64 = row.get(0)?;
				let created_at_unix: i64 = row.get(1)?;
				let updated_at_unix: i64 = row.get(2)?;
				let last_visit_unix: i64 = row.get(3)?;
				let revision: i64 = row.get(4)?;
				let prefs_json: String = row.get(5)?;
				Ok((version, created_at_unix, updated_at_unix, last_visit_unix, revision, prefs_json))
			},
		)
		.optional()
		.map_err(|error| error.to_string())?;

	let Some((version, _created_at_unix, _updated_at_unix, last_visit_unix, _revision, prefs_json)) = record else {
		return Ok(None);
	};

	if version != i64::from(RECORD_VERSION) {
		warn!("preference store: record {id} has version {version}, current version is {RECORD_VERSION}");
	}

	let mut prefs: Preferences = serde_json::from_str(&prefs_json).map_err(|error| error.to_string())?;
	prefs.prepare_for_runtime();
	Ok(Some(PreferenceRecord {
		last_visit_unix: db_to_u64(last_visit_unix),
		prefs,
	}))
}

fn prepare_legacy_preferences_record(req: &Request<Body>, preferred_id: Option<String>) -> Option<(String, Preferences)> {
	let legacy_prefs = Preferences::from_cookies(req);
	match find_recent_superset_record_for_legacy(req, &legacy_prefs, preferred_id.as_deref()) {
		Ok(Some((id, mut record))) => {
			touch_record(&id, &mut record);
			let mut prefs = record.prefs;
			prefs.prepare_for_runtime();
			return Some((id, prefs));
		}
		Ok(None) => {}
		Err(error) => warn!("preference store: failed to find legacy preference recovery candidate: {error}"),
	}

	let id = preferred_id.unwrap_or_else(new_session_id);
	match save_record(&id, &legacy_prefs) {
		Ok(()) => Some((id, legacy_prefs)),
		Err(error) => {
			warn!("preference store: failed to save legacy preference record {id}: {error}");
			None
		}
	}
}

fn save_record(id: &str, prefs: &Preferences) -> Result<(), String> {
	if !valid_session_id(id) {
		return Err("invalid preference session id".to_string());
	}

	let mut store = STORE.lock().map_err(|_| "preference store lock poisoned".to_string())?;
	let conn = open_db()?;
	let now = now_unix();
	maybe_evict_locked(&mut store, &conn, now);

	let prefs_json = serde_json::to_string(&prefs.prepare_for_storage()).map_err(|error| error.to_string())?;
	conn
		.execute(
			"
		INSERT INTO preferences (
			id, version, created_at_unix, updated_at_unix, last_visit_unix, revision, prefs_json
		)
		VALUES (?1, ?2, ?3, ?3, ?3, 1, ?4)
		ON CONFLICT(id) DO UPDATE SET
			version = excluded.version,
			updated_at_unix = excluded.updated_at_unix,
			last_visit_unix = excluded.last_visit_unix,
			revision = preferences.revision + 1,
			prefs_json = excluded.prefs_json
		",
			params![id, i64::from(RECORD_VERSION), u64_to_db(now), prefs_json],
		)
		.map_err(|error| error.to_string())?;
	Ok(())
}

fn find_recent_superset_record_for_legacy(req: &Request<Body>, legacy_prefs: &Preferences, exclude_id: Option<&str>) -> Result<Option<(String, PreferenceRecord)>, String> {
	if !legacy_preferences_look_partial(legacy_prefs) {
		return Ok(None);
	}

	let conn = open_db()?;
	let now = now_unix();
	let cutoff = now.saturating_sub(LEGACY_ADOPTION_LOOKBACK_SECS);
	let mut stmt = conn
		.prepare(
			"
			SELECT id, last_visit_unix, prefs_json
			FROM preferences
			WHERE last_visit_unix >= ?1
			ORDER BY last_visit_unix DESC
			LIMIT ?2
			",
		)
		.map_err(|error| error.to_string())?;
	let rows = stmt
		.query_map(params![u64_to_db(cutoff), i64::try_from(LEGACY_ADOPTION_SCAN_LIMIT).unwrap_or(i64::MAX)], |row| {
			let id: String = row.get(0)?;
			let last_visit_unix: i64 = row.get(1)?;
			let prefs_json: String = row.get(2)?;
			Ok((id, last_visit_unix, prefs_json))
		})
		.map_err(|error| error.to_string())?;

	let mut best: Option<(String, PreferenceRecord, usize, u64)> = None;
	for row in rows {
		let (id, last_visit_unix, prefs_json) = row.map_err(|error| error.to_string())?;
		if exclude_id.is_some_and(|excluded| excluded == id) {
			continue;
		}
		let mut prefs: Preferences = match serde_json::from_str(&prefs_json) {
			Ok(prefs) => prefs,
			Err(error) => {
				warn!("preference store: skipping unparsable legacy recovery candidate {id}: {error}");
				continue;
			}
		};
		prefs.prepare_for_runtime();
		if !candidate_can_adopt_legacy_preferences(req, legacy_prefs, &prefs) {
			continue;
		}

		let feed_items = preference_feed_item_count(&prefs);
		let last_visit = db_to_u64(last_visit_unix);
		let replace = best
			.as_ref()
			.is_none_or(|(_, _, best_items, best_last_visit)| (feed_items, last_visit) > (*best_items, *best_last_visit));
		if replace {
			best = Some((
				id,
				PreferenceRecord {
					last_visit_unix: last_visit,
					prefs,
				},
				feed_items,
				last_visit,
			));
		}
	}

	Ok(best.map(|(id, record, _, _)| (id, record)))
}

fn legacy_preferences_look_partial(legacy_prefs: &Preferences) -> bool {
	let feed_items = preference_feed_item_count(legacy_prefs);
	feed_items > 0 && feed_items <= LEGACY_ADOPTION_MAX_LEGACY_FEED_ITEMS
}

fn candidate_can_adopt_legacy_preferences(req: &Request<Body>, legacy_prefs: &Preferences, candidate: &Preferences) -> bool {
	let legacy_feed_items = preference_feed_item_count(legacy_prefs);
	let candidate_feed_items = preference_feed_item_count(candidate);
	candidate_feed_items >= LEGACY_ADOPTION_MIN_CANDIDATE_FEED_ITEMS
		&& candidate_feed_items >= legacy_feed_items.saturating_add(LEGACY_ADOPTION_MIN_EXTRA_FEED_ITEMS)
		&& legacy_feed_preferences_are_subset(legacy_prefs, candidate)
		&& legacy_scalar_cookie_values_match(req, candidate)
}

fn preference_feed_item_count(prefs: &Preferences) -> usize {
	prefs.subscriptions.len() + prefs.filters.len() + prefs.flair_subscriptions.len()
}

fn normalized_string_set(values: &[String]) -> HashSet<String> {
	values.iter().map(|value| value.trim().to_ascii_lowercase()).filter(|value| !value.is_empty()).collect()
}

fn normalized_flair_set(prefs: &Preferences) -> HashSet<(String, String)> {
	prefs
		.flair_subscriptions
		.iter()
		.map(|subscription| (subscription.subreddit.trim().to_ascii_lowercase(), subscription.flair.trim().to_ascii_lowercase()))
		.filter(|(subreddit, flair)| !subreddit.is_empty() && !flair.is_empty())
		.collect()
}

fn legacy_feed_preferences_are_subset(legacy_prefs: &Preferences, candidate: &Preferences) -> bool {
	let legacy_subscriptions = normalized_string_set(&legacy_prefs.subscriptions);
	let candidate_subscriptions = normalized_string_set(&candidate.subscriptions);
	let legacy_filters = normalized_string_set(&legacy_prefs.filters);
	let candidate_filters = normalized_string_set(&candidate.filters);
	let legacy_flairs = normalized_flair_set(legacy_prefs);
	let candidate_flairs = normalized_flair_set(candidate);

	legacy_subscriptions.is_subset(&candidate_subscriptions) && legacy_filters.is_subset(&candidate_filters) && legacy_flairs.is_subset(&candidate_flairs)
}

fn legacy_scalar_cookie_values_match(req: &Request<Body>, candidate: &Preferences) -> bool {
	let mut saw_scalar = false;
	for name in LEGACY_SIMPLE_COOKIES {
		let Some(cookie) = req.cookie(name) else {
			continue;
		};
		let value = cookie.value().trim();
		if value.is_empty() {
			continue;
		}
		saw_scalar = true;
		if candidate.setting_value(name).is_none_or(|candidate_value| candidate_value != value) {
			return false;
		}
	}

	if saw_scalar {
		return true;
	}

	let legacy_items = LEGACY_SHARDED_COOKIES.iter().map(|name| read_legacy_sharded_cookie_items(req, name)).sum::<usize>();
	legacy_items <= LEGACY_ADOPTION_MAX_LEGACY_FEED_ITEMS / 2
}

fn read_legacy_sharded_cookie_items(req: &Request<Body>, name: &str) -> usize {
	crate::utils::read_sharded_cookie(req, name)
		.split('+')
		.map(str::trim)
		.filter(|item| !item.is_empty())
		.count()
}

fn touch_record(id: &str, record: &mut PreferenceRecord) {
	let now = now_unix();
	let interval = config_u64("REDLIB_PREFS_TOUCH_INTERVAL_SECS", DEFAULT_TOUCH_INTERVAL_SECS);
	if now.saturating_sub(record.last_visit_unix) < interval {
		return;
	}

	let Ok(mut store) = STORE.lock() else {
		return;
	};
	let Ok(conn) = open_db() else {
		return;
	};
	maybe_evict_locked(&mut store, &conn, now);

	if let Err(error) = conn.execute("UPDATE preferences SET last_visit_unix = ?2 WHERE id = ?1", params![id, u64_to_db(now)]) {
		warn!("preference store: failed to touch record {id}: {error}");
		return;
	}
	record.last_visit_unix = now;
}

fn maybe_evict_locked(store: &mut StoreRuntime, conn: &Connection, now: u64) {
	if now.saturating_sub(store.last_eviction_unix) < EVICTION_INTERVAL_SECS {
		return;
	}
	store.last_eviction_unix = now;

	let max_age_secs = config_u64("REDLIB_PREFS_MAX_AGE_DAYS", DEFAULT_MAX_AGE_DAYS).saturating_mul(24 * 60 * 60);
	let cutoff = now.saturating_sub(max_age_secs);
	if let Err(error) = conn.execute("DELETE FROM preferences WHERE last_visit_unix < ?1", params![u64_to_db(cutoff)]) {
		warn!("preference store: failed to evict old preference records: {error}");
	}
}

fn db_to_u64(value: i64) -> u64 {
	u64::try_from(value).unwrap_or_default()
}

fn u64_to_db(value: u64) -> i64 {
	i64::try_from(value).unwrap_or(i64::MAX)
}

fn has_legacy_preferences(req: &Request<Body>) -> bool {
	LEGACY_SIMPLE_COOKIES.iter().any(|name| req.cookie(name).is_some()) || LEGACY_SHARDED_COOKIES.iter().any(|base| legacy_sharded_cookie_present(req, base))
}

fn legacy_sharded_cookie_present(req: &Request<Body>, base_name: &str) -> bool {
	(0..MAX_COOKIE_SHARDS).any(|index| {
		let name = if index == 0 { base_name.to_string() } else { format!("{base_name}{index}") };
		req.cookie(&name).is_some()
	})
}

fn legacy_preference_cookie_names(req: &Request<Body>) -> Vec<String> {
	let mut names = Vec::new();
	for name in LEGACY_SIMPLE_COOKIES {
		if req.cookie(name).is_some() {
			names.push((*name).to_string());
		}
	}
	for name in LEGACY_SHARDED_COOKIES {
		for index in 0..MAX_COOKIE_SHARDS {
			let cookie_name = if index == 0 { (*name).to_string() } else { format!("{name}{index}") };
			if req.cookie(&cookie_name).is_some() {
				names.push(cookie_name);
			}
		}
	}
	names
}

fn clear_legacy_preference_cookies(response: &mut Response<Body>, names: &[String]) {
	for name in names {
		response.remove_cookie(name.clone());
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sealed_test::prelude::*;

	fn prefs(subscriptions: &[&str], filters: &[&str]) -> Preferences {
		let mut prefs = Preferences {
			subscriptions: subscriptions.iter().map(|value| (*value).to_string()).collect(),
			filters: filters.iter().map(|value| (*value).to_string()).collect(),
			theme: "dark".to_string(),
			layout: "compact".to_string(),
			show_nsfw: "on".to_string(),
			..Preferences::default()
		};
		prefs.prepare_for_runtime();
		prefs
	}

	fn request_with_cookie(cookie: &str) -> Request<Body> {
		Request::builder().header("Cookie", cookie).body(Body::empty()).unwrap()
	}

	#[test]
	fn legacy_recovery_accepts_small_feed_subset_with_matching_scalar_cookie() {
		let req = request_with_cookie("theme=dark; subscriptions=rust%2Bprogramming");
		let legacy = prefs(&["rust", "programming"], &[]);
		let candidate_subscriptions = (0..70)
			.map(|index| format!("subreddit{index}"))
			.chain(["rust".to_string(), "programming".to_string()])
			.collect::<Vec<_>>();
		let mut candidate = prefs(&[], &[]);
		candidate.subscriptions = candidate_subscriptions;
		candidate.prepare_for_runtime();

		assert!(candidate_can_adopt_legacy_preferences(&req, &legacy, &candidate));
	}

	#[test]
	fn legacy_recovery_rejects_candidate_missing_feed_subset() {
		let req = request_with_cookie("theme=dark; subscriptions=rust%2Bprogramming");
		let legacy = prefs(&["rust", "programming"], &[]);
		let candidate_subscriptions = (0..70).map(|index| format!("subreddit{index}")).chain(["rust".to_string()]).collect::<Vec<_>>();
		let mut candidate = prefs(&[], &[]);
		candidate.subscriptions = candidate_subscriptions;
		candidate.prepare_for_runtime();

		assert!(!candidate_can_adopt_legacy_preferences(&req, &legacy, &candidate));
	}

	#[test]
	fn legacy_recovery_rejects_scalar_cookie_mismatch() {
		let req = request_with_cookie("theme=light; subscriptions=rust%2Bprogramming");
		let legacy = prefs(&["rust", "programming"], &[]);
		let candidate_subscriptions = (0..70)
			.map(|index| format!("subreddit{index}"))
			.chain(["rust".to_string(), "programming".to_string()])
			.collect::<Vec<_>>();
		let mut candidate = prefs(&[], &[]);
		candidate.subscriptions = candidate_subscriptions;
		candidate.prepare_for_runtime();

		assert!(!candidate_can_adopt_legacy_preferences(&req, &legacy, &candidate));
	}

	#[test]
	fn legacy_recovery_only_adopts_partial_legacy_feed_lists() {
		let legacy_subscriptions = (0..70).map(|index| format!("subreddit{index}")).collect::<Vec<_>>();
		let legacy = Preferences {
			subscriptions: legacy_subscriptions,
			..Preferences::default()
		};

		assert!(!legacy_preferences_look_partial(&legacy));
	}

	#[test]
	fn trusted_profiles_resolve_to_first_allowlisted_id() {
		let canonical = "00000000-0000-4000-8000-000000000001";
		let member = "00000000-0000-4000-8000-000000000002";
		let outsider = "00000000-0000-4000-8000-000000000003";
		let configured = format!("{canonical},{member}");

		assert_eq!(trusted_profile_canonical_id_from(canonical, &configured), Some(canonical.to_string()));
		assert_eq!(trusted_profile_canonical_id_from(member, &configured), Some(canonical.to_string()));
		assert_eq!(trusted_profile_canonical_id_from(outsider, &configured), None);
	}

	#[test]
	fn trusted_profiles_ignore_invalid_ids() {
		let member = "00000000-0000-4000-8000-000000000002";
		assert_eq!(trusted_profile_canonical_id_from(member, "not-a-uuid,also-invalid"), None);
	}

	#[test]
	#[sealed_test(env = [
		(
			"REDLIB_TRUSTED_PREFS_PROFILE_IDS",
			"00000000-0000-4000-8000-000000000001,00000000-0000-4000-8000-000000000002"
		)
	])]
	fn trusted_member_request_loads_canonical_profile_and_migrates_cookie() {
		let db_path = PathBuf::from(format!("/tmp/redlib-trusted-prefs-profile-alias-test-{}.sqlite3", std::process::id()));
		std::env::set_var("REDLIB_PREFS_DB", &db_path);
		for suffix in ["", "-wal", "-shm"] {
			let _ = fs::remove_file(format!("{}{}", db_path.display(), suffix));
		}

		let canonical = "00000000-0000-4000-8000-000000000001";
		let member = "00000000-0000-4000-8000-000000000002";
		let canonical_prefs = prefs(&["rust"], &["news"]);
		let mut member_prefs = prefs(&["programming"], &["sports"]);
		member_prefs.theme = "light".to_string();
		save_record(canonical, &canonical_prefs).unwrap();
		save_record(member, &member_prefs).unwrap();

		let mut req = request_with_cookie(&format!("{PREFS_COOKIE}={member}"));
		let actions = prepare_request(&mut req);
		let loaded = req.extensions().get::<RequestPreferences>().unwrap();
		assert_eq!(loaded.id, canonical);
		assert_eq!(loaded.prefs.theme, "dark");
		assert_eq!(loaded.prefs.subscriptions, ["rust"]);
		assert_eq!(actions.set_session_id.as_deref(), Some(canonical));

		for suffix in ["", "-wal", "-shm"] {
			let _ = fs::remove_file(format!("{}{}", db_path.display(), suffix));
		}
	}
}
