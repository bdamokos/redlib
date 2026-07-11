#!/usr/bin/env python3
"""Build a deterministic, auditable NSFWDOG semantic-embedding plan.

The planner is read-only. It never calls Reddit, Ollama, or Redlib write paths.
It consumes a cached NSFWDOG catalog snapshot plus an optional newline-delimited
inventory of already embedded subreddit names and emits a ledger and batches.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import re
import time
import urllib.request
from pathlib import Path

API = "https://api2.nsfwdog.com"
ALLOWED_RANGES = {"10k–100k": 0, "5k–10k": 1, "1k–5k": 2}
PERSON_PATTERNS = [
    re.compile(pattern, re.I)
    for pattern in (
        r"\bfan (?:community|subreddit|page|club)\b",
        r"\bdedicated to\b",
        r"\bcontent (?:of|from) (?:adult model|creator|actress|model|pornstar|performer)\b",
        r"\b(?:onlyfans|fansly) (?:creator|model)\b",
        r"\bfor (?:adult model|creator|actress|model|pornstar|performer)\b",
        r"\bpersonal subreddit\b",
        r"\b(?:her|his|their|a user's) own content\b",
    )
]


def get_json(url: str) -> dict:
    request = urllib.request.Request(url, headers={"User-Agent": "redlib-nsfwdog-plan/1.0", "Accept": "application/json"})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def fetch_page(page: int) -> list[dict]:
    return get_json(f"{API}/v1/subreddits/?page={page}")["results"]


def snapshot_catalog(path: Path, workers: int) -> dict:
    first = get_json(f"{API}/v1/subreddits/?page=1")
    page_size = len(first["results"])
    pages = (first["count"] + page_size - 1) // page_size
    rows = list(first["results"])
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        for page_rows in pool.map(fetch_page, range(2, pages + 1)):
            rows.extend(page_rows)
    snapshot = {
        "source_url": f"{API}/v1/subreddits/",
        "retrieved_at_unix": int(time.time()),
        "reported_count": first["count"],
        "page_size": page_size,
        "pages": pages,
        "results": rows,
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(snapshot, sort_keys=True, separators=(",", ":")) + "\n")
    return snapshot


def normalized_name(value: str) -> str:
    value = value.strip().lower()
    value = value.removeprefix("/r/").removeprefix("r/")
    return re.sub(r"[^a-z0-9_]", "", value)


def exclusion_reason(row: dict, singular_person_hashes: set[str]) -> str | None:
    subscriber_range = row.get("subscriber_range", "")
    if subscriber_range == "<1k":
        return "extremely_niche_lt_1k"
    if subscriber_range not in ALLOWED_RANGES:
        return "outside_small_mid_1k_100k"
    text = " ".join(str(row.get(key, "")) for key in ("heading", "subheading"))
    if any(pattern.search(text) for pattern in PERSON_PATTERNS):
        return "singular_person_fan_heuristic"
    if singular_person_hashes.intersection(row.get("hierarchy_hashes", [])):
        return "singular_person_fandom_taxonomy"
    if not row.get("has_similar"):
        return "inactive_no_similarity_signal"
    if not row.get("hierarchy_hashes"):
        return "not_special_interest_no_taxonomy"
    return None


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--snapshot", type=Path, required=True)
    parser.add_argument("--refresh-snapshot", action="store_true")
    parser.add_argument("--already-embedded", type=Path)
    parser.add_argument("--hierarchy", type=Path)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--ceiling", type=int, default=1000)
    parser.add_argument("--batch-size", type=int, default=50)
    parser.add_argument("--workers", type=int, default=24)
    args = parser.parse_args()
    if not 1 <= args.ceiling <= 1000 or args.batch_size != 50:
        raise SystemExit("ceiling must be 1..1000 and batch size must be exactly 50")

    snapshot = snapshot_catalog(args.snapshot, args.workers) if args.refresh_snapshot or not args.snapshot.exists() else json.loads(args.snapshot.read_text())
    snapshot_sha256 = hashlib.sha256(args.snapshot.read_bytes()).hexdigest()
    embedded = set()
    if args.already_embedded and args.already_embedded.exists():
        embedded = {normalized_name(line) for line in args.already_embedded.read_text().splitlines() if normalized_name(line)}
    singular_person_hashes = set()
    if args.hierarchy:
        def walk(nodes: list[dict]) -> None:
            for node in nodes:
                if node.get("name", "").casefold() in {"fandom", "famous"}:
                    singular_person_hashes.add(node["hash"])
                walk(node.get("children", []))
        walk(json.loads(args.hierarchy.read_text()))

    exclusions: dict[str, int] = {}
    eligible = []
    seen = set()
    for row in snapshot["results"]:
        reason = exclusion_reason(row, singular_person_hashes)
        if reason:
            exclusions[reason] = exclusions.get(reason, 0) + 1
            continue
        slug = row["slug"]
        if slug in seen:
            exclusions["duplicate_slug"] = exclusions.get("duplicate_slug", 0) + 1
            continue
        seen.add(slug)
        # Resolved Reddit names are filled by the metadata-gated execution step.
        # A slug-derived hint is used only for dedupe planning, never eligibility.
        name_hint = normalized_name(slug.replace("-", "_"))
        if name_hint in embedded:
            exclusions["already_embedded_name_hint"] = exclusions.get("already_embedded_name_hint", 0) + 1
            continue
        eligible.append({
            "slug": slug,
            "name_hint": name_hint,
            "heading": row.get("heading", ""),
            "subheading": row.get("subheading", ""),
            "subscriber_range": row.get("subscriber_range", ""),
            "hierarchy_hashes": sorted(row.get("hierarchy_hashes", [])),
            "source_url": f"{API}/v1/subreddits/{slug}/name/",
            "metadata_gate": "pending_required",
        })

    eligible.sort(key=lambda row: (ALLOWED_RANGES[row["subscriber_range"]], -len(row["hierarchy_hashes"]), row["slug"]))
    # Round-robin the first taxonomy hash so the ceiling is not monopolized by
    # one interest cluster. Sorting within a cluster remains deterministic.
    buckets: dict[str, list[dict]] = {}
    for row in eligible:
        buckets.setdefault(row["hierarchy_hashes"][0], []).append(row)
    selected = []
    bucket_names = sorted(buckets)
    while len(selected) < args.ceiling and bucket_names:
        remaining = []
        for bucket in bucket_names:
            if buckets[bucket] and len(selected) < args.ceiling:
                selected.append(buckets[bucket].pop(0))
            if buckets[bucket]:
                remaining.append(bucket)
        bucket_names = remaining
    args.output_dir.mkdir(parents=True, exist_ok=True)
    ledger = args.output_dir / "candidate-ledger.jsonl"
    ledger.write_text("".join(json.dumps({"ordinal": i + 1, **row}, sort_keys=True) + "\n" for i, row in enumerate(selected)))
    batches = []
    for offset in range(0, len(selected), args.batch_size):
        number = offset // args.batch_size + 1
        rows = selected[offset : offset + args.batch_size]
        manifest = args.output_dir / f"batch-{number:03d}.jsonl"
        manifest.write_text("".join(json.dumps({"ordinal": offset + i + 1, **row}, sort_keys=True) + "\n" for i, row in enumerate(rows)))
        batches.append({
            "batch": number,
            "start_ordinal": offset + 1,
            "count": len(rows),
            "manifest": manifest.name,
            "state": "not_released",
            "attempted": 0,
            "succeeded": 0,
            "skipped": 0,
            "failed": 0,
            "skipped_identifiers": [],
            "failed_identifiers": [],
        })
    summary = {
        "schema_version": 1,
        "planner": "scripts/nsfwdog_embedding_plan.py",
        "source": snapshot["source_url"],
        "snapshot_sha256": snapshot_sha256,
        "snapshot_retrieved_at_unix": snapshot["retrieved_at_unix"],
        "source_rows": len(snapshot["results"]),
        "selection_rules": {"subscriber_ranges": list(ALLOWED_RANGES), "requires_has_similar": True, "requires_taxonomy": True, "person_patterns": [p.pattern for p in PERSON_PATTERNS], "singular_person_taxonomy_hashes": sorted(singular_person_hashes)},
        "exclusions": exclusions,
        "already_embedded_inventory_count": len(embedded),
        "eligible_after_filters_and_dedupe": len(eligible),
        "selected": len(selected),
        "batch_size": args.batch_size,
        "batches": batches,
        "resume_state": {"next_batch": 1 if batches else None, "next_ordinal": 1 if selected else None, "state": "not_released"},
        "execution_gate": "Resolve every slug; refresh Reddit metadata successfully; require NSFW=true plus accounts_active>0 and at least one non-sticky post in the prior 30 days; re-dedupe exact normalized name and current text hash; then embed. No name-only fallback.",
    }
    (args.output_dir / "plan-summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    print(json.dumps(summary, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
