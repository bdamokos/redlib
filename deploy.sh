#!/bin/bash

set -euo pipefail

cd "$(dirname "$0")"

SERVER_HOST=${SERVER_HOST:-192.168.50.25}
SERVER_USER=${SERVER_USER:-bence}
DEPLOY_PATH=${DEPLOY_PATH:-"~/redlib"}

REGISTRY_HOST_PUSH=${REGISTRY_HOST_PUSH:-192.168.50.25:5010}
REGISTRY_HOST_PULL=${REGISTRY_HOST_PULL:-127.0.0.1:5010}

IMAGE_NAME=${IMAGE_NAME:-redlib}
IMAGE_TAG=${IMAGE_TAG:-latest}
TARGET_PLATFORM=${TARGET_PLATFORM:-linux/arm64}
HOST_PORT=${HOST_PORT:-8081}
LOCAL_REUSE_IMAGE=${LOCAL_REUSE_IMAGE:-redlib:local}
REUSE_LOCAL_IMAGE=${REUSE_LOCAL_IMAGE:-auto}
COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-redlib}
REDLIB_CONTAINER_NAME=${REDLIB_CONTAINER_NAME:-redlib}
REDLIB_CACHE_VOLUME=${REDLIB_CACHE_VOLUME:-redlib-cache}
REDLIB_STATE_VOLUME=${REDLIB_STATE_VOLUME:-redlib-state}
REDLIB_PEER_API_FALLBACK_SOURCES=${REDLIB_PEER_API_FALLBACK_SOURCES-http://192.168.50.251:18081}
REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS=${REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS:-5}
REDLIB_PEER_API_FALLBACK_FORCE_TESTING=${REDLIB_PEER_API_FALLBACK_FORCE_TESTING:-off}
REDLIB_REMEMBERED_FEED_DAEMON_ENABLED=${REDLIB_REMEMBERED_FEED_DAEMON_ENABLED:-on}
REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL=${REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL:-on}
REDLIB_INSTANCE_SCRAPE_ENABLED=${REDLIB_INSTANCE_SCRAPE_ENABLED:-off}

KEYCHAIN_SERVICE=${KEYCHAIN_SERVICE:-redlib-deploy}
KEYCHAIN_ACCOUNT="$SERVER_USER@$SERVER_HOST"
SSH_PROXY_JUMP=${SSH_PROXY_JUMP:-}
SSH_CONTROL_PERSIST=${SSH_CONTROL_PERSIST:-600}
SSH_CONTROL_DIR_BASE=${SSH_CONTROL_DIR_BASE:-/tmp/redlib-ssh}
REMOTE_DOCKER_CMD=${REMOTE_DOCKER_CMD:-docker}
SSH_PASSWORD_AUTH=true

FOLLOW_LOGS=false
DEPLOY_ONLY=false

SSH_COMMON_OPTS=(
	-o StrictHostKeyChecking=no
	-o ConnectTimeout=10
)
if [[ -n "$SSH_PROXY_JUMP" ]]; then
	SSH_COMMON_OPTS+=(-J "$SSH_PROXY_JUMP")
fi
SSH_CONTROL_DIR=""
SSH_CONTROL_PATH=""

require_command() {
	if ! command -v "$1" >/dev/null 2>&1; then
		echo "Error: missing required command: $1" >&2
		exit 1
	fi
}

parse_args() {
	while [[ $# -gt 0 ]]; do
		case "$1" in
			-f|--follow-logs)
				FOLLOW_LOGS=true
				shift
				;;
			-d|--deploy-only)
				DEPLOY_ONLY=true
				shift
				;;
			-h|--help)
				cat <<EOF
Usage: $0 [OPTIONS]

Options:
  -d, --deploy-only    Skip local build and registry push
  -f, --follow-logs    Follow Redlib logs after deployment
  -h, --help           Show this help

Environment overrides:
  SERVER_HOST          Default: $SERVER_HOST
  SERVER_USER          Default: $SERVER_USER
  DEPLOY_PATH          Default: $DEPLOY_PATH
  REGISTRY_HOST_PUSH   Default: $REGISTRY_HOST_PUSH
  REGISTRY_HOST_PULL   Default: $REGISTRY_HOST_PULL
  IMAGE_NAME           Default: $IMAGE_NAME
  IMAGE_TAG            Default: $IMAGE_TAG
  TARGET_PLATFORM      Default: $TARGET_PLATFORM
  HOST_PORT            Default: $HOST_PORT
  COMPOSE_PROJECT_NAME Default: $COMPOSE_PROJECT_NAME
  REDLIB_CONTAINER_NAME Default: $REDLIB_CONTAINER_NAME
  REDLIB_CACHE_VOLUME  Default: $REDLIB_CACHE_VOLUME
  REDLIB_STATE_VOLUME  Default: $REDLIB_STATE_VOLUME
  REDLIB_PEER_API_FALLBACK_SOURCES Default: $REDLIB_PEER_API_FALLBACK_SOURCES
  REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS Default: $REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS
  REDLIB_PEER_API_FALLBACK_FORCE_TESTING Default: $REDLIB_PEER_API_FALLBACK_FORCE_TESTING
  REDLIB_REMEMBERED_FEED_DAEMON_ENABLED Default: $REDLIB_REMEMBERED_FEED_DAEMON_ENABLED
  REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL Default: $REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL
  REDLIB_INSTANCE_SCRAPE_ENABLED Default: $REDLIB_INSTANCE_SCRAPE_ENABLED
  LOCAL_REUSE_IMAGE    Default: $LOCAL_REUSE_IMAGE
  REUSE_LOCAL_IMAGE    Default: $REUSE_LOCAL_IMAGE (auto, never, required)
  KEYCHAIN_SERVICE     Default: $KEYCHAIN_SERVICE
  SSH_PROXY_JUMP       Default: ${SSH_PROXY_JUMP:-none}
  SSH_CONTROL_PERSIST  Default: $SSH_CONTROL_PERSIST
  SSH_CONTROL_DIR_BASE Default: $SSH_CONTROL_DIR_BASE
  REMOTE_DOCKER_CMD    Default: $REMOTE_DOCKER_CMD
EOF
				exit 0
				;;
			*)
				echo "Unknown option: $1" >&2
				exit 1
				;;
		esac
	done
}

