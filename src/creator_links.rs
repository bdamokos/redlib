use crate::utils::{self, param, template, ExternalLink, Preferences};
use askama::Template;
use hyper::{Body, Request, Response};
use serde::Serialize;
use std::collections::HashSet;
use std::time::Duration as StdDuration;
use tokio::task::JoinSet;

const CREATOR_LINK_BATCH_SIZE: usize = 8;
const CREATOR_LINK_MAX_BATCH_SIZE: usize = 16;
const CREATOR_LINK_AUTHOR_CONCURRENCY: usize = 8;
const CREATOR_LINK_AUTHOR_TIMEOUT: StdDuration = StdDuration::from_secs(7);

#[derive(Clone, PartialEq, Eq, Serialize)]
struct CreatorOverviewLink {
	label: String,
	url: String,
	icon_path: String,
	is_archive: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
struct CreatorOverviewRow {
	name: String,
	links: Vec<CreatorOverviewLink>,
}

#[derive(Serialize)]
struct CreatorLinksBatch {
	followed_count: usize,
	offset: usize,
	processed_count: usize,
	next_offset: Option<usize>,
	rows: Vec<CreatorOverviewRow>,
}

#[derive(Template)]
#[template(path = "creator_links.html")]
struct CreatorLinksTemplate {
	prefs: Preferences,
	url: String,
	followed_count: usize,
	batch_size: usize,
}

crate::impl_template_str_eq!(CreatorLinksTemplate);

fn followed_profile_names(prefs: &Preferences) -> Vec<String> {
	let mut seen = HashSet::new();
	let mut names = Vec::new();
	for subscription in &prefs.subscriptions {
		let Some(name) = subscription.strip_prefix("u_").map(str::trim).filter(|name| !name.is_empty()) else {
			continue;
		};
		if seen.insert(name.to_ascii_lowercase()) {
			names.push(name.to_string());
		}
	}
	names.sort_by_key(|name| name.to_ascii_lowercase());
	names
}

fn overview_link_from_external_link(link: ExternalLink) -> Option<CreatorOverviewLink> {
	let is_archive = utils::is_creator_archive_link(&link);
	(is_archive || utils::is_creator_profile_overview_link(&link)).then_some(CreatorOverviewLink {
		label: link.label,
		url: link.url,
		icon_path: link.icon_path,
		is_archive,
	})
}

async fn creator_overview_row_for_author(name: String) -> Option<CreatorOverviewRow> {
	let links = utils::author_external_links(&name).await;
	let links = utils::creator_profile_links_with_archive_lookups(links)
		.await
		.into_iter()
		.filter_map(overview_link_from_external_link)
		.collect::<Vec<_>>();

	(!links.is_empty()).then_some(CreatorOverviewRow { name, links })
}

async fn collect_next_row(pending: &mut JoinSet<Option<CreatorOverviewRow>>, rows: &mut Vec<CreatorOverviewRow>) {
	if let Some(result) = pending.join_next().await {
		if let Ok(Some(row)) = result {
			rows.push(row);
		}
	}
}

async fn creator_overview_rows(names: Vec<String>) -> Vec<CreatorOverviewRow> {
	let mut rows = Vec::new();
	let mut pending = JoinSet::new();

	for name in names {
		pending.spawn(async move {
			tokio::time::timeout(CREATOR_LINK_AUTHOR_TIMEOUT, creator_overview_row_for_author(name))
				.await
				.unwrap_or(None)
		});
		if pending.len() >= CREATOR_LINK_AUTHOR_CONCURRENCY {
			collect_next_row(&mut pending, &mut rows).await;
		}
	}

	while !pending.is_empty() {
		collect_next_row(&mut pending, &mut rows).await;
	}

	rows.sort_by_key(|row| row.name.to_ascii_lowercase());
	rows
}

fn requested_offset(req: &Request<Body>) -> usize {
	let query = format!("?{}", req.uri().query().unwrap_or_default());
	param(&query, "offset").and_then(|offset| offset.parse::<usize>().ok()).unwrap_or_default()
}

fn requested_limit(req: &Request<Body>) -> usize {
	let query = format!("?{}", req.uri().query().unwrap_or_default());
	param(&query, "limit")
		.and_then(|limit| limit.parse::<usize>().ok())
		.filter(|limit| *limit > 0)
		.map(|limit| limit.min(CREATOR_LINK_MAX_BATCH_SIZE))
		.unwrap_or(CREATOR_LINK_BATCH_SIZE)
}

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let followed = followed_profile_names(&prefs);

