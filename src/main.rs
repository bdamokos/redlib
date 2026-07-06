// Global specifiers
#![forbid(unsafe_code)]
#![allow(clippy::cmp_owned)]

use cached::proc_macro::cached;
use clap::{Arg, ArgAction, Command};
use std::str::FromStr;
use std::sync::LazyLock;

use futures_lite::FutureExt;
use hyper::Uri;
use hyper::{header::HeaderValue, Body, Request, Response};
use log::{info, warn};
use redlib::client::{canonical_path, proxy, rate_limit_check, CLIENT};
use redlib::json_api;
use redlib::server::{self, RequestExt};
use redlib::utils::{error, redirect, ThemeAssets};
use redlib::{
	bookmarks, cache_persistence, config, creator_links, duplicates, headers, instance_info, observed_semantic_backfill, post, post_cache, post_feedback, search, settings,
	site_icon, subreddit, suggested_profiles, suggested_subreddits, user, video_mode_api, video_preview,
};
use url::Url;

use redlib::client::OAUTH_CLIENT;
use redlib::oauth::spawn_oauth_maintenance;

// Create Services

// Required for the manifest to be valid
async fn pwa_logo() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/png")
			.body(include_bytes!("../static/logo.png").as_ref().into())
			.unwrap_or_default(),
	)
}

// Required for iOS App Icons
async fn iphone_logo() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/png")
			.body(include_bytes!("../static/apple-touch-icon.png").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn favicon() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "image/vnd.microsoft.icon")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_bytes!("../static/favicon.ico").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn font() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "font/woff2")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_bytes!("../static/Inter.var.woff2").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn opensearch() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/opensearchdescription+xml")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(include_bytes!("../static/opensearch.xml").as_ref().into())
			.unwrap_or_default(),
	)
}

async fn resource(body: &str, content_type: &str, cache: bool) -> Result<Response<Body>, String> {
	let mut res = Response::builder()
		.status(200)
		.header("content-type", content_type)
		.body(body.to_string().into())
		.unwrap_or_default();

	if cache {
		if let Ok(val) = HeaderValue::from_str("public, max-age=1209600, s-maxage=86400") {
			res.headers_mut().insert("Cache-Control", val);
		}
	}

	Ok(res)
}

static STYLE_CSS: LazyLock<String> = LazyLock::new(|| {
	let mut res = include_str!("../static/style.css").to_string();
	for file in ThemeAssets::iter() {
		res.push('\n');
		let theme = ThemeAssets::get(file.as_ref()).unwrap();
		res.push_str(std::str::from_utf8(theme.data.as_ref()).unwrap());
	}
	res
});

fn configured_origin(url: &str) -> Option<String> {
	let parsed = Url::parse(url).ok()?;
	let host = parsed.host_str()?;
	let mut origin = format!("{}://{}", parsed.scheme(), host);
	if let Some(port) = parsed.port() {
		origin.push(':');
		origin.push_str(&port.to_string());
	}
	Some(origin)
}

fn content_security_policy() -> String {
	let mut connect_src = vec!["'self'".to_string()];

	if let Some(origin) = config::get_setting("REDLIB_FULL_URL").and_then(|url| configured_origin(&url)) {
		if !connect_src.contains(&origin) {
			connect_src.push(origin);
		}
	}

	if let Some(extra_sources) = config::get_setting("REDLIB_CSP_CONNECT_SRC") {
		for source in extra_sources.split_whitespace() {
			let source = source.trim();
			if !source.is_empty() && !connect_src.iter().any(|existing| existing == source) {
				connect_src.push(source.to_string());
			}
		}
	}

	format!(
		"default-src 'none'; font-src 'self'; script-src 'self' blob:; manifest-src 'self'; media-src 'self' data: blob: about: https://media.redgifs.com; style-src 'self' 'unsafe-inline'; base-uri 'none'; img-src 'self' data: https://media.redgifs.com; form-action 'self'; frame-ancestors 'none'; connect-src {}; worker-src blob:; frame-src https://*.redgifs.com https://*.gfycat.com https://*.imgur.com https://*.redd.it;",
		connect_src.join(" ")
	)
}

fn register_subreddit_fragment_routes(app: &mut server::Server) {
	app.at("/subreddit-fragment").get(|r| subreddit::community_fragment(r).boxed());
	app.at("/subreddit-fragment/r/:sub").get(|r| subreddit::community_fragment(r).boxed());
	app.at("/subreddit-fragment/r/:sub/:sort").get(|r| subreddit::community_fragment(r).boxed());
	app.at("/subreddit-fragment/following").get(|r| subreddit::community_fragment(r).boxed());
	app.at("/subreddit-fragment/following/:sort").get(|r| subreddit::community_fragment(r).boxed());
	app.at("/subreddit-fragment/:sort").get(|r| subreddit::community_fragment(r).boxed());
}

