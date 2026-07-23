# Canonical SQLite cache architecture

Status: reviewed architecture and benchmark checkpoint. This is not deployment authorization.

## Decision

Use SQLite as Redlib's canonical post and comment cache:

- one ordinary `ROWID` object row per Reddit fullname, including its bincode payload;
- normalized source membership, discovery, duplicate and thread relationships;
- versioned materialized entries only for active subscription profiles;
- direct indexes for user Submitted and other object-local views;
- no global materialized card table.

The global-card alternative is rejected. On the arm64 synthetic benchmark it added about 8.8 MiB, slowed writes, still required profile-membership probes, and could select a duplicate representative that was ineligible for the requesting profile. Request-time comment grouping still cost 31-42 ms. Active-profile entries produced covering index seeks and moved grouping to bounded refresh transactions.

Ordinary Home `Hot` remains behaviorally separate for the first migration stage. The SQLite cutover must preserve its source diversity and initial-page latency.

## Invariants

1. `objects.fullname` is globally unique and each bincode payload is stored once.
2. Object identity and `discovery_seq` are immutable. Payload and derived metadata may be refreshed.
3. A post first observed interactively is never promoted by a later daemon refresh.
4. Every legacy object imported during deployment receives the same discovery baseline and explicit migration provenance.
5. Source generation, membership changes, discovery allocation, relationship updates and affected profile entries commit atomically.
6. Profile membership uses exact reference counts. Removing one source cannot remove an object still supplied by another eligible source.
7. Duplicate representatives are chosen within the profile's eligible objects, never globally.
8. Comments from several followed users bundle through one stable submission/thread identity, even when the linked `t3` payload is absent.
9. Page reads are indexed keyset scans. Deep pages do not rescan all sources or all previous rows.
10. Active browsers may pin bounded page references but never disable global eviction.
11. Legacy `sources.v2` blobs remain untouched until parity, soak, rollback and retention gates pass.

## Authoritative schema shape

The SQL below is structural rather than a frozen migration number. Integer `object_pk` values keep relationship tables and indexes compact. `objects` deliberately remains an ordinary rowid table; do not make the payload object table `WITHOUT ROWID`.

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
PRAGMA temp_store = FILE;
PRAGMA cache_size = -8192;   -- 8 MiB per writer; readers are smaller
PRAGMA mmap_size = 0;

CREATE TABLE cache_meta (
    key                     TEXT PRIMARY KEY,
    value                   BLOB NOT NULL
);

CREATE TABLE objects (
    object_pk               INTEGER PRIMARY KEY,
    fullname                TEXT NOT NULL UNIQUE,
    kind                    INTEGER NOT NULL,       -- 1 comment, 3 submission
    created_utc             INTEGER NOT NULL,
    link_created_utc        INTEGER NOT NULL,
    author_lc               TEXT NOT NULL,
    community_lc            TEXT NOT NULL,
    submission_fullname     TEXT,
    parent_fullname         TEXT,
    post_type               TEXT NOT NULL,
    duration_seconds        INTEGER NOT NULL,
    is_video                INTEGER NOT NULL CHECK (is_video IN (0, 1)),
    is_deleted              INTEGER NOT NULL CHECK (is_deleted IN (0, 1)),
    is_promotional          INTEGER NOT NULL CHECK (is_promotional IN (0, 1)),
    first_seen_origin       INTEGER NOT NULL,
    first_seen_at           INTEGER NOT NULL,
    discovery_seq           INTEGER NOT NULL,
    discovered_at           INTEGER NOT NULL,
    last_seen_at            INTEGER NOT NULL,
    payload_revision        INTEGER NOT NULL,
    payload_codec           INTEGER NOT NULL,       -- versioned raw/zstd encoding
    payload_hash            BLOB NOT NULL,
    payload                 BLOB NOT NULL
);

CREATE INDEX objects_new
    ON objects(created_utc DESC, object_pk DESC);
CREATE INDEX objects_submitted
    ON objects(author_lc, kind, created_utc DESC, object_pk DESC);
CREATE INDEX objects_videos
    ON objects(duration_seconds DESC, created_utc DESC, object_pk DESC)
    WHERE duration_seconds > 0;
CREATE INDEX objects_submission
    ON objects(submission_fullname, created_utc DESC, object_pk DESC);
CREATE INDEX objects_discovery
    ON objects(discovery_seq DESC, object_pk DESC)
    WHERE discovery_seq > 0;

CREATE TABLE sources (
    source_pk               INTEGER PRIMARY KEY,
    cache_key               TEXT NOT NULL UNIQUE,
    source_kind             INTEGER NOT NULL,
    display_name            TEXT NOT NULL,
    sort                    INTEGER NOT NULL,
    generation              INTEGER NOT NULL,
    fetched_at              INTEGER NOT NULL,
    refresh_origin          INTEGER,
    after_cursor            TEXT NOT NULL,
    content_hash            BLOB NOT NULL
);

