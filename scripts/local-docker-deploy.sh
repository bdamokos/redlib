#!/bin/bash

set -euo pipefail

cd "$(dirname "$0")/.."

git_hash=dev
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
	head_hash=$(git rev-parse HEAD 2>/dev/null || true)
	if [[ -n "$head_hash" ]] &&
		git diff --quiet --ignore-submodules -- &&
		git diff --cached --quiet --ignore-submodules --; then
		git_hash="$head_hash"
	fi
fi

export GIT_HASH="${GIT_HASH:-$git_hash}"
export REDLIB_DEPLOY_GIT_HASH="${REDLIB_DEPLOY_GIT_HASH:-$GIT_HASH}"

docker compose -f compose.dev.yaml up -d --build "$@"
