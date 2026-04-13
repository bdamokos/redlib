#!/usr/bin/env python3

import argparse
import html
import json
import re
import sys
import urllib.error
import urllib.request
from html.parser import HTMLParser
from xml.etree import ElementTree


TITLE_RE = re.compile(r"<title>(.*?)</title>", re.IGNORECASE | re.DOTALL)


def collapse_ws(value: str) -> str:
    return " ".join(html.unescape(value).split())


def match_text(pattern: str, document: str) -> str:
    match = re.search(pattern, document, re.IGNORECASE | re.DOTALL)
    return collapse_ws(match.group(1)) if match else ""


def match_attr(pattern: str, document: str) -> str:
    match = re.search(pattern, document, re.IGNORECASE | re.DOTALL)
    return html.unescape(match.group(1)) if match else ""


class ListingParser(HTMLParser):
    def __init__(self, limit: int):
        super().__init__()
        self.limit = limit
        self.posts = []
        self.current = None
        self.capture = None
        self.in_title = False

    def handle_starttag(self, tag, attrs):
        attr_map = dict(attrs)
        cls = attr_map.get("class", "")
        classes = set(cls.split())

        if tag == "div" and "post" in classes and attr_map.get("id") and len(self.posts) < self.limit:
            self.current = {
                "id": attr_map["id"],
                "created": attr_map.get("data-created-ts", ""),
                "kind": attr_map.get("data-post-type", ""),
            }
            self.posts.append(self.current)

        if not self.current:
            return

        if tag == "a" and cls == "post_subreddit":
            self.capture = "community"
        elif tag == "a" and cls.startswith("post_author"):
            self.capture = "author"
        elif tag == "span" and cls == "created":
            self.current["created_label"] = attr_map.get("title", "")
            self.capture = "rel_time"
        elif tag == "div" and cls == "post_score":
            self.current["score_label"] = attr_map.get("title", "")
            self.capture = "score"
        elif tag == "a" and cls == "post_comments":
            self.current["permalink"] = attr_map.get("href", "")
            self.capture = "comments"
        elif tag in {"h1", "h2"} and cls == "post_title":
            self.in_title = True
        elif tag == "a" and self.in_title and "href" in attr_map:
            self.current["title_url"] = attr_map["href"]
            self.capture = "title"

    def handle_endtag(self, tag):
        if tag in {"h1", "h2"}:
            self.in_title = False
        if tag in {"a", "span", "div"}:
            self.capture = None
        if tag == "div" and self.current:
            self.current = None

    def handle_data(self, data):
        if not self.current or not self.capture:
            return
        text = collapse_ws(data)
        if not text:
            return
        current = self.current.get(self.capture, "")
        self.current[self.capture] = f"{current} {text}".strip()


def parse_subreddit(document: str) -> dict:
    details = re.search(
        r'<div id="sub_details">\s*<label>Members</label>\s*<label>Active</label>\s*'
        r'<div title="(.*?)">(.*?)</div>\s*<div title="(.*?)">(.*?)</div>',
        document,
        re.IGNORECASE | re.DOTALL,
    )

    return {
        "title": match_text(r'<h1 id="sub_title">(.*?)</h1>', document),
        "name": match_text(r'<p id="sub_name">(.*?)</p>', document),
        "description": match_text(r'<p id="sub_description">(.*?)</p>', document),
        "icon": match_attr(r'id="sub_icon"\s+src="(.*?)"', document),
        "wiki": bool(re.search(r'<a href="/r/[^"]+/wiki/index">Wiki</a>', document, re.IGNORECASE)),
        "members": {
            "raw": collapse_ws(details.group(1)) if details else "",
            "display": collapse_ws(details.group(2)) if details else "",
        },
        "active": {
            "raw": collapse_ws(details.group(3)) if details else "",
            "display": collapse_ws(details.group(4)) if details else "",
        },
        "sidebar_html_present": 'id="sidebar_contents"' in document,
    }


