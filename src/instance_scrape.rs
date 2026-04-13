use ammonia::Builder as AmmoniaBuilder;
use chrono::NaiveDateTime;
use hyper::body::to_bytes;
use hyper::header::{ACCEPT, USER_AGENT};
use hyper::{Body, Method, Request, StatusCode};
use log::info;
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use url::Url;

use crate::client::{latest_ratelimit_snapshot, CLIENT};
use crate::config::get_setting;
use crate::utils::{cached_author_external_links, format_num, Author, Awards, Comment, ExternalLink, Flags, Flair, Media, Post, Preferences, Subreddit, User};

const INSTANCE_CATALOG_URL: &str = "https://raw.githubusercontent.com/redlib-org/redlib-instances/main/instances.json";
const INSTANCE_CATALOG_TTL_SECS: u64 = 3 * 24 * 60 * 60;
const SOURCE_FAILURE_BACKOFF_SECS: u64 = 10 * 60;
const PROBE_PATH: &str = "/r/rust/new";
const DEFAULT_TIMEOUT_SECS: u64 = 6;
const SCRAPE_NOTICE: &str = "Showing content fetched from a public Redlib instance because Reddit rate limited this request.";
const PROBE_CONCURRENCY: usize = 8;
const SCRAPE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";

static INSTANCE_HEALTH_CACHE: LazyLock<Mutex<Option<InstanceHealthCache>>> = LazyLock::new(|| Mutex::new(None));
static INSTANCE_FAILURE_BACKOFF: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

static SEL_SUB_TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#sub_title").unwrap());
static SEL_SUB_NAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#sub_name").unwrap());
static SEL_SUB_DESCRIPTION: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#sub_description").unwrap());
static SEL_SUB_ICON: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#sub_icon").unwrap());
static SEL_SUB_DETAILS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#sub_details").unwrap());
static SEL_SIDEBAR_CONTENTS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#sidebar_contents").unwrap());
static SEL_WIKI_LINK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href*=\"/wiki/index\"]").unwrap());
static SEL_USER_TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#user_title").unwrap());
static SEL_USER_NAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#user_name").unwrap());
static SEL_USER_DESCRIPTION: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#user_description").unwrap());
static SEL_USER_ICON: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#user_icon").unwrap());
static SEL_USER_DETAILS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#user_details").unwrap());
static SEL_USER_BIO: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#user_bio").unwrap());
static SEL_COLUMN_ONE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#column_one").unwrap());
static SEL_POST: LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.post").unwrap());
static SEL_POST_HEADER: LazyLock<Selector> = LazyLock::new(|| Selector::parse("p.post_header").unwrap());
static SEL_POST_SUBREDDIT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.post_subreddit").unwrap());
static SEL_POST_AUTHOR: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.post_author").unwrap());
static SEL_POST_TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".post_title").unwrap());
static SEL_POST_TITLE_LINKS: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".post_title a").unwrap());
static SEL_POST_FLAIR: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.post_flair").unwrap());
static SEL_CREATED: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".created").unwrap());
static SEL_POST_SCORE: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".post_score").unwrap());
static SEL_POST_BODY: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".post_body").unwrap());
static SEL_POST_PREVIEW: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".post_body.post_preview").unwrap());
static SEL_POST_COMMENTS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.post_comments").unwrap());
static SEL_POST_FOOTER_LINKS: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".post_footer a").unwrap());
static SEL_POST_MEDIA_IMAGE: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".post_media_content img").unwrap());
static SEL_POST_THUMBNAIL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.post_thumbnail").unwrap());
static SEL_POST_THUMBNAIL_DESC_IMG: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.post_thumbnail desc img").unwrap());
static SEL_FOOTER_NEXT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("footer a[accesskey=\"N\"]").unwrap());
static SEL_THREAD: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".thread").unwrap());
static SEL_COMMENT_AUTHOR: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.comment_author").unwrap());
static SEL_COMMENT_LINK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.comment_link").unwrap());
static SEL_COMMENT_SUBREDDIT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.comment_subreddit").unwrap());
static SEL_COMMENT_SCORE: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".comment_score").unwrap());
static SEL_COMMENT_BODY: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".comment_body").unwrap());
static SEL_REPLIES: LazyLock<Selector> = LazyLock::new(|| Selector::parse("blockquote.replies").unwrap());
static SEL_COMMENT_COUNT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("#comment_count").unwrap());
static SEL_SEARCH_SUBREDDIT_CARD: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.search_subreddit").unwrap());
static SEL_SEARCH_SUBREDDIT_NAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".search_subreddit_name").unwrap());
static SEL_SEARCH_SUBREDDIT_MEMBERS: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".search_subreddit_members").unwrap());
static SEL_SEARCH_SUBREDDIT_DESCRIPTION: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".search_subreddit_description").unwrap());
static SEL_SEARCH_SUBREDDIT_ICON: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".search_subreddit_left img").unwrap());

#[derive(Clone)]
struct InstanceHealthCache {
	refreshed_at: Instant,
	usable_origins: Vec<String>,
}

#[derive(Deserialize)]
struct CatalogResponse {
	instances: Vec<CatalogInstance>,
}

