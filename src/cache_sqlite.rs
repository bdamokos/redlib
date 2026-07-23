//! Feature-gated SQLite shadow cache.
//!
//! The legacy `sources.v2` files remain the write authority while this module
//! is enabled. A legacy file is written and synced first; only then is the
//! corresponding source transaction applied here. Reads remain on the legacy
//! path unless `REDLIB_CACHE_SQLITE_READS_ENABLED` is explicitly enabled.

use crate::cache_persistence::{PersistedPost, PersistedRefreshOrigin, PersistedSourceCacheEntry};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use log::{info, warn};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const DATABASE_FILE_NAME: &str = "canonical-cache.sqlite3";
const CURRENT_SCHEMA_VERSION: i64 = 1;
const MIGRATION_ORIGIN: i64 = 3;
const DAEMON_ORIGIN: i64 = 1;
const INTERACTIVE_ORIGIN: i64 = 2;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cache_meta (
    key TEXT PRIMARY KEY,
    value BLOB NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS objects (
    object_pk INTEGER PRIMARY KEY,
    fullname TEXT NOT NULL UNIQUE,
    kind INTEGER NOT NULL,
    created_utc INTEGER NOT NULL,
    link_created_utc INTEGER NOT NULL,
    author_lc TEXT NOT NULL,
    community_lc TEXT NOT NULL,
    submission_fullname TEXT,
    parent_fullname TEXT,
    post_type TEXT NOT NULL,
    duration_seconds INTEGER NOT NULL,
    is_video INTEGER NOT NULL CHECK (is_video IN (0, 1)),
    is_deleted INTEGER NOT NULL CHECK (is_deleted IN (0, 1)),
    is_promotional INTEGER NOT NULL CHECK (is_promotional IN (0, 1)),
    first_seen_origin INTEGER NOT NULL,
    first_seen_at INTEGER NOT NULL,
    discovery_seq INTEGER NOT NULL,
    discovered_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    payload_revision INTEGER NOT NULL,
    payload_codec INTEGER NOT NULL,
    payload_hash BLOB NOT NULL,
    payload BLOB NOT NULL
);

CREATE INDEX IF NOT EXISTS objects_new
    ON objects(created_utc DESC, object_pk DESC);
CREATE INDEX IF NOT EXISTS objects_submitted
    ON objects(author_lc, kind, created_utc DESC, object_pk DESC);
CREATE INDEX IF NOT EXISTS objects_videos
    ON objects(duration_seconds DESC, created_utc DESC, object_pk DESC)
    WHERE duration_seconds > 0;
CREATE INDEX IF NOT EXISTS objects_submission
    ON objects(submission_fullname, created_utc DESC, object_pk DESC);
CREATE INDEX IF NOT EXISTS objects_discovery
    ON objects(discovery_seq DESC, object_pk DESC)
    WHERE discovery_seq > 0;

CREATE TABLE IF NOT EXISTS sources (
    source_pk INTEGER PRIMARY KEY,
    cache_key TEXT NOT NULL UNIQUE,
    source_kind INTEGER NOT NULL,
    display_name TEXT NOT NULL,
    sort INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    fetched_at INTEGER NOT NULL,
    refresh_origin INTEGER,
    after_cursor TEXT NOT NULL,
    content_hash BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS source_memberships (
    source_pk INTEGER NOT NULL REFERENCES sources(source_pk) ON DELETE CASCADE,
    object_pk INTEGER NOT NULL REFERENCES objects(object_pk) ON DELETE CASCADE,
    source_position INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY (source_pk, object_pk)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS source_memberships_object
    ON source_memberships(object_pk, source_pk);

CREATE TABLE IF NOT EXISTS duplicate_groups (
    duplicate_group_pk INTEGER PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS duplicate_keys (
    duplicate_key_hash BLOB PRIMARY KEY,
    duplicate_group_pk INTEGER NOT NULL REFERENCES duplicate_groups(duplicate_group_pk)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS duplicate_members (
    duplicate_group_pk INTEGER NOT NULL REFERENCES duplicate_groups(duplicate_group_pk) ON DELETE CASCADE,
    object_pk INTEGER NOT NULL UNIQUE REFERENCES objects(object_pk) ON DELETE CASCADE,
    PRIMARY KEY (duplicate_group_pk, object_pk)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS thread_groups (
    thread_group_pk INTEGER PRIMARY KEY,
    submission_fullname TEXT NOT NULL UNIQUE,
    linked_submission_pk INTEGER REFERENCES objects(object_pk)
);

CREATE TABLE IF NOT EXISTS thread_members (
    thread_group_pk INTEGER NOT NULL REFERENCES thread_groups(thread_group_pk) ON DELETE CASCADE,
    object_pk INTEGER NOT NULL UNIQUE REFERENCES objects(object_pk) ON DELETE CASCADE,
    PRIMARY KEY (thread_group_pk, object_pk)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS active_profiles (
    profile_pk INTEGER PRIMARY KEY,
    fingerprint BLOB NOT NULL UNIQUE,
    generation INTEGER NOT NULL,
    random_epoch INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS profile_sources (
    profile_pk INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    source_pk INTEGER NOT NULL REFERENCES sources(source_pk) ON DELETE CASCADE,
    source_role INTEGER NOT NULL,
    PRIMARY KEY (profile_pk, source_pk)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS profile_sources_by_source
    ON profile_sources(source_pk, profile_pk);

CREATE TABLE IF NOT EXISTS profile_object_refs (
    profile_pk INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    object_pk INTEGER NOT NULL REFERENCES objects(object_pk) ON DELETE CASCADE,
    source_refcount INTEGER NOT NULL CHECK (source_refcount > 0),
    PRIMARY KEY (profile_pk, object_pk)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS profile_thread_entries (
    profile_pk INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    thread_group_pk INTEGER NOT NULL REFERENCES thread_groups(thread_group_pk),
    version INTEGER NOT NULL,
    valid_from_generation INTEGER NOT NULL,
    valid_to_generation INTEGER,
    representative_pk INTEGER NOT NULL REFERENCES objects(object_pk),
    created_sort INTEGER NOT NULL,
    activity_sort INTEGER NOT NULL,
    discovery_sort INTEGER NOT NULL,
    member_count INTEGER NOT NULL,
    PRIMARY KEY (profile_pk, thread_group_pk, version)
);

CREATE TABLE IF NOT EXISTS profile_feed_entries (
    profile_pk INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    view_kind INTEGER NOT NULL,
    card_key TEXT NOT NULL,
    version INTEGER NOT NULL,
    valid_from_generation INTEGER NOT NULL,
    valid_to_generation INTEGER,
    representative_pk INTEGER NOT NULL REFERENCES objects(object_pk),
    duplicate_group_pk INTEGER REFERENCES duplicate_groups(duplicate_group_pk),
    thread_group_pk INTEGER REFERENCES thread_groups(thread_group_pk),
    created_sort INTEGER NOT NULL,
    activity_sort INTEGER NOT NULL,
    discovery_sort INTEGER NOT NULL,
    duration_seconds INTEGER NOT NULL,
    community_lc TEXT NOT NULL,
    author_lc TEXT NOT NULL,
    random_epoch INTEGER NOT NULL,
    random_round INTEGER NOT NULL,
    random_rank INTEGER NOT NULL,
    member_count INTEGER NOT NULL,
    PRIMARY KEY (profile_pk, view_kind, card_key, version)
);

CREATE INDEX IF NOT EXISTS profile_feed_new
    ON profile_feed_entries(profile_pk, view_kind, created_sort DESC, card_key DESC)
    WHERE valid_to_generation IS NULL;
CREATE INDEX IF NOT EXISTS profile_feed_activity
    ON profile_feed_entries(profile_pk, view_kind, activity_sort DESC, card_key DESC)
    WHERE valid_to_generation IS NULL;
CREATE INDEX IF NOT EXISTS profile_feed_discovered
    ON profile_feed_entries(profile_pk, view_kind, discovery_sort DESC, card_key DESC)
    WHERE valid_to_generation IS NULL AND discovery_sort > 0;
CREATE INDEX IF NOT EXISTS profile_feed_random
    ON profile_feed_entries(profile_pk, view_kind, random_epoch, random_round, random_rank, card_key)
    WHERE valid_to_generation IS NULL;
CREATE INDEX IF NOT EXISTS profile_feed_videos
    ON profile_feed_entries(profile_pk, view_kind, duration_seconds DESC, created_sort DESC, card_key DESC)
    WHERE valid_to_generation IS NULL AND duration_seconds > 0;

CREATE TABLE IF NOT EXISTS migration_sources (
    cache_key TEXT PRIMARY KEY,
    source_size INTEGER NOT NULL,
    source_mtime_ns INTEGER NOT NULL,
    source_hash BLOB NOT NULL,
    imported_generation INTEGER NOT NULL,
    imported_at INTEGER NOT NULL,
    object_count INTEGER NOT NULL,
    status INTEGER NOT NULL
) WITHOUT ROWID;
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstSeen {
	Migration,
	Daemon,
	Interactive,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImportStats {
	pub scanned_sources: usize,
	pub imported_sources: usize,
	pub skipped_sources: usize,
	pub objects_seen: usize,
}

#[derive(Clone, Debug)]
struct SourceDescriptor {
	cache_key: String,
	path: PathBuf,
	size: u64,
	mtime_ns: u128,
}

struct ShadowCache {
	connection: Mutex<Connection>,
}

static SHADOW_CACHE: OnceLock<ShadowCache> = OnceLock::new();

fn env_bool(name: &str) -> bool {
	std::env::var(name)
		.ok()
		.map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
		.unwrap_or(false)
}

pub(crate) fn shadow_enabled() -> bool {
	env_bool("REDLIB_CACHE_SQLITE_SHADOW_ENABLED")
}

pub(crate) fn reads_enabled() -> bool {
	shadow_enabled() && env_bool("REDLIB_CACHE_SQLITE_READS_ENABLED") && !env_bool("REDLIB_CACHE_SQLITE_ROLLBACK_TO_LEGACY")
}

pub(crate) fn database_path(cache_dir: &Path) -> PathBuf {
	std::env::var_os("REDLIB_CACHE_SQLITE_PATH")
		.map(PathBuf::from)
		.unwrap_or_else(|| cache_dir.join(DATABASE_FILE_NAME))
}

pub(crate) fn init(cache_dir: &Path) -> Result<ImportStats, String> {
	if !shadow_enabled() {
		return Ok(ImportStats::default());
	}
	if SHADOW_CACHE.get().is_some() {
		return Ok(ImportStats::default());
	}

	let path = database_path(cache_dir);
	let mut connection = open_database(&path)?;
	let stats = import_legacy_sources(&mut connection, cache_dir)?;
	SHADOW_CACHE
		.set(ShadowCache {
			connection: Mutex::new(connection),
		})
		.map_err(|_| "SQLite shadow cache initialized concurrently".to_string())?;
	info!(
		"cache_sqlite: shadow ready at {} (scanned={}, imported={}, skipped={}, objects={})",
		path.display(),
		stats.scanned_sources,
		stats.imported_sources,
		stats.skipped_sources,
		stats.objects_seen
	);
	Ok(stats)
}

fn configured_page_size() -> Result<i64, String> {
	let value = std::env::var("REDLIB_CACHE_SQLITE_PAGE_SIZE").unwrap_or_else(|_| "8192".to_string());
	let page_size = value.parse::<i64>().map_err(|_| format!("invalid REDLIB_CACHE_SQLITE_PAGE_SIZE={value}"))?;
	if matches!(page_size, 8192 | 16384) {
		Ok(page_size)
	} else {
		Err(format!("REDLIB_CACHE_SQLITE_PAGE_SIZE must be 8192 or 16384, got {page_size}"))
	}
}

fn configured_synchronous() -> Result<&'static str, String> {
	match std::env::var("REDLIB_CACHE_SQLITE_SYNCHRONOUS")
		.unwrap_or_else(|_| "FULL".to_string())
		.trim()
		.to_ascii_uppercase()
		.as_str()
	{
		"FULL" => Ok("FULL"),
		"NORMAL" => Ok("NORMAL"),
		value => Err(format!("REDLIB_CACHE_SQLITE_SYNCHRONOUS must be FULL or NORMAL, got {value}")),
	}
}

fn open_database(path: &Path) -> Result<Connection, String> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}
	let existed = path.exists();
	let connection = Connection::open(path).map_err(|error| error.to_string())?;
	let requested_page_size = configured_page_size()?;
	if !existed {
		connection
			.execute_batch(&format!("PRAGMA page_size={requested_page_size};"))
			.map_err(|error| error.to_string())?;
	}
	let actual_page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	if actual_page_size != requested_page_size {
		return Err(format!("existing SQLite cache page size is {actual_page_size}, configured value is {requested_page_size}"));
	}
	let synchronous = configured_synchronous()?;
	connection
		.execute_batch(&format!(
			"PRAGMA journal_mode=WAL;
			 PRAGMA synchronous={synchronous};
			 PRAGMA foreign_keys=ON;
			 PRAGMA temp_store=FILE;
			 PRAGMA cache_size=-8192;
			 PRAGMA mmap_size=0;
			 PRAGMA wal_autocheckpoint=1000;"
		))
		.map_err(|error| error.to_string())?;
	connection.execute_batch(SCHEMA).map_err(|error| error.to_string())?;
	set_meta_i64(&connection, "schema_version", CURRENT_SCHEMA_VERSION)?;
	Ok(connection)
}

fn set_meta_i64(connection: &Connection, key: &str, value: i64) -> Result<(), String> {
	connection
		.execute(
			"INSERT INTO cache_meta(key,value) VALUES(?1,CAST(?2 AS BLOB))
			 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
			params![key, value.to_string()],
		)
		.map_err(|error| error.to_string())?;
	Ok(())
}

fn meta_i64(transaction: &Transaction<'_>, key: &str) -> Result<Option<i64>, String> {
	let value = transaction
		.query_row("SELECT CAST(value AS TEXT) FROM cache_meta WHERE key=?1", [key], |row| row.get::<_, String>(0))
		.optional()
		.map_err(|error| error.to_string())?;
	value
		.map(|value| value.parse::<i64>().map_err(|error| format!("invalid cache_meta {key}: {error}")))
		.transpose()
}

fn set_meta_i64_tx(transaction: &Transaction<'_>, key: &str, value: i64) -> Result<(), String> {
	transaction
		.execute(
			"INSERT INTO cache_meta(key,value) VALUES(?1,CAST(?2 AS BLOB))
			 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
			params![key, value.to_string()],
		)
		.map_err(|error| error.to_string())?;
	Ok(())
}

fn ensure_migration_baseline(transaction: &Transaction<'_>, now: u64) -> Result<(i64, i64), String> {
	let baseline_at = meta_i64(transaction, "migration_baseline_at")?;
	let baseline_seq = meta_i64(transaction, "migration_baseline_seq")?;
	match (baseline_at, baseline_seq) {
		(Some(at), Some(seq)) => Ok((at, seq)),
		(None, None) => {
			let max_seq: i64 = transaction
				.query_row("SELECT COALESCE(MAX(discovery_seq),0) FROM objects", [], |row| row.get(0))
				.map_err(|error| error.to_string())?;
			let seq = max_seq.saturating_add(1);
			set_meta_i64_tx(transaction, "migration_baseline_at", now as i64)?;
			set_meta_i64_tx(transaction, "migration_baseline_seq", seq)?;
			set_meta_i64_tx(transaction, "next_discovery_seq", seq.saturating_add(1))?;
			Ok((now as i64, seq))
		}
		_ => Err("incomplete SQLite migration baseline metadata".to_string()),
	}
}

fn source_descriptors(cache_dir: &Path) -> Result<Vec<SourceDescriptor>, String> {
	let mut descriptors = Vec::new();
	let mut seen = HashSet::new();
	for directory in [cache_dir.join("sources.v2"), cache_dir.join("sources.v1")] {
		let entries = match fs::read_dir(&directory) {
			Ok(entries) => entries,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
			Err(error) => return Err(error.to_string()),
		};
		for entry in entries {
			let entry = entry.map_err(|error| error.to_string())?;
			let path = entry.path();
			if path.extension().and_then(|value| value.to_str()) != Some("bin") {
				continue;
			}
			let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
				continue;
			};
			let Ok(decoded) = URL_SAFE_NO_PAD.decode(stem) else {
				continue;
			};
			let Ok(cache_key) = String::from_utf8(decoded) else {
				continue;
			};
			if !seen.insert(cache_key.clone()) {
				continue;
			}
			let metadata = entry.metadata().map_err(|error| error.to_string())?;
			let mtime_ns = metadata
				.modified()
				.ok()
				.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
				.map(|value| value.as_nanos())
				.unwrap_or_default();
			descriptors.push(SourceDescriptor {
				cache_key,
				path,
				size: metadata.len(),
				mtime_ns,
			});
		}
	}
	descriptors.sort_by(|left, right| left.cache_key.cmp(&right.cache_key));
	Ok(descriptors)
}

fn import_legacy_sources(connection: &mut Connection, cache_dir: &Path) -> Result<ImportStats, String> {
	let descriptors = source_descriptors(cache_dir)?;
	let mut stats = ImportStats {
		scanned_sources: descriptors.len(),
		..ImportStats::default()
	};
	for descriptor in descriptors {
		let bytes = fs::read(&descriptor.path).map_err(|error| format!("{}: {error}", descriptor.path.display()))?;
		let source_hash = sha256(&bytes);
		let unchanged = connection
			.query_row(
				"SELECT 1 FROM migration_sources
				 WHERE cache_key=?1 AND source_size=?2 AND source_mtime_ns=?3 AND source_hash=?4 AND status=1",
				params![
					descriptor.cache_key,
					descriptor.size as i64,
					i64::try_from(descriptor.mtime_ns).unwrap_or(i64::MAX),
					source_hash
				],
				|row| row.get::<_, i64>(0),
			)
			.optional()
			.map_err(|error| error.to_string())?
			.is_some();
		if unchanged {
			stats.skipped_sources += 1;
			continue;
		}
		let entry: PersistedSourceCacheEntry = bincode::deserialize(&bytes).map_err(|error| format!("{}: {error}", descriptor.path.display()))?;
		let metadata = SourceWriteMetadata {
			size: descriptor.size,
			mtime_ns: descriptor.mtime_ns,
			hash: source_hash,
			imported_at: unix_now(),
		};
		apply_source(connection, &descriptor.cache_key, &entry, &metadata, FirstSeen::Migration)?;
		stats.imported_sources += 1;
		stats.objects_seen += entry.posts.len();
	}
	Ok(stats)
}

#[derive(Clone, Debug)]
struct SourceWriteMetadata {
	size: u64,
	mtime_ns: u128,
	hash: Vec<u8>,
	imported_at: u64,
}

pub(crate) fn shadow_source_after_legacy_write(source_cache_key: &str, entry: &PersistedSourceCacheEntry, legacy_bytes: &[u8], legacy_path: &Path) {
	if !shadow_enabled() {
		return;
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		warn!("cache_sqlite: shadow write skipped because initialization did not complete");
		return;
	};
	let metadata = fs::metadata(legacy_path).ok();
	let mtime_ns = metadata
		.as_ref()
		.and_then(|value| value.modified().ok())
		.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
		.map(|value| value.as_nanos())
		.unwrap_or_default();
	let write_metadata = SourceWriteMetadata {
		size: legacy_bytes.len() as u64,
		mtime_ns,
		hash: sha256(legacy_bytes),
		imported_at: unix_now(),
	};
	let first_seen = match entry.refresh_origin {
		Some(PersistedRefreshOrigin::Daemon) => FirstSeen::Daemon,
		Some(PersistedRefreshOrigin::FrontPageSession | PersistedRefreshOrigin::DirectLiveFetch) | None => FirstSeen::Interactive,
	};
	let result = cache
		.connection
		.lock()
		.map_err(|_| "SQLite shadow connection lock poisoned".to_string())
		.and_then(|mut connection| apply_source(&mut connection, source_cache_key, entry, &write_metadata, first_seen));
	if let Err(error) = result {
		warn!("cache_sqlite: shadow write failed for {source_cache_key}: {error}");
	}
}

pub(crate) fn shadow_remove_after_legacy(source_cache_key: &str) {
	if !shadow_enabled() {
		return;
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return;
	};
	let result = cache
		.connection
		.lock()
		.map_err(|_| "SQLite shadow connection lock poisoned".to_string())
		.and_then(|mut connection| remove_source(&mut connection, source_cache_key));
	if let Err(error) = result {
		warn!("cache_sqlite: shadow removal failed for {source_cache_key}: {error}");
	}
}

fn remove_source(connection: &mut Connection, source_cache_key: &str) -> Result<(), String> {
	let transaction = connection.transaction().map_err(|error| error.to_string())?;
	reset_delta_tables(&transaction)?;
	let source_pk = transaction
		.query_row("SELECT source_pk FROM sources WHERE cache_key=?1", [source_cache_key], |row| row.get::<_, i64>(0))
		.optional()
		.map_err(|error| error.to_string())?;
	let Some(source_pk) = source_pk else {
		return transaction.commit().map_err(|error| error.to_string());
	};
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_objects(object_pk)
			 SELECT object_pk FROM source_memberships WHERE source_pk=?1",
			[source_pk],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_profiles(profile_pk)
			 SELECT profile_pk FROM profile_sources WHERE source_pk=?1",
			[source_pk],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute("DELETE FROM sources WHERE source_pk=?1", [source_pk])
		.map_err(|error| error.to_string())?;
	transaction
		.execute("DELETE FROM migration_sources WHERE cache_key=?1", [source_cache_key])
		.map_err(|error| error.to_string())?;
	refresh_affected_profiles(&transaction)?;
	transaction.commit().map_err(|error| error.to_string())
}

pub(crate) fn read_source(source_cache_key: &str) -> Result<Option<PersistedSourceCacheEntry>, String> {
	if !reads_enabled() {
		return Ok(None);
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return Err("SQLite reads enabled but shadow store is unavailable".to_string());
	};
	let connection = cache.connection.lock().map_err(|_| "SQLite shadow connection lock poisoned".to_string())?;
	let source = connection
		.query_row(
			"SELECT display_name,after_cursor,fetched_at,refresh_origin FROM sources WHERE cache_key=?1",
			[source_cache_key],
			|row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, Option<i64>>(3)?)),
		)
		.optional()
		.map_err(|error| error.to_string())?;
	let Some((subreddit, after, fetched_at, refresh_origin)) = source else {
		return Ok(None);
	};
	let mut statement = connection
		.prepare(
			"SELECT o.payload_codec,o.payload
			 FROM source_memberships sm
			 JOIN sources s ON s.source_pk=sm.source_pk
			 JOIN objects o ON o.object_pk=sm.object_pk
			 WHERE s.cache_key=?1
			 ORDER BY sm.source_position",
		)
		.map_err(|error| error.to_string())?;
	let rows = statement
		.query_map([source_cache_key], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))
		.map_err(|error| error.to_string())?;
	let mut posts = Vec::new();
	for row in rows {
		let (codec, payload) = row.map_err(|error| error.to_string())?;
		if codec != 0 {
			return Err(format!("unsupported SQLite payload codec {codec}"));
		}
		posts.push(bincode::deserialize(&payload).map_err(|error| error.to_string())?);
	}
	Ok(Some(PersistedSourceCacheEntry {
		subreddit,
		posts,
		after,
		fetched_at_unix: fetched_at.max(0) as u64,
		refresh_origin: refresh_origin.and_then(refresh_origin_from_i64),
	}))
}

fn apply_source(connection: &mut Connection, cache_key: &str, entry: &PersistedSourceCacheEntry, metadata: &SourceWriteMetadata, first_seen: FirstSeen) -> Result<(), String> {
	let transaction = connection.transaction().map_err(|error| error.to_string())?;
	reset_delta_tables(&transaction)?;
	let (baseline_at, baseline_seq) = ensure_migration_baseline(&transaction, metadata.imported_at)?;
	let source_kind = source_kind(cache_key);
	let refresh_origin = entry.refresh_origin.map(refresh_origin_to_i64);
	transaction
		.execute(
			"INSERT INTO sources(cache_key,source_kind,display_name,sort,generation,fetched_at,refresh_origin,after_cursor,content_hash)
			 VALUES(?1,?2,?3,1,1,?4,?5,?6,?7)
			 ON CONFLICT(cache_key) DO UPDATE SET
			   source_kind=excluded.source_kind,
			   display_name=excluded.display_name,
			   generation=sources.generation+1,
			   fetched_at=excluded.fetched_at,
			   refresh_origin=excluded.refresh_origin,
			   after_cursor=excluded.after_cursor,
			   content_hash=excluded.content_hash",
			params![
				cache_key,
				source_kind,
				entry.subreddit,
				entry.fetched_at_unix as i64,
				refresh_origin,
				entry.after,
				metadata.hash
			],
		)
		.map_err(|error| error.to_string())?;
	let (source_pk, generation): (i64, i64) = transaction
		.query_row("SELECT source_pk,generation FROM sources WHERE cache_key=?1", [cache_key], |row| {
			Ok((row.get(0)?, row.get(1)?))
		})
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_objects(object_pk)
			 SELECT object_pk FROM source_memberships WHERE source_pk=?1",
			[source_pk],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_profiles(profile_pk)
			 SELECT profile_pk FROM profile_sources WHERE source_pk=?1",
			[source_pk],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute("DELETE FROM source_memberships WHERE source_pk=?1", [source_pk])
		.map_err(|error| error.to_string())?;
	for (position, post) in entry.posts.iter().enumerate() {
		let object_pk = upsert_object(&transaction, post, metadata.imported_at, first_seen, baseline_at, baseline_seq)?;
		transaction
			.execute("INSERT OR IGNORE INTO temp_affected_objects(object_pk) VALUES(?1)", [object_pk])
			.map_err(|error| error.to_string())?;
		transaction
			.execute(
				"INSERT INTO source_memberships(source_pk,object_pk,source_position,generation)
				 VALUES(?1,?2,?3,?4)
				 ON CONFLICT(source_pk,object_pk) DO UPDATE SET
				   source_position=MIN(source_memberships.source_position,excluded.source_position),
				   generation=excluded.generation",
				params![source_pk, object_pk, position as i64, generation],
			)
			.map_err(|error| error.to_string())?;
	}
	refresh_affected_profiles(&transaction)?;
	transaction
		.execute(
			"INSERT INTO migration_sources(cache_key,source_size,source_mtime_ns,source_hash,imported_generation,imported_at,object_count,status)
			 VALUES(?1,?2,?3,?4,?5,?6,?7,1)
			 ON CONFLICT(cache_key) DO UPDATE SET
			   source_size=excluded.source_size,
			   source_mtime_ns=excluded.source_mtime_ns,
			   source_hash=excluded.source_hash,
			   imported_generation=excluded.imported_generation,
			   imported_at=excluded.imported_at,
			   object_count=excluded.object_count,
			   status=1",
			params![
				cache_key,
				metadata.size as i64,
				i64::try_from(metadata.mtime_ns).unwrap_or(i64::MAX),
				metadata.hash,
				generation,
				metadata.imported_at as i64,
				entry.posts.len() as i64
			],
		)
		.map_err(|error| error.to_string())?;
	transaction.commit().map_err(|error| error.to_string())
}

fn reset_delta_tables(transaction: &Transaction<'_>) -> Result<(), String> {
	transaction
		.execute_batch(
			r#"
CREATE TEMP TABLE IF NOT EXISTS temp_affected_objects(
    object_pk INTEGER PRIMARY KEY
) WITHOUT ROWID;
CREATE TEMP TABLE IF NOT EXISTS temp_affected_profiles(
    profile_pk INTEGER PRIMARY KEY
) WITHOUT ROWID;
CREATE TEMP TABLE IF NOT EXISTS temp_affected_cards(
    card_key TEXT PRIMARY KEY
) WITHOUT ROWID;
CREATE TEMP TABLE IF NOT EXISTS temp_affected_threads(
    thread_group_pk INTEGER PRIMARY KEY
) WITHOUT ROWID;
DELETE FROM temp_affected_objects;
DELETE FROM temp_affected_profiles;
DELETE FROM temp_affected_cards;
DELETE FROM temp_affected_threads;
"#,
		)
		.map_err(|error| error.to_string())
}

const OBJECT_CARD_KEY_SQL: &str = r#"
CASE
  WHEN o.kind=1 THEN COALESCE(
    (
      SELECT 'd:' || linked_dm.duplicate_group_pk
      FROM thread_members linked_tm
      JOIN thread_groups linked_tg ON linked_tg.thread_group_pk=linked_tm.thread_group_pk
      JOIN duplicate_members linked_dm ON linked_dm.object_pk=linked_tg.linked_submission_pk
      WHERE linked_tm.object_pk=o.object_pk
    ),
    (
      SELECT 't:' || own_tm.thread_group_pk
      FROM thread_members own_tm
      WHERE own_tm.object_pk=o.object_pk
    )
  )
  ELSE (
    SELECT 'd:' || own_dm.duplicate_group_pk
    FROM duplicate_members own_dm
    WHERE own_dm.object_pk=o.object_pk
  )
END
"#;

fn refresh_affected_profiles(transaction: &Transaction<'_>) -> Result<(), String> {
	let affected_profile_count: i64 = transaction
		.query_row("SELECT COUNT(*) FROM temp_affected_profiles", [], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	if affected_profile_count == 0 {
		return Ok(());
	}
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_cards(card_key)
			 SELECT DISTINCT CASE
			   WHEN o.kind=1 THEN COALESCE(
			     (SELECT 'd:' || linked_dm.duplicate_group_pk
			      FROM thread_members linked_tm
			      JOIN thread_groups linked_tg ON linked_tg.thread_group_pk=linked_tm.thread_group_pk
			      JOIN duplicate_members linked_dm ON linked_dm.object_pk=linked_tg.linked_submission_pk
			      WHERE linked_tm.object_pk=o.object_pk),
			     (SELECT 't:' || own_tm.thread_group_pk FROM thread_members own_tm WHERE own_tm.object_pk=o.object_pk)
			   )
			   ELSE (SELECT 'd:' || own_dm.duplicate_group_pk FROM duplicate_members own_dm WHERE own_dm.object_pk=o.object_pk)
			 END
			 FROM temp_affected_objects changed
			 JOIN objects o ON o.object_pk=changed.object_pk",
			[],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_threads(thread_group_pk)
			 SELECT tm.thread_group_pk
			 FROM temp_affected_objects changed
			 JOIN thread_members tm ON tm.object_pk=changed.object_pk",
			[],
		)
		.map_err(|error| error.to_string())?;

	transaction
		.execute(
			"DELETE FROM profile_object_refs
			 WHERE profile_pk IN (SELECT profile_pk FROM temp_affected_profiles)
			   AND object_pk IN (SELECT object_pk FROM temp_affected_objects)",
			[],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"INSERT INTO profile_object_refs(profile_pk,object_pk,source_refcount)
			 SELECT affected.profile_pk,sm.object_pk,COUNT(DISTINCT ps.source_pk)
			 FROM temp_affected_profiles affected
			 JOIN profile_sources ps ON ps.profile_pk=affected.profile_pk
			 JOIN source_memberships sm ON sm.source_pk=ps.source_pk
			 JOIN temp_affected_objects changed ON changed.object_pk=sm.object_pk
			 GROUP BY affected.profile_pk,sm.object_pk",
			[],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"UPDATE active_profiles
			 SET generation=generation+1,last_accessed_at=?1
			 WHERE profile_pk IN (SELECT profile_pk FROM temp_affected_profiles)",
			[unix_now() as i64],
		)
		.map_err(|error| error.to_string())?;

	close_and_insert_feed_entries(transaction)?;
	close_and_insert_thread_entries(transaction)
}

fn close_and_insert_feed_entries(transaction: &Transaction<'_>) -> Result<(), String> {
	transaction
		.execute(
			"UPDATE profile_feed_entries
			 SET valid_to_generation=(
			   SELECT generation FROM active_profiles p WHERE p.profile_pk=profile_feed_entries.profile_pk
			 )
			 WHERE valid_to_generation IS NULL
			   AND profile_pk IN (SELECT profile_pk FROM temp_affected_profiles)
			   AND card_key IN (SELECT card_key FROM temp_affected_cards)",
			[],
		)
		.map_err(|error| error.to_string())?;

	let member_card_key_sql = OBJECT_CARD_KEY_SQL.replace("o.", "member.");
	let sql = format!(
		r#"
WITH eligible AS (
  SELECT DISTINCT affected.profile_pk,views.view_kind,
         o.object_pk,o.kind,o.created_utc,o.link_created_utc,o.discovery_seq,
         o.duration_seconds,o.community_lc,o.author_lc,
         {OBJECT_CARD_KEY_SQL} AS card_key,
         (SELECT dm.duplicate_group_pk FROM duplicate_members dm WHERE dm.object_pk=o.object_pk) AS duplicate_group_pk,
         (SELECT tm.thread_group_pk FROM thread_members tm WHERE tm.object_pk=o.object_pk) AS thread_group_pk
  FROM temp_affected_profiles affected
  JOIN profile_sources ps ON ps.profile_pk=affected.profile_pk
  JOIN source_memberships sm ON sm.source_pk=ps.source_pk
  JOIN objects o ON o.object_pk=sm.object_pk
  CROSS JOIN (SELECT 1 AS view_kind UNION ALL SELECT 2) views
  WHERE (views.view_kind=1 AND ps.source_role IN (0,1))
     OR (views.view_kind=2 AND ps.source_role IN (1,2))
),
affected_eligible AS (
  SELECT eligible.*
  FROM eligible
  JOIN temp_affected_cards cards ON cards.card_key=eligible.card_key
),
card_stats AS (
  SELECT profile_pk,view_kind,card_key,
         MAX(CASE WHEN kind=3 THEN duplicate_group_pk END) AS duplicate_group_pk,
         MAX(thread_group_pk) AS thread_group_pk,
         MAX(CASE WHEN kind=3 THEN created_utc ELSE link_created_utc END) AS created_sort,
         MAX(created_utc) AS activity_sort,
         MAX(discovery_seq) AS discovery_sort,
         MAX(duration_seconds) AS duration_seconds,
         COUNT(*) AS member_count
  FROM affected_eligible
  GROUP BY profile_pk,view_kind,card_key
),
chosen AS (
  SELECT stats.*,
         (
           SELECT candidate.object_pk
           FROM affected_eligible candidate
           WHERE candidate.profile_pk=stats.profile_pk
             AND candidate.view_kind=stats.view_kind
             AND candidate.card_key=stats.card_key
           ORDER BY candidate.kind DESC,candidate.created_utc DESC,candidate.object_pk DESC
           LIMIT 1
         ) AS representative_pk
  FROM card_stats stats
),
expanded AS (
  SELECT chosen.*,representative.community_lc,representative.author_lc,
         COALESCE((
           SELECT MIN(sm.source_position)
           FROM profile_sources ps
           JOIN source_memberships sm ON sm.source_pk=ps.source_pk
           JOIN objects member ON member.object_pk=sm.object_pk
           WHERE ps.profile_pk=chosen.profile_pk
             AND ((chosen.view_kind=1 AND ps.source_role IN (0,1))
               OR (chosen.view_kind=2 AND ps.source_role IN (1,2)))
             AND ({member_card_key_sql})=chosen.card_key
         ),0) AS random_round
  FROM chosen
  JOIN objects representative ON representative.object_pk=chosen.representative_pk
)
INSERT INTO profile_feed_entries(
  profile_pk,view_kind,card_key,version,valid_from_generation,valid_to_generation,
  representative_pk,duplicate_group_pk,thread_group_pk,created_sort,activity_sort,
  discovery_sort,duration_seconds,community_lc,author_lc,random_epoch,random_round,
  random_rank,member_count)
SELECT expanded.profile_pk,expanded.view_kind,expanded.card_key,
       COALESCE((
         SELECT MAX(old.version)+1
         FROM profile_feed_entries old
         WHERE old.profile_pk=expanded.profile_pk
           AND old.view_kind=expanded.view_kind
           AND old.card_key=expanded.card_key
       ),1),
       profiles.generation,NULL,expanded.representative_pk,expanded.duplicate_group_pk,
       expanded.thread_group_pk,expanded.created_sort,expanded.activity_sort,
       expanded.discovery_sort,expanded.duration_seconds,expanded.community_lc,
       expanded.author_lc,profiles.random_epoch,expanded.random_round,
       ABS((expanded.representative_pk*1103515245 + expanded.created_sort*12345) % 9223372036854775807),
       expanded.member_count
FROM expanded
JOIN active_profiles profiles ON profiles.profile_pk=expanded.profile_pk
"#
	);
	transaction.execute(&sql, []).map_err(|error| error.to_string())?;
	Ok(())
}

fn close_and_insert_thread_entries(transaction: &Transaction<'_>) -> Result<(), String> {
	transaction
		.execute(
			"UPDATE profile_thread_entries
			 SET valid_to_generation=(
			   SELECT generation FROM active_profiles p WHERE p.profile_pk=profile_thread_entries.profile_pk
			 )
			 WHERE valid_to_generation IS NULL
			   AND profile_pk IN (SELECT profile_pk FROM temp_affected_profiles)
			   AND thread_group_pk IN (SELECT thread_group_pk FROM temp_affected_threads)",
			[],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			r#"
WITH eligible AS (
  SELECT DISTINCT affected.profile_pk,tm.thread_group_pk,o.object_pk,o.kind,o.created_utc,
         o.link_created_utc,o.discovery_seq
  FROM temp_affected_profiles affected
  JOIN profile_sources ps ON ps.profile_pk=affected.profile_pk AND ps.source_role IN (1,2)
  JOIN source_memberships sm ON sm.source_pk=ps.source_pk
  JOIN thread_members tm ON tm.object_pk=sm.object_pk
  JOIN temp_affected_threads changed ON changed.thread_group_pk=tm.thread_group_pk
  JOIN objects o ON o.object_pk=sm.object_pk
),
stats AS (
  SELECT profile_pk,thread_group_pk,
         MAX(CASE WHEN kind=3 THEN created_utc ELSE link_created_utc END) AS created_sort,
         MAX(created_utc) AS activity_sort,
         MAX(discovery_seq) AS discovery_sort,
         COUNT(*) AS member_count
  FROM eligible
  GROUP BY profile_pk,thread_group_pk
),
chosen AS (
  SELECT stats.*,
         (
           SELECT candidate.object_pk
           FROM eligible candidate
           WHERE candidate.profile_pk=stats.profile_pk
             AND candidate.thread_group_pk=stats.thread_group_pk
           ORDER BY candidate.kind DESC,candidate.created_utc DESC,candidate.object_pk DESC
           LIMIT 1
         ) AS representative_pk
  FROM stats
)
INSERT INTO profile_thread_entries(
  profile_pk,thread_group_pk,version,valid_from_generation,valid_to_generation,
  representative_pk,created_sort,activity_sort,discovery_sort,member_count)
SELECT chosen.profile_pk,chosen.thread_group_pk,
       COALESCE((
         SELECT MAX(old.version)+1
         FROM profile_thread_entries old
         WHERE old.profile_pk=chosen.profile_pk
           AND old.thread_group_pk=chosen.thread_group_pk
       ),1),
       profiles.generation,NULL,chosen.representative_pk,chosen.created_sort,
       chosen.activity_sort,chosen.discovery_sort,chosen.member_count
FROM chosen
JOIN active_profiles profiles ON profiles.profile_pk=chosen.profile_pk
"#,
			[],
		)
		.map_err(|error| error.to_string())?;
	Ok(())
}

pub(crate) fn sync_active_profile(fingerprint: &[u8], sources: &[(String, i64)]) -> Result<(), String> {
	if !shadow_enabled() {
		return Ok(());
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return Err("SQLite shadow store is unavailable".to_string());
	};
	let mut connection = cache.connection.lock().map_err(|_| "SQLite shadow connection lock poisoned".to_string())?;
	sync_active_profile_on(&mut connection, fingerprint, sources)
}

fn sync_active_profile_on(connection: &mut Connection, fingerprint: &[u8], sources: &[(String, i64)]) -> Result<(), String> {
	let transaction = connection.transaction().map_err(|error| error.to_string())?;
	reset_delta_tables(&transaction)?;
	transaction
		.execute(
			"INSERT INTO active_profiles(fingerprint,generation,random_epoch,last_accessed_at)
			 VALUES(?1,0,1,?2)
			 ON CONFLICT(fingerprint) DO UPDATE SET last_accessed_at=excluded.last_accessed_at",
			params![fingerprint, unix_now() as i64],
		)
		.map_err(|error| error.to_string())?;
	let profile_pk: i64 = transaction
		.query_row("SELECT profile_pk FROM active_profiles WHERE fingerprint=?1", [fingerprint], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	transaction
		.execute("INSERT INTO temp_affected_profiles(profile_pk) VALUES(?1)", [profile_pk])
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_objects(object_pk)
			 SELECT sm.object_pk
			 FROM profile_sources ps
			 JOIN source_memberships sm ON sm.source_pk=ps.source_pk
			 WHERE ps.profile_pk=?1",
			[profile_pk],
		)
		.map_err(|error| error.to_string())?;

	transaction
		.execute_batch(
			"CREATE TEMP TABLE IF NOT EXISTS temp_requested_sources(
			   source_pk INTEGER PRIMARY KEY,
			   source_role INTEGER NOT NULL
			 ) WITHOUT ROWID;
			 DELETE FROM temp_requested_sources;",
		)
		.map_err(|error| error.to_string())?;
	for (cache_key, source_role) in sources {
		transaction
			.execute(
				"INSERT INTO sources(
				   cache_key,source_kind,display_name,sort,generation,fetched_at,
				   refresh_origin,after_cursor,content_hash)
				 VALUES(?1,?2,?1,1,0,0,NULL,'',X'')
				 ON CONFLICT(cache_key) DO NOTHING",
				params![cache_key, source_kind(cache_key)],
			)
			.map_err(|error| error.to_string())?;
		transaction
			.execute(
				"INSERT OR REPLACE INTO temp_requested_sources(source_pk,source_role)
				 SELECT source_pk,?2 FROM sources WHERE cache_key=?1",
				params![cache_key, source_role],
			)
			.map_err(|error| error.to_string())?;
	}
	let changed_source_count: i64 = transaction
		.query_row(
			"SELECT
			   (SELECT COUNT(*) FROM profile_sources existing
			    LEFT JOIN temp_requested_sources requested
			      ON requested.source_pk=existing.source_pk
			     AND requested.source_role=existing.source_role
			    WHERE existing.profile_pk=?1 AND requested.source_pk IS NULL)
			 + (SELECT COUNT(*) FROM temp_requested_sources requested
			    LEFT JOIN profile_sources existing
			      ON existing.profile_pk=?1
			     AND existing.source_pk=requested.source_pk
			     AND existing.source_role=requested.source_role
			    WHERE existing.source_pk IS NULL)",
			[profile_pk],
			|row| row.get(0),
		)
		.map_err(|error| error.to_string())?;
	if changed_source_count == 0 {
		return transaction.commit().map_err(|error| error.to_string());
	}
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_objects(object_pk)
			 SELECT sm.object_pk
			 FROM temp_requested_sources requested
			 JOIN source_memberships sm ON sm.source_pk=requested.source_pk",
			[],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute("DELETE FROM profile_sources WHERE profile_pk=?1", [profile_pk])
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"INSERT INTO profile_sources(profile_pk,source_pk,source_role)
			 SELECT ?1,source_pk,source_role FROM temp_requested_sources",
			[profile_pk],
		)
		.map_err(|error| error.to_string())?;
	refresh_affected_profiles(&transaction)?;
	transaction.commit().map_err(|error| error.to_string())
}

fn upsert_object(transaction: &Transaction<'_>, post: &PersistedPost, observed_at: u64, first_seen: FirstSeen, baseline_at: i64, baseline_seq: i64) -> Result<i64, String> {
	let fullname = fullname(post);
	let payload = bincode::serialize(post).map_err(|error| error.to_string())?;
	let payload_hash = sha256(&payload);
	let existing = transaction
		.query_row("SELECT object_pk FROM objects WHERE fullname=?1", [&fullname], |row| row.get::<_, i64>(0))
		.optional()
		.map_err(|error| error.to_string())?;
	let kind = post_kind(post);
	let submission_fullname = submission_fullname(post);
	let parent_fullname = normalized_fullname(&post.parent_id, if post.parent_kind == "t3" { 3 } else { 1 });
	let is_video = i64::from(matches!(post.post_type.as_str(), "gif" | "video") && post.media.duration_seconds > 0);
	let is_deleted = i64::from(post.author.name == "[deleted]" || (post.body == "[deleted]" && post.title == "[deleted]"));
	let is_promotional = i64::from(crate::promo_detection::post_is_promotional(&post.id));
	if let Some(object_pk) = existing {
		transaction
			.execute(
				"UPDATE objects SET
				   kind=?2,created_utc=?3,link_created_utc=?4,author_lc=?5,community_lc=?6,
				   submission_fullname=?7,parent_fullname=?8,post_type=?9,duration_seconds=?10,
				   is_video=?11,is_deleted=?12,is_promotional=?13,last_seen_at=MAX(last_seen_at,?14),
				   payload_revision=payload_revision+1,payload_codec=0,payload_hash=?15,payload=?16
				 WHERE object_pk=?1",
				params![
					object_pk,
					kind,
					post.created_ts as i64,
					post.link_created_ts as i64,
					post.author.name.trim().to_ascii_lowercase(),
					post.community.trim().to_ascii_lowercase(),
					submission_fullname,
					parent_fullname,
					post.post_type,
					post.media.duration_seconds,
					is_video,
					is_deleted,
					is_promotional,
					observed_at as i64,
					payload_hash,
					payload
				],
			)
			.map_err(|error| error.to_string())?;
		maintain_relationships(transaction, object_pk, post)?;
		return Ok(object_pk);
	}
	let (origin, first_seen_at, discovery_seq, discovered_at) = match first_seen {
		FirstSeen::Migration => (MIGRATION_ORIGIN, baseline_at, baseline_seq, baseline_at),
		FirstSeen::Interactive => (INTERACTIVE_ORIGIN, observed_at as i64, 0, 0),
		FirstSeen::Daemon => {
			let sequence = meta_i64(transaction, "next_discovery_seq")?.unwrap_or_else(|| baseline_seq.saturating_add(1));
			set_meta_i64_tx(transaction, "next_discovery_seq", sequence.saturating_add(1))?;
			(DAEMON_ORIGIN, observed_at as i64, sequence, observed_at as i64)
		}
	};
	transaction
		.execute(
			"INSERT INTO objects(
			   fullname,kind,created_utc,link_created_utc,author_lc,community_lc,submission_fullname,parent_fullname,
			   post_type,duration_seconds,is_video,is_deleted,is_promotional,first_seen_origin,first_seen_at,
			   discovery_seq,discovered_at,last_seen_at,payload_revision,payload_codec,payload_hash,payload)
			 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,1,0,?19,?20)",
			params![
				fullname,
				kind,
				post.created_ts as i64,
				post.link_created_ts as i64,
				post.author.name.trim().to_ascii_lowercase(),
				post.community.trim().to_ascii_lowercase(),
				submission_fullname,
				parent_fullname,
				post.post_type,
				post.media.duration_seconds,
				is_video,
				is_deleted,
				is_promotional,
				origin,
				first_seen_at,
				discovery_seq,
				discovered_at,
				observed_at as i64,
				payload_hash,
				payload
			],
		)
		.map_err(|error| error.to_string())?;
	let object_pk = transaction.last_insert_rowid();
	maintain_relationships(transaction, object_pk, post)?;
	Ok(object_pk)
}

