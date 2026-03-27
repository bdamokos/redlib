use crate::user::collapse_duplicate_submissions;
use crate::utils::Post;
use log::{info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;
use tokio::sync::Semaphore;

const CACHE_TTL_SECS: u64 = 3600;
const FETCH_CONCURRENCY: usize = 5;
const PER_SUB_LIMIT: usize = 100;
pub const POSTS_PER_PAGE: usize = 25;

struct CachedSubredditPosts {
	posts: Vec<Post>,
	after: String,
	fetched_at: Instant,
}

static CACHE: LazyLock<Mutex<HashMap<String, CachedSubredditPosts>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn is_expired(entry: &CachedSubredditPosts) -> bool {
	entry.fetched_at.elapsed().as_secs() > CACHE_TTL_SECS
}

/// Get cached posts for a single subreddit (if present and not expired).
pub fn get_cached_posts(subreddit: &str) -> Option<Vec<Post>> {
	let cache = CACHE.lock().ok()?;
	let entry = cache.get(&subreddit.to_lowercase())?;
	if is_expired(entry) {
		None
	} else {
		Some(entry.posts.clone())
	}
}

/// Get a page of cached posts for a single subreddit.
/// Returns (posts_for_page, after_token, has_prev, has_next).
/// Returns None if the subreddit is not cached or cache is expired.
pub fn get_cached_subreddit_page(subreddit: &str, page: usize, per_page: usize) -> Option<(Vec<Post>, String, bool, bool)> {
	let cache = CACHE.lock().ok()?;
	let entry = cache.get(&subreddit.to_lowercase())?;
	if is_expired(entry) {
		return None;
	}

	let posts = &entry.posts;
	let start = page * per_page;
	if start >= posts.len() {
		// Beyond cached range — caller should fall through to API.
		return None;
	}
	let end = (start + per_page).min(posts.len());
	let has_next = end < posts.len() || !entry.after.is_empty();
	let has_prev = page > 0;

	// If we're on the last cached page and there's an after token,
	// return it so the caller can use it for further Reddit API pagination.
	let after = if end >= posts.len() {
		entry.after.clone()
	} else {
		String::new()
	};

	Some((posts[start..end].to_vec(), after, has_prev, has_next))
}

/// Store fetched posts for a subreddit.
fn store_cached_posts(subreddit: &str, posts: Vec<Post>, after: String) {
	if let Ok(mut cache) = CACHE.lock() {
		cache.insert(
			subreddit.to_lowercase(),
			CachedSubredditPosts {
				posts,
				after,
				fetched_at: Instant::now(),
			},
		);
	}
}

/// Spawn background tasks to fetch each subreddit individually.
/// Uses a semaphore to limit concurrency.
pub fn trigger_background_fetch(subreddits: Vec<String>, quarantine: bool) {
	tokio::spawn(async move {
		let semaphore = std::sync::Arc::new(Semaphore::new(FETCH_CONCURRENCY));
		let mut handles = Vec::new();

		// Filter out subreddits that already have a fresh cache entry.
		let to_fetch: Vec<String> = {
			let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
			subreddits
				.into_iter()
				.filter(|sub| {
					match cache.get(&sub.to_lowercase()) {
						Some(entry) if !is_expired(entry) => false,
						_ => true,
					}
				})
				.collect()
		};

		if to_fetch.is_empty() {
			return;
		}

		info!("post_cache: background fetch starting for {} subreddits", to_fetch.len());

		for sub in to_fetch {
			let permit = semaphore.clone().acquire_owned().await;
			let Ok(permit) = permit else { break };

			handles.push(tokio::spawn(async move {
				let path = format!("/r/{}/new.json?limit={}&raw_json=1", sub.replace('+', "%2B"), PER_SUB_LIMIT);
				match Post::fetch(&path, quarantine).await {
					Ok((posts, after)) => {
						let count = posts.len();
						crate::utils::store_post_flairs(&posts);
						store_cached_posts(&sub, posts, after);
						log::trace!("post_cache: fetched {} posts for r/{}", count, sub);
					}
					Err(e) => {
						warn!("post_cache: failed to fetch r/{}: {}", sub, e);
					}
				}
				drop(permit);
			}));
		}

		for handle in handles {
			let _ = handle.await;
		}

		info!("post_cache: background fetch complete");
	});
}

/// Build a round-robin merged feed from cached posts, with cross-subreddit
/// duplicate detection and collapsing.
///
/// Takes one post from each subreddit in turn, cycling through all subs,
/// so every subreddit gets equal representation regardless of activity.
/// Duplicate posts (same image/video/gallery across subs) are collapsed
/// under the first occurrence.
///
/// Returns `None` if no subreddits have cached data.
/// Returns `Some((page_posts, has_next_page))` otherwise.
pub fn build_round_robin_feed(
	subreddits: &[String],
	page: usize,
	per_page: usize,
	filters: &HashSet<String>,
) -> Option<(Vec<Post>, bool)> {
	let cache = CACHE.lock().ok()?;

	// Build per-subreddit post queues from cache (only non-expired).
	let mut queues: Vec<VecDeque<Post>> = Vec::new();
	let mut any_cached = false;

	for sub in subreddits {
		let key = sub.to_lowercase();
		if let Some(entry) = cache.get(&key) {
			if !is_expired(entry) {
				any_cached = true;
				// Filter out posts from filtered subs/authors at queue build time.
				let filtered: VecDeque<Post> = entry
					.posts
					.iter()
					.filter(|p| {
						!filters.contains(&p.community) && !filters.contains(&format!("u_{}", p.author.name))
					})
					.cloned()
					.collect();
				if !filtered.is_empty() {
					queues.push(filtered);
				}
			}
		}
	}

	if !any_cached {
		return None;
	}

	// Round-robin: take one post from each queue in turn.
	let mut all_posts: Vec<Post> = Vec::new();
	let mut seen_ids: HashSet<String> = HashSet::new();

	loop {
		let mut made_progress = false;
		for queue in queues.iter_mut() {
			while let Some(post) = queue.pop_front() {
				if seen_ids.insert(post.id.clone()) {
					all_posts.push(post);
					made_progress = true;
					break;
				}
				// Duplicate id, try next post from same queue.
			}
		}
		if !made_progress {
			break;
		}
	}

	// Collapse cross-subreddit duplicates (same image/video posted to multiple subs).
	all_posts = collapse_duplicate_submissions(all_posts);

	// Paginate.
	let start = page * per_page;
	if start >= all_posts.len() {
		return Some((Vec::new(), false));
	}
	let end = (start + per_page).min(all_posts.len());
	let has_next = end < all_posts.len();
	Some((all_posts[start..end].to_vec(), has_next))
}

/// Return (cached_subreddit_count, total_cached_posts) for debugging.
pub fn cache_stats() -> (usize, usize) {
	let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
	let subs = cache.values().filter(|e| !is_expired(e)).count();
	let posts: usize = cache.values().filter(|e| !is_expired(e)).map(|e| e.posts.len()).sum();
	(subs, posts)
}