#[derive(Clone, Deserialize)]
struct CatalogInstance {
	url: Option<String>,
}

pub(crate) struct ScrapedSubredditPage {
	pub subreddit: Subreddit,
	pub posts: Vec<Post>,
	pub after: String,
}

pub(crate) struct ScrapedUserPage {
	pub user: User,
	pub posts: Vec<Post>,
	pub after: String,
}

pub(crate) struct ScrapedPostPage {
	pub post: Post,
	pub comments: Vec<Comment>,
}

pub(crate) struct ScrapedSearchSubreddit {
	pub name: String,
	pub url: String,
	pub icon: String,
	pub description: String,
	pub subscribers: (String, String),
}

pub(crate) struct ScrapedSearchPage {
	pub posts: Vec<Post>,
	pub subreddits: Vec<ScrapedSearchSubreddit>,
	pub after: String,
}

pub(crate) fn scrape_notice() -> &'static str {
	SCRAPE_NOTICE
}

pub(crate) fn instance_scrape_enabled() -> bool {
	get_setting("REDLIB_INSTANCE_SCRAPE_ENABLED")
		.map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
		.unwrap_or(false)
}

pub(crate) fn should_attempt_scrape_fallback(error: &str) -> bool {
	let message = error.to_ascii_lowercase();
	message.contains("too many requests") || message.contains("rate limit") || message.contains("ratelimit") || reddit_rate_limited_now()
}

pub(crate) fn reddit_rate_limited_now() -> bool {
	latest_ratelimit_snapshot().is_some_and(|snapshot| snapshot.is_rate_limited())
}

pub(crate) fn apply_comment_prefs(comments: &mut [Comment], prefs: &Preferences) {
	for comment in comments {
		comment.prefs = prefs.clone();
		apply_comment_prefs(&mut comment.replies, prefs);
	}
}

pub(crate) async fn scrape_subreddit_page(path_and_query: &str) -> Result<ScrapedSubredditPage, String> {
	let html = fetch_page_html(path_and_query).await?;
	parse_subreddit_page(&html)
}

pub(crate) async fn scrape_user_page(path_and_query: &str) -> Result<ScrapedUserPage, String> {
	let html = fetch_page_html(path_and_query).await?;
	parse_user_page(&html)
}

pub(crate) async fn scrape_post_page(path_and_query: &str, highlighted_comment_id: &str) -> Result<ScrapedPostPage, String> {
	let html = fetch_page_html(path_and_query).await?;
	parse_post_page(&html, highlighted_comment_id)
}

pub(crate) async fn scrape_search_page(path_and_query: &str) -> Result<ScrapedSearchPage, String> {
	let html = fetch_page_html(path_and_query).await?;
	parse_search_page(&html)
}

fn request_timeout() -> Duration {
	let secs = get_setting("REDLIB_INSTANCE_SCRAPE_TIMEOUT_SECS")
		.and_then(|value| value.parse::<u64>().ok())
		.filter(|value| *value > 0)
		.unwrap_or(DEFAULT_TIMEOUT_SECS);
	Duration::from_secs(secs)
}

async fn fetch_page_html(path_and_query: &str) -> Result<String, String> {
	let origins = candidate_origins().await?;
	if origins.is_empty() {
		return Err("no healthy Redlib instances available for scrape fallback".to_string());
	}

	let path_and_query = if path_and_query.starts_with('/') {
		path_and_query.to_string()
	} else {
		format!("/{path_and_query}")
	};

	let mut last_error = String::from("no scrape candidate attempted");
	for origin in origins {
		if source_is_backed_off(&origin) {
			continue;
		}

		let url = format!("{origin}{path_and_query}");
		match fetch_text(&url, "text/html,application/xhtml+xml").await {
			Ok((status, body)) => {
				if status != StatusCode::OK {
					note_source_failure(&origin);
					last_error = format!("instance returned HTTP {}", status.as_u16());
					continue;
				}

				if detect_block_page(status, &body).is_some() {
					note_source_failure(&origin);
					last_error = "instance returned anti-bot interstitial".to_string();
					continue;
				}

				info!("instance_scrape: fetched {path_and_query} from {origin}");
				return Ok(body);
			}
			Err(error) => {
				note_source_failure(&origin);
				last_error = error;
			}
		}
	}

	Err(last_error)
}

async fn candidate_origins() -> Result<Vec<String>, String> {
	if let Some(overrides) = configured_override_origins() {
		return Ok(overrides);
	}

	{
		let Ok(cache) = INSTANCE_HEALTH_CACHE.lock() else {
			return Err("instance health cache lock poisoned".to_string());
		};
		if let Some(cache) = cache.as_ref() {
			if cache.refreshed_at.elapsed().as_secs() < INSTANCE_CATALOG_TTL_SECS {
				return Ok(cache.usable_origins.clone());
			}
		}
	}

	refresh_catalog().await
}

fn configured_override_origins() -> Option<Vec<String>> {
	let configured = get_setting("REDLIB_INSTANCE_SCRAPE_SOURCES")?;
	let origins = configured
		.split(',')
		.map(str::trim)
		.filter(|value| !value.is_empty())
		.map(ToString::to_string)
		.collect::<Vec<_>>();
	(!origins.is_empty()).then_some(origins)
}

