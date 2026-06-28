#!/bin/bash

set -euo pipefail

cd "$(dirname "$0")"

SERVER_HOST=${SERVER_HOST:-100.119.233.126}
SERVER_USER=${SERVER_USER:-bence}
DEPLOY_PATH=${DEPLOY_PATH:-"~/redlib"}

REGISTRY_HOST_PUSH=${REGISTRY_HOST_PUSH:-100.119.233.126:5010}
REGISTRY_HOST_PULL=${REGISTRY_HOST_PULL:-127.0.0.1:5010}

IMAGE_NAME=${IMAGE_NAME:-redlib}
IMAGE_TAG=${IMAGE_TAG:-latest}
TARGET_PLATFORM=${TARGET_PLATFORM:-linux/arm64}
HOST_PORT=${HOST_PORT:-8081}
LOCAL_REUSE_IMAGE=${LOCAL_REUSE_IMAGE:-redlib:local}
REUSE_LOCAL_IMAGE=${REUSE_LOCAL_IMAGE:-auto}

KEYCHAIN_SERVICE=${KEYCHAIN_SERVICE:-redlib-deploy}
KEYCHAIN_ACCOUNT="$SERVER_USER@$SERVER_HOST"
SSH_CONTROL_PERSIST=${SSH_CONTROL_PERSIST:-600}
SSH_CONTROL_DIR_BASE=${SSH_CONTROL_DIR_BASE:-/tmp/redlib-ssh}

FOLLOW_LOGS=false
DEPLOY_ONLY=false

SSH_COMMON_OPTS=(
	-o StrictHostKeyChecking=no
	-o ConnectTimeout=10
)
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
  LOCAL_REUSE_IMAGE    Default: $LOCAL_REUSE_IMAGE
  REUSE_LOCAL_IMAGE    Default: $REUSE_LOCAL_IMAGE (auto, never, required)
  KEYCHAIN_SERVICE     Default: $KEYCHAIN_SERVICE
  SSH_CONTROL_PERSIST  Default: $SSH_CONTROL_PERSIST
  SSH_CONTROL_DIR_BASE Default: $SSH_CONTROL_DIR_BASE
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

# Keep REDLIB_DEPLOY_GIT_HASH in .env in sync with the commit being deployed (upsert one line).
upsert_redlib_deploy_git_hash_in_env() {
	local env_file="$1"
	local hash="$2"
	local tmp
	tmp="$(mktemp)"
	local found=false
	while IFS= read -r line || [[ -n "$line" ]]; do
		if [[ "$line" == REDLIB_DEPLOY_GIT_HASH=* ]]; then
			printf '%s\n' "REDLIB_DEPLOY_GIT_HASH=$hash" >> "$tmp"
			found=true
		else
			printf '%s\n' "$line" >> "$tmp"
		fi
	done < "$env_file"
	if [[ "$found" == false ]]; then
		printf '%s\n' "REDLIB_DEPLOY_GIT_HASH=$hash" >> "$tmp"
	fi
	mv "$tmp" "$env_file"
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
	sshpass -p "$PASSWORD" ssh \
		"${SSH_COMMON_OPTS[@]}" \
		-o ControlMaster=yes \
		-o ControlPersist="$SSH_CONTROL_PERSIST" \
		-o ControlPath="$SSH_CONTROL_PATH" \
		-fN \
		"$SERVER_USER@$SERVER_HOST"
}

run_ssh() {
	sshpass -p "$PASSWORD" ssh \
		"${SSH_COMMON_OPTS[@]}" \
		-o ControlMaster=auto \
		-o ControlPersist="$SSH_CONTROL_PERSIST" \
		-o ControlPath="$SSH_CONTROL_PATH" \
		"$SERVER_USER@$SERVER_HOST" \
		"$1"
}

run_scp() {
	sshpass -p "$PASSWORD" scp \
		"${SSH_COMMON_OPTS[@]}" \
		-o ControlMaster=auto \
		-o ControlPersist="$SSH_CONTROL_PERSIST" \
		-o ControlPath="$SSH_CONTROL_PATH" \
		"$@"
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
		deploy_git_hash="$(git rev-parse HEAD 2>/dev/null || true)"
		if [[ -n "$deploy_git_hash" ]]; then
			echo "Recording REDLIB_DEPLOY_GIT_HASH in .env ($deploy_git_hash)..."
			upsert_redlib_deploy_git_hash_in_env .env "$deploy_git_hash"
		else
			echo "Warning: could not read git HEAD; .env left unchanged for REDLIB_DEPLOY_GIT_HASH." >&2
		fi
	fi

	if [[ -f .env ]]; then
		echo "Copying .env to server..."
		run_scp .env "$SERVER_USER@$SERVER_HOST:$REMOTE_DEPLOY_DIR/.env"
	else
		echo "Local .env not found; leaving remote .env unchanged."
	fi

	echo "Updating Redlib on the Raspberry Pi..."
	run_ssh "
		set -e
		cd \"$REMOTE_DEPLOY_DIR\"

		if [ ! -f .env ]; then
			touch .env
		fi

		compose_cmd() {
			if docker compose version >/dev/null 2>&1; then
				docker compose \"\$@\"
			else
				docker-compose \"\$@\"
			fi
		}

		export REDLIB_IMAGE=\"$remote_image_ref\"
		export REDLIB_HOST_PORT=\"$HOST_PORT\"

		echo \"Pulling \$REDLIB_IMAGE...\"
		compose_cmd -f compose.pi.yaml -p redlib pull redlib

		echo \"Recreating redlib on port \$REDLIB_HOST_PORT...\"
		compose_cmd -f compose.pi.yaml -p redlib up -d --force-recreate --remove-orphans redlib

		echo \"Container status:\"
		compose_cmd -f compose.pi.yaml -p redlib ps

		echo \"Recent image list:\"
		docker images --format \"table {{.Repository}}\\t{{.Tag}}\\t{{.ID}}\\t{{.CreatedSince}}\" | grep -i \"^.*redlib\" || true
	"
}

follow_logs() {
	run_ssh "
		set -e
		cd \"$REMOTE_DEPLOY_DIR\"

		compose_cmd() {
			if docker compose version >/dev/null 2>&1; then
				docker compose \"\$@\"
			else
				docker-compose \"\$@\"
			fi
		}

		export REDLIB_IMAGE=\"$REGISTRY_HOST_PULL/$IMAGE_NAME:$IMAGE_TAG\"
		export REDLIB_HOST_PORT=\"$HOST_PORT\"

		compose_cmd -f compose.pi.yaml -p redlib logs -f redlib
	"
}

parse_args "$@"

require_command docker
require_command sshpass

echo "Server: $SERVER_USER@$SERVER_HOST"
echo "Remote path: $DEPLOY_PATH"
echo "Registry push host: $REGISTRY_HOST_PUSH"
echo "Registry pull host: $REGISTRY_HOST_PULL"
echo "Image: $IMAGE_NAME:$IMAGE_TAG"
echo "Local reuse image: $LOCAL_REUSE_IMAGE"
echo "Local image reuse mode: $REUSE_LOCAL_IMAGE"
echo "Host port: $HOST_PORT"
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
