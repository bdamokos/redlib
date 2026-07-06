#!/bin/bash

set -euo pipefail

cd "$(dirname "$0")/.."

TARGET=${TARGET:-both}
DRY_RUN=false
DEPLOY_ARGS=()

usage() {
	cat <<EOF
Usage: $0 [--target main|spare|both] [--dry-run] [deploy.sh options]

Deploy targets:
  main   Server 1 at 192.168.50.25, port 8081, with Server 2 peer API fallback
  spare  Server 2 at 192.168.50.251, port 18081, daemon/cache workers disabled
  both   Deploy main, then update spare from the main registry image

Environment overrides:
  MAIN_SERVER_HOST                 Default: 192.168.50.25
  MAIN_REGISTRY_HOST_PUSH          Default: 192.168.50.25:5010
  MAIN_REGISTRY_HOST_PULL          Default: 127.0.0.1:5010
  MAIN_PEER_API_FALLBACK_SOURCES   Default: http://192.168.50.251:18081
  MAIN_REMOTE_DOCKER_CMD           Default: docker
  SPARE_SERVER_HOST                Default: 192.168.50.251
  SPARE_SSH_PROXY_JUMP             Default: raspberryserver
  SPARE_REGISTRY_HOST_PULL         Default: 192.168.50.25:5010
  SPARE_HOST_PORT                  Default: 18081
  SPARE_COMPOSE_PROJECT_NAME       Default: redlib-rate-test
  SPARE_CONTAINER_NAME             Default: redlib-rate-test
  SPARE_REMOTE_DOCKER_CMD          Default: sudo -n docker
EOF
}

while [[ $# -gt 0 ]]; do
	case "$1" in
		--target)
			TARGET=${2:-}
			if [[ -z "$TARGET" ]]; then
				echo "Error: --target requires main, spare, or both." >&2
				exit 1
			fi
			shift 2
			;;
		--target=*)
			TARGET=${1#*=}
			shift
			;;
		--dry-run)
			DRY_RUN=true
			shift
			;;
		-h|--help)
			usage
			exit 0
			;;
		main|spare|both)
			TARGET=$1
			shift
			;;
		*)
			DEPLOY_ARGS+=("$1")
			shift
			;;
	esac
done

case "$TARGET" in
	main|spare|both)
		;;
	*)
		echo "Error: target must be main, spare, or both; got '$TARGET'." >&2
		exit 1
		;;
esac