async fn refresh_catalog() -> Result<Vec<String>, String> {
	let (_, body) = fetch_text(INSTANCE_CATALOG_URL, "application/json").await?;
	let catalog: CatalogResponse = serde_json::from_str(&body).map_err(|error| error.to_string())?;

	let probe_candidates = catalog
		.instances
		.into_iter()
		.filter_map(|instance| instance.url)
		.filter(|origin| origin.starts_with("https://"))
		.collect::<Vec<_>>();
	let usable_origins = probe_catalog_origins(probe_candidates).await;

	let Ok(mut cache) = INSTANCE_HEALTH_CACHE.lock() else {
		return Err("instance health cache lock poisoned".to_string());
	};
	*cache = Some(InstanceHealthCache {
		refreshed_at: Instant::now(),
		usable_origins: usable_origins.clone(),
	});
	Ok(usable_origins)
}

async fn probe_catalog_origins(origins: Vec<String>) -> Vec<String> {
	let mut ordered_results = Vec::new();
	let mut probes = JoinSet::new();

	for (index, origin) in origins.into_iter().enumerate() {
		probes.spawn(async move {
			let usable = probe_instance(&origin).await;
			(index, origin, usable)
		});

		if probes.len() >= PROBE_CONCURRENCY {
			if let Some((index, origin, usable)) = join_probe_result(&mut probes).await {
				ordered_results.push((index, origin, usable));
			}
		}
	}

	while let Some((index, origin, usable)) = join_probe_result(&mut probes).await {
		ordered_results.push((index, origin, usable));
	}

	ordered_results.sort_by_key(|(index, _, _)| *index);
	ordered_results.into_iter().filter_map(|(_, origin, usable)| usable.then_some(origin)).collect()
}

async fn join_probe_result(probes: &mut JoinSet<(usize, String, bool)>) -> Option<(usize, String, bool)> {
	while let Some(result) = probes.join_next().await {
		if let Ok(result) = result {
			return Some(result);
		}
	}
	None
}

async fn probe_instance(origin: &str) -> bool {
	let url = format!("{origin}{PROBE_PATH}");
	match fetch_text(&url, "text/html,application/xhtml+xml").await {
		Ok((status, body)) => {
			if status != StatusCode::OK {
				return false;
			}
			if detect_block_page(status, &body).is_some() {
				return false;
			}
			let document = Html::parse_document(&body);
			document.select(&SEL_SUB_TITLE).next().is_some() && document.select(&SEL_POST).next().is_some()
		}
		Err(_) => false,
	}
}

fn source_is_backed_off(origin: &str) -> bool {
	let Ok(mut backoff) = INSTANCE_FAILURE_BACKOFF.lock() else {
		return false;
	};
	backoff.retain(|_, until| *until > Instant::now());
	backoff.get(origin).is_some_and(|until| *until > Instant::now())
}

fn note_source_failure(origin: &str) {
	if let Ok(mut backoff) = INSTANCE_FAILURE_BACKOFF.lock() {
		backoff.insert(origin.to_string(), Instant::now() + Duration::from_secs(SOURCE_FAILURE_BACKOFF_SECS));
	}
}

async fn fetch_text(url: &str, accept: &str) -> Result<(StatusCode, String), String> {
	let request = Request::builder()
		.method(Method::GET)
		.uri(url)
		.header(USER_AGENT, SCRAPE_UA)
		.header(ACCEPT, accept)
		.body(Body::empty())
		.map_err(|error| error.to_string())?;

	let response = tokio::time::timeout(request_timeout(), CLIENT.request(request))
		.await
		.map_err(|_| format!("request timed out for {url}"))?
		.map_err(|error| error.to_string())?;
	let status = response.status();
	let body = to_bytes(response.into_body()).await.map_err(|error| error.to_string())?;
	Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

fn detect_block_page(status: StatusCode, document: &str) -> Option<&'static str> {
	let lower = document.to_ascii_lowercase();
	if lower.contains("making sure you&#39;re not a bot")
		|| lower.contains("making sure you're not a bot")
		|| lower.contains("cf-browser-verification")
		|| lower.contains("just a moment")
		|| lower.contains("anubis")
	{
		return Some("anti-bot interstitial");
	}

	if status != StatusCode::OK {
		return Some("http error");
	}

	if lower.contains("backend is down") {
		return Some("backend down");
	}

	None
}