fn maintain_relationships(transaction: &Transaction<'_>, object_pk: i64, post: &PersistedPost) -> Result<(), String> {
	let runtime = post.clone().into_runtime();
	let mut duplicate_key_hashes = crate::user::post_merge_keys_attr(&runtime)
		.split('\t')
		.filter(|value| !value.trim().is_empty())
		.map(|value| sha256(value.as_bytes()))
		.collect::<Vec<_>>();
	if duplicate_key_hashes.is_empty() {
		duplicate_key_hashes.push(sha256(fullname(post).as_bytes()));
	}
	duplicate_key_hashes.sort();
	duplicate_key_hashes.dedup();

	let mut group_ids = Vec::new();
	if let Some(group_id) = transaction
		.query_row("SELECT duplicate_group_pk FROM duplicate_members WHERE object_pk=?1", [object_pk], |row| {
			row.get::<_, i64>(0)
		})
		.optional()
		.map_err(|error| error.to_string())?
	{
		group_ids.push(group_id);
	}
	for key_hash in &duplicate_key_hashes {
		if let Some(group_id) = transaction
			.query_row("SELECT duplicate_group_pk FROM duplicate_keys WHERE duplicate_key_hash=?1", [key_hash], |row| {
				row.get::<_, i64>(0)
			})
			.optional()
			.map_err(|error| error.to_string())?
		{
			group_ids.push(group_id);
		}
	}
	group_ids.sort_unstable();
	group_ids.dedup();
	let target_group = if let Some(group_id) = group_ids.first().copied() {
		group_id
	} else {
		transaction.execute("INSERT INTO duplicate_groups DEFAULT VALUES", []).map_err(|error| error.to_string())?;
		transaction.last_insert_rowid()
	};
	for merged_group in group_ids.into_iter().filter(|group_id| *group_id != target_group) {
		transaction
			.execute(
				"INSERT OR IGNORE INTO temp_affected_cards(card_key) VALUES('d:' || ?1),('d:' || ?2)",
				params![merged_group, target_group],
			)
			.map_err(|error| error.to_string())?;
		transaction
			.execute(
				"UPDATE duplicate_keys SET duplicate_group_pk=?1 WHERE duplicate_group_pk=?2",
				params![target_group, merged_group],
			)
			.map_err(|error| error.to_string())?;
		transaction
			.execute(
				"UPDATE duplicate_members SET duplicate_group_pk=?1 WHERE duplicate_group_pk=?2",
				params![target_group, merged_group],
			)
			.map_err(|error| error.to_string())?;
		transaction
			.execute(
				"UPDATE profile_feed_entries SET duplicate_group_pk=?1 WHERE duplicate_group_pk=?2",
				params![target_group, merged_group],
			)
			.map_err(|error| error.to_string())?;
		transaction
			.execute("DELETE FROM duplicate_groups WHERE duplicate_group_pk=?1", [merged_group])
			.map_err(|error| error.to_string())?;
	}
	for key_hash in duplicate_key_hashes {
		transaction
			.execute(
				"INSERT INTO duplicate_keys(duplicate_key_hash,duplicate_group_pk) VALUES(?1,?2)
				 ON CONFLICT(duplicate_key_hash) DO UPDATE SET duplicate_group_pk=excluded.duplicate_group_pk",
				params![key_hash, target_group],
			)
			.map_err(|error| error.to_string())?;
	}
	transaction
		.execute(
			"INSERT INTO duplicate_members(duplicate_group_pk,object_pk) VALUES(?1,?2)
			 ON CONFLICT(object_pk) DO UPDATE SET duplicate_group_pk=excluded.duplicate_group_pk",
			params![target_group, object_pk],
		)
		.map_err(|error| error.to_string())?;

	if let Some(submission_fullname) = submission_fullname(post) {
		transaction
			.execute(
				"INSERT INTO thread_groups(submission_fullname,linked_submission_pk)
				 VALUES(?1,CASE WHEN ?2=3 THEN ?3 ELSE NULL END)
				 ON CONFLICT(submission_fullname) DO UPDATE SET
				   linked_submission_pk=COALESCE(thread_groups.linked_submission_pk,excluded.linked_submission_pk)",
				params![submission_fullname, post_kind(post), object_pk],
			)
			.map_err(|error| error.to_string())?;
		let thread_group_pk: i64 = transaction
			.query_row("SELECT thread_group_pk FROM thread_groups WHERE submission_fullname=?1", [submission_fullname], |row| {
				row.get(0)
			})
			.map_err(|error| error.to_string())?;
		transaction
			.execute(
				"INSERT INTO thread_members(thread_group_pk,object_pk) VALUES(?1,?2)
				 ON CONFLICT(object_pk) DO UPDATE SET thread_group_pk=excluded.thread_group_pk",
				params![thread_group_pk, object_pk],
			)
			.map_err(|error| error.to_string())?;
	}
	Ok(())
}