test_ssh_connection() {
	sshpass -p "$PASSWORD" ssh \
		"${SSH_COMMON_OPTS[@]}" \
		"$SERVER_USER@$SERVER_HOST" \
		"echo ok" >/dev/null 2>&1
}

test_key_ssh_connection() {
	ssh \
		"${SSH_COMMON_OPTS[@]}" \
		-o BatchMode=yes \
		"$SERVER_USER@$SERVER_HOST" \
		"echo ok" >/dev/null 2>&1
}

prompt_for_password() {
	printf "Enter SSH password for %s: " "$KEYCHAIN_ACCOUNT"
	read -rs PASSWORD
	printf "\n"

	if ! test_ssh_connection; then
		echo "Error: SSH connection failed" >&2
		exit 1
	fi
}

get_password() {
	if test_key_ssh_connection; then
		SSH_PASSWORD_AUTH=false
		PASSWORD=""
		return
	fi

	require_command sshpass

	if command -v security >/dev/null 2>&1; then
		PASSWORD=$(security find-generic-password -a "$KEYCHAIN_ACCOUNT" -s "$KEYCHAIN_SERVICE" -w 2>/dev/null || true)
	else
		PASSWORD=""
	fi

	if [[ -n "${PASSWORD:-}" ]] && test_ssh_connection; then
		return
	fi

	prompt_for_password

	if command -v security >/dev/null 2>&1; then
		printf "Store password in macOS Keychain for future deploys? (y/n): "
		read -r store_password
		if [[ "$store_password" =~ ^[Yy]$ ]]; then
			security add-generic-password -U -a "$KEYCHAIN_ACCOUNT" -s "$KEYCHAIN_SERVICE" -w "$PASSWORD" >/dev/null 2>&1 || true
		fi
	fi
}

