use crate::config;
use crate::server::{RequestExt, ResponseExt};
use crate::utils::Preferences;
use cookie::{Cookie, SameSite};
use hyper::{Body, Request, Response};
use log::warn;
use rusqlite::{params, Connection, OptionalExtension};
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

	if let Some(id) = cookie_id {
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
				let prefs = Preferences::from_cookies(req);
				if save_record(&id, &prefs).is_ok() {
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
					let prefs = Preferences::from_cookies(req);
					if save_record(&id, &prefs).is_ok() {
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
		let id = new_session_id();
		let prefs = Preferences::from_cookies(req);
		if save_record(&id, &prefs).is_ok() {
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
