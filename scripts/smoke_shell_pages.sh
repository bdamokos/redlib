#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:18081}"

BASE_URL="$BASE_URL" python3 - <<'PY'
import html
import os
import re
import sys
import time
import urllib.error
import urllib.request


BASE_URL = os.environ["BASE_URL"].rstrip("/")


def fail(message: str) -> None:
    print(f"smoke failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def fetch(path: str, *, cookie: str = "", timeout: int = 45) -> str:
    request = urllib.request.Request(
        f"{BASE_URL}{path}",
        headers={"Cookie": cookie} if cookie else {},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.read().decode("utf-8", "replace")
    except urllib.error.URLError as error:
        fail(f"{path} -> {error}")


def require(pattern: str, body: str, label: str) -> None:
    if not re.search(pattern, body, re.S):
        fail(f"{label} (pattern: {pattern})")


def extract(pattern: str, body: str, label: str) -> str:
    match = re.search(pattern, body, re.S)
    if not match:
        fail(label)
    return html.unescape(match.group(1))


def extract_next(body: str, label: str) -> str:
    return extract(r'href="([^"]+)" accesskey="N"', body, label)


def heavy_subscription_cookie() -> str:
    subscriptions = ["rust"] + [f"subreddit{i}" for i in range(1200)]
    parts = ["front_page=default", "post_sort=new"]
    current = []
    current_length = 0
    chunks = []

    for subreddit in subscriptions:
        addition = len(subreddit) + (1 if current else 0)
        if current and current_length + addition > 3800:
            chunks.append("+".join(current))
            current = [subreddit]
            current_length = len(subreddit)
        else:
            current.append(subreddit)
            current_length += addition

    if current:
        chunks.append("+".join(current))

    for index, chunk in enumerate(chunks):
        name = "subscriptions" if index == 0 else f"subscriptions{index}"
        parts.append(f"{name}={chunk}")

    return "; ".join(parts)


print(f"Checking shell routes against {BASE_URL}")

front_shell = fetch("/")
require(r'data-fragment-url="/subreddit-fragment"', front_shell, "front shell fragment url")

sub_shell = fetch("/r/rust")
require(r'data-fragment-url="/subreddit-fragment/r/rust"', sub_shell, "subreddit shell fragment url")

user_shell = fetch("/user/spez")
require(r'data-fragment-url="/user-fragment/spez"', user_shell, "user shell fragment url")

sub_fragment = fetch("/subreddit-fragment/r/rust")
require(r'id="column_one"', sub_fragment, "subreddit fragment main column")
first_post_path = extract(r'href="(/r/rust/comments/[^"]+)"', sub_fragment, "subreddit fragment post link")

post_shell = fetch(first_post_path)
require(r'data-fragment-url="/post-fragment/r/rust/comments/', post_shell, "post shell fragment url")
post_fragment_path = extract(r'data-fragment-url="([^"]+)"', post_shell, "post fragment path")
post_fragment = fetch(post_fragment_path)
require(r'class="post_page"', post_fragment, "post fragment content")

user_fragment = fetch("/user-fragment/spez")
require(r'id="posts"', user_fragment, "user fragment posts")
require(r'data-subscription-form', user_fragment, "user fragment inline follow forms")

front_fragment = fetch("/subreddit-fragment")
front_next = extract_next(front_fragment, "front next page")
fetch(front_next)

sub_next = extract_next(sub_fragment, "subreddit next page")
fetch(sub_next)

user_next = extract_next(user_fragment, "user next page")
fetch(f"/user-fragment/spez{user_next}")

search_shell = fetch("/search?q=rust")
require(r'data-fragment-url="/search-fragment\?q=rust"', search_shell, "search shell fragment url")
search_fragment = fetch("/search-fragment?q=rust")
search_next = extract_next(search_fragment, "search next page")
fetch(f"/search-fragment{search_next}")

duplicates_shell = fetch("/r/rust/duplicates/1s3uoo0/this_week_in_rust_644")
duplicates_fragment_path = extract(r'data-fragment-url="([^"]+)"', duplicates_shell, "duplicates fragment path")
duplicates_fragment = fetch(duplicates_fragment_path)
require(r'id="column_one"', duplicates_fragment, "duplicates fragment content")

wiki_shell = fetch("/r/rust/wiki/install")
wiki_fragment_path = extract(r'data-fragment-url="([^"]+)"', wiki_shell, "wiki fragment path")
wiki_fragment = fetch(wiki_fragment_path)
require(r'id="column_one"', wiki_fragment, "wiki fragment content")

sidebar_shell = fetch("/r/rust/about/sidebar")
sidebar_fragment_path = extract(r'data-fragment-url="([^"]+)"', sidebar_shell, "sidebar fragment path")
sidebar_fragment = fetch(sidebar_fragment_path)
require(r'id="column_one"', sidebar_fragment, "sidebar fragment content")

heavy_cookie = heavy_subscription_cookie()
heavy_front_shell = fetch("/new", cookie=heavy_cookie, timeout=20)
require(
    r'Loading posts from the first 32 feeds\. More feeds will warm in the background\.',
    heavy_front_shell,
    "heavy front shell warmup banner",
)

heavy_front_fragment = fetch("/subreddit-fragment/new", cookie=heavy_cookie, timeout=20)
require(r'data-feed-session-id="', heavy_front_fragment, "heavy front fragment session id")
require(r'id="feed_content"', heavy_front_fragment, "heavy front fragment feed content")
require(r'accesskey="N"', heavy_front_fragment, "heavy front next page link")

status_path = extract(r'data-feed-status-url="([^"]+)"', heavy_front_fragment, "heavy feed status url")
status_body = fetch(status_path, cookie=heavy_cookie, timeout=20)
require(r'"warming":(true|false)', status_body, "heavy front status warming field")
require(r'"ready_sources":[0-9]+', status_body, "heavy front status ready source count")

reloaded_fragment = fetch("/subreddit-fragment/new", cookie=heavy_cookie, timeout=20)
require(r'data-feed-session-id="', reloaded_fragment, "heavy front reload session id")
require(r'id="feed_content"', reloaded_fragment, "heavy front reload feed content")

feed_session_id = extract(r'data-feed-session-id="([^"]+)"', heavy_front_fragment, "heavy feed session id")
next_page_fragment = fetch(
    f"/feed-fragment?feed_session={feed_session_id}&cache_page=1&current_url=%2Fnew",
    cookie=heavy_cookie,
    timeout=20,
)
require(r'id="posts"', next_page_fragment, "heavy front next page posts")
require(r'post_comments', next_page_fragment, "heavy front next page content")

heavy_sub_fragment = fetch("/subreddit-fragment/r/rust", cookie=heavy_cookie, timeout=20)
require(r'data-subscription-state="subscribed"', heavy_sub_fragment, "heavy subscribed subreddit state")
require(r'>Unsubscribe<', heavy_sub_fragment, "heavy subscribed subreddit label")

print("Shell smoke checks passed.")
PY
