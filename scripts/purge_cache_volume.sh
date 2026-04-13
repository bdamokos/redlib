#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${1:-compose.dev.yaml}"
VOLUME_NAME="redlib-cache"

cd "$ROOT_DIR"

docker compose -f "$COMPOSE_FILE" stop redlib
docker run --rm -v "${VOLUME_NAME}:/cache" alpine:3.22 sh -c 'rm -rf /cache/* /cache/.[!.]* /cache/..?*'
docker compose -f "$COMPOSE_FILE" up -d redlib