cleanup_ssh_multiplexing() {
	if [[ -n "$SSH_CONTROL_PATH" ]]; then
		ssh "${SSH_COMMON_OPTS[@]}" \
			-o ControlPath="$SSH_CONTROL_PATH" \
			-O exit \
			"$SERVER_USER@$SERVER_HOST" >/dev/null 2>&1 || true
	fi

	if [[ -n "$SSH_CONTROL_DIR" ]]; then
		rmdir "$SSH_CONTROL_DIR" >/dev/null 2>&1 || true
	fi
}

start_ssh_multiplexing() {
	SSH_CONTROL_DIR="$SSH_CONTROL_DIR_BASE"
	mkdir -p "$SSH_CONTROL_DIR"
	chmod 700 "$SSH_CONTROL_DIR"
	SSH_CONTROL_PATH="$SSH_CONTROL_DIR/control-%C"
	trap cleanup_ssh_multiplexing EXIT

	echo "Opening SSH control connection..."
	if [[ "$SSH_PASSWORD_AUTH" == true ]]; then
		sshpass -p "$PASSWORD" ssh \
			"${SSH_COMMON_OPTS[@]}" \
			-o ControlMaster=yes \
			-o ControlPersist="$SSH_CONTROL_PERSIST" \
			-o ControlPath="$SSH_CONTROL_PATH" \
			-fN \
			"$SERVER_USER@$SERVER_HOST"
	else
		ssh \
			"${SSH_COMMON_OPTS[@]}" \
			-o ControlMaster=yes \
			-o ControlPersist="$SSH_CONTROL_PERSIST" \
			-o ControlPath="$SSH_CONTROL_PATH" \
			-fN \
			"$SERVER_USER@$SERVER_HOST"
	fi
}

run_ssh() {
	if [[ "$SSH_PASSWORD_AUTH" == true ]]; then
		sshpass -p "$PASSWORD" ssh \
			"${SSH_COMMON_OPTS[@]}" \
			-o ControlMaster=auto \
			-o ControlPersist="$SSH_CONTROL_PERSIST" \
			-o ControlPath="$SSH_CONTROL_PATH" \
			"$SERVER_USER@$SERVER_HOST" \
			"$1"
	else
		ssh \
			"${SSH_COMMON_OPTS[@]}" \
			-o ControlMaster=auto \
			-o ControlPersist="$SSH_CONTROL_PERSIST" \
			-o ControlPath="$SSH_CONTROL_PATH" \
			"$SERVER_USER@$SERVER_HOST" \
			"$1"
	fi
}

run_scp() {
	if [[ "$SSH_PASSWORD_AUTH" == true ]]; then
		sshpass -p "$PASSWORD" scp \
			"${SSH_COMMON_OPTS[@]}" \
			-o ControlMaster=auto \
			-o ControlPersist="$SSH_CONTROL_PERSIST" \
			-o ControlPath="$SSH_CONTROL_PATH" \
			"$@"
	else
		scp \
			"${SSH_COMMON_OPTS[@]}" \
			-o ControlMaster=auto \
			-o ControlPersist="$SSH_CONTROL_PERSIST" \
			-o ControlPath="$SSH_CONTROL_PATH" \
			"$@"
	fi
}

resolve_remote_dir() {
	local remote_home
	remote_home=$(run_ssh 'printf %s "$HOME"')

	if [[ "$DEPLOY_PATH" == "~" ]]; then
		REMOTE_DEPLOY_DIR="$remote_home"
	elif [[ "${DEPLOY_PATH:0:2}" == "~/" ]]; then
		REMOTE_DEPLOY_DIR="$remote_home/${DEPLOY_PATH:2}"
	else
		REMOTE_DEPLOY_DIR="$DEPLOY_PATH"
	fi
}

current_git_hash() {
	git rev-parse HEAD 2>/dev/null || echo dev
}

git_worktree_clean() {
	git rev-parse --is-inside-work-tree >/dev/null 2>&1 &&
		git diff --quiet --ignore-submodules -- &&
		git diff --cached --quiet --ignore-submodules --
}

docker_image_field() {
	local image="$1"
	local format="$2"
	local value
	value=$(docker image inspect "$image" --format "$format" 2>/dev/null || true)
	if [[ "$value" == "<no value>" ]]; then
		value=""
	fi
	printf '%s' "$value"
}

