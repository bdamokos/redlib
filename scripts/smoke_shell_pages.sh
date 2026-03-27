#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:18081}"

require_match() {
    local body="$1"
    local pattern="$2"
    local label="$3"

    if ! printf '%s' "$body" | rg -q "$pattern"; then
        printf 'smoke failed: %s\npattern: %s\n' "$label" "$pattern" >&2
        exit 1
    fi
}

fetch() {
    curl -fsSL --max-time 45 "$1"
}

printf 'Checking shell routes against %s\n' "$BASE_URL"

front_shell="$(fetch "$BASE_URL/")"
require_match "$front_shell" 'data-fragment-url="/subreddit-fragment"' 'front page shell fragment url'
require_match "$front_shell" 'Loading posts\.\.\.' 'front page shell loading message'

sub_shell="$(fetch "$BASE_URL/r/rust")"
require_match "$sub_shell" 'data-fragment-url="/subreddit-fragment/r/rust"' 'subreddit shell fragment url'

user_shell="$(fetch "$BASE_URL/user/spez")"
require_match "$user_shell" 'data-fragment-url="/user-fragment/spez"' 'user shell fragment url'

sub_fragment="$(fetch "$BASE_URL/subreddit-fragment/r/rust")"
require_match "$sub_fragment" 'id="column_one"' 'subreddit fragment main column'
require_match "$sub_fragment" 'href="/r/rust/' 'subreddit fragment real links'

first_post_path="$(
    printf '%s' "$sub_fragment" \
        | rg -o 'href="/r/rust/comments/[^"]+"' \
        | head -n 1 \
        | sed -E 's/^href="([^"]+)"$/\1/'
)"

if [[ -z "$first_post_path" ]]; then
    printf 'smoke failed: could not extract post link from subreddit fragment\n' >&2
    exit 1
fi

post_shell="$(fetch "$BASE_URL$first_post_path")"
require_match "$post_shell" 'data-fragment-url="/post-fragment/r/rust/comments/' 'post shell fragment url'

post_fragment_path="$(
    printf '%s' "$post_shell" \
        | rg -o 'data-fragment-url="[^"]+"' \
        | head -n 1 \
        | sed -E 's/^data-fragment-url="([^"]+)"$/\1/'
)"

if [[ -z "$post_fragment_path" ]]; then
    printf 'smoke failed: could not extract post fragment path from post shell\n' >&2
    exit 1
fi

post_fragment="$(fetch "$BASE_URL$post_fragment_path")"
require_match "$post_fragment" 'class="post_page"' 'post fragment content'

user_fragment="$(fetch "$BASE_URL/user-fragment/spez")"
require_match "$user_fragment" 'id="column_one"' 'user fragment main column'
require_match "$user_fragment" 'id="user_actions"' 'user fragment sidebar actions'

printf 'Shell smoke checks passed.\n'