fn source_kind(cache_key: &str) -> i64 {
	if cache_key.starts_with("user_comments:") {
		2
	} else if cache_key.starts_with("flair:") {
		3
	} else if cache_key.starts_with("u_") {
		1
	} else {
		0
	}
}

fn post_kind(post: &PersistedPost) -> i64 {
	if post.kind == "t1" || post.post_type == "comment" {
		1
	} else {
		3
	}
}

fn normalized_fullname(value: &str, kind: i64) -> Option<String> {
	let value = value.trim();
	if value.is_empty() {
		None
	} else if value.starts_with("t1_") || value.starts_with("t3_") {
		Some(value.to_ascii_lowercase())
	} else {
		Some(format!("t{kind}_{}", value.to_ascii_lowercase()))
	}
}

fn fullname(post: &PersistedPost) -> String {
	normalized_fullname(&post.id, post_kind(post)).unwrap_or_else(|| {
		let digest = sha256(post.permalink.as_bytes());
		format!("synthetic_{}", hex_prefix(&digest))
	})
}

fn submission_fullname(post: &PersistedPost) -> Option<String> {
	if post_kind(post) == 3 {
		normalized_fullname(&post.id, 3)
	} else if !post.link_id.trim().is_empty() {
		normalized_fullname(&post.link_id, 3)
	} else if post.parent_kind == "t3" {
		normalized_fullname(&post.parent_id, 3)
	} else {
		None
	}
}