docker_image_label() {
	local image="$1"
	local label="$2"
	docker_image_field "$image" "{{ index .Config.Labels \"$label\" }}"
}

reuse_local_image_mode() {
	local mode
	mode=$(printf '%s' "$REUSE_LOCAL_IMAGE" | tr '[:upper:]' '[:lower:]')
	case "$mode" in
		auto|yes|true|1|on)
			printf '%s' auto
			;;
		required|require|must)
			printf '%s' required
			;;
		never|no|false|0|off)
			printf '%s' never
			;;
		*)
			echo "Error: REUSE_LOCAL_IMAGE must be auto, never, or required; got '$REUSE_LOCAL_IMAGE'." >&2
			exit 1
			;;
	esac
}

local_image_reuse_blocker() {
	local expected_hash="$1"
	local target_os="${TARGET_PLATFORM%%/*}"
	local target_arch_part="${TARGET_PLATFORM#*/}"
	local target_arch="${target_arch_part%%/*}"
	local image_os
	local image_arch
	local revision

	if ! docker image inspect "$LOCAL_REUSE_IMAGE" >/dev/null 2>&1; then
		echo "$LOCAL_REUSE_IMAGE was not found"
		return
	fi

	if ! git_worktree_clean; then
		echo "git worktree has uncommitted changes"
		return
	fi

	revision=$(docker_image_label "$LOCAL_REUSE_IMAGE" "org.opencontainers.image.revision")
	if [[ -z "$revision" ]]; then
		echo "$LOCAL_REUSE_IMAGE has no org.opencontainers.image.revision label"
		return
	fi
	if [[ "$revision" != "$expected_hash" ]]; then
		echo "$LOCAL_REUSE_IMAGE revision label is $revision, expected $expected_hash"
		return
	fi

	image_os=$(docker_image_field "$LOCAL_REUSE_IMAGE" '{{ .Os }}')
	image_arch=$(docker_image_field "$LOCAL_REUSE_IMAGE" '{{ .Architecture }}')
	if [[ "$image_os" != "$target_os" || "$image_arch" != "$target_arch" ]]; then
		echo "$LOCAL_REUSE_IMAGE platform is $image_os/$image_arch, expected $target_os/$target_arch"
		return
	fi
}

push_reusable_local_image() {
	local expected_hash="$1"
	local image_ref_push="$2"
	local mode
	local blocker

	mode=$(reuse_local_image_mode) || exit 1
	if [[ "$mode" == "never" ]]; then
		echo "Local image reuse disabled by REUSE_LOCAL_IMAGE=never."
		return 1
	fi

	blocker=$(local_image_reuse_blocker "$expected_hash")
	if [[ -n "$blocker" ]]; then
		if [[ "$mode" == "required" ]]; then
			echo "Error: cannot reuse $LOCAL_REUSE_IMAGE: $blocker." >&2
			exit 1
		fi
		echo "Not reusing $LOCAL_REUSE_IMAGE: $blocker."
		return 1
	fi

	echo "Reusing local test image $LOCAL_REUSE_IMAGE ($expected_hash, $TARGET_PLATFORM)."
	echo "Tagging $IMAGE_NAME:$IMAGE_TAG..."
	docker tag "$LOCAL_REUSE_IMAGE" "$IMAGE_NAME:$IMAGE_TAG" || exit 1

	echo "Tagging $image_ref_push..."
	docker tag "$LOCAL_REUSE_IMAGE" "$image_ref_push" || exit 1

	echo "Pushing $image_ref_push..."
	docker push "$image_ref_push" || exit 1
}

build_and_push() {
	local image_ref_push="$REGISTRY_HOST_PUSH/$IMAGE_NAME:$IMAGE_TAG"
	local deploy_git_hash

	deploy_git_hash=$(current_git_hash)

	if push_reusable_local_image "$deploy_git_hash" "$image_ref_push"; then
		return
	fi

	echo "Building $IMAGE_NAME for $TARGET_PLATFORM..."
	docker buildx build \
		--platform "$TARGET_PLATFORM" \
		--build-arg "GIT_HASH=$deploy_git_hash" \
		--load \
		-t "$IMAGE_NAME:$IMAGE_TAG" \
		.

	echo "Tagging $image_ref_push..."
	docker tag "$IMAGE_NAME:$IMAGE_TAG" "$image_ref_push"

	echo "Pushing $image_ref_push..."
	docker push "$image_ref_push"
}

