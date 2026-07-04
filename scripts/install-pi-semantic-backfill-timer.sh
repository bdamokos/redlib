#!/usr/bin/env bash

set -euo pipefail

if [[ "$(id -u)" -eq 0 ]]; then
	SUDO=()
else
	SUDO=(sudo)
fi

IMAGE="${REDLIB_SEMANTIC_BACKFILL_IMAGE:-127.0.0.1:5010/redlib:latest}"
MODEL="${REDLIB_SUBREDDIT_SEMANTIC_MODEL:-nomic-embed-text}"
OLLAMA_URL="${OLLAMA_BASE_URL:-http://127.0.0.1:11434}"
METADATA_REFRESH_LIMIT="${REDLIB_SEMANTIC_BACKFILL_METADATA_REFRESH_LIMIT:-200}"
CPUS="${REDLIB_SEMANTIC_BACKFILL_CPUS:-1.5}"
MEMORY="${REDLIB_SEMANTIC_BACKFILL_MEMORY:-1536m}"

"${SUDO[@]}" install -m 0644 /dev/null /etc/default/redlib-semantic-backfill
"${SUDO[@]}" tee /etc/default/redlib-semantic-backfill >/dev/null <<EOF
REDLIB_SEMANTIC_BACKFILL_IMAGE=${IMAGE}
REDLIB_SUBREDDIT_SEMANTIC_MODEL=${MODEL}
OLLAMA_BASE_URL=${OLLAMA_URL}
REDLIB_SEMANTIC_BACKFILL_METADATA_REFRESH_LIMIT=${METADATA_REFRESH_LIMIT}
REDLIB_SEMANTIC_BACKFILL_CPUS=${CPUS}
REDLIB_SEMANTIC_BACKFILL_MEMORY=${MEMORY}
REDLIB_CACHE_DIR=/var/lib/redlib/cache
EOF

"${SUDO[@]}" tee /etc/systemd/system/redlib-semantic-backfill.service >/dev/null <<'EOF'
[Unit]
Description=Redlib semantic subreddit embedding refresh
Requires=docker.service ollama.service
After=docker.service ollama.service

[Service]
Type=oneshot
EnvironmentFile=/etc/default/redlib-semantic-backfill
ExecStart=/bin/sh -c 'set -eu; was_running=0; if /usr/bin/docker inspect -f "{{.State.Running}}" redlib 2>/dev/null | grep -qx true; then was_running=1; /usr/bin/docker stop redlib >/dev/null; fi; cleanup() { if [ "$was_running" = "1" ]; then /usr/bin/docker start redlib >/dev/null; fi; }; trap cleanup EXIT; /usr/bin/docker run --rm --name redlib-semantic-backfill --network host --user nobody --cpus "$REDLIB_SEMANTIC_BACKFILL_CPUS" --memory "$REDLIB_SEMANTIC_BACKFILL_MEMORY" -e REDLIB_CACHE_DIR="$REDLIB_CACHE_DIR" -e REDLIB_SUBREDDIT_SEMANTIC_MODEL="$REDLIB_SUBREDDIT_SEMANTIC_MODEL" -e OLLAMA_BASE_URL="$OLLAMA_BASE_URL" -v redlib-cache:/var/lib/redlib/cache "$REDLIB_SEMANTIC_BACKFILL_IMAGE" subreddit_semantic_backfill --refresh-missing-metadata --metadata-refresh-limit "$REDLIB_SEMANTIC_BACKFILL_METADATA_REFRESH_LIMIT" --name-only-if-missing-metadata --quiet'
Nice=10
IOSchedulingClass=best-effort
IOSchedulingPriority=7
TimeoutStartSec=3h

[Install]
WantedBy=multi-user.target
EOF

"${SUDO[@]}" tee /etc/systemd/system/redlib-semantic-backfill.timer >/dev/null <<'EOF'
[Unit]
Description=Run Redlib semantic subreddit embedding refresh overnight

[Timer]
OnCalendar=*-*-* 03:00:00
RandomizedDelaySec=9900
Persistent=false
Unit=redlib-semantic-backfill.service

[Install]
WantedBy=timers.target
EOF

"${SUDO[@]}" systemctl daemon-reload
"${SUDO[@]}" systemctl enable --now redlib-semantic-backfill.timer
"${SUDO[@]}" systemctl list-timers redlib-semantic-backfill.timer --no-pager
