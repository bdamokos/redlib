use crate::utils::{self, template, ExternalLink, Preferences};
use askama::Template;
use hyper::{Body, Request, Response};
use std::collections::HashSet;
use std::time::Duration as StdDuration;
use tokio::task::JoinSet;

const CREATOR_LINK_AUTHOR_CONCURRENCY: usize = 12;
const CREATOR_LINK_AUTHOR_TIMEOUT: StdDuration = StdDuration::from_secs(10);

#[derive(Clone, PartialEq, Eq)]
struct CreatorOverviewLink {
	label: String,
	url: String,
	icon_path: String,
	is_archive: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct CreatorOverviewRow {
	name: String,
	links: Vec<CreatorOverviewLink>,
}

#[derive(Template)]
#[template(path = "creator_links.html")]
struct CreatorLinksTemplate {
	prefs: Preferences,
	url: String,
	followed_count: usize,
	rows: Vec<CreatorOverviewRow>,
}

crate::impl_template_str_eq!(CreatorLinksTemplate);

impl CreatorLinksTemplate {
	fn total_link_count(&self) -> usize {
		self.rows.iter().map(|row| row.links.len()).sum()
	}

	fn archive_link_count(&self) -> usize {
		self.rows.iter().flat_map(|row| row.links.iter()).filter(|link| link.is_archive).count()
	}

	fn skipped_profile_count(&self) -> usize {
		self.followed_count.saturating_sub(self.rows.len())
	}
}

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

pub async fn page(req: Request<Body>) -> Result<Response<Body>, String> {
	let prefs = Preferences::new(&req);
	let followed = followed_profile_names(&prefs);
	let rows = creator_overview_rows(followed.clone()).await;

	Ok(template(&CreatorLinksTemplate {
		prefs,
		url: "/creator-links".to_string(),
		followed_count: followed.len(),
		rows,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

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
			rows: vec![CreatorOverviewRow {
				name: "demo".to_string(),
				links: vec![
					CreatorOverviewLink {
						label: "OnlyFans".to_string(),
						url: "https://onlyfans.com/demo".to_string(),
						icon_path: "/site_icon/onlyfans.com".to_string(),
						is_archive: false,
					},
					CreatorOverviewLink {
						label: "Coomer".to_string(),
						url: "https://coomer.st/onlyfans/user/demo".to_string(),
						icon_path: "/site_icon/coomer.st".to_string(),
						is_archive: true,
					},
				],
			}],
		};

		let html = template.render().expect("template should render");

		assert!(html.contains("href=\"/user/demo\""));
		assert!(html.contains("src=\"/favicon.ico\""));
		assert!(html.contains("src=\"/site_icon/onlyfans.com\""));
		assert!(html.contains("creator_link_icon_archive"));
	}
}
