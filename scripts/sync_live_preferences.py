#!/usr/bin/env python3
"""Synchronize live Redlib preference rows on the Raspberry Pi deploy.

The default behavior targets the two most recently active non-empty preference
rows, unions subscriptions and filters, copies scalar settings from the most
recent row, backs up the SQLite DB, restarts Redlib, and verifies /settings.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys


REMOTE_CODE = r'''
import copy
import datetime
import hashlib
import json
import os
import re
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import urllib.request

CONFIG = json.loads(__CONFIG_JSON__)
DB_NAME = "preferences.sqlite3"
DB_PATH = "/state/" + DB_NAME


def run(args, cwd=None, check=True, quiet=False):
    if not quiet:
        print("+", " ".join(args), flush=True)
    return subprocess.run(args, cwd=cwd, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=check)


def compose(*args, check=True):
    return run(["docker", "compose", "-f", "compose.pi.yaml", *args], cwd=CONFIG["compose_dir"], check=check)


def helper(code, workdir, check=True):
    return run(
        [
            "docker",
            "run",
            "--rm",
            "-v",
            "redlib-state:/state",
            "-v",
            f"{workdir}:/work",
            CONFIG["helper_image"],
            "python",
            "-c",
            code,
        ],
        check=check,
    )


def split_plus(value):
    if value is None:
        return []
    if isinstance(value, list):
        return [str(item) for item in value if str(item)]
    return [part for part in str(value).split("+") if part]


def plus_join(values):
    return "+".join(values)


def dedupe_key(value):
    return str(value).casefold()


def ordered_union(records, field):
    merged = []
    seen = set()
    for record in records:
        for value in split_plus(record["prefs"].get(field)):
            key = dedupe_key(value)
            if key in seen:
                continue
            seen.add(key)
            merged.append(value)
    return merged


def human(ts):
    return datetime.datetime.fromtimestamp(int(ts), datetime.timezone.utc).isoformat(timespec="seconds")


def hash_values(values):
    return hashlib.sha1("\n".join(sorted(str(value).casefold() for value in values)).encode()).hexdigest()[:12]


def summarize_prefs(prefs):
    subs = split_plus(prefs.get("subscriptions"))
    filters = split_plus(prefs.get("filters"))
    users = [item for item in subs if item.casefold().startswith("u_")]
    subreddits = [item for item in subs if not item.casefold().startswith("u_")]
    scalar = copy.deepcopy(prefs)
    scalar["subscriptions"] = ""
    scalar["filters"] = ""
    return {
        "subscriptions": len(subs),
        "users": len(users),
        "subreddits": len(subreddits),
        "filters": len(filters),
        "subscription_hash": hash_values(subs),
        "filter_hash": hash_values(filters),
        "scalar_hash": hashlib.sha1(json.dumps(scalar, sort_keys=True, separators=(",", ":")).encode()).hexdigest()[:12],
    }


def read_rows(conn):
    rows = conn.execute(
        """
        SELECT id, created_at_unix, updated_at_unix, last_visit_unix, revision, prefs_json
        FROM preferences
        ORDER BY last_visit_unix DESC
        """
    ).fetchall()
    records = []
    for row in rows:
        id_, created, updated, last, revision, prefs_json = row
        prefs = json.loads(prefs_json)
        records.append(
            {
                "id": id_,
                "created": int(created),
                "updated": int(updated),
                "last": int(last),
                "revision": int(revision),
                "prefs": prefs,
                "prefs_json": prefs_json,
            }
        )
    return records


def target_records(records):
    explicit = CONFIG["target_ids"]
    if explicit:
        by_id = {record["id"]: record for record in records}
        missing = [id_ for id_ in explicit if id_ not in by_id]
        if missing:
            raise RuntimeError(f"target preference ids not found: {', '.join(missing)}")
        return [by_id[id_] for id_ in explicit]

    nonempty = [
        record
        for record in records
        if split_plus(record["prefs"].get("subscriptions")) or split_plus(record["prefs"].get("filters"))
    ]
    if len(nonempty) < CONFIG["target_count"]:
        raise RuntimeError(f"found only {len(nonempty)} non-empty rows, need {CONFIG['target_count']}")
    return nonempty[: CONFIG["target_count"]]


def source_record(targets):
    source_id = CONFIG["source_id"]
    if source_id:
        for record in targets:
            if record["id"] == source_id:
                return record
        raise RuntimeError(f"source preference id {source_id} is not one of the target rows")
    return targets[0]


def install_merged_db(workdir):
    code = f"""
import os
import shutil
import stat
state = {DB_PATH!r}
merged = "/work/preferences.merged.sqlite3"
st = os.stat(state)
shutil.copy2(merged, state)
os.chown(state, st.st_uid, st.st_gid)
os.chmod(state, stat.S_IMODE(st.st_mode))
for suffix in ("-wal", "-shm"):
    path = state + suffix
    if os.path.exists(path):
        os.remove(path)
        print("removed", path, flush=True)