cleanup_local_target() {
	if [[ -d target ]]; then
		echo "Removing local Cargo target directory..."
		rm -rf target
	fi
}

deploy_remote() {
	local remote_image_ref="$REGISTRY_HOST_PULL/$IMAGE_NAME:$IMAGE_TAG"
	local deploy_git_hash
	deploy_git_hash=$(current_git_hash)

	echo "Resolving remote deploy directory..."
	resolve_remote_dir
	echo "Remote deploy directory: $REMOTE_DEPLOY_DIR"

	echo "Inspecting existing remote redlib checkout..."
	run_ssh "
		set -e
		mkdir -p \"$REMOTE_DEPLOY_DIR\"
		cd \"$REMOTE_DEPLOY_DIR\"
		if [ -d .git ]; then
			echo \"Remote git branch: \$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)\"
			echo \"Remote git status:\"
			git status --short || true
		else
			echo \"Remote deploy directory is not a git checkout.\"
		fi
	"

	echo "Copying compose.pi.yaml to server..."
	run_scp compose.pi.yaml "$SERVER_USER@$SERVER_HOST:$REMOTE_DEPLOY_DIR/compose.pi.yaml"

	if [[ -f .env ]]; then
		echo "Copying .env to server..."
		run_scp .env "$SERVER_USER@$SERVER_HOST:$REMOTE_DEPLOY_DIR/.env"
	else
		echo "Local .env not found; preserving remote .env except REDLIB_DEPLOY_GIT_HASH."
	fi

	echo "Updating Redlib on the Raspberry Pi..."
	run_ssh "
		set -e
		cd \"$REMOTE_DEPLOY_DIR\"

		if [ ! -f .env ]; then
			touch .env
		fi

		echo \"Recording REDLIB_DEPLOY_GIT_HASH in remote .env ($deploy_git_hash)...\"
		tmp_env=\$(mktemp)
		awk -v hash=\"$deploy_git_hash\" '
			BEGIN { found = 0 }
			/^REDLIB_DEPLOY_GIT_HASH=/ {
				print \"REDLIB_DEPLOY_GIT_HASH=\" hash
				found = 1
				next
			}
			{ print }
			END {
				if (!found) {
					print \"REDLIB_DEPLOY_GIT_HASH=\" hash
				}
			}
		' .env > \"\$tmp_env\"
		mv \"\$tmp_env\" .env

			REMOTE_DOCKER_CMD=\"$REMOTE_DOCKER_CMD\"

			docker_cmd() {
				\$REMOTE_DOCKER_CMD \"\$@\"
			}

			compose_backend() {
				if docker_cmd compose version >/dev/null 2>&1; then
					echo docker-plugin
				elif command -v docker-compose >/dev/null 2>&1; then
					echo docker-compose
				else
					echo plain-docker
				fi
			}

			compose_cmd() {
				case \"\$COMPOSE_BACKEND\" in
					docker-plugin)
						docker_cmd compose \"\$@\"
						;;
					docker-compose)
						docker-compose \"\$@\"
						;;
					*)
						echo \"docker compose is unavailable on this host.\" >&2
						return 127
						;;
				esac
			}

			plain_docker_recreate() {
				echo \"Pulling \$REDLIB_IMAGE with \$REMOTE_DOCKER_CMD...\"
				docker_cmd pull \"\$REDLIB_IMAGE\"

				echo \"Recreating \$REDLIB_CONTAINER_NAME on port \$REDLIB_HOST_PORT with plain Docker...\"
				docker_cmd rm -f \"\$REDLIB_CONTAINER_NAME\" >/dev/null 2>&1 || true
				docker_cmd volume create \"\$REDLIB_CACHE_VOLUME\" >/dev/null
				docker_cmd volume create \"\$REDLIB_STATE_VOLUME\" >/dev/null
				docker_cmd run -d \\
					--name \"\$REDLIB_CONTAINER_NAME\" \\
					--restart always \\
					--user nobody \\
					--read-only \\
					--security-opt no-new-privileges:true \\
					--cap-drop ALL \\
					--add-host host.docker.internal:host-gateway \\
					-p \"\$REDLIB_HOST_PORT:8080\" \\
					--env-file .env \\
					-e REDLIB_DEPLOY_GIT_HASH=\"\$REDLIB_DEPLOY_GIT_HASH\" \\
					-e REDLIB_VIDEO_PREVIEW_ENABLED=off \\
					-e REDLIB_VIDEO_PREVIEW_MAX_CONCURRENT=1 \\
					-e REDLIB_VIDEO_PREVIEW_ANIMATION_FPS=\"\${REDLIB_VIDEO_PREVIEW_ANIMATION_FPS:-15}\" \\
					-e REDLIB_VIDEO_PREVIEW_ANIMATION_WEBP_QUALITY=\"\${REDLIB_VIDEO_PREVIEW_ANIMATION_WEBP_QUALITY:-75}\" \\
					-e REDLIB_VIDEO_PREVIEW_ANIMATION_WEBP_COMPRESSION_LEVEL=\"\${REDLIB_VIDEO_PREVIEW_ANIMATION_WEBP_COMPRESSION_LEVEL:-0}\" \\
					-e REDLIB_VIDEO_PREVIEW_ANIMATION_MAX_WIDTH=\"\${REDLIB_VIDEO_PREVIEW_ANIMATION_MAX_WIDTH:-360}\" \\
					-e REDLIB_CACHE_DIR=\"\${REDLIB_CACHE_DIR:-/var/lib/redlib/cache}\" \\
					-e REDLIB_PREFS_DB=\"\${REDLIB_PREFS_DB:-/var/lib/redlib/state/preferences.sqlite3}\" \\
					-e REDLIB_PEER_API_FALLBACK_SOURCES=\"\$REDLIB_PEER_API_FALLBACK_SOURCES\" \\
					-e REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS=\"\$REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS\" \\
					-e REDLIB_PEER_API_FALLBACK_FORCE_TESTING=\"\$REDLIB_PEER_API_FALLBACK_FORCE_TESTING\" \\
					-e REDLIB_REMEMBERED_FEED_DAEMON_ENABLED=\"\$REDLIB_REMEMBERED_FEED_DAEMON_ENABLED\" \\
					-e REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL=\"\$REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL\" \\
					-e REDLIB_INSTANCE_SCRAPE_ENABLED=\"\$REDLIB_INSTANCE_SCRAPE_ENABLED\" \\
					-e OLLAMA_BASE_URL=\"\${OLLAMA_BASE_URL:-http://host.docker.internal:11434}\" \\
					-e REDLIB_SUBREDDIT_SEMANTIC_MODEL=\"\${REDLIB_SUBREDDIT_SEMANTIC_MODEL:-nomic-embed-text}\" \\
					-v \"\$REDLIB_CACHE_VOLUME:/var/lib/redlib/cache\" \\
					-v \"\$REDLIB_STATE_VOLUME:/var/lib/redlib/state\" \\
					--health-cmd 'wget --spider -q --tries=1 http://localhost:8080/settings' \\
					--health-interval 5m \\
					--health-timeout 3s \\
					\"\$REDLIB_IMAGE\"
			}

		export REDLIB_IMAGE=\"$remote_image_ref\"
		export REDLIB_DEPLOY_GIT_HASH=\"$deploy_git_hash\"
		export REDLIB_HOST_PORT=\"$HOST_PORT\"
		export REDLIB_CONTAINER_NAME=\"$REDLIB_CONTAINER_NAME\"
		export REDLIB_CACHE_VOLUME=\"$REDLIB_CACHE_VOLUME\"
		export REDLIB_STATE_VOLUME=\"$REDLIB_STATE_VOLUME\"
		export REDLIB_PEER_API_FALLBACK_SOURCES=\"$REDLIB_PEER_API_FALLBACK_SOURCES\"
		export REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS=\"$REDLIB_PEER_API_FALLBACK_TIMEOUT_SECS\"
		export REDLIB_PEER_API_FALLBACK_FORCE_TESTING=\"$REDLIB_PEER_API_FALLBACK_FORCE_TESTING\"
		export REDLIB_REMEMBERED_FEED_DAEMON_ENABLED=\"$REDLIB_REMEMBERED_FEED_DAEMON_ENABLED\"
		export REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL=\"$REDLIB_CONTENT_SEMANTIC_OBSERVED_BACKFILL\"
			export REDLIB_INSTANCE_SCRAPE_ENABLED=\"$REDLIB_INSTANCE_SCRAPE_ENABLED\"
			COMPOSE_BACKEND=\$(compose_backend)
			echo \"Remote container backend: \$COMPOSE_BACKEND\"

			if [ \"\$COMPOSE_BACKEND\" = plain-docker ]; then
				plain_docker_recreate
			else
				echo \"Pulling \$REDLIB_IMAGE...\"
				compose_cmd -f compose.pi.yaml -p \"$COMPOSE_PROJECT_NAME\" pull redlib

				echo \"Recreating redlib on port \$REDLIB_HOST_PORT...\"
				compose_cmd -f compose.pi.yaml -p \"$COMPOSE_PROJECT_NAME\" up -d --force-recreate --remove-orphans redlib
			fi

			echo \"Container status:\"
			if [ \"\$COMPOSE_BACKEND\" = plain-docker ]; then
				docker_cmd ps --filter \"name=^\${REDLIB_CONTAINER_NAME}\$\" --format \"table {{.Names}}\\t{{.Image}}\\t{{.Status}}\\t{{.Ports}}\"
			else
				compose_cmd -f compose.pi.yaml -p \"$COMPOSE_PROJECT_NAME\" ps
			fi

			echo \"Recent image list:\"
			docker_cmd images --format \"table {{.Repository}}\\t{{.Tag}}\\t{{.ID}}\\t{{.CreatedSince}}\" | grep -i \"^.*redlib\" || true
		"
	}

follow_logs() {
	run_ssh "
		set -e
		cd \"$REMOTE_DEPLOY_DIR\"

			REMOTE_DOCKER_CMD=\"$REMOTE_DOCKER_CMD\"

			docker_cmd() {
				\$REMOTE_DOCKER_CMD \"\$@\"
			}

			compose_cmd() {
				if docker_cmd compose version >/dev/null 2>&1; then
					docker_cmd compose \"\$@\"
				elif command -v docker-compose >/dev/null 2>&1; then
					docker-compose \"\$@\"
				else
					docker_cmd logs -f \"$REDLIB_CONTAINER_NAME\"
					return
				fi
			}
		}

		export REDLIB_IMAGE=\"$REGISTRY_HOST_PULL/$IMAGE_NAME:$IMAGE_TAG\"
		export REDLIB_HOST_PORT=\"$HOST_PORT\"
		export REDLIB_CONTAINER_NAME=\"$REDLIB_CONTAINER_NAME\"

		compose_cmd -f compose.pi.yaml -p \"$COMPOSE_PROJECT_NAME\" logs -f redlib
	"
}

parse_args "$@"

require_command docker

echo "Server: $SERVER_USER@$SERVER_HOST"
echo "Remote path: $DEPLOY_PATH"
echo "Registry push host: $REGISTRY_HOST_PUSH"
echo "Registry pull host: $REGISTRY_HOST_PULL"
echo "Image: $IMAGE_NAME:$IMAGE_TAG"
echo "Local reuse image: $LOCAL_REUSE_IMAGE"
echo "Local image reuse mode: $REUSE_LOCAL_IMAGE"
echo "Host port: $HOST_PORT"
echo "Compose project: $COMPOSE_PROJECT_NAME"
echo "Container name: $REDLIB_CONTAINER_NAME"
echo "Remote Docker command: $REMOTE_DOCKER_CMD"
if [[ -n "$SSH_PROXY_JUMP" ]]; then
	echo "SSH proxy jump: $SSH_PROXY_JUMP"
fi
echo

get_password
start_ssh_multiplexing

if [[ "$DEPLOY_ONLY" == false ]]; then
	build_and_push
	cleanup_local_target
else
	echo "Skipping build and push."
fi

deploy_remote

if [[ "$FOLLOW_LOGS" == true ]]; then
	follow_logs
fi
