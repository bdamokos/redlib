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

build_and_push() {
	local image_ref_push="$REGISTRY_HOST_PUSH/$IMAGE_NAME:$IMAGE_TAG"

	echo "Building $IMAGE_NAME for $TARGET_PLATFORM..."
	docker buildx build \
		--platform "$TARGET_PLATFORM" \
		--load \
		-t "$IMAGE_NAME:$IMAGE_TAG" \
		.

	echo "Tagging $image_ref_push..."
	docker tag "$IMAGE_NAME:$IMAGE_TAG" "$image_ref_push"

	echo "Pushing $image_ref_push..."
	docker push "$image_ref_push"
}

deploy_remote() {
	local remote_image_ref="$REGISTRY_HOST_PULL/$IMAGE_NAME:$IMAGE_TAG"

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
echo "Host port: $HOST_PORT"
echo

get_password
start_ssh_multiplexing

if [[ "$DEPLOY_ONLY" == false ]]; then
	build_and_push
else
	echo "Skipping build and push."
fi

deploy_remote

if [[ "$FOLLOW_LOGS" == true ]]; then
	follow_logs
fi