fn parse_subreddit_page(document: &str) -> Result<ScrapedSubredditPage, String> {
	let parsed = Html::parse_document(document);
	let mut subreddit = Subreddit::default();
	subreddit.title = first_text(&parsed, &SEL_SUB_TITLE);
	subreddit.name = normalize_resource_name(&first_text(&parsed, &SEL_SUB_NAME));
	subreddit.description = first_text(&parsed, &SEL_SUB_DESCRIPTION);
	subreddit.icon = first_attr(&parsed, &SEL_SUB_ICON, "src");
	subreddit.info = sanitize_html(&first_inner_html(&parsed, &SEL_SIDEBAR_CONTENTS));
	subreddit.has_sidebar = !subreddit.info.is_empty();
	subreddit.wiki = parsed.select(&SEL_WIKI_LINK).next().is_some();

	if let Some(details) = parsed.select(&SEL_SUB_DETAILS).next() {
		let values = details
			.children()
			.filter_map(ElementRef::wrap)
			.filter(|element| element.value().name() == "div")
			.map(|element| (text_contents(element), element.value().attr("title").unwrap_or_default().to_string()))
			.collect::<Vec<_>>();
		if let Some((display, raw)) = values.first() {
			subreddit.members = (display.clone(), if raw.is_empty() { display.clone() } else { raw.clone() });
		}
		if let Some((display, raw)) = values.get(1) {
			subreddit.active = (display.clone(), if raw.is_empty() { display.clone() } else { raw.clone() });
		}
	}

	if subreddit.name.is_empty() {
		return Err("scraped subreddit page missing subreddit name".to_string());
	}

	let posts = parsed
		.select(&SEL_POST)
		.filter(|element| !has_class(element, "highlighted"))
		.filter_map(|element| parse_post_card(element, ListingContext::Subreddit))
		.collect::<Vec<_>>();

	Ok(ScrapedSubredditPage {
		subreddit,
		posts,
		after: next_after_token(&parsed).unwrap_or_default(),
	})
}

fn parse_user_page(document: &str) -> Result<ScrapedUserPage, String> {
	let parsed = Html::parse_document(document);
	let mut user = User::default();
	user.title = first_text(&parsed, &SEL_USER_TITLE);
	user.name = normalize_resource_name(&first_text(&parsed, &SEL_USER_NAME));
	user.icon = first_attr(&parsed, &SEL_USER_ICON, "src");
	user.description = first_text(&parsed, &SEL_USER_DESCRIPTION);
	user.bio_html = sanitize_html(&first_inner_html(&parsed, &SEL_USER_BIO));
	user.external_links = Vec::<ExternalLink>::new();

	if let Some(details) = parsed.select(&SEL_USER_DETAILS).next() {
		let labels = details
			.children()
			.filter_map(ElementRef::wrap)
			.filter(|element| element.value().name() == "label")
			.map(text_contents)
			.collect::<Vec<_>>();
		let values = details
			.children()
			.filter_map(ElementRef::wrap)
			.filter(|element| element.value().name() == "div")
			.map(text_contents)
			.collect::<Vec<_>>();
		for (index, label) in labels.iter().enumerate() {
			let value = values.get(index).cloned().unwrap_or_default();
			match label.trim().to_ascii_lowercase().as_str() {
				"post karma" => user.post_karma = (value.clone(), value),
				"comment karma" => user.comment_karma = (value.clone(), value),
				"karma" => {
					user.post_karma = (value.clone(), value.clone());
					user.comment_karma = ("Unavailable".to_string(), "Stock Redlib only exposes total karma on the scraped page".to_string());
				}
				_ => {}
			}
		}
	}

	if user.name.is_empty() {
		return Err("scraped user page missing username".to_string());
	}

	let mut posts = Vec::new();
	if let Some(column_one) = parsed.select(&SEL_COLUMN_ONE).next() {
		for child in column_one.children().filter_map(ElementRef::wrap) {
			if has_class(&child, "post") {
				if let Some(post) = parse_post_card(child, ListingContext::User) {
					posts.push(post);
				}
			} else if has_class(&child, "comment") {
				if let Some(comment_post) = parse_user_comment_card(child, &user) {
					posts.push(comment_post);
				}
			}
		}
	}

	Ok(ScrapedUserPage {
		user,
		posts,
		after: next_after_token(&parsed).unwrap_or_default(),
	})
}

fn parse_post_page(document: &str, highlighted_comment_id: &str) -> Result<ScrapedPostPage, String> {
	let parsed = Html::parse_document(document);
	let highlighted = parsed
		.select(&SEL_POST)
		.find(|element| has_class(element, "highlighted"))
		.ok_or_else(|| "scraped post page missing highlighted post".to_string())?;
	let mut post = parse_post_card(highlighted, ListingContext::PostPage).ok_or_else(|| "scraped post page highlighted post was not parseable".to_string())?;
	post.comments = parse_count_pair(parsed.select(&SEL_COMMENT_COUNT).next().map(text_contents).unwrap_or_default().as_str());

	let comments = parsed
		.select(&SEL_THREAD)
		.filter_map(|thread| {
			thread
				.children()
				.filter_map(ElementRef::wrap)
				.find(|element| has_class(element, "comment"))
				.and_then(|comment| parse_comment_tree(comment, &post.permalink, &post.author.name, "t3", highlighted_comment_id))
		})
		.collect::<Vec<_>>();

	Ok(ScrapedPostPage { post, comments })
}