run_deploy() {
	local label="$1"
	shift
	local -a env_args=("$@")

	if [[ "$DRY_RUN" == true ]]; then
		printf '[dry-run] %s: ' "$label"
		printf '%q ' env "${env_args[@]}" ./deploy.sh
		if (( ${#DEPLOY_ARGS[@]} > 0 )); then
			printf '%q ' "${DEPLOY_ARGS[@]}"
		fi
		printf '\n'
		return
	fi

	echo "Deploying $label..."
	if (( ${#DEPLOY_ARGS[@]} > 0 )); then
		env "${env_args[@]}" ./deploy.sh "${DEPLOY_ARGS[@]}"
	else
		env "${env_args[@]}" ./deploy.sh
	fi
}

deploy_main() {
	run_deploy "main Server 1" \
		"SERVER_HOST=${MAIN_SERVER_HOST:-192.168.50.25}" \
		"REGISTRY_HOST_PUSH=${MAIN_REGISTRY_HOST_PUSH:-192.168.50.25:5010}" \
		"REGISTRY_HOST_PULL=${MAIN_REGISTRY_HOST_PULL:-127.0.0.1:5010}" \
		"HOST_PORT=${MAIN_HOST_PORT:-8081}" \
		"COMPOSE_PROJECT_NAME=${MAIN_COMPOSE_PROJECT_NAME:-redlib}" \
		"REDLIB_CONTAINER_NAME=${MAIN_CONTAINER_NAME:-redlib}" \
		"REDLIB_CACHE_VOLUME=${MAIN_CACHE_VOLUME:-redlib-cache}" \
		"REDLIB_STATE_VOLUME=${MAIN_STATE_VOLUME:-redlib-state}" \
		"REDLIB_PEER_API_FALLBACK_SOURCES=${MAIN_PEER_API_FALLBACK_SOURCES-http://192.168.50.251:18081}" \
		"REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS=${MAIN_PEER_API_FALLBACK_TIMEOUT_SECS:-5}" \
		"REDLIB_PEER_API_FALLBACK_FORCE_TESTING=${MAIN_PEER_API_FALLBACK_FORCE_TESTING:-off}" \
		"REDLIB_REMEMBERED_FEED_DAEMON_ENABLED=${MAIN_REMEMBERED_FEED_DAEMON_ENABLED:-on}" \
		"REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL=${MAIN_CONTENT_SEMANTIC_OBSERVED_BACKFILL:-on}" \
		"REMOTE_DOCKER_CMD=${MAIN_REMOTE_DOCKER_CMD:-docker}"
}

deploy_spare() {
	local -a original_args=()
	if (( ${#DEPLOY_ARGS[@]} > 0 )); then
		original_args=("${DEPLOY_ARGS[@]}")
	fi
	if (( ${#original_args[@]} > 0 )); then
		DEPLOY_ARGS=(--deploy-only "${original_args[@]}")
	else
		DEPLOY_ARGS=(--deploy-only)
	fi
	run_deploy "hot spare Server 2" \
		"SERVER_HOST=${SPARE_SERVER_HOST:-192.168.50.251}" \
		"SSH_PROXY_JUMP=${SPARE_SSH_PROXY_JUMP:-raspberryserver}" \
		"REGISTRY_HOST_PUSH=${SPARE_REGISTRY_HOST_PUSH:-192.168.50.25:5010}" \
		"REGISTRY_HOST_PULL=${SPARE_REGISTRY_HOST_PULL:-192.168.50.25:5010}" \
		"HOST_PORT=${SPARE_HOST_PORT:-18081}" \
		"COMPOSE_PROJECT_NAME=${SPARE_COMPOSE_PROJECT_NAME:-redlib-rate-test}" \
		"REDLIB_CONTAINER_NAME=${SPARE_CONTAINER_NAME:-redlib-rate-test}" \
		"REDLIB_CACHE_VOLUME=${SPARE_CACHE_VOLUME:-redlib-rate-test-cache}" \
		"REDLIB_STATE_VOLUME=${SPARE_STATE_VOLUME:-redlib-rate-test-state}" \
		"REDLIB_PEER_API_FALLBACK_SOURCES=${SPARE_PEER_API_FALLBACK_SOURCES-}" \
		"REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS=${SPARE_PEER_API_FALLBACK_TIMEOUT_SECS:-5}" \
		"REDLIB_PEER_API_FALLBACK_FORCE_TESTING=${SPARE_PEER_API_FALLBACK_FORCE_TESTING:-off}" \
		"REDLIB_REMEMBERED_FEED_DAEMON_ENABLED=${SPARE_REMEMBERED_FEED_DAEMON_ENABLED:-off}" \
		"REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL=${SPARE_CONTENT_SEMANTIC_OBSERVED_BACKFILL:-off}" \
		"REDLIB_INSTANCE_SCRAPE_ENABLED=${SPARE_INSTANCE_SCRAPE_ENABLED:-off}" \
		"REMOTE_DOCKER_CMD=${SPARE_REMOTE_DOCKER_CMD:-sudo -n docker}"
	if (( ${#original_args[@]} > 0 )); then
		DEPLOY_ARGS=("${original_args[@]}")
	else
		DEPLOY_ARGS=()
	fi
}

case "$TARGET" in
	main)
		deploy_main
		;;
	spare)
		deploy_spare
		;;
	both)
		deploy_main
		deploy_spare
		;;
esac