fn register_json_api_routes(app: &mut server::Server) {
	app.at("/api/search").get(|r| json_api::search(r).boxed());
	app.at("/api/r/:sub/search").get(|r| json_api::search(r).boxed());
	app.at("/api/comments/:id").get(|r| json_api::post(r).boxed());
	app.at("/api/comments/:id/:title").get(|r| json_api::post(r).boxed());
	app.at("/api/comments/:id/:title/:comment_id").get(|r| json_api::post(r).boxed());
	app.at("/api/r/:sub/comments/:id").get(|r| json_api::post(r).boxed());
	app.at("/api/r/:sub/comments/:id/:title").get(|r| json_api::post(r).boxed());
	app.at("/api/r/:sub/comments/:id/:title/:comment_id").get(|r| json_api::post(r).boxed());
	app.at("/api/user/:name").get(|r| json_api::user_listing(r).boxed());
	app.at("/api/user/:name/:listing").get(|r| json_api::user_listing(r).boxed());
	app.at("/api/r/:sub").get(|r| json_api::subreddit_listing(r).boxed());
	app.at("/api/r/:sub/:sort").get(|r| json_api::subreddit_listing(r).boxed());

	app.at("/search.js").get(|r| json_api::search(r).boxed());
	app.at("/r/:sub/search.js").get(|r| json_api::search(r).boxed());
	app.at("/comments/:id.js").get(|r| json_api::post(r).boxed());
	app.at("/comments/:id/:title.js").get(|r| json_api::post(r).boxed());
	app.at("/comments/:id/:title/:comment_id.js").get(|r| json_api::post(r).boxed());
	app.at("/r/:sub/comments/:id.js").get(|r| json_api::post(r).boxed());
	app.at("/r/:sub/comments/:id/:title.js").get(|r| json_api::post(r).boxed());
	app.at("/r/:sub/comments/:id/:title/:comment_id.js").get(|r| json_api::post(r).boxed());
	app.at("/user/:name.js").get(|r| json_api::user_listing(r).boxed());
	app.at("/user/:name/:listing.js").get(|r| json_api::user_listing(r).boxed());
	app.at("/u/:name.js").get(|r| json_api::user_listing(r).boxed());
	app.at("/r/:sub.js").get(|r| json_api::subreddit_listing(r).boxed());
	app.at("/r/:sub/:sort.js").get(|r| json_api::subreddit_listing(r).boxed());
}

fn is_front_page_sort_route(id: &str) -> bool {
	matches!(id, "best" | "hot" | "new" | "random" | "top" | "rising" | "controversial" | "video-length")
}

#[cfg(test)]
mod tests {
	use super::{configured_origin, is_front_page_sort_route, register_json_api_routes};
	use hyper::Method;
	use redlib::server::Server;

	#[test]
	fn configured_origin_keeps_scheme_host_and_port() {
		assert_eq!(configured_origin("https://redlib1.bdamokos.org/path?q=1"), Some("https://redlib1.bdamokos.org".to_string()));
		assert_eq!(configured_origin("http://127.0.0.1:18081/settings"), Some("http://127.0.0.1:18081".to_string()));
		assert_eq!(configured_origin("not a url"), None);
	}

	#[test]
	fn subreddit_fragment_route_keeps_subreddit_param() {
		let mut app = Server::new();
		super::register_subreddit_fragment_routes(&mut app);

		let params = app.recognize_params_for_test(&Method::GET, "/subreddit-fragment/r/rust").expect("subreddit fragment route");

		assert_eq!(params.find("sub"), Some("rust"));
		assert_eq!(params.find("sort"), None);
	}

	#[test]
	fn subreddit_fragment_route_keeps_subreddit_sort_param() {
		let mut app = Server::new();
		super::register_subreddit_fragment_routes(&mut app);

		let params = app
			.recognize_params_for_test(&Method::GET, "/subreddit-fragment/r/rust/top")
			.expect("subreddit sorted fragment route");

		assert_eq!(params.find("sub"), Some("rust"));
		assert_eq!(params.find("sort"), Some("top"));
	}

	#[test]
	fn front_page_sort_routes_include_random() {
		assert!(is_front_page_sort_route("random"));
		assert!(is_front_page_sort_route("new"));
		assert!(!is_front_page_sort_route("comments"));
	}

	#[test]
	fn json_api_routes_preserve_listing_params() {
		let mut app = Server::new();
		register_json_api_routes(&mut app);

		let params = app.recognize_params_for_test(&Method::GET, "/r/rust/top.js").expect("subreddit json route");
		assert_eq!(params.find("sub"), Some("rust"));
		assert_eq!(params.find("sort"), Some("top"));

		let params = app.recognize_params_for_test(&Method::GET, "/api/user/spez/submitted").expect("user api route");
		assert_eq!(params.find("name"), Some("spez"));
		assert_eq!(params.find("listing"), Some("submitted"));
	}

