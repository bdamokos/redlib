# Plan: Materialized post index for fast “new” feeds (feature parity)

This document outlines how to replace **per-request full merges** of many per-subreddit caches with a **shared, daemon-maintained post catalog** and **indexed reads**, while keeping behavior aligned with today’s Redlib for **new** ordering, **duplicate handling**, **video-length view**, **user pages**, and related features.

**Out of scope here:** Reddit `hot` aggregate ordering (not a current goal).

---

## 1. Problem statement

Today, each subscribed subreddit is cached separately (`SOURCE_CACHE` in `post_cache.rs`). The home (and similar) views **rebuild a global timeline in memory** by merging every ready source, often **many times per HTTP request**. CPU and allocations scale with **subscription count × posts per source**, even when Reddit is not hit.

**Target:** Background ingestion updates a **shared store**; HTTP handlers **query** the first *N* posts (then the next *N*), optionally applying **session filters** (e.g. hide seen), with **no regression** in product behavior listed below.

---

## 2. Current integration points (parity anchors)

These are the behaviors that must remain correct when data comes from an index instead of a live merge.

| Area | Where it lives | What to preserve |
|------|----------------|------------------|
| **Per-source cache write path** | `store_source_posts_with_origin` / `publish_remembered_source_refresh` in `post_cache.rs` | On ingest: `update_user_video_stats_from_posts`, flair storage, persistence hooks, `video_preview::prewarm_listing_video_previews`, `spawn_author_link_prefetch` |
| **Author links + avg video length** | `utils::hydrate_post_author_links_from_cache`, `CachedUserMetadata` / `USER_METADATA_CACHE` | Render-time hydration from cached author metadata; stats used for labels/CSS classes |
| **Video / RedGIFs-oriented stats** | `user.rs`: `observe_user_video_post`, `video_aggregate_key`, `seen_redgifs_users`, `update_user_video_stats_from_posts` | Rolling stats per author (duration totals, distinct RedGIFs uploaders, dedupe keys) |
| **Cross-sub duplicate posts** | `user::collapse_duplicate_submissions`, `duplicate_submission_key` | Gallery/image/gif/RedGIFs keys merged into one row with `collapsed_duplicates` |
| **Seen posts** | `seen_posts` + `contextual_listing_posts_from_feed_page` in `subreddit.rs` | Stable paging over a merged stream; optional “fill with next unseen” using full merged list |
| **Video length view** | `video_mode::build_listing` | Filters to video/gif with `duration_seconds > 0`, age band, facet filters, id dedupe, then `collapse_duplicate_submissions`, sort by duration |
| **User pages** | `user.rs` routes, submitted/comments listings | Single-user or small fixed set of sources; may continue using existing fetches or query index by `author` / route-specific keys |
| **Persistence** | `cache_persistence.rs` (subreddit cache snapshot) | Decide whether the new index is persisted, rebuilt on boot, or hybrid |

---

## 3. Proposed architecture (high level)

### 3.1 Shared post catalog

- **Single logical table** (implementation: SQLite, `sled`, or in-process `HashMap` + secondary indexes—choose after sizing) keyed at minimum by:
  - `post_id` (Reddit id, primary key)
  - `subreddit` / `community`
  - `author` (normalized for lookups)
  - `created_ts` (for global “new” ordering)
  - Serialized **`Post`** payload (or normalized columns + blob) so rendering stays identical

- **Ingestion rule:** Whenever the daemon (or any path) finishes refreshing a **feed source**, **upsert** all posts from that response into the catalog. Subreddit-scoped rows are **replaced/updated** for that source’s window (same as today’s “~40 posts per source” semantics).

- **Deletion / eviction:** When a post drops out of a source’s top window, either remove it from the catalog or mark stale; **document** how this matches current “only cached window” behavior.

### 3.2 Per-user “home new” query (not one global timeline)

- Users differ by **subscription list** (and blocklist / flair filters from `FeedSessionRequest` / prefs).

- **Query pattern:** “Posts where `community` ∈ user’s subscriptions (and flair rules if modeled), ordered by `created_ts` DESC, **cursor** = `(created_ts, post_id)` for stable pagination.”

- **Indexes:** Composite index aligned with that query, e.g. `(created_ts DESC, post_id)` **plus** efficient membership in subscription set (see below).

- **Subscription membership:** Options: (a) store `subreddit` on each row and filter `WHERE subreddit IN (...)` for typical list sizes; (b) maintain **per-subreddit posting lists** and run a **bounded k-way merge** only over **subscribed** subs (much smaller than merging *all* daemon subs); (c) bitmap / set index if the store grows huge. Pick (a) or (b) first for simplicity and measure.

### 3.3 Duplicates

- **Parity with `collapse_duplicate_submissions`:** Either:
  - **At read time:** Fetch a **buffer** of candidates (e.g. 2–3× page size), run existing `collapse_duplicate_submissions` on that slice before cutting a page (simplest parity), or  
  - **At ingest time:** Compute `duplicate_submission_key` and merge into canonical rows (faster reads, more write complexity).

- **Tests:** Port existing `user.rs` tests for duplicate collapsing and RedGIFs-dedupe to run against the new path.

### 3.4 Seen posts and stable pages

- Keep **session cursor** (or frozen page ids) so “next page” does not reshuffle when the index updates mid-session.