CREATE TABLE source_memberships (
    source_pk               INTEGER NOT NULL REFERENCES sources(source_pk) ON DELETE CASCADE,
    object_pk               INTEGER NOT NULL REFERENCES objects(object_pk) ON DELETE CASCADE,
    source_position         INTEGER NOT NULL,
    generation              INTEGER NOT NULL,
    PRIMARY KEY (source_pk, object_pk)
) WITHOUT ROWID;

CREATE INDEX source_memberships_object
    ON source_memberships(object_pk, source_pk);

CREATE TABLE duplicate_groups (
    duplicate_group_pk      INTEGER PRIMARY KEY
);

CREATE TABLE duplicate_keys (
	duplicate_key_hash      BLOB PRIMARY KEY,       -- SHA-256 of the normalized key
	duplicate_group_pk      INTEGER NOT NULL REFERENCES duplicate_groups(duplicate_group_pk)
) WITHOUT ROWID;

CREATE TABLE duplicate_members (
    duplicate_group_pk      INTEGER NOT NULL REFERENCES duplicate_groups(duplicate_group_pk) ON DELETE CASCADE,
    object_pk               INTEGER NOT NULL UNIQUE REFERENCES objects(object_pk) ON DELETE CASCADE,
    PRIMARY KEY (duplicate_group_pk, object_pk)
) WITHOUT ROWID;

CREATE TABLE thread_groups (
    thread_group_pk         INTEGER PRIMARY KEY,
    submission_fullname     TEXT NOT NULL UNIQUE,
    linked_submission_pk    INTEGER REFERENCES objects(object_pk)
);

CREATE TABLE thread_members (
    thread_group_pk         INTEGER NOT NULL REFERENCES thread_groups(thread_group_pk) ON DELETE CASCADE,
    object_pk               INTEGER NOT NULL UNIQUE REFERENCES objects(object_pk) ON DELETE CASCADE,
    PRIMARY KEY (thread_group_pk, object_pk)
) WITHOUT ROWID;

CREATE TABLE active_profiles (
    profile_pk              INTEGER PRIMARY KEY,
    fingerprint             BLOB NOT NULL UNIQUE,
    generation              INTEGER NOT NULL,
    random_epoch            INTEGER NOT NULL,
    last_accessed_at        INTEGER NOT NULL
);

CREATE TABLE profile_sources (
    profile_pk              INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    source_pk               INTEGER NOT NULL REFERENCES sources(source_pk) ON DELETE CASCADE,
    source_role             INTEGER NOT NULL,
    PRIMARY KEY (profile_pk, source_pk)
) WITHOUT ROWID;

CREATE INDEX profile_sources_by_source
    ON profile_sources(source_pk, profile_pk);

CREATE TABLE profile_object_refs (
    profile_pk              INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    object_pk               INTEGER NOT NULL REFERENCES objects(object_pk) ON DELETE CASCADE,
    source_refcount         INTEGER NOT NULL CHECK (source_refcount > 0),
    PRIMARY KEY (profile_pk, object_pk)
) WITHOUT ROWID;

CREATE TABLE profile_thread_entries (
    profile_pk              INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    thread_group_pk         INTEGER NOT NULL REFERENCES thread_groups(thread_group_pk),
    version                 INTEGER NOT NULL,
    valid_from_generation   INTEGER NOT NULL,
    valid_to_generation     INTEGER,
    representative_pk       INTEGER NOT NULL REFERENCES objects(object_pk),
    created_sort            INTEGER NOT NULL,
    activity_sort           INTEGER NOT NULL,
    discovery_sort          INTEGER NOT NULL,
    member_count            INTEGER NOT NULL,
    PRIMARY KEY (profile_pk, thread_group_pk, version)
);

CREATE TABLE profile_feed_entries (
    profile_pk              INTEGER NOT NULL REFERENCES active_profiles(profile_pk) ON DELETE CASCADE,
    view_kind               INTEGER NOT NULL,       -- Home or Following
    card_key                TEXT NOT NULL,
    version                 INTEGER NOT NULL,
    valid_from_generation   INTEGER NOT NULL,
    valid_to_generation     INTEGER,
    representative_pk       INTEGER NOT NULL REFERENCES objects(object_pk),
    duplicate_group_pk      INTEGER REFERENCES duplicate_groups(duplicate_group_pk),
    thread_group_pk         INTEGER REFERENCES thread_groups(thread_group_pk),
    created_sort            INTEGER NOT NULL,
    activity_sort           INTEGER NOT NULL,
    discovery_sort          INTEGER NOT NULL,
    duration_seconds        INTEGER NOT NULL,
    community_lc            TEXT NOT NULL,
    author_lc               TEXT NOT NULL,
    random_epoch            INTEGER NOT NULL,
    random_round            INTEGER NOT NULL,
    random_rank             INTEGER NOT NULL,
    member_count            INTEGER NOT NULL,
    PRIMARY KEY (profile_pk, view_kind, card_key, version)
);

