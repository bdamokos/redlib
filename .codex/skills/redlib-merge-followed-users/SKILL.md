---
name: redlib-merge-followed-users
description: Merge or recover followed-user preference lists for the user's deployed Redlib instance on Raspberry Pi. Use when working in /Users/bence/Developer/redlib and the user asks to merge desktop/mobile following, restore followed users, inspect Redlib server-side preferences, or repair the deployed Pi preference DB.
---

# Redlib Merge Followed Users

## Workflow

1. Start from `/Users/bence/Developer/redlib` and read `AGENTS.md`. The deployed Pi instance lives in `~/redlib`, uses `compose.pi.yaml`, and serves local HTTP on `127.0.0.1:8081`.

2. Inspect the live preference rows before editing. The Redlib preference DB is `/var/lib/redlib/state/preferences.sqlite3` in the container and normally `/var/lib/docker/volumes/redlib-state/_data/preferences.sqlite3` on the Pi host. If direct host access is blocked, copy it out with:

```bash
docker cp redlib:/var/lib/redlib/state/preferences.sqlite3 /tmp/redlib-prefs.sqlite3
```

3. Parse `preferences.prefs_json` as JSON. `subscriptions` is a `+`-joined string; followed users are entries beginning with `u_`. Compare rows by `last_visit_unix`, `updated_at_unix`, total subscription count, followed-user count, filters, and settings. Treat rows with non-empty followed-user lists as candidate device profiles.

4. Merge only what the user asked for. For followed-user merge tasks, build a case-insensitive union of `u_...` entries, keep each row's non-user subscriptions in its existing order, replace that row's user block with the sorted union, and leave filters, flair subscriptions, theme, layout, and other settings unchanged.

5. Back up before writing. Prefer stopping Redlib briefly while writing the SQLite file:

```bash
cd ~/redlib
docker compose -f compose.pi.yaml stop redlib
sudo cp -a /var/lib/docker/volumes/redlib-state/_data/preferences.sqlite3 \
  /var/lib/docker/volumes/redlib-state/_data/preferences.sqlite3.pre-followed-user-merge-$(date +%s)
```

6. When updating rows directly, set `updated_at_unix` to now and increment `revision`; do not change `last_visit_unix` unless the task explicitly needs visit freshness. After writing as root, restore ownership and permissions for the container user:

```bash
sudo chown nobody:nogroup /var/lib/docker/volumes/redlib-state/_data/preferences.sqlite3*
sudo chmod 644 /var/lib/docker/volumes/redlib-state/_data/preferences.sqlite3*
docker compose -f compose.pi.yaml up -d redlib
```

7. Verify through the deployed app, not just SQLite. For each merged prefs id:

```bash
curl -fsS -H "Cookie: rl_prefs_id=<id>" http://127.0.0.1:8081/settings
```

Count `/user/...` links in the rendered settings page and confirm the expected merged followed-user count. A fresh restart may show Docker health as `starting` because this compose healthcheck interval is five minutes; `/settings` serving is the immediate readiness proof.