	#[test]
	fn json_api_routes_preserve_post_params() {
		let mut app = Server::new();
		register_json_api_routes(&mut app);

		let params = app.recognize_params_for_test(&Method::GET, "/r/rust/comments/abc/title/c42.js").expect("post json route");
		assert_eq!(params.find("sub"), Some("rust"));
		assert_eq!(params.find("id"), Some("abc"));
		assert_eq!(params.find("title"), Some("title"));
		assert_eq!(params.find("comment_id"), Some("c42"));

		let params = app.recognize_params_for_test(&Method::GET, "/api/comments/abc").expect("short post api route");
		assert_eq!(params.find("id"), Some("abc"));
	}
}

async fn style() -> Result<Response<Body>, String> {
	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "text/css")
			.header("Cache-Control", "public, max-age=1209600, s-maxage=86400")
			.body(Body::from(STYLE_CSS.clone()))
			.unwrap_or_default(),
	)
}

fn static_asset_not_found() -> Result<Response<Body>, String> {
	Response::builder()
		.status(404)
		.header("content-type", "text/plain; charset=utf-8")
		.body(Body::from("Not found"))
		.map_err(|e| e.to_string())
}

/// Serves bundled CSS/JS at `/redlib-static/:rev/:file`. The `rev` segment is ignored so stale HTML still loads.
async fn redlib_versioned_static_asset(req: Request<Body>) -> Result<Response<Body>, String> {
	let Some(file) = req.param("file") else {
		return static_asset_not_found();
	};
	if file.contains('/') || file.contains("..") {
		return static_asset_not_found();
	}
	match file.as_str() {
		"style.css" => style().await,
		"playHLSVideo.js" => resource(include_str!("../static/playHLSVideo.js"), "text/javascript", false).await,
		"hls.min.js" => resource(include_str!("../static/hls.min.js"), "text/javascript", false).await,
		"highlighted.js" => resource(include_str!("../static/highlighted.js"), "text/javascript", false).await,
		"dash.all.min.js" => resource(include_str!("../static/dash.all.min.js"), "text/javascript", false).await,
		"dash.all.min.js.map" => resource(include_str!("../static/dash.all.min.js.map"), "application/json", false).await,
		"dash-player.js" => resource(include_str!("../static/dash-player.js"), "text/javascript", false).await,
		"check_update.js" => resource(include_str!("../static/check_update.js"), "text/javascript", false).await,
		"copy.js" => resource(include_str!("../static/copy.js"), "text/javascript", false).await,
		"space_scroll.js" => resource(include_str!("../static/space_scroll.js"), "text/javascript", false).await,
		"network_image.js" => resource(include_str!("../static/network_image.js"), "text/javascript", false).await,
		"post_preview.js" => resource(include_str!("../static/post_preview.js"), "text/javascript", false).await,
		"post_feedback.js" => resource(include_str!("../static/post_feedback.js"), "text/javascript", false).await,
		"relative_time.js" => resource(include_str!("../static/relative_time.js"), "text/javascript", false).await,
		"browser_session.js" => resource(include_str!("../static/browser_session.js"), "text/javascript", false).await,
		"author_links.js" => resource(include_str!("../static/author_links.js"), "text/javascript", false).await,
		"creator_links.js" => resource(include_str!("../static/creator_links.js"), "text/javascript", false).await,
		"subscription.js" => resource(include_str!("../static/subscription.js"), "text/javascript", false).await,
		"suggested_profiles.js" => resource(include_str!("../static/suggested_profiles.js"), "text/javascript", false).await,
		"suggested_subreddits.js" => resource(include_str!("../static/suggested_subreddits.js"), "text/javascript", false).await,
		"feed_status.js" => resource(include_str!("../static/feed_status.js"), "text/javascript", false).await,
		"page_fragment.js" => resource(include_str!("../static/page_fragment.js"), "text/javascript", false).await,
		"dynamic_content.js" => resource(include_str!("../static/dynamic_content.js"), "text/javascript", false).await,
		"video_visibility.js" => resource(include_str!("../static/video_visibility.js"), "text/javascript", false).await,
		"video_preview_gallery.js" => resource(include_str!("../static/video_preview_gallery.js"), "text/javascript", false).await,
		"submitted_duplicates.js" => resource(include_str!("../static/submitted_duplicates.js"), "text/javascript", false).await,
		"video_mode_filters.js" => resource(include_str!("../static/video_mode_filters.js"), "text/javascript", false).await,
		"cache_status.js" => resource(include_str!("../static/cache_status.js"), "text/javascript", false).await,
		_ => static_asset_not_found(),
	}
}