CREATE INDEX profile_feed_new
    ON profile_feed_entries(profile_pk, view_kind, created_sort DESC, card_key DESC)
    WHERE valid_to_generation IS NULL;
CREATE INDEX profile_feed_activity
    ON profile_feed_entries(profile_pk, view_kind, activity_sort DESC, card_key DESC)
    WHERE valid_to_generation IS NULL;
CREATE INDEX profile_feed_discovered
    ON profile_feed_entries(profile_pk, view_kind, discovery_sort DESC, card_key DESC)
    WHERE valid_to_generation IS NULL AND discovery_sort > 0;
CREATE INDEX profile_feed_random
    ON profile_feed_entries(profile_pk, view_kind, random_epoch, random_round, random_rank, card_key)
    WHERE valid_to_generation IS NULL;
CREATE INDEX profile_feed_videos
    ON profile_feed_entries(profile_pk, view_kind, duration_seconds DESC, created_sort DESC, card_key DESC)
    WHERE valid_to_generation IS NULL AND duration_seconds > 0;

CREATE TABLE migration_sources (
    cache_key               TEXT PRIMARY KEY,
    source_size             INTEGER NOT NULL,
    source_mtime_ns         INTEGER NOT NULL,
    source_hash             BLOB NOT NULL,
    imported_generation     INTEGER NOT NULL,
    imported_at             INTEGER NOT NULL,
    object_count            INTEGER NOT NULL,
    status                  INTEGER NOT NULL
);
```

## Discovery semantics

`discovery_seq` is allocated in the same transaction that first creates the canonical object:

- daemon first observation: next positive monotonic sequence;
- interactive first observation: `0`, permanently;
- migration: one common positive baseline sequence and timestamp for every pre-existing object, with migration provenance in `cache_meta`.

An upsert never overwrites `first_seen_origin`, `first_seen_at`, `discovery_seq` or `discovered_at`.

Following `Discovered` orders a materialized card by the maximum immutable discovery sequence among its eligible submission and comment members.

## Stable duplicate and thread groups

Duplicate keys include the same normalized media URLs, gallery tuples, embed URLs and content fingerprints used by current rendering. If new keys connect several groups, retain the smallest group ID, move their keys and members in one transaction, and close/rebuild only affected profile cards. Group IDs merge but do not churn on normal payload refresh.

Every comment maps to a stable `thread_groups.submission_fullname`. The linked submission pointer is nullable, so comments can materialize a synthetic card until a real `t3` becomes available. Each profile thread entry includes every eligible cached comment across followed users. There is no request-time scan and no fixed overfetch factor.

## Bounded source refresh

The daemon remains the single writer. Readers use a small fixed pool of read-only WAL connections.

Before `BEGIN IMMEDIATE`, parse and serialize the source response and derive duplicate keys. Inside the transaction:

1. Reject stale source generations.
2. Upsert canonical objects while preserving immutable first-observation columns.
3. Stage old and new memberships, then derive added and removed object IDs.
4. Update exact `source_memberships`.
5. Expand changed IDs into temporary affected object, duplicate-group and thread-group sets.
6. Find affected active profiles through `profile_sources`.
7. Apply set-based `profile_object_refs` increments/decrements.
8. For only the affected cards, close current materialized versions and insert replacements with profile-local representatives.
9. Update source and profile generations and commit.

Do not rebuild a complete profile and do not loop over cards with one SQL statement per card. The arm64 upper-bound full rebuild was roughly 300 ms, versus about 0.8 ms p50 for set-based affected-source refresh.

## Preference changes

A subscription toggle uses the same delta machinery:

1. stage added and removed source IDs;
2. derive affected objects/groups/threads from those sources;
3. update `profile_sources` and refcounts set-wise;
4. close/insert only changed profile entry versions;
5. increment the profile generation.

Creating a wholly new profile performs a bounded initial build in a blocking
worker. Requests continue on the legacy feed until that profile generation
commits; later requests use the indexed timeline. Existing SQLite profile
requests continue using the prior generation until an updated generation
commits.

## Indexed reads

- Home New: active profile entry index by `(created_sort, card_key)`.
- Following New: active profile entry index by `(activity_sort, card_key)`.
- Following Discovered: `(discovery_sort, card_key)`.
- Random: precomputed `(random_epoch, random_round, random_rank, card_key)`.
- Videos: `(duration_seconds, created_sort, card_key)`.
- User Submitted: direct `objects(author_lc, kind, created_utc, object_pk)` scan.

All cursors include the profile fingerprint/generation and complete ordering tuple. New objects above a cursor do not shift later pages. Back pages retain bounded object/card references, not full `Post` clones. Old materialized versions are garbage-collected only after no live session can reference their generation.

`random_rank` is a stable keyed hash of `(profile fingerprint, epoch, card key)`. `random_round` is the lowest eligible within-source ordinal so each source contributes before repeats. Build the next epoch in bounded background transactions and activate it atomically; never call SQLite `random()` over the feed.

## Migration and rollback

1. Create the database beside `sources.v2`.
2. Record one deployment baseline sequence/timestamp before importing.
3. Import a bounded batch of source blobs per idempotent transaction and checkpoint each source's size, mtime and hash.
4. Resume after interruption; re-import only changed fingerprints.
5. Keep daemon refresh disabled until catch-up, or enable legacy-first dual writes before the daemon starts.
6. Verify source counts, memberships, unique fullnames, sampled payload hashes, relationships, semantic page IDs and `PRAGMA quick_check`.
7. Cut reads over behind a rollback flag. Keep legacy-first dual writes and retain every blob through a validated soak.
8. Retire blobs only in a later explicitly approved release.

Writing the legacy blob before the SQLite transaction keeps the rollback cache at least as new as the database. A failed SQLite transaction is reconciled from the blob at restart. Request state changes only after SQLite commit.

RS2 migrates its own sparse cache independently; this design does not copy RS1's personalized cache into it.

## Memory policy

Initial hard limits:

- one fixed SQLite connection with an 8 MiB page cache, serialized behind the
  shadow-store mutex;
- `mmap_size = 0` until a bounded mapping proves useful;
- at most four materialized active profiles by default;
- at most 64 timeline cursor sessions, 2,048 page cursors per session and 16
  frozen full-post pages per feed session;
- file-backed temp store and migration batches capped at 64 sources;
- no full-post materialized catalog, Following clone or browser-heartbeat eviction exemption.

Expose page-cache, render-cache, frozen-page, connection, WAL and eviction metrics. RSS and PSS must be measured in the real service because benchmark fixture construction can dominate process memory.

The real bincode lane must also compare a 16 KiB database page size. Current
items average slightly above 2 KiB; a 4 KiB SQLite page can push most of each
payload to a dedicated overflow page and almost double payload-table storage.
Duplicate keys must be stored as fixed-size hashes so long embed HTML and URLs
do not become a second large text corpus in the indexes.

The real-cache comparator also showed that zstd level 1 can materially shrink
bincode payload storage with a small hydration penalty, but compression remains
a measured option rather than an architectural assumption. The corrected ROWID
importer must compare raw and compressed payloads end to end; `payload_codec`
keeps the on-disk encoding explicit and migratable.

## Evidence so far

### RS2 arm64 synthetic benchmark

Fixture: 1,400 sources x 50 memberships, 64,400 canonical objects, 1.7 KiB payloads, duplicates, comments and four active profiles. Python 3.13.5 / SQLite 3.46.1, WAL, `synchronous=NORMAL`, 8 MiB cache, mmap disabled.

- normalized/global/profile DB: 139.02 / 147.79 / 149.08 MiB;
- import: 1.76 / 2.32 / 2.49 s;
- active-profile warm p50 New/Random/Video/Submitted/Comments: 0.057 / 0.051 / 0.063 / 0.026 / 0.092 ms;
- cold-ish: 0.139 / 0.195 / 0.227 / 0.123 / 0.265 ms;
- 400-page New keyset p50/p95/p99: 0.111 / 0.125 / 0.151 ms;
- affected-source refresh p50/p95/p99: 0.812 / 1.450 / 1.725 ms;
- concurrent read p50/p95/p99: 0.099 / 0.240 / 0.377 ms;
- concurrent write p50/p95/p99: 1.556 / 2.329 / 2.424 ms, no errors;
- WAL peak about 4.08 MiB;
- killed uncommitted transaction recovered with `quick_check=ok` and no leaked rows.

The cold-ish run reset SQLite caches but did not force cold device I/O. RSS/PSS included about 104 MiB of in-process synthetic fixture data; the useful profile-materialization increment was about 8-9 MiB.

### Required real-cache gates

- Import the current 119 MiB bincode snapshot without modifying source blobs.
- Compare `ROWID`/8 KiB and 4 KiB page layouts, plus `FULL` versus `NORMAL` durability cost.
- Exercise real payload hydration and bincode conversion.
- Validate comments on one submission, multi-source duplicates, missing linked submissions, filters, deleted/promotional rows and stale generations.
- Measure pages 1/20/100/1000, refresh write amplification, WAL, concurrent readers, RSS/PSS and allocator heap.
- Fault-inject migration and refresh phases.
- Prove ordinary Home Hot latency and source diversity do not regress.
- Pass personalized Following New/Discovered semantic parity before shared-source implementation approval.