fn parse_search_page(document: &str) -> Result<ScrapedSearchPage, String> {
	let parsed = Html::parse_document(document);
	let mut posts = Vec::new();
	if let Some(column_one) = parsed.select(&SEL_COLUMN_ONE).next() {
		for child in column_one.children().filter_map(ElementRef::wrap) {
			if has_class(&child, "post") {
				if let Some(post) = parse_post_card(child, ListingContext::Search) {
					posts.push(post);
				}
			}
		}
	}

	let subreddits = parsed
		.select(&SEL_SEARCH_SUBREDDIT_CARD)
		.filter(|element| element.value().attr("id").unwrap_or_default() != "more_subreddits")
		.filter_map(parse_search_subreddit_card)
		.collect::<Vec<_>>();

	if posts.is_empty() && subreddits.is_empty() {
		return Err("scraped search page did not contain posts or subreddit results".to_string());
	}

	Ok(ScrapedSearchPage {
		posts,
		subreddits,
		after: next_after_token(&parsed).unwrap_or_default(),
	})
}

#[derive(Clone, Copy)]
enum ListingContext {
	Subreddit,
	User,
	PostPage,
	Search,
}

fn parse_post_card(element: ElementRef<'_>, context: ListingContext) -> Option<Post> {
	let header = element.select(&SEL_POST_HEADER).next();
	let post_subreddit = header.as_ref().and_then(|header| header.select(&SEL_POST_SUBREDDIT).next());
	let community = normalize_community_name(&post_subreddit.map(text_contents).unwrap_or_default());
	let author_link = header.as_ref().and_then(|header| header.select(&SEL_POST_AUTHOR).next());
	let author_name = normalize_resource_name(&author_link.map(text_contents).unwrap_or_default());
	let distinguished = author_link.as_ref().map(|author| distinguished_from_classes(author)).unwrap_or_default();
	let created_el = header.as_ref().and_then(|header| header.select(&SEL_CREATED).next());
	let created = created_el.as_ref().and_then(|element| element.value().attr("title")).unwrap_or_default().to_string();
	let rel_time = created_el.as_ref().map(|element| text_contents(*element)).unwrap_or_default();
	let created_ts = parse_created_ts(&created);
	let flair = element.select(&SEL_POST_FLAIR).next().map(parse_flair).unwrap_or_else(empty_flair);
	let title_el = element.select(&SEL_POST_TITLE).next()?;
	let title_link = title_el.select(&SEL_POST_TITLE_LINKS).find(|link| !has_class(link, "post_flair"));
	let permalink = title_link
		.as_ref()
		.and_then(|link| link.value().attr("href"))
		.or_else(|| {
			element
				.select(&SEL_POST_FOOTER_LINKS)
				.find(|link| text_contents(*link).contains("permalink"))
				.and_then(|link| link.value().attr("href"))
		})
		.unwrap_or_default()
		.to_string();
	let title = title_from_heading(title_el, title_link, &flair.text);
	let score = element
		.select(&SEL_POST_SCORE)
		.next()
		.map(|node| {
			let display = text_contents(node);
			let raw = node.value().attr("title").unwrap_or(&display).to_string();
			(if display.is_empty() { raw.clone() } else { display }, if raw.is_empty() { "0".to_string() } else { raw })
		})
		.unwrap_or_else(|| ("0".to_string(), "0".to_string()));
	let comments = element
		.select(&SEL_POST_COMMENTS)
		.next()
		.map(|node| parse_count_pair(node.value().attr("title").unwrap_or(&text_contents(node))))
		.unwrap_or_else(|| ("0".to_string(), "0".to_string()));

	let preview_html = element.select(&SEL_POST_PREVIEW).next().map(|body| sanitize_html(&body.inner_html())).unwrap_or_default();
	let body_html = if matches!(context, ListingContext::PostPage) {
		element.select(&SEL_POST_BODY).next().map(|body| sanitize_html(&body.inner_html())).unwrap_or_default()
	} else {
		String::new()
	};

	let mut post_type = if matches!(context, ListingContext::PostPage) && !body_html.is_empty() {
		"self".to_string()
	} else if !preview_html.is_empty() {
		"self".to_string()
	} else {
		"link".to_string()
	};
	let mut media = Media::empty();
	let mut thumbnail = Media::empty();

	if let Some(image) = element.select(&SEL_POST_MEDIA_IMAGE).next() {
		let src = image.value().attr("data-full-src").or_else(|| image.value().attr("src")).unwrap_or_default().to_string();
		if !src.is_empty() {
			post_type = "image".to_string();
			media.url = src.clone();
			media.preview_url = image.value().attr("src").unwrap_or_default().to_string();
			media.width = image.value().attr("width").and_then(|value| value.parse::<i64>().ok()).unwrap_or_default();
			media.height = image.value().attr("height").and_then(|value| value.parse::<i64>().ok()).unwrap_or_default();
		}
	}

	if let Some(thumbnail_link) = element.select(&SEL_POST_THUMBNAIL).next() {
		if !has_class(&thumbnail_link, "no_thumbnail") {
			if let Some(image) = thumbnail_link.select(&SEL_POST_THUMBNAIL_DESC_IMG).next() {
				thumbnail.url = image.value().attr("src").unwrap_or_default().to_string();
			}
		}
	}

	let link_title = if matches!(context, ListingContext::PostPage) { String::new() } else { title.clone() };
	let link_permalink = permalink.clone();
	let id = element
		.value()
		.attr("id")
		.map(ToString::to_string)
		.or_else(|| permalink.split('/').find(|segment| segment.starts_with('1')).map(ToString::to_string))
		.unwrap_or_default();

	Some(Post {
		id,
		kind: "t3".to_string(),
		title,
		community,
		body: body_html,
		author: Author {
			name: author_name.clone(),
			flair: empty_flair(),
			distinguished,
			external_links: cached_author_external_links(&author_name),
			avg_video_length: crate::user::cached_avg_video_length(&author_name),
		},
		permalink,
		parent_kind: String::new(),
		link_title,
		link_permalink,
		poll: None,
		score,
		upvote_ratio: 0,
		post_type,
		flair,
		flags: Flags::new(false, false, has_class(&element, "stickied")),
		thumbnail,
		media,
		domain: String::new(),
		rel_time,
		created,
		created_ts,
		num_duplicates: 0,
		comments,
		gallery: Vec::new(),
		awards: Awards(Vec::new()),
		nsfw: false,
		out_url: None,
		ws_url: String::new(),
		preview_body: if matches!(context, ListingContext::PostPage) { String::new() } else { preview_html },
		collapsed_duplicates: Vec::new(),
	})
}

