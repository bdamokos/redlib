# SQLite cache benchmark results

Date: 2026-07-22. These are architecture experiments, not production acceptance.

## Decision summary

The evidence supports one canonical ordinary-ROWID object table, normalized integer-key relationships, and materialized feed/thread entries only for active subscription profiles. Request-time normalized aggregation and a global card catalog are rejected.

## Mac real-cache experiment

Input was a read-only copy of the current RS1 `sources.v2` directory:

- 1,411 source files;
- 121,049,717 encoded bytes (119 MiB on disk);
- 51,647 source memberships;
- 50,833 unique Reddit objects;
- 119,060,864 bytes of unique per-object bincode payloads.

No live blob, cache volume, container or deployment state was modified.

### Initial comparator

The first feature-gated Rust harness imported the real files into the earlier
text-key, separate-payload, global-card design. Those measurements are retained
as a historical rejection result. The committed harness now uses canonical
ROWID objects while retaining global cards only as the aggregation comparator;
neither harness is production implementation.

- import with one `synchronous=FULL` transaction per source: 81.77 s;
- materialization: 5.38 s;
- database: 288,559,104 bytes;
- current RSS at completion: 26,476,544 bytes;
- `PRAGMA quick_check`: `ok`;
- killed uncommitted transaction: rolled back;
- killed post-commit transaction: durable.

The separate `WITHOUT ROWID` payload table consumed 250,454,016 bytes for 119,060,864 bytes of payload. That layout is rejected.

A second run used 32-source migration transactions, fixed-size duplicate-key
hashes and 8 KiB pages. It still grew to 400,474,112 bytes because the rejected
separate payload table interacted badly with payload overflow pages. Page-size
results therefore apply to a particular row layout; they are not a general
argument for increasing SQLite's page size.

On that same 8 KiB comparator, zstd level 1 reduced the database to 91,783,168
bytes. Import took 84.30 s, materialization 3.56 s, and hydration plus bincode
decode of 25 real objects measured 3.701/5.962/6.323 ms p50/p95/p99. The raw
8 KiB comparator measured about 3.187 ms p50 hydration. This is strong evidence
that payload compression deserves a direct corrected-ROWID comparison, but the
absolute database size is not transferable from the rejected table layout.
`PRAGMA quick_check`, concurrent readers and killed pre/post-commit recovery all
passed in the compressed run.

Real bincode timings, 100 warm repetitions unless noted:

| Operation | p50 | p95 | p99 |
|---|---:|---:|---:|
| New page 1 | 0.047 ms | 0.064 ms | 0.082 ms |
| New page 100 | 0.090 ms | 0.246 ms | 0.473 ms |
| New page 1000 | 0.069 ms | 0.149 ms | 0.355 ms |
| Activity page 1000 | 0.103 ms | 0.404 ms | 0.877 ms |
| Discovered page 1000 | 0.137 ms | 0.563 ms | 0.824 ms |
| Random page 100 | 0.051 ms | 0.052 ms | 0.055 ms |
| Submitted page 1 | 0.043 ms | 0.043 ms | 0.047 ms |
| Hydrate and bincode-decode 25 objects | 3.196 ms | 7.631 ms | 17.478 ms |
| Four concurrent readers | 0.049 ms | 0.237 ms | 0.462 ms |

The decisive negative result was request-time normalized aggregation over 100 selected sources: 86.0 ms p50, 166.3 ms p95 and 255.3 ms p99. Relationship normalization alone is insufficient for Home or Following; active-profile materialization is required.

The first video comparator query did not fully match the final composite cursor and had an anomalous page-20 p50 of 3.1 ms. It is not used as a final video-path claim; the arm64 profile benchmark below exercised the intended covering index.

### Corrected ROWID/profile-local layout

The real imported objects were copied into an ordinary `object_pk INTEGER PRIMARY KEY` table with payload stored in the same row. Relationships use integer foreign keys. Two deliberately broad active profiles were materialized: all cached Home sources and all cached followed-user sources. Home does not materialize comment-thread rows.

| Layout | Page size | Bytes |
|---|---:|---:|
| Rejected text-key/global comparator | 4 KiB | 288,559,104 |
| ROWID, unoptimized duplicate-key table | 8 KiB | 198,434,816 |
| ROWID, normalized relation tables | 8 KiB | 186,834,944 |
| ROWID, only Following thread materialization | 8 KiB | 183,517,184 |
| Same unoptimized ROWID DB | 4 KiB | 236,326,912 |