print("installed merged DB", state, flush=True)
"""
    result = helper(code, workdir)
    if result.stdout.strip():
        print(result.stdout.strip(), flush=True)
    if result.stderr.strip():
        print(result.stderr.strip(), file=sys.stderr, flush=True)


def copy_db_to_workdir(workdir, backup_suffix):
    backup_clause = "" if CONFIG["dry_run"] else f"shutil.copy2(src, backup_base + suffix)"
    code = f"""
import os
import shutil
os.makedirs("/work/source", exist_ok=True)
os.chmod("/work/source", 0o777)
base = {DB_PATH!r}
backup_base = base + {backup_suffix!r}
for suffix in ("", "-wal", "-shm"):
    src = base + suffix
    if os.path.exists(src):
        dst = "/work/source/{DB_NAME}" + suffix
        shutil.copy2(src, dst)
        os.chmod(dst, 0o666)
        {backup_clause}
        if {str(CONFIG["dry_run"])}:
            print("copied", src, "to working snapshot", flush=True)
        else:
            print("copied", src, "to backup", backup_base + suffix, flush=True)
"""
    result = helper(code, workdir)
    if result.stdout.strip():
        print(result.stdout.strip(), flush=True)
    if result.stderr.strip():
        print(result.stderr.strip(), file=sys.stderr, flush=True)


def wait_for_settings():
    last_error = None
    for _ in range(CONFIG["readiness_attempts"]):
        try:
            with urllib.request.urlopen("http://127.0.0.1:8081/settings", timeout=3) as response:
                response.read(200)
                print("settings_probe:", response.status, flush=True)
                return
        except Exception as exc:
            last_error = exc
            time.sleep(1)
    raise RuntimeError(f"/settings did not respond after restart: {last_error}")


def verify_live(target_ids):
    verify_dir = tempfile.mkdtemp(prefix="redlib-prefs-sync-verify-")
    for suffix in ("", "-wal", "-shm"):
        subprocess.run(
            ["docker", "cp", f"redlib:/var/lib/redlib/state/{DB_NAME}" + suffix, os.path.join(verify_dir, DB_NAME + suffix)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    conn = sqlite3.connect(os.path.join(verify_dir, DB_NAME))
    quick_check = conn.execute("pragma quick_check").fetchone()[0]
    print("post_restart_quick_check:", quick_check, flush=True)
    if quick_check != "ok":
        raise RuntimeError("post-restart quick_check failed")

    by_id = {record["id"]: record for record in read_rows(conn)}
    print("post_restart_counts: id,last_visit,updated,revision,subscriptions,users,subreddits,filters,subscription_hash,filter_hash,scalar_hash")
    for id_ in target_ids:
        record = by_id[id_]
        summary = summarize_prefs(record["prefs"])
        print(
            ",".join(
                map(
                    str,
                    [
                        record["id"],
                        human(record["last"]),
                        human(record["updated"]),
                        record["revision"],
                        summary["subscriptions"],
                        summary["users"],
                        summary["subreddits"],
                        summary["filters"],
                        summary["subscription_hash"],
                        summary["filter_hash"],
                        summary["scalar_hash"],
                    ],
                )
            ),
            flush=True,
        )
    conn.close()

    for id_ in target_ids:
        request = urllib.request.Request("http://127.0.0.1:8081/settings", headers={"Cookie": f"rl_prefs_id={id_}"})
        with urllib.request.urlopen(request, timeout=5) as response:
            html = response.read().decode("utf-8", "replace")
        user_links = len(set(re.findall(r'href="/user/([^"?#]+)', html)))
        subreddit_links = len(set(re.findall(r'href="/r/([^"?#]+)', html)))
        print(
            f"settings_cookie_check: {id_} status=200 unique_user_links={user_links} unique_subreddit_links={subreddit_links}",
            flush=True,
        )


def main():
    status = run(["docker", "ps", "--filter", "name=^/redlib$", "--format", "{{.Names}} {{.Status}}"], check=False, quiet=True)
    print("container:", status.stdout.strip() or status.stderr.strip() or "not running", flush=True)
    workdir = tempfile.mkdtemp(prefix="redlib-prefs-sync-")
    timestamp = int(time.time())
    backup_suffix = f".pre-full-profile-sync-{timestamp}"
    print("workdir:", workdir, flush=True)
    if CONFIG["dry_run"]:
        print("dry_run: true", flush=True)
    else:
        print("volume_backup:", f"/var/lib/redlib/state/{DB_NAME}{backup_suffix}", flush=True)

    target_ids = []
    changed_rows = []
    try:
        compose("stop", "redlib")
        copy_db_to_workdir(workdir, backup_suffix)

        source_db = os.path.join(workdir, "source", DB_NAME)
        conn = sqlite3.connect(source_db)
        quick_check = conn.execute("pragma quick_check").fetchone()[0]
        print("pre_merge_quick_check:", quick_check, flush=True)
        if quick_check != "ok":
            raise RuntimeError("pre-merge quick_check failed")

        records = read_rows(conn)
        targets = target_records(records)
        source = source_record(targets)
        target_ids = [record["id"] for record in targets]
        print("source_row:", source["id"], flush=True)
        print("target_rows:")
        for record in targets:
            summary = summarize_prefs(record["prefs"])
            print(
                f"  {record['id']} last={human(record['last'])} rev={record['revision']} "
                f"subscriptions={summary['subscriptions']} users={summary['users']} "
                f"subreddits={summary['subreddits']} filters={summary['filters']} "
                f"scalar_hash={summary['scalar_hash']}",
                flush=True,
            )

        merged_subscriptions = ordered_union(targets, "subscriptions")
        merged_filters = ordered_union(targets, "filters")
        merged_prefs = copy.deepcopy(source["prefs"])
        merged_prefs["subscriptions"] = plus_join(merged_subscriptions)
        merged_prefs["filters"] = plus_join(merged_filters)
        merged_json = json.dumps(merged_prefs, separators=(",", ":"), ensure_ascii=False)
        now = int(time.time())

        print(
            "merged_union:",
            f"subscriptions={len(merged_subscriptions)}",
            f"users={sum(1 for item in merged_subscriptions if item.casefold().startswith('u_'))}",
            f"subreddits={sum(1 for item in merged_subscriptions if not item.casefold().startswith('u_'))}",
            f"filters={len(merged_filters)}",
            flush=True,
        )

        for record in targets:
            if record["prefs_json"] == merged_json:
                continue
            conn.execute(
                "UPDATE preferences SET updated_at_unix = ?, revision = revision + 1, prefs_json = ? WHERE id = ?",
                (now, merged_json, record["id"]),
            )
            before = summarize_prefs(record["prefs"])
            after = summarize_prefs(merged_prefs)
            changed_rows.append((record["id"], before, after))
        conn.commit()
        print("changed_rows:", len(changed_rows), flush=True)
        for id_, before, after in changed_rows:
            print(
                f"  {id_} subscriptions={before['subscriptions']}->{after['subscriptions']} "
                f"users={before['users']}->{after['users']} "
                f"subreddits={before['subreddits']}->{after['subreddits']} "
                f"filters={before['filters']}->{after['filters']} "
                f"scalar_hash={before['scalar_hash']}->{after['scalar_hash']}",
                flush=True,
            )

        merged_db = os.path.join(workdir, "preferences.merged.sqlite3")
        dst = sqlite3.connect(merged_db)
        conn.backup(dst)
        dst.close()
        conn.close()
        os.chmod(merged_db, 0o666)

        merged_conn = sqlite3.connect(merged_db)
        merged_quick_check = merged_conn.execute("pragma quick_check").fetchone()[0]
        print("merged_quick_check:", merged_quick_check, flush=True)
        if merged_quick_check != "ok":
            raise RuntimeError("merged quick_check failed")
        merged_conn.close()

        if changed_rows and not CONFIG["dry_run"]:
            install_merged_db(workdir)
        elif CONFIG["dry_run"]:
            print("dry-run complete; live DB not replaced", flush=True)
        else:
            print("no preference changes needed", flush=True)
    finally:
        up = compose("up", "-d", "redlib", check=False)
        if up.stdout.strip():
            print(up.stdout.strip(), flush=True)
        if up.stderr.strip():
            print(up.stderr.strip(), file=sys.stderr, flush=True)
        if up.returncode != 0:
            raise SystemExit(up.returncode)

    wait_for_settings()
    if target_ids:
        verify_live(target_ids)


main()
'''


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Sync live Redlib preference rows on the Pi.")
    parser.add_argument("--host", default="bence@100.119.233.126", help="SSH host for the Pi deploy")
    parser.add_argument("--compose-dir", default="/home/bence/redlib", help="Remote Redlib compose directory")
    parser.add_argument("--helper-image", default="python:3.11-slim", help="Docker image used for volume helper operations")
    parser.add_argument("--target-count", type=int, default=2, help="Number of most recent non-empty rows to sync")
    parser.add_argument("--target-id", action="append", default=[], help="Explicit rl_prefs_id to sync; may be repeated")
    parser.add_argument("--source-id", help="rl_prefs_id used as the source for scalar settings; defaults to newest target")
    parser.add_argument("--connect-timeout", type=int, default=10, help="SSH connect timeout in seconds")
    parser.add_argument("--readiness-attempts", type=int, default=40, help="Seconds to wait for /settings after restart")
    parser.add_argument("--dry-run", action="store_true", help="Compute the merge without replacing the live DB")
    return parser


def main() -> int:
    args = build_parser().parse_args()
    if args.target_count < 1:
        print("--target-count must be positive", file=sys.stderr)
        return 2

    config = {
        "compose_dir": args.compose_dir,
        "helper_image": args.helper_image,
        "target_count": args.target_count,
        "target_ids": args.target_id,
        "source_id": args.source_id,
        "dry_run": args.dry_run,
        "readiness_attempts": args.readiness_attempts,
    }
    remote_code = REMOTE_CODE.replace("__CONFIG_JSON__", repr(json.dumps(config)))
    ssh_cmd = [
        "ssh",
        "-o",
        "BatchMode=yes",
        "-o",
        f"ConnectTimeout={args.connect_timeout}",
        args.host,
        "python3",
        "-",
    ]
    result = subprocess.run(ssh_cmd, input=remote_code, text=True)
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