fn parse_search_subreddit_card(element: ElementRef<'_>) -> Option<ScrapedSearchSubreddit> {
	let name = normalize_resource_name(&element.select(&SEL_SEARCH_SUBREDDIT_NAME).next().map(text_contents).unwrap_or_default());
	if name.is_empty() {
		return None;
	}

	let url = element.value().attr("href").unwrap_or_default().to_string();
	let icon = element
		.select(&SEL_SEARCH_SUBREDDIT_ICON)
		.next()
		.and_then(|image| image.value().attr("src"))
		.unwrap_or_default()
		.to_string();
	let description = element.select(&SEL_SEARCH_SUBREDDIT_DESCRIPTION).next().map(text_contents).unwrap_or_default();
	let subscribers = element
		.select(&SEL_SEARCH_SUBREDDIT_MEMBERS)
		.next()
		.map(|members| {
			let display = text_contents(members);
			let raw = members.value().attr("title").unwrap_or(&display).to_string();
			(if display.is_empty() { raw.clone() } else { display }, if raw.is_empty() { "0".to_string() } else { raw })
		})
		.unwrap_or_else(|| ("0".to_string(), "0".to_string()));

	Some(ScrapedSearchSubreddit {
		name,
		url,
		icon,
		description,
		subscribers,
	})
}

fn parse_user_comment_card(element: ElementRef<'_>, user: &User) -> Option<Post> {
	let link = element.select(&SEL_COMMENT_LINK).next()?;
	let created = element
		.select(&SEL_CREATED)
		.next()
		.and_then(|created| created.value().attr("title"))
		.unwrap_or_default()
		.to_string();
	let rel_time = element.select(&SEL_CREATED).next().map(text_contents).unwrap_or_default();
	let community = normalize_community_name(&element.select(&SEL_COMMENT_SUBREDDIT).next().map(text_contents).unwrap_or_default());
	let permalink = link.value().attr("href").unwrap_or_default().to_string();
	let title = link.value().attr("title").unwrap_or_default().to_string();
	let body = element.select(&SEL_COMMENT_BODY).next().map(|body| sanitize_html(&body.inner_html())).unwrap_or_default();
	let score = element
		.select(&SEL_COMMENT_SCORE)
		.next()
		.map(|score| {
			let display = text_contents(score);
			let raw = score.value().attr("title").unwrap_or(&display).to_string();
			(if display.is_empty() { raw.clone() } else { display }, if raw.is_empty() { "0".to_string() } else { raw })
		})
		.unwrap_or_else(|| ("0".to_string(), "0".to_string()));

	Some(Post {
		id: permalink
			.trim_end_matches('/')
			.rsplit('/')
			.find(|segment| !segment.is_empty())
			.map(ToString::to_string)
			.unwrap_or_default(),
		kind: "t1".to_string(),
		title: String::new(),
		community,
		body,
		author: Author {
			name: user.name.clone(),
			flair: empty_flair(),
			distinguished: String::new(),
			external_links: user.external_links.clone(),
			avg_video_length: crate::user::cached_avg_video_length(&user.name),
		},
		permalink: permalink.clone(),
		parent_kind: "t3".to_string(),
		link_title: title,
		link_permalink: permalink,
		poll: None,
		score,
		upvote_ratio: 0,
		post_type: "comment".to_string(),
		flair: empty_flair(),
		flags: Flags::new(false, false, false),
		thumbnail: Media::empty(),
		media: Media::empty(),
		domain: String::new(),
		rel_time,
		created,
		created_ts: 0,
		num_duplicates: 0,
		comments: ("0".to_string(), "0".to_string()),
		gallery: Vec::new(),
		awards: Awards(Vec::new()),
		nsfw: false,
		out_url: None,
		ws_url: String::new(),
		preview_body: String::new(),
		collapsed_duplicates: Vec::new(),
	})
}