Eight-kibibyte pages saved about 52.8 MiB versus the equivalent four-kibibyte copy. The final 8 KiB experiment includes 42,026 Home entries, 13,137 Following entries, all normalized source memberships, duplicate keys/groups, thread groups/members, and 50,833 canonical payload objects. A keyset New query uses `profile_feed_new` directly.

The remaining gap above the 119 MiB unique payload sum is SQLite page/overflow overhead plus metadata, indexes and relationships. It replaces the current 119 MiB source files only after rollback retention ends; during migration both stores intentionally coexist.

### Direct corrected-ROWID harness

After the schema/runtime mismatch found by exact-SHA review was corrected, the
feature-gated harness imported the same 1,411 real files directly into canonical
ROWID object rows. It retains global feed cards only to compare the rejected
aggregation strategy; it is still not the production profile-local design.

| Payload | DB bytes | Import | Materialize | Hydrate 25 p50/p95/p99 |
|---|---:|---:|---:|---:|
| raw bincode | 171,032,576 | 27.55 s | 3.64 s | 0.895/1.033/1.204 ms |
| zstd level 1 | 76,570,624 | 34.36 s | 2.01 s | 1.706/1.857/1.860 ms |

Raw New/Activity/Discovered/Video keyset reads through page 1,000 remained
roughly 0.03-0.05 ms p50. Four concurrent readers had 0.035/0.128/0.198 ms
p50/p95/p99 and no errors. Both raw and compressed runs passed killed
pre/post-commit recovery and `PRAGMA quick_check`.

Even with compact integer relationships, request-time aggregation over 100
selected sources cost 20.1/23.8/26.1 ms p50/p95/p99 in the raw run. This
independently preserves the decision to materialize active-profile cards during
bounded refresh transactions.

## Raspberry Server 2 arm64 synthetic experiment

Environment: Python 3.13.5, SQLite 3.46.1, aarch64, WAL, `synchronous=NORMAL`, 8 MiB cache, mmap disabled. Fixture: 1,400 sources x 50 memberships, 64,400 canonical objects, 1.7 KiB payloads, exact/shared duplicates, comment/submission bundles and four active profiles.

| Design | DB size | Import |
|---|---:|---:|
| Normalized only | 139.02 MiB | 1.76 s |
| Global cards | 147.79 MiB | 2.32 s |
| Active-profile entries | 149.08 MiB | 2.49 s |

Active-profile query results:

- warm p50 New/Random/Video/Submitted/Comments: 0.057/0.051/0.063/0.026/0.092 ms;
- cold-ish: 0.139/0.195/0.227/0.123/0.265 ms;
- New across 400 keyset pages p50/p95/p99: 0.111/0.125/0.151 ms;
- affected-source refresh p50/p95/p99: 0.812/1.450/1.725 ms;
- concurrent read p50/p95/p99: 0.099/0.240/0.377 ms;
- concurrent write p50/p95/p99: 1.556/2.329/2.424 ms, no errors;
- WAL peak: about 4.08 MiB;
- killed uncommitted transaction: `quick_check=ok`, no leaked rows.

A full profile rebuild was about 300 ms p50 and is rejected. Refresh and preference changes must stage affected object, duplicate-group and thread-group IDs and update reference counts/materialized versions set-wise.

The cold-ish test reset SQLite caches but did not force cold storage reads. Linux RSS/PSS included about 104 MiB of in-process fixture construction; the meaningful materialization increment was about 8-9 MiB.

## Remaining gates

- Compare 8 KiB and 16 KiB pages in the direct corrected-ROWID importer.
- Measure zstd level-1 through actual end-to-end page rendering, not only
  payload hydration, before choosing the production payload codec.
- Batch migration checkpoints so `FULL` durability does not require 1,411 fsync-bound commits; measure 16/32/64-source batches and crash redo bounds.
- Compare `FULL` and `NORMAL` on the target filesystem. Production defaults to `FULL` until discovery durability is explicitly proven equivalent.
- Exercise exact current duplicate-key normalization, promotion/deleted filters and profile-local representative choice.
- Run real multi-user comment bundles and missing-linked-submission cases through semantic page comparison.
- Measure the actual Redlib process RSS/PSS, render-cache caps and multiple frozen sessions.
- Prove Home Hot latency and source diversity in the integrated service before deployment handoff.
