//! Feature-gated SQLite shadow cache.
//!
//! The legacy `sources.v2` files remain the write authority while this module
//! is enabled. A legacy file is written and synced first; only then is the
//! corresponding source transaction applied here. Reads remain on the legacy
//! path unless `REDLIB_CACHE_SQLITE_READS_ENABLED` is explicitly enabled.

use crate::cache_persistence::{PersistedPost, PersistedRefreshOrigin, PersistedSourceCacheEntry};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use log::{info, warn};
use rusqlite::functions::FunctionFlags;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DATABASE_FILE_NAME: &str = "canonical-cache.sqlite3";
const CURRENT_SCHEMA_VERSION: i64 = 3;
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

CREATE TABLE IF NOT EXISTS object_feed_cards (
    object_pk INTEGER PRIMARY KEY REFERENCES objects(object_pk) ON DELETE CASCADE,
    card_key TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS object_feed_cards_key
    ON object_feed_cards(card_key, object_pk);

CREATE TABLE IF NOT EXISTS active_profiles (
    profile_pk INTEGER PRIMARY KEY,
    fingerprint BLOB NOT NULL UNIQUE,
    generation INTEGER NOT NULL,
    random_epoch INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS active_profile_source_sets (
    profile_pk INTEGER PRIMARY KEY REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    source_set_hash BLOB NOT NULL
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

CREATE TABLE IF NOT EXISTS profile_view_object_refs (
    profile_pk INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    view_kind INTEGER NOT NULL,
    object_pk INTEGER NOT NULL REFERENCES objects(object_pk) ON DELETE CASCADE,
    source_refcount INTEGER NOT NULL CHECK (source_refcount > 0),
    min_source_position INTEGER NOT NULL,
    PRIMARY KEY (profile_pk, view_kind, object_pk)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS profile_view_object_refs_object
    ON profile_view_object_refs(object_pk, profile_pk, view_kind);

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

CREATE TABLE IF NOT EXISTS profile_feed_entry_members (
    profile_pk INTEGER NOT NULL,
    view_kind INTEGER NOT NULL,
    card_key TEXT NOT NULL,
    version INTEGER NOT NULL,
    object_pk INTEGER NOT NULL REFERENCES objects(object_pk) ON DELETE CASCADE,
    PRIMARY KEY (profile_pk, view_kind, card_key, version, object_pk),
    FOREIGN KEY (profile_pk, view_kind, card_key, version)
      REFERENCES profile_feed_entries(profile_pk, view_kind, card_key, version)
      ON DELETE CASCADE
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS profile_feed_entry_members_object
    ON profile_feed_entry_members(object_pk, profile_pk, view_kind);

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
CREATE INDEX IF NOT EXISTS profile_feed_new_history
    ON profile_feed_entries(profile_pk, view_kind, created_sort DESC, card_key DESC, valid_from_generation, valid_to_generation);
CREATE INDEX IF NOT EXISTS profile_feed_activity_history
    ON profile_feed_entries(profile_pk, view_kind, activity_sort DESC, card_key DESC, valid_from_generation, valid_to_generation);
CREATE INDEX IF NOT EXISTS profile_feed_discovered_history
    ON profile_feed_entries(profile_pk, view_kind, discovery_sort DESC, card_key DESC, valid_from_generation, valid_to_generation);
CREATE INDEX IF NOT EXISTS profile_feed_random_history
    ON profile_feed_entries(profile_pk, view_kind, random_epoch, random_round, random_rank, card_key, valid_from_generation, valid_to_generation);
CREATE INDEX IF NOT EXISTS profile_feed_videos_history
    ON profile_feed_entries(profile_pk, view_kind, duration_seconds DESC, created_sort DESC, card_key DESC, valid_from_generation, valid_to_generation);

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PayloadCodec {
	Raw,
	Zstd,
}

impl PayloadCodec {
	fn from_environment() -> Result<Self, String> {
		match std::env::var("REDLIB_CACHE_SQLITE_PAYLOAD_CODEC")
			.unwrap_or_else(|_| "raw".to_string())
			.trim()
			.to_ascii_lowercase()
			.as_str()
		{
			"raw" => Ok(Self::Raw),
			"zstd" => Ok(Self::Zstd),
			value => Err(format!("REDLIB_CACHE_SQLITE_PAYLOAD_CODEC must be raw or zstd, got {value}")),
		}
	}

	fn database_value(self) -> i64 {
		match self {
			Self::Raw => 0,
			Self::Zstd => 1,
		}
	}

	fn encode(self, payload: &[u8]) -> Result<Vec<u8>, String> {
		match self {
			Self::Raw => Ok(payload.to_vec()),
			Self::Zstd => zstd::stream::encode_all(payload, 1).map_err(|error| error.to_string()),
		}
	}
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

#[derive(Clone, Debug)]
struct SourceWriteMetadata {
	size: u64,
	mtime_ns: u128,
	hash: Vec<u8>,
	imported_at: u64,
}

#[derive(Clone, Debug)]
struct PreparedSourceImport {
	cache_key: String,
	entry: PersistedSourceCacheEntry,
	metadata: SourceWriteMetadata,
}

struct ShadowCache {
	connection: Mutex<Connection>,
	payload_codec: PayloadCodec,
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
	let payload_codec = PayloadCodec::from_environment()?;
	let stats = import_legacy_sources(&mut connection, cache_dir, payload_codec)?;
	SHADOW_CACHE
		.set(ShadowCache {
			connection: Mutex::new(connection),
			payload_codec,
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

fn configured_migration_batch_sources() -> Result<usize, String> {
	let value = std::env::var("REDLIB_CACHE_SQLITE_MIGRATION_BATCH_SOURCES").unwrap_or_else(|_| "32".to_string());
	let batch_size = value.parse::<usize>().map_err(|_| format!("invalid REDLIB_CACHE_SQLITE_MIGRATION_BATCH_SOURCES={value}"))?;
	if (1..=64).contains(&batch_size) {
		Ok(batch_size)
	} else {
		Err(format!("REDLIB_CACHE_SQLITE_MIGRATION_BATCH_SOURCES must be between 1 and 64, got {batch_size}"))
	}
}

fn configured_active_profile_limit() -> Result<usize, String> {
	let value = std::env::var("REDLIB_CACHE_SQLITE_ACTIVE_PROFILE_LIMIT").unwrap_or_else(|_| "4".to_string());
	let limit = value.parse::<usize>().map_err(|_| format!("invalid REDLIB_CACHE_SQLITE_ACTIVE_PROFILE_LIMIT={value}"))?;
	if (1..=32).contains(&limit) {
		Ok(limit)
	} else {
		Err(format!("REDLIB_CACHE_SQLITE_ACTIVE_PROFILE_LIMIT must be between 1 and 32, got {limit}"))
	}
}

fn open_database(path: &Path) -> Result<Connection, String> {
	open_database_with_options(path, configured_page_size()?, configured_synchronous()?)
}

fn open_database_with_options(path: &Path, requested_page_size: i64, synchronous: &str) -> Result<Connection, String> {
	if !matches!(requested_page_size, 8192 | 16384) {
		return Err(format!("SQLite cache page size must be 8192 or 16384, got {requested_page_size}"));
	}
	if !matches!(synchronous, "FULL" | "NORMAL") {
		return Err(format!("SQLite synchronous mode must be FULL or NORMAL, got {synchronous}"));
	}
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).map_err(|error| error.to_string())?;
	}
	let existed = path.exists();
	let connection = Connection::open(path).map_err(|error| error.to_string())?;
	if !existed {
		connection
			.execute_batch(&format!("PRAGMA page_size={requested_page_size};"))
			.map_err(|error| error.to_string())?;
	}
	let actual_page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	if actual_page_size != requested_page_size {
		return Err(format!("existing SQLite cache page size is {actual_page_size}, configured value is {requested_page_size}"));
	}
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
	register_connection_functions(&connection)?;
	connection.execute_batch(SCHEMA).map_err(|error| error.to_string())?;
	backfill_object_feed_cards(&connection)?;
	backfill_current_feed_entry_members(&connection)?;
	set_meta_i64(&connection, "schema_version", CURRENT_SCHEMA_VERSION)?;
	Ok(connection)
}

fn register_connection_functions(connection: &Connection) -> Result<(), String> {
	connection
		.create_scalar_function("redlib_random_rank", 3, FunctionFlags::SQLITE_DETERMINISTIC, |context| {
			let fingerprint = context.get::<Vec<u8>>(0)?;
			let epoch = context.get::<i64>(1)?;
			let card_key = context.get::<String>(2)?;
			Ok(deterministic_random_rank(&fingerprint, epoch, &card_key))
		})
		.map_err(|error| error.to_string())
}

fn deterministic_random_rank(fingerprint: &[u8], epoch: i64, card_key: &str) -> i64 {
	let mut digest = Sha256::new();
	digest.update(fingerprint);
	digest.update(epoch.to_be_bytes());
	digest.update(card_key.as_bytes());
	let bytes: [u8; 8] = digest.finalize()[..8].try_into().expect("SHA-256 prefix has eight bytes");
	(u64::from_be_bytes(bytes) & i64::MAX as u64) as i64
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

fn import_legacy_sources(connection: &mut Connection, cache_dir: &Path, payload_codec: PayloadCodec) -> Result<ImportStats, String> {
	let descriptors = source_descriptors(cache_dir)?;
	let batch_size = configured_migration_batch_sources()?;
	let mut stats = ImportStats {
		scanned_sources: descriptors.len(),
		..ImportStats::default()
	};
	let mut pending = Vec::with_capacity(batch_size);
	for descriptor in descriptors {
		let metadata_unchanged = connection
			.query_row(
				"SELECT 1 FROM migration_sources
				 WHERE cache_key=?1 AND source_size=?2 AND source_mtime_ns=?3 AND status=1",
				params![descriptor.cache_key, descriptor.size as i64, i64::try_from(descriptor.mtime_ns).unwrap_or(i64::MAX)],
				|row| row.get::<_, i64>(0),
			)
			.optional()
			.map_err(|error| error.to_string())?
			.is_some();
		if metadata_unchanged {
			stats.skipped_sources += 1;
			continue;
		}
		let bytes = fs::read(&descriptor.path).map_err(|error| format!("{}: {error}", descriptor.path.display()))?;
		let source_hash = sha256(&bytes);
		let entry: PersistedSourceCacheEntry = bincode::deserialize(&bytes).map_err(|error| format!("{}: {error}", descriptor.path.display()))?;
		let metadata = SourceWriteMetadata {
			size: descriptor.size,
			mtime_ns: descriptor.mtime_ns,
			hash: source_hash,
			imported_at: unix_now(),
		};
		pending.push(PreparedSourceImport {
			cache_key: descriptor.cache_key,
			entry,
			metadata,
		});
		if pending.len() == batch_size {
			commit_import_batch(connection, &pending, payload_codec, &mut stats)?;
			pending.clear();
		}
	}
	if !pending.is_empty() {
		commit_import_batch(connection, &pending, payload_codec, &mut stats)?;
	}
	Ok(stats)
}

fn commit_import_batch(connection: &mut Connection, pending: &[PreparedSourceImport], payload_codec: PayloadCodec, stats: &mut ImportStats) -> Result<(), String> {
	let transaction = connection.transaction().map_err(|error| error.to_string())?;
	reset_delta_tables(&transaction)?;
	for source in pending {
		apply_source_in_transaction(&transaction, &source.cache_key, &source.entry, &source.metadata, FirstSeen::Migration, payload_codec)?;
	}
	refresh_affected_object_feed_cards(&transaction)?;
	refresh_affected_profiles(&transaction)?;
	transaction.commit().map_err(|error| error.to_string())?;
	stats.imported_sources += pending.len();
	stats.objects_seen += pending.iter().map(|source| source.entry.posts.len()).sum::<usize>();
	Ok(())
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
		.and_then(|mut connection| apply_source_with_codec(&mut connection, source_cache_key, entry, &write_metadata, first_seen, cache.payload_codec));
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
		posts.push(decode_payload(codec, &payload)?);
	}
	Ok(Some(PersistedSourceCacheEntry {
		subreddit,
		posts,
		after,
		fetched_at_unix: fetched_at.max(0) as u64,
		refresh_origin: refresh_origin.and_then(refresh_origin_from_i64),
	}))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimelineSort {
	New,
	Discovered,
	Random,
	Videos,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimelineCursor {
	pub generation: i64,
	pub random_epoch: i64,
	pub sort_primary: i64,
	pub sort_secondary: i64,
	pub card_key: String,
}

#[derive(Clone, Debug)]
pub(crate) struct TimelinePage {
	pub posts: Vec<PersistedPost>,
	pub next: Option<TimelineCursor>,
}

pub(crate) fn query_profile_timeline(
	fingerprint: &[u8],
	view_kind: i64,
	sort: TimelineSort,
	cursor: Option<&TimelineCursor>,
	limit: usize,
	filters: &HashSet<String>,
) -> Result<Option<TimelinePage>, String> {
	if !reads_enabled() {
		return Ok(None);
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return Err("SQLite reads enabled but shadow store is unavailable".to_string());
	};
	let connection = cache.connection.lock().map_err(|_| "SQLite shadow connection lock poisoned".to_string())?;
	let profile_exists = connection
		.query_row("SELECT 1 FROM active_profiles WHERE fingerprint=?1", [fingerprint], |row| row.get::<_, i64>(0))
		.optional()
		.map_err(|error| error.to_string())?
		.is_some();
	if !profile_exists {
		return Ok(None);
	}
	query_profile_timeline_on(&connection, fingerprint, view_kind, sort, cursor, limit, filters).map(Some)
}

pub(crate) fn query_profile_video_candidates(fingerprint: &[u8], view_kind: i64, limit: usize, filters: &HashSet<String>) -> Result<Option<Vec<PersistedPost>>, String> {
	if !reads_enabled() {
		return Ok(None);
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return Err("SQLite reads enabled but shadow store is unavailable".to_string());
	};
	let connection = cache.connection.lock().map_err(|_| "SQLite shadow connection lock poisoned".to_string())?;
	let profile_exists = connection
		.query_row("SELECT 1 FROM active_profiles WHERE fingerprint=?1", [fingerprint], |row| row.get::<_, i64>(0))
		.optional()
		.map_err(|error| error.to_string())?
		.is_some();
	if !profile_exists {
		return Ok(None);
	}
	query_profile_timeline_on(&connection, fingerprint, view_kind, TimelineSort::Videos, None, limit, filters).map(|page| Some(page.posts))
}

pub(crate) fn source_exists(source_cache_key: &str) -> bool {
	if !reads_enabled() {
		return false;
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return false;
	};
	cache
		.connection
		.lock()
		.ok()
		.and_then(|connection| {
			connection
				.query_row("SELECT 1 FROM sources WHERE cache_key=?1 AND generation>0", [source_cache_key], |row| row.get::<_, i64>(0))
				.optional()
				.ok()
				.flatten()
		})
		.is_some()
}

pub(crate) fn source_seed_entries() -> Result<Option<Vec<(String, u64)>>, String> {
	if !reads_enabled() {
		return Ok(None);
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return Err("SQLite reads enabled but shadow store is unavailable".to_string());
	};
	let connection = cache.connection.lock().map_err(|_| "SQLite shadow connection lock poisoned".to_string())?;
	let mut statement = connection
		.prepare("SELECT cache_key,fetched_at FROM sources WHERE generation>0 ORDER BY cache_key")
		.map_err(|error| error.to_string())?;
	let rows = statement
		.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
		.map_err(|error| error.to_string())?;
	let mut entries = Vec::new();
	for row in rows {
		let (cache_key, fetched_at) = row.map_err(|error| error.to_string())?;
		entries.push((cache_key, fetched_at.max(0) as u64));
	}
	Ok(Some(entries))
}

fn query_profile_timeline_on(
	connection: &Connection,
	fingerprint: &[u8],
	view_kind: i64,
	sort: TimelineSort,
	cursor: Option<&TimelineCursor>,
	limit: usize,
	filters: &HashSet<String>,
) -> Result<TimelinePage, String> {
	if !matches!(view_kind, 1 | 2) {
		return Err(format!("invalid profile view kind {view_kind}"));
	}
	let (profile_pk, current_generation, current_random_epoch): (i64, i64, i64) = connection
		.query_row(
			"SELECT profile_pk,generation,random_epoch FROM active_profiles WHERE fingerprint=?1",
			[fingerprint],
			|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
		)
		.map_err(|error| error.to_string())?;
	let generation = cursor.map_or(current_generation, |value| value.generation);
	let random_epoch = cursor.map_or(current_random_epoch, |value| value.random_epoch);
	if generation > current_generation {
		return Err("timeline cursor generation is newer than the active profile".to_string());
	}
	let limit = if sort == TimelineSort::Videos { limit.clamp(1, 8_000) } else { limit.clamp(1, 250) };
	let scan_limit = limit.saturating_mul(4).min(if sort == TimelineSort::Videos { 32_000 } else { 1_000 });
	let mut normalized_filters = filters
		.iter()
		.map(|value| value.trim().to_ascii_lowercase())
		.filter(|value| !value.is_empty())
		.collect::<Vec<_>>();
	normalized_filters.sort();
	normalized_filters.dedup();
	let member_filter_clause = if normalized_filters.is_empty() {
		String::new()
	} else {
		let values = normalized_filters
			.iter()
			.map(|value| format!("'{}'", value.replace('\'', "''")))
			.collect::<Vec<_>>()
			.join(",");
		format!(
			" AND EXISTS(
			   SELECT 1
			   FROM profile_feed_entry_members filter_members
			   JOIN objects filter_object ON filter_object.object_pk=filter_members.object_pk
			   WHERE filter_members.profile_pk=feed.profile_pk
			     AND filter_members.view_kind=feed.view_kind
			     AND filter_members.card_key=feed.card_key
			     AND filter_members.version=feed.version
			     AND filter_object.community_lc NOT IN ({values})
			     AND ('u_' || filter_object.author_lc) NOT IN ({values})
			 )"
		)
	};

	let snapshot_clause = "feed.valid_from_generation<=?3 AND (feed.valid_to_generation IS NULL OR feed.valid_to_generation>?3)";
	let (order_clause, cursor_clause) = match sort {
		TimelineSort::New if view_kind == 2 => (
			"feed.activity_sort DESC,feed.card_key DESC",
			cursor.map(|_| " AND (feed.activity_sort,feed.card_key)<(?4,?6)").unwrap_or_default(),
		),
		TimelineSort::New => (
			"feed.created_sort DESC,feed.card_key DESC",
			cursor.map(|_| " AND (feed.created_sort,feed.card_key)<(?4,?6)").unwrap_or_default(),
		),
		TimelineSort::Discovered => (
			"feed.discovery_sort DESC,feed.card_key DESC",
			cursor.map(|_| " AND (feed.discovery_sort,feed.card_key)<(?4,?6)").unwrap_or_default(),
		),
		TimelineSort::Videos => (
			"feed.duration_seconds DESC,feed.created_sort DESC,feed.card_key DESC",
			cursor
				.map(|_| " AND (feed.duration_seconds,feed.created_sort,feed.card_key)<(?4,?5,?6)")
				.unwrap_or_default(),
		),
		TimelineSort::Random => (
			"feed.random_round,feed.random_rank,feed.card_key",
			cursor.map(|_| " AND (feed.random_round,feed.random_rank,feed.card_key)>(?4,?5,?6)").unwrap_or_default(),
		),
	};
	let sort_filter = match sort {
		TimelineSort::Discovered => " AND feed.discovery_sort>0",
		TimelineSort::Videos => " AND feed.duration_seconds>0",
		TimelineSort::Random => " AND feed.random_epoch=?7",
		TimelineSort::New => "",
	};
	let sql = format!(
		"SELECT feed.card_key,feed.version,feed.created_sort,feed.activity_sort,feed.discovery_sort,
		        feed.duration_seconds,feed.random_round,feed.random_rank
		 FROM profile_feed_entries feed
		 WHERE feed.profile_pk=?1 AND feed.view_kind=?2 AND {snapshot_clause}
		   {sort_filter}{member_filter_clause}{cursor_clause}
		 ORDER BY {order_clause}
		 LIMIT ?8"
	);
	let (cursor_primary, cursor_secondary, cursor_key) = cursor
		.map(|value| (value.sort_primary, value.sort_secondary, value.card_key.as_str()))
		.unwrap_or((0, 0, ""));
	let mut statement = connection.prepare(&sql).map_err(|error| error.to_string())?;
	let rows = statement
		.query_map(
			params![
				profile_pk,
				view_kind,
				generation,
				cursor_primary,
				cursor_secondary,
				cursor_key,
				random_epoch,
				scan_limit as i64
			],
			|row| {
				Ok((
					row.get::<_, String>(0)?,
					row.get::<_, i64>(1)?,
					row.get::<_, i64>(2)?,
					row.get::<_, i64>(3)?,
					row.get::<_, i64>(4)?,
					row.get::<_, i64>(5)?,
					row.get::<_, i64>(6)?,
					row.get::<_, i64>(7)?,
				))
			},
		)
		.map_err(|error| error.to_string())?;
	let mut posts = Vec::new();
	let mut next = None;
	for row in rows {
		let (card_key, version, created_sort, activity_sort, discovery_sort, duration, random_round, random_rank) = row.map_err(|error| error.to_string())?;
		let (sort_primary, sort_secondary) = match sort {
			TimelineSort::New if view_kind == 2 => (activity_sort, 0),
			TimelineSort::New => (created_sort, 0),
			TimelineSort::Discovered => (discovery_sort, 0),
			TimelineSort::Videos => (duration, created_sort),
			TimelineSort::Random => (random_round, random_rank),
		};
		next = Some(TimelineCursor {
			generation,
			random_epoch,
			sort_primary,
			sort_secondary,
			card_key: card_key.clone(),
		});
		let Some(post) = hydrate_profile_card(connection, profile_pk, view_kind, &card_key, version, sort, &normalized_filters)? else {
			continue;
		};
		posts.push(post);
		if posts.len() >= limit {
			break;
		}
	}
	Ok(TimelinePage { posts, next })
}

fn hydrate_profile_card(
	connection: &Connection,
	profile_pk: i64,
	view_kind: i64,
	card_key: &str,
	version: i64,
	sort: TimelineSort,
	filters: &[String],
) -> Result<Option<PersistedPost>, String> {
	let mut object_statement = connection
		.prepare_cached(
			"SELECT o.community_lc,o.author_lc,o.duration_seconds,o.created_utc,
			        o.object_pk,o.payload_codec,o.payload
			 FROM profile_feed_entry_members members
			 JOIN objects o ON o.object_pk=members.object_pk
			 WHERE members.profile_pk=?1
			   AND members.view_kind=?2
			   AND members.card_key=?3
			   AND members.version=?4
			   AND o.is_deleted=0 AND o.is_promotional=0",
		)
		.map_err(|error| error.to_string())?;
	let mut candidates = Vec::new();
	let rows = object_statement
		.query_map(params![profile_pk, view_kind, card_key, version], |row| {
			Ok((
				row.get::<_, String>(0)?,
				row.get::<_, String>(1)?,
				row.get::<_, i64>(2)?,
				row.get::<_, i64>(3)?,
				row.get::<_, i64>(4)?,
				row.get::<_, i64>(5)?,
				row.get::<_, Vec<u8>>(6)?,
			))
		})
		.map_err(|error| error.to_string())?;
	for row in rows {
		let (community_lc, author_lc, duration, created, object_pk, codec, payload) = row.map_err(|error| error.to_string())?;
		if filters.binary_search(&community_lc).is_ok() || filters.binary_search(&format!("u_{author_lc}")).is_ok() {
			continue;
		}
		candidates.push((duration, created, object_pk, codec, payload));
	}
	if sort == TimelineSort::Videos {
		candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)).then_with(|| right.2.cmp(&left.2)));
	} else {
		candidates.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.2.cmp(&left.2)));
	}
	let mut runtime_posts = Vec::with_capacity(candidates.len());
	for (_, _, _, codec, payload) in candidates {
		runtime_posts.push(decode_payload(codec, &payload)?.into_runtime());
	}
	if runtime_posts.is_empty() {
		return Ok(None);
	}
	if view_kind == 2 {
		runtime_posts = crate::user::bundle_followed_user_comments(runtime_posts);
	}
	let mut collapsed = crate::user::collapse_duplicate_submissions(runtime_posts);
	collapsed.sort_by(|left, right| {
		crate::user::post_feed_activity_ts(right)
			.cmp(&crate::user::post_feed_activity_ts(left))
			.then_with(|| right.created_ts.cmp(&left.created_ts))
			.then_with(|| right.id.cmp(&left.id))
	});
	Ok(collapsed.first().map(PersistedPost::from_runtime))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubmittedCursor {
	pub created_utc: i64,
	pub object_pk: i64,
}

pub(crate) fn query_submitted(author: &str, limit: usize, filters: &HashSet<String>) -> Result<Option<Vec<PersistedPost>>, String> {
	if !reads_enabled() {
		return Ok(None);
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return Err("SQLite reads enabled but shadow store is unavailable".to_string());
	};
	let connection = cache.connection.lock().map_err(|_| "SQLite shadow connection lock poisoned".to_string())?;
	query_submitted_on(&connection, author, None, limit, filters).map(|(posts, _)| Some(posts))
}

pub(crate) fn query_submitted_on(
	connection: &Connection,
	author: &str,
	cursor: Option<&SubmittedCursor>,
	limit: usize,
	filters: &HashSet<String>,
) -> Result<(Vec<PersistedPost>, Option<SubmittedCursor>), String> {
	let author = author.trim().trim_start_matches("u_").to_ascii_lowercase();
	let limit = limit.clamp(1, 250);
	let mut normalized_filters = filters
		.iter()
		.map(|value| value.trim().to_ascii_lowercase())
		.filter(|value| !value.is_empty())
		.collect::<Vec<_>>();
	normalized_filters.sort();
	normalized_filters.dedup();
	let filter_clause = if normalized_filters.is_empty() {
		String::new()
	} else {
		let values = normalized_filters
			.iter()
			.map(|value| format!("'{}'", value.replace('\'', "''")))
			.collect::<Vec<_>>()
			.join(",");
		format!(" AND community_lc NOT IN ({values}) AND ('u_' || author_lc) NOT IN ({values})")
	};
	let cursor_clause = cursor.map(|_| " AND (created_utc<?2 OR (created_utc=?2 AND object_pk<?3))").unwrap_or_default();
	let sql = format!(
		"SELECT object_pk,created_utc,payload_codec,payload
		 FROM objects
		 WHERE author_lc=?1 AND kind=3 AND is_deleted=0 AND is_promotional=0
		   {filter_clause}{cursor_clause}
		 ORDER BY created_utc DESC,object_pk DESC
		 LIMIT ?4"
	);
	let (cursor_created, cursor_pk) = cursor.map(|value| (value.created_utc, value.object_pk)).unwrap_or((0, 0));
	let mut statement = connection.prepare(&sql).map_err(|error| error.to_string())?;
	let rows = statement
		.query_map(params![author, cursor_created, cursor_pk, limit as i64], |row| {
			Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, Vec<u8>>(3)?))
		})
		.map_err(|error| error.to_string())?;
	let mut posts = Vec::new();
	let mut next = None;
	for row in rows {
		let (object_pk, created_utc, codec, payload) = row.map_err(|error| error.to_string())?;
		posts.push(decode_payload(codec, &payload)?);
		next = Some(SubmittedCursor { created_utc, object_pk });
	}
	Ok((posts, next))
}

fn decode_payload(codec: i64, payload: &[u8]) -> Result<PersistedPost, String> {
	let decoded = match codec {
		0 => payload.to_vec(),
		1 => zstd::stream::decode_all(payload).map_err(|error| error.to_string())?,
		_ => return Err(format!("unsupported SQLite payload codec {codec}")),
	};
	bincode::deserialize(&decoded).map_err(|error| error.to_string())
}

fn apply_source_with_codec(
	connection: &mut Connection,
	cache_key: &str,
	entry: &PersistedSourceCacheEntry,
	metadata: &SourceWriteMetadata,
	first_seen: FirstSeen,
	payload_codec: PayloadCodec,
) -> Result<(), String> {
	let transaction = connection.transaction().map_err(|error| error.to_string())?;
	reset_delta_tables(&transaction)?;
	apply_source_in_transaction(&transaction, cache_key, entry, metadata, first_seen, payload_codec)?;
	refresh_affected_object_feed_cards(&transaction)?;
	refresh_affected_profiles(&transaction)?;
	transaction.commit().map_err(|error| error.to_string())
}

fn apply_source_in_transaction(
	transaction: &Transaction<'_>,
	cache_key: &str,
	entry: &PersistedSourceCacheEntry,
	metadata: &SourceWriteMetadata,
	first_seen: FirstSeen,
	payload_codec: PayloadCodec,
) -> Result<(), String> {
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
	let observed_at = if entry.fetched_at_unix == 0 { metadata.imported_at } else { entry.fetched_at_unix };
	for (position, post) in entry.posts.iter().enumerate() {
		let object_pk = upsert_object(&transaction, post, observed_at, first_seen, baseline_at, baseline_seq, payload_codec)?;
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
	Ok(())
}

#[cfg(test)]
fn apply_source(connection: &mut Connection, cache_key: &str, entry: &PersistedSourceCacheEntry, metadata: &SourceWriteMetadata, first_seen: FirstSeen) -> Result<(), String> {
	apply_source_with_codec(connection, cache_key, entry, metadata, first_seen, PayloadCodec::Raw)
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

fn refresh_affected_object_feed_cards(transaction: &Transaction<'_>) -> Result<(), String> {
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_objects(object_pk)
			 SELECT thread_member.object_pk
			 FROM temp_affected_objects changed
			 JOIN duplicate_members changed_duplicate ON changed_duplicate.object_pk=changed.object_pk
			 JOIN duplicate_members sibling ON sibling.duplicate_group_pk=changed_duplicate.duplicate_group_pk
			 JOIN thread_groups thread_group ON thread_group.linked_submission_pk=sibling.object_pk
			 JOIN thread_members thread_member ON thread_member.thread_group_pk=thread_group.thread_group_pk",
			[],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"DELETE FROM object_feed_cards
			 WHERE object_pk IN (SELECT object_pk FROM temp_affected_objects)",
			[],
		)
		.map_err(|error| error.to_string())?;
	let sql = format!(
		r#"
INSERT INTO object_feed_cards(object_pk,card_key)
SELECT o.object_pk,{OBJECT_CARD_KEY_SQL}
FROM temp_affected_objects changed
JOIN objects o ON o.object_pk=changed.object_pk
WHERE {OBJECT_CARD_KEY_SQL} IS NOT NULL
"#
	);
	transaction.execute(&sql, []).map_err(|error| error.to_string())?;
	Ok(())
}

fn backfill_object_feed_cards(connection: &Connection) -> Result<(), String> {
	let missing: i64 = connection
		.query_row(
			"SELECT EXISTS(
			   SELECT 1 FROM objects o
			   LEFT JOIN object_feed_cards cards ON cards.object_pk=o.object_pk
			   WHERE cards.object_pk IS NULL
			 )",
			[],
			|row| row.get(0),
		)
		.map_err(|error| error.to_string())?;
	if missing == 0 {
		return Ok(());
	}
	let sql = format!(
		r#"
INSERT INTO object_feed_cards(object_pk,card_key)
SELECT o.object_pk,{OBJECT_CARD_KEY_SQL}
FROM objects o
WHERE {OBJECT_CARD_KEY_SQL} IS NOT NULL
ON CONFLICT(object_pk) DO UPDATE SET card_key=excluded.card_key
"#
	);
	connection.execute(&sql, []).map_err(|error| error.to_string())?;
	Ok(())
}

fn backfill_current_feed_entry_members(connection: &Connection) -> Result<(), String> {
	let missing: i64 = connection
		.query_row(
			"SELECT EXISTS(
			   SELECT 1
			   FROM profile_feed_entries feed
			   WHERE feed.valid_to_generation IS NULL
			     AND NOT EXISTS(
			       SELECT 1
			       FROM profile_feed_entry_members members
			       WHERE members.profile_pk=feed.profile_pk
			         AND members.view_kind=feed.view_kind
			         AND members.card_key=feed.card_key
			         AND members.version=feed.version
			     )
			 )",
			[],
			|row| row.get(0),
		)
		.map_err(|error| error.to_string())?;
	if missing == 0 {
		return Ok(());
	}
	let sql = r#"
INSERT OR IGNORE INTO profile_feed_entry_members(
  profile_pk,view_kind,card_key,version,object_pk)
SELECT feed.profile_pk,feed.view_kind,feed.card_key,feed.version,refs.object_pk
FROM profile_view_object_refs refs
JOIN object_feed_cards cards ON cards.object_pk=refs.object_pk
JOIN profile_feed_entries feed
  ON feed.profile_pk=refs.profile_pk
 AND feed.view_kind=refs.view_kind
 AND feed.card_key=cards.card_key
WHERE feed.valid_to_generation IS NULL
"#;
	connection.execute(sql, []).map_err(|error| error.to_string())?;
	Ok(())
}

fn refresh_affected_profiles(transaction: &Transaction<'_>) -> Result<(), String> {
	let affected_profile_count: i64 = transaction
		.query_row("SELECT COUNT(*) FROM temp_affected_profiles", [], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	if affected_profile_count == 0 {
		return Ok(());
	}
	let trace_timings = env_bool("REDLIB_CACHE_SQLITE_TRACE_TIMINGS");
	let mut stage_started = Instant::now();
	let trace_stage = |stage: &str, started: &mut Instant| {
		if trace_timings {
			eprintln!("cache_sqlite stage={stage} elapsed_ms={}", started.elapsed().as_millis());
		}
		*started = Instant::now();
	};
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_cards(card_key)
			 SELECT DISTINCT cards.card_key
			 FROM temp_affected_objects changed
			 JOIN object_feed_cards cards ON cards.object_pk=changed.object_pk",
			[],
		)
		.map_err(|error| error.to_string())?;
	trace_stage("affected_cards", &mut stage_started);
	transaction
		.execute(
			"INSERT OR IGNORE INTO temp_affected_threads(thread_group_pk)
			 SELECT tm.thread_group_pk
			 FROM temp_affected_objects changed
			 JOIN thread_members tm ON tm.object_pk=changed.object_pk",
			[],
		)
		.map_err(|error| error.to_string())?;
	trace_stage("affected_threads", &mut stage_started);

	transaction
		.execute(
			"DELETE FROM profile_object_refs
			 WHERE profile_pk IN (SELECT profile_pk FROM temp_affected_profiles)
			   AND object_pk IN (SELECT object_pk FROM temp_affected_objects)",
			[],
		)
		.map_err(|error| error.to_string())?;
	trace_stage("profile_refs", &mut stage_started);
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
	trace_stage("profile_view_refs", &mut stage_started);
	transaction
		.execute(
			"DELETE FROM profile_view_object_refs
			 WHERE profile_pk IN (SELECT profile_pk FROM temp_affected_profiles)
			   AND object_pk IN (SELECT object_pk FROM temp_affected_objects)",
			[],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"INSERT INTO profile_view_object_refs(
			   profile_pk,view_kind,object_pk,source_refcount,min_source_position)
			 SELECT affected.profile_pk,views.view_kind,sm.object_pk,
			        COUNT(DISTINCT ps.source_pk),MIN(sm.source_position)
			 FROM temp_affected_profiles affected
			 JOIN profile_sources ps ON ps.profile_pk=affected.profile_pk
			 JOIN source_memberships sm ON sm.source_pk=ps.source_pk
			 JOIN temp_affected_objects changed ON changed.object_pk=sm.object_pk
			 CROSS JOIN (SELECT 1 AS view_kind UNION ALL SELECT 2) views
			 WHERE (views.view_kind=1 AND ps.source_role IN (0,1))
			    OR (views.view_kind=2 AND ps.source_role IN (1,2))
			 GROUP BY affected.profile_pk,views.view_kind,sm.object_pk",
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
	trace_stage("profile_generation", &mut stage_started);

	close_and_insert_feed_entries(transaction)?;
	trace_stage("feed_entries", &mut stage_started);
	close_and_insert_thread_entries(transaction)?;
	trace_stage("thread_entries", &mut stage_started);
	Ok(())
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

	let sql = r#"
WITH raw_eligible AS (
  SELECT affected.profile_pk,refs.view_kind,
         o.object_pk,o.kind,o.created_utc,o.link_created_utc,o.discovery_seq,
         o.duration_seconds,o.community_lc,o.author_lc,
         refs.min_source_position AS source_position,
         object_card.card_key,
         (SELECT dm.duplicate_group_pk FROM duplicate_members dm WHERE dm.object_pk=o.object_pk) AS duplicate_group_pk,
         (SELECT tm.thread_group_pk FROM thread_members tm WHERE tm.object_pk=o.object_pk) AS thread_group_pk
  FROM temp_affected_profiles affected
  JOIN profile_view_object_refs refs ON refs.profile_pk=affected.profile_pk
  JOIN objects o ON o.object_pk=refs.object_pk
  JOIN object_feed_cards object_card ON object_card.object_pk=o.object_pk
  WHERE o.is_deleted=0 AND o.is_promotional=0
),
eligible AS (
  SELECT profile_pk,view_kind,object_pk,kind,created_utc,link_created_utc,
         discovery_seq,duration_seconds,community_lc,author_lc,card_key,
         duplicate_group_pk,thread_group_pk,MIN(source_position) AS source_position
  FROM raw_eligible
  GROUP BY profile_pk,view_kind,object_pk
),
affected_eligible AS (
  SELECT eligible.*
  FROM eligible
  JOIN temp_affected_cards cards ON cards.card_key=eligible.card_key
),
ranked_eligible AS (
  SELECT affected_eligible.*,
         ROW_NUMBER() OVER (
           PARTITION BY profile_pk,view_kind,card_key
           ORDER BY kind DESC,created_utc DESC,object_pk DESC
         ) AS representative_rank
  FROM affected_eligible
),
card_stats AS (
  SELECT profile_pk,view_kind,card_key,
         MAX(CASE WHEN kind=3 THEN duplicate_group_pk END) AS duplicate_group_pk,
         MAX(thread_group_pk) AS thread_group_pk,
         MAX(CASE WHEN representative_rank=1 THEN object_pk END) AS representative_pk,
         MAX(CASE WHEN kind=3 THEN created_utc ELSE link_created_utc END) AS created_sort,
         MAX(created_utc) AS activity_sort,
         MAX(discovery_seq) AS discovery_sort,
         MAX(duration_seconds) AS duration_seconds,
         MIN(source_position) AS random_round,
         COUNT(*) AS member_count
  FROM ranked_eligible
  GROUP BY profile_pk,view_kind,card_key
),
expanded AS (
  SELECT stats.*,representative.community_lc,representative.author_lc
  FROM card_stats stats
  JOIN objects representative ON representative.object_pk=stats.representative_pk
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
       redlib_random_rank(profiles.fingerprint,profiles.random_epoch,expanded.card_key),
       expanded.member_count
FROM expanded
JOIN active_profiles profiles ON profiles.profile_pk=expanded.profile_pk
"#;
	transaction.execute(&sql, []).map_err(|error| error.to_string())?;
	let member_sql = r#"
INSERT OR IGNORE INTO profile_feed_entry_members(
  profile_pk,view_kind,card_key,version,object_pk)
SELECT feed.profile_pk,feed.view_kind,feed.card_key,feed.version,refs.object_pk
FROM temp_affected_cards changed
JOIN profile_feed_entries feed ON feed.card_key=changed.card_key
JOIN object_feed_cards object_card ON object_card.card_key=feed.card_key
JOIN profile_view_object_refs refs
  ON refs.profile_pk=feed.profile_pk
 AND refs.view_kind=feed.view_kind
 AND refs.object_pk=object_card.object_pk
JOIN objects o ON o.object_pk=refs.object_pk
WHERE feed.valid_to_generation IS NULL
  AND feed.profile_pk IN (SELECT profile_pk FROM temp_affected_profiles)
  AND o.is_deleted=0
  AND o.is_promotional=0
"#;
	transaction.execute(&member_sql, []).map_err(|error| error.to_string())?;
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
  SELECT affected.profile_pk,tm.thread_group_pk,o.object_pk,o.kind,o.created_utc,
         o.link_created_utc,o.discovery_seq
  FROM temp_affected_profiles affected
  JOIN profile_view_object_refs refs ON refs.profile_pk=affected.profile_pk AND refs.view_kind=2
  JOIN thread_members tm ON tm.object_pk=refs.object_pk
  JOIN temp_affected_threads changed ON changed.thread_group_pk=tm.thread_group_pk
  JOIN objects o ON o.object_pk=refs.object_pk
  WHERE o.is_deleted=0 AND o.is_promotional=0
),
ranked AS (
  SELECT eligible.*,
         ROW_NUMBER() OVER (
           PARTITION BY profile_pk,thread_group_pk
           ORDER BY kind DESC,created_utc DESC,object_pk DESC
         ) AS representative_rank
  FROM eligible
),
stats AS (
  SELECT profile_pk,thread_group_pk,
         MAX(CASE WHEN representative_rank=1 THEN object_pk END) AS representative_pk,
         MAX(CASE WHEN kind=3 THEN created_utc ELSE link_created_utc END) AS created_sort,
         MAX(created_utc) AS activity_sort,
         MAX(discovery_seq) AS discovery_sort,
         COUNT(*) AS member_count
  FROM ranked
  GROUP BY profile_pk,thread_group_pk
)
INSERT INTO profile_thread_entries(
  profile_pk,thread_group_pk,version,valid_from_generation,valid_to_generation,
  representative_pk,created_sort,activity_sort,discovery_sort,member_count)
SELECT stats.profile_pk,stats.thread_group_pk,
       COALESCE((
         SELECT MAX(old.version)+1
         FROM profile_thread_entries old
         WHERE old.profile_pk=stats.profile_pk
           AND old.thread_group_pk=stats.thread_group_pk
       ),1),
       profiles.generation,NULL,stats.representative_pk,stats.created_sort,
       stats.activity_sort,stats.discovery_sort,stats.member_count
FROM stats
JOIN active_profiles profiles ON profiles.profile_pk=stats.profile_pk
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

pub(crate) fn ensure_profile_random_epoch(fingerprint: &[u8], random_epoch: i64) -> Result<(), String> {
	if !reads_enabled() {
		return Ok(());
	}
	let Some(cache) = SHADOW_CACHE.get() else {
		return Err("SQLite reads enabled but shadow store is unavailable".to_string());
	};
	let mut connection = cache.connection.lock().map_err(|_| "SQLite shadow connection lock poisoned".to_string())?;
	ensure_profile_random_epoch_on(&mut connection, fingerprint, random_epoch)
}

fn ensure_profile_random_epoch_on(connection: &mut Connection, fingerprint: &[u8], random_epoch: i64) -> Result<(), String> {
	let transaction = connection.transaction().map_err(|error| error.to_string())?;
	let (profile_pk, current_epoch): (i64, i64) = transaction
		.query_row("SELECT profile_pk,random_epoch FROM active_profiles WHERE fingerprint=?1", [fingerprint], |row| {
			Ok((row.get(0)?, row.get(1)?))
		})
		.map_err(|error| error.to_string())?;
	if current_epoch == random_epoch {
		return transaction.commit().map_err(|error| error.to_string());
	}
	transaction
		.execute(
			"UPDATE active_profiles SET random_epoch=?2,last_accessed_at=?3 WHERE profile_pk=?1",
			params![profile_pk, random_epoch, unix_now() as i64],
		)
		.map_err(|error| error.to_string())?;
	transaction
		.execute(
			"UPDATE profile_feed_entries
			 SET random_epoch=?2,
			     random_rank=redlib_random_rank(?3,?2,card_key)
			 WHERE profile_pk=?1 AND valid_to_generation IS NULL",
			params![profile_pk, random_epoch, fingerprint],
		)
		.map_err(|error| error.to_string())?;
	transaction.commit().map_err(|error| error.to_string())
}

fn sync_active_profile_on(connection: &mut Connection, fingerprint: &[u8], sources: &[(String, i64)]) -> Result<(), String> {
	let source_set_hash = profile_source_set_hash(sources);
	let transaction = connection.transaction().map_err(|error| error.to_string())?;
	reset_delta_tables(&transaction)?;
	evict_active_profiles_for_new(&transaction, fingerprint, configured_active_profile_limit()?)?;
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
	let source_set_unchanged = transaction
		.query_row(
			"SELECT source_set_hash=?2 FROM active_profile_source_sets WHERE profile_pk=?1",
			params![profile_pk, source_set_hash],
			|row| row.get::<_, i64>(0),
		)
		.optional()
		.map_err(|error| error.to_string())?
		.is_some_and(|matches| matches != 0);
	if source_set_unchanged {
		return transaction.commit().map_err(|error| error.to_string());
	}
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
			    WHERE existing.source_pk IS NULL)
			 + CASE WHEN
			     EXISTS(
			       SELECT 1
			       FROM profile_sources ps
			       JOIN source_memberships sm ON sm.source_pk=ps.source_pk
			       WHERE ps.profile_pk=?1
			     )
			     AND NOT EXISTS(
			       SELECT 1 FROM profile_view_object_refs refs WHERE refs.profile_pk=?1
			     )
			   THEN 1 ELSE 0 END",
			[profile_pk],
			|row| row.get(0),
		)
		.map_err(|error| error.to_string())?;
	if changed_source_count == 0 {
		transaction
			.execute(
				"INSERT INTO active_profile_source_sets(profile_pk,source_set_hash)
				 VALUES(?1,?2)
				 ON CONFLICT(profile_pk) DO UPDATE SET source_set_hash=excluded.source_set_hash",
				params![profile_pk, source_set_hash],
			)
			.map_err(|error| error.to_string())?;
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
	transaction
		.execute(
			"INSERT INTO active_profile_source_sets(profile_pk,source_set_hash)
			 VALUES(?1,?2)
			 ON CONFLICT(profile_pk) DO UPDATE SET source_set_hash=excluded.source_set_hash",
			params![profile_pk, source_set_hash],
		)
		.map_err(|error| error.to_string())?;
	transaction.commit().map_err(|error| error.to_string())
}

fn profile_source_set_hash(sources: &[(String, i64)]) -> Vec<u8> {
	let mut sorted = sources.to_vec();
	sorted.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
	let mut digest = Sha256::new();
	for (cache_key, role) in sorted {
		digest.update((cache_key.len() as u64).to_be_bytes());
		digest.update(cache_key.as_bytes());
		digest.update(role.to_be_bytes());
	}
	digest.finalize().to_vec()
}

fn evict_active_profiles_for_new(transaction: &Transaction<'_>, fingerprint: &[u8], limit: usize) -> Result<(), String> {
	let exists = transaction
		.query_row("SELECT 1 FROM active_profiles WHERE fingerprint=?1", [fingerprint], |row| row.get::<_, i64>(0))
		.optional()
		.map_err(|error| error.to_string())?
		.is_some();
	if exists {
		return Ok(());
	}
	let profile_count: i64 = transaction
		.query_row("SELECT COUNT(*) FROM active_profiles", [], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	let remove_count = profile_count.saturating_sub(limit.saturating_sub(1) as i64);
	if remove_count > 0 {
		transaction
			.execute(
				"DELETE FROM active_profiles
				 WHERE profile_pk IN (
				   SELECT profile_pk FROM active_profiles
				   ORDER BY last_accessed_at ASC,profile_pk ASC
				   LIMIT ?1
				 )",
				[remove_count],
			)
			.map_err(|error| error.to_string())?;
	}
	Ok(())
}

fn upsert_object(
	transaction: &Transaction<'_>,
	post: &PersistedPost,
	observed_at: u64,
	first_seen: FirstSeen,
	baseline_at: i64,
	baseline_seq: i64,
	payload_codec: PayloadCodec,
) -> Result<i64, String> {
	let fullname = fullname(post);
	let decoded_payload = bincode::serialize(post).map_err(|error| error.to_string())?;
	let payload_hash = sha256(&decoded_payload);
	let payload = payload_codec.encode(&decoded_payload)?;
	let payload_codec_value = payload_codec.database_value();
	let existing = transaction
		.query_row(
			"SELECT object_pk,discovery_seq,discovered_at,last_seen_at FROM objects WHERE fullname=?1",
			[&fullname],
			|row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?)),
		)
		.optional()
		.map_err(|error| error.to_string())?;
	let kind = post_kind(post);
	let submission_fullname = submission_fullname(post);
	let parent_fullname = normalized_fullname(&post.parent_id, if post.parent_kind == "t3" { 3 } else { 1 });
	let is_video = i64::from(matches!(post.post_type.as_str(), "gif" | "video") && post.media.duration_seconds > 0);
	let is_deleted = i64::from(post.author.name == "[deleted]" || (post.body == "[deleted]" && post.title == "[deleted]"));
	let is_promotional = i64::from(crate::promo_detection::post_is_promotional(&post.id));
	if let Some((object_pk, existing_discovery_seq, existing_discovered_at, existing_last_seen_at)) = existing {
		let (discovery_seq, discovered_at) = if first_seen == FirstSeen::Daemon && existing_discovery_seq == 0 {
			let sequence = meta_i64(transaction, "next_discovery_seq")?.unwrap_or_else(|| baseline_seq.saturating_add(1));
			set_meta_i64_tx(transaction, "next_discovery_seq", sequence.saturating_add(1))?;
			(sequence, observed_at as i64)
		} else {
			(existing_discovery_seq, existing_discovered_at)
		};
		if (observed_at as i64) < existing_last_seen_at {
			if discovery_seq != existing_discovery_seq {
				transaction
					.execute(
						"UPDATE objects SET discovery_seq=?2,discovered_at=?3 WHERE object_pk=?1",
						params![object_pk, discovery_seq, discovered_at],
					)
					.map_err(|error| error.to_string())?;
			}
			return Ok(object_pk);
		}
		transaction
			.execute(
				"UPDATE objects SET
				   kind=?2,created_utc=?3,link_created_utc=?4,author_lc=?5,community_lc=?6,
				   submission_fullname=?7,parent_fullname=?8,post_type=?9,duration_seconds=?10,
				   is_video=?11,is_deleted=?12,is_promotional=?13,last_seen_at=MAX(last_seen_at,?14),
				   payload_revision=payload_revision+1,payload_codec=?15,payload_hash=?16,payload=?17,
				   discovery_seq=?18,discovered_at=?19
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
					payload_codec_value,
					payload_hash,
					payload,
					discovery_seq,
					discovered_at
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
			 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,1,?19,?20,?21)",
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
				payload_codec_value,
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
				"INSERT OR IGNORE INTO temp_affected_objects(object_pk)
				 SELECT object_pk FROM duplicate_members
				 WHERE duplicate_group_pk IN (?1,?2)",
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
	} else if cache_key.starts_with("u_") || cache_key.strip_prefix("subreddit:").is_some_and(|value| value.starts_with("u_")) {
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

#[cfg(feature = "cache-sqlite-bench")]
pub fn run_shadow_verifier() -> Result<(), String> {
	use std::collections::HashMap;
	use std::hint::black_box;
	use std::time::Instant;

	let raw = std::env::args().skip(1).collect::<Vec<_>>();
	let mut values = HashMap::<String, String>::new();
	let mut index = 0;
	while index < raw.len() {
		let key = raw[index].clone();
		let value = raw.get(index + 1).ok_or_else(|| format!("missing value for {key}"))?.clone();
		values.insert(key, value);
		index += 2;
	}
	let cache_dir = values.remove("--cache-dir").map(PathBuf::from).ok_or("--cache-dir is required")?;
	let db_path = values.remove("--db").map(PathBuf::from).ok_or("--db is required")?;
	let iterations = values
		.remove("--iterations")
		.map_or(Ok(100usize), |value| value.parse().map_err(|_| "invalid --iterations"))?;
	let profile = values.remove("--profile").unwrap_or_else(|| "shadow-verifier-all-sources".to_string()).into_bytes();
	if !values.is_empty() {
		return Err(format!("unknown arguments: {:?}", values.keys().collect::<Vec<_>>()));
	}
	if db_path.extension().and_then(|value| value.to_str()) != Some("sqlite3") || (!db_path.starts_with("/private/tmp") && !db_path.starts_with("/tmp")) {
		return Err("verification database must be an explicit .sqlite3 path under /private/tmp or /tmp".to_string());
	}
	let payload_codec = PayloadCodec::from_environment()?;
	let mut connection = open_database(&db_path)?;
	let import_started = Instant::now();
	let import_stats = import_legacy_sources(&mut connection, &cache_dir, payload_codec)?;
	let import_elapsed = import_started.elapsed();

	let mut statement = connection
		.prepare("SELECT cache_key,source_kind FROM sources WHERE generation>0 ORDER BY cache_key")
		.map_err(|error| error.to_string())?;
	let source_rows = statement
		.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
		.map_err(|error| error.to_string())?;
	let mut sources = Vec::new();
	for row in source_rows {
		let (cache_key, source_kind) = row.map_err(|error| error.to_string())?;
		let role = match source_kind {
			1 => 1,
			2 => 2,
			_ => 0,
		};
		sources.push((cache_key, role));
	}
	drop(statement);
	let materialize_started = Instant::now();
	sync_active_profile_on(&mut connection, &profile, &sources)?;
	let materialize_elapsed = materialize_started.elapsed();

	let mut query_samples = Vec::with_capacity(iterations);
	let mut discovered_samples = Vec::with_capacity(iterations);
	let mut random_samples = Vec::with_capacity(iterations);
	let mut video_samples = Vec::with_capacity(iterations);
	let mut rendered_bytes = 0usize;
	let mut rendered_cards = 0usize;
	let mut render_samples = Vec::with_capacity(iterations);
	for _ in 0..iterations {
		let query_started = Instant::now();
		let page = query_profile_timeline_on(&connection, &profile, 1, TimelineSort::New, None, 25, &HashSet::new())?;
		query_samples.push(query_started.elapsed().as_micros() as u64);
		for (sort, samples) in [
			(TimelineSort::Discovered, &mut discovered_samples),
			(TimelineSort::Random, &mut random_samples),
			(TimelineSort::Videos, &mut video_samples),
		] {
			let started = Instant::now();
			let sorted_page = query_profile_timeline_on(&connection, &profile, 1, sort, None, 25, &HashSet::new())?;
			black_box(sorted_page.posts.len());
			samples.push(started.elapsed().as_micros() as u64);
		}
		let render_started = Instant::now();
		for post in page.posts {
			let runtime = post.into_runtime();
			let revision = crate::post_components::revision_for_post(&runtime);
			if revision.is_empty() {
				continue;
			}
			if let Some(rendered) = crate::post_components::render_latest_stable_card(&runtime.id)? {
				rendered_bytes = rendered_bytes.saturating_add(black_box(rendered.html.len()));
				rendered_cards += 1;
			}
		}
		render_samples.push(render_started.elapsed().as_micros() as u64);
	}
	query_samples.sort_unstable();
	discovered_samples.sort_unstable();
	random_samples.sort_unstable();
	video_samples.sort_unstable();
	render_samples.sort_unstable();
	let mut deep_samples = Vec::with_capacity(400);
	let mut deep_cursor = None;
	let mut deep_pages = 0usize;
	for _ in 0..400 {
		let started = Instant::now();
		let page = query_profile_timeline_on(&connection, &profile, 1, TimelineSort::New, deep_cursor.as_ref(), 25, &HashSet::new())?;
		deep_samples.push(started.elapsed().as_micros() as u64);
		if page.posts.is_empty() {
			break;
		}
		deep_pages += 1;
		black_box(page.posts.len());
		deep_cursor = page.next;
	}
	deep_samples.sort_unstable();
	let percentile = |samples: &[u64], percentile: usize| -> u64 {
		let index = samples.len().saturating_sub(1).saturating_mul(percentile) / 100;
		samples.get(index).copied().unwrap_or_default()
	};
	let quick_check: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	let object_count: i64 = connection
		.query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	let membership_count: i64 = connection
		.query_row("SELECT COUNT(*) FROM source_memberships", [], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	let feed_count: i64 = connection
		.query_row("SELECT COUNT(*) FROM profile_feed_entries WHERE valid_to_generation IS NULL", [], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	connection
		.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;")
		.map_err(|error| error.to_string())?;
	let db_bytes = fs::metadata(&db_path).map_err(|error| error.to_string())?.len();
	println!(
		"VERIFY codec={:?} page_size={} sync={} migration_batch={} scanned={} imported={} skipped={} objects_seen={} canonical_objects={} memberships={} active_feed_entries={} db_bytes={} import_ms={} materialize_ms={} new_us_p50={} new_us_p95={} new_us_p99={} discovered_us_p50={} random_us_p50={} videos_us_p50={} deep_pages={} deep_us_p50={} deep_us_p95={} deep_us_p99={} render25_us_p50={} render25_us_p95={} render25_us_p99={} rendered_cards={} rendered_bytes={} quick_check={}",
		payload_codec,
		page_size,
		configured_synchronous()?,
		configured_migration_batch_sources()?,
		import_stats.scanned_sources,
		import_stats.imported_sources,
		import_stats.skipped_sources,
		import_stats.objects_seen,
		object_count,
		membership_count,
		feed_count,
		db_bytes,
		import_elapsed.as_millis(),
		materialize_elapsed.as_millis(),
		percentile(&query_samples, 50),
		percentile(&query_samples, 95),
		percentile(&query_samples, 99),
		percentile(&discovered_samples, 50),
		percentile(&random_samples, 50),
		percentile(&video_samples, 50),
		deep_pages,
		percentile(&deep_samples, 50),
		percentile(&deep_samples, 95),
		percentile(&deep_samples, 99),
		percentile(&render_samples, 50),
		percentile(&render_samples, 95),
		percentile(&render_samples, 99),
		rendered_cards,
		rendered_bytes,
		quick_check
	);
	Ok(())
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
		let submission_fullname = if submission_id.starts_with("t3_") {
			submission_id.to_string()
		} else {
			format!("t3_{submission_id}")
		};
		value.kind = "t1".to_string();
		value.post_type = "comment".to_string();
		value.author.name = author.to_string();
		value.link_id = submission_fullname.clone();
		value.link_created_ts = created_ts.saturating_sub(100);
		value.parent_id = submission_fullname;
		value.parent_kind = "t3".to_string();
		value
	}

	fn test_connection() -> Connection {
		let connection = Connection::open_in_memory().unwrap();
		connection.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
		register_connection_functions(&connection).unwrap();
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
	fn daemon_discovery_is_monotonic_and_records_first_daemon_observation() {
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
		let interactive_before_daemon: i64 = connection
			.query_row("SELECT discovery_seq FROM objects WHERE fullname='t3_interactive'", [], |row| row.get(0))
			.unwrap();
		assert_eq!(interactive_before_daemon, 0);
		apply_source(
			&mut connection,
			"u_bob",
			&source(vec![post("interactive", 2), post("daemon-a", 3), post("daemon-b", 4)], Some(PersistedRefreshOrigin::Daemon)),
			&metadata(b"daemon", 300),
			FirstSeen::Daemon,
		)
		.unwrap();
		let interactive: i64 = connection
			.query_row("SELECT discovery_seq FROM objects WHERE fullname='t3_interactive'", [], |row| row.get(0))
			.unwrap();
		assert_eq!(interactive, 2);
		let sequences = connection
			.prepare("SELECT discovery_seq FROM objects WHERE fullname LIKE 't3_daemon-%' ORDER BY object_pk")
			.unwrap()
			.query_map([], |row| row.get::<_, i64>(0))
			.unwrap()
			.collect::<Result<Vec<_>, _>>()
			.unwrap();
		assert_eq!(sequences, vec![3, 4]);
	}

	#[test]
	fn source_kinds_distinguish_followed_user_submitted_and_comments() {
		assert_eq!(source_kind("subreddit:rust:new"), 0);
		assert_eq!(source_kind("subreddit:u_alice:new"), 1);
		assert_eq!(source_kind("u_alice"), 1);
		assert_eq!(source_kind("user_comments:u_alice:new"), 2);
		assert_eq!(source_kind("flair:rust:news:new"), 3);
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
	fn older_source_occurrence_does_not_overwrite_fresher_canonical_payload() {
		let mut connection = test_connection();
		let mut fresh_post = post("shared", 10);
		fresh_post.title = "fresh payload".to_string();
		let mut fresh_source = source(vec![fresh_post], None);
		fresh_source.fetched_at_unix = 2_000;
		apply_source(&mut connection, "subreddit:fresh:new", &fresh_source, &metadata(b"fresh", 100), FirstSeen::Migration).unwrap();

		let mut stale_post = post("shared", 10);
		stale_post.title = "stale payload".to_string();
		let mut stale_source = source(vec![stale_post], None);
		stale_source.fetched_at_unix = 1_000;
		apply_source(&mut connection, "subreddit:stale:new", &stale_source, &metadata(b"stale", 200), FirstSeen::Migration).unwrap();

		let (codec, payload): (i64, Vec<u8>) = connection
			.query_row("SELECT payload_codec,payload FROM objects WHERE fullname='t3_shared'", [], |row| {
				Ok((row.get(0)?, row.get(1)?))
			})
			.unwrap();
		assert_eq!(decode_payload(codec, &payload).unwrap().title, "fresh payload");
		assert_eq!(
			connection
				.query_row("SELECT last_seen_at FROM objects WHERE fullname='t3_shared'", [], |row| row.get::<_, i64>(0))
				.unwrap(),
			2_000
		);
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
		let generation_before_fast_path: i64 = connection
			.query_row("SELECT generation FROM active_profiles WHERE fingerprint=?1", [b"profile-a".as_slice()], |row| row.get(0))
			.unwrap();
		connection.execute("DELETE FROM active_profile_source_sets", []).unwrap();
		sync_active_profile_on(&mut connection, b"profile-a", &[("rust".to_string(), 0)]).unwrap();
		assert_eq!(
			connection
				.query_row("SELECT generation FROM active_profiles WHERE fingerprint=?1", [b"profile-a".as_slice()], |row| row
					.get::<_, i64>(0))
				.unwrap(),
			generation_before_fast_path
		);
		assert_eq!(
			connection
				.query_row("SELECT COUNT(*) FROM active_profile_source_sets", [], |row| row.get::<_, i64>(0))
				.unwrap(),
			1
		);
		sync_active_profile_on(&mut connection, b"profile-a", &[("rust".to_string(), 0)]).unwrap();
		assert_eq!(
			connection
				.query_row("SELECT generation FROM active_profiles WHERE fingerprint=?1", [b"profile-a".as_slice()], |row| row
					.get::<_, i64>(0))
				.unwrap(),
			generation_before_fast_path
		);
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
		let diagnostic: (i64, i64, i64, i64, i64) = connection
			.query_row(
				"SELECT
				   (SELECT COUNT(*) FROM thread_groups),
				   (SELECT COUNT(*) FROM thread_members),
				   (SELECT COUNT(*) FROM profile_sources),
				   (SELECT COUNT(*) FROM temp_affected_threads),
				   (SELECT COUNT(*) FROM profile_object_refs)",
				[],
				|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
			)
			.unwrap();
		assert_eq!(diagnostic, (1, 2, 2, 1, 2));
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
		let page = query_profile_timeline_on(&connection, b"profile-comments", 2, TimelineSort::New, None, 25, &HashSet::new()).unwrap();
		assert_eq!(page.posts.len(), 1);
		assert_eq!(page.posts[0].following_comments.len(), 2);
	}

	#[test]
	fn temporal_keyset_cursor_keeps_snapshot_stable_across_refresh() {
		let mut connection = test_connection();
		apply_source(
			&mut connection,
			"subreddit:rust:new",
			&source(vec![post("older", 10), post("newer", 20)], None),
			&metadata(b"initial", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		sync_active_profile_on(&mut connection, b"timeline-profile", &[("subreddit:rust:new".to_string(), 0)]).unwrap();
		let first = query_profile_timeline_on(&connection, b"timeline-profile", 1, TimelineSort::New, None, 1, &HashSet::new()).unwrap();
		assert_eq!(first.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["newer"]);
		let cursor = first.next.unwrap();

		apply_source(
			&mut connection,
			"subreddit:rust:new",
			&source(vec![post("latest", 30)], Some(PersistedRefreshOrigin::Daemon)),
			&metadata(b"refresh", 200),
			FirstSeen::Daemon,
		)
		.unwrap();
		let continuation = query_profile_timeline_on(&connection, b"timeline-profile", 1, TimelineSort::New, Some(&cursor), 10, &HashSet::new()).unwrap();
		assert_eq!(continuation.next.as_ref().map(|next| next.generation), Some(cursor.generation));
		assert_eq!(continuation.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["older"]);
		let current = query_profile_timeline_on(&connection, b"timeline-profile", 1, TimelineSort::New, None, 10, &HashSet::new()).unwrap();
		assert_eq!(current.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["latest"]);
	}

	#[test]
	fn submitted_query_is_direct_author_chronology_with_filters() {
		let mut connection = test_connection();
		let mut other_community = post("other", 30);
		other_community.community = "programming".to_string();
		let mut other_author = post("bob", 40);
		other_author.author.name = "bob".to_string();
		apply_source(
			&mut connection,
			"subreddit:rust:new",
			&source(vec![post("alice-old", 10), post("alice-new", 20), other_community, other_author], None),
			&metadata(b"submitted", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		let mut filters = HashSet::new();
		filters.insert("programming".to_string());
		let (first, cursor) = query_submitted_on(&connection, "u_alice", None, 1, &filters).unwrap();
		assert_eq!(first.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["alice-new"]);
		let (second, _) = query_submitted_on(&connection, "alice", cursor.as_ref(), 10, &filters).unwrap();
		assert_eq!(second.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["alice-old"]);

		filters.insert("u_alice".to_string());
		let (filtered_author, _) = query_submitted_on(&connection, "alice", None, 10, &filters).unwrap();
		assert!(filtered_author.is_empty());
	}

	#[test]
	fn file_importer_resumes_by_checksum_and_reimports_only_changed_source() {
		let root = std::env::temp_dir().join(format!("redlib-cache-sqlite-import-test-{}", uuid::Uuid::new_v4()));
		let source_dir = root.join("sources.v2");
		fs::create_dir_all(&source_dir).unwrap();
		let rust_path = source_dir.join(format!("{}.bin", URL_SAFE_NO_PAD.encode("subreddit:rust:new")));
		let programming_path = source_dir.join(format!("{}.bin", URL_SAFE_NO_PAD.encode("subreddit:programming:new")));
		fs::write(&rust_path, bincode::serialize(&source(vec![post("rust", 10)], None)).unwrap()).unwrap();
		fs::write(&programming_path, bincode::serialize(&source(vec![post("programming", 20)], None)).unwrap()).unwrap();
		let mut connection = open_database(&root.join("cache.sqlite3")).unwrap();
		let first = import_legacy_sources(&mut connection, &root, PayloadCodec::Raw).unwrap();
		assert_eq!((first.imported_sources, first.skipped_sources), (2, 0));
		let second = import_legacy_sources(&mut connection, &root, PayloadCodec::Raw).unwrap();
		assert_eq!((second.imported_sources, second.skipped_sources), (0, 2));

		fs::write(&rust_path, bincode::serialize(&source(vec![post("rust-new", 30), post("rust", 10)], None)).unwrap()).unwrap();
		let third = import_legacy_sources(&mut connection, &root, PayloadCodec::Raw).unwrap();
		assert_eq!((third.imported_sources, third.skipped_sources), (1, 1));
		let baselines: i64 = connection.query_row("SELECT COUNT(DISTINCT discovery_seq) FROM objects", [], |row| row.get(0)).unwrap();
		assert_eq!(baselines, 1);
		drop(connection);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn migration_batch_is_atomic_and_replays_after_failure() {
		let mut connection = test_connection();
		connection
			.execute_batch(
				"CREATE TEMP TRIGGER reject_bad_source
				 BEFORE INSERT ON sources
				 WHEN NEW.cache_key='bad'
				 BEGIN SELECT RAISE(ABORT,'injected batch failure'); END;",
			)
			.unwrap();
		let pending = vec![
			PreparedSourceImport {
				cache_key: "good".to_string(),
				entry: source(vec![post("good", 10)], None),
				metadata: metadata(b"good", 100),
			},
			PreparedSourceImport {
				cache_key: "bad".to_string(),
				entry: source(vec![post("bad", 20)], None),
				metadata: metadata(b"bad", 100),
			},
		];
		let mut stats = ImportStats::default();
		assert!(commit_import_batch(&mut connection, &pending, PayloadCodec::Raw, &mut stats).is_err());
		assert_eq!(connection.query_row("SELECT COUNT(*) FROM sources", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
		assert_eq!(connection.query_row("SELECT COUNT(*) FROM objects", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
		assert_eq!(stats.imported_sources, 0);

		connection.execute_batch("DROP TRIGGER reject_bad_source;").unwrap();
		commit_import_batch(&mut connection, &pending, PayloadCodec::Raw, &mut stats).unwrap();
		assert_eq!(connection.query_row("SELECT COUNT(*) FROM migration_sources", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
		assert_eq!(stats.imported_sources, 2);
	}

	#[test]
	fn random_rank_is_profile_keyed_deterministic_and_epoch_sensitive() {
		let profile = b"profile-fingerprint";
		let first = deterministic_random_rank(profile, 7, "d:123");
		assert_eq!(first, deterministic_random_rank(profile, 7, "d:123"));
		assert_ne!(first, deterministic_random_rank(b"other-profile", 7, "d:123"));
		assert_ne!(first, deterministic_random_rank(profile, 8, "d:123"));
		assert_ne!(first, deterministic_random_rank(profile, 7, "d:456"));

		let mut connection = test_connection();
		apply_source(
			&mut connection,
			"subreddit:rust:new",
			&source(vec![post("ranked", 10)], None),
			&metadata(b"ranked", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		sync_active_profile_on(&mut connection, profile, &[("subreddit:rust:new".to_string(), 0)]).unwrap();
		let (card_key, epoch, stored_rank): (String, i64, i64) = connection
			.query_row(
				"SELECT feed.card_key,feed.random_epoch,feed.random_rank
				 FROM profile_feed_entries feed
				 WHERE feed.valid_to_generation IS NULL",
				[],
				|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
			)
			.unwrap();
		assert_eq!(stored_rank, deterministic_random_rank(profile, epoch, &card_key));
		ensure_profile_random_epoch_on(&mut connection, profile, epoch + 1).unwrap();
		let (rotated_epoch, rotated_rank): (i64, i64) = connection
			.query_row(
				"SELECT random_epoch,random_rank
				 FROM profile_feed_entries
				 WHERE valid_to_generation IS NULL",
				[],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.unwrap();
		assert_eq!(rotated_epoch, epoch + 1);
		assert_eq!(rotated_rank, deterministic_random_rank(profile, rotated_epoch, &card_key));
		assert_ne!(stored_rank, rotated_rank);
	}

	#[test]
	fn profile_source_set_hash_is_order_independent_and_role_sensitive() {
		let first = vec![("subreddit:rust:new".to_string(), 0), ("u_alice".to_string(), 1)];
		let reversed = vec![("u_alice".to_string(), 1), ("subreddit:rust:new".to_string(), 0)];
		let changed_role = vec![("subreddit:rust:new".to_string(), 0), ("u_alice".to_string(), 2)];
		assert_eq!(profile_source_set_hash(&first), profile_source_set_hash(&reversed));
		assert_ne!(profile_source_set_hash(&first), profile_source_set_hash(&changed_role));
	}

	#[test]
	fn active_profile_limit_evicts_oldest_profiles_before_insert() {
		let mut connection = test_connection();
		let transaction = connection.transaction().unwrap();
		for (fingerprint, last_accessed_at) in [(b"oldest".as_slice(), 1), (b"middle".as_slice(), 2), (b"newest".as_slice(), 3)] {
			transaction
				.execute(
					"INSERT INTO active_profiles(fingerprint,generation,random_epoch,last_accessed_at)
					 VALUES(?1,0,1,?2)",
					params![fingerprint, last_accessed_at],
				)
				.unwrap();
		}
		evict_active_profiles_for_new(&transaction, b"incoming", 2).unwrap();
		let remaining = transaction
			.prepare("SELECT CAST(fingerprint AS TEXT) FROM active_profiles ORDER BY profile_pk")
			.unwrap()
			.query_map([], |row| row.get::<_, String>(0))
			.unwrap()
			.collect::<Result<Vec<_>, _>>()
			.unwrap();
		assert_eq!(remaining, vec!["newest"]);
		transaction.rollback().unwrap();
	}

	#[test]
	fn videos_query_uses_materialized_duration_order_and_video_payloads() {
		let mut connection = test_connection();
		let mut short = post("short-video", 30);
		short.post_type = "video".to_string();
		short.media.url = "https://v.redd.it/short/DASH_720.mp4".to_string();
		short.media.duration_seconds = 15;
		let mut long = post("long-video", 20);
		long.post_type = "video".to_string();
		long.media.url = "https://v.redd.it/long/DASH_720.mp4".to_string();
		long.media.duration_seconds = 120;
		apply_source(
			&mut connection,
			"subreddit:rust:new",
			&source(vec![short, post("text", 40), long], None),
			&metadata(b"videos", 100),
			FirstSeen::Migration,
		)
		.unwrap();
		sync_active_profile_on(&mut connection, b"videos-profile", &[("subreddit:rust:new".to_string(), 0)]).unwrap();
		let page = query_profile_timeline_on(&connection, b"videos-profile", 1, TimelineSort::Videos, None, 25, &HashSet::new()).unwrap();
		assert_eq!(page.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["long-video", "short-video"]);
		assert!(page.posts.iter().all(|post| post.media.duration_seconds > 0));
	}

	#[test]
	fn zstd_payloads_round_trip_through_timeline_hydration() {
		let mut connection = test_connection();
		apply_source_with_codec(
			&mut connection,
			"subreddit:rust:new",
			&source(vec![post("compressed", 10)], None),
			&metadata(b"compressed", 100),
			FirstSeen::Migration,
			PayloadCodec::Zstd,
		)
		.unwrap();
		sync_active_profile_on(&mut connection, b"zstd-profile", &[("subreddit:rust:new".to_string(), 0)]).unwrap();
		let codec: i64 = connection.query_row("SELECT payload_codec FROM objects", [], |row| row.get(0)).unwrap();
		assert_eq!(codec, 1);
		let page = query_profile_timeline_on(&connection, b"zstd-profile", 1, TimelineSort::New, None, 25, &HashSet::new()).unwrap();
		assert_eq!(page.posts.iter().map(|post| post.id.as_str()).collect::<Vec<_>>(), vec!["compressed"]);
	}

	#[test]
	fn supported_page_sizes_and_durability_modes_are_applied() {
		for (page_size, synchronous, expected_sync) in [(8192, "FULL", 2i64), (16384, "NORMAL", 1i64)] {
			let root = std::env::temp_dir().join(format!("redlib-cache-sqlite-options-test-{}", uuid::Uuid::new_v4()));
			let connection = open_database_with_options(&root.join("cache.sqlite3"), page_size, synchronous).unwrap();
			let actual_page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0)).unwrap();
			let actual_sync: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
			assert_eq!(actual_page_size, page_size);
			assert_eq!(actual_sync, expected_sync);
			drop(connection);
			fs::remove_dir_all(root).unwrap();
		}
	}
}