fn parse_comment_tree(element: ElementRef<'_>, post_link: &str, post_author: &str, parent_kind: &str, highlighted_comment_id: &str) -> Option<Comment> {
	let id = element.value().attr("id")?.to_string();
	let author_link = element.select(&SEL_COMMENT_AUTHOR).next();
	let author_name = normalize_resource_name(&author_link.map(text_contents).unwrap_or_else(|| "[deleted]".to_string()));
	let distinguished = author_link.as_ref().map(|author| distinguished_from_classes(author)).unwrap_or_default();
	let created_link = element.select(&SEL_CREATED).next();
	let created = created_link.as_ref().and_then(|created| created.value().attr("title")).unwrap_or_default().to_string();
	let rel_time = created_link.as_ref().map(|created| text_contents(*created)).unwrap_or_default();
	let body = element.select(&SEL_COMMENT_BODY).next().map(|body| sanitize_html(&body.inner_html())).unwrap_or_default();
	let score = element
		.select(&SEL_COMMENT_SCORE)
		.next()
		.map(|score| {
			let display = text_contents(score);
			let raw = score.value().attr("title").unwrap_or(&display).to_string();
			(if display.is_empty() { raw.clone() } else { display }, if raw.is_empty() { "0".to_string() } else { raw })
		})
		.unwrap_or_else(|| ("0".to_string(), "0".to_string()));
	let replies = element
		.select(&SEL_REPLIES)
		.next()
		.map(|replies| {
			replies
				.children()
				.filter_map(ElementRef::wrap)
				.filter(|child| has_class(child, "comment"))
				.filter_map(|child| parse_comment_tree(child, post_link, post_author, "t1", highlighted_comment_id))
				.collect::<Vec<_>>()
		})
		.unwrap_or_default();

	Some(Comment {
		id: id.clone(),
		kind: "t1".to_string(),
		parent_id: id,
		parent_kind: parent_kind.to_string(),
		post_link: post_link.to_string(),
		post_author: post_author.to_string(),
		body,
		author: Author {
			name: author_name,
			flair: empty_flair(),
			distinguished,
			external_links: Vec::new(),
			avg_video_length: None,
		},
		score,
		rel_time,
		created,
		edited: (String::new(), String::new()),
		replies,
		highlighted: !highlighted_comment_id.is_empty() && highlighted_comment_id == element.value().attr("id").unwrap_or_default(),
		awards: Awards(Vec::new()),
		collapsed: false,
		is_filtered: false,
		more_count: 0,
		prefs: Preferences::default(),
	})
}

fn sanitize_html(fragment: &str) -> String {
	AmmoniaBuilder::default().clean(fragment).to_string()
}

fn next_after_token(parsed: &Html) -> Option<String> {
	let href = parsed.select(&SEL_FOOTER_NEXT).next()?.value().attr("href")?;
	let base = Url::parse("https://redlib.local").ok()?;
	let url = base.join(href).ok()?;
	url.query_pairs().find_map(|(key, value)| (key == "after").then_some(value.into_owned()))
}

fn parse_flair(element: ElementRef<'_>) -> Flair {
	let style = element.value().attr("style").unwrap_or_default();
	Flair {
		flair_parts: vec![crate::utils::FlairPart {
			flair_part_type: "text".to_string(),
			value: text_contents(element),
		}],
		text: text_contents(element),
		background_color: style_value(style, "background").unwrap_or_default(),
		foreground_color: style_value(style, "color").unwrap_or_else(|| "white".to_string()),
	}
}

fn style_value(style: &str, name: &str) -> Option<String> {
	style
		.split(';')
		.filter_map(|segment| segment.split_once(':'))
		.find_map(|(key, value)| (key.trim() == name).then_some(value.trim().to_string()))
}

fn parse_count_pair(raw: &str) -> (String, String) {
	let trimmed = collapse_whitespace(raw);
	if trimmed.is_empty() {
		return ("0".to_string(), "0".to_string());
	}
	let leading_digits = trimmed
		.split_whitespace()
		.next()
		.unwrap_or_default()
		.trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '.')
		.replace(',', "");
	if let Ok(value) = leading_digits.parse::<i64>() {
		return format_num(value);
	}
	(trimmed.clone(), trimmed)
}

fn parse_created_ts(created: &str) -> u64 {
	NaiveDateTime::parse_from_str(created, "%b %d %Y, %H:%M:%S UTC")
		.map(|value| value.and_utc().timestamp().max(0) as u64)
		.unwrap_or_default()
}

fn title_from_heading(title_el: ElementRef<'_>, title_link: Option<ElementRef<'_>>, flair_text: &str) -> String {
	if let Some(link) = title_link {
		return text_contents(link);
	}

	let mut combined = text_contents(title_el);
	let flair_trimmed = collapse_whitespace(flair_text);
	if !flair_trimmed.is_empty() && combined.starts_with(&flair_trimmed) {
		combined = combined[flair_trimmed.len()..].trim().to_string();
	}
	combined
}

fn distinguished_from_classes(element: &ElementRef<'_>) -> String {
	let classes = element.value().attr("class").unwrap_or_default();
	if classes.split_whitespace().any(|class_name| class_name == "admin") {
		"admin".to_string()
	} else if classes.split_whitespace().any(|class_name| class_name == "moderator") {
		"moderator".to_string()
	} else {
		String::new()
	}
}

fn first_text(document: &Html, selector: &Selector) -> String {
	document.select(selector).next().map(text_contents).unwrap_or_default()
}