	Ok(template(&CreatorLinksTemplate {
		prefs,
		url: "/creator-links".to_string(),
		followed_count: followed.len(),
		batch_size: CREATOR_LINK_BATCH_SIZE,
	}))
}

pub async fn data(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let followed = followed_profile_names(&prefs);
	let offset = requested_offset(&req).min(followed.len());
	let limit = requested_limit(&req);
	let end = (offset + limit).min(followed.len());
	let batch_names = followed[offset..end].to_vec();
	let rows = creator_overview_rows(batch_names).await;
	let body = serde_json::to_string(&CreatorLinksBatch {
		followed_count: followed.len(),
		offset,
		processed_count: end.saturating_sub(offset),
		next_offset: (end < followed.len()).then_some(end),
		rows,
	})
	.map_err(|err| err.to_string())?;

	Ok(
		Response::builder()
			.status(200)
			.header("content-type", "application/json")
			.header("Cache-Control", "no-store")
			.body(body.into())
			.unwrap_or_default(),
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures_lite::future::block_on;
	use hyper::body::to_bytes;

	fn external_link(label: &str, url: &str, host: &str) -> ExternalLink {
		ExternalLink {
			label: label.to_string(),
			url: url.to_string(),
			host: host.to_string(),
			icon_path: format!("/site_icon/{host}"),
			has_coomer: false,
		}
	}

	#[test]
	fn followed_profile_names_dedupes_and_sorts_user_subscriptions() {
		let prefs = Preferences {
			subscriptions: vec!["rust".to_string(), "u_Beta".to_string(), "u_alpha".to_string(), "u_beta".to_string()],
			..Preferences::default()
		};

		assert_eq!(followed_profile_names(&prefs), vec!["alpha".to_string(), "Beta".to_string()]);
	}

	#[test]
	fn overview_link_marks_archive_links() {
		let profile = overview_link_from_external_link(external_link("OnlyFans", "https://onlyfans.com/demo", "onlyfans.com")).expect("profile link");
		let archive = overview_link_from_external_link(external_link("Coomer", "https://coomer.st/onlyfans/user/demo", "coomer.st")).expect("archive link");

		assert!(!profile.is_archive);
		assert!(archive.is_archive);
	}

	#[test]
	fn overview_template_renders_redlib_creator_and_archive_icons() {
		let template = CreatorLinksTemplate {
			prefs: Preferences {
				subscriptions: vec!["u_demo".to_string()],
				..Preferences::default()
			},
			url: "/creator-links".to_string(),
			followed_count: 1,
			batch_size: CREATOR_LINK_BATCH_SIZE,
		};

		let html = template.render().expect("template should render");

		assert!(html.contains("data-creator-links-url=\"/creator-links.json\""));
		assert!(html.contains("data-creator-links-followed-count=\"1\""));
		assert!(html.contains("data-creator-links-grid"));
		assert!(html.contains("creator_links.js"));
	}

	#[test]
	fn empty_data_response_returns_json_without_profile_fetches() {
		let response = block_on(data(Request::builder().uri("/creator-links.json?offset=10&limit=99").body(Body::empty()).unwrap())).expect("response");
		assert_eq!(response.status(), 200);
		assert_eq!(response.headers().get("content-type").and_then(|value| value.to_str().ok()), Some("application/json"));

		let bytes = block_on(to_bytes(response.into_body())).expect("body");
		let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("json");

		assert_eq!(parsed["followed_count"], 0);
		assert_eq!(parsed["offset"], 0);
		assert_eq!(parsed["processed_count"], 0);
		assert!(parsed["next_offset"].is_null());
		assert_eq!(parsed["rows"].as_array().map(Vec::len), Some(0));
	}
}
