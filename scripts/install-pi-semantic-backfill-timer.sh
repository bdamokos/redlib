#!/usr/bin/env bash

set -euo pipefail

if [[ "$(id -u)" -eq 0 ]]; then
	SUDO=()
else
	SUDO=(sudo)
fi

IMAGE="${REDLIB_SEMANTIC_BACKFILL_IMAGE:-127.0.0.1:5010/redlib:latest}"
MODEL="${REDLIB_SUBREDDIT_SEMANTIC_MODEL:-nomic-embed-text}"
default_ollama_url() {
	local url
	for url in http://127.0.0.1:11434 http://172.17.0.1:11434; do
		if command -v curl >/dev/null 2>&1 && curl -fsS --max-time 2 "$url/api/version" >/dev/null 2>&1; then
			printf '%s' "$url"
			return
		fi
	done
	printf '%s' "http://127.0.0.1:11434"
}

OLLAMA_URL="${OLLAMA_BASE_URL:-$(default_ollama_url)}"
CONTENT_MAX_POSTS="${REDLIB_SUBREDDIT_CONTENT_MAX_POSTS:-16}"
CONTENT_MAX_POST_CHARS="${REDLIB_SUBREDDIT_CONTENT_MAX_POST_CHARS:-360}"
CONTENT_MAX_CORPUS_CHARS="${REDLIB_SUBREDDIT_CONTENT_MAX_CORPUS_CHARS:-3000}"
CONTENT_MIN_POSTS="${REDLIB_SUBREDDIT_CONTENT_MIN_POSTS:-6}"
CONTENT_MIN_CHARS="${REDLIB_SUBREDDIT_CONTENT_MIN_CHARS:-180}"
CPUS="${REDLIB_SEMANTIC_BACKFILL_CPUS:-1.5}"
MEMORY="${REDLIB_SEMANTIC_BACKFILL_MEMORY:-1536m}"

"${SUDO[@]}" install -m 0644 /dev/null /etc/default/redlib-semantic-backfill
"${SUDO[@]}" tee /etc/default/redlib-semantic-backfill >/dev/null <<EOF
REDLIB_SEMANTIC_BACKFILL_IMAGE=${IMAGE}
REDLIB_SUBREDDIT_SEMANTIC_MODEL=${MODEL}
OLLAMA_BASE_URL=${OLLAMA_URL}
REDLIB_SUBREDDIT_CONTENT_MAX_POSTS=${CONTENT_MAX_POSTS}
REDLIB_SUBREDDIT_CONTENT_MAX_POST_CHARS=${CONTENT_MAX_POST_CHARS}
REDLIB_SUBREDDIT_CONTENT_MAX_CORPUS_CHARS=${CONTENT_MAX_CORPUS_CHARS}
REDLIB_SUBREDDIT_CONTENT_MIN_POSTS=${CONTENT_MIN_POSTS}
REDLIB_SUBREDDIT_CONTENT_MIN_CHARS=${CONTENT_MIN_CHARS}
REDLIB_SEMANTIC_BACKFILL_CPUS=${CPUS}
REDLIB_SEMANTIC_BACKFILL_MEMORY=${MEMORY}
REDLIB_CACHE_DIR=/var/lib/redlib/cache
EOF

"${SUDO[@]}" tee /etc/systemd/system/redlib-semantic-backfill.service >/dev/null <<'EOF'
[Unit]
Description=Redlib content semantic embedding refresh
Requires=docker.service ollama.service
After=docker.service ollama.service

[Service]
Type=oneshot
EnvironmentFile=/etc/default/redlib-semantic-backfill
ExecStart=/bin/sh -c 'set -eu; /usr/bin/docker run --rm --entrypoint /usr/local/bin/subreddit_content_semantic_backfill "$REDLIB_SEMANTIC_BACKFILL_IMAGE" --help | grep -q "content semantic sidecar"; /usr/bin/docker run --rm --name redlib-semantic-backfill --network host --user nobody --cpus "$REDLIB_SEMANTIC_BACKFILL_CPUS" --memory "$REDLIB_SEMANTIC_BACKFILL_MEMORY" -e REDLIB_CACHE_DIR="$REDLIB_CACHE_DIR" -e REDLIB_SUBREDDIT_SEMANTIC_MODEL="$REDLIB_SUBREDDIT_SEMANTIC_MODEL" -e OLLAMA_BASE_URL="$OLLAMA_BASE_URL" -e REDLIB_SUBREDDIT_CONTENT_MAX_POSTS="$REDLIB_SUBREDDIT_CONTENT_MAX_POSTS" -e REDLIB_SUBREDDIT_CONTENT_MAX_POST_CHARS="$REDLIB_SUBREDDIT_CONTENT_MAX_POST_CHARS" -e REDLIB_SUBREDDIT_CONTENT_MAX_CORPUS_CHARS="$REDLIB_SUBREDDIT_CONTENT_MAX_CORPUS_CHARS" -e REDLIB_SUBREDDIT_CONTENT_MIN_POSTS="$REDLIB_SUBREDDIT_CONTENT_MIN_POSTS" -e REDLIB_SUBREDDIT_CONTENT_MIN_CHARS="$REDLIB_SUBREDDIT_CONTENT_MIN_CHARS" -v redlib-cache:/var/lib/redlib/cache "$REDLIB_SEMANTIC_BACKFILL_IMAGE" subreddit_content_semantic_backfill --quiet'
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