def parse_user(document: str) -> dict:
    details = re.search(
        r'<div id="user_details">\s*<label>(.*?)</label>\s*<label>(.*?)</label>\s*'
        r'<div>(.*?)</div>\s*<div>(.*?)</div>',
        document,
        re.IGNORECASE | re.DOTALL,
    )

    return {
        "title": match_text(r'<h1 id="user_title">(.*?)</h1>', document),
        "name": match_text(r'<p id="user_name">(.*?)</p>', document),
        "description": match_text(r'<div id="user_description">(.*?)</div>', document),
        "icon": match_attr(r'id="user_icon"\s+src="(.*?)"', document),
        "metric_1": {
            "label": collapse_ws(details.group(1)) if details else "",
            "value": collapse_ws(details.group(3)) if details else "",
        },
        "metric_2": {
            "label": collapse_ws(details.group(2)) if details else "",
            "value": collapse_ws(details.group(4)) if details else "",
        },
        "has_bio": 'id="user_bio"' in document,
        "has_subreddit_list": 'id="user_subreddits"' in document,
    }


def detect_block(document: str, status: int, headers: dict) -> dict | None:
    title_match = TITLE_RE.search(document)
    title = collapse_ws(title_match.group(1)) if title_match else ""
    lower = document.lower()

    if "anubis" in lower or "making sure you&#39;re not a bot" in lower or "making sure you're not a bot" in lower:
        return {
            "kind": "anubis",
            "status": status,
            "title": title,
        }

    if status >= 400:
        return {
            "kind": "http_error",
            "status": status,
            "title": title,
            "server": headers.get("server", ""),
        }

    if "cf-browser-verification" in lower or "just a moment" in title.lower():
        return {
            "kind": "cloudflare_challenge",
            "status": status,
            "title": title,
        }

    return None


def fetch(url: str, user_agent: str) -> tuple[int, dict, str]:
    request = urllib.request.Request(
        url,
        headers={
            "User-Agent": user_agent,
            "Accept": "text/html,application/xhtml+xml",
        },
    )

    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return response.status, dict(response.headers.items()), response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        return error.code, dict(error.headers.items()), error.read().decode("utf-8", "replace")


def parse_rss(document: str, limit: int) -> dict:
    root = ElementTree.fromstring(document)
    channel = root.find("channel")
    if channel is None:
        return {"title": "", "description": "", "items": []}

    items = []
    for item in channel.findall("item")[:limit]:
        items.append(
            {
                "title": collapse_ws(item.findtext("title", default="")),
                "link": collapse_ws(item.findtext("link", default="")),
                "author": collapse_ws(item.findtext("author", default="")),
                "pub_date": collapse_ws(item.findtext("pubDate", default="")),
                "description": collapse_ws(item.findtext("description", default="")),
                "content_html_present": any(
                    child.tag.endswith("encoded") and collapse_ws(child.text or "") for child in item
                ),
            }
        )

    return {
        "title": collapse_ws(channel.findtext("title", default="")),
        "description": collapse_ws(channel.findtext("description", default="")),
        "items": items,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Fetch and parse a stock Redlib subreddit or user page.")
    parser.add_argument("url", help="Full Redlib page URL, for example https://redlib.perennialte.ch/r/rust")
    parser.add_argument("--limit", type=int, default=3, help="Number of listing posts to include")
    parser.add_argument(
        "--user-agent",
        default="Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36",
        help="HTTP user agent to send",
    )
    args = parser.parse_args()

    status, headers, document = fetch(args.url, args.user_agent)
    title_match = TITLE_RE.search(document)
    title = collapse_ws(title_match.group(1)) if title_match else ""

    result = {
        "url": args.url,
        "status": status,
        "title": title,
        "headers": {
            "server": headers.get("server", ""),
            "cf-cache-status": headers.get("cf-cache-status", ""),
            "cf-ray": headers.get("cf-ray", ""),
        },
    }

    block = detect_block(document, status, headers)
    if block:
        result["block"] = block
        print(json.dumps(result, indent=2))
        return 0

    content_type = headers.get("content-type", "").lower()
    if "application/rss+xml" in content_type or document.lstrip().startswith("<?xml"):
        result["page_type"] = "rss"
        result["feed"] = parse_rss(document, args.limit)
        print(json.dumps(result, indent=2))
        return 0

    listing = ListingParser(limit=args.limit)
    listing.feed(document)
    result["posts"] = listing.posts

    if 'id="sub_title"' in document:
        result["page_type"] = "subreddit"
        result["page"] = parse_subreddit(document)
    elif 'id="user_title"' in document:
        result["page_type"] = "user"
        result["page"] = parse_user(document)
    else:
        result["page_type"] = "unknown"

    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
