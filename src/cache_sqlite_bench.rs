//! Disposable real-cache comparator for the SQLite cache architecture study.
//!
//! This module is compiled only with `--features cache-sqlite-bench`. It is not
//! used by the Redlib server and deliberately writes only to an explicit test DB.
//! Its text-key, separate-payload and global-card tables reproduce the rejected
//! baseline; the reviewed production shape is documented in
//! `docs/cache-sqlite-architecture.md` and deliberately uses ordinary ROWID
//! canonical object rows with profile-local materialization.

use crate::cache_persistence::{PersistedPost, PersistedSourceCacheEntry};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA foreign_keys=ON;
PRAGMA temp_store=FILE;
PRAGMA cache_size=-8192;
PRAGMA mmap_size=0;
PRAGMA wal_autocheckpoint=1000;

CREATE TABLE IF NOT EXISTS cache_meta(key TEXT PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS items(
    object_pk INTEGER PRIMARY KEY,
    item_id TEXT NOT NULL UNIQUE,
    kind INTEGER NOT NULL,
    created_utc INTEGER NOT NULL,
    link_created_utc INTEGER NOT NULL,
    author_lc TEXT NOT NULL,
    community_lc TEXT NOT NULL,
    submission_id TEXT NOT NULL,
    parent_id TEXT NOT NULL,
    post_type TEXT NOT NULL,
    duration_seconds INTEGER NOT NULL,
    is_video INTEGER NOT NULL,
    first_seen_origin INTEGER NOT NULL,
    first_seen_at INTEGER NOT NULL,
    discovery_seq INTEGER NOT NULL,
    discovered_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    payload_hash BLOB NOT NULL,
    payload BLOB NOT NULL,
    group_id INTEGER,
    card_key TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS sources(
    source_id INTEGER PRIMARY KEY,
    cache_key TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    generation INTEGER NOT NULL,
    fetched_at INTEGER NOT NULL,
    after_cursor TEXT NOT NULL,
    content_hash BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS source_memberships(
    source_id INTEGER NOT NULL REFERENCES sources(source_id) ON DELETE CASCADE,
    object_pk INTEGER NOT NULL REFERENCES items(object_pk) ON DELETE CASCADE,
    source_position INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY(source_id,object_pk)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS source_memberships_item ON source_memberships(object_pk,source_id);
CREATE TABLE IF NOT EXISTS duplicate_groups(group_id INTEGER PRIMARY KEY);
CREATE TABLE IF NOT EXISTS duplicate_keys(
    duplicate_key BLOB PRIMARY KEY,
    group_id INTEGER NOT NULL REFERENCES duplicate_groups(group_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS duplicate_members(
    group_id INTEGER NOT NULL REFERENCES duplicate_groups(group_id) ON DELETE CASCADE,
    object_pk INTEGER NOT NULL REFERENCES items(object_pk) ON DELETE CASCADE,
    PRIMARY KEY(group_id,object_pk),
    UNIQUE(object_pk)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS profiles(
    profile_id INTEGER PRIMARY KEY,
    profile_name_lc TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS source_profiles(
    source_id INTEGER NOT NULL REFERENCES sources(source_id) ON DELETE CASCADE,
    profile_id INTEGER NOT NULL REFERENCES profiles(profile_id) ON DELETE CASCADE,
    PRIMARY KEY(source_id,profile_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS thread_groups(
    submission_id TEXT PRIMARY KEY,
    representative_pk INTEGER,
    created_sort INTEGER NOT NULL,
    activity_sort INTEGER NOT NULL,
    discovery_sort INTEGER NOT NULL,
    member_count INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS thread_members(
    submission_id TEXT NOT NULL,
    object_pk INTEGER NOT NULL REFERENCES items(object_pk) ON DELETE CASCADE,
    PRIMARY KEY(submission_id,object_pk),
    UNIQUE(object_pk)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS feed_entries(
    feed_id INTEGER NOT NULL,
    card_key TEXT NOT NULL,
    representative_pk INTEGER NOT NULL REFERENCES items(object_pk),
    created_sort INTEGER NOT NULL,
    activity_sort INTEGER NOT NULL,
    discovery_sort INTEGER NOT NULL,
    duration_seconds INTEGER NOT NULL,
    random_epoch INTEGER NOT NULL,
    random_round INTEGER NOT NULL,
    random_rank INTEGER NOT NULL,
    member_count INTEGER NOT NULL,
    PRIMARY KEY(feed_id,card_key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS feed_new ON feed_entries(feed_id,created_sort DESC,card_key DESC);
CREATE INDEX IF NOT EXISTS feed_activity ON feed_entries(feed_id,activity_sort DESC,card_key DESC);
CREATE INDEX IF NOT EXISTS feed_discovered ON feed_entries(feed_id,discovery_sort DESC,card_key DESC);
CREATE INDEX IF NOT EXISTS feed_random ON feed_entries(feed_id,random_epoch,random_round,random_rank,card_key);
CREATE INDEX IF NOT EXISTS feed_videos ON feed_entries(feed_id,duration_seconds DESC,created_sort DESC,card_key DESC) WHERE duration_seconds>0;
CREATE TABLE IF NOT EXISTS profile_feed_entries(
    profile_id INTEGER NOT NULL REFERENCES profiles(profile_id) ON DELETE CASCADE,
    card_key TEXT NOT NULL,
    representative_pk INTEGER NOT NULL REFERENCES items(object_pk),
    created_sort INTEGER NOT NULL,
    activity_sort INTEGER NOT NULL,
    discovery_sort INTEGER NOT NULL,
    duration_seconds INTEGER NOT NULL,
    PRIMARY KEY(profile_id,card_key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS profile_new ON profile_feed_entries(profile_id,created_sort DESC,card_key DESC);
CREATE INDEX IF NOT EXISTS profile_activity ON profile_feed_entries(profile_id,activity_sort DESC,card_key DESC);
CREATE INDEX IF NOT EXISTS profile_discovered ON profile_feed_entries(profile_id,discovery_sort DESC,card_key DESC);
CREATE INDEX IF NOT EXISTS profile_videos ON profile_feed_entries(profile_id,duration_seconds DESC,created_sort DESC,card_key DESC) WHERE duration_seconds>0;
CREATE TABLE IF NOT EXISTS migration_sources(
    cache_key TEXT PRIMARY KEY,
    source_size INTEGER NOT NULL,
    source_hash BLOB NOT NULL,
    item_count INTEGER NOT NULL
) WITHOUT ROWID;
"#;

#[derive(Debug)]
struct Args {
	source_dir: PathBuf,
	db_path: PathBuf,
	iterations: usize,
	page_size: usize,
	db_page_size: usize,
	selected_sources: usize,
	compression: String,
}

#[derive(Default)]
struct ImportStats {
	sources: usize,
	memberships: usize,
	decoded_payload_bytes: u64,
}

pub fn run() -> Result<(), String> {
	let raw = std::env::args().skip(1).collect::<Vec<_>>();
	if raw.first().is_some_and(|arg| arg == "--crash-child") {
		return crash_child(&raw);
	}
	let args = parse_args(&raw)?;
	validate_db_target(&args.db_path)?;
	remove_db_files(&args.db_path)?;

	let mut conn = Connection::open(&args.db_path).map_err(|error| error.to_string())?;
	conn.execute_batch(&format!("PRAGMA page_size={};", args.db_page_size)).map_err(|error| error.to_string())?;
	configure(&conn)?;
	conn.execute_batch(SCHEMA).map_err(|error| error.to_string())?;
	let import_started = Instant::now();
	let stats = import_sources(&mut conn, &args.source_dir, &args.compression)?;
	let import_elapsed = import_started.elapsed();

	let materialize_started = Instant::now();
	materialize(&mut conn)?;
	let materialize_elapsed = materialize_started.elapsed();
	conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;").map_err(|error| error.to_string())?;

	let counts = counts(&conn)?;
	println!(
		"IMPORT sources={} memberships={} unique_items={} feed_entries={} profiles={} decoded_bytes={} import_ms={} materialize_ms={}",
		stats.sources,
		stats.memberships,
		counts[0],
		counts[1],
		counts[2],
		stats.decoded_payload_bytes,
		import_elapsed.as_millis(),
		materialize_elapsed.as_millis()
	);

	bench_queries(&conn, &args)?;
	bench_hydration(&conn, args.page_size, args.iterations, &args.compression)?;
	bench_refresh(&mut conn, args.iterations.min(100).max(20))?;
	drop(conn);
	bench_concurrency(&args.db_path, args.iterations.min(200).max(50))?;
	bench_crash_recovery(&args.db_path)?;

	let conn = Connection::open(&args.db_path).map_err(|error| error.to_string())?;
	configure(&conn)?;
	let quick_check: String = conn.query_row("PRAGMA quick_check", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	let page_count: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	println!(
		"FINAL db_bytes={} page_count={} page_size={} quick_check={} rss_bytes={}",
		fs::metadata(&args.db_path).map_err(|error| error.to_string())?.len(),
		page_count,
		page_size,
		quick_check,
		current_rss_bytes()
	);
	Ok(())
}

fn parse_args(raw: &[String]) -> Result<Args, String> {
	let mut values = HashMap::<String, String>::new();
	let mut index = 0;
	while index < raw.len() {
		let key = raw[index].clone();
		let value = raw.get(index + 1).ok_or_else(|| format!("missing value for {key}"))?.clone();
		values.insert(key, value);
		index += 2;
	}
	let source_dir = values.remove("--source-dir").map(PathBuf::from).ok_or("--source-dir is required")?;
	let db_path = values.remove("--db").map(PathBuf::from).ok_or("--db is required")?;
	let iterations = values
		.remove("--iterations")
		.map_or(Ok(200usize), |value| value.parse().map_err(|_| "invalid --iterations"))?;
	let page_size = values.remove("--page-size").map_or(Ok(25usize), |value| value.parse().map_err(|_| "invalid --page-size"))?;
	let db_page_size = values
		.remove("--db-page-size")
		.map_or(Ok(8192usize), |value| value.parse().map_err(|_| "invalid --db-page-size"))?;
	if !matches!(db_page_size, 4096 | 8192 | 16384 | 32768 | 65536) {
		return Err("--db-page-size must be 4096, 8192, 16384, 32768, or 65536".into());
	}
	let compression = values.remove("--compression").unwrap_or_else(|| "raw".into());
	if !matches!(compression.as_str(), "raw" | "zstd") {
		return Err("--compression must be raw or zstd".into());
	}
	let selected_sources = values
		.remove("--selected-sources")
		.map_or(Ok(100usize), |value| value.parse().map_err(|_| "invalid --selected-sources"))?;
	if !values.is_empty() {
		return Err(format!("unknown arguments: {:?}", values.keys().collect::<Vec<_>>()));
	}
	Ok(Args {
		source_dir,
		db_path,
		iterations,
		page_size,
		db_page_size,
		selected_sources,
		compression,
	})
}

fn validate_db_target(path: &Path) -> Result<(), String> {
	if path.extension().and_then(|value| value.to_str()) != Some("sqlite3") {
		return Err("benchmark DB path must end in .sqlite3".into());
	}
	if !path.starts_with("/private/tmp/") && !path.starts_with("/tmp/") {
		return Err("benchmark DB must be under /private/tmp or /tmp".into());
	}
	Ok(())
}

fn remove_db_files(path: &Path) -> Result<(), String> {
	for candidate in [
		path.to_path_buf(),
		PathBuf::from(format!("{}-wal", path.display())),
		PathBuf::from(format!("{}-shm", path.display())),
	] {
		match fs::remove_file(candidate) {
			Ok(()) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.to_string()),
		}
	}
	Ok(())
}

fn configure(conn: &Connection) -> Result<(), String> {
	conn.busy_timeout(Duration::from_secs(5)).map_err(|error| error.to_string())?;
	conn
		.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA temp_store=FILE; PRAGMA cache_size=-8192; PRAGMA mmap_size=0;")
		.map_err(|error| error.to_string())
}

fn import_sources(conn: &mut Connection, source_dir: &Path, compression: &str) -> Result<ImportStats, String> {
	let mut files = fs::read_dir(source_dir)
		.map_err(|error| error.to_string())?
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| path.extension().and_then(|value| value.to_str()) == Some("bin"))
		.collect::<Vec<_>>();
	files.sort();
	let mut stats = ImportStats::default();
	for batch in files.chunks(32) {
		let tx = conn.transaction().map_err(|error| error.to_string())?;
		for path in batch {
			let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
			let entry: PersistedSourceCacheEntry = bincode::deserialize(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
			let stem = path.file_stem().and_then(|value| value.to_str()).ok_or("invalid source filename")?;
			let cache_key = String::from_utf8(URL_SAFE_NO_PAD.decode(stem).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
			import_source(&tx, &cache_key, &entry, bytes.len() as i64, sha256(&bytes), compression)?;
			stats.sources += 1;
			stats.memberships += entry.posts.len();
			stats.decoded_payload_bytes += bytes.len() as u64;
		}
		tx.commit().map_err(|error| error.to_string())?;
	}
	Ok(stats)
}

fn import_source(tx: &Transaction<'_>, cache_key: &str, entry: &PersistedSourceCacheEntry, source_size: i64, source_hash: Vec<u8>, compression: &str) -> Result<(), String> {
	tx.execute(
		"INSERT INTO sources(cache_key,display_name,generation,fetched_at,after_cursor,content_hash)
		 VALUES(?1,?2,1,?3,?4,?5)
		 ON CONFLICT(cache_key) DO UPDATE SET generation=generation+1,fetched_at=excluded.fetched_at,after_cursor=excluded.after_cursor,content_hash=excluded.content_hash",
		params![cache_key, entry.subreddit, entry.fetched_at_unix as i64, entry.after, source_hash],
	)
	.map_err(|error| error.to_string())?;
	let source_id: i64 = tx
		.query_row("SELECT source_id FROM sources WHERE cache_key=?1", [cache_key], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	let generation: i64 = tx
		.query_row("SELECT generation FROM sources WHERE source_id=?1", [source_id], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	tx.execute("DELETE FROM source_memberships WHERE source_id=?1", [source_id])
		.map_err(|error| error.to_string())?;
	if let Some(profile) = profile_from_source(cache_key, &entry.subreddit) {
		tx.execute("INSERT INTO profiles(profile_name_lc) VALUES(?1) ON CONFLICT(profile_name_lc) DO NOTHING", [&profile])
			.map_err(|error| error.to_string())?;
		let profile_id: i64 = tx
			.query_row("SELECT profile_id FROM profiles WHERE profile_name_lc=?1", [&profile], |row| row.get(0))
			.map_err(|error| error.to_string())?;
		tx.execute("INSERT OR IGNORE INTO source_profiles(source_id,profile_id) VALUES(?1,?2)", params![source_id, profile_id])
			.map_err(|error| error.to_string())?;
	}
	for (position, post) in entry.posts.iter().enumerate() {
		let object_pk = upsert_item(tx, post, entry.fetched_at_unix, compression)?;
		tx.execute(
			"INSERT INTO source_memberships(source_id,object_pk,source_position,generation) VALUES(?1,?2,?3,?4)",
			params![source_id, object_pk, position as i64, generation],
		)
		.map_err(|error| error.to_string())?;
	}
	tx.execute(
		"INSERT INTO migration_sources(cache_key,source_size,source_hash,item_count) VALUES(?1,?2,?3,?4)
		 ON CONFLICT(cache_key) DO UPDATE SET source_size=excluded.source_size,source_hash=excluded.source_hash,item_count=excluded.item_count",
		params![cache_key, source_size, source_hash, entry.posts.len() as i64],
	)
	.map_err(|error| error.to_string())?;
	Ok(())
}

fn upsert_item(tx: &Transaction<'_>, post: &PersistedPost, observed_at: u64, compression: &str) -> Result<i64, String> {
	let payload = bincode::serialize(post).map_err(|error| error.to_string())?;
	let payload_hash = sha256(&payload);
	let stored_payload = match compression {
		"zstd" => zstd::stream::encode_all(payload.as_slice(), 1).map_err(|error| error.to_string())?,
		_ => payload,
	};
	let kind = if post.kind == "t1" || post.post_type == "comment" { 1i64 } else { 3i64 };
	let submission_id = if kind == 1 {
		if post.link_id.trim().is_empty() {
			post.parent_id.clone()
		} else {
			post.link_id.clone()
		}
	} else {
		post.id.clone()
	};
	let is_video = i64::from(matches!(post.post_type.as_str(), "video" | "gif") && post.media.duration_seconds > 0);
	let group_id = duplicate_group(tx, post)?;
	tx.execute(
		"INSERT INTO items(item_id,kind,created_utc,link_created_utc,author_lc,community_lc,submission_id,parent_id,post_type,duration_seconds,is_video,
		 first_seen_origin,first_seen_at,discovery_seq,discovered_at,last_seen_at,payload_hash,payload,group_id)
		 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,3,?12,1,?12,?12,?13,?14,?15)
		 ON CONFLICT(item_id) DO UPDATE SET kind=excluded.kind,created_utc=excluded.created_utc,link_created_utc=excluded.link_created_utc,
		 author_lc=excluded.author_lc,community_lc=excluded.community_lc,submission_id=excluded.submission_id,parent_id=excluded.parent_id,
		 post_type=excluded.post_type,duration_seconds=excluded.duration_seconds,is_video=excluded.is_video,last_seen_at=MAX(items.last_seen_at,excluded.last_seen_at),
		 payload_hash=CASE WHEN excluded.last_seen_at>=items.last_seen_at THEN excluded.payload_hash ELSE items.payload_hash END,
		 payload=CASE WHEN excluded.last_seen_at>=items.last_seen_at THEN excluded.payload ELSE items.payload END,group_id=excluded.group_id",
		params![
			post.id,
			kind,
			post.created_ts as i64,
			post.link_created_ts as i64,
			post.author.name.trim().to_lowercase(),
			post.community.trim().to_lowercase(),
			submission_id,
			post.parent_id,
			post.post_type,
			post.media.duration_seconds,
			is_video,
			observed_at as i64,
			payload_hash,
			stored_payload,
			group_id,
		],
	)
	.map_err(|error| error.to_string())?;
	let object_pk: i64 = tx
		.query_row("SELECT object_pk FROM items WHERE item_id=?1", [&post.id], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	tx.execute(
		"INSERT OR IGNORE INTO duplicate_members(group_id,object_pk) VALUES(?1,?2)",
		params![group_id, object_pk],
	)
		.map_err(|error| error.to_string())?;
	Ok(object_pk)
}

fn duplicate_group(tx: &Transaction<'_>, post: &PersistedPost) -> Result<i64, String> {
	let keys = duplicate_keys(post);
	let mut groups = BTreeSet::new();
	for key in &keys {
		if let Some(group_id) = tx
			.query_row("SELECT group_id FROM duplicate_keys WHERE duplicate_key=?1", [key], |row| row.get::<_, i64>(0))
			.optional()
			.map_err(|error| error.to_string())?
		{
			groups.insert(group_id);
		}
	}
	let target = if let Some(group_id) = groups.iter().next().copied() {
		group_id
	} else {
		tx.execute("INSERT INTO duplicate_groups DEFAULT VALUES", []).map_err(|error| error.to_string())?;
		tx.last_insert_rowid()
	};
	for group_id in groups.into_iter().filter(|group_id| *group_id != target) {
		tx.execute("UPDATE duplicate_keys SET group_id=?1 WHERE group_id=?2", params![target, group_id])
			.map_err(|error| error.to_string())?;
		tx.execute("UPDATE OR IGNORE duplicate_members SET group_id=?1 WHERE group_id=?2", params![target, group_id])
			.map_err(|error| error.to_string())?;
		tx.execute("DELETE FROM duplicate_members WHERE group_id=?1", [group_id])
			.map_err(|error| error.to_string())?;
		tx.execute("DELETE FROM duplicate_groups WHERE group_id=?1", [group_id])
			.map_err(|error| error.to_string())?;
	}
	for key in keys {
		tx.execute("INSERT OR IGNORE INTO duplicate_keys(duplicate_key,group_id) VALUES(?1,?2)", params![key, target])
			.map_err(|error| error.to_string())?;
	}
	Ok(target)
}

fn duplicate_keys(post: &PersistedPost) -> Vec<Vec<u8>> {
	let mut keys = BTreeSet::new();
	if !post.media.content_fingerprint.trim().is_empty() {
		keys.insert(format!("content:{}", post.media.content_fingerprint.trim().to_lowercase()));
	}
	for (prefix, value) in [
		("media", post.media.url.as_str()),
		("preview", post.media.preview_url.as_str()),
		("thumb", post.thumbnail.url.as_str()),
		("embed", post.media.embed_code.as_str()),
	] {
		let normalized = value.trim().to_lowercase();
		if !normalized.is_empty() {
			keys.insert(format!("{prefix}:{normalized}"));
		}
	}
	if keys.is_empty() {
		keys.insert(format!("id:{}", post.id));
	}
	keys.into_iter().map(|key| sha256(key.as_bytes())).collect()
}

fn profile_from_source(cache_key: &str, display_name: &str) -> Option<String> {
	cache_key
		.split(':')
		.chain(display_name.split(':'))
		.find_map(|part| part.trim().to_lowercase().strip_prefix("u_").map(str::to_string))
		.filter(|value| !value.is_empty())
}

fn materialize(conn: &mut Connection) -> Result<(), String> {
	let tx = conn.transaction().map_err(|error| error.to_string())?;
	tx.execute_batch(
		r#"
UPDATE items
SET card_key = CASE
  WHEN kind=1 THEN COALESCE(
    (SELECT 'd:' || linked.group_id FROM items AS linked WHERE linked.item_id=items.submission_id),
    't:' || submission_id
  )
  ELSE 'd:' || group_id
END;

DELETE FROM thread_members;
DELETE FROM thread_groups;
INSERT INTO thread_groups(submission_id,representative_id,created_sort,activity_sort,discovery_sort,member_count)
SELECT submission_id,
       COALESCE(MAX(CASE WHEN kind=3 THEN item_id END),MIN(item_id)),
       MAX(CASE WHEN kind=3 THEN created_utc ELSE link_created_utc END),
       MAX(created_utc),MAX(discovery_seq),COUNT(*)
FROM items GROUP BY submission_id;
INSERT INTO thread_members(submission_id,item_id)
SELECT submission_id,item_id FROM items;

DELETE FROM feed_entries;
INSERT INTO feed_entries(feed_id,card_key,representative_id,created_sort,activity_sort,discovery_sort,duration_seconds,
                         random_epoch,random_round,random_rank,member_count)
SELECT 1,i.card_key,
       COALESCE(MAX(CASE WHEN i.kind=3 THEN i.item_id END),MIN(i.item_id)),
       MAX(CASE WHEN i.kind=3 THEN i.created_utc ELSE i.link_created_utc END),
       MAX(i.created_utc),MAX(i.discovery_seq),MAX(i.duration_seconds),1,MIN(sm.source_position),
       ABS((COALESCE(MIN(i.group_id),0)*1103515245 + MAX(i.created_utc)*12345) % 9223372036854775807),
       COUNT(DISTINCT i.item_id)
FROM items i JOIN source_memberships sm ON sm.item_id=i.item_id
GROUP BY i.card_key;

DELETE FROM profile_feed_entries;
INSERT INTO profile_feed_entries(profile_id,card_key,representative_id,created_sort,activity_sort,discovery_sort,duration_seconds)
SELECT sp.profile_id,i.card_key,
       COALESCE(MAX(CASE WHEN i.kind=3 THEN i.item_id END),MIN(i.item_id)),
       MAX(CASE WHEN i.kind=3 THEN i.created_utc ELSE i.link_created_utc END),
       MAX(i.created_utc),MAX(i.discovery_seq),MAX(i.duration_seconds)
FROM source_profiles sp
JOIN source_memberships sm ON sm.source_id=sp.source_id
JOIN items i ON i.item_id=sm.item_id
GROUP BY sp.profile_id,i.card_key;
"#,
	)
	.map_err(|error| error.to_string())?;
	tx.commit().map_err(|error| error.to_string())
}

fn counts(conn: &Connection) -> Result<[i64; 3], String> {
	let item_count = conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	let feed_count = conn
		.query_row("SELECT COUNT(*) FROM feed_entries", [], |row| row.get(0))
		.map_err(|error| error.to_string())?;
	let profile_count = conn.query_row("SELECT COUNT(*) FROM profiles", [], |row| row.get(0)).map_err(|error| error.to_string())?;
	Ok([item_count, feed_count, profile_count])
}

fn bench_queries(conn: &Connection, args: &Args) -> Result<(), String> {
	for (name, column, extra) in [
		("new", "created_sort", ""),
		("activity", "activity_sort", ""),
		("discovered", "discovery_sort", "AND discovery_sort>0"),
	] {
		for page in [1usize, 20, 100, 1000] {
			let offset = page.saturating_sub(1).saturating_mul(args.page_size);
			let cursor_sql = format!("SELECT {column},card_key FROM feed_entries WHERE feed_id=1 {extra} ORDER BY {column} DESC,card_key DESC LIMIT 1 OFFSET ?1");
			let cursor = conn
				.query_row(&cursor_sql, [offset as i64], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
				.optional()
				.map_err(|error| error.to_string())?;
			let Some((value, key)) = cursor else {
				continue;
			};
			let sql = format!("SELECT representative_id FROM feed_entries WHERE feed_id=1 {extra} AND ({column},card_key)<(?1,?2) ORDER BY {column} DESC,card_key DESC LIMIT ?3");
			let timings = time_query(args.iterations, || {
				let mut statement = conn.prepare_cached(&sql).map_err(|error| error.to_string())?;
				let rows = statement
					.query_map(params![value, key, args.page_size as i64], |row| row.get::<_, String>(0))
					.map_err(|error| error.to_string())?;
				let ids = rows.collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?;
				black_box(ids);
				Ok(())
			})?;
			print_timing(&format!("QUERY mode={name} page={page}"), &timings);
		}
	}
	for page in [1usize, 20, 100, 1000] {
		let offset = page.saturating_sub(1).saturating_mul(args.page_size);
		let cursor = conn
			.query_row(
				"SELECT duration_seconds,created_sort,card_key FROM feed_entries WHERE feed_id=1 AND duration_seconds>0
				 ORDER BY duration_seconds DESC,created_sort DESC,card_key DESC LIMIT 1 OFFSET ?1",
				[offset as i64],
				|row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)),
			)
			.optional()
			.map_err(|error| error.to_string())?;
		let Some((duration, created, key)) = cursor else {
			continue;
		};
		let timings = time_query(args.iterations, || {
			let mut statement = conn
				.prepare_cached(
					"SELECT representative_id FROM feed_entries WHERE feed_id=1 AND duration_seconds>0
					 AND (duration_seconds,created_sort,card_key)<(?1,?2,?3)
					 ORDER BY duration_seconds DESC,created_sort DESC,card_key DESC LIMIT ?4",
				)
				.map_err(|error| error.to_string())?;
			let rows = statement
				.query_map(params![duration, created, key, args.page_size as i64], |row| row.get::<_, String>(0))
				.map_err(|error| error.to_string())?;
			black_box(rows.collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?);
			Ok(())
		})?;
		print_timing(&format!("QUERY mode=videos page={page}"), &timings);
	}

	let random_cursor = conn
		.query_row(
			"SELECT random_round,random_rank,card_key FROM feed_entries WHERE feed_id=1 AND random_epoch=1 ORDER BY random_round,random_rank,card_key LIMIT 1 OFFSET ?1",
			[(args.page_size * 99) as i64],
			|row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)),
		)
		.optional()
		.map_err(|error| error.to_string())?;
	if let Some((round, rank, key)) = random_cursor {
		let timings = time_query(args.iterations, || {
			let mut statement = conn
				.prepare_cached(
					"SELECT representative_id FROM feed_entries WHERE feed_id=1 AND random_epoch=1 AND (random_round,random_rank,card_key)>(?1,?2,?3)
					 ORDER BY random_round,random_rank,card_key LIMIT ?4",
				)
				.map_err(|error| error.to_string())?;
			let rows = statement
				.query_map(params![round, rank, key, args.page_size as i64], |row| row.get::<_, String>(0))
				.map_err(|error| error.to_string())?;
			black_box(rows.collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?);
			Ok(())
		})?;
		print_timing("QUERY mode=random page=100", &timings);
	}

	let profile_id = conn
		.query_row(
			"SELECT profile_id FROM profile_feed_entries GROUP BY profile_id ORDER BY COUNT(*) DESC LIMIT 1",
			[],
			|row| row.get::<_, i64>(0),
		)
		.optional()
		.map_err(|error| error.to_string())?;
	if let Some(profile_id) = profile_id {
		let timings = time_query(args.iterations, || {
			let mut statement = conn
				.prepare_cached("SELECT representative_id FROM profile_feed_entries WHERE profile_id=?1 ORDER BY created_sort DESC,card_key DESC LIMIT ?2")
				.map_err(|error| error.to_string())?;
			let rows = statement
				.query_map(params![profile_id, args.page_size as i64], |row| row.get::<_, String>(0))
				.map_err(|error| error.to_string())?;
			black_box(rows.collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?);
			Ok(())
		})?;
		print_timing("QUERY mode=submitted page=1", &timings);
	}

	conn
		.execute_batch("DROP TABLE IF EXISTS temp.selected_sources; CREATE TEMP TABLE selected_sources(source_id INTEGER PRIMARY KEY);")
		.map_err(|error| error.to_string())?;
	conn
		.execute(
			"INSERT INTO selected_sources SELECT source_id FROM sources ORDER BY source_id LIMIT ?1",
			[args.selected_sources as i64],
		)
		.map_err(|error| error.to_string())?;
	let timings = time_query(args.iterations.min(50), || {
		let mut statement = conn
			.prepare_cached(
				"SELECT MAX(i.created_utc),MIN(i.item_id) FROM selected_sources ss JOIN source_memberships sm ON sm.source_id=ss.source_id
				 JOIN items i ON i.item_id=sm.item_id GROUP BY i.card_key ORDER BY MAX(i.created_utc) DESC,i.card_key DESC LIMIT ?1",
			)
			.map_err(|error| error.to_string())?;
		let rows = statement
			.query_map([args.page_size as i64], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
			.map_err(|error| error.to_string())?;
		black_box(rows.collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?);
		Ok(())
	})?;
	print_timing("QUERY mode=normalized_direct selected_sources", &timings);
	Ok(())
}

fn bench_hydration(conn: &Connection, page_size: usize, iterations: usize, compression: &str) -> Result<(), String> {
	let ids = conn
		.prepare(
			"WITH page AS (
			 SELECT representative_id,created_sort FROM feed_entries WHERE feed_id=1 ORDER BY created_sort DESC,card_key DESC LIMIT ?1
			) SELECT DISTINCT tm.item_id FROM page JOIN items rep ON rep.item_id=page.representative_id
			 JOIN thread_members tm ON tm.submission_id=rep.submission_id ORDER BY page.created_sort DESC",
		)
		.map_err(|error| error.to_string())?
		.query_map([page_size as i64], |row| row.get::<_, String>(0))
		.map_err(|error| error.to_string())?
		.collect::<Result<Vec<_>, _>>()
		.map_err(|error| error.to_string())?;
	if ids.is_empty() {
		return Ok(());
	}
	let timings = time_query(iterations, || {
		let mut bytes = 0usize;
		let mut decoded = 0usize;
		let mut statement = conn
			.prepare_cached("SELECT payload FROM item_payloads WHERE item_id=?1")
			.map_err(|error| error.to_string())?;
		for id in &ids {
			let stored_payload: Vec<u8> = statement.query_row([id], |row| row.get(0)).map_err(|error| error.to_string())?;
			bytes += stored_payload.len();
			let payload = match compression {
				"zstd" => zstd::stream::decode_all(stored_payload.as_slice()).map_err(|error| error.to_string())?,
				_ => stored_payload,
			};
			let post: PersistedPost = bincode::deserialize(&payload).map_err(|error| error.to_string())?;
			decoded += usize::from(!post.id.is_empty());
		}
		black_box((bytes, decoded));
		Ok(())
	})?;
	print_timing(&format!("HYDRATE items={}", ids.len()), &timings);
	Ok(())
}

fn bench_refresh(conn: &mut Connection, iterations: usize) -> Result<(), String> {
	let source_id = conn
		.query_row("SELECT source_id FROM source_memberships GROUP BY source_id ORDER BY COUNT(*) DESC LIMIT 1", [], |row| {
			row.get::<_, i64>(0)
		})
		.map_err(|error| error.to_string())?;
	let mut timings = Vec::with_capacity(iterations);
	for generation in 0..iterations {
		let started = Instant::now();
		let tx = conn.transaction().map_err(|error| error.to_string())?;
		tx.execute("UPDATE sources SET generation=generation+1 WHERE source_id=?1", [source_id])
			.map_err(|error| error.to_string())?;
		tx.execute("UPDATE source_memberships SET generation=?2 WHERE source_id=?1", params![source_id, generation as i64 + 2])
			.map_err(|error| error.to_string())?;
		tx.commit().map_err(|error| error.to_string())?;
		timings.push(started.elapsed());
	}
	print_timing("REFRESH bounded_source_transaction", &timings);
	Ok(())
}

fn bench_concurrency(db_path: &Path, iterations: usize) -> Result<(), String> {
	let samples = Arc::new(Mutex::new(Vec::<Duration>::new()));
	let errors = Arc::new(Mutex::new(0usize));
	let mut handles = Vec::new();
	for _ in 0..4 {
		let path = db_path.to_path_buf();
		let samples = Arc::clone(&samples);
		let errors = Arc::clone(&errors);
		handles.push(thread::spawn(move || {
			let conn = match Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
				Ok(conn) => conn,
				Err(_) => {
					*errors.lock().unwrap() += iterations;
					return;
				}
			};
			let _ = configure(&conn);
			for _ in 0..iterations {
				let started = Instant::now();
				let result = conn.query_row(
					"SELECT COUNT(*) FROM (SELECT card_key FROM feed_entries WHERE feed_id=1 ORDER BY activity_sort DESC,card_key DESC LIMIT 25)",
					[],
					|row| row.get::<_, i64>(0),
				);
				if result.is_err() {
					*errors.lock().unwrap() += 1;
				} else {
					samples.lock().unwrap().push(started.elapsed());
				}
			}
		}));
	}
	let mut writer = Connection::open(db_path).map_err(|error| error.to_string())?;
	configure(&writer)?;
	for value in 0..iterations.min(50) {
		let tx = writer.transaction().map_err(|error| error.to_string())?;
		tx.execute(
			"INSERT INTO cache_meta(key,value) VALUES('concurrency',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
			[value as i64],
		)
		.map_err(|error| error.to_string())?;
		tx.commit().map_err(|error| error.to_string())?;
	}
	for handle in handles {
		handle.join().map_err(|_| "reader thread panicked".to_string())?;
	}
	let samples = samples.lock().unwrap();
	print_timing(&format!("CONCURRENCY readers=4 errors={}", *errors.lock().unwrap()), &samples);
	Ok(())
}

fn bench_crash_recovery(db_path: &Path) -> Result<(), String> {
	let executable = std::env::current_exe().map_err(|error| error.to_string())?;
	let status = Command::new(&executable)
		.arg("--crash-child")
		.arg("uncommitted")
		.arg(db_path)
		.status()
		.map_err(|error| error.to_string())?;
	if status.code() != Some(86) {
		return Err(format!("uncommitted crash child exited {status}"));
	}
	let conn = Connection::open(db_path).map_err(|error| error.to_string())?;
	configure(&conn)?;
	let uncommitted: Option<Vec<u8>> = conn
		.query_row("SELECT value FROM cache_meta WHERE key='crash-uncommitted'", [], |row| row.get(0))
		.optional()
		.map_err(|error| error.to_string())?;
	if uncommitted.is_some() {
		return Err("uncommitted crash row survived".into());
	}
	drop(conn);
	let status = Command::new(&executable)
		.arg("--crash-child")
		.arg("committed")
		.arg(db_path)
		.status()
		.map_err(|error| error.to_string())?;
	if status.code() != Some(87) {
		return Err(format!("committed crash child exited {status}"));
	}
	let conn = Connection::open(db_path).map_err(|error| error.to_string())?;
	configure(&conn)?;
	let committed: Option<Vec<u8>> = conn
		.query_row("SELECT value FROM cache_meta WHERE key='crash-committed'", [], |row| row.get(0))
		.optional()
		.map_err(|error| error.to_string())?;
	if committed.is_none() {
		return Err("committed crash row was lost".into());
	}
	conn.execute("DELETE FROM cache_meta WHERE key='crash-committed'", []).map_err(|error| error.to_string())?;
	println!("CRASH_RECOVERY uncommitted_rolled_back=true committed_durable=true");
	Ok(())
}

fn crash_child(raw: &[String]) -> Result<(), String> {
	let phase = raw.get(1).ok_or("missing crash phase")?;
	let db_path = raw.get(2).ok_or("missing crash DB")?;
	let mut conn = Connection::open(db_path).map_err(|error| error.to_string())?;
	configure(&conn)?;
	let tx = conn.transaction().map_err(|error| error.to_string())?;
	match phase.as_str() {
		"uncommitted" => {
			tx.execute("INSERT OR REPLACE INTO cache_meta(key,value) VALUES('crash-uncommitted',X'01')", [])
				.map_err(|error| error.to_string())?;
			std::process::exit(86);
		}
		"committed" => {
			tx.execute("INSERT OR REPLACE INTO cache_meta(key,value) VALUES('crash-committed',X'01')", [])
				.map_err(|error| error.to_string())?;
			tx.commit().map_err(|error| error.to_string())?;
			std::process::exit(87);
		}
		_ => Err("invalid crash phase".into()),
	}
}

fn time_query(mut iterations: usize, mut operation: impl FnMut() -> Result<(), String>) -> Result<Vec<Duration>, String> {
	iterations = iterations.max(1);
	operation()?;
	let mut timings = Vec::with_capacity(iterations);
	for _ in 0..iterations {
		let started = Instant::now();
		operation()?;
		timings.push(started.elapsed());
	}
	Ok(timings)
}

fn print_timing(label: &str, timings: &[Duration]) {
	if timings.is_empty() {
		println!("{label} samples=0");
		return;
	}
	let mut micros = timings.iter().map(Duration::as_micros).collect::<Vec<_>>();
	micros.sort_unstable();
	let percentile = |percent: usize| micros[(micros.len().saturating_sub(1) * percent) / 100];
	println!(
		"{label} samples={} p50_us={} p95_us={} p99_us={} max_us={}",
		micros.len(),
		percentile(50),
		percentile(95),
		percentile(99),
		micros[micros.len() - 1]
	);
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
	Sha256::digest(bytes).to_vec()
}

fn current_rss_bytes() -> i64 {
	let output = Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output();
	output
		.ok()
		.and_then(|output| String::from_utf8(output.stdout).ok())
		.and_then(|value| value.trim().parse::<i64>().ok())
		.map(|kibibytes| kibibytes.saturating_mul(1024))
		.unwrap_or(-1)
}
