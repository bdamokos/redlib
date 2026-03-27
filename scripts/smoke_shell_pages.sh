#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:18081}"

extract_next_href() {
    python3 -c 'import html,re,sys; body=sys.stdin.read(); m=re.search(r"href=\"([^\"]+)\" accesskey=\"N\"", body, re.S); print(html.unescape("".join(m.group(1).split())) if m else "")'
}

extract_fragment_url() {
    python3 -c 'import html,re,sys; body=sys.stdin.read(); m=re.search(r"data-fragment-url=\"([^\"]+)\"", body); print(html.unescape(m.group(1)) if m else "")'
}

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

front_fragment="$(fetch "$BASE_URL/subreddit-fragment")"
front_next="$(printf '%s' "$front_fragment" | extract_next_href)"
if [[ -z "$front_next" ]]; then
    printf 'smoke failed: could not extract front next page\n' >&2
    exit 1
fi
fetch "$BASE_URL$front_next" >/dev/null

sub_next="$(printf '%s' "$sub_fragment" | extract_next_href)"
if [[ -z "$sub_next" ]]; then
    printf 'smoke failed: could not extract subreddit next page\n' >&2
    exit 1
fi
fetch "$BASE_URL$sub_next" >/dev/null

user_next="$(printf '%s' "$user_fragment" | extract_next_href)"
if [[ -z "$user_next" ]]; then
    printf 'smoke failed: could not extract user next page\n' >&2
    exit 1
fi
fetch "$BASE_URL/user-fragment/spez$user_next" >/dev/null

search_shell="$(fetch "$BASE_URL/search?q=rust")"
require_match "$search_shell" 'data-fragment-url="/search-fragment\?q=rust"' 'search shell fragment url'

search_fragment="$(fetch "$BASE_URL/search-fragment?q=rust")"
require_match "$search_fragment" 'id="column_one"' 'search fragment main column'
search_next="$(printf '%s' "$search_fragment" | extract_next_href)"
if [[ -z "$search_next" ]]; then
    printf 'smoke failed: could not extract search next page\n' >&2
    exit 1
fi
fetch "$BASE_URL/search-fragment$search_next" >/dev/null

duplicates_shell="$(fetch "$BASE_URL/r/rust/duplicates/1s3uoo0/this_week_in_rust_644")"
duplicates_fragment_path="$(printf '%s' "$duplicates_shell" | extract_fragment_url)"
if [[ -z "$duplicates_fragment_path" ]]; then
    printf 'smoke failed: could not extract duplicates fragment path\n' >&2
    exit 1
fi
duplicates_fragment="$(fetch "$BASE_URL$duplicates_fragment_path")"
require_match "$duplicates_fragment" 'id="column_one"' 'duplicates fragment main column'

printf 'Shell smoke checks passed.\n'
