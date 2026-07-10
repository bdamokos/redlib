#!/usr/bin/env bash

set -u -o pipefail

usage() {
	cat <<'EOF'
Usage: scripts/deploy-with-codex-notify.sh [--] COMMAND [ARGS...]

Runs a deploy command, mirrors output to a log, and notifies a Codex thread when
the command exits. A watchdog sends one "still running" message if the command is
still active after five minutes.

Environment:
  CODEX_NOTIFY_THREAD_ID       Default: 019f4cf0-4037-7101-addd-8dd170284f6b
  CODEX_NOTIFY_BIN             Default: /Applications/ChatGPT.app/Contents/Resources/codex
  CODEX_NOTIFY_WATCHDOG_SECS   Default: 300
  CODEX_NOTIFY_SEND_TIMEOUT_SECS Default: 60
  CODEX_NOTIFY_LOG_DIR         Default: /tmp/redlib-deploy-notify
  CODEX_NOTIFY_SEND            Default: on; set to off for local dry-run output
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
	usage
	exit 0
fi

if [[ "${1:-}" == "--" ]]; then
	shift
fi

if [[ $# -eq 0 ]]; then
	usage >&2
	exit 2
fi

THREAD_ID="${CODEX_NOTIFY_THREAD_ID:-019f4cf0-4037-7101-addd-8dd170284f6b}"
CODEX_BIN="${CODEX_NOTIFY_BIN:-/Applications/ChatGPT.app/Contents/Resources/codex}"
WATCHDOG_SECS="${CODEX_NOTIFY_WATCHDOG_SECS:-300}"
SEND_TIMEOUT_SECS="${CODEX_NOTIFY_SEND_TIMEOUT_SECS:-60}"
LOG_DIR="${CODEX_NOTIFY_LOG_DIR:-/tmp/redlib-deploy-notify}"
SEND_MODE="${CODEX_NOTIFY_SEND:-on}"
COMMAND_DISPLAY=$(printf '%q ' "$@")
COMMAND_DISPLAY=${COMMAND_DISPLAY% }
START_EPOCH=$(date +%s)
STAMP=$(date -u +"%Y%m%dT%H%M%SZ")
mkdir -p "$LOG_DIR"
STATE_DIR=$(mktemp -d "$LOG_DIR/state.XXXXXX")
LOG_FILE="$LOG_DIR/deploy-$STAMP-$$.log"
DONE_FILE="$STATE_DIR/done"

notify_thread() {
	local message="$1"
	if [[ "$SEND_MODE" == "off" ]]; then
		printf '%s\n' "$message"
		return 0
	fi
	if [[ ! -x "$CODEX_BIN" ]]; then
		printf 'Codex notify skipped: %s is not executable\n' "$CODEX_BIN" >&2
		return 0
	fi
	"$CODEX_BIN" exec resume "$THREAD_ID" "$message" --json --skip-git-repo-check >/dev/null 2>&1 &
	local notify_pid=$!
	local notify_started
	notify_started=$(date +%s)
	while kill -0 "$notify_pid" >/dev/null 2>&1; do
		local now
		now=$(date +%s)
		if (( now - notify_started >= SEND_TIMEOUT_SECS )); then
			kill "$notify_pid" >/dev/null 2>&1 || true
			sleep 1
			kill -9 "$notify_pid" >/dev/null 2>&1 || true
			wait "$notify_pid" >/dev/null 2>&1 || true
			printf 'Codex notify timed out after %ss\n' "$SEND_TIMEOUT_SECS" >&2
			return 0
		fi
		sleep 1
	done
	wait "$notify_pid" >/dev/null 2>&1 || true
}

tail_log() {
	if [[ -f "$LOG_FILE" ]]; then
		tail -80 "$LOG_FILE"
	fi
}

(
	sleep "$WATCHDOG_SECS"
	if [[ ! -f "$DONE_FILE" ]]; then
		now_epoch=$(date +%s)
		elapsed=$((now_epoch - START_EPOCH))
		message=$(cat <<EOF
Redlib deploy watchdog: command still running after ${elapsed}s.

Command: ${COMMAND_DISPLAY}
Working directory: $(pwd)
Log: ${LOG_FILE}

Recent output:
$(tail_log)
EOF
)
		notify_thread "$message"
	fi
) &
WATCHDOG_PID=$!

{
	printf 'Command: %s\n' "$COMMAND_DISPLAY"
	printf 'Working directory: %s\n' "$(pwd)"
	printf 'Started: %s\n' "$(date -Iseconds)"
	printf '\n'
} | tee "$LOG_FILE"

"$@" 2>&1 | tee -a "$LOG_FILE"
STATUS=${PIPESTATUS[0]}

touch "$DONE_FILE"
if kill -0 "$WATCHDOG_PID" >/dev/null 2>&1; then
	kill "$WATCHDOG_PID" >/dev/null 2>&1 || true
	wait "$WATCHDOG_PID" >/dev/null 2>&1 || true
fi

END_EPOCH=$(date +%s)
ELAPSED=$((END_EPOCH - START_EPOCH))
if [[ "$STATUS" -eq 0 ]]; then
	RESULT="succeeded"
else
	RESULT="failed with exit status $STATUS"
fi

message=$(cat <<EOF
Redlib deploy ${RESULT} after ${ELAPSED}s.

Command: ${COMMAND_DISPLAY}
Working directory: $(pwd)
Log: ${LOG_FILE}

Recent output:
$(tail_log)
EOF
)
notify_thread "$message"

rm -rf "$STATE_DIR"
exit "$STATUS"