fn refresh_origin_to_i64(origin: PersistedRefreshOrigin) -> i64 {
	match origin {
		PersistedRefreshOrigin::Daemon => DAEMON_ORIGIN,
		PersistedRefreshOrigin::FrontPageSession => 4,
		PersistedRefreshOrigin::DirectLiveFetch => INTERACTIVE_ORIGIN,
	}
}

fn refresh_origin_from_i64(origin: i64) -> Option<PersistedRefreshOrigin> {
	match origin {
		DAEMON_ORIGIN => Some(PersistedRefreshOrigin::Daemon),
		4 => Some(PersistedRefreshOrigin::FrontPageSession),
		INTERACTIVE_ORIGIN => Some(PersistedRefreshOrigin::DirectLiveFetch),
		_ => None,
	}
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
	Sha256::digest(bytes).to_vec()
}

fn hex_prefix(bytes: &[u8]) -> String {
	bytes.iter().take(12).map(|byte| format!("{byte:02x}")).collect()
}

fn unix_now() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::cache_persistence::{PersistedAuthor, PersistedFlags, PersistedFlair, PersistedMedia, PersistedParentComment};

	fn empty_flair() -> PersistedFlair {
		PersistedFlair {
			flair_parts: Vec::new(),
			text: String::new(),
			background_color: String::new(),
			foreground_color: String::new(),
		}
	}

	fn empty_media() -> PersistedMedia {
		PersistedMedia {
			url: String::new(),
			preview_url: String::new(),
			preview_srcset: String::new(),
			content_fingerprint: String::new(),
			hls_url: String::new(),
			dash_url: String::new(),
			width: 0,
			height: 0,
			poster: String::new(),
			hd_poster: String::new(),
			duration_seconds: 0,
			duration: String::new(),
			download_name: String::new(),
			embed_code: String::new(),
			video_preview_enabled: false,
			video_preview_job_hint: String::new(),
			video_preview_fallback_url: String::new(),
			redgifs_username: String::new(),
			redgifs_created_at_unix: 0,
		}
	}

	fn post(id: &str, created_ts: u64) -> PersistedPost {
		PersistedPost {
			id: id.to_string(),
			kind: "t3".to_string(),
			title: id.to_string(),
			community: "rust".to_string(),
			body: String::new(),
			author: PersistedAuthor {
				name: "alice".to_string(),
				flair: empty_flair(),
				distinguished: String::new(),
				external_links: Vec::new(),
				avg_video_length: None,
			},
			permalink: format!("/r/rust/comments/{id}"),
			parent_id: String::new(),
			parent_kind: String::new(),
			parent_comment: PersistedParentComment::default(),
			link_id: String::new(),
			link_title: String::new(),
			link_permalink: String::new(),
			link_author: String::new(),
			link_created_ts: 0,
			poll: None,
			score: ("1".to_string(), "1".to_string()),
			upvote_ratio: 100,
			post_type: "text".to_string(),
			flair: empty_flair(),
			flags: PersistedFlags {
				spoiler: false,
				nsfw: false,
				stickied: false,
			},
			thumbnail: empty_media(),
			media: empty_media(),
			domain: "self.rust".to_string(),
			rel_time: String::new(),
			created: String::new(),
			created_ts,
			num_duplicates: 0,
			comments: ("0".to_string(), "0".to_string()),
			gallery: Vec::new(),
			awards: Vec::new(),
			nsfw: false,
			out_url: None,
			ws_url: String::new(),
			preview_body: String::new(),
			collapsed_duplicates: Vec::new(),
			following_comments: Vec::new(),
		}
	}

	fn source(posts: Vec<PersistedPost>, origin: Option<PersistedRefreshOrigin>) -> PersistedSourceCacheEntry {
		PersistedSourceCacheEntry {
			subreddit: "rust".to_string(),
			posts,
			after: "next".to_string(),
			fetched_at_unix: 1_000,
			refresh_origin: origin,
		}
	}

	fn comment(id: &str, author: &str, submission_id: &str, created_ts: u64) -> PersistedPost {
		let mut value = post(id, created_ts);
		value.kind = "t1".to_string();
		value.post_type = "comment".to_string();
		value.author.name = author.to_string();
		value.link_id = submission_id.to_string();
		value.link_created_ts = created_ts.saturating_sub(100);
		value.parent_id = submission_id.to_string();
		value.parent_kind = "t3".to_string();
		value
	}

	fn test_connection() -> Connection {
		let connection = Connection::open_in_memory().unwrap();
		connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
		connection.execute_batch(SCHEMA).unwrap();
		connection
	}

	fn metadata(hash_seed: &[u8], imported_at: u64) -> SourceWriteMetadata {
		SourceWriteMetadata {
			size: hash_seed.len() as u64,
			mtime_ns: imported_at as u128,
			hash: sha256(hash_seed),
			imported_at,
		}
	}

	#[test]
	fn migration_uses_one_baseline_and_is_idempotent() {
		let mut connection = test_connection();
		let first = source(vec![post("a", 10), post("b", 20)], None);
		let second = source(vec![post("b", 20), post("c", 30)], None);
		apply_source(&mut connection, "rust", &first, &metadata(b"first", 100), FirstSeen::Migration).unwrap();
		apply_source(&mut connection, "programming", &second, &metadata(b"second", 200), FirstSeen::Migration).unwrap();
		let distinct: i64 = connection.query_row("SELECT COUNT(DISTINCT discovery_seq) FROM objects", [], |row| row.get(0)).unwrap();
		assert_eq!(distinct, 1);
		let objects_before: i64 = connection.query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0)).unwrap();
		apply_source(&mut connection, "rust", &first, &metadata(b"first", 100), FirstSeen::Migration).unwrap();
		let objects_after: i64 = connection.query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0)).unwrap();
		assert_eq!(objects_before, objects_after);
		let first_seen: i64 = connection
			.query_row("SELECT first_seen_at FROM objects WHERE fullname='t3_a'", [], |row| row.get(0))
			.unwrap();
		assert_eq!(first_seen, 100);
	}

	#[test]
	fn daemon_discovery_is_monotonic_and_interactive_stays_zero() {
		let mut connection = test_connection();
		apply_source(
			&mut connection,
			"rust",
			&source(vec![post("old", 1)], None),
			&metadata(b"migration", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		apply_source(
			&mut connection,
			"u_alice",
			&source(vec![post("interactive", 2)], Some(PersistedRefreshOrigin::DirectLiveFetch)),
			&metadata(b"interactive", 200),
			FirstSeen::Interactive,
		)
		.unwrap();
		apply_source(
			&mut connection,
			"u_bob",
			&source(vec![post("daemon-a", 3), post("daemon-b", 4)], Some(PersistedRefreshOrigin::Daemon)),
			&metadata(b"daemon", 300),
			FirstSeen::Daemon,
		)
		.unwrap();
		let interactive: i64 = connection
			.query_row("SELECT discovery_seq FROM objects WHERE fullname='t3_interactive'", [], |row| row.get(0))
			.unwrap();
		assert_eq!(interactive, 0);
		let sequences = connection
			.prepare("SELECT discovery_seq FROM objects WHERE fullname LIKE 't3_daemon-%' ORDER BY object_pk")
			.unwrap()
			.query_map([], |row| row.get::<_, i64>(0))
			.unwrap()
			.collect::<Result<Vec<_>, _>>()
			.unwrap();
		assert_eq!(sequences, vec![2, 3]);
	}

	#[test]
	fn source_replacement_keeps_shared_object_and_exact_memberships() {
		let mut connection = test_connection();
		let shared = post("shared", 10);
		apply_source(
			&mut connection,
			"rust",
			&source(vec![shared.clone(), post("rust-only", 11)], None),
			&metadata(b"rust-1", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		apply_source(
			&mut connection,
			"programming",
			&source(vec![shared, post("programming-only", 12)], None),
			&metadata(b"programming", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		apply_source(
			&mut connection,
			"rust",
			&source(vec![post("rust-new", 13)], Some(PersistedRefreshOrigin::Daemon)),
			&metadata(b"rust-2", 200),
			FirstSeen::Daemon,
		)
		.unwrap();
		let shared_memberships: i64 = connection
			.query_row(
				"SELECT COUNT(*) FROM source_memberships sm JOIN objects o ON o.object_pk=sm.object_pk WHERE o.fullname='t3_shared'",
				[],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(shared_memberships, 1);
		let rust_memberships: i64 = connection
			.query_row(
				"SELECT COUNT(*) FROM source_memberships sm JOIN sources s ON s.source_pk=sm.source_pk WHERE s.cache_key='rust'",
				[],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(rust_memberships, 1);
	}

	#[test]
	fn profile_delta_materializes_duplicate_cards_without_full_rebuild() {
		let mut connection = test_connection();
		let mut first = post("first", 10);
		first.post_type = "image".to_string();
		first.media.content_fingerprint = "same-image".to_string();
		let mut duplicate = post("duplicate", 20);
		duplicate.post_type = "image".to_string();
		duplicate.media.content_fingerprint = "same-image".to_string();
		apply_source(
			&mut connection,
			"rust",
			&source(vec![first, duplicate], None),
			&metadata(b"rust-1", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		sync_active_profile_on(&mut connection, b"profile-a", &[("rust".to_string(), 0)]).unwrap();
		let (current_cards, member_count): (i64, i64) = connection
			.query_row(
				"SELECT COUNT(*),MAX(member_count)
				 FROM profile_feed_entries
				 WHERE view_kind=1 AND valid_to_generation IS NULL",
				[],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.unwrap();
		assert_eq!(current_cards, 1);
		assert_eq!(member_count, 2);

		apply_source(
			&mut connection,
			"rust",
			&source(vec![post("replacement", 30)], Some(PersistedRefreshOrigin::Daemon)),
			&metadata(b"rust-2", 200),
			FirstSeen::Daemon,
		)
		.unwrap();
		let current_ids = connection
			.prepare(
				"SELECT o.fullname
				 FROM profile_feed_entries feed
				 JOIN objects o ON o.object_pk=feed.representative_pk
				 WHERE feed.view_kind=1 AND feed.valid_to_generation IS NULL",
			)
			.unwrap()
			.query_map([], |row| row.get::<_, String>(0))
			.unwrap()
			.collect::<Result<Vec<_>, _>>()
			.unwrap();
		assert_eq!(current_ids, vec!["t3_replacement"]);
		let old_versions: i64 = connection
			.query_row("SELECT COUNT(*) FROM profile_feed_entries WHERE valid_to_generation IS NOT NULL", [], |row| row.get(0))
			.unwrap();
		assert_eq!(old_versions, 1);
		let exact_refs: i64 = connection.query_row("SELECT COUNT(*) FROM profile_object_refs", [], |row| row.get(0)).unwrap();
		assert_eq!(exact_refs, 1);
	}

	#[test]
	fn profile_thread_entries_bundle_comments_from_multiple_followed_users() {
		let mut connection = test_connection();
		apply_source(
			&mut connection,
			"user_comments:u_alice",
			&source(vec![comment("comment-a", "alice", "submission", 200)], None),
			&metadata(b"alice", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		apply_source(
			&mut connection,
			"user_comments:u_bob",
			&source(vec![comment("comment-b", "bob", "submission", 300)], None),
			&metadata(b"bob", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		sync_active_profile_on(
			&mut connection,
			b"profile-comments",
			&[("user_comments:u_alice".to_string(), 1), ("user_comments:u_bob".to_string(), 1)],
		)
		.unwrap();
		let (thread_count, member_count): (i64, i64) = connection
			.query_row(
				"SELECT COUNT(*),MAX(member_count)
				 FROM profile_thread_entries
				 WHERE valid_to_generation IS NULL",
				[],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.unwrap();
		assert_eq!(thread_count, 1);
		assert_eq!(member_count, 2);
		let feed_member_count: i64 = connection
			.query_row(
				"SELECT member_count FROM profile_feed_entries
				 WHERE view_kind=2 AND valid_to_generation IS NULL",
				[],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(feed_member_count, 2);
	}
}
