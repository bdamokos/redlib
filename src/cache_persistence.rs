use crate::config::get_setting;
use crate::{post_cache, subreddit, utils};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

const SNAPSHOT_VERSION: u8 = 5;
const LEGACY_SNAPSHOT_VERSION_V4: u8 = 4;
const LEGACY_SNAPSHOT_VERSION_V3: u8 = 3;
const LEGACY_SNAPSHOT_VERSION_V2: u8 = 2;
const SNAPSHOT_FILE_NAME: &str = "subreddit-cache.v2.bin";
const SOURCE_CACHE_DIR_NAME: &str = "sources.v2";
const DEFAULT_CACHE_DIR: &str = "/var/lib/redlib/cache";
const WRITE_DEBOUNCE_SECS: u64 = 1;

static PERSISTENCE_EVENTS: OnceLock<mpsc::UnboundedSender<()>> = OnceLock::new();
static PERSISTENCE_PATH: LazyLock<PathBuf> = LazyLock::new(snapshot_path);

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum PersistedRefreshOrigin {
	Daemon,
	FrontPageSession,
	DirectLiveFetch,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedFlairPart {
	pub flair_part_type: String,
	pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedFlair {
	pub flair_parts: Vec<PersistedFlairPart>,
	pub text: String,
	pub background_color: String,
	pub foreground_color: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedExternalLink {
	pub label: String,
	pub url: String,
	pub host: String,
	pub icon_path: String,
	pub has_coomer: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedAuthor {
	pub name: String,
	pub flair: PersistedFlair,
	pub distinguished: String,
	pub external_links: Vec<PersistedExternalLink>,
	pub avg_video_length: Option<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedPollOption {
	pub id: u64,
	pub text: String,
	pub vote_count: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedPoll {
	pub poll_options: Vec<PersistedPollOption>,
	pub voting_end_timestamp: (String, String),
	pub total_vote_count: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedFlags {
	pub spoiler: bool,
	pub nsfw: bool,
	pub stickied: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedMedia {
	pub url: String,
	pub preview_url: String,
	pub preview_srcset: String,
	#[serde(default)]
	pub content_fingerprint: String,
	pub hls_url: String,
	pub dash_url: String,
	pub width: i64,
	pub height: i64,
	pub poster: String,
	#[serde(default)]
	pub hd_poster: String,
	pub duration_seconds: i64,
	pub duration: String,
	pub download_name: String,
	pub embed_code: String,
	pub video_preview_enabled: bool,
	pub video_preview_job_hint: String,
	#[serde(default)]
	pub video_preview_fallback_url: String,
	#[serde(default)]
	pub redgifs_username: String,
	#[serde(default)]
	pub redgifs_created_at_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedGalleryMedia {
	pub url: String,
	pub preview_url: String,
	pub preview_srcset: String,
	pub width: i64,
	pub height: i64,
	pub caption: String,
	pub outbound_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedAward {
	pub name: String,
	pub icon_url: String,
	pub description: String,
	pub count: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedCollapsedDuplicate {
	pub title: String,
	pub permalink: String,
	pub community: String,
	pub display_community: String,
	pub created_ts: u64,
	pub rel_time: String,
	pub comments: (String, String),
	pub flair: PersistedFlair,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct PersistedParentComment {
	pub id: String,
	pub body: String,
	pub permalink: String,
	pub author: String,
	pub rel_time: String,
	pub expand: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedFollowingComment {
	pub id: String,
	#[serde(default)]
	pub author: String,
	pub body: String,
	pub permalink: String,
	pub created_ts: u64,
	pub rel_time: String,
	pub community: String,
	pub score: (String, String),
	#[serde(default)]
	pub parent_id: String,
	pub parent_kind: String,
	#[serde(default)]
	pub parent_comment: PersistedParentComment,
	pub link_title: String,
	pub link_permalink: String,
	#[serde(default)]
	pub link_author: String,
	#[serde(default)]
	pub link_created_ts: u64,
	pub thread_comments: (String, String),
	pub thread_nsfw: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedPost {
	pub id: String,
	pub kind: String,
	pub title: String,
	pub community: String,
	pub body: String,
	pub author: PersistedAuthor,
	pub permalink: String,
	#[serde(default)]
	pub parent_id: String,
	pub parent_kind: String,
	#[serde(default)]
	pub parent_comment: PersistedParentComment,
	#[serde(default)]
	pub link_id: String,
	pub link_title: String,
	pub link_permalink: String,
	#[serde(default)]
	pub link_author: String,
	#[serde(default)]
	pub link_created_ts: u64,
	pub poll: Option<PersistedPoll>,
	pub score: (String, String),
	pub upvote_ratio: i64,
	pub post_type: String,
	pub flair: PersistedFlair,
	pub flags: PersistedFlags,
	pub thumbnail: PersistedMedia,
	pub media: PersistedMedia,
	pub domain: String,
	pub rel_time: String,
	pub created: String,
	pub created_ts: u64,
	pub num_duplicates: u64,
	pub comments: (String, String),
	pub gallery: Vec<PersistedGalleryMedia>,
	pub awards: Vec<PersistedAward>,
	pub nsfw: bool,
	pub out_url: Option<String>,
	pub ws_url: String,
	pub preview_body: String,
	pub collapsed_duplicates: Vec<PersistedCollapsedDuplicate>,
	#[serde(default)]
	pub following_comments: Vec<PersistedFollowingComment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedSubreddit {
	pub name: String,
	pub title: String,
	pub description: String,
	pub info: String,
	pub has_sidebar: bool,
	pub icon: String,
	pub members: (String, String),
	pub active: (String, String),
	pub wiki: bool,
	pub nsfw: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedSourceCacheEntry {
	pub subreddit: String,
	pub posts: Vec<PersistedPost>,
	pub after: String,
	pub fetched_at_unix: u64,
	pub refresh_origin: Option<PersistedRefreshOrigin>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedRememberedSubredditEntry {
	pub name: String,
	pub last_requested_at_unix: u64,
	pub last_refresh_started_at_unix: Option<u64>,
	pub last_refresh_succeeded_at_unix: Option<u64>,
	pub last_refresh_failed_at_unix: Option<u64>,
	pub banned_until_unix: Option<u64>,
	pub consecutive_failures: u32,
	pub estimated_post_interval_secs: u64,
	pub refresh_in_flight: bool,
	#[serde(default)]
	pub visit_priority_score: f64,
	#[serde(default)]
	pub visit_priority_last_update_unix: Option<u64>,
	#[serde(default)]
	pub visit_signal_last_applied_unix: Option<u64>,
	#[serde(default)]
	pub feedback_priority_score: f64,
	#[serde(default)]
	pub feedback_priority_last_update_unix: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedRememberedSubredditEntryWithoutFeedback {
	pub name: String,
	pub last_requested_at_unix: u64,
	pub last_refresh_started_at_unix: Option<u64>,
	pub last_refresh_succeeded_at_unix: Option<u64>,
	pub last_refresh_failed_at_unix: Option<u64>,
	pub banned_until_unix: Option<u64>,
	pub consecutive_failures: u32,
	pub estimated_post_interval_secs: u64,
	pub refresh_in_flight: bool,
	#[serde(default)]
	pub visit_priority_score: f64,
	#[serde(default)]
	pub visit_priority_last_update_unix: Option<u64>,
	#[serde(default)]
	pub visit_signal_last_applied_unix: Option<u64>,
}

impl From<PersistedRememberedSubredditEntryWithoutFeedback> for PersistedRememberedSubredditEntry {
	fn from(entry: PersistedRememberedSubredditEntryWithoutFeedback) -> Self {
		Self {
			name: entry.name,
			last_requested_at_unix: entry.last_requested_at_unix,
			last_refresh_started_at_unix: entry.last_refresh_started_at_unix,
			last_refresh_succeeded_at_unix: entry.last_refresh_succeeded_at_unix,
			last_refresh_failed_at_unix: entry.last_refresh_failed_at_unix,
			banned_until_unix: entry.banned_until_unix,
			consecutive_failures: entry.consecutive_failures,
			estimated_post_interval_secs: entry.estimated_post_interval_secs,
			refresh_in_flight: entry.refresh_in_flight,
			visit_priority_score: entry.visit_priority_score,
			visit_priority_last_update_unix: entry.visit_priority_last_update_unix,
			visit_signal_last_applied_unix: entry.visit_signal_last_applied_unix,
			feedback_priority_score: 0.0,
			feedback_priority_last_update_unix: None,
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedSubredditMetadataEntry {
	pub subreddit: PersistedSubreddit,
	pub fetched_at_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedSubredditNsfwEntry {
	pub subreddit: String,
	pub is_nsfw: bool,
	pub fetched_at_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedSubredditFlairsEntry {
	pub subreddit: String,
	pub flairs: Vec<PersistedFlair>,
	pub cached_at_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PersistedSubredditUsersEntry {
	pub subreddit: String,
	pub users: Vec<(String, u32)>,
	pub cached_at_unix: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedCacheSnapshot {
	version: u8,
	saved_at_unix: u64,
	source_entries: Vec<PersistedSourceCacheEntry>,
	remembered_subreddits: Vec<PersistedRememberedSubredditEntry>,
	subreddit_metadata: Vec<PersistedSubredditMetadataEntry>,
	subreddit_nsfw: Vec<PersistedSubredditNsfwEntry>,
	subreddit_flairs: Vec<PersistedSubredditFlairsEntry>,
	subreddit_users: Vec<PersistedSubredditUsersEntry>,
	author_history: Vec<crate::materialized_catalog::PersistedAuthorHistoryEntry>,
	rejected_author_redgifs_profiles: Vec<crate::materialized_catalog::PersistedRejectedAuthorRedgifsProfileEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedCacheSnapshotV4WithoutFeedback {
	version: u8,
	saved_at_unix: u64,
	source_entries: Vec<PersistedSourceCacheEntry>,
	remembered_subreddits: Vec<PersistedRememberedSubredditEntryWithoutFeedback>,
	subreddit_metadata: Vec<PersistedSubredditMetadataEntry>,
	subreddit_nsfw: Vec<PersistedSubredditNsfwEntry>,
	subreddit_flairs: Vec<PersistedSubredditFlairsEntry>,
	subreddit_users: Vec<PersistedSubredditUsersEntry>,
	author_history: Vec<crate::materialized_catalog::PersistedAuthorHistoryEntry>,
	rejected_author_redgifs_profiles: Vec<crate::materialized_catalog::PersistedRejectedAuthorRedgifsProfileEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedCacheSnapshotV3 {
	version: u8,
	saved_at_unix: u64,
	source_entries: Vec<PersistedSourceCacheEntry>,
	remembered_subreddits: Vec<PersistedRememberedSubredditEntryWithoutFeedback>,
	subreddit_metadata: Vec<PersistedSubredditMetadataEntry>,
	subreddit_nsfw: Vec<PersistedSubredditNsfwEntry>,
	subreddit_flairs: Vec<PersistedSubredditFlairsEntry>,
	subreddit_users: Vec<PersistedSubredditUsersEntry>,
	author_history: Vec<crate::materialized_catalog::PersistedAuthorHistoryEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedCacheSnapshotV2 {
	version: u8,
	saved_at_unix: u64,
	source_entries: Vec<PersistedSourceCacheEntry>,
	remembered_subreddits: Vec<PersistedRememberedSubredditEntryWithoutFeedback>,
	subreddit_metadata: Vec<PersistedSubredditMetadataEntry>,
	subreddit_nsfw: Vec<PersistedSubredditNsfwEntry>,
	subreddit_flairs: Vec<PersistedSubredditFlairsEntry>,
	subreddit_users: Vec<PersistedSubredditUsersEntry>,
}

#[cfg(test)]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedCatalogSnapshotFile {
	version: u8,
	saved_at_unix: u64,
	snapshot: crate::materialized_catalog::PersistedCatalogSnapshot,
}

pub fn cache_dir() -> String {
	get_setting("REDLIB_CACHE_DIR").unwrap_or_else(|| DEFAULT_CACHE_DIR.to_string())
}

pub(crate) fn snapshot_path() -> PathBuf {
	Path::new(&cache_dir()).join(SNAPSHOT_FILE_NAME)
}

fn source_cache_dir() -> PathBuf {
	Path::new(&cache_dir()).join(SOURCE_CACHE_DIR_NAME)
}

fn source_cache_path(source_cache_key: &str) -> PathBuf {
	source_cache_dir().join(format!("{}.bin", URL_SAFE_NO_PAD.encode(source_cache_key.as_bytes())))
}

pub(crate) fn unix_now() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_else(|_| Duration::from_secs(0)).as_secs()
}

pub(crate) fn instant_to_unix(instant: Instant, now_unix: u64) -> u64 {
	now_unix.saturating_sub(instant.elapsed().as_secs())
}

pub(crate) fn option_instant_to_unix(instant: Option<Instant>, now_unix: u64) -> Option<u64> {
	instant.map(|value| instant_to_unix(value, now_unix))
}

pub(crate) fn unix_to_instant(timestamp_unix: u64, now_unix: u64) -> Instant {
	Instant::now() - Duration::from_secs(now_unix.saturating_sub(timestamp_unix))
}

pub(crate) fn option_unix_to_instant(timestamp_unix: Option<u64>, now_unix: u64) -> Option<Instant> {
	timestamp_unix.map(|value| unix_to_instant(value, now_unix))
}

pub fn init() {
	if let Err(error) = fs::create_dir_all(cache_dir()) {
		warn!("cache_persistence: failed to create cache dir: {error}");
	}

	let mut persist_startup_recovery = false;
	if let Err(error) = load_snapshot() {
		warn!("cache_persistence: failed to load persisted cache snapshot: {error}");
	}

	match source_cache_seed_entries() {
		Ok(entries) => {
			let inserted = post_cache::seed_remembered_feeds_from_source_cache_entries(entries, unix_now());
			if inserted > 0 {
				info!("cache_persistence: seeded {inserted} remembered feed rows from persisted source cache entries");
				persist_startup_recovery = true;
			}
		}
		Err(error) => warn!("cache_persistence: failed to inspect persisted source cache entries: {error}"),
	}
	load_catalog_snapshot_or_rebuild();

	if PERSISTENCE_EVENTS.get().is_none() {
		let (tx, mut rx) = mpsc::unbounded_channel();
		if PERSISTENCE_EVENTS.set(tx).is_ok() {
			tokio::spawn(async move {
				while rx.recv().await.is_some() {
					tokio::time::sleep(Duration::from_secs(WRITE_DEBOUNCE_SECS)).await;
					while rx.try_recv().is_ok() {}
					if let Err(error) = write_snapshot() {
						warn!("cache_persistence: failed to persist subreddit cache snapshot: {error}");
					}
				}
			});
		}
	}

	if persist_startup_recovery {
		mark_dirty();
	}
}

pub fn mark_dirty() {
	if let Some(sender) = PERSISTENCE_EVENTS.get() {
		let _ = sender.send(());
	}
}

#[cfg(not(test))]
pub(crate) fn write_source_cache_entry(source_cache_key: &str, entry: &PersistedSourceCacheEntry) -> Result<(), String> {
	let final_path = source_cache_path(source_cache_key);
	let temp_path = final_path.with_extension("tmp");

	if let Some(parent) = final_path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let bytes = bincode::serialize(entry).map_err(|error| error.to_string())?;
	let mut file = File::create(&temp_path).map_err(|error| error.to_string())?;
	file.write_all(&bytes).map_err(|error| error.to_string())?;
	file.sync_all().map_err(|error| error.to_string())?;
	drop(file);
	fs::rename(&temp_path, final_path).map_err(|error| error.to_string())
}

#[cfg(not(test))]
pub(crate) fn read_source_cache_entry(source_cache_key: &str) -> Result<Option<PersistedSourceCacheEntry>, String> {
	let path = source_cache_path(source_cache_key);
	let bytes = match fs::read(path) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.to_string()),
	};
	bincode::deserialize(&bytes).map(Some).map_err(|error| error.to_string())
}

#[cfg(not(test))]
pub(crate) fn source_cache_entry_exists(source_cache_key: &str) -> bool {
	source_cache_path(source_cache_key).is_file()
}

#[cfg(not(test))]
pub(crate) fn source_cache_seed_entries() -> Result<Vec<(String, u64)>, String> {
	let entries = match fs::read_dir(source_cache_dir()) {
		Ok(entries) => entries,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(error) => return Err(error.to_string()),
	};

	let mut seeds = Vec::new();
	for entry in entries {
		let entry = match entry {
			Ok(entry) => entry,
			Err(error) => {
				warn!("cache_persistence: failed to read source cache dir entry: {error}");
				continue;
			}
		};
		let path = entry.path();
		if path.extension().and_then(|extension| extension.to_str()) != Some("bin") {
			continue;
		}
		let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
			continue;
		};
		let decoded = match URL_SAFE_NO_PAD.decode(stem) {
			Ok(decoded) => decoded,
			Err(error) => {
				warn!("cache_persistence: ignoring source cache file with undecodable name {:?}: {error}", path.file_name());
				continue;
			}
		};
		let key = match String::from_utf8(decoded) {
			Ok(key) => key,
			Err(error) => {
				warn!("cache_persistence: ignoring source cache file with non-utf8 key {:?}: {error}", path.file_name());
				continue;
			}
		};
		let modified = match entry.metadata().and_then(|metadata| metadata.modified()) {
			Ok(modified) => modified,
			Err(error) => {
				warn!("cache_persistence: failed to stat source cache file {:?}: {error}", path.file_name());
				continue;
			}
		};
		let fetched_at_unix = match modified.duration_since(UNIX_EPOCH) {
			Ok(duration) => duration.as_secs(),
			Err(error) => {
				warn!("cache_persistence: source cache file {:?} has pre-epoch mtime: {error}", path.file_name());
				continue;
			}
		};
		seeds.push((key, fetched_at_unix));
	}

	Ok(seeds)
}

#[cfg(test)]
pub(crate) fn source_cache_seed_entries() -> Result<Vec<(String, u64)>, String> {
	Ok(Vec::new())
}

pub(crate) fn remove_source_cache_entry(source_cache_key: &str) {
	if let Err(error) = fs::remove_file(source_cache_path(source_cache_key)) {
		if error.kind() != std::io::ErrorKind::NotFound {
			warn!("cache_persistence: failed to remove source cache entry {source_cache_key}: {error}");
		}
	}
}

fn load_snapshot() -> Result<(), String> {
	let path = &*PERSISTENCE_PATH;
	let bytes = match fs::read(path) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Err(error) => return Err(error.to_string()),
	};

	let snapshot = deserialize_snapshot(&bytes)?;
	let now_unix = unix_now();
	post_cache::restore_persisted_state(snapshot.source_entries, snapshot.remembered_subreddits, snapshot.saved_at_unix, now_unix);
	crate::materialized_catalog::restore_persisted_rejected_author_redgifs_profiles(snapshot.rejected_author_redgifs_profiles, snapshot.saved_at_unix, now_unix);
	crate::materialized_catalog::restore_persisted_author_history(snapshot.author_history, snapshot.saved_at_unix, now_unix);
	subreddit::restore_persisted_state(snapshot.subreddit_metadata, snapshot.subreddit_nsfw, snapshot.saved_at_unix, now_unix);
	utils::restore_persisted_state(snapshot.subreddit_flairs, snapshot.subreddit_users, snapshot.saved_at_unix, now_unix);
	Ok(())
}

fn write_snapshot() -> Result<(), String> {
	let now_unix = unix_now();
	let author_history = crate::materialized_catalog::snapshot_persisted_author_history(now_unix);
	let rejected_author_redgifs_profiles = crate::materialized_catalog::snapshot_persisted_rejected_author_redgifs_profiles(now_unix);
	let snapshot = PersistedCacheSnapshot {
		version: SNAPSHOT_VERSION,
		saved_at_unix: now_unix,
		source_entries: post_cache::snapshot_persisted_source_entries(now_unix),
		remembered_subreddits: post_cache::snapshot_persisted_remembered_subreddits(now_unix),
		subreddit_metadata: subreddit::snapshot_persisted_metadata(now_unix),
		subreddit_nsfw: subreddit::snapshot_persisted_nsfw(now_unix),
		subreddit_flairs: utils::snapshot_persisted_subreddit_flairs(now_unix),
		subreddit_users: utils::snapshot_persisted_subreddit_users(now_unix),
		author_history,
		rejected_author_redgifs_profiles,
	};

	let bytes = serialize_snapshot(&snapshot)?;
	let final_path = &*PERSISTENCE_PATH;
	let temp_path = final_path.with_extension("tmp");

	if let Some(parent) = final_path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}

	let mut file = File::create(&temp_path).map_err(|error| error.to_string())?;
	file.write_all(&bytes).map_err(|error| error.to_string())?;
	file.sync_all().map_err(|error| error.to_string())?;
	drop(file);
	fs::rename(&temp_path, final_path).map_err(|error| error.to_string())
}

fn load_catalog_snapshot_or_rebuild() {
	post_cache::rebuild_materialized_catalog_from_source_cache();
}

fn serialize_snapshot(snapshot: &PersistedCacheSnapshot) -> Result<Vec<u8>, String> {
	bincode::serialize(snapshot).map_err(|error| error.to_string())
}

fn deserialize_snapshot(bytes: &[u8]) -> Result<PersistedCacheSnapshot, String> {
	let current_error = match bincode::deserialize::<PersistedCacheSnapshot>(bytes) {
		Ok(snapshot) if snapshot.version == SNAPSHOT_VERSION => return Ok(snapshot),
		Ok(mut snapshot) if snapshot.version == LEGACY_SNAPSHOT_VERSION_V4 => {
			snapshot.version = SNAPSHOT_VERSION;
			return Ok(snapshot);
		}
		Ok(snapshot) => format!("unsupported snapshot version {}", snapshot.version),
		Err(error) => error.to_string(),
	};

	if let Ok(legacy) = bincode::deserialize::<PersistedCacheSnapshotV4WithoutFeedback>(bytes) {
		if legacy.version == LEGACY_SNAPSHOT_VERSION_V4 {
			return Ok(PersistedCacheSnapshot {
				version: SNAPSHOT_VERSION,
				saved_at_unix: legacy.saved_at_unix,
				source_entries: legacy.source_entries,
				remembered_subreddits: legacy.remembered_subreddits.into_iter().map(Into::into).collect(),
				subreddit_metadata: legacy.subreddit_metadata,
				subreddit_nsfw: legacy.subreddit_nsfw,
				subreddit_flairs: legacy.subreddit_flairs,
				subreddit_users: legacy.subreddit_users,
				author_history: legacy.author_history,
				rejected_author_redgifs_profiles: legacy.rejected_author_redgifs_profiles,
			});
		}
	}

	if let Ok(legacy) = bincode::deserialize::<PersistedCacheSnapshotV3>(bytes) {
		if legacy.version == LEGACY_SNAPSHOT_VERSION_V3 {
			return Ok(PersistedCacheSnapshot {
				version: SNAPSHOT_VERSION,
				saved_at_unix: legacy.saved_at_unix,
				source_entries: legacy.source_entries,
				remembered_subreddits: legacy.remembered_subreddits.into_iter().map(Into::into).collect(),
				subreddit_metadata: legacy.subreddit_metadata,
				subreddit_nsfw: legacy.subreddit_nsfw,
				subreddit_flairs: legacy.subreddit_flairs,
				subreddit_users: legacy.subreddit_users,
				author_history: legacy.author_history,
				rejected_author_redgifs_profiles: Vec::new(),
			});
		}
	}

	let legacy: PersistedCacheSnapshotV2 = bincode::deserialize(bytes).map_err(|_| current_error)?;
	if legacy.version != LEGACY_SNAPSHOT_VERSION_V2 {
		return Err(format!("unsupported snapshot version {}", legacy.version));
	}
	Ok(PersistedCacheSnapshot {
		version: SNAPSHOT_VERSION,
		saved_at_unix: legacy.saved_at_unix,
		source_entries: legacy.source_entries,
		remembered_subreddits: legacy.remembered_subreddits.into_iter().map(Into::into).collect(),
		subreddit_metadata: legacy.subreddit_metadata,
		subreddit_nsfw: legacy.subreddit_nsfw,
		subreddit_flairs: legacy.subreddit_flairs,
		subreddit_users: legacy.subreddit_users,
		author_history: Vec::new(),
		rejected_author_redgifs_profiles: Vec::new(),
	})
}

#[cfg(test)]
fn serialize_catalog_snapshot(snapshot: &PersistedCatalogSnapshotFile) -> Result<Vec<u8>, String> {
	bincode::serialize(snapshot).map_err(|error| error.to_string())
}

#[cfg(test)]
fn deserialize_catalog_snapshot(bytes: &[u8]) -> Result<PersistedCatalogSnapshotFile, String> {
	let snapshot: PersistedCatalogSnapshotFile = bincode::deserialize(bytes).map_err(|error| error.to_string())?;
	if snapshot.version != SNAPSHOT_VERSION {
		return Err(format!("unsupported snapshot version {}", snapshot.version));
	}
	Ok(snapshot)
}

impl PersistedRefreshOrigin {
	pub(crate) fn from_runtime(origin: post_cache::RefreshOrigin) -> Self {
		match origin {
			post_cache::RefreshOrigin::Daemon => Self::Daemon,
			post_cache::RefreshOrigin::FrontPageSession => Self::FrontPageSession,
			post_cache::RefreshOrigin::DirectLiveFetch => Self::DirectLiveFetch,
		}
	}

	#[cfg(not(test))]
	pub(crate) fn into_runtime(self) -> post_cache::RefreshOrigin {
		match self {
			Self::Daemon => post_cache::RefreshOrigin::Daemon,
			Self::FrontPageSession => post_cache::RefreshOrigin::FrontPageSession,
			Self::DirectLiveFetch => post_cache::RefreshOrigin::DirectLiveFetch,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{
		deserialize_catalog_snapshot, deserialize_snapshot, serialize_catalog_snapshot, serialize_snapshot, PersistedCacheSnapshot, PersistedCacheSnapshotV2,
		PersistedCacheSnapshotV3, PersistedCacheSnapshotV4WithoutFeedback, PersistedCatalogSnapshotFile, PersistedRememberedSubredditEntry,
		PersistedRememberedSubredditEntryWithoutFeedback, LEGACY_SNAPSHOT_VERSION_V2, LEGACY_SNAPSHOT_VERSION_V3, LEGACY_SNAPSHOT_VERSION_V4, SNAPSHOT_VERSION,
	};
	use crate::materialized_catalog::PersistedCatalogSnapshot;

	fn empty_snapshot() -> PersistedCacheSnapshot {
		PersistedCacheSnapshot {
			version: SNAPSHOT_VERSION,
			saved_at_unix: 1_000_000,
			source_entries: Vec::new(),
			remembered_subreddits: Vec::new(),
			subreddit_metadata: Vec::new(),
			subreddit_nsfw: Vec::new(),
			subreddit_flairs: Vec::new(),
			subreddit_users: Vec::new(),
			author_history: Vec::new(),
			rejected_author_redgifs_profiles: Vec::new(),
		}
	}

	fn remembered_entry_without_feedback() -> PersistedRememberedSubredditEntryWithoutFeedback {
		PersistedRememberedSubredditEntryWithoutFeedback {
			name: "rust".to_string(),
			last_requested_at_unix: 999_900,
			last_refresh_started_at_unix: Some(999_910),
			last_refresh_succeeded_at_unix: Some(999_920),
			last_refresh_failed_at_unix: None,
			banned_until_unix: None,
			consecutive_failures: 2,
			estimated_post_interval_secs: 3_600,
			refresh_in_flight: true,
			visit_priority_score: 1.25,
			visit_priority_last_update_unix: Some(999_930),
			visit_signal_last_applied_unix: Some(999_940),
		}
	}

	fn remembered_entry_with_feedback() -> PersistedRememberedSubredditEntry {
		PersistedRememberedSubredditEntry {
			name: "rust".to_string(),
			last_requested_at_unix: 999_900,
			last_refresh_started_at_unix: Some(999_910),
			last_refresh_succeeded_at_unix: Some(999_920),
			last_refresh_failed_at_unix: None,
			banned_until_unix: None,
			consecutive_failures: 2,
			estimated_post_interval_secs: 3_600,
			refresh_in_flight: true,
			visit_priority_score: 1.25,
			visit_priority_last_update_unix: Some(999_930),
			visit_signal_last_applied_unix: Some(999_940),
			feedback_priority_score: 0.75,
			feedback_priority_last_update_unix: Some(999_950),
		}
	}

	fn empty_legacy_v4_without_feedback_snapshot() -> PersistedCacheSnapshotV4WithoutFeedback {
		PersistedCacheSnapshotV4WithoutFeedback {
			version: LEGACY_SNAPSHOT_VERSION_V4,
			saved_at_unix: 1_000_000,
			source_entries: Vec::new(),
			remembered_subreddits: Vec::new(),
			subreddit_metadata: Vec::new(),
			subreddit_nsfw: Vec::new(),
			subreddit_flairs: Vec::new(),
			subreddit_users: Vec::new(),
			author_history: Vec::new(),
			rejected_author_redgifs_profiles: Vec::new(),
		}
	}

	fn empty_legacy_v3_snapshot() -> PersistedCacheSnapshotV3 {
		PersistedCacheSnapshotV3 {
			version: LEGACY_SNAPSHOT_VERSION_V3,
			saved_at_unix: 1_000_000,
			source_entries: Vec::new(),
			remembered_subreddits: Vec::new(),
			subreddit_metadata: Vec::new(),
			subreddit_nsfw: Vec::new(),
			subreddit_flairs: Vec::new(),
			subreddit_users: Vec::new(),
			author_history: Vec::new(),
		}
	}

	fn empty_legacy_v2_snapshot() -> PersistedCacheSnapshotV2 {
		PersistedCacheSnapshotV2 {
			version: LEGACY_SNAPSHOT_VERSION_V2,
			saved_at_unix: 1_000_000,
			source_entries: Vec::new(),
			remembered_subreddits: Vec::new(),
			subreddit_metadata: Vec::new(),
			subreddit_nsfw: Vec::new(),
			subreddit_flairs: Vec::new(),
			subreddit_users: Vec::new(),
		}
	}

	fn empty_catalog_snapshot() -> PersistedCatalogSnapshotFile {
		PersistedCatalogSnapshotFile {
			version: SNAPSHOT_VERSION,
			saved_at_unix: 1_000_000,
			snapshot: PersistedCatalogSnapshot::default(),
		}
	}

	#[test]
	fn snapshot_round_trips_through_bincode() {
		let snapshot = empty_snapshot();
		let bytes = serialize_snapshot(&snapshot).expect("serialize snapshot");
		let decoded = deserialize_snapshot(&bytes).expect("deserialize snapshot");
		assert_eq!(decoded.version, SNAPSHOT_VERSION);
		assert_eq!(decoded.saved_at_unix, snapshot.saved_at_unix);
		assert!(decoded.source_entries.is_empty());
		assert!(decoded.author_history.is_empty());
		assert!(decoded.rejected_author_redgifs_profiles.is_empty());
	}

	#[test]
	fn legacy_v4_current_shape_snapshot_deserializes_and_preserves_feedback_priority() {
		let mut snapshot = empty_snapshot();
		snapshot.version = LEGACY_SNAPSHOT_VERSION_V4;
		snapshot.remembered_subreddits.push(remembered_entry_with_feedback());
		let bytes = bincode::serialize(&snapshot).expect("serialize v4 current-shape snapshot");
		let decoded = deserialize_snapshot(&bytes).expect("deserialize v4 current-shape snapshot");

		assert_eq!(decoded.version, SNAPSHOT_VERSION);
		assert_eq!(decoded.remembered_subreddits.len(), 1);
		assert_eq!(decoded.remembered_subreddits[0].feedback_priority_score, 0.75);
		assert_eq!(decoded.remembered_subreddits[0].feedback_priority_last_update_unix, Some(999_950));
	}

	#[test]
	fn legacy_v4_without_feedback_snapshot_deserializes_with_neutral_feedback_priority() {
		let mut snapshot = empty_legacy_v4_without_feedback_snapshot();
		snapshot.remembered_subreddits.push(remembered_entry_without_feedback());
		let bytes = bincode::serialize(&snapshot).expect("serialize legacy v4 snapshot");
		let decoded = deserialize_snapshot(&bytes).expect("deserialize legacy v4 snapshot");

		assert_eq!(decoded.version, SNAPSHOT_VERSION);
		assert_eq!(decoded.remembered_subreddits.len(), 1);
		let entry = &decoded.remembered_subreddits[0];
		assert_eq!(entry.name, "rust");
		assert_eq!(entry.last_refresh_succeeded_at_unix, Some(999_920));
		assert_eq!(entry.consecutive_failures, 2);
		assert_eq!(entry.visit_priority_score, 1.25);
		assert_eq!(entry.feedback_priority_score, 0.0);
		assert_eq!(entry.feedback_priority_last_update_unix, None);
	}

	#[test]
	fn legacy_v3_snapshot_deserializes_with_empty_rejected_author_redgifs_profiles() {
		let mut snapshot = empty_legacy_v3_snapshot();
		snapshot.remembered_subreddits.push(remembered_entry_without_feedback());
		let bytes = bincode::serialize(&snapshot).expect("serialize legacy snapshot");
		let decoded = deserialize_snapshot(&bytes).expect("deserialize legacy snapshot");

		assert_eq!(decoded.version, SNAPSHOT_VERSION);
		assert_eq!(decoded.saved_at_unix, snapshot.saved_at_unix);
		assert!(decoded.author_history.is_empty());
		assert!(decoded.rejected_author_redgifs_profiles.is_empty());
		assert_eq!(decoded.remembered_subreddits[0].feedback_priority_score, 0.0);
	}

	#[test]
	fn legacy_v2_snapshot_deserializes_with_empty_author_history() {
		let snapshot = empty_legacy_v2_snapshot();
		let bytes = bincode::serialize(&snapshot).expect("serialize legacy snapshot");
		let decoded = deserialize_snapshot(&bytes).expect("deserialize legacy snapshot");

		assert_eq!(decoded.version, SNAPSHOT_VERSION);
		assert_eq!(decoded.saved_at_unix, snapshot.saved_at_unix);
		assert!(decoded.author_history.is_empty());
		assert!(decoded.rejected_author_redgifs_profiles.is_empty());
	}

	#[test]
	fn deserialize_rejects_corrupt_snapshot_bytes() {
		let error = deserialize_snapshot(b"not-bincode").expect_err("corrupt snapshot should fail");
		assert!(!error.is_empty());
	}

	#[test]
	fn deserialize_rejects_unsupported_snapshot_version() {
		let mut snapshot = empty_snapshot();
		snapshot.version = SNAPSHOT_VERSION + 1;
		let bytes = bincode::serialize(&snapshot).expect("serialize mismatched version");
		let error = deserialize_snapshot(&bytes).expect_err("version mismatch should fail");
		assert_eq!(error, format!("unsupported snapshot version {}", SNAPSHOT_VERSION + 1));
	}

	#[test]
	fn catalog_snapshot_round_trips_through_bincode() {
		let snapshot = empty_catalog_snapshot();
		let bytes = serialize_catalog_snapshot(&snapshot).expect("serialize catalog snapshot");
		let decoded = deserialize_catalog_snapshot(&bytes).expect("deserialize catalog snapshot");
		assert_eq!(decoded.version, SNAPSHOT_VERSION);
		assert_eq!(decoded.saved_at_unix, snapshot.saved_at_unix);
		assert!(decoded.snapshot.records.is_empty());
	}
}

impl PersistedFlairPart {
	pub(crate) fn from_runtime(value: &utils::FlairPart) -> Self {
		Self {
			flair_part_type: value.flair_part_type.clone(),
			value: value.value.clone(),
		}
	}

	pub(crate) fn into_runtime(self) -> utils::FlairPart {
		utils::FlairPart {
			flair_part_type: self.flair_part_type,
			value: self.value,
		}
	}
}

impl PersistedFlair {
	pub(crate) fn from_runtime(value: &utils::Flair) -> Self {
		Self {
			flair_parts: value.flair_parts.iter().map(PersistedFlairPart::from_runtime).collect(),
			text: value.text.clone(),
			background_color: value.background_color.clone(),
			foreground_color: value.foreground_color.clone(),
		}
	}

	pub(crate) fn into_runtime(self) -> utils::Flair {
		utils::Flair {
			flair_parts: self.flair_parts.into_iter().map(PersistedFlairPart::into_runtime).collect(),
			text: self.text,
			background_color: self.background_color,
			foreground_color: self.foreground_color,
		}
	}
}

impl PersistedExternalLink {
	pub(crate) fn from_runtime(value: &utils::ExternalLink) -> Self {
		Self {
			label: value.label.clone(),
			url: value.url.clone(),
			host: value.host.clone(),
			icon_path: value.icon_path.clone(),
			has_coomer: value.has_coomer,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::ExternalLink {
		utils::ExternalLink {
			label: self.label,
			url: self.url,
			host: self.host,
			icon_path: self.icon_path,
			has_coomer: self.has_coomer,
		}
	}
}

impl PersistedAuthor {
	pub(crate) fn from_runtime(value: &utils::Author) -> Self {
		Self {
			name: value.name.clone(),
			flair: PersistedFlair::from_runtime(&value.flair),
			distinguished: value.distinguished.clone(),
			external_links: value.external_links.iter().map(PersistedExternalLink::from_runtime).collect(),
			avg_video_length: value.avg_video_length.clone(),
		}
	}

	pub(crate) fn into_runtime(self) -> utils::Author {
		utils::Author {
			name: self.name,
			flair: self.flair.into_runtime(),
			distinguished: self.distinguished,
			external_links: self.external_links.into_iter().map(PersistedExternalLink::into_runtime).collect(),
			avg_video_length: self.avg_video_length,
		}
	}
}

impl PersistedPollOption {
	pub(crate) fn from_runtime(value: &utils::PollOption) -> Self {
		Self {
			id: value.id,
			text: value.text.clone(),
			vote_count: value.vote_count,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::PollOption {
		utils::PollOption {
			id: self.id,
			text: self.text,
			vote_count: self.vote_count,
		}
	}
}

impl PersistedPoll {
	pub(crate) fn from_runtime(value: &utils::Poll) -> Self {
		Self {
			poll_options: value.poll_options.iter().map(PersistedPollOption::from_runtime).collect(),
			voting_end_timestamp: value.voting_end_timestamp.clone(),
			total_vote_count: value.total_vote_count,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::Poll {
		utils::Poll {
			poll_options: self.poll_options.into_iter().map(PersistedPollOption::into_runtime).collect(),
			voting_end_timestamp: self.voting_end_timestamp,
			total_vote_count: self.total_vote_count,
		}
	}
}

impl PersistedFlags {
	pub(crate) fn from_runtime(value: &utils::Flags) -> Self {
		Self {
			spoiler: value.spoiler,
			nsfw: value.nsfw,
			stickied: value.stickied,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::Flags {
		utils::Flags::new(self.spoiler, self.nsfw, self.stickied)
	}
}

impl PersistedMedia {
	pub(crate) fn from_runtime(value: &utils::Media) -> Self {
		Self {
			url: value.url.clone(),
			preview_url: value.preview_url.clone(),
			preview_srcset: value.preview_srcset.clone(),
			content_fingerprint: value.content_fingerprint.clone(),
			hls_url: value.hls_url.clone(),
			dash_url: value.dash_url.clone(),
			width: value.width,
			height: value.height,
			poster: value.poster.clone(),
			hd_poster: value.hd_poster.clone(),
			duration_seconds: value.duration_seconds,
			duration: value.duration.clone(),
			download_name: value.download_name.clone(),
			embed_code: value.embed_code.clone(),
			video_preview_enabled: value.video_preview_enabled,
			video_preview_job_hint: value.video_preview_job_hint.clone(),
			video_preview_fallback_url: value.video_preview_fallback_url.clone(),
			redgifs_username: value.redgifs_username.clone(),
			redgifs_created_at_unix: value.redgifs_created_at_unix,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::Media {
		utils::Media {
			url: self.url,
			preview_url: self.preview_url,
			preview_srcset: self.preview_srcset,
			content_fingerprint: self.content_fingerprint,
			hls_url: self.hls_url,
			dash_url: self.dash_url,
			width: self.width,
			height: self.height,
			poster: self.poster,
			hd_poster: self.hd_poster,
			duration_seconds: self.duration_seconds,
			duration: self.duration,
			download_name: self.download_name,
			embed_code: self.embed_code,
			video_preview_enabled: self.video_preview_enabled,
			video_preview_job_hint: self.video_preview_job_hint,
			video_preview_fallback_url: self.video_preview_fallback_url,
			redgifs_username: self.redgifs_username,
			redgifs_created_at_unix: self.redgifs_created_at_unix,
		}
	}
}

impl PersistedGalleryMedia {
	pub(crate) fn from_runtime(value: &utils::GalleryMedia) -> Self {
		Self {
			url: value.url.clone(),
			preview_url: value.preview_url.clone(),
			preview_srcset: value.preview_srcset.clone(),
			width: value.width,
			height: value.height,
			caption: value.caption.clone(),
			outbound_url: value.outbound_url.clone(),
		}
	}

	pub(crate) fn into_runtime(self) -> utils::GalleryMedia {
		utils::GalleryMedia {
			url: self.url,
			preview_url: self.preview_url,
			preview_srcset: self.preview_srcset,
			width: self.width,
			height: self.height,
			caption: self.caption,
			outbound_url: self.outbound_url,
		}
	}
}

impl PersistedAward {
	pub(crate) fn from_runtime(value: &utils::Award) -> Self {
		Self {
			name: value.name.clone(),
			icon_url: value.icon_url.clone(),
			description: value.description.clone(),
			count: value.count,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::Award {
		utils::Award {
			name: self.name,
			icon_url: self.icon_url,
			description: self.description,
			count: self.count,
		}
	}
}

impl PersistedParentComment {
	pub(crate) fn from_runtime(value: &utils::ParentComment) -> Self {
		Self {
			id: value.id.clone(),
			body: value.body.clone(),
			permalink: value.permalink.clone(),
			author: value.author.clone(),
			rel_time: value.rel_time.clone(),
			expand: value.expand,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::ParentComment {
		utils::ParentComment {
			id: self.id,
			body: self.body,
			permalink: self.permalink,
			author: self.author,
			rel_time: self.rel_time,
			expand: self.expand,
		}
	}
}

impl PersistedFollowingComment {
	pub(crate) fn from_runtime(value: &utils::FollowingComment) -> Self {
		Self {
			id: value.id.clone(),
			author: value.author.clone(),
			body: value.body.clone(),
			permalink: value.permalink.clone(),
			created_ts: value.created_ts,
			rel_time: value.rel_time.clone(),
			community: value.community.clone(),
			score: value.score.clone(),
			parent_id: value.parent_id.clone(),
			parent_kind: value.parent_kind.clone(),
			parent_comment: PersistedParentComment::from_runtime(&value.parent_comment),
			link_title: value.link_title.clone(),
			link_permalink: value.link_permalink.clone(),
			link_author: value.link_author.clone(),
			link_created_ts: value.link_created_ts,
			thread_comments: value.thread_comments.clone(),
			thread_nsfw: value.thread_nsfw,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::FollowingComment {
		utils::FollowingComment {
			id: self.id,
			author: self.author,
			body: self.body,
			permalink: self.permalink,
			created_ts: self.created_ts,
			rel_time: self.rel_time,
			community: self.community,
			score: self.score,
			parent_id: self.parent_id,
			parent_kind: self.parent_kind,
			parent_comment: self.parent_comment.into_runtime(),
			link_title: self.link_title,
			link_permalink: self.link_permalink,
			link_author: self.link_author,
			link_created_ts: self.link_created_ts,
			thread_comments: self.thread_comments,
			thread_nsfw: self.thread_nsfw,
		}
	}
}

impl PersistedCollapsedDuplicate {
	pub(crate) fn from_runtime(value: &utils::CollapsedDuplicate) -> Self {
		Self {
			title: value.title.clone(),
			permalink: value.permalink.clone(),
			community: value.community.clone(),
			display_community: value.display_community.clone(),
			created_ts: value.created_ts,
			rel_time: value.rel_time.clone(),
			comments: value.comments.clone(),
			flair: PersistedFlair::from_runtime(&value.flair),
		}
	}

	pub(crate) fn into_runtime(self) -> utils::CollapsedDuplicate {
		utils::CollapsedDuplicate {
			title: self.title,
			permalink: self.permalink,
			community: self.community,
			display_community: self.display_community,
			created_ts: self.created_ts,
			rel_time: self.rel_time,
			comments: self.comments,
			flair: self.flair.into_runtime(),
		}
	}
}

impl PersistedPost {
	pub(crate) fn from_runtime(value: &utils::Post) -> Self {
		Self {
			id: value.id.clone(),
			kind: value.kind.clone(),
			title: value.title.clone(),
			community: value.community.clone(),
			body: value.body.clone(),
			author: PersistedAuthor::from_runtime(&value.author),
			permalink: value.permalink.clone(),
			parent_id: value.parent_id.clone(),
			parent_kind: value.parent_kind.clone(),
			parent_comment: PersistedParentComment::from_runtime(&value.parent_comment),
			link_id: value.link_id.clone(),
			link_title: value.link_title.clone(),
			link_permalink: value.link_permalink.clone(),
			link_author: value.link_author.clone(),
			link_created_ts: value.link_created_ts,
			poll: value.poll.as_ref().map(PersistedPoll::from_runtime),
			score: value.score.clone(),
			upvote_ratio: value.upvote_ratio,
			post_type: value.post_type.clone(),
			flair: PersistedFlair::from_runtime(&value.flair),
			flags: PersistedFlags::from_runtime(&value.flags),
			thumbnail: PersistedMedia::from_runtime(&value.thumbnail),
			media: PersistedMedia::from_runtime(&value.media),
			domain: value.domain.clone(),
			rel_time: value.rel_time.clone(),
			created: value.created.clone(),
			created_ts: value.created_ts,
			num_duplicates: value.num_duplicates,
			comments: value.comments.clone(),
			gallery: value.gallery.iter().map(PersistedGalleryMedia::from_runtime).collect(),
			awards: value.awards.iter().map(PersistedAward::from_runtime).collect(),
			nsfw: value.nsfw,
			out_url: value.out_url.clone(),
			ws_url: value.ws_url.clone(),
			preview_body: value.preview_body.clone(),
			collapsed_duplicates: value.collapsed_duplicates.iter().map(PersistedCollapsedDuplicate::from_runtime).collect(),
			following_comments: value.following_comments.iter().map(PersistedFollowingComment::from_runtime).collect(),
		}
	}

	pub(crate) fn into_runtime(self) -> utils::Post {
		utils::Post {
			id: self.id,
			kind: self.kind,
			title: self.title,
			community: self.community,
			body: self.body,
			author: self.author.into_runtime(),
			permalink: self.permalink,
			parent_id: self.parent_id,
			parent_kind: self.parent_kind,
			parent_comment: self.parent_comment.into_runtime(),
			link_id: self.link_id,
			link_title: self.link_title,
			link_permalink: self.link_permalink,
			link_author: self.link_author,
			link_created_ts: self.link_created_ts,
			poll: self.poll.map(PersistedPoll::into_runtime),
			score: self.score,
			upvote_ratio: self.upvote_ratio,
			post_type: self.post_type,
			flair: self.flair.into_runtime(),
			flags: self.flags.into_runtime(),
			thumbnail: self.thumbnail.into_runtime(),
			media: self.media.into_runtime(),
			domain: self.domain,
			rel_time: self.rel_time,
			created: self.created,
			created_ts: self.created_ts,
			num_duplicates: self.num_duplicates,
			comments: self.comments,
			gallery: self.gallery.into_iter().map(PersistedGalleryMedia::into_runtime).collect(),
			awards: utils::Awards(self.awards.into_iter().map(PersistedAward::into_runtime).collect()),
			nsfw: self.nsfw,
			out_url: self.out_url,
			ws_url: self.ws_url,
			preview_body: self.preview_body,
			collapsed_duplicates: self.collapsed_duplicates.into_iter().map(PersistedCollapsedDuplicate::into_runtime).collect(),
			following_comments: self.following_comments.into_iter().map(PersistedFollowingComment::into_runtime).collect(),
			bookmarked: false,
			bookmark_new_comments: None,
		}
	}
}

impl PersistedSubreddit {
	pub(crate) fn from_runtime(value: &utils::Subreddit) -> Self {
		Self {
			name: value.name.clone(),
			title: value.title.clone(),
			description: value.description.clone(),
			info: value.info.clone(),
			has_sidebar: value.has_sidebar,
			icon: value.icon.clone(),
			members: value.members.clone(),
			active: value.active.clone(),
			wiki: value.wiki,
			nsfw: value.nsfw,
		}
	}

	pub(crate) fn into_runtime(self) -> utils::Subreddit {
		utils::Subreddit {
			name: self.name,
			title: self.title,
			description: self.description,
			info: self.info,
			has_sidebar: self.has_sidebar,
			icon: self.icon,
			members: self.members,
			active: self.active,
			wiki: self.wiki,
			nsfw: self.nsfw,
		}
	}
}