#[tokio::main]
async fn main() {
	// Load environment variables
	_ = dotenvy::dotenv();

	// Initialize logger with timestamps to make upstream pacing visible in logs.
	pretty_env_logger::init_timed();

	let matches = Command::new("Redlib")
		.version(env!("CARGO_PKG_VERSION"))
		.about("Private front-end for Reddit written in Rust ")
		.arg(Arg::new("ipv4-only").short('4').long("ipv4-only").help("Listen on IPv4 only").num_args(0))
		.arg(Arg::new("ipv6-only").short('6').long("ipv6-only").help("Listen on IPv6 only").num_args(0))
		.arg(
			Arg::new("redirect-https")
				.short('r')
				.long("redirect-https")
				.help("Redirect all HTTP requests to HTTPS (no longer functional)")
				.num_args(0),
		)
		.arg(
			Arg::new("address")
				.short('a')
				.long("address")
				.value_name("ADDRESS")
				.help("Sets address to listen on")
				.default_value("[::]")
				.num_args(1),
		)
		.arg(
			Arg::new("port")
				.short('p')
				.long("port")
				.value_name("PORT")
				.env("PORT")
				.help("Port to listen on")
				.default_value("8080")
				.action(ArgAction::Set)
				.num_args(1),
		)
		.arg(
			Arg::new("hsts")
				.short('H')
				.long("hsts")
				.value_name("EXPIRE_TIME")
				.help("HSTS header to tell browsers that this site should only be accessed over HTTPS")
				.default_value("604800")
				.num_args(1),
		)
		.get_matches();

	match rate_limit_check().await {
		Ok(()) => {
			info!("[✅] Rate limit check passed");
		}
		Err(e) => {
			let mut message = format!("Rate limit check failed: {e}");
			message += "\nThis may cause issues with the rate limit.";
			message += "\nPlease report this error with the above information.";
			message += "\nhttps://github.com/redlib-org/redlib/issues/new?assignees=sigaloid&labels=bug&title=%F0%9F%90%9B+Bug+Report%3A+Rate+limit+mismatch";
			warn!("{}", message);
			eprintln!("{message}");
		}
	}

	let address = matches.get_one::<String>("address").unwrap();
	let port = matches.get_one::<String>("port").unwrap();
	let hsts = matches.get_one("hsts").map(|m: &String| m.as_str());

	let ipv4_only = std::env::var("IPV4_ONLY").is_ok() || matches.get_flag("ipv4-only");
	let ipv6_only = std::env::var("IPV6_ONLY").is_ok() || matches.get_flag("ipv6-only");

	let listener = if ipv4_only {
		format!("0.0.0.0:{port}")
	} else if ipv6_only {
		format!("[::]:{port}")
	} else {
		[address, ":", port].concat()
	};

	println!("Starting Redlib...");

	// Begin constructing a server
	let mut app = server::Server::new();

	// Force evaluation of statics. In instance_info case, we need to evaluate
	// the timestamp so deploy date is accurate - in config case, we need to
	// evaluate the configuration to avoid paying penalty at first request -
	// in OAUTH case, we only need the shared holder ready; background
	// maintenance handles initial acquisition and refreshes.

	info!("Evaluating config.");
	LazyLock::force(&config::CONFIG);
	info!("Evaluating instance info.");
	LazyLock::force(&instance_info::INSTANCE_INFO);
	info!("Initializing OAUTH client state.");
	LazyLock::force(&OAUTH_CLIENT);
	spawn_oauth_maintenance();
	video_preview::init();
	cache_persistence::init();
	post_cache::spawn_remembered_feed_daemon();
	observed_semantic_backfill::spawn_observed_semantic_backfill_worker();

	// Define default headers (added to all responses)
	app.default_headers = headers! {
		"Referrer-Policy" => "no-referrer",
		"X-Content-Type-Options" => "nosniff",
		"X-Frame-Options" => "DENY"
	};
	if let Ok(csp) = HeaderValue::from_str(&content_security_policy()) {
		app.default_headers.insert("Content-Security-Policy", csp);
	}

	if let Some(expire_time) = hsts {
		if let Ok(val) = HeaderValue::from_str(&format!("max-age={expire_time}")) {
			app.default_headers.insert("Strict-Transport-Security", val);
		}
	}

	// Read static files
	app.at("/redlib-static/:rev/:file").get(|r| redlib_versioned_static_asset(r).boxed());
	app.at("/style.css").get(|_| style().boxed());
	app
		.at("/manifest.json")
		.get(|_| resource(include_str!("../static/manifest.json"), "application/json", false).boxed());
	app.at("/robots.txt").get(|_| {
		resource(
			if match config::get_setting("REDLIB_ROBOTS_DISABLE_INDEXING") {
				Some(val) => val == "on",
				None => false,
			} {
				"User-agent: *\nDisallow: /"
			} else {
				"User-agent: *\nDisallow: /u/\nDisallow: /user/"
			},
			"text/plain",
			true,
		)
		.boxed()
	});
	app.at("/favicon.ico").get(|_| favicon().boxed());
	app.at("/site_icon/:host").get(|r| site_icon::icon(r).boxed());
	app.at("/logo.png").get(|_| pwa_logo().boxed());
	app.at("/Inter.var.woff2").get(|_| font().boxed());
	app.at("/touch-icon-iphone.png").get(|_| iphone_logo().boxed());
	app.at("/apple-touch-icon.png").get(|_| iphone_logo().boxed());
	app.at("/opensearch.xml").get(|_| opensearch().boxed());
	app
		.at("/playHLSVideo.js")
		.get(|_| resource(include_str!("../static/playHLSVideo.js"), "text/javascript", false).boxed());
	app
		.at("/hls.min.js")
		.get(|_| resource(include_str!("../static/hls.min.js"), "text/javascript", false).boxed());
	app
		.at("/highlighted.js")
		.get(|_| resource(include_str!("../static/highlighted.js"), "text/javascript", false).boxed());
	app
		.at("/dash.all.min.js")
		.get(|_| resource(include_str!("../static/dash.all.min.js"), "text/javascript", false).boxed());
	app
		.at("/dash.all.min.js.map")
		.get(|_| resource(include_str!("../static/dash.all.min.js.map"), "application/json", false).boxed());
	app
		.at("/dash-player.js")
		.get(|_| resource(include_str!("../static/dash-player.js"), "text/javascript", false).boxed());
	app
		.at("/check_update.js")
		.get(|_| resource(include_str!("../static/check_update.js"), "text/javascript", false).boxed());
	app.at("/copy.js").get(|_| resource(include_str!("../static/copy.js"), "text/javascript", false).boxed());
	app
		.at("/space_scroll.js")
		.get(|_| resource(include_str!("../static/space_scroll.js"), "text/javascript", false).boxed());
	app
		.at("/network_image.js")
		.get(|_| resource(include_str!("../static/network_image.js"), "text/javascript", false).boxed());
	app
		.at("/post_preview.js")
		.get(|_| resource(include_str!("../static/post_preview.js"), "text/javascript", false).boxed());
	app
		.at("/post_feedback.js")
		.get(|_| resource(include_str!("../static/post_feedback.js"), "text/javascript", false).boxed());
	app
		.at("/browser_session.js")
		.get(|_| resource(include_str!("../static/browser_session.js"), "text/javascript", false).boxed());
	app
		.at("/author_links.js")
		.get(|_| resource(include_str!("../static/author_links.js"), "text/javascript", false).boxed());
	app
		.at("/creator_links.js")
		.get(|_| resource(include_str!("../static/creator_links.js"), "text/javascript", false).boxed());
	app
		.at("/subscription.js")
		.get(|_| resource(include_str!("../static/subscription.js"), "text/javascript", false).boxed());
	app
		.at("/suggested_profiles.js")
		.get(|_| resource(include_str!("../static/suggested_profiles.js"), "text/javascript", false).boxed());
	app
		.at("/suggested_subreddits.js")
		.get(|_| resource(include_str!("../static/suggested_subreddits.js"), "text/javascript", false).boxed());
	app
		.at("/feed_status.js")
		.get(|_| resource(include_str!("../static/feed_status.js"), "text/javascript", false).boxed());
	app
		.at("/page_fragment.js")
		.get(|_| resource(include_str!("../static/page_fragment.js"), "text/javascript", false).boxed());
	app
		.at("/dynamic_content.js")
		.get(|_| resource(include_str!("../static/dynamic_content.js"), "text/javascript", false).boxed());
	app
		.at("/video_visibility.js")
		.get(|_| resource(include_str!("../static/video_visibility.js"), "text/javascript", false).boxed());
	app
		.at("/video_preview_gallery.js")
		.get(|_| resource(include_str!("../static/video_preview_gallery.js"), "text/javascript", false).boxed());
	app
		.at("/submitted_duplicates.js")
		.get(|_| resource(include_str!("../static/submitted_duplicates.js"), "text/javascript", false).boxed());
	app
		.at("/video_mode_filters.js")
		.get(|_| resource(include_str!("../static/video_mode_filters.js"), "text/javascript", false).boxed());

	app.at("/api/video-previews/lease").post(|r| video_preview::lease(r).boxed());
	app.at("/api/video-previews/heartbeat").post(|r| video_preview::heartbeat(r).boxed());
	app.at("/api/video-previews/release").post(|r| video_preview::release(r).boxed());
	app.at("/api/video-mode/apply").post(|r| video_mode_api::apply(r).boxed());
	app.at("/api/video-mode/clear").post(|r| video_mode_api::clear(r).boxed());
	app.at("/api/video-mode/clear-redirect").get(|r| video_mode_api::clear_redirect(r).boxed());
	app.at("/api/browser-sessions/heartbeat").post(|r| post_cache::browser_session_heartbeat(r).boxed());
	app.at("/api/browser-sessions/release").post(|r| post_cache::browser_session_release(r).boxed());
	app.at("/post-feedback").post(|r| post_feedback::update(r).boxed());
	app.at("/api/video-previews/:job_id").get(|r| video_preview::status(r).boxed());
	app.at("/api/video-previews/:job_id/animation").get(|r| video_preview::animation(r).boxed());
	app.at("/api/video-previews/:job_id/frame/:index").get(|r| video_preview::frame(r).boxed());
	register_json_api_routes(&mut app);

	app.at("/commits.atom").get(|_| async move { proxy_commit_info().await }.boxed());
	app.at("/instances.json").get(|_| async move { proxy_instances().await }.boxed());

	// Proxy media through Redlib
	app.at("/vid/:id/:size").get(|r| proxy(r, "https://v.redd.it/{id}/DASH_{size}").boxed());
	app.at("/hls/:id/*path").get(|r| proxy(r, "https://v.redd.it/{id}/{path}").boxed());
	app.at("/dash/:id/*path").get(|r| proxy(r, "https://v.redd.it/{id}/{path}").boxed());
	app.at("/img/*path").get(|r| proxy(r, "https://i.redd.it/{path}").boxed());
	app.at("/thumb/:point/:id").get(|r| proxy(r, "https://{point}.thumbs.redditmedia.com/{id}").boxed());
	app.at("/emoji/:id/:name").get(|r| proxy(r, "https://emoji.redditmedia.com/{id}/{name}").boxed());
	app
		.at("/emote/:subreddit_id/:filename")
		.get(|r| proxy(r, "https://reddit-econ-prod-assets-permanent.s3.amazonaws.com/asset-manager/{subreddit_id}/{filename}").boxed());
	app
		.at("/preview/:loc/award_images/:fullname/:id")
		.get(|r| proxy(r, "https://{loc}view.redd.it/award_images/{fullname}/{id}").boxed());
	app.at("/preview/:loc/:id").get(|r| proxy(r, "https://{loc}view.redd.it/{id}").boxed());
	app.at("/style/*path").get(|r| proxy(r, "https://styles.redditmedia.com/{path}").boxed());
	app.at("/static/*path").get(|r| proxy(r, "https://www.redditstatic.com/{path}").boxed());
	app.at("/redgifs/:path").get(|req| redlib::redgifs::handler(req).boxed());

	// Browse user profile
	app
		.at("/u/:name")
		.get(|r| async move { Ok(redirect(&format!("/user/{}", r.param("name").unwrap_or_default()))) }.boxed());
	app.at("/u/:name/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/u/:name/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());
	app.at("/post-fragment/u/:name/comments/:id/:title").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/u/:name/comments/:id/:title/:comment_id").get(|r| post::item_fragment(r).boxed());

	app.at("/user/[deleted]").get(|req| error(req, "User has deleted their account").boxed());
	app.at("/author_links").get(|r| user::author_links(r).boxed());
	app.at("/user/:name.rss").get(|r| user::rss(r).boxed());
	app.at("/user-fragment/:name").get(|r| user::profile_fragment(r).boxed());
	app.at("/user-fragment/:name/:listing").get(|r| user::profile_fragment(r).boxed());
	app.at("/user-sidebar/:name").get(|r| user::sidebar_fragment(r).boxed());
	app.at("/user-sidebar/:name/:listing").get(|r| user::sidebar_fragment(r).boxed());
	app.at("/user/:name").get(|r| user::profile(r).boxed());
	app.at("/user/:name/:listing").get(|r| user::profile(r).boxed());
	app.at("/user/:name/comments/:id").get(|r| post::item(r).boxed());
	app.at("/user/:name/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/user/:name/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());
	app.at("/post-fragment/user/:name/comments/:id").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/user/:name/comments/:id/:title").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/user/:name/comments/:id/:title/:comment_id").get(|r| post::item_fragment(r).boxed());

	// Configure settings
	app.at("/settings").get(|r| settings::get(r).boxed()).post(|r| settings::set(r).boxed());
	app.at("/settings/restore").get(|r| settings::restore(r).boxed());
	app.at("/settings/encoded-restore").post(|r| settings::encoded_restore(r).boxed());
	app.at("/settings/update").get(|r| settings::update(r).boxed());

	// RSS Subscriptions
	app.at("/r/:sub.rss").get(|r| subreddit::rss(r).boxed());

	// Subreddit services
	app
		.at("/r/:sub")
		.get(|r| subreddit::community(r).boxed())
		.post(|r| subreddit::add_quarantine_exception(r).boxed());

	app
		.at("/r/u_:name")
		.get(|r| async move { Ok(redirect(&format!("/user/{}", r.param("name").unwrap_or_default()))) }.boxed());

	app.at("/r/:sub/subscribe").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/unsubscribe").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/flair/subscribe").post(|r| subreddit::flair_subscriptions(r).boxed());
	app.at("/r/:sub/flair/unsubscribe").post(|r| subreddit::flair_subscriptions(r).boxed());
	app.at("/r/:sub/filter").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/r/:sub/unfilter").post(|r| subreddit::subscriptions_filters(r).boxed());
	app.at("/feed-status").get(|r| subreddit::feed_status_api(r).boxed());
	app.at("/cache-status").get(|r| subreddit::cache_status_page(r).boxed());
	app.at("/cache-status.json").get(|r| subreddit::cache_status_api(r).boxed());
	app.at("/feed-fragment").get(|r| subreddit::feed_fragment(r).boxed());
	app.at("/bookmarks").get(|r| bookmarks::page(r).boxed());
	app.at("/bookmarks/add").post(|r| bookmarks::add(r).boxed());
	app.at("/bookmarks/remove").post(|r| bookmarks::remove(r).boxed());
	app.at("/creator-links").get(|r| creator_links::page(r).boxed());
	app.at("/creator-links.json").get(|r| creator_links::data(r).boxed());
	app.at("/suggested-profiles").get(|r| suggested_profiles::page(r).boxed());
	app.at("/suggested-subreddits").get(|r| suggested_subreddits::page(r).boxed());
	register_subreddit_fragment_routes(&mut app);

	app.at("/r/:sub/comments/:id").get(|r| post::item(r).boxed());
	app.at("/r/:sub/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/r/:sub/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());
	app.at("/post-fragment/r/:sub/comments/:id").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/r/:sub/comments/:id/:title").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/r/:sub/comments/:id/:title/:comment_id").get(|r| post::item_fragment(r).boxed());
	app.at("/comments/:id").get(|r| post::item(r).boxed());
	app.at("/comments/:id/comments").get(|r| post::item(r).boxed());
	app.at("/comments/:id/comments/:comment_id").get(|r| post::item(r).boxed());
	app.at("/comments/:id/:title").get(|r| post::item(r).boxed());
	app.at("/comments/:id/:title/:comment_id").get(|r| post::item(r).boxed());
	app.at("/post-fragment/comments/:id").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/comments/:id/comments").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/comments/:id/comments/:comment_id").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/comments/:id/:title").get(|r| post::item_fragment(r).boxed());
	app.at("/post-fragment/comments/:id/:title/:comment_id").get(|r| post::item_fragment(r).boxed());

	app.at("/r/:sub/duplicates/:id").get(|r| duplicates::item(r).boxed());
	app.at("/r/:sub/duplicates/:id/:title").get(|r| duplicates::item(r).boxed());
	app.at("/duplicates/:id").get(|r| duplicates::item(r).boxed());
	app.at("/duplicates/:id/:title").get(|r| duplicates::item(r).boxed());
	app.at("/duplicates-fragment/r/:sub/duplicates/:id").get(|r| duplicates::item_fragment(r).boxed());
	app.at("/duplicates-fragment/r/:sub/duplicates/:id/:title").get(|r| duplicates::item_fragment(r).boxed());
	app.at("/duplicates-fragment/duplicates/:id").get(|r| duplicates::item_fragment(r).boxed());
	app.at("/duplicates-fragment/duplicates/:id/:title").get(|r| duplicates::item_fragment(r).boxed());

	app.at("/search-fragment").get(|r| search::find_fragment(r).boxed());
	app.at("/search-fragment/r/:sub/search").get(|r| search::find_fragment(r).boxed());
	app.at("/r/:sub/search").get(|r| search::find(r).boxed());

	app
		.at("/r/:sub/w")
		.get(|r| async move { Ok(redirect(&format!("/r/{}/wiki", r.param("sub").unwrap_or_default()))) }.boxed());
	app
		.at("/r/:sub/w/*page")
		.get(|r| async move { Ok(redirect(&format!("/r/{}/wiki/{}", r.param("sub").unwrap_or_default(), r.param("page").unwrap_or_default()))) }.boxed());
	app.at("/r/:sub/wiki").get(|r| subreddit::wiki(r).boxed());
	app.at("/r/:sub/wiki/*page").get(|r| subreddit::wiki(r).boxed());
	app.at("/wiki-fragment/r/:sub/wiki").get(|r| subreddit::wiki_fragment(r).boxed());
	app.at("/wiki-fragment/r/:sub/wiki/*page").get(|r| subreddit::wiki_fragment(r).boxed());

	app.at("/r/:sub/about/sidebar").get(|r| subreddit::sidebar(r).boxed());
	app.at("/wiki-fragment/r/:sub/about/sidebar").get(|r| subreddit::sidebar_fragment(r).boxed());

	app.at("/r/:sub/:sort").get(|r| subreddit::community(r).boxed());

	// Front page
	app.at("/following").get(|r| subreddit::community(r).boxed());
	app.at("/following/:sort").get(|r| subreddit::community(r).boxed());
	app.at("/").get(|r| subreddit::community(r).boxed());

	// View Reddit wiki
	app.at("/w").get(|_| async { Ok(redirect("/wiki")) }.boxed());
	app
		.at("/w/*page")
		.get(|r| async move { Ok(redirect(&format!("/wiki/{}", r.param("page").unwrap_or_default()))) }.boxed());
	app.at("/wiki").get(|r| subreddit::wiki(r).boxed());
	app.at("/wiki/*page").get(|r| subreddit::wiki(r).boxed());
	app.at("/wiki-fragment/wiki").get(|r| subreddit::wiki_fragment(r).boxed());
	app.at("/wiki-fragment/wiki/*page").get(|r| subreddit::wiki_fragment(r).boxed());

	// Search all of Reddit
	app.at("/search").get(|r| search::find(r).boxed());

	// Handle about pages
	app.at("/about").get(|req| error(req, "About pages aren't added yet").boxed());

	// Instance info page
	app.at("/info").get(|r| instance_info::instance_info(r).boxed());
	app.at("/info.:extension").get(|r| instance_info::instance_info(r).boxed());

	// Handle obfuscated share links.
	// Note that this still forces the server to follow the share link to get to the post, so maybe this wants to be updated with a warning before it follow it
	app.at("/r/:sub/s/:id").get(|req: Request<Body>| {
		Box::pin(async move {
			let sub = req.param("sub").unwrap_or_default();
			match req.param("id").as_deref() {
				// Share link
				Some(id) if (8..12).contains(&id.len()) => match canonical_path(format!("/r/{sub}/s/{id}"), 3).await {
					Ok(Some(path)) => Ok(redirect(&path)),
					Ok(None) => error(req, "Post ID is invalid. It may point to a post on a community that has been banned.").await,
					Err(e) => error(req, &e).await,
				},

				// Error message for unknown pages
				_ => error(req, "Nothing here").await,
			}
		})
	});

	app.at("/:id").get(|req: Request<Body>| {
		Box::pin(async move {
			match req.param("id").as_deref() {
				// Sort front page
				Some(id) if is_front_page_sort_route(id) => subreddit::community(req).await,

				// Short link for post
				Some(id) if (5..8).contains(&id.len()) => match canonical_path(format!("/comments/{id}"), 3).await {
					Ok(path_opt) => match path_opt {
						Some(path) => Ok(redirect(&path)),
						None => error(req, "Post ID is invalid. It may point to a post on a community that has been banned.").await,
					},
					Err(e) => error(req, &e).await,
				},

				// Error message for unknown pages
				_ => error(req, "Nothing here").await,
			}
		})
	});

	// Default service in case no routes match
	app.at("/*").get(|req| error(req, "Nothing here").boxed());

	println!("Running Redlib v{} on {listener}!", env!("CARGO_PKG_VERSION"));

	let server = app.listen(&listener);

	// Run this server for... forever!
	if let Err(e) = server.await {
		eprintln!("Server error: {e}");
	}
}