- **`seen_posts`:** Continue to record seen ids from **what we actually showed**. For “hide seen, fill next unseen,” either:
  - Scan forward in the **deduped** new-stream until the page is full (may need larger candidate pulls), or  
  - Reuse a **merged id list** cached for the session revision (lighter CPU than full `Post` merge).

### 3.5 Video length view

- **Input:** Instead of `collect_cached_source_posts` + `build_listing`, pass either:
  - **Candidate posts** from the same catalog query restricted to `post_type` video/gif and `duration_seconds > 0`, plus the same filters as `VideoModeQuery`, or  
  - A **materialized subset** updated on ingest (only if profiling requires it).

- **Parity:** Reuse `video_mode::build_listing` on the candidate `Vec<Post>` so sorting, age bounds, facet options, id dedupe, and `collapse_duplicate_submissions` stay **one code path**.

### 3.6 User pages (submitted / comments / following)

- **Submitted:** Query catalog by `author == user` + sort by `created_ts` (with pagination). Ensure ingest includes user-submitted fetches if those posts are only in `/user/x/submitted.json` today.

- **Comments-as-posts** (`UserComments` / following): If stored differently in Reddit’s API, either **keep a separate small cache** for those feeds or **extend the catalog** with a `feed_kind` column so user timelines stay correct.

- **Stats:** `update_user_video_stats_from_posts` on **ingest** (as today on `store_source_posts`) keeps **global** author stats independent of which page read the posts.

### 3.7 Author links, RedGIFs, video previews

- **Ingest (daemon):** After upserting posts, keep calling:
  - `update_user_video_stats_from_posts` on the **batch** (or per post as today),
  - `spawn_author_link_prefetch` / flair writes / `video_preview::prewarm_listing_video_previews` where applicable.

- **Render:** Keep `hydrate_post_author_links_from_cache` on the page’s `Vec<Post>` so **avg video length**, RedGIFs uploader CSS, and external links behave as now.

---

## 4. Implementation phases

### Phase A — Catalog + ingestion only (no UI switch)

1. Define schema + upsert API (“apply refresh payload for `FeedSource`”).
2. On existing `publish_remembered_source_refresh` / `store_source_posts` path, **also** write to the catalog (dual-write).
3. Background job or startup: **backfill** from current `SOURCE_CACHE` into the catalog (optional but helps testing).
4. Metrics/logging: row counts, ingest latency, query latency for a sample “home new” query.

**Exit:** Catalog matches per-source data for a test instance; no route uses it yet.

### Phase B — Read path for home “new” (feature-flagged)

1. Implement `query_home_new(user_subs, filters, cursor, limit)` returning `Vec<Post>`.
2. Wire **contextual front page** (`new` only) to use the query behind a flag; fall back to current merge on failure.
3. Integrate **dedupe** and **seen_posts** on the returned stream; run **integration tests** mirroring `subreddit.rs` fragment tests.

**Exit:** A/B parity on post order and counts for a few fixed fixtures (subscriptions + cache snapshots).

### Phase C — Video mode + user surfaces

1. Route **video length** listing to pull from catalog + `video_mode::build_listing`.
2. **User submitted** (and optionally following) from catalog where possible; keep legacy fetch for gaps.

**Exit:** Same sidebar facet counts / ordering as before on sampled queries.

### Phase D — Decommission hot path merge for `new`

1. Remove or narrow `merged_posts_for_session` for **default home new** once stable.
2. Reduce duplicate `feed_page` / `feed_status` / `feed_merged_posts` **full merges** to **one query + cached session tail** where applicable.

### Phase E — Persistence

1. Extend disk snapshot (or add a second file) for the catalog so restarts do not cold-start Reddit.
2. Document recovery: rebuild catalog from per-source cache if catalog corrupt.

---

## 5. Testing strategy (parity)

- **Unit:** Ingest → query → compare ordering and membership to **golden merge** output from current `merged_posts_for_session` on the same `SOURCE_CACHE` fixture.
- **Duplicates:** Same posts in two subs → one visible primary + `collapsed_duplicates` as today.
- **Video stats:** After ingesting a batch, `cached_user_video_stats` / UI fields match pre-index behavior for controlled `Post` vectors.
- **Video mode:** `build_listing` output unchanged given identical input `Vec<Post>`.
- **Seen:** Hide-seen pagination tests in `subreddit.rs` still pass with index-backed stream.

---

## 6. Risks and mitigations

| Risk | Mitigation |
|------|------------|
| Catalog drift vs `SOURCE_CACHE` | Dual-write during migration; periodic consistency check in tests |
| Larger DB / memory | TTL or cap by `(subreddit, created_ts)`; optional compaction |
| Following / multi-source user bundles | Explicit `feed_kind` + tests for `/following` |
| OAuth / quarantine / deleted placeholders | Same filters as `filtered_source_queues` at query or ingest |

---

## 7. Summary

**Yes, it should be faster** for large subscription lists: work moves to **daemon-time ingestion** and **indexed reads** at visit time.

**Feature parity** is not automatic—it requires **hooking every post-touch** that today runs on cache write or render (`video stats`, **RedGIFs**-related keys, **author link** hydration, **duplicate** collapse, **seen** behavior, **video mode** pipeline, **user** routes). The table above is the checklist to close before removing the old merge path for **new**.