fn first_inner_html(document: &Html, selector: &Selector) -> String {
	document.select(selector).next().map(|element| element.inner_html()).unwrap_or_default()
}

fn first_attr(document: &Html, selector: &Selector, attr: &str) -> String {
	document
		.select(selector)
		.next()
		.and_then(|element| element.value().attr(attr))
		.unwrap_or_default()
		.to_string()
}

fn text_contents(element: ElementRef<'_>) -> String {
	collapse_whitespace(&element.text().collect::<Vec<_>>().join(" "))
}

fn collapse_whitespace(value: &str) -> String {
	value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_resource_name(value: &str) -> String {
	let trimmed = value.trim();
	if let Some(name) = trimmed.strip_prefix("u/") {
		name.trim().to_string()
	} else if let Some(name) = trimmed.strip_prefix("r/") {
		name.trim().to_string()
	} else {
		trimmed.to_string()
	}
}

fn normalize_community_name(value: &str) -> String {
	let trimmed = value.trim();
	if let Some(name) = trimmed.strip_prefix("r/u_") {
		format!("u_{}", name.trim())
	} else if let Some(name) = trimmed.strip_prefix("u/") {
		format!("u_{}", name.trim())
	} else if let Some(name) = trimmed.strip_prefix("r/") {
		name.trim().to_string()
	} else {
		trimmed.to_string()
	}
}

fn has_class(element: &ElementRef<'_>, class_name: &str) -> bool {
	element.value().attr("class").unwrap_or_default().split_whitespace().any(|value| value == class_name)
}

fn empty_flair() -> Flair {
	Flair {
		flair_parts: Vec::new(),
		text: String::new(),
		background_color: String::new(),
		foreground_color: String::new(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use hyper::StatusCode;

	const SUBREDDIT_FIXTURE: &str = include_str!("../tests/fixtures/stock_redlib_subreddit.html");
	const USER_FIXTURE: &str = include_str!("../tests/fixtures/stock_redlib_user.html");
	const POST_FIXTURE: &str = include_str!("../tests/fixtures/stock_redlib_post.html");
	const SEARCH_FIXTURE: &str = include_str!("../tests/fixtures/stock_redlib_search.html");
	const ANUBIS_FIXTURE: &str = include_str!("../tests/fixtures/anubis_block.html");
	const HTTP_403_FIXTURE: &str = include_str!("../tests/fixtures/http_403.html");

	#[test]
	fn parses_stock_subreddit_fixture() {
		let parsed = parse_subreddit_page(SUBREDDIT_FIXTURE).unwrap();
		assert_eq!(parsed.subreddit.name, "rust");
		assert_eq!(parsed.subreddit.title, "The Rust Programming Language");
		assert_eq!(parsed.subreddit.members.0, "332k");
		assert_eq!(parsed.subreddit.active.0, "1.2k");
		assert!(parsed.subreddit.wiki);
		assert_eq!(parsed.posts.len(), 1);
		assert_eq!(parsed.posts[0].title, "Example post");
		assert_eq!(parsed.after, "t3_abc123");
	}

	#[test]
	fn parses_stock_user_fixture() {
		let parsed = parse_user_page(USER_FIXTURE).unwrap();
		assert_eq!(parsed.user.name, "spez");
		assert_eq!(parsed.user.description, "Reddit CEO");
		assert_eq!(parsed.posts.len(), 2);
		assert_eq!(parsed.posts[0].kind, "t1");
		assert_eq!(parsed.posts[1].title, "Humans welcome");
		assert_eq!(parsed.after, "t3_def456");
	}

	#[test]
	fn parses_stock_post_fixture() {
		let parsed = parse_post_page(POST_FIXTURE, "c2").unwrap();
		assert_eq!(parsed.post.title, "Example post");
		assert_eq!(parsed.post.comments.0, "28");
		assert_eq!(parsed.comments.len(), 1);
		assert_eq!(parsed.comments[0].replies.len(), 1);
		assert!(parsed.comments[0].replies[0].highlighted);
	}

	#[test]
	fn parses_stock_search_fixture() {
		let parsed = parse_search_page(SEARCH_FIXTURE).unwrap();
		assert_eq!(parsed.subreddits.len(), 2);
		assert_eq!(parsed.subreddits[0].name, "rust");
		assert_eq!(parsed.posts.len(), 2);
		assert_eq!(parsed.posts[0].title, "Example search result");
		assert_eq!(parsed.after, "t3_searchnext");
	}

	#[test]
	fn detects_anubis_block_fixture() {
		assert_eq!(detect_block_page(StatusCode::OK, ANUBIS_FIXTURE), Some("anti-bot interstitial"));
	}

	#[test]
	fn detects_http_error_fixture() {
		assert_eq!(detect_block_page(StatusCode::FORBIDDEN, HTTP_403_FIXTURE), Some("http error"));
	}

	#[test]
	fn sanitizes_untrusted_html() {
		let sanitized = sanitize_html(r#"<div><script>alert(1)</script><a href="https://example.com" onclick="evil()">ok</a></div>"#);
		assert!(!sanitized.contains("<script"));
		assert!(!sanitized.contains("onclick"));
		assert!(sanitized.contains("https://example.com"));
	}
}