pub async fn proxy_commit_info() -> Result<Response<Body>, String> {
	let body = match fetch_commit_info().await {
		Ok(body) => body,
		Err(e) => {
			warn!("Failed to fetch commit info: {e}");
			return Ok(
				Response::builder()
					.status(502)
					.header("content-type", "text/plain; charset=utf-8")
					.body(Body::from("Failed to fetch commit info"))
					.unwrap_or_default(),
			);
		}
	};

	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/atom+xml")
			.body(Body::from(body))
			.unwrap_or_default(),
	)
}

#[cached(time = 600, result = true)]
async fn fetch_commit_info() -> Result<String, String> {
	let uri = Uri::from_str("https://github.com/redlib-org/redlib/commits/main.atom").expect("Invalid URI");

	let resp: Body = CLIENT.get(uri).await.map_err(|e| format!("Failed to request GitHub: {e}"))?.into_body();

	hyper::body::to_bytes(resp)
		.await
		.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
		.map_err(|e| format!("Failed to read body: {e}"))
}

pub async fn proxy_instances() -> Result<Response<Body>, String> {
	let body = match fetch_instances().await {
		Ok(body) => body,
		Err(e) => {
			warn!("Failed to fetch instances: {e}");
			return Ok(
				Response::builder()
					.status(502)
					.header("content-type", "text/plain; charset=utf-8")
					.body(Body::from("Failed to fetch instances"))
					.unwrap_or_default(),
			);
		}
	};

	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/json")
			.body(Body::from(body))
			.unwrap_or_default(),
	)
}

#[cached(time = 600, result = true)]
async fn fetch_instances() -> Result<String, String> {
	let uri = Uri::from_str("https://raw.githubusercontent.com/redlib-org/redlib-instances/refs/heads/main/instances.json").expect("Invalid URI");

	let resp: Body = CLIENT.get(uri).await.map_err(|e| format!("Failed to request GitHub: {e}"))?.into_body();

	hyper::body::to_bytes(resp)
		.await
		.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
		.map_err(|e| format!("Failed to read body: {e}"))
}
